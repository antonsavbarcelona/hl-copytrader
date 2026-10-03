//! Hyperliquid's public websocket: every trade on every perp coin (each names its buyer and
//! seller, so followed accounts' fills are picked out of it) and the live L2 books the paper
//! copies are filled against. For the watched coins (open signal trades and their limit-order
//! twins) every trade and every book change is passed on too.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::RwLock;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::api::{exchange_now, num};
use crate::log;

const WS: &str = "wss://api.hyperliquid.xyz/ws";
const SILENCE_S: u64 = 30;

/// One account's side of a trade.
#[derive(Clone, Debug)]
pub struct UserFill {
    pub user: String,
    pub coin: String,
    /// + bought, - sold (signed size).
    pub delta: f64,
    pub px: f64,
    pub time_ms: u64,
    /// The exchange's trade id (one trade, both its sides).
    pub tid: u64,
    /// When it reached us, on the exchange's clock.
    pub recv: f64,
}

/// What the websockets pass on to the engine.
#[derive(Debug)]
pub enum Feed {
    Fill(UserFill),
    /// A trade in a watched coin (`taker_buy`: its aggressor bought).
    Print { coin: String, px: f64, sz: f64, taker_buy: bool, time_ms: u64 },
    /// A watched coin's book changed (it is in `Books`).
    Book(String),
}

pub type Watched = Arc<RwLock<HashSet<String>>>;

#[derive(Clone, Debug, Default)]
pub struct Book {
    /// (price, size), best first.
    pub bids: Vec<(f64, f64)>,
    pub asks: Vec<(f64, f64)>,
    pub time_ms: u64,
}

impl Book {
    pub fn mid(&self) -> Option<f64> {
        Some((self.bids.first()?.0 + self.asks.first()?.0) / 2.0)
    }
}

pub type Books = Arc<RwLock<HashMap<String, Book>>>;

async fn connect(subs: &[Value]) -> anyhow::Result<
    (futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, Message>,
     futures_util::stream::SplitStream<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>>)>
{
    let (ws, _) = tokio_tungstenite::connect_async(WS).await?;
    let (mut sink, read) = ws.split();
    for s in subs {
        sink.send(Message::Text(json!({"method": "subscribe", "subscription": s}).to_string())).await?;
    }
    Ok((sink, read))
}

/// Runs `on_msg` for every channel message; reconnects on error or silence.
async fn run(name: &str, subs: Vec<Value>, mut on_msg: impl FnMut(&Value)) {
    loop {
        match connect(&subs).await {
            Ok((mut sink, mut read)) => {
                log!("{name}: connected ({} subscriptions)", subs.len());
                let mut ping = tokio::time::interval(Duration::from_secs(20));
                loop {
                    tokio::select! {
                        _ = ping.tick() => {
                            if sink.send(Message::Text(r#"{"method":"ping"}"#.into())).await.is_err() { break; }
                        }
                        m = tokio::time::timeout(Duration::from_secs(SILENCE_S), read.next()) => {
                            let text = match m {
                                Err(_) => { log!("{name}: silent {SILENCE_S} s, reconnecting"); break; }
                                Ok(None) => { log!("{name}: closed, reconnecting"); break; }
                                Ok(Some(Err(e))) => { log!("{name}: {e}, reconnecting"); break; }
                                Ok(Some(Ok(Message::Text(t)))) => t,
                                Ok(Some(Ok(_))) => continue,
                            };
                            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                                on_msg(&v);
                            }
                        }
                    }
                }
            }
            Err(e) => log!("{name}: connect failed: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Fills of the followed accounts, and every trade in a watched coin.
pub async fn run_trades(coins: Vec<String>, followed: Arc<RwLock<HashSet<String>>>, watched: Watched, tx: mpsc::UnboundedSender<Feed>) {
    let subs = coins.iter().map(|c| json!({"type": "trades", "coin": c})).collect();
    // A plain lock, held for one message (never across an await).
    let followed2 = followed.clone();
    // A (re)subscription first sends recent trades again: per coin, the latest trade time seen
    // and the trade ids at it drop those already passed on; before any, trades from before we
    // started are dropped (the positions read at enrolment already hold them).
    let started_ms = (exchange_now() * 1000.0) as u64;
    let mut seen: HashMap<String, (u64, HashSet<u64>)> = HashMap::new();
    run("trades", subs, move |v| {
        if v["channel"] != "trades" {
            return;
        }
        let Some(list) = v["data"].as_array() else { return };
        let mut list: Vec<&Value> = list.iter().collect();
        list.sort_by_key(|t| (t["time"].as_u64().unwrap_or(0), t["tid"].as_u64().unwrap_or(0)));
        let set = followed2.read().unwrap();
        let watch = watched.read().unwrap();
        let recv = exchange_now();
        for t in list {
            let coin = t["coin"].as_str().unwrap_or("").to_string();
            let (time_ms, tid) = (t["time"].as_u64().unwrap_or(0), t["tid"].as_u64().unwrap_or(0));
            let (last, ids) = seen.entry(coin.clone()).or_insert_with(|| (started_ms, HashSet::new()));
            if time_ms < *last || (time_ms == *last && !ids.insert(tid)) {
                continue;
            }
            if time_ms > *last {
                *last = time_ms;
                ids.clear();
                ids.insert(tid);
            }
            if watch.contains(&coin) {
                let _ = tx.send(Feed::Print { coin: coin.clone(), px: num(&t["px"]), sz: num(&t["sz"]), taker_buy: t["side"] == "B", time_ms });
            }
            let users = t["users"].as_array();
            let (Some(buyer), Some(seller)) = (users.and_then(|u| u.first()).and_then(Value::as_str),
                                                users.and_then(|u| u.get(1)).and_then(Value::as_str)) else { continue };
            let (px, sz) = (num(&t["px"]), num(&t["sz"]));
            for (user, sign) in [(buyer.to_lowercase(), 1.0), (seller.to_lowercase(), -1.0)] {
                if set.contains(&user) {
                    let _ = tx.send(Feed::Fill(UserFill { user, coin: coin.clone(), delta: sign * sz, px, time_ms, tid, recv }));
                }
            }
        }
    })
    .await;
    drop(followed);
}

/// Keeps `books` current for every coin; a watched coin's changes are passed on.
pub async fn run_books(coins: Vec<String>, books: Books, watched: Watched, tx: mpsc::UnboundedSender<Feed>) {
    let subs = coins.iter().map(|c| json!({"type": "l2Book", "coin": c})).collect();
    run("books", subs, move |v| {
        if v["channel"] != "l2Book" {
            return;
        }
        let d = &v["data"];
        let side = |i: usize| -> Vec<(f64, f64)> {
            d["levels"][i].as_array().map(|a| a.iter().map(|l| (num(&l["px"]), num(&l["sz"]))).collect()).unwrap_or_default()
        };
        let book = Book { bids: side(0), asks: side(1), time_ms: d["time"].as_u64().unwrap_or(0) };
        if let Some(coin) = d["coin"].as_str() {
            books.write().unwrap().insert(coin.to_string(), book);
            if watched.read().unwrap().contains(coin) {
                let _ = tx.send(Feed::Book(coin.to_string()));
            }
        }
    })
    .await;
}
