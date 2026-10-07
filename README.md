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
$200k–$2M, and three exits — S, M, L — with one signal run under all three to tell the signal
from the exit. Stops follow each coin's volatility: S 1.5x, M 3x, L 5x its average hourly range
(high − low of the minute mids over the last 4 h), 0.5% to 15%, the take profit twice the stop
(about 2x the range over the profile's holding time, so an alt swinging 3% an hour gets a 9% M
stop and BTC moving 0.2% a 0.6% one); until a coin has 3 h of prices, the fixed 0.75 / 1.5 / 3%
stops (take profit 1.5 / 3 / 6%). The size still risks 1% of the account at the stop: a wider
stop is a smaller position. Every coin's last 5 h of one-minute prices are read at a start. `best-…` count only the traders
whose copies run at a profit, `low-…` only those whose every leverage setting seen is 10x or
less with an account of $30k+; `fade-…` take the opposite trade, as controls (if a fade wins
too, the signal is noise). Exit variations of the best signals: `-tr` a trailing stop (as far
behind the best price since the entry as the stop, or twice that, no take profit), `-be` the stop moved to the entry
once the trade is 1R up, `-60` held at most 60 min. A name reads e.g. `h15m-5t-75-M`: heads, 15
min, 5+ traders, 75%+ of them one way, exit M.

**Top traders, out with them** (`top-…`). Only the top 10% of the followed traders by their copy's
PnL (those at a profit, 3+ copy fills), the list worked out again every hour, so who is in it
rotates (`top10` in `bot_status`). `-F<n>` variants exit with the traders instead of at a take
profit: held while the listed traders' positions in the coin, summed as % of each one's equity,
stay the way they took ("traders out" once they are out or turned), a fixed n% stop (5 / 10 / 20,
not from the volatility), at most 48 h; a fresh flow they already undid is not entered. The same
signal also runs under the usual exit (`-M`, `-L`), `best-h15m-3t-70-F10` takes every profitable
copy with that exit, `fade-top-h15m-2t-70-F10` is the control.

**Smart money by action** (`sm-…`, `src/smart.rs`). Each followed trader's fills become actions
on its position — open from flat, add, reduce, close, flip (fills of one order, 2 s apart, are
one action; a close and an open the other way within 10 min, a flip) — with their context: the
position's PnL and age, the price since its last action, its size against its usual entries,
what it closed elsewhere just before, its book. Every entry (one per trader, coin and side per 15
min: scaling in is one decision) is scored by the price 5 / 15 / 60 / 240 min later, at 60 min
per coin, direction and regime (trend: 2%+ over 4 h, else range), with a recent form halving
daily, how far it went its way / against in the hour, and how many other traders entered the
same way in the 30 min before / after. These ratings start empty and grow with the run (kept in
`smart_state`). A variant counts the traders whose latest action of its kind still stands:

| # | kind | variant | fires on |
|---|------|---------|----------|
| 1 | entry | `sm-open-15m-2t-1p-M`, `sm-open-15m-1t-10p-100k-L` | opens from flat (1%+ of equity; one of 10%+ and $100k+) |
| 2 | add | `sm-add50-15m-2t-M` | adds of half the position or more |
| 3 | exit | `sm-exit-15m-2t-M` | half or more off positions (2%+) held 1 h+ |
| 4 | flip | `sm-flip-15m-1t-M` / `-L` | long to short or back, both sides 2%+ |
| 5 | accelerating | `sm-accel-10m-1t-M` | flow growing over each third of 10 min, 3%+ in all |
| 6 | fresh | `sm-fresh-30m-2t-M` | positions of 5%+ opened in the last 30 min |
| 7 | winner adds / loser cuts | `sm-winadd-30m-2t-M`, `sm-losecut-30m-2t-M` | adds to positions 1%+ up / cuts of positions 1%+ down |
| 8 | averaging | `sm-avgdown-60m-2t-L`, `sm-avgup-60m-2t-L` | adds after the price went 2%+ against / for it |
| 9 | coordinated | `sm-coord-5m-3t-M` | 3+ independent traders opening (traders entering together 3+ times count once) |
| 10 | leaders | `sm-leader-5m-1t-M` | entries by traders others follow in (2x as many after as before) |
| 11 | specialists | `sm-spec-15m-1t-M` | entries by traders good in this coin (20 bp+) and direction |
| 12 | risk-adjusted | `sm-riskadj-15m-2t-M` | entries by traders whose copies made more than their worst drawdown |
| 13 | recent form | `sm-recent-15m-2t-M` | entries by traders 10 bp+ in recent form |
| 14 | regime | `sm-regime-15m-2t-M` | entries by traders 10 bp+ in the coin's current regime |
| 15 | size surprise | `sm-sizez-15m-1t-25k-M` | entries 2.5 sd over the trader's usual size, $25k+ |
| 16 | concentration | `sm-conc-30m-1t-25p-L` | entries of 25%+ taking the coin to 1x equity and half the book (+25 points) |
| 17 | rotation | `sm-rot-15m-1t-M` | new positions of 5%+ right after closing as much elsewhere |
| 18 | disagreement | `sm-disagree-30m-2t-M` | the top-rated tenth one way, 60%+ of the others the other |
| 19 | vs the crowd | `sm-vscrowd-30m-3t-250k-M` | positively rated traders one way, $250k+ of all takers' flow the other |
| 20 | vs funding / OI | `sm-fund-entry-30m-3t-L`, `sm-fund-exit-30m-3t-L` | entries paid funding to hold with OI up 1%+ in 1 h / exits from a side paying crowded funding |
| 21 | early | `sm-early-15m-1t-M` | entries by traders whose entries lead the price at 15 min (t ≥ 2) |
| 22 | execution | `sm-exec-15m-1t-M` | entries by traders whose entries go 60%+ their way |
| 23 | horizon | `sm-hz15-15m-1t-M` (held 30 min), `sm-hz240-30m-1t-L` | entries by traders whose edge is at 15 min / 4 h |
| 24 | playbook | `sm-playbook-30m-1t-M` | a probe (1% or less) left 10 min+, then within 2 h an add to 4%+ |
| 25 | capitulation | `sm-capit-30m-1t-L` | averaged down twice, then 75%+ off at 3%+ down: faded |

Controls: `fade-sm-open-…`, `fade-sm-flip-…`, `fade-sm-coord-…`. The rated kinds (10–14, 18, 19,
21–23) stay quiet until the ratings have enough entries (5–10 per trader).

Every entry is a complete trade, a row of `signal_trades`: coin, side, entry, stop, take
profit, expiry, size from 1% of the account's equity at risk at the stop (all positions at
most 10x equity, at most 10 open; 100 for those exiting with the traders), and why (traders each way, agreement, conviction, dollars
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
- `smart_state`: the smart-money positions and ratings (one row per run), every 10 min.
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
