//! A strategy's trade keeping, the same for all: its events (to the log, the chat by its own `message`, the store),
//! its state saved, the account read, and its open trades swept: a resting stop or take filled (or the position
//! closed by hand) ends a trade, one due out (`exit_at`) is closed at market; each end told and taken off the family.

use std::collections::HashMap;

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
    /// The strategy's kind: the events', the family registry's.
    pub(crate) kind: &'static str,
    /// The chat's line for an event (None: not told).
    message: fn(&Value) -> Option<String>,
    held_in: HeldIn,
}

impl Keeper {
    pub(crate) fn new(venue: Venue, unified: bool, cfg: Config, store: Store, kind: &'static str, message: fn(&Value) -> Option<String>, held_in: HeldIn) -> Self {
        Keeper { venue, unified, cfg, store, kind, message, held_in }
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
        let (cfg, v, kind) = (self.cfg.clone(), serde_json::to_value(state).unwrap_or_default(), self.kind);
        tokio::spawn(async move {
            if let Err(e) = crate::store::save_doc(&cfg, doc, &v).await {
                log!("{kind}: state not saved: {e:#}");
            }
        });
    }

    /// The account's positions (coin -> signed size), the family's equity set from it; None: not read.
    pub(crate) async fn held(&self) -> Option<HashMap<String, f64>> {
        match crate::venue::equity_of(&self.venue.ex, &self.venue.account, self.unified).await {
            Ok((eq, p)) => {
                trades::set_equity(eq);
                Some(p)
            }
            Err(e) => {
                log!("{}: account not read: {e:#}", self.kind);
                None
            }
        }
    }

    /// A trade ended (`why`, at `px`): told, taken off the family.
    pub(crate) fn ended(&self, tr: &Trade, why: &str, px: f64) {
        let ret = tr.ret(px);
        let mut ev = json!({"what": "close", "coin": tr.coin, "why": why, "entry_px": tr.entry_px, "px": px, "ret_pct": ret * 100.0,
            "pnl_usd": ret * tr.size.abs() * tr.entry_px});
        match self.held_in {
            HeldIn::Hours => ev["held_h"] = json!((now() - tr.at) / H),
            HeldIn::Days => ev["held_days"] = json!((now() - tr.at) / DAY),
            HeldIn::No => {}
        }
        self.event(ev);
        trades::unregister(&tr.coin, self.kind);
    }

    /// The trade out at market (its stop and take cancelled): told and taken off; false: the close failed (told).
    pub(crate) async fn exit(&self, tr: &Trade, why: &str) -> bool {
        match trades::close(&self.venue, tr).await {
            Ok(px) => {
                self.ended(tr, why, px);
                true
            }
            Err(e) => {
                self.event(json!({"what": "error", "coin": tr.coin, "error": format!("exit: {e:#}")}));
                false
            }
        }
    }

    /// The open trades (`tr` reads a trade of one): those whose stop or take filled end; those due (`exit_at` passed)
    /// go out at market when `time_exit`. -> whether any ended (the caller saves its state).
    pub(crate) async fn sweep<P>(&self, open: &mut Vec<P>, tr: impl Fn(&P) -> &Trade, time_exit: bool) -> bool {
        if open.is_empty() {
            return false;
        }
        let held = self.held().await;
        let mut keep = Vec::new();
        let mut changed = false;
        for p in std::mem::take(open) {
            let t = tr(&p);
            let gone = match &held {
                Some(h) => trades::gone(&self.venue, t, h.get(&t.coin).copied().unwrap_or(0.0), self.kind).await,
                None => None,
            };
            let out = match gone {
                Some((why, px)) => {
                    self.ended(t, why, px);
                    true
                }
                None if time_exit && now() >= t.exit_at => self.exit(t, "time").await,
                None => false,
            };
            if out {
                changed = true;
            } else {
                keep.push(p);
            }
        }
        *open = keep;
        changed
    }
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
        assert!(k.sweep(&mut open, |t| t, true).await);
        assert!(open.is_empty() && f.position("KPA") == 0.0);
        assert!(crate::store::drain(&mut rx).iter().any(|e| e["why"] == "time"));
        // nothing open: nothing read
        assert!(!k.sweep(&mut Vec::<Trade>::new(), |t| t, true).await);
    }

    #[tokio::test]
    async fn a_failed_close_is_told_and_the_trade_kept() {
        let f = FakeHl::start(&[("KPB", 2, 10, 100.0)]).await;
        let (store, mut rx) = Store::for_test();
        let k = Keeper::new(f.venue(), false, cfg(), store, "kp", msg, HeldIn::No);
        let tr = trades::open_market(&k.venue, "KPB", 1.0, 0.1, None, 3600.0).await.unwrap();
        f.with(|s| s.ioc_needs = 0.5);                       // nothing within reach: the market close fails
        let mut open = vec![Trade { exit_at: now() - 1.0, ..tr }];
        assert!(!k.sweep(&mut open, |t| t, true).await);
        assert_eq!(open.len(), 1);
        let ev = crate::store::drain(&mut rx);
        assert!(ev.iter().any(|e| e["what"] == "error" && e["error"].as_str().unwrap().starts_with("exit:")), "{ev:?}");
    }
}
