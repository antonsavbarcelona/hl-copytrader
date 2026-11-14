# hl-copytrader

A trading bot on Hyperliquid: a family of strategies on one account, sized from one deposit with one open-risk cap
(`src/trades.rs`), each strategy in its own module with its research notes at the top:

| module | what it trades |
|---|---|
| `dip.rs` | a liquid alt's own sharp drop (and the pump short), held 1-4 h |
| `hip3.rs`, `wkd.rs` | the HIP-3 (stock) perps' dips, weekends and US shut hours |
| `wick.rs` | resting bids / asks under and over the price (the wick catch) |
| `rsidip.rs` | the 5-minute RSI hook far under the EMA200, with adds |
| `listing.rs` | the new listings |
| `trend.rs` | the daily trend breakout |
| `expiry.rs` | the monthly-expiry short of BTC / ETH |
| `squeeze.rs` | the 4h Bollinger squeeze breakout, long and short |
| `breakout.rs` | the 7-day breakout after a quiet week, long and short |

Around them: `venue.rs` (orders and the account), `exchange.rs` (signing, the exchange API), `api.rs` (the info API,
paced), `ws.rs` (the books), `store.rs` (events and states: Postgres or files), `notify.rs` (Telegram),
`clusters.rs` (the crash cap's coin clusters), `walls.rs` / `micro.rs` (order-book recorders, no trades).

```
cargo build --release
target/release/hl-copytrader run --data data [--weight 400] [--run main]
```

Env: `LIVE_KEY`, `LIVE_ACCOUNT`, `LIVE_NET=mainnet` (the account; nothing is traded without them), `DATABASE_URL`
(Postgres; files in `--data` without it), `RUN_ID`, `TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID`. Every strategy is on by
default (`<NAME>_ENABLED=0` turns one off; set empty: a trial of its first 3 trades); the settings live in the code.

Deploy: Railway builds the `Dockerfile` (`start.sh`), one replica. Tests: `cargo test` (the end-to-end ones run the
order code against a fake Hyperliquid, `src/testkit.rs`); coverage: `cargo llvm-cov`.
