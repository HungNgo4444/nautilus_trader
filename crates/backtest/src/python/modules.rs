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
    /// Simulates perpetual-swap funding settled on a per-instrument schedule.
    ///
    /// `funding_rates` maps an instrument ID string to a `(ts_ns, rates)` pair of
    /// parallel ascending lists: `ts_ns` are the settlement timestamps (UNIX
    /// nanoseconds, UTC) and `rates` the funding rate applied at each. A long
    /// position pays a positive rate.
    #[new]
    fn py_new(funding_rates: IndexMap<String, (Vec<u64>, Vec<f64>)>) -> PyResult<Self> {
        Self::new(funding_rates).map_err(to_pyvalue_err)
    }

    fn __repr__(&self) -> String {
        format!("{self:?}")
    }

    #[getter]
    #[pyo3(name = "total_cost")]
    fn py_total_cost(&self) -> f64 {
        self.total_cost()
    }

    #[getter]
    #[pyo3(name = "unapplied_cost")]
    fn py_unapplied_cost(&self) -> f64 {
        self.unapplied_cost()
    }

    #[getter]
    #[pyo3(name = "instrument_costs")]
    fn py_instrument_costs(&self) -> IndexMap<String, f64> {
        self.instrument_costs()
            .into_iter()
            .map(|(instrument_id, cost)| (instrument_id.to_string(), cost))
            .collect()
    }

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
    /// Simulates CFD overnight swap charged at 17:00 `America/New_York`.
    ///
    /// `swap_specs` maps an instrument ID string to a
    /// `(swap_mode, swap_long, swap_short, contract_size, swap_3day_dow)` tuple.
    /// `swap_mode` is one of `disabled`, `ccy_margin` or `interest`, the swap
    /// points already carry their sign (negative = the account pays), and
    /// `swap_3day_dow` is the ISO weekday charged triple.
    #[new]
    fn py_new(swap_specs: IndexMap<String, (String, f64, f64, f64, u8)>) -> PyResult<Self> {
        Self::new(swap_specs).map_err(to_pyvalue_err)
    }

    fn __repr__(&self) -> String {
        format!("{self:?}")
    }

    #[getter]
    #[pyo3(name = "total_cost")]
    fn py_total_cost(&self) -> f64 {
        self.total_cost()
    }

    #[getter]
    #[pyo3(name = "unapplied_cost")]
    fn py_unapplied_cost(&self) -> f64 {
        self.unapplied_cost()
    }

    #[getter]
    #[pyo3(name = "instrument_costs")]
    fn py_instrument_costs(&self) -> IndexMap<String, f64> {
        self.instrument_costs()
            .into_iter()
            .map(|(instrument_id, cost)| (instrument_id.to_string(), cost))
            .collect()
    }
}

/// Returns the signed cash flow (quote currency) for one funding settlement.
#[pyo3_stub_gen::derive::gen_stub_pyfunction(module = "nautilus_trader.backtest")]
#[pyfunction]
#[pyo3(name = "funding_cash_flow")]
#[must_use]
pub fn py_funding_cash_flow(rate: f64, notional_abs: f64, is_long: bool) -> f64 {
    funding_cash_flow(rate, notional_abs, is_long)
}

/// Returns the signed cash flow (quote currency) for one rollover night.
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

/// Returns every chargeable 17:00-ET rollover instant in `[start_ns, end_ns)`.
#[pyo3_stub_gen::derive::gen_stub_pyfunction(module = "nautilus_trader.backtest")]
#[pyfunction]
#[pyo3(name = "cfd_roll_instants_ns")]
#[must_use]
pub fn py_cfd_roll_instants_ns(start_ns: u64, end_ns: u64) -> Vec<u64> {
    cfd_roll_instants_ns(start_ns, end_ns)
}
