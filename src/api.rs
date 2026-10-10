//! Hyperliquid's public info API and the official leaderboard. Requests are paced by their
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
const LEADERBOARD: &str = "https://stats-data.hyperliquid.xyz/Mainnet/leaderboard";

pub fn now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64()
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
pub struct Leader {
    pub address: String,
    pub account_value: f64,
    pub month_volume: f64,
    pub month_pnl: f64,
    /// PnL over the last 7 days.
    pub week_pnl: f64,
    pub name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct CoinInfo {
    pub name: String,
    pub sz_decimals: u32,
}

/// A coin's state on the exchange.
#[derive(Clone, Debug, Default)]
pub struct CoinCtx {
    /// Funding rate (hourly) and mark price.
    pub funding: f64,
    pub mark: f64,
    /// Traded in the last 24 h (USD) and open interest (coins).
    pub day_volume: f64,
    pub open_interest: f64,
}

/// How a position of the account is set up on the exchange.
#[derive(Clone, Debug, Default)]
pub struct PosSetup {
    pub entry: f64,
    /// Leverage setting: isolated or cross, and its value.
    pub isolated: bool,
    pub leverage: f64,
    /// None when the account's margin would never be used up (cross, low leverage).
    pub liq_px: Option<f64>,
}

/// A stop or take-profit order resting on the exchange (trigger order).
#[derive(Clone, Debug)]
pub struct Trigger {
    pub coin: String,
    /// Stop (Stop Market / Stop Limit) rather than take profit.
    pub stop: bool,
    /// Sells (closes a long).
    pub sell: bool,
    pub trigger_px: f64,
    /// None = the whole position (a position TP/SL).
    pub size: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct AccountState {
    /// Perp account value (the leaderboard's accountValue also counts spot and vaults).
    pub account_value: f64,
    /// coin -> signed size (main perp dex).
    pub positions: HashMap<String, f64>,
    pub setups: HashMap<String, PosSetup>,
    /// Exchange time of the snapshot, unix ms.
    pub time_ms: u64,
}

/// Weight budget per minute, spent evenly.
struct Pacer {
    per_weight: Duration,
    next: Instant,
}

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    pacer: Arc<Mutex<Pacer>>,
    used: Arc<AtomicU64>,
}

impl Api {
    pub fn new(weight_per_min: f64) -> Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).user_agent("hl-copytrader").build()?;
        let per_weight = Duration::from_secs_f64(60.0 / weight_per_min.max(1.0));
        Ok(Self { http, pacer: Arc::new(Mutex::new(Pacer { per_weight, next: Instant::now() })), used: Default::default() })
    }

    /// Weight spent since the last call, and how far ahead the requests are booked (s): a
    /// backlog that keeps growing means the budget is too small for what is read.
    pub fn usage(&self) -> (u64, f64) {
        (self.used.swap(0, Ordering::Relaxed), self.backlog())
    }

    /// How far behind the budget is (s): requests queued for that long.
    pub fn backlog(&self) -> f64 {
        self.pacer.try_lock().map(|p| p.next.saturating_duration_since(Instant::now()).as_secs_f64()).unwrap_or(0.0)
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

    async fn info(&self, body: Value, weight: u32) -> Result<Value> {
        let mut backoff = 1.0;
        for _ in 0..6 {
            self.pace(weight).await;
            match self.http.post(INFO).json(&body).send().await {
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
        let v = self.info(json!({"type": "metaAndAssetCtxs"}), 20).await?;
        let mut coins = Vec::new();
        let mut ctx = HashMap::new();
        let universe = v[0]["universe"].as_array().cloned().unwrap_or_default();
        let ctxs = v[1].as_array().cloned().unwrap_or_default();
        for (i, u) in universe.iter().enumerate() {
            let name = u["name"].as_str().unwrap_or("").to_string();
            if let Some(c) = ctxs.get(i) {
                ctx.insert(name.clone(), CoinCtx {
                    funding: num(&c["funding"]), mark: num(&c["markPx"]), day_volume: num(&c["dayNtlVlm"]), open_interest: num(&c["openInterest"]),
                });
            }
            if !u["isDelisted"].as_bool().unwrap_or(false) {
                coins.push(CoinInfo { name, sz_decimals: u["szDecimals"].as_u64().unwrap_or(0) as u32 });
            }
        }
        Ok((coins, ctx))
    }

    pub async fn account(&self, user: &str) -> Result<AccountState> {
        let v = self.info(json!({"type": "clearinghouseState", "user": user}), 2).await?;
        let mut positions = HashMap::new();
        let mut setups = HashMap::new();
        for p in v["assetPositions"].as_array().cloned().unwrap_or_default() {
            let pos = &p["position"];
            let szi = num(&pos["szi"]);
            if szi != 0.0 {
                let coin = pos["coin"].as_str().unwrap_or("").to_string();
                positions.insert(coin.clone(), szi);
                setups.insert(coin, PosSetup {
                    entry: num(&pos["entryPx"]),
                    isolated: pos["leverage"]["type"] == "isolated",
                    leverage: num(&pos["leverage"]["value"]),
                    liq_px: Some(num(&pos["liquidationPx"])).filter(|&x| x > 0.0),
                });
            }
        }
        Ok(AccountState {
            account_value: num(&v["marginSummary"]["accountValue"]),
            positions,
            setups,
            time_ms: v["time"].as_u64().unwrap_or(0),
        })
    }

    /// Its latest fills (up to 2000), perp and spot, raw.
    pub async fn fills(&self, user: &str) -> Result<Vec<Value>> {
        let v = self.info(json!({"type": "userFills", "user": user}), 20).await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    /// Its PnL and account value histories (see `stable::History`).
    pub async fn portfolio(&self, user: &str) -> Result<crate::stable::History> {
        let v = self.info(json!({"type": "portfolio", "user": user}), 20).await?;
        crate::stable::History::from_portfolio(&v).ok_or_else(|| anyhow::anyhow!("portfolio {user}: no perp history"))
    }

    /// Its resting stop and take-profit orders.
    pub async fn triggers(&self, user: &str) -> Result<Vec<Trigger>> {
        let v = self.info(json!({"type": "frontendOpenOrders", "user": user}), 20).await?;
        Ok(v.as_array().cloned().unwrap_or_default().iter().filter(|o| o["isTrigger"].as_bool().unwrap_or(false)).map(|o| {
            let size = num(&o["sz"]);
            Trigger {
                coin: o["coin"].as_str().unwrap_or("").to_string(),
                stop: o["orderType"].as_str().unwrap_or("").starts_with("Stop"),
                sell: o["side"] == "A",
                trigger_px: num(&o["triggerPx"]),
                size: if o["isPositionTpsl"].as_bool().unwrap_or(false) || size <= 0.0 { None } else { Some(size) },
            }
        }).collect())
    }

    /// Sets the exchange clock offset from a few timed requests (the one with the shortest round
    /// trip, its server time taken as at the middle); returns (offset s, round trip s).
    pub async fn sync_clock(&self) -> Result<(f64, f64)> {
        let mut best: Option<(f64, f64)> = None;
        for _ in 0..5 {
            self.pace(2).await;
            let t0 = now();
            let v: Value = self.http.post(INFO).json(&json!({"type": "clearinghouseState", "user": format!("0x{:040}", 0)}))
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

    /// Every leaderboard account that traded perps this month.
    pub async fn leaders(&self) -> Result<Vec<Leader>> {
        let v: Value = self.http.get(LEADERBOARD).send().await?.json().await?;
        let mut out = Vec::new();
        for r in v["leaderboardRows"].as_array().cloned().unwrap_or_default() {
            let (mut month, mut week_pnl) = ((0.0, 0.0), 0.0);
            for w in r["windowPerformances"].as_array().cloned().unwrap_or_default() {
                if w[0] == "month" {
                    month = (num(&w[1]["vlm"]), num(&w[1]["pnl"]));
                } else if w[0] == "week" {
                    week_pnl = num(&w[1]["pnl"]);
                }
            }
            out.push(Leader {
                address: r["ethAddress"].as_str().unwrap_or("").to_lowercase(),
                account_value: num(&r["accountValue"]),
                month_volume: month.0,
                month_pnl: month.1,
                week_pnl,
                name: r["displayName"].as_str().map(str::to_string),
            });
        }
        Ok(out)
    }
}
