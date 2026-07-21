//! Non-secret settings (`~/.config/lofty/config.toml`).
//!
//! The API key lives in the OS keychain (`piekstra.lofty`, account `api-key`),
//! never here.

use serde::{Deserialize, Serialize};

/// Official SDK API base (the `@loftyaicode/sdk` wire contract). Auth: `Authorization: Bearer lofty_live_…`.
pub const DEFAULT_BASE_URL: &str = "https://api.lofty.ai";

/// The internal platform API used by the website (Cognito/SigV4; some reads are
/// open). Reachable via `lofty api --internal`.
pub const INTERNAL_BASE_URL: &str = "https://api.lofty.ai/prod";

/// Keychain account name the API key is stored under.
pub const KEYCHAIN_ACCOUNT: &str = "api-key";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Override the API base URL (default [`DEFAULT_BASE_URL`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// Account email (identity label only; secrets stay in the keychain).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

impl Config {
    pub fn base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
    }
}

/// Config keys settable via `lofty config set <key> <value>`.
pub const KNOWN_KEYS: &[&str] = &["base_url", "username"];
