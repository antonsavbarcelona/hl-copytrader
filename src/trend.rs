//! The daily trend long (`TREND_ENABLED`: unset or "1" live, "0" off; with `LIVE_KEY` / `LIVE_ACCOUNT` / `LIVE_NET` as for the dip): after each
//! UTC day's close, a main-dex perp whose close broke the high of the `TREND_DAYS` (20) days before it, with
//! `TREND_MIN_VOL_USD` (10M) or more traded that day, is bought at market while BTC's close is over its
//! `TREND_BTC_MA_DAYS` (50) day mean. Out at market when a day closes `TREND_ATR_K` (3) x ATR20 (the day before's)
//! under the best close since the entry (a chandelier on the closes: no intraday wick-out), or `TREND_MAX_DAYS`
//! (90) on. The size: `TREND_RISK_USD` ($20; `FAMILY_RISK_PCT` of the equity when set) lost at the chandelier's
//! first distance; a resting stop twice that far under the entry guards a crash between the daily checks.
//! At most `TREND_MAX_OPEN` (3) and the family's cap; one a coin.
//!
//! The research (2026-10-31, V120 / V121b, Binance 1m 2020-26, the HL-listed coins): +0.66 R a trade against
//! +0.03 on random days; the daily P&L's correlation with the live strategies +0.08, 1% of its entries on a coin a
//! live trade holds; with the live set at the same drawdown 18%: x12 -> x33, both halves. By year: 2020 +310 R,
//! 2021 +264, 2022 -103, 2023 +437, 2024 +264, 2025 -24, 2026 +17: flat in the alts' bear / chop (the HL span
//! 2025-05 .. was flat on Binance too, -1.1% a trade): a trend follower's years.
//!
//! Its state is the store's doc `trend` (the trades, each with its entry day: the chandelier is replayed from the
//! daily candles on each check, so a restart loses nothing).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{now, num};
use crate::config::Config;
use crate::live::Venue;
use crate::log;
use crate::store::Store;
use crate::trades::{self, Trade};

const DOC: &str = "trend";
const DAY: f64 = 86_400.0;
/// The daily step this long after the UTC day's close (the day's candle final).
const AFTER_CLOSE_S: f64 = 300.0;
const CHECK_EVERY_S: f64 = 300.0;
const ATR_DAYS: usize = 20;

#[derive(Clone, Debug)]
struct Params {
    days: usize,
    atr_k: f64,
    max_days: f64,
    btc_ma: usize,
    min_vol: f64,
    risk_usd: f64,
    /// `TREND_RISK_SHARE` (1): the share of the family's risk a trade (V138: at 2% each the whole HL portfolio's
    /// drawdown 10% -> 17%, the trend flat in the 2025-26 chop; 0.5 halves it).
    share: f64,
    max_open: usize,
    /// The trial (`TREND_ENABLED` set empty): its live trades at most.
    trial: Option<u32>,
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { days: v("TREND_DAYS", 20.0) as usize, atr_k: v("TREND_ATR_K", 3.0), max_days: v("TREND_MAX_DAYS", 90.0),
            btc_ma: v("TREND_BTC_MA_DAYS", 50.0) as usize, min_vol: v("TREND_MIN_VOL_USD", 10e6), risk_usd: v("TREND_RISK_USD", 20.0), share: v("TREND_RISK_SHARE", 1.0),
            max_open: v("TREND_MAX_OPEN", 3.0) as usize, trial: trades::switch_on("TREND_ENABLED").flatten() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pos {
    tr: Trade,
    /// The entry day's candle open (s): the chandelier is replayed from the day after it.
    day: f64,
    /// The $ lost at the chandelier's first distance (the family's risk).
    risk: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    open: Vec<Pos>,
    /// The last daily step done (the day's candle open, s).
    #[serde(default)]
    stepped: f64,
    /// The live trades opened (the trial's count).
    #[serde(default)]
    opened: u32,
}

/// A coin's closed daily candles: (open time s, high, low, close, $ volume).
type Day = (f64, f64, f64, f64, f64);

/// The ATR (a simple mean of the true ranges over `ATR_DAYS`) at each day; None before enough days.
fn atr(d: &[Day]) -> Vec<Option<f64>> {
    let tr: Vec<f64> = d.iter().enumerate().map(|(i, x)| {
        let pc = if i > 0 { d[i - 1].3 } else { x.3 };
        (x.1 - x.2).max((x.1 - pc).abs()).max((x.2 - pc).abs())
    }).collect();
    (0..d.len()).map(|i| (i + 1 >= ATR_DAYS).then(|| tr[i + 1 - ATR_DAYS..=i].iter().sum::<f64>() / ATR_DAYS as f64)).collect()
}

/// Whether the last day breaks out: its close over the high of the `n` days before it, its $ volume >= `min_vol`.
fn breakout(d: &[Day], n: usize, min_vol: f64) -> bool {
    let k = d.len();
    k > n && d[k - 1].4 >= min_vol && d[k - 1].3 > d[k - 1 - n..k - 1].iter().map(|x| x.1).fold(f64::MIN, f64::max)
}

/// Whether BTC's last close is over the mean of its last `n` closes.
fn btc_up(d: &[Day], n: usize) -> bool {
    d.len() >= n && d[d.len() - 1].3 > d[d.len() - n..].iter().map(|x| x.3).sum::<f64>() / n as f64
}

/// The chandelier from the entry day (`day`, its open s) to the last closed day: the day the close fell to
/// the best close since the entry minus `k` x the day before's ATR (Some(its close)), or None (still in).
fn chandelier_out(d: &[Day], day: f64, k: f64) -> Option<f64> {
    let a = atr(d);
    let i0 = d.iter().position(|x| (x.0 - day).abs() < 1.0)?;
    let mut best = d[i0].3;
    for j in i0 + 1..d.len() {
        if let Some(at) = a[j - 1] {
            if d[j].3 <= best - k * at {
                return Some(d[j].3);
            }
        }
        best = best.max(d[j].3);
    }
    None
}

fn usd(x: f64) -> String {
    if x >= 0.0 { format!("+${x:.2}") } else { format!("-${:.2}", -x) }
}

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    Some(match ev["what"].as_str()? {
        "open" => format!("📈 ТРЕНД ЛОНГ {coin}: пробой {:.0}-дн. максимума, цена {}, на ${:.0}, выход: закрытие дня на {:.0} ATR ниже лучшего (сейчас ~{}), аварийный стоп {}",
            f("days"), f("px"), f("notional"), f("atr_k"), f("chandelier_px"), f("stop_px")),
        "close" => format!("{} тренд лонг {} {coin}: {} -> {} ({:+.2}%, {}, {:.0} дн.)", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            match ev["why"].as_str().unwrap_or("") { "stop" => "АВАРИЙНЫЙ СТОП", "trail" => "выход по трейлу", _ => "ЗАКРЫЛ по времени" },
            f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_usd")), f("held_days")),
        "error" => format!("⚠️ тренд лонг {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        "signal" => format!("📈 сигнал тренд лонга {coin} по {} (проба окончена, без сделки)", f("px")),
        _ => return None,
    })
}

struct TrendLong {
    venue: Venue,
    unified: bool,
    cfg: Config,
    store: Store,
    p: Params,
    state: State,
}

impl TrendLong {
    fn event(&self, mut ev: Value) {
        ev["kind"] = json!("trend");
        ev["at"] = json!(now());
        log!("trend: {ev}");
        if let Some(m) = message(&ev) {
            crate::notify::send(m);
        }
        self.store.event(ev);
    }

    fn save(&self) {
        let (cfg, v) = (self.cfg.clone(), serde_json::to_value(&self.state).unwrap_or_default());
        tokio::spawn(async move {
            if let Err(e) = crate::store::save_doc(&cfg, DOC, &v).await {
                log!("trend: state not saved: {e:#}");
            }
        });
    }

    /// A coin's closed daily candles over the last `n` days (the forming one left out).
    async fn days(&self, coin: &str, n: usize) -> Option<Vec<Day>> {
        let t = now();
        let r = self.venue.ex.info(json!({"type": "candleSnapshot", "req": {"coin": coin, "interval": "1d",
            "startTime": ((t - (n as f64 + 2.0) * DAY) * 1000.0) as u64, "endTime": (t * 1000.0) as u64}})).await;
        let r = match r {
            Ok(v) => v,
            Err(e) => {
                log!("trend: {coin} candles not read: {e:#}");
                return None;
            }
        };
        let d: Vec<Day> = r.as_array()?.iter().map(|c| (num(&c["t"]) / 1000.0, num(&c["h"]), num(&c["l"]), num(&c["c"]), num(&c["v"]) * num(&c["c"])))
            .filter(|x| x.0 + DAY <= t && x.3 > 0.0).collect();
        Some(d)
    }

    /// After a UTC day's close: the trades' chandelier exits, then the day's breakouts bought.
    async fn step(&mut self, day: f64) {
        let mut changed = false;
        // The exits: the chandelier on the closes, the time.
        let hist = (self.p.max_days as usize) + ATR_DAYS + 5;
        for pos in std::mem::take(&mut self.state.open) {
            let d = self.days(&pos.tr.coin, hist).await;
            let why = match &d {
                Some(d) => chandelier_out(d, pos.day, self.p.atr_k).map(|_| "trail"),
                None => None,
            }.or_else(|| (now() >= pos.tr.exit_at).then_some("time"));
            match why {
                Some(why) => match trades::close(&self.venue, &pos.tr).await {
                    Ok(px) => {
                        let ret = pos.tr.ret(px);
                        self.event(json!({"what": "close", "coin": pos.tr.coin, "why": why, "entry_px": pos.tr.entry_px, "px": px, "ret_pct": ret * 100.0,
                            "pnl_usd": ret * pos.tr.size.abs() * pos.tr.entry_px, "held_days": (now() - pos.tr.at) / DAY}));
                        trades::unregister(&pos.tr.coin, "trend");
                        changed = true;
                    }
                    Err(e) => {
                        self.event(json!({"what": "error", "coin": pos.tr.coin, "error": format!("exit: {e:#}")}));
                        self.state.open.push(pos);
                    }
                },
                None => self.state.open.push(pos),
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        // The entries: BTC over its mean, then each liquid main-dex perp's last closed day.
        let n = self.p.days.max(ATR_DAYS) + 5;
        let btc = self.days("BTC", self.p.btc_ma + 5).await.unwrap_or_default();
        if !btc_up(&btc, self.p.btc_ma) {
            log!("trend: BTC under its {}-day mean (or not read): no entries today", self.p.btc_ma);
        } else {
            let ctx = self.venue.ex.info(json!({"type": "metaAndAssetCtxs"})).await;
            let mut liquid = Vec::new();
            if let Ok(ctx) = ctx {
                let u = ctx[0]["universe"].as_array().cloned().unwrap_or_default();
                let c = ctx[1].as_array().cloned().unwrap_or_default();
                for (u, c) in u.iter().zip(c.iter()) {
                    let Some(name) = u["name"].as_str() else { continue };
                    if u["isDelisted"] != true && name != "BTC" && self.venue.coins.contains_key(name) && num(&c["dayNtlVlm"]) >= self.p.min_vol * 0.5 {
                        liquid.push(name.to_string());
                    }
                }
            } else {
                log!("trend: contexts not read: no entries today");
            }
            for coin in liquid {
                if self.state.open.iter().any(|x| x.tr.coin == coin) {
                    continue;
                }
                let Some(d) = self.days(&coin, n).await else { continue };
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                if d.last().is_none_or(|x| (x.0 - day).abs() > 1.0) || !breakout(&d, self.p.days, self.p.min_vol) {
                    continue;
                }
                let Some(Some(a)) = atr(&d).last().copied() else { continue };
                let px = d[d.len() - 1].3;
                let dist = self.p.atr_k * a / px;
                if !(dist > 0.0 && dist < 0.5) {
                    continue;
                }
                let risk = trades::risk_usd(self.p.risk_usd) * self.p.share;
                if self.state.open.len() >= self.p.max_open || !trades::can_open(&coin, "trend", 1.0, risk) {
                    log!("trend: {coin} breaks out but at most open, held or the family's cap");
                    continue;
                }
                if !trades::trial_open(self.p.trial, self.state.opened) {
                    self.event(json!({"what": "signal", "coin": coin, "px": px}));
                    continue;
                }
                let size = trades::size_for(risk, dist, px);
                match trades::open_market(&self.venue, &coin, size, (2.0 * dist).min(0.9), None, self.p.max_days * DAY).await {
                    Ok(tr) => {
                        self.event(json!({"what": "open", "coin": coin, "days": self.p.days, "px": tr.entry_px, "size": tr.size, "notional": tr.size * tr.entry_px,
                            "atr": a, "atr_k": self.p.atr_k, "chandelier_px": px - self.p.atr_k * a, "stop_px": tr.stop_px, "close": px}));
                        let r = (tr.size * self.p.atr_k * a).abs();
                        trades::register(&coin, "trend", r, tr.size);
                        self.state.open.push(Pos { tr, day, risk: r });
                        self.state.opened += 1;
                        changed = true;
                    }
                    Err(e) => self.event(json!({"what": "error", "coin": coin, "error": format!("{e:#}")})),
                }
            }
        }
        self.state.stepped = day;
        if changed {
            self.save();
        } else {
            let (cfg, v) = (self.cfg.clone(), serde_json::to_value(&self.state).unwrap_or_default());
            tokio::spawn(async move { let _ = crate::store::save_doc(&cfg, DOC, &v).await; });
        }
    }

    /// Between the daily steps: a trade whose resting stop filled (a crash) ends.
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
                log!("trend: account not read: {e:#}");
                return;
            }
        };
        let mut keep = Vec::new();
        let mut changed = false;
        for pos in std::mem::take(&mut self.state.open) {
            match trades::gone(&self.venue, &pos.tr, held.get(&pos.tr.coin).copied().unwrap_or(0.0), "trend").await {
                Some((why, px)) => {
                    let ret = pos.tr.ret(px);
                    self.event(json!({"what": "close", "coin": pos.tr.coin, "why": why, "entry_px": pos.tr.entry_px, "px": px, "ret_pct": ret * 100.0,
                        "pnl_usd": ret * pos.tr.size.abs() * pos.tr.entry_px, "held_days": (now() - pos.tr.at) / DAY}));
                    trades::unregister(&pos.tr.coin, "trend");
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

/// Starts the trend long unless `TREND_ENABLED` is "0" (unset or "1": live; set empty: the trial).
pub async fn start(cfg: &Config, store: Store) -> Result<()> {
    if trades::switch_on("TREND_ENABLED").is_none() {
        log!("trend: off (TREND_ENABLED=0)");
        return Ok(());
    }
    let Some((venue, unified)) = crate::live::venue_from_env(None).await? else {
        log!("trend: LIVE_KEY / LIVE_ACCOUNT not set, off");
        return Ok(());
    };
    let p = Params::from_env();
    let state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    for pos in &state.open {
        trades::register(&pos.tr.coin, "trend", pos.risk, pos.tr.size);
    }
    log!("trend: account {}: the close over the {}-day high (${:.0}M+ that day), BTC over its {}-day mean; out {:.0} ATR under the best close or {:.0} days on; risk {}, at most {}; {} held",
        venue.account, p.days, p.min_vol / 1e6, p.btc_ma, p.atr_k, p.max_days, trades::sizing_label(p.risk_usd, 0.1), p.max_open, state.open.len());
    crate::notify::send(format!("📈 тренд лонг НА ДЕНЬГИ: пробой {}-дн. максимума (оборот от ${:.0}M), BTC выше {}-дн. средней; выход: закрытие дня на {:.0} ATR ниже лучшего или {:.0} дн.; риск {} на сделку, до {} позиций{}",
        p.days, p.min_vol / 1e6, p.btc_ma, p.atr_k, p.max_days, if trades::family_risk_pct() > 0.0 { format!("{:.1}% депозита", trades::family_risk_pct()) } else { format!("${:.0}", p.risk_usd) },
        p.max_open, trades::trial_label(p.trial, state.opened)));
    let mut l = TrendLong { venue, unified, cfg: cfg.clone(), store, p, state };
    tokio::spawn(async move {
        let mut check_at = 0.0;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            let t = now();
            // The last closed day's open: the step once a day, AFTER_CLOSE_S after the UTC midnight.
            let day = ((t - AFTER_CLOSE_S) / DAY).floor() * DAY - DAY;
            if day > l.state.stepped {
                l.step(day).await;
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

    fn days(closes: &[f64]) -> Vec<Day> {
        closes.iter().enumerate().map(|(i, c)| (i as f64 * DAY, c * 1.01, c * 0.99, *c, 20e6)).collect()
    }

    #[test]
    fn a_breakout_over_the_days_before() {
        let mut c: Vec<f64> = (0..25).map(|_| 100.0).collect();
        assert!(!breakout(&days(&c), 20, 10e6));
        c.push(103.0);                                         // over the 101 highs
        assert!(breakout(&days(&c), 20, 10e6));
        assert!(!breakout(&days(&c), 20, 50e6));                // thin
        assert!(btc_up(&days(&c), 20) && !btc_up(&days(&[100.0, 90.0]), 2));
    }

    #[test]
    fn the_chandelier_on_the_closes() {
        // 25 flat days (ATR ~2), the entry at 100 on day 24, a run to 120, then a close 3 ATR under 120: out
        let mut c: Vec<f64> = (0..25).map(|_| 100.0).collect();
        c.extend([110.0, 120.0, 118.0]);
        let d = days(&c);
        assert_eq!(chandelier_out(&d, 24.0 * DAY, 3.0), None);
        c.push(100.0);
        assert_eq!(chandelier_out(&days(&c), 24.0 * DAY, 3.0), Some(100.0));
        let m = message(&json!({"what": "close", "coin": "SOL", "why": "trail", "entry_px": 100.0, "px": 130.0, "ret_pct": 30.0, "pnl_usd": 60.0, "held_days": 20.0})).unwrap();
        assert!(m.contains("трейлу") && m.contains("+$60.00"));
    }
}
