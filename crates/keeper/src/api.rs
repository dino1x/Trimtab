//! HTTP API (axum). Mounted by the web server too, so one process serves both.

use crate::backtest::Report;
use crate::demo::{DemoVault, VaultSnapshot};
use crate::intents;
use crate::venue::{Hyperliquid, PerpVenue};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct AppState {
    pub demo: Arc<Mutex<DemoVault>>,
    pub report: Arc<Report>,
    pub http: reqwest::Client,
    /// Fetch the venue's live funding and 1Click dry quotes. Off in tests and offline runs.
    pub online: bool,
}

impl AppState {
    pub fn new(online: bool) -> anyhow::Result<Self> {
        let report = crate::backtest::default_report()?;
        Ok(Self {
            demo: Arc::new(Mutex::new(DemoVault::new(&report))),
            report: Arc::new(report),
            http: reqwest::Client::builder().timeout(Duration::from_secs(10)).user_agent("trimtab-keeper").build()?,
            online,
        })
    }

    pub fn snapshot(&self) -> VaultSnapshot {
        self.demo.lock().unwrap().snapshot()
    }
}

/// Replays one backtest hour every `every`.
pub fn spawn_ticker(state: AppState, every: Duration) {
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(every);
        loop {
            iv.tick().await;
            state.demo.lock().unwrap().tick();
        }
    });
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/api/state", get(state_h))
        .route("/api/backtest", get(backtest_h))
        .route("/api/market", get(market_h))
        .route("/api/receipt", get(receipt_h))
        .route("/api/quote", get(quote_h))
        .route("/api/demo/deposit", post(deposit_h))
        .route("/api/demo/withdraw", post(withdraw_h))
        .with_state(state)
}

async fn state_h(State(s): State<AppState>) -> Json<VaultSnapshot> {
    Json(s.snapshot())
}

async fn backtest_h(State(s): State<AppState>) -> Json<Report> {
    Json((*s.report).clone())
}

async fn receipt_h(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    match (crate::receipt::latest(&s.report), crate::receipt::year(&s.report)) {
        (Ok(d), Ok(y)) => (StatusCode::OK, Json(json!({"day": d, "year": y}))),
        _ => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "receipt unavailable"}))),
    }
}

async fn market_h(State(s): State<AppState>) -> (StatusCode, Json<Value>) {
    if !s.online {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error": "offline mode"})));
    }
    match Hyperliquid::mainnet().market("ETH").await {
        Ok(m) => (StatusCode::OK, Json(json!({"source": "live", "venue": "hyperliquid", "market": m,
            "funding_apr_pct": m.funding * 8760.0 * 100.0}))),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({"error": e.to_string()}))),
    }
}

async fn quote_h(State(s): State<AppState>) -> Json<Value> {
    if s.online {
        if let Ok(q) = intents::quote(&s.http, true, intents::USDC_NEAR, intents::ETH_NEAR, 25_000_000, "intents.near", "intents.near").await {
            return Json(json!(q));
        }
    }
    Json(json!(intents::fixture_quote()))
}

#[derive(Deserialize)]
struct DemoReq {
    account: String,
    #[serde(default)]
    usd: f64,
    #[serde(default)]
    shares: f64,
}

fn clean(account: &str) -> Result<String, (StatusCode, Json<Value>)> {
    let a = account.trim();
    if a.is_empty() || a.len() > 64 || !a.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "invalid account id"}))));
    }
    Ok(a.to_lowercase())
}

async fn deposit_h(State(s): State<AppState>, Json(r): Json<DemoReq>) -> (StatusCode, Json<Value>) {
    let a = match clean(&r.account) { Ok(a) => a, Err(e) => return e };
    let mut d = s.demo.lock().unwrap();
    match d.deposit(&a, r.usd) {
        Ok(m) => (StatusCode::OK, Json(json!({"label": crate::demo::DEMO_LABEL, "minted_shares": m as f64 / 1e6, "shares": d.shares_of(&a)}))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))),
    }
}

async fn withdraw_h(State(s): State<AppState>, Json(r): Json<DemoReq>) -> (StatusCode, Json<Value>) {
    let a = match clean(&r.account) { Ok(a) => a, Err(e) => return e };
    let mut d = s.demo.lock().unwrap();
    match d.withdraw(&a, r.shares) {
        Ok(out) => (StatusCode::OK, Json(json!({"label": crate::demo::DEMO_LABEL, "paid_usdc": out as f64 / 1e6, "shares": d.shares_of(&a)}))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn call(app: Router, req: Request<Body>) -> (StatusCode, Value) {
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn state_is_labelled_demo() {
        let app = router(AppState::new(false).unwrap());
        let (st, v) = call(app, Request::get("/api/state").body(Body::empty()).unwrap()).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["mode"], "demo");
        assert!(v["label"].as_str().unwrap().starts_with("DEMO"));
    }

    #[tokio::test]
    async fn demo_deposit_validates_input() {
        let app = router(AppState::new(false).unwrap());
        let bad = Request::post("/api/demo/deposit").header("content-type", "application/json")
            .body(Body::from(r#"{"account":"<script>","usd":10}"#)).unwrap();
        assert_eq!(call(app.clone(), bad).await.0, StatusCode::BAD_REQUEST);
        let ok = Request::post("/api/demo/deposit").header("content-type", "application/json")
            .body(Body::from(r#"{"account":"alice.near","usd":100}"#)).unwrap();
        let (st, v) = call(app, ok).await;
        assert_eq!(st, StatusCode::OK);
        assert!(v["shares"].as_f64().unwrap() > 0.0);
    }

    #[tokio::test]
    async fn quote_falls_back_to_labelled_fixture_offline() {
        let app = router(AppState::new(false).unwrap());
        let (_, v) = call(app, Request::get("/api/quote").body(Body::empty()).unwrap()).await;
        assert_eq!(v["source"], "fixture");
        assert_eq!(v["dry"], true);
    }
}
