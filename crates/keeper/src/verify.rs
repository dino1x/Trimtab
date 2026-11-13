//! `trimtab-keeper verify`: keyless check of the testnet run. Re-reads every
//! transaction listed in `docs/testnet-run.log` from a public archival RPC,
//! checks it succeeded or was refused as recorded, then checks the vault's
//! live state and that every Chain Signatures signature it holds recovers to
//! the vault's derived key under the MPC network's current root key.

use crate::mpc;
use anyhow::{Context, Result};
use serde_json::{json, Value};

pub const RUN_LOG: &str = include_str!("../../../docs/testnet-run.log");
pub const VAULT: &str = "vault.trimtab-hk1113.testnet";
pub const MPC: &str = "v1.signer-prod.testnet";
pub const DEFAULT_RPC: &str = "https://archival-rpc.testnet.fastnear.com";

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub label: String,
    pub signer: String,
    pub target: String,
    pub tx: String,
    pub expect_ok: bool,
}

pub fn parse_log(s: &str) -> Vec<Entry> {
    s.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('|').collect();
            (f.len() == 5 && !f[3].is_empty()).then(|| Entry {
                label: f[0].into(),
                signer: f[1].into(),
                target: f[2].into(),
                tx: f[3].into(),
                expect_ok: f[4] == "ok",
            })
        })
        .collect()
}

/// The panic text a refused call must carry.
pub fn expected_refusal(e: &Entry) -> Option<&'static str> {
    if e.expect_ok {
        return None;
    }
    Some(if e.target.ends_with(".set_config") {
        "destinations are frozen"
    } else if e.target.ends_with(".allocate") {
        "epoch allocation limit"
    } else {
        "order breaks the hedge band"
    })
}

async fn rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> Result<Value> {
    let body = json!({"jsonrpc": "2.0", "id": "verify", "method": method, "params": params});
    let mut last = None;
    for _ in 0..4 {
        match http.post(url).json(&body).send().await {
            Ok(r) => {
                let v: Value = r.json().await?;
                if v.get("error").is_none() {
                    return Ok(v["result"].clone());
                }
                last = Some(anyhow::anyhow!("{}", v["error"]));
            }
            Err(e) => last = Some(e.into()),
        }
        tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    }
    Err(last.unwrap())
}

async fn view(http: &reqwest::Client, url: &str, account: &str, method: &str, args: Value) -> Result<Value> {
    let r = rpc(http, url, "query", json!({
        "request_type": "call_function", "finality": "final", "account_id": account,
        "method_name": method, "args_base64": b64(args.to_string().as_bytes())
    }))
    .await?;
    let bytes: Vec<u8> = serde_json::from_value(r["result"].clone()).context("view result")?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn b64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

/// Returns (passed, total) and prints one line per check.
pub async fn run(url: &str) -> Result<(usize, usize)> {
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).user_agent("trimtab-verify").build()?;
    let mut pass = 0;
    let mut total = 0;
    let mut check = |ok: bool, what: String| {
        total += 1;
        if ok {
            pass += 1;
        }
        println!("{} {what}", if ok { "PASS" } else { "FAIL" });
    };

    for e in parse_log(RUN_LOG) {
        let r = rpc(&http, url, "EXPERIMENTAL_tx_status", json!({"tx_hash": e.tx, "sender_account_id": e.signer, "wait_until": "NONE"})).await;
        let ok = match &r {
            Ok(v) => {
                let status = &v["status"];
                let failed = status.get("Failure").is_some();
                match expected_refusal(&e) {
                    None => !failed,
                    Some(needle) => failed && status.to_string().contains(needle),
                }
            }
            Err(_) => false,
        };
        let what = if e.expect_ok { "succeeded" } else { "refused" };
        check(ok, format!("{what}: {} — https://testnet.nearblocks.io/txns/{}", e.label, e.tx));
    }

    // Live state.
    let cfg = view(&http, url, VAULT, "get_config", json!({})).await?;
    let root_s = view(&http, url, MPC, "public_key", json!({})).await?;
    let root = mpc::parse_near_secp_key(root_s.as_str().context("mpc key")?)?;
    let path = cfg["mpc_path"].as_str().unwrap_or_default();
    let derived = mpc::derive_public_key(&root, VAULT, path)?;
    let addr = format!("0x{}", hex::encode(mpc::eth_address(&derived)));
    check(
        cfg["perp_evm_address"].as_str() == Some(addr.as_str()),
        format!("vault margin address {} is its Chain Signatures address for path '{path}'", addr),
    );
    let st = view(&http, url, VAULT, "get_state", json!({})).await?;
    check(st["mpc"] == MPC, format!("vault signs through the real testnet MPC contract ({MPC})"));
    check(st["owed"] == "0", "no withdrawal is left unpaid (owed = 0)".into());
    let signs = view(&http, url, VAULT, "get_sign_requests", json!({})).await?;
    for rec in signs.as_array().into_iter().flatten() {
        let id = rec["id"].as_u64().unwrap_or(0);
        let ok = (|| -> Result<bool> {
            let sig: Value = serde_json::from_str(rec["signature"].as_str().context("unsigned")?)?;
            let digest: [u8; 32] = hex::decode(rec["digest"].as_str().unwrap_or_default())?
                .try_into()
                .map_err(|_| anyhow::anyhow!("digest length"))?;
            Ok(mpc::verify_response(&sig, &digest, &derived).is_ok())
        })()
        .unwrap_or(false);
        check(ok, format!("MPC signature #{id} recovers to the vault's derived key {addr}"));
    }
    println!("\n{pass}/{total} checks passed against {url}");
    Ok((pass, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_log_parses_and_every_refusal_has_a_reason() {
        let entries = parse_log(RUN_LOG);
        assert!(entries.len() >= 20, "{} entries", entries.len());
        assert!(entries.iter().all(|e| e.tx.len() >= 43 && e.tx.len() <= 44));
        let refused: Vec<_> = entries.iter().filter(|e| !e.expect_ok).collect();
        assert!(refused.len() >= 3);
        assert!(refused.iter().all(|e| e.label.starts_with("REFUSED")));
        assert!(refused.iter().all(|e| expected_refusal(e).is_some()));
    }

    #[test]
    fn b64_matches_standard() {
        assert_eq!(b64(b"{}"), "e30=");
        assert_eq!(b64(b"abcd"), "YWJjZA==");
    }
}
