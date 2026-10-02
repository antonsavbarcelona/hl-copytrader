//! What each copy account went through: how late and at what price its trades were copied,
//! how big its bets and how deep its losses were relative to our equity, how much it held at
//! once. Kept in `state.json` with the account and summed up by `report`.

use serde::{Deserialize, Serialize};

/// Lag histogram: 0.1 s buckets, the last one holds 10 s and more.
pub const LAG_BUCKET_S: f64 = 0.1;
pub const LAG_BUCKETS: usize = 100;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// Copy fills (that followed one of its trades) with its fill time and price known.
    pub copies: u64,
    /// Its fill (exchange time) -> our fill, s.
    pub lag_sum: f64,
    pub lag_max: f64,
    pub lag_hist: Vec<u32>,
    /// Its fill (exchange time) -> the trade reaching us: the feed's share of the lag, s.
    pub feed_sum: f64,

    /// Our price vs its average price on the trade we copied; + = worse for us (bought
    /// higher / sold lower). Notional of these fills and the cost in USD, split into
    /// `move` (the best price on our side had moved from its price: delay, spread) and
    /// `impact` (our order walked the book past the best price).
    pub slip_notional: f64,
    pub slip_usd: f64,
    pub move_usd: f64,
    pub impact_usd: f64,
    pub worse: u64,
    pub better: u64,

    /// Entries: copy fills that open or add to a position, notional as % of our equity then.
    pub entries: u64,
    pub entry_pct_sum: f64,
    pub entry_pct_max: f64,

    /// Most positions open at once; largest gross notional as % of equity (leverage x 100).
    pub max_open: usize,
    pub max_gross_pct: f64,
    /// Equity marked every tick: its peak and the deepest drop from a peak, %.
    pub peak: f64,
    pub max_dd_pct: f64,

    /// Closed trips (flat -> flat, or up to a flip): count, winners, PnL after fees, hold time.
    pub trips: u64,
    pub wins: u64,
    pub win_usd: f64,
    pub loss_usd: f64,
    pub hold_s: f64,
    /// Per trip, the deepest it went against us (realized + unrealized, after fees), % of
    /// equity at its open: the risk the trip actually took.
    pub risk_pct_sum: f64,
    pub risk_pct_max: f64,
    /// Per trip, its largest notional, % of equity at its open.
    pub size_pct_sum: f64,
    pub size_pct_max: f64,

    /// Trips whose set-up on the trader's side was read (`Plan`): how many had a stop, were
    /// isolated, at what leverage setting, and the planned risk (% of equity).
    pub planned: u64,
    pub with_stop: u64,
    pub isolated: u64,
    pub lev_sum: f64,
    pub lev_max: f64,
    pub plan_pct_sum: f64,
    pub plan_pct_max: f64,
}

/// The trader's set-up for its position, read from the exchange.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Plan {
    pub isolated: bool,
    pub leverage: f64,
    /// Share of the position covered by stop orders (0..1).
    pub stop_cover: f64,
    /// What it loses if its stops are hit, and the rest at its liquidation price (or all of
    /// its notional when it has none), % of its equity: the risk it planned for.
    pub risk_pct: f64,
}

impl Plan {
    /// `size` signed, `liq_px` None = cannot be liquidated, `stops` (trigger px, size or None
    /// for all of the position) on the closing side.
    pub fn new(size: f64, entry: f64, isolated: bool, leverage: f64, liq_px: Option<f64>, stops: &[(f64, Option<f64>)], equity: f64) -> Self {
        let (abs, dir) = (size.abs(), size.signum());
        let loss_at = |px: f64| ((entry - px) * dir).max(0.0);
        // Nearest stops first: they are the ones that fire.
        let mut stops = stops.to_vec();
        stops.sort_by(|a, b| ((b.0 - a.0) * dir).total_cmp(&0.0));
        let (mut covered, mut loss) = (0.0, 0.0);
        for (px, sz) in stops {
            let take = sz.unwrap_or(abs).min(abs - covered);
            if take <= 0.0 {
                break;
            }
            covered += take;
            loss += take * loss_at(px);
        }
        let rest = abs - covered;
        loss += rest * liq_px.map(loss_at).unwrap_or(entry);
        Self { isolated, leverage, stop_cover: if abs > 0.0 { covered / abs } else { 0.0 }, risk_pct: pct(loss, equity) }
    }
}

/// An open position from flat.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Trip {
    pub opened: f64,
    /// Our equity when it opened.
    pub equity: f64,
    /// Realized PnL minus fees so far.
    pub pnl: f64,
    /// Lowest `pnl` + unrealized seen (<= 0).
    pub worst: f64,
    pub max_notional: f64,
    /// The trader's set-up, the riskiest one read while the trip was open.
    #[serde(default)]
    pub plan: Option<Plan>,
}

impl Trip {
    pub fn new(at: f64, equity: f64) -> Self {
        Self { opened: at, equity, ..Default::default() }
    }

    pub fn plan(&mut self, p: Plan) {
        if self.plan.as_ref().is_none_or(|old| p.risk_pct >= old.risk_pct) {
            self.plan = Some(p);
        }
    }

    /// Marks the trip with the position's current unrealized PnL and notional.
    pub fn mark(&mut self, unrealized: f64, notional: f64) {
        self.worst = self.worst.min(self.pnl + unrealized);
        self.max_notional = self.max_notional.max(notional);
    }
}

fn pct(x: f64, of: f64) -> f64 {
    if of > 0.0 { x / of * 100.0 } else { 0.0 }
}

impl Stats {
    pub fn copy(&mut self, lag: f64, feed: f64, notional: f64, slip: f64, mv: f64, impact: f64) {
        self.copies += 1;
        self.lag_sum += lag;
        self.lag_max = self.lag_max.max(lag);
        if self.lag_hist.len() != LAG_BUCKETS {
            self.lag_hist.resize(LAG_BUCKETS, 0);
        }
        self.lag_hist[((lag / LAG_BUCKET_S) as usize).min(LAG_BUCKETS - 1)] += 1;
        self.feed_sum += feed;
        self.slip_notional += notional;
        self.slip_usd += slip;
        self.move_usd += mv;
        self.impact_usd += impact;
        if slip > 1e-9 {
            self.worse += 1;
        } else if slip < -1e-9 {
            self.better += 1;
        }
    }

    pub fn entry(&mut self, notional: f64, equity: f64) {
        let p = pct(notional, equity);
        self.entries += 1;
        self.entry_pct_sum += p;
        self.entry_pct_max = self.entry_pct_max.max(p);
    }

    pub fn mark(&mut self, equity: f64, open: usize, gross: f64) {
        self.peak = self.peak.max(equity);
        self.max_dd_pct = self.max_dd_pct.max(pct(self.peak - equity, self.peak));
        self.max_open = self.max_open.max(open);
        self.max_gross_pct = self.max_gross_pct.max(pct(gross, equity.max(0.0)));
    }

    pub fn close(&mut self, trip: &Trip, at: f64) {
        self.trips += 1;
        if trip.pnl > 0.0 {
            self.wins += 1;
            self.win_usd += trip.pnl;
        } else {
            self.loss_usd -= trip.pnl;
        }
        self.hold_s += at - trip.opened;
        let risk = pct(-trip.worst.min(trip.pnl).min(0.0), trip.equity);
        self.risk_pct_sum += risk;
        self.risk_pct_max = self.risk_pct_max.max(risk);
        let size = pct(trip.max_notional, trip.equity);
        self.size_pct_sum += size;
        self.size_pct_max = self.size_pct_max.max(size);
        if let Some(p) = &trip.plan {
            self.add_plan(p);
        }
    }

    /// Counts a trip's set-up (closed trips do it on close; `report` adds the open ones).
    pub fn add_plan(&mut self, p: &Plan) {
        self.planned += 1;
        self.with_stop += (p.stop_cover > 0.0) as u64;
        self.isolated += p.isolated as u64;
        self.lev_sum += p.leverage;
        self.lev_max = self.lev_max.max(p.leverage);
        self.plan_pct_sum += p.risk_pct;
        self.plan_pct_max = self.plan_pct_max.max(p.risk_pct);
    }

    /// Lag below which `q` (0..1) of the copies landed, to the bucket's upper edge.
    pub fn lag_q(&self, q: f64) -> f64 {
        lag_q(&self.lag_hist, q)
    }
}

pub fn lag_q(hist: &[u32], q: f64) -> f64 {
    let n: u64 = hist.iter().map(|&x| x as u64).sum();
    if n == 0 {
        return 0.0;
    }
    let want = (q * n as f64).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (i, &x) in hist.iter().enumerate() {
        seen += x as u64;
        if seen >= want {
            return (i + 1) as f64 * LAG_BUCKET_S;
        }
    }
    hist.len() as f64 * LAG_BUCKET_S
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lag_quantiles() {
        let mut s = Stats::default();
        for lag in [1.02, 1.05, 1.15, 1.31, 4.0] {
            s.copy(lag, 0.1, 100.0, 0.0, 0.0, 0.0);
        }
        assert!((s.lag_q(0.5) - 1.2).abs() < 1e-9);
        assert!((s.lag_q(0.95) - 4.1).abs() < 1e-9);
        assert_eq!(s.lag_max, 4.0);
    }

    #[test]
    fn planned_risk() {
        // Long 2 @ 100 on $1000: a stop for 1 at 95, the rest to liquidation at 80.
        let p = Plan::new(2.0, 100.0, true, 5.0, Some(80.0), &[(95.0, Some(1.0))], 1000.0);
        assert!((p.risk_pct - 2.5).abs() < 1e-9);
        assert!((p.stop_cover - 0.5).abs() < 1e-9);
        // Short with a position SL at 110 and a farther one: the nearest covers it all.
        let p = Plan::new(-2.0, 100.0, false, 10.0, None, &[(120.0, None), (110.0, None)], 1000.0);
        assert!((p.risk_pct - 2.0).abs() < 1e-9);
        assert_eq!(p.stop_cover, 1.0);
        // No stop, cross with no liquidation price: the whole notional.
        let p = Plan::new(2.0, 100.0, false, 3.0, None, &[], 1000.0);
        assert!((p.risk_pct - 20.0).abs() < 1e-9);
        // A stop past the entry locks in profit: no risk.
        let p = Plan::new(2.0, 100.0, false, 3.0, None, &[(101.0, None)], 1000.0);
        assert_eq!(p.risk_pct, 0.0);
    }

    #[test]
    fn trip_risk_and_drawdown() {
        let mut s = Stats::default();
        let mut t = Trip::new(0.0, 1000.0);
        t.pnl = -0.5; // entry fee
        t.mark(-30.0, 500.0);
        t.mark(20.0, 520.0);
        t.pnl = 15.0;
        s.close(&t, 60.0);
        assert_eq!((s.trips, s.wins), (1, 1));
        assert!((s.risk_pct_max - 3.05).abs() < 1e-9);
        assert!((s.size_pct_max - 52.0).abs() < 1e-9);
        s.mark(1000.0, 1, 500.0);
        s.mark(900.0, 3, 2700.0);
        assert!((s.max_dd_pct - 10.0).abs() < 1e-9);
        assert_eq!(s.max_open, 3);
        assert!((s.max_gross_pct - 300.0).abs() < 1e-9);
    }
}
