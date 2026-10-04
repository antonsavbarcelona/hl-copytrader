//! Smart money by what each followed trader does, not only by its net flow. Every fill becomes
//! an action on its position — open (from flat), add, reduce, close, flip — with the context it
//! was taken in: the position's PnL and age, the price since its last action in the coin, how
//! big it is against its usual size, what it just took out of other coins.
//!
//! Every entry (open, add, flip) is scored against the price 5 / 15 / 60 / 240 min later, per
//! trader and, at 60 min, per coin, direction and the coin's regime then (trend / range), with
//! a recent-form copy that halves every day, how far the price went its way and against it in
//! the hour, and how many other traders entered the same way in the 30 min before and after:
//! the ratings the action signals choose their traders by. They start empty and grow as the run
//! goes (saved with the run, `smart_state`).
//!
//! The signals (`Kind::Smart`, one `Rule` each) count, per coin over a window, the traders whose
//! latest action of the rule's kind still stands (none of theirs the other way since), one vote
//! each.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::log;
use crate::signals::{Ctx, Prices, Reading, Variant};

/// Fills of one trader in one coin this close together, the same way, are one action.
const MERGE_MS: u64 = 2_000;
/// A trader's entries in a coin the same way within this of a scored one are not scored again:
/// scaling in with many orders is one decision, not many.
const SCORE_GAP_MS: u64 = 900_000;
/// An entry's size is taken this long after it (its fills merged) for the trader's usual size.
const SIZE_AFTER_MS: u64 = 10_000;
/// Actions are kept this long (longer than any window, and the lead count's hour).
pub const KEEP_MS: u64 = 4 * 3_600_000;
/// Entries are scored this long after: the price then against the entry.
pub const HORIZONS_MS: [u64; 4] = [300_000, 900_000, 3_600_000, 14_400_000];
const H15: usize = 1;
const H60: usize = 2;
const H240: usize = 3;
/// The recent form halves over this.
const RECENT_HALF_LIFE_MS: f64 = 86_400_000.0;
/// A close, then an open the other way within this: a flip.
const FLIP_MS: u64 = 600_000;
/// What a trader took out of other coins this long before an entry is what it rotates.
const ROTATION_MS: u64 = 600_000;
/// Other traders' entries this long before / after one: who leads, who follows.
const LEAD_MS: u64 = 1_800_000;
/// Two traders entering the same coin the same way within this, this many times, are taken as
/// one (the same owner, or one copying the other) when counting independent entries.
const LINK_MS: u64 = 10_000;
const LINKED: u32 = 3;
/// A coin moving this much (%) over 4 h is trending, else ranging.
const TREND_PCT: f64 = 2.0;
const TREND_WINDOW_S: f64 = 4.0 * 3600.0;
/// A position opened this recently is fresh (a new decision, not a legacy exposure).
const FRESH_MS: u64 = 1_800_000;
/// A probe (first entry of 1% of equity or less) confirmed within this.
const PLAYBOOK_MS: u64 = 2 * 3_600_000;
/// ... the probe left alone this long first (not an order sliced in pieces).
const PROBE_MIN_MS: u64 = 600_000;
/// An entry this many standard deviations over the trader's usual size (log) is a surprise.
const SIZE_Z: f64 = 2.5;
/// Funding (hourly): the smart side paid at least this to hold (entries), or the side being
/// left paying at least this (a crowded side, exits); open interest up this much in an hour.
const FUNDING_PAID: f64 = 0.000005;
const FUNDING_CROWDED: f64 = 0.0000125;
const OI_UP_PCT: f64 = 1.0;
/// Sizes this small are flat.
const FLAT: f64 = 1e-9;

/// What an action did to the trader's position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Open,
    Add,
    Reduce,
    Close,
    Flip,
}

fn classify(before: f64, after: f64) -> Act {
    if before.abs() < FLAT {
        Act::Open
    } else if after.abs() < FLAT {
        Act::Close
    } else if before.signum() != after.signum() {
        Act::Flip
    } else if after.abs() > before.abs() {
        Act::Add
    } else {
        Act::Reduce
    }
}

/// Which action signal a variant trades (see `VARIANTS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    None,
    /// 1. Opened from flat, `min_pct`% of equity or more (and `min_usd`).
    Open,
    /// 2. Added half of the position or more.
    Add,
    /// 3. Took half or more off a position (`min_pct`%+ of equity) held an hour or more.
    Exit,
    /// 4. Long to short or back, both sides `min_pct`%+ of equity (at once, or a close and an
    ///    open within 10 min).
    Flip,
    /// 5. Its flow in the coin growing over the window's thirds, `min_pct`%+ in all.
    Accel,
    /// 6. A position of `min_pct`%+ of equity opened within the last 30 min.
    Fresh,
    /// 7. Adds to a winning position (1%+ up) / cuts a losing one (1%+ down).
    WinnerAdd,
    LoserCut,
    /// 8. Adds after the price went 2%+ against / for it since its last action.
    AvgDown,
    AvgUp,
    /// 9. Independent traders (not linked, see `LINKED`) opening the same way.
    Coordinated,
    /// 10. Entries by traders others follow in (twice as many enter after them as before).
    Leader,
    /// 11. Entries by traders good in this coin and this direction.
    Specialist,
    /// 12. Entries by traders whose copies made more than their worst drawdown (5+ trips).
    RiskAdj,
    /// 13. Entries by traders in good recent form.
    Recent,
    /// 14. Entries by traders good in the coin's current regime (trend / range).
    Regime,
    /// 15. An entry 2.5 sd over the trader's usual size, `min_usd`+.
    SizeSurprise,
    /// 16. An entry of `min_pct`%+ of equity leaving the coin 1x equity or more and half its book,
    ///     25 points of its book more than before.
    Concentration,
    /// 17. A new position of `min_pct`%+ of equity, half or more of what closing another (as big)
    ///     freed in the 10 min before.
    Rotation,
    /// 18. The top-rated traders one way, most of the others the other way: the top followed.
    Disagree,
    /// 19. Positively rated traders one way, every taker's net flow ($`min_usd`+) the other.
    VsCrowd,
    /// 20. Entries paid funding to hold, open interest rising / exits from a side paying a lot.
    FundingEntry,
    FundingExit,
    /// 21. Entries by traders whose entries lead the price at 15 min (t >= 2).
    Early,
    /// 22. Entries by traders whose entries go further their way than against in the hour.
    Execution,
    /// 23. Entries by traders whose edge is at 15 min / at 4 h (held for that long).
    HorizonShort,
    HorizonLong,
    /// 24. A probe (1% of equity or less) left alone 10 min+, then within 2 h an add to `min_pct`%+.
    Playbook,
    /// 25. Averaged down twice or more, then gave up (75%+ off at 3%+ down): faded.
    Capitulation,
}

/// Running count, sum and sum of squares.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct Acc {
    pub n: f64,
    pub sum: f64,
    pub sq: f64,
}

impl Acc {
    pub fn add(&mut self, x: f64) {
        self.n += 1.0;
        self.sum += x;
        self.sq += x * x;
    }

    pub fn mean(&self) -> f64 {
        if self.n > 0.0 { self.sum / self.n } else { 0.0 }
    }

    pub fn sd(&self) -> f64 {
        if self.n < 2.0 {
            return 0.0;
        }
        ((self.sq - self.sum * self.sum / self.n) / (self.n - 1.0)).max(0.0).sqrt()
    }

    /// The mean over its standard error: how sure a positive mean is (Sharpe-like, per entry).
    pub fn t(&self) -> f64 {
        let sd = self.sd();
        if sd > 0.0 { self.mean() / sd * self.n.sqrt() } else { 0.0 }
    }

    fn scale(&mut self, f: f64) {
        self.n *= f;
        self.sum *= f;
        self.sq *= f;
    }
}

/// A trader's record, from its entries.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Score {
    /// The price 5 / 15 / 60 / 240 min after each entry against it, bps its way.
    pub h: [Acc; 4],
    /// The same at 60 min, halving every day (its recent form), as of `recent_ms`.
    pub recent: Acc,
    pub recent_ms: u64,
    /// At 60 min: per coin, per direction (long, short), per the coin's regime at the entry
    /// (trend, range).
    pub coin: HashMap<String, Acc>,
    pub dir: [Acc; 2],
    pub regime: [Acc; 2],
    /// The hour after each entry: how far the price went its way and against it, bps summed,
    /// over `excursions` entries.
    pub mfe: f64,
    pub mae: f64,
    pub excursions: f64,
    /// Other traders entering the same coin the same way in the 30 min before / after its
    /// entries, summed over `leads` entries.
    pub lead_before: f64,
    pub lead_after: f64,
    pub leads: f64,
    /// ln of its entries' size (USD, every entry, its fills merged): its usual size.
    pub size: Acc,
}

/// A trader's position in a coin as we track it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Pos {
    pub size: f64,
    /// Average entry price (0: unknown).
    pub entry: f64,
    /// When it was opened from flat (None: held before we saw it).
    pub opened_ms: Option<u64>,
    /// Its first entry, % of equity.
    pub open_pct: f64,
    /// The price of its last action in the coin.
    pub last_px: f64,
    /// Adds made while it was at a loss.
    pub underwater_adds: u32,
}

/// One action of a trader in a coin.
#[derive(Clone, Debug)]
pub struct Action {
    pub t_ms: u64,
    /// The last fill merged into it.
    end_ms: u64,
    pub who: String,
    pub act: Act,
    /// +1 bought, -1 sold.
    pub side: f64,
    /// Its position before and after (size; before a two-step flip, the position it closed).
    pub before_sz: f64,
    pub after_sz: f64,
    pub equity: f64,
    /// The first fill's price.
    pub px: f64,
    /// The position before it: PnL against its entry and the price since its last action, %
    /// its way; how long it had been open (None: held before we saw it).
    pub upnl_pct: f64,
    pub since_last_pct: f64,
    pub age_ms: Option<u64>,
    /// The position's first entry (% of equity) and its adds at a loss.
    pub open_pct: f64,
    pub underwater_adds: u32,
    /// Its usual entry size (ln USD) at the time.
    size: Acc,
    /// The positions it closed in other coins in the 10 min before (USD).
    pub freed_usd: f64,
    /// Its other positions (USD at their last actions' prices).
    pub others_usd: f64,
    /// Scheduled for scoring (an entry).
    scored: bool,
}

impl Action {
    pub fn before(&self) -> f64 {
        self.before_sz * self.px
    }

    pub fn after(&self) -> f64 {
        self.after_sz * self.px
    }

    /// USD, signed.
    pub fn delta(&self) -> f64 {
        self.after() - self.before()
    }

    /// `usd` as % of its equity.
    pub fn pct(&self, usd: f64) -> f64 {
        usd.abs() / self.equity * 100.0
    }

    pub fn delta_pct(&self) -> f64 {
        self.pct(self.delta())
    }

    pub fn is_entry(&self) -> bool {
        matches!(self.act, Act::Open | Act::Add | Act::Flip)
    }

    pub fn is_exit(&self) -> bool {
        matches!(self.act, Act::Reduce | Act::Close)
    }

    /// Share of the position taken off.
    fn reduced(&self) -> f64 {
        match self.act {
            Act::Close | Act::Flip => 1.0,
            Act::Reduce if self.before_sz.abs() >= FLAT => 1.0 - self.after_sz.abs() / self.before_sz.abs(),
            _ => 0.0,
        }
    }

    /// The coin's share of its book with `usd` in it.
    fn share(&self, usd: f64) -> f64 {
        let x = usd.abs();
        if x + self.others_usd > 0.0 { x / (x + self.others_usd) } else { 0.0 }
    }

    /// Its size against its usual entries, standard deviations of ln USD (20+ entries known).
    fn size_z(&self) -> Option<f64> {
        let sd = self.size.sd();
        (self.size.n >= 20.0 && sd > 0.0 && self.delta() != 0.0).then(|| (self.delta().abs().ln() - self.size.mean()) / sd)
    }
}

/// An entry to score once its horizon has passed.
#[derive(Clone, Debug)]
struct Due {
    t_ms: u64,
    who: String,
    coin: String,
    side: f64,
    px: f64,
    /// The coin's regime at the entry: 0 trend, 1 range.
    regime: Option<usize>,
}

/// What `read` needs besides the actions.
pub struct View<'a> {
    pub now_ms: u64,
    pub prices: &'a Prices,
    pub ctx: &'a Ctx,
    /// The traders whose copies made more than their worst drawdown.
    pub risk_adj: &'a HashSet<String>,
    /// The top-rated traders (`top_tier`).
    pub top: &'a HashSet<String>,
    /// Every taker's net flow in the coin over the window, USD (`VsCrowd`).
    pub crowd_usd: Option<f64>,
}

/// What is saved with the run.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    pos: HashMap<String, HashMap<String, Pos>>,
    scores: HashMap<String, Score>,
    links: HashMap<String, u32>,
}

#[derive(Default)]
pub struct Smart {
    /// trader -> coin -> its position.
    pos: HashMap<String, HashMap<String, Pos>>,
    pub scores: HashMap<String, Score>,
    /// "a|b" -> times the two entered the same coin the same way within `LINK_MS`.
    links: HashMap<String, u32>,
    /// Per coin, oldest first.
    actions: HashMap<String, VecDeque<Action>>,
    due: [VecDeque<Due>; 4],
    lead: VecDeque<Due>,
    /// Entries (when, trader, coin) whose size goes into the trader's usual size.
    sizes: VecDeque<(u64, String, String)>,
    /// (trader, coin) -> when it last closed a position there, and the size it closed.
    closed: HashMap<(String, String), (u64, f64)>,
    /// (trader, coin) -> its last scored entry: when, which way.
    last_scored: HashMap<(String, String), (u64, f64)>,
    /// trader -> (when, coin, USD) of what it took off, the last 10 min.
    exits: HashMap<String, VecDeque<(u64, String, f64)>>,
    /// Since when (exchange ms) the actions are complete.
    pub since_ms: u64,
    pruned_ms: u64,
}

fn pair(a: &str, b: &str) -> String {
    if a < b { format!("{a}|{b}") } else { format!("{b}|{a}") }
}

/// The coin's regime at `t_ms`: 0 trending, 1 ranging (None until 4 h of prices).
fn regime(prices: &Prices, coin: &str, t_ms: u64) -> Option<usize> {
    prices.ret(coin, t_ms, TREND_WINDOW_S).map(|x| if x.abs() * 100.0 >= TREND_PCT { 0 } else { 1 })
}

impl Smart {
    /// From what was saved (positions, ratings, links), actions complete from `since_ms`.
    pub fn load(saved: Option<&str>, since_ms: u64) -> Self {
        let s: Saved = match saved.map(serde_json::from_str) {
            Some(Ok(s)) => s,
            Some(Err(e)) => {
                log!("smart money state unreadable, starting empty: {e}");
                Saved::default()
            }
            None => Saved::default(),
        };
        log!("smart money: {} traders rated, {} positions tracked", s.scores.len(), s.pos.values().map(HashMap::len).sum::<usize>());
        Self { pos: s.pos, scores: s.scores, links: s.links, since_ms, ..Default::default() }
    }

    /// Positions, ratings and the links seen twice or more, serialized.
    pub fn saved(&self) -> String {
        #[derive(Serialize)]
        struct Out<'a> {
            pos: &'a HashMap<String, HashMap<String, Pos>>,
            scores: &'a HashMap<String, Score>,
            links: HashMap<&'a String, u32>,
        }
        let links = self.links.iter().filter(|(_, n)| **n >= 2).map(|(k, n)| (k, *n)).collect();
        serde_json::to_string(&Out { pos: &self.pos, scores: &self.scores, links }).unwrap_or_default()
    }

    /// Actions held, all coins.
    pub fn len(&self) -> usize {
        self.actions.values().map(VecDeque::len).sum()
    }

    /// Traders with 10+ entries scored at 60 min.
    pub fn rated(&self) -> usize {
        self.scores.values().filter(|s| s.h[H60].n >= 10.0).count()
    }

    /// Its positions as read from the exchange (`entry`: their entry prices).
    pub fn sync(&mut self, who: &str, positions: &HashMap<String, f64>, entry: impl Fn(&str) -> Option<f64>) {
        let m = self.pos.entry(who.to_string()).or_default();
        m.retain(|c, _| positions.contains_key(c));
        for (c, &size) in positions {
            let e = entry(c).filter(|x| *x > 0.0);
            match m.get_mut(c) {
                Some(p) if p.size.signum() == size.signum() => {
                    p.size = size;
                    if let Some(e) = e {
                        p.entry = e;
                    }
                }
                _ => {
                    let px = e.unwrap_or(0.0);
                    m.insert(c.clone(), Pos { size, entry: px, opened_ms: None, open_pct: 0.0, last_px: px, underwater_adds: 0 });
                }
            }
        }
    }

    /// A fill of a followed trader: `before` its position in the coin (size), `delta` the fill.
    #[allow(clippy::too_many_arguments)]
    pub fn on_fill(&mut self, who: &str, coin: &str, before: f64, delta: f64, px: f64, t_ms: u64, equity: f64, prices: &Prices) {
        if equity <= 0.0 || px <= 0.0 || delta == 0.0 {
            return;
        }
        let after = before + delta;
        let (flat_before, flat_after) = (before.abs() < FLAT, after.abs() < FLAT);
        let dir = before.signum();
        // The position before, and after this fill.
        let others_usd: f64 = self.pos.get(who).map(|m| m.iter().filter(|(c, _)| c.as_str() != coin).map(|(_, p)| (p.size * p.last_px).abs()).sum())
            .unwrap_or(0.0);
        let (upnl_pct, since_last_pct, age_ms, open_pct, underwater_adds) = {
            let pos = self.pos.entry(who.to_string()).or_default().entry(coin.to_string()).or_default();
            if flat_before {
                *pos = Pos::default();
            } else if pos.entry <= 0.0 || pos.size.abs() < FLAT || pos.size.signum() != dir {
                // Out of step (a fill missed): taken as it is now, at this price.
                *pos = Pos { size: before, entry: px, opened_ms: None, open_pct: 0.0, last_px: px, underwater_adds: 0 };
            }
            pos.size = before;
            let upnl = if flat_before { 0.0 } else { (px / pos.entry - 1.0) * dir * 100.0 };
            let since_last = if flat_before || pos.last_px <= 0.0 { 0.0 } else { (px / pos.last_px - 1.0) * dir * 100.0 };
            let age = if flat_before { Some(0) } else { pos.opened_ms.map(|o| t_ms.saturating_sub(o)) };
            let ctx = (upnl, since_last, age, pos.open_pct, pos.underwater_adds);
            if !flat_after && (flat_before || after.signum() != dir) {
                *pos = Pos { size: after, entry: px, opened_ms: Some(t_ms), open_pct: after.abs() * px / equity * 100.0, last_px: px,
                             underwater_adds: 0 };
            } else if after.abs() > before.abs() {
                if upnl < 0.0 {
                    pos.underwater_adds += 1;
                }
                pos.entry = (before.abs() * pos.entry + delta.abs() * px) / after.abs();
                pos.size = after;
                pos.last_px = px;
            } else {
                pos.size = after;
                pos.last_px = px;
            }
            ctx
        };
        let key = (who.to_string(), coin.to_string());
        if flat_after {
            if let Some(m) = self.pos.get_mut(who) {
                m.remove(coin);
            }
            if !flat_before {
                self.closed.insert(key.clone(), (t_ms, before));
            }
        }
        // The positions it closed, for its rotations.
        let off = if !flat_before && (flat_after || after.signum() != dir) { before.abs() } else { 0.0 };
        let exits = self.exits.entry(who.to_string()).or_default();
        if off > 0.0 {
            exits.push_back((t_ms, coin.to_string(), off * px));
        }
        while exits.front().is_some_and(|x| x.0 + ROTATION_MS < t_ms) {
            exits.pop_front();
        }
        let freed_usd: f64 = exits.iter().filter(|x| x.1 != coin && x.0 + ROTATION_MS >= t_ms && x.0 <= t_ms).map(|x| x.2).sum();

        let side = delta.signum();
        // A fill of the same order (or a quick series of them): the same action.
        let q = self.actions.entry(coin.to_string()).or_default();
        if let Some(a) = q.iter_mut().rev().take(64).find(|a| a.who == who && a.side == side && t_ms.saturating_sub(a.end_ms) <= MERGE_MS) {
            a.after_sz += delta;
            a.end_ms = a.end_ms.max(t_ms);
            a.act = classify(a.before_sz, a.after_sz);
            // A sale that went through its long (or the reverse) is an entry now.
            let newly = a.is_entry() && !a.scored;
            a.scored |= newly;
            let (t0, px0) = (a.t_ms, a.px);
            if newly {
                self.schedule(Due { t_ms: t0, who: who.to_string(), coin: coin.to_string(), side, px: px0, regime: regime(prices, coin, t0) });
            }
            return;
        }
        // A close, then an open the other way soon after: a flip in two steps.
        let two_step = self.closed.get(&key).copied().filter(|&(t, closed)| {
            flat_before && t_ms.saturating_sub(t) <= FLIP_MS && closed.signum() == -side && delta.abs() >= 0.5 * closed.abs()
        });
        let before_sz = two_step.map(|x| x.1).unwrap_or(before);
        let act = classify(before_sz, after);
        let q = self.actions.entry(coin.to_string()).or_default();
        let a = Action {
            t_ms,
            end_ms: t_ms,
            who: who.to_string(),
            act,
            side,
            before_sz,
            after_sz: after,
            equity,
            px,
            upnl_pct,
            since_last_pct,
            age_ms,
            open_pct,
            underwater_adds,
            size: self.scores.get(who).map(|s| s.size).unwrap_or_default(),
            freed_usd,
            others_usd,
            scored: matches!(act, Act::Open | Act::Add | Act::Flip),
        };
        if a.scored {
            // Who entered the same way just before: links.
            let mut others: HashSet<&str> = HashSet::new();
            for b in q.iter().rev().take_while(|b| b.t_ms + LINK_MS >= t_ms) {
                if b.who != who && b.side == side && b.is_entry() {
                    others.insert(b.who.as_str());
                }
            }
            for o in others {
                *self.links.entry(pair(who, o)).or_insert(0) += 1;
            }
        }
        let scored = a.scored;
        q.push_back(a);
        if scored {
            self.sizes.push_back((t_ms, who.to_string(), coin.to_string()));
            self.schedule(Due { t_ms, who: who.to_string(), coin: coin.to_string(), side, px, regime: regime(prices, coin, t_ms) });
        }
        if t_ms >= self.pruned_ms + 60_000 {
            self.prune(t_ms);
        }
    }

    /// Scores the entry once its horizons pass (not one of the same decision, `SCORE_GAP_MS`).
    fn schedule(&mut self, d: Due) {
        let key = (d.who.clone(), d.coin.clone());
        if self.last_scored.get(&key).is_some_and(|&(t, side)| side == d.side && d.t_ms.saturating_sub(t) < SCORE_GAP_MS) {
            return;
        }
        self.last_scored.insert(key, (d.t_ms, d.side));
        for q in self.due.iter_mut() {
            q.push_back(d.clone());
        }
        self.lead.push_back(d);
    }

    fn prune(&mut self, now_ms: u64) {
        self.pruned_ms = now_ms;
        let cut = now_ms.saturating_sub(KEEP_MS);
        self.actions.retain(|_, q| {
            while q.front().is_some_and(|a| a.t_ms < cut) {
                q.pop_front();
            }
            !q.is_empty()
        });
        self.closed.retain(|_, (t, _)| *t + FLIP_MS >= now_ms);
        self.last_scored.retain(|_, (t, _)| *t + SCORE_GAP_MS >= now_ms);
        self.exits.retain(|_, q| q.back().is_some_and(|x| x.0 + ROTATION_MS >= now_ms));
    }

    /// Scores the entries whose horizons have passed.
    pub fn tick(&mut self, now_ms: u64, prices: &Prices) {
        for (i, h) in HORIZONS_MS.iter().enumerate() {
            while self.due[i].front().is_some_and(|d| d.t_ms + h <= now_ms) {
                let d = self.due[i].pop_front().unwrap();
                let Some(p) = prices.last(&d.coin).filter(|p| *p > 0.0) else { continue };
                let ret = (p / d.px - 1.0) * d.side * 1e4;
                let range = if i == H60 { prices.range(&d.coin, d.t_ms, d.t_ms + h) } else { None };
                let s = self.scores.entry(d.who.clone()).or_default();
                s.h[i].add(ret);
                if i == H60 {
                    let f = 0.5f64.powf(now_ms.saturating_sub(s.recent_ms) as f64 / RECENT_HALF_LIFE_MS);
                    s.recent.scale(f);
                    s.recent.add(ret);
                    s.recent_ms = now_ms;
                    s.coin.entry(d.coin.clone()).or_default().add(ret);
                    s.dir[if d.side > 0.0 { 0 } else { 1 }].add(ret);
                    if let Some(r) = d.regime {
                        s.regime[r].add(ret);
                    }
                    if let Some((lo, hi)) = range {
                        let (up, down) = ((hi / d.px - 1.0) * 1e4, (1.0 - lo / d.px) * 1e4);
                        let (fav, adv) = if d.side > 0.0 { (up, down) } else { (down, up) };
                        s.mfe += fav.max(0.0);
                        s.mae += adv.max(0.0);
                        s.excursions += 1.0;
                    }
                }
            }
        }
        while self.sizes.front().is_some_and(|x| x.0 + SIZE_AFTER_MS <= now_ms) {
            let (t, who, coin) = self.sizes.pop_front().unwrap();
            let usd = self.actions.get(&coin).and_then(|q| q.iter().rev().find(|a| a.t_ms == t && a.who == who)).map(|a| a.delta().abs());
            if let Some(u) = usd.filter(|u| *u > 0.0) {
                self.scores.entry(who).or_default().size.add(u.ln());
            }
        }
        while self.lead.front().is_some_and(|d| d.t_ms + LEAD_MS <= now_ms) {
            let d = self.lead.pop_front().unwrap();
            let (mut before, mut after): (HashSet<&str>, HashSet<&str>) = Default::default();
            for a in self.actions.get(&d.coin).into_iter().flatten() {
                if a.who == d.who || a.side != d.side || !a.is_entry() || a.t_ms + LEAD_MS < d.t_ms || a.t_ms > d.t_ms + LEAD_MS {
                    continue;
                }
                if a.t_ms < d.t_ms {
                    before.insert(a.who.as_str());
                } else if a.t_ms > d.t_ms {
                    after.insert(a.who.as_str());
                }
            }
            let (b, a) = (before.len() as f64, after.len() as f64);
            let s = self.scores.entry(d.who.clone()).or_default();
            s.lead_before += b;
            s.lead_after += a;
            s.leads += 1.0;
        }
    }

    /// The top tenth (at least 5) of the traders with 10+ entries scored, by their entries' t
    /// at 60 min, those above zero.
    pub fn top_tier(&self) -> HashSet<String> {
        let mut v: Vec<(&String, f64)> = self.scores.iter().filter(|(_, s)| s.h[H60].n >= 10.0).map(|(w, s)| (w, s.h[H60].t())).collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
        let k = (v.len() / 10).max(5);
        v.into_iter().take(k).filter(|x| x.1 > 0.0).map(|(w, _)| w.clone()).collect()
    }

    fn linked(&self, a: &str, b: &str) -> bool {
        self.links.get(&pair(a, b)).is_some_and(|&n| n >= LINKED)
    }

    /// Its rating passes the rule's bar.
    fn passes(&self, rule: Rule, a: &Action, coin: &str, w: &View) -> bool {
        if rule == Rule::RiskAdj {
            return w.risk_adj.contains(&a.who);
        }
        let Some(s) = self.scores.get(&a.who) else { return false };
        let dir = if a.side > 0.0 { 0 } else { 1 };
        match rule {
            Rule::Leader => s.leads >= 5.0 && s.lead_after >= 0.5 * s.leads && s.lead_after >= 2.0 * s.lead_before,
            Rule::Specialist => {
                s.coin.get(coin).is_some_and(|c| c.n >= 4.0 && c.mean() >= 20.0) && s.dir[dir].n >= 4.0 && s.dir[dir].mean() > 0.0
            }
            Rule::Recent => {
                let f = 0.5f64.powf(w.now_ms.saturating_sub(s.recent_ms) as f64 / RECENT_HALF_LIFE_MS);
                s.recent.n * f >= 3.0 && s.recent.mean() >= 10.0
            }
            Rule::Regime => regime(w.prices, coin, w.now_ms).is_some_and(|r| s.regime[r].n >= 5.0 && s.regime[r].mean() >= 10.0),
            Rule::Early => s.h[H15].n >= 10.0 && s.h[H15].t() >= 2.0,
            Rule::Execution => s.excursions >= 8.0 && s.mfe / (s.mfe + s.mae).max(1e-9) >= 0.6,
            Rule::HorizonShort => s.h[H15].n >= 8.0 && s.h[H15].t() >= 1.5 && s.h[H15].t() > s.h[H240].t(),
            Rule::HorizonLong => s.h[H240].n >= 8.0 && s.h[H240].t() >= 1.5 && s.h[H240].t() > s.h[H15].t(),
            _ => false,
        }
    }

    /// The action is of the variant's kind.
    fn qualifies(&self, v: &Variant, a: &Action, coin: &str, w: &View) -> bool {
        let entry = a.is_entry() && a.delta_pct() >= v.min_pct;
        match v.rule {
            Rule::Open | Rule::Coordinated => a.act == Act::Open && a.delta_pct() >= v.min_pct && a.delta().abs() >= v.min_usd,
            Rule::Add => a.act == Act::Add && a.delta().abs() >= 0.5 * a.before().abs() && a.delta_pct() >= v.min_pct,
            Rule::Exit | Rule::FundingExit => {
                a.is_exit() && a.reduced() >= 0.5 && a.pct(a.before()) >= v.min_pct && a.age_ms.is_none_or(|x| x >= 3_600_000)
            }
            Rule::Flip => a.act == Act::Flip && a.pct(a.before()) >= v.min_pct && a.pct(a.after()) >= v.min_pct,
            Rule::Fresh => {
                a.is_entry() && a.pct(a.after()) >= v.min_pct && (a.act != Act::Add || a.age_ms.is_some_and(|x| x <= FRESH_MS))
            }
            Rule::WinnerAdd => a.act == Act::Add && a.upnl_pct >= 1.0 && a.delta_pct() >= v.min_pct,
            Rule::LoserCut => a.is_exit() && a.upnl_pct <= -1.0 && a.reduced() >= 0.25 && a.pct(a.before()) >= v.min_pct,
            Rule::AvgDown => a.act == Act::Add && a.since_last_pct <= -2.0 && a.delta_pct() >= v.min_pct,
            Rule::AvgUp => a.act == Act::Add && a.since_last_pct >= 2.0 && a.delta_pct() >= v.min_pct,
            Rule::SizeSurprise => a.is_entry() && a.delta().abs() >= v.min_usd && a.size_z().is_some_and(|z| z >= SIZE_Z),
            Rule::Concentration => entry && a.pct(a.after()) >= 100.0 && a.share(a.after()) >= 0.5 && a.share(a.after()) - a.share(a.before()) >= 0.25,
            Rule::Rotation => {
                matches!(a.act, Act::Open | Act::Flip) && a.pct(a.after()) >= v.min_pct && a.freed_usd >= v.min_pct / 100.0 * a.equity
                    && a.after().abs() >= 0.5 * a.freed_usd
            }
            Rule::Playbook => {
                a.act == Act::Add && a.open_pct > 0.0 && a.open_pct <= 1.0 && a.pct(a.before()) <= 1.0
                    && a.age_ms.is_some_and(|x| (PROBE_MIN_MS..=PLAYBOOK_MS).contains(&x)) && a.pct(a.after()) >= v.min_pct
            }
            Rule::Capitulation => {
                a.is_exit() && a.reduced() >= 0.75 && a.upnl_pct <= -3.0 && a.underwater_adds >= 2 && a.pct(a.before()) >= v.min_pct
            }
            Rule::FundingEntry | Rule::Disagree => entry,
            Rule::VsCrowd => entry && self.scores.get(&a.who).is_some_and(|s| s.h[H60].n >= 5.0 && s.h[H60].mean() > 0.0),
            Rule::Leader | Rule::Specialist | Rule::RiskAdj | Rule::Recent | Rule::Regime | Rule::Early | Rule::Execution
            | Rule::HorizonShort | Rule::HorizonLong => entry && self.passes(v.rule, a, coin, w),
            Rule::None | Rule::Accel => false,
        }
    }

    /// How `v` reads `coin` now: (fires, side).
    pub fn read(&self, v: &Variant, coin: &str, w: &View, rd: &mut Reading) -> (bool, f64) {
        let none = (false, 0.0);
        let window_ms = (v.window_s * 1000.0) as u64;
        let since = w.now_ms.saturating_sub(window_ms);
        if since < self.since_ms {
            return none;
        }
        let Some(q) = self.actions.get(coin) else { return none };
        // Newest first; each trader's latest action is its side now.
        let recent: Vec<&Action> = q.iter().rev().take_while(|a| a.t_ms >= since).collect();
        if recent.is_empty() {
            return none;
        }
        let mut last: HashMap<&str, f64> = HashMap::new();
        for a in &recent {
            last.entry(a.who.as_str()).or_insert(a.side);
        }
        if v.rule == Rule::Accel {
            let votes = accelerating(&recent, since, window_ms, v.min_pct);
            tally(rd, &votes.iter().map(|x| (x.0, x.1)).collect::<Vec<_>>());
            // The traders' whole flow over the window, not their last actions.
            rd.score = votes.iter().map(|x| x.2.min(20.0) * x.1).sum();
            return by_heads(v, rd);
        }
        // Each trader's latest action of the kind, if it still stands.
        let mut seen: HashSet<&str> = HashSet::new();
        let mut votes: Vec<(&Action, f64)> = Vec::new();
        for a in &recent {
            if seen.contains(a.who.as_str()) || !self.qualifies(v, a, coin, w) {
                continue;
            }
            seen.insert(a.who.as_str());
            if last[a.who.as_str()] == a.side {
                votes.push((a, if v.rule == Rule::Capitulation { -a.side } else { a.side }));
            }
        }
        if v.rule == Rule::Disagree {
            let (top, rest): (Vec<_>, Vec<_>) = votes.into_iter().partition(|(a, _)| w.top.contains(&a.who));
            tally(rd, &top);
            let (fires, side) = by_heads(v, rd);
            let against = rest.iter().filter(|x| x.1 == -side).count();
            // The others against, % of the others who took a side.
            rd.score = if rest.is_empty() { 0.0 } else { against as f64 / rest.len() as f64 * 100.0 };
            return (fires && against >= 3 && rd.score >= 60.0, side);
        }
        tally(rd, &votes);
        if v.rule == Rule::Coordinated {
            // Linked traders count once.
            let indep = |s: f64| {
                let mut kept: Vec<&str> = Vec::new();
                for (a, _) in votes.iter().filter(|x| x.1 == s) {
                    if !kept.iter().any(|k| self.linked(k, &a.who)) {
                        kept.push(&a.who);
                    }
                }
                kept.len()
            };
            (rd.longs, rd.shorts) = (indep(1.0), indep(-1.0));
        }
        let (fires, side) = by_heads(v, rd);
        let funding = w.ctx.funding.get(coin).copied();
        let ok = match v.rule {
            Rule::VsCrowd => w.crowd_usd.is_some_and(|c| c * side < 0.0 && c.abs() >= v.min_usd),
            Rule::FundingEntry => {
                funding.is_some_and(|f| f * side <= -FUNDING_PAID) && w.ctx.oi_change_pct(coin, w.now_ms, 3600.0).is_some_and(|x| x >= OI_UP_PCT)
            }
            // Leaving a side (selling longs: side -1) that pays a lot to be on.
            Rule::FundingExit => funding.is_some_and(|f| -f * side >= FUNDING_CROWDED),
            _ => true,
        };
        (fires && ok, side)
    }
}

/// Traders whose flow in the coin grows over the window's thirds, all one way, `min_pct`%+ of
/// equity in all: (its latest action, the way, its flow % of equity).
fn accelerating<'a>(recent: &[&'a Action], since: u64, window_ms: u64, min_pct: f64) -> Vec<(&'a Action, f64, f64)> {
    let mut by: HashMap<&str, ([f64; 3], &'a Action)> = HashMap::new();
    for a in recent {
        let k = (((a.t_ms - since) * 3) / window_ms.max(1)).min(2) as usize;
        let e = by.entry(a.who.as_str()).or_insert(([0.0; 3], a));
        e.0[k] += a.delta() / a.equity * 100.0;
    }
    by.into_values().filter_map(|(d, a)| {
        let s = d[2].signum();
        let growing = d.iter().all(|x| x * s > 0.0) && d[0].abs() < d[1].abs() && d[1].abs() < d[2].abs();
        let total = d.iter().sum::<f64>() * s;
        (s != 0.0 && growing && total >= min_pct).then_some((a, s, total))
    }).collect()
}

/// Counts the votes: traders each way, dollars each way, their sizes (% of equity, 20 at most
/// each) summed the way they voted.
fn tally(rd: &mut Reading, votes: &[(&Action, f64)]) {
    for (a, side) in votes {
        let usd = a.delta().abs();
        if *side > 0.0 {
            rd.longs += 1;
            rd.buy_usd += usd;
        } else {
            rd.shorts += 1;
            rd.sell_usd += usd;
        }
        rd.score += a.delta_pct().min(20.0) * side;
    }
}

/// The way most voted (none on a tie): `min_traders`+ that way and `min_agree` of the votes.
fn by_heads(v: &Variant, rd: &mut Reading) -> (bool, f64) {
    let n = rd.longs + rd.shorts;
    if n == 0 || rd.longs == rd.shorts {
        return (false, 0.0);
    }
    let side = if rd.longs > rd.shorts { 1.0 } else { -1.0 };
    let win = rd.longs.max(rd.shorts);
    rd.agree = win as f64 / n as f64;
    (win >= v.min_traders && rd.agree >= v.min_agree, side)
}

/// One line on what a smart-money variant reads.
pub fn describe(v: &Variant) -> String {
    let (w, n, p) = (v.window_s / 60.0, v.min_traders, v.min_pct);
    let who = |s: &str| format!("{n}+ traders' entries ({p}%+ of equity) {s}, over {w:.0} min");
    match v.rule {
        Rule::None => String::new(),
        Rule::Open => format!("{n}+ traders opening from flat, {p}%+ of equity{}, over {w:.0} min",
            if v.min_usd > 0.0 { format!(" and ${:.0}k+", v.min_usd / 1000.0) } else { String::new() }),
        Rule::Add => format!("{n}+ traders adding half their position or more ({p}%+ of equity), over {w:.0} min"),
        Rule::Exit => format!("{n}+ traders taking half or more off positions of {p}%+ of equity held 1 h+, over {w:.0} min"),
        Rule::Flip => format!("{n}+ traders flipping, both sides {p}%+ of equity, over {w:.0} min"),
        Rule::Accel => format!("{n}+ traders' flow growing over each third of {w:.0} min, {p}%+ of equity in all"),
        Rule::Fresh => format!("{n}+ traders with positions of {p}%+ of equity opened in the last 30 min, over {w:.0} min"),
        Rule::WinnerAdd => format!("{n}+ traders adding to positions 1%+ up, over {w:.0} min"),
        Rule::LoserCut => format!("{n}+ traders cutting positions 1%+ down, over {w:.0} min"),
        Rule::AvgDown => format!("{n}+ traders adding after the price went 2%+ against them, over {w:.0} min"),
        Rule::AvgUp => format!("{n}+ traders adding after the price went 2%+ their way, over {w:.0} min"),
        Rule::Coordinated => format!("{n}+ independent traders opening from flat ({p}%+ of equity), over {w:.0} min"),
        Rule::Leader => who("by traders others follow in"),
        Rule::Specialist => who("by traders good in this coin and direction"),
        Rule::RiskAdj => who("by traders whose copies made more than their worst drawdown"),
        Rule::Recent => who("by traders in good recent form"),
        Rule::Regime => who("by traders good in the coin's current regime"),
        Rule::SizeSurprise => format!("{n}+ entries {SIZE_Z} sd over the trader's usual size, ${:.0}k+, over {w:.0} min", v.min_usd / 1000.0),
        Rule::Concentration => format!("{n}+ entries of {p}%+ of equity taking the coin to 1x equity+ and half the trader's book (+25 points), over {w:.0} min"),
        Rule::Rotation => format!("{n}+ traders opening {p}%+ of equity right after closing as much in another coin, over {w:.0} min"),
        Rule::Disagree => format!("{n}+ top-rated traders one way, 3+ and 60%+ of the others the other way, over {w:.0} min"),
        Rule::VsCrowd => format!("{n}+ positively rated traders one way, ${:.0}k+ of all takers' net flow the other, over {w:.0} min",
            v.min_usd / 1000.0),
        Rule::FundingEntry => who("paid funding to hold, open interest up 1%+ in 1 h"),
        Rule::FundingExit => format!("{n}+ traders leaving a side that pays crowded funding, over {w:.0} min"),
        Rule::Early => who("by traders whose entries lead the price at 15 min"),
        Rule::Execution => who("by traders whose entries go further their way than against"),
        Rule::HorizonShort => who("by traders whose edge is at 15 min"),
        Rule::HorizonLong => who("by traders whose edge is at 4 h"),
        Rule::Playbook => format!("{n}+ traders confirming a probe (1% of equity or less) to {p}%+ within 2 h, over {w:.0} min"),
        Rule::Capitulation => format!("{n}+ traders giving up after averaging down twice (positions of {p}%+ of equity), faded, over {w:.0} min"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::VARIANTS;

    fn var(name: &str) -> &'static Variant {
        VARIANTS.iter().find(|v| v.name == name).unwrap_or_else(|| panic!("no variant {name}"))
    }

    const T0: u64 = 10_000_000;

    fn read(s: &Smart, name: &str, coin: &str, now_ms: u64) -> Reading {
        read_with(s, name, coin, now_ms, &HashSet::new(), None)
    }

    fn read_with(s: &Smart, name: &str, coin: &str, now_ms: u64, risk_adj: &HashSet<String>, crowd: Option<f64>) -> Reading {
        let (prices, ctx, top) = (Prices::default(), Ctx::default(), s.top_tier());
        let w = View { now_ms, prices: &prices, ctx: &ctx, risk_adj, top: &top, crowd_usd: crowd };
        let mut rd = Reading::default();
        let v = var(name);
        let (fires, side) = s.read(v, coin, &w, &mut rd);
        rd.side = if !fires { 0.0 } else if v.fade { -side } else { side };
        rd
    }

    /// A fill at `t_ms` (s after T0), its position before taken from what the test did so far.
    fn fill(s: &mut Smart, who: &str, coin: &str, delta: f64, px: f64, t_s: f64, equity: f64) {
        let before = s.pos.get(who).and_then(|m| m.get(coin)).map(|p| p.size).unwrap_or(0.0);
        s.on_fill(who, coin, before, delta, px, T0 + (t_s * 1000.0) as u64, equity, &Prices::default());
    }

    fn last(s: &Smart, coin: &str) -> Action {
        s.actions[coin].back().unwrap().clone()
    }

    #[test]
    fn actions_from_fills() {
        let mut s = Smart::default();
        // Two fills of one order: one open of $200.
        fill(&mut s, "a", "BTC", 1.0, 100.0, 0.0, 1000.0);
        fill(&mut s, "a", "BTC", 1.0, 100.0, 1.0, 1000.0);
        assert_eq!(s.actions["BTC"].len(), 1);
        let a = last(&s, "BTC");
        assert_eq!((a.act, a.side), (Act::Open, 1.0));
        assert!((a.delta_pct() - 20.0).abs() < 1e-9);
        // Later, at 110: an add to a winner; the entry averages.
        fill(&mut s, "a", "BTC", 1.0, 110.0, 30.0, 1000.0);
        let a = last(&s, "BTC");
        assert_eq!(a.act, Act::Add);
        assert!((a.upnl_pct - 10.0).abs() < 1e-9 && (a.since_last_pct - 10.0).abs() < 1e-9);
        assert!((s.pos["a"]["BTC"].entry - 310.0 / 3.0).abs() < 1e-9);
        // Partly off, then all off, then short within 10 min: reduce, close, flip.
        fill(&mut s, "a", "BTC", -1.0, 105.0, 60.0, 1000.0);
        assert_eq!(last(&s, "BTC").act, Act::Reduce);
        fill(&mut s, "a", "BTC", -2.0, 105.0, 90.0, 1000.0);
        assert_eq!(last(&s, "BTC").act, Act::Close);
        assert!(!s.pos["a"].contains_key("BTC"));
        fill(&mut s, "a", "BTC", -2.0, 104.0, 200.0, 1000.0);
        let a = last(&s, "BTC");
        assert_eq!((a.act, a.before_sz, a.after_sz), (Act::Flip, 2.0, -2.0));
        // Long to short in one order.
        fill(&mut s, "b", "ETH", 1.0, 100.0, 0.0, 1000.0);
        fill(&mut s, "b", "ETH", -3.0, 100.0, 30.0, 1000.0);
        assert_eq!(last(&s, "ETH").act, Act::Flip);
    }

    #[test]
    fn opens_and_turns() {
        let mut s = Smart::default();
        fill(&mut s, "a", "SOL", 10.0, 100.0, 0.0, 10_000.0); // 10%
        let now = T0 + 60_000;
        assert_eq!(read(&s, "sm-open-15m-2t-1p-M", "SOL", now).side, 0.0);
        fill(&mut s, "b", "SOL", 2.0, 100.0, 10.0, 10_000.0); // 2%
        let rd = read(&s, "sm-open-15m-2t-1p-M", "SOL", now);
        assert_eq!((rd.longs, rd.side), (2, 1.0));
        assert_eq!(read(&s, "fade-sm-open-15m-2t-1p-M", "SOL", now).side, -1.0);
        // b sells since: its open no longer stands.
        fill(&mut s, "b", "SOL", -1.0, 100.0, 20.0, 10_000.0);
        assert_eq!(read(&s, "sm-open-15m-2t-1p-M", "SOL", now).longs, 1);
        // Out of the window: nothing.
        assert_eq!(read(&s, "sm-open-15m-2t-1p-M", "SOL", T0 + 20 * 60_000).longs, 0);
    }

    #[test]
    fn capitulation_is_faded() {
        let mut s = Smart::default();
        fill(&mut s, "a", "ETH", 10.0, 100.0, 0.0, 10_000.0); // 10% long
        fill(&mut s, "a", "ETH", 5.0, 96.0, 600.0, 10_000.0); // add at a loss
        fill(&mut s, "a", "ETH", 5.0, 94.0, 1200.0, 10_000.0); // again
        assert_eq!(s.pos["a"]["ETH"].underwater_adds, 2);
        // The averaging down itself: one trader, under the 2 needed.
        let rd = read(&s, "sm-avgdown-60m-2t-L", "ETH", T0 + 1_300_000);
        assert_eq!((rd.longs, rd.side), (1, 0.0));
        fill(&mut s, "a", "ETH", -20.0, 92.0, 1800.0, 10_000.0); // gives up
        let rd = read(&s, "sm-capit-30m-1t-L", "ETH", T0 + 1_801_000);
        assert_eq!(rd.side, 1.0); // its sale, faded
        assert_eq!(read(&s, "sm-avgdown-60m-2t-L", "ETH", T0 + 1_801_000).longs, 0); // it sold since
    }

    #[test]
    fn linked_traders_count_once() {
        let mut s = Smart::default();
        for i in 0..3 {
            let t = i as f64 * 1000.0;
            fill(&mut s, "b", "XRP", 100.0, 1.0, t, 1000.0);
            fill(&mut s, "c", "XRP", 100.0, 1.0, t + 1.0, 1000.0);
            fill(&mut s, "b", "XRP", -100.0, 1.0, t + 500.0, 1000.0);
            fill(&mut s, "c", "XRP", -100.0, 1.0, t + 501.0, 1000.0);
        }
        assert!(s.linked("b", "c"));
        let t = 10_000.0;
        for w in ["a", "b", "c"] {
            fill(&mut s, w, "XRP", 100.0, 1.0, t, 1000.0);
        }
        let now = T0 + 10_060_000;
        assert_eq!(read(&s, "sm-coord-5m-3t-M", "XRP", now).longs, 2);
        fill(&mut s, "d", "XRP", 100.0, 1.0, t + 20.0, 1000.0);
        assert_eq!(read(&s, "sm-coord-5m-3t-M", "XRP", now).side, 1.0);
    }

    #[test]
    fn rotation_playbook_and_flip() {
        let mut s = Smart::default();
        fill(&mut s, "a", "ETH", 10.0, 100.0, 0.0, 10_000.0);
        fill(&mut s, "a", "ETH", -10.0, 100.0, 3600.0, 10_000.0); // frees $1000 (10%)
        fill(&mut s, "a", "SOL", 8.0, 100.0, 3700.0, 10_000.0); // $800 into SOL
        assert!((last(&s, "SOL").freed_usd - 1000.0).abs() < 1e-9);
        assert_eq!(read(&s, "sm-rot-15m-1t-M", "SOL", T0 + 3_760_000).side, 1.0);
        // A probe of 0.5%, confirmed to 5%.
        fill(&mut s, "b", "DOGE", 50.0, 1.0, 0.0, 10_000.0);
        fill(&mut s, "b", "DOGE", 450.0, 1.0, 1800.0, 10_000.0);
        assert_eq!(read(&s, "sm-playbook-30m-1t-M", "DOGE", T0 + 1_860_000).side, 1.0);
        // A flip by one trader, both sides 10%.
        fill(&mut s, "c", "BTC", 1.0, 1000.0, 0.0, 10_000.0);
        fill(&mut s, "c", "BTC", -2.0, 1000.0, 7200.0, 10_000.0);
        assert_eq!(read(&s, "sm-flip-15m-1t-M", "BTC", T0 + 7_260_000).side, -1.0);
    }

    #[test]
    fn entries_scored_and_rated() {
        let mut s = Smart::default();
        let mut p = Prices::new(0);
        // The price rises 1% an hour; a buys every hour and sells 10 min later (scored as the run
        // goes, every minute).
        let price = |t: u64| 100.0 * (1.0 + 0.01 * (t - T0) as f64 / 3_600_000.0);
        let end = T0 + 13 * 3_600_000;
        for t in (T0..=end).step_by(60_000) {
            p.sample("BTC", t, price(t));
            if (t - T0) % 3_600_000 == 0 && t < T0 + 12 * 3_600_000 {
                s.on_fill("a", "BTC", 0.0, 1.0, price(t), t, 1000.0, &p);
                s.on_fill("a", "BTC", 1.0, -1.0, price(t + 600_000), t + 600_000, 1000.0, &p);
            }
            s.tick(t, &p);
        }
        let sc = &s.scores["a"];
        assert_eq!((sc.h[H15].n, sc.h[H60].n, sc.coin["BTC"].n, sc.dir[0].n, sc.size.n), (12.0, 12.0, 12.0, 12.0, 12.0));
        // +1% an hour on a price from 100 to 111: 90-100 bps.
        assert!(sc.h[H60].mean() > 90.0 && sc.h[H60].mean() < 100.0);
        assert!((sc.size.mean() - 105f64.ln()).abs() < 0.01);
        // Every entry went its way, none against.
        assert_eq!((sc.excursions, sc.mae), (12.0, 0.0));
        assert!(sc.mfe > 0.0);
        // Its next entry: by a specialist (coin and direction), an early trader (every entry up
        // at 15 min), one with clean entries; and by its copy's record (`risk_adj`).
        s.on_fill("a", "BTC", 0.0, 1.0, price(end), end, 1000.0, &p);
        let now = end + 1000;
        for name in ["sm-spec-15m-1t-M", "sm-early-15m-1t-M", "sm-exec-15m-1t-M", "sm-recent-15m-2t-M"] {
            let rd = read(&s, name, "BTC", now);
            assert_eq!((name, rd.longs), (name, 1));
        }
        assert_eq!(read(&s, "sm-spec-15m-1t-M", "BTC", now).side, 1.0);
        assert_eq!(read(&s, "sm-recent-15m-2t-M", "BTC", now).side, 0.0); // 2 needed
        assert_eq!(read(&s, "sm-riskadj-15m-2t-M", "BTC", now).longs, 0);
        let risk: HashSet<String> = ["a".to_string()].into();
        assert_eq!(read_with(&s, "sm-riskadj-15m-2t-M", "BTC", now, &risk, None).longs, 1);
        // Nobody follows it in: not a leader.
        assert_eq!(read(&s, "sm-leader-5m-1t-M", "BTC", now).longs, 0);
        assert!(s.top_tier().contains("a"));
    }

    #[test]
    fn saved_and_loaded() {
        let mut s = Smart::default();
        fill(&mut s, "a", "BTC", 1.0, 100.0, 0.0, 1000.0);
        s.scores.entry("a".into()).or_default().h[0].add(5.0);
        let back = Smart::load(Some(&s.saved()), 0);
        assert_eq!(back.pos["a"]["BTC"].size, 1.0);
        assert_eq!(back.scores["a"].h[0].n, 1.0);
        // Read from the exchange: a position it holds the other way now, one closed.
        let mut back = back;
        back.sync("a", &[("ETH".to_string(), -2.0)].into(), |_| Some(50.0));
        assert!(!back.pos["a"].contains_key("BTC"));
        assert_eq!((back.pos["a"]["ETH"].size, back.pos["a"]["ETH"].entry, back.pos["a"]["ETH"].opened_ms), (-2.0, 50.0, None));
    }
}
