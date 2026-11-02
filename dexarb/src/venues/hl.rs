//! Hyperliquid (main dex only; the HIP-3 dexes are other underlyings). Funding hourly, a fraction, + = longs pay.
use super::*;
use crate::http::{num, s};
use serde_json::json;

const URL: &str = "https://api.hyperliquid.xyz/info";

pub async fn instruments(h: &Http) -> Result<Vec<Instrument>> {
    let r = h.post(URL, &json!({"type": "meta"})).await?;
    Ok(r["universe"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|u| u["isDelisted"] != json!(true))
        .map(|u| {
            let name = s(&u["name"]);
            let mut i = inst(V::Hyperliquid, &name, &name);
            i.max_leverage = Some(D::from(u["maxLeverage"].as_i64().unwrap_or(1)));
            // base tier (docs.hyperliquid.xyz, fees): taker 0.045%, maker 0.015%
            i.fees = fees(
                "0.00045",
                "0.00015",
                Source::Docs("hyperliquid docs: base tier 0.045% / 0.015%".into()),
            );
            i.funding_cap_h = Some(0.04);
            i.category = if u["onlyIsolated"] == json!(true) {
                "isolated-only".into()
            } else {
                String::new()
            };
            i
        })
        .collect())
}

pub async fn book(h: &Http, i: &Instrument) -> Result<Book> {
    let r = h
        .post(URL, &json!({"type": "l2Book", "coin": i.symbol}))
        .await?;
    let lv = &r["levels"];
    Ok(sort_book(Book {
        bids: side(&lv[0]),
        asks: side(&lv[1]),
        ts_exchange: r["time"].as_i64(),
        ts_recv: now_ms(),
    }))
}

pub async fn funding_now(h: &Http) -> Result<Vec<FundingNow>> {
    let r = h.post(URL, &json!({"type": "metaAndAssetCtxs"})).await?;
    let (u, c) = (&r[0]["universe"], &r[1]);
    let next = (now_ms() / 3_600_000 + 1) * 3_600_000;
    Ok(u.as_array()
        .into_iter()
        .flatten()
        .zip(c.as_array().into_iter().flatten())
        .map(|(u, c)| FundingNow {
            symbol: s(&u["name"]),
            rate_h: num(&c["funding"]),
            predicted_h: None,
            next_ms: Some(next),
            interval_h: 1.0,
            long_cost_h: None,
            short_cost_h: None,
        })
        .collect())
}

pub async fn history(h: &Http, i: &Instrument, from: i64, to: i64) -> Result<Vec<FundingPoint>> {
    let mut out = Vec::new();
    let mut t = from;
    while t < to {
        let r = h
            .post(
                URL,
                &json!({"type": "fundingHistory", "coin": i.symbol, "startTime": t, "endTime": to}),
            )
            .await?;
        let v: Vec<FundingPoint> = r
            .as_array()
            .into_iter()
            .flatten()
            .map(|x| FundingPoint {
                ts_ms: x["time"].as_i64().unwrap_or(0),
                rate_h: num(&x["fundingRate"]),
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
