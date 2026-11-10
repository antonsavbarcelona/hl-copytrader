# Tests

`cargo test` runs everything: no network, no database, no Telegram (`notify::send` is a no-op under test).

## Layers

| Layer | Where | What it proves |
|---|---|---|
| Unit | `#[cfg(test)] mod` in each module | pure rules: signals, sizes, rounding, the family registry, hours, reply parsing |
| End-to-end | `src/e2e_tests.rs` on `src/testkit.rs` | the bot's real order code (`live::Venue`, `trades::*`, the wick's cancel settlement) over real HTTP and real signing against a fake Hyperliquid |

### The fake Hyperliquid (`testkit::FakeHl`)

A local HTTP server answering `/info` (meta, allMids, orderStatus, openOrders, frontendOpenOrders, clearinghouseState,
userFillsByTime, l2Book, perpDexs, userAbstraction) and `/exchange` (order, cancel, updateLeverage) with the API's reply
shapes, and a toy matching engine: the test sets mids (`set_mid`); resting limits fill when the mid crosses them; stop
triggers fire; IOC orders fill at the mid; reduce-only is clipped to the position; positions and fills are kept.

Faults on demand (`f.with(|s| ...)`):

| Knob | The real incident it replays |
|---|---|
| `status_lag`, `lag_next_order` | orderStatus behind a fill (xyz:HIMS 2026-11-09, xyz:SNXX 2026-11-04) |
| `refuse_cross`, `refuse_isolated` | a HIP-3 coin kept isolated (stops past the liquidation, 2026-11-05) |
| `refuse_stops` | a stop not placed: the position must be closed again |
| `ioc_needs` | a thin book: the IOC matches only further through the mid |
| `margin_refusals` | the margin held by the resting bids (2026-11-02) |
| `lose_replies`, `lose_order` | a reply lost after the action took effect (a timeout, a 502) |
| `modify_keeps_oid`, `modify_bare_reply` | the modify's reply shapes the docs leave open |
| `partial_fill`, `hand_trade` | a take filled in part (io:PRL 2026-11-03), a position closed by hand (xyz:RELIANCE) |

Each test uses its own coin names: the family registry (`trades::OPEN`) is process-wide.

## Coverage map (2026-11-09)

Covered end-to-end: opening long / short with the stop and the take resting; the IOC retry; nothing filled; a stop
refused -> closed again; isolated HIP-3 stop cap; the take from the price expected (worse and better fills); the margin
refusal releasing resting bids; post-only crossing; reduce-only with nothing held; a trade ending at the take, the stop,
by hand, a partial take, a lagging take status; closing on time; a held trade not ended; a bid filled just before its
cancel with a lagging status (HIMS), cancelled clean, filled in part, an unknown status; the request limit's top-up
(bought and sold back reduce-only, nothing left open; a held BTC left alone, ETH used; both held: nothing traded); a timed close of a position
already gone (ended at its close price, not retried for ever); a bid moved in one request (`modify`: a new oid or the
same, a bare reply looked up among the open orders; a filled or crossing bid not moved, nothing new resting).

Unit: signing (the SDK's vectors), wire / price / size rounding at the edges, order replies, the margin error, the
after-cancel decision, the family registry (`coin_fits`), sizes and returns, isolated leverage and the liquidation stop,
the take base, the walls' record (`walls::Tracker`: told after its age, touched once per approach, crossed /
pulled / pulled near, kept down to half its bar, its price path an hour from the touch), the wkd / US hours (DST in and out, the weekend, Sunday night), the dex lists, and the strategies' own
signal tests (dip, rsidip, squeeze, trend, wkd, liq, stable, alloc, engine).

Mutation checks run by hand when this was written (each must turn a test red): the HIMS keep -> drop (2 red), the
close-again after a refused stop removed (1 red), the take base always from the expected price (1 red).

## Not covered yet (the next steps)

1. The strategies' run loops (`wick::run`, `hip3::run`, `dip`, `rsidip`, `squeeze`, `trend`, `listing`, `expiry`):
   they need the `Store` (Postgres) and the `api::Api` behind a seam. Next: a `Store` in memory under `cfg(test)`, then
   one loop step each against the fake (a fill -> a trade with its stop and take; an orphan told; a restart picking the
   saved state up).
2. `watch_orphans`: a position no strategy holds is told, a family one is not.
3. The websocket feed (`ws.rs`): reconnects, a gap in the books.
4. `store.rs` round trips of each strategy's state (old saved shapes still load: `serde(default)`).
5. A fake-time harness (`tokio::time::pause`) for the hold timers and the 2 s retries in `trades::gone`.
