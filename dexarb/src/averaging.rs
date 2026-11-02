//! The basis-averaging simulator (deliverable 4) on the live books: layers of LONG A / SHORT B entered as the mid basis
//! (B over A) reaches each step; each layer pays its spread + impact on the books walked for the CUMULATIVE size (the book
//! does not refill between the layers - the conservative side), the size-weighted average entry basis, the margin each
//! layer adds, and the PnL of closing everything at a target basis. Compared with ONE entry of the same total size at the
//! first step: if the ladder wins only by holding more, it is not better.
//!
//!   dexarb ladder ASSET LONG SHORT USD_PER_LAYER STEP% [STEP% ...] [--exit X%] [--lev L]

use crate::exec;
use crate::http::Http;
use crate::model::*;
use crate::registry::Registry;
use crate::venues::V;
use anyhow::{Result, bail};

pub struct Layer {
    pub basis: f64,
    pub usd: f64,
    pub cost: f64,
    pub margin: f64,
}

/// The layers' cost on the books for the cumulative size: (the extra cost of this layer, $).
pub fn layer_cost(
    ba: &Book,
    bb: &Book,
    ma: D,
    mb: D,
    taker: f64,
    cum_before: f64,
    usd: f64,
) -> Option<f64> {
    let cost = |sz: f64| -> Option<f64> {
        if sz <= 0.0 {
            return Some(0.0);
        }
        let la = exec::walk(ba, true, df(sz))?;
        let sb = exec::walk_qty(bb, false, la.qty * ma / mb)?;
        if !la.complete || !sb.complete {
            return None;
        }
        let mid_a = f(exec::mid(ba)?);
        let mid_b = f(exec::mid(bb)?);
        Some(f(la.qty) * (f(la.vwap) - mid_a) + f(sb.qty) * (mid_b - f(sb.vwap)) + taker * 2.0 * sz)
    };
    Some(cost(cum_before + usd)? - cost(cum_before)?)
}

pub async fn cli(h: &Http, a: &[String]) -> Result<()> {
    if a.len() < 5 {
        bail!("ladder ASSET LONG SHORT USD_PER_LAYER STEP% [STEP% ...] [--exit X%] [--lev L]");
    }
    let asset = a[0].to_uppercase();
    let (lv, sv) = (
        V::from_name(&a[1]).ok_or_else(|| anyhow::anyhow!("venue {}", a[1]))?,
        V::from_name(&a[2]).ok_or_else(|| anyhow::anyhow!("venue {}", a[2]))?,
    );
    let usd: f64 = a[3].parse()?;
    let mut steps = Vec::new();
    let (mut exit, mut lev) = (0.10, 3.0);
    let mut i = 4;
    while i < a.len() {
        match a[i].as_str() {
            "--exit" => {
                exit = a
                    .get(i + 1)
                    .and_then(|x| x.trim_end_matches('%').parse().ok())
                    .unwrap_or(0.10);
                i += 1;
            }
            "--lev" => {
                lev = a.get(i + 1).and_then(|x| x.parse().ok()).unwrap_or(3.0);
                i += 1;
            }
            x => steps.push(x.trim_end_matches('%').parse::<f64>()?),
        }
        i += 1;
    }
    let reg = Registry::load(h, &[lv, sv]).await;
    let pick = |v: V| {
        reg.by_venue
            .get(&v)
            .and_then(|x| x.iter().find(|i| i.base == asset && i.active))
            .cloned()
    };
    let (ia, ib) = (
        pick(lv).ok_or_else(|| anyhow::anyhow!("{asset} not on {}", lv.name()))?,
        pick(sv).ok_or_else(|| anyhow::anyhow!("{asset} not on {}", sv.name()))?,
    );
    let (ba, bb) = (lv.book(h, &ia).await?, sv.book(h, &ib).await?);
    let taker = (f(ia.fees.taker) + f(ib.fees.taker)) / 2.0;
    let mid_basis = f(exec::mid(&bb).unwrap_or_default() / ib.multiplier)
        / f(exec::mid(&ba).unwrap_or_default() / ia.multiplier)
        - 1.0;
    println!(
        "{asset} LONG {} / SHORT {}: the mid basis now {:+.4}%; layers of ${usd} at {:?}%, the exit at {exit}%, {lev}x",
        ia.venue,
        ib.venue,
        mid_basis * 100.0,
        steps
    );
    let mut layers: Vec<Layer> = Vec::new();
    let mut cum = 0.0;
    for &s in &steps {
        let Some(c) = layer_cost(&ba, &bb, ia.multiplier, ib.multiplier, taker, cum, usd) else {
            println!(
                "  step {s}%: NOT executable on the books at ${} cumulative - the ladder stops",
                cum + usd
            );
            break;
        };
        cum += usd;
        layers.push(Layer {
            basis: s / 100.0,
            usd,
            cost: c,
            margin: 2.0 * usd / lev * 1.5,
        });
    }
    let total: f64 = layers.iter().map(|l| l.usd).sum();
    let mut acc = 0.0;
    println!(
        "  layer | basis   | layer cost $ | avg entry basis | margin $ (cum) | PnL at the exit (all layers) $"
    );
    for (k, l) in layers.iter().enumerate() {
        acc += l.usd * l.basis;
        let sz: f64 = layers[..=k].iter().map(|x| x.usd).sum();
        let avg = acc / sz;
        let cost: f64 = layers[..=k].iter().map(|x| x.cost).sum();
        let margin: f64 = layers[..=k].iter().map(|x| x.margin).sum();
        // closing: the basis captured (entry - exit) on each layer, the close's cost ~ the opens' (today's books)
        let pnl = layers[..=k]
            .iter()
            .map(|x| x.usd * (x.basis - exit / 100.0))
            .sum::<f64>()
            - 2.0 * cost;
        println!(
            "  {:>5} | {:+.3}% | {:>12.3} | {:>+14.4}% | {:>14.2} | {:>+.3}",
            k + 1,
            l.basis * 100.0,
            l.cost,
            avg * 100.0,
            margin,
            pnl
        );
    }
    if let Some(first) = layers.first() {
        let c = layer_cost(&ba, &bb, ia.multiplier, ib.multiplier, taker, 0.0, total);
        match c {
            Some(c) => println!(
                "  vs ONE entry of ${total} at {:+.3}%: PnL at the exit {:+.3} $ (if the basis only reaches the first step, the ladder holds {:.0}% of that size)",
                first.basis * 100.0,
                total * (first.basis - exit / 100.0) - 2.0 * c,
                first.usd / total * 100.0
            ),
            None => println!("  ONE entry of ${total}: not executable on the books"),
        }
        println!(
            "  the risk: the margin rises with each layer; if the basis keeps widening past the last step the loss is ${:.2} per +0.1% (all layers)",
            total * 0.001
        );
    }
    Ok(())
}
