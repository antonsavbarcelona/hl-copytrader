//! Paper copy-trading of Hyperliquid accounts that make money steadily (picked daily, see
//! `stable`), $1000 copy account each, mirrored 1:1 against the live books.
//!
//!     hl-copytrader run    [--data DIR] [--start 1000] [--delay-ms 1000] [--weight 400]
//!     hl-copytrader report [--data DIR] [--min-fills 10] [--top 30]

mod account;
mod api;
mod config;
mod engine;
mod report;
mod stable;
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
    let selection = store::load_selection(&cfg).await?;
    let store = store::open(&cfg).await?;
    let engine = engine::Engine::new(cfg.clone(), api.clone(), books.clone(), coins, tx.clone(), traders, store)?;
    let followed = engine.followed.clone();

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
    // Leaderboard now and every 6 h; from it, once a day, the traders to follow (the saved
    // list until it is a day old).
    {
        let (api, tx, cfg) = (api.clone(), tx.clone(), cfg.clone());
        let mut last = 0.0;
        if let Some(s) = selection {
            log!("selection: saved one of {:.1} h ago, {} traders", (api::now() - s.at) / 3600.0, s.picks.len());
            last = s.at;
            let _ = tx.send(engine::Msg::Selected(s));
        }
        tokio::spawn(async move {
            loop {
                let wait = match api.leaders().await {
                    Ok(l) => {
                        let _ = tx.send(engine::Msg::Leaders(l.clone()));
                        if api::now() - last >= stable::EVERY_S {
                            let pause = if last > 0.0 { stable::PAUSE_S } else { 0.0 };
                            let s = stable::select(&api, &l, pause).await;
                            last = s.at;
                            if let Err(e) = store::save_selection(&cfg, &s).await {
                                log!("selection: not saved: {e:#}");
                            }
                            let _ = tx.send(engine::Msg::Selected(s));
                        }
                        (6.0 * 3600.0f64).min(last + stable::EVERY_S - api::now()).max(60.0)
                    }
                    Err(e) => {
                        log!("leaderboard read failed: {e}");
                        300.0
                    }
                };
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
            }
        });
    }
    // Fills go through their own channel into the engine's.
    let (ftx, mut frx) = mpsc::unbounded_channel();
    tokio::spawn(ws::run_books(names.clone(), books.clone()));
    tokio::spawn(ws::run_trades(names.clone(), followed, ftx));
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(f) = frx.recv().await {
                if tx.send(engine::Msg::Fill(f)).is_err() {
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
