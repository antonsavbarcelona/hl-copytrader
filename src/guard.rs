//! The drawdown guard (2026-11-15, the user's call: "не готов терпеть глубокие ямы"): the family's risk a trade halved
//! while the account is more than `CUT_DD` under its peak, back to full once it is less than `BACK_DD` under it.
//! Scratchpad rsiclu/ddcut.py / ddcut2.py, the whole portfolio at 2% with the 40% cap: HL 2025-26 +300%/yr, max DD 38%,
//! P(DD > 30% in a year) 50% -> +224%, 27%, 17%; Binance 2021-26 +287%, 51%, 41% -> +185%, 37%, 26%. Back at once
//! (not at a new peak: +152% / +178%, nor at DD < 5%: +176% / +184%). The worst day is not changed (a crash is faster).
//!
//! The peak is the equity's (the account's value, the positions' open P&L in) net of the deposits and withdrawals
//! (the account's ledger, USDC only): a withdrawal is no loss, a deposit no gain. The equity comes from the strategies'
//! reads (`trades::set_equity`); a reading counts for the peak only with the one after it as high (a read's glitch or a
//! deposit seen before the ledger: no false peak), and the risk changes only after two checks in a row agree.
//!
//! A flow moves the peak in proportion (2026-11-18, the user's call): $5,002 of $9,049 out at 5.8% under the peak took
//! the peak down by the dollars, $9,607 -> $4,605, and the same $558 under it read 12.1%: the risk cut. Now the peak goes
//! with the equity's share moved (x (E + flow) / E, E the equity a reading at least `FLOW_PRE_S` before the flow saw);
//! without such a reading, by the dollars as before.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{now, Api};
use crate::config::Config;
use crate::log;
use crate::store::Store;

/// Under the peak by more than this: the risk halved.
const CUT_DD: f64 = 0.10;
/// Back to the full risk under this.
const BACK_DD: f64 = 0.095;
/// The risk's share while cut.
const CUT_X: f64 = 0.5;
const CHECK_S: f64 = 60.0;
const DOC: &str = "guard";
/// The equity before a flow: a reading at least this long before its ledger time (the equity sees a flow first at times).
const FLOW_PRE_S: f64 = 30.0;
/// The readings kept for the equity before a flow.
const HIST_S: f64 = 7200.0;
/// The 2026-11-18 withdrawal taken off by the dollars (the saved hwm, flows): its peak moved in proportion once, the
/// equity before it $9,049.41 ($4,047.41 after + $5,002 out).
const FIX_2026_11_18: (f64, f64, f64) = (9607.36008537, -5002.0, 9049.41);

/// What the guard keeps over a restart.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct Saved {
    /// The peak of the equity net of the flows (`flows` taken off).
    pub(crate) hwm: f64,
    /// The deposits less the withdrawals since the guard began ($).
    pub(crate) flows: f64,
    /// The ledger read up to (ms).
    pub(crate) ledger_ms: f64,
    pub(crate) cut: bool,
}

#[derive(Debug, Default)]
pub(crate) struct Guard {
    pub(crate) s: Saved,
    /// The equity readings (s, $) not yet counted for the peak.
    readings: Vec<(f64, f64)>,
    /// The last counted reading's net equity (the next one counts with it).
    prev_net: Option<f64>,
    /// The flows read lately (s, $): a reading before one is net of the flows up to it.
    recent: Vec<(f64, f64)>,
    /// The latest equity.
    last_eq: f64,
    /// The readings of the last `HIST_S` (s, $): the equity before a flow.
    hist: Vec<(f64, f64)>,
    /// Checks in a row that want the other side.
    streak: u32,
}

impl Guard {
    /// The flows up to `t`.
    fn flows_at(&self, t: f64) -> f64 {
        self.s.flows - self.recent.iter().filter(|(at, _)| *at > t).map(|(_, x)| x).sum::<f64>()
    }

    /// The account's peak now ($): the net peak with the flows.
    pub(crate) fn peak(&self) -> f64 {
        self.s.hwm + self.s.flows
    }

    /// Under the peak now (0..1; 0 without a peak).
    pub(crate) fn dd(&self) -> f64 {
        let p = self.peak();
        if p > 0.0 && self.last_eq > 0.0 { (1.0 - self.last_eq / p).max(0.0) } else { 0.0 }
    }

    fn seen(&mut self, t: f64, eq: f64) {
        if eq > 0.0 {
            self.readings.push((t, eq));
            if self.readings.len() > 500 {
                self.readings.remove(0);
            }
            self.last_eq = eq;
            self.hist.push((t, eq));
            self.hist.retain(|(at, _)| t - at <= HIST_S);
        }
    }

    /// The peak moved by a flow `x` at `t` in proportion to the equity it moved (`hwm` set so the peak is `P x (E + x) / E`
    /// with the flow in); without an equity read `FLOW_PRE_S` before it, by the dollars.
    fn scale_peak(&mut self, t: f64, x: f64) {
        let e = self.hist.iter().filter(|(at, _)| *at <= t - FLOW_PRE_S).map(|x| x.1).next_back();
        let p = self.peak();
        if let Some(e) = e.filter(|e| *e > 0.0 && e + x > 0.0 && p > 0.0) {
            self.s.hwm = p * (e + x) / e - (self.s.flows + x);
        }
    }

    /// The 2026-11-18 withdrawal's peak moved in proportion, once (`FIX_2026_11_18`): the saved state as it left it.
    fn fix_2026_11_18(&mut self) -> bool {
        let (hwm, flows, e) = FIX_2026_11_18;
        if (self.s.hwm - hwm).abs() > 0.01 || (self.s.flows - flows).abs() > 0.01 {
            return false;
        }
        let before = hwm;
        self.s.hwm = before * (e + flows) / e - flows;
        true
    }

    /// The flows (ms, $) read from the ledger up to `upto_ms`.
    fn flowed(&mut self, flows: &[(f64, f64)], upto_ms: f64) {
        for &(ms, x) in flows {
            if ms > self.s.ledger_ms {
                self.scale_peak(ms / 1000.0, x);
                self.s.flows += x;
                self.recent.push((ms / 1000.0, x));
            }
        }
        self.s.ledger_ms = self.s.ledger_ms.max(upto_ms);
        let t = upto_ms / 1000.0;
        self.recent.retain(|(at, _)| t - at < 3600.0);
    }

    /// The readings up to `upto` (s; the ledger read to it) counted for the peak: each with the one before it.
    fn count(&mut self, upto: f64) {
        let (old, new): (Vec<_>, Vec<_>) = std::mem::take(&mut self.readings).into_iter().partition(|(t, _)| *t <= upto);
        self.readings = new;
        for (t, eq) in old {
            let net = eq - self.flows_at(t);
            if let Some(p) = self.prev_net {
                self.s.hwm = self.s.hwm.max(p.min(net));
            }
            self.prev_net = Some(net);
        }
    }

    /// The check: the risk cut or back when two checks in a row want it; the change (true: cut) if any.
    fn decide(&mut self) -> Option<bool> {
        if self.peak() <= 0.0 || self.last_eq <= 0.0 {
            return None;
        }
        let dd = self.dd();
        let want = if self.s.cut { dd >= BACK_DD } else { dd > CUT_DD };
        if want == self.s.cut {
            self.streak = 0;
            return None;
        }
        self.streak += 1;
        if self.streak < 2 {
            return None;
        }
        self.streak = 0;
        self.s.cut = want;
        Some(want)
    }
}

#[cfg(not(test))]
static G: std::sync::Mutex<Option<Guard>> = std::sync::Mutex::new(None);

#[cfg(test)]
thread_local! {
    /// A test's own guard (the tests run on threads of one process).
    static G: std::cell::RefCell<Option<Guard>> = const { std::cell::RefCell::new(None) };
}

fn with<R>(f: impl FnOnce(&mut Guard) -> R) -> R {
    #[cfg(not(test))]
    {
        f(G.lock().unwrap().get_or_insert_with(Guard::default))
    }
    #[cfg(test)]
    {
        G.with(|g| f(g.borrow_mut().get_or_insert_with(Guard::default)))
    }
}

/// An equity reading (`trades::set_equity`).
pub(crate) fn seen(eq: f64) {
    with(|g| g.seen(now(), eq));
}

/// The risk's share now: `CUT_X` while cut, else 1.
pub(crate) fn scale() -> f64 {
    with(|g| if g.s.cut { CUT_X } else { 1.0 })
}

/// A test's guard cut (or not).
#[cfg(test)]
pub(crate) fn set_cut(cut: bool) {
    with(|g| g.s.cut = cut);
}

fn num(v: &Value) -> f64 {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_f64()).unwrap_or(0.0)
}

/// The deposits (+) and withdrawals (-) of the account `me` in the ledger's entries (ms, $): USDC only (a token comes
/// in as no equity; sold, the equity rises as a gain would); the account's own moves between its dexes none.
pub(crate) fn ledger_flows(entries: &Value, me: &str) -> Vec<(f64, f64)> {
    let me = me.to_lowercase();
    let mut out = Vec::new();
    for e in entries.as_array().into_iter().flatten() {
        let (d, ms) = (&e["delta"], e["time"].as_f64().unwrap_or(0.0));
        let x = match d["type"].as_str().unwrap_or("") {
            "deposit" => num(&d["usdc"]),
            "withdraw" => -(num(&d["usdc"]) + num(&d["fee"])),
            "send" | "spotTransfer" | "internalTransfer" | "subAccountTransfer" => {
                if d["token"].as_str().is_some_and(|t| t != "USDC") {
                    continue;
                }
                let amount = if d["amount"].is_null() { num(&d["usdc"]) } else { num(&d["amount"]) };
                let from = d["user"].as_str().unwrap_or("").to_lowercase();
                let to = d["destination"].as_str().unwrap_or("").to_lowercase();
                match (from == me, to == me) {
                    (true, false) => -amount,
                    (false, true) => amount,
                    _ => continue,
                }
            }
            _ => continue,
        };
        if x != 0.0 {
            out.push((ms, x));
        }
    }
    out
}

fn message(cut: bool, dd: f64, peak: f64, eq: f64) -> String {
    if cut {
        format!("🛡 ПРОСАДКА {:.1}% от пика (депозит ${eq:.0}, пик ${peak:.0}): риск новых сделок снижен вдвое (2% → 1%); \
                 открытые не трогаю. Вернётся к 2%, когда просадка будет меньше {:.1}%", dd * 100.0, BACK_DD * 100.0)
    } else {
        format!("✅ просадка снова {:.1}% (депозит ${eq:.0}, пик ${peak:.0}): риск новых сделок снова полный (2%)", dd * 100.0)
    }
}

/// One check: the ledger since its last read, the readings counted, the decision; the change told and saved.
async fn check(cfg: &Config, store: &Store, api: &Api, me: &str) {
    let since = with(|g| g.s.ledger_ms);
    let upto_ms = now() * 1000.0;
    match api.info(json!({"type": "userNonFundingLedgerUpdates", "user": me, "startTime": since as u64 + 1}), 20).await {
        Ok(v) => {
            let flows = ledger_flows(&v, me);
            if !flows.is_empty() {
                log!("guard: deposits / withdrawals {flows:?}");
            }
            // a ledger entry later than its read (the clocks): counted up to it
            let upto = flows.iter().map(|x| x.0).fold(upto_ms, f64::max);
            with(|g| {
                g.flowed(&flows, upto);
                g.count(upto_ms / 1000.0 - 5.0);
            });
        }
        Err(e) => log!("guard: the ledger not read: {e:#}"),
    }
    let (change, dd, peak, eq, saved) = with(|g| (g.decide(), g.dd(), g.peak(), g.last_eq, g.s.clone()));
    if let Some(cut) = change {
        let ev = json!({"kind": "guard", "what": if cut { "risk_cut" } else { "risk_back" }, "dd": dd, "peak": peak,
                        "equity": eq, "scale": if cut { CUT_X } else { 1.0 }, "at": now()});
        log!("guard: {ev}");
        crate::notify::send(message(cut, dd, peak, eq));
        store.event(ev);
    }
    crate::store::save_state(cfg, DOC, "guard", &saved);
}

/// The guard on the live account (`LIVE_ACCOUNT`): its state read before the strategies start (a cut kept over a
/// restart), then checked every `CHECK_S`.
pub(crate) async fn start(cfg: &Config, store: Store, api: Api) -> anyhow::Result<()> {
    let Some(me) = std::env::var("LIVE_ACCOUNT").ok().map(|x| x.trim().to_lowercase()).filter(|x| !x.is_empty()) else {
        log!("guard: LIVE_ACCOUNT not set, off");
        return Ok(());
    };
    let mut s: Saved = crate::store::load_state(cfg, DOC).await.unwrap_or_default();
    if s.ledger_ms <= 0.0 {
        s.ledger_ms = now() * 1000.0; // the guard begins now: the flows before it are in the equity already
    }
    with(|g| {
        g.s = s;
        if g.fix_2026_11_18() {
            log!("guard: the 2026-11-18 withdrawal's peak moved in proportion: peak ${:.0}", g.peak());
        }
        log!("guard: peak net ${:.0}, flows ${:.0}, risk {}", g.s.hwm, g.s.flows, if g.s.cut { "cut" } else { "full" });
    });
    let cfg = cfg.clone();
    crate::runner::born("guard", CHECK_S);
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(std::time::Duration::from_secs_f64(CHECK_S));
        loop {
            iv.tick().await;
            let t0 = now();
            check(&cfg, &store, &api, &me).await;
            crate::runner::beat("guard", t0);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(hwm: f64, flows: f64) -> Guard {
        Guard { s: Saved { hwm, flows, ..Saved::default() }, ..Guard::default() }
    }

    /// Readings fed and counted (all before `upto`).
    fn feed(g: &mut Guard, xs: &[(f64, f64)]) {
        for &(t, eq) in xs {
            g.seen(t, eq);
        }
        g.count(1e12);
    }

    #[test]
    fn the_peak_counts_a_reading_only_with_the_next_as_high() {
        let mut x = g(0.0, 0.0);
        feed(&mut x, &[(1.0, 10_000.0), (2.0, 13_000.0), (3.0, 10_100.0)]);
        // the 13,000 glitch alone: no peak at it
        assert_eq!(x.s.hwm, 10_100.0);
        feed(&mut x, &[(4.0, 11_000.0), (5.0, 11_000.0)]);
        assert_eq!(x.s.hwm, 11_000.0);
    }

    #[test]
    fn the_risk_is_cut_after_two_checks_over_10_percent_and_back_under_9_5() {
        let mut x = g(10_000.0, 0.0);
        x.seen(1.0, 8_950.0); // 10.5% under
        assert_eq!(x.decide(), None);
        assert_eq!(x.decide(), Some(true));
        assert!(x.s.cut);
        x.seen(2.0, 9_030.0); // 9.7%: still cut
        assert_eq!((x.decide(), x.decide()), (None, None));
        x.seen(3.0, 9_100.0); // 9.0%
        assert_eq!(x.decide(), None);
        x.seen(4.0, 8_900.0); // one check back over: the streak starts again
        assert_eq!(x.decide(), None);
        x.seen(5.0, 9_100.0);
        assert_eq!((x.decide(), x.decide()), (None, Some(false)));
        // exactly 10%: not cut
        x.seen(6.0, 9_000.0);
        assert_eq!((x.decide(), x.decide()), (None, None));
    }

    #[test]
    fn a_withdrawal_is_no_drawdown_and_a_deposit_no_peak() {
        let mut x = g(0.0, 0.0);
        feed(&mut x, &[(1.0, 10_000.0), (2.0, 10_000.0)]);
        // 3,000 out at 3 s; the equity sees it at 3.5 s, the ledger later
        x.seen(3.5, 7_000.0);
        x.seen(4.0, 7_000.0);
        x.flowed(&[(3000.0, -3000.0)], 10_000.0);
        x.count(9.0);
        assert_eq!((x.s.hwm, x.peak(), x.dd()), (10_000.0, 7_000.0, 0.0));
        assert_eq!((x.decide(), x.decide()), (None, None));
        // a 5,000 deposit seen in the equity before its ledger entry's time (12 s): no peak at 12,000 net
        x.seen(11.0, 12_000.0);
        x.seen(12.5, 12_000.0);
        x.flowed(&[(12_000.0, 5000.0)], 20_000.0);
        x.count(19.0);
        assert_eq!(x.s.hwm, 10_000.0);
        assert_eq!(x.peak(), 12_000.0);
        // then a loss of 1,300 of 12,000: 10.8% under -> cut
        x.seen(15.0, 10_700.0);
        assert!((x.dd() - 1300.0 / 12_000.0).abs() < 1e-9);
        assert_eq!((x.decide(), x.decide()), (None, Some(true)));
    }

    #[test]
    fn the_ledger_s_flows_usdc_in_and_out_not_the_own_moves_nor_the_tokens() {
        let me = "0xFE19";
        let v = json!([
            {"time": 1, "delta": {"type": "send", "user": "0xaaa", "destination": "0xfe19", "token": "USDC", "amount": "200.5"}},
            {"time": 2, "delta": {"type": "send", "user": "0xfe19", "destination": "0x2000", "token": "USDC", "amount": "230"}},
            {"time": 3, "delta": {"type": "send", "user": "0xfe19", "destination": "0xfe19", "sourceDex": "spot", "destinationDex": "", "token": "USDC", "amount": "50"}},
            {"time": 4, "delta": {"type": "spotTransfer", "user": "0x2222", "destination": "0xfe19", "token": "HYPE", "amount": "24", "usdcValue": "1626"}},
            {"time": 5, "delta": {"type": "deposit", "usdc": "1000"}},
            {"time": 6, "delta": {"type": "withdraw", "usdc": "300", "fee": "1"}},
            {"time": 7, "delta": {"type": "accountClassTransfer", "usdc": "10", "toPerp": true}},
            {"time": 8, "delta": {"type": "internalTransfer", "user": "0xfe19", "destination": "0xbbb", "usdc": "40"}}]);
        assert_eq!(ledger_flows(&v, me), vec![(1.0, 200.5), (2.0, -230.0), (5.0, 1000.0), (6.0, -301.0), (8.0, -40.0)]);
    }

    #[test]
    fn a_flow_moves_the_peak_in_proportion_to_the_equity_it_moved() {
        // a peak of 10,000, the equity 9,000 (10% under): 4,500 out -> the peak 5,000, still 10% under
        let mut x = g(10_000.0, 0.0);
        x.seen(100.0, 9_000.0);
        x.seen(200.0, 4_500.0);
        x.flowed(&[(150_000.0, -4500.0)], 300_000.0);
        assert!((x.peak() - 5_000.0).abs() < 1e-6, "{}", x.peak());
        assert!((x.dd() - 0.10).abs() < 1e-9);
        // a deposit of 4,500 back: the peak 10,000 again
        x.seen(400.0, 9_000.0);
        x.flowed(&[(350_000.0, 4500.0)], 500_000.0);
        assert!((x.peak() - 10_000.0).abs() < 1e-6, "{}", x.peak());
        // a reading too near the flow (< FLOW_PRE_S before it): by the dollars
        let mut y = g(10_000.0, 0.0);
        y.seen(140.0, 9_000.0);
        y.flowed(&[(150_000.0, -4500.0)], 300_000.0);
        assert!((y.peak() - 5_500.0).abs() < 1e-6);
        // old readings leave the history
        y.seen(140.0 + HIST_S + 1.0, 4_500.0);
        assert_eq!(y.hist.len(), 1);
    }

    #[test]
    fn the_2026_11_18_withdrawal_is_moved_once() {
        let mut x = g(FIX_2026_11_18.0, FIX_2026_11_18.1);
        assert!(x.fix_2026_11_18());
        // the peak 9,607 x 4,047.41 / 9,049.41 = 4,297: the equity 4,047 is 5.8% under, the risk back
        assert!((x.peak() - 4_297.0).abs() < 1.0, "{}", x.peak());
        x.s.cut = true;
        x.seen(1.0, 4_047.41);
        assert!((x.dd() - 0.058).abs() < 0.001);
        assert_eq!((x.decide(), x.decide()), (None, Some(false)));
        // once: the state it left no longer matches
        assert!(!x.fix_2026_11_18());
        assert!(!g(10_000.0, 0.0).fix_2026_11_18());
    }

    #[test]
    fn the_scale_halves_while_cut() {
        set_cut(false);
        assert_eq!(scale(), 1.0);
        set_cut(true);
        assert_eq!(scale(), 0.5);
        set_cut(false);
    }

    #[test]
    fn the_messages() {
        assert!(message(true, 0.105, 10_000.0, 8_950.0).contains("снижен вдвое"));
        assert!(message(false, 0.09, 10_000.0, 9_100.0).contains("снова полный"));
    }
}
