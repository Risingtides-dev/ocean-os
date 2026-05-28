//! Shared retry helper used by HTTP-backed providers.
//!
//! Retries on transient failures (5xx, 429) with exponential back-off. If the
//! response includes a `Retry-After` header, the delay honors it (capped at
//! `max_retry_delay`).

use std::time::Duration;

use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(60),
        }
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
