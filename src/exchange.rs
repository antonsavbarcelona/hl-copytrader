//! Orders on a Hyperliquid account: the `/exchange` endpoint, its actions signed as the
//! Python SDK signs them (`sign_l1_action`): the action's msgpack + nonce + no vault, hashed
//! (keccak), signed as the EIP-712 `Agent { source, connectionId }` ("a" mainnet, "b"
//! testnet). The key may be the account's own or an API wallet ("agent") approved for it.

use anyhow::{Context, Result, bail};
use k256::ecdsa::SigningKey;
use serde::Serialize;
use serde_json::{Value, json};
use sha3::{Digest, Keccak256};

pub const TESTNET: &str = "https://api.hyperliquid-testnet.xyz";
pub const MAINNET: &str = "https://api.hyperliquid.xyz";

#[derive(Serialize)]
pub struct OrderWire {
    pub a: u32,
    pub b: bool,
    pub p: String,
    pub s: String,
    pub r: bool,
    pub t: OrderType,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub enum OrderType {
    Limit { tif: String },
    Trigger {
        #[serde(rename = "isMarket")]
        is_market: bool,
        #[serde(rename = "triggerPx")]
        trigger_px: String,
        tpsl: String,
    },
}

#[derive(Serialize)]
pub struct CancelWire {
    pub a: u32,
    pub o: u64,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Action {
    Order { orders: Vec<OrderWire>, grouping: String },
    Cancel { cancels: Vec<CancelWire> },
}

/// A number as the API wants it: up to 8 decimals, no trailing zeros (`float_to_wire`).
pub fn wire(x: f64) -> String {
    let s = format!("{x:.8}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" || s.is_empty() { "0".into() } else { s.to_string() }
}

/// A perp price the exchange accepts: 5 significant figures, at most 6 - sz decimals.
pub fn round_px(px: f64, sz_decimals: u32) -> f64 {
    if px <= 0.0 {
        return 0.0;
    }
    let mag = px.log10().floor() as i32;
    let sig = 10f64.powi(4 - mag);
    let px = (px * sig).round() / sig;
    let d = 10f64.powi(6 - sz_decimals as i32);
    (px * d).round() / d
}

fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// The action's hash that is signed (`action_hash`, no vault, no expiry).
pub fn action_hash(action: &Action, nonce: u64) -> Result<[u8; 32]> {
    let mut data = rmp_serde::to_vec_named(action)?;
    data.extend_from_slice(&nonce.to_be_bytes());
    data.push(0);
    Ok(keccak(&data))
}

/// The EIP-712 digest of `Agent { source, connectionId }` in the "Exchange" domain.
fn agent_digest(connection_id: [u8; 32], mainnet: bool) -> [u8; 32] {
    let domain_type = keccak(b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)");
    let mut chain = [0u8; 32];
    chain[24..].copy_from_slice(&1337u64.to_be_bytes());
    let domain = keccak(&[domain_type, keccak(b"Exchange"), keccak(b"1"), chain, [0u8; 32]].concat());
    let agent_type = keccak(b"Agent(string source,bytes32 connectionId)");
    let agent = keccak(&[agent_type, keccak(if mainnet { b"a" } else { b"b" }), connection_id].concat());
    keccak(&[&[0x19u8, 0x01][..], &domain, &agent].concat())
}

fn hex_int(b: &[u8]) -> String {
    let h = hex::encode(b);
    let h = h.trim_start_matches('0');
    format!("0x{}", if h.is_empty() { "0" } else { h })
}

/// `{r, s, v}` over the action, as `sign_l1_action` makes it.
pub fn sign(key: &SigningKey, action: &Action, nonce: u64, mainnet: bool) -> Result<Value> {
    let digest = agent_digest(action_hash(action, nonce)?, mainnet);
    let (sig, rec) = key.sign_prehash_recoverable(&digest)?;
    let (r, s) = sig.split_bytes();
    Ok(json!({"r": hex_int(&r), "s": hex_int(&s), "v": 27 + rec.to_byte() as u64}))
}

/// The address a key signs as (lowercase 0x hex).
pub fn address(key: &SigningKey) -> String {
    let point = key.verifying_key().to_encoded_point(false);
    let hash = keccak(&point.as_bytes()[1..]);
    format!("0x{}", hex::encode(&hash[12..]))
}

pub struct Exchange {
    http: reqwest::Client,
    base: String,
    key: SigningKey,
    mainnet: bool,
    last_nonce: u64,
}

impl Exchange {
    pub fn new(base: &str, key_hex: &str) -> Result<Self> {
        let bytes = hex::decode(key_hex.trim().trim_start_matches("0x")).context("key is not hex")?;
        let key = SigningKey::from_slice(&bytes).context("not a private key")?;
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20)).build()?;
        Ok(Self { http, base: base.trim_end_matches('/').to_string(), key, mainnet: base.contains("api.hyperliquid.xyz"), last_nonce: 0 })
    }

    pub fn mainnet(&self) -> bool {
        self.mainnet
    }

    pub fn signer(&self) -> String {
        address(&self.key)
    }

    pub async fn info(&self, body: Value) -> Result<Value> {
        let r = self.http.post(format!("{}/info", self.base)).json(&body).send().await?;
        let status = r.status();
        let v: Value = r.json().await?;
        if !status.is_success() {
            bail!("info {status}: {v}");
        }
        Ok(v)
    }

    /// Signs and sends `action`; the reply's `response` (an error if it is not "ok").
    pub async fn act(&mut self, action: &Action) -> Result<Value> {
        let nonce = (crate::api::now() * 1000.0) as u64;
        // Nonces must differ: two actions in one millisecond get consecutive ones.
        let nonce = nonce.max(self.last_nonce + 1);
        self.last_nonce = nonce;
        let signature = sign(&self.key, action, nonce, self.mainnet)?;
        let body = json!({"action": action, "nonce": nonce, "signature": signature, "vaultAddress": null, "expiresAfter": null});
        let r = self.http.post(format!("{}/exchange", self.base)).json(&body).send().await?;
        let status = r.status();
        let text = r.text().await?;
        let v: Value = serde_json::from_str(&text).with_context(|| format!("exchange {status}: {text}"))?;
        if v["status"] != "ok" {
            bail!("exchange: {v}");
        }
        Ok(v["response"].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_on_the_wire() {
        assert_eq!(wire(82605.0), "82605");
        assert_eq!(wire(0.00015), "0.00015");
        assert_eq!(wire(1.10), "1.1");
        assert_eq!(wire(0.0), "0");
        assert_eq!(round_px(82605.123, 5), 82605.0);
        assert_eq!(round_px(0.0591234, 1), 0.05912);
        assert_eq!(round_px(1.234567, 0), 1.2346);
    }

    /// The same as the Python SDK's `action_hash` / `sign_l1_action` for these actions.
    #[test]
    fn signs_as_the_sdk() {
        let key = SigningKey::from_slice(&hex::decode("0123456789012345678901234567890123456789012345678901234567890123").unwrap()).unwrap();
        assert_eq!(address(&key), "0x14791697260e4c9a71f18484c9f997b308e59325");
        let nonce = 1791540000123;
        let order = Action::Order {
            orders: vec![OrderWire { a: 3, b: true, p: wire(82605.0), s: wire(0.00015), r: false, t: OrderType::Limit { tif: "Ioc".into() } }],
            grouping: "na".into(),
        };
        let trigger = Action::Order {
            orders: vec![OrderWire { a: 12, b: false, p: "0.04728".into(), s: "1682.5".into(), r: true,
                t: OrderType::Trigger { is_market: true, trigger_px: "0.05254".into(), tpsl: "sl".into() } }],
            grouping: "na".into(),
        };
        let cancel = Action::Cancel { cancels: vec![CancelWire { a: 12, o: 62246465112 }] };
        let cases = [
            (&order, "08458109a06a62044f3c37e98c589bbe4d30bda32b2aa40e056da1e3841d86ec", false,
             "0x160e61a7191b20d63231960bd6442f3e4981d2edbe6f56d585d2914a84d51720", "0x479a6708b73ac578b303098d68ae3b3fca79557a013ea017e97ed665d26fba37", 27),
            (&order, "08458109a06a62044f3c37e98c589bbe4d30bda32b2aa40e056da1e3841d86ec", true,
             "0x89b8027031cde3b909608982c1f72e9ce32a12d3ad44e673589296eef162693e", "0x19b1c5d0769ae9960523e64a48f2a6f69c83ce27f252a0fae9cf8b5d431742bf", 28),
            (&trigger, "5b8bb00905ad4ea5f1e2e0980577dbe30eb36b80b4b93b00e2b653ab41981e3c", false,
             "0xb35207f67a8a6c0df407ecb3e5bfec6a3d887617bd666e95ac1a67e960fc36fd", "0x3f859a44564d5329938cb02a4c20b250f22766b9fd080f27227ae0f5c2e6cfc2", 28),
            (&cancel, "f220594006ae723ef922ef6ef87aaf9fbb774fef0ca1c9f20b59ac15be10e66c", false,
             "0x4cf57a35df1d217496c89caf8d112871194c51f673b0401419e14ce402cd9e04", "0xf782832a27050f5c4edcf1977695f88afd883e1fb88c50a3f4ee9a72ff7c23d", 27),
        ];
        for (action, hash, mainnet, r, s, v) in cases {
            assert_eq!(hex::encode(action_hash(action, nonce).unwrap()), hash);
            assert_eq!(sign(&key, action, nonce, mainnet).unwrap(), json!({"r": r, "s": s, "v": v}));
        }
    }
}
