//! The walls' record (2026-11-10, the user's call): no trading, the data for a "bounce off a wall" study. Only the
//! walls are written, not the books: a level of the grouped book (`SIG` significant figures: 20 levels reach
//! ~0.25-2% from the mid, the exchange's own ticks only ~0.03-0.9%) of at least `WALL_X` times the side's median
//! level and `MIN_USD`, within `MAX_DIST` of the mid, on the `TOP_COINS` main-dex coins by volume.
//!
//! Events (kind "walls"):
//! - a wall counts once it stands `MIN_AGE_S` (the shorter ones only counted: `flicker`, hourly); once one, it stays one
//!   down to `HOLD` of the bar (a 3 min sample on 2026-11-10 without it: 2 of 3 walls 'pulled' at the bar's edge);
//! - `touch`: the mid within `TOUCH` of it (each approach once), with the exchange-tick level found inside its bucket;
//! - `off`: its one row, at its end (its first look in it too), and why it went: `crossed` (the price went through it), `pulled_near` (removed with the price within
//!   `NEAR`), `pulled` (removed, the price away), `out_of_view` (the price moved off: past the book's 20 levels);
//! - `after`: from a wall's first touch, the mid 1 / 5 / 15 / 60 min on and its high / low over 15 and 60 min.
//!
//! The books come over the websocket (no request off the address's limit). A book older than `STALE_S` is not read
//! (a reconnect is not a wall pulled). A restart loses the walls in flight (the `after`s not yet written too).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use serde_json::{Value, json};

use crate::api::{Api, now};
use crate::log;
use crate::store::Store;
use crate::ws::{Book, Books};

const TOP_COINS: usize = 40;
const MIN_VOL_USD: f64 = 10e6;
pub(crate) const SIG: u32 = 4;
const WALL_X: f64 = 5.0;
const MIN_USD: f64 = 25_000.0;
const MAX_DIST: f64 = 0.03;
const MIN_AGE_S: f64 = 30.0;
/// A wall's bar once it is one: half (see step).
const HOLD: f64 = 0.5;
const TOUCH: f64 = 0.001;
const NEAR: f64 = 0.003;
const STALE_S: f64 = 10.0;
const AFTER_S: [f64; 4] = [60.0, 300.0, 900.0, 3600.0];
const FLICKER_EVERY_S: f64 = 3600.0;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Side {
    Bid,
    Ask,
}

impl Side {
    fn name(self) -> &'static str {
        match self {
            Side::Bid => "bid",
            Side::Ask => "ask",
        }
    }
}

/// A grouped level's width at `px` with `SIG` significant figures.
fn bucket(px: f64) -> f64 {
    10f64.powi(px.log10().floor() as i32 + 1 - SIG as i32)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n == 0 { 0.0 } else if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

struct Wall {
    id: String,
    px: f64,
    first: f64,
    usd0: f64,
    usd: f64,
    max_usd: f64,
    x: f64,
    dist0: f64,
    min_dist: f64,
    mid0: f64,
    told: bool,
    near: bool,
    touches: u32,
    followed: bool,
}

struct Follow {
    id: String,
    coin: String,
    side: Side,
    px: f64,
    t0: f64,
    mid0: f64,
    marks: [Option<f64>; 4],
    hi15: f64,
    lo15: f64,
    hi60: f64,
    lo60: f64,
}

#[derive(Default)]
pub(crate) struct Tracker {
    walls: HashMap<(String, Side, i64), Wall>,
    follows: Vec<Follow>,
    flicker: u64,
}

fn key_px(px: f64) -> i64 {
    (px * 1e8).round() as i64
}

impl Tracker {
    /// One look at `coin`'s grouped book `agg` (and its exchange-tick book `fine`, for the mid and the touches): the
    /// events it makes.
    pub(crate) fn step(&mut self, coin: &str, t: f64, agg: &Book, fine: Option<&Book>) -> Vec<Value> {
        let mut out = Vec::new();
        let Some(mid) = fine.and_then(Book::mid).or_else(|| agg.mid()) else { return out };
        for side in [Side::Bid, Side::Ask] {
            let levels = if side == Side::Bid { &agg.bids } else { &agg.asks };
            if levels.len() < 10 {
                continue;
            }
            let med = median(levels.iter().map(|(p, s)| p * s).collect());
            let mut seen = HashSet::new();
            for &(px, sz) in levels {
                let (usd, dist) = (px * sz, (px / mid - 1.0).abs());
                let k = (coin.to_string(), side, key_px(px));
                // A wall stays one until under half the bar (not on and off at the bar's edge as the median moves).
                let keep = if self.walls.contains_key(&k) { HOLD } else { 1.0 };
                if usd < keep * WALL_X * med || usd < keep * MIN_USD || dist > MAX_DIST {
                    continue;
                }
                seen.insert(k.2);
                let w = self.walls.entry(k).or_insert_with(|| Wall {
                    id: format!("{coin}-{}-{px}-{}", side.name(), (t * 1000.0) as i64), px, first: t, usd0: usd, usd, max_usd: usd,
                    x: usd / med, dist0: dist, min_dist: dist, mid0: mid, told: false, near: false, touches: 0, followed: false,
                });
                w.usd = usd;
                w.max_usd = w.max_usd.max(usd);
                w.min_dist = w.min_dist.min(dist);
                // A wall is one row, at its end (`off`: its first look too); a 30 min live sample on 2026-11-10: ~730
                // walls, 40 touches - a row at its start as well doubled the rows for nothing.
                if !w.told && t - w.first >= MIN_AGE_S {
                    w.told = true;
                }
                let near = dist <= TOUCH;
                if w.told && near && !w.near {
                    w.touches += 1;
                    let fine_lvl = fine.and_then(|b| {
                        let lv = if side == Side::Bid { &b.bids } else { &b.asks };
                        let (lo, hi) = if side == Side::Bid { (px, px + bucket(px)) } else { (px - bucket(px), px) };
                        lv.iter().filter(|(p, _)| *p >= lo - 1e-12 && *p <= hi + 1e-12).max_by(|a, b| (a.0 * a.1).total_cmp(&(b.0 * b.1))).copied()
                    });
                    out.push(json!({"what": "touch", "id": w.id, "coin": coin, "side": side.name(), "px": px, "usd": usd, "n": w.touches,
                        "mid": mid, "age_s": t - w.first, "fine_px": fine_lvl.map(|l| l.0), "fine_usd": fine_lvl.map(|l| l.0 * l.1)}));
                    if !w.followed {
                        w.followed = true;
                        self.follows.push(Follow { id: w.id.clone(), coin: coin.to_string(), side, px, t0: t, mid0: mid, marks: [None; 4],
                            hi15: mid, lo15: mid, hi60: mid, lo60: mid });
                    }
                }
                w.near = near;
            }
            // The walls of this side not seen now: gone.
            let best = fine.and_then(|b| if side == Side::Bid { b.bids.first() } else { b.asks.first() }).or(levels.first()).map(|l| l.0);
            let edge = levels.last().map(|l| l.0);
            let gone: Vec<(String, Side, i64)> = self.walls.keys().filter(|k| k.0 == coin && k.1 == side && !seen.contains(&k.2)).cloned().collect();
            for k in gone {
                let w = self.walls.remove(&k).unwrap();
                if !w.told {
                    self.flicker += 1;
                    continue;
                }
                let crossed = best.is_some_and(|b| if side == Side::Bid { b < w.px } else { b > w.px });
                let beyond = edge.is_some_and(|e| if side == Side::Bid { w.px < e } else { w.px > e });
                let dist = (w.px / mid - 1.0).abs();
                let why = if crossed { "crossed" } else if beyond { "out_of_view" } else if dist <= NEAR { "pulled_near" } else { "pulled" };
                out.push(json!({"what": "off", "id": w.id, "coin": coin, "side": side.name(), "px": w.px, "why": why, "life_s": t - w.first,
                    "bucket": bucket(w.px), "usd_first": w.usd0, "usd_last": w.usd, "usd_max": w.max_usd, "x_median_first": w.x,
                    "dist0_pct": w.dist0 * 100.0, "min_dist_pct": w.min_dist * 100.0, "dist_pct": dist * 100.0, "touches": w.touches,
                    "mid0": w.mid0, "mid": mid}));
            }
        }
        // The touched walls' price paths.
        let mut done = Vec::new();
        for (i, f) in self.follows.iter_mut().enumerate().filter(|(_, f)| f.coin == coin) {
            let dt = t - f.t0;
            if dt <= 900.0 {
                f.hi15 = f.hi15.max(mid);
                f.lo15 = f.lo15.min(mid);
            }
            f.hi60 = f.hi60.max(mid);
            f.lo60 = f.lo60.min(mid);
            for (j, s) in AFTER_S.iter().enumerate() {
                if f.marks[j].is_none() && dt >= *s {
                    f.marks[j] = Some(mid);
                }
            }
            if dt >= AFTER_S[3] {
                done.push(i);
            }
        }
        for i in done.into_iter().rev() {
            let f = self.follows.remove(i);
            out.push(json!({"what": "after", "id": f.id, "coin": f.coin, "side": f.side.name(), "px": f.px, "mid0": f.mid0,
                "mid_1m": f.marks[0], "mid_5m": f.marks[1], "mid_15m": f.marks[2], "mid_60m": f.marks[3],
                "hi_15m": f.hi15, "lo_15m": f.lo15, "hi_60m": f.hi60, "lo_60m": f.lo60}));
        }
        out
    }

    /// The walls gone before `MIN_AGE_S` since the last call.
    fn take_flicker(&mut self) -> u64 {
        std::mem::take(&mut self.flicker)
    }
}

/// Starts the record: the grouped books of the top coins over the websocket, a look each second. The coins and
/// their grouped books back (the minute record `micro` reads them too).
pub async fn start(store: Store, api: Api, fine: Books) -> Result<(Vec<String>, Books)> {
    let (_, ctx) = api.meta().await?;
    let mut coins: Vec<(String, f64)> = ctx.into_iter().filter(|(n, c)| c.day_volume >= MIN_VOL_USD && !n.contains(':')).map(|(n, c)| (n, c.day_volume)).collect();
    coins.sort_by(|a, b| b.1.total_cmp(&a.1));
    let coins: Vec<String> = coins.into_iter().take(TOP_COINS).map(|c| c.0).collect();
    log!("walls: recording {} coins' walls (x{WALL_X} the median level, ${MIN_USD:.0}+, within {:.0}%): {}", coins.len(), MAX_DIST * 100.0, coins.join(","));
    let agg: Books = Arc::default();
    tokio::spawn(crate::ws::run_books_sig(coins.clone(), Some(SIG), agg.clone()));
    let back = (coins.clone(), agg.clone());
    tokio::spawn(async move {
        let mut tr = Tracker::default();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut flicker_at = now();
        loop {
            tick.tick().await;
            let t = now();
            for coin in &coins {
                let a = agg.read().unwrap().get(coin).cloned();
                let Some(a) = a else { continue };
                if t - a.time_ms as f64 / 1000.0 > STALE_S {
                    continue;
                }
                let f = fine.read().unwrap().get(coin).cloned().filter(|b| t - b.time_ms as f64 / 1000.0 <= STALE_S);
                for mut ev in tr.step(coin, t, &a, f.as_ref()) {
                    ev["kind"] = json!("walls");
                    ev["at"] = json!(t);
                    store.event(ev);
                }
            }
            if t - flicker_at >= FLICKER_EVERY_S {
                store.event(json!({"kind": "walls", "at": t, "what": "flicker", "n": tr.take_flicker(), "over_s": t - flicker_at}));
                flicker_at = t;
            }
        }
    });
    Ok(back)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grouped book around `mid` with 1-unit-wide levels of `base` size, `wall` added at a bid `px`.
    fn book(mid: f64, base: f64, wall: Option<(f64, f64)>) -> Book {
        let bids = (0..20).map(|i| { let p = (mid - 0.5).floor() - i as f64; (p, if wall.is_some_and(|w| w.0 == p) { wall.unwrap().1 } else { base }) }).collect();
        let asks = (0..20).map(|i| ((mid + 0.5).ceil() + i as f64, base)).collect();
        Book { bids, asks, time_ms: 0 }
    }

    #[test]
    fn the_bucket_widths() {
        assert_eq!(bucket(73280.0), 10.0);
        assert!((bucket(0.1499) - 1e-4).abs() < 1e-12);
        assert!((bucket(82.22) - 0.01).abs() < 1e-12);
    }

    #[test]
    fn a_wall_is_told_after_its_age_touched_and_crossed() {
        let mut tr = Tracker::default();
        // $1k levels at ~1000, a wall of 100 units (~$99k) at 990 (1% under)
        let b = book(1000.0, 1.0, Some((990.0, 100.0)));
        assert!(tr.step("X", 0.0, &b, None).is_empty());
        assert!(tr.step("X", 20.0, &b, None).is_empty());       // not yet MIN_AGE_S
        assert!(tr.step("X", 31.0, &b, None).is_empty());       // one row at its end, none at its start
        // the price comes down to it: touched once, not again while it stays near
        let b2 = book(990.5, 1.0, Some((990.0, 100.0)));
        let ev = tr.step("X", 40.0, &b2, None);
        assert_eq!(ev.iter().filter(|e| e["what"] == "touch").count(), 1);
        assert!(tr.step("X", 41.0, &b2, None).iter().all(|e| e["what"] != "touch"));
        // it goes, the best bid now under it: crossed
        let ev = tr.step("X", 50.0, &book(985.0, 1.0, None), None);
        let off = ev.iter().find(|e| e["what"] == "off").unwrap();
        assert_eq!(off["why"], "crossed");
        assert_eq!(off["touches"], 1);
        assert!((off["dist0_pct"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        // its path is followed from the touch (t 40): the hour's record at +3600 s
        let mut last = Vec::new();
        for s in [100.0, 340.0, 940.0, 3640.0] {
            last = tr.step("X", s, &book(1000.0, 1.0, None), None);
        }
        let after = last.iter().find(|e| e["what"] == "after").unwrap();
        assert_eq!(after["mid0"], 990.5);
        assert_eq!(after["lo_60m"], 985.0);
        assert_eq!(after["mid_1m"], 1000.0);
        assert_eq!(after["mid_60m"], 1000.0);
    }

    #[test]
    fn a_wall_removed_away_is_pulled_a_short_one_only_counted() {
        let mut tr = Tracker::default();
        let b = book(1000.0, 1.0, Some((990.0, 100.0)));
        tr.step("X", 0.0, &b, None);
        tr.step("X", 31.0, &b, None);
        let ev = tr.step("X", 32.0, &book(1000.0, 1.0, None), None);
        assert_eq!(ev[0]["why"], "pulled");
        // near the price (0.2%): pulled_near
        let b = book(992.0, 1.0, Some((990.0, 100.0)));
        tr.step("X", 40.0, &b, None);
        tr.step("X", 71.0, &b, None);
        assert_eq!(tr.step("X", 72.0, &book(992.0, 1.0, None), None)[0]["why"], "pulled_near");
        // gone before MIN_AGE_S: no event, a flicker
        tr.step("X", 80.0, &b, None);
        assert!(tr.step("X", 85.0, &book(992.0, 1.0, None), None).is_empty());
        assert_eq!(tr.take_flicker(), 1);
    }

    #[test]
    fn a_wall_stays_one_down_to_half_its_bar() {
        let mut tr = Tracker::default();
        tr.step("X", 0.0, &book(1000.0, 1.0, Some((990.0, 100.0))), None);
        tr.step("X", 31.0, &book(1000.0, 1.0, Some((990.0, 100.0))), None);
        // at 30 units (~$29.7k, 30x): kept
        assert!(tr.step("X", 32.0, &book(1000.0, 1.0, Some((990.0, 30.0))), None).is_empty());
        // at 20 units (~$19.8k: under $25k, over $12.5k): kept, not told again
        assert!(tr.step("X", 33.0, &book(1000.0, 1.0, Some((990.0, 20.0))), None).is_empty());
        // at 10 units (~$9.9k: under half the $ bar): off
        assert_eq!(tr.step("X", 34.0, &book(1000.0, 1.0, Some((990.0, 10.0))), None)[0]["what"], "off");
    }

    #[test]
    fn small_or_far_levels_are_not_walls() {
        let mut tr = Tracker::default();
        // 20x the median but under $25k
        let b = book(1000.0, 1.0, Some((990.0, 20.0)));
        tr.step("X", 0.0, &b, None);
        assert!(tr.step("X", 31.0, &b, None).is_empty());
    }
}