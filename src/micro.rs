//! The market's microstructure record (2026-11-11, the user's call: a two-week test of the order-book and tape
//! strategies, ~50 ideas, results to be trusted): no trading, the data only. Next to `walls` (the walls' life):
//!
//! - `min`, one row a minute: per coin (the walls' main-dex coins and the HIP-3 coins of `H3_BOOK_MIN_VOL`+ a day,
//!   plus any coin traded that minute out of `H3_TAPE_MIN_VOL`+) the columns `COLS`: the book (the bbo's spread, the
//!   grouped book's depth within 0.5 / 1 / 2% each side, its lowest 1% depth in the minute, how far its 20 levels
//!   reach) and the tape (the takers' buys / sells, the trades, the largest one, the distinct buying / selling
//!   takers, the busiest account's share and its id, high / low / last). Sampled each `SAMPLE_S`.
//! - `fill`: each fill of the bot's own account (`LIVE_ACCOUNT`): who traded against it - the takers on the other
//!   side in the same block and over the `LOOKBACK_MS` before, their volume, the book then (the "one seller or a
//!   crowd" filter of the wick, the book filters of every strategy).
//! - `addrs`: each `ADDR_EVERY_S`, the open orders (the triggers - stops / takes - of `TRIGGER_MIN_USD`+, the
//!   limits of `LIMIT_MIN_USD`+, `PER_ADDR_LIMITS` an account at most) and the positions (`POS_MIN_USD`+, with their
//!   liquidation price) of the `ADDR_TOP` accounts busiest on the tape and `ADDR_ROTATE` more taken in turn out of
//!   the next `ADDR_POOL`: the stops' and liquidations' map (HL shows any account's orders), the walls' owners.
//!   The main dex only. On its own request budget (`ADDR_WEIGHT_PER_MIN`), off the bot's.
//!
//! The books and the tape over the websocket. A restart loses the minute in flight.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{Value, json};

use crate::api::{Api, now, num};
use crate::log;
use crate::store::Store;
use crate::ws::{Bbos, Book, Books, Tape};

const H3_BOOK_MIN_VOL: f64 = 3e6;
const H3_BOOK_MAX: usize = 40;
const H3_TAPE_MIN_VOL: f64 = 0.5e6;
const SAMPLE_S: f64 = 5.0;
const STALE_S: f64 = 10.0;
const DEPTHS: [f64; 3] = [0.005, 0.01, 0.02];
const LOOKBACK_MS: u64 = 60_000;
const ADDR_EVERY_S: f64 = 600.0;
const ADDR_TOP: usize = 30;
const ADDR_ROTATE: usize = 30;
const ADDR_POOL: usize = 600;
const ADDR_WEIGHT_PER_MIN: f64 = 150.0;
/// The accounts' tape volume decays with this half-life (s).
const HALF_LIFE_S: f64 = 6.0 * 3600.0;
const TRIGGER_MIN_USD: f64 = 1_000.0;
const LIMIT_MIN_USD: f64 = 25_000.0;
const PER_ADDR_LIMITS: usize = 20;
const POS_MIN_USD: f64 = 10_000.0;

/// The `min` row's columns, a coin's array in this order.
pub(crate) const COLS: [&str; 22] = ["mid", "spread_bp", "bid05", "bid1", "bid2", "ask05", "ask1", "ask2", "bid1_min", "ask1_min",
    "reach_bid_pct", "reach_ask_pct", "trades", "buy_usd", "sell_usd", "max_trade_usd", "buyers", "sellers", "top_share", "hi", "lo", "last"];

/// `x` to `n` significant figures (the rows' size).
fn sig(x: f64, n: i32) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return 0.0;
    }
    let k = 10f64.powi(n - 1 - x.abs().log10().floor() as i32);
    (x * k).round() / k
}

/// The book's $ depth within each of `DEPTHS` of `mid` (bids, asks) and how far its levels reach (%).
fn depth(b: &Book, mid: f64) -> ([f64; 3], [f64; 3], f64, f64) {
    let side = |lv: &[(f64, f64)]| {
        let mut d = [0.0; 3];
        let mut reach: f64 = 0.0;
        for &(p, s) in lv {
            let dist = (p / mid - 1.0).abs();
            reach = reach.max(dist);
            for (i, x) in DEPTHS.iter().enumerate() {
                if dist <= *x {
                    d[i] += p * s;
                }
            }
        }
        (d, reach * 100.0)
    };
    let (bd, br) = side(&b.bids);
    let (ad, ar) = side(&b.asks);
    (bd, ad, br, ar)
}

#[derive(Default)]
struct Acc {
    n: u32,
    buy: f64,
    sell: f64,
    max_trade: f64,
    hi: f64,
    lo: f64,
    last: f64,
    buyers: HashSet<String>,
    sellers: HashSet<String>,
    by: HashMap<String, f64>,
    depth: Option<([f64; 3], [f64; 3], f64, f64)>,
    bid1_min: f64,
    ask1_min: f64,
    spread_sum: f64,
    spread_n: u32,
    mid: f64,
}

impl Acc {
    fn trade(&mut self, t: &Tape) {
        let usd = t.px * t.sz;
        self.n += 1;
        if t.taker_buy {
            self.buy += usd;
            self.buyers.insert(t.buyer.clone());
        } else {
            self.sell += usd;
            self.sellers.insert(t.seller.clone());
        }
        self.max_trade = self.max_trade.max(usd);
        self.hi = if self.n == 1 { t.px } else { self.hi.max(t.px) };
        self.lo = if self.n == 1 { t.px } else { self.lo.min(t.px) };
        self.last = t.px;
        *self.by.entry(t.buyer.clone()).or_default() += usd;
        *self.by.entry(t.seller.clone()).or_default() += usd;
    }

    fn sample(&mut self, mid: f64, spread_bp: Option<f64>, book: Option<&Book>) {
        self.mid = mid;
        if let Some(s) = spread_bp {
            self.spread_sum += s;
            self.spread_n += 1;
        }
        if let Some(b) = book {
            let d = depth(b, mid);
            self.bid1_min = if self.depth.is_none() { d.0[1] } else { self.bid1_min.min(d.0[1]) };
            self.ask1_min = if self.depth.is_none() { d.1[1] } else { self.ask1_min.min(d.1[1]) };
            self.depth = Some(d);
        }
    }

    /// The coin's row (`COLS`) and its busiest account; None when nothing was seen.
    fn row(&self) -> Option<(Vec<f64>, Option<String>)> {
        if self.n == 0 && self.mid == 0.0 {
            return None;
        }
        let (bd, ad, br, ar) = self.depth.unwrap_or(([0.0; 3], [0.0; 3], 0.0, 0.0));
        let vol = self.buy + self.sell;
        let top = self.by.iter().max_by(|a, b| a.1.total_cmp(b.1));
        let spread = if self.spread_n > 0 { self.spread_sum / self.spread_n as f64 } else { 0.0 };
        let r = vec![sig(self.mid, 6), sig(spread, 3), sig(bd[0], 3), sig(bd[1], 3), sig(bd[2], 3), sig(ad[0], 3), sig(ad[1], 3), sig(ad[2], 3),
            sig(self.bid1_min, 3), sig(self.ask1_min, 3), sig(br, 3), sig(ar, 3), self.n as f64, sig(self.buy, 4), sig(self.sell, 4),
            sig(self.max_trade, 3), self.buyers.len() as f64, self.sellers.len() as f64,
            if vol > 0.0 { sig(top.map(|t| *t.1).unwrap_or(0.0) / vol, 3) } else { 0.0 }, sig(self.hi, 6), sig(self.lo, 6), sig(self.last, 6)];
        Some((r, top.filter(|_| vol > 0.0).map(|t| t.0.chars().take(10).collect())))
    }
}

/// The bot's own fills of one block on a coin, with who traded against them.
struct OurFill {
    coin: String,
    time_ms: u64,
    buy: bool,
    usd: f64,
    px_usd: f64,
    sz: f64,
    maker_usd: f64,
    ctx: Value,
}

/// Who was on the other side of the bot's fill `t` (`we_buy`), from the coin's recent tape `hist` (the trade itself
/// in it): the same block's and the `LOOKBACK_MS` before's takers on that side.
fn counterparties(hist: &VecDeque<Tape>, t: &Tape, we_buy: bool, me: &str) -> Value {
    let other_side = |x: &Tape| x.taker_buy != we_buy; // the takers selling into our bid (or buying our ask)
    let taker = |x: &Tape| if x.taker_buy { x.buyer.clone() } else { x.seller.clone() };
    let mut block: HashSet<String> = HashSet::new();
    let (mut block_usd, mut win_usd) = (0.0, 0.0);
    let mut win: HashMap<String, f64> = HashMap::new();
    let mut first_px = None;
    for x in hist.iter().filter(|x| x.time_ms + LOOKBACK_MS >= t.time_ms && x.time_ms <= t.time_ms) {
        first_px.get_or_insert(x.px);
        let who = taker(x);
        if !other_side(x) || who == me {
            continue;
        }
        let usd = x.px * x.sz;
        if x.time_ms == t.time_ms {
            block.insert(who.clone());
            block_usd += usd;
        }
        win_usd += usd;
        *win.entry(who).or_default() += usd;
    }
    let top = win.values().cloned().fold(0.0, f64::max);
    json!({"block_takers": block.len(), "block_usd": sig(block_usd, 4), "lookback_takers": win.len(), "lookback_usd": sig(win_usd, 4),
        "lookback_top_share": if win_usd > 0.0 { sig(top / win_usd, 3) } else { 0.0 },
        "move_lookback_pct": first_px.map(|p| sig((t.px / p - 1.0) * 100.0, 3)), "lookback_s": LOOKBACK_MS / 1000})
}

/// The orders worth keeping of an account's `frontendOpenOrders`: [coin, side, px, usd, type, trigger px, reduce only],
/// and how many it has.
fn orders_of(v: &Value) -> (Vec<Value>, usize) {
    let list = v.as_array().cloned().unwrap_or_default();
    let n = list.len();
    let mut trig = Vec::new();
    let mut lim: Vec<(f64, Value)> = Vec::new();
    for o in &list {
        let (px, sz) = (num(&o["limitPx"]), num(&o["sz"]));
        let is_trig = o["isTrigger"].as_bool().unwrap_or(false);
        let tpx = num(&o["triggerPx"]);
        let usd = sz * if is_trig && tpx > 0.0 { tpx } else { px };
        let row = json!([o["coin"], o["side"], sig(px, 6), sig(usd, 3), o["orderType"], if is_trig { json!(sig(tpx, 6)) } else { Value::Null }, o["reduceOnly"]]);
        // a position's own stop / take carries no size (the whole position: its $ in the account's `pos`)
        if is_trig && (usd >= TRIGGER_MIN_USD || o["isPositionTpsl"].as_bool().unwrap_or(false)) {
            trig.push(row);
        } else if !is_trig && usd >= LIMIT_MIN_USD {
            lim.push((usd, row));
        }
    }
    lim.sort_by(|a, b| b.0.total_cmp(&a.0));
    trig.extend(lim.into_iter().take(PER_ADDR_LIMITS).map(|x| x.1));
    (trig, n)
}

/// An account's positions worth keeping: [coin, signed $, entry, liquidation px, leverage], and its account value.
fn positions_of(v: &Value) -> (Vec<Value>, f64) {
    let mut out = Vec::new();
    for p in v["assetPositions"].as_array().into_iter().flatten() {
        let p = &p["position"];
        let (szi, val) = (num(&p["szi"]), num(&p["positionValue"]));
        if val < POS_MIN_USD {
            continue;
        }
        out.push(json!([p["coin"], sig(val * szi.signum(), 4), sig(num(&p["entryPx"]), 6), sig(num(&p["liquidationPx"]), 6), num(&p["leverage"]["value"])]));
    }
    (out, num(&v["marginSummary"]["accountValue"]))
}

/// The accounts to read this round: the `ADDR_TOP` busiest, then `ADDR_ROTATE` from the next `ADDR_POOL` at `cursor`.
fn pick(vol: &HashMap<String, f64>, me: &str, cursor: usize) -> Vec<(String, f64, usize)> {
    let mut v: Vec<(&String, &f64)> = vol.iter().filter(|(a, _)| a.as_str() != me).collect();
    v.sort_by(|a, b| b.1.total_cmp(a.1).then(a.0.cmp(b.0)));
    let mut out: Vec<(String, f64, usize)> = v.iter().take(ADDR_TOP).enumerate().map(|(i, (a, x))| ((*a).clone(), **x, i)).collect();
    let pool: Vec<_> = v.iter().enumerate().skip(ADDR_TOP).take(ADDR_POOL).collect();
    if !pool.is_empty() {
        for k in 0..ADDR_ROTATE.min(pool.len()) {
            let (i, (a, x)) = pool[(cursor + k) % pool.len()];
            out.push(((*a).clone(), **x, i));
        }
    }
    out
}

/// Starts the record (`coins`: the walls' main-dex coins, `agg`: their grouped books).
pub async fn start(store: Store, api: Api, coins: Vec<String>, agg: Books) -> Result<()> {
    let me = std::env::var("LIVE_ACCOUNT").unwrap_or_default().trim().to_lowercase();
    let mut h3: Vec<(String, f64)> = Vec::new();
    for d in api.dexes().await? {
        match api.dex_volumes(&d).await {
            Ok(v) => h3.extend(v.into_iter().filter(|x| x.1 >= H3_TAPE_MIN_VOL)),
            Err(e) => log!("micro: {d} volumes not read: {e:#}"),
        }
    }
    h3.sort_by(|a, b| b.1.total_cmp(&a.1));
    let h3_book: Vec<String> = h3.iter().filter(|x| x.1 >= H3_BOOK_MIN_VOL).take(H3_BOOK_MAX).map(|x| x.0.clone()).collect();
    let h3_tape: Vec<String> = h3.iter().map(|x| x.0.clone()).collect();
    let booked: Vec<String> = coins.iter().chain(h3_book.iter()).cloned().collect();
    log!("micro: recording {} main + {} HIP-3 coins' books / tape, {} HIP-3 more on the tape; the bot's fills{}", coins.len(), h3_book.len(),
        h3_tape.len() - h3_book.len(), if me.is_empty() { " (no LIVE_ACCOUNT: off)" } else { "" });
    store.event(json!({"kind": "micro", "at": now(), "what": "start", "cols": COLS, "main": coins, "h3_book": h3_book, "h3_tape": h3_tape,
        "depths": DEPTHS, "sample_s": SAMPLE_S}));
    let h3_agg: Books = Arc::default();
    tokio::spawn(crate::ws::run_books_sig(h3_book.clone(), Some(crate::walls::SIG), h3_agg.clone()));
    let bbos: Bbos = Arc::default();
    tokio::spawn(crate::ws::run_bbo("bbo", booked.clone(), bbos.clone()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Tape>();
    tokio::spawn(crate::ws::run_tape("tape (main)", coins.clone(), tx.clone()));
    tokio::spawn(crate::ws::run_tape("tape (HIP-3)", h3_tape, tx));
    let vol: Arc<Mutex<HashMap<String, f64>>> = Arc::default();
    // The accounts' orders and positions, on their own budget.
    {
        let (vol, store, me) = (vol.clone(), store.clone(), me.clone());
        let api = Api::new(ADDR_WEIGHT_PER_MIN)?;
        tokio::spawn(async move {
            let mut cursor = 0usize;
            let mut round = 0u64;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs_f64(ADDR_EVERY_S));
            tick.tick().await;
            loop {
                tick.tick().await;
                let picked = {
                    let mut v = vol.lock().unwrap();
                    let f = 0.5f64.powf(ADDR_EVERY_S / HALF_LIFE_S);
                    v.values_mut().for_each(|x| *x *= f);
                    if v.len() > 40_000 {
                        let mut all: Vec<f64> = v.values().cloned().collect();
                        all.sort_by(|a, b| b.total_cmp(a));
                        let cut = all[20_000];
                        v.retain(|_, x| *x > cut);
                    }
                    pick(&v, &me, cursor)
                };
                cursor += ADDR_ROTATE;
                round += 1;
                let t0 = now();
                let mut rows = Vec::new();
                let mut failed = 0;
                for (a, tape_usd, rank) in picked {
                    let o = api.info(json!({"type": "frontendOpenOrders", "user": a}), 20).await;
                    let p = api.info(json!({"type": "clearinghouseState", "user": a}), 2).await;
                    let (Ok(o), Ok(p)) = (o, p) else { failed += 1; continue };
                    let ((orders, n_orders), (pos, acct)) = (orders_of(&o), positions_of(&p));
                    rows.push(json!({"addr": a, "rank": rank, "tape_usd": sig(tape_usd, 3), "n_orders": n_orders, "orders": orders, "pos": pos, "acct": sig(acct, 4)}));
                }
                store.event(json!({"kind": "micro", "at": now(), "what": "addrs", "round": round, "t0": t0, "failed": failed, "a": rows}));
            }
        });
    }
    // The tape, the samples and the minute rows.
    tokio::spawn(async move {
        let mut accs: HashMap<String, Acc> = HashMap::new();
        let mut hist: HashMap<String, VecDeque<Tape>> = HashMap::new();
        let mut ours: Vec<OurFill> = Vec::new();
        let mut minute = (now() / 60.0).floor() as i64;
        let mut sampled = 0.0;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                Some(t) = rx.recv() => {
                    let usd = t.px * t.sz;
                    {
                        let mut v = vol.lock().unwrap();
                        *v.entry(t.buyer.clone()).or_default() += usd;
                        *v.entry(t.seller.clone()).or_default() += usd;
                    }
                    accs.entry(t.coin.clone()).or_default().trade(&t);
                    let h = hist.entry(t.coin.clone()).or_default();
                    while h.front().is_some_and(|x| x.time_ms + LOOKBACK_MS < t.time_ms) {
                        h.pop_front();
                    }
                    h.push_back(t.clone());
                    if !me.is_empty() && (t.buyer == me || t.seller == me) {
                        let buy = t.buyer == me;
                        let maker = t.taker_buy != buy;
                        if let Some(f) = ours.iter_mut().find(|f| f.coin == t.coin && f.time_ms == t.time_ms && f.buy == buy) {
                            f.usd += usd;
                            f.px_usd += t.px * usd;
                            f.sz += t.sz;
                            if maker { f.maker_usd += usd; }
                        } else {
                            let mut ctx = counterparties(h, &t, buy, &me);
                            let book = agg.read().unwrap().get(&t.coin).cloned().or_else(|| h3_agg.read().unwrap().get(&t.coin).cloned());
                            let bbo = bbos.read().unwrap().get(&t.coin).cloned();
                            if let Some((b, a, _)) = bbo {
                                let mid = (b + a) / 2.0;
                                ctx["spread_bp"] = json!(sig((a - b) / mid * 1e4, 3));
                                if let Some(bk) = book {
                                    let (bd, ad, _, _) = depth(&bk, mid);
                                    ctx["bid1"] = json!(sig(bd[1], 3));
                                    ctx["ask1"] = json!(sig(ad[1], 3));
                                }
                            }
                            ours.push(OurFill { coin: t.coin.clone(), time_ms: t.time_ms, buy, usd, px_usd: t.px * usd, sz: t.sz, maker_usd: if maker { usd } else { 0.0 }, ctx });
                        }
                    }
                }
                _ = tick.tick() => {
                    let t = now();
                    // The bot's fills, once their block is surely all in.
                    let (done, keep): (Vec<OurFill>, Vec<OurFill>) = std::mem::take(&mut ours).into_iter().partition(|f| t * 1000.0 - f.time_ms as f64 > 3000.0);
                    ours = keep;
                    for f in done {
                        let mut ev = json!({"kind": "micro", "at": t, "what": "fill", "coin": f.coin, "time_ms": f.time_ms, "buy": f.buy, "usd": sig(f.usd, 4),
                            "px": sig(f.px_usd / f.usd.max(1e-12), 6), "sz": f.sz, "maker_share": sig(f.maker_usd / f.usd.max(1e-12), 3)});
                        if let (Some(o), Some(c)) = (ev.as_object_mut(), f.ctx.as_object()) {
                            o.extend(c.clone());
                        }
                        store.event(ev);
                    }
                    if t - sampled >= SAMPLE_S {
                        sampled = t;
                        let (a1, a2, bb) = (agg.read().unwrap().clone(), h3_agg.read().unwrap().clone(), bbos.read().unwrap().clone());
                        for c in &booked {
                            let bbo = bbo_fresh(bb.get(c), t);
                            let book = a1.get(c).or_else(|| a2.get(c)).filter(|b| t - b.time_ms as f64 / 1000.0 <= STALE_S);
                            let Some(mid) = bbo.map(|(b, a)| (b + a) / 2.0).or_else(|| book.and_then(Book::mid)) else { continue };
                            accs.entry(c.clone()).or_default().sample(mid, bbo.map(|(b, a)| (a - b) / mid * 1e4), book);
                        }
                    }
                    let m = (t / 60.0).floor() as i64;
                    if m > minute {
                        let mut c = serde_json::Map::new();
                        let mut top = serde_json::Map::new();
                        for (coin, acc) in std::mem::take(&mut accs) {
                            if let Some((r, who)) = acc.row() {
                                c.insert(coin.clone(), json!(r));
                                if let Some(w) = who {
                                    top.insert(coin, json!(w));
                                }
                            }
                        }
                        store.event(json!({"kind": "micro", "at": t, "what": "min", "m": minute, "c": c, "top": top}));
                        minute = m;
                    }
                }
            }
        }
    });
    Ok(())
}

fn bbo_fresh(x: Option<&(f64, f64, u64)>, t: f64) -> Option<(f64, f64)> {
    x.filter(|b| t - b.2 as f64 / 1000.0 <= STALE_S).map(|b| (b.0, b.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tape(px: f64, sz: f64, buy: bool, ms: u64, buyer: &str, seller: &str) -> Tape {
        Tape { coin: "X".into(), px, sz, taker_buy: buy, time_ms: ms, buyer: buyer.into(), seller: seller.into() }
    }

    #[test]
    fn sig_figs() {
        assert_eq!(sig(123456.0, 3), 123000.0);
        assert_eq!(sig(0.00123456, 3), 0.00123);
        assert_eq!(sig(-98.76, 2), -99.0);
        assert_eq!(sig(0.0, 3), 0.0);
    }

    #[test]
    fn depth_within_the_bands() {
        let b = Book { bids: vec![(99.6, 10.0), (99.1, 10.0), (98.5, 10.0)], asks: vec![(100.4, 1.0), (101.5, 1.0), (103.0, 1.0)], time_ms: 0 };
        let (bd, ad, br, ar) = depth(&b, 100.0);
        assert!((bd[0] - 996.0).abs() < 1e-9); // 0.4% in, 1% not
        assert!((bd[1] - 1987.0).abs() < 1e-9);
        assert!((bd[2] - 2972.0).abs() < 1e-9);
        assert!((ad[2] - 201.9).abs() < 1e-9); // 103 (3%) out
        assert!((br - 1.5).abs() < 1e-9 && (ar - 3.0).abs() < 1e-9);
    }

    #[test]
    fn a_minute_row() {
        let mut a = Acc::default();
        a.trade(&tape(100.0, 1.0, true, 1, "a", "mm"));
        a.trade(&tape(101.0, 2.0, false, 2, "mm", "b"));
        a.trade(&tape(99.0, 1.0, false, 3, "mm", "c"));
        a.sample(100.0, Some(2.0), None);
        let (r, who) = a.row().unwrap();
        let col = |n: &str| r[COLS.iter().position(|c| *c == n).unwrap()];
        assert_eq!(col("trades"), 3.0);
        assert_eq!(col("buy_usd"), 100.0);
        assert_eq!(col("sell_usd"), 301.0);
        assert_eq!(col("buyers"), 1.0);
        assert_eq!(col("sellers"), 2.0);
        assert_eq!(col("hi"), 101.0);
        assert_eq!(col("lo"), 99.0);
        assert_eq!(col("last"), 99.0);
        assert_eq!(col("top_share"), 1.0); // "mm" in every trade
        assert_eq!(who.as_deref(), Some("mm"));
        assert_eq!(col("spread_bp"), 2.0);
    }

    #[test]
    fn one_seller_or_a_crowd_into_our_bid() {
        let me = "me";
        let mut h = VecDeque::new();
        h.push_back(tape(100.0, 1.0, false, 1_000, "mm", "s1")); // a taker sells, 69 s before: out of the window
        h.push_back(tape(100.0, 1.0, false, 50_000, "mm", "s2"));
        h.push_back(tape(99.0, 1.0, true, 60_000, "b1", "mm"));  // a buy: not the other side
        h.push_back(tape(98.0, 3.0, false, 70_000, "mm", "s3"));
        let ours = tape(98.0, 2.0, false, 70_000, me, "s3"); // s3 sells into our bid in the same block
        h.push_back(ours.clone());
        let c = counterparties(&h, &ours, true, me);
        assert_eq!(c["block_takers"], 1);
        assert_eq!(c["lookback_takers"], 2); // s2, s3
        assert_eq!(c["block_usd"], 490.0);
        assert!((c["lookback_top_share"].as_f64().unwrap() - 0.831).abs() < 1e-9); // 490 / 590
    }

    #[test]
    fn the_orders_and_positions_kept() {
        let o = json!([
            {"coin": "BTC", "side": "A", "limitPx": "90000", "sz": "0.5", "isTrigger": true, "triggerPx": "91000", "orderType": "Stop Market", "reduceOnly": true},
            {"coin": "BTC", "side": "B", "limitPx": "80000", "sz": "1", "isTrigger": false, "triggerPx": "0", "orderType": "Limit", "reduceOnly": false},
            {"coin": "ETH", "side": "B", "limitPx": "3000", "sz": "1", "isTrigger": false, "triggerPx": "0", "orderType": "Limit", "reduceOnly": false}]);
        let (rows, n) = orders_of(&o);
        assert_eq!(n, 3);
        assert_eq!(rows.len(), 2); // the $3k limit left out
        assert_eq!(rows[0][4], "Stop Market");
        assert_eq!(rows[0][3], 45500.0);
        let p = json!({"marginSummary": {"accountValue": "1000000"}, "assetPositions": [
            {"position": {"coin": "ETH", "szi": "-100", "positionValue": "300000", "entryPx": "3100", "liquidationPx": "3500", "leverage": {"value": 5}}},
            {"position": {"coin": "SOL", "szi": "1", "positionValue": "150", "entryPx": "150", "liquidationPx": null, "leverage": {"value": 3}}}]});
        let (pos, acct) = positions_of(&p);
        assert_eq!(pos.len(), 1);
        assert_eq!(pos[0][1], -300000.0);
        assert_eq!(pos[0][3], 3500.0);
        assert_eq!(acct, 1e6);
    }

    #[test]
    fn the_accounts_read_top_and_in_turn() {
        let mut v = HashMap::new();
        for i in 0..100 {
            v.insert(format!("a{i:03}"), 1000.0 - i as f64);
        }
        v.insert("me".into(), 1e9);
        let p = pick(&v, "me", 0);
        assert_eq!(p.len(), ADDR_TOP + ADDR_ROTATE);
        assert!(p.iter().all(|x| x.0 != "me"));
        assert_eq!(p[0].0, "a000");
        assert_eq!(p[ADDR_TOP].0, format!("a{:03}", ADDR_TOP));
        let q = pick(&v, "me", 60); // the pool of 70 wraps
        assert_eq!(q[ADDR_TOP].0, format!("a{:03}", ADDR_TOP + 60));
        assert_eq!(q[ADDR_TOP + 10].0, format!("a{:03}", ADDR_TOP));
    }
}
