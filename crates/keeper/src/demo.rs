//! Demo vault: runs with no keys and no network. It replays the last 7 days of
//! the backtest hour by hour (one hour every few seconds) and applies the
//! contract's own share math to simulated deposits. Everything it returns is
//! labelled `DEMO`.

use crate::backtest::{Report, Snapshot};
use serde::Serialize;
use std::collections::BTreeMap;
use crate::math;

pub const DEMO_LABEL: &str = "DEMO: simulated vault replaying backtest hours; no funds move";
const USDC: f64 = 1_000_000.0;

#[derive(Clone, Debug, Serialize)]
pub struct VaultSnapshot {
    pub label: &'static str,
    pub mode: &'static str,
    pub vault_mode: String,
    pub t: u64,
    pub nav_usd: f64,
    pub idle_usd: f64,
    pub total_shares: f64,
    pub price_per_share: f64,
    pub spot_qty: f64,
    pub short_qty: f64,
    pub margin_equity_usd: f64,
    pub px: f64,
    pub funding_hourly: f64,
    pub funding_apr_pct: f64,
    pub net_delta_usd: f64,
    pub net_delta_pct: f64,
    pub drift_bps: f64,
    pub hedge_band_bps: f64,
    pub leverage: f64,
    pub drawdown_pct: f64,
    pub max_drawdown_pct: f64,
    pub apy_7d_pct: f64,
    pub epoch_rebalances: u32,
    pub max_rebalances_per_epoch: u32,
    pub events: Vec<Event>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Event {
    pub t: u64,
    pub kind: String,
    pub detail: String,
}

pub struct DemoVault {
    tail: Vec<Snapshot>,
    scale: f64,
    step: usize,
    total_shares: u128,
    /// Simulated idle USDC from demo deposits, base units.
    idle: u128,
    shares: BTreeMap<String, u128>,
    events: Vec<Event>,
    hwm: f64,
    apy_7d: f64,
    max_dd: f64,
}

impl DemoVault {
    /// Seeds a 25,000 USDC vault: the backtest position scaled 2.5x.
    pub fn new(report: &Report) -> Self {
        let tail = report.tail.clone();
        let scale = 25_000.0 / tail[0].nav;
        let seed = (25_000.0 * USDC) as u128;
        let mut shares = BTreeMap::new();
        shares.insert("seed.demo".to_string(), seed);
        Self {
            tail,
            scale,
            step: 0,
            total_shares: seed,
            idle: 0,
            shares,
            events: vec![],
            hwm: 0.0,
            apy_7d: report.apr_7d_pct,
            max_dd: report.max_drawdown_pct,
        }
    }

    fn snap(&self) -> &Snapshot {
        &self.tail[self.step % self.tail.len()]
    }

    fn deployed_base(&self) -> u128 {
        (self.snap().nav * self.scale * USDC) as u128
    }

    fn nav_base(&self) -> u128 {
        self.deployed_base() + self.idle
    }

    /// Advances one replayed hour.
    pub fn tick(&mut self) {
        self.step += 1;
        let s = *self.snap();
        if s.rebalanced {
            self.push(s.t, "rebalance", format!("legs reset to {:.4} ETH each", s.spot_qty * self.scale));
        }
        if self.step % self.tail.len() == 0 {
            self.push(s.t, "replay", "7-day replay restarted".into());
        }
    }

    pub fn deposit(&mut self, account: &str, usd: f64) -> anyhow::Result<u128> {
        anyhow::ensure!(usd >= 1.0 && usd <= 1_000_000.0, "deposit must be between 1 and 1,000,000 USDC");
        let amount = (usd * USDC) as u128;
        let minted = math::shares_for_deposit(amount, self.nav_base(), self.total_shares);
        *self.shares.entry(account.to_string()).or_default() += minted;
        self.total_shares += minted;
        self.idle += amount;
        self.push(self.snap().t, "deposit", format!("{account} deposited {usd:.2} USDC, minted {:.4} shares", minted as f64 / USDC));
        Ok(minted)
    }

    pub fn withdraw(&mut self, account: &str, shares: f64) -> anyhow::Result<u128> {
        let s = (shares * USDC) as u128;
        let held = self.shares.get(account).copied().unwrap_or(0);
        anyhow::ensure!(s > 0 && s <= held, "not enough shares");
        let out = math::assets_for_shares(s, self.nav_base(), self.total_shares);
        anyhow::ensure!(out <= self.idle, "demo vault only pays from idle USDC; in the real vault this is queued");
        self.shares.insert(account.to_string(), held - s);
        self.total_shares -= s;
        self.idle -= out;
        self.push(self.snap().t, "withdraw", format!("{account} withdrew {:.2} USDC", out as f64 / USDC));
        Ok(out)
    }

    pub fn shares_of(&self, account: &str) -> f64 {
        self.shares.get(account).copied().unwrap_or(0) as f64 / USDC
    }

    fn push(&mut self, t: u64, kind: &str, detail: String) {
        self.events.push(Event { t, kind: kind.into(), detail });
        if self.events.len() > 50 {
            self.events.remove(0);
        }
    }

    pub fn snapshot(&mut self) -> VaultSnapshot {
        let s = *self.snap();
        let nav = self.nav_base() as f64 / USDC;
        let pps = math::price_per_share(self.nav_base(), self.total_shares) as f64 / math::PPS_SCALE as f64;
        self.hwm = self.hwm.max(pps);
        let spot = s.spot_qty * self.scale;
        let short = s.short_qty * self.scale;
        let margin = s.margin_equity * self.scale;
        let delta = (spot - short) * s.px;
        VaultSnapshot {
            label: DEMO_LABEL,
            mode: "demo",
            vault_mode: "Active".into(),
            t: s.t,
            nav_usd: nav,
            idle_usd: self.idle as f64 / USDC,
            total_shares: self.total_shares as f64 / USDC,
            price_per_share: pps,
            spot_qty: spot,
            short_qty: short,
            margin_equity_usd: margin,
            px: s.px,
            funding_hourly: s.funding,
            funding_apr_pct: s.funding * 8_760.0 * 100.0,
            net_delta_usd: delta,
            net_delta_pct: delta / nav * 100.0,
            drift_bps: s.drift_bps,
            hedge_band_bps: 100.0,
            leverage: if margin > 0.0 { short * s.px / margin } else { 0.0 },
            drawdown_pct: (self.hwm - pps) / self.hwm * 100.0,
            max_drawdown_pct: self.max_dd,
            apy_7d_pct: self.apy_7d,
            epoch_rebalances: s.rebalanced as u32,
            max_rebalances_per_epoch: 6,
            events: self.events.iter().rev().take(8).cloned().collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_is_deterministic_and_share_math_holds() {
        let r = crate::backtest::default_report().unwrap();
        let mut a = DemoVault::new(&r);
        let mut b = DemoVault::new(&r);
        for _ in 0..30 {
            a.tick();
            b.tick();
        }
        assert_eq!(a.snapshot().nav_usd, b.snapshot().nav_usd);
        let minted = a.deposit("alice.demo", 1_000.0).unwrap();
        assert!(minted > 0);
        let out = a.withdraw("alice.demo", minted as f64 / USDC).unwrap();
        assert!(out <= 1_000 * 1_000_000, "round trip paid more than deposited");
    }
}
