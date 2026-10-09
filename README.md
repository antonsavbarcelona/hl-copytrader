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
loses at our stop), `--stop 20` (% against our entry), `--max-positions 50` (over all copy accounts),
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

- Optional variables: `RUN_ID` (default `main`), `API_WEIGHT` (default 800), `LIVE_*` (see
  Live), `EXTRA_ARGS`
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
  and 5 closed trips, it is golden while its PnL since is above zero; until then an account
  keeps its place. The list started (migrations `2026-10-09-golden-seed`, `-seed-all`) with the
  19 accounts that can be copied (swing traders on liquid coins, not market makers, HFT or
  grids), 13 of them with copies at a profit on 2026-10-09. Worked out every 10 min; golden accounts are followed
  even after they drop off the day's list.
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
  - At most 50 positions open at once over all copy accounts: a new one beyond that is not
    entered (`skipped` in the trader's state). Positions already open run their course.
  - Our stop: 20% against our average entry (checked every 5 s at the book's mid), closed at
    the book; we stay out of that position until it is flat (`legs` in the state).
- **Enrollment**: on its first fill we read its positions; the one that fill opened is
  followed, the ones it held before are not entered. Its positions are read again every 2 h
  while active (a copy we already hold is resized to our size then).
- **Venue mechanics**: taker fee 0.045%; orders under $10 are not placed unless they close a
  position (the difference waits for its next change); hourly funding at each coin's rate;
  a copy account at zero equity is liquidated and stops. Main perp dex only (no HIP-3
  dexes).

## Live

With `LIVE_ACCOUNT` (the account's address) and `LIVE_KEY` (its private key, or an API
wallet's approved for it) set, the golden list is also traded for real on that account:
testnet (`api.hyperliquid-testnet.xyz`) unless `LIVE_NET=mainnet` (`src/live.rs`).

- Health check at every start: a $15 market buy of BTC (else ETH, SOL: one not held), sold at
  once; the result (ok, prices, fee, PnL, time, error) is a row of `live_checks`
  (`live_checks.jsonl` without a database). A failed one is logged; trading runs on regardless.
- A position a golden account enters is entered with a market order (IOC 2% through the mid)
  at a size whose stop loses 2% of the account's equity (unified account: perp value plus the
  spot USDC not held as margin), and a reduce-only stop-market order rests on the exchange 20%
  against our fill. It is then followed to its end (reduced as the trader reduces from its
  peak, closed when it is flat, or closed whole when a reduce would leave under $11, which
  could not be sold: the exchange checks its $10 minimum at the order's price), golden or not
  by then.
- Each account's position in a coin is a leg of ours on its own: its own entry, size, stop
  order (for the leg's size, placed again when the leg's size changes) and exits; the exchange
  holds their sum. A leg the other way from the coin's legs is not entered (on one account a
  long and a short in a coin would cancel out, and their stops act on each other's size); at
  50 legs open none is (`skipped`, with why). Every minute the exchange's positions and orders
  are read: a leg whose stop order is gone was stopped out; a coin's position gone (a
  liquidation) ends its legs.
- Prices are the live venue's own: on testnet, its books, not mainnet's.
- State in `docs` (name `live`; `live.json` without a database); every action an event of kind
  `live` (`what`: open / add / reduce / close / gone / error); `live` in `bot_status.data`
  (equity, positions, orders, skipped).
- `hl-copytrader live-check [COIN]` trades through the same path to check the account and key:
  two accounts' legs in the coin with their stops, one half out, both out, a third one's entry
  the other way skipped.

## Liquidation research

A separate strategy, researched on paper apart from the copies (`src/liq.rs`; its own tables,
its own paper accounts; it shares only the process, the books and the API budget, in which
it goes last): when the price reaches a dense level of liquidations, does it run on through
it (a cascade) or bounce back?

- The positions of the leaderboard's accounts of $250k+ (~7.7k) are read one after another,
  each position's liquidation price kept; a pass takes ~30 min at ~300 weight a minute, and
  waits while the API budget is over 5 s behind (the copies and the day's selection first).
- Every 5 s, per coin trading $10M+ a day (~37): the notional to be liquidated summed by
  liquidation price in 0.5% bins, longs below the price and shorts above; a bin of $250k+ is
  a cluster. Logged every 10 min (`liq:` lines: the biggest within 10% of the price, and each
  strategy's equity).
- `liq_touches`: when the mid (mainnet's books) enters a cluster's bin, that touch followed for
  an hour: the move from it in the cascade's direction at 5 / 15 / 60 min, its furthest point
  and when, the bounce from there, the worst against, whether it went through the bin.
- `liq_trades`: on every touch, four strategies trade on paper, each on its own $1000 account:
  `cascade_15m` / `cascade_60m` (with the move) and `bounce_15m` / `bounce_60m` (against it),
  out after 15 / 60 min or at a 2% stop, sized to lose 2% of the account there; taker fills on
  the book in and out, taker fees. Their equities are kept across restarts (`docs`, `liq_sim`).
- `liq_daily`: at midnight UTC, each strategy's day per cluster size (all, 0.25-1M, 1-5M, 5M+):
  trades, winners, average PnL (bp of notional), PnL, equity at the end.

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
    leaves the golden list), `liquidated`;
  - `live` (the live account's orders).
- `selection`: the day's followed traders with their months' PnL and weeks up (one row per run).
- `docs`: named state documents of a run (`live`: the live account's positions).
- `live_checks`: the live account's health check at every start (migration
  `2026-10-09-live-checks`).
- `liq_touches`, `liq_trades`, `liq_daily`: the liquidation research (see above; migration
  `2026-10-09-liq-tables`).
- `schema_migrations`: the one-off migrations applied (`MIGRATIONS` in `src/store.rs`);
  `2026-10-07-reset` emptied every table above, for all runs, when the traders' pick changed,
  and dropped the tables of the signals the run used to trade (removed with them);
  `2026-10-09-golden` added the golden columns, `2026-10-09-golden-seed` and `-seed-all` put
  the first 19 accounts on the golden list.
- `bot_status`: the bot's health every 10 min — accounts, followed, open positions, API weight
  used and backlog, how long its 5 s ticks take on average and at most (`tick_*_ms`), and more
  in `data` (`selected`: traders on the day's list; `golden`, `golden_pnl`: on the golden
  list and their copies' PnL since measured). The ticks run on the loop the copies use:
  a few ms is fine, hundreds would start to delay copies.

The database being away does not stop the run: writes wait and go out once it is back.
Without `DATABASE_URL` the same goes to files in `--data`: `state.json`, `events.jsonl`,
`status.jsonl`, `selection.json`.
`report` writes `report.csv` (all accounts, all the measures) to `--data`.
