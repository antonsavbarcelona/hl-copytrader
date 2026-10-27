//! The wick bids (`WICK_ENABLED=1`, with `LIVE_KEY` / `LIVE_ACCOUNT` / `LIVE_NET` as for the dip):
//! a resting buy `WICK_PCT` (7) under each liquid perp's anchor - the last minute's close, or its
//! oracle when lower - on `WICK_MAX_COINS` (30) of the coins with a day volume of `WICK_MIN_VOL_USD`
//! (10M) or more; on the HIP-3 dexes margined in USDC `WICK_H3_PCT` (4) under the last minute's close
//! on `WICK_H3_MAX_COINS` (10; 0: none) with `WICK_H3_MIN_VOL_USD` (1M)+ (hyna, the crypto copies, left
//! out). Which coins (every 5 min): the most restless - the biggest standard deviation of the 1-minute
//! returns over the last 24 h (the main dex's mids every 5 s, every HIP-3 dex's every 15 s; after a
//! start 24 h of 1-minute candles read, a coin a minute; a coin with under 6 h of them after the
//! others, by volume); `WICK_PICK=volume`: the biggest day volume (the HIP-3 dexes bid on every 5 s). A bid moves when its level drifts `WICK_REPRICE_PCT` (2) or more
//! (the address's rate limit: not every minute); the crypto bids come off while BTC moves 1%+ in a
//! minute. Filled (a wick through it): a resting stop `WICK_STOP_PCT` (20) under and a resting take
//! `WICK_TAKE_PCT` (5) over the fill, out at market `WICK_HOLD_MIN` (60) later; sized so that the
//! stop loses `WICK_RISK_USD` ($20; `FAMILY_RISK_PCT` of the equity when set); at most
//! `WICK_MAX_OPEN` (2) crypto and `WICK_H3_MAX_OPEN` (2) HIP-3 trades and the family's caps (a side's
//! bids off while it is full; all off when the family's open risk is); a coin not bid again for
//! 60 min after its fill; a coin the account or another strategy holds is not bid.
//!
//! The archive research of 2026-10-25/26 (the raw tape; a fill when trades go 10 bp through the
//! bid; maker in, the take maker, else taker out): crypto 8% $10M+ n 188 +328 bp a trade, halves
//! +368 / +275; HIP-3 5% $1M+ n 309 +257 bp, t 7.4, 12 of 12 months up. Resting bids tie up margin
//! (the HIP-3 ones isolated at 3x: a third of each bid's notional).
//! The levels 2026-10-26 (the tape, the bot's trading at $20 risk, 2 slots): crypto 7% 13 trades a month
//! +234 bp, $513 / 16 months (8%: 10, +273 bp, $428; 6% fails its first half); HIP-3 4% 43 a month
//! +158 bp, $747 / 11 months, 12 of 12 months up (5%: 28, +257 bp, $784): more trades, the same $.
//! The pick (2026-10-27, the tape, the 24 h deviation known an hour before the fill): of the HIP-3 4%
//! fills the top 5 by volume held 5% ($40 / 11 months), the top 5 by the deviation 69% ($589), 10 79%
//! ($615); crypto 7%: the top 20 by volume 60% ($328 / 16 months), by the deviation 80% ($434), 30 93%
//! ($475). A bid holds its margin: ~$10 a crypto one (10x), ~$41 a HIP-3 one, at $20 risk.
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
/// The minutes of closes kept a coin (24 h), the fewest returns its deviation is read from (6 h).
const HIST: usize = 1441;
const MIN_HIST: usize = 360;
/// A coin's 24 h of 1-minute candles read at most this often, and every HIP-3 dex's mids (the
/// deviation picking from all of them) this often: the API's weight (the HIP-3 dip reads them too).
const BACKFILL_EVERY_S: f64 = 60.0;
const H3_MIDS_EVERY_S: f64 = 15.0;

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
    h3_max_open: usize,
    /// The trial (`WICK_ENABLED` unset): each side's live trades at most.
    trial: Option<u32>,
    /// The coins picked by the 24 h deviation (else by the day volume).
    by_spread: bool,
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { pct: v("WICK_PCT", 7.0) / 100.0, min_vol: v("WICK_MIN_VOL_USD", 10e6), max_coins: v("WICK_MAX_COINS", 30.0) as usize,
            h3_pct: v("WICK_H3_PCT", 4.0) / 100.0, h3_min_vol: v("WICK_H3_MIN_VOL_USD", 1e6), h3_max_coins: v("WICK_H3_MAX_COINS", 10.0) as usize,
            reprice: v("WICK_REPRICE_PCT", 2.0) / 100.0, take: v("WICK_TAKE_PCT", 5.0) / 100.0, stop: v("WICK_STOP_PCT", 20.0) / 100.0,
            hold_s: v("WICK_HOLD_MIN", 60.0) * 60.0, risk_usd: v("WICK_RISK_USD", 20.0), max_open: v("WICK_MAX_OPEN", 2.0) as usize,
            h3_max_open: v("WICK_H3_MAX_OPEN", 2.0) as usize, trial: trades::switch("WICK_ENABLED").flatten(),
            by_spread: std::env::var("WICK_PICK").map(|x| x.trim() != "volume").unwrap_or(true) }
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
    /// The fills on each side (the trial's counts).
    #[serde(default)]
    opened: u32,
    #[serde(default)]
    h3_opened: u32,
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

/// The coins to bid: of those with a volume of `min_vol`+ (left out: those `skip` says), the top `n`
/// by `score` (the coins without one after, by volume).
fn pick(volume: &HashMap<String, f64>, min_vol: f64, n: usize, skip: impl Fn(&str) -> bool, score: impl Fn(&str) -> Option<f64>) -> Vec<String> {
    let mut v: Vec<(&String, Option<f64>, f64)> = volume.iter().filter(|(c, x)| **x >= min_vol && !skip(c)).map(|(c, x)| (c, score(c), *x)).collect();
    v.sort_by(|a, b| match (a.1, b.1) {
        (Some(x), Some(y)) => y.total_cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.2.total_cmp(&a.2),
    });
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
    /// The coins bid on (crypto, HIP-3), picked every `VOLUME_EVERY_S`; the coins whose candles were read.
    picked: Vec<String>,
    h3_picked: Vec<String>,
    backfilled: HashSet<String>,
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
        let n_h3 = self.state.open.iter().filter(|x| x.coin.contains(':')).count();
        let n_crypto = self.state.open.len() - n_h3;
        let risk = trades::risk_usd(self.p.risk_usd);
        let t = now();
        let free = |c: &str| {
            self.held.get(c).is_none_or(|x| *x == 0.0)
                && self.state.last_fill.get(c).is_none_or(|x| t - x >= AGAIN_AFTER_S)
                && trades::can_open(c, risk)
        };
        let btc_calm = match (self.closes.at("BTC", i64::MAX), self.closes.at("BTC", minute - 1)) {
            (Some(a), Some(b)) if b > 0.0 => (a / b - 1.0).abs() < BTC_MAX_MINUTE,
            _ => false,
        };
        if btc_calm && n_crypto < self.p.max_open && trades::trial_open(self.p.trial, self.state.opened) {
            for c in self.picked.clone() {
                let (Some(close), o) = (self.closes.at(&c, minute - 1), self.oracle.get(&c).copied().unwrap_or(0.0)) else { continue };
                if !free(&c) {
                    continue;
                }
                let anchor = if o > 0.0 { close.min(o) } else { close };
                out.insert(c, level(anchor, self.p.pct));
            }
        }
        if self.p.h3_max_coins > 0 && n_h3 < self.p.h3_max_open && trades::trial_open(self.p.trial, self.state.h3_opened) {
            for c in self.h3_picked.clone() {
                let Some(close) = self.closes.at(&c, minute - 1) else { continue };
                if free(&c) {
                    out.insert(c, level(close, self.p.h3_pct));
                }
            }
        }
        out
    }

    /// The coins to bid on now: crypto and HIP-3.
    fn repick(&mut self, minute: i64) {
        let closes = &self.closes;
        let by_spread = self.p.by_spread;
        let spread = |c: &str| if by_spread { closes.spread(c, minute - 1, HIST as i64 - 1, MIN_HIST) } else { None };
        let coins = &self.venue.coins;
        self.picked = pick(&self.volume, self.p.min_vol, self.p.max_coins, |c| c == "BTC" || !coins.contains_key(c), spread);
        self.h3_picked = if self.p.h3_max_coins > 0 {
            pick(&self.h3_volume, self.p.h3_min_vol, self.p.h3_max_coins, |c| c.starts_with("hyna:") || !coins.contains_key(c), spread)
        } else {
            Vec::new()
        };
    }

    /// One coin's 24 h of 1-minute closes (the liquid ones without 6 h of them, the biggest first).
    async fn backfill(&mut self) {
        let (coins, closes, done) = (&self.venue.coins, &self.closes, &self.backfilled);
        let short = |c: &String| !done.contains(c) && coins.contains_key(c) && closes.0.get(c).is_none_or(|q| q.len() < MIN_HIST);
        let mut todo: Vec<(&String, &f64)> = self.volume.iter().filter(|(c, x)| **x >= self.p.min_vol && c.as_str() != "BTC" && short(c))
            .chain(self.h3_volume.iter().filter(|(c, x)| **x >= self.p.h3_min_vol && !c.starts_with("hyna:") && short(c))).collect();
        todo.sort_by(|a, b| b.1.total_cmp(a.1));
        let Some(c) = todo.first().map(|x| x.0.clone()) else { return };
        self.backfilled.insert(c.clone());
        match self.api.minute_closes(&c, now() - HIST as f64 * 60.0).await {
            Ok(v) => self.closes.backfill(&c, &v, HIST),
            Err(e) => log!("wick: {c} candles not read: {e:#}"),
        }
    }

    /// The prices of the main dex and, with `h3`, of the HIP-3 dexes: by the deviation every one
    /// (the coins not bid on too), else those bid on.
    async fn read_mids(&mut self, minute: i64, h3: bool) {
        let mut dexes: HashSet<String> = HashSet::from([String::new()]);
        if h3 && self.p.h3_max_coins > 0 {
            if self.p.by_spread {
                dexes.extend(self.venue.coins.keys().filter(|c| !c.starts_with("hyna:")).filter_map(|c| c.split_once(':').map(|d| d.0.to_string())));
            } else {
                dexes.extend(self.h3_picked.iter().map(|c| dex_of(c).to_string()));
            }
        }
        for d in dexes {
            match self.api.mids(&d).await {
                Ok(m) => {
                    for (c, px) in m {
                        self.closes.update(&c, minute, px, HIST);
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
        let (side, n) = if coin.contains(':') {
            self.state.h3_opened += 1;
            ("HIP-3", self.state.h3_opened)
        } else {
            self.state.opened += 1;
            ("крипта", self.state.opened)
        };
        if !trades::trial_open(self.p.trial, n) {
            crate::notify::send(format!("🏁 проколы ({side}): проба окончена ({n} сделки), лимитки сняты"));
        }
        let pct = if coin.contains(':') { self.p.h3_pct } else { self.p.pct };
        match trades::protect(&self.venue, coin, got, px, self.p.stop, Some(self.p.take), self.p.hold_s).await {
            Ok(tr) => {
                self.event(json!({"what": "open", "coin": coin, "pct": pct, "px": px, "size": got, "notional": got * px, "take_px": tr.take_px,
                    "stop_px": tr.stop_px, "hold_min": self.p.hold_s / 60.0}));
                trades::register(coin, "wick", tr.risk());
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
    if trades::switch("WICK_ENABLED").is_none() {
        log!("wick: off (WICK_ENABLED=0)");
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
        trades::register(&tr.coin, "wick", tr.risk());
    }
    log!("wick: account {}: crypto {:.0}% under on {} coins (${:.0}M+), HIP-3 {:.0}% under on {} (${:.0}M+); take {:.0}%, stop {:.0}%, {:.0} min, risk {}, at most {} + {}; picked by {}; {} held",
        venue.account, p.pct * 100.0, p.max_coins, p.min_vol / 1e6, p.h3_pct * 100.0, p.h3_max_coins, p.h3_min_vol / 1e6, p.take * 100.0, p.stop * 100.0,
        p.hold_s / 60.0, trades::sizing_label(p.risk_usd, p.stop), p.max_open, p.h3_max_open, if p.by_spread { "the 24 h deviation" } else { "volume" }, state.open.len());
    crate::notify::send(format!("🎣 проколы лимиток НА ДЕНЬГИ: крипта лимитка на {:.0}% ниже на {} монетах (оборот от ${:.0}M), HIP-3 на {:.0}% ниже на {} монетах; тейк +{:.0}%, стоп {:.0}%, выход через {:.0} мин, риск {}, до {} + {} позиций (крипта + HIP-3); монеты {}",
        p.pct * 100.0, p.max_coins, p.min_vol / 1e6, p.h3_pct * 100.0, p.h3_max_coins, p.take * 100.0, p.stop * 100.0, p.hold_s / 60.0,
        trades::sizing_label(p.risk_usd, p.stop), p.max_open, p.h3_max_open,
        if p.by_spread { "самые дёрганые за сутки (по разбросу минутных движений)" } else { "с наибольшим оборотом" })
        + &trades::trial_label(p.trial, state.opened.max(state.h3_opened)).replace("сделки", "сделки на каждой стороне"));
    let mut w = Wick { venue, max_lev, unified, api, cfg: cfg.clone(), store, p, state, closes: Closes::default(), oracle: HashMap::new(),
        volume: HashMap::new(), h3_volume: HashMap::new(), held: HashMap::new(), lev_set: HashSet::new(),
        picked: Vec::new(), h3_picked: Vec::new(), backfilled: HashSet::new() };
    tokio::spawn(async move {
        let (mut vol_at, mut oracle_at, mut orders_at, mut backfill_at, mut h3_at) = (0.0, 0.0, 0.0, 0.0, 0.0);
        let mut last_minute = (now() / 60.0).floor() as i64;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs_f64(POLL_S));
        loop {
            tick.tick().await;
            let t = now();
            let minute = (t / 60.0).floor() as i64;
            let repick = t - vol_at >= VOLUME_EVERY_S;
            if repick {
                w.read_ctx(true).await;
                vol_at = t;
                oracle_at = t;
            } else if t - oracle_at >= ORACLE_EVERY_S {
                w.read_ctx(false).await;
                oracle_at = t;
            }
            let h3 = !w.p.by_spread || t - h3_at >= H3_MIDS_EVERY_S;
            w.read_mids(minute, h3).await;
            if h3 {
                h3_at = t;
            }
            if repick {
                w.repick(minute);
            }
            if w.p.by_spread && t - backfill_at >= BACKFILL_EVERY_S {
                w.backfill().await;
                backfill_at = t;
            }
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
        assert_eq!(pick(&vol, 10e6, 5, |c| c.starts_with("hyna:"), |_| None), vec!["A".to_string(), "B".to_string()]);
        assert_eq!(pick(&vol, 10e6, 1, |_| false, |_| None), vec!["hyna:X".to_string()]);
        // By the score first (B the more restless), the coins without one after by volume.
        let score = |c: &str| match c { "B" => Some(0.02), "hyna:X" => Some(0.01), _ => None };
        assert_eq!(pick(&vol, 10e6, 3, |_| false, score), vec!["B".to_string(), "hyna:X".to_string(), "A".to_string()]);
    }

    #[test]
    fn the_deviation_and_the_candles() {
        let mut cl = Closes::default();
        // 400 minutes alternating 100 / 101: returns +1% / -0.99%.
        let older: Vec<(i64, f64)> = (0..400).map(|m| (m, if m % 2 == 0 { 100.0 } else { 101.0 })).collect();
        cl.update("A", 400, 100.0, HIST);
        cl.backfill("A", &older, HIST);
        assert_eq!(cl.0["A"].len(), 401);
        let sd = cl.spread("A", 400, 1440, MIN_HIST).unwrap();
        assert!((sd - 0.00995).abs() < 1e-4, "{sd}");
        assert!(cl.spread("A", 400, 100, MIN_HIST).is_none());       // under 360 returns in the window
        cl.backfill("A", &[(5, 1.0)], HIST);                            // not older than what it has: left out
        assert_eq!(cl.0["A"].front(), Some(&(0, 100.0)));
        assert_eq!(dex_of("xyz:TSLA"), "xyz");
        assert_eq!(dex_of("SOL"), "");
        let m = message(&json!({"what": "close", "coin": "SOL", "why": "take", "entry_px": 92.0, "px": 96.6, "ret_pct": 5.0, "pnl_usd": 5.0})).unwrap();
        assert!(m.contains("ТЕЙК") && m.contains("+$5.00"));
    }
}
