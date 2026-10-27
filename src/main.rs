//! Paper copy-trading of Hyperliquid accounts that make money steadily (picked daily, see
//! `stable`), $1000 copy account each, following their trades against the live books with a
//! 2% risk per entry to a 20% stop (see `engine`). Only with `PAPER_ENABLED=1` (off since
//! 2026-10-22); without it `run` is the dip-buy alone (see `dip`).
//!
//!     hl-copytrader run    [--data DIR] [--start 1000] [--risk 2] [--stop 20] [--delay-ms 0] [--weight 400]
//!     hl-copytrader report [--data DIR] [--min-fills 10] [--top 30]

mod account;
mod alloc;
mod api;
mod config;
mod dip;
mod engine;
mod exchange;
mod hip3;
mod liq;
mod listing;
mod live;
mod notify;
mod report;
mod rollstats;
mod rsidip;
mod stable;
mod stats;
mod store;
mod trades;
mod wick;
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
        "live-check" => live::check(&cfg, rest.first().map(String::as_str).unwrap_or("BTC")).await,
        other => anyhow::bail!("unknown command {other} (run | report | live-check)"),
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
    // The paper copy (the traders' copy accounts, the allocator's portfolios, and the live copy that
    // rides on them) only with PAPER_ENABLED=1: switched off 2026-10-22, the dip-buy trades alone.
    if std::env::var("PAPER_ENABLED").unwrap_or_default().trim() != "1" {
        log!("paper: the paper copy is off (PAPER_ENABLED=1 to run it); the dip-buy only");
        let store = store::open(&cfg).await?;
        dip::start(&cfg, store.clone(), api.clone(), books.clone()).await?;
        hip3::start(&cfg, store.clone(), api.clone()).await?;
        wick::start(&cfg, store.clone(), api.clone()).await?;
        rsidip::start(&cfg, store.clone(), api.clone()).await?;
        listing::start(&cfg, store).await?;
        tokio::spawn(ws::run_books(names, books));
        shutdown_signal().await;
        log!("stopped");
        return Ok(());
    }
    let (tx, rx) = mpsc::unbounded_channel();
    let traders = store::load(&cfg).await?;
    let selection = store::load_selection(&cfg).await?;
    let picks = store::load_picks(&cfg).await?;
    let store = store::open(&cfg).await?;
    let live = live::start(&cfg, store.clone(), false, Some(books.clone())).await?;
    // The dip-buy on the same account (off unless DIP_ENABLED=1): its own positions, on the books.
    dip::start(&cfg, store.clone(), api.clone(), books.clone()).await?;
    hip3::start(&cfg, store.clone(), api.clone()).await?;
    wick::start(&cfg, store.clone(), api.clone()).await?;
    rsidip::start(&cfg, store.clone(), api.clone()).await?;
    listing::start(&cfg, store.clone()).await?;
    let store_liq = store.clone();
    let mut engine = engine::Engine::new(cfg.clone(), api.clone(), books.clone(), coins, tx.clone(), traders, store)?;
    engine.set_picks(picks);
    let saved = store::load_doc(&cfg, "allocator").await?.and_then(|v| serde_json::from_value::<alloc::Allocators>(v).ok());
    engine.set_alloc(alloc::Allocators::new(saved), store::load_backtest_stats(&cfg).await?);
    // After the allocator: the live account may enter one of its portfolios' picks.
    if let Some(l) = live {
        engine.set_live(l);
    }
    let followed = engine.followed.clone();
    rollstats::spawn(cfg.clone(), tx.clone());

    // Leaderboard now and every 6 h: round 1 from it (the saved list until then), and the
    // larger accounts for the liquidation research (off unless LIQ_ENABLED=1: switched off
    // 2026-10-14, all four strategies at a loss on its ~54 touches).
    {
        let (api, tx, cfg) = (api.clone(), tx.clone(), cfg.clone());
        if let Some(s) = selection {
            log!("selection: saved one of {:.1} h ago, {} traders", (api::now() - s.at) / 3600.0, s.picks.len());
            let _ = tx.send(engine::Msg::Selected(s));
        }
        let (acc_tx, acc_rx) = tokio::sync::watch::channel(Vec::new());
        if std::env::var("LIQ_ENABLED").unwrap_or_default().trim() == "1" {
            liq::spawn(api.clone(), books.clone(), acc_rx, store_liq, cfg.clone());
        } else {
            log!("liq: the liquidation research is off (LIQ_ENABLED=1 to run it)");
        }
        tokio::spawn(async move {
            loop {
                let wait = match api.leaders().await {
                    Ok(l) => {
                        let _ = tx.send(engine::Msg::Leaders(l.clone()));
                        let mut big: Vec<&api::Leader> = l.iter().filter(|x| x.account_value >= liq::MIN_ACCOUNT).collect();
                        big.sort_by(|a, b| b.account_value.total_cmp(&a.account_value));
                        let _ = acc_tx.send(big.into_iter().map(|x| x.address.clone()).collect());
                        let s = stable::round1(&l);
                        log!("selection: round 1, {} of {} leaderboard accounts", s.picks.len(), s.read);
                        if let Err(e) = store::save_selection(&cfg, &s).await {
                            log!("selection: not saved: {e:#}");
                        }
                        let _ = tx.send(engine::Msg::Selected(s));
                        6.0 * 3600.0
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
            shutdown_signal().await;
            let _ = tx.send(engine::Msg::Shutdown);
        });
    }
    engine.run(rx).await;
    Ok(())
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
