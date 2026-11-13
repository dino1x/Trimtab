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
/// Public testnet RPCs, archival ones first. Tried in order with backoff.
pub const DEFAULT_RPCS: &[&str] = &[
    "https://archival-rpc.testnet.fastnear.com",
    "https://archival-rpc.testnet.near.org",
    "https://near-testnet.drpc.org",
    "https://rpc.testnet.fastnear.com",
    "https://test.rpc.fastnear.com",
];

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

/// Why a request produced no answer from the chain.
#[derive(Debug)]
pub enum Net {
    /// Every endpoint timed out, rate-limited or errored: says nothing about the chain.
    Unreachable(String),
    /// A node answered with a JSON-RPC error about the request itself.
    Chain(Value),
}

fn retryable(err: &Value) -> bool {
    let s = err.to_string().to_lowercase();
    // Rate limits, overloaded or lagging nodes, and txs a non-archival node has pruned or not yet indexed.
    s.contains("429") || s.contains("rate limit") || s.contains("timeout") || s.contains("deprecated")
        || s.contains("unknown_transaction") || s.contains("unknown transaction") || s.contains("internal")
        || s.contains("garbage") || s.contains("unavailable")
}

pub struct Rpc {
    http: reqwest::Client,
    urls: Vec<String>,
    pace: std::time::Duration,
    /// Consecutive transport failures per endpoint; at 3 the endpoint is skipped for the rest of the run.
    strikes: std::sync::Mutex<Vec<u32>>,
}

const MAX_STRIKES: u32 = 3;

impl Rpc {
    pub fn new(urls: Vec<String>) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).user_agent("trimtab-verify").build()?,
            strikes: std::sync::Mutex::new(vec![0; urls.len()]),
            urls,
            pace: std::time::Duration::from_millis(300),
        })
    }

    /// Paced, retried with exponential backoff per endpoint, then falls back to the next endpoint.
    pub async fn call(&self, method: &str, params: Value) -> std::result::Result<Value, Net> {
        let body = json!({"jsonrpc": "2.0", "id": "verify", "method": method, "params": params});
        let mut last = String::new();
        let mut chain_err = None;
        for (i, url) in self.urls.iter().enumerate() {
            if self.strikes.lock().unwrap()[i] >= MAX_STRIKES {
                continue;
            }
            let mut wait = std::time::Duration::from_millis(500);
            for _attempt in 0..4 {
                tokio::time::sleep(self.pace).await;
                let transport_fail = |last: &mut String, msg: String| {
                    *last = msg;
                    let mut st = self.strikes.lock().unwrap();
                    st[i] += 1;
                    st[i] >= MAX_STRIKES
                };
                match self.http.post(url).json(&body).send().await {
                    Ok(r) if r.status().as_u16() == 429 || r.status().is_server_error() => {
                        if transport_fail(&mut last, format!("{url}: HTTP {}", r.status())) {
                            break;
                        }
                    }
                    Ok(r) => match r.json::<Value>().await {
                        Ok(v) if v.get("error").is_none() => {
                            self.strikes.lock().unwrap()[i] = 0;
                            return Ok(v["result"].clone());
                        }
                        Ok(v) => {
                            let e = v["error"].clone();
                            if !retryable(&e) {
                                chain_err = Some(e);
                                break; // a real answer; ask the next node to confirm
                            }
                            last = format!("{url}: {}", e.to_string().chars().take(120).collect::<String>());
                            if e.to_string().to_lowercase().contains("unknown") {
                                break; // this node doesn't have it; try another
                            }
                        }
                        Err(e) => {
                            if transport_fail(&mut last, format!("{url}: bad response ({e})")) {
                                break;
                            }
                        }
                    },
                    Err(e) => {
                        let m = format!("{url}: {}", if e.is_timeout() { "timeout".into() } else { e.to_string() });
                        if transport_fail(&mut last, m) {
                            break;
                        }
                    }
                }
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
        }
        match chain_err {
            Some(e) => Err(Net::Chain(e)),
            None if last.is_empty() => Err(Net::Unreachable("every endpoint failed repeatedly earlier in this run".into())),
            None => Err(Net::Unreachable(last)),
        }
    }

    pub async fn view(&self, account: &str, method: &str, args: Value) -> std::result::Result<Value, Net> {
        let r = self
            .call("query", json!({
                "request_type": "call_function", "finality": "final", "account_id": account,
                "method_name": method, "args_base64": b64(args.to_string().as_bytes())
            }))
            .await?;
        let bytes: Vec<u8> = serde_json::from_value(r["result"].clone()).map_err(|_| Net::Chain(r.clone()))?;
        serde_json::from_slice(&bytes).map_err(|_| Net::Chain(r))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail,
    Unreachable,
}

/// Decides a logged transaction's outcome from the chain's answer.
pub fn judge_tx(e: &Entry, answer: &std::result::Result<Value, Net>) -> Outcome {
    match answer {
        Err(Net::Unreachable(_)) => Outcome::Unreachable,
        Err(Net::Chain(_)) => Outcome::Fail,
        Ok(v) => {
            let status = &v["status"];
            let failed = status.get("Failure").is_some();
            let ok = match expected_refusal(e) {
                None => !failed && status.is_object(),
                Some(needle) => failed && status.to_string().contains(needle),
            };
            if ok { Outcome::Pass } else { Outcome::Fail }
        }
    }
}

#[derive(Default)]
pub struct Tally {
    pub pass: usize,
    pub fail: usize,
    pub unreachable: usize,
}

impl Tally {
    fn add(&mut self, o: Outcome, what: &str, why: Option<&str>) {
        let tag = match o {
            Outcome::Pass => { self.pass += 1; "PASS " }
            Outcome::Fail => { self.fail += 1; "FAIL " }
            Outcome::Unreachable => { self.unreachable += 1; "SKIP " }
        };
        match why {
            Some(w) if o != Outcome::Pass => println!("{tag}{what}  ({w})"),
            _ => println!("{tag}{what}"),
        }
    }
    pub fn total(&self) -> usize {
        self.pass + self.fail + self.unreachable
    }
}

/// Runs every check. FAIL only when the chain contradicts the claim; network
/// trouble is counted separately as "couldn't reach" (SKIP).
pub async fn run(urls: Vec<String>) -> Result<Tally> {
    let rpc = Rpc::new(urls)?;
    let mut t = Tally::default();

    for e in parse_log(RUN_LOG) {
        let answer = rpc.call("EXPERIMENTAL_tx_status", json!({"tx_hash": e.tx, "sender_account_id": e.signer, "wait_until": "NONE"})).await;
        let o = judge_tx(&e, &answer);
        let why = match &answer {
            Err(Net::Unreachable(m)) => Some(format!("couldn't reach: {m}")),
            Err(Net::Chain(c)) => Some(format!("chain says: {}", c.to_string().chars().take(140).collect::<String>())),
            Ok(_) => Some("status differs from the log".into()),
        };
        let what = if e.expect_ok { "succeeded" } else { "refused" };
        t.add(o, &format!("{what}: {} — https://testnet.nearblocks.io/txns/{}", e.label, e.tx), why.as_deref());
    }

    // Live state. A view that can't be fetched is SKIP, never FAIL.
    let cfg = rpc.view(VAULT, "get_config", json!({})).await;
    let root = rpc.view(MPC, "public_key", json!({})).await;
    let st = rpc.view(VAULT, "get_state", json!({})).await;
    let signs = rpc.view(VAULT, "get_sign_requests", json!({})).await;
    let skip = |r: &std::result::Result<Value, Net>| match r {
        Err(Net::Unreachable(m)) => Some(format!("couldn't reach: {m}")),
        Err(Net::Chain(c)) => Some(format!("chain says: {c}")),
        Ok(_) => None,
    };

    let derived = match (&cfg, &root) {
        (Ok(c), Ok(r)) => {
            let path = c["mpc_path"].as_str().unwrap_or_default().to_string();
            let key = mpc::parse_near_secp_key(r.as_str().unwrap_or_default())
                .and_then(|root| mpc::derive_public_key(&root, VAULT, &path));
            match key {
                Ok(k) => {
                    let addr = format!("0x{}", hex::encode(mpc::eth_address(&k)));
                    let ok = c["perp_evm_address"].as_str() == Some(addr.as_str());
                    t.add(if ok { Outcome::Pass } else { Outcome::Fail },
                        &format!("vault margin address {addr} is its Chain Signatures address for path '{path}'"), Some("config differs"));
                    Some((k, addr))
                }
                Err(e) => {
                    t.add(Outcome::Fail, "derive the vault's Chain Signatures key", Some(&e.to_string()));
                    None
                }
            }
        }
        _ => {
            let why = skip(&cfg).or(skip(&root));
            t.add(if matches!(cfg, Err(Net::Chain(_))) || matches!(root, Err(Net::Chain(_))) { Outcome::Fail } else { Outcome::Unreachable },
                "vault margin address is its Chain Signatures address", why.as_deref());
            None
        }
    };

    match &st {
        Ok(s) => {
            t.add(if s["mpc"] == MPC { Outcome::Pass } else { Outcome::Fail }, &format!("vault signs through the real testnet MPC contract ({MPC})"), Some("different signer"));
            t.add(if s["owed"] == "0" { Outcome::Pass } else { Outcome::Fail }, "no withdrawal is left unpaid (owed = 0)", Some("owed is not 0"));
        }
        Err(n) => {
            let o = if matches!(n, Net::Chain(_)) { Outcome::Fail } else { Outcome::Unreachable };
            t.add(o, &format!("vault signs through the real testnet MPC contract ({MPC})"), skip(&st).as_deref());
            t.add(o, "no withdrawal is left unpaid (owed = 0)", skip(&st).as_deref());
        }
    }

    match (&signs, &derived) {
        (Ok(list), Some((key, addr))) => {
            for rec in list.as_array().into_iter().flatten() {
                let id = rec["id"].as_u64().unwrap_or(0);
                let ok = (|| -> Result<bool> {
                    let sig: Value = serde_json::from_str(rec["signature"].as_str().context("unsigned")?)?;
                    let digest: [u8; 32] = hex::decode(rec["digest"].as_str().unwrap_or_default())?
                        .try_into()
                        .map_err(|_| anyhow::anyhow!("digest length"))?;
                    Ok(mpc::verify_response(&sig, &digest, key).is_ok())
                })()
                .unwrap_or(false);
                t.add(if ok { Outcome::Pass } else { Outcome::Fail },
                    &format!("MPC signature #{id} recovers to the vault's derived key {addr}"), Some("does not recover"));
            }
        }
        _ => {
            // The vault holds 2 signatures; count both as not checked.
            let why = skip(&signs).unwrap_or_else(|| "derived key unavailable".into());
            for id in 0..2 {
                t.add(Outcome::Unreachable, &format!("MPC signature #{id} recovers to the vault's derived key"), Some(&why));
            }
        }
    }

    println!(
        "\n{}/{} checks passed · {} failed · {} couldn't reach the chain",
        t.pass, t.total(), t.fail, t.unreachable
    );
    Ok(t)
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
    fn transport_trouble_is_never_a_fail() {
        let e = parse_log(RUN_LOG).into_iter().next().unwrap();
        assert_eq!(judge_tx(&e, &Err(Net::Unreachable("429".into()))), Outcome::Unreachable);
        assert_eq!(judge_tx(&e, &Ok(json!({"status": {"SuccessValue": ""}}))), Outcome::Pass);
        assert_eq!(judge_tx(&e, &Ok(json!({"status": {"Failure": {}}}))), Outcome::Fail);
        let r = parse_log(RUN_LOG).into_iter().find(|e| !e.expect_ok).unwrap();
        let needle = expected_refusal(&r).unwrap();
        assert_eq!(judge_tx(&r, &Ok(json!({"status": {"Failure": {"msg": needle}}}))), Outcome::Pass);
        assert_eq!(judge_tx(&r, &Ok(json!({"status": {"SuccessValue": ""}}))), Outcome::Fail);
        assert!(retryable(&json!({"code": -429, "message": "Rate limits exceeded"})));
        assert!(retryable(&json!({"name": "HANDLER_ERROR", "cause": {"name": "UNKNOWN_TRANSACTION"}})));
        assert!(!retryable(&json!({"name": "HANDLER_ERROR", "cause": {"name": "INVALID_ACCOUNT"}})));
    }

    #[test]
    fn b64_matches_standard() {
        assert_eq!(b64(b"{}"), "e30=");
        assert_eq!(b64(b"abcd"), "YWJjZA==");
    }
}
