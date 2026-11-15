//! External signals: calls from outside the market's own data (a Telegram channel's, parsed elsewhere) put in the
//! inbox (`store::push_signal`: the table `ext_signals`, or `ext_signals.jsonl` on files; `hl-copytrader signal JSON`
//! from a shell), each one decided on and traded like any strategy's (`EXT_ENABLED`: on unless "0").
//!
//! A signal (JSON): `source` (the channel), `id` (its message: each once), `coin` (the channel's ticker: "PEPE",
//! "1000PEPE", "BTCUSDT" are read as HL's "kPEPE", "BTC"), `side` ("long" / "short"), `at` (the message's time, s);
//! optional: `level` with `tf` ("5m", "1h", "4h": the entry once a candle of `tf` closes beyond the level; without a
//! level, the entry at once), `stop`, `take` (the channel's: kept with the signal; the trade's are `Params`), `text`.
//!
//! Each signal is: opened (`decide`: a fresh one at once; a level's on the first close beyond it, if the price has not
//! run more than `chase` past it), waited for (a level not closed beyond yet), or dropped with its reason (not on HL,
//! stale, expired, ran away, the family full). The trade: a market order with the stop and the take resting, out after
//! `hold_s` (V418: the channel followed by these rules: stop 3% take 6% 3 days, +144 bp a trade, t 3.5, both halves;
//! 22 of 140 calls not on HL). `Params::RESEARCH.live` false: the trades on paper only (the events, the result at the
//! mid), until the user's OK.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::now;
use crate::config::Config;
use crate::keeper::{HeldIn, Keeper};
use crate::log;
use crate::market::Candle;
use crate::notify::usd;
use crate::store::Store;
use crate::trades::{self, Trade};

const KIND: &str = "ext";
const H: f64 = 3600.0;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Params {
    /// Real orders (false: paper: the decisions and the results at the mid only).
    pub(crate) live: bool,
    pub(crate) stop: f64,
    pub(crate) take: f64,
    pub(crate) hold_s: f64,
    /// A signal without a level older than this at its reading: dropped (stale).
    pub(crate) max_age_s: f64,
    /// A level's signal not closed beyond in this long: dropped (expired).
    pub(crate) wait_s: f64,
    /// The price past the level (a fraction of it) beyond which the entry is not chased.
    pub(crate) chase: f64,
    pub(crate) max_open: usize,
    /// The risk a trade: this share of the family's.
    pub(crate) share: f64,
    /// The risk a trade without the family's sizing.
    pub(crate) risk_usd: f64,
}

impl Params {
    /// The research's choice (V418, the channel followed: stop 3%, take 6%, 3 days; the level's close within 7 days).
    pub(crate) const RESEARCH: Params = Params { live: false, stop: 0.03, take: 0.06, hold_s: 72.0 * H, max_age_s: H, wait_s: 7.0 * 86_400.0,
        chase: 0.02, max_open: 3, share: 1.0, risk_usd: 20.0 };
}

/// A signal as read from the inbox.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Signal {
    pub(crate) source: String,
    pub(crate) id: String,
    /// HL's coin.
    pub(crate) coin: String,
    /// +1 long, -1 short.
    pub(crate) dir: f64,
    pub(crate) at: f64,
    pub(crate) level: Option<f64>,
    pub(crate) tf: Option<String>,
    pub(crate) stop: Option<f64>,
    pub(crate) take: Option<f64>,
}

/// The channel's ticker as HL's coin (`known`: HL's coins): "BTCUSDT" / "BTC-USDT" / "BTC/USDT" / "BTC.P" -> "BTC";
/// "1000PEPE" or "PEPE" -> "kPEPE" when HL lists the coin by the thousand. None: not on HL.
pub(crate) fn hl_coin(sym: &str, known: &dyn Fn(&str) -> bool) -> Option<String> {
    let mut s = sym.trim().trim_start_matches('$').to_uppercase();
    for tail in [".P", "PERP", "USDT", "USDC", "USD"] {
        if s.len() > tail.len() && s.ends_with(tail) {
            s.truncate(s.len() - tail.len());
        }
        s = s.trim_end_matches(['-', '/', '_']).to_string();
    }
    let k = |x: &str| format!("k{x}");
    let base = s.strip_prefix("1000").filter(|b| !b.is_empty()).map(str::to_string);
    let cands: Vec<String> = match &base {
        Some(b) => vec![k(b), b.clone(), s.clone()],
        None => vec![s.clone(), k(&s)],
    };
    cands.into_iter().find(|c| known(c))
}

/// A signal from the inbox's JSON (`known`: HL's coins); Err: why it is not one.
pub(crate) fn parse(v: &Value, known: &dyn Fn(&str) -> bool) -> std::result::Result<Signal, &'static str> {
    let num = |k: &str| v[k].as_f64().or_else(|| v[k].as_str().and_then(|x| x.trim().parse().ok())).filter(|x| x.is_finite() && *x > 0.0);
    let sym = v["coin"].as_str().ok_or("no coin")?;
    let dir = match v["side"].as_str().map(|s| s.trim().to_lowercase()).as_deref() {
        Some("long" | "buy" | "лонг") => 1.0,
        Some("short" | "sell" | "шорт") => -1.0,
        _ => return Err("no side"),
    };
    let coin = hl_coin(sym, known).ok_or("not on HL")?;
    let tf = v["tf"].as_str().map(str::to_string).filter(|t| crate::market::step(t) > 0.0);
    let level = num("level");
    if level.is_some() && tf.is_none() {
        return Err("a level without its timeframe");
    }
    Ok(Signal { source: v["source"].as_str().unwrap_or("").to_string(), id: match &v["id"] { Value::String(s) => s.clone(), x => x.to_string() },
        coin, dir, at: num("at").ok_or("no time")?, level, tf, stop: num("stop"), take: num("take") })
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Decision {
    Open,
    Wait,
    Drop(&'static str),
}

/// What to do with a signal at `t` with the coin's `mid` and its `tf` candles closed since the signal (oldest first).
pub(crate) fn decide(s: &Signal, p: &Params, t: f64, mid: f64, closed: &[Candle]) -> Decision {
    match s.level {
        None if t - s.at > p.max_age_s => Decision::Drop("stale"),
        None => Decision::Open,
        Some(_) if t - s.at > p.wait_s => Decision::Drop("expired"),
        Some(l) => {
            let step = s.tf.as_deref().map(crate::market::step).unwrap_or(0.0);
            let crossed = closed.iter().any(|c| c.t + step > s.at && s.dir * (c.c - l) > 0.0);
            if !crossed {
                Decision::Wait
            } else if s.dir * (mid / l - 1.0) > p.chase {
                Decision::Drop("ran away")
            } else {
                Decision::Open
            }
        }
    }
}

/// A paper trade's end at the mid `px` at `t`: its stop, take or time.
pub(crate) fn paper_out(tr: &Trade, px: f64, t: f64) -> Option<&'static str> {
    let d = tr.dir();
    if d * (px - tr.stop_px) <= 0.0 {
        Some("stop")
    } else if tr.take_px > 0.0 && d * (px - tr.take_px) >= 0.0 {
        Some("take")
    } else if t >= tr.exit_at {
        Some("time")
    } else {
        None
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Pos {
    pub(crate) tr: Trade,
    pub(crate) source: String,
    pub(crate) id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct State {
    /// The inbox read up to this number.
    pub(crate) seen: i64,
    /// Signals waiting for their level.
    pub(crate) pending: Vec<Signal>,
    pub(crate) open: Vec<Pos>,
    pub(crate) paper: Vec<Pos>,
    pub(crate) opened: u32,
    pub(crate) paper_pnl_r: f64,
}

pub(crate) fn message(ev: &Value) -> Option<String> {
    let f = |k: &str| ev[k].as_f64().unwrap_or(0.0);
    let (coin, src) = (ev["coin"].as_str().unwrap_or(""), ev["source"].as_str().unwrap_or(""));
    let side = |d: f64| if d > 0.0 { "ЛОНГ" } else { "ШОРТ" };
    Some(match ev["what"].as_str()? {
        "open" => format!("📡 {src}: {} {coin} по {}, на ${:.0}; стоп {}, тейк {}, выход через {:.0} ч", side(f("dir")), f("px"), f("notional"), f("stop_px"), f("take_px"), f("hold_h")),
        "paper_open" => format!("📝 {src}: {} {coin} по {} (на бумаге); стоп {}, тейк {}", side(f("dir")), f("px"), f("stop_px"), f("take_px")),
        "drop" => format!("📡 {src}: {coin} {} — не берём: {}", ev["side"].as_str().unwrap_or(""), match ev["why"].as_str().unwrap_or("") {
            "stale" => "сигнал устарел", "expired" => "уровень не пробит за 7 дней", "ran away" => "цена ушла от уровня", "full" => "нет места (лимиты)",
            "not on HL" => "монеты нет на HL", w => w }),
        "close" | "paper_close" => format!("{} {src} {coin}{}: {} -> {} ({:+.2}%{})", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            if ev["what"] == "paper_close" { " (бумага)" } else { "" }, f("entry_px"), f("px"), f("ret_pct"),
            ev["pnl_usd"].as_f64().map(|x| format!(", {}", usd(x))).unwrap_or_default()),
        "error" => format!("⚠️ внешний сигнал {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
        _ => return None,
    })
}

pub(crate) struct Ext {
    pub(crate) k: Keeper,
    pub(crate) p: Params,
    pub(crate) state: State,
}

impl Ext {
    fn save(&self) {
        self.k.save(KIND, &self.state);
    }

    /// The inbox's new signals read: each parsed (a bad one dropped, told) and queued.
    async fn read_inbox(&mut self) -> bool {
        let got = match crate::store::signals_after(&self.k.cfg, self.state.seen).await {
            Ok(g) => g,
            Err(e) => {
                log!("ext: the inbox not read: {e:#}");
                return false;
            }
        };
        let known = |c: &str| self.k.venue.coins.contains_key(c);
        for (seq, v) in &got {
            self.state.seen = *seq;
            match parse(v, &known) {
                Ok(s) => {
                    self.k.event(json!({"what": "received", "source": s.source, "id": s.id, "coin": s.coin, "dir": s.dir, "level": s.level, "tf": s.tf,
                        "signal_stop": s.stop, "signal_take": s.take, "signal_at": s.at}));
                    if !self.state.pending.iter().any(|x| x.source == s.source && x.id == s.id) {
                        self.state.pending.push(s);
                    }
                }
                Err(why) => self.k.event(json!({"what": "drop", "why": why, "source": v["source"], "id": v["id"], "coin": v["coin"], "side": v["side"]})),
            }
        }
        !got.is_empty()
    }

    /// The pending signals decided on at `t`: opened, dropped or left waiting.
    async fn work(&mut self, t: f64) -> bool {
        if self.state.pending.is_empty() {
            return false;
        }
        let mids = match crate::market::mids(&self.k.venue.ex, "").await {
            Ok(m) => m,
            Err(e) => {
                log!("ext: mids not read: {e:#}");
                return false;
            }
        };
        let mut changed = false;
        for s in std::mem::take(&mut self.state.pending) {
            let mid = mids.get(&s.coin).copied().unwrap_or(0.0);
            let closed = match (&s.level, &s.tf) {
                (Some(_), Some(tf)) => match crate::market::candles(&self.k.venue.ex, &s.coin, tf, s.at - crate::market::step(tf)).await {
                    Ok(v) => crate::market::closed(v, tf, t),
                    Err(e) => {
                        log!("ext: {} candles not read: {e:#}", s.coin);
                        self.state.pending.push(s);
                        continue;
                    }
                },
                _ => Vec::new(),
            };
            let d = if mid > 0.0 { decide(&s, &self.p, t, mid, &closed) } else { Decision::Drop("no price") };
            match d {
                Decision::Wait => self.state.pending.push(s),
                Decision::Drop(why) => {
                    self.drop(&s, why);
                    changed = true;
                }
                Decision::Open => {
                    self.open(&s, mid).await;
                    changed = true;
                }
            }
        }
        changed
    }

    fn drop(&self, s: &Signal, why: &str) {
        self.k.event(json!({"what": "drop", "why": why, "source": s.source, "id": s.id, "coin": s.coin, "side": if s.dir > 0.0 { "long" } else { "short" }}));
    }

    /// The signal traded at `mid`: live within the family's limits, or on paper.
    async fn open(&mut self, s: &Signal, mid: f64) {
        let p = self.p.clone();
        if !p.live {
            let tr = Trade { coin: s.coin.clone(), size: s.dir, entry_px: mid, at: now(), exit_at: now() + p.hold_s, stop_px: mid * (1.0 - s.dir * p.stop),
                stop_oid: None, take_px: mid * (1.0 + s.dir * p.take), take_oid: None };
            self.k.event(json!({"what": "paper_open", "source": s.source, "id": s.id, "coin": s.coin, "dir": s.dir, "px": mid, "stop_px": tr.stop_px, "take_px": tr.take_px}));
            self.state.paper.push(Pos { tr, source: s.source.clone(), id: s.id.clone() });
            return;
        }
        let risk = trades::risk_usd(p.risk_usd) * p.share;
        if self.state.open.len() >= p.max_open || self.state.open.iter().any(|x| x.tr.coin == s.coin) || !trades::can_open(&s.coin, KIND, s.dir, risk) {
            return self.drop(s, "full");
        }
        let size = trades::size_for(risk, p.stop, mid) * s.dir;
        match trades::open_market_at(&self.k.venue, &s.coin, size, p.stop, Some(p.take), p.hold_s, mid).await {
            Ok(tr) => {
                self.k.event(json!({"what": "open", "source": s.source, "id": s.id, "coin": s.coin, "dir": s.dir, "px": tr.entry_px, "mid": mid, "size": tr.size,
                    "notional": (tr.size * tr.entry_px).abs(), "stop_px": tr.stop_px, "take_px": tr.take_px, "hold_h": p.hold_s / H}));
                trades::register(&s.coin, KIND, tr.risk(), tr.size);
                self.state.open.push(Pos { tr, source: s.source.clone(), id: s.id.clone() });
                self.state.opened += 1;
            }
            Err(e) => self.k.event(json!({"what": "error", "coin": s.coin, "error": format!("{e:#}")})),
        }
    }

    /// The paper trades' ends at the mids.
    async fn check_paper(&mut self) -> bool {
        if self.state.paper.is_empty() {
            return false;
        }
        let Ok(mids) = crate::market::mids(&self.k.venue.ex, "").await else { return false };
        let t = now();
        let mut changed = false;
        let p = self.p.clone();
        let mut keep = Vec::new();
        for x in std::mem::take(&mut self.state.paper) {
            let px = mids.get(&x.tr.coin).copied().unwrap_or(0.0);
            match (px > 0.0).then(|| paper_out(&x.tr, px, t)).flatten() {
                Some(why) => {
                    let ret = x.tr.ret(px);
                    self.state.paper_pnl_r += ret / p.stop;
                    self.k.event(json!({"what": "paper_close", "source": x.source, "id": x.id, "coin": x.tr.coin, "why": why, "entry_px": x.tr.entry_px, "px": px,
                        "ret_pct": ret * 100.0, "r": ret / p.stop, "held_h": (t - x.tr.at) / H}));
                    changed = true;
                }
                None => keep.push(x),
            }
        }
        self.state.paper = keep;
        changed
    }
}

impl crate::runner::Strategy for Ext {
    fn schedule(&self) -> crate::runner::Schedule {
        // the inbox every 30 s, the trades every minute
        crate::runner::Schedule { tick_s: 10.0, step: crate::runner::Due::Every(30.0), check_s: 60.0 }
    }

    async fn step(&mut self, at: f64) {
        let a = self.read_inbox().await;
        let b = self.work(at).await;
        if a || b {
            self.save();
        }
    }

    async fn check(&mut self) {
        let a = self.k.sweep(&mut self.state.open, |p| &p.tr, true, true).await;
        let b = self.check_paper().await;
        if a || b {
            self.save();
        }
    }
}

/// Starts the external signals unless `EXT_ENABLED` is "0".
pub async fn start(cfg: &Config, store: Store) -> Result<()> {
    if crate::runner::off(KIND, "EXT_ENABLED") {
        return Ok(());
    }
    let Some((venue, unified)) = crate::runner::venue(KIND).await? else {
        return Ok(());
    };
    let p = Params::RESEARCH;
    let state: State = crate::store::load_state(cfg, KIND).await?;
    for x in &state.open {
        trades::register(&x.tr.coin, KIND, x.tr.risk(), x.tr.size);
    }
    log!("ext: {}; stop {:.0}% take {:.0}% {:.0} h, a level's close within {:.0} d, chase {:.0}%, at most {}; {} pending, {} open, {} on paper (paper {:+.1} R)",
        if p.live { "live" } else { "paper" }, p.stop * 100.0, p.take * 100.0, p.hold_s / H, p.wait_s / 86_400.0, p.chase * 100.0, p.max_open,
        state.pending.len(), state.open.len(), state.paper.len(), state.paper_pnl_r);
    crate::notify::send(format!("📡 внешние сигналы {}: стоп {:.0}%, тейк {:.0}%, выход через {:.0} ч; вход сразу или по закрытию за уровнем (до {:.0} дн., не дальше {:.0}% от уровня), до {} позиций",
        if p.live { "НА ДЕНЬГИ" } else { "НА БУМАГЕ (без сделок)" }, p.stop * 100.0, p.take * 100.0, p.hold_s / H, p.wait_s / 86_400.0, p.chase * 100.0, p.max_open));
    let x = Ext { k: Keeper::new(venue, unified, cfg.clone(), store, KIND, message, HeldIn::Hours), p, state };
    crate::runner::spawn(x);
    Ok(())
}

/// Puts a signal (JSON) in the inbox from a shell: `hl-copytrader signal '{"source": "tg", "id": 1, ...}'`.
pub async fn push(cfg: &Config, json_text: &str) -> Result<()> {
    let v: Value = serde_json::from_str(json_text)?;
    let fresh = crate::store::push_signal(cfg, &v).await?;
    log!("ext: signal {}", if fresh { "put in the inbox" } else { "already there" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(c: &str) -> bool {
        ["BTC", "kPEPE", "SOL", "XAA"].contains(&c)
    }

    fn c(t: f64, close: f64) -> Candle {
        Candle { t, o: close, h: close, l: close, c: close, v: 1.0 }
    }

    #[test]
    fn the_channel_s_tickers_as_hl_s_coins() {
        assert_eq!(hl_coin("BTCUSDT", &known).as_deref(), Some("BTC"));
        assert_eq!(hl_coin("btc-usdt", &known).as_deref(), Some("BTC"));
        assert_eq!(hl_coin("$SOL", &known).as_deref(), Some("SOL"));
        assert_eq!(hl_coin("SOL/USDT", &known).as_deref(), Some("SOL"));
        assert_eq!(hl_coin("SOLUSDT.P", &known).as_deref(), Some("SOL"));
        assert_eq!(hl_coin("1000PEPE", &known).as_deref(), Some("kPEPE"));
        assert_eq!(hl_coin("PEPE", &known).as_deref(), Some("kPEPE"));
        assert_eq!(hl_coin("ZEST", &known), None);
        assert_eq!(hl_coin("1000", &known), None);
    }

    #[test]
    fn a_signal_read_and_the_bad_ones_told_why() {
        let v = json!({"source": "jt", "id": 114, "coin": "BTCUSDT", "side": "Long", "at": 1000.0, "level": "0.8563", "tf": "5m", "stop": 0.8});
        let s = parse(&v, &known).unwrap();
        assert_eq!(s, Signal { source: "jt".into(), id: "114".into(), coin: "BTC".into(), dir: 1.0, at: 1000.0, level: Some(0.8563), tf: Some("5m".into()), stop: Some(0.8), take: None });
        assert_eq!(parse(&json!({"source": "jt", "id": "a", "coin": "SOL", "side": "шорт", "at": 5}), &known).unwrap().dir, -1.0);
        assert_eq!(parse(&json!({"coin": "SOL", "at": 5}), &known), Err("no side"));
        assert_eq!(parse(&json!({"side": "long", "at": 5}), &known), Err("no coin"));
        assert_eq!(parse(&json!({"coin": "ZEST", "side": "long", "at": 5}), &known), Err("not on HL"));
        assert_eq!(parse(&json!({"coin": "SOL", "side": "long"}), &known), Err("no time"));
        assert_eq!(parse(&json!({"coin": "SOL", "side": "long", "at": 5, "level": 1.0}), &known), Err("a level without its timeframe"));
    }

    #[test]
    fn decided_at_once_on_a_close_beyond_the_level_or_dropped() {
        let p = Params::RESEARCH;
        let now_sig = Signal { source: "jt".into(), id: "1".into(), coin: "SOL".into(), dir: 1.0, at: 10_000.0, level: None, tf: None, stop: None, take: None };
        assert_eq!(decide(&now_sig, &p, 10_060.0, 100.0, &[]), Decision::Open);
        assert_eq!(decide(&now_sig, &p, 10_000.0 + H + 1.0, 100.0, &[]), Decision::Drop("stale"));
        let lv = Signal { level: Some(100.0), tf: Some("1h".into()), ..now_sig.clone() };
        // the candle closed before the signal does not count; one after, under the level: wait; over it: open
        assert_eq!(decide(&lv, &p, 20_000.0, 100.5, &[c(10_000.0 - 2.0 * H, 101.0)]), Decision::Wait);
        assert_eq!(decide(&lv, &p, 20_000.0, 99.0, &[c(10_800.0, 99.5)]), Decision::Wait);
        assert_eq!(decide(&lv, &p, 20_000.0, 101.0, &[c(10_800.0, 99.5), c(14_400.0, 100.4)]), Decision::Open);
        // the candle the signal came in, closing after it, counts
        assert_eq!(decide(&lv, &p, 20_000.0, 100.5, &[c(7_200.0, 100.4)]), Decision::Open);
        assert_eq!(decide(&lv, &p, 20_000.0, 102.5, &[c(14_400.0, 100.4)]), Decision::Drop("ran away"));
        assert_eq!(decide(&lv, &p, 10_000.0 + 7.0 * 86_400.0 + 1.0, 100.5, &[c(14_400.0, 100.4)]), Decision::Drop("expired"));
        // a short: a close under its level
        let sh = Signal { dir: -1.0, ..lv.clone() };
        assert_eq!(decide(&sh, &p, 20_000.0, 99.0, &[c(14_400.0, 99.6)]), Decision::Open);
        assert_eq!(decide(&sh, &p, 20_000.0, 97.0, &[c(14_400.0, 99.6)]), Decision::Drop("ran away"));
    }

    #[test]
    fn a_paper_trade_ends_on_its_stop_take_or_time() {
        let tr = Trade { coin: "SOL".into(), size: -1.0, entry_px: 100.0, at: 0.0, exit_at: 100.0, stop_px: 103.0, take_px: 94.0, ..Default::default() };
        assert_eq!(paper_out(&tr, 103.0, 1.0), Some("stop"));
        assert_eq!(paper_out(&tr, 93.0, 1.0), Some("take"));
        assert_eq!(paper_out(&tr, 99.0, 1.0), None);
        assert_eq!(paper_out(&tr, 99.0, 100.0), Some("time"));
    }

    #[test]
    fn the_research_config() {
        // V418: the channel followed, stop 3% take 6% 3 days; paper until the user's OK
        let p = Params::RESEARCH;
        assert_eq!((p.live, p.stop, p.take, p.hold_s, p.wait_s, p.max_open), (false, 0.03, 0.06, 72.0 * 3600.0, 7.0 * 86_400.0, 3));
        assert!(message(&json!({"what": "drop", "why": "stale", "coin": "SOL", "source": "jt", "side": "long"})).unwrap().contains("устарел"));
        assert!(message(&json!({"what": "paper_close", "coin": "SOL", "source": "jt", "ret_pct": 6.0, "entry_px": 1.0, "px": 1.06})).unwrap().contains("бумага"));
        assert!(message(&json!({"what": "received"})).is_none());
    }
}

#[cfg(test)]
mod lifecycle {
    use super::*;
    use crate::testkit::{FakeHl, cfg};

    async fn setup(coin: &str, live: bool) -> (FakeHl, Ext, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        let f = FakeHl::start(&[(coin, 2, 10, 100.0)]).await;
        let (store, rx) = Store::for_test();
        let x = Ext { k: Keeper::new(f.venue(), false, cfg(), store, KIND, message, HeldIn::Hours), p: Params { live, ..Params::RESEARCH }, state: State::default() };
        (f, x, rx)
    }

    fn whats(ev: &[Value]) -> Vec<String> {
        ev.iter().map(|e| e["what"].as_str().unwrap_or("").to_string()).collect()
    }

    #[tokio::test]
    async fn a_fresh_call_is_bought_with_its_stop_and_take_then_its_take_ends_it() {
        use crate::runner::Strategy;
        let (f, mut x, mut rx) = setup("EXA", true).await;
        crate::store::push_signal(&x.k.cfg, &json!({"source": "jt", "id": 1, "coin": "EXAUSDT", "side": "long", "at": now() - 60.0})).await.unwrap();
        crate::store::push_signal(&x.k.cfg, &json!({"source": "jt", "id": 2, "coin": "NOPE", "side": "long", "at": now()})).await.unwrap();
        x.step(now()).await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!(whats(&ev), vec!["received", "drop", "open"], "{ev:?}");
        assert_eq!(ev[1]["why"], "not on HL");
        assert!(f.position("EXA") > 0.0 && f.open_orders("EXA").len() == 2 && trades::registered("EXA"));
        let tr = &x.state.open[0].tr;
        assert!((tr.stop_px / tr.entry_px - 0.97).abs() < 1e-3 && (tr.take_px / tr.entry_px - 1.06).abs() < 1e-3);
        assert_eq!((x.state.seen, x.state.opened), (2, 1));
        // read once: the same inbox again brings nothing
        x.step(now()).await;
        assert!(crate::store::drain(&mut rx).is_empty());
        f.set_mid("EXA", 107.0);
        x.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((whats(&ev), ev[0]["why"].as_str()), (vec!["close".to_string()], Some("take")), "{ev:?}");
        assert!(x.state.open.is_empty() && !trades::registered("EXA"));
        assert!(crate::testkit::saved(&x.k.cfg, KIND).await.is_some());
    }

    #[tokio::test]
    async fn a_level_call_waits_for_its_close_and_on_paper_only_is_told_its_result() {
        use crate::runner::Strategy;
        let (f, mut x, mut rx) = setup("EXB", false).await;
        let t0 = (now() / H).floor() * H - 3.0 * H;
        crate::store::push_signal(&x.k.cfg, &json!({"source": "jt", "id": 9, "coin": "EXB", "side": "short", "at": t0 + 60.0, "level": 99.0, "tf": "1h"})).await.unwrap();
        f.set_candles("EXB", "1h", &[(t0, 100.0, 100.0, 100.0, 99.5, 1.0)]);
        x.step(now()).await;
        assert_eq!(whats(&crate::store::drain(&mut rx)), vec!["received"]);
        assert_eq!(x.state.pending.len(), 1);
        f.set_candles("EXB", "1h", &[(t0, 100.0, 100.0, 100.0, 99.5, 1.0), (t0 + H, 99.5, 99.5, 98.0, 98.6, 1.0)]);
        f.set_mid("EXB", 98.5);
        x.step(now()).await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!(whats(&ev), vec!["paper_open"], "{ev:?}");
        assert!(x.state.pending.is_empty() && x.state.paper.len() == 1 && f.position("EXB") == 0.0 && !trades::registered("EXB"));
        f.set_mid("EXB", 98.5 * 1.031);
        x.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((whats(&ev), ev[0]["why"].as_str()), (vec!["paper_close".to_string()], Some("stop")), "{ev:?}");
        assert!((x.state.paper_pnl_r + 1.03).abs() < 0.02, "{}", x.state.paper_pnl_r);
    }

    #[tokio::test]
    async fn a_stale_call_is_dropped_and_a_full_family_takes_none() {
        use crate::runner::Strategy;
        let (f, mut x, mut rx) = setup("EXC", true).await;
        crate::store::push_signal(&x.k.cfg, &json!({"source": "jt", "id": 1, "coin": "EXC", "side": "long", "at": now() - 2.0 * H})).await.unwrap();
        x.step(now()).await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((whats(&ev), ev[1]["why"].as_str()), (vec!["received".to_string(), "drop".to_string()], Some("stale")));
        x.p.max_open = 0;
        crate::store::push_signal(&x.k.cfg, &json!({"source": "jt", "id": 2, "coin": "EXC", "side": "long", "at": now()})).await.unwrap();
        x.step(now()).await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!(ev.last().unwrap()["why"], "full");
        assert_eq!(f.position("EXC"), 0.0);
    }
}
