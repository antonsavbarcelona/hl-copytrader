//! The shorts' sub-account (2026-11-21, the user's call: "шорты будем только на саб аккаунте открывать"): the squeeze
//! short trades on the sub-account `SHORT_ACCOUNT` ("short", the live account's), so a short no longer meets the long
//! another strategy holds on the coin (Hyperliquid nets one position a coin an account: 2026-11 CASHCAT +34%, ZEC +18%
//! skipped behind the RSI dip's / dip's longs; V427: the other side free +1..2% on Binance, +7% on HL 2025-26 at 2%).
//!
//! The money: the live account is unified (one USDC: a `subAccountTransfer` is refused there, "Action disabled when
//! unified account is active"), and its API wallet (`LIVE_KEY`) trades for the sub-account but moves no money. So the
//! account's own key (`MAIN_KEY`) signs a `sendAsset`: the live account's spot USDC to the sub-account's perps before a
//! short (`fund`: its margin and its loss to the resting stop over what is free there), the sub-account's free USDC
//! over `keep` back after (`sweep`). Without `MAIN_KEY` nothing moves: a short goes on what is there, or is skipped and
//! told. Each move is told in the chat.
//!
//! The family counts both accounts: the equity (the risk a trade, the 40% cap, the guard) is the live account's plus the
//! sub-account's (`equity`), the open risk all trades', and a coin's place (`trades::can_open`) only the same
//! account's trades (a short here takes no long's place there).

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::api::num;
use crate::exchange::{Exchange, SendAsset};
use crate::log;
use crate::venue::Venue;

/// The sub-account "short" of the live account 0xfe19..2719 (made 2026-11-21 by the user; checked that day: the API
/// wallet's order and cancel for it went through, `sendAsset` by the account's key both ways).
pub(crate) const SHORT_ACCOUNT: &str = "0x123644e79020f028a0cd11886598fd6a403be4f4";
/// The strategies trading on it.
const KINDS: &[&str] = &["squeeze_short"];
/// USDC as `sendAsset` names it (the spot token 0).
const USDC: &str = "USDC:0x6d1e7cde53ba9467b783cb7c530ce054";
/// A move under this is not worth a transfer.
const MIN_MOVE: f64 = 5.0;
/// Moved over the shortfall: the margin's rounding and a mark's move between the read and the order.
const PAD: f64 = 1.05;

/// Whether `kind` trades on the sub-account.
pub(crate) fn on_sub(kind: &str) -> bool {
    KINDS.contains(&kind)
}

#[cfg(not(test))]
static EQUITY: std::sync::Mutex<f64> = std::sync::Mutex::new(0.0);
#[cfg(test)]
thread_local! {
    static EQUITY: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

/// The sub-account's equity as last read (0 before), the moves since added.
pub(crate) fn equity() -> f64 {
    #[cfg(not(test))]
    return *EQUITY.lock().unwrap();
    #[cfg(test)]
    return EQUITY.with(|e| e.get());
}

pub(crate) fn set_equity(x: f64) {
    if x >= 0.0 {
        #[cfg(not(test))]
        {
            *EQUITY.lock().unwrap() = x;
        }
        #[cfg(test)]
        EQUITY.with(|e| e.set(x));
    }
}

fn add_equity(dx: f64) {
    set_equity((equity() + dx).max(0.0));
}

/// The sub-account's venue: the live one's key acting for `SHORT_ACCOUNT`.
pub(crate) fn venue(main: &Venue) -> Result<Venue> {
    main.for_sub(SHORT_ACCOUNT)
}

/// The account's own key (`MAIN_KEY`, the live account's: it alone signs a money move), on the live venue's server.
#[derive(Clone)]
pub(crate) struct Mover {
    ex: Exchange,
    /// The live account (the money's home).
    main: String,
}

impl Mover {
    /// From `MAIN_KEY` (None: unset or empty) for the live account `main`, on `base`.
    pub(crate) fn from_env(base: &str, main: &str) -> Result<Option<Mover>> {
        let Some(key) = std::env::var("MAIN_KEY").ok().filter(|k| !k.trim().is_empty()) else { return Ok(None) };
        Ok(Some(Mover { ex: Exchange::new(base, &key).context("MAIN_KEY")?, main: main.to_lowercase() }))
    }

    #[cfg(test)]
    pub(crate) fn for_test(base: &str, main: &str) -> Mover {
        Mover { ex: Exchange::new(base, crate::testkit::KEY).unwrap(), main: main.to_string() }
    }

    async fn send(&self, t: SendAsset) -> Result<()> {
        self.ex.send_asset(t).await.map(|_| ())
    }
}

/// A $ amount as `sendAsset` takes it (cents, no float tail).
fn amount(x: f64) -> String {
    let s = format!("{:.2}", (x * 100.0).floor() / 100.0);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The sub-account's free USDC (its perps' withdrawable) and value.
async fn free(v: &Venue) -> Result<(f64, f64)> {
    let st = v.ex.info(json!({"type": "clearinghouseState", "user": v.account})).await?;
    Ok((num(&st["withdrawable"]), num(&st["marginSummary"]["accountValue"])))
}

/// At least `need` $ free on the sub-account `v` before a trade: the shortfall (and `PAD`) sent from the live account's
/// spot by `m`; Err (the trade skipped) when it is short and nothing may move or the move failed.
pub(crate) async fn fund(v: &Venue, m: Option<&Mover>, need: f64) -> Result<()> {
    let (have, value) = free(v).await?;
    set_equity(value);
    if have >= need {
        return Ok(());
    }
    let Some(m) = m else {
        bail!("на шорт-аккаунте свободно ${have:.0}, нужно ${need:.0}; MAIN_KEY не задан, перевести не могу");
    };
    let x = ((need - have) * PAD).max(MIN_MOVE).ceil();
    m.send(SendAsset { destination: v.account.clone(), source_dex: "spot".into(), destination_dex: String::new(), token: USDC.into(),
        amount: amount(x), from_sub_account: String::new(), nonce: 0 }).await.context("перевод на шорт-аккаунт")?;
    add_equity(x);
    log!("sub: ${x:.0} to the short account (free ${have:.2}, need ${need:.2})");
    crate::notify::send(format!("💸 на шорт-аккаунт ${x:.0} с основного (там было свободно ${have:.0}, нужно ${need:.0})"));
    Ok(())
}

/// The sub-account's free USDC over `keep` back to the live account's spot (a move under `MIN_MOVE` not made); the $
/// moved. Nothing without `m`.
pub(crate) async fn sweep(v: &Venue, m: Option<&Mover>, keep: f64) -> Result<f64> {
    let (have, value) = free(v).await?;
    set_equity(value);
    let Some(m) = m else { return Ok(0.0) };
    let x = ((have - keep).min(value - keep) * 100.0).floor() / 100.0;
    if x < MIN_MOVE {
        return Ok(0.0);
    }
    m.send(SendAsset { destination: m.main.clone(), source_dex: String::new(), destination_dex: "spot".into(), token: USDC.into(),
        amount: amount(x), from_sub_account: v.account.clone(), nonce: 0 }).await.context("возврат с шорт-аккаунта")?;
    add_equity(-x);
    log!("sub: ${x:.2} back to the main account (free ${have:.2}, kept ${keep:.2})");
    crate::notify::send(format!("💸 с шорт-аккаунта ${x:.0} обратно на основной (оставил ${keep:.0})"));
    Ok(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::FakeHl;

    #[test]
    fn the_amounts_in_cents_and_the_kinds() {
        assert_eq!((amount(12.5), amount(3.0), amount(7.019), amount(0.1 + 0.2)), ("12.5".into(), "3".into(), "7.01".into(), "0.3".into()));
        assert!(on_sub("squeeze_short") && !on_sub("squeeze") && !on_sub("rsidip"));
    }

    #[tokio::test]
    async fn a_shortfall_is_sent_over_and_the_surplus_back() {
        let f = FakeHl::start(&[("SBA", 2, 10, 100.0)]).await;
        f.with(|s| s.account_value = Some(40.0));
        let v = venue(&f.venue()).unwrap();
        assert_eq!(v.account, SHORT_ACCOUNT);
        let m = Mover::for_test(&f.base, "0x00000000000000000000000000000000000000aa");
        // enough free: nothing sent
        fund(&v, Some(&m), 30.0).await.unwrap();
        assert!(f.with(|s| s.actions.is_empty()));
        assert_eq!(equity(), 40.0);
        // short: the gap and 5% more, from the main's spot to the sub-account's perps
        fund(&v, Some(&m), 100.0).await.unwrap();
        let a = f.with(|s| s.actions.pop().unwrap());
        assert_eq!((a["type"].as_str(), a["destination"].as_str(), a["sourceDex"].as_str(), a["destinationDex"].as_str(), a["amount"].as_str(), a["fromSubAccount"].as_str()),
            (Some("sendAsset"), Some(SHORT_ACCOUNT), Some("spot"), Some(""), Some("63"), Some("")));
        assert_eq!(f.with(|s| s.account_value), Some(103.0));
        assert_eq!(equity(), 103.0);
        // without the key: short, the trade is skipped (the error told)
        let e = fund(&v, None, 500.0).await.unwrap_err();
        assert!(format!("{e:#}").contains("MAIN_KEY"), "{e:#}");
        // the surplus over `keep` back, from the sub-account to the main's spot; under $5 not moved
        assert_eq!(sweep(&v, Some(&m), 20.0).await.unwrap(), 83.0);
        let a = f.with(|s| s.actions.pop().unwrap());
        assert_eq!((a["destination"].as_str(), a["sourceDex"].as_str(), a["destinationDex"].as_str(), a["amount"].as_str(), a["fromSubAccount"].as_str()),
            (Some("0x00000000000000000000000000000000000000aa"), Some(""), Some("spot"), Some("83"), Some(SHORT_ACCOUNT)));
        assert_eq!(equity(), 20.0);
        assert_eq!(sweep(&v, Some(&m), 16.0).await.unwrap(), 0.0);
        assert_eq!(sweep(&v, None, 0.0).await.unwrap(), 0.0, "no key: nothing moves");
        // a refused move: an error, the equity unchanged
        f.with(|s| s.refuse_sends = true);
        assert!(fund(&v, Some(&m), 500.0).await.is_err());
        assert_eq!(equity(), 20.0);
    }

    #[tokio::test]
    async fn the_orders_go_for_the_sub_account() {
        let f = FakeHl::start(&[("SBB", 2, 10, 100.0)]).await;
        let v = venue(&f.venue()).unwrap();
        v.market("SBB", -1.0, false).await.unwrap();
        let body = f.with(|s| s.bodies.last().cloned().unwrap());
        assert_eq!(body["vaultAddress"].as_str(), Some(SHORT_ACCOUNT), "{body}");
        // the live venue's: none
        f.venue().market("SBB", 1.0, false).await.unwrap();
        assert!(f.with(|s| s.bodies.last().unwrap()["vaultAddress"].is_null()));
    }
}
