//! Parallel tool-execution scheduler tests.
//!
//! The agent loop executes a batch of tool calls by walking it in order and
//! running maximal runs of consecutive [`Concurrency::Shared`] tools
//! *concurrently*, while a [`Concurrency::Exclusive`] tool is a full barrier:
//! everything before it finishes, it runs alone, everything after waits. These
//! tests drive the real loop through a scripted provider that emits a *batch* of
//! tool calls in one assistant message and assert:
//!
//! 1. a batch of `Shared` tools genuinely overlaps (observed concurrency ≥ 2),
//! 2. an `Exclusive` tool never runs concurrently with any peer (barrier),
//! 3. the persisted transcript keeps the ORIGINAL batch order regardless of
//!    which tool finished first,
//! 4. the default is `Exclusive` — a tool that doesn't opt in never parallelizes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream;
use ocean_protocol::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context,
    Message, Model, Provider, StopReason, StreamOptions, Usage,
};
use ocean_runtime::types::AgentEvent;
use ocean_runtime::types::{AgentConfig, AgentTool, AgentToolResult, Concurrency};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Scripted provider — round 1 emits a batch of tool calls, round 2 wraps up.
// ---------------------------------------------------------------------------

type Turn = Vec<AssistantMessageEvent>;

struct MockProvider {
    turns: std::sync::Mutex<std::collections::VecDeque<Turn>>,
}

impl MockProvider {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns: std::sync::Mutex::new(turns.into()),
        }
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
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .expect("MockProvider ran out of scripted turns");
        let events: Vec<ocean_protocol::Result<AssistantMessageEvent>> =
            turn.into_iter().map(Ok).collect();
        Ok(Box::pin(stream::iter(events)))
    }
}

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

fn tool_call(id: &str, name: &str, args: Value) -> Content {
    Content::ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
    }
}

fn user(s: &str) -> Message {
    Message::user_text(s)
}

fn base_config(provider: Arc<MockProvider>) -> AgentConfig {
    AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test system")
        .with_session_id("parallel")
        .with_provider(provider)
        .with_max_turns(4)
}

// ---------------------------------------------------------------------------
// A tool that tracks live concurrency. On entry it bumps a shared counter and
// records the peak; it sleeps for `args.sleep_ms` (so a batch can overlap in
// wall-clock time), then decrements. `concurrency()` is whatever the tool was
// built with — so the same tool body models both a Shared read and an Exclusive
// write. It echoes back a stable `result-<tag>` from `args.tag` so the test can
// assert transcript order independent of finish order.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Tracker {
    current: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    /// Peak observed *while an Exclusive tool was running*. Must stay 1.
    exclusive_peak: Arc<AtomicUsize>,
}

impl Tracker {
    fn new() -> Self {
        Self {
            current: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            exclusive_peak: Arc::new(AtomicUsize::new(0)),
        }
    }
}

struct TrackedTool {
    name: String,
    concurrency: Concurrency,
    tracker: Tracker,
}

#[async_trait]
impl AgentTool for TrackedTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        "records live concurrency while sleeping"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn requires_permission(&self) -> bool {
        false
    }
    fn concurrency(&self) -> Concurrency {
        self.concurrency
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let now = self.tracker.current.fetch_add(1, Ordering::SeqCst) + 1;
        // Record global peak.
        self.tracker.peak.fetch_max(now, Ordering::SeqCst);
        // If this is the exclusive tool, record how many peers it saw (must be 1).
        if self.concurrency == Concurrency::Exclusive {
            self.tracker.exclusive_peak.fetch_max(now, Ordering::SeqCst);
        }
        let sleep_ms = args.get("sleep_ms").and_then(|v| v.as_u64()).unwrap_or(100);
        tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
        self.tracker.current.fetch_sub(1, Ordering::SeqCst);
        let tag = args
            .get("tag")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        Ok(AgentToolResult::text(format!("result-{tag}")))
    }
}

/// Pull the tool_result texts out of the transcript, in transcript order.
fn result_texts(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::ToolResult(tr) => Some(
                tr.content
                    .iter()
                    .filter_map(|c| c.as_text())
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect()
}

// ===========================================================================
// 1 — A batch of Shared tools genuinely overlaps.
// Three `read`-like Shared calls, each sleeping 200ms. Sequential execution
// would take ≥600ms; concurrent execution ~200ms. We assert both the observed
// peak concurrency ≥ 2 (the robust signal) AND a generous wall-clock bound.
// ===========================================================================
#[tokio::test]
async fn shared_tools_in_one_batch_run_concurrently() {
    let tracker = Tracker::new();
    let provider = Arc::new(MockProvider::new(vec![
        vec![done(
            vec![
                tool_call(
                    "c0",
                    "sread",
                    serde_json::json!({ "sleep_ms": 200, "tag": "0" }),
                ),
                tool_call(
                    "c1",
                    "sread",
                    serde_json::json!({ "sleep_ms": 200, "tag": "1" }),
                ),
                tool_call(
                    "c2",
                    "sread",
                    serde_json::json!({ "sleep_ms": 200, "tag": "2" }),
                ),
            ],
            StopReason::ToolUse,
        )],
        vec![done(vec![Content::text("done")], StopReason::Stop)],
    ]));

    let cfg = base_config(provider).with_tools(vec![Arc::new(TrackedTool {
        name: "sread".into(),
        concurrency: Concurrency::Shared,
        tracker: tracker.clone(),
    })]);

    let start = Instant::now();
    let run = ocean_runtime::run_agent(&cfg, user("read three files"), None)
        .await
        .expect("run completes");
    let elapsed = start.elapsed();

    assert!(
        tracker.peak.load(Ordering::SeqCst) >= 2,
        "shared tools must overlap — peak concurrency was {}",
        tracker.peak.load(Ordering::SeqCst)
    );
    // 3 × 200ms sequential = 600ms; concurrent ≈ 200ms. 500ms is a safe ceiling
    // that a sequential loop could never meet but a slow CI concurrent run will.
    assert!(
        elapsed < Duration::from_millis(500),
        "three concurrent 200ms reads should finish well under 600ms, took {elapsed:?}"
    );
    // All three results present, in call order.
    assert_eq!(
        result_texts(&run.messages),
        vec!["result-0", "result-1", "result-2"]
    );
}

// ===========================================================================
// 2 — An Exclusive tool is a full barrier: [shared, shared, EXCLUSIVE, shared].
// The exclusive `write` must never run alongside a peer. We assert the peak
// concurrency observed *while the exclusive tool ran* stayed 1, while the global
// peak (from the two leading shared reads) reached ≥ 2.
// ===========================================================================
#[tokio::test]
async fn exclusive_tool_is_a_barrier_never_runs_with_a_peer() {
    let tracker = Tracker::new();
    let provider = Arc::new(MockProvider::new(vec![
        vec![done(
            vec![
                tool_call(
                    "c0",
                    "sread",
                    serde_json::json!({ "sleep_ms": 150, "tag": "0" }),
                ),
                tool_call(
                    "c1",
                    "sread",
                    serde_json::json!({ "sleep_ms": 150, "tag": "1" }),
                ),
                tool_call(
                    "c2",
                    "xwrite",
                    serde_json::json!({ "sleep_ms": 100, "tag": "2" }),
                ),
                tool_call(
                    "c3",
                    "sread",
                    serde_json::json!({ "sleep_ms": 150, "tag": "3" }),
                ),
            ],
            StopReason::ToolUse,
        )],
        vec![done(vec![Content::text("done")], StopReason::Stop)],
    ]));

    let cfg = base_config(provider).with_tools(vec![
        Arc::new(TrackedTool {
            name: "sread".into(),
            concurrency: Concurrency::Shared,
            tracker: tracker.clone(),
        }),
        Arc::new(TrackedTool {
            name: "xwrite".into(),
            concurrency: Concurrency::Exclusive,
            tracker: tracker.clone(),
        }),
    ]);

    let run = ocean_runtime::run_agent(&cfg, user("read, write, read"), None)
        .await
        .expect("run completes");

    // The two leading shared reads overlapped.
    assert!(
        tracker.peak.load(Ordering::SeqCst) >= 2,
        "the two leading shared reads should overlap (peak ≥ 2), got {}",
        tracker.peak.load(Ordering::SeqCst)
    );
    // The exclusive write ran alone — it never saw a concurrent peer.
    assert_eq!(
        tracker.exclusive_peak.load(Ordering::SeqCst),
        1,
        "the exclusive tool must run alone; it observed {} concurrent tools",
        tracker.exclusive_peak.load(Ordering::SeqCst)
    );
    // Transcript keeps original batch order.
    assert_eq!(
        result_texts(&run.messages),
        vec!["result-0", "result-1", "result-2", "result-3"]
    );
}

// ===========================================================================
// 3 — Transcript order is the ORIGINAL batch order, not the finish order.
// Three shared reads where the FIRST sleeps longest, so completion order is the
// reverse of call order. The persisted tool_result sequence must still be
// result-0, result-1, result-2 — deterministic and provider-valid.
// ===========================================================================
#[tokio::test]
async fn transcript_order_follows_call_order_not_finish_order() {
    let tracker = Tracker::new();
    let provider = Arc::new(MockProvider::new(vec![
        vec![done(
            vec![
                // Reverse-staggered sleeps: c0 finishes LAST.
                tool_call(
                    "c0",
                    "sread",
                    serde_json::json!({ "sleep_ms": 250, "tag": "0" }),
                ),
                tool_call(
                    "c1",
                    "sread",
                    serde_json::json!({ "sleep_ms": 150, "tag": "1" }),
                ),
                tool_call(
                    "c2",
                    "sread",
                    serde_json::json!({ "sleep_ms": 50, "tag": "2" }),
                ),
            ],
            StopReason::ToolUse,
        )],
        vec![done(vec![Content::text("done")], StopReason::Stop)],
    ]));

    let cfg = base_config(provider).with_tools(vec![Arc::new(TrackedTool {
        name: "sread".into(),
        concurrency: Concurrency::Shared,
        tracker: tracker.clone(),
    })]);

    let run = ocean_runtime::run_agent(&cfg, user("staggered reads"), None)
        .await
        .expect("run completes");

    assert_eq!(
        result_texts(&run.messages),
        vec!["result-0", "result-1", "result-2"],
        "results must be in call order even though c2 finished first and c0 last"
    );
}

// ===========================================================================
// 4 — Default concurrency is Exclusive: a tool that does NOT override
// `concurrency()` never parallelizes, even in a batch of identical calls.
// ===========================================================================
#[tokio::test]
async fn default_tools_do_not_parallelize() {
    // A tool that leaves `concurrency()` at its trait default (Exclusive).
    struct DefaultTool {
        tracker: Tracker,
    }
    #[async_trait]
    impl AgentTool for DefaultTool {
        fn name(&self) -> &str {
            "plain"
        }
        fn description(&self) -> &str {
            "uses the default (Exclusive) concurrency"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
            let now = self.tracker.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.tracker.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.tracker.current.fetch_sub(1, Ordering::SeqCst);
            Ok(AgentToolResult::text("ok"))
        }
    }

    let tracker = Tracker::new();
    let provider = Arc::new(MockProvider::new(vec![
        vec![done(
            vec![
                tool_call("c0", "plain", serde_json::json!({})),
                tool_call("c1", "plain", serde_json::json!({})),
                tool_call("c2", "plain", serde_json::json!({})),
            ],
            StopReason::ToolUse,
        )],
        vec![done(vec![Content::text("done")], StopReason::Stop)],
    ]));

    let cfg = base_config(provider).with_tools(vec![Arc::new(DefaultTool {
        tracker: tracker.clone(),
    })]);

    ocean_runtime::run_agent(&cfg, user("three default calls"), None)
        .await
        .expect("run completes");

    assert_eq!(
        tracker.peak.load(Ordering::SeqCst),
        1,
        "tools at the default (Exclusive) concurrency must never overlap — peak was {}",
        tracker.peak.load(Ordering::SeqCst)
    );
}

// ===========================================================================
// 5 — LIVE End emission: a Shared batch member's ToolExecutionEnd must be
// emitted the moment IT completes, not after the whole segment drains. The
// slow member is gated on a Notify (no timing race): the fast member's End
// must arrive while the slow one is still gated; releasing the gate then
// completes the run with the transcript in ORIGINAL call order.
// ===========================================================================

struct GatedTool {
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl AgentTool for GatedTool {
    fn name(&self) -> &str {
        "gated"
    }
    fn description(&self) -> &str {
        "fast returns immediately; slow waits on the test's gate"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn requires_permission(&self) -> bool {
        false
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let tag = args
            .get("tag")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        if tag == "slow" {
            self.release.notified().await;
        }
        Ok(AgentToolResult::text(format!("result-{tag}")))
    }
}

#[tokio::test]
async fn shared_batch_emits_each_end_as_it_completes() {
    let release = Arc::new(tokio::sync::Notify::new());
    let provider = Arc::new(MockProvider::new(vec![
        vec![done(
            vec![
                tool_call("c0", "gated", serde_json::json!({ "tag": "slow" })),
                tool_call("c1", "gated", serde_json::json!({ "tag": "fast" })),
            ],
            StopReason::ToolUse,
        )],
        vec![done(vec![Content::text("done")], StopReason::Stop)],
    ]));
    let cfg = base_config(provider).with_tools(vec![Arc::new(GatedTool {
        release: release.clone(),
    })]);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let handle =
        tokio::spawn(async move { ocean_runtime::run_agent(&cfg, user("go"), Some(tx)).await });

    // The fast member's End must arrive while the slow member is still gated.
    let mut saw_fast_end = false;
    let waited = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(ev) = rx.recv().await {
            if let AgentEvent::ToolExecutionEnd { tool_call_id, .. } = &ev {
                assert_ne!(
                    tool_call_id, "c0",
                    "slow (gated) member must not End before its gate releases"
                );
                if tool_call_id == "c1" {
                    saw_fast_end = true;
                    break;
                }
            }
        }
    })
    .await;
    assert!(
        waited.is_ok() && saw_fast_end,
        "fast member's ToolExecutionEnd must be emitted while the slow member is still running \
         (a join_all barrier holds every End hostage to the slowest tool in the batch)"
    );

    release.notify_one();
    let run = handle.await.expect("join").expect("run completes");
    // Transcript stays in ORIGINAL call order regardless of finish order.
    assert_eq!(
        result_texts(&run.messages),
        vec!["result-slow", "result-fast"]
    );
}
