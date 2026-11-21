//! The bot's health, watched from inside (2026-11-17, the user's call: a check of every strategy, "дергать за ручки и
//! проверять что все работает как надо"). Every `EVERY_S`:
//! - each loop's pulse (`runner::pulses`: every strategy on the runner, the guard): a loop silent past `quiet_after`
//!   (dead: a panic ends its task without a word; or stuck in a step) is told to the chat;
//! - the account's positions (every USDC dex) against its resting orders: a position with no resting stop (a reduce-only
//!   trigger order) for `BARE_AFTER_S` gets one (`guard_stop`: `GUARD_STOP` from its entry, short of its liquidation) and
//!   is told to the chat (a fill gets its stop within a check, 15 s for the wick; 2026-11-18 xyz:SOXL: the wick's bid
//!   filled while the bot stood frozen, the new run knew nothing of it, the long sat bare, -7.5% at the worst);
//! - each told again every `AGAIN_S` while so, and once when it is fine again;
//! - the whole of it saved as the doc `health` (read by `tools/health.py`, with the events and the exchange).

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::api::now;
use crate::config::Config;
use crate::log;
use crate::runner::Pulse;
use crate::venue::Venue;

const EVERY_S: f64 = 60.0;
/// A loop silent this long is told (at least; `QUIET_TICKS` of its ticks when they are longer).
const QUIET_MIN_S: f64 = 600.0;
const QUIET_TICKS: f64 = 10.0;
/// A position without a resting stop this long is told.
const BARE_AFTER_S: f64 = 120.0;
/// A bare position's stop: this far from its entry, or `trades::LIQ_STOP_SHARE` of the way to its liquidation when that
/// is nearer (the families' widest, the dips' / the HIP-3 bids' 20%). Tried again this often while still bare.
const GUARD_STOP: f64 = 0.20;
const GUARD_AGAIN_S: f64 = 300.0;
/// Told again this often while so.
const AGAIN_S: f64 = 3600.0;
const DOC: &str = "health";
/// The shorts' sub-account's (`sub`).
const SUB_DOC: &str = "health_short";

/// The loops whose step is long by design, with the silence they get: the breakout's step reads every coin's candles
/// 2.5 s apart (its first, 900 a coin, took 476 s on 2026-11-17's deploy; each hour's about as long).
const SLOW: [(&str, f64); 1] = [("breakout", 1800.0)];

/// The silence after which the loop `name` ticking every `tick_s` is told.
pub(crate) fn quiet_after(name: &str, tick_s: f64) -> f64 {
    let slow = SLOW.iter().find(|x| x.0 == name).map_or(0.0, |x| x.1);
    QUIET_MIN_S.max(QUIET_TICKS * tick_s).max(slow)
}

/// How long the loop `name` has been silent at `t` (since its last tick done, or its start before any), when past its
/// `quiet_after`.
pub(crate) fn silent(name: &str, p: &Pulse, t: f64) -> Option<f64> {
    let s = t - if p.ticks > 0 { p.done } else { p.since };
    (s > quiet_after(name, p.tick_s)).then_some(s)
}

/// The whole bot still this long (no loop's tick done: the runtime stuck) -> the process ends, Railway starts it again
/// (`restartPolicyType = "ALWAYS"`). 2026-11-18 11:57 UTC: a lock taken twice stood every loop still for 2 h 36 min, the
/// process alive (no restart), the bids resting at 11:50's prices, one filled bare at the US open.
const FROZEN_S: f64 = 180.0;
/// One loop silent this many times its `quiet_after` -> the same (a step stuck; a loop dead by a panic).
const STUCK_TIMES: f64 = 2.0;
const WATCHDOG_EVERY_S: u64 = 15;

/// Why the process must end at `t`: every loop still `FROZEN_S`, or one silent `STUCK_TIMES` its `quiet_after`.
pub(crate) fn frozen(pulses: &[(&'static str, Pulse)], t: f64) -> Option<String> {
    let last = pulses.iter().map(|(_, p)| if p.ticks > 0 { p.done } else { p.since }).fold(f64::MIN, f64::max);
    if !pulses.is_empty() && t - last > FROZEN_S {
        return Some(format!("все циклы стоят {:.0} с", t - last));
    }
    pulses.iter().find_map(|(n, p)| {
        let s = t - if p.ticks > 0 { p.done } else { p.since };
        (s > STUCK_TIMES * quiet_after(n, p.tick_s)).then(|| format!("{n} молчит {:.0} мин", s / 60.0))
    })
}

/// The watchdog: its own thread (not the runtime's: a stuck runtime stands it still too), every `WATCHDOG_EVERY_S`;
/// `frozen` -> told (on a runtime of its own) and the process ends with 1.
pub(crate) fn watchdog() {
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_secs(WATCHDOG_EVERY_S));
        if let Some(why) = frozen(&crate::runner::pulses(), now()) {
            let m = format!("♻️ бот завис ({why}): перезапуск. Заявки стоят по старым ценам до старта.");
            log!("health: {m}");
            crate::notify::send_now(&m);
            std::process::exit(1);
        }
    });
}

/// What was told: a silent loop -> when; a position without a stop -> (first seen so, when told: 0 not yet).
#[derive(Default)]
struct Watch {
    /// The shorts' sub-account's watch (`sub`): its positions only (the loops are the live one's), its own doc.
    sub: bool,
    silent: HashMap<&'static str, f64>,
    bare: HashMap<String, (f64, f64)>,
    /// A bare position's stop last tried (coin -> when).
    guarded: HashMap<String, f64>,
}

impl Watch {
    /// The loops' messages at `t`.
    fn loops(&mut self, pulses: &[(&'static str, Pulse)], t: f64) -> Vec<String> {
        let mut out = Vec::new();
        for (n, p) in pulses {
            match silent(n, p, t) {
                Some(s) => {
                    if self.silent.get(n).is_none_or(|x| t - x >= AGAIN_S) {
                        out.push(format!("⚠️ {n}: стратегия молчит {:.0} мин (шаг раз в {:.0} с): зависла или упала. Стопы открытых позиций на бирже, но новых сигналов она не ловит.",
                            s / 60.0, p.tick_s));
                        self.silent.insert(n, t);
                    }
                }
                None => {
                    if self.silent.remove(n).is_some() {
                        out.push(format!("✅ {n}: снова работает"));
                    }
                }
            }
        }
        out
    }

    /// The positions' messages at `t`: `pos` coin -> signed size, `stops` the coins with a resting stop.
    fn positions(&mut self, pos: &HashMap<String, f64>, stops: &HashSet<String>, t: f64) -> Vec<String> {
        let mut out = Vec::new();
        let bare: HashSet<&String> = pos.iter().filter(|(c, x)| **x != 0.0 && !stops.contains(*c)).map(|x| x.0).collect();
        let mut fine: Vec<String> = self.bare.keys().filter(|c| !bare.contains(c)).cloned().collect();
        fine.sort();
        for c in fine {
            if self.bare.remove(&c).is_some_and(|x| x.1 > 0.0) {
                out.push(format!("✅ {c}: стоп на месте (или позиция закрыта)"));
            }
        }
        let mut bare: Vec<&String> = bare.into_iter().collect();
        bare.sort();
        for c in bare {
            let e = self.bare.entry(c.clone()).or_insert((t, 0.0));
            if t - e.0 >= BARE_AFTER_S && (e.1 == 0.0 || t - e.1 >= AGAIN_S) {
                let x = pos[c];
                out.push(format!("🚨 {c}: {} {} без стопа на бирже уже {:.0} мин. Проверь руками.",
                    if x > 0.0 { "лонг" } else { "шорт" }, x.abs(), (t - e.0) / 60.0));
                e.1 = t;
            }
        }
        out
    }
}

impl Watch {
    /// The positions bare `BARE_AFTER_S` (by the rounds before) whose stop is due at `t` (none tried, or `GUARD_AGAIN_S` ago).
    fn guard_due(&mut self, pos: &HashMap<String, f64>, stops: &HashSet<String>, t: f64) -> Vec<String> {
        self.guarded.retain(|c, _| pos.contains_key(c) && !stops.contains(c));
        let mut v: Vec<String> = pos.keys().filter(|c| !stops.contains(*c))
            .filter(|c| self.bare.get(*c).is_some_and(|x| t - x.0 >= BARE_AFTER_S))
            .filter(|c| self.guarded.get(*c).is_none_or(|x| t - x >= GUARD_AGAIN_S)).cloned().collect();
        v.sort();
        for c in &v {
            self.guarded.insert(c.clone(), t);
        }
        v
    }
}

/// A bare position's stop trigger: `GUARD_STOP` from `entry` on the losing side, or `LIQ_STOP_SHARE` of the way to `liq`
/// when that is nearer (`liq` 0: none known).
pub(crate) fn guard_trigger(entry: f64, size: f64, liq: f64) -> f64 {
    let to_liq = if liq > 0.0 { crate::trades::LIQ_STOP_SHARE * (entry - liq).abs() / entry } else { f64::MAX };
    let d = GUARD_STOP.min(to_liq);
    entry * if size > 0.0 { 1.0 - d } else { 1.0 + d }
}

/// The bare position `coin` (signed `size`) gets its stop (`guard_trigger` from its entry and liquidation, read from its
/// dex): the chat's line.
async fn guard_stop(v: &Venue, coin: &str, size: f64) -> Result<String> {
    let dex = coin.split_once(':').map_or("", |x| x.0);
    let st = v.ex.info(json!({"type": "clearinghouseState", "user": v.account, "dex": dex})).await?;
    let p = st["assetPositions"].as_array().into_iter().flatten().map(|p| &p["position"]).find(|p| p["coin"] == coin)
        .with_context(|| format!("{coin}: its position not read"))?;
    let num = |x: &Value| x.as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let entry = num(&p["entryPx"]);
    anyhow::ensure!(entry > 0.0, "{coin}: no entry price");
    let trigger = guard_trigger(entry, size, num(&p["liquidationPx"]));
    v.stop(coin, size.signum(), size.abs(), trigger).await?;
    Ok(format!("🛡 {coin}: {} {} стоял без стопа {:.0} мин — бот поставил стоп {:.6} ({:+.1}% от входа {entry}). Позиция не из живой сделки стратегии: тейка и выхода по времени нет, проверь.",
        if size > 0.0 { "лонг" } else { "шорт" }, size.abs(), BARE_AFTER_S / 60.0, trigger, (trigger / entry - 1.0) * 100.0))
}

/// The account's positions on the main dex and `dexes` (coin -> signed size, none zero) and the coins with a resting
/// stop (a reduce-only trigger order).
pub(crate) async fn account(v: &Venue, unified: bool, dexes: &[String]) -> Result<(HashMap<String, f64>, HashSet<String>)> {
    let (_, mut pos) = crate::venue::equity_of(&v.ex, &v.account, unified).await?;
    let mut stops = HashSet::new();
    for d in std::iter::once("").chain(dexes.iter().map(String::as_str)) {
        let q = if d.is_empty() {
            json!({"type": "frontendOpenOrders", "user": v.account})
        } else {
            pos.extend(v.dex_positions(d).await?);
            json!({"type": "frontendOpenOrders", "user": v.account, "dex": d})
        };
        for o in v.ex.info(q).await?.as_array().into_iter().flatten() {
            if o["reduceOnly"].as_bool() == Some(true) && o["isTrigger"].as_bool() == Some(true)
                && let Some(c) = o["coin"].as_str()
            {
                stops.insert(c.to_string());
            }
        }
    }
    pos.retain(|_, x| *x != 0.0);
    Ok((pos, stops))
}

/// The doc saved: the loops (silent: s past `quiet_after`, else null), the positions with their stop or none, the
/// account's read error.
fn snapshot(pulses: &[(&'static str, Pulse)], t: f64, pos: &HashMap<String, f64>, stops: &HashSet<String>, err: Option<String>) -> Value {
    let mut ps: Vec<Value> = pos.iter().map(|(c, x)| json!({"coin": c, "size": x, "stop": stops.contains(c)})).collect();
    ps.sort_by(|a, b| a["coin"].as_str().cmp(&b["coin"].as_str()));
    json!({"at": t,
        "loops": pulses.iter().map(|(n, p)| json!({"name": n, "tick_s": p.tick_s, "since": p.since, "done": p.done, "ticks": p.ticks,
            "slowest_s": p.slowest_s, "silent_s": silent(n, p, t)})).collect::<Vec<_>>(),
        "positions": ps, "account_error": err})
}

impl Watch {
    /// One round at `now()`: the loops, the account, the messages to the chat, the doc.
    async fn round(&mut self, cfg: &Config, v: &Venue, unified: bool, dexes: &[String]) -> Vec<String> {
        let t = now();
        let pulses = crate::runner::pulses();
        let mut msgs = if self.sub { Vec::new() } else { self.loops(&pulses, t) };
        let (pos, stops, err) = match account(v, unified, dexes).await {
            Ok((p, mut s)) => {
                for c in self.guard_due(&p, &s, t) {
                    if !v.coins.contains_key(&c) {
                        continue;
                    }
                    match guard_stop(v, &c, p[&c]).await {
                        Ok(m) => {
                            msgs.push(m);
                            s.insert(c);
                        }
                        Err(e) => msgs.push(format!("🚨 {c}: стоп поставить не вышло: {e:#}")),
                    }
                }
                msgs.extend(self.positions(&p, &s, t));
                (p, s, None)
            }
            Err(e) => {
                log!("health: the account not read: {e:#}");
                (HashMap::new(), HashSet::new(), Some(format!("{e:#}")))
            }
        };
        if self.sub {
            for m in &mut msgs {
                *m = format!("шорт-аккаунт: {m}");
            }
        }
        for m in &msgs {
            log!("health: {m}");
            crate::notify::send(m.clone());
        }
        if !self.sub {
            crate::halt::save(cfg).await;
        }
        if let Err(e) = crate::store::save_doc(cfg, if self.sub { SUB_DOC } else { DOC }, &snapshot(&pulses, t, &pos, &stops, err)).await {
            log!("health: not saved: {e:#}");
        }
        msgs
    }
}

/// The watch on the live account, every `EVERY_S` (none without the account's keys).
pub(crate) async fn start(cfg: &Config) -> Result<()> {
    let Some((base, unified)) = crate::runner::venue("health").await? else {
        return Ok(());
    };
    let cfg = cfg.clone();
    let (sub, cfg2) = (crate::sub::venue(&base)?, cfg.clone());
    tokio::spawn(async move {
        let mut w = Watch::default();
        let mut venue = (f64::MIN, base.clone());
        let mut iv = tokio::time::interval(std::time::Duration::from_secs_f64(EVERY_S));
        loop {
            iv.tick().await;
            if now() - venue.0 >= 3600.0 {
                venue = (now(), crate::runner::with_hip3("health", &base).await);
            }
            let venue = &venue.1;
            let dexes = crate::venue::usdc_dexes(&venue.ex).await;
            w.round(&cfg, &venue, unified, &dexes).await;
        }
    });
    // the shorts' sub-account: its positions against its stops (the main dex: its coins), told as its own
    tokio::spawn(async move {
        let mut w = Watch { sub: true, ..Watch::default() };
        let mut iv = tokio::time::interval(std::time::Duration::from_secs_f64(EVERY_S));
        loop {
            iv.tick().await;
            w.round(&cfg2, &sub, false, &[]).await;
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Clock, FakeHl, cfg, saved};

    fn pulse(tick_s: f64, since: f64, done: f64, ticks: u64) -> Pulse {
        Pulse { tick_s, since, done, ticks, slowest_s: 0.5 }
    }

    #[test]
    fn a_loop_is_silent_past_10_min_or_10_of_its_ticks() {
        assert_eq!((quiet_after("wick", 5.0), quiet_after("guard", 60.0), quiet_after("x", 300.0)), (600.0, 600.0, 3000.0));
        // its last tick done 10 min ago: not yet; a second more: silent
        assert_eq!(silent("wick", &pulse(5.0, 0.0, 1000.0, 3), 1600.0), None);
        assert_eq!(silent("wick", &pulse(5.0, 0.0, 1000.0, 3), 1601.0), Some(601.0));
        // never ticked: from its start (a step stuck from the first)
        assert_eq!(silent("wick", &pulse(5.0, 1000.0, 0.0, 0), 1500.0), None);
        assert_eq!(silent("wick", &pulse(5.0, 1000.0, 0.0, 0), 1700.0), Some(700.0));
        // the breakout's first step reads every coin's candles (476 s live): 30 min
        assert_eq!(quiet_after("breakout", 10.0), 1800.0);
        assert_eq!(silent("breakout", &pulse(10.0, 1000.0, 0.0, 0), 1000.0 + 476.0 + 600.0), None);
        assert_eq!(silent("breakout", &pulse(10.0, 1000.0, 0.0, 0), 2801.0), Some(1801.0));
    }

    #[test]
    fn a_silent_loop_is_told_then_again_each_hour_then_once_fine() {
        let mut w = Watch::default();
        let ok = pulse(5.0, 0.0, 9_990.0, 100);
        let dead = pulse(5.0, 0.0, 9_000.0, 100);
        let m = w.loops(&[("wick", ok), ("dip", dead)], 10_000.0);
        assert_eq!(m.len(), 1);
        assert!(m[0].starts_with("⚠️ dip: стратегия молчит 17 мин"), "{m:?}");
        assert!(w.loops(&[("dip", dead)], 10_060.0).is_empty(), "told once");
        assert_eq!(w.loops(&[("dip", dead)], 13_600.0).len(), 1, "again in an hour");
        assert_eq!(w.loops(&[("dip", pulse(5.0, 0.0, 13_650.0, 101))], 13_660.0), vec!["✅ dip: снова работает".to_string()]);
        assert!(w.loops(&[("dip", pulse(5.0, 0.0, 13_700.0, 102))], 13_710.0).is_empty());
    }

    #[test]
    fn a_position_without_a_stop_is_told_after_2_min_then_hourly_then_once_fine() {
        let mut w = Watch::default();
        let pos: HashMap<String, f64> = [("LIT".to_string(), 121.0), ("ARB".to_string(), -4808.7), ("ZRO".to_string(), 0.0)].into();
        let stops: HashSet<String> = ["ARB".to_string()].into();
        // just filled: its stop comes at the next check
        assert!(w.positions(&pos, &stops, 1000.0).is_empty());
        assert!(w.positions(&pos, &stops, 1119.0).is_empty());
        let m = w.positions(&pos, &stops, 1120.0);
        assert_eq!(m, vec!["🚨 LIT: лонг 121 без стопа на бирже уже 2 мин. Проверь руками.".to_string()]);
        assert!(w.positions(&pos, &stops, 1200.0).is_empty(), "told once");
        let short: HashMap<String, f64> = [("LIT".to_string(), 121.0), ("ARB".to_string(), -4808.7)].into();
        let m = w.positions(&short, &HashSet::new(), 4720.0);
        assert_eq!(m.len(), 1, "LIT again in an hour; ARB's stop gone just now: {m:?}");
        assert!(m[0].starts_with("🚨 LIT: лонг 121 без стопа на бирже уже 62 мин"), "{m:?}");
        let m = w.positions(&short, &HashSet::new(), 4840.0);
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(m[0].starts_with("🚨 ARB: шорт 4808.7"), "{m:?}");
        // LIT's stop back, ARB closed
        let m = w.positions(&[("LIT".to_string(), 121.0)].into(), &["LIT".to_string()].into(), 4800.0);
        assert_eq!(m, ["✅ ARB: стоп на месте (или позиция закрыта)", "✅ LIT: стоп на месте (или позиция закрыта)"]);
        assert!(w.bare.is_empty());
        // a bare one gone before it was told: nothing
        assert!(w.positions(&[("ZEC".to_string(), 1.0)].into(), &HashSet::new(), 5000.0).is_empty());
        assert!(w.positions(&HashMap::new(), &HashSet::new(), 5030.0).is_empty());
    }

    #[tokio::test]
    async fn the_round_reads_the_account_on_every_dex_tells_and_saves_the_doc() {
        let f = FakeHl::start(&[("BTC", 5, 40, 100_000.0), ("WTA", 2, 10, 100.0), ("xyz:WTH", 2, 10, 50.0)]).await;
        let clock = Clock::at(1_000_000.0);
        let v = f.venue();
        let dexes = vec!["xyz".to_string()];
        f.with(|s| {
            s.positions.insert("WTA".into(), (2.0, 100.0));
            s.positions.insert("xyz:WTH".into(), (-3.0, 50.0));
        });
        v.stop("WTA", 1.0, 2.0, 80.0).await.unwrap();
        v.limit("xyz:WTH", 3.0, 45.0, true, "Gtc").await.unwrap(); // a reduce-only take is not a stop
        let (pos, stops) = account(&v, false, &dexes).await.unwrap();
        assert_eq!(pos, [("WTA".to_string(), 2.0), ("xyz:WTH".to_string(), -3.0)].into());
        assert_eq!(stops, ["WTA".to_string()].into());
        let c = cfg();
        let mut w = Watch::default();
        assert!(w.round(&c, &v, false, &dexes).await.iter().all(|m| !m.contains("xyz:WTH")), "not yet");
        clock.go(130.0);
        let m = w.round(&c, &v, false, &dexes).await;
        // bare 2 min: the bot's own stop, 20% over the short's entry (no liquidation known), told; not "bare"
        assert_eq!(m.len(), 1, "{m:?}");
        assert!(m[0].starts_with("🛡 xyz:WTH: шорт 3 стоял без стопа 2 мин — бот поставил стоп 60"), "{m:?}");
        let o = f.with(|s| s.orders.values().filter(|o| o.coin == "xyz:WTH" && o.status == "open").map(|o| o.oid).count());
        assert_eq!(o, 2, "the take and the new stop");
        let d = saved(&c, DOC).await.unwrap();
        assert_eq!(d["at"], json!(1_000_130.0));
        assert_eq!(d["positions"], json!([{"coin": "WTA", "size": 2.0, "stop": true}, {"coin": "xyz:WTH", "size": -3.0, "stop": true}]));
        // the next round: the stop rests, nothing more
        clock.go(60.0);
        let m = w.round(&c, &v, false, &dexes).await;
        assert!(m.iter().all(|m| !m.contains("xyz:WTH")), "{m:?}");
        assert!(d["account_error"].is_null() && d["loops"].is_array());
        // the account not read: said in the doc, nothing told of the positions
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut s, _)) = l.accept().await {
                let mut b = [0u8; 4096];
                let _ = s.read(&mut b).await;
                let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\ncontent-length: 4\r\nconnection: close\r\n\r\nnope").await;
            }
        });
        let gone = crate::venue::Venue::for_test(crate::exchange::Exchange::new(&base, crate::testkit::KEY).unwrap(), "0x0", HashMap::new(), HashMap::new());
        let c2 = cfg();
        assert!(w.round(&c2, &gone, false, &dexes).await.iter().all(|m| !m.contains("стоп")));
        assert!(saved(&c2, DOC).await.unwrap()["account_error"].is_string());
    }

    #[test]
    fn a_bare_position_s_stop_sits_20_pct_off_or_short_of_its_liquidation() {
        assert!((guard_trigger(100.0, 2.0, 0.0) - 80.0).abs() < 1e-9);
        assert!((guard_trigger(100.0, -2.0, 0.0) - 120.0).abs() < 1e-9);
        // 2026-11-18 xyz:SOXL: 50.452 at 5x isolated, liquidation 42.487: 90% of the way, -14.2%
        let t = guard_trigger(50.452, 8.02, 42.4875);
        assert!((t - (50.452 - 0.9 * (50.452 - 42.4875))).abs() < 1e-9 && t > 42.4875);
        // a short's liquidation over it
        assert!((guard_trigger(100.0, -1.0, 110.0) - 109.0).abs() < 1e-9);
        // a far liquidation (cross): 20%
        assert!((guard_trigger(100.0, 1.0, 30.0) - 80.0).abs() < 1e-9);
    }

    #[test]
    fn a_bare_position_s_stop_is_due_after_2_min_then_every_5_while_bare() {
        let mut w = Watch::default();
        let pos: HashMap<String, f64> = [("LIT".to_string(), 1.0), ("ARB".to_string(), -2.0)].into();
        let stops: HashSet<String> = ["ARB".to_string()].into();
        w.positions(&pos, &stops, 1000.0);
        assert!(w.guard_due(&pos, &stops, 1119.0).is_empty());
        assert_eq!(w.guard_due(&pos, &stops, 1120.0), vec!["LIT".to_string()]);
        assert!(w.guard_due(&pos, &stops, 1300.0).is_empty(), "tried just now");
        assert_eq!(w.guard_due(&pos, &stops, 1420.0), vec!["LIT".to_string()], "still bare 5 min on");
        // its stop rests: forgotten
        let both: HashSet<String> = ["ARB".to_string(), "LIT".to_string()].into();
        assert!(w.guard_due(&pos, &both, 1500.0).is_empty());
        assert!(w.guarded.is_empty());
    }

    #[test]
    fn the_bot_is_frozen_when_every_loop_stands_3_min_or_one_twice_its_silence() {
        let ok = pulse(5.0, 0.0, 1000.0, 9);
        assert_eq!(frozen(&[], 99_999.0), None);
        assert_eq!(frozen(&[("wick", ok), ("dip", ok)], 1180.0), None);
        assert_eq!(frozen(&[("wick", ok), ("dip", ok)], 1181.0).as_deref(), Some("все циклы стоят 181 с"));
        // one alive: the other stuck 20 min (2 x its 10)
        let fresh = pulse(5.0, 0.0, 2200.0, 9);
        assert_eq!(frozen(&[("wick", ok), ("dip", fresh)], 2200.0), None);
        assert_eq!(frozen(&[("wick", ok), ("dip", fresh)], 2201.0).as_deref(), Some("wick молчит 20 мин"));
        // the breakout's first long step (30 min its silence): an hour
        let b = pulse(10.0, 0.0, 0.0, 0);
        assert_eq!(frozen(&[("breakout", b), ("dip", pulse(5.0, 0.0, 3600.0, 9))], 3600.0), None);
        // just born, not ticked yet: from its start
        assert_eq!(frozen(&[("wick", pulse(5.0, 5000.0, 0.0, 0))], 5100.0), None);
    }

    #[test]
    fn the_doc_holds_each_loop_with_its_silence() {
        let d = snapshot(&[("wick", pulse(5.0, 0.0, 990.0, 9)), ("dip", pulse(5.0, 0.0, 100.0, 9))], 1000.0, &HashMap::new(), &HashSet::new(), None);
        assert_eq!(d["loops"][0]["silent_s"], Value::Null);
        assert_eq!(d["loops"][1]["silent_s"], json!(900.0));
        assert_eq!(d["loops"][1]["name"], "dip");
    }
}
