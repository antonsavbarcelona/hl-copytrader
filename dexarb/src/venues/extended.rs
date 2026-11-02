//! Extended (Starknet). Funding hourly, a fraction (`f`, `marketStats.fundingRate`), + = longs pay. Fees: docs
//! (docs.extended.exchange, trading fees: taker 0.025%, maker 0%); the per-account fee endpoint needs a key.
use super::*;
use crate::http::{num, s};

const API: &str = "https://api.starknet.extended.exchange/api/v1";

pub async fn instruments(h: &Http) -> Result<Vec<Instrument>> {
    let r = h.get(&format!("{API}/info/markets")).await?;
    Ok(r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["type"] == "PERPETUAL" || m["type"].is_null())
        .map(|m| {
            let name = s(&m["name"]);
            let mut i = inst(V::Extended, &name, &s(&m["assetName"]));
            i.collateral = s(&m["collateralAssetName"]);
            i.category = s(&m["category"]);
            i.active = m["active"] == serde_json::json!(true) && m["status"] == "ACTIVE";
            i.min_size = d(&s(&m["tradingConfig"]["minOrderSize"]));
            i.max_leverage = d(&s(&m["tradingConfig"]["maxLeverage"]));
            i.fees = fees(
                "0.00025",
                "0",
                Source::Docs("docs.extended.exchange trading fees: taker 0.025%, maker 0%".into()),
            );
            i
        })
        .collect())
}

pub async fn book(h: &Http, i: &Instrument) -> Result<Book> {
    let r = h
        .get(&format!("{API}/info/markets/{}/orderbook", i.symbol))
        .await?;
    let mut b = sort_book(Book {
        bids: side(&r["data"]["bid"]),
        asks: side(&r["data"]["ask"]),
        ts_exchange: None,
        ts_recv: now_ms(),
    });
    b.bids.truncate(300);
    b.asks.truncate(300);
    Ok(b)
}

pub async fn funding_now(h: &Http) -> Result<Vec<FundingNow>> {
    let r = h.get(&format!("{API}/info/markets")).await?;
    Ok(r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let st = &m["marketStats"];
            FundingNow {
                symbol: s(&m["name"]),
                rate_h: num(&st["fundingRate"]),
                predicted_h: None,
                next_ms: st["nextFundingRate"].as_i64(),
                interval_h: 1.0,
                long_cost_h: None,
                short_cost_h: None,
            }
        })
        .collect())
}

pub async fn history(h: &Http, i: &Instrument, from: i64, to: i64) -> Result<Vec<FundingPoint>> {
    let mut out = Vec::new();
    let mut t = from;
    while t < to {
        let e = (t + 7 * 86_400_000).min(to);
        let r = h
            .get(&format!(
                "{API}/info/{}/funding?startTime={t}&endTime={e}",
                i.symbol
            ))
            .await?;
        out.extend(
            r["data"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|x| FundingPoint {
                    ts_ms: x["T"].as_i64().unwrap_or(0),
                    rate_h: num(&x["f"]),
                }),
        );
        t = e;
    }
    Ok(out)
}
