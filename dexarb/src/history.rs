//! The funding history of the shared underlyings, every venue that publishes it (GMX does not), to
//! data/funding/<venue>__<symbol>.csv (ts_ms, rate_h: fraction / hour, + = longs pay) and data/funding/meta.json.

use crate::http::Http;
use crate::model::*;
use crate::registry::{self, Registry};
use crate::venues::ALL;
use anyhow::Result;

pub async fn run(h: &Http, days: f64, max_assets: usize) -> Result<()> {
    let reg = Registry::load(h, &ALL).await;
    let shared = reg.shared();
    let mut bases: Vec<(&String, &Vec<Instrument>)> = shared.iter().collect();
    bases.sort_by_key(|(b, v)| {
        (
            !["BTC", "ETH", "SOL"].contains(&b.as_str()),
            std::cmp::Reverse(v.len()),
        )
    });
    bases.truncate(max_assets);
    let dir = crate::data_dir().join("funding");
    std::fs::create_dir_all(&dir)?;
    let t = now_ms();
    let from = t - (days * 86_400_000.0) as i64;
    let mut meta = Vec::new();
    let jobs: Vec<Instrument> = bases
        .iter()
        .flat_map(|(_, v)| v.iter().cloned())
        .filter(|i| i.venue != "GMX")
        .collect();
    println!(
        "{} assets, {} instruments, {} days",
        bases.len(),
        jobs.len(),
        days
    );
    let mut failed: Vec<Instrument> = Vec::new();
    let mut pass = 0;
    let mut todo = jobs.clone();
    while !todo.is_empty() && pass < 3 {
        // the second and third passes: the failed ones one at a time (throttled venues)
        let width = if pass == 0 { 6 } else { 1 };
        for c in todo.chunks(width) {
            let r = futures::future::join_all(c.iter().map(|i| async move {
                (
                    i.clone(),
                    registry::venue_of(i).funding_history(h, i, from, t).await,
                )
            }))
            .await;
            for (i, x) in r {
                match x {
                    Ok(v) => {
                        let name = format!(
                            "{}__{}.csv",
                            i.venue,
                            i.symbol.replace(['/', ' ', ':'], "_")
                        );
                        let body: String = std::iter::once("ts_ms,rate_h\n".to_string())
                            .chain(v.iter().map(|p| format!("{},{}\n", p.ts_ms, p.rate_h)))
                            .collect();
                        std::fs::write(dir.join(&name), body)?;
                        let span = v
                            .first()
                            .zip(v.last())
                            .map(|(a, b)| (b.ts_ms - a.ts_ms) as f64 / 3.6e6)
                            .unwrap_or(0.0);
                        println!(
                            "  {:<11} {:<16} {:>5} points over {:>6.0} h",
                            i.venue,
                            i.symbol,
                            v.len(),
                            span
                        );
                        meta.push(serde_json::json!({"venue": i.venue, "symbol": i.symbol, "base": i.base, "interval_h": i.funding_interval_h, "file": name, "points": v.len(), "taker": f(i.fees.taker)}));
                    }
                    Err(e) => {
                        println!(
                            "  {:<11} {:<16} ERROR (pass {}) {}",
                            i.venue,
                            i.symbol,
                            pass + 1,
                            format!("{e:#}").chars().take(120).collect::<String>()
                        );
                        failed.push(i);
                    }
                }
            }
            if pass > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            }
        }
        todo = std::mem::take(&mut failed);
        pass += 1;
    }
    std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
    Ok(())
}

pub fn load(dir: &std::path::Path, file: &str) -> Vec<FundingPoint> {
    std::fs::read_to_string(dir.join(file))
        .unwrap_or_default()
        .lines()
        .skip(1)
        .filter_map(|l| {
            let (a, b) = l.split_once(',')?;
            Some(FundingPoint {
                ts_ms: a.parse().ok()?,
                rate_h: b.parse().ok()?,
            })
        })
        .collect()
}
