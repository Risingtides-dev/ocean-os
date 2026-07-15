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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream;
use ocean_protocol::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context, Error,
    Message, Model, Provider, StopReason, StreamOptions, Usage,
};
use ocean_runtime::types::{
    AgentConfig, AgentEvent, AgentTool, AgentToolResult, PermissionDecision, PermissionPolicy,
};
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
    saw_bound_session_id: AtomicBool,
}

impl MockProvider {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns: std::sync::Mutex::new(turns.into()),
            calls: AtomicUsize::new(0),
            saw_bound_session_id: AtomicBool::new(false),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn saw_bound_session_id(&self) -> bool {
        self.saw_bound_session_id.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream(
        &self,
        _model: &Model,
        _context: &Context,
        options: &StreamOptions,
    ) -> ocean_protocol::Result<AssistantMessageEventStream> {
        self.saw_bound_session_id.store(
            options.session_id.as_deref() == Some("e2e"),
            Ordering::SeqCst,
        );
        self.calls.fetch_add(1, Ordering::SeqCst);
        let turn = self.turns.lock().unwrap().pop_front().expect(
            "MockProvider ran out of scripted turns — loop requested more rounds than scripted",
        );
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

// ---------------------------------------------------------------------------
// A permission-requiring tool — same as EchoTool but `requires_permission()`
// is true, so the loop consults `config.permission.check(...)` before running
// it. This is the seam that exercises the Allow / AllowSession / Deny gate arms.
// ---------------------------------------------------------------------------

struct GatedTool {
    ran: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentTool for GatedTool {
    fn name(&self) -> &str {
        "gated"
    }
    fn description(&self) -> &str {
        "a tool that requires permission before running"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    // The crux: this forces the loop through the permission gate.
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        self.ran.fetch_add(1, Ordering::SeqCst);
        Ok(AgentToolResult::text(format!("gated ran: {args}")))
    }
}

// ---------------------------------------------------------------------------
// A scriptable mock permission policy. Returns a pre-set decision and counts
// how many times `check` was consulted, so a test can assert the session cache
// (AllowSession) means the policy is NOT re-consulted on the second call.
//
// Implemented entirely in the test crate against the public `PermissionPolicy`
// trait + `PermissionDecision` enum — no production permission code is touched.
// ---------------------------------------------------------------------------

struct ScriptedPolicy {
    decision: PermissionDecision,
    checks: Arc<AtomicUsize>,
    check_all: bool,
}

impl ScriptedPolicy {
    fn new(decision: PermissionDecision) -> (Arc<Self>, Arc<AtomicUsize>) {
        Self::with_check_all(decision, false)
    }

    fn always_check(decision: PermissionDecision) -> (Arc<Self>, Arc<AtomicUsize>) {
        Self::with_check_all(decision, true)
    }

    fn with_check_all(
        decision: PermissionDecision,
        check_all: bool,
    ) -> (Arc<Self>, Arc<AtomicUsize>) {
        let checks = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                decision,
                checks: checks.clone(),
                check_all,
            }),
            checks,
        )
    }
}

#[async_trait]
impl PermissionPolicy for ScriptedPolicy {
    fn should_check(
        &self,
        _tool_name: &str,
        _args: &Value,
        tool_requires_permission: bool,
    ) -> bool {
        self.check_all || tool_requires_permission
    }

    async fn check(&self, _tool_name: &str, _args: &Value) -> PermissionDecision {
        self.checks.fetch_add(1, Ordering::SeqCst);
        self.decision.clone()
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

#[tokio::test]
async fn session_identity_reaches_every_provider_round() {
    let provider = Arc::new(MockProvider::new(vec![vec![done(
        vec![Content::text("done")],
        StopReason::Stop,
    )]]));
    let cfg = base_config(provider.clone());

    ocean_runtime::run_agent(&cfg, user("finish"), None)
        .await
        .expect("agent run succeeds");

    assert!(
        provider.saw_bound_session_id(),
        "AgentConfig.session_id must reach StreamOptions for provider cache identity"
    );
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

    let cfg =
        base_config(provider.clone()).with_tools(vec![Arc::new(EchoTool { ran: ran.clone() })]);

    let run = ocean_runtime::run_agent(&cfg, user("do a thing"), None)
        .await
        .expect("truncated tool_use must not error the run; it pairs a synthetic result");

    // The tool was NOT executed (its args may be truncated, and the stop reason
    // was not ToolUse).
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "truncated tool_use must not run the tool"
    );

    // Transcript is provider-valid: the orphan tool_use got a paired result.
    assert_no_orphan_tool_use(&run.messages);

    // That paired result is a flagged error so the model sees the call didn't
    // run (not a fake success).
    let Some(Message::ToolResult(tr)) = run.messages.last() else {
        panic!(
            "last message must be the synthetic paired tool result, got {:#?}",
            run.messages.last()
        );
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
async fn final_round_after_tool_result_disables_tools_and_forces_reply() {
    struct FinalizeOnNoToolsProvider {
        calls: AtomicUsize,
        saw_empty_tools: std::sync::Mutex<bool>,
    }

    #[async_trait]
    impl Provider for FinalizeOnNoToolsProvider {
        async fn stream(
            &self,
            _model: &Model,
            context: &Context,
            _options: &StreamOptions,
        ) -> ocean_protocol::Result<AssistantMessageEventStream> {
            let round = self.calls.fetch_add(1, Ordering::SeqCst);
            let events = if context.tools.is_empty() {
                *self.saw_empty_tools.lock().unwrap() = true;
                vec![
                    text_delta("final"),
                    done(vec![Content::text("final answer")], StopReason::Stop),
                ]
            } else {
                vec![done(
                    vec![tool_call(
                        &format!("call-{round}"),
                        "echo",
                        serde_json::json!({ "round": round }),
                    )],
                    StopReason::ToolUse,
                )]
            };
            Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
        }
    }

    let ran = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(FinalizeOnNoToolsProvider {
        calls: AtomicUsize::new(0),
        saw_empty_tools: std::sync::Mutex::new(false),
    });
    let cfg = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test system")
        .with_session_id("e2e")
        .with_provider(provider.clone())
        .with_max_turns(2)
        .with_tools(vec![Arc::new(EchoTool { ran: ran.clone() })]);

    let run = ocean_runtime::run_agent(&cfg, user("use tools, then answer"), None)
        .await
        .expect("final round should produce text instead of another tool call");

    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "only the first round runs a tool"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert!(*provider.saw_empty_tools.lock().unwrap());
    assert!(!run.stopped_at_turn_limit);
    let last_text = run
        .messages
        .iter()
        .rev()
        .find_map(|m| match m {
            Message::Assistant(a) => a.content.iter().find_map(|c| c.as_text()),
            _ => None,
        })
        .expect("final assistant text");
    assert_eq!(last_text, "final answer");
}

#[tokio::test]
async fn multi_round_tool_loop_runs_tool_then_completes() {
    let ran = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(MockProvider::new(vec![
        // Round 1: a tool call.
        vec![done(
            vec![tool_call(
                "call-1",
                "echo",
                serde_json::json!({ "msg": "hi" }),
            )],
            StopReason::ToolUse,
        )],
        // Round 2: final answer.
        vec![
            text_delta("all "),
            text_delta("done"),
            done(vec![Content::text("all done")], StopReason::Stop),
        ],
    ]));

    let cfg =
        base_config(provider.clone()).with_tools(vec![Arc::new(EchoTool { ran: ran.clone() })]);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let run = ocean_runtime::run_agent(&cfg, user("use the tool"), Some(tx))
        .await
        .expect("multi-round tool loop must complete cleanly");

    // The tool ran exactly once.
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "echo tool should have run once"
    );
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
        events.iter().any(
            |e| matches!(e, AgentEvent::ToolExecutionStart { tool_name, .. } if tool_name == "echo")
        ),
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
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta { delta, .. } if delta == "all ")),
        "expected the streamed text delta to be emitted"
    );
}

// ===========================================================================
// Scenario 4 — Cancellation mid-loop.
// The loop checks the cancel token between rounds and mid-stream. We cancel
// after round 1's tool call, so round 2 must never run and the loop unwinds with
// `Cancelled` — without leaving an orphan tool_use. Cancellation becomes ready
// at the exact tool-completion boundary, so the runtime must checkpoint a
// conservative consumed/error result before returning.
// ===========================================================================
#[tokio::test]
async fn cancel_after_tool_round_unwinds_clean_no_orphan() {
    let ran = Arc::new(AtomicUsize::new(0));
    let later_ran = Arc::new(AtomicUsize::new(0));
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

    // A later exclusive barrier that must never execute after the first tool
    // cancels, but whose assistant tool call still needs a synthetic result in
    // the durable checkpoint.
    struct NeverTool(Arc<AtomicUsize>);
    #[async_trait]
    impl AgentTool for NeverTool {
        fn name(&self) -> &str {
            "later"
        }
        fn description(&self) -> &str {
            "must not run after cancellation"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(AgentToolResult::text("unexpected"))
        }
    }

    let provider = Arc::new(MockProvider::new(vec![
        // Round 1: cancelling tool followed by a later exclusive barrier.
        vec![done(
            vec![
                tool_call("call-1", "echo", serde_json::json!({})),
                tool_call("call-2", "later", serde_json::json!({})),
            ],
            StopReason::ToolUse,
        )],
        // Round 2 should NEVER be requested — the cancel check fires first. If
        // the loop did request it, MockProvider has a scripted turn to give, so
        // we'd see call_count == 2 (and the test below catches that).
        vec![done(
            vec![Content::text("should not happen")],
            StopReason::Stop,
        )],
    ]));

    let mut cfg = base_config(provider.clone()).with_tools(vec![
        Arc::new(CancellingTool {
            ran: ran.clone(),
            token: token.clone(),
        }),
        Arc::new(NeverTool(later_ran.clone())),
    ]);
    cfg.stream_options.cancel = Some(token);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let err = ocean_runtime::run_agent(&cfg, user("use the tool then we halt"), Some(tx))
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
        later_ran.load(Ordering::SeqCst),
        0,
        "later execution barrier must not start after cancellation"
    );
    assert_eq!(
        provider.call_count(),
        1,
        "round 2 must not have been requested after cancel"
    );

    let mut checkpoint = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::TurnCheckpoint { messages, .. } = event {
            checkpoint = Some(messages);
        }
    }
    let checkpoint = checkpoint.expect("cancelled tool batch must be checkpointed");
    assert_eq!(
        checkpoint.len(),
        3,
        "assistant batch + both ordered results stay paired"
    );
    assert!(matches!(checkpoint[0], Message::Assistant(_)));
    assert!(matches!(
        &checkpoint[1],
        Message::ToolResult(result) if result.tool_call_id == "call-1"
    ));
    assert!(matches!(
        &checkpoint[2],
        Message::ToolResult(result)
            if result.is_error && result.tool_call_id == "call-2"
    ));
}

// ===========================================================================
// Scenario 5 — Cancellation DURING a long-running tool call (OCEAN-116).
// Before this fix, once a tool started executing the loop blocked on it until
// it returned, even if the turn was cancelled — the only cancel checks were at
// turn-start and between provider stream chunks, never while a tool was in
// flight. This test fires a tool that sleeps far longer than the test's patience,
// cancels the run from another task shortly after the tool starts, and asserts:
//   1. the run unwinds with `Cancelled` PROMPTLY (well under the tool's duration),
//   2. the slow tool never reached its completion line (it was aborted, not awaited).
// If the loop awaited the tool to completion, the test would block for the full
// sleep and the "finished" flag would be set — both of which we reject.
// ===========================================================================
#[tokio::test]
async fn cancel_during_long_tool_aborts_promptly_without_awaiting_completion() {
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    // How long the fake tool "runs" for. Deliberately huge relative to the
    // promptness bound below: if cancellation were NOT racing the tool, the run
    // would block for this entire duration.
    const TOOL_RUN: Duration = Duration::from_secs(30);
    // Cancellation is fired this long after the run starts (enough for the tool
    // to have begun executing).
    const CANCEL_AFTER: Duration = Duration::from_millis(150);
    // The run must return within this bound after cancel — far below TOOL_RUN.
    const PROMPTNESS_BOUND: Duration = Duration::from_secs(5);

    let started = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let token = CancellationToken::new();

    // A tool that records when it starts, sleeps for a long time, then records
    // completion. The cancel must abort it between "started" and "finished".
    struct SlowTool {
        started: Arc<AtomicBool>,
        finished: Arc<AtomicBool>,
        run_for: Duration,
    }
    #[async_trait]
    impl AgentTool for SlowTool {
        fn name(&self) -> &str {
            "slow"
        }
        fn description(&self) -> &str {
            "sleeps for a long time, simulating a slow bash / network tool"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        // No permission gate, so the loop runs it directly.
        fn requires_permission(&self) -> bool {
            false
        }
        async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
            self.started.store(true, Ordering::SeqCst);
            tokio::time::sleep(self.run_for).await;
            // Reached ONLY if the tool was awaited to completion — i.e. the bug.
            self.finished.store(true, Ordering::SeqCst);
            Ok(AgentToolResult::text("slow tool finished"))
        }
    }

    let provider = Arc::new(MockProvider::new(vec![
        // Round 1: emit the slow tool call.
        vec![done(
            vec![tool_call("call-slow", "slow", serde_json::json!({}))],
            StopReason::ToolUse,
        )],
        // Round 2 must NEVER run — the tool is aborted mid-flight and the loop
        // unwinds with Cancelled before requesting another round.
        vec![done(
            vec![Content::text("should not happen")],
            StopReason::Stop,
        )],
    ]));

    let mut cfg = base_config(provider.clone()).with_tools(vec![Arc::new(SlowTool {
        started: started.clone(),
        finished: finished.clone(),
        run_for: TOOL_RUN,
    })]);
    cfg.stream_options.cancel = Some(token.clone());

    // Fire the cancel from a separate task shortly after the run begins, while
    // the slow tool is mid-sleep.
    let canceller = {
        let token = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CANCEL_AFTER).await;
            token.cancel();
        })
    };

    let start = Instant::now();
    let err = ocean_runtime::run_agent(&cfg, user("run the slow tool"), None)
        .await
        .err()
        .expect("cancel during a long tool call must unwind with Err(Cancelled)");
    let elapsed = start.elapsed();
    let _ = canceller.await;

    // 1. Unwound with Cancelled.
    assert!(
        matches!(err, ocean_runtime::AgentError::Cancelled),
        "expected AgentError::Cancelled, got {err:?}"
    );
    // 2. The tool DID start (so we genuinely tested mid-execution cancel, not a
    //    between-rounds one).
    assert!(
        started.load(Ordering::SeqCst),
        "the slow tool should have started executing before the cancel"
    );
    // 3. The tool was aborted, NOT awaited to completion.
    assert!(
        !finished.load(Ordering::SeqCst),
        "the slow tool must have been aborted mid-flight, not run to completion"
    );
    // 4. The run returned PROMPTLY — well under the tool's full duration. This is
    //    the core of OCEAN-116: cancel takes effect without waiting for the tool.
    assert!(
        elapsed < PROMPTNESS_BOUND,
        "run must unwind promptly after cancel ({elapsed:?}), not block for the \
         tool's full {TOOL_RUN:?}"
    );
    // 5. Round 2 was never requested.
    assert_eq!(
        provider.call_count(),
        1,
        "the loop must not request another round after cancelling a tool"
    );
}

// ===========================================================================
// Scenario 5b — Halt DURING a silent (never-yielding) provider stream.
//
// The defect (spec §1): the stream-read boundary at `agent_loop.rs:205` checks
// the cancel token *post-yield* — `if is_cancelled(config)` runs only after
// `stream.next().await` resolves. A provider that accepts the connection and
// then goes silent (no Done, no error, no bytes — the "accepts then stalls"
// hang) leaves `stream.next().await` blocked forever, so a user Halt landing on
// that silent socket is ignored until the 120s byte-idle read_timeout or the
// 300s round deadline fires — NOT immediate.
//
// This test reproduces the silent socket with a provider whose stream is
// `futures::stream::pending()` (next() is Pending forever), trips the
// CancellationToken from a second task ~50ms in, and asserts the post-fix
// contract: the run unwinds with `Err(AgentError::Cancelled)` inside a
// sub-second outer budget — far below 120s/300s — proving a cancel-race at the
// read boundary (not a wall-clock bound) broke the blocking read.
//
// ASSERTION DIRECTION (load-bearing): the outer-budget-elapsed branch is a
// TEST FAILURE (panic), never an expected pass. Pre-fix the run stays blocked
// in `stream.next().await` past the 750ms budget and the test MUST fail here;
// post-fix the biased `select!` resolves the cancel arm promptly and it passes.
// Real time only — `start_paused` would auto-fire the 300s round deadline and
// invalidate the proof.
// ===========================================================================
#[tokio::test]
async fn halt_during_silent_provider_stream_cancels_promptly() {
    use std::time::{Duration, Instant};

    // Trip the token this long after the run starts — long enough for round 1
    // to have entered `stream_work` and be blocked on `stream.next().await`,
    // short enough to land well inside the outer budget.
    const CANCEL_AFTER: Duration = Duration::from_millis(50);
    // Sub-second outer budget. Far below the 120s read_timeout and the 300s
    // round deadline — if the run is still live at this point, Halt did NOT
    // break the silent read, i.e. the pre-fix bug. Elapsing here is a failure.
    const OUTER_BUDGET: Duration = Duration::from_millis(750);

    let token = CancellationToken::new();

    // A provider whose stream NEVER resolves — its `next()` is `Pending`
    // forever (no Done, no Error, no items). This is the silent socket: the
    // existing MockProvider can't express "never yields" (it pops scripted
    // turns), so we reuse the same `Provider` impl pattern locally and return
    // `futures::stream::pending()` boxed as the protocol's event stream.
    struct SilentProvider {
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl Provider for SilentProvider {
        async fn stream(
            &self,
            _model: &Model,
            _context: &Context,
            _options: &StreamOptions,
        ) -> ocean_protocol::Result<AssistantMessageEventStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // `pending::<T>()` is a stream of `T` that never yields anything —
            // `StreamExt::next()` on it is Pending forever, exactly the silent
            // socket. `Box::pin` coerces the concrete stream to `BoxStream`.
            Ok(Box::pin(stream::pending::<
                ocean_protocol::Result<AssistantMessageEvent>,
            >()))
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    // `with_provider` takes `Arc<dyn Provider>`, so build the config directly
    // (mirroring `base_config`) rather than through the `Arc<MockProvider>`
    // helper.
    let provider: Arc<dyn Provider> = Arc::new(SilentProvider {
        calls: calls.clone(),
    });
    let mut cfg = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test system")
        .with_session_id("e2e")
        .with_provider(provider);
    cfg.stream_options.cancel = Some(token.clone());

    // Trip the token from a second task a fixed interval after the run begins,
    // while the loop is blocked mid-stream-read. (Not at run start: the
    // start-of-round `is_cancelled` guard would bail before calling the
    // provider, which is a different path and wouldn't exercise the boundary.)
    let canceller = {
        let token = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(CANCEL_AFTER).await;
            token.cancel();
        })
    };

    let start = Instant::now();
    let outcome = tokio::time::timeout(
        OUTER_BUDGET,
        ocean_runtime::run_agent(&cfg, user("anything — provider is silent"), None),
    )
    .await;
    let elapsed = start.elapsed();
    let _ = canceller.await;

    // SELF-VALIDATION (deliberately BEFORE the budget panic below): the provider
    // stream MUST have been entered — call_count == 1 proves the run got past
    // the start-of-round `is_cancelled` guard and was genuinely blocked on the
    // never-yielding read (`stream.next().await`), not stalled earlier in setup.
    // This is what makes the budget-elapse failure proof of the read-boundary
    // defect rather than a generic hang that never reached the provider.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "round 1 must have entered stream_work and blocked on the silent stream"
    );

    // The load-bearing assertion: the run MUST resolve inside the budget.
    // Elapsing means the silent-stream read was not broken by Halt — the
    // post-fix cancel-race at the read boundary is missing (the pre-fix bug).
    let result = outcome.unwrap_or_else(|_elapsed| {
        panic!(
            "run did NOT unwind on Halt within {OUTER_BUDGET:?} (elapsed {elapsed:?}); \
             the silent provider stream blocked `stream.next().await` past the \
             budget, so Halt was ignored until the 120s/300s wall-clock bound — \
             the read-boundary cancel-race (spec §6) is absent"
        );
    });

    match result {
        Err(ocean_runtime::AgentError::Cancelled) => {}
        other => panic!(
            "expected Err(AgentError::Cancelled) within {OUTER_BUDGET:?}; got {}",
            match other {
                Err(e) => format!("Err({e:?})"),
                Ok(_) => "Ok(AgentRun)".to_string(),
            }
        ),
    }
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

// ===========================================================================
// Scenario 6 — Permission gate: Deny arm + OCEAN-60 orphan invariant (OCEAN-197).
//
// The model calls a permission-requiring tool; the policy returns `Deny`. The
// gate must:
//   1. NOT execute the tool (no side effect — `ran` stays 0),
//   2. emit a `PermissionDenied` event and append an is_error tool_result so the
//      transcript stays provider-valid (the call has a matching result),
//   3. NEVER emit a `ToolExecutionStart` for that tool_call_id — the Deny arm's
//      `continue` fires *before* the Start emit (OCEAN-60: no Start-without-End
//      orphan on the event stream for a denied call).
//
// This proves no-execution (ran == 0, no Start) and the orphan invariant: if the
// gate were broken (Deny fell through to execution or emitted Start), `ran` would
// be 1 and/or a ToolExecutionStart for "gated" would appear — both rejected here.
// ===========================================================================
#[tokio::test]
async fn denied_tool_does_not_run_and_emits_no_execution_start() {
    let ran = Arc::new(AtomicUsize::new(0));
    let (policy, checks) = ScriptedPolicy::new(PermissionDecision::Deny {
        reason: "operator denied".into(),
    });

    let provider = Arc::new(MockProvider::new(vec![
        // Round 1: the model calls the gated tool.
        vec![done(
            vec![tool_call(
                "call-deny",
                "gated",
                serde_json::json!({ "x": 1 }),
            )],
            StopReason::ToolUse,
        )],
        // Round 2: after the denial result is fed back, the model wraps up. (The
        // loop continues the turn — denial is not a hard stop — so it needs a
        // final round to terminate cleanly.)
        vec![done(
            vec![Content::text("understood, stopping")],
            StopReason::Stop,
        )],
    ]));

    let cfg = base_config(provider.clone())
        .with_tools(vec![Arc::new(GatedTool { ran: ran.clone() })])
        .with_permission(policy);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let run = ocean_runtime::run_agent(&cfg, user("call the gated tool"), Some(tx))
        .await
        .expect("a denied tool does not error the run — it pairs a denial result");

    // 1. The policy WAS consulted for the gated call.
    assert_eq!(
        checks.load(Ordering::SeqCst),
        1,
        "the Deny gate must be consulted once"
    );
    // 2. The tool was NEVER executed — no side effect.
    assert_eq!(
        ran.load(Ordering::SeqCst),
        0,
        "a denied tool must not execute"
    );
    // 3. Transcript is provider-valid: the tool_use got a matching (error) result.
    assert_no_orphan_tool_use(&run.messages);
    let denial = run
        .messages
        .iter()
        .find_map(|m| match m {
            Message::ToolResult(tr) if tr.tool_call_id == "call-deny" => Some(tr),
            _ => None,
        })
        .expect("the denied call must have a paired tool_result");
    assert!(
        denial.is_error,
        "the denial tool_result must be flagged is_error"
    );

    let events = collect_events(&mut rx);
    // 4. A PermissionDenied event was emitted for the gated tool.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::PermissionDenied { tool_name, reason, .. }
                if tool_name == "gated" && reason == "operator denied"
        )),
        "expected a PermissionDenied event for the gated tool"
    );
    // 5. OCEAN-60 orphan invariant: NO ToolExecutionStart for the denied call.
    //    The Deny arm `continue`s before the Start emit, so a denied call must
    //    never appear on the stream as a Start (which would then have no End).
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolExecutionStart { tool_call_id, .. } if tool_call_id == "call-deny"
        )),
        "a denied tool must NOT emit ToolExecutionStart (OCEAN-60 orphan invariant)"
    );
    // And, symmetrically, no End either (nothing ran).
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolExecutionEnd { tool_name, .. } if tool_name == "gated"
        )),
        "a denied tool must not emit ToolExecutionEnd"
    );
}

// ===========================================================================
// Scenario 7 — Permission gate: AllowSession caches across calls (OCEAN-197).
//
// The policy returns `AllowSession` on first check. The model calls the same
// gated tool twice in one session/run. The gate must consult the policy only
// ONCE — the first AllowSession records the tool name in the run's session
// allow-set, and the second call skips the gate entirely (`needs_perm` is false
// once the name is cached). Both calls execute.
//
// Verified by the policy's check counter: `checks == 1` despite two executions.
// If the cache were broken, the policy would be consulted twice (checks == 2).
// ===========================================================================
#[tokio::test]
async fn allow_session_caches_decision_across_calls() {
    let ran = Arc::new(AtomicUsize::new(0));
    let (policy, checks) = ScriptedPolicy::new(PermissionDecision::AllowSession);

    let provider = Arc::new(MockProvider::new(vec![
        // Round 1: first call to the gated tool — gate is consulted, returns
        // AllowSession, caches the name.
        vec![done(
            vec![tool_call("call-a", "gated", serde_json::json!({ "n": 1 }))],
            StopReason::ToolUse,
        )],
        // Round 2: second call to the SAME tool — must run WITHOUT consulting the
        // policy again (session cache hit).
        vec![done(
            vec![tool_call("call-b", "gated", serde_json::json!({ "n": 2 }))],
            StopReason::ToolUse,
        )],
        // Round 3: wrap up.
        vec![done(vec![Content::text("both done")], StopReason::Stop)],
    ]));

    let cfg = base_config(provider.clone())
        .with_tools(vec![Arc::new(GatedTool { ran: ran.clone() })])
        .with_permission(policy);

    let run = ocean_runtime::run_agent(&cfg, user("call gated twice"), None)
        .await
        .expect("AllowSession run must complete cleanly");

    // Both calls executed.
    assert_eq!(
        ran.load(Ordering::SeqCst),
        2,
        "both gated calls should have run"
    );
    // The cache works: the policy was consulted exactly ONCE across two calls.
    assert_eq!(
        checks.load(Ordering::SeqCst),
        1,
        "AllowSession must cache — the policy is consulted once, not per call"
    );
    // Transcript stays provider-valid.
    assert_no_orphan_tool_use(&run.messages);
    assert_eq!(provider.call_count(), 3);
}

// ===========================================================================
// Scenario 8 — Permission gate: Allow arm (OCEAN-197 sanity/contrast).
//
// The policy returns `Allow`. The gated tool runs normally and emits a
// Start/End pair. Allow does NOT cache (unlike AllowSession), so a second call
// would consult the policy again — but here one call suffices to confirm the
// happy path: gate consulted, tool runs, Start+End emitted.
// ===========================================================================
#[tokio::test]
async fn allow_lets_gated_tool_run_normally() {
    let ran = Arc::new(AtomicUsize::new(0));
    let (policy, checks) = ScriptedPolicy::new(PermissionDecision::Allow);

    let provider = Arc::new(MockProvider::new(vec![
        vec![done(
            vec![tool_call("call-ok", "gated", serde_json::json!({}))],
            StopReason::ToolUse,
        )],
        vec![done(vec![Content::text("done")], StopReason::Stop)],
    ]));

    let cfg = base_config(provider.clone())
        .with_tools(vec![Arc::new(GatedTool { ran: ran.clone() })])
        .with_permission(policy);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let run = ocean_runtime::run_agent(&cfg, user("call gated once"), Some(tx))
        .await
        .expect("Allow run must complete cleanly");

    assert_eq!(
        checks.load(Ordering::SeqCst),
        1,
        "the Allow gate must be consulted once"
    );
    assert_eq!(
        ran.load(Ordering::SeqCst),
        1,
        "an allowed gated tool must run"
    );
    assert_no_orphan_tool_use(&run.messages);

    let events = collect_events(&mut rx);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolExecutionStart { tool_call_id, tool_name, .. }
                if tool_call_id == "call-ok" && tool_name == "gated"
        )),
        "an allowed gated tool must emit ToolExecutionStart"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolExecutionEnd { tool_name, is_error, .. }
                if tool_name == "gated" && !is_error
        )),
        "an allowed gated tool must emit a non-error ToolExecutionEnd"
    );
}

// ===========================================================================
// Scenario 9 — Manual policy broadens approval to otherwise-safe tools.
// ===========================================================================
#[tokio::test]
async fn manual_policy_can_check_every_tool_call() {
    let ran = Arc::new(AtomicUsize::new(0));
    let (policy, checks) = ScriptedPolicy::always_check(PermissionDecision::Allow);
    let provider = Arc::new(MockProvider::new(vec![
        vec![done(
            vec![tool_call("call-safe", "echo", serde_json::json!({}))],
            StopReason::ToolUse,
        )],
        vec![done(vec![Content::text("done")], StopReason::Stop)],
    ]));

    let cfg = base_config(provider)
        .with_tools(vec![Arc::new(EchoTool { ran: ran.clone() })])
        .with_permission(policy);

    ocean_runtime::run_agent(&cfg, user("call the safe tool"), None)
        .await
        .expect("manual approval of a safe tool must complete");

    assert_eq!(checks.load(Ordering::SeqCst), 1);
    assert_eq!(ran.load(Ordering::SeqCst), 1);
}
