//! The trading bot on Hyperliquid: the family of strategies on one account (the dip buys, the HIP-3 dips and
//! weekend dips, the wick bids, the RSI dip, the listings, the trend, the monthly expiry short, the squeezes, the
//! breakouts), sized from one deposit with one open-risk cap (see `trades`), and the order-book recorders.
//!
//!     hl-copytrader run [--data DIR] [--weight 400]

mod api;
mod breakout;
mod clusters;
mod config;
mod dip;
mod exchange;
mod expiry;
mod hip3;
mod keeper;
mod listing;
mod venue;
mod notify;
mod rsidip;
mod squeeze;
mod store;
mod trades;
mod trend;
mod walls;
mod micro;
mod wick;
mod wickwatch;
mod wkd;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod e2e_tests;
mod ws;

use std::time::Duration;

#[macro_export]
macro_rules! log {
    ($($a:tt)*) => {{
        let t = $crate::api::now() as u64 % 86400;
        println!("{:02}:{:02}:{:02} {}", t / 3600, t / 60 % 60, t % 60, format!($($a)*));
    }};
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // DATABASE_URL, RUN_ID, ... from a .env file when there is one (the environment wins).
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().cloned().unwrap_or_else(|| "run".into());
    let (cfg, rest) = config::Config::from_args(if args.is_empty() { &args[..] } else { &args[1..] })?;
    if !rest.is_empty() {
        log!("config: options not used, left out: {rest:?}");
    }
    match cmd.as_str() {
        "run" => run(cfg).await,
        other => anyhow::bail!("unknown command {other} (run)"),
    }
}

async fn run(cfg: config::Config) -> anyhow::Result<()> {
    // The start, to the chat (when it is set): the deploy's commit on Railway.
    let commit = std::env::var("RAILWAY_GIT_COMMIT_SHA").ok().map(|c| c.chars().take(7).collect::<String>());
    notify::send(format!("🚀 бот запущен{}", commit.map(|c| format!(", версия {c}")).unwrap_or_default()));
    let api = api::Api::new(cfg.weight_per_min)?;
    let (coins, _) = api.meta().await?;
    log!("{} perp coins", coins.len());
    let names: Vec<String> = coins.iter().map(|c| c.name.clone()).collect();
    let books: ws::Books = Default::default();
    spawn_clock_sync(api.clone());
    let store = store::open(&cfg).await?;
    // the start in the events too: a deploy is checked by its version there
    store.event(serde_json::json!({"kind": "bot", "what": "start", "version": commit_full(), "at": api::now()}));
    clusters::start(&cfg, api.clone()).await?;
    dip::start(&cfg, store.clone(), api.clone(), books.clone()).await?;
    hip3::start(&cfg, store.clone(), api.clone()).await?;
    wick::start(&cfg, store.clone(), api.clone()).await?;
    rsidip::start(&cfg, store.clone(), api.clone()).await?;
    listing::start(&cfg, store.clone()).await?;
    trend::start(&cfg, store.clone()).await?;
    expiry::start(&cfg, store.clone()).await?;
    match walls::start(store.clone(), api.clone(), books.clone()).await {
        Ok((coins, agg)) => if let Err(e) = micro::start(store.clone(), api.clone(), coins, agg).await {
            log!("micro: not recording: {e:#}");
        },
        Err(e) => log!("walls: not recording: {e:#}"),
    }
    squeeze::start(&cfg, store.clone()).await?;
    breakout::start(&cfg, store).await?;
    tokio::spawn(ws::run_books(names, books));
    shutdown_signal().await;
    log!("stopped");
    Ok(())
}

/// The deploy's commit (Railway sets it), "" elsewhere.
fn commit_full() -> String {
    std::env::var("RAILWAY_GIT_COMMIT_SHA").unwrap_or_default()
}

/// Our clock against the exchange's, now and every 10 min (lags are measured on its clock).
fn spawn_clock_sync(api: api::Api) {
    tokio::spawn(async move {
        loop {
            match api.sync_clock().await {
                Ok((offset, rtt)) => log!("clock: exchange {offset:+.3} s vs ours, API round trip {:.0} ms", rtt * 1000.0),
                Err(e) => log!("clock sync failed: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
}

/// Ctrl-C, or SIGTERM (docker stop, systemd, a Railway redeploy): the process exits cleanly.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
