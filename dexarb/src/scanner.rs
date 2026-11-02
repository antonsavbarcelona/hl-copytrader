//! The opportunity scanner: every shared underlying x every ordered venue pair x the size grid x the horizons, priced on
//! the live books and the funding, then the best candidates re-priced with their funding history (the scenarios).
//! A negative base-scenario net is never shown as an opportunity; an estimate resting on missing data says so.

use crate::exec;
use crate::http::Http;
use crate::model::*;
use crate::pnl::{self, FundingScenario, Params};
use crate::registry::{self, Registry};
use crate::venues::V;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};

/// A venue's funding or books not in within this: skipped for the round (a throttled venue does not stall the scan).
pub const VENUE_LIMIT_S: u64 = 40;
pub const SIZES: [f64; 10] = [
    100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 25_000.0, 50_000.0, 100_000.0,
];
pub const HORIZONS: [f64; 6] = [1.0, 3.0, 8.0, 24.0, 72.0, 168.0];

#[derive(Clone, Debug, Serialize)]
pub struct Quote {
    pub inst: Instrument,
    pub book: Book,
    pub funding: Option<FundingNow>,
    pub mid: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub asset: String,
    pub long: String,
    pub short: String,
    pub usd: f64,
    pub hours: f64,
    pub entry_basis_pct: f64,
    pub mid_basis_pct: f64,
    /// The current net funding (short's receipt - long's payment), APR on one leg's notional.
    pub funding_apr_now: f64,
    pub funding_apr_base: f64,
    pub entry_cost: f64,
    pub exit_cost: f64,
    pub fees: f64,
    pub treasury: f64,
    pub break_even_h: Option<f64>,
    pub net: [f64; 3],
    pub capital: f64,
    pub apr_base: f64,
    pub risk: f64,
    pub quality: String,
    pub history_h: usize,
    pub neg_share: f64,
}

/// The current funding of an instrument as (long pays / h, short gets / h).
pub fn legs_now(long: &Quote, short: &Quote) -> Option<(f64, f64)> {
    let fl = long.funding.as_ref()?;
    let fs = short.funding.as_ref()?;
    let lp = fl.long_cost_h.unwrap_or(fl.rate_h);
    let sg = fs.short_cost_h.map(|c| -c).unwrap_or(fs.rate_h);
    Some((lp, sg))
}

pub async fn quotes(
    h: &Http,
    reg: &Registry,
    max_assets: usize,
    must: &[&str],
) -> BTreeMap<String, Vec<Quote>> {
    let shared = reg.shared();
    let mut bases: Vec<(&String, &Vec<Instrument>)> = shared.iter().collect();
    bases.sort_by_key(|(b, v)| (!must.contains(&b.as_str()), std::cmp::Reverse(v.len())));
    bases.truncate(max_assets);
    let chosen: BTreeMap<String, Vec<Instrument>> = bases
        .into_iter()
        .map(|(b, v)| (b.clone(), v.clone()))
        .collect();
    // the funding, a call (or one per instrument) a venue
    let mut per_venue: HashMap<V, Vec<Instrument>> = HashMap::new();
    for v in chosen.values().flatten() {
        per_venue
            .entry(registry::venue_of(v))
            .or_default()
            .push(v.clone());
    }
    let lim = std::time::Duration::from_secs(VENUE_LIMIT_S);
    let fund = futures::future::join_all(per_venue.iter().map(|(v, ins)| async move {
        (
            *v,
            tokio::time::timeout(lim, v.funding_now(h, ins))
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("over {VENUE_LIMIT_S} s"))),
        )
    }))
    .await;
    let mut fmap: HashMap<(String, String), FundingNow> = HashMap::new();
    for (v, r) in fund {
        match r {
            Ok(x) => {
                for fnow in x {
                    fmap.insert((v.name().to_string(), fnow.symbol.clone()), fnow);
                }
            }
            Err(e) => eprintln!("funding {}: {e:#}", v.name()),
        }
    }
    // the books: the venues in parallel, each venue's instruments 4 at a time
    let jobs: Vec<(V, Vec<Instrument>)> = per_venue.into_iter().collect();
    let books = futures::future::join_all(jobs.iter().map(|(v, ins)| async move {
        let mut out = Vec::new();
        let t0 = std::time::Instant::now();
        for c in ins.chunks(4) {
            if t0.elapsed() > lim {
                eprintln!(
                    "{}: books over {VENUE_LIMIT_S} s, the rest skipped this round",
                    v.name()
                );
                break;
            }
            let r = futures::future::join_all(c.iter().map(|i| async move {
                (
                    i.clone(),
                    tokio::time::timeout(std::time::Duration::from_secs(10), v.book(h, i))
                        .await
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("book over 10 s"))),
                )
            }))
            .await;
            out.extend(r);
        }
        out
    }))
    .await;
    let mut out: BTreeMap<String, Vec<Quote>> = BTreeMap::new();
    for (i, b) in books.into_iter().flatten() {
        let Ok(book) = b else { continue };
        let Some(m) = exec::mid(&book) else { continue };
        let mid = f(m / i.multiplier);
        let funding = fmap.get(&(i.venue.clone(), i.symbol.clone())).cloned();
        out.entry(i.base.clone()).or_default().push(Quote {
            inst: i,
            book,
            funding,
            mid,
        });
    }
    // the price check: a ticker collision drops out
    for (_, qs) in out.iter_mut() {
        let ok = registry::price_consistent(
            &qs.iter()
                .map(|q| (q.inst.venue.clone(), q.mid))
                .collect::<Vec<_>>(),
        );
        qs.retain(|q| ok.contains(&q.inst.venue));
    }
    out.retain(|_, v| v.len() >= 2);
    out
}

pub fn treasury_per_trade(usd: f64) -> f64 {
    // the transfers' cost amortised: ~$2 a transfer a venue (gas + the venue's withdrawal fee, an estimate), two venues,
    // one deposit + one withdrawal, over the 20 round trips the capital is assumed to serve; never below $0.10
    (2.0 * 2.0 * 2.0 / 20.0f64).max(0.1) * (usd / 1_000.0).clamp(0.25, 1.0)
}

/// Every pair x size x horizon on the current funding only (stage 1).
pub fn stage1(qs: &BTreeMap<String, Vec<Quote>>, lev: f64) -> Vec<Row> {
    let mut rows = Vec::new();
    for (asset, v) in qs {
        for a in v {
            for b in v {
                if a.inst.venue == b.inst.venue {
                    continue;
                }
                let Some((lp, sg)) = legs_now(a, b) else {
                    continue;
                };
                let cur = sg - lp;
                for &usd in &SIZES {
                    for &hours in &HORIZONS {
                        let sc = pnl::scenarios_from(&[], cur);
                        if let Some(r) = row(asset, a, b, usd, hours, lev, sc, cur, 0, 0.0) {
                            rows.push(r);
                        }
                    }
                }
            }
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)]
pub fn row(
    asset: &str,
    a: &Quote,
    b: &Quote,
    usd: f64,
    hours: f64,
    lev: f64,
    sc: [FundingScenario; 3],
    cur: f64,
    hist: usize,
    neg: f64,
) -> Option<Row> {
    let p = Params {
        usd,
        hours,
        leverage: lev,
        reserve: 0.5,
        treasury_usd: treasury_per_trade(usd),
        basis_change: 0.0,
    };
    let (la, lb, r) = pnl::calc(&a.inst, &a.book, &b.inst, &b.book, sc, &p)?;
    if !r.executable {
        return None;
    }
    let (_la, _lb) = (la, lb);
    let ma = a.mid;
    let (entry_cost, exit_cost) = (r.entry_cost, r.exit_cost);
    let mut warn = r.warnings.clone();
    if hist == 0 {
        warn.push("no funding history: base = half the current".into());
    }
    let risk = risk_score(&a.inst, &b.inst, lev, neg, hist, &r.warnings);
    Some(Row {
        asset: asset.into(),
        long: a.inst.venue.clone(),
        short: b.inst.venue.clone(),
        usd,
        hours,
        entry_basis_pct: r.entry_basis_usd / (r.q_base * ma) * 100.0,
        mid_basis_pct: r.mid_basis * 100.0,
        funding_apr_now: cur * 8760.0 * 100.0,
        funding_apr_base: sc[1].short_gets_h * 8760.0 * 100.0,
        entry_cost,
        exit_cost,
        fees: r.fees,
        treasury: r.treasury,
        break_even_h: r.break_even_h,
        net: r.net,
        capital: r.capital,
        apr_base: r.apr[1] * 100.0,
        risk,
        quality: if warn.is_empty() {
            "ok".into()
        } else {
            warn.join("; ")
        },
        history_h: hist,
        neg_share: neg,
    })
}

pub fn risk_score(
    a: &Instrument,
    b: &Instrument,
    lev: f64,
    neg_share: f64,
    hist: usize,
    warn: &[String],
) -> f64 {
    let mut r = 0.0;
    r += (lev - 1.0).max(0.0) * 4.0;
    r += neg_share * 40.0;
    if hist < 72 {
        r += 15.0;
    }
    r += warn.len() as f64 * 5.0;
    for x in [a, b] {
        if x.collateral == "DUSD" {
            r += 10.0;
        }
        if x.venue == "GMX" {
            r += 10.0;
        }
    }
    r.min(100.0)
}

/// The hourly net funding of LONG a / SHORT b from both venues' histories (each settlement spread over its interval's
/// hours, before the settlement); only the hours both venues cover.
pub fn hourly_net(la: &[FundingPoint], ia: f64, lb: &[FundingPoint], ib: f64) -> Vec<(i64, f64)> {
    let expand = |v: &[FundingPoint], n: f64| -> BTreeMap<i64, f64> {
        let mut m = BTreeMap::new();
        let k = n.round().max(1.0) as i64;
        for p in v {
            let h = (p.ts_ms - 1) / 3_600_000;
            for j in 0..k {
                m.insert(h - j, p.rate_h);
            }
        }
        m
    };
    let (ma, mb) = (expand(la, ia), expand(lb, ib));
    ma.iter()
        .filter_map(|(h, ra)| mb.get(h).map(|rb| (*h, rb - ra)))
        .collect()
}
