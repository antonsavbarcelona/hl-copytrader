//! dexarb: the DEX funding & basis arbitrage research engine (read-only: no keys, no orders, no transfers).
//!
//!   dexarb audit                       the ten venues' APIs checked live (deliverable 1)
//!   dexarb scan [--assets N] [--lev L] the ranked opportunities on the live books + funding history (deliverable 2)
//!   dexarb calc ASSET LONG SHORT USD HOURS [LEV]   one pair's full net PnL (deliverable 3)
//!   dexarb ladder ASSET LONG SHORT USD STEPS...    the averaging simulator on the live books (deliverable 4)
//!   dexarb history [--days D] [--assets N]        the funding history of the shared assets to data/funding (deliverable 5)
//!   dexarb research                    the funding-only / funding+basis studies on that history (deliverables 5, 7)
//!   dexarb collect [--every S]         the forward collector + the paper engine (deliverables 5 level D, 6)

mod averaging;
mod collect;
mod exec;
mod history;
mod http;
mod model;
mod paper;
mod pnl;
mod registry;
mod research;
mod scanner;
mod venues;

use anyhow::Result;
use model::*;
use std::path::PathBuf;
use venues::{ALL, V};

pub fn data_dir() -> PathBuf {
    let p = PathBuf::from(std::env::var("DEXARB_DATA").unwrap_or_else(|_| "data".into()));
    let _ = std::fs::create_dir_all(&p);
    p
}

fn arg(args: &[String], k: &str, def: f64) -> f64 {
    args.iter()
        .position(|a| a == k)
        .and_then(|i| args.get(i + 1))
        .and_then(|x| x.parse().ok())
        .unwrap_or(def)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let h = http::Http::new();
    match args.get(1).map(String::as_str).unwrap_or("scan") {
        "audit" => audit(&h).await,
        "scan" => scan(
            &h,
            arg(&args, "--assets", 40.0) as usize,
            arg(&args, "--lev", 3.0),
            true,
        )
        .await
        .map(|_| ()),
        "calc" => calc(&h, &args[2..]).await,
        "ladder" => averaging::cli(&h, &args[2..]).await,
        "history" => {
            history::run(
                &h,
                arg(&args, "--days", 30.0),
                arg(&args, "--assets", 40.0) as usize,
            )
            .await
        }
        "research" => research::run(),
        "collect" => {
            collect::run(
                &h,
                arg(&args, "--every", 60.0),
                arg(&args, "--assets", 25.0) as usize,
                arg(&args, "--hours", 0.0),
            )
            .await
        }
        x => {
            eprintln!("unknown command {x}");
            Ok(())
        }
    }
}

/// Deliverable 1: each venue's endpoints called for real; what works, what does not, the latency.
async fn audit(h: &http::Http) -> Result<()> {
    let mut rows = Vec::new();
    println!(
        "| venue | perps (active) | BTC book (levels, spread bp) | funding now | funding history 3 d (points) | fees | notes |"
    );
    println!("|---|---|---|---|---|---|---|");
    for v in ALL {
        let t0 = now_ms();
        let ins = v.instruments(h).await;
        let (n, act, btc) = match &ins {
            Ok(i) => (
                i.len(),
                i.iter().filter(|x| x.active).count(),
                i.iter().find(|x| x.base == "BTC" && x.active).cloned(),
            ),
            Err(_) => (0, 0, None),
        };
        let mut notes = Vec::new();
        if let Err(e) = &ins {
            notes.push(format!("instruments: {e:#}"));
        }
        let (mut bk, mut fnow, mut fh, mut fee) = (
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
            "-".to_string(),
        );
        if let Some(i) = &btc {
            fee = format!(
                "{:.3}% / {:.3}% ({})",
                f(i.fees.taker) * 100.0,
                f(i.fees.maker) * 100.0,
                match &i.fees.source {
                    Source::Api => "API".to_string(),
                    Source::Docs(_) => "docs".into(),
                    Source::Unavailable => "n/a".into(),
                }
            );
            match v.book(h, i).await {
                Ok(b) => {
                    bk = format!(
                        "{} / {} lv, {:.2} bp",
                        b.bids.len(),
                        b.asks.len(),
                        exec::spread(&b).unwrap_or(0.0) * 1e4
                    )
                }
                Err(e) => notes.push(format!("book: {e:#}")),
            }
            match v.funding_now(h, std::slice::from_ref(i)).await {
                Ok(x) => {
                    let me = x.iter().find(|y| y.symbol == i.symbol);
                    fnow = me.map_or("not listed".into(), |y| {
                        format!(
                            "{:+.5}%/h ({:+.1}% APR){}",
                            y.rate_h * 100.0,
                            y.rate_h * 8760.0 * 100.0,
                            if y.predicted_h.is_some() {
                                ", predicted"
                            } else {
                                ""
                            }
                        )
                    });
                }
                Err(e) => notes.push(format!("funding: {e:#}")),
            }
            match v
                .funding_history(h, i, now_ms() - 3 * 86_400_000, now_ms())
                .await
            {
                Ok(x) => fh = format!("{}", x.len()),
                Err(e) => {
                    fh = format!(
                        "no ({})",
                        format!("{e:#}").chars().take(60).collect::<String>()
                    )
                }
            }
        }
        let dt = (now_ms() - t0) as f64 / 1000.0;
        println!(
            "| {} | {} ({}) | {} | {} | {} | {} | {:.1} s{} |",
            v.name(),
            n,
            act,
            bk,
            fnow,
            fh,
            fee,
            dt,
            if notes.is_empty() {
                String::new()
            } else {
                format!("; {}", notes.join("; "))
            }
        );
        rows.push(serde_json::json!({"venue": v.name(), "instruments": n, "active": act, "btc_book": bk, "funding_now": fnow, "funding_hist_3d": fh, "fees": fee, "secs": dt, "notes": notes}));
    }
    std::fs::write(
        data_dir().join("audit.json"),
        serde_json::to_string_pretty(&rows)?,
    )?;
    Ok(())
}

/// Deliverable 2: the ranked opportunities.
pub async fn scan(
    h: &http::Http,
    max_assets: usize,
    lev: f64,
    print: bool,
) -> Result<Vec<scanner::Row>> {
    let reg = registry::Registry::load(h, &ALL).await;
    for (v, e) in &reg.errors {
        eprintln!("{}: {e}", v.name());
    }
    let qs = scanner::quotes(h, &reg, max_assets, &["BTC", "ETH", "SOL"]).await;
    let s1 = scanner::stage1(&qs, lev);
    // stage 2: the best 40 (pair, asset) by the stage-1 base net at 24 h, $1k, re-priced on 7 days of funding history
    let mut pairs: Vec<(String, String, String, f64)> = s1
        .iter()
        .filter(|r| r.hours == 24.0 && r.usd == 1_000.0)
        .map(|r| (r.asset.clone(), r.long.clone(), r.short.clone(), r.net[2]))
        .collect();
    pairs.sort_by(|a, b| b.3.total_cmp(&a.3));
    pairs.truncate(40);
    let t = now_ms();
    let mut rows = Vec::new();
    let mut cache: std::collections::HashMap<(String, String), Vec<FundingPoint>> =
        Default::default();
    for (asset, lv, sv, _) in &pairs {
        let Some(v) = qs.get(asset) else { continue };
        let (Some(a), Some(b)) = (
            v.iter().find(|q| &q.inst.venue == lv),
            v.iter().find(|q| &q.inst.venue == sv),
        ) else {
            continue;
        };
        for q in [a, b] {
            let k = (q.inst.venue.clone(), q.inst.symbol.clone());
            if !cache.contains_key(&k) {
                let vv = registry::venue_of(&q.inst);
                let x = vv
                    .funding_history(h, &q.inst, t - 7 * 86_400_000, t)
                    .await
                    .unwrap_or_default();
                cache.insert(k, x);
            }
        }
        let ha = &cache[&(a.inst.venue.clone(), a.inst.symbol.clone())];
        let hb = &cache[&(b.inst.venue.clone(), b.inst.symbol.clone())];
        let net = scanner::hourly_net(ha, a.inst.funding_interval_h, hb, b.inst.funding_interval_h);
        let series: Vec<f64> = net.iter().map(|x| x.1).collect();
        let neg = if series.is_empty() {
            1.0
        } else {
            series.iter().filter(|x| **x < 0.0).count() as f64 / series.len() as f64
        };
        let Some((lp, sg)) = scanner::legs_now(a, b) else {
            continue;
        };
        let sc = pnl::scenarios_from(&series, sg - lp);
        for &usd in &scanner::SIZES {
            for &hours in &scanner::HORIZONS {
                if let Some(r) =
                    scanner::row(asset, a, b, usd, hours, lev, sc, sg - lp, series.len(), neg)
                {
                    rows.push(r);
                }
            }
        }
    }
    // positive base-scenario net only
    rows.retain(|r| r.net[1] > 0.0);
    rows.sort_by(|a, b| b.net[1].total_cmp(&a.net[1]));
    let file = data_dir().join(format!("scan_{}.json", t / 1000));
    std::fs::write(&file, serde_json::to_string_pretty(&rows)?)?;
    std::fs::write(
        data_dir().join("scan_latest.json"),
        serde_json::to_string_pretty(&rows)?,
    )?;
    if print {
        println!(
            "{} assets quoted on 2+ venues (price-checked), {} stage-1 rows; {} positive after the history (saved {})",
            qs.len(),
            s1.len(),
            rows.len(),
            file.display()
        );
        println!(
            "{:<8} {:<11} {:<11} {:>8} {:>5} {:>8} {:>9} {:>9} {:>8} {:>8} {:>9} {:>9} {:>9} {:>8} {:>5} quality",
            "asset",
            "LONG",
            "SHORT",
            "size$",
            "h",
            "entry%",
            "fund%now",
            "fund%base",
            "in$",
            "out$",
            "be_h",
            "net_c",
            "net_b",
            "net_o",
            "risk"
        );
        let mut seen = std::collections::HashSet::new();
        for r in rows
            .iter()
            .filter(|r| seen.insert((r.asset.clone(), r.long.clone(), r.short.clone())))
            .take(30)
        {
            println!(
                "{:<8} {:<11} {:<11} {:>8.0} {:>5.0} {:>8.3} {:>9.1} {:>9.1} {:>8.2} {:>8.2} {:>9} {:>9.2} {:>9.2} {:>8.2} {:>5.0} {}",
                r.asset,
                r.long,
                r.short,
                r.usd,
                r.hours,
                r.entry_basis_pct,
                r.funding_apr_now,
                r.funding_apr_base,
                r.entry_cost,
                r.exit_cost,
                r.break_even_h.map_or("never".into(), |x| format!("{x:.1}")),
                r.net[0],
                r.net[1],
                r.net[2],
                r.risk,
                r.quality.chars().take(70).collect::<String>()
            );
        }
    }
    Ok(rows)
}

/// Deliverable 3: one pair decomposed.
async fn calc(h: &http::Http, a: &[String]) -> Result<()> {
    if a.len() < 5 {
        anyhow::bail!("calc ASSET LONG_VENUE SHORT_VENUE USD HOURS [LEV]");
    }
    let (asset, lv, sv) = (
        a[0].to_uppercase(),
        V::from_name(&a[1]).ok_or_else(|| anyhow::anyhow!("venue {}", a[1]))?,
        V::from_name(&a[2]).ok_or_else(|| anyhow::anyhow!("venue {}", a[2]))?,
    );
    let (usd, hours) = (a[3].parse::<f64>()?, a[4].parse::<f64>()?);
    let lev = a.get(5).and_then(|x| x.parse().ok()).unwrap_or(3.0);
    let reg = registry::Registry::load(h, &[lv, sv]).await;
    let pick = |v: V| {
        reg.by_venue
            .get(&v)
            .and_then(|x| x.iter().find(|i| i.base == asset && i.active))
            .cloned()
    };
    let (ia, ib) = (
        pick(lv).ok_or_else(|| anyhow::anyhow!("{asset} not on {}", lv.name()))?,
        pick(sv).ok_or_else(|| anyhow::anyhow!("{asset} not on {}", sv.name()))?,
    );
    let (ba, bb) = (lv.book(h, &ia).await?, sv.book(h, &ib).await?);
    let (fa, fb) = (
        lv.funding_now(h, std::slice::from_ref(&ia)).await?,
        sv.funding_now(h, std::slice::from_ref(&ib)).await?,
    );
    let fa = fa.into_iter().find(|x| x.symbol == ia.symbol);
    let fb = fb.into_iter().find(|x| x.symbol == ib.symbol);
    let t = now_ms();
    let (ha, hb) = (
        lv.funding_history(h, &ia, t - 7 * 86_400_000, t)
            .await
            .unwrap_or_default(),
        sv.funding_history(h, &ib, t - 7 * 86_400_000, t)
            .await
            .unwrap_or_default(),
    );
    let net = scanner::hourly_net(&ha, ia.funding_interval_h, &hb, ib.funding_interval_h);
    let series: Vec<f64> = net.iter().map(|x| x.1).collect();
    let qa = scanner::Quote {
        mid: f(exec::mid(&ba).unwrap_or_default() / ia.multiplier),
        inst: ia.clone(),
        book: ba.clone(),
        funding: fa.clone(),
    };
    let qb = scanner::Quote {
        mid: f(exec::mid(&bb).unwrap_or_default() / ib.multiplier),
        inst: ib.clone(),
        book: bb.clone(),
        funding: fb.clone(),
    };
    let (lp, sg) = scanner::legs_now(&qa, &qb).ok_or_else(|| anyhow::anyhow!("funding missing"))?;
    let sc = pnl::scenarios_from(&series, sg - lp);
    let p = pnl::Params {
        usd,
        hours,
        leverage: lev,
        reserve: 0.5,
        treasury_usd: scanner::treasury_per_trade(usd),
        basis_change: 0.0,
    };
    let Some((la, lb, r)) = pnl::calc(&ia, &ba, &ib, &bb, sc, &p) else {
        anyhow::bail!("no fill on one of the books")
    };
    println!(
        "{asset}: LONG {} ({}) / SHORT {} ({}), ${usd} a leg, {hours} h, {lev}x",
        ia.venue, ia.symbol, ib.venue, ib.symbol
    );
    println!(
        "  long  open VWAP {:.6} (best {:.6}, impact {:.2} bp, {} levels{}), close now {:.6}",
        f(la.open.vwap / ia.multiplier),
        f(la.open.best / ia.multiplier),
        la.open.impact * 1e4,
        la.open.levels,
        if la.open.complete { "" } else { ", INCOMPLETE" },
        f(la.close_now.vwap / ia.multiplier)
    );
    println!(
        "  short open VWAP {:.6} (best {:.6}, impact {:.2} bp, {} levels{}), close now {:.6}",
        f(lb.open.vwap / ib.multiplier),
        f(lb.open.best / ib.multiplier),
        lb.open.impact * 1e4,
        lb.open.levels,
        if lb.open.complete { "" } else { ", INCOMPLETE" },
        f(lb.close_now.vwap / ib.multiplier)
    );
    println!(
        "  quantity {:.6} {asset}; mid basis (short - long) {:+.4}%",
        r.q_base,
        r.mid_basis * 100.0
    );
    println!(
        "  entry basis {:+.4} $, exit basis (today's books) {:+.4} $, basis PnL {:+.4} $",
        r.entry_basis_usd, r.exit_basis_usd, r.basis_pnl
    );
    println!(
        "  entry cost {:.4} $, exit cost {:.4} $ (fees inside: {:.4} $, taker {:.3}% {} / {:.3}% {})",
        r.entry_cost,
        r.exit_cost,
        r.fees,
        la.taker * 100.0,
        la.fee_source.chars().take(4).collect::<String>(),
        lb.taker * 100.0,
        lb.fee_source.chars().take(4).collect::<String>()
    );
    println!(
        "  funding now: long pays {:+.5}%/h, short gets {:+.5}%/h -> net {:+.1}% APR; history {} h, negative {:.0}% of hours",
        lp * 100.0,
        sg * 100.0,
        (sg - lp) * 8760.0 * 100.0,
        series.len(),
        if series.is_empty() {
            100.0
        } else {
            series.iter().filter(|x| **x < 0.0).count() as f64 / series.len() as f64 * 100.0
        }
    );
    for (k, nm) in ["conservative", "base", "optimistic"].iter().enumerate() {
        println!(
            "  {nm:<12} funding {:+.4} $  net {:+.4} $  ROI {:+.3}%  APR {:+.1}%",
            r.funding[k],
            r.net[k],
            r.roi[k] * 100.0,
            r.apr[k] * 100.0
        );
    }
    println!(
        "  treasury (amortised) {:.4} $; capital (both legs' margin + 50% reserve) {:.2} $; break-even {}; a leg liquidates on its own at ~{:.0}% adverse",
        r.treasury,
        r.capital,
        r.break_even_h
            .map_or("never (base funding <= 0)".into(), |x| format!("{x:.1} h")),
        r.liq_move * 100.0
    );
    for b in [2.0, 3.0] {
        let p2 = pnl::Params {
            basis_change: r.mid_basis * (b - 1.0),
            ..p.clone()
        };
        if let Some((_, _, r2)) = pnl::calc(&ia, &ba, &ib, &bb, sc, &p2) {
            println!(
                "  stress: the basis x{b} at the exit -> net (base) {:+.4} $",
                r2.net[1]
            );
        }
    }
    if !r.warnings.is_empty() {
        println!("  data: {}", r.warnings.join("; "));
    }
    Ok(())
}
