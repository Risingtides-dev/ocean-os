//! In-loop round retry tests.
//!
//! The provider layer retries the *initial request*; the agent loop adds a
//! bounded retry for streams that fail transiently mid-flight — but ONLY on a
//! clean round (nothing emitted to the event sink yet), so a retry is invisible
//! rather than a duplicated partial. These tests script a provider that fails
//! with a transport-shaped error on the first call(s) and assert:
//!
//! 1. a clean-round transient failure is retried and the turn succeeds,
//! 2. a round that already streamed text is NOT retried (fails as before),
//! 3. a non-transient error (provider `Error` frame) is NOT retried,
//! 4. the retry budget is bounded — persistent failure still fails the turn.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream;
use ocean_protocol::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context,
    Error as ProtocolError, Message, Model, Provider, StopReason, StreamOptions, Usage,
};
use ocean_runtime::types::{AgentConfig, AgentEvent};
use tokio::sync::mpsc;

fn assistant_msg(content: Vec<Content>, stop: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "mock".into(),
        provider: "mock".into(),
        model: "mock".into(),
        usage: Usage::default(),
        stop_reason: stop,
        error_message: None,
        timestamp: 0,
    }
}

fn done(content: Vec<Content>, stop: StopReason) -> AssistantMessageEvent {
    AssistantMessageEvent::Done {
        reason: stop,
        message: assistant_msg(content, stop),
    }
}

fn text_delta(s: &str) -> AssistantMessageEvent {
    AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: s.into(),
    }
}

/// A transport-shaped transient error, as the protocol layer surfaces a
/// mid-stream drop (5xx here; `Http` variants require a live reqwest error).
fn transient_err() -> ProtocolError {
    ProtocolError::ProviderError {
        status: 529,
        body: "overloaded".into(),
    }
}

fn user(s: &str) -> Message {
    Message::user_text(s)
}

/// A provider that fails the first `fail_first` stream calls with a scripted
/// error sequence, then serves a clean final answer.
struct FlakyProvider {
    calls: AtomicUsize,
    fail_first: usize,
    /// Events yielded on a failing call BEFORE the error (e.g. a text delta to
    /// simulate a partially-streamed round).
    pre_error_events: Vec<AssistantMessageEvent>,
    error_is_transient: bool,
}

impl FlakyProvider {
    fn clean_failures(n: usize) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            fail_first: n,
            pre_error_events: Vec::new(),
            error_is_transient: true,
        }
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for FlakyProvider {
    async fn stream(
        &self,
        _model: &Model,
        _context: &Context,
        _options: &StreamOptions,
    ) -> ocean_protocol::Result<AssistantMessageEventStream> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call < self.fail_first {
            let mut events: Vec<ocean_protocol::Result<AssistantMessageEvent>> =
                self.pre_error_events.clone().into_iter().map(Ok).collect();
            if self.error_is_transient {
                events.push(Err(transient_err()));
            } else {
                // A provider-reported error frame (content block, refusal…) —
                // deterministic, must NOT be retried.
                events.push(Ok(AssistantMessageEvent::Error {
                    reason: StopReason::Error,
                    error: AssistantMessage {
                        error_message: Some("content blocked".into()),
                        ..assistant_msg(vec![], StopReason::Error)
                    },
                }));
            }
            return Ok(Box::pin(stream::iter(events)));
        }
        let events = vec![
            Ok(text_delta("recovered")),
            Ok(done(vec![Content::text("recovered")], StopReason::Stop)),
        ];
        Ok(Box::pin(stream::iter(events)))
    }
}

fn config(provider: Arc<FlakyProvider>) -> AgentConfig {
    AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test")
        .with_session_id("retry")
        .with_provider(provider)
}

// ===========================================================================
// 1 — Clean-round transient failure: retried, turn succeeds. Two failures then
// success exercises more than one retry (budget is 3 attempts).
// ===========================================================================
#[tokio::test]
async fn clean_round_transient_failure_is_retried_to_success() {
    let provider = Arc::new(FlakyProvider::clean_failures(2));
    let cfg = config(provider.clone());

    let (tx, mut rx) = mpsc::unbounded_channel();
    let start = tokio::time::Instant::now();
    let run = ocean_runtime::run_agent(&cfg, user("hi"), Some(tx))
        .await
        .expect("two clean transient failures must be retried to success");

    // Three provider calls: fail, fail, succeed.
    assert_eq!(provider.call_count(), 3);
    // Backoff actually waited (500ms + 1s = 1.5s minimum).
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(1400),
        "expected backoff delays before the retries, elapsed {:?}",
        start.elapsed()
    );
    // The final answer landed.
    let last_text = run
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => a.content.iter().find_map(|c| c.as_text()),
            _ => None,
        })
        .expect("final assistant text");
    assert_eq!(last_text, "recovered");
    // No duplicated text deltas from the failed attempts (they streamed nothing).
    let mut deltas = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::TextDelta { delta, .. } = ev {
            deltas.push(delta);
        }
    }
    assert_eq!(
        deltas,
        vec!["recovered"],
        "only the successful attempt's text may reach the sink"
    );
}

// ===========================================================================
// 2 — A round that already streamed text is NOT retried: the partial reached
// the user; replaying would duplicate it. The turn fails with the error.
// ===========================================================================
#[tokio::test]
async fn partially_streamed_round_is_not_retried() {
    let provider = Arc::new(FlakyProvider {
        calls: AtomicUsize::new(0),
        fail_first: 1,
        pre_error_events: vec![text_delta("partial ")],
        error_is_transient: true,
    });
    let cfg = config(provider.clone());

    let err = ocean_runtime::run_agent(&cfg, user("hi"), None)
        .await
        .err()
        .expect("a partially-streamed round must fail, not silently retry");
    assert!(
        format!("{err}").contains("overloaded"),
        "the transient error must surface, got: {err}"
    );
    assert_eq!(
        provider.call_count(),
        1,
        "a dirty round must not be re-attempted"
    );
}

// ===========================================================================
// 3 — A non-transient failure (provider error frame — content block, refusal)
// is NOT retried even on a clean round.
// ===========================================================================
#[tokio::test]
async fn non_transient_error_is_not_retried() {
    let provider = Arc::new(FlakyProvider {
        calls: AtomicUsize::new(0),
        fail_first: 1,
        pre_error_events: Vec::new(),
        error_is_transient: false,
    });
    let cfg = config(provider.clone());

    let err = ocean_runtime::run_agent(&cfg, user("hi"), None)
        .await
        .err()
        .expect("a provider error frame must fail the turn");
    assert!(format!("{err}").contains("content blocked"));
    assert_eq!(
        provider.call_count(),
        1,
        "a deterministic provider error must not be re-attempted"
    );
}

// ===========================================================================
// 4 — Bounded budget: a provider that always fails transiently exhausts the
// 3-attempt budget and the turn fails with the underlying error.
// ===========================================================================
#[tokio::test]
async fn persistent_transient_failure_exhausts_budget_and_fails() {
    let provider = Arc::new(FlakyProvider::clean_failures(usize::MAX));
    let cfg = config(provider.clone());

    let err = ocean_runtime::run_agent(&cfg, user("hi"), None)
        .await
        .err()
        .expect("persistent failure must eventually fail the turn");
    assert!(
        format!("{err}").contains("overloaded"),
        "the underlying error must surface after the budget, got: {err}"
    );
    assert_eq!(
        provider.call_count(),
        3,
        "exactly MAX_ROUND_ATTEMPTS provider calls, then give up"
    );
}
