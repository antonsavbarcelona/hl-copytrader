//! A second layer over the paper copies, apart from them: allocator portfolios that get the same
//! signals (each followed account's position moves, `Signal`) but decide whom to let in, each
//! with its own paper account and portfolio limits. The copies per account go on as they were.
//!
//! Each portfolio is a test of its own (`MODES`): a score to rank the eligible traders by and
//! how many of the top it takes, or as many at random (the benchmark: does the ranking add
//! anything over picking among the eligible blindly?). They share nothing but the signals.
//!
//!   - Each trader's statistics (`TraderStats`): its paper copy's once that has `MIN_TRADES`
//!     closed trips, until then the backtest's (`backtest_stats`) if it has them. Eligible:
//!     `MIN_TRADES`+ closed trips, PnL above 0, profit factor above `MIN_PF`, drawdown under
//!     `MAX_DD_PCT`, slippage under `MAX_SLIP_BP`.
//!   - Scores: expectancy / drawdown, PnL, t-statistic of the trips, profit factor, expectancy
//!     x sqrt(trips). Re-ranked once a day (`RANK_EVERY_S`).
//!   - A picked trader's entry gets `RISK` (its stop `STOP` away loses that much of the
//!     portfolio's equity), less as the caps require: total open risk `CAP_TOTAL`, per trader
//!     `CAP_TRADER`; at most `MAX_PER_COIN` positions in a coin (no cap per direction: on 90
//!     days walked forward it cut more good entries than it saved); under `MIN_GRANT` it is
//!     refused. Each entry's decision is a row of `allocator_decisions`.
//!   - A position is followed to its end (reduced in proportion as the trader reduces from its
//!     peak, closed when it is flat), or closed at the stop: a trader dropping off the list
//!     stops new entries only.
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
const RISK: f64 = 0.02;
const STOP: f64 = 0.20;
const MIN_GRANT: f64 = 0.0025;
const CAP_TOTAL: f64 = 1.00;
const CAP_TRADER: f64 = 0.20;
const MAX_PER_COIN: usize = 2;
pub const MIN_TRADES: u64 = 20;
const MIN_PF: f64 = 1.1;
const MAX_DD_PCT: f64 = 15.0;
const MAX_SLIP_BP: f64 = 30.0;
const MIN_ORDER_USD: f64 = 10.0;

/// The portfolios: name, score (None: at random among the eligible), how many traders.
pub const MODES: [(&str, Option<Score>, usize); 13] = [
    ("EXPDD_10", Some(Score::ExpDd), 10),
    ("EXPDD_30", Some(Score::ExpDd), 30),
    ("EXPDD_50", Some(Score::ExpDd), 50),
    ("PNL_10", Some(Score::Pnl), 10),
    ("PNL_30", Some(Score::Pnl), 30),
    ("PNLSQRTN_10", Some(Score::PnlSqrtN), 10),
    ("PNLSQRTN_30", Some(Score::PnlSqrtN), 30),
    ("PF_30", Some(Score::Pf), 30),
    ("PF_50", Some(Score::Pf), 50),
    ("TSTAT_30", Some(Score::TStat), 30),
    ("RANDOM_10", None, 10),
    ("RANDOM_30", None, 30),
    ("RANDOM_50", None, 50),
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Score {
    ExpDd,
    Pnl,
    TStat,
    Pf,
    PnlSqrtN,
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

    /// Why it is not eligible (None: it is).
    pub fn ineligible(&self) -> Option<&'static str> {
        if self.closed_trades < MIN_TRADES {
            Some("too few closed trades")
        } else if self.pnl_net <= 0.0 {
            Some("at a loss")
        } else if self.profit_factor <= MIN_PF {
            Some("profit factor")
        } else if self.max_drawdown_pct >= MAX_DD_PCT {
            Some("drawdown")
        } else if self.avg_slippage_bps >= MAX_SLIP_BP {
            Some("slippage")
        } else {
            None
        }
    }

    pub fn score(&self, s: Score) -> f64 {
        let n = self.closed_trades as f64;
        match s {
            Score::ExpDd => self.expectancy / self.max_drawdown_pct.max(1.0),
            Score::Pnl => self.pnl_net,
            Score::TStat => if self.pnl_sd > 0.0 { self.expectancy / self.pnl_sd * n.sqrt() } else { 0.0 },
            Score::Pf => self.profit_factor.min(50.0),
            Score::PnlSqrtN => self.expectancy * n.sqrt(),
        }
    }
}

/// A move of a followed account's position (as the paper copies see it).
#[derive(Clone, Debug)]
pub struct Signal {
    pub user: String,
    pub coin: String,
    /// Its direction and its size as a share of its peak in the position (0: flat).
    pub dir: f64,
    pub frac: f64,
    /// Its fills opened the position.
    pub entry: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Leg {
    pub user: String,
    pub coin: String,
    pub dir: f64,
    /// Size at the trader's peak (set at entry), now (signed), entry price, risk granted.
    pub full: f64,
    pub size: f64,
    pub entry_px: f64,
    pub risk: f64,
    pub opened: f64,
}

impl Leg {
    /// Its risk now: as granted, less what has been reduced.
    fn risk_now(&self) -> f64 {
        if self.full > 0.0 { self.risk * self.size.abs() / self.full } else { 0.0 }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Portfolio {
    pub name: String,
    pub acct: Account,
    /// `user|coin` -> leg.
    pub legs: HashMap<String, Leg>,
    /// Its traders now: rank (1 = best) and score.
    pub picked: BTreeMap<String, (usize, f64)>,
    pub entries: u64,
    pub refused: u64,
    pub stops: u64,
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
}

fn key(user: &str, coin: &str) -> String {
    format!("{user}|{coin}")
}

impl Allocators {
    /// The portfolios of `MODES` (any kept from `saved` by name, the rest new).
    pub fn new(saved: Option<Allocators>) -> Self {
        let mut old: HashMap<String, Portfolio> = saved.as_ref().map(|s| s.portfolios.iter().map(|p| (p.name.clone(), p.clone())).collect()).unwrap_or_default();
        let portfolios = MODES.iter().map(|(n, ..)| old.remove(*n).unwrap_or_else(|| Portfolio { name: n.to_string(), acct: Account::new(START_USD), ..Default::default() })).collect();
        Self {
            portfolios,
            eligible: saved.as_ref().map(|s| s.eligible).unwrap_or(0),
            seeded: saved.as_ref().map(|s| s.seeded).unwrap_or(0),
            ranked_at: saved.map(|s| s.ranked_at).unwrap_or(0.0),
        }
    }

    /// Ranks the eligible traders and picks each portfolio's.
    pub fn rerank(&mut self, stats: &[TraderStats], at: f64) -> Value {
        let elig: Vec<&TraderStats> = stats.iter().filter(|s| s.ineligible().is_none()).collect();
        self.eligible = elig.len();
        self.ranked_at = at;
        let mut out = serde_json::Map::new();
        for (i, (name, score, n)) in MODES.iter().enumerate() {
            let chosen: Vec<(usize, &TraderStats, f64)> = match score {
                Some(s) => {
                    let mut v: Vec<&TraderStats> = elig.clone();
                    v.sort_by(|a, b| b.score(*s).total_cmp(&a.score(*s)));
                    v.into_iter().take(*n).enumerate().map(|(k, t)| (k + 1, t, t.score(*s))).collect()
                }
                None => {
                    // At random, seeded by the day and the portfolio: a restart picks the same.
                    let mut pool = elig.clone();
                    let mut seed = ((at / 86_400.0) as u64 ^ 0x9e37_79b9_7f4a_7c15).wrapping_add((i as u64).wrapping_mul(0x2545_f491_4f6c_dd1d));
                    let mut v = Vec::new();
                    while !pool.is_empty() && v.len() < *n {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        let t = pool.swap_remove((seed % pool.len() as u64) as usize);
                        v.push((v.len() + 1, t, 0.0));
                    }
                    v
                }
            };
            if let Some(p) = self.portfolios.iter_mut().find(|p| p.name == *name) {
                p.picked = chosen.iter().map(|(r, t, s)| (t.trader.clone(), (*r, *s))).collect();
            }
            out.insert(name.to_string(), json!(chosen.iter().take(10).map(|(_, t, s)| json!({"trader": t.trader, "score": s, "source": t.source,
                "trades": t.closed_trades, "pnl": t.pnl_net})).collect::<Vec<_>>()));
        }
        json!({"eligible": elig.len(), "of": stats.len(), "from_paper": elig.iter().filter(|s| s.source == "paper").count(), "top": out})
    }

    /// A signal: each portfolio follows its legs and decides on entries. Rows for
    /// `allocator_decisions` (entries only).
    pub fn on_signal(&mut self, s: &Signal, book: &Book, marks: &dyn Fn(&str) -> Option<f64>, at: f64) -> Vec<Value> {
        let mut rows = Vec::new();
        for p in self.portfolios.iter_mut() {
            let k = key(&s.user, &s.coin);
            if let Some(l) = p.legs.get(&k).cloned() {
                if s.frac <= 0.0 || s.dir != l.dir {
                    close(p, &k, book);
                } else {
                    let target = l.dir * l.full * s.frac.min(1.0);
                    let delta = target - l.size;
                    let mid = book.mid().unwrap_or(l.entry_px);
                    if delta.abs() * mid >= MIN_ORDER_USD {
                        if let Some(got) = trade(&mut p.acct, &s.coin, delta, book) {
                            if let Some(leg) = p.legs.get_mut(&k) {
                                leg.size += got;
                            }
                        }
                    }
                    continue;
                }
            }
            if !(s.entry && s.frac > 0.0) {
                continue;
            }
            let picked = p.picked.get(&s.user).copied();
            let mut row = json!({"portfolio": p.name, "trader": s.user, "coin": s.coin, "side": if s.dir > 0.0 { "long" } else { "short" },
                "action": "open", "eligible": picked.is_some(), "rank": picked.map(|r| r.0), "score": picked.map(|r| r.1),
                "requested_risk": if picked.is_some() { RISK } else { 0.0 }, "granted_risk": 0.0, "reject_reason": null, "at": crate::liq::iso(at)});
            if picked.is_none() {
                row["reject_reason"] = json!("NOT_PICKED");
                rows.push(row);
                continue;
            }
            if p.legs.values().filter(|l| l.coin == s.coin).count() >= MAX_PER_COIN {
                row["reject_reason"] = json!("COIN_POSITIONS_CAP");
                p.refused += 1;
                rows.push(row);
                continue;
            }
            let sum = |f: &dyn Fn(&Leg) -> bool| p.legs.values().filter(|l| f(l)).map(Leg::risk_now).sum::<f64>();
            let rooms = [
                (CAP_TOTAL - sum(&|_| true), "TOTAL_RISK_CAP"),
                (CAP_TRADER - sum(&|l| l.user == s.user), "TRADER_RISK_CAP"),
            ];
            let (room, binding) = rooms.iter().fold((RISK, None), |(g, why), &(r, n)| if r < g { (r.max(0.0), Some(n)) } else { (g, why) });
            if room < MIN_GRANT {
                row["reject_reason"] = json!(binding.unwrap_or("TOTAL_RISK_CAP"));
                p.refused += 1;
                rows.push(row);
                continue;
            }
            let equity = p.acct.equity(marks).max(0.0);
            let Some(mid) = book.mid() else { continue };
            let full = room / STOP * equity / mid;
            if full * mid < MIN_ORDER_USD {
                row["reject_reason"] = json!("TOO_SMALL");
                rows.push(row);
                continue;
            }
            let Some(got) = trade(&mut p.acct, &s.coin, s.dir * full * s.frac.min(1.0), book) else { continue };
            let px = p.acct.positions.get(&s.coin).map(|x| x.entry).unwrap_or(mid);
            p.legs.insert(k, Leg { user: s.user.clone(), coin: s.coin.clone(), dir: s.dir, full, size: got, entry_px: px, risk: room, opened: at });
            p.entries += 1;
            row["granted_risk"] = json!(room);
            if room < RISK {
                row["reject_reason"] = json!(binding);
            }
            rows.push(row);
        }
        rows
    }

    /// Stops: a leg `STOP` against its entry is closed at the book.
    pub fn check_stops(&mut self, books: &HashMap<String, Book>) {
        for p in self.portfolios.iter_mut() {
            let hit: Vec<String> = p.legs.iter().filter(|(_, l)| {
                books.get(&l.coin).and_then(Book::mid).is_some_and(|m| (m / l.entry_px - 1.0) * l.dir <= -STOP)
            }).map(|(k, _)| k.clone()).collect();
            for k in hit {
                let coin = p.legs[&k].coin.clone();
                if let Some(b) = books.get(&coin) {
                    close(p, &k, b);
                    p.stops += 1;
                }
            }
        }
    }

    pub fn status(&self, marks: &dyn Fn(&str) -> Option<f64>) -> Value {
        Value::Array(self.portfolios.iter().map(|p| {
            let eq = p.acct.equity(marks);
            json!({"name": p.name, "equity": (eq * 100.0).round() / 100.0, "return_pct": ((eq / START_USD - 1.0) * 1e4).round() / 100.0,
                "open_risk_pct": (p.legs.values().map(Leg::risk_now).sum::<f64>() * 1e4).round() / 100.0 + 0.0, "legs": p.legs.len(),
                "picked": p.picked.len(), "entries": p.entries, "refused": p.refused, "stops": p.stops, "fees": p.acct.fees})
        }).collect())
    }
}

/// A taker fill of `delta` on the book: the size filled (signed).
fn trade(acct: &mut Account, coin: &str, delta: f64, book: &Book) -> Option<f64> {
    let side = if delta > 0.0 { &book.asks } else { &book.bids };
    let (got, px) = walk(side, delta.abs());
    if got <= 0.0 {
        return None;
    }
    let filled = got * delta.signum();
    acct.fill(coin, filled, px, got * px * TAKER_FEE);
    Some(filled)
}

fn close(p: &mut Portfolio, k: &str, book: &Book) {
    if let Some(l) = p.legs.remove(k) {
        if l.size != 0.0 {
            trade(&mut p.acct, &l.coin, -l.size, book);
        }
    }
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

    fn port<'a>(a: &'a Allocators, n: &str) -> &'a Portfolio {
        a.portfolios.iter().find(|p| p.name == n).unwrap()
    }

    #[test]
    fn modes_rank_on_their_own() {
        // b: the better expectancy / drawdown; a: the bigger PnL.
        let s = [stats("a", 100, 5.0, 10.0), stats("b", 30, 5.0, 1.0), stats("c", 10, 50.0, 1.0)];
        assert_eq!(s[2].ineligible(), Some("too few closed trades"));
        let mut a = Allocators::new(None);
        assert_eq!(a.portfolios.len(), MODES.len());
        a.rerank(&s, 0.0);
        assert_eq!(port(&a, "EXPDD_10").picked["b"].0, 1);
        assert_eq!(port(&a, "PNL_10").picked["a"].0, 1);
        assert!(!port(&a, "PNL_10").picked.contains_key("c"));
        assert_eq!(port(&a, "RANDOM_10").picked.len(), 2);
        // Kept across a restart by name.
        let b = Allocators::new(Some(a.clone()));
        assert_eq!(port(&b, "PNL_10").picked.len(), 2);
    }

    #[test]
    fn two_positions_a_coin_and_risk_caps() {
        let s: Vec<TraderStats> = (0..6).map(|i| stats(&format!("t{i}"), 30, 5.0, 2.0 + i as f64)).collect();
        let mut a = Allocators::new(None);
        a.rerank(&s, 0.0);
        let b = book(100.0);
        let marks = |_: &str| Some(100.0);
        let sig = |u: &str, c: &str| Signal { user: u.into(), coin: c.into(), dir: 1.0, frac: 1.0, entry: true };
        a.on_signal(&sig("t0", "BTC"), &b, &marks, 0.0);
        a.on_signal(&sig("t1", "BTC"), &b, &marks, 0.0);
        let rows = a.on_signal(&sig("t2", "BTC"), &b, &marks, 0.0);
        let r = rows.iter().find(|r| r["portfolio"] == "EXPDD_10").unwrap();
        assert_eq!(r["reject_reason"], "COIN_POSITIONS_CAP");
        let p = port(&a, "EXPDD_10");
        assert_eq!(p.legs.len(), 2);
        assert!((p.legs["t0|BTC"].full * 100.0 - RISK / STOP * START_USD).abs() < 1.0);
        // Not picked.
        let rows = a.on_signal(&sig("zz", "ETH"), &b, &marks, 0.0);
        assert!(rows.iter().all(|r| r["reject_reason"] == "NOT_PICKED"));
        // The trader halves: the leg too; flat: closed.
        a.on_signal(&Signal { user: "t0".into(), coin: "BTC".into(), dir: 1.0, frac: 0.5, entry: false }, &b, &marks, 1.0);
        let l = &port(&a, "EXPDD_10").legs["t0|BTC"];
        assert!((l.size - l.full / 2.0).abs() < 1e-6);
        a.on_signal(&Signal { user: "t0".into(), coin: "BTC".into(), dir: 1.0, frac: 0.0, entry: false }, &b, &marks, 2.0);
        assert!(!port(&a, "EXPDD_10").legs.contains_key("t0|BTC"));
        // Per trader: 20% = ten 2% entries, the eleventh refused.
        let mut a = Allocators::new(None);
        a.rerank(&s, 0.0);
        for c in ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"] {
            a.on_signal(&sig("t0", c), &b, &marks, 0.0);
        }
        let rows = a.on_signal(&sig("t0", "K"), &b, &marks, 0.0);
        assert_eq!(rows.iter().find(|r| r["portfolio"] == "EXPDD_10").unwrap()["reject_reason"], "TRADER_RISK_CAP");
    }

    #[test]
    fn stop_closes_a_leg() {
        let s = [stats("t0", 30, 5.0, 2.0)];
        let mut a = Allocators::new(None);
        a.rerank(&s, 0.0);
        let marks = |_: &str| Some(100.0);
        a.on_signal(&Signal { user: "t0".into(), coin: "BTC".into(), dir: 1.0, frac: 1.0, entry: true }, &book(100.0), &marks, 0.0);
        let books: HashMap<String, Book> = [("BTC".to_string(), book(79.0))].into();
        a.check_stops(&books);
        let p = port(&a, "EXPDD_10");
        assert!(p.legs.is_empty() && p.stops == 1);
        assert!(p.acct.equity(&|_| Some(79.0)) < START_USD);
    }
}
