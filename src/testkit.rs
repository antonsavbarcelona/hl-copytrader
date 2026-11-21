//! A fake Hyperliquid for the end-to-end tests: a local HTTP server on 127.0.0.1 that answers `/info` and
//! `/exchange` as the real API does for the calls the bot makes, with a toy matching engine behind them:
//! mids the test sets, resting limits that fill when the mid crosses them, stop-market triggers, IOC orders
//! filled at the mid, reduce-only clipped to the position, positions and their fills. Faults on demand:
//! an order's status that lags (stale "open" for N reads), the cross leverage refused (a HIP-3 coin kept
//! isolated), stops refused, IOC orders that only match beyond a slippage, a margin refusal.
//! The bot's own code (`venue::Venue`, `trades::*`) runs against it unchanged: `Exchange::new(base, key)`.
#![cfg(test)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::config::Config;
use crate::exchange::Exchange;
use crate::venue::Venue;

/// A throwaway key (no account behind it): the fake does not check signatures.
pub(crate) const KEY: &str = "0123456789012345678901234567890123456789012345678901234567890123";
pub(crate) const ACCOUNT: &str = "0x00000000000000000000000000000000000000aa";

#[derive(Clone, Debug)]
pub(crate) struct Order {
    pub(crate) oid: u64,
    pub(crate) coin: String,
    pub(crate) buy: bool,
    pub(crate) px: f64,
    pub(crate) orig: f64,
    pub(crate) left: f64,
    pub(crate) reduce: bool,
    /// Some(trigger): a stop-market order.
    pub(crate) trigger: Option<f64>,
    /// "open", "filled", "canceled", "triggered".
    pub(crate) status: String,
}

#[derive(Default)]
pub(crate) struct State {
    /// asset id -> (coin, size decimals, max leverage)
    pub(crate) assets: HashMap<u32, (String, u32, u32)>,
    pub(crate) mids: HashMap<String, f64>,
    /// coin -> its mark when set apart from the mid (a HIP-3 oracle trailing the trades): the triggers go by it
    pub(crate) marks: HashMap<String, f64>,
    /// coin -> its category (perpAnnotation; "stocks" when not set)
    pub(crate) categories: HashMap<String, String>,
    /// (coin, interval) -> its candles as the API sends them ({"t": open ms, "o", "h", "l", "c", "v"})
    pub(crate) candles: HashMap<(String, String), Vec<Value>>,
    /// coin -> its day's notional volume (metaAndAssetCtxs' dayNtlVlm)
    pub(crate) volumes: HashMap<String, f64>,
    pub(crate) orders: HashMap<u64, Order>,
    /// coin -> (signed size, entry)
    pub(crate) positions: HashMap<String, (f64, f64)>,
    pub(crate) fills: Vec<Value>,
    pub(crate) next_oid: u64,
    /// every /exchange action, as sent
    pub(crate) actions: Vec<Value>,
    // --- faults ---
    /// oid -> how many more orderStatus reads answer the stale "open, nothing filled"
    pub(crate) status_lag: HashMap<u64, u32>,
    /// the next resting order placed gets this many stale status reads
    pub(crate) lag_next_order: u32,
    pub(crate) refuse_cross: HashSet<String>,
    pub(crate) refuse_isolated: HashSet<String>,
    pub(crate) refuse_stops: bool,
    /// IOC orders match only when their limit is at least this far through the mid
    pub(crate) ioc_needs: f64,
    /// the next opening IOC is refused for margin
    pub(crate) margin_refusals: u32,
    /// the next exchange replies lost after the action took effect (a timeout, a proxy's 502)
    pub(crate) lose_replies: u32,
    /// the reply of the n-th order action from now lost (1: the next), after it took effect
    pub(crate) lose_order: u32,
    /// a modify keeps the order's oid (else the old one is cancelled and a new oid placed); its reply without
    /// the statuses ({"type": "default"})
    pub(crate) modify_keeps_oid: bool,
    pub(crate) modify_bare_reply: bool,
    pub(crate) leverage: HashMap<String, (u32, bool)>,
    pub(crate) now_ms: u64,
    /// The account's value (clearinghouseState's accountValue; none: $1000).
    pub(crate) account_value: Option<f64>,
    /// every /exchange body, as sent (the vault address, the signature)
    pub(crate) bodies: Vec<Value>,
    /// the sends (`sendAsset`) refused
    pub(crate) refuse_sends: bool,
}

#[derive(Clone)]
pub(crate) struct FakeHl {
    pub(crate) st: Arc<Mutex<State>>,
    pub(crate) base: String,
}

fn n(x: &Value) -> f64 {
    match x {
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Number(v) => v.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

impl State {
    fn coin_of(&self, a: u32) -> String {
        self.assets.get(&a).map(|x| x.0.clone()).unwrap_or_default()
    }

    fn fill(&mut self, oid: u64, coin: &str, buy: bool, px: f64, sz: f64, crossed: bool) {
        let d = if buy { sz } else { -sz };
        let (old, entry) = self.positions.get(coin).copied().unwrap_or((0.0, 0.0));
        let new = old + d;
        let dir = if old == 0.0 || old * d > 0.0 {
            if buy { "Open Long" } else { "Open Short" }
        } else if old > 0.0 {
            "Close Long"
        } else {
            "Close Short"
        };
        let entry = if old * d > 0.0 { (old * entry + d * px) / new } else if new * old < 0.0 || old == 0.0 { px } else { entry };
        if new.abs() < 1e-12 {
            self.positions.remove(coin);
        } else {
            self.positions.insert(coin.to_string(), (new, entry));
        }
        self.now_ms += 1;
        self.fills.push(json!({"coin": coin, "px": px.to_string(), "sz": sz.to_string(), "side": if buy { "B" } else { "A" },
            "time": self.now_ms, "dir": dir, "oid": oid, "crossed": crossed}));
    }

    /// A reduce-only order's size clipped to what closes the position (0: nothing to reduce).
    fn reducible(&self, coin: &str, buy: bool, sz: f64) -> f64 {
        let pos = self.positions.get(coin).map_or(0.0, |x| x.0);
        if (buy && pos < 0.0) || (!buy && pos > 0.0) { sz.min(pos.abs()) } else { 0.0 }
    }

    /// The resting orders and the triggers against the mids now.
    pub(crate) fn sweep(&mut self) {
        let mut ids: Vec<u64> = self.orders.iter().filter(|(_, o)| o.status == "open").map(|(k, _)| *k).collect();
        ids.sort();
        for oid in ids {
            let o = self.orders[&oid].clone();
            let Some(&mid) = self.mids.get(&o.coin) else { continue };
            if let Some(trig) = o.trigger {
                let mark = self.marks.get(&o.coin).copied().unwrap_or(mid);
                let hit = if o.buy { mark >= trig } else { mark <= trig };
                if hit {
                    let sz = if o.reduce { self.reducible(&o.coin, o.buy, o.left) } else { o.left };
                    if sz > 0.0 {
                        self.fill(oid, &o.coin, o.buy, mid, sz, true);
                    }
                    let x = self.orders.get_mut(&oid).unwrap();
                    x.status = "triggered".into();
                    x.left = 0.0;
                }
                continue;
            }
            let hit = if o.buy { mid <= o.px } else { mid >= o.px };
            if hit {
                let sz = if o.reduce { self.reducible(&o.coin, o.buy, o.left) } else { o.left };
                if sz > 0.0 {
                    self.fill(oid, &o.coin, o.buy, o.px, sz, false);
                }
                let x = self.orders.get_mut(&oid).unwrap();
                x.left -= sz;
                if x.left <= 1e-12 || o.reduce {
                    x.left = x.left.max(0.0);
                    x.status = if sz > 0.0 { "filled".into() } else { "canceled".into() };
                }
            }
        }
    }

    fn order_json(&self, o: &Order) -> Value {
        json!({"coin": o.coin, "side": if o.buy { "B" } else { "A" }, "limitPx": o.px.to_string(), "sz": o.left.to_string(),
            "origSz": o.orig.to_string(), "oid": o.oid, "timestamp": self.now_ms, "reduceOnly": o.reduce,
            "isTrigger": o.trigger.is_some(), "triggerPx": o.trigger.unwrap_or(0.0).to_string(),
            "orderType": if o.trigger.is_some() { "Stop Market" } else { "Limit" }})
    }

    fn info(&mut self, q: &Value) -> Value {
        let dex = q["dex"].as_str().unwrap_or("");
        let on_dex = |c: &str| if dex.is_empty() { !c.contains(':') } else { c.starts_with(&format!("{dex}:")) };
        match q["type"].as_str().unwrap_or("") {
            "meta" => {
                let mut u: Vec<(u32, Value)> = self.assets.iter().filter(|(_, x)| on_dex(&x.0))
                    .map(|(a, x)| (*a, json!({"name": x.0, "szDecimals": x.1, "maxLeverage": x.2}))).collect();
                u.sort_by_key(|x| x.0);
                json!({"universe": u.into_iter().map(|x| x.1).collect::<Vec<_>>(), "collateralToken": 0})
            }
            // a coin's category: "stocks" unless set (`categories`)
            "perpAnnotation" => json!({"category": self.categories.get(q["coin"].as_str().unwrap_or("")).map_or("stocks", String::as_str), "description": ""}),
            "metaAndAssetCtxs" => {
                let mut u: Vec<(u32, String, u32, u32)> = self.assets.iter().filter(|(_, x)| on_dex(&x.0)).map(|(a, x)| (*a, x.0.clone(), x.1, x.2)).collect();
                u.sort_by_key(|x| x.0);
                let ctx: Vec<Value> = u.iter().map(|x| json!({"dayNtlVlm": self.volumes.get(&x.1).copied().unwrap_or(0.0).to_string(),
                    "funding": "0", "markPx": self.mids.get(&x.1).copied().unwrap_or(0.0).to_string()})).collect();
                json!([{"universe": u.iter().map(|x| json!({"name": x.1, "szDecimals": x.2, "maxLeverage": x.3})).collect::<Vec<_>>()}, ctx])
            }
            "candleSnapshot" => {
                let r = &q["req"];
                let key = (r["coin"].as_str().unwrap_or("").to_string(), r["interval"].as_str().unwrap_or("").to_string());
                let (from, to) = (r["startTime"].as_f64().unwrap_or(0.0), r["endTime"].as_f64().unwrap_or(f64::MAX));
                Value::Array(self.candles.get(&key).into_iter().flatten().filter(|c| {
                    let t = c["t"].as_f64().unwrap_or(0.0);
                    t >= from && t <= to
                }).cloned().collect())
            }
            "allMids" => Value::Object(self.mids.iter().filter(|(c, _)| on_dex(c)).map(|(c, m)| (c.clone(), json!(m.to_string()))).collect()),
            "perpDexs" => {
                // the main dex (null), then the HIP-3 dexes of the coins ("xyz:A" -> "xyz")
                let mut d: Vec<String> = self.assets.values().filter_map(|x| x.0.split_once(':').map(|(d, _)| d.to_string())).collect();
                d.sort();
                d.dedup();
                Value::Array(std::iter::once(Value::Null).chain(d.into_iter().map(|n| json!({"name": n}))).collect())
            }
            "userAbstraction" => json!("default"),
            "orderStatus" => {
                let oid = q["oid"].as_u64().unwrap_or(0);
                let Some(o) = self.orders.get(&oid).cloned() else { return json!({"status": "unknownOid"}) };
                if let Some(k) = self.status_lag.get_mut(&oid).filter(|k| **k > 0) {
                    *k -= 1;
                    let stale = Order { left: o.orig, status: "open".into(), ..o };
                    return json!({"status": "order", "order": {"order": self.order_json(&stale), "status": "open", "statusTimestamp": self.now_ms}});
                }
                json!({"status": "order", "order": {"order": self.order_json(&o), "status": o.status, "statusTimestamp": self.now_ms}})
            }
            "openOrders" | "frontendOpenOrders" => {
                let mut v: Vec<&Order> = self.orders.values().filter(|o| o.status == "open" && on_dex(&o.coin)).collect();
                v.sort_by_key(|o| o.oid);
                Value::Array(v.into_iter().map(|o| self.order_json(o)).collect())
            }
            "clearinghouseState" => {
                let ps: Vec<Value> = self.positions.iter().filter(|(c, _)| on_dex(c))
                    .map(|(c, (s, e))| json!({"position": {"coin": c, "szi": s.to_string(), "entryPx": e.to_string(),
                        "positionValue": (s.abs() * self.mids.get(c).copied().unwrap_or(*e)).to_string(), "unrealizedPnl": "0"}})).collect();
                let v = self.account_value.unwrap_or(1000.0).to_string();
                json!({"assetPositions": ps, "marginSummary": {"accountValue": v, "totalMarginUsed": "0"}, "withdrawable": v})
            }
            "userFillsByTime" | "userFills" => Value::Array(self.fills.clone()),
            "l2Book" => {
                let c = q["coin"].as_str().unwrap_or("");
                let m = self.mids.get(c).copied().unwrap_or(0.0);
                json!({"coin": c, "time": self.now_ms, "levels": [[{"px": (m * 0.999).to_string(), "sz": "1000", "n": 1}], [{"px": (m * 1.001).to_string(), "sz": "1000", "n": 1}]]})
            }
            t => json!({"error": format!("the fake has no info type {t}")}),
        }
    }

    fn exchange(&mut self, body: &Value) -> Value {
        let a = &body["action"];
        self.actions.push(a.clone());
        self.bodies.push(body.clone());
        match a["type"].as_str().unwrap_or("") {
            // the fake's account is the sub-account: a send to it (none from a sub-account) adds to its value, one from it takes off
            "sendAsset" => {
                if self.refuse_sends {
                    return json!({"status": "err", "response": "Must deposit before performing actions. (the fake's fault)"});
                }
                let x = n(&a["amount"]) * if a["fromSubAccount"].as_str().unwrap_or("").is_empty() { 1.0 } else { -1.0 };
                self.account_value = Some(self.account_value.unwrap_or(1000.0) + x);
                json!({"status": "ok", "response": {"type": "default"}})
            }
            "updateLeverage" => {
                let coin = self.coin_of(a["asset"].as_u64().unwrap_or(0) as u32);
                let cross = a["isCross"].as_bool().unwrap_or(true);
                if (cross && self.refuse_cross.contains(&coin)) || (!cross && self.refuse_isolated.contains(&coin)) {
                    return json!({"status": "err", "response": "Cross margin is not allowed for this asset."});
                }
                self.leverage.insert(coin, (a["leverage"].as_u64().unwrap_or(1) as u32, cross));
                json!({"status": "ok", "response": {"type": "default"}})
            }
            "cancel" => {
                let mut st = Vec::new();
                for c in a["cancels"].as_array().into_iter().flatten() {
                    let oid = c["o"].as_u64().unwrap_or(0);
                    match self.orders.get_mut(&oid) {
                        Some(o) if o.status == "open" => {
                            o.status = "canceled".into();
                            st.push(json!("success"));
                        }
                        _ => st.push(json!({"error": "Order was never placed, already canceled, or filled."})),
                    }
                }
                json!({"status": "ok", "response": {"type": "cancel", "data": {"statuses": st}}})
            }
            "order" => {
                let mut st = Vec::new();
                for w in a["orders"].as_array().into_iter().flatten() {
                    st.push(self.place(w));
                }
                json!({"status": "ok", "response": {"type": "order", "data": {"statuses": st}}})
            }
            "modify" => {
                let oid = a["oid"].as_u64().unwrap_or(0);
                let st = match self.orders.get(&oid).cloned() {
                    Some(o) if o.status == "open" => {
                        let w = &a["order"];
                        let (px, sz) = (n(&w["p"]), n(&w["s"]));
                        let mid = self.mids.get(&o.coin).copied().unwrap_or(0.0);
                        let tif = w["t"]["limit"]["tif"].as_str().unwrap_or("Gtc");
                        if tif == "Alo" && (if o.buy { px >= mid } else { px <= mid }) {
                            json!({"error": "Post only order would have immediately matched, bbo was 1. asset=0"})
                        } else if self.modify_keeps_oid {
                            let x = self.orders.get_mut(&oid).unwrap();
                            (x.px, x.orig, x.left) = (px, sz, sz);
                            json!({"resting": {"oid": oid}})
                        } else {
                            // the old one off (whatever of it filled stays filled), the new order placed
                            self.orders.get_mut(&oid).unwrap().status = "canceled".into();
                            self.place(w)
                        }
                    }
                    _ => json!({"error": "Cannot modify canceled or filled order"}),
                };
                if self.modify_bare_reply && st.get("error").is_none() {
                    json!({"status": "ok", "response": {"type": "default"}})
                } else {
                    json!({"status": "ok", "response": {"type": "order", "data": {"statuses": [st]}}})
                }
            }
            t => json!({"status": "err", "response": format!("the fake has no action {t}")}),
        }
    }

    fn place(&mut self, w: &Value) -> Value {
        let coin = self.coin_of(w["a"].as_u64().unwrap_or(0) as u32);
        let Some(&mid) = self.mids.get(&coin) else { return json!({"error": format!("no mid for {coin}")}) };
        let (buy, px, sz, reduce) = (w["b"].as_bool().unwrap_or(false), n(&w["p"]), n(&w["s"]), w["r"].as_bool().unwrap_or(false));
        self.next_oid += 1;
        let oid = self.next_oid;
        if let Some(t) = w["t"].get("trigger") {
            if self.refuse_stops {
                return json!({"error": "Trigger orders are refused (the fake's fault)."});
            }
            self.orders.insert(oid, Order { oid, coin, buy, px, orig: sz, left: sz, reduce, trigger: Some(n(&t["triggerPx"])), status: "open".into() });
            return json!({"resting": {"oid": oid}});
        }
        let tif = w["t"]["limit"]["tif"].as_str().unwrap_or("Gtc").to_string();
        let crosses = if buy { px >= mid } else { px <= mid };
        if tif == "Ioc" {
            if !reduce && self.margin_refusals > 0 {
                self.margin_refusals -= 1;
                return json!({"error": "Insufficient margin to place order. asset=0"});
            }
            let through = if buy { px / mid - 1.0 } else { 1.0 - px / mid };
            if !crosses || through < self.ioc_needs {
                return json!({"error": "Order could not immediately match against any resting orders. asset=0"});
            }
            let sz = if reduce { self.reducible(&coin, buy, sz) } else { sz };
            if sz <= 0.0 {
                return json!({"error": "Reduce only order would increase position. asset=0"});
            }
            self.orders.insert(oid, Order { oid, coin: coin.clone(), buy, px, orig: sz, left: 0.0, reduce, trigger: None, status: "filled".into() });
            self.fill(oid, &coin, buy, mid, sz, true);
            return json!({"filled": {"totalSz": sz.to_string(), "avgPx": mid.to_string(), "oid": oid}});
        }
        if crosses {
            if tif == "Alo" {
                return json!({"error": "Post only order would have immediately matched, bbo was 1. asset=0"});
            }
            self.orders.insert(oid, Order { oid, coin: coin.clone(), buy, px, orig: sz, left: 0.0, reduce, trigger: None, status: "filled".into() });
            self.fill(oid, &coin, buy, mid, sz, true);
            return json!({"filled": {"totalSz": sz.to_string(), "avgPx": mid.to_string(), "oid": oid}});
        }
        self.orders.insert(oid, Order { oid, coin, buy, px, orig: sz, left: sz, reduce, trigger: None, status: "open".into() });
        if self.lag_next_order > 0 {
            self.status_lag.insert(oid, self.lag_next_order);
            self.lag_next_order = 0;
        }
        json!({"resting": {"oid": oid}})
    }
}

impl FakeHl {
    /// A fake with these coins: (name, size decimals, max leverage, mid); HIP-3 names ("dex:NAME") get asset ids
    /// 110000 + their index, as the first USDC dex's would.
    pub(crate) async fn start(coins: &[(&str, u32, u32, f64)]) -> FakeHl {
        let mut st = State { next_oid: 1000, now_ms: 1_800_000_000_000, ..Default::default() };
        let (mut i_main, mut i_h3) = (0u32, 0u32);
        for (c, dec, lev, mid) in coins {
            let a = if c.contains(':') { i_h3 += 1; 110_000 + i_h3 - 1 } else { i_main += 1; i_main - 1 };
            st.assets.insert(a, (c.to_string(), *dec, *lev));
            st.mids.insert(c.to_string(), *mid);
        }
        let st = Arc::new(Mutex::new(st));
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let s2 = st.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else { return };
                let st = s2.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    loop {
                        // one request: the headers, then Content-Length bytes
                        let head_end = loop {
                            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break p + 4;
                            }
                            let mut chunk = [0u8; 8192];
                            match sock.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(k) => buf.extend_from_slice(&chunk[..k]),
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                        let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                        let len = head.lines().find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok()).flatten()
                        }).unwrap_or(0);
                        while buf.len() < head_end + len {
                            let mut chunk = [0u8; 8192];
                            match sock.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(k) => buf.extend_from_slice(&chunk[..k]),
                            }
                        }
                        let body: Value = serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Value::Null);
                        buf.drain(..head_end + len);
                        let out = {
                            let mut s = st.lock().unwrap();
                            let ex = path.ends_with("/exchange");
                            let v = if ex { s.exchange(&body) } else { s.info(&body) };
                            s.sweep();
                            let is_order = ex && body["action"]["type"] == "order";
                            let lose_order = is_order && s.lose_order > 0 && { s.lose_order -= 1; s.lose_order == 0 };
                            if lose_order || (ex && s.lose_replies > 0 && { s.lose_replies -= 1; true }) {
                                Value::String("<html>502 Bad Gateway</html>".into())
                            } else {
                                v
                            }
                        };
                        let text = match &out { Value::String(t) => t.clone(), o => o.to_string() };
                        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", text.len(), text);
                        if sock.write_all(resp.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        FakeHl { st, base }
    }

    /// A venue on the fake: the main-dex and HIP-3 coins as `venue::venue_from_env` and `with_hip3` read them.
    pub(crate) fn venue(&self) -> Venue {
        let ex = Exchange::new(&self.base, KEY).unwrap();
        let s = self.st.lock().unwrap();
        let coins = s.assets.iter().map(|(a, x)| (x.0.clone(), (*a, x.1))).collect();
        let max_lev = s.assets.values().map(|x| (x.0.clone(), x.2)).collect();
        Venue::for_test(ex, ACCOUNT, coins, max_lev)
    }

    /// The mid moves; the resting orders and the triggers it crosses fill.
    pub(crate) fn set_mid(&self, coin: &str, px: f64) {
        let mut s = self.st.lock().unwrap();
        s.mids.insert(coin.to_string(), px);
        s.sweep();
    }

    /// The mark held apart from the mid (the triggers go by it); the triggers it crosses fill at the mid.
    pub(crate) fn set_mark(&self, coin: &str, px: f64) {
        let mut s = self.st.lock().unwrap();
        s.marks.insert(coin.to_string(), px);
        s.sweep();
    }

    pub(crate) fn position(&self, coin: &str) -> f64 {
        self.st.lock().unwrap().positions.get(coin).map_or(0.0, |x| x.0)
    }

    pub(crate) fn open_orders(&self, coin: &str) -> Vec<Order> {
        let s = self.st.lock().unwrap();
        let mut v: Vec<Order> = s.orders.values().filter(|o| o.coin == coin && o.status == "open").cloned().collect();
        v.sort_by_key(|o| o.oid);
        v
    }

    pub(crate) fn order(&self, oid: u64) -> Option<Order> {
        self.st.lock().unwrap().orders.get(&oid).cloned()
    }

    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.st.lock().unwrap())
    }

    /// A resting order filled in part, `sz` of it at its price (it keeps resting for the rest).
    pub(crate) fn partial_fill(&self, oid: u64, sz: f64) {
        let mut s = self.st.lock().unwrap();
        let o = s.orders[&oid].clone();
        let sz = if o.reduce { s.reducible(&o.coin, o.buy, sz) } else { sz }.min(o.left);
        s.fill(oid, &o.coin, o.buy, o.px, sz, false);
        s.orders.get_mut(&oid).unwrap().left -= sz;
    }

    /// A position opened outside the bot (by hand / another tool): `size` at the mid.
    pub(crate) fn hand_trade(&self, coin: &str, size: f64) {
        let mut s = self.st.lock().unwrap();
        let mid = s.mids[coin];
        s.next_oid += 1;
        let oid = s.next_oid;
        s.fill(oid, coin, size > 0.0, mid, size.abs(), true);
    }
}

/// A test's clock: `api::now()` is `at` on this thread until it is dropped (then the system clock again); `go`
/// moves it on. Only for what reads `now()`: tokio's timers still run on their own time.
pub(crate) struct Clock;

impl Clock {
    pub(crate) fn at(t: f64) -> Clock {
        crate::api::TEST_NOW.with(|c| c.set(Some(t)));
        Clock
    }

    pub(crate) fn go(&self, s: f64) {
        crate::api::TEST_NOW.with(|c| c.set(Some(c.get().unwrap_or(0.0) + s)));
    }
}

impl Drop for Clock {
    fn drop(&mut self) {
        crate::api::TEST_NOW.with(|c| c.set(None));
    }
}

#[test]
fn the_test_clock_moves_and_goes_back_to_the_system_one() {
    let sys = crate::api::now();
    {
        let c = Clock::at(1000.0);
        assert_eq!(crate::api::now(), 1000.0);
        c.go(3600.0);
        assert_eq!(crate::api::now(), 4600.0);
    }
    assert!(crate::api::now() >= sys);
}

/// A run's settings for a test: no database, the states in a fresh temp folder.
pub(crate) fn cfg() -> Config {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!("hlct-test-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    Config { data_dir: dir, weight_per_min: 400.0, database_url: None, run_id: "test".into() }
}

/// A state document a strategy saved (the file `<name>.json` of `cfg`), waited for up to 2 s (saved in the background).
pub(crate) async fn saved(cfg: &Config, name: &str) -> Option<Value> {
    for _ in 0..40 {
        if let Ok(s) = std::fs::read_to_string(cfg.data_dir.join(format!("{name}.json"))) {
            return serde_json::from_str(&s).ok();
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    None
}

impl FakeHl {
    /// A coin's candles of `interval` ("1h", "4h", "1d"): (open time s, open, high, low, close, volume in coins).
    pub(crate) fn set_candles(&self, coin: &str, interval: &str, bars: &[(f64, f64, f64, f64, f64, f64)]) {
        let step = match interval { "1m" => 60.0, "5m" => 300.0, "15m" => 900.0, "1h" => 3600.0, "4h" => 14_400.0, "1d" => 86_400.0, _ => 0.0 };
        let v = bars.iter().map(|b| json!({"t": (b.0 * 1000.0) as u64, "T": ((b.0 + step) * 1000.0) as u64 - 1, "o": b.1.to_string(), "h": b.2.to_string(), "l": b.3.to_string(),
            "c": b.4.to_string(), "v": b.5.to_string()})).collect();
        self.with(|s| { s.candles.insert((coin.to_string(), interval.to_string()), v); });
    }

    /// The info API on the fake (`Api::at`).
    pub(crate) fn api(&self) -> crate::api::Api {
        crate::api::Api::at(&self.base)
    }

    pub(crate) fn set_volume(&self, coin: &str, usd: f64) {
        self.with(|s| { s.volumes.insert(coin.to_string(), usd); });
    }
}
