//! Daily receipt: what the vault earned on a day vs. what US Treasury bills
//! paid on the same money, with the benchmark's source. Built from the
//! backtest (labelled as such) until the vault runs live.

use crate::backtest::{Day, Report};
use anyhow::{bail, Context, Result};
use serde::Serialize;

pub const BENCHMARK_CSV: &str = include_str!("../../../data/benchmark-tbill-monthly.csv");
pub const BENCHMARK_NAME: &str = "US Treasury bills, average interest rate (monthly)";
pub const BENCHMARK_SOURCE: &str = "https://api.fiscaldata.treasury.gov/services/api/fiscal_service/v2/accounting/od/avg_interest_rates?filter=security_desc:eq:Treasury%20Bills";

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Receipt {
    pub label: &'static str,
    pub date: String,
    pub deposit_usd: f64,
    pub nav_start: f64,
    pub nav_end: f64,
    pub earned_usd: f64,
    pub funding_usd: f64,
    pub fees_usd: f64,
    pub basis_usd: f64,
    pub vault_apr_pct: f64,
    pub benchmark: &'static str,
    pub benchmark_rate_pct: f64,
    pub benchmark_earned_usd: f64,
    pub difference_usd: f64,
    pub source: &'static str,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct YearSummary {
    pub label: &'static str,
    pub vault_earned_usd: f64,
    pub benchmark_earned_usd: f64,
    pub days_vault_ahead: usize,
    pub days: usize,
    pub months_vault_ahead: usize,
    pub months: usize,
    pub benchmark: &'static str,
    pub source: &'static str,
}

/// (year, month) -> T-bill average rate in percent.
pub fn benchmark_rates() -> Result<Vec<((i64, u32), f64)>> {
    let mut out = Vec::new();
    for line in BENCHMARK_CSV.lines().skip(1).filter(|l| !l.is_empty()) {
        let (date, rate) = line.split_once(',').context("bad benchmark row")?;
        let y: i64 = date[0..4].parse()?;
        let m: u32 = date[5..7].parse()?;
        out.push(((y, m), rate.parse()?));
    }
    if out.is_empty() {
        bail!("benchmark data is empty");
    }
    Ok(out)
}

fn ym(ms: u64) -> (i64, u32, u32) {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

fn rate_for(rates: &[((i64, u32), f64)], ms: u64) -> f64 {
    let (y, m, _) = ym(ms);
    rates.iter().find(|(k, _)| *k == (y, m)).or(rates.last()).map(|(_, r)| *r).unwrap()
}

pub fn for_day(r: &Report, d: &Day, rates: &[((i64, u32), f64)]) -> Receipt {
    let (y, m, dd) = ym(d.t);
    let rate = rate_for(rates, d.t);
    let earned = d.nav_end - d.nav_start;
    let bench = d.nav_start * rate / 100.0 / 365.0;
    Receipt {
        label: "BACKTEST day (real funding and prices, simulated execution)",
        date: format!("{y}-{m:02}-{dd:02}"),
        deposit_usd: r.start_nav,
        nav_start: d.nav_start,
        nav_end: d.nav_end,
        earned_usd: earned,
        funding_usd: d.funding,
        fees_usd: d.costs,
        basis_usd: earned - d.funding + d.costs,
        vault_apr_pct: earned / d.nav_start * 365.0 * 100.0,
        benchmark: BENCHMARK_NAME,
        benchmark_rate_pct: rate,
        benchmark_earned_usd: bench,
        difference_usd: earned - bench,
        source: BENCHMARK_SOURCE,
    }
}

/// Receipt for the last full day of the backtest.
pub fn latest(r: &Report) -> Result<Receipt> {
    let rates = benchmark_rates()?;
    let d = r.days.iter().rev().find(|d| d.nav_end > 0.0).context("no days")?;
    Ok(for_day(r, d, &rates))
}

/// Receipt for a given `YYYY-MM-DD` inside the window.
pub fn on(r: &Report, date: &str) -> Result<Receipt> {
    let rates = benchmark_rates()?;
    r.days
        .iter()
        .map(|d| for_day(r, d, &rates))
        .find(|x| x.date == date)
        .with_context(|| format!("{date} is outside the backtest window"))
}

pub fn year(r: &Report) -> Result<YearSummary> {
    let rates = benchmark_rates()?;
    let all: Vec<Receipt> = r.days.iter().map(|d| for_day(r, d, &rates)).collect();
    let mut months: Vec<(String, f64, f64)> = Vec::new();
    for x in &all {
        let key = x.date[..7].to_string();
        match months.last_mut() {
            Some(m) if m.0 == key => {
                m.1 += x.earned_usd;
                m.2 += x.benchmark_earned_usd;
            }
            _ => months.push((key, x.earned_usd, x.benchmark_earned_usd)),
        }
    }
    Ok(YearSummary {
        label: "BACKTEST year, $10,000 deposit",
        vault_earned_usd: all.iter().map(|x| x.earned_usd).sum(),
        benchmark_earned_usd: all.iter().map(|x| x.benchmark_earned_usd).sum(),
        days_vault_ahead: all.iter().filter(|x| x.difference_usd > 0.0).count(),
        days: all.len(),
        months_vault_ahead: months.iter().filter(|m| m.1 > m.2).count(),
        months: months.len(),
        benchmark: BENCHMARK_NAME,
        source: BENCHMARK_SOURCE,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benchmark_covers_every_month_of_the_window() {
        let r = benchmark_rates().unwrap();
        assert_eq!(r.len(), 12);
        assert!(r.iter().all(|(_, v)| *v > 0.0 && *v < 20.0));
    }

    #[test]
    fn receipts_add_up_to_the_backtest() {
        let r = crate::backtest::default_report().unwrap();
        let y = year(&r).unwrap();
        assert!((y.vault_earned_usd - (r.end_nav - r.start_nav)).abs() < 1e-6);
        let l = latest(&r).unwrap();
        assert!((l.earned_usd - (l.funding_usd - l.fees_usd + l.basis_usd)).abs() < 1e-9);
        assert_eq!(on(&r, &l.date).unwrap(), l);
        assert!(on(&r, "2020-01-01").is_err());
    }
}
