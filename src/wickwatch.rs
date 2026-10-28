//! The wick bids' observer (2026-10-28, the audit of the wick backtest): it watches, it never orders. On its own
//! websocket:
//! - the account's `orderUpdates` and `userFills` (the exchange's side of every order's life, on the exchange's clock:
//!   placed, cancelled, filled, the stops' and takes' fills) as events kind `wick_ex` (`order` / `fill`), each fill with
//!   the coin's mark and oracle then (when watched) and their age;
//! - the trades and the mark / oracle (`activeAssetCtx`) of the coins the wick picks (`set_watch`), and of those a
//!   shadow still follows;
//! - the shadow of the research: a trade 0.5% through the research's level (the last trade of the minute before x
//!   (1 - pct)) is a fill the backtest counts (event `shadow_fill`: the level, our bid resting then (`set_bid`) or none,
//!   whether the trade went through it, the mark / oracle); then the backtest's exit followed on the trades (the stop at
//!   the first trade at or under -20%, the take on a trade over +tp, else on time) and the mark's low over it (whether a
//!   stop by the mark would have fired): event `shadow_result`. A coin one shadow an hour, as the research's bids.
//! A restart drops the shadows in flight (their fills are logged).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

use crate::api::{exchange_now, num};
use crate::log;
use crate::store::Store;

const WS: &str = "wss://api.hyperliquid.xyz/ws";
const AGAIN_MS: u64 = 3_600_000;

/// A bid resting as the wick bot sees it.
#[derive(Clone, Debug)]
pub(crate) struct BidView {
    pub px: f64,
    pub oid: u64,
    /// When the exchange took it (our clock).
    pub since: f64,
}

static BIDS: Mutex<Option<HashMap<String, BidView>>> = Mutex::new(None);
/// The coins watched: coin -> the research's depth (0.07 crypto, 0.04 HIP-3) and take.
static WATCH: Mutex<Option<HashMap<String, (f64, f64)>>> = Mutex::new(None);

/// The bid on `coin` now (None: none resting, or its cancel sent).
pub(crate) fn set_bid(coin: &str, bid: Option<BidView>) {
    let mut g = BIDS.lock().unwrap();
    let m = g.get_or_insert_with(Default::default);
    match bid {
        Some(b) => m.insert(coin.to_string(), b),
        None => m.remove(coin),
    };
}

fn bid_of(coin: &str) -> Option<BidView> {
    BIDS.lock().unwrap().as_ref().and_then(|m| m.get(coin).cloned())
}

/// The coins to watch (the wick's picks): coin -> (depth, take).
pub(crate) fn set_watch(coins: HashMap<String, (f64, f64)>) {
    *WATCH.lock().unwrap() = Some(coins);
}

fn watched() -> HashMap<String, (f64, f64)> {
    WATCH.lock().unwrap().clone().unwrap_or_default()
}

struct Shadow {
    coin: String,
    t_ms: u64,
    level: f64,
    take: f64,
    end_ms: u64,
    last_px: f64,
    trade_min: f64,
    mark_min: f64,
    mark_stop_ms: Option<u64>,
}

#[derive(Default)]
struct Coin {
    minute: u64,
    last: f64,
    /// The last trade of the minute before.
    prev: f64,
    subscribed_ms: u64,
    mark: f64,
    oracle: f64,
    ctx_at: f64,
}

pub(crate) fn spawn(store: Store, account: String, stop: f64, hold_s: f64) {
    tokio::spawn(async move {
        let mut coins: HashMap<String, Coin> = HashMap::new();
        let mut shadows: Vec<Shadow> = Vec::new();
        let mut last_shadow: HashMap<String, u64> = HashMap::new();
        let ev = |mut e: Value, kind: &str| {
            e["kind"] = json!(kind);
            e["at"] = json!(exchange_now());
            store.event(e);
        };
        loop {
            let ws = match tokio_tungstenite::connect_async(WS).await {
                Ok((ws, _)) => ws,
                Err(e) => {
                    log!("wickwatch: connect failed: {e}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
            let (mut sink, mut read) = ws.split();
            let mut ok = true;
            for s in [json!({"type": "orderUpdates", "user": account}), json!({"type": "userFills", "user": account})] {
                ok &= sink.send(Message::Text(json!({"method": "subscribe", "subscription": s}).to_string())).await.is_ok();
            }
            let mut subs: HashSet<String> = HashSet::new();
            let mut ping = tokio::time::interval(Duration::from_secs(20));
            let mut resync = tokio::time::interval(Duration::from_secs(10));
            log!("wickwatch: connected");
            while ok {
                tokio::select! {
                    _ = ping.tick() => {
                        ok = sink.send(Message::Text(r#"{"method":"ping"}"#.into())).await.is_ok();
                    }
                    _ = resync.tick() => {
                        let now_ms = (exchange_now() * 1000.0) as u64;
                        // The shadows over on time.
                        shadows.retain_mut(|s| {
                            if now_ms < s.end_ms { return true; }
                            ev(result(s, "time", s.last_px, now_ms, stop), "wick_ex");
                            false
                        });
                        let mut want: HashSet<String> = watched().into_keys().collect();
                        want.extend(shadows.iter().map(|s| s.coin.clone()));
                        for c in want.difference(&subs.clone()).cloned().collect::<Vec<_>>() {
                            for t in ["trades", "activeAssetCtx"] {
                                ok &= sink.send(Message::Text(json!({"method": "subscribe", "subscription": {"type": t, "coin": c}}).to_string())).await.is_ok();
                            }
                            coins.entry(c.clone()).or_default().subscribed_ms = now_ms;
                            subs.insert(c);
                        }
                        for c in subs.difference(&want).cloned().collect::<Vec<_>>() {
                            for t in ["trades", "activeAssetCtx"] {
                                ok &= sink.send(Message::Text(json!({"method": "unsubscribe", "subscription": {"type": t, "coin": c}}).to_string())).await.is_ok();
                            }
                            subs.remove(&c);
                            coins.remove(&c);
                        }
                    }
                    m = tokio::time::timeout(Duration::from_secs(60), read.next()) => {
                        let text = match m {
                            Ok(Some(Ok(Message::Text(t)))) => t,
                            Ok(Some(Ok(_))) => continue,
                            _ => { log!("wickwatch: closed or silent, reconnecting"); break; }
                        };
                        let Ok(v) = serde_json::from_str::<Value>(&text) else { continue };
                        let recv = exchange_now();
                        match v["channel"].as_str().unwrap_or("") {
                            "trades" => {
                                let mut list: Vec<&Value> = v["data"].as_array().into_iter().flatten().collect();
                                list.sort_by_key(|t| (t["time"].as_u64().unwrap_or(0), t["tid"].as_u64().unwrap_or(0)));
                                let w = watched();
                                for t in list {
                                    let (coin, px, ms) = (t["coin"].as_str().unwrap_or(""), num(&t["px"]), t["time"].as_u64().unwrap_or(0));
                                    let Some(st) = coins.get_mut(coin) else { continue };
                                    if px <= 0.0 { continue; }
                                    let minute = ms / 60_000;
                                    if minute > st.minute {
                                        if st.minute > 0 { st.prev = st.last; }
                                        st.minute = minute;
                                    }
                                    st.last = px;
                                    // The shadows of this coin: the backtest's exit on the trades (the stop first).
                                    let mut done = Vec::new();
                                    for (i, s) in shadows.iter_mut().enumerate().filter(|(_, s)| s.coin == coin && ms > s.t_ms) {
                                        s.last_px = px;
                                        s.trade_min = s.trade_min.min(px);
                                        if px <= s.level * (1.0 - stop) { done.push((i, "stop", px, ms)); }
                                        else if px > s.level * (1.0 + s.take) { done.push((i, "take", s.level * (1.0 + s.take), ms)); }
                                        else if ms >= s.end_ms { done.push((i, "time", px, ms)); }
                                    }
                                    for (i, why, x, at) in done.into_iter().rev() {
                                        let s = shadows.remove(i);
                                        ev(result(&s, why, x, at, stop), "wick_ex");
                                    }
                                    // A new shadow fill: 0.5% through the research's level, live trades only.
                                    let Some((pct, take)) = w.get(coin).copied() else { continue };
                                    if st.prev <= 0.0 || ms < st.subscribed_ms || last_shadow.get(coin).is_some_and(|x| ms < x + AGAIN_MS) { continue; }
                                    let level = st.prev * (1.0 - pct);
                                    if px > level * 0.995 { continue; }
                                    last_shadow.insert(coin.to_string(), ms);
                                    let bid = bid_of(coin);
                                    ev(json!({"what": "shadow_fill", "coin": coin, "t_ms": ms, "recv": recv, "px": px, "sz": num(&t["sz"]), "prev_close": st.prev,
                                        "pct": pct, "level": level, "bid_px": bid.as_ref().map(|b| b.px), "bid_oid": bid.as_ref().map(|b| b.oid),
                                        "bid_age_s": bid.as_ref().map(|b| recv - b.since), "through_bid": bid.as_ref().map(|b| px <= b.px),
                                        "through_bid_05": bid.as_ref().map(|b| px <= b.px * 0.995),
                                        "mark": (st.mark > 0.0).then_some(st.mark), "oracle": (st.oracle > 0.0).then_some(st.oracle),
                                        "ctx_age_s": (st.ctx_at > 0.0).then(|| recv - st.ctx_at)}), "wick_ex");
                                    shadows.push(Shadow { coin: coin.to_string(), t_ms: ms, level, take, end_ms: ms + (hold_s * 1000.0) as u64, last_px: px,
                                        trade_min: px, mark_min: if st.mark > 0.0 { st.mark } else { f64::MAX }, mark_stop_ms: None });
                                }
                            }
                            "activeAssetCtx" => {
                                let coin = v["data"]["coin"].as_str().unwrap_or("");
                                let ctx = &v["data"]["ctx"];
                                if let Some(st) = coins.get_mut(coin) {
                                    (st.mark, st.oracle, st.ctx_at) = (num(&ctx["markPx"]), num(&ctx["oraclePx"]), recv);
                                    let now_ms = (recv * 1000.0) as u64;
                                    for s in shadows.iter_mut().filter(|s| s.coin == coin && st.mark > 0.0) {
                                        s.mark_min = s.mark_min.min(st.mark);
                                        if s.mark_stop_ms.is_none() && st.mark <= s.level * (1.0 - stop) { s.mark_stop_ms = Some(now_ms); }
                                    }
                                }
                            }
                            "orderUpdates" => {
                                for u in v["data"].as_array().into_iter().flatten() {
                                    let o = &u["order"];
                                    ev(json!({"what": "order", "coin": o["coin"], "oid": o["oid"], "status": u["status"], "status_ms": u["statusTimestamp"],
                                        "placed_ms": o["timestamp"], "side": o["side"], "px": num(&o["limitPx"]), "sz": num(&o["sz"]), "orig_sz": num(&o["origSz"]),
                                        "reduce": o["reduceOnly"], "recv": recv}), "wick_ex");
                                }
                            }
                            "userFills" => {
                                if v["data"]["isSnapshot"] == true { continue; }
                                for f in v["data"]["fills"].as_array().into_iter().flatten() {
                                    let coin = f["coin"].as_str().unwrap_or("");
                                    let st = coins.get(coin);
                                    ev(json!({"what": "fill", "coin": coin, "oid": f["oid"], "tid": f["tid"], "t_ms": f["time"], "px": num(&f["px"]), "sz": num(&f["sz"]),
                                        "side": f["side"], "dir": f["dir"], "crossed": f["crossed"], "fee": num(&f["fee"]), "closed_pnl": num(&f["closedPnl"]),
                                        "mark": st.map(|s| s.mark).filter(|x| *x > 0.0), "oracle": st.map(|s| s.oracle).filter(|x| *x > 0.0),
                                        "ctx_age_s": st.filter(|s| s.ctx_at > 0.0).map(|s| recv - s.ctx_at), "recv": recv}), "wick_ex");
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

/// A shadow's end: the backtest's result by the trades, and what the mark did over it.
fn result(s: &Shadow, why: &str, exit_px: f64, at_ms: u64, stop: f64) -> Value {
    json!({"what": "shadow_result", "coin": s.coin, "t_fill_ms": s.t_ms, "level": s.level, "why": why, "exit_px": exit_px, "t_exit_ms": at_ms,
        "ret": exit_px / s.level - 1.0, "trade_min_ret": s.trade_min / s.level - 1.0,
        "mark_min_ret": (s.mark_min < f64::MAX).then(|| s.mark_min / s.level - 1.0), "mark_stop_ms": s.mark_stop_ms,
        "stop_by_trade_not_mark": why == "stop" && s.mark_stop_ms.is_none(), "stop_level": s.level * (1.0 - stop)})
}
