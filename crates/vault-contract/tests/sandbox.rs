//! End-to-end invariant tests on a local NEAR sandbox (near-workspaces).
//!
//! Real wasm, real cross-contract calls: the vault, two NEP-141 tokens (USDC,
//! bridged ETH) and a stand-in MPC signer that uses the network's key
//! derivation with a public test key. Nothing touches testnet or mainnet.

use anyhow::Result;
use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use near_workspaces::network::Sandbox;
use near_workspaces::result::ExecutionFinalResult;
use near_workspaces::types::NearToken;
use near_workspaces::{Account, Contract, Worker};
use serde_json::{json, Value};

const USDC: u128 = 1_000_000;
const E8: u128 = 100_000_000;
const ETH_UNIT: u128 = 10_000_000_000; // 1e18 / 1e8
const PATH: &str = "trimtab-perp-1";

struct Env {
    worker: Worker<Sandbox>,
    owner: Account,
    keeper: Account,
    alice: Account,
    mallory: Account,
    usdc: Contract,
    eth: Contract,
    mpc: Contract,
    vault: Contract,
}

fn implicit(n: u8) -> String {
    hex::encode([n; 32])
}

async fn sub(worker: &Worker<Sandbox>, name: &str) -> Result<Account> {
    Ok(worker
        .root_account()?
        .create_subaccount(name)
        .initial_balance(NearToken::from_near(20))
        .transact()
        .await?
        .into_result()?)
}

async fn setup_with(config: Value) -> Result<Env> {
    let worker = near_workspaces::sandbox().await?;
    let vault_wasm = near_workspaces::compile_project("./").await?;
    let fixture_wasm = near_workspaces::compile_project("../sandbox-fixtures").await?;

    let owner = sub(&worker, "owner").await?;
    let keeper = sub(&worker, "keeper").await?;
    let alice = sub(&worker, "alice").await?;
    let mallory = sub(&worker, "mallory").await?;

    let usdc = sub(&worker, "usdc").await?.deploy(&fixture_wasm).await?.into_result()?;
    usdc.call("new").args_json(json!({"decimals": 6})).transact().await?.into_result()?;
    let eth = sub(&worker, "eth").await?.deploy(&fixture_wasm).await?.into_result()?;
    eth.call("new").args_json(json!({"decimals": 18})).transact().await?.into_result()?;
    let mpc = sub(&worker, "mpc").await?.deploy(&fixture_wasm).await?.into_result()?;
    mpc.call("new").args_json(json!({"decimals": 0})).transact().await?.into_result()?;

    let vault = sub(&worker, "vault").await?.deploy(&vault_wasm).await?.into_result()?;
    vault
        .call("new")
        .args_json(json!({
            "owner": owner.id(),
            "usdc": usdc.id(),
            "spot_token": eth.id(),
            "mpc": mpc.id(),
            "keepers": [keeper.id()],
            "config": config,
        }))
        .transact()
        .await?
        .into_result()?;

    for (token, who) in [(&usdc, vault.id()), (&eth, vault.id())] {
        token.call("register").args_json(json!({"account_id": who})).transact().await?.into_result()?;
    }
    for who in [alice.id(), keeper.id(), mallory.id()] {
        usdc.call("mint")
            .args_json(json!({"account_id": who, "amount": (100_000 * USDC).to_string()}))
            .transact()
            .await?
            .into_result()?;
    }
    for n in 1..=3u8 {
        for token in [&usdc, &eth] {
            token.call("register").args_json(json!({"account_id": implicit(n)})).transact().await?.into_result()?;
        }
    }
    Ok(Env { worker, owner, keeper, alice, mallory, usdc, eth, mpc, vault })
}

fn default_config() -> Value {
    json!({
        "hedge_band_bps": 100,
        "max_drawdown_bps": 500,
        "epoch_secs": 3600,
        "max_rebalances_per_epoch": 6,
        "max_turnover_bps": 20000,
        "max_allocation_bps": 6000,
        "max_nav_move_bps": 300,
        "max_slippage_bps": 100,
        "min_deposit": "1000000",
        "perp_asset": 1,
        "venue_mainnet": true,
        "mpc_path": PATH,
        "mpc_domain_id": 0,
        "spot_decimals": 18,
        "perp_evm_address": "0x1111111111111111111111111111111111111111",
        "settlement_chain_id": 42161,
        "settlement_usdc": "0xaf88d065e77c8cc2239327c5edb3a432268e5831",
        "venue_bridge": "0x2df1c51e09aecf9cacb7bc98cb1742757f163df7",
        "max_gas_limit": 200000,
        "max_fee_per_gas": "1000000000"
    })
}

async fn setup() -> Result<Env> {
    setup_with(default_config()).await
}

async fn deposit(env: &Env, who: &Account, amount: u128) -> Result<ExecutionFinalResult> {
    Ok(who
        .call(env.usdc.id(), "ft_transfer_call")
        .args_json(json!({"receiver_id": env.vault.id(), "amount": amount.to_string(), "msg": "deposit"}))
        .deposit(NearToken::from_yoctonear(1))
        .max_gas()
        .transact()
        .await?)
}

async fn state(env: &Env) -> Result<Value> {
    Ok(env.vault.view("get_state").args_json(json!({})).await?.json()?)
}

fn u(v: &Value) -> u128 {
    v.as_str().map(|s| s.parse().unwrap()).unwrap_or_else(|| v.as_u64().unwrap() as u128)
}

async fn ft_balance(token: &Contract, who: &str) -> Result<u128> {
    let v: Value = token.view("ft_balance_of").args_json(json!({"account_id": who})).await?.json()?;
    Ok(u(&v))
}

async fn now_ms(env: &Env) -> Result<u64> {
    Ok(env.worker.view_block().await?.timestamp() / 1_000_000)
}

async fn keeper_call(env: &Env, method: &str, args: Value, yocto: u128) -> Result<ExecutionFinalResult> {
    Ok(env
        .keeper
        .call(env.vault.id(), method)
        .args_json(args)
        .deposit(NearToken::from_yoctonear(yocto))
        .max_gas()
        .transact()
        .await?)
}

fn failed_with(r: &ExecutionFinalResult, needle: &str) -> bool {
    !r.is_success() && format!("{:?}", r.failures()).contains(needle)
}

/// Builds a hedged position of `eth` ETH at $2,000: deposit, buy spot (simulated
/// 1Click delivery), sync it from the token, report, then short via the MPC path.
async fn hedge(env: &Env, eth_qty: u128) -> Result<()> {
    let notional = eth_qty * 2_000 * USDC;
    assert!(deposit(env, &env.alice, notional * 2).await?.is_success());
    // Spot leg: USDC to the 1Click deposit address, spot arrives at the vault.
    let r = keeper_call(env, "allocate", json!({"leg": "Spot", "amount": notional.to_string(), "deposit_address": implicit(1)}), 0).await?;
    assert!(r.is_success(), "{:?}", r.failures());
    env.eth
        .call("mint")
        .args_json(json!({"account_id": env.vault.id(), "amount": (eth_qty * E8 * ETH_UNIT).to_string()}))
        .transact()
        .await?
        .into_result()?;
    assert!(env.keeper.call(env.vault.id(), "sync_spot").max_gas().transact().await?.is_success());
    let s = state(env).await?;
    let report = keeper_call(
        env,
        "report",
        json!({"deployed_value": u(&s["deployed"]).to_string(), "perp_short_e8": "0", "mark_px_e8": (2_000 * E8).to_string()}),
        0,
    )
    .await?;
    assert!(report.is_success(), "{:?}", report.failures());
    let t = now_ms(env).await?;
    let r = keeper_call(
        env,
        "request_perp_order",
        json!({"order": {"is_buy": false, "sz_e8": (eth_qty * E8).to_string(), "limit_px_e8": (2_000 * E8).to_string(), "reduce_only": false}, "nonce": t.to_string()}),
        1,
    )
    .await?;
    assert!(r.is_success(), "{:?}", r.failures());
    Ok(())
}

// ---------------------------------------------------------------- I1 shares

#[tokio::test]
async fn i1_shares_deposit_and_withdraw_at_nav() -> Result<()> {
    let env = setup().await?;
    assert!(deposit(&env, &env.alice, 1_000 * USDC).await?.is_success());
    let shares: Value = env.vault.view("shares_of").args_json(json!({"account_id": env.alice.id()})).await?.json()?;
    assert_eq!(u(&shares), 1_000 * USDC);

    let before = ft_balance(&env.usdc, env.alice.id().as_str()).await?;
    let r = env.alice.call(env.vault.id(), "withdraw").args_json(json!({"shares": (400 * USDC).to_string()})).max_gas().transact().await?;
    assert!(r.is_success(), "{:?}", r.failures());
    let after = ft_balance(&env.usdc, env.alice.id().as_str()).await?;
    assert_eq!(after - before, 400 * USDC);
    let s = state(&env).await?;
    assert_eq!(u(&s["total_shares"]), 600 * USDC);
    assert_eq!(u(&s["nav"]), 600 * USDC);
    Ok(())
}

#[tokio::test]
async fn i1_shares_rejects_wrong_token_and_dust() -> Result<()> {
    let env = setup().await?;
    // Below the 1 USDC minimum: refunded by the token's resolve step.
    let before = ft_balance(&env.usdc, env.alice.id().as_str()).await?;
    deposit(&env, &env.alice, 10).await?;
    assert_eq!(ft_balance(&env.usdc, env.alice.id().as_str()).await?, before);
    // A different token calling ft_on_transfer is refused.
    env.eth.call("mint").args_json(json!({"account_id": env.mallory.id(), "amount": "1000000"})).transact().await?.into_result()?;
    let r = env
        .mallory
        .call(env.vault.id(), "ft_on_transfer")
        .args_json(json!({"sender_id": env.mallory.id(), "amount": "1000000", "msg": "deposit"}))
        .transact()
        .await?;
    assert!(failed_with(&r, "only USDC deposits"));
    Ok(())
}

#[tokio::test]
async fn i1_shares_queued_withdrawal_is_claimable_after_refill() -> Result<()> {
    let env = setup().await?;
    assert!(deposit(&env, &env.alice, 1_000 * USDC).await?.is_success());
    let r = keeper_call(&env, "allocate", json!({"leg": "Margin", "amount": (600 * USDC).to_string(), "deposit_address": implicit(2)}), 0).await?;
    assert!(r.is_success(), "{:?}", r.failures());
    // 400 idle, Alice asks for 700: queued, not paid.
    let r = env.alice.call(env.vault.id(), "withdraw").args_json(json!({"shares": (700 * USDC).to_string()})).max_gas().transact().await?;
    assert!(r.is_success());
    let id: u64 = r.json::<Value>()?.as_str().unwrap().parse()?;
    let w: Value = env.vault.view("get_withdrawal").args_json(json!({"id": id})).await?.json()?;
    assert_eq!(u(&w["amount"]), 700 * USDC);
    let early = env.alice.call(env.vault.id(), "claim").args_json(json!({"id": id})).max_gas().transact().await?;
    assert!(failed_with(&early, "vault has not refilled yet"));
    // Keeper brings 600 back.
    let refill = env
        .keeper
        .call(env.usdc.id(), "ft_transfer_call")
        .args_json(json!({"receiver_id": env.vault.id(), "amount": (600 * USDC).to_string(), "msg": "refill"}))
        .deposit(NearToken::from_yoctonear(1))
        .max_gas()
        .transact()
        .await?;
    assert!(refill.is_success());
    let before = ft_balance(&env.usdc, env.alice.id().as_str()).await?;
    let ok = env.mallory.call(env.vault.id(), "claim").args_json(json!({"id": id})).max_gas().transact().await?;
    assert!(ok.is_success(), "{:?}", ok.failures());
    assert_eq!(ft_balance(&env.usdc, env.alice.id().as_str()).await? - before, 700 * USDC);
    Ok(())
}

// ---------------------------------------------------------------- I2 hedge band + MPC

#[tokio::test]
async fn i2_hedge_band_signs_valid_order_through_mpc() -> Result<()> {
    let env = setup().await?;
    hedge(&env, 1).await?;
    let s = state(&env).await?;
    assert_eq!(u(&s["spot_qty_e8"]), E8);
    assert_eq!(u(&s["perp_short_e8"]), E8);
    assert_eq!(s["drift_bps"], 0);

    // The signature came back through the callback and recovers to the vault's derived key.
    let recs: Value = env.vault.view("get_sign_requests").args_json(json!({})).await?.json()?;
    let rec = &recs[0];
    assert_eq!(rec["status"], "Signed");
    let sig: Value = serde_json::from_str(rec["signature"].as_str().unwrap())?;
    let digest = hex::decode(rec["digest"].as_str().unwrap())?;
    let big_r = hex::decode(sig["big_r"]["affine_point"].as_str().unwrap())?;
    let s_bytes = hex::decode(sig["s"]["scalar"].as_str().unwrap())?;
    let signature = Signature::from_scalars(
        <[u8; 32]>::try_from(&big_r[1..])?,
        <[u8; 32]>::try_from(&s_bytes[..])?,
    )?;
    let rec_id = RecoveryId::from_byte(sig["recovery_id"].as_u64().unwrap() as u8).unwrap();
    let recovered = VerifyingKey::recover_from_prehash(&digest, &signature, rec_id)?;
    let expected = *sandbox_fixtures::derived_signing_key(env.vault.id().as_str(), PATH).verifying_key();
    assert_eq!(recovered, expected);
    Ok(())
}

#[tokio::test]
async fn i2_hedge_band_refuses_order_that_breaks_band() -> Result<()> {
    let env = setup().await?;
    hedge(&env, 1).await?;
    let signed_before: u64 = env.mpc.view("signed_count").args_json(json!({})).await?.json()?;
    let t = now_ms(&env).await? + 1;
    // Shorting another 0.05 ETH = 500 bps drift > 100 bps band.
    let r = keeper_call(
        &env,
        "request_perp_order",
        json!({"order": {"is_buy": false, "sz_e8": "5000000", "limit_px_e8": (2_000 * E8).to_string(), "reduce_only": false}, "nonce": t.to_string()}),
        1,
    )
    .await?;
    assert!(failed_with(&r, "order breaks the hedge band"));
    let signed_after: u64 = env.mpc.view("signed_count").args_json(json!({})).await?.json()?;
    assert_eq!(signed_before, signed_after, "no signature may be produced");
    Ok(())
}

#[tokio::test]
async fn i2_paired_reduce_keeps_legs_matched() -> Result<()> {
    let env = setup().await?;
    hedge(&env, 2).await?;
    let t = now_ms(&env).await? + 1;
    let r = keeper_call(
        &env,
        "request_paired_reduce",
        json!({"sz_e8": E8.to_string(), "limit_px_e8": (2_000 * E8).to_string(), "nonce": t.to_string(), "spot_deposit_address": implicit(3)}),
        1,
    )
    .await?;
    assert!(r.is_success(), "{:?}", r.failures());
    let s = state(&env).await?;
    assert_eq!(u(&s["spot_qty_e8"]), E8);
    assert_eq!(u(&s["perp_short_e8"]), E8);
    assert_eq!(ft_balance(&env.eth, &implicit(3)).await?, E8 * ETH_UNIT);
    Ok(())
}

#[tokio::test]
async fn i8_price_sanity_refuses_off_market_limit() -> Result<()> {
    let env = setup().await?;
    hedge(&env, 1).await?;
    let t = now_ms(&env).await? + 1;
    let r = keeper_call(
        &env,
        "request_perp_order",
        json!({"order": {"is_buy": true, "sz_e8": "100000", "limit_px_e8": (2_500 * E8).to_string(), "reduce_only": true}, "nonce": t.to_string()}),
        1,
    )
    .await?;
    assert!(failed_with(&r, "limit price too far from mark"));
    Ok(())
}

#[tokio::test]
async fn i2_failed_signature_rolls_back_position() -> Result<()> {
    let env = setup().await?;
    hedge(&env, 1).await?;
    env.mpc.call("set_fail_sign").args_json(json!({"fail": true})).transact().await?.into_result()?;
    let t = now_ms(&env).await? + 1;
    let _ = keeper_call(
        &env,
        "request_perp_order",
        json!({"order": {"is_buy": true, "sz_e8": "500000", "limit_px_e8": (2_000 * E8).to_string(), "reduce_only": true}, "nonce": t.to_string()}),
        1,
    )
    .await?;
    let s = state(&env).await?;
    assert_eq!(u(&s["perp_short_e8"]), E8, "optimistic update must be undone");
    let recs: Value = env.vault.view("get_sign_requests").args_json(json!({})).await?.json()?;
    assert_eq!(recs[1]["status"], "Failed");
    Ok(())
}

// ---------------------------------------------------------------- I3 drawdown

#[tokio::test]
async fn i3_drawdown_switches_vault_to_unwind_only() -> Result<()> {
    let env = setup().await?;
    hedge(&env, 1).await?;
    let mut nav = u(&state(&env).await?["nav"]);
    // Report losses of 2.9% per step (inside the 3% per-report cap) until the 5% stop trips.
    for _ in 0..2 {
        let s = state(&env).await?;
        let idle = u(&s["idle"]);
        nav = nav * 971 / 1000;
        let deployed = nav - idle;
        let r = keeper_call(&env, "report", json!({"deployed_value": deployed.to_string(), "perp_short_e8": E8.to_string(), "mark_px_e8": (2_000 * E8).to_string()}), 0).await?;
        assert!(r.is_success(), "{:?}", r.failures());
    }
    let s = state(&env).await?;
    assert_eq!(s["mode"], "Unwinding");
    // New shorts are refused, reduce-only buys are still signed.
    let t = now_ms(&env).await? + 1;
    let r = keeper_call(&env, "request_perp_order", json!({"order": {"is_buy": false, "sz_e8": "100000", "limit_px_e8": (2_000 * E8).to_string(), "reduce_only": false}, "nonce": t.to_string()}), 1).await?;
    assert!(failed_with(&r, "unwinding: reduce-only buys only"));
    let r = keeper_call(&env, "request_perp_order", json!({"order": {"is_buy": true, "sz_e8": E8.to_string(), "limit_px_e8": (2_000 * E8).to_string(), "reduce_only": true}, "nonce": (t + 1).to_string()}), 1).await?;
    assert!(r.is_success(), "{:?}", r.failures());
    // Deposits are closed while unwinding.
    let before = ft_balance(&env.usdc, env.alice.id().as_str()).await?;
    deposit(&env, &env.alice, 10 * USDC).await?;
    assert_eq!(ft_balance(&env.usdc, env.alice.id().as_str()).await?, before);
    Ok(())
}

// ---------------------------------------------------------------- I4 keepers

#[tokio::test]
async fn i4_keeper_allowlist_guards_every_risk_method() -> Result<()> {
    let env = setup().await?;
    assert!(deposit(&env, &env.alice, 1_000 * USDC).await?.is_success());
    let calls = [
        ("allocate", json!({"leg": "Spot", "amount": "1000000", "deposit_address": implicit(1)}), 0u128),
        ("report", json!({"deployed_value": "0", "perp_short_e8": "0", "mark_px_e8": "1"}), 0),
        ("request_perp_order", json!({"order": {"is_buy": false, "sz_e8": "1", "limit_px_e8": "1", "reduce_only": false}, "nonce": "1"}), 1),
        ("request_bridge_deposit", json!({"amount": "1", "nonce": "0", "max_priority_fee_per_gas": "1", "max_fee_per_gas": "1", "gas_limit": "21000"}), 1),
        ("request_margin_withdraw", json!({"amount": "1", "time_ms": "1"}), 1),
    ];
    for (m, args, yocto) in calls {
        let r = env.mallory.call(env.vault.id(), m).args_json(args).deposit(NearToken::from_yoctonear(yocto)).max_gas().transact().await?;
        assert!(failed_with(&r, "only keeper"), "{m} must be keeper-only");
    }
    // Owner can add and remove keepers; a keeper cannot add itself or others.
    let r = env.keeper.call(env.vault.id(), "add_keeper").args_json(json!({"account_id": env.mallory.id()})).transact().await?;
    assert!(failed_with(&r, "only owner"));
    env.owner.call(env.vault.id(), "remove_keeper").args_json(json!({"account_id": env.keeper.id()})).transact().await?.into_result()?;
    let is: bool = env.vault.view("is_keeper").args_json(json!({"account_id": env.keeper.id()})).await?.json()?;
    assert!(!is);
    Ok(())
}

// ---------------------------------------------------------------- I5 pause / unwind

#[tokio::test]
async fn i5_pause_stops_new_risk_but_not_withdrawals() -> Result<()> {
    let env = setup().await?;
    assert!(deposit(&env, &env.alice, 1_000 * USDC).await?.is_success());
    env.keeper.call(env.vault.id(), "pause").transact().await?.into_result()?;
    assert_eq!(state(&env).await?["mode"], "Paused");

    let r = keeper_call(&env, "allocate", json!({"leg": "Spot", "amount": "1000000", "deposit_address": implicit(1)}), 0).await?;
    assert!(failed_with(&r, "vault is not active"));
    let before = ft_balance(&env.usdc, env.alice.id().as_str()).await?;
    let r = env.alice.call(env.vault.id(), "withdraw").args_json(json!({"shares": (100 * USDC).to_string()})).max_gas().transact().await?;
    assert!(r.is_success());
    assert_eq!(ft_balance(&env.usdc, env.alice.id().as_str()).await? - before, 100 * USDC);

    let r = env.keeper.call(env.vault.id(), "resume").transact().await?;
    assert!(failed_with(&r, "only owner"));
    env.owner.call(env.vault.id(), "emergency_unwind").transact().await?.into_result()?;
    assert_eq!(state(&env).await?["mode"], "Unwinding");
    env.owner.call(env.vault.id(), "resume").transact().await?.into_result()?;
    assert_eq!(state(&env).await?["mode"], "Active");
    Ok(())
}

// ---------------------------------------------------------------- I6 epoch limits

#[tokio::test]
async fn i6_epoch_limits_cap_rebalances_and_allocation() -> Result<()> {
    let mut cfg = default_config();
    cfg["max_rebalances_per_epoch"] = json!(2);
    let env = setup_with(cfg).await?;
    hedge(&env, 1).await?; // uses 1 of 2 rebalances
    let t = now_ms(&env).await? + 1;
    let small = |buy: bool, n: u64| {
        json!({"order": {"is_buy": buy, "sz_e8": "100000", "limit_px_e8": (2_000 * E8).to_string(), "reduce_only": buy}, "nonce": (t + n).to_string()})
    };
    assert!(keeper_call(&env, "request_perp_order", small(true, 0), 1).await?.is_success());
    let r = keeper_call(&env, "request_perp_order", small(false, 1), 1).await?;
    assert!(failed_with(&r, "epoch rebalance limit"));

    // Allocation cap: 60% of NAV per epoch. hedge() allocated 2,000 of 4,000 already.
    let r = keeper_call(&env, "allocate", json!({"leg": "Margin", "amount": (500 * USDC).to_string(), "deposit_address": implicit(2)}), 0).await?;
    assert!(failed_with(&r, "epoch allocation limit"));
    Ok(())
}

#[tokio::test]
async fn i6_epoch_turnover_cap() -> Result<()> {
    let mut cfg = default_config();
    cfg["max_turnover_bps"] = json!(5000);
    let env = setup_with(cfg).await?;
    hedge(&env, 1).await?; // trades $2,000 = 50% of the $4,000 NAV: the whole epoch budget
    let t = now_ms(&env).await? + 1;
    let r = keeper_call(
        &env,
        "request_perp_order",
        json!({"order": {"is_buy": true, "sz_e8": "100000", "limit_px_e8": (2_000 * E8).to_string(), "reduce_only": true}, "nonce": t.to_string()}),
        1,
    )
    .await?;
    assert!(failed_with(&r, "epoch turnover limit"));
    Ok(())
}

// ---------------------------------------------------------------- I7 NAV sanity

#[tokio::test]
async fn i7_nav_report_cannot_jump() -> Result<()> {
    let env = setup().await?;
    hedge(&env, 1).await?;
    let s = state(&env).await?;
    let deployed = u(&s["deployed"]);
    // +10% in one report is refused: a keeper cannot inflate NAV to drain withdrawals.
    let r = keeper_call(&env, "report", json!({"deployed_value": (deployed + deployed / 10 + 400 * USDC).to_string(), "perp_short_e8": E8.to_string(), "mark_px_e8": (2_000 * E8).to_string()}), 0).await?;
    assert!(failed_with(&r, "nav move exceeds per-report limit"));
    Ok(())
}

// ---------------------------------------------------------------- spot is measured on chain

#[tokio::test]
async fn spot_leg_is_read_from_token_contract() -> Result<()> {
    let env = setup().await?;
    env.eth
        .call("mint")
        .args_json(json!({"account_id": env.vault.id(), "amount": (3 * E8 * ETH_UNIT / 2).to_string()}))
        .transact()
        .await?
        .into_result()?;
    let r = env.mallory.call(env.vault.id(), "sync_spot").max_gas().transact().await?;
    assert!(r.is_success(), "{:?}", r.failures());
    assert_eq!(u(&state(&env).await?["spot_qty_e8"]), 3 * E8 / 2);
    Ok(())
}

#[tokio::test]
async fn margin_withdraw_destination_is_fixed() -> Result<()> {
    let env = setup().await?;
    hedge(&env, 1).await?;
    let t = now_ms(&env).await? + 1;
    let r = keeper_call(&env, "request_margin_withdraw", json!({"amount": (100 * USDC).to_string(), "time_ms": t.to_string()}), 1).await?;
    assert!(r.is_success(), "{:?}", r.failures());
    let recs: Value = env.vault.view("get_sign_requests").args_json(json!({})).await?.json()?;
    let last = recs.as_array().unwrap().last().unwrap();
    let expected = vault_contract::evm::withdraw_digest(
        true,
        &vault_contract::evm::parse_address("0x1111111111111111111111111111111111111111").unwrap(),
        100 * USDC,
        t,
        |b| {
            use sha3::Digest;
            sha3::Keccak256::digest(b).into()
        },
    );
    assert_eq!(last["digest"], hex::encode(expected));
    let _ = &env.owner;
    Ok(())
}
