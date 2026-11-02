//! ApeX Omni. Funding hourly, a fraction, + = longs pay; the ticker has `predictedFundingRate`. Fees: docs / reviews
//! (base 0.05% taker, 0.02% maker; verify on apex.exchange).
use super::*;
use crate::http::{num, s};

const API: &str = "https://omni.apex.exchange/api/v3";

pub async fn instruments(h: &Http) -> Result<Vec<Instrument>> {
    let r = h.get(&format!("{API}/symbols")).await?;
    Ok(r["data"]["contractConfig"]["perpetualContract"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let sym = s(&m["symbol"]);
            let mut i = inst(V::Apex, &sym, &s(&m["baseTokenId"]));
            i.id = Some(s(&m["crossSymbolName"]));
            i.collateral = s(&m["settleAssetId"]);
            i.quote = i.collateral.clone();
            i.active = m["enableTrade"] == serde_json::json!(true)
                && m["enableOpenPosition"] == serde_json::json!(true);
            i.min_size = d(&s(&m["minOrderSize"]));
            i.max_leverage = d(&s(&m["displayMaxLeverage"]));
            i.fees = fees(
                "0.0005",
                "0.0002",
                Source::Docs("ApeX base 0.05% / 0.02% (reviews; verify)".into()),
            );
            i
        })
        .collect())
}

pub async fn book(h: &Http, i: &Instrument) -> Result<Book> {
    let r = h
        .get(&format!(
            "{API}/depth?symbol={}&limit=100",
            i.id.as_deref().unwrap_or("")
        ))
        .await?;
    Ok(sort_book(Book {
        bids: side(&r["data"]["b"]),
        asks: side(&r["data"]["a"]),
        ts_exchange: None,
        ts_recv: now_ms(),
    }))
}

pub async fn funding_one(h: &Http, i: &Instrument) -> Result<FundingNow> {
    let r = h
        .get(&format!(
            "{API}/ticker?symbol={}",
            i.id.as_deref().unwrap_or("")
        ))
        .await?;
    let x = &r["data"][0];
    let p = s(&x["predictedFundingRate"]);
    Ok(FundingNow {
        symbol: i.symbol.clone(),
        rate_h: num(&x["fundingRate"]),
        predicted_h: (!p.is_empty()).then(|| p.parse().unwrap_or(0.0)),
        next_ms: None,
        interval_h: 1.0,
        long_cost_h: None,
        short_cost_h: None,
    })
}

pub async fn history(h: &Http, i: &Instrument, from: i64, to: i64) -> Result<Vec<FundingPoint>> {
    let mut out = Vec::new();
    let mut end = to;
    for _ in 0..60 {
        let r = h.get(&format!("{API}/history-funding?symbol={}&limit=100&beginTimeInclusive={from}&endTimeExclusive={end}", i.symbol)).await?;
        let v: Vec<FundingPoint> = r["data"]["historyFunds"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|x| FundingPoint {
                ts_ms: x["fundingTime"].as_i64().unwrap_or(0),
                rate_h: num(&x["rate"]),
            })
            .collect();
        let Some(first) = v.iter().map(|p| p.ts_ms).min() else {
            break;
        };
        let n = v.len();
        out.extend(v);
        if n < 100 || first <= from || first >= end {
            break;
        }
        end = first;
    }
    Ok(out)
}
