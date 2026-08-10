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
            tracing::warn!(
                env = key,
                value = trimmed,
                "ignoring unparseable retry env var"
            );
            None
        }
    }
}

/// Parse a millisecond env var into a [`Duration`], with the same lenient
/// unset/invalid handling as [`parse_env_u64`].
fn parse_env_millis(key: &str) -> Option<Duration> {
    parse_env_u64(key).map(Duration::from_millis)
}

/// Why a request is being retried, classified into a short operator-facing
/// phrase.
///
/// Deliberately a fixed vocabulary rather than the underlying [`Error`]'s
/// `Display`: a provider error body is attacker-influenced and can be
/// arbitrarily long, and surfacing raw text to every client would put unbounded
/// upstream content on screen. The variants below carry everything a human needs
/// to know ("the network dropped" vs "we're being throttled") and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryReason {
    /// Connect/DNS/TLS/timeout — the endpoint was unreachable ([`Error::Http`]).
    Connection,
    /// HTTP 429.
    RateLimited,
    /// HTTP 5xx.
    ServerError,
    /// Anything else classified as retry-worthy by a provider.
    Transient,
}

impl RetryReason {
    /// Short lowercase phrase for status rows and transcripts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection failed",
            Self::RateLimited => "rate limited",
            Self::ServerError => "server error",
            Self::Transient => "transient error",
        }
    }

    /// Classify the error that caused a retry.
    pub fn classify(error: &Error) -> Self {
        match error {
            Error::Http(_) => Self::Connection,
            Error::ProviderError { status, .. } => match classify_status(*status) {
                Some(ClassifiedStatus::RateLimited) => Self::RateLimited,
                Some(ClassifiedStatus::ServerError) => Self::ServerError,
                None => Self::Transient,
            },
            Error::RetryExhausted { source, .. } => Self::classify(source),
            _ => Self::Transient,
        }
    }
}

/// One "about to retry" notification, emitted *before* the backoff sleep.
///
/// `attempt` is the attempt that just failed (1-based), so a client can render
/// "retrying 2/8" and know how much budget is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryNotice {
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
    pub reason: RetryReason,
}

/// Sink for [`RetryNotice`]s, installed by the caller that owns a user-facing
/// event stream (the agent loop) and threaded down via
/// [`crate::StreamOptions::retry_observer`].
///
/// Retries used to be `tracing::warn!`-only. On a degraded link that meant a
/// turn could spend its entire budget silently reconnecting while the client
/// showed nothing but "working" — indistinguishable from a hang, which reads as
/// "the agent is broken" rather than "the network is bad". This is the seam that
/// lets the truth reach a surface.
#[derive(Clone)]
pub struct RetryObserver(std::sync::Arc<dyn Fn(RetryNotice) + Send + Sync>);

impl RetryObserver {
    pub fn new(f: impl Fn(RetryNotice) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(f))
    }

    pub fn notify(&self, notice: RetryNotice) {
        (self.0)(notice)
    }
}

// `StreamOptions` derives `Debug`; a boxed closure can't, so print the presence
// of an observer rather than blocking the derive on the whole options struct.
impl std::fmt::Debug for RetryObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RetryObserver(..)")
    }
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
    f: F,
) -> Result<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Attempt<T>>,
{
    with_retry_observed(cfg, cancel, None, f).await
}

/// [`with_retry`], plus an optional [`RetryObserver`] notified before each
/// backoff sleep.
///
/// Split from `with_retry` rather than added as a parameter so call sites with
/// no user-facing stream to report to (e.g. `ocean-mcp`) stay untouched.
pub async fn with_retry_observed<T, F, Fut>(
    cfg: &RetryConfig,
    cancel: Option<&CancellationToken>,
    observer: Option<&RetryObserver>,
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
                let reason = RetryReason::classify(&error);
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
                // Before the sleep, not after: the whole point is to tell the
                // operator *while* we are waiting, not once the wait is over.
                if let Some(obs) = observer {
                    obs.notify(RetryNotice {
                        attempt,
                        max_attempts: cfg.max_attempts,
                        delay_ms: delay.as_millis() as u64,
                        reason,
                    });
                }
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
        assert!(
            std::ptr::eq(a, b),
            "accessor must return the cached instance"
        );
        assert_eq!(a.max_attempts, b.max_attempts);
    }

    // ── retry observability ─────────────────────────────────────────────────

    fn collector() -> (RetryObserver, std::sync::Arc<Mutex<Vec<RetryNotice>>>) {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let sink = std::sync::Arc::clone(&seen);
        let obs = RetryObserver::new(move |n| sink.lock().unwrap().push(n));
        (obs, seen)
    }

    fn http_error() -> Error {
        Error::Other("boom".into())
    }

    /// One notice per *wait*, not per attempt: a 3-attempt budget that fails
    /// throughout sleeps twice, so the operator sees "1/3" then "2/3" and never
    /// a "3/3" that promises a wait which never happens.
    #[tokio::test]
    async fn observer_sees_one_notice_per_backoff_not_per_attempt() {
        let cfg = RetryConfig {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        };
        let (obs, seen) = collector();

        let out: Result<()> = with_retry_observed(&cfg, None, Some(&obs), |_| async {
            Attempt::Retry {
                error: http_error(),
                retry_after: None,
            }
        })
        .await;

        assert!(matches!(
            out,
            Err(Error::RetryExhausted { attempts: 3, .. })
        ));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "3 attempts => 2 waits => 2 notices");
        assert_eq!(seen[0].attempt, 1);
        assert_eq!(seen[1].attempt, 2);
        assert!(
            seen.iter().all(|n| n.max_attempts == 3),
            "every notice carries the budget so a client can render x/y"
        );
    }

    /// A call that succeeds on the first try must stay completely silent —
    /// otherwise every healthy turn would flash a spurious reconnect notice.
    #[tokio::test]
    async fn observer_silent_when_first_attempt_succeeds() {
        let cfg = RetryConfig::default();
        let (obs, seen) = collector();

        let out: Result<u8> =
            with_retry_observed(&cfg, None, Some(&obs), |_| async { Attempt::Ok(7) }).await;

        assert_eq!(out.unwrap(), 7);
        assert!(seen.lock().unwrap().is_empty());
    }

    /// A `Retry-After` hint is what the operator actually waits, so it — not the
    /// computed backoff — must be the delay reported.
    #[tokio::test]
    async fn notice_reports_the_delay_actually_waited() {
        let cfg = RetryConfig {
            max_attempts: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(50),
        };
        let (obs, seen) = collector();

        let _: Result<()> = with_retry_observed(&cfg, None, Some(&obs), |_| async {
            Attempt::Retry {
                error: http_error(),
                retry_after: Some(Duration::from_millis(40)),
            }
        })
        .await;

        assert_eq!(seen.lock().unwrap()[0].delay_ms, 40);
    }

    /// The reported reason is a fixed phrase derived from the error class, never
    /// the provider's own text — an upstream body is attacker-influenced and
    /// must not reach a client through this path.
    #[test]
    fn reason_classification_is_a_fixed_vocabulary() {
        assert_eq!(
            RetryReason::classify(&Error::ProviderError {
                status: 429,
                body: "slow down".into()
            }),
            RetryReason::RateLimited
        );
        assert_eq!(
            RetryReason::classify(&Error::ProviderError {
                status: 503,
                body: "oops".into()
            }),
            RetryReason::ServerError
        );
        // An exhausted retry is classified by what actually failed underneath.
        assert_eq!(
            RetryReason::classify(&Error::RetryExhausted {
                attempts: 3,
                source: Box::new(Error::ProviderError {
                    status: 429,
                    body: String::new()
                }),
            }),
            RetryReason::RateLimited
        );

        let secret = "sk-live-do-not-leak";
        let reason = RetryReason::classify(&Error::ProviderError {
            status: 500,
            body: format!("bad key {secret}"),
        });
        assert!(
            !reason.as_str().contains(secret),
            "classified reason must never carry provider body text"
        );
    }

    /// `with_retry` keeps its historical signature and stays log-only, so call
    /// sites with no surface to report to are unaffected.
    #[tokio::test]
    async fn plain_with_retry_still_works_without_an_observer() {
        let cfg = RetryConfig {
            max_attempts: 2,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(1),
        };
        let mut calls = 0;
        let out: Result<u8> = with_retry(&cfg, None, |_| {
            calls += 1;
            async move { Attempt::Ok(1) }
        })
        .await;
        assert_eq!(out.unwrap(), 1);
        assert_eq!(calls, 1);
    }
}
