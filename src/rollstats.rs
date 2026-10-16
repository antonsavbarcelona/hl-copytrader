//! The rolling backtest (`rollstats/daily.py`, Python: the research's model as it was), run by
//! the bot once a day: the allocator's statistics on the 90 days to today 00:00 UTC, so its
//! ranking moves a day every day and the portfolios rotate as they did walked forward. At a
//! start (`START_DELAY_S` on, when today's are not in yet) and each day `RUN_AT_S` past midnight UTC; a run cut
//! short (a redeploy) resumes, its fills saved as it goes. When today's are in, they go to the
//! engine (`Msg::Seed`), which ranks again at once. Off without a database, without the
//! script (a local build), or with ROLLSTATS=0.

use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::engine::Msg;
use crate::log;

const SCRIPT: &str = "/app/rollstats/daily.py";
/// Seconds past midnight UTC it runs at.
const RUN_AT_S: f64 = 300.0;
/// After a run that did not bring today's in, again in this long.
const RETRY_S: f64 = 3600.0;
/// After a start it waits this long: the bot's own start reads thousands of accounts, and the
/// two on one IP's API limit kept it from coming up (2026-10-16).
const START_DELAY_S: u64 = 1800;

pub fn spawn(cfg: Config, tx: mpsc::UnboundedSender<Msg>) {
    let Some(url) = cfg.database_url.clone() else { return };
    if std::env::var("ROLLSTATS").is_ok_and(|v| v.trim() == "0") || !std::path::Path::new(SCRIPT).exists() {
        log!("rollstats: off ({SCRIPT} not here, or ROLLSTATS=0)");
        return;
    }
    tokio::spawn(async move {
        // Let the bot's start settle first.
        tokio::time::sleep(Duration::from_secs(START_DELAY_S)).await;
        loop {
            let today = (crate::api::now() / 86_400.0).floor() * 86_400.0;
            let have = |d: Option<f64>| d.is_some_and(|d| d >= today);
            let wait = if have(crate::store::stats_day(&cfg).await.ok().flatten()) {
                today + 86_400.0 + RUN_AT_S - crate::api::now()
            } else {
                run(&url).await;
                if have(crate::store::stats_day(&cfg).await.ok().flatten()) {
                    match crate::store::load_backtest_stats(&cfg).await {
                        Ok(s) => {
                            let _ = tx.send(Msg::Seed(s));
                        }
                        Err(e) => log!("rollstats: statistics not loaded: {e:#}"),
                    }
                    today + 86_400.0 + RUN_AT_S - crate::api::now()
                } else {
                    RETRY_S
                }
            };
            tokio::time::sleep(Duration::from_secs_f64(wait.max(60.0))).await;
        }
    });
}

async fn run(url: &str) {
    log!("rollstats: started");
    let child = tokio::process::Command::new("python3")
        .arg(SCRIPT)
        .env("DATABASE_URL", url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            log!("rollstats: not started: {e}");
            return;
        }
    };
    let (out, err) = (child.stdout.take(), child.stderr.take());
    let (_, _, status) = tokio::join!(echo(out), echo(err), child.wait());
    log!("rollstats: ended, {}", status.map(|s| s.to_string()).unwrap_or_else(|e| e.to_string()));
}

/// Its output into the bot's log, line by line.
async fn echo<R: tokio::io::AsyncRead + Unpin>(r: Option<R>) {
    if let Some(r) = r {
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            println!("{l}");
        }
    }
}
