//! Research, apart from the copies (its own tables, its own paper accounts): when the price
//! reaches a dense level of liquidations, does it run on through it (the liquidations' forced
//! orders push it further: a cascade) or bounce back?
//!
//!   - The positions of the leaderboard's accounts of `MIN_ACCOUNT`+ are read one after
//!     another (`clearinghouseState`: each position's liquidation price), `PAUSE_S` apart: a
//!     pass takes about half an hour. It waits while the API budget is behind (the copies and
//!     the day's selection go first).
//!   - Every 5 s, per liquid coin (`MIN_VOLUME`+ a day): the notional to be liquidated summed
//!     by liquidation price in bins `BIN` wide, longs (below the price) and shorts (above)
//!     apart. A bin of `MIN_CLUSTER`+ is a cluster.
//!   - When the mid reaches a cluster (enters its bin from outside), that touch is followed
//!     for `FOLLOW_S`: the move from the touch in the cascade's direction (down through long
//!     liquidations, up through short ones) at 5 / 15 / 60 min, its furthest point and when,
//!     the bounce back from there, and whether the price went through the whole bin
//!     (`liq_touches`).
//!   - On every touch, each of `STRATEGIES` trades on paper, on its own account ($1000 to
//!     start): with the cascade or against it (the bounce), out after 15 or 60 min, or at its
//!     stop `SIM_STOP_PCT` away, sized to lose `SIM_RISK_PCT` of its equity there; taker fills
//!     on the book in and out, taker fees (`liq_trades`; equities in `docs`, "liq_sim").
//!   - At midnight UTC, each strategy's day per cluster size (`liq_daily`).
//!
//! Prices are the mids and books of mainnet the copies use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;

use crate::account::walk;
use crate::api::{Api, now};
use crate::config::{Config, TAKER_FEE};
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

    /// Its name, linking its simulated trades to it.
    fn id(&self) -> String {
        format!("{}-{}-{}", self.coin, if self.side > 0 { "longs" } else { "shorts" }, (self.at * 1000.0) as u64)
    }

    /// Its row of `liq_touches` (`end`: the move at the end, bp).
    fn row(&self, end: f64) -> Value {
        let r = |x: f64| (x * 10.0).round() / 10.0;
        json!({"touch": self.id(), "at": iso(self.at), "coin": self.coin, "side": if self.side > 0 { "longs" } else { "shorts" },
            "cluster_usd": self.usd.round(), "accounts": self.accounts, "bin_lo": self.lo, "bin_hi": self.hi, "touch_px": self.px,
            "oi_usd": self.oi.round(), "bp_5m": self.at_5.map(r), "bp_15m": self.at_15.map(r), "bp_60m": r(end),
            "best_bp": r(self.best), "best_after_s": (self.best_at - self.at).round(), "bounce_bp": r(self.best - self.after_best_low),
            "worst_bp": r(self.worst), "through": self.through})
    }
}

/// Starts the scanner and the tracker. `accounts`: the leaderboard's accounts to read, largest
/// first (sent again as the leaderboard is read).
pub fn spawn(api: Api, books: Books, accounts: watch::Receiver<Vec<String>>, store: Store, cfg: Config) {
    let positions: Positions = Default::default();
    tokio::spawn(scan(api.clone(), accounts, positions.clone()));
    tokio::spawn(track(api, books, positions, store, cfg));
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

/// The trades simulated on every touch: name, with the cascade (else against it), held (s).
const STRATEGIES: [(&str, bool, f64); 4] =
    [("cascade_15m", true, 900.0), ("cascade_60m", true, 3600.0), ("bounce_15m", false, 900.0), ("bounce_60m", false, 3600.0)];
/// Each strategy's own paper account: start, risk per trade to its stop, and the stop (%).
const SIM_START: f64 = 1000.0;
const SIM_RISK_PCT: f64 = 2.0;
const SIM_STOP_PCT: f64 = 2.0;
const SIM_DOC: &str = "liq_sim";

/// A simulated trade open.
#[derive(Clone, Debug)]
struct Sim {
    touch: String,
    strategy: usize,
    coin: String,
    cluster_usd: f64,
    dir: f64,
    opened: f64,
    entry: f64,
    size: f64,
    fee: f64,
    stop: f64,
}

/// `ts` (unix s) as an ISO 8601 UTC time, e.g. "2026-10-09T11:19:57.250Z".
pub fn iso(ts: f64) -> String {
    let secs = ts.floor() as i64;
    let ms = ((ts - secs as f64) * 1000.0).round() as i64;
    let (days, sod) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Days to the civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z", sod / 3600, sod / 60 % 60, sod % 60)
}

async fn track(api: Api, books: Books, positions: Positions, store: Store, cfg: Config) {
    let mut liquid: HashMap<String, f64> = HashMap::new();
    let mut liquid_at = 0.0;
    let mut prev: HashMap<String, f64> = HashMap::new();
    let mut last: Clusters = HashMap::new();
    let mut cool: HashMap<(String, i8, i64), f64> = HashMap::new();
    let mut open: Vec<Touch> = Vec::new();
    let mut sims: Vec<Sim> = Vec::new();
    // Each strategy's paper equity, kept across restarts.
    let saved = crate::store::load_doc(&cfg, SIM_DOC).await.ok().flatten().unwrap_or_default();
    let mut equity: Vec<f64> = STRATEGIES.iter().map(|(n, ..)| saved["equity"][n].as_f64().unwrap_or(SIM_START)).collect();
    // The day's closed trades: strategy, cluster size, PnL ($, bp).
    let mut today: Vec<(usize, f64, f64, f64)> = Vec::new();
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
        let book_now: HashMap<String, Book> = {
            let b = books.read().unwrap();
            liquid.keys().filter_map(|c| Some((c.clone(), b.get(c)?.clone()))).collect()
        };
        let mids: HashMap<String, f64> = book_now.iter().filter_map(|(c, b)| Some((c.clone(), b.mid()?))).collect();
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
            let x = Touch {
                coin: coin.clone(), side: *side, dir: -(*side as f64), lo, hi, usd: *usd, accounts: *accounts, at: t, px: edge,
                best: 0.0, best_at: t, after_best_low: 0.0, worst: 0.0, at_5: None, at_15: None, through: false,
                oi: liquid.get(coin).copied().unwrap_or(0.0),
            };
            // Each strategy's trade on it: a taker fill on the book now, sized so its stop loses
            // `SIM_RISK_PCT` of its equity.
            let book = &book_now[coin];
            for (k, (_, cascade, _)) in STRATEGIES.iter().enumerate() {
                let dir = if *cascade { x.dir } else { -x.dir };
                let size = equity[k].max(0.0) * SIM_RISK_PCT / SIM_STOP_PCT / mid;
                let (got, px) = walk(if dir > 0.0 { &book.asks } else { &book.bids }, size);
                if got <= 0.0 {
                    continue;
                }
                sims.push(Sim {
                    touch: x.id(), strategy: k, coin: coin.clone(), cluster_usd: *usd, dir, opened: t, entry: px, size: got,
                    fee: got * px * TAKER_FEE, stop: px * (1.0 - dir * SIM_STOP_PCT / 100.0),
                });
            }
            open.push(x);
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
            store.insert("liq_touches", row);
        }
        // Simulated trades out at their stop or their time: a taker fill on the book.
        let mut closed = false;
        let mut i = 0;
        while i < sims.len() {
            let s = &sims[i];
            let (Some(&mid), Some(book)) = (mids.get(&s.coin), book_now.get(&s.coin)) else {
                i += 1;
                continue;
            };
            let why = if (mid - s.stop) * s.dir <= 0.0 {
                "stop"
            } else if t - s.opened >= STRATEGIES[s.strategy].2 {
                "time"
            } else {
                ""
            };
            let (got, px) = walk(if s.dir > 0.0 { &book.bids } else { &book.asks }, s.size);
            if why.is_empty() || got <= 0.0 {
                i += 1;
                continue;
            }
            let s = sims.swap_remove(i);
            let fee = s.fee + s.size * px * TAKER_FEE;
            let notional = s.size * s.entry;
            let pnl = s.dir * s.size * (px - s.entry) - fee;
            equity[s.strategy] += pnl;
            today.push((s.strategy, s.cluster_usd, pnl, pnl / notional * 1e4));
            store.insert("liq_trades", json!({
                "touch": s.touch, "strategy": STRATEGIES[s.strategy].0, "coin": s.coin, "cluster_usd": s.cluster_usd.round(),
                "dir": s.dir as i32, "opened_at": iso(s.opened), "closed_at": iso(t), "entry_px": s.entry, "exit_px": px,
                "exit_why": why, "notional": notional, "fee": fee, "pnl_usd": pnl, "pnl_bp": pnl / notional * 1e4,
                "equity_after": equity[s.strategy],
            }));
            closed = true;
        }
        if closed {
            let eq: serde_json::Map<String, Value> = STRATEGIES.iter().zip(&equity).map(|((n, ..), e)| (n.to_string(), json!(e))).collect();
            if let Err(e) = crate::store::save_doc(&cfg, SIM_DOC, &json!({"equity": eq})).await {
                log!("liq: simulation not saved: {e:#}");
            }
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
            let eq: Vec<String> = STRATEGIES.iter().zip(&equity).map(|((n, ..), e)| format!("{n} ${e:.0}")).collect();
            log!("liq: {} clusters over {} coins ({} within {:.0}%), {} touches and {} trades open; {}; biggest near: {}", last.len(),
                mids.len(), big.len(), LOG_WITHIN * 100.0, open.len(), sims.len(), eq.join(", "), top.join(", "));
        }
        prev = mids;
        let d = (t / 86400.0).floor();
        if d > day {
            for row in daily(&today, day, &equity) {
                store.insert("liq_daily", row);
            }
            today.clear();
            day = d;
        }
    }
}

/// The day's closed trades per strategy and cluster size (and "all"): count, winners, average
/// PnL (bp of notional), PnL ($), and the strategy's equity at the end of the day.
fn daily(trades: &[(usize, f64, f64, f64)], day: f64, equity: &[f64]) -> Vec<Value> {
    let sizes: [(&str, f64, f64); 4] = [("all", 0.0, f64::INFINITY), ("0.25-1M", 0.0, 1e6), ("1-5M", 1e6, 5e6), ("5M+", 5e6, f64::INFINITY)];
    let date = iso(day * 86400.0)[..10].to_string();
    let mut out = Vec::new();
    for (k, (name, ..)) in STRATEGIES.iter().enumerate() {
        for (size, lo, hi) in sizes {
            let ts: Vec<_> = trades.iter().filter(|x| x.0 == k && (lo..hi).contains(&x.1)).collect();
            let n = ts.len();
            out.push(json!({
                "day": date, "strategy": name, "cluster_size": size, "trades": n,
                "win_pct": (n > 0).then(|| ts.iter().filter(|x| x.2 > 0.0).count() as f64 * 100.0 / n as f64),
                "avg_pnl_bp": (n > 0).then(|| ts.iter().map(|x| x.3).sum::<f64>() / n as f64),
                "pnl_usd": ts.iter().map(|x| x.2).sum::<f64>(),
                "equity_end": (size == "all").then_some(equity[k]),
            }));
        }
    }
    out
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
    fn iso_times() {
        assert_eq!(iso(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso(1_791_544_797.25), "2026-10-09T11:19:57.250Z");
        assert_eq!(iso(951_782_400.0), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn a_day_per_strategy_and_size() {
        // cascade_15m: a win on a 2M cluster and a loss on a 0.5M one; bounce_60m: one loss.
        let trades = [(0, 2e6, 10.0, 100.0), (0, 5e5, -4.0, -40.0), (3, 6e6, -2.0, -20.0)];
        let rows = daily(&trades, 20_000.0, &[1006.0, 1000.0, 1000.0, 998.0]);
        assert_eq!(rows.len(), 16);
        let row = |s: &str, size: &str| rows.iter().find(|r| r["strategy"] == s && r["cluster_size"] == size).unwrap().clone();
        let all = row("cascade_15m", "all");
        assert_eq!((all["trades"].as_u64(), all["win_pct"].as_f64(), all["avg_pnl_bp"].as_f64()), (Some(2), Some(50.0), Some(30.0)));
        assert_eq!((all["pnl_usd"].as_f64(), all["equity_end"].as_f64(), all["day"].as_str()), (Some(6.0), Some(1006.0), Some("2024-10-04")));
        assert_eq!(row("cascade_15m", "1-5M")["trades"].as_u64(), Some(1));
        assert!(row("cascade_15m", "1-5M")["equity_end"].is_null());
        assert_eq!(row("bounce_60m", "5M+")["pnl_usd"].as_f64(), Some(-2.0));
        assert!(row("cascade_60m", "all")["win_pct"].is_null());
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
