//! Pacifica (Solana). Funding hourly, a fraction, + = longs pay; `next_funding` published. Fees: docs / reviews
//! (entry tier taker 0.04%, maker 0.015%; verify pacifica.gitbook.io trading fees).
use super::*;
use crate::http::{num, s};

const API: &str = "https://api.pacifica.fi/api/v1";

pub async fn instruments(h: &Http) -> Result<Vec<Instrument>> {
    let r = h.get(&format!("{API}/info")).await?;
    Ok(r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let sym = s(&m["symbol"]);
            let base = if m["base_asset"].is_string() {
                s(&m["base_asset"])
            } else {
                sym.clone()
            };
            let mut i = inst(V::Pacifica, &sym, &base);
            i.min_size = d(&s(&m["lot_size"]));
            i.max_leverage = m["max_leverage"].as_i64().map(D::from);
            i.category = if m["isolated_only"] == serde_json::json!(true) {
                "isolated-only".into()
            } else {
                String::new()
            };
            i.fees = fees(
                "0.0004",
                "0.00015",
                Source::Docs(
                    "Pacifica entry tier 0.04% / 0.015% (docs mirror + reviews; verify)".into(),
                ),
            );
            i
        })
        .collect())
}

pub async fn book(h: &Http, i: &Instrument) -> Result<Book> {
    let r = h.get(&format!("{API}/book?symbol={}", i.symbol)).await?;
    let l = &r["data"]["l"];
    Ok(sort_book(Book {
        bids: side(&l[0]),
        asks: side(&l[1]),
        ts_exchange: r["data"]["t"].as_i64(),
        ts_recv: now_ms(),
    }))
}

pub async fn funding_now(h: &Http) -> Result<Vec<FundingNow>> {
    let r = h.get(&format!("{API}/info/prices")).await?;
    let next = (now_ms() / 3_600_000 + 1) * 3_600_000;
    Ok(r["data"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|x| FundingNow {
            symbol: s(&x["symbol"]),
            rate_h: num(&x["funding"]),
            predicted_h: Some(num(&x["next_funding"])),
            next_ms: Some(next),
            interval_h: 1.0,
            long_cost_h: None,
            short_cost_h: None,
        })
        .collect())
}

pub async fn history(h: &Http, i: &Instrument, from: i64, to: i64) -> Result<Vec<FundingPoint>> {
    let mut out = Vec::new();
    let mut cursor = String::new();
    for _ in 0..40 {
        let url = format!(
            "{API}/funding_rate/history?symbol={}&limit=4000{}",
            i.symbol,
            if cursor.is_empty() {
                String::new()
            } else {
                format!("&cursor={cursor}")
            }
        );
        let r = h.get(&url).await?;
        let v: Vec<FundingPoint> = r["data"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|x| FundingPoint {
                ts_ms: x["created_at"].as_i64().unwrap_or(0),
                rate_h: num(&x["funding_rate"]),
            })
            .collect();
        let oldest = v.iter().map(|p| p.ts_ms).min().unwrap_or(0);
        out.extend(v);
        cursor = s(&r["next_cursor"]);
        if cursor.is_empty()
            || r["has_more"] == serde_json::json!(false)
            || oldest < from
            || oldest > to + 86_400_000 * 400
        {
            break;
        }
    }
    Ok(out)
}
