//! The stops by the price (2026-11-18, the user's call): the exchange triggers a resting stop by the MARK price, and a
//! HIP-3 dex's mark follows its deployer's oracle, which can trail the trades by minutes (io:PRL 2026-11-18: the stop
//! -10% at 1.644, the trades through it from 08:14, the mark from 08:27: out at 1.555, -14.9%, -$280 for the $187
//! planned). So every `POLL_S` the account's resting stops (reduce-only trigger orders) are held against the dexes'
//! MIDS: a stop whose mid stayed through its trigger for `CONFIRM_S` and still rests on the exchange (not triggered)
//! goes out at market here (reduce-only, its size), its order cancelled, the oid kept (`closed`) so the strategy's
//! sweep reads that stop done (`trades::order_done`): its trade ends "stop" at the close's fills. The exchange's stop
//! stays as it was until then: the backstop while the bot is down. The stops read again every `READ_S` (the positions
//! first: only the dexes holding one read for their orders), and at once after an exit.

use std::collections::{HashMap, HashSet};
#[cfg(not(test))]
use std::sync::Mutex;

use anyhow::Result;
use serde_json::{Value, json};

use crate::api::now;
use crate::log;
use crate::store::Store;
use crate::venue::Venue;

const POLL_S: f64 = 5.0;
const READ_S: f64 = 30.0;
/// The mid through the trigger this long (two polls) before the exit: a single print of a thin book is not acted on.
const CONFIRM_S: f64 = 4.0;

/// The stops closed here: oid -> the exit's price (a test's own: the fakes' oids repeat from test to test).
#[cfg(not(test))]
static CLOSED: Mutex<Option<HashMap<u64, f64>>> = Mutex::new(None);
#[cfg(test)]
thread_local! {
    static CLOSED_T: std::cell::RefCell<HashMap<u64, f64>> = std::cell::RefCell::new(HashMap::new());
}

/// The stop `oid` closed here by the price (its exit's price).
pub(crate) fn closed(oid: u64) -> Option<f64> {
    #[cfg(not(test))]
    return CLOSED.lock().unwrap().as_ref().and_then(|m| m.get(&oid).copied());
    #[cfg(test)]
    return CLOSED_T.with(|m| m.borrow().get(&oid).copied());
}

pub(crate) fn mark_closed(oid: u64, px: f64) {
    #[cfg(not(test))]
    CLOSED.lock().unwrap().get_or_insert_with(HashMap::new).insert(oid, px);
    #[cfg(test)]
    CLOSED_T.with(|m| m.borrow_mut().insert(oid, px));
}

/// A resting stop: a reduce-only trigger order.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Stop {
    pub(crate) coin: String,
    pub(crate) oid: u64,
    /// A sell stop guards a long, a buy stop a short.
    pub(crate) sell: bool,
    pub(crate) sz: f64,
    pub(crate) trigger: f64,
}

impl Stop {
    /// The mid through the trigger: at or under it for a long's stop, at or over it for a short's.
    pub(crate) fn crossed(&self, mid: f64) -> bool {
        mid > 0.0 && if self.sell { mid <= self.trigger } else { mid >= self.trigger }
    }
}

fn num(v: &Value) -> f64 {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64()).unwrap_or(0.0)
}

/// The stops among a `frontendOpenOrders` reply: reduce-only trigger orders of a stop type (not a take-profit trigger).
pub(crate) fn stops_of(orders: &Value) -> Vec<Stop> {
    orders.as_array().into_iter().flatten()
        .filter(|o| o["reduceOnly"].as_bool() == Some(true) && o["isTrigger"].as_bool() == Some(true)
            && o["orderType"].as_str().is_none_or(|t| t.contains("Stop")))
        .filter_map(|o| Some(Stop { coin: o["coin"].as_str()?.to_string(), oid: o["oid"].as_u64()?, sell: o["side"].as_str()? == "A",
            sz: num(&o["sz"]), trigger: num(&o["triggerPx"]) }))
        .filter(|s| s.sz > 0.0 && s.trigger > 0.0)
        .collect()
}

fn dex_of(coin: &str) -> &str {
    coin.split_once(':').map_or("", |x| x.0)
}

#[derive(Default)]
pub(crate) struct Watch {
    stops: Vec<Stop>,
    read_at: f64,
    /// A stop's oid -> when its mid was first seen through its trigger.
    seen: HashMap<u64, f64>,
}

impl Watch {
    /// The resting stops read again: the positions on the main dex and `dexes`, then the orders of the dexes holding one.
    async fn read(&mut self, v: &Venue, dexes: &[String]) -> Result<()> {
        let mut held = HashSet::new();
        for d in std::iter::once("").chain(dexes.iter().map(String::as_str)) {
            if v.dex_positions(d).await?.values().any(|x| *x != 0.0) {
                held.insert(d.to_string());
            }
        }
        let mut stops = Vec::new();
        for d in &held {
            let q = if d.is_empty() { json!({"type": "frontendOpenOrders", "user": v.account}) } else { json!({"type": "frontendOpenOrders", "user": v.account, "dex": d}) };
            stops.extend(stops_of(&v.ex.info(q).await?));
        }
        let live: HashSet<u64> = stops.iter().map(|s| s.oid).collect();
        self.seen.retain(|o, _| live.contains(o));
        self.stops = stops;
        self.read_at = now();
        Ok(())
    }

    /// One poll: the stops read when due, the mids of their dexes, the stops through their trigger for `CONFIRM_S` closed.
    pub(crate) async fn round(&mut self, v: &Venue, dexes: &[String], store: &Store) {
        if now() - self.read_at >= READ_S {
            if let Err(e) = self.read(v, dexes).await {
                log!("pxstop: stops not read: {e:#}");
                return;
            }
        }
        if self.stops.is_empty() {
            return;
        }
        let mut mids: HashMap<String, f64> = HashMap::new();
        for d in self.stops.iter().map(|s| dex_of(&s.coin).to_string()).collect::<HashSet<_>>() {
            let q = if d.is_empty() { json!({"type": "allMids"}) } else { json!({"type": "allMids", "dex": d}) };
            match v.ex.info(q).await {
                Ok(m) => mids.extend(m.as_object().into_iter().flatten().map(|(c, x)| (c.clone(), num(x)))),
                Err(e) => log!("pxstop: mids{} not read: {e:#}", if d.is_empty() { String::new() } else { format!(" ({d})") }),
            }
        }
        let t = now();
        let mut exited = false;
        for s in self.stops.clone() {
            let Some(&mid) = mids.get(&s.coin) else { continue };
            if !s.crossed(mid) {
                self.seen.remove(&s.oid);
                continue;
            }
            let first = *self.seen.entry(s.oid).or_insert(t);
            if t - first < CONFIRM_S {
                continue;
            }
            exited |= self.exit(v, &s, mid, store).await;
        }
        if exited {
            self.read_at = 0.0;
        }
    }

    /// The stop `s` out at market (its mid `mid` through the trigger): only while its order still rests (not triggered).
    async fn exit(&mut self, v: &Venue, s: &Stop, mid: f64, store: &Store) -> bool {
        // a coin the venue does not hold (listed since its read): the exchange's own stop stays, nothing ordered
        if !v.coins.contains_key(&s.coin) {
            log!("pxstop: {} not on the venue: its stop {} left to the exchange", s.coin, s.oid);
            return false;
        }
        match v.order_state(s.oid).await {
            Ok((st, _)) if st == "open" => {}
            Ok(_) => {
                self.seen.remove(&s.oid);
                return true;
            }
            Err(e) => {
                log!("pxstop: {} stop {} status not read: {e:#}", s.coin, s.oid);
                return false;
            }
        }
        let size = if s.sell { -s.sz } else { s.sz };
        let r = v.market(&s.coin, size, true).await;
        v.cancel(&s.coin, s.oid).await;
        self.seen.remove(&s.oid);
        let mut ev = json!({"kind": "pxstop", "at": now(), "coin": s.coin, "oid": s.oid, "trigger": s.trigger, "mid": mid, "size": size});
        match r {
            Ok((_, px, _)) => {
                mark_closed(s.oid, px);
                ev["what"] = json!("exit");
                ev["px"] = json!(px);
                crate::notify::send(format!("🛑 {}: цена {} прошла стоп {} (биржа его по mark-цене ещё не сработала): закрыто по рынку {} {}",
                    s.coin, mid, s.trigger, s.sz, px));
            }
            Err(e) => {
                ev["what"] = json!("error");
                ev["error"] = json!(format!("{e:#}"));
                crate::notify::send(format!("🚨 {}: цена {} прошла стоп {}, закрыть по рынку не вышло: {e:#}. Проверь руками.", s.coin, mid, s.trigger));
            }
        }
        log!("pxstop: {ev}");
        store.event(ev);
        true
    }
}

/// The watch on the live account, every `POLL_S` (never in the way of the trading: its errors only logged).
pub(crate) async fn start(store: Store) -> Result<()> {
    let Some((base, _)) = crate::runner::venue("pxstop").await? else {
        return Ok(());
    };
    crate::runner::born("pxstop", POLL_S);
    let (sub, store2) = (crate::sub::venue(&base)?, store.clone());
    tokio::spawn(async move {
        let mut w = Watch::default();
        let mut dexes: (f64, Vec<String>) = (f64::MIN, Vec::new());
        let mut venue = base.clone();
        let mut iv = tokio::time::interval(std::time::Duration::from_secs_f64(POLL_S));
        loop {
            iv.tick().await;
            let t0 = now();
            if t0 - dexes.0 >= 3600.0 {
                dexes = (t0, crate::venue::usdc_dexes(&base.ex).await);
                venue = crate::runner::with_hip3("pxstop", &base).await;
            }
            w.round(&venue, &dexes.1, &store).await;
            crate::runner::beat("pxstop", t0);
        }
    });
    // the shorts' sub-account (`sub`): its stops watched the same way (its main-dex coins), on its own loop
    crate::runner::born("pxstop_short", POLL_S);
    tokio::spawn(async move {
        let mut w = Watch::default();
        let mut iv = tokio::time::interval(std::time::Duration::from_secs_f64(POLL_S));
        loop {
            iv.tick().await;
            let t0 = now();
            w.round(&sub, &[], &store2).await;
            crate::runner::beat("pxstop_short", t0);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Clock, FakeHl};

    #[test]
    fn a_stop_is_crossed_by_the_mid_on_its_side() {
        let long = Stop { coin: "X".into(), oid: 1, sell: true, sz: 1.0, trigger: 10.0 };
        assert!(long.crossed(10.0) && long.crossed(9.0) && !long.crossed(10.1) && !long.crossed(0.0));
        let short = Stop { sell: false, ..long };
        assert!(short.crossed(10.0) && short.crossed(11.0) && !short.crossed(9.9));
    }

    #[test]
    fn the_stops_are_the_reduce_only_stop_triggers() {
        let o = json!([
            {"coin": "A", "oid": 1, "side": "A", "sz": "2", "triggerPx": "9", "reduceOnly": true, "isTrigger": true, "orderType": "Stop Market"},
            {"coin": "B", "oid": 2, "side": "A", "sz": "2", "triggerPx": "11", "reduceOnly": true, "isTrigger": true, "orderType": "Take Profit Market"},
            {"coin": "C", "oid": 3, "side": "A", "sz": "2", "limitPx": "11", "reduceOnly": true, "isTrigger": false, "orderType": "Limit"},
            {"coin": "D", "oid": 4, "side": "B", "sz": "2", "triggerPx": "9", "reduceOnly": false, "isTrigger": true, "orderType": "Stop Market"},
            {"coin": "E", "oid": 5, "side": "B", "sz": "3", "triggerPx": "12", "reduceOnly": true, "isTrigger": true, "orderType": "Stop Limit"}]);
        assert_eq!(stops_of(&o), vec![Stop { coin: "A".into(), oid: 1, sell: true, sz: 2.0, trigger: 9.0 },
            Stop { coin: "E".into(), oid: 5, sell: false, sz: 3.0, trigger: 12.0 }]);
    }

    #[tokio::test]
    async fn a_stop_the_mark_trails_goes_out_at_market_by_the_mid() {
        let c = Clock::at(1_000_000.0);
        let hl = FakeHl::start(&[("WTA", 0, 10, 100.0), ("xyz:WTB", 1, 10, 50.0)]).await;
        let v = hl.venue();
        let (store, mut rx) = Store::for_test();
        // a long on the HIP-3 coin with its stop -10%, a long on the main dex with its stop
        v.market("xyz:WTB", 2.0, false).await.unwrap();
        let b = v.stop("xyz:WTB", 1.0, 2.0, 45.0).await.unwrap();
        v.market("WTA", 1.0, false).await.unwrap();
        let a = v.stop("WTA", 1.0, 1.0, 90.0).await.unwrap();
        // the mark stays: the exchange's stop does not trigger while the trades go through it
        hl.set_mark("xyz:WTB", 50.0);
        let dexes = vec!["xyz".to_string()];
        let mut w = Watch::default();
        w.round(&v, &dexes, &store).await;
        assert_eq!(w.stops.len(), 2);
        // the mid through the stop: seen, not acted on at once (a single print)
        hl.set_mid("xyz:WTB", 44.0);
        w.round(&v, &dexes, &store).await;
        assert_eq!(hl.position("xyz:WTB"), 2.0);
        // back over it: forgotten; through again, held CONFIRM_S: out at market, its stop cancelled, the oid kept
        hl.set_mid("xyz:WTB", 46.0);
        c.go(5.0);
        w.round(&v, &dexes, &store).await;
        hl.set_mid("xyz:WTB", 44.0);
        c.go(5.0);
        w.round(&v, &dexes, &store).await;
        assert_eq!(hl.position("xyz:WTB"), 2.0);
        c.go(5.0);
        w.round(&v, &dexes, &store).await;
        assert_eq!(hl.position("xyz:WTB"), 0.0);
        assert!(hl.open_orders("xyz:WTB").is_empty());
        assert_eq!(closed(b), Some(44.0));
        assert!(crate::trades::order_done(&v, b).await);
        // the strategy's sweep reads its trade ended by its stop, at the close's price
        let tr = crate::trades::Trade { coin: "xyz:WTB".into(), size: 2.0, entry_px: 50.0, at: 999_000.0, exit_at: 2e9, stop_px: 45.0, stop_oid: Some(b), ..Default::default() };
        assert_eq!(crate::trades::gone(&v, &tr, 0.0, "test").await, Some(("stop", 44.0)));
        let ev = crate::store::drain(&mut rx);
        assert_eq!(ev.len(), 1);
        assert_eq!((ev[0]["kind"].as_str(), ev[0]["what"].as_str(), ev[0]["coin"].as_str()), (Some("pxstop"), Some("exit"), Some("xyz:WTB")));
        // the main coin untouched; its stop read again after the exit
        assert_eq!(hl.position("WTA"), 1.0);
        assert_eq!(w.read_at, 0.0);
        w.round(&v, &dexes, &store).await;
        assert_eq!(w.stops.iter().map(|s| s.oid).collect::<Vec<_>>(), vec![a]);
    }

    #[tokio::test]
    async fn a_stop_the_exchange_triggered_is_left_to_it() {
        let c = Clock::at(2_000_000.0);
        let hl = FakeHl::start(&[("WTC", 0, 10, 100.0)]).await;
        let v = hl.venue();
        let (store, mut rx) = Store::for_test();
        v.market("WTC", -1.0, false).await.unwrap();
        let s = v.stop("WTC", -1.0, 1.0, 110.0).await.unwrap();
        let mut w = Watch::default();
        w.round(&v, &[], &store).await;
        assert_eq!(w.stops.len(), 1);
        // a short's stop: the mid over it, the mark too: the exchange's stop fills; ours finds it no longer resting
        hl.set_mid("WTC", 111.0);
        c.go(5.0);
        w.round(&v, &[], &store).await;
        assert_eq!(hl.position("WTC"), 0.0);
        assert_eq!(closed(s), None);
        // held CONFIRM_S: its order no longer rests (triggered): nothing sent, nothing told
        c.go(5.0);
        w.round(&v, &[], &store).await;
        assert_eq!(closed(s), None);
        assert!(crate::store::drain(&mut rx).is_empty());
        // nothing held: no stops, no mids asked
        c.go(31.0);
        w.round(&v, &[], &store).await;
        assert!(w.stops.is_empty());
    }

    /// 2026-11-18: the live venue held the main dex's coins only; an order on a HIP-3 coin would have panicked the loop.
    /// A coin the venue does not hold: its stop left to the exchange, nothing ordered, no panic.
    #[tokio::test]
    async fn a_stop_on_a_coin_the_venue_does_not_hold_is_left_to_the_exchange() {
        let _c = Clock::at(2_100_000.0);
        let hl = FakeHl::start(&[("xyz:WTD", 1, 10, 50.0)]).await;
        let full = hl.venue();
        let (store, mut rx) = Store::for_test();
        let main_only = Venue::for_test(full.ex.clone(), &full.account, HashMap::new(), HashMap::new());
        let mut w = Watch::default();
        let s = Stop { coin: "xyz:WTD".into(), oid: 77, sell: true, sz: 1.0, trigger: 49.0 };
        assert!(!w.exit(&main_only, &s, 40.0, &store).await);
        assert!(hl.with(|s| s.actions.is_empty()), "nothing sent");
        assert!(crate::store::drain(&mut rx).is_empty());
    }
}
