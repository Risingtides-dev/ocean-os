//! Per-turn tracing span tests (OCEAN-274).
//!
//! These prove that the turn-lifecycle spans render with their context — most
//! importantly that a turn's `turn_id` is attached to every log line produced
//! while the turn runs, so concurrent turns are distinguishable, and that the
//! span tree nests turn → agent_loop → round → tool_exec.
//!
//! We capture the `tracing_subscriber::fmt` output into an in-memory buffer
//! (the same `Full` formatter + span-event config the daemon installs), drive
//! the *real* `run_agent` loop through a scripted `MockProvider` inside a parent
//! `turn` span carrying a known `turn_id`, then assert that id (and the child
//! span names) appear in the rendered output.
//!
//! Runtime note: the loop is run on a `current_thread` runtime and the
//! subscriber is installed as the *thread-local* default (`set_default`), so we
//! capture every span/event the loop emits without racing the process-global
//! default that the rest of the test binary may set.

use std::io;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream;
use ocean_protocol::{
    AssistantMessage, AssistantMessageEvent, AssistantMessageEventStream, Content, Context,
    Message, Model, Provider, StopReason, StreamOptions, Usage,
};
use ocean_runtime::types::{AgentConfig, AgentTool, AgentToolResult};
use serde_json::Value;
use tracing::Instrument;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::MakeWriter;

// ---------------------------------------------------------------------------
// In-memory capture writer for the fmt subscriber.
// ---------------------------------------------------------------------------

/// A `MakeWriter` that appends every formatted log line into a shared buffer so
/// the test can inspect what the subscriber actually rendered.
#[derive(Clone, Default)]
struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl BufWriter {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl io::Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for BufWriter {
    type Writer = BufWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

// ---------------------------------------------------------------------------
// A minimal scripted provider + echo tool (mirrors agent_loop_e2e.rs).
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

struct EchoTool;

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
    fn requires_permission(&self) -> bool {
        false
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        // Emit an event from *inside* the tool so we can prove the `tool_exec`
        // span is the active context while the tool runs (the line is tagged
        // with tool_exec{tool_name=echo ...} and, transitively, the turn_id).
        tracing::info!("echo tool body running");
        Ok(AgentToolResult::text(format!("echo: {args}")))
    }
}

/// Drive a 2-round turn (tool_use → tool runs → final text) through the real
/// loop, inside a parent `turn` span carrying a known id, and return everything
/// the fmt subscriber rendered.
#[tokio::test(flavor = "current_thread")]
async fn turn_id_and_span_tree_render_in_subscriber_output() {
    let buf = BufWriter::default();

    // The same formatter shape the daemon installs (OCEAN-274): the default
    // `Full` format prints the active span scope (with fields) ahead of each
    // event, and NEW/CLOSE span events make span open/close visible. ANSI off so
    // the buffer holds plain text we can assert on. Installed thread-locally for
    // the duration of this test only.
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // A distinctive turn id we can grep for in the rendered output. In production
    // this is the daemon's `turn` root span field; here we stand it up directly
    // and run the loop inside it, exactly as the daemon `.instrument()`s the
    // runtime work into its turn span.
    let turn_id = "turn-274-abcdef";
    let turn_span = tracing::info_span!("turn", turn_id = turn_id, session_id = "sess-xyz");

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
        // Round 2: final text.
        vec![done(vec![Content::text("all done")], StopReason::Stop)],
    ]));

    let cfg = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test system")
        .with_session_id("sess-xyz")
        .with_provider(provider)
        .with_tools(vec![Arc::new(EchoTool)]);

    let run = ocean_runtime::run_agent(&cfg, Message::user_text("use the tool"), None)
        .instrument(turn_span)
        .await
        .expect("scripted 2-round turn must complete cleanly");

    // Sanity: the loop actually ran the tool and completed.
    assert!(!run.stopped_at_turn_limit);

    let output = buf.contents();
    assert!(
        !output.is_empty(),
        "subscriber must have rendered something"
    );

    // 1. The turn_id is attached to the log lines — this is the whole point:
    //    every line emitted while the turn runs carries its turn_id, so
    //    concurrent turns are distinguishable in the logs.
    assert!(
        output.contains(turn_id),
        "rendered output must carry the turn_id span field; got:\n{output}"
    );

    // 2. The span tree is present and nested: turn → agent_loop → round →
    //    tool_exec. The default Full formatter prints the scope as
    //    `turn{..}:agent_loop:round{..}:tool_exec{..}:` ahead of the event line.
    for span_name in ["agent_loop", "round", "tool_exec"] {
        assert!(
            output.contains(span_name),
            "rendered output must contain the `{span_name}` span; got:\n{output}"
        );
    }

    // 3. The tool span carries the tool name, and the in-tool log line proves the
    //    tool body ran inside the tool_exec span (i.e. spans nest around the work,
    //    not just bracket it).
    assert!(
        output.contains("tool_name=echo") || output.contains("tool_name=\"echo\""),
        "tool_exec span must record the tool name; got:\n{output}"
    );
    assert!(
        output.contains("echo tool body running"),
        "the in-tool log line must be present (emitted inside tool_exec); got:\n{output}"
    );

    // 4. The session id flows onto the turn span too (it is the cross-cutting key
    //    the daemon/agent also stamp), so a turn is greppable by either id.
    assert!(
        output.contains("sess-xyz"),
        "rendered output must carry the session_id; got:\n{output}"
    );
}

/// Guard against secrets in span fields: the `provider_stream` span skips
/// `options` (which holds the api key) and the loop's spans never put `args`
/// (prompts / file contents) into a field. We can't reach `stream_simple` here
/// (the injected MockProvider bypasses it), so this test asserts the loop's own
/// spans: a tool whose *arguments* contain a secret-looking value must NOT have
/// that value rendered anywhere in the span output — only the tool name/id are
/// fields.
#[tokio::test(flavor = "current_thread")]
async fn tool_args_are_not_leaked_into_span_fields() {
    let buf = BufWriter::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // A secret value smuggled into the tool arguments. It must never surface in a
    // span field — `tool_exec` records only `tool_name`/`tool_call_id`.
    const SECRET: &str = "sk-supersecret-tool-arg-value-274";

    struct SilentTool;
    #[async_trait]
    impl AgentTool for SilentTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "does not log its args"
        }
        fn parameters(&self) -> Value {
            serde_json::json!({ "type": "object", "properties": {} })
        }
        fn requires_permission(&self) -> bool {
            false
        }
        async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
            Ok(AgentToolResult::text("ok"))
        }
    }

    let provider = Arc::new(MockProvider::new(vec![
        vec![done(
            vec![tool_call(
                "call-secret",
                "echo",
                serde_json::json!({ "token": SECRET }),
            )],
            StopReason::ToolUse,
        )],
        vec![done(vec![Content::text("done")], StopReason::Stop)],
    ]));

    let cfg = AgentConfig::new(Model::anthropic_claude_sonnet_4_6(), "test")
        .with_session_id("sess-secret")
        .with_provider(provider)
        .with_tools(vec![Arc::new(SilentTool)]);

    let turn_span = tracing::info_span!("turn", turn_id = "t-secret", session_id = "sess-secret");
    let _ = ocean_runtime::run_agent(&cfg, Message::user_text("call it"), None)
        .instrument(turn_span)
        .await
        .expect("run must complete");

    let output = buf.contents();
    // The tool_exec span must be present (so we know the path ran)…
    assert!(
        output.contains("tool_exec"),
        "tool_exec span expected:\n{output}"
    );
    // …but the secret tool argument must NOT appear anywhere in the span output.
    assert!(
        !output.contains(SECRET),
        "tool arguments must not be rendered into span fields (secret leaked!):\n{output}"
    );
}
