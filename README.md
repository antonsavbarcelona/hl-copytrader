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
loses at our stop), `--stop 20` (% against our entry), `--max-positions 50` (legs the live account holds at once),
`--delay-ms 0` (a delay added before our order, after the trader's fill reaches us; none by
default: our latency is the feed's and our own),
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

- **Round 1, who** (`src/stable.rs`, at every read of the official leaderboard,
  `stats-data.hyperliquid.xyz/Mainnet/leaderboard`, every 6 h): the large accounts that trade:
  $250k+ (spot and vaults counted) and 0.5-200x that traded in the last month (~3k on
  2026-10-10). Nothing more: whether a copy of it makes money is round 2's question. Saved
  (`selection`) so a restart keeps it until the next read.
- **Round 2, the trial** (`src/engine.rs`, `verdict`; **off since 2026-10-11**, `TRIALS`: all of round 1 and
  the backtest's picks are copied on paper for a week, none rotated out, to set the paper copies
  against the backtest): every account is copied on paper week by
  week from its enrollment, and at each week's end its PnL over the week (the leaderboard's)
  is set against our copy's:
  - our copy at a profit: its steadiness is measured (`stable::steadiness`), rule B on its perp
    PnL history: a profit in each of the last 3 months, a loss in at most 4 of the last 12
    weeks, $100k+ in perps. Steady: on the golden list. Not: a lucky week, rotated out;
  - it at a profit and our copy not: it cannot be copied, rotated out;
  - both at a loss: another week; a second in a row a third only if it is still on round 1's
    list, else rotated out.

  Rotated out: not followed nor entered for 30 days. Golden accounts are tried every week the
  same way (a profitable week keeps them on). Each week is a row of `trader_weeks`, with rule
  B's measures and how it trades (orders a day, resting share, liquid share, edge) when the
  copy made money: for the analysis.
  Why not rule B up front: on July-September 2026 picking by it (3 months up, 60%+ weeks up)
  did no better the next month than looser rules, nor clearly better than all large accounts;
  whether we can copy an account at a profit is what decides.
- **Backtest picks** (migration `2026-10-11-backtest-picks`, table `backtest_picks`): the 975
  traders our copy of made over 2.5% a month in a backtest of all 9 342 accounts at a profit
  over the month before (their last up to 90 days, our ~1 s latency, slippage measured on our
  copies, taker fees, funding, the 20% stop). Followed and copied on paper always, not tried
  week by week: their paper copies check the backtest (latency, slippage, fees, PnL against
  its expectation). `python tools/paper_vs_backtest.py` (needs `RAILWAY_DATABASE_URL` in `.env`,
  `psycopg2`) prints the comparison.
- **Golden list**: traded for real (see Live). It started (migrations `2026-10-09-golden-seed`,
  `-seed-all`) with 19 accounts put on it by hand; from then on round 2 decides. Golden accounts
  are followed even off round 1's list. The paper copies have no cap on positions held (each
  is its own account); the live account has (50 legs). Their fills (`their_fill`) and set-up
  reads (`plan`) are kept for golden accounts only.
- **How**: its fills are picked out of the public trades stream (every trade names buyer and
  seller), our order is a taker fill on the live L2 book as soon as it reaches us (no delay
  added).
  - When it opens a position (from flat, or flips), we open in its direction at a size whose
    stop loses 2% of our equity: 2% / 20% = a position of 10% of equity.
  - While it adds, we hold that size. As it reduces from its largest size in the position we
    reduce in proportion, and we close when it is flat.
  - A rotated-out account's positions (round 2) are followed out, not into.
  - Our stop: 20% against our average entry (checked every 5 s at the book's mid), closed at
    the book; we stay out of that position until it is flat (`legs` in the state).
- **Enrollment**: on its first fill we read its positions; the one that fill opened is
  followed, the ones it held before are not entered. Its positions are read again every 2 h
  while active (a copy we already hold is resized to our size then).
- **Venue mechanics**: taker fee 0.045%; orders under $10 are not placed unless they close a
  position (the difference waits for its next change); hourly funding at each coin's rate;
  a copy account at zero equity is liquidated and stops. Main perp dex only (no HIP-3
  dexes).

## Allocator

A second layer over the paper copies (`src/alloc.rs`), apart from them: paper portfolios of
$10 000 that get the same signals as the copies (each followed account's position moves) but
decide whom to let in. The copies per account go on as they were. Each portfolio is a test of
its own (`modes`), sharing nothing with the others but the signals. From generation 4 they are
a grid, every combination run (paper costs nothing): 8 x 5 x 3 x 3 = 360 portfolios.

| | values |
|---|---|
| who: a score to rank the eligible by, or at random (3 draws: the benchmark) | expectancy / drawdown `EXPDD`, PnL `PNL`, expectancy x sqrt(trades) `PNLSQRTN`, profit factor `PF`, t-statistic `TSTAT`; `RANDOM1`, `RANDOM2`, `RANDOM3` |
| how many of the top (or at random) | 10, 20, 30, 50, 100 |
| risk an entry (lost at its 20% stop) | 0.5%, 1%, 2% |
| cap on the open risk in one direction | 20%, 50%, 100% (none but the total's) |

Named `EXPDD_20_R0.5_D20`, `RANDOM2_20_R0.5_D20`. A draw picks the same traders at every risk
and cap, so those portfolios differ by their sizing alone; each score's portfolio is judged
against the three draws of its size, risk and cap (the spread of chance, not one draw).

- Each trader's statistics: the backtest's over 90 days, the source of truth (`backtest_stats`, migrations `2026-10-12-backtest-stats` and `-all`: all 6 887
  wallets with copy trades, as the allocator was walked forward on, not only the 975 picks;
  all of them followed, for their signals): closed trades, PnL, expectancy and its spread,
  profit factor, drawdown, slippage, coins entered. The paper copies only calibrate the
  backtester: a copy's own statistics (with holding time, PnL over 7 / 30 days, open risk)
  take the backtest's place only after 90 days copied without a break (`PAPER_DAYS`, since
  its enrollment). A trader with neither is not ranked. (Generations 1-3 let a paper copy of
  20 closed trades replace the backtest: in generation 3, 304 of 530 entries were of three
  traders on a day of paper, two of them scalpers the backtest had at a loss.)
- Eligible for all: PnL above 0 (from paper, on 20+ closed trades). Beyond that each portfolio filters by its
  score only (`Score::min_trades`): a score over the trades' risk (t-statistic: their spread;
  profit factor: the losses; expectancy / drawdown: the drawdown) or their number
  (expectancy x sqrt(trades) is the PnL / sqrt(trades): the fewer, the higher) needs 20
  closed trades for it to be measured; the PnL alone needs one; at random draws from all the
  eligible. Nothing the score itself weighs is filtered again (slippage is in the PnL
  already). Re-ranked once a day. Generation 4 (the portfolios started again at $10 000):
  ranked on the backtest only. Generation 3 (2026-10-12 22:30 UTC on): paper of 20 trades in
  place of the backtest; generation 2 (20:07-22:30): also PNLSQRTN on any number of trades,
  its top traders of 1-5; generation 1: all had 20+ trades, profit factor above 1.1, drawdown
  under 15%, slippage under 30 bp.
- An entry gets its portfolio's risk (the 20% stop loses that much of the portfolio), cut to
  the limits: open risk in its direction (its cap: the traders mostly long together, so a
  portfolio is mostly one way; walked forward at 1% an entry, the real drawdown with open
  positions was 50-60% with no cap, ~20% at 30% in all), in all 100%, per trader 20%, at most 2
  positions in a coin; under an eighth of its risk it is refused. A position is followed to its end (reduced as
  the trader reduces, closed when it is flat) or the stop: a trader dropping off the list stops
  new entries only.
- `allocator_decisions` (migration `2026-10-12-allocator`): each entry of a picked trader per
  portfolio, with rank, score, requested and granted risk and why it was cut or refused
  (`COIN_POSITIONS_CAP`, `DIRECTION_RISK_CAP`, `TOTAL_RISK_CAP`, `TRADER_RISK_CAP`, `TOO_SMALL`). `allocator_trades`
  (migration `2026-10-12-allocator-trades`, from generation 2): each position closed, with its
  own entry and exit price (not the coin's legs' average), PnL net of its fees, R (PnL / the
  risk granted in USD), hours held and why (`TRADER_CLOSED`, `TRADER_FLIPPED`, `STOP`). Both
  carry the `generation`. The portfolios' state in `docs` (`allocator`, saved every 30 min and
  at shutdown: a few MB), their equity, return, open risk (and of it long), entries of traders not picked (`not_picked`: counted only; until generation 3 a
  `NOT_PICKED` row each, 99% of the rows) in `bot_status.data.allocators`; each ranking an event `allocator_rank`. No
  funding on the portfolios (the copies have it).
- Report: `python tools/alloc_report.py [--generation N] [--risk 0.5] [--cap 20] [--detail
  NAME] [--traders NAME]` (read-only, `RAILWAY_DATABASE_URL` in `.env`): a block per risk and
  cap, each score's return and drawdown at each size with its verdict, the random draws' range;
  one portfolio's numbers, its PnL by trader. `python tools/alloc_audit.py`: an independent
  check of the portfolios from the database (filters, caps, sizing, stops, PnL, the books).
- The criterion, fixed before the paper: after 21 days of a generation, a portfolio works if
  its return is above all three random draws' of its size, risk and cap, and its closed
  trades' mean R is above theirs in 80%+ of bootstrap draws, with 30+ closed trades each; else
  it is no better than picking at random (before: too early).
- Walked forward on the backtest (research 2026-10-12: all 6 887 wallets with copy trades,
  ranked every day on the trips closed before it, days 30-90 traded): EXPDD_30 +109% (max
  drawdown 15%), PNLSQRTN_30 +99% (19%), PF_50 +69% (22%), EXPDD_10 +55% (13%), against
  +3% / +24% / +23% for 10 / 30 / 50 at random (30 draws; drawdown 22-35%); PNL_10 -1% (41%),
  TSTAT_30 +5% (33%). At up to 5x on a portfolio, in two months good for longs: the paper
  decides. With each score's own filter (generation 2, same walk; PNLSQRTN on 1+ trades, 20+
  from generation 3: +145% / +40% for 30 / 10): PNLSQRTN_30 +164% (21%),
  PNL_30 +129% (28%), EXPDD_30 +111% (22%), PNLSQRTN_10 +74% (18%), EXPDD_50 +73% (25%), PF_50
  +61% (24%), EXPDD_10 +55% (11%), PF_30 +51% (18%), TSTAT_30 +36% (28%), PNL_10 +34% (36%),
  against +1% / +29% / +41% at random from all the eligible (median of 10 draws).

## Live

With `LIVE_ENABLED=1`, `LIVE_ACCOUNT` (the account's address) and `LIVE_KEY` (its private key,
or an API wallet's approved for it) set, the golden list is also traded for real on that account
(off by default; it was on the testnet 2026-10-09..11, switched off: testnet's books are nothing
like mainnet's, so it showed the mechanics only):
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
  - `their_fill`: every fill of a golden account as it reached us (size signed, price,
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
- `allocator_decisions`, `allocator_trades`: the allocator portfolios' decisions and closed
  positions (above).
- `backtest_stats`: the backtest's statistics of its picks (the allocator's start).
- `backtest_picks`: the backtest's picks and what it expects of their copies (above).
- `trader_weeks`: round 2, each account's weeks: its PnL and our copy's, the verdict (golden /
  again / out) and why (migration `2026-10-10-trader-weeks`).
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
