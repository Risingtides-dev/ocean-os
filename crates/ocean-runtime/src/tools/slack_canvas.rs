//! `slack_canvas` — the agent's Slack-canvas-as-playground tool (OCEAN-214 ph2).
//!
//! Slack canvases become a persistent, **bidirectional** surface the agent owns —
//! the Slack analogue of the GPUI canvas (`surface_patch` to write + a ledger to
//! read back). Through this tool the agent can:
//!
//! - `create` a new canvas (optional title + initial markdown, optional channel),
//! - `read` back a canvas's current contents — the **awareness** op — so it can
//!   reason over what the canvas holds,
//! - `update`/`append` a canvas's contents,
//! - `list` the canvases in a channel.
//!
//! Each op is validated against the shared SDK
//! [`SlackCanvasOp`](ocean_agent_sdk::slack_canvas::SlackCanvasOp) vocabulary —
//! this tool does **not** redefine the wire types, it reuses them (exactly as
//! `surface_patch` reuses `SurfacePatch`).
//!
//! On success the tool returns a structured
//! [`SlackCanvasResult`](ocean_agent_sdk::slack_canvas::SlackCanvasResult) and
//! emits a [`ToolSideEffect::SlackCanvas`] carrying the validated op. The agent
//! loop forwards that side effect onto the event bus; the **Slack canvas bridge**
//! (`ocean-agents`, a later phase) round-trips the op to the real Slack Canvas API
//! and, for `read`/`list`, fills in live contents. THIS phase is the tool layer
//! only: the runtime emits a well-formed, contracted result (`bridged: false`) so
//! the agent loop + tests work end-to-end; no Slack call happens here.

use async_trait::async_trait;
use ocean_agent_sdk::slack_canvas::{SlackCanvasOp, SlackCanvasResult};
use serde_json::{json, Value};

use crate::types::{AgentTool, AgentToolResult, ToolSideEffect};

pub struct SlackCanvasTool;

#[async_trait]
impl AgentTool for SlackCanvasTool {
    fn name(&self) -> &str {
        "slack_canvas"
    }

    /// Mutating ops (`create`/`update`/`append`) make this a side-effecting tool,
    /// so the tool is permission-gated as a whole — mirroring how `write`/`edit`
    /// gate. The non-mutating reads (`read`/`list`) are pure awareness; a
    /// permission policy that wants finer granularity can inspect the `op` field
    /// in `args` (passed to `PermissionPolicy::check`) and wave reads through. The
    /// safe default gates the tool because its dominant ops mutate a shared Slack
    /// surface.
    fn requires_permission(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Create, read, update, append, or list Slack canvases — your persistent, \
         bidirectional Slack workspace. Use 'read' to fetch a canvas's current \
         contents before reasoning over or modifying it (full awareness), and \
         'create'/'update'/'append' to write to it. Prefer a canvas over long \
         chat messages when the user wants a durable, editable document in Slack."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["op"],
            "properties": {
                "op": {
                    "type": "string",
                    "enum": SlackCanvasOp::VALID_OPS,
                    "description": "The canvas operation. 'create' makes a new canvas \
                        (optional title/markdown/channel_id); 'read' returns the current \
                        contents of canvas_id (awareness); 'update' rewrites canvas_id \
                        (mode: replace|append|prepend); 'append' adds markdown to the end \
                        of canvas_id; 'list' returns the canvases in channel_id."
                },
                "canvas_id": {
                    "type": "string",
                    "description": "Slack canvas id (e.g. \"F0123ABCD\"). Required for \
                        'read', 'update', and 'append'."
                },
                "channel_id": {
                    "type": "string",
                    "description": "Slack channel id (e.g. \"C0123ABCD\"). Required for \
                        'list'; optional for 'create' to scope the new canvas to a channel."
                },
                "title": {
                    "type": "string",
                    "description": "Optional title for a new canvas ('create')."
                },
                "markdown": {
                    "type": "string",
                    "description": "Markdown body. Optional initial content for 'create'; \
                        required for 'update' and 'append'."
                },
                "mode": {
                    "type": "string",
                    "enum": ["replace", "append", "prepend"],
                    "description": "How 'update' applies its markdown. Defaults to 'replace'."
                }
            }
        })
    }

    async fn execute(&self, _tool_call_id: &str, args: Value) -> Result<AgentToolResult, String> {
        // --- op: present and non-empty ---
        let op_name = args
            .get("op")
            .and_then(|v| v.as_str())
            .ok_or("missing 'op'")?
            .trim();
        if op_name.is_empty() {
            return Err("'op' must not be empty".to_string());
        }
        if !SlackCanvasOp::VALID_OPS.contains(&op_name) {
            return Err(format!(
                "unknown op '{op_name}'; expected one of: {}",
                SlackCanvasOp::VALID_OPS.join(", ")
            ));
        }

        // --- parse the whole payload into the SDK vocabulary, which enforces the
        // per-op required fields (canvas_id on read/update/append, channel_id on
        // list, markdown on update/append, …) via serde. ---
        let op: SlackCanvasOp = serde_json::from_value(args.clone())
            .map_err(|e| format!("invalid '{op_name}' op: {e}"))?;

        // Extra guards beyond what serde's `Option`/required split gives us:
        // mutating writes must carry non-empty markdown.
        match &op {
            SlackCanvasOp::Update { markdown, .. } | SlackCanvasOp::Append { markdown, .. }
                if markdown.trim().is_empty() =>
            {
                return Err(format!("'{op_name}' requires non-empty 'markdown'"));
            }
            _ => {}
        }

        // --- build the contracted result. The bridge (later phase) fills the
        // awareness/list payloads with live Slack data and flips `bridged`. ---
        let (canvas_id, contents, canvases) = match &op {
            SlackCanvasOp::Create { .. } => (None, None, None),
            SlackCanvasOp::Read { canvas_id } => (
                Some(canvas_id.clone()),
                // Contracted awareness placeholder until the bridge fetches the
                // real canvas body. Shape is stable so the agent + tests can rely
                // on `contents` being the read-back channel.
                Some(String::new()),
                None,
            ),
            SlackCanvasOp::Update { canvas_id, .. } | SlackCanvasOp::Append { canvas_id, .. } => {
                (Some(canvas_id.clone()), None, None)
            }
            SlackCanvasOp::List { .. } => (None, None, Some(Vec::new())),
        };

        let result = SlackCanvasResult {
            ok: true,
            op: op_name.to_string(),
            canvas_id,
            contents,
            canvases,
            bridged: false,
            metadata: Value::Null,
        };

        let summary = match &op {
            SlackCanvasOp::Create { title, .. } => match title {
                Some(t) => format!("queued create of Slack canvas '{t}'"),
                None => "queued create of a new Slack canvas".to_string(),
            },
            SlackCanvasOp::Read { canvas_id } => {
                format!("read-back requested for Slack canvas '{canvas_id}'")
            }
            SlackCanvasOp::Update { canvas_id, mode, .. } => {
                format!("queued {mode:?} update of Slack canvas '{canvas_id}'")
            }
            SlackCanvasOp::Append { canvas_id, .. } => {
                format!("queued append to Slack canvas '{canvas_id}'")
            }
            SlackCanvasOp::List { channel_id } => {
                format!("listing Slack canvases in channel '{channel_id}'")
            }
        };

        let details = serde_json::to_value(&result)
            .map_err(|e| format!("failed to encode slack_canvas result: {e}"))?;

        Ok(AgentToolResult {
            content: vec![ocean_protocol::Content::text(summary)],
            details,
            terminate: false,
            side_effects: vec![ToolSideEffect::SlackCanvas { op }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolSideEffect;

    /// A valid `create` (title + markdown) is accepted, the result echoes the op,
    /// and the side effect carries the parsed op.
    #[tokio::test]
    async fn slack_canvas_accepts_create() {
        let tool = SlackCanvasTool;
        let args = json!({
            "op": "create",
            "title": "Campaign Plan",
            "markdown": "# Plan\n- step one",
            "channel_id": "C0123ABCD"
        });

        let res = tool.execute("call-1", args).await.expect("valid create");
        assert_eq!(res.details["ok"], true);
        assert_eq!(res.details["op"], "create");
        assert_eq!(res.details["bridged"], false);
        // create has no canvas_id until the bridge mints one.
        assert!(res.details.get("canvas_id").is_none());

        assert_eq!(res.side_effects.len(), 1);
        match &res.side_effects[0] {
            ToolSideEffect::SlackCanvas { op } => match op {
                SlackCanvasOp::Create {
                    title,
                    markdown,
                    channel_id,
                } => {
                    assert_eq!(title.as_deref(), Some("Campaign Plan"));
                    assert_eq!(markdown.as_deref(), Some("# Plan\n- step one"));
                    assert_eq!(channel_id.as_ref().unwrap().as_str(), "C0123ABCD");
                }
                other => panic!("expected Create, got {other:?}"),
            },
            other => panic!("expected SlackCanvas side effect, got {other:?}"),
        }
    }

    /// A bare `create` (no body) is accepted — the app/bridge fills in.
    #[tokio::test]
    async fn slack_canvas_accepts_bare_create() {
        let tool = SlackCanvasTool;
        let res = tool
            .execute("call-2", json!({ "op": "create" }))
            .await
            .expect("bare create ok");
        assert_eq!(res.details["op"], "create");
    }

    /// `read` is the awareness op: the result carries the canvas_id and a
    /// `contents` field (the read-back channel) the agent reasons over.
    #[tokio::test]
    async fn slack_canvas_read_returns_contents_field() {
        let tool = SlackCanvasTool;
        let args = json!({ "op": "read", "canvas_id": "F0123ABCD" });
        let res = tool.execute("call-3", args).await.expect("valid read");
        assert_eq!(res.details["op"], "read");
        assert_eq!(res.details["canvas_id"], "F0123ABCD");
        // `contents` is present (the awareness channel), even if empty until the
        // bridge populates live data.
        assert!(
            res.details.get("contents").is_some(),
            "read result must carry a contents field: {}",
            res.details
        );

        match &res.side_effects[0] {
            ToolSideEffect::SlackCanvas {
                op: SlackCanvasOp::Read { canvas_id },
            } => assert_eq!(canvas_id.as_str(), "F0123ABCD"),
            other => panic!("expected Read side effect, got {other:?}"),
        }
    }

    /// A valid `update` is accepted and defaults its mode to replace.
    #[tokio::test]
    async fn slack_canvas_accepts_update() {
        let tool = SlackCanvasTool;
        let args = json!({
            "op": "update",
            "canvas_id": "F1",
            "markdown": "rewritten body"
        });
        let res = tool.execute("call-4", args).await.expect("valid update");
        assert_eq!(res.details["op"], "update");
        assert_eq!(res.details["canvas_id"], "F1");
        match &res.side_effects[0] {
            ToolSideEffect::SlackCanvas {
                op: SlackCanvasOp::Update { mode, .. },
            } => assert_eq!(
                *mode,
                ocean_agent_sdk::slack_canvas::CanvasEditMode::Replace
            ),
            other => panic!("expected Update side effect, got {other:?}"),
        }
    }

    /// A valid `append` is accepted.
    #[tokio::test]
    async fn slack_canvas_accepts_append() {
        let tool = SlackCanvasTool;
        let args = json!({ "op": "append", "canvas_id": "F1", "markdown": "more" });
        let res = tool.execute("call-5", args).await.expect("valid append");
        assert_eq!(res.details["op"], "append");
        assert_eq!(res.details["canvas_id"], "F1");
    }

    /// A valid `list` is accepted and carries a `canvases` array.
    #[tokio::test]
    async fn slack_canvas_accepts_list() {
        let tool = SlackCanvasTool;
        let args = json!({ "op": "list", "channel_id": "C1" });
        let res = tool.execute("call-6", args).await.expect("valid list");
        assert_eq!(res.details["op"], "list");
        assert!(res.details["canvases"].is_array());
    }

    /// Missing `op` is rejected.
    #[tokio::test]
    async fn slack_canvas_rejects_missing_op() {
        let tool = SlackCanvasTool;
        let err = tool
            .execute("call-7", json!({ "canvas_id": "F1" }))
            .await
            .expect_err("missing op must be rejected");
        assert!(err.contains("op"), "unexpected error: {err}");
    }

    /// An unknown op is rejected.
    #[tokio::test]
    async fn slack_canvas_rejects_unknown_op() {
        let tool = SlackCanvasTool;
        let err = tool
            .execute("call-8", json!({ "op": "obliterate" }))
            .await
            .expect_err("unknown op must be rejected");
        assert!(err.contains("obliterate"), "unexpected error: {err}");
    }

    /// `read` without `canvas_id` is rejected (serde enforces the required field).
    #[tokio::test]
    async fn slack_canvas_rejects_read_without_canvas_id() {
        let tool = SlackCanvasTool;
        let err = tool
            .execute("call-9", json!({ "op": "read" }))
            .await
            .expect_err("read without canvas_id must be rejected");
        assert!(err.contains("read"), "unexpected error: {err}");
    }

    /// `update` without `canvas_id` is rejected.
    #[tokio::test]
    async fn slack_canvas_rejects_update_without_canvas_id() {
        let tool = SlackCanvasTool;
        let err = tool
            .execute("call-10", json!({ "op": "update", "markdown": "x" }))
            .await
            .expect_err("update without canvas_id must be rejected");
        assert!(err.contains("update"), "unexpected error: {err}");
    }

    /// `update`/`append` with empty markdown is rejected.
    #[tokio::test]
    async fn slack_canvas_rejects_empty_markdown() {
        let tool = SlackCanvasTool;
        let err = tool
            .execute(
                "call-11",
                json!({ "op": "append", "canvas_id": "F1", "markdown": "   " }),
            )
            .await
            .expect_err("empty markdown must be rejected");
        assert!(err.contains("markdown"), "unexpected error: {err}");
    }

    /// `list` without `channel_id` is rejected.
    #[tokio::test]
    async fn slack_canvas_rejects_list_without_channel_id() {
        let tool = SlackCanvasTool;
        let err = tool
            .execute("call-12", json!({ "op": "list" }))
            .await
            .expect_err("list without channel_id must be rejected");
        assert!(err.contains("list"), "unexpected error: {err}");
    }

    /// The tool is permission-gated (mutating ops dominate).
    #[test]
    fn slack_canvas_requires_permission() {
        assert!(SlackCanvasTool.requires_permission());
    }
}
