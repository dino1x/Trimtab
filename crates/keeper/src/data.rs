//! Historical market data for the backtest.
//!
//! Both files are real public data, fetched by `trimtab-keeper fetch-history`
//! and committed so the backtest is reproducible offline:
//! - `data/ETH-funding-1h.csv`: the perp venue's hourly funding rate and premium
//!   (`POST https://api.hyperliquid.xyz/info {"type":"fundingHistory"}`)
//! - `data/ETH-spot-1h.csv`: hourly ETH/USDT spot candles
//!   (`GET https://api.binance.com/api/v3/klines?interval=1h`)

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::Path;

/// Fixed window so every run sees the same data: 2025-11-01 .. 2026-11-01 UTC.
pub const WINDOW_START_MS: u64 = 1_761_955_200_000;
pub const WINDOW_END_MS: u64 = 1_793_491_200_000;
pub const HOUR_MS: u64 = 3_600_000;

pub const FUNDING_CSV: &str = include_str!("../../../data/ETH-funding-1h.csv");
pub const SPOT_CSV: &str = include_str!("../../../data/ETH-spot-1h.csv");

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Hour {
    /// Start of the hour, unix ms.
    pub t: u64,
    /// Funding rate paid by longs to shorts for this hour (negative: shorts pay).
    pub funding: f64,
    /// Perp premium over the oracle at funding time.
    pub premium: f64,
    /// Spot close for the hour.
    pub px: f64,
}

/// Joins the two CSVs on the hour. Hours missing from either side are skipped
/// and counted, never filled in.
pub fn load_embedded() -> Result<(Vec<Hour>, usize)> {
    join(FUNDING_CSV, SPOT_CSV)
}

pub fn join(funding_csv: &str, spot_csv: &str) -> Result<(Vec<Hour>, usize)> {
    use std::collections::BTreeMap;
    let mut spot = BTreeMap::new();
    for line in spot_csv.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 3 {
            bail!("bad spot row: {line}");
        }
        let t: u64 = f[0].parse()?;
        let close: f64 = f[2].parse()?;
        spot.insert(t / HOUR_MS * HOUR_MS, close);
    }
    let mut out = Vec::new();
    let mut missing = 0;
    for line in funding_csv.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let f: Vec<&str> = line.split(',').collect();
        if f.len() < 3 {
            bail!("bad funding row: {line}");
        }
        let t: u64 = f[0].parse()?;
        let hour = t / HOUR_MS * HOUR_MS;
        // Funding is stamped at the end of the hour it accrues over; price it at that hour's open candle.
        let price_hour = hour.saturating_sub(HOUR_MS);
        match spot.get(&price_hour) {
            Some(px) => out.push(Hour { t: price_hour, funding: f[1].parse()?, premium: f[2].parse()?, px: *px }),
            None => missing += 1,
        }
    }
    out.sort_by_key(|h| h.t);
    out.dedup_by_key(|h| h.t);
    Ok((out, missing))
}

// ------------------------------------------------------------------ fetching

async fn post_json(client: &reqwest::Client, url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    let mut last = None;
    for attempt in 0..5 {
        match client.post(url).json(body).send().await {
            Ok(r) if r.status().is_success() => return Ok(r.json().await?),
            Ok(r) => last = Some(anyhow::anyhow!("HTTP {}", r.status())),
            Err(e) => last = Some(e.into()),
        }
        tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt + 1))).await;
    }
    Err(last.unwrap())
}

async fn get_json(client: &reqwest::Client, url: &str) -> Result<serde_json::Value> {
    let mut last = None;
    for attempt in 0..5 {
        match client.get(url).send().await {
            Ok(r) if r.status().is_success() => return Ok(r.json().await?),
            Ok(r) => last = Some(anyhow::anyhow!("HTTP {}", r.status())),
            Err(e) => last = Some(e.into()),
        }
        tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt + 1))).await;
    }
    Err(last.unwrap())
}

/// Downloads the window's hourly funding and spot candles into `dir`.
pub async fn fetch_history(dir: &Path, coin: &str, spot_symbol: &str) -> Result<(usize, usize)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("trimtab-keeper")
        .build()?;

    let mut funding = String::from("time_ms,funding_rate,premium\n");
    let mut n_funding = 0;
    let mut t = WINDOW_START_MS;
    while t < WINDOW_END_MS {
        let body = serde_json::json!({"type": "fundingHistory", "coin": coin, "startTime": t, "endTime": WINDOW_END_MS});
        let v = post_json(&client, "https://api.hyperliquid.xyz/info", &body).await.context("funding history")?;
        let rows = v.as_array().context("funding response is not an array")?;
        if rows.is_empty() {
            break;
        }
        for r in rows {
            let time = r["time"].as_u64().context("time")?;
            if time >= WINDOW_END_MS {
                continue;
            }
            funding.push_str(&format!(
                "{},{},{}\n",
                time,
                r["fundingRate"].as_str().context("fundingRate")?,
                r["premium"].as_str().context("premium")?
            ));
            n_funding += 1;
        }
        t = rows.last().unwrap()["time"].as_u64().unwrap() + 1;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    let mut spot = String::from("open_time_ms,open,close\n");
    let mut n_spot = 0;
    let mut t = WINDOW_START_MS;
    while t < WINDOW_END_MS {
        let url = format!(
            "https://api.binance.com/api/v3/klines?symbol={spot_symbol}&interval=1h&startTime={t}&endTime={}&limit=1000",
            WINDOW_END_MS - 1
        );
        let v = get_json(&client, &url).await.context("spot klines")?;
        let rows = v.as_array().context("klines response is not an array")?;
        if rows.is_empty() {
            break;
        }
        for r in rows {
            let open_time = r[0].as_u64().context("open time")?;
            spot.push_str(&format!(
                "{},{},{}\n",
                open_time,
                r[1].as_str().context("open")?,
                r[4].as_str().context("close")?
            ));
            n_spot += 1;
        }
        t = rows.last().unwrap()[0].as_u64().unwrap() + HOUR_MS;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(format!("{coin}-funding-1h.csv")), funding)?;
    std::fs::write(dir.join(format!("{coin}-spot-1h.csv")), spot)?;
    Ok((n_funding, n_spot))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_data_covers_the_window() {
        let (hours, missing) = load_embedded().unwrap();
        assert!(hours.len() > 8_500, "only {} joined hours", hours.len());
        assert!(missing < 100, "{missing} hours missing a spot candle");
        assert!(hours.first().unwrap().t >= WINDOW_START_MS - HOUR_MS);
        assert!(hours.last().unwrap().t < WINDOW_END_MS);
        assert!(hours.windows(2).all(|w| w[0].t < w[1].t));
    }

    #[test]
    fn join_skips_unmatched_hours() {
        let f = "time_ms,funding_rate,premium\n7200000,0.0001,0\n10800000,0.0002,0\n";
        let s = "open_time_ms,open,close\n3600000,1,2\n";
        let (h, missing) = join(f, s).unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(missing, 1);
        assert_eq!(h[0].px, 2.0);
    }
}
