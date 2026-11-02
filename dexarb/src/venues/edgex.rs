//! edgeX (API v2; the v1 host serves the web app). Fees from the API's contract defaults. Funding: a fraction per the
//! contract's `fundingRateIntervalMin` (240 = 4 h), + = longs pay; `forecastFundingRate` is the next one.
use super::*;
use crate::http::{num, s};

const API: &str = "https://edgex-prod-v2.edgex.exchange/api/v2/public";

pub async fn instruments(h: &Http) -> Result<Vec<Instrument>> {
    let r = h.get(&format!("{API}/meta/getMetaData")).await?;
    let coins: std::collections::HashMap<String, String> = r["data"]["coinList"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| (s(&c["coinId"]), s(&c["coinName"])))
        .collect();
    Ok(r["data"]["contractList"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let name = s(&m["contractName"]);
            let base = coins.get(&s(&m["baseCoinId"])).cloned().unwrap_or_else(|| {
                name.trim_end_matches("USDC")
                    .trim_end_matches("USDT")
                    .to_string()
            });
            let mut i = inst(V::Edgex, &name, &base);
            i.id = Some(s(&m["contractId"]));
            i.quote = if name.ends_with("USDT") {
                "USDT".into()
            } else {
                "USDC".into()
            };
            i.collateral = "USDC".into();
            i.category = if m["isStock"] == serde_json::json!(true) {
                "stock".into()
            } else {
                String::new()
            };
            i.active = m["enableTrade"] == serde_json::json!(true)
                && m["enableDisplay"] == serde_json::json!(true);
            i.min_size = d(&s(&m["minOrderSize"]));
            i.max_leverage = d(&s(&m["displayMaxLeverage"]));
            i.fees = Fees {
                taker: d(&s(&m["defaultTakerFeeRate"])).unwrap_or_default(),
                maker: d(&s(&m["defaultMakerFeeRate"])).unwrap_or_default(),
                source: Source::Api,
            };
            let iv = num(&m["fundingRateIntervalMin"]).max(60.0) / 60.0;
            i.funding_interval_h = iv;
            i.funding_cap_h = Some(num(&m["fundingMaxRate"]) / iv);
            i
        })
        .collect())
}

pub async fn book(h: &Http, i: &Instrument) -> Result<Book> {
    let r = h
        .get(&format!(
            "{API}/quote/getDepth?contractId={}&level=200",
            i.id.as_deref().unwrap_or("")
        ))
        .await?;
    let x = &r["data"][0];
    Ok(sort_book(Book {
        bids: side(&x["bids"]),
        asks: side(&x["asks"]),
        ts_exchange: None,
        ts_recv: now_ms(),
    }))
}

pub async fn funding_one(h: &Http, i: &Instrument) -> Result<FundingNow> {
    let r = h
        .get(&format!(
            "{API}/funding/getLatestFundingRate?contractId={}",
            i.id.as_deref().unwrap_or("")
        ))
        .await?;
    let x = &r["data"][0];
    let n = num(&x["fundingRateIntervalMin"]).max(60.0) / 60.0;
    let fc = s(&x["forecastFundingRate"]);
    Ok(FundingNow {
        symbol: i.symbol.clone(),
        rate_h: num(&x["fundingRate"]) / n,
        predicted_h: (!fc.is_empty()).then(|| fc.parse::<f64>().unwrap_or(0.0) / n),
        next_ms: s(&x["fundingTime"])
            .parse::<i64>()
            .ok()
            .map(|t| t + (n * 3_600_000.0) as i64),
        interval_h: n,
        long_cost_h: None,
        short_cost_h: None,
    })
}

pub async fn history(h: &Http, i: &Instrument, from: i64, to: i64) -> Result<Vec<FundingPoint>> {
    let mut out = Vec::new();
    let mut offset = String::new();
    for _ in 0..100 {
        let url = format!(
            "{API}/funding/getFundingRatePage?contractId={}&size=100&filterSettlementFundingRate=true&filterBeginTimeInclusive={from}&filterEndTimeExclusive={to}{}",
            i.id.as_deref().unwrap_or(""),
            if offset.is_empty() {
                String::new()
            } else {
                format!("&offsetData={offset}")
            }
        );
        let r = h.get(&url).await?;
        let data = &r["data"];
        for x in data["dataList"].as_array().into_iter().flatten() {
            let n = num(&x["fundingRateIntervalMin"]).max(60.0) / 60.0;
            // the settled periods only (filterSettlementFundingRate; without it each minute's preview comes back)
            let t = s(&x["fundingTime"]).parse::<i64>().unwrap_or(0);
            out.push(FundingPoint {
                ts_ms: t,
                rate_h: num(&x["fundingRate"]) / n,
            });
        }
        offset = s(&data["nextPageOffsetData"]);
        let oldest = out.iter().map(|p| p.ts_ms).min().unwrap_or(0);
        if offset.is_empty() || oldest < from {
            break;
        }
    }
    Ok(out)
}
