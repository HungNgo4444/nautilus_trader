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

//! Shared exposure, mark and currency helpers for the carry cost modules.

use ahash::AHashMap;
use nautilus_model::{
    enums::PriceType,
    identifiers::InstrumentId,
    instruments::{Instrument, InstrumentAny},
    types::{Currency, Money, Price},
};
use rust_decimal::prelude::ToPrimitive;

use super::ExchangeContext;

/// Value copy of one open position's carry inputs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Exposure {
    /// Whether the position was long.
    pub is_long: bool,
    /// The signed position quantity.
    pub signed_qty: f64,
    /// The absolute position quantity, rounded to the size precision.
    pub quantity: f64,
    /// The instrument multiplier.
    pub multiplier: f64,
}

/// One process call's carry inputs, captured by value.
///
/// A missing mark means the snapshot has no mark for that instrument, on which
/// consumers fall back to a live read. Which instruments are priced is the
/// owning module's policy.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Open positions grouped by instrument, in cache order.
    pub exposures: AHashMap<InstrumentId, Vec<Exposure>>,
    /// The mark resolved for an instrument at the same instant as the exposure.
    pub marks: AHashMap<InstrumentId, f64>,
}

/// Groups every open venue position matching `keep` into value copies.
pub fn snapshot_exposures(
    ctx: &ExchangeContext,
    keep: impl Fn(&InstrumentId) -> bool,
) -> AHashMap<InstrumentId, Vec<Exposure>> {
    let mut grouped: AHashMap<InstrumentId, Vec<Exposure>> = AHashMap::new();

    for position in ctx
        .cache
        .positions_open(Some(&ctx.venue), None, None, None, None)
    {
        if !keep(&position.instrument_id) {
            continue;
        }
        grouped
            .entry(position.instrument_id)
            .or_default()
            .push(Exposure {
                is_long: position.is_long(),
                signed_qty: position.signed_qty,
                quantity: position.quantity.as_f64(),
                multiplier: position.multiplier.as_f64(),
            });
    }

    grouped
}

/// Returns the best available mark for `instrument_id`.
///
/// Book midpoint, then best bid, then best ask, then the cached last price.
#[must_use]
pub fn mark_price(
    ctx: &ExchangeContext,
    instrument_id: &InstrumentId,
    instrument: &InstrumentAny,
) -> Option<Price> {
    if let Some(matching_engine) = ctx.matching_engines.get(instrument_id) {
        let book = matching_engine.get_book();
        let mid = book
            .midpoint()
            .or_else(|| book.best_bid_price().map(|price| price.as_f64()))
            .or_else(|| book.best_ask_price().map(|price| price.as_f64()));

        if let Some(mid) = mid {
            return match Price::new_checked(mid, instrument.price_precision()) {
                Ok(price) => Some(price),
                Err(e) => {
                    log::warn!("Cannot resolve mark for {instrument_id}: {e}");
                    None
                }
            };
        }
    }

    ctx.cache.price(instrument_id, PriceType::Last)
}

/// Converts `amount` in `quote_currency` to the account base currency.
///
/// Passes the amount through for a multi-currency account or when the quote
/// currency is the base, and contributes nothing when no exchange rate is
/// available.
#[must_use]
pub fn to_base(
    ctx: &ExchangeContext,
    instrument_id: &InstrumentId,
    amount: f64,
    quote_currency: Currency,
) -> (f64, Currency) {
    let Some(base) = ctx.base_currency else {
        return (amount, quote_currency);
    };
    if quote_currency == base {
        return (amount, quote_currency);
    }

    let xrate = match ctx
        .cache
        .try_get_xrate(instrument_id.venue, quote_currency, base, PriceType::Mid)
    {
        Ok(Some(xrate)) => xrate.to_f64().unwrap_or(0.0),
        Ok(None) => 0.0,
        Err(e) => {
            log::warn!("Cannot convert carry charge for {instrument_id} to {base}: {e}");
            0.0
        }
    };

    if xrate == 0.0 {
        log::warn!("No exchange rate from {quote_currency} to {base} for {instrument_id}");
        return (0.0, base);
    }

    (amount * xrate, base)
}

/// Returns the position notional in the quote currency, from values only.
#[must_use]
pub fn exposure_notional_abs(
    exposure: &Exposure,
    mark_px: f64,
    quote_currency: Currency,
) -> Option<f64> {
    Money::new_checked(
        exposure.quantity * exposure.multiplier * mark_px,
        quote_currency,
    )
    .ok()
    .map(|money| money.as_f64())
}
