//! Real orders for the golden list on one Hyperliquid account (testnet unless `LIVE_NET` is
//! "mainnet"), set by env: `LIVE_KEY` (its private key, or an API wallet's approved for it)
//! and `LIVE_ACCOUNT` (the account's address). Without them nothing is sent.
//!
//! The engine passes on every move of a followed account's position (`Update`); here:
//!   - a position a golden account enters is entered on our account at a size whose stop,
//!     `stop_pct` against our fill, loses `risk_pct` of our account's equity, with a stop
//!     order resting on the exchange at that price;
//!   - it is followed (reduced in proportion as the account reduces from its peak in the
//!     position, closed when it is flat) to its end, golden or not by then;
//!   - one position per coin (the account's, ours are one per coin on the exchange): another
//!     account's entry in a coin we hold is not followed; at `max_positions` open none is;
//!   - every minute the exchange's positions are read: a position gone there (our stop, or a
//!     liquidation) ends ours.
//!
//! Orders are IOC limits 5% through the mid (market orders, as the SDK places them). Prices are
//! the live account's venue's: on testnet its own books, not mainnet's.
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
    /// Coin -> our position.
    pub legs: HashMap<String, Leg>,
    /// Entries not followed: the coin held for another account, or `max_positions` open.
    pub skipped: u64,
    pub orders: u64,
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
}

/// Starts the live trader if `LIVE_KEY` and `LIVE_ACCOUNT` are set: its sender, and its status
/// (for `bot_status`).
pub async fn start(cfg: &Config, store: Store) -> Result<Option<(mpsc::UnboundedSender<Update>, Arc<Mutex<Value>>)>> {
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
    let state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    log!("live: {} account {} (signer {}{}), {} coins, {} positions held", if base == MAINNET { "MAINNET" } else { "testnet" },
        account, ex.signer(), if unified { ", unified" } else { "" }, coins.len(), state.legs.len());
    let status = Arc::new(Mutex::new(json!({})));
    let mut live = Live { ex, account, unified, coins, state, cfg: cfg.clone(), store, status: status.clone() };
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
        let px = round_px(mid * if buy { 1.0 + SLIPPAGE } else { 1.0 - SLIPPAGE }, dec);
        let sz = round_size(size.abs(), dec);
        if sz <= 0.0 {
            bail!("{coin}: size {size} rounds to 0");
        }
        let order = OrderWire { a: asset, b: buy, p: wire(px), s: wire(sz), r: reduce, t: OrderType::Limit { tif: "Ioc".into() } };
        self.state.orders += 1;
        let r = self.ex.act(&Action::Order { orders: vec![order], grouping: "na".into() }).await?;
        let st = &r["data"]["statuses"][0];
        if let Some(f) = st.get("filled") {
            let got = num(&f["totalSz"]);
            return Ok((if buy { got } else { -got }, num(&f["avgPx"]), f["oid"].as_u64().unwrap_or(0)));
        }
        bail!("{coin} order not filled: {st}");
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
        let Some(coin) = ["BTC", "ETH", "SOL"].into_iter().find(|c| self.coins.contains_key(*c) && !self.state.legs.contains_key(*c)) else {
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
            return;
        }
        let held = self.state.legs.get(&u.coin).map(|l| (l.user == u.user, l.dir));
        let r = match held {
            Some((false, _)) => {
                // The coin is held for another account: its entry is not followed.
                if u.entry && u.golden && u.frac > 0.0 {
                    self.state.skipped += 1;
                    let holder = self.state.legs[&u.coin].user.clone();
                    self.event(json!({"what": "skipped", "why": "coin held", "user": u.user, "coin": u.coin, "held_for": holder}));
                    self.save().await;
                }
                return;
            }
            Some((true, dir)) if u.frac <= 0.0 || u.dir != dir => {
                let r = self.close(&u.coin, if u.frac <= 0.0 { "exit" } else { "flip" }).await;
                if r.is_ok() && u.frac > 0.0 && u.entry && u.golden {
                    self.open(&u).await
                } else {
                    r
                }
            }
            Some(_) => self.follow(&u).await,
            None if u.entry && u.golden && u.frac > 0.0 => self.open(&u).await,
            None => return,
        };
        if let Err(e) = r {
            self.event(json!({"what": "error", "user": u.user, "coin": u.coin, "error": format!("{e:#}")}));
        }
        self.save().await;
    }

    async fn open(&mut self, u: &Update) -> Result<()> {
        if self.state.legs.len() >= self.cfg.max_positions {
            self.state.skipped += 1;
            return Ok(());
        }
        let (equity, _) = self.equity().await?;
        let mid = self.mid(&u.coin).await?;
        let dec = self.coins[&u.coin].1;
        let full = round_size(self.cfg.risk_pct / self.cfg.stop_pct * equity / mid, dec);
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
        let stop_oid = match self.stop(&u.coin, u.dir, full, stop_px).await {
            Ok(oid) => Some(oid),
            Err(e) => {
                self.event(json!({"what": "stop failed", "coin": u.coin, "error": format!("{e:#}")}));
                None
            }
        };
        self.state.legs.insert(u.coin.clone(), Leg {
            user: u.user.clone(), dir: u.dir, full, size: got, entry_px: px, stop_px, stop_oid, opened: now(),
        });
        self.event(json!({"what": "open", "user": u.user, "coin": u.coin, "size": got, "px": px, "notional": got.abs() * px,
            "equity": equity, "stop_px": stop_px, "stop_oid": stop_oid}));
        Ok(())
    }

    /// Moves our size to its share of `full` (reduces only on the exchange's side if smaller).
    async fn follow(&mut self, u: &Update) -> Result<()> {
        let l = self.state.legs[&u.coin].clone();
        let mid = self.mid(&u.coin).await?;
        let target = l.dir * l.full * u.frac.min(1.0);
        if target.abs() * mid < MIN_LEFT_USD {
            return self.close(&u.coin, "small").await;
        }
        let delta = target - l.size;
        if round_size(delta.abs(), self.coins[&u.coin].1) * mid < MIN_ORDER_USD {
            return Ok(());
        }
        let reduce = target.abs() < l.size.abs();
        let (got, px) = self.market(&u.coin, delta, reduce).await?;
        let leg = self.state.legs.get_mut(&u.coin).context("leg gone")?;
        leg.size += got;
        let after = leg.size;
        self.event(json!({"what": if reduce { "reduce" } else { "add" }, "user": u.user, "coin": u.coin, "size": got, "px": px,
            "pos_after": after}));
        Ok(())
    }

    async fn close(&mut self, coin: &str, why: &str) -> Result<()> {
        let Some(l) = self.state.legs.get(coin).cloned() else { return Ok(()) };
        let (got, px) = if l.size != 0.0 { self.market(coin, -l.size, true).await? } else { (0.0, 0.0) };
        if let Some(oid) = l.stop_oid {
            self.cancel(coin, oid).await;
        }
        self.state.legs.remove(coin);
        let pnl = -got * (px - l.entry_px);
        self.event(json!({"what": "close", "why": why, "user": l.user, "coin": coin, "size": got, "px": px, "pnl_approx": pnl}));
        Ok(())
    }

    /// The exchange's positions: one of ours gone there (our stop filled, or a liquidation)
    /// ends it; sizes are taken from there. Then the status.
    async fn reconcile(&mut self) {
        let (equity, positions) = match self.equity().await {
            Ok(x) => x,
            Err(e) => {
                log!("live: account read failed: {e:#}");
                return;
            }
        };
        let mut changed = false;
        for coin in self.state.legs.keys().cloned().collect::<Vec<_>>() {
            let held = positions.get(&coin).copied().unwrap_or(0.0);
            let l = self.state.legs[&coin].clone();
            if held == 0.0 || held.signum() != l.dir {
                if let Some(oid) = l.stop_oid.filter(|_| held != 0.0) {
                    self.cancel(&coin, oid).await;
                }
                self.state.legs.remove(&coin);
                self.event(json!({"what": "gone", "user": l.user, "coin": coin, "size_was": l.size, "stop_px": l.stop_px}));
                changed = true;
            } else if (held - l.size).abs() > 1e-12 {
                self.state.legs.get_mut(&coin).unwrap().size = held;
                changed = true;
            }
        }
        if changed {
            self.save().await;
        }
        let ours = self.state.legs.len();
        let other = positions.keys().filter(|c| !self.state.legs.contains_key(*c)).count();
        *self.status.lock().unwrap() = json!({"equity": (equity * 100.0).round() / 100.0, "positions": ours, "other_positions": other,
            "skipped": self.state.skipped, "orders": self.state.orders});
    }
}

/// `live-check [COIN]`: one trade on the live account through the same path as a golden
/// account's (entry with its stop, half out, out), to see it work (testnet unless set).
pub async fn check(cfg: &Config, coin: &str) -> Result<()> {
    let store = crate::store::open(cfg).await?;
    let (tx, _) = start(cfg, store.clone()).await?.context("LIVE_KEY and LIVE_ACCOUNT are not set")?;
    let account = std::env::var("LIVE_ACCOUNT")?.trim().to_lowercase();
    let base = if std::env::var("LIVE_NET").unwrap_or_default() == "mainnet" { MAINNET } else { TESTNET };
    let info = Exchange::new(base, &std::env::var("LIVE_KEY")?)?;
    let send = |frac: f64, entry: bool| tx.send(Update { user: "live-check".into(), coin: coin.into(), dir: 1.0, frac, entry, golden: true });
    let pause = |s| tokio::time::sleep(std::time::Duration::from_secs(s));
    pause(3).await;
    send(1.0, true)?;
    pause(6).await;
    let orders = info.info(json!({"type": "frontendOpenOrders", "user": account})).await?;
    for o in orders.as_array().into_iter().flatten().filter(|o| o["coin"] == coin) {
        log!("live-check: resting {} {} {} trigger {} ({})", o["orderType"], o["side"], o["sz"], o["triggerPx"], o["triggerCondition"]);
    }
    send(0.5, false)?;
    pause(6).await;
    send(0.0, false)?;
    pause(6).await;
    let orders = info.info(json!({"type": "frontendOpenOrders", "user": account})).await?;
    let left = orders.as_array().map(|a| a.iter().filter(|o| o["coin"] == coin).count()).unwrap_or(0);
    let st = info.info(json!({"type": "clearinghouseState", "user": account})).await?;
    let pos = st["assetPositions"].as_array().map(|a| a.iter().filter(|p| p["position"]["coin"] == coin).count()).unwrap_or(0);
    log!("live-check: {coin} after: {pos} position(s), {left} order(s) resting");
    store.flush().await;
    Ok(())
}
