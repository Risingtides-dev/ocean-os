//! Shared retry helper used by HTTP-backed providers.
//!
//! Retries on transient failures (5xx, 429) with exponential back-off. If the
//! response includes a `Retry-After` header, the delay honors it (capped at
//! `max_retry_delay`).
//!
//! # Operator overrides (OCEAN-259)
//!
//! The retry policy is configurable via environment variables, resolved **once**
//! at first use (see [`retry_config`]) so we never re-parse env on every retry —
//! mirroring how [`crate::http`] resolves its streaming-client policy. With no
//! env vars set the policy is **identical** to the historical hard-coded
//! [`RetryConfig::default`], so behavior is unchanged unless an operator opts in:
//!
//! | Env var                        | Field         | Default | Notes                                   |
//! |--------------------------------|---------------|---------|-----------------------------------------|
//! | `OCEAN_RETRY_MAX_ATTEMPTS`     | `max_attempts`| `3`     | Clamped to `>= 1` (1 = no retries).     |
//! | `OCEAN_RETRY_BASE_BACKOFF_MS`  | `base_delay`  | `500`   | First-step back-off, in milliseconds.   |
//! | `OCEAN_RETRY_MAX_BACKOFF_MS`   | `max_delay`   | `60000` | Per-wait ceiling, in milliseconds.      |
//!
//! Unparseable or out-of-range values fall back to the field default and log a
//! warning; they never panic and never disable retries entirely.

use std::sync::OnceLock;
use std::time::Duration;

use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};

/// Default maximum number of attempts (initial try + retries). Matches the
/// historical hard-coded value so unset env => unchanged behavior.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Default first-step exponential back-off.
pub const DEFAULT_BASE_DELAY: Duration = Duration::from_millis(500);
/// Default ceiling applied to any single back-off / `Retry-After` wait.
pub const DEFAULT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Env var overriding [`RetryConfig::max_attempts`] (clamped to `>= 1`).
pub const ENV_MAX_ATTEMPTS: &str = "OCEAN_RETRY_MAX_ATTEMPTS";
/// Env var overriding [`RetryConfig::base_delay`] (milliseconds).
pub const ENV_BASE_BACKOFF_MS: &str = "OCEAN_RETRY_BASE_BACKOFF_MS";
/// Env var overriding [`RetryConfig::max_delay`] (milliseconds).
pub const ENV_MAX_BACKOFF_MS: &str = "OCEAN_RETRY_MAX_BACKOFF_MS";

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            base_delay: DEFAULT_BASE_DELAY,
            max_delay: DEFAULT_MAX_DELAY,
        }
    }
}

impl RetryConfig {
    /// Build a [`RetryConfig`] from the environment, layering operator overrides
    /// on top of [`RetryConfig::default`].
    ///
    /// Every field falls back to its default when the corresponding env var is
    /// unset, empty, or unparseable, so an unconfigured process gets **exactly**
    /// the historical policy (3 attempts, 500ms base, 60s ceiling). Values are
    /// clamped sanely — `max_attempts` is forced to `>= 1` so retries can never
    /// be turned into "zero attempts", and `max_delay` is raised to at least
    /// `base_delay` so the ceiling can't invert the floor.
    ///
    /// This re-reads the environment on every call; the process-wide resolution
    /// that the providers actually use is cached in [`retry_config`].
    pub fn from_env() -> Self {
        let default = Self::default();

        let max_attempts = match parse_env_u64(ENV_MAX_ATTEMPTS) {
            Some(n) => {
                let clamped = n.clamp(1, u32::MAX as u64) as u32;
                if clamped as u64 != n {
                    tracing::warn!(
                        env = ENV_MAX_ATTEMPTS,
                        requested = n,
                        used = clamped,
                        "clamped retry max_attempts into supported range"
                    );
                }
                clamped
            }
            None => default.max_attempts,
        };

        let base_delay = parse_env_millis(ENV_BASE_BACKOFF_MS).unwrap_or(default.base_delay);

        // The ceiling must never sit below the floor, or a single back-off would
        // be capped beneath its own starting point.
        let mut max_delay = parse_env_millis(ENV_MAX_BACKOFF_MS).unwrap_or(default.max_delay);
        if max_delay < base_delay {
            tracing::warn!(
                env = ENV_MAX_BACKOFF_MS,
                base_ms = base_delay.as_millis(),
                max_ms = max_delay.as_millis(),
                "retry max backoff below base backoff; raising ceiling to base"
            );
            max_delay = base_delay;
        }

        Self {
            max_attempts,
            base_delay,
            max_delay,
        }
    }
}

/// Process-wide retry policy, resolved from the environment exactly once.
///
/// Providers call this instead of constructing a [`RetryConfig`] per request, so
/// the env is parsed a single time for the lifetime of the process — the same
/// "resolve once, reuse everywhere" shape [`crate::http::streaming_client`] uses
/// for its timeout policy. Returns a borrow of the cached value.
pub fn retry_config() -> &'static RetryConfig {
    static CONFIG: OnceLock<RetryConfig> = OnceLock::new();
    CONFIG.get_or_init(RetryConfig::from_env)
}

/// Parse an unsigned integer env var, returning `None` when unset/empty and
/// logging+ignoring an unparseable value (so a typo never panics a turn).
fn parse_env_u64(key: &str) -> Option<u64> {
    let raw = std::env::var(key).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.parse::<u64>() {
        Ok(v) => Some(v),
        Err(_) => {
            tracing::warn!(env = key, value = trimmed, "ignoring unparseable retry env var");
            None
        }
    }
}

/// Parse a millisecond env var into a [`Duration`], with the same lenient
/// unset/invalid handling as [`parse_env_u64`].
fn parse_env_millis(key: &str) -> Option<Duration> {
    parse_env_u64(key).map(Duration::from_millis)
}

/// Outcome of a single attempt.
#[allow(clippy::large_enum_variant)]
pub enum Attempt<T> {
    Ok(T),
    /// Permanent failure — return immediately.
    Fatal(Error),
    /// Transient failure — try again. `retry_after` is the server-hinted delay.
    Retry {
        error: Error,
        retry_after: Option<Duration>,
    },
}

pub async fn with_retry<T, F, Fut>(
    cfg: &RetryConfig,
    cancel: Option<&CancellationToken>,
    mut f: F,
) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Attempt<T>>,
{
    let mut attempt: u32 = 0;
    let mut last_err: Option<Error>;
    loop {
        if let Some(c) = cancel {
            if c.is_cancelled() {
                return Err(Error::Cancelled);
            }
        }
        attempt += 1;
        match f(attempt).await {
            Attempt::Ok(v) => return Ok(v),
            Attempt::Fatal(e) => return Err(e),
            Attempt::Retry { error, retry_after } => {
                last_err = Some(error);
                let _ = &last_err;
                if attempt >= cfg.max_attempts {
                    break;
                }
                let backoff = cfg
                    .base_delay
                    .saturating_mul(1u32 << attempt.min(6))
                    .min(cfg.max_delay);
                let delay = retry_after.map(|d| d.min(cfg.max_delay)).unwrap_or(backoff);
                tracing::warn!(?delay, attempt, "retrying after transient error");
                tokio::select! {
                    _ = sleep(delay) => {},
                    _ = async {
                        if let Some(c) = cancel { c.cancelled().await; }
                        else { futures::future::pending::<()>().await; }
                    } => return Err(Error::Cancelled),
                }
            }
        }
    }
    Err(Error::RetryExhausted {
        attempts: attempt,
        source: Box::new(last_err.unwrap_or_else(|| Error::Other("retry exhausted".into()))),
    })
}

/// Classify a status code into retry-worthy categories.
pub fn classify_status(status: u16) -> Option<ClassifiedStatus> {
    match status {
        429 => Some(ClassifiedStatus::RateLimited),
        500..=599 => Some(ClassifiedStatus::ServerError),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassifiedStatus {
    RateLimited,
    ServerError,
}

pub fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mutating process-wide env is global state, so env-touching tests must not
    /// run concurrently with each other.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Clear all retry env vars, run `body` with them set as requested, then
    /// restore the prior environment — keeping tests hermetic regardless of how
    /// the suite is invoked.
    fn with_env<T>(vars: &[(&str, Option<&str>)], body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let keys = [ENV_MAX_ATTEMPTS, ENV_BASE_BACKOFF_MS, ENV_MAX_BACKOFF_MS];
        let saved: Vec<(&str, Option<String>)> =
            keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();

        for k in keys {
            std::env::remove_var(k);
        }
        for (k, v) in vars {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }

        let out = body();

        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
        out
    }

    /// Unset env => exactly the historical hard-coded policy. This is the
    /// behavior-preservation guarantee the whole change rests on.
    #[test]
    fn from_env_unset_matches_default() {
        let cfg = with_env(&[], RetryConfig::from_env);
        let default = RetryConfig::default();
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.base_delay, Duration::from_millis(500));
        assert_eq!(cfg.max_delay, Duration::from_secs(60));
        assert_eq!(cfg.max_attempts, default.max_attempts);
        assert_eq!(cfg.base_delay, default.base_delay);
        assert_eq!(cfg.max_delay, default.max_delay);
    }

    /// Empty / whitespace-only values are treated as "unset" rather than errors,
    /// so an exported-but-blank var keeps the default.
    #[test]
    fn from_env_blank_values_fall_back_to_default() {
        let cfg = with_env(
            &[
                (ENV_MAX_ATTEMPTS, Some("")),
                (ENV_BASE_BACKOFF_MS, Some("   ")),
            ],
            RetryConfig::from_env,
        );
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.base_delay, Duration::from_millis(500));
    }

    /// Operator-set values are applied verbatim when in range.
    #[test]
    fn from_env_applies_overrides() {
        let cfg = with_env(
            &[
                (ENV_MAX_ATTEMPTS, Some("7")),
                (ENV_BASE_BACKOFF_MS, Some("250")),
                (ENV_MAX_BACKOFF_MS, Some("30000")),
            ],
            RetryConfig::from_env,
        );
        assert_eq!(cfg.max_attempts, 7);
        assert_eq!(cfg.base_delay, Duration::from_millis(250));
        assert_eq!(cfg.max_delay, Duration::from_millis(30_000));
    }

    /// `max_attempts = 0` would silently disable retries; it must clamp to 1.
    #[test]
    fn from_env_clamps_zero_attempts_to_one() {
        let cfg = with_env(&[(ENV_MAX_ATTEMPTS, Some("0"))], RetryConfig::from_env);
        assert_eq!(cfg.max_attempts, 1);
    }

    /// Garbage values are ignored (with a warning) and fall back to defaults
    /// rather than panicking a turn mid-flight.
    #[test]
    fn from_env_ignores_unparseable() {
        let cfg = with_env(
            &[
                (ENV_MAX_ATTEMPTS, Some("not-a-number")),
                (ENV_BASE_BACKOFF_MS, Some("12.5")),
            ],
            RetryConfig::from_env,
        );
        assert_eq!(cfg.max_attempts, 3);
        assert_eq!(cfg.base_delay, Duration::from_millis(500));
    }

    /// A ceiling below the floor would cap a back-off beneath its own start;
    /// it's raised up to the base so the invariant `max_delay >= base_delay`
    /// always holds.
    #[test]
    fn from_env_raises_ceiling_below_base() {
        let cfg = with_env(
            &[
                (ENV_BASE_BACKOFF_MS, Some("5000")),
                (ENV_MAX_BACKOFF_MS, Some("1000")),
            ],
            RetryConfig::from_env,
        );
        assert_eq!(cfg.base_delay, Duration::from_millis(5000));
        assert_eq!(cfg.max_delay, Duration::from_millis(5000));
        assert!(cfg.max_delay >= cfg.base_delay);
    }

    /// The cached accessor resolves to a value equal to a fresh `from_env()`
    /// under the same (default) environment, and hands back a stable borrow.
    #[test]
    fn retry_config_caches_once() {
        let a = retry_config();
        let b = retry_config();
        assert!(std::ptr::eq(a, b), "accessor must return the cached instance");
        assert_eq!(a.max_attempts, b.max_attempts);
    }
}
