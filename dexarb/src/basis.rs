//! Level B basis research: 30 days of 15-minute closes on 8 venues (the trade prices; no depth - the executable costs come
//! from the collector's books or an explicit assumption), the cross-venue basis b = close_B / close_A - 1 per pair, and
//!  * strategy D (basis only): at bar t, when b_t is above its trailing 24 h mean by k trailing std AND by more than the
//!    round trip, LONG A / SHORT B at bar t's closes (the cost of crossing both books in and out = the round trip);
//!    out when b is back at the entry's trailing mean or after 24 h; k chosen on half 1, scored on half 2;
//!  * strategy C (funding + the basis): the same entries, the pair's funding over the hold added;
//!  * the ladders: A fixed (absolute basis 0.3 / 0.6 / 1.0 / 1.5%, out at 0.1%), C z-score (2 / 3 / 4 std, out at the mean)
//!    vs ONE entry of the same total size at the first step - per unit of the maximum exposure.
//! No look-ahead: every threshold from the bars before t.  `dexarb basis [--days D] [--assets N]`

use crate::http::Http;
use crate::model::*;
use crate::registry::{self, Registry};
use crate::venues::{candles, ALL, V};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;

fn mean(x: &[f64]) -> f64 {
    if x.is_empty() { 0.0 } else { x.iter().sum::<f64>() / x.len() as f64 }
}
fn std(x: &[f64]) -> f64 {
    let m = mean(x);
    (x.iter().map(|v| (v - m).powi(2)).sum::<f64>() / (x.len().max(2) - 1) as f64).sqrt()
}

pub async fn run(h: &Http, days: f64, assets: usize) -> Result<()> {
    let reg = Registry::load(h, &ALL).await;
    let shared = reg.shared();
    let mut bases: Vec<(&String, &Vec<Instrument>)> = shared.iter().collect();
    bases.sort_by_key(|(b, v)| (!["BTC", "ETH", "SOL"].contains(&b.as_str()), std::cmp::Reverse(v.len())));
    bases.truncate(assets);
    let t = now_ms();
    let from = t - (days * 86_400_000.0) as i64;
    let jobs: Vec<Instrument> = bases.iter().flat_map(|(_, v)| v.iter().cloned()).filter(|i| !matches!(registry::venue_of(i), V::Lighter | V::Gmx)).collect();
    let mut bars: BTreeMap<(String, String), BTreeMap<i64, f64>> = BTreeMap::new();
    let mut fails = 0;
    let bdir = crate::data_dir().join("bars");
    std::fs::create_dir_all(&bdir)?;
    let cached = |i: &Instrument| bdir.join(format!("{}__{}.csv", i.base, i.venue));
    let mut todo = Vec::new();
    for i in &jobs {
        let p = cached(i);
        let fresh = std::fs::metadata(&p).ok().and_then(|m| m.modified().ok()).is_some_and(|m| m.elapsed().map_or(false, |e| e.as_secs() < 12 * 3600));
        if fresh {
            let m: BTreeMap<i64, f64> = std::fs::read_to_string(&p).unwrap_or_default().lines().filter_map(|l| { let (a, b) = l.split_once(',')?; Some((a.parse().ok()?, b.parse().ok()?)) }).collect();
            bars.insert((i.base.clone(), i.venue.clone()), m);
        } else {
            todo.push(i.clone());
        }
    }
    for c in todo.chunks(5) {
        let r = futures::future::join_all(c.iter().map(|i| async move { (i.clone(), candles::candles(registry::venue_of(i), h, i, from, t).await) })).await;
        for (i, x) in r {
            match x {
                Ok(v) if v.len() > 200 => {
                    let m: BTreeMap<i64, f64> = v.into_iter().map(|b| (b.ts / candles::BAR_MS, b.close)).collect();
                    let body: String = m.iter().map(|(k, c)| format!("{k},{c}
")).collect();
                    let _ = std::fs::write(cached(&i), body);
                    bars.insert((i.base.clone(), i.venue.clone()), m);
                }
                Ok(v) => println!("  {} {}: only {} bars", i.venue, i.symbol, v.len()),
                Err(e) => {
                    fails += 1;
                    println!("  {} {}: {}", i.venue, i.symbol, format!("{e:#}").chars().take(100).collect::<String>());
                }
            }
        }
    }
    println!("{} series of 15-min bars ({} failed)", bars.len(), fails);
    // the round trip per (asset, venue) from the collector's last books, else taker x 2 + 2 bp a leg (assumed)
    let snaps = crate::collect::load_all();
    let mut rt_leg: HashMap<(String, String), f64> = HashMap::new();
    for s in &snaps {
        if s.buy1k > 0.0 && s.sell1k > 0.0 {
            rt_leg.insert((s.asset.clone(), s.venue.clone()), (s.buy1k - s.sell1k) / s.mid + 2.0 * s.taker);
        }
    }
    let taker: HashMap<(String, String), f64> = reg.by_venue.values().flatten().map(|i| ((i.base.clone(), i.venue.clone()), f(i.fees.taker))).collect();
    // the funding (level C files) for strategy C
    let fdir = crate::data_dir().join("funding");
    let fmeta: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(fdir.join("meta.json")).unwrap_or_else(|_| "[]".into())).unwrap_or_default();
    let fund: HashMap<(String, String), (Vec<FundingPoint>, f64)> = fmeta.iter().map(|m| {
        ((m["base"].as_str().unwrap_or("").to_string(), m["venue"].as_str().unwrap_or("").to_string()), (crate::history::load(&fdir, m["file"].as_str().unwrap_or("")), m["interval_h"].as_f64().unwrap_or(1.0)))
    }).collect();

    let mut rep = String::new();
    writeln!(rep, "# Level B: the cross-venue basis on 15-minute closes ({} days)\n", days)?;
    writeln!(rep, "Trade-price closes (GRVT: mid), no depth; the round trip from the collector's $1k books where it has them, else taker x 2 + 2 bp a leg (marked *). Bars with no trades dropped. {} series.\n", bars.len())?;
    let keys: Vec<(String, String)> = bars.keys().cloned().collect();
    let mut by_asset: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (a, v) in &keys {
        by_asset.entry(a.clone()).or_default().push(v.clone());
    }
    struct Pr {
        asset: String,
        long: String,
        short: String,
        b: Vec<(i64, f64)>,
        rt: f64,
        measured: bool,
        fnet: HashMap<i64, f64>,
    }
    let mut prs = Vec::new();
    for (asset, vs) in &by_asset {
        for la in vs {
            for sb in vs {
                if la == sb {
                    continue;
                }
                let (ba, bb) = (&bars[&(asset.clone(), la.clone())], &bars[&(asset.clone(), sb.clone())]);
                let raw: Vec<(i64, f64)> = ba.iter().filter_map(|(k, ca)| bb.get(k).map(|cb| (*k, cb / ca - 1.0))).collect();
                if raw.len() < 500 {
                    continue;
                }
                let mut srt: Vec<f64> = raw.iter().map(|x| x.1).collect();
                srt.sort_by(|a, b| a.total_cmp(b));
                let med = srt[srt.len() / 2];
                if med.abs() > 0.02 {
                    println!("  {asset} {la}/{sb}: median basis {:+.1}% - a unit or ticker mismatch, dropped", med * 100.0);
                    continue;
                }
                let b: Vec<(i64, f64)> = raw.into_iter().filter(|x| (x.1 - med).abs() <= 0.05).collect();
                let leg = |v: &String| rt_leg.get(&(asset.clone(), v.clone())).copied();
                let (rt, measured) = match (leg(la), leg(sb)) {
                    (Some(x), Some(y)) => (x + y, true),
                    _ => (2.0 * (taker.get(&(asset.clone(), la.clone())).unwrap_or(&0.0005) + taker.get(&(asset.clone(), sb.clone())).unwrap_or(&0.0005)) + 4e-4, false),
                };
                let fnet = match (fund.get(&(asset.clone(), la.clone())), fund.get(&(asset.clone(), sb.clone()))) {
                    (Some((fa, ia)), Some((fb, ib))) => crate::scanner::hourly_net(fa, *ia, fb, *ib).into_iter().collect(),
                    _ => HashMap::new(),
                };
                prs.push(Pr { asset: asset.clone(), long: la.clone(), short: sb.clone(), b, rt, measured, fnet });
            }
        }
    }
    // the basis stats
    writeln!(rep, "## The basis per pair (the 30 widest by std)\n")?;
    writeln!(rep, "| asset | LONG | SHORT | bars | mean | std | p1 | p99 | AR(1) half-life (bars) | round trip |")?;
    writeln!(rep, "|---|---|---|---|---|---|---|---|---|---|")?;
    let mut st: Vec<(f64, String)> = Vec::new();
    for p in prs.iter().filter(|p| p.long < p.short) {
        let x: Vec<f64> = p.b.iter().map(|y| y.1).collect();
        let (m, sd) = (mean(&x), std(&x));
        let mut s = x.clone();
        s.sort_by(|a, b| a.total_cmp(b));
        let dev: Vec<f64> = x.iter().map(|v| v - m).collect();
        let (num, den) = dev.windows(2).fold((0.0, 0.0), |a, w| (a.0 + w[0] * w[1], a.1 + w[0] * w[0]));
        let phi = if den > 0.0 { num / den } else { 0.0 };
        let hl = if phi > 0.0 && phi < 1.0 { -(2f64.ln()) / phi.ln() } else { f64::INFINITY };
        st.push((sd, format!("| {} | {} | {} | {} | {:+.3}% | {:.3}% | {:+.3}% | {:+.3}% | {} | {:.2}%{} |", p.asset, p.long, p.short, x.len(), m * 100.0, sd * 100.0, s[s.len() / 100] * 100.0, s[s.len() * 99 / 100] * 100.0,
            if hl.is_finite() { format!("{hl:.1}") } else { "none".into() }, p.rt * 100.0, if p.measured { "" } else { "*" })));
    }
    st.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (_, l) in st.iter().take(30) {
        writeln!(rep, "{l}")?;
    }
    // strategy D / C walked forward
    let tmin = prs.iter().flat_map(|p| p.b.first().map(|x| x.0)).min().unwrap_or(0);
    let tmax = prs.iter().flat_map(|p| p.b.last().map(|x| x.0)).max().unwrap_or(0);
    let mid = tmin + (tmax - tmin) / 2;
    const W: usize = 96;
    let run_d = |k: f64, half: Option<bool>, with_funding: bool, delay: usize| -> (usize, f64, f64, f64) {
        let (mut n, mut tot, mut wins, mut worst) = (0usize, 0.0, 0usize, 0.0f64);
        for p in &prs {
            let x = &p.b;
            let mut i = W;
            while i < x.len() {
                let past: Vec<f64> = x[i - W..i].iter().map(|y| y.1).collect();
                let (pm, ps) = (mean(&past), std(&past));
                let dev = x[i].1 - pm;
                let in_half = half.is_none_or(|h| (x[i].0 < mid) == h);
                if in_half && ps > 0.0 && dev > k * ps && dev > p.rt && i + delay < x.len() {
                    // the entry a bar late when delay = 1: a last-trade bounce / a stale close is gone by then
                    let e = i + delay;
                    let mut j = e + 1;
                    while j < x.len() && x[j].1 > pm && x[j].0 - x[i].0 < 96 {
                        j += 1;
                    }
                    let j = j.min(x.len() - 1);
                    let mut r = (x[e].1 - x[j].1) - p.rt;
                    if with_funding {
                        // the bars' hours: 4 bars an hour; the funding net of each hour held
                        let (h0, h1) = (x[e].0 * candles::BAR_MS / 3_600_000, x[j].0 * candles::BAR_MS / 3_600_000);
                        r += (h0..h1).filter_map(|hh| p.fnet.get(&hh)).sum::<f64>();
                    }
                    n += 1;
                    tot += r;
                    wins += (r > 0.0) as usize;
                    worst = worst.min(r);
                    i = j + 1;
                } else {
                    i += 1;
                }
            }
        }
        (n, tot, if n > 0 { wins as f64 / n as f64 } else { 0.0 }, worst)
    };
    writeln!(rep, "\n## Strategy D (basis only) and C (+ the funding), walked forward\n")?;
    writeln!(rep, "| k (trailing std) | D half 1: n / mean bp / win | D half 2: n / mean bp / win / worst | C half 2 (+ funding): n / mean bp / win | D half 2, the entry a bar (15 min) late: n / mean bp / win |")?;
    writeln!(rep, "|---|---|---|---|---|")?;
    let mut best = (f64::MIN, 0.0);
    for k in [1.5, 2.0, 2.5, 3.0, 4.0] {
        let (n1, s1, w1, _) = run_d(k, Some(true), false, 0);
        let (n2, s2, w2, wo2) = run_d(k, Some(false), false, 0);
        let (n3, s3, w3, _) = run_d(k, Some(false), true, 0);
        let (n4, s4, w4, _) = run_d(k, Some(false), false, 1);
        let m1 = if n1 > 0 { s1 / n1 as f64 * 1e4 } else { 0.0 };
        if n1 >= 20 && m1 > best.0 {
            best = (m1, k);
        }
        let mm = |s: f64, n: usize| if n > 0 { s / n as f64 * 1e4 } else { 0.0 };
        writeln!(rep, "| {k} | {n1} / {m1:+.1} / {:.0}% | {n2} / {:+.1} / {:.0}% / {:+.0} | {n3} / {:+.1} / {:.0}% | {n4} / {:+.1} / {:.0}% |", w1 * 100.0, mm(s2, n2), w2 * 100.0, wo2 * 1e4, mm(s3, n3), w3 * 100.0, mm(s4, n4), w4 * 100.0)?;
    }
    let (n2, s2, w2, _) = run_d(best.1, Some(false), false, 0);
    writeln!(rep, "\n**Chosen on half 1:** k {} (half 1 {:+.1} bp). **Half 2:** {} trades, {:+.1} bp, win {:.0}%.\n", best.1, best.0, n2, if n2 > 0 { s2 / n2 as f64 * 1e4 } else { 0.0 }, w2 * 100.0)?;

    // the ladders vs one entry, per unit of the maximum exposure
    writeln!(rep, "## The averaging ladders vs one entry (per unit of the maximum exposure, all 30 days)\n")?;
    writeln!(rep, "| ladder | episodes | layers filled (mean) | PnL / max exposure, bp (mean) | worst | ONE entry of the full size at the first step: mean / worst bp |")?;
    writeln!(rep, "|---|---|---|---|---|---|")?;
    for (name, absolute, steps, exit) in [("A fixed 0.3/0.6/1.0/1.5%, out 0.1%", true, vec![0.003, 0.006, 0.010, 0.015], 0.001), ("C z-score 2/3/4 std, out at the mean", false, vec![2.0, 3.0, 4.0], 0.0)] {
        let (mut eps, mut layers, mut pnl, mut worst, mut one, mut one_worst) = (0usize, 0usize, Vec::new(), 0.0f64, Vec::new(), 0.0f64);
        for p in &prs {
            let x = &p.b;
            let mut i = W;
            while i < x.len() {
                let past: Vec<f64> = x[i - W..i].iter().map(|y| y.1).collect();
                let (pm, ps) = (mean(&past), std(&past));
                let lvl = |s: f64| if absolute { s } else { pm + s * ps };
                let first = lvl(steps[0]);
                if ps > 0.0 && x[i].1 >= first && x[i].1 - (if absolute { exit } else { pm }) > p.rt {
                    let out_at = if absolute { exit } else { pm };
                    let mut filled = vec![x[i].1];
                    let mut j = i + 1;
                    while j < x.len() && x[j].1 > out_at && x[j].0 - x[i].0 < 4 * 96 {
                        if filled.len() < steps.len() && x[j].1 >= lvl(steps[filled.len()]) {
                            filled.push(x[j].1);
                        }
                        j += 1;
                    }
                    let j = j.min(x.len() - 1);
                    let exit_b = x[j].1;
                    let n = steps.len() as f64;
                    let r: f64 = filled.iter().map(|e| e - exit_b - p.rt).sum::<f64>() / n;
                    let r1 = (filled[0] - exit_b - p.rt) * n / n;
                    eps += 1;
                    layers += filled.len();
                    pnl.push(r);
                    worst = worst.min(r);
                    one.push(r1);
                    one_worst = one_worst.min(r1);
                    i = j + 1;
                } else {
                    i += 1;
                }
            }
        }
        writeln!(rep, "| {name} | {eps} | {:.2} | {:+.1} | {:+.0} | {:+.1} / {:+.0} |", layers as f64 / eps.max(1) as f64, mean(&pnl) * 1e4, worst * 1e4, mean(&one) * 1e4, one_worst * 1e4)?;
    }
    writeln!(rep, "\n(per unit of the maximum exposure: the ladder's PnL / its full size; the one entry holds the full size from the first step - more risk when the basis widens, the same exposure cap)")?;
    std::fs::write(crate::data_dir().join("research_basis.md"), &rep)?;
    println!("{rep}");
    Ok(())
}
