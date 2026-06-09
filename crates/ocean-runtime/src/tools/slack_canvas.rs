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
//! loop forwards that side effect onto the event bus, and the daemon relays it as
//! `AgentTurnEvent::SlackCanvas` over `/v1/agent/events`; the **Slack canvas
//! bridge** (`ocean-agents`) consumes it, round-trips the op to the real Slack
//! Canvas API, and for `read`/`list` fetches the live content.
//!
//! # Why the awareness ops return *pending*, not fake content (OCEAN-235)
//!
//! The runtime **cannot** fetch live canvas content itself: it holds no Slack
//! token and no Slack API client — all Slack I/O is owned by the `ocean-agents`
//! bridge transport by design. So for `read`/`list` the runtime returns an
//! **honest** result via [`SlackCanvasResult::pending_read`] /
//! [`SlackCanvasResult::pending_list`]: `contents`/`canvases` are absent (never a
//! fabricated empty string) and the result is stamped
//! [`CanvasFetchStatus::PendingBridge`]. The bridge fetches the real body and
//! stamps it back through [`SlackCanvasResult::fulfilled_read`] /
//! [`SlackCanvasResult::fulfilled_list`] — the typed fulfillment seam this runtime
//! defines so content flows through the moment the bridge provides it.

use async_trait::async_trait;
use ocean_agent_sdk::slack_canvas::{CanvasFetchStatus, SlackCanvasOp, SlackCanvasResult};
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

        // --- build the contracted result.
        //
        // OCEAN-235: the runtime cannot itself fetch live Slack canvas content —
        // it holds no Slack token and no Slack API client (by design: all Slack
        // I/O lives in the `ocean-agents` Python bridge transport). So for the
        // awareness ops (`read`/`list`) the runtime emits an **honest** pending
        // result and forwards the op onto the event bus; the Slack bridge fetches
        // the live content and stamps it back through
        // `SlackCanvasResult::fulfilled_read` / `fulfilled_list`.
        //
        // Crucially, a pending `read` carries **no** `contents` (not an empty
        // string) and is marked `CanvasFetchStatus::PendingBridge`, so the agent
        // can never mistake an un-fulfilled read for a genuinely empty canvas.
        let result = match &op {
            SlackCanvasOp::Create { .. } => SlackCanvasResult {
                ok: true,
                op: op_name.to_string(),
                canvas_id: None,
                contents: None,
                canvases: None,
                fetch_status: CanvasFetchStatus::NotApplicable,
                bridged: false,
                metadata: Value::Null,
            },
            SlackCanvasOp::Read { canvas_id } => {
                SlackCanvasResult::pending_read(canvas_id.clone())
            }
            SlackCanvasOp::Update { canvas_id, .. } | SlackCanvasOp::Append { canvas_id, .. } => {
                SlackCanvasResult {
                    ok: true,
                    op: op_name.to_string(),
                    canvas_id: Some(canvas_id.clone()),
                    contents: None,
                    canvases: None,
                    fetch_status: CanvasFetchStatus::NotApplicable,
                    bridged: false,
                    metadata: Value::Null,
                }
            }
            SlackCanvasOp::List { .. } => SlackCanvasResult::pending_list(),
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

    /// `read` is the awareness op. OCEAN-235: until the bridge fetches live
    /// content the runtime returns an **honest pending** result — it carries the
    /// canvas_id and is marked `fetch_status: "pending_bridge"` with `bridged:
    /// false`, and it does **not** fabricate a `contents` value (the key is
    /// absent), so the agent can't mistake an un-fulfilled read for an empty
    /// canvas. The read op is forwarded as a side effect for the bridge to fulfill.
    #[tokio::test]
    async fn slack_canvas_read_is_honest_pending_not_fake_empty() {
        let tool = SlackCanvasTool;
        let args = json!({ "op": "read", "canvas_id": "F0123ABCD" });
        let res = tool.execute("call-3", args).await.expect("valid read");
        assert_eq!(res.details["op"], "read");
        assert_eq!(res.details["canvas_id"], "F0123ABCD");
        // Honesty marker: pending, not fetched, not bridged.
        assert_eq!(res.details["fetch_status"], "pending_bridge");
        assert_eq!(res.details["bridged"], false);
        // The `contents` key must be ABSENT — never an empty string masquerading
        // as a genuinely empty canvas body.
        assert!(
            res.details.get("contents").is_none(),
            "pending read must not fabricate a contents value: {}",
            res.details
        );

        // The op is forwarded so the bridge can fetch and fulfill it.
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

    /// A valid `list` is accepted. OCEAN-235: like `read`, it returns an honest
    /// pending result — `fetch_status: "pending_bridge"`, no fabricated `canvases`
    /// array — until the bridge resolves the channel's real canvases.
    #[tokio::test]
    async fn slack_canvas_list_is_honest_pending() {
        let tool = SlackCanvasTool;
        let args = json!({ "op": "list", "channel_id": "C1" });
        let res = tool.execute("call-6", args).await.expect("valid list");
        assert_eq!(res.details["op"], "list");
        assert_eq!(res.details["fetch_status"], "pending_bridge");
        assert_eq!(res.details["bridged"], false);
        assert!(
            res.details.get("canvases").is_none(),
            "pending list must not fabricate a canvases array: {}",
            res.details
        );
    }

    /// Mutating ops carry `fetch_status: "not_applicable"` — they are complete on
    /// their own and don't await any bridge fetch.
    #[tokio::test]
    async fn slack_canvas_mutating_ops_are_not_applicable() {
        let tool = SlackCanvasTool;
        for args in [
            json!({ "op": "create", "title": "T" }),
            json!({ "op": "update", "canvas_id": "F1", "markdown": "x" }),
            json!({ "op": "append", "canvas_id": "F1", "markdown": "x" }),
        ] {
            let op = args["op"].as_str().unwrap().to_string();
            let res = tool.execute("call-na", args).await.expect("valid mutating op");
            assert_eq!(
                res.details["fetch_status"], "not_applicable",
                "{op} should be not_applicable: {}",
                res.details
            );
        }
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
