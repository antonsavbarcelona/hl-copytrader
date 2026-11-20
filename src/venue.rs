//! The account's venue: orders on one Hyperliquid account (testnet unless `LIVE_NET` is "mainnet"), set by env:
//! `LIVE_KEY` (its private key, or an API wallet's approved for it) and `LIVE_ACCOUNT` (the account's address). Without
//! them nothing is sent. `Venue` places, moves and cancels the strategies' orders (IOC limits through the mid as market
//! orders, resting stops and takes), reads the account (its equity on a unified account, its positions, an order's
//! state and fills), and keeps the address's request budget topped up for the wick's bids (`watch_rate_limit`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::exchange::round_size;
use crate::api::{now, num};
use crate::exchange::{Action, CancelWire, Exchange, MAINNET, OrderType, OrderWire, TESTNET, round_px, wire};
use crate::log;

const SLIPPAGE: f64 = 0.02;
/// An order that found nothing to match within `SLIPPAGE` (a thin book: testnet's spreads run
/// to several %) is sent once more this far through the mid.
const SLIPPAGE_RETRY: f64 = 0.08;
/// The stop order fills up to this far past its trigger.
const STOP_SLIPPAGE: f64 = 0.1;

/// The venue and what its orders need, shared by the coins' jobs (each runs on its own: one
/// coin's orders do not wait for another's).
#[derive(Clone)]
pub(crate) struct Venue {
    pub(crate) ex: Exchange,
    /// Coin -> (asset index, size decimals) on the live venue.
    pub(crate) coins: Arc<HashMap<String, (u32, u32)>>,
    /// The engine's live books (mainnet's): a mid without a request. None: read from the API.
    books: Option<crate::ws::Books>,
    /// Every coin's mid, read in the background (`MIDS_EVERY_S`): where the books have none.
    /// Our account (its orders' status).
    pub(crate) account: String,
    /// Coin -> its max leverage (the main dex's and, after `with_hip3`, the HIP-3 perps'), and the coins whose leverage
    /// this venue has set (`ensure_leverage`).
    max_lev: Arc<HashMap<String, u32>>,
    /// The leverage each coin was set to (cross at the max, or the HIP-3 isolated fallback).
    lev_set: Arc<Mutex<HashMap<String, u32>>>,
    /// The coins set isolated (the cross refused): their liquidation is the position's own (`stop_cap`).
    isolated: Arc<Mutex<std::collections::HashSet<String>>>,
}

#[cfg(test)]
impl Venue {
    /// A venue on a test server (`testkit::FakeHl`): no live books, nothing set yet.
    pub(crate) fn for_test(ex: Exchange, account: &str, coins: HashMap<String, (u32, u32)>, max_lev: HashMap<String, u32>) -> Venue {
        Venue { ex, coins: Arc::new(coins), books: None, account: account.to_string(), max_lev: Arc::new(max_lev),
            lev_set: Default::default(), isolated: Default::default() }
    }
}

impl Venue {
    pub(crate) async fn mid(&self, coin: &str) -> Result<f64> {
        if self.ex.mainnet() {
            if let Some(m) = self.books.as_ref().and_then(|b| b.read().unwrap().get(coin).and_then(crate::ws::Book::mid)) {
                return Ok(m);
            }
        }
        // A HIP-3 coin ("dex:NAME"): its dex's mids.
        let body = match coin.split_once(':') {
            Some((dex, _)) => json!({"type": "allMids", "dex": dex}),
            None => json!({"type": "allMids"}),
        };
        let mids = self.ex.info(body).await?;
        let m = num(&mids[coin]);
        if m <= 0.0 {
            bail!("no mid for {coin}");
        }
        Ok(m)
    }

    /// The coin's leverage at its max on cross, once a venue (2026-11-01, the user's call: the margin an order holds is its
    /// notional / the leverage, and on cross the liquidation is the account's, not the leverage's: the max saves margin and
    /// the risk stays the stops'). A coin the dex keeps isolated only: isolated at `trades::isolated_leverage` (10x max -> 5x), the
    /// liquidation ~15% away (V298: 0-2.6% of the HIP-3 trades reach it, each losing its margin ~ the 20% stop's loss). Not set on the main dex (an error): the order goes on at the account's
    /// leverage; a HIP-3 coin neither cross nor isolated took: the error.
    pub(crate) async fn ensure_leverage(&self, coin: &str) -> Result<()> {
        if self.lev_set.lock().unwrap().contains_key(coin) {
            return Ok(());
        }
        let Some(max) = self.max_lev.get(coin).copied().filter(|x| *x > 0) else {
            self.lev_set.lock().unwrap().insert(coin.to_string(), 1);
            return Ok(());
        };
        let mut set = max;
        if let Err(e) = self.leverage(coin, max, true).await {
            if !coin.contains(':') {
                log!("live: {coin} leverage {max}x cross not set: {e:#}");
                return Ok(());
            }
            set = crate::trades::isolated_leverage(max);
            log!("live: {coin} cross refused ({e:#}): isolated {set}x");
            if let Err(e2) = self.leverage(coin, set, false).await {
                // e.g. a position open at the old isolated leverage: the order goes on at the coin's current setting,
                // counted at the old 3x (the conservative margin) until a later venue sets it
                log!("live: {coin} isolated {set}x not set ({e2:#}): kept at its current leverage");
                self.lev_set.lock().unwrap().insert(coin.to_string(), crate::trades::H3_ISOLATED_LEVERAGE.min(max));
                self.isolated.lock().unwrap().insert(coin.to_string());
                return Ok(());
            }
            self.isolated.lock().unwrap().insert(coin.to_string());
        }
        self.lev_set.lock().unwrap().insert(coin.to_string(), set);
        Ok(())
    }

    /// The book for an order of `size` from the API (a HIP-3 coin has no live book here): `dip::snapshot`, Null when not read.
    pub(crate) async fn book_now(&self, coin: &str, buy: bool, size: f64) -> Value {
        let v = match self.ex.info(json!({"type": "l2Book", "coin": coin})).await {
            Ok(v) => v,
            Err(e) => {
                log!("live: {coin} book not read: {e:#}");
                return Value::Null;
            }
        };
        let side = |i: usize| v["levels"][i].as_array().map_or(Vec::new(), |l| l.iter().map(|x| (num(&x["px"]), num(&x["sz"]))).collect());
        let b = crate::ws::Book { bids: side(0), asks: side(1), time_ms: v["time"].as_u64().unwrap_or(0) };
        crate::dip::snapshot(&b, buy, size, now())
    }

    /// The farthest a stop may sit on the coin (a fraction of the entry): an isolated position's liquidation comes at
    /// ~ 1/L - the maintenance (half the initial margin at the max), and a stop past it never fires (the margin goes
    /// first; 2026-11-05: 5x on a 10x coin liquidates at ~-15%, the asks' / dips' stops at 20%), so the stop goes
    /// `trades::LIQ_STOP_SHARE` of the way to it (10x max at 5x: 13.5%). None: cross (the account's liquidation) or not set.
    pub(crate) fn stop_cap(&self, coin: &str) -> Option<f64> {
        if !self.isolated.lock().unwrap().contains(coin) {
            return None;
        }
        // the leverage set, or (a coin kept at its old setting, counted at 3x for the margin) the higher one it may be at
        let max = self.max_lev.get(coin).copied().unwrap_or(1).max(1);
        let l = (*self.lev_set.lock().unwrap().get(coin)?).max(crate::trades::isolated_leverage(max));
        Some(crate::trades::liq_stop(f64::from(l), f64::from(max)))
    }

    /// The leverage an order on the coin is margined at: the one set, else what it will be (a HIP-3 coin not set yet: the
    /// isolated fallback, the worst case; a crypto one: its max; unknown: 1).
    pub(crate) fn leverage_of(&self, coin: &str) -> f64 {
        if let Some(x) = self.lev_set.lock().unwrap().get(coin) {
            return f64::from(*x).max(1.0);
        }
        let max = self.max_lev.get(coin).copied().unwrap_or(1).max(1);
        f64::from(if coin.contains(':') { crate::trades::isolated_leverage(max) } else { max })
    }

    /// A market order (IOC through the mid): the size filled (signed), its average price and
    /// its order id. An opening one: the coin's leverage first (`ensure_leverage`).
    /// An opening one refused for the margin: the weakest resting orders taken off for it (1.5x its margin), then once
    /// more (2026-11-02, the user's call: the bids may hold most of the margin, the market entries come first).
    pub(crate) async fn market(&self, coin: &str, size: f64, reduce: bool) -> Result<(f64, f64, u64)> {
        match self.market_once(coin, size, reduce).await {
            Err(e) if !reduce && no_margin(&format!("{e:#}")) => {
                let need = 1.5 * size.abs() * self.mid(coin).await.unwrap_or(0.0) / self.leverage_of(coin);
                let freed = self.release(need).await;
                log!("live: {coin} no margin for the market entry: {freed:.2} of the resting orders' margin taken off (need {need:.2}), once more");
                self.market_once(coin, size, reduce).await
            }
            r => r,
        }
    }

    /// The weakest resting orders off until `need` $ of margin is freed; the margin freed.
    async fn release(&self, need: f64) -> f64 {
        let mut freed = 0.0;
        while freed < need {
            let next = {
                let mut v = RELEASE.lock().unwrap();
                if v.is_empty() { None } else { Some(v.remove(0)) }
            };
            let Some((c, oid, m)) = next else { break };
            if self.coins.contains_key(&c) {
                self.cancel(&c, oid).await;
                freed += m;
            }
        }
        freed
    }

    async fn market_once(&self, coin: &str, size: f64, reduce: bool) -> Result<(f64, f64, u64)> {
        let (asset, dec) = self.coins[coin];
        if !reduce {
            self.ensure_leverage(coin).await?;
        }
        let mid = self.mid(coin).await?;
        let buy = size > 0.0;
        let sz = round_size(size.abs(), dec);
        if sz <= 0.0 {
            bail!("{coin}: size {size} rounds to 0");
        }
        let mut last = Value::Null;
        for slip in [SLIPPAGE, SLIPPAGE_RETRY] {
            let px = round_px(mid * if buy { 1.0 + slip } else { 1.0 - slip }, dec);
            let order = OrderWire { a: asset, b: buy, p: wire(px), s: wire(sz), r: reduce, t: OrderType::Limit { tif: "Ioc".into() } };
            let r = self.ex.act(&Action::Order { orders: vec![order], grouping: "na".into() }).await?;
            let st = r["data"]["statuses"][0].clone();
            if let Some(f) = st.get("filled") {
                let got = num(&f["totalSz"]);
                return Ok((if buy { got } else { -got }, num(&f["avgPx"]), f["oid"].as_u64().unwrap_or(0)));
            }
            // Nothing within reach: once more, further through the mid; any other error is final.
            let nothing = st["error"].as_str().is_some_and(|e| e.contains("could not immediately match"));
            last = st;
            if !nothing {
                break;
            }
        }
        crate::halt::note_error(coin, &last.to_string());
        bail!("{coin} order not filled: {last}");
    }

    /// Our stop: a reduce-only stop-market order for `size` at `trigger`; its order id.
    pub(crate) async fn stop(&self, coin: &str, dir: f64, size: f64, trigger: f64) -> Result<u64> {
        let (asset, dec) = self.coins[coin];
        let trigger = round_px(trigger, dec);
        let px = round_px(trigger * (1.0 - dir * STOP_SLIPPAGE), dec);
        let order = OrderWire {
            a: asset, b: dir < 0.0, p: wire(px), s: wire(round_size(size.abs(), dec)), r: true,
            t: OrderType::Trigger { is_market: true, trigger_px: wire(trigger), tpsl: "sl".into() },
        };
        let r = self.ex.act(&Action::Order { orders: vec![order], grouping: "na".into() }).await?;
        let st = &r["data"]["statuses"][0];
        st["resting"]["oid"].as_u64().with_context(|| format!("{coin} stop not placed: {st}"))
    }

    pub(crate) async fn cancel(&self, coin: &str, oid: u64) {
        let a = self.coins[coin].0;
        if let Err(e) = self.ex.act(&Action::Cancel { cancels: vec![CancelWire { a, o: oid }] }).await {
            log!("live: cancel {coin} {oid}: {e:#}");
        }
    }

    /// A limit order for `size` (signed) at `px` (`tif` "Gtc" / "Alo" / "Ioc"): what it did. An opening one: the
    /// coin's leverage first (`ensure_leverage`).
    pub(crate) async fn limit(&self, coin: &str, size: f64, px: f64, reduce: bool, tif: &str) -> Result<Placed> {
        let (asset, dec) = self.coins[coin];
        if !reduce {
            self.ensure_leverage(coin).await?;
        }
        let sz = round_size(size.abs(), dec);
        if sz <= 0.0 {
            bail!("{coin}: size {size} rounds to 0");
        }
        let order = OrderWire { a: asset, b: size > 0.0, p: wire(round_px(px, dec)), s: wire(sz), r: reduce, t: OrderType::Limit { tif: tif.into() } };
        let r = self.ex.act(&Action::Order { orders: vec![order], grouping: "na".into() }).await?;
        placed(coin, &r["data"]["statuses"][0], size > 0.0)
    }

    /// A resting limit moved to `px` / `size` (signed) in one request ("modify", not a cancel and a new order: half
    /// the address's requests): the order's oid now. The new order goes only if the old one was still open
    /// (`always_place` off). Err: not moved (the old one filled or gone, the new one refused). A reply without the
    /// status: the order looked up among the open ones at its new price.
    pub(crate) async fn modify(&self, coin: &str, oid: u64, size: f64, px: f64, tif: &str) -> Result<u64> {
        let (asset, dec) = self.coins[coin];
        let sz = round_size(size.abs(), dec);
        if sz <= 0.0 {
            bail!("{coin}: size {size} rounds to 0");
        }
        let p = round_px(px, dec);
        let order = OrderWire { a: asset, b: size > 0.0, p: wire(p), s: wire(sz), r: false, t: OrderType::Limit { tif: tif.into() } };
        let r = match self.ex.act(&Action::Modify { oid, order }).await {
            Ok(r) => {
                MODIFY_FAILS.store(0, std::sync::atomic::Ordering::Relaxed);
                r
            }
            Err(e) => {
                // the whole action refused (not one order's status): after MODIFY_MAX_FAILS in a row the moves go back to
                // a cancel and a new order (the limit's own refusal aside)
                if !crate::exchange::rate_limited(&format!("{e:#}"))
                    && MODIFY_FAILS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 == MODIFY_MAX_FAILS {
                    log!("live: modify refused {MODIFY_MAX_FAILS} times in a row ({e:#}): moves by a cancel and a new order");
                    crate::notify::send(format!("⚠️ перестановка лимиток одним запросом не работает ({e:#}); вернулся к отмене + новой заявке"));
                }
                return Err(e);
            }
        };
        if let Some(st) = r["data"]["statuses"].get(0) {
            return match placed(coin, st, size > 0.0)? {
                Placed::Resting(o) => Ok(o),
                Placed::Filled(..) => bail!("{coin} order {oid} moved and filled at once"),
            };
        }
        // the coin's own dex: the main dex's list has no HIP-3 order (2026-11-10: the moved xyz bids not found there,
        // placed again on top of themselves)
        let dex = coin.split_once(':').map(|d| d.0).unwrap_or("");
        for i in 0..3 {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            let open = self.ex.info(json!({"type": "openOrders", "user": self.account, "dex": dex})).await?;
            let found = open.as_array().into_iter().flatten()
                .filter(|o| o["coin"] == coin && (o["side"] == "B") == (size > 0.0) && (num(&o["limitPx"]) - p).abs() <= 1e-9 * p.max(1.0))
                .filter_map(|o| o["oid"].as_u64()).max();
            if let Some(o) = found {
                return Ok(o);
            }
        }
        bail!("{coin} order {oid} moved ({r}) but not found among the open orders at {p}")
    }

    /// The account's leverage on `coin` (isolated unless `cross`).
    pub(crate) async fn leverage(&self, coin: &str, leverage: u32, cross: bool) -> Result<()> {
        let asset = self.coins[coin].0;
        self.ex.act(&Action::UpdateLeverage { asset, is_cross: cross, leverage }).await.map(|_| ())
    }

    /// The same venue with the HIP-3 perps of the dexes margined in USDC too ("dex:NAME" -> their
    /// asset ids: 100000 + the dex's index x 10000 + the coin's index in its meta) and each one's
    /// max leverage.
    pub(crate) async fn with_hip3(&self) -> Result<(Venue, HashMap<String, u32>)> {
        let dexes = self.ex.info(json!({"type": "perpDexs"})).await.context("perpDexs")?;
        let mut coins = (*self.coins).clone();
        let mut max_lev = (*self.max_lev).clone();
        for (di, d) in dexes.as_array().into_iter().flatten().enumerate() {
            let Some(name) = d["name"].as_str() else { continue };
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let meta = match self.ex.info(json!({"type": "meta", "dex": name})).await {
                Ok(m) => m,
                Err(e) => {
                    log!("live: {name} meta not read: {e:#}");
                    continue;
                }
            };
            if meta["collateralToken"].as_u64() != Some(0) {
                continue;
            }
            for (i, u) in meta["universe"].as_array().into_iter().flatten().enumerate() {
                if u["isDelisted"] == true {
                    continue;
                }
                let (Some(n), Some(dec)) = (u["name"].as_str(), u["szDecimals"].as_u64()) else { continue };
                coins.insert(n.to_string(), (100_000 + di as u32 * 10_000 + i as u32, dec as u32));
                max_lev.insert(n.to_string(), u["maxLeverage"].as_u64().unwrap_or(1) as u32);
            }
        }
        Ok((Venue { coins: Arc::new(coins), max_lev: Arc::new(max_lev.clone()), ..self.clone() }, max_lev))
    }

    /// The account's positions on a HIP-3 dex (coin -> signed size).
    pub(crate) async fn dex_positions(&self, dex: &str) -> Result<HashMap<String, f64>> {
        let st = self.ex.info(json!({"type": "clearinghouseState", "user": self.account, "dex": dex})).await?;
        Ok(st["assetPositions"].as_array().into_iter().flatten()
            .filter_map(|p| Some((p["position"]["coin"].as_str()?.to_string(), num(&p["position"]["szi"]))))
            .filter(|(_, x)| *x != 0.0).collect())
    }

    /// An order's size filled so far, and whether it still rests.
    pub(crate) async fn filled(&self, oid: u64) -> Result<(f64, bool)> {
        let r = self.ex.info(json!({"type": "orderStatus", "user": self.account, "oid": oid})).await?;
        let o = &r["order"];
        if o.is_null() {
            bail!("order {oid}: no status ({r})");
        }
        Ok((num(&o["order"]["origSz"]) - num(&o["order"]["sz"]), o["status"] == "open"))
    }

    /// The account's last close of `coin` since `since_ms` (its fills within `LAST_CLOSE_MS` of the latest closing fill):
    /// their size-weighted price and their size (None: none read). Only the last burst: on a coin two strategies share, an
    /// earlier close of the other's is not this one's (2026-11-11, INIT: the RSI dip's stop at 0.132 told at 0.1518 with
    /// the dip's close at 0.1749 seven hours before in it, -$9.47 told for -$22.8).
    pub(crate) async fn close_fills(&self, coin: &str, since_ms: i64) -> Option<(f64, f64)> {
        let r = self.ex.info(json!({"type": "userFillsByTime", "user": self.account, "startTime": since_ms})).await.ok()?;
        last_close(&r, coin)
    }

    /// An order's status ("open", "filled", "triggered", "canceled", ...) and the size filled.
    pub(crate) async fn order_state(&self, oid: u64) -> Result<(String, f64)> {
        let r = self.ex.info(json!({"type": "orderStatus", "user": self.account, "oid": oid})).await?;
        let o = &r["order"];
        if o.is_null() {
            bail!("order {oid}: no status ({r})");
        }
        Ok((o["status"].as_str().unwrap_or("").to_string(), num(&o["order"]["origSz"]) - num(&o["order"]["sz"])))
    }
}

/// The fills of one close are this close (ms) of the latest closing fill.
const LAST_CLOSE_MS: u64 = 30_000;

/// The size-weighted price and the size of `coin`'s last close in `userFillsByTime` rows (see `Venue::close_fills`).
pub(crate) fn last_close(fills: &Value, coin: &str) -> Option<(f64, f64)> {
    let rows: Vec<&Value> = fills.as_array()?.iter()
        .filter(|x| x["coin"] == coin && x["dir"].as_str().is_some_and(|d| d.starts_with("Close") || d.contains("Liquidat"))).collect();
    let last = rows.iter().filter_map(|x| x["time"].as_u64()).max()?;
    let (mut q, mut c) = (0.0, 0.0);
    for x in rows.into_iter().filter(|x| x["time"].as_u64().is_some_and(|t| t + LAST_CLOSE_MS >= last)) {
        let sz = num(&x["sz"]);
        q += sz;
        c += sz * num(&x["px"]);
    }
    (q > 0.0).then(|| (c / q, q))
}

/// What a limit order did: rests (its id), or filled at once (the size, signed, its average price).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Placed {
    Resting(u64),
    Filled(f64, f64, u64),
}

fn placed(coin: &str, st: &Value, buy: bool) -> Result<Placed> {
    if let Some(o) = st["resting"]["oid"].as_u64() {
        return Ok(Placed::Resting(o));
    }
    if let Some(f) = st.get("filled") {
        let got = num(&f["totalSz"]);
        return Ok(Placed::Filled(if buy { got } else { -got }, num(&f["avgPx"]), f["oid"].as_u64().unwrap_or(0)));
    }
    crate::halt::note_error(coin, &st.to_string());
    bail!("{coin} order not placed: {st}")
}

const MODIFY_MAX_FAILS: u32 = 3;
static MODIFY_FAILS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Whether the bids move by `Venue::modify` (one request), not by a cancel and a new order.
pub(crate) fn modify_on() -> bool {
    MODIFY_FAILS.load(std::sync::atomic::Ordering::Relaxed) < MODIFY_MAX_FAILS
}

/// Requests the wick keeps free for the exits, the stops and the takes (`exchange::headroom`): under it
/// the wick places and moves nothing (its cancels go on: they have their own, bigger limit).
pub(crate) const RATE_RESERVE: i64 = 2000;

/// The headroom a top-up aims at, and its bounds: one side's $ at most, the volume a UTC day may spend
/// on it ($20 000: ~$9 of taker fees at 0.045% at most; the requests then cost ~$0.00045 each, as the
/// `reserveRequestWeight` purchase would, which a unified account cannot pay: 2026-11-10). $6 000 until
/// 2026-11-10: a falling day's wick moves ~600-850 requests an hour, and the user's call is no bid lost.
pub(crate) const TOPUP_TARGET: i64 = RATE_RESERVE + 1500;
pub(crate) const TOPUP_SIDE_MAX_USD: f64 = 2500.0;
pub(crate) const TOPUP_DAY_MAX_USD: f64 = 20000.0;
/// The coins a top-up trades (the first the account holds no position in): the deepest books.
const TOPUP_COINS: [&str; 2] = ["BTC", "ETH"];

/// A top-up's one side in $ (bought, then sold: twice that traded, 1 request per $), None when the
/// headroom is enough or the day's volume is spent; at least $12 (the exchange's $10 minimum).
pub(crate) fn topup_side_usd(headroom: i64, spent_today: f64) -> Option<f64> {
    if headroom >= RATE_RESERVE {
        return None;
    }
    let side = ((TOPUP_TARGET - headroom) as f64 / 2.0).min(TOPUP_SIDE_MAX_USD).min((TOPUP_DAY_MAX_USD - spent_today) / 2.0);
    (side >= 12.0).then(|| side.max(12.0))
}

/// Buys `usd` of a top-up coin at market and sells it back (reduce-only, retried until the position is
/// gone: never left open); the volume traded. The coin: one the account has no position, no resting order
/// and no strategy's trade in, held in the family registry while it runs (no strategy opens a short on it
/// meanwhile); none free: skipped (0). A reply lost after the buy went through: the position read and sold.
pub(crate) async fn topup(v: &Venue, usd: f64) -> Result<f64> {
    let held = v.dex_positions("").await?;
    let resting = v.ex.info(json!({"type": "openOrders", "user": v.account})).await?;
    let busy = |c: &str| held.contains_key(c) || crate::trades::registered(c)
        || resting.as_array().into_iter().flatten().any(|o| o["coin"] == c);
    let Some(coin) = TOPUP_COINS.iter().copied().find(|c| v.coins.contains_key(*c) && !busy(c)) else { return Ok(0.0) };
    crate::trades::register(coin, TOPUP_KIND, 0.0, 1.0);
    let r = topup_on(v, coin, usd).await;
    crate::trades::unregister(coin, TOPUP_KIND);
    r
}

const TOPUP_KIND: &str = "topup";

async fn topup_on(v: &Venue, coin: &str, usd: f64) -> Result<f64> {
    let mid = v.mid(coin).await?;
    // market_once: no resting bids taken off for its margin
    let (got, px) = match v.market_once(coin, usd / mid, false).await {
        Ok((got, px, _)) => (got, px),
        Err(e) => {
            // refused, or the reply lost after it filled: what is there is ours (the coin was flat)
            let pos = v.dex_positions("").await.ok().and_then(|p| p.get(coin).copied()).unwrap_or(0.0);
            if pos == 0.0 {
                return Err(e);
            }
            log!("rate limit: top-up {coin}: the buy errored ({e:#}) but {pos} is held: sold back");
            (pos, mid)
        }
    };
    let mut last = String::new();
    for i in 0..60 {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            // a sell whose reply was lost: done when the position is
            match v.dex_positions("").await {
                Ok(p) if p.get(coin).copied().unwrap_or(0.0) <= 0.0 => return Ok(2.0 * got.abs() * px),
                _ => {}
            }
        }
        match v.market_once(coin, -got, true).await {
            Ok((sold, px2, _)) => return Ok(got.abs() * px + sold.abs() * px2),
            Err(e) => last = format!("{e:#}"),
        }
    }
    crate::notify::send(format!("🆘 добор лимита: {coin} {got} куплен и не продан ({last}) — закрой руками"));
    bail!("top-up {coin}: bought {got}, not sold back: {last}")
}

/// Re-reads the account's request headroom each minute (`userRateLimit`: an info request, outside the
/// address's limit) and tops it up with volume when it runs low (`topup`); once per process. Told to the
/// chat when it runs out and when it is back.
fn watch_rate_limit(v: Venue) {
    static ON: std::sync::Once = std::sync::Once::new();
    ON.call_once(|| {
        tokio::spawn(async move {
            let (mut low, mut day, mut spent) = (false, 0i64, 0.0);
            loop {
                let mut wait = 60;
                if let Ok(r) = v.ex.info(json!({"type": "userRateLimit", "user": v.account})).await {
                    if let (Some(used), Some(cap)) = (r["nRequestsUsed"].as_i64(), r["nRequestsCap"].as_i64()) {
                        let h = cap + r["nRequestsSurplus"].as_i64().unwrap_or(0) - used;
                        crate::exchange::set_headroom(h);
                        if (h < RATE_RESERVE) != low {
                            low = h < RATE_RESERVE;
                            log!("rate limit: headroom {h} (used {used}, cap {cap})");
                            crate::notify::send(if low {
                                format!("⚠️ лимит запросов биржи: запас {h} (< {RATE_RESERVE}). Wick на паузе, добираю оборотом")
                            } else {
                                format!("✅ лимит запросов биржи: запас {h}, wick снова ставит лимитки")
                            });
                        }
                        let today = (now() / 86400.0) as i64;
                        if today != day {
                            (day, spent) = (today, 0.0);
                        }
                        if let Some(usd) = topup_side_usd(h, spent) {
                            match topup(&v, usd).await {
                                Ok(vol) if vol > 0.0 => {
                                    spent += vol;
                                    log!("rate limit: topped up, ${vol:.0} traded (${spent:.0} today), headroom was {h}");
                                    // the cap catches up with the volume: not read again at once
                                    wait = 180;
                                }
                                Ok(_) => log!("rate limit: no top-up coin free (positions in all)"),
                                Err(e) => log!("rate limit: top-up failed: {e:#}"),
                            }
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }
        });
    });
}

/// Whether the wick may spend requests (placing, moving).
pub(crate) fn wick_may_spend() -> bool {
    crate::exchange::headroom() > RATE_RESERVE
}

/// The live account's venue from the env (`LIVE_KEY`, `LIVE_ACCOUNT`, `LIVE_NET`), for another
/// strategy on the account (`dip`): the venue and whether the account is unified; None without them.
pub(crate) async fn venue_from_env(books: Option<crate::ws::Books>) -> Result<Option<(Venue, bool)>> {
    let (Ok(key), Ok(account)) = (std::env::var("LIVE_KEY"), std::env::var("LIVE_ACCOUNT")) else { return Ok(None) };
    if key.trim().is_empty() || account.trim().is_empty() {
        return Ok(None);
    }
    let base = if std::env::var("LIVE_NET").unwrap_or_default() == "mainnet" { MAINNET } else { TESTNET };
    let ex = Exchange::new(base, &key)?;
    let account = account.trim().to_lowercase();
    let meta = ex.info(json!({"type": "meta"})).await.context("meta")?;
    let coins = meta["universe"].as_array().into_iter().flatten().enumerate()
        .filter(|(_, u)| u["isDelisted"] != true)
        .filter_map(|(i, u)| Some((u["name"].as_str()?.to_string(), (i as u32, u["szDecimals"].as_u64()? as u32))))
        .collect::<HashMap<_, _>>();
    let unified = ex.info(json!({"type": "userAbstraction", "user": account})).await.map(|v| v == "unifiedAccount").unwrap_or(false);
    let max_lev = Arc::new(max_leverage(&meta));
    let venue = Venue { ex, coins: Arc::new(coins), books, account, max_lev, lev_set: Default::default(), isolated: Default::default() };
    // mainnet only: the testnet's limits are not ours to pay
    if base == MAINNET {
        watch_rate_limit(venue.clone());
    }
    Ok(Some((venue, unified)))
}

/// A meta's coins' max leverage.
fn max_leverage(meta: &Value) -> HashMap<String, u32> {
    meta["universe"].as_array().into_iter().flatten()
        .filter_map(|u| Some((u["name"].as_str()?.to_string(), u["maxLeverage"].as_u64()? as u32))).collect()
}

/// Our account's equity: its perp account value, and on a unified account the spot USDC not
/// held as perp margin (it backs the perps too); and its positions.
/// The HIP-3 dexes margined in USDC (their meta's collateral token 0), read once (not cached when the read fails).
static USDC_DEXES: tokio::sync::OnceCell<Vec<String>> = tokio::sync::OnceCell::const_new();

/// The resting orders a market entry may take the margin of (the wick's; the weakest first): (coin, order id, margin $).
static RELEASE: Mutex<Vec<(String, u64, f64)>> = Mutex::new(Vec::new());

/// The wick's resting orders, the weakest first (each minute).
pub(crate) fn set_release(v: Vec<(String, u64, f64)>) {
    *RELEASE.lock().unwrap() = v;
}

/// An HL refusal for the margin.
pub(crate) fn no_margin(e: &str) -> bool {
    e.contains("Insufficient margin")
}

pub(crate) async fn usdc_dexes(ex: &Exchange) -> Vec<String> {
    let read = || async {
        let dexes = ex.info(json!({"type": "perpDexs"})).await?;
        let mut out = Vec::new();
        for d in dexes.as_array().into_iter().flatten() {
            let Some(name) = d["name"].as_str() else { continue };
            if ex.info(json!({"type": "meta", "dex": name})).await?["collateralToken"].as_u64() == Some(0) {
                out.push(name.to_string());
            }
        }
        anyhow::Ok(out)
    };
    match USDC_DEXES.get_or_try_init(read).await {
        Ok(v) => v.clone(),
        Err(e) => {
            log!("live: the HIP-3 dexes not read: {e:#}");
            Vec::new()
        }
    }
}

pub(crate) async fn equity_of(ex: &Exchange, account: &str, unified: bool) -> Result<(f64, HashMap<String, f64>)> {
    let st = ex.info(json!({"type": "clearinghouseState", "user": account})).await?;
    let main = num(&st["marginSummary"]["accountValue"]);
    let mut eq = main;
    if unified {
        let spot = ex.info(json!({"type": "spotClearinghouseState", "user": account})).await?;
        if let Some(b) = spot["balances"].as_array().into_iter().flatten().find(|b| b["coin"] == "USDC") {
            let hold = num(&b["hold"]);
            eq += num(&b["total"]) - hold;
            // The unified account's one USDC: the hold is every dex's account value (the main dex's and the
            // HIP-3 dexes' margined in USDC: their positions and resting bids); the HIP-3 ones added back too.
            if hold > main + 0.01 {
                for d in usdc_dexes(ex).await {
                    match ex.info(json!({"type": "clearinghouseState", "user": account, "dex": d})).await {
                        Ok(s) => eq += num(&s["marginSummary"]["accountValue"]),
                        Err(e) => log!("live: {d} account not read (the equity without it): {e:#}"),
                    }
                }
            }
        }
    }
    let positions = st["assetPositions"].as_array().into_iter().flatten()
        .filter_map(|p| Some((p["position"]["coin"].as_str()?.to_string(), num(&p["position"]["szi"]))))
        .collect();
    Ok((eq, positions))
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_close_only() {
        // 2026-11-11 INIT: the dip's close 7 h before the RSI dip's stop, both since the RSI dip's open
        let f = json!([
            {"coin": "INIT", "dir": "Open Long", "px": "0.17631", "sz": "214", "time": 1_000_000u64},
            {"coin": "INIT", "dir": "Close Long", "px": "0.17492", "sz": "585", "time": 2_000_000u64},
            {"coin": "INIT", "dir": "Close Long", "px": "0.1320", "sz": "600", "time": 27_000_000u64},
            {"coin": "INIT", "dir": "Close Long", "px": "0.1310", "sz": "86", "time": 27_000_400u64},
            {"coin": "ZEC", "dir": "Close Long", "px": "500", "sz": "1", "time": 27_000_500u64}]);
        let (px, sz) = last_close(&f, "INIT").unwrap();
        assert_eq!(sz, 686.0);
        assert!((px - (0.132 * 600.0 + 0.131 * 86.0) / 686.0).abs() < 1e-12);
        assert_eq!(last_close(&f, "BTC"), None);
    }

    /// The equity of a real unified account on mainnet (reads only): `EQUITY_ACCOUNT` = its address.
    /// `cargo test -- --ignored the_unified_equity --nocapture`
    #[tokio::test]
    #[ignore]
    async fn the_unified_equity() {
        let ex = Exchange::new(MAINNET, &format!("0x{}", "11".repeat(32))).unwrap();
        let a = std::env::var("EQUITY_ACCOUNT").expect("EQUITY_ACCOUNT");
        let (eq, held) = equity_of(&ex, &a, true).await.unwrap();
        println!("equity {eq:.2}, {} positions", held.len());
        assert!(eq > 0.0);
    }
}

#[cfg(test)]
mod reply_tests {
    use super::*;

    #[test]
    fn an_order_reply_is_read_as_resting_filled_or_an_error() {
        assert!(matches!(placed("X", &json!({"resting": {"oid": 7}}), true).unwrap(), Placed::Resting(7)));
        match placed("X", &json!({"filled": {"totalSz": "1.5", "avgPx": "10.2", "oid": 8}}), false).unwrap() {
            Placed::Filled(got, px, oid) => assert!(got == -1.5 && px == 10.2 && oid == 8),
            _ => panic!(),
        }
        let e = placed("X", &json!({"error": "Post only order would have immediately matched"}), true).unwrap_err();
        assert!(format!("{e:#}").contains("not placed"));
        assert!(placed("X", &Value::Null, true).is_err());
    }

    #[test]
    fn a_margin_refusal_is_told_apart() {
        assert!(no_margin("exchange: Insufficient margin to place order. asset=5"));
        assert!(!no_margin("Order could not immediately match against any resting orders."));
    }
}
