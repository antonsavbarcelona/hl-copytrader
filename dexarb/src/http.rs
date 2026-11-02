//! One HTTP client: timeouts, retries with backoff on 429 / 5xx / network errors, the raw answers optionally kept.

use anyhow::{Result, bail};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone)]
pub struct Http {
    c: reqwest::Client,
    pub raw_dir: Option<std::path::PathBuf>,
}

impl Http {
    pub fn new() -> Self {
        let c = reqwest::Client::builder()
            .timeout(Duration::from_secs(25))
            .user_agent("dexarb-research/0.1 (read-only)")
            .gzip(true)
            .build()
            .expect("http client");
        Http { c, raw_dir: None }
    }

    pub async fn get(&self, url: &str) -> Result<Value> {
        self.send(url, None).await
    }

    pub async fn post(&self, url: &str, body: &Value) -> Result<Value> {
        self.send(url, Some(body)).await
    }

    async fn send(&self, url: &str, body: Option<&Value>) -> Result<Value> {
        let mut wait = 500u64;
        let mut last = String::new();
        for _ in 0..4 {
            let rq = match body {
                Some(b) => self.c.post(url).json(b),
                None => self.c.get(url),
            };
            match rq.send().await {
                Ok(r) => {
                    let st = r.status();
                    let txt = r.text().await.unwrap_or_default();
                    if st.is_success() {
                        if let Some(dir) = &self.raw_dir {
                            let _ = std::fs::create_dir_all(dir);
                            let name = url
                                .chars()
                                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                                .take(120)
                                .collect::<String>();
                            let _ = std::fs::write(dir.join(format!("{name}.json")), &txt);
                        }
                        return serde_json::from_str(&txt).map_err(|e| {
                            anyhow::anyhow!("{url}: not JSON ({e}): {}", &txt[..txt.len().min(160)])
                        });
                    }
                    last = format!("{st}: {}", &txt[..txt.len().min(200)]);
                    if !(st.as_u16() == 429 || st.is_server_error()) {
                        bail!("{url}: {last}");
                    }
                    if st.as_u16() == 429 {
                        // throttled: a longer pause (2, 4, 8, 16 s)
                        wait = wait.max(2_000);
                    }
                }
                Err(e) => last = format!("{e}"),
            }
            tokio::time::sleep(Duration::from_millis(wait)).await;
            wait *= 2;
        }
        bail!("{url}: {last}")
    }
}

pub fn s(v: &Value) -> String {
    match v {
        Value::String(x) => x.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    }
}

pub fn num(v: &Value) -> f64 {
    match v {
        Value::String(x) => x.parse().unwrap_or(0.0),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}
