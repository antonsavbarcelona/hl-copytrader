//! Run settings.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub data_dir: PathBuf,
    /// Each followed account gets its own copy account starting with this much.
    pub start_usd: f64,
    /// Followed: leaderboard accounts that traded this month with at least this equity
    /// (a $1000 copy of a smaller account would multiply its positions).
    pub min_equity: f64,
    /// ... and at least this leaderboard PnL this month (USD) ...
    pub min_month_pnl: f64,
    /// ... and at least this leaderboard ROI this month (fraction, 0.5 = +50%).
    pub min_month_roi: f64,
    /// Our order lands this long after the account's fill reaches us.
    pub exec_delay_ms: u64,
    /// Hyperliquid taker fee (base tier).
    pub taker_fee: f64,
    /// Exchange minimum order value (closing a position is always allowed).
    pub min_order_usd: f64,
    /// Position / equity above this is a stale equity read: read the account again.
    pub stale_leverage: f64,
    /// An active account's positions are read again this often (drift correction).
    pub reconcile_s: f64,
    /// Info API weight per minute this process may use (1200 per IP in total).
    pub weight_per_min: f64,
    /// Postgres to keep the run in (env `DATABASE_URL`); files in `data_dir` without it.
    pub database_url: Option<String>,
    /// Name of this run in the database (env `RUN_ID`, `--run`): runs share its tables.
    pub run_id: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("data"),
            start_usd: 1000.0,
            min_equity: 1000.0,
            min_month_pnl: 10_000.0,
            min_month_roi: 0.5,
            exec_delay_ms: 1000,
            taker_fee: 0.00045,
            min_order_usd: 10.0,
            stale_leverage: 60.0,
            reconcile_s: 2.0 * 3600.0,
            weight_per_min: 400.0,
            database_url: std::env::var("DATABASE_URL").ok().filter(|s| !s.trim().is_empty()),
            run_id: std::env::var("RUN_ID").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "main".into()),
        }
    }
}

impl Config {
    pub fn from_args(args: &[String]) -> anyhow::Result<(Self, Vec<String>)> {
        let mut c = Self::default();
        let mut rest = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let v = || args.get(i + 1).cloned().ok_or_else(|| anyhow::anyhow!("{} needs a value", args[i]));
            match args[i].as_str() {
                "--data" => c.data_dir = PathBuf::from(v()?),
                "--start" => c.start_usd = v()?.parse()?,
                "--min-equity" => c.min_equity = v()?.parse()?,
                "--min-pnl" => c.min_month_pnl = v()?.parse()?,
                "--min-roi" => c.min_month_roi = v()?.parse::<f64>()? / 100.0,
                "--delay-ms" => c.exec_delay_ms = v()?.parse()?,
                "--weight" => c.weight_per_min = v()?.parse()?,
                "--run" => c.run_id = v()?,
                _ => {
                    rest.push(args[i].clone());
                    if let Some(x) = args.get(i + 1) {
                        rest.push(x.clone());
                    }
                }
            }
            i += 2;
        }
        Ok((c, rest))
    }
}
