//! Shared HTTP client construction for LLM providers.
//!
//! # Why this exists (OCEAN-221)
//!
//! Every provider in this crate uses its `reqwest::Client` for exactly one
//! thing: opening a **long-lived streaming** (SSE) connection to a model
//! endpoint. Historically the clients were built with `reqwest::Client::new()`
//! (or a builder that set only pool options) and **no timeout at all**.
//!
//! That is a latent hang. The retry helper already branches on
//! `e.is_timeout() || e.is_connect()` to retry transient network failures —
//! but with no timeout configured, the timeout branch can never fire. A server
//! that accepts the TCP connection and then stalls (no response headers, or
//! headers then silence) wedges the agent turn **forever**, which in turn
//! wedges the daemon session that owns the turn.
//!
//! # The streaming trap
//!
//! The naive fix — `reqwest::ClientBuilder::timeout(..)` — is **wrong here**.
//! `timeout()` is a deadline on the *entire* request, including reading the
//! response body. For a streaming response the body is the token stream, so a
//! blanket `timeout()` would kill any completion that legitimately runs longer
//! than the deadline (long generations, slow tool-using turns, big outputs).
//! That would be worse than the bug we are fixing.
//!
//! # The right policy for streaming clients
//!
//! - **`connect_timeout`** — a hard cap on establishing the connection
//!   (DNS + TCP + TLS). This is cheap, always safe, and catches the
//!   "accepts then stalls at connect" case. A breach surfaces as
//!   `reqwest::Error::is_connect()` so the retry helper retries it.
//! - **`read_timeout`** — an **idle** timeout between successive reads, *not*
//!   a deadline on the whole request. It only fires when no bytes arrive for
//!   the configured window. A healthy SSE stream emits deltas continuously, so
//!   this never trips on a legitimate long stream; but a provider that goes
//!   silent mid-stream (the real "accepts then stalls" hang) trips it. A breach
//!   surfaces as `reqwest::Error::is_timeout()` so the retry helper retries it.
//!   (Added to reqwest in 0.12.5.)
//!
//! We deliberately do **not** set a total `timeout()`, precisely so long
//! streaming completions are never cut off.

use std::time::Duration;

/// Hard cap on connection establishment (DNS + TCP + TLS handshake).
///
/// Catches the "server accepts the socket then stalls before responding" case
/// without affecting in-flight streams. Surfaces as `is_connect()` on breach.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle (between-reads) timeout for streaming responses.
///
/// This is **not** a deadline on the whole request — it only fires when the
/// stream goes silent for this long. Sized generously so slow-but-alive token
/// streams (and brief provider-side pauses) are never killed, while a truly
/// hung connection is detected and retried. Surfaces as `is_timeout()`.
pub const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Idle connections kept warm per host. Mirrors the pre-OCEAN-221 Anthropic
/// client; applied uniformly so every provider shares one connection policy.
pub const DEFAULT_POOL_MAX_IDLE_PER_HOST: usize = 4;

/// Build a `reqwest::Client` configured for **streaming** LLM endpoints.
///
/// Applies [`DEFAULT_CONNECT_TIMEOUT`] + [`DEFAULT_READ_TIMEOUT`] (idle) and a
/// shared connection pool policy. Crucially it sets **no** total request
/// timeout, so long streaming completions are never truncated.
pub fn streaming_client() -> reqwest::Client {
    streaming_client_builder()
        .build()
        .expect("build streaming reqwest client")
}

/// The pre-configured builder behind [`streaming_client`], exposed so a caller
/// can layer on additional, provider-specific options before building.
pub fn streaming_client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
        .read_timeout(DEFAULT_READ_TIMEOUT)
        .pool_max_idle_per_host(DEFAULT_POOL_MAX_IDLE_PER_HOST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// The streaming client must build without panicking with the configured
    /// timeout policy applied. (reqwest exposes no getters for these settings,
    /// so we assert construction succeeds — a regression to an unsupported
    /// builder call would fail to compile or panic here.)
    #[test]
    fn streaming_client_builds() {
        let _c = streaming_client();
    }

    /// `connect_timeout` must actually bound a connection attempt rather than
    /// hang. We point the client at a non-routable TEST-NET address
    /// (RFC 5737, 192.0.2.0/24) with a short connect-timeout override; the
    /// connect can never complete, so the request must fail promptly (well
    /// under the 10s default) and classify as a connect/timeout failure —
    /// exactly the class the retry helper retries on. This proves the
    /// `connect_timeout` mechanism is in force.
    #[tokio::test]
    async fn connect_timeout_trips_on_non_routable_address() {
        let client = streaming_client_builder()
            .connect_timeout(Duration::from_millis(200))
            .build()
            .expect("build");

        let start = Instant::now();
        // 192.0.2.1 is reserved for documentation/tests and is not routable,
        // so the TCP connect will stall until our short timeout fires.
        let result = client.get("http://192.0.2.1:81/").send().await;
        let elapsed = start.elapsed();

        // Must fail (never succeed against a black-hole address).
        let err = result.expect_err("connect to non-routable addr must fail");

        // Must fail FAST — comfortably under the 10s default — proving the
        // override (and thus the connect_timeout mechanism) actually applied.
        assert!(
            elapsed < Duration::from_secs(5),
            "connect should have timed out fast, took {elapsed:?}"
        );

        // And it must be the retry-worthy connect/timeout class.
        assert!(
            err.is_timeout() || err.is_connect(),
            "expected connect/timeout error, got: {err:?}"
        );
    }
}
