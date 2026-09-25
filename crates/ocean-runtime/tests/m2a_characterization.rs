//! Minimizer M2a — characterization of current raw behavior.
//!
//! These tests freeze the exact pre-M2 contracts that
//! `docs/specs/2026-07-16-ocean-minimizer-command-capture-runtime-integration-design.md`
//! (§6 M2a) requires before any command-output minimization is wired:
//!
//! - the exact `BashTool` stdout/stderr/cap/exit text envelope;
//! - the legacy `command`-only Bash schema and argument handling;
//! - the spill threshold boundary, exact raw artifact round-trip, and
//!   artifact-read bypass;
//! - decorator forwarding, including the currently MISSING `concurrency`
//!   forwarding (pinned as-is; repairing it changes live scheduling and needs
//!   separate review);
//! - one inner execution per call behind the permission gate, with live
//!   `ToolExecutionEnd`, checkpoints, and the final transcript all carrying the
//!   same raw tool text;
//! - direct `SessionContext` defaults.
//!
//! Timeout/Halt process-tree behavior is characterized by the existing
//! `tools_smoke` `bash_timeout_*` / `bash_halt_*` tests; full-live versus capped
//! transcript content by `runtime_event_queue_retains_full_tool_payload_until_drained`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use ocean_protocol::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context,
    Message, Model, Provider, StopReason, StreamOptions, Usage,
};
use ocean_runtime::capability::{
    BuiltinProvider, CapabilityProvider, CapabilityRegistry, SessionContext, SpillingTool,
    SPILL_THRESHOLD_BYTES,
};
use ocean_runtime::tools::bash::BashTool;
use ocean_runtime::types::{
    AgentConfig, AgentEvent, AgentTool, AgentToolResult, Concurrency, PermissionDecision,
    PermissionPolicy,
};
use serde_json::{json, Value};
use tokio::sync::mpsc;

fn text_of(result: &AgentToolResult) -> &str {
    assert_eq!(result.content.len(), 1, "bash returns exactly one block");
    result.content[0].as_text().expect("bash block is text")
}

async fn bash(command: &str) -> AgentToolResult {
    BashTool::for_cwd(std::env::temp_dir())
        .execute("c", json!({ "command": command }))
        .await
        .expect("bash command completes")
}

// ---------------------------------------------------------------------------
// Exact Bash envelope
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bash_envelope_is_frozen_for_every_stream_shape() {
    let cases: &[(&str, &str)] = &[
        // stdout with and without a trailing newline
        ("printf 'a\\nb\\n'", "a\nb\n\n[exit 0]"),
        ("printf a", "a\n[exit 0]"),
        // stderr only: generated header, no leading newline
        ("printf e >&2", "[stderr]\ne\n[exit 0]"),
        // stdout first, then stderr, then the exit suffix
        ("printf o; printf e >&2; exit 3", "o\n[stderr]\ne\n[exit 3]"),
        (
            "printf 'o\\n'; printf 'e\\n' >&2",
            "o\n[stderr]\ne\n\n[exit 0]",
        ),
        // no output at all
        ("true", "\n[exit 0]"),
        // signal death has no exit code and renders as -1
        ("kill -9 $$", "\n[exit -1]"),
        // invalid UTF-8 is decoded lossily
        ("printf '\\377'", "\u{FFFD}\n[exit 0]"),
    ];
    for (command, expected) in cases {
        let result = bash(command).await;
        assert_eq!(text_of(&result), *expected, "envelope for {command:?}");
        assert_eq!(result.details, Value::Null, "bash details stay Null");
        assert!(!result.terminate);
        assert!(result.side_effects.is_empty());
    }
}

#[tokio::test]
async fn bash_capture_cap_markers_are_frozen() {
    let result = BashTool::new()
        .execute(
            "c",
            json!({
                "command": "head -c 2097160 /dev/zero | tr '\\0' 'x'; head -c 2097160 /dev/zero | tr '\\0' 'y' >&2",
                "timeout_ms": 60000
            }),
        )
        .await
        .expect("flood completes");
    let text = text_of(&result);
    let stdout_marker = "\n[stdout capped at 2MiB; the command ran to completion]";
    let stderr_marker = "\n[stderr capped at 2MiB; the command ran to completion]";
    let expected = format!(
        "{}{stdout_marker}\n[stderr]\n{}{stderr_marker}\n[exit 0]",
        "x".repeat(2 * 1024 * 1024),
        "y".repeat(2 * 1024 * 1024),
    );
    assert_eq!(text.len(), expected.len());
    assert!(text == expected, "capped envelope drifted");
}

#[tokio::test]
async fn bash_timeout_error_text_is_frozen() {
    let error = BashTool::new()
        .execute("c", json!({ "command": "sleep 5", "timeout_ms": 200 }))
        .await
        .expect_err("times out");
    assert_eq!(error, "command timed out after 200ms");
}

// ---------------------------------------------------------------------------
// Legacy Bash schema and argument handling
// ---------------------------------------------------------------------------

#[test]
fn legacy_bash_schema_and_identity_are_frozen() {
    let tool = BashTool::new();
    assert_eq!(tool.name(), "bash");
    assert_eq!(tool.label(), "bash");
    assert!(tool.requires_permission());
    assert_eq!(tool.concurrency(), Concurrency::Exclusive);
    assert_eq!(
        tool.description(),
        "Run a shell command via `bash -lc <cmd>`. Returns combined stdout/stderr and exit code."
    );
    assert_eq!(
        tool.parameters(),
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "timeout_ms": {"type": "integer", "default": 120000}
            },
            "required": ["command"]
        })
    );
}

#[tokio::test]
async fn legacy_bash_requires_command_and_ignores_unknown_argv() {
    let dir = std::env::temp_dir().join(format!("ocean-m2a-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("argv-must-not-run");
    let _ = std::fs::remove_file(&marker);
    let tool = BashTool::for_cwd(dir.clone());

    let error = tool
        .execute("c", json!({ "argv": ["touch", marker.to_string_lossy()] }))
        .await
        .expect_err("argv alone is not a legacy command");
    assert_eq!(error, "missing 'command'");
    assert!(!marker.exists(), "legacy mode never executes argv");

    let both = tool
        .execute(
            "c",
            json!({ "command": "printf legacy", "argv": ["touch", marker.to_string_lossy()] }),
        )
        .await
        .expect("legacy command wins");
    assert_eq!(text_of(&both), "legacy\n[exit 0]");
    assert!(!marker.exists(), "legacy mode ignores argv entirely");
}

// ---------------------------------------------------------------------------
// Spill threshold, exact artifact, read bypass, decorator forwarding
// ---------------------------------------------------------------------------

struct FixedTool {
    name: &'static str,
    payload: String,
    concurrency: Concurrency,
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentTool for FixedTool {
    fn name(&self) -> &str {
        self.name
    }
    fn label(&self) -> &str {
        "fixed-label"
    }
    fn description(&self) -> &str {
        "fixed-description"
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {"k": {"type": "string"}}})
    }
    fn requires_permission(&self) -> bool {
        true
    }
    fn concurrency(&self) -> Concurrency {
        self.concurrency
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok(AgentToolResult::text(self.payload.clone()))
    }
}

fn fixed(payload: String, concurrency: Concurrency) -> (Arc<FixedTool>, Arc<AtomicUsize>) {
    let executions = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(FixedTool {
            name: "fixed",
            payload,
            concurrency,
            executions: executions.clone(),
        }),
        executions,
    )
}

#[tokio::test]
async fn spill_threshold_boundary_and_exact_artifact_round_trip() {
    let store = ocean_runtime::artifacts::new_shared();

    let at_threshold = "z".repeat(SPILL_THRESHOLD_BYTES);
    let (inner, executions) = fixed(at_threshold.clone(), Concurrency::Exclusive);
    let out = SpillingTool::new(inner, store.clone())
        .execute("1", json!({}))
        .await
        .unwrap();
    assert_eq!(out.content[0].as_text(), Some(at_threshold.as_str()));
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(store.lock().unwrap().is_empty(), "no artifact at threshold");

    let over: String = (0..3_000).map(|i| format!("line {i:05}\n")).collect();
    assert!(over.len() > SPILL_THRESHOLD_BYTES);
    let (inner, executions) = fixed(over.clone(), Concurrency::Exclusive);
    let out = SpillingTool::new(inner, store.clone())
        .execute("2", json!({}))
        .await
        .unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1, "inner runs once");
    let shown = out.content[0].as_text().unwrap();
    assert!(
        shown.ends_with("· full output: read artifact://a1]"),
        "{shown:.80}"
    );
    assert_eq!(
        store.lock().unwrap().get("a1").map(|a| a.text.clone()),
        Some(over),
        "artifact holds the exact raw text"
    );
}

#[tokio::test]
async fn artifact_read_bypasses_respill() {
    let big: String = "q".repeat(SPILL_THRESHOLD_BYTES * 2);
    let store = ocean_runtime::artifacts::new_shared();
    store.lock().unwrap().put("bash", big.clone());
    let read = ocean_runtime::tools::read::ReadTool::for_cwd(std::env::temp_dir())
        .with_artifacts(store.clone());
    let out = SpillingTool::new(Arc::new(read), store.clone())
        .execute("r", json!({ "path": "artifact://a1", "limit": 100000 }))
        .await
        .expect("artifact read");
    assert!(
        !out.content[0]
            .as_text()
            .unwrap()
            .contains("read artifact://a2"),
        "an artifact read is never re-spilled"
    );
    assert_eq!(store.lock().unwrap().len(), 1, "no nested artifact");
}

#[test]
fn spilling_decorator_forwarding_is_frozen_including_missing_concurrency() {
    let store = ocean_runtime::artifacts::new_shared();
    let (inner, _) = fixed(String::new(), Concurrency::Shared);
    let wrapped = SpillingTool::new(inner.clone(), store);
    assert_eq!(wrapped.name(), inner.name());
    assert_eq!(wrapped.label(), inner.label());
    assert_eq!(wrapped.description(), inner.description());
    assert_eq!(wrapped.parameters(), inner.parameters());
    assert_eq!(wrapped.requires_permission(), inner.requires_permission());
    // CHARACTERIZATION: SpillingTool does not forward `concurrency()`, so a
    // Shared inner tool is scheduled as Exclusive whenever artifact spill is
    // enabled. The M2 design (§3.6) calls for repairing this, but the repair
    // changes live batch scheduling for every artifact-enabled daemon turn and
    // is held for separate review. Update this pin only with that repair.
    assert_eq!(inner.concurrency(), Concurrency::Shared);
    assert_eq!(wrapped.concurrency(), Concurrency::Exclusive);
}

#[test]
fn direct_session_context_defaults_are_frozen() {
    let ctx = SessionContext::default();
    assert!(!ctx.hashline, "hashline is daemon-granted");
    assert!(!ctx.artifacts, "artifact spill is daemon-granted");
    assert!(ctx.code_intelligence, "lsp stays on for direct callers");
    assert!(ctx.session_id.is_none());
}

#[tokio::test]
async fn registry_bash_under_spill_profile_keeps_legacy_schema() {
    let registry = CapabilityRegistry::new(vec![Arc::new(BuiltinProvider::new())]);
    let ctx = SessionContext {
        cwd: std::env::temp_dir(),
        session_id: Some("m2a".into()),
        artifacts: true,
        ..SessionContext::default()
    };
    let tools = registry.tools_for_session(&ctx).await;
    let bash = tools.iter().find(|t| t.name() == "bash").unwrap();
    assert_eq!(bash.parameters(), BashTool::new().parameters());
    let out = bash
        .execute("c", json!({ "command": "printf spill-on" }))
        .await
        .unwrap();
    assert_eq!(text_of(&out), "spill-on\n[exit 0]");
    // Keep the provider trait in scope for the reader: every provider shares the
    // one built-in store.
    let _ = BuiltinProvider::new().artifacts_store("m2a");
}

// ---------------------------------------------------------------------------
// Loop: permission gate, one execution, identical raw live/checkpoint/transcript
// ---------------------------------------------------------------------------

struct Scripted {
    turns: Mutex<std::collections::VecDeque<Vec<AssistantMessageEvent>>>,
    contexts: Mutex<Vec<Context>>,
}

#[async_trait]
impl Provider for Scripted {
    async fn stream(
        &self,
        _model: &Model,
        context: &Context,
        _options: &StreamOptions,
    ) -> ocean_protocol::Result<AssistantMessageEventStream> {
        self.contexts.lock().unwrap().push(context.clone());
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted turn");
        Ok(Box::pin(stream::iter(turn.into_iter().map(Ok))))
    }
}

fn done(content: Vec<Content>, stop: StopReason) -> AssistantMessageEvent {
    AssistantMessageEvent::Done {
        reason: stop,
        message: AssistantMessage {
            content,
            api: "mock".into(),
            provider: "mock".into(),
            model: "mock".into(),
            usage: Usage::default(),
            stop_reason: stop,
            error_message: None,
            timestamp: 0,
        },
    }
}

struct DenyFirst {
    checks: AtomicUsize,
}

#[async_trait]
impl PermissionPolicy for DenyFirst {
    async fn check(&self, _tool: &str, _args: &Value) -> PermissionDecision {
        if self.checks.fetch_add(1, Ordering::SeqCst) == 0 {
            PermissionDecision::Deny {
                reason: "no".into(),
            }
        } else {
            PermissionDecision::Allow
        }
    }
}

#[tokio::test]
async fn permission_gate_precedes_one_execution_and_raw_text_is_everywhere() {
    let store = ocean_runtime::artifacts::new_shared();
    let (inner, executions) = fixed("raw tool text\n[exit 0]".into(), Concurrency::Exclusive);
    let wrapped: Arc<dyn AgentTool> = Arc::new(SpillingTool::new(inner, store));
    let provider = Arc::new(Scripted {
        turns: Mutex::new(
            vec![
                vec![done(
                    vec![
                        Content::ToolCall {
                            id: "call_1".into(),
                            name: "fixed".into(),
                            arguments: json!({}),
                        },
                        Content::ToolCall {
                            id: "call_2".into(),
                            name: "fixed".into(),
                            arguments: json!({}),
                        },
                    ],
                    StopReason::ToolUse,
                )],
                vec![done(vec![Content::text("done")], StopReason::Stop)],
            ]
            .into(),
        ),
        contexts: Mutex::new(Vec::new()),
    });
    let config = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "sys")
        .with_provider(provider.clone())
        .with_tools(vec![wrapped])
        .with_permission(Arc::new(DenyFirst {
            checks: AtomicUsize::new(0),
        }));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let run = ocean_runtime::run_agent(&config, Message::user_text("go"), Some(tx))
        .await
        .expect("run");

    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the denied call never executes; the allowed call executes once"
    );

    let mut live = Vec::new();
    let mut checkpointed = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::ToolExecutionEnd { content, .. } => live.push(content),
            AgentEvent::TurnCheckpoint { messages, .. } => checkpointed.extend(messages),
            _ => {}
        }
    }
    let raw = vec![Content::text("raw tool text\n[exit 0]")];
    assert_eq!(live, vec![raw.clone()], "live end carries raw text");

    let results = |messages: &[Message]| -> Vec<(String, Vec<Content>)> {
        messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult(r) => Some((r.tool_call_id.clone(), r.content.clone())),
                _ => None,
            })
            .collect()
    };
    let expected = vec![
        (
            "call_1".to_string(),
            vec![Content::text("permission denied: no")],
        ),
        ("call_2".to_string(), raw.clone()),
    ];
    assert_eq!(results(&run.messages), expected);
    assert_eq!(results(&checkpointed), expected);
    let second_request = &provider.contexts.lock().unwrap()[1];
    assert_eq!(
        results(&second_request.messages),
        expected,
        "provider request carries the same raw text"
    );
}
