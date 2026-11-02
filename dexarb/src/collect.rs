//! The forward collector (level D: no venue publishes historical books): every `every` seconds the shared assets' books
//! and funding on every venue - the mid, the best bid / ask, the VWAPs to buy and to sell $1k and $10k, the funding - to
//! data/collect/<day>.jsonl, the exchange's timestamp and ours both kept. Each round also steps the paper engine.

use crate::exec;
use crate::http::Http;
use crate::model::*;
use crate::registry::Registry;
use crate::scanner;
use crate::venues::ALL;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snap {
    pub ts: i64,
    pub asset: String,
    pub venue: String,
    pub symbol: String,
    pub mid: f64,
    pub bid: f64,
    pub ask: f64,
    /// Per unit of the underlying; 0 when the size is not on the book.
    pub buy1k: f64,
    pub sell1k: f64,
    pub buy10k: f64,
    pub sell10k: f64,
    pub rate_h: Option<f64>,
    pub long_cost_h: Option<f64>,
    pub short_cost_h: Option<f64>,
    pub taker: f64,
    pub ts_exchange: Option<i64>,
    pub age_ms: i64,
}

pub fn snaps(qs: &std::collections::BTreeMap<String, Vec<scanner::Quote>>, t: i64) -> Vec<Snap> {
    let mut out = Vec::new();
    for (asset, v) in qs {
        for q in v {
            let m = q.inst.multiplier;
            let px = |buy: bool, usd: f64| {
                exec::walk(&q.book, buy, df(usd))
                    .filter(|x| x.complete)
                    .map_or(0.0, |x| f(x.vwap / m))
            };
            let (bid, ask) = (
                q.book.bids.first().map_or(0.0, |x| f(x.0 / m)),
                q.book.asks.first().map_or(0.0, |x| f(x.0 / m)),
            );
            out.push(Snap {
                ts: t,
                asset: asset.clone(),
                venue: q.inst.venue.clone(),
                symbol: q.inst.symbol.clone(),
                mid: q.mid,
                bid,
                ask,
                buy1k: px(true, 1_000.0),
                sell1k: px(false, 1_000.0),
                buy10k: px(true, 10_000.0),
                sell10k: px(false, 10_000.0),
                rate_h: q.funding.as_ref().map(|x| x.rate_h),
                long_cost_h: q.funding.as_ref().and_then(|x| x.long_cost_h),
                short_cost_h: q.funding.as_ref().and_then(|x| x.short_cost_h),
                taker: f(q.inst.fees.taker),
                ts_exchange: q.book.ts_exchange,
                age_ms: t - q.book.ts_recv,
            });
        }
    }
    out
}

pub async fn run(h: &Http, every: f64, assets: usize, hours: f64) -> Result<()> {
    let dir = crate::data_dir().join("collect");
    std::fs::create_dir_all(&dir)?;
    let start = now_ms();
    let mut reg = Registry::load(h, &ALL).await;
    let mut reg_at = now_ms();
    let mut paper = crate::paper::Paper::load();
    let mut rounds = 0u64;
    loop {
        let t = now_ms();
        if t - reg_at > 3_600_000 {
            reg = Registry::load(h, &ALL).await;
            reg_at = t;
        }
        let qs = scanner::quotes(h, &reg, assets, &["BTC", "ETH", "SOL"]).await;
        let ss = snaps(&qs, t);
        let day = t / 86_400_000;
        let mut fh = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join(format!("{day}.jsonl")))?;
        for s in &ss {
            writeln!(fh, "{}", serde_json::to_string(s)?)?;
        }
        let venues: std::collections::BTreeSet<&str> =
            ss.iter().map(|s| s.venue.as_str()).collect();
        paper.step(&ss, t);
        paper.save();
        rounds += 1;
        println!(
            "[{}] round {rounds}: {} quotes, {} assets, venues {:?}; paper open {}, closed {}, PnL {:+.2} $ ({} s)",
            t / 1000,
            ss.len(),
            qs.len(),
            venues,
            paper.open.len(),
            paper.closed.len(),
            paper.realized(),
            (now_ms() - t) / 1000
        );
        if hours > 0.0 && (now_ms() - start) as f64 >= hours * 3.6e6 {
            return Ok(());
        }
        let left = every * 1000.0 - (now_ms() - t) as f64;
        if left > 0.0 {
            tokio::time::sleep(std::time::Duration::from_millis(left as u64)).await;
        }
    }
}

pub fn load_all() -> Vec<Snap> {
    let dir = crate::data_dir().join("collect");
    let mut out = Vec::new();
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    files.sort();
    for p in files {
        for l in std::fs::read_to_string(p).unwrap_or_default().lines() {
            if let Ok(s) = serde_json::from_str::<Snap>(l) {
                out.push(s);
            }
        }
    }
    out
}
