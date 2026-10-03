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

// Native CASH continuation conformance. Keep this in backtest so ExecutionEngine and Portfolio
// process genuine matcher events over one Cache; neither execution nor portfolio depends on both.

use std::{cell::RefCell, rc::Rc};

use nautilus_common::{
    cache::Cache,
    clock::{Clock, TestClock},
    msgbus::{self, MessageBus, MessagingSwitchboard, TypedIntoHandler},
};
use nautilus_core::{UUID4, UnixNanos};
use nautilus_execution::{
    engine::{ExecutionEngine, stubs::StubExecutionClient},
    matching_engine::{OrderMatchingEngine, config::OrderMatchingEngineConfig},
    models::{
        fee::{FeeModelAny, FeeModelHandle, FixedFeeModel},
        fill::FillModelHandle,
    },
};
use nautilus_model::{
    accounts::{Account, AccountAny, CashAccount},
    data::QuoteTick,
    enums::{
        AccountType, BookType, CurrencyType, OmsType, OrderSide, OrderStatus, OrderType,
        TimeInForce,
    },
    events::{AccountState, OrderEventAny, order::spec::OrderSubmittedSpec},
    identifiers::{AccountId, ClientId, ClientOrderId, InstrumentId, Symbol, Venue},
    instruments::{Instrument, InstrumentAny, stubs::equity_aapl},
    orders::{Order, OrderTestBuilder},
    types::{AccountBalance, Currency, Money, Price, Quantity},
};
use nautilus_portfolio::Portfolio;
use rstest::rstest;
use rust_decimal::Decimal;

#[derive(Clone, Copy)]
enum CashFee {
    Zero,
    EveryFill,
    Once,
}

#[rstest]
#[case::zero_insufficient("1000000 IDR", CashFee::Zero, "500000 IDR", false)]
#[case::zero_sufficient("1000100 IDR", CashFee::Zero, "500100 IDR", true)]
#[case::per_fill_fee_insufficient("1000200 IDR", CashFee::EveryFill, "500100 IDR", false)]
#[case::per_fill_fee_sufficient("1000300 IDR", CashFee::EveryFill, "500200 IDR", true)]
#[case::once_fee_sufficient("1000200 IDR", CashFee::Once, "500100 IDR", true)]
fn cash_market_partial_continuation_uses_native_free_balance(
    #[case] seed: &str,
    #[case] fee: CashFee,
    #[case] first_cash: &str,
    #[case] funded: bool,
    #[values(TimeInForce::Day, TimeInForce::Gtc)] time_in_force: TimeInForce,
    #[values(false, true)] deferred_events: bool,
) {
    *msgbus::get_message_bus().borrow_mut() = MessageBus::default();
    Currency::register(
        Currency::new("IDR", 2, 360, "Indonesian rupiah", CurrencyType::Fiat),
        false,
    )
    .unwrap();
    let mut equity = equity_aapl();
    equity.id = InstrumentId::from("BBRI.XIDX");
    equity.raw_symbol = Symbol::from("BBRI");
    equity.currency = Currency::from("IDR");
    equity.price_precision = 0;
    equity.price_increment = Price::from("1");
    equity.lot_size = Some(Quantity::from("100"));
    equity.maker_fee = Decimal::ZERO;
    equity.taker_fee = Decimal::ZERO;
    let instrument = InstrumentAny::Equity(equity);
    let account_id = AccountId::from("XIDX-001");
    let seed = Money::from(seed);
    let state = AccountState::new(
        account_id,
        AccountType::Cash,
        vec![AccountBalance::new(seed, Money::zero(seed.currency), seed)],
        vec![],
        true,
        UUID4::new(),
        UnixNanos::default(),
        UnixNanos::default(),
        Some(seed.currency),
    );
    let cache = Rc::new(RefCell::new(Cache::default()));
    cache
        .borrow_mut()
        .add_instrument(instrument.clone())
        .unwrap();
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
    let _portfolio = Portfolio::new(clock.clone(), cache.clone(), None);
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
    let fee_model = match fee {
        CashFee::Zero => FeeModelHandle::default(),
        CashFee::EveryFill | CashFee::Once => FeeModelAny::Fixed(
            FixedFeeModel::new(Money::from("100 IDR"), Some(matches!(fee, CashFee::Once))).unwrap(),
        )
        .into(),
    };
    let mut matcher = OrderMatchingEngine::new(
        instrument.clone(),
        1,
        FillModelHandle::default(),
        fee_model,
        BookType::L1_MBP,
        OmsType::Netting,
        AccountType::Cash,
        clock,
        cache.clone(),
        OrderMatchingEngineConfig {
            price_protection_points: Some(0),
            ..Default::default()
        },
    );
    let quote = |ask: &str, ts: u64| {
        QuoteTick::new(
            instrument.id(),
            Price::from("5000"),
            Price::from(ask),
            Quantity::from("100"),
            Quantity::from("100"),
            UnixNanos::from(ts),
            UnixNanos::from(ts),
        )
    };
    matcher.process_quote_tick(&quote("5000", 1));
    let mut order = OrderTestBuilder::new(OrderType::Market)
        .instrument_id(instrument.id())
        .side(OrderSide::Buy)
        .quantity(Quantity::from("200"))
        .time_in_force(time_in_force)
        .client_order_id(ClientOrderId::from("CASH-CONTINUATION-1"))
        .build();
    order
        .apply(OrderEventAny::Submitted(
            OrderSubmittedSpec::builder()
                .trader_id(order.trader_id())
                .strategy_id(order.strategy_id())
                .instrument_id(order.instrument_id())
                .client_order_id(order.client_order_id())
                .account_id(account_id)
                .build(),
        ))
        .unwrap();
    cache
        .borrow_mut()
        .add_order(order.clone(), None, Some(ClientId::from("STUB")), false)
        .unwrap();
    matcher.process_order(&mut order, account_id);

    if deferred_events {
        let initial_events = queued_events.borrow().clone();
        let fill_count = |events: &[OrderEventAny]| {
            events
                .iter()
                .filter(|event| matches!(event, OrderEventAny::Filled(_)))
                .count()
        };
        assert_eq!(fill_count(&initial_events), 1);
        {
            let pending_cache = cache.borrow();
            assert_eq!(
                pending_cache
                    .order(&order.client_order_id())
                    .unwrap()
                    .filled_qty(),
                Quantity::from("0")
            );
            assert!(
                pending_cache
                    .positions(None, None, None, None, None)
                    .is_empty()
            );
            assert_eq!(
                pending_cache
                    .account_owned(&account_id)
                    .unwrap()
                    .balance_free(Some(Currency::from("IDR"))),
                Some(seed)
            );
        }
        for _ in 0..2 {
            matcher.fill_market_order(order.client_order_id());
            let pending = queued_events.borrow();
            assert_eq!(pending.len(), initial_events.len());
            assert_eq!(fill_count(&pending), 1);
        }
        for event in std::mem::take(&mut *queued_events.borrow_mut()) {
            execution.borrow_mut().process(&event);
        }
        assert!(queued_events.borrow().is_empty());
        ExecutionEngine::register_msgbus_handlers(&execution);
    }

    let assert_snapshot = |status: OrderStatus,
                           filled: &str,
                           cash: &str,
                           second_price: Option<&str>| {
        let cache = cache.borrow();
        let cached_order = cache.order(&order.client_order_id()).unwrap();
        assert_eq!(cached_order.status(), status);
        assert_eq!(cached_order.filled_qty(), Quantity::from(filled));
        let fills: Vec<_> = cached_order
            .events()
            .into_iter()
            .filter_map(|event| match event {
                OrderEventAny::Filled(fill) => Some((fill.last_qty, fill.last_px, fill.commission)),
                _ => None,
            })
            .collect();
        let first_fee = if matches!(fee, CashFee::Zero) {
            "0 IDR"
        } else {
            "100 IDR"
        };
        let second_fee = if matches!(fee, CashFee::EveryFill) {
            "100 IDR"
        } else {
            "0 IDR"
        };
        let mut expected_fills = vec![(
            Quantity::from("100"),
            Price::from("5000"),
            Some(Money::from(first_fee)),
        )];
        if let Some(price) = second_price {
            expected_fills.push((
                Quantity::from("100"),
                Price::from(price),
                Some(Money::from(second_fee)),
            ));
        }
        assert_eq!(fills, expected_fills);
        let positions = cache.positions(None, None, None, None, None);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].quantity, Quantity::from(filled));
        let account = cache.account_owned(&account_id).unwrap();
        assert_eq!(
            account.balance_total(Some(Currency::from("IDR"))),
            Some(Money::from(cash))
        );
        assert_eq!(
            account.balance_free(Some(Currency::from("IDR"))),
            Some(Money::from(cash))
        );
    };
    assert_snapshot(OrderStatus::PartiallyFilled, "100", first_cash, None);
    // The held remainder keeps working: later quote liquidity re-fills it natively when funded.
    matcher.process_quote_tick(&quote("5001", 2));
    if funded {
        assert_snapshot(OrderStatus::Filled, "200", "0 IDR", Some("5001"));
        matcher.fill_market_order(order.client_order_id()); // Explicit public retry edge.
        assert_snapshot(OrderStatus::Filled, "200", "0 IDR", Some("5001"));
    } else {
        assert_snapshot(OrderStatus::PartiallyFilled, "100", first_cash, None);
        for _ in 0..2 {
            matcher.fill_market_order(order.client_order_id()); // Explicit public retry edge.
            assert_snapshot(OrderStatus::PartiallyFilled, "100", first_cash, None);
        }
        matcher.process_quote_tick(&quote("5000", 3));
        assert_snapshot(OrderStatus::Filled, "200", "0 IDR", Some("5000"));
        matcher.fill_market_order(order.client_order_id());
        assert_snapshot(OrderStatus::Filled, "200", "0 IDR", Some("5000"));
    }
}
