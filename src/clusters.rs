//! The coin clusters and the crash cap (2026-11-04, the user's call after the robustness run): in a crash the dip
//! buyers (`dip`, `rsidip`) hold at most `CLUSTER_CAP` (1) position in one cluster of coins that move together; outside
//! a crash nothing changes.
//!
//! The clusters: every `REFRESH_DAYS` (7) the main dex's coins with $10M+ a day, their 1-hour log returns over the last
//! `DAYS` (60) days (a coin needs 90% of the hours), the correlation matrix, average-linkage hierarchical clustering cut
//! at the distance 1 - rho = `CUT` (0.45: the members correlate ~0.55+ on average). A coin not in the map is its own
//! cluster (never capped against another).
//! The crash: BTC down `CLUSTER_BTC_DROP_PCT` (2) % or more over the last 60 minutes (the dip loop's minute closes, fresh
//! within 5 minutes).
//!
//! The research (Binance 5m 2021-01 .. 2026-09, the HL-listed coins, the dip V2 + the RSI dip on one deposit at 2% a
//! trade; robust/s_cluster_port.py, cluster_halves.py): no cap +446% max DD 48.0%; 1 a cluster while BTC -2%/h +542%
//! DD 42.8% (5,322 trades vs 5,382), at the same DD +708%; 2025-10-10 -13.3% vs -18.9%; both halves better at the same
//! DD (2021-23 +435% vs +352%, 2024-26 +49% vs +21%); a cap always cut 4-14% of the trades and did less; a cap on bursts
//! of entries did nothing. The bot reads 1-hour returns (the research 5-minute ones).
//!
//! The map in the store's doc `clusters` (the coins' cluster ids and when computed).

use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::json;

use crate::api::{Api, now};
use crate::config::Config;
use crate::log;

const DOC: &str = "clusters";
const DAYS: f64 = 60.0;
const REFRESH_DAYS: f64 = 7.0;
const CUT: f64 = 0.45;
const MIN_VOL: f64 = 10e6;

static MAP: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);
/// BTC's change over the last 60 minutes and when it was read.
static BTC_1H: Mutex<(f64, f64)> = Mutex::new((0.0, 0.0));

/// The strategies the crash cap applies to.
pub(crate) fn capped_kind(kind: &str) -> bool {
    matches!(kind, "dip" | "rsidip")
}

pub(crate) fn set_btc_1h(change: f64) {
    *BTC_1H.lock().unwrap() = (change, now());
}

fn env_f(k: &str, d: f64) -> f64 {
    std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d)
}

/// A crash now: BTC down the threshold over the last hour (a fresh reading).
pub(crate) fn crash() -> bool {
    let (ch, at) = *BTC_1H.lock().unwrap();
    now() - at < 300.0 && ch <= -env_f("CLUSTER_BTC_DROP_PCT", 2.0) / 100.0
}

pub(crate) fn cluster_of(coin: &str) -> Option<u32> {
    MAP.lock().unwrap().as_ref().and_then(|m| m.get(coin).copied())
}

/// The cap: how many positions of the capped kinds one cluster may hold in a crash (0: off).
pub(crate) fn cap() -> usize {
    env_f("CLUSTER_CAP", 1.0) as usize
}

/// Average-linkage clustering of a distance matrix, cut at `cut`: the cluster id of each item.
pub(crate) fn average_linkage(d: &[Vec<f64>], cut: f64) -> Vec<u32> {
    let n = d.len();
    let mut members: Vec<Vec<usize>> = (0..n).map(|i| vec![i]).collect();
    let mut alive: Vec<bool> = vec![true; n];
    // the distance between two clusters = the mean of their members' pairwise distances
    let mut dist: Vec<Vec<f64>> = d.to_vec();
    loop {
        let mut best = (f64::MAX, 0, 0);
        for i in 0..n {
            if !alive[i] { continue; }
            for j in (i + 1)..n {
                if alive[j] && dist[i][j] < best.0 {
                    best = (dist[i][j], i, j);
                }
            }
        }
        if best.0 > cut || best.0 == f64::MAX {
            break;
        }
        let (_, a, b) = best;
        let (na, nb) = (members[a].len() as f64, members[b].len() as f64);
        for k in 0..n {
            if alive[k] && k != a && k != b {
                let v = (dist[a][k] * na + dist[b][k] * nb) / (na + nb);
                dist[a][k] = v;
                dist[k][a] = v;
            }
        }
        let mb = std::mem::take(&mut members[b]);
        members[a].extend(mb);
        alive[b] = false;
    }
    let mut out = vec![0u32; n];
    let mut id = 0u32;
    for (i, m) in members.iter().enumerate() {
        if alive[i] {
            for &k in m {
                out[k] = id;
            }
            id += 1;
        }
    }
    out
}

async fn compute(api: &Api) -> anyhow::Result<HashMap<String, u32>> {
    let (coins, ctx) = api.meta().await?;
    let liquid: Vec<String> = coins.iter().map(|c| c.name.clone()).filter(|c| ctx.get(c).is_some_and(|x| x.day_volume >= MIN_VOL)).collect();
    let start = now() - DAYS * 86_400.0;
    let mut series: Vec<(String, HashMap<i64, f64>)> = Vec::new();
    for c in &liquid {
        match api.bar_closes(c, 60, start).await {
            Ok(v) if v.len() > 10 => {
                let mut r = HashMap::new();
                for w in v.windows(2) {
                    if w[1].0 == w[0].0 + 1 && w[0].1 > 0.0 {
                        r.insert(w[1].0, (w[1].1 / w[0].1).ln());
                    }
                }
                series.push((c.clone(), r));
            }
            Ok(_) => {}
            Err(e) => log!("clusters: {c} not read: {e:#}"),
        }
        // gently: the bot's other loops share the info budget
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    }
    let hours = (DAYS * 24.0) as usize;
    let last = (now() / 3600.0) as i64;
    let first = last - hours as i64;
    let mut names = Vec::new();
    let mut rows: Vec<Vec<f64>> = Vec::new();
    for (c, r) in &series {
        let row: Vec<f64> = (first..last).map(|h| r.get(&h).copied().unwrap_or(0.0)).collect();
        let have = (first..last).filter(|h| r.contains_key(h)).count();
        if have as f64 >= 0.9 * hours as f64 {
            names.push(c.clone());
            rows.push(row);
        }
    }
    let n = rows.len();
    let stat: Vec<(f64, f64)> = rows.iter().map(|x| {
        let m = x.iter().sum::<f64>() / x.len() as f64;
        let sd = (x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
        (m, sd)
    }).collect();
    let mut d = vec![vec![0.0; n]; n];
    for i in 0..n {
        for j in (i + 1)..n {
            let (mi, si) = stat[i];
            let (mj, sj) = stat[j];
            let cov = rows[i].iter().zip(&rows[j]).map(|(a, b)| (a - mi) * (b - mj)).sum::<f64>() / rows[i].len() as f64;
            let rho = if si > 0.0 && sj > 0.0 { cov / (si * sj) } else { 0.0 };
            d[i][j] = (1.0 - rho).clamp(0.0, 2.0);
            d[j][i] = d[i][j];
        }
    }
    let lab = average_linkage(&d, CUT);
    Ok(names.into_iter().zip(lab).collect())
}

/// Loads the saved map and refreshes it every `REFRESH_DAYS` in the background.
pub async fn start(cfg: &Config, api: Api) -> anyhow::Result<()> {
    let saved = crate::store::load_doc(cfg, DOC).await.ok().flatten();
    let mut at = 0.0;
    if let Some(v) = saved {
        at = v["at"].as_f64().unwrap_or(0.0);
        if let Ok(m) = serde_json::from_value::<HashMap<String, u32>>(v["map"].clone()) {
            log!("clusters: {} coins in {} clusters (computed {:.1} days ago); in a crash (BTC -{}%/h) the dip buyers hold {} a cluster",
                m.len(), m.values().collect::<std::collections::HashSet<_>>().len(), (now() - at) / 86_400.0, env_f("CLUSTER_BTC_DROP_PCT", 2.0), cap());
            *MAP.lock().unwrap() = Some(m);
        }
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let mut at = at;
        loop {
            if now() - at >= REFRESH_DAYS * 86_400.0 {
                match compute(&api).await {
                    Ok(m) if m.len() >= 10 => {
                        let k = m.values().collect::<std::collections::HashSet<_>>().len();
                        log!("clusters: {} coins in {k} clusters", m.len());
                        at = now();
                        let _ = crate::store::save_doc(&cfg, DOC, &json!({"at": at, "map": m})).await;
                        *MAP.lock().unwrap() = Some(m);
                    }
                    Ok(m) => log!("clusters: only {} coins read, the map kept", m.len()),
                    Err(e) => log!("clusters: not computed: {e:#}"),
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_average_linkage_groups_the_close_ones() {
        // a, b close; c, d close; the two pairs far
        let d = vec![vec![0.0, 0.1, 0.9, 0.95], vec![0.1, 0.0, 0.92, 0.9], vec![0.9, 0.92, 0.0, 0.2], vec![0.95, 0.9, 0.2, 0.0]];
        let l = average_linkage(&d, 0.45);
        assert_eq!(l[0], l[1]);
        assert_eq!(l[2], l[3]);
        assert_ne!(l[0], l[2]);
    }
}
