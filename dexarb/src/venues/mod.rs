//! The ten venue adapters, read-only. Each one converts its venue's own units into the `model` conventions.
//! What a venue does not publish is `None` / `Source::Unavailable`, never a made-up figure.

pub mod apex;
pub mod aster;
pub mod candles;
pub mod edgex;
pub mod extended;
pub mod gmx;
pub mod grvt;
pub mod hl;
pub mod lighter;
pub mod pacifica;
pub mod standx;

use crate::http::Http;
use crate::model::*;
use anyhow::Result;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum V {
    Hyperliquid,
    Lighter,
    Extended,
    Aster,
    Grvt,
    Apex,
    Edgex,
    Pacifica,
    Standx,
    Gmx,
}

pub const ALL: [V; 10] = [
    V::Hyperliquid,
    V::Lighter,
    V::Extended,
    V::Aster,
    V::Grvt,
    V::Apex,
    V::Edgex,
    V::Pacifica,
    V::Standx,
    V::Gmx,
];

impl V {
    pub fn name(self) -> &'static str {
        match self {
            V::Hyperliquid => "Hyperliquid",
            V::Lighter => "Lighter",
            V::Extended => "Extended",
            V::Aster => "Aster",
            V::Grvt => "GRVT",
            V::Apex => "ApeX",
            V::Edgex => "edgeX",
            V::Pacifica => "Pacifica",
            V::Standx => "StandX",
            V::Gmx => "GMX",
        }
    }

    pub fn from_name(s: &str) -> Option<V> {
        ALL.into_iter().find(|v| v.name().eq_ignore_ascii_case(s))
    }

    /// An order book venue (GMX: a pool priced by its oracle).
    pub fn has_book(self) -> bool {
        self != V::Gmx
    }

    pub async fn instruments(self, h: &Http) -> Result<Vec<Instrument>> {
        match self {
            V::Hyperliquid => hl::instruments(h).await,
            V::Lighter => lighter::instruments(h).await,
            V::Extended => extended::instruments(h).await,
            V::Aster => aster::instruments(h).await,
            V::Grvt => grvt::instruments(h).await,
            V::Apex => apex::instruments(h).await,
            V::Edgex => edgex::instruments(h).await,
            V::Pacifica => pacifica::instruments(h).await,
            V::Standx => standx::instruments(h).await,
            V::Gmx => gmx::instruments(h).await,
        }
    }

    pub async fn book(self, h: &Http, i: &Instrument) -> Result<Book> {
        match self {
            V::Hyperliquid => hl::book(h, i).await,
            V::Lighter => lighter::book(h, i).await,
            V::Extended => extended::book(h, i).await,
            V::Aster => aster::book(h, i).await,
            V::Grvt => grvt::book(h, i).await,
            V::Apex => apex::book(h, i).await,
            V::Edgex => edgex::book(h, i).await,
            V::Pacifica => pacifica::book(h, i).await,
            V::Standx => standx::book(h, i).await,
            V::Gmx => gmx::book(h, i).await,
        }
    }

    /// The current funding of the given instruments (some venues answer for all at once, some one by one).
    pub async fn funding_now(self, h: &Http, ins: &[Instrument]) -> Result<Vec<FundingNow>> {
        match self {
            V::Hyperliquid => hl::funding_now(h).await,
            V::Lighter => lighter::funding_now(h, ins).await,
            V::Extended => extended::funding_now(h).await,
            V::Aster => aster::funding_now(h, ins).await,
            V::Grvt => per_symbol(h, ins, grvt::funding_one).await,
            V::Apex => per_symbol(h, ins, apex::funding_one).await,
            V::Edgex => per_symbol(h, ins, edgex::funding_one).await,
            V::Pacifica => pacifica::funding_now(h).await,
            V::Standx => per_symbol(h, ins, standx::funding_one).await,
            V::Gmx => gmx::funding_now(h).await,
        }
    }

    pub async fn funding_history(
        self,
        h: &Http,
        i: &Instrument,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<FundingPoint>> {
        let mut v = match self {
            V::Hyperliquid => hl::history(h, i, from_ms, to_ms).await?,
            V::Lighter => lighter::history(h, i, from_ms, to_ms).await?,
            V::Extended => extended::history(h, i, from_ms, to_ms).await?,
            V::Aster => aster::history(h, i, from_ms, to_ms).await?,
            V::Grvt => grvt::history(h, i, from_ms, to_ms).await?,
            V::Apex => apex::history(h, i, from_ms, to_ms).await?,
            V::Edgex => edgex::history(h, i, from_ms, to_ms).await?,
            V::Pacifica => pacifica::history(h, i, from_ms, to_ms).await?,
            V::Standx => standx::history(h, i, from_ms, to_ms).await?,
            V::Gmx => {
                anyhow::bail!("GMX: no funding history in its REST API (on-chain events only)")
            }
        };
        v.retain(|p| p.ts_ms >= from_ms && p.ts_ms <= to_ms && p.rate_h.is_finite());
        v.sort_by_key(|p| p.ts_ms);
        v.dedup_by_key(|p| p.ts_ms);
        Ok(v)
    }
}

async fn per_symbol<'a, F, Fut>(
    h: &'a Http,
    ins: &'a [Instrument],
    one: F,
) -> Result<Vec<FundingNow>>
where
    F: Fn(&'a Http, &'a Instrument) -> Fut,
    Fut: std::future::Future<Output = Result<FundingNow>> + 'a,
{
    let mut out = Vec::new();
    for chunk in ins.chunks(8) {
        let r = futures::future::join_all(chunk.iter().map(|i| one(h, i))).await;
        out.extend(r.into_iter().filter_map(|x| x.ok()));
    }
    Ok(out)
}

/// The underlying and the multiplier from a venue symbol's base part: kPEPE / 1000PEPE / 1000000MOG / 1MBABYDOGE.
pub fn norm_base(raw: &str) -> (String, D) {
    let b = raw.trim().to_uppercase();
    let b = b
        .trim_end_matches("-PERP")
        .trim_end_matches("_PERP")
        .to_string();
    for (p, m) in [("1000000", 1_000_000i64), ("1000", 1000), ("1M", 1_000_000)] {
        if let Some(rest) = b.strip_prefix(p) {
            if rest.len() >= 2 && rest.chars().all(|c| c.is_ascii_alphabetic()) {
                return (rest.to_string(), D::from(m));
            }
        }
    }
    // Hyperliquid's k-prefixed (kPEPE, kBONK, ...): the original raw symbol has a lower-case k
    if let Some(rest) = raw.strip_prefix('k') {
        if rest.len() >= 2
            && rest
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            return (rest.to_string(), D::from(1000));
        }
    }
    (b, D::ONE)
}

/// A book side from [[price, size], ...] or [{price|px|p, size|sz|qty|a}, ...].
pub fn side(v: &Value) -> Vec<(D, D)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|l| {
            let (p, q) = if let Some(a) = l.as_array() {
                (a.first()?.clone(), a.get(1)?.clone())
            } else {
                let p = ["price", "px", "p"]
                    .iter()
                    .find_map(|k| l.get(*k))
                    .cloned()?;
                let q = ["size", "sz", "qty", "a", "quantity"]
                    .iter()
                    .find_map(|k| l.get(*k))
                    .cloned()?;
                (p, q)
            };
            let p = d(&crate::http::s(&p))?;
            let q = d(&crate::http::s(&q))?;
            (p > D::ZERO && q > D::ZERO).then_some((p, q))
        })
        .collect()
}

pub fn sort_book(mut b: Book) -> Book {
    b.bids.sort_by(|a, c| c.0.cmp(&a.0));
    b.asks.sort_by(|a, c| a.0.cmp(&c.0));
    b
}

pub fn fees(taker: &str, maker: &str, src: Source) -> Fees {
    Fees {
        taker: d(taker).unwrap_or_default(),
        maker: d(maker).unwrap_or_default(),
        source: src,
    }
}

pub fn inst(venue: V, symbol: &str, base_raw: &str) -> Instrument {
    let (base, multiplier) = norm_base(base_raw);
    Instrument {
        venue: venue.name().into(),
        symbol: symbol.into(),
        id: None,
        base,
        multiplier,
        quote: "USD".into(),
        collateral: "USDC".into(),
        category: String::new(),
        min_size: None,
        max_leverage: None,
        fees: Fees {
            taker: D::ZERO,
            maker: D::ZERO,
            source: Source::Unavailable,
        },
        funding_interval_h: 1.0,
        funding_cap_h: None,
        active: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bases() {
        assert_eq!(norm_base("kPEPE"), ("PEPE".into(), D::from(1000)));
        assert_eq!(norm_base("1000PEPE"), ("PEPE".into(), D::from(1000)));
        assert_eq!(norm_base("1000000MOG"), ("MOG".into(), D::from(1_000_000)));
        assert_eq!(norm_base("BTC"), ("BTC".into(), D::ONE));
        assert_eq!(norm_base("KAITO"), ("KAITO".into(), D::ONE));
        assert_eq!(norm_base("1INCH"), ("1INCH".into(), D::ONE));
    }
}
