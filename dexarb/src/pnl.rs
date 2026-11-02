//! The full net-PnL calculator of one hedged pair: LONG on venue A, SHORT on venue B, the same quantity of the underlying.
//!
//! NetPnL = BasisPnL + FundingPnL - TradingFees - TreasuryCosts, where
//!   EntryBasisUSD = q x (ShortSellVWAP_B - LongBuyVWAP_A)      (the books walked for the size, per unit of the underlying)
//!   ExitBasisUSD  = q x (ShortBuyVWAP_B  - LongSellVWAP_A)     (the exit priced on the books as they are now, shifted by the
//!                                                                scenario's change of the mid basis)
//!   BasisPnL      = EntryBasisUSD - ExitBasisUSD               (the spreads and the impact of both legs, both ways, inside)
//!   FundingPnL    = H x (rate_B x N_B - rate_A x N_A)          (+ = longs pay: the short receives rate_B, the long pays rate_A)
//! The funding scenarios come from the history (no "the current rate stays"): conservative / base / optimistic.
//! The capital is both legs' initial margin plus a reserve, and the ROI is on it, not on one leg's notional.

use crate::exec::{self, Fill};
use crate::model::*;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct Leg {
    pub venue: String,
    pub symbol: String,
    pub open: Fill,
    pub close_now: Fill,
    pub taker: f64,
    pub fee_source: String,
    pub mult: f64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct FundingScenario {
    /// Per-hour fractions, + = longs pay (for GMX the side's own cost).
    pub long_pays_h: f64,
    pub short_gets_h: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Params {
    pub usd: f64,
    pub hours: f64,
    pub leverage: f64,
    /// The margin kept free on each venue over the initial margin (0.5 = 50%).
    pub reserve: f64,
    /// The treasury cost a round trip carries (the transfers amortised over the trades the capital serves), $.
    pub treasury_usd: f64,
    /// The change of the mid basis (B - A, fraction of the price) from the entry to the exit: 0 = unchanged.
    pub basis_change: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Result_ {
    pub q_base: f64,
    pub entry_basis_usd: f64,
    pub exit_basis_usd: f64,
    pub basis_pnl: f64,
    pub mid_basis: f64,
    /// The costs vs the mids: the open (both legs' spread + impact + fees) and the close (same, on today's books).
    pub entry_cost: f64,
    pub exit_cost: f64,
    pub fees: f64,
    pub funding: [f64; 3],
    pub treasury: f64,
    pub net: [f64; 3],
    pub capital: f64,
    pub roi: [f64; 3],
    pub apr: [f64; 3],
    /// Hours until the base scenario's funding pays the round trip (None: it never does).
    pub break_even_h: Option<f64>,
    pub executable: bool,
    pub liq_move: f64,
    pub warnings: Vec<String>,
}

pub fn calc(
    a: &Instrument,
    ba: &Book,
    b: &Instrument,
    bb: &Book,
    sc: [FundingScenario; 3],
    p: &Params,
) -> Option<(Leg, Leg, Result_)> {
    let usd = df(p.usd);
    let la = exec::walk(ba, true, usd)?;
    let q_base = la.qty * a.multiplier;
    let qb = q_base / b.multiplier;
    let sb = exec::walk_qty(bb, false, qb)?;
    let la_c = exec::walk_qty(ba, false, la.qty)?;
    let sb_c = exec::walk_qty(bb, true, qb)?;
    let pa_buy = f(la.vwap / a.multiplier);
    let pb_sell = f(sb.vwap / b.multiplier);
    let pa_sell = f(la_c.vwap / a.multiplier);
    let pb_buy = f(sb_c.vwap / b.multiplier);
    let q = f(q_base);
    let (ma, mb) = (
        f(exec::mid(ba)? / a.multiplier),
        f(exec::mid(bb)? / b.multiplier),
    );
    let mid_basis = mb / ma - 1.0;
    let entry = q * (pb_sell - pa_buy);
    // the exit: today's books, the mid basis moved by the scenario's change
    let shift = p.basis_change * ma * q;
    let exit = q * (pb_buy - pa_sell) + shift;
    let basis_pnl = entry - exit;
    let (na, nb) = (q * pa_buy, q * pb_sell);
    let (ta, tb) = (f(a.fees.taker), f(b.fees.taker));
    let fees = ta * (na + q * pa_sell) + tb * (nb + q * pb_buy);
    let entry_cost = q * ((pa_buy - ma) + (mb - pb_sell)) + ta * na + tb * nb;
    let exit_cost = q * ((ma - pa_sell) + (pb_buy - mb)) + ta * q * pa_sell + tb * q * pb_buy;
    let funding = sc.map(|s| p.hours * (s.short_gets_h * nb - s.long_pays_h * na));
    let treasury = p.treasury_usd;
    let net = funding.map(|fu| basis_pnl + fu - fees - treasury);
    let capital = (na + nb) / p.leverage.max(1.0) * (1.0 + p.reserve);
    let roi = net.map(|x| x / capital);
    let apr = roi.map(|r| r * 8760.0 / p.hours.max(1e-9));
    let per_h = sc[1].short_gets_h * nb - sc[1].long_pays_h * na;
    let cost = fees + treasury - basis_pnl;
    let break_even_h = (per_h > 0.0).then(|| (cost / per_h).max(0.0));
    let executable = la.complete && sb.complete && la_c.complete && sb_c.complete;
    let mut warnings = Vec::new();
    for (x, n) in [(a, ba), (b, bb)] {
        if !matches!(x.fees.source, Source::Api) {
            warnings.push(format!("{}: fees from docs", x.venue));
        }
        if x.venue == "GMX" {
            warnings.push("GMX: the close's price impact not modelled".into());
        }
        if now_ms() - n.ts_recv > 30_000 {
            warnings.push(format!(
                "{}: the book is {} s old",
                x.venue,
                (now_ms() - n.ts_recv) / 1000
            ));
        }
        if x.collateral == "DUSD" {
            warnings.push("StandX: DUSD collateral (conversion + peg)".into());
        }
    }
    // the adverse move that liquidates a leg on its own: ~ 1/leverage less the maintenance (taken as half the initial at the max)
    let liq_move = 1.0 / p.leverage.max(1.0)
        - 0.5
            / a.max_leverage
                .map(f)
                .unwrap_or(10.0)
                .max(b.max_leverage.map(f).unwrap_or(10.0));
    let leg = |i: &Instrument, o: Fill, c: Fill| Leg {
        venue: i.venue.clone(),
        symbol: i.symbol.clone(),
        open: o,
        close_now: c,
        taker: f(i.fees.taker),
        fee_source: format!("{:?}", i.fees.source),
        mult: f(i.multiplier),
    };
    Some((
        leg(a, la, la_c),
        leg(b, sb, sb_c),
        Result_ {
            q_base: q,
            entry_basis_usd: entry,
            exit_basis_usd: exit,
            basis_pnl,
            mid_basis,
            entry_cost,
            exit_cost,
            fees,
            funding,
            treasury,
            net,
            capital,
            roi,
            apr,
            break_even_h,
            executable,
            liq_move,
            warnings,
        },
    ))
}

/// The funding scenarios of a pair from its hourly history (+ = longs pay on each venue), no look-ahead: only the hours
/// before `now`. Conservative: the 25th percentile of the hourly net (short's - long's), base: the last 72 h mean, optimistic:
/// the 75th percentile - each as a net carried by (long_pays, short_gets) = (0, net).
pub fn scenarios_from(net_hourly: &[f64], current_net: f64) -> [FundingScenario; 3] {
    let mk = |x: f64| FundingScenario {
        long_pays_h: 0.0,
        short_gets_h: x,
    };
    if net_hourly.len() < 24 {
        // no history: conservative 0, base half the current, optimistic the current
        return [
            mk(0.0f64.min(current_net)),
            mk(current_net * 0.5),
            mk(current_net),
        ];
    }
    let mut s = net_hourly.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| s[((s.len() - 1) as f64 * p) as usize];
    let last: Vec<f64> = net_hourly.iter().rev().take(72).copied().collect();
    let base = last.iter().sum::<f64>() / last.len() as f64;
    [mk(pct(0.25).min(base)), mk(base), mk(pct(0.75).max(base))]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn book(bid: i64, ask: i64) -> Book {
        // prices in cents: one deep level each side
        Book { bids: vec![(D::new(bid, 2), D::from(1_000))], asks: vec![(D::new(ask, 2), D::from(1_000))], ts_exchange: None, ts_recv: now_ms() }
    }
    fn ins(v: &str, taker: &str) -> Instrument {
        let mut i = crate::venues::inst(crate::venues::V::Hyperliquid, "X", "X");
        i.venue = v.into();
        i.fees = Fees { taker: d(taker).unwrap(), maker: D::ZERO, source: Source::Api };
        i
    }
    #[test]
    fn the_decomposition() {
        // LONG A at 100.10 (bid 100.00), SHORT B at 100.50 (ask 100.60), $1001 = 10 units, takers 0.05%
        let (a, b) = (ins("A", "0.0005"), ins("B", "0.0005"));
        let sc = [FundingScenario { long_pays_h: 0.0, short_gets_h: 0.0001 }; 3];
        let p = Params { usd: 1001.0, hours: 24.0, leverage: 2.0, reserve: 0.5, treasury_usd: 0.0, basis_change: 0.0 };
        let (_, _, r) = calc(&a, &book(10000, 10010), &b, &book(10050, 10060), sc, &p).unwrap();
        assert!((r.q_base - 10.0).abs() < 1e-9);
        assert!((r.entry_basis_usd - 4.0).abs() < 1e-9); // 10 x (100.50 - 100.10)
        assert!((r.exit_basis_usd - 6.0).abs() < 1e-9); // 10 x (100.60 - 100.00)
        assert!((r.basis_pnl + 2.0).abs() < 1e-9); // the two spreads, both ways
        let fees = 0.0005 * (1001.0 + 1000.0 + 1005.0 + 1006.0);
        assert!((r.fees - fees).abs() < 1e-9);
        assert!((r.funding[1] - 24.0 * 0.0001 * 1005.0).abs() < 1e-9); // the short's notional receives
        assert!((r.net[1] - (-2.0 + 24.0 * 0.0001 * 1005.0 - fees)).abs() < 1e-9);
        assert!((r.capital - (1001.0 + 1005.0) / 2.0 * 1.5).abs() < 1e-9);
    }
}
