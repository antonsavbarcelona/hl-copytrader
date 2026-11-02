//! The instrument registry: every venue's perps, normalised, and the assets that trade on two or more venues.
//! A match is never by the ticker alone: the same normalised base AND a mid price per unit of the underlying within
//! `PRICE_TOL` of the venues' median (a ticker collision - another token with the same symbol, a stock vs a coin - fails
//! it), both venues active, a USD-like quote.

use crate::http::Http;
use crate::model::*;
use crate::venues::{ALL, V};
use std::collections::BTreeMap;

pub const PRICE_TOL: f64 = 0.02;

pub struct Registry {
    pub by_venue: BTreeMap<V, Vec<Instrument>>,
    pub errors: Vec<(V, String)>,
}

impl Registry {
    pub async fn load(h: &Http, venues: &[V]) -> Registry {
        let r = futures::future::join_all(
            venues
                .iter()
                .map(|v| async move { (*v, v.instruments(h).await) }),
        )
        .await;
        let mut by_venue = BTreeMap::new();
        let mut errors = Vec::new();
        for (v, x) in r {
            match x {
                Ok(i) => {
                    by_venue.insert(v, i);
                }
                Err(e) => errors.push((v, format!("{e:#}"))),
            }
        }
        Registry { by_venue, errors }
    }

    /// The underlyings listed on two venues or more (by the normalised base; the price check comes with the quotes).
    pub fn shared(&self) -> BTreeMap<String, Vec<Instrument>> {
        let mut m: BTreeMap<String, Vec<Instrument>> = BTreeMap::new();
        for (_, ins) in &self.by_venue {
            for i in ins.iter().filter(|i| i.active && usd_like(&i.quote)) {
                m.entry(i.base.clone()).or_default().push(i.clone());
            }
        }
        // one instrument a venue per base (the first: e.g. edgeX's USDC over a USDT twin)
        for v in m.values_mut() {
            let mut seen = std::collections::HashSet::new();
            v.retain(|i| seen.insert(i.venue.clone()));
        }
        m.retain(|_, v| v.len() >= 2);
        m
    }
}

pub fn usd_like(q: &str) -> bool {
    matches!(
        q.to_uppercase().as_str(),
        "USD" | "USDC" | "USDT" | "DUSD" | "USD1" | "USDE"
    )
}

/// The price check: the venues whose mid per unit of the underlying is within `PRICE_TOL` of the median.
pub fn price_consistent(mids: &[(String, f64)]) -> Vec<String> {
    let mut xs: Vec<f64> = mids.iter().map(|x| x.1).filter(|x| *x > 0.0).collect();
    if xs.len() < 2 {
        return Vec::new();
    }
    xs.sort_by(|a, b| a.total_cmp(b));
    let med = xs[xs.len() / 2];
    mids.iter()
        .filter(|(_, m)| *m > 0.0 && (m / med - 1.0).abs() <= PRICE_TOL)
        .map(|x| x.0.clone())
        .collect()
}

pub fn venue_of(i: &Instrument) -> V {
    ALL.into_iter()
        .find(|v| v.name() == i.venue)
        .unwrap_or(V::Hyperliquid)
}
