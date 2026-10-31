//! The listing short (`LIST_ENABLED=1`, with `LIVE_KEY` / `LIVE_ACCOUNT` / `LIVE_NET` as for the
//! dip): a perp newly listed on the main dex (its first daily candle), `LIST_DELAY_DAYS` (7) on,
//! with `LIST_MIN_VOL_USD` (5M) or more a day, is shorted at market; a resting stop `LIST_STOP_PCT`
//! (25) over, a resting take `LIST_TAKE_PCT` (30; 20 until 2026-10-27) under, out at market `LIST_HOLD_DAYS` (14) later;
//! the stop losing `LIST_RISK_USD` ($20; `FAMILY_RISK_PCT` of the equity when set); at most
//! `LIST_MAX_OPEN` (3) and the family's cap; each listing once. Coins listed before the first look
//! (older than 60 days) are never shorted.
//!
//! The archive research of 2026-10-25 (37 listings): the listings fell +5..+10% more than the old
//! alts over the same dates at any entry delay of 1-14 days; in 7 d, held 14 d, take 20%: +16% a
//! trade against +7% for the old alts. A small sample with fat tails: a stop is a must.
//!
//! Its state is the store's doc `listing` (the coins' listing times, the ones shorted, the trades).

use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{now, num};
use crate::config::Config;
use crate::live::Venue;
use crate::log;
use crate::store::Store;
use crate::trades::{self, Trade};

const DOC: &str = "listing";
const LOOK_EVERY_S: f64 = 3600.0;
const CHECK_EVERY_S: f64 = 300.0;
/// A coin whose first candle is older than this was listed before we looked: never shorted.
const NEW_WITHIN_DAYS: f64 = 60.0;
const DAY: f64 = 86_400.0;

#[derive(Clone, Debug)]
struct Params {
    delay_days: f64,
    min_vol: f64,
    stop: f64,
    take: f64,
    hold_s: f64,
    risk_usd: f64,
    max_open: usize,
    /// The trial (`LIST_ENABLED` unset): its live trades at most.
    trial: Option<u32>,
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { delay_days: v("LIST_DELAY_DAYS", 7.0), min_vol: v("LIST_MIN_VOL_USD", 5e6), stop: v("LIST_STOP_PCT", 25.0) / 100.0,
            take: v("LIST_TAKE_PCT", 30.0) / 100.0, hold_s: v("LIST_HOLD_DAYS", 14.0) * DAY, risk_usd: v("LIST_RISK_USD", 20.0),
            max_open: v("LIST_MAX_OPEN", 3.0) as usize, trial: trades::switch("LIST_ENABLED").flatten() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    /// Coin -> its listing time (s); 0: listed before we looked.
    #[serde(default)]
    listed: HashMap<String, f64>,
    /// The coins shorted (each listing once).
    #[serde(default)]
    done: Vec<String>,
    #[serde(default)]
    open: Vec<Trade>,
    /// The live trades opened (the trial's count).
    #[serde(default)]
    opened: u32,
}

/// Whether a coin listed at `listed` is due a short at `t` (the delay passed, within a day of it).
fn due(listed: f64, t: f64, delay_days: f64) -> bool {
    listed > 0.0 && t >= listed + delay_days * DAY && t < listed + (delay_days + 1.0) * DAY
}

fn usd(x: f64) -> String {
    if x >= 0.0 { format!("+${x:.2}") } else { format!("-${:.2}", -x) }
}

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    Some(match ev["what"].as_str()? {
        "open" => format!("🔻 ШОРТ ЛИСТИНГА {coin}: {:.0} дн. после листинга, цена {}, на ${:.0}, тейк {} / стоп {}, держим {:.0} дн.",
            f("days_listed"), f("px"), f("notional"), f("take_px"), f("stop_px"), f("hold_days")),
        "close" => format!("{} шорт листинга {} {coin}: {} -> {} ({:+.2}%, {})", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            match ev["why"].as_str().unwrap_or("") { "stop" => "СТОП", "take" => "ТЕЙК", _ => "ЗАКРЫЛ по времени" }, f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_usd"))),
        "error" => format!("⚠️ шорт листинга {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        "signal" => format!("🔻 сигнал шорта листинга {coin} по {} (проба окончена, без сделки)", f("px")),
        _ => return None,
    })
}

struct Listing {
    venue: Venue,
    unified: bool,
    cfg: Config,
    store: Store,
    p: Params,
    state: State,
}

impl Listing {
    fn event(&self, mut ev: Value) {
        ev["kind"] = json!("listing");
        ev["at"] = json!(now());
        log!("listing: {ev}");
        if let Some(m) = message(&ev) {
            crate::notify::send(m);
        }
        self.store.event(ev);
    }

    fn save(&self) {
        let (cfg, v) = (self.cfg.clone(), serde_json::to_value(&self.state).unwrap_or_default());
        tokio::spawn(async move {
            if let Err(e) = crate::store::save_doc(&cfg, DOC, &v).await {
                log!("listing: state not saved: {e:#}");
            }
        });
    }

    /// A coin's listing time: its first daily candle within `NEW_WITHIN_DAYS` (0: older).
    async fn listed_at(&self, coin: &str) -> Option<f64> {
        let t = now();
        let r = self.venue.ex.info(json!({"type": "candleSnapshot", "req": {"coin": coin, "interval": "1d",
            "startTime": ((t - (NEW_WITHIN_DAYS + 5.0) * DAY) * 1000.0) as u64, "endTime": (t * 1000.0) as u64}})).await;
        let first = r.ok()?.as_array()?.first()?["t"].as_f64()? / 1000.0;
        Some(if first <= t - NEW_WITHIN_DAYS * DAY { 0.0 } else { first })
    }

    /// The new coins' listing times; the listings due shorted.
    async fn look(&mut self) {
        let ctx = match self.venue.ex.info(json!({"type": "metaAndAssetCtxs"})).await {
            Ok(v) => v,
            Err(e) => {
                log!("listing: contexts not read: {e:#}");
                return;
            }
        };
        let u = ctx[0]["universe"].as_array().cloned().unwrap_or_default();
        let c = ctx[1].as_array().cloned().unwrap_or_default();
        let mut vol = HashMap::new();
        for (u, c) in u.iter().zip(c.iter()) {
            if u["isDelisted"] == true {
                continue;
            }
            if let Some(n) = u["name"].as_str() {
                vol.insert(n.to_string(), (num(&c["dayNtlVlm"]), num(&c["midPx"])));
            }
        }
        let mut changed = false;
        for coin in vol.keys() {
            if self.state.listed.contains_key(coin) {
                continue;
            }
            if let Some(at) = self.listed_at(coin).await {
                self.state.listed.insert(coin.clone(), at);
                changed = true;
                if at > 0.0 {
                    log!("listing: {coin} listed {:.1} days ago", (now() - at) / DAY);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        let t = now();
        let due_now: Vec<(String, f64)> = self.state.listed.iter().filter(|(c, at)| due(**at, t, self.p.delay_days) && !self.state.done.contains(c))
            .map(|(c, at)| (c.clone(), *at)).collect();
        for (coin, at) in due_now {
            let (v, mid) = vol.get(&coin).copied().unwrap_or((0.0, 0.0));
            if v < self.p.min_vol || mid <= 0.0 || !self.venue.coins.contains_key(&coin) {
                log!("listing: {coin} due but thin (${:.1}M a day) or not tradable", v / 1e6);
                continue;
            }
            let risk = trades::risk_usd(self.p.risk_usd);
            if self.state.open.len() >= self.p.max_open || !trades::can_open(&coin, "listing", -1.0, risk) {
                log!("listing: {coin} due but at most open or held");
                continue;
            }
            self.state.done.push(coin.clone());
            changed = true;
            if !trades::trial_open(self.p.trial, self.state.opened) {
                self.event(json!({"what": "signal", "coin": coin, "days_listed": (t - at) / DAY, "px": mid, "volume": v}));
                continue;
            }
            let size = trades::size_for(risk, self.p.stop, mid);
            match trades::open_market(&self.venue, &coin, -size, self.p.stop, Some(self.p.take), self.p.hold_s).await {
                Ok(tr) => {
                    self.event(json!({"what": "open", "coin": coin, "days_listed": (t - at) / DAY, "px": tr.entry_px, "size": tr.size,
                        "notional": tr.size.abs() * tr.entry_px, "take_px": tr.take_px, "stop_px": tr.stop_px, "hold_days": self.p.hold_s / DAY, "volume": v}));
                    trades::register(&coin, "listing", tr.risk(), tr.size);
                    self.state.open.push(tr);
                    self.state.opened += 1;
                }
                Err(e) => self.event(json!({"what": "error", "coin": coin, "error": format!("{e:#}")})),
            }
        }
        if changed {
            self.save();
        }
    }

    /// The trades: out on time; gone (the stop or the take filled).
    async fn check(&mut self) {
        if self.state.open.is_empty() {
            return;
        }
        let held = match crate::live::equity_of(&self.venue.ex, &self.venue.account, self.unified).await {
            Ok((eq, p)) => {
                trades::set_equity(eq);
                Some(p)
            }
            Err(e) => {
                log!("listing: account not read: {e:#}");
                None
            }
        };
        let t = now();
        let mut keep = Vec::new();
        let mut changed = false;
        for tr in std::mem::take(&mut self.state.open) {
            let out = if t >= tr.exit_at {
                match trades::close(&self.venue, &tr).await {
                    Ok(px) => Some(("time", px)),
                    Err(e) => {
                        self.event(json!({"what": "error", "coin": tr.coin, "error": format!("exit: {e:#}")}));
                        None
                    }
                }
            } else if let Some(h) = &held {
                trades::gone(&self.venue, &tr, h.get(&tr.coin).copied().unwrap_or(0.0), "listing").await
            } else {
                None
            };
            match out {
                Some((why, px)) => {
                    let ret = tr.ret(px);
                    self.event(json!({"what": "close", "coin": tr.coin, "why": why, "entry_px": tr.entry_px, "px": px, "ret_pct": ret * 100.0,
                        "pnl_usd": ret * tr.size.abs() * tr.entry_px, "held_days": (t - tr.at) / DAY}));
                    trades::unregister(&tr.coin, "listing");
                    changed = true;
                }
                None => keep.push(tr),
            }
        }
        self.state.open = keep;
        if changed {
            self.save();
        }
    }
}

/// Starts the listing short if `LIST_ENABLED` is "1" (see the module).
pub async fn start(cfg: &Config, store: Store) -> Result<()> {
    if trades::switch("LIST_ENABLED").is_none() {
        log!("listing: off (LIST_ENABLED=0)");
        return Ok(());
    }
    let Some((venue, unified)) = crate::live::venue_from_env(None).await? else {
        log!("listing: LIVE_KEY / LIVE_ACCOUNT not set, off");
        return Ok(());
    };
    let p = Params::from_env();
    let state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    for tr in &state.open {
        trades::register(&tr.coin, "listing", tr.risk(), tr.size);
    }
    log!("listing: account {}: a listing shorted {:.0} days on (${:.0}M+ a day), stop {:.0}%, take {:.0}%, {:.0} days, risk {}, at most {}; {} held, {} coins known",
        venue.account, p.delay_days, p.min_vol / 1e6, p.stop * 100.0, p.take * 100.0, p.hold_s / DAY, trades::sizing_label(p.risk_usd, p.stop), p.max_open,
        state.open.len(), state.listed.len());
    crate::notify::send(format!("🔻 шорт листингов НА ДЕНЬГИ: через {:.0} дн. после листинга (оборот от ${:.0}M), стоп +{:.0}%, тейк −{:.0}%, держим {:.0} дн., риск {}, до {} позиций{}",
        p.delay_days, p.min_vol / 1e6, p.stop * 100.0, p.take * 100.0, p.hold_s / DAY, trades::sizing_label(p.risk_usd, p.stop), p.max_open,
        trades::trial_label(p.trial, state.opened)));
    let mut l = Listing { venue, unified, cfg: cfg.clone(), store, p, state };
    tokio::spawn(async move {
        let (mut look_at, mut check_at) = (0.0, 0.0);
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            let t = now();
            if t - look_at >= LOOK_EVERY_S {
                l.look().await;
                look_at = t;
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
    fn due_a_week_on_within_a_day() {
        let at = 1_000_000.0;
        assert!(!due(at, at + 6.9 * DAY, 7.0));
        assert!(due(at, at + 7.2 * DAY, 7.0));
        assert!(!due(at, at + 8.1 * DAY, 7.0));
        assert!(!due(0.0, at, 7.0));                          // listed before we looked
        let m = message(&json!({"what": "close", "coin": "NEW", "why": "take", "entry_px": 1.0, "px": 0.8, "ret_pct": 20.0, "pnl_usd": 16.0})).unwrap();
        assert!(m.contains("ТЕЙК") && m.contains("+$16.00"));
    }
}
