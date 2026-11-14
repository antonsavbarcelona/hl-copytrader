//! The monthly-expiry short (`EXP_ENABLED`: on unless "0"; with `LIVE_KEY` / `LIVE_ACCOUNT` / `LIVE_NET` as for the
//! dip): on the last Friday of the month (Deribit's monthly options settle at 08:00 UTC) BTC and ETH are shorted at
//! market from `EXP_FROM_MIN` (00:01 UTC; not after `EXP_LATE_MIN`, 00:59) and closed at market at `EXP_TO_MIN`
//! (08:00); a resting stop `EXP_STOP_PCT` (4) over, which is also the size's stop (the risk: `FAMILY_RISK_PCT` of the
//! equity, the stop losing it; `EXP_RISK_USD` $20 without it); each coin once an expiry.
//!
//! V218 .. V222, Binance 1m 2020-01 .. 2026-09, 81 monthly expiries: short 00:00 -> 08:00 UTC, net of 11 bp, BTC +39 bp
//! (t 2.6, 64% up, worst -3.9%), ETH +49 bp (t 2.5); 6 of 7 years up (2023 ~0); the same window a week before -18 bp,
//! a week after -1, the other Fridays +6; a 3% stop as without one, 2% halves it; HL (17 expiries from 2025-05) BTC +30.
//! The Binance portfolio (the bot's set, 2%) at the same drawdown: 2020-23 +9% / +13%, 2023-26 +7% (sized at a 4% stop).
//!
//! Its state is the store's doc `expiry` (the expiries done, the trades).

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

const DOC: &str = "expiry";
const KIND: &str = "expiry";
const COINS: [&str; 2] = ["BTC", "ETH"];
const EVERY_S: f64 = 30.0;

#[derive(Clone, Debug)]
struct Params {
    from_min: i64,
    late_min: i64,
    to_min: i64,
    stop: f64,
    risk_usd: f64,
    /// The trial (`EXP_ENABLED` set empty): its live trades at most.
    trial: Option<u32>,
}

impl Params {
    fn from_env() -> Self {
        let v = trades::env_f;
        Params { from_min: v("EXP_FROM_MIN", 1.0) as i64, late_min: v("EXP_LATE_MIN", 59.0) as i64, to_min: v("EXP_TO_MIN", 480.0) as i64,
            stop: v("EXP_STOP_PCT", 4.0) / 100.0, risk_usd: v("EXP_RISK_USD", 20.0), trial: trades::switch("EXP_ENABLED").flatten() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    /// "<day>:<coin>" of the expiries shorted (each coin once an expiry).
    #[serde(default)]
    done: Vec<String>,
    #[serde(default)]
    open: Vec<Trade>,
    /// The live trades opened (the trial's count).
    #[serde(default)]
    opened: u32,
}

/// The civil month of a day since 1970-01-01 (H. Hinnant's civil_from_days).
fn month(day: i64) -> i64 {
    let z = day + 719_468;
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mp = (5 * (doe - (365 * yoe + yoe / 4 - yoe / 100)) + 2) / 153;
    if mp < 10 { mp + 3 } else { mp - 9 }
}

/// The last Friday of its month (the Deribit monthly expiry's day).
fn expiry_day(day: i64) -> bool {
    (day + 3).rem_euclid(7) == 4 && month(day + 7) != month(day)
}

/// Whether an epoch minute is in the entry window of an expiry day.
fn entry_window(minute: i64, p: &Params) -> bool {
    let m = minute.rem_euclid(1440);
    expiry_day(minute.div_euclid(1440)) && m >= p.from_min && m <= p.late_min
}

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    Some(match ev["what"].as_str()? {
        "open" => format!("🔻 ШОРТ ЭКСПИРАЦИИ {coin} (последняя пятница месяца): цена {}, на ${:.0}, стоп {}, закроем в 08:00 UTC", f("px"), f("notional"), f("stop_px")),
        "close" => format!("{} шорт экспирации {} {coin}: {} -> {} ({:+.2}%, {})", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            if ev["why"] == "stop" { "СТОП" } else if ev["why"] == "manual" { "ЗАКРЫТО ВНЕ БОТА (руками или ликвидация)" } else { "ЗАКРЫЛ по времени" }, f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_usd"))),
        "error" => format!("⚠️ шорт экспирации {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        "signal" if !crate::notify::PAPER_TO_CHAT => return None,
        "signal" => format!("🔻 сигнал шорта экспирации {coin} по {} (проба окончена, без сделки)", f("px")),
        _ => return None,
    })
}

struct Expiry {
    k: Keeper,
    p: Params,
    state: State,
}

impl Expiry {
    /// In the entry window: each coin not yet shorted this expiry, shorted at market.
    async fn enter(&mut self) {
        let minute = (now() / 60.0) as i64;
        if !entry_window(minute, &self.p) {
            return;
        }
        let day = minute.div_euclid(1440);
        let hold_s = ((day * 1440 + self.p.to_min - minute) * 60) as f64;
        for coin in COINS {
            let key = format!("{day}:{coin}");
            if self.state.done.contains(&key) || !self.k.venue.coins.contains_key(coin) {
                continue;
            }
            if let Ok((eq, _)) = crate::venue::equity_of(&self.k.venue.ex, &self.k.venue.account, self.k.unified).await {
                trades::set_equity(eq);
            }
            let risk = trades::risk_usd(self.p.risk_usd);
            if !trades::can_open(coin, KIND, -1.0, risk) {
                log!("expiry: {coin} held or the family's cap: not shorted");
                continue;
            }
            let px = match self.k.venue.mid(coin).await {
                Ok(px) if px > 0.0 => px,
                _ => continue,
            };
            self.state.done.push(key);
            self.k.save(DOC, &self.state);
            if !trades::trial_open(self.p.trial, self.state.opened) {
                self.k.event(json!({"what": "signal", "coin": coin, "px": px}));
                continue;
            }
            let size = trades::size_for(risk, self.p.stop, px);
            match trades::open_market(&self.k.venue, coin, -size, self.p.stop, None, hold_s).await {
                Ok(tr) => {
                    self.k.event(json!({"what": "open", "coin": coin, "px": tr.entry_px, "size": tr.size, "notional": tr.size.abs() * tr.entry_px, "stop_px": tr.stop_px}));
                    trades::register(coin, KIND, tr.risk(), tr.size);
                    self.state.open.push(tr);
                    self.state.opened += 1;
                    self.k.save(DOC, &self.state);
                }
                Err(e) => self.k.event(json!({"what": "error", "coin": coin, "error": format!("{e:#}")})),
            }
        }
    }

    /// The trades swept (see `Keeper::sweep`).
    async fn check(&mut self) {
        if self.k.sweep(&mut self.state.open, |t| t, true).await {
            self.k.save(DOC, &self.state);
        }
    }

}

/// Starts the monthly-expiry short unless `EXP_ENABLED` is "0" (see the module).
pub async fn start(cfg: &Config, store: Store) -> Result<()> {
    if trades::switch("EXP_ENABLED").is_none() {
        log!("expiry: off (EXP_ENABLED=0)");
        return Ok(());
    }
    let Some((venue, unified)) = crate::venue::venue_from_env(None).await? else {
        log!("expiry: LIVE_KEY / LIVE_ACCOUNT not set, off");
        return Ok(());
    };
    let p = Params::from_env();
    let mut state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    let keep_from = (now() / 86_400.0) as i64 - 60;
    state.done.retain(|k| k.split(':').next().and_then(|d| d.parse::<i64>().ok()).is_some_and(|d| d >= keep_from));
    for tr in &state.open {
        trades::register(&tr.coin, KIND, tr.risk(), tr.size);
    }
    log!("expiry: account {}: BTC / ETH shorted on the last Friday of the month {:02}:{:02} .. {:02}:{:02} UTC, stop {:.0}%, risk {}; {} held",
        venue.account, p.from_min / 60, p.from_min % 60, p.to_min / 60, p.to_min % 60, p.stop * 100.0, trades::sizing_label(p.risk_usd, p.stop), state.open.len());
    crate::notify::send(format!("🔻 шорт экспирации НА ДЕНЬГИ: BTC и ETH в последнюю пятницу месяца с {:02}:{:02} до {:02}:{:02} UTC, стоп +{:.0}%, риск {}{}",
        p.from_min / 60, p.from_min % 60, p.to_min / 60, p.to_min % 60, p.stop * 100.0, trades::sizing_label(p.risk_usd, p.stop), trades::trial_label(p.trial, state.opened)));
    let mut x = Expiry { k: Keeper::new(venue, unified, cfg.clone(), store, KIND, message, HeldIn::No), p, state };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(EVERY_S as u64));
        loop {
            tick.tick().await;
            x.enter().await;
            x.check().await;
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_fridays() {
        // 2026-10-30, 2026-09-25, 2026-01-30 are the last Fridays; 2026-10-23 a Friday but not the last; 2026-10-31 a Saturday
        let d = |y: i64, m: i64, dd: i64| {
            let y_ = if m <= 2 { y - 1 } else { y };
            let era = y_.div_euclid(400);
            let yoe = y_ - era * 400;
            era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + dd - 1 - 719_468
        };
        assert!(expiry_day(d(2026, 10, 30)) && expiry_day(d(2026, 9, 25)) && expiry_day(d(2026, 1, 30)));
        assert!(!expiry_day(d(2026, 10, 23)) && !expiry_day(d(2026, 10, 31)) && !expiry_day(d(2026, 11, 6)));
        assert_eq!(month(d(2026, 12, 31)), 12);
        assert_eq!(month(d(2027, 1, 1)), 1);
        let p = Params { from_min: 1, late_min: 59, to_min: 480, stop: 0.04, risk_usd: 20.0, trial: None };
        let m0 = d(2026, 10, 30) * 1440;
        assert!(!entry_window(m0, &p) && entry_window(m0 + 1, &p) && entry_window(m0 + 59, &p) && !entry_window(m0 + 60, &p));
        assert!(!entry_window(d(2026, 10, 23) * 1440 + 5, &p));
        let m = message(&json!({"what": "close", "coin": "BTC", "why": "time", "entry_px": 100.0, "px": 99.0, "ret_pct": 1.0, "pnl_usd": 5.0})).unwrap();
        assert!(m.contains("по времени") && m.contains("+$5.00"));
    }
}

#[cfg(test)]
mod lifecycle {
    use super::*;
    use crate::testkit::{FakeHl, cfg, saved};

    async fn setup(coin: &str) -> (FakeHl, Expiry, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        let f = FakeHl::start(&[(coin, 2, 10, 100.0)]).await;
        let (store, rx) = Store::for_test();
        let s = Expiry { k: Keeper::new(f.venue(), false, cfg(), store, KIND, message, HeldIn::No), p: Params::from_env(), state: State::default() };
        (f, s, rx)
    }

    async fn hold(s: &mut Expiry, coin: &str) {
        let tr = trades::open_market(&s.k.venue, coin, -1.0, 0.04, Some(0.05), 3600.0).await.unwrap();
        trades::register(coin, "expiry", tr.risk(), tr.size);
        s.state.open.push(tr);
    }

    #[tokio::test]
    async fn a_stop_filled_ends_the_short_told_and_saved() {
        let (f, mut s, mut rx) = setup("EXA").await;
        hold(&mut s, "EXA").await;
        f.set_mid("EXA", 105.0);
        s.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!(ev.len(), 1, "{ev:?}");
        let e = &ev[0];
        assert_eq!((e["kind"].as_str(), e["what"].as_str(), e["why"].as_str()), (Some("expiry"), Some("close"), Some("stop")));
        assert!(e["ret_pct"].as_f64().unwrap() < -3.9 && e["pnl_usd"].as_f64().unwrap() < 0.0 && e["entry_px"].as_f64() == Some(100.0));
        assert!(s.state.open.is_empty() && !trades::registered("EXA"));
        assert_eq!(saved(&s.k.cfg, "expiry").await.unwrap()["open"], json!([]));
    }

    #[tokio::test]
    async fn a_take_filled_ends_it() {
        let (f, mut s, mut rx) = setup("EXB").await;
        hold(&mut s, "EXB").await;
        f.set_mid("EXB", 94.0);
        s.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((ev.len(), ev[0]["why"].as_str()), (1, Some("take")), "{ev:?}");
        assert!(s.state.open.is_empty() && f.position("EXB") == 0.0);
    }

    #[tokio::test]
    async fn still_held_it_stays() {
        let (f, mut s, mut rx) = setup("EXC").await;
        hold(&mut s, "EXC").await;
        f.set_mid("EXC", 99.0);
        s.check().await;
        assert!(crate::store::drain(&mut rx).is_empty() && s.state.open.len() == 1 && trades::registered("EXC"));
    }

    #[tokio::test]
    async fn its_time_up_it_is_closed_at_market() {
        let (f, mut s, mut rx) = setup("EXD").await;
        hold(&mut s, "EXD").await;
        s.state.open[0].exit_at = now() - 1.0;
        f.set_mid("EXD", 98.0);
        s.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((ev.len(), ev[0]["why"].as_str()), (1, Some("time")), "{ev:?}");
        assert!((ev[0]["px"].as_f64().unwrap() - 98.0).abs() < 1e-9 && ev[0]["ret_pct"].as_f64().unwrap() > 1.9);
        assert!(f.position("EXD") == 0.0 && f.open_orders("EXD").is_empty() && s.state.open.is_empty());
    }
}
