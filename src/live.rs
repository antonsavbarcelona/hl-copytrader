//! Real orders for the golden list on one Hyperliquid account (testnet unless `LIVE_NET` is
//! "mainnet"), set by env: `LIVE_ENABLED=1` (off without it), `LIVE_KEY` (its private key, or an
//! API wallet's approved for it) and `LIVE_ACCOUNT` (the account's address). Without them nothing
//! is sent.
//!
//! The engine passes on every move of a followed account's position (`Update`); here:
//!   - a position a golden account enters is entered on our account at a size whose stop,
//!     `stop_pct` against our fill, loses `risk_pct` of our account's equity, with a stop
//!     order resting on the exchange at that price;
//!   - it is followed (reduced in proportion as the account reduces from its peak in the
//!     position, closed when it is flat) to its end, golden or not by then;
//!   - each account's position in a coin is a leg of ours on its own: its own entry, size and
//!     stop order (reduce-only, for the leg's size: placed again when the leg's size changes),
//!     its own exits. The exchange holds their sum in the coin. A leg the other way from the
//!     legs held in the coin is not entered (one net position per coin on an account: a long
//!     and a short there would cancel out, and their stops would act on each other's size);
//!     at `max_positions` legs open none is;
//!   - every minute the exchange's positions and orders are read: a leg whose stop order is
//!     gone was stopped out; a coin's position gone there (a liquidation) ends its legs.
//!
//! Orders are IOC limits 5% through the mid (market orders, as the SDK places them). Prices are
//! the live account's venue's: on testnet its own books, not mainnet's.
//!
//! What it enters (`LIVE_PORTFOLIO`): the traders an allocator portfolio picks today, e.g.
//! `SHARPE_20_R1_D50` (the archive walk-forward of 2026-10-18), instead of the golden list; with
//! that portfolio's limits on our account: `LIVE_RISK_PCT` an entry (default `risk_pct`), the
//! open risk in all (`LIVE_CAP_TOTAL_PCT`, 100), in one direction (`LIVE_CAP_DIR_PCT`, 50), of
//! one trader (`LIVE_CAP_TRADER_PCT`, 20), two legs a coin; no entries once the account is
//! `LIVE_MAX_DD_PCT` below its peak (off at 0), its legs followed out.
//!
//! The mix of the walk-forward of 2026-10-19 (on the real-price copies, 14 months, no
//! reinvestment: SHARPE_20_R2_D20 +40% against +6% without it), each off unless set:
//!   - `LIVE_RANK_WEIGHT=1`: an entry's risk is `LIVE_RISK_PCT` times its trader's weight by his
//!     place among the portfolio's picks (`rank_weight`: the first 1.6x, the last 0.4x, the
//!     same on average); the limits count each leg's own risk;
//!   - `LIVE_EXIT_LIMIT_S` (e.g. 15): a trader's exit is followed by a post-only order at the
//!     book's best price on our side (a sell at the best ask), in the background (the coin's
//!     next moves do not wait for it); what it has not filled in that many seconds is sold at
//!     market. Flips and stops stay market.
//!
//! At every start a health check runs first: a small market buy ($15) of BTC (else ETH,
//! SOL: a coin we hold no position in), sold at once; its result goes to `live_checks`. A
//! failed check is recorded and logged; the live trader runs on regardless.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::account::round_size;
use crate::api::{now, num};
use crate::config::Config;
use crate::exchange::{Action, CancelWire, Exchange, MAINNET, OrderType, OrderWire, TESTNET, round_px, wire};
use crate::log;
use crate::store::Store;

/// Our market orders cross the mid by this much at most (the exchange's $10 minimum is
/// checked at the order's limit price, so not much more).
const SLIPPAGE: f64 = 0.02;
/// An order that found nothing to match within `SLIPPAGE` (a thin book: testnet's spreads run
/// to several %) is sent once more this far through the mid.
const SLIPPAGE_RETRY: f64 = 0.08;
/// The stop order fills up to this far past its trigger.
const STOP_SLIPPAGE: f64 = 0.1;
const MIN_ORDER_USD: f64 = 10.0;
/// A reduce that would leave less than this (USD) closes the position: a smaller one could
/// not be sold (under the minimum at the order's price).
const MIN_LEFT_USD: f64 = 11.0;
const DOC: &str = "live";
/// The start's check trades this much (USD; the exchange's minimum is $10).
const CHECK_USD: f64 = 15.0;
/// An entry this long (s) after its fill reached the engine is not taken: the price has moved
/// on (a scalper may be out already).
const ENTRY_MAX_AGE_S: f64 = 3.0;
/// How often the account's equity is read in the background (entries size on it).
const EQUITY_EVERY_S: f64 = 15.0;
/// How often every coin's mid is read in the background (the books' fallback).
const MIDS_EVERY_S: f64 = 5.0;

/// A move of a followed account's position in a coin.
#[derive(Debug)]
pub struct Update {
    pub user: String,
    pub coin: String,
    /// Its direction (+1 long, -1 short) and its size as a share of its peak in the position
    /// (0: flat).
    pub dir: f64,
    pub frac: f64,
    /// Its fills opened the position.
    pub entry: bool,
    pub golden: bool,
    /// When its fill reached the engine (unix s): a late entry is not taken.
    pub at: f64,
    /// Its trader's weight by his place among the picks (`rank_weight`; 1 without one).
    pub weight: f64,
}

/// The weight of the pick at `rank` (1: the first) of `n`: from 2 down by 1.5 over the picks, at
/// least 0.5, over its mean (the risk on average as set). The walk-forward of 2026-10-19: the
/// better a trader's place by our real-price copy's Sharpe, the better his next months.
pub fn rank_weight(rank: usize, n: usize) -> f64 {
    if n == 0 || rank == 0 || rank > n {
        return 1.0;
    }
    let w = |i: usize| (2.0 - i as f64 * 1.5 / n as f64).max(0.5);
    w(rank - 1) / ((0..n).map(w).sum::<f64>() / n as f64)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Leg {
    #[serde(default)]
    pub coin: String,
    pub user: String,
    pub dir: f64,
    /// Our size at the account's peak (set at entry), and ours now (signed).
    pub full: f64,
    pub size: f64,
    pub entry_px: f64,
    pub stop_px: f64,
    pub stop_oid: Option<u64>,
    pub opened: f64,
    /// Its stop order is being placed (in the background, after the entry's fill).
    #[serde(skip)]
    pub stop_pending: bool,
    /// The risk it got at entry (% of the account's equity; 0: saved before, `LIVE_RISK_PCT`).
    #[serde(default)]
    pub risk: f64,
    /// It is being closed by a post-only order in the background (`LIVE_EXIT_LIMIT_S`).
    #[serde(default)]
    pub closing: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// `key(coin, account)` -> our leg.
    pub legs: HashMap<String, Leg>,
    /// Entries not followed: the other way from the coin's legs, or `max_positions` open.
    pub skipped: u64,
    pub orders: u64,
    /// The account's highest equity seen at an entry (`LIVE_MAX_DD_PCT`).
    #[serde(default)]
    pub peak: f64,
}

/// A leg's key: the coin and the account it follows.
fn key(coin: &str, user: &str) -> String {
    format!("{coin}|{user}")
}

impl State {
    /// Legs saved by coin alone (one per coin, before legs per account) under their new keys.
    fn upgrade(mut self) -> Self {
        self.legs = self.legs.into_iter().map(|(k, mut l)| {
            if !k.contains('|') {
                l.coin = k;
            }
            (key(&l.coin, &l.user), l)
        }).collect();
        self
    }

    fn in_coin(&self, coin: &str) -> impl Iterator<Item = (&String, &Leg)> {
        self.legs.iter().filter(move |(_, l)| l.coin == coin)
    }
}

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
    mids: Arc<std::sync::RwLock<HashMap<String, f64>>>,
    /// Our account (its orders' status).
    pub(crate) account: String,
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

    /// A market order (IOC through the mid): the size filled (signed), its average price and
    /// its order id.
    pub(crate) async fn market(&self, coin: &str, size: f64, reduce: bool) -> Result<(f64, f64, u64)> {
        let (asset, dec) = self.coins[coin];
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

    /// The book's best price an order to `buy` rests at without crossing: the best bid for a
    /// buy, the best ask for a sell (mainnet's books; None without one).
    fn best(&self, coin: &str, buy: bool) -> Option<f64> {
        if !self.ex.mainnet() {
            return None;
        }
        let books = self.books.as_ref()?.read().unwrap();
        let b = books.get(coin)?;
        (if buy { b.bids.first() } else { b.asks.first() }).map(|x| x.0)
    }

    /// A reduce-only post-only order for `size` (signed) at the book's best price on its side:
    /// its id and price; None when it does not rest (no book, or the book moved through it).
    async fn post_only(&self, coin: &str, size: f64) -> Result<Option<(u64, f64)>> {
        let (asset, dec) = self.coins[coin];
        let buy = size > 0.0;
        let Some(px) = self.best(coin, buy) else { return Ok(None) };
        let px = round_px(px, dec);
        let order = OrderWire { a: asset, b: buy, p: wire(px), s: wire(round_size(size.abs(), dec)), r: true, t: OrderType::Limit { tif: "Alo".into() } };
        let r = self.ex.act(&Action::Order { orders: vec![order], grouping: "na".into() }).await?;
        Ok(r["data"]["statuses"][0]["resting"]["oid"].as_u64().map(|o| (o, px)))
    }

    /// A limit order for `size` (signed) at `px` (`tif` "Gtc" / "Alo" / "Ioc"): what it did.
    pub(crate) async fn limit(&self, coin: &str, size: f64, px: f64, reduce: bool, tif: &str) -> Result<Placed> {
        let (asset, dec) = self.coins[coin];
        let sz = round_size(size.abs(), dec);
        if sz <= 0.0 {
            bail!("{coin}: size {size} rounds to 0");
        }
        let order = OrderWire { a: asset, b: size > 0.0, p: wire(round_px(px, dec)), s: wire(sz), r: reduce, t: OrderType::Limit { tif: tif.into() } };
        let r = self.ex.act(&Action::Order { orders: vec![order], grouping: "na".into() }).await?;
        placed(coin, &r["data"]["statuses"][0], size > 0.0)
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
        let mut max_lev = HashMap::new();
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
        Ok((Venue { coins: Arc::new(coins), ..self.clone() }, max_lev))
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
    bail!("{coin} order not placed: {st}")
}

/// A leg out by a post-only order at the book's best price, what it has not filled in `secs` at
/// market (see the module): the size traded (signed), its average price, the post-only order's
/// share of it. The leg's stop is cancelled once the leg is out (else it guards the rest).
async fn close_limit(v: &Venue, leg: &Leg, secs: f64) -> Result<(f64, f64, f64)> {
    let want = -leg.size;
    let (mut got, mut paid, mut maker) = (0.0, 0.0, 0.0);
    if let Some((oid, px)) = v.post_only(&leg.coin, want).await? {
        let until = std::time::Instant::now() + std::time::Duration::from_secs_f64(secs);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            let open = v.filled(oid).await.map(|x| x.1).unwrap_or(true);
            if !open || std::time::Instant::now() >= until {
                if open {
                    v.cancel(&leg.coin, oid).await;
                }
                // What it filled, read after the cancel (a fill may come in before it).
                let f = v.filled(oid).await.map(|x| x.0).unwrap_or(0.0);
                maker = f;
                got = f * want.signum();
                paid = f * px;
                break;
            }
        }
    }
    let left = want - got;
    if round_size(left.abs(), v.coins[&leg.coin].1) > 0.0 {
        match v.market(&leg.coin, left, true).await {
            Ok((g, p, _)) => {
                got += g;
                paid += g.abs() * p;
            }
            // The post-only order filled part: the rest (under the minimum, say) to the stop.
            Err(e) if got != 0.0 => log!("live: {} rest of the exit not sold, left to its stop: {e:#}", leg.coin),
            Err(e) => return Err(e),
        }
    }
    if (got - want).abs() <= 1e-9 * want.abs().max(1.0) {
        if let Some(o) = leg.stop_oid {
            v.cancel(&leg.coin, o).await;
        }
    }
    Ok((got, if got != 0.0 { paid / got.abs() } else { 0.0 }, if got != 0.0 { maker / got.abs() } else { 0.0 }))
}

/// One coin's next moves, planned at once from the state (and from each other: a close before
/// the same account's entry the other way), done in order by the coin's job.
#[derive(Debug)]
enum Step {
    Open { key: String, user: String, coin: String, dir: f64, full: f64, size: f64, risk: f64, equity: f64, wait_ms: u64 },
    Follow { key: String, leg: Leg, delta: f64, reduce: bool },
    Close { key: String, leg: Leg, why: &'static str },
    /// Out by a post-only order in the background (`close_limit`), waiting up to `secs`.
    CloseLimit { key: String, leg: Leg, why: &'static str, secs: f64 },
    /// The leg's stop order for its size now (placed after the entry's, for its size then).
    Restop { key: String, leg: Leg },
}

/// A stop order placed in the background for a leg's entry: its id (None: not placed) and size.
#[derive(Debug)]
struct StopPlaced {
    key: String,
    coin: String,
    oid: Option<u64>,
    size: f64,
}

/// A leg out by `close_limit` in the background: what it traded (Err: what went wrong).
#[derive(Debug)]
struct LimitClosed {
    key: String,
    leg: Leg,
    why: &'static str,
    out: Result<(f64, f64, f64), String>,
    ms: u64,
}

/// What a step did, for the state.
#[derive(Debug)]
enum Done {
    Opened { key: String, leg: Leg, equity: f64, ms: Value },
    Moved { key: String, leg: Leg, got: f64, px: f64, reduce: bool, ms: Value },
    Closed { key: String, leg: Leg, got: f64, px: f64, why: &'static str, ms: Value },
    Restopped { key: String, coin: String, oid: Option<u64> },
    /// Its post-only exit is on (`LimitClosed` when it is done).
    Closing { key: String },
    Failed { user: String, coin: String, error: String },
}

/// A coin's steps against the venue, in order: an entry's stop placed in the background after
/// its fill (`stx`: the coin's next moves do not wait for it); a move's stop replaced (the new one
/// placed and the old one cancelled at once; while the entry's is still being placed, it is put
/// right once that one is in); a close's stop cancelled with the close (the exchange drops a
/// reduce-only order without a position anyway).
async fn run_steps(v: Venue, stop_pct: f64, steps: Vec<Step>, stx: mpsc::UnboundedSender<StopPlaced>,
    ctx: mpsc::UnboundedSender<LimitClosed>) -> Vec<Done> {
    let mut out = Vec::new();
    for s in steps {
        let (user, coin) = match &s {
            Step::Open { user, coin, .. } => (user.clone(), coin.clone()),
            Step::Follow { leg, .. } | Step::Close { leg, .. } | Step::CloseLimit { leg, .. } | Step::Restop { leg, .. } => (leg.user.clone(), leg.coin.clone()),
        };
        let r: Result<Done> = async {
            Ok(match s {
                Step::Open { key, user, coin, dir, full, size, risk, equity, wait_ms } => {
                    let t0 = std::time::Instant::now();
                    let (got, px, _) = v.market(&coin, size, false).await?;
                    let order_ms = t0.elapsed().as_millis() as u64;
                    if got == 0.0 {
                        bail!("nothing filled");
                    }
                    let stop_px = px * (1.0 - dir * stop_pct / 100.0);
                    let (v2, stx, k2, c2) = (v.clone(), stx.clone(), key.clone(), coin.clone());
                    tokio::spawn(async move {
                        let oid = match v2.stop(&c2, dir, got, stop_px).await {
                            Ok(o) => Some(o),
                            Err(e) => {
                                log!("live: {c2} stop not placed (the next check places it): {e:#}");
                                None
                            }
                        };
                        let _ = stx.send(StopPlaced { key: k2, coin: c2, oid, size: got });
                    });
                    let ms = json!({"wait": wait_ms, "order": order_ms});
                    Done::Opened { key, leg: Leg { coin, user, dir, full, size: got, entry_px: px, stop_px, stop_oid: None, opened: now(), stop_pending: true,
                        risk, closing: false }, equity, ms }
                }
                Step::Follow { key, mut leg, delta, reduce } => {
                    let t0 = std::time::Instant::now();
                    let (got, px, _) = v.market(&leg.coin, delta, reduce).await?;
                    let order_ms = t0.elapsed().as_millis() as u64;
                    leg.size += got;
                    // The entry's stop still being placed: put right when it is in (`stop_placed`).
                    if !leg.stop_pending {
                        let old = leg.stop_oid;
                        let (new, _) = tokio::join!(v.stop(&leg.coin, leg.dir, leg.size, leg.stop_px), async {
                            if let Some(o) = old {
                                v.cancel(&leg.coin, o).await;
                            }
                        });
                        leg.stop_oid = new.ok();
                    }
                    let ms = json!({"order": order_ms, "stop": t0.elapsed().as_millis() as u64 - order_ms});
                    Done::Moved { key, leg, got, px, reduce, ms }
                }
                Step::Close { key, leg, why } => {
                    let close = async {
                        if leg.size != 0.0 { v.market(&leg.coin, -leg.size, true).await.map(|(g, p, _)| (g, p)) } else { Ok((0.0, 0.0)) }
                    };
                    let cancel = async {
                        if let Some(o) = leg.stop_oid {
                            v.cancel(&leg.coin, o).await;
                        }
                    };
                    let t0 = std::time::Instant::now();
                    let (r, _) = tokio::join!(close, cancel);
                    let (got, px) = r?;
                    let ms = json!({"order": t0.elapsed().as_millis() as u64});
                    Done::Closed { key, leg, got, px, why, ms }
                }
                Step::CloseLimit { key, leg, why, secs } => {
                    let (v2, ctx) = (v.clone(), ctx.clone());
                    let k2 = key.clone();
                    tokio::spawn(async move {
                        let t0 = std::time::Instant::now();
                        let out = close_limit(&v2, &leg, secs).await.map_err(|e| format!("{e:#}"));
                        let _ = ctx.send(LimitClosed { key: k2, leg, why, out, ms: t0.elapsed().as_millis() as u64 });
                    });
                    Done::Closing { key }
                }
                Step::Restop { key, leg } => {
                    let old = leg.stop_oid;
                    let (new, _) = tokio::join!(v.stop(&leg.coin, leg.dir, leg.size, leg.stop_px), async {
                        if let Some(o) = old {
                            v.cancel(&leg.coin, o).await;
                        }
                    });
                    Done::Restopped { key, coin: leg.coin.clone(), oid: new.ok() }
                }
            })
        }.await;
        match r {
            Ok(d) => out.push(d),
            Err(e) => out.push(Done::Failed { user, coin, error: format!("{e:#}") }),
        }
    }
    out
}

pub struct Live {
    venue: Venue,
    account: String,
    unified: bool,
    state: State,
    cfg: Config,
    store: Store,
    status: Arc<Mutex<Value>>,
    limits: Limits,
    /// The account's equity and when it was read (refreshed in the background).
    equity: (f64, f64),
    /// Moves not acted on yet, by coin, and the coins with a job in flight.
    pending: HashMap<String, Vec<Update>>,
    busy: std::collections::HashSet<String>,
    /// When each coin's last job was done: a read begun before it is stale for the coin.
    touched: HashMap<String, f64>,
    /// Legs whose stop is to be put right for their size (by their coin's next job).
    restop: std::collections::HashSet<String>,
    /// The background stops' results.
    stx: mpsc::UnboundedSender<StopPlaced>,
    /// The background post-only exits' results.
    ctx: mpsc::UnboundedSender<LimitClosed>,
}

/// The entries' limits (env, see the module's doc), in % of the account's equity.
#[derive(Clone, Debug)]
struct Limits {
    risk: f64,
    total: f64,
    dir: f64,
    trader: f64,
    per_coin: usize,
    max_dd: f64,
    /// `LIVE_RANK_WEIGHT`, `LIVE_EXIT_LIMIT_S` (0: off).
    rank_weight: bool,
    exit_limit_s: f64,
}

impl Limits {
    fn from_env(cfg: &Config) -> Self {
        let pct = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d);
        Limits { risk: pct("LIVE_RISK_PCT", cfg.risk_pct), total: pct("LIVE_CAP_TOTAL_PCT", 100.0), dir: pct("LIVE_CAP_DIR_PCT", 50.0),
            trader: pct("LIVE_CAP_TRADER_PCT", 20.0), per_coin: 2, max_dd: pct("LIVE_MAX_DD_PCT", 0.0),
            rank_weight: pct("LIVE_RANK_WEIGHT", 0.0) == 1.0, exit_limit_s: pct("LIVE_EXIT_LIMIT_S", 0.0).max(0.0) }
    }

    /// A leg's risk: its own, or (saved before it had one) the entries'.
    fn of(&self, l: &Leg) -> f64 {
        if l.risk > 0.0 { l.risk } else { self.risk }
    }
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
    Ok(Some((Venue { ex, coins: Arc::new(coins), books, mids: Default::default(), account }, unified)))
}

/// Starts the live trader if `LIVE_ENABLED` is "1" (or `force`, for `live-check`) and
/// `LIVE_KEY` and `LIVE_ACCOUNT` are set: its sender, and its status (for `bot_status`).
/// `books`: the engine's live books (mainnet's), its mids.
pub async fn start(cfg: &Config, store: Store, force: bool, books: Option<crate::ws::Books>)
    -> Result<Option<(mpsc::UnboundedSender<Update>, Arc<Mutex<Value>>)>> {
    if !force && std::env::var("LIVE_ENABLED").unwrap_or_default().trim() != "1" {
        return Ok(None);
    }
    let (Ok(key), Ok(account)) = (std::env::var("LIVE_KEY"), std::env::var("LIVE_ACCOUNT")) else { return Ok(None) };
    if key.trim().is_empty() || account.trim().is_empty() {
        return Ok(None);
    }
    let base = match std::env::var("LIVE_NET").unwrap_or_default().as_str() {
        "mainnet" => MAINNET,
        _ => TESTNET,
    };
    let ex = Exchange::new(base, &key)?;
    let account = account.trim().to_lowercase();
    let meta = ex.info(json!({"type": "meta"})).await.context("live: meta")?;
    let coins = meta["universe"].as_array().into_iter().flatten().enumerate()
        .filter(|(_, u)| u["isDelisted"] != true)
        .filter_map(|(i, u)| Some((u["name"].as_str()?.to_string(), (i as u32, u["szDecimals"].as_u64()? as u32))))
        .collect::<HashMap<_, _>>();
    let unified = ex.info(json!({"type": "userAbstraction", "user": account})).await.map(|v| v == "unifiedAccount").unwrap_or(false);
    let state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value::<State>(v).ok()).unwrap_or_default().upgrade();
    log!("live: {} account {} (signer {}{}), {} coins, {} legs held", if base == MAINNET { "MAINNET" } else { "testnet" },
        account, ex.signer(), if unified { ", unified" } else { "" }, coins.len(), state.legs.len());
    let status = Arc::new(Mutex::new(json!({})));
    let (stx, mut srx) = mpsc::unbounded_channel::<StopPlaced>();
    let (ctx, mut crx) = mpsc::unbounded_channel::<LimitClosed>();
    let venue = Venue { ex, coins: Arc::new(coins), books, mids: Default::default(), account: account.clone() };
    let limits = Limits::from_env(cfg);
    log!("live: entries {}% each{}; exits {}", limits.risk, if limits.rank_weight { " times the trader's weight by his place" } else { "" },
        if limits.exit_limit_s > 0.0 { format!("post-only, at market after {} s", limits.exit_limit_s) } else { "at market".into() });
    let mut live = Live { venue, account, unified, state, cfg: cfg.clone(), store, status: status.clone(), limits,
        equity: (0.0, 0.0), pending: HashMap::new(), busy: Default::default(), touched: HashMap::new(), restop: Default::default(), stx, ctx };
    let (tx, mut rx) = mpsc::unbounded_channel::<Update>();
    let (dtx, mut drx) = mpsc::unbounded_channel::<(String, Vec<Done>)>();
    let (etx, mut erx) = mpsc::unbounded_channel::<f64>();
    tokio::spawn(async move {
        live.health().await;
        live.reconcile().await;
        // Post-only exits a restart cut off: out again (their trader is flat).
        for l in live.state.legs.values_mut().filter(|l| l.closing).map(|l| { l.closing = false; l.clone() }).collect::<Vec<_>>() {
            live.queue(Update { user: l.user, coin: l.coin, dir: l.dir, frac: 0.0, entry: false, golden: false, at: now(), weight: 1.0 }, &dtx);
        }
        // The equity in the background: entries size on it without a request of their own.
        let (venue, account, unified) = (live.venue.clone(), live.account.clone(), live.unified);
        tokio::spawn(async move {
            loop {
                if let Ok((e, _)) = equity_of(&venue.ex, &account, unified).await {
                    if etx.send(e).is_err() {
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(EQUITY_EVERY_S)).await;
            }
        });
        let v = live.venue.clone();
        tokio::spawn(async move {
            loop {
                if let Ok(m) = v.ex.info(json!({"type": "allMids"})).await {
                    if let Some(o) = m.as_object() {
                        *v.mids.write().unwrap() = o.iter().map(|(c, x)| (c.clone(), num(x))).filter(|(_, x)| *x > 0.0).collect();
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs_f64(MIDS_EVERY_S)).await;
            }
        });
        let (rtx, mut rrx) = mpsc::unbounded_channel::<Read>();
        let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
        every.tick().await;
        loop {
            tokio::select! {
                u = rx.recv() => match u {
                    Some(u) => live.queue(u, &dtx),
                    None => break,
                },
                Some((coin, done)) = drx.recv() => live.done(coin, done, &dtx),
                Some(sp) = srx.recv() => live.stop_placed(sp, &dtx),
                Some(c) = crx.recv() => live.limit_closed(c),
                Some(e) = erx.recv() => live.equity = (e, now()),
                Some(r) = rrx.recv() => live.reconcile_with(r).await,
                _ = every.tick() => {
                    let (ex, account, unified, rtx) = (live.venue.ex.clone(), live.account.clone(), live.unified, rtx.clone());
                    tokio::spawn(async move {
                        if let Some(r) = read_account(&ex, &account, unified).await {
                            let _ = rtx.send(r);
                        }
                    });
                }
            }
        }
    });
    Ok(Some((tx, status)))
}

/// The account as read for `reconcile_with`: when the read began, the equity, the positions,
/// the resting orders' ids (None: not read).
struct Read {
    at: f64,
    equity: f64,
    positions: HashMap<String, f64>,
    resting: Option<std::collections::HashSet<u64>>,
}

async fn read_account(ex: &Exchange, account: &str, unified: bool) -> Option<Read> {
    let at = now();
    let (equity, positions) = match equity_of(ex, account, unified).await {
        Ok(x) => x,
        Err(e) => {
            log!("live: account read failed: {e:#}");
            return None;
        }
    };
    let resting = match ex.info(json!({"type": "frontendOpenOrders", "user": account})).await {
        Ok(v) => Some(v.as_array().into_iter().flatten().filter_map(|o| o["oid"].as_u64()).collect()),
        Err(e) => {
            log!("live: orders read failed: {e:#}");
            None
        }
    };
    Some(Read { at, equity, positions, resting })
}

/// Our account's equity: its perp account value, and on a unified account the spot USDC not
/// held as perp margin (it backs the perps too); and its positions.
/// The HIP-3 dexes margined in USDC (their meta's collateral token 0), read once (not cached when the read fails).
static USDC_DEXES: tokio::sync::OnceCell<Vec<String>> = tokio::sync::OnceCell::const_new();

async fn usdc_dexes(ex: &Exchange) -> Vec<String> {
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

impl Live {
    fn event(&self, mut ev: Value) {
        ev["kind"] = json!("live");
        ev["at"] = json!(now());
        log!("live: {ev}");
        self.store.event(ev);
    }

    /// The state to the store, in the background (a job's results do not wait for it).
    fn save(&self) {
        let (cfg, v) = (self.cfg.clone(), serde_json::to_value(&self.state).unwrap_or_default());
        tokio::spawn(async move {
            if let Err(e) = crate::store::save_doc(&cfg, DOC, &v).await {
                log!("live: state not saved: {e:#}");
            }
        });
    }

    fn skip(&mut self, u: &Update, why: &str) {
        self.state.skipped += 1;
        self.event(json!({"what": "skipped", "why": why, "user": u.user, "coin": u.coin, "late_s": (now() - u.at).max(0.0)}));
    }

    /// A move in: acted on at once if its coin has no job in flight, else when that job is done
    /// (the moves that came meanwhile taken together: each account's last, an entry among them
    /// kept).
    fn queue(&mut self, u: Update, dtx: &mpsc::UnboundedSender<(String, Vec<Done>)>) {
        if !self.venue.coins.contains_key(&u.coin) {
            // A followed account's entry in a coin the live venue does not list: recorded, so it
            // is not taken for a missed one.
            if u.entry && u.golden && u.frac > 0.0 {
                self.skip(&u, "coin not listed on the live venue");
            }
            return;
        }
        let coin = u.coin.clone();
        self.pending.entry(coin.clone()).or_default().push(u);
        if !self.busy.contains(&coin) {
            self.launch(&coin, dtx);
        }
    }

    /// Plans the coin's pending moves and starts its job.
    fn launch(&mut self, coin: &str, dtx: &mpsc::UnboundedSender<(String, Vec<Done>)>) {
        let us = self.pending.remove(coin).unwrap_or_default();
        let restops: Vec<String> = self.restop.iter().filter(|k| self.state.legs.get(*k).is_some_and(|l| l.coin == coin)).cloned().collect();
        if us.is_empty() && restops.is_empty() {
            return;
        }
        // Each account's moves together: its last state; an entry if any was one (the first's time).
        let mut merged: Vec<Update> = Vec::new();
        for u in us {
            match merged.iter_mut().find(|m| m.user == u.user) {
                Some(m) => {
                    let (entry, at) = (m.entry || u.entry, if m.entry { m.at } else { u.at });
                    *m = Update { entry, at, ..u };
                }
                None => merged.push(u),
            }
        }
        let mut steps = Vec::new();
        for k in restops {
            self.restop.remove(&k);
            if let Some(l) = self.state.legs.get(&k) {
                steps.push(Step::Restop { key: k.clone(), leg: l.clone() });
            }
        }
        // The legs as they will be after the steps planned so far.
        let mut view = self.state.legs.clone();
        for u in merged {
            self.plan(&u, &mut view, &mut steps);
        }
        if steps.is_empty() {
            return;
        }
        self.state.orders += steps.len() as u64;
        self.busy.insert(coin.to_string());
        let (v, stop_pct, dtx, coin, stx, ctx) = (self.venue.clone(), self.cfg.stop_pct, dtx.clone(), coin.to_string(), self.stx.clone(), self.ctx.clone());
        tokio::spawn(async move {
            let done = run_steps(v, stop_pct, steps, stx, ctx).await;
            let _ = dtx.send((coin, done));
        });
    }

    /// One account's move as steps, against `view` (updated as if they were done).
    fn plan(&mut self, u: &Update, view: &mut HashMap<String, Leg>, steps: &mut Vec<Step>) {
        let k = key(&u.coin, &u.user);
        match view.get(&k).cloned() {
            // Its post-only exit still on: the trader's moves meanwhile are not followed.
            Some(leg) if leg.closing => {
                if u.entry && u.golden && u.frac > 0.0 {
                    self.skip(u, "our exit of the last one still on");
                }
            }
            Some(leg) if u.frac <= 0.0 && self.limits.exit_limit_s > 0.0 => {
                // Held in the view (its risk counted) until it is out.
                if let Some(l) = view.get_mut(&k) {
                    l.closing = true;
                }
                steps.push(Step::CloseLimit { key: k, leg, why: "exit", secs: self.limits.exit_limit_s });
            }
            Some(leg) if u.frac <= 0.0 || u.dir != leg.dir => {
                view.remove(&k);
                steps.push(Step::Close { key: k, leg, why: if u.frac <= 0.0 { "exit" } else { "flip" } });
                if u.frac > 0.0 && u.entry && u.golden {
                    self.plan_open(u, view, steps);
                }
            }
            Some(leg) => {
                let Some(mid) = self.mid_now(&u.coin) else { return };
                let target = leg.dir * leg.full * u.frac.min(1.0);
                if target.abs() * mid < MIN_LEFT_USD {
                    view.remove(&k);
                    steps.push(Step::Close { key: k, leg, why: "small" });
                    return;
                }
                let delta = target - leg.size;
                if round_size(delta.abs(), self.venue.coins[&u.coin].1) * mid < MIN_ORDER_USD {
                    return;
                }
                let reduce = target.abs() < leg.size.abs();
                if let Some(l) = view.get_mut(&k) {
                    l.size = target;
                }
                steps.push(Step::Follow { key: k, leg, delta, reduce });
            }
            None if u.entry && u.golden && u.frac > 0.0 => self.plan_open(u, view, steps),
            None => {}
        }
    }

    /// The mid from the books, else from the background read (no request): None if neither has it.
    fn mid_now(&self, coin: &str) -> Option<f64> {
        self.venue.books.as_ref().filter(|_| self.venue.ex.mainnet()).and_then(|b| b.read().unwrap().get(coin).and_then(crate::ws::Book::mid))
            .or_else(|| self.venue.mids.read().unwrap().get(coin).copied())
    }

    fn plan_open(&mut self, u: &Update, view: &mut HashMap<String, Leg>, steps: &mut Vec<Step>) {
        let late = now() - u.at;
        if late > ENTRY_MAX_AGE_S {
            self.skip(u, "late");
            return;
        }
        if view.len() >= self.cfg.max_positions {
            self.skip(u, "max positions");
            return;
        }
        if view.values().any(|l| l.coin == u.coin && l.dir != u.dir) {
            self.skip(u, "other way from the coin's legs");
            return;
        }
        // The portfolio's limits: each open leg holds its risk of the account at its entry.
        let lim = self.limits.clone();
        let risk = lim.risk * if lim.rank_weight { u.weight } else { 1.0 };
        let legs = |f: &dyn Fn(&Leg) -> bool| view.values().filter(|l| f(l)).map(|l| lim.of(l)).sum::<f64>();
        let why = if legs(&|_| true) + risk > lim.total + 1e-9 {
            Some("total risk")
        } else if legs(&|l| l.dir == u.dir) + risk > lim.dir + 1e-9 {
            Some("risk in one direction")
        } else if legs(&|l| l.user == u.user) + risk > lim.trader + 1e-9 {
            Some("risk of one trader")
        } else if view.values().filter(|l| l.coin == u.coin).count() >= lim.per_coin {
            Some("legs in the coin")
        } else {
            None
        };
        if let Some(why) = why {
            self.skip(u, why);
            return;
        }
        let equity = self.equity.0;
        if equity <= 0.0 {
            self.skip(u, "equity not read yet");
            return;
        }
        self.state.peak = self.state.peak.max(equity);
        if lim.max_dd > 0.0 && equity < self.state.peak * (1.0 - lim.max_dd / 100.0) {
            self.skip(u, "drawdown brake");
            return;
        }
        let Some(mid) = self.mid_now(&u.coin) else {
            self.skip(u, "no book");
            return;
        };
        let dec = self.venue.coins[&u.coin].1;
        let full = round_size(risk / self.cfg.stop_pct * equity / mid, dec);
        if full * mid < MIN_ORDER_USD {
            self.event(json!({"what": "too small", "user": u.user, "coin": u.coin, "equity": equity, "size": full}));
            return;
        }
        let k = key(&u.coin, &u.user);
        let size = u.dir * full * u.frac.min(1.0);
        // In the view as held: the limits count it for the moves after it.
        view.insert(k.clone(), Leg { coin: u.coin.clone(), user: u.user.clone(), dir: u.dir, full, size, risk, ..Default::default() });
        steps.push(Step::Open { key: k, user: u.user.clone(), coin: u.coin.clone(), dir: u.dir, full, size, risk, equity,
            wait_ms: (late.max(0.0) * 1000.0) as u64 });
    }

    /// A job's results into the state; the coin's moves that came meanwhile next.
    fn done(&mut self, coin: String, done: Vec<Done>, dtx: &mpsc::UnboundedSender<(String, Vec<Done>)>) {
        for d in done {
            match d {
                Done::Opened { key, leg, equity, ms } => {
                    self.event(json!({"what": "open", "user": leg.user, "coin": leg.coin, "size": leg.size, "px": leg.entry_px,
                        "notional": leg.size.abs() * leg.entry_px, "equity": equity, "risk_pct": leg.risk, "stop_px": leg.stop_px, "stop_oid": leg.stop_oid, "ms": ms}));
                    self.state.legs.insert(key, leg);
                }
                Done::Moved { key, leg, got, px, reduce, ms } => {
                    self.event(json!({"what": if reduce { "reduce" } else { "add" }, "user": leg.user, "coin": leg.coin, "size": got, "px": px,
                        "pos_after": leg.size, "stop_oid": leg.stop_oid, "ms": ms}));
                    self.state.legs.insert(key, leg);
                }
                Done::Closed { key, leg, got, px, why, ms } => {
                    self.event(json!({"what": "close", "why": why, "user": leg.user, "coin": leg.coin, "size": got, "px": px,
                        "pnl_approx": -got * (px - leg.entry_px), "ms": ms}));
                    self.state.legs.remove(&key);
                }
                Done::Restopped { key, coin, oid } => match self.state.legs.get_mut(&key) {
                    Some(l) => l.stop_oid = oid,
                    None => {
                        if let Some(o) = oid {
                            let v = self.venue.clone();
                            tokio::spawn(async move { v.cancel(&coin, o).await });
                        }
                    }
                },
                Done::Closing { key } => {
                    if let Some(l) = self.state.legs.get_mut(&key) {
                        l.closing = true;
                    }
                }
                Done::Failed { user, coin, error } => self.event(json!({"what": "error", "user": user, "coin": coin, "error": error})),
            }
        }
        self.busy.remove(&coin);
        self.touched.insert(coin.clone(), now());
        self.publish_users();
        self.save();
        if self.pending.contains_key(&coin) || self.restop.iter().any(|k| self.state.legs.get(k).is_some_and(|l| l.coin == coin)) {
            self.launch(&coin, dtx);
        }
    }

    /// A post-only exit done: the leg out (on an error too: the next check reads what is held).
    fn limit_closed(&mut self, c: LimitClosed) {
        let LimitClosed { key, leg, why, out, ms } = c;
        self.state.legs.remove(&key);
        self.touched.insert(leg.coin.clone(), now());
        match out {
            Ok((got, px, maker)) => self.event(json!({"what": "close", "why": why, "user": leg.user, "coin": leg.coin, "size": got, "px": px,
                "post_only_share": (maker * 1000.0).round() / 1000.0, "pnl_approx": -got * (px - leg.entry_px), "ms": {"order": ms}})),
            Err(e) => self.event(json!({"what": "error", "user": leg.user, "coin": leg.coin, "error": format!("post-only exit: {e}")})),
        }
        self.publish_users();
        self.save();
    }

    /// An entry's stop, placed in the background: on its leg; for the leg's size now if it moved
    /// meanwhile (by the coin's next job); cancelled if the leg is gone already.
    fn stop_placed(&mut self, sp: StopPlaced, dtx: &mpsc::UnboundedSender<(String, Vec<Done>)>) {
        let StopPlaced { key, coin, oid, size } = sp;
        match self.state.legs.get_mut(&key) {
            Some(l) => {
                l.stop_pending = false;
                l.stop_oid = oid;
                let off = oid.is_some() && (l.size - size).abs() > 1e-12 * l.size.abs().max(1.0);
                let ev = json!({"what": "stop", "user": l.user, "coin": coin, "stop_px": l.stop_px, "stop_oid": oid, "resize": off});
                self.event(ev);
                if off {
                    self.restop.insert(key);
                    if !self.busy.contains(&coin) {
                        self.launch(&coin, dtx);
                    }
                }
                self.save();
            }
            None => {
                if let Some(o) = oid {
                    let v = self.venue.clone();
                    tokio::spawn(async move { v.cancel(&coin, o).await });
                }
            }
        }
    }

    /// The accounts we hold legs of, in the status (`users`): the engine keeps following them
    /// to their exits when it follows nothing else (`LIVE_ONLY`).
    fn publish_users(&self) {
        let users: std::collections::BTreeSet<&String> = self.state.legs.values().map(|l| &l.user).collect();
        self.status.lock().unwrap()["users"] = json!(users);
    }

    /// The start's check (see the module): bought and sold at once, recorded either way.
    async fn health(&mut self) {
        let started = std::time::Instant::now();
        let Some(coin) = ["BTC", "ETH", "SOL"].into_iter().find(|c| self.venue.coins.contains_key(*c) && self.state.in_coin(c).next().is_none()) else {
            log!("live: health check skipped (BTC, ETH and SOL all held)");
            return;
        };
        let mut rec = json!({"net": if self.venue.ex.mainnet() { "mainnet" } else { "testnet" }, "account": self.account, "coin": coin});
        let v = self.venue.clone();
        let r: Result<()> = async {
            let (equity, _) = equity_of(&v.ex, &self.account, self.unified).await?;
            self.equity = (equity, now());
            rec["equity"] = json!(equity);
            let mid = v.mid(coin).await?;
            let m = 10f64.powi(v.coins[coin].1 as i32);
            let size = (CHECK_USD / mid * m).ceil() / m;
            let t0 = std::time::Instant::now();
            let (got, open_px, open_oid) = v.market(coin, size, false).await?;
            rec["order_ms"] = json!(t0.elapsed().as_millis() as u64);
            rec["size"] = json!(got);
            rec["open_px"] = json!(open_px);
            let close = match v.market(coin, -got, true).await {
                Ok(x) => x,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    v.market(coin, -got, true).await.context("bought, not sold: the position is left open")?
                }
            };
            rec["close_px"] = json!(close.1);
            let fills = v.ex.info(json!({"type": "userFills", "user": self.account})).await?;
            let fee: f64 = fills.as_array().into_iter().flatten()
                .filter(|f| [open_oid, close.2].contains(&f["oid"].as_u64().unwrap_or(0))).map(|f| num(&f["fee"])).sum();
            rec["fee"] = json!(fee);
            rec["pnl"] = json!(got * (close.1 - open_px) - fee);
            Ok(())
        }.await;
        rec["ok"] = json!(r.is_ok());
        rec["error"] = json!(r.as_ref().err().map(|e| format!("{e:#}")));
        rec["took_ms"] = json!(started.elapsed().as_millis() as u64);
        log!("live: health check {}: {rec}", if r.is_ok() { "ok" } else { "FAILED" });
        if let Err(e) = crate::store::save_live_check(&self.cfg, &rec).await {
            log!("live: health check not saved: {e:#}");
        }
    }

    /// Puts the leg's stop order on for its size now (the old one cancelled): its size alone,
    /// so it does not close the coin's other legs.
    async fn restop(&mut self, k: &str) {
        let Some(l) = self.state.legs.get(k).cloned() else { return };
        if let Some(oid) = l.stop_oid {
            self.venue.cancel(&l.coin, oid).await;
        }
        let oid = match self.venue.stop(&l.coin, l.dir, l.size, l.stop_px).await {
            Ok(oid) => Some(oid),
            Err(e) => {
                self.event(json!({"what": "stop failed", "user": l.user, "coin": l.coin, "error": format!("{e:#}")}));
                None
            }
        };
        if let Some(leg) = self.state.legs.get_mut(k) {
            leg.stop_oid = oid;
        }
    }

    /// The exchange's positions and orders: a leg whose stop order is no longer resting was
    /// stopped out; a coin's position gone (a liquidation) or the other way ends its legs; the
    /// rest take their share of what is held. A leg without a stop order gets one. Then the
    /// status.
    async fn reconcile(&mut self) {
        if let Some(r) = read_account(&self.venue.ex, &self.account, self.unified).await {
            self.reconcile_with(r).await;
        }
    }

    /// `reconcile` on what `read_account` read (in the background: the moves do not wait for
    /// the reads). A coin moved since the read began is left for the next one.
    async fn reconcile_with(&mut self, r: Read) {
        let Read { at, equity, positions, resting } = r;
        self.equity = (equity, now());
        let fresh = |c: &String, s: &Self| s.busy.contains(c) || s.pending.contains_key(c) || s.touched.get(c).is_some_and(|t| *t >= at - 1.0)
            || s.state.in_coin(c).any(|(_, l)| l.closing);
        let mut changed = false;
        if let Some(resting) = &resting {
            for (k, l) in self.state.legs.clone() {
                // A coin with a job in flight or moved since: read it next time.
                if fresh(&l.coin, self) {
                    continue;
                }
                if l.stop_oid.is_some_and(|o| !resting.contains(&o)) {
                    self.state.legs.remove(&k);
                    self.event(json!({"what": "stopped", "user": l.user, "coin": l.coin, "size_was": l.size, "stop_px": l.stop_px,
                        "entry_px": l.entry_px}));
                    changed = true;
                }
            }
        }
        let coins: std::collections::HashSet<String> = self.state.legs.values().map(|l| l.coin.clone())
            .filter(|c| !fresh(c, self)).collect();
        for coin in coins {
            let held = positions.get(&coin).copied().unwrap_or(0.0);
            let legs: Vec<(String, Leg)> = self.state.in_coin(&coin).map(|(k, l)| (k.clone(), l.clone())).collect();
            let sum: f64 = legs.iter().map(|(_, l)| l.size).sum();
            if held == 0.0 || legs.iter().any(|(_, l)| held.signum() != l.dir) {
                for (k, l) in legs {
                    if let Some(oid) = l.stop_oid.filter(|_| held != 0.0) {
                        self.venue.cancel(&coin, oid).await;
                    }
                    self.state.legs.remove(&k);
                    self.event(json!({"what": "gone", "user": l.user, "coin": coin, "size_was": l.size, "stop_px": l.stop_px}));
                }
                changed = true;
            } else if sum != 0.0 && (held - sum).abs() > 1e-9 {
                for (k, _) in &legs {
                    if let Some(leg) = self.state.legs.get_mut(k) {
                        leg.size *= held / sum;
                    }
                }
                changed = true;
            }
        }
        if resting.is_some() {
            for k in self.state.legs.iter().filter(|(_, l)| l.stop_oid.is_none() && !l.stop_pending && !fresh(&l.coin, self))
                .map(|(k, _)| k.clone()).collect::<Vec<_>>() {
                self.restop(&k).await;
                changed = true;
            }
        }
        if changed {
            self.save();
        }
        let coins_held: std::collections::HashSet<&String> = self.state.legs.values().map(|l| &l.coin).collect();
        let other = positions.keys().filter(|c| !coins_held.contains(c)).count();
        *self.status.lock().unwrap() = json!({"equity": (equity * 100.0).round() / 100.0, "positions": self.state.legs.len(),
            "coins": coins_held.len(), "other_positions": other, "skipped": self.state.skipped, "orders": self.state.orders});
        self.publish_users();
    }
}

/// `live-check [COIN]`: trades on the live account through the same path as golden accounts'
/// (testnet unless set): two accounts' legs in one coin, each with its own stop order; one
/// half out (its stop resized), the other out, then the first out; a third one's entry the
/// other way is skipped.
pub async fn check(cfg: &Config, coin: &str) -> Result<()> {
    let store = crate::store::open(cfg).await?;
    let (tx, _) = start(cfg, store.clone(), true, None).await?.context("LIVE_KEY and LIVE_ACCOUNT are not set")?;
    let account = std::env::var("LIVE_ACCOUNT")?.trim().to_lowercase();
    let base = if std::env::var("LIVE_NET").unwrap_or_default() == "mainnet" { MAINNET } else { TESTNET };
    let info = Exchange::new(base, &std::env::var("LIVE_KEY")?)?;
    let send = |user: &str, dir: f64, frac: f64, entry: bool| {
        tx.send(Update { user: format!("live-check-{user}"), coin: coin.into(), dir, frac, entry, golden: true, at: now(), weight: 1.0 })
    };
    let show = |what: &'static str| {
        let (info, account, coin) = (&info, &account, coin);
        async move {
            let orders = info.info(json!({"type": "frontendOpenOrders", "user": account})).await?;
            let stops: Vec<String> = orders.as_array().into_iter().flatten().filter(|o| o["coin"] == coin)
                .map(|o| format!("{} {} @ {}", o["side"], o["sz"], o["triggerPx"])).collect();
            let st = info.info(json!({"type": "clearinghouseState", "user": account})).await?;
            let pos: Vec<String> = st["assetPositions"].as_array().into_iter().flatten().filter(|p| p["position"]["coin"] == coin)
                .map(|p| p["position"]["szi"].to_string()).collect();
            log!("live-check: {what}: position {pos:?}, stop orders {stops:?}");
            anyhow::Ok(())
        }
    };
    let pause = |s| tokio::time::sleep(std::time::Duration::from_secs(s));
    // After the start's own check and read (an entry sent before them would be late).
    pause(12).await;
    send("a", 1.0, 1.0, true)?;
    send("b", 1.0, 1.0, true)?;
    send("c", -1.0, 1.0, true)?;
    pause(10).await;
    show("a and b in, c skipped").await?;
    send("a", 1.0, 0.5, false)?;
    pause(6).await;
    show("a half out").await?;
    send("b", 1.0, 0.0, false)?;
    pause(6).await;
    show("b out").await?;
    send("a", 1.0, 0.0, false)?;
    pause(6).await;
    show("a out").await?;
    store.flush().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn rank_weights_fall_with_the_place_and_keep_the_mean() {
        let w: Vec<f64> = (1..=20).map(|r| rank_weight(r, 20)).collect();
        assert!((w.iter().sum::<f64>() / 20.0 - 1.0).abs() < 1e-9);
        assert!(w.windows(2).all(|p| p[0] >= p[1]) && w[0] > 1.5 && w[19] < 0.5);
        // Not among the picks, or no picks: as set.
        assert_eq!((rank_weight(0, 20), rank_weight(21, 20), rank_weight(1, 0)), (1.0, 1.0, 1.0));
    }

    #[test]
    fn legs_saved_before_risk_and_closing_load_with_the_defaults() {
        let v = json!({"legs": {"BTC|0xa": {"coin": "BTC", "user": "0xa", "dir": 1.0, "full": 0.1, "size": 0.1, "entry_px": 1.0,
            "stop_px": 0.8, "stop_oid": 7, "opened": 0.0}}, "skipped": 0, "orders": 2});
        let s = serde_json::from_value::<State>(v).unwrap();
        let l = &s.legs["BTC|0xa"];
        assert!(l.risk == 0.0 && !l.closing);
        let lim = Limits { risk: 1.0, total: 100.0, dir: 50.0, trader: 20.0, per_coin: 2, max_dd: 0.0, rank_weight: true, exit_limit_s: 15.0 };
        assert_eq!(lim.of(l), 1.0);
        assert_eq!(lim.of(&Leg { risk: 1.4, ..l.clone() }), 1.4);
    }

    #[test]
    fn legs_saved_by_coin_get_keys_per_account() {
        let v = json!({"legs": {"BTC": {"user": "0xa", "dir": 1.0, "full": 0.1, "size": 0.1, "entry_px": 1.0, "stop_px": 0.8,
            "stop_oid": 7, "opened": 0.0}}, "skipped": 0, "orders": 2});
        let s = serde_json::from_value::<State>(v).unwrap().upgrade();
        let l = &s.legs["BTC|0xa"];
        assert_eq!((l.coin.as_str(), l.stop_oid), ("BTC", Some(7)));
        // Already per account: unchanged.
        let again = s.clone().upgrade();
        assert_eq!(again.legs.keys().collect::<Vec<_>>(), vec!["BTC|0xa"]);
        assert_eq!(again.in_coin("BTC").count(), 1);
        assert_eq!(again.in_coin("ETH").count(), 0);
    }
}
