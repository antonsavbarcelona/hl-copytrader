//! The wick bids (`WICK_ENABLED`: on unless "0"; with `LIVE_KEY` / `LIVE_ACCOUNT` / `LIVE_NET` as for the dip):
//! a resting buy `WICK_PCT` (7) under each liquid perp's anchor - the last minute's close, or its
//! oracle when lower - on `WICK_MAX_COINS` (30) of the coins with a day volume of `WICK_MIN_VOL_USD`
//! (10M) or more; on the HIP-3 dexes margined in USDC `WICK_H3_PCT` (4) under the last minute's close
//! on `WICK_H3_MAX_COINS` (10; 0: none) with `WICK_H3_MIN_VOL_USD` (0.7M; 1M until 2026-10-28)+ (hyna, the crypto copies, left
//! out). Which coins (every 5 min): the most restless - the biggest standard deviation of the 1-minute
//! returns over the last 24 h (the main dex's and the bid HIP-3 dexes' mids every 5 s, every other
//! HIP-3 dex's every 30 s; after a start 24 h of 1-minute candles read, a coin a minute; a coin with
//! under 6 h of them after the others, by volume); `WICK_PICK=volume`: the biggest day volume. A bid
//! moves down as soon as its level falls `WICK_REPRICE_DOWN_PCT` (0.5; 0.2 until 2026-11-12: V410, HL 1m
//! 180 d, 0.5 vs 0.2 ~44% fewer requests, R crypto flat, HIP-3 -2.4%; the user's call) under it - never resting
//! over the level the research had, or a slow slide fills it (xyz:MRNA 2026-10-27: a bid 3.5% stale,
//! filled 0.5% under the last close) - and up when the level rises `WICK_REPRICE_PCT` (2) over it
//! (the address's rate limit: not every minute); the crypto bids come off while BTC moves 1%+ in a
//! minute. Filled (a wick through it): a resting stop `WICK_STOP_PCT` (20) under and a resting take
//! `WICK_TAKE_PCT` (5; HIP-3 `WICK_H3_TAKE_PCT`, 4) over the fill, out at market `WICK_HOLD_MIN` (60) later; sized so that the
//! stop loses `WICK_RISK_USD` ($20; `FAMILY_RISK_PCT` of the equity when set); at most
//! `WICK_MAX_OPEN` (2) crypto and `WICK_H3_MAX_OPEN` (2) HIP-3 trades and the family's caps (a side's
//! bids off while it is full; all off when the family's open risk is); a coin not bid again for
//! 60 min after its fill; a coin the account or another strategy holds is not bid.
//!
//! The archive research of 2026-10-25/26 (the raw tape; a fill when trades go 10 bp through the
//! bid; maker in, the take maker, else taker out): crypto 8% $10M+ n 188 +328 bp a trade, halves
//! +368 / +275; HIP-3 5% $1M+ n 309 +257 bp, t 7.4, 12 of 12 months up. Resting bids tie up margin
//! (the HIP-3 ones isolated at 3x: a third of each bid's notional).
//! The levels 2026-10-26 (the tape, the bot's trading at $20 risk, 2 slots): crypto 7% 13 trades a month
//! +234 bp, $513 / 16 months (8%: 10, +273 bp, $428; 6% fails its first half); HIP-3 4% 43 a month
//! +158 bp, $747 / 11 months, 12 of 12 months up (5%: 28, +257 bp, $784): more trades, the same $.
//! The pick (2026-10-27, the tape, the 24 h deviation known an hour before the fill): of the HIP-3 4%
//! fills the top 5 by volume held 5% ($40 / 11 months), the top 5 by the deviation 69% ($589), 10 79%
//! ($615); crypto 7%: the top 20 by volume 60% ($328 / 16 months), by the deviation 80% ($434), 30 93%
//! ($475). The HIP-3 take (2026-10-27, the tape, the 4% bid on the USDC dexes, 307 fills, 60 min,
//! stop 20%): 3% +119 bp, 4% +145 bp (hit 58%, halves +182 / +109), 5% +132 bp (hit 38%), 6% +126:
//! 4% sits just under the last close the wick fell from. A bid holds its margin: ~$10 a crypto one (10x), ~$41 a HIP-3 one, at $20 risk.
//! The crash insurance (2026-10-28, V69/V75): no crypto bid while the coin's last hour is already 2.5x as restless as
//! its day (`WICK_HOT_X`): on the tape it skips 82 of the 309 fills, those of 2025-10-10 (-16 stops, all the crypto
//! wick's profit) and on the other days +3 stops; on one deposit it keeps the drawdown with the open at 19.4% at 3.3% a
//! trade (23.2% without it).
//! The HIP-3 volume floor (2026-10-28, V67..V69: the filter matrix and 27 filter kinds on the bars, then the tape):
//! $0.7M against $1M: 246 fills against 217, the win rate 80.1% against 78.8%, the sum +33% (w/o the 5 best days
//! +44%), 12 of 12 months up; no other filter beat shuffled luck (a closer bid: twice the fills, the win rate -7..10 pp,
//! the same sum).
//!
//! The audit's trace (2026-10-28; observing only, the trading as above): every bid placed, moved, cancelled, filled is an
//! event kind `wick_order` (`place` / `cancel` / `fill`: the level, its anchor (the minute before's last mid, the oracle),
//! the mid now, the reason, our clock at the level's calculation, the order's send and the exchange's answer, the
//! quotes' age); `wickwatch` logs the exchange's side (orderUpdates, userFills) and the research's shadow.
//!
//! The shut hours (2026-11-01, V197 .. V201): while the US market is shut - Saturday / Sunday UTC and the US weeknights
//! (Mon-Thu 20:00 .. 04:00 New York time; `wkd`'s clock) - a picked US stock (`wkd::us_stock`) is bid closer:
//! `WICK_H3_WKD_PCT` (2.5) on the weekend, `WICK_H3_NIGHT_PCT` (3) at night; that trade's take = its level, its stop
//! 2.5x it, its hold `WICK_H3_SHUT_HOLD_H` (24) h, at most `WICK_H3_SHUT_MAX_OPEN` (3) of them besides the HIP-3 slots. The tape (HL 2025-12 .. 2026-10, the US stocks of the USDC dexes, a fill
//! only when a print trades under the bid; maker in, the take maker, else taker out): the weekend 2.5% 187 fills +154 bp
//! (t 9.4), 88% up, w/o the 10 best stocks +131; the night 3% 102 fills +167 bp, w/o the 10 best +126; the same bids in
//! the session / the pre-market lose. The top 10 by the deviation (this pick) hold 64% (the weekend) / 43% (the night) of
//! the sum. On the HL portfolio at 2% (3 slots) with this pick's fills (the top 10) x13.4 -> x21.7 at the same drawdown 28%
//! (h1 +9%, h2 +23%; the top 20 x26.8; all the US stocks x32.8). Sunday night in New York too (Mon 00:00 UTC .. 04:00
//! ET, `wkd::us_sunday_night`; V203 .. V209) at the night's level: Sunday evening's on the tape 38 fills +206 bp (t 5.3),
//! the halves +245 / +167, w/o the 5 best +174; Monday 00-04 ET 10 fills +188 bp (the bars); the portfolio with both
//! (this pick) x21.7 -> x25.7 at the same drawdown (h1 +4%, h2 +14%).
//! The ladder (V210 .. V213): at those hours a second bid `WICK_H3_DEEP_PCT` (5) under the same close, its own trade (the
//! registry's `wick2`: a coin holds both), take 5%, stop 12.5%, 24 h, `WICK_H3_DEEP_MAX_OPEN` (3) slots. The tape (a fill
//! only on a print under it): 103 fills +427 bp (t 21), 93% up, the halves +427 / +427, w/o the 5 best +407 (4%: 142
//! +317; 6%: 70 +521); the portfolio (the top 10 pick) x25.7 -> x30.4 at the same drawdown (h1 +0%, h2 +23%); one level at
//! 4% or 5% instead of the two: x19 / x18.
//! The crypto ladder (V223 .. V227): a second crypto bid `WICK_DEEP_PCT` (10) under the shallow one's anchor on the
//! `WICK_DEEP_MAX_COINS` (10) most restless of the picked coins (each resting bid holds margin), its own trade (`wick2`)
//! with the side's exits (take 5%, stop 20%, 60 min), `WICK_DEEP_MAX_OPEN` (3) slots, the same BTC / hot checks. The tape
//! (HL 2025-05 .. 2026-10, a fill only on a print under the bid): 73 fills +385 bp (t 6.2), 96% up, the halves +424 / +347,
//! w/o the 5 best coins +344 (12% take 6%: 38 +525); the top 10 / 15 coins keep 74% / 96% of the sum; Binance 1m 2020-26
//! (the bars): 178 fills +215 bp (t 4.0), 6 of 7 years up (the 7% bid +151); the HL portfolio (the bot's set incl. the
//! HIP-3 ladder, 2%) with 3 slots x30.4 -> x36.8 at the same drawdown (h1 +25%, h2 +23%).
//! Its third rung (V234): `WICK_DEEP3_PCT` (12) under the same anchor on the same coins, take `WICK_DEEP3_TAKE_PCT` (6),
//! stop 20%, 60 min, `WICK_DEEP3_MAX_OPEN` (3) slots, the registry's `wick3` (exempt from the per-coin limit: a coin the 7%
//! and 10% rungs hold takes it). The tape: the top 10 coins 33 fills +515 bp; Binance 1m 2020-26 78 +271 bp (t 3.1), w/o the
//! 5 best coins +194, every year up; the HL portfolio over the bot with the 10% rung (x35.7) x41.7 at the same drawdown
//! (h1 +24%, h2 +8%; 15% take 7.5% instead: x39.5).
//! The HIP-3 session ladder (V250 .. V255): while the US market is open (or in its pre / post hours) the picked US stocks get a second bid
//! `WICK_H3_SESSION_DEEP_PCT` (7) under the minute before's close (the ladder's slots), take = its level, stop 20%, 60 min. The tape: all
//! the US stocks 117 fills +432 bp (t 9.8), 82% up, the halves +351 / +511, w/o the 5 best +391 (6%: 157 +329); the 4% -> 7% fall takes
//! < 1 s in 2/3 of them (the bid must rest); the HL portfolio over the bot (x41.7, 2%) with the top 10 x48.5 at the same drawdown (h2's
//! drawdown 17.3% -> 15.7%).
//! The asks (V248 .. V258), the wick's mirror: a resting SELL `WICK_ASK_PCT` (5) over the anchor (the close, or the oracle when higher) on the
//! `WICK_ASK_MAX_COINS` (5) most restless picked coins, short, take `WICK_ASK_TAKE_PCT` (3) under, stop `WICK_ASK_STOP_PCT` (10) over,
//! `WICK_ASK_HOLD_MIN` (240), `WICK_ASK_MAX_OPEN` (2) slots, the registry's `wicka`, the same BTC / hot checks; moved up as soon as its
//! level rises `WICK_REPRICE_DOWN_PCT` (0.5%; a stale ask under the level fills on a slow climb). The tape (HL 2025-05 .. 2026-10, a fill only on a print over it):
//! the 30 coins 308 fills +100 bp (t 3.9), 82% up, the halves +94 / +105, w/o the 10 best coins +52; the top 5 hold 264 of them (+103);
//! the spikes Binance did not share (its high < +2% that minute) +265 bp, 98% up; those it shared (+4%+) +67: an HL book's hole, which
//! is why Binance 2020-26 shows ~0. The HL portfolio (2%, the top 5) x41.7 -> x69.7, the drawdown 26.1% -> 22.8% (a short side: it
//! hedges the long book); with the HIP-3 session ladder x81.1.
//! The HIP-3 asks (V263 .. V267): on the picked HIP-3 coins a resting SELL over the minute before's close, short, stop 20%, take = its level:
//! a US stock while the US market is shut `WICK_H3_ASK_SHUT_PCT` (4, 240 min), while open `WICK_H3_ASK_OPEN_PCT` (7, 60 min); an Asian
//! stock while the US market is open `WICK_H3_ASIA_ASK_PCT` (3, 60 min); `WICK_H3_ASK_MAX_OPEN` (3) slots. The tape (HL 2025-10 .. 2026-10,
//! the USDC dexes): the US shut 4% 185 fills +286 bp (t 15.1), 88% up, the halves +247 / +324, w/o the 5 best +276 (53 stocks); the US open
//! 7% 111 +361 (t 7.6); the Asian 3% 160 +124 (t 6.7); the stop 20% as good as 10% (+238 vs +231 on the bars). And the non-equity bids
//! (the commodities / FX / the crypto-linked names) while the US market is shut: `WICK_H3_NONEQ_SHUT_PCT` (3) under, take 3%, stop 20%, 240
//! min, the shut-hours slots; the tape 283 fills +170 bp (t 9.4), the halves +178 / +162. The HL portfolio at 2% over the bot (x81.1): the
//! US shut asks (the top 10) x133 at the same drawdown 22.8% (h2 x15.0 -> x22.7); + the non-equity bids x200 (h2's drawdown 15.6 -> 14.6%);
//! + the Asian asks x194; + the US open asks x188.
//! The majors' bids (V273 .. V276): ETH / SOL / XRP / BNB / DOGE (`MAJORS`; seldom among the most restless picked) bid `WICK_MAJ_PCT` (4) under
//! the crypto anchor, take = its level, stop 20%, `WICK_MAJ_HOLD_MIN` (240), `WICK_MAJ_MAX_OPEN` (2) slots, the BTC / hot checks, the registry's
//! `wickm`. Binance 1m 2020-26 (the bars, the hot filter): 251 fills +120 bp (t 3.1), 82% up, every one of the 7 years up (3%: 566 +98, 2025
//! -16); HL 2025-05 .. 2026-10 (the bars) 18 fills +303 bp, 94% up, both years up; the Binance portfolio (the bot's set, 2%) at the same
//! drawdown 2020-23 +15..22%, 2023-26 +15%. The first crypto rule of the round that both venues and six years agree on.
//! The low-volume bids (V280 .. V283): the `WICK_LOWVOL_MAX_COINS` (20) most restless of the coins under the $10M floor (a day volume of
//! `WICK_LOWVOL_MIN_VOL_USD` $1M+; thinner books, more HL-only wicks) bid `WICK_LOWVOL_PCT` (7) under the crypto anchor, the crypto exits (take
//! 5%, stop 20%, 60 min), `WICK_LOWVOL_MAX_OPEN` (3) slots, the BTC / hot checks, the registry's `wickl`. The tape (HL 2025-05 .. 2026-10): the
//! $3-10M band 177 fills +339 bp (t 9.8), 89% up, the halves +457 / +223, w/o the 10 best coins +283 (92 coins); the $1-3M band 200 +273 (t 6.6);
//! Binance 1m (its $10-30M / $3-10M bands) 231 +223 (t 5.8) / 146 +200, every year with fills since 2022 up; the HL portfolio (2%) x410 ->
//! x958 at the drawdown 22.8 -> 21.5% (1%: x22.8 -> x35.1, the drawdown 12.4 -> 14.3%). Their ladder (V284): a second bid `WICK_LOWVOL_DEEP_PCT`
//! (12) under on the same coins, the same exits, its own 3 slots, the registry's `wickl2`; the tape ($1-10M) 87 fills +390 .. +452 bp; Binance
//! thin (its $10-30M band 30 fills +243, t 1.8); the HL portfolio (2%) x850 -> x1121 at the drawdown 22.3 -> 21.2% (1%: x33.0 -> x38.1, 14.3 -> 15.2%).
//!
//! Its state is the store's doc `wick` (the bids resting, the trades, the coins' last fills).

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{Api, now, num};
use crate::config::Config;
use crate::dip::Closes;
use crate::live::{Placed, Venue};
use crate::log;
use crate::store::Store;
use crate::trades::{self, Trade};

const DOC: &str = "wick";
/// The ladder's trades in the family's registry: a strategy of their own, so a coin holds a shallow and a deep one.
const DEEP: &str = "wick2";
/// The registry's kinds of the rungs: the shallow bid, the ladder's second and third, the asks (short).
const KINDS: [&str; 7] = ["wick", DEEP, trades::RUNG3, "wicka", "wickm", "wickl", "wickl2"];
/// The asks' rung; the majors' bids' rung; the low-volume bids' rung.
const ASK: usize = 3;
const MAJ: usize = 4;
const LOW: usize = 5;
const LOW2: usize = 6;
/// The majors bid on their own (BTC is the calm filter's reference).
const MAJORS: [&str; 5] = ["ETH", "SOL", "XRP", "BNB", "DOGE"];

/// The main HIP-3 bids' risk a trade, % of the equity (the user's call 2026-11-11): the shallow `WICK_H3_PCT` bid while
/// the US market trades (rung 0, a HIP-3 coin, not a shut-hours bid); every other rung keeps the family's
/// (`trades::family_risk_pct`, 2%). V404 (the V75 portfolio, 2025-05 .. 2026-10, HIP-3 cross 10x, the 40% cap): all at 2%
/// x6.3, the drawdown 14.3%; this one at 5% x13.4 at the same 14.3% (h2 x2.40 -> x4.80, its drawdown 6.3 -> 10.1%); all at
/// 3% x15.2 with 21%. Its 2 slots at 5% + the 6 shut / ladder slots at 2%: 22% at most, not 40%. With the family's sizing
/// off (the $ test): its $ x2.5.
const H3_MAIN_RISK_PCT: f64 = 5.0;

/// A rung's risk $ a trade: `H3_MAIN_RISK_PCT` for the main HIP-3 bid, else the family's.
fn rung_risk(coin: &str, rung: usize, shut: bool, base_usd: f64) -> f64 {
    rung_risk_at(coin, rung, shut, trades::risk_usd(base_usd), trades::family_risk_pct(), trades::equity())
}

/// `rung_risk` with the family's risk $ `base`, its % `pct` and the equity `eq` given.
fn rung_risk_at(coin: &str, rung: usize, shut: bool, base: f64, pct: f64, eq: f64) -> f64 {
    if rung != 0 || shut || !coin.contains(':') {
        return base;
    }
    if pct > 0.0 && eq > 0.0 { H3_MAIN_RISK_PCT / 100.0 * eq } else { base * H3_MAIN_RISK_PCT / 2.0 }
}
const POLL_S: f64 = 5.0;
const ORDERS_EVERY_S: f64 = 15.0;
/// The account's positions against the strategies' trades: every minute; one no strategy holds for `ORPHAN_AFTER_S`
/// (a fresh trade registers within seconds) is told on Telegram, again every `ORPHAN_AGAIN_S` while it stays; not in
/// the first `ORPHAN_BOOT_S` (the modules registering their trades as they start).
const ORPHANS_EVERY_S: f64 = 60.0;
const ORPHAN_AFTER_S: f64 = 180.0;
const ORPHAN_AGAIN_S: f64 = 6.0 * 3_600.0;
const ORPHAN_BOOT_S: f64 = 300.0;
const VOLUME_EVERY_S: f64 = 300.0;
const ORACLE_EVERY_S: f64 = 60.0;
const AGAIN_AFTER_S: f64 = 3600.0;
const BTC_MAX_MINUTE: f64 = 0.01;
/// The minutes of closes kept a coin (24 h), the fewest returns its deviation is read from (6 h).
const HIST: usize = 1441;
const MIN_HIST: usize = 360;
/// A coin's 24 h of 1-minute candles read at most this often, and every HIP-3 dex's mids (the
/// deviation picking from all of them) this often: the API's weight (the HIP-3 dip reads them too).
const BACKFILL_EVERY_S: f64 = 60.0;
const H3_MIDS_EVERY_S: f64 = 30.0;

#[derive(Clone, Debug)]
struct Params {
    pct: f64,
    min_vol: f64,
    max_coins: usize,
    h3_pct: f64,
    h3_min_vol: f64,
    h3_max_coins: usize,
    reprice: f64,
    reprice_down: f64,
    take: f64,
    h3_take: f64,
    stop: f64,
    hold_s: f64,
    risk_usd: f64,
    max_open: usize,
    h3_max_open: usize,
    /// The trial (`WICK_ENABLED` set empty): each side's live trades at most.
    trial: Option<u32>,
    /// The coins picked by the 24 h deviation (else by the day volume).
    by_spread: bool,
    /// `WICK_HOT_X` (2.5; 0: off): no crypto bid while the last hour's 1-minute deviation is over this many times the day's.
    hot_x: f64,
    /// The US stocks' levels while the US market is shut (the weekend, the nights) and those trades' hold.
    h3_wkd_pct: f64,
    h3_night_pct: f64,
    shut_hold_s: f64,
    shut_max_open: usize,
    /// The ladder's second, deeper bid on those stocks while shut (`WICK_H3_DEEP_PCT`, 5; 0: none) and its own slots.
    deep_pct: f64,
    deep_max_open: usize,
    /// The crypto ladder: the second bid's level (`WICK_DEEP_PCT`, 10; 0: none), on how many of the picked coins, its slots.
    cry_deep_pct: f64,
    cry_deep_coins: usize,
    cry_deep_max_open: usize,
    /// The crypto ladder's third rung: its level (`WICK_DEEP3_PCT`, 12; 0: none), its take, its slots.
    cry_deep3_pct: f64,
    cry_deep3_take: f64,
    cry_deep3_max_open: usize,
    /// The HIP-3 session ladder's level (`WICK_H3_SESSION_DEEP_PCT`, 7; 0: none).
    h3_session_deep_pct: f64,
    /// The asks: the level over the anchor (0: none), the coins, the exits, the slots.
    ask_pct: f64,
    ask_coins: usize,
    ask_take: f64,
    ask_stop: f64,
    ask_hold_s: f64,
    ask_max_open: usize,
    /// The HIP-3 asks' levels (0: none) and their slots; the non-equity shut-hours bid.
    h3_ask_shut_pct: f64,
    h3_ask_open_pct: f64,
    asia_ask_pct: f64,
    h3_ask_max_open: usize,
    /// No HIP-3 ask while the coin itself fell this much over the last 24 h (0: off): an ask after a crash fills on the
    /// bounce (2026-11-05 KORU: -30% on the day, the 3% ask filled off the low). HL 1m 2025-26 (night/h3ask_crash.py): the
    /// asks after a 24 h fall of 10%+ -0.2% a fill (14) vs +2.5% the rest (51); the HL portfolio at DD 40% +275% vs +270%.
    h3_ask_crash: f64,
    noneq_shut_pct: f64,
    /// The majors' bids: the level (0: none), the hold, the slots.
    maj_pct: f64,
    maj_hold_s: f64,
    maj_max_open: usize,
    /// The low-volume bids: the level (0: none), the volume floor, the coins, the slots.
    lowvol_pct: f64,
    lowvol_min_vol: f64,
    lowvol_coins: usize,
    lowvol_max_open: usize,
    lowvol_deep_pct: f64,
    /// The margin the resting orders may hold, % of the equity (0: no limit).
    margin_budget_pct: f64,
    /// The thin coins among the low-volume ones: the floor (0: none) and how many (their 7% bid only, the lowest priority).
    thin_min_vol: f64,
    thin_coins: usize,
}

impl Params {
    fn from_env() -> Self {
        let v = |k: &str, d: f64| std::env::var(k).ok().and_then(|x| x.trim().parse().ok()).unwrap_or(d);
        Params { pct: v("WICK_PCT", 7.0) / 100.0, min_vol: v("WICK_MIN_VOL_USD", 10e6), max_coins: v("WICK_MAX_COINS", 30.0) as usize,
            h3_pct: v("WICK_H3_PCT", 4.0) / 100.0, h3_min_vol: v("WICK_H3_MIN_VOL_USD", 0.7e6), h3_max_coins: v("WICK_H3_MAX_COINS", 10.0) as usize,
            reprice: v("WICK_REPRICE_PCT", 2.0) / 100.0, reprice_down: v("WICK_REPRICE_DOWN_PCT", 0.5) / 100.0, take: v("WICK_TAKE_PCT", 5.0) / 100.0, h3_take: v("WICK_H3_TAKE_PCT", 4.0) / 100.0, stop: v("WICK_STOP_PCT", 20.0) / 100.0,
            hold_s: v("WICK_HOLD_MIN", 60.0) * 60.0, risk_usd: v("WICK_RISK_USD", 20.0), max_open: v("WICK_MAX_OPEN", 2.0) as usize,
            h3_max_open: v("WICK_H3_MAX_OPEN", 2.0) as usize, trial: trades::switch("WICK_ENABLED").flatten(),
            by_spread: std::env::var("WICK_PICK").map(|x| x.trim() != "volume").unwrap_or(true), hot_x: v("WICK_HOT_X", 2.5),
            h3_wkd_pct: v("WICK_H3_WKD_PCT", 2.5) / 100.0, h3_night_pct: v("WICK_H3_NIGHT_PCT", 3.0) / 100.0, shut_hold_s: v("WICK_H3_SHUT_HOLD_H", 24.0) * 3600.0,
            shut_max_open: v("WICK_H3_SHUT_MAX_OPEN", 3.0) as usize, deep_pct: v("WICK_H3_DEEP_PCT", 5.0) / 100.0,
            deep_max_open: v("WICK_H3_DEEP_MAX_OPEN", 3.0) as usize, cry_deep_pct: v("WICK_DEEP_PCT", 10.0) / 100.0,
            cry_deep_coins: v("WICK_DEEP_MAX_COINS", 10.0) as usize, cry_deep_max_open: v("WICK_DEEP_MAX_OPEN", 3.0) as usize,
            cry_deep3_pct: v("WICK_DEEP3_PCT", 12.0) / 100.0, cry_deep3_take: v("WICK_DEEP3_TAKE_PCT", 6.0) / 100.0,
            cry_deep3_max_open: v("WICK_DEEP3_MAX_OPEN", 3.0) as usize, h3_session_deep_pct: v("WICK_H3_SESSION_DEEP_PCT", 7.0) / 100.0,
            ask_pct: v("WICK_ASK_PCT", 5.0) / 100.0, ask_coins: v("WICK_ASK_MAX_COINS", 5.0) as usize, ask_take: v("WICK_ASK_TAKE_PCT", 3.0) / 100.0,
            ask_stop: v("WICK_ASK_STOP_PCT", 10.0) / 100.0, ask_hold_s: v("WICK_ASK_HOLD_MIN", 240.0) * 60.0, ask_max_open: v("WICK_ASK_MAX_OPEN", 2.0) as usize,
            h3_ask_shut_pct: v("WICK_H3_ASK_SHUT_PCT", 4.0) / 100.0, h3_ask_open_pct: v("WICK_H3_ASK_OPEN_PCT", 7.0) / 100.0, asia_ask_pct: v("WICK_H3_ASIA_ASK_PCT", 3.0) / 100.0,
            h3_ask_max_open: v("WICK_H3_ASK_MAX_OPEN", 3.0) as usize, h3_ask_crash: v("WICK_H3_ASK_CRASH_PCT", 15.0) / 100.0, noneq_shut_pct: v("WICK_H3_NONEQ_SHUT_PCT", 3.0) / 100.0,
            maj_pct: v("WICK_MAJ_PCT", 4.0) / 100.0, maj_hold_s: v("WICK_MAJ_HOLD_MIN", 240.0) * 60.0, maj_max_open: v("WICK_MAJ_MAX_OPEN", 2.0) as usize,
            lowvol_pct: v("WICK_LOWVOL_PCT", 7.0) / 100.0, lowvol_min_vol: v("WICK_LOWVOL_MIN_VOL_USD", 1e6), lowvol_coins: v("WICK_LOWVOL_MAX_COINS", 20.0) as usize,
            lowvol_max_open: v("WICK_LOWVOL_MAX_OPEN", 3.0) as usize, lowvol_deep_pct: v("WICK_LOWVOL_DEEP_PCT", 12.0) / 100.0,
            margin_budget_pct: v("WICK_MARGIN_BUDGET_PCT", 95.0), thin_min_vol: v("WICK_THIN_MIN_VOL_USD", 0.3e6), thin_coins: v("WICK_THIN_MAX_COINS", 20.0) as usize }
    }

    /// A shut-hours bid's level for `coin` at `minute` (a US stock on the weekend, a US night or Sunday night), else None.
    fn shut_pct(&self, coin: &str, minute: i64) -> Option<f64> {
        if !crate::wkd::us_stock(coin) {
            return None;
        }
        if crate::wkd::weekend(minute) {
            Some(self.h3_wkd_pct)
        } else if crate::wkd::us_night(minute) || crate::wkd::us_sunday_night(minute) {
            Some(self.h3_night_pct)
        } else {
            None
        }
    }

    /// A rung's trade's (take, stop, hold s): the asks' own; the third rung's take; the HIP-3 session ladder's take = its level; else `exits`.
    fn rung_exits(&self, coin: &str, rung: usize, shut: bool, pct: f64) -> (f64, f64, f64) {
        let (take, stop, hold) = self.exits(coin, shut, pct);
        match rung {
            MAJ => (pct, stop, self.maj_hold_s),
            ASK if coin.contains(':') => (pct, 0.20, if shut { 240.0 * 60.0 } else { 60.0 * 60.0 }),
            ASK => (self.ask_take, self.ask_stop, self.ask_hold_s),
            0 if shut && crate::wkd::non_equity(coin) => (pct, 0.20, 240.0 * 60.0),
            2 => (self.cry_deep3_take, stop, hold),
            1 if coin.contains(':') && !shut => (pct, stop, hold),
            _ => (take, stop, hold),
        }
    }

    /// A trade's (take, stop, hold s): a shut-hours bid's at its level `pct`, else the side's.
    fn exits(&self, coin: &str, shut: bool, pct: f64) -> (f64, f64, f64) {
        if shut {
            (pct, 2.5 * pct, self.shut_hold_s)
        } else {
            (if coin.contains(':') { self.h3_take } else { self.take }, self.stop, self.hold_s)
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Bid {
    oid: u64,
    px: f64,
    size: f64,
    /// The level under the anchor, and whether a shut-hours bid (its own exits).
    #[serde(default)]
    pct: f64,
    #[serde(default)]
    shut: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    bids: HashMap<String, Bid>,
    #[serde(default)]
    open: Vec<Trade>,
    #[serde(default)]
    last_fill: HashMap<String, f64>,
    /// The ladder's deeper bids, their trades and their coins' last fills (the registry's kind `DEEP`).
    #[serde(default)]
    deep: HashMap<String, Bid>,
    #[serde(default)]
    deep_open: Vec<Trade>,
    #[serde(default)]
    deep_fill: HashMap<String, f64>,
    /// The crypto ladder's third rung: its bids, trades, last fills (the registry's kind `RUNG3`).
    #[serde(default)]
    deep3: HashMap<String, Bid>,
    #[serde(default)]
    deep3_open: Vec<Trade>,
    #[serde(default)]
    deep3_fill: HashMap<String, f64>,
    /// The asks (short): resting, their trades, their coins' last fills (the registry's kind `wicka`).
    #[serde(default)]
    asks: HashMap<String, Bid>,
    #[serde(default)]
    asks_open: Vec<Trade>,
    #[serde(default)]
    ask_fill: HashMap<String, f64>,
    /// The majors' bids, their trades, their coins' last fills (the registry's kind `wickm`).
    #[serde(default)]
    maj: HashMap<String, Bid>,
    #[serde(default)]
    maj_open: Vec<Trade>,
    #[serde(default)]
    maj_fill: HashMap<String, f64>,
    /// The low-volume bids, their trades, their coins' last fills (the registry's kind `wickl`).
    #[serde(default)]
    low: HashMap<String, Bid>,
    #[serde(default)]
    low_open: Vec<Trade>,
    #[serde(default)]
    low_fill: HashMap<String, f64>,
    /// Their ladder's (the registry's kind `wickl2`).
    #[serde(default)]
    low2: HashMap<String, Bid>,
    #[serde(default)]
    low2_open: Vec<Trade>,
    #[serde(default)]
    low2_fill: HashMap<String, f64>,
    /// The fills on each side (the trial's counts).
    #[serde(default)]
    opened: u32,
    #[serde(default)]
    h3_opened: u32,
    /// The trials' round: the counts reset once when it is under `TRIAL_ROUND` (a new round: the earlier fills
    /// were not the bot's present logic).
    #[serde(default)]
    trial_round: u32,
}

/// The trials' round now: 2 (2026-10-28: the HIP-3 trial's first fill, xyz:MRNA 2026-10-27, came from a bid left over
/// the research's level before the down-repricing; the trials start again on today's settings).
const TRIAL_ROUND: u32 = 2;

/// A coin's dex ("" for the main one).
fn dex_of(coin: &str) -> &str {
    coin.split_once(':').map_or("", |d| d.0)
}

/// The bid's level for an anchor: `pct` under it.
fn level(anchor: f64, pct: f64) -> f64 {
    anchor * (1.0 - pct)
}

/// Whether the level `new` is past a resting order at `old`: under a bid, over an ask (the order would
/// fill nearer the price than the research had).
fn level_past(ask: bool, old: f64, new: f64) -> bool {
    if ask { new > old } else { new < old }
}

/// Whether a resting bid at `old` should move to `new`.
fn drifted(old: f64, new: f64, up: f64, down: f64) -> bool {
    old <= 0.0 || new >= old * (1.0 + up) || new <= old * (1.0 - down)
}

/// The coins to bid: of those with a volume of `min_vol`+ (left out: those `skip` says), the top `n`
/// by `score` (the coins without one after, by volume).
fn pick(volume: &HashMap<String, f64>, min_vol: f64, n: usize, skip: impl Fn(&str) -> bool, score: impl Fn(&str) -> Option<f64>) -> Vec<String> {
    let mut v: Vec<(&String, Option<f64>, f64)> = volume.iter().filter(|(c, x)| **x >= min_vol && !skip(c)).map(|(c, x)| (c, score(c), *x)).collect();
    v.sort_by(|a, b| match (a.1, b.1) {
        (Some(x), Some(y)) => y.total_cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => b.2.total_cmp(&a.2),
    });
    v.into_iter().take(n).map(|x| x.0.clone()).collect()
}

/// The thin coins' bids ($0.3-1M; V285 .. V289: the tape 210 fills +285 bp, t 7.0, the halves +347 / +223, w/o the 10 best +238;
/// the portfolio x1121 -> x1767 at the drawdown 21.2 -> 19.9%, but 5x and less: ~2.4% of the equity's margin a bid): last.
const THIN_PRIORITY: f64 = 0.074;

/// A resting order's worth per $ of the margin it holds (V289, HL 2025-05 .. 2026-10: the fills a month x the mean x the
/// notional over the orders resting x their margin): the HIP-3 shallow bids 0.48, the crypto asks 0.33, the HIP-3 asks
/// 0.19, the low-volume bids 0.16, the crypto ladder 0.13, the majors 0.12, the third rung 0.115, the HIP-3 ladders 0.11,
/// the crypto shallow bids 0.10, the low-volume ladder 0.06. The budget keeps the first.
fn priority(rung: usize, hip3: bool) -> f64 {
    match (rung, hip3) {
        (0, true) => 0.48,
        (ASK, false) => 0.33,
        (ASK, true) => 0.19,
        (LOW, _) => 0.16,
        (1, false) => 0.13,
        (MAJ, _) => 0.12,
        (2, _) => 0.115,
        (1, true) => 0.11,
        (0, false) => 0.10,
        _ => 0.06,
    }
}

/// A coin's resting orders' margin on HL: (its bids', its asks') -> the larger side only (2026-11-14: HL holds margin
/// for one side of a coin's book, both can't fill at once; xyz $2181 summed vs $1768 held, io $646 vs $507: the budget
/// left ~$450 unused; the user's call).
fn sides_margin((bid, ask): (f64, f64)) -> f64 {
    bid.max(ask)
}

/// The hour's deviation over `x` times the day's (`x` 0: off; either unknown: not hot).
fn is_hot(hour: Option<f64>, day: Option<f64>, x: f64) -> bool {
    x > 0.0 && matches!((hour, day), (Some(h), Some(d)) if d > 0.0 && h > x * d)
}

fn usd(x: f64) -> String {
    if x >= 0.0 { format!("+${x:.2}") } else { format!("-${:.2}", -x) }
}

fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let coin = ev["coin"].as_str().unwrap_or("?");
    Some(match ev["what"].as_str()? {
        "open" if ev["ask"] == true => format!("🎣 ВЫНОС {coin}: лимитка на продажу {:.0}% выше исполнилась по {} (шорт), на ${:.0}, тейк {} / стоп {}, выход через {:.0} мин",
            f("pct") * 100.0, f("px"), f("notional").abs(), f("take_px"), f("stop_px"), f("hold_min")),
        "open" => format!("🎣 ПРОКОЛ {coin}: лимитка {:.0}% ниже{} исполнилась по {}, на ${:.0}, тейк {} / стоп {}, выход через {:.0} мин",
            f("pct") * 100.0, if ev["major"] == true { " (крупная монета)" } else if ev["deep"] == true { " (лесенка)" } else { "" }, f("px"), f("notional"), f("take_px"), f("stop_px"), f("hold_min")),
        "close" => format!("{} прокол {} {coin}: {} -> {} ({:+.2}%, {})", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            match ev["why"].as_str().unwrap_or("") { "stop" => "СТОП", "take" => "ТЕЙК", "manual" => "ЗАКРЫТО ВНЕ БОТА (руками или ликвидация)", _ => "ЗАКРЫЛ по времени" }, f("entry_px"), f("px"), f("ret_pct"), usd(f("pnl_usd"))),
        "error" => format!("⚠️ прокол {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        _ => return None,
    })
}

struct Wick {
    venue: Venue,
    unified: bool,
    /// The budget HL's refusal set: (the margin $ the orders held then, until when).
    cap: Option<(f64, f64)>,
    /// The account's positions no strategy holds: (first seen, last told).
    orphans: HashMap<String, (f64, f64)>,
    boot: f64,
    /// The budget's diagnostic trace, last written.
    diag_at: f64,
    api: Api,
    cfg: Config,
    store: Store,
    p: Params,
    state: State,
    closes: Closes,
    oracle: HashMap<String, f64>,
    volume: HashMap<String, f64>,
    h3_volume: HashMap<String, f64>,
    held: HashMap<String, f64>,
    /// The coins bid on (crypto, HIP-3), picked every `VOLUME_EVERY_S`; the coins whose candles were read.
    picked: Vec<String>,
    h3_picked: Vec<String>,
    /// The low-volume coins picked (under the crypto floor).
    low_picked: Vec<String>,
    backfilled: HashSet<String>,
    /// When the mids and the contexts (the oracles) were read last (the trace's quote ages).
    mids_at: f64,
    ctx_at: f64,
    /// Bids moved in one request whose oid changed (`Venue::modify`): (coin, rung, the old bid, when), the old
    /// order's status read on for a fill in part just before the move (the cancel's settlement, deferred).
    moved: Vec<(String, usize, Bid, f64)>,
}

impl Wick {
    fn event(&self, mut ev: Value) {
        ev["kind"] = json!("wick");
        ev["at"] = json!(now());
        log!("wick: {ev}");
        if let Some(m) = message(&ev) {
            crate::notify::send(m);
        }
        self.store.event(ev);
    }

    /// The audit's trace of the bids' life (no chat, no log line).
    fn trace(&self, mut ev: Value) {
        ev["kind"] = json!("wick_order");
        ev["at"] = json!(now());
        self.store.event(ev);
    }

    /// The open trades: (crypto, HIP-3, the HIP-3 shut-hours ones; a shut one: held over twice the side's hold).
    fn counts(&self) -> (usize, usize, usize) {
        let shut = |x: &Trade| x.coin.contains(':') && x.exit_at - x.at > 2.0 * self.p.hold_s;
        let n_shut = self.state.open.iter().filter(|x| shut(x)).count();
        let n_h3 = self.state.open.iter().filter(|x| x.coin.contains(':')).count() - n_shut;
        (self.state.open.len() - n_h3 - n_shut, n_h3, n_shut)
    }

    /// Why `coin` gets no bid now (the checks of `wanted`, in its order).
    fn why_not(&self, c: &str, minute: i64) -> &'static str {
        let h3 = c.contains(':');
        let (n_crypto, n_h3, n_shut) = self.counts();
        if h3 {
            if !self.h3_picked.iter().any(|x| x == c) { return "unpicked"; }
            if self.p.shut_pct(c, minute).map_or(n_h3 >= self.p.h3_max_open, |_| n_shut >= self.p.shut_max_open) { return "slots"; }
            if !trades::trial_open(self.p.trial, self.state.h3_opened) { return "trial"; }
        } else {
            if !self.btc_calm(minute) { return "btc"; }
            if n_crypto >= self.p.max_open { return "slots"; }
            if !trades::trial_open(self.p.trial, self.state.opened) { return "trial"; }
            if !self.picked.iter().any(|x| x == c) { return "unpicked"; }
        }
        if self.closes.at(c, minute - 1).is_none() { return "no price"; }
        if self.held.get(c).is_some_and(|x| *x != 0.0) && !trades::family_holds(c) { return "held"; }
        if self.state.last_fill.get(c).is_some_and(|x| now() - x < AGAIN_AFTER_S) { return "after its fill"; }
        if !trades::can_open(c, "wick", 1.0, trades::risk_usd(self.p.risk_usd)) { return "family cap"; }
        if !h3 && self.hot(c, minute) { return "hot"; }
        "other"
    }

    fn save(&self) {
        let (cfg, v) = (self.cfg.clone(), serde_json::to_value(&self.state).unwrap_or_default());
        tokio::spawn(async move {
            if let Err(e) = crate::store::save_doc(&cfg, DOC, &v).await {
                log!("wick: state not saved: {e:#}");
            }
        });
    }

    /// The main dex's day volumes and oracles; the HIP-3 dexes' volumes (every `VOLUME_EVERY_S`).
    async fn read_ctx(&mut self, with_h3: bool) {
        match self.venue.ex.info(json!({"type": "metaAndAssetCtxs"})).await {
            Ok(v) => {
                let u = v[0]["universe"].as_array().cloned().unwrap_or_default();
                let c = v[1].as_array().cloned().unwrap_or_default();
                for (u, c) in u.iter().zip(c.iter()) {
                    let Some(n) = u["name"].as_str() else { continue };
                    if u["isDelisted"] == true {
                        continue;
                    }
                    self.volume.insert(n.to_string(), num(&c["dayNtlVlm"]));
                    self.oracle.insert(n.to_string(), num(&c["oraclePx"]));
                }
                self.ctx_at = now();
            }
            Err(e) => log!("wick: contexts not read: {e:#}"),
        }
        if with_h3 && self.p.h3_max_coins > 0 {
            let dexes: HashSet<&str> = self.venue.coins.keys().filter_map(|c| c.split_once(':').map(|d| d.0)).collect();
            for d in dexes {
                match self.api.dex_volumes(d).await {
                    Ok(x) => self.h3_volume.extend(x),
                    Err(e) => log!("wick: {d} volumes not read: {e:#}"),
                }
            }
        }
    }

    /// The coins to bid now and their levels.
    /// A crash on the coin already: the last hour's 1-minute deviation over `hot_x` times the day's (both to the
    /// minute before; the day's from 600 minutes); unknown: not hot.
    fn hot(&self, coin: &str, minute: i64) -> bool {
        is_hot(self.closes.spread(coin, minute - 1, 60, 30), self.closes.spread(coin, minute - 1, 1440, 600), self.p.hot_x)
    }

    /// BTC within `BTC_MAX_MINUTE` of the minute before's close.
    fn btc_calm(&self, minute: i64) -> bool {
        match (self.closes.at("BTC", i64::MAX), self.closes.at("BTC", minute - 1)) {
            (Some(a), Some(b)) if b > 0.0 => (a / b - 1.0).abs() < BTC_MAX_MINUTE,
            _ => false,
        }
    }

    /// The coins to bid now: (the level, its pct under the anchor, a shut-hours bid).
    fn wanted(&self, minute: i64) -> HashMap<String, (f64, f64, bool)> {
        let mut out = HashMap::new();
        let (n_crypto, n_h3, n_shut) = self.counts();
        let risk = trades::risk_usd(self.p.risk_usd);
        let t = now();
        let free_at = |c: &str, risk: f64| {
            (self.held.get(c).is_none_or(|x| *x == 0.0) || trades::family_holds(c))
                && self.state.last_fill.get(c).is_none_or(|x| t - x >= AGAIN_AFTER_S)
                && trades::can_open(c, "wick", 1.0, risk)
        };
        let free = |c: &str| free_at(c, risk);
        if self.btc_calm(minute) && n_crypto < self.p.max_open && trades::trial_open(self.p.trial, self.state.opened) {
            for c in self.picked.clone() {
                let (Some(close), o) = (self.closes.at(&c, minute - 1), self.oracle.get(&c).copied().unwrap_or(0.0)) else { continue };
                if !free(&c) || self.hot(&c, minute) {
                    continue;
                }
                let anchor = if o > 0.0 { close.min(o) } else { close };
                out.insert(c, (level(anchor, self.p.pct), self.p.pct, false));
            }
        }
        if self.p.h3_max_coins > 0 && trades::trial_open(self.p.trial, self.state.h3_opened) {
            for c in self.h3_picked.clone() {
                let Some(close) = self.closes.at(&c, minute - 1) else { continue };
                // the non-equity names while the US market is shut: their own level, the shut-hours slots
                let noneq = self.p.noneq_shut_pct > 0.0 && crate::wkd::non_equity(&c) && crate::wkd::us_shut(minute);
                let shut = if noneq { Some(self.p.noneq_shut_pct) } else { self.p.shut_pct(&c, minute) };
                // the shut-hours trades their own slots (held up to a day, they would keep the 4% bids off)
                let room = if shut.is_some() { n_shut < self.p.shut_max_open } else { n_h3 < self.p.h3_max_open };
                if room && free_at(&c, rung_risk(&c, 0, shut.is_some(), self.p.risk_usd)) {
                    let pct = shut.unwrap_or(self.p.h3_pct);
                    out.insert(c, (level(close, pct), pct, shut.is_some()));
                }
            }
        }
        out
    }

    /// The ladder's deeper bids now: the picked US stocks while the US market is shut, `deep_pct` under the minute
    /// before's close (the same anchor as the shallow bid), while its slots last; the most restless picked crypto coins
    /// `cry_deep_pct` under the crypto anchor (the close, or the oracle when lower), BTC calm, the coin not hot.
    fn wanted_deep(&self, minute: i64) -> HashMap<String, (f64, f64, bool)> {
        let mut out = HashMap::new();
        let (risk, t) = (trades::risk_usd(self.p.risk_usd), now());
        let n_h3 = self.state.deep_open.iter().filter(|x| x.coin.contains(':')).count();
        let free = |c: &str| {
            !self.state.deep_open.iter().any(|x| x.coin == c)
                && (self.held.get(c).is_none_or(|x| *x == 0.0) || trades::family_holds(c))
                && self.state.deep_fill.get(c).is_none_or(|x| t - x >= AGAIN_AFTER_S)
                && trades::can_open(c, DEEP, 1.0, risk)
        };
        if (self.p.deep_pct > 0.0 || self.p.h3_session_deep_pct > 0.0) && n_h3 < self.p.deep_max_open && trades::trial_open(self.p.trial, self.state.h3_opened) {
            for c in &self.h3_picked {
                let Some(close) = self.closes.at(c, minute - 1) else { continue };
                if !free(c) {
                    continue;
                }
                if self.p.shut_pct(c, minute).is_some() {
                    if self.p.deep_pct > 0.0 {
                        out.insert(c.clone(), (level(close, self.p.deep_pct), self.p.deep_pct, true));
                    }
                } else if self.p.h3_session_deep_pct > 0.0 && crate::wkd::us_stock(c) {
                    // the session ladder: the US market open (or its pre / post hours)
                    out.insert(c.clone(), (level(close, self.p.h3_session_deep_pct), self.p.h3_session_deep_pct, false));
                }
            }
        }
        if self.p.cry_deep_pct > 0.0 && self.state.deep_open.len() - n_h3 < self.p.cry_deep_max_open && self.btc_calm(minute)
            && trades::trial_open(self.p.trial, self.state.opened)
        {
            for c in self.picked.iter().take(self.p.cry_deep_coins) {
                let (Some(close), o) = (self.closes.at(c, minute - 1), self.oracle.get(c).copied().unwrap_or(0.0)) else { continue };
                if free(c) && !self.hot(c, minute) {
                    let anchor = if o > 0.0 { close.min(o) } else { close };
                    out.insert(c.clone(), (level(anchor, self.p.cry_deep_pct), self.p.cry_deep_pct, false));
                }
            }
        }
        out
    }

    /// The crypto ladder's third rung now: the same coins and checks as its second, `cry_deep3_pct` under the anchor.
    fn wanted_deep3(&self, minute: i64) -> HashMap<String, (f64, f64, bool)> {
        let mut out = HashMap::new();
        if self.p.cry_deep3_pct <= 0.0 || self.state.deep3_open.len() >= self.p.cry_deep3_max_open || !self.btc_calm(minute)
            || !trades::trial_open(self.p.trial, self.state.opened)
        {
            return out;
        }
        let (risk, t) = (trades::risk_usd(self.p.risk_usd), now());
        for c in self.picked.iter().take(self.p.cry_deep_coins) {
            let (Some(close), o) = (self.closes.at(c, minute - 1), self.oracle.get(c).copied().unwrap_or(0.0)) else { continue };
            if !self.state.deep3_open.iter().any(|x| &x.coin == c)
                && (self.held.get(c).is_none_or(|x| *x == 0.0) || trades::family_holds(c))
                && self.state.deep3_fill.get(c).is_none_or(|x| t - x >= AGAIN_AFTER_S)
                && trades::can_open(c, trades::RUNG3, 1.0, risk)
                && !self.hot(c, minute)
            {
                let anchor = if o > 0.0 { close.min(o) } else { close };
                out.insert(c.clone(), (level(anchor, self.p.cry_deep3_pct), self.p.cry_deep3_pct, false));
            }
        }
        out
    }

    /// The asks now: the most restless picked crypto coins, `ask_pct` over the anchor (the close, or the oracle when higher), BTC calm,
    /// the coin not hot, while their slots last.
    fn wanted_asks(&self, minute: i64) -> HashMap<String, (f64, f64, bool)> {
        let mut out = HashMap::new();
        let (risk, t) = (trades::risk_usd(self.p.risk_usd), now());
        let free = |c: &str| {
            !self.state.asks_open.iter().any(|x| x.coin == c)
                && (self.held.get(c).is_none_or(|x| *x == 0.0) || trades::family_holds(c))
                && self.state.ask_fill.get(c).is_none_or(|x| t - x >= AGAIN_AFTER_S)
                && trades::can_open(c, KINDS[ASK], -1.0, risk)
        };
        let n_h3 = self.state.asks_open.iter().filter(|x| x.coin.contains(':')).count();
        if self.p.ask_pct > 0.0 && self.state.asks_open.len() - n_h3 < self.p.ask_max_open && self.btc_calm(minute) && trades::trial_open(self.p.trial, self.state.opened) {
            for c in self.picked.iter().take(self.p.ask_coins) {
                let (Some(close), o) = (self.closes.at(c, minute - 1), self.oracle.get(c).copied().unwrap_or(0.0)) else { continue };
                if free(c) && !self.hot(c, minute) {
                    out.insert(c.clone(), (close.max(o) * (1.0 + self.p.ask_pct), self.p.ask_pct, false));
                }
            }
        }
        // the HIP-3 asks: a US stock shut / open, an Asian stock while the US market is open
        if n_h3 < self.p.h3_ask_max_open && trades::trial_open(self.p.trial, self.state.h3_opened) {
            let shut = crate::wkd::us_shut(minute);
            for c in &self.h3_picked {
                let Some(close) = self.closes.at(c, minute - 1) else { continue };
                let pct = if crate::wkd::us_stock(c) {
                    if shut { self.p.h3_ask_shut_pct } else { self.p.h3_ask_open_pct }
                } else if crate::wkd::asian_stock(c) && !shut {
                    self.p.asia_ask_pct
                } else {
                    0.0
                };
                // the move over the ~24 h the closes keep (HIST: the oldest one at minute - 1440)
                let crashed = self.p.h3_ask_crash > 0.0 && self.closes.change(c, minute - 1, HIST as i64 - 2).is_some_and(|x| x <= -self.p.h3_ask_crash);
                if pct > 0.0 && free(c) && !crashed {
                    out.insert(c.clone(), (close * (1.0 + pct), pct, shut && crate::wkd::us_stock(c)));
                }
            }
        }
        out
    }

    /// The majors' bids now: `MAJORS` `maj_pct` under the crypto anchor, BTC calm, the coin not hot, while their slots last.
    fn wanted_majors(&self, minute: i64) -> HashMap<String, (f64, f64, bool)> {
        let mut out = HashMap::new();
        if self.p.maj_pct <= 0.0 || self.state.maj_open.len() >= self.p.maj_max_open || !self.btc_calm(minute) || !trades::trial_open(self.p.trial, self.state.opened) {
            return out;
        }
        let (risk, t) = (trades::risk_usd(self.p.risk_usd), now());
        for c in MAJORS {
            if !self.venue.coins.contains_key(c) {
                continue;
            }
            let (Some(close), o) = (self.closes.at(c, minute - 1), self.oracle.get(c).copied().unwrap_or(0.0)) else { continue };
            if !self.state.maj_open.iter().any(|x| x.coin == c)
                && (self.held.get(c).is_none_or(|x| *x == 0.0) || trades::family_holds(c))
                && self.state.maj_fill.get(c).is_none_or(|x| t - x >= AGAIN_AFTER_S)
                && trades::can_open(c, KINDS[MAJ], 1.0, risk)
                && !self.hot(c, minute)
            {
                let anchor = if o > 0.0 { close.min(o) } else { close };
                out.insert(c.to_string(), (level(anchor, self.p.maj_pct), self.p.maj_pct, false));
            }
        }
        out
    }

    /// The low-volume bids now: the picked low-volume coins `lowvol_pct` under the crypto anchor, BTC calm, the coin not hot, while their
    /// slots last.
    fn wanted_low(&self, minute: i64, rung: usize) -> HashMap<String, (f64, f64, bool)> {
        let mut out = HashMap::new();
        let (pct, open, fill) = if rung == LOW { (self.p.lowvol_pct, &self.state.low_open, &self.state.low_fill) } else { (self.p.lowvol_deep_pct, &self.state.low2_open, &self.state.low2_fill) };
        if pct <= 0.0 || self.p.lowvol_pct <= 0.0 || open.len() >= self.p.lowvol_max_open || !self.btc_calm(minute) || !trades::trial_open(self.p.trial, self.state.opened) {
            return out;
        }
        let (risk, t) = (trades::risk_usd(self.p.risk_usd), now());
        for c in &self.low_picked {
            if rung == LOW2 && self.thin(c) {
                continue;
            }
            let (Some(close), o) = (self.closes.at(c, minute - 1), self.oracle.get(c).copied().unwrap_or(0.0)) else { continue };
            if !open.iter().any(|x| &x.coin == c)
                && (self.held.get(c).is_none_or(|x| *x == 0.0) || trades::family_holds(c))
                && fill.get(c).is_none_or(|x| t - x >= AGAIN_AFTER_S)
                && trades::can_open(c, KINDS[rung], 1.0, risk)
                && !self.hot(c, minute)
            {
                let anchor = if o > 0.0 { close.min(o) } else { close };
                out.insert(c.clone(), (level(anchor, pct), pct, false));
            }
        }
        out
    }

    /// A rung's bids: the shallow ones (0), the ladder's second (1) and third (2), the asks (3), the majors' (4), the low-volume (5).
    fn book(&mut self, rung: usize) -> &mut HashMap<String, Bid> {
        match rung {
            0 => &mut self.state.bids,
            1 => &mut self.state.deep,
            2 => &mut self.state.deep3,
            ASK => &mut self.state.asks,
            MAJ => &mut self.state.maj,
            LOW => &mut self.state.low,
            _ => &mut self.state.low2,
        }
    }

    /// A rung's trades.
    fn opens(&mut self, rung: usize) -> &mut Vec<Trade> {
        match rung {
            0 => &mut self.state.open,
            1 => &mut self.state.deep_open,
            2 => &mut self.state.deep3_open,
            ASK => &mut self.state.asks_open,
            MAJ => &mut self.state.maj_open,
            LOW => &mut self.state.low_open,
            _ => &mut self.state.low2_open,
        }
    }

    /// The coins to bid on now: crypto and HIP-3.
    fn repick(&mut self, minute: i64) {
        let closes = &self.closes;
        let by_spread = self.p.by_spread;
        let spread = |c: &str| if by_spread { closes.spread(c, minute - 1, HIST as i64 - 1, MIN_HIST) } else { None };
        let coins = &self.venue.coins;
        self.picked = pick(&self.volume, self.p.min_vol, self.p.max_coins, |c| c == "BTC" || !coins.contains_key(c), spread);
        let under: HashMap<String, f64> = self.volume.iter().filter(|(_, v)| **v < self.p.min_vol).map(|(c, v)| (c.clone(), *v)).collect();
        self.low_picked = if self.p.lowvol_pct > 0.0 {
            let mut v = pick(&under, self.p.lowvol_min_vol, self.p.lowvol_coins, |c| !coins.contains_key(c) || MAJORS.contains(&c), spread);
            // the thin ones ($0.3-1M): their own 20 most restless
            if self.p.thin_min_vol > 0.0 && self.p.thin_min_vol < self.p.lowvol_min_vol {
                let thin: HashMap<String, f64> = under.iter().filter(|(_, v)| **v < self.p.lowvol_min_vol).map(|(c, v)| (c.clone(), *v)).collect();
                v.extend(pick(&thin, self.p.thin_min_vol, self.p.thin_coins, |c| !coins.contains_key(c) || MAJORS.contains(&c), spread));
            }
            v
        } else {
            Vec::new()
        };
        self.h3_picked = if self.p.h3_max_coins > 0 {
            pick(&self.h3_volume, self.p.h3_min_vol, self.p.h3_max_coins, |c| c.starts_with("hyna:") || !coins.contains_key(c), spread)
        } else {
            Vec::new()
        };
        // The observer's coins: the picks, bid or not (the research's shadow follows them either way).
        let mut watch: HashMap<String, (f64, f64)> = self.picked.iter().map(|c| (c.clone(), (self.p.pct, self.p.take))).collect();
        watch.extend(self.h3_picked.iter().map(|c| (c.clone(), (self.p.h3_pct, self.p.h3_take))));
        crate::wickwatch::set_watch(watch);
    }

    /// One coin's 24 h of 1-minute closes (the liquid ones without 6 h of them, the biggest first).
    async fn backfill(&mut self) {
        let (coins, closes, done) = (&self.venue.coins, &self.closes, &self.backfilled);
        let short = |c: &String| !done.contains(c) && coins.contains_key(c) && closes.0.get(c).is_none_or(|q| q.len() < MIN_HIST);
        let floor = if self.p.lowvol_pct > 0.0 { self.p.min_vol.min(self.p.lowvol_min_vol).min(if self.p.thin_min_vol > 0.0 { self.p.thin_min_vol } else { f64::MAX }) } else { self.p.min_vol };
        let mut todo: Vec<(&String, &f64)> = self.volume.iter().filter(|(c, x)| **x >= floor && c.as_str() != "BTC" && short(c))
            .chain(self.h3_volume.iter().filter(|(c, x)| **x >= self.p.h3_min_vol && !c.starts_with("hyna:") && short(c))).collect();
        todo.sort_by(|a, b| b.1.total_cmp(a.1));
        let Some(c) = todo.first().map(|x| x.0.clone()) else { return };
        self.backfilled.insert(c.clone());
        match self.api.minute_closes(&c, now() - HIST as f64 * 60.0).await {
            Ok(v) => self.closes.backfill(&c, &v, HIST),
            Err(e) => log!("wick: {c} candles not read: {e:#}"),
        }
    }

    /// The prices of the main dex and of the HIP-3 dexes bid on (the anchors: every poll); with
    /// `all`, every HIP-3 dex (the deviations of the coins not bid on).
    async fn read_mids(&mut self, minute: i64, all: bool) {
        let mut dexes: HashSet<String> = HashSet::from([String::new()]);
        dexes.extend(self.h3_picked.iter().map(|c| dex_of(c).to_string()));
        if all && self.p.by_spread && self.p.h3_max_coins > 0 {
            dexes.extend(self.venue.coins.keys().filter(|c| !c.starts_with("hyna:")).filter_map(|c| c.split_once(':').map(|d| d.0.to_string())));
        }
        for d in dexes {
            match self.api.mids(&d).await {
                Ok(m) => {
                    for (c, px) in m {
                        self.closes.update(&c, minute, px, HIST);
                    }
                }
                Err(e) => log!("wick: mids {d} not read: {e:#}"),
            }
        }
        self.mids_at = now();
    }

    /// The bids put right: off where not wanted, placed where missing, moved where drifted (the shallow ones, then
    /// the ladder's rungs).
    async fn place(&mut self, minute: i64) {
        let mut wants: Vec<HashMap<String, (f64, f64, bool)>> = (0..=LOW2)
            .map(|rung| match rung {
                0 => self.wanted(minute),
                1 => self.wanted_deep(minute),
                2 => self.wanted_deep3(minute),
                ASK => self.wanted_asks(minute),
                MAJ => self.wanted_majors(minute),
                _ => self.wanted_low(minute, rung),
            })
            .collect();
        let before: Vec<usize> = wants.iter().map(|w| w.len()).collect();
        let pre0: HashSet<String> = wants[0].keys().cloned().collect();
        self.within_budget(&mut wants);
        // every 10 minutes: what each rung wanted, what the budget kept, and why the first picked crypto coins get no bid
        if now() - self.diag_at >= 600.0 {
            self.diag_at = now();
            let kept: Vec<usize> = wants.iter().map(|w| w.len()).collect();
            let held: f64 = self.held_margin();
            let why: serde_json::Map<String, Value> = self.picked.iter().take(8).map(|c| (c.clone(), json!(if wants[0].contains_key(c) { "wanted" } else if pre0.contains(c) { "budget" } else { self.why_not(c, minute) }))).collect();
            self.trace(json!({"what": "budget", "wanted": before, "kept": kept, "budget": self.p.margin_budget_pct / 100.0 * trades::equity(), "equity": trades::equity(),
                "held_margin": held, "cap": self.cap.map(|x| x.0), "btc_calm": self.btc_calm(minute), "picked": self.picked.len(), "low_picked": self.low_picked.len(), "h3_picked": self.h3_picked.len(), "why_not": why}));
        }
        for (rung, want) in wants.into_iter().enumerate() {
            self.place_book(minute, rung, want).await;
        }
        self.publish_release(minute);
    }

    /// An order just cancelled: its status read; filled (wholly or in part) before the cancel: the trade opened
    /// (2026-11-02, xyz:LITE: filled at 12:19:00, repriced at 12:19:05 as if still resting, the position left
    /// with no stop). True when it was not filled (off the book); the status unread: kept for the check.
    async fn settle_cancelled(&mut self, c: &str, b: &Bid, rung: usize) -> bool {
        let r = self.venue.filled(b.oid).await;
        match after_cancel(&r) {
            AfterCancel::Filled(f) => {
                self.trace(json!({"what": "filled before the cancel", "coin": c, "oid": b.oid, "px": b.px, "size": f, "rung": rung, "t_done": now()}));
                self.filled(c, f, b.px, rung).await;
                false
            }
            AfterCancel::Gone => {
                self.book(rung).remove(c);
                true
            }
            AfterCancel::Keep => {
                match r {
                    Err(e) => log!("wick: {c} order {} status not read after the cancel (kept for the check): {e:#}", b.oid),
                    Ok(_) => log!("wick: {c} order {} still open after the cancel (kept for the check)", b.oid),
                }
                false
            }
        }
    }

    /// The account's positions (the main dex and the HIP-3 dexes margined in USDC) no strategy's trade holds (the
    /// registry): told on Telegram, not closed (2026-11-02, the user's call, after xyz:LITE). Any dex unread: that
    /// dex skipped this time.
    async fn watch_orphans(&mut self) {
        let t = now();
        if t - self.boot < ORPHAN_BOOT_S {
            return;
        }
        let mut pos: HashMap<String, f64> = HashMap::new();
        match crate::live::equity_of(&self.venue.ex, &self.venue.account, self.unified).await {
            Ok((_, p)) => pos.extend(p),
            Err(e) => log!("wick: the positions not read for the orphan check: {e:#}"),
        }
        for d in crate::live::usdc_dexes(&self.venue.ex).await {
            match self.venue.dex_positions(&d).await {
                Ok(p) => pos.extend(p),
                Err(e) => log!("wick: {d} positions not read for the orphan check: {e:#}"),
            }
        }
        pos.retain(|c, x| *x != 0.0 && !trades::family_holds(c));
        self.orphans.retain(|c, _| pos.contains_key(c));
        for (c, size) in pos {
            let (first, told) = *self.orphans.entry(c.clone()).or_insert((t, 0.0));
            if t - first >= ORPHAN_AFTER_S && t - told >= ORPHAN_AGAIN_S {
                let px = self.closes.at(&c, i64::MAX).unwrap_or(0.0);
                crate::notify::send(format!("🚨 НЕСАНКЦИОНИРОВАННАЯ СДЕЛКА: {c} {} {:.6} (~${:.0}) — ни одна стратегия бота её не ведёт: нет стопа, тейка и выхода по времени. Проверь и закрой руками.",
                    if size > 0.0 { "лонг" } else { "шорт" }, size.abs(), size.abs() * px));
                self.trace(json!({"what": "orphan", "coin": c, "size": size, "px": px, "first_seen": first, "t_done": t}));
                self.orphans.insert(c, (first, t));
            }
        }
    }

    /// An order's worth per $ of its margin: its family's (`priority`, V289) at the leverage the table assumed, scaled by
    /// the coin's own (2026-11-03: the picked HIP-3 names isolated at 3x hold ~$35 a bid, a cross one at 10x+ a third
    /// of it for the same fills; a crypto coin at 3x three times its family's $10).
    fn worth(&self, c: &str, rung: usize) -> f64 {
        let h3 = c.contains(':');
        let (base, lev_ref) = if rung == LOW && self.thin(c) {
            (THIN_PRIORITY, 4.1)
        } else {
            (priority(rung, h3), match (rung, h3) {
                (_, true) => 3.0,
                (LOW | LOW2, _) => 5.5,
                (MAJ, _) => 20.0,
                _ => 10.0,
            })
        };
        base * self.venue.leverage_of(c) / lev_ref
    }

    /// A resting order's margin $ (its notional / its coin's leverage).
    fn margin_of(&self, c: &str, b: &Bid) -> f64 {
        (b.size * b.px).abs() / self.venue.leverage_of(c)
    }

    /// The margin the resting orders hold now: a coin's larger side only (`sides_margin`).
    fn held_margin(&mut self) -> f64 {
        let mut by: HashMap<String, (f64, f64)> = HashMap::new();
        for rung in 0..=LOW2 {
            for (c, b) in self.book(rung).clone() {
                let m = self.margin_of(&c, &b);
                let e = by.entry(c).or_default();
                if rung == ASK { e.1 += m } else { e.0 += m }
            }
        }
        by.values().map(|s| sides_margin(*s)).sum()
    }

    /// The resting orders the market entries may take off, the weakest first.
    /// The least volatile coins' orders first (their 24 h 1-minute deviation; the user's call 2026-11-03: a market entry
    /// short of margin takes the orders of the calmest coins - the least likely to be hit).
    fn publish_release(&mut self, minute: i64) {
        let mut v: Vec<(f64, String, u64, f64)> = Vec::new();
        for rung in 0..=LOW2 {
            for (c, b) in self.book(rung).clone() {
                let vol = self.closes.spread(&c, minute - 1, 1440, 600).unwrap_or(0.0);
                v.push((vol, c.clone(), b.oid, self.margin_of(&c, &b)));
            }
        }
        v.sort_by(|a, b| a.0.total_cmp(&b.0));
        crate::live::set_release(v.into_iter().map(|(_, c, o, m)| (c, o, m)).collect());
    }

    /// The margin the account's open positions hold (all strategies'), by the model: notional / the coin's leverage.
    fn positions_margin(&self) -> f64 {
        self.held.iter().filter(|(_, x)| **x != 0.0).map(|(c, x)| x.abs() * self.closes.at(c, i64::MAX).unwrap_or(0.0) / self.venue.leverage_of(c)).sum()
    }

    /// A thin coin: the day's volume under the low-volume floor.
    fn thin(&self, c: &str) -> bool {
        self.volume.get(c).is_some_and(|v| *v < self.p.lowvol_min_vol)
    }

    /// The wanted orders cut to the margin budget (each holds its notional / its coin's leverage; a coin's bids and asks
    /// its larger side only, `sides_margin`): kept by
    /// `priority` (what an order earns a month per $ of the margin it holds), the rest not placed (or taken off).
    fn within_budget(&self, wants: &mut [HashMap<String, (f64, f64, bool)>]) {
        let mut budget = self.p.margin_budget_pct / 100.0 * trades::equity();
        if self.p.margin_budget_pct <= 0.0 || budget <= 0.0 {
            return;
        }
        // the open positions' margin (every strategy's: a market entry's margin comes out of the orders' budget; when it
        // closes the orders come back the next minute)
        budget -= self.positions_margin();
        // HL refused one for the margin: no more than the orders held then, for 10 minutes
        if let Some((m, _)) = self.cap.filter(|(_, until)| now() < *until) {
            budget = budget.min(m);
        }
        // within a priority the most restless first (the pick's order)
        let rank = |c: &str| [&self.picked, &self.h3_picked, &self.low_picked].iter().find_map(|v| v.iter().position(|x| x == c)).unwrap_or(usize::MAX);
        let mut all: Vec<(f64, usize, String, f64)> = wants.iter().enumerate()
            .flat_map(|(rung, w)| w.iter().map(move |(c, (_, pct, shut))| (rung, c, *pct, *shut)))
            .map(|(rung, c, pct, shut)| {
                let stop = self.p.rung_exits(c, rung, shut, pct).1;
                (self.worth(c, rung), rung, c.clone(), rung_risk(c, rung, shut, self.p.risk_usd) / stop.max(1e-9) / self.venue.leverage_of(c))
            })
            .collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)).then(rank(&a.2).cmp(&rank(&b.2))).then(a.2.cmp(&b.2)));
        let mut used = 0.0;
        let mut by: HashMap<String, (f64, f64)> = HashMap::new();
        for (_, rung, c, m) in all {
            let cur = by.get(&c).copied().unwrap_or_default();
            let next = if rung == ASK { (cur.0, cur.1 + m) } else { (cur.0 + m, cur.1) };
            let add = sides_margin(next) - sides_margin(cur);
            if used + add > budget {
                wants[rung].remove(&c);
            } else {
                used += add;
                by.insert(c, next);
            }
        }
    }

    /// A bid moved to `lvl` in one request (`Venue::modify`): only an order still open with nothing filled (a fill in
    /// part goes the cancel's way, settled as a trade); None: not moved (the caller cancels and places).
    async fn try_move(&mut self, c: &str, b: &Bid, rung: usize, lvl: f64, size: f64) -> Option<u64> {
        if !crate::live::modify_on() {
            return None;
        }
        match self.venue.filled(b.oid).await {
            Ok((f, true)) if f == 0.0 => {}
            _ => return None,
        }
        match self.venue.modify(c, b.oid, if rung == ASK { -size } else { size }, lvl, "Alo").await {
            Ok(oid) => Some(oid),
            Err(e) => {
                log!("wick: {c} order {} not moved ({e:#}): cancelled and placed", b.oid);
                None
            }
        }
    }

    /// The moved bids' old orders (`moved`): one filled in part just before its move (its status behind) - that part
    /// the trade, the moved order taken off first; one gone with nothing filled, or 10 minutes on, let go.
    async fn settle_moved(&mut self, rung: usize) {
        let due: Vec<(String, usize, Bid, f64)> = self.moved.iter().filter(|m| m.1 == rung).cloned().collect();
        for (c, r, old, at) in due {
            let st = self.venue.filled(old.oid).await;
            let done = match st {
                Ok((f, _)) if f > 0.0 => {
                    self.trace(json!({"what": "filled before the move", "coin": c, "oid": old.oid, "px": old.px, "size": f, "rung": r, "t_done": now()}));
                    if let Some(cur) = self.book(r).get(&c).cloned() {
                        self.venue.cancel(&c, cur.oid).await;
                        if !self.settle_cancelled(&c, &cur, r).await {
                            // the moved order filled too, or its status behind: the old part told, the orphan check holds the rest
                            log!("wick: {c} order {} filled {f} before its move, the moved order {} not settled clean", old.oid, cur.oid);
                            crate::notify::send(format!("⚠️ wick {c}: лимитка исполнилась на {f} прямо перед перестановкой — проверь позицию"));
                            true
                        } else {
                            self.filled(&c, f, old.px, r).await;
                            true
                        }
                    } else {
                        self.filled(&c, f, old.px, r).await;
                        true
                    }
                }
                Ok((_, false)) => now() - at > 120.0,
                _ => now() - at > 600.0,
            };
            if done {
                self.moved.retain(|m| m.2.oid != old.oid);
            }
        }
    }

    async fn place_book(&mut self, minute: i64, rung: usize, want: HashMap<String, (f64, f64, bool)>) {
        let t_calc = now();
        if !self.moved.is_empty() {
            self.settle_moved(rung).await;
        }
        let deep = rung > 0;
        let mut changed = false;
        for (c, b) in self.book(rung).clone() {
            if !want.contains_key(&c) {
                let (why, t_sent) = (if deep { "deep: not wanted" } else { self.why_not(&c, minute) }, now());
                if !deep {
                    crate::wickwatch::set_bid(&c, None);
                }
                self.venue.cancel(&c, b.oid).await;
                self.trace(json!({"what": "cancel", "coin": c, "oid": b.oid, "px": b.px, "why": why, "deep": deep, "rung": rung, "minute": minute, "t_calc": t_calc, "t_sent": t_sent, "t_done": now()}));
                self.settle_cancelled(&c, &b, rung).await;
                changed = true;
            }
        }
        for (c, (lvl, pct, shut)) in want {
            // no requests to spare (`live::RATE_RESERVE`): nothing placed or moved; a bid its level left
            // behind (the level past it: the MRNA case) only cancelled; one the level moved away from stays
            let spend = crate::live::wick_may_spend();
            let size = trades::size_for(rung_risk(&c, rung, shut, self.p.risk_usd), self.p.rung_exits(&c, rung, shut, pct).1, lvl);
            let mut old = None;
            if let Some(b) = self.book(rung).get(&c).cloned() {
                // the level's kind changed (the US market shut or open again): moved whichever way
                // an ask the mirror: moved up as soon as its level rises, down when it falls far
                let (up, down) = if rung == ASK { (self.p.reprice_down, self.p.reprice) } else { (self.p.reprice, self.p.reprice_down) };
                if b.shut == shut && !drifted(b.px, lvl, up, down) {
                    continue;
                }
                if !spend && b.shut == shut && !level_past(rung == ASK, b.px, lvl) {
                    continue;
                }
                // moved in one request: an order open with nothing filled, its kind unchanged (else cancelled, settled, placed)
                if spend && b.shut == shut {
                    let t_sent = now();
                    if let Some(oid) = self.try_move(&c, &b, rung, lvl, size).await {
                        self.book(rung).insert(c.clone(), Bid { oid, px: lvl, size, pct, shut });
                        if !deep {
                            crate::wickwatch::set_bid(&c, Some(crate::wickwatch::BidView { px: lvl, oid, since: now() }));
                        }
                        if oid != b.oid {
                            self.moved.push((c.clone(), rung, b.clone(), now()));
                        }
                        self.trace(json!({"what": "move", "coin": c, "oid": oid, "old_oid": b.oid, "px": lvl, "old_px": b.px, "size": size,
                            "why": if lvl > b.px { "drift up" } else { "drift down" }, "deep": deep, "rung": rung, "minute": minute,
                            "t_calc": t_calc, "t_sent": t_sent, "t_done": now()}));
                        changed = true;
                        continue;
                    }
                }
                let t_sent = now();
                if !deep {
                    crate::wickwatch::set_bid(&c, None);
                }
                self.venue.cancel(&c, b.oid).await;
                self.trace(json!({"what": "cancel", "coin": c, "oid": b.oid, "px": b.px, "why": if lvl > b.px { "drift up" } else { "drift down" },
                    "deep": deep, "new_px": lvl, "minute": minute, "t_calc": t_calc, "t_sent": t_sent, "t_done": now()}));
                old = Some(b.px);
                changed = true;
                // filled before the cancel (between the checks): the trade, not a new order
                if !self.settle_cancelled(&c, &b, rung).await {
                    continue;
                }
            }
            if !spend {
                continue;
            }
            let t_sent = now();
            let placed = self.venue.limit(&c, if rung == ASK { -size } else { size }, lvl, false, "Alo").await;
            let t_ack = now();
            let (result, oid, err) = match &placed {
                Ok(Placed::Resting(oid)) => ("resting", Some(*oid), None),
                Ok(Placed::Filled(..)) => ("filled at once", None, None),
                Err(e) => ("error", None, Some(format!("{e:#}"))),
            };
            self.trace(json!({"what": "place", "coin": c, "oid": oid, "result": result, "error": err, "px": lvl, "size": size, "old_px": old,
                "pct": pct, "shut": shut, "deep": deep, "rung": rung, "anchor_close": self.closes.at(&c, minute - 1),
                "oracle": (!c.contains(':')).then(|| self.oracle.get(&c).copied()).flatten(), "mid": self.closes.at(&c, i64::MAX),
                "mids_age_s": t_sent - self.mids_at, "ctx_age_s": t_sent - self.ctx_at, "minute": minute, "t_calc": t_calc, "t_sent": t_sent, "t_ack": t_ack}));
            match placed {
                Ok(Placed::Resting(oid)) => {
                    self.book(rung).insert(c.clone(), Bid { oid, px: lvl, size, pct, shut });
                    if !deep {
                        crate::wickwatch::set_bid(&c, Some(crate::wickwatch::BidView { px: lvl, oid, since: t_ack }));
                    }
                    changed = true;
                }
                Ok(Placed::Filled(got, px, _)) => self.filled(&c, got, px, rung).await,
                // Post-only refused (the price already under the level) or no margin: next time; the margin: the budget
                // capped at what the orders hold now, for 10 minutes (the market entries keep what is left).
                Err(e) => {
                    if crate::live::no_margin(&format!("{e:#}")) {
                        let held = self.held_margin();
                        if self.cap.is_none_or(|(_, until)| now() >= until) {
                            log!("wick: no margin for {c}: the orders capped at their {held:.2} for 10 minutes");
                        }
                        self.cap = Some((held, now() + 600.0));
                    }
                    log!("wick: {c} bid not placed: {e:#}");
                }
            }
        }
        if changed {
            self.save();
        }
    }

    /// A bid filled for `got` at `px` (`rung` > 0: the ladder's): the trade protected (or out again).
    async fn filled(&mut self, coin: &str, got: f64, px: f64, rung: usize) {
        let deep = rung > 0;
        // the fills read as sizes; an ask's trade is short
        let got = if rung == ASK { -got.abs() } else { got.abs() };
        let oid = self.book(rung).get(coin).map(|b| b.oid);
        let (bpct, shut) = self.book(rung).get(coin).map_or((0.0, deep && coin.contains(':')), |b| (b.pct, b.shut));
        self.trace(json!({"what": "fill", "coin": coin, "oid": oid, "deep": deep, "rung": rung, "got": got, "px": px, "t_detect": now()}));
        self.book(rung).remove(coin);
        if rung == LOW2 {
            self.state.low2_fill.insert(coin.to_string(), now());
        } else if rung == LOW {
            self.state.low_fill.insert(coin.to_string(), now());
        } else if rung == MAJ {
            self.state.maj_fill.insert(coin.to_string(), now());
        } else if rung == ASK {
            self.state.ask_fill.insert(coin.to_string(), now());
        } else if rung == 2 {
            self.state.deep3_fill.insert(coin.to_string(), now());
        } else if deep {
            self.state.deep_fill.insert(coin.to_string(), now());
        } else {
            crate::wickwatch::set_bid(coin, None);
            self.state.last_fill.insert(coin.to_string(), now());
        }
        let (side, n) = if deep {
            ("HIP-3", 0)
        } else if coin.contains(':') {
            self.state.h3_opened += 1;
            ("HIP-3", self.state.h3_opened)
        } else {
            self.state.opened += 1;
            ("крипта", self.state.opened)
        };
        if !deep && !trades::trial_open(self.p.trial, n) {
            crate::notify::send(format!("🏁 проколы ({side}): проба окончена ({n} сделки), лимитки сняты"));
        }
        let pct = if bpct > 0.0 {
            bpct
        } else if rung == LOW2 {
            self.p.lowvol_deep_pct
        } else if rung == LOW {
            self.p.lowvol_pct
        } else if rung == MAJ {
            self.p.maj_pct
        } else if rung == ASK {
            self.p.ask_pct
        } else if rung == 2 {
            self.p.cry_deep3_pct
        } else if deep {
            if coin.contains(':') { self.p.deep_pct } else { self.p.cry_deep_pct }
        } else if coin.contains(':') {
            self.p.h3_pct
        } else {
            self.p.pct
        };
        let (take, stop, hold_s) = self.p.rung_exits(coin, rung, shut, pct);
        match trades::protect(&self.venue, coin, got, px, stop, Some(take), hold_s).await {
            Ok(tr) => {
                self.event(json!({"what": "open", "coin": coin, "pct": pct, "shut": shut, "deep": deep, "rung": rung, "ask": rung == ASK, "major": rung == MAJ, "px": px, "size": got, "notional": got * px, "take_px": tr.take_px,
                    "stop_px": tr.stop_px, "hold_min": hold_s / 60.0}));
                trades::register(coin, KINDS[rung], tr.risk(), tr.size);
                self.opens(rung).push(tr);
            }
            Err(e) => {
                let _ = self.venue.market(coin, -got, true).await;
                self.event(json!({"what": "error", "coin": coin, "error": format!("the stop not placed, closed again: {e:#}")}));
            }
        }
        self.save();
    }

    /// The bids' fills (from the open orders) and the trades' ends; the account's positions.
    async fn check(&mut self) {
        let mut dexes: HashSet<String> = self.state.bids.keys().chain(self.state.deep.keys()).chain(self.state.deep3.keys()).chain(self.state.asks.keys()).chain(self.state.maj.keys()).chain(self.state.low.keys()).chain(self.state.low2.keys())
            .chain(self.state.open.iter().chain(&self.state.deep_open).chain(&self.state.deep3_open).chain(&self.state.asks_open).chain(&self.state.maj_open).chain(&self.state.low_open).chain(&self.state.low2_open).map(|x| &x.coin))
            .map(|c| dex_of(c).to_string()).collect();
        dexes.insert(String::new());
        let mut resting: HashMap<u64, (f64, f64)> = HashMap::new();
        let mut read: HashSet<String> = HashSet::new();
        let mut held = HashMap::new();
        // Bids resting on the exchange that no state knows of (dropped by the old open-orders
        // check, 29.10): only this module rests orders that are not reduce-only, so a buy or a sell (the
        // asks) not reduce-only, not ours and older than a minute is cancelled. A moved bid's reply lost
        // leaves one too (2026-11-10: the moved xyz bids placed again on top of themselves).
        let mut strays: Vec<(String, u64)> = Vec::new();
        let ours: HashSet<u64> = self.state.bids.values().chain(self.state.deep.values()).chain(self.state.deep3.values()).chain(self.state.asks.values()).chain(self.state.maj.values()).chain(self.state.low.values()).chain(self.state.low2.values()).map(|b| b.oid).collect();
        let t0 = now();
        for d in &dexes {
            let q = if d.is_empty() { json!({"type": "frontendOpenOrders", "user": self.venue.account}) } else { json!({"type": "frontendOpenOrders", "user": self.venue.account, "dex": d}) };
            let orders = match self.venue.ex.info(q).await {
                Ok(v) => v,
                Err(e) => {
                    log!("wick: open orders {d} not read: {e:#}");
                    continue;
                }
            };
            for o in orders.as_array().into_iter().flatten() {
                if let Some(oid) = o["oid"].as_u64() {
                    resting.insert(oid, (num(&o["origSz"]), num(&o["sz"])));
                    let stray = o["reduceOnly"] == json!(false) && o["isTrigger"] != json!(true) && !ours.contains(&oid)
                        && o["timestamp"].as_f64().is_some_and(|ms| t0 - ms / 1000.0 > 60.0);
                    if let (true, Some(c)) = (stray, o["coin"].as_str()) {
                        strays.push((c.to_string(), oid));
                    }
                }
            }
            let pos = if d.is_empty() {
                crate::live::equity_of(&self.venue.ex, &self.venue.account, self.unified).await.map(|x| {
                    trades::set_equity(x.0);
                    x.1
                })
            } else {
                self.venue.dex_positions(d).await
            };
            match pos {
                Ok(p) => {
                    held.extend(p);
                    read.insert(d.clone());
                }
                Err(e) => log!("wick: positions {d} not read: {e:#}"),
            }
        }
        self.held = held;
        for (c, oid) in strays {
            if self.venue.coins.contains_key(&c) {
                self.venue.cancel(&c, oid).await;
                self.trace(json!({"what": "cancel", "coin": c, "oid": oid, "why": "a stray bid, no state knows of it", "t_done": now()}));
            } else {
                log!("wick: a stray bid {c} {oid}, the coin unknown here, not cancelled");
            }
        }
        // The bids: gone from the book (filled, or cancelled by the exchange) or partly filled.
        let all: Vec<(usize, String, Bid)> = (0..=LOW2).flat_map(|r| self.book(r).clone().into_iter().map(move |(c, b)| (r, c, b))).collect();
        for (rung, c, b) in all {
            if !read.contains(dex_of(&c)) {
                continue;
            }
            match resting.get(&b.oid) {
                Some((orig, left)) if *left < *orig => {
                    self.venue.cancel(&c, b.oid).await;
                    let f = self.venue.filled(b.oid).await.map(|x| x.0).unwrap_or(orig - left);
                    self.filled(&c, f, b.px, rung).await;
                }
                Some(_) => {}
                // Not in the open orders: the order's own status decides; the open-orders read lags
                // a fresh order at busy times, and a bid dropped while it still rests fills later
                // with no stop (AAVE 29.10: five such bids filled). Still open, or no status: kept.
                None => {
                    let (f, open) = match self.venue.filled(b.oid).await {
                        Ok(x) => x,
                        Err(e) => {
                            log!("wick: {c} bid {} status not read: {e:#}", b.oid);
                            continue;
                        }
                    };
                    if f > 0.0 {
                        let f = if open {
                            self.venue.cancel(&c, b.oid).await;
                            self.venue.filled(b.oid).await.map(|x| x.0).unwrap_or(f)
                        } else {
                            f
                        };
                        self.filled(&c, f, b.px, rung).await;
                    } else if open {
                        continue;
                    } else {
                        self.trace(json!({"what": "cancel", "coin": c, "oid": b.oid, "px": b.px, "why": "gone from the book, not filled", "deep": rung > 0, "rung": rung, "t_done": now()}));
                        if rung == 0 {
                            crate::wickwatch::set_bid(&c, None);
                        }
                        self.book(rung).remove(&c);
                        self.save();
                    }
                }
            }
        }
        // The trades: out on time; gone (the stop or the take filled).
        let t = now();
        let mut changed = false;
        for rung in 0..=LOW2 {
            let (kind, deep) = (KINDS[rung], rung > 0);
            let mut keep = Vec::new();
            for tr in std::mem::take(self.opens(rung)) {
                let out = if t >= tr.exit_at {
                    match trades::close(&self.venue, &tr).await {
                        Ok(px) => Some(("time", px)),
                        Err(e) => {
                            self.event(json!({"what": "error", "coin": tr.coin, "error": format!("exit: {e:#}")}));
                            None
                        }
                    }
                } else if read.contains(dex_of(&tr.coin)) {
                    trades::gone(&self.venue, &tr, self.held.get(&tr.coin).copied().unwrap_or(0.0), kind).await
                } else {
                    None
                };
                match out {
                    Some((why, px)) => {
                        let ret = tr.ret(px);
                        self.event(json!({"what": "close", "coin": tr.coin, "why": why, "deep": deep, "rung": rung, "entry_px": tr.entry_px, "px": px, "ret_pct": ret * 100.0,
                            "pnl_usd": ret * tr.size.abs() * tr.entry_px, "held_min": (t - tr.at) / 60.0}));
                        trades::unregister(&tr.coin, kind);
                        changed = true;
                    }
                    None => keep.push(tr),
                }
            }
            *self.opens(rung) = keep;
        }
        if changed {
            self.save();
        }
    }
}

/// Starts the wick bids unless `WICK_ENABLED` is "0" (see the module).
pub async fn start(cfg: &Config, store: Store, api: Api) -> Result<()> {
    if trades::switch("WICK_ENABLED").is_none() {
        log!("wick: off (WICK_ENABLED=0)");
        return Ok(());
    }
    let Some((core, unified)) = crate::live::venue_from_env(None).await? else {
        log!("wick: LIVE_KEY / LIVE_ACCOUNT not set, off");
        return Ok(());
    };
    let p = Params::from_env();
    let venue = if p.h3_max_coins > 0 {
        match core.with_hip3().await {
            Ok(x) => x.0,
            Err(e) => {
                log!("wick: the HIP-3 perps not read, the main dex only: {e:#}");
                core.clone()
            }
        }
    } else {
        core.clone()
    };
    let mut state: State = crate::store::load_doc(cfg, DOC).await?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
    if state.trial_round < TRIAL_ROUND {
        log!("wick: the trials start again (round {TRIAL_ROUND}): crypto {} and HIP-3 {} fills before not counted", state.opened, state.h3_opened);
        (state.opened, state.h3_opened, state.trial_round) = (0, 0, TRIAL_ROUND);
    }
    // The bids a restart left: off (placed again at the levels now).
    for (c, b) in std::mem::take(&mut state.bids).into_iter().chain(std::mem::take(&mut state.deep)).chain(std::mem::take(&mut state.deep3)).chain(std::mem::take(&mut state.asks)).chain(std::mem::take(&mut state.maj)).chain(std::mem::take(&mut state.low)).chain(std::mem::take(&mut state.low2)) {
        if venue.coins.contains_key(&c) {
            venue.cancel(&c, b.oid).await;
        }
    }
    for tr in &state.open {
        trades::register(&tr.coin, "wick", tr.risk(), tr.size);
    }
    for tr in &state.deep_open {
        trades::register(&tr.coin, DEEP, tr.risk(), tr.size);
    }
    for tr in &state.deep3_open {
        trades::register(&tr.coin, trades::RUNG3, tr.risk(), tr.size);
    }
    for tr in &state.asks_open {
        trades::register(&tr.coin, KINDS[ASK], tr.risk(), tr.size);
    }
    for tr in &state.maj_open {
        trades::register(&tr.coin, KINDS[MAJ], tr.risk(), tr.size);
    }
    for tr in &state.low_open {
        trades::register(&tr.coin, KINDS[LOW], tr.risk(), tr.size);
    }
    for tr in &state.low2_open {
        trades::register(&tr.coin, KINDS[LOW2], tr.risk(), tr.size);
    }
    log!("wick: account {}: crypto {:.0}% under on {} coins (${:.0}M+), HIP-3 {:.0}% under on {} (${:.0}M+); take {:.0}% (HIP-3 {:.0}%), stop {:.0}%, {:.0} min, risk {}, at most {} + {}; picked by {}; {} held",
        venue.account, p.pct * 100.0, p.max_coins, p.min_vol / 1e6, p.h3_pct * 100.0, p.h3_max_coins, p.h3_min_vol / 1e6, p.take * 100.0, p.h3_take * 100.0, p.stop * 100.0,
        p.hold_s / 60.0, trades::sizing_label(p.risk_usd, p.stop), p.max_open, p.h3_max_open, if p.by_spread { "the 24 h deviation" } else { "volume" }, state.open.len());
    crate::notify::send(format!("🎣 проколы лимиток НА ДЕНЬГИ: крипта лимитка на {:.0}% ниже на {} монетах (оборот от ${:.0}M), HIP-3 на {:.0}% ниже на {} монетах; тейк +{:.0}% (HIP-3 +{:.0}%), стоп {:.0}%, выход через {:.0} мин, риск {}, до {} + {} позиций (крипта + HIP-3); монеты {}",
        p.pct * 100.0, p.max_coins, p.min_vol / 1e6, p.h3_pct * 100.0, p.h3_max_coins, p.take * 100.0, p.h3_take * 100.0, p.stop * 100.0, p.hold_s / 60.0,
        trades::sizing_label(p.risk_usd, p.stop), p.max_open, p.h3_max_open,
        if p.by_spread { "самые дёрганые за сутки (по разбросу минутных движений)" } else { "с наибольшим оборотом" })
        + &trades::trial_label(p.trial, state.opened.max(state.h3_opened)).replace("сделки", "сделки на каждой стороне"));
    crate::notify::send(format!("🎣 акции США, пока биржа закрыта: лимитка на {:.1}% ниже в выходные и на {:.0}% ночью (и в ночь на понедельник по Нью-Йорку), тейк = отступ, стоп ×2.5, до {:.0} ч, до {} позиций; вторая лимитка (лесенка) на {:.0}% ниже, до {} позиций",
        p.h3_wkd_pct * 100.0, p.h3_night_pct * 100.0, p.shut_hold_s / 3600.0, p.shut_max_open, p.deep_pct * 100.0, p.deep_max_open));
    if p.cry_deep_pct > 0.0 {
        crate::notify::send(format!("🎣 крипта, лесенка: вторая лимитка на {:.0}% ниже на {} самых дёрганых монетах, тейк +{:.0}%, стоп {:.0}%, выход через {:.0} мин, до {} позиций; третья на {:.0}% ниже, тейк +{:.0}%, до {} позиций",
            p.cry_deep_pct * 100.0, p.cry_deep_coins, p.take * 100.0, p.stop * 100.0, p.hold_s / 60.0, p.cry_deep_max_open,
            p.cry_deep3_pct * 100.0, p.cry_deep3_take * 100.0, p.cry_deep3_max_open));
    }
    if p.ask_pct > 0.0 {
        crate::notify::send(format!("🎣 крипта, выносы вверх: лимитка на ПРОДАЖУ на {:.0}% выше на {} самых дёрганых монетах (шорт), тейк −{:.0}%, стоп +{:.0}%, выход через {:.0} мин, до {} позиций",
            p.ask_pct * 100.0, p.ask_coins, p.ask_take * 100.0, p.ask_stop * 100.0, p.ask_hold_s / 60.0, p.ask_max_open));
    }
    if p.h3_ask_shut_pct > 0.0 || p.h3_ask_open_pct > 0.0 || p.asia_ask_pct > 0.0 || p.noneq_shut_pct > 0.0 {
        crate::notify::send(format!("🎣 HIP-3 выносы вверх: лимитка на ПРОДАЖУ у акций США на {:.0}% выше, пока биржа закрыта, и на {:.0}% в сессию, у азиатских на {:.0}% в часы США (шорт, тейк = отступ, стоп 20%, на изолированной марже внутри ликвидации, до {} позиций; не ставим, если бумага упала на {:.0}%+ за сутки); сырьё и крипто-связанные: покупка на {:.0}% ниже, пока биржа США закрыта",
            p.h3_ask_shut_pct * 100.0, p.h3_ask_open_pct * 100.0, p.asia_ask_pct * 100.0, p.h3_ask_max_open, p.h3_ask_crash * 100.0, p.noneq_shut_pct * 100.0));
    }
    if p.lowvol_pct > 0.0 {
        crate::notify::send(format!("🎣 монеты с малым оборотом (${:.0}M .. ${:.0}M в день, {} самых дёрганых): лимитка на {:.0}% ниже, тейк +{:.0}%, стоп {:.0}%, выход через {:.0} мин, до {} позиций",
            p.lowvol_min_vol / 1e6, p.min_vol / 1e6, p.lowvol_coins, p.lowvol_pct * 100.0, p.take * 100.0, p.stop * 100.0, p.hold_s / 60.0, p.lowvol_max_open));
        if p.lowvol_deep_pct > 0.0 {
            crate::notify::send(format!("🎣 лесенка малого оборота: вторая лимитка на {:.0}% ниже, те же выходы, до {} позиций", p.lowvol_deep_pct * 100.0, p.lowvol_max_open));
        }
    }
    if p.maj_pct > 0.0 {
        crate::notify::send(format!("🎣 крупные монеты (ETH, SOL, XRP, BNB, DOGE): лимитка на {:.0}% ниже, тейк +{:.0}%, стоп {:.0}%, выход через {:.0} мин, до {} позиций",
            p.maj_pct * 100.0, p.maj_pct * 100.0, p.stop * 100.0, p.maj_hold_s / 60.0, p.maj_max_open));
    }
    if p.h3_session_deep_pct > 0.0 {
        crate::notify::send(format!("🎣 акции США в сессию: вторая лимитка (лесенка) на {:.0}% ниже, тейк = отступ, стоп {:.0}%, {:.0} мин",
            p.h3_session_deep_pct * 100.0, p.stop * 100.0, p.hold_s / 60.0));
    }
    let mut w = Wick { venue, unified, cap: None, orphans: HashMap::new(), boot: now(), diag_at: 0.0, api, cfg: cfg.clone(), store, p, state, closes: Closes::default(), oracle: HashMap::new(),
        volume: HashMap::new(), h3_volume: HashMap::new(), held: HashMap::new(),
        picked: Vec::new(), h3_picked: Vec::new(), low_picked: Vec::new(), backfilled: HashSet::new(), mids_at: 0.0, ctx_at: 0.0, moved: Vec::new() };
    crate::wickwatch::spawn(w.store.clone(), w.venue.account.clone(), w.p.stop, w.p.hold_s);
    tokio::spawn(async move {
        let (mut vol_at, mut oracle_at, mut orders_at, mut backfill_at, mut h3_at, mut orphans_at) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        let mut last_minute = (now() / 60.0).floor() as i64;
        let mut tick = tokio::time::interval(std::time::Duration::from_secs_f64(POLL_S));
        loop {
            tick.tick().await;
            let t = now();
            let minute = (t / 60.0).floor() as i64;
            let repick = t - vol_at >= VOLUME_EVERY_S;
            if repick {
                w.read_ctx(true).await;
                vol_at = t;
                oracle_at = t;
            } else if t - oracle_at >= ORACLE_EVERY_S {
                w.read_ctx(false).await;
                oracle_at = t;
            }
            let all = t - h3_at >= H3_MIDS_EVERY_S;
            w.read_mids(minute, all).await;
            if all {
                h3_at = t;
            }
            if repick {
                w.repick(minute);
            }
            if w.p.by_spread && t - backfill_at >= BACKFILL_EVERY_S {
                w.backfill().await;
                backfill_at = t;
            }
            if t - orders_at >= ORDERS_EVERY_S {
                w.check().await;
                orders_at = t;
            }
            if t - orphans_at >= ORPHANS_EVERY_S {
                w.watch_orphans().await;
                orphans_at = t;
            }
            // The levels move with the minutes' closes: once a minute (and at once when BTC jumps).
            if minute > last_minute || !w.state.bids.is_empty() && w.wanted(minute).len() < w.state.bids.len() {
                w.place(minute).await;
                last_minute = minute;
            }
        }
    });
    Ok(())
}

/// What a cancelled bid was, from its status read right after the cancel.
#[derive(Debug, PartialEq)]
pub(crate) enum AfterCancel {
    /// filled (in part) before the cancel: the trade, with its stop and take
    Filled(f64),
    /// cancelled with nothing filled: off the book
    Gone,
    /// not known yet: kept for the next check, which settles it on its own status
    Keep,
}

/// The status lags a fill by seconds (2026-11-09, xyz:HIMS: the bid filled at 21:08:00.56, cancelled 1.4 s on as "held"
/// (its own position seen first), the status still "open" with nothing filled -> dropped; the position ran 8 minutes with
/// no stop, told as an orphan): still "open" after the cancel, or unread -> kept.
pub(crate) fn after_cancel(r: &anyhow::Result<(f64, bool)>) -> AfterCancel {
    match r {
        Ok((f, _)) if *f > 0.0 => AfterCancel::Filled(*f),
        Ok((_, true)) | Err(_) => AfterCancel::Keep,
        Ok(_) => AfterCancel::Gone,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coin_holds_its_larger_side_only() {
        // xyz:CRWV 2026-11-14: a bid holding $230.7 and an ask $72.4 -> HL held $230.7, not $303.1
        assert_eq!(sides_margin((230.7, 72.4)), 230.7);
        assert_eq!(sides_margin((0.0, 72.4)), 72.4);
        assert_eq!(sides_margin((115.1 + 230.4, 71.9)), 345.5);   // two bids add, the ask under them free
        assert_eq!(sides_margin((0.0, 0.0)), 0.0);
    }

    #[test]
    fn only_the_main_hip3_bid_takes_5_pct() {
        // the family at 2% of $1000: $20; the main HIP-3 bid (rung 0, US session) 5%: $50
        assert_eq!(rung_risk_at("xyz:SOXL", 0, false, 20.0, 2.0, 1000.0), 50.0);
        assert_eq!(rung_risk_at("xyz:SOXL", 0, true, 20.0, 2.0, 1000.0), 20.0);   // a shut-hours bid
        assert_eq!(rung_risk_at("xyz:SOXL", 1, false, 20.0, 2.0, 1000.0), 20.0);  // the ladder
        assert_eq!(rung_risk_at("xyz:SOXL", ASK, false, 20.0, 2.0, 1000.0), 20.0);
        assert_eq!(rung_risk_at("ZEC", 0, false, 20.0, 2.0, 1000.0), 20.0);       // a crypto bid
        // the family's sizing off (the $ test): the $ x2.5
        assert_eq!(rung_risk_at("xyz:SOXL", 0, false, 20.0, 0.0, 1000.0), 50.0);
    }

    #[test]
    fn the_crash_insurance() {
        assert!(is_hot(Some(0.026), Some(0.01), 2.5));
        assert!(!is_hot(Some(0.024), Some(0.01), 2.5));
        assert!(!is_hot(None, Some(0.01), 2.5) && !is_hot(Some(0.05), None, 2.5));     // unknown: bid
        assert!(!is_hot(Some(0.05), Some(0.01), 0.0));                                // off
        // on the closes: a calm day, then an hour swinging 3% a minute
        let mut c = Closes::default();
        for m in 0..1440 { c.update("X", m, 100.0 * (1.0 + 0.001 * ((m % 2) as f64)), 2000); }
        for m in 1440..1500 { c.update("X", m, 100.0 * (1.0 + 0.03 * ((m % 2) as f64)), 2000); }
        let (h, d) = (c.spread("X", 1499, 60, 30), c.spread("X", 1499, 1440, 600));
        assert!(is_hot(h, d, 2.5), "{h:?} {d:?}");
        assert!(!is_hot(c.spread("X", 1400, 60, 30), c.spread("X", 1400, 1440, 600), 2.5));
    }

    #[test]
    fn the_shut_hours_bids() {
        let p = Params::from_env();
        // Sat 2026-10-31 00:10 UTC; Wed 22:00 EDT (Thu 02:00 UTC); Thu 11:00 EDT (15:00 UTC, the session)
        let (sat, night, sess) = (29_890_090, 29_887_320, 29_888_100);
        assert_eq!(p.shut_pct("xyz:NVDA", sat), Some(0.025));
        assert_eq!(p.shut_pct("xyz:NVDA", night), Some(0.03));
        assert_eq!(p.shut_pct("xyz:NVDA", sess), None);
        // Sunday night: Sun 2026-11-01 19:30 EST (Mon 00:30 UTC), Mon 00:30 EST (05:30 UTC), Mon 2026-06-01 03:59 UTC (Sun
        // 23:59 EDT); not the UTC weekend (Sun 23:30 UTC), not Mon 04:00 EST (09:00 UTC, the pre-market)
        assert_eq!(p.shut_pct("xyz:NVDA", 29_892_990), Some(0.03));
        assert_eq!(p.shut_pct("xyz:NVDA", 29_893_290), Some(0.03));
        assert!(crate::wkd::us_sunday_night(29_892_990) && crate::wkd::us_sunday_night(29_893_290) && crate::wkd::us_sunday_night(29_671_439));
        assert!(!crate::wkd::us_sunday_night(29_892_930) && !crate::wkd::us_sunday_night(29_893_500) && !crate::wkd::us_night(29_893_290));
        assert_eq!(p.shut_pct("xyz:SKHX", night), None);       // Seoul open then
        assert_eq!(p.shut_pct("xyz:GOLD", sat), None);         // not a stock
        assert_eq!(p.shut_pct("BTC", sat), None);
        let (take, stop, hold) = p.exits("xyz:NVDA", true, 0.03);
        assert!((take - 0.03).abs() < 1e-12 && (stop - 0.075).abs() < 1e-12 && (hold - 86_400.0).abs() < 1e-9);
        assert_eq!(p.exits("xyz:NVDA", false, 0.04), (p.h3_take, p.stop, p.hold_s));
        assert_eq!(p.exits("ETH", false, 0.07), (p.take, p.stop, p.hold_s));
        // the crypto ladder: 10% under on the 10 most restless, the crypto exits (take 5%, stop 20%, 60 min), 3 slots
        assert!((p.cry_deep_pct - 0.10).abs() < 1e-12 && p.cry_deep_coins == 10 && p.cry_deep_max_open == 3);
        let (take, stop, hold) = p.exits("SOL", false, p.cry_deep_pct);
        assert!((take - 0.05).abs() < 1e-12 && (stop - 0.20).abs() < 1e-12 && (hold - 3600.0).abs() < 1e-9);
        // the HIP-3 asks: take = the level, stop 20%, 240 min shut / 60 min open; the non-equity shut bids 3%, 240 min
        assert_eq!(p.rung_exits("xyz:NVDA", ASK, true, p.h3_ask_shut_pct), (0.04, 0.20, 14_400.0));
        assert_eq!(p.rung_exits("xyz:NVDA", ASK, false, p.h3_ask_open_pct), (0.07, 0.20, 3_600.0));
        assert_eq!(p.rung_exits("xyz:GOLD", 0, true, p.noneq_shut_pct), (0.03, 0.20, 14_400.0));
        assert!(crate::wkd::non_equity("xyz:GOLD") && crate::wkd::asian_stock("xyz:SMSN") && !crate::wkd::non_equity("xyz:NVDA"));
        // the asks: 5% over on the 5 most restless, take 3%, stop 10%, 240 min, 2 slots; the HIP-3 session ladder 7%, take = its level
        assert!((p.ask_pct - 0.05).abs() < 1e-12 && p.ask_coins == 5 && p.ask_max_open == 2);
        assert_eq!(p.rung_exits("SOL", ASK, false, p.ask_pct), (0.03, 0.10, 14_400.0));
        assert!((p.h3_session_deep_pct - 0.07).abs() < 1e-12);
        let (take, stop, hold) = p.rung_exits("xyz:NVDA", 1, false, p.h3_session_deep_pct);
        assert!((take - 0.07).abs() < 1e-12 && (stop - 0.20).abs() < 1e-12 && (hold - 3600.0).abs() < 1e-9);
        assert_eq!(p.rung_exits("xyz:NVDA", 1, true, p.deep_pct).0, p.deep_pct);
        // the crypto ladder's third rung: 12% under, take 6%, 3 slots
        assert!((p.cry_deep3_pct - 0.12).abs() < 1e-12 && (p.cry_deep3_take - 0.06).abs() < 1e-12 && p.cry_deep3_max_open == 3);
        assert_eq!(KINDS, ["wick", "wick2", "wick3", "wicka", "wickm", "wickl", "wickl2"]);
        assert_eq!(p.rung_exits("WIF", LOW2, false, p.lowvol_deep_pct), (0.05, 0.20, 3_600.0));
        assert!((p.lowvol_deep_pct - 0.12).abs() < 1e-12 && (p.margin_budget_pct - 95.0).abs() < 1e-12);
        // the budget's order: the HIP-3 shallow bids first, the low-volume ladder last
        let order = [(0, true), (ASK, false), (ASK, true), (LOW, false), (1, false), (MAJ, false), (2, false), (1, true), (0, false), (LOW2, false)];
        assert!(order.windows(2).all(|w| priority(w[0].0, w[0].1) > priority(w[1].0, w[1].1)));
        assert!(priority(0, false) > THIN_PRIORITY && THIN_PRIORITY > priority(LOW2, false));
        assert!((p.thin_min_vol - 0.3e6).abs() < 1e-6 && p.thin_coins == 20);
        assert_eq!(p.rung_exits("WIF", LOW, false, p.lowvol_pct), (0.05, 0.20, 3_600.0));
        assert!((p.lowvol_pct - 0.07).abs() < 1e-12 && p.lowvol_coins == 20 && p.lowvol_max_open == 3);
        assert_eq!(p.rung_exits("SOL", MAJ, false, p.maj_pct), (0.04, 0.20, 14_400.0));
        // the ladder's deeper bid: 5% under, take 5%, stop 12.5%, a day, 3 slots
        assert!((p.deep_pct - 0.05).abs() < 1e-12 && p.deep_max_open == 3);
        let (take, stop, hold) = p.exits("xyz:NVDA", true, p.deep_pct);
        assert!((take - 0.05).abs() < 1e-12 && (stop - 0.125).abs() < 1e-12 && (hold - 86_400.0).abs() < 1e-9);
    }

    #[test]
    fn levels_drift_and_the_pick() {
        // with no requests to spare only a bid the level fell past is cancelled (a bid under it, an ask over it)
        assert!(level_past(false, 92.0, 91.7));
        assert!(!level_past(false, 92.0, 94.0));
        assert!(level_past(true, 108.0, 108.5));
        assert!(!level_past(true, 108.0, 107.0));
        assert!((level(100.0, 0.08) - 92.0).abs() < 1e-12);
        assert!(!drifted(92.0, 93.0, 0.02, 0.002));      // up 1.1%: stays
        assert!(drifted(92.0, 94.0, 0.02, 0.002));       // up 2.2%: moves
        assert!(!drifted(92.0, 91.9, 0.02, 0.002));      // down 0.1%: stays
        assert!(drifted(92.0, 91.7, 0.02, 0.002));       // down 0.3%: moves at once
        assert!(drifted(186.24, 179.7, 0.02, 0.002));    // the MRNA case: the level 3.5% under the bid
        assert!(drifted(0.0, 94.0, 0.02, 0.002));
        let vol: HashMap<String, f64> = [("A".into(), 50e6), ("B".into(), 20e6), ("C".into(), 5e6), ("hyna:X".into(), 90e6)].into();
        assert_eq!(pick(&vol, 10e6, 5, |c| c.starts_with("hyna:"), |_| None), vec!["A".to_string(), "B".to_string()]);
        assert_eq!(pick(&vol, 10e6, 1, |_| false, |_| None), vec!["hyna:X".to_string()]);
        // By the score first (B the more restless), the coins without one after by volume.
        let score = |c: &str| match c { "B" => Some(0.02), "hyna:X" => Some(0.01), _ => None };
        assert_eq!(pick(&vol, 10e6, 3, |_| false, score), vec!["B".to_string(), "hyna:X".to_string(), "A".to_string()]);
    }

    #[test]
    fn the_deviation_and_the_candles() {
        let mut cl = Closes::default();
        // 400 minutes alternating 100 / 101: returns +1% / -0.99%.
        let older: Vec<(i64, f64)> = (0..400).map(|m| (m, if m % 2 == 0 { 100.0 } else { 101.0 })).collect();
        cl.update("A", 400, 100.0, HIST);
        cl.backfill("A", &older, HIST);
        assert_eq!(cl.0["A"].len(), 401);
        let sd = cl.spread("A", 400, 1440, MIN_HIST).unwrap();
        assert!((sd - 0.00995).abs() < 1e-4, "{sd}");
        assert!(cl.spread("A", 400, 100, MIN_HIST).is_none());       // under 360 returns in the window
        cl.backfill("A", &[(5, 1.0)], HIST);                            // not older than what it has: left out
        assert_eq!(cl.0["A"].front(), Some(&(0, 100.0)));
        assert_eq!(dex_of("xyz:TSLA"), "xyz");
        assert_eq!(dex_of("SOL"), "");
        let m = message(&json!({"what": "close", "coin": "SOL", "why": "take", "entry_px": 92.0, "px": 96.6, "ret_pct": 5.0, "pnl_usd": 5.0})).unwrap();
        assert!(m.contains("ТЕЙК") && m.contains("+$5.00"));
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::*;

    #[test]
    fn a_cancelled_bid_is_settled_on_its_status() {
        assert_eq!(after_cancel(&Ok((2.0, false))), AfterCancel::Filled(2.0));
        assert_eq!(after_cancel(&Ok((0.5, true))), AfterCancel::Filled(0.5));       // in part, the rest still resting
        assert_eq!(after_cancel(&Ok((0.0, false))), AfterCancel::Gone);
        assert_eq!(after_cancel(&Ok((0.0, true))), AfterCancel::Keep);            // the HIMS case: the status behind
        assert_eq!(after_cancel(&Err(anyhow::anyhow!("no status"))), AfterCancel::Keep);
    }
}
