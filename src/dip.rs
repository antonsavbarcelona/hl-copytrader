//! The dip-buy on the live account (`DIP_ENABLED=1`, with `LIVE_KEY`, `LIVE_ACCOUNT`, `LIVE_NET`
//! as for `live`): a perp with `DIP_MIN_VOL_USD` (20M) or more traded over the last 24 h whose mid
//! fell `DIP_DROP_PCT` (8) or more over the last `DIP_WINDOW_MIN` (15) minutes, by the minutes'
//! closes, while BTC's moved less than `DIP_BTC_MAX_PCT` (2) over them (the coin's own fall, not
//! the market's), is bought at market as its minute closes; out at market `DIP_HOLD_MIN` (60)
//! later, with a resting stop `DIP_STOP_PCT` (20) under our fill; sized so that the stop loses
//! `DIP_RISK_USD` ($20; 0: `DIP_RISK_PCT` (5) of the account's equity instead: 25% a trade); at most
//! `DIP_MAX_OPEN` (3) open, one a coin, a coin not again for `DIP_COOLDOWN_MIN` (240) after its
//! move of the drop or more either way (a rise, a fall on any volume: as in the research, see
//! `cooled`; from 2026-10-25, before it only after our entries) or our entry; a coin the account
//! holds otherwise (the copy's legs) is left alone. `DIP_DRY=1`: the signals only (events
//! `signal`), no orders. `DIP_RSI_MAX` (0: off, as V1): a fall is skipped when the coin's 1 h RSI14
//! (Wilder; the closes of the completed hours from the candles and the mid now) is at or above it
//! (event `skipped`, why `rsi`); the candles not read: bought as V1. The research of 2026-10-26 (the
//! tape): the dips under RSI 50 earn more a trade in every quarter, but with this module's cooldown
//! at V1's $20M and $20 risk the filter lowers the $ and raises the drawdown ($138 / DD $40 -> $107 /
//! $66 over 16 months): not for V1. It helps a $10M variant (DIP_MIN_VOL_USD=10e6: $55 / $66 -> $117 / $54).
//!
//! DIP V1: these settings are frozen as of 2026-10-20 (no further fitting on the history; the
//! live record is the independent check). Every signal is logged with the book and the fill
//! (events `open` / `skipped` / `too small` / `error`, and `fill` with the exchange's fills and fees
//! a moment later): best bid / ask, the spread, the depth on our side within 0.5 / 1 / 2%, the
//! VWAP our whole order would fill at by the book, the fill's price, the slippage against the mid,
//! the best price and that VWAP, the delays (the signal minute's end to our decision, the order's
//! round trip, the book's age); the exits the same (`close`). And the market's regime then
//! (`regime`, logged only: V1 does not use it): the share of the liquid coins ($5M+ a day, the
//! signal's coin left out) down 3%+ over 15 min (the breadth) now and 5 / 10 / 30 / 60 s before,
//! their equal-weighted 15 min return (the index), the coin's return against it, BTC's.
//!
//! The archive research of 2026-10-20 (2025-05 .. 2026-10, the raw tape: our buy at the worst
//! price of the first block a second after the signal, the taker fee both ways): +199 bp a trade
//! over 198 such falls; at these settings +85% over the 16 months without reinvestment (+125%
//! with it, as here: the size follows the equity), max drawdown 10%, 13 months of 16 up; every
//! neighbouring setting (5-8%, 5-15 min, 1-4 h) in profit, and a walk-forward choice of the
//! setting too. Fading rises the same way loses: their tails are fatal.
//!
//! The same family's other side (`PUMP_ENABLED=1`, off by default; DIP V1 above is untouched by
//! it): a perp with `PUMP_MIN_VOL_USD` (10M) or more a day, traded `PUMP_MIN_AGE_DAYS` (14)+, whose
//! mid rose `PUMP_RISE_PCT` (20) or more over the same window is shorted at market; out at market
//! `PUMP_HOLD_MIN` (120) later, no take, a resting stop `PUMP_STOP_PCT` (50) above our fill, sized
//! so that it loses `PUMP_RISK_USD` ($20; 0: `PUMP_RISK_PCT` (10) of the equity: a short of 20%); at most `PUMP_MAX_OPEN`
//! (3), a coin not again for 240 min after its move of the rise either way. The archive research
//! of 2026-10-25 (the raw tape, the short at the worst price of the first block a second after):
//! ~34 such pumps over 16 months, +6.7% a trade over 2 h, ~+47% reinvested at that risk, the
//! drawdown with the open ~16%; the worst ran +139% against (the stop's case). `PUMP_DRY=1`: the
//! signals only (event `pump_signal`, and `pump_dry_result` 2 h and 4 h later with the short's
//! result at the mid).
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
const VERSION: &str = "V1";
const TAKER_FEE: f64 = 0.00045;
const MIN_ORDER_USD: f64 = 10.0;
/// A book older than this (s) gives no close.
const BOOK_STALE_S: f64 = 30.0;
/// The day volumes are read this often (s).
const VOLUME_EVERY_S: f64 = 300.0;
/// The regime's coins: $5M+ a day; a coin down this much over 15 min counts in the breadth.
const REGIME_MIN_VOL: f64 = 5e6;
const REGIME_FALL: f64 = 0.03;

#[derive(Clone, Debug)]
struct Params {
    drop: f64,
    window: i64,
    btc_max: f64,
    min_vol: f64,
    hold_s: f64,
    stop: f64,
    risk: f64,
    /// The $ a trade's stop loses (`DIP_RISK_USD`, 20; 0: `risk` of the equity instead).
    risk_usd: f64,
    max_open: usize,
    cooldown_s: f64,
    dry: bool,
    /// `DIP_RSI_MAX` (0: off): skip a fall at or above this 1 h RSI14.
    rsi_max: f64,
    pump: PumpParams,
}

/// The $ a trade's stop loses: `usd` when set, else `pct` of the equity.
fn risk_dollars(pct: f64, usd: f64, equity: f64) -> f64 {
    if usd > 0.0 { usd } else { pct * equity }
}

/// The RSI (Wilder: the gains' and the losses' EMA at 1/n, from the first close) at the last of `closes`.
fn rsi(closes: &[f64], n: usize) -> Option<f64> {
    if closes.len() < 2 * n {
        return None;
    }
    let a = 1.0 / n as f64;
    let (mut up, mut dn) = (0.0, 0.0);
    for w in closes.windows(2) {
        let d = w[1] - w[0];
        up += a * (d.max(0.0) - up);
        dn += a * ((-d).max(0.0) - dn);
    }
    Some(100.0 - 100.0 / (1.0 + up / dn.max(1e-12)))
}

/// The risk as said in the chat and the log.
fn risk_label(pct: f64, usd: f64, stop: f64) -> String {
    if usd > 0.0 { format!("${usd:.0} на сделку (позиция ${:.0})", usd / stop) } else { format!("{:.1}% на сделку ({:.0}% депозита)", pct * 100.0, pct / stop * 100.0) }
}

/// The pump short's settings (see the module).
#[derive(Clone, Debug)]
struct PumpParams {
    enabled: bool,
    dry: bool,
    rise: f64,
    min_vol: f64,
    min_age_days: f64,
    hold_s: f64,
    stop: f64,
    risk: f64,
    /// `PUMP_RISK_USD` (20; 0: `risk` of the equity).
    risk_usd: f64,
    max_open: usize,
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        let on = |k: &str| std::env::var(k).unwrap_or_default().trim() == "1";
        Params { drop: v("DIP_DROP_PCT", 8.0) / 100.0, window: v("DIP_WINDOW_MIN", 15.0) as i64, btc_max: v("DIP_BTC_MAX_PCT", 2.0) / 100.0,
            min_vol: v("DIP_MIN_VOL_USD", 20e6), hold_s: v("DIP_HOLD_MIN", 60.0) * 60.0, stop: v("DIP_STOP_PCT", 20.0) / 100.0,
            risk: v("DIP_RISK_PCT", 5.0) / 100.0, risk_usd: v("DIP_RISK_USD", 20.0), max_open: v("DIP_MAX_OPEN", 3.0) as usize, cooldown_s: v("DIP_COOLDOWN_MIN", 240.0) * 60.0,
            dry: on("DIP_DRY"), rsi_max: v("DIP_RSI_MAX", 0.0),
            pump: PumpParams { enabled: on("PUMP_ENABLED"), dry: on("PUMP_DRY"), rise: v("PUMP_RISE_PCT", 20.0) / 100.0, min_vol: v("PUMP_MIN_VOL_USD", 10e6),
                min_age_days: v("PUMP_MIN_AGE_DAYS", 14.0), hold_s: v("PUMP_HOLD_MIN", 120.0) * 60.0, stop: v("PUMP_STOP_PCT", 50.0) / 100.0,
                risk: v("PUMP_RISK_PCT", 10.0) / 100.0, risk_usd: v("PUMP_RISK_USD", 20.0), max_open: v("PUMP_MAX_OPEN", 3.0) as usize } }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pos {
    size: f64,
    entry_px: f64,
    opened: f64,
    stop_px: f64,
    stop_oid: Option<u64>,
    /// The fall that triggered it (a fraction, negative); for a pump short, the rise (positive).
    fall: f64,
    /// A pump short (its size negative, its stop above, its own hold).
    #[serde(default)]
    pump: bool,
}

/// A pump signal of `PUMP_DRY=1`: its result is logged 2 h and 4 h on.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DryPump {
    coin: String,
    at: f64,
    mid: f64,
    rise: f64,
    /// The results logged so far (1: 2 h, 2: 2 h and 4 h).
    done: u8,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    open: HashMap<String, Pos>,
    /// A coin's last entry (unix s): the cooldown.
    last_entry: HashMap<String, f64>,
    entries: u64,
    /// A coin's last move of `drop`+ either way (the minute) that started a cooldown: see `cooled`.
    #[serde(default)]
    last_move: HashMap<String, i64>,
    /// The same for the pump short's rise (its own threshold).
    #[serde(default)]
    last_pump_move: HashMap<String, i64>,
    #[serde(default)]
    dry_pumps: Vec<DryPump>,
}

/// The archive research's cooldown (`moves.py`, the 198 falls of V1): a coin's move of the drop or
/// more either way over the window (any volume, whatever BTC did) starts `cooldown_min` in which
/// its falls are not bought; the moves inside it don't restart it. Whether `coin`, moving that
/// much at `minute`, is in one (else this move starts one).
pub(crate) fn cooled(last_move: &mut HashMap<String, i64>, coin: &str, minute: i64, cooldown_min: i64) -> bool {
    if last_move.get(coin).is_some_and(|m| minute - m < cooldown_min) {
        return true;
    }
    last_move.insert(coin.to_string(), minute);
    false
}

/// Each coin's last minutes' closes (minute index, the last mid seen in it).
#[derive(Default)]
pub(crate) struct Closes(pub(crate) HashMap<String, VecDeque<(i64, f64)>>);

impl Closes {
    pub(crate) fn update(&mut self, coin: &str, minute: i64, px: f64, keep: usize) {
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
    pub(crate) fn at(&self, coin: &str, minute: i64) -> Option<f64> {
        self.0.get(coin)?.iter().rev().find(|(m, _)| *m <= minute).map(|x| x.1)
    }

    /// The move over `w` minutes to the close of `minute` (it and the one `w` before both seen).
    pub(crate) fn change(&self, coin: &str, minute: i64, w: i64) -> Option<f64> {
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
    books: crate::ws::Books,
    /// The breadth and the index (all the liquid coins), a sample a second, the last 70 s.
    regime_hist: VecDeque<(f64, f64, f64)>,
    /// Whether a coin has traded `PUMP_MIN_AGE_DAYS`+ (its daily candles), read at its first pump.
    old_enough: HashMap<String, bool>,
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
    log!("dip: {VERSION} {}{} account {}, {} held; buys a fall of {:.0}% in {} min (BTC under {:.0}%, ${:.0}M+ a day), {:.0} min hold, stop {:.0}%, risk {}, at most {}",
        if p.dry { "DRY (signals only) " } else { "" }, if venue.ex.mainnet() { "MAINNET" } else { "testnet" }, venue.account, state.open.len(), p.drop * 100.0, p.window, p.btc_max * 100.0,
        p.min_vol / 1e6, p.hold_s / 60.0, p.stop * 100.0, risk_label(p.risk, p.risk_usd, p.stop), p.max_open);
    if std::env::var("LIVE_ENABLED").unwrap_or_default().trim() == "1" {
        log!("dip: LIVE_ENABLED is on too: the copy's coins are left alone (one position a coin on the account)");
    }
    crate::notify::send(format!("🤖 покупка падений запущена{} ({}), открыто {}
сигнал: падение {:.0}% за {} мин (BTC меньше {:.0}%, оборот от ${:.0}M), держим {:.0} мин, стоп {:.0}%, риск {}, до {} позиций",
        if p.dry { " БЕЗ СДЕЛОК (только сигналы)" } else { "" }, if venue.ex.mainnet() { "mainnet" } else { "testnet" }, state.open.len(), p.drop * 100.0, p.window,
        p.btc_max * 100.0, p.min_vol / 1e6, p.hold_s / 60.0, p.stop * 100.0, risk_label(p.risk, p.risk_usd, p.stop), p.max_open));
    let q = &p.pump;
    if q.enabled {
        log!("dip: the pump short is on{}: a rise of {:.0}% in {} min (${:.0}M+ a day, {:.0}+ days traded), {:.0} min hold, stop {:.0}%, risk {}, at most {}",
            if q.dry { " (DRY: signals only)" } else { "" }, q.rise * 100.0, p.window, q.min_vol / 1e6, q.min_age_days, q.hold_s / 60.0, q.stop * 100.0, risk_label(q.risk, q.risk_usd, q.stop), q.max_open);
        crate::notify::send(format!("🔻 шорт пампов запущен{}
сигнал: рост {:.0}% за {} мин (оборот от ${:.0}M, монете {:.0}+ дней), держим {:.0} мин, без тейка, стоп +{:.0}%, риск {}, до {} позиций",
            if q.dry { " БЕЗ СДЕЛОК (только сигналы)" } else { "" }, q.rise * 100.0, p.window, q.min_vol / 1e6, q.min_age_days, q.hold_s / 60.0, q.stop * 100.0,
            risk_label(q.risk, q.risk_usd, q.stop), q.max_open));
    } else {
        log!("dip: the pump short is off (PUMP_ENABLED=1 to run it)");
    }
    let mut d = Dip { venue, unified, cfg: cfg.clone(), store, p, state, closes: Closes::default(), volume: HashMap::new(),
        positions: HashMap::new(), equity: 0.0, books: books.clone(), regime_hist: VecDeque::new(), old_enough: HashMap::new() };
    tokio::spawn(async move {
        let mut last_minute = (now() / 60.0).floor() as i64;
        let mut vol_at = 0.0;
        let mut read_at = 0.0;
        // The day's note to the chat: alive, the equity, what it did.
        let (mut day_at, mut day_entries) = (now(), d.state.entries);
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
        // The start's health check to the chat: once the books, the volumes and the equity are in
        // (or what is missing after 2 min).
        let started = now();
        let mut checked = false;
        loop {
            tick.tick().await;
            let t = now();
            if !checked && (d.health_ready() || t - started > 120.0) {
                checked = true;
                let h = d.health();
                log!("dip: {}", h.replace('\n', " | "));
                crate::notify::send(h);
            }
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
            // The window and the regime's 15 minutes, whichever is longer.
            let keep = d.p.window.max(15) as usize + 3;
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
            if d.regime_hist.back().is_none_or(|x| t - x.0 >= 1.0) {
                let (b, i, _) = d.breadth(None, minute);
                d.regime_hist.push_back((t, b, i));
                while d.regime_hist.front().is_some_and(|x| t - x.0 > 70.0) {
                    d.regime_hist.pop_front();
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
        ev["version"] = json!(VERSION);
        log!("dip: {ev}");
        if let Some(m) = message(&ev) {
            crate::notify::send(m);
        }
        self.store.event(ev);
    }

    /// The breadth and the index now over the liquid coins (`except` left out): each coin's mid
    /// against its close 15 minutes before; and how many coins counted.
    fn breadth(&self, except: Option<&str>, minute: i64) -> (f64, f64, usize) {
        let (mut down, mut sum, mut n) = (0usize, 0.0, 0usize);
        for c in self.closes.0.keys() {
            if Some(c.as_str()) == except || self.volume.get(c).copied().unwrap_or(0.0) < REGIME_MIN_VOL {
                continue;
            }
            let (Some(now_px), Some(then)) = (self.closes.at(c, i64::MAX), self.closes.at(c, minute - 15)) else { continue };
            if then <= 0.0 { continue; }
            let r = now_px / then - 1.0;
            n += 1;
            sum += r;
            if r <= -REGIME_FALL { down += 1; }
        }
        if n == 0 { (f64::NAN, f64::NAN, 0) } else { (down as f64 / n as f64, sum / n as f64, n) }
    }

    /// The regime at a signal in `coin` (logged with it).
    fn regime(&self, coin: &str, fall: f64, btc: f64) -> Value {
        let minute = (now() / 60.0).floor() as i64;
        let (b, idx, n) = self.breadth(Some(coin), minute);
        let t = now();
        let ago = |s: f64| self.regime_hist.iter().rev().find(|x| t - x.0 >= s).map(|x| x.1);
        let f = |x: f64| if x.is_finite() { json!((x * 1e4).round() / 1e4) } else { Value::Null };
        json!({"breadth": f(b), "index_r15": f(idx), "rel_r15": f(fall - idx), "btc_r15": f(btc), "n_liquid": n,
            "breadth_5s_ago": ago(5.0).map(f), "breadth_10s_ago": ago(10.0).map(f), "breadth_30s_ago": ago(30.0).map(f), "breadth_60s_ago": ago(60.0).map(f)})
    }

    fn health_ready(&self) -> bool {
        self.equity > 0.0 && !self.volume.is_empty() && self.closes.0.len() >= 50 && self.closes.at("BTC", i64::MAX).is_some()
    }

    /// The start's check: what it sees, or what is missing.
    fn health(&self) -> String {
        let liquid = self.closes.0.keys().filter(|c| self.volume.get(*c).copied().unwrap_or(0.0) >= self.p.min_vol).count();
        let mut missing = Vec::new();
        if self.closes.0.len() < 50 { missing.push(format!("стаканы: видно только {} монет", self.closes.0.len())); }
        if self.volume.is_empty() { missing.push("обороты монет не прочитаны".to_string()); }
        if self.equity <= 0.0 { missing.push("депозит не прочитан (ключ / аккаунт?)".to_string()); }
        if self.closes.at("BTC", i64::MAX).is_none() { missing.push("нет цены BTC".to_string()); }
        let head = if missing.is_empty() { "✅ health check: всё работает" } else { "⚠️ health check: есть проблемы" };
        let mut m = format!("{head}
режим: {}{}
монет в стаканах: {}, ликвидных (от ${:.0}M): {}
депозит: ${:.2}, BTC: {}
открыто позиций: {}, первый сигнал возможен через ~{} мин (нужна история цен)",
            if self.p.dry { "только сигналы (DIP_DRY=1)" } else { "живая торговля" }, if self.venue.ex.mainnet() { ", mainnet" } else { ", TESTNET" },
            self.closes.0.len(), self.p.min_vol / 1e6, liquid, self.equity, self.closes.at("BTC", i64::MAX).map(|x| format!("{x:.0}")).unwrap_or("-".into()),
            self.state.open.len(), self.p.window + 1);
        if self.p.pump.enabled {
            m.push_str(&format!("
шорт пампов: {}", if self.p.pump.dry { "только сигналы (PUMP_DRY=1)" } else { "живая торговля" }));
        }
        for x in missing {
            m.push_str(&format!("
• {x}"));
        }
        m
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
        let decided = now();
        let delay_ms = ((decided - (minute + 1) as f64 * 60.0) * 1000.0).max(0.0) as u64;
        // Every coin's move of the drop either way, before the filters: the research's cooldown.
        let movers: Vec<String> = self.closes.0.keys()
            .filter(|c| self.closes.change(c, minute, self.p.window).is_some_and(|r| r.abs() >= self.p.drop))
            .cloned().collect();
        let cooldown_min = (self.p.cooldown_s / 60.0).round() as i64;
        let in_cooldown: std::collections::HashSet<String> = movers.into_iter()
            .filter(|c| cooled(&mut self.state.last_move, c, minute, cooldown_min)).collect();
        if !self.state.last_move.is_empty() && self.state.last_move.values().any(|m| *m == minute) {
            self.state.last_move.retain(|_, m| minute - *m < cooldown_min);
            self.save();
        }
        let mut falls: Vec<(String, f64)> = self.closes.0.keys()
            .filter(|c| self.venue.coins.contains_key(*c) && self.volume.get(*c).copied().unwrap_or(0.0) >= self.p.min_vol)
            .filter_map(|c| Some((c.clone(), self.closes.change(c, minute, self.p.window)?)))
            .filter(|(_, r)| *r <= -self.p.drop).collect();
        falls.sort_by(|a, b| a.1.total_cmp(&b.1));
        for (coin, fall) in falls {
            let t = now();
            let why = if btc.abs() >= self.p.btc_max {
                Some("btc moved")
            } else if self.state.open.values().filter(|p| !p.pump).count() >= self.p.max_open {
                Some("at most open")
            } else if self.state.open.contains_key(&coin) {
                Some("held")
            } else if in_cooldown.contains(&coin) || self.state.last_entry.get(&coin).is_some_and(|x| t - x < self.p.cooldown_s) {
                Some("cooldown")
            } else if self.positions.get(&coin).is_some_and(|x| *x != 0.0) {
                Some("the account holds it otherwise")
            } else if self.equity <= 0.0 {
                Some("equity not read yet")
            } else {
                None
            };
            if let Some(why) = why {
                let size = self.order_size(&coin);
                self.event(json!({"what": "skipped", "coin": coin, "fall": fall, "btc": btc, "why": why, "signal_delay_ms": delay_ms,
                    "book": self.book(&coin, true, size), "regime": self.regime(&coin, fall, btc)}));
                continue;
            }
            if self.p.rsi_max > 0.0 {
                match self.rsi_1h(&coin).await {
                    Some(r) if r >= self.p.rsi_max => {
                        self.event(json!({"what": "skipped", "coin": coin, "fall": fall, "btc": btc, "why": "rsi", "rsi": r, "rsi_max": self.p.rsi_max,
                            "signal_delay_ms": delay_ms}));
                        continue;
                    }
                    Some(r) => log!("dip: {coin} RSI 1h {r:.1} under {:.0}: bought", self.p.rsi_max),
                    None => log!("dip: {coin} RSI 1h not read: bought as V1"),
                }
            }
            self.enter(&coin, fall, btc, delay_ms).await;
        }
        if self.p.pump.enabled {
            self.pumps(minute, delay_ms).await;
        }
    }

    /// The pump short (see the module): the rises of the minute that closed, biggest first,
    /// shorted within its own limits (its own cooldown and count; the dip's untouched).
    async fn pumps(&mut self, minute: i64, delay_ms: u64) {
        let q = self.p.pump.clone();
        let cooldown_min = (self.p.cooldown_s / 60.0).round() as i64;
        let movers: Vec<String> = self.closes.0.keys()
            .filter(|c| self.closes.change(c, minute, self.p.window).is_some_and(|r| r.abs() >= q.rise))
            .cloned().collect();
        let in_cooldown: std::collections::HashSet<String> = movers.into_iter()
            .filter(|c| cooled(&mut self.state.last_pump_move, c, minute, cooldown_min)).collect();
        if self.state.last_pump_move.values().any(|m| *m == minute) {
            self.state.last_pump_move.retain(|_, m| minute - *m < cooldown_min);
            self.save();
        }
        let mut rises: Vec<(String, f64)> = self.closes.0.keys()
            .filter(|c| self.venue.coins.contains_key(*c) && self.volume.get(*c).copied().unwrap_or(0.0) >= q.min_vol)
            .filter_map(|c| Some((c.clone(), self.closes.change(c, minute, self.p.window)?)))
            .filter(|(_, r)| *r >= q.rise).collect();
        rises.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (coin, rise) in rises {
            let open_pumps = self.state.open.values().filter(|p| p.pump).count();
            let why = if open_pumps >= q.max_open {
                Some("at most open")
            } else if self.state.open.contains_key(&coin) {
                Some("held")
            } else if in_cooldown.contains(&coin) {
                Some("cooldown")
            } else if self.positions.get(&coin).is_some_and(|x| *x != 0.0) {
                Some("the account holds it otherwise")
            } else if self.equity <= 0.0 {
                Some("equity not read yet")
            } else if !self.old_enough(&coin, q.min_age_days).await {
                Some("too new")
            } else {
                None
            };
            if let Some(why) = why {
                let size = self.pump_size(&coin);
                self.event(json!({"what": "pump_skipped", "family": "pump", "coin": coin, "rise": rise, "why": why, "signal_delay_ms": delay_ms,
                    "book": self.book(&coin, false, size)}));
                continue;
            }
            self.enter_pump(&coin, rise, delay_ms).await;
        }
    }

    /// `coin`'s 1 h RSI14 now: the closes of its completed hours (the last 100) and its mid.
    async fn rsi_1h(&self, coin: &str) -> Option<f64> {
        let mid = self.closes.at(coin, i64::MAX)?;
        let t = now();
        let r = self.venue.ex.info(json!({"type": "candleSnapshot", "req": {"coin": coin, "interval": "1h",
            "startTime": ((t - 100.0 * 3600.0) * 1000.0) as u64, "endTime": (t * 1000.0) as u64}})).await;
        let v = match r {
            Ok(v) => v,
            Err(e) => {
                log!("dip: {coin} hour candles not read: {e:#}");
                return None;
            }
        };
        let mut closes: Vec<f64> = v.as_array()?.iter()
            .filter(|c| c["T"].as_f64().is_some_and(|end| end / 1000.0 < t))
            .filter_map(|c| c["c"].as_str()?.parse().ok()).collect();
        closes.push(mid);
        rsi(&closes, 14)
    }

    /// Whether `coin` has daily candles `days` or more back (cached once true).
    async fn old_enough(&mut self, coin: &str, days: f64) -> bool {
        if self.old_enough.get(coin) == Some(&true) {
            return true;
        }
        let t = now();
        let r = self.venue.ex.info(json!({"type": "candleSnapshot", "req": {"coin": coin, "interval": "1d",
            "startTime": ((t - (days + 10.0) * 86_400.0) * 1000.0) as u64, "endTime": (t * 1000.0) as u64}})).await;
        let ok = match r {
            Ok(v) => v.as_array().and_then(|a| a.first()).and_then(|c| c["t"].as_f64()).is_some_and(|first| first / 1000.0 <= t - days * 86_400.0),
            Err(e) => {
                log!("dip: {coin} candles not read (the pump skipped): {e:#}");
                false
            }
        };
        self.old_enough.insert(coin.to_string(), ok);
        ok
    }

    /// The pump short's size in `coin` now (positive; 0 without a mid): its stop loses `PUMP_RISK_PCT`.
    fn pump_size(&self, coin: &str) -> f64 {
        match (self.closes.at(coin, i64::MAX), self.venue.coins.get(coin)) {
            (Some(mid), Some(&(_, dec))) if self.equity > 0.0 =>
                round_size(risk_dollars(self.p.pump.risk, self.p.pump.risk_usd, self.equity) / self.p.pump.stop / mid, dec),
            _ => 0.0,
        }
    }

    async fn enter_pump(&mut self, coin: &str, rise: f64, delay_ms: u64) {
        let Some(mid) = self.closes.at(coin, i64::MAX) else { return };
        let q = self.p.pump.clone();
        let size = self.pump_size(coin);
        let book = self.book(coin, false, size);
        if size * mid < MIN_ORDER_USD {
            self.event(json!({"what": "too small", "family": "pump", "coin": coin, "equity": self.equity, "size": size, "signal_delay_ms": delay_ms, "book": book}));
            return;
        }
        if q.dry {
            self.state.dry_pumps.push(DryPump { coin: coin.to_string(), at: now(), mid, rise, done: 0 });
            self.event(json!({"what": "pump_signal", "family": "pump", "coin": coin, "rise": rise, "mid": mid, "size": size, "notional": size * mid,
                "equity": self.equity, "signal_delay_ms": delay_ms, "book": book}));
            self.save();
            return;
        }
        let t0 = std::time::Instant::now();
        let (got, px, oid) = match self.venue.market(coin, -size, false).await {
            Ok(x) if x.0 < 0.0 => x,
            Ok(_) => {
                self.event(json!({"what": "error", "family": "pump", "coin": coin, "error": "nothing filled", "signal_delay_ms": delay_ms, "book": book}));
                return;
            }
            Err(e) => {
                self.event(json!({"what": "error", "family": "pump", "coin": coin, "error": format!("{e:#}"), "signal_delay_ms": delay_ms, "book": book}));
                return;
            }
        };
        let order_ms = t0.elapsed().as_millis() as u64;
        let stop_px = px * (1.0 + q.stop);
        let stop_oid = match self.venue.stop(coin, -1.0, got.abs(), stop_px).await {
            Ok(o) => Some(o),
            Err(e) => {
                log!("dip: {coin} the pump short's stop not placed (the next check places it): {e:#}");
                None
            }
        };
        let t = now();
        self.state.open.insert(coin.to_string(), Pos { size: got, entry_px: px, opened: t, stop_px, stop_oid, fall: rise, pump: true });
        self.positions.insert(coin.to_string(), got);
        self.event(json!({"what": "pump_open", "family": "pump", "coin": coin, "rise": rise, "size": got, "px": px, "notional": got.abs() * px,
            "equity": self.equity, "stop_px": stop_px, "stop_oid": stop_oid, "oid": oid, "signal_mid": mid,
            "fee_est": got.abs() * px * TAKER_FEE, "slippage": slippage(&book, px, false, mid),
            "delays_ms": {"signal_to_decision": delay_ms, "order": order_ms, "book_age": book["age_ms"]}, "book": book}));
        self.fills(coin, oid, "pump_open");
        self.save();
    }

    /// The `PUMP_DRY=1` signals' results at the mid, one hold and two holds on.
    fn dry_results(&mut self) {
        let (t, hold) = (now(), self.p.pump.hold_s);
        let mut changed = false;
        let mut evs = Vec::new();
        for d in self.state.dry_pumps.iter_mut() {
            if d.done >= 2 || t - d.at < hold * (d.done + 1) as f64 {
                continue;
            }
            d.done += 1;
            changed = true;
            if let Some(m) = self.closes.at(&d.coin, i64::MAX) {
                evs.push(json!({"what": "pump_dry_result", "family": "pump", "coin": d.coin, "rise": d.rise, "mid_signal": d.mid, "mid": m,
                    "after_min": (t - d.at) / 60.0, "ret_pct": (1.0 - m / d.mid) * 100.0}));
            }
        }
        if changed {
            self.state.dry_pumps.retain(|d| d.done < 2);
            for ev in evs {
                self.event(ev);
            }
            self.save();
        }
    }

    /// Our order's size in `coin` now (0 without a mid).
    fn order_size(&self, coin: &str) -> f64 {
        match (self.closes.at(coin, i64::MAX), self.venue.coins.get(coin)) {
            (Some(mid), Some(&(_, dec))) if self.equity > 0.0 => round_size(risk_dollars(self.p.risk, self.p.risk_usd, self.equity) / self.p.stop / mid, dec),
            _ => 0.0,
        }
    }

    /// The book for an order of `size` to `buy` (else to sell): its best prices, spread, depth on
    /// our side, the VWAP the whole order would fill at, its age.
    fn book(&self, coin: &str, buy: bool, size: f64) -> Value {
        let books = self.books.read().unwrap();
        let Some(b) = books.get(coin) else { return Value::Null };
        snapshot(b, buy, size, now())
    }

    async fn enter(&mut self, coin: &str, fall: f64, btc: f64, delay_ms: u64) {
        let Some(mid) = self.closes.at(coin, i64::MAX) else { return };
        let size = self.order_size(coin);
        let book = self.book(coin, true, size);
        let regime = self.regime(coin, fall, btc);
        if size * mid < MIN_ORDER_USD {
            self.event(json!({"what": "too small", "coin": coin, "equity": self.equity, "size": size, "signal_delay_ms": delay_ms, "book": book, "regime": regime}));
            return;
        }
        if self.p.dry {
            // The cooldown as if bought: one signal a coin a fall.
            self.state.last_entry.insert(coin.to_string(), now());
            self.event(json!({"what": "signal", "coin": coin, "fall": fall, "btc": btc, "mid": mid, "size": size, "notional": size * mid, "equity": self.equity,
                "signal_delay_ms": delay_ms, "book": book, "regime": regime}));
            return;
        }
        let t0 = std::time::Instant::now();
        let (got, px, oid) = match self.venue.market(coin, size, false).await {
            Ok(x) if x.0 > 0.0 => x,
            Ok(_) => {
                self.event(json!({"what": "error", "coin": coin, "error": "nothing filled", "signal_delay_ms": delay_ms, "book": book, "regime": regime}));
                return;
            }
            Err(e) => {
                self.event(json!({"what": "error", "coin": coin, "error": format!("{e:#}"), "signal_delay_ms": delay_ms, "book": book, "regime": regime}));
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
        self.state.open.insert(coin.to_string(), Pos { size: got, entry_px: px, opened: t, stop_px, stop_oid, fall, pump: false });
        self.state.last_entry.insert(coin.to_string(), t);
        self.state.entries += 1;
        self.positions.insert(coin.to_string(), got);
        self.event(json!({"what": "open", "coin": coin, "fall": fall, "btc": btc, "size": got, "px": px, "notional": got * px,
            "equity": self.equity, "stop_px": stop_px, "stop_oid": stop_oid, "oid": oid, "signal_mid": mid,
            "fee_est": got * px * TAKER_FEE, "slippage": slippage(&book, px, true, mid),
            "delays_ms": {"signal_to_decision": delay_ms, "order": order_ms, "book_age": book["age_ms"]}, "book": book, "regime": regime}));
        self.fills(coin, oid, "open");
        self.save();
    }

    /// The exchange's fills of order `oid` (their price, size, fees), a moment after it: event `fill`.
    fn fills(&self, coin: &str, oid: u64, of: &'static str) {
        let (v, store, coin) = (self.venue.clone(), self.store.clone(), coin.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let since = ((now() - 600.0) * 1000.0) as u64;
            let r = v.ex.info(json!({"type": "userFillsByTime", "user": v.account, "startTime": since})).await;
            let mut ev = match r {
                Ok(list) => {
                    let fs: Vec<&Value> = list.as_array().into_iter().flatten().filter(|f| f["oid"].as_u64() == Some(oid)).collect();
                    let n = |f: &Value, k: &str| f[k].as_str().and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0);
                    let sz: f64 = fs.iter().map(|f| n(f, "sz")).sum();
                    let ntl: f64 = fs.iter().map(|f| n(f, "sz") * n(f, "px")).sum();
                    let fee: f64 = fs.iter().map(|f| n(f, "fee")).sum();
                    json!({"what": "fill", "of": of, "coin": coin, "oid": oid, "fills": fs.len(), "size": sz, "vwap": if sz > 0.0 { ntl / sz } else { 0.0 },
                        "fee": fee, "fee_bp": if ntl > 0.0 { fee / ntl * 1e4 } else { 0.0 },
                        "first_ms": fs.iter().filter_map(|f| f["time"].as_u64()).min(), "crossed": fs.iter().all(|f| f["crossed"] == true)})
                }
                Err(e) => json!({"what": "fill", "of": of, "coin": coin, "oid": oid, "error": format!("{e:#}")}),
            };
            ev["kind"] = json!("dip");
            ev["at"] = json!(now());
            ev["version"] = json!(VERSION);
            log!("dip: {ev}");
            store.event(ev);
        });
    }

    /// The positions whose hold is over: out at market, their stops cancelled.
    async fn exits(&mut self) {
        let t = now();
        if !self.state.dry_pumps.is_empty() {
            self.dry_results();
        }
        let due: Vec<(String, Pos)> = self.state.open.iter().filter(|(_, p)| t - p.opened >= if p.pump { self.p.pump.hold_s } else { self.p.hold_s })
            .map(|(c, p)| (c.clone(), p.clone())).collect();
        for (coin, p) in due {
            // A long sells into the bids; a pump short buys back from the asks.
            let book = self.book(&coin, p.pump, p.size.abs());
            let mid = self.closes.at(&coin, i64::MAX).unwrap_or(0.0);
            let t0 = std::time::Instant::now();
            match self.venue.market(&coin, -p.size, true).await {
                Ok((got, px, oid)) => {
                    let order_ms = t0.elapsed().as_millis() as u64;
                    if let Some(o) = p.stop_oid {
                        self.venue.cancel(&coin, o).await;
                    }
                    self.state.open.remove(&coin);
                    self.positions.remove(&coin);
                    let mut ev = json!({"what": "close", "why": "hold", "coin": coin, "size": got, "px": px, "entry_px": p.entry_px,
                        "pnl_approx": p.size * (px - p.entry_px), "ret_pct": (px / p.entry_px - 1.0) * 100.0 * p.size.signum(), "held_min": (t - p.opened) / 60.0,
                        "fee_est": got.abs() * px * TAKER_FEE, "slippage": slippage(&book, px, p.pump, mid), "delays_ms": {"order": order_ms, "book_age": book["age_ms"]},
                        "oid": oid, "book": book});
                    if p.pump {
                        ev["family"] = json!("pump");
                    }
                    self.event(ev);
                    self.fills(&coin, oid, "close");
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
            if gone(held, p.size) {
                self.state.open.remove(&coin);
                let mut ev = json!({"what": "stopped", "coin": coin, "size_was": p.size, "entry_px": p.entry_px, "stop_px": p.stop_px,
                    "pnl_approx": p.size * (p.stop_px - p.entry_px)});
                if p.pump {
                    ev["family"] = json!("pump");
                }
                self.event(ev);
                changed = true;
            } else if p.stop_oid.is_none() {
                if let Ok(o) = self.venue.stop(&coin, p.size.signum(), p.size.abs().min(held.abs()), p.stop_px).await {
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

/// Our position of `size` (signed) gone from the account's `held` (a stop, a liquidation, closed).
fn gone(held: f64, size: f64) -> bool {
    held * size.signum() <= 0.0
}

/// A book for an order of `size` (`buy`: against the asks): best bid / ask, the spread (bp), the
/// $ depth on our side within 0.5 / 1 / 2% of the best price, on the other side within 1%, the
/// top 5 levels of our side, the VWAP the whole order would fill at by these levels (`filled`:
/// the share of the order the levels hold), its age.
fn snapshot(b: &crate::ws::Book, buy: bool, size: f64, at: f64) -> Value {
    let (ours, theirs) = if buy { (&b.asks, &b.bids) } else { (&b.bids, &b.asks) };
    let (Some(bid), Some(ask)) = (b.bids.first().map(|x| x.0), b.asks.first().map(|x| x.0)) else { return Value::Null };
    let best = if buy { ask } else { bid };
    let within = |side: &Vec<(f64, f64)>, from: f64, pct: f64| side.iter().filter(|(px, _)| (px / from - 1.0).abs() <= pct + 1e-9).map(|(px, sz)| px * sz).sum::<f64>();
    let (mut left, mut ntl, mut levels) = (size, 0.0, 0);
    for (px, sz) in ours {
        if left <= 0.0 { break; }
        let take = left.min(*sz);
        ntl += take * px;
        left -= take;
        levels += 1;
    }
    let filled = if size > 0.0 { (size - left.max(0.0)) / size } else { 0.0 };
    let mid = (bid + ask) / 2.0;
    json!({"bid": bid, "ask": ask, "mid": mid, "spread_bp": (ask / bid - 1.0) * 1e4,
        "depth_usd": {"0.5%": within(ours, best, 0.005), "1%": within(ours, best, 0.01), "2%": within(ours, best, 0.02), "other_side_1%": within(theirs, if buy { bid } else { ask }, 0.01)},
        "top5": ours.iter().take(5).map(|(px, sz)| json!([px, sz])).collect::<Vec<_>>(),
        "order_size": size, "vwap_expected": if size - left.max(0.0) > 0.0 { ntl / (size - left.max(0.0)) } else { 0.0 }, "levels_used": levels, "filled": filled,
        "age_ms": ((at - b.time_ms as f64 / 1000.0) * 1000.0).max(0.0) as u64})
}

/// Our fill's slippage (bp, +: worse for us) against the mid at the signal, the book's best price
/// on our side and the VWAP the book promised for the whole order.
fn slippage(book: &Value, px: f64, buy: bool, signal_mid: f64) -> Value {
    let s = if buy { 1.0 } else { -1.0 };
    let bp = |r: f64| if r > 0.0 { json!((s * (px / r - 1.0) * 1e4 * 10.0).round() / 10.0) } else { Value::Null };
    let best = book[if buy { "ask" } else { "bid" }].as_f64().unwrap_or(0.0);
    json!({"vs_signal_mid": bp(signal_mid), "vs_book_mid": bp(book["mid"].as_f64().unwrap_or(0.0)), "vs_best": bp(best),
        "vs_vwap_expected": bp(book["vwap_expected"].as_f64().unwrap_or(0.0))})
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
        "close" => format!("{} {} {coin} через {:.0} мин: {} -> {} ({:+.2}%), {}",
            if f("pnl_approx") >= 0.0 { "✅" } else { "🔻" }, if ev["family"] == "pump" { "ЗАКРЫЛ ШОРТ" } else { "ПРОДАЛ" },
            f("held_min"), f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_approx"))),
        "stopped" => format!("🛑 СТОП{} {coin}: {} -> {}, примерно {}", if ev["family"] == "pump" { " ШОРТА" } else { "" }, f("entry_px"), f("stop_px"), usd(f("pnl_approx"))),
        "pump_open" => format!("🔴 ШОРТ {coin}: выросла на {:.1}% за 15 мин
цена {}, на ${:.0} ({:.0}% от депозита ${:.0}), стоп {}",
            f("rise") * 100.0, f("px"), f("notional"), f("notional") / f("equity").max(1e-9) * 100.0, f("equity"), f("stop_px")),
        "pump_signal" => format!("👀 сигнал шорта (без сделки) {coin}: выросла на {:.1}%, цена {}, шортил бы на ${:.0}", f("rise") * 100.0, f("mid"), f("notional")),
        "pump_dry_result" => format!("{} итог сигнала шорта {coin} через {:.0} мин: {} -> {} ({:+.2}% для шорта)",
            if f("ret_pct") >= 0.0 { "✅" } else { "🔻" }, f("after_min"), f("mid_signal"), f("mid"), f("ret_pct")),
        "pump_skipped" if ev["why"] == "cooldown" => return None,
        "pump_skipped" => format!("⏭ {coin} выросла на {:.1}%, не шортил: {}", f("rise") * 100.0, match ev["why"].as_str().unwrap_or("") {
            "at most open" => "уже открыто максимум шортов",
            "held" => "уже держим эту монету",
            "the account holds it otherwise" => "на аккаунте уже есть другая позиция в этой монете",
            "equity not read yet" => "депозит ещё не прочитан",
            "too new" => "монета торгуется меньше 14 дней",
            x => x,
        }),
        "error" => format!("⚠️ {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        "signal" => format!("👀 сигнал (без сделки) {coin}: упала на {:.1}% (BTC {:+.1}%), цена {}, купил бы на ${:.0}", f("fall") * 100.0, f("btc") * 100.0, f("mid"), f("notional")),
        "skipped" if ev["why"] == "btc moved" => return None,
        "skipped" => format!("⏭ {coin} упала на {:.1}%, не купил: {}", f("fall") * 100.0, match ev["why"].as_str().unwrap_or("") {
            "at most open" => "уже открыто максимум позиций",
            "held" => "уже держим эту монету",
            "cooldown" => "по этой монете пауза после прошлой покупки",
            "the account holds it otherwise" => "на аккаунте уже есть другая позиция в этой монете",
            "equity not read yet" => "депозит ещё не прочитан",
            "rsi" => return Some(format!("⏭ {coin} упала на {:.1}%, не купил: RSI 1ч {:.0} (фильтр: до {:.0})", f("fall") * 100.0, f("rsi"), f("rsi_max"))),
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
    fn the_rsi_as_in_the_research() {
        // The research's RSI (v23_ev_lib.rsi, numpy) on the same closes: 65.89306344882871.
        let c: Vec<f64> = (0..60).map(|i| 100.0 + 10.0 * (i as f64 / 3.0).sin() + i as f64 * 0.1).collect();
        assert!((rsi(&c, 14).unwrap() - 65.89306344882871).abs() < 1e-9);
        let up: Vec<f64> = (0..40).map(|i| i as f64).collect();
        assert!(rsi(&up, 14).unwrap() > 99.9);
        assert_eq!(rsi(&up[..20], 14), None);                // too few closes
        let m = message(&json!({"what": "skipped", "coin": "X", "fall": -0.09, "why": "rsi", "rsi": 63.2, "rsi_max": 50.0})).unwrap();
        assert!(m.contains("RSI 1ч 63") && m.contains("до 50"));
    }

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
    fn a_move_either_way_cools_the_coin_down_from_its_first_minute() {
        let mut last = HashMap::new();
        assert!(!cooled(&mut last, "X", 1000, 240));        // a rise: starts the cooldown
        assert!(cooled(&mut last, "X", 1001, 240));         // the fall a minute later: not bought
        assert!(cooled(&mut last, "X", 1239, 240));         // still inside, and not restarted by the moves in it
        assert!(!cooled(&mut last, "X", 1240, 240));        // 240 min after the first: a signal again
        assert_eq!(last["X"], 1240);
        assert!(!cooled(&mut last, "Y", 1001, 240));        // per coin
        // The old state (no `last_move`) still reads.
        let s: State = serde_json::from_value(json!({"open": {}, "last_entry": {}, "entries": 3})).unwrap();
        assert!(s.last_move.is_empty() && s.entries == 3);
    }

    #[test]
    fn the_book_walked_for_our_order() {
        let b = crate::ws::Book { bids: vec![(99.0, 5.0), (98.0, 5.0)], asks: vec![(100.0, 1.0), (101.0, 2.0), (103.0, 10.0)], time_ms: 1_000_000 };
        let s = snapshot(&b, true, 2.0, 1000.5);
        assert_eq!(s["levels_used"], 2);
        assert!((s["vwap_expected"].as_f64().unwrap() - 100.5).abs() < 1e-9 && s["filled"] == 1.0);
        assert!((s["depth_usd"]["1%"].as_f64().unwrap() - 302.0).abs() < 1e-9 && s["age_ms"] == 500);
        let sl = slippage(&s, 100.5, true, 100.0);
        assert_eq!(sl["vs_best"], 50.0);
        assert_eq!(sl["vs_vwap_expected"], 0.0);
        // More than the book holds: the share it fills.
        assert!((snapshot(&b, true, 26.0, 1000.0)["filled"].as_f64().unwrap() - 0.5).abs() < 1e-9);
        assert!(message(&json!({"what": "skipped", "coin": "X", "why": "btc moved"})).is_none());
    }

    #[test]
    fn the_risk_in_dollars_or_of_the_equity() {
        assert_eq!(risk_dollars(0.05, 20.0, 1000.0), 20.0);     // $20 whatever the equity
        assert_eq!(risk_dollars(0.05, 0.0, 1000.0), 50.0);      // 0: the share of the equity
        assert!((risk_dollars(0.05, 20.0, 1000.0) / 0.20 - 100.0).abs() < 1e-9);   // the dip: a $100 position at a 20% stop
        assert!(risk_label(0.10, 20.0, 0.50).contains("$40"));   // the pump short: $40 at +50%
    }

    #[test]
    fn a_pump_short_is_gone_or_held_by_its_sign() {
        assert!(!gone(-5.0, -5.0) && !gone(-2.0, -5.0));      // the short still on (part of it)
        assert!(gone(0.0, -5.0) && gone(3.0, -5.0));          // closed, or the account now long
        assert!(!gone(5.0, 5.0) && gone(0.0, 5.0));           // a dip long as before
        let p: Pos = serde_json::from_value(json!({"size": 1.0, "entry_px": 2.0, "opened": 0.0, "stop_px": 1.6, "stop_oid": null, "fall": -0.09})).unwrap();
        assert!(!p.pump);                                     // the old state: a dip long
        let m = message(&json!({"what": "pump_open", "coin": "XYZ", "rise": 0.23, "px": 1.5, "notional": 200.0, "equity": 1000.0, "stop_px": 2.25})).unwrap();
        assert!(m.contains("ШОРТ XYZ") && m.contains("23.0%") && m.contains("20%"));
        let m = message(&json!({"what": "close", "family": "pump", "coin": "XYZ", "entry_px": 1.5, "px": 1.35, "ret_pct": 10.0, "pnl_approx": 20.0, "held_min": 120.0})).unwrap();
        assert!(m.contains("ЗАКРЫЛ ШОРТ") && m.contains("+10.00%"));
        assert!(message(&json!({"what": "pump_skipped", "coin": "XYZ", "rise": 0.2, "why": "cooldown"})).is_none());
    }

    #[test]
    fn events_read_as_messages() {
        let m = message(&json!({"what": "close", "coin": "ZEC", "entry_px": 100.0, "px": 103.0, "ret_pct": 3.0, "pnl_approx": 7.5, "held_min": 60.0})).unwrap();
        assert!(m.contains("ZEC") && m.contains("+3.00%") && m.contains("+$7.50") && m.contains("ПРОДАЛ"));
        assert!(message(&json!({"what": "stop", "coin": "ZEC"})).is_none());
    }
}
