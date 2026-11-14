//! The market's candles, one way for every strategy: read from the info API (the exchange client `Exchange`, retried
//! with a backoff, or the paced `Api`), parsed once into `Candle`s; `closed` keeps the completed ones.

use anyhow::Result;
use serde_json::{Value, json};

use crate::api::{Api, now, num};
use crate::exchange::Exchange;

/// A candle: its open time (s), prices and volume (in the coin).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Candle {
    pub(crate) t: f64,
    pub(crate) o: f64,
    pub(crate) h: f64,
    pub(crate) l: f64,
    pub(crate) c: f64,
    pub(crate) v: f64,
}

impl Candle {
    /// The $ traded (the volume at the close).
    pub(crate) fn notional(&self) -> f64 {
        self.v * self.c
    }
}

/// An info API: the body sent, the reply's JSON; `weight` for a paced one.
pub(crate) trait Info {
    async fn ask(&self, body: Value, weight: u32) -> Result<Value>;
}

impl Info for Exchange {
    async fn ask(&self, body: Value, _weight: u32) -> Result<Value> {
        self.info(body).await
    }
}

impl Info for Api {
    async fn ask(&self, body: Value, weight: u32) -> Result<Value> {
        self.info(body, weight).await
    }
}

/// An interval's length (s): "1m", "5m", "15m", "1h", "4h", "1d", ...
pub(crate) fn step(interval: &str) -> f64 {
    let (n, unit) = interval.split_at(interval.len().saturating_sub(1));
    let n: f64 = n.parse().unwrap_or(1.0);
    n * match unit {
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86_400.0,
        "w" => 604_800.0,
        _ => 0.0,
    }
}

/// A coin's candles of `interval` opened from `start` (s) to now, oldest first, the forming one included; the API's
/// weight: 20 and one more a 60 candles.
pub(crate) async fn candles(src: &impl Info, coin: &str, interval: &str, start: f64) -> Result<Vec<Candle>> {
    let t = now();
    let n = ((t - start) / step(interval).max(1.0)).max(1.0);
    let v = src.ask(json!({"type": "candleSnapshot", "req": {"coin": coin, "interval": interval,
        "startTime": (start.max(0.0) * 1000.0) as u64, "endTime": (t * 1000.0) as u64}}), 20 + (n / 60.0).ceil() as u32).await?;
    Ok(parse(&v))
}

/// The API's candles as `Candle`s (none from a reply not a list).
pub(crate) fn parse(v: &Value) -> Vec<Candle> {
    v.as_array().into_iter().flatten()
        .map(|c| Candle { t: num(&c["t"]) / 1000.0, o: num(&c["o"]), h: num(&c["h"]), l: num(&c["l"]), c: num(&c["c"]), v: num(&c["v"]) })
        .collect()
}

/// The candles of `interval` completed at `t` (their close at or before it) with a price.
pub(crate) fn closed(v: Vec<Candle>, interval: &str, t: f64) -> Vec<Candle> {
    let s = step(interval);
    v.into_iter().filter(|x| x.t + s <= t && x.c > 0.0).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::FakeHl;

    #[test]
    fn the_steps_and_the_parse() {
        assert_eq!((step("1m"), step("5m"), step("1h"), step("4h"), step("1d"), step("1w"), step("x")), (60.0, 300.0, 3600.0, 14_400.0, 86_400.0, 604_800.0, 0.0));
        let v = json!([{"t": 3_600_000u64, "o": "1", "h": "2", "l": "0.5", "c": "1.5", "v": "10"}]);
        let c = parse(&v);
        assert_eq!(c, vec![Candle { t: 3600.0, o: 1.0, h: 2.0, l: 0.5, c: 1.5, v: 10.0 }]);
        assert_eq!(c[0].notional(), 15.0);
        assert!(parse(&json!({"error": "x"})).is_empty());
        let x = |t: f64, c: f64| Candle { t, o: c, h: c, l: c, c, v: 1.0 };
        assert_eq!(closed(vec![x(0.0, 1.0), x(3600.0, 0.0), x(7200.0, 2.0)], "1h", 7200.0), vec![x(0.0, 1.0)]);
    }

    #[tokio::test]
    async fn read_the_same_through_either_client() {
        let f = FakeHl::start(&[("MKA", 2, 10, 1.0)]).await;
        f.set_candles("MKA", "1h", &[(3600.0, 1.0, 2.0, 0.5, 1.5, 10.0)]);
        let a = candles(&f.venue().ex, "MKA", "1h", 0.0).await.unwrap();
        let b = candles(&f.api(), "MKA", "1h", 0.0).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 1);
    }
}
