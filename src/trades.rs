//! The family's newer strategies' trades on the live account (the HIP-3 dip, the wick bids, the
//! listing short): a position with its resting stop (and take) on the exchange and an exit time.
//! A trade is opened at market (`open_market`) or from a bid's fill (`protect`); out at market
//! when its time is up (`close`); a position gone from the account (its stop or take filled, a
//! liquidation) ends it (`gone`). The HIP-3 perps are isolated: their leverage is set to
//! `H3_LEVERAGE` (3) first, so the stop is hit before the liquidation.

use std::collections::HashSet;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::api::now;
use crate::live::{Placed, Venue};

/// The HIP-3 perps' isolated leverage: the liquidation ~30% away, past the 20-25% stops.
pub(crate) const H3_LEVERAGE: u32 = 3;

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
/// family's open trades (coin -> strategy), shared by the strategies' tasks.
static EQUITY: std::sync::Mutex<f64> = std::sync::Mutex::new(0.0);
/// Coin -> (strategy, the trade's risk in $: its stop's loss).
static OPEN: std::sync::Mutex<Option<std::collections::HashMap<String, (String, f64)>>> = std::sync::Mutex::new(None);

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

/// A trade's risk ($ its stop loses): `FAMILY_RISK_PCT` (0: off) of the equity when set and the
/// equity known (the compounding: the sizes follow the deposit), else the strategy's own `usd`.
pub(crate) fn risk_usd(usd: f64) -> f64 {
    let pct = env_f("FAMILY_RISK_PCT", 0.0);
    let eq = equity();
    if pct > 0.0 && eq > 0.0 { pct / 100.0 * eq } else { usd }
}

/// A strategy's trades held now (coin, risk $): the registry's entries of `kind` set to them.
pub(crate) fn sync(kind: &str, trades: impl Iterator<Item = (String, f64)>) {
    let mut g = OPEN.lock().unwrap();
    let m = g.get_or_insert_with(Default::default);
    m.retain(|_, k| k.0 != kind);
    for (c, r) in trades {
        m.insert(c, (kind.to_string(), r));
    }
}

/// Whether one more trade risking `risk` $ fits: the coin free, `FAMILY_MAX_OPEN` (12; 0: no cap)
/// trades at once, and the open trades' risks with it within `FAMILY_RISK_CAP_PCT` (10; 0: no cap)
/// of the equity (the deposit's simultaneous risk; each strategy keeps its own slots besides).
pub(crate) fn can_open(coin: &str, risk: f64) -> bool {
    let cap = env_f("FAMILY_MAX_OPEN", 12.0) as usize;
    let risk_cap = env_f("FAMILY_RISK_CAP_PCT", 10.0) / 100.0;
    let eq = equity();
    let g = OPEN.lock().unwrap();
    let (n, open_risk, held) = g.as_ref().map_or((0, 0.0, false), |m| (m.len(), m.values().map(|x| x.1).sum::<f64>(), m.contains_key(coin)));
    !held && (cap == 0 || n < cap) && (risk_cap <= 0.0 || eq <= 0.0 || open_risk + risk <= risk_cap * eq + 1e-9)
}

pub(crate) fn register(coin: &str, kind: &str, risk: f64) {
    OPEN.lock().unwrap().get_or_insert_with(Default::default).insert(coin.to_string(), (kind.to_string(), risk));
}

pub(crate) fn unregister(coin: &str) {
    if let Some(m) = OPEN.lock().unwrap().as_mut() {
        m.remove(coin);
    }
}

/// The family's line for the chat: the sizing and the cap.
pub(crate) fn sizing_label(usd: f64, stop: f64) -> String {
    let pct = env_f("FAMILY_RISK_PCT", 0.0);
    if pct > 0.0 { format!("{pct:.1}% депозита на сделку (позиция {:.0}% депозита)", pct / stop) } else { format!("${usd:.0} на сделку (позиция ${:.0})", usd / stop) }
}

/// The size for a `risk_usd` loss at a `stop` (fraction) from `px`: positive.
pub(crate) fn size_for(risk_usd: f64, stop: f64, px: f64) -> f64 {
    if px > 0.0 && stop > 0.0 { risk_usd / stop / px } else { 0.0 }
}

/// Sets a HIP-3 coin's isolated leverage once (`done` remembers the coins set).
pub(crate) async fn h3_leverage(v: &Venue, coin: &str, max_lev: u32, done: &mut HashSet<String>) -> Result<()> {
    if !coin.contains(':') || done.contains(coin) {
        return Ok(());
    }
    v.leverage(coin, H3_LEVERAGE.min(max_lev.max(1)), false).await?;
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

/// A trade whose position is gone (held now: the account's signed size in its coin): how it
/// ended, the price, and its other order cancelled. None: still held.
pub(crate) async fn gone(v: &Venue, t: &Trade, held: f64) -> Option<(&'static str, f64)> {
    if held * t.size > 0.0 {
        return None;
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
    fn sizes_and_returns() {
        assert!((size_for(20.0, 0.2, 50.0) - 2.0).abs() < 1e-12);     // $100 at $50
        assert_eq!(size_for(20.0, 0.0, 50.0), 0.0);
        let t = Trade { size: -2.0, entry_px: 100.0, ..Default::default() };
        assert!((t.ret(90.0) - 0.10).abs() < 1e-12);                   // a short up 10%
        let l = Trade { size: 2.0, entry_px: 100.0, ..Default::default() };
        assert!((l.ret(105.0) - 0.05).abs() < 1e-12);
    }
}
