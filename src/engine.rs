//! The paper copier. Every followed account gets its own $1000 copy account that follows its
//! trades with our own sizing and stop:
//!   - when it opens a position (from flat, or flips), we open in its direction sized so that
//!     our stop, `stop_pct` against our entry, loses `risk_pct` of our equity (2% / 20%: a
//!     position of 10% of equity);
//!   - while it adds, we hold that size; as it reduces from its largest size in the position
//!     we reduce in proportion, and we close when it is flat;
//!   - at our stop we close and stay out of that position until it is flat;
//!   - positions it held before we followed it are not entered, nor new ones while all our
//!     copies together hold `max_positions` (50).
//!
//! Only the venue's mechanics are modelled beyond that:
//!   - fills are taker fills on the live L2 book, `exec_delay_ms` after the account's fill
//!     reaches us (the order would land then), at the taker fee;
//!   - an order under $10 is not placed (exchange minimum) unless it closes the position: the
//!     difference waits for the next change;
//!   - funding is paid/received every hour on open positions at the coin's rate;
//!   - a copy account at zero equity is liquidated (closed out at the book) and stops.
//!
//! An account is enrolled on its first fill: its positions are read (clearinghouseState), then
//! each of its fills moves our copy. Its positions are read again every `reconcile_s` while it
//! is active, to correct drift.
//!
//! Followed: the day's list (`stable`), the golden list, and any account our copy still holds
//! a position of. The golden list is the accounts whose copy makes money: measured from its
//! enrollment (or, for copies from before the list, from its first start), after
//! `GOLDEN_MIN_DAYS` and `GOLDEN_MIN_TRIPS` closed trips, its copy's PnL above zero; until then
//! an account keeps its place (the list started with the accounts put on it by hand, see the
//! migrations). A golden account is followed even after it drops off the day's list.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Semaphore, mpsc};

use crate::account::{Account, round_size, walk};
use crate::api::{AccountState, Api, CoinCtx, CoinInfo, Leader, Trigger, exchange_now, now};
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
    /// Our fills that followed one of its trades (not resizes of positions held before,
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
    /// Its positions we follow (coin -> leg).
    #[serde(default)]
    pub legs: HashMap<String, Leg>,
    /// Where the golden list measures our copy from.
    #[serde(default)]
    pub base: Option<Base>,
    #[serde(default)]
    pub golden: bool,
    /// Its entries not followed: our copies held `max_positions` already.
    #[serde(default)]
    pub skipped: u64,
}

/// Our copy when its measuring for the golden list began.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Base {
    pub at: f64,
    pub equity: f64,
    /// Closed trips then.
    pub trips: u64,
}

/// The golden list: a copy measured this long, with this many trips closed since...
const GOLDEN_MIN_DAYS: f64 = 3.0;
const GOLDEN_MIN_TRIPS: u64 = 5;

/// ... and at a profit since: its PnL since, and whether it is golden (until it is measured
/// that long, it stays as it is: on the list if it was put there).
fn measured(t: &Trader, equity: f64, at: f64) -> (f64, bool) {
    let Some(b) = &t.base else { return (0.0, t.golden && !t.acct.liquidated) };
    let pnl = equity - b.equity;
    let long = at - b.at >= GOLDEN_MIN_DAYS * 86400.0 && t.stats.trips.saturating_sub(b.trips) >= GOLDEN_MIN_TRIPS;
    (pnl, !t.acct.liquidated && if long { pnl > 0.0 } else { t.golden })
}

/// Our side of one of its positions, from its entry until it is flat again.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Leg {
    /// Its direction (+1 long, -1 short).
    pub dir: f64,
    /// Its largest size in the position.
    pub peak: f64,
    /// Our size while it holds its largest: set when it enters, from the risk to our stop.
    pub full: f64,
    /// Not followed: our stop was hit, or it held the position before we followed it.
    pub out: bool,
}

/// Our target size in `coin` for its position `theirs`. A position it enters (`entry`: we saw
/// its fill open it) is ours at `full`, and followed down in proportion as it reduces from its
/// peak; one it already held is not entered, except that a copy we hold is kept (resized).
fn target(legs: &mut HashMap<String, Leg>, coin: &str, theirs: f64, ours: f64, entry: bool, full: f64) -> f64 {
    if theirs.abs() < 1e-12 {
        legs.remove(coin);
        return 0.0;
    }
    let dir = theirs.signum();
    let leg = legs.entry(coin.to_string()).or_default();
    if leg.dir == dir {
        leg.peak = leg.peak.max(theirs.abs());
    } else {
        *leg = Leg { dir, peak: theirs.abs(), full, out: !(entry || ours * dir > 0.0) };
    }
    if leg.out { 0.0 } else { dir * leg.full * theirs.abs() / leg.peak }
}

/// Its fills on one coin that one scheduled execution of ours follows.
#[derive(Clone, Copy, Debug)]
struct Pending {
    /// Exchange time of the first (ms) and when it reached us (s, exchange clock).
    time_ms: u64,
    recv: f64,
    /// Size and size x price of all of them (their average price), and their signed sum.
    size: f64,
    size_px: f64,
    net: f64,
}

#[derive(Debug)]
pub enum Msg {
    Fill(UserFill),
    Exec { user: String, coin: String, why: &'static str },
    Read { user: String, why: &'static str, state: anyhow::Result<AccountState> },
    /// Its positions' set-up and its stop / take-profit orders.
    Plan { user: String, state: anyhow::Result<(AccountState, Vec<Trigger>)> },
    Leaders(Vec<Leader>),
    /// The day's list of traders to follow (`stable`).
    Selected(crate::stable::Selection),
    Funding(HashMap<String, CoinCtx>),
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
    /// Accounts being enrolled, and the coin and size of the fill that brought them.
    enrolling: HashMap<String, (String, f64)>,
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
    /// Traders on the day's list (`stable`).
    listed: HashSet<String>,
    /// Time spent per tick since the last status line: ticks, total and longest (ms).
    load: Load,
    /// Real orders for the golden list (`live`): its updates and its status.
    live: Option<(mpsc::UnboundedSender<crate::live::Update>, Arc<std::sync::Mutex<serde_json::Value>>)>,
}

#[derive(Default)]
struct Load {
    ticks: u64,
    tick_ms: f64,
    tick_max_ms: f64,
}

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
            enrolling: HashMap::new(),
            planning: HashSet::new(),
            resync,
            scheduled: HashMap::new(),
            tx,
            store,
            read_limit: Arc::new(Semaphore::new(4)),
            dirty: false,
            listed: HashSet::new(),
            load: Load::default(),
            live: None,
            cfg,
        })
    }

    pub fn set_live(&mut self, live: (mpsc::UnboundedSender<crate::live::Update>, Arc<std::sync::Mutex<serde_json::Value>>)) {
        self.live = Some(live);
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
                golden: t.golden,
                measured_pnl: t.base.as_ref().map(|b| equity - b.equity).unwrap_or(0.0),
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
                Msg::Selected(s) => self.on_selected(s),
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

    /// The leaderboard: names and equity of the accounts (who is followed is `on_selected`).
    fn on_leaders(&mut self, leaders: Vec<Leader>) {
        self.leaders = leaders.into_iter().map(|l| (l.address.clone(), l)).collect();
    }

    fn on_selected(&mut self, sel: crate::stable::Selection) {
        self.listed = sel.addresses();
        let n = self.refollow();
        log!("selection: {n} traders followed ({} picked of {} read, the rest golden or holding our copies)", sel.picks.len(), sel.read);
    }

    /// Follows the day's list, the golden list and the accounts our copy holds positions of.
    fn refollow(&mut self) -> usize {
        let mut set = self.listed.clone();
        set.extend(self.traders.iter().filter(|(_, t)| !t.acct.liquidated && (t.golden || !t.acct.positions.is_empty())).map(|(a, _)| a.clone()));
        let n = set.len();
        *self.followed.write().unwrap() = set;
        n
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
            // First sight: read its positions; the position this fill opened is followed.
            self.enrolling.entry(f.user.clone()).or_insert((f.coin.clone(), f.delta));
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
            p.net += f.delta;
        } else {
            let p = Pending { time_ms: f.time_ms, recv: f.recv, size: f.delta.abs(), size_px: f.delta.abs() * f.px, net: f.delta };
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
        let first = self.enrolling.remove(&user);
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
            base: Some(Base { at: now(), equity: start, trips: 0 }),
            ..Default::default()
        });
        if equity <= 0.0 {
            return;
        }
        t.equity = equity;
        t.read_ms = state.time_ms;
        // Coins where it or we hold something: move ours to our target for its positions.
        let mut coins_now: HashSet<String> = theirs.keys().cloned().collect();
        coins_now.extend(t.acct.positions.keys().cloned());
        t.theirs = theirs;
        // On enrollment, the position its first fill opened (it was flat or the other way
        // before it) is an entry like any later one; the rest it held before.
        let entered = first.filter(|(c, d)| {
            let size = t.theirs.get(c).copied().unwrap_or(0.0);
            size.abs() > 1e-12 && size * (size - d) <= 1e-12
        }).map(|(c, _)| c);
        self.dirty = true;
        if new {
            self.write(json!({"kind": "enroll", "user": user, "equity": r(equity, 2), "positions": coins_now.len()}));
        }
        for c in coins_now {
            let why = if entered.as_ref() == Some(&c) { "copy" } else if new { "seed" } else { why };
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

    /// Moves our position in `coin` to our target for its position (`target`), at the book.
    fn exec(&mut self, user: &str, coin: &str, why: &'static str) {
        let first = self.scheduled.remove(&(user.to_string(), coin.to_string()));
        let Some(info) = self.coins.get(coin).cloned() else { return };
        let Some(book) = self.books.read().unwrap().get(coin).cloned() else { return };
        let Some(mid) = book.mid() else { return };
        let (min_order, fee_rate) = (self.cfg.min_order_usd, self.cfg.taker_fee);
        let equity_now = {
            let b = self.books.read().unwrap();
            let marks = |c: &str| b.get(c).and_then(Book::mid);
            match self.traders.get(user) {
                Some(t) if !t.acct.liquidated => t.acct.equity(&marks),
                _ => return,
            }
        };
        // Our size for a position it enters: the stop away, it loses `risk_pct` of our equity.
        let full = (self.cfg.risk_pct / self.cfg.stop_pct * equity_now.max(0.0) / mid).max(0.0);
        let open: usize = self.traders.values().filter(|t| !t.acct.liquidated).map(|t| t.acct.positions.len()).sum();
        let Some(t) = self.traders.get_mut(user) else { return };
        let ours = t.acct.size(coin);
        let theirs = t.theirs.get(coin).copied().unwrap_or(0.0);
        // An entry: its fills opened the position (it was flat or the other way before them);
        // a copy without fills pending is the one its enrollment fill opened.
        let mut entry = why == "copy" && first.is_none_or(|p| (theirs - p.net) * theirs <= 1e-12);
        let opened = entry;
        // At `max_positions` open over all copies, a new position is not entered (a flip of
        // one we hold is).
        if entry && ours == 0.0 && open >= self.cfg.max_positions && !t.legs.get(coin).is_some_and(|l| l.dir == theirs.signum()) {
            entry = false;
            t.skipped += 1;
        }
        let target = target(&mut t.legs, coin, theirs, ours, entry, full);
        // The live account follows its position on its own (golden accounts' entries).
        if let Some((live, _)) = &self.live {
            let peak = t.legs.get(coin).map(|l| l.peak).unwrap_or(0.0);
            let frac = if peak > 0.0 { theirs.abs() / peak } else { 0.0 };
            let _ = live.send(crate::live::Update { user: user.to_string(), coin: coin.to_string(), dir: theirs.signum(), frac,
                entry: opened, golden: t.golden });
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

    fn on_funding(&mut self, ctx: HashMap<String, CoinCtx>) {
        let mut total = 0.0;
        for t in self.traders.values_mut() {
            if t.acct.liquidated {
                continue;
            }
            let mut paid = 0.0;
            for (c, p) in &t.acct.positions {
                if let Some(x) = ctx.get(c) {
                    // Longs pay a positive rate.
                    paid -= p.size * x.mark * x.funding;
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
        log!(
            "status: {} copy accounts ({} liquidated), {} followed, {} positions open; {} copy fills in all",
            self.traders.len(),
            self.traders.len() - live.len(),
            self.followed.read().unwrap().len(),
            live.iter().map(|t| t.acct.positions.len()).sum::<usize>(),
            self.traders.values().map(|t| t.copy_fills).sum::<u64>(),
        );
    }

    /// Liquidations and reconciliation of active accounts.
    fn on_tick(&mut self) {
        let started = std::time::Instant::now();
        let books = self.books.clone();
        let b = books.read().unwrap();
        let marks = |c: &str| b.get(c).and_then(Book::mid);
        let mut liquidate = Vec::new();
        let mut stops = Vec::new();
        let mut stale = Vec::new();
        let n = now();
        let stop = self.cfg.stop_pct / 100.0;
        for (a, t) in self.traders.iter_mut() {
            if t.acct.liquidated {
                continue;
            }
            let equity = t.acct.equity(&marks);
            mark(t, &marks, equity);
            // Copies from before the golden list are measured from now on (once every position
            // has a price).
            if t.base.is_none() && t.acct.positions.keys().all(|c| marks(c).is_some()) {
                t.base = Some(Base { at: n, equity, trips: t.stats.trips });
            }
            if !t.acct.positions.is_empty() && equity <= 0.0 {
                liquidate.push(a.clone());
            }
            for (c, p) in &t.acct.positions {
                if marks(c).is_some_and(|m| (m / p.entry - 1.0) * p.size.signum() <= -stop) {
                    stops.push((a.clone(), c.clone()));
                }
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
        for (a, c) in stops {
            self.stop_out(&a, &c);
        }
        let tick = started.elapsed().as_secs_f64() * 1000.0;
        self.load.ticks += 1;
        self.load.tick_ms += tick;
        self.load.tick_max_ms = self.load.tick_max_ms.max(tick);
        if self.load.ticks >= 120 {
            self.status();
        }
        for a in stale.into_iter().take(20) {
            self.read_account(&a, "reconcile");
        }
        let n = self.resync.len().saturating_sub(20);
        for a in self.resync.split_off(n) {
            self.read_account(&a, "restart");
        }
    }

    /// Every 10 min (120 ticks): the bot's health to `bot_status` — what it follows and holds,
    /// the API budget, and how long its ticks take (they run on the one loop the copies use).
    fn status(&mut self) {
        let (golden, golden_pnl) = self.golden();
        let l = std::mem::take(&mut self.load);
        let n = l.ticks.max(1) as f64;
        let (weight, backlog) = self.api.usage();
        let live: Vec<&Trader> = self.traders.values().filter(|t| !t.acct.liquidated).collect();
        let st = json!({
            "at": r(now(), 3),
            "copy_accounts": self.traders.len(),
            "liquidated": self.traders.len() - live.len(),
            "followed": self.followed.read().unwrap().len(),
            "selected": self.listed.len(),
            "golden": golden,
            "golden_pnl": r(golden_pnl, 2),
            "live": self.live.as_ref().map(|(_, s)| s.lock().unwrap().clone()),
            "open_positions": live.iter().map(|t| t.acct.positions.len()).sum::<usize>(),
            "copy_fills": self.traders.values().map(|t| t.copy_fills).sum::<u64>(),
            "api_weight": weight,
            "api_backlog_s": r(backlog, 1),
            "ticks": l.ticks,
            "tick_avg_ms": r(l.tick_ms / n, 2),
            "tick_max_ms": r(l.tick_max_ms, 2),
        });
        log!("status: {st}");
        self.store.status(st);
    }

    /// Works out the golden list again (an event for each account that joins or leaves it) and
    /// follows it: how many are on it and their copies' PnL since measured.
    fn golden(&mut self) -> (usize, f64) {
        let b = self.books.read().unwrap();
        let marks = |c: &str| b.get(c).and_then(Book::mid);
        let at = now();
        let (mut evs, mut n, mut sum) = (Vec::new(), 0, 0.0);
        for (a, t) in self.traders.iter_mut() {
            let (pnl, golden) = measured(t, t.acct.equity(&marks), at);
            if golden != t.golden {
                t.golden = golden;
                let days = t.base.as_ref().map(|b| (at - b.at) / 86400.0).unwrap_or(0.0);
                log!("golden: {} {} (PnL ${pnl:.2} over {days:.1} d)", &a[..10], if golden { "joins" } else { "leaves" });
                evs.push(json!({"kind": "golden", "user": a, "golden": golden, "pnl": r(pnl, 4), "days": r(days, 2),
                    "trips": t.stats.trips.saturating_sub(t.base.as_ref().map(|b| b.trips).unwrap_or(0))}));
            }
            if golden {
                n += 1;
                sum += pnl;
            }
        }
        drop(b);
        for ev in evs {
            self.write(ev);
        }
        self.refollow();
        (n, sum)
    }

    /// Our stop: closes our position in `coin` at the book and stays out of its position.
    fn stop_out(&mut self, user: &str, coin: &str) {
        let Some(book) = self.books.read().unwrap().get(coin).cloned() else { return };
        let fee_rate = self.cfg.taker_fee;
        let Some(t) = self.traders.get_mut(user).filter(|t| !t.acct.liquidated) else { return };
        let ours = t.acct.size(coin);
        let entry = t.acct.positions.get(coin).map(|p| p.entry).unwrap_or(0.0);
        let (got, px) = walk(if ours < 0.0 { &book.asks } else { &book.bids }, ours.abs());
        if got <= 0.0 {
            return;
        }
        let theirs = t.theirs.get(coin).copied().unwrap_or(0.0);
        t.legs.entry(coin.to_string()).or_insert(Leg { dir: theirs.signum(), peak: theirs.abs(), ..Default::default() }).out = true;
        let filled = -ours.signum() * got;
        let fee = got * px * fee_rate;
        let realized = book_fill(t, coin, filled, px, fee, 0.0, now(), false);
        let ev = json!({"kind": "fill", "why": "stop", "user": user, "coin": coin, "size": filled, "px": px,
            "notional": r(got * px, 4), "fee": r(fee, 6), "realized": r(realized, 6), "pos_after": t.acct.size(coin),
            "entry": r(entry, 8), "book_ms": book.time_ms});
        self.write(ev);
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
    fn golden_after_days_and_trips_at_a_profit() {
        let mut t = Trader { acct: Account::new(1000.0), base: Some(Base { at: 0.0, equity: 900.0, trips: 2 }), ..Default::default() };
        t.stats.trips = 7;
        let day = 86400.0;
        assert_eq!(measured(&t, 950.0, 3.0 * day), (50.0, true));
        // Too soon or too few trips: as it is (not golden, or golden if put on the list).
        assert!(!measured(&t, 950.0, 2.9 * day).1);
        t.stats.trips = 6;
        assert!(!measured(&t, 950.0, 3.0 * day).1);
        t.golden = true;
        assert!(measured(&t, 850.0, 3.0 * day).1);
        // Measured long enough: at a loss since, off; liquidated, off.
        t.stats.trips = 7;
        assert_eq!(measured(&t, 890.0, 3.0 * day), (-10.0, false));
        t.acct.liquidated = true;
        assert!(!measured(&t, 950.0, 3.0 * day).1);
        t.acct.liquidated = false;
        t.base = None;
        assert_eq!(measured(&t, 950.0, 3.0 * day), (0.0, true));
    }

    #[test]
    fn targets_follow_its_position_at_our_size() {
        let mut legs = HashMap::new();
        // It enters long 2: we hold our full 0.5; it adds to 4: we hold 0.5.
        assert_eq!(target(&mut legs, "ETH", 2.0, 0.0, true, 0.5), 0.5);
        assert_eq!(target(&mut legs, "ETH", 4.0, 0.5, true, 0.7), 0.5);
        // It reduces to 1 of its peak 4: we hold a quarter; back to 3: three quarters.
        assert_eq!(target(&mut legs, "ETH", 1.0, 0.5, false, 0.7), 0.125);
        assert_eq!(target(&mut legs, "ETH", 3.0, 0.125, true, 0.7), 0.375);
        // It flips short: a new entry at the full size of then.
        assert_eq!(target(&mut legs, "ETH", -1.0, 0.375, true, 0.7), -0.7);
        // Our stop: out until it is flat, then its next entry is followed.
        legs.get_mut("ETH").unwrap().out = true;
        assert_eq!(target(&mut legs, "ETH", -2.0, 0.0, true, 0.7), 0.0);
        assert_eq!(target(&mut legs, "ETH", 0.0, 0.0, false, 0.7), 0.0);
        assert!(legs.is_empty());
        assert_eq!(target(&mut legs, "ETH", 1.0, 0.0, true, 0.6), 0.6);
        // A position it held before we followed is not entered; a copy we hold is resized.
        assert_eq!(target(&mut legs, "BTC", 1.0, 0.0, false, 0.1), 0.0);
        assert_eq!(target(&mut legs, "BTC", 2.0, 0.0, true, 0.1), 0.0);
        assert_eq!(target(&mut legs, "SOL", -5.0, -3.0, false, 2.0), -2.0);
    }

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
