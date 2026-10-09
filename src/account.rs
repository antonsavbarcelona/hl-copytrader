//! One paper copy account: cash, perp positions with their average entry, realized PnL,
//! fees and funding. Fills are taker fills against an L2 book snapshot.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Pos {
    /// Signed size (+ long).
    pub size: f64,
    pub entry: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Account {
    pub start: f64,
    /// Starting cash + realized PnL - fees + funding.
    pub cash: f64,
    pub positions: HashMap<String, Pos>,
    pub realized: f64,
    pub fees: f64,
    pub funding: f64,
    pub fills: u64,
    pub volume: f64,
    pub liquidated: bool,
}

impl Account {
    pub fn new(start: f64) -> Self {
        Self { start, cash: start, ..Default::default() }
    }

    pub fn size(&self, coin: &str) -> f64 {
        self.positions.get(coin).map(|p| p.size).unwrap_or(0.0)
    }

    /// Applies a fill of `delta` (signed) at `px`; returns the realized PnL.
    pub fn fill(&mut self, coin: &str, delta: f64, px: f64, fee: f64) -> f64 {
        let p = self.positions.entry(coin.to_string()).or_default();
        let mut realized = 0.0;
        if p.size != 0.0 && p.size.signum() != delta.signum() {
            // Reducing (and maybe flipping).
            let closed = delta.abs().min(p.size.abs());
            realized = closed * (px - p.entry) * p.size.signum();
            let rest = delta.abs() - closed;
            p.size += delta.signum() * closed;
            if p.size.abs() < 1e-12 {
                p.size = 0.0;
            }
            if rest > 1e-12 {
                p.size = delta.signum() * rest;
                p.entry = px;
            }
        } else {
            let new = p.size + delta;
            p.entry = if new.abs() > 1e-12 { (p.entry * p.size.abs() + px * delta.abs()) / new.abs() } else { 0.0 };
            p.size = new;
        }
        if p.size == 0.0 {
            self.positions.remove(coin);
        }
        self.realized += realized;
        self.fees += fee;
        self.cash += realized - fee;
        self.fills += 1;
        self.volume += delta.abs() * px;
        realized
    }

    pub fn unrealized(&self, marks: &dyn Fn(&str) -> Option<f64>) -> f64 {
        self.positions.iter().map(|(c, p)| marks(c).map(|m| p.size * (m - p.entry)).unwrap_or(0.0)).sum()
    }

    pub fn equity(&self, marks: &dyn Fn(&str) -> Option<f64>) -> f64 {
        self.cash + self.unrealized(marks)
    }
}

/// Taker fill of `delta` (signed size) against a book side, best first: (filled size, avg px).
pub fn walk(levels: &[(f64, f64)], size: f64) -> (f64, f64) {
    let (mut got, mut cost) = (0.0, 0.0);
    for &(px, sz) in levels {
        if got >= size - 1e-12 {
            break;
        }
        let take = (size - got).min(sz);
        got += take;
        cost += take * px;
    }
    if got > 0.0 { (got, cost / got) } else { (0.0, 0.0) }
}

/// Size rounded toward zero to the coin's size decimals.
pub fn round_size(sz: f64, decimals: u32) -> f64 {
    let m = 10f64.powi(decimals as i32);
    // A hair up first: 0.00014 is 13.999... steps in binary, still 14 of them.
    let steps = sz * m;
    (steps + steps.signum() * 1e-9).trunc() / m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_add_reduce_flip() {
        let mut a = Account::new(1000.0);
        a.fill("BTC", 0.01, 100_000.0, 0.45);
        a.fill("BTC", 0.01, 110_000.0, 0.495);
        assert!((a.positions["BTC"].entry - 105_000.0).abs() < 1e-6);
        let r = a.fill("BTC", -0.03, 120_000.0, 1.62); // close 0.02 at +15k each, flip 0.01 short
        assert!((r - 300.0).abs() < 1e-6);
        assert!((a.size("BTC") + 0.01).abs() < 1e-12);
        assert_eq!(a.positions["BTC"].entry, 120_000.0);
        let marks = |_: &str| Some(110_000.0);
        assert!((a.unrealized(&marks) - 100.0).abs() < 1e-6);
        assert!((a.cash - (1000.0 + 300.0 - 0.45 - 0.495 - 1.62)).abs() < 1e-9);
    }

    #[test]
    fn walks_the_book() {
        let asks = [(10.0, 1.0), (11.0, 1.0)];
        let (got, avg) = walk(&asks, 1.5);
        assert_eq!(got, 1.5);
        assert!((avg - 10.333333333).abs() < 1e-6);
        assert_eq!(walk(&asks, 5.0).0, 2.0);
        assert_eq!(round_size(0.123456, 3), 0.123);
        assert_eq!(round_size(-0.123456, 3), -0.123);
        assert_eq!(round_size(14.0 / 100_000.0, 5), 0.00014);
        assert_eq!(round_size(0.0, 5), 0.0);
    }
}
