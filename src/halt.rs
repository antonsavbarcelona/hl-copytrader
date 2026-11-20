//! A coin the exchange halted, or whose price jumped across a gap, gets no new trade for `PAUSE_S` (2026-11-20, the
//! user's call: xyz:SNDK halted 2026-11-19 21:00 UTC, reopened 01:07 at ~93 for 754 (a split by 8), an empty book (the
//! spread 29-43%); the wick's night bids and a weekend / night dip bought it in that: -$64 by the morning. No research
//! has such a move: its candles read it as a fall of 87%).
//!
//! The signs: an order refused "Trading is halted" (any coin: `note_error`, from the venue's replies), or a HIP-3 coin's
//! mid `JUMP` off its last read (`mid_seen`, from every dex's mids read). The pause runs from the last sign, so a halt's
//! refusals keep it on until the coin trades again. `trades::can_open` reads it: every strategy's entries, the wick's
//! bids (a paused coin's resting bids come off as not wanted); the open trades keep their stops and takes. Kept in the
//! doc `halts` (`save` each minute when changed, `load` at the start): a restart in the pause does not end it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{Value, json};

use crate::api::now;
use crate::config::Config;
use crate::log;

pub(crate) const PAUSE_S: f64 = 86_400.0;
/// A HIP-3 mid this far off its last read: a jump across a gap (a halt's reopening, a split), not a trade's move.
pub(crate) const JUMP: f64 = 0.25;
const DOC: &str = "halts";

#[cfg(not(test))]
static HALTS: std::sync::Mutex<Option<HashMap<String, f64>>> = std::sync::Mutex::new(None);
#[cfg(not(test))]
static LAST_MID: std::sync::Mutex<Option<HashMap<String, f64>>> = std::sync::Mutex::new(None);
#[cfg(test)]
thread_local! {
    // a test's own: the tests' fakes jump their mids at will, a shared pause would leak into the others
    static HALTS: std::cell::RefCell<Option<HashMap<String, f64>>> = const { std::cell::RefCell::new(None) };
    static LAST_MID: std::cell::RefCell<Option<HashMap<String, f64>>> = const { std::cell::RefCell::new(None) };
}
static DIRTY: AtomicBool = AtomicBool::new(false);

/// The halts' map (coin -> its last sign), the lock held only inside `f` (nothing in `f` takes it again).
fn halts<R>(f: impl FnOnce(&mut HashMap<String, f64>) -> R) -> R {
    #[cfg(not(test))]
    return f(HALTS.lock().unwrap().get_or_insert_with(HashMap::new));
    #[cfg(test)]
    return HALTS.with(|h| f(h.borrow_mut().get_or_insert_with(HashMap::new)));
}

fn last_mids<R>(f: impl FnOnce(&mut HashMap<String, f64>) -> R) -> R {
    #[cfg(not(test))]
    return f(LAST_MID.lock().unwrap().get_or_insert_with(HashMap::new));
    #[cfg(test)]
    return LAST_MID.with(|h| f(h.borrow_mut().get_or_insert_with(HashMap::new)));
}

/// A sign on `coin` now (`why` for the log and the chat, told once a pause).
pub(crate) fn note(coin: &str, why: &str) {
    let t = now();
    let new = halts(|m| m.insert(coin.to_string(), t).is_none_or(|x| t - x >= PAUSE_S));
    DIRTY.store(true, Ordering::Relaxed);
    if new {
        log!("halt: {coin} {why}: no new trade for {:.0} h", PAUSE_S / 3600.0);
        crate::notify::send(format!("⏸ {coin}: {why} — новые входы по монете на паузе {:.0} ч (открытые сделки со своими стопами)", PAUSE_S / 3600.0));
    }
}

/// No new trade on `coin` now.
pub(crate) fn paused(coin: &str) -> bool {
    let t = now();
    halts(|m| m.get(coin).is_some_and(|x| t - x < PAUSE_S))
}

/// An order's refusal `err` on `coin`: "Trading is halted" a sign.
pub(crate) fn note_error(coin: &str, err: &str) {
    if err.contains("Trading is halted") {
        note(coin, "торги остановлены биржей");
    }
}

/// A mid read: a HIP-3 coin's `JUMP` off its last read a sign.
pub(crate) fn mid_seen(coin: &str, px: f64) {
    if !coin.contains(':') || px <= 0.0 {
        return;
    }
    let prev = last_mids(|m| m.insert(coin.to_string(), px));
    if let Some(p) = prev
        && p > 0.0
        && (px / p - 1.0).abs() > JUMP
    {
        note(coin, &format!("цена прыгнула {} -> {} ({:+.0}%)", crate::notify::px(p), crate::notify::px(px), (px / p - 1.0) * 100.0));
    }
}

/// The pauses still on, as the doc.
fn snapshot() -> Value {
    let t = now();
    halts(|m| {
        m.retain(|_, x| t - *x < PAUSE_S);
        json!(m.clone())
    })
}

/// The doc saved when a sign came since the last save.
pub(crate) async fn save(cfg: &Config) {
    if DIRTY.swap(false, Ordering::Relaxed)
        && let Err(e) = crate::store::save_doc(cfg, DOC, &snapshot()).await
    {
        DIRTY.store(true, Ordering::Relaxed);
        log!("halt: not saved: {e:#}");
    }
}

/// The pauses saved, back at the start (before any strategy).
pub(crate) async fn load(cfg: &Config) {
    match crate::store::load_doc(cfg, DOC).await {
        Ok(Some(v)) => {
            let n = halts(|m| {
                for (c, x) in v.as_object().into_iter().flatten() {
                    if let Some(x) = x.as_f64() {
                        m.insert(c.clone(), x);
                    }
                }
                m.len()
            });
            let on: Vec<String> = halts(|m| m.iter().filter(|(_, x)| now() - **x < PAUSE_S).map(|(c, _)| c.clone()).collect());
            log!("halt: {n} read, paused: {on:?}");
        }
        Ok(None) => {}
        Err(e) => log!("halt: not read: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Clock, cfg};

    #[test]
    fn a_halt_s_refusal_pauses_the_coin_a_day_from_its_last_sign() {
        let c = Clock::at(1_000_000.0);
        assert!(!paused("xyz:HLT"));
        note_error("xyz:HLT", r#"xyz:HLT order not placed: {"error":"Insufficient margin to place order."}"#);
        assert!(!paused("xyz:HLT"), "another refusal: no sign");
        note_error("xyz:HLT", r#"xyz:HLT order not placed: {"error":"Trading is halted."}"#);
        assert!(paused("xyz:HLT") && !paused("xyz:OTH"));
        // every strategy's entries go through the family's check
        assert!(!crate::trades::can_open("xyz:HLT", "wick", 1.0, 1.0) && !crate::trades::can_open("xyz:HLT", "wkd", 1.0, 1.0));
        // refused again 4 h on (still halted): the day from then
        c.go(4.0 * 3600.0);
        note_error("xyz:HLT", "Trading is halted.");
        c.go(PAUSE_S - 1.0);
        assert!(paused("xyz:HLT"));
        c.go(1.0);
        assert!(!paused("xyz:HLT"));
    }

    #[test]
    fn a_hip3_mid_jumping_a_quarter_pauses_it_a_crypto_one_does_not() {
        let _c = Clock::at(2_000_000.0);
        // 2026-11-20 xyz:SNDK: 753.69 before the halt, 92.58 at the reopening
        mid_seen("xyz:SPL", 753.69);
        mid_seen("xyz:SPL", 760.0);
        assert!(!paused("xyz:SPL"), "a move of 1%");
        mid_seen("xyz:SPL", 92.58);
        assert!(paused("xyz:SPL"));
        // up as well
        mid_seen("xyz:UPJ", 10.0);
        mid_seen("xyz:UPJ", 12.6);
        assert!(paused("xyz:UPJ"));
        mid_seen("xyz:UPK", 10.0);
        mid_seen("xyz:UPK", 12.4);
        assert!(!paused("xyz:UPK"));
        // a new listing's crypto mid may run: not read
        mid_seen("NEWC", 1.0);
        mid_seen("NEWC", 3.0);
        assert!(!paused("NEWC"));
    }

    #[tokio::test]
    async fn the_pauses_outlive_a_restart() {
        let _c = Clock::at(3_000_000.0);
        let k = cfg();
        note("xyz:RST", "test");
        save(&k).await;
        halts(|m| m.clear());
        assert!(!paused("xyz:RST"));
        load(&k).await;
        assert!(paused("xyz:RST"));
        // nothing new: nothing saved
        assert!(!DIRTY.load(Ordering::Relaxed) || { save(&k).await; true });
    }
}
