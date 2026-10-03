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

//! Tracks CASH that a venue's matching engines committed to BUY orders before the Account applied
//! it.

use std::{cell::RefCell, rc::Rc};

use indexmap::IndexMap;
use nautilus_common::cache::Cache;
use nautilus_model::{
    accounts::{Account, CashAccount},
    enums::{LiquiditySide, OrderSide},
    identifiers::{AccountId, ClientOrderId, InstrumentId},
    instruments::{Instrument, InstrumentAny},
    orders::{Order, OrderAny},
    types::{Currency, Money, Price, Quantity},
};

/// Shared CASH commitments for a venue's matching engines.
///
/// A matching engine commits cash when it generates a BUY fill or admits a LIMIT BUY. Until the
/// Portfolio applies that fill or lock, the Account free balance overstates the cash available to
/// the venue's other orders. The unapplied part is derived from the Cache at check time.
#[derive(Debug, Clone, Default)]
pub struct CashCommitments {
    commitments: Rc<RefCell<IndexMap<ClientOrderId, CashCommitment>>>,
}

#[derive(Debug, Clone)]
struct CashCommitment {
    account_id: AccountId,
    instrument_id: InstrumentId,
    quantity: Quantity,
    /// Generated fills as (cumulative filled quantity after the fill, notional plus commission).
    fills: Vec<(Quantity, Money)>,
    /// The price of an admitted LIMIT reservation.
    limit_lock: Option<Price>,
}

impl CashCommitment {
    fn new(account_id: AccountId, instrument_id: InstrumentId, quantity: Quantity) -> Self {
        Self {
            account_id,
            instrument_id,
            quantity,
            fills: Vec::new(),
            limit_lock: None,
        }
    }

    fn generated_qty(&self) -> Quantity {
        self.fills
            .last()
            .map_or_else(|| Quantity::zero(self.quantity.precision), |(cum, _)| *cum)
    }
}

impl CashCommitments {
    /// Records an admitted LIMIT BUY reservation at `price` for the order's quantity.
    pub fn record_limit_lock(&self, order: &OrderAny, account_id: AccountId, price: Price) {
        let mut commitments = self.commitments.borrow_mut();
        let commitment = commitments
            .entry(order.client_order_id())
            .or_insert_with(|| {
                CashCommitment::new(account_id, order.instrument_id(), order.quantity())
            });
        commitment.quantity = order.quantity();
        commitment.limit_lock = Some(price);
    }

    /// Records a generated BUY fill that brings the order to `filled_qty` and debits `debit`.
    pub fn record_fill(
        &self,
        order: &OrderAny,
        account_id: AccountId,
        filled_qty: Quantity,
        debit: Money,
    ) {
        let mut commitments = self.commitments.borrow_mut();
        let commitment = commitments
            .entry(order.client_order_id())
            .or_insert_with(|| {
                CashCommitment::new(account_id, order.instrument_id(), order.quantity())
            });
        commitment.fills.push((filled_qty, debit));
    }

    /// Returns the cash committed to `account_id`'s other orders that the Account has not applied.
    ///
    /// Per order, the commitment is its generated fill debits plus the lock on its ungenerated
    /// LIMIT quantity. The Account has applied the debits of fills the Cache order holds, plus the
    /// lock on its leaves quantity once the Cache order is open. An order closed in the Cache with
    /// every generated fill applied is settled and dropped, as is one the Cache no longer holds
    /// (purged), which nothing can apply any more.
    ///
    /// # Errors
    ///
    /// Returns an error if a commitment is not in `currency` or a lock cannot be calculated.
    pub fn pending(
        &self,
        cache: &Cache,
        account: &CashAccount,
        currency: Currency,
        exclude: ClientOrderId,
    ) -> anyhow::Result<Money> {
        let mut commitments = self.commitments.borrow_mut();
        commitments.retain(|client_order_id, commitment| {
            cache.order(client_order_id).is_some_and(|order| {
                !order.is_closed() || order.filled_qty() != commitment.generated_qty()
            })
        });

        let mut pending = Money::zero(currency);
        for (client_order_id, commitment) in commitments.iter() {
            if *client_order_id == exclude || commitment.account_id != account.id {
                continue;
            }
            let Some(order) = cache.order(client_order_id) else {
                continue;
            };
            let (applied_qty, open, closed) =
                (order.filled_qty(), order.is_open(), order.is_closed());
            let mut committed = Money::zero(currency);
            let mut applied = Money::zero(currency);
            for (filled_qty, debit) in &commitment.fills {
                anyhow::ensure!(
                    debit.currency == currency,
                    "Committed {} differs from Account currency {currency}",
                    debit.currency
                );
                committed = checked_sum(committed, *debit)?;
                if *filled_qty <= applied_qty {
                    applied = checked_sum(applied, *debit)?;
                }
            }
            if let Some(price) = commitment.limit_lock
                && !closed
            {
                let instrument = cache
                    .instrument(&commitment.instrument_id)
                    .ok_or_else(|| anyhow::anyhow!("Missing {}", commitment.instrument_id))?;
                let ungenerated = commitment
                    .quantity
                    .saturating_sub(commitment.generated_qty());
                let lock = cash_limit_lock(account, instrument, ungenerated, price)?;
                committed = checked_sum(committed, lock)?;
                if open {
                    let leaves = commitment.quantity.saturating_sub(applied_qty);
                    let lock = cash_limit_lock(account, instrument, leaves, price)?;
                    applied = checked_sum(applied, lock)?;
                }
            }
            if committed > applied {
                pending = checked_sum(pending, committed - applied)?;
            }
        }
        Ok(pending)
    }

    /// Clears all commitments when the venue discards its state.
    pub fn clear(&self) {
        self.commitments.borrow_mut().clear();
    }
}

/// Returns the cash a CASH LIMIT BUY locks: notional plus commission at the worse fee rate.
///
/// The Account calculates both parts, so admission and the Portfolio lock agree.
///
/// # Errors
///
/// Returns an error if the Account cannot calculate the lock or its commission, or their
/// currencies differ.
pub fn cash_limit_lock(
    account: &CashAccount,
    instrument: &InstrumentAny,
    quantity: Quantity,
    price: Price,
) -> anyhow::Result<Money> {
    let notional =
        account.calculate_balance_locked(instrument, OrderSide::Buy, quantity, price, None)?;
    let liquidity_side = if instrument.maker_fee() > instrument.taker_fee() {
        LiquiditySide::Maker
    } else {
        LiquiditySide::Taker
    };
    let commission =
        account.calculate_commission(instrument, quantity, price, liquidity_side, None)?;
    anyhow::ensure!(
        commission.currency == notional.currency,
        "Commission {} differs from lock currency {}",
        commission.currency,
        notional.currency
    );
    if commission.is_negative() {
        return Ok(notional);
    }
    checked_sum(notional, commission)
}

fn checked_sum(lhs: Money, rhs: Money) -> anyhow::Result<Money> {
    lhs.checked_add(rhs)
        .ok_or_else(|| anyhow::anyhow!("CASH commitment exceeds Money bounds"))
}
