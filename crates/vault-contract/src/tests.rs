//! Unit tests run against near-sdk's mocked blockchain. The same invariants
//! are exercised end to end in `tests/sandbox.rs`.
#![allow(unused_must_use)]

use super::*;
use near_sdk::test_utils::{get_logs, VMContextBuilder};
use near_sdk::testing_env;

const MS: u64 = 1_000_000;
const T0_MS: u64 = 1_790_000_000_000;

fn acc(s: &str) -> AccountId {
    s.parse().unwrap()
}

fn implicit() -> AccountId {
    acc(&"ab".repeat(32))
}

fn ctx(predecessor: &str, deposit: u128, at_ms: u64) {
    let mut b = VMContextBuilder::new();
    b.current_account_id(acc("vault.near"))
        .predecessor_account_id(acc(predecessor))
        .attached_deposit(NearToken::from_yoctonear(deposit))
        .block_timestamp(at_ms * MS);
    testing_env!(b.build());
}

fn cfg() -> Config {
    Config { perp_evm_address: "0x1111111111111111111111111111111111111111".into(), ..Config::default() }
}

fn vault() -> Vault {
    ctx("owner.near", 0, T0_MS);
    Vault::new(
        acc("owner.near"),
        acc("usdc.near"),
        acc("eth.near"),
        acc("mpc.near"),
        vec![acc("keeper.near")],
        Some(cfg()),
    )
}

fn deposit(v: &mut Vault, who: &str, amount: u128) {
    ctx("usdc.near", 0, T0_MS);
    v.ft_on_transfer(acc(who), U128(amount), "deposit".into());
}

/// Puts the vault into a hedged state: 10 ETH spot, 10 ETH short at $2,000.
fn hedged() -> Vault {
    let mut v = vault();
    deposit(&mut v, "alice.near", 40_000_000_000);
    v.spot_qty_e8 = 10 * 100_000_000;
    v.perp_short_e8 = 10 * 100_000_000;
    v.mark_px_e8 = 2_000 * 100_000_000;
    v
}

fn order(is_buy: bool, sz_eth_e8: u128, reduce_only: bool) -> PerpOrder {
    PerpOrder { is_buy, sz_e8: U128(sz_eth_e8), limit_px_e8: U128(2_000 * 100_000_000), reduce_only }
}

#[test]
fn deposit_mints_one_to_one_then_pro_rata() {
    let mut v = vault();
    deposit(&mut v, "alice.near", 10_000_000_000);
    assert_eq!(v.shares_of(acc("alice.near")).0, 10_000_000_000);
    // NAV doubles: a new depositor gets about half the shares per USDC
    // (the 1-USDC virtual offset shifts it by a hair).
    v.deployed = 10_000_000_000;
    deposit(&mut v, "bob.near", 10_000_000_000);
    let bob = v.shares_of(acc("bob.near")).0;
    assert!(bob >= 5_000_000_000 && bob < 5_001_000_000, "bob={bob}");
}

#[test]
#[should_panic(expected = "only USDC deposits")]
fn rejects_other_tokens() {
    let mut v = vault();
    ctx("fake.near", 0, T0_MS);
    v.ft_on_transfer(acc("alice.near"), U128(10_000_000), "deposit".into());
}

#[test]
fn withdraw_queues_when_idle_is_short() {
    let mut v = vault();
    deposit(&mut v, "alice.near", 10_000_000);
    v.idle = 2_000_000;
    v.deployed = 8_000_000;
    ctx("alice.near", 0, T0_MS);
    let id = v.withdraw(U128(5_000_000));
    let w = v.get_withdrawal(id.0).expect("queued");
    assert_eq!(w.amount.0, 5_000_000);
    assert_eq!(v.owed, 5_000_000);
    assert_eq!(v.get_state().nav.0, 5_000_000);
}

#[test]
fn withdraw_works_while_paused_and_unwinding() {
    let mut v = vault();
    deposit(&mut v, "alice.near", 10_000_000);
    ctx("owner.near", 0, T0_MS);
    v.pause();
    ctx("alice.near", 0, T0_MS);
    v.withdraw(U128(1_000_000));
    ctx("owner.near", 0, T0_MS);
    v.emergency_unwind();
    ctx("alice.near", 0, T0_MS);
    v.withdraw(U128(1_000_000));
    assert_eq!(v.shares_of(acc("alice.near")).0, 8_000_000);
}

#[test]
#[should_panic(expected = "deposits are closed")]
fn no_deposits_while_paused() {
    let mut v = vault();
    ctx("keeper.near", 0, T0_MS);
    v.pause();
    deposit(&mut v, "alice.near", 10_000_000);
}

#[test]
#[should_panic(expected = "only keeper")]
fn stranger_cannot_report() {
    let mut v = hedged();
    ctx("mallory.near", 0, T0_MS);
    v.report(U128(0), U128(0), U128(1));
}

#[test]
#[should_panic(expected = "only owner")]
fn keeper_cannot_resume() {
    let mut v = vault();
    ctx("keeper.near", 0, T0_MS);
    v.pause();
    v.resume();
}

#[test]
fn order_inside_band_is_signed() {
    let mut v = hedged();
    ctx("keeper.near", 1, T0_MS);
    // 10 -> 10.05 ETH short: 50 bps drift, inside the 100 bps band.
    v.request_perp_order(order(false, 5_000_000, false), U64(T0_MS));
    assert_eq!(v.perp_short_e8, 1_005_000_000);
    assert_eq!(v.signs.len(), 1);
    assert!(get_logs().iter().any(|l| l.contains("sign_requested")));
}

#[test]
#[should_panic(expected = "order breaks the hedge band")]
fn order_outside_band_is_refused() {
    let mut v = hedged();
    ctx("keeper.near", 1, T0_MS);
    v.request_perp_order(order(false, 50_000_000, false), U64(T0_MS));
}

#[test]
fn order_toward_neutral_is_allowed_even_outside_band() {
    let mut v = hedged();
    v.perp_short_e8 = 5 * 100_000_000; // 50% drift after spot arrived
    ctx("keeper.near", 1, T0_MS);
    v.request_perp_order(order(false, 2 * 100_000_000, false), U64(T0_MS));
    assert_eq!(v.perp_short_e8, 7 * 100_000_000);
}

#[test]
#[should_panic(expected = "limit price too far from mark")]
fn price_sanity() {
    let mut v = hedged();
    ctx("keeper.near", 1, T0_MS);
    let mut o = order(false, 1_000_000, false);
    o.limit_px_e8 = U128(1_900 * 100_000_000);
    v.request_perp_order(o, U64(T0_MS));
}

#[test]
#[should_panic(expected = "epoch rebalance limit")]
fn epoch_rebalance_cap() {
    let mut v = hedged();
    for i in 0..7u64 {
        ctx("keeper.near", 1, T0_MS + i);
        let o = order(i % 2 == 0, 1_000_000, i % 2 == 0);
        v.request_perp_order(o, U64(T0_MS + i));
    }
}

#[test]
fn epoch_resets_after_epoch_secs() {
    let mut v = hedged();
    for i in 0..6u64 {
        ctx("keeper.near", 1, T0_MS + i);
        v.request_perp_order(order(i % 2 == 0, 1_000_000, i % 2 == 0), U64(T0_MS + i));
    }
    ctx("keeper.near", 1, T0_MS + 3_600_000);
    v.request_perp_order(order(true, 1_000_000, true), U64(T0_MS + 3_600_000));
}

#[test]
#[should_panic(expected = "nonce must increase")]
fn nonce_replay_refused() {
    let mut v = hedged();
    ctx("keeper.near", 1, T0_MS);
    v.request_perp_order(order(false, 1_000_000, false), U64(T0_MS));
    v.request_perp_order(order(true, 1_000_000, true), U64(T0_MS));
}

#[test]
#[should_panic(expected = "nav move exceeds per-report limit")]
fn nav_report_bounded() {
    let mut v = hedged();
    ctx("keeper.near", 0, T0_MS);
    // NAV is 40,000 USDC; a report claiming +10% is refused.
    v.report(U128(4_000_000_000), U128(1_000_000_000), U128(2_000 * 100_000_000));
}

#[test]
fn drawdown_switches_to_unwind() {
    let mut v = vault();
    deposit(&mut v, "alice.near", 10_000_000_000);
    v.idle = 0;
    v.deployed = 10_000_000_000;
    ctx("keeper.near", 0, T0_MS);
    // Walk NAV down 3% per report (inside the per-report cap) until the 5% stop trips.
    let mut nav = 10_000_000_000u128;
    for _ in 0..2 {
        nav = nav * 97 / 100;
        v.report(U128(nav), U128(0), U128(1));
    }
    assert_eq!(v.mode, Mode::Unwinding);
    assert!(get_logs().iter().any(|l| l.contains("max drawdown breached")));
}

#[test]
#[should_panic(expected = "unwinding: reduce-only buys only")]
fn unwind_refuses_new_shorts() {
    let mut v = hedged();
    ctx("owner.near", 0, T0_MS);
    v.emergency_unwind();
    ctx("keeper.near", 1, T0_MS);
    v.request_perp_order(order(false, 1_000_000, false), U64(T0_MS));
}

#[test]
fn unwind_allows_reduce_only_buy() {
    let mut v = hedged();
    ctx("owner.near", 0, T0_MS);
    v.emergency_unwind();
    ctx("keeper.near", 1, T0_MS);
    v.request_perp_order(order(true, 100_000_000, true), U64(T0_MS));
    assert_eq!(v.perp_short_e8, 9 * 100_000_000);
}

#[test]
fn paired_reduce_moves_both_legs() {
    let mut v = hedged();
    ctx("keeper.near", 1, T0_MS);
    v.request_paired_reduce(U128(200_000_000), U128(2_000 * 100_000_000), U64(T0_MS), implicit());
    assert_eq!(v.spot_qty_e8, 8 * 100_000_000);
    assert_eq!(v.perp_short_e8, 8 * 100_000_000);
}

#[test]
#[should_panic(expected = "deposit address must be an implicit account")]
fn allocate_only_to_implicit_accounts() {
    let mut v = hedged();
    ctx("keeper.near", 0, T0_MS);
    v.allocate(Leg::Spot, U128(1_000_000), acc("mallory.near"));
}

#[test]
#[should_panic(expected = "epoch allocation limit")]
fn allocation_cap() {
    let mut v = vault();
    deposit(&mut v, "alice.near", 10_000_000_000);
    ctx("keeper.near", 0, T0_MS);
    v.allocate(Leg::Spot, U128(7_000_000_000), implicit());
}

#[test]
#[should_panic(expected = "hedge band out of range")]
fn config_hard_limits() {
    let mut v = vault();
    ctx("owner.near", 0, T0_MS);
    v.set_config(Config { hedge_band_bps: 5_000, ..cfg() });
}

#[test]
fn digest_matches_pure_function() {
    let mut v = hedged();
    ctx("keeper.near", 1, T0_MS);
    let o = order(false, 5_000_000, false);
    v.request_perp_order(o.clone(), U64(T0_MS));
    let wire = hl::OrderWire {
        asset: 1,
        is_buy: false,
        px: "2000".into(),
        sz: "0.05".into(),
        reduce_only: false,
        tif: hl::Tif::Ioc,
    };
    let expected = hex::encode(hl::order_digest(&wire, T0_MS, true, keccak));
    assert_eq!(v.signs.get(0).unwrap().digest, expected);
}

#[test]
#[should_panic(expected = "destinations are frozen")]
fn owner_cannot_redirect_margin_destination() {
    let mut v = vault();
    ctx("owner.near", 0, T0_MS);
    v.set_config(Config { perp_evm_address: "0x2222222222222222222222222222222222222222".into(), ..cfg() });
}

#[test]
fn owner_can_set_destination_once_and_tune_limits() {
    ctx("owner.near", 0, T0_MS);
    let mut v = Vault::new(acc("owner.near"), acc("usdc.near"), acc("eth.near"), acc("mpc.near"), vec![], None);
    v.set_config(cfg());
    v.set_config(Config { hedge_band_bps: 50, ..cfg() });
    assert_eq!(v.get_config().hedge_band_bps, 50);
}
