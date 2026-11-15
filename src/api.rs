//! Hyperliquid's public info API. Requests are paced by their
//! weight (the API allows 1200 weight per minute per IP, shared with anything else running
//! on this machine).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::Mutex;

const INFO: &str = "https://api.hyperliquid.xyz/info";

/// Now (s since the epoch): the system clock; in a test, the test's clock once it is set (`testkit::Clock`).
pub fn now() -> f64 {
    #[cfg(test)]
    if let Some(t) = TEST_NOW.with(|c| c.get()) {
        return t;
    }
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
}

#[cfg(test)]
thread_local! {
    /// A test's clock (its thread's: a `#[tokio::test]` runs its tasks on that thread).
    pub(crate) static TEST_NOW: std::cell::Cell<Option<f64>> = const { std::cell::Cell::new(None) };
}

/// Exchange clock minus ours, microseconds (see `Api::sync_clock`).
static CLOCK_OFFSET_US: AtomicI64 = AtomicI64::new(0);

/// Now on the exchange's clock: lags are measured against its trade timestamps, and this
/// machine's clock can be off by a good part of a second.
pub fn exchange_now() -> f64 {
    now() + CLOCK_OFFSET_US.load(Ordering::Relaxed) as f64 / 1e6
}

pub fn num(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

#[derive(Clone, Debug)]
pub struct CoinInfo {
    pub name: String,
}

/// A coin's state on the exchange.
#[derive(Clone, Debug, Default)]
pub struct CoinCtx {
    /// Funding rate (hourly).
    pub funding: f64,
    /// Traded in the last 24 h (USD).
    pub day_volume: f64,
}

/// Weight budget per minute, spent evenly.
struct Pacer {
    per_weight: Duration,
    next: Instant,
}

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    /// The info endpoint (`INFO`; a test's fake).
    url: String,
    pacer: Arc<Mutex<Pacer>>,
    used: Arc<AtomicU64>,
}

impl Api {
    pub fn new(weight_per_min: f64) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).user_agent("hl-copytrader").build()?;
        let per_weight = Duration::from_secs_f64(60.0 / weight_per_min.max(1.0));
        Ok(Self { http, url: INFO.to_string(), pacer: Arc::new(Mutex::new(Pacer { per_weight, next: Instant::now() })), used: Default::default() })
    }

    async fn pace(&self, weight: u32) {
        self.used.fetch_add(weight as u64, Ordering::Relaxed);
        let wait = {
            let mut p = self.pacer.lock().await;
            let start = p.next.max(Instant::now());
            p.next = start + p.per_weight * weight;
            start.saturating_duration_since(Instant::now())
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }

    /// The API at `base` (a test's fake Hyperliquid), unpaced.
    #[cfg(test)]
    pub(crate) fn at(base: &str) -> Self {
        Api { url: format!("{}/info", base.trim_end_matches('/')), ..Self::new(1e9).unwrap() }
    }

    pub(crate) async fn info(&self, body: Value, weight: u32) -> Result<Value> {
        let mut backoff = 1.0;
        for _ in 0..6 {
            self.pace(weight).await;
            match self.http.post(&self.url).json(&body).send().await {
                Ok(r) if r.status().is_success() => return Ok(r.json().await?),
                Ok(r) if r.status().as_u16() == 429 || r.status().is_server_error() => {}
                Ok(r) => bail!("info {body}: {}", r.status()),
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_secs_f64(backoff)).await;
            backoff *= 2.0;
        }
        bail!("info {body}: still failing")
    }

    /// Active perp coins (main dex) and, per coin, its funding and mark.
    pub async fn meta(&self) -> Result<(Vec<CoinInfo>, HashMap<String, CoinCtx>)> {
        let all = crate::market::contexts(self, "").await?;
        let ctx = all.iter().map(|c| (c.name.clone(), CoinCtx { funding: c.funding, day_volume: c.volume })).collect();
        Ok((all.into_iter().filter(|c| !c.delisted).map(|c| CoinInfo { name: c.name }).collect(), ctx))
    }


    /// The HIP-3 perp dexes' names (the main dex left out).
    pub async fn dexes(&self) -> Result<Vec<String>> {
        let v = self.info(json!({"type": "perpDexs"}), 20).await?;
        Ok(v.as_array().into_iter().flatten().filter_map(|d| d["name"].as_str().map(String::from)).collect())
    }

    /// Every coin's mid on a dex ("" for the main one).
    pub async fn mids(&self, dex: &str) -> Result<HashMap<String, f64>> {
        let body = if dex.is_empty() { json!({"type": "allMids"}) } else { json!({"type": "allMids", "dex": dex}) };
        let v = self.info(body, 2).await?;
        Ok(v.as_object().into_iter().flatten().map(|(k, x)| (k.clone(), num(x))).filter(|(_, m)| *m > 0.0).collect())
    }

    /// A coin's 1-minute closes from `start` (s) on: (the minute, the close).
    pub async fn minute_closes(&self, coin: &str, start: f64) -> Result<Vec<(i64, f64)>> {
        self.bar_closes(coin, 1, start).await
    }

    /// A coin's closes of `bar_min`-minute candles (1, 5, 15 ..) from `start` (s) on: (the bar's index =
    /// its opening minute / `bar_min`, the close).
    pub async fn bar_closes(&self, coin: &str, bar_min: i64, start: f64) -> Result<Vec<(i64, f64)>> {
        let interval = if bar_min == 1440 { "1d".to_string() } else if bar_min % 60 == 0 { format!("{}h", bar_min / 60) } else { format!("{bar_min}m") };
        let v = crate::market::candles(self, coin, &interval, start).await?;
        Ok(v.into_iter().filter(|c| c.c > 0.0).map(|c| (c.t as i64 / 60 / bar_min, c.c)).collect())
    }

    /// A HIP-3 dex's coins and their day volume ($).
    pub async fn dex_volumes(&self, dex: &str) -> Result<HashMap<String, f64>> {
        Ok(crate::market::contexts(self, dex).await?.into_iter().filter(|c| !c.delisted).map(|c| (c.name, c.volume)).collect())
    }

    /// Sets the exchange clock offset from a few timed requests (the one with the shortest round
    /// trip, its server time taken as at the middle); returns (offset s, round trip s).
    pub async fn sync_clock(&self) -> Result<(f64, f64)> {
        let mut best: Option<(f64, f64)> = None;
        for _ in 0..5 {
            self.pace(2).await;
            let t0 = now();
            let v: Value = self.http.post(&self.url).json(&json!({"type": "clearinghouseState", "user": format!("0x{:040}", 0)}))
                .send().await?.json().await?;
            let t1 = now();
            let Some(ms) = v["time"].as_u64() else { bail!("no server time") };
            let (offset, rtt) = (ms as f64 / 1000.0 - (t0 + t1) / 2.0, t1 - t0);
            if best.is_none_or(|b| rtt < b.1) {
                best = Some((offset, rtt));
            }
        }
        let (offset, rtt) = best.unwrap();
        CLOCK_OFFSET_US.store((offset * 1e6) as i64, Ordering::Relaxed);
        Ok((offset, rtt))
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::FakeHl;

    #[tokio::test]
    async fn the_bar_closes_with_the_forming_one_and_the_reads_on_the_fake() {
        let f = FakeHl::start(&[("APA", 2, 10, 100.0), ("xyz:APB", 2, 10, 50.0)]).await;
        let api = f.api();
        let t = (now() / 300.0).floor() * 300.0;
        f.set_candles("APA", "5m", &[(t - 300.0, 1.0, 1.0, 1.0, 10.0, 1.0), (t, 1.0, 1.0, 1.0, 11.0, 1.0), (t - 600.0, 1.0, 1.0, 1.0, 0.0, 1.0)]);
        let v = api.bar_closes("APA", 5, t - 900.0).await.unwrap();
        assert_eq!(v, vec![((t - 300.0) as i64 / 300, 10.0), (t as i64 / 300, 11.0)], "a zero close left out, the forming bar in");
        let (coins, ctx) = api.meta().await.unwrap();
        assert_eq!(coins.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["APA"]);
        assert!(ctx.contains_key("APA"));
        assert_eq!(api.mids("").await.unwrap().get("APA"), Some(&100.0));
        assert_eq!(api.mids("xyz").await.unwrap().get("xyz:APB"), Some(&50.0));
        f.set_volume("xyz:APB", 7e6);
        assert_eq!(api.dex_volumes("xyz").await.unwrap().get("xyz:APB"), Some(&7e6));
    }
}
