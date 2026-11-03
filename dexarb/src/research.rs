//! The research (deliverables 5 and 7), honest about its level:
//!  * Level C (funding only: every venue's funding history, no historical books): the pairs' hourly funding net - its
//!    level, its stability, how often it flips - and strategy A (funding only, no averaging) walked forward: the entry
//!    rule decided on the trailing hours only, the funding realised over the next hours, the round trip's cost taken from
//!    the collector's books when there are any (else fees + an assumed 2 bp spread a leg, stated), the thresholds chosen
//!    on the first half and scored on the second. Baselines: always in, and no trading.
//!  * Level D (the collector's own snapshots): the executable basis per pair - its spread, its mean reversion (AR(1)
//!    half-life), strategy D (basis only) and B (funding + basis) on the snapshots, the ladders vs one entry.
//! The report goes to data/research.md.

use crate::collect::{self, Snap};
use crate::history;
use crate::scanner;
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;

/// The venues' maker fees (base tier; the sources as in the adapters) for the maker upper bound.
fn maker_fee(v: &str) -> f64 {
    match v {
        "Hyperliquid" => 0.00015,
        "Lighter" | "Extended" | "Aster" => 0.0,
        "GRVT" => 0.0003,
        "ApeX" => 0.0002,
        "edgeX" => 0.0004,
        "Pacifica" => 0.00015,
        "StandX" => 0.0001,
        _ => 0.0005,
    }
}

#[derive(Clone)]
struct Meta {
    venue: String,
    symbol: String,
    base: String,
    interval: f64,
    file: String,
    taker: f64,
}

fn mean(x: &[f64]) -> f64 {
    if x.is_empty() {
        0.0
    } else {
        x.iter().sum::<f64>() / x.len() as f64
    }
}

fn std(x: &[f64]) -> f64 {
    let m = mean(x);
    (x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (x.len().max(2) - 1) as f64).sqrt()
}

/// The round trip's cost (fraction of one leg's notional) of LONG a / SHORT b at $1k from the latest collected snapshots.
fn rt_from_snaps(snaps: &[Snap]) -> HashMap<(String, String), f64> {
    let mut last: HashMap<(String, String), &Snap> = HashMap::new();
    for s in snaps {
        last.insert((s.asset.clone(), s.venue.clone()), s);
    }
    last.into_iter()
        .filter(|(_, s)| s.buy1k > 0.0 && s.sell1k > 0.0)
        .map(|(k, s)| (k, (s.buy1k - s.sell1k) / s.mid + 2.0 * s.taker))
        .collect()
}

pub fn run() -> Result<()> {
    let dir = crate::data_dir().join("funding");
    let meta: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(dir.join("meta.json")).unwrap_or_else(|_| "[]".into()),
    )?;
    let meta: Vec<Meta> = meta
        .iter()
        .map(|m| Meta {
            venue: m["venue"].as_str().unwrap_or("").into(),
            symbol: m["symbol"].as_str().unwrap_or("").into(),
            base: m["base"].as_str().unwrap_or("").into(),
            interval: m["interval_h"].as_f64().unwrap_or(1.0),
            file: m["file"].as_str().unwrap_or("").into(),
            taker: m["taker"].as_f64().unwrap_or(0.0005),
        })
        .collect();
    let snaps = collect::load_all();
    let rt_leg = rt_from_snaps(&snaps);
    let mut by_base: BTreeMap<String, Vec<(Meta, Vec<crate::model::FundingPoint>)>> =
        BTreeMap::new();
    for m in &meta {
        let v = history::load(&dir, &m.file);
        if v.len() >= 24 {
            by_base
                .entry(m.base.clone())
                .or_default()
                .push((m.clone(), v));
        }
    }
    let mut rep = String::new();
    writeln!(rep, "# DEX funding / basis research\n")?;
    writeln!(
        rep,
        "Data: {} funding series over {} underlyings (level C); {} collected book snapshots (level D).\n",
        meta.len(),
        by_base.len(),
        snaps.len()
    )?;

    // ---- Level C: the pairs' funding net
    struct P {
        base: String,
        long: String,
        short: String,
        net: Vec<(i64, f64)>,
        rt: f64,
        rt_measured: bool,
        rt_maker: f64,
    }
    let mut pairs = Vec::new();
    for (base, v) in &by_base {
        for (ma, ha) in v {
            for (mb, hb) in v {
                if ma.venue == mb.venue {
                    continue;
                }
                let net = scanner::hourly_net(ha, ma.interval, hb, mb.interval);
                if net.len() < 72 {
                    continue;
                }
                let (ra, rb) = (
                    rt_leg.get(&(base.clone(), ma.venue.clone())),
                    rt_leg.get(&(base.clone(), mb.venue.clone())),
                );
                let (rt, measured) = match (ra, rb) {
                    (Some(a), Some(b)) => (a + b, true),
                    _ => (2.0 * (ma.taker + mb.taker) + 4e-4, false),
                };
                let rt_maker = 2.0 * (maker_fee(&ma.venue) + maker_fee(&mb.venue));
                pairs.push(P {
                    base: base.clone(),
                    long: ma.venue.clone(),
                    short: mb.venue.clone(),
                    net,
                    rt,
                    rt_measured: measured,
                    rt_maker,
                });
            }
        }
    }
    writeln!(
        rep,
        "## Level C: the funding net of {} ordered pairs (LONG on the first venue, SHORT on the second)\n",
        pairs.len()
    )?;
    writeln!(
        rep,
        "The net = the short venue's rate - the long venue's (fraction / h; + = the pair receives). APR = x 8760 on one leg's notional, before costs.\n"
    )?;
    writeln!(
        rep,
        "| asset | LONG | SHORT | hours | mean APR | median APR | std APR (hourly) | hours > 0 | P(next 24 h > 0 given last 24 h > 0) | round trip | 24 h mean needed |"
    )?;
    writeln!(rep, "|---|---|---|---|---|---|---|---|---|---|---|")?;
    let mut stats: Vec<(f64, String)> = Vec::new();
    for p in &pairs {
        let x: Vec<f64> = p.net.iter().map(|y| y.1).collect();
        let m = mean(&x);
        let mut s = x.clone();
        s.sort_by(|a, b| a.total_cmp(b));
        let med = s[s.len() / 2];
        let pos = x.iter().filter(|v| **v > 0.0).count() as f64 / x.len() as f64;
        let (mut hit, mut n) = (0, 0);
        for i in (24..x.len().saturating_sub(24)).step_by(24) {
            if mean(&x[i - 24..i]) > 0.0 {
                n += 1;
                if mean(&x[i..i + 24]) > 0.0 {
                    hit += 1;
                }
            }
        }
        let persist = if n > 0 {
            hit as f64 / n as f64
        } else {
            f64::NAN
        };
        if m > 0.0 {
            stats.push((m, format!("| {} | {} | {} | {} | {:+.1}% | {:+.1}% | {:.1}% | {:.0}% | {} | {:.2}%{} | {:+.1}% APR |", p.base, p.long, p.short, x.len(), m * 876_000.0, med * 876_000.0, std(&x) * 876_000.0,
                pos * 100.0, if n > 0 { format!("{:.0}% (n {n})", persist * 100.0) } else { "-".into() }, p.rt * 100.0, if p.rt_measured { "" } else { " (assumed)" }, p.rt / 24.0 * 876_000.0)));
        }
    }
    stats.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, l) in stats.iter().take(40) {
        writeln!(rep, "{l}")?;
    }
    writeln!(
        rep,
        "\n(the 40 best by the mean net; the mirror of each pair has the opposite sign)\n"
    )?;

    // ---- strategy A walked forward
    writeln!(rep, "## Strategy A (funding only), walked forward\n")?;
    writeln!(
        rep,
        "Enter a pair at hour t when its trailing 24 h mean net >= theta (decided on the hours before t only); hold H hours; the funding realised over (t, t+H]; minus the round trip. One position a pair at a time. The thresholds and H chosen on the first half of the hours, scored on the second.\n"
    )?;
    let all_t: Vec<i64> = pairs
        .iter()
        .flat_map(|p| p.net.iter().map(|x| x.0))
        .collect();
    let (t0, t1) = (
        all_t.iter().copied().min().unwrap_or(0),
        all_t.iter().copied().max().unwrap_or(0),
    );
    let mid = t0 + (t1 - t0) / 2;
    let thetas = [0.0, 0.10, 0.20, 0.30, 0.50, 1.0];
    let holds = [8usize, 24, 72, 168];
    let run_a = |theta_apr: f64, hold: usize, half: Option<bool>| -> (usize, f64, f64, f64) {
        let th = theta_apr / 8760.0;
        let (mut n, mut tot, mut wins, mut worst) = (0usize, 0.0, 0usize, 0.0f64);
        for p in &pairs {
            let x = &p.net;
            let mut i = 24;
            while i + hold <= x.len() {
                let in_half = half.is_none_or(|h| (x[i].0 < mid) == h);
                // only contiguous hours count
                let contiguous = x[i + hold - 1].0 - x[i - 24].0 == (hold + 23) as i64;
                let trail = mean(&x[i - 24..i].iter().map(|y| y.1).collect::<Vec<_>>());
                if in_half && contiguous && trail >= th {
                    let r = x[i..i + hold].iter().map(|y| y.1).sum::<f64>() - p.rt;
                    n += 1;
                    tot += r;
                    wins += (r > 0.0) as usize;
                    worst = worst.min(r);
                    i += hold;
                } else {
                    i += 1;
                }
            }
        }
        (
            n,
            tot,
            if n > 0 { wins as f64 / n as f64 } else { 0.0 },
            worst,
        )
    };
    writeln!(
        rep,
        "| theta (APR) | H | half 1: trades / mean bp / win | half 2: trades / mean bp / win / worst bp |"
    )?;
    writeln!(rep, "|---|---|---|---|")?;
    let mut best = (f64::MIN, 0.0, 0usize);
    for &th in &thetas {
        for &hd in &holds {
            let (n1, s1, w1, _) = run_a(th, hd, Some(true));
            let (n2, s2, w2, wo2) = run_a(th, hd, Some(false));
            let m1 = if n1 > 0 { s1 / n1 as f64 * 1e4 } else { 0.0 };
            if n1 >= 20 && m1 > best.0 {
                best = (m1, th, hd);
            }
            writeln!(
                rep,
                "| {:.0}% | {hd} h | {n1} / {m1:+.1} / {:.0}% | {n2} / {:+.1} / {:.0}% / {:+.0} |",
                th * 100.0,
                w1 * 100.0,
                if n2 > 0 { s2 / n2 as f64 * 1e4 } else { 0.0 },
                w2 * 100.0,
                wo2 * 1e4
            )?;
        }
    }
    let (n2, s2, w2, wo2) = run_a(best.1, best.2, Some(false));
    writeln!(
        rep,
        "\n**Chosen on half 1:** theta {:.0}% APR, H {} h (half 1 mean {:+.1} bp a trade). **Half 2 (out of sample):** {} trades, mean {:+.1} bp, win {:.0}%, worst {:+.0} bp.\n",
        best.1 * 100.0,
        best.2,
        best.0,
        n2,
        if n2 > 0 { s2 / n2 as f64 * 1e4 } else { 0.0 },
        w2 * 100.0,
        wo2 * 1e4
    )?;
    let (na, sa, wa, _) = run_a(-1e9, 24, None);
    writeln!(
        rep,
        "Baseline always in (every pair, every 24 h, both directions): {} trades, mean {:+.1} bp, win {:.0}%. No trading: 0.\n",
        na,
        if na > 0 { sa / na as f64 * 1e4 } else { 0.0 },
        wa * 100.0
    )?;
    writeln!(
        rep,
        "bp = of one leg's notional per trade. The capital is ~2 legs x (1 / leverage) x 1.5 (reserve): at 3x a trade's return on the capital is ~ the bp x 1.0.\n"
    )?;

    // ---- strategy A-long: the persistent pairs chosen on half 1, held through half 2 (one round trip)
    writeln!(rep, "## Strategy A-long: pick on half 1, hold all of half 2 (one round trip)\n")?;
    writeln!(rep, "A pair qualifies when on half 1 its mean net >= theta and its hours > 0 >= the share; it is opened at the mid-point and held to the end (one round trip, taker both legs both ways, the costs as above); one pair an asset (the best on half 1). The capital lies in the hedge for the whole half: this is carry, it locks the margin.\n")?;
    writeln!(rep, "| theta (APR) | share > 0 | pairs | mean half-2 net bp (taker) | positive | total bp | mean bp, MAKER upper bound | positive | half-2 hours |")?;
    writeln!(rep, "|---|---|---|---|---|---|---|---|---|")?;
    for &(th, share) in &[(0.05, 0.7), (0.10, 0.7), (0.10, 0.9), (0.15, 0.9), (0.20, 0.8), (0.0, 0.0)] {
        let mut best_by_asset: HashMap<String, (f64, f64, usize, f64)> = HashMap::new();
        for p in &pairs {
            let h1: Vec<f64> = p.net.iter().filter(|x| x.0 < mid).map(|x| x.1).collect();
            let h2: Vec<f64> = p.net.iter().filter(|x| x.0 >= mid).map(|x| x.1).collect();
            if h1.len() < 100 || h2.len() < 100 {
                continue;
            }
            let (m1, pos1) = (mean(&h1), h1.iter().filter(|v| **v > 0.0).count() as f64 / h1.len() as f64);
            if m1 * 8760.0 >= th && pos1 >= share && m1 > 0.0 {
                let r = h2.iter().sum::<f64>() - p.rt;
                let rm = h2.iter().sum::<f64>() - p.rt_maker;
                let e = best_by_asset.entry(p.base.clone()).or_insert((f64::MIN, 0.0, 0, 0.0));
                if m1 > e.0 {
                    *e = (m1, r, h2.len(), rm);
                }
            }
        }
        let rs: Vec<f64> = best_by_asset.values().map(|x| x.1).collect();
        let hrs = mean(&best_by_asset.values().map(|x| x.2 as f64).collect::<Vec<_>>());
        let rm: Vec<f64> = best_by_asset.values().map(|x| x.3).collect();
        writeln!(rep, "| {:.0}% | {:.0}% | {} | {:+.1} | {:.0}% | {:+.0} | {:+.1} | {:.0}% | {:.0} |", th * 100.0, share * 100.0, rs.len(), mean(&rs) * 1e4,
            rs.iter().filter(|v| **v > 0.0).count() as f64 / rs.len().max(1) as f64 * 100.0, rs.iter().sum::<f64>() * 1e4, mean(&rm) * 1e4,
            rm.iter().filter(|v| **v > 0.0).count() as f64 / rm.len().max(1) as f64 * 100.0, hrs)?;
    }
    writeln!(rep, "\n(theta 0 / share 0 = every pair positive on half 1: the baseline of picking by the sign alone)\n")?;

    // ---- Level D: the basis on the collector's snapshots
    writeln!(
        rep,
        "## Level D: the executable basis on the collector's snapshots\n"
    )?;
    if snaps.is_empty() {
        writeln!(rep, "No snapshots yet (run `dexarb collect`).")?;
    } else {
        basis_study(&snaps, &mut rep)?;
    }
    std::fs::write(crate::data_dir().join("research.md"), &rep)?;
    println!("{rep}");
    Ok(())
}

/// The basis per pair over the snapshots: the mid basis (B / A - 1), the executable entry ($1k: sell B - buy A), its
/// mean, spread, AR(1) half-life; strategy D (enter when the executable entry beats the round trip + the pair's own
/// median basis by k std, out at the median or 24 h).
fn basis_study(snaps: &[Snap], rep: &mut String) -> Result<()> {
    let mut by: BTreeMap<(String, i64), Vec<&Snap>> = BTreeMap::new();
    for s in snaps.iter().filter(|s| s.age_ms < 60_000) {
        by.entry((s.asset.clone(), s.ts)).or_default().push(s);
    }
    let mut series: BTreeMap<(String, String, String), Vec<(i64, f64, f64, f64)>> = BTreeMap::new();
    for ((asset, ts), v) in &by {
        for a in v {
            for b in v {
                if a.venue == b.venue
                    || a.buy1k <= 0.0
                    || b.sell1k <= 0.0
                    || a.sell1k <= 0.0
                    || b.buy1k <= 0.0
                {
                    continue;
                }
                let mb = b.mid / a.mid - 1.0;
                let entry = (b.sell1k - a.buy1k) / a.mid - 2.0 * (a.taker + b.taker);
                let exit_cost = (a.mid - a.sell1k + b.buy1k - b.mid) / a.mid;
                series
                    .entry((asset.clone(), a.venue.clone(), b.venue.clone()))
                    .or_default()
                    .push((*ts, mb, entry, exit_cost));
            }
        }
    }
    let span_h = snaps.iter().map(|s| s.ts).max().unwrap_or(0)
        - snaps.iter().map(|s| s.ts).min().unwrap_or(0);
    writeln!(
        rep,
        "{} pair series over {:.1} h. The mid basis = SHORT venue's mid / LONG venue's - 1; the executable entry = (sell $1k on SHORT - buy $1k on LONG) / mid - both takers x 2 (the whole round trip's fees).\n",
        series.len(),
        span_h as f64 / 3.6e6
    )?;
    writeln!(
        rep,
        "| asset | LONG | SHORT | snaps | mid basis mean | std | max | executable entry max | half-life (snaps) | snaps with the executable entry > 0 | D at the signal: n / mean bp | D a snapshot later: n / mean bp / win |"
    )?;
    writeln!(rep, "|---|---|---|---|---|---|---|---|---|---|---|---|")?;
    let mut rows = Vec::new();
    for ((asset, l, s), v) in &series {
        if v.len() < 10 {
            continue;
        }
        let mb: Vec<f64> = v.iter().map(|x| x.1).collect();
        let (m, sd) = (mean(&mb), std(&mb));
        let emax = v.iter().map(|x| x.2).fold(f64::MIN, f64::max);
        // AR(1) on the deviations: x_t = phi x_{t-1}
        let dev: Vec<f64> = mb.iter().map(|x| x - m).collect();
        let (num, den) = dev.windows(2).fold((0.0, 0.0), |acc, w| {
            (acc.0 + w[0] * w[1], acc.1 + w[0] * w[0])
        });
        let phi = if den > 0.0 { num / den } else { 0.0 };
        let hl = if phi > 0.0 && phi < 1.0 {
            -(2f64.ln()) / phi.ln()
        } else {
            f64::INFINITY
        };
        // strategy D on the snapshots (executable both ways): the signal at snapshot i (the executable entry above the
        // trailing mean + 2 std of the mid basis AND positive after the whole round trip's fees), the entry at i + delay
        // (delay 1 = the next snapshot, ~60 s later: what a REST bot gets), out at the first snapshot the mid basis is
        // back at the trailing mean, or 24 h; r = the entry's executable basis - the exit's (the mid basis + the exit's spread)
        let run = |delay: usize| -> (usize, f64, usize) {
            let (mut n, mut tot, mut wins) = (0usize, 0.0, 0usize);
            let mut i = 20;
            while i + delay < v.len() {
                let past: Vec<f64> = v[i - 20..i].iter().map(|x| x.1).collect();
                let (pm, ps) = (mean(&past), std(&past));
                if v[i].2 > pm + 2.0 * ps && v[i].2 > 0.0 {
                    let e = i + delay;
                    let mut j = e + 1;
                    while j < v.len() && v[j].1 > pm && v[j].0 - v[e].0 < 86_400_000 {
                        j += 1;
                    }
                    let j = j.min(v.len() - 1);
                    let r = v[e].2 - (v[j].1 + v[j].3);
                    n += 1;
                    tot += r;
                    wins += (r > 0.0) as usize;
                    i = j + 1;
                } else {
                    i += 1;
                }
            }
            (n, tot, wins)
        };
        let ((n, tot, w0), (n1, tot1, w1)) = (run(0), run(1));
        let pos_now = v.iter().filter(|x| x.2 > 0.0).count();
        let pos_next = v.windows(2).filter(|w| w[0].2 > 0.0 && w[1].2 > 0.0).count();
        let _ = (w0, w1);
        let d1 = if n1 > 0 { format!("{n1} / {:+.1} / {:.0}%", tot1 / n1 as f64 * 1e4, w1 as f64 / n1 as f64 * 100.0) } else { "-".into() };
        let persist = if pos_now > 0 { format!("{pos_now} ({:.0}% still > 0 a snapshot later)", pos_next as f64 / pos_now as f64 * 100.0) } else { "0".into() };
        rows.push((emax, format!("| {asset} | {l} | {s} | {} | {:+.3}% | {:.3}% | {:+.3}% | {:+.3}% | {} | {persist} | {n} / {} | {d1} |", v.len(), m * 100.0, sd * 100.0, mb.iter().copied().fold(f64::MIN, f64::max) * 100.0, emax * 100.0,
            if hl.is_finite() { format!("{hl:.1}") } else { "none".into() }, if n > 0 { format!("{:+.1}", tot / n as f64 * 1e4) } else { "-".into() })));
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, r) in rows.iter().take(40) {
        writeln!(rep, "{r}")?;
    }
    writeln!(
        rep,
        "\n(the 40 pairs with the best executable entry seen; a positive executable entry is a pair one could have opened with the basis already paying the whole round trip's fees)"
    )?;
    Ok(())
}
