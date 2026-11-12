//! MCP server (stdio, JSON-RPC 2.0, one message per line) so an AI agent can
//! park idle USDC in the vault. It never holds keys and never sends a
//! transaction: `deposit_intent` returns the exact NEAR transaction for the
//! agent's own wallet to sign. In demo mode it can also simulate the deposit.

use crate::api::AppState;
use crate::intents;
use serde_json::{json, Value};

pub const PROTOCOL_VERSION: &str = "2025-06-18";

fn tools() -> Value {
    json!([
        {
            "name": "quote_deposit",
            "description": "Preview a USDC deposit: shares you would get at current NAV/share, and a dry 1Click quote for the spot leg it funds. Nothing moves.",
            "inputSchema": {"type": "object", "properties": {"usdc": {"type": "number", "minimum": 1}}, "required": ["usdc"]}
        },
        {
            "name": "deposit_intent",
            "description": "Build the NEAR transaction that deposits USDC into the vault (ft_transfer_call with msg \"deposit\"). Your wallet signs it; this server holds no keys. With simulate=true, applies it to the demo vault instead.",
            "inputSchema": {"type": "object", "properties": {
                "account": {"type": "string", "description": "NEAR account that deposits"},
                "usdc": {"type": "number", "minimum": 1},
                "simulate": {"type": "boolean", "default": false}
            }, "required": ["account", "usdc"]}
        },
        {
            "name": "position",
            "description": "Vault state: NAV, both legs, net delta, drift vs band, leverage, and the account's shares if given.",
            "inputSchema": {"type": "object", "properties": {"account": {"type": "string"}}}
        },
        {
            "name": "receipt",
            "description": "Daily receipt: what $10,000 earned in the vault on a day vs US Treasury bills, with the benchmark source. Backtest until the vault runs live.",
            "inputSchema": {"type": "object", "properties": {"date": {"type": "string", "description": "YYYY-MM-DD"}}}
        }
    ])
}

fn ok(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn err(id: &Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

fn text(v: Value, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(&v).unwrap()}], "isError": is_error})
}

fn account_ok(a: &str) -> bool {
    !a.is_empty() && a.len() <= 64 && a.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-".contains(c))
}

/// Handles one JSON-RPC message. Returns `None` for notifications.
pub async fn handle(state: &AppState, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg["method"].as_str().unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(json!({}));
    Some(match method {
        "initialize" => ok(&id, json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "trimtab", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Trimtab is a delta-neutral USDC vault on NEAR. Use quote_deposit, then deposit_intent; your wallet signs. Demo data is labelled DEMO or BACKTEST."
        })),
        "ping" => ok(&id, json!({})),
        "tools/list" => ok(&id, json!({"tools": tools()})),
        "tools/call" => {
            let args = params["arguments"].clone();
            match call_tool(state, params["name"].as_str().unwrap_or_default(), &args).await {
                Ok(v) => ok(&id, text(v, false)),
                Err(e) => ok(&id, text(json!({"error": e}), true)),
            }
        }
        _ => err(&id, -32601, "method not found"),
    })
}

async fn call_tool(state: &AppState, name: &str, a: &Value) -> Result<Value, String> {
    let vault = std::env::var("TRIMTAB_VAULT").ok();
    match name {
        "quote_deposit" => {
            let usdc = a["usdc"].as_f64().filter(|v| *v >= 1.0).ok_or("usdc must be a number >= 1")?;
            let snap = state.snapshot();
            let spot_usdc = (usdc * 0.75 * 1e6) as u128;
            let quote = if state.online {
                intents::quote(&state.http, true, intents::USDC_NEAR, intents::ETH_NEAR, spot_usdc, "intents.near", "intents.near")
                    .await
                    .unwrap_or_else(|_| intents::fixture_quote())
            } else {
                intents::fixture_quote()
            };
            Ok(json!({
                "label": snap.label,
                "usdc": usdc,
                "estimated_shares": usdc / snap.price_per_share,
                "price_per_share": snap.price_per_share,
                "split": {"spot_usdc": usdc * 0.75, "margin_usdc": usdc * 0.25},
                "spot_leg_quote": quote,
            }))
        }
        "deposit_intent" => {
            let account = a["account"].as_str().unwrap_or_default().trim().to_lowercase();
            if !account_ok(&account) {
                return Err("account must be a NEAR account id like alice.near".into());
            }
            let usdc = a["usdc"].as_f64().filter(|v| *v >= 1.0).ok_or("usdc must be a number >= 1")?;
            let amount = ((usdc * 1e6).round() as u128).to_string();
            let usdc_contract = intents::USDC_NEAR.trim_start_matches("nep141:");
            let tx = json!({
                "signer_id": account,
                "receiver_id": usdc_contract,
                "actions": [{"FunctionCall": {
                    "method_name": "ft_transfer_call",
                    "args": {"receiver_id": vault.clone().unwrap_or_else(|| "<vault not deployed yet>".into()), "amount": amount, "msg": "deposit"},
                    "gas": "100000000000000",
                    "deposit": "1"
                }}]
            });
            if a["simulate"].as_bool().unwrap_or(false) {
                let minted = state.demo.lock().unwrap().deposit(&account, usdc).map_err(|e| e.to_string())?;
                return Ok(json!({"label": crate::demo::DEMO_LABEL, "simulated": true, "minted_shares": minted as f64 / 1e6, "transaction": tx}));
            }
            Ok(json!({
                "simulated": false,
                "vault_deployed": vault.is_some(),
                "transaction": tx,
                "note": "Sign and send with your own NEAR wallet. Shares mint in the same transaction; withdraw any time with vault.withdraw(shares)."
            }))
        }
        "position" => {
            let snap = state.snapshot();
            let shares = a["account"].as_str().map(|acc| state.demo.lock().unwrap().shares_of(&acc.to_lowercase()));
            Ok(json!({"vault": snap, "account_shares": shares}))
        }
        "receipt" => {
            let r = match a["date"].as_str() {
                Some(d) => crate::receipt::on(&state.report, d),
                None => crate::receipt::latest(&state.report),
            }
            .map_err(|e| e.to_string())?;
            let y = crate::receipt::year(&state.report).map_err(|e| e.to_string())?;
            Ok(json!({"day": r, "year": y}))
        }
        other => Err(format!("unknown tool {other}")),
    }
}

/// Runs the server on stdin/stdout until stdin closes.
pub async fn serve_stdio(state: AppState) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&state, msg).await,
            Err(_) => Some(err(&Value::Null, -32700, "parse error")),
        };
        if let Some(r) = reply {
            out.write_all(format!("{r}\n").as_bytes()).await?;
            out.flush().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st() -> AppState {
        AppState::new(false).unwrap()
    }

    async fn call(s: &AppState, name: &str, args: Value) -> Value {
        let r = handle(s, json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": name, "arguments": args}}))
            .await
            .unwrap();
        let t = r["result"]["content"][0]["text"].as_str().unwrap();
        let mut v: Value = serde_json::from_str(t).unwrap();
        v["_is_error"] = r["result"]["isError"].clone();
        v
    }

    #[tokio::test]
    async fn handshake_and_tool_list() {
        let s = st();
        let init = handle(&s, json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}})).await.unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "trimtab");
        assert!(handle(&s, json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await.is_none());
        let list = handle(&s, json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).await.unwrap();
        let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["quote_deposit", "deposit_intent", "position", "receipt"]);
        let unknown = handle(&s, json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).await.unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn deposit_intent_builds_a_transaction_and_holds_no_keys() {
        let s = st();
        let v = call(&s, "deposit_intent", json!({"account": "agent.near", "usdc": 250})).await;
        assert_eq!(v["_is_error"], false);
        let fc = &v["transaction"]["actions"][0]["FunctionCall"];
        assert_eq!(fc["method_name"], "ft_transfer_call");
        assert_eq!(fc["args"]["amount"], "250000000");
        assert_eq!(fc["args"]["msg"], "deposit");
        assert_eq!(v["simulated"], false);
    }

    #[tokio::test]
    async fn simulated_deposit_shows_up_in_position() {
        let s = st();
        let v = call(&s, "deposit_intent", json!({"account": "agent.near", "usdc": 100, "simulate": true})).await;
        assert!(v["minted_shares"].as_f64().unwrap() > 0.0);
        let p = call(&s, "position", json!({"account": "agent.near"})).await;
        assert!(p["account_shares"].as_f64().unwrap() > 0.0);
        assert_eq!(p["vault"]["mode"], "demo");
    }

    #[tokio::test]
    async fn bad_input_is_an_error_not_a_crash() {
        let s = st();
        assert_eq!(call(&s, "deposit_intent", json!({"account": "<x>", "usdc": 5})).await["_is_error"], true);
        assert_eq!(call(&s, "quote_deposit", json!({"usdc": 0})).await["_is_error"], true);
        assert_eq!(call(&s, "receipt", json!({"date": "1999-01-01"})).await["_is_error"], true);
        let q = call(&s, "quote_deposit", json!({"usdc": 100})).await;
        assert_eq!(q["spot_leg_quote"]["dry"], true);
        let r = call(&s, "receipt", json!({})).await;
        assert!(r["day"]["source"].as_str().unwrap().contains("fiscaldata.treasury.gov"));
    }
}
