//! Rebalance policy. One function, used by both the backtest and the live
//! keeper loop, so the backtest's numbers come from the code that trades.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Policy {
    /// Target perp leverage: short notional / margin equity.
    pub target_lev: f64,
    /// Rebalance when leverage leaves [lev_lo, lev_hi].
    pub lev_lo: f64,
    pub lev_hi: f64,
    /// Rebalance when |spot - short| / spot exceeds this (bps). Mirrors the contract band.
    pub band_bps: f64,
    /// Idle USDC above this is put to work.
    pub min_trade_usd: f64,
    /// Perp lot size in coins (ETH trades in 0.0001 on the default venue).
    pub lot: f64,
}

impl Default for Policy {
    fn default() -> Self {
        Self { target_lev: 3.0, lev_lo: 2.0, lev_hi: 4.5, band_bps: 100.0, min_trade_usd: 50.0, lot: 0.0001 }
    }
}

/// What the keeper sees.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Book {
    pub spot_qty: f64,
    pub short_qty: f64,
    /// Margin account equity on the venue (cash + unrealized PnL), USD.
    pub margin_equity: f64,
    /// USDC sitting in the vault contract, not yet deployed.
    pub idle: f64,
    /// Spot price.
    pub px: f64,
}

impl Book {
    pub fn equity(&self) -> f64 {
        self.spot_qty * self.px + self.margin_equity + self.idle
    }
    pub fn leverage(&self) -> f64 {
        if self.margin_equity <= 0.0 {
            return f64::INFINITY;
        }
        self.short_qty * self.px / self.margin_equity
    }
    pub fn drift_bps(&self) -> f64 {
        let hi = self.spot_qty.max(self.short_qty);
        if hi == 0.0 {
            0.0
        } else {
            (self.spot_qty - self.short_qty).abs() / hi * 10_000.0
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub enum Reason {
    Idle,
    LeverageHigh,
    LeverageLow,
    Drift,
}

/// Target position after a rebalance: both legs at `qty`, margin at `margin_usd`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Target {
    pub qty: f64,
    pub margin_usd: f64,
    pub reason: Reason,
}

pub fn decide(p: &Policy, b: &Book) -> Option<Target> {
    let reason = if b.idle >= p.min_trade_usd {
        Reason::Idle
    } else if b.short_qty > 0.0 && b.leverage() > p.lev_hi {
        Reason::LeverageHigh
    } else if b.short_qty > 0.0 && b.leverage() < p.lev_lo {
        Reason::LeverageLow
    } else if b.drift_bps() > p.band_bps {
        Reason::Drift
    } else {
        return None;
    };
    let e = b.equity();
    let spot_val = e * p.target_lev / (p.target_lev + 1.0);
    let qty = (spot_val / b.px / p.lot).floor() * p.lot;
    Some(Target { qty, margin_usd: e - qty * b.px, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_cash_is_deployed_three_to_one() {
        let b = Book { idle: 10_000.0, px: 2_000.0, ..Default::default() };
        let t = decide(&Policy::default(), &b).unwrap();
        assert_eq!(t.reason, Reason::Idle);
        assert!((t.qty - 3.75).abs() < 1e-9);
        assert!((t.margin_usd - 2_500.0).abs() < 1e-6);
    }

    #[test]
    fn quiet_book_does_nothing() {
        let b = Book { spot_qty: 3.75, short_qty: 3.75, margin_equity: 2_500.0, idle: 0.0, px: 2_000.0 };
        assert!(decide(&Policy::default(), &b).is_none());
    }

    #[test]
    fn price_rally_triggers_deleverage() {
        // ETH +40%: short loses 3.75 * 800 = 3,000, margin falls to -500 + ... → leverage explodes.
        let b = Book { spot_qty: 3.75, short_qty: 3.75, margin_equity: 1_200.0, idle: 0.0, px: 2_800.0 };
        let t = decide(&Policy::default(), &b).unwrap();
        assert_eq!(t.reason, Reason::LeverageHigh);
        assert!(t.margin_usd > 1_200.0);
    }
}
