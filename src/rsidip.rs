//! The RSI dip (`RSIDIP_ENABLED`: unset the trial, its first `TRIAL_TRADES` (3) positions live, then
//! paper; "1" live; "0" off; without `LIVE_KEY` / `LIVE_ACCOUNT` paper only). On each 5-minute close: a
//! main-dex perp with `RSIDIP_MIN_VOL_USD` (10M)+ a day whose RSI14 (Wilder, the 5m closes) was under
//! `RSIDIP_RSI_MAX` (35) the bar before and is up now, the close `RSIDIP_UNDER_PCT` (8) or more under the
//! EMA200 of the 5m closes, BTC's last 15 minutes under `RSIDIP_BTC_MAX_PCT` (0.5); or the wider signal: the
//! RSI under `RSIDIP_WIDE_RSI_MAX` (45.5) and up, `RSIDIP_WIDE_UNDER_PCT` (5.6)+ under the 5m EMA200, while
//! the price is `RSIDIP_H1_UNDER_PCT` (10.65; 0: off)+ under the EMA200 of the hourly closes (the coin in a
//! deep fall on the hours: there the weaker 5m hook pays as well): bought at market; up
//! to `RSIDIP_ADDS` (2) adds at market, each at a 5m close `RSIDIP_ADD_PCT` (3) under the last buy; out at
//! the average +`RSIDIP_TAKE_PCT` (2) (a resting reduce-only limit: maker), at the average
//! -`RSIDIP_STOP_PCT` (20) (a resting stop) or `RSIDIP_HOLD_H` (24) h after the first buy (market). After
//! each add the stop and the take are placed again for the whole size at the new average (the new stop
//! first; it not placed: the position closed). A buy risks a third of the family's risk
//! (`trades::risk_usd`: 3.3% of the equity, so 1.1% a buy at its 20% stop, 5.5% of the equity in coin;
//! `RSIDIP_RISK_USD` ($10) a buy when the family's is off) and a position reserves all its buys' risk in
//! the family's cap from the first; one a coin (the family's too), at most `RSIDIP_MAX_OPEN` (5; 3 until 2026-10-28: the
//! portfolio on one deposit, V75, skipped 656 of its signals for the slots; 5 takes 144 more, the cap binds past it) live
//! and as many on paper. Paper (the trial over, the cap full, no account): simulated on the mids, costs
//! taker 4.5 bp + 5 bp slippage, maker 1.5 bp, a stop +5 bp. Events kind `rsidip` (`open` / `add` /
//! `close` / `error`, `live` true or false), to the chat too.
//!
//! The research (2026-10-27, V52..V56): 202 + 1.58M configs from the user's pinescript logic, chosen
//! on 11 months, checked on 7, a placebo grid, Binance 2020..2026 and the raw tape: this one held on
//! the tape (the buys at the worst trade of the first block a second on), 2025-05 .. 2026-10: 1,127
//! trades, +73 bp a buy, +$1,196 at $100 a buy, w/o the 20 best days +$480, the 2 best days 12%,
//! 14 of 18 months up, t 3.8; at most 3 open: +$650, the drawdown with the open $212 at $100 a buy.
//! V61 (all 4,525 raw signals replayed on the tape a coin at a time, the stop and the take on the
//! average re-read after each add): 1,166 trades, +64 bp a buy; an independent version on the bars
//! agrees (98% the same exits). On Binance only on the coins HL lists (all of Binance: nothing), so the
//! live record decides.
//! V67..V72 (2026-10-28, the filter search, 33 kinds, checked against shuffled luck and on the tape): the wider
//! signal under the 1h EMA200 gate: 2,006 trades against 1,157, the win rate the same 95.8%, the sum x2.3
//! (w/o the 5 best days x2.0), 18 of 18 months up; at most 3 at once, compounding at 1.1% a buy: +62% over
//! 16.8 months, the drawdown with the open 9.1% (the old signal alone +42%, 10.9%). The gate on the hourly
//! candles (the EMA of the last 1,000 hours) decides as the research's minute EMA (span 12,000) 99.8% of the time.
//!
//! Prices: the main dex's mids each `POLL_S`, the 5m closes their last mid in each bar; after a
//! start the liquid coins' 5m candles (50 h) read, a coin each `BACKFILL_EVERY_S`; their hourly candles (1,000
//! h, for the 1h EMA200) read again each hour, a coin a poll. The live positions
//! checked on the account each `CHECK_EVERY_S`. Its state is the store's doc `rsidip`.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{Api, now};
use crate::config::Config;
use crate::dip::Closes;
use crate::live::{Placed, Venue};
use crate::log;
use crate::store::Store;
use crate::trades::{self, Trade};

const DOC: &str = "rsidip";
const BAR_MIN: i64 = 5;
const POLL_S: f64 = 10.0;
const VOLUME_EVERY_S: f64 = 300.0;
const BACKFILL_EVERY_S: f64 = 10.0;
const CHECK_EVERY_S: f64 = 30.0;
/// The hourly closes read for the 1h EMA200 (its EMA seeded at the first), and the fewest it is read from.
const H1_HOURS: f64 = 1000.0;
const H1_MIN: usize = 300;
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
    /// The wider signal: its RSI ceiling, its depth under the 5m EMA200, and how far under the 1h EMA200 it needs (0: off).
    wide_rsi_max: f64,
    wide_under: f64,
    h1_under: f64,
    /// The trial (`RSIDIP_ENABLED` unset): its live positions at most.
    trial: Option<u32>,
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { rsi_max: v("RSIDIP_RSI_MAX", 35.0), under: v("RSIDIP_UNDER_PCT", 8.0) / 100.0, btc_max: v("RSIDIP_BTC_MAX_PCT", 0.5) / 100.0,
            min_vol: v("RSIDIP_MIN_VOL_USD", 10e6), take: v("RSIDIP_TAKE_PCT", 2.0) / 100.0, stop: v("RSIDIP_STOP_PCT", 20.0) / 100.0,
            hold_s: v("RSIDIP_HOLD_H", 24.0) * 3600.0, adds: v("RSIDIP_ADDS", 2.0) as u32, add_step: v("RSIDIP_ADD_PCT", 3.0) / 100.0,
            max_open: v("RSIDIP_MAX_OPEN", 5.0) as usize, risk_usd: v("RSIDIP_RISK_USD", 10.0),
            wide_rsi_max: v("RSIDIP_WIDE_RSI_MAX", 45.5), wide_under: v("RSIDIP_WIDE_UNDER_PCT", 5.6) / 100.0, h1_under: v("RSIDIP_H1_UNDER_PCT", 10.65) / 100.0,
            trial: trades::switch("RSIDIP_ENABLED").flatten() }
    }

    /// The buys a position can have.
    fn buys(&self) -> f64 {
        (self.adds + 1) as f64
    }

    /// The $ a buy's stop loses: its share of the family's risk (`RSIDIP_RISK_USD` without it).
    fn buy_risk(&self) -> f64 {
        trades::risk_usd(self.risk_usd * self.buys()) / self.buys()
    }

    /// The $ of a buy: the risk over the stop.
    fn buy_usd(&self) -> f64 {
        self.buy_risk() / self.stop
    }
}

/// A position: its buys, their fees ($); `tr` the live one's account side (its size, the average,
/// the resting stop and take), None on paper.
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
    #[serde(default)]
    tr: Option<Trade>,
}

impl Pos {
    fn avg(&self) -> f64 {
        self.cost / self.qty
    }

    fn live(&self) -> bool {
        self.tr.is_some()
    }

    /// A buy of `qty` coins at `px`.
    fn fill(&mut self, px: f64, qty: f64) {
        self.qty += qty;
        self.cost += qty * px;
        self.units += 1;
        self.last = px;
        self.fees += TAKER * qty * px;
    }

    fn buy(&mut self, px: f64, usd: f64) {
        self.fill(px, usd / px);
    }

    /// The risk it holds in the family's cap: all its buys' (a buy's $ x the stop x the buys).
    fn reserve(&self, p: &Params) -> f64 {
        if self.units > 0 { self.cost / self.units as f64 * p.stop * p.buys() } else { 0.0 }
    }

    /// The $ at an exit at `px` (`why` its fee).
    fn pnl(&self, px: f64, why: &str) -> f64 {
        let fee = match why { "take" => MAKER, "stop" => TAKER + STOP_X, _ => TAKER };
        px * self.qty - self.cost - fee * px * self.qty - self.fees
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
    /// The live positions opened (the trial's count).
    #[serde(default)]
    opened: u32,
}

/// A live position: bought at market for `usd` (at the `mid` now), its stop and take at the fill.
async fn open_live(v: &Venue, coin: &str, usd: f64, mid: f64, p: &Params) -> Result<Pos> {
    let tr = trades::open_market(v, coin, usd / mid, p.stop, Some(p.take), p.hold_s).await?;
    let mut pos = Pos { coin: coin.to_string(), at: tr.at, ..Default::default() };
    pos.fill(tr.entry_px, tr.size);
    pos.tr = Some(tr);
    Ok(pos)
}

/// A live add: bought at market for `usd`; the stop and the take placed again for the whole size at
/// the new average (the new stop first, then the old orders cancelled). An error with the position's
/// size and average already updated: the caller closes it (`trades::close` sells the whole size).
async fn add_live(v: &Venue, pos: &mut Pos, usd: f64, mid: f64, p: &Params) -> Result<()> {
    let (got, px, _) = v.market(&pos.coin, usd / mid, false).await?;
    if got <= 0.0 {
        bail!("{}: nothing filled", pos.coin);
    }
    pos.fill(px, got);
    let (coin, qty, avg) = (pos.coin.clone(), pos.qty, pos.avg());
    let Some(tr) = pos.tr.as_mut() else { bail!("{coin}: not live") };
    tr.size = qty;
    tr.entry_px = avg;
    let stop_px = avg * (1.0 - p.stop);
    let stop_oid = v.stop(&coin, 1.0, qty, stop_px).await?;
    for o in [tr.stop_oid.take(), tr.take_oid.take()].into_iter().flatten() {
        v.cancel(&coin, o).await;
    }
    tr.stop_px = stop_px;
    tr.stop_oid = Some(stop_oid);
    tr.take_px = avg * (1.0 + p.take);
    match v.limit(&coin, -qty, tr.take_px, true, "Gtc").await {
        Ok(Placed::Resting(o)) => tr.take_oid = Some(o),
        Ok(Placed::Filled(..)) => {}
        Err(e) => log!("rsidip: {coin} take not placed (out on the stop or the time): {e:#}"),
    }
    Ok(())
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

/// The signal on the last close of `c` (`h1`: the 1h EMA200, None unknown): (the RSI the bar before, how far
/// under the 5m EMA200, the wider signal's or not).
fn signal(c: &[f64], p: &Params, h1: Option<f64>) -> Option<(f64, f64, bool)> {
    if c.len() < MIN_BARS {
        return None;
    }
    let (r, e) = (rsi(c, 14), ema(c, 200));
    let n = c.len() - 1;
    let under = c[n] / e[n] - 1.0;
    let hook = r[n] > r[n - 1];
    if hook && r[n - 1] < p.rsi_max && under < -p.under {
        return Some((r[n - 1], under, false));
    }
    let deep = p.h1_under > 0.0 && h1.is_some_and(|x| x > 0.0 && c[n] / x - 1.0 <= -p.h1_under);
    (deep && hook && r[n - 1] < p.wide_rsi_max && under < -p.wide_under).then_some((r[n - 1], under, true))
}

/// The EMA200 of the hourly closes (oldest first; seeded at the first), None with fewer than `H1_MIN`.
fn h1_ema(closes: &[f64]) -> Option<f64> {
    (closes.len() >= H1_MIN).then(|| *ema(closes, 200).last().unwrap())
}

fn usd(x: f64) -> String {
    if x >= 0.0 { format!("+${x:.2}") } else { format!("-${:.2}", -x) }
}

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    let live = ev["live"] == true;
    let tag = if live { "💵 RSI-дип НА ДЕНЬГИ" } else { "📝 RSI-дип (бумага, без сделки)" };
    Some(match ev["what"].as_str()? {
        "open" => format!("{tag} ВХОД {coin}{}: RSI был {:.0} и развернулся, цена {:.1}% ниже EMA200{}; по {}, ${:.0}, тейк {} / стоп {}, до {:.0} ч{}",
            if ev["wide"] == true { " (расширенный сигнал)" } else { "" }, f("rsi"), -f("under") * 100.0,
            if ev["wide"] == true { format!(" и {:.1}% ниже часовой EMA200", -f("under_h1") * 100.0) } else { String::new() },
            f("px"), f("usd"), f("take_px"), f("stop_px"), f("hold_h"), ev["trial"].as_str().unwrap_or("")),
        "add" => format!("{tag} ДОБОР {coin} №{:.0}: по {}, средняя {}, тейк {} / стоп {} (на весь объём)", f("units"), f("px"), f("avg"), f("take_px"), f("stop_px")),
        "close" => format!("{} {tag} {} {coin}: средняя {} -> {}, покупок {:.0}, {} ({:+.2}%) | всего {:.0} сделок, в плюсе {:.0}, итог {}",
            if f("pnl_usd") >= 0.0 { "✅" } else { "🔻" },
            match ev["why"].as_str().unwrap_or("") { "take" => "ТЕЙК", "stop" => "СТОП", "error" => "ЗАКРЫТ ИЗ-ЗА ОШИБКИ", _ => "ВЫХОД ПО ВРЕМЕНИ" },
            f("avg"), f("px"), f("units"), usd(f("pnl_usd")), f("ret_pct"), f("closed"), f("wins"), usd(f("total_usd"))),
        "error" => format!("⚠️ RSI-дип {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        _ => return None,
    })
}

/// Starts the RSI dip unless `RSIDIP_ENABLED` is "0" (see the module).
pub async fn start(cfg: &Config, store: Store, api: Api) -> Result<()> {
    if trades::switch("RSIDIP_ENABLED").is_none() {
        log!("rsidip: off (RSIDIP_ENABLED=0)");
        return Ok(());
    }
    let p = Params::from_env();
    let venue = crate::live::venue_from_env(None).await?;
    let mut state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    for pos in state.open.iter().filter(|x| x.live()) {
        trades::register(&pos.coin, "rsidip", pos.reserve(&p));
    }
    let mode = match (&venue, p.trial) {
        (None, _) => "paper (no LIVE_KEY / LIVE_ACCOUNT)".to_string(),
        (Some(_), Some(n)) => format!("the trial: the first {n} live (opened {}), then paper", state.opened),
        (Some(_), None) => "live".to_string(),
    };
    log!("rsidip: {mode}; RSI14 5m hook from < {:.0}, {:.0}%+ under the EMA200 (or from < {:.1}, {:.1}%+ under, {:.2}%+ under the 1h EMA200), BTC 15 min < {:.1}%, ${:.0}M+; take {:.0}% stop {:.0}% {:.0} h, {} adds {:.0}% apart, risk {} a position, at most {}; {} open, {} closed",
        p.rsi_max, p.under * 100.0, p.wide_rsi_max, p.wide_under * 100.0, p.h1_under * 100.0, p.btc_max * 100.0, p.min_vol / 1e6, p.take * 100.0, p.stop * 100.0,
        p.hold_s / 3600.0, p.adds, p.add_step * 100.0, trades::sizing_label(p.risk_usd * p.buys(), p.stop), p.max_open, state.open.len(), state.closed);
    crate::notify::send(format!("{} RSI-дип запущен{}: 5-минутки, RSI14 разворот из-под {:.0}, цена на {:.0}%+ ниже EMA200 (или разворот из-под {:.1} и {:.1}%+ ниже, если монета на {:.2}%+ ниже часовой EMA200), BTC спокоен (за 15 мин меньше {:.1}%), оборот от ${:.0}M; тейк +{:.0}% / стоп −{:.0}% от средней, до {:.0} ч, {} добора через {:.0}%, риск на позицию (все {:.0} покупки) {}, до {} позиций. Открыто {}, закрыто {} (итог {}){}",
        if venue.is_some() { "💵" } else { "📝" }, if venue.is_some() { " НА ДЕНЬГИ" } else { " НА БУМАГЕ (без сделок)" },
        p.rsi_max, p.under * 100.0, p.wide_rsi_max, p.wide_under * 100.0, p.h1_under * 100.0, p.btc_max * 100.0, p.min_vol / 1e6, p.take * 100.0, p.stop * 100.0,
        p.hold_s / 3600.0, p.adds, p.add_step * 100.0, p.buys(),
        trades::sizing_label(p.risk_usd * p.buys(), p.stop), p.max_open, state.open.len(), state.closed, usd(state.pnl_usd),
        if venue.is_some() { trades::trial_label(p.trial, state.opened).replace("сделки", "позиции") } else { String::new() }));
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let mut closes = Closes::default();
        let mut volume: HashMap<String, f64> = HashMap::new();
        let mut backfilled: HashSet<String> = HashSet::new();
        // Coin -> (the hour it was read in, its 1h EMA200).
        let mut h1: HashMap<String, (i64, Option<f64>)> = HashMap::new();
        let (mut vol_at, mut backfill_at, mut check_at) = (0.0, 0.0, 0.0);
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
        // A position out at `px`: the tally, the event, the family's cap freed.
        let closed = |store: &Store, state: &mut State, pos: &Pos, why: &str, px: f64, t: f64| {
            let pnl = pos.pnl(px, why);
            state.closed += 1;
            state.wins += (pnl > 0.0) as u32;
            state.pnl_usd += pnl;
            if pos.live() {
                trades::unregister(&pos.coin);
            }
            event(store, json!({"what": "close", "live": pos.live(), "coin": pos.coin, "why": why, "avg": pos.avg(), "px": px, "units": pos.units, "pnl_usd": pnl,
                "ret_pct": pnl / pos.cost * 100.0, "held_h": (t - pos.at) / 3600.0, "closed": state.closed, "wins": state.wins, "total_usd": state.pnl_usd}));
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
            // The 1h EMA200s: a liquid coin not read this hour, the biggest first, one a poll.
            let hour = (t / 3600.0).floor() as i64;
            if p.h1_under > 0.0 {
                let mut todo: Vec<(&String, &f64)> = volume.iter().filter(|(c, v)| **v >= p.min_vol && c.as_str() != "BTC" && h1.get(*c).is_none_or(|x| x.0 < hour)).collect();
                todo.sort_by(|a, b| b.1.total_cmp(a.1));
                if let Some(c) = todo.first().map(|x| x.0.clone()) {
                    match api.bar_closes(&c, 60, t - H1_HOURS * 3600.0).await {
                        Ok(v) => {
                            let closes: Vec<f64> = v.iter().filter(|x| x.0 < hour).map(|x| x.1).collect();      // the completed hours
                            h1.insert(c, (hour, h1_ema(&closes)));
                        }
                        Err(e) => {
                            log!("rsidip: {c} hourly candles not read: {e:#}");
                            let old = h1.get(&c).and_then(|x| x.1);
                            h1.insert(c, (hour, old));
                        }
                    }
                }
            }
            let mut changed = false;
            // The live positions on the account: out on time; gone (the stop or the take filled).
            if let Some((v, unified)) = &venue {
                if t - check_at >= CHECK_EVERY_S && state.open.iter().any(|x| x.live()) {
                    check_at = t;
                    match crate::live::equity_of(&v.ex, &v.account, *unified).await {
                        Ok((eq, held)) => {
                            trades::set_equity(eq);
                            let mut keep = Vec::new();
                            for pos in std::mem::take(&mut state.open) {
                                let Some(tr) = pos.tr.clone() else {
                                    keep.push(pos);
                                    continue;
                                };
                                let out = if t >= tr.exit_at {
                                    match trades::close(v, &tr).await {
                                        Ok(px) => Some(("time", px)),
                                        Err(e) => {
                                            event(&store, json!({"what": "error", "live": true, "coin": pos.coin, "error": format!("exit: {e:#}")}));
                                            None
                                        }
                                    }
                                } else {
                                    trades::gone(v, &tr, held.get(&pos.coin).copied().unwrap_or(0.0)).await
                                };
                                match out {
                                    Some((why, px)) => {
                                        closed(&store, &mut state, &pos, why, px, t);
                                        changed = true;
                                    }
                                    None => keep.push(pos),
                                }
                            }
                            state.open.extend(keep);
                        }
                        Err(e) => log!("rsidip: account not read: {e:#}"),
                    }
                }
            }
            // The paper positions' exits, on the mids now: the take, the stop, the time.
            let mut keep = Vec::new();
            for pos in std::mem::take(&mut state.open) {
                let Some(&mid) = mids.get(&pos.coin).filter(|_| !pos.live()) else {
                    keep.push(pos);
                    continue;
                };
                let avg = pos.avg();
                let why = if mid >= avg * (1.0 + p.take) { Some("take") } else if mid <= avg * (1.0 - p.stop) { Some("stop") } else if t - pos.at >= p.hold_s { Some("time") } else { None };
                let Some(why) = why else {
                    keep.push(pos);
                    continue;
                };
                closed(&store, &mut state, &pos, why, if why == "take" { avg * (1.0 + p.take) } else { mid }, t);
                changed = true;
            }
            state.open = keep;
            if bar > last_bar {
                let b = bar - 1;          // the bar that just closed
                // The adds: a close the step under the last buy (live: at market, the stop and the take moved).
                let mut keep = Vec::new();
                for mut pos in std::mem::take(&mut state.open) {
                    let (Some(c), Some(&mid)) = (closes.0.get(&pos.coin).map(|q| series(q, b)), mids.get(&pos.coin)) else {
                        keep.push(pos);
                        continue;
                    };
                    if pos.units > p.adds || !c.last().is_some_and(|x| *x <= pos.last * (1.0 - p.add_step)) {
                        keep.push(pos);
                        continue;
                    }
                    let usd_buy = pos.cost / pos.units as f64;          // the first buy's $ again
                    match &venue {
                        Some((v, _)) if pos.live() => {
                            if let Err(e) = add_live(v, &mut pos, usd_buy, mid, &p).await {
                                event(&store, json!({"what": "error", "live": true, "coin": pos.coin, "error": format!("add: {e:#}; closing the position")}));
                                let tr = pos.tr.clone().unwrap_or_default();
                                match trades::close(v, &tr).await {
                                    Ok(px) => closed(&store, &mut state, &pos, "error", px, t),
                                    Err(e) => {
                                        event(&store, json!({"what": "error", "live": true, "coin": pos.coin, "error": format!("not closed: {e:#}")}));
                                        keep.push(pos);
                                    }
                                }
                                changed = true;
                                continue;
                            }
                        }
                        _ => pos.buy(mid, usd_buy),
                    }
                    let avg = pos.avg();
                    event(&store, json!({"what": "add", "live": pos.live(), "coin": pos.coin, "px": pos.last, "units": pos.units, "avg": avg,
                        "take_px": avg * (1.0 + p.take), "stop_px": avg * (1.0 - p.stop)}));
                    keep.push(pos);
                    changed = true;
                }
                state.open = keep;
                // The signals.
                let btc = closes.0.get("BTC").map(|q| series(q, b)).filter(|c| c.len() > 3).map(|c| c[c.len() - 1] / c[c.len() - 4] - 1.0);
                if btc.is_some_and(|x| x.abs() < p.btc_max) {
                    let mut cands: Vec<(String, f64, f64, bool)> = Vec::new();
                    for (c, q) in &closes.0 {
                        if c == "BTC" || volume.get(c).copied().unwrap_or(0.0) < p.min_vol || state.open.iter().any(|x| &x.coin == c) {
                            continue;
                        }
                        if let Some((r, u, w)) = signal(&series(q, b), &p, h1.get(c).and_then(|x| x.1)) {
                            cands.push((c.clone(), r, u, w));
                        }
                    }
                    cands.sort_by(|a, b| a.2.total_cmp(&b.2));      // the deepest first
                    for (c, r, u, wide) in cands {
                        let Some(&mid) = mids.get(&c) else { continue };
                        let n_live = state.open.iter().filter(|x| x.live()).count();
                        let usd_buy = p.buy_usd();
                        let reserve = usd_buy * p.stop * p.buys();
                        let live = venue.as_ref().filter(|(v, _)| v.coins.contains_key(&c) && n_live < p.max_open
                            && trades::trial_open(p.trial, state.opened) && trades::can_open(&c, reserve));
                        let mut pos = match live {
                            Some((v, _)) => match open_live(v, &c, usd_buy, mid, &p).await {
                                Ok(pos) => {
                                    state.opened += 1;
                                    trades::register(&c, "rsidip", pos.reserve(&p));
                                    pos
                                }
                                Err(e) => {
                                    event(&store, json!({"what": "error", "live": true, "coin": c, "error": format!("open: {e:#}")}));
                                    continue;
                                }
                            },
                            None if state.open.len() - n_live < p.max_open => {
                                let mut pos = Pos { coin: c.clone(), at: t, ..Default::default() };
                                pos.buy(mid, usd_buy);
                                pos
                            }
                            None => continue,
                        };
                        pos.rsi = r;
                        pos.under = u;
                        let trial = if pos.live() && !trades::trial_open(p.trial, state.opened) { ", ПРОБА окончена: дальше на бумаге" } else { "" };
                        event(&store, json!({"what": "open", "live": pos.live(), "coin": c, "px": pos.last, "usd": pos.cost, "rsi": r, "under": u, "wide": wide,
                            "under_h1": h1.get(&c).and_then(|x| x.1).map(|e| pos.last / e - 1.0),
                            "take_px": pos.avg() * (1.0 + p.take), "stop_px": pos.avg() * (1.0 - p.stop), "hold_h": p.hold_s / 3600.0, "trial": trial}));
                        state.open.push(pos);
                        changed = true;
                    }
                }
                if bar % 12 == 0 {
                    let ready = closes.0.iter().filter(|(c, q)| volume.get(*c).copied().unwrap_or(0.0) >= p.min_vol && q.len() >= MIN_BARS).count();
                    let liquid = volume.values().filter(|v| **v >= p.min_vol).count();
                    let deep: Vec<String> = h1.iter().filter_map(|(c, x)| Some((c, mids.get(c)? / x.1? - 1.0))).filter(|x| x.1 <= -p.h1_under).map(|x| format!("{} {:.1}%", x.0, x.1 * 100.0)).collect();
                    log!("rsidip: {ready} of {liquid} liquid coins with {MIN_BARS}+ bars, {} with the 1h EMA200 (deep under it: {}), {} open ({} live), {} closed, {}",
                        h1.values().filter(|x| x.1.is_some()).count(), if deep.is_empty() { "none".to_string() } else { deep.join(", ") }, state.open.len(),
                        state.open.iter().filter(|x| x.live()).count(), state.closed, usd(state.pnl_usd));
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
        assert!(signal(&c, &p, None).is_none());                // still falling: no hook
        c.push(85.2);
        let (r, u, w) = signal(&c, &p, None).expect("a hook under 35, 8%+ under the EMA200");
        assert!(!w);
        assert!(r < 35.0 && u < -0.08, "{r} {u}");
        assert!(signal(&c[..300], &p, None).is_none());          // too few bars
        // the wider signal: a fall to ~93 (about 7% under the 5m EMA200: not the bot's 8%) and a tick up; taken only
        // when the price is 10.65%+ under the 1h EMA200
        let p = Params { rsi_max: 35.0, under: 0.08, wide_rsi_max: 45.5, wide_under: 0.056, h1_under: 0.1065, ..Params::from_env() };
        let mut c: Vec<f64> = vec![100.0; 400];
        c.extend((1..=14).map(|i| 100.0 - i as f64 * 0.5));
        c.push(93.2);
        assert!(signal(&c, &p, None).is_none());                 // the 1h EMA unknown: no wider signal
        assert!(signal(&c, &p, Some(100.0)).is_none());          // 6.8% under the 1h EMA200: not deep enough
        let (r, u, w) = signal(&c, &p, Some(105.0)).expect("the wider signal 11.2% under the 1h EMA200");
        assert!(w && r < 45.5 && u < -0.056 && u > -0.08, "{r} {u}");
        assert!(signal(&c, &Params { h1_under: 0.0, ..p.clone() }, Some(105.0)).is_none());    // the gate off
        assert!(h1_ema(&[1.0; 299]).is_none() && (h1_ema(&[2.0; 300]).unwrap() - 2.0).abs() < 1e-12);
    }

    /// The indicators against the research's (Python, v52lib) on real 5m closes: `RSIDIP_FIXTURE` = a JSON list of
    /// {closes, rsi, ema, signal}. `cargo test -- --ignored the_research_s_indicators`.
    #[test]
    #[ignore]
    fn the_research_s_indicators() {
        let path = std::env::var("RSIDIP_FIXTURE").expect("RSIDIP_FIXTURE");
        let fx: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let arr = |v: &Value| v.as_array().unwrap().iter().map(|x| x.as_f64().unwrap()).collect::<Vec<f64>>();
        let p = Params::from_env();
        for f in &fx {
            let (c, r0, e0) = (arr(&f["closes"]), arr(&f["rsi"]), arr(&f["ema"]));
            let (r, e) = (rsi(&c, 14), ema(&c, 200));
            for i in 0..c.len() {
                assert!((r[i] - r0[i]).abs() < 1e-9, "{} rsi[{i}] {} vs {}", f["coin"], r[i], r0[i]);
                assert!((e[i] / e0[i] - 1.0).abs() < 1e-12, "{} ema[{i}] {} vs {}", f["coin"], e[i], e0[i]);
            }
            assert_eq!(signal(&c, &p, None).is_some(), f["signal"].as_bool().unwrap(), "{} signal", f["coin"]);
        }
    }

    #[test]
    fn a_paper_buy() {
        let mut pos = Pos::default();
        pos.buy(100.0, 50.0);
        pos.buy(97.0, 50.0);
        assert!((pos.avg() - 98.477).abs() < 1e-3, "{}", pos.avg());
        assert!((pos.fees - 2.0 * 50.0 * TAKER).abs() < 1e-12);
    }

    /// The stop and the take on the average after the adds (V61's RESOLV walk-through: three equal $
    /// buys, the average 0.142567, the stop 0.114054), the reserve, a buy a third of the family's risk.
    #[test]
    fn the_average_the_stop_and_the_risk() {
        let p = Params { stop: 0.20, take: 0.02, adds: 2, risk_usd: 10.0, ..Params::from_env() };
        let mut pos = Pos::default();
        for px in [0.15314, 0.14226, 0.13363] {
            pos.buy(px, 100.0);
        }
        assert!((pos.avg() - 0.142567).abs() < 1e-6, "{}", pos.avg());
        assert!((pos.avg() * (1.0 - p.stop) - 0.114054).abs() < 1e-6);
        assert!((pos.reserve(&p) - 60.0).abs() < 1e-9);           // 3 buys of $100 at a 20% stop
        // out at the take: the average +2% on all the coins, less the maker fee and the buys' fees
        let px = pos.avg() * 1.02;
        assert!((pos.pnl(px, "take") - (300.0 * 0.02 - MAKER * 306.0 - 3.0 * TAKER * 100.0)).abs() < 1e-9);
        // a buy: a third of the family's risk (off: RSIDIP_RISK_USD a buy)
        trades::set_equity(1000.0);
        let fam = trades::family_risk_pct();
        let want = if fam > 0.0 { fam / 100.0 * 1000.0 / 3.0 } else { 10.0 };
        assert!((p.buy_risk() - want).abs() < 1e-9, "{} {want}", p.buy_risk());
        assert!((p.buy_usd() - want / 0.20).abs() < 1e-9);
    }
}
