//! Messages to a Telegram chat (`TELEGRAM_BOT_TOKEN`, a bot's token from @BotFather, and
//! `TELEGRAM_CHAT_ID`, the chat it writes to): sent in the background, in order; nothing without
//! them. A failed send is logged and dropped (the trading never waits on it).

use std::sync::OnceLock;

use serde_json::json;
use tokio::sync::mpsc;

use crate::log;

/// Whether the paper side reaches the chat: signals with no trade and their paper results (the HIP-3 dip, the dip,
/// the pump short, the weekend / US-night dips' paper, the RSI dip's paper, the trials-over signals). Off since
/// 2026-11-12 (the user's call: a paper "-4.85%" read next to a real +$23 on the same stock); all of it stays in the
/// events table for the paper-vs-live comparison.
pub const PAPER_TO_CHAT: bool = false;

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
    // the tests never reach the chat, whatever the environment holds
    if cfg!(test) {
        return;
    }
    if let Some(tx) = sender() {
        let _ = tx.send(text.into());
    }
}

/// Sends `text` now, on a runtime of its own (the bot's may be stuck): the watchdog's last word. Waits at most 10 s.
pub fn send_now(text: &str) {
    if cfg!(test) {
        return;
    }
    let (Ok(token), Ok(chat)) = (std::env::var("TELEGRAM_BOT_TOKEN"), std::env::var("TELEGRAM_CHAT_ID")) else { return };
    let (token, chat) = (token.trim().to_string(), chat.trim().to_string());
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
    rt.block_on(async move {
        let Ok(http) = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10)).build() else { return };
        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        if let Err(e) = http.post(&url).json(&json!({"chat_id": chat, "text": text, "disable_web_page_preview": true})).send().await {
            log!("telegram: not sent: {e}");
        }
    });
}

/// A price for the chat: 6 significant digits, no float tail (107.77919999999999 -> 107.779).
pub fn px(x: f64) -> String {
    if x == 0.0 || !x.is_finite() {
        return format!("{x}");
    }
    let prec = (5 - x.abs().log10().floor() as i32).max(0) as usize;
    let t = format!("{x:.prec$}");
    if t.contains('.') { t.trim_end_matches('0').trim_end_matches('.').to_string() } else { t }
}

/// A $ amount signed for the chat: +$1.50 / -$2.00.
pub fn usd(x: f64) -> String {
    if x >= 0.0 { format!("+${x:.2}") } else { format!("-${:.2}", -x) }
}

#[cfg(test)]
mod usd_tests {
    #[test]
    fn a_signed_amount() {
        assert_eq!((super::usd(1.5), super::usd(-2.0), super::usd(0.0)), ("+$1.50".to_string(), "-$2.00".to_string(), "+$0.00".to_string()));
    }
}
