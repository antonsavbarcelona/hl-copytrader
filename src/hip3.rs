//! The dip family on the HIP-3 perps (`HIP3_ENABLED`: on unless "0"): a coin of a HIP-3 dex
//! (xyz's stocks, vntl's pre-IPO names, ...) with `HIP3_MIN_VOL_USD` (10M) or more a day whose mid
//! fell `HIP3_DROP_PCT` (8) or more over 5 or over 15 minutes, by the minutes' closes, while BTC
//! moved less than `HIP3_BTC_MAX_PCT` (2) over the same minutes; a coin not again for 240 min after
//! its move of the drop either way. For now the signals only (event kind `hip3`: `signal`, and
//! `result` `HIP3_HOLD_MIN` (30) later at the mid), to the chat too: the orders on the HIP-3 dexes
//! come once the live signals match the research.
//!
//! The archive research of 2026-10-25 (the raw tape, Dec 2025 .. Oct 2026; the buy at the worst
//! trade of the first block a second after the signal, out at the block 30 min on, the HIP-3 fee
//! 9 bp + 5 bp slippage a side): $10M+ a day: 54 falls, +2.05% a trade, 74% up, 10 of 10 months up;
//! at a 10% risk a trade with a 20% stop (half the equity a trade, 3 open, reinvested) +67% with
//! an 11% drawdown with the open. The thin coins ($1M+) more often (146) but partly untradable.
//!
//! The dip's own buys were switched off on 2026-11-11 (the user's call: 7 live trades -$13, its paper +0.8% a trade) and
//! their code removed on 2026-11-14; `HIP3_LIVE` (on unless "0") now opens the venue for the weekend dips (`wkd`). Before:
//! `HIP3_LIVE=1`: the signals traded
//! too, on the dexes margined in USDC: bought at market, a resting stop `HIP3_STOP_PCT` (20) under,
//! out at market `HIP3_HOLD_MIN` later, the stop losing `HIP3_RISK_USD` ($20; `FAMILY_RISK_PCT` of
//! the equity when set), within `HIP3_MAX_OPEN` (3) and the family's cap, isolated at 3x (see
//! `trades`); events `open` / `close` / `error`.
//!
//! Prices: every dex's mids (`allMids`) each `POLL_S`; volumes each 5 min; the dexes each hour.
//! Its state is the store's doc `hip3` (the pending results, the coins' last moves).

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{Api, now};
use crate::config::Config;
use crate::dip::{Closes, cooled};
use crate::notify::usd;
use crate::log;
use crate::keeper::{HeldIn, Keeper};
use crate::store::Store;
use crate::venue::Venue;

const DOC: &str = "hip3";
const POLL_S: f64 = 10.0;
const VOLUME_EVERY_S: f64 = 300.0;
const DEXES_EVERY_S: f64 = 3600.0;
/// The windows (min) a fall is read over.
const WINDOWS: [i64; 2] = [5, 15];
const COOLDOWN_MIN: i64 = 240;

#[derive(Clone, Debug)]
struct Params {
    drop: f64,
    btc_max: f64,
    min_vol: f64,
    hold_s: f64,
    /// The venue for the weekend dips' trades (`HIP3_LIVE` on unless "0").
    live: bool,
}

impl Params {
    fn from_env() -> Self {
        let v = crate::trades::env_f;
        Params { drop: v("HIP3_DROP_PCT", 8.0) / 100.0, btc_max: v("HIP3_BTC_MAX_PCT", 2.0) / 100.0, min_vol: v("HIP3_MIN_VOL_USD", 10e6),
            hold_s: v("HIP3_HOLD_MIN", 30.0) * 60.0, live: crate::trades::switch("HIP3_LIVE").is_some() }
    }
}

/// A signal whose result is logged a hold on.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pending {
    coin: String,
    at: f64,
    mid: f64,
    fall: f64,
    window: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    pending: Vec<Pending>,
    #[serde(default)]
    last_move: HashMap<String, i64>,
    #[serde(default)]
    signals: u64,
    /// The weekend dips' shadow (`wkd`).
    #[serde(default)]
    wkd: crate::wkd::Book,
}

/// The falls of `minute` worth a signal: (coin, fall, window), biggest first. A coin's move of the
/// drop either way over either window (any volume) starts its cooldown; the falls on the liquid
/// coins outside one, with BTC calm over the same window, are the signals.
fn falls(closes: &Closes, volume: &HashMap<String, f64>, last_move: &mut HashMap<String, i64>, minute: i64, p: &Params) -> Vec<(String, f64, i64)> {
    let mut out = Vec::new();
    for c in closes.0.keys() {
        if c == "BTC" {
            continue;
        }
        // The biggest move of the two windows, and its window.
        let Some((r, w)) = WINDOWS.iter().filter_map(|&w| closes.change(c, minute, w).map(|r| (r, w))).max_by(|a, b| a.0.abs().total_cmp(&b.0.abs())) else { continue };
        if r.abs() < p.drop || cooled(last_move, c, minute, COOLDOWN_MIN) {
            continue;
        }
        let btc = closes.change("BTC", minute, w);
        if r <= -p.drop && volume.get(c).copied().unwrap_or(0.0) >= p.min_vol && btc.is_some_and(|b| b.abs() < p.btc_max) {
            out.push((c.clone(), r, w));
        }
    }
    out.sort_by(|a, b| a.1.total_cmp(&b.1));
    out
}

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    Some(match ev["what"].as_str()? {
        "signal" | "result" if !crate::notify::PAPER_TO_CHAT => return None,
        "signal" => format!("👀 HIP-3 сигнал (без сделки) {coin}: упала на {:.1}% за {} мин, цена {}, оборот ${:.0}M", f("fall") * 100.0, f("window"), f("mid"), f("volume") / 1e6),
        "open" => format!("🟢 HIP-3 КУПИЛ {coin}: упала на {:.1}%, цена {}, на ${:.0}, стоп {}, выход через {:.0} мин", f("fall") * 100.0, f("px"), f("notional"), f("stop_px"), f("hold_min")),
        "close" => format!("{} HIP-3 {} {coin}: {} -> {} ({:+.2}%, {})", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            match ev["why"].as_str().unwrap_or("") { "stop" => "СТОП", "take" => "ТЕЙК", "manual" => "ЗАКРЫТО ВНЕ БОТА (руками или ликвидация)", _ => "ЗАКРЫЛ" }, f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_usd"))),
        "error" => format!("⚠️ HIP-3 {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        "result" => format!("{} итог HIP-3 сигнала {coin} через {:.0} мин: {} -> {} ({:+.2}% для лонга, до комиссий)",
            if f("ret_pct") >= 0.0 { "✅" } else { "🔻" }, f("after_min"), f("mid_signal"), f("mid"), f("ret_pct")),
        _ => return crate::wkd::message(ev),
    })
}

/// An event of the HIP-3 family (the dip's paper, the weekend dips): logged, told, stored.
fn event(store: &Store, mut ev: Value) {
    ev["kind"] = json!("hip3");
    ev["at"] = json!(now());
    log!("hip3: {ev}");
    if let Some(m) = message(&ev) {
        crate::notify::send(m);
    }
    store.event(ev);
}

/// The weekend dips' keeping: events of kind "hip3" ("wkd_exit" / "wkd_error"), trades "wkd" in the family registry.
fn wkd_keeper(v: Venue, unified: bool, cfg: Config, store: Store) -> Keeper {
    Keeper::new(v, unified, cfg, store, "hip3", message, HeldIn::Hours).registered_as("wkd").words("wkd_exit", "wkd_error")
}

/// Starts the HIP-3 dip's signals unless `HIP3_ENABLED` is "0" (see the module).
pub async fn start(cfg: &Config, store: Store, api: Api) -> Result<()> {
    if crate::trades::switch("HIP3_ENABLED").is_none() {
        log!("hip3: off (HIP3_ENABLED=0)");
        return Ok(());
    }
    let p = Params::from_env();
    let mut state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    log!("hip3: signals only; a fall of {:.0}% in 5 or 15 min (BTC under {:.0}%, ${:.0}M+ a day), the result {:.0} min on; {} pending",
        p.drop * 100.0, p.btc_max * 100.0, p.min_vol / 1e6, p.hold_s / 60.0, state.pending.len());
    // The live venue with the HIP-3 perps (HIP3_LIVE on unless "0"): without it the signals only.
    let mut venue: Option<(crate::venue::Venue, HashMap<String, u32>)> = None;
    let mut unified = false;
    if p.live {
        match crate::venue::venue_from_env(None).await {
            Ok(Some((v, u))) => match v.with_hip3().await {
                Ok(x) => {
                    unified = u;
                    venue = Some(x);
                }
                Err(e) => log!("hip3: the HIP-3 perps not read, signals only: {e:#}"),
            },
            Ok(None) => log!("hip3: LIVE_KEY / LIVE_ACCOUNT not set, signals only"),
            Err(e) => log!("hip3: the venue not read, signals only: {e:#}"),
        }
    }
    crate::notify::send("📈 HIP-3 покупка падений: только на бумаге (сигналы и итог через 30 мин), без сделок".to_string());
    let wp = crate::wkd::Params::from_env();
    for tr in &state.wkd.live {
        crate::trades::register(&tr.coin, "wkd", tr.risk(), tr.size);
    }
    let wkd_mode = match (&venue, &wp.live) {
        (None, _) | (_, None) => "БЕЗ СДЕЛОК (только сигналы)".to_string(),
        (_, Some(None)) => format!("НА ДЕНЬГИ: риск {}, до {} позиций", crate::trades::sizing_label(wp.risk_usd, wp.stop()), wp.max_open),
        (_, Some(Some(n))) => format!("НА ДЕНЬГИ: риск {}, до {} позиций, ПРОБА: первые {n} сделки (было {}), потом только сигналы",
            crate::trades::sizing_label(wp.risk_usd, wp.stop()), wp.max_open, state.wkd.opened),
    };
    log!("hip3: the weekend dips: {}; {wkd_mode}; {} signals so far, {} live held", wp.label(), state.wkd.signals, state.wkd.live.len());
    crate::notify::send(format!("🧪 HIP-3 {}, {wkd_mode}", wp.label()));
    let cfg = cfg.clone();
    let wkd_k = venue.as_ref().map(|(v, _)| wkd_keeper(v.clone(), unified, cfg.clone(), store.clone()));
    tokio::spawn(async move {
        let mut closes = Closes::default();
        let mut lows: HashMap<String, (i64, f64)> = HashMap::new();
        let (mut dexes, mut dex_at, mut vol_at): (Vec<String>, f64, f64) = (Vec::new(), 0.0, 0.0);
        let mut volume: HashMap<String, f64> = HashMap::new();
        let mut last_minute = (now() / 60.0).floor() as i64;
        let mut checked = false;
        let started = now();
        let mut wkd_pos_at = 0.0;
        let save = |cfg: &Config, state: &State| {
            let (cfg, v) = (cfg.clone(), serde_json::to_value(state).unwrap_or_default());
            tokio::spawn(async move {
                if let Err(e) = crate::store::save_doc(&cfg, DOC, &v).await {
                    log!("hip3: state not saved: {e:#}");
                }
            });
        };
        let mut tick = tokio::time::interval(std::time::Duration::from_secs_f64(POLL_S));
        loop {
            tick.tick().await;
            let t = now();
            if t - dex_at >= DEXES_EVERY_S || dexes.is_empty() {
                match api.dexes().await {
                    Ok(d) => {
                        dexes = d;
                        dex_at = t;
                    }
                    Err(e) => log!("hip3: dexes not read: {e:#}"),
                }
            }
            if t - vol_at >= VOLUME_EVERY_S {
                let mut v = HashMap::new();
                for d in &dexes {
                    match api.dex_volumes(d).await {
                        Ok(x) => v.extend(x),
                        Err(e) => log!("hip3: {d} volumes not read: {e:#}"),
                    }
                }
                if !v.is_empty() {
                    volume = v;
                    vol_at = t;
                }
            }
            let minute = (t / 60.0).floor() as i64;
            let keep = WINDOWS[1] as usize + 3;
            if let Ok(m) = api.mids("").await {
                if let Some(b) = m.get("BTC") {
                    closes.update("BTC", minute, *b, keep);
                }
            }
            for d in &dexes {
                match api.mids(d).await {
                    Ok(m) => {
                        for (c, px) in m {
                            closes.update(&c, minute, px, keep);
                            let lo = lows.entry(c).or_insert((minute, px));
                            if lo.0 != minute || px < lo.1 {
                                *lo = (minute, if lo.0 == minute { lo.1.min(px) } else { px });
                            }
                        }
                    }
                    Err(e) => log!("hip3: {d} mids not read: {e:#}"),
                }
            }
            // The weekend dips' shadow: the signals and their paper results.
            let (wev, wentries) = state.wkd.step(&wp, &closes, &lows, &volume, minute, t);
            let mut wchanged = !wev.is_empty();
            for ev in wev {
                event(&store, ev);
            }
            // Traded (WKD_LIVE): bought on the poll after the signal (px: the signal's mid, or the paper entry), within the trial, the slots and the family's cap.
            if let Some((v, _)) = &venue {
                for (coin, px) in wentries {
                    let Some(trial) = wp.live else { continue };
                    if !crate::trades::trial_open(trial, state.wkd.opened) {
                        log!("hip3: weekend {coin} not traded (the trial's {} trades done)", state.wkd.opened);
                        continue;
                    }
                    let risk = crate::trades::risk_usd(wp.risk_usd);
                    if !v.coins.contains_key(&coin) || state.wkd.live.len() >= wp.max_open || !crate::trades::can_open(&coin, "wkd", 1.0, risk) {
                        log!("hip3: weekend {coin} not traded (not on a USDC dex, at most open, held or the family's risk cap)");
                        continue;
                    }
                    let mid = closes.at(&coin, i64::MAX).unwrap_or(px);
                    let size = crate::trades::size_for(risk, wp.stop(), mid);
                    let book = v.book_now(&coin, true, size).await;
                    let r = crate::trades::open_market_at(v, &coin, size, wp.stop(), Some(wp.take()), wp.hold_s(), mid).await;
                    match r {
                        Ok(tr) => {
                            event(&store, json!({"what": "wkd_open", "coin": coin, "px": tr.entry_px, "paper_px": px, "mid": mid, "book": book, "size": tr.size, "notional": tr.size * tr.entry_px,
                                "stop_px": tr.stop_px, "take_px": tr.take_px, "hold_h": wp.hold_s() / 3600.0}));
                            crate::trades::register(&coin, "wkd", tr.risk(), tr.size);
                            state.wkd.live.push(tr);
                            state.wkd.opened += 1;
                            if !crate::trades::trial_open(trial, state.wkd.opened) {
                                crate::notify::send(format!("🏁 HIP-3 выходные: проба окончена ({} сделки), дальше только сигналы", state.wkd.opened));
                            }
                        }
                        Err(e) => event(&store, json!({"what": "wkd_error", "coin": coin, "error": format!("{e:#}")})),
                    }
                    wchanged = true;
                }
                // Out on time; a position gone (its stop or take filled) ends its trade.
                if !state.wkd.live.is_empty() {
                    let check = t - wkd_pos_at >= 30.0;
                    if check {
                        wkd_pos_at = t;
                    }
                    if let Some(k) = &wkd_k {
                        wchanged |= k.sweep(&mut state.wkd.live, |t| t, true, check).await;
                    }
                }
            }
            if wchanged {
                save(&cfg, &state);
            }
            if !checked && (t - started > 120.0) {
                checked = true;
                let liquid = volume.values().filter(|v| **v >= p.min_vol).count();
                let ok = !dexes.is_empty() && !volume.is_empty() && closes.at("BTC", i64::MAX).is_some();
                crate::notify::send(format!("{} HIP-3 health check: бирж {}, монет {}, ликвидных (от ${:.0}M): {}, BTC {}",
                    if ok { "✅" } else { "⚠️" }, dexes.len(), closes.0.len().saturating_sub(1), p.min_vol / 1e6, liquid,
                    closes.at("BTC", i64::MAX).map(|x| format!("{x:.0}")).unwrap_or("-".into())));
            }
            if minute > last_minute && (minute % 10 == 0 || !checked) {
                log!("hip3: {} dexes, {} coins priced, {} with ${:.0}M+ a day, BTC {}", dexes.len(), closes.0.len(), volume.values().filter(|v| **v >= p.min_vol).count(),
                    p.min_vol / 1e6, closes.at("BTC", i64::MAX).map(|x| format!("{x:.0}")).unwrap_or("-".into()));
            }
            if minute > last_minute {
                // The minute before closed: its falls.
                let before = state.last_move.len();
                let sig = falls(&closes, &volume, &mut state.last_move, minute - 1, &p);
                let mut changed = state.last_move.len() != before || state.last_move.values().any(|m| *m == minute - 1);
                state.last_move.retain(|_, m| minute - 1 - *m < COOLDOWN_MIN);
                for (coin, fall, w) in sig {
                    let Some(mid) = closes.at(&coin, i64::MAX) else { continue };
                    state.signals += 1;
                    state.pending.push(Pending { coin: coin.clone(), at: t, mid, fall, window: w });
                    event(&store, json!({"what": "signal", "coin": coin, "fall": fall, "window": w, "mid": mid, "volume": volume.get(&coin).copied().unwrap_or(0.0),
                        "btc": closes.change("BTC", minute - 1, w)}));
                    changed = true;
                }
                if changed {
                    save(&cfg, &state);
                }
                last_minute = minute;
            }
            // The results due.
            let due: Vec<Pending> = state.pending.iter().filter(|x| t - x.at >= p.hold_s).cloned().collect();
            if !due.is_empty() {
                for x in &due {
                    let mid = closes.at(&x.coin, i64::MAX).unwrap_or(0.0);
                    event(&store, json!({"what": "result", "coin": x.coin, "fall": x.fall, "window": x.window, "mid_signal": x.mid, "mid": mid,
                        "after_min": (t - x.at) / 60.0, "ret_pct": if mid > 0.0 { (mid / x.mid - 1.0) * 100.0 } else { 0.0 }}));
                }
                state.pending.retain(|x| t - x.at < p.hold_s);
                save(&cfg, &state);
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fall_over_either_window_with_btc_calm_and_volume() {
        let p = Params { drop: 0.08, btc_max: 0.02, min_vol: 10e6, hold_s: 1800.0, live: false };
        let mut c = Closes::default();
        for m in 100..=116 {
            c.update("BTC", m, 100_000.0, 20);
            // xyz:A falls 10% in the last 5 minutes; xyz:B 10% but thin; xyz:C 5%
            c.update("xyz:A", m, if m >= 114 { 90.0 } else { 100.0 }, 20);
            c.update("xyz:B", m, if m >= 114 { 90.0 } else { 100.0 }, 20);
            c.update("xyz:C", m, if m >= 114 { 95.0 } else { 100.0 }, 20);
        }
        let vol: HashMap<String, f64> = [("xyz:A".to_string(), 20e6), ("xyz:B".to_string(), 1e6), ("xyz:C".to_string(), 20e6)].into();
        let mut last = HashMap::new();
        let s = falls(&c, &vol, &mut last, 116, &p);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, "xyz:A");
        assert!((s[0].1 + 0.10).abs() < 1e-9);
        // The thin coin's fall still starts its cooldown; the next minute neither signals again.
        assert!(last.contains_key("xyz:B") && last.contains_key("xyz:A"));
        assert!(falls(&c, &vol, &mut last, 116, &p).is_empty());
        // BTC moving 3% over the window: no signal.
        let mut c2 = Closes::default();
        for m in 100..=116 {
            c2.update("BTC", m, if m >= 114 { 97_000.0 } else { 100_000.0 }, 20);
            c2.update("xyz:A", m, if m >= 114 { 90.0 } else { 100.0 }, 20);
        }
        assert!(falls(&c2, &vol, &mut HashMap::new(), 116, &p).is_empty());
        let m = message(&json!({"what": "result", "coin": "xyz:A", "after_min": 30.0, "mid_signal": 90.0, "mid": 92.0, "ret_pct": 2.22}));
        // the paper side off the chat (`notify::PAPER_TO_CHAT`): nothing sent
        assert!(if crate::notify::PAPER_TO_CHAT { m.is_some_and(|m| m.contains("xyz:A") && m.contains("+2.22%")) } else { m.is_none() });
        // a live trade's close is still told
        assert!(message(&json!({"what": "close", "coin": "xyz:A", "why": "time", "entry_px": 90.0, "px": 92.0, "ret_pct": 2.22, "pnl_usd": 2.5})).is_some());
    }
}

#[cfg(test)]
mod wkd_lifecycle {
    use super::*;
    use crate::testkit::{FakeHl, cfg};
    use crate::trades::Trade;

    async fn sweep_wkd(v: &Venue, store: &Store, live: &mut Vec<Trade>, _t: f64, check: bool) -> bool {
        wkd_keeper(v.clone(), false, cfg(), store.clone()).sweep(live, |t| t, true, check).await
    }

    async fn held(f: &FakeHl, coin: &str) -> Trade {
        let tr = crate::trades::open_market(&f.venue(), coin, 2.0, 0.05, Some(0.03), 6.0 * 3600.0).await.unwrap();
        crate::trades::register(coin, "wkd", tr.risk(), tr.size);
        tr
    }

    #[tokio::test]
    async fn a_stop_or_a_take_filled_ends_the_weekend_trade_when_the_positions_are_read() {
        let f = FakeHl::start(&[("xyz:WKA", 2, 10, 100.0), ("xyz:WKB", 2, 10, 50.0)]).await;
        let (store, mut rx) = Store::for_test();
        let mut live = vec![held(&f, "xyz:WKA").await, held(&f, "xyz:WKB").await];
        f.set_mid("xyz:WKA", 94.0);                       // under the stop
        f.set_mid("xyz:WKB", 52.0);                       // over the take
        // not a check: nothing read, nothing ended (the time not due)
        assert!(!sweep_wkd(&f.venue(), &store, &mut live, now(), false).await);
        assert_eq!(live.len(), 2);
        assert!(crate::store::drain(&mut rx).is_empty());
        assert!(sweep_wkd(&f.venue(), &store, &mut live, now(), true).await);
        assert!(live.is_empty());
        let ev = crate::store::drain(&mut rx);
        assert_eq!(ev.len(), 2, "{ev:?}");
        let by = |c: &str| ev.iter().find(|e| e["coin"] == c).unwrap().clone();
        let (a, b) = (by("xyz:WKA"), by("xyz:WKB"));
        assert_eq!((a["kind"].as_str(), a["what"].as_str(), a["why"].as_str()), (Some("hip3"), Some("wkd_exit"), Some("stop")));
        assert_eq!((b["what"].as_str(), b["why"].as_str()), (Some("wkd_exit"), Some("take")));
        assert!(a["ret_pct"].as_f64().unwrap() < -4.9 && a["held_h"].as_f64().is_some() && b["pnl_usd"].as_f64().unwrap() > 0.0);
        assert!(!crate::trades::registered("xyz:WKA") && !crate::trades::registered("xyz:WKB"));
    }

    #[tokio::test]
    async fn its_time_up_it_goes_out_at_market_without_a_check_and_a_failed_close_is_kept() {
        let f = FakeHl::start(&[("xyz:WKC", 2, 10, 100.0), ("xyz:WKD", 2, 10, 100.0)]).await;
        let (store, mut rx) = Store::for_test();
        let mut c = held(&f, "xyz:WKC").await;
        c.exit_at = now() - 1.0;
        let mut live = vec![c, held(&f, "xyz:WKD").await];
        f.set_mid("xyz:WKC", 101.0);
        assert!(sweep_wkd(&f.venue(), &store, &mut live, now(), false).await);
        assert_eq!(live.len(), 1);
        let ev = crate::store::drain(&mut rx);
        assert_eq!((ev.len(), ev[0]["why"].as_str(), ev[0]["coin"].as_str()), (1, Some("time"), Some("xyz:WKC")), "{ev:?}");
        assert!(f.position("xyz:WKC") == 0.0 && f.open_orders("xyz:WKC").is_empty());
        // the close refused: told as an error, the trade kept for the next poll
        live[0].exit_at = now() - 1.0;
        f.with(|s| s.ioc_needs = 0.5);
        assert!(!sweep_wkd(&f.venue(), &store, &mut live, now(), false).await);
        assert_eq!(live.len(), 1);
        let ev = crate::store::drain(&mut rx);
        assert!(ev.iter().any(|e| e["what"] == "wkd_error" && e["error"].as_str().unwrap().starts_with("exit:")), "{ev:?}");
        assert!(crate::trades::registered("xyz:WKD"));
    }
}
