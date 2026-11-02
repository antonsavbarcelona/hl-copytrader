//! Executable prices: a market order walks the book. Never the mid or the last as a fill price.

use crate::model::*;

/// The result of walking one side of the book for `usd` of notional.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Fill {
    pub usd: D,
    /// The contract units filled and the average price per CONTRACT unit.
    pub qty: D,
    pub vwap: D,
    /// The best price on that side and the impact vs it (fraction, >= 0).
    pub best: D,
    pub impact: f64,
    /// The whole size was there (false: not executable at this size on this snapshot).
    pub complete: bool,
    pub levels: usize,
}

/// Buy (walk the asks) or sell (walk the bids) `usd` of notional.
pub fn walk(book: &Book, buy: bool, usd: D) -> Option<Fill> {
    let side = if buy { &book.asks } else { &book.bids };
    let best = side.first()?.0;
    let (mut left, mut qty, mut cost, mut n) = (usd, D::ZERO, D::ZERO, 0usize);
    for (p, q) in side {
        if left <= D::ZERO {
            break;
        }
        let lvl = *p * *q;
        let take = if lvl <= left { *q } else { left / *p };
        qty += take;
        cost += take * *p;
        left -= take * *p;
        n += 1;
    }
    if qty <= D::ZERO {
        return None;
    }
    let vwap = cost / qty;
    let impact = (f(vwap) / f(best) - 1.0).abs();
    Some(Fill {
        usd: cost,
        qty,
        vwap,
        best,
        impact,
        complete: left <= usd * D::new(1, 6),
        levels: n,
    })
}

/// Walk for a given contract quantity (the hedge leg must match the other leg's base units).
pub fn walk_qty(book: &Book, buy: bool, qty: D) -> Option<Fill> {
    let side = if buy { &book.asks } else { &book.bids };
    let best = side.first()?.0;
    let (mut left, mut got, mut cost, mut n) = (qty, D::ZERO, D::ZERO, 0usize);
    for (p, q) in side {
        if left <= D::ZERO {
            break;
        }
        let take = (*q).min(left);
        got += take;
        cost += take * *p;
        left -= take;
        n += 1;
    }
    if got <= D::ZERO {
        return None;
    }
    let vwap = cost / got;
    Some(Fill {
        usd: cost,
        qty: got,
        vwap,
        best,
        impact: (f(vwap) / f(best) - 1.0).abs(),
        complete: left <= qty * D::new(1, 6),
        levels: n,
    })
}

pub fn mid(book: &Book) -> Option<D> {
    Some((book.bids.first()?.0 + book.asks.first()?.0) / D::TWO)
}

pub fn spread(book: &Book) -> Option<f64> {
    let (b, a) = (book.bids.first()?.0, book.asks.first()?.0);
    Some(f(a - b) / f((a + b) / D::TWO))
}

pub fn depth_usd(book: &Book, buy: bool, within: f64) -> f64 {
    let side = if buy { &book.asks } else { &book.bids };
    let Some(best) = side.first().map(|x| f(x.0)) else {
        return 0.0;
    };
    side.iter()
        .take_while(|(p, _)| (f(*p) / best - 1.0).abs() <= within)
        .map(|(p, q)| f(*p * *q))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn b() -> Book {
        Book {
            bids: vec![(D::from(99), D::from(1)), (D::from(98), D::from(2))],
            asks: vec![(D::from(101), D::from(1)), (D::from(102), D::from(2))],
            ts_exchange: None,
            ts_recv: 0,
        }
    }
    #[test]
    fn vwap_walks() {
        let x = walk(&b(), true, D::from(203)).unwrap();
        assert_eq!(x.qty, D::from(2));
        assert_eq!(x.vwap, D::new(1015, 1));
        assert!(x.complete);
        let y = walk(&b(), false, D::from(10_000)).unwrap();
        assert!(!y.complete);
        let z = walk_qty(&b(), false, D::from(2)).unwrap();
        assert_eq!(z.vwap, D::new(985, 1));
    }
}
