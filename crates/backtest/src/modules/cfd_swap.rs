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

//! CFD overnight swap simulation module.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    str::FromStr,
    sync::LazyLock,
};

use ahash::AHashMap;
use indexmap::IndexMap;
use jiff::{
    civil::{Date, Time},
    tz::TimeZone,
};
use nautilus_core::{UnixNanos, datetime::get_timezone};
use nautilus_model::{data::Data, identifiers::InstrumentId, instruments::Instrument, types::Money};

use super::{
    AccountAdjustmentOutcome, ExchangeContext, SimulationModule, SimulationModuleResult,
    carry::{Snapshot, exposure_notional_abs, mark_price, snapshot_exposures, to_base},
};

static EASTERN_TIMEZONE: LazyLock<TimeZone> =
    LazyLock::new(|| get_timezone("America/New_York").expect("bundled America/New_York timezone"));

fn eastern_timezone() -> &'static TimeZone {
    &EASTERN_TIMEZONE
}

/// ISO weekday of the default triple-swap night.
pub const FRIDAY: u8 = 5;
const SATURDAY: u8 = 6;
const SUNDAY: u8 = 7;

/// How a symbol's overnight swap is quoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapMode {
    /// No swap is charged.
    Disabled,
    /// A fixed quote-currency amount per lot per night (index CFDs).
    CcyMargin,
    /// An annual percentage of the position notional, accrued on a 360-day
    /// basis (US-stock CFDs).
    Interest,
}

impl SwapMode {
    /// Returns the wire string for this mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::CcyMargin => "ccy_margin",
            Self::Interest => "interest",
        }
    }
}

impl FromStr for SwapMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "disabled" => Ok(Self::Disabled),
            "ccy_margin" => Ok(Self::CcyMargin),
            "interest" => Ok(Self::Interest),
            other => anyhow::bail!(
                "Invalid swap mode '{other}', expected 'disabled', 'ccy_margin' or 'interest'"
            ),
        }
    }
}

/// Returns the signed cash flow for one rollover night in the quote currency.
///
/// `iso_weekday` is the ISO weekday of the rollover instant; weekend nights do
/// not roll and `swap_3day_dow` is charged triple. The swap points carry their
/// own sign, and an unrecognised mode charges nothing.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn swap_cash_flow(
    swap_mode: &str,
    swap_long: f64,
    swap_short: f64,
    is_long: bool,
    iso_weekday: u8,
    notional_abs: f64,
    lots: f64,
    swap_3day_dow: u8,
) -> f64 {
    if swap_mode == "disabled" {
        return 0.0;
    }
    if iso_weekday == SATURDAY || iso_weekday == SUNDAY {
        return 0.0;
    }
    let mult = if iso_weekday == swap_3day_dow {
        3.0
    } else {
        1.0
    };
    let rate = if is_long { swap_long } else { swap_short };
    let per_night = if swap_mode == "ccy_margin" {
        lots * rate
    } else if swap_mode == "interest" {
        (rate / 100.0) * notional_abs / 360.0
    } else {
        return 0.0;
    };
    per_night * mult
}

/// Returns the 17:00 Eastern rollover instant of a date, in UNIX nanoseconds.
///
/// Returns `None` for a pre-epoch instant, which no UNIX-nanosecond window can contain.
fn roll_time_ns(date: Date) -> Option<u64> {
    let rollover_eastern = date.to_datetime(Time::constant(17, 0, 0, 0));
    let timestamp = eastern_timezone()
        .to_ambiguous_timestamp(rollover_eastern)
        .unambiguous()
        .expect("unambiguous rollover time")
        .as_nanosecond();
    u64::try_from(timestamp).ok()
}

/// Returns the Eastern calendar date of an instant.
fn eastern_date(ts: UnixNanos) -> Date {
    ts.to_datetime_utc()
        .to_zoned(eastern_timezone().clone())
        .date()
}

fn weekday_of(date: Date) -> u8 {
    u8::try_from(date.weekday().to_monday_one_offset()).expect("ISO weekday in 1..=7")
}

/// Returns every chargeable rollover instant in `[start_ns, end_ns)`.
///
/// Weekend instants are omitted, since the module never charges them. A window opening before
/// the first representable rollover simply starts at that rollover.
#[must_use]
pub fn cfd_roll_instants_ns(start_ns: u64, end_ns: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut date = eastern_date(UnixNanos::from(start_ns));

    loop {
        match roll_time_ns(date) {
            Some(roll) if roll >= end_ns => break,
            Some(roll) if roll >= start_ns => {
                let iso_weekday = weekday_of(date);
                if iso_weekday != SATURDAY && iso_weekday != SUNDAY {
                    out.push(roll);
                }
            }
            _ => {}
        }

        let Ok(next) = date.tomorrow() else {
            break;
        };
        date = next;
    }

    out
}

/// One instrument's swap terms.
#[derive(Debug, Clone)]
struct SwapSpec {
    instrument_id: InstrumentId,
    mode: SwapMode,
    swap_long: f64,
    swap_short: f64,
    contract_size: f64,
    swap_3day_dow: u8,
}

/// One computed swap charge awaiting acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SwapAdjustment {
    instrument_id: InstrumentId,
    amount: Money,
}

#[derive(Debug)]
struct SwapState {
    next_roll_date: RefCell<Option<Date>>,
    next_roll_ns: Cell<u64>,
    initialized: Cell<bool>,
    prev_snapshot: RefCell<Option<Snapshot>>,
    pending_adjustments: RefCell<Option<Vec<SwapAdjustment>>>,
    instrument_costs: RefCell<IndexMap<InstrumentId, f64>>,
    total_cost: Cell<f64>,
    unapplied_cost: Cell<f64>,
}

/// Applies CFD overnight swap at the broker rollover, 17:00 `America/New_York`.
///
/// Every rollover at or before the current timestamp that has not yet been
/// applied is charged, so a rollover landing in a data gap is not dropped. A run
/// that starts after the day's rollover is not retro-charged. A rollover that
/// fell inside a gap is charged against the exposure and mark captured before
/// the gap, since no fill can occur while the market is gapped.
#[derive(Debug, Clone)]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(module = "nautilus_trader.backtest", unsendable, skip_from_py_object)
)]
#[cfg_attr(
    feature = "python",
    pyo3_stub_gen::derive::gen_stub_pyclass(module = "nautilus_trader.backtest")
)]
pub struct CfdSwapModule {
    specs: Rc<[SwapSpec]>,
    priced: Rc<AHashMap<InstrumentId, bool>>,
    state: Rc<SwapState>,
}

impl CfdSwapModule {
    /// Creates a new swap module from per-instrument swap terms.
    ///
    /// `specs` maps an instrument ID string to a
    /// `(swap_mode, swap_long, swap_short, contract_size, swap_3day_dow)` tuple,
    /// where `swap_3day_dow` is the ISO weekday of the triple-charge night.
    ///
    /// # Errors
    ///
    /// Returns an error if an instrument ID or swap mode cannot be parsed, a
    /// swap point is not finite, the contract size is negative, or the 3-day
    /// weekday is outside 1..=7.
    pub fn new(specs: IndexMap<String, (String, f64, f64, f64, u8)>) -> anyhow::Result<Self> {
        let mut parsed = Vec::with_capacity(specs.len());
        let mut priced = AHashMap::with_capacity(specs.len());

        for (key, (mode, swap_long, swap_short, contract_size, swap_3day_dow)) in specs {
            let instrument_id = InstrumentId::from_str(&key)?;
            let mode = SwapMode::from_str(&mode)?;
            anyhow::ensure!(
                swap_long.is_finite() && swap_short.is_finite(),
                "Swap points for '{key}' must be finite, were {swap_long} / {swap_short}"
            );
            anyhow::ensure!(
                contract_size.is_finite() && contract_size >= 0.0,
                "Contract size for '{key}' must be finite and non-negative, was {contract_size}"
            );
            anyhow::ensure!(
                (1..=7).contains(&swap_3day_dow),
                "3-day rollover weekday for '{key}' must be an ISO weekday, was {swap_3day_dow}"
            );
            if priced
                .insert(instrument_id, mode == SwapMode::Interest)
                .is_some()
            {
                anyhow::bail!("Duplicate swap spec for '{key}'");
            }
            parsed.push(SwapSpec {
                instrument_id,
                mode,
                swap_long,
                swap_short,
                contract_size,
                swap_3day_dow,
            });
        }

        Ok(Self {
            specs: parsed.into(),
            priced: Rc::new(priced),
            state: Rc::new(SwapState {
                next_roll_date: RefCell::new(None),
                next_roll_ns: Cell::new(0),
                initialized: Cell::new(false),
                prev_snapshot: RefCell::new(None),
                pending_adjustments: RefCell::new(None),
                instrument_costs: RefCell::new(IndexMap::new()),
                total_cost: Cell::new(0.0),
                unapplied_cost: Cell::new(0.0),
            }),
        })
    }

    /// Returns the applied swap cost across all instruments, positive when paid.
    #[must_use]
    pub fn total_cost(&self) -> f64 {
        self.state.total_cost.get()
    }

    /// Returns the swap cost the exchange could not apply.
    #[must_use]
    pub fn unapplied_cost(&self) -> f64 {
        self.state.unapplied_cost.get()
    }

    /// Returns the applied swap cost per instrument.
    #[must_use]
    pub fn instrument_costs(&self) -> IndexMap<InstrumentId, f64> {
        self.state.instrument_costs.borrow().clone()
    }

    fn set_next_roll(&self, date: Date) {
        let mut date = date;
        loop {
            if let Some(roll) = roll_time_ns(date) {
                self.state.next_roll_ns.set(roll);
                self.state.next_roll_date.replace(Some(date));
                return;
            }
            date = date.tomorrow().expect("next rollover date in range");
        }
    }

    /// Snapshots every open position, and its mark, on a configured instrument.
    ///
    /// Only `interest` instruments read a mark.
    fn exposure_snapshot(&self, ctx: &ExchangeContext) -> Snapshot {
        let exposures =
            snapshot_exposures(ctx, |instrument_id| self.priced.contains_key(instrument_id));
        if exposures.is_empty() {
            return Snapshot::default();
        }

        let mut marks = AHashMap::new();
        for instrument_id in exposures.keys() {
            if !self.priced[instrument_id] {
                continue;
            }
            let Some(instrument) = ctx.instruments.get(instrument_id) else {
                continue;
            };
            if let Some(mark) = mark_price(ctx, instrument_id, instrument) {
                marks.insert(*instrument_id, mark.as_f64());
            }
        }

        Snapshot { exposures, marks }
    }

    fn apply(
        &self,
        iso_weekday: u8,
        snapshot: &Snapshot,
        use_snapshot_mark: bool,
        ctx: &ExchangeContext,
        batch: &mut Vec<SwapAdjustment>,
    ) {
        if iso_weekday == SATURDAY || iso_weekday == SUNDAY {
            return;
        }

        for spec in self.specs.iter() {
            let Some(instrument) = ctx.instruments.get(&spec.instrument_id) else {
                continue;
            };
            let Some(exposures) = snapshot.exposures.get(&spec.instrument_id) else {
                continue;
            };

            for exposure in exposures {
                let lots = if spec.contract_size == 0.0 {
                    0.0
                } else {
                    exposure.signed_qty.abs() / spec.contract_size
                };

                let mut notional_abs = 0.0;
                if spec.mode == SwapMode::Interest {
                    let snapshot_mark = if use_snapshot_mark {
                        snapshot.marks.get(&spec.instrument_id).copied()
                    } else {
                        None
                    };
                    let mark_px = if let Some(mark_px) = snapshot_mark {
                        mark_px
                    } else if let Some(mark) = mark_price(ctx, &spec.instrument_id, instrument) {
                        mark.as_f64()
                    } else {
                        log::warn!(
                            "Swap: no mark price for {}, rollover skipped",
                            spec.instrument_id
                        );
                        continue;
                    };
                    let Some(resolved) =
                        exposure_notional_abs(exposure, mark_px, instrument.quote_currency())
                    else {
                        log::warn!(
                            "Swap: invalid notional for {}, rollover skipped",
                            spec.instrument_id
                        );
                        continue;
                    };
                    notional_abs = resolved;
                }

                let cash_flow = swap_cash_flow(
                    spec.mode.as_str(),
                    spec.swap_long,
                    spec.swap_short,
                    exposure.is_long,
                    iso_weekday,
                    notional_abs,
                    lots,
                    spec.swap_3day_dow,
                );
                if cash_flow == 0.0 {
                    continue;
                }

                let (cash_flow, currency) = to_base(
                    ctx,
                    &spec.instrument_id,
                    cash_flow,
                    instrument.quote_currency(),
                );
                let money = match Money::new_checked(cash_flow, currency) {
                    Ok(money) => money,
                    Err(e) => {
                        log::warn!("Swap: invalid adjustment for {}: {e}", spec.instrument_id);
                        continue;
                    }
                };
                if money.raw == 0 {
                    continue;
                }

                batch.push(SwapAdjustment {
                    instrument_id: spec.instrument_id,
                    amount: money,
                });
            }
        }
    }
}

impl SimulationModule for CfdSwapModule {
    fn pre_process(&self, _data: &Data) {}

    fn process(&self, ts_now: UnixNanos, ctx: &ExchangeContext) -> SimulationModuleResult {
        if self.specs.is_empty() {
            self.state.pending_adjustments.replace(Some(Vec::new()));
            return SimulationModuleResult::Completed(Vec::new());
        }

        if !self.state.initialized.get() {
            self.state.initialized.set(true);
            let date = eastern_date(ts_now);
            if roll_time_ns(date).is_some_and(|roll| ts_now.as_u64() < roll) {
                self.set_next_roll(date);
            } else {
                self.set_next_roll(date.tomorrow().expect("next rollover date in range"));
            }
        }

        let current = self.exposure_snapshot(ctx);
        let mut batch = Vec::new();
        let ts_now_u64 = ts_now.as_u64();

        {
            let prev_snapshot = self.state.prev_snapshot.borrow();

            loop {
                let next_roll_date = *self.state.next_roll_date.borrow();
                let Some(date) = next_roll_date else {
                    break;
                };
                let roll_ns = self.state.next_roll_ns.get();
                if ts_now_u64 < roll_ns {
                    break;
                }
                let (snapshot, use_snapshot_mark) = match prev_snapshot.as_ref() {
                    Some(prev) if roll_ns < ts_now_u64 => (prev, true),
                    _ => (&current, false),
                };
                self.apply(
                    weekday_of(date),
                    snapshot,
                    use_snapshot_mark,
                    ctx,
                    &mut batch,
                );
                self.set_next_roll(date.tomorrow().expect("next rollover date in range"));
            }
        }

        self.state.prev_snapshot.replace(Some(current));

        let adjustments = batch.iter().map(|a| a.amount).collect();
        self.state.pending_adjustments.replace(Some(batch));
        SimulationModuleResult::Completed(adjustments)
    }

    fn acknowledge(&self, outcomes: &[AccountAdjustmentOutcome]) {
        let adjustments = self
            .state
            .pending_adjustments
            .take()
            .expect("no completed swap batch to acknowledge");
        assert_eq!(
            outcomes.len(),
            adjustments.len(),
            "swap acknowledgement count must match adjustment count"
        );

        let mut instrument_costs = self.state.instrument_costs.borrow_mut();

        for (adjustment, outcome) in adjustments.into_iter().zip(outcomes) {
            let cost = -adjustment.amount.as_f64();
            match outcome {
                AccountAdjustmentOutcome::Applied => {
                    *instrument_costs
                        .entry(adjustment.instrument_id)
                        .or_insert(0.0) += cost;
                    self.state.total_cost.set(self.state.total_cost.get() + cost);
                }
                AccountAdjustmentOutcome::Failed(error) => {
                    log::warn!(
                        "Cannot apply swap adjustment {} for {}: {error}",
                        adjustment.amount,
                        adjustment.instrument_id
                    );
                    self.state
                        .unapplied_cost
                        .set(self.state.unapplied_cost.get() + cost);
                }
            }
        }
    }

    fn log_diagnostics(&self) {
        log::info!("Swap cost (total): {}", self.state.total_cost.get());
        let unapplied = self.state.unapplied_cost.get();
        if unapplied != 0.0 {
            log::warn!("Swap cost (unapplied): {unapplied}");
        }
    }

    fn reset(&self) {
        self.state.next_roll_date.replace(None);
        self.state.next_roll_ns.set(0);
        self.state.initialized.set(false);
        self.state.prev_snapshot.replace(None);
        self.state.pending_adjustments.replace(None);
        self.state.instrument_costs.borrow_mut().clear();
        self.state.total_cost.set(0.0);
        self.state.unapplied_cost.set(0.0);
    }
}

#[cfg(test)]
#[expect(clippy::float_cmp, reason = "exact-equality carry arithmetic")]
mod tests {
    use rstest::rstest;

    use super::*;

    const SWAP_LONG_INDEX: f64 = -4.48;
    const SWAP_SHORT_INDEX: f64 = 0.51;
    const SWAP_LONG_STOCK: f64 = -8.73;
    const SWAP_SHORT_STOCK: f64 = -1.27;

    #[rstest]
    #[case(1, 1.0)]
    #[case(2, 1.0)]
    #[case(3, 1.0)]
    #[case(4, 1.0)]
    #[case(5, 3.0)]
    #[case(6, 0.0)]
    #[case(7, 0.0)]
    fn test_swap_index_ccy_margin_weekday_multiplier(#[case] iso_wd: u8, #[case] mult: f64) {
        let cf = swap_cash_flow(
            "ccy_margin",
            SWAP_LONG_INDEX,
            SWAP_SHORT_INDEX,
            true,
            iso_wd,
            0.0,
            1.0,
            FRIDAY,
        );
        assert_eq!(cf, SWAP_LONG_INDEX * mult);
    }

    #[rstest]
    fn test_swap_index_short_uses_swap_short() {
        let cf = swap_cash_flow(
            "ccy_margin",
            SWAP_LONG_INDEX,
            SWAP_SHORT_INDEX,
            false,
            2,
            0.0,
            2.0,
            FRIDAY,
        );
        assert_eq!(cf, SWAP_SHORT_INDEX * 2.0);
    }

    #[rstest]
    fn test_swap_stock_interest_notional_over_360() {
        let cf = swap_cash_flow(
            "interest",
            SWAP_LONG_STOCK,
            SWAP_SHORT_STOCK,
            true,
            4,
            1000.0,
            1.0,
            FRIDAY,
        );
        assert_eq!(cf, (SWAP_LONG_STOCK / 100.0) * 1000.0 / 360.0);
    }

    #[rstest]
    fn test_swap_stock_interest_friday_triple() {
        let cf = swap_cash_flow(
            "interest",
            SWAP_LONG_STOCK,
            SWAP_SHORT_STOCK,
            true,
            5,
            1000.0,
            1.0,
            FRIDAY,
        );
        assert_eq!(cf, (SWAP_LONG_STOCK / 100.0) * 1000.0 / 360.0 * 3.0);
    }

    #[rstest]
    #[case(1, 1.0)]
    #[case(2, 1.0)]
    #[case(3, 3.0)]
    #[case(4, 1.0)]
    #[case(5, 1.0)]
    #[case(6, 0.0)]
    #[case(7, 0.0)]
    fn test_swap_3day_dow_moves_the_triple_night(#[case] iso_wd: u8, #[case] mult: f64) {
        let cf = swap_cash_flow(
            "ccy_margin",
            SWAP_LONG_INDEX,
            SWAP_SHORT_INDEX,
            true,
            iso_wd,
            0.0,
            1.0,
            3,
        );
        assert_eq!(cf, SWAP_LONG_INDEX * mult);
    }

    #[rstest]
    fn test_swap_disabled_is_zero() {
        let cf = swap_cash_flow(
            "disabled",
            SWAP_LONG_STOCK,
            SWAP_SHORT_STOCK,
            true,
            4,
            1000.0,
            1.0,
            FRIDAY,
        );
        assert_eq!(cf, 0.0);
    }

    #[rstest]
    fn test_roll_instants_skip_the_weekend() {
        // 2024-03-04 (Mon) 00:00 UTC through 2024-03-12 (Tue) 00:00 UTC.
        let start = 1_709_510_400_000_000_000_u64;
        let end = 1_710_201_600_000_000_000_u64;
        let instants = cfd_roll_instants_ns(start, end);
        assert_eq!(instants.len(), 6);
        assert!(instants.windows(2).all(|w| w[0] < w[1]));
    }

    #[rstest]
    fn test_roll_time_tracks_us_dst() {
        // 2024-03-09 is EST (UTC-5) -> 22:00 UTC; 2024-03-11 is EDT (UTC-4) -> 21:00 UTC.
        let est = roll_time_ns(Date::new(2024, 3, 9).unwrap()).unwrap();
        let edt = roll_time_ns(Date::new(2024, 3, 11).unwrap()).unwrap();
        assert_eq!(est % 86_400_000_000_000, 22 * 3_600_000_000_000);
        assert_eq!(edt % 86_400_000_000_000, 21 * 3_600_000_000_000);
    }

    #[rstest]
    #[case(0, 0)]
    #[case(1, 1)]
    #[case(0, 17_999_999_999_999)]
    #[case(17_999_999_999_999, 17_999_999_999_999)]
    fn test_roll_instants_before_first_rollover_are_empty(
        #[case] start_ns: u64,
        #[case] end_ns: u64,
    ) {
        assert!(cfd_roll_instants_ns(start_ns, end_ns).is_empty());
    }

    #[rstest]
    fn test_roll_instants_from_epoch_start_at_first_representable_rollover() {
        // 1969-12-31 17:00 ET predates the epoch, so the first instant is 1970-01-01 17:00 ET.
        let instants = cfd_roll_instants_ns(0, 200_000_000_000_000);
        assert_eq!(instants, vec![79_200_000_000_000, 165_600_000_000_000]);
    }

    #[rstest]
    fn test_roll_time_ns_is_none_before_the_epoch() {
        assert!(roll_time_ns(Date::new(1969, 12, 31).unwrap()).is_none());
        assert_eq!(
            roll_time_ns(Date::new(1970, 1, 1).unwrap()),
            Some(79_200_000_000_000)
        );
    }

    #[rstest]
    fn test_invalid_swap_mode_rejected() {
        let mut specs = IndexMap::new();
        specs.insert(
            "US500.DARWINEX".to_string(),
            ("nonsense".to_string(), -4.48, 0.51, 1.0, 5),
        );
        assert!(CfdSwapModule::new(specs).is_err());
    }

    #[rstest]
    fn test_clone_shares_state() {
        let mut specs = IndexMap::new();
        specs.insert(
            "US500.DARWINEX".to_string(),
            ("ccy_margin".to_string(), -4.48, 0.51, 1.0, 5),
        );
        let module = CfdSwapModule::new(specs).unwrap();
        let clone = module.clone();
        clone.state.total_cost.set(7.25);
        assert_eq!(module.total_cost(), 7.25);
    }
}
