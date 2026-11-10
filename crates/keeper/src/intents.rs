//! NEAR Intents 1Click client. The keeper only ever asks for `dry: true` quotes
//! unless live mode is explicitly confirmed.

use anyhow::Result;
use serde::Serialize;

pub const ONECLICK: &str = "https://1click.chaindefuser.com";
/// USDC on NEAR (native, Circle-issued).
pub const USDC_NEAR: &str = "nep141:17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1";
/// Bridged ETH on NEAR: the vault's spot leg.
pub const ETH_NEAR: &str = "nep141:eth.bridge.near";
/// USDC on the perp venue's settlement chain: the margin leg.
pub const USDC_ARB: &str = "nep141:arb-0xaf88d065e77c8cc2239327c5edb3a432268e5831.omft.near";

#[derive(Clone, Debug, Serialize)]
pub struct QuoteSummary {
    pub dry: bool,
    pub origin: String,
    pub destination: String,
    pub amount_in: String,
    pub amount_out: String,
    pub amount_out_formatted: String,
    pub min_amount_out: String,
    pub time_estimate_s: u64,
    pub deposit_address: Option<String>,
    /// "live" when fetched now, "fixture" when the offline sample was used.
    pub source: &'static str,
}

/// Recorded on 2026-11-10 from a real `dry: true` quote: 25 USDC -> bridged ETH.
pub fn fixture_quote() -> QuoteSummary {
    QuoteSummary {
        dry: true,
        origin: USDC_NEAR.into(),
        destination: ETH_NEAR.into(),
        amount_in: "25000000".into(),
        amount_out: "11786381603895690".into(),
        amount_out_formatted: "0.01178638160389569".into(),
        min_amount_out: "11727449695876211".into(),
        time_estimate_s: 20,
        deposit_address: None,
        source: "fixture",
    }
}

pub async fn quote(
    client: &reqwest::Client,
    dry: bool,
    origin: &str,
    destination: &str,
    amount: u128,
    refund_to: &str,
    recipient: &str,
) -> Result<QuoteSummary> {
    let deadline_s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs() + 600;
    let body = serde_json::json!({
        "dry": dry,
        "swapType": "EXACT_INPUT",
        "slippageTolerance": 50,
        "originAsset": origin,
        "depositType": "ORIGIN_CHAIN",
        "destinationAsset": destination,
        "amount": amount.to_string(),
        "refundTo": refund_to,
        "refundType": "ORIGIN_CHAIN",
        "recipient": recipient,
        "recipientType": "DESTINATION_CHAIN",
        "deadline": rfc3339(deadline_s),
    });
    let v: serde_json::Value =
        client.post(format!("{ONECLICK}/v0/quote")).json(&body).send().await?.error_for_status()?.json().await?;
    let q = &v["quote"];
    let s = |k: &str| q[k].as_str().unwrap_or_default().to_string();
    Ok(QuoteSummary {
        dry,
        origin: origin.into(),
        destination: destination.into(),
        amount_in: s("amountIn"),
        amount_out: s("amountOut"),
        amount_out_formatted: s("amountOutFormatted"),
        min_amount_out: s("minAmountOut"),
        time_estimate_s: q["timeEstimate"].as_u64().unwrap_or(0),
        deposit_address: q["depositAddress"].as_str().map(String::from),
        source: "live",
    })
}

fn rfc3339(unix_s: u64) -> String {
    let days = unix_s / 86_400;
    let rem = unix_s % 86_400;
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    #[test]
    fn rfc3339_formats() {
        assert_eq!(super::rfc3339(1_793_491_200), "2026-11-01T00:00:00Z");
    }
}
