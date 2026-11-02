# dexarb — DEX funding & basis arbitrage research engine

Read-only: no keys, no orders, no transfers. Separate crate (own `[workspace]`): the live bot is not touched and does not
build it. Data goes to `$DEXARB_DATA` (default `./data`; the runs of 2026-11-03 used `D:/dexarb_data`).

```
cargo build --release
dexarb audit                                   # deliverable 1: the ten venues' APIs called live -> data/audit.json (AUDIT.md)
dexarb scan --assets 40 --lev 3                # deliverable 2: the ranked opportunities -> data/scan_latest.json
dexarb calc BTC Lighter Hyperliquid 1000 24 3  # deliverable 3: one pair's net PnL, decomposed, 3 scenarios, stress
dexarb ladder BTC Lighter Hyperliquid 1000 0.05 0.1 0.2 0.4 --exit 0   # deliverable 4: the averaging ladder vs one entry
dexarb history --days 30 --assets 40           # deliverable 5 (level C): funding history -> data/funding/
dexarb research                                # deliverables 5 / 7: the studies -> data/research.md
dexarb collect --every 60 --assets 25          # level D collector + the paper engine (deliverable 6) -> data/collect/, data/paper*.json
```

## Modules (the ТЗ's architecture, one crate)

| ТЗ module | here |
|---|---|
| venue_adapters | `src/venues/*.rs` (Hyperliquid, Lighter, Extended, Aster, GRVT, ApeX, edgeX, Pacifica, StandX, GMX) |
| instrument_registry | `registry.rs` (normalised base + multiplier, the price check against ticker collisions) |
| market_data_collector | `collect.rs` |
| funding_engine | each adapter's normalisation (fraction / h, + = longs pay) + `scanner::hourly_net` + `pnl::scenarios_from` |
| execution_estimator | `exec.rs` (book walks: VWAP, impact, completeness; never the mid as a fill) |
| basis_engine | `pnl.rs` (entry / exit basis on the walked books), `research.rs` level D |
| opportunity_scanner | `scanner.rs` + `main.rs scan` |
| strategy / averaging | `paper.rs` (model B), `averaging.rs` (the ladder vs one entry) |
| risk_engine | `pnl.rs` (capital on both legs + reserve, a leg's liquidation distance, the basis stress), `scanner::risk_score` |
| treasury_engine | an explicit amortised estimate (`scanner::treasury_per_trade`); the routes in AUDIT.md (account APIs need keys) |
| historical_replay | `research.rs` (strategy A walked forward, A-long, D on the snapshots) |
| paper_engine | `paper.rs` (DISCOVERED -> ENTERED -> HEDGED -> CLOSED, data/paper_log.jsonl) |
| reporting | `data/research.md`, `REPORT.md` |

Conventions: prices / sizes `rust_decimal`; funding everywhere a fraction per hour, + = the longs pay; a figure a venue
does not publish is `None` / "unavailable", never filled in.
