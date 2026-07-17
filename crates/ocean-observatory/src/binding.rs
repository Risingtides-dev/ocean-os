use rand::RngCore;
use serde_json::Value;
use std::{
    collections::HashMap,
    fmt,
    time::{Duration, Instant},
};
use thiserror::Error;

pub const BINDING_TTL: Duration = Duration::from_secs(30);
pub const OBSERVATION_BINDING_FIELD: &str = "_observation_binding";

/// An opaque, single-use credential kept only in daemon memory.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct BindingToken(String);

impl BindingToken {
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let mut encoded = String::with_capacity(64);
        for byte in bytes {
            use std::fmt::Write as _;
            write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
        }
        Self(encoded)
    }

    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for BindingToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BindingToken([REDACTED])")
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BindingError {
    #[error("binding token is invalid or was already consumed")]
    InvalidOrConsumed,
    #[error("binding token has expired")]
    Expired,
    #[error("binding token does not match the execution")]
    ExecutionMismatch,
}

struct BindingEntry {
    execution_id: String,
    expires_at: Instant,
}

/// In-memory authority for short-lived, single-use observation bindings.
#[derive(Default)]
pub struct BindingRegistry {
    entries: HashMap<BindingToken, BindingEntry>,
}

impl BindingRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn issue(&mut self, execution_id: impl Into<String>) -> BindingToken {
        self.issue_at(execution_id, Instant::now())
    }

    fn issue_at(&mut self, execution_id: impl Into<String>, now: Instant) -> BindingToken {
        self.evict_expired_at(now);
        let token = BindingToken::generate();
        self.entries.insert(
            token.clone(),
            BindingEntry {
                execution_id: execution_id.into(),
                expires_at: now + BINDING_TTL,
            },
        );
        token
    }

    /// Consumes a token exactly once. Failed execution matching does not consume it.
    pub fn consume_binding(
        &mut self,
        token: &BindingToken,
        execution_id: &str,
    ) -> Result<(), BindingError> {
        self.consume_binding_at(token, execution_id, Instant::now())
    }

    fn consume_binding_at(
        &mut self,
        token: &BindingToken,
        execution_id: &str,
        now: Instant,
    ) -> Result<(), BindingError> {
        let Some(entry) = self.entries.get(token) else {
            return Err(BindingError::InvalidOrConsumed);
        };
        if entry.expires_at <= now {
            self.entries.remove(token);
            return Err(BindingError::Expired);
        }
        if entry.execution_id != execution_id {
            return Err(BindingError::ExecutionMismatch);
        }
        self.entries.remove(token);
        Ok(())
    }

    pub fn revoke(&mut self, token: &BindingToken) -> bool {
        self.entries.remove(token).is_some()
    }

    pub fn evict_expired(&mut self) {
        self.evict_expired_at(Instant::now());
    }

    fn evict_expired_at(&mut self, now: Instant) {
        self.entries.retain(|_, entry| entry.expires_at > now);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Removes the additive binding envelope before any provider serialization.
pub fn strip_binding(prompt: &mut Value) -> Option<Value> {
    prompt
        .as_object_mut()
        .and_then(|object| object.remove(OBSERVATION_BINDING_FIELD))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_256_bit_hex_and_debug_is_redacted() {
        let token = BindingToken::generate();
        assert_eq!(token.expose_secret().len(), 64);
        assert!(token
            .expose_secret()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(format!("{token:?}"), "BindingToken([REDACTED])");
    }

    #[test]
    fn consumption_is_single_use_and_execution_bound() {
        let mut registry = BindingRegistry::new();
        let token = registry.issue("execution-a");
        assert_eq!(
            registry.consume_binding(&token, "execution-b"),
            Err(BindingError::ExecutionMismatch)
        );
        assert_eq!(registry.consume_binding(&token, "execution-a"), Ok(()));
        assert_eq!(
            registry.consume_binding(&token, "execution-a"),
            Err(BindingError::InvalidOrConsumed)
        );
    }

    #[test]
    fn expired_token_is_rejected_and_evicted() {
        let now = Instant::now();
        let mut registry = BindingRegistry::new();
        let token = registry.issue_at("execution-a", now);
        assert_eq!(
            registry.consume_binding_at(&token, "execution-a", now + BINDING_TTL),
            Err(BindingError::Expired)
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn binding_is_removed_before_prompt_serialization() {
        let mut prompt = serde_json::json!({
            "messages": [{"role": "user", "content": "hello"}],
            "_observation_binding": {"execution_id": "e", "binding_token": "secret"}
        });
        let removed = strip_binding(&mut prompt).expect("binding exists");
        assert_eq!(removed["binding_token"], "secret");
        let serialized = serde_json::to_string(&prompt).expect("serialize prompt");
        assert!(!serialized.contains("_observation_binding"));
        assert!(!serialized.contains("secret"));
    }
}
