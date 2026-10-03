//! Our own signals out of what the followed traders do, a grid of variants side by side. Each
//! variant trades its own $1000 paper account (`signal:<name>`), filled on the live books like
//! the copies, and every entry is a complete trade: coin, side, entry, stop, take profit, size
//! from a fixed risk, expiry, and why (traders each way, agreement, conviction, dollars).
//!
//! What the traders did is read per coin over a window from each one's net flow (bought minus
//! sold), so an order split in many fills, or a market maker's churn, counts once:
//!   - heads: traders net buying vs net selling (one whale does not outvote the crowd); a trader
//!     takes a side when its net flow is at least 0.2% of its own equity;
//!   - conviction: the sum of the traders' net flows, each as % of its own equity;
//!   - volume: the dollars of net buying vs net selling;
//!   - positioning: the copied traders holding the coin long vs short now (1%+ of equity).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Heads,
    Conviction,
    Volume,
    Positioning,
}

/// Stop / take profit, % from the entry.
#[derive(Clone, Copy, Debug)]
pub struct Exit {
    pub stop_pct: f64,
    pub tp_pct: f64,
}

pub const SHORT: Exit = Exit { stop_pct: 0.75, tp_pct: 1.5 };
pub const MID: Exit = Exit { stop_pct: 1.5, tp_pct: 3.0 };
pub const LONG: Exit = Exit { stop_pct: 3.0, tp_pct: 6.0 };

#[derive(Clone, Copy, Debug)]
pub struct Variant {
    pub name: &'static str,
    pub kind: Kind,
    /// Flow window (not for positioning).
    pub window_s: f64,
    /// Traders on the winning side at least...
    pub min_traders: usize,
    /// ... and at least this share of the traders (heads, positioning) or of the dollars
    /// (volume) that took a side.
    pub min_agree: f64,
    /// Conviction: |sum of % of equity| at least this.
    pub min_score: f64,
    /// Volume: net buying + net selling at least this many dollars.
    pub min_usd: f64,
    /// Only traders whose copies we run at a profit.
    pub best_only: bool,
    /// Trade against the signal (a control: if this wins too, it is noise).
    pub fade: bool,
    pub exit: Exit,
    pub hold_s: f64,
    /// After a close, the coin is not entered again for this long.
    pub cooldown_s: f64,
}

const BASE: Variant = Variant {
    name: "",
    kind: Kind::Heads,
    window_s: 0.0,
    min_traders: 0,
    min_agree: 0.0,
    min_score: 0.0,
    min_usd: 0.0,
    best_only: false,
    fade: false,
    exit: MID,
    hold_s: 0.0,
    cooldown_s: 0.0,
};

/// Heads over `window_m`: `traders`+ on one side and `agree` of those taking one.
const fn heads(name: &'static str, window_m: f64, traders: usize, agree: f64, exit: Exit, hold_m: f64) -> Variant {
    Variant { name, kind: Kind::Heads, window_s: window_m * 60.0, min_traders: traders, min_agree: agree, exit, hold_s: hold_m * 60.0,
              cooldown_s: cooldown(window_m), ..BASE }
}

/// Conviction over `window_m`: `traders`+ taking a side and `score`% of equity summed one way.
const fn conviction(name: &'static str, window_m: f64, traders: usize, score: f64, exit: Exit, hold_m: f64) -> Variant {
    Variant { name, kind: Kind::Conviction, window_s: window_m * 60.0, min_traders: traders, min_score: score, exit,
              hold_s: hold_m * 60.0, cooldown_s: cooldown(window_m), ..BASE }
}

/// Volume over `window_m`: `usd`+ of net flow, `share` of it one way, `traders`+ on that side.
const fn volume(name: &'static str, window_m: f64, traders: usize, share: f64, usd: f64, exit: Exit, hold_m: f64) -> Variant {
    Variant { name, kind: Kind::Volume, window_s: window_m * 60.0, min_traders: traders, min_agree: share, min_usd: usd, exit,
              hold_s: hold_m * 60.0, cooldown_s: cooldown(window_m), ..BASE }
}

/// Positioning: `holders`+ on one side and `agree` of the holders.
const fn positioning(name: &'static str, holders: usize, agree: f64, exit: Exit, hold_m: f64) -> Variant {
    Variant { name, kind: Kind::Positioning, min_traders: holders, min_agree: agree, exit, hold_s: hold_m * 60.0, cooldown_s: 7200.0, ..BASE }
}

const fn best(v: Variant, name: &'static str) -> Variant {
    Variant { name, best_only: true, ..v }
}

const fn fade(v: Variant, name: &'static str) -> Variant {
    Variant { name, fade: true, ..v }
}

/// A coin rests after a close for the variant's window (5 min at least).
const fn cooldown(window_m: f64) -> f64 {
    if window_m < 5.0 { 300.0 } else { window_m * 60.0 }
}

/// The grid. Names: kind, window, traders (t), agreement (%), score (c) or dollars ($), exit
/// (S 0.75/1.5%, M 1.5/3%, L 3/6%).
pub const VARIANTS: &[Variant] = &[
    // Heads, by window and how many / how unanimous.
    heads("h1m-2t-80-S", 1.0, 2, 0.80, SHORT, 15.0),
    heads("h1m-3t-90-S", 1.0, 3, 0.90, SHORT, 15.0),
    heads("h5m-2t-70-S", 5.0, 2, 0.70, SHORT, 30.0),
    heads("h5m-3t-70-S", 5.0, 3, 0.70, SHORT, 30.0),
    heads("h5m-3t-90-S", 5.0, 3, 0.90, SHORT, 30.0),
    heads("h5m-5t-80-S", 5.0, 5, 0.80, SHORT, 30.0),
    heads("h15m-3t-70-M", 15.0, 3, 0.70, MID, 120.0),
    heads("h15m-5t-75-M", 15.0, 5, 0.75, MID, 120.0),
    heads("h15m-5t-90-M", 15.0, 5, 0.90, MID, 120.0),
    heads("h15m-8t-80-M", 15.0, 8, 0.80, MID, 120.0),
    heads("h30m-5t-75-M", 30.0, 5, 0.75, MID, 180.0),
    heads("h30m-8t-80-M", 30.0, 8, 0.80, MID, 180.0),
    heads("h60m-8t-75-L", 60.0, 8, 0.75, LONG, 360.0),
    heads("h60m-12t-80-L", 60.0, 12, 0.80, LONG, 360.0),
    heads("h240m-12t-75-L", 240.0, 12, 0.75, LONG, 1440.0),
    // The same signal, other exits.
    heads("h15m-5t-75-S", 15.0, 5, 0.75, SHORT, 60.0),
    heads("h15m-5t-75-L", 15.0, 5, 0.75, LONG, 360.0),
    // Conviction.
    conviction("c5m-2t-2c-S", 5.0, 2, 2.0, SHORT, 30.0),
    conviction("c15m-3t-5c-M", 15.0, 3, 5.0, MID, 120.0),
    conviction("c15m-3t-15c-M", 15.0, 3, 15.0, MID, 120.0),
    conviction("c60m-5t-10c-L", 60.0, 5, 10.0, LONG, 360.0),
    // Volume.
    volume("v5m-2t-80-200k-S", 5.0, 2, 0.80, 200_000.0, SHORT, 30.0),
    volume("v15m-2t-70-500k-M", 15.0, 2, 0.70, 500_000.0, MID, 120.0),
    volume("v15m-2t-90-250k-M", 15.0, 2, 0.90, 250_000.0, MID, 120.0),
    volume("v60m-3t-75-2m-L", 60.0, 3, 0.75, 2_000_000.0, LONG, 360.0),
    // Positioning of the copied traders.
    positioning("p5-70-L", 5, 0.70, LONG, 1440.0),
    positioning("p10-80-L", 10, 0.80, LONG, 1440.0),
    positioning("p20-75-L", 20, 0.75, LONG, 1440.0),
    // Only the traders whose copies run at a profit.
    best(heads("", 15.0, 3, 0.70, MID, 120.0), "best-h15m-3t-70-M"),
    best(heads("", 60.0, 3, 0.75, LONG, 360.0), "best-h60m-3t-75-L"),
    best(conviction("", 15.0, 2, 5.0, MID, 120.0), "best-c15m-2t-5c-M"),
    // Controls: the opposite trade.
    fade(heads("", 5.0, 3, 0.70, SHORT, 30.0), "fade-h5m-3t-70-S"),
    fade(heads("", 15.0, 5, 0.75, MID, 120.0), "fade-h15m-5t-75-M"),
    fade(volume("", 15.0, 2, 0.70, 500_000.0, MID, 120.0), "fade-v15m-2t-70-500k-M"),
    fade(positioning("", 10, 0.80, LONG, 1440.0), "fade-p10-80-L"),
];

/// Flow is kept this long (the longest window).
pub const MAX_WINDOW_S: f64 = 4.0 * 3600.0;
/// Each trade risks this % of the account's equity if its stop is hit...
pub const RISK_PCT: f64 = 1.0;
/// ... with all positions together at most this many times equity (the leverage cap)...
pub const MAX_GROSS: f64 = 10.0;
/// ... and at most this many open at once.
pub const MAX_OPEN: usize = 10;
/// A trader takes a side when its net flow in the window is at least this share of its equity
/// (0.2%), or, for positioning, its position is (1%).
const MIN_FLOW: f64 = 0.002;
const MIN_POSITION: f64 = 0.01;
/// One trader counts for at most this much in a conviction score (20% of its equity).
const MAX_ONE: f64 = 0.2;

/// An open signal trade of a variant's account.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OpenSignal {
    pub id: String,
    /// +1 long, -1 short.
    pub side: f64,
    pub entry: f64,
    pub stop: f64,
    pub tp: f64,
    pub opened: f64,
    pub expires: f64,
    /// The rest of the ticket, for its row when it closes.
    #[serde(default)]
    pub mid: f64,
    #[serde(default)]
    pub size: f64,
    #[serde(default)]
    pub risk_usd: f64,
    #[serde(default)]
    pub risk_pct: f64,
    #[serde(default)]
    pub fee: f64,
    #[serde(default)]
    pub reason: serde_json::Value,
}

/// A variant account's signal state: open trades and when each coin was last closed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SignalState {
    pub open: HashMap<String, OpenSignal>,
    pub closed_at: HashMap<String, f64>,
    /// Signals taken (opened).
    pub taken: u64,
    /// Limit-order twins of its trades still working or open (see `maker`).
    #[serde(default)]
    pub makers: Vec<crate::maker::MakerTrade>,
}

/// What the traders are doing in one coin, as a variant reads it.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Reading {
    /// +1 / -1: the signal's side (after `fade`), 0: none.
    pub side: f64,
    /// Traders on each side.
    pub longs: usize,
    pub shorts: usize,
    /// Share of the traders (or, for volume, of the dollars) on the winning side.
    pub agree: f64,
    /// Sum of the traders' net flows (or positions), % of each one's equity.
    pub score: f64,
    /// Dollars of net buying and of net selling (flow kinds).
    pub buy_usd: f64,
    pub sell_usd: f64,
}

/// The followed traders' fills per coin over the last hours: (exchange ms, trader, signed USD,
/// its equity).
#[derive(Default)]
pub struct Flow {
    per_coin: HashMap<String, VecDeque<(u64, String, f64, f64)>>,
    /// Since when (exchange ms) the flow is complete: a window reaching further back is not
    /// read (after a start it fills up first).
    pub since_ms: u64,
}

impl Flow {
    pub fn new(since_ms: u64) -> Self {
        Self { since_ms, ..Default::default() }
    }

    pub fn push(&mut self, coin: &str, time_ms: u64, trader: &str, usd: f64, equity: f64) {
        self.per_coin.entry(coin.to_string()).or_default().push_back((time_ms, trader.to_string(), usd, equity));
    }

    pub fn prune(&mut self, now_ms: u64) {
        let cut = now_ms.saturating_sub((MAX_WINDOW_S * 1000.0) as u64);
        self.per_coin.retain(|_, q| {
            while q.front().is_some_and(|x| x.0 < cut) {
                q.pop_front();
            }
            !q.is_empty()
        });
    }

    /// Fills held, all coins.
    pub fn len(&self) -> usize {
        self.per_coin.values().map(VecDeque::len).sum()
    }

    pub fn coins(&self) -> impl Iterator<Item = &String> {
        self.per_coin.keys()
    }

    /// Each trader's net flow in `coin` since `since_ms`: (share of its equity, USD).
    fn nets(&self, coin: &str, since_ms: u64, only: Option<&HashSet<String>>) -> Vec<(f64, f64)> {
        let mut by: HashMap<&str, (f64, f64)> = HashMap::new();
        if let Some(q) = self.per_coin.get(coin) {
            for (t, who, usd, equity) in q.iter().rev() {
                if *t < since_ms {
                    break;
                }
                if *equity > 0.0 && only.is_none_or(|s| s.contains(who)) {
                    let e = by.entry(who.as_str()).or_insert((0.0, 0.0));
                    e.0 += usd / equity;
                    e.1 += usd;
                }
            }
        }
        by.into_values().collect()
    }
}

/// One tick's view for every variant: the flow, the copied traders' positions (trader,
/// position / its equity, per coin) and the profitable ones; each (coin, window, who) is
/// summed once and shared by the variants reading it.
pub struct Inputs<'a> {
    flow: &'a Flow,
    positions: &'a HashMap<String, Vec<(String, f64)>>,
    best: &'a HashSet<String>,
    now_ms: u64,
    nets: RefCell<HashMap<(String, u64, bool), Rc<Vec<(f64, f64)>>>>,
}

impl<'a> Inputs<'a> {
    pub fn new(flow: &'a Flow, positions: &'a HashMap<String, Vec<(String, f64)>>, best: &'a HashSet<String>, now_ms: u64) -> Self {
        Self { flow, positions, best, now_ms, nets: RefCell::new(HashMap::new()) }
    }

    fn nets(&self, coin: &str, window_s: f64, best_only: bool) -> Option<Rc<Vec<(f64, f64)>>> {
        let since = self.now_ms.saturating_sub((window_s * 1000.0) as u64);
        if since < self.flow.since_ms {
            return None;
        }
        let key = (coin.to_string(), window_s as u64, best_only);
        let mut cache = self.nets.borrow_mut();
        Some(cache.entry(key).or_insert_with(|| Rc::new(self.flow.nets(coin, since, best_only.then_some(self.best)))).clone())
    }

    /// How `v` reads `coin` now.
    pub fn read(&self, v: &Variant, coin: &str) -> Reading {
        let mut rd = Reading::default();
        match v.kind {
            Kind::Heads | Kind::Conviction | Kind::Volume => {
                let Some(nets) = self.nets(coin, v.window_s, v.best_only) else { return rd };
                for &(share, usd) in nets.iter() {
                    if usd > 0.0 {
                        rd.buy_usd += usd;
                    } else {
                        rd.sell_usd -= usd;
                    }
                    count(&mut rd, share, MIN_FLOW);
                }
            }
            Kind::Positioning => {
                for (who, share) in self.positions.get(coin).into_iter().flatten() {
                    if !v.best_only || self.best.contains(who) {
                        count(&mut rd, *share, MIN_POSITION);
                    }
                }
            }
        }
        let n = rd.longs + rd.shorts;
        let fires;
        let side;
        if v.kind == Kind::Volume {
            let total = rd.buy_usd + rd.sell_usd;
            side = if rd.buy_usd >= rd.sell_usd { 1.0 } else { -1.0 };
            rd.agree = if total > 0.0 { rd.buy_usd.max(rd.sell_usd) / total } else { 0.0 };
            let heads_that_way = if side > 0.0 { rd.longs } else { rd.shorts };
            fires = total >= v.min_usd && rd.agree >= v.min_agree && heads_that_way >= v.min_traders;
        } else {
            let win = rd.longs.max(rd.shorts);
            side = if rd.longs >= rd.shorts { 1.0 } else { -1.0 };
            rd.agree = if n > 0 { win as f64 / n as f64 } else { 0.0 };
            fires = match v.kind {
                Kind::Conviction => n >= v.min_traders && rd.score.abs() >= v.min_score && rd.score.signum() == side,
                _ => win >= v.min_traders && rd.agree >= v.min_agree,
            };
        }
        rd.side = if fires { if v.fade { -side } else { side } } else { 0.0 };
        rd
    }
}

/// Counts one trader's flow or position (share of its equity) if it takes a side.
fn count(rd: &mut Reading, share: f64, min: f64) {
    if share.abs() < min {
        return;
    }
    if share > 0.0 {
        rd.longs += 1;
    } else {
        rd.shorts += 1;
    }
    rd.score += share.clamp(-MAX_ONE, MAX_ONE) * 100.0;
}

/// The size of a new trade: notional risking `RISK_PCT` of `equity` at `stop_pct`, within what
/// `MAX_GROSS` leaves over `gross` (USD).
pub fn notional(equity: f64, gross: f64, stop_pct: f64) -> f64 {
    let by_risk = equity * RISK_PCT / stop_pct;
    by_risk.min(MAX_GROSS * equity - gross).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flow(fills: &[(&str, f64, f64)]) -> Flow {
        let mut f = Flow::default();
        for (i, (who, usd, eq)) in fills.iter().enumerate() {
            f.push("BTC", 1_000_000 + i as u64, who, *usd, *eq);
        }
        f
    }

    fn var(name: &str) -> &'static Variant {
        VARIANTS.iter().find(|v| v.name == name).unwrap()
    }

    fn read(name: &str, f: &Flow, now_ms: u64) -> Reading {
        let (p, b) = (HashMap::new(), HashSet::new());
        Inputs::new(f, &p, &b, now_ms).read(var(name), "BTC")
    }

    #[test]
    fn names_are_unique() {
        let names: HashSet<&str> = VARIANTS.iter().map(|v| v.name).collect();
        assert_eq!(names.len(), VARIANTS.len());
        assert!(VARIANTS.iter().all(|v| v.window_s <= MAX_WINDOW_S && v.hold_s > 0.0));
    }

    #[test]
    fn heads_not_dollars() {
        // 5 small buyers (1% of their equity each) and one whale selling a lot.
        let f = flow(&[("a", 100.0, 1e4), ("b", 100.0, 1e4), ("c", 100.0, 1e4), ("d", 100.0, 1e4), ("e", 100.0, 1e4),
                       ("w", -1e6, 1e7)]);
        let rd = read("h15m-5t-75-M", &f, 1_000_100);
        assert_eq!((rd.longs, rd.shorts), (5, 1));
        assert!((rd.agree - 5.0 / 6.0).abs() < 1e-9);
        assert_eq!(rd.side, 1.0);
        // ... while by dollars the whale wins.
        let rd = read("v15m-2t-70-500k-M", &f, 1_000_100);
        assert_eq!((rd.buy_usd, rd.sell_usd), (500.0, 1e6));
        assert_eq!(rd.side, 0.0); // one seller: under 2 traders that way
        // Below 0.2% of equity a trader takes no side; a split order counts once.
        let f = flow(&[("a", 10.0, 1e4), ("b", 50.0, 1e4), ("b", 50.0, 1e4)]);
        let rd = read("h15m-5t-75-M", &f, 1_000_100);
        assert_eq!((rd.longs, rd.shorts, rd.side), (1, 0, 0.0));
    }

    #[test]
    fn volume() {
        let f = flow(&[("a", 300_000.0, 1e7), ("b", 250_000.0, 1e7), ("c", -50_000.0, 1e7)]);
        let rd = read("v15m-2t-70-500k-M", &f, 1_000_100);
        assert!((rd.agree - 550.0 / 600.0).abs() < 1e-9);
        assert_eq!(rd.side, 1.0);
        assert_eq!(read("fade-v15m-2t-70-500k-M", &f, 1_000_100).side, -1.0);
        assert_eq!(read("v15m-2t-90-250k-M", &f, 1_000_100).side, 1.0);
        assert_eq!(read("v60m-3t-75-2m-L", &f, 4_000_000).side, 0.0); // $600k < $2M
    }

    #[test]
    fn fade_and_window() {
        let f = flow(&[("a", 100.0, 1e4), ("b", 100.0, 1e4), ("c", 100.0, 1e4), ("d", 100.0, 1e4), ("e", 100.0, 1e4)]);
        assert_eq!(read("fade-h15m-5t-75-M", &f, 1_000_100).side, -1.0);
        // Out of the window: nothing.
        assert_eq!(read("h5m-3t-70-S", &f, 1_000_000 + 6 * 60_000).side, 0.0);
        // A window reaching back before the flow is complete (just started): nothing yet...
        let mut f = f;
        f.since_ms = 1_000_000;
        assert_eq!(read("h5m-3t-70-S", &f, 1_000_100).side, 0.0);
        // ... once it is covered, it fires.
        assert_eq!(read("h5m-3t-70-S", &f, 1_000_000 + 5 * 60_000).side, 1.0);
    }

    #[test]
    fn conviction() {
        // 3 traders: +10%, +10%, -2% of equity: score 18 (capped at 20 each).
        let f = flow(&[("a", 1000.0, 1e4), ("b", 1000.0, 1e4), ("c", -200.0, 1e4)]);
        let rd = read("c15m-3t-15c-M", &f, 1_000_100);
        assert!((rd.score - 18.0).abs() < 1e-9);
        assert_eq!(rd.side, 1.0);
        let f = flow(&[("a", 5000.0, 1e4), ("b", -100.0, 1e4), ("c", -100.0, 1e4)]);
        assert!((read("c15m-3t-15c-M", &f, 1_000_100).score - 18.0).abs() < 1e-9); // +50% capped at 20, -1, -1
        assert_eq!(read("c15m-3t-15c-M", &f, 1_000_100).side, 0.0); // most heads short, score long: no
    }

    #[test]
    fn positioning_and_best_only() {
        let mut p = HashMap::new();
        let mut holders: Vec<(String, f64)> = (0..9).map(|i| (format!("l{i}"), 0.5)).collect();
        holders.push(("s0".into(), -0.5));
        p.insert("ETH".to_string(), holders.clone());
        let none = HashSet::new();
        let f = Flow::default();
        // 9 of 10: under 10 heads.
        assert_eq!(Inputs::new(&f, &p, &none, 0).read(var("p10-80-L"), "ETH").side, 0.0);
        holders.push(("l9".into(), 0.3));
        p.insert("ETH".to_string(), holders);
        let rd = Inputs::new(&f, &p, &none, 0).read(var("p10-80-L"), "ETH");
        assert_eq!((rd.longs, rd.shorts, rd.side), (10, 1, 1.0));
        assert_eq!(Inputs::new(&f, &p, &none, 0).read(var("fade-p10-80-L"), "ETH").side, -1.0);
        // best-only counts only the listed traders.
        let f = flow(&[("a", 100.0, 1e4), ("b", 100.0, 1e4), ("c", 100.0, 1e4)]);
        let good: HashSet<String> = ["a", "b"].iter().map(|s| s.to_string()).collect();
        let empty = HashMap::new();
        assert_eq!(Inputs::new(&f, &empty, &good, 1_000_100).read(var("best-h15m-3t-70-M"), "BTC").side, 0.0);
        let good: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(Inputs::new(&f, &empty, &good, 1_000_100).read(var("best-h15m-3t-70-M"), "BTC").side, 1.0);
    }

    #[test]
    fn sizing() {
        // 1% risk at a 2% stop: half the equity; capped by what 10x leaves.
        assert!((notional(1000.0, 0.0, 2.0) - 500.0).abs() < 1e-9);
        assert!((notional(1000.0, 9800.0, 2.0) - 200.0).abs() < 1e-9);
        assert_eq!(notional(1000.0, 10500.0, 2.0), 0.0);
    }
}
