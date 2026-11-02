//! Lighter (zkLighter). Fees from the API (percent strings). Funding hourly: the history's `rate` is a PERCENT per hour with
//! a `direction` ("long": the longs pay - inferred, checked against `funding-rates`' sign); `funding-rates` (exchange
//! "lighter") is the same rate as a fraction per 8 h.
use super::*;
use crate::http::{num, s};

const API: &str = "https://mainnet.zklighter.elliot.ai/api/v1";

pub async fn instruments(h: &Http) -> Result<Vec<Instrument>> {
    let r = h.get(&format!("{API}/orderBooks")).await?;
    Ok(r["order_books"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["market_type"] == "perp")
        .map(|m| {
            let sym = s(&m["symbol"]);
            let mut i = inst(V::Lighter, &sym, &sym);
            i.id = Some(s(&m["market_id"]));
            let pct = |k: &str| d(&s(&m[k])).unwrap_or_default() / D::from(100);
            i.fees = Fees {
                taker: pct("taker_fee"),
                maker: pct("maker_fee"),
                source: Source::Api,
            };
            i.min_size = d(&s(&m["min_base_amount"]));
            i.active = m["status"] == "active" && m["is_frozen"] != serde_json::json!(true);
            i.funding_cap_h = Some(0.005);
            i
        })
        .collect())
}

pub async fn book(h: &Http, i: &Instrument) -> Result<Book> {
    let r = h
        .get(&format!(
            "{API}/orderBookOrders?market_id={}&limit=250",
            i.id.as_deref().unwrap_or("0")
        ))
        .await?;
    let agg = |v: &serde_json::Value| {
        let mut m: std::collections::BTreeMap<D, D> = Default::default();
        for o in v.as_array().into_iter().flatten() {
            if let (Some(p), Some(q)) = (d(&s(&o["price"])), d(&s(&o["remaining_base_amount"]))) {
                *m.entry(p).or_default() += q;
            }
        }
        m.into_iter().filter(|x| x.1 > D::ZERO).collect::<Vec<_>>()
    };
    Ok(sort_book(Book {
        bids: agg(&r["bids"]),
        asks: agg(&r["asks"]),
        ts_exchange: None,
        ts_recv: now_ms(),
    }))
}

pub async fn funding_now(h: &Http, ins: &[Instrument]) -> Result<Vec<FundingNow>> {
    let r = h.get(&format!("{API}/funding-rates")).await?;
    let by_id: std::collections::HashMap<String, &Instrument> = ins
        .iter()
        .filter_map(|i| Some((i.id.clone()?, i)))
        .collect();
    let next = (now_ms() / 3_600_000 + 1) * 3_600_000;
    Ok(r["funding_rates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|x| x["exchange"] == "lighter")
        .filter_map(|x| {
            let i = by_id.get(&s(&x["market_id"]))?;
            Some(FundingNow {
                symbol: i.symbol.clone(),
                rate_h: num(&x["rate"]) / 8.0,
                predicted_h: None,
                next_ms: Some(next),
                interval_h: 1.0,
                long_cost_h: None,
                short_cost_h: None,
            })
        })
        .collect())
}

pub async fn history(h: &Http, i: &Instrument, from: i64, to: i64) -> Result<Vec<FundingPoint>> {
    let mut out = Vec::new();
    let mut t = from / 1000;
    let end = to / 1000;
    while t < end {
        let e = (t + 30 * 86_400).min(end);
        let r = h.get(&format!("{API}/fundings?market_id={}&resolution=1h&start_timestamp={t}&end_timestamp={e}&count_back=0", i.id.as_deref().unwrap_or("0"))).await?;
        for x in r["fundings"].as_array().into_iter().flatten() {
            let sign = if x["direction"] == "short" { -1.0 } else { 1.0 };
            out.push(FundingPoint {
                ts_ms: x["timestamp"].as_i64().unwrap_or(0) * 1000,
                rate_h: sign * num(&x["rate"]) / 100.0,
            });
        }
        t = e;
    }
    Ok(out)
}
