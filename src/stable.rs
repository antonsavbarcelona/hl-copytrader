//! Who is followed, and how an account's steadiness is measured.
//!
//! Round 1 (`round1`, from the leaderboard at every read of it): the large accounts that trade:
//! `ROUND1_EQUITY`+ (spot and vaults counted) and `MIN_TURNOVER`..`MAX_TURNOVER` times it traded
//! in the last month (not idle; not a pure market maker). No more than that: whether a copy of
//! it makes money is round 2's question (`engine`, a week on paper each).
//!
//! Steadiness (`steadiness`), measured when a week of round 2 ends with our copy at a profit, to
//! tell a steady trader from a lucky week: rule B on its perp PnL history (`portfolio`) over the
//! last 90 days: a profit in each of the 3 months (30 days each), a loss in at most
//! `MAX_WEEKS_DOWN` of the last 12 weeks (a week without a trade is neither), its perp account
//! still `STEADY_EQUITY`+. Kept with it, for the analysis: how it trades, from its latest fills
//! (`Style`: orders a day, resting share, liquid share, edge).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{Api, Leader, exchange_now, num};


/// Round 1: accounts of this much (USD) and more...
pub const ROUND1_EQUITY: f64 = 250_000.0;
/// ... that traded this many times it in the last month.
const MIN_TURNOVER: f64 = 0.5;
const MAX_TURNOVER: f64 = 200.0;
const MONTHS: u64 = 3;
const WEEKS: u64 = 12;
const MAX_WEEKS_DOWN: u32 = 4;
const STEADY_EQUITY: f64 = 100_000.0;
const MAX_ORDERS_PER_DAY: f64 = 300.0;
const MAKER_PCT: f64 = 90.0;
const MIN_EDGE_BP: f64 = 20.0;
const MIN_LIQUID_PCT: f64 = 70.0;
pub const LIQUID_VOLUME: f64 = 5_000_000.0;
/// A period's capital under this (USD) does not count: the account was funded later.
const MIN_BASE: f64 = 1000.0;
const DAY_MS: u64 = 86_400_000;

/// Round 1: the leaderboard's large accounts that trade, largest first.
pub fn round1(leaders: &[Leader]) -> Selection {
    let mut picks: Vec<Pick> = leaders.iter().filter(|l| {
        let turnover = if l.account_value > 0.0 { l.month_volume / l.account_value } else { 0.0 };
        l.account_value >= ROUND1_EQUITY && (MIN_TURNOVER..=MAX_TURNOVER).contains(&turnover)
    }).map(|l| Pick {
        address: l.address.clone(), name: l.name.clone(), equity: l.account_value, months: Vec::new(), pnl: l.month_pnl,
        weeks_up: 0, weeks_down: 0, style: None,
    }).collect();
    picks.sort_by(|a, b| b.equity.total_cmp(&a.equity));
    Selection { at: crate::api::now(), read: leaders.len(), picks }
}

/// (unix ms, USD) points.
type Series = Vec<(u64, f64)>;

/// An account's perp PnL (cumulative) and account value over time.
#[derive(Clone, Debug, Default)]
pub struct History {
    pub pnl: Series,
    pub value: Series,
}

impl History {
    /// From a `portfolio` reply: the whole life (`perpAllTime`, a point every few days), its last
    /// 30 days replaced by the finer `perpMonth` (its PnL put on the all-time level).
    pub fn from_portfolio(v: &Value) -> Option<Self> {
        let window = |name: &str| -> Option<(Series, Series)> {
            let w = v.as_array()?.iter().find(|x| x[0] == name)?;
            let series = |k: &str| -> Series {
                w[1][k].as_array().into_iter().flatten().filter_map(|p| Some((p[0].as_u64()?, num(&p[1])))).collect()
            };
            Some((series("pnlHistory"), series("accountValueHistory")))
        };
        let (mut pnl, mut value) = window("perpAllTime")?;
        if pnl.len() < 2 || value.len() < 2 {
            return None;
        }
        if let Some((mp, mv)) = window("perpMonth").filter(|(p, v)| p.len() > 2 && v.len() > 2) {
            let t0 = mp[0].0;
            if let Some(base) = at(&pnl, t0) {
                pnl.retain(|x| x.0 < t0);
                pnl.extend(mp.iter().map(|&(t, x)| (t, base + x)));
                value.retain(|x| x.0 < t0);
                value.extend(mv);
            }
        }
        Some(Self { pnl, value })
    }
}

/// The value at `t`, linear between the points; None outside them.
fn at(s: &[(u64, f64)], t: u64) -> Option<f64> {
    let (first, last) = (s.first()?, s.last()?);
    if t < first.0 || t > last.0 {
        return None;
    }
    let i = s.partition_point(|x| x.0 < t);
    if s[i].0 == t {
        return Some(s[i].1);
    }
    let ((t0, v0), (t1, v1)) = (s[i - 1], s[i]);
    Some(v0 + (v1 - v0) * (t - t0) as f64 / (t1 - t0) as f64)
}

/// A pick: the account and how it did.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pick {
    pub address: String,
    pub name: Option<String>,
    /// Perp account value now.
    pub equity: f64,
    /// PnL of each month, oldest first, and over the 3.
    pub months: Vec<f64>,
    pub pnl: f64,
    /// Weeks of the last 12 at a profit, and at a loss.
    pub weeks_up: u32,
    #[serde(default)]
    pub weeks_down: u32,
    #[serde(default)]
    pub style: Option<Style>,
}

/// How an account trades, from its latest fills.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Style {
    /// Orders (distinct order ids) a day over the fills' span.
    pub orders_per_day: f64,
    /// Share of its volume (%) from resting orders (maker), and in liquid perps.
    pub maker_pct: f64,
    pub liquid_pct: f64,
    /// Closed PnL less fees over the fills, bp of their volume.
    pub edge_bp: f64,
}

impl Style {
    /// From `userFills` (spot fills left out); None without perp fills.
    pub fn from_fills(fills: &[Value], liquid: &HashSet<String>) -> Option<Self> {
        let perp: Vec<&Value> = fills.iter().filter(|f| !f["coin"].as_str().unwrap_or("@").starts_with('@')).collect();
        let times: Vec<u64> = perp.iter().filter_map(|f| f["time"].as_u64()).collect();
        let (first, last) = (*times.iter().min()?, *times.iter().max()?);
        let days = ((last - first) as f64 / 86_400_000.0).max(1.0 / 24.0);
        let ntl = |f: &&Value| num(&f["px"]) * num(&f["sz"]);
        let vol: f64 = perp.iter().map(ntl).sum();
        if vol <= 0.0 {
            return None;
        }
        let orders: HashSet<u64> = perp.iter().filter_map(|f| f["oid"].as_u64()).collect();
        let maker: f64 = perp.iter().filter(|f| f["crossed"] == false).map(ntl).sum();
        let liq: f64 = perp.iter().filter(|f| f["coin"].as_str().is_some_and(|c| liquid.contains(c))).map(ntl).sum();
        let edge: f64 = perp.iter().map(|f| num(&f["closedPnl"]) - num(&f["fee"])).sum();
        Some(Self { orders_per_day: orders.len() as f64 / days, maker_pct: maker / vol * 100.0, liquid_pct: liq / vol * 100.0, edge_bp: edge / vol * 1e4 })
    }

    /// Why a copy could not follow it (None: it can).
    pub fn not_copyable(&self) -> Option<&'static str> {
        if self.orders_per_day > MAX_ORDERS_PER_DAY {
            Some("high frequency")
        } else if self.maker_pct >= MAKER_PCT && self.edge_bp < MIN_EDGE_BP {
            Some("market maker")
        } else if self.liquid_pct < MIN_LIQUID_PCT {
            Some("illiquid markets")
        } else {
            None
        }
    }
}

/// The PnL from `a` to `b` and the capital it was made on: the value at `a`, or what was in by
/// `b` without that PnL if more came in (deposits).
fn period(h: &History, a: u64, b: u64) -> Option<(f64, f64)> {
    let p = at(&h.pnl, b)? - at(&h.pnl, a)?;
    Some((p, at(&h.value, a)?.max(at(&h.value, b)? - p)))
}

/// Its PnL history measured as of `now_ms`: perp account value, each month's PnL (oldest
/// first), weeks up and down of the last 12. None if the history does not cover 90 days, has
/// stopped, or a week had under `MIN_BASE` in.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Measure {
    pub equity: f64,
    pub months: Vec<f64>,
    pub weeks_up: u32,
    pub weeks_down: u32,
}

pub fn measure(h: &History, now_ms: u64) -> Option<Measure> {
    let last = h.pnl.last()?.0.min(h.value.last()?.0);
    if last + 2 * DAY_MS < now_ms {
        return None;
    }
    // "Now" is the account's latest point (a reply a little older than our clock).
    let now = now_ms.min(last);
    if h.pnl.first()?.0 > now.checked_sub(MONTHS * 30 * DAY_MS)? {
        return None;
    }
    let equity = at(&h.value, now)?;
    let mut months = Vec::new();
    for k in (1..=MONTHS).rev() {
        months.push(period(h, now - k * 30 * DAY_MS, now - (k - 1) * 30 * DAY_MS)?.0);
    }
    let (mut weeks_up, mut weeks_down) = (0, 0);
    for k in (1..=WEEKS).rev() {
        let (p, base) = period(h, now - k * 7 * DAY_MS, now - (k - 1) * 7 * DAY_MS)?;
        if base < MIN_BASE {
            return None;
        }
        weeks_up += u32::from(p > 0.0);
        weeks_down += u32::from(p < 0.0);
    }
    Some(Measure { equity, months, weeks_up, weeks_down })
}

impl Measure {
    /// Rule B: steady, not a lucky stretch.
    pub fn steady(&self) -> bool {
        self.equity >= STEADY_EQUITY && self.months.iter().all(|&m| m > 0.0) && self.weeks_down <= MAX_WEEKS_DOWN
    }
}

/// The picks of one day, kept so a restart does not work them out again.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Selection {
    /// Unix seconds.
    pub at: f64,
    /// Candidates read.
    pub read: usize,
    pub picks: Vec<Pick>,
}

impl Selection {
    pub fn addresses(&self) -> HashSet<String> {
        self.picks.iter().map(|p| p.address.clone()).collect()
    }
}

/// An account's steadiness now (see the module): rule B on its history (`measure` None: the
/// history does not cover it, so not steady), and how it trades.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Steadiness {
    pub steady: bool,
    pub measure: Option<Measure>,
    pub style: Option<Style>,
}

pub async fn steadiness(api: &Api, address: &str) -> anyhow::Result<Steadiness> {
    let m = measure(&api.portfolio(address).await?, (exchange_now() * 1000.0) as u64);
    let liquid: HashSet<String> = api.meta().await?.1.into_iter().filter(|(_, c)| c.day_volume >= LIQUID_VOLUME).map(|(k, _)| k).collect();
    let style = Style::from_fills(&api.fills(address).await?, &liquid);
    Ok(Steadiness { steady: m.as_ref().is_some_and(Measure::steady), measure: m, style })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 120 days of history, a point a day: PnL growing by `daily(d)` on day d, account value
    /// `value` throughout.
    fn history(daily: impl Fn(u64) -> f64, value: f64) -> History {
        let mut pnl = vec![(0, 0.0)];
        for d in 1..=120 {
            pnl.push((d * DAY_MS, pnl.last().unwrap().1 + daily(d)));
        }
        History { value: pnl.iter().map(|x| (x.0, value)).collect(), pnl }
    }

    #[test]
    fn steady_profit_is_picked() {
        let now = 120 * DAY_MS;
        let p = measure(&history(|_| 100.0, 200_000.0), now).filter(Measure::steady).unwrap();
        assert_eq!(p.weeks_up, 12);
        assert_eq!(p.months.len(), 3);
        assert!((p.months.iter().sum::<f64>() - 9000.0).abs() < 1e-6 && p.months.iter().all(|m| (m - 3000.0).abs() < 1e-6));
        // Too small an account.
        assert!(measure(&history(|_| 100.0, 50_000.0), now).filter(Measure::steady).is_none());
        // Not 90 days of history.
        let mut h = history(|_| 100.0, 200_000.0);
        h.pnl.retain(|x| x.0 >= 40 * DAY_MS);
        assert!(measure(&h, now).filter(Measure::steady).is_none());
        // A history that stopped a week ago.
        assert!(measure(&history(|_| 100.0, 200_000.0), now + 7 * DAY_MS).filter(Measure::steady).is_none());
    }

    #[test]
    fn a_losing_month_or_too_few_good_weeks() {
        let now = 120 * DAY_MS;
        // The middle month (days 60-90) at a loss.
        assert!(measure(&history(|d| if (61..=90).contains(&d) { -50.0 } else { 100.0 }, 2e5), now).filter(Measure::steady).is_none());
        // Every month up, but only every other week (counted back from now, day 120): 6 of 12.
        let h = history(|d| if ((120 - d) / 7) % 2 == 0 { 300.0 } else { -100.0 }, 2e5);
        let weeks: u32 = (1..=12).map(|k| u32::from(period(&h, now - k * 7 * DAY_MS, now - (k - 1) * 7 * DAY_MS).unwrap().0 > 0.0)).sum();
        assert_eq!(weeks, 6);
        let months: Vec<f64> = (1..=3).map(|k| period(&h, now - k * 30 * DAY_MS, now - (k - 1) * 30 * DAY_MS).unwrap().0).collect();
        assert!(months.iter().all(|&m| m > 0.0));
        assert!(measure(&h, now).filter(Measure::steady).is_none());
    }

    #[test]
    fn idle_weeks_are_no_loss() {
        // Three weeks without a trade (counted back from day 120: days 79-99).
        let h = history(|d| if (79..=99).contains(&d) { 0.0 } else { 100.0 }, 2e5);
        let p = measure(&h, 120 * DAY_MS).filter(Measure::steady).unwrap();
        assert_eq!((p.weeks_up, p.weeks_down), (9, 0));
    }

    #[test]
    fn deposits_count_as_capital() {
        // $500 in until day 100, then $200k deposited: early weeks had under $1000 of capital.
        let mut h = history(|_| 1.0, 500.0);
        for x in h.value.iter_mut().filter(|x| x.0 >= 100 * DAY_MS) {
            x.1 = 200_000.0;
        }
        assert!(measure(&h, 120 * DAY_MS).filter(Measure::steady).is_none());
    }

    #[test]
    fn portfolio_month_spliced_on_all_time() {
        let v = json!([
            ["perpAllTime", {"pnlHistory": [[0, "0"], [50, "100"], [100, "200"]], "accountValueHistory": [[0, "1"], [50, "1"], [100, "1"]]}],
            ["perpMonth", {"pnlHistory": [[60, "0"], [80, "30"], [110, "90"]], "accountValueHistory": [[60, "2"], [80, "2"], [110, "3"]]}],
        ]);
        let h = History::from_portfolio(&v).unwrap();
        // All-time at 60 is 120: the month's points sit on it.
        assert_eq!(h.pnl, vec![(0, 0.0), (50, 100.0), (60, 120.0), (80, 150.0), (110, 210.0)]);
        assert_eq!(h.value, vec![(0, 1.0), (50, 1.0), (60, 2.0), (80, 2.0), (110, 3.0)]);
    }

    #[test]
    fn round1_by_leaderboard() {
        let l = |av: f64, vlm: f64| Leader {
            address: format!("{av}"), account_value: av, month_volume: vlm, month_pnl: -1.0, week_pnl: 0.0, name: None,
        };
        // Large and trading (at a loss too): in; small, idle, or churning 300x: out.
        let ls = [l(3e5, 1e6), l(1e6, 6e5), l(2e5, 1e6), l(3e5, 1e4), l(3e5, 1e8)];
        let s = round1(&ls);
        assert_eq!(s.picks.iter().map(|p| p.address.as_str()).collect::<Vec<_>>(), vec!["1000000", "300000"]);
        assert_eq!(s.read, 5);
    }

    #[test]
    fn steady_by_rule_b() {
        let now = 120 * DAY_MS;
        let m = measure(&history(|_| 100.0, 200_000.0), now).unwrap();
        assert!(m.steady() && m.months.len() == 3 && m.weeks_up == 12);
        // A losing month, or too small an account: measured, not steady.
        let m = measure(&history(|d| if (61..=90).contains(&d) { -50.0 } else { 100.0 }, 2e5), now).unwrap();
        assert!(!m.steady() && m.months[1] < 0.0);
        assert!(!measure(&history(|_| 100.0, 50_000.0), now).unwrap().steady());
    }
}
