//! The wick bids (`WICK_ENABLED=1`, with `LIVE_KEY` / `LIVE_ACCOUNT` / `LIVE_NET` as for the dip):
//! a resting buy `WICK_PCT` (8) under each liquid perp's anchor - the last minute's close, or its
//! oracle when lower - on the `WICK_MAX_COINS` (20) coins with the most day volume of
//! `WICK_MIN_VOL_USD` (10M) or more; on the HIP-3 dexes margined in USDC `WICK_H3_PCT` (5) under the
//! last minute's close on the `WICK_H3_MAX_COINS` (5; 0: none) with `WICK_H3_MIN_VOL_USD` (1M)+ (hyna,
//! the crypto copies, left out). A bid moves when its level drifts `WICK_REPRICE_PCT` (2) or more
//! (the address's rate limit: not every minute); the crypto bids come off while BTC moves 1%+ in a
//! minute. Filled (a wick through it): a resting stop `WICK_STOP_PCT` (20) under and a resting take
//! `WICK_TAKE_PCT` (5) over the fill, out at market `WICK_HOLD_MIN` (60) later; sized so that the
//! stop loses `WICK_RISK_USD` ($20; `FAMILY_RISK_PCT` of the equity when set); at most
//! `WICK_MAX_OPEN` (3) and the family's cap (all the bids off while full); a coin not bid again for
//! 60 min after its fill; a coin the account or another strategy holds is not bid.
//!
//! The archive research of 2026-10-25/26 (the raw tape; a fill when trades go 10 bp through the
//! bid; maker in, the take maker, else taker out): crypto 8% $10M+ n 188 +328 bp a trade, halves
//! +368 / +275; HIP-3 5% $1M+ n 309 +257 bp, t 7.4, 12 of 12 months up. Resting bids tie up margin
//! (the HIP-3 ones isolated at 3x: a third of each bid's notional).
//!
//! Its state is the store's doc `wick` (the bids resting, the trades, the coins' last fills).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{Api, now, num};
use crate::config::Config;
use crate::dip::Closes;
use crate::live::{Placed, Venue};
use crate::log;
use crate::store::Store;
use crate::trades::{self, Trade};

const DOC: &str = "wick";
const POLL_S: f64 = 5.0;
const ORDERS_EVERY_S: f64 = 15.0;
const VOLUME_EVERY_S: f64 = 300.0;
const ORACLE_EVERY_S: f64 = 60.0;
const AGAIN_AFTER_S: f64 = 3600.0;
const BTC_MAX_MINUTE: f64 = 0.01;

#[derive(Clone, Debug)]
struct Params {
    pct: f64,
    min_vol: f64,
    max_coins: usize,
    h3_pct: f64,
    h3_min_vol: f64,
    h3_max_coins: usize,
    reprice: f64,
    take: f64,
    stop: f64,
    hold_s: f64,
    risk_usd: f64,
    max_open: usize,
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { pct: v("WICK_PCT", 8.0) / 100.0, min_vol: v("WICK_MIN_VOL_USD", 10e6), max_coins: v("WICK_MAX_COINS", 20.0) as usize,
            h3_pct: v("WICK_H3_PCT", 5.0) / 100.0, h3_min_vol: v("WICK_H3_MIN_VOL_USD", 1e6), h3_max_coins: v("WICK_H3_MAX_COINS", 5.0) as usize,
            reprice: v("WICK_REPRICE_PCT", 2.0) / 100.0, take: v("WICK_TAKE_PCT", 5.0) / 100.0, stop: v("WICK_STOP_PCT", 20.0) / 100.0,
            hold_s: v("WICK_HOLD_MIN", 60.0) * 60.0, risk_usd: v("WICK_RISK_USD", 20.0), max_open: v("WICK_MAX_OPEN", 3.0) as usize }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Bid {
    oid: u64,
    px: f64,
    size: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    bids: HashMap<String, Bid>,
    #[serde(default)]
    open: Vec<Trade>,
    #[serde(default)]
    last_fill: HashMap<String, f64>,
}

/// A coin's dex ("" for the main one).
fn dex_of(coin: &str) -> &str {
    coin.split_once(':').map_or("", |d| d.0)
}

/// The bid's level for an anchor: `pct` under it.
fn level(anchor: f64, pct: f64) -> f64 {
    anchor * (1.0 - pct)
}

/// Whether a resting bid at `old` should move to `new`.
fn drifted(old: f64, new: f64, reprice: f64) -> bool {
    old <= 0.0 || (new / old - 1.0).abs() >= reprice
}

/// The coins to bid: the top `n` by volume of `min_vol`+ (left out: those `skip` says).
fn pick(volume: &HashMap<String, f64>, min_vol: f64, n: usize, skip: impl Fn(&str) -> bool) -> Vec<String> {
    let mut v: Vec<(&String, &f64)> = volume.iter().filter(|(c, x)| **x >= min_vol && !skip(c)).collect();
    v.sort_by(|a, b| b.1.total_cmp(a.1));
    v.into_iter().take(n).map(|x| x.0.clone()).collect()
}

fn usd(x: f64) -> String {
    if x >= 0.0 { format!("+${x:.2}") } else { format!("-${:.2}", -x) }
}

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    Some(match ev["what"].as_str()? {
        "open" => format!("🎣 ПРОКОЛ {coin}: лимитка {:.0}% ниже исполнилась по {}, на ${:.0}, тейк {} / стоп {}, выход через {:.0} мин",
            f("pct") * 100.0, f("px"), f("notional"), f("take_px"), f("stop_px"), f("hold_min")),
        "close" => format!("{} прокол {} {coin}: {} -> {} ({:+.2}%, {})", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            match ev["why"].as_str().unwrap_or("") { "stop" => "СТОП", "take" => "ТЕЙК", _ => "ЗАКРЫЛ по времени" }, f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_usd"))),
        "error" => format!("⚠️ прокол {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        _ => return None,
    })
}

struct Wick {
    venue: Venue,
    max_lev: HashMap<String, u32>,
    unified: bool,
    api: Api,
    cfg: Config,
    store: Store,
    p: Params,
    state: State,
    closes: Closes,
    oracle: HashMap<String, f64>,
    volume: HashMap<String, f64>,
    h3_volume: HashMap<String, f64>,
    held: HashMap<String, f64>,
    lev_set: HashSet<String>,
}

impl Wick {
    fn event(&self, mut ev: Value) {
        ev["kind"] = json!("wick");
        ev["at"] = json!(now());
        log!("wick: {ev}");
        if let Some(m) = message(&ev) {
            crate::notify::send(m);
        }
        self.store.event(ev);
    }

    fn save(&self) {
        let (cfg, v) = (self.cfg.clone(), serde_json::to_value(&self.state).unwrap_or_default());
        tokio::spawn(async move {
            if let Err(e) = crate::store::save_doc(&cfg, DOC, &v).await {
                log!("wick: state not saved: {e:#}");
            }
        });
    }

    /// The main dex's day volumes and oracles; the HIP-3 dexes' volumes (every `VOLUME_EVERY_S`).
    async fn read_ctx(&mut self, with_h3: bool) {
        match self.venue.ex.info(json!({"type": "metaAndAssetCtxs"})).await {
            Ok(v) => {
                let u = v[0]["universe"].as_array().cloned().unwrap_or_default();
                let c = v[1].as_array().cloned().unwrap_or_default();
                for (u, c) in u.iter().zip(c.iter()) {
                    let Some(n) = u["name"].as_str() else { continue };
                    if u["isDelisted"] == true {
                        continue;
                    }
                    self.volume.insert(n.to_string(), num(&c["dayNtlVlm"]));
                    self.oracle.insert(n.to_string(), num(&c["oraclePx"]));
                }
            }
            Err(e) => log!("wick: contexts not read: {e:#}"),
        }
        if with_h3 && self.p.h3_max_coins > 0 {
            let dexes: HashSet<&str> = self.venue.coins.keys().filter_map(|c| c.split_once(':').map(|d| d.0)).collect();
            for d in dexes {
                match self.api.dex_volumes(d).await {
                    Ok(x) => self.h3_volume.extend(x),
                    Err(e) => log!("wick: {d} volumes not read: {e:#}"),
                }
            }
        }
    }

    /// The coins to bid now and their levels.
    fn wanted(&self, minute: i64) -> HashMap<String, f64> {
        let mut out = HashMap::new();
        if self.state.open.len() >= self.p.max_open {
            return out;
        }
        let t = now();
        let free = |c: &str| {
            self.held.get(c).is_none_or(|x| *x == 0.0)
                && self.state.last_fill.get(c).is_none_or(|x| t - x >= AGAIN_AFTER_S)
                && trades::can_open(c)
        };
        let btc_calm = match (self.closes.at("BTC", i64::MAX), self.closes.at("BTC", minute - 1)) {
            (Some(a), Some(b)) if b > 0.0 => (a / b - 1.0).abs() < BTC_MAX_MINUTE,
            _ => false,
        };
        if btc_calm {
            for c in pick(&self.volume, self.p.min_vol, self.p.max_coins, |c| c == "BTC" || !self.venue.coins.contains_key(c)) {
                let (Some(close), o) = (self.closes.at(&c, minute - 1), self.oracle.get(&c).copied().unwrap_or(0.0)) else { continue };
                if !free(&c) {
                    continue;
                }
                let anchor = if o > 0.0 { close.min(o) } else { close };
                out.insert(c, level(anchor, self.p.pct));
            }
        }
        if self.p.h3_max_coins > 0 {
            for c in pick(&self.h3_volume, self.p.h3_min_vol, self.p.h3_max_coins, |c| c.starts_with("hyna:") || !self.venue.coins.contains_key(c)) {
                let Some(close) = self.closes.at(&c, minute - 1) else { continue };
                if free(&c) {
                    out.insert(c, level(close, self.p.h3_pct));
                }
            }
        }
        out
    }

    /// The prices of the main dex and of the dexes bid on.
    async fn read_mids(&mut self, minute: i64) {
        let mut dexes: HashSet<String> = HashSet::from([String::new()]);
        if self.p.h3_max_coins > 0 {
            for c in pick(&self.h3_volume, self.p.h3_min_vol, self.p.h3_max_coins, |c| c.starts_with("hyna:")) {
                dexes.insert(dex_of(&c).to_string());
            }
        }
        for d in dexes {
            match self.api.mids(&d).await {
                Ok(m) => {
                    for (c, px) in m {
                        self.closes.update(&c, minute, px, 4);
                    }
                }
                Err(e) => log!("wick: mids {d} not read: {e:#}"),
            }
        }
    }

    /// The bids put right: off where not wanted, placed where missing, moved where drifted.
    async fn place(&mut self, minute: i64) {
        let want = self.wanted(minute);
        let mut changed = false;
        for (c, b) in self.state.bids.clone() {
            if !want.contains_key(&c) {
                self.venue.cancel(&c, b.oid).await;
                self.state.bids.remove(&c);
                changed = true;
            }
        }
        for (c, lvl) in want {
            let size = trades::size_for(trades::risk_usd(self.p.risk_usd), self.p.stop, lvl);
            if let Some(b) = self.state.bids.get(&c) {
                if !drifted(b.px, lvl, self.p.reprice) {
                    continue;
                }
                self.venue.cancel(&c, b.oid).await;
                self.state.bids.remove(&c);
                changed = true;
            }
            if c.contains(':') {
                if let Err(e) = trades::h3_leverage(&self.venue, &c, self.max_lev.get(&c).copied().unwrap_or(1), &mut self.lev_set).await {
                    log!("wick: {c} leverage not set: {e:#}");
                    continue;
                }
            }
            match self.venue.limit(&c, size, lvl, false, "Alo").await {
                Ok(Placed::Resting(oid)) => {
                    self.state.bids.insert(c.clone(), Bid { oid, px: lvl, size });
                    changed = true;
                }
                Ok(Placed::Filled(got, px, _)) => self.filled(&c, got, px).await,
                // Post-only refused (the price already under the level) or no margin: next time.
                Err(e) => log!("wick: {c} bid not placed: {e:#}"),
            }
        }
        if changed {
            self.save();
        }
    }

    /// A bid filled for `got` at `px`: the trade protected (or out again).
    async fn filled(&mut self, coin: &str, got: f64, px: f64) {
        self.state.bids.remove(coin);
        self.state.last_fill.insert(coin.to_string(), now());
        let pct = if coin.contains(':') { self.p.h3_pct } else { self.p.pct };
        match trades::protect(&self.venue, coin, got, px, self.p.stop, Some(self.p.take), self.p.hold_s).await {
            Ok(tr) => {
                self.event(json!({"what": "open", "coin": coin, "pct": pct, "px": px, "size": got, "notional": got * px, "take_px": tr.take_px,
                    "stop_px": tr.stop_px, "hold_min": self.p.hold_s / 60.0}));
                trades::register(coin, "wick");
                self.state.open.push(tr);
            }
            Err(e) => {
                let _ = self.venue.market(coin, -got, true).await;
                self.event(json!({"what": "error", "coin": coin, "error": format!("the stop not placed, closed again: {e:#}")}));
            }
        }
        self.save();
    }

    /// The bids' fills (from the open orders) and the trades' ends; the account's positions.
    async fn check(&mut self) {
        let mut dexes: HashSet<String> = self.state.bids.keys().chain(self.state.open.iter().map(|x| &x.coin)).map(|c| dex_of(c).to_string()).collect();
        dexes.insert(String::new());
        let mut resting: HashMap<u64, (f64, f64)> = HashMap::new();
        let mut read: HashSet<String> = HashSet::new();
        let mut held = HashMap::new();
        for d in &dexes {
            let q = if d.is_empty() { json!({"type": "openOrders", "user": self.venue.account}) } else { json!({"type": "openOrders", "user": self.venue.account, "dex": d}) };
            let orders = match self.venue.ex.info(q).await {
                Ok(v) => v,
                Err(e) => {
                    log!("wick: open orders {d} not read: {e:#}");
                    continue;
                }
            };
            for o in orders.as_array().into_iter().flatten() {
                if let Some(oid) = o["oid"].as_u64() {
                    resting.insert(oid, (num(&o["origSz"]), num(&o["sz"])));
                }
            }
            let pos = if d.is_empty() {
                crate::live::equity_of(&self.venue.ex, &self.venue.account, self.unified).await.map(|x| {
                    trades::set_equity(x.0);
                    x.1
                })
            } else {
                self.venue.dex_positions(d).await
            };
            match pos {
                Ok(p) => {
                    held.extend(p);
                    read.insert(d.clone());
                }
                Err(e) => log!("wick: positions {d} not read: {e:#}"),
            }
        }
        self.held = held;
        // The bids: gone from the book (filled, or cancelled by the exchange) or partly filled.
        for (c, b) in self.state.bids.clone() {
            if !read.contains(dex_of(&c)) {
                continue;
            }
            match resting.get(&b.oid) {
                Some((orig, left)) if *left < *orig => {
                    self.venue.cancel(&c, b.oid).await;
                    let f = self.venue.filled(b.oid).await.map(|x| x.0).unwrap_or(orig - left);
                    self.filled(&c, f, b.px).await;
                }
                Some(_) => {}
                None => {
                    let f = self.venue.filled(b.oid).await.map(|x| x.0).unwrap_or(0.0);
                    if f > 0.0 {
                        self.filled(&c, f, b.px).await;
                    } else {
                        self.state.bids.remove(&c);
                        self.save();
                    }
                }
            }
        }
        // The trades: out on time; gone (the stop or the take filled).
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
            } else if read.contains(dex_of(&tr.coin)) {
                trades::gone(&self.venue, &tr, self.held.get(&tr.coin).copied().unwrap_or(0.0)).await
            } else {
                None
            };
            match out {
                Some((why, px)) => {
                    let ret = tr.ret(px);
                    self.event(json!({"what": "close", "coin": tr.coin, "why": why, "entry_px": tr.entry_px, "px": px, "ret_pct": ret * 100.0,
                        "pnl_usd": ret * tr.size.abs() * tr.entry_px, "held_min": (t - tr.at) / 60.0}));
                    trades::unregister(&tr.coin);
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

/// Starts the wick bids if `WICK_ENABLED` is "1" (see the module).
pub async fn start(cfg: &Config, store: Store, api: Api) -> Result<()> {
    if std::env::var("WICK_ENABLED").unwrap_or_default().trim() != "1" {
        log!("wick: off (WICK_ENABLED=1 to trade it)");
        return Ok(());
    }
    let Some((core, unified)) = crate::live::venue_from_env(None).await? else {
        log!("wick: LIVE_KEY / LIVE_ACCOUNT not set, off");
        return Ok(());
    };
    let p = Params::from_env();
    let (venue, max_lev) = if p.h3_max_coins > 0 {
        match core.with_hip3().await {
            Ok(x) => x,
            Err(e) => {
                log!("wick: the HIP-3 perps not read, the main dex only: {e:#}");
                (core.clone(), HashMap::new())
            }
        }
    } else {
        (core.clone(), HashMap::new())
    };
    let mut state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    // The bids a restart left: off (placed again at the levels now).
    for (c, b) in std::mem::take(&mut state.bids) {
        if venue.coins.contains_key(&c) {
            venue.cancel(&c, b.oid).await;
        }
    }
    for tr in &state.open {
        trades::register(&tr.coin, "wick");
    }
    log!("wick: account {}: crypto {:.0}% under on {} coins (${:.0}M+), HIP-3 {:.0}% under on {} (${:.0}M+); take {:.0}%, stop {:.0}%, {:.0} min, risk {}, at most {}; {} held",
        venue.account, p.pct * 100.0, p.max_coins, p.min_vol / 1e6, p.h3_pct * 100.0, p.h3_max_coins, p.h3_min_vol / 1e6, p.take * 100.0, p.stop * 100.0,
        p.hold_s / 60.0, trades::sizing_label(p.risk_usd, p.stop), p.max_open, state.open.len());
    crate::notify::send(format!("🎣 проколы лимиток НА ДЕНЬГИ: крипта лимитка на {:.0}% ниже на {} монетах (оборот от ${:.0}M), HIP-3 на {:.0}% ниже на {} монетах; тейк +{:.0}%, стоп {:.0}%, выход через {:.0} мин, риск {}, до {} позиций",
        p.pct * 100.0, p.max_coins, p.min_vol / 1e6, p.h3_pct * 100.0, p.h3_max_coins, p.take * 100.0, p.stop * 100.0, p.hold_s / 60.0,
        trades::sizing_label(p.risk_usd, p.stop), p.max_open));
    let mut w = Wick { venue, max_lev, unified, api, cfg: cfg.clone(), store, p, state, closes: Closes::default(), oracle: HashMap::new(),
        volume: HashMap::new(), h3_volume: HashMap::new(), held: HashMap::new(), lev_set: HashSet::new() };
    tokio::spawn(async move {
        let (mut vol_at, mut oracle_at, mut orders_at) = (0.0, 0.0, 0.0);
        let mut last_minute = (now() / 60.0).floor() as i64;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs_f64(POLL_S));
        loop {
            tick.tick().await;
            let t = now();
            let minute = (t / 60.0).floor() as i64;
            if t - vol_at >= VOLUME_EVERY_S {
                w.read_ctx(true).await;
                vol_at = t;
                oracle_at = t;
            } else if t - oracle_at >= ORACLE_EVERY_S {
                w.read_ctx(false).await;
                oracle_at = t;
            }
            w.read_mids(minute).await;
            if t - orders_at >= ORDERS_EVERY_S {
                w.check().await;
                orders_at = t;
            }
            // The levels move with the minutes' closes: once a minute (and at once when BTC jumps).
            if minute > last_minute || !w.state.bids.is_empty() && w.wanted(minute).len() < w.state.bids.len() {
                w.place(minute).await;
                last_minute = minute;
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_drift_and_the_pick() {
        assert!((level(100.0, 0.08) - 92.0).abs() < 1e-12);
        assert!(!drifted(92.0, 93.0, 0.02));
        assert!(drifted(92.0, 94.0, 0.02));
        assert!(drifted(0.0, 94.0, 0.02));
        let vol: HashMap<String, f64> = [("A".into(), 50e6), ("B".into(), 20e6), ("C".into(), 5e6), ("hyna:X".into(), 90e6)].into();
        assert_eq!(pick(&vol, 10e6, 5, |c| c.starts_with("hyna:")), vec!["A".to_string(), "B".to_string()]);
        assert_eq!(pick(&vol, 10e6, 1, |_| false), vec!["hyna:X".to_string()]);
        assert_eq!(dex_of("xyz:TSLA"), "xyz");
        assert_eq!(dex_of("SOL"), "");
        let m = message(&json!({"what": "close", "coin": "SOL", "why": "take", "entry_px": 92.0, "px": 96.6, "ret_pct": 5.0, "pnl_usd": 5.0})).unwrap();
        assert!(m.contains("ТЕЙК") && m.contains("+$5.00"));
    }
}
