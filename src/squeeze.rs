//! The 4h squeeze breakout long (`SQZ_ENABLED=1`, with `LIVE_KEY` / `LIVE_ACCOUNT` / `LIVE_NET` as for the dip):
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
//! The SHORT side (`SQZS_ENABLED`: unset off; "1" live; set empty the trial of 3; else off): the mirror, a 4h close under the lower
//! band after a squeeze (the lowest `SQZS_QUANTILE` (0.1) width), any BTC; out on a 4h close `SQZS_ATR_K` (2) ATR over the
//! entry or `SQZS_HOLD_DAYS` (5) on; `SQZS_RISK_SHARE` (0.5) of the family's risk; at most `SQZS_MAX_OPEN` (3). V144 / V145
//! (Binance 2020-26): +0.32 R a trade vs -0.04 on random bars, 6 of 7 years (2022 +128 R), +0.36 R on the untouched
//! 2023-26; a candle late +0.27, costs x2 +0.29, w/o the 10 best coins +0.24; the daily P&L corr with the live +0.03, the
//! trend -0.04, the long squeeze -0.01; in the portfolio at half the risk up in all four spans (Binance 2020-23 / 2023-26,
//! HL halves: +12 / +328 / +12 / +4% at the same drawdown) and the Binance drawdown at 2% 48% -> 37%: a hedge of the longs.
//! On HL alone flat so far (731 trades 2025-26, -0.02 R).
//!
//! The states: the store's docs `squeeze` and `squeeze_short`.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{now, num};
use crate::config::Config;
use crate::live::Venue;
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
    /// `SQZ_RISK_SHARE` (1): the share of the family's risk a trade (V138: at 2% each the whole HL portfolio's
    /// drawdown 10% -> 19%; 0.5 halves it).
    share: f64,
    max_open: usize,
    /// The trial (`SQZ_ENABLED` unset): its live trades at most.
    trial: Option<u32>,
    /// +1 the breakout long, -1 the breakdown short.
    side: f64,
    /// The registry's and the events' kind, the state's doc.
    kind: &'static str,
    /// Only while BTC is over its mean (the long).
    btc_filter: bool,
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { q: v("SQZ_QUANTILE", 0.2), atr_k: v("SQZ_ATR_K", 3.0), hold_s: v("SQZ_HOLD_DAYS", 10.0) * DAY, btc_ma: (v("SQZ_BTC_MA_DAYS", 50.0) * 6.0) as usize,
            min_vol: v("SQZ_MIN_VOL_USD", 10e6), risk_usd: v("SQZ_RISK_USD", 20.0), share: v("SQZ_RISK_SHARE", 1.0), max_open: v("SQZ_MAX_OPEN", 3.0) as usize,
            trial: trades::switch("SQZ_ENABLED").flatten(), side: 1.0, kind: "squeeze", btc_filter: true }
    }

    fn short_from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { q: v("SQZS_QUANTILE", 0.1), atr_k: v("SQZS_ATR_K", 2.0), hold_s: v("SQZS_HOLD_DAYS", 5.0) * DAY, btc_ma: (v("SQZ_BTC_MA_DAYS", 50.0) * 6.0) as usize,
            min_vol: v("SQZS_MIN_VOL_USD", 10e6), risk_usd: v("SQZS_RISK_USD", 20.0), share: v("SQZS_RISK_SHARE", 0.5), max_open: v("SQZS_MAX_OPEN", 3.0) as usize,
            trial: trades::switch("SQZS_ENABLED").flatten(), side: -1.0, kind: "squeeze_short", btc_filter: false }
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
    let k = c.len();
    if k < N_BAND + N_HIST_MIN + 1 {
        return false;
    }
    let w = widths(&c);
    let prev = k - 2;
    let hist: Vec<f64> = w[prev + 1 - N_HIST.min(prev + 1)..=prev].iter().flatten().copied().collect();
    let Some(wp) = w[prev] else { return false };
    if hist.len() < N_HIST_MIN || wp > quantile(&hist, q) {
        return false;
    }
    let band = &c[k - N_BAND..];
    if side > 0.0 { c[k - 1] > mean(band) + 2.0 * std(band) } else { c[k - 1] < mean(band) - 2.0 * std(band) }
}

/// Whether BTC's last close is over the mean of its last `n` closes.
fn btc_up(b: &[Bar], n: usize) -> bool {
    b.len() >= n && b[b.len() - 1].3 > mean(&b[b.len() - n..].iter().map(|x| x.3).collect::<Vec<_>>())
}

fn usd(x: f64) -> String {
    if x >= 0.0 { format!("+${x:.2}") } else { format!("-${:.2}", -x) }
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
            match ev["why"].as_str().unwrap_or("") { "stop" => "АВАРИЙНЫЙ СТОП", "close_stop" => "СТОП по закрытию 4ч", _ => "ЗАКРЫЛ по времени" },
            f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_usd")), f("held_days")),
        "error" => format!("⚠️ {nm} {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        "signal" => format!("🗜 сигнал: {nm} {coin} по {} (проба окончена, без сделки)", f("px")),
        _ => return None,
    })
}

struct Squeeze {
    venue: Venue,
    unified: bool,
    cfg: Config,
    store: Store,
    p: Params,
    state: State,
}

impl Squeeze {
    fn event(&self, mut ev: Value) {
        ev["kind"] = json!(self.p.kind);
        ev["at"] = json!(now());
        log!("{}: {ev}", self.p.kind);
        if let Some(m) = message(&ev) {
            crate::notify::send(m);
        }
        self.store.event(ev);
    }

    fn save(&self) {
        let (cfg, v, doc) = (self.cfg.clone(), serde_json::to_value(&self.state).unwrap_or_default(), self.p.kind);
        tokio::spawn(async move {
            if let Err(e) = crate::store::save_doc(&cfg, doc, &v).await {
                log!("squeeze: state not saved: {e:#}");
            }
        });
    }

    /// A coin's closed 4h candles over the last `n` (the forming one left out).
    async fn bars(&self, coin: &str, n: usize) -> Option<Vec<Bar>> {
        let t = now();
        let r = self.venue.ex.info(json!({"type": "candleSnapshot", "req": {"coin": coin, "interval": "4h",
            "startTime": ((t - (n as f64 + 2.0) * BAR) * 1000.0) as u64, "endTime": (t * 1000.0) as u64}})).await;
        let r = match r {
            Ok(v) => v,
            Err(e) => {
                log!("squeeze: {coin} candles not read: {e:#}");
                return None;
            }
        };
        Some(r.as_array()?.iter().map(|c| (num(&c["t"]) / 1000.0, num(&c["h"]), num(&c["l"]), num(&c["c"])))
            .filter(|x| x.0 + BAR <= t && x.3 > 0.0).collect())
    }

    async fn exit(&mut self, pos: &Pos, why: &str) -> bool {
        match trades::close(&self.venue, &pos.tr).await {
            Ok(px) => {
                let ret = pos.tr.ret(px);
                self.event(json!({"what": "close", "coin": pos.tr.coin, "why": why, "entry_px": pos.tr.entry_px, "px": px, "ret_pct": ret * 100.0,
                    "pnl_usd": ret * pos.tr.size.abs() * pos.tr.entry_px, "held_days": (now() - pos.tr.at) / DAY}));
                trades::unregister(&pos.tr.coin, self.p.kind);
                true
            }
            Err(e) => {
                self.event(json!({"what": "error", "coin": pos.tr.coin, "error": format!("exit: {e:#}")}));
                false
            }
        }
    }

    /// After a 4h candle's close (its open `bar`): the trades' exits on the close, then the breakouts bought.
    async fn step(&mut self, bar: f64) {
        for pos in std::mem::take(&mut self.state.open) {
            let last = self.bars(&pos.tr.coin, 3).await.and_then(|b| b.last().copied());
            let why = if now() >= pos.tr.exit_at { Some("time") } else if last.is_some_and(|x| (x.3 - pos.close_stop) * self.p.side <= 0.0) { Some("close_stop") } else { None };
            if let Some(why) = why {
                if self.exit(&pos, why).await {
                    continue;
                }
            }
            self.state.open.push(pos);
        }
        let btc = self.bars("BTC", self.p.btc_ma + 5).await.unwrap_or_default();
        if self.p.btc_filter && !btc_up(&btc, self.p.btc_ma) {
            log!("squeeze: BTC under its 50-day mean (or not read): no entries");
        } else if let Ok(ctx) = self.venue.ex.info(json!({"type": "metaAndAssetCtxs"})).await {
            let u = ctx[0]["universe"].as_array().cloned().unwrap_or_default();
            let c = ctx[1].as_array().cloned().unwrap_or_default();
            let liquid: Vec<String> = u.iter().zip(c.iter()).filter_map(|(u, c)| {
                let name = u["name"].as_str()?;
                (u["isDelisted"] != true && name != "BTC" && self.venue.coins.contains_key(name) && num(&c["dayNtlVlm"]) >= self.p.min_vol).then(|| name.to_string())
            }).collect();
            for coin in liquid {
                if self.state.open.iter().any(|x| x.tr.coin == coin) {
                    continue;
                }
                let Some(b) = self.bars(&coin, N_BAND + N_HIST + 5).await else { continue };
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                if b.last().is_none_or(|x| (x.0 - bar).abs() > 1.0) || !breakout(&b, self.p.q, self.p.side) {
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
                    self.event(json!({"what": "signal", "coin": coin, "px": px}));
                    continue;
                }
                let size = trades::size_for(risk, dist, px) * self.p.side;
                match trades::open_market(&self.venue, &coin, size, (2.0 * dist).min(0.8), None, self.p.hold_s).await {
                    Ok(tr) => {
                        let close_stop = tr.entry_px - self.p.side * self.p.atr_k * a;
                        self.event(json!({"what": "open", "coin": coin, "px": tr.entry_px, "size": tr.size, "notional": (tr.size * tr.entry_px).abs(), "atr": a,
                            "close_stop": close_stop, "stop_px": tr.stop_px, "hold_days": self.p.hold_s / DAY, "close": px}));
                        let r = (tr.size * (tr.entry_px - close_stop)).abs();
                        trades::register(&coin, self.p.kind, r, tr.size);
                        self.state.open.push(Pos { tr, close_stop, risk: r });
                        self.state.opened += 1;
                    }
                    Err(e) => self.event(json!({"what": "error", "coin": coin, "error": format!("{e:#}")})),
                }
            }
        } else {
            log!("squeeze: contexts not read: no entries");
        }
        self.state.stepped = bar;
        self.save();
    }

    /// Between the closes: a trade whose resting stop filled (a crash) ends.
    async fn check(&mut self) {
        if self.state.open.is_empty() {
            return;
        }
        let held = match crate::live::equity_of(&self.venue.ex, &self.venue.account, self.unified).await {
            Ok((eq, p)) => {
                trades::set_equity(eq);
                p
            }
            Err(e) => {
                log!("squeeze: account not read: {e:#}");
                return;
            }
        };
        let mut keep = Vec::new();
        let mut changed = false;
        for pos in std::mem::take(&mut self.state.open) {
            match trades::gone(&self.venue, &pos.tr, held.get(&pos.tr.coin).copied().unwrap_or(0.0), self.p.kind).await {
                Some((why, px)) => {
                    let ret = pos.tr.ret(px);
                    self.event(json!({"what": "close", "coin": pos.tr.coin, "why": why, "entry_px": pos.tr.entry_px, "px": px, "ret_pct": ret * 100.0,
                        "pnl_usd": ret * pos.tr.size.abs() * pos.tr.entry_px, "held_days": (now() - pos.tr.at) / DAY}));
                    trades::unregister(&pos.tr.coin, self.p.kind);
                    changed = true;
                }
                None => keep.push(pos),
            }
        }
        self.state.open = keep;
        if changed {
            self.save();
        }
    }
}

/// Starts the squeeze breakout long if `SQZ_ENABLED` is "1" (unset: the trial), and the breakdown short if `SQZS_ENABLED`
/// is set (unset: off; see the module).
pub async fn start(cfg: &Config, store: Store) -> Result<()> {
    if trades::switch("SQZ_ENABLED").is_none() {
        log!("squeeze: off (SQZ_ENABLED=0)");
    } else {
        launch(cfg, store.clone(), Params::from_env()).await?;
    }
    if std::env::var("SQZS_ENABLED").is_err() || trades::switch("SQZS_ENABLED").is_none() {
        log!("squeeze_short: off (SQZS_ENABLED unset or 0)");
    } else {
        launch(cfg, store, Params::short_from_env()).await?;
    }
    Ok(())
}

async fn launch(cfg: &Config, store: Store, p: Params) -> Result<()> {
    let Some((venue, unified)) = crate::live::venue_from_env(None).await? else {
        log!("{}: LIVE_KEY / LIVE_ACCOUNT not set, off", p.kind);
        return Ok(());
    };
    let state: State = crate::store::load_doc(cfg, p.kind).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
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
    let mut l = Squeeze { venue, unified, cfg: cfg.clone(), store, p, state };
    tokio::spawn(async move {
        let mut check_at = 0.0;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
        loop {
            tick.tick().await;
            let t = now();
            let bar = ((t - AFTER_CLOSE_S) / BAR).floor() * BAR - BAR;
            if bar > l.state.stepped {
                l.step(bar).await;
            }
            if t - check_at >= CHECK_EVERY_S {
                l.check().await;
                check_at = t;
            }
        }
    });
    Ok(())
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
}
