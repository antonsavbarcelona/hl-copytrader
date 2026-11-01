//! The HIP-3 closed-market dips (V85 hypothesis 47, V87; V185): on Saturday or Sunday (UTC) a stock of a
//! USDC-margined HIP-3 dex (xyz, para, mkts, io, abcd; US, Asian and pre-IPO stocks and the indices; not the commodities, FX,
//! the rates or the crypto-linked names), and on the US weeknights (Mon-Thu 20:00 .. 04:00 New York time, the US cash market
//! shut) a US stock of the same dexes, with
//! `WKD_MIN_VOL_USD` ($100k)+ a day whose mid goes `WKD_DROP_PCT` (4) or more under the highest close of the 15
//! minutes before: a signal; a coin not again for 120 min. Bought on paper at the close of the minute after (the
//! research's entry), out at +`WKD_TAKE_PCT` (5; a maker 3 bp), -`WKD_STOP_PCT` (10) or `WKD_HOLD_H` (24) h on, by
//! the mids, less 9 bp taker + 5 bp slippage a side. Events kind `hip3`, `wkd_signal` / `wkd_close`, to the chat too.
//! Run inside the HIP-3 loop (its 10 s mids, its volumes); its state with the `hip3` doc.
//!
//! Traded (`WKD_LIVE`: unset or "1" live; set empty the trial, its first `TRIAL_TRADES` (3) live, then the shadow only; "0" the shadow
//! only; the HIP-3 venue needed, as the HIP-3 dip's): at the paper entry (the close of the minute after the signal) bought
//! at market, a resting stop -`WKD_STOP_PCT` and a resting take +`WKD_TAKE_PCT` (reduce-only), out at market `WKD_HOLD_H`
//! on; the stop losing `WKD_RISK_USD` ($20; `FAMILY_RISK_PCT` of the equity when set), at most `WKD_MAX_OPEN` (3) and the
//! family's cap, the HIP-3 leverage (cross 10x). Events `wkd_open` / `wkd_exit` / `wkd_error`. The paper record goes on
//! for every signal (`wkd_close`), traded or not.
//!
//! The research (the raw tape, 2025-12 .. 2026-10, 131 dips of 4%+, the tape's ask in / bid out): +104..+204 bp a trade,
//! 34 weekends, 56 coins, 82% of the weekends up, w/o the best 3 weekends +143%; 116 of the 131 not overlapping the
//! HIP-3 wick (+214 bp); the same dips in the session lose. The mids see the wicks shallower than the trades: fewer
//! signals here than on the tape.
//!
//! The nights (V184 .. V186, HL 1m 2025-10 .. 2026-10, the US stocks of the USDC dexes, 4%): 84 dips +198 bp a trade (t 3.9;
//! random night minutes -18), 75% up, 9 of 11 months up, w/o the 5 best stocks +144, costs x2 +170, the entry 5 min late
//! +125; on the raw tape (the ask in, the bid out) 73 trades +203 bp, w/o the 5 best +152; the same dips in the session
//! (-65 bp), the pre-market (-46) and 16:00 .. 20:00 (the earnings hours, ~0) do not pay; 0% on a weekend day, 13% on a
//! HIP-3 wick day; the HL portfolio at 2% with them x10.5 -> x13.7 at the same drawdown 28%.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dip::Closes;

const DEXES: [&str; 5] = ["xyz", "para", "mkts", "io", "abcd"];
/// The HIP-3 names left out: the commodities, FX, the rates and the crypto-linked ones (their markets do not close for
/// the weekend). The Asian stocks, the pre-IPO names and the indices are in since 2026-10-31 (V182 / V183, HL 1m
/// 2025-10 .. 2026-10, the USDC dexes, the 4% dip: 33 trades +217 bp, w/o the 5 best +205, 6 of 8 months up; on the
/// weekdays the same dips lose; with them the HL portfolio at 2% x9.4 -> x10.5 at the same drawdown 28%).
const EXCLUDED: &[&str] = &["GOLD", "SILVER", "CL", "OIL", "USOIL", "WTI", "BRENTOIL", "COPPER", "NATGAS", "GAS", "PLATINUM", "PALLADIUM",
    "URANIUM", "CORN", "WHEAT", "SOY", "COFFEE", "COCOA", "SUGAR", "ALUMINIUM", "ALUMINUM", "NICKEL", "ZINC", "LITHIUM", "HO", "EUR", "JPY",
    "DXY", "GBP", "USBOND", "10Y", "2Y", "USDE", "XMR", "BTCD", "OTHERS", "TOTAL2", "BVIV", "PURRDAT", "TREAD", "ANSEM", "BIRD", "CBRS"];
/// The tradable names that are not US stocks, left out of the US nights (their markets are open then, or have none; the
/// Korea ETFs KORU / EWY follow Seoul, open then: 10 night dips +61 bp vs +198 the rest).
const NON_US: &[&str] = &["ANTH", "CXMT", "DRAM", "EWJ", "EWT", "GIGADEV", "HYUNDAI", "JP225", "KIOXIA", "KORU", "EWY", "KR200", "MINIMAX", "OAI", "SHEIN",
    "SKHX", "SKHY", "SMALL2000", "SMSN", "SOFTBANK", "SP500", "SPCX", "UNITREE", "US500", "USA100", "USA500", "USTECH", "XYZ100", "ZHIPU"];
const REF_MIN: i64 = 15;
const GAP_MIN: i64 = 120;
const TAKER: f64 = 9e-4 + 5e-4;
const MAKER: f64 = 3e-4;

#[derive(Clone, Debug)]
pub(crate) struct Params {
    drop: f64,
    min_vol: f64,
    take: f64,
    stop: f64,
    hold_s: f64,
    pub(crate) risk_usd: f64,
    pub(crate) max_open: usize,
    /// None: the shadow only; Some(None) live; Some(Some(n)) the trial of n.
    pub(crate) live: Option<Option<u32>>,
}

impl Params {
    pub(crate) fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { drop: v("WKD_DROP_PCT", 4.0) / 100.0, min_vol: v("WKD_MIN_VOL_USD", 1e5), take: v("WKD_TAKE_PCT", 5.0) / 100.0,
            stop: v("WKD_STOP_PCT", 10.0) / 100.0, hold_s: v("WKD_HOLD_H", 24.0) * 3600.0, risk_usd: v("WKD_RISK_USD", 20.0),
            max_open: v("WKD_MAX_OPEN", 3.0) as usize, live: crate::trades::switch("WKD_LIVE") }
    }

    pub(crate) fn stop(&self) -> f64 {
        self.stop
    }

    pub(crate) fn take(&self) -> f64 {
        self.take
    }

    pub(crate) fn hold_s(&self) -> f64 {
        self.hold_s
    }

    pub(crate) fn label(&self) -> String {
        format!("выходные и ночи США, HIP-3 акции: падение {:.0}% под максимум 15 мин, оборот от ${:.0}k; тейк +{:.0}% / стоп −{:.0}% / {:.0} ч", self.drop * 100.0,
            self.min_vol / 1e3, self.take * 100.0, self.stop * 100.0, self.hold_s / 3600.0)
    }
}

/// A paper position: the signal's minute and mid, the entry once its minute closed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Pos {
    coin: String,
    minute: i64,
    mid: f64,
    fall: f64,
    entry: Option<f64>,
    entry_at: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Book {
    #[serde(default)]
    open: Vec<Pos>,
    #[serde(default)]
    last: HashMap<String, i64>,
    #[serde(default)]
    pub(crate) signals: u64,
    #[serde(default)]
    closed: u64,
    #[serde(default)]
    sum_pct: f64,
    /// The live trades held, and how many opened (the trial's count).
    #[serde(default)]
    pub(crate) live: Vec<crate::trades::Trade>,
    #[serde(default)]
    pub(crate) opened: u32,
}

/// Saturday or Sunday (UTC) for an epoch minute.
pub(crate) fn weekend(minute: i64) -> bool {
    (minute.div_euclid(1440) + 3).rem_euclid(7) >= 5
}

/// The days since 1970-01-01 of a civil date (H. Hinnant's days_from_civil).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The US daylight time on a UTC day: the 2nd Sunday of March .. the 1st Sunday of November.
fn us_dst(day: i64) -> bool {
    let y = 1970 + day.div_euclid(146_097) * 400 + (day.rem_euclid(146_097) as f64 / 365.2425) as i64;
    let y = if days_from_civil(y + 1, 1, 1) <= day { y + 1 } else if days_from_civil(y, 1, 1) > day { y - 1 } else { y };
    let sunday = |m: i64, n: i64| {
        let first = days_from_civil(y, m, 1);
        first + (6 - (first + 3).rem_euclid(7)) + 7 * (n - 1)
    };
    sunday(3, 2) <= day && day < sunday(11, 1)
}

/// A US weeknight (the cash market shut): Mon-Thu 20:00 .. the next day's 04:00, New York time.
pub(crate) fn us_night(minute: i64) -> bool {
    let et = minute - if us_dst(minute.div_euclid(1440)) { 240 } else { 300 };
    let (wd, m) = ((et.div_euclid(1440) + 3).rem_euclid(7), et.rem_euclid(1440));
    (wd <= 3 && m >= 1200) || ((1..=4).contains(&wd) && m < 240)
}

/// A stock or an index of a USDC dex (US, Asian, pre-IPO).
fn tradable(coin: &str) -> bool {
    coin.split_once(':').is_some_and(|(d, n)| DEXES.contains(&d) && !EXCLUDED.contains(&n))
}

/// A tradable US stock (for the nights).
pub(crate) fn us_stock(coin: &str) -> bool {
    tradable(coin) && coin.split_once(':').is_some_and(|(_, n)| !NON_US.contains(&n))
}

impl Book {
    /// One poll: `lows` the current minute's lowest mid a coin; the events to log and the paper entries made now (coin,
    /// px: the live buys). `minute` the current minute, `t` now (s).
    pub(crate) fn step(&mut self, p: &Params, closes: &Closes, lows: &HashMap<String, (i64, f64)>, volume: &HashMap<String, f64>, minute: i64, t: f64) -> (Vec<Value>, Vec<(String, f64)>) {
        let mut out = Vec::new();
        let mut entries = Vec::new();
        let (wkend, night) = (weekend(minute), us_night(minute));
        if wkend || night {
            for (c, &(m, lo)) in lows {
                if m != minute || !(if wkend { tradable(c) } else { us_stock(c) }) || volume.get(c).copied().unwrap_or(0.0) < p.min_vol || self.last.get(c).is_some_and(|x| minute - x < GAP_MIN) {
                    continue;
                }
                let Some(q) = closes.0.get(c) else { continue };
                let prev: Vec<f64> = q.iter().filter(|(mm, _)| *mm >= minute - REF_MIN && *mm < minute).map(|x| x.1).collect();
                if prev.len() < 10 {
                    continue;
                }
                let r = prev.iter().copied().fold(f64::MIN, f64::max);
                let fall = lo / r - 1.0;
                if fall <= -p.drop {
                    self.last.insert(c.clone(), minute);
                    self.signals += 1;
                    self.open.push(Pos { coin: c.clone(), minute, mid: lo, fall, entry: None, entry_at: 0.0 });
                    out.push(json!({"what": "wkd_signal", "coin": c, "night": !wkend, "fall": fall, "mid": lo, "ref": r, "volume": volume.get(c).copied().unwrap_or(0.0)}));
                }
            }
        }
        self.last.retain(|_, x| minute - *x < GAP_MIN);
        let mut keep = Vec::new();
        for mut x in std::mem::take(&mut self.open) {
            let Some(e) = x.entry else {
                if minute >= x.minute + 2 {
                    match closes.at(&x.coin, x.minute + 1) {
                        Some(px) if px > 0.0 => {
                            x.entry = Some(px);
                            x.entry_at = t;
                            entries.push((x.coin.clone(), px));
                        }
                        _ => continue,
                    }
                }
                keep.push(x);
                continue;
            };
            let Some(mid) = closes.at(&x.coin, i64::MAX) else {
                keep.push(x);
                continue;
            };
            let out_ = if mid >= e * (1.0 + p.take) {
                Some(("take", p.take - TAKER - MAKER))
            } else if mid <= e * (1.0 - p.stop) {
                Some(("stop", mid / e - 1.0 - 2.0 * TAKER))
            } else if t - x.entry_at >= p.hold_s {
                Some(("time", mid / e - 1.0 - 2.0 * TAKER))
            } else {
                None
            };
            match out_ {
                Some((why, ret)) => {
                    self.closed += 1;
                    self.sum_pct += ret * 100.0;
                    out.push(json!({"what": "wkd_close", "coin": x.coin, "why": why, "fall": x.fall, "entry_px": e, "px": mid, "ret_pct": ret * 100.0,
                        "held_h": (t - x.entry_at) / 3600.0, "closed": self.closed, "sum_pct": self.sum_pct}));
                }
                None => keep.push(x),
            }
        }
        self.open = keep;
        (out, entries)
    }
}

pub(crate) fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    Some(match ev["what"].as_str()? {
        "wkd_signal" => format!("🧪 HIP-3 {} (без сделки) {coin}: {:.1}% под максимум 15 мин, цена {}, оборот ${:.0}k",
            if ev["night"].as_bool().unwrap_or(false) { "ночь США" } else { "выходные" }, f("fall") * 100.0, f("mid"), f("volume") / 1e3),
        "wkd_close" => format!("🧪 {} итог выходного сигнала {coin} ({}): {} -> {} ({:+.2}% после комиссий, {:.1} ч); всего {:.0} сигналов, сумма {:+.1}%",
            if f("ret_pct") >= 0.0 { "✅" } else { "🔻" }, match ev["why"].as_str().unwrap_or("") { "take" => "тейк", "stop" => "стоп", _ => "время" },
            f("entry_px"), f("px"), f("ret_pct"), f("held_h"), f("closed"), f("sum_pct")),
        "wkd_open" => format!("🟢 HIP-3 выходные КУПИЛ {coin}: цена {}, на ${:.0}, стоп {}, тейк {}, выход через {:.0} ч",
            f("px"), f("notional"), f("stop_px"), f("take_px"), f("hold_h")),
        "wkd_exit" => format!("{} HIP-3 выходные {} {coin}: {} -> {} ({:+.2}%, {})", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            match ev["why"].as_str().unwrap_or("") { "stop" => "СТОП", "take" => "ТЕЙК", _ => "ЗАКРЫЛ" }, f("entry_px"), f("px"), f("ret_pct"),
            if f("pnl_usd") >= 0.0 { format!("+${:.2}", f("pnl_usd")) } else { format!("-${:.2}", -f("pnl_usd")) }),
        "wkd_error" => format!("⚠️ HIP-3 выходные {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_weekend_stock_dip_its_entry_and_its_take() {
        let p = Params { drop: 0.04, min_vol: 1e5, take: 0.05, stop: 0.10, hold_s: 86400.0, risk_usd: 20.0, max_open: 3, live: None };
        // 2026-10-31 is a Saturday; 2026-10-29 15:00 UTC a Thursday in the US session
        let sat = 29_890_080 + 600;
        let thu = 29_888_100;
        assert!(weekend(sat) && weekend(sat + 1440) && !weekend(thu) && !weekend(sat + 2 * 1440));
        // the nights: Wed 22:00 EDT (Thu 02:00 UTC), Tue 22:00 EST (Wed 03:00 UTC), Tue 20:30 EDT (Wed 00:30 UTC); not Wed
        // 19:30 EST (Thu 00:30 UTC), not the session, not Fri 21:00 EST (Sat 02:00 UTC: the weekend's)
        assert!(us_night(29_887_320) && us_night(29_472_660) && us_night(29_734_590) && !us_night(29_473_950) && !us_night(thu)
            && !us_night(29_476_860 + 60));
        assert!(us_dst(days_from_civil(2026, 3, 8)) && !us_dst(days_from_civil(2026, 3, 7)) && us_dst(days_from_civil(2026, 10, 31))
            && !us_dst(days_from_civil(2026, 11, 1)) && days_from_civil(2026, 10, 31) * 1440 == 29_890_080);
        assert!(us_stock("xyz:NVDA") && !us_stock("xyz:SKHX") && !us_stock("io:OAI") && !us_stock("xyz:GOLD"));
        assert!(tradable("xyz:NVDA") && tradable("xyz:SKHX") && tradable("io:OAI") && tradable("xyz:SP500") && !tradable("xyz:GOLD") && !tradable("para:TOTAL2")
            && !tradable("flx:NVDA") && !tradable("NVDA"));
        let vol: HashMap<String, f64> = [("xyz:NVDA".to_string(), 2e5), ("xyz:GOLD".to_string(), 1e7)].into();
        for m0 in [thu, sat] {
            let mut c = Closes::default();
            for m in m0 - 20..m0 {
                c.update("xyz:NVDA", m, 100.0, 40);
                c.update("xyz:GOLD", m, 100.0, 40);
            }
            let lows: HashMap<String, (i64, f64)> = [("xyz:NVDA".to_string(), (m0, 95.5)), ("xyz:GOLD".to_string(), (m0, 90.0))].into();
            let mut b = Book::default();
            let (ev, _) = b.step(&p, &c, &lows, &vol, m0, 0.0);
            if m0 == thu {
                assert!(ev.is_empty());
                continue;
            }
            assert_eq!(ev.len(), 1, "{ev:?}");
            assert_eq!(ev[0]["coin"], "xyz:NVDA");
            assert!(b.step(&p, &c, &lows, &vol, m0, 1.0).0.is_empty(), "the gap");
            // the entry at the close of the minute after, then the take
            c.update("xyz:NVDA", m0, 96.0, 40);
            c.update("xyz:NVDA", m0 + 1, 97.0, 40);
            c.update("xyz:NVDA", m0 + 2, 98.0, 40);
            let (ev, en) = b.step(&p, &c, &HashMap::new(), &vol, m0 + 2, 120.0);
            assert!(ev.is_empty() && en == vec![("xyz:NVDA".to_string(), 97.0)]);
            assert_eq!(b.open[0].entry, Some(97.0));
            c.update("xyz:NVDA", m0 + 30, 102.0, 40);
            let (ev, _) = b.step(&p, &c, &HashMap::new(), &vol, m0 + 30, 1800.0);
            assert_eq!(ev[0]["why"], "take");
            assert!((ev[0]["ret_pct"].as_f64().unwrap() - (5.0 - 0.17)).abs() < 1e-9);
            assert!(b.open.is_empty());
            assert!(message(&ev[0]).unwrap().contains("тейк"));
        }
        // a US night: the US stock signals, the Asian one does not
        let night = 29_887_320;
        let mut c = Closes::default();
        for m in night - 20..night {
            c.update("xyz:NVDA", m, 100.0, 40);
            c.update("xyz:SKHX", m, 100.0, 40);
        }
        let vol: HashMap<String, f64> = [("xyz:NVDA".to_string(), 2e5), ("xyz:SKHX".to_string(), 2e5)].into();
        let lows: HashMap<String, (i64, f64)> = [("xyz:NVDA".to_string(), (night, 95.5)), ("xyz:SKHX".to_string(), (night, 95.5))].into();
        let mut b = Book::default();
        let (ev, _) = b.step(&p, &c, &lows, &vol, night, 0.0);
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert_eq!(ev[0]["coin"], "xyz:NVDA");
        assert!(message(&ev[0]).unwrap().contains("ночь США"));
    }
}
