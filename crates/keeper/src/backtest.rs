//! Backtest: replays a year of real hourly funding and spot prices through the
//! same `strategy::decide` the keeper uses. Every headline number in the UI and
//! README comes from `run()`; nothing is typed in by hand.
//!
//! Model (stated so it can be argued with):
//! - start with 10,000 USDC idle; target 3x perp leverage (75% spot, 25% margin)
//! - each hour the short receives `funding * qty * spot` (pays it when negative)
//! - the perp is marked at `spot * (1 + premium)` from the venue's funding history,
//!   so basis moves show up in PnL
//! - costs per rebalance: 1Click fee + slippage on the spot leg, venue taker fee on
//!   the perp leg, a bridge cost on USDC moved between legs
//! - perp size is rounded down to the venue lot; the spot leg is exact
//! - no liquidations are simulated; the minimum margin ratio is reported instead

use crate::data::Hour;
use crate::strategy::{decide, Book, Policy, Reason};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Costs {
    pub spot_bps: f64,
    pub perp_taker_bps: f64,
    pub bridge_bps: f64,
}

impl Default for Costs {
    fn default() -> Self {
        // 1Click without a partner key charges 20 bps (measured in a dry quote) + 5 bps slippage allowance;
        // venue base-tier taker fee 4.5 bps; USDC NEAR -> settlement chain measured at ~4 bps, rounded up.
        Self { spot_bps: 25.0, perp_taker_bps: 4.5, bridge_bps: 10.0 }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct MonthRow {
    pub month: String,
    pub funding_apr_pct: f64,
    pub vault_return_pct: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub label: &'static str,
    pub start_ms: u64,
    pub end_ms: u64,
    pub hours: usize,
    pub start_nav: f64,
    pub end_nav: f64,
    pub net_apr_pct: f64,
    pub apr_7d_pct: f64,
    pub apr_30d_pct: f64,
    pub funding_income: f64,
    pub costs_paid: f64,
    pub basis_pnl: f64,
    pub max_drawdown_pct: f64,
    pub max_drift_pct: f64,
    pub final_net_delta_pct: f64,
    pub max_leverage: f64,
    pub min_margin_ratio_pct: f64,
    pub rebalances: usize,
    pub negative_funding_hours_pct: f64,
    pub mean_funding_apr_pct: f64,
    pub months: Vec<MonthRow>,
    /// Daily NAV (every 24th hour).
    pub nav_daily: Vec<(u64, f64)>,
    /// Last 7 days of hourly states, replayed by the demo vault.
    pub tail: Vec<Snapshot>,
    pub policy: Policy,
    pub costs: Costs,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Snapshot {
    pub t: u64,
    pub px: f64,
    pub funding: f64,
    pub nav: f64,
    pub spot_qty: f64,
    pub short_qty: f64,
    pub margin_equity: f64,
    pub drift_bps: f64,
    pub rebalanced: bool,
}

pub fn run(hours: &[Hour], policy: Policy, costs: Costs, start_nav: f64) -> Report {
    assert!(hours.len() > 48, "not enough data");
    let mut spot_qty = 0.0;
    let mut short_qty = 0.0;
    let mut entry = 0.0; // perp average entry (mark)
    let mut margin_cash = 0.0;
    let mut idle = start_nav;
    let (mut funding_income, mut costs_paid, mut rebalances) = (0.0, 0.0, 0usize);
    let (mut peak, mut max_dd, mut max_drift, mut max_lev, mut min_mr) = (start_nav, 0.0f64, 0.0f64, 0.0f64, f64::MAX);
    let mut navs = Vec::with_capacity(hours.len());
    let mut snaps = Vec::with_capacity(hours.len());

    for h in hours {
        let mark = h.px * (1.0 + h.premium);
        // Funding accrues on the open short for this hour.
        let f = short_qty * h.px * h.funding;
        margin_cash += f;
        funding_income += f;

        let upnl = short_qty * (entry - mark);
        let book = Book { spot_qty, short_qty, margin_equity: margin_cash + upnl, idle, px: h.px };
        let mut rebalanced = false;
        if let Some(t) = decide(&policy, &book) {
            let dq_spot = t.qty - spot_qty;
            let dq_perp = t.qty - short_qty;
            let spot_cost = dq_spot.abs() * h.px * costs.spot_bps / 1e4;
            let perp_cost = dq_perp.abs() * mark * costs.perp_taker_bps / 1e4;
            // Realize PnL on the part of the short that is closed.
            if dq_perp < 0.0 {
                margin_cash += -dq_perp * (entry - mark);
            } else if t.qty > 0.0 {
                entry = (entry * short_qty + mark * dq_perp) / t.qty;
            }
            short_qty = t.qty;
            spot_qty = t.qty;
            let eq_before = book.equity();
            let upnl_new = short_qty * (entry - mark);
            let margin_now = margin_cash + upnl_new;
            let target_margin = t.margin_usd - spot_cost - perp_cost;
            let moved = (target_margin - margin_now).abs();
            let bridge_cost = if t.reason == Reason::Idle { 0.0 } else { moved * costs.bridge_bps / 1e4 };
            let total_cost = spot_cost + perp_cost + bridge_cost;
            costs_paid += total_cost;
            margin_cash = target_margin - bridge_cost - upnl_new;
            idle = 0.0;
            debug_assert!((spot_qty * h.px + margin_cash + upnl_new - (eq_before - total_cost)).abs() < 1e-6);
            rebalances += 1;
            rebalanced = true;
        }

        let upnl = short_qty * (entry - mark);
        let margin_eq = margin_cash + upnl;
        let nav = spot_qty * h.px + margin_eq + idle;
        peak = peak.max(nav);
        max_dd = max_dd.max((peak - nav) / peak);
        let b = Book { spot_qty, short_qty, margin_equity: margin_eq, idle, px: h.px };
        max_drift = max_drift.max(b.drift_bps());
        if short_qty > 0.0 {
            max_lev = max_lev.max(b.leverage());
            min_mr = min_mr.min(margin_eq / (short_qty * mark));
        }
        navs.push((h.t, nav));
        snaps.push(Snapshot {
            t: h.t,
            px: h.px,
            funding: h.funding,
            nav,
            spot_qty,
            short_qty,
            margin_equity: margin_eq,
            drift_bps: b.drift_bps(),
            rebalanced,
        });
    }

    let n = hours.len();
    let end_nav = navs.last().unwrap().1;
    let apr = |from: usize| {
        let (h0, v0) = (from, navs[from].1);
        (end_nav / v0 - 1.0) * 8_760.0 / (n - 1 - h0) as f64 * 100.0
    };
    let neg = hours.iter().filter(|h| h.funding < 0.0).count();
    let mean_f = hours.iter().map(|h| h.funding).sum::<f64>() / n as f64;
    let last = snaps.last().unwrap();
    let final_delta = (last.spot_qty - last.short_qty) * last.px / last.nav * 100.0;
    let basis_pnl = end_nav - start_nav - funding_income + costs_paid;

    Report {
        label: "BACKTEST: real hourly funding (perp venue) and spot prices, simulated execution",
        start_ms: hours[0].t,
        end_ms: hours[n - 1].t,
        hours: n,
        start_nav,
        end_nav,
        net_apr_pct: apr(0),
        apr_7d_pct: apr(n - 1 - 168),
        apr_30d_pct: apr(n - 1 - 720),
        funding_income,
        costs_paid,
        basis_pnl,
        max_drawdown_pct: max_dd * 100.0,
        max_drift_pct: max_drift / 100.0,
        final_net_delta_pct: final_delta,
        max_leverage: max_lev,
        min_margin_ratio_pct: min_mr * 100.0,
        rebalances,
        negative_funding_hours_pct: neg as f64 / n as f64 * 100.0,
        mean_funding_apr_pct: mean_f * 8_760.0 * 100.0,
        months: months(hours, &navs),
        nav_daily: navs.iter().step_by(24).copied().collect(),
        tail: snaps[n.saturating_sub(168)..].to_vec(),
        policy,
        costs,
    }
}

/// Runs the default policy on the embedded data.
pub fn default_report() -> anyhow::Result<Report> {
    let (hours, _) = crate::data::load_embedded()?;
    Ok(run(&hours, Policy::default(), Costs::default(), 10_000.0))
}

fn month_of(ms: u64) -> String {
    // Civil-from-days (Howard Hinnant), UTC.
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y}-{m:02}")
}

fn months(hours: &[Hour], navs: &[(u64, f64)]) -> Vec<MonthRow> {
    let mut out: Vec<MonthRow> = Vec::new();
    let mut i = 0;
    while i < hours.len() {
        let m = month_of(hours[i].t);
        let start = i;
        while i < hours.len() && month_of(hours[i].t) == m {
            i += 1;
        }
        let slice = &hours[start..i];
        let f = slice.iter().map(|h| h.funding).sum::<f64>() / slice.len() as f64 * 8_760.0 * 100.0;
        let v0 = if start == 0 { navs[0].1 } else { navs[start - 1].1 };
        let v1 = navs[i - 1].1;
        out.push(MonthRow { month: m, funding_apr_pct: f, vault_return_pct: (v1 / v0 - 1.0) * 100.0 });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(n: usize, funding: f64) -> Vec<Hour> {
        (0..n).map(|i| Hour { t: i as u64 * 3_600_000, funding, premium: 0.0, px: 2_000.0 }).collect()
    }

    #[test]
    fn flat_price_positive_funding_earns_funding_minus_costs() {
        let r = run(&flat(8_761, 0.0000125), Policy::default(), Costs::default(), 10_000.0);
        // 0.00125%/h on 7,500 notional for a year = 821 USD before entry costs.
        assert!(r.funding_income > 800.0 && r.funding_income < 830.0, "{}", r.funding_income);
        assert!(r.end_nav < 10_000.0 + r.funding_income);
        assert_eq!(r.rebalances, 1);
        assert!(r.max_drift_pct < 0.01);
    }

    #[test]
    fn price_moves_do_not_change_nav_when_hedged() {
        let mut h = flat(500, 0.0);
        for (i, x) in h.iter_mut().enumerate() {
            x.px = 2_000.0 + (i as f64 * 0.37).sin() * 150.0;
        }
        let r = run(&h, Policy::default(), Costs { spot_bps: 0.0, perp_taker_bps: 0.0, bridge_bps: 0.0 }, 10_000.0);
        assert!((r.end_nav - 10_000.0).abs() < 1.0, "end nav {}", r.end_nav);
    }

    #[test]
    fn month_labels() {
        assert_eq!(month_of(1_761_955_200_000), "2025-11");
        assert_eq!(month_of(1_793_491_199_000), "2026-10");
    }

    /// The README's headline table must be the backtest's own output.
    #[test]
    fn readme_matches_backtest() {
        let r = default_report().unwrap();
        let readme = include_str!("../../../README.md");
        for (name, want) in [
            ("Net APR after fees", format!("{:.2}%", r.net_apr_pct)),
            ("APR, last 7 days of the window", format!("{:.2}%", r.apr_7d_pct)),
            ("Max hedge drift", format!("{:.3}%", r.max_drift_pct)),
            ("Max drawdown", format!("{:.2}%", r.max_drawdown_pct)),
        ] {
            let row = readme.lines().find(|l| l.starts_with(&format!("| {name} |"))).expect(name);
            assert!(row.contains(&want), "README row '{row}' should say {want}");
        }
    }

    #[test]
    fn real_data_backtest_is_sane() {
        let r = default_report().unwrap();
        assert!(r.hours > 8_500);
        assert!(r.end_nav.is_finite() && r.end_nav > 0.0);
        assert!(r.min_margin_ratio_pct > 2.0, "would have been liquidated: {}", r.min_margin_ratio_pct);
    }
}
