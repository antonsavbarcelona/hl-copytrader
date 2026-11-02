# Venue audit (deliverable 1) — checked live 2026-11-03

All calls are public and read-only (no keys). "Live" = called by `dexarb audit` / `dexarb scan` and parsed into the
model; the numbers below are from the run of 2026-11-03 ~00:15 UTC (BTC). Regenerate: `dexarb audit` (writes data/audit.json).

| venue | API base (used) | perps (active) | book | funding now | funding history | funding model | fees | status |
|---|---|---|---|---|---|---|---|---|
| Hyperliquid | api.hyperliquid.xyz/info (POST) | 179 (179) | l2Book, 20 levels | metaAndAssetCtxs `funding` | fundingHistory (500/call, paged) | hourly, fraction, + longs pay, cap 4%/h | docs: base tier 0.045% / 0.015% | live |
| Lighter | mainnet.zklighter.elliot.ai/api/v1 | 235 (213) | orderBookOrders (orders aggregated by price) | funding-rates (exchange=lighter, fraction / 8 h) | fundings (percent / h + `direction`; verified: "short" = shorts pay) | hourly | API: 0% / 0% (standard account) | live |
| Extended | api.starknet.extended.exchange/api/v1 | 416 (339) | info/markets/{m}/orderbook (full book) | markets `marketStats.fundingRate` | info/{m}/funding (paged by 7 d) | hourly, fraction | docs: 0.025% / 0% (account fees need a key) | live |
| Aster | fapi.asterdex.com/fapi/v1 | 638 (599) | depth (100) | premiumIndex `lastFundingRate` | fundingRate (1000/call) | per symbol 1/4/8 h (fundingInfo), fraction, cap ±2%/interval | maker 0% docs; taker 0.04% third-party (verify) | live |
| GRVT | market-data.grvt.io/full/v1 (POST) | 184 (184) | book (100) | ticker `funding_rate_8h_curr` (percent / 8 h) | funding (percent / interval, ns times) | per instrument 1/4/8 h | docs level 1: 0.05% / 0.03% (reviews quote rebates) | live |
| ApeX Omni | omni.apex.exchange/api/v3 | 139 (88) | depth (100) | ticker `fundingRate` + `predictedFundingRate` | history-funding (100/call, paged) | hourly, fraction | reviews: 0.05% / 0.02% (verify) | live |
| edgeX | edgex-prod-v2.edgex.exchange/api/v2/public (v1 host = the web app) | 193 (156) | quote/getDepth (level 200) | funding/getLatestFundingRate (+ forecast) | funding/getFundingRatePage (paged) | per contract `fundingRateIntervalMin` (240 = 4 h), cap per interval | API: contract defaults (BTC 0.045% / 0.04%) | live (slowest: 14 s for the audit) |
| Pacifica | api.pacifica.fi/api/v1 | 79 (79) | book (10 levels a side) | info/prices `funding` + `next_funding` | funding_rate/history (cursor) | hourly, fraction | docs mirror / reviews: 0.04% / 0.015% (verify) | live, shallow book |
| StandX | perps.standx.com/api | 13 (13) | query_depth_book | query_funding_rates (the last hour) | query_funding_rates (start/end) | hourly, fraction, cap ±0.0469%/h | API: 0.04% / 0.01% | live; quote / collateral DUSD; BTC book ~200 bp wide on the run |
| GMX v2 | arbitrum-api.gmxinfra.io | 103 indexes (deepest pool each) | none: oracle min / max price as one level, size = the pool's available liquidity | markets/info `fundingRate*` + `borrowingRate*` per side (1e30, read as annual, + = pays: inferred) | **none in REST** (on-chain events only) | continuous, side-specific, + borrowing | third-party: position fee 0.05% / 0.07% + price impact at the close (not modelled) | live, estimates incomplete |

## Limitations (not invented, not filled in)

* **Account data** (balances, free collateral, the account's real fee tier, deposits / withdrawals and their status) needs
  each venue's keys or signatures: not implemented (read-only research). The treasury model uses documented routes and an
  explicit amortised estimate ($2 a transfer a venue, 1 deposit + 1 withdrawal per venue per 20 round trips); replace it
  with the account's figures before any live use.
* **Fees** marked "docs" / "reviews" are the public base tier; Aster's taker, ApeX and Pacifica are from third-party
  pages and must be confirmed per account.
* **GMX**: no historical funding in REST; the close's price impact needs the on-chain impact factors; the funding units
  were inferred (annual fraction) - flagged on every GMX row.
* **StandX**: DUSD collateral (its conversion cost and peg are not priced); only 13 markets.
* **Historical books**: none of the ten publishes them (level D: our own collector, `dexarb collect`, from 2026-11-03).
  Funding history: 9 of 10 (level C).
* **WebSocket** streams are not used (REST snapshots every 60 s); the latency assumption is the REST round trip (~0.3-2 s).

## Deposit / withdrawal routes (from the venues' docs, not verified through the API)

| venue | collateral | routes (docs) | notes |
|---|---|---|---|
| Hyperliquid | USDC | Arbitrum bridge in; withdrawal to Arbitrum (1 USDC fee) | ~minutes |
| Lighter | USDC | Ethereum L1 / supported L2s via its bridge | fast vs secure withdrawals |
| Extended | USD (USDC) | Starknet; multi-chain via its bridge | |
| Aster | USDT / USDC / USD1 | BNB Chain, Arbitrum, Ethereum, Solana | |
| GRVT | USDT | Ethereum / Arbitrum / ZKsync routes | |
| ApeX Omni | USDT | Ethereum, Arbitrum, Base, BNB, Mantle and more | |
| edgeX | USDC / USDT | Ethereum, Arbitrum, BNB and more | |
| Pacifica | USDC | Solana | |
| StandX | DUSD | minted from USDT / USDC | the conversion is a cost |
| GMX | wallet tokens | Arbitrum / Avalanche on-chain; gas + an execution fee each order | |
