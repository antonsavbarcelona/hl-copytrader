//! The family's newer strategies' trades on the live account (the HIP-3 dip, the wick bids, the
//! listing short): a position with its resting stop (and take) on the exchange and an exit time.
//! A trade is opened at market (`open_market`) or from a bid's fill (`protect`); out at market
//! when its time is up (`close`); a position gone from the account (its stop or take filled, a
//! liquidation) ends it (`gone`). Every coin goes cross at its max leverage before its first opening order
//! (`Venue::ensure_leverage`); a HIP-3 coin the dex keeps isolated only at `isolated_leverage` (10x max -> 5x; was 3): its
//! liquidation ~15% away, inside a 20% stop for the 0-2.6% of trades that go that far (V298).

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::api::now;
use crate::live::{Placed, Venue};

/// A HIP-3 coin only isolated: at 3 (the liquidation ~30% away, past the 20-25% stops). The cross ones at their max
/// (2026-11-01; was 10 from 2026-10-28, V77: the bids' margin a third of the isolated 3x's; on cross the liquidation is
/// the account's, far past the stops within the family's cap).
pub(crate) const H3_ISOLATED_LEVERAGE: u32 = 3;

/// The liquidation's distance an isolated position keeps at least (fraction of an adverse move).
pub(crate) const ISOLATED_LIQ_MIN: f64 = 0.15;

/// The share of the way to an isolated position's liquidation its stop sits at most (`Venue::stop_cap`).
pub(crate) const LIQ_STOP_SHARE: f64 = 0.9;

/// The stop's farthest distance on an isolated position at leverage `l` on a coin of max `max`: `LIQ_STOP_SHARE` of the
/// liquidation's (~ 1/l - 0.5/max): 10x max at 5x -> 13.5%, 3x at 3x -> 15%.
pub(crate) fn liq_stop(l: f64, max: f64) -> f64 {
    (LIQ_STOP_SHARE * (1.0 / l.max(1.0) - 0.5 / max.max(1.0))).max(0.01)
}

/// The isolated leverage of a coin the dex keeps isolated only, its max `max` (2026-11-03, the user's call: the HIP-3 coins
/// offer 10x isolated). The liquidation comes at ~ 1/L - the maintenance (half the initial margin at the max: 5% at 10x);
/// L is the highest that keeps it `ISOLATED_LIQ_MIN` (15%) away: 10x max -> 5x (liquidation ~-15%), 20x -> 5x, 50x -> 6x,
/// 5x -> 4x, 3x -> 3x. V298 (the HIP-3 families' tape legs, their adverse paths): at 5x 0-2.6% of the trades reach the
/// liquidation (each then loses its margin, 20% of the notional - the stop's loss), the mean result 161 -> 155 bp (the
/// shallow bids), 216 -> 197 (the wick), the others unchanged; the margin a trade or a resting order holds 33% -> 20% of
/// the notional (the result per $ of margin +60%). At 10x 12-30% liquidated: worse.
pub(crate) fn isolated_leverage(max: u32) -> u32 {
    let max = max.max(1);
    let l = (1.0 / (ISOLATED_LIQ_MIN + 0.5 / f64::from(max))).floor() as u32;
    l.clamp(1, max)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Trade {
    pub(crate) coin: String,
    /// Signed: + long, - short.
    pub(crate) size: f64,
    pub(crate) entry_px: f64,
    pub(crate) at: f64,
    pub(crate) exit_at: f64,
    pub(crate) stop_px: f64,
    pub(crate) stop_oid: Option<u64>,
    #[serde(default)]
    pub(crate) take_px: f64,
    #[serde(default)]
    pub(crate) take_oid: Option<u64>,
}

impl Trade {
    pub(crate) fn dir(&self) -> f64 {
        self.size.signum()
    }

    /// The $ its stop loses.
    pub(crate) fn risk(&self) -> f64 {
        (self.size * (self.entry_px - self.stop_px)).abs()
    }

    /// The return (as a fraction, signed for the side) at `px`.
    pub(crate) fn ret(&self, px: f64) -> f64 {
        if self.entry_px > 0.0 { self.dir() * (px / self.entry_px - 1.0) } else { 0.0 }
    }
}

/// One deposit for the whole family: the account's equity (the last read by any strategy) and the
/// family's open trades (coin -> strategies), shared by the strategies' tasks.
static EQUITY: std::sync::Mutex<f64> = std::sync::Mutex::new(0.0);
/// Coin -> its trades: (strategy, the trade's risk in $: its stop's loss, its side +1 / -1).
static OPEN: std::sync::Mutex<Option<std::collections::HashMap<String, Vec<(String, f64, f64)>>>> = std::sync::Mutex::new(None);

/// A strategy's switch `var`, all on by default (2026-10-31, the user's call: the config lives in the code, not in
/// the env): unset or "1" on; set empty the trial: on for its first `TRIAL_TRADES` (3) live trades, then the signals
/// only; anything else ("0") off. None off, Some(None) on, Some(Some(n)) the trial of n trades.
pub(crate) fn switch(var: &str) -> Option<Option<u32>> {
    match std::env::var(var).map(|x| x.trim().to_string()) {
        Err(_) => Some(None),
        Ok(x) if x.is_empty() => Some(Some(env_f("TRIAL_TRADES", 3.0) as u32)),
        Ok(x) if x == "1" => Some(None),
        _ => None,
    }
}

/// Whether one more live trade fits the trial (`used` opened so far; no trial: always).
pub(crate) fn trial_open(trial: Option<u32>, used: u32) -> bool {
    trial.is_none_or(|n| used < n)
}

/// The trial for the messages: "" without one.
pub(crate) fn trial_label(trial: Option<u32>, used: u32) -> String {
    trial.map(|n| format!(", ПРОБА: первые {n} сделки (было {used}), потом только сигналы")).unwrap_or_default()
}

fn env_f(k: &str, d: f64) -> f64 {
    std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d)
}

pub(crate) fn set_equity(x: f64) {
    if x > 0.0 {
        *EQUITY.lock().unwrap() = x;
    }
}

pub(crate) fn equity() -> f64 {
    *EQUITY.lock().unwrap()
}

/// The family's risk a trade, % of the equity: `FAMILY_RISK_PCT` (0 for the test: each strategy's own $20; the
/// target after it 5: V77, the HIP-3 perps cross at 10x and a cap of 40%, x103 over 16.8 months, the drawdown with
/// the open 28.1%, 1,935 trades, win 83%, halves x11.0 / x9.3; before: 3.3).
/// The 20% setting (2026-10-27, V60/V61 on the tape, 2025-05 .. 2026-10, all the strategies on one
/// deposit, compounding, the slots, one a coin): 3.3% a trade within a `FAMILY_RISK_CAP_PCT` of 20%,
/// the dip held 120 min, the pump short 240, the listing's take 30%, the RSI dip a third a buy:
/// +1,108%, the drawdown with the open 17.3% (3.38%: 20.2%), ~2.9 trades a day.
/// 2 from 2026-10-31 (the user's call: V134, the whole HL portfolio 2025-05 .. 2026-10 with 2 strategies a coin was
/// x409 at 5% with the open drawdown 24%; 2% keeps the drawdown near 10%).
pub(crate) fn family_risk_pct() -> f64 {
    env_f("FAMILY_RISK_PCT", 2.0)
}

/// How many of the family's strategies may hold one coin at once: `FAMILY_PER_COIN` (2 from 2026-10-31; was 1).
/// V134, the whole HL portfolio (the 8 live strategies, the V77 sim, 2025-05 .. 2026-10) at 3.3%: x42.7, the
/// drawdown with the open 17.8% -> x62.6, 16.1%; Binance 2020-26 (V118): +45%, 6 of 7 years. The same side only
/// and not the same strategy twice: the account nets a coin's positions, so a long and a short would cancel.
pub(crate) fn per_coin() -> usize {
    env_f("FAMILY_PER_COIN", 2.0).max(1.0) as usize
}

/// A trade's risk ($ its stop loses): `family_risk_pct` of the equity when on and the equity known
/// (the compounding: the sizes follow the deposit), else the strategy's own `usd`.
pub(crate) fn risk_usd(usd: f64) -> f64 {
    let pct = family_risk_pct();
    let eq = equity();
    if pct > 0.0 && eq > 0.0 { pct / 100.0 * eq } else { usd }
}

/// A strategy's trades held now (coin, risk $, side): the registry's entries of `kind` set to them.
pub(crate) fn sync(kind: &str, trades: impl Iterator<Item = (String, f64, f64)>) {
    let mut g = OPEN.lock().unwrap();
    let m = g.get_or_insert_with(Default::default);
    for v in m.values_mut() {
        v.retain(|k| k.0 != kind);
    }
    m.retain(|_, v| !v.is_empty());
    for (c, r, d) in trades {
        m.entry(c).or_default().push((kind.to_string(), r, d.signum()));
    }
}

/// The crypto wick ladder's third rung (2026-11-01, V234): outside the per-coin limit - the coin its 7% and 10% rungs hold
/// takes it (the same side, not twice, the family's risk cap as any), and it takes no other strategy's place there.
pub(crate) const RUNG3: &str = "wick3";

/// Whether a trade of `held` on the coin takes a `kind` trade's place there (`per_coin`): the third rung never; for the
/// HIP-3 dip no wick rung (2026-11-10, the user's call: xyz:NET 2026-11-06 skipped behind two wick rungs, +2.26% on
/// paper) - still on the same side only, the dip once, the family's risk cap as any.
fn counts_against(kind: &str, held: &str) -> bool {
    held != RUNG3 && !(kind == "hip3" && held.starts_with("wick"))
}

/// Whether the coin may take one more trade of `kind` on side `dir`: free, or held by fewer than `per_coin`
/// trades (the third rung not counted), none of them `kind`'s, all on the same side; the third rung: any number held.
fn coin_fits(held: &[(String, f64, f64)], kind: &str, dir: f64, per_coin: usize) -> bool {
    let same = held.iter().all(|k| k.0 != kind && k.2 * dir > 0.0);
    let n = held.iter().filter(|k| counts_against(kind, &k.0)).count();
    if kind == RUNG3 { same } else { held.is_empty() || (n < per_coin && same) }
}

/// Whether one more trade of `kind` on side `dir` (+1 long, -1 short) risking `risk` $ fits: the coin free (or
/// shared within `per_coin`), `FAMILY_MAX_OPEN` (0: no cap, as the 20% setting) trades at once, and the open trades'
/// risks with it within `FAMILY_RISK_CAP_PCT` (40, V77: at 5% a trade it skipped 17 trades in 16.8 months; all the
/// stops at once lose 40% at most, far from a liquidation; was 20; 0: no cap) of the equity (the deposit's
/// simultaneous risk; each strategy keeps its own slots besides).
pub(crate) fn can_open(coin: &str, kind: &str, dir: f64, risk: f64) -> bool {
    let cap = env_f("FAMILY_MAX_OPEN", 0.0) as usize;
    let risk_cap = env_f("FAMILY_RISK_CAP_PCT", 40.0) / 100.0;
    let eq = equity();
    let g = OPEN.lock().unwrap();
    let (n, open_risk, fits) = g.as_ref().map_or((0, 0.0, true), |m| {
        (m.values().map(Vec::len).sum::<usize>(), m.values().flatten().map(|x| x.1).sum::<f64>(),
         coin_fits(m.get(coin).map_or(&[][..], Vec::as_slice), kind, dir, per_coin()))
    });
    // the crash cap (crate::clusters): in a crash a dip buyer opens no more than the cap in one cluster of coins
    let crowded = crate::clusters::capped_kind(kind) && crate::clusters::cap() > 0 && crate::clusters::crash() && {
        let cl = crate::clusters::cluster_of(coin);
        cl.is_some() && g.as_ref().is_some_and(|m| m.iter()
            .filter(|(c, v)| crate::clusters::cluster_of(c) == cl && v.iter().any(|k| crate::clusters::capped_kind(&k.0)))
            .count() >= crate::clusters::cap())
    };
    fits && !crowded && (cap == 0 || n < cap) && (risk_cap <= 0.0 || eq <= 0.0 || open_risk + risk <= risk_cap * eq + 1e-9)
}

pub(crate) fn register(coin: &str, kind: &str, risk: f64, dir: f64) {
    let mut g = OPEN.lock().unwrap();
    let v = g.get_or_insert_with(Default::default).entry(coin.to_string()).or_default();
    v.retain(|k| k.0 != kind);
    v.push((kind.to_string(), risk, dir.signum()));
}

/// Whether any strategy's trade (or a top-up) holds `coin`.
pub(crate) fn registered(coin: &str) -> bool {
    OPEN.lock().unwrap().as_ref().is_some_and(|m| m.get(coin).is_some_and(|v| !v.is_empty()))
}

pub(crate) fn unregister(coin: &str, kind: &str) {
    if let Some(m) = OPEN.lock().unwrap().as_mut() {
        if let Some(v) = m.get_mut(coin) {
            v.retain(|k| k.0 != kind);
            if v.is_empty() {
                m.remove(coin);
            }
        }
    }
}

/// Whether a strategy other than `kind` holds `coin` too (the account's position there is theirs and ours).
pub(crate) fn shared(coin: &str, kind: &str) -> bool {
    OPEN.lock().unwrap().as_ref().and_then(|m| m.get(coin)).is_some_and(|v| v.iter().any(|k| k.0 != kind))
}

/// Whether the family holds `coin` (a position there is the family's, not a stray one).
pub(crate) fn family_holds(coin: &str) -> bool {
    OPEN.lock().unwrap().as_ref().is_some_and(|m| m.contains_key(coin))
}

/// Whether our resting stop / take `oid` has done its work (filled, or triggered): on a shared coin the account's
/// position is not ours alone, so a trade's end is read from its own orders. Not read: false (the next check).
pub(crate) async fn order_done(v: &Venue, oid: u64) -> bool {
    match v.order_state(oid).await {
        Ok((st, filled)) => st == "filled" || st == "triggered" || (filled > 0.0 && st != "open"),
        Err(e) => {
            crate::log!("trades: order {oid} status not read: {e:#}");
            false
        }
    }
}

/// The family's line for the chat: the sizing and the cap.
pub(crate) fn sizing_label(usd: f64, stop: f64) -> String {
    let pct = family_risk_pct();
    if pct > 0.0 { format!("{pct:.1}% депозита на сделку (позиция {:.0}% депозита)", pct / stop) } else { format!("${usd:.0} на сделку (позиция ${:.0})", usd / stop) }
}

/// The size for a `risk_usd` loss at a `stop` (fraction) from `px`: positive.
pub(crate) fn size_for(risk_usd: f64, stop: f64, px: f64) -> f64 {
    if px > 0.0 && stop > 0.0 { risk_usd / stop / px } else { 0.0 }
}

/// The stop (and the take, a resting reduce-only limit) for a position of `size` (signed) filled at
/// `px`: the trade, held `hold_s`. A stop not placed is an error (the caller closes the position).
pub(crate) async fn protect(v: &Venue, coin: &str, size: f64, px: f64, stop: f64, take: Option<f64>, hold_s: f64) -> Result<Trade> {
    protect_at(v, coin, size, px, stop, take, hold_s, 0.0).await
}

/// The take's base: the fill, or the price expected (`expect` > 0) when the fill came worse (a long above it, a short
/// under): the take where the backtest and the paper put it, not pushed out by the slippage (2026-11-07, xyz:SOXL: the
/// paper in at 45.132 and its take hit at 47.475; ours in at 45.226, its take at 47.487 missed by 0.03%).
pub(crate) fn take_base(px: f64, expect: f64, dir: f64) -> f64 {
    if expect > 0.0 && (expect - px) * dir < 0.0 { expect } else { px }
}

/// `protect` with the take from `take_base(px, expect)`; the stop from the fill.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn protect_at(v: &Venue, coin: &str, size: f64, px: f64, stop: f64, take: Option<f64>, hold_s: f64, expect: f64) -> Result<Trade> {
    let dir = size.signum();
    // an isolated position: the stop inside its liquidation (a stop past it never fires)
    let stop = v.stop_cap(coin).map_or(stop, |c| stop.min(c));
    let stop_px = px * (1.0 - dir * stop);
    let stop_oid = v.stop(coin, dir, size, stop_px).await?;
    let (mut take_px, mut take_oid) = (0.0, None);
    if let Some(tk) = take {
        take_px = take_base(px, expect, dir) * (1.0 + dir * tk);
        match v.limit(coin, -size, take_px, true, "Gtc").await {
            Ok(Placed::Resting(o)) => take_oid = Some(o),
            Ok(Placed::Filled(..)) => {}
            Err(e) => crate::log!("trades: {coin} take not placed (out on time or the stop): {e:#}"),
        }
    }
    let t = now();
    Ok(Trade { coin: coin.to_string(), size, entry_px: px, at: t, exit_at: t + hold_s, stop_px, stop_oid: Some(stop_oid), take_px, take_oid })
}

/// A position opened at market for `size` (signed), then protected (see `protect`). The stop not
/// placed: the position closed again and the error returned.
pub(crate) async fn open_market(v: &Venue, coin: &str, size: f64, stop: f64, take: Option<f64>, hold_s: f64) -> Result<Trade> {
    open_market_at(v, coin, size, stop, take, hold_s, 0.0).await
}

/// `open_market` with the take from the price expected when the fill came worse (see `take_base`).
pub(crate) async fn open_market_at(v: &Venue, coin: &str, size: f64, stop: f64, take: Option<f64>, hold_s: f64, expect: f64) -> Result<Trade> {
    let (got, px, _) = v.market(coin, size, false).await?;
    if got == 0.0 {
        bail!("{coin}: nothing filled");
    }
    match protect_at(v, coin, got, px, stop, take, hold_s, expect).await {
        Ok(t) => Ok(t),
        Err(e) => {
            let _ = v.market(coin, -got, true).await;
            bail!("{coin}: the stop not placed, closed again: {e:#}")
        }
    }
}

/// The trade out at market: its stop and take cancelled first; the exit's price (0: nothing left).
pub(crate) async fn close(v: &Venue, t: &Trade) -> Result<f64> {
    for o in [t.take_oid, t.stop_oid].into_iter().flatten() {
        v.cancel(&t.coin, o).await;
    }
    match v.market(&t.coin, -t.size, true).await {
        Ok((_, px, _)) => Ok(px),
        Err(e) => {
            // Already out (a lost reply after the sell filled, the stop, a hand): ended at its close fills' price,
            // not tried again for ever (2026-11-10, rsidip ZRO: closed at 09:15, its exit retried each poll for
            // hours, every try a request off the exhausted limit). The position read is an info request: it goes
            // while the exchange refuses the orders.
            let dex = t.coin.split_once(':').map(|d| d.0).unwrap_or("");
            match v.dex_positions(dex).await {
                Ok(p) if p.get(&t.coin).copied().unwrap_or(0.0) * t.size <= 0.0 => {
                    let px = match v.close_fills(&t.coin, (t.at * 1000.0) as i64).await {
                        Some((px, _)) => px,
                        None => v.mid(&t.coin).await.unwrap_or(t.entry_px),
                    };
                    crate::log!("{}: the close errored ({e:#}) but no position is held: ended at {px}", t.coin);
                    Ok(px)
                }
                _ => Err(e),
            }
        }
    }
}

/// A trade of `kind` whose position is gone (held now: the account's signed size in its coin): how it
/// ended, the price, and its other order cancelled. None: still held. On a coin another strategy holds too
/// the account's position outlives ours: then ours is gone when its own stop or take has filled.
pub(crate) async fn gone(v: &Venue, t: &Trade, held: f64, kind: &str) -> Option<(&'static str, f64)> {
    // The take filled in part (2026-11-03, io:PRL: 140.6 of 145.2 at the take, 4.6 left with the stop and the take's rest):
    // the rest closed at market at once, the take's rest and the stop cancelled (the user's call).
    if held * t.size > 0.0 {
        if let Some(o) = t.take_oid {
            if let Ok((f, open)) = v.filled(o).await {
                if f > 0.0 && f < t.size.abs() * (1.0 - 1e-9) {
                    if open {
                        v.cancel(&t.coin, o).await;
                    }
                    if let Some(s) = t.stop_oid {
                        v.cancel(&t.coin, s).await;
                    }
                    let rest = (t.size.abs() - f).min(held.abs());
                    match v.market(&t.coin, -t.size.signum() * rest, true).await {
                        Ok(_) => crate::log!("{kind}: {} the take filled {f} of {}: the rest {rest} closed at market", t.coin, t.size.abs()),
                        Err(e) => crate::log!("{kind}: {} the take filled {f} of {}: the rest {rest} not closed: {e:#}", t.coin, t.size.abs()),
                    }
                    return Some(("take", t.take_px));
                }
            }
        }
    }
    if held * t.size > 0.0 {
        if !shared(&t.coin, kind) {
            return None;
        }
        let mut done = false;
        for o in [t.stop_oid, t.take_oid].into_iter().flatten() {
            if order_done(v, o).await {
                done = true;
                break;
            }
        }
        if !done {
            return None;
        }
    }
    // The exchange's order status and fills lag the position by a few seconds (2026-11-04, xyz:SNXX: the take filled at
    // 6.4181, the position gone, the take's status and the fills not yet there -> told "closed outside the bot" at the
    // mid 6.4274): neither the take nor the stop seen done and no closing fill -> asked again, up to 3 times 2 s apart.
    let mut took = false;
    let mut stopped = false;
    for attempt in 0..4 {
        took = match t.take_oid {
            Some(o) => v.filled(o).await.map(|x| x.0 > 0.0).unwrap_or(false),
            None => false,
        };
        // the stop filled or triggered; else the position went another way: closed by hand on the exchange, or liquidated
        // (2026-11-03, xyz:RELIANCE closed by hand at ~13.90 was told "the stop 12.114, -10%, -$21")
        stopped = !took && match t.stop_oid {
            Some(o) => order_done(v, o).await,
            None => false,
        };
        if took || stopped || attempt == 3 || v.close_fills(&t.coin, (t.at * 1000.0) as i64).await.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    for o in [t.take_oid, t.stop_oid].into_iter().flatten() {
        if (took && Some(o) == t.take_oid) || (stopped && Some(o) == t.stop_oid) {
            continue;
        }
        v.cancel(&t.coin, o).await;
    }
    let fills = v.close_fills(&t.coin, (t.at * 1000.0) as i64).await;
    // the order status still behind but the closing fills at the take's price: the take
    if !took && !stopped && fills.is_some_and(|x| t.take_px > 0.0 && (x.0 / t.take_px - 1.0).abs() < 0.002) {
        took = true;
    }
    Some(if took {
        ("take", t.take_px)
    } else if stopped {
        ("stop", fills.map_or(t.stop_px, |x| x.0))
    } else {
        let px = match fills {
            Some(x) => x.0,
            None => v.mid(&t.coin).await.unwrap_or(t.entry_px),
        };
        ("manual", px)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_isolated_leverage() {
        assert_eq!([3, 5, 10, 20, 50].map(isolated_leverage), [3, 4, 5, 5, 6]);
    }

    #[test]
    fn the_take_from_the_price_expected_when_the_fill_came_worse() {
        assert_eq!(take_base(45.226, 45.132, 1.0), 45.132);
        assert_eq!(take_base(45.0, 45.132, 1.0), 45.0);
        assert_eq!(take_base(45.226, 0.0, 1.0), 45.226);
        assert_eq!(take_base(10.0, 10.1, -1.0), 10.1);
        assert_eq!(take_base(10.2, 10.1, -1.0), 10.2);
    }

    #[test]
    fn an_isolated_stop_sits_inside_the_liquidation() {
        assert!((liq_stop(5.0, 10.0) - 0.135).abs() < 1e-9);
        assert!((liq_stop(3.0, 3.0) - 0.15).abs() < 1e-9);
        for max in [3, 5, 10, 20, 50] {
            let l = isolated_leverage(max);
            let liq = 1.0 / f64::from(l) - 0.5 / f64::from(max);
            assert!(liq_stop(f64::from(l), f64::from(max)) < liq);
        }
    }

    #[test]
    fn a_switch_is_on_by_default() {
        let v = "TEST_SWITCH_ON_X";
        // SAFETY: a test-only variable no other code reads.
        unsafe { std::env::remove_var(v); }
        assert_eq!(switch(v), Some(None));
        unsafe { std::env::set_var(v, "1"); }
        assert_eq!(switch(v), Some(None));
        unsafe { std::env::set_var(v, ""); }
        assert_eq!(switch(v), Some(Some(3)));
        unsafe { std::env::set_var(v, "0"); }
        assert_eq!(switch(v), None);
        unsafe { std::env::remove_var(v); }
    }

    #[test]
    fn a_coin_shared_by_two_strategies_on_one_side_only() {
        let k = |s: &str, d: f64| (s.to_string(), 20.0, d);
        assert!(coin_fits(&[], "dip", 1.0, 1));
        assert!(!coin_fits(&[k("wick", 1.0)], "dip", 1.0, 1));                 // one a coin
        assert!(coin_fits(&[k("wick", 1.0)], "dip", 1.0, 2));                  // two a coin, both long
        assert!(!coin_fits(&[k("wick", 1.0)], "dip", -1.0, 2));                // a short against a long: never
        assert!(!coin_fits(&[k("dip", 1.0)], "dip", 1.0, 2));                  // not the same strategy twice
        assert!(!coin_fits(&[k("wick", 1.0), k("rsidip", 1.0)], "dip", 1.0, 2));
        // the ladder's third rung: past the two (the 7% and 10% rungs), not twice, not against a short; it takes no place
        assert!(coin_fits(&[k("wick", 1.0), k("wick2", 1.0)], RUNG3, 1.0, 2));
        assert!(!coin_fits(&[k("wick", 1.0), k(RUNG3, 1.0)], RUNG3, 1.0, 2));
        assert!(!coin_fits(&[k("pump", -1.0)], RUNG3, 1.0, 2));
        // the HIP-3 dip: the wick's rungs on the coin take no place of it (NET 2026-11-06), the side and once still hold
        assert!(!coin_fits(&[k("wick", 1.0), k("wick2", 1.0)], "dip", 1.0, 2));
        assert!(coin_fits(&[k("wick", 1.0), k("wick2", 1.0)], "hip3", 1.0, 2));
        assert!(coin_fits(&[k("wick", 1.0), k("wick2", 1.0), k(RUNG3, 1.0)], "hip3", 1.0, 1));
        assert!(!coin_fits(&[k("wicka", -1.0)], "hip3", 1.0, 2));               // a wick ask (short): never against it
        assert!(!coin_fits(&[k("wick", 1.0), k("hip3", 1.0)], "hip3", 1.0, 2));  // the dip once
        assert!(!coin_fits(&[k("wick", 1.0), k("rsidip", 1.0)], "hip3", 1.0, 1)); // another strategy still counts
        // and the wick still bids a coin the dip holds (one of two places)
        assert!(coin_fits(&[k("hip3", 1.0)], "wick", 1.0, 2));
        assert!(coin_fits(&[k("wick", 1.0), k(RUNG3, 1.0)], "wick2", 1.0, 2));
        assert!(!coin_fits(&[k("wick", 1.0), k("wick2", 1.0), k(RUNG3, 1.0)], "dip", 1.0, 2));
    }

    #[test]
    fn sizes_and_returns() {
        assert!((size_for(20.0, 0.2, 50.0) - 2.0).abs() < 1e-12);     // $100 at $50
        assert_eq!(size_for(20.0, 0.0, 50.0), 0.0);
        let t = Trade { size: -2.0, entry_px: 100.0, ..Default::default() };
        assert!((t.ret(90.0) - 0.10).abs() < 1e-12);                   // a short up 10%
        let l = Trade { size: 2.0, entry_px: 100.0, ..Default::default() };
        assert!((l.ret(105.0) - 0.05).abs() < 1e-12);
    }
}
