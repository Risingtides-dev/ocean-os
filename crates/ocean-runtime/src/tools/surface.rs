//! `surface_patch` — the agent-native canvas mutation tool (GPUI Masterbuild
//! Slice 2, OCEAN-149).
//!
//! The agent calls this to apply one or more structured patches to the current
//! Ocean surface canvas. Each patch is validated against the shared SDK
//! [`SurfacePatch`] vocabulary (Slice 1, `ocean-agent-sdk::surface`) — this tool
//! does **not** redefine the wire types, it reuses them.
//!
//! On success the tool returns the §7 structured result
//! (`{ok, applied, canvas_id, revision, component_ids}`) and emits a
//! [`ToolSideEffect::SurfacePatch`] carrying the validated patches. The agent
//! loop forwards that side effect onto the event bus; the daemon (Slice 3) wraps
//! each patch in a [`SurfacePatchEnvelope`](ocean_agent_sdk::surface::SurfacePatchEnvelope)
//! and streams it over `/v1/agent/events`.
//!
//! The `revision` is a passthrough/echo for now: the GPUI `CanvasLedger`
//! (Slice 4/6) owns the authoritative revision, so the tool only reports what it
//! accepted. An optional `revision` arg lets a caller echo the ledger's current
//! revision back; absent, it reports `0`.

use async_trait::async_trait;
use ocean_agent_sdk::surface::SurfacePatch;
use serde_json::{json, Value};

use crate::types::{AgentTool, AgentToolResult, ToolSideEffect};

/// The `op` discriminants accepted by the tool, mirrored into the JSON schema so
/// the model sees the closed set up front (§7).
const VALID_OPS: &[&str] = &[
    "upsert_component",
    "move_component",
    "resize_component",
    "delete_component",
    "connect",
    "disconnect",
    "focus",
    "select",
    "set_viewport",
    "layout",
    "group",
];

pub struct SurfacePatchTool;

#[async_trait]
impl AgentTool for SurfacePatchTool {
    fn name(&self) -> &str {
        "surface_patch"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Apply one or more structured patches to the current Ocean surface canvas. \
         Use this whenever the user asks for visual/canvas/workflow/storyboard/board output. \
         Do not draw ASCII diagrams in chat when the canvas is available."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "required": ["canvas_id", "patches"],
            "properties": {
                "canvas_id": {
                    "type": "string",
                    "description": "Id of the canvas to mutate, e.g. \"canvas:main\"."
                },
                "patches": {
                    "type": "array",
                    "description": "One or more patch operations to apply, in order.",
                    "items": {
                        "type": "object",
                        "required": ["op"],
                        "properties": {
                            "op": {
                                "type": "string",
                                "enum": VALID_OPS
                            }
                        }
                    }
                }
            }
        })
    }

    async fn execute(&self, _tool_call_id: &str, args: Value) -> Result<AgentToolResult, String> {
        // --- canvas_id: present and non-empty ---
        let canvas_id = args
            .get("canvas_id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'canvas_id'")?
            .to_string();
        if canvas_id.trim().is_empty() {
            return Err("'canvas_id' must not be empty".to_string());
        }

        // --- patches: present, an array, non-empty ---
        let raw_patches = args
            .get("patches")
            .ok_or("missing 'patches'")?
            .as_array()
            .ok_or("'patches' must be an array")?;
        if raw_patches.is_empty() {
            return Err("'patches' must not be empty".to_string());
        }

        // --- parse each patch into the SDK vocabulary ---
        let mut patches: Vec<SurfacePatch> = Vec::with_capacity(raw_patches.len());
        for (i, raw) in raw_patches.iter().enumerate() {
            let patch: SurfacePatch = serde_json::from_value(raw.clone())
                .map_err(|e| format!("patch[{i}] is invalid: {e}"))?;
            patches.push(patch);
        }

        // --- collect the component ids touched by the accepted patches ---
        let component_ids: Vec<String> = patches.iter().filter_map(component_id_of).collect();

        // The GPUI ledger owns the real revision (Slice 4/6). Echo an optional
        // caller-supplied revision; otherwise report 0.
        let revision = args.get("revision").and_then(|v| v.as_u64()).unwrap_or(0);

        let applied = patches.len() as u32;

        let result = json!({
            "ok": true,
            "applied": applied,
            "canvas_id": canvas_id,
            "revision": revision,
            "component_ids": component_ids,
        });

        let summary = format!(
            "applied {applied} patch(es) to canvas '{canvas_id}'{}",
            if component_ids.is_empty() {
                String::new()
            } else {
                format!(" touching: {}", component_ids.join(", "))
            }
        );

        Ok(AgentToolResult {
            content: vec![ocean_protocol::Content::text(summary)],
            details: result,
            terminate: false,
            side_effects: vec![ToolSideEffect::SurfacePatch { canvas_id, patches }],
        })
    }
}

/// Extract the component id a patch primarily targets, if any. Used to build the
/// §7 `component_ids` field. Ops that don't name a single component (focus on the
/// whole canvas, viewport changes, multi-select, etc.) contribute nothing.
fn component_id_of(patch: &SurfacePatch) -> Option<String> {
    match patch {
        SurfacePatch::UpsertComponent { component } => Some(component.id.to_string()),
        SurfacePatch::MoveComponent { component_id, .. }
        | SurfacePatch::ResizeComponent { component_id, .. }
        | SurfacePatch::DeleteComponent { component_id } => Some(component_id.to_string()),
        SurfacePatch::Group { frame_id, .. } => Some(frame_id.to_string()),
        // Edges, focus, select, viewport, layout do not map to a single
        // component id for the §7 result.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolSideEffect;

    /// A valid multi-patch payload is accepted, the §7 result references every
    /// touched component id, and the side effect carries the parsed patches.
    #[tokio::test]
    async fn surface_patch_accepts_valid_multi_patch_payload() {
        let tool = SurfacePatchTool;
        let args = json!({
            "canvas_id": "canvas:main",
            "patches": [
                {
                    "op": "upsert_component",
                    "component": { "id": "brief-1", "kind": "brief_card" }
                },
                {
                    "op": "upsert_component",
                    "component": { "id": "proposal-1", "kind": "card" }
                },
                {
                    "op": "move_component",
                    "component_id": "brief-1",
                    "x": 10.0,
                    "y": 20.0
                }
            ]
        });

        let res = tool.execute("call-1", args).await.expect("valid payload");

        assert_eq!(res.details["ok"], true);
        assert_eq!(res.details["applied"], 3);
        assert_eq!(res.details["canvas_id"], "canvas:main");
        // revision is a passthrough; defaults to 0 when not supplied.
        assert_eq!(res.details["revision"], 0);
        assert_eq!(
            res.details["component_ids"],
            json!(["brief-1", "proposal-1", "brief-1"])
        );

        // The side effect carries the parsed patches for Slice 3 to stream.
        assert_eq!(res.side_effects.len(), 1);
        match &res.side_effects[0] {
            ToolSideEffect::SurfacePatch { canvas_id, patches } => {
                assert_eq!(canvas_id, "canvas:main");
                assert_eq!(patches.len(), 3);
            }
            other => panic!("expected SurfacePatch side effect, got {other:?}"),
        }
    }

    /// The EXACT §6 `upsert_component` JSON is accepted and the touched id is
    /// reflected in the result.
    #[tokio::test]
    async fn surface_patch_accepts_section_6_upsert_component() {
        let tool = SurfacePatchTool;
        let args = json!({
            "canvas_id": "canvas:main",
            "patches": [
                {
                    "op": "upsert_component",
                    "component": {
                        "id": "brief-1",
                        "kind": "brief_card",
                        "rect": { "x": 420, "y": 120, "w": 320, "h": 220 },
                        "content": {
                            "title": "Sales Brief",
                            "body": "Draft brief for the Warner campaign"
                        },
                        "metadata": { "source": "longhouse.sales" }
                    }
                }
            ]
        });

        let res = tool
            .execute("call-2", args)
            .await
            .expect("§6 shape accepted");
        assert_eq!(res.details["applied"], 1);
        assert_eq!(res.details["component_ids"], json!(["brief-1"]));

        match &res.side_effects[0] {
            ToolSideEffect::SurfacePatch { patches, .. } => match &patches[0] {
                SurfacePatch::UpsertComponent { component } => {
                    assert_eq!(component.id.as_str(), "brief-1");
                    assert_eq!(component.kind, "brief_card");
                    let rect = component.rect.expect("rect present");
                    assert_eq!(rect.x, 420.0);
                    assert_eq!(component.content["title"], "Sales Brief");
                    assert_eq!(component.metadata["source"], "longhouse.sales");
                }
                other => panic!("expected UpsertComponent, got {other:?}"),
            },
            other => panic!("expected SurfacePatch side effect, got {other:?}"),
        }
    }

    /// An optional `revision` arg is echoed back into the result.
    #[tokio::test]
    async fn surface_patch_echoes_supplied_revision() {
        let tool = SurfacePatchTool;
        let args = json!({
            "canvas_id": "canvas:main",
            "revision": 12,
            "patches": [
                { "op": "delete_component", "component_id": "c1" }
            ]
        });
        let res = tool.execute("call-3", args).await.expect("valid");
        assert_eq!(res.details["revision"], 12);
        assert_eq!(res.details["component_ids"], json!(["c1"]));
    }

    /// Empty `patches` is rejected.
    #[tokio::test]
    async fn surface_patch_rejects_empty_patches() {
        let tool = SurfacePatchTool;
        let args = json!({ "canvas_id": "canvas:main", "patches": [] });
        let err = tool
            .execute("call-4", args)
            .await
            .expect_err("empty patches must be rejected");
        assert!(err.contains("patches"), "unexpected error: {err}");
    }

    /// Missing `canvas_id` is rejected.
    #[tokio::test]
    async fn surface_patch_rejects_missing_canvas_id() {
        let tool = SurfacePatchTool;
        let args = json!({
            "patches": [
                { "op": "upsert_component", "component": { "id": "a", "kind": "card" } }
            ]
        });
        let err = tool
            .execute("call-5", args)
            .await
            .expect_err("missing canvas_id must be rejected");
        assert!(err.contains("canvas_id"), "unexpected error: {err}");
    }

    /// An empty `canvas_id` string is rejected.
    #[tokio::test]
    async fn surface_patch_rejects_empty_canvas_id() {
        let tool = SurfacePatchTool;
        let args = json!({
            "canvas_id": "   ",
            "patches": [
                { "op": "upsert_component", "component": { "id": "a", "kind": "card" } }
            ]
        });
        let err = tool
            .execute("call-6", args)
            .await
            .expect_err("empty canvas_id must be rejected");
        assert!(err.contains("canvas_id"), "unexpected error: {err}");
    }

    /// A patch with an unknown/invalid op is rejected with a clear, indexed error.
    #[tokio::test]
    async fn surface_patch_rejects_invalid_patch() {
        let tool = SurfacePatchTool;
        let args = json!({
            "canvas_id": "canvas:main",
            "patches": [
                { "op": "upsert_component", "component": { "id": "a", "kind": "card" } },
                { "op": "not_a_real_op" }
            ]
        });
        let err = tool
            .execute("call-7", args)
            .await
            .expect_err("invalid patch must be rejected");
        assert!(err.contains("patch[1]"), "unexpected error: {err}");
    }
}
