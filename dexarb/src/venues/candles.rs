//! Level B: 15-minute bars (close, volume) from the venues' own kline endpoints - the trade prices, NOT executable quotes
//! (no depth), for the basis' history. A bar with no volume carries a stale close: dropped (GRVT: the MID bars instead).
//! Lighter: its candlesticks endpoint answers 403 to an unauthenticated call (2026-11-03) - not available.

use super::*;
use crate::http::{num, s};
use serde_json::json;

pub const BAR_MS: i64 = 900_000;

#[derive(Clone, Debug)]
pub struct Bar {
    pub ts: i64,
    pub close: f64,
    pub vol: f64,
}

pub async fn candles(v: V, h: &Http, i: &Instrument, from: i64, to: i64) -> Result<Vec<Bar>> {
    let mut out: Vec<Bar> = Vec::new();
    match v {
        V::Hyperliquid => {
            let mut t = from;
            while t < to {
                let e = (t + 4000 * BAR_MS).min(to);
                let r = h.post("https://api.hyperliquid.xyz/info", &json!({"type": "candleSnapshot", "req": {"coin": i.symbol, "interval": "15m", "startTime": t, "endTime": e}})).await?;
                out.extend(r.as_array().into_iter().flatten().map(|x| Bar { ts: x["t"].as_i64().unwrap_or(0), close: num(&x["c"]), vol: num(&x["v"]) }));
                t = e;
            }
        }
        V::Aster => {
            let mut t = from;
            while t < to {
                let r = h.get(&format!("https://fapi.asterdex.com/fapi/v1/klines?symbol={}&interval=15m&startTime={t}&endTime={to}&limit=1500", i.symbol)).await?;
                let v: Vec<Bar> = r.as_array().into_iter().flatten().map(|x| Bar { ts: x[0].as_i64().unwrap_or(0), close: num(&x[4]), vol: num(&x[5]) }).collect();
                let Some(last) = v.last().map(|b| b.ts) else { break };
                out.extend(v);
                if last <= t {
                    break;
                }
                t = last + 1;
            }
        }
        V::Extended => {
            let mut end = to;
            for _ in 0..40 {
                let r = h.get(&format!("https://api.starknet.extended.exchange/api/v1/info/candles/{}/trades?interval=PT15M&limit=1000&endTime={end}", i.symbol)).await?;
                let v: Vec<Bar> = r["data"].as_array().into_iter().flatten().map(|x| Bar { ts: x["T"].as_i64().unwrap_or(0), close: num(&x["c"]), vol: num(&x["v"]) }).collect();
                let Some(first) = v.iter().map(|b| b.ts).min() else { break };
                out.extend(v);
                if first <= from || first >= end {
                    break;
                }
                end = first - 1;
            }
        }
        V::Grvt => {
            let mut end = to;
            for _ in 0..40 {
                let r = h.post("https://market-data.grvt.io/full/v1/kline", &json!({"instrument": i.symbol, "interval": "CI_15_M", "type": "MID", "start_time": (from * 1_000_000).to_string(), "end_time": (end * 1_000_000).to_string(), "limit": 1000})).await?;
                let v: Vec<Bar> = r["result"].as_array().into_iter().flatten().map(|x| Bar { ts: s(&x["open_time"]).parse::<i64>().unwrap_or(0) / 1_000_000, close: num(&x["close"]), vol: 1.0 }).collect();
                let Some(first) = v.iter().map(|b| b.ts).min() else { break };
                let n = v.len();
                out.extend(v);
                if n < 1000 || first <= from {
                    break;
                }
                end = first - 1;
            }
        }
        V::Apex => {
            let mut t = from;
            while t < to {
                let e = (t + 200 * BAR_MS).min(to);
                let r = h.get(&format!("https://omni.apex.exchange/api/v3/klines?symbol={}&interval=15&start={}&end={}&limit=200", i.id.as_deref().unwrap_or(""), t / 1000, e / 1000)).await?;
                let key = i.id.clone().unwrap_or_default();
                out.extend(r["data"][key.as_str()].as_array().into_iter().flatten().map(|x| Bar { ts: x["t"].as_i64().unwrap_or(0), close: num(&x["c"]), vol: num(&x["v"]) }));
                t = e;
            }
        }
        V::Edgex => {
            let mut offset = String::new();
            for _ in 0..30 {
                let r = h.get(&format!("https://edgex-prod-v2.edgex.exchange/api/v2/public/quote/getKline?contractId={}&klineType=MINUTE_15&priceType=LAST_PRICE&size=200&filterBeginKlineTimeInclusive={from}&filterEndKlineTimeExclusive={to}{}",
                    i.id.as_deref().unwrap_or(""), if offset.is_empty() { String::new() } else { format!("&offsetData={offset}") })).await?;
                let d = &r["data"];
                out.extend(d["dataList"].as_array().into_iter().flatten().map(|x| Bar { ts: s(&x["klineTime"]).parse().unwrap_or(0), close: num(&x["close"]), vol: num(&x["size"]).max(num(&x["value"])) }));
                offset = s(&d["nextPageOffsetData"]);
                if offset.is_empty() || out.iter().map(|b| b.ts).min().unwrap_or(0) <= from {
                    break;
                }
            }
        }
        V::Pacifica => {
            let mut t = from;
            while t < to {
                let e = (t + 900 * BAR_MS).min(to);
                let r = h.get(&format!("https://api.pacifica.fi/api/v1/kline?symbol={}&interval=15m&start_time={t}&end_time={e}", i.symbol)).await?;
                out.extend(r["data"].as_array().into_iter().flatten().map(|x| Bar { ts: x["t"].as_i64().unwrap_or(0), close: num(&x["c"]), vol: num(&x["v"]) }));
                t = e;
            }
        }
        V::Standx => {
            let r = h.get(&format!("https://perps.standx.com/api/kline/history?symbol={}&resolution=15&from={}&to={}", i.symbol, from / 1000, to / 1000)).await?;
            let (t, c, vv) = (&r["t"], &r["c"], &r["v"]);
            for k in 0..t.as_array().map_or(0, |a| a.len()) {
                out.push(Bar { ts: t[k].as_i64().unwrap_or(0) * 1000, close: num(&c[k]), vol: if vv.is_array() { num(&vv[k]) } else { 1.0 } });
            }
        }
        V::Lighter | V::Gmx => anyhow::bail!("{}: no public bars", v.name()),
    }
    out.retain(|b| b.ts >= from && b.ts <= to && b.close > 0.0 && b.vol > 0.0);
    out.sort_by_key(|b| b.ts);
    out.dedup_by_key(|b| b.ts);
    // the close per unit of the underlying
    let m = f(i.multiplier);
    for b in &mut out {
        b.close /= m;
    }
    Ok(out)
}
