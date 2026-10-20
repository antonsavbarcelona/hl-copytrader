//! The dip-buy on the live account (`DIP_ENABLED=1`, with `LIVE_KEY`, `LIVE_ACCOUNT`, `LIVE_NET`
//! as for `live`): a perp with `DIP_MIN_VOL_USD` (20M) or more traded over the last 24 h whose mid
//! fell `DIP_DROP_PCT` (8) or more over the last `DIP_WINDOW_MIN` (15) minutes, by the minutes'
//! closes, while BTC's moved less than `DIP_BTC_MAX_PCT` (2) over them (the coin's own fall, not
//! the market's), is bought at market as its minute closes; out at market `DIP_HOLD_MIN` (60)
//! later, with a resting stop `DIP_STOP_PCT` (20) under our fill; sized so that the stop loses
//! `DIP_RISK_PCT` (5) of the account's equity (the notional risk / stop: 25%); at most
//! `DIP_MAX_OPEN` (3) open, one a coin, a coin not again for `DIP_COOLDOWN_MIN` (240); a coin the
//! account holds otherwise (the copy's legs) is left alone. `DIP_DRY=1`: the signals only (events
//! `signal`), no orders.
//!
//! The archive research of 2026-10-20 (2025-05 .. 2026-10, the raw tape: our buy at the worst
//! price of the first block a second after the signal, the taker fee both ways): +199 bp a trade
//! over 198 such falls; at these settings +85% over the 16 months without reinvestment (+125%
//! with it, as here: the size follows the equity), max drawdown 10%, 13 months of 16 up; every
//! neighbouring setting (5-8%, 5-15 min, 1-4 h) in profit, and a walk-forward choice of the
//! setting too. Fading rises the same way loses: their tails are fatal.
//!
//! Its state is the store's doc `dip` (the positions it holds, the coins' last entries): a restart
//! keeps their timers. Every minute the account's positions are read: a position gone (its stop
//! filled, a liquidation) ends ours. Events: kind `dip`; with `TELEGRAM_BOT_TOKEN` and
//! `TELEGRAM_CHAT_ID` each entry, exit, stop, error and skipped fall goes to the chat too, and a
//! note a day (`notify`).

use std::collections::{HashMap, VecDeque};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::account::round_size;
use crate::api::{Api, now};
use crate::config::Config;
use crate::live::{Venue, equity_of};
use crate::log;
use crate::store::Store;

const DOC: &str = "dip";
const MIN_ORDER_USD: f64 = 10.0;
/// A book older than this (s) gives no close.
const BOOK_STALE_S: f64 = 30.0;
/// The day volumes are read this often (s).
const VOLUME_EVERY_S: f64 = 300.0;

#[derive(Clone, Debug)]
struct Params {
    drop: f64,
    window: i64,
    btc_max: f64,
    min_vol: f64,
    hold_s: f64,
    stop: f64,
    risk: f64,
    max_open: usize,
    cooldown_s: f64,
    dry: bool,
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { drop: v("DIP_DROP_PCT", 8.0) / 100.0, window: v("DIP_WINDOW_MIN", 15.0) as i64, btc_max: v("DIP_BTC_MAX_PCT", 2.0) / 100.0,
            min_vol: v("DIP_MIN_VOL_USD", 20e6), hold_s: v("DIP_HOLD_MIN", 60.0) * 60.0, stop: v("DIP_STOP_PCT", 20.0) / 100.0,
            risk: v("DIP_RISK_PCT", 5.0) / 100.0, max_open: v("DIP_MAX_OPEN", 3.0) as usize, cooldown_s: v("DIP_COOLDOWN_MIN", 240.0) * 60.0,
            dry: std::env::var("DIP_DRY").unwrap_or_default().trim() == "1" }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pos {
    size: f64,
    entry_px: f64,
    opened: f64,
    stop_px: f64,
    stop_oid: Option<u64>,
    /// The fall that triggered it (a fraction, negative).
    fall: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    open: HashMap<String, Pos>,
    /// A coin's last entry (unix s): the cooldown.
    last_entry: HashMap<String, f64>,
    entries: u64,
}

/// Each coin's last minutes' closes (minute index, the last mid seen in it).
#[derive(Default)]
struct Closes(HashMap<String, VecDeque<(i64, f64)>>);

impl Closes {
    fn update(&mut self, coin: &str, minute: i64, px: f64, keep: usize) {
        let q = self.0.entry(coin.to_string()).or_default();
        match q.back_mut() {
            Some(last) if last.0 == minute => last.1 = px,
            _ => {
                q.push_back((minute, px));
                while q.len() > keep {
                    q.pop_front();
                }
            }
        }
    }

    /// The close of the last minute at or before `minute`.
    fn at(&self, coin: &str, minute: i64) -> Option<f64> {
        self.0.get(coin)?.iter().rev().find(|(m, _)| *m <= minute).map(|x| x.1)
    }

    /// The move over `w` minutes to the close of `minute` (it and the one `w` before both seen).
    fn change(&self, coin: &str, minute: i64, w: i64) -> Option<f64> {
        let q = self.0.get(coin)?;
        if q.front()?.0 > minute - w {
            return None;
        }
        Some(self.at(coin, minute)? / self.at(coin, minute - w)? - 1.0)
    }
}

struct Dip {
    venue: Venue,
    unified: bool,
    cfg: Config,
    store: Store,
    p: Params,
    state: State,
    closes: Closes,
    volume: HashMap<String, f64>,
    positions: HashMap<String, f64>,
    equity: f64,
}

/// Starts the dip-buy if `DIP_ENABLED` is "1" and the live account is set (see the module).
pub async fn start(cfg: &Config, store: Store, api: Api, books: crate::ws::Books) -> Result<()> {
    if std::env::var("DIP_ENABLED").unwrap_or_default().trim() != "1" {
        log!("dip: off (DIP_ENABLED=1 to trade it)");
        return Ok(());
    }
    let Some((venue, unified)) = crate::live::venue_from_env(Some(books.clone())).await? else {
        log!("dip: LIVE_KEY / LIVE_ACCOUNT not set, off");
        return Ok(());
    };
    let state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    let p = Params::from_env();
    log!("dip: {}{} account {}, {} held; buys a fall of {:.0}% in {} min (BTC under {:.0}%, ${:.0}M+ a day), {:.0} min hold, stop {:.0}%, risk {:.1}% ({:.0}% of the equity a trade), at most {}",
        if p.dry { "DRY (signals only) " } else { "" }, if venue.ex.mainnet() { "MAINNET" } else { "testnet" }, venue.account, state.open.len(), p.drop * 100.0, p.window, p.btc_max * 100.0,
        p.min_vol / 1e6, p.hold_s / 60.0, p.stop * 100.0, p.risk * 100.0, p.risk / p.stop * 100.0, p.max_open);
    if std::env::var("LIVE_ENABLED").unwrap_or_default().trim() == "1" {
        log!("dip: LIVE_ENABLED is on too: the copy's coins are left alone (one position a coin on the account)");
    }
    crate::notify::send(format!("🤖 покупка падений запущена{} ({}), открыто {}
сигнал: падение {:.0}% за {} мин (BTC меньше {:.0}%, оборот от ${:.0}M), держим {:.0} мин, стоп {:.0}%, риск {:.1}% на сделку ({:.0}% депозита), до {} позиций",
        if p.dry { " БЕЗ СДЕЛОК (только сигналы)" } else { "" }, if venue.ex.mainnet() { "mainnet" } else { "testnet" }, state.open.len(), p.drop * 100.0, p.window,
        p.btc_max * 100.0, p.min_vol / 1e6, p.hold_s / 60.0, p.stop * 100.0, p.risk * 100.0, p.risk / p.stop * 100.0, p.max_open));
    let mut d = Dip { venue, unified, cfg: cfg.clone(), store, p, state, closes: Closes::default(), volume: HashMap::new(),
        positions: HashMap::new(), equity: 0.0 };
    tokio::spawn(async move {
        let mut last_minute = (now() / 60.0).floor() as i64;
        let mut vol_at = 0.0;
        let mut read_at = 0.0;
        // The day's note to the chat: alive, the equity, what it did.
        let (mut day_at, mut day_entries) = (now(), d.state.entries);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        loop {
            tick.tick().await;
            let t = now();
            if t - vol_at >= VOLUME_EVERY_S {
                match api.meta().await {
                    Ok((_, ctx)) => {
                        d.volume = ctx.into_iter().map(|(c, x)| (c, x.day_volume)).collect();
                        vol_at = t;
                    }
                    Err(e) => log!("dip: volumes not read: {e:#}"),
                }
            }
            if t - read_at >= 60.0 {
                read_at = t;
                d.reconcile().await;
            }
            if t - day_at >= 86_400.0 && d.equity > 0.0 {
                crate::notify::send(format!("📊 покупка падений работает: депозит ${:.2}, открыто {}, покупок за сутки {}",
                    d.equity, d.state.open.len(), d.state.entries - day_entries));
                (day_at, day_entries) = (t, d.state.entries);
            }
            // The minute's close so far: each coin's mid now.
            let minute = (t / 60.0).floor() as i64;
            let keep = d.p.window as usize + 3;
            {
                let b = books.read().unwrap();
                for (c, book) in b.iter() {
                    if t - book.time_ms as f64 / 1000.0 > BOOK_STALE_S {
                        continue;
                    }
                    if let Some(m) = book.mid() {
                        d.closes.update(c, minute, m, keep);
                    }
                }
            }
            if minute > last_minute {
                // The minute before closed: its signals, then the exits due.
                d.signals(minute - 1).await;
                last_minute = minute;
            }
            d.exits().await;
        }
    });
    Ok(())
}

impl Dip {
    fn event(&self, mut ev: Value) {
        ev["kind"] = json!("dip");
        ev["at"] = json!(now());
        log!("dip: {ev}");
        if let Some(m) = message(&ev) {
            crate::notify::send(m);
        }
        self.store.event(ev);
    }

    fn save(&self) {
        let (cfg, v) = (self.cfg.clone(), serde_json::to_value(&self.state).unwrap_or_default());
        tokio::spawn(async move {
            if let Err(e) = crate::store::save_doc(&cfg, DOC, &v).await {
                log!("dip: state not saved: {e:#}");
            }
        });
    }

    /// The falls of the minute that closed, biggest first, bought within the limits.
    async fn signals(&mut self, minute: i64) {
        let Some(btc) = self.closes.change("BTC", minute, self.p.window) else { return };
        if btc.abs() >= self.p.btc_max {
            return;
        }
        let mut falls: Vec<(String, f64)> = self.closes.0.keys()
            .filter(|c| self.venue.coins.contains_key(*c) && self.volume.get(*c).copied().unwrap_or(0.0) >= self.p.min_vol)
            .filter_map(|c| Some((c.clone(), self.closes.change(c, minute, self.p.window)?)))
            .filter(|(_, r)| *r <= -self.p.drop).collect();
        falls.sort_by(|a, b| a.1.total_cmp(&b.1));
        for (coin, fall) in falls {
            let t = now();
            let why = if self.state.open.len() >= self.p.max_open {
                Some("at most open")
            } else if self.state.open.contains_key(&coin) {
                Some("held")
            } else if self.state.last_entry.get(&coin).is_some_and(|x| t - x < self.p.cooldown_s) {
                Some("cooldown")
            } else if self.positions.get(&coin).is_some_and(|x| *x != 0.0) {
                Some("the account holds it otherwise")
            } else if self.equity <= 0.0 {
                Some("equity not read yet")
            } else {
                None
            };
            if let Some(why) = why {
                self.event(json!({"what": "skipped", "coin": coin, "fall": fall, "btc": btc, "why": why}));
                continue;
            }
            self.enter(&coin, fall, btc).await;
        }
    }

    async fn enter(&mut self, coin: &str, fall: f64, btc: f64) {
        let Some(mid) = self.closes.at(coin, i64::MAX) else { return };
        let dec = self.venue.coins[coin].1;
        let size = round_size(self.p.risk / self.p.stop * self.equity / mid, dec);
        if size * mid < MIN_ORDER_USD {
            self.event(json!({"what": "too small", "coin": coin, "equity": self.equity, "size": size}));
            return;
        }
        if self.p.dry {
            // The cooldown as if bought: one signal a coin a fall.
            self.state.last_entry.insert(coin.to_string(), now());
            self.event(json!({"what": "signal", "coin": coin, "fall": fall, "btc": btc, "mid": mid, "size": size, "notional": size * mid, "equity": self.equity}));
            return;
        }
        let t0 = std::time::Instant::now();
        let (got, px, _) = match self.venue.market(coin, size, false).await {
            Ok(x) if x.0 > 0.0 => x,
            Ok(_) => {
                self.event(json!({"what": "error", "coin": coin, "error": "nothing filled"}));
                return;
            }
            Err(e) => {
                self.event(json!({"what": "error", "coin": coin, "error": format!("{e:#}")}));
                return;
            }
        };
        let order_ms = t0.elapsed().as_millis() as u64;
        let stop_px = px * (1.0 - self.p.stop);
        let stop_oid = match self.venue.stop(coin, 1.0, got, stop_px).await {
            Ok(o) => Some(o),
            Err(e) => {
                log!("dip: {coin} stop not placed (the next check places it): {e:#}");
                None
            }
        };
        let t = now();
        self.state.open.insert(coin.to_string(), Pos { size: got, entry_px: px, opened: t, stop_px, stop_oid, fall });
        self.state.last_entry.insert(coin.to_string(), t);
        self.state.entries += 1;
        self.positions.insert(coin.to_string(), got);
        self.event(json!({"what": "open", "coin": coin, "fall": fall, "btc": btc, "size": got, "px": px, "notional": got * px,
            "equity": self.equity, "stop_px": stop_px, "stop_oid": stop_oid, "order_ms": order_ms, "signal_mid": mid}));
        self.save();
    }

    /// The positions whose hold is over: out at market, their stops cancelled.
    async fn exits(&mut self) {
        let t = now();
        let due: Vec<(String, Pos)> = self.state.open.iter().filter(|(_, p)| t - p.opened >= self.p.hold_s).map(|(c, p)| (c.clone(), p.clone())).collect();
        for (coin, p) in due {
            match self.venue.market(&coin, -p.size, true).await {
                Ok((got, px, _)) => {
                    if let Some(o) = p.stop_oid {
                        self.venue.cancel(&coin, o).await;
                    }
                    self.state.open.remove(&coin);
                    self.positions.remove(&coin);
                    self.event(json!({"what": "close", "why": "hold", "coin": coin, "size": got, "px": px, "entry_px": p.entry_px,
                        "pnl_approx": p.size * (px - p.entry_px), "ret_pct": (px / p.entry_px - 1.0) * 100.0, "held_min": (t - p.opened) / 60.0}));
                    self.save();
                }
                Err(e) => {
                    // Tried again on the next tick; a position gone meanwhile ends it at the next check.
                    self.event(json!({"what": "error", "coin": coin, "error": format!("exit: {e:#}")}));
                    // Not every 250 ms: the hold's end moved a minute on.
                    if let Some(x) = self.state.open.get_mut(&coin) {
                        x.opened += 60.0;
                    }
                }
            }
        }
    }

    /// The account's equity and positions: a position of ours gone (its stop, a liquidation) ends
    /// it; one without a stop order gets one.
    async fn reconcile(&mut self) {
        let (equity, positions) = match equity_of(&self.venue.ex, &self.venue.account, self.unified).await {
            Ok(x) => x,
            Err(e) => {
                log!("dip: account read failed: {e:#}");
                return;
            }
        };
        self.equity = equity;
        self.positions = positions;
        let mut changed = false;
        for (coin, p) in self.state.open.clone() {
            let held = self.positions.get(&coin).copied().unwrap_or(0.0);
            if held <= 0.0 {
                self.state.open.remove(&coin);
                self.event(json!({"what": "stopped", "coin": coin, "size_was": p.size, "entry_px": p.entry_px, "stop_px": p.stop_px,
                    "pnl_approx": p.size * (p.stop_px - p.entry_px)}));
                changed = true;
            } else if p.stop_oid.is_none() {
                if let Ok(o) = self.venue.stop(&coin, 1.0, p.size.min(held), p.stop_px).await {
                    if let Some(x) = self.state.open.get_mut(&coin) {
                        x.stop_oid = Some(o);
                    }
                    changed = true;
                }
            }
        }
        if changed {
            self.save();
        }
    }
}

/// An event as a chat message (None: not sent).
fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    let usd = |x: f64| if x >= 0.0 { format!("+${x:.2}") } else { format!("-${:.2}", -x) };
    Some(match ev["what"].as_str()? {
        "open" => format!("🟢 КУПИЛ {coin}: упала на {:.1}% (BTC {:+.1}%)
цена {}, на ${:.0} ({:.0}% от депозита ${:.0}), стоп {}",
            f("fall") * 100.0, f("btc") * 100.0, f("px"), f("notional"), f("notional") / f("equity").max(1e-9) * 100.0, f("equity"), f("stop_px")),
        "close" => format!("{} ПРОДАЛ {coin} через {:.0} мин: {} -> {} ({:+.2}%), {}",
            if f("pnl_approx") >= 0.0 { "✅" } else { "🔻" }, f("held_min"), f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_approx"))),
        "stopped" => format!("🛑 СТОП {coin}: {} -> {}, примерно {}", f("entry_px"), f("stop_px"), usd(f("pnl_approx"))),
        "error" => format!("⚠️ {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        "signal" => format!("👀 сигнал (без сделки) {coin}: упала на {:.1}% (BTC {:+.1}%), цена {}, купил бы на ${:.0}", f("fall") * 100.0, f("btc") * 100.0, f("mid"), f("notional")),
        "skipped" => format!("⏭ {coin} упала на {:.1}%, не купил: {}", f("fall") * 100.0, match ev["why"].as_str().unwrap_or("") {
            "at most open" => "уже открыто максимум позиций",
            "held" => "уже держим эту монету",
            "cooldown" => "по этой монете пауза после прошлой покупки",
            "the account holds it otherwise" => "на аккаунте уже есть другая позиция в этой монете",
            "equity not read yet" => "депозит ещё не прочитан",
            x => x,
        }),
        "too small" => format!("⏭ {coin}: размер меньше минимума $10 (депозит ${:.0})", f("equity")),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fall_over_the_window_by_the_minutes_closes() {
        let mut c = Closes::default();
        for (m, px) in [(100, 10.0), (101, 10.2), (105, 9.9), (115, 9.2), (116, 9.1)] {
            c.update("X", m, px, 20);
        }
        c.update("X", 116, 9.0, 20);                        // the minute's last mid is its close
        assert_eq!(c.at("X", 116), Some(9.0));
        assert_eq!(c.at("X", 110), Some(9.9));              // no update in 110: the last before
        assert!((c.change("X", 115, 15).unwrap() - (9.2 / 10.0 - 1.0)).abs() < 1e-12);
        assert_eq!(c.change("X", 110, 15), None);           // not seen 15 min back
        assert_eq!(c.change("Y", 115, 15), None);
        // Only the last `keep` minutes are kept.
        for m in 117..140 {
            c.update("X", m, 9.0, 20);
        }
        assert_eq!(c.0["X"].len(), 20);
    }

    #[test]
    fn events_read_as_messages() {
        let m = message(&json!({"what": "close", "coin": "ZEC", "entry_px": 100.0, "px": 103.0, "ret_pct": 3.0, "pnl_approx": 7.5, "held_min": 60.0})).unwrap();
        assert!(m.contains("ZEC") && m.contains("+3.00%") && m.contains("+$7.50") && m.contains("ПРОДАЛ"));
        assert!(message(&json!({"what": "stop", "coin": "ZEC"})).is_none());
    }
}
