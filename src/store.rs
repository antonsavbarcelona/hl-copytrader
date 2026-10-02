//! Where a run lives: Postgres when `DATABASE_URL` is set (tables created on first start),
//! else files in the data directory (`state.json`, `events.jsonl`, `signal_trades.jsonl`).
//! The copies (`copy_accounts`, `events`) and the signals (`signal_accounts`, `signal_trades`)
//! are kept apart; `bot_status` (`status.jsonl`) has the bot's health every 10 minutes.
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
use crate::engine::{SIGNAL_PREFIX, Trader};
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

/// One signal variant's account as saved.
#[derive(serde::Serialize)]
pub struct SignalRow {
    pub variant: String,
    pub rule: String,
    pub equity: f64,
    pub roi_pct: f64,
    pub taken: i64,
    pub closed: i64,
    pub wins: i64,
    pub open_positions: i32,
    /// The account (`Trader`), serialized.
    pub state: String,
}

/// One signal trade: written when it opens, written again (whole) when it closes.
#[derive(Clone, Debug, serde::Serialize)]
pub struct TradeRow {
    pub id: String,
    pub variant: String,
    pub coin: String,
    /// long / short
    pub side: String,
    /// Unix seconds.
    pub opened_at: f64,
    /// The mid when it fired, and our fill.
    pub mid: f64,
    pub entry: f64,
    pub stop: f64,
    pub take_profit: f64,
    pub size: f64,
    pub notional: f64,
    pub risk_usd: f64,
    pub risk_pct: f64,
    pub expires_at: f64,
    /// Why it fired: traders each way, agreement, conviction, dollars, window.
    pub reason: Value,
    pub closed_at: Option<f64>,
    pub exit_px: Option<f64>,
    /// stop / take profit / expiry / traders turned
    pub exit_reason: Option<String>,
    /// After fees, USD and % of equity at the open.
    pub pnl: Option<f64>,
    pub pnl_pct: Option<f64>,
    pub fees: f64,
}

enum Cmd {
    Event(Value),
    Save(Vec<Row>),
    SaveSignals(Vec<SignalRow>),
    Trade(TradeRow),
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

    pub fn save_signals(&self, rows: Vec<SignalRow>) {
        let _ = self.tx.send(Cmd::SaveSignals(rows));
    }

    pub fn trade(&self, row: TradeRow) {
        let _ = self.tx.send(Cmd::Trade(row));
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

/// The saved accounts of this run: the copies by address, the signal accounts as
/// `signal:<variant>`.
pub async fn load(cfg: &Config) -> Result<HashMap<String, Trader>> {
    match &cfg.database_url {
        Some(url) => {
            let client = pg_connect(url).await?;
            migrate(&client).await?;
            let rows = client.query(
                "SELECT address, state::text FROM copy_accounts WHERE run_id = $1
                 UNION ALL SELECT $2 || variant, state::text FROM signal_accounts WHERE run_id = $1",
                &[&cfg.run_id, &SIGNAL_PREFIX],
            ).await?;
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
                #[serde(default)]
                signals: HashMap<String, Trader>,
            }
            let path = cfg.data_dir.join("state.json");
            Ok(match std::fs::read_to_string(&path) {
                Ok(s) => {
                    let saved = serde_json::from_str::<Saved>(&s).with_context(|| format!("{}", path.display()))?;
                    let mut out = saved.traders;
                    out.extend(saved.signals.into_iter().map(|(v, t)| (format!("{SIGNAL_PREFIX}{v}"), t)));
                    out
                }
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
    let mut trades = match std::fs::OpenOptions::new().create(true).append(true).open(dir.join("signal_trades.jsonl")) {
        Ok(f) => std::io::BufWriter::new(f),
        Err(e) => {
            log!("store: cannot open signal_trades.jsonl: {e}");
            return;
        }
    };
    // Every account as last saved (the file holds all of them): copies, signal variants.
    let mut all: HashMap<String, String> = HashMap::new();
    let mut sigs: HashMap<String, String> = HashMap::new();
    let obj = |m: &HashMap<String, String>| m.iter().map(|(a, s)| format!("{}:{s}", Value::String(a.clone()))).collect::<Vec<_>>().join(",");
    while let Some(cmd) = rx.recv().await {
        match cmd {
            Cmd::Event(ev) => {
                let _ = writeln!(events, "{ev}");
            }
            Cmd::Trade(t) => {
                if let Ok(s) = serde_json::to_string(&t) {
                    let _ = writeln!(trades, "{s}");
                }
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
            }
            Cmd::SaveSignals(rows) => {
                for r in rows {
                    sigs.insert(r.variant, r.state);
                }
                // The signals are saved right after the copies: write the file once for both.
                let tmp = state_path.with_extension("tmp");
                if std::fs::write(&tmp, format!("{{\"traders\":{{{}}},\"signals\":{{{}}}}}", obj(&all), obj(&sigs))).is_ok() {
                    let _ = std::fs::rename(&tmp, &state_path);
                }
                let _ = events.flush();
                let _ = trades.flush();
            }
            Cmd::Flush(done) => {
                let _ = events.flush();
                let _ = trades.flush();
                let _ = done.send(());
            }
        }
    }
    let _ = events.flush();
    let _ = trades.flush();
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
CREATE TABLE IF NOT EXISTS signal_accounts (
    run_id          text        NOT NULL,
    variant         text        NOT NULL,
    rule            text        NOT NULL,
    equity          float8      NOT NULL,
    roi_pct         float8      NOT NULL,
    taken           bigint      NOT NULL,
    closed          bigint      NOT NULL,
    wins            bigint      NOT NULL,
    open_positions  integer     NOT NULL,
    state           jsonb       NOT NULL,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, variant)
);
CREATE TABLE IF NOT EXISTS signal_trades (
    run_id       text        NOT NULL,
    id           text        NOT NULL,
    variant      text        NOT NULL,
    coin         text        NOT NULL,
    side         text        NOT NULL,
    opened_at    timestamptz NOT NULL,
    mid          float8      NOT NULL,
    entry        float8      NOT NULL,
    stop         float8      NOT NULL,
    take_profit  float8      NOT NULL,
    size         float8      NOT NULL,
    notional     float8      NOT NULL,
    risk_usd     float8      NOT NULL,
    risk_pct     float8      NOT NULL,
    expires_at   timestamptz NOT NULL,
    reason       jsonb       NOT NULL,
    closed_at    timestamptz,
    exit_px      float8,
    exit_reason  text,
    pnl          float8,
    pnl_pct      float8,
    fees         float8      NOT NULL,
    PRIMARY KEY (run_id, id)
);
CREATE INDEX IF NOT EXISTS signal_trades_variant ON signal_trades (run_id, variant, opened_at);
CREATE INDEX IF NOT EXISTS signal_trades_coin ON signal_trades (run_id, coin, opened_at);
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
    signals_avg_ms  float8      NOT NULL,
    signals_max_ms  float8      NOT NULL,
    data            jsonb       NOT NULL,
    PRIMARY KEY (run_id, at)
);
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
    // The same for the signal variants, and signal trades by id (a close replaces its open).
    let mut pending_sigs: HashMap<String, SignalRow> = HashMap::new();
    let mut written_sigs: HashMap<String, u64> = HashMap::new();
    let mut trades: HashMap<String, TradeRow> = HashMap::new();
    let mut statuses: Vec<Value> = Vec::new();
    let mut waiting: Vec<oneshot::Sender<()>> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut open = true;
    while open || !events.is_empty() || !pending.is_empty() || !pending_sigs.is_empty() || !trades.is_empty() || !statuses.is_empty()
        || !waiting.is_empty() {
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
                Some(Cmd::SaveSignals(rows)) => {
                    for r in rows {
                        if written_sigs.get(&r.variant) != Some(&hash(&r.state)) {
                            pending_sigs.insert(r.variant.clone(), r);
                        }
                    }
                    false
                }
                Some(Cmd::Trade(t)) => {
                    trades.insert(t.id.clone(), t);
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
        if !due || (events.is_empty() && pending.is_empty() && pending_sigs.is_empty() && trades.is_empty() && statuses.is_empty()
            && waiting.is_empty()) {
            continue;
        }
        if client.as_ref().is_none_or(|c| c.is_closed()) {
            client = match pg_connect(&url).await {
                Ok(c) => {
                    log!("store: postgres reconnected");
                    Some(c)
                }
                Err(e) => {
                    log!("store: postgres unavailable ({e:#}), {} events, {} accounts, {} signal trades waiting", events.len(),
                        pending.len() + pending_sigs.len(), trades.len());
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            };
        }
        let Some(c) = &client else { continue };
        match write(c, &run, &events, &pending, &pending_sigs, &trades, &statuses).await {
            Ok(()) => {
                events.clear();
                trades.clear();
                statuses.clear();
                for (a, r) in pending.drain() {
                    written.insert(a, hash(&r.state));
                }
                for (v, r) in pending_sigs.drain() {
                    written_sigs.insert(v, hash(&r.state));
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

async fn write(c: &tokio_postgres::Client, run: &str, events: &[Value], accounts: &HashMap<String, Row>,
               signals: &HashMap<String, SignalRow>, trades: &HashMap<String, TradeRow>, statuses: &[Value]) -> Result<()> {
    if !statuses.is_empty() {
        let rows = serde_json::to_string(statuses)?;
        c.execute(
            "INSERT INTO bot_status (run_id, at, copy_accounts, followed, open_positions, api_weight, api_backlog_s, tick_avg_ms,
                 tick_max_ms, signals_avg_ms, signals_max_ms, data)
             SELECT $1, to_timestamp(r.at), r.copy_accounts, r.followed, r.open_positions, r.api_weight, r.api_backlog_s, r.tick_avg_ms,
                 r.tick_max_ms, r.signals_avg_ms, r.signals_max_ms, d
             FROM jsonb_array_elements($2::text::jsonb) AS d,
                  jsonb_to_record(d) AS r(at float8, copy_accounts int4, followed int4, open_positions int4, api_weight int4,
                      api_backlog_s float8, tick_avg_ms float8, tick_max_ms float8, signals_avg_ms float8, signals_max_ms float8)
             ON CONFLICT DO NOTHING",
            &[&run, &rows],
        )
        .await
        .context("saving status")?;
    }
    if !signals.is_empty() {
        let rows = serde_json::to_string(&signals.values().collect::<Vec<_>>())?;
        c.execute(
            "INSERT INTO signal_accounts (run_id, variant, rule, equity, roi_pct, taken, closed, wins, open_positions, state, updated_at)
             SELECT $1, r.variant, r.rule, r.equity, r.roi_pct, r.taken, r.closed, r.wins, r.open_positions, r.state::jsonb, now()
             FROM jsonb_to_recordset($2::text::jsonb) AS r(variant text, rule text, equity float8, roi_pct float8, taken int8, closed int8,
                  wins int8, open_positions int4, state text)
             ON CONFLICT (run_id, variant) DO UPDATE SET rule = excluded.rule, equity = excluded.equity, roi_pct = excluded.roi_pct,
                 taken = excluded.taken, closed = excluded.closed, wins = excluded.wins, open_positions = excluded.open_positions,
                 state = excluded.state, updated_at = excluded.updated_at",
            &[&run, &rows],
        )
        .await
        .context("saving signal accounts")?;
    }
    if !trades.is_empty() {
        let rows = serde_json::to_string(&trades.values().collect::<Vec<_>>())?;
        c.execute(
            "INSERT INTO signal_trades (run_id, id, variant, coin, side, opened_at, mid, entry, stop, take_profit, size, notional, risk_usd,
                 risk_pct, expires_at, reason, closed_at, exit_px, exit_reason, pnl, pnl_pct, fees)
             SELECT $1, r.id, r.variant, r.coin, r.side, to_timestamp(r.opened_at), r.mid, r.entry, r.stop, r.take_profit, r.size, r.notional,
                 r.risk_usd, r.risk_pct, to_timestamp(r.expires_at), r.reason, to_timestamp(r.closed_at), r.exit_px, r.exit_reason, r.pnl,
                 r.pnl_pct, r.fees
             FROM jsonb_to_recordset($2::text::jsonb) AS r(id text, variant text, coin text, side text, opened_at float8, mid float8,
                  entry float8, stop float8, take_profit float8, size float8, notional float8, risk_usd float8, risk_pct float8,
                  expires_at float8, reason jsonb, closed_at float8, exit_px float8, exit_reason text, pnl float8, pnl_pct float8, fees float8)
             ON CONFLICT (run_id, id) DO UPDATE SET closed_at = excluded.closed_at, exit_px = excluded.exit_px,
                 exit_reason = excluded.exit_reason, pnl = excluded.pnl, pnl_pct = excluded.pnl_pct, fees = excluded.fees",
            &[&run, &rows],
        )
        .await
        .context("saving signal trades")?;
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
