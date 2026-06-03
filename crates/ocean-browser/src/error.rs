//! Browser errors split into retryable (transient CDP/page faults the agent can
//! retry) and fatal (Chrome missing / failed to launch) so callers map them
//! onto tool errors vs hard failures.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("chrome could not be launched: {0}")]
    Launch(String),
    #[error("no active page/tab")]
    NoPage,
    #[error("navigation failed: {0}")]
    Navigation(String),
    #[error("cdp call failed: {0}")]
    Cdp(String),
    #[error("element not found: {0}")]
    ElementNotFound(String),
    #[error("timeout: {0}")]
    Timeout(String),
}

impl BrowserError {
    /// Whether the agent should be told it may retry. Launch failures are fatal;
    /// everything else is a transient page-state issue.
    pub fn retryable(&self) -> bool {
        !matches!(self, BrowserError::Launch(_))
    }
}
