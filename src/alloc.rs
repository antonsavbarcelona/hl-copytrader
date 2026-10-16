//! A second layer over the paper copies, apart from them: allocator portfolios that get the same
//! signals (each followed account's position moves, `Signal`) but decide whom to let in, each
//! with its own paper account and portfolio limits. The copies per account go on as they were.
//!
//! Each portfolio is a test of its own (`modes`), sharing nothing with the others but the
//! signals. They are a grid, every combination run (paper costs nothing): who it takes (a score
//! to rank the eligible traders by and how many of its top, or as many at random: the
//! benchmark, ten draws) and its sizing (the risk an entry gets, the cap on the open risk in
//! one direction, and whether it follows the trader's adds). A score is judged against the ten
//! draws of the same size and sizing: the spread of chance, not one draw.
//!
//!   - Each trader's statistics (`TraderStats`): the backtest's over 90 days (`backtest_stats`),
//!     the source of truth. The paper copies only calibrate the backtester: a copy's own take
//!     the backtest's place only after `PAPER_DAYS` copied without a break (a day of paper
//!     is not 90 days of history: it put scalpers the backtest had at a loss on top). A trader
//!     with neither is not ranked.
//!   - Eligible for all: PnL above 0 (and, from paper, `MIN_TRADES` closed trips). Beyond
//!     that a portfolio's filter is its score's own (`Score::min_trades`): a score over the
//!     trips' risk (their spread, the losses, the drawdown), their number or their time needs
//!     `RISK_TRADES` trips for it to be measured at all; a PnL sum needs none. Nothing the
//!     score itself weighs is filtered again.
//!   - Scores (`Score`): over the trips (expectancy / drawdown, PnL, expectancy x sqrt(trips),
//!     profit factor, t-statistic, the expectancy's lower bound), over time (daily Sharpe and
//!     Sortino, Calmar, PnL of the last 30 days, the share of weeks in profit) and COMBO, a
//!     trader's mean percentile over all of them. Re-ranked once a day (`RANK_EVERY_S`) and at
//!     every start (the picks are not saved: the backtest's statistics are fixed and the draws
//!     seeded by the day, so a start picks the same).
//!   - A picked trader's entry gets its portfolio's risk (`RISKS`: its stop `STOP` away loses
//!     that much of the portfolio's equity), less as the caps require: open risk in its
//!     direction (`CAPS_DIR`: the portfolios are mostly one way, the traders long together),
//!     in all `CAP_TOTAL`, per trader `CAP_TRADER`; at most `MAX_PER_COIN` positions in a coin;
//!     under an eighth of its risk it is refused. An entry cut or refused is a row of
//!     `allocator_decisions` (one granted in full is in the state, then `allocator_trades`);
//!     each position closed a row of `allocator_trades` (its own entry, exit, PnL net of its
//!     fees, and why).
//!   - A position is followed to its end (reduced in proportion as the trader reduces from its
//!     peak, closed when it is flat), or closed at the stop: a trader dropping off the list
//!     stops new entries only. With adds (`_ADD`): as the trader adds, so does it, up to
//!     `ADD_MAX` times its entry (as much risk), within the caps; the stop then 20% from its
//!     average entry; reduced from its size at the trader's peak.
//!   - The breaker (`_BRK`, a family of its own: the strongest scores and the draws at 10-30
//!     traders, `brk_modes`): after `BRK_LOSSES` losing positions of a trader in a row, no new
//!     entries of its for `BRK_PAUSE_S` (walked forward it lifted the scores' return and cut
//!     their drawdown, the draws' hardly: it works on a selection).
//!   - Each portfolio's status is a row of `allocator_status` every `STATUS_EVERY_S`; the state
//!     is saved every `SAVE_EVERY_S` and at shutdown.
//!
//! Fills are taker fills on mainnet's book, as the paper copies'.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::account::{Account, walk};
use crate::config::TAKER_FEE;
use crate::ws::Book;

pub const START_USD: f64 = 10_000.0;
pub const RANK_EVERY_S: f64 = 86_400.0;
const STOP: f64 = 0.20;
const CAP_TOTAL: f64 = 1.00;
const CAP_TRADER: f64 = 0.20;
const MAX_PER_COIN: usize = 2;
pub const MIN_TRADES: u64 = 20;
/// Days a paper copy runs without a break before its statistics take the backtest's place.
pub const PAPER_DAYS: f64 = 90.0;
/// Closed trips a score over their risk, number or time needs (`Score::min_trades`).
const RISK_TRADES: u64 = 20;
/// The portfolios' generation: a saved state of another is not restored (started again).
const GENERATION: u32 = 4;
/// The state is saved this often (and at shutdown), each portfolio's status written this often.
pub const SAVE_EVERY_S: f64 = 3600.0;
pub const STATUS_EVERY_S: f64 = 3600.0;
/// Following the trader's adds: up to this many times the entry's size (and risk).
const ADD_MAX: f64 = 2.0;
const MIN_ORDER_USD: f64 = 10.0;
/// The breaker: this many losing positions of a trader in a row pause its new entries this long.
const BRK_LOSSES: u32 = 5;
const BRK_PAUSE_S: f64 = 3.0 * 86_400.0;
/// COMBO3: the traders in this top of 2+ of LCB, EXPDD, PNLSQRTN.
const COMBO3_TOP: usize = 40;

/// The grid: scores and ten random draws, how many traders, the risk an entry gets, the cap on
/// the open risk in one direction (1.0: none but the total's), the trader's adds followed or not.
pub const SCORES: [Score; 13] = [Score::ExpDd, Score::Pnl, Score::PnlSqrtN, Score::Pf, Score::TStat, Score::Lcb, Score::Sharpe, Score::Sortino,
    Score::Calmar, Score::Pnl30, Score::Weeks, Score::Combo, Score::Combo3];
/// The breaker's family: these scores and the draws, at these sizes, risks and caps, no adds.
pub const BRK_SCORES: [Score; 7] = [Score::Lcb, Score::PnlSqrtN, Score::ExpDd, Score::Pnl, Score::Pnl30, Score::Combo, Score::Combo3];
pub const BRK_SIZES: [usize; 3] = [10, 20, 30];
pub const BRK_CAPS: [f64; 2] = [0.20, 0.50];
pub const RANDOMS: u64 = 10;
pub const SIZES: [usize; 5] = [10, 20, 30, 50, 100];
pub const RISKS: [f64; 3] = [0.005, 0.01, 0.02];
pub const CAPS_DIR: [f64; 3] = [0.20, 0.50, 1.00];
pub const ADDS: [bool; 2] = [false, true];

/// A portfolio's test: picked by `score` (None: at random, draw `draw`), `n` traders, `risk` an
/// entry, `cap_dir` in one direction, the trader's `adds` followed. Its name:
/// `EXPDD_20_R0.5_D20`, `RANDOM7_20_R0.5_D20_ADD`.
#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    pub score: Option<Score>,
    pub draw: u64,
    pub n: usize,
    pub risk: f64,
    pub cap_dir: f64,
    pub adds: bool,
    pub brk: bool,
}

impl Mode {
    pub fn name(&self) -> String {
        let who = match self.score {
            Some(s) => s.name().to_string(),
            None => format!("RANDOM{}", self.draw),
        };
        format!("{who}_{}_R{}_D{}{}{}", self.n, (self.risk * 1000.0).round() / 10.0, (self.cap_dir * 100.0).round(),
            if self.adds { "_ADD" } else { "" }, if self.brk { "_BRK" } else { "" })
    }
}

pub fn modes() -> Vec<Mode> {
    let mut out = Vec::new();
    let who: Vec<(Option<Score>, u64)> = SCORES.iter().map(|s| (Some(*s), 0)).chain((1..=RANDOMS).map(|d| (None, d))).collect();
    for (score, draw) in who {
        for n in SIZES {
            for risk in RISKS {
                for cap_dir in CAPS_DIR {
                    for adds in ADDS {
                        out.push(Mode { score, draw, n, risk, cap_dir, adds, brk: false });
                    }
                }
            }
        }
    }
    let who: Vec<(Option<Score>, u64)> = BRK_SCORES.iter().map(|s| (Some(*s), 0)).chain((1..=RANDOMS).map(|d| (None, d))).collect();
    for (score, draw) in who {
        for n in BRK_SIZES {
            for risk in RISKS {
                for cap_dir in BRK_CAPS {
                    out.push(Mode { score, draw, n, risk, cap_dir, adds: false, brk: true });
                }
            }
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Score {
    ExpDd,
    Pnl,
    TStat,
    Pf,
    PnlSqrtN,
    /// The expectancy less two standard errors: what a trip earns, counted with a margin.
    Lcb,
    /// The copy's daily return over its spread (a year's), over its downside only.
    Sharpe,
    Sortino,
    /// Its return a month over its max drawdown.
    Calmar,
    /// PnL of the trips closed in its last 30 days.
    Pnl30,
    /// The share of its weeks with a closed trip that were in profit.
    Weeks,
    /// Its mean percentile over all the other scores.
    Combo,
    /// In the top of 2+ of LCB, EXPDD, PNLSQRTN (`COMBO3_TOP`), by its mean place in them.
    Combo3,
}

impl Score {
    pub fn name(self) -> &'static str {
        match self {
            Score::ExpDd => "EXPDD",
            Score::Pnl => "PNL",
            Score::TStat => "TSTAT",
            Score::Pf => "PF",
            Score::PnlSqrtN => "PNLSQRTN",
            Score::Lcb => "LCB",
            Score::Sharpe => "SHARPE",
            Score::Sortino => "SORTINO",
            Score::Calmar => "CALMAR",
            Score::Pnl30 => "PNL30",
            Score::Weeks => "WEEKS",
            Score::Combo => "COMBO",
            Score::Combo3 => "COMBO3",
        }
    }

    /// The closed trips a trader needs for the score to mean anything: over their risk
    /// (t-statistic, the lower bound: their spread; profit factor: the losses; expectancy /
    /// drawdown, Calmar: the drawdown), their number (expectancy x sqrt(trips) is the PnL /
    /// sqrt(trips): the fewer, the higher) or their time (Sharpe, Sortino, weeks)
    /// `RISK_TRADES`; a PnL sum one.
    pub fn min_trades(self) -> u64 {
        match self {
            Score::Pnl | Score::Pnl30 => 1,
            _ => RISK_TRADES,
        }
    }
}

/// A trader's statistics: its paper copy's, or the backtest's (`source`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct TraderStats {
    pub trader: String,
    pub source: &'static str,
    pub closed_trades: u64,
    pub pnl_net: f64,
    /// Average closed trip's PnL ($) and their standard deviation.
    pub expectancy: f64,
    pub pnl_sd: f64,
    pub profit_factor: f64,
    pub win_rate: f64,
    pub max_drawdown_pct: f64,
    pub avg_holding_h: f64,
    pub avg_slippage_bps: f64,
    pub pnl_7d: Option<f64>,
    pub pnl_30d: Option<f64>,
    pub current_open_risk: f64,
    pub coins: BTreeMap<String, u32>,
    /// Over time (the backtest's): daily Sharpe and Sortino, Calmar, weeks in profit.
    pub sharpe: f64,
    pub sortino: f64,
    pub calmar: f64,
    pub weeks_pos: f64,
    pub weeks: u32,
}

impl TraderStats {
    /// From closed trips' counts and sums: wins ($), losses ($, positive), the sum of squares.
    #[allow(clippy::too_many_arguments)]
    pub fn from_trips(trader: &str, source: &'static str, trips: u64, wins: u64, win_usd: f64, loss_usd: f64, pnl_sq: f64, dd: f64, slip: f64) -> Self {
        let n = trips.max(1) as f64;
        let exp = (win_usd - loss_usd) / n;
        Self {
            trader: trader.to_string(),
            source,
            closed_trades: trips,
            expectancy: exp,
            pnl_sd: (pnl_sq / n - exp * exp).max(0.0).sqrt(),
            profit_factor: if loss_usd > 0.0 { win_usd / loss_usd } else if win_usd > 0.0 { 99.0 } else { 0.0 },
            win_rate: wins as f64 / n,
            max_drawdown_pct: dd,
            avg_slippage_bps: slip,
            ..Default::default()
        }
    }

    /// Why it is not eligible for any portfolio (None: it is).
    pub fn ineligible(&self) -> Option<&'static str> {
        if self.closed_trades == 0 || (self.source != "backtest" && self.closed_trades < MIN_TRADES) {
            Some("too little history")
        } else if self.pnl_net <= 0.0 {
            Some("at a loss")
        } else {
            None
        }
    }

    /// The trader's score (COMBO is ranked from the others', in `rerank`).
    pub fn score(&self, s: Score) -> f64 {
        let n = self.closed_trades as f64;
        match s {
            Score::ExpDd => self.expectancy / self.max_drawdown_pct.max(1.0),
            Score::Pnl => self.pnl_net,
            Score::TStat => if self.pnl_sd > 0.0 { self.expectancy / self.pnl_sd * n.sqrt() } else { 0.0 },
            Score::Pf => self.profit_factor.min(50.0),
            Score::PnlSqrtN => self.expectancy * n.sqrt(),
            Score::Lcb => self.expectancy - 2.0 * self.pnl_sd / n.max(1.0).sqrt(),
            Score::Sharpe => self.sharpe,
            Score::Sortino => self.sortino.min(99.0),
            Score::Calmar => self.calmar,
            Score::Pnl30 => self.pnl_30d.unwrap_or(0.0),
            // Ties: the more weeks first.
            Score::Weeks => self.weeks_pos + self.weeks as f64 * 1e-6,
            Score::Combo | Score::Combo3 => 0.0,
        }
    }
}

/// A move of a followed account's position (as the paper copies see it).
#[derive(Clone, Debug)]
pub struct Signal {
    pub user: String,
    pub coin: String,
    /// Its direction, its size as a share of its peak in the position (0: flat), and its size.
    pub dir: f64,
    pub frac: f64,
    pub size: f64,
    /// Its fills opened the position.
    pub entry: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Leg {
    pub user: String,
    pub coin: String,
    pub dir: f64,
    /// Size at entry (set then), now (signed), (average) entry price, risk granted at entry.
    pub full: f64,
    pub size: f64,
    pub entry_px: f64,
    pub risk: f64,
    pub opened: f64,
    /// The risk in USD at entry (the portfolio's equity x `risk`), its PnL realized net of its
    /// fees, its fees, and the size and notional it has closed (its exit price).
    #[serde(default)]
    pub risk_usd: f64,
    #[serde(default)]
    pub pnl: f64,
    #[serde(default)]
    pub fees: f64,
    #[serde(default)]
    pub closed_sz: f64,
    #[serde(default)]
    pub closed_ntl: f64,
    /// Following adds: the trader's size at our entry and at its peak since, ours at that peak,
    /// and the most it was of `full`.
    #[serde(default)]
    pub size0: f64,
    #[serde(default)]
    pub their_peak: f64,
    #[serde(default)]
    pub top: f64,
    #[serde(default)]
    pub max_x: f64,
}

impl Leg {
    /// Its risk now: as granted, scaled by its size over its entry's (reduced: less; added: more).
    fn risk_now(&self) -> f64 {
        if self.full > 0.0 { self.risk * self.size.abs() / self.full } else { 0.0 }
    }

    /// A fill of the leg's own: adding averages its entry, reducing realizes against it.
    fn fill(&mut self, got: f64, px: f64, fee: f64) {
        if got * self.dir > 0.0 {
            let (a, b) = (self.size.abs(), got.abs());
            self.entry_px = if a + b > 0.0 { (self.entry_px * a + px * b) / (a + b) } else { px };
        } else {
            self.pnl += got.abs() * (px - self.entry_px) * self.dir;
            self.closed_sz += got.abs();
            self.closed_ntl += got.abs() * px;
        }
        self.size += got;
        self.pnl -= fee;
        self.fees += fee;
        if self.full > 0.0 {
            self.max_x = self.max_x.max(self.size.abs() / self.full);
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Portfolio {
    pub name: String,
    /// Its sizing (`Mode`): risk an entry, cap in one direction, the trader's adds followed.
    #[serde(default)]
    pub risk: f64,
    #[serde(default)]
    pub cap_dir: f64,
    #[serde(default)]
    pub adds: bool,
    /// The breaker on (`_BRK`): each trader's losing positions in a row, and when its pause ends.
    #[serde(default)]
    pub brk: bool,
    #[serde(default)]
    pub losing: HashMap<String, u32>,
    #[serde(default)]
    pub paused: HashMap<String, f64>,
    pub acct: Account,
    /// `user|coin` -> leg.
    pub legs: HashMap<String, Leg>,
    /// Its traders now: rank (1 = best) and score (not saved: ranked again at a start).
    #[serde(skip)]
    pub picked: BTreeMap<String, (usize, f64)>,
    pub entries: u64,
    pub refused: u64,
    pub stops: u64,
    /// Entries of traders it has not picked (counted, not written), and of a trader paused.
    #[serde(default)]
    pub not_picked: u64,
    #[serde(default)]
    pub braked: u64,
}

impl Portfolio {
    /// The risk it can still take for `user` in `dir` (the least room of its caps; under 0: none)
    /// and the cap that binds.
    fn room(&self, user: &str, dir: f64) -> (f64, &'static str) {
        let (mut total, mut side, mut trader) = (0.0, 0.0, 0.0);
        for l in self.legs.values() {
            let r = l.risk_now();
            total += r;
            if l.dir == dir {
                side += r;
            }
            if l.user == user {
                trader += r;
            }
        }
        [(CAP_TOTAL - total, "TOTAL_RISK_CAP"), (self.cap_dir - side, "DIRECTION_RISK_CAP"), (CAP_TRADER - trader, "TRADER_RISK_CAP")]
            .into_iter().fold((f64::MAX, "TOTAL_RISK_CAP"), |a, b| if b.0 < a.0 { b } else { a })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Allocators {
    pub portfolios: Vec<Portfolio>,
    /// Eligible traders as of `ranked_at`.
    pub eligible: usize,
    pub ranked_at: f64,
    /// How many traders' backtest statistics it was ranked with (a different set: rank again).
    #[serde(default)]
    pub seeded: usize,
    #[serde(default)]
    pub generation: u32,
    /// When its state was last saved and its status written (`SAVE_EVERY_S`, `STATUS_EVERY_S`).
    #[serde(skip)]
    pub saved_at: f64,
    #[serde(skip)]
    pub status_at: f64,
}

fn key(user: &str, coin: &str) -> String {
    format!("{user}|{coin}")
}

impl Allocators {
    /// The portfolios of `modes` (any kept from `saved` by name, the rest new; none of a
    /// saved state of another `GENERATION`), to be ranked at once.
    pub fn new(saved: Option<Allocators>) -> Self {
        let saved = saved.filter(|s| s.generation == GENERATION);
        let mut old: HashMap<String, Portfolio> = saved.as_ref().map(|s| s.portfolios.iter().map(|p| (p.name.clone(), p.clone())).collect()).unwrap_or_default();
        let portfolios = modes().into_iter().map(|m| {
            let mut p = old.remove(&m.name()).unwrap_or_else(|| Portfolio { name: m.name(), acct: Account::new(START_USD), ..Default::default() });
            (p.risk, p.cap_dir, p.adds, p.brk) = (m.risk, m.cap_dir, m.adds, m.brk);
            p
        }).collect();
        Self {
            portfolios,
            eligible: saved.as_ref().map(|s| s.eligible).unwrap_or(0),
            seeded: saved.as_ref().map(|s| s.seeded).unwrap_or(0),
            ranked_at: 0.0,
            generation: GENERATION,
            saved_at: 0.0,
            status_at: 0.0,
        }
    }

    /// Ranks the eligible traders and picks each portfolio's: those with its score's
    /// `min_trades`, at random from all.
    pub fn rerank(&mut self, stats: &[TraderStats], at: f64) -> Value {
        let elig: Vec<&TraderStats> = stats.iter().filter(|s| s.ineligible().is_none()).collect();
        self.eligible = elig.len();
        self.ranked_at = at;
        // Each score's ranking once (its portfolios of any size and sizing take its top).
        let mut ranked: HashMap<Score, Vec<(&TraderStats, f64)>> = SCORES.iter().filter(|s| !matches!(s, Score::Combo | Score::Combo3)).map(|s| {
            let mut v: Vec<(&TraderStats, f64)> = elig.iter().copied().filter(|t| t.closed_trades >= s.min_trades()).map(|t| (t, t.score(*s))).collect();
            v.sort_by(|a, b| b.1.total_cmp(&a.1));
            (*s, v)
        }).collect();
        // COMBO: the mean percentile (0 = top) over the others, of the traders all of them rank.
        let mut pct: HashMap<&str, f64> = HashMap::new();
        for v in ranked.values() {
            let r: Vec<&TraderStats> = v.iter().map(|x| x.0).filter(|t| t.closed_trades >= RISK_TRADES).collect();
            let m = r.len().saturating_sub(1).max(1) as f64;
            for (j, t) in r.iter().enumerate() {
                *pct.entry(t.trader.as_str()).or_default() += j as f64 / m;
            }
        }
        let others = (SCORES.len() - 2) as f64;
        let mut combo: Vec<(&TraderStats, f64)> = elig.iter().copied().filter(|t| t.closed_trades >= RISK_TRADES)
            .map(|t| (t, 1.0 - pct.get(t.trader.as_str()).copied().unwrap_or(others) / others)).collect();
        combo.sort_by(|a, b| b.1.total_cmp(&a.1));
        // COMBO3: in the top `COMBO3_TOP` of 2+ of the three steadiest on paper, by mean place.
        let place: Vec<HashMap<&str, usize>> = [Score::Lcb, Score::ExpDd, Score::PnlSqrtN].iter()
            .map(|s| ranked[s].iter().take(COMBO3_TOP).enumerate().map(|(j, (t, _))| (t.trader.as_str(), j)).collect()).collect();
        let mut combo3: Vec<(&TraderStats, f64)> = elig.iter().copied().filter(|t| t.closed_trades >= RISK_TRADES)
            .filter(|t| place.iter().filter(|m| m.contains_key(t.trader.as_str())).count() >= 2)
            .map(|t| (t, -(place.iter().map(|m| m.get(t.trader.as_str()).copied().unwrap_or(COMBO3_TOP)).sum::<usize>() as f64) / 3.0)).collect();
        combo3.sort_by(|a, b| b.1.total_cmp(&a.1));
        ranked.insert(Score::Combo, combo);
        ranked.insert(Score::Combo3, combo3);
        let index: HashMap<String, usize> = self.portfolios.iter().enumerate().map(|(i, p)| (p.name.clone(), i)).collect();
        let mut out = serde_json::Map::new();
        for m in modes() {
            let chosen: Vec<(usize, &TraderStats, f64)> = match m.score {
                Some(s) => ranked[&s].iter().take(m.n).enumerate().map(|(k, (t, sc))| (k + 1, *t, *sc)).collect(),
                None => {
                    // At random, seeded by the day, the draw and the size: a start picks the
                    // same, and the draw's portfolios of every sizing the same traders.
                    let mut pool = elig.clone();
                    let mut seed = ((at / 86_400.0) as u64 ^ 0x9e37_79b9_7f4a_7c15)
                        .wrapping_add(m.draw.wrapping_mul(0x2545_f491_4f6c_dd1d))
                        .wrapping_add((m.n as u64).wrapping_mul(0x9e37_79b9));
                    let mut v = Vec::new();
                    while !pool.is_empty() && v.len() < m.n {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        let t = pool.swap_remove((seed % pool.len() as u64) as usize);
                        v.push((v.len() + 1, t, 0.0));
                    }
                    v
                }
            };
            let name = m.name();
            if let Some(&i) = index.get(&name) {
                self.portfolios[i].picked = chosen.iter().map(|(r, t, s)| (t.trader.clone(), (*r, *s))).collect();
            }
            // The top of each score / draw once (all its sizes and sizings take from it).
            if m.risk == RISKS[0] && m.cap_dir == CAPS_DIR[0] && m.n == SIZES[0] && !m.adds && !m.brk {
                out.insert(name.split('_').next().unwrap_or("").to_string(), json!(chosen.iter().take(10).map(|(_, t, s)| json!({"trader": t.trader,
                    "score": s, "source": t.source, "trades": t.closed_trades, "pnl": t.pnl_net})).collect::<Vec<_>>()));
            }
        }
        json!({"eligible": elig.len(), "of": stats.len(), "from_paper": elig.iter().filter(|s| s.source == "paper").count(), "top": out})
    }

    /// A signal: each portfolio follows its legs and decides on entries. Rows: (table, row),
    /// `allocator_decisions` for each entry cut or refused, `allocator_trades` for each
    /// position closed.
    pub fn on_signal(&mut self, s: &Signal, book: &Book, marks: &dyn Fn(&str) -> Option<f64>, at: f64) -> Vec<(&'static str, Value)> {
        let mut rows = Vec::new();
        let generation = self.generation;
        let k = key(&s.user, &s.coin);
        for p in self.portfolios.iter_mut() {
            if let Some(l) = p.legs.get(&k).cloned() {
                if s.frac <= 0.0 || s.dir != l.dir {
                    let why = if s.frac <= 0.0 { "TRADER_CLOSED" } else { "TRADER_FLIPPED" };
                    rows.extend(close(p, &k, book, why, at, generation).map(|r| ("allocator_trades", r)));
                    continue;
                }
                let new_peak = s.size >= l.their_peak;
                let mut target = if !p.adds {
                    l.full * s.frac.min(1.0)
                } else if new_peak {
                    l.full * (s.size / l.size0.max(1e-12)).min(ADD_MAX)
                } else {
                    l.top * s.size / l.their_peak.max(1e-12)
                };
                // An add grows the leg's risk: within the caps.
                let now = l.size.abs();
                if p.adds && target > now && l.full > 0.0 {
                    let room = p.room(&l.user, l.dir).0.max(0.0);
                    target = target.min(now + room / l.risk * l.full);
                }
                let delta = l.dir * target - l.size;
                let mid = book.mid().unwrap_or(l.entry_px);
                if delta.abs() * mid >= MIN_ORDER_USD {
                    if let Some((got, px, fee)) = trade(&mut p.acct, &s.coin, delta, book) {
                        if let Some(leg) = p.legs.get_mut(&k) {
                            leg.fill(got, px, fee);
                        }
                    }
                }
                if new_peak {
                    if let Some(leg) = p.legs.get_mut(&k) {
                        leg.their_peak = s.size;
                        leg.top = leg.size.abs();
                    }
                }
                continue;
            }
            if !(s.entry && s.frac > 0.0) {
                continue;
            }
            let Some(picked) = p.picked.get(&s.user).copied() else {
                p.not_picked += 1;
                continue;
            };
            if p.brk && p.paused.get(&s.user).is_some_and(|&until| until > at) {
                p.braked += 1;
                continue;
            }
            let mut row = json!({"portfolio": p.name, "generation": generation, "trader": s.user, "coin": s.coin, "side": if s.dir > 0.0 { "long" } else { "short" },
                "action": "open", "eligible": true, "rank": picked.0, "score": picked.1,
                "requested_risk": p.risk, "granted_risk": 0.0, "reject_reason": null, "at": crate::liq::iso(at)});
            let mut decide = |row: Value| rows.push(("allocator_decisions", row));
            if p.legs.values().filter(|l| l.coin == s.coin).count() >= MAX_PER_COIN {
                row["reject_reason"] = json!("COIN_POSITIONS_CAP");
                p.refused += 1;
                decide(row);
                continue;
            }
            let (left, binding) = p.room(&s.user, s.dir);
            let room = p.risk.min(left.max(0.0));
            if room < p.risk / 8.0 {
                row["reject_reason"] = json!(binding);
                p.refused += 1;
                decide(row);
                continue;
            }
            let equity = p.acct.equity(marks).max(0.0);
            let Some(mid) = book.mid() else { continue };
            let full = room / STOP * equity / mid;
            if full * mid < MIN_ORDER_USD {
                row["reject_reason"] = json!("TOO_SMALL");
                decide(row);
                continue;
            }
            let Some((got, px, fee)) = trade(&mut p.acct, &s.coin, s.dir * full * s.frac.min(1.0), book) else { continue };
            // Its own fill's price: the account's entry is the coin's legs' average.
            let mut leg = Leg { user: s.user.clone(), coin: s.coin.clone(), dir: s.dir, full, size: 0.0, entry_px: px, risk: room, opened: at,
                risk_usd: room * equity, size0: s.size, their_peak: s.size, ..Default::default() };
            leg.fill(got, px, fee);
            leg.top = leg.size.abs();
            p.legs.insert(k.clone(), leg);
            p.entries += 1;
            // Granted in full: no row (the state has it, then allocator_trades); cut: why.
            if room < p.risk {
                row["granted_risk"] = json!(room);
                row["reject_reason"] = json!(binding);
                decide(row);
            }
        }
        rows
    }

    /// Stops: a leg `STOP` against its (average) entry is closed at the book. Rows for
    /// `allocator_trades`.
    pub fn check_stops(&mut self, books: &HashMap<String, Book>, at: f64) -> Vec<Value> {
        let mids: HashMap<&str, f64> = books.iter().filter_map(|(c, b)| Some((c.as_str(), b.mid()?))).collect();
        let mut rows = Vec::new();
        for p in self.portfolios.iter_mut() {
            let hit: Vec<String> = p.legs.iter().filter(|(_, l)| {
                mids.get(l.coin.as_str()).is_some_and(|m| (m / l.entry_px - 1.0) * l.dir <= -STOP)
            }).map(|(k, _)| k.clone()).collect();
            for k in hit {
                let coin = p.legs[&k].coin.clone();
                if let Some(b) = books.get(&coin) {
                    rows.extend(close(p, &k, b, "STOP", at, self.generation));
                    p.stops += 1;
                }
            }
        }
        rows
    }

    /// For `bot_status`: the portfolios in all (each one's is `status_rows`).
    pub fn status(&self, marks: &dyn Fn(&str) -> Option<f64>) -> Value {
        let eq: Vec<(f64, &str)> = self.portfolios.iter().map(|p| (p.acct.equity(marks), p.name.as_str())).collect();
        let best = eq.iter().fold((f64::MIN, ""), |a, b| if b.0 > a.0 { *b } else { a });
        let worst = eq.iter().fold((f64::MAX, ""), |a, b| if b.0 < a.0 { *b } else { a });
        json!({"generation": self.generation, "portfolios": self.portfolios.len(), "eligible": self.eligible,
            "legs": self.portfolios.iter().map(|p| p.legs.len()).sum::<usize>(),
            "entries": self.portfolios.iter().map(|p| p.entries).sum::<u64>(), "stops": self.portfolios.iter().map(|p| p.stops).sum::<u64>(),
            "best": {"name": best.1, "return_pct": ((best.0 / START_USD - 1.0) * 1e4).round() / 100.0},
            "worst": {"name": worst.1, "return_pct": ((worst.0 / START_USD - 1.0) * 1e4).round() / 100.0}})
    }

    /// Each portfolio's status, rows of `allocator_status`.
    pub fn status_rows(&self, marks: &dyn Fn(&str) -> Option<f64>, at: f64) -> Vec<Value> {
        self.portfolios.iter().map(|p| {
            let eq = p.acct.equity(marks);
            let risk = |f: &dyn Fn(&Leg) -> bool| (p.legs.values().filter(|l| f(l)).map(Leg::risk_now).sum::<f64>() * 1e4).round() / 100.0 + 0.0;
            json!({"generation": self.generation, "at": crate::liq::iso(at), "portfolio": p.name, "equity": (eq * 100.0).round() / 100.0,
                "return_pct": ((eq / START_USD - 1.0) * 1e4).round() / 100.0, "open_risk_pct": risk(&|_| true), "long_risk_pct": risk(&|l| l.dir > 0.0),
                "legs": p.legs.len(), "entries": p.entries, "refused": p.refused, "stops": p.stops, "not_picked": p.not_picked, "braked": p.braked,
                "fees": p.acct.fees})
        }).collect()
    }
}

/// A taker fill of `delta` on the book: the size filled (signed), its price and fee.
fn trade(acct: &mut Account, coin: &str, delta: f64, book: &Book) -> Option<(f64, f64, f64)> {
    let side = if delta > 0.0 { &book.asks } else { &book.bids };
    let (got, px) = walk(side, delta.abs());
    if got <= 0.0 {
        return None;
    }
    let (filled, fee) = (got * delta.signum(), got * px * TAKER_FEE);
    acct.fill(coin, filled, px, fee);
    Some((filled, px, fee))
}

/// Closes a leg at the book: its `allocator_trades` row.
fn close(p: &mut Portfolio, k: &str, book: &Book, why: &str, at: f64, generation: u32) -> Option<Value> {
    let mut l = p.legs.remove(k)?;
    if l.size != 0.0 {
        if let Some((got, px, fee)) = trade(&mut p.acct, &l.coin, -l.size, book) {
            l.fill(got, px, fee);
        }
    }
    if p.brk {
        let n = p.losing.entry(l.user.clone()).or_default();
        *n = if l.pnl < 0.0 { *n + 1 } else { 0 };
        if *n >= BRK_LOSSES {
            p.losing.remove(&l.user);
            p.paused.insert(l.user.clone(), at + BRK_PAUSE_S);
        }
        p.paused.retain(|_, &mut until| until > at);
    }
    Some(json!({"portfolio": p.name, "generation": generation, "trader": l.user, "coin": l.coin,
        "side": if l.dir > 0.0 { "long" } else { "short" }, "opened_at": crate::liq::iso(l.opened), "closed_at": crate::liq::iso(at),
        "held_h": (at - l.opened) / 3600.0, "entry_px": l.entry_px, "exit_px": if l.closed_sz > 0.0 { l.closed_ntl / l.closed_sz } else { l.entry_px },
        "risk": l.risk, "risk_usd": l.risk_usd, "notional": l.full * l.entry_px, "pnl": l.pnl, "fees": l.fees,
        "r": if l.risk_usd > 0.0 { l.pnl / l.risk_usd } else { 0.0 }, "max_x": l.max_x, "reason": why}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(mid: f64) -> Book {
        Book { bids: vec![(mid * 0.9999, 1e9)], asks: vec![(mid * 1.0001, 1e9)], time_ms: 0 }
    }

    fn stats(t: &str, trades: u64, exp: f64, dd: f64) -> TraderStats {
        let mut s = TraderStats::from_trips(t, "paper", trades, trades / 2, exp * trades as f64 * 2.0, exp * trades as f64, 0.0, dd, 5.0);
        s.pnl_net = exp * trades as f64;
        s
    }

    /// A portfolio by its score / draw and size, at 2% risk, no cap in one direction, no adds.
    fn port<'a>(a: &'a Allocators, n: &str) -> &'a Portfolio {
        named(a, &format!("{n}_R2_D100"))
    }

    fn named<'a>(a: &'a Allocators, n: &str) -> &'a Portfolio {
        a.portfolios.iter().find(|p| p.name == n).unwrap_or_else(|| panic!("{n}"))
    }

    fn sig(u: &str, c: &str) -> Signal {
        Signal { user: u.into(), coin: c.into(), dir: 1.0, frac: 1.0, size: 1.0, entry: true }
    }

    fn row(rows: &[(&str, Value)], n: &str) -> Option<Value> {
        rows.iter().find(|r| r.1["portfolio"] == n).map(|r| r.1.clone())
    }

    const RISK: f64 = 0.02;

    #[test]
    fn modes_rank_on_their_own() {
        // b: the better expectancy / drawdown; a: the bigger PnL.
        // c: few trips (the backtest's), the biggest PnL; d: at a loss.
        let mut s = [stats("a", 100, 5.0, 10.0), stats("b", 30, 5.0, 1.0), stats("c", 10, 60.0, 1.0), stats("d", 50, -1.0, 1.0)];
        s[2].source = "backtest";
        assert_eq!(s[3].ineligible(), Some("at a loss"));
        let mut few = stats("e", 5, 10.0, 1.0);
        assert_eq!(few.ineligible(), Some("too little history"));
        few.source = "backtest";
        assert_eq!(few.ineligible(), None);
        let mut a = Allocators::new(None);
        assert_eq!(a.portfolios.len(), (13 + 10) * 5 * 3 * 3 * 2 + (7 + 10) * 3 * 3 * 2);
        assert!(a.portfolios.iter().any(|p| p.name == "LCB_20_R0.5_D20_BRK" && p.brk));
        assert!(a.portfolios.iter().any(|p| p.name == "EXPDD_20_R0.5_D20" && p.risk == 0.005 && p.cap_dir == 0.2 && !p.adds));
        assert!(a.portfolios.iter().any(|p| p.name == "RANDOM10_100_R1_D50_ADD" && p.adds));
        a.rerank(&s, 0.0);
        assert_eq!(port(&a, "EXPDD_10").picked["b"].0, 1);
        // A score over the trips' risk, number or time: not on 10 trips; a PnL sum: on any.
        for n in ["EXPDD_10", "TSTAT_30", "PF_30", "PNLSQRTN_10", "LCB_10", "SHARPE_10", "WEEKS_10", "COMBO_10"] {
            assert!(!port(&a, n).picked.contains_key("c"), "{n}");
        }
        assert_eq!(port(&a, "PNL_10").picked["c"].0, 1);
        assert!(!port(&a, "PNL_10").picked.contains_key("d"));
        // COMBO: the traders all the scores rank (20+ trips, eligible).
        assert_eq!(port(&a, "COMBO_10").picked.keys().cloned().collect::<Vec<_>>(), ["a", "b"]);
        assert_eq!(port(&a, "RANDOM1_10").picked.len(), 3);
        // A draw picks the same traders at every sizing.
        let same = |n: &str| named(&a, n).picked.keys().cloned().collect::<Vec<_>>();
        assert_eq!(same("RANDOM2_10_R0.5_D20"), same("RANDOM2_10_R2_D100_ADD"));
        // Kept across a restart by name (not its picks: ranked again); not from another generation.
        let saved: Allocators = serde_json::from_value(serde_json::to_value(&a).unwrap()).unwrap();
        let b = Allocators::new(Some(saved));
        assert!(b.ranked_at == 0.0 && b.portfolios.iter().all(|p| p.picked.is_empty()) && b.portfolios.len() == a.portfolios.len());
        let b = Allocators::new(Some(Allocators { generation: 1, ..a.clone() }));
        assert!(port(&b, "PNL_10").legs.is_empty());
    }

    #[test]
    fn two_positions_a_coin_and_risk_caps() {
        let s: Vec<TraderStats> = (0..6).map(|i| stats(&format!("t{i}"), 30, 5.0, 2.0 + i as f64)).collect();
        let mut a = Allocators::new(None);
        a.rerank(&s, 0.0);
        let b = book(100.0);
        let marks = |_: &str| Some(100.0);
        a.on_signal(&sig("t0", "BTC"), &b, &marks, 0.0);
        a.on_signal(&sig("t1", "BTC"), &b, &marks, 0.0);
        let rows = a.on_signal(&sig("t2", "BTC"), &b, &marks, 0.0);
        assert_eq!(row(&rows, "EXPDD_10_R2_D100").unwrap()["reject_reason"], "COIN_POSITIONS_CAP");
        let p = port(&a, "EXPDD_10");
        assert_eq!(p.legs.len(), 2);
        assert!((p.legs["t0|BTC"].full * 100.0 - RISK / STOP * START_USD).abs() < 1.0);
        // Granted in full: no row.
        let rows = a.on_signal(&sig("t3", "SOL"), &b, &marks, 0.0);
        assert!(row(&rows, "EXPDD_10_R2_D100").is_none() && port(&a, "EXPDD_10").legs.contains_key("t3|SOL"));
        // Not picked.
        let rows = a.on_signal(&sig("zz", "ETH"), &b, &marks, 0.0);
        assert!(rows.is_empty() && port(&a, "EXPDD_10").not_picked == 1);
        // The trader halves: the leg too; flat: closed.
        a.on_signal(&Signal { frac: 0.5, size: 0.5, entry: false, ..sig("t0", "BTC") }, &b, &marks, 1.0);
        let l = &port(&a, "EXPDD_10").legs["t0|BTC"];
        assert!((l.size - l.full / 2.0).abs() < 1e-6);
        let rows = a.on_signal(&Signal { frac: 0.0, size: 0.0, entry: false, ..sig("t0", "BTC") }, &b, &marks, 7200.0);
        assert!(!port(&a, "EXPDD_10").legs.contains_key("t0|BTC"));
        let t = &rows.iter().find(|r| r.0 == "allocator_trades" && r.1["portfolio"] == "EXPDD_10_R2_D100").unwrap().1;
        assert_eq!((t["reason"].as_str(), t["held_h"].as_f64()), (Some("TRADER_CLOSED"), Some(2.0)));
        // At the same price: a loss of its fees only (and the spread).
        assert!(t["pnl"].as_f64().unwrap() < 0.0 && t["pnl"].as_f64().unwrap() > -2.0 * t["fees"].as_f64().unwrap());
        // Per trader: 20% = ten 2% entries, the eleventh refused.
        let mut a = Allocators::new(None);
        a.rerank(&s, 0.0);
        for c in ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"] {
            a.on_signal(&sig("t0", c), &b, &marks, 0.0);
        }
        let rows = a.on_signal(&sig("t0", "K"), &b, &marks, 0.0);
        assert_eq!(row(&rows, "EXPDD_10_R2_D100").unwrap()["reject_reason"], "TRADER_RISK_CAP");
        // In one direction: 20% at 2% = ten longs (of five traders), the eleventh refused, a short let in.
        let mut a = Allocators::new(None);
        a.rerank(&s, 0.0);
        for (i, c) in ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"].iter().enumerate() {
            a.on_signal(&sig(&format!("t{}", i % 5), c), &b, &marks, 0.0);
        }
        let rows = a.on_signal(&sig("t5", "K"), &b, &marks, 0.0);
        assert_eq!(row(&rows, "EXPDD_10_R2_D20").unwrap()["reject_reason"], "DIRECTION_RISK_CAP");
        assert!(row(&rows, "EXPDD_10_R2_D50").is_none() && named(&a, "EXPDD_10_R2_D50").legs.contains_key("t5|K"));
        a.on_signal(&Signal { dir: -1.0, ..sig("t5", "L") }, &b, &marks, 0.0);
        assert!(named(&a, "EXPDD_10_R2_D20").legs.contains_key("t5|L"));
        // At 0.5% an entry is a quarter the size.
        assert!((named(&a, "EXPDD_10_R0.5_D100").legs["t0|A"].full * 100.0 - 0.005 / STOP * START_USD).abs() < 1.0);
    }

    #[test]
    fn adds_followed_up_to_twice_within_the_caps() {
        let s = [stats("t0", 30, 5.0, 2.0)];
        let mut a = Allocators::new(None);
        a.rerank(&s, 0.0);
        let b = book(100.0);
        let marks = |_: &str| Some(100.0);
        a.on_signal(&sig("t0", "BTC"), &b, &marks, 0.0);
        // The trader adds 50%: with adds, the leg too; without, it holds.
        a.on_signal(&Signal { size: 1.5, entry: false, ..sig("t0", "BTC") }, &b, &marks, 1.0);
        let (fixed, add) = (&named(&a, "EXPDD_10_R2_D100").legs["t0|BTC"], &named(&a, "EXPDD_10_R2_D100_ADD").legs["t0|BTC"]);
        assert!((fixed.size / fixed.full - 1.0).abs() < 1e-6);
        assert!((add.size / add.full - 1.5).abs() < 1e-6);
        // It triples: twice at most.
        a.on_signal(&Signal { size: 3.0, entry: false, ..sig("t0", "BTC") }, &b, &marks, 2.0);
        let add = &named(&a, "EXPDD_10_R2_D100_ADD").legs["t0|BTC"];
        assert!((add.size / add.full - ADD_MAX).abs() < 1e-6 && (add.risk_now() - 2.0 * RISK).abs() < 1e-9);
        // It halves from its peak: so does the leg, from its size at that peak.
        a.on_signal(&Signal { frac: 0.5, size: 1.5, entry: false, ..sig("t0", "BTC") }, &b, &marks, 3.0);
        let (fixed, add) = (&named(&a, "EXPDD_10_R2_D100").legs["t0|BTC"], &named(&a, "EXPDD_10_R2_D100_ADD").legs["t0|BTC"]);
        assert!((fixed.size / fixed.full - 0.5).abs() < 1e-6 && (add.size / add.full - 1.0).abs() < 1e-6);
        // Within the caps: at 2% with 20% in one direction, nine more longs and its own fill
        // the 20%: no add; at 50%, the whole add.
        let s: Vec<TraderStats> = (0..6).map(|i| stats(&format!("t{i}"), 30, 5.0, 2.0)).collect();
        let mut a = Allocators::new(None);
        a.rerank(&s, 0.0);
        for (i, c) in ["A", "B", "C", "D", "E", "F", "G", "H", "I"].iter().enumerate() {
            a.on_signal(&sig(&format!("t{}", 1 + i % 5), c), &b, &marks, 0.0);
        }
        a.on_signal(&sig("t0", "BTC"), &b, &marks, 0.0);
        a.on_signal(&Signal { size: 2.0, entry: false, ..sig("t0", "BTC") }, &b, &marks, 1.0);
        let l = &named(&a, "EXPDD_10_R2_D20_ADD").legs["t0|BTC"];
        assert!((l.size / l.full - 1.0).abs() < 1e-6, "{}", l.size / l.full);
        let l = &named(&a, "EXPDD_10_R2_D50_ADD").legs["t0|BTC"];
        assert!((l.size / l.full - 2.0).abs() < 1e-6);
    }

    #[test]
    fn stop_closes_a_leg() {
        let s = [stats("t0", 30, 5.0, 2.0)];
        let mut a = Allocators::new(None);
        a.rerank(&s, 0.0);
        let marks = |_: &str| Some(100.0);
        a.on_signal(&sig("t0", "BTC"), &book(100.0), &marks, 0.0);
        let books: HashMap<String, Book> = [("BTC".to_string(), book(79.0))].into();
        let rows = a.check_stops(&books, 1.0);
        let p = port(&a, "EXPDD_10");
        assert!(p.legs.is_empty() && p.stops == 1);
        assert!(p.acct.equity(&|_| Some(79.0)) < START_USD);
        let t = rows.iter().find(|r| r["portfolio"] == "EXPDD_10_R2_D100").unwrap();
        assert_eq!(t["reason"], "STOP");
        // 2% of the portfolio at the stop, a hair more with the fees and the spread.
        let r = t["r"].as_f64().unwrap();
        assert!(r < -1.0 && r > -1.1, "{r}");
        // The status rows: one a portfolio.
        assert_eq!(a.status_rows(&marks, 2.0).len(), a.portfolios.len());
    }

    #[test]
    fn breaker_pauses_a_trader_after_five_losses() {
        let s = [stats("t0", 30, 5.0, 2.0)];
        let mut a = Allocators::new(None);
        a.rerank(&s, 0.0);
        let marks = |_: &str| Some(100.0);
        let b = book(100.0);
        // Five round trips at the same price: each a loss of its fees.
        for k in 0..5 {
            let at = k as f64 * 10.0;
            a.on_signal(&sig("t0", "BTC"), &b, &marks, at);
            a.on_signal(&Signal { frac: 0.0, size: 0.0, entry: false, ..sig("t0", "BTC") }, &b, &marks, at + 1.0);
        }
        a.on_signal(&sig("t0", "ETH"), &b, &marks, 100.0);
        assert!(!named(&a, "LCB_10_R2_D20_BRK").legs.contains_key("t0|ETH") && named(&a, "LCB_10_R2_D20_BRK").braked == 1);
        assert!(named(&a, "LCB_10_R2_D20").legs.contains_key("t0|ETH"));
        // Three days on: in again.
        a.on_signal(&sig("t0", "SOL"), &b, &marks, 100.0 + BRK_PAUSE_S);
        assert!(named(&a, "LCB_10_R2_D20_BRK").legs.contains_key("t0|SOL"));
    }
}
