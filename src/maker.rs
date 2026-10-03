//! Limit-order twins of the signal trades: every market (taker) signal trade gets two twins of
//! the same size that enter with a post-only limit order instead, so the two ways of trading
//! the same signals can be compared trade by trade (`signal_maker_trades`, by `parent_id`):
//!   - `limit`: the order waits up to `ENTRY_WAIT_S`; what is not filled by then is cancelled
//!     (a trade that got nothing is "not filled");
//!   - `limit+market`: the same, then the rest is taken at the book.
//!
//! The entry order sits at the most aggressive post-only price: one tick inside the opposite
//! best when the spread is wider than a tick, else at our side's best, and follows the price
//! when it moves away. It fills when a trade prints through its price, when trades at its price
//! use up the size resting ahead of it (the size at that level when it was placed, less what
//! the book later shows), or when the opposite side of the book reaches it. Once in, the take
//! profit is a resting limit order too (maker fee); stop, expiry and the traders turning close
//! at the book (taker), like the market trade.

use serde::{Deserialize, Serialize};

use crate::account::walk;
use crate::ws::Book;

/// Hyperliquid maker fee (base tier).
pub const MAKER_FEE: f64 = 0.00015;
/// An entry order waits this long.
pub const ENTRY_WAIT_S: f64 = 30.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// Unfilled at the deadline: cancelled.
    Limit,
    /// Unfilled at the deadline: the rest at the book.
    LimitThenMarket,
}

pub const MODES: [Mode; 2] = [Mode::Limit, Mode::LimitThenMarket];

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Limit => "limit",
            Mode::LimitThenMarket => "limit+market",
        }
    }
}

/// The price step of a perp on Hyperliquid at `px`: 5 significant figures, at most
/// 6 - szDecimals decimals, whole numbers always allowed.
pub fn tick(px: f64, sz_decimals: u32) -> f64 {
    let sig = 10f64.powi(px.log10().floor() as i32 - 4);
    let dec = 10f64.powi(-(6 - sz_decimals as i32));
    sig.max(dec).min(1.0)
}

/// The most aggressive post-only price for a buy (`side` +1) or sell (-1): one tick inside the
/// opposite best if the spread is wider than a tick, else our side's best.
pub fn maker_px(side: f64, book: &Book, tick: f64) -> Option<f64> {
    let (bid, ask) = (book.bids.first()?.0, book.asks.first()?.0);
    let wide = ask - bid > tick * 1.5;
    Some(match (side > 0.0, wide) {
        (true, true) => ask - tick,
        (true, false) => bid,
        (false, true) => bid + tick,
        (false, false) => ask,
    })
}

fn same_px(a: f64, b: f64) -> bool {
    (a - b).abs() <= a.abs() * 1e-9
}

/// A resting post-only order (simulated).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Resting {
    /// +1 buy, -1 sell.
    pub side: f64,
    pub px: f64,
    /// Size not filled yet.
    pub left: f64,
    /// Size resting ahead of it at its price.
    pub ahead: f64,
    /// Exchange time of the book it was placed on: trades up to then are in that book.
    #[serde(default)]
    pub since_ms: u64,
}

impl Resting {
    /// Placed at `px` behind what the book shows resting there.
    pub fn new(side: f64, px: f64, size: f64, book: &Book) -> Self {
        let ours = if side > 0.0 { &book.bids } else { &book.asks };
        let ahead = ours.iter().find(|l| same_px(l.0, px)).map(|l| l.1).unwrap_or(0.0);
        Self { side, px, left: size, ahead, since_ms: book.time_ms }
    }

    /// A trade printed at `time_ms` (`taker_buy`: its aggressor bought): the size of ours it
    /// filled.
    pub fn on_print(&mut self, taker_buy: bool, px: f64, sz: f64, time_ms: u64) -> f64 {
        // A buy of ours is filled by aggressive sells, a sell by aggressive buys; trades from
        // before it was placed (arriving late) do not count.
        if self.left <= 0.0 || (self.side > 0.0) == taker_buy || time_ms <= self.since_ms {
            return 0.0;
        }
        if (px - self.px) * self.side < 0.0 && !same_px(px, self.px) {
            // Through our price: everything at it, ours too, went first.
            return std::mem::take(&mut self.left);
        }
        if !same_px(px, self.px) {
            return 0.0;
        }
        self.ahead -= sz;
        if self.ahead >= 0.0 {
            return 0.0;
        }
        let f = self.left.min(-self.ahead);
        self.ahead = 0.0;
        self.left -= f;
        f
    }

    /// The book changed: the opposite side reaching our price fills us; what rests ahead of us
    /// is at most what the book shows at our price (cancels ahead of us move us up).
    pub fn on_book(&mut self, book: &Book) -> f64 {
        if self.left <= 0.0 {
            return 0.0;
        }
        let (ours, theirs) = if self.side > 0.0 { (&book.bids, &book.asks) } else { (&book.asks, &book.bids) };
        if theirs.first().is_some_and(|l| (l.0 - self.px) * self.side <= 0.0) {
            return std::mem::take(&mut self.left);
        }
        // Levels shown best first: our price is inside the shown depth if it is not past the last.
        if let Some(last) = ours.last() {
            let shown = ours.iter().find(|l| same_px(l.0, self.px)).map(|l| l.1);
            match shown {
                Some(sz) => self.ahead = self.ahead.min(sz),
                None if (self.px - last.0) * self.side >= 0.0 => self.ahead = 0.0,
                None => {}
            }
        }
        0.0
    }
}

/// One limit-order twin of a market signal trade.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MakerTrade {
    /// `<parent id>:<mode>`.
    pub id: String,
    pub parent: String,
    pub mode: Option<Mode>,
    pub coin: String,
    pub side: f64,
    /// The market trade's size: ours aims for the same.
    pub size: f64,
    pub placed: f64,
    /// The mid when the signal fired, and the market trade's entry.
    pub mid: f64,
    pub market_entry: f64,
    pub stop_pct: f64,
    pub tp_pct: f64,
    pub expires: f64,
    /// The variant account's equity at the open (for % of equity).
    pub equity: f64,
    pub sz_decimals: u32,
    /// The entry order while it works, until `deadline`.
    pub order: Option<Resting>,
    pub deadline: f64,
    pub requotes: u32,
    /// Entry fills: size, size x price, of it at the maker fee, fees.
    pub filled: f64,
    pub cost: f64,
    pub maker_filled: f64,
    pub entry_fee: f64,
    /// When the entry ended (filled, or the deadline).
    pub entered_at: Option<f64>,
    /// The take-profit order once in.
    pub tp_order: Option<Resting>,
    /// Exit fills: size, size x price, of it at the maker fee, fees, PnL before fees.
    pub exited: f64,
    pub exit_value: f64,
    pub exit_maker: f64,
    pub exit_fee: f64,
    pub realized: f64,
    pub closed_at: Option<f64>,
    pub exit_reason: Option<String>,
    pub reason: serde_json::Value,
}

impl MakerTrade {
    pub fn entry(&self) -> f64 {
        if self.filled > 0.0 { self.cost / self.filled } else { 0.0 }
    }

    pub fn stop(&self) -> f64 {
        self.entry() * (1.0 - self.side * self.stop_pct / 100.0)
    }

    pub fn tp(&self) -> f64 {
        self.entry() * (1.0 + self.side * self.tp_pct / 100.0)
    }

    /// Size held now.
    pub fn held(&self) -> f64 {
        self.filled - self.exited
    }

    pub fn pnl(&self) -> f64 {
        self.realized - self.entry_fee - self.exit_fee
    }

    /// Opens the entry order at the book.
    pub fn place(&mut self, book: &Book, at: f64) -> bool {
        let Some(mid) = book.mid() else { return false };
        let Some(px) = maker_px(self.side, book, tick(mid, self.sz_decimals)) else { return false };
        self.order = Some(Resting::new(self.side, px, self.size, book));
        self.deadline = at + ENTRY_WAIT_S;
        true
    }

    fn fill_entry(&mut self, sz: f64, px: f64, maker: bool) {
        if sz <= 0.0 {
            return;
        }
        self.filled += sz;
        self.cost += sz * px;
        self.entry_fee += sz * px * if maker { MAKER_FEE } else { crate::config::TAKER_FEE };
        if maker {
            self.maker_filled += sz;
        }
    }

    fn fill_exit(&mut self, sz: f64, px: f64, maker: bool) {
        let sz = sz.min(self.held());
        if sz <= 0.0 {
            return;
        }
        self.realized += sz * (px - self.entry()) * self.side;
        self.exited += sz;
        self.exit_value += sz * px;
        self.exit_fee += sz * px * if maker { MAKER_FEE } else { crate::config::TAKER_FEE };
        if maker {
            self.exit_maker += sz;
        }
    }

    /// Ends the entry: the take profit goes up, or with nothing filled the trade is over.
    fn end_entry(&mut self, book: &Book, at: f64) {
        self.order = None;
        self.entered_at = Some(at);
        if self.filled <= 0.0 {
            self.close(at, "not filled");
            return;
        }
        self.tp_order = Some(Resting::new(-self.side, self.tp(), self.filled, book));
    }

    fn close(&mut self, at: f64, why: &str) {
        self.order = None;
        self.tp_order = None;
        self.closed_at = Some(at);
        self.exit_reason = Some(why.to_string());
    }

    /// Closes what is held at the book (taker). Returns false if the book could not take it.
    pub fn close_at_book(&mut self, book: &Book, at: f64, why: &str) -> bool {
        if self.order.is_some() {
            // Still entering (expiry or the traders turning first): stop working the order.
            self.order = None;
            self.entered_at = Some(at);
        }
        let held = self.held();
        if held > 0.0 {
            let (got, px) = walk(if self.side > 0.0 { &book.bids } else { &book.asks }, held);
            if got <= 0.0 {
                return false;
            }
            self.fill_exit(got, px, false);
            if self.held() > 1e-12 {
                return false;
            }
        }
        self.close(at, if self.filled > 0.0 { why } else { "not filled" });
        true
    }

    /// A trade printed in its coin. Returns true when the trade changed.
    pub fn on_print(&mut self, taker_buy: bool, px: f64, sz: f64, time_ms: u64, book: Option<&Book>, at: f64) -> bool {
        if self.closed_at.is_some() {
            return false;
        }
        if let Some(o) = self.order.as_mut() {
            let (f, opx) = (o.on_print(taker_buy, px, sz, time_ms), o.px);
            if f > 0.0 {
                self.fill_entry(f, opx, true);
                if self.order.as_ref().is_some_and(|o| o.left <= 1e-12) {
                    match book {
                        Some(b) => self.end_entry(b, at),
                        None => self.end_entry(&Book::default(), at),
                    }
                }
                return true;
            }
            return false;
        }
        if let Some(o) = self.tp_order.as_mut() {
            let (f, opx) = (o.on_print(taker_buy, px, sz, time_ms), o.px);
            if f > 0.0 {
                self.fill_exit(f, opx, true);
                if self.held() <= 1e-12 {
                    self.close(at, "take profit");
                }
                return true;
            }
        }
        false
    }

    /// The book changed (or a tick): fills from the book, the order following the price, the
    /// entry deadline, and the stop. Returns true when the trade changed.
    pub fn on_book(&mut self, book: &Book, at: f64) -> bool {
        if self.closed_at.is_some() {
            return false;
        }
        let mut changed = false;
        if let Some(o) = self.order.as_mut() {
            let opx = o.px;
            let f = o.on_book(book);
            if f > 0.0 {
                self.fill_entry(f, opx, true);
                changed = true;
            }
            let left = self.order.as_ref().map(|o| o.left).unwrap_or(0.0);
            if left <= 1e-12 {
                self.end_entry(book, at);
                changed = true;
            } else if at >= self.deadline {
                if self.mode == Some(Mode::LimitThenMarket) {
                    let (got, px) = walk(if self.side > 0.0 { &book.asks } else { &book.bids }, left);
                    self.fill_entry(got, px, false);
                }
                self.end_entry(book, at);
                changed = true;
            } else if let Some(px) = book.mid().and_then(|m| maker_px(self.side, book, tick(m, self.sz_decimals))) {
                // The price moved away: follow it (the queue starts again).
                if (px - opx) * self.side > 0.0 && !same_px(px, opx) {
                    self.order = Some(Resting::new(self.side, px, left, book));
                    self.requotes += 1;
                    changed = true;
                }
            }
        }
        if let Some(o) = self.tp_order.as_mut() {
            let (f, opx) = (o.on_book(book), o.px);
            if f > 0.0 {
                self.fill_exit(f, opx, true);
                changed = true;
                if self.held() <= 1e-12 {
                    self.close(at, "take profit");
                    return true;
                }
            }
        }
        if self.closed_at.is_none() && self.held() > 0.0 && book.mid().is_some_and(|m| (m - self.stop()) * self.side <= 0.0) {
            self.close_at_book(book, at, "stop");
            changed = true;
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(bids: &[(f64, f64)], asks: &[(f64, f64)]) -> Book {
        Book { bids: bids.to_vec(), asks: asks.to_vec(), time_ms: 0 }
    }

    #[test]
    fn ticks() {
        assert_eq!(tick(84_805.0, 5), 1.0);
        assert_eq!(tick(150_000.0, 5), 1.0);
        assert!((tick(2681.0, 4) - 0.1).abs() < 1e-12);
        assert!((tick(88.39, 2) - 0.001).abs() < 1e-12);
        // 0.24: 5 significant figures would be 0.00001, but szDecimals 0 allows 6 decimals.
        assert!((tick(0.24031, 0) - 0.00001).abs() < 1e-15);
        assert!((tick(0.0036, 0) - 0.000001).abs() < 1e-15);
    }

    #[test]
    fn most_aggressive_post_only_price() {
        let tight = book(&[(100.0, 5.0)], &[(100.1, 5.0)]);
        assert_eq!(maker_px(1.0, &tight, 0.1), Some(100.0));
        assert_eq!(maker_px(-1.0, &tight, 0.1), Some(100.1));
        let wide = book(&[(100.0, 5.0)], &[(100.5, 5.0)]);
        assert!((maker_px(1.0, &wide, 0.1).unwrap() - 100.4).abs() < 1e-9);
        assert!((maker_px(-1.0, &wide, 0.1).unwrap() - 100.1).abs() < 1e-9);
    }

    #[test]
    fn queue_then_fill() {
        let b = book(&[(100.0, 3.0)], &[(100.1, 5.0)]);
        let mut o = Resting::new(1.0, 100.0, 2.0, &Book { time_ms: 5, ..b.clone() });
        assert_eq!(o.ahead, 3.0);
        assert_eq!(o.on_print(false, 99.0, 9.0, 5), 0.0); // in the book it was placed on
        o.since_ms = 0;
        assert_eq!(o.on_print(true, 100.1, 10.0, 1), 0.0); // buys do not fill a buy
        assert_eq!(o.on_print(false, 100.0, 2.0, 1), 0.0); // 1 left ahead
        assert_eq!(o.on_print(false, 100.0, 1.5, 1), 0.5);
        assert_eq!(o.left, 1.5);
        // Cancels ahead: nothing ahead now; a sell through our price fills the rest.
        o.ahead = 1.0;
        o.on_book(&book(&[(100.0, 0.5)], &[(100.1, 5.0)]));
        assert_eq!(o.ahead, 0.5);
        assert_eq!(o.on_print(false, 99.9, 0.1, 1), 1.5);
        assert_eq!(o.left, 0.0);
    }

    #[test]
    fn book_reaching_the_order_fills_it() {
        let mut o = Resting::new(1.0, 100.0, 2.0, &book(&[(100.0, 3.0)], &[(100.1, 5.0)]));
        assert_eq!(o.on_book(&book(&[(99.9, 3.0)], &[(100.0, 1.0)])), 2.0);
        // A take profit far from the book keeps its place until the book shows its price.
        let mut tp = Resting::new(-1.0, 103.0, 1.0, &book(&[(100.0, 3.0)], &[(100.1, 5.0)]));
        tp.ahead = 4.0;
        tp.on_book(&book(&[(100.0, 3.0)], &[(100.1, 5.0), (100.2, 5.0)]));
        assert_eq!(tp.ahead, 4.0);
    }

    fn twin(mode: Mode) -> MakerTrade {
        MakerTrade { mode: Some(mode), side: 1.0, size: 2.0, stop_pct: 1.0, tp_pct: 2.0, sz_decimals: 2, equity: 1000.0,
                     ..Default::default() }
    }

    #[test]
    fn limit_then_market_takes_the_rest_at_the_deadline() {
        let b = book(&[(100.0, 3.0)], &[(100.01, 5.0)]);
        let mut t = twin(Mode::LimitThenMarket);
        assert!(t.place(&b, 0.0));
        assert_eq!(t.order.as_ref().unwrap().px, 100.0);
        t.on_print(false, 100.0, 4.0, 1, Some(&b), 1.0); // 1 of ours
        assert_eq!(t.maker_filled, 1.0);
        t.on_book(&b, ENTRY_WAIT_S + 1.0);
        assert_eq!(t.filled, 2.0);
        assert!((t.entry() - 100.005).abs() < 1e-9);
        assert!((t.entry_fee - (100.0 * MAKER_FEE + 100.01 * crate::config::TAKER_FEE)).abs() < 1e-9);
        assert!(t.order.is_none() && t.tp_order.is_some());
        // Take profit at entry +2%, filled by buyers through it.
        let tp = t.tp();
        t.on_print(true, tp + 0.5, 1.0, 1, Some(&b), 100.0);
        assert_eq!(t.exit_reason.as_deref(), Some("take profit"));
        assert!((t.realized - 2.0 * (tp - t.entry())).abs() < 1e-9);
    }

    #[test]
    fn limit_not_filled_and_following_the_price() {
        let mut t = twin(Mode::Limit);
        t.place(&book(&[(100.0, 3.0)], &[(100.01, 5.0)]), 0.0);
        t.on_book(&book(&[(100.05, 1.0)], &[(100.06, 5.0)]), 5.0);
        assert_eq!(t.requotes, 1);
        assert_eq!(t.order.as_ref().unwrap().px, 100.05);
        t.on_book(&book(&[(100.05, 1.0)], &[(100.06, 5.0)]), ENTRY_WAIT_S + 1.0);
        assert_eq!(t.exit_reason.as_deref(), Some("not filled"));
        assert_eq!(t.pnl(), 0.0);
    }

    #[test]
    fn stop_at_the_book() {
        let b = book(&[(100.0, 3.0)], &[(100.01, 5.0)]);
        let mut t = twin(Mode::Limit);
        t.place(&b, 0.0);
        t.on_print(false, 99.99, 1.0, 1, Some(&b), 1.0); // through: all of it
        assert_eq!(t.filled, 2.0);
        t.on_book(&book(&[(98.9, 10.0)], &[(98.92, 5.0)]), 2.0);
        assert_eq!(t.exit_reason.as_deref(), Some("stop"));
        assert!((t.realized - 2.0 * (98.9 - 100.0)).abs() < 1e-9);
    }
}
