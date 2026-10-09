//! Research, nothing traded: when the price reaches a dense level of liquidations, does it run
//! on through it (the liquidations' forced orders push it further: a cascade) or bounce back?
//!
//!   - The positions of the leaderboard's accounts of `MIN_ACCOUNT`+ are read one after
//!     another (`clearinghouseState`: each position's liquidation price), `PAUSE_S` apart: a
//!     pass takes about half an hour.
//!   - Every 5 s, per liquid coin (`MIN_VOLUME`+ a day): the notional to be liquidated summed
//!     by liquidation price in bins `BIN` wide, longs (below the price) and shorts (above)
//!     apart. A bin of `MIN_CLUSTER`+ is a cluster.
//!   - When the mid reaches a cluster (enters its bin from outside), that touch is followed
//!     for `FOLLOW_S`: the move from the touch in the cascade's direction (down through long
//!     liquidations, up through short ones) at 5 / 15 / 60 min, its furthest point and when,
//!     the bounce back from there, and whether the price went through the whole bin. Each one
//!     is an event `liq_touch`.
//!   - Once a day the touches finished that day are summed up (`liq_daily`): per cluster size,
//!     the average move at each horizon against what a round trip costs (taker fees both ways
//!     and some slippage, `COST_BPS`): a cascade trade (in at the touch, with it) earns the
//!     move, a bounce trade (against it) its negative.
//!
//! Prices are the mids of mainnet's books the copies use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;

use crate::api::{Api, now};
use crate::log;
use crate::store::Store;
use crate::ws::{Book, Books};

/// Accounts read: the leaderboard's of this much and more (USD)...
pub const MIN_ACCOUNT: f64 = 250_000.0;
/// ... one at a time with this pause (s) between: ~300 weight a minute; while the API budget
/// is behind by more than `MAX_BACKLOG_S` (the day's selection, the copies' reads) it waits.
const PAUSE_S: f64 = 0.4;
const MAX_BACKLOG_S: f64 = 5.0;
/// Clusters logged: within this far of the price.
const LOG_WITHIN: f64 = 0.1;
/// Coins traded this much a day (USD) and more.
const MIN_VOLUME: f64 = 10_000_000.0;
/// Bin width, as a ratio of price (0.5%).
const BIN: f64 = 0.005;
const MIN_CLUSTER: f64 = 250_000.0;
const FOLLOW_S: f64 = 3600.0;
/// The same cluster is touched again only after this long.
const COOLDOWN_S: f64 = 1800.0;
/// A round trip's cost: 2 x 4.5 bp taker fee + slippage.
const COST_BPS: f64 = 12.0;

/// A position as liquidation: coin, +1 long / -1 short, liquidation price, size.
#[derive(Clone, Debug)]
struct Liq {
    coin: String,
    side: f64,
    px: f64,
    size: f64,
}

/// The positions read, by account.
type Positions = Arc<Mutex<HashMap<String, Vec<Liq>>>>;

fn bin(px: f64) -> i64 {
    (px.ln() / (1.0 + BIN).ln()).floor() as i64
}

fn bin_range(i: i64) -> (f64, f64) {
    ((1.0 + BIN).powi(i as i32), (1.0 + BIN).powi(i as i32 + 1))
}

/// A cluster: (coin, side, bin) -> (notional at the mid, accounts).
type Clusters = HashMap<(String, i8, i64), (f64, usize)>;

fn clusters(pos: &HashMap<String, Vec<Liq>>, mids: &HashMap<String, f64>) -> Clusters {
    let mut out: Clusters = HashMap::new();
    for liqs in pos.values() {
        for l in liqs {
            let Some(&mid) = mids.get(&l.coin) else { continue };
            // Only on its side of the price: a long liquidates below it, a short above.
            if (l.px - mid) * l.side >= 0.0 {
                continue;
            }
            let e = out.entry((l.coin.clone(), l.side as i8, bin(l.px))).or_default();
            e.0 += l.size.abs() * mid;
            e.1 += 1;
        }
    }
    out.retain(|_, v| v.0 >= MIN_CLUSTER);
    out
}

/// A touch being followed.
#[derive(Clone, Debug)]
struct Touch {
    coin: String,
    /// The cluster's side (+1 longs to liquidate below) and the cascade's direction (-1 down).
    side: i8,
    dir: f64,
    lo: f64,
    hi: f64,
    usd: f64,
    accounts: usize,
    at: f64,
    px: f64,
    /// Move in the cascade's direction (bp): furthest and when, and the lowest after it.
    best: f64,
    best_at: f64,
    after_best_low: f64,
    worst: f64,
    at_5: Option<f64>,
    at_15: Option<f64>,
    through: bool,
    oi: f64,
}

impl Touch {
    fn mark(&mut self, mid: f64, t: f64) {
        let m = (mid / self.px - 1.0) * self.dir * 1e4;
        if m > self.best {
            self.best = m;
            self.best_at = t;
            self.after_best_low = m;
        }
        self.after_best_low = self.after_best_low.min(m);
        self.worst = self.worst.min(m);
        if self.at_5.is_none() && t - self.at >= 300.0 {
            self.at_5 = Some(m);
        }
        if self.at_15.is_none() && t - self.at >= 900.0 {
            self.at_15 = Some(m);
        }
        // Through the bin: past its far edge in the cascade's direction.
        if (self.dir < 0.0 && mid < self.lo) || (self.dir > 0.0 && mid > self.hi) {
            self.through = true;
        }
    }

    fn row(&self, end: f64) -> Value {
        let r = |x: f64| (x * 10.0).round() / 10.0;
        json!({"kind": "liq_touch", "coin": self.coin, "side": if self.side > 0 { "longs" } else { "shorts" },
            "cluster_usd": self.usd.round(), "accounts": self.accounts, "bin_lo": self.lo, "bin_hi": self.hi, "touch_px": self.px,
            "touch_at": self.at, "oi_usd": self.oi.round(), "bp_5m": self.at_5.map(r), "bp_15m": self.at_15.map(r), "bp_60m": r(end),
            "best_bp": r(self.best), "best_after_s": (self.best_at - self.at).round(), "bounce_bp": r(self.best - self.after_best_low),
            "worst_bp": r(self.worst), "through": self.through})
    }
}

/// Starts the scanner and the tracker. `accounts`: the leaderboard's accounts to read, largest
/// first (sent again as the leaderboard is read).
pub fn spawn(api: Api, books: Books, accounts: watch::Receiver<Vec<String>>, store: Store) {
    let positions: Positions = Default::default();
    tokio::spawn(scan(api.clone(), accounts, positions.clone()));
    tokio::spawn(track(api, books, positions, store));
}

async fn scan(api: Api, mut accounts: watch::Receiver<Vec<String>>, positions: Positions) {
    loop {
        let list = accounts.borrow_and_update().clone();
        if list.is_empty() {
            if accounts.changed().await.is_err() {
                return;
            }
            continue;
        }
        let started = now();
        let (mut read, mut liqs) = (0, 0);
        for a in &list {
            if let Ok(s) = api.account(a).await {
                let v: Vec<Liq> = s.positions.iter().filter_map(|(c, &size)| {
                    let px = s.setups.get(c)?.liq_px?;
                    Some(Liq { coin: c.clone(), side: size.signum(), px, size })
                }).collect();
                read += 1;
                liqs += v.len();
                let mut p = positions.lock().unwrap();
                if v.is_empty() {
                    p.remove(a);
                } else {
                    p.insert(a.clone(), v);
                }
            }
            tokio::time::sleep(Duration::from_secs_f64(PAUSE_S)).await;
            while api.backlog() > MAX_BACKLOG_S {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
        // Accounts no longer on the list.
        let keep: std::collections::HashSet<&String> = list.iter().collect();
        positions.lock().unwrap().retain(|a, _| keep.contains(a));
        log!("liq: pass of {read} accounts in {:.0} min, {liqs} positions with a liquidation price", (now() - started) / 60.0);
    }
}

async fn track(api: Api, books: Books, positions: Positions, store: Store) {
    let mut liquid: HashMap<String, f64> = HashMap::new();
    let mut liquid_at = 0.0;
    let mut prev: HashMap<String, f64> = HashMap::new();
    let mut last: Clusters = HashMap::new();
    let mut cool: HashMap<(String, i8, i64), f64> = HashMap::new();
    let mut open: Vec<Touch> = Vec::new();
    let mut done: Vec<Value> = Vec::new();
    let mut day = (now() / 86400.0).floor();
    let mut logged = now();
    let mut every = tokio::time::interval(Duration::from_secs(5));
    loop {
        every.tick().await;
        let t = now();
        if t - liquid_at > 3600.0 {
            match api.meta().await {
                Ok((_, ctx)) => {
                    liquid = ctx.into_iter().filter(|(_, c)| c.day_volume >= MIN_VOLUME).map(|(k, c)| (k, c.open_interest * c.mark)).collect();
                    liquid_at = t;
                }
                Err(e) => log!("liq: meta read failed: {e}"),
            }
        }
        let mids: HashMap<String, f64> = {
            let b = books.read().unwrap();
            liquid.keys().filter_map(|c| Some((c.clone(), b.get(c).and_then(Book::mid)?))).collect()
        };
        // Touches: the mid entered a cluster's bin (as of the last tick) from outside.
        for ((coin, side, i), (usd, accounts)) in &last {
            let (Some(&mid), Some(&was)) = (mids.get(coin), prev.get(coin)) else { continue };
            let (lo, hi) = bin_range(*i);
            let entered = if *side > 0 { was > hi && mid <= hi } else { was < lo && mid >= lo };
            let key = (coin.clone(), *side, *i);
            if !entered || cool.get(&key).is_some_and(|&c| t - c < COOLDOWN_S) {
                continue;
            }
            cool.insert(key, t);
            let edge = if *side > 0 { hi } else { lo };
            open.push(Touch {
                coin: coin.clone(), side: *side, dir: -(*side as f64), lo, hi, usd: *usd, accounts: *accounts, at: t, px: edge,
                best: 0.0, best_at: t, after_best_low: 0.0, worst: 0.0, at_5: None, at_15: None, through: false,
                oi: liquid.get(coin).copied().unwrap_or(0.0),
            });
        }
        let mut finished = Vec::new();
        open.retain_mut(|x| {
            let Some(&mid) = mids.get(&x.coin) else { return true };
            x.mark(mid, t);
            if t - x.at >= FOLLOW_S {
                finished.push(x.row((mid / x.px - 1.0) * x.dir * 1e4));
                return false;
            }
            true
        });
        for row in finished {
            store.event(row.clone());
            done.push(row);
        }
        cool.retain(|_, c| t - *c < COOLDOWN_S);
        last = clusters(&positions.lock().unwrap(), &mids);
        if t - logged >= 600.0 {
            logged = t;
            let near = |c: &String, i: i64| mids.get(c).is_some_and(|m| (bin_range(i).0 / m - 1.0).abs() <= LOG_WITHIN);
            let mut big: Vec<_> = last.iter().filter(|((c, _, i), _)| near(c, *i)).collect();
            big.sort_by(|a, b| b.1.0.total_cmp(&a.1.0));
            let top: Vec<String> = big.iter().take(5).map(|((c, side, i), (usd, _))| {
                let (lo, _) = bin_range(*i);
                let dist = mids.get(c).map(|m| (lo / m - 1.0) * 100.0).unwrap_or(0.0);
                format!("{c} {} ${:.1}M at {dist:+.1}%", if *side > 0 { "longs" } else { "shorts" }, usd / 1e6)
            }).collect();
            log!("liq: {} clusters over {} coins ({} within {:.0}%), {} touches followed; biggest near: {}", last.len(), mids.len(), big.len(),
                LOG_WITHIN * 100.0, open.len(), top.join(", "));
        }
        prev = mids;
        let today = (t / 86400.0).floor();
        if today > day {
            let ev = summary(&done, day);
            log!("liq: day summed up: {ev}");
            store.event(ev);
            done.clear();
            day = today;
        }
    }
}

/// The day's touches summed up per cluster size: count, how many went through, the average
/// move (bp, the cascade's way) at 5 / 15 / 60 min and at its furthest, the bounce, and what
/// a cascade trade (in at the touch, out at 15 / 60 min) would net after `COST_BPS`.
fn summary(rows: &[Value], day: f64) -> Value {
    let buckets: [(&str, f64, f64); 3] = [("0.25-1M", 0.0, 1e6), ("1-5M", 1e6, 5e6), ("5M+", 5e6, f64::INFINITY)];
    let mut out = serde_json::Map::new();
    for (name, lo, hi) in buckets {
        let rs: Vec<&Value> = rows.iter().filter(|r| (lo..hi).contains(&r["cluster_usd"].as_f64().unwrap_or(0.0))).collect();
        let n = rs.len();
        let avg = |k: &str| {
            let v: Vec<f64> = rs.iter().filter_map(|r| r[k].as_f64()).collect();
            if v.is_empty() { None } else { Some((v.iter().sum::<f64>() / v.len() as f64 * 10.0).round() / 10.0) }
        };
        let (m15, m60) = (avg("bp_15m"), avg("bp_60m"));
        out.insert(name.into(), json!({
            "touches": n, "through_pct": (rs.iter().filter(|r| r["through"] == true).count() * 100).checked_div(n).unwrap_or(0),
            "bp_5m": avg("bp_5m"), "bp_15m": m15, "bp_60m": m60, "best_bp": avg("best_bp"), "bounce_bp": avg("bounce_bp"),
            "cascade_net_15m": m15.map(|m| m - COST_BPS), "cascade_net_60m": m60.map(|m| m - COST_BPS),
            "bounce_net_15m": m15.map(|m| -m - COST_BPS), "bounce_net_60m": m60.map(|m| -m - COST_BPS),
        }));
    }
    json!({"kind": "liq_daily", "day": day * 86400.0, "touches": rows.len(), "by_size": out})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clusters_sum_by_side_and_bin() {
        let l = |coin: &str, side: f64, px: f64, size: f64| Liq { coin: coin.into(), side, px, size };
        let mut pos = HashMap::new();
        // Longs liquidating at ~90 (two accounts), a short at 110, a long above the price (not a
        // cluster: it would be liquidated already), and one too small.
        pos.insert("a".into(), vec![l("X", 1.0, 90.0, 3000.0), l("X", -1.0, 110.0, -5000.0)]);
        pos.insert("b".into(), vec![l("X", 1.0, 90.1, 2000.0), l("X", 1.0, 120.0, 9000.0), l("X", 1.0, 50.0, 10.0)]);
        let mids = HashMap::from([("X".to_string(), 100.0)]);
        let c = clusters(&pos, &mids);
        assert_eq!(c.len(), 2);
        assert_eq!(c[&("X".to_string(), 1, bin(90.0))], (500_000.0, 2));
        assert_eq!(c[&("X".to_string(), -1, bin(110.0))], (500_000.0, 1));
        let (lo, hi) = bin_range(bin(90.0));
        assert!(lo <= 90.0 && 90.1 < hi && (hi / lo - 1.0 - BIN).abs() < 1e-9);
    }

    #[test]
    fn a_touch_measures_the_cascade() {
        let mut x = Touch { coin: "X".into(), side: 1, dir: -1.0, lo: 99.5, hi: 100.0, usd: 1e6, accounts: 3, at: 0.0, px: 100.0,
            best: 0.0, best_at: 0.0, after_best_low: 0.0, worst: 0.0, at_5: None, at_15: None, through: false, oi: 0.0 };
        x.mark(100.2, 60.0); // against: -20 bp
        x.mark(98.0, 300.0); // down 2%: +200 bp, through the bin
        x.mark(99.0, 900.0); // bounced to +100
        let r = x.row(100.0);
        assert_eq!((r["best_bp"].as_f64(), r["bounce_bp"].as_f64(), r["worst_bp"].as_f64()), (Some(200.0), Some(100.0), Some(-20.0)));
        assert_eq!((r["bp_5m"].as_f64(), r["bp_15m"].as_f64(), r["through"].as_bool()), (Some(200.0), Some(100.0), Some(true)));
        assert_eq!(r["best_after_s"].as_f64(), Some(300.0));
    }
}
