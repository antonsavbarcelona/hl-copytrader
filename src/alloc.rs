//! A second layer over the paper copies, apart from them: allocator portfolios that get the same
//! signals (each followed account's position moves, `Signal`) but decide whom to let in, with
//! one shared paper account each and portfolio risk limits. The copies per account go on as
//! they were; this answers whether choosing among them adds anything.
//!
//!   - Each account's copy has its statistics (`TraderStats`, from its paper copy). Eligible:
//!     `MIN_TRADES`+ closed trips, PnL above 0, profit factor above `MIN_PF`, drawdown under
//!     `MAX_DD_PCT`, slippage under `MAX_SLIP_BP`. Ranked by expectancy / drawdown.
//!   - Re-ranked once a day (`RANK_EVERY_S`): each portfolio picks its traders and their risk:
//!     EQUAL the top `TOP_N`, `RISK` each; RANK_WEIGHTED the top `TOP_N`, from `RISK` down to
//!     `RISK_MIN` by rank; DIVERSIFIED the top ones but none trading much the same coins as one
//!     picked already (`MAX_SIMILAR`), and no two of its traders the same way in a coin;
//!     RANDOM_ELIGIBLE_10 `TOP_N` of the eligible at random, as the benchmark of the ranking.
//!   - A picked trader's entry is entered at its risk (the stop `STOP` away loses that much of
//!     the portfolio's equity), less as the portfolio's caps require (total, per coin, per
//!     trader, per direction); under `MIN_GRANT` it is not. Each entry's decision is a row of
//!     `allocator_decisions`, with why when it was cut or refused.
//!   - A position is followed to its end (reduced in proportion as the trader reduces from its
//!     peak, closed when it is flat), or closed at the stop: a trader dropping off the list
//!     stops new entries only.
//!
//! Fills are taker fills on mainnet's book, as the paper copies'.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::account::{Account, walk};
use crate::config::TAKER_FEE;
use crate::ws::Book;

pub const START_USD: f64 = 10_000.0;
pub const RANK_EVERY_S: f64 = 86_400.0;
const TOP_N: usize = 10;
const RISK: f64 = 0.02;
const RISK_MIN: f64 = 0.005;
const STOP: f64 = 0.20;
const MIN_GRANT: f64 = 0.0025;
const CAP_TOTAL: f64 = 0.10;
const CAP_COIN: f64 = 0.04;
const CAP_TRADER: f64 = 0.02;
const CAP_DIRECTION: f64 = 0.06;
const MIN_TRADES: u64 = 20;
const MIN_PF: f64 = 1.1;
const MAX_DD_PCT: f64 = 15.0;
const MAX_SLIP_BP: f64 = 30.0;
const MAX_SIMILAR: f64 = 0.5;
const MIN_ORDER_USD: f64 = 10.0;

/// A copy account's statistics, from its paper copy.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TraderStats {
    pub trader: String,
    pub closed_trades: u64,
    pub pnl_net: f64,
    pub return_pct: f64,
    /// Average closed trip's PnL ($), wins over losses ($), win share.
    pub expectancy: f64,
    pub profit_factor: f64,
    pub win_rate: f64,
    pub max_drawdown_pct: f64,
    pub avg_holding_h: f64,
    pub avg_slippage_bps: f64,
    pub pnl_7d: Option<f64>,
    pub pnl_30d: Option<f64>,
    /// Its copy's open positions' risk to the stop, % of its equity.
    pub current_open_risk: f64,
    /// Coins its copy entered, and how often.
    pub coins: BTreeMap<String, u32>,
}

impl TraderStats {
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

    pub fn score(&self) -> f64 {
        self.expectancy / self.max_drawdown_pct.max(1.0)
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
    /// Its traders now and the risk each gets.
    pub picked: BTreeMap<String, f64>,
    pub entries: u64,
    pub refused: u64,
    pub stops: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Allocators {
    pub portfolios: Vec<Portfolio>,
    /// Eligible traders: rank (1 = best) and score, as of `ranked_at`.
    pub ranks: HashMap<String, (usize, f64)>,
    pub ranked_at: f64,
}

fn key(user: &str, coin: &str) -> String {
    format!("{user}|{coin}")
}

fn jaccard(a: &BTreeMap<String, u32>, b: &BTreeMap<String, u32>) -> f64 {
    let (sa, sb): (HashSet<&String>, HashSet<&String>) = (a.keys().collect(), b.keys().collect());
    let u = sa.union(&sb).count();
    if u == 0 { 0.0 } else { sa.intersection(&sb).count() as f64 / u as f64 }
}

impl Allocators {
    pub fn new() -> Self {
        let p = |n: &str| Portfolio { name: n.into(), acct: Account::new(START_USD), ..Default::default() };
        Self { portfolios: vec![p("EQUAL"), p("RANK_WEIGHTED"), p("DIVERSIFIED"), p("RANDOM_ELIGIBLE_10")], ..Default::default() }
    }

    /// Ranks the eligible traders and picks each portfolio's traders and their risk.
    pub fn rerank(&mut self, stats: &[TraderStats], at: f64) -> Value {
        let mut elig: Vec<&TraderStats> = stats.iter().filter(|s| s.ineligible().is_none()).collect();
        elig.sort_by(|a, b| b.score().total_cmp(&a.score()));
        self.ranks = elig.iter().enumerate().map(|(i, s)| (s.trader.clone(), (i + 1, s.score()))).collect();
        self.ranked_at = at;
        let top: Vec<&TraderStats> = elig.iter().take(TOP_N).copied().collect();
        let mut diverse: Vec<&TraderStats> = Vec::new();
        for s in &elig {
            if diverse.len() >= TOP_N {
                break;
            }
            if diverse.iter().all(|d| jaccard(&d.coins, &s.coins) < MAX_SIMILAR) {
                diverse.push(s);
            }
        }
        // Random: seeded by the day, so a restart picks the same ones.
        let mut pool: Vec<&TraderStats> = elig.clone();
        let mut seed = (at / 86_400.0) as u64 ^ 0x9e37_79b9_7f4a_7c15;
        let mut random = Vec::new();
        while !pool.is_empty() && random.len() < TOP_N {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            random.push(pool.swap_remove((seed % pool.len() as u64) as usize));
        }
        for p in self.portfolios.iter_mut() {
            p.picked = match p.name.as_str() {
                "EQUAL" => top.iter().map(|s| (s.trader.clone(), RISK)).collect(),
                "RANK_WEIGHTED" => top.iter().enumerate()
                    .map(|(i, s)| (s.trader.clone(), RISK - (RISK - RISK_MIN) * i as f64 / (TOP_N - 1) as f64)).collect(),
                "DIVERSIFIED" => diverse.iter().map(|s| (s.trader.clone(), RISK)).collect(),
                _ => random.iter().map(|s| (s.trader.clone(), RISK)).collect(),
            };
        }
        json!({"eligible": elig.len(), "of": stats.len(), "top": top.iter().map(|s| json!({"trader": s.trader, "score": s.score(),
            "closed_trades": s.closed_trades, "pnl_net": s.pnl_net, "pf": s.profit_factor, "dd_pct": s.max_drawdown_pct})).collect::<Vec<_>>()})
    }

    /// A signal: each portfolio follows its legs and decides on entries. Rows for
    /// `allocator_decisions` (entries only).
    pub fn on_signal(&mut self, s: &Signal, book: &Book, marks: &dyn Fn(&str) -> Option<f64>, at: f64) -> Vec<Value> {
        let mut rows = Vec::new();
        let rank = self.ranks.get(&s.user).copied();
        for p in self.portfolios.iter_mut() {
            let k = key(&s.user, &s.coin);
            if let Some(l) = p.legs.get(&k).cloned() {
                if s.frac <= 0.0 || s.dir != l.dir {
                    close(p, &k, book);
                } else {
                    let target = l.dir * l.full * s.frac.min(1.0);
                    let delta = target - l.size;
                    let mid = book.mid().unwrap_or(l.entry_px);
                    if (delta.abs() * mid >= MIN_ORDER_USD || target == 0.0) && delta.abs() > 0.0 {
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
            let wanted = p.picked.get(&s.user).copied();
            let mut row = json!({"portfolio": p.name, "trader": s.user, "coin": s.coin, "side": if s.dir > 0.0 { "long" } else { "short" },
                "action": "open", "eligible": rank.is_some(), "rank": rank.map(|r| r.0), "score": rank.map(|r| r.1),
                "requested_risk": wanted.unwrap_or(0.0), "granted_risk": 0.0, "reject_reason": null, "at": crate::liq::iso(at)});
            let Some(want) = wanted else {
                row["reject_reason"] = json!(if rank.is_some() { "NOT_PICKED" } else { "NOT_ELIGIBLE" });
                rows.push(row);
                continue;
            };
            let sum = |f: &dyn Fn(&Leg) -> bool| p.legs.values().filter(|l| f(l)).map(Leg::risk_now).sum::<f64>();
            let rooms = [
                (CAP_TOTAL - sum(&|_| true), "TOTAL_RISK_CAP"),
                (CAP_COIN - sum(&|l| l.coin == s.coin), "ASSET_RISK_CAP"),
                (CAP_TRADER - sum(&|l| l.user == s.user), "TRADER_RISK_CAP"),
                (CAP_DIRECTION - sum(&|l| l.dir == s.dir), "DIRECTION_RISK_CAP"),
            ];
            let (room, binding) = rooms.iter().fold((want, None), |(g, why), &(r, n)| if r < g { (r.max(0.0), Some(n)) } else { (g, why) });
            let duplicate = p.name == "DIVERSIFIED" && p.legs.values().any(|l| l.coin == s.coin && l.dir == s.dir && l.user != s.user);
            if duplicate || room < MIN_GRANT {
                row["reject_reason"] = json!(if duplicate { "DUPLICATE_EXPOSURE" } else { binding.unwrap_or("TOTAL_RISK_CAP") });
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
            if room < want {
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

    fn stats(t: &str, trades: u64, exp: f64, dd: f64, coins: &[&str]) -> TraderStats {
        TraderStats { trader: t.into(), closed_trades: trades, pnl_net: exp * trades as f64, expectancy: exp, profit_factor: 2.0,
            max_drawdown_pct: dd, avg_slippage_bps: 5.0, coins: coins.iter().map(|c| (c.to_string(), 1)).collect(), ..Default::default() }
    }

    #[test]
    fn eligibility_and_rank() {
        let s = [stats("a", 30, 5.0, 5.0, &["BTC"]), stats("b", 30, 5.0, 2.0, &["BTC"]), stats("c", 10, 50.0, 1.0, &["ETH"])];
        assert_eq!(s[2].ineligible(), Some("too few closed trades"));
        let mut a = Allocators::new();
        a.rerank(&s, 0.0);
        assert_eq!(a.ranks["b"].0, 1);
        assert_eq!(a.ranks["a"].0, 2);
        assert!(!a.ranks.contains_key("c"));
        // DIVERSIFIED: a trades the same coins as b, picked first.
        let d = a.portfolios.iter().find(|p| p.name == "DIVERSIFIED").unwrap();
        assert_eq!(d.picked.keys().collect::<Vec<_>>(), vec!["b"]);
        let w = a.portfolios.iter().find(|p| p.name == "RANK_WEIGHTED").unwrap();
        assert!((w.picked["b"] - RISK).abs() < 1e-12 && w.picked["a"] < RISK);
    }

    #[test]
    fn caps_cut_and_refuse() {
        let s: Vec<TraderStats> = (0..6).map(|i| stats(&format!("t{i}"), 30, 5.0, 2.0 + i as f64, &[&format!("C{i}")])).collect();
        let mut a = Allocators::new();
        a.rerank(&s, 0.0);
        let b = book(100.0);
        let marks = |_: &str| Some(100.0);
        let sig = |u: &str, c: &str| Signal { user: u.into(), coin: c.into(), dir: 1.0, frac: 1.0, entry: true };
        // Two traders long BTC take 4%: the coin cap; a third is refused.
        a.on_signal(&sig("t0", "BTC"), &b, &marks, 0.0);
        a.on_signal(&sig("t1", "BTC"), &b, &marks, 0.0);
        let rows = a.on_signal(&sig("t2", "BTC"), &b, &marks, 0.0);
        let eq = rows.iter().find(|r| r["portfolio"] == "EQUAL").unwrap();
        assert_eq!(eq["reject_reason"], "ASSET_RISK_CAP");
        assert_eq!(eq["granted_risk"], 0.0);
        // DIVERSIFIED refused the second BTC long already.
        let p = a.portfolios.iter().find(|p| p.name == "EQUAL").unwrap();
        assert_eq!(p.legs.len(), 2);
        let l = &p.legs["t0|BTC"];
        assert!((l.full * 100.0 - RISK / STOP * START_USD).abs() < 1.0);
        // Not ranked: not eligible.
        let rows = a.on_signal(&sig("zz", "ETH"), &b, &marks, 0.0);
        assert!(rows.iter().all(|r| r["reject_reason"] == "NOT_ELIGIBLE"));
        // The trader halves: its leg too; flat: closed.
        a.on_signal(&Signal { user: "t0".into(), coin: "BTC".into(), dir: 1.0, frac: 0.5, entry: false }, &b, &marks, 1.0);
        let p = a.portfolios.iter().find(|p| p.name == "EQUAL").unwrap();
        assert!((p.legs["t0|BTC"].size - p.legs["t0|BTC"].full / 2.0).abs() < 1e-6);
        a.on_signal(&Signal { user: "t0".into(), coin: "BTC".into(), dir: 1.0, frac: 0.0, entry: false }, &b, &marks, 2.0);
        let p = a.portfolios.iter().find(|p| p.name == "EQUAL").unwrap();
        assert!(!p.legs.contains_key("t0|BTC"));
    }

    #[test]
    fn stop_closes_a_leg() {
        let s = [stats("t0", 30, 5.0, 2.0, &["BTC"])];
        let mut a = Allocators::new();
        a.rerank(&s, 0.0);
        let marks = |_: &str| Some(100.0);
        a.on_signal(&Signal { user: "t0".into(), coin: "BTC".into(), dir: 1.0, frac: 1.0, entry: true }, &book(100.0), &marks, 0.0);
        let books: HashMap<String, Book> = [("BTC".to_string(), book(79.0))].into();
        a.check_stops(&books);
        let p = a.portfolios.iter().find(|p| p.name == "EQUAL").unwrap();
        assert!(p.legs.is_empty() && p.stops == 1);
        assert!(p.acct.equity(&|_| Some(79.0)) < START_USD);
    }
}
