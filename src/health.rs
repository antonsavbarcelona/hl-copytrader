//! The bot's health, watched from inside (2026-11-17, the user's call: a check of every strategy, "дергать за ручки и
//! проверять что все работает как надо"). Every `EVERY_S`:
//! - each loop's pulse (`runner::pulses`: every strategy on the runner, the guard): a loop silent past `quiet_after`
//!   (dead: a panic ends its task without a word; or stuck in a step) is told to the chat;
//! - the account's positions (every USDC dex) against its resting orders: a position with no resting stop (a reduce-only
//!   trigger order) for `BARE_AFTER_S` is told to the chat (a fill gets its stop within a check, 15 s for the wick);
//! - each told again every `AGAIN_S` while so, and once when it is fine again;
//! - the whole of it saved as the doc `health` (read by `tools/health.py`, with the events and the exchange).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
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
/// Told again this often while so.
const AGAIN_S: f64 = 3600.0;
const DOC: &str = "health";

/// The silence after which a loop ticking every `tick_s` is told.
pub(crate) fn quiet_after(tick_s: f64) -> f64 {
    QUIET_MIN_S.max(QUIET_TICKS * tick_s)
}

/// How long the loop has been silent at `t` (since its last tick done, or its start before any), when past `quiet_after`.
pub(crate) fn silent(p: &Pulse, t: f64) -> Option<f64> {
    let s = t - if p.ticks > 0 { p.done } else { p.since };
    (s > quiet_after(p.tick_s)).then_some(s)
}

/// What was told: a silent loop -> when; a position without a stop -> (first seen so, when told: 0 not yet).
#[derive(Default)]
struct Watch {
    silent: HashMap<&'static str, f64>,
    bare: HashMap<String, (f64, f64)>,
}

impl Watch {
    /// The loops' messages at `t`.
    fn loops(&mut self, pulses: &[(&'static str, Pulse)], t: f64) -> Vec<String> {
        let mut out = Vec::new();
        for (n, p) in pulses {
            match silent(p, t) {
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
            "slowest_s": p.slowest_s, "silent_s": silent(p, t)})).collect::<Vec<_>>(),
        "positions": ps, "account_error": err})
}

impl Watch {
    /// One round at `now()`: the loops, the account, the messages to the chat, the doc.
    async fn round(&mut self, cfg: &Config, v: &Venue, unified: bool, dexes: &[String]) -> Vec<String> {
        let t = now();
        let pulses = crate::runner::pulses();
        let mut msgs = self.loops(&pulses, t);
        let (pos, stops, err) = match account(v, unified, dexes).await {
            Ok((p, s)) => {
                msgs.extend(self.positions(&p, &s, t));
                (p, s, None)
            }
            Err(e) => {
                log!("health: the account not read: {e:#}");
                (HashMap::new(), HashSet::new(), Some(format!("{e:#}")))
            }
        };
        for m in &msgs {
            log!("health: {m}");
            crate::notify::send(m.clone());
        }
        if let Err(e) = crate::store::save_doc(cfg, DOC, &snapshot(&pulses, t, &pos, &stops, err)).await {
            log!("health: not saved: {e:#}");
        }
        msgs
    }
}

/// The watch on the live account, every `EVERY_S` (none without the account's keys).
pub(crate) async fn start(cfg: &Config) -> Result<()> {
    let Some((venue, unified)) = crate::runner::venue("health").await? else {
        return Ok(());
    };
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let mut w = Watch::default();
        let mut iv = tokio::time::interval(std::time::Duration::from_secs_f64(EVERY_S));
        loop {
            iv.tick().await;
            let dexes = crate::venue::usdc_dexes(&venue.ex).await;
            w.round(&cfg, &venue, unified, &dexes).await;
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
        assert_eq!((quiet_after(5.0), quiet_after(60.0), quiet_after(300.0)), (600.0, 600.0, 3000.0));
        // its last tick done 10 min ago: not yet; a second more: silent
        assert_eq!(silent(&pulse(5.0, 0.0, 1000.0, 3), 1600.0), None);
        assert_eq!(silent(&pulse(5.0, 0.0, 1000.0, 3), 1601.0), Some(601.0));
        // never ticked: from its start (a step stuck from the first)
        assert_eq!(silent(&pulse(5.0, 1000.0, 0.0, 0), 1500.0), None);
        assert_eq!(silent(&pulse(5.0, 1000.0, 0.0, 0), 1700.0), Some(700.0));
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
        assert!(m.iter().any(|m| m.starts_with("🚨 xyz:WTH: шорт 3 без стопа")), "{m:?}");
        let d = saved(&c, DOC).await.unwrap();
        assert_eq!(d["at"], json!(1_000_130.0));
        assert_eq!(d["positions"], json!([{"coin": "WTA", "size": 2.0, "stop": true}, {"coin": "xyz:WTH", "size": -3.0, "stop": false}]));
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
    fn the_doc_holds_each_loop_with_its_silence() {
        let d = snapshot(&[("wick", pulse(5.0, 0.0, 990.0, 9)), ("dip", pulse(5.0, 0.0, 100.0, 9))], 1000.0, &HashMap::new(), &HashSet::new(), None);
        assert_eq!(d["loops"][0]["silent_s"], Value::Null);
        assert_eq!(d["loops"][1]["silent_s"], json!(900.0));
        assert_eq!(d["loops"][1]["name"], "dip");
    }
}
