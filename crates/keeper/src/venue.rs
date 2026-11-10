//! Perp venue adapters. The vault contract only knows how to hash orders for
//! one wire format today; the keeper talks to venues through this trait so a
//! second venue is an adapter plus a contract hashing module.

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Serialize;

use crate::mpc::EvmSig;

#[derive(Clone, Debug, Default, Serialize)]
pub struct Market {
    pub coin: String,
    /// Current hourly funding rate.
    pub funding: f64,
    pub mark_px: f64,
    pub oracle_px: f64,
    pub open_interest: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Position {
    /// Positive = short size in coins.
    pub short_qty: f64,
    pub entry_px: f64,
    pub margin_equity: f64,
}

#[async_trait]
pub trait PerpVenue: Send + Sync {
    fn name(&self) -> &'static str;
    async fn market(&self, coin: &str) -> Result<Market>;
    async fn position(&self, user: &str, coin: &str) -> Result<Position>;
    /// Posts an order whose digest the vault contract built and the MPC network signed.
    async fn submit_order(&self, action: serde_json::Value, nonce: u64, sig: &EvmSig) -> Result<serde_json::Value>;
}

pub struct Hyperliquid {
    pub base: String,
    client: reqwest::Client,
}

impl Hyperliquid {
    pub fn mainnet() -> Self {
        Self::new("https://api.hyperliquid.xyz")
    }
    pub fn new(base: &str) -> Self {
        Self {
            base: base.into(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .user_agent("trimtab-keeper")
                .build()
                .unwrap(),
        }
    }
    async fn info(&self, body: serde_json::Value) -> Result<serde_json::Value> {
        Ok(self.client.post(format!("{}/info", self.base)).json(&body).send().await?.error_for_status()?.json().await?)
    }
}

fn num(v: &serde_json::Value) -> f64 {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64()).unwrap_or(0.0)
}

#[async_trait]
impl PerpVenue for Hyperliquid {
    fn name(&self) -> &'static str {
        "hyperliquid"
    }

    async fn market(&self, coin: &str) -> Result<Market> {
        let v = self.info(serde_json::json!({"type": "metaAndAssetCtxs"})).await?;
        let universe = v[0]["universe"].as_array().context("universe")?;
        let i = universe.iter().position(|a| a["name"] == coin).context("coin not listed")?;
        let ctx = &v[1][i];
        Ok(Market {
            coin: coin.into(),
            funding: num(&ctx["funding"]),
            mark_px: num(&ctx["markPx"]),
            oracle_px: num(&ctx["oraclePx"]),
            open_interest: num(&ctx["openInterest"]),
        })
    }

    async fn position(&self, user: &str, coin: &str) -> Result<Position> {
        let v = self.info(serde_json::json!({"type": "clearinghouseState", "user": user})).await?;
        let equity = num(&v["marginSummary"]["accountValue"]);
        let mut p = Position { margin_equity: equity, ..Default::default() };
        for ap in v["assetPositions"].as_array().into_iter().flatten() {
            let pos = &ap["position"];
            if pos["coin"] == coin {
                p.short_qty = -num(&pos["szi"]);
                p.entry_px = num(&pos["entryPx"]);
            }
        }
        Ok(p)
    }

    async fn submit_order(&self, action: serde_json::Value, nonce: u64, sig: &EvmSig) -> Result<serde_json::Value> {
        let body = serde_json::json!({
            "action": action,
            "nonce": nonce,
            "signature": {"r": format!("0x{}", hex::encode(sig.r)), "s": format!("0x{}", hex::encode(sig.s)), "v": sig.v},
            "vaultAddress": null,
        });
        Ok(self.client.post(format!("{}/exchange", self.base)).json(&body).send().await?.error_for_status()?.json().await?)
    }
}

/// JSON form of the order action, identical in content to what the contract hashed.
pub fn order_action_json(o: &vault_contract::hl::OrderWire) -> serde_json::Value {
    let tif = match o.tif {
        vault_contract::hl::Tif::Ioc => "Ioc",
        vault_contract::hl::Tif::Gtc => "Gtc",
    };
    serde_json::json!({
        "type": "order",
        "orders": [{"a": o.asset, "b": o.is_buy, "p": o.px, "s": o.sz, "r": o.reduce_only, "t": {"limit": {"tif": tif}}}],
        "grouping": "na",
    })
}
