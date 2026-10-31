//! The family's newer strategies' trades on the live account (the HIP-3 dip, the wick bids, the
//! listing short): a position with its resting stop (and take) on the exchange and an exit time.
//! A trade is opened at market (`open_market`) or from a bid's fill (`protect`); out at market
//! when its time is up (`close`); a position gone from the account (its stop or take filled, a
//! liquidation) ends it (`gone`). The HIP-3 perps go cross at `H3_LEVERAGE` (10; the coin's max when
//! lower) first; a coin the dex keeps isolated only at `H3_ISOLATED_LEVERAGE` (3), so its stop is hit before
//! its liquidation.

use std::collections::HashSet;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::api::now;
use crate::live::{Placed, Venue};

/// The HIP-3 perps' leverage: cross at 10 (2026-10-28, V77: the bids' margin a third of the isolated 3x's, so the
/// family's margin no longer skips trades at a higher risk; on cross the liquidation is the account's, far past the
/// stops within the family's cap); a coin only isolated at 3 (the liquidation ~30% away, past the 20-25% stops).
pub(crate) const H3_LEVERAGE: u32 = 10;
pub(crate) const H3_ISOLATED_LEVERAGE: u32 = 3;

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

/// A strategy's switch `var`: "1" on; unset (or empty) the trial: on for its first `TRIAL_TRADES`
/// (3) live trades, then the signals only; anything else ("0") off. None off, Some(None) on,
/// Some(Some(n)) the trial of n trades.
pub(crate) fn switch(var: &str) -> Option<Option<u32>> {
    match std::env::var(var).map(|x| x.trim().to_string()) {
        Ok(x) if x == "1" => Some(None),
        Ok(x) if !x.is_empty() => None,
        _ => Some(Some(env_f("TRIAL_TRADES", 3.0) as u32)),
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

/// Whether the coin may take one more trade of `kind` on side `dir`: free, or held by fewer than `per_coin`
/// trades, none of them `kind`'s, all on the same side.
fn coin_fits(held: &[(String, f64, f64)], kind: &str, dir: f64, per_coin: usize) -> bool {
    held.is_empty() || (held.len() < per_coin && held.iter().all(|k| k.0 != kind && k.2 * dir > 0.0))
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
    fits && (cap == 0 || n < cap) && (risk_cap <= 0.0 || eq <= 0.0 || open_risk + risk <= risk_cap * eq + 1e-9)
}

pub(crate) fn register(coin: &str, kind: &str, risk: f64, dir: f64) {
    let mut g = OPEN.lock().unwrap();
    let v = g.get_or_insert_with(Default::default).entry(coin.to_string()).or_default();
    v.retain(|k| k.0 != kind);
    v.push((kind.to_string(), risk, dir.signum()));
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

/// Sets a HIP-3 coin's leverage once (`done` remembers the coins set): cross at `H3_LEVERAGE` (the coin's max when
/// lower); refused (a coin the dex keeps isolated only): isolated at `H3_ISOLATED_LEVERAGE`.
pub(crate) async fn h3_leverage(v: &Venue, coin: &str, max_lev: u32, done: &mut HashSet<String>) -> Result<()> {
    if !coin.contains(':') || done.contains(coin) {
        return Ok(());
    }
    if let Err(e) = v.leverage(coin, H3_LEVERAGE.min(max_lev.max(1)), true).await {
        crate::log!("trades: {coin} cross refused ({e:#}): isolated {H3_ISOLATED_LEVERAGE}x");
        v.leverage(coin, H3_ISOLATED_LEVERAGE.min(max_lev.max(1)), false).await?;
    }
    done.insert(coin.to_string());
    Ok(())
}

/// The stop (and the take, a resting reduce-only limit) for a position of `size` (signed) filled at
/// `px`: the trade, held `hold_s`. A stop not placed is an error (the caller closes the position).
pub(crate) async fn protect(v: &Venue, coin: &str, size: f64, px: f64, stop: f64, take: Option<f64>, hold_s: f64) -> Result<Trade> {
    let dir = size.signum();
    let stop_px = px * (1.0 - dir * stop);
    let stop_oid = v.stop(coin, dir, size, stop_px).await?;
    let (mut take_px, mut take_oid) = (0.0, None);
    if let Some(tk) = take {
        take_px = px * (1.0 + dir * tk);
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
    let (got, px, _) = v.market(coin, size, false).await?;
    if got == 0.0 {
        bail!("{coin}: nothing filled");
    }
    match protect(v, coin, got, px, stop, take, hold_s).await {
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
    let (_, px, _) = v.market(&t.coin, -t.size, true).await?;
    Ok(px)
}

/// A trade of `kind` whose position is gone (held now: the account's signed size in its coin): how it
/// ended, the price, and its other order cancelled. None: still held. On a coin another strategy holds too
/// the account's position outlives ours: then ours is gone when its own stop or take has filled.
pub(crate) async fn gone(v: &Venue, t: &Trade, held: f64, kind: &str) -> Option<(&'static str, f64)> {
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
    let took = match t.take_oid {
        Some(o) => v.filled(o).await.map(|x| x.0 > 0.0).unwrap_or(false),
        None => false,
    };
    let other = if took { t.stop_oid } else { t.take_oid };
    if let Some(o) = other {
        v.cancel(&t.coin, o).await;
    }
    Some(if took { ("take", t.take_px) } else { ("stop", t.stop_px) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coin_shared_by_two_strategies_on_one_side_only() {
        let k = |s: &str, d: f64| (s.to_string(), 20.0, d);
        assert!(coin_fits(&[], "dip", 1.0, 1));
        assert!(!coin_fits(&[k("wick", 1.0)], "dip", 1.0, 1));                 // one a coin
        assert!(coin_fits(&[k("wick", 1.0)], "dip", 1.0, 2));                  // two a coin, both long
        assert!(!coin_fits(&[k("wick", 1.0)], "dip", -1.0, 2));                // a short against a long: never
        assert!(!coin_fits(&[k("dip", 1.0)], "dip", 1.0, 2));                  // not the same strategy twice
        assert!(!coin_fits(&[k("wick", 1.0), k("rsidip", 1.0)], "dip", 1.0, 2));
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
