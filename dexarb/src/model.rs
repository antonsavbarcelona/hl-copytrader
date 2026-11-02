//! The shared types. Prices and sizes are `Decimal` (exact); statistics are `f64`.
//!
//! Funding convention everywhere: `rate_h` is a FRACTION PER HOUR, positive = the LONGS PAY the shorts. Each venue's
//! adapter converts its own units (percent / fraction, 1 h / 4 h / 8 h / annual) and sign rules into this.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

pub type D = Decimal;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn d(s: &str) -> Option<D> {
    s.trim()
        .parse::<D>()
        .ok()
        .or_else(|| s.trim().parse::<f64>().ok().and_then(D::from_f64_retain))
}

pub fn df(x: f64) -> D {
    D::from_f64_retain(x).unwrap_or_default()
}

pub fn f(x: D) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    x.to_f64().unwrap_or(0.0)
}

/// Where a figure came from: the venue's API, its official docs (checked on the date), or not available.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Source {
    Api,
    Docs(String),
    Unavailable,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fees {
    pub taker: D,
    pub maker: D,
    pub source: Source,
}

/// A tradable perp on a venue, normalised.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instrument {
    pub venue: String,
    /// The venue's own symbol / id used in its API calls.
    pub symbol: String,
    /// The venue's numeric id where its API wants one (Lighter market_id, edgeX contractId).
    pub id: Option<String>,
    /// The underlying, normalised (kPEPE / 1000PEPE -> PEPE).
    pub base: String,
    /// Base units per contract unit (kPEPE = 1000 PEPE): a price per unit of the underlying = px / multiplier.
    pub multiplier: D,
    pub quote: String,
    pub collateral: String,
    pub category: String,
    pub min_size: Option<D>,
    pub max_leverage: Option<D>,
    pub fees: Fees,
    pub funding_interval_h: f64,
    /// The cap on the hourly funding (fraction), when published.
    pub funding_cap_h: Option<f64>,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Book {
    /// (price, size in the venue's contract units), best first.
    pub bids: Vec<(D, D)>,
    pub asks: Vec<(D, D)>,
    pub ts_exchange: Option<i64>,
    pub ts_recv: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundingNow {
    pub symbol: String,
    /// The current (or the period's applied) rate, normalised: fraction / hour, + = longs pay.
    pub rate_h: f64,
    /// The predicted next rate when the venue publishes one.
    pub predicted_h: Option<f64>,
    pub next_ms: Option<i64>,
    pub interval_h: f64,
    /// GMX: the long and the short sides differ (+ = that side pays, borrowing included).
    pub long_cost_h: Option<f64>,
    pub short_cost_h: Option<f64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FundingPoint {
    pub ts_ms: i64,
    pub rate_h: f64,
}

/// What an adapter really delivers (the venue audit).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Capabilities {
    pub instruments: bool,
    pub book: bool,
    pub funding_now: bool,
    pub funding_history: bool,
    pub predicted_funding: bool,
    pub fees_from_api: bool,
    pub notes: Vec<String>,
}
