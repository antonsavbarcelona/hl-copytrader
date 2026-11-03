# DEX funding & basis arbitrage — research report (2026-11-03, first night)

Everything here comes from the venues' own public APIs (no CEX, no third-party data). Levels as in the ТЗ:
**C** = funding history only (9 of 10 venues publish it, 30 days pulled); **D** = our own book snapshots (the collector
started 2026-11-03 ~00:30 UTC, every 60 s; no venue publishes historical books, so A/B-level replays need weeks of it).

## 1. What works (phases 1-5, 7 partly)

* All ten adapters live (AUDIT.md): instruments, books (GMX: oracle min/max + pool liquidity), current funding, funding
  history (all but GMX), fees (API: Lighter, edgeX, StandX; docs elsewhere). Units and signs normalised and checked:
  BTC's base rate reads ~0.00125 %/h (~11% APR) on every venue; Lighter's `direction` verified on live markets (MNT:
  0.0134 %/h "short" = -0.107 % per 8 h vs the summary endpoint's -0.112 %).
* The matcher: 40 underlyings on 2+ venues after the price check (a mid more than 2% off the venues' median drops out).
* The scanner: ~150k (asset x pair x size x horizon) rows priced on the walked books in ~25 s; the top 40 re-priced on 7
  days of funding history (conservative / base / optimistic), only positive base-scenario nets kept.
* The calculator and the ladder simulator: every term shown (VWAPs, impact, entry / exit basis, fees, funding by
  scenario, treasury, capital on both legs + 50% reserve, break-even, a leg's liquidation distance, the basis x2 / x3 stress).
* The collector + the paper engine run (232 quotes a round from all ten venues).

## 2. Findings (level C, 30 days, 40 assets, 2170 ordered pairs)

* **Funding spreads exist and some are steady** — e.g. ADA LONG Lighter / SHORT edgeX: 100% of hours positive, +14.6%
  APR; ARB LONG Lighter / SHORT Pacifica: 100%, +12.9%; AVAX LONG Lighter / SHORT ApeX: 100%, +12.0% (Lighter's funding
  sits below the others' on many coins). The XRP-on-ApeX pairs show the highest means (+15..23% APR) but from spikes
  (median ~0-3%).
* **Taker execution kills them.** A round trip (both legs' spread + impact + taker fees, in and out) is 0.14-0.6% of a
  leg's notional; a +14% APR spread needs ~4-15 days just to pay it.
* **Strategy A (funding only), walked forward** (enter when the trailing 24 h net >= theta, hold H, taker): every theta x
  H loses on both halves except tiny samples; chosen on half 1 (theta 50% APR, H 168 h, +3.2 bp) -> half 2: 15 trades,
  **-9.9 bp**, win 33%. Always in: -34.7 bp a trade.
* **Strategy A-long (pick the persistent pairs on half 1, hold all of half 2, one round trip)**, taker: **-7 to -27 bp**
  a pair over ~15 days for most selections (only the strictest, 5 and 1 pairs, ~0). The half-1 winners do not keep winning.
* **The maker upper bound** (both legs at maker fees, no spread paid, fills assumed — not a forecast): +17..27 bp a pair
  over 15 days, 88-100% of pairs positive -> ~4-6% a year on the capital at 3x. But picking by half 1 adds little over
  "any pair positive on half 1" (+17 bp): the edge is the average spread, not the selection; and it is carry (the margin
  sits in the hedge for weeks).
* **GMX rows dominate the scanner's top** (LDO / AAVE / ENA / WLD: shorts on GMX receiving 50-195% APR) — but GMX has no
  funding history in REST (base = half the current rate, flagged), its units / sign are inferred, the close's price
  impact is not modelled, and the rates come from pool imbalances that move. **Not confirmable from the data we have.**
* **Basis, level B (30 days of 15-min closes, 8 venues, `dexarb basis`)**: strategy D (enter when the basis is k trailing
  std over its 24 h mean and over the round trip, out at the mean) shows +27..40 bp a trade at the signal bar's close -
  and **-20 bp, win 12-15%, when entered one bar (15 min) later**. The bar-close dislocations are stale closes / the last
  trade's bid-ask bounce, not executable. The ladders (fixed 0.3/0.6/1.0/1.5%, z-score 2/3/4) fill ~1.05 layers on
  average and add nothing over one entry per unit of exposure.
* **Basis, level D (the collector, first ~hour)**: a **persistent quoting offset**: Extended, edgeX and GMX quote every
  asset ~12-16 bp UNDER the other venues (BTC, ETH, NEAR alike, every snapshot) - the "constant premium / discount" the
  ТЗ warns about (a quote-currency / oracle convention, not a dislocation): LONG Extended / SHORT another shows a positive
  executable entry most of the time, but it does not converge, so it is no trade; the trailing-mean rule correctly does not
  fire on it. Real dislocations need the collector's days; the D test there enters a snapshot (~60 s) late, as a REST bot would.

## 3. Honest status vs the ТЗ

| phase | status |
|---|---|
| 1 venue audit | done, live (AUDIT.md) |
| 2 first venues, executable quotes + funding | done for all ten |
| 3 net-PnL calculator | done (`calc`) |
| 4 basis engine + averaging simulator | done on live books (`ladder`); historical ladders need level-D data |
| 5 scanner across venues | done (`scan`); no fictitious adapters |
| 6 historical replay | level C done (A, A-long, maker bound); B/C/D replays wait for the collector's weeks |
| 7 paper | running (model B); the first entry after ~30 min (AAVE LONG Lighter / SHORT Extended), the log in data/paper_log.jsonl |
| 8 live-readiness | NOT ready: account APIs (balances, fee tiers, transfers) need keys; treasury costs are an estimate; GMX impact missing; WebSockets not used |

**Bottom line after night one:** no executable, repeatable cross-DEX inefficiency found yet. With taker execution the
funding arbitrage lost out of sample over the last 30 days; the only positive variant needs maker fills on both legs and
parks the margin for weeks (carry, ~4-6%/yr on the capital at best). The basis seen on bar closes is not executable; the
executable books show a constant offset of three venues, not a converging spread. The collector keeps building the
level-D history; the B / C / D replays rerun on it (`dexarb research`) are the next check.
