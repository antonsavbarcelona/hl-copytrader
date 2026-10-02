//! Where a run lives: Postgres when `DATABASE_URL` is set (tables created on first start),
//! else files in the data directory (`state.json`, `events.jsonl`).
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

/// One copy account as saved: its full state plus a few columns to query by.
pub struct Row {
    pub address: String,
    pub name: Option<String>,
    pub equity: f64,
    pub roi_pct: f64,
    pub copy_fills: i64,
    pub open_positions: i32,
    pub liquidated: bool,
    /// The `Trader`, serialized.
    pub state: String,
}

enum Cmd {
    Event(Value),
    Save(Vec<Row>),
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

    /// Waits until everything sent so far is written.
    pub async fn flush(&self) {
        let (tx, rx) = oneshot::channel();
        if self.tx.send(Cmd::Flush(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

/// The saved copy accounts of this run.
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
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Event(ev) => {
                let _ = writeln!(events, "{ev}");
            }
            Cmd::Save(rows) => {
                for r in rows {
                    all.insert(r.address, r.state);
                }
                let body: Vec<String> = all.iter().map(|(a, s)| format!("{}:{s}", Value::String(a.clone()))).collect();
                let tmp = state_path.with_extension("tmp");
                if std::fs::write(&tmp, format!("{{\"traders\":{{{}}}}}", body.join(","))).is_ok() {
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
";

async fn migrate(client: &tokio_postgres::Client) -> Result<()> {
    client.batch_execute(SCHEMA).await.context("creating tables")
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
    let mut waiting: Vec<oneshot::Sender<()>> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut open = true;
    while open || !events.is_empty() || !pending.is_empty() || !waiting.is_empty() {
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
        if !due || (events.is_empty() && pending.is_empty() && waiting.is_empty()) {
            continue;
        }
        if client.as_ref().is_none_or(|c| c.is_closed()) {
            client = match pg_connect(&url).await {
                Ok(c) => {
                    log!("store: postgres reconnected");
                    Some(c)
                }
                Err(e) => {
                    log!("store: postgres unavailable ({e:#}), {} events and {} accounts waiting", events.len(), pending.len());
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
        }
        let Some(c) = &client else { continue };
        match write(c, &run, &events, &pending).await {
            Ok(()) => {
                events.clear();
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

async fn write(c: &tokio_postgres::Client, run: &str, events: &[Value], accounts: &HashMap<String, Row>) -> Result<()> {
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
        let state: Vec<&str> = rows.iter().map(|r| r.state.as_str()).collect();
        c.execute(
            "INSERT INTO copy_accounts (run_id, address, name, equity, roi_pct, copy_fills, open_positions, liquidated, state, updated_at)
             SELECT $1, a.address, a.name, a.equity, a.roi, a.fills, a.open, a.liq, a.state::jsonb, now()
             FROM unnest($2::text[], $3::text[], $4::float8[], $5::float8[], $6::int8[], $7::int4[], $8::bool[], $9::text[])
                  AS a(address, name, equity, roi, fills, open, liq, state)
             ON CONFLICT (run_id, address) DO UPDATE SET name = excluded.name, equity = excluded.equity, roi_pct = excluded.roi_pct,
                 copy_fills = excluded.copy_fills, open_positions = excluded.open_positions, liquidated = excluded.liquidated,
                 state = excluded.state, updated_at = excluded.updated_at",
            &[&run, &address, &name, &equity, &roi, &fills, &open, &liq, &state],
        )
        .await
        .context("saving copy accounts")?;
    }
    Ok(())
}
