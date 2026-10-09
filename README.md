# hl-copytrader

Paper copy-trading of Hyperliquid accounts that make money steadily (picked daily from the
leaderboard by their last 3 months): each gets its own $1000 copy account that follows its
trades against the live order books, sized so that each entry risks 2% of the account to a
20% stop.

```
cargo build --release
target/release/hl-copytrader run    --data data          # runs until stopped
target/release/hl-copytrader report --data data [--min-fills 10] [--top 30]
```

`run` options: `--start 1000` (USD per copy account), `--risk 2` (% of equity each entry
loses at our stop), `--stop 20` (% against our entry), `--max-positions 50` (per copy account),
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
  months (30 days each) and a loss in at most 4 of the last 12 weeks (a week without a trade
  counts as neither; 139 on 2026-10-07). Read one at a time with pauses (a few hours); the
  list is saved (`selection`) so a restart within the day keeps it. An account on the list
  stays on it unless it clearly got worse: its last month at a loss, a loss in over 6 weeks,
  or its perp account under $50k (read even if the leaderboard no longer makes it a
  candidate). An account that drops out stays followed while our copy holds positions.
- **Golden list** (`src/engine.rs`): the accounts whose copy makes money. Each copy is measured
  from its enrollment (copies from before the list: from the 2026-10-09 start); after 3 days
  and 5 closed trips, it is golden while its PnL since is above zero. Worked out every 10 min;
  golden accounts are followed even after they drop off the day's list.
  Why: picked by one month's ROI (50%+, as before) the accounts did worse the next month than
  all accounts in 2 of 3 months checked (July-September 2026); picked this way, 69% / 50% / 68%
  were at a profit the next month against 54% / 57% / 51% of all. With a drawdown limit too
  (20%) it was 71% / 83% / 80%, but the list is a third as long.
- **How**: its fills are picked out of the public trades stream (every trade names buyer and
  seller), our order is a taker fill on the live L2 book 1 s later.
  - When it opens a position (from flat, or flips), we open in its direction at a size whose
    stop loses 2% of our equity: 2% / 20% = a position of 10% of equity.
  - While it adds, we hold that size. As it reduces from its largest size in the position we
    reduce in proportion, and we close when it is flat.
  - At most 50 positions open per copy account: a new one beyond that is not entered
    (`skipped` in the state).
  - Our stop: 20% against our average entry (checked every 5 s at the book's mid), closed at
    the book; we stay out of that position until it is flat (`legs` in the state).
- **Enrollment**: on its first fill we read its positions; the one that fill opened is
  followed, the ones it held before are not entered. Its positions are read again every 2 h
  while active (a copy we already hold is resized to our size then).
- **Venue mechanics**: taker fee 0.045%; orders under $10 are not placed unless they close a
  position (the difference waits for its next change); hourly funding at each coin's rate;
  a copy account at zero equity is liquidated and stops. Main perp dex only (no HIP-3
  dexes).

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
  `open_positions`, `liquidated`, `golden` and `measured_pnl` (its PnL since measured for the
  golden list), and `state` (jsonb: cash, positions, the trader's tracked
  positions, all the measures above, open trips), upserted every 30 s when changed.
- `events`: `at`, `kind`, `address`, `coin`, `data` (jsonb):
  - `their_fill`: every fill of a followed account as it reached us (size signed, price,
    exchange time, the exchange's trade id `tid`, feed delay, its position after) — what the
    traders did;
  - `fill`: ours (`why`: seed / copy / reconcile / restart / stop), with lag, the trader's
    price and slippage vs it (`slip_bps`, of it `move_bps`);
  - `enroll`, `plan` (the trader's set-up for a position), `golden` (an account joins or
    leaves the golden list), `liquidated`.
- `selection`: the day's followed traders with their months' PnL and weeks up (one row per run).
- `schema_migrations`: the one-off migrations applied (`MIGRATIONS` in `src/store.rs`);
  `2026-10-07-reset` emptied every table above, for all runs, when the traders' pick changed,
  and dropped the tables of the signals the run used to trade (removed with them);
  `2026-10-09-golden` added the golden columns.
- `bot_status`: the bot's health every 10 min — accounts, followed, open positions, API weight
  used and backlog, how long its 5 s ticks take on average and at most (`tick_*_ms`), and more
  in `data` (`selected`: traders on the day's list; `golden`, `golden_pnl`: on the golden
  list and their copies' PnL since measured). The ticks run on the loop the copies use:
  a few ms is fine, hundreds would start to delay copies.

The database being away does not stop the run: writes wait and go out once it is back.
Without `DATABASE_URL` the same goes to files in `--data`: `state.json`, `events.jsonl`,
`status.jsonl`, `selection.json`.
`report` writes `report.csv` (all accounts, all the measures) to `--data`.
