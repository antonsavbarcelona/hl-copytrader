//! External trades: a trade manager for calls from outside (a Telegram channel's, parsed elsewhere into a trade). It
//! reads nothing and analyses nothing: each trade put in the inbox (`store::push_signal`: the table `ext_signals`, or
//! `ext_signals.jsonl` on files; `hl-copytrader signal JSON` from a shell) is checked against the account (the coin on
//! HL, the stop and take on their sides of the price, the family's limits), sized by the family's risk at its stop,
//! opened at market with its stop and take resting, and kept as any strategy's trade (`EXT_ENABLED`: on unless "0").
//!
//! A trade (JSON): `source` (the channel), `id` (its message: each once), `coin` (the channel's ticker: "PEPE",
//! "1000PEPE", "BTCUSDT" are read as HL's "kPEPE", "BTC"), `side` ("long" / "short"), `at` (the call's time, s);
//! optional: `stop`, `take` (prices; without them `Params::stop` / `take` from the entry), `hold_h` (the time out).
//!
//! Dropped, with the reason told: not on HL, older than `max_age_s` when read, the stop or take already passed, the
//! stop nearer than `min_stop` or farther than `max_stop`, the family full. `Params::RESEARCH.live` false: the trades on
//! paper only (the events, the result at the mid), until the user's OK. The defaults are the research's (V418: the
//! channel followed with a stop 3%, take 6%, 3 days: +144 bp a trade, t 3.5, both halves).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::now;
use crate::config::Config;
use crate::keeper::{HeldIn, Keeper};
use crate::log;
use crate::notify::usd;
use crate::store::Store;
use crate::trades::{self, Trade};

const KIND: &str = "ext";
const H: f64 = 3600.0;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Params {
    /// Real orders (false: paper: the decisions and the results at the mid only).
    pub(crate) live: bool,
    /// The stop and take (fractions from the entry) of a trade without its own.
    pub(crate) stop: f64,
    pub(crate) take: f64,
    /// The time out of a trade without its own.
    pub(crate) hold_s: f64,
    /// A trade older than this when read: dropped (stale).
    pub(crate) max_age_s: f64,
    /// The stop's distance from the price allowed (fractions).
    pub(crate) min_stop: f64,
    pub(crate) max_stop: f64,
    pub(crate) max_open: usize,
    /// The risk a trade: this share of the family's.
    pub(crate) share: f64,
    /// The risk a trade without the family's sizing.
    pub(crate) risk_usd: f64,
}

impl Params {
    /// The research's defaults (V418: stop 3%, take 6%, 3 days); paper until the user's OK.
    pub(crate) const RESEARCH: Params = Params { live: false, stop: 0.03, take: 0.06, hold_s: 72.0 * H, max_age_s: 30.0 * 60.0,
        min_stop: 0.003, max_stop: 0.15, max_open: 3, share: 1.0, risk_usd: 20.0 };
}

/// A trade as read from the inbox.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Signal {
    pub(crate) source: String,
    pub(crate) id: String,
    /// HL's coin.
    pub(crate) coin: String,
    /// +1 long, -1 short.
    pub(crate) dir: f64,
    pub(crate) at: f64,
    pub(crate) stop: Option<f64>,
    pub(crate) take: Option<f64>,
    pub(crate) hold_s: Option<f64>,
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
    let cands: Vec<String> = match s.strip_prefix("1000").filter(|b| !b.is_empty()) {
        Some(b) => vec![k(b), b.to_string(), s.clone()],
        None => vec![s.clone(), k(&s)],
    };
    cands.into_iter().find(|c| known(c))
}

/// A trade from the inbox's JSON (`known`: HL's coins); Err: why it is not one.
pub(crate) fn parse(v: &Value, known: &dyn Fn(&str) -> bool) -> std::result::Result<Signal, &'static str> {
    let num = |k: &str| v[k].as_f64().or_else(|| v[k].as_str().and_then(|x| x.trim().parse().ok())).filter(|x| x.is_finite() && *x > 0.0);
    let sym = v["coin"].as_str().ok_or("no coin")?;
    let dir = match v["side"].as_str().map(|s| s.trim().to_lowercase()).as_deref() {
        Some("long" | "buy" | "лонг") => 1.0,
        Some("short" | "sell" | "шорт") => -1.0,
        _ => return Err("no side"),
    };
    let coin = hl_coin(sym, known).ok_or("not on HL")?;
    Ok(Signal { source: v["source"].as_str().unwrap_or("").to_string(), id: match &v["id"] { Value::String(s) => s.clone(), x => x.to_string() },
        coin, dir, at: num("at").ok_or("no time")?, stop: num("stop"), take: num("take"), hold_s: num("hold_h").map(|h| h * H) })
}

/// The trade to open: its stop and take as fractions from the price, its time out.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Plan {
    pub(crate) stop: f64,
    pub(crate) take: f64,
    pub(crate) hold_s: f64,
}

/// Whether the trade opens at `t` at the price `mid`, and how; Err: why not.
pub(crate) fn decide(s: &Signal, p: &Params, t: f64, mid: f64) -> std::result::Result<Plan, &'static str> {
    if mid <= 0.0 {
        return Err("no price");
    }
    if t - s.at > p.max_age_s {
        return Err("stale");
    }
    // the distances to the stop and the take on the trade's side (> 0: still ahead)
    let stop = s.stop.map_or(p.stop, |x| s.dir * (mid - x) / mid);
    let take = s.take.map_or(p.take, |x| s.dir * (x - mid) / mid);
    if stop <= 0.0 {
        return Err("stop passed");
    }
    if take <= 0.0 {
        return Err("take passed");
    }
    if stop < p.min_stop {
        return Err("stop too close");
    }
    if stop > p.max_stop {
        return Err("stop too far");
    }
    Ok(Plan { stop, take, hold_s: s.hold_s.unwrap_or(p.hold_s) })
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
    /// The stop's distance at the entry (a fraction): the R of a paper result.
    #[serde(default)]
    pub(crate) stop: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct State {
    /// The inbox read up to this number.
    pub(crate) seen: i64,
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
        "drop" => format!("📡 {src}: {} {} — не берём: {}", ev["coin"].as_str().unwrap_or("?"), ev["side"].as_str().unwrap_or(""), match ev["why"].as_str().unwrap_or("") {
            "stale" => "сделка устарела", "stop passed" => "цена уже за стопом", "take passed" => "цена уже за тейком", "stop too close" => "стоп слишком близко",
            "stop too far" => "стоп слишком далеко", "full" => "нет места (лимиты)", "not on HL" => "монеты нет на HL", "no price" => "нет цены", w => w }),
        "close" | "paper_close" => format!("{} {src} {coin}{}: {} -> {} ({:+.2}%{})", if f("ret_pct") >= 0.0 { "✅" } else { "🔻" },
            if ev["what"] == "paper_close" { " (бумага)" } else { "" }, f("entry_px"), f("px"), f("ret_pct"),
            ev["pnl_usd"].as_f64().map(|x| format!(", {}", usd(x))).unwrap_or_default()),
        "error" => format!("⚠️ внешняя сделка {coin}: {}", ev["error"].as_str().unwrap_or("ошибка")),
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

    fn drop(&self, v: &Value, why: &str) {
        self.k.event(json!({"what": "drop", "why": why, "source": v["source"], "id": v["id"], "coin": v["coin"], "side": v["side"]}));
    }

    /// The inbox's new trades, each opened or dropped at `t`.
    async fn read_inbox(&mut self, t: f64) -> bool {
        let got = match crate::store::signals_after(&self.k.cfg, self.state.seen).await {
            Ok(g) => g,
            Err(e) => {
                log!("ext: the inbox not read: {e:#}");
                return false;
            }
        };
        if got.is_empty() {
            return false;
        }
        let mids = match crate::market::mids(&self.k.venue.ex, "").await {
            Ok(m) => m,
            Err(e) => {
                log!("ext: mids not read: {e:#}");
                return false;
            }
        };
        for (seq, v) in &got {
            self.state.seen = *seq;
            let known = |c: &str| self.k.venue.coins.contains_key(c);
            let s = match parse(v, &known) {
                Ok(s) => s,
                Err(why) => {
                    self.drop(v, why);
                    continue;
                }
            };
            let mid = mids.get(&s.coin).copied().unwrap_or(0.0);
            self.k.event(json!({"what": "received", "source": s.source, "id": s.id, "coin": s.coin, "dir": s.dir, "stop": s.stop, "take": s.take, "signal_at": s.at, "mid": mid}));
            match decide(&s, &self.p, t, mid) {
                Ok(plan) => self.open(&s, &plan, mid).await,
                Err(why) => self.drop(&json!({"source": s.source, "id": s.id, "coin": s.coin, "side": if s.dir > 0.0 { "long" } else { "short" }}), why),
            }
        }
        true
    }

    /// The trade opened at `mid` as planned: live within the family's limits, or on paper.
    async fn open(&mut self, s: &Signal, plan: &Plan, mid: f64) {
        let p = self.p.clone();
        let pos = |tr: Trade| Pos { tr, source: s.source.clone(), id: s.id.clone(), stop: plan.stop };
        if !p.live {
            let tr = Trade { coin: s.coin.clone(), size: s.dir, entry_px: mid, at: now(), exit_at: now() + plan.hold_s, stop_px: mid * (1.0 - s.dir * plan.stop),
                stop_oid: None, take_px: mid * (1.0 + s.dir * plan.take), take_oid: None };
            self.k.event(json!({"what": "paper_open", "source": s.source, "id": s.id, "coin": s.coin, "dir": s.dir, "px": mid, "stop_px": tr.stop_px, "take_px": tr.take_px,
                "hold_h": plan.hold_s / H}));
            self.state.paper.push(pos(tr));
            return;
        }
        let risk = trades::risk_usd(p.risk_usd) * p.share;
        if self.state.open.len() >= p.max_open || self.state.open.iter().any(|x| x.tr.coin == s.coin) || !trades::can_open(&s.coin, KIND, s.dir, risk) {
            return self.drop(&json!({"source": s.source, "id": s.id, "coin": s.coin, "side": if s.dir > 0.0 { "long" } else { "short" }}), "full");
        }
        let size = trades::size_for(risk, plan.stop, mid) * s.dir;
        match trades::open_market_at(&self.k.venue, &s.coin, size, plan.stop, Some(plan.take), plan.hold_s, mid).await {
            Ok(tr) => {
                self.k.event(json!({"what": "open", "source": s.source, "id": s.id, "coin": s.coin, "dir": s.dir, "px": tr.entry_px, "mid": mid, "size": tr.size,
                    "notional": (tr.size * tr.entry_px).abs(), "stop_px": tr.stop_px, "take_px": tr.take_px, "hold_h": plan.hold_s / H}));
                trades::register(&s.coin, KIND, tr.risk(), tr.size);
                self.state.open.push(pos(tr));
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
        let mut keep = Vec::new();
        for x in std::mem::take(&mut self.state.paper) {
            let px = mids.get(&x.tr.coin).copied().unwrap_or(0.0);
            match (px > 0.0).then(|| paper_out(&x.tr, px, t)).flatten() {
                Some(why) => {
                    let ret = x.tr.ret(px);
                    let r = if x.stop > 0.0 { ret / x.stop } else { 0.0 };
                    self.state.paper_pnl_r += r;
                    self.k.event(json!({"what": "paper_close", "source": x.source, "id": x.id, "coin": x.tr.coin, "why": why, "entry_px": x.tr.entry_px, "px": px,
                        "ret_pct": ret * 100.0, "r": r, "held_h": (t - x.tr.at) / H}));
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
        // the inbox every 10 s, the trades every minute
        crate::runner::Schedule { tick_s: 10.0, step: crate::runner::Due::Every(0.0), check_s: 60.0 }
    }

    async fn step(&mut self, at: f64) {
        if self.read_inbox(at).await {
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

/// Starts the external trades unless `EXT_ENABLED` is "0".
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
    log!("ext: {}; without its own: stop {:.0}% take {:.0}% {:.0} h; stale after {:.0} min, the stop {:.1}..{:.0}%, at most {}; {} open, {} on paper (paper {:+.1} R)",
        if p.live { "live" } else { "paper" }, p.stop * 100.0, p.take * 100.0, p.hold_s / H, p.max_age_s / 60.0, p.min_stop * 100.0, p.max_stop * 100.0, p.max_open,
        state.open.len(), state.paper.len(), state.paper_pnl_r);
    crate::notify::send(format!("📡 внешние сделки {}: вход по рынку со стопом и тейком сигнала (без них: стоп {:.0}%, тейк {:.0}%, выход через {:.0} ч), до {} позиций",
        if p.live { "НА ДЕНЬГИ" } else { "НА БУМАГЕ (без сделок)" }, p.stop * 100.0, p.take * 100.0, p.hold_s / H, p.max_open));
    let x = Ext { k: Keeper::new(venue, unified, cfg.clone(), store, KIND, message, HeldIn::Hours), p, state };
    crate::runner::spawn(x);
    Ok(())
}

/// Puts a trade (JSON) in the inbox from a shell: `hl-copytrader signal '{"source": "tg", "id": 1, ...}'`.
pub async fn push(cfg: &Config, json_text: &str) -> Result<()> {
    let v: Value = serde_json::from_str(json_text)?;
    let fresh = crate::store::push_signal(cfg, &v).await?;
    log!("ext: trade {}", if fresh { "put in the inbox" } else { "already there" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(c: &str) -> bool {
        ["BTC", "kPEPE", "SOL"].contains(&c)
    }

    fn sig(dir: f64, stop: Option<f64>, take: Option<f64>) -> Signal {
        Signal { source: "jt".into(), id: "1".into(), coin: "SOL".into(), dir, at: 10_000.0, stop, take, hold_s: None }
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
    fn a_trade_read_and_the_bad_ones_told_why() {
        let v = json!({"source": "jt", "id": 114, "coin": "BTCUSDT", "side": "Long", "at": 1000.0, "stop": "0.8", "take": 1.2, "hold_h": 4});
        assert_eq!(parse(&v, &known).unwrap(), Signal { source: "jt".into(), id: "114".into(), coin: "BTC".into(), dir: 1.0, at: 1000.0,
            stop: Some(0.8), take: Some(1.2), hold_s: Some(4.0 * H) });
        assert_eq!(parse(&json!({"source": "jt", "id": "a", "coin": "SOL", "side": "шорт", "at": 5}), &known).unwrap().dir, -1.0);
        assert_eq!(parse(&json!({"coin": "SOL", "at": 5}), &known), Err("no side"));
        assert_eq!(parse(&json!({"side": "long", "at": 5}), &known), Err("no coin"));
        assert_eq!(parse(&json!({"coin": "ZEST", "side": "long", "at": 5}), &known), Err("not on HL"));
        assert_eq!(parse(&json!({"coin": "SOL", "side": "long"}), &known), Err("no time"));
    }

    #[test]
    fn opened_with_its_own_stop_and_take_or_the_defaults_or_dropped() {
        let p = Params::RESEARCH;
        assert_eq!(decide(&sig(1.0, None, None), &p, 10_060.0, 100.0), Ok(Plan { stop: 0.03, take: 0.06, hold_s: 72.0 * H }));
        let x = decide(&sig(1.0, Some(95.0), Some(110.0)), &p, 10_060.0, 100.0).unwrap();
        assert!((x.stop - 0.05).abs() < 1e-12 && (x.take - 0.10).abs() < 1e-12);
        let x = decide(&sig(-1.0, Some(104.0), Some(90.0)), &p, 10_060.0, 100.0).unwrap();
        assert!((x.stop - 0.04).abs() < 1e-12 && (x.take - 0.10).abs() < 1e-12);
        assert_eq!(decide(&Signal { hold_s: Some(4.0 * H), ..sig(1.0, None, None) }, &p, 10_060.0, 100.0).unwrap().hold_s, 4.0 * H);
        assert_eq!(decide(&sig(1.0, None, None), &p, 10_000.0 + 31.0 * 60.0, 100.0), Err("stale"));
        assert_eq!(decide(&sig(1.0, Some(101.0), None), &p, 10_060.0, 100.0), Err("stop passed"));
        assert_eq!(decide(&sig(-1.0, None, Some(101.0)), &p, 10_060.0, 100.0), Err("take passed"));
        assert_eq!(decide(&sig(1.0, Some(99.9), None), &p, 10_060.0, 100.0), Err("stop too close"));
        assert_eq!(decide(&sig(1.0, Some(80.0), None), &p, 10_060.0, 100.0), Err("stop too far"));
        assert_eq!(decide(&sig(1.0, None, None), &p, 10_060.0, 0.0), Err("no price"));
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
        assert_eq!((p.live, p.stop, p.take, p.hold_s, p.max_age_s, p.max_open), (false, 0.03, 0.06, 72.0 * 3600.0, 1800.0, 3));
        assert!(message(&json!({"what": "drop", "why": "stale", "coin": "SOL", "source": "jt", "side": "long"})).unwrap().contains("устарела"));
        assert!(message(&json!({"what": "paper_close", "coin": "SOL", "source": "jt", "ret_pct": 6.0, "entry_px": 1.0, "px": 1.06})).unwrap().contains("бумага"));
        assert!(message(&json!({"what": "received"})).is_none());
    }
}

#[cfg(test)]
mod lifecycle {
    use super::*;
    use crate::runner::Strategy;
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
    async fn a_trade_is_opened_with_its_stop_and_take_resting_then_its_take_ends_it() {
        let (f, mut x, mut rx) = setup("XTA", true).await;
        trades::uncapped();
        crate::store::push_signal(&x.k.cfg, &json!({"source": "jt", "id": 1, "coin": "XTAUSDT", "side": "long", "at": now() - 60.0, "stop": 96.0, "take": 108.0})).await.unwrap();
        crate::store::push_signal(&x.k.cfg, &json!({"source": "jt", "id": 2, "coin": "NOPE", "side": "long", "at": now()})).await.unwrap();
        x.step(now()).await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!(whats(&ev), vec!["received", "open", "drop"], "{ev:?}");
        assert_eq!(ev[2]["why"], "not on HL");
        assert!(f.position("XTA") > 0.0 && f.open_orders("XTA").len() == 2 && trades::registered("XTA"));
        let tr = &x.state.open[0].tr;
        assert!((tr.stop_px - 96.0).abs() < 0.2 && (tr.take_px - 108.0).abs() < 0.2, "{tr:?}");
        assert_eq!((x.state.seen, x.state.opened), (2, 1));
        // read once: the same inbox again brings nothing
        x.step(now()).await;
        assert!(crate::store::drain(&mut rx).is_empty());
        f.set_mid("XTA", 109.0);
        x.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((whats(&ev), ev[0]["why"].as_str()), (vec!["close".to_string()], Some("take")), "{ev:?}");
        assert!(x.state.open.is_empty() && !trades::registered("XTA"));
        assert!(crate::testkit::saved(&x.k.cfg, KIND).await.is_some());
    }

    #[tokio::test]
    async fn on_paper_no_order_and_its_result_told_in_r() {
        let (f, mut x, mut rx) = setup("XTB", false).await;
        crate::store::push_signal(&x.k.cfg, &json!({"source": "jt", "id": 9, "coin": "XTB", "side": "short", "at": now()})).await.unwrap();
        x.step(now()).await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!(whats(&ev), vec!["received", "paper_open"], "{ev:?}");
        assert_eq!((x.state.paper.len(), f.position("XTB"), trades::registered("XTB")), (1, 0.0, false));
        f.set_mid("XTB", 103.1);
        x.check().await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((whats(&ev), ev[0]["why"].as_str()), (vec!["paper_close".to_string()], Some("stop")), "{ev:?}");
        assert!((x.state.paper_pnl_r + 1.033).abs() < 0.01, "{}", x.state.paper_pnl_r);
    }

    #[tokio::test]
    async fn a_stale_trade_is_dropped_and_a_full_family_takes_none() {
        let (f, mut x, mut rx) = setup("XTC", true).await;
        crate::store::push_signal(&x.k.cfg, &json!({"source": "jt", "id": 1, "coin": "XTC", "side": "long", "at": now() - 2.0 * H})).await.unwrap();
        x.step(now()).await;
        let ev = crate::store::drain(&mut rx);
        assert_eq!((whats(&ev), ev[1]["why"].as_str()), (vec!["received".to_string(), "drop".to_string()], Some("stale")));
        x.p.max_open = 0;
        crate::store::push_signal(&x.k.cfg, &json!({"source": "jt", "id": 2, "coin": "XTC", "side": "long", "at": now()})).await.unwrap();
        x.step(now()).await;
        assert_eq!(crate::store::drain(&mut rx).last().unwrap()["why"], "full");
        assert_eq!(f.position("XTC"), 0.0);
    }
}
