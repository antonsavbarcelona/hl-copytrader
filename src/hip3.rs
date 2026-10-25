//! The dip family on the HIP-3 perps (`HIP3_ENABLED=1`, off by default): a coin of a HIP-3 dex
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
//! Prices: every dex's mids (`allMids`) each `POLL_S`; volumes each 5 min; the dexes each hour.
//! Its state is the store's doc `hip3` (the pending results, the coins' last moves).

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{Api, now};
use crate::config::Config;
use crate::dip::{Closes, cooled};
use crate::log;
use crate::store::Store;

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
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { drop: v("HIP3_DROP_PCT", 8.0) / 100.0, btc_max: v("HIP3_BTC_MAX_PCT", 2.0) / 100.0, min_vol: v("HIP3_MIN_VOL_USD", 10e6),
            hold_s: v("HIP3_HOLD_MIN", 30.0) * 60.0 }
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
        "signal" => format!("👀 HIP-3 сигнал (без сделки) {coin}: упала на {:.1}% за {} мин, цена {}, оборот ${:.0}M", f("fall") * 100.0, f("window"), f("mid"), f("volume") / 1e6),
        "result" => format!("{} итог HIP-3 сигнала {coin} через {:.0} мин: {} -> {} ({:+.2}% для лонга, до комиссий)",
            if f("ret_pct") >= 0.0 { "✅" } else { "🔻" }, f("after_min"), f("mid_signal"), f("mid"), f("ret_pct")),
        _ => return None,
    })
}

/// Starts the HIP-3 dip's signals if `HIP3_ENABLED` is "1" (see the module).
pub async fn start(cfg: &Config, store: Store, api: Api) -> Result<()> {
    if std::env::var("HIP3_ENABLED").unwrap_or_default().trim() != "1" {
        log!("hip3: off (HIP3_ENABLED=1 to run its signals)");
        return Ok(());
    }
    let p = Params::from_env();
    let mut state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    log!("hip3: signals only; a fall of {:.0}% in 5 or 15 min (BTC under {:.0}%, ${:.0}M+ a day), the result {:.0} min on; {} pending",
        p.drop * 100.0, p.btc_max * 100.0, p.min_vol / 1e6, p.hold_s / 60.0, state.pending.len());
    crate::notify::send(format!("📈 HIP-3 покупка падений запущена БЕЗ СДЕЛОК (только сигналы)
сигнал: падение {:.0}% за 5 или 15 мин (BTC меньше {:.0}%, оборот от ${:.0}M), итог через {:.0} мин", p.drop * 100.0, p.btc_max * 100.0, p.min_vol / 1e6, p.hold_s / 60.0));
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let mut closes = Closes::default();
        let (mut dexes, mut dex_at, mut vol_at): (Vec<String>, f64, f64) = (Vec::new(), 0.0, 0.0);
        let mut volume: HashMap<String, f64> = HashMap::new();
        let mut last_minute = (now() / 60.0).floor() as i64;
        let mut checked = false;
        let started = now();
        let event = |store: &Store, mut ev: Value| {
            ev["kind"] = json!("hip3");
            ev["at"] = json!(now());
            log!("hip3: {ev}");
            if let Some(m) = message(&ev) {
                crate::notify::send(m);
            }
            store.event(ev);
        };
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
                        }
                    }
                    Err(e) => log!("hip3: {d} mids not read: {e:#}"),
                }
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
        let p = Params { drop: 0.08, btc_max: 0.02, min_vol: 10e6, hold_s: 1800.0 };
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
        let m = message(&json!({"what": "result", "coin": "xyz:A", "after_min": 30.0, "mid_signal": 90.0, "mid": 92.0, "ret_pct": 2.22})).unwrap();
        assert!(m.contains("xyz:A") && m.contains("+2.22%"));
    }
}
