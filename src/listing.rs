//! The listing short (`LIST_ENABLED`: on unless "0"; with `LIVE_KEY` / `LIVE_ACCOUNT` / `LIVE_NET` as for the
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
use crate::keeper::{HeldIn, Keeper};
use crate::notify::usd;
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
    /// The trial (`LIST_ENABLED` set empty): its live trades at most.
    trial: Option<u32>,
}

impl Params {
    fn from_env() -> Self {
        let v = trades::env_f;
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

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    Some(match ev["what"].as_str()? {
        "open" => format!("🔻 ШОРТ ЛИСТИНГА {coin}: {:.0} дн. после листинга, цена {}, на ${:.0}, тейк {} / стоп {}, держим {:.0} дн.",
            f("days_listed"), f("px"), f("notional"), f("take_px"), f("stop_px"), f("hold_days")),
        "close" => format!("{} шорт листинга {} {coin}: {} -> {} ({:+.2}%, {})", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            match ev["why"].as_str().unwrap_or("") { "stop" => "СТОП", "take" => "ТЕЙК", "manual" => "ЗАКРЫТО ВНЕ БОТА (руками или ликвидация)", _ => "ЗАКРЫЛ по времени" }, f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_usd"))),
        "error" => format!("⚠️ шорт листинга {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        "signal" if !crate::notify::PAPER_TO_CHAT => return None,
        "signal" => format!("🔻 сигнал шорта листинга {coin} по {} (проба окончена, без сделки)", f("px")),
        _ => return None,
    })
}

struct Listing {
    k: Keeper,
    p: Params,
    state: State,
}

impl Listing {
    /// A coin's listing time: its first daily candle within `NEW_WITHIN_DAYS` (0: older).
    async fn listed_at(&self, coin: &str) -> Option<f64> {
        let t = now();
        let r = self.k.venue.ex.info(json!({"type": "candleSnapshot", "req": {"coin": coin, "interval": "1d",
            "startTime": ((t - (NEW_WITHIN_DAYS + 5.0) * DAY) * 1000.0) as u64, "endTime": (t * 1000.0) as u64}})).await;
        let first = r.ok()?.as_array()?.first()?["t"].as_f64()? / 1000.0;
        Some(if first <= t - NEW_WITHIN_DAYS * DAY { 0.0 } else { first })
    }

    /// The new coins' listing times; the listings due shorted.
    async fn look(&mut self) {
        let ctx = match self.k.venue.ex.info(json!({"type": "metaAndAssetCtxs"})).await {
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
            if v < self.p.min_vol || mid <= 0.0 || !self.k.venue.coins.contains_key(&coin) {
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
                self.k.event(json!({"what": "signal", "coin": coin, "days_listed": (t - at) / DAY, "px": mid, "volume": v}));
                continue;
            }
            let size = trades::size_for(risk, self.p.stop, mid);
            match trades::open_market(&self.k.venue, &coin, -size, self.p.stop, Some(self.p.take), self.p.hold_s).await {
                Ok(tr) => {
                    self.k.event(json!({"what": "open", "coin": coin, "days_listed": (t - at) / DAY, "px": tr.entry_px, "size": tr.size,
                        "notional": tr.size.abs() * tr.entry_px, "take_px": tr.take_px, "stop_px": tr.stop_px, "hold_days": self.p.hold_s / DAY, "volume": v}));
                    trades::register(&coin, "listing", tr.risk(), tr.size);
                    self.state.open.push(tr);
                    self.state.opened += 1;
                }
                Err(e) => self.k.event(json!({"what": "error", "coin": coin, "error": format!("{e:#}")})),
            }
        }
        if changed {
            self.k.save(DOC, &self.state);
        }
    }

    /// The trades swept (see `Keeper::sweep`).
    async fn check(&mut self) {
        if self.k.sweep(&mut self.state.open, |t| t, true, true).await {
            self.k.save(DOC, &self.state);
        }
    }

}

/// Starts the listing short unless `LIST_ENABLED` is "0" (see the module).
pub async fn start(cfg: &Config, store: Store) -> Result<()> {
    if trades::switch("LIST_ENABLED").is_none() {
        log!("listing: off (LIST_ENABLED=0)");
        return Ok(());
    }
    let Some((venue, unified)) = crate::venue::venue_from_env(None).await? else {
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
    let mut l = Listing { k: Keeper::new(venue, unified, cfg.clone(), store, "listing", message, HeldIn::Days), p, state };
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

#[cfg(test)]
mod lifecycle {
    use super::*;
    use crate::testkit::{FakeHl, cfg, saved};

    async fn setup(coin: &str) -> (FakeHl, Listing, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        let f = FakeHl::start(&[(coin, 2, 10, 100.0)]).await;
        let (store, rx) = Store::for_test();
        let s = Listing { k: Keeper::new(f.venue(), false, cfg(), store, "listing", message, HeldIn::Days), p: Params::from_env(), state: State::default() };
        (f, s, rx)
    }

    async fn hold(s: &mut Listing, coin: &str) {
        let tr = trades::open_market(&s.k.venue, coin, -1.0, 0.04, Some(0.05), 3600.0).await.unwrap();
        trades::register(coin, "listing", tr.risk(), tr.size);
        s.state.open.push(tr);
    }

    #[tokio::test]
    async fn a_stop_filled_ends_the_short_told_and_saved() {
        let (f, mut s, mut rx) = setup("LSA").await;
        hold(&mut s, "LSA").await;
        f.set_mid("LSA", 105.0);
        s.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!(ev.len(), 1, "{ev:?}");
        let e = &ev[0];
        assert_eq!((e["kind"].as_str(), e["what"].as_str(), e["why"].as_str()), (Some("listing"), Some("close"), Some("stop")));
        assert!(e["ret_pct"].as_f64().unwrap() < -3.9 && e["pnl_usd"].as_f64().unwrap() < 0.0 && e["entry_px"].as_f64() == Some(100.0));
        assert!(s.state.open.is_empty() && !trades::registered("LSA"));
        assert_eq!(saved(&s.k.cfg, "listing").await.unwrap()["open"], json!([]));
    }

    #[tokio::test]
    async fn a_take_filled_ends_it() {
        let (f, mut s, mut rx) = setup("LSB").await;
        hold(&mut s, "LSB").await;
        f.set_mid("LSB", 94.0);
        s.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((ev.len(), ev[0]["why"].as_str()), (1, Some("take")), "{ev:?}");
        assert!(s.state.open.is_empty() && f.position("LSB") == 0.0);
    }

    #[tokio::test]
    async fn still_held_it_stays() {
        let (f, mut s, mut rx) = setup("LSC").await;
        hold(&mut s, "LSC").await;
        f.set_mid("LSC", 99.0);
        s.check().await;
        assert!(crate::store::drain(&mut rx).is_empty() && s.state.open.len() == 1 && trades::registered("LSC"));
    }

    #[tokio::test]
    async fn its_time_up_it_is_closed_at_market() {
        let (f, mut s, mut rx) = setup("LSD").await;
        hold(&mut s, "LSD").await;
        s.state.open[0].exit_at = now() - 1.0;
        f.set_mid("LSD", 98.0);
        s.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((ev.len(), ev[0]["why"].as_str()), (1, Some("time")), "{ev:?}");
        assert!((ev[0]["px"].as_f64().unwrap() - 98.0).abs() < 1e-9 && ev[0]["ret_pct"].as_f64().unwrap() > 1.9);
        assert!(f.position("LSD") == 0.0 && f.open_orders("LSD").is_empty() && s.state.open.is_empty());
    }
}

#[cfg(test)]
mod candles {
    use super::*;
    use crate::testkit::{FakeHl, cfg};

    #[tokio::test]
    async fn the_listing_time_is_the_first_day_within_the_window_else_0() {
        let f = FakeHl::start(&[("LCA", 2, 10, 1.0)]).await;
        let (store, _) = Store::for_test();
        let s = Listing { k: Keeper::new(f.venue(), false, cfg(), store, "listing", message, HeldIn::Days), p: Params::from_env(), state: State::default() };
        let day = (now() / DAY).floor() * DAY;
        f.set_candles("LCA", "1d", &[(day - 2.0 * DAY, 1.0, 1.0, 1.0, 1.0, 1.0), (day - DAY, 1.0, 1.0, 1.0, 1.0, 1.0)]);
        assert_eq!(s.listed_at("LCA").await, Some(day - 2.0 * DAY));
        f.set_candles("LCB", "1d", &(0..(NEW_WITHIN_DAYS as i64 + 3)).map(|i| (day - (NEW_WITHIN_DAYS + 2.0 - i as f64) * DAY, 1.0, 1.0, 1.0, 1.0, 1.0)).collect::<Vec<_>>());
        assert_eq!(s.listed_at("LCB").await, Some(0.0), "listed before the window");
        assert_eq!(s.listed_at("LCC").await, None, "no candles");
    }
}
