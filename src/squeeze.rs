//! The 4h squeeze breakout long (`SQZ_ENABLED`: unset or "1" live, "0" off; with `LIVE_KEY` / `LIVE_ACCOUNT` / `LIVE_NET` as for the dip):
//! after each 4h candle's close (UTC 00, 04, ...), a main-dex perp with `SQZ_MIN_VOL_USD` (10M) or more a day is
//! bought at market when the candle before was a squeeze (the 20-day Bollinger width, std / mean of 120 closes, in its
//! lowest `SQZ_QUANTILE` (0.2) of the last 120 days) and this candle closed over the upper band (mean + 2 std), while
//! BTC's 4h close is over its 50-day mean. Out at market when a 4h candle closes `SQZ_ATR_K` (3) x ATR (the mean true
//! range of 120 candles, at the entry) under the entry, or `SQZ_HOLD_DAYS` (10) on. The size: `SQZ_RISK_USD` ($20;
//! `FAMILY_RISK_PCT` of the equity when set) lost at that stop; a resting stop twice that far guards a crash between
//! the closes. At most `SQZ_MAX_OPEN` (3) and the family's cap; one a coin.
//!
//! The research (2026-10-31, V136 / V137, Binance 1m 2020-26, the HL-listed coins): +0.49 R a trade against 0.00 on
//! random bars, every year up (2022 +22 R, 2025 +199); chosen on 2020-22 it made +0.48 R on the untouched 2023-26, and
//! all 32 squeeze variants were up there; the entry a candle later +0.38, costs x2 +0.46, w/o the 10 best coins +0.27;
//! the daily P&L corr with the live strategies 0.00, no entry on a coin a live trade held; with the live set and the
//! trend long at the same drawdown 18%: x33 -> x55 (3 slots; 5 slots worse). HL 2025-10 .. 2026-10: +0.27 R a trade.
//! Win 40%: the profit is in the few runs.
//!
//! No entry on a candle closing on the UTC weekend (Saturday / Sunday), either side (2026-11-23, the audit; night/sqs3.py,
//! the live slots, 3 open): the short's weekend entries -0.17 R a trade on Binance 2020-26 (n 174) and -0.33 R on HL (n 163),
//! the weekdays +0.40 / +0.05 R; without them the short's sum Binance 2025-26 +38 -> +44 R, HL +22 -> +29 R, Binance 2020-26
//! the same (+131 / +127); the long's sum Binance 2020-26 +62 -> +74 R (2023-24 +21 -> +45), HL -22 -> +10 R. The HL portfolio
//! (2%, the live floors on the real HL volume, night/floors.py) at DD 40%: +830% -> +853% (the long) / +864% (the short), both halves.
//!
//! The SHORT side (`SQZS_ENABLED`, as the long's): the mirror, a 4h close under the lower
//! band after a squeeze (the lowest `SQZS_QUANTILE` (0.1) width), any BTC, `SQZS_MIN_VOL_USD` ($5M; 2026-11-23: HL's day volume is
//! ~0.21 of Binance's on the same coin-days (median), so the research's Binance $30M is ~$5-6M on HL; the live slots on HL
//! 2025-05 .. 2026-10: $10M 31 trades +0.14 R, $5M 45 +0.27 R, $2M 68 +0.32 R, under $1M negative); out on a 4h close `SQZS_ATR_K` (2) ATR over the
//! entry or `SQZS_HOLD_DAYS` (5) on; `SQZS_RISK_SHARE` (1: the full family risk; the hedge of the longs, robust/bn_full2.py) of the family's risk; at most `SQZS_MAX_OPEN` (3). V144 / V145
//! (Binance 2020-26): +0.32 R a trade vs -0.04 on random bars, 6 of 7 years (2022 +128 R), +0.36 R on the untouched
//! 2023-26; a candle late +0.27, costs x2 +0.29, w/o the 10 best coins +0.24; the daily P&L corr with the live +0.03, the
//! trend -0.04, the long squeeze -0.01; in the portfolio at half the risk up in all four spans (Binance 2020-23 / 2023-26,
//! HL halves: +12 / +328 / +12 / +4% at the same drawdown) and the Binance drawdown at 2% 48% -> 37%: a hedge of the longs.
//! On HL alone flat so far (731 trades 2025-26, -0.02 R).
//!
//! The short trades on the shorts' sub-account (`sub`, 2026-11-21, the user's call): a long another strategy holds on
//! the coin no longer bars it. Before an entry the sub-account gets its margin and its loss to the resting stop (and the
//! open shorts' losses to theirs) over what is free there; after each 4h step what is free over those and one trade's
//! risk (`trades::risk_usd`, the user's "always 2% risk there") goes back to the live account.
//!
//! The states: the store's docs `squeeze` and `squeeze_short`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::now;
use crate::config::Config;
use crate::keeper::{HeldIn, Keeper};
use crate::notify::usd;
use crate::log;
use crate::store::Store;
use crate::trades::{self, Trade};

const BAR: f64 = 4.0 * 3600.0;
const DAY: f64 = 86_400.0;
/// The step this long after a 4h candle's close (the candle final).
const AFTER_CLOSE_S: f64 = 60.0;
const CHECK_EVERY_S: f64 = 300.0;
/// The band's and the ATR's window (20 days of 4h candles); the width's history (120 days).
const N_BAND: usize = 120;
const N_HIST: usize = 720;
const N_HIST_MIN: usize = 360;

#[derive(Clone, Debug)]
struct Params {
    q: f64,
    atr_k: f64,
    hold_s: f64,
    btc_ma: usize,
    min_vol: f64,
    risk_usd: f64,
    /// `SQZ_RISK_SHARE` (1; 0.5 for 2026-11-04 only, back to 1 by the user) / `SQZS_RISK_SHARE` (1): the share of the family's risk a trade.
    /// robust/bn_full2.py, the full portfolio (dip, RSI dip, trend, both squeezes), the shares trend / long / short at the same
    /// DD 50%: Binance 2021-26 1/1/1 +8446%, 0.5/0.5/0.5 +5786%, 0.5/0.5/1 +17024%; HL 2025-26 at 2% +568% DD 44.1% / +718% 31.6% /
    /// +662% 36.9%: the short hedges the longs over the 6 years, so it keeps the full share.
    share: f64,
    max_open: usize,
    /// The trial (`SQZ_ENABLED` / `SQZS_ENABLED` set empty): its live trades at most.
    trial: Option<u32>,
    /// +1 the breakout long, -1 the breakdown short.
    side: f64,
    /// The registry's and the events' kind, the state's doc.
    kind: &'static str,
    /// Only while BTC is over its mean (the long).
    btc_filter: bool,
    /// No entry on a candle closing on the UTC weekend (both sides; see the module's doc).
    no_weekend: bool,
    /// `SQZ_OWN_DAYS` (30 for the long; 0 = off, the short): new entries only while the strategy's own paper trades (all
    /// its signals on the liquid coins, traded by its rules from the 4h candles, see `paper`) closed in the last that many
    /// days sum under 0 R. The research (2026-11-05, night/stage1-5, Binance 5m 2021-26, the HL coins): the best of ~600
    /// regime gates on 2021-23 (+113 R), and on the untouched 2023-26 +46 R; a trade's mean +0.86 R vs +0.46 (+0.73 vs
    /// +0.43 in 2023-26), 6 of 6 years; the other Binance coins 6 of 7; the full portfolio (the live family) at the same
    /// drawdown both halves better (+7851% vs +3191%, +1628% vs +693%), beats a random removal of the same count (+71k% vs
    /// a median +17k%); the windows 14 / 60 days the same way. HL 2025-26: a trade +0.27 R in vs +0.01 out, the portfolio
    /// -11% (322 trades). The other strategies: no such gate survived (the short worse with it).
    own_days: f64,
}

impl Params {
    fn from_env() -> Self {
        let v = trades::env_f;
        Params { q: v("SQZ_QUANTILE", 0.2), atr_k: v("SQZ_ATR_K", 3.0), hold_s: v("SQZ_HOLD_DAYS", 10.0) * DAY, btc_ma: (v("SQZ_BTC_MA_DAYS", 50.0) * 6.0) as usize,
            min_vol: v("SQZ_MIN_VOL_USD", 10e6), risk_usd: v("SQZ_RISK_USD", 20.0), share: v("SQZ_RISK_SHARE", 1.0), max_open: v("SQZ_MAX_OPEN", 3.0) as usize,
            trial: trades::switch("SQZ_ENABLED").flatten(), side: 1.0, kind: "squeeze", btc_filter: true, no_weekend: true, own_days: v("SQZ_OWN_DAYS", 30.0) }
    }

    fn short_from_env() -> Self {
        let v = trades::env_f;
        Params { q: v("SQZS_QUANTILE", 0.1), atr_k: v("SQZS_ATR_K", 2.0), hold_s: v("SQZS_HOLD_DAYS", 5.0) * DAY, btc_ma: (v("SQZ_BTC_MA_DAYS", 50.0) * 6.0) as usize,
            min_vol: v("SQZS_MIN_VOL_USD", 5e6), risk_usd: v("SQZS_RISK_USD", 20.0), share: v("SQZS_RISK_SHARE", 1.0), max_open: v("SQZS_MAX_OPEN", 3.0) as usize,
            trial: trades::switch("SQZS_ENABLED").flatten(), side: -1.0, kind: "squeeze_short", btc_filter: false, no_weekend: true, own_days: 0.0 }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pos {
    tr: Trade,
    /// The stop on the 4h closes: out when a candle closes at or under it.
    close_stop: f64,
    /// The $ lost at that stop (the family's risk).
    risk: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    open: Vec<Pos>,
    /// The last step done (the candle's open, s).
    #[serde(default)]
    stepped: f64,
    #[serde(default)]
    opened: u32,
}

/// A coin's closed 4h candles: (open time s, high, low, close).
type Bar = (f64, f64, f64, f64);

fn mean(x: &[f64]) -> f64 {
    x.iter().sum::<f64>() / x.len() as f64
}

/// The sample std (as pandas' rolling std).
fn std(x: &[f64]) -> f64 {
    let m = mean(x);
    (x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (x.len() as f64 - 1.0)).sqrt()
}

/// The q-quantile with the linear interpolation (as pandas').
fn quantile(x: &[f64], q: f64) -> f64 {
    let mut v = x.to_vec();
    v.sort_by(f64::total_cmp);
    let h = (v.len() - 1) as f64 * q;
    let (lo, hi) = (h.floor() as usize, h.ceil() as usize);
    v[lo] + (v[hi] - v[lo]) * (h - lo as f64)
}

/// The Bollinger width (std / mean of the `N_BAND` closes to i) at each candle from `N_BAND - 1` on.
fn widths(c: &[f64]) -> Vec<Option<f64>> {
    (0..c.len()).map(|i| (i + 1 >= N_BAND).then(|| { let w = &c[i + 1 - N_BAND..=i]; std(w) / mean(w) })).collect()
}

/// The ATR (the mean true range of the `N_BAND` candles to the last).
fn atr(b: &[Bar]) -> Option<f64> {
    if b.len() < N_BAND + 1 {
        return None;
    }
    let k = b.len();
    Some(mean(&(k - N_BAND..k).map(|i| (b[i].1 - b[i].2).max((b[i].1 - b[i - 1].3).abs()).max((b[i].2 - b[i - 1].3).abs())).collect::<Vec<_>>()))
}

/// Whether the last candle breaks out of a squeeze: the candle before's width within its lowest `q` of the widths
/// of the `N_HIST` candles to it (at least `N_HIST_MIN` known), the last close over the band's mean + 2 std (`side` +1)
/// or under its mean - 2 std (-1).
fn breakout(b: &[Bar], q: f64, side: f64) -> bool {
    let c: Vec<f64> = b.iter().map(|x| x.3).collect();
    !c.is_empty() && breakout_at(&c, &widths(&c), c.len() - 1, q, side)
}

/// `breakout` at the candle `i` of the closes `c` (their widths `w`).
fn breakout_at(c: &[f64], w: &[Option<f64>], i: usize, q: f64, side: f64) -> bool {
    if i + 1 < N_BAND + N_HIST_MIN + 1 {
        return false;
    }
    let prev = i - 1;
    let hist: Vec<f64> = w[prev + 1 - N_HIST.min(prev + 1)..=prev].iter().flatten().copied().collect();
    let Some(wp) = w[prev] else { return false };
    if hist.len() < N_HIST_MIN || wp > quantile(&hist, q) {
        return false;
    }
    let band = &c[i + 1 - N_BAND..=i];
    if side > 0.0 { c[i] > mean(band) + 2.0 * std(band) } else { c[i] < mean(band) - 2.0 * std(band) }
}

/// The coin's signals in its last `look` candles (the last, the live one, left out) traded on paper by the rules, one at a
/// time: in at the breakout's close, out on a 4h close `atr_k` ATR against it (R -1 or worse), on the resting stop twice
/// that far (R -2) or `hold` candles on; only where `btc_ok` (the candle's open time s). -> (the exit candle's close s,
/// R = the return / the stop's distance) of each closed one.
fn paper(b: &[Bar], q: f64, side: f64, atr_k: f64, hold: usize, look: usize, btc_ok: &dyn Fn(f64) -> bool) -> Vec<(f64, f64)> {
    let c: Vec<f64> = b.iter().map(|x| x.3).collect();
    let w = widths(&c);
    let mut out = Vec::new();
    let mut i = c.len().saturating_sub(look + 1).max(1);
    while i + 1 < c.len() {
        let dist = atr(&b[..=i]).map(|a| atr_k * a / c[i]).unwrap_or(0.0);
        if dist > 0.0 && dist < 0.4 && btc_ok(b[i].0) && breakout_at(&c, &w, i, q, side) {
            let px = c[i];
            let mut done = None;
            for j in i + 1..c.len().min(i + hold + 1) {
                if (side > 0.0 && b[j].2 <= px * (1.0 - 2.0 * dist)) || (side < 0.0 && b[j].1 >= px * (1.0 + 2.0 * dist)) {
                    done = Some((j, -2.0));
                    break;
                }
                let r = side * (c[j] / px - 1.0) / dist;
                if r <= -1.0 || j == i + hold {
                    done = Some((j, r));
                    break;
                }
            }
            let Some((j, r)) = done else { break };   // still open: no later signal on the coin
            out.push((b[j].0 + BAR, r));
            i = j + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// Whether BTC's last close is over the mean of its last `n` closes.
/// The 4h candle opened at `bar` (s) closes on the UTC weekend (its close is the entry).
fn weekend_close(bar: f64) -> bool {
    crate::wkd::weekend(((bar + BAR) / 60.0).floor() as i64)
}

fn btc_up(b: &[Bar], n: usize) -> bool {
    b.len() >= n && b[b.len() - 1].3 > mean(&b[b.len() - n..].iter().map(|x| x.3).collect::<Vec<_>>())
}

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    let short = ev["kind"] == "squeeze_short";
    let nm = if short { "шорт пробоя вниз из сжатия" } else { "пробой из сжатия" };
    Some(match ev["what"].as_str()? {
        "open" => format!("🗜 {} {coin}: 4ч закрытие {} полосы, цена {}, на ${:.0}, выход: закрытие 4ч {} {} или через {:.0} дн., аварийный стоп {}",
            nm.to_uppercase(), if short { "под" } else { "над" }, f("px"), f("notional"), if short { "выше" } else { "ниже" }, f("close_stop"), f("hold_days"), f("stop_px")),
        "close" => format!("{} {nm} {} {coin}: {} -> {} ({:+.2}%, {}, {:.1} дн.)", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            match ev["why"].as_str().unwrap_or("") { "stop" => "АВАРИЙНЫЙ СТОП", "close_stop" => "СТОП по закрытию 4ч", "manual" => "ЗАКРЫТО ВНЕ БОТА (руками или ликвидация)", _ => "ЗАКРЫЛ по времени" },
            f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_usd")), f("held_days")),
        "error" => format!("⚠️ {nm} {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        "signal" if !crate::notify::PAPER_TO_CHAT => return None,
        "signal" => format!("🗜 сигнал: {nm} {coin} по {} (проба окончена, без сделки)", f("px")),
        "weekend" if !crate::notify::PAPER_TO_CHAT => return None,
        "weekend" => format!("🗜 {nm} {coin} по {}: пропущен — свеча закрылась в выходные (в выходные не входим)", f("px")),
        "own_off" => format!("🗜 {nm} {coin} по {}: пропущен — свои бумажные сделки за {:.0} дн. {:+.1}R ({:.0} шт.), вход только после своего минуса",
            f("px"), f("days"), f("own_r"), f("papers")),
        _ => return None,
    })
}

struct Squeeze {
    k: Keeper,
    p: Params,
    state: State,
    /// On the shorts' sub-account: the money's mover (None: `MAIN_KEY` unset, nothing moves).
    sub: Option<Option<crate::sub::Mover>>,
}

impl Squeeze {
    /// A coin's closed 4h candles over the last `n` (the forming one left out).
    async fn bars(&self, coin: &str, n: usize) -> Option<Vec<Bar>> {
        let t = now();
        match crate::market::candles(&self.k.venue.ex, coin, "4h", t - (n as f64 + 2.0) * BAR).await {
            Ok(v) => Some(crate::market::closed(v, "4h", t).into_iter().map(|c| (c.t, c.h, c.l, c.c)).collect()),
            Err(e) => {
                log!("squeeze: {coin} candles not read: {e:#}");
                None
            }
        }
    }

    /// The open trades' losses to their resting stops (twice the close stop's distance), every short's on the
    /// sub-account (the breakout short's too): kept free there.
    fn held_loss(&self) -> f64 {
        crate::sub::held()
    }

    /// On the sub-account: what is free there over the open trades' losses and one trade's risk back to the live account.
    async fn sweep_sub(&self) {
        let Some(m) = &self.sub else { return };
        let keep = self.held_loss() + trades::risk_usd(self.p.risk_usd) * self.p.share;
        if let Err(e) = crate::sub::sweep(&self.k.venue, m.as_ref(), keep).await {
            self.k.event(json!({"what": "error", "coin": "", "error": format!("{e:#}")}));
        }
    }

    /// After a 4h candle's close (its open `bar`): the trades' exits on the close, then the breakouts bought.
    async fn step(&mut self, bar: f64) {
        for pos in std::mem::take(&mut self.state.open) {
            let last = self.bars(&pos.tr.coin, 3).await.and_then(|b| b.last().copied());
            let why = if now() >= pos.tr.exit_at { Some("time") } else if last.is_some_and(|x| (x.3 - pos.close_stop) * self.p.side <= 0.0) { Some("close_stop") } else { None };
            if let Some(why) = why {
                if self.k.exit(&pos.tr, why).await {
                    continue;
                }
            }
            self.state.open.push(pos);
        }
        // the own gate's paper window: the trades closed in the last `own_days` began up to `hold` before
        let look = if self.p.own_days > 0.0 { ((self.p.own_days * DAY + self.p.hold_s) / BAR) as usize + 2 } else { 0 };
        let btc = self.bars("BTC", self.p.btc_ma + 5 + look).await.unwrap_or_default();
        // BTC over its mean at each of its candles (the open time s -> bool), for the paper trades' filter
        let btc_at: std::collections::HashMap<i64, bool> = (self.p.btc_ma.max(1) - 1..btc.len())
            .map(|i| (btc[i].0 as i64, btc_up(&btc[..=i], self.p.btc_ma))).collect();
        if self.p.btc_filter && !btc_up(&btc, self.p.btc_ma) {
            log!("squeeze: BTC under its 50-day mean (or not read): no entries");
        } else if let Ok(ctx) = crate::market::contexts(&self.k.venue.ex, "").await {
            let liquid: Vec<String> = ctx.into_iter()
                .filter(|c| !c.delisted && c.name != "BTC" && self.k.venue.coins.contains_key(&c.name) && c.volume >= self.p.min_vol).map(|c| c.name).collect();
            let (mut cands, mut own, mut papers) = (Vec::new(), 0.0, 0usize);
            let hold = (self.p.hold_s / BAR).round() as usize;
            let btc_ok = |t: f64| !self.p.btc_filter || btc_at.get(&(t as i64)).copied().unwrap_or(false);
            for coin in liquid {
                let held = self.state.open.iter().any(|x| x.tr.coin == coin);
                if held && look == 0 {
                    continue;
                }
                let Some(b) = self.bars(&coin, N_BAND + N_HIST + 5 + look).await else { continue };
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if look > 0 {
                    for (t, r) in paper(&b, self.p.q, self.p.side, self.p.atr_k, hold, look, &btc_ok) {
                        if now() - t <= self.p.own_days * DAY {
                            own += r;
                            papers += 1;
                        }
                    }
                }
                if held || b.last().is_none_or(|x| (x.0 - bar).abs() > 1.0) || !breakout(&b, self.p.q, self.p.side) {
                    continue;
                }
                cands.push((coin, b));
            }
            let own_ok = look == 0 || own < 0.0;
            if self.p.no_weekend && weekend_close(bar) && !cands.is_empty() {
                log!("{}: {} breakout(s) on a candle closing on the weekend: no entries", self.p.kind, cands.len());
                for (coin, b) in std::mem::take(&mut cands) {
                    self.k.event(json!({"what": "weekend", "coin": coin, "px": b[b.len() - 1].3}));
                }
            }
            if look > 0 {
                log!("{}: its paper trades closed in the last {:.0} days: {papers}, {own:+.1} R: new entries {}", self.p.kind, self.p.own_days, if own_ok { "on (under 0)" } else { "off" });
            }
            for (coin, b) in cands {
                if !own_ok {
                    self.k.event(json!({"what": "own_off", "coin": coin, "px": b[b.len() - 1].3, "own_r": own, "papers": papers, "days": self.p.own_days}));
                    continue;
                }
                let Some(a) = atr(&b) else { continue };
                let px = b[b.len() - 1].3;
                let dist = self.p.atr_k * a / px;
                if !(dist > 0.0 && dist < 0.4) {
                    continue;
                }
                let risk = trades::risk_usd(self.p.risk_usd) * self.p.share;
                if self.state.open.len() >= self.p.max_open || !trades::can_open(&coin, self.p.kind, self.p.side, risk) {
                    log!("squeeze: {coin} breaks out but at most open, held or the family's cap");
                    continue;
                }
                if !trades::trial_open(self.p.trial, self.state.opened) {
                    self.k.event(json!({"what": "signal", "coin": coin, "px": px}));
                    continue;
                }
                let size = trades::size_for(risk, dist, px) * self.p.side;
                if let Some(m) = &self.sub {
                    // its margin and its loss to the resting stop, the open ones' to theirs, free there first
                    let notional = size.abs() * px;
                    let need = notional / self.k.venue.leverage_of(&coin) + notional * (2.0 * dist).min(0.8) + self.held_loss();
                    if let Err(e) = crate::sub::fund(&self.k.venue, m.as_ref(), need).await {
                        self.k.event(json!({"what": "error", "coin": coin, "error": format!("{e:#}")}));
                        continue;
                    }
                }
                match trades::open_market(&self.k.venue, &coin, size, (2.0 * dist).min(0.8), None, self.p.hold_s).await {
                    Ok(tr) => {
                        let close_stop = tr.entry_px - self.p.side * self.p.atr_k * a;
                        self.k.event(json!({"what": "open", "coin": coin, "px": tr.entry_px, "size": tr.size, "notional": (tr.size * tr.entry_px).abs(), "atr": a,
                            "close_stop": close_stop, "stop_px": tr.stop_px, "hold_days": self.p.hold_s / DAY, "close": px}));
                        let r = (tr.size * (tr.entry_px - close_stop)).abs();
                        trades::register(&coin, self.p.kind, r, tr.size);
                        self.state.open.push(Pos { tr, close_stop, risk: r });
                        self.state.opened += 1;
                    }
                    Err(e) => self.k.event(json!({"what": "error", "coin": coin, "error": format!("{e:#}")})),
                }
            }
        } else {
            log!("squeeze: contexts not read: no entries");
        }
        self.sweep_sub().await;
        self.state.stepped = bar;
        self.k.save(self.p.kind, &self.state);
    }

    /// The trades swept (see `Keeper::sweep`).
    async fn check(&mut self) {
        if self.k.sweep(&mut self.state.open, |p| &p.tr, false, true).await {
            self.k.save(self.p.kind, &self.state);
            self.sweep_sub().await;
        }
    }

}

/// Starts the squeeze breakout long unless `SQZ_ENABLED` is "0" (unset or "1": live; set empty: the trial), and the
/// breakdown short likewise by `SQZS_ENABLED`.
pub async fn start(cfg: &Config, store: Store) -> Result<()> {
    if !crate::runner::off("squeeze", "SQZ_ENABLED") {
        launch(cfg, store.clone(), Params::from_env()).await?;
    }
    if !crate::runner::off("squeeze_short", "SQZS_ENABLED") {
        launch(cfg, store, Params::short_from_env()).await?;
    }
    Ok(())
}

async fn launch(cfg: &Config, store: Store, p: Params) -> Result<()> {
    let Some((venue, unified)) = crate::runner::venue(p.kind).await? else {
        return Ok(());
    };
    // the short on the shorts' sub-account (`sub`): its own positions; the money moved by the account's own key
    let (venue, unified, sub) = if crate::sub::on_sub(p.kind) {
        let mover = crate::sub::Mover::from_env(venue.ex.base(), &venue.account)?;
        if mover.is_none() {
            log!("{}: MAIN_KEY not set: nothing moves to the short account, a short goes on what is there", p.kind);
        }
        (crate::sub::venue(&venue)?, false, Some(mover))
    } else {
        (venue, unified, None)
    };
    let state: State = crate::store::load_state(cfg, p.kind).await?;
    for pos in &state.open {
        trades::register(&pos.tr.coin, p.kind, pos.risk, pos.tr.size);
    }
    let risk = if trades::family_risk_pct() > 0.0 { format!("{:.1}% депозита", trades::family_risk_pct() * p.share) } else { format!("${:.0}", p.risk_usd * p.share) };
    let short = p.side < 0.0;
    log!("{}: account {}: a 4h close {} the band after a squeeze (the lowest {:.0}% width){}, ${:.0}M+ a day; out {:.0} ATR {} on a 4h close or {:.0} days on; risk {risk}, at most {}; {} held",
        p.kind, venue.account, if short { "under" } else { "over" }, p.q * 100.0, if p.btc_filter { ", BTC over its 50-day mean" } else { "" }, p.min_vol / 1e6, p.atr_k,
        if short { "over" } else { "under" }, p.hold_s / DAY, p.max_open, state.open.len());
    crate::notify::send(format!("🗜 {} НА ДЕНЬГИ: 4ч закрытие {} полосы Боллинджера после сжатия (нижние {:.0}% ширины){}, оборот от ${:.0}M; выход: закрытие 4ч на {:.0} ATR {} входа или {:.0} дн.; риск {risk} на сделку, до {} позиций{}",
        if short { "шорт пробоя вниз из сжатия" } else { "пробой из сжатия" }, if short { "под" } else { "над" }, p.q * 100.0,
        if p.btc_filter { ", BTC выше 50-дн. средней" } else { "" }, p.min_vol / 1e6, p.atr_k, if short { "выше" } else { "ниже" }, p.hold_s / DAY, p.max_open,
        trades::trial_label(p.trial, state.opened)));
    let kind = p.kind;
    let l = Squeeze { k: Keeper::new(venue, unified, cfg.clone(), store, p.kind, message, HeldIn::Days), p, state, sub };
    l.sweep_sub().await;
    crate::runner::spawn(kind, l);
    Ok(())
}

impl crate::runner::Strategy for Squeeze {
    fn schedule(&self) -> crate::runner::Schedule {
        crate::runner::Schedule { tick_s: 20.0, step: crate::runner::Due::Bar { len: BAR, after: AFTER_CLOSE_S }, check_s: CHECK_EVERY_S }
    }

    fn stepped(&self) -> f64 {
        self.state.stepped
    }

    async fn step(&mut self, at: f64) {
        Squeeze::step(self, at).await
    }

    async fn check(&mut self) {
        Squeeze::check(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_quantile_and_the_std_as_pandas() {
        assert!((quantile(&[1.0, 2.0, 3.0, 4.0], 0.2) - 1.6).abs() < 1e-12);
        assert!((std(&[1.0, 2.0, 3.0, 4.0]) - 1.290_994_448_7).abs() < 1e-9);
    }

    #[test]
    fn a_breakout_after_a_squeeze_only() {
        // 600 wide candles (closes alternating 90 / 110), then 300 tight ones at 100: a squeeze; then a close at 103
        let mut c: Vec<f64> = (0..600).map(|i| if i % 2 == 0 { 90.0 } else { 110.0 }).collect();
        c.extend((0..300).map(|i| 100.0 + if i % 2 == 0 { 0.1 } else { -0.1 }));
        let bars = |c: &[f64]| c.iter().enumerate().map(|(i, x)| (i as f64 * BAR, x * 1.001, x * 0.999, *x)).collect::<Vec<Bar>>();
        assert!(!breakout(&bars(&c), 0.2, 1.0) && !breakout(&bars(&c), 0.2, -1.0));
        let mut up = c.clone();
        up.push(103.0);
        assert!(breakout(&bars(&up), 0.2, 1.0) && !breakout(&bars(&up), 0.2, -1.0));
        let mut dn = c.clone();
        dn.push(97.0);                                          // the breakdown: the short's signal
        assert!(breakout(&bars(&dn), 0.2, -1.0) && !breakout(&bars(&dn), 0.2, 1.0));
        // the swings widening: the last width the widest, no squeeze; a close over the band
        let mut wide = (0..900).map(|i| 100.0 + if i % 2 == 0 { -1.0 } else { 1.0 } * (5.0 + i as f64 * 0.02)).collect::<Vec<f64>>();
        wide.push(160.0);
        assert!(!breakout(&bars(&wide), 0.2, 1.0));
        assert!(atr(&bars(&up)).is_some_and(|a| a > 0.0));
    }

    #[test]
    fn the_paper_trades_of_the_own_gate() {
        let mut c: Vec<f64> = (0..600).map(|i| if i % 2 == 0 { 90.0 } else { 110.0 }).collect();
        c.extend((0..300).map(|i| 100.0 + if i % 2 == 0 { 0.1 } else { -0.1 }));
        c.push(103.0);                                          // the breakout
        c.extend([103.5, 104.0, 100.0, 100.0, 100.0]);          // then a fall through the stops
        let b: Vec<Bar> = c.iter().enumerate().map(|(i, x)| (i as f64 * BAR, x * 1.001, x * 0.999, *x)).collect();
        let p = paper(&b, 0.2, 1.0, 3.0, 60, 10, &|_| true);
        assert_eq!(p.len(), 1);
        assert!(p[0].1 <= -1.0, "{p:?}");
        assert!((p[0].0 - (903.0 * BAR + BAR)).abs() < 1.0, "{p:?}");      // out on the candle of the fall
        assert!(paper(&b, 0.2, 1.0, 3.0, 60, 10, &|_| false).is_empty());  // BTC under its mean: no paper trade
        assert!(paper(&b, 0.2, 1.0, 3.0, 60, 3, &|_| true).is_empty());    // the breakout before the window
        // still open at the end: not counted
        let open: Vec<Bar> = b[..903].to_vec();
        assert!(paper(&open, 0.2, 1.0, 3.0, 60, 10, &|_| true).is_empty());
    }
}

#[cfg(test)]
mod lifecycle {
    use super::*;
    use crate::testkit::{FakeHl, cfg, saved};

    async fn setup(coin: &str, p: Params) -> (FakeHl, Squeeze, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        let f = FakeHl::start(&[(coin, 2, 10, 100.0)]).await;
        let (store, rx) = Store::for_test();
        let s = Squeeze { k: Keeper::new(f.venue(), false, cfg(), store, p.kind, message, HeldIn::Days), p, state: State::default(), sub: None };
        (f, s, rx)
    }

    async fn hold(s: &mut Squeeze, coin: &str, size: f64, close_stop: f64) {
        let tr = trades::open_market(&s.k.venue, coin, size, 0.10, None, 5.0 * DAY).await.unwrap();
        trades::register(coin, s.p.kind, tr.risk(), tr.size);
        s.state.open.push(Pos { tr, close_stop, risk: 5.0 });
    }

    #[tokio::test]
    async fn the_resting_stop_filled_ends_it_told_and_saved() {
        let (f, mut s, mut rx) = setup("SQA", Params::from_env()).await;
        hold(&mut s, "SQA", 1.0, 95.0).await;
        f.set_mid("SQA", 89.0);
        s.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!(ev.len(), 1, "{ev:?}");
        let e = &ev[0];
        assert_eq!((e["kind"].as_str(), e["what"].as_str(), e["why"].as_str()), (Some("squeeze"), Some("close"), Some("stop")));
        assert!(e["ret_pct"].as_f64().unwrap() < -9.9 && e["held_days"].as_f64().is_some() && e["entry_px"].as_f64() == Some(100.0));
        assert!(s.state.open.is_empty() && !trades::registered("SQA"));
        assert_eq!(saved(&s.k.cfg, "squeeze").await.unwrap()["open"], json!([]));
    }

    #[tokio::test]
    async fn still_held_it_stays() {
        let (f, mut s, mut rx) = setup("SQB", Params::short_from_env()).await;
        hold(&mut s, "SQB", -1.0, 105.0).await;
        f.set_mid("SQB", 103.0);
        s.check().await;
        assert!(crate::store::drain(&mut rx).is_empty() && s.state.open.len() == 1 && trades::registered("SQB"));
    }

    #[tokio::test]
    async fn a_4h_close_over_the_short_s_stop_ends_it_at_the_step() {
        let (f, mut s, mut rx) = setup("SQC", Params::short_from_env()).await;
        hold(&mut s, "SQC", -1.0, 104.0).await;
        let bar = ((now() - AFTER_CLOSE_S) / BAR).floor() * BAR - BAR;
        f.set_candles("SQC", "4h", &[(bar - BAR, 100.0, 101.0, 99.0, 100.0, 1.0), (bar, 100.0, 105.0, 100.0, 104.5, 1.0)]);
        f.set_mid("SQC", 104.5);
        s.step(bar).await;
        let ev = crate::store::drain(&mut rx);
        let c: Vec<&Value> = ev.iter().filter(|e| e["what"] == "close").collect();
        assert_eq!((c.len(), c[0]["why"].as_str()), (1, Some("close_stop")), "{ev:?}");
        assert!((c[0]["px"].as_f64().unwrap() - 104.5).abs() < 1e-9);
        assert!(f.position("SQC") == 0.0 && f.open_orders("SQC").is_empty() && s.state.open.is_empty() && !trades::registered("SQC"));
        assert_eq!(s.state.stepped, bar);
    }

    /// The short's breakdown on the shorts' sub-account (2026-11-21): the coin long on the live account (the RSI dip) does
    /// not bar it; the margin and the loss to the stop sent there first, the order for the sub-account, the surplus back.
    #[tokio::test]
    async fn the_short_opens_on_the_sub_account_past_a_long_on_the_coin() {
        let f = FakeHl::start(&[("SQE", 2, 10, 97.0)]).await;
        f.set_volume("SQE", 50e6);
        f.with(|x| x.account_value = Some(0.0));
        let (store, mut rx) = Store::for_test();
        let mut p = Params::short_from_env();
        p.no_weekend = false; // the bar is today's; the weekend rule's own test below
        let v = crate::sub::venue(&f.venue()).unwrap();
        let mut s = Squeeze { k: Keeper::new(v, false, cfg(), store, p.kind, message, HeldIn::Days), p, state: State::default(),
            sub: Some(Some(crate::sub::Mover::for_test(&f.base, crate::testkit::ACCOUNT))) };
        trades::register("SQE", "rsidip", 5.0, 1.0);
        assert!(!trades::can_open("SQE", "squeeze", -1.0, 1.0), "the live account: a short against the long, never");
        // 545 wide closes, 300 tight ones at 100, then a close at 97 under the band: the breakdown
        let bar = ((now() - AFTER_CLOSE_S) / BAR).floor() * BAR - BAR;
        let mut c: Vec<f64> = (0..545).map(|i| if i % 2 == 0 { 90.0 } else { 110.0 }).collect();
        c.extend((0..300).map(|i| 100.0 + if i % 2 == 0 { 0.1 } else { -0.1 }));
        c.push(97.0);
        let t0 = bar - (c.len() - 1) as f64 * BAR;
        f.set_candles("SQE", "4h", &c.iter().enumerate().map(|(i, x)| (t0 + i as f64 * BAR, *x, x * 1.001, x * 0.999, *x, 1.0)).collect::<Vec<_>>());
        s.step(bar).await;
        let ev = crate::store::drain(&mut rx);
        assert!(ev.iter().any(|e| e["what"] == "open" && e["coin"] == "SQE"), "{ev:?}");
        assert!(f.position("SQE") < 0.0 && s.state.open.len() == 1);
        let bodies = f.with(|x| x.bodies.clone());
        let send = bodies.iter().position(|b| b["action"]["type"] == "sendAsset" && b["action"]["fromSubAccount"] == "").expect("funded");
        let order = bodies.iter().position(|b| b["action"]["type"] == "order").unwrap();
        assert!(send < order, "the money first");
        assert!(bodies.iter().filter(|b| b["action"]["type"] == "order").all(|b| b["vaultAddress"] == crate::sub::SHORT_ACCOUNT));
        // what it needs sent: the margin at 10x and the loss to the stop twice the 2 ATR away, 5% over
        let tr = &s.state.open[0].tr;
        let n = (tr.size * tr.entry_px).abs();
        let sent: f64 = bodies[send]["action"]["amount"].as_str().unwrap().parse().unwrap();
        assert!(sent >= n / 10.0 + n * (tr.stop_px / tr.entry_px - 1.0) - 1.0 && sent < 1.2 * (n / 10.0 + n * (tr.stop_px / tr.entry_px - 1.0)) + 6.0, "{sent} for {n}");
        // the step's end: nothing over the open loss and one trade's risk to send back (the fake's withdrawable is its value)
        trades::unregister("SQE", "rsidip");
    }

    #[tokio::test]
    async fn its_time_up_it_ends_at_the_step_and_a_close_inside_keeps_it() {
        let (f, mut s, mut rx) = setup("SQD", Params::from_env()).await;
        hold(&mut s, "SQD", 1.0, 95.0).await;
        let bar = ((now() - AFTER_CLOSE_S) / BAR).floor() * BAR - BAR;
        f.set_candles("SQD", "4h", &[(bar, 100.0, 101.0, 99.0, 100.5, 1.0)]);
        s.step(bar).await;
        assert!(crate::store::drain(&mut rx).iter().all(|e| e["what"] != "close"), "a close over the stop: kept");
        assert_eq!(s.state.open.len(), 1);
        s.state.open[0].tr.exit_at = now() - 1.0;
        f.set_mid("SQD", 101.0);
        s.step(bar + BAR).await;
        let ev = crate::store::drain(&mut rx);
        let c: Vec<&Value> = ev.iter().filter(|e| e["what"] == "close").collect();
        assert_eq!((c.len(), c[0]["why"].as_str()), (1, Some("time")), "{ev:?}");
        assert!(s.state.open.is_empty() && f.position("SQD") == 0.0);
    }
}

#[cfg(test)]
mod candles {
    use super::*;
    use crate::testkit::{FakeHl, cfg};

    #[tokio::test]
    async fn the_weekend_rule_and_the_floors() {
        // 2026-11-21 was a Saturday: the candle 16:00-20:00 Friday 11-20 closes on Friday, 20:00-24:00 on Saturday 00:00
        let fri = 1_795_132_800.0; // 2026-11-20 00:00 UTC, a Friday
        assert!(!weekend_close(fri + 16.0 * 3600.0) && weekend_close(fri + 20.0 * 3600.0));
        assert!(weekend_close(fri + 2.0 * 86_400.0 + 16.0 * 3600.0) && !weekend_close(fri + 2.0 * 86_400.0 + 20.0 * 3600.0), "Sunday's last candle closes on Monday");
        let (l, s) = (Params::from_env(), Params::short_from_env());
        assert!(l.no_weekend && s.no_weekend);
        assert_eq!((l.min_vol, s.min_vol), (10e6, 5e6));
        // a breakdown on a candle closing on the weekend: told, not traded
        let f = FakeHl::start(&[("SQW", 2, 10, 97.0)]).await;
        f.set_volume("SQW", 50e6);
        let (store, mut rx) = Store::for_test();
        let mut s = Squeeze { k: Keeper::new(f.venue(), false, cfg(), store, s.kind, message, HeldIn::Days), p: s, state: State::default(), sub: None };
        let bar = fri + 20.0 * 3600.0;
        let mut c: Vec<f64> = (0..545).map(|i| if i % 2 == 0 { 90.0 } else { 110.0 }).collect();
        c.extend((0..300).map(|i| 100.0 + if i % 2 == 0 { 0.1 } else { -0.1 }));
        c.push(97.0);
        let t0 = bar - (c.len() - 1) as f64 * BAR;
        f.set_candles("SQW", "4h", &c.iter().enumerate().map(|(i, x)| (t0 + i as f64 * BAR, *x, x * 1.001, x * 0.999, *x, 1.0)).collect::<Vec<_>>());
        s.step(bar).await;
        let ev = crate::store::drain(&mut rx);
        assert!(ev.iter().any(|e| e["what"] == "weekend" && e["coin"] == "SQW"), "{ev:?}");
        assert!(!ev.iter().any(|e| e["what"] == "open") && s.state.open.is_empty() && f.position("SQW") == 0.0);
    }

    #[tokio::test]
    async fn the_closed_4h_candles_only() {
        let f = FakeHl::start(&[("SCA", 2, 10, 100.0)]).await;
        let (store, _) = Store::for_test();
        let p = Params::from_env();
        let s = Squeeze { k: Keeper::new(f.venue(), false, cfg(), store, p.kind, message, HeldIn::Days), p, state: State::default(), sub: None };
        let b = (now() / BAR).floor() * BAR;
        f.set_candles("SCA", "4h", &[(b - BAR, 1.0, 2.0, 0.5, 1.5, 1.0), (b, 1.5, 3.0, 1.0, 2.5, 1.0)]);
        assert_eq!(s.bars("SCA", 5).await.unwrap(), vec![(b - BAR, 2.0, 0.5, 1.5)]);
    }
}
