//! StandX. Quote / collateral DUSD (StandX's own dollar: the conversion and its peg are a cost and a risk of their own).
//! Fees and the hourly funding cap from the API. Funding hourly, a fraction, + = longs pay.
use super::*;
use crate::http::{num, s};

const API: &str = "https://perps.standx.com/api";

pub async fn instruments(h: &Http) -> Result<Vec<Instrument>> {
    let r = h.get(&format!("{API}/query_symbol_info")).await?;
    Ok(r.as_array()
        .into_iter()
        .flatten()
        .map(|m| {
            let sym = s(&m["symbol"]);
            let mut i = inst(V::Standx, &sym, &s(&m["base_asset"]));
            i.quote = s(&m["quote_asset"]);
            i.collateral = "DUSD".into();
            i.active = m["status"] == "trading";
            i.min_size = d(&s(&m["min_order_qty"]));
            i.max_leverage = d(&s(&m["max_leverage"]));
            i.fees = Fees {
                taker: d(&s(&m["taker_fee"])).unwrap_or_default(),
                maker: d(&s(&m["maker_fee"])).unwrap_or_default(),
                source: Source::Api,
            };
            i.funding_cap_h = Some(num(&m["funding_rate_cap"]));
            i
        })
        .collect())
}

pub async fn book(h: &Http, i: &Instrument) -> Result<Book> {
    let r = h
        .get(&format!("{API}/query_depth_book?symbol={}", i.symbol))
        .await?;
    Ok(sort_book(Book {
        bids: side(&r["bids"]),
        asks: side(&r["asks"]),
        ts_exchange: None,
        ts_recv: now_ms(),
    }))
}

pub async fn funding_one(h: &Http, i: &Instrument) -> Result<FundingNow> {
    let t = now_ms();
    let v = history(h, i, t - 6 * 3_600_000, t).await?;
    let last = v
        .iter()
        .max_by_key(|p| p.ts_ms)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("StandX {}: no recent funding", i.symbol))?;
    Ok(FundingNow {
        symbol: i.symbol.clone(),
        rate_h: last.rate_h,
        predicted_h: None,
        next_ms: Some((t / 3_600_000 + 1) * 3_600_000),
        interval_h: 1.0,
        long_cost_h: None,
        short_cost_h: None,
    })
}

pub async fn history(h: &Http, i: &Instrument, from: i64, to: i64) -> Result<Vec<FundingPoint>> {
    let mut out = Vec::new();
    let mut t = from;
    while t < to {
        let e = (t + 10 * 86_400_000).min(to);
        let r = h
            .get(&format!(
                "{API}/query_funding_rates?symbol={}&start_time={t}&end_time={e}",
                i.symbol
            ))
            .await?;
        for x in r.as_array().into_iter().flatten() {
            let ts = parse_iso(&s(&x["time"])).unwrap_or(0);
            out.push(FundingPoint {
                ts_ms: ts,
                rate_h: num(&x["funding_rate"]),
            });
        }
        t = e;
    }
    Ok(out)
}

/// "2026-10-31T01:00:00Z" -> ms (UTC; no chrono needed).
pub fn parse_iso(x: &str) -> Option<i64> {
    let (date, time) = x.trim_end_matches('Z').split_once('T')?;
    let mut dp = date.split('-').map(|p| p.parse::<i64>());
    let (y, m, dd) = (dp.next()?.ok()?, dp.next()?.ok()?, dp.next()?.ok()?);
    let time = time.split('.').next()?;
    let mut tp = time.split(':').map(|p| p.parse::<i64>());
    let (hh, mi, ss) = (
        tp.next()?.ok()?,
        tp.next()?.ok()?,
        tp.next().and_then(|r| r.ok()).unwrap_or(0),
    );
    // days from the civil date (Howard Hinnant's algorithm)
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + dd - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400) + hh * 3600 + mi * 60 + ss) * 1000)
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso() {
        assert_eq!(super::parse_iso("1970-01-02T00:00:00Z"), Some(86_400_000));
        assert_eq!(
            super::parse_iso("2026-10-31T01:00:00Z"),
            Some(1_793_408_400_000)
        );
    }
}
