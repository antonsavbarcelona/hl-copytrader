//! Aster (Binance-style futures API). Funding per the symbol's interval (fundingInfo, 1 / 4 / 8 h), a fraction, + = longs
//! pay. Fees: maker 0% (docs.asterdex.com fees), taker 0.04% (third-party guides 2026; the docs list by product - verify).
use super::*;
use crate::http::{num, s};

const API: &str = "https://fapi.asterdex.com/fapi/v1";

async fn intervals(h: &Http) -> std::collections::HashMap<String, f64> {
    h.get(&format!("{API}/fundingInfo"))
        .await
        .ok()
        .and_then(|r| r.as_array().cloned())
        .into_iter()
        .flatten()
        .map(|x| {
            (
                s(&x["symbol"]),
                x["fundingIntervalHours"].as_f64().unwrap_or(8.0),
            )
        })
        .collect()
}

pub async fn instruments(h: &Http) -> Result<Vec<Instrument>> {
    let r = h.get(&format!("{API}/exchangeInfo")).await?;
    let iv = intervals(h).await;
    Ok(r["symbols"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["contractType"] == "PERPETUAL")
        .map(|m| {
            let sym = s(&m["symbol"]);
            let mut i = inst(V::Aster, &sym, &s(&m["baseAsset"]));
            i.quote = s(&m["quoteAsset"]);
            i.collateral = s(&m["marginAsset"]);
            i.active = m["status"] == "TRADING";
            i.min_size = m["filters"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|f| f["filterType"] == "LOT_SIZE")
                .and_then(|f| d(&s(&f["minQty"])));
            i.fees = fees(
                "0.0004",
                "0",
                Source::Docs(
                    "maker 0% docs.asterdex.com; taker 0.04% third-party 2026 (verify)".into(),
                ),
            );
            i.funding_interval_h = iv.get(&sym).copied().unwrap_or(8.0);
            i
        })
        .collect())
}

pub async fn book(h: &Http, i: &Instrument) -> Result<Book> {
    let r = h
        .get(&format!("{API}/depth?symbol={}&limit=100", i.symbol))
        .await?;
    Ok(sort_book(Book {
        bids: side(&r["bids"]),
        asks: side(&r["asks"]),
        ts_exchange: r["T"].as_i64(),
        ts_recv: now_ms(),
    }))
}

pub async fn funding_now(h: &Http, ins: &[Instrument]) -> Result<Vec<FundingNow>> {
    let r = h.get(&format!("{API}/premiumIndex")).await?;
    let iv: std::collections::HashMap<&str, f64> = ins
        .iter()
        .map(|i| (i.symbol.as_str(), i.funding_interval_h))
        .collect();
    Ok(r.as_array()
        .into_iter()
        .flatten()
        .map(|x| {
            let sym = s(&x["symbol"]);
            let n = iv.get(sym.as_str()).copied().unwrap_or(8.0);
            FundingNow {
                rate_h: num(&x["lastFundingRate"]) / n,
                symbol: sym,
                predicted_h: None,
                next_ms: x["nextFundingTime"].as_i64(),
                interval_h: n,
                long_cost_h: None,
                short_cost_h: None,
            }
        })
        .collect())
}

pub async fn history(h: &Http, i: &Instrument, from: i64, to: i64) -> Result<Vec<FundingPoint>> {
    let mut out = Vec::new();
    let mut t = from;
    let n = i.funding_interval_h.max(1.0);
    while t < to {
        let r = h
            .get(&format!(
                "{API}/fundingRate?symbol={}&startTime={t}&endTime={to}&limit=1000",
                i.symbol
            ))
            .await?;
        let v: Vec<FundingPoint> = r
            .as_array()
            .into_iter()
            .flatten()
            .map(|x| FundingPoint {
                ts_ms: x["fundingTime"].as_i64().unwrap_or(0),
                rate_h: num(&x["fundingRate"]) / n,
            })
            .collect();
        let Some(last) = v.last().map(|p| p.ts_ms) else {
            break;
        };
        out.extend(v);
        if last <= t {
            break;
        }
        t = last + 1;
    }
    Ok(out)
}
