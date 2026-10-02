//! Copy accounts ranked by ROI combined with how often they traded
//! (activity score = ROI% x sqrt(our fills)), open positions at current mids, with how their
//! trades were copied (lag, price vs the trader's) and the risk they took.

use std::collections::HashMap;
use std::io::Write;

use anyhow::Result;
use serde_json::{Value, json};

use crate::api::{now, num};
use crate::config::Config;
use crate::engine::Trader;
use crate::stats::{LAG_BUCKETS, Stats, lag_q};

fn div(a: f64, b: f64) -> f64 {
    if b != 0.0 { a / b } else { 0.0 }
}

struct Row<'a> {
    addr: &'a str,
    t: &'a Trader,
    equity: f64,
    roi: f64,
    per_day: f64,
    score: f64,
    /// Its stats with the set-ups of its open trips counted in.
    stats: Stats,
}

impl Row<'_> {
    fn s(&self) -> &Stats {
        &self.stats
    }
    /// Our price vs the trader's, bps of the copied notional (+ = we paid more).
    fn slip_bps(&self) -> f64 {
        div(self.s().slip_usd, self.s().slip_notional) * 1e4
    }
    fn avg_entry_pct(&self) -> f64 {
        div(self.s().entry_pct_sum, self.s().entries as f64)
    }
    fn avg_risk_pct(&self) -> f64 {
        div(self.s().risk_pct_sum, self.s().trips as f64)
    }
    fn win_pct(&self) -> f64 {
        div(self.s().wins as f64, self.s().trips as f64) * 100.0
    }
    /// Of the trips with a known set-up: share with a stop, isolated, average planned risk
    /// (% of equity) and leverage setting.
    fn stop_pct(&self) -> f64 {
        div(self.s().with_stop as f64, self.s().planned as f64) * 100.0
    }
    fn isolated_pct(&self) -> f64 {
        div(self.s().isolated as f64, self.s().planned as f64) * 100.0
    }
    fn avg_plan_pct(&self) -> f64 {
        div(self.s().plan_pct_sum, self.s().planned as f64)
    }
    fn avg_lev(&self) -> f64 {
        div(self.s().lev_sum, self.s().planned as f64)
    }
}

pub async fn run(cfg: &Config, args: &[String]) -> Result<()> {
    let mut min_fills = 10u64;
    let mut top = 30usize;
    let mut i = 0;
    while i + 1 < args.len() {
        match args[i].as_str() {
            "--min-fills" => min_fills = args[i + 1].parse()?,
            "--top" => top = args[i + 1].parse()?,
            _ => {}
        }
        i += 2;
    }
    let traders: HashMap<String, Trader> = crate::store::load(cfg).await?;
    let mids: Value = reqwest::Client::new()
        .post("https://api.hyperliquid.xyz/info")
        .json(&json!({"type": "allMids"}))
        .send()
        .await?
        .json()
        .await?;
    let mid = |c: &str| mids.get(c).map(num);
    let mut rows = Vec::new();
    for (addr, t) in &traders {
        let equity = t.acct.equity(&mid);
        // Fills that followed its trades; the seed mirror of old positions does not count.
        let fills = t.copy_fills;
        let days = ((now() - t.enrolled_at) / 86400.0).max(1.0 / 24.0);
        let roi = (div(equity, t.acct.start) - 1.0) * 100.0;
        let mut stats = t.stats.clone();
        for p in t.trips.values().filter_map(|x| x.plan.as_ref()) {
            stats.add_plan(p);
        }
        rows.push(Row { addr, t, equity, roi, per_day: fills as f64 / days, score: roi * (fills as f64).sqrt(), stats });
    }

    let n = rows.len();
    let total_start: f64 = n as f64 * cfg.start_usd;
    let total_eq: f64 = rows.iter().map(|r| r.equity).sum();
    let liq = rows.iter().filter(|r| r.t.acct.liquidated).count();
    let traded: Vec<&Row> = rows.iter().filter(|r| r.t.copy_fills > 0).collect();
    let up = traded.iter().filter(|r| r.equity > cfg.start_usd).count();
    println!("copy accounts: {n} (${:.0} each), copied a trade {}, only the seed mirror so far {}, liquidated {liq}",
        cfg.start_usd, traded.len(), n - traded.len());
    println!("all accounts together: ${total_start:.0} -> ${total_eq:.0} ({:+.2}%), {up} of {} traded accounts up",
        (total_eq / total_start.max(1.0) - 1.0) * 100.0, traded.len());
    let sum = |f: &dyn Fn(&Trader) -> f64| -> f64 { rows.iter().map(|r| f(r.t)).sum() };
    let fees = sum(&|t| t.acct.fees);
    println!("fees paid ${fees:.2}, funding {:+.2}", sum(&|t| t.acct.funding));

    // Execution across all accounts.
    let mut hist = vec![0u32; LAG_BUCKETS];
    for r in &rows {
        for (h, x) in hist.iter_mut().zip(&r.s().lag_hist) {
            *h += x;
        }
    }
    let copies = sum(&|t| t.stats.copies as f64);
    let (slip, mv, impact, notional) =
        (sum(&|t| t.stats.slip_usd), sum(&|t| t.stats.move_usd), sum(&|t| t.stats.impact_usd), sum(&|t| t.stats.slip_notional));
    let (worse, better) = (sum(&|t| t.stats.worse as f64), sum(&|t| t.stats.better as f64));
    println!("execution, {copies:.0} copy fills on ${notional:.0}:");
    println!("  lag (its fill -> ours): median {:.1} s, p90 {:.1} s, p99 {:.1} s, max {:.1} s; of it the feed {:.2} s on average",
        lag_q(&hist, 0.5), lag_q(&hist, 0.9), lag_q(&hist, 0.99),
        rows.iter().map(|r| r.s().lag_max).fold(0.0, f64::max), div(sum(&|t| t.stats.feed_sum), copies));
    println!("  price vs the trader's: worse {:.0}%, better {:.0}%, same {:.0}% of fills; {:+.2} bps on average = ${slip:.2} \
        (price moved {:+.2} bps ${mv:.2}, book walked {:+.2} bps ${impact:.2}); taker fee for comparison {:.1} bps",
        div(worse, copies) * 100.0, div(better, copies) * 100.0, div(copies - worse - better, copies) * 100.0,
        div(slip, notional) * 1e4, div(mv, notional) * 1e4, div(impact, notional) * 1e4, cfg.taker_fee * 1e4);

    // Risk across all accounts.
    let entries = sum(&|t| t.stats.entries as f64);
    let trips = sum(&|t| t.stats.trips as f64);
    let max = |f: &dyn Fn(&Stats) -> f64| rows.iter().map(|r| f(r.s())).fold(0.0, f64::max);
    let median = |f: &dyn Fn(&Row) -> f64| {
        let mut v: Vec<f64> = traded.iter().map(|r| f(r)).collect();
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    println!("risk (median account / worst account):");
    println!("  bet (open or add, % of equity): average {:.1}% over all {entries:.0}; per account avg {:.1}% / {:.1}%, biggest {:.1}% / {:.1}%",
        div(sum(&|t| t.stats.entry_pct_sum), entries), median(&|r| r.avg_entry_pct()),
        traded.iter().map(|r| r.avg_entry_pct()).fold(0.0, f64::max), median(&|r| r.s().entry_pct_max), max(&|s| s.entry_pct_max));
    println!("  trips closed {trips:.0}, won {:.0}%; deepest point of a trip (% of equity at its open): avg {:.1}%, per account max {:.1}% / {:.1}%",
        div(sum(&|t| t.stats.wins as f64), trips) * 100.0, div(sum(&|t| t.stats.risk_pct_sum), trips),
        median(&|r| r.s().risk_pct_max), max(&|s| s.risk_pct_max));
    println!("  at once: positions {:.0} / {:.0}, leverage {:.1}x / {:.1}x; max drawdown {:.1}% / {:.1}%",
        median(&|r| r.s().max_open as f64), max(&|s| s.max_open as f64), median(&|r| r.s().max_gross_pct) / 100.0,
        max(&|s| s.max_gross_pct) / 100.0, median(&|r| r.s().max_dd_pct), max(&|s| s.max_dd_pct));
    let ssum = |f: &dyn Fn(&Stats) -> f64| -> f64 { rows.iter().map(|r| f(r.s())).sum() };
    let planned = ssum(&|s| s.planned as f64);
    println!("  trader's set-up, {planned:.0} trips read (open ones too): with a stop {:.0}%, isolated {:.0}%, leverage setting avg {:.1}x max {:.0}x;\n  planned risk (to its stops, else liquidation, else all of it; % of equity) avg {:.1}%, per account max {:.1}% / {:.1}%",
        div(ssum(&|s| s.with_stop as f64), planned) * 100.0, div(ssum(&|s| s.isolated as f64), planned) * 100.0,
        div(ssum(&|s| s.lev_sum), planned), max(&|s| s.lev_max), div(ssum(&|s| s.plan_pct_sum), planned),
        median(&|r| r.s().plan_pct_max), max(&|s| s.plan_pct_max));

    let mut ranked: Vec<&Row> = rows.iter().filter(|r| r.t.copy_fills >= min_fills).collect();
    ranked.sort_by(|a, b| b.score.total_cmp(&a.score));
    println!("top {top} by ROI x sqrt(fills), at least {min_fills} fills ({} qualify):", ranked.len());
    println!("  {:42} {:>8} {:>7} {:>5} {:>5} {:>6} {:>6} {:>6} {:>6} {:>6} {:>5} {:>6} {:>6} {:>4} {:>5} {:>5} {:>5} {:>5} name",
        "account", "equity", "roi%", "fills", "lag50", "slipbp", "bet%", "bet^%", "risk%", "risk^%", "stop%", "plan%", "plan^%",
        "pos^", "lev^", "dd%", "trips", "win%");
    for r in ranked.iter().take(top) {
        let s = r.s();
        println!("  {:42} {:8.2} {:+7.2} {:5} {:5.1} {:+6.1} {:6.1} {:6.1} {:6.1} {:6.1} {:5.0} {:6.1} {:6.1} {:4} {:5.1} {:5.1} {:5} {:5.0} {}{}",
            r.addr, r.equity, r.roi, r.t.copy_fills, s.lag_q(0.5), r.slip_bps(), r.avg_entry_pct(), s.entry_pct_max,
            r.avg_risk_pct(), s.risk_pct_max, r.stop_pct(), r.avg_plan_pct(), s.plan_pct_max, s.max_open, s.max_gross_pct / 100.0, s.max_dd_pct, s.trips, r.win_pct(),
            r.t.name.as_deref().unwrap_or(""), if r.t.acct.liquidated { " LIQUIDATED" } else { "" });
    }
    println!("  (lag50 median lag s; slipbp our price vs its, + = worse; bet% / bet^% average / biggest bet, % of equity; \
        risk% / risk^% average / deepest trip; stop% trips with a stop; plan% / plan^% planned risk average / biggest; \
        pos^ most positions at once; lev^ highest leverage; dd% max drawdown)");

    std::fs::create_dir_all(&cfg.data_dir)?;
    let path = cfg.data_dir.join("report.csv");
    let mut f = std::fs::File::create(&path)?;
    writeln!(f, "account,name,equity,roi_pct,fills,fills_per_day,activity_score,fees,funding,open_positions,liquidated,their_equity,\
        lag_p50_s,lag_p90_s,lag_max_s,feed_avg_s,slip_bps,slip_usd,move_usd,impact_usd,worse_pct,better_pct,\
        bets,bet_avg_pct,bet_max_pct,trips,win_pct,win_usd,loss_usd,avg_hold_min,risk_avg_pct,risk_max_pct,size_avg_pct,size_max_pct,\
        max_positions,max_leverage,max_drawdown_pct,        planned_trips,stop_pct,isolated_pct,lev_setting_avg,lev_setting_max,plan_risk_avg_pct,plan_risk_max_pct")?;
    let mut all: Vec<&Row> = rows.iter().collect();
    all.sort_by(|a, b| b.score.total_cmp(&a.score));
    for r in all {
        let (t, s) = (r.t, r.s());
        let c = s.copies as f64;
        let trips = s.trips as f64;
        writeln!(f, "{},{},{:.4},{:.4},{},{:.2},{:.3},{:.4},{:.4},{},{},{:.2},\
            {:.1},{:.1},{:.3},{:.3},{:.3},{:.4},{:.4},{:.4},{:.1},{:.1},\
            {},{:.3},{:.3},{},{:.1},{:.4},{:.4},{:.1},{:.3},{:.3},{:.3},{:.3},\
            {},{:.3},{:.3},            {},{:.1},{:.1},{:.2},{:.0},{:.3},{:.3}",
            r.addr, t.name.as_deref().unwrap_or("").replace(',', " "), r.equity, r.roi, t.copy_fills, r.per_day, r.score,
            t.acct.fees, t.acct.funding, t.acct.positions.len(), t.acct.liquidated, t.equity,
            s.lag_q(0.5), s.lag_q(0.9), s.lag_max, div(s.feed_sum, c), r.slip_bps(), s.slip_usd, s.move_usd, s.impact_usd,
            div(s.worse as f64, c) * 100.0, div(s.better as f64, c) * 100.0,
            s.entries, r.avg_entry_pct(), s.entry_pct_max, s.trips, r.win_pct(), s.win_usd, s.loss_usd, div(s.hold_s, trips) / 60.0,
            r.avg_risk_pct(), s.risk_pct_max, div(s.size_pct_sum, trips), s.size_pct_max,
            s.max_open, s.max_gross_pct / 100.0, s.max_dd_pct,
            s.planned, r.stop_pct(), r.isolated_pct(), r.avg_lev(), s.lev_max, r.avg_plan_pct(), s.plan_pct_max)?;
    }
    println!("all accounts: {}", path.display());
    Ok(())
}
