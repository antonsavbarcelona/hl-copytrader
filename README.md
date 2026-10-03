# hl-copytrader

Paper copy-trading of the top Hyperliquid leaderboard accounts: each gets its own
$1000 copy account that mirrors it 1:1 against the live order books.

```
cargo build --release
target/release/hl-copytrader run    --data data          # runs until stopped
target/release/hl-copytrader report --data data [--min-fills 10] [--top 30]
```

`run` options: `--start 1000` (USD per copy account), `--min-equity 1000`,
`--min-pnl 10000` (leaderboard PnL this month, USD), `--min-roi 50` (leaderboard ROI this month, %),
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
  (more `run` options, e.g. `--min-roi 30 --min-pnl 5000`).
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

- **Who**: every account on the official leaderboard
  (`stats-data.hyperliquid.xyz/Mainnet/leaderboard`) that traded this month with equity
  of $1000 or more, month PnL of $10k or more and month ROI over 50% (~1.3k on 2026-10-02),
  re-read every 6 h. An account that drops out stays followed while our copy holds positions.
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

## Signals

Besides copying each trader, the run trades its own signals: a grid of 61 variants side by side
(`VARIANTS` in `src/signals.rs`), each on its own $1000 paper account (`signal:<name>`) filled on
the live books like the copies.

Out of what the followed traders do, per coin over a window from each trader's net flow (bought
minus sold, so a split order or a market maker's churn counts once):

- **heads** (`h…`): traders net buying vs net selling; a trader takes a side when its net flow
  is 0.2%+ of its own equity, so one whale does not outvote the crowd;
- **conviction** (`c…`): the sum of the traders' net flows, each as % of its own equity
  (one trader counts up to 20%);
- **volume** (`v…`): dollars of net buying vs net selling;
- **positioning** (`p…`): the copied traders holding the coin long vs short now (1%+ of equity);
  `fade-p…-fund` bet against a crowded side only when funding (0.00125%/h+, ~11% a year) is paid
  by that side, `-oi` also when open interest rose 3%+ in 4 h;
- **clusters** (`sl…`): the copied traders' stop orders and liquidation prices (read with their
  set-ups) within 1–2% of the price: $250k–$1M of them on one side (70%+ of those in the band)
  is where a cascade would start; the trade goes the way they would push the price.

Out of everyone's trades (the public stream names every taker):

- **whales** (`w…`): wallets whose net taker flow in a coin over 5–60 min is $250k–$1M+.

Out of prices alone (mids every minute; at a start the last 5 h of one-minute candles of the 60
most traded coins are read, so these do not wait hours):

- **trend** (`t…`): a coin among the 60 most traded moved 0.6%+ in 15 min / 1.2%+ in 60 min:
  followed; `fade-t240m…`: 3.3%+ in 4 h, faded (reversal);
- **cross momentum** (`x…`): among the 40 most traded, the 3 strongest against BTC over 60 / 240
  min long, the 3 weakest short, held as long and entered again while still among them.

The grid spans windows of 1 / 5 / 15 / 30 / 60 / 240 min, 2–20 traders, 60–90% agreement,
$200k–$2M, and three exits — S (stop 0.75%, take profit 1.5%), M (1.5 / 3%), L (3 / 6%) — with
one signal run under all three to tell the signal from the exit. `best-…` count only the traders
whose copies run at a profit, `low-…` only those whose every leverage setting seen is 10x or
less with an account of $30k+; `fade-…` take the opposite trade, as controls (if a fade wins
too, the signal is noise). Exit variations of the best signals: `-tr` a trailing stop (1.5% /
3% behind the best price since the entry, no take profit), `-be` the stop moved to the entry
once the trade is 1R up, `-60` held at most 60 min. A name reads e.g. `h15m-5t-75-M`: heads, 15
min, 5+ traders, 75%+ of them one way, exit M.

Every entry is a complete trade, a row of `signal_trades`: coin, side, entry, stop, take
profit, expiry, size from 1% of the account's equity at risk at the stop (all positions at
most 10x equity, at most 10 open), and why (traders each way, agreement, conviction, dollars
each way). A trade is closed at its stop, take profit or expiry, or when the traders turn the
other way — the same row gets the exit, its reason, PnL after fees and the fees — and the coin
then rests for the variant's window. Signals are read every 5 s; after a start, a window is read only once the
flow covers it. Stops and take profits are checked on every change of the coin's book, not only every 5 s.
`report` lists the signal accounts after the copies.

**Market vs limit orders.** Every signal trade above is a market trade: taker in (0.045%), taker
out. Each one also gets two limit-order twins of the same size (`signal_maker_trades`, by
`parent_id`), so the two ways of trading the same signals compare trade by trade:

- the entry is a post-only order at the most aggressive maker price (one tick inside the
  opposite best when the spread is wider than a tick, else our side's best), following the price
  when it moves away, for up to 30 s; then `limit` cancels the rest (a trade that got nothing is
  "not filled"), `limit+market` takes it at the book;
- trades through its price fill it by their size; trades at its price, once they used up the size
  that rested ahead of it (the size at that level when it was placed, less what the book later
  shows); and so does what the opposite side of the book shows at or through its price (counted
  once while it stays there); maker fee 0.015%;
- once in: stop and take profit at the same distances from its own entry, the take profit a resting
  limit order (maker), the stop, expiry and the traders turning at the book (taker).

```
select m.mode, count(*), avg((m.filled > 0)::int) filled_share, avg(m.maker_pct) maker_pct,
       sum(m.pnl) limit_pnl, sum(t.pnl) market_pnl, sum(m.fees) limit_fees, sum(t.fees) market_fees
from signal_maker_trades m join signal_trades t on t.run_id = m.run_id and t.id = m.parent_id
where m.closed_at is not null and t.closed_at is not null group by 1
```

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
- `signal_accounts`: one row per signal variant — `rule`, `equity`, `roi_pct`, `taken`,
  `closed`, `wins`, `open_positions`, `state`.
- `signal_trades`: one row per signal trade (key `id`), see Signals; e.g.
  `select variant, count(*), avg((pnl > 0)::int), sum(pnl) from signal_trades
  where closed_at is not null group by 1 order by 4 desc`.
- `signal_maker_trades`: the limit-order twins (key `id` = `parent_id:mode`): how much of the
  market trade's size it got and how (`filled`, `maker_pct`, `wait_s`, `requotes`), its entry vs
  the market trade's (`market_entry`), exit (`exit_reason` also "not filled", `exit_maker_pct`),
  PnL after fees and the fees each way.
- `bot_status`: the bot's health every 10 min — accounts, followed, open positions, API weight
  used and backlog, how long its 5 s ticks take on average and at most (`tick_*_ms`, of it
  `signals_*_ms`), and more in `data` (of it the watched coins' trades and book changes:
  `market_msgs`, `market_*_ms`, `watched_coins`, `maker_trades_open`; `whale_wallets` tracked,
  `ctx_coins` with funding / open interest). The ticks run on the loop
  the copies use: a few ms is fine, hundreds would start to delay copies.

The database being away does not stop the run: writes wait and go out once it is back.
Without `DATABASE_URL` the same goes to files in `--data`: `state.json`, `events.jsonl`,
`signal_trades.jsonl` (a line when a trade opens, another when it closes),
`signal_maker_trades.jsonl` (a line at every change), `status.jsonl`.
`report` writes `report.csv` (all accounts, all the measures) to `--data`.
