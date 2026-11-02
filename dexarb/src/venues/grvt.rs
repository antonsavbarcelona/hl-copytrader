//! GRVT. Funding: PERCENT per the instrument's interval (`funding_interval_hours`, 1 / 4 / 8); the ticker's
//! `funding_rate_8h_curr` is normalised to 8 h. + = longs pay. Fees: docs (grvt.gitbook.io trading fees, level 1 futures:
//! taker 0.05%, maker 0.03%; third-party reviews quote maker rebates - verify per account).
use super::*;
use crate::http::{num, s};
use serde_json::json;

const API: &str = "https://market-data.grvt.io/full/v1";

pub async fn instruments(h: &Http) -> Result<Vec<Instrument>> {
    let r = h
        .post(
            &format!("{API}/all_instruments"),
            &json!({"is_active": true}),
        )
        .await?;
    Ok(r["result"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["kind"] == "PERPETUAL")
        .map(|m| {
            let sym = s(&m["instrument"]);
            let mut i = inst(V::Grvt, &sym, &s(&m["base"]));
            i.quote = s(&m["quote"]);
            i.collateral = "USDT".into();
            i.category = s(&m["asset_class"]);
            i.active = m["status"] == "ACTIVE" || m["status"].is_null();
            i.min_size = d(&s(&m["min_size"]));
            i.funding_interval_h = m["funding_interval_hours"].as_f64().unwrap_or(8.0);
            i.fees = fees(
                "0.0005",
                "0.0003",
                Source::Docs(
                    "grvt.gitbook.io trading fees, level 1: taker 0.05%, maker 0.03%".into(),
                ),
            );
            i
        })
        .collect())
}

pub async fn book(h: &Http, i: &Instrument) -> Result<Book> {
    let r = h
        .post(
            &format!("{API}/book"),
            &json!({"instrument": i.symbol, "depth": 100}),
        )
        .await?;
    let x = &r["result"];
    let ts = s(&x["event_time"])
        .parse::<i64>()
        .ok()
        .map(|ns| ns / 1_000_000);
    Ok(sort_book(Book {
        bids: side(&x["bids"]),
        asks: side(&x["asks"]),
        ts_exchange: ts,
        ts_recv: now_ms(),
    }))
}

pub async fn funding_one(h: &Http, i: &Instrument) -> Result<FundingNow> {
    let r = h
        .post(&format!("{API}/ticker"), &json!({"instrument": i.symbol}))
        .await?;
    let x = &r["result"];
    Ok(FundingNow {
        symbol: i.symbol.clone(),
        rate_h: num(&x["funding_rate_8h_curr"]) / 100.0 / 8.0,
        predicted_h: None,
        next_ms: s(&x["next_funding_time"])
            .parse::<i64>()
            .ok()
            .map(|ns| ns / 1_000_000),
        interval_h: i.funding_interval_h,
        long_cost_h: None,
        short_cost_h: None,
    })
}

pub async fn history(h: &Http, i: &Instrument, from: i64, to: i64) -> Result<Vec<FundingPoint>> {
    let mut out = Vec::new();
    let mut end = to;
    loop {
        let r = h.post(&format!("{API}/funding"), &json!({"instrument": i.symbol, "start_time": (from * 1_000_000).to_string(), "end_time": (end * 1_000_000).to_string(), "limit": 1000})).await?;
        let v: Vec<FundingPoint> = r["result"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|x| {
                let n = x["funding_interval_hours"]
                    .as_f64()
                    .unwrap_or(i.funding_interval_h)
                    .max(1.0);
                FundingPoint {
                    ts_ms: s(&x["funding_time"]).parse::<i64>().unwrap_or(0) / 1_000_000,
                    rate_h: num(&x["funding_rate"]) / 100.0 / n,
                }
            })
            .collect();
        let Some(first) = v.iter().map(|p| p.ts_ms).min() else {
            break;
        };
        let more = v.len() >= 1000 && first > from;
        out.extend(v);
        if !more {
            break;
        }
        end = first - 1;
    }
    Ok(out)
}
