# hl-copytrader

Paper copy-trading of Hyperliquid accounts that make money steadily (picked daily from the
leaderboard by their last 3 months): each gets its own $1000 copy account that mirrors it 1:1
against the live order books.

```
cargo build --release
target/release/hl-copytrader run    --data data          # runs until stopped
target/release/hl-copytrader report --data data [--min-fills 10] [--top 30]
```

`run` options: `--start 1000` (USD per copy account),
`--delay-ms 1000` (our order lands this long after the trader's fill reaches us),
`--weight 400` (info API weight per minute; 1200 per IP shared with anything else).
Needs Rust 1.85+. The running copy uses `data/bin/hl-copytrader.exe` so rebuilds are not
blocked by a locked exe.

## Running on a server

**Railway**: `railway.toml` and the `Dockerfile` are all it needs.

1. Create a service from this repo (or `railway up` from this directory).
2. Add a Postgres database to the project, and on the service set the variable
   `DATABASE_URL` = `${{Postgres.DATABASE_URL}}`. Tables are created on first start.
3. No port, domain or volume is needed: it is a worker.

- Optional variables: `RUN_ID` (default `main`), `API_WEIGHT` (default 800), `EXTRA_ARGS`
  (more `run` options, e.g. `--start 2000`).
- Report: locally with `DATABASE_URL` set to the database's public URL (in `.env`, see
  `.env.example`): `hl-copytrader report`. Or SQL on the tables below.
- Health: the `bot_status` table (every 10 min); `api_backlog_s` should stay at a few seconds
  and `tick_max_ms` well under a second.

**Any Docker host**:

```
docker build -t hl-copytrader .
docker run -d --name hl-copytrader --restart unless-stopped -v /srv/hl-copytrader:/data hl-copytrader
docker logs -f hl-copytrader
docker exec hl-copytrader hl-copytrader report --data /data
```

A stop (SIGTERM) saves the state first. After any start, accounts with positions are read
again (20 per 5 s), so trades missed while it was down (or after a crash, up to 30 s of
unsaved state) are caught up. One process per IP: the info API budget (1200 weight per
minute) is per IP, and the image takes 800 of it.

## What is copied

- **Who** (`src/stable.rs`): once a day, from the official leaderboard
  (`stats-data.hyperliquid.xyz/Mainnet/leaderboard`), the accounts of $100k+ at a profit this
  month and over all time that trade 0.5-200x their equity a month (~3.7k on 2026-10-07); of
  those, the ones whose perp PnL history (`portfolio`) shows a profit in each of the last 3
  months (30 days each) and in 8+ of the last 12 weeks (139 on 2026-10-07). Read one at a time
  with pauses (a few hours); the list is saved (`selection`) so a restart within the day keeps
  it. An account that drops out stays followed while our copy holds positions.
  Why: picked by one month's ROI (50%+, as before) the accounts did worse the next month than
  all accounts in 2 of 3 months checked (July-September 2026); picked this way, 69% / 50% / 68%
  were at a profit the next month against 54% / 57% / 51% of all. With a drawdown limit too
  (20%) it was 71% / 83% / 80%, but the list is a third as long.
- **How**: our position in each coin = its position x $1000 / its equity (leaderboard
  accountValue, or its perp account value if larger). Its fills are picked out of the public
  trades stream (every trade names buyer and seller), our order is a taker fill on the live
  L2 book 1 s later. No caps, stops or filters of our own.
- **Enrollment**: on its first fill we read its positions and mirror them at once ("seed"
  fills, not counted as its activity); its positions are read again every 2 h while active.
- **Venue mechanics**: taker fee 0.045%; orders under $10 are not placed unless they close a
  position (the difference waits for its next change); hourly funding at each coin's rate;
  a copy account at zero equity is liquidated and stops; position / equity over 60x is
  treated as a stale equity read and re-read first. Main perp dex only (no HIP-3 dexes).

## What is measured

Per copy account, kept in `state.json` (`stats`) and summed up by `report`:

- **Lag**: the trader's fill (exchange time) -> our fill: median / p90 / p99 / max, and the
  feed's share (trade -> us). Measured on the exchange's clock: ours is synced to it every
  10 min (offset and API round trip in `run.log`).
- **Price vs the trader's**: our fill price vs its average price on the trade we copied
  (+ = we paid more), in bps and USD, split into the best price having moved (delay, spread)
  and our order walking the book; share of fills worse / better / same.
- **Bets**: every copy fill that opens or adds to a position, notional as % of our equity
  then (average and biggest).
- **Trips** (flat -> flat): count, win rate, PnL after fees, hold time, largest size and the
  deepest point it went against us (realized + unrealized), % of equity at its open — the
  risk each trade actually took.
- **The trader's set-up** for each of our trips, read from the exchange 10 s after its entry
  (at most once a minute per account) and at every positions read: margin (cross /
  isolated), leverage setting, liquidation price, its stop and take-profit orders
  (`frontendOpenOrders`), and the **planned risk**: what it loses if its stops are hit and
  the rest of the position goes to its liquidation price (or to zero when it has none), % of
  its equity. Stops it keeps off the exchange (placed by hand or by its own bot) are not
  visible. The riskiest set-up seen while the trip is open is kept.
- **At once**: most positions open together, highest leverage (gross notional / equity),
  max drawdown of the equity curve (marked every 5 s).

## Data

With `DATABASE_URL` (env or `.env`) the run is kept in Postgres, tables created on first start;
`RUN_ID` (default `main`) names the run, so runs with different settings can share a database:

- `copy_accounts`: one row per copy account — `equity`, `roi_pct`, `copy_fills`,
  `open_positions`, `liquidated`, and `state` (jsonb: cash, positions, the trader's tracked
  positions, all the measures above, open trips), upserted every 30 s when changed.
- `events`: `at`, `kind`, `address`, `coin`, `data` (jsonb):
  - `their_fill`: every fill of a followed account as it reached us (size signed, price,
    exchange time, the exchange's trade id `tid`, feed delay, its position after) — what the
    traders did;
  - `fill`: ours (`why`: seed / copy / reconcile / restart / stale), with lag, the trader's
    price and slippage vs it (`slip_bps`, of it `move_bps`);
  - `enroll`, `plan` (the trader's set-up for a position), `liquidated`.
- `selection`: the day's followed traders with their months' PnL and weeks up (one row per run).
- `schema_migrations`: the one-off migrations applied (`MIGRATIONS` in `src/store.rs`);
  `2026-10-07-reset` emptied every table above, for all runs, when the traders' pick changed,
  and dropped the tables of the signals the run used to trade (removed with them).
- `bot_status`: the bot's health every 10 min — accounts, followed, open positions, API weight
  used and backlog, how long its 5 s ticks take on average and at most (`tick_*_ms`), and more
  in `data` (`selected`: traders on the day's list). The ticks run on the loop the copies use:
  a few ms is fine, hundreds would start to delay copies.

The database being away does not stop the run: writes wait and go out once it is back.
Without `DATABASE_URL` the same goes to files in `--data`: `state.json`, `events.jsonl`,
`status.jsonl`, `selection.json`.
`report` writes `report.csv` (all accounts, all the measures) to `--data`.
