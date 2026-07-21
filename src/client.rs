//! HTTP client for the Lofty SDK API (`/public/v1/*`, Bearer API key).
//!
//! Wire contract (the `@loftyaicode/sdk` surface): success → payload JSON
//! as-is; failure → HTTP status
//! plus `{"error": {"code", "message"}}`. Writes require an `Idempotency-Key`
//! header (deduplicated upstream for 24h). Rate limits per key: 300 reads/min
//! and 30 writes/min (60/min per account).

use pk_cli_core::CliError;
use pk_cli_secrets::CredentialStore;
use serde_json::Value;

use crate::config::{Config, KEYCHAIN_ACCOUNT};

pub struct LoftyClient {
    http: reqwest::blocking::Client,
    base: String,
    key: Option<String>,
}

impl LoftyClient {
    /// Client for authenticated calls; errors with exit 3 if no key is stored.
    pub fn new(cfg: &Config, creds: &CredentialStore) -> Result<Self, CliError> {
        let key = creds
            .get(KEYCHAIN_ACCOUNT)?
            .ok_or_else(|| CliError::Auth("no API key stored — run `lofty auth login`".into()))?;
        Ok(Self {
            http: pk_cli_http::client("lofty", env!("CARGO_PKG_VERSION"))?,
            base: cfg.base_url(),
            key: Some(key.expose().to_string()),
        })
    }

    /// Client with an explicit key (used by `auth login --no-verify=false` to
    /// verify a candidate key before storing it).
    pub fn with_key(cfg: &Config, key: &str) -> Result<Self, CliError> {
        Ok(Self {
            http: pk_cli_http::client("lofty", env!("CARGO_PKG_VERSION"))?,
            base: cfg.base_url(),
            key: Some(key.to_string()),
        })
    }

    /// Unauthenticated client (internal `/prod` passthrough; some reads are open).
    pub fn anonymous(base: &str) -> Result<Self, CliError> {
        Ok(Self {
            http: pk_cli_http::client("lofty", env!("CARGO_PKG_VERSION"))?,
            base: base.to_string(),
            key: None,
        })
    }

    pub fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, CliError> {
        let mut req = self.http.get(self.url(path));
        if !query.is_empty() {
            req = req.query(query);
        }
        self.send(req, None)
    }

    /// POST with a JSON body and a fresh idempotency key.
    pub fn post(&self, path: &str, body: &Value) -> Result<Value, CliError> {
        let req = self.http.post(self.url(path)).json(body);
        self.send(req, Some(idempotency_key()))
    }

    /// DELETE with a fresh idempotency key.
    pub fn delete(&self, path: &str) -> Result<Value, CliError> {
        let req = self.http.delete(self.url(path));
        self.send(req, Some(idempotency_key()))
    }

    /// Raw passthrough for `lofty api`.
    pub fn request(
        &self,
        method: reqwest::Method,
        url: &str,
        body: Option<Value>,
    ) -> Result<Value, CliError> {
        let is_write = matches!(
            method,
            reqwest::Method::POST | reqwest::Method::PUT | reqwest::Method::DELETE
        );
        let mut req = self.http.request(method, url);
        if let Some(b) = body {
            req = req.json(&b);
        }
        self.send(req, is_write.then(idempotency_key))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base.trim_end_matches('/'), path)
    }

    fn send(
        &self,
        mut req: reqwest::blocking::RequestBuilder,
        idempotency: Option<String>,
    ) -> Result<Value, CliError> {
        if let Some(key) = &self.key {
            req = req.bearer_auth(key);
        }
        req = req.header("Accept", "application/json");
        if let Some(ik) = idempotency {
            req = req.header("Idempotency-Key", ik);
        }
        let resp = req
            .send()
            .map_err(|e| CliError::Upstream(format!("request failed: {e}")))?;
        Self::handle(resp)
    }

    /// Map the SDK API's error contract onto the family exit codes.
    fn handle(resp: reqwest::blocking::Response) -> Result<Value, CliError> {
        let status = resp.status();
        if status.is_success() {
            return resp
                .json::<Value>()
                .map_err(|e| CliError::Upstream(format!("parsing response JSON: {e}")));
        }
        let retry_after = resp
            .headers()
            .get("Retry-After")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        let body: Value = resp.json().unwrap_or_default();
        let code = body
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("no error message");
        match status.as_u16() {
            401 => Err(CliError::Auth(format!(
                "invalid or revoked API key ({code}): {message}"
            ))),
            403 => Err(CliError::Auth(format!(
                "forbidden ({code}): {message} — if this is a trading call, enable Trading on your API key"
            ))),
            404 => Err(CliError::NotFound(format!("{code}: {message}"))),
            429 => Err(CliError::Upstream(format!(
                "rate limited ({code}): {message}{}",
                retry_after
                    .map(|s| format!(" — retry after {s}s"))
                    .unwrap_or_default()
            ))),
            s => Err(CliError::Upstream(format!("HTTP {s} ({code}): {message}"))),
        }
    }
}

/// UUID v4 without extra dependencies (idempotency keys just need uniqueness).
fn idempotency_key() -> String {
    let mut b = [0u8; 16];
    // getrandom via std: fill from a few OS entropy sources we already have.
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let pid = std::process::id() as u128;
    let addr = &b as *const _ as u128;
    let mut seed = now.as_nanos() ^ (pid << 64) ^ addr;
    for chunk in b.chunks_mut(8) {
        // xorshift on the seed; good enough for request dedup keys, not crypto.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let bytes = (seed as u64).to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h =
        |r: std::ops::Range<usize>| -> String { b[r].iter().map(|x| format!("{x:02x}")).collect() };
    format!(
        "{}-{}-{}-{}-{}",
        h(0..4),
        h(4..6),
        h(6..8),
        h(8..10),
        h(10..16)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_keys_are_uuid_shaped_and_unique() {
        let a = idempotency_key();
        let b = idempotency_key();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(a.chars().filter(|c| *c == '-').count(), 4);
        assert_eq!(a.as_bytes()[14], b'4'); // version nibble
    }
}
