//! End-to-end agent-loop tests (OCEAN-105).
//!
//! These drive the *real* `run_agent` loop through a scripted [`MockProvider`]
//! injected via [`AgentConfig::with_provider`] — no network, no credentials. The
//! loop's actual behavior (truncation handling, error surfacing, multi-round
//! tool loops, cancellation) is exercised through the same code path production
//! takes, only with the provider stream swapped for a canned sequence.
//!
//! Covers the regressions the OCEAN-99/101/103 sweeps fixed:
//! - OCEAN-103: a complete tool_use that stops on `Length` (truncation) must be
//!   paired with a synthetic error tool_result — no orphan tool_use survives.
//! - OCEAN-101: a provider `Error`/blocked frame must surface as `Err`, not a
//!   silent empty success.
//! - Multi-round tool loop: tool_use → tool runs → final text → clean completion.
//! - Cancellation: a cancel mid-loop unwinds with `Cancelled`, no orphan call.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream;
use ocean_protocol::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context, Error,
    Message, Model, Provider, StopReason, StreamOptions, Usage,
};
use ocean_runtime::types::{AgentConfig, AgentEvent, AgentTool, AgentToolResult};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// MockProvider — replays a scripted sequence of provider "turns".
// ---------------------------------------------------------------------------

/// One scripted round of streamed provider events. Each call to
/// [`Provider::stream`] consumes the next `Turn` in order, so a multi-round
/// agent loop (tool_use → tool runs → final text) is driven by supplying a
/// `Vec<Turn>`.
type Turn = Vec<AssistantMessageEvent>;

/// A provider whose `stream` returns a canned sequence of events per call.
/// Round N of the agent loop receives `turns[N]`. Running past the end of the
/// script panics (a test bug — the loop asked for more turns than scripted).
struct MockProvider {
    turns: std::sync::Mutex<std::collections::VecDeque<Turn>>,
    calls: AtomicUsize,
}

impl MockProvider {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns: std::sync::Mutex::new(turns.into()),
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream(
        &self,
        _model: &Model,
        _context: &Context,
        _options: &StreamOptions,
    ) -> ocean_protocol::Result<AssistantMessageEventStream> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .expect("MockProvider ran out of scripted turns — loop requested more rounds than scripted");
        let events: Vec<ocean_protocol::Result<AssistantMessageEvent>> =
            turn.into_iter().map(Ok).collect();
        Ok(Box::pin(stream::iter(events)))
    }
}

// ---------------------------------------------------------------------------
// Helpers to build scripted events.
// ---------------------------------------------------------------------------

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

fn tool_call(id: &str, name: &str, args: Value) -> Content {
    Content::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

/// A `Done` terminal event carrying `content` and the given stop reason.
fn done(content: Vec<Content>, stop: StopReason) -> AssistantMessageEvent {
    AssistantMessageEvent::Done {
        reason: stop,
        message: assistant_msg(content, stop),
    }
}

/// A streamed text delta (so the loop's TextDelta emit path is exercised).
fn text_delta(s: &str) -> AssistantMessageEvent {
    AssistantMessageEvent::TextDelta {
        content_index: 0,
        delta: s.into(),
    }
}

// ---------------------------------------------------------------------------
// A no-op echo tool — records that it ran and echoes its args back.
// ---------------------------------------------------------------------------

struct EchoTool {
    ran: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echoes its input back"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    // No permission required, so the loop runs it without consulting the policy.
    fn requires_permission(&self) -> bool {
        false
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(AgentToolResult::text(format!("echo: {args}")))
    }
}

fn user(s: &str) -> Message {
    Message::user_text(s)
}

fn collect_events(rx: &mut mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        out.push(ev);
    }
    out
}

/// No assistant tool_use may be left without a matching tool_result later in the
/// transcript (the invariant providers enforce on the *next* turn).
fn assert_no_orphan_tool_use(messages: &[Message]) {
    use std::collections::HashSet;
    let mut answered: HashSet<&str> = HashSet::new();
    for m in messages {
        if let Message::ToolResult(r) = m {
            answered.insert(r.tool_call_id.as_str());
        }
    }
    for m in messages {
        if let Message::Assistant(a) = m {
            for c in &a.content {
                if let Content::ToolCall { id, .. } = c {
                    assert!(
                        answered.contains(id.as_str()),
                        "orphan tool_use {id:?} has no matching tool_result: {messages:#?}",
                    );
                }
            }
        }
    }
}

fn base_config(provider: Arc<MockProvider>) -> AgentConfig {
    AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test system")
        .with_session_id("e2e")
        .with_provider(provider)
}

// ===========================================================================
// Scenario 1 — Truncated tool_use (OCEAN-103).
// Provider emits a *complete* tool_use but the round stops on `Length`
// (output truncated at the token limit after the call was assembled). The loop
// must NOT execute it and must pair it with a synthetic error tool_result so the
// transcript has no orphan tool_use.
// ===========================================================================
#[tokio::test]
async fn truncated_tool_use_is_paired_with_error_result_no_orphan() {
    let ran = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(MockProvider::new(vec![vec![
        text_delta("partial answer "),
        done(
            vec![
                Content::text("partial answer "),
                tool_call("call-trunc", "echo", serde_json::json!({ "x": 1 })),
            ],
            // Complete tool_use, but stopped on Length, NOT ToolUse.
            StopReason::Length,
        ),
    ]]));

    let cfg = base_config(provider.clone()).with_tools(vec![Arc::new(EchoTool { ran: ran.clone() })]);

    let run = ocean_runtime::run_agent(&cfg, user("do a thing"), None)
        .await
        .expect("truncated tool_use must not error the run; it pairs a synthetic result");

    // The tool was NOT executed (its args may be truncated, and the stop reason
    // was not ToolUse).
    assert_eq!(ran.load(Ordering::SeqCst), 0, "truncated tool_use must not run the tool");

    // Transcript is provider-valid: the orphan tool_use got a paired result.
    assert_no_orphan_tool_use(&run.messages);

    // That paired result is a flagged error so the model sees the call didn't
    // run (not a fake success).
    let Some(Message::ToolResult(tr)) = run.messages.last() else {
        panic!("last message must be the synthetic paired tool result, got {:#?}", run.messages.last());
    };
    assert!(tr.is_error, "truncated tool_use result must be is_error");
    assert_eq!(tr.tool_call_id, "call-trunc");

    // Exactly one provider round happened — the loop did not spin again.
    assert_eq!(provider.call_count(), 1);
}

// ===========================================================================
// Scenario 2 — Blocked / error response (OCEAN-101).
// Provider emits an `Error` frame → the loop must surface it as `Err`, never a
// silent empty success.
// ===========================================================================
#[tokio::test]
async fn provider_error_frame_surfaces_as_err_not_silent_success() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        AssistantMessageEvent::Error {
            reason: StopReason::Error,
            error: AssistantMessage {
                error_message: Some("content blocked by provider safety filter".into()),
                ..assistant_msg(vec![], StopReason::Error)
            },
        },
    ]]));

    let cfg = base_config(provider);

    let err = ocean_runtime::run_agent(&cfg, user("trigger a block"), None)
        .await
        .err()
        .expect("a provider error frame must produce Err, not Ok(empty success)");

    let msg = format!("{err}");
    assert!(
        msg.contains("content blocked"),
        "the provider error message must surface to the caller, got: {msg}"
    );
}

// A provider stream that ends abruptly (no `Done`, no `Error` — abrupt EOF) must
// surface as an error, not a silent empty success.
#[tokio::test]
async fn abrupt_eof_without_terminal_event_is_an_error() {
    let provider = Arc::new(MockProvider::new(vec![vec![
        // Some text, then the stream just ends — no Done/Error terminal frame.
        text_delta("typing"),
    ]]));

    let cfg = base_config(provider);

    let err = ocean_runtime::run_agent(&cfg, user("hi"), None)
        .await
        .err()
        .expect("a stream with no terminal event must be Err, not Ok");
    assert!(
        format!("{err}").contains("no terminal event"),
        "abrupt EOF must surface as a 'no terminal event' error, got: {err}"
    );
}

// A transport-level `Err` mid-stream must surface as an `Err` from the run.
#[tokio::test]
async fn transport_error_mid_stream_surfaces_as_err() {
    // Hand-roll a stream that yields a transport error directly (not the
    // protocol-level `Error` event, but a `Result::Err`).
    struct ErrProvider;
    #[async_trait]
    impl Provider for ErrProvider {
        async fn stream(
            &self,
            _m: &Model,
            _c: &Context,
            _o: &StreamOptions,
        ) -> ocean_protocol::Result<AssistantMessageEventStream> {
            let events: Vec<ocean_protocol::Result<AssistantMessageEvent>> =
                vec![Err(Error::Other("connection reset".into()))];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    let cfg = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test")
        .with_session_id("e2e")
        .with_provider(Arc::new(ErrProvider));

    let err = ocean_runtime::run_agent(&cfg, user("hi"), None)
        .await
        .err()
        .expect("a transport error mid-stream must be Err");
    assert!(
        format!("{err}").contains("connection reset"),
        "transport error text must surface, got: {err}"
    );
}

// ===========================================================================
// Scenario 3 — Multi-round tool loop.
// Round 1: provider emits a tool_use (stop=ToolUse). The loop runs the tool.
// Round 2: provider emits final text (stop=Stop). Loop completes cleanly.
// ===========================================================================
#[tokio::test]
async fn multi_round_tool_loop_runs_tool_then_completes() {
    let ran = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(MockProvider::new(vec![
        // Round 1: a tool call.
        vec![done(
            vec![tool_call("call-1", "echo", serde_json::json!({ "msg": "hi" }))],
            StopReason::ToolUse,
        )],
        // Round 2: final answer.
        vec![
            text_delta("all "),
            text_delta("done"),
            done(vec![Content::text("all done")], StopReason::Stop),
        ],
    ]));

    let cfg = base_config(provider.clone()).with_tools(vec![Arc::new(EchoTool { ran: ran.clone() })]);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let run = ocean_runtime::run_agent(&cfg, user("use the tool"), Some(tx))
        .await
        .expect("multi-round tool loop must complete cleanly");

    // The tool ran exactly once.
    assert_eq!(ran.load(Ordering::SeqCst), 1, "echo tool should have run once");
    // Two provider rounds: the tool-call round and the final-text round.
    assert_eq!(provider.call_count(), 2);
    // Transcript is provider-valid — the tool_use has its matching result.
    assert_no_orphan_tool_use(&run.messages);
    // The run did not stop at the turn limit.
    assert!(!run.stopped_at_turn_limit);

    // Transcript shape: user, assistant(tool_use), tool_result, assistant(text).
    assert!(matches!(run.messages.first(), Some(Message::User { .. })));
    let last_text = run
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => a.content.iter().find_map(|c| c.as_text()),
            _ => None,
        })
        .expect("a final assistant text block");
    assert_eq!(last_text, "all done");

    // Event stream carried a ToolExecutionStart + End pair for the call.
    let events = collect_events(&mut rx);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolExecutionStart { tool_name, .. } if tool_name == "echo")),
        "expected a ToolExecutionStart for echo"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolExecutionEnd { tool_name, is_error, .. } if tool_name == "echo" && !is_error)),
        "expected a non-error ToolExecutionEnd for echo"
    );
    // And the streamed text deltas reached the bus.
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::TextDelta { delta, .. } if delta == "all ")),
        "expected the streamed text delta to be emitted"
    );
}

// ===========================================================================
// Scenario 4 — Cancellation mid-loop.
// The loop checks the cancel token between rounds and mid-stream. We cancel
// after round 1's tool call, so round 2 must never run and the loop unwinds with
// `Cancelled` — without leaving an orphan tool_use (the tool result for round 1
// was already appended).
// ===========================================================================
#[tokio::test]
async fn cancel_after_tool_round_unwinds_clean_no_orphan() {
    let ran = Arc::new(AtomicUsize::new(0));
    let token = CancellationToken::new();

    // A tool whose execution cancels the run — simulating a halt arriving while
    // a tool was running. The loop's between-rounds cancel check then fires
    // before round 2.
    struct CancellingTool {
        ran: Arc<AtomicUsize>,
        token: CancellationToken,
    }
    #[async_trait]
    impl AgentTool for CancellingTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "cancels the run when called"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
            self.ran.fetch_add(1, Ordering::SeqCst);
            self.token.cancel();
            Ok(AgentToolResult::text("ran, then cancelled"))
        }
    }

    let provider = Arc::new(MockProvider::new(vec![
        // Round 1: a tool call.
        vec![done(
            vec![tool_call("call-1", "echo", serde_json::json!({}))],
            StopReason::ToolUse,
        )],
        // Round 2 should NEVER be requested — the cancel check fires first. If
        // the loop did request it, MockProvider has a scripted turn to give, so
        // we'd see call_count == 2 (and the test below catches that).
        vec![done(vec![Content::text("should not happen")], StopReason::Stop)],
    ]));

    let mut cfg = base_config(provider.clone()).with_tools(vec![Arc::new(CancellingTool {
        ran: ran.clone(),
        token: token.clone(),
    })]);
    cfg.stream_options.cancel = Some(token);

    let err = ocean_runtime::run_agent(&cfg, user("use the tool then we halt"), None)
        .await
        .err()
        .expect("a cancel after the tool round must unwind with Err(Cancelled)");
    assert!(
        matches!(err, ocean_runtime::AgentError::Cancelled),
        "expected AgentError::Cancelled, got {err:?}"
    );

    // The tool ran once (round 1), and round 2 was never requested from the
    // provider — the between-rounds cancel check stopped the loop.
    assert_eq!(ran.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.call_count(),
        1,
        "round 2 must not have been requested after cancel"
    );
}

// Pre-cancelled before any round: must bail before touching the provider at all.
#[tokio::test]
async fn pre_cancelled_run_never_calls_provider() {
    let token = CancellationToken::new();
    token.cancel();

    let provider = Arc::new(MockProvider::new(vec![vec![done(
        vec![Content::text("never reached")],
        StopReason::Stop,
    )]]));

    let mut cfg = base_config(provider.clone());
    cfg.stream_options.cancel = Some(token);

    let err = ocean_runtime::run_agent(&cfg, user("hi"), None)
        .await
        .err()
        .expect("a pre-cancelled run must return Err");
    assert!(matches!(err, ocean_runtime::AgentError::Cancelled));
    assert_eq!(
        provider.call_count(),
        0,
        "pre-cancelled run must never call the provider"
    );
}
