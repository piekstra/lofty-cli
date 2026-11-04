//! HTTP client for the Lofty SDK API (`/public/v1/*`, Bearer API key).
//!
//! Wire contract (the `@loftyaicode/sdk` surface): success → payload JSON
//! as-is; failure → HTTP status
//! plus `{"error": {"code", "message"}}`. Writes require an `Idempotency-Key`
//! header (deduplicated upstream for 24h). Rate limits per key: 300 reads/min
//! and 30 writes/min (60/min per account).

use pk_cli_core::transport::{require_secure_url, secure_redirects};
use pk_cli_core::{CliError, Reason};
use pk_cli_secrets::CredentialStore;
use serde_json::Value;

use crate::config::{Config, KEYCHAIN_ACCOUNT};

/// A 403 from Lofty. Its remedy is kept apart, since a tool replaces it: its reads are not
/// trading calls, and a 403 here can mean a wrong key or a path that does not exist.
pub fn forbidden(code: &str, message: &str) -> CliError {
    CliError::Auth(format!("forbidden ({code}): {message}"))
        .because(Reason::Forbidden)
        .with_remedy("if this is a trading call, enable Trading on your API key")
}

/// A status Lofty answered with that has no meaning of its own here, past a 400's.
pub fn with_status(status: u16, code: &str, message: &str) -> CliError {
    let error = CliError::Upstream(format!("HTTP {status} ({code}): {message}"));
    if status == 400 {
        error.because(Reason::BadRequest)
    } else {
        error
    }
}

/// The request got no answer. A reqwest error prints the URL it was for, and this
/// text can reach a model.
fn no_answer(e: reqwest::Error) -> CliError {
    // Through `CliError::from`, so a refused redirect keeps its reason.
    match CliError::from(e) {
        CliError::Upstream(message) => CliError::Upstream(format!("request failed: {message}")),
        other => other,
    }
}

pub struct LoftyClient {
    http: reqwest::blocking::Client,
    base: String,
    key: Option<String>,
}

fn http_client() -> Result<reqwest::blocking::Client, CliError> {
    pk_cli_http::builder("lofty", env!("CARGO_PKG_VERSION"))
        .redirect(secure_redirects())
        .build()
        .map_err(|e| CliError::Other(format!("failed to build HTTP client: {e}")))
}

impl LoftyClient {
    /// Client for authenticated calls; errors with exit 3 if no key is stored.
    pub fn new(cfg: &Config, creds: &CredentialStore) -> Result<Self, CliError> {
        require_secure_url(&cfg.base_url())?;
        let key = creds
            .get(KEYCHAIN_ACCOUNT)?
            .ok_or_else(|| CliError::Auth("no API key stored — run `lofty auth login`".into()))?;
        Ok(Self {
            http: http_client()?,
            base: cfg.base_url(),
            key: Some(key.expose().to_string()),
        })
    }

    /// Client with an explicit key (used by `auth login --no-verify=false` to
    /// verify a candidate key before storing it).
    pub fn with_key(cfg: &Config, key: &str) -> Result<Self, CliError> {
        require_secure_url(&cfg.base_url())?;
        Ok(Self {
            http: http_client()?,
            base: cfg.base_url(),
            key: Some(key.to_string()),
        })
    }

    /// Unauthenticated client (internal `/prod` passthrough; some reads are open).
    pub fn anonymous(base: &str) -> Result<Self, CliError> {
        Ok(Self {
            http: http_client()?,
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
        let resp = req.send().map_err(no_answer)?;
        Self::handle(resp)
    }

    /// Map the SDK API's error contract onto the family exit codes.
    fn handle(resp: reqwest::blocking::Response) -> Result<Value, CliError> {
        let status = resp.status();
        if status.is_success() {
            return resp.json::<Value>().map_err(|e| {
                CliError::Upstream(format!("parsing response JSON: {}", e.without_url()))
            });
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
            403 => Err(forbidden(code, message)),
            404 => Err(CliError::NotFound(format!("{code}: {message}"))),
            429 => Err(CliError::Upstream(format!(
                "rate limited ({code}): {message}{}",
                retry_after
                    .map(|s| format!(" — retry after {s}s"))
                    .unwrap_or_default()
            ))),
            s => Err(with_status(s, code, message)),
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

    /// A stub on 127.0.0.1. Answers each request with `respond(request)` as a raw
    /// HTTP response, and keeps each request so a test can see what arrived.
    fn stub(
        respond: impl Fn(&str) -> String + Send + 'static,
    ) -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                let request = loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    let complete = text.split_once("\r\n\r\n").is_some_and(|(head, body)| {
                        let length = head
                            .lines()
                            .find_map(|l| {
                                let (name, value) = l.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        body.len() >= length
                    });
                    if n == 0 || complete {
                        break text;
                    }
                };
                let response = respond(&request);
                log.lock().unwrap().push(request);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (base, seen)
    }

    fn redirect_to(location: &str) -> String {
        format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    #[test]
    fn a_redirect_that_leaves_https_is_refused() {
        let (base, seen) = stub(|_| redirect_to("http://www.example.invalid/next"));
        let err = LoftyClient::anonymous(&base)
            .unwrap()
            .request(reqwest::Method::GET, &format!("{base}/start"), None)
            .unwrap_err();
        assert!(err.to_string().contains("leaves https"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_base_url_off_https_is_refused() {
        let cfg = Config {
            base_url: Some("http://api.example.invalid".into()),
            ..Config::default()
        };
        let err = LoftyClient::with_key(&cfg, "dummy-key")
            .err()
            .expect("refused");
        assert!(err.to_string().contains("only https is allowed"), "{err}");
    }

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
