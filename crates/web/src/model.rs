//! What the page needs, as plain serializable data. The server fills it from
//! the keeper (demo vault + backtest); the browser only ever sees these.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Snap {
    pub label: String,
    pub mode: String,
    pub vault_mode: String,
    pub t: u64,
    pub nav_usd: f64,
    pub idle_usd: f64,
    pub price_per_share: f64,
    pub spot_qty: f64,
    pub short_qty: f64,
    pub margin_equity_usd: f64,
    pub px: f64,
    pub funding_apr_pct: f64,
    pub net_delta_usd: f64,
    pub net_delta_pct: f64,
    pub drift_bps: f64,
    pub hedge_band_bps: f64,
    pub leverage: f64,
    pub drawdown_pct: f64,
    pub apy_7d_pct: f64,
    pub epoch_rebalances: u32,
    pub max_rebalances_per_epoch: u32,
    pub events: Vec<Ev>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Ev {
    pub t: u64,
    pub kind: String,
    pub detail: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Month {
    pub month: String,
    pub funding_apr_pct: f64,
    pub vault_return_pct: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Bt {
    pub label: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub hours: usize,
    pub net_apr_pct: f64,
    pub apr_7d_pct: f64,
    pub apr_30d_pct: f64,
    pub funding_income: f64,
    pub costs_paid: f64,
    pub max_drawdown_pct: f64,
    pub max_drift_pct: f64,
    pub max_leverage: f64,
    pub min_margin_ratio_pct: f64,
    pub rebalances: usize,
    pub negative_funding_hours_pct: f64,
    pub mean_funding_apr_pct: f64,
    pub months: Vec<Month>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RcptDay {
    pub date: String,
    pub earned_usd: f64,
    pub funding_usd: f64,
    pub fees_usd: f64,
    pub benchmark_rate_pct: f64,
    pub benchmark_earned_usd: f64,
    pub difference_usd: f64,
    pub source: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RcptYear {
    pub vault_earned_usd: f64,
    pub benchmark_earned_usd: f64,
    pub days_vault_ahead: usize,
    pub days: usize,
    pub months_vault_ahead: usize,
    pub months: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Dash {
    pub snap: Snap,
    pub bt: Bt,
    pub day: RcptDay,
    pub year: RcptYear,
}

pub fn fmt_usd(v: f64) -> String {
    let neg = v < 0.0;
    let whole = v.abs().round() as u64;
    let s = whole.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    format!("{}${out}", if neg { "−" } else { "" })
}

pub fn fmt_signed_pct(v: f64, dp: usize) -> String {
    let sign = if v < 0.0 { "−" } else { "+" };
    format!("{sign}{:.dp$}%", v.abs())
}

pub fn fmt_date(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    const M: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    format!("{} {} {}", d, M[(m - 1) as usize], y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(fmt_usd(25_431.6), "$25,432");
        assert_eq!(fmt_usd(-12.0), "−$12");
        assert_eq!(fmt_signed_pct(0.0123, 2), "+0.01%");
        assert_eq!(fmt_date(1_761_955_200_000), "1 Nov 2025");
    }
}
