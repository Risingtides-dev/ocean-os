//! Configuration for connecting Ocean to a Longhouse coordinator.
//!
//! This module is intentionally side-effect-light: it reads environment values
//! and describes the requested mode, but it does not start services or perform
//! network calls. The daemon can use this as the single source of truth when it
//! grows dynamic Longhouse preparation.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How Ocean should use Longhouse for this process/workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LonghouseMode {
    /// Do not consult Longhouse.
    Disabled,
    /// Use daemon-embedded Longhouse routes/logic.
    Embedded,
    /// Connect to a separate local service, usually `127.0.0.1:4781`.
    Local,
    /// Connect to a remote team Longhouse service.
    Remote,
}

impl LonghouseMode {
    pub fn parse(value: impl AsRef<str>) -> Option<Self> {
        match value.as_ref().trim().to_ascii_lowercase().as_str() {
            "disabled" | "off" | "false" | "0" => Some(Self::Disabled),
            "embedded" | "embed" | "in-process" | "in_process" => Some(Self::Embedded),
            "local" => Some(Self::Local),
            "remote" => Some(Self::Remote),
            _ => None,
        }
    }
}

impl Default for LonghouseMode {
    fn default() -> Self {
        Self::Embedded
    }
}

/// Daemon/service configuration for Longhouse integration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LonghouseConfig {
    pub mode: LonghouseMode,
    pub url: Option<String>,
    /// Environment variable that contains the bearer token. Defaults to
    /// `OCEAN_TOKEN`; the token value is intentionally not stored here.
    pub token_env: String,
    pub timeout_ms: u64,
}

impl Default for LonghouseConfig {
    fn default() -> Self {
        Self {
            mode: LonghouseMode::default(),
            url: None,
            token_env: "OCEAN_TOKEN".to_string(),
            timeout_ms: 5_000,
        }
    }
}

impl LonghouseConfig {
    pub const DEFAULT_LOCAL_URL: &'static str = "http://127.0.0.1:4781";

    /// Build config from process environment.
    ///
    /// Supported variables:
    /// - `OCEAN_LONGHOUSE_MODE` = disabled | embedded | local | remote
    /// - `OCEAN_LONGHOUSE_URL` = local/remote service URL
    /// - `OCEAN_LONGHOUSE_TOKEN_ENV` = env var name for bearer token
    /// - `OCEAN_LONGHOUSE_TIMEOUT_MS` = request timeout
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(mode) = std::env::var("OCEAN_LONGHOUSE_MODE") {
            if let Some(parsed) = LonghouseMode::parse(mode) {
                cfg.mode = parsed;
            }
        }

        if let Ok(url) = std::env::var("OCEAN_LONGHOUSE_URL") {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                cfg.url = Some(trimmed.to_string());
            }
        }

        if let Ok(token_env) = std::env::var("OCEAN_LONGHOUSE_TOKEN_ENV") {
            let trimmed = token_env.trim();
            if !trimmed.is_empty() {
                cfg.token_env = trimmed.to_string();
            }
        }

        if let Ok(timeout_ms) = std::env::var("OCEAN_LONGHOUSE_TIMEOUT_MS") {
            if let Ok(parsed) = timeout_ms.trim().parse::<u64>() {
                cfg.timeout_ms = parsed;
            }
        }

        if cfg.url.is_none() && matches!(cfg.mode, LonghouseMode::Local) {
            cfg.url = Some(Self::DEFAULT_LOCAL_URL.to_string());
        }

        cfg
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    pub fn effective_url(&self) -> Option<&str> {
        self.url.as_deref()
    }

    pub fn bearer_token(&self) -> Option<String> {
        std::env::var(&self.token_env)
            .ok()
            .filter(|s| !s.is_empty())
    }

    pub fn enabled(&self) -> bool {
        !matches!(self.mode, LonghouseMode::Disabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modes() {
        assert_eq!(
            LonghouseMode::parse("disabled"),
            Some(LonghouseMode::Disabled)
        );
        assert_eq!(
            LonghouseMode::parse("embedded"),
            Some(LonghouseMode::Embedded)
        );
        assert_eq!(LonghouseMode::parse("local"), Some(LonghouseMode::Local));
        assert_eq!(LonghouseMode::parse("remote"), Some(LonghouseMode::Remote));
        assert_eq!(LonghouseMode::parse("wat"), None);
    }
}
