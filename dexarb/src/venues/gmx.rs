//! GMX v2 (Arbitrum): a pool / oracle market, not a book. The execution model: a long opens at the oracle's max price, a
//! short at its min (the pair is the "book": one level, its size capped by the pool's available liquidity); the position
//! fee 0.05% (balance-improving) / 0.07% (third-party docs; verify docs.gmx.io); the price impact is charged at the close
//! from the open-interest imbalance - NOT modelled here (it needs the on-chain impact factors): the estimate is incomplete.
//! Funding and borrowing: the API's `fundingRateLong/Short`, `borrowingRateLong/Short` (scaled 1e30; read as an ANNUAL
//! fraction, + = that side pays - inferred from the OI imbalance's sign, verify), separate for each side.
use super::*;
use crate::http::s;

const API: &str = "https://arbitrum-api.gmxinfra.io";

fn e30(x: &serde_json::Value) -> f64 {
    s(x).parse::<f64>().unwrap_or(0.0) / 1e30
}

pub async fn instruments(h: &Http) -> Result<Vec<Instrument>> {
    let r = h.get(&format!("{API}/markets/info")).await?;
    // one market per index: the deepest pool
    let mut best: std::collections::HashMap<String, (f64, Instrument)> = Default::default();
    for m in r["markets"].as_array().into_iter().flatten() {
        if m["isListed"] != serde_json::json!(true) {
            continue;
        }
        let name = s(&m["name"]);
        let Some((base, _)) = name.split_once('/') else {
            continue;
        };
        if name.contains("SWAP") || base.is_empty() {
            continue;
        }
        let liq = e30(&m["availableLiquidityLong"]).min(e30(&m["availableLiquidityShort"]));
        let mut i = inst(V::Gmx, &name, base);
        i.id = Some(s(&m["indexToken"]));
        i.collateral = "USDC / pool tokens".into();
        i.fees = fees("0.0007", "0.0005", Source::Docs("GMX v2 position fee 0.05% balance-improving / 0.07% otherwise (third-party; verify) + price impact at the close (not modelled)".into()));
        i.category = "pool".into();
        if best.get(&i.base).is_none_or(|(l, _)| liq > *l) {
            best.insert(i.base.clone(), (liq, i));
        }
    }
    Ok(best.into_values().map(|x| x.1).collect())
}

async fn decimals(h: &Http) -> std::collections::HashMap<String, i32> {
    h.get(&format!("{API}/tokens"))
        .await
        .ok()
        .map(|r| {
            r["tokens"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|t| {
                    (
                        s(&t["address"]).to_lowercase(),
                        t["decimals"].as_i64().unwrap_or(18) as i32,
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The oracle's min / max as a one-level book (size: the pool's liquidity on that side, in index units).
pub async fn book(h: &Http, i: &Instrument) -> Result<Book> {
    let (u1, u2) = (
        format!("{API}/prices/tickers"),
        format!("{API}/markets/info"),
    );
    let (t, dec, mk) = tokio::join!(h.get(&u1), decimals(h), h.get(&u2));
    let (t, mk) = (t?, mk?);
    let addr = i.id.clone().unwrap_or_default().to_lowercase();
    let x = t
        .as_array()
        .into_iter()
        .flatten()
        .find(|x| s(&x["tokenAddress"]).to_lowercase() == addr)
        .ok_or_else(|| anyhow::anyhow!("GMX {}: no ticker", i.symbol))?;
    let dd = *dec.get(&addr).unwrap_or(&18);
    let scale = 10f64.powi(30 - dd);
    let (lo, hi) = (
        s(&x["minPrice"]).parse::<f64>().unwrap_or(0.0) / scale,
        s(&x["maxPrice"]).parse::<f64>().unwrap_or(0.0) / scale,
    );
    let m = mk["markets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| s(&m["name"]) == i.symbol);
    let (ll, ls) = m.map_or((0.0, 0.0), |m| {
        (
            e30(&m["availableLiquidityLong"]),
            e30(&m["availableLiquidityShort"]),
        )
    });
    let mid = (lo + hi) / 2.0;
    if mid <= 0.0 {
        anyhow::bail!("GMX {}: no price", i.symbol);
    }
    Ok(Book {
        bids: vec![(df(lo), df(ls / mid))],
        asks: vec![(df(hi), df(ll / mid))],
        ts_exchange: x["timestamp"].as_i64().map(|t| t * 1000),
        ts_recv: now_ms(),
    })
}

pub async fn funding_now(h: &Http) -> Result<Vec<FundingNow>> {
    let r = h.get(&format!("{API}/markets/info")).await?;
    Ok(r["markets"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let y = 8760.0;
            let (fl, fs) = (
                e30(&m["fundingRateLong"]) / y,
                e30(&m["fundingRateShort"]) / y,
            );
            let (bl, bs) = (
                e30(&m["borrowingRateLong"]) / y,
                e30(&m["borrowingRateShort"]) / y,
            );
            FundingNow {
                symbol: s(&m["name"]),
                rate_h: fl,
                predicted_h: None,
                next_ms: None,
                interval_h: 0.0,
                long_cost_h: Some(fl + bl),
                short_cost_h: Some(fs + bs),
            }
        })
        .collect())
}
