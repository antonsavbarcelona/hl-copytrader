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
a grid, every combination run (paper costs nothing): 22 x 5 x 3 x 3 x 2 = 1 980 portfolios.

| | values |
|---|---|
| who: a score to rank the eligible by, or at random (10 draws: the benchmark) | over the trades: expectancy / drawdown `EXPDD`, PnL `PNL`, expectancy x sqrt(trades) `PNLSQRTN`, profit factor `PF`, t-statistic `TSTAT`, expectancy less 2 standard errors `LCB`; over time: daily `SHARPE`, `SORTINO`, return a month / max drawdown `CALMAR`, PnL of the last 30 days `PNL30`, share of weeks in profit `WEEKS`; `COMBO`, the mean percentile over all of them; `RANDOM1`..`RANDOM10` |
| how many of the top (or at random) | 10, 20, 30, 50, 100 |
| risk an entry (lost at its 20% stop) | 0.5%, 1%, 2% |
| cap on the open risk in one direction | 20%, 50%, 100% (none but the total's) |
| the trader's adds | not followed; followed (`_ADD`) up to 2x the entry (as much risk) |

Added 2026-10-16 (alongside, the others kept): COMBO3 (in the top 40 of 2+ of LCB, EXPDD,
PNLSQRTN, by mean place; its 90) and the breaker family `_BRK` (LCB, PNLSQRTN, EXPDD, PNL,
PNL30, COMBO, COMBO3 and the 10 draws x 10/20/30 traders x 0.5/1/2% x 20/50% in one
direction, no adds: 306): after 5 losing positions of a trader in a row, no new entries of
its for 3 days. Walked forward (days 10-90) it lifted the return in 10 of 12 score x sizing
cells (+10 pp on average, -7 to +29) and cut the real drawdown in most (-3.6 pp), the draws'
hardly (+0.3 pp): it works on a selection. 2 466 portfolios in all; one added later is
ranked on its own window (its return since its first status against the draws' over the
same hours).

Named `EXPDD_20_R0.5_D20`, `RANDOM7_20_R0.5_D20_ADD`, `LCB_20_R1_D50_BRK`. A draw picks the same traders at every
sizing, so those portfolios differ by their sizing alone; each score's portfolio is ranked
among the ten draws of its traders and sizing (the spread of chance, not one draw).

- Each trader's statistics: the backtest's over 90 days, the source of truth (`backtest_stats`,
  migrations `2026-10-12-backtest-stats`, `-all`: all 6 887 wallets with copy trades, as the
  allocator was walked forward on; all of them followed, for their signals; `-time` from
  generation 4: daily Sharpe / Sortino, Calmar, PnL of the last 30 days, weeks in profit, from
  the same trips as of the backtest's end): closed trades, PnL, expectancy and its spread,
  profit factor, drawdown, slippage, coins entered. The paper copies only calibrate the
  backtester: a copy's own statistics take the backtest's place only after 90 days copied
  without a break (`PAPER_DAYS`, since its enrollment). A trader with neither is not ranked.
  (Generations 1-3 let a paper copy of 20 closed trades replace the backtest: in generation 3,
  304 of 530 entries were of three traders on a day of paper, two of them scalpers the
  backtest had at a loss.)
- Eligible for all: PnL above 0 (from paper, on 20+ closed trades). Beyond that each portfolio
  filters by its score only (`Score::min_trades`): a score over the trades' risk, their number
  or their time needs 20 closed trades for it to be measured; a PnL sum (`PNL`, `PNL30`) one;
  at random draws from all the eligible. Nothing the score itself weighs is filtered again
  (slippage is in the PnL already). Re-ranked once a day and at every start (the picks are not
  saved: the statistics are fixed and the draws seeded by the day, so a start picks the same).
  Generation 4 (the portfolios started again at $10 000): the grid, ranked on the backtest
  only. Generation 3 (2026-10-12 22:30 UTC on): paper of 20 trades in place of the backtest;
  generation 2 (20:07-22:30): also PNLSQRTN on any number of trades, its top traders of 1-5;
  generation 1: all had 20+ trades, profit factor above 1.1, drawdown under 15%, slippage
  under 30 bp.
- An entry gets its portfolio's risk (the 20% stop loses that much of the portfolio), cut to
  the limits: open risk in its direction (its cap: the traders mostly long together, so a
  portfolio is mostly one way; walked forward at 1% an entry, the real drawdown with open
  positions was 50-60% with no cap, ~20% at 30% in all), in all 100%, per trader 20%, at most 2
  positions in a coin; under an eighth of its risk it is refused. A position is followed to
  its end (reduced as the trader reduces, closed when it is flat) or the stop: a trader
  dropping off the list stops new entries only. With adds (`_ADD`): as the trader adds, so
  does it, up to 2x its entry, within the caps; its stop 20% from its average entry; reduced
  from its size at the trader's peak. (On the backtest the added money earned 51 bp a dollar
  against 32 bp for the entries; in the portfolios walked forward it raised the drawdown more
  than the return: the paper decides.)
- `allocator_decisions` (migration `2026-10-12-allocator`): each entry of a picked trader that
  a cap cut or refused (from generation 4; before, every entry), with rank, score, requested
  and granted risk and why (`COIN_POSITIONS_CAP`, `DIRECTION_RISK_CAP`, `TOTAL_RISK_CAP`,
  `TRADER_RISK_CAP`, `TOO_SMALL`). `allocator_trades` (migrations `2026-10-12-allocator-trades`,
  `2026-10-13-allocator-status`): each position closed, with its own entry and exit price (not
  the coin's legs' average), PnL net of its fees, R (PnL / the risk granted in USD), the most
  it grew with adds (`max_x`), hours held and why (`TRADER_CLOSED`, `TRADER_FLIPPED`, `STOP`).
  `allocator_status` (from generation 4): each portfolio's equity, return, open risk (and of it
  long), entries, refusals, stops, entries of traders not picked, every hour. All carry the
  `generation`. The state in `docs` (`allocator`, saved every hour and at shutdown: some MB);
  the portfolios in all in `bot_status.data.allocators`; each ranking an event
  `allocator_rank`. No funding on the portfolios (the copies have it).
- Report: `python tools/alloc_report.py [--generation N] [--score NAME] [--detail NAME]
  [--traders NAME]` (read-only, `RAILWAY_DATABASE_URL` in `.env`): each score's mean rank among
  the random draws at each size and its verdict, each sizing's return and drawdown; a score's
  portfolios; one portfolio, its PnL by trader. `python tools/alloc_audit.py`: an independent
  check of the portfolios from the database (filters, caps, sizing, stops, PnL, the books).
- The criterion, fixed before the paper: a score is judged on all its 90 portfolios, never on
  one (among 990 some beat chance by chance). Each is ranked by return among the ten random
  draws of its traders and sizing (0-10; at random, 5 on average). After 21 days of a
  generation a score works if its mean rank is 7.5+ and above 5 at 4 of its 5 sizes; else it
  is no better than picking at random (before: too early).
- Walked forward on the backtest (research 2026-10-12/13: all 6 887 wallets with copy trades,
  ranked every day on the trips closed before it, days 10-90 traded; one period, good for
  longs; the paper decides). Rank among 10 draws, mean over 10 / 20 / 30 / 50 / 100 traders, at
  0.5% an entry and 20% in one direction: PNL 7.0, COMBO 6.8, PNL30 6.8, LCB 6.6, EXPDD 6.4,
  PNLSQRTN 5.8, TSTAT 4.4, PF 4.2, WEEKS 3.6, SORTINO 1.8, CALMAR 1.4, SHARPE 0.6 (at 10-30
  traders the first five beat all ten draws; at 100 none beat any: the cap fills with whoever
  comes first); at 1% and no cap: PNL30 10.0, COMBO 10.0, PNL 9.8, LCB 9.8, PNLSQRTN 8.2,
  EXPDD 7.6, WEEKS 7.2, TSTAT 5.2, PF 4.4, SORTINO 4.4, CALMAR 3.8, SHARPE 3.0. Returns over
  80 days at 0.5% / 20%: the best +25..+35% at a real drawdown (open positions) of 10-15%,
  against +7% for 20 at random; at 1% with no cap +50..+113% but a real drawdown of 30-55%.

## Rolling backtest

The allocator ranks on the backtest, and the backtest moves a day every day (`src/rollstats.rs`,
`rollstats/`): its 90-day window ends at today 00:00 UTC, so the scores see the traders' last
90 days and the portfolios rotate as they did walked forward (with the original backtest
fixed at 2026-10-10 they never did: the same picks every day). From 2026-10-16.

- The bot runs `rollstats/daily.py` (Python: the research's model as it was, `sim.py`,
  `metrics.py`, its data in `model.json`) 30 min after a start (its own start first: the two at once on one IP's API limit kept
  the bot from coming up, 2026-10-16) when today's statistics are not in yet,
  and every day at 00:05 UTC; a run that did not bring them is retried in an hour. Its output
  goes to the bot's log (`rollstats: ...`). Off without a database or with `ROLLSTATS=0`.
- Each trader's fills are in `trader_fills` (gzip JSON, the last 100 days; migration
  `2026-10-16-rollstats`), topped up each run with the fills since its last (incremental and
  saved as it goes: a run cut short by a redeploy resumes). Its API weight is
  `ROLL_API_PER_MIN` (default 250 a minute, beside the bot's ~110-330: ~11 h a run for 9 342
  traders, ~25k weight for the coins' candles and funding).
- Each day's statistics are rows of `backtest_stats_daily` (the columns of `backtest_stats`;
  14 days kept). The bot loads the latest day's at a start and when a run brings a new one
  (`Msg::Seed`), and the allocator ranks again at once; without any it uses `backtest_stats`.
- The universe is the research's 9 342 accounts (in profit over the month to 2026-10-10),
  loaded once with `python rollstats/upload.py <research dir>`; refreshing it from the
  leaderboard is a later step. Checked: the window ending at the original backtest's end
  gives backtest_stats again (6 882 of 6 887 rows; 5 differ at its edge, the original
  download having fills after its end), and the job's statistics equal the research code's.

- Sharpe, Sortino and Calmar are over the whole 90-day window (the days before a copy's first
  trip count as flat; from 2026-10-18): from the first trip, a wallet of a few lucky days
  ranked first by them. The slippage means (`model.json`) are the paper's of 2026-10-14..16.
- Every 10 min the latest day's rows are counted: a change (rows written from outside, the
  archive pick below) is loaded at once (`statistics watch: ...`), with rollstats on or off.

## Archive pick

`archive/daily.py`, run each morning on a desktop (`archive/run_daily.ps1`, which reads the
repo's `.env`: `RAILWAY_DATABASE_URL`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`); Railway
only trades. It reads Hyperliquid's S3 archive (every fill of every wallet,
`s3://hl-mainnet-node-data/node_fills_by_block`, requester pays; ~0.7 GB a day, kept extracted
for 120 days in `D:\hl-daily`, ~20 GB), backtests every wallet's copy with rollstats' model
(stops on minute bars from every fill, funding from the archive's asset contexts) and writes the
day's `backtest_stats_daily` rows: the bot's universe (`trader_fills`) and the exchange's 500
best by the copy's Sharpe over 90 days (10+ closed trips, in profit). It replaces rollstats:
run the bot with `ROLLSTATS=0`. Market makers (mostly maker in an hour of 500+ fills) and HFT
(by its decisions, not its fills) are left out. The research walk-forward behind it (2026-10-18,
2025-05 .. 2026-10, the paper as its check): the copy's Sharpe over 90 days was the one score in
profit both on the months it was chosen on and on the months after.

## Live

With `LIVE_ENABLED=1`, `LIVE_ACCOUNT` (the account's address) and `LIVE_KEY` (its private key,
or an API wallet's approved for it) set, the golden list is also traded for real on that account
(off by default; it was on the testnet 2026-10-09..11, switched off: testnet's books are nothing
like mainnet's, so it showed the mechanics only):
testnet (`api.hyperliquid-testnet.xyz`) unless `LIVE_NET=mainnet` (`src/live.rs`).

- `LIVE_PORTFOLIO` (e.g. `SHARPE_20_R1_D50`): the traders that portfolio picks today are entered
  instead of the golden list, with its limits on the account: `LIVE_RISK_PCT` an entry (default
  the paper's 2), open risk at most `LIVE_CAP_TOTAL_PCT` (100) in all, `LIVE_CAP_DIR_PCT` (50) in
  one direction, `LIVE_CAP_TRADER_PCT` (20) of one trader, two legs a coin; with
  `LIVE_MAX_DD_PCT` (off at 0) no entries once the account is that far below its peak (its legs
  are followed out).
- `LIVE_ONLY=1`: only the live portfolio's picks and the accounts the live account holds legs
  of are followed, nothing on paper beyond them (no trial, no selection's thousands). The picks
  are enrolled as soon as they are picked (their positions read), not on their first fill: an
  entry in several fills read whole on enrolment was taken for a position held before.
- Orders do not wait for each other across coins: each coin's moves are one job at a time, the
  coins' jobs run at once; the moves that came while a coin's job ran are taken together (each
  account's last); an entry more than 3 s after its fill reached the bot is skipped (`late`).
  The mid comes from the bot's books (the API's mids, read every 5 s, where they have none),
  the equity from a read every 15 s, the minute's account check is read in the background: an
  entry is one request. Measured from a desktop in Europe (2026-10-18): 1.7 s from the trader's
  fill to ours, against 5-48 s before; most of it the round trip to the exchange (Tokyo), so
  the bot trades from Railway's Singapore region.
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

Off since 2026-10-14 (`LIQ_ENABLED=1` to run it again). On 2026-10-09..13 it simulated 214
trades on ~54 touches (14 coins, mostly BTC and ETH): all four strategies lost (average trade
cascade_15m -6 bp, bounce_60m -12, bounce_15m -19, cascade_60m -33; 9 bp of fees a trade);
after a touch the price went +15 bp in 5 min, +18 in 15, -19 in 60 on average, through the
level 58% of the times: no direction to trade. Its scanner had then stopped finding touches
(none after 2026-10-13 07:35), not looked into. The tables are kept.

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
- `allocator_decisions`, `allocator_trades`, `allocator_status`: the allocator portfolios'
  decisions, closed positions and hourly status (above).
- `trader_fills`, `backtest_stats_daily`: the rolling backtest's fills and daily statistics (above).
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
