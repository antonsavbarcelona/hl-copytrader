//! Where a run lives: Postgres when `DATABASE_URL` is set (tables created on first start),
//! else files in the data directory: the copies (`copy_accounts`; `state.json`) and what they
//! did (`events`; `events.jsonl`), the bot's health every 10 minutes (`bot_status`;
//! `status.jsonl`) and the day's list of followed traders (`selection`; `selection.json`, see
//! `stable`).
//!
//! On Postgres, `MIGRATIONS` run once each, in order, at the first start that has them
//! (`schema_migrations` keeps which ran).
//!
//! The engine never waits on it: events and state snapshots go to a writer task, which
//! batches them (one insert per second for events; for the state, only the accounts that
//! changed since the last save) and, on Postgres, reconnects and retries when the database
//! is away, keeping what it has not written yet.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::engine::Trader;
use crate::log;
use crate::stable::Selection;

/// One copy account as saved: its full state plus a few columns to query by.
pub struct Row {
    pub address: String,
    pub name: Option<String>,
    pub equity: f64,
    pub roi_pct: f64,
    pub copy_fills: i64,
    pub open_positions: i32,
    pub liquidated: bool,
    /// On the golden list, and the PnL it is judged by (since measured, see `engine`).
    pub golden: bool,
    pub measured_pnl: f64,
    /// The `Trader`, serialized.
    pub state: String,
}

enum Cmd {
    Event(Value),
    Save(Vec<Row>),
    Status(Value),
    Flush(oneshot::Sender<()>),
}

#[derive(Clone)]
pub struct Store {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Store {
    pub fn event(&self, ev: Value) {
        let _ = self.tx.send(Cmd::Event(ev));
    }

    pub fn save(&self, rows: Vec<Row>) {
        let _ = self.tx.send(Cmd::Save(rows));
    }

    pub fn status(&self, status: Value) {
        let _ = self.tx.send(Cmd::Status(status));
    }

    /// Waits until everything sent so far is written.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

/// The saved copy accounts of this run, by address.
pub async fn load(cfg: &Config) -> Result<HashMap<String, Trader>> {
    match &cfg.database_url {
        Some(url) => {
            let client = pg_connect(url).await?;
            migrate(&client).await?;
            let rows = client.query("SELECT address, state::text FROM copy_accounts WHERE run_id = $1", &[&cfg.run_id]).await?;
            let mut out = HashMap::new();
            for r in rows {
                let (address, state): (String, String) = (r.get(0), r.get(1));
                out.insert(address, serde_json::from_str(&state)?);
            }
            Ok(out)
        }
        None => {
            #[derive(serde::Deserialize, Default)]
            struct Saved {
                traders: HashMap<String, Trader>,
            }
            let path = cfg.data_dir.join("state.json");
            Ok(match std::fs::read_to_string(&path) {
                Ok(s) => serde_json::from_str::<Saved>(&s).with_context(|| format!("{}", path.display()))?.traders,
                Err(_) => HashMap::new(),
            })
        }
    }
}

/// Starts the writer for this run.
pub async fn open(cfg: &Config) -> Result<Store> {
    let (tx, rx) = mpsc::unbounded_channel();
    match &cfg.database_url {
        Some(url) => {
            // Fail now (and loudly) if the database is not reachable at start.
            let client = pg_connect(url).await?;
            migrate(&client).await?;
            log!("store: postgres, run \"{}\"", cfg.run_id);
            tokio::spawn(pg_writer(url.clone(), cfg.run_id.clone(), client, rx));
        }
        None => {
            std::fs::create_dir_all(&cfg.data_dir)?;
            log!("store: files in {}", cfg.data_dir.display());
            tokio::spawn(file_writer(cfg.data_dir.clone(), rx));
        }
    }
    Ok(Store { tx })
}

// ------------------------------------------------------------------------------------ files

async fn file_writer(dir: PathBuf, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let state_path = dir.join("state.json");
    let mut events = match std::fs::OpenOptions::new().create(true).append(true).open(dir.join("events.jsonl")) {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            log!("store: cannot open events.jsonl: {e}");
            return;
        }
    };
    // Every account as last saved (the file holds all of them).
    let mut all: HashMap<String, String> = HashMap::new();
    let obj = |m: &HashMap<String, String>| m.iter().map(|(a, s)| format!("{}:{s}", Value::String(a.clone()))).collect::<Vec<_>>().join(",");
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Event(ev) => {
                let _ = writeln!(events, "{ev}");
            }
            Cmd::Status(s) => {
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("status.jsonl")) {
                    let _ = writeln!(f, "{s}");
                }
            }
            Cmd::Save(rows) => {
                for r in rows {
                    all.insert(r.address, r.state);
                }
                let tmp = state_path.with_extension("tmp");
                if std::fs::write(&tmp, format!("{{\"traders\":{{{}}}}}", obj(&all))).is_ok() {
                    let _ = std::fs::rename(&tmp, &state_path);
                }
                let _ = events.flush();
            }
            Cmd::Flush(done) => {
                let _ = events.flush();
                let _ = done.send(());
            }
        }
    }
    let _ = events.flush();
}

// --------------------------------------------------------------------------------- postgres

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS copy_accounts (
    run_id          text        NOT NULL,
    address         text        NOT NULL,
    name            text,
    equity          float8      NOT NULL,
    roi_pct         float8      NOT NULL,
    copy_fills      bigint      NOT NULL,
    open_positions  integer     NOT NULL,
    liquidated      boolean     NOT NULL,
    golden          boolean     NOT NULL DEFAULT false,
    measured_pnl    float8      NOT NULL DEFAULT 0,
    state           jsonb       NOT NULL,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, address)
);
CREATE TABLE IF NOT EXISTS events (
    id       bigserial   PRIMARY KEY,
    run_id   text        NOT NULL,
    at       timestamptz NOT NULL,
    kind     text        NOT NULL,
    address  text,
    coin     text,
    data     jsonb       NOT NULL
);
CREATE INDEX IF NOT EXISTS events_run_at ON events (run_id, at);
CREATE INDEX IF NOT EXISTS events_run_address ON events (run_id, address, at);
CREATE TABLE IF NOT EXISTS bot_status (
    run_id          text        NOT NULL,
    at              timestamptz NOT NULL,
    copy_accounts   integer     NOT NULL,
    followed        integer     NOT NULL,
    open_positions  integer     NOT NULL,
    api_weight      integer     NOT NULL,
    api_backlog_s   float8      NOT NULL,
    tick_avg_ms     float8      NOT NULL,
    tick_max_ms     float8      NOT NULL,
    data            jsonb       NOT NULL,
    PRIMARY KEY (run_id, at)
);
CREATE TABLE IF NOT EXISTS selection (
    run_id      text        PRIMARY KEY,
    state       jsonb       NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS docs (
    run_id      text        NOT NULL,
    name        text        NOT NULL,
    state       jsonb       NOT NULL,
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, name)
);
CREATE TABLE IF NOT EXISTS schema_migrations (
    name        text        PRIMARY KEY,
    applied_at  timestamptz NOT NULL DEFAULT now()
);
";

/// One-off changes to what is in the database, by name, oldest first.
const MIGRATIONS: &[(&str, &str)] = &[
    // The traders followed until now were picked by one month's ROI (see `stable`), and the
    // signals are gone: every run starts over, the signals' tables and columns dropped.
    ("2026-10-07-reset",
     "TRUNCATE copy_accounts, events, bot_status, selection RESTART IDENTITY;
      DROP TABLE IF EXISTS signal_accounts, signal_trades, signal_maker_trades, smart_state;
      ALTER TABLE bot_status DROP COLUMN IF EXISTS signals_avg_ms, DROP COLUMN IF EXISTS signals_max_ms"),
    // The golden list: copies judged by their PnL since then.
    ("2026-10-09-golden",
     "ALTER TABLE copy_accounts ADD COLUMN IF NOT EXISTS golden boolean NOT NULL DEFAULT false,
                                ADD COLUMN IF NOT EXISTS measured_pnl float8 NOT NULL DEFAULT 0"),
    // The golden list starts with the copies at a profit on 2026-10-09 of the accounts that can
    // be copied (swing traders; not market makers, HFT or grids: see the README).
    ("2026-10-09-golden-seed",
     "UPDATE copy_accounts SET golden = true, state = jsonb_set(state, '{golden}', 'true')
      WHERE address IN (
          '0x72774e2fe1992d5da8c6e9cef73fd2ab980c0b98',
          '0x95da8596c44dd09f4b8becce87ad3b7894fb2328',
          '0x0f4fbea1aaecd66967af7ef02c7fa4693999834a',
          '0xc0b2a1ce425d0cd8bc321c445fa6ae0eaed2e688',
          '0x8fc7c0442e582bca195978c5a4fdec2e7c5bb0f7',
          '0x9c68cd0568eb47bad36ecd8090e6c1d1396a7783',
          '0x1a94785cc11b1b4225374a8e0015f788b222a538',
          '0x9a991732cd4cf14712b07fbeb8c5d87de90dd21a',
          '0x25554a80781ee62414c3747e81c3f50157c634b1',
          '0x1d74a7760df9d563d0b6610b1705266c4e2fdb26',
          '0x8aa077f5998d234ac8641d73d6bc4976e2a210fc',
          '0xe79d69fd1ed52dd14d7f55155259519ea20d0534',
          '0x9db82c502472d76742fdd69609dfcc6e01327401')"),
];

async fn migrate(client: &tokio_postgres::Client) -> Result<()> {
    client.batch_execute(SCHEMA).await.context("creating tables")?;
    for (name, sql) in MIGRATIONS {
        // One at a time across processes; each once.
        client.batch_execute("BEGIN; SELECT pg_advisory_xact_lock(7262051)").await?;
        let done = client.query_opt("SELECT 1 FROM schema_migrations WHERE name = $1", &[name]).await;
        let run = async {
            if done?.is_none() {
                client.batch_execute(sql).await?;
                client.execute("INSERT INTO schema_migrations (name) VALUES ($1)", &[name]).await?;
                log!("store: migration {name} applied");
            }
            anyhow::Ok(())
        }.await;
        match run {
            Ok(()) => client.batch_execute("COMMIT").await?,
            Err(e) => {
                let _ = client.batch_execute("ROLLBACK").await;
                return Err(e).with_context(|| format!("migration {name}"));
            }
        }
    }
    Ok(())
}

/// The day's list of followed traders saved by this run, if any.
pub async fn load_selection(cfg: &Config) -> Result<Option<Selection>> {
    let s = match &cfg.database_url {
        Some(url) => {
            let client = pg_connect(url).await?;
            migrate(&client).await?;
            client.query_opt("SELECT state::text FROM selection WHERE run_id = $1", &[&cfg.run_id]).await?.map(|r| r.get::<_, String>(0))
        }
        None => std::fs::read_to_string(cfg.data_dir.join("selection.json")).ok(),
    };
    Ok(s.and_then(|s| serde_json::from_str(&s).ok()))
}

/// Saves the day's list (once a day: written at once, not through the writer).
pub async fn save_selection(cfg: &Config, sel: &Selection) -> Result<()> {
    let s = serde_json::to_string(sel)?;
    match &cfg.database_url {
        Some(url) => {
            let client = pg_connect(url).await?;
            client.execute(
                "INSERT INTO selection (run_id, state, updated_at) VALUES ($1, $2::text::jsonb, now())
                 ON CONFLICT (run_id) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
                &[&cfg.run_id, &s],
            ).await?;
        }
        None => {
            std::fs::create_dir_all(&cfg.data_dir)?;
            let (path, tmp) = (cfg.data_dir.join("selection.json"), cfg.data_dir.join("selection.tmp"));
            std::fs::write(&tmp, s)?;
            std::fs::rename(&tmp, &path)?;
        }
    }
    Ok(())
}

/// A named state document of this run (e.g. "live"), if saved.
pub async fn load_doc(cfg: &Config, name: &str) -> Result<Option<Value>> {
    let s = match &cfg.database_url {
        Some(url) => {
            let client = pg_connect(url).await?;
            migrate(&client).await?;
            client.query_opt("SELECT state::text FROM docs WHERE run_id = $1 AND name = $2", &[&cfg.run_id, &name]).await?.map(|r| r.get::<_, String>(0))
        }
        None => std::fs::read_to_string(cfg.data_dir.join(format!("{name}.json"))).ok(),
    };
    Ok(s.and_then(|s| serde_json::from_str(&s).ok()))
}

/// Saves a named state document (written at once; for rare changes).
pub async fn save_doc(cfg: &Config, name: &str, state: &Value) -> Result<()> {
    let s = state.to_string();
    match &cfg.database_url {
        Some(url) => {
            let client = pg_connect(url).await?;
            client.execute(
                "INSERT INTO docs (run_id, name, state, updated_at) VALUES ($1, $2, $3::text::jsonb, now())
                 ON CONFLICT (run_id, name) DO UPDATE SET state = excluded.state, updated_at = excluded.updated_at",
                &[&cfg.run_id, &name, &s],
            ).await?;
        }
        None => {
            std::fs::create_dir_all(&cfg.data_dir)?;
            let (path, tmp) = (cfg.data_dir.join(format!("{name}.json")), cfg.data_dir.join(format!("{name}.tmp")));
            std::fs::write(&tmp, s)?;
            std::fs::rename(&tmp, &path)?;
        }
    }
    Ok(())
}

/// TLS when the URL asks for it (`sslmode=require` / `verify-ca` / `verify-full`, as hosted
/// databases do), plain otherwise (e.g. Railway's private network).
async fn pg_connect(url: &str) -> Result<tokio_postgres::Client> {
    let wants_tls = ["sslmode=require", "sslmode=verify-ca", "sslmode=verify-full"].iter().any(|m| url.contains(m));
    let client = if wants_tls {
        let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let (client, conn) = tokio_postgres::connect(url, tokio_postgres_rustls::MakeRustlsConnect::new(tls)).await.context("connecting to postgres")?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                log!("store: postgres connection: {e}");
            }
        });
        client
    } else {
        let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls).await.context("connecting to postgres")?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                log!("store: postgres connection: {e}");
            }
        });
        client
    };
    Ok(client)
}

fn hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

async fn pg_writer(url: String, run: String, client: tokio_postgres::Client, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    let mut client = Some(client);
    let mut events: Vec<Value> = Vec::new();
    // Accounts waiting to be written (latest snapshot of each), and what was last written.
    let mut pending: HashMap<String, Row> = HashMap::new();
    let mut written: HashMap<String, u64> = HashMap::new();
    let mut statuses: Vec<Value> = Vec::new();
    let mut waiting: Vec<oneshot::Sender<()>> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut open = true;
    while open || !events.is_empty() || !pending.is_empty() || !statuses.is_empty() || !waiting.is_empty() {
        // Gather until the next tick (or a flush, a big batch, or the end), then write.
        let due = tokio::select! {
            cmd = rx.recv(), if open => match cmd {
                Some(Cmd::Event(ev)) => {
                    events.push(ev);
                    events.len() >= 1000
                }
                Some(Cmd::Save(rows)) => {
                    for r in rows {
                        if written.get(&r.address) != Some(&hash(&r.state)) {
                            pending.insert(r.address.clone(), r);
                        }
                    }
                    false
                }
                Some(Cmd::Status(s)) => {
                    statuses.push(s);
                    false
                }
                Some(Cmd::Flush(done)) => {
                    waiting.push(done);
                    true
                }
                None => {
                    open = false;
                    true
                }
            },
            _ = tick.tick() => true,
        };
        if !due || (events.is_empty() && pending.is_empty() && statuses.is_empty() && waiting.is_empty()) {
            continue;
        }
        if client.as_ref().is_none_or(|c| c.is_closed()) {
            client = match pg_connect(&url).await {
                Ok(c) => {
                    log!("store: postgres reconnected");
                    Some(c)
                }
                Err(e) => {
                    log!("store: postgres unavailable ({e:#}), {} events, {} accounts waiting", events.len(), pending.len());
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
        }
        let Some(c) = &client else { continue };
        match write(c, &run, &events, &pending, &statuses).await {
            Ok(()) => {
                events.clear();
                statuses.clear();
                for (a, r) in pending.drain() {
                    written.insert(a, hash(&r.state));
                }
                for done in waiting.drain(..) {
                    let _ = done.send(());
                }
            }
            Err(e) => {
                log!("store: postgres write failed ({e:#}), retrying");
                client = None;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn write(c: &tokio_postgres::Client, run: &str, events: &[Value], accounts: &HashMap<String, Row>, statuses: &[Value]) -> Result<()> {
    if !statuses.is_empty() {
        let rows = serde_json::to_string(statuses)?;
        c.execute(
            "INSERT INTO bot_status (run_id, at, copy_accounts, followed, open_positions, api_weight, api_backlog_s, tick_avg_ms,
                 tick_max_ms, data)
             SELECT $1, to_timestamp(r.at), r.copy_accounts, r.followed, r.open_positions, r.api_weight, r.api_backlog_s, r.tick_avg_ms,
                 r.tick_max_ms, d
             FROM jsonb_array_elements($2::text::jsonb) AS d,
                  jsonb_to_record(d) AS r(at float8, copy_accounts int4, followed int4, open_positions int4, api_weight int4,
                      api_backlog_s float8, tick_avg_ms float8, tick_max_ms float8)
             ON CONFLICT DO NOTHING",
            &[&run, &rows],
        )
        .await
        .context("saving status")?;
    }
    if !events.is_empty() {
        let at: Vec<f64> = events.iter().map(|e| e["at"].as_f64().unwrap_or(0.0)).collect();
        let kind: Vec<String> = events.iter().map(|e| e["kind"].as_str().unwrap_or("").to_string()).collect();
        let address: Vec<Option<String>> = events.iter().map(|e| e["user"].as_str().map(str::to_string)).collect();
        let coin: Vec<Option<String>> = events.iter().map(|e| e["coin"].as_str().map(str::to_string)).collect();
        let data: Vec<String> = events.iter().map(|e| e.to_string()).collect();
        c.execute(
            "INSERT INTO events (run_id, at, kind, address, coin, data)
             SELECT $1, to_timestamp(e.at), e.kind, e.address, e.coin, e.data::jsonb
             FROM unnest($2::float8[], $3::text[], $4::text[], $5::text[], $6::text[]) AS e(at, kind, address, coin, data)",
            &[&run, &at, &kind, &address, &coin, &data],
        )
        .await
        .context("inserting events")?;
    }
    if !accounts.is_empty() {
        let rows: Vec<&Row> = accounts.values().collect();
        let address: Vec<&str> = rows.iter().map(|r| r.address.as_str()).collect();
        let name: Vec<Option<&str>> = rows.iter().map(|r| r.name.as_deref()).collect();
        let equity: Vec<f64> = rows.iter().map(|r| r.equity).collect();
        let roi: Vec<f64> = rows.iter().map(|r| r.roi_pct).collect();
        let fills: Vec<i64> = rows.iter().map(|r| r.copy_fills).collect();
        let open: Vec<i32> = rows.iter().map(|r| r.open_positions).collect();
        let liq: Vec<bool> = rows.iter().map(|r| r.liquidated).collect();
        let golden: Vec<bool> = rows.iter().map(|r| r.golden).collect();
        let measured: Vec<f64> = rows.iter().map(|r| r.measured_pnl).collect();
        let state: Vec<&str> = rows.iter().map(|r| r.state.as_str()).collect();
        c.execute(
            "INSERT INTO copy_accounts (run_id, address, name, equity, roi_pct, copy_fills, open_positions, liquidated, golden, measured_pnl,
                                        state, updated_at)
             SELECT $1, a.address, a.name, a.equity, a.roi, a.fills, a.open, a.liq, a.golden, a.measured, a.state::jsonb, now()
             FROM unnest($2::text[], $3::text[], $4::float8[], $5::float8[], $6::int8[], $7::int4[], $8::bool[], $9::bool[], $10::float8[],
                         $11::text[])
                  AS a(address, name, equity, roi, fills, open, liq, golden, measured, state)
             ON CONFLICT (run_id, address) DO UPDATE SET name = excluded.name, equity = excluded.equity, roi_pct = excluded.roi_pct,
                 copy_fills = excluded.copy_fills, open_positions = excluded.open_positions, liquidated = excluded.liquidated,
                 golden = excluded.golden, measured_pnl = excluded.measured_pnl, state = excluded.state, updated_at = excluded.updated_at",
            &[&run, &address, &name, &equity, &roi, &fills, &open, &liq, &golden, &measured, &state],
        )
        .await
        .context("saving copy accounts")?;
    }
    Ok(())
}
