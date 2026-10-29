//! The HIP-3 weekend dips, a shadow only (no orders; V85 hypothesis 47, V87): on Saturday or Sunday (UTC) a stock of a
//! USDC-margined HIP-3 dex (xyz, para, mkts, io, abcd; not the commodities, FX, indices or the Asian names) with
//! `WKD_MIN_VOL_USD` ($100k)+ a day whose mid goes `WKD_DROP_PCT` (4) or more under the highest close of the 15
//! minutes before: a signal; a coin not again for 120 min. Bought on paper at the close of the minute after (the
//! research's entry), out at +`WKD_TAKE_PCT` (5; a maker 3 bp), -`WKD_STOP_PCT` (10) or `WKD_HOLD_H` (24) h on, by
//! the mids, less 9 bp taker + 5 bp slippage a side. Events kind `hip3`, `wkd_signal` / `wkd_close`, to the chat too.
//! Run inside the HIP-3 loop (its 10 s mids, its volumes); its state with the `hip3` doc.
//!
//! The research (the raw tape, 2025-12 .. 2026-10, 131 dips of 4%+, the tape's ask in / bid out): +104..+204 bp a trade,
//! 34 weekends, 56 coins, 82% of the weekends up, w/o the best 3 weekends +143%; 116 of the 131 not overlapping the
//! HIP-3 wick (+214 bp); the same dips in the session lose. The mids see the wicks shallower than the trades: fewer
//! signals here than on the tape.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::dip::Closes;

const DEXES: [&str; 5] = ["xyz", "para", "mkts", "io", "abcd"];
/// The HIP-3 names that are not US stocks (the research's: the commodities, FX, the indices, the Asian and pre-IPO names).
const NONEQ: [&str; 69] = ["GOLD", "SILVER", "CL", "OIL", "WTI", "BRENTOIL", "COPPER", "NATGAS", "GAS", "PLATINUM", "PALLADIUM", "URANIUM", "EUR",
    "JPY", "DXY", "GBP", "CORN", "WHEAT", "SOY", "COFFEE", "COCOA", "SUGAR", "USDE", "XMR", "ALUMINIUM", "ALUMINUM", "NICKEL", "ZINC", "LITHIUM", "HO",
    "SP500", "XYZ100", "JP225", "KR200", "USBOND", "10Y", "2Y", "BTCD", "OTHERS", "TOTAL2", "BVIV", "SMALL2000", "US500", "USTECH", "USA100", "USA500",
    "SKHX", "SKHY", "SMSN", "KIOXIA", "SOFTBANK", "HYUNDAI", "CXMT", "ZHIPU", "MINIMAX", "UNITREE", "GIGADEV", "DRAM", "SHEIN", "OAI", "ANTH",
    "SPCX", "PURRDAT", "TREAD", "ANSEM", "BIRD", "CBRS", "EWJ", "EWT"];
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
}

impl Params {
    pub(crate) fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { drop: v("WKD_DROP_PCT", 4.0) / 100.0, min_vol: v("WKD_MIN_VOL_USD", 1e5), take: v("WKD_TAKE_PCT", 5.0) / 100.0,
            stop: v("WKD_STOP_PCT", 10.0) / 100.0, hold_s: v("WKD_HOLD_H", 24.0) * 3600.0 }
    }

    pub(crate) fn label(&self) -> String {
        format!("выходные HIP-3 акции: падение {:.0}% под максимум 15 мин, оборот от ${:.0}k; тейк +{:.0}% / стоп −{:.0}% / {:.0} ч", self.drop * 100.0,
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
}

/// Saturday or Sunday (UTC) for an epoch minute.
fn weekend(minute: i64) -> bool {
    (minute.div_euclid(1440) + 3).rem_euclid(7) >= 5
}

/// A US stock of a USDC dex.
fn equity(coin: &str) -> bool {
    coin.split_once(':').is_some_and(|(d, n)| DEXES.contains(&d) && !NONEQ.contains(&n))
}

impl Book {
    /// One poll: `lows` the current minute's lowest mid a coin; the events to log. `minute` the current minute, `t` now (s).
    pub(crate) fn step(&mut self, p: &Params, closes: &Closes, lows: &HashMap<String, (i64, f64)>, volume: &HashMap<String, f64>, minute: i64, t: f64) -> Vec<Value> {
        let mut out = Vec::new();
        if weekend(minute) {
            for (c, &(m, lo)) in lows {
                if m != minute || !equity(c) || volume.get(c).copied().unwrap_or(0.0) < p.min_vol || self.last.get(c).is_some_and(|x| minute - x < GAP_MIN) {
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
                    out.push(json!({"what": "wkd_signal", "coin": c, "fall": fall, "mid": lo, "ref": r, "volume": volume.get(c).copied().unwrap_or(0.0)}));
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
        out
    }
}

pub(crate) fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    Some(match ev["what"].as_str()? {
        "wkd_signal" => format!("🧪 HIP-3 выходные (без сделки) {coin}: {:.1}% под максимум 15 мин, цена {}, оборот ${:.0}k", f("fall") * 100.0, f("mid"), f("volume") / 1e3),
        "wkd_close" => format!("🧪 {} итог выходного сигнала {coin} ({}): {} -> {} ({:+.2}% после комиссий, {:.1} ч); всего {:.0} сигналов, сумма {:+.1}%",
            if f("ret_pct") >= 0.0 { "✅" } else { "🔻" }, match ev["why"].as_str().unwrap_or("") { "take" => "тейк", "stop" => "стоп", _ => "время" },
            f("entry_px"), f("px"), f("ret_pct"), f("held_h"), f("closed"), f("sum_pct")),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_weekend_stock_dip_its_entry_and_its_take() {
        let p = Params { drop: 0.04, min_vol: 1e5, take: 0.05, stop: 0.10, hold_s: 86400.0 };
        // 2026-10-31 is a Saturday; 2026-10-29 a Thursday
        let sat = 29_890_080 + 600;
        let thu = sat - 2 * 1440;
        assert!(weekend(sat) && weekend(sat + 1440) && !weekend(thu) && !weekend(sat + 2 * 1440));
        assert!(equity("xyz:NVDA") && !equity("xyz:GOLD") && !equity("flx:NVDA") && !equity("NVDA"));
        let vol: HashMap<String, f64> = [("xyz:NVDA".to_string(), 2e5), ("xyz:GOLD".to_string(), 1e7)].into();
        for m0 in [thu, sat] {
            let mut c = Closes::default();
            for m in m0 - 20..m0 {
                c.update("xyz:NVDA", m, 100.0, 40);
                c.update("xyz:GOLD", m, 100.0, 40);
            }
            let lows: HashMap<String, (i64, f64)> = [("xyz:NVDA".to_string(), (m0, 95.5)), ("xyz:GOLD".to_string(), (m0, 90.0))].into();
            let mut b = Book::default();
            let ev = b.step(&p, &c, &lows, &vol, m0, 0.0);
            if m0 == thu {
                assert!(ev.is_empty());
                continue;
            }
            assert_eq!(ev.len(), 1, "{ev:?}");
            assert_eq!(ev[0]["coin"], "xyz:NVDA");
            assert!(b.step(&p, &c, &lows, &vol, m0, 1.0).is_empty(), "the gap");
            // the entry at the close of the minute after, then the take
            c.update("xyz:NVDA", m0, 96.0, 40);
            c.update("xyz:NVDA", m0 + 1, 97.0, 40);
            c.update("xyz:NVDA", m0 + 2, 98.0, 40);
            assert!(b.step(&p, &c, &HashMap::new(), &vol, m0 + 2, 120.0).is_empty());
            assert_eq!(b.open[0].entry, Some(97.0));
            c.update("xyz:NVDA", m0 + 30, 102.0, 40);
            let ev = b.step(&p, &c, &HashMap::new(), &vol, m0 + 30, 1800.0);
            assert_eq!(ev[0]["why"], "take");
            assert!((ev[0]["ret_pct"].as_f64().unwrap() - (5.0 - 0.17)).abs() < 1e-9);
            assert!(b.open.is_empty());
            assert!(message(&ev[0]).unwrap().contains("тейк"));
        }
    }
}
