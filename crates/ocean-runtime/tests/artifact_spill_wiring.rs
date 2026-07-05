//! End-to-end wiring test for W3 output-meta + artifact spill through the real
//! tool path: `BuiltinProvider` under an artifact-spill `SessionContext` →
//! `tools_for_session` wraps every tool in the spill decorator → a tool whose
//! output exceeds the threshold returns a truncated HEAD + notice while the full
//! output is spilled to the session store → `read artifact://<id>` reads it back.
//! Also proves the profile gate: with `artifacts: false`, output is byte-for-byte
//! unchanged and `artifact://` does not resolve.

use std::sync::Arc;

use async_trait::async_trait;
use ocean_runtime::capability::{
    BuiltinProvider, CapabilityRegistry, SessionContext, SPILL_THRESHOLD_BYTES,
};
use ocean_runtime::types::{AgentTool, AgentToolResult};
use serde_json::{json, Value};

fn body(r: &AgentToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.as_text())
        .unwrap_or("")
        .to_string()
}

/// A tool that echoes back a fixed body — lets us drive an over- or
/// under-threshold output deterministically through the decorator.
struct EchoTool {
    payload: String,
}
#[async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo"
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        Ok(AgentToolResult::text(self.payload.clone()))
    }
}

use ocean_runtime::capability::CapabilityProvider;

/// A provider offering the echo tool, so `tools_for_session` merges + decorates
/// it exactly like a real (MCP) provider's tool.
struct EchoProvider {
    payload: String,
}
#[async_trait]
impl CapabilityProvider for EchoProvider {
    fn id(&self) -> &str {
        "echo"
    }
    async fn tools(&self, _ctx: &SessionContext) -> Vec<Arc<dyn AgentTool>> {
        vec![Arc::new(EchoTool {
            payload: self.payload.clone(),
        })]
    }
}

fn spill_ctx(session: &str) -> SessionContext {
    SessionContext {
        cwd: std::env::temp_dir(),
        session_id: Some(session.into()),
        hashline: false,
        artifacts: true,
    }
}

async fn tool_named(reg: &CapabilityRegistry, ctx: &SessionContext, name: &str) -> Arc<dyn AgentTool> {
    reg.tools_for_session(ctx)
        .await
        .into_iter()
        .find(|t| t.name() == name)
        .unwrap_or_else(|| panic!("tool {name} present"))
}

#[tokio::test]
async fn oversized_output_spills_and_reads_back() {
    // A payload comfortably over the threshold, with real line structure so the
    // notice's "lines 1-N of M" is meaningful.
    let line = "the quick brown fox jumps over the lazy dog"; // 43 bytes + \n
    let total_lines = (SPILL_THRESHOLD_BYTES / (line.len() + 1)) + 200;
    let payload: String = (0..total_lines)
        .map(|i| format!("{i}: {line}\n"))
        .collect();
    assert!(payload.len() > SPILL_THRESHOLD_BYTES, "payload must exceed threshold");

    let registry = CapabilityRegistry::new(vec![
        Arc::new(BuiltinProvider::new()),
        Arc::new(EchoProvider {
            payload: payload.clone(),
        }),
    ]);
    let ctx = spill_ctx("s-spill");

    // The echo tool (an MCP-style provider tool) is wrapped by the decorator.
    let echo = tool_named(&registry, &ctx, "echo").await;
    let out = echo.execute("1", json!({})).await.expect("echo ok");
    let shown = body(&out);

    // The model sees a truncated HEAD + a notice, far smaller than the full body.
    assert!(shown.len() < payload.len(), "output truncated for the model");
    assert!(
        shown.contains("[output truncated: showing lines 1-"),
        "truncation notice present, got tail: {:?}",
        &shown[shown.len().saturating_sub(160)..]
    );
    assert!(
        shown.contains("full output: read artifact://"),
        "notice carries the artifact handle"
    );
    // The HEAD is real content (the model can act on it).
    assert!(shown.starts_with("0: the quick brown fox"), "head is the real output start");

    // Extract the artifact id from the notice.
    let id = shown
        .rsplit_once("read artifact://")
        .and_then(|(_, rest)| rest.split(']').next())
        .expect("artifact id in notice")
        .to_string();

    // read artifact://<id> resolves the FULL output back, byte-for-byte.
    let read = tool_named(&registry, &ctx, "read").await;
    let back = read
        .execute("2", json!({ "path": format!("artifact://{id}") }))
        .await
        .expect("artifact read ok");
    assert_eq!(body(&back), payload, "full output round-trips exactly");

    // Offset/limit window the artifact by lines.
    let windowed = read
        .execute(
            "3",
            json!({ "path": format!("artifact://{id}"), "offset": 1, "limit": 2 }),
        )
        .await
        .expect("windowed artifact read ok");
    assert_eq!(
        body(&windowed),
        format!("0: {line}\n1: {line}").trim_end(),
        "line window returns the selected lines"
    );
}

#[tokio::test]
async fn under_threshold_output_is_untouched() {
    let payload = "small output".to_string();
    let registry = CapabilityRegistry::new(vec![
        Arc::new(BuiltinProvider::new()),
        Arc::new(EchoProvider {
            payload: payload.clone(),
        }),
    ]);
    let ctx = spill_ctx("s-small");

    let echo = tool_named(&registry, &ctx, "echo").await;
    let out = echo.execute("1", json!({})).await.expect("echo ok");
    assert_eq!(body(&out), payload, "under-threshold output passes through verbatim");
}

#[tokio::test]
async fn profile_off_is_byte_identical_and_no_artifact_scheme() {
    // Same oversized payload, but artifacts OFF: the decorator is not applied, so
    // output is byte-for-byte the raw tool result and `artifact://` does not
    // resolve (it falls through to a failing disk read).
    let payload: String = "x".repeat(SPILL_THRESHOLD_BYTES * 2);
    let registry = CapabilityRegistry::new(vec![
        Arc::new(BuiltinProvider::new()),
        Arc::new(EchoProvider {
            payload: payload.clone(),
        }),
    ]);
    let ctx = SessionContext {
        cwd: std::env::temp_dir(),
        session_id: Some("s-off".into()),
        hashline: false,
        artifacts: false,
    };

    let echo = tool_named(&registry, &ctx, "echo").await;
    let out = echo.execute("1", json!({})).await.expect("echo ok");
    assert_eq!(body(&out), payload, "off-profile output is unchanged (no truncation)");

    // read of an artifact:// path with the profile off is a plain (failing) read,
    // not a store resolution.
    let read = tool_named(&registry, &ctx, "read").await;
    let err = read
        .execute("2", json!({ "path": "artifact://a1" }))
        .await
        .expect_err("artifact:// is not a store lookup when the profile is off");
    assert!(
        !err.contains("artifact not found"),
        "off-profile must not consult the store, got: {err}"
    );
}
