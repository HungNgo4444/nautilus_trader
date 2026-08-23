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

//! Perpetual swap funding simulation module.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    str::FromStr,
};

use ahash::AHashMap;
use indexmap::IndexMap;
use nautilus_core::UnixNanos;
use nautilus_model::{
    data::Data,
    identifiers::InstrumentId,
    instruments::{Instrument, InstrumentAny},
    types::Money,
};

use super::{
    AccountAdjustmentOutcome, ExchangeContext, SimulationModule, SimulationModuleResult,
    carry::{Snapshot, exposure_notional_abs, mark_price, snapshot_exposures, to_base},
};

/// Returns the signed cash flow for one funding settlement in the quote currency.
///
/// A long position pays a positive rate. `notional_abs` is the unsigned
/// position notional.
#[must_use]
pub fn funding_cash_flow(rate: f64, notional_abs: f64, is_long: bool) -> f64 {
    let signed_notional = if is_long { notional_abs } else { -notional_abs };
    -rate * signed_notional
}

/// One instrument's funding settlement schedule.
#[derive(Debug, Clone)]
struct FundingSeries {
    instrument_id: InstrumentId,
    ts_ns: Vec<u64>,
    rates: Vec<f64>,
}

/// One computed funding charge awaiting acknowledgement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FundingAdjustment {
    instrument_id: InstrumentId,
    ts_settle: UnixNanos,
    amount: Money,
}

#[derive(Debug)]
struct FundingState {
    next_idx: RefCell<Vec<usize>>,
    pending: Cell<usize>,
    prev_snapshot: RefCell<Option<Snapshot>>,
    pending_adjustments: RefCell<Option<Vec<FundingAdjustment>>>,
    instrument_costs: RefCell<IndexMap<InstrumentId, f64>>,
    total_cost: Cell<f64>,
    unapplied_cost: Cell<f64>,
    settlements: RefCell<Vec<(UnixNanos, f64)>>,
}

/// Applies perpetual swap funding at each scheduled settlement.
///
/// Every settlement at or before the current timestamp that has not yet been
/// applied is charged against each open position in that instrument, valued at
/// the mark. A settlement instant carrying no data item of its own is drained on
/// the following call and charged against the exposure and mark captured before
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
pub struct FundingRateModule {
    series: Rc<[FundingSeries]>,
    index_of: Rc<AHashMap<InstrumentId, usize>>,
    state: Rc<FundingState>,
}

impl FundingRateModule {
    /// Creates a new funding module from a per-instrument settlement schedule.
    ///
    /// `schedule` maps an instrument ID to parallel ascending lists of
    /// settlement timestamps and the rate applied at each.
    ///
    /// # Errors
    ///
    /// Returns an error if an instrument ID cannot be parsed, a schedule has
    /// mismatched lengths or non-ascending timestamps, or a rate is not finite.
    pub fn new(schedule: IndexMap<String, (Vec<u64>, Vec<f64>)>) -> anyhow::Result<Self> {
        let mut series = Vec::with_capacity(schedule.len());
        let mut index_of = AHashMap::with_capacity(schedule.len());

        for (key, (ts_ns, rates)) in schedule {
            let instrument_id = InstrumentId::from_str(&key)?;
            anyhow::ensure!(
                ts_ns.len() == rates.len(),
                "Funding schedule for '{key}' has {} timestamps and {} rates",
                ts_ns.len(),
                rates.len()
            );
            anyhow::ensure!(
                ts_ns.windows(2).all(|w| w[0] <= w[1]),
                "Funding schedule for '{key}' must have ascending timestamps"
            );
            for rate in &rates {
                anyhow::ensure!(
                    rate.is_finite(),
                    "Funding rate for '{key}' must be finite, was {rate}"
                );
            }
            if index_of.insert(instrument_id, series.len()).is_some() {
                anyhow::bail!("Duplicate funding schedule for '{key}'");
            }
            series.push(FundingSeries {
                instrument_id,
                ts_ns,
                rates,
            });
        }

        let pending = series.iter().map(|s| s.ts_ns.len()).sum();
        let next_idx = vec![0; series.len()];

        Ok(Self {
            series: series.into(),
            index_of: Rc::new(index_of),
            state: Rc::new(FundingState {
                next_idx: RefCell::new(next_idx),
                pending: Cell::new(pending),
                prev_snapshot: RefCell::new(None),
                pending_adjustments: RefCell::new(None),
                instrument_costs: RefCell::new(IndexMap::new()),
                total_cost: Cell::new(0.0),
                unapplied_cost: Cell::new(0.0),
                settlements: RefCell::new(Vec::new()),
            }),
        })
    }

    /// Returns the applied funding cost across all instruments, positive when paid.
    #[must_use]
    pub fn total_cost(&self) -> f64 {
        self.state.total_cost.get()
    }

    /// Returns the funding cost the exchange could not apply.
    #[must_use]
    pub fn unapplied_cost(&self) -> f64 {
        self.state.unapplied_cost.get()
    }

    /// Returns the applied funding cost per instrument.
    #[must_use]
    pub fn instrument_costs(&self) -> IndexMap<InstrumentId, f64> {
        self.state.instrument_costs.borrow().clone()
    }

    /// Returns every applied settlement timestamp and cost, in charge order.
    #[must_use]
    pub fn settlements(&self) -> Vec<(UnixNanos, f64)> {
        self.state.settlements.borrow().clone()
    }

    /// Snapshots every open position, and its mark, on a funded instrument.
    ///
    /// Inverse instruments are left unpriced, so a settlement on one falls back
    /// to a live read rather than a notional this module does not model.
    fn exposure_snapshot(&self, ctx: &ExchangeContext) -> Snapshot {
        let exposures = snapshot_exposures(ctx, |instrument_id| {
            self.index_of.contains_key(instrument_id)
        });
        if exposures.is_empty() {
            return Snapshot::default();
        }

        let mut marks = AHashMap::new();
        for instrument_id in exposures.keys() {
            let Some(instrument) = ctx.instruments.get(instrument_id) else {
                continue;
            };
            if instrument.is_inverse() {
                continue;
            }
            if let Some(mark) = mark_price(ctx, instrument_id, instrument) {
                marks.insert(*instrument_id, mark.as_f64());
            }
        }

        Snapshot { exposures, marks }
    }

    /// Charges one settlement against the snapshot exposure for `instrument_id`.
    #[allow(clippy::too_many_arguments)]
    fn settle(
        instrument_id: InstrumentId,
        rate: f64,
        ts_settle: u64,
        snapshot: &Snapshot,
        use_snapshot_mark: bool,
        ctx: &ExchangeContext,
        batch: &mut Vec<FundingAdjustment>,
    ) {
        if rate == 0.0 {
            return;
        }
        let Some(instrument) = ctx.instruments.get(&instrument_id) else {
            return;
        };
        let Some(exposures) = snapshot.exposures.get(&instrument_id) else {
            return;
        };

        for exposure in exposures {
            let Some(mark_px) =
                resolve_mark(ctx, &instrument_id, instrument, snapshot, use_snapshot_mark)
            else {
                log::warn!("Funding: no mark price for {instrument_id}, settlement skipped");
                continue;
            };
            let Some(notional_abs) =
                exposure_notional_abs(exposure, mark_px, instrument.quote_currency())
            else {
                log::warn!("Funding: invalid notional for {instrument_id}, settlement skipped");
                continue;
            };
            let cash_flow = funding_cash_flow(rate, notional_abs, exposure.is_long);
            let (cash_flow, currency) =
                to_base(ctx, &instrument_id, cash_flow, instrument.quote_currency());
            let money = match Money::new_checked(cash_flow, currency) {
                Ok(money) => money,
                Err(e) => {
                    log::warn!("Funding: invalid adjustment for {instrument_id}: {e}");
                    continue;
                }
            };
            if money.raw == 0 {
                continue;
            }
            batch.push(FundingAdjustment {
                instrument_id,
                ts_settle: UnixNanos::from(ts_settle),
                amount: money,
            });
        }
    }
}

/// Resolves the mark for one charge, preferring the snapshot when it applies.
fn resolve_mark(
    ctx: &ExchangeContext,
    instrument_id: &InstrumentId,
    instrument: &InstrumentAny,
    snapshot: &Snapshot,
    use_snapshot_mark: bool,
) -> Option<f64> {
    if use_snapshot_mark
        && let Some(mark_px) = snapshot.marks.get(instrument_id)
    {
        return Some(*mark_px);
    }
    mark_price(ctx, instrument_id, instrument).map(|mark| mark.as_f64())
}

impl SimulationModule for FundingRateModule {
    fn pre_process(&self, _data: &Data) {}

    fn process(&self, ts_now: UnixNanos, ctx: &ExchangeContext) -> SimulationModuleResult {
        if self.state.pending.get() == 0 {
            self.state.prev_snapshot.replace(None);
            self.state.pending_adjustments.replace(Some(Vec::new()));
            return SimulationModuleResult::Completed(Vec::new());
        }

        let current = self.exposure_snapshot(ctx);
        let mut batch = Vec::new();
        let ts_now_u64 = ts_now.as_u64();

        {
            let prev_snapshot = self.state.prev_snapshot.borrow();
            let mut next_idx = self.state.next_idx.borrow_mut();

            for (series_index, series) in self.series.iter().enumerate() {
                let start = next_idx[series_index];
                let mut idx = start;
                while idx < series.ts_ns.len() && series.ts_ns[idx] <= ts_now_u64 {
                    let ts_settle = series.ts_ns[idx];
                    let (snapshot, use_snapshot_mark) = match prev_snapshot.as_ref() {
                        Some(prev) if ts_settle < ts_now_u64 => (prev, true),
                        _ => (&current, false),
                    };
                    Self::settle(
                        series.instrument_id,
                        series.rates[idx],
                        ts_settle,
                        snapshot,
                        use_snapshot_mark,
                        ctx,
                        &mut batch,
                    );
                    idx += 1;
                }
                self.state
                    .pending
                    .set(self.state.pending.get() - (idx - start));
                next_idx[series_index] = idx;
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
            .expect("no completed funding batch to acknowledge");
        assert_eq!(
            outcomes.len(),
            adjustments.len(),
            "funding acknowledgement count must match adjustment count"
        );

        let mut instrument_costs = self.state.instrument_costs.borrow_mut();
        let mut settlements = self.state.settlements.borrow_mut();

        for (adjustment, outcome) in adjustments.into_iter().zip(outcomes) {
            let cost = -adjustment.amount.as_f64();
            match outcome {
                AccountAdjustmentOutcome::Applied => {
                    *instrument_costs
                        .entry(adjustment.instrument_id)
                        .or_insert(0.0) += cost;
                    self.state.total_cost.set(self.state.total_cost.get() + cost);
                    settlements.push((adjustment.ts_settle, cost));
                }
                AccountAdjustmentOutcome::Failed(error) => {
                    log::warn!(
                        "Cannot apply funding adjustment {} for {}: {error}",
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
        log::info!("Funding cost (total): {}", self.state.total_cost.get());
        let unapplied = self.state.unapplied_cost.get();
        if unapplied != 0.0 {
            log::warn!("Funding cost (unapplied): {unapplied}");
        }
    }

    fn reset(&self) {
        self.state.next_idx.borrow_mut().fill(0);
        self.state
            .pending
            .set(self.series.iter().map(|s| s.ts_ns.len()).sum());
        self.state.prev_snapshot.replace(None);
        self.state.pending_adjustments.replace(None);
        self.state.instrument_costs.borrow_mut().clear();
        self.state.total_cost.set(0.0);
        self.state.unapplied_cost.set(0.0);
        self.state.settlements.borrow_mut().clear();
    }
}

#[cfg(test)]
#[expect(clippy::float_cmp, reason = "exact-equality carry arithmetic")]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    fn test_funding_long_pays_positive_rate() {
        assert_eq!(funding_cash_flow(0.01, 200.0, true), -2.0);
    }

    #[rstest]
    fn test_funding_short_receives_positive_rate() {
        assert_eq!(funding_cash_flow(0.01, 200.0, false), 2.0);
    }

    #[rstest]
    fn test_funding_negative_rate_flips() {
        assert_eq!(funding_cash_flow(-0.005, 1000.0, true), 5.0);
        assert_eq!(funding_cash_flow(-0.005, 1000.0, false), -5.0);
    }

    #[rstest]
    fn test_funding_zero_rate_zero_flow() {
        assert_eq!(funding_cash_flow(0.0, 1000.0, true), 0.0);
    }

    #[rstest]
    fn test_schedule_length_mismatch_rejected() {
        let mut schedule = IndexMap::new();
        schedule.insert(
            "BTCUSDT-PERP.BINANCE".to_string(),
            (vec![1_u64, 2], vec![0.01]),
        );
        assert!(FundingRateModule::new(schedule).is_err());
    }

    #[rstest]
    fn test_clone_shares_state() {
        let mut schedule = IndexMap::new();
        schedule.insert(
            "BTCUSDT-PERP.BINANCE".to_string(),
            (vec![1_u64, 2], vec![0.01, 0.02]),
        );
        let module = FundingRateModule::new(schedule).unwrap();
        let clone = module.clone();
        clone.state.total_cost.set(12.5);
        assert_eq!(module.total_cost(), 12.5);
    }

    #[rstest]
    fn test_reset_restores_pending_count() {
        let mut schedule = IndexMap::new();
        schedule.insert(
            "BTCUSDT-PERP.BINANCE".to_string(),
            (vec![1_u64, 2], vec![0.01, 0.02]),
        );
        let module = FundingRateModule::new(schedule).unwrap();
        assert_eq!(module.state.pending.get(), 2);
        module.state.pending.set(0);
        module.reset();
        assert_eq!(module.state.pending.get(), 2);
    }
}
