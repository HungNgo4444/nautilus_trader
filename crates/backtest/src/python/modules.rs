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

//! Python bindings for simulation module types.

use indexmap::IndexMap;
use nautilus_core::python::to_pyvalue_err;
use pyo3::prelude::*;

use crate::modules::{
    cfd_swap::{CfdSwapModule, cfd_roll_instants_ns, swap_cash_flow},
    funding_rate::{FundingRateModule, funding_cash_flow},
    fx_rollover::{FXRolloverInterestModule, InterestRateRecord},
};

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl InterestRateRecord {
    /// A single interest rate data entry.
    #[new]
    fn py_new(location: String, time: String, value: f64) -> PyResult<Self> {
        let record = Self {
            location,
            time,
            value,
        };
        record.validate().map_err(to_pyvalue_err)?;
        Ok(record)
    }

    fn __repr__(&self) -> String {
        format!("{self:?}")
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl FXRolloverInterestModule {
    /// Simulates FX rollover (swap) interest applied at 5 PM US/Eastern daily.
    ///
    /// When holding FX positions overnight, the interest rate differential
    /// between the two currencies is credited or debited. Wednesday and Friday
    /// rollovers are tripled (Wednesday for T+2 settlement, Friday for the weekend).
    #[new]
    fn py_new(records: Vec<InterestRateRecord>) -> PyResult<Self> {
        Self::new(records).map_err(to_pyvalue_err)
    }

    fn __repr__(&self) -> String {
        format!("{self:?}")
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl FundingRateModule {
    /// Applies perpetual swap funding at each scheduled settlement.
    ///
    /// Every settlement at or before the current timestamp that has not yet been
    /// applied is charged against each open position in that instrument, valued at
    /// the mark. A settlement instant carrying no data item of its own is drained on
    /// the following call and charged against the exposure and mark captured before
    /// the gap, since no fill can occur while the market is gapped.
    #[new]
    fn py_new(funding_rates: IndexMap<String, (Vec<u64>, Vec<f64>)>) -> PyResult<Self> {
        Self::new(funding_rates).map_err(to_pyvalue_err)
    }

    fn __repr__(&self) -> String {
        format!("{self:?}")
    }

    /// Returns the applied funding cost across all instruments, positive when paid.
    #[getter]
    #[pyo3(name = "total_cost")]
    fn py_total_cost(&self) -> f64 {
        self.total_cost()
    }

    /// Returns the funding cost the exchange could not apply.
    #[getter]
    #[pyo3(name = "unapplied_cost")]
    fn py_unapplied_cost(&self) -> f64 {
        self.unapplied_cost()
    }

    /// Returns the applied funding cost per instrument.
    #[getter]
    #[pyo3(name = "instrument_costs")]
    fn py_instrument_costs(&self) -> IndexMap<String, f64> {
        self.instrument_costs()
            .into_iter()
            .map(|(instrument_id, cost)| (instrument_id.to_string(), cost))
            .collect()
    }

    /// Returns every applied settlement timestamp and cost, in charge order.
    #[getter]
    #[pyo3(name = "settlements")]
    fn py_settlements(&self) -> Vec<(u64, f64)> {
        self.settlements()
            .into_iter()
            .map(|(ts, cost)| (ts.as_u64(), cost))
            .collect()
    }
}

#[pyo3_stub_gen::derive::gen_stub_pymethods]
#[pymethods]
impl CfdSwapModule {
    /// Applies CFD overnight swap at the broker rollover, 17:00 `America/New_York`.
    ///
    /// Every rollover at or before the current timestamp that has not yet been
    /// applied is charged, so a rollover landing in a data gap is not dropped. A run
    /// that starts after the day's rollover is not retro-charged. A rollover that
    /// fell inside a gap is charged against the exposure and mark captured before
    /// the gap, since no fill can occur while the market is gapped.
    #[new]
    fn py_new(swap_specs: IndexMap<String, (String, f64, f64, f64, u8)>) -> PyResult<Self> {
        Self::new(swap_specs).map_err(to_pyvalue_err)
    }

    fn __repr__(&self) -> String {
        format!("{self:?}")
    }

    /// Returns the applied swap cost across all instruments, positive when paid.
    #[getter]
    #[pyo3(name = "total_cost")]
    fn py_total_cost(&self) -> f64 {
        self.total_cost()
    }

    /// Returns the swap cost the exchange could not apply.
    #[getter]
    #[pyo3(name = "unapplied_cost")]
    fn py_unapplied_cost(&self) -> f64 {
        self.unapplied_cost()
    }

    /// Returns the applied swap cost per instrument.
    #[getter]
    #[pyo3(name = "instrument_costs")]
    fn py_instrument_costs(&self) -> IndexMap<String, f64> {
        self.instrument_costs()
            .into_iter()
            .map(|(instrument_id, cost)| (instrument_id.to_string(), cost))
            .collect()
    }
}

/// Returns the signed cash flow for one funding settlement in the quote currency.
///
/// A long position pays a positive rate. `notional_abs` is the unsigned
/// position notional.
#[pyo3_stub_gen::derive::gen_stub_pyfunction(module = "nautilus_trader.backtest")]
#[pyfunction]
#[pyo3(name = "funding_cash_flow")]
#[must_use]
pub fn py_funding_cash_flow(rate: f64, notional_abs: f64, is_long: bool) -> f64 {
    funding_cash_flow(rate, notional_abs, is_long)
}

/// Returns the signed cash flow for one rollover night in the quote currency.
///
/// `iso_weekday` is the ISO weekday of the rollover instant; weekend nights do
/// not roll and `swap_3day_dow` is charged triple. The swap points carry their
/// own sign, and an unrecognised mode charges nothing.
#[pyo3_stub_gen::derive::gen_stub_pyfunction(module = "nautilus_trader.backtest")]
#[pyfunction]
#[pyo3(name = "swap_cash_flow", signature = (swap_mode, swap_long, swap_short, is_long, iso_weekday, notional_abs, lots, swap_3day_dow = 5))]
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn py_swap_cash_flow(
    swap_mode: &str,
    swap_long: f64,
    swap_short: f64,
    is_long: bool,
    iso_weekday: u8,
    notional_abs: f64,
    lots: f64,
    swap_3day_dow: u8,
) -> f64 {
    swap_cash_flow(
        swap_mode,
        swap_long,
        swap_short,
        is_long,
        iso_weekday,
        notional_abs,
        lots,
        swap_3day_dow,
    )
}

/// Returns every chargeable rollover instant in `[start_ns, end_ns)`.
///
/// Weekend instants are omitted, since the module never charges them. A window opening before
/// the first representable rollover simply starts at that rollover.
#[pyo3_stub_gen::derive::gen_stub_pyfunction(module = "nautilus_trader.backtest")]
#[pyfunction]
#[pyo3(name = "cfd_roll_instants_ns")]
#[must_use]
pub fn py_cfd_roll_instants_ns(start_ns: u64, end_ns: u64) -> Vec<u64> {
    cfd_roll_instants_ns(start_ns, end_ns)
}
