// -------------------------------------------------------------------------------------------------
//  Copyright (C) 2015-2026 Nautech Systems Pty Ltd. All rights reserved.
//  https://nautechsystems.io
//
//  Licensed under the GNU Lesser General Public License Version 3.0 (the "License");
//  You may not use this file except in compliance with the License.
//  You may obtain a copy of the License at https://www.gnu.org/licenses/lgpl-3.0.en.html
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
// -------------------------------------------------------------------------------------------------

// Native CASH reservation conformance. One native free balance must fund each CASH BUY exactly
// once across MARKET batches, LIMIT reservations with their commissions, and held MARKET residuals,
// which keep filling on later liquidity. Setup is duplicated from `cash_market_continuation`.

use std::{cell::RefCell, rc::Rc};

use nautilus_common::{
    cache::Cache,
    clock::{Clock, TestClock},
    messages::execution::CancelOrder,
    msgbus::{self, MessageBus, MessagingSwitchboard, TypedIntoHandler},
};
use nautilus_core::{UUID4, UnixNanos};
use nautilus_execution::{
    engine::{ExecutionEngine, stubs::StubExecutionClient},
    matching_engine::{
        OrderMatchingEngine, cash_commitments::CashCommitments, config::OrderMatchingEngineConfig,
    },
    models::{fee::FeeModelHandle, fill::FillModelHandle},
};
use nautilus_model::{
    accounts::{Account, AccountAny, CashAccount},
    data::{Bar, BarType, QuoteTick, TradeTick},
    enums::{
        AccountType, AggressorSide, BookType, CurrencyType, OmsType, OrderSide, OrderStatus,
        OrderType, TimeInForce,
    },
    events::{AccountState, OrderEventAny, order::spec::OrderSubmittedSpec},
    identifiers::{
        AccountId, ClientId, ClientOrderId, InstrumentId, StrategyId, Symbol, TradeId, Venue,
    },
    instruments::{Instrument, InstrumentAny, stubs::equity_aapl},
    orders::{Order, OrderAny, OrderTestBuilder},
    types::{AccountBalance, Currency, Money, Price, Quantity},
};
use nautilus_portfolio::Portfolio;
use rstest::rstest;
use rust_decimal::Decimal;

const CASH_BATCH_DEBIT: &str = "CASH_BATCH: Complete native batch debit";
const CASH_LIMIT: &str = "CASH_LIMIT:";

fn reset_native_globals() {
    *msgbus::get_message_bus().borrow_mut() = MessageBus::default();
    Currency::register(
        Currency::new("IDR", 2, 360, "Indonesian rupiah", CurrencyType::Fiat),
        false,
    )
    .unwrap();
}

fn idr(amount: &str) -> Money {
    Money::from(format!("{amount} IDR").as_str())
}

fn idx_equity(symbol: &str, fee_rate: Decimal) -> InstrumentAny {
    idx_equity_with_fees(symbol, fee_rate, fee_rate)
}

fn idx_equity_with_fees(symbol: &str, maker_fee: Decimal, taker_fee: Decimal) -> InstrumentAny {
    let mut equity = equity_aapl();
    equity.id = InstrumentId::from(format!("{symbol}.XIDX").as_str());
    equity.raw_symbol = Symbol::from(symbol);
    equity.currency = Currency::from("IDR");
    equity.price_precision = 0;
    equity.price_increment = Price::from("1");
    equity.lot_size = Some(Quantity::from("100"));
    equity.maker_fee = maker_fee;
    equity.taker_fee = taker_fee;
    InstrumentAny::Equity(equity)
}

fn quote(instrument_id: InstrumentId, bid: &str, ask: &str, size: &str, ts: u64) -> QuoteTick {
    QuoteTick::new(
        instrument_id,
        Price::from(bid),
        Price::from(ask),
        Quantity::from(size),
        Quantity::from(size),
        UnixNanos::from(ts),
        UnixNanos::from(ts),
    )
}

// Later market data that brings new L1 liquidity at `price`.
#[derive(Clone, Copy, Debug)]
enum Liquidity {
    Quote,
    Trade,
    Bar,
}

fn buy(
    id: &str,
    instrument_id: InstrumentId,
    strategy_id: &str,
    quantity: &str,
    limit_price: Option<&str>,
    time_in_force: TimeInForce,
) -> OrderAny {
    let mut builder = OrderTestBuilder::new(OrderType::Market);
    builder
        .instrument_id(instrument_id)
        .strategy_id(StrategyId::from(strategy_id))
        .side(OrderSide::Buy)
        .quantity(Quantity::from(quantity))
        .time_in_force(time_in_force)
        .client_order_id(ClientOrderId::from(id));
    if let Some(price) = limit_price {
        builder.kind(OrderType::Limit).price(Price::from(price));
    }
    builder.build()
}

fn pending_kinds(pending: &[(String, &'static str)], id: &str) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    for (event_id, kind) in pending {
        if event_id == id {
            kinds.push(*kind);
        }
    }
    kinds
}

// Before native application only `committed` may hold the free balance. The contender is
// rejected at once or held without events (open design question 2), never committed as well.
fn assert_one_pending_commitment(
    pending: &[(String, &'static str)],
    committed: &str,
    kind: &str,
    contender: &str,
) {
    assert_eq!(pending_kinds(pending, committed), [kind], "{committed}");
    let contender_kinds = pending_kinds(pending, contender);
    assert!(
        contender_kinds.is_empty() || contender_kinds == ["Rejected"],
        "{contender}: {contender_kinds:?}"
    );
}

struct CashHarness {
    account_id: AccountId,
    cache: Rc<RefCell<Cache>>,
    execution: Rc<RefCell<ExecutionEngine>>,
    queued_events: Rc<RefCell<Vec<OrderEventAny>>>,
    instrument_ids: Vec<InstrumentId>,
    matchers: Vec<OrderMatchingEngine>,
    _portfolio: Portfolio,
}

impl CashHarness {
    fn new(
        balances: &[Money],
        base_currency: Option<Currency>,
        instruments: &[InstrumentAny],
        deferred_events: bool,
    ) -> Self {
        let account_id = AccountId::from("XIDX-001");
        let mut account_balances = Vec::new();
        for total in balances {
            account_balances.push(AccountBalance::new(
                *total,
                Money::zero(total.currency),
                *total,
            ));
        }
        let state = AccountState::new(
            account_id,
            AccountType::Cash,
            account_balances,
            vec![],
            true,
            UUID4::new(),
            UnixNanos::default(),
            UnixNanos::default(),
            base_currency,
        );
        let cache = Rc::new(RefCell::new(Cache::default()));
        for instrument in instruments {
            cache
                .borrow_mut()
                .add_instrument(instrument.clone())
                .unwrap();
        }
        cache
            .borrow_mut()
            .add_account(AccountAny::Cash(CashAccount::new(state, true, false)))
            .unwrap();
        let clock: Rc<RefCell<dyn Clock>> = Rc::new(RefCell::new(TestClock::new()));
        let execution = Rc::new(RefCell::new(ExecutionEngine::new(
            clock.clone(),
            cache.clone(),
            None,
        )));
        execution
            .borrow_mut()
            .register_client(Box::new(StubExecutionClient::new(
                ClientId::from("STUB"),
                account_id,
                Venue::from("XIDX"),
                OmsType::Netting,
                None,
            )))
            .unwrap();
        let portfolio = Portfolio::new(clock.clone(), cache.clone(), None);
        let queued_events = Rc::new(RefCell::new(Vec::<OrderEventAny>::new()));
        if deferred_events {
            let queue = queued_events.clone();
            msgbus::register_order_event_endpoint(
                MessagingSwitchboard::exec_engine_process(),
                TypedIntoHandler::from(move |event: OrderEventAny| queue.borrow_mut().push(event)),
            );
        } else {
            ExecutionEngine::register_msgbus_handlers(&execution);
        }
        // One venue: its matchers share CASH commitments, as BacktestExchange and Sandbox wire them.
        let cash_commitments = CashCommitments::default();
        let mut instrument_ids = Vec::new();
        let mut matchers = Vec::new();
        for (instrument, raw_id) in instruments.iter().zip(1..) {
            instrument_ids.push(instrument.id());
            let mut matcher = OrderMatchingEngine::new(
                instrument.clone(),
                raw_id,
                FillModelHandle::default(),
                FeeModelHandle::default(),
                BookType::L1_MBP,
                OmsType::Netting,
                AccountType::Cash,
                clock.clone(),
                cache.clone(),
                OrderMatchingEngineConfig {
                    price_protection_points: Some(0),
                    ..Default::default()
                },
            );
            matcher.set_cash_commitments(cash_commitments.clone());
            matchers.push(matcher);
        }
        Self {
            account_id,
            cache,
            execution,
            queued_events,
            instrument_ids,
            matchers,
            _portfolio: portfolio,
        }
    }

    fn instrument_id(&self, matcher: usize) -> InstrumentId {
        self.instrument_ids[matcher]
    }

    fn process_quote(&mut self, matcher: usize, bid: &str, ask: &str, size: &str, ts: u64) {
        let tick = quote(self.instrument_ids[matcher], bid, ask, size, ts);
        self.matchers[matcher].process_quote_tick(&tick);
    }

    // Brings `size` of new liquidity at `price` as a quote, a trade or a LAST bar's ticks.
    fn liquidity(&mut self, matcher: usize, kind: Liquidity, price: &str, size: &str, ts: u64) {
        let instrument_id = self.instrument_ids[matcher];
        let engine = &mut self.matchers[matcher];
        match kind {
            Liquidity::Quote => {
                let bid = Price::from(price) - Price::from("100");
                engine.process_quote_tick(&quote(instrument_id, &bid.to_string(), price, size, ts));
            }
            Liquidity::Trade => engine.process_trade_tick(&TradeTick::new(
                instrument_id,
                Price::from(price),
                Quantity::from(size),
                AggressorSide::NoAggressor,
                TradeId::from(format!("T-{ts}").as_str()),
                UnixNanos::from(ts),
                UnixNanos::from(ts),
            )),
            Liquidity::Bar => {
                // Each of the four synthetic bar ticks carries a quarter of the volume.
                engine.config.bar_execution = true;
                let volume = Quantity::from(size).as_decimal() * Decimal::from(4);
                engine.process_bar(&Bar::new(
                    BarType::from(format!("{instrument_id}-1-MINUTE-LAST-EXTERNAL").as_str()),
                    Price::from(price),
                    Price::from(price),
                    Price::from(price),
                    Price::from(price),
                    Quantity::from(volume.to_string().as_str()),
                    UnixNanos::from(ts),
                    UnixNanos::from(ts),
                ));
            }
        }
    }

    // Simulates the Cache purging an order (e.g. `purge_closed_orders`).
    fn purge(&self, id: &str) {
        self.cache.borrow_mut().purge_order(ClientOrderId::from(id));
        assert!(
            self.cache
                .borrow()
                .order(&ClientOrderId::from(id))
                .is_none()
        );
    }

    fn submit(&mut self, matcher: usize, order: &mut OrderAny) {
        order
            .apply(OrderEventAny::Submitted(
                OrderSubmittedSpec::builder()
                    .trader_id(order.trader_id())
                    .strategy_id(order.strategy_id())
                    .instrument_id(order.instrument_id())
                    .client_order_id(order.client_order_id())
                    .account_id(self.account_id)
                    .build(),
            ))
            .unwrap();
        self.cache
            .borrow_mut()
            .add_order(order.clone(), None, Some(ClientId::from("STUB")), false)
            .unwrap();
        self.process(matcher, order);
    }

    fn process(&mut self, matcher: usize, order: &mut OrderAny) {
        let account_id = self.account_id;
        self.matchers[matcher].process_order(order, account_id);
    }

    fn retry_market(&mut self, matcher: usize, id: &str) {
        let client_order_id = ClientOrderId::from(id);
        self.matchers[matcher].fill_market_order(client_order_id);
    }

    fn cancel(&mut self, matcher: usize, id: &str) {
        let order = self.order(id);
        let command = CancelOrder::new(
            order.trader_id(),
            Some(ClientId::from("STUB")),
            order.strategy_id(),
            order.instrument_id(),
            order.client_order_id(),
            order.venue_order_id(),
            UUID4::new(),
            UnixNanos::default(),
            None,
            None,
        );
        let account_id = self.account_id;
        self.matchers[matcher].process_cancel(&command, account_id);
    }

    // Applies deferred matcher events through the genuine ExecutionEngine (no-op when direct).
    fn drain(&self) {
        loop {
            let events = std::mem::take(&mut *self.queued_events.borrow_mut());
            if events.is_empty() {
                break;
            }
            for event in events {
                self.execution.borrow_mut().process(&event);
            }
        }
    }

    fn queued(&self) -> Vec<(String, &'static str)> {
        let mut queued = Vec::new();
        for event in self.queued_events.borrow().iter() {
            let kind = match event {
                OrderEventAny::Accepted(_) => "Accepted",
                OrderEventAny::Rejected(_) => "Rejected",
                OrderEventAny::Filled(_) => "Filled",
                _ => "Other",
            };
            queued.push((event.client_order_id().to_string(), kind));
        }
        queued
    }

    fn order(&self, id: &str) -> OrderAny {
        self.cache
            .borrow()
            .order(&ClientOrderId::from(id))
            .unwrap()
            .cloned()
    }

    fn fills(&self, id: &str) -> Vec<(Quantity, Price, Option<Money>)> {
        let order = self.order(id);
        let mut fills = Vec::new();
        for event in order.events() {
            if let OrderEventAny::Filled(fill) = event {
                fills.push((fill.last_qty, fill.last_px, fill.commission));
            }
        }
        fills
    }

    fn positions(&self) -> Vec<(String, String, Quantity)> {
        let cache = self.cache.borrow();
        let mut positions = Vec::new();
        for position in cache.positions(None, None, None, None, None) {
            let instrument_id = position.instrument_id.to_string();
            let strategy_id = position.strategy_id.to_string();
            positions.push((instrument_id, strategy_id, position.quantity));
        }
        positions.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        positions
    }

    // Returns (total, locked, free) for `currency`.
    fn balances(&self, currency: Currency) -> (Money, Money, Money) {
        let account = self.cache.borrow().account_owned(&self.account_id).unwrap();
        (
            account.balance_total(Some(currency)).unwrap(),
            account.balance_locked(Some(currency)).unwrap(),
            account.balance_free(Some(currency)).unwrap(),
        )
    }

    fn account_event_count(&self) -> usize {
        let account = self.cache.borrow().account_owned(&self.account_id).unwrap();
        account.events().len()
    }

    fn assert_status(&self, id: &str, status: OrderStatus) {
        assert_eq!(self.order(id).status(), status, "{id}");
    }

    fn assert_fills(&self, id: &str, expected: &[(&str, &str, &str)]) {
        let mut fills = Vec::new();
        for (quantity, price, commission) in expected {
            let commission = Some(idr(commission));
            fills.push((Quantity::from(*quantity), Price::from(*price), commission));
        }
        assert_eq!(self.fills(id), fills, "{id} fills");
    }

    fn assert_positions(&self, expected: &[(&str, &str, &str)]) {
        let mut positions = Vec::new();
        for (instrument_id, strategy_id, quantity) in expected {
            let quantity = Quantity::from(*quantity);
            positions.push((instrument_id.to_string(), strategy_id.to_string(), quantity));
        }
        assert_eq!(self.positions(), positions, "positions");
    }

    fn assert_idr(&self, total: &str, locked: &str, free: &str) {
        let expected = (idr(total), idr(locked), idr(free));
        assert_eq!(
            self.balances(Currency::from("IDR")),
            expected,
            "(total, locked, free)"
        );
    }

    fn assert_cash_batch_rejected(&self, id: &str) {
        self.assert_rejected_with(id, CASH_BATCH_DEBIT);
    }

    fn assert_rejected_with(&self, id: &str, reason_prefix: &str) {
        let order = self.order(id);
        assert_eq!(order.status(), OrderStatus::Rejected, "{id}");
        assert!(order.filled_qty().is_zero(), "{id}");
        let mut reasons = Vec::new();
        for event in order.events() {
            if let OrderEventAny::Rejected(rejected) = event {
                reasons.push(rejected.reason.to_string());
            }
        }
        assert_eq!(reasons.len(), 1, "{id}: {reasons:?}");
        assert!(reasons[0].starts_with(reason_prefix), "{id}: {reasons:?}");
    }

    // Deferred delivery only: every listed order is committed before any native application.
    fn assert_pending(&self, expected: &[(&str, &str)]) {
        let queued = self.queued();
        for (id, kind) in expected {
            assert_eq!(pending_kinds(&queued, id), [*kind], "{id}");
        }
    }

    // Covenant cash identity over native state: total = locked + free with nothing negative,
    // seed - total = sum of Fill debits (qty * px + commission), and Positions hold every Fill.
    fn assert_cash_identity(&self, seed: Money) {
        let (total, locked, free) = self.balances(seed.currency);
        assert_eq!(total.as_decimal(), locked.as_decimal() + free.as_decimal());
        assert!(!total.is_negative() && !locked.is_negative() && !free.is_negative());
        let cache = self.cache.borrow();
        let mut debited = Decimal::ZERO;
        let mut filled = Decimal::ZERO;
        for order in cache.orders(None, None, None, None, None) {
            for event in order.events() {
                if let OrderEventAny::Filled(fill) = event {
                    assert_eq!(fill.currency, seed.currency);
                    filled += fill.last_qty.as_decimal();
                    debited += fill.last_qty.as_decimal() * fill.last_px.as_decimal();
                    if let Some(commission) = fill.commission {
                        assert_eq!(commission.currency, seed.currency);
                        debited += commission.as_decimal();
                    }
                }
            }
        }
        assert_eq!(
            seed.as_decimal() - total.as_decimal(),
            debited,
            "seed - total"
        );
        let mut held = Decimal::ZERO;
        for position in cache.positions(None, None, None, None, None) {
            held += position.quantity.as_decimal();
        }
        assert_eq!(held, filled, "Position quantity");
    }
}

// SA-1: two strategies' MARKET BUYs on one instrument share the single native free balance.
#[rstest]
#[case::second_buy_100("750000", "100", "250000")]
#[case::second_buy_200("1000000", "200", "500000")]
fn cash_market_orders_share_native_free_balance(
    #[case] seed: &str,
    #[case] second_quantity: &str,
    #[case] cash_after_first: &str,
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr(seed);
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    let mut first = buy("CASH-A", instrument_id, "S-A", "100", None, time_in_force);
    let mut second = buy(
        "CASH-B",
        instrument_id,
        "S-B",
        second_quantity,
        None,
        time_in_force,
    );
    harness.submit(0, &mut first);
    harness.submit(0, &mut second);
    if deferred_events {
        assert_one_pending_commitment(&harness.queued(), "CASH-A", "Filled", "CASH-B");
    }
    harness.drain();
    harness.retry_market(0, "CASH-B"); // Explicit public retry edge for a held submission.
    harness.drain();

    harness.assert_status("CASH-A", OrderStatus::Filled);
    harness.assert_fills("CASH-A", &[("100", "5000", "0")]);
    harness.assert_cash_batch_rejected("CASH-B");
    harness.assert_positions(&[("BBRI.XIDX", "S-A", "100")]);
    harness.assert_idr(cash_after_first, "0", cash_after_first);
    harness.assert_cash_identity(seed);
}

// SA-2: MARKET BUYs on two instruments of one venue share the single native free balance.
#[rstest]
fn cash_market_orders_share_free_balance_across_instruments(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("750000");
    let instruments = [
        idx_equity("BBRI", Decimal::ZERO),
        idx_equity("BBCA", Decimal::ZERO),
    ];
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &instruments, deferred_events);
    let bbri = harness.instrument_id(0);
    let bbca = harness.instrument_id(1);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    harness.process_quote(1, "2900", "3000", "1000", 1);
    let mut first = buy("CASH-A", bbri, "S-A", "100", None, time_in_force);
    let mut second = buy("CASH-B", bbca, "S-B", "100", None, time_in_force);
    harness.submit(0, &mut first);
    harness.submit(1, &mut second);
    if deferred_events {
        assert_one_pending_commitment(&harness.queued(), "CASH-A", "Filled", "CASH-B");
    }
    harness.drain();
    harness.retry_market(1, "CASH-B"); // Explicit public retry edge for a held submission.
    harness.drain();

    harness.assert_status("CASH-A", OrderStatus::Filled);
    harness.assert_fills("CASH-A", &[("100", "5000", "0")]);
    harness.assert_status("CASH-B", OrderStatus::Rejected);
    harness.assert_fills("CASH-B", &[]);
    harness.assert_positions(&[("BBRI.XIDX", "S-A", "100")]);
    harness.assert_idr("250000", "0", "250000");
    harness.assert_cash_identity(seed);
}

// SA-3: a held MARKET residual reserves no cash and stays cancelable after other orders spend.
#[rstest]
fn cash_market_held_residual_cancels_without_reserving_cash(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1000100");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "100", 1);
    let mut held = buy("CASH-H", instrument_id, "S-A", "200", None, time_in_force);
    harness.submit(0, &mut held);
    if deferred_events {
        assert_eq!(harness.queued(), [("CASH-H".to_string(), "Filled")]);
    }
    harness.drain();
    // Zero price protection leaves the slipped L1 remainder unfilled, as in the continuation file.
    harness.assert_status("CASH-H", OrderStatus::PartiallyFilled);
    harness.assert_fills("CASH-H", &[("100", "5000", "0")]);
    harness.assert_idr("500100", "0", "500100"); // Open question 4: the residual reserves nothing.

    let mut funded = buy("CASH-F", instrument_id, "S-B", "100", None, time_in_force);
    harness.submit(0, &mut funded);
    harness.drain();
    harness.assert_status("CASH-F", OrderStatus::Filled);
    harness.retry_market(0, "CASH-H");
    harness.drain();
    harness.assert_status("CASH-H", OrderStatus::PartiallyFilled); // Unfunded residual is held.
    harness.assert_idr("100", "0", "100");

    harness.cancel(0, "CASH-H");
    harness.drain();
    let assert_settled = |harness: &CashHarness| {
        harness.assert_status("CASH-H", OrderStatus::Canceled);
        assert_eq!(harness.order("CASH-H").filled_qty(), Quantity::from("100"));
        harness.assert_fills("CASH-H", &[("100", "5000", "0")]);
        harness.assert_status("CASH-F", OrderStatus::Filled);
        harness.assert_fills("CASH-F", &[("100", "5000", "0")]);
        harness.assert_positions(&[("BBRI.XIDX", "S-A", "100"), ("BBRI.XIDX", "S-B", "100")]);
        harness.assert_idr("100", "0", "100");
        harness.assert_cash_identity(seed);
    };
    assert_settled(&harness);
    harness.process_quote(0, "4900", "5000", "100", 2);
    harness.retry_market(0, "CASH-H");
    harness.drain();
    assert_settled(&harness);
}

// L-1: a resting CASH LIMIT BUY locks its notional and releases it on cancel.
#[rstest]
fn cash_limit_resting_order_locks_and_releases_notional(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1000000");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    let mut order = buy(
        "CASH-L",
        instrument_id,
        "S-A",
        "100",
        Some("4900"),
        time_in_force,
    );
    harness.submit(0, &mut order);
    harness.drain();
    harness.assert_status("CASH-L", OrderStatus::Accepted);
    harness.assert_idr("1000000", "490000", "510000");
    harness.assert_cash_identity(seed);

    harness.cancel(0, "CASH-L");
    harness.drain();
    harness.assert_status("CASH-L", OrderStatus::Canceled);
    harness.assert_fills("CASH-L", &[]);
    harness.assert_positions(&[]);
    harness.assert_idr("1000000", "0", "1000000");
    harness.assert_cash_identity(seed);
}

// L-2: a partially filled CASH LIMIT BUY keeps only its residual locked within total.
#[rstest]
fn cash_limit_partial_fill_locks_only_residual(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1000000");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "100", 1);
    let mut order = buy(
        "CASH-L",
        instrument_id,
        "S-A",
        "200",
        Some("5000"),
        time_in_force,
    );
    harness.submit(0, &mut order);
    harness.drain();
    harness.assert_status("CASH-L", OrderStatus::PartiallyFilled);
    harness.assert_fills("CASH-L", &[("100", "5000", "0")]);
    harness.assert_positions(&[("BBRI.XIDX", "S-A", "100")]);
    harness.assert_idr("500000", "500000", "0");
    harness.assert_cash_identity(seed);

    harness.cancel(0, "CASH-L");
    harness.drain();
    harness.assert_status("CASH-L", OrderStatus::Canceled);
    harness.assert_fills("CASH-L", &[("100", "5000", "0")]);
    harness.assert_idr("500000", "0", "500000");
    harness.assert_cash_identity(seed);
}

// L-3: a resting CASH LIMIT BUY reservation also covers its worst-case commission.
#[rstest]
#[case::sufficient("1001000", true)]
#[case::insufficient("1000000", false)]
fn cash_limit_lock_includes_commission(
    #[case] seed: &str,
    #[case] funded: bool,
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr(seed);
    let bbri = idx_equity("BBRI", Decimal::new(1, 3)); // Maker and taker fee 0.001.
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5100", "200", 1);
    let mut order = buy(
        "CASH-L",
        instrument_id,
        "S-A",
        "200",
        Some("5000"),
        time_in_force,
    );
    harness.submit(0, &mut order);
    harness.drain();
    harness.assert_cash_identity(seed);
    if funded {
        // PENDING USER DECISION (open question 1): these two assertions encode a fee-inclusive
        // reservation, notional 1000000 plus worst-case commission 1000. A notional-only
        // decision would expect locked 1000000 and free 1000 instead; the post-Fill assertions
        // hold either way.
        harness.assert_status("CASH-L", OrderStatus::Accepted);
        harness.assert_idr("1001000", "1001000", "0");
    }

    harness.process_quote(0, "4900", "5000", "200", 2);
    harness.drain();
    if funded {
        harness.assert_status("CASH-L", OrderStatus::Filled);
        harness.assert_fills("CASH-L", &[("200", "5000", "1000")]);
        harness.assert_positions(&[("BBRI.XIDX", "S-A", "200")]);
        harness.assert_idr("0", "0", "0");
    } else {
        // The commission makes any Fill unfundable, so Covenant never fills: the order is refused
        // at submission (fee-inclusive lock) or when the Fill would overdraw, releasing all cash.
        let status = harness.order("CASH-L").status();
        assert!(
            matches!(status, OrderStatus::Rejected | OrderStatus::Canceled),
            "{status:?}"
        );
        harness.assert_fills("CASH-L", &[]);
        harness.assert_positions(&[]);
        harness.assert_idr("1000000", "0", "1000000");
    }
    harness.assert_cash_identity(seed);
}

// L-4: two LIMIT BUYs submitted before native application share the single free balance.
#[rstest]
fn cash_limit_deferred_orders_share_native_free_balance(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
) {
    reset_native_globals();
    let seed = idr("750000");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], true);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5100", "1000", 1);
    let mut first = buy(
        "CASH-A",
        instrument_id,
        "S-A",
        "100",
        Some("5000"),
        time_in_force,
    );
    let mut second = buy(
        "CASH-B",
        instrument_id,
        "S-B",
        "100",
        Some("5000"),
        time_in_force,
    );
    harness.submit(0, &mut first);
    harness.submit(0, &mut second);
    assert_one_pending_commitment(&harness.queued(), "CASH-A", "Accepted", "CASH-B");
    harness.drain();
    if !harness.order("CASH-B").is_closed() {
        // Held path (open question 2): resubmission is the public retry edge for an order the
        // venue never acknowledged; an acknowledged order is ignored by `process_order`.
        let mut held = harness.order("CASH-B");
        harness.process(0, &mut held);
        harness.drain();
    }

    harness.assert_status("CASH-A", OrderStatus::Accepted);
    harness.assert_status("CASH-B", OrderStatus::Rejected);
    harness.assert_fills("CASH-A", &[]);
    harness.assert_fills("CASH-B", &[]);
    harness.assert_idr("750000", "500000", "250000");
    harness.assert_cash_identity(seed);
}

// M-1: funding is decided at the live quote when the matcher processes the order, not at the
// pre-trade view a strategy recorded earlier.
#[rstest]
#[case::market_unfunded_after_move("500000", None, "5001", None, "500000")]
#[case::market_funded_after_move("500100", None, "5001", Some("5001"), "0")]
#[case::limit_improved_after_move("500000", Some("5000"), "4990", Some("4990"), "1000")]
fn cash_funding_prices_live_quote_at_processing(
    #[case] seed: &str,
    #[case] limit_price: Option<&str>,
    #[case] moved_ask: &str,
    #[case] fill_price: Option<&str>,
    #[case] cash: &str,
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr(seed);
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    let (_, _, pre_trade_free) = harness.balances(seed.currency);
    assert!(pre_trade_free >= idr("500000")); // 100 at the 5000 ask looked fundable.
    harness.process_quote(0, "4900", moved_ask, "1000", 2);
    let mut order = buy(
        "CASH-M",
        instrument_id,
        "S-A",
        "100",
        limit_price,
        time_in_force,
    );
    harness.submit(0, &mut order);
    harness.drain();
    if let Some(price) = fill_price {
        harness.assert_status("CASH-M", OrderStatus::Filled);
        harness.assert_fills("CASH-M", &[("100", price, "0")]);
        harness.assert_positions(&[("BBRI.XIDX", "S-A", "100")]);
    } else {
        harness.assert_status("CASH-M", OrderStatus::Rejected);
        harness.assert_fills("CASH-M", &[]);
        harness.assert_positions(&[]);
    }
    harness.assert_idr(cash, "0", cash);
    harness.assert_cash_identity(seed);
}

// NEG: a CASH Account that is not single-currency IDR cannot fund any BUY (Foundation is IDR-only).
#[rstest]
#[case::market(None)]
#[case::limit_resting(Some("4900"))]
#[case::limit_marketable(Some("5000"))]
fn cash_buy_rejects_non_single_currency_account(
    #[case] limit_price: Option<&str>,
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1000000");
    let usd = Money::from("100 USD");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed, usd], None, &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    let mut order = buy(
        "CASH-N",
        instrument_id,
        "S-A",
        "100",
        limit_price,
        time_in_force,
    );
    harness.submit(0, &mut order);
    harness.drain();
    harness.assert_status("CASH-N", OrderStatus::Rejected);
    harness.assert_fills("CASH-N", &[]);
    harness.assert_positions(&[]);
    harness.assert_idr("1000000", "0", "1000000");
    let usd_zero = Money::zero(usd.currency);
    assert_eq!(harness.balances(usd.currency), (usd, usd_zero, usd));
    assert_eq!(harness.account_event_count(), 1);
    harness.assert_cash_identity(seed);
}

// C-1: three strategies' concurrent MARKET BUYs on one symbol all fill while funds suffice.
#[rstest]
fn cash_concurrent_market_orders_fill_across_strategies(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1600000");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    let mut first = buy("CASH-A", instrument_id, "S-A", "100", None, time_in_force);
    let mut second = buy("CASH-B", instrument_id, "S-B", "100", None, time_in_force);
    let mut third = buy("CASH-C", instrument_id, "S-C", "100", None, time_in_force);
    harness.submit(0, &mut first);
    harness.submit(0, &mut second);
    harness.submit(0, &mut third);
    if deferred_events {
        harness.assert_pending(&[
            ("CASH-A", "Filled"),
            ("CASH-B", "Filled"),
            ("CASH-C", "Filled"),
        ]);
    }
    harness.drain();

    for id in ["CASH-A", "CASH-B", "CASH-C"] {
        harness.assert_status(id, OrderStatus::Filled);
        harness.assert_fills(id, &[("100", "5000", "0")]);
    }
    harness.assert_positions(&[
        ("BBRI.XIDX", "S-A", "100"),
        ("BBRI.XIDX", "S-B", "100"),
        ("BBRI.XIDX", "S-C", "100"),
    ]);
    harness.assert_idr("100000", "0", "100000");
    harness.assert_cash_identity(seed);
}

// C-2: concurrent MARKET BUYs across symbols and strategies all fill while funds suffice.
#[rstest]
fn cash_concurrent_market_orders_fill_across_instruments(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1000000");
    let instruments = [
        idx_equity("BBRI", Decimal::ZERO),
        idx_equity("BBCA", Decimal::ZERO),
    ];
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &instruments, deferred_events);
    let bbri = harness.instrument_id(0);
    let bbca = harness.instrument_id(1);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    harness.process_quote(1, "2900", "3000", "1000", 1);
    let mut first = buy("CASH-A", bbri, "S-A", "100", None, time_in_force);
    let mut second = buy("CASH-B", bbca, "S-B", "100", None, time_in_force);
    let mut third = buy("CASH-C", bbri, "S-B", "20", None, time_in_force);
    harness.submit(0, &mut first);
    harness.submit(1, &mut second);
    harness.submit(0, &mut third);
    if deferred_events {
        harness.assert_pending(&[
            ("CASH-A", "Filled"),
            ("CASH-B", "Filled"),
            ("CASH-C", "Filled"),
        ]);
    }
    harness.drain();

    harness.assert_status("CASH-A", OrderStatus::Filled);
    harness.assert_fills("CASH-A", &[("100", "5000", "0")]);
    harness.assert_status("CASH-B", OrderStatus::Filled);
    harness.assert_fills("CASH-B", &[("100", "3000", "0")]);
    harness.assert_status("CASH-C", OrderStatus::Filled);
    harness.assert_fills("CASH-C", &[("20", "5000", "0")]);
    harness.assert_positions(&[
        ("BBCA.XIDX", "S-B", "100"),
        ("BBRI.XIDX", "S-A", "100"),
        ("BBRI.XIDX", "S-B", "20"),
    ]);
    harness.assert_idr("100000", "0", "100000");
    harness.assert_cash_identity(seed);
}

// C-3: a resting LIMIT reservation and a concurrent MARKET BUY are both funded.
#[rstest]
fn cash_concurrent_limit_and_market_orders_share_free_balance(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1000000");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    let mut limit = buy(
        "CASH-A",
        instrument_id,
        "S-A",
        "100",
        Some("4900"),
        time_in_force,
    );
    let mut market = buy("CASH-B", instrument_id, "S-B", "100", None, time_in_force);
    harness.submit(0, &mut limit);
    harness.submit(0, &mut market);
    if deferred_events {
        harness.assert_pending(&[("CASH-A", "Accepted"), ("CASH-B", "Filled")]);
    }
    harness.drain();

    harness.assert_status("CASH-A", OrderStatus::Accepted);
    harness.assert_fills("CASH-A", &[]);
    harness.assert_status("CASH-B", OrderStatus::Filled);
    harness.assert_fills("CASH-B", &[("100", "5000", "0")]);
    harness.assert_positions(&[("BBRI.XIDX", "S-B", "100")]);
    harness.assert_idr("500000", "490000", "10000");
    harness.assert_cash_identity(seed);

    harness.cancel(0, "CASH-A");
    harness.drain();
    harness.assert_status("CASH-A", OrderStatus::Canceled);
    harness.assert_idr("500000", "0", "500000");
    harness.assert_cash_identity(seed);
}

// C-4a: concurrent MARKET BUYs whose debits with commission exactly exhaust the free balance.
#[rstest]
fn cash_concurrent_market_orders_fund_commissions_exactly(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1001000");
    let bbri = idx_equity("BBRI", Decimal::new(1, 3)); // Maker and taker fee 0.001.
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    let mut first = buy("CASH-A", instrument_id, "S-A", "100", None, time_in_force);
    let mut second = buy("CASH-B", instrument_id, "S-B", "100", None, time_in_force);
    harness.submit(0, &mut first);
    harness.submit(0, &mut second);
    if deferred_events {
        harness.assert_pending(&[("CASH-A", "Filled"), ("CASH-B", "Filled")]);
    }
    harness.drain();

    for id in ["CASH-A", "CASH-B"] {
        harness.assert_status(id, OrderStatus::Filled);
        harness.assert_fills(id, &[("100", "5000", "500")]);
    }
    harness.assert_positions(&[("BBRI.XIDX", "S-A", "100"), ("BBRI.XIDX", "S-B", "100")]);
    harness.assert_idr("0", "0", "0");
    harness.assert_cash_identity(seed);
}

// C-4b: a LIMIT reservation is admitted against exactly the free balance a concurrent MARKET BUY
// leaves.
#[rstest]
fn cash_concurrent_limit_admits_exact_remaining_free_balance(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("990000");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    let mut market = buy("CASH-A", instrument_id, "S-A", "100", None, time_in_force);
    let mut limit = buy(
        "CASH-B",
        instrument_id,
        "S-B",
        "100",
        Some("4900"),
        time_in_force,
    );
    harness.submit(0, &mut market);
    harness.submit(0, &mut limit);
    if deferred_events {
        harness.assert_pending(&[("CASH-A", "Filled"), ("CASH-B", "Accepted")]);
    }
    harness.drain();

    harness.assert_status("CASH-A", OrderStatus::Filled);
    harness.assert_fills("CASH-A", &[("100", "5000", "0")]);
    harness.assert_status("CASH-B", OrderStatus::Accepted);
    harness.assert_fills("CASH-B", &[]);
    harness.assert_positions(&[("BBRI.XIDX", "S-A", "100")]);
    harness.assert_idr("490000", "490000", "0");
    harness.assert_cash_identity(seed);
}

// C-5: among concurrent orders only the one that would overspend the shared free balance is
// refused, whether it is a MARKET or a LIMIT BUY.
#[rstest]
#[case::market(None, CASH_BATCH_DEBIT)]
#[case::limit(Some("4900"), CASH_LIMIT)]
fn cash_concurrent_order_overspending_free_balance_is_rejected(
    #[case] limit_price: Option<&str>,
    #[case] reason_prefix: &str,
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1200000");
    let instruments = [
        idx_equity("BBRI", Decimal::ZERO),
        idx_equity("BBCA", Decimal::ZERO),
    ];
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &instruments, deferred_events);
    let bbri = harness.instrument_id(0);
    let bbca = harness.instrument_id(1);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    harness.process_quote(1, "2900", "3000", "1000", 1);
    let mut first = buy("CASH-A", bbri, "S-A", "100", None, time_in_force);
    let mut second = buy("CASH-B", bbca, "S-B", "100", None, time_in_force);
    let mut third = buy("CASH-C", bbri, "S-C", "100", limit_price, time_in_force);
    harness.submit(0, &mut first);
    harness.submit(1, &mut second);
    harness.submit(0, &mut third);
    if deferred_events {
        harness.assert_pending(&[
            ("CASH-A", "Filled"),
            ("CASH-B", "Filled"),
            ("CASH-C", "Rejected"),
        ]);
    }
    harness.drain();

    harness.assert_status("CASH-A", OrderStatus::Filled);
    harness.assert_fills("CASH-A", &[("100", "5000", "0")]);
    harness.assert_status("CASH-B", OrderStatus::Filled);
    harness.assert_fills("CASH-B", &[("100", "3000", "0")]);
    harness.assert_rejected_with("CASH-C", reason_prefix);
    harness.assert_fills("CASH-C", &[]);
    harness.assert_positions(&[("BBCA.XIDX", "S-B", "100"), ("BBRI.XIDX", "S-A", "100")]);
    harness.assert_idr("400000", "0", "400000");
    harness.assert_cash_identity(seed);
}

// One Strategy may hold several concurrent MARKET BUYs on one symbol; all fill while funds suffice.
#[rstest]
fn cash_concurrent_same_strategy_orders_fill_on_one_symbol(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1000000");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    let mut first = buy("CASH-A1", instrument_id, "S-A", "100", None, time_in_force);
    let mut second = buy("CASH-A2", instrument_id, "S-A", "100", None, time_in_force);
    harness.submit(0, &mut first);
    harness.submit(0, &mut second);
    if deferred_events {
        harness.assert_pending(&[("CASH-A1", "Filled"), ("CASH-A2", "Filled")]);
    }
    harness.drain();

    for id in ["CASH-A1", "CASH-A2"] {
        harness.assert_status(id, OrderStatus::Filled);
        harness.assert_fills(id, &[("100", "5000", "0")]);
    }
    harness.assert_positions(&[("BBRI.XIDX", "S-A", "200")]);
    harness.assert_idr("0", "0", "0");
    harness.assert_cash_identity(seed);
}

// R-1: a held CASH MARKET remainder keeps working like a live order and fills natively on later
// quote, trade or bar liquidity, with exact cash.
#[rstest]
fn cash_market_held_remainder_refills_on_later_liquidity(
    #[values(Liquidity::Quote, Liquidity::Trade, Liquidity::Bar)] liquidity: Liquidity,
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1000000");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "100", 1);
    let mut order = buy("CASH-H", instrument_id, "S-A", "200", None, time_in_force);
    harness.submit(0, &mut order);
    harness.drain();
    harness.assert_status("CASH-H", OrderStatus::PartiallyFilled);
    harness.assert_idr("500000", "0", "500000"); // The held remainder reserves nothing.

    harness.liquidity(0, liquidity, "5000", "100", 2);
    harness.drain();
    harness.assert_status("CASH-H", OrderStatus::Filled);
    harness.assert_fills("CASH-H", &[("100", "5000", "0"), ("100", "5000", "0")]);
    harness.assert_positions(&[("BBRI.XIDX", "S-A", "200")]);
    harness.assert_idr("0", "0", "0");
    harness.assert_cash_identity(seed);
}

// R-2: a re-fill obeys the MARKET funding guard (the complete batch or nothing) and the new
// liquidity; whatever stays held remains cancelable.
#[rstest]
#[case::refused_by_funds("750000", "5000", "100", None, "100", "250000")]
#[case::funded_at_lower_price("750000", "2500", "100", Some(("100", "2500")), "200", "0")]
#[case::limited_by_liquidity("1000000", "5000", "50", Some(("50", "5000")), "150", "250000")]
fn cash_market_held_remainder_refill_follows_funding_and_liquidity(
    #[case] seed: &str,
    #[case] later_ask: &str,
    #[case] later_size: &str,
    #[case] refill: Option<(&str, &str)>,
    #[case] filled: &str,
    #[case] cash: &str,
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr(seed);
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "100", 1);
    let mut order = buy("CASH-H", instrument_id, "S-A", "200", None, time_in_force);
    harness.submit(0, &mut order);
    harness.drain();
    harness.assert_status("CASH-H", OrderStatus::PartiallyFilled);

    harness.liquidity(0, Liquidity::Quote, later_ask, later_size, 2);
    harness.drain();
    let mut fills = vec![("100", "5000", "0")];
    if let Some((quantity, price)) = refill {
        fills.push((quantity, price, "0"));
    }
    harness.assert_fills("CASH-H", &fills);
    assert_eq!(harness.order("CASH-H").filled_qty(), Quantity::from(filled));
    harness.assert_positions(&[("BBRI.XIDX", "S-A", filled)]);
    harness.assert_idr(cash, "0", cash);
    harness.assert_cash_identity(seed);
    if filled == "200" {
        harness.assert_status("CASH-H", OrderStatus::Filled);
        return;
    }

    harness.assert_status("CASH-H", OrderStatus::PartiallyFilled);
    harness.cancel(0, "CASH-H");
    harness.drain();
    harness.assert_status("CASH-H", OrderStatus::Canceled);
    harness.liquidity(0, Liquidity::Quote, "2500", "1000", 3); // A canceled remainder never refills.
    harness.drain();
    harness.assert_fills("CASH-H", &fills);
    harness.assert_idr(cash, "0", cash);
    harness.assert_cash_identity(seed);
}

// Review finding 2: a commitment whose order the Cache no longer holds reserves nothing.
#[rstest]
fn cash_commitment_without_cache_order_is_released(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1000000");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], deferred_events);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    let mut first = buy("CASH-A", instrument_id, "S-A", "100", None, time_in_force);
    harness.submit(0, &mut first);
    harness.drain();
    harness.assert_status("CASH-A", OrderStatus::Filled);
    harness.purge("CASH-A");

    let mut second = buy("CASH-B", instrument_id, "S-B", "100", None, time_in_force);
    harness.submit(0, &mut second);
    harness.drain();
    harness.assert_status("CASH-B", OrderStatus::Filled);
    harness.assert_fills("CASH-B", &[("100", "5000", "0")]);
    harness.assert_positions(&[("BBRI.XIDX", "S-A", "100"), ("BBRI.XIDX", "S-B", "100")]);
    harness.assert_idr("0", "0", "0");
}

// Review finding 5a: refusing the overspending order leaks no reservation; a later affordable
// order fills.
#[rstest]
#[case::market(None)]
#[case::limit(Some("4900"))]
fn cash_concurrent_rejection_leaves_later_order_funded(
    #[case] limit_price: Option<&str>,
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    reset_native_globals();
    let seed = idr("1200000");
    let instruments = [
        idx_equity("BBRI", Decimal::ZERO),
        idx_equity("BBCA", Decimal::ZERO),
    ];
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &instruments, deferred_events);
    let bbri = harness.instrument_id(0);
    let bbca = harness.instrument_id(1);
    harness.process_quote(0, "4900", "5000", "1000", 1);
    harness.process_quote(1, "2900", "3000", "1000", 1);
    let mut first = buy("CASH-A", bbri, "S-A", "100", None, time_in_force);
    let mut second = buy("CASH-B", bbca, "S-B", "100", None, time_in_force);
    let mut third = buy("CASH-C", bbri, "S-C", "100", limit_price, time_in_force);
    harness.submit(0, &mut first);
    harness.submit(1, &mut second);
    harness.submit(0, &mut third);
    harness.drain();
    harness.assert_status("CASH-C", OrderStatus::Rejected);

    let mut fourth = buy("CASH-D", bbca, "S-D", "100", None, time_in_force);
    harness.submit(1, &mut fourth);
    harness.drain();
    harness.assert_status("CASH-D", OrderStatus::Filled);
    harness.assert_fills("CASH-D", &[("100", "3000", "0")]);
    harness.assert_positions(&[
        ("BBCA.XIDX", "S-B", "100"),
        ("BBCA.XIDX", "S-D", "100"),
        ("BBRI.XIDX", "S-A", "100"),
    ]);
    harness.assert_idr("100000", "0", "100000");
    harness.assert_cash_identity(seed);
}

// Review finding 5b: before its own Fill is applied, a held remainder neither re-fills on new
// liquidity nor on an explicit retry; once applied, the next liquidity fills it.
#[rstest]
fn cash_market_held_remainder_waits_for_own_fill_application(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
) {
    reset_native_globals();
    let seed = idr("1000000");
    let bbri = idx_equity("BBRI", Decimal::ZERO);
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], true);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5000", "100", 1);
    let mut order = buy("CASH-H", instrument_id, "S-A", "200", None, time_in_force);
    harness.submit(0, &mut order);
    harness.process_quote(0, "4900", "5000", "100", 2);
    harness.retry_market(0, "CASH-H");
    assert_eq!(harness.queued(), [("CASH-H".to_string(), "Filled")]);
    harness.drain();
    harness.assert_status("CASH-H", OrderStatus::PartiallyFilled);
    harness.assert_idr("500000", "0", "500000");

    harness.process_quote(0, "4900", "5000", "100", 3);
    harness.drain();
    harness.assert_status("CASH-H", OrderStatus::Filled);
    harness.assert_fills("CASH-H", &[("100", "5000", "0"), ("100", "5000", "0")]);
    harness.assert_idr("0", "0", "0");
    harness.assert_cash_identity(seed);
}

// Review finding 5c: an unapplied maker Fill below the fee-inclusive LIMIT lock credits nothing
// before application (pending is clamped at zero), so a concurrent MARKET BUY sees only the
// applied free balance; once applied, the released lock funds it.
#[rstest]
fn cash_limit_unapplied_fill_credits_nothing_before_application(
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
) {
    reset_native_globals();
    let seed = idr("1101000");
    // Maker 0.0005 fills below the lock's worse (taker 0.001) commission.
    let bbri = idx_equity_with_fees("BBRI", Decimal::new(5, 4), Decimal::new(1, 3));
    let mut harness = CashHarness::new(&[seed], Some(seed.currency), &[bbri], true);
    let instrument_id = harness.instrument_id(0);
    harness.process_quote(0, "4900", "5100", "100", 1);
    let mut limit = buy(
        "CASH-L",
        instrument_id,
        "S-A",
        "200",
        Some("5000"),
        time_in_force,
    );
    harness.submit(0, &mut limit);
    harness.drain();
    harness.assert_idr("1101000", "1001000", "100000");

    harness.process_quote(0, "4900", "5000", "100", 2);
    let mut early = buy("CASH-B", instrument_id, "S-B", "20", None, time_in_force);
    harness.submit(0, &mut early); // Needs 100100 of the 100000 applied free balance.
    harness.assert_pending(&[("CASH-L", "Filled"), ("CASH-B", "Rejected")]);
    harness.drain();
    harness.assert_cash_batch_rejected("CASH-B");
    harness.assert_fills("CASH-L", &[("100", "5000", "250")]);
    harness.assert_idr("600750", "500500", "100250");

    let mut later = buy("CASH-C", instrument_id, "S-C", "20", None, time_in_force);
    harness.submit(0, &mut later);
    harness.drain();
    harness.assert_status("CASH-C", OrderStatus::Filled);
    harness.assert_fills("CASH-C", &[("20", "5000", "100")]);
    harness.assert_idr("500650", "500500", "150");
    harness.assert_cash_identity(seed);
}
