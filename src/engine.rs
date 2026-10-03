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
use crate::api::{AccountState, Api, CoinCtx, CoinInfo, Leader, Trigger, exchange_now, now};
use crate::config::Config;
use crate::log;
use crate::maker::{self, MakerTrade};
use crate::signals::{self, Ctx, Data, Flow, Inputs, Level, OpenSignal, Prices, Reading, SignalState, VARIANTS, Variant, WhaleFlow};
use crate::stats::{Plan, Stats, Trip};
use crate::store::{MakerRow, Row, SignalRow, Store, TradeRow};
use crate::ws::{Book, Books, UserFill, Watched, Whales};

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
    /// Signal accounts (`signal:<variant>`) only: their open signal trades.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<SignalState>,
    /// Its positions' stops and liquidation prices as last read (coin -> set-up).
    #[serde(default)]
    pub setups: HashMap<String, Setup>,
}

/// A trader's position set-up as read: its side, liquidation price, stop orders (trigger
/// price, size or None for the whole position).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Setup {
    pub side: f64,
    pub liq_px: Option<f64>,
    pub stops: Vec<(f64, Option<f64>)>,
    pub at: f64,
}

/// A set-up older than this is not used for the clusters.
const SETUP_MAX_AGE_S: f64 = 6.0 * 3600.0;

/// Signal accounts are kept under this prefix, next to the copy accounts.
pub const SIGNAL_PREFIX: &str = "signal:";

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
    Funding(HashMap<String, CoinCtx>),
    /// Funding, open interest and volume per coin, every few minutes.
    Ctx(HashMap<String, CoinCtx>),
    /// A coin's recent one-minute closes, read at a start (price signals need not wait hours).
    Seed { coin: String, closes: Vec<(u64, f64)> },
    /// A trade in a watched coin (`taker_buy`: its aggressor bought).
    Print { coin: String, px: f64, sz: f64, taker_buy: bool, time_ms: u64 },
    /// A watched coin's book changed.
    Book(String),
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
    /// Coins with open signal trades or limit-order twins: their trades and book changes come
    /// in as they happen (stops, take profits, limit fills).
    pub watched: Watched,
    /// Everyone's taker flow (the trades websocket adds to it).
    pub whales: Whales,
    /// Mids every minute, and the exchange's per-coin state, for the price and crowding signals.
    prices: Prices,
    ctx: Ctx,
    leaders: HashMap<String, Leader>,
    traders: HashMap<String, Trader>,
    /// One account per signal variant (`signal:<name>`).
    signals: HashMap<String, Trader>,
    /// The followed traders' recent fills, for the signals.
    flow: Flow,
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
    /// Time spent per tick since the last status line: ticks, total and longest (ms), and
    /// of it the signals.
    load: Load,
}

#[derive(Default)]
struct Load {
    ticks: u64,
    tick_ms: f64,
    tick_max_ms: f64,
    signals_ms: f64,
    signals_max_ms: f64,
    /// Trades and book changes of the watched coins, and the time spent on them (ms).
    market_msgs: u64,
    market_ms: f64,
    market_max_ms: f64,
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
        let (sigs, traders): (HashMap<String, Trader>, HashMap<String, Trader>) =
            traders.into_iter().partition(|(a, _)| a.starts_with(SIGNAL_PREFIX));
        let mut signals = HashMap::new();
        for v in VARIANTS {
            let key = format!("{SIGNAL_PREFIX}{}", v.name);
            let t = sigs.get(&key).cloned().unwrap_or_else(|| Trader {
                address: key.clone(),
                name: Some(describe(v)),
                enrolled_at: now(),
                acct: Account::new(cfg.start_usd),
                ..Default::default()
            });
            signals.insert(key, Trader { signal: Some(t.signal.clone().unwrap_or_default()), ..t });
        }
        log!("state: {} copy accounts, {} signal accounts", traders.len(), signals.len());
        let resync = traders.iter().filter(|(_, t)| !t.acct.liquidated && (!t.acct.positions.is_empty() || !t.theirs.is_empty()))
            .map(|(a, _)| a.clone()).collect();
        Ok(Self {
            api,
            books,
            coins: coins.into_iter().map(|c| (c.name.clone(), c)).collect(),
            followed: Arc::new(RwLock::new(HashSet::new())),
            watched: Arc::new(RwLock::new(HashSet::new())),
            whales: Arc::new(std::sync::Mutex::new(WhaleFlow::new((exchange_now() * 1000.0) as u64))),
            prices: Prices::new((exchange_now() * 1000.0) as u64),
            ctx: Ctx::default(),
            leaders: HashMap::new(),
            traders,
            signals,
            flow: Flow::new((exchange_now() * 1000.0) as u64),
            reading: HashSet::new(),
            planning: HashSet::new(),
            resync,
            scheduled: HashMap::new(),
            tx,
            store,
            read_limit: Arc::new(Semaphore::new(4)),
            dirty: false,
            load: Load::default(),
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
        let sig_rows = self.signals.iter().filter_map(|(a, t)| {
            let equity = t.acct.equity(&marks);
            Some(SignalRow {
                variant: a.trim_start_matches(SIGNAL_PREFIX).to_string(),
                rule: t.name.clone().unwrap_or_default(),
                equity,
                roi_pct: if t.acct.start > 0.0 { (equity / t.acct.start - 1.0) * 100.0 } else { 0.0 },
                taken: t.signal.as_ref().map(|s| s.taken).unwrap_or(0) as i64,
                closed: t.stats.trips as i64,
                wins: t.stats.wins as i64,
                open_positions: t.acct.positions.len() as i32,
                state: serde_json::to_string(t).ok()?,
            })
        }).collect();
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
        self.store.save_signals(sig_rows);
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
                Msg::Seed { coin, closes } => self.prices.seed(&coin, &closes),
                Msg::Ctx(ctx) => {
                    let rows = ctx.iter().map(|(c, x)| (c, x.funding, x.day_volume, x.oi_usd));
                    self.ctx.update((exchange_now() * 1000.0) as u64, rows);
                }
                Msg::Print { coin, px, sz, taker_buy, time_ms } => self.timed(|e| e.on_print(&coin, px, sz, taker_buy, time_ms)),
                Msg::Book(coin) => self.timed(|e| e.on_book(&coin)),
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
        let equity = self.traders.get(&f.user).map(|t| t.equity).filter(|&e| e > 0.0)
            .or_else(|| self.leaders.get(&f.user).map(|l| l.account_value)).unwrap_or(0.0);
        self.flow.push(&f.coin, f.time_ms, &f.user, f.delta * f.px, equity);
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
        // Its stops and liquidation prices, every position (for the clusters).
        t.setups = state.positions.iter().map(|(coin, &size)| {
            let stops = triggers.iter().filter(|o| &o.coin == coin && o.stop && o.sell == (size > 0.0)).map(|o| (o.trigger_px, o.size)).collect();
            let liq_px = state.setups.get(coin).and_then(|s| s.liq_px);
            (coin.clone(), Setup { side: size.signum(), liq_px, stops, at: t.plan_read })
        }).collect();
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

    fn on_funding(&mut self, ctx: HashMap<String, CoinCtx>) {
        let mut total = 0.0;
        for t in self.traders.values_mut().chain(self.signals.values_mut()) {
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
        log!("funding: ${total:.2} across copy and signal accounts");
        let live: Vec<&Trader> = self.traders.values().filter(|t| !t.acct.liquidated).collect();
        log!(
            "status: {} copy accounts ({} liquidated), {} followed, {} positions open; {} copy fills in all",
            self.traders.len(),
            self.traders.len() - live.len(),
            self.followed.read().unwrap().len(),
            live.iter().map(|t| t.acct.positions.len()).sum::<usize>(),
            self.traders.values().map(|t| t.copy_fills).sum::<u64>(),
        );
        let b = self.books.read().unwrap();
        let marks = |c: &str| b.get(c).and_then(Book::mid);
        let line: Vec<String> = VARIANTS.iter().filter_map(|v| {
            let t = self.signals.get(&format!("{SIGNAL_PREFIX}{}", v.name))?;
            let s = t.signal.as_ref()?;
            Some(format!("{} ${:.0} ({} taken, {} open)", v.name, t.acct.equity(&marks), s.taken, s.open.len()))
        }).collect();
        log!("signals: {}", line.join(", "));

    }

    /// Liquidations and reconciliation of active accounts.
    fn on_tick(&mut self) {
        let started = std::time::Instant::now();
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
        for t in self.signals.values_mut() {
            let equity = t.acct.equity(&marks);
            mark(t, &marks, equity);
        }
        let now_ms = (exchange_now() * 1000.0) as u64;
        for (c, book) in b.iter() {
            if let Some(m) = book.mid() {
                self.prices.sample(c, now_ms, m);
            }
        }
        drop(b);
        for a in liquidate {
            self.liquidate(&a);
        }
        let signals_at = std::time::Instant::now();
        self.run_signals();
        let (tick, sig) = (started.elapsed().as_secs_f64() * 1000.0, signals_at.elapsed().as_secs_f64() * 1000.0);
        self.load.ticks += 1;
        self.load.tick_ms += tick;
        self.load.tick_max_ms = self.load.tick_max_ms.max(tick);
        self.load.signals_ms += sig;
        self.load.signals_max_ms = self.load.signals_max_ms.max(sig);
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
        let l = std::mem::take(&mut self.load);
        let n = l.ticks.max(1) as f64;
        let (weight, backlog) = self.api.usage();
        let live: Vec<&Trader> = self.traders.values().filter(|t| !t.acct.liquidated).collect();
        let st = json!({
            "at": r(now(), 3),
            "copy_accounts": self.traders.len(),
            "liquidated": self.traders.len() - live.len(),
            "followed": self.followed.read().unwrap().len(),
            "open_positions": live.iter().map(|t| t.acct.positions.len()).sum::<usize>(),
            "copy_fills": self.traders.values().map(|t| t.copy_fills).sum::<u64>(),
            "api_weight": weight,
            "api_backlog_s": r(backlog, 1),
            "ticks": l.ticks,
            "tick_avg_ms": r(l.tick_ms / n, 2),
            "tick_max_ms": r(l.tick_max_ms, 2),
            "signals_avg_ms": r(l.signals_ms / n, 2),
            "signals_max_ms": r(l.signals_max_ms, 2),
            "market_msgs": l.market_msgs,
            "market_avg_ms": r(l.market_ms / (l.market_msgs.max(1) as f64), 3),
            "market_max_ms": r(l.market_max_ms, 2),
            "watched_coins": self.watched.read().unwrap().len(),
            "whale_wallets": self.whales.lock().unwrap().wallets(),
            "ctx_coins": self.ctx.volume.len(),
            "maker_trades_open": self.signals.values().filter_map(|t| t.signal.as_ref()).map(|s| s.makers.len()).sum::<usize>(),
            "flow_fills": self.flow.len(),
            "signal_trades_open": self.signals.values().filter_map(|t| t.signal.as_ref()).map(|s| s.open.len()).sum::<usize>(),
            "signal_trades_taken": self.signals.values().filter_map(|t| t.signal.as_ref()).map(|s| s.taken).sum::<u64>(),
        });
        log!("status: {st}");
        self.store.status(st);
    }

    /// Runs a handler of the watched coins' trades / book changes, timing it.
    fn timed(&mut self, f: impl FnOnce(&mut Self)) {
        let started = std::time::Instant::now();
        f(self);
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        self.load.market_msgs += 1;
        self.load.market_ms += ms;
        self.load.market_max_ms = self.load.market_max_ms.max(ms);
    }

    /// A trade in a watched coin: the limit orders of the twins it fills.
    fn on_print(&mut self, coin: &str, px: f64, sz: f64, taker_buy: bool, time_ms: u64) {
        let books = self.books.clone();
        let b = books.read().unwrap();
        let book = b.get(coin);
        let at = now();
        let mut rows = Vec::new();
        for (key, t) in self.signals.iter_mut() {
            let Some(s) = t.signal.as_mut() else { continue };
            for m in s.makers.iter_mut().filter(|m| m.coin == coin) {
                if m.on_print(taker_buy, px, sz, time_ms, book, at) {
                    rows.push(maker_row(key.trim_start_matches(SIGNAL_PREFIX), m));
                }
            }
            s.makers.retain(|m| m.closed_at.is_none());
        }
        drop(b);
        if !rows.is_empty() {
            self.dirty = true;
        }
        for row in rows {
            self.store.maker(row);
        }
    }

    /// A watched coin's book changed: market trades' stops and take profits at the mid, as
    /// soon as the book shows them; the twins' fills from the book, orders following the
    /// price, entry deadlines and stops.
    fn on_book(&mut self, coin: &str) {
        let books = self.books.clone();
        let b = books.read().unwrap();
        let Some(book) = b.get(coin) else { return };
        let Some(mid) = book.mid() else { return };
        let at = now();
        let mut closes: Vec<(String, &'static str)> = Vec::new();
        for (key, t) in self.signals.iter_mut() {
            let Some(os) = t.signal.as_mut().and_then(|s| s.open.get_mut(coin)) else { continue };
            let stop = os.stop;
            os.follow(mid);
            if os.stop != stop {
                self.dirty = true;
            }
            if let Some(why) = stop_or_tp(os, mid) {
                closes.push((key.clone(), why));
            }
        }
        let mut rows = Vec::new();
        for (key, t) in self.signals.iter_mut() {
            let Some(s) = t.signal.as_mut() else { continue };
            for m in s.makers.iter_mut().filter(|m| m.coin == coin) {
                if m.on_book(book, at) {
                    rows.push(maker_row(key.trim_start_matches(SIGNAL_PREFIX), m));
                }
            }
            s.makers.retain(|m| m.closed_at.is_none());
        }
        drop(b);
        if !rows.is_empty() {
            self.dirty = true;
        }
        for row in rows {
            self.store.maker(row);
        }
        for (key, why) in closes {
            self.close_signal(&key, coin, why, None);
        }
    }

    /// The coins to watch: open signal trades and twins.
    fn update_watched(&self) {
        let mut set = HashSet::new();
        for s in self.signals.values().filter_map(|t| t.signal.as_ref()) {
            set.extend(s.open.keys().cloned());
            set.extend(s.makers.iter().map(|m| m.coin.clone()));
        }
        *self.watched.write().unwrap() = set;
    }

    /// Every variant reads the coins with recent flow (and the held ones): open trades are
    /// closed at their stop, take profit, expiry or when the traders turn the other way; new
    /// ones are opened where a variant fires. The twins are closed at expiry or when the
    /// traders turn, and checked against the book (in case its changes did not come in).
    fn run_signals(&mut self) {
        let now_ms = (exchange_now() * 1000.0) as u64;
        self.flow.prune(now_ms);
        let at = now();
        let books = self.books.clone();
        let b = books.read().unwrap();
        let marks = |c: &str| b.get(c).and_then(Book::mid);
        // Trailing / break-even stops follow the price.
        for t in self.signals.values_mut() {
            for (c, os) in t.signal.iter_mut().flat_map(|s| s.open.iter_mut()) {
                if let Some(m) = marks(c) {
                    os.follow(m);
                }
            }
        }
        // The traders whose copies we run at a profit, those with low leverage settings and a
        // large account, every copied trader's positions as a share of its equity, and their
        // stops and liquidation prices.
        let best: HashSet<String> = self.traders.iter()
            .filter(|(_, t)| !t.acct.liquidated && t.copy_fills >= 3 && t.acct.equity(&marks) > t.acct.start)
            .map(|(a, _)| a.clone()).collect();
        let low_lev: HashSet<String> = self.traders.iter().filter(|(_, t)| is_low_lev(t)).map(|(a, _)| a.clone()).collect();
        let levels = trigger_levels(&self.traders, at);
        let mut positions: HashMap<String, Vec<(String, f64)>> = HashMap::new();
        for (a, t) in &self.traders {
            if t.acct.liquidated || t.equity <= 0.0 {
                continue;
            }
            for (c, s) in &t.theirs {
                if let Some(m) = marks(c) {
                    positions.entry(c.clone()).or_default().push((a.clone(), s * m / t.equity));
                }
            }
        }
        let whales = self.whales.clone();
        let w = whales.lock().unwrap();
        let liquid = self.ctx.liquid(signals::TREND_COINS);
        let mut coins: Vec<String> = self.flow.coins().chain(positions.keys()).chain(levels.keys()).chain(liquid.iter()).chain(w.coins())
            .cloned().collect::<HashSet<_>>().into_iter().collect();
        coins.sort();
        let inputs = Inputs::new(Data {
            flow: &self.flow, positions: &positions, best: &best, low_lev: &low_lev, whales: &w, prices: &self.prices, ctx: &self.ctx,
            levels: &levels, now_ms,
        });
        let mut closes: Vec<(String, String, &'static str, Reading)> = Vec::new();
        let mut opens: Vec<(&'static Variant, String, Reading)> = Vec::new();
        let mut twin_closes: Vec<(String, String, &'static str)> = Vec::new();
        for v in VARIANTS {
            let key = format!("{SIGNAL_PREFIX}{}", v.name);
            let Some(state) = self.signals.get(&key).and_then(|t| t.signal.as_ref()) else { continue };
            for m in &state.makers {
                let why = if at >= m.expires {
                    "expiry"
                } else if inputs.read(v, &m.coin).side == -m.side {
                    "traders turned"
                } else {
                    continue;
                };
                twin_closes.push((key.clone(), m.id.clone(), why));
            }
            for (coin, os) in &state.open {
                let Some(mid) = marks(coin) else { continue };
                let rd = inputs.read(v, coin);
                let why = if let Some(why) = stop_or_tp(os, mid) {
                    why
                } else if at >= os.expires {
                    "expiry"
                } else if rd.side == -os.side {
                    "traders turned"
                } else {
                    continue;
                };
                closes.push((key.clone(), coin.clone(), why, rd));
            }
            let mut open = state.open.len();
            for coin in &coins {
                if open >= signals::MAX_OPEN {
                    break;
                }
                if state.open.contains_key(coin) || state.closed_at.get(coin).is_some_and(|&t| at - t < v.cooldown_s) || marks(coin).is_none() {
                    continue;
                }
                let rd = inputs.read(v, coin);
                if rd.side != 0.0 {
                    opens.push((v, coin.clone(), rd));
                    open += 1;
                }
            }
        }
        drop(inputs);
        drop(w);
        // The twins: expiry and the traders turning at the book; the rest as on a book change.
        let mut rows = Vec::new();
        for (key, t) in self.signals.iter_mut() {
            let Some(s) = t.signal.as_mut() else { continue };
            let variant = key.trim_start_matches(SIGNAL_PREFIX);
            for m in s.makers.iter_mut() {
                let Some(book) = b.get(&m.coin) else { continue };
                let why = twin_closes.iter().find(|(k, id, _)| k == key && id == &m.id).map(|x| x.2);
                if why.is_some_and(|why| m.close_at_book(book, at, why)) || m.on_book(book, at) {
                    rows.push(maker_row(variant, m));
                }
            }
            s.makers.retain(|m| m.closed_at.is_none());
        }
        drop(b);
        if !rows.is_empty() {
            self.dirty = true;
        }
        for row in rows {
            self.store.maker(row);
        }
        for (key, coin, why, rd) in closes {
            self.close_signal(&key, &coin, why, Some(&rd));
        }
        for (v, coin, rd) in opens {
            self.open_signal(v, &coin, &rd);
        }
        self.update_watched();
    }

    /// Opens `v`'s trade in `coin`: a taker order sized to risk `RISK_PCT` of the account at the
    /// variant's stop, stop and take profit set from the fill.
    fn open_signal(&mut self, v: &Variant, coin: &str, rd: &Reading) {
        let key = format!("{SIGNAL_PREFIX}{}", v.name);
        let Some(info) = self.coins.get(coin).cloned() else { return };
        let Some(book) = self.books.read().unwrap().get(coin).cloned() else { return };
        let Some(mid) = book.mid() else { return };
        let (equity, gross) = {
            let b = self.books.read().unwrap();
            let marks = |c: &str| b.get(c).and_then(Book::mid);
            let Some(t) = self.signals.get(&key) else { return };
            let gross: f64 = t.acct.positions.iter().map(|(c, p)| p.size.abs() * marks(c).unwrap_or(p.entry)).sum();
            (t.acct.equity(&marks), gross)
        };
        if equity <= 0.0 {
            return;
        }
        let usd = signals::notional(equity, gross, v.exit.stop_pct);
        let size = round_size(usd / mid, info.sz_decimals);
        if size * mid < self.cfg.min_order_usd {
            return;
        }
        let side_levels = if rd.side > 0.0 { &book.asks } else { &book.bids };
        let (got, px) = walk(side_levels, size);
        if got <= 0.0 {
            return;
        }
        let best_px = side_levels[0].0;
        let fee = got * px * self.cfg.taker_fee;
        let at = now();
        let Some(t) = self.signals.get_mut(&key) else { return };
        book_fill(t, coin, got * rd.side, px, fee, equity, at, true);
        // Price vs the mid when the signal fired: the spread and the book walked.
        let (mv, impact) = (got * (best_px - mid) * rd.side, got * (px - best_px) * rd.side);
        t.stats.copy(0.0, 0.0, got * px, mv + impact, mv, impact);
        let stop = px * (1.0 - rd.side * v.exit.stop_pct / 100.0);
        let os = OpenSignal {
            id: format!("{}-{coin}-{}", v.name, (at * 1000.0) as u64),
            side: rd.side,
            entry: px,
            stop,
            tp: px * (1.0 + rd.side * v.exit.tp_pct / 100.0),
            opened: at,
            expires: at + v.hold_s,
            mid,
            size: got,
            risk_usd: got * (px - stop).abs(),
            risk_pct: got * (px - stop).abs() / equity * 100.0,
            fee,
            reason: json!({"longs": rd.longs, "shorts": rd.shorts, "agree": r(rd.agree, 3), "score_pct": r(rd.score, 2),
                "buy_usd": r(rd.buy_usd, 0), "sell_usd": r(rd.sell_usd, 0), "window_s": v.window_s, "book_ms": book.time_ms}),
            stop_pct: v.exit.stop_pct,
            trail_pct: v.exit.trail_pct,
            be_r: v.exit.be_r,
            peak: 0.0,
        };
        let state = t.signal.get_or_insert_with(Default::default);
        state.taken += 1;
        state.open.insert(coin.to_string(), os.clone());
        // Its limit-order twins: the same size, entering with a post-only order instead.
        let mut rows = Vec::new();
        for mode in maker::MODES {
            let mut m = MakerTrade {
                id: format!("{}:{}", os.id, mode.name()),
                parent: os.id.clone(),
                mode: Some(mode),
                coin: coin.to_string(),
                side: rd.side,
                size: got,
                placed: at,
                mid,
                market_entry: px,
                stop_pct: v.exit.stop_pct,
                tp_pct: v.exit.tp_pct,
                trail_pct: v.exit.trail_pct,
                be_r: v.exit.be_r,
                expires: os.expires,
                equity,
                sz_decimals: info.sz_decimals,
                reason: os.reason.clone(),
                ..Default::default()
            };
            if m.place(&book, at) {
                rows.push(maker_row(v.name, &m));
                state.makers.push(m);
            }
        }
        self.dirty = true;
        self.store.trade(ticket(v.name, coin, &os, None));
        for row in rows {
            self.store.maker(row);
        }
    }

    /// Closes `key`'s trade in `coin` at the book; `rd`: the traders then, when read.
    fn close_signal(&mut self, key: &str, coin: &str, why: &'static str, rd: Option<&Reading>) {
        let Some(book) = self.books.read().unwrap().get(coin).cloned() else { return };
        let fee_rate = self.cfg.taker_fee;
        let at = now();
        let Some(t) = self.signals.get_mut(key) else { return };
        let ours = t.acct.size(coin);
        let Some(os) = t.signal.as_ref().and_then(|s| s.open.get(coin)).cloned() else { return };
        if ours != 0.0 {
            let (got, px) = walk(if ours > 0.0 { &book.bids } else { &book.asks }, ours.abs());
            if got <= 0.0 {
                return;
            }
            let fee = got * px * fee_rate;
            let pnl = t.trips.get(coin).map(|x| x.pnl).unwrap_or(0.0) + (px - os.entry) * got * os.side - fee;
            let opened_equity = t.trips.get(coin).map(|x| x.equity).unwrap_or(t.acct.start);
            book_fill(t, coin, -ours.signum() * got, px, fee, 0.0, at, false);
            if t.acct.size(coin) != 0.0 {
                // Not all of it filled (thin book): the rest goes at the next tick.
                if let Some(o) = t.signal.as_mut().and_then(|s| s.open.get_mut(coin)) {
                    o.fee += fee;
                }
                return;
            }
            let variant = key.trim_start_matches(SIGNAL_PREFIX).to_string();
            let mut row = ticket(&variant, coin, &os, Some((at, px, why)));
            row.fees = os.fee + fee;
            row.pnl = Some(r(pnl, 4));
            row.pnl_pct = Some(r(pnl / opened_equity * 100.0, 3));
            if let (serde_json::Value::Object(m), Some(rd)) = (&mut row.reason, rd) {
                m.insert("traders_at_close".into(), json!({"longs": rd.longs, "shorts": rd.shorts, "agree": r(rd.agree, 3)}));
            }
            self.store.trade(row);
        }
        let Some(t) = self.signals.get_mut(key) else { return };
        if let Some(s) = t.signal.as_mut() {
            s.open.remove(coin);
            s.closed_at.insert(coin.to_string(), at);
        }
        self.dirty = true;
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

    #[test]
    fn stop_and_liquidation_levels() {
        let setup = |side: f64, liq: Option<f64>, stops: Vec<(f64, Option<f64>)>, at: f64| Setup { side, liq_px: liq, stops, at };
        let mut long = Trader { acct: Account::new(1000.0), ..Default::default() };
        long.theirs.insert("ETH".into(), 10.0);
        long.theirs.insert("SOL".into(), -5.0);
        // ETH long 10: a stop for 4 at 1900, a position stop at 1850 (capped at the position), liq 1500.
        long.setups.insert("ETH".into(), setup(1.0, Some(1500.0), vec![(1900.0, Some(4.0)), (1850.0, None)], 1000.0));
        // SOL: read when it was long, now short: stale, not used.
        long.setups.insert("SOL".into(), setup(1.0, Some(90.0), vec![], 1000.0));
        let mut old = Trader { acct: Account::new(1000.0), ..Default::default() };
        old.theirs.insert("ETH".into(), -1.0);
        old.setups.insert("ETH".into(), setup(-1.0, Some(2500.0), vec![], 1000.0 - SETUP_MAX_AGE_S - 1.0));
        let traders: HashMap<String, Trader> = [("a".to_string(), long), ("b".to_string(), old)].into_iter().collect();
        let lv = trigger_levels(&traders, 1000.0);
        assert!(!lv.contains_key("SOL"));
        let eth = &lv["ETH"];
        assert_eq!(eth.len(), 3);
        let usd: Vec<(f64, f64, f64)> = eth.iter().map(|l| (l.px, l.usd, l.dir)).collect();
        assert_eq!(usd, vec![(1900.0, 7600.0, -1.0), (1850.0, 18_500.0, -1.0), (1500.0, 15_000.0, -1.0)]);
    }

    #[test]
    fn low_leverage_large_accounts() {
        let mut t = Trader { acct: Account::new(1000.0), equity: 50_000.0, ..Default::default() };
        assert!(!is_low_lev(&t)); // nothing read yet
        t.stats.planned = 3;
        t.stats.lev_max = 5.0;
        assert!(is_low_lev(&t));
        // An open trip read at 20x counts too.
        let mut trip = Trip::new(0.0, 1000.0);
        trip.plan(Plan { leverage: 20.0, ..Default::default() });
        t.trips.insert("BTC".into(), trip);
        assert!(!is_low_lev(&t));
        t.trips.clear();
        t.equity = 20_000.0;
        assert!(!is_low_lev(&t));
    }
}

/// One line on what a variant does, for its account's name.
fn describe(v: &Variant) -> String {
    let w = v.window_s / 60.0;
    let what = match v.kind {
        signals::Kind::Heads => format!("{}+ traders, {:.0}%+ of them one way over {w:.0} min", v.min_traders, v.min_agree * 100.0),
        signals::Kind::Conviction => format!("{}+ traders, net {}% of equity one way over {w:.0} min", v.min_traders, v.min_score),
        signals::Kind::Volume => format!("${:.0}k+ net, {:.0}%+ of it one way, {}+ traders, over {w:.0} min", v.min_usd / 1000.0,
            v.min_agree * 100.0, v.min_traders),
        signals::Kind::Positioning => {
            let mut s = format!("{}+ traders holding, {:.0}%+ one way", v.min_traders, v.min_agree * 100.0);
            if v.min_funding > 0.0 {
                s += &format!(", funding {:.5}%/h+ their way", v.min_funding * 100.0);
            }
            if v.min_oi_pct > 0.0 {
                s += &format!(", open interest +{}%+ over 4 h", v.min_oi_pct);
            }
            s
        }
        signals::Kind::Cluster => format!("${:.0}k+ of the traders' stops / liquidations within {}% on one side, {:.0}%+ of those in the band",
            v.min_usd / 1000.0, v.band_pct, v.min_agree * 100.0),
        signals::Kind::Whales => format!("{}+ wallets with ${:.0}k+ net taker flow, {:.0}%+ of the whales' dollars one way, over {w:.0} min",
            v.min_traders, v.min_usd / 1000.0, v.min_agree * 100.0),
        signals::Kind::Trend => format!("moved {}%+ over {w:.0} min (top {} coins by volume)", v.move_pct, signals::TREND_COINS),
        signals::Kind::CrossMomentum => format!("{} strongest long / weakest short vs BTC over {w:.0} min (top {} coins by volume)",
            v.top_k, signals::XMOM_COINS),
    };
    let who = match v.who {
        signals::Who::All => "",
        signals::Who::Best => "profitable copies only: ",
        signals::Who::LowLev => "low-leverage large accounts only: ",
    };
    let mut exit = format!("stop {}%", v.exit.stop_pct);
    if v.exit.trail_pct > 0.0 {
        exit += &format!(", trailing {}%", v.exit.trail_pct);
    } else {
        exit += &format!(", tp {}%", v.exit.tp_pct);
    }
    if v.exit.be_r > 0.0 {
        exit += &format!(", to break even at {}R", v.exit.be_r);
    }
    format!("{}{who}{what}; {exit}, {:.0} min max", if v.fade { "against: " } else { "" }, v.hold_s / 60.0)
}

/// Low leverage and a large account: every leverage setting read (closed trips and open ones)
/// at most `LOW_LEV_MAX`, at least one read, equity at least `LOW_LEV_MIN_EQUITY`.
fn is_low_lev(t: &Trader) -> bool {
    let open = t.trips.values().filter_map(|x| x.plan.as_ref());
    let max = open.clone().map(|p| p.leverage).fold(t.stats.lev_max, f64::max);
    let read = t.stats.planned > 0 || open.count() > 0;
    !t.acct.liquidated && read && max > 0.0 && max <= signals::LOW_LEV_MAX && t.equity >= signals::LOW_LEV_MIN_EQUITY
}

/// The copied traders' stops and liquidation prices per coin, from set-ups read within
/// `SETUP_MAX_AGE_S` of positions they still hold that way.
fn trigger_levels(traders: &HashMap<String, Trader>, at: f64) -> HashMap<String, Vec<Level>> {
    let mut out: HashMap<String, Vec<Level>> = HashMap::new();
    for (a, t) in traders {
        if t.acct.liquidated {
            continue;
        }
        for (coin, s) in &t.setups {
            let pos = t.theirs.get(coin).copied().unwrap_or(0.0);
            if at - s.at > SETUP_MAX_AGE_S || pos == 0.0 || pos.signum() != s.side {
                continue;
            }
            let (abs, dir) = (pos.abs(), -s.side);
            let lv = out.entry(coin.clone()).or_default();
            for &(px, size) in &s.stops {
                lv.push(Level { px, usd: size.unwrap_or(abs).min(abs) * px, dir, who: a.clone() });
            }
            if let Some(px) = s.liq_px {
                lv.push(Level { px, usd: abs * px, dir, who: a.clone() });
            }
        }
    }
    out
}

/// A market signal trade's stop or take profit, if the mid has reached it.
fn stop_or_tp(os: &OpenSignal, mid: f64) -> Option<&'static str> {
    if (mid - os.stop) * os.side <= 0.0 {
        Some("stop")
    } else if (mid - os.tp) * os.side >= 0.0 {
        Some("take profit")
    } else {
        None
    }
}

/// A limit-order twin's row.
fn maker_row(variant: &str, m: &MakerTrade) -> MakerRow {
    let entered = m.entered_at.is_some() && m.filled > 0.0;
    let closed = m.closed_at.is_some();
    MakerRow {
        id: m.id.clone(),
        parent_id: m.parent.clone(),
        variant: variant.to_string(),
        mode: m.mode.map(|x| x.name()).unwrap_or("").to_string(),
        coin: m.coin.clone(),
        side: if m.side > 0.0 { "long" } else { "short" }.to_string(),
        placed_at: r(m.placed, 3),
        mid: m.mid,
        market_entry: m.market_entry,
        size: m.size,
        filled: m.filled,
        maker_pct: if m.filled > 0.0 { r(m.maker_filled / m.filled * 100.0, 2) } else { 0.0 },
        entry: (m.filled > 0.0).then(|| m.entry()),
        wait_s: m.entered_at.map(|e| r(e - m.placed, 3)),
        requotes: m.requotes as i64,
        stop: entered.then(|| m.stop()),
        take_profit: entered.then(|| m.tp()),
        expires_at: r(m.expires, 3),
        reason: m.reason.clone(),
        closed_at: m.closed_at.map(|c| r(c, 3)),
        exit_px: (m.exited > 0.0).then(|| m.exit_value / m.exited),
        exit_reason: m.exit_reason.clone(),
        exit_maker_pct: (m.exited > 0.0).then(|| r(m.exit_maker / m.exited * 100.0, 2)),
        pnl: closed.then(|| r(m.pnl(), 4)),
        pnl_pct: closed.then(|| r(m.pnl() / m.equity.max(1e-9) * 100.0, 3)),
        entry_fees: r(m.entry_fee, 6),
        exit_fees: r(m.exit_fee, 6),
        fees: r(m.entry_fee + m.exit_fee, 6),
    }
}

/// A signal trade's row: its ticket, and how it closed once it has.
fn ticket(variant: &str, coin: &str, os: &OpenSignal, closed: Option<(f64, f64, &str)>) -> TradeRow {
    TradeRow {
        id: os.id.clone(),
        variant: variant.to_string(),
        coin: coin.to_string(),
        side: if os.side > 0.0 { "long" } else { "short" }.to_string(),
        opened_at: r(os.opened, 3),
        mid: os.mid,
        entry: os.entry,
        stop: os.stop,
        take_profit: os.tp,
        size: os.size,
        notional: r(os.size * os.entry, 4),
        risk_usd: r(os.risk_usd, 4),
        risk_pct: r(os.risk_pct, 4),
        expires_at: r(os.expires, 3),
        reason: os.reason.clone(),
        closed_at: closed.map(|c| r(c.0, 3)),
        exit_px: closed.map(|c| c.1),
        exit_reason: closed.map(|c| c.2.to_string()),
        pnl: None,
        pnl_pct: None,
        fees: r(os.fee, 6),
    }
}
