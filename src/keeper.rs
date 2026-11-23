//! A strategy's trade keeping, the same for all: its events (to the log, the chat by its own `message`, the store),
//! its state saved, the account read, and its open trades swept: a resting stop or take filled (or the position
//! closed by hand) ends a trade, one due out (`exit_at`) is closed at market; each end told and taken off the family.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Value, json};

use crate::api::now;
use crate::config::Config;
use crate::log;
use crate::store::Store;
use crate::trades::{self, Trade};
use crate::venue::Venue;

const H: f64 = 3600.0;
const DAY: f64 = 86_400.0;

/// How a close event tells the time held: as the strategy's messages read it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum HeldIn {
    No,
    Hours,
    Days,
}

pub(crate) struct Keeper {
    pub(crate) venue: Venue,
    pub(crate) unified: bool,
    pub(crate) cfg: Config,
    pub(crate) store: Store,
    /// The strategy's kind: the events'.
    pub(crate) kind: &'static str,
    /// Its trades' kind in the family registry (the kind unless set, `registered_as`).
    reg: &'static str,
    /// The events' names for a trade ended and an exit failed (`words`).
    close_what: &'static str,
    error_what: &'static str,
    /// The chat's line for an event (None: not told).
    message: fn(&Value) -> Option<String>,
    held_in: HeldIn,
}

impl Keeper {
    pub(crate) fn new(venue: Venue, unified: bool, cfg: Config, store: Store, kind: &'static str, message: fn(&Value) -> Option<String>, held_in: HeldIn) -> Self {
        Keeper { venue, unified, cfg, store, kind, reg: kind, close_what: "close", error_what: "error", message, held_in }
    }

    /// Its trades registered in the family under `reg` (the HIP-3 weekend dips: events "hip3", trades "wkd").
    pub(crate) fn registered_as(self, reg: &'static str) -> Self {
        Keeper { reg, ..self }
    }

    /// Its events' names for a trade ended and a failed exit ("close" / "error" unless set).
    pub(crate) fn words(self, close_what: &'static str, error_what: &'static str) -> Self {
        Keeper { close_what, error_what, ..self }
    }

    /// The event with its kind and time: logged, told when it has a message, stored.
    pub(crate) fn event(&self, mut ev: Value) {
        ev["kind"] = json!(self.kind);
        ev["at"] = json!(now());
        log!("{}: {ev}", self.kind);
        if let Some(m) = (self.message)(&ev) {
            crate::notify::send(m);
        }
        self.store.event(ev);
    }

    /// The state saved as the document `doc` (in the background).
    pub(crate) fn save<T: Serialize>(&self, doc: &'static str, state: &T) {
        crate::store::save_state(&self.cfg, doc, self.kind, state);
    }

    /// The account's positions (coin -> signed size) on the dexes of `coins` (the main dex: "", the family's equity set
    /// from it; a HIP-3 dex: "xyz", ...), and the dexes read.
    pub(crate) async fn held(&self, coins: impl Iterator<Item = &str>) -> (HashMap<String, f64>, HashSet<String>) {
        let dexes: HashSet<String> = coins.map(dex_of).map(str::to_string).collect();
        let (mut held, mut read) = (HashMap::new(), HashSet::new());
        for d in dexes {
            let got = if d.is_empty() {
                crate::venue::equity_of(&self.venue.ex, &self.venue.account, self.unified).await.map(|(eq, p)| {
                    if self.venue.ex.is_sub() { crate::sub::set_equity(eq) } else { trades::set_equity(eq) }
                    p
                })
            } else {
                self.venue.dex_positions(&d).await
            };
            match got {
                Ok(p) => {
                    held.extend(p);
                    read.insert(d);
                }
                Err(e) => log!("{}: account{} not read: {e:#}", self.kind, if d.is_empty() { String::new() } else { format!(" ({d})") }),
            }
        }
        (held, read)
    }

    /// A trade ended (`why`, at `px`): told, taken off the family.
    pub(crate) fn ended(&self, tr: &Trade, why: &str, px: f64) {
        let ret = tr.ret(px);
        let mut ev = json!({"what": self.close_what, "coin": tr.coin, "why": why, "entry_px": tr.entry_px, "px": px, "ret_pct": ret * 100.0,
            "pnl_usd": ret * tr.size.abs() * tr.entry_px});
        match self.held_in {
            HeldIn::Hours => ev["held_h"] = json!((now() - tr.at) / H),
            HeldIn::Days => ev["held_days"] = json!((now() - tr.at) / DAY),
            HeldIn::No => {}
        }
        self.event(ev);
        trades::unregister(&tr.coin, self.reg);
    }

    /// The trade out at market (its stop and take cancelled): told and taken off; false: the close failed (told).
    pub(crate) async fn exit(&self, tr: &Trade, why: &str) -> bool {
        match trades::close(&self.venue, tr).await {
            Ok(px) => {
                self.ended(tr, why, px);
                true
            }
            Err(e) => {
                self.event(json!({"what": self.error_what, "coin": tr.coin, "error": format!("exit: {e:#}")}));
                false
            }
        }
    }

    /// The open trades (`tr` reads a trade of one): with `fills` the positions read and those whose stop or take filled
    /// (or closed by hand) end; those due (`exit_at` passed) go out at market when `time_exit`. Each end told (`ended`).
    /// -> whether any ended (the caller saves its state).
    pub(crate) async fn sweep<P>(&self, open: &mut Vec<P>, tr: impl Fn(&P) -> &Trade, time_exit: bool, fills: bool) -> bool {
        let out = self.sweep_out(open, |p| Some(tr(p)), time_exit, fills).await;
        for (p, why, px) in &out {
            self.ended(tr(p), why, *px);
        }
        !out.is_empty()
    }

    /// `sweep` with the ends handed back, not told (a strategy with its own close event): (the position, why, the price),
    /// taken out of `open`; a position without a trade (`tr` None: a paper one) left as it is. A failed exit is told.
    pub(crate) async fn sweep_out<P>(&self, open: &mut Vec<P>, tr: impl Fn(&P) -> Option<&Trade>, time_exit: bool, fills: bool) -> Vec<(P, &'static str, f64)> {
        let mut ended = Vec::new();
        if !open.iter().any(|p| tr(p).is_some()) {
            return ended;
        }
        let (held, read) = if fills { self.held(open.iter().filter_map(|p| tr(p)).map(|t| t.coin.as_str())).await } else { Default::default() };
        let mut keep = Vec::new();
        for p in std::mem::take(open) {
            let Some(t) = tr(&p) else {
                keep.push(p);
                continue;
            };
            let gone = if read.contains(dex_of(&t.coin)) {
                trades::gone(&self.venue, t, held.get(&t.coin).copied().unwrap_or(0.0), self.reg).await
            } else {
                None
            };
            let out = match gone {
                Some(x) => Some(x),
                None if time_exit && now() >= t.exit_at => match trades::close(&self.venue, t).await {
                    Ok(px) => Some(("time", px)),
                    Err(e) => {
                        self.event(json!({"what": self.error_what, "coin": t.coin, "error": format!("exit: {e:#}")}));
                        None
                    }
                },
                None => None,
            };
            match out {
                Some((why, px)) => ended.push((p, why, px)),
                None => keep.push(p),
            }
        }
        *open = keep;
        ended
    }
}

/// A coin's dex: "" for the main dex, "xyz" for "xyz:TSLA".
fn dex_of(coin: &str) -> &str {
    coin.split_once(':').map_or("", |d| d.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{FakeHl, cfg};

    fn msg(ev: &Value) -> Option<String> {
        (ev["what"] == "close").then(|| format!("closed {}", ev["coin"]))
    }

    #[tokio::test]
    async fn the_held_time_as_the_strategy_tells_it_and_a_trade_due_goes_out() {
        let f = FakeHl::start(&[("KPA", 2, 10, 100.0)]).await;
        let (store, mut rx) = Store::for_test();
        for (unit, key) in [(HeldIn::Hours, Some("held_h")), (HeldIn::Days, Some("held_days")), (HeldIn::No, None)] {
            let k = Keeper::new(f.venue(), false, cfg(), store.clone(), "kp", msg, unit);
            let tr = Trade { coin: "KPA".into(), size: 1.0, entry_px: 100.0, at: now() - 7200.0, ..Default::default() };
            k.ended(&tr, "stop", 90.0);
            let e = crate::store::drain(&mut rx).pop().unwrap();
            assert_eq!((e["kind"].as_str(), e["why"].as_str()), (Some("kp"), Some("stop")));
            assert!((e["ret_pct"].as_f64().unwrap() + 10.0).abs() < 1e-9 && (e["pnl_usd"].as_f64().unwrap() + 10.0).abs() < 1e-9);
            for k in ["held_h", "held_days"] {
                assert_eq!(e.get(k).is_some(), Some(k) == key, "{e}");
            }
        }
        // a trade due goes out at market
        let k = Keeper::new(f.venue(), false, cfg(), store, "kp", msg, HeldIn::No);
        let tr = trades::open_market(&k.venue, "KPA", 1.0, 0.1, None, 3600.0).await.unwrap();
        let mut open = vec![Trade { exit_at: now() - 1.0, ..tr }];
        assert!(k.sweep(&mut open, |t| t, true, true).await);
        assert!(open.is_empty() && f.position("KPA") == 0.0);
        assert!(crate::store::drain(&mut rx).iter().any(|e| e["why"] == "time"));
        // nothing open: nothing read
        assert!(!k.sweep(&mut Vec::<Trade>::new(), |t| t, true, true).await);
    }

    /// 2026-11-20 xyz:SNDK: the coin shared (the HIP-3 dip and a wick rung long), the dip's part closed by `pxstop` at
    /// the price, its take's status showing size gone with no fill: told the stop at pxstop's price, not the take; the
    /// other strategy's part left alone.
    #[tokio::test]
    async fn a_shared_coin_s_part_closed_by_the_price_is_its_stop_not_its_take() {
        let f = FakeHl::start(&[("KPC", 2, 10, 100.0)]).await;
        let (store, mut rx) = Store::for_test();
        let k = Keeper::new(f.venue(), false, cfg(), store, "kp", msg, HeldIn::No);
        let tr = trades::open_market(&k.venue, "KPC", 1.0, 0.1, Some(0.05), 3600.0).await.unwrap();
        trades::register("KPC", "kp", tr.risk(), 1.0);
        let other = trades::open_market(&k.venue, "KPC", 1.0, 0.2, None, 3600.0).await.unwrap();
        trades::register("KPC", "kp_other", other.risk(), 1.0);
        // pxstop: the mark trailing (the resting stop not triggered), out at market by the price, the stop cancelled,
        // its oid kept
        f.with(|s| s.marks.insert("KPC".into(), 100.0));
        f.set_mid("KPC", 89.0);
        k.venue.market("KPC", -1.0, true).await.unwrap();
        k.venue.cancel("KPC", tr.stop_oid.unwrap()).await;
        crate::pxstop::mark_closed(tr.stop_oid.unwrap(), 89.0);
        // the take's status: some of its size gone, no fill of it
        f.with(|s| s.orders.get_mut(&tr.take_oid.unwrap()).unwrap().left = 0.4);
        let mut open = vec![tr.clone()];
        assert!(k.sweep(&mut open, |t| t, false, true).await);
        let e = crate::store::drain(&mut rx).pop().unwrap();
        assert_eq!((e["why"].as_str(), e["px"].as_f64()), (Some("stop"), Some(89.0)), "{e}");
        assert!(e["pnl_usd"].as_f64().unwrap() < -10.0);
        assert!((f.position("KPC") - 1.0).abs() < 1e-9, "the other strategy's part held: {} {:?}", f.position("KPC"), f.with(|s| s.fills.clone()));
        trades::unregister("KPC", "kp_other");
    }

    #[tokio::test]
    async fn a_failed_close_is_told_and_the_trade_kept() {
        let f = FakeHl::start(&[("KPB", 2, 10, 100.0)]).await;
        let (store, mut rx) = Store::for_test();
        let k = Keeper::new(f.venue(), false, cfg(), store, "kp", msg, HeldIn::No);
        let tr = trades::open_market(&k.venue, "KPB", 1.0, 0.1, None, 3600.0).await.unwrap();
        f.with(|s| s.ioc_needs = 0.5);                       // nothing within reach: the market close fails
        let mut open = vec![Trade { exit_at: now() - 1.0, ..tr }];
        assert!(!k.sweep(&mut open, |t| t, true, true).await);
        assert_eq!(open.len(), 1);
        let ev = crate::store::drain(&mut rx);
        assert!(ev.iter().any(|e| e["what"] == "error" && e["error"].as_str().unwrap().starts_with("exit:")), "{ev:?}");
    }
}
