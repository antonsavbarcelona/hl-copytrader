//! The RSI dip on paper (`RSIDIP_ENABLED`, on by default; "0" off): no orders, the trades simulated on
//! the mids and logged (events kind `rsidip`: `open` / `add` / `close`, to the chat too) to check live
//! what the archive found. On each 5-minute close: a main-dex perp with `RSIDIP_MIN_VOL_USD` (10M)+ a
//! day whose RSI14 (Wilder, the 5m closes) was under `RSIDIP_RSI_MAX` (35) the bar before and is up
//! now, the close `RSIDIP_UNDER_PCT` (8) or more under the EMA200 of the 5m closes, BTC's last 15
//! minutes under `RSIDIP_BTC_MAX_PCT` (0.5): bought at the mid; up to `RSIDIP_ADDS` (2) adds, each at a
//! 5m close `RSIDIP_ADD_PCT` (3) under the last buy; out at the average +`RSIDIP_TAKE_PCT` (2) (a
//! resting limit: maker), at the average -`RSIDIP_STOP_PCT` (20) or `RSIDIP_HOLD_H` (24) h on (taker);
//! a buy of `RSIDIP_RISK_USD` / the stop ($10 / 20% = $50) each; one a coin, at most `RSIDIP_MAX_OPEN`
//! (3). Costs: taker 4.5 bp + 5 bp slippage, maker 1.5 bp, a stop +5 bp; no funding.
//!
//! The research (2026-10-27, V52..V56): 202 + 1.58M configs from the user's pinescript logic, chosen
//! on 11 months, checked on 7, a placebo grid, Binance 2020..2026 and the raw tape: this one held on
//! the tape (the buys at the worst trade of the first block a second on), 2025-05 .. 2026-10: 1,127
//! trades, +73 bp a buy, +$1,196 at $100 a buy, w/o the 20 best days +$480, the 2 best days 12%,
//! 14 of 18 months up, t 3.8; at most 3 open: +$650, the drawdown with the open $212 at $100 a buy.
//! On Binance only on the coins HL lists (all of Binance: nothing), so the live record decides.
//!
//! Prices: the main dex's mids each `POLL_S`, the 5m closes their last mid in each bar; after a
//! start the liquid coins' 5m candles (50 h) read, a coin each `BACKFILL_EVERY_S`. Its state is the
//! store's doc `rsidip` (the positions, the tally).

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{Api, now};
use crate::config::Config;
use crate::dip::Closes;
use crate::log;
use crate::store::Store;

const DOC: &str = "rsidip";
const BAR_MIN: i64 = 5;
const POLL_S: f64 = 10.0;
const VOLUME_EVERY_S: f64 = 300.0;
const BACKFILL_EVERY_S: f64 = 10.0;
/// The 5m bars kept a coin (50 h), and the fewest the signal is read from (the EMA200 warmed).
const KEEP: usize = 600;
const MIN_BARS: usize = 400;
const TAKER: f64 = 4.5e-4 + 5e-4;
const MAKER: f64 = 1.5e-4;
const STOP_X: f64 = 5e-4;

#[derive(Clone, Debug)]
struct Params {
    rsi_max: f64,
    under: f64,
    btc_max: f64,
    min_vol: f64,
    take: f64,
    stop: f64,
    hold_s: f64,
    adds: u32,
    add_step: f64,
    max_open: usize,
    risk_usd: f64,
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { rsi_max: v("RSIDIP_RSI_MAX", 35.0), under: v("RSIDIP_UNDER_PCT", 8.0) / 100.0, btc_max: v("RSIDIP_BTC_MAX_PCT", 0.5) / 100.0,
            min_vol: v("RSIDIP_MIN_VOL_USD", 10e6), take: v("RSIDIP_TAKE_PCT", 2.0) / 100.0, stop: v("RSIDIP_STOP_PCT", 20.0) / 100.0,
            hold_s: v("RSIDIP_HOLD_H", 24.0) * 3600.0, adds: v("RSIDIP_ADDS", 2.0) as u32, add_step: v("RSIDIP_ADD_PCT", 3.0) / 100.0,
            max_open: v("RSIDIP_MAX_OPEN", 3.0) as usize, risk_usd: v("RSIDIP_RISK_USD", 10.0) }
    }

    /// The $ of a buy: the risk over the stop.
    fn buy_usd(&self) -> f64 {
        self.risk_usd / self.stop
    }
}

/// A paper position: its buys, their fees ($).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pos {
    coin: String,
    at: f64,
    units: u32,
    qty: f64,
    cost: f64,
    last: f64,
    fees: f64,
    rsi: f64,
    under: f64,
}

impl Pos {
    fn avg(&self) -> f64 {
        self.cost / self.qty
    }

    fn buy(&mut self, px: f64, usd: f64) {
        self.qty += usd / px;
        self.cost += usd;
        self.units += 1;
        self.last = px;
        self.fees += TAKER * usd;
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    open: Vec<Pos>,
    #[serde(default)]
    closed: u32,
    #[serde(default)]
    wins: u32,
    #[serde(default)]
    pnl_usd: f64,
}

pub(crate) fn ema(x: &[f64], n: usize) -> Vec<f64> {
    let a = 2.0 / (n as f64 + 1.0);
    let mut out = Vec::with_capacity(x.len());
    for (i, v) in x.iter().enumerate() {
        out.push(if i == 0 { *v } else { a * v + (1.0 - a) * out[i - 1] });
    }
    out
}

/// Wilder's RSI as the research's (the averages seeded with the first change, 0; the first `n` 50).
pub(crate) fn rsi(x: &[f64], n: usize) -> Vec<f64> {
    let a = 1.0 / n as f64;
    let (mut up, mut dn) = (0.0, 0.0);
    let mut out = Vec::with_capacity(x.len());
    for i in 0..x.len() {
        let d = if i == 0 { 0.0 } else { x[i] - x[i - 1] };
        if i > 0 {
            up = a * d.max(0.0) + (1.0 - a) * up;
            dn = a * (-d).max(0.0) + (1.0 - a) * dn;
        }
        out.push(if i < n { 50.0 } else if dn == 0.0 { 100.0 } else if up == 0.0 { 0.0 } else { 100.0 - 100.0 / (1.0 + up / dn) });
    }
    out
}

/// The closes of the bars up to `upto` (the gaps carried), oldest first.
fn series(q: &VecDeque<(i64, f64)>, upto: i64) -> Vec<f64> {
    let mut out = Vec::new();
    let mut prev: Option<(i64, f64)> = None;
    for &(b, px) in q.iter().filter(|x| x.0 <= upto) {
        if let Some((pb, pp)) = prev {
            for _ in pb + 1..b {
                out.push(pp);
            }
        }
        out.push(px);
        prev = Some((b, px));
    }
    if let Some((pb, pp)) = prev {
        for _ in pb + 1..=upto {
            out.push(pp);
        }
    }
    out
}

/// The signal on the last close of `c`: (the RSI the bar before, how far under the EMA200).
fn signal(c: &[f64], p: &Params) -> Option<(f64, f64)> {
    if c.len() < MIN_BARS {
        return None;
    }
    let (r, e) = (rsi(c, 14), ema(c, 200));
    let n = c.len() - 1;
    let under = c[n] / e[n] - 1.0;
    (r[n - 1] < p.rsi_max && r[n] > r[n - 1] && under < -p.under).then_some((r[n - 1], under))
}

fn usd(x: f64) -> String {
    if x >= 0.0 { format!("+${x:.2}") } else { format!("-${:.2}", -x) }
}

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    Some(match ev["what"].as_str()? {
        "open" => format!("📝 RSI-дип (БУМАГА, без сделки) ВХОД {coin}: RSI был {:.0} и развернулся, цена {:.1}% ниже EMA200; по {}, ${:.0}, тейк {} / стоп {}, до {:.0} ч",
            f("rsi"), -f("under") * 100.0, f("px"), f("usd"), f("take_px"), f("stop_px"), f("hold_h")),
        "add" => format!("📝 RSI-дип (бумага) ДОБОР {coin} №{:.0}: по {}, средняя {}, тейк {} / стоп {}", f("units"), f("px"), f("avg"), f("take_px"), f("stop_px")),
        "close" => format!("{} RSI-дип (бумага) {} {coin}: средняя {} -> {}, покупок {:.0}, {} ({:+.2}%) | всего {:.0} сделок, в плюсе {:.0}, итог {}",
            if f("pnl_usd") >= 0.0 { "✅" } else { "🔻" }, match ev["why"].as_str().unwrap_or("") { "take" => "ТЕЙК", "stop" => "СТОП", _ => "ВЫХОД ПО ВРЕМЕНИ" },
            f("avg"), f("px"), f("units"), usd(f("pnl_usd")), f("ret_pct"), f("closed"), f("wins"), usd(f("total_usd"))),
        _ => return None,
    })
}

/// Starts the RSI dip on paper unless `RSIDIP_ENABLED` is "0" (see the module).
pub async fn start(cfg: &Config, store: Store, api: Api) -> Result<()> {
    if crate::trades::switch("RSIDIP_ENABLED").is_none() {
        log!("rsidip: off (RSIDIP_ENABLED=0)");
        return Ok(());
    }
    let p = Params::from_env();
    let mut state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    log!("rsidip: paper; RSI14 5m hook from < {:.0}, {:.0}%+ under the EMA200, BTC 15 min < {:.1}%, ${:.0}M+; take {:.0}% stop {:.0}% {:.0} h, {} adds {:.0}% apart, ${:.0} a buy, at most {}; {} open, {} closed",
        p.rsi_max, p.under * 100.0, p.btc_max * 100.0, p.min_vol / 1e6, p.take * 100.0, p.stop * 100.0, p.hold_s / 3600.0, p.adds, p.add_step * 100.0, p.buy_usd(), p.max_open,
        state.open.len(), state.closed);
    crate::notify::send(format!("📝 RSI-дип запущен НА БУМАГЕ (без сделок): 5-минутки, RSI14 разворот из-под {:.0}, цена на {:.0}%+ ниже EMA200, BTC спокоен (за 15 мин меньше {:.1}%), оборот от ${:.0}M; тейк +{:.0}%, стоп −{:.0}%, до {:.0} ч, {} добора через {:.0}%, по ${:.0} на покупку (риск ${:.0}), до {} позиций. Открыто {}, закрыто {} (итог {})",
        p.rsi_max, p.under * 100.0, p.btc_max * 100.0, p.min_vol / 1e6, p.take * 100.0, p.stop * 100.0, p.hold_s / 3600.0, p.adds, p.add_step * 100.0, p.buy_usd(), p.risk_usd,
        p.max_open, state.open.len(), state.closed, usd(state.pnl_usd)));
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let mut closes = Closes::default();
        let mut volume: HashMap<String, f64> = HashMap::new();
        let mut backfilled: HashSet<String> = HashSet::new();
        let (mut vol_at, mut backfill_at) = (0.0, 0.0);
        let mut last_bar = (now() / 60.0).floor() as i64 / BAR_MIN;
        let event = |store: &Store, mut ev: Value| {
            ev["kind"] = json!("rsidip");
            ev["at"] = json!(now());
            log!("rsidip: {ev}");
            if let Some(m) = message(&ev) {
                crate::notify::send(m);
            }
            store.event(ev);
        };
        let save = |cfg: &Config, state: &State| {
            let (cfg, v) = (cfg.clone(), serde_json::to_value(state).unwrap_or_default());
            tokio::spawn(async move {
                if let Err(e) = crate::store::save_doc(&cfg, DOC, &v).await {
                    log!("rsidip: state not saved: {e:#}");
                }
            });
        };
        let mut tick = tokio::time::interval(std::time::Duration::from_secs_f64(POLL_S));
        loop {
            tick.tick().await;
            let t = now();
            let bar = (t / 60.0).floor() as i64 / BAR_MIN;
            if t - vol_at >= VOLUME_EVERY_S {
                match api.meta().await {
                    Ok((_, ctx)) => {
                        volume = ctx.into_iter().map(|(c, x)| (c, x.day_volume)).collect();
                        vol_at = t;
                    }
                    Err(e) => log!("rsidip: volumes not read: {e:#}"),
                }
            }
            let mids = match api.mids("").await {
                Ok(m) => m,
                Err(e) => {
                    log!("rsidip: mids not read: {e:#}");
                    continue;
                }
            };
            for (c, px) in &mids {
                closes.update(c, bar, *px, KEEP);
            }
            // The 5m candles after a start: the liquid coins (and BTC) without enough bars.
            if t - backfill_at >= BACKFILL_EVERY_S {
                let short = |c: &String| !backfilled.contains(c) && closes.0.get(c).is_none_or(|q| q.len() < MIN_BARS);
                let mut todo: Vec<(&String, &f64)> = volume.iter().filter(|(c, v)| (**v >= p.min_vol || c.as_str() == "BTC") && short(c)).collect();
                todo.sort_by(|a, b| b.1.total_cmp(a.1));
                if let Some(c) = todo.first().map(|x| x.0.clone()) {
                    backfilled.insert(c.clone());
                    match api.bar_closes(&c, BAR_MIN, t - KEEP as f64 * 300.0).await {
                        Ok(v) => closes.backfill(&c, &v, KEEP),
                        Err(e) => log!("rsidip: {c} candles not read: {e:#}"),
                    }
                }
                backfill_at = t;
            }
            let mut changed = false;
            // The exits, on the mids now: the take, the stop, the time.
            let mut keep = Vec::new();
            for pos in std::mem::take(&mut state.open) {
                let Some(&mid) = mids.get(&pos.coin) else { keep.push(pos); continue };
                let avg = pos.avg();
                let why = if mid >= avg * (1.0 + p.take) { Some("take") } else if mid <= avg * (1.0 - p.stop) { Some("stop") } else if t - pos.at >= p.hold_s { Some("time") } else { None };
                let Some(why) = why else { keep.push(pos); continue };
                let (px, fee) = match why { "take" => (avg * (1.0 + p.take), MAKER), "stop" => (mid, TAKER + STOP_X), _ => (mid, TAKER) };
                let pnl = px * pos.qty - pos.cost - fee * px * pos.qty - pos.fees;
                state.closed += 1;
                state.wins += (pnl > 0.0) as u32;
                state.pnl_usd += pnl;
                event(&store, json!({"what": "close", "coin": pos.coin, "why": why, "avg": avg, "px": px, "units": pos.units, "pnl_usd": pnl,
                    "ret_pct": pnl / pos.cost * 100.0, "held_h": (t - pos.at) / 3600.0, "closed": state.closed, "wins": state.wins, "total_usd": state.pnl_usd}));
                changed = true;
            }
            state.open = keep;
            if bar > last_bar {
                let b = bar - 1;          // the bar that just closed
                // The adds: a close the step under the last buy.
                for pos in state.open.iter_mut() {
                    let (Some(c), Some(&mid)) = (closes.0.get(&pos.coin).map(|q| series(q, b)), mids.get(&pos.coin)) else { continue };
                    if pos.units <= p.adds && c.last().is_some_and(|x| *x <= pos.last * (1.0 - p.add_step)) {
                        pos.buy(mid, p.buy_usd());
                        let avg = pos.avg();
                        event(&store, json!({"what": "add", "coin": pos.coin, "px": mid, "units": pos.units, "avg": avg,
                            "take_px": avg * (1.0 + p.take), "stop_px": avg * (1.0 - p.stop)}));
                        changed = true;
                    }
                }
                // The signals.
                let btc = closes.0.get("BTC").map(|q| series(q, b)).filter(|c| c.len() > 3).map(|c| c[c.len() - 1] / c[c.len() - 4] - 1.0);
                if btc.is_some_and(|x| x.abs() < p.btc_max) {
                    let mut cands: Vec<(String, f64, f64)> = Vec::new();
                    for (c, q) in &closes.0 {
                        if c == "BTC" || volume.get(c).copied().unwrap_or(0.0) < p.min_vol || state.open.iter().any(|x| &x.coin == c) {
                            continue;
                        }
                        if let Some((r, u)) = signal(&series(q, b), &p) {
                            cands.push((c.clone(), r, u));
                        }
                    }
                    cands.sort_by(|a, b| a.2.total_cmp(&b.2));      // the deepest first
                    for (c, r, u) in cands {
                        if state.open.len() >= p.max_open {
                            break;
                        }
                        let Some(&mid) = mids.get(&c) else { continue };
                        let mut pos = Pos { coin: c.clone(), at: t, rsi: r, under: u, ..Default::default() };
                        pos.buy(mid, p.buy_usd());
                        event(&store, json!({"what": "open", "coin": c, "px": mid, "usd": p.buy_usd(), "rsi": r, "under": u, "take_px": mid * (1.0 + p.take),
                            "stop_px": mid * (1.0 - p.stop), "hold_h": p.hold_s / 3600.0}));
                        state.open.push(pos);
                        changed = true;
                    }
                }
                if bar % 12 == 0 {
                    let ready = closes.0.iter().filter(|(c, q)| volume.get(*c).copied().unwrap_or(0.0) >= p.min_vol && q.len() >= MIN_BARS).count();
                    let liquid = volume.values().filter(|v| **v >= p.min_vol).count();
                    log!("rsidip: {ready} of {liquid} liquid coins with {MIN_BARS}+ bars, {} open, {} closed, {}", state.open.len(), state.closed, usd(state.pnl_usd));
                }
                last_bar = bar;
            }
            if changed {
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
    fn the_indicators_and_the_signal() {
        let e = ema(&[1.0, 2.0, 3.0], 1);
        assert_eq!(e, vec![1.0, 2.0, 3.0]);
        let r = rsi(&(0..40).map(|i| 100.0 + i as f64).collect::<Vec<_>>(), 14);
        assert_eq!(r[13], 50.0);
        assert_eq!(r[39], 100.0);
        // the gaps carried
        let q: VecDeque<(i64, f64)> = [(1, 10.0), (3, 12.0)].into();
        assert_eq!(series(&q, 5), vec![10.0, 10.0, 12.0, 12.0, 12.0]);
        // flat at 100 for 400 bars, then a fall to 85 over 30 bars, then a tick up: a signal
        let p = Params::from_env();
        let mut c: Vec<f64> = vec![100.0; 400];
        c.extend((1..=30).map(|i| 100.0 - i as f64 * 0.5));
        assert!(signal(&c, &p).is_none());                      // still falling: no hook
        c.push(85.2);
        let (r, u) = signal(&c, &p).expect("a hook under 35, 8%+ under the EMA200");
        assert!(r < 35.0 && u < -0.08, "{r} {u}");
        assert!(signal(&c[..300], &p).is_none());                // too few bars
    }

    #[test]
    fn a_paper_buy() {
        let mut pos = Pos::default();
        pos.buy(100.0, 50.0);
        pos.buy(97.0, 50.0);
        assert!((pos.avg() - 98.477).abs() < 1e-3, "{}", pos.avg());
        assert!((pos.fees - 2.0 * 50.0 * TAKER).abs() < 1e-12);
    }
}
