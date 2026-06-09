use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("missing API key for provider {0}")]
    MissingApiKey(String),

    #[error("unsupported provider/api: {0}")]
    UnsupportedProvider(String),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("invalid response from provider: {0}")]
    InvalidResponse(String),

    #[error("provider returned an error (status {status}): {body}")]
    ProviderError { status: u16, body: String },

    #[error("request cancelled")]
    Cancelled,

    #[error("retried {attempts} times, last error: {source}")]
    RetryExhausted { attempts: u32, source: Box<Error> },

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Whether this error is an **availability / transient** failure — the only
    /// class for which routing to a fallback provider is appropriate (OCEAN-275).
    ///
    /// Returns `true` for:
    /// - missing credentials ([`Error::MissingApiKey`]) — the provider can't
    ///   authenticate, so an alternate that *can* should serve the turn;
    /// - transport failures ([`Error::Http`]) — connect/timeout/DNS, i.e. the
    ///   primary endpoint is unreachable;
    /// - HTTP `429` (rate-limited) and `5xx` (server error) from
    ///   [`Error::ProviderError`] — the upstream is degraded;
    /// - [`Error::RetryExhausted`], which wraps the *last* underlying error after
    ///   the retry budget ran out — classified by recursing into its source so an
    ///   exhausted transient failure is still treated as availability.
    ///
    /// Returns `false` for everything that is a **caller/content** problem and so
    /// would fail identically on any provider — `4xx` other than `429` (bad
    /// request, auth rejected, content filter, context-length), invalid/empty
    /// responses, JSON/IO errors, an unsupported provider, and cancellation.
    /// Failing those over would only duplicate the failure (and any cost) against
    /// a second provider for no benefit.
    pub fn is_retryable_availability(&self) -> bool {
        match self {
            // No usable credential for the primary — an alternate may have one.
            Self::MissingApiKey(_) => true,
            // Transport-level failure: connect refused, DNS, TLS, timeout, etc.
            Self::Http(_) => true,
            // Upstream said it's overloaded (429) or broke (5xx). A 4xx other than
            // 429 is the caller's request being rejected — not an availability
            // problem — and must NOT fail over.
            Self::ProviderError { status, .. } => {
                *status == 429 || (500..=599).contains(status)
            }
            // The retry helper already exhausted its budget; classify by the
            // wrapped cause so an exhausted transient stays "availability".
            Self::RetryExhausted { source, .. } => source.is_retryable_availability(),
            // Caller/content problems or control-flow — identical on any provider.
            Self::UnsupportedProvider(_)
            | Self::InvalidResponse(_)
            | Self::Cancelled
            | Self::Json(_)
            | Self::Io(_)
            | Self::Other(_) => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_errors_are_failover_worthy() {
        assert!(Error::MissingApiKey("anthropic".into()).is_retryable_availability());
        assert!(Error::ProviderError {
            status: 429,
            body: "rate limited".into()
        }
        .is_retryable_availability());
        assert!(Error::ProviderError {
            status: 503,
            body: "overloaded".into()
        }
        .is_retryable_availability());
    }

    #[test]
    fn user_errors_do_not_fail_over() {
        // 400 bad request — same failure on every provider.
        assert!(!Error::ProviderError {
            status: 400,
            body: "bad request".into()
        }
        .is_retryable_availability());
        // 401 auth rejected — the key is wrong, not absent; a rejected key is a
        // caller problem we surface, not silently route around.
        assert!(!Error::ProviderError {
            status: 401,
            body: "unauthorized".into()
        }
        .is_retryable_availability());
        // Content filter / unprocessable — deterministic across providers.
        assert!(!Error::ProviderError {
            status: 422,
            body: "content filtered".into()
        }
        .is_retryable_availability());
        assert!(!Error::Cancelled.is_retryable_availability());
        assert!(!Error::InvalidResponse("garbage".into()).is_retryable_availability());
    }

    #[test]
    fn retry_exhausted_classifies_by_wrapped_cause() {
        // Exhausted after repeated 503s — still availability.
        let exhausted_transient = Error::RetryExhausted {
            attempts: 3,
            source: Box::new(Error::ProviderError {
                status: 503,
                body: "overloaded".into(),
            }),
        };
        assert!(exhausted_transient.is_retryable_availability());

        // Exhausted wrapping a non-availability cause stays non-availability.
        let exhausted_user = Error::RetryExhausted {
            attempts: 1,
            source: Box::new(Error::InvalidResponse("garbage".into())),
        };
        assert!(!exhausted_user.is_retryable_availability());
    }
}
