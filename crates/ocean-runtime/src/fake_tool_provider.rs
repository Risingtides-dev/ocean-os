//! `FakeToolProvider` — a keyless, deterministic [`Provider`] that drives the
//! real agent loop through exactly one tool call, so the permission gate's
//! *release* path can be live-tested over HTTP with no external LLM key
//! (OCEAN-130).
//!
//! Motivation: the permission gate's *blocking* half is unit-tested, but the
//! *release* half — `permission_decision` → oneshot waiter → the suspended turn
//! resumes → the tool actually runs — has no live coverage, because no in-repo
//! provider deterministically emits a tool call. `fake-ok` only echoes text; the
//! only real key returns 401. This provider closes that gap: select it with
//! `OCEAN_MODEL=fake-tool` and the daemon will, on the first round of the turn,
//! emit one `write` tool call (a built-in, permission-gated tool). With gating
//! on, that trips the gate exactly like a real provider's tool_use would; once
//! the operator allows it, the loop runs the tool and the provider's second
//! round returns a plain `done` completion.
//!
//! It is injected via [`AgentConfig::with_provider`] — the same seam the e2e
//! tests use — so it drives the genuine loop (permission check, tool execution,
//! event emission) rather than a synthesized shortcut.

use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::stream;
use ocean_protocol::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context, Model,
    Provider, Result, StopReason, StreamOptions, Usage,
};
use serde_json::json;

/// The model id that selects this provider. Mirrors the alias added to
/// `ocean-providers` so the two stay in lockstep.
pub const FAKE_TOOL_MODEL: &str = "fake-tool";

/// Fixed, harmless target the scripted `write` tool call writes to. Deterministic
/// and side-effect-minimal — a single small file under the OS temp dir.
pub const FAKE_TOOL_TARGET_PATH: &str = "/tmp/ocean-fake-tool-test.txt";

/// Fixed contents written by the scripted tool call.
pub const FAKE_TOOL_CONTENT: &str = "ok";

/// Stable id stamped on the scripted tool call, so its paired tool_result is
/// easy to spot in a transcript / SSE trace.
pub const FAKE_TOOL_CALL_ID: &str = "fake-tool-call-1";

/// A keyless provider that scripts a two-round agent loop:
///
/// 1. **Round 1** — emit one `write` tool call (`path`, `content` fixed). This
///    is a built-in, permission-gated tool, so with gating on the loop suspends
///    here on the permission gate.
/// 2. **Round 2** — after the tool result comes back, emit a plain text `done`
///    completion that terminates the loop.
///
/// Requires no API key and performs no network I/O — the whole point is to
/// exercise the block→decide→proceed cycle deterministically and offline.
pub struct FakeToolProvider {
    calls: AtomicUsize,
}

impl FakeToolProvider {
    pub fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }
}

impl Default for FakeToolProvider {
    fn default() -> Self {
        Self::new()
    }
}

fn fake_assistant_message(content: Vec<Content>, stop: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "fake".into(),
        provider: "fake".into(),
        model: FAKE_TOOL_MODEL.into(),
        usage: Usage::default(),
        stop_reason: stop,
        error_message: None,
        timestamp: ocean_protocol::now_ms(),
    }
}

#[async_trait]
impl Provider for FakeToolProvider {
    async fn stream(
        &self,
        _model: &Model,
        _context: &Context,
        _options: &StreamOptions,
    ) -> Result<AssistantMessageEventStream> {
        // Round number is derived from how many times the loop has streamed us.
        // The agent loop calls `stream` once per round, so the first call is the
        // tool-call round and the second is the completion round. Any further
        // call (shouldn't happen — the second round terminates) also returns a
        // clean completion so the loop can never hang on us.
        let round = self.calls.fetch_add(1, Ordering::SeqCst);

        let events: Vec<AssistantMessageEvent> = if round == 0 {
            // Round 1: one deterministic, permission-gated `write` tool call.
            let call = Content::ToolCall {
                id: FAKE_TOOL_CALL_ID.into(),
                name: "write".into(),
                arguments: json!({
                    "path": FAKE_TOOL_TARGET_PATH,
                    "content": FAKE_TOOL_CONTENT,
                }),
            };
            vec![AssistantMessageEvent::Done {
                reason: StopReason::ToolUse,
                message: fake_assistant_message(vec![call], StopReason::ToolUse),
            }]
        } else {
            // Round 2+: stream a short text delta then a terminal completion, so
            // the loop's TextDelta path runs (mirroring a real provider) and the
            // turn finishes cleanly with stop reason `Stop`.
            vec![
                AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: "done".into(),
                },
                AssistantMessageEvent::Done {
                    reason: StopReason::Stop,
                    message: fake_assistant_message(vec![Content::text("done")], StopReason::Stop),
                },
            ]
        };

        let events: Vec<Result<AssistantMessageEvent>> = events.into_iter().map(Ok).collect();
        Ok(Box::pin(stream::iter(events)))
    }
}
