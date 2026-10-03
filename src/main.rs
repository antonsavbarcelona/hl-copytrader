//! Paper copy-trading of the top Hyperliquid leaderboard accounts, $1000 copy account each,
//! mirrored 1:1 against the live books.
//!
//!     hl-copytrader run    [--data DIR] [--start 1000] [--min-equity 1000] [--min-pnl 10000] [--min-roi 50]
//!                          [--delay-ms 1000] [--weight 400]
//!     hl-copytrader report [--data DIR] [--min-fills 10] [--top 30]

mod account;
mod api;
mod config;
mod engine;
mod maker;
mod report;
mod signals;
mod stats;
mod store;
mod ws;

use std::time::Duration;

use tokio::sync::mpsc;

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
    match cmd.as_str() {
        "run" => run(cfg).await,
        "report" => report::run(&cfg, &rest).await,
        other => anyhow::bail!("unknown command {other} (run | report)"),
    }
}

async fn run(cfg: config::Config) -> anyhow::Result<()> {
    let api = api::Api::new(cfg.weight_per_min)?;
    let (coins, _) = api.meta().await?;
    log!("{} perp coins", coins.len());
    let names: Vec<String> = coins.iter().map(|c| c.name.clone()).collect();
    let books: ws::Books = Default::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let traders = store::load(&cfg).await?;
    let store = store::open(&cfg).await?;
    let engine = engine::Engine::new(cfg.clone(), api.clone(), books.clone(), coins, tx.clone(), traders, store)?;
    let (followed, watched) = (engine.followed.clone(), engine.watched.clone());

    // Our clock against the exchange's, now and every 10 min (lags are measured on its clock).
    {
        let api = api.clone();
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
    // Leaderboard now and every 6 h.
    {
        let (api, tx) = (api.clone(), tx.clone());
        tokio::spawn(async move {
            loop {
                let wait = match api.leaders().await {
                    Ok(l) => {
                        let _ = tx.send(engine::Msg::Leaders(l));
                        6 * 3600
                    }
                    Err(e) => {
                        log!("leaderboard read failed: {e}");
                        300
                    }
                };
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
        });
    }
    // Fills, trades and book changes go through their own channel into the engine's.
    let (ftx, mut frx) = mpsc::unbounded_channel();
    tokio::spawn(ws::run_books(names.clone(), books.clone(), watched.clone(), ftx.clone()));
    tokio::spawn(ws::run_trades(names, followed, watched, ftx));
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(f) = frx.recv().await {
                let msg = match f {
                    ws::Feed::Fill(f) => engine::Msg::Fill(f),
                    ws::Feed::Print { coin, px, sz, taker_buy, time_ms } => engine::Msg::Print { coin, px, sz, taker_buy, time_ms },
                    ws::Feed::Book(coin) => engine::Msg::Book(coin),
                };
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
    }
    // Funding at every hour (rates read just after it), liquidation/reconcile ticks.
    {
        let (api, tx) = (api.clone(), tx.clone());
        tokio::spawn(async move {
            loop {
                let t = api::now();
                let next = (t / 3600.0).floor() * 3600.0 + 3600.0 + 5.0;
                tokio::time::sleep(Duration::from_secs_f64(next - t)).await;
                match api.meta().await {
                    Ok((_, ctx)) => {
                        let _ = tx.send(engine::Msg::Funding(ctx));
                    }
                    Err(e) => log!("funding read failed: {e}"),
                }
            }
        });
    }
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if tx.send(engine::Msg::Tick).is_err() {
                    break;
                }
            }
        });
    }
    // Ctrl-C / SIGTERM (docker stop, systemd): save the state before exiting.
    {
        let tx = tx.clone();
        tokio::spawn(async move {
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
            let _ = tx.send(engine::Msg::Shutdown);
        });
    }
    engine.run(rx).await;
    Ok(())
}
