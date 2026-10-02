//! The paper copier. Every followed account gets its own $1000 copy account that mirrors it
//! 1:1: for every coin, our position = its position x (our start / its equity). Nothing else
//! is ours — no caps, stops or filters (see README); only the venue's mechanics are modelled:
//!   - fills are taker fills on the live L2 book, `exec_delay_ms` after the account's fill
//!     reaches us (the order would land then), at the taker fee;
//!   - an order under $10 is not placed (exchange minimum) unless it closes the position: the
//!     difference waits for the next change;
//!   - funding is paid/received every hour on open positions at the coin's rate;
//!   - a copy account at zero equity is liquidated (closed out at the book) and stops;
//!   - position / equity over 60x means a stale equity read: the account is read again first.
//!
//! An account is enrolled on its first fill: its positions are read (clearinghouseState) and
//! mirrored at once ("seed" fills), then each of its fills moves the mirror. Its positions are
//! read again every `reconcile_s` while it is active, to correct drift.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Semaphore, mpsc};

use crate::account::{Account, round_size, walk};
use crate::api::{AccountState, Api, CoinInfo, Leader, Trigger, exchange_now, now};
use crate::config::Config;
use crate::log;
use crate::stats::{Plan, Stats, Trip};
use crate::store::{Row, Store};
use crate::ws::{Book, Books, UserFill};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Trader {
    pub address: String,
    pub name: Option<String>,
    /// Its equity used for the copy ratio (leaderboard accountValue, or perp value if larger).
    pub equity: f64,
    /// Its positions as we track them (coin -> signed size).
    pub theirs: HashMap<String, f64>,
    /// Exchange time (ms) of the last positions read: fills up to it are already in `theirs`.
    pub read_ms: u64,
    pub enrolled_at: f64,
    pub last_fill: f64,
    pub their_fills: u64,
    /// Our fills that followed one of its trades (not the seed mirror of its old positions,
    /// nor drift corrections): the activity measure.
    #[serde(default)]
    pub copy_fills: u64,
    pub acct: Account,
    #[serde(default)]
    pub stats: Stats,
    /// Last read of its set-up (leverage, liquidation, stops).
    #[serde(default)]
    pub plan_read: f64,
    /// Our open positions as trips from flat (coin -> trip).
    #[serde(default)]
    pub trips: HashMap<String, Trip>,
}

/// Its fills on one coin that one scheduled execution of ours follows.
#[derive(Clone, Copy, Debug)]
struct Pending {
    /// Exchange time of the first (ms) and when it reached us (s, exchange clock).
    time_ms: u64,
    recv: f64,
    /// Size and size x price of all of them (their average price).
    size: f64,
    size_px: f64,
}

#[derive(Debug)]
pub enum Msg {
    Fill(UserFill),
    Exec { user: String, coin: String, why: &'static str },
    Read { user: String, why: &'static str, state: anyhow::Result<AccountState> },
    /// Its positions' set-up and its stop / take-profit orders.
    Plan { user: String, state: anyhow::Result<(AccountState, Vec<Trigger>)> },
    Leaders(Vec<Leader>),
    Funding(HashMap<String, (f64, f64)>),
    Tick,
    /// Save and stop.
    Shutdown,
}

pub struct Engine {
    cfg: Config,
    api: Api,
    books: Books,
    coins: HashMap<String, CoinInfo>,
    pub followed: Arc<RwLock<HashSet<String>>>,
    leaders: HashMap<String, Leader>,
    traders: HashMap<String, Trader>,
    /// Accounts whose positions are being read.
    reading: HashSet<String>,
    /// Accounts with a set-up read scheduled.
    planning: HashSet<String>,
    /// Active accounts to read again after a start: fills between the last save and the stop
    /// are not in the saved state.
    resync: Vec<String>,
    /// (user, coin) with an execution already scheduled, and the fills it covers.
    scheduled: HashMap<(String, String), Pending>,
    tx: mpsc::UnboundedSender<Msg>,
    store: Store,
    read_limit: Arc<Semaphore>,
    dirty: bool,
}

/// An account over `stale_leverage` is read again only if its last read is older than this.
const STALE_REREAD_S: f64 = 60.0;

/// A set-up read follows an entry by this much (its stop orders usually follow the entry)...
const PLAN_AFTER_S: f64 = 10.0;
/// ... and the previous read of the same account by at least this much.
const PLAN_EVERY_S: f64 = 60.0;

fn r(x: f64, d: i32) -> f64 {
    let m = 10f64.powi(d);
    (x * m).round() / m
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(cfg: Config, api: Api, books: Books, coins: Vec<CoinInfo>, tx: mpsc::UnboundedSender<Msg>, traders: HashMap<String, Trader>,
               store: Store) -> anyhow::Result<Self> {
        log!("state: {} copy accounts", traders.len());
        let resync = traders.iter().filter(|(_, t)| !t.acct.liquidated && (!t.acct.positions.is_empty() || !t.theirs.is_empty()))
            .map(|(a, _)| a.clone()).collect();
        Ok(Self {
            api,
            books,
            coins: coins.into_iter().map(|c| (c.name.clone(), c)).collect(),
            followed: Arc::new(RwLock::new(HashSet::new())),
            leaders: HashMap::new(),
            traders,
            reading: HashSet::new(),
            planning: HashSet::new(),
            resync,
            scheduled: HashMap::new(),
            tx,
            store,
            read_limit: Arc::new(Semaphore::new(4)),
            dirty: false,
            cfg,
        })
    }

    fn write(&mut self, mut ev: serde_json::Value) {
        ev["at"] = json!(r(now(), 3));
        self.store.event(ev);
        self.dirty = true;
    }

    /// Hands every copy account (marked at the books' mids) to the store.
    fn save(&mut self) {
        let b = self.books.read().unwrap();
        let marks = |c: &str| b.get(c).and_then(Book::mid);
        let rows = self.traders.iter().filter_map(|(a, t)| {
            let equity = t.acct.equity(&marks);
            Some(Row {
                address: a.clone(),
                name: t.name.clone(),
                equity,
                roi_pct: if t.acct.start > 0.0 { (equity / t.acct.start - 1.0) * 100.0 } else { 0.0 },
                copy_fills: t.copy_fills as i64,
                open_positions: t.acct.positions.len() as i32,
                liquidated: t.acct.liquidated,
                state: serde_json::to_string(t).ok()?,
            })
        }).collect();
        drop(b);
        self.store.save(rows);
        self.dirty = false;
    }

    pub async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let mut last_save = now();
        while let Some(msg) = rx.recv().await {
            match msg {
                Msg::Fill(f) => self.on_fill(f),
                Msg::Exec { user, coin, why } => self.exec(&user, &coin, why),
                Msg::Read { user, why, state } => self.on_read(user, why, state),
                Msg::Plan { user, state } => self.on_plan(user, state),
                Msg::Leaders(l) => self.on_leaders(l),
                Msg::Funding(ctx) => self.on_funding(ctx),
                Msg::Tick => self.on_tick(),
                Msg::Shutdown => {
                    self.save();
                    self.store.flush().await;
                    log!("stopped, state saved");
                    return;
                }
            }
            if self.dirty && now() - last_save > 30.0 {
                self.save();
                last_save = now();
            }
        }
    }

    fn on_leaders(&mut self, leaders: Vec<Leader>) {
        let (min, min_pnl, min_roi) = (self.cfg.min_equity, self.cfg.min_month_pnl, self.cfg.min_month_roi);
        let mut set: HashSet<String> = HashSet::new();
        self.leaders.clear();
        for l in leaders {
            if l.month_volume > 0.0 && l.account_value >= min && l.month_pnl >= min_pnl && l.month_roi > min_roi {
                set.insert(l.address.clone());
            }
            self.leaders.insert(l.address.clone(), l);
        }
        // Accounts we already copy stay followed while their copy holds positions.
        for (a, t) in &self.traders {
            if !t.acct.positions.is_empty() && !t.acct.liquidated {
                set.insert(a.clone());
            }
        }
        let n = set.len();
        *self.followed.write().unwrap() = set;
        log!(
            "leaderboard: {n} accounts followed (this month: traded, PnL >= ${min_pnl}, ROI > {}%; equity >= ${min})",
            min_roi * 100.0
        );
    }

    fn read_account(&mut self, user: &str, why: &'static str) {
        if !self.reading.insert(user.to_string()) {
            return;
        }
        let (api, tx, limit, user) = (self.api.clone(), self.tx.clone(), self.read_limit.clone(), user.to_string());
        tokio::spawn(async move {
            let _p = limit.acquire().await;
            let state = api.account(&user).await;
            let _ = tx.send(Msg::Read { user, why, state });
        });
    }

    fn on_fill(&mut self, f: UserFill) {
        if !self.coins.contains_key(&f.coin) {
            return;
        }
        // Every fill of a followed account, as it reached us: its own activity.
        let applied = self.traders.get(&f.user).is_some_and(|t| !t.acct.liquidated && f.time_ms > t.read_ms);
        let pos_after = self.traders.get(&f.user).filter(|_| applied).map(|t| t.theirs.get(&f.coin).copied().unwrap_or(0.0) + f.delta);
        self.write(json!({"kind": "their_fill", "user": f.user, "coin": f.coin, "size": f.delta, "px": f.px,
            "time_ms": f.time_ms, "tid": f.tid, "feed_s": r(f.recv - f.time_ms as f64 / 1000.0, 3), "pos_after": pos_after}));
        let Some(t) = self.traders.get_mut(&f.user) else {
            // First sight: read its positions, then mirror them.
            self.read_account(&f.user.clone(), "enroll");
            return;
        };
        if t.acct.liquidated || f.time_ms <= t.read_ms {
            return;
        }
        *t.theirs.entry(f.coin.clone()).or_insert(0.0) += f.delta;
        if t.theirs.get(&f.coin).is_some_and(|s| s.abs() < 1e-9) {
            t.theirs.remove(&f.coin);
        }
        t.last_fill = f.recv;
        t.their_fills += 1;
        self.dirty = true;
        let key = (f.user.clone(), f.coin.clone());
        if let Some(p) = self.scheduled.get_mut(&key) {
            p.size += f.delta.abs();
            p.size_px += f.delta.abs() * f.px;
        } else {
            let p = Pending { time_ms: f.time_ms, recv: f.recv, size: f.delta.abs(), size_px: f.delta.abs() * f.px };
            self.scheduled.insert(key, p);
            let (tx, delay) = (self.tx.clone(), self.cfg.exec_delay_ms);
            let (user, coin) = (f.user, f.coin);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                let _ = tx.send(Msg::Exec { user, coin, why: "copy" });
            });
        }
    }

    fn on_read(&mut self, user: String, why: &'static str, state: anyhow::Result<AccountState>) {
        self.reading.remove(&user);
        let state = match state {
            Ok(s) => s,
            Err(e) => {
                log!("read {}: {e}", &user[..10]);
                return;
            }
        };
        let lb = self.leaders.get(&user).cloned();
        let equity = lb.as_ref().map(|l| l.account_value).unwrap_or(0.0).max(state.account_value);
        let start = self.cfg.start_usd;
        let coins = &self.coins;
        let theirs: HashMap<String, f64> = state.positions.into_iter().filter(|(c, _)| coins.contains_key(c)).collect();
        let new = !self.traders.contains_key(&user);
        let t = self.traders.entry(user.clone()).or_insert_with(|| Trader {
            address: user.clone(),
            name: lb.as_ref().and_then(|l| l.name.clone()),
            enrolled_at: now(),
            acct: Account::new(start),
            ..Default::default()
        });
        if equity <= 0.0 {
            return;
        }
        t.equity = equity;
        t.read_ms = state.time_ms;
        // Coins where it or we hold something: mirror to its current positions.
        let mut coins_now: HashSet<String> = theirs.keys().cloned().collect();
        coins_now.extend(t.acct.positions.keys().cloned());
        t.theirs = theirs;
        self.dirty = true;
        if new {
            self.write(json!({"kind": "enroll", "user": user, "equity": r(equity, 2), "positions": coins_now.len()}));
        }
        let why = if new { "seed" } else { why };
        for c in coins_now {
            self.exec(&user, &c, why);
        }
        self.read_plan(&user);
    }

    /// Reads its set-up (leverage, liquidation price, stop orders) for our open trips: soon
    /// after it enters (stops usually follow the entry), at most once a minute.
    fn read_plan(&mut self, user: &str) {
        let Some(t) = self.traders.get(user) else { return };
        if t.trips.is_empty() || !self.planning.insert(user.to_string()) {
            return;
        }
        let at = (now() + PLAN_AFTER_S).max(t.plan_read + PLAN_EVERY_S);
        let (api, tx, limit, user) = (self.api.clone(), self.tx.clone(), self.read_limit.clone(), user.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f64((at - now()).max(0.0))).await;
            let _p = limit.acquire().await;
            let state = match api.account(&user).await {
                Ok(s) => api.triggers(&user).await.map(|t| (s, t)),
                Err(e) => Err(e),
            };
            let _ = tx.send(Msg::Plan { user, state });
        });
    }

    fn on_plan(&mut self, user: String, state: anyhow::Result<(AccountState, Vec<Trigger>)>) {
        self.planning.remove(&user);
        let (state, triggers) = match state {
            Ok(s) => s,
            Err(e) => {
                log!("set-up read {}: {e}", &user[..10]);
                return;
            }
        };
        let Some(t) = self.traders.get_mut(&user) else { return };
        t.plan_read = now();
        let equity = if t.equity > 0.0 { t.equity } else { state.account_value };
        let mut evs = Vec::new();
        for (coin, trip) in t.trips.iter_mut() {
            let (Some(&size), Some(s)) = (state.positions.get(coin), state.setups.get(coin)) else { continue };
            // Its stops on the side that closes the position.
            let stops: Vec<(f64, Option<f64>)> =
                triggers.iter().filter(|o| &o.coin == coin && o.stop && o.sell == (size > 0.0)).map(|o| (o.trigger_px, o.size)).collect();
            let tps = triggers.iter().filter(|o| &o.coin == coin && !o.stop && o.sell == (size > 0.0)).count();
            let plan = Plan::new(size, s.entry, s.isolated, s.leverage, s.liq_px, &stops, equity);
            evs.push(json!({"kind": "plan", "user": user, "coin": coin, "their_size": size, "entry": s.entry,
                "margin": if s.isolated { "isolated" } else { "cross" }, "leverage": s.leverage, "liq_px": s.liq_px,
                "stops": stops.iter().map(|x| x.0).collect::<Vec<_>>(), "take_profits": tps,
                "stop_cover": r(plan.stop_cover, 3), "risk_pct": r(plan.risk_pct, 3)}));
            trip.plan(plan);
        }
        for ev in evs {
            self.write(ev);
        }
    }

    /// Moves our position in `coin` to the mirror of theirs, at the book.
    fn exec(&mut self, user: &str, coin: &str, why: &'static str) {
        let first = self.scheduled.remove(&(user.to_string(), coin.to_string()));
        let Some(info) = self.coins.get(coin).cloned() else { return };
        let Some(book) = self.books.read().unwrap().get(coin).cloned() else { return };
        let Some(mid) = book.mid() else { return };
        let (min_order, max_lev, fee_rate) = (self.cfg.min_order_usd, self.cfg.stale_leverage, self.cfg.taker_fee);
        let (equity, theirs, ours, start, read_ms) = match self.traders.get(user) {
            Some(t) if !t.acct.liquidated && t.equity > 0.0 => (t.equity, t.theirs.clone(), t.acct.size(coin), t.acct.start, t.read_ms),
            _ => return,
        };
        let ratio = start / equity;
        let target = theirs.get(coin).copied().unwrap_or(0.0) * ratio;
        let (gross, equity_now) = {
            let b = self.books.read().unwrap();
            let gross: f64 = theirs.iter().map(|(c, s)| (s * ratio).abs() * b.get(c).and_then(|x| x.mid()).unwrap_or(0.0)).sum();
            let marks = |c: &str| b.get(c).and_then(Book::mid);
            (gross, self.traders.get(user).map(|t| t.acct.equity(&marks)).unwrap_or(0.0))
        };
        if gross / start > max_lev {
            // Its equity is out of date (or far over any real leverage): read it again first,
            // unless it was just read (then it really is that far over, e.g. about to be
            // liquidated: not copied until that changes).
            if exchange_now() - read_ms as f64 / 1000.0 > STALE_REREAD_S {
                let user = user.to_string();
                self.read_account(&user, "stale equity");
            }
            return;
        }
        let closing = target.abs() < 1e-12;
        let mut delta = round_size(target - ours, info.sz_decimals);
        if closing {
            delta = -ours;
        }
        if delta.abs() < 1e-12 || (!closing && delta.abs() * mid < min_order) {
            return;
        }
        let side = if delta > 0.0 { &book.asks } else { &book.bids };
        let (got, px) = walk(side, delta.abs());
        if got <= 0.0 {
            return;
        }
        let best = side[0].0;
        let Some(t) = self.traders.get_mut(user) else { return };
        let filled = got * delta.signum();
        let fee = got * px * fee_rate;
        let at = now();
        let realized = book_fill(t, coin, filled, px, fee, equity_now, at, why == "copy");
        let opened = t.acct.size(coin).abs() > ours.abs() || t.acct.size(coin).signum() * ours.signum() < 0.0;
        if why == "copy" {
            t.copy_fills += 1;
        }
        let mut ev = json!({"kind": "fill", "why": why, "user": user, "coin": coin, "size": filled, "px": px,
            "notional": r(got * px, 4), "fee": r(fee, 6), "realized": r(realized, 6), "pos_after": t.acct.size(coin),
            "their_px": null, "slip_bps": null, "lag_s": 0.0, "book_ms": book.time_ms});
        if let Some(p) = first.filter(|p| p.size > 0.0) {
            // + = worse for us than its price: the best price had moved (delay, spread), and
            // our order walked the book past it.
            let (their_px, s) = (p.size_px / p.size, delta.signum());
            let lag = exchange_now() - p.time_ms as f64 / 1000.0;
            let feed = p.recv - p.time_ms as f64 / 1000.0;
            let (mv, impact) = (got * (best - their_px) * s, got * (px - best) * s);
            t.stats.copy(lag, feed, got * px, mv + impact, mv, impact);
            ev["their_px"] = json!(r(their_px, 8));
            ev["slip_bps"] = json!(r((px / their_px - 1.0) * 1e4 * s, 2));
            ev["move_bps"] = json!(r((best / their_px - 1.0) * 1e4 * s, 2));
            ev["lag_s"] = json!(r(lag, 3));
            ev["feed_s"] = json!(r(feed, 3));
        }
        self.write(ev);
        if opened && why == "copy" {
            self.read_plan(user);
        }
    }

    fn on_funding(&mut self, ctx: HashMap<String, (f64, f64)>) {
        let mut total = 0.0;
        for t in self.traders.values_mut() {
            if t.acct.liquidated {
                continue;
            }
            let mut paid = 0.0;
            for (c, p) in &t.acct.positions {
                if let Some((rate, mark)) = ctx.get(c) {
                    // Longs pay a positive rate.
                    paid -= p.size * mark * rate;
                }
            }
            if paid != 0.0 {
                t.acct.cash += paid;
                t.acct.funding += paid;
                total += paid;
            }
        }
        self.dirty = true;
        log!("funding: ${total:.2} across copy accounts");
        let live: Vec<&Trader> = self.traders.values().filter(|t| !t.acct.liquidated).collect();
        let (weight, backlog) = self.api.usage();
        log!(
            "status: {} copy accounts ({} liquidated), {} followed, {} positions open; {} copy fills in all; \
             API weight {weight} in the last hour, backlog {backlog:.0} s",
            self.traders.len(),
            self.traders.len() - live.len(),
            self.followed.read().unwrap().len(),
            live.iter().map(|t| t.acct.positions.len()).sum::<usize>(),
            self.traders.values().map(|t| t.copy_fills).sum::<u64>(),
        );
    }

    /// Liquidations and reconciliation of active accounts.
    fn on_tick(&mut self) {
        let books = self.books.clone();
        let b = books.read().unwrap();
        let marks = |c: &str| b.get(c).and_then(Book::mid);
        let mut liquidate = Vec::new();
        let mut stale = Vec::new();
        let n = now();
        for (a, t) in self.traders.iter_mut() {
            if t.acct.liquidated {
                continue;
            }
            let equity = t.acct.equity(&marks);
            mark(t, &marks, equity);
            if !t.acct.positions.is_empty() && equity <= 0.0 {
                liquidate.push(a.clone());
            }
            let active = !t.acct.positions.is_empty() || !t.theirs.is_empty();
            if active && n - (t.read_ms as f64 / 1000.0) > self.cfg.reconcile_s {
                stale.push(a.clone());
            }
        }
        drop(b);
        for a in liquidate {
            self.liquidate(&a);
        }
        for a in stale.into_iter().take(20) {
            self.read_account(&a, "reconcile");
        }
        let n = self.resync.len().saturating_sub(20);
        for a in self.resync.split_off(n) {
            self.read_account(&a, "restart");
        }
    }

    fn liquidate(&mut self, user: &str) {
        let coins: Vec<String> = self.traders.get(user).map(|t| t.acct.positions.keys().cloned().collect()).unwrap_or_default();
        let fee_rate = self.cfg.taker_fee;
        for c in coins {
            let Some(book) = self.books.read().unwrap().get(&c).cloned() else { continue };
            let Some(t) = self.traders.get_mut(user) else { return };
            let ours = t.acct.size(&c);
            let (got, px) = walk(if ours < 0.0 { &book.asks } else { &book.bids }, ours.abs());
            if got > 0.0 {
                book_fill(t, &c, -ours.signum() * got, px, got * px * fee_rate, 0.0, now(), false);
            }
        }
        if let Some(t) = self.traders.get_mut(user) {
            t.acct.liquidated = true;
            let cash = t.acct.cash;
            log!("{} copy account liquidated (equity {cash:.2})", &user[..10]);
            self.write(json!({"kind": "liquidated", "user": user, "cash": r(cash, 4)}));
        }
    }
}

/// Fills our copy of `t` and keeps its trips and stats: `equity` is ours just before the fill,
/// `entry` counts an opening / adding fill as a bet of ours.
#[allow(clippy::too_many_arguments)]
fn book_fill(t: &mut Trader, coin: &str, filled: f64, px: f64, fee: f64, equity: f64, at: f64, entry: bool) -> f64 {
    let before = t.acct.size(coin);
    let realized = t.acct.fill(coin, filled, px, fee);
    let after = t.acct.size(coin);
    // The part that closes (up to |before|) and the part that opens or adds.
    let closing = if before != 0.0 && before.signum() != filled.signum() { filled.abs().min(before.abs()) } else { 0.0 };
    let opening = filled.abs() - closing;
    if closing > 0.0 {
        if let Some(mut trip) = t.trips.remove(coin) {
            trip.pnl += realized - fee * closing / filled.abs();
            if closing >= before.abs() - 1e-12 {
                t.stats.close(&trip, at);
            } else {
                let entry = t.acct.positions.get(coin).map(|p| p.entry).unwrap_or(px);
                trip.mark(after * (px - entry), after.abs() * px);
                t.trips.insert(coin.to_string(), trip);
            }
        }
    }
    if opening > 1e-12 && after != 0.0 {
        let trip = t.trips.entry(coin.to_string()).or_insert_with(|| Trip::new(at, equity));
        trip.pnl -= fee * opening / filled.abs();
        trip.mark(0.0, after.abs() * px);
        if entry {
            t.stats.entry(opening * px, equity);
        }
    }
    realized
}

/// Marks `t`'s copy at `marks`: equity curve, positions held at once, trips' worst point.
fn mark(t: &mut Trader, marks: &dyn Fn(&str) -> Option<f64>, equity: f64) {
    let mut gross = 0.0;
    for (c, p) in &t.acct.positions {
        let Some(m) = marks(c) else { continue };
        gross += p.size.abs() * m;
        if let Some(trip) = t.trips.get_mut(c) {
            trip.mark(p.size * (m - p.entry), p.size.abs() * m);
        }
    }
    t.stats.mark(equity, t.acct.positions.len(), gross);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trips_through_add_reduce_flip() {
        let mut t = Trader { acct: Account::new(1000.0), ..Default::default() };
        book_fill(&mut t, "ETH", 0.1, 2000.0, 0.1, 1000.0, 0.0, true); // open $200
        book_fill(&mut t, "ETH", 0.1, 2100.0, 0.1, 1010.0, 10.0, true); // add $210
        assert_eq!(t.stats.entries, 2);
        assert!((t.stats.entry_pct_max - 210.0 / 1010.0 * 100.0).abs() < 1e-9);
        book_fill(&mut t, "ETH", -0.1, 2000.0, 0.1, 1000.0, 20.0, true); // reduce at a loss
        assert_eq!(t.stats.trips, 0);
        assert_eq!(t.stats.entries, 2);
        book_fill(&mut t, "ETH", -0.2, 2200.0, 0.2, 1000.0, 30.0, true); // close 0.1, open 0.1 short
        assert_eq!((t.stats.trips, t.stats.wins), (1, 1));
        // -5 - 0.1 + 15 - 0.1 (fees: open, add, reduce, half of the flip's)
        assert!((t.stats.win_usd - (-5.0 + 15.0 - 0.1 - 0.1 - 0.1 - 0.1)).abs() < 1e-9);
        assert!((t.stats.hold_s - 30.0).abs() < 1e-9);
        assert_eq!(t.trips["ETH"].opened, 30.0);
        assert!((t.trips["ETH"].pnl + 0.1).abs() < 1e-9);
        book_fill(&mut t, "ETH", 0.1, 2300.0, 0.1, 990.0, 40.0, false); // close short at a loss
        assert_eq!((t.stats.trips, t.stats.wins), (2, 1));
        assert!((t.stats.loss_usd - 10.2).abs() < 1e-9);
        assert!(t.trips.is_empty());
        assert_eq!(t.stats.entries, 3);
    }
}
