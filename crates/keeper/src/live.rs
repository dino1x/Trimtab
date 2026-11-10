//! Live keeper loop (testnet or mainnet). Off by default.
//!
//! Mainnet needs an explicit opt-in: `TRIMTAB_NETWORK=mainnet`, a notional cap
//! of at most 100 USD, and `TRIMTAB_MAINNET_CONFIRM` set to the exact phrase
//! returned by [`confirm_phrase`]. Without all three the loop refuses to start.
//!
//! Transactions are signed by `near` (near-cli-rs) with the keeper key from the
//! system keychain, so this process never holds a NEAR private key.

use crate::mpc;
use crate::strategy::{decide, Book, Policy};
use crate::venue::{order_action_json, Hyperliquid, PerpVenue};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::process::Command;

pub const HARD_CAP_USD: f64 = 100.0;

pub fn confirm_phrase(cap_usd: f64) -> String {
    format!("I accept real-money risk up to {cap_usd:.0} USD")
}

#[derive(Clone, Debug)]
pub struct LiveConfig {
    pub network: String,
    pub rpc: String,
    pub vault: String,
    pub keeper: String,
    pub cap_usd: f64,
    pub coin: String,
    pub execute: bool,
}

impl LiveConfig {
    pub fn from_env() -> Result<Self> {
        let get = |k: &str| std::env::var(k).with_context(|| format!("{k} is not set"));
        let network = get("TRIMTAB_NETWORK")?;
        let cap_usd: f64 = std::env::var("TRIMTAB_MAX_NOTIONAL_USD").unwrap_or_else(|_| "50".into()).parse()?;
        let confirm = std::env::var("TRIMTAB_MAINNET_CONFIRM").ok();
        check_opt_in(&network, cap_usd, confirm.as_deref())?;
        let rpc = std::env::var("NEAR_RPC").unwrap_or_else(|_| format!("https://rpc.{network}.near.org"));
        Ok(Self {
            network,
            rpc,
            vault: get("TRIMTAB_VAULT")?,
            keeper: get("TRIMTAB_KEEPER")?,
            cap_usd,
            coin: "ETH".into(),
            execute: std::env::var("TRIMTAB_EXECUTE").as_deref() == Ok("1"),
        })
    }
}

pub fn check_opt_in(network: &str, cap_usd: f64, confirm: Option<&str>) -> Result<()> {
    match network {
        "testnet" => Ok(()),
        "mainnet" => {
            if !(cap_usd > 0.0 && cap_usd <= HARD_CAP_USD) {
                bail!("TRIMTAB_MAX_NOTIONAL_USD must be in (0, {HARD_CAP_USD}] on mainnet");
            }
            if confirm != Some(confirm_phrase(cap_usd).as_str()) {
                bail!("mainnet refused: set TRIMTAB_MAINNET_CONFIRM=\"{}\"", confirm_phrase(cap_usd));
            }
            Ok(())
        }
        other => bail!("unknown network {other}"),
    }
}

async fn view(cfg: &LiveConfig, http: &reqwest::Client, method: &str, args: Value) -> Result<Value> {
    use base64_lite::encode;
    let body = json!({"jsonrpc": "2.0", "id": "trimtab", "method": "query", "params": {
        "request_type": "call_function", "finality": "final", "account_id": cfg.vault,
        "method_name": method, "args_base64": encode(args.to_string().as_bytes())}});
    let v: Value = http.post(&cfg.rpc).json(&body).send().await?.json().await?;
    let bytes: Vec<u8> = serde_json::from_value(v["result"]["result"].clone()).context("rpc view failed")?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Calls a vault method through near-cli-rs. Prints the command instead of
/// running it unless `TRIMTAB_EXECUTE=1`.
fn call(cfg: &LiveConfig, method: &str, args: Value, deposit_yocto: u128) -> Result<()> {
    let deposit = format!("{deposit_yocto} yoctoNEAR");
    let a = args.to_string();
    let argv = [
        "contract", "call-function", "as-transaction", &cfg.vault, method, "json-args", &a,
        "prepaid-gas", "300 Tgas", "attached-deposit", &deposit,
        "sign-as", &cfg.keeper, "network-config", &cfg.network, "sign-with-keychain", "send",
    ];
    if !cfg.execute {
        println!("[dry-run] near {}", argv.map(|s| if s.contains(' ') || s.contains('{') { format!("'{s}'") } else { s.to_string() }).join(" "));
        return Ok(());
    }
    let st = Command::new("near").args(argv).status().context("running near CLI")?;
    if !st.success() {
        bail!("near {method} failed");
    }
    Ok(())
}

/// One keeper cycle: read chain + venue, report, and request a rebalance if the policy says so.
pub async fn tick(cfg: &LiveConfig, policy: &Policy) -> Result<()> {
    let http = reqwest::Client::new();
    let venue = if cfg.network == "mainnet" { Hyperliquid::mainnet() } else { Hyperliquid::new("https://api.hyperliquid-testnet.xyz") };
    let state = view(cfg, &http, "get_state", json!({})).await?;
    let config = view(cfg, &http, "get_config", json!({})).await?;
    let evm = config["perp_evm_address"].as_str().context("perp_evm_address")?;
    let market = venue.market(&cfg.coin).await?;
    let pos = venue.position(evm, &cfg.coin).await?;
    let num = |v: &Value| v.as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let spot_qty = num(&state["spot_qty_e8"]) / 1e8;
    let idle = (num(&state["idle"]) - num(&state["owed"])) / 1e6;

    let deployed = spot_qty * market.oracle_px + pos.margin_equity;
    call(cfg, "sync_spot", json!({}), 0)?;
    call(cfg, "report", json!({
        "deployed_value": format!("{:.0}", deployed * 1e6),
        "perp_short_e8": format!("{:.0}", pos.short_qty * 1e8),
        "mark_px_e8": format!("{:.0}", market.mark_px * 1e8),
    }), 0)?;

    let book = Book { spot_qty, short_qty: pos.short_qty, margin_equity: pos.margin_equity, idle, px: market.oracle_px };
    let Some(target) = decide(policy, &book) else {
        println!("no rebalance: drift {:.1} bps, leverage {:.2}", book.drift_bps(), book.leverage());
        return Ok(());
    };
    // Cap every step to the opt-in notional.
    let max_qty = cfg.cap_usd / market.mark_px;
    let dq = (target.qty - pos.short_qty).clamp(-max_qty, max_qty);
    let sz_e8 = ((dq.abs() / policy.lot).floor() * policy.lot * 1e8).round() as u128;
    if sz_e8 == 0 {
        return Ok(());
    }
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
    let px = if dq > 0.0 { market.mark_px * 0.995 } else { market.mark_px * 1.005 };
    let px_e8 = (px * 1e8).round() as u128 / 1_000_000 * 1_000_000; // whole cents
    let order = json!({"is_buy": dq < 0.0, "sz_e8": sz_e8.to_string(), "limit_px_e8": px_e8.to_string(), "reduce_only": dq < 0.0});
    println!("rebalance ({:?}): {order}", target.reason);
    call(cfg, "request_perp_order", json!({"order": order, "nonce": nonce.to_string()}), 1)?;
    if cfg.execute {
        submit_last_signed(cfg, &http, &venue).await?;
    }
    Ok(())
}

/// Posts the newest signed order to the venue after checking the signature
/// recovers to the vault's derived key.
async fn submit_last_signed(cfg: &LiveConfig, http: &reqwest::Client, venue: &Hyperliquid) -> Result<()> {
    let n = view(cfg, http, "get_state", json!({})).await?["sign_requests"].as_u64().unwrap_or(0);
    let recs = view(cfg, http, "get_sign_requests", json!({"from_index": n.saturating_sub(1)})).await?;
    let rec = &recs[0];
    let config = view(cfg, http, "get_config", json!({})).await?;
    let root_view: Value = {
        let mpc_id = view(cfg, http, "get_state", json!({})).await?["mpc"].as_str().unwrap_or_default().to_string();
        let body = json!({"jsonrpc": "2.0", "id": "trimtab", "method": "query", "params": {
            "request_type": "call_function", "finality": "final", "account_id": mpc_id,
            "method_name": "public_key", "args_base64": base64_lite::encode(b"{}")}});
        let v: Value = http.post(&cfg.rpc).json(&body).send().await?.json().await?;
        let bytes: Vec<u8> = serde_json::from_value(v["result"]["result"].clone())?;
        serde_json::from_slice(&bytes)?
    };
    let root = mpc::parse_near_secp_key(root_view.as_str().context("mpc public key")?)?;
    let derived = mpc::derive_public_key(&root, &cfg.vault, config["mpc_path"].as_str().unwrap_or_default())?;
    let sig_json: Value = serde_json::from_str(rec["signature"].as_str().context("not signed yet")?)?;
    let digest: [u8; 32] = hex::decode(rec["digest"].as_str().unwrap_or_default())?.try_into().map_err(|_| anyhow::anyhow!("digest"))?;
    let sig = mpc::verify_response(&sig_json, &digest, &derived)?;
    let o = &rec["kind"]["PerpOrder"];
    let e8 = |v: &Value| v.as_str().unwrap_or("0").parse::<u128>().unwrap_or(0);
    let wire = vault_contract::hl::OrderWire {
        asset: config["perp_asset"].as_u64().unwrap_or(1) as u32,
        is_buy: o["order"]["is_buy"].as_bool().unwrap_or(false),
        px: vault_contract::hl::e8_to_wire(e8(&o["order"]["limit_px_e8"])),
        sz: vault_contract::hl::e8_to_wire(e8(&o["order"]["sz_e8"])),
        reduce_only: o["order"]["reduce_only"].as_bool().unwrap_or(false),
        tif: vault_contract::hl::Tif::Ioc,
    };
    let resp = venue.submit_order(order_action_json(&wire), e8(&o["nonce"]) as u64, &sig).await?;
    println!("venue response: {resp}");
    Ok(())
}

/// Tiny base64 encoder so the RPC call needs no extra dependency.
mod base64_lite {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(b: &[u8]) -> String {
        let mut s = String::new();
        for c in b.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= c.len() {
                    s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mainnet_requires_exact_phrase_and_cap() {
        assert!(check_opt_in("testnet", 1_000.0, None).is_ok());
        assert!(check_opt_in("mainnet", 50.0, None).is_err());
        assert!(check_opt_in("mainnet", 50.0, Some("yes")).is_err());
        assert!(check_opt_in("mainnet", 500.0, Some(&confirm_phrase(500.0))).is_err());
        assert!(check_opt_in("mainnet", 50.0, Some(&confirm_phrase(50.0))).is_ok());
    }

    #[test]
    fn base64() {
        assert_eq!(base64_lite::encode(b"{}"), "e30=");
        assert_eq!(base64_lite::encode(b"abc"), "YWJj");
        assert_eq!(base64_lite::encode(b"ab"), "YWI=");
    }
}
