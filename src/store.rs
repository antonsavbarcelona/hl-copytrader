//! Where a run lives: Postgres when `DATABASE_URL` is set (tables created on first start), else files in the data
//! directory: what the strategies did (`events`; `events.jsonl`) and their states (`docs`; `<name>.json`).
//!
//! On Postgres, `MIGRATIONS` run once each, in order, at the first start that has them (`schema_migrations` keeps
//! which ran; the copy-trading's were removed with it on 2026-11-14, their tables left as they were).
//!
//! The strategies never wait on the events: they go to a writer task, which batches them (one insert a second) and,
//! on Postgres, reconnects and retries when the database is away, keeping what it has not written yet.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::log;

#[derive(Clone)]
pub struct Store {
    tx: mpsc::UnboundedSender<Value>,
}

impl Store {
    pub fn event(&self, ev: Value) {
        let _ = self.tx.send(ev);
    }

    /// A store whose events the test reads back.
    #[cfg(test)]
    pub(crate) fn for_test() -> (Store, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Store { tx }, rx)
    }
}

/// The events sent so far.
#[cfg(test)]
pub(crate) fn drain(rx: &mut mpsc::UnboundedReceiver<Value>) -> Vec<Value> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        v.push(e);
    }
    v
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

async fn file_writer(dir: PathBuf, mut rx: mpsc::UnboundedReceiver<Value>) {
    let mut events = match std::fs::OpenOptions::new().create(true).append(true).open(dir.join("events.jsonl")) {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            log!("store: cannot open events.jsonl: {e}");
            return;
        }
    };
    while let Some(ev) = rx.recv().await {
        let _ = writeln!(events, "{ev}");
        if rx.is_empty() {
            let _ = events.flush();
        }
    }
    let _ = events.flush();
}

// --------------------------------------------------------------------------------- postgres

const SCHEMA: &str = "
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
const MIGRATIONS: &[(&str, &str)] = &[];

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

/// A strategy's state document read into its type: the default when none is saved or it does not parse as one.
pub(crate) async fn load_state<T: serde::de::DeserializeOwned + Default>(cfg: &Config, name: &str) -> Result<T> {
    Ok(load_doc(cfg, name).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
}

/// A strategy's state document saved in the background (the strategy never waits on it); a failure is logged as
/// "`kind`: state not saved".
pub(crate) fn save_state<T: serde::Serialize>(cfg: &Config, name: &str, kind: &str, state: &T) {
    let (cfg, name, kind, v) = (cfg.clone(), name.to_string(), kind.to_string(), serde_json::to_value(state).unwrap_or_default());
    tokio::spawn(async move {
        if let Err(e) = save_doc(&cfg, &name, &v).await {
            log!("{kind}: state not saved: {e:#}");
        }
    });
}

/// TLS when the URL asks for it (`sslmode=require` / `verify-ca` / `verify-full`, as hosted
/// databases do), plain otherwise (e.g. Railway's private network).
/// The longest a connect or a batch's write may take before it is given up and tried again.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

async fn pg_connect(url: &str) -> Result<tokio_postgres::Client> {
    tokio::time::timeout(WRITE_TIMEOUT, pg_connect_now(url)).await.unwrap_or_else(|_| Err(anyhow::anyhow!("connecting to postgres: no answer in {} s", WRITE_TIMEOUT.as_secs())))
}

async fn pg_connect_now(url: &str) -> Result<tokio_postgres::Client> {
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

async fn pg_writer(url: String, run: String, client: tokio_postgres::Client, mut rx: mpsc::UnboundedReceiver<Value>) {
    let mut client = Some(client);
    let mut events: Vec<Value> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut open = true;
    while open || !events.is_empty() {
        // Gather until the next tick (or a big batch, or the end), then write.
        let due = tokio::select! {
            ev = rx.recv(), if open => match ev {
                Some(ev) => {
                    events.push(ev);
                    events.len() >= 1000
                }
                None => {
                    open = false;
                    true
                }
            },
            _ = tick.tick() => true,
        };
        if !due || events.is_empty() {
            continue;
        }
        if client.as_ref().is_none_or(|c| c.is_closed()) {
            client = match pg_connect(&url).await {
                Ok(c) => {
                    log!("store: postgres reconnected");
                    Some(c)
                }
                Err(e) => {
                    log!("store: postgres unavailable ({e:#}), {} events waiting", events.len());
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
        }
        let Some(c) = &client else { continue };
        // A connection gone quiet (dropped on the way, not closed) would hang the write for good:
        // given up after WRITE_TIMEOUT, the connection dropped (the server rolls the batch back),
        // and written again on a new one.
        let res = match tokio::time::timeout(WRITE_TIMEOUT, write_events(c, &run, &events)).await {
            Ok(r) => r,
            Err(_) => Err(anyhow::anyhow!("no answer in {} s", WRITE_TIMEOUT.as_secs())),
        };
        match res {
            Ok(()) => events.clear(),
            Err(e) => {
                log!("store: postgres write failed ({e:#}), retrying");
                client = None;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// The events in one statement (all of them or none).
async fn write_events(c: &tokio_postgres::Client, run: &str, events: &[Value]) -> Result<()> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Serialize, serde::Deserialize, Default, Debug, PartialEq)]
    struct S {
        n: u32,
        #[serde(default)]
        coins: Vec<String>,
    }

    #[tokio::test]
    async fn a_state_saved_in_the_background_is_read_back_and_a_missing_or_foreign_one_is_the_default() {
        let cfg = crate::testkit::cfg();
        assert_eq!(load_state::<S>(&cfg, "s").await.unwrap(), S::default());
        save_state(&cfg, "s", "test", &S { n: 3, coins: vec!["BTC".into()] });
        assert!(crate::testkit::saved(&cfg, "s").await.is_some());
        assert_eq!(load_state::<S>(&cfg, "s").await.unwrap(), S { n: 3, coins: vec!["BTC".into()] });
        save_doc(&cfg, "s", &serde_json::json!({"n": "not a number"})).await.unwrap();
        assert_eq!(load_state::<S>(&cfg, "s").await.unwrap(), S::default());
        // a file that is not JSON at all
        std::fs::write(cfg.data_dir.join("s.json"), "{oops").unwrap();
        assert_eq!(load_state::<S>(&cfg, "s").await.unwrap(), S::default());
    }
}
