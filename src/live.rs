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

pub struct Live {
    ex: Exchange,
    account: String,
    unified: bool,
    /// Coin -> (asset index, size decimals) on the live venue.
    coins: HashMap<String, (u32, u32)>,
    state: State,
    cfg: Config,
    store: Store,
    status: Arc<Mutex<Value>>,
    limits: Limits,
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
}

impl Limits {
    fn from_env(cfg: &Config) -> Self {
        let pct = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d);
        Limits { risk: pct("LIVE_RISK_PCT", cfg.risk_pct), total: pct("LIVE_CAP_TOTAL_PCT", 100.0), dir: pct("LIVE_CAP_DIR_PCT", 50.0),
            trader: pct("LIVE_CAP_TRADER_PCT", 20.0), per_coin: 2, max_dd: pct("LIVE_MAX_DD_PCT", 0.0) }
    }
}

/// Starts the live trader if `LIVE_ENABLED` is "1" (or `force`, for `live-check`) and
/// `LIVE_KEY` and `LIVE_ACCOUNT` are set: its sender, and its status (for `bot_status`).
pub async fn start(cfg: &Config, store: Store, force: bool) -> Result<Option<(mpsc::UnboundedSender<Update>, Arc<Mutex<Value>>)>> {
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
    let mut live = Live { ex, account, unified, coins, state, cfg: cfg.clone(), store, status: status.clone(), limits: Limits::from_env(cfg) };
    let (tx, mut rx) = mpsc::unbounded_channel::<Update>();
    tokio::spawn(async move {
        live.health().await;
        live.reconcile().await;
        let mut every = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                u = rx.recv() => match u {
                    Some(u) => live.on_update(u).await,
                    None => break,
                },
                _ = every.tick() => live.reconcile().await,
            }
        }
    });
    Ok(Some((tx, status)))
}

impl Live {
    fn event(&self, mut ev: Value) {
        ev["kind"] = json!("live");
        ev["at"] = json!(now());
        log!("live: {ev}");
        self.store.event(ev);
    }

    async fn save(&self) {
        if let Err(e) = crate::store::save_doc(&self.cfg, DOC, &serde_json::to_value(&self.state).unwrap_or_default()).await {
            log!("live: state not saved: {e:#}");
        }
    }

    /// Our account's equity: its perp account value, and on a unified account the spot USDC
    /// not held as perp margin (it backs the perps too).
    async fn equity(&self) -> Result<(f64, HashMap<String, f64>)> {
        let st = self.ex.info(json!({"type": "clearinghouseState", "user": self.account})).await?;
        let mut eq = num(&st["marginSummary"]["accountValue"]);
        if self.unified {
            let spot = self.ex.info(json!({"type": "spotClearinghouseState", "user": self.account})).await?;
            if let Some(b) = spot["balances"].as_array().into_iter().flatten().find(|b| b["coin"] == "USDC") {
                eq += num(&b["total"]) - num(&b["hold"]);
            }
        }
        let positions = st["assetPositions"].as_array().into_iter().flatten()
            .filter_map(|p| Some((p["position"]["coin"].as_str()?.to_string(), num(&p["position"]["szi"]))))
            .collect();
        Ok((eq, positions))
    }

    async fn mid(&self, coin: &str) -> Result<f64> {
        let mids = self.ex.info(json!({"type": "allMids"})).await?;
        let m = num(&mids[coin]);
        if m <= 0.0 {
            bail!("no mid for {coin}");
        }
        Ok(m)
    }

    /// A market order (IOC through the mid): the size filled (signed) and its average price.
    async fn market(&mut self, coin: &str, size: f64, reduce: bool) -> Result<(f64, f64)> {
        self.market_oid(coin, size, reduce).await.map(|(got, px, _)| (got, px))
    }

    /// `market`, with the order's id.
    async fn market_oid(&mut self, coin: &str, size: f64, reduce: bool) -> Result<(f64, f64, u64)> {
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
            self.state.orders += 1;
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
    async fn stop(&mut self, coin: &str, dir: f64, size: f64, trigger: f64) -> Result<u64> {
        let (asset, dec) = self.coins[coin];
        let trigger = round_px(trigger, dec);
        let px = round_px(trigger * (1.0 - dir * STOP_SLIPPAGE), dec);
        let order = OrderWire {
            a: asset, b: dir < 0.0, p: wire(px), s: wire(round_size(size.abs(), dec)), r: true,
            t: OrderType::Trigger { is_market: true, trigger_px: wire(trigger), tpsl: "sl".into() },
        };
        self.state.orders += 1;
        let r = self.ex.act(&Action::Order { orders: vec![order], grouping: "na".into() }).await?;
        let st = &r["data"]["statuses"][0];
        st["resting"]["oid"].as_u64().with_context(|| format!("{coin} stop not placed: {st}"))
    }

    /// The start's check (see the module): bought and sold at once, recorded either way.
    async fn health(&mut self) {
        let started = std::time::Instant::now();
        let Some(coin) = ["BTC", "ETH", "SOL"].into_iter().find(|c| self.coins.contains_key(*c) && self.state.in_coin(c).next().is_none()) else {
            log!("live: health check skipped (BTC, ETH and SOL all held)");
            return;
        };
        let mut rec = json!({"net": if self.ex.mainnet() { "mainnet" } else { "testnet" }, "account": self.account, "coin": coin});
        let r: Result<()> = async {
            let (equity, _) = self.equity().await?;
            rec["equity"] = json!(equity);
            let mid = self.mid(coin).await?;
            let m = 10f64.powi(self.coins[coin].1 as i32);
            let size = (CHECK_USD / mid * m).ceil() / m;
            let (got, open_px, open_oid) = self.market_oid(coin, size, false).await?;
            rec["size"] = json!(got);
            rec["open_px"] = json!(open_px);
            let close = match self.market_oid(coin, -got, true).await {
                Ok(x) => x,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    self.market_oid(coin, -got, true).await.context("bought, not sold: the position is left open")?
                }
            };
            rec["close_px"] = json!(close.1);
            let fills = self.ex.info(json!({"type": "userFills", "user": self.account})).await?;
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

    async fn cancel(&mut self, coin: &str, oid: u64) {
        let a = self.coins[coin].0;
        if let Err(e) = self.ex.act(&Action::Cancel { cancels: vec![CancelWire { a, o: oid }] }).await {
            log!("live: cancel {coin} {oid}: {e:#}");
        }
    }

    async fn on_update(&mut self, u: Update) {
        if !self.coins.contains_key(&u.coin) {
            // A golden account's entry in a coin the live venue does not list (testnet lists
            // fewer coins than mainnet): recorded, so it is not taken for a missed one.
            if u.entry && u.golden && u.frac > 0.0 {
                self.skip(&u, "coin not listed on the live venue");
                self.save().await;
            }
            return;
        }
        let k = key(&u.coin, &u.user);
        let r = match self.state.legs.get(&k).map(|l| l.dir) {
            Some(dir) if u.frac <= 0.0 || u.dir != dir => {
                let r = self.close(&k, if u.frac <= 0.0 { "exit" } else { "flip" }).await;
                if r.is_ok() && u.frac > 0.0 && u.entry && u.golden {
                    self.open(&u).await
                } else {
                    r
                }
            }
            Some(_) => self.follow(&k, &u).await,
            None if u.entry && u.golden && u.frac > 0.0 => self.open(&u).await,
            None => return,
        };
        if let Err(e) = r {
            self.event(json!({"what": "error", "user": u.user, "coin": u.coin, "error": format!("{e:#}")}));
        }
        self.save().await;
    }

    fn skip(&mut self, u: &Update, why: &str) {
        self.state.skipped += 1;
        self.event(json!({"what": "skipped", "why": why, "user": u.user, "coin": u.coin}));
    }

    async fn open(&mut self, u: &Update) -> Result<()> {
        if self.state.legs.len() >= self.cfg.max_positions {
            self.skip(u, "max positions");
            return Ok(());
        }
        if self.state.in_coin(&u.coin).any(|(_, l)| l.dir != u.dir) {
            self.skip(u, "other way from the coin's legs");
            return Ok(());
        }
        // The portfolio's limits: each open leg holds `risk` of the account at its entry.
        let lim = self.limits.clone();
        let legs = |f: &dyn Fn(&Leg) -> bool| self.state.legs.values().filter(|l| f(l)).count() as f64 * lim.risk;
        let why = if legs(&|_| true) + lim.risk > lim.total + 1e-9 {
            Some("total risk")
        } else if legs(&|l| l.dir == u.dir) + lim.risk > lim.dir + 1e-9 {
            Some("risk in one direction")
        } else if legs(&|l| l.user == u.user) + lim.risk > lim.trader + 1e-9 {
            Some("risk of one trader")
        } else if self.state.in_coin(&u.coin).count() >= lim.per_coin {
            Some("legs in the coin")
        } else {
            None
        };
        if let Some(why) = why {
            self.skip(u, why);
            return Ok(());
        }
        let (equity, _) = self.equity().await?;
        self.state.peak = self.state.peak.max(equity);
        if lim.max_dd > 0.0 && equity < self.state.peak * (1.0 - lim.max_dd / 100.0) {
            self.skip(u, "drawdown brake");
            return Ok(());
        }
        let mid = self.mid(&u.coin).await?;
        let dec = self.coins[&u.coin].1;
        let full = round_size(lim.risk / self.cfg.stop_pct * equity / mid, dec);
        if full * mid < MIN_ORDER_USD {
            self.event(json!({"what": "too small", "user": u.user, "coin": u.coin, "equity": equity, "size": full}));
            return Ok(());
        }
        let size = u.dir * full * u.frac.min(1.0);
        let (got, px) = self.market(&u.coin, size, false).await?;
        if got == 0.0 {
            bail!("nothing filled");
        }
        let stop_px = px * (1.0 - u.dir * self.cfg.stop_pct / 100.0);
        let k = key(&u.coin, &u.user);
        self.state.legs.insert(k.clone(), Leg {
            coin: u.coin.clone(), user: u.user.clone(), dir: u.dir, full, size: got, entry_px: px, stop_px, stop_oid: None, opened: now(),
        });
        self.restop(&k).await;
        let oid = self.state.legs[&k].stop_oid;
        self.event(json!({"what": "open", "user": u.user, "coin": u.coin, "size": got, "px": px, "notional": got.abs() * px,
            "equity": equity, "stop_px": stop_px, "stop_oid": oid}));
        Ok(())
    }

    /// Puts the leg's stop order on for its size now (the old one cancelled): its size alone,
    /// so it does not close the coin's other legs.
    async fn restop(&mut self, k: &str) {
        let Some(l) = self.state.legs.get(k).cloned() else { return };
        if let Some(oid) = l.stop_oid {
            self.cancel(&l.coin, oid).await;
        }
        let oid = match self.stop(&l.coin, l.dir, l.size, l.stop_px).await {
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

    /// Moves the leg's size to its share of `full`, its stop order with it.
    async fn follow(&mut self, k: &str, u: &Update) -> Result<()> {
        let l = self.state.legs[k].clone();
        let mid = self.mid(&l.coin).await?;
        let target = l.dir * l.full * u.frac.min(1.0);
        if target.abs() * mid < MIN_LEFT_USD {
            return self.close(k, "small").await;
        }
        let delta = target - l.size;
        if round_size(delta.abs(), self.coins[&l.coin].1) * mid < MIN_ORDER_USD {
            return Ok(());
        }
        let reduce = target.abs() < l.size.abs();
        let (got, px) = self.market(&l.coin, delta, reduce).await?;
        let leg = self.state.legs.get_mut(k).context("leg gone")?;
        leg.size += got;
        let after = leg.size;
        self.restop(k).await;
        self.event(json!({"what": if reduce { "reduce" } else { "add" }, "user": l.user, "coin": l.coin, "size": got, "px": px,
            "pos_after": after}));
        Ok(())
    }

    async fn close(&mut self, k: &str, why: &str) -> Result<()> {
        let Some(l) = self.state.legs.get(k).cloned() else { return Ok(()) };
        let (got, px) = if l.size != 0.0 { self.market(&l.coin, -l.size, true).await? } else { (0.0, 0.0) };
        if let Some(oid) = l.stop_oid {
            self.cancel(&l.coin, oid).await;
        }
        self.state.legs.remove(k);
        let pnl = -got * (px - l.entry_px);
        self.event(json!({"what": "close", "why": why, "user": l.user, "coin": l.coin, "size": got, "px": px, "pnl_approx": pnl}));
        Ok(())
    }

    /// The exchange's positions and orders: a leg whose stop order is no longer resting was
    /// stopped out; a coin's position gone (a liquidation) or the other way ends its legs; the
    /// rest take their share of what is held. A leg without a stop order gets one. Then the
    /// status.
    async fn reconcile(&mut self) {
        let (equity, positions) = match self.equity().await {
            Ok(x) => x,
            Err(e) => {
                log!("live: account read failed: {e:#}");
                return;
            }
        };
        let resting: Option<std::collections::HashSet<u64>> = match self.ex.info(json!({"type": "frontendOpenOrders", "user": self.account})).await {
            Ok(v) => Some(v.as_array().into_iter().flatten().filter_map(|o| o["oid"].as_u64()).collect()),
            Err(e) => {
                log!("live: orders read failed: {e:#}");
                None
            }
        };
        let mut changed = false;
        if let Some(resting) = &resting {
            for (k, l) in self.state.legs.clone() {
                if l.stop_oid.is_some_and(|o| !resting.contains(&o)) {
                    self.state.legs.remove(&k);
                    self.event(json!({"what": "stopped", "user": l.user, "coin": l.coin, "size_was": l.size, "stop_px": l.stop_px,
                        "entry_px": l.entry_px}));
                    changed = true;
                }
            }
        }
        let coins: std::collections::HashSet<String> = self.state.legs.values().map(|l| l.coin.clone()).collect();
        for coin in coins {
            let held = positions.get(&coin).copied().unwrap_or(0.0);
            let legs: Vec<(String, Leg)> = self.state.in_coin(&coin).map(|(k, l)| (k.clone(), l.clone())).collect();
            let sum: f64 = legs.iter().map(|(_, l)| l.size).sum();
            if held == 0.0 || legs.iter().any(|(_, l)| held.signum() != l.dir) {
                for (k, l) in legs {
                    if let Some(oid) = l.stop_oid.filter(|_| held != 0.0) {
                        self.cancel(&coin, oid).await;
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
            for k in self.state.legs.iter().filter(|(_, l)| l.stop_oid.is_none()).map(|(k, _)| k.clone()).collect::<Vec<_>>() {
                self.restop(&k).await;
                changed = true;
            }
        }
        if changed {
            self.save().await;
        }
        let coins_held: std::collections::HashSet<&String> = self.state.legs.values().map(|l| &l.coin).collect();
        let other = positions.keys().filter(|c| !coins_held.contains(c)).count();
        *self.status.lock().unwrap() = json!({"equity": (equity * 100.0).round() / 100.0, "positions": self.state.legs.len(),
            "coins": coins_held.len(), "other_positions": other, "skipped": self.state.skipped, "orders": self.state.orders});
    }
}

/// `live-check [COIN]`: trades on the live account through the same path as golden accounts'
/// (testnet unless set): two accounts' legs in one coin, each with its own stop order; one
/// half out (its stop resized), the other out, then the first out; a third one's entry the
/// other way is skipped.
pub async fn check(cfg: &Config, coin: &str) -> Result<()> {
    let store = crate::store::open(cfg).await?;
    let (tx, _) = start(cfg, store.clone(), true).await?.context("LIVE_KEY and LIVE_ACCOUNT are not set")?;
    let account = std::env::var("LIVE_ACCOUNT")?.trim().to_lowercase();
    let base = if std::env::var("LIVE_NET").unwrap_or_default() == "mainnet" { MAINNET } else { TESTNET };
    let info = Exchange::new(base, &std::env::var("LIVE_KEY")?)?;
    let send = |user: &str, dir: f64, frac: f64, entry: bool| {
        tx.send(Update { user: format!("live-check-{user}"), coin: coin.into(), dir, frac, entry, golden: true })
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
    pause(3).await;
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
