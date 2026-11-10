//! Pure vault arithmetic. No NEAR imports, so the keeper's demo vault and the
//! backtest use exactly the same rules as the contract.

/// Virtual shares and assets (1 share / 1 USDC at 6 decimals). They make the
/// first-depositor inflation attack unprofitable and keep share price defined
/// when the vault is empty.
pub const VIRTUAL_SHARES: u128 = 1_000_000;
pub const VIRTUAL_ASSETS: u128 = 1_000_000;

/// Fixed-point scale for price-per-share and the high-water mark.
pub const PPS_SCALE: u128 = 1_000_000_000_000;

pub const BPS: u128 = 10_000;

/// Shares minted for `assets` deposited into a vault holding `nav` with `supply` shares.
/// Rounds down (in the vault's favour).
pub fn shares_for_deposit(assets: u128, nav: u128, supply: u128) -> u128 {
    mul_div(assets, supply + VIRTUAL_SHARES, nav + VIRTUAL_ASSETS)
}

/// Assets paid out for burning `shares`. Rounds down (in the vault's favour).
pub fn assets_for_shares(shares: u128, nav: u128, supply: u128) -> u128 {
    mul_div(shares, nav + VIRTUAL_ASSETS, supply + VIRTUAL_SHARES)
}

/// Price per share scaled by [`PPS_SCALE`].
pub fn price_per_share(nav: u128, supply: u128) -> u128 {
    mul_div(nav + VIRTUAL_ASSETS, PPS_SCALE, supply + VIRTUAL_SHARES)
}

/// `|a - b| / max(a, b)` in basis points; 0 when both are 0.
pub fn drift_bps(spot: u128, short: u128) -> u128 {
    let hi = spot.max(short);
    if hi == 0 {
        return 0;
    }
    mul_div(spot.abs_diff(short), BPS, hi)
}

/// `x * bps / 10_000`, rounding down.
pub fn bps_of(x: u128, bps: u128) -> u128 {
    mul_div(x, bps, BPS)
}

/// True when `pps` is more than `max_dd_bps` below `hwm`.
pub fn drawdown_breached(pps: u128, hwm: u128, max_dd_bps: u128) -> bool {
    if hwm == 0 {
        return false;
    }
    let floor = hwm - bps_of(hwm, max_dd_bps);
    pps < floor
}

/// Drawdown from the high-water mark in basis points.
pub fn drawdown_bps(pps: u128, hwm: u128) -> u128 {
    if hwm == 0 || pps >= hwm {
        return 0;
    }
    mul_div(hwm - pps, BPS, hwm)
}

/// `a * b / d` without overflow for the ranges a vault sees (u128 split into
/// 64-bit limbs would be overkill: NAV < 2^64 in USDC base units).
pub fn mul_div(a: u128, b: u128, d: u128) -> u128 {
    assert!(d != 0, "division by zero");
    match a.checked_mul(b) {
        Some(p) => p / d,
        None => {
            // Fall back to a 256-bit product.
            let (hi, lo) = mul_wide(a, b);
            div_wide(hi, lo, d)
        }
    }
}

fn mul_wide(a: u128, b: u128) -> (u128, u128) {
    let mask = u64::MAX as u128;
    let (a_hi, a_lo) = (a >> 64, a & mask);
    let (b_hi, b_lo) = (b >> 64, b & mask);
    let ll = a_lo * b_lo;
    let lh = a_lo * b_hi;
    let hl = a_hi * b_lo;
    let hh = a_hi * b_hi;
    let mid = (ll >> 64) + (lh & mask) + (hl & mask);
    let lo = (ll & mask) | ((mid & mask) << 64);
    let hi = hh + (lh >> 64) + (hl >> 64) + (mid >> 64);
    (hi, lo)
}

fn div_wide(hi: u128, lo: u128, d: u128) -> u128 {
    assert!(hi < d, "mul_div overflow");
    // Restoring long division, one bit at a time.
    let mut rem = hi;
    let mut q: u128 = 0;
    for i in (0..128).rev() {
        let carry = rem >> 127;
        rem = (rem << 1) | ((lo >> i) & 1);
        if carry == 1 || rem >= d {
            rem = rem.wrapping_sub(d);
            q |= 1 << i;
        }
    }
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_deposit_is_one_to_one() {
        assert_eq!(shares_for_deposit(5_000_000, 0, 0), 5_000_000);
    }

    #[test]
    fn round_trip_never_pays_more_than_deposited() {
        for (nav, supply) in [(0, 0), (10_000_000, 9_000_000), (123_456_789, 100_000_000)] {
            let s = shares_for_deposit(1_000_000, nav, supply);
            let back = assets_for_shares(s, nav + 1_000_000, supply + s);
            assert!(back <= 1_000_000, "nav={nav} supply={supply} back={back}");
        }
    }

    #[test]
    fn donation_attack_is_unprofitable() {
        // Attacker deposits 1 unit, then donates 1,000 USDC to inflate the share price.
        let attacker = shares_for_deposit(1, 0, 0);
        let nav = 1 + 1_000_000_000;
        let victim = shares_for_deposit(2_000_000_000, nav, attacker);
        assert!(victim > 0);
        let attacker_out = assets_for_shares(attacker, nav + 2_000_000_000, attacker + victim);
        assert!(attacker_out < 1_000_000_000, "attacker recovered the donation: {attacker_out}");
    }

    #[test]
    fn drift_is_symmetric() {
        assert_eq!(drift_bps(100, 99), 100);
        assert_eq!(drift_bps(99, 100), 100);
        assert_eq!(drift_bps(0, 0), 0);
    }

    #[test]
    fn drawdown_threshold() {
        let hwm = PPS_SCALE;
        assert!(!drawdown_breached(hwm - bps_of(hwm, 500), hwm, 500));
        assert!(drawdown_breached(hwm - bps_of(hwm, 501), hwm, 500));
    }

    #[test]
    fn mul_div_wide() {
        let a = u128::MAX / 3;
        assert_eq!(mul_div(a, 6, 6), a);
        assert_eq!(mul_div(1 << 100, 1 << 27, 1 << 30), 1 << 97);
    }
}
