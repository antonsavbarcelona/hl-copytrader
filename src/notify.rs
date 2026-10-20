//! Messages to a Telegram chat (`TELEGRAM_BOT_TOKEN`, a bot's token from @BotFather, and
//! `TELEGRAM_CHAT_ID`, the chat it writes to): sent in the background, in order; nothing without
//! them. A failed send is logged and dropped (the trading never waits on it).

use std::sync::OnceLock;

use serde_json::json;
use tokio::sync::mpsc;

use crate::log;

static TX: OnceLock<Option<mpsc::UnboundedSender<String>>> = OnceLock::new();

fn sender() -> &'static Option<mpsc::UnboundedSender<String>> {
    TX.get_or_init(|| {
        let token = std::env::var("TELEGRAM_BOT_TOKEN").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())?;
        let chat = std::env::var("TELEGRAM_CHAT_ID").ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())?;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        log!("telegram: on, chat {chat}");
        tokio::spawn(async move {
            let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build().expect("http client");
            let url = format!("https://api.telegram.org/bot{token}/sendMessage");
            while let Some(text) = rx.recv().await {
                let r = http.post(&url).json(&json!({"chat_id": chat, "text": text, "disable_web_page_preview": true})).send().await;
                match r {
                    Ok(r) if r.status().is_success() => {}
                    Ok(r) => log!("telegram: not sent ({})", r.status()),
                    Err(e) => log!("telegram: not sent: {e}"),
                }
            }
        });
        Some(tx)
    })
}

/// Sends `text` to the chat (nothing if it is not set).
pub fn send(text: impl Into<String>) {
    if let Some(tx) = sender() {
        let _ = tx.send(text.into());
    }
}
