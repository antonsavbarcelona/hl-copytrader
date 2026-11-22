//! The 7-day breakout after an accumulation, both sides, on the main-dex perps' 1h candles (`BRK_ENABLED` /
//! `BRKS_ENABLED`: unset or "1" live, "0" off, set empty the trial; with `LIVE_KEY` / `LIVE_ACCOUNT` as for the dip).
//!
//! The rule (from the JewTrade channel's setups, research V418-V424, 2026-11-14): after each 1h candle's close,
//! - LONG: the close over the highest high of the 168 candles before (the first close over it, the close before at or
//!   under it), the last 3 days' return under +3%, the last 7 days' under +10%, the 7-day range (high - low, 168 candles
//!   to it) under 30% of the 30-day range (720 candles): a breakout of a quiet week; and at least `BRK_MIN_CNT` (1) other
//!   long signal on the scanned coins in the 72 hours before (the breakouts in a batch; a lone one is ~0 R).
//! - SHORT: the mirror, the close under the lowest low of the 168 before, the 3 days' return over -5%, the range under
//!   30%, the coin's 24h notional (from the candles) at least `BRKS_SIGNAL_VOL_USD` ($0.5M; the research's Binance floor
//!   $3.2M ~ that on HL), and at least `BRKS_MIN_CNT` (3) other short signals in the 72 hours before.
//!
//! A coin's crossings less than 24 candles apart: the later one skipped (as the research). In at market at once; a resting
//! stop 3% and a resting take 9% from the fill; out at market 48 h on. The size: the family's risk (`FAMILY_RISK_PCT`)
//! lost at the stop; at most `BRK_MAX_OPEN` / `BRKS_MAX_OPEN` (3) each, the family's cap and the coin limits.
//! Traded only on a coin with `BRK_MIN_VOL_USD` ($0.5M) a day or more.
//!
//! The research: Binance 2020-26 (179 HL-listed coins, 3/9/48 h): long n 236 +0.53 R t 5.4 (2020-22 ~0, few signals),
//! short n 519 +0.49 R t 6.8, every year 2022-26 up; HL prices 2025-06 .. 2026-10: long 133 trades win 61% +0.76 R,
//! short 270 win 44% +0.28 R. The portfolio with the live 5 (Binance 2021-26, same drawdown 52%): x218 -> x2727, the
//! drawdown and the worst day the same, no live trade displaced (the 40% cap never bound), the daily P&L corr ~0; the
//! short gains in the crash months (BTC < -15%: +0.87 R) where the trend / squeeze longs lose. Win ~50%: the profit
//! comes in batches (a few months make most of it).
//!
//! The candles: loaded once (900 a coin, paced), then the last few each hour; the requests spaced (`BRK_REQ_GAP_S`) so the
//! scan stays well inside the IP's weight budget. The states: the store's docs `breakout` and `breakout_short`.
//!
//! The short trades on the shorts' sub-account (`sub`, 2026-11-22, the user's call: every short there): a long another
//! strategy holds on the coin no longer bars it. Before an entry the sub-account gets its margin and twice its loss to
//! the stop (and the open shorts' there, `sub::held`), the surplus back after each step and each exit.

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::now;
use crate::config::Config;
use crate::keeper::{HeldIn, Keeper};
use crate::venue::Venue;
use crate::notify::usd;
use crate::log;
use crate::store::Store;
use crate::trades::{self, Trade};

const H: f64 = 3600.0;
/// The candles kept a coin (720 for the 30-day range + the 72 h of signals + spare).
const KEEP: usize = 900;
/// The first candle a signal may be on (the research: 745).
const MIN_I: usize = 745;
const W7: usize = 168;
const W30: usize = 720;
const W3: usize = 72;
/// A coin's crossings closer than this (candles): the later skipped.
const COOLDOWN: usize = 24;
/// The window of the other signals counted (hours).
const CNT_H: f64 = 72.0;
/// The step this long after a 1h candle's close.
const AFTER_CLOSE_S: f64 = 20.0;
const CHECK_EVERY_S: f64 = 120.0;

/// A closed 1h candle: (open time s, open, high, low, close, notional $).
pub(crate) type Bar = (f64, f64, f64, f64, f64, f64);

#[derive(Clone, Debug)]
struct Side {
    kind: &'static str,
    dir: f64,
    min_cnt: usize,
    max_open: usize,
    stop: f64,
    take: f64,
    hold_s: f64,
    share: f64,
    risk_usd: f64,
    trial: Option<u32>,
}

#[derive(Clone, Debug)]
struct Params {
    min_vol: f64,
    short_signal_vol: f64,
    gap_s: f64,
    sides: Vec<Side>,
}

impl Params {
    fn from_env() -> Self {
        let v = trades::env_f;
        let mut sides = Vec::new();
        if let Some(trial) = trades::switch("BRK_ENABLED") {
            sides.push(Side { kind: "breakout", dir: 1.0, min_cnt: v("BRK_MIN_CNT", 1.0) as usize, max_open: v("BRK_MAX_OPEN", 3.0) as usize,
                stop: 0.03, take: 0.09, hold_s: 48.0 * H, share: v("BRK_RISK_SHARE", 1.0), risk_usd: 20.0, trial });
        }
        if let Some(trial) = trades::switch("BRKS_ENABLED") {
            sides.push(Side { kind: "breakout_short", dir: -1.0, min_cnt: v("BRKS_MIN_CNT", 3.0) as usize, max_open: v("BRKS_MAX_OPEN", 3.0) as usize,
                stop: 0.03, take: 0.09, hold_s: 48.0 * H, share: v("BRKS_RISK_SHARE", 1.0), risk_usd: 20.0, trial });
        }
        Params { min_vol: v("BRK_MIN_VOL_USD", 0.5e6), short_signal_vol: v("BRKS_SIGNAL_VOL_USD", 0.5e6), gap_s: v("BRK_REQ_GAP_S", 2.5), sides }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pos {
    tr: Trade,
    risk: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    open: Vec<Pos>,
    #[serde(default)]
    opened: u32,
}

fn max_of(x: impl Iterator<Item = f64>) -> f64 {
    x.fold(f64::NEG_INFINITY, f64::max)
}

fn min_of(x: impl Iterator<Item = f64>) -> f64 {
    x.fold(f64::INFINITY, f64::min)
}

/// The candles with the missing hours filled (o = h = l = c = the close before, no notional), as the research's reindex.
pub(crate) fn contiguous(b: &[Bar]) -> Vec<Bar> {
    let mut out: Vec<Bar> = Vec::with_capacity(b.len());
    for x in b {
        if let Some(p) = out.last().copied() {
            if x.0 <= p.0 {
                continue;
            }
            let mut t = p.0 + H;
            while t < x.0 - 1.0 {
                out.push((t, p.4, p.4, p.4, p.4, 0.0));
                t += H;
            }
        }
        out.push(*x);
    }
    out
}

/// The signals of side `dir` on contiguous candles `b` from candle `from` on (the cooldown run from `from - COOLDOWN*4`):
/// the candles' indexes. `vol_min`: the short's 24h notional floor.
pub(crate) fn signals(b: &[Bar], dir: f64, from: usize, vol_min: f64) -> Vec<usize> {
    let n = b.len();
    let mut out = Vec::new();
    if n < MIN_I + 2 {
        return out;
    }
    let hi = |i: usize| b[i].2;
    let lo = |i: usize| b[i].3;
    let c = |i: usize| b[i].4;
    // the extreme of the 168 candles before i
    let prior = |i: usize| if dir > 0.0 { max_of((i - W7..i).map(hi)) } else { min_of((i - W7..i).map(lo)) };
    let start = from.saturating_sub(COOLDOWN * 4).max(MIN_I);
    let mut last: Option<usize> = None;
    for i in start..n {
        let p = prior(i);
        let crossed = if dir > 0.0 { c(i) > p && c(i - 1) <= prior(i - 1) } else { c(i) < p && c(i - 1) >= prior(i - 1) };
        if !crossed {
            continue;
        }
        if last.is_some_and(|l| i - l < COOLDOWN) {
            continue;
        }
        last = Some(i);
        let h7 = max_of((i + 1 - W7..=i).map(hi));
        let l7 = min_of((i + 1 - W7..=i).map(lo));
        let h30 = max_of((i + 1 - W30..=i).map(hi));
        let l30 = min_of((i + 1 - W30..=i).map(lo));
        if h30 - l30 <= 0.0 {
            continue;
        }
        let cp = (h7 - l7) / (h30 - l30);
        let r3 = c(i) / c(i - W3) - 1.0;
        let r7 = c(i) / c(i - W7) - 1.0;
        let ok = if dir > 0.0 {
            r3 < 0.03 && r7 < 0.10 && cp < 0.30
        } else {
            let v24: f64 = (i + 1 - 24..=i).map(|k| b[k].5).sum();
            r3 > -0.05 && cp < 0.30 && v24 >= vol_min
        };
        if ok && i >= from {
            out.push(i);
        }
    }
    out
}

/// The other signals in the `CNT_H` hours before the candle opened at `t` (their candles' open times), any coin.
fn count_before(sig_times: &[(String, f64)], coin: &str, t: f64) -> usize {
    sig_times.iter().filter(|(c, s)| (c != coin || *s != t) && *s < t && *s >= t - CNT_H * H).count()
}

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    let short = ev["kind"] == "breakout_short";
    let nm = if short { "ШОРТ ПРОБОЯ НЕДЕЛЬНОГО ЛОЯ" } else { "ЛОНГ ПРОБОЯ НЕДЕЛЬНОГО ХАЯ" };
    Some(match ev["what"].as_str()? {
        "open" => format!("📈 {nm} {coin}: цена {}, на ${:.0}, других пробоев за 72ч: {:.0}; стоп {}, тейк {}, выход через {:.0} ч",
            f("px"), f("notional"), f("cnt"), f("stop_px"), f("take_px"), f("hold_h")),
        "close" => format!("{} {} {coin}: {} -> {} ({:+.2}%, {}, {:.1} ч)", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            match ev["why"].as_str().unwrap_or("") { "stop" => "СТОП", "take" => "ТЕЙК", "manual" => "ЗАКРЫТО ВНЕ БОТА (руками или ликвидация)", _ => "ЗАКРЫЛ по времени" },
            f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_usd")), f("held_h")),
        "error" => format!("⚠️ пробой {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        _ => return None,
    })
}

struct Breakout {
    /// The candles' and the contexts' reads (each side's keeper has its own handle on the same account).
    venue: Venue,
    /// Each side's keeping (its kind's events, state, trades).
    keepers: HashMap<&'static str, Keeper>,
    p: Params,
    states: HashMap<&'static str, State>,
    bars: HashMap<String, Vec<Bar>>,
    /// The last candle stepped (its open time s).
    stepped: f64,
    /// The sides on the shorts' sub-account (`sub`: the short, its keeper's venue the sub-account's) and their money's
    /// mover (None: `MAIN_KEY` unset, nothing moves).
    subs: HashMap<&'static str, Option<crate::sub::Mover>>,
}

impl Breakout {
    fn event(&self, kind: &str, ev: Value) {
        self.keepers[kind].event(ev);
    }

    fn save(&self, kind: &'static str) {
        self.keepers[kind].save(kind, &self.states.get(kind).cloned().unwrap_or_default());
    }

    /// On the sub-account: what is free there over every short's loss to its stop (`sub::held`) and one trade's risk
    /// back to the live account.
    async fn sweep_sub(&self, kind: &'static str) {
        let (Some(m), Some(side)) = (self.subs.get(kind), self.p.sides.iter().find(|s| s.kind == kind)) else { return };
        let keep = crate::sub::held() + trades::risk_usd(side.risk_usd) * side.share;
        if let Err(e) = crate::sub::sweep(&self.keepers[kind].venue, m.as_ref(), keep).await {
            self.event(kind, json!({"what": "error", "coin": "", "error": format!("{e:#}")}));
        }
    }

    /// A coin's closed 1h candles from `start` (s).
    async fn fetch(&self, coin: &str, start: f64) -> Option<Vec<Bar>> {
        let t = now();
        match crate::market::candles(&self.venue.ex, coin, "1h", start).await {
            Ok(v) => Some(crate::market::closed(v, "1h", t).into_iter().map(|c| (c.t, c.o, c.h, c.l, c.c, c.notional())).collect()),
            Err(e) => {
                log!("breakout: {coin} candles not read: {e:#}");
                None
            }
        }
    }

    /// The main-dex coins (not delisted, tradable here) and their day volume.
    async fn universe(&self) -> Option<Vec<(String, f64)>> {
        let ctx = crate::market::contexts(&self.venue.ex, "").await.ok()?;
        Some(ctx.into_iter().filter(|c| !c.delisted && self.venue.coins.contains_key(&c.name)).map(|c| (c.name, c.volume)).collect())
    }

    /// The candles brought up to the last closed one: a coin new to the cache loads `KEEP`, the others the last few.
    async fn refresh(&mut self, coins: &[(String, f64)]) {
        for (coin, _) in coins {
            let have = self.bars.get(coin).and_then(|b| b.last().map(|x| x.0));
            let start = match have {
                Some(t) => t - 2.0 * H,
                None => now() - (KEEP as f64 + 2.0) * H,
            };
            if let Some(new) = self.fetch(coin, start).await {
                let b = self.bars.entry(coin.clone()).or_default();
                for x in new {
                    if let Some(k) = b.iter().rposition(|y| (y.0 - x.0).abs() < 1.0) {
                        b[k] = x;
                    } else if b.last().is_none_or(|y| y.0 < x.0) {
                        b.push(x);
                    }
                }
                if b.len() > KEEP + 50 {
                    b.drain(..b.len() - KEEP);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs_f64(self.p.gap_s)).await;
        }
    }

    /// After a 1h candle's close (its open `bar`): the candles brought up, the signals of the last 72 h counted, the new
    /// ones traded.
    async fn step(&mut self, bar: f64) {
        let Some(coins) = self.universe().await else {
            log!("breakout: contexts not read: no step");
            return;
        };
        self.refresh(&coins).await;
        let vol: HashMap<&str, f64> = coins.iter().map(|(c, v)| (c.as_str(), *v)).collect();
        let sides = self.p.sides.clone();
        for side in &sides {
            let mut times: Vec<(String, f64)> = Vec::new();
            let mut fresh: Vec<(String, f64)> = Vec::new();
            let mut ready = 0usize;
            for (coin, raw) in &self.bars {
                let b = contiguous(raw);
                if b.len() < MIN_I + 2 {
                    continue;
                }
                ready += 1;
                let from = b.len().saturating_sub(CNT_H as usize + 3);
                for i in signals(&b, side.dir, from, self.p.short_signal_vol) {
                    times.push((coin.clone(), b[i].0));
                    if (b[i].0 - bar).abs() < 1.0 && i == b.len() - 1 {
                        fresh.push((coin.clone(), b[i].4));
                    }
                }
            }
            log!("{}: candle {}: {} coins ready of {}, signals in 72 h {}, new {}", side.kind, bar, ready, coins.len(), times.len(), fresh.len());
            for (coin, close) in fresh {
                let cnt = count_before(&times, &coin, bar);
                let v = vol.get(coin.as_str()).copied().unwrap_or(0.0);
                if cnt < side.min_cnt {
                    self.keepers[side.kind].store.event(json!({"kind": side.kind, "what": "lone", "coin": coin, "px": close, "cnt": cnt, "at": now()}));
                    continue;
                }
                if v < self.p.min_vol {
                    self.keepers[side.kind].store.event(json!({"kind": side.kind, "what": "thin", "coin": coin, "px": close, "cnt": cnt, "vol": v, "at": now()}));
                    continue;
                }
                let st = self.states.entry(side.kind).or_default();
                let risk = trades::risk_usd(side.risk_usd) * side.share;
                if st.open.len() >= side.max_open || st.open.iter().any(|x| x.tr.coin == coin) || !trades::can_open(&coin, side.kind, side.dir, risk) {
                    self.keepers[side.kind].store.event(json!({"kind": side.kind, "what": "full", "coin": coin, "px": close, "cnt": cnt, "at": now()}));
                    continue;
                }
                if !trades::trial_open(side.trial, st.opened) {
                    self.keepers[side.kind].store.event(json!({"kind": side.kind, "what": "signal", "coin": coin, "px": close, "cnt": cnt, "at": now()}));
                    continue;
                }
                let size = trades::size_for(risk, side.stop, close) * side.dir;
                let venue = &self.keepers[side.kind].venue;
                if let Some(m) = self.subs.get(side.kind) {
                    // its margin and its loss to the stop (twice), the open shorts' there, free on the sub-account first
                    let notional = size.abs() * close;
                    let need = notional / venue.leverage_of(&coin) + notional * 2.0 * side.stop + crate::sub::held();
                    if let Err(e) = crate::sub::fund(venue, m.as_ref(), need).await {
                        self.event(side.kind, json!({"what": "error", "coin": coin, "error": format!("{e:#}")}));
                        continue;
                    }
                }
                match trades::open_market_at(venue, &coin, size, side.stop, Some(side.take), side.hold_s, close).await {
                    Ok(tr) => {
                        self.event(side.kind, json!({"what": "open", "coin": coin, "px": tr.entry_px, "close": close, "size": tr.size, "notional": (tr.size * tr.entry_px).abs(),
                            "cnt": cnt, "stop_px": tr.stop_px, "take_px": tr.take_px, "hold_h": side.hold_s / H, "vol": v}));
                        let r = tr.risk();
                        trades::register(&coin, side.kind, r, tr.size);
                        let st = self.states.entry(side.kind).or_default();
                        st.open.push(Pos { tr, risk: r });
                        st.opened += 1;
                        self.save(side.kind);
                    }
                    Err(e) => self.event(side.kind, json!({"what": "error", "coin": coin, "error": format!("{e:#}")})),
                }
            }
            self.sweep_sub(side.kind).await;
        }
        self.stepped = bar;
    }

    /// Between the steps: each side's trades swept (`Keeper::sweep`: the stop or take filled, the time exit).
    async fn check(&mut self) {
        let kinds: Vec<&'static str> = self.states.keys().copied().collect();
        for kind in kinds {
            let open = &mut self.states.get_mut(kind).unwrap().open;
            if self.keepers[kind].sweep(open, |p| &p.tr, true, true).await {
                self.save(kind);
                self.sweep_sub(kind).await;
            }
        }
    }
}

/// Starts the breakout long (`BRK_ENABLED`) and short (`BRKS_ENABLED`): both on unless "0".
pub async fn start(cfg: &Config, store: Store) -> Result<()> {
    let p = Params::from_env();
    if p.sides.is_empty() {
        log!("breakout: off (BRK_ENABLED=0, BRKS_ENABLED=0)");
        return Ok(());
    }
    let Some((venue, unified)) = crate::runner::venue("breakout").await? else {
        return Ok(());
    };
    let mut states = HashMap::new();
    for s in &p.sides {
        let st: State = crate::store::load_state(cfg, s.kind).await?;
        for pos in &st.open {
            trades::register(&pos.tr.coin, s.kind, pos.risk, pos.tr.size);
        }
        states.insert(s.kind, st);
    }
    let risk = if trades::family_risk_pct() > 0.0 { format!("{:.1}% депозита", trades::family_risk_pct()) } else { "$20".to_string() };
    for s in &p.sides {
        let short = s.dir < 0.0;
        log!("{}: account {}: 1h close {} the 7-day {} after a quiet week, >= {} other signals in 72 h; stop {:.0}% take {:.0}% {:.0} h; risk {risk}, at most {}; {} held",
            s.kind, venue.account, if short { "under" } else { "over" }, if short { "low" } else { "high" }, s.min_cnt, s.stop * 100.0, s.take * 100.0, s.hold_s / H,
            s.max_open, states[s.kind].open.len());
        crate::notify::send(format!("📈 {} НА ДЕНЬГИ: часовое закрытие {} 7-дневного {} после спокойной недели (диапазон недели < 30% месячного), не меньше {} других таких пробоев за 72 ч; стоп {:.0}%, тейк {:.0}%, выход через {:.0} ч; риск {risk} на сделку, до {} позиций, оборот монеты от ${:.1}M{}",
            if short { "ШОРТ ПРОБОЯ ВНИЗ" } else { "ЛОНГ ПРОБОЯ ВВЕРХ" }, if short { "ниже" } else { "выше" }, if short { "лоя" } else { "хая" }, s.min_cnt,
            s.stop * 100.0, s.take * 100.0, s.hold_s / H, s.max_open, p.min_vol / 1e6, trades::trial_label(s.trial, states[s.kind].opened)));
    }
    // the short on the shorts' sub-account (`sub`): its own positions, the money moved by the account's own key; the
    // candles and the contexts read on the live venue
    let mut keepers = HashMap::new();
    let mut subs = HashMap::new();
    for s in &p.sides {
        if crate::sub::on_sub(s.kind) {
            let mover = crate::sub::Mover::from_env(venue.ex.base(), &venue.account)?;
            if mover.is_none() {
                log!("{}: MAIN_KEY not set: nothing moves to the short account, a short goes on what is there", s.kind);
            }
            keepers.insert(s.kind, Keeper::new(crate::sub::venue(&venue)?, false, cfg.clone(), store.clone(), s.kind, message, HeldIn::Hours));
            subs.insert(s.kind, mover);
        } else {
            keepers.insert(s.kind, Keeper::new(venue.clone(), unified, cfg.clone(), store.clone(), s.kind, message, HeldIn::Hours));
        }
    }
    let l = Breakout { venue, keepers, p, states, bars: HashMap::new(), stepped: 0.0, subs };
    for kind in l.subs.keys() {
        l.sweep_sub(kind).await;
    }
    crate::runner::spawn("breakout", l);
    Ok(())
}

impl crate::runner::Strategy for Breakout {
    fn schedule(&self) -> crate::runner::Schedule {
        crate::runner::Schedule { tick_s: 10.0, step: crate::runner::Due::Bar { len: H, after: AFTER_CLOSE_S }, check_s: CHECK_EVERY_S }
    }

    fn stepped(&self) -> f64 {
        self.stepped
    }

    async fn step(&mut self, at: f64) {
        Breakout::step(self, at).await
    }

    async fn check(&mut self) {
        Breakout::check(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` candles an hour apart around `px` swinging by `amp` (a fraction) every candle, from t = 0.
    fn flat(n: usize, px: f64, amp: f64) -> Vec<Bar> {
        (0..n).map(|i| {
            let c = px * (1.0 + if i % 2 == 0 { amp } else { -amp } * 0.5);
            (i as f64 * H, c, c * (1.0 + amp), c * (1.0 - amp), c, 1e6)
        }).collect()
    }

    #[test]
    fn a_long_after_a_quiet_week_only() {
        // a wide month (swings of 20%) then a quiet week (1%) then a close over the week's high
        let mut b = flat(600, 100.0, 0.10);
        b.extend(flat(300, 100.0, 0.005).into_iter().map(|x| (x.0 + 600.0 * H, x.1, x.2, x.3, x.4, x.5)));
        assert!(signals(&b, 1.0, 0, 0.0).is_empty(), "no break yet");
        let t = b.len() as f64 * H;
        b.push((t, 100.0, 102.0, 100.0, 101.5, 1e6));
        let s = signals(&b, 1.0, 0, 0.0);
        assert_eq!(s, vec![b.len() - 1]);
        assert!(signals(&b, -1.0, 0, 0.0).is_empty());
        // the next candle still over: not a new crossing
        b.push((t + H, 101.5, 102.5, 101.0, 102.0, 1e6));
        assert_eq!(signals(&b, 1.0, 0, 0.0), vec![b.len() - 2]);
    }

    #[test]
    fn a_loud_week_or_a_run_up_gives_none() {
        // the whole span wide: the week's range ~ the month's -> no signal
        let mut b = flat(900, 100.0, 0.05);
        let t = b.len() as f64 * H;
        b.push((t, 100.0, 120.0, 100.0, 119.0, 1e6));
        assert!(signals(&b, 1.0, 0, 0.0).is_empty());
    }

    #[test]
    fn a_short_needs_its_volume() {
        let mut b = flat(600, 100.0, 0.10);
        b.extend(flat(300, 100.0, 0.005).into_iter().map(|x| (x.0 + 600.0 * H, x.1, x.2, x.3, x.4, x.5)));
        let t = b.len() as f64 * H;
        b.push((t, 100.0, 100.0, 97.5, 98.0, 1e6));
        assert_eq!(signals(&b, -1.0, 0, 0.5e6), vec![b.len() - 1]);
        assert!(signals(&b, -1.0, 0, 1e9).is_empty(), "under the volume floor");
    }

    #[test]
    fn the_gaps_filled_and_the_count_of_the_others() {
        let b = vec![(0.0, 1.0, 1.0, 1.0, 1.0, 1.0), (3.0 * H, 2.0, 2.0, 2.0, 2.0, 1.0)];
        let c = contiguous(&b);
        assert_eq!(c.len(), 4);
        assert_eq!(c[1], (H, 1.0, 1.0, 1.0, 1.0, 0.0));
        let t = 100.0 * H;
        let s = vec![("A".to_string(), t), ("B".to_string(), t), ("C".to_string(), t - H), ("D".to_string(), t - 72.0 * H), ("E".to_string(), t - 73.0 * H)];
        // A's own and B's at the same hour not counted; C and D in the 72 h, E out
        assert_eq!(count_before(&s, "A", t), 2);
    }
}

#[cfg(test)]
mod parity {
    use super::*;

    /// The research's parity check (run by hand): `BRK_PARITY_DIR` holds `<coin>.json` files of [[t, o, h, l, c, notional], ...]
    /// 1h candles; prints each coin's long / short signals (the candle's open time) for comparing with the Python rule.
    #[test]
    #[ignore]
    fn print_signals_of_files() {
        let Ok(dir) = std::env::var("BRK_PARITY_DIR") else { return };
        let mut names: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        names.sort();
        let mut out = String::new();
        for p in names {
            let v: Vec<Vec<f64>> = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            let b: Vec<Bar> = contiguous(&v.iter().map(|x| (x[0], x[1], x[2], x[3], x[4], x[5])).collect::<Vec<_>>());
            let coin = p.file_stem().unwrap().to_string_lossy().to_string();
            for (d, nm) in [(1.0, "L"), (-1.0, "S")] {
                for i in signals(&b, d, 0, 10f64.powf(6.5) - 1.0) {
                    out += &format!("{coin},{nm},{}\n", b[i].0 as i64);
                }
            }
        }
        std::fs::write(format!("{dir}/rust_signals.csv"), out).unwrap();
    }
}

#[cfg(test)]
mod lifecycle {
    use super::*;
    use crate::testkit::{FakeHl, cfg, saved};

    async fn setup(coin: &str) -> (FakeHl, Breakout, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        let f = FakeHl::start(&[(coin, 2, 10, 100.0)]).await;
        let (store, rx) = Store::for_test();
        let p = Params::from_env();
        let states = p.sides.iter().map(|s| (s.kind, State::default())).collect();
        let c = cfg();
        let keepers = p.sides.iter().map(|s| (s.kind, Keeper::new(f.venue(), false, c.clone(), store.clone(), s.kind, message, HeldIn::Hours))).collect();
        let b = Breakout { venue: f.venue(), keepers, p, states, bars: HashMap::new(), stepped: 0.0, subs: HashMap::new() };
        (f, b, rx)
    }

    async fn hold(b: &mut Breakout, kind: &'static str, coin: &str, size: f64) -> Trade {
        let tr = trades::open_market(&b.venue, coin, size, 0.03, Some(0.09), 48.0 * H).await.unwrap();
        trades::register(coin, kind, tr.risk(), tr.size);
        b.states.get_mut(kind).unwrap().open.push(Pos { tr: tr.clone(), risk: tr.risk() });
        tr
    }

    #[tokio::test]
    async fn a_stop_filled_ends_the_trade_told_and_saved() {
        let (f, mut b, mut rx) = setup("BKA").await;
        hold(&mut b, "breakout", "BKA", 1.0).await;
        f.set_mid("BKA", 96.0);
        b.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!(ev.len(), 1, "{ev:?}");
        let e = &ev[0];
        assert_eq!((e["kind"].as_str(), e["what"].as_str(), e["why"].as_str(), e["coin"].as_str()), (Some("breakout"), Some("close"), Some("stop"), Some("BKA")));
        assert!(e["px"].as_f64().unwrap() < 97.5 && e["ret_pct"].as_f64().unwrap() < -2.5 && e["pnl_usd"].as_f64().unwrap() < 0.0);
        assert!(e["entry_px"].as_f64().unwrap() == 100.0 && e["held_h"].as_f64().is_some() && e["at"].as_f64().is_some());
        assert!(b.states["breakout"].open.is_empty() && !trades::registered("BKA"));
        assert_eq!(saved(&b.keepers["breakout"].cfg, "breakout").await.unwrap()["open"], json!([]));
    }

    #[tokio::test]
    async fn a_take_filled_ends_the_short() {
        let (f, mut b, mut rx) = setup("BKB").await;
        hold(&mut b, "breakout_short", "BKB", -1.0).await;
        f.set_mid("BKB", 90.0);
        b.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((ev.len(), ev[0]["why"].as_str(), ev[0]["kind"].as_str()), (1, Some("take"), Some("breakout_short")), "{ev:?}");
        assert!((ev[0]["px"].as_f64().unwrap() - 91.0).abs() < 1e-9 && ev[0]["ret_pct"].as_f64().unwrap() > 8.9);
        assert!(b.states["breakout_short"].open.is_empty() && f.position("BKB") == 0.0);
    }

    #[tokio::test]
    async fn a_trade_still_held_stays() {
        let (f, mut b, mut rx) = setup("BKC").await;
        hold(&mut b, "breakout", "BKC", 1.0).await;
        f.set_mid("BKC", 101.0);
        b.check().await;
        assert!(crate::store::drain(&mut rx).is_empty());
        assert_eq!(b.states["breakout"].open.len(), 1);
        assert!(trades::registered("BKC"));
    }

    #[tokio::test]
    async fn its_time_up_it_is_closed_at_market() {
        let (f, mut b, mut rx) = setup("BKD").await;
        hold(&mut b, "breakout", "BKD", 1.0).await;
        b.states.get_mut("breakout").unwrap().open[0].tr.exit_at = now() - 1.0;
        f.set_mid("BKD", 102.0);
        b.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((ev.len(), ev[0]["why"].as_str()), (1, Some("time")), "{ev:?}");
        assert!((ev[0]["px"].as_f64().unwrap() - 102.0).abs() < 1e-9);
        assert!(f.position("BKD") == 0.0 && f.open_orders("BKD").is_empty(), "the stop and the take cancelled");
        assert!(b.states["breakout"].open.is_empty() && !trades::registered("BKD"));
    }

    #[tokio::test]
    async fn a_step_trades_a_batch_breakout_and_skips_a_lone_one() {
        // 3 coins: a quiet week then a break on the last hour; the long needs >= 1 other long in the 72 h before
        let (f, mut b, mut rx) = setup("BKE").await;
        let f2 = f; // one fake: add the others
        f2.with(|s| {
            s.assets.insert(1, ("BKF".into(), 2, 10));
            s.mids.insert("BKF".into(), 100.0);
        });
        b.venue = f2.venue();
        for k in b.keepers.values_mut() {
            k.venue = f2.venue();
        }
        let now_h = (now() / H).floor() * H;
        let bar = now_h - H;                                       // the candle just closed
        let mk = |break_at: f64| -> Vec<Bar> {
            let n = 900usize;
            (0..n).map(|i| {
                let t = bar - (n - 1 - i) as f64 * H;
                let wide = i < 600;
                let c = 100.0 * (1.0 + if i % 2 == 0 { 1.0 } else { -1.0 } * if wide { 0.05 } else { 0.0025 });
                let mut x = (t, c, c * if wide { 1.10 } else { 1.005 }, c * if wide { 0.90 } else { 0.995 }, c, 1e4);
                if (t - break_at).abs() < 1.0 { x = (t, 100.0, 102.0, 100.0, 101.5, 1e4) }
                if t > break_at { x = (t, 101.5, 102.0, 101.0, 101.6, 1e4) }
                x
            }).collect()
        };
        f2.set_candles("BKE", "1h", &mk(bar).iter().map(|x| (x.0, x.1, x.2, x.3, x.4, x.5)).collect::<Vec<_>>());
        f2.set_candles("BKF", "1h", &mk(bar - 10.0 * H).iter().map(|x| (x.0, x.1, x.2, x.3, x.4, x.5)).collect::<Vec<_>>());
        f2.set_volume("BKE", 5e6);
        f2.set_volume("BKF", 5e6);
        b.p.gap_s = 0.0;
        f2.set_mid("BKE", 101.5);
        f2.set_mid("BKF", 101.6);
        b.step(bar).await;
        let ev = crate::store::drain(&mut rx);
        let opened: Vec<&Value> = ev.iter().filter(|e| e["what"] == "open").collect();
        assert_eq!(opened.len(), 1, "{ev:?}");
        assert_eq!((opened[0]["coin"].as_str(), opened[0]["cnt"].as_u64()), (Some("BKE"), Some(1)));
        assert!(f2.position("BKE") > 0.0 && f2.open_orders("BKE").len() == 2, "in with its stop and take");
        assert_eq!(b.states["breakout"].open.len(), 1);
        assert_eq!(b.stepped, bar);
    }

    /// The short's breakdown on the shorts' sub-account (2026-11-22): the coin long on the live account (the RSI dip)
    /// does not bar it; its margin and twice its loss to the stop sent there first, the orders for the sub-account.
    #[tokio::test]
    async fn the_short_opens_on_the_sub_account_past_a_long_on_the_coin() {
        let (f, mut b, mut rx) = setup("BKG").await;
        let others = ["BKH", "BKI", "BKJ"];
        f.with(|s| {
            for (k, c) in others.iter().enumerate() {
                s.assets.insert(k as u32 + 1, (c.to_string(), 2, 10));
                s.mids.insert(c.to_string(), 98.4);
            }
            s.account_value = Some(0.0);
        });
        let sub = crate::sub::venue(&f.venue()).unwrap();
        b.venue = f.venue();
        b.keepers = b.p.sides.iter().map(|s| (s.kind, Keeper::new(if s.dir < 0.0 { sub.clone() } else { f.venue() }, false, cfg(), b.keepers[s.kind].store.clone(), s.kind, message, HeldIn::Hours))).collect();
        b.subs.insert("breakout_short", Some(crate::sub::Mover::for_test(&f.base, crate::testkit::ACCOUNT)));
        b.p.gap_s = 0.0;
        let bar = (now() / H).floor() * H - H;
        // a quiet week after a loud month, then a close under the week's low on `break_at`
        let mk = |break_at: f64| -> Vec<(f64, f64, f64, f64, f64, f64)> {
            (0..900usize).map(|i| {
                let t = bar - (899 - i) as f64 * H;
                let wide = i < 600;
                let c = 100.0 * (1.0 + if i % 2 == 0 { 1.0 } else { -1.0 } * if wide { 0.05 } else { 0.0025 });
                let mut x = (t, c, c * if wide { 1.10 } else { 1.005 }, c * if wide { 0.90 } else { 0.995 }, c, 1e5);
                if (t - break_at).abs() < 1.0 { x = (t, 100.0, 100.0, 98.0, 98.5, 1e5) }
                if t > break_at { x = (t, 98.5, 99.0, 98.0, 98.4, 1e5) }
                x
            }).collect()
        };
        f.set_candles("BKG", "1h", &mk(bar));
        f.set_mid("BKG", 98.5);
        f.set_volume("BKG", 5e6);
        for (k, c) in others.iter().enumerate() {
            f.set_candles(c, "1h", &mk(bar - (5 + 10 * k) as f64 * H));
            f.set_volume(c, 5e6);
        }
        trades::register("BKG", "rsidip", 5.0, 1.0);
        assert!(!trades::can_open("BKG", "breakout", -1.0, 1.0), "the live account: a short against the long, never");
        b.step(bar).await;
        let ev = crate::store::drain(&mut rx);
        let opened: Vec<&Value> = ev.iter().filter(|e| e["what"] == "open").collect();
        assert_eq!(opened.len(), 1, "{ev:?}");
        assert_eq!((opened[0]["coin"].as_str(), opened[0]["kind"].as_str(), opened[0]["cnt"].as_u64()), (Some("BKG"), Some("breakout_short"), Some(3)));
        assert!(f.position("BKG") < 0.0 && b.states["breakout_short"].open.len() == 1);
        let bodies = f.with(|x| x.bodies.clone());
        let send = bodies.iter().position(|x| x["action"]["type"] == "sendAsset" && x["action"]["fromSubAccount"] == "").expect("funded");
        let order = bodies.iter().position(|x| x["action"]["type"] == "order").unwrap();
        assert!(send < order, "the money first");
        assert!(bodies.iter().filter(|x| x["action"]["type"] == "order").all(|x| x["vaultAddress"] == crate::sub::SHORT_ACCOUNT));
        // sent: the margin at 10x and twice the 3% loss, 5% over
        let tr = &b.states["breakout_short"].open[0].tr;
        let n = (tr.size * tr.entry_px).abs();
        let sent: f64 = bodies[send]["action"]["amount"].as_str().unwrap().parse().unwrap();
        assert!(sent >= n / 10.0 + 0.06 * n - 1.0 && sent <= 1.06 * (n / 10.0 + 0.06 * n) + 6.0, "{sent} for {n}");
        // the sub-account keeps twice this short's risk
        assert!((crate::sub::held() - 2.0 * tr.risk()).abs() < 1e-6);
        trades::unregister("BKG", "rsidip");
        trades::unregister("BKG", "breakout_short");
    }
}

#[cfg(test)]
mod candles {
    use super::*;
    use crate::testkit::{FakeHl, cfg};

    #[tokio::test]
    async fn the_closed_hours_with_their_notional_and_the_cache_brought_up() {
        let f = FakeHl::start(&[("BCA", 2, 10, 100.0)]).await;
        let (store, _) = Store::for_test();
        let p = Params { gap_s: 0.0, ..Params::from_env() };
        let keepers = p.sides.iter().map(|s| (s.kind, Keeper::new(f.venue(), false, cfg(), store.clone(), s.kind, message, HeldIn::Hours))).collect();
        let mut b = Breakout { venue: f.venue(), keepers, p, states: HashMap::new(), bars: HashMap::new(), stepped: 0.0, subs: HashMap::new() };
        let h = (now() / H).floor() * H;
        f.set_candles("BCA", "1h", &[(h - 2.0 * H, 1.0, 2.0, 0.5, 1.5, 4.0), (h - H, 1.5, 2.5, 1.0, 2.0, 3.0), (h, 2.0, 3.0, 1.5, 2.5, 1.0)]);
        assert_eq!(b.fetch("BCA", h - 10.0 * H).await.unwrap(), vec![(h - 2.0 * H, 1.0, 2.0, 0.5, 1.5, 6.0), (h - H, 1.5, 2.5, 1.0, 2.0, 6.0)]);
        b.refresh(&[("BCA".to_string(), 1e6)]).await;
        assert_eq!(b.bars["BCA"].len(), 2);
        // the hour closes: the next refresh adds it, the last ones read again replace theirs
        f.set_candles("BCA", "1h", &[(h - H, 1.5, 2.6, 1.0, 2.1, 3.0), (h, 2.0, 3.0, 1.5, 2.5, 1.0)]);
        b.refresh(&[("BCA".to_string(), 1e6)]).await;
        assert_eq!(b.bars["BCA"].len(), 2);
        assert_eq!(b.bars["BCA"][1].2, 2.6, "the hour read again replaced");
    }
}
