//! Our own signals, a grid of variants side by side. Each variant trades its own $1000 paper
//! account (`signal:<name>`), filled on the live books like the copies, and every entry is a
//! complete trade: coin, side, entry, stop, take profit, size from a fixed risk, expiry, and why.
//!
//! Out of what the followed traders do, per coin over a window from each one's net flow (bought
//! minus sold), so an order split in many fills, or a market maker's churn, counts once:
//!   - heads: traders net buying vs net selling (one whale does not outvote the crowd); a trader
//!     takes a side when its net flow is at least 0.2% of its own equity;
//!   - conviction: the sum of the traders' net flows, each as % of its own equity;
//!   - volume: the dollars of net buying vs net selling;
//!   - positioning: the copied traders holding the coin long vs short now (1%+ of equity),
//!     optionally only when funding (and open interest) confirm the crowd: faded, a bet
//!     against a crowded side;
//!   - clusters: the copied traders' stops and liquidation prices near the price (from their
//!     set-ups): a heavy cluster on one side, within a band, is where a cascade would start.
//!
//! Out of everyone's trades (the public trades stream names every taker):
//!   - whales: wallets whose net taker flow in a coin over a window is at least a size.
//!
//! Out of prices alone (mids sampled every minute):
//!   - trend: a coin's move over a window at least a size (followed; faded: reversal);
//!   - cross momentum: among the most traded coins, the strongest / weakest against BTC over
//!     a window.
//!
//! The flow kinds can read only some traders: those whose copies run at a profit (`best-`), or
//! those with low leverage settings and a large account (`low-`). Exits: stop and take profit
//! at fixed distances, optionally a trailing stop (`-tr`, no take profit) or the stop moved to
//! the entry once the trade is a set multiple of its risk up (`-be`).

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
    Cluster,
    Whales,
    Trend,
    CrossMomentum,
}

/// Whose flow or positions a variant reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Who {
    All,
    /// The traders whose copies we run at a profit.
    Best,
    /// The traders with low leverage settings and a large account (`LOW_LEV_*`).
    LowLev,
}

/// Stop / take profit, % from the entry; a trailing stop (% from the best price since the
/// entry, 0 = none) and the stop moved to the entry once the trade is `be_r` times its risk up
/// (0 = never).
#[derive(Clone, Copy, Debug)]
pub struct Exit {
    pub stop_pct: f64,
    pub tp_pct: f64,
    pub trail_pct: f64,
    pub be_r: f64,
}

pub const SHORT: Exit = Exit { stop_pct: 0.75, tp_pct: 1.5, trail_pct: 0.0, be_r: 0.0 };
pub const MID: Exit = Exit { stop_pct: 1.5, tp_pct: 3.0, trail_pct: 0.0, be_r: 0.0 };
pub const LONG: Exit = Exit { stop_pct: 3.0, tp_pct: 6.0, trail_pct: 0.0, be_r: 0.0 };
/// A take profit this far is none: the trade ends at its (trailing) stop, expiry or a turn.
const NO_TP: f64 = 50.0;

#[derive(Clone, Copy, Debug)]
pub struct Variant {
    pub name: &'static str,
    pub kind: Kind,
    /// Flow window, or the move's window (trend, cross momentum).
    pub window_s: f64,
    /// Traders (wallets, for whales) on the winning side at least...
    pub min_traders: usize,
    /// ... and at least this share of the traders (heads, positioning) or of the dollars
    /// (volume, whales, clusters) that took a side.
    pub min_agree: f64,
    /// Conviction: |sum of % of equity| at least this.
    pub min_score: f64,
    /// Volume: net buying + net selling at least this many dollars; whales: a wallet's net
    /// taker flow at least this; clusters: the stops / liquidations on the side at least this.
    pub min_usd: f64,
    pub who: Who,
    /// Trade against the signal (a control: if this wins too, it is noise).
    pub fade: bool,
    pub exit: Exit,
    pub hold_s: f64,
    /// After a close, the coin is not entered again for this long.
    pub cooldown_s: f64,
    /// Trend: the move over the window at least this, %.
    pub move_pct: f64,
    /// Cross momentum: this many strongest (long) and weakest (short).
    pub top_k: usize,
    /// Clusters: stops and liquidations within this % of the price.
    pub band_pct: f64,
    /// Positioning: the funding rate (hourly) at least this in the crowd's direction (it pays
    /// to be on that side); 0 = not read.
    pub min_funding: f64,
    /// Positioning: open interest up at least this % over `OI_WINDOW_S`; 0 = not read.
    pub min_oi_pct: f64,
}

const BASE: Variant = Variant {
    name: "",
    kind: Kind::Heads,
    window_s: 0.0,
    min_traders: 0,
    min_agree: 0.0,
    min_score: 0.0,
    min_usd: 0.0,
    who: Who::All,
    fade: false,
    exit: MID,
    hold_s: 0.0,
    cooldown_s: 0.0,
    move_pct: 0.0,
    top_k: 0,
    band_pct: 0.0,
    min_funding: 0.0,
    min_oi_pct: 0.0,
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

/// Clusters: within `band_pct`% of the price, `usd`+ of stops / liquidations on one side and
/// `share` of those in the band; the trade goes the way they would push the price.
const fn cluster(name: &'static str, band_pct: f64, usd: f64, share: f64, exit: Exit, hold_m: f64) -> Variant {
    Variant { name, kind: Kind::Cluster, band_pct, min_usd: usd, min_agree: share, min_traders: 1, exit, hold_s: hold_m * 60.0,
              cooldown_s: 1800.0, ..BASE }
}

/// Whales over `window_m`: `wallets`+ with `usd`+ of net taker flow one way, `share` of the
/// whales' dollars that way.
const fn whales(name: &'static str, window_m: f64, wallets: usize, share: f64, usd: f64, exit: Exit, hold_m: f64) -> Variant {
    Variant { name, kind: Kind::Whales, window_s: window_m * 60.0, min_traders: wallets, min_agree: share, min_usd: usd, exit,
              hold_s: hold_m * 60.0, cooldown_s: cooldown(window_m), ..BASE }
}

/// Trend: the coin moved `move_pct`%+ over `window_m`, followed.
const fn trend(name: &'static str, window_m: f64, move_pct: f64, exit: Exit, hold_m: f64) -> Variant {
    Variant { name, kind: Kind::Trend, window_s: window_m * 60.0, move_pct, exit, hold_s: hold_m * 60.0, cooldown_s: cooldown(window_m), ..BASE }
}

/// Cross momentum over `window_m`: long the `k` strongest, short the `k` weakest against BTC;
/// held for `hold_m`, entered again at once while still among them.
const fn xmom(name: &'static str, window_m: f64, k: usize, exit: Exit, hold_m: f64) -> Variant {
    Variant { name, kind: Kind::CrossMomentum, window_s: window_m * 60.0, top_k: k, exit, hold_s: hold_m * 60.0, cooldown_s: 0.0, ..BASE }
}

const fn best(v: Variant, name: &'static str) -> Variant {
    Variant { name, who: Who::Best, ..v }
}

const fn low_lev(v: Variant, name: &'static str) -> Variant {
    Variant { name, who: Who::LowLev, ..v }
}

const fn fade(v: Variant, name: &'static str) -> Variant {
    Variant { name, fade: true, ..v }
}

/// Positioning only when funding (hourly rate) and, if `oi_pct` > 0, open interest confirm it.
const fn crowded(v: Variant, funding: f64, oi_pct: f64, name: &'static str) -> Variant {
    Variant { name, min_funding: funding, min_oi_pct: oi_pct, ..v }
}

/// A trailing stop `pct`% from the best price since the entry, no take profit.
const fn trail(v: Variant, pct: f64, name: &'static str) -> Variant {
    Variant { name, exit: Exit { trail_pct: pct, tp_pct: NO_TP, ..v.exit }, ..v }
}

/// The stop moved to the entry once the trade is `r` times its risk up.
const fn breakeven(v: Variant, r: f64, name: &'static str) -> Variant {
    Variant { name, exit: Exit { be_r: r, ..v.exit }, ..v }
}

const fn held(v: Variant, hold_m: f64, name: &'static str) -> Variant {
    Variant { name, hold_s: hold_m * 60.0, ..v }
}

/// A coin rests after a close for the variant's window (5 min at least).
const fn cooldown(window_m: f64) -> f64 {
    if window_m < 5.0 { 300.0 } else { window_m * 60.0 }
}

/// The grid. Names: kind, window, traders (t) / wallets (w), agreement (%), score (c) or
/// dollars ($), exit (S 0.75/1.5%, M 1.5/3%, L 3/6%), then the exit's variation.
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
    // Whales: any wallet's net taker flow.
    whales("w5m-1w-80-500k-L", 5.0, 1, 0.80, 500_000.0, LONG, 240.0),
    whales("w15m-1w-80-1m-L", 15.0, 1, 0.80, 1_000_000.0, LONG, 240.0),
    whales("w15m-2w-80-250k-M", 15.0, 2, 0.80, 250_000.0, MID, 120.0),
    whales("w60m-2w-75-1m-L", 60.0, 2, 0.75, 1_000_000.0, LONG, 360.0),
    fade(whales("", 15.0, 1, 0.80, 1_000_000.0, LONG, 240.0), "fade-w15m-1w-80-1m-L"),
    // Prices: trend (followed) and reversal (faded), cross momentum against BTC.
    trend("t15m-60bp-M", 15.0, 0.6, MID, 30.0),
    trend("t60m-120bp-M", 60.0, 1.2, MID, 60.0),
    fade(trend("", 240.0, 3.3, LONG, 240.0), "fade-t240m-330bp-L"),
    xmom("x60m-top3-L", 60.0, 3, LONG, 60.0),
    xmom("x240m-top3-L", 240.0, 3, LONG, 240.0),
    fade(xmom("", 60.0, 3, LONG, 60.0), "fade-x60m-top3-L"),
    // The copied traders' stops and liquidations near the price.
    cluster("sl1-250k-70-M", 1.0, 250_000.0, 0.70, MID, 120.0),
    cluster("sl2-1m-70-L", 2.0, 1_000_000.0, 0.70, LONG, 240.0),
    fade(cluster("", 1.0, 250_000.0, 0.70, MID, 120.0), "fade-sl1-250k-70-M"),
    // Against a crowded side: positioning, confirmed by funding (0.00125%/h, ~11% a year) and
    // open interest.
    crowded(fade(positioning("", 10, 0.80, LONG, 1440.0), ""), 0.0000125, 0.0, "fade-p10-80-fund-L"),
    crowded(fade(positioning("", 10, 0.80, LONG, 1440.0), ""), 0.0000125, 3.0, "fade-p10-80-fund-oi-L"),
    crowded(fade(positioning("", 5, 0.70, LONG, 1440.0), ""), 0.0000125, 0.0, "fade-p5-70-fund-L"),
    // Only low-leverage, large accounts.
    low_lev(conviction("", 15.0, 1, 5.0, MID, 120.0), "low-c15m-1t-5c-M"),
    low_lev(heads("", 15.0, 2, 0.70, MID, 120.0), "low-h15m-2t-70-M"),
    low_lev(volume("", 15.0, 1, 0.70, 250_000.0, MID, 120.0), "low-v15m-1t-70-250k-M"),
    // The best signals so far, other exits.
    breakeven(best(conviction("", 15.0, 2, 5.0, MID, 120.0), ""), 1.0, "best-c15m-2t-5c-M-be"),
    trail(best(conviction("", 15.0, 2, 5.0, MID, 120.0), ""), 1.5, "best-c15m-2t-5c-M-tr"),
    trail(volume("", 15.0, 2, 0.70, 500_000.0, MID, 120.0), 1.5, "v15m-2t-70-500k-M-tr"),
    held(volume("", 15.0, 2, 0.70, 500_000.0, MID, 120.0), 60.0, "v15m-2t-70-500k-M-60"),
    trail(volume("", 60.0, 3, 0.75, 2_000_000.0, LONG, 360.0), 3.0, "v60m-3t-75-2m-L-tr"),
    breakeven(conviction("", 60.0, 5, 10.0, LONG, 360.0), 1.0, "c60m-5t-10c-L-be"),
];

/// Flow is kept this long (the longest window).
pub const MAX_WINDOW_S: f64 = 4.0 * 3600.0;
/// Whale flow is kept this long (the longest whale window).
pub const WHALE_WINDOW_S: f64 = 3600.0;
/// Mid samples are kept this long (the longest price window, with room).
pub const PRICE_KEEP_S: f64 = 5.0 * 3600.0;
/// Open interest change is read over this.
pub const OI_WINDOW_S: f64 = 4.0 * 3600.0;
/// Trend reads the most traded coins (24 h volume), cross momentum fewer.
pub const TREND_COINS: usize = 60;
pub const XMOM_COINS: usize = 40;
/// Low leverage: every leverage setting seen at most this, account at least this.
pub const LOW_LEV_MAX: f64 = 10.0;
pub const LOW_LEV_MIN_EQUITY: f64 = 30_000.0;
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

/// The stop after the price reached `mid` (`peak`: the best price since the entry, kept up to
/// date): moved to the entry once the trade is `be_r` x its risk up, trailing `trail_pct`%
/// behind the peak; never loosened.
#[allow(clippy::too_many_arguments)]
pub fn follow_stop(side: f64, entry: f64, stop: f64, stop_pct: f64, trail_pct: f64, be_r: f64, peak: &mut f64, mid: f64) -> f64 {
    if trail_pct <= 0.0 && be_r <= 0.0 {
        return stop;
    }
    if *peak <= 0.0 || (mid - *peak) * side > 0.0 {
        *peak = mid;
    }
    let tighter = |a: f64, b: f64| if side > 0.0 { a.max(b) } else { a.min(b) };
    let mut s = stop;
    if be_r > 0.0 && (*peak - entry) * side >= be_r * entry * stop_pct / 100.0 {
        s = tighter(s, entry);
    }
    if trail_pct > 0.0 {
        s = tighter(s, *peak * (1.0 - side * trail_pct / 100.0));
    }
    s
}

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
    /// The exit's trailing / break-even rules (see `Exit`) and the best mid since the entry.
    #[serde(default)]
    pub stop_pct: f64,
    #[serde(default)]
    pub trail_pct: f64,
    #[serde(default)]
    pub be_r: f64,
    #[serde(default)]
    pub peak: f64,
}

impl OpenSignal {
    /// Moves the stop as the price reaches `mid` (trailing / break-even exits).
    pub fn follow(&mut self, mid: f64) {
        self.stop = follow_stop(self.side, self.entry, self.stop, self.stop_pct, self.trail_pct, self.be_r, &mut self.peak, mid);
    }
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

/// What a variant reads in one coin.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Reading {
    /// +1 / -1: the signal's side (after `fade`), 0: none.
    pub side: f64,
    /// Traders (wallets) on each side.
    pub longs: usize,
    pub shorts: usize,
    /// Share of the traders (or, for volume, whales, clusters, of the dollars) on the winning
    /// side.
    pub agree: f64,
    /// Sum of the traders' net flows (or positions), % of each one's equity; for trend and
    /// cross momentum, the move in bps (cross momentum: against BTC).
    pub score: f64,
    /// Dollars of net buying and of net selling (flow kinds, whales); for clusters, of the
    /// buy-side triggers above the price and the sell-side ones below it.
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

/// Every wallet's net taker flow per coin, in one-minute buckets over `WHALE_WINDOW_S`, from
/// the public trades stream (the websocket task adds to it, the engine reads it).
#[derive(Default)]
pub struct WhaleFlow {
    per_coin: HashMap<String, HashMap<String, VecDeque<(u64, f64)>>>,
    /// Since when (exchange ms) the flow is complete.
    pub since_ms: u64,
    pruned_ms: u64,
}

impl WhaleFlow {
    pub fn new(since_ms: u64) -> Self {
        Self { since_ms, ..Default::default() }
    }

    /// A trade's taker `user` bought (+) or sold (-) `usd` at `time_ms`.
    pub fn push(&mut self, coin: &str, user: &str, time_ms: u64, usd: f64) {
        let minute = time_ms / 60_000 * 60_000;
        let q = self.per_coin.entry(coin.to_string()).or_default().entry(user.to_string()).or_default();
        match q.back_mut() {
            Some(b) if b.0 == minute => b.1 += usd,
            _ => q.push_back((minute, usd)),
        }
        if time_ms >= self.pruned_ms + 60_000 {
            self.prune(time_ms);
        }
    }

    fn prune(&mut self, now_ms: u64) {
        self.pruned_ms = now_ms;
        let cut = now_ms.saturating_sub((WHALE_WINDOW_S * 1000.0) as u64 + 60_000);
        self.per_coin.retain(|_, users| {
            users.retain(|_, q| {
                while q.front().is_some_and(|x| x.0 < cut) {
                    q.pop_front();
                }
                !q.is_empty()
            });
            !users.is_empty()
        });
    }

    /// Wallets tracked, all coins.
    pub fn wallets(&self) -> usize {
        self.per_coin.values().map(HashMap::len).sum()
    }

    pub fn coins(&self) -> impl Iterator<Item = &String> {
        self.per_coin.keys()
    }

    /// Each wallet's net taker flow (USD) in `coin` from the minute of `since_ms` on.
    fn nets(&self, coin: &str, since_ms: u64) -> Vec<f64> {
        let from = since_ms / 60_000 * 60_000;
        self.per_coin.get(coin).map(|users| {
            users.values().map(|q| q.iter().rev().take_while(|b| b.0 >= from).map(|b| b.1).sum::<f64>()).filter(|n| *n != 0.0).collect()
        }).unwrap_or_default()
    }
}

/// Mids per coin, the latest of each minute, over `PRICE_KEEP_S`.
#[derive(Default)]
pub struct Prices {
    per_coin: HashMap<String, VecDeque<(u64, f64)>>,
    /// Since when (ms) sampling runs.
    pub since_ms: u64,
}

impl Prices {
    pub fn new(since_ms: u64) -> Self {
        Self { since_ms, ..Default::default() }
    }

    pub fn sample(&mut self, coin: &str, now_ms: u64, mid: f64) {
        let minute = now_ms / 60_000 * 60_000;
        let q = self.per_coin.entry(coin.to_string()).or_default();
        match q.back_mut() {
            Some(b) if b.0 == minute => b.1 = mid,
            _ => q.push_back((minute, mid)),
        }
        let cut = now_ms.saturating_sub((PRICE_KEEP_S * 1000.0) as u64);
        while q.front().is_some_and(|x| x.0 < cut) {
            q.pop_front();
        }
    }

    /// Earlier mids (minute, price), e.g. candles read at a start: only those before what is
    /// sampled already; sampling then counts as covering them.
    pub fn seed(&mut self, coin: &str, samples: &[(u64, f64)]) {
        let q = self.per_coin.entry(coin.to_string()).or_default();
        let first = q.front().map(|x| x.0).unwrap_or(u64::MAX);
        for &(t, p) in samples.iter().rev() {
            let minute = t / 60_000 * 60_000;
            if minute < q.front().map(|x| x.0).unwrap_or(u64::MAX) && minute < first && p > 0.0 {
                q.push_front((minute, p));
            }
        }
        if let Some(f) = q.front() {
            self.since_ms = self.since_ms.min(f.0);
        }
    }

    pub fn last(&self, coin: &str) -> Option<f64> {
        self.per_coin.get(coin)?.back().map(|x| x.1)
    }

    /// The move over the last `window_s` (fraction): the latest mid against the last one known
    /// `window_s` back (the latest of the last minute that ended by then; None until sampling
    /// covers it).
    pub fn ret(&self, coin: &str, now_ms: u64, window_s: f64) -> Option<f64> {
        let back = now_ms.checked_sub((window_s * 1000.0) as u64)?;
        if back < self.since_ms {
            return None;
        }
        let q = self.per_coin.get(coin)?;
        let then = q.iter().rev().find(|x| x.0 + 60_000 <= back)?;
        if back - (then.0 + 60_000) > 120_000 || then.1 <= 0.0 {
            return None;
        }
        Some(q.back()?.1 / then.1 - 1.0)
    }
}

/// Per coin from the exchange (every few minutes): funding (hourly rate), 24 h volume (USD),
/// and open interest (USD) over `OI_WINDOW_S`.
#[derive(Default)]
pub struct Ctx {
    pub funding: HashMap<String, f64>,
    pub volume: HashMap<String, f64>,
    oi: HashMap<String, VecDeque<(u64, f64)>>,
    /// The first read (ms).
    pub since_ms: u64,
}

impl Ctx {
    /// (coin, funding, 24 h volume USD, open interest USD) read at `now_ms`.
    pub fn update<'a>(&mut self, now_ms: u64, rows: impl Iterator<Item = (&'a String, f64, f64, f64)>) {
        if self.since_ms == 0 {
            self.since_ms = now_ms;
        }
        let cut = now_ms.saturating_sub((OI_WINDOW_S * 1000.0) as u64 + 3_600_000);
        for (coin, funding, volume, oi) in rows {
            self.funding.insert(coin.clone(), funding);
            self.volume.insert(coin.clone(), volume);
            let q = self.oi.entry(coin.clone()).or_default();
            q.push_back((now_ms, oi));
            while q.front().is_some_and(|x| x.0 < cut) {
                q.pop_front();
            }
        }
    }

    /// Open interest change over `window_s`, % (None until the reads cover it).
    pub fn oi_change_pct(&self, coin: &str, now_ms: u64, window_s: f64) -> Option<f64> {
        let back = now_ms.checked_sub((window_s * 1000.0) as u64)?;
        if self.since_ms == 0 || back < self.since_ms {
            return None;
        }
        let q = self.oi.get(coin)?;
        let then = q.iter().rev().find(|x| x.0 <= back)?;
        if then.1 <= 0.0 {
            return None;
        }
        Some((q.back()?.1 / then.1 - 1.0) * 100.0)
    }

    /// The `n` most traded coins (24 h).
    pub fn liquid(&self, n: usize) -> Vec<String> {
        let mut v: Vec<(&String, f64)> = self.volume.iter().map(|(c, x)| (c, *x)).collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
        v.into_iter().take(n).map(|(c, _)| c.clone()).collect()
    }
}

/// A copied trader's stop or liquidation price: hit, it buys (`dir` +1: a short's) or sells
/// (-1: a long's) `usd` at the market.
#[derive(Clone, Debug)]
pub struct Level {
    pub px: f64,
    pub usd: f64,
    pub dir: f64,
    pub who: String,
}

/// What the variants read this tick.
pub struct Data<'a> {
    pub flow: &'a Flow,
    /// Per coin: (trader, position / its equity) of the copied traders.
    pub positions: &'a HashMap<String, Vec<(String, f64)>>,
    pub best: &'a HashSet<String>,
    pub low_lev: &'a HashSet<String>,
    pub whales: &'a WhaleFlow,
    pub prices: &'a Prices,
    pub ctx: &'a Ctx,
    pub levels: &'a HashMap<String, Vec<Level>>,
    pub now_ms: u64,
}

/// What one tick works out once and shares between the variants reading it.
type Cache<K, V> = RefCell<HashMap<K, Rc<V>>>;

/// One tick's view for every variant; what several variants read (a coin's flow over a window,
/// a ranking) is worked out once and shared.
pub struct Inputs<'a> {
    d: Data<'a>,
    trend_coins: HashSet<String>,
    xmom_coins: Vec<String>,
    nets: Cache<(String, u64, Who), Vec<(f64, f64)>>,
    whale_nets: Cache<(String, u64), Vec<f64>>,
    /// Per window: coin -> (rank from the weakest, coins ranked, move against BTC).
    ranks: Cache<u64, HashMap<String, (usize, usize, f64)>>,
}

impl<'a> Inputs<'a> {
    pub fn new(d: Data<'a>) -> Self {
        let trend_coins = d.ctx.liquid(TREND_COINS).into_iter().collect();
        let xmom_coins = d.ctx.liquid(XMOM_COINS);
        Self { d, trend_coins, xmom_coins, nets: RefCell::default(), whale_nets: RefCell::default(), ranks: RefCell::default() }
    }

    fn who(&self, w: Who) -> Option<&HashSet<String>> {
        match w {
            Who::All => None,
            Who::Best => Some(self.d.best),
            Who::LowLev => Some(self.d.low_lev),
        }
    }

    fn nets(&self, coin: &str, window_s: f64, who: Who) -> Option<Rc<Vec<(f64, f64)>>> {
        let since = self.d.now_ms.saturating_sub((window_s * 1000.0) as u64);
        if since < self.d.flow.since_ms {
            return None;
        }
        let key = (coin.to_string(), window_s as u64, who);
        let mut cache = self.nets.borrow_mut();
        Some(cache.entry(key).or_insert_with(|| Rc::new(self.d.flow.nets(coin, since, self.who(who)))).clone())
    }

    fn whale_nets(&self, coin: &str, window_s: f64) -> Option<Rc<Vec<f64>>> {
        let since = self.d.now_ms.saturating_sub((window_s * 1000.0) as u64);
        if since < self.d.whales.since_ms {
            return None;
        }
        let mut cache = self.whale_nets.borrow_mut();
        Some(cache.entry((coin.to_string(), window_s as u64)).or_insert_with(|| Rc::new(self.d.whales.nets(coin, since))).clone())
    }

    fn ranks(&self, window_s: f64) -> Rc<HashMap<String, (usize, usize, f64)>> {
        let mut cache = self.ranks.borrow_mut();
        cache.entry(window_s as u64).or_insert_with(|| {
            let (p, now) = (self.d.prices, self.d.now_ms);
            let mut out = HashMap::new();
            if let Some(btc) = p.ret("BTC", now, window_s) {
                let mut v: Vec<(&String, f64)> = self.xmom_coins.iter().filter(|c| c.as_str() != "BTC")
                    .filter_map(|c| Some((c, p.ret(c, now, window_s)? - btc))).collect();
                v.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(b.0)));
                let n = v.len();
                for (i, (c, x)) in v.into_iter().enumerate() {
                    out.insert(c.clone(), (i, n, x));
                }
            }
            Rc::new(out)
        }).clone()
    }

    /// How `v` reads `coin` now.
    pub fn read(&self, v: &Variant, coin: &str) -> Reading {
        let mut rd = Reading::default();
        let (fires, side) = match v.kind {
            Kind::Heads | Kind::Conviction | Kind::Volume => {
                let Some(nets) = self.nets(coin, v.window_s, v.who) else { return rd };
                for &(share, usd) in nets.iter() {
                    if usd > 0.0 {
                        rd.buy_usd += usd;
                    } else {
                        rd.sell_usd -= usd;
                    }
                    count(&mut rd, share, MIN_FLOW);
                }
                let big = rd.buy_usd + rd.sell_usd >= v.min_usd;
                if v.kind == Kind::Volume { self.by_dollars(v, &mut rd, big) } else { self.by_heads(v, &mut rd) }
            }
            Kind::Positioning => {
                let who = self.who(v.who);
                for (t, share) in self.d.positions.get(coin).into_iter().flatten() {
                    if who.is_none_or(|s| s.contains(t)) {
                        count(&mut rd, *share, MIN_POSITION);
                    }
                }
                let (fires, side) = self.by_heads(v, &mut rd);
                let funding_ok = v.min_funding <= 0.0 || self.d.ctx.funding.get(coin).is_some_and(|f| f * side >= v.min_funding);
                let oi_ok = v.min_oi_pct <= 0.0
                    || self.d.ctx.oi_change_pct(coin, self.d.now_ms, OI_WINDOW_S).is_some_and(|x| x >= v.min_oi_pct);
                (fires && funding_ok && oi_ok, side)
            }
            Kind::Whales => {
                let Some(nets) = self.whale_nets(coin, v.window_s) else { return rd };
                for &net in nets.iter().filter(|n| n.abs() >= v.min_usd) {
                    if net > 0.0 {
                        rd.longs += 1;
                        rd.buy_usd += net;
                    } else {
                        rd.shorts += 1;
                        rd.sell_usd -= net;
                    }
                }
                let any = rd.longs + rd.shorts > 0;
                self.by_dollars(v, &mut rd, any)
            }
            Kind::Cluster => {
                let Some(mid) = self.d.prices.last(coin) else { return rd };
                let band = mid * v.band_pct / 100.0;
                let (mut up, mut down) = (HashSet::new(), HashSet::new());
                for l in self.d.levels.get(coin).into_iter().flatten() {
                    if l.dir > 0.0 && l.px >= mid && l.px <= mid + band {
                        rd.buy_usd += l.usd;
                        up.insert(l.who.as_str());
                    } else if l.dir < 0.0 && l.px <= mid && l.px >= mid - band {
                        rd.sell_usd += l.usd;
                        down.insert(l.who.as_str());
                    }
                }
                (rd.longs, rd.shorts) = (up.len(), down.len());
                let side = if rd.buy_usd >= rd.sell_usd { 1.0 } else { -1.0 };
                let that_way = if side > 0.0 { rd.buy_usd } else { rd.sell_usd };
                self.by_dollars(v, &mut rd, that_way >= v.min_usd)
            }
            Kind::Trend => {
                if !self.trend_coins.contains(coin) {
                    return rd;
                }
                let Some(x) = self.d.prices.ret(coin, self.d.now_ms, v.window_s) else { return rd };
                rd.score = x * 1e4;
                rd.agree = 1.0;
                (x.abs() * 100.0 >= v.move_pct, x.signum())
            }
            Kind::CrossMomentum => {
                let ranks = self.ranks(v.window_s);
                let Some(&(i, n, x)) = ranks.get(coin) else { return rd };
                rd.score = x * 1e4;
                rd.agree = 1.0;
                if n < 4 * v.top_k {
                    (false, 0.0)
                } else if i + v.top_k >= n {
                    (true, 1.0)
                } else if i < v.top_k {
                    (true, -1.0)
                } else {
                    (false, 0.0)
                }
            }
        };
        rd.side = if fires { if v.fade { -side } else { side } } else { 0.0 };
        rd
    }

    /// The side most dollars took; it fires with `big` and `min_agree` of the dollars and
    /// `min_traders` that way.
    fn by_dollars(&self, v: &Variant, rd: &mut Reading, big: bool) -> (bool, f64) {
        let total = rd.buy_usd + rd.sell_usd;
        let side = if rd.buy_usd >= rd.sell_usd { 1.0 } else { -1.0 };
        rd.agree = if total > 0.0 { rd.buy_usd.max(rd.sell_usd) / total } else { 0.0 };
        let heads = if side > 0.0 { rd.longs } else { rd.shorts };
        (big && total > 0.0 && rd.agree >= v.min_agree && heads >= v.min_traders, side)
    }

    /// The side most traders took; conviction also needs its score that way.
    fn by_heads(&self, v: &Variant, rd: &mut Reading) -> (bool, f64) {
        let n = rd.longs + rd.shorts;
        let win = rd.longs.max(rd.shorts);
        let side = if rd.longs >= rd.shorts { 1.0 } else { -1.0 };
        rd.agree = if n > 0 { win as f64 / n as f64 } else { 0.0 };
        let fires = match v.kind {
            Kind::Conviction => n >= v.min_traders && rd.score.abs() >= v.min_score && rd.score.signum() == side,
            _ => win >= v.min_traders && rd.agree >= v.min_agree,
        };
        (fires, side)
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
        VARIANTS.iter().find(|v| v.name == name).unwrap_or_else(|| panic!("no variant {name}"))
    }

    /// Everything empty but what a test sets.
    #[derive(Default)]
    struct World {
        flow: Flow,
        positions: HashMap<String, Vec<(String, f64)>>,
        best: HashSet<String>,
        low_lev: HashSet<String>,
        whales: WhaleFlow,
        prices: Prices,
        ctx: Ctx,
        levels: HashMap<String, Vec<Level>>,
    }

    impl World {
        fn read(&self, name: &str, coin: &str, now_ms: u64) -> Reading {
            Inputs::new(Data {
                flow: &self.flow, positions: &self.positions, best: &self.best, low_lev: &self.low_lev, whales: &self.whales,
                prices: &self.prices, ctx: &self.ctx, levels: &self.levels, now_ms,
            }).read(var(name), coin)
        }
    }

    fn read(name: &str, f: Flow, now_ms: u64) -> Reading {
        World { flow: f, ..Default::default() }.read(name, "BTC", now_ms)
    }

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn names_are_unique() {
        let names: HashSet<&str> = VARIANTS.iter().map(|v| v.name).collect();
        assert_eq!(names.len(), VARIANTS.len());
        assert!(VARIANTS.iter().all(|v| !v.name.is_empty() && v.window_s <= MAX_WINDOW_S && v.hold_s > 0.0));
        assert!(VARIANTS.iter().filter(|v| v.kind == Kind::Whales).all(|v| v.window_s <= WHALE_WINDOW_S));
        assert!(VARIANTS.iter().all(|v| v.window_s <= PRICE_KEEP_S - 600.0));
    }

    #[test]
    fn heads_not_dollars() {
        // 5 small buyers (1% of their equity each) and one whale selling a lot.
        let f = || flow(&[("a", 100.0, 1e4), ("b", 100.0, 1e4), ("c", 100.0, 1e4), ("d", 100.0, 1e4), ("e", 100.0, 1e4),
                          ("w", -1e6, 1e7)]);
        let rd = read("h15m-5t-75-M", f(), 1_000_100);
        assert_eq!((rd.longs, rd.shorts), (5, 1));
        assert!((rd.agree - 5.0 / 6.0).abs() < 1e-9);
        assert_eq!(rd.side, 1.0);
        // ... while by dollars the whale wins.
        let rd = read("v15m-2t-70-500k-M", f(), 1_000_100);
        assert_eq!((rd.buy_usd, rd.sell_usd), (500.0, 1e6));
        assert_eq!(rd.side, 0.0); // one seller: under 2 traders that way
        // Below 0.2% of equity a trader takes no side; a split order counts once.
        let rd = read("h15m-5t-75-M", flow(&[("a", 10.0, 1e4), ("b", 50.0, 1e4), ("b", 50.0, 1e4)]), 1_000_100);
        assert_eq!((rd.longs, rd.shorts, rd.side), (1, 0, 0.0));
    }

    #[test]
    fn volume() {
        let f = || flow(&[("a", 300_000.0, 1e7), ("b", 250_000.0, 1e7), ("c", -50_000.0, 1e7)]);
        let rd = read("v15m-2t-70-500k-M", f(), 1_000_100);
        assert!((rd.agree - 550.0 / 600.0).abs() < 1e-9);
        assert_eq!(rd.side, 1.0);
        assert_eq!(read("fade-v15m-2t-70-500k-M", f(), 1_000_100).side, -1.0);
        assert_eq!(read("v15m-2t-90-250k-M", f(), 1_000_100).side, 1.0);
        assert_eq!(read("v60m-3t-75-2m-L", f(), 4_000_000).side, 0.0); // $600k < $2M
    }

    #[test]
    fn fade_and_window() {
        let f = || flow(&[("a", 100.0, 1e4), ("b", 100.0, 1e4), ("c", 100.0, 1e4), ("d", 100.0, 1e4), ("e", 100.0, 1e4)]);
        assert_eq!(read("fade-h15m-5t-75-M", f(), 1_000_100).side, -1.0);
        // Out of the window: nothing.
        assert_eq!(read("h5m-3t-70-S", f(), 1_000_000 + 6 * 60_000).side, 0.0);
        // A window reaching back before the flow is complete (just started): nothing yet...
        let mut g = f();
        g.since_ms = 1_000_000;
        assert_eq!(read("h5m-3t-70-S", g, 1_000_100).side, 0.0);
        // ... once it is covered, it fires.
        let mut g = f();
        g.since_ms = 1_000_000;
        assert_eq!(read("h5m-3t-70-S", g, 1_000_000 + 5 * 60_000).side, 1.0);
    }

    #[test]
    fn conviction() {
        // 3 traders: +10%, +10%, -2% of equity: score 18 (capped at 20 each).
        let rd = read("c15m-3t-15c-M", flow(&[("a", 1000.0, 1e4), ("b", 1000.0, 1e4), ("c", -200.0, 1e4)]), 1_000_100);
        assert!((rd.score - 18.0).abs() < 1e-9);
        assert_eq!(rd.side, 1.0);
        let f = || flow(&[("a", 5000.0, 1e4), ("b", -100.0, 1e4), ("c", -100.0, 1e4)]);
        assert!((read("c15m-3t-15c-M", f(), 1_000_100).score - 18.0).abs() < 1e-9); // +50% capped at 20, -1, -1
        assert_eq!(read("c15m-3t-15c-M", f(), 1_000_100).side, 0.0); // most heads short, score long: no
    }

    fn holders(longs: usize, shorts: usize) -> Vec<(String, f64)> {
        (0..longs).map(|i| (format!("l{i}"), 0.5)).chain((0..shorts).map(|i| (format!("s{i}"), -0.5))).collect()
    }

    #[test]
    fn positioning_and_who() {
        let mut w = World::default();
        // 9 of 10: under 10 heads.
        w.positions.insert("ETH".into(), holders(9, 1));
        assert_eq!(w.read("p10-80-L", "ETH", 0).side, 0.0);
        w.positions.insert("ETH".into(), holders(10, 1));
        let rd = w.read("p10-80-L", "ETH", 0);
        assert_eq!((rd.longs, rd.shorts, rd.side), (10, 1, 1.0));
        assert_eq!(w.read("fade-p10-80-L", "ETH", 0).side, -1.0);
        // best-only / low-leverage-only count only the listed traders.
        let mut w = World { flow: flow(&[("a", 100.0, 1e4), ("b", 100.0, 1e4), ("c", 100.0, 1e4)]), ..Default::default() };
        w.best = set(&["a", "b"]);
        assert_eq!(w.read("best-h15m-3t-70-M", "BTC", 1_000_100).side, 0.0);
        w.best = set(&["a", "b", "c"]);
        assert_eq!(w.read("best-h15m-3t-70-M", "BTC", 1_000_100).side, 1.0);
        w.low_lev = set(&["a"]);
        assert_eq!(w.read("low-h15m-2t-70-M", "BTC", 1_000_100).side, 0.0);
        w.low_lev = set(&["a", "c"]);
        assert_eq!(w.read("low-h15m-2t-70-M", "BTC", 1_000_100).side, 1.0);
        // One low-leverage trader putting 5% of its equity in is enough for low-c15m.
        let mut w = World { flow: flow(&[("a", 600.0, 1e4), ("x", -5000.0, 1e4)]), ..Default::default() };
        w.low_lev = set(&["a"]);
        let rd = w.read("low-c15m-1t-5c-M", "BTC", 1_000_100);
        assert_eq!((rd.longs, rd.shorts, rd.side), (1, 0, 1.0));
    }

    fn ctx(now_ms: u64, rows: &[(&str, f64, f64, f64)]) -> Ctx {
        let mut c = Ctx::default();
        c.update(now_ms, rows.iter().map(|(n, f, v, o)| (Box::leak(Box::new(n.to_string())) as &String, *f, *v, *o)));
        c
    }

    #[test]
    fn crowded_needs_funding_and_open_interest() {
        let mut w = World::default();
        w.positions.insert("ETH".into(), holders(10, 1));
        // Crowd long; funding paid by longs, but under the threshold: no.
        w.ctx = ctx(0, &[("ETH", 0.00001, 1e9, 1e8)]);
        assert_eq!(w.read("fade-p10-80-fund-L", "ETH", 0).side, 0.0);
        w.ctx = ctx(0, &[("ETH", 0.00002, 1e9, 1e8)]);
        assert_eq!(w.read("fade-p10-80-fund-L", "ETH", 0).side, -1.0);
        // Funding against the crowd: no.
        w.ctx = ctx(0, &[("ETH", -0.00002, 1e9, 1e8)]);
        assert_eq!(w.read("fade-p10-80-fund-L", "ETH", 0).side, 0.0);
        // Open interest: unknown until 4 h of reads, then +3% needed.
        let h = 3_600_000;
        w.ctx = ctx(h, &[("ETH", 0.00002, 1e9, 1e8)]);
        assert_eq!(w.read("fade-p10-80-fund-oi-L", "ETH", 2 * h).side, 0.0);
        w.ctx.update(5 * h, [(&"ETH".to_string(), 0.00002, 1e9, 1.02e8)].into_iter());
        assert_eq!(w.read("fade-p10-80-fund-oi-L", "ETH", 5 * h).side, 0.0);
        w.ctx.update(5 * h + 1, [(&"ETH".to_string(), 0.00002, 1e9, 1.04e8)].into_iter());
        assert!((w.ctx.oi_change_pct("ETH", 5 * h + 1, OI_WINDOW_S).unwrap() - 4.0).abs() < 1e-9);
        assert_eq!(w.read("fade-p10-80-fund-oi-L", "ETH", 5 * h + 1).side, -1.0);
    }

    #[test]
    fn whales_by_taker_wallet() {
        let t0 = 10 * 60_000;
        let mut w = World { whales: WhaleFlow::new(0), ..Default::default() };
        // A whale buying $1.2M in pieces over two minutes; a market maker churning both ways.
        for i in 0..12 {
            w.whales.push("SOL", "whale", t0 + i * 10_000, 100_000.0);
            w.whales.push("SOL", "mm", t0 + i * 10_000, if i % 2 == 0 { 900_000.0 } else { -900_000.0 });
        }
        w.whales.push("SOL", "small", t0, -300_000.0);
        let now = t0 + 3 * 60_000;
        let rd = w.read("w15m-1w-80-1m-L", "SOL", now);
        assert_eq!((rd.longs, rd.shorts, rd.side), (1, 0, 1.0));
        assert!((rd.buy_usd - 1.2e6).abs() < 1e-6);
        assert_eq!(w.read("fade-w15m-1w-80-1m-L", "SOL", now).side, -1.0);
        // $500k+: the seller counts too, 1.2M vs 0.3M = 80% that way: still long.
        let rd = w.read("w5m-1w-80-500k-L", "SOL", now);
        assert_eq!((rd.longs, rd.shorts), (1, 0)); // the $300k seller is under $500k
        let rd = w.read("w15m-2w-80-250k-M", "SOL", now);
        assert_eq!((rd.longs, rd.shorts, rd.side), (1, 1, 0.0)); // needs 2 whales that way
        // Outside the window, or before the flow covers it: nothing.
        assert_eq!(w.read("w5m-1w-80-500k-L", "SOL", now + 10 * 60_000).side, 0.0);
        w.whales.since_ms = t0;
        assert_eq!(w.read("w15m-1w-80-1m-L", "SOL", now).side, 0.0);
        // Pruned after the longest window.
        w.whales.push("BTC", "x", t0 + 2 * 3_600_000, 1.0);
        assert_eq!(w.whales.wallets(), 1);
    }

    fn prices(coins: &[(&str, f64, f64)], from_ms: u64, minutes: u64) -> Prices {
        // Each coin moves linearly from its first price to its last over `minutes`.
        let mut p = Prices::new(from_ms);
        for m in 0..=minutes {
            for (c, a, b) in coins {
                p.sample(c, from_ms + m * 60_000, a + (b - a) * m as f64 / minutes as f64);
            }
        }
        p
    }

    #[test]
    fn seeded_prices() {
        let mut p = Prices::new(100 * 60_000);
        p.sample("ETH", 100 * 60_000 + 5, 110.0);
        // Candles of minutes 0..=100: the one of minute 100 is already sampled, kept as sampled.
        let candles: Vec<(u64, f64)> = (0..=100).map(|m| (m * 60_000, 100.0 + m as f64 / 10.0)).collect();
        p.seed("ETH", &candles);
        assert_eq!(p.since_ms, 0);
        // An hour back from 100:00.005 is 40:00.005: the last price known then is minute 39's.
        assert!((p.ret("ETH", 100 * 60_000 + 5, 3600.0).unwrap() - (110.0 / 103.9 - 1.0)).abs() < 1e-12);
        assert_eq!(p.per_coin["ETH"].len(), 101);
    }

    #[test]
    fn trend_and_reversal() {
        let mut w = World::default();
        w.prices = prices(&[("SOL", 100.0, 101.5), ("ETH", 100.0, 100.5), ("DOGE", 100.0, 95.0)], 0, 240);
        // Minute 240's mid is the latest; 4 h back from 241:00 the last known is minute 0's.
        let now = 241 * 60_000;
        // Not among the traded coins: not read.
        assert_eq!(w.read("t60m-120bp-M", "SOL", now).side, 0.0);
        w.ctx = ctx(0, &[("SOL", 0.0, 1e9, 1.0), ("ETH", 0.0, 1e9, 1.0), ("DOGE", 0.0, 1e9, 1.0)]);
        // SOL: +1.5% over 4 h, 0.37% the last hour; DOGE -5% over 4 h.
        assert_eq!(w.read("t60m-120bp-M", "SOL", now).side, 0.0);
        let rd = w.read("t15m-60bp-M", "SOL", now);
        assert_eq!(rd.side, 0.0);
        assert!((rd.score - (101.5 / (100.0 + 1.5 * 225.0 / 240.0) - 1.0) * 1e4).abs() < 1e-6);
        assert_eq!(w.read("fade-t240m-330bp-L", "SOL", now).side, 0.0);
        assert_eq!(w.read("fade-t240m-330bp-L", "DOGE", now).side, 1.0); // -5%: bet on the bounce
        // Before sampling covers the window: nothing.
        w.prices.since_ms = 60 * 60_000;
        assert_eq!(w.read("fade-t240m-330bp-L", "DOGE", now).side, 0.0);
    }

    #[test]
    fn cross_momentum_against_btc() {
        let mut w = World::default();
        // BTC +1%; 12 coins from -5% to +6%.
        let mut coins: Vec<(String, f64, f64)> = vec![("BTC".into(), 100.0, 101.0)];
        for i in 0..12 {
            coins.push((format!("C{i:02}"), 100.0, 100.0 + i as f64 - 5.0));
        }
        let refs: Vec<(&str, f64, f64)> = coins.iter().map(|(c, a, b)| (c.as_str(), *a, *b)).collect();
        w.prices = prices(&refs, 0, 60);
        let rows: Vec<(&str, f64, f64, f64)> = coins.iter().map(|(c, ..)| (c.as_str(), 0.0, 1e9, 1.0)).collect();
        w.ctx = ctx(0, &rows);
        let now = 61 * 60_000;
        let side = |c: &str| w.read("x60m-top3-L", c, now).side;
        assert_eq!((side("C11"), side("C09"), side("C08")), (1.0, 1.0, 0.0));
        assert_eq!((side("C00"), side("C02"), side("C03")), (-1.0, -1.0, 0.0));
        assert_eq!(side("BTC"), 0.0);
        let rd = w.read("x60m-top3-L", "C11", now);
        assert!((rd.score - (0.06 - 0.01) * 1e4).abs() < 1e-6);
        assert_eq!(w.read("fade-x60m-top3-L", "C11", now).side, -1.0);
        // Too few coins ranked (under 4 x 3): nothing.
        let few: Vec<(&str, f64, f64, f64)> = rows[..8].to_vec();
        w.ctx = ctx(0, &few);
        assert_eq!(w.read("x60m-top3-L", "C06", now).side, 0.0);
    }

    #[test]
    fn clusters_of_stops_and_liquidations() {
        let mut w = World::default();
        w.prices = prices(&[("ETH", 2000.0, 2000.0)], 0, 1);
        let lv = |px: f64, usd: f64, dir: f64, who: &str| Level { px, usd, dir, who: who.into() };
        // Longs' stops / liquidations just below: $300k within 1%; a short's stop above: $50k.
        w.levels.insert("ETH".into(), vec![lv(1990.0, 200_000.0, -1.0, "a"), lv(1985.0, 100_000.0, -1.0, "b"),
                                           lv(2010.0, 50_000.0, 1.0, "c"), lv(1965.0, 5_000_000.0, -1.0, "far")]);
        let rd = w.read("sl1-250k-70-M", "ETH", 60_000);
        assert_eq!((rd.sell_usd, rd.buy_usd, rd.shorts, rd.longs), (300_000.0, 50_000.0, 2, 1));
        assert_eq!(rd.side, -1.0);
        assert_eq!(w.read("fade-sl1-250k-70-M", "ETH", 60_000).side, 1.0);
        // 2% band: the farther $5M is in ($1M needed one way); and a long's stop above the
        // price (stale: already through it) never counts.
        w.levels.get_mut("ETH").unwrap().push(lv(2020.0, 9_000_000.0, -1.0, "stale"));
        let rd = w.read("sl2-1m-70-L", "ETH", 60_000);
        assert_eq!((rd.sell_usd, rd.side), (5_300_000.0, -1.0));
    }

    #[test]
    fn trailing_and_break_even_stops() {
        let mut peak = 0.0;
        // Long at 100, stop 98.5 (1.5%): trail 1.5% behind the best price, never down.
        assert_eq!(follow_stop(1.0, 100.0, 98.5, 1.5, 1.5, 0.0, &mut peak, 99.0), 98.5);
        let s = follow_stop(1.0, 100.0, 98.5, 1.5, 1.5, 0.0, &mut peak, 104.0);
        assert!((s - 104.0 * 0.985).abs() < 1e-9 && peak == 104.0);
        assert!((follow_stop(1.0, 100.0, s, 1.5, 1.5, 0.0, &mut peak, 102.0) - s).abs() < 1e-12);
        // Short at 100, stop 101.5: break even once 1R (1.5) down.
        let mut peak = 0.0;
        assert_eq!(follow_stop(-1.0, 100.0, 101.5, 1.5, 0.0, 1.0, &mut peak, 98.6), 101.5);
        assert_eq!(follow_stop(-1.0, 100.0, 101.5, 1.5, 0.0, 1.0, &mut peak, 98.5), 100.0);
        assert_eq!(follow_stop(-1.0, 100.0, 100.0, 1.5, 0.0, 1.0, &mut peak, 99.9), 100.0);
        // Neither rule: unchanged, peak untouched.
        let mut peak = 0.0;
        assert_eq!(follow_stop(1.0, 100.0, 98.5, 1.5, 0.0, 0.0, &mut peak, 150.0), 98.5);
        assert_eq!(peak, 0.0);
        assert_eq!(var("v15m-2t-70-500k-M-tr").exit.tp_pct, NO_TP);
    }

    #[test]
    fn sizing() {
        // 1% risk at a 2% stop: half the equity; capped by what 10x leaves.
        assert!((notional(1000.0, 0.0, 2.0) - 500.0).abs() < 1e-9);
        assert!((notional(1000.0, 9800.0, 2.0) - 200.0).abs() < 1e-9);
        assert_eq!(notional(1000.0, 10500.0, 2.0), 0.0);
    }
}
