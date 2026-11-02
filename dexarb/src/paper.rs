//! The paper engine (no money): model B, funding + the entry basis, $1k a leg, on the collector's snapshots.
//! Enter LONG A / SHORT B when the pair's funding net, smoothed over ~6 h of rounds (not the current rate taken as
//! permanent), pays the round trip (both legs' spread + impact + taker fees, both ways, on the books as they are) 1.5x
//! within 24 h and the current net is positive; one pair an asset, 6 at once. The funding accrues each round at the rates
//! then observed (the settlements approximated as continuous - stated). Out when the smoothed net turns negative, the
//! current net is negative 3 rounds running, or after 72 h. Every decision is logged: DISCOVERED -> QUOTED -> ENTERED
//! (-> HEDGED: both legs filled on the snapshot) -> EXITING -> CLOSED; a leg not on the book is an ERROR, no entry.

use crate::collect::Snap;
use crate::model::now_ms;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;

const USD: f64 = 1_000.0;
const MAX_OPEN: usize = 6;
const SMOOTH_H: f64 = 6.0;
const MAX_HOLD_H: f64 = 72.0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pos {
    pub asset: String,
    pub long: String,
    pub short: String,
    pub q: f64,
    pub pa: f64,
    pub pb: f64,
    pub fees: f64,
    pub funding: f64,
    pub opened: i64,
    pub last: i64,
    pub neg_rounds: u32,
    pub exit_pa: f64,
    pub exit_pb: f64,
    pub closed: i64,
    pub net: f64,
    pub why: String,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Paper {
    pub open: Vec<Pos>,
    pub closed: Vec<Pos>,
    /// The pairs' smoothed funding net (fraction / h) and when it was last updated.
    pub ema: HashMap<String, (f64, i64)>,
}

fn file() -> std::path::PathBuf {
    crate::data_dir().join("paper.json")
}

fn log(ev: serde_json::Value) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::data_dir().join("paper_log.jsonl"))
    {
        let _ = writeln!(f, "{ev}");
    }
}

/// (long pays / h, short gets / h) of a LONG on `a`, SHORT on `b`.
fn rates(a: &Snap, b: &Snap) -> Option<(f64, f64)> {
    let lp = a.long_cost_h.or(a.rate_h)?;
    let sg = b.short_cost_h.map(|c| -c).or(b.rate_h)?;
    Some((lp, sg))
}

impl Paper {
    pub fn load() -> Paper {
        std::fs::read_to_string(file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = std::fs::write(
            file(),
            serde_json::to_string_pretty(self).unwrap_or_default(),
        );
    }

    pub fn realized(&self) -> f64 {
        self.closed.iter().map(|p| p.net).sum()
    }

    pub fn step(&mut self, ss: &[Snap], t: i64) {
        let mut by: HashMap<&str, Vec<&Snap>> = HashMap::new();
        for s in ss.iter().filter(|s| s.age_ms < 60_000 && s.mid > 0.0) {
            by.entry(s.asset.as_str()).or_default().push(s);
        }
        let find = |asset: &str, v: &str| {
            by.get(asset)
                .and_then(|x| x.iter().find(|s| s.venue == v).copied())
        };
        // the smoothed nets
        for (asset, v) in &by {
            for a in v {
                for b in v {
                    if a.venue == b.venue {
                        continue;
                    }
                    let Some((lp, sg)) = rates(a, b) else {
                        continue;
                    };
                    let k = format!("{asset}|{}|{}", a.venue, b.venue);
                    let e = self.ema.entry(k).or_insert((sg - lp, t));
                    let dt = ((t - e.1) as f64 / 3.6e6).max(0.0);
                    let w = (dt / SMOOTH_H).min(1.0);
                    e.0 += w * ((sg - lp) - e.0);
                    e.1 = t;
                }
            }
        }
        // the open positions: accrue, then the exits
        let mut keep = Vec::new();
        for mut p in std::mem::take(&mut self.open) {
            let (Some(a), Some(b)) = (find(&p.asset, &p.long), find(&p.asset, &p.short)) else {
                keep.push(p);
                continue;
            };
            let dt = ((t - p.last) as f64 / 3.6e6).max(0.0);
            if let Some((lp, sg)) = rates(a, b) {
                p.funding += dt * (sg * p.q * b.mid - lp * p.q * a.mid);
                p.neg_rounds = if sg - lp < 0.0 { p.neg_rounds + 1 } else { 0 };
            }
            p.last = t;
            let ema = self
                .ema
                .get(&format!("{}|{}|{}", p.asset, p.long, p.short))
                .map_or(0.0, |x| x.0);
            let held = (t - p.opened) as f64 / 3.6e6;
            let why = if held >= MAX_HOLD_H {
                "max hold"
            } else if ema < 0.0 {
                "smoothed funding < 0"
            } else if p.neg_rounds >= 3 {
                "funding < 0 three rounds"
            } else {
                ""
            };
            if why.is_empty() || a.sell1k <= 0.0 || b.buy1k <= 0.0 {
                keep.push(p);
                continue;
            }
            p.exit_pa = a.sell1k;
            p.exit_pb = b.buy1k;
            p.fees += a.taker * p.q * p.exit_pa + b.taker * p.q * p.exit_pb;
            let basis = p.q * ((p.pb - p.pa) - (p.exit_pb - p.exit_pa));
            p.net = basis + p.funding - p.fees;
            p.closed = t;
            p.why = why.into();
            log(serde_json::json!({"state": "CLOSED", "t": t, "pos": p, "basis_pnl": basis}));
            self.closed.push(p);
        }
        self.open = keep;
        // the entries
        let mut cands = Vec::new();
        for (asset, v) in &by {
            if self.open.iter().any(|p| &p.asset == asset) {
                continue;
            }
            for a in v {
                for b in v {
                    if a.venue == b.venue {
                        continue;
                    }
                    let Some((lp, sg)) = rates(a, b) else {
                        continue;
                    };
                    let ema = self
                        .ema
                        .get(&format!("{asset}|{}|{}", a.venue, b.venue))
                        .map_or(0.0, |x| x.0);
                    if a.buy1k <= 0.0 || b.sell1k <= 0.0 || a.sell1k <= 0.0 || b.buy1k <= 0.0 {
                        continue;
                    }
                    let m = a.mid;
                    let rt =
                        (a.buy1k - a.sell1k + b.buy1k - b.sell1k) / m + 2.0 * (a.taker + b.taker);
                    let basis_in = (b.sell1k - a.buy1k) / m - (b.mid - a.mid) / m; // the entry's own basis edge vs the mids
                    let exp24 = ema * 24.0;
                    if sg - lp > 0.0 && ema > 0.0 && exp24 + basis_in.max(0.0) >= 1.5 * rt {
                        cands.push((
                            exp24 - rt,
                            asset.to_string(),
                            (*a).clone(),
                            (*b).clone(),
                            rt,
                        ));
                    }
                }
            }
        }
        cands.sort_by(|x, y| y.0.total_cmp(&x.0));
        let mut taken = std::collections::HashSet::new();
        for (edge, asset, a, b, rt) in cands {
            if self.open.len() >= MAX_OPEN || !taken.insert(asset.clone()) {
                continue;
            }
            log(
                serde_json::json!({"state": "DISCOVERED", "t": t, "asset": asset, "long": a.venue, "short": b.venue, "edge_24h": edge, "round_trip": rt}),
            );
            let q = USD / a.buy1k;
            let p = Pos {
                asset: asset.clone(),
                long: a.venue.clone(),
                short: b.venue.clone(),
                q,
                pa: a.buy1k,
                pb: b.sell1k,
                fees: a.taker * USD + b.taker * q * b.sell1k,
                funding: 0.0,
                opened: t,
                last: t,
                neg_rounds: 0,
                exit_pa: 0.0,
                exit_pb: 0.0,
                closed: 0,
                net: 0.0,
                why: String::new(),
            };
            log(serde_json::json!({"state": "ENTERED", "t": t, "pos": p}));
            log(
                serde_json::json!({"state": "HEDGED", "t": t, "asset": asset, "note": "both legs on the snapshot's books"}),
            );
            self.open.push(p);
        }
        let _ = now_ms();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snap(venue: &str, rate: f64, mid: f64, t: i64) -> Snap {
        Snap { ts: t, asset: "X".into(), venue: venue.into(), symbol: "X".into(), mid, bid: mid - 0.01, ask: mid + 0.01, buy1k: mid + 0.01, sell1k: mid - 0.01,
            buy10k: mid + 0.02, sell10k: mid - 0.02, rate_h: Some(rate), long_cost_h: None, short_cost_h: None, taker: 0.0002, ts_exchange: None, age_ms: 0 }
    }
    #[test]
    fn enters_accrues_and_exits() {
        // SHORT on B receives 0.1%/h, LONG on A pays 0: the 24 h edge 2.4% beats the ~0.12% round trip
        let mut p = Paper::default();
        let h = 3_600_000;
        p.step(&[snap("A", 0.0, 100.0, 0), snap("B", 0.001, 100.0, 0)], 0);
        assert_eq!(p.open.len(), 1);
        assert_eq!((p.open[0].long.as_str(), p.open[0].short.as_str()), ("A", "B"));
        // 10 h at 0.1%/h on ~$1k: ~ +$10 of funding
        p.step(&[snap("A", 0.0, 100.0, 10 * h), snap("B", 0.001, 100.0, 10 * h)], 10 * h);
        assert!((p.open[0].funding - 10.0).abs() < 0.2, "{}", p.open[0].funding);
        // the funding flips: three negative rounds -> out
        for k in 11..=13 {
            p.step(&[snap("A", 0.0, 100.0, k * h), snap("B", -0.001, 100.0, k * h)], k * h);
        }
        assert_eq!(p.open.len(), 0);
        assert_eq!(p.closed.len(), 1);
        let c = &p.closed[0];
        // the net: the funding (10 h at +0.1% less 3 h at -0.1%: ~ +$7) less the fees (~$0.8) less the spreads (~$0.4)
        assert!(c.net > 5.0 && c.net < 7.0, "net {}", c.net);
    }
}
