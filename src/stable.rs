//! Who is followed: traders who make money steadily, picked once a day.
//!
//! Candidates come from the leaderboard: an account of `MIN_EQUITY`+ (spot and vaults counted),
//! at a profit this month and over all time, trading `MIN_TURNOVER`..`MAX_TURNOVER` times its
//! equity a month (not idle; not a market maker, whose edge is the spread and rebates a copy
//! cannot get). Each one's perp PnL history (`portfolio`) then has to show, over the last 90
//! days:
//!   - a profit in each of the 3 months (30 days each), and
//!   - a profit in at least `MIN_WEEKS_UP` of the last 12 weeks,
//!
//! with its perp account still `MIN_EQUITY`+. On the history (picked this way in July, August,
//! September; the next month): 69%, 50%, 68% of the picks at a profit, against 54%, 57%, 51%
//! of all accounts; no drawdown limit, so high-leverage accounts are in too (see the README).

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::{Api, Leader, exchange_now, num};
use crate::log;

pub const MIN_EQUITY: f64 = 100_000.0;
const MIN_TURNOVER: f64 = 0.5;
const MAX_TURNOVER: f64 = 200.0;
const MONTHS: u64 = 3;
const WEEKS: u64 = 12;
const MIN_WEEKS_UP: f64 = 0.6;
/// A period's capital under this (USD) does not count: the account was funded later.
const MIN_BASE: f64 = 1000.0;
/// The list is worked out again this often...
pub const EVERY_S: f64 = 24.0 * 3600.0;
/// ... one history read at a time with this pause between (s), so the copies' own reads keep
/// most of the API budget (thousands of candidates: a few hours). None when there is no list
/// yet: nothing is copied, so nothing to leave the budget to.
pub const PAUSE_S: f64 = 1.5;

const DAY_MS: u64 = 86_400_000;

/// The leaderboard accounts worth reading the history of.
pub fn candidates(leaders: &[Leader]) -> Vec<&Leader> {
    leaders.iter().filter(|l| {
        let turnover = if l.account_value > 0.0 { l.month_volume / l.account_value } else { 0.0 };
        l.account_value >= MIN_EQUITY && l.month_pnl > 0.0 && l.all_pnl > 0.0 && (MIN_TURNOVER..=MAX_TURNOVER).contains(&turnover)
    }).collect()
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
    /// Weeks of the last 12 at a profit.
    pub weeks_up: u32,
}

/// The PnL from `a` to `b` and the capital it was made on: the value at `a`, or what was in by
/// `b` without that PnL if more came in (deposits).
fn period(h: &History, a: u64, b: u64) -> Option<(f64, f64)> {
    let p = at(&h.pnl, b)? - at(&h.pnl, a)?;
    Some((p, at(&h.value, a)?.max(at(&h.value, b)? - p)))
}

/// `h` as of `now_ms` by the rule (see the module): its pick, or None.
pub fn pick(address: &str, name: Option<String>, h: &History, now_ms: u64) -> Option<Pick> {
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
    if equity < MIN_EQUITY {
        return None;
    }
    let mut months = Vec::new();
    for k in (1..=MONTHS).rev() {
        let (p, _) = period(h, now - k * 30 * DAY_MS, now - (k - 1) * 30 * DAY_MS)?;
        if p <= 0.0 {
            return None;
        }
        months.push(p);
    }
    let mut weeks_up = 0;
    for k in (1..=WEEKS).rev() {
        let (p, base) = period(h, now - k * 7 * DAY_MS, now - (k - 1) * 7 * DAY_MS)?;
        if base < MIN_BASE {
            return None;
        }
        weeks_up += u32::from(p > 0.0);
    }
    if (weeks_up as f64) < MIN_WEEKS_UP * WEEKS as f64 {
        return None;
    }
    Some(Pick { address: address.to_string(), name, equity, pnl: months.iter().sum(), months, weeks_up })
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

/// Reads every candidate's history, `pause_s` apart, and picks by the rule.
pub async fn select(api: &Api, leaders: &[Leader], pause_s: f64) -> Selection {
    let cands = candidates(leaders);
    log!("selection: reading {} candidates' histories", cands.len());
    let (mut picks, mut read) = (Vec::new(), 0);
    for (i, l) in cands.iter().enumerate() {
        match api.portfolio(&l.address).await {
            Ok(h) => {
                read += 1;
                picks.extend(pick(&l.address, l.name.clone(), &h, (exchange_now() * 1000.0) as u64));
            }
            Err(e) => log!("selection: {}: {e}", &l.address[..10]),
        }
        if (i + 1) % 500 == 0 {
            log!("selection: {} of {} read, {} picked so far", i + 1, cands.len(), picks.len());
        }
        tokio::time::sleep(std::time::Duration::from_secs_f64(pause_s)).await;
    }
    picks.sort_by(|a, b| b.pnl.total_cmp(&a.pnl));
    log!("selection: {} of {read} picked", picks.len());
    Selection { at: crate::api::now(), read, picks }
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
        let p = pick("a", None, &history(|_| 100.0, 200_000.0), now).unwrap();
        assert_eq!(p.weeks_up, 12);
        assert_eq!(p.months.len(), 3);
        assert!((p.pnl - 9000.0).abs() < 1e-6 && p.months.iter().all(|m| (m - 3000.0).abs() < 1e-6));
        // Too small an account.
        assert!(pick("a", None, &history(|_| 100.0, 50_000.0), now).is_none());
        // Not 90 days of history.
        let mut h = history(|_| 100.0, 200_000.0);
        h.pnl.retain(|x| x.0 >= 40 * DAY_MS);
        assert!(pick("a", None, &h, now).is_none());
        // A history that stopped a week ago.
        assert!(pick("a", None, &history(|_| 100.0, 200_000.0), now + 7 * DAY_MS).is_none());
    }

    #[test]
    fn a_losing_month_or_too_few_good_weeks() {
        let now = 120 * DAY_MS;
        // The middle month (days 60-90) at a loss.
        assert!(pick("a", None, &history(|d| if (61..=90).contains(&d) { -50.0 } else { 100.0 }, 2e5), now).is_none());
        // Every month up, but only every other week (counted back from now, day 120): 6 of 12.
        let h = history(|d| if ((120 - d) / 7) % 2 == 0 { 300.0 } else { -100.0 }, 2e5);
        let weeks: u32 = (1..=12).map(|k| u32::from(period(&h, now - k * 7 * DAY_MS, now - (k - 1) * 7 * DAY_MS).unwrap().0 > 0.0)).sum();
        assert_eq!(weeks, 6);
        let months: Vec<f64> = (1..=3).map(|k| period(&h, now - k * 30 * DAY_MS, now - (k - 1) * 30 * DAY_MS).unwrap().0).collect();
        assert!(months.iter().all(|&m| m > 0.0));
        assert!(pick("a", None, &h, now).is_none());
    }

    #[test]
    fn deposits_count_as_capital() {
        // $500 in until day 100, then $200k deposited: early weeks had under $1000 of capital.
        let mut h = history(|_| 1.0, 500.0);
        for x in h.value.iter_mut().filter(|x| x.0 >= 100 * DAY_MS) {
            x.1 = 200_000.0;
        }
        assert!(pick("a", None, &h, 120 * DAY_MS).is_none());
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
    fn candidates_by_leaderboard() {
        let l = |av: f64, vlm: f64, month: f64, all: f64| Leader {
            address: "x".into(), account_value: av, month_volume: vlm, month_pnl: month, all_pnl: all, name: None,
        };
        let ls = [l(2e5, 1e6, 1.0, 1.0), l(5e4, 1e6, 1.0, 1.0), l(2e5, 1e6, -1.0, 1.0), l(2e5, 1e6, 1.0, -1.0), l(2e5, 1e4, 1.0, 1.0),
                  l(2e5, 1e8, 1.0, 1.0)];
        assert_eq!(candidates(&ls).len(), 1);
    }
}
