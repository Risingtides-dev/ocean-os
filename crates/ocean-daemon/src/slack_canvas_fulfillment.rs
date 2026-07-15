use super::{evict_overflow, AppState};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent, AgentTurnId};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

/// Per-`(session, canvas)` store of bridge-fulfilled `slack_canvas` results
/// (OCEAN-262). The value is the bridge's POSTed `result` body verbatim — a
/// superset of the SDK [`ocean_agent_sdk::slack_canvas::SlackCanvasResult`]
/// (it adds `bridged: true`, an optional `error`, and a raw passthrough) — so we
/// preserve exactly what the bridge sent for the `GET` query, and separately
/// derive a typed `SlackCanvasResult` for the SSE re-emit.
pub(super) type CanvasFulfillmentStore =
    Arc<Mutex<HashMap<CanvasFulfillmentKey, CanvasFulfillment>>>;

/// Key into [`CanvasFulfillmentStore`]: a fulfilled result is addressable by the
/// session it belongs to plus a stable per-canvas key. For `read`/`update`/
/// `append` the canvas key is the real Slack `canvas_id`; for `list` (which has
/// no single canvas) and `create` (no id yet) it's a synthetic key derived from
/// the op (see [`canvas_fulfillment_key_for_op`]).
pub(super) type CanvasFulfillmentKey = (AgentSessionId, String);

/// One stored bridge fulfillment (OCEAN-262): the raw `result` body the bridge
/// POSTed plus the wall-clock time we received it. `received_at` drives TTL
/// eviction in `gc_registries` (OCEAN-273) so the store stays bounded.
#[derive(Clone)]
pub(super) struct CanvasFulfillment {
    /// The bridge's `result` JSON verbatim (SDK-result-shaped superset).
    pub(super) result: Value,
    /// When the daemon received this fulfillment. Used by the GC sweep to evict
    /// entries older than `CANVAS_FULFILLMENT_TTL` (OCEAN-273).
    pub(super) received_at: DateTime<Utc>,
}

/// TTL for `canvas_fulfillments` (OCEAN-273). Unlike requests/permissions a
/// fulfillment has no terminal state — a read never consumes it — so it's
/// evictable purely by age once it's old enough that the agent has almost
/// certainly read it back. Kept equal to the runtime's
/// [`ocean_runtime::tools::slack_canvas::CANVAS_FULFILLMENT_TTL`] so both halves
/// of the same `(session, canvas)` slot (the daemon's query store and the
/// runtime's lookup registry) expire on the same schedule.
pub(super) const CANVAS_FULFILLMENT_TTL: chrono::Duration = chrono::Duration::minutes(30);

/// Sweep both halves of the fulfilled-canvas bridge on the same injected clock
/// and cap: the daemon-owned raw query store, then the runtime-owned typed lookup
/// registry. Synchronous by design so no await can split the coupled lifecycle.
pub(super) fn gc_canvas_fulfillments(
    canvas_fulfillments: &CanvasFulfillmentStore,
    now: DateTime<Utc>,
    max_entries: usize,
) {
    // OCEAN-273: bound the bridge-fulfillment query store. A fulfillment has no
    // terminal state (a `GET`/SSE read never removes it), so every entry is
    // evictable purely by age — drop anything older than `CANVAS_FULFILLMENT_TTL`,
    // then enforce the injected cap as a burst backstop. For the cap, every entry
    // is treated as "terminal" (`is_terminal = true`) so `evict_overflow` simply
    // removes the oldest by `received_at`.
    {
        let mut store = canvas_fulfillments
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let cttl = CANVAS_FULFILLMENT_TTL;
        store.retain(|_, f| (now - f.received_at) <= cttl);
        if store.len() > max_entries {
            evict_overflow(&mut store, |_| true, |f| f.received_at, max_entries);
        }
    }
    // OCEAN-273: bound the runtime-owned lookup registry (OCEAN-271) the same way.
    // The daemon writes both halves of each fulfillment in lock-step, so they
    // share a scheduler tick, injected clock, TTL contract, and cap.
    ocean_runtime::tools::slack_canvas::CANVAS_FULFILLMENT_REGISTRY.gc(
        now,
        ocean_runtime::tools::slack_canvas::CANVAS_FULFILLMENT_TTL,
        max_entries,
    );
}

// ---------------------------------------------------------------------------
// OCEAN-262: slack_canvas host fulfillment seam
//
// The generic Slack transport and real API bridge belong to the `ocean-slack`
// extension. This module is the daemon-owned compatibility/enforcement side:
// session-scoped ingress, raw query storage, runtime lookup delivery, fulfilled
// event re-emission, and coupled lifecycle GC.
// ---------------------------------------------------------------------------

/// The store key for a fulfilled `slack_canvas` op (OCEAN-262). `read`/`update`/
/// `append` key on the real Slack `canvas_id`; `list` has no single canvas so it
/// keys on `list:{channel_id}`; `create` has no id yet so it keys on
/// `create:{title}` (or `create:` when untitled). Stable across the pending event
/// and the fulfillment POST as long as the op is the same.
pub(super) fn canvas_fulfillment_key_for_op(
    op: &ocean_agent_sdk::slack_canvas::SlackCanvasOp,
) -> String {
    use ocean_agent_sdk::slack_canvas::SlackCanvasOp;
    match op {
        SlackCanvasOp::Read { canvas_id } => canvas_id.as_str().to_string(),
        SlackCanvasOp::Update { canvas_id, .. } | SlackCanvasOp::Append { canvas_id, .. } => {
            canvas_id.as_str().to_string()
        }
        SlackCanvasOp::List { channel_id } => format!("list:{channel_id}"),
        SlackCanvasOp::Create { title, .. } => {
            format!("create:{}", title.as_deref().unwrap_or(""))
        }
    }
}

/// Build the typed [`SlackCanvasResult`](ocean_agent_sdk::slack_canvas::SlackCanvasResult)
/// the daemon re-emits onto the SSE bus from the bridge's POSTed `result` body
/// (OCEAN-262). The bridge result is a superset of the SDK type (it adds
/// `bridged`, `error`, `raw` and omits `fetch_status`), so:
///
/// - a `read` with `contents` present → [`SlackCanvasResult::fulfilled_read`]
///   (content stamped, `fetch_status: fetched`);
/// - a `list` with `canvases` present → [`SlackCanvasResult::fulfilled_list`];
/// - anything else (mutating op, or an awareness op the bridge reported `ok:false`
///   for) → a best-effort lenient deserialize, falling back to the matching
///   pending result so the re-emit is always a well-formed `SlackCanvasResult`.
pub(super) fn fulfilled_result_from_bridge(
    op: &ocean_agent_sdk::slack_canvas::SlackCanvasOp,
    result: &Value,
) -> ocean_agent_sdk::slack_canvas::SlackCanvasResult {
    use ocean_agent_sdk::slack_canvas::{SlackCanvasOp, SlackCanvasResult, SlackCanvasSummary};

    let bridge_ok = result.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);

    match op {
        SlackCanvasOp::Read { canvas_id } => {
            match result.get("contents").and_then(|v| v.as_str()) {
                Some(contents) if bridge_ok => SlackCanvasResult::fulfilled_read(
                    canvas_id.clone(),
                    contents,
                    result.get("raw").cloned().unwrap_or(Value::Null),
                ),
                // Bridge couldn't fetch (ok:false) or sent no contents: stay honest —
                // re-emit the pending shape rather than fabricate empty content.
                _ => SlackCanvasResult::pending_read(canvas_id.clone()),
            }
        }
        SlackCanvasOp::List { .. } => match result.get("canvases") {
            Some(canvases) if bridge_ok => {
                let summaries: Vec<SlackCanvasSummary> =
                    serde_json::from_value(canvases.clone()).unwrap_or_default();
                SlackCanvasResult::fulfilled_list(
                    summaries,
                    result.get("raw").cloned().unwrap_or(Value::Null),
                )
            }
            _ => SlackCanvasResult::pending_list(),
        },
        // Mutating ops: the bridge's effect is already live in Slack. Re-emit a
        // `bridged: true` result, carrying the real `canvas_id` the bridge minted
        // for `create` (the agent's create had none until now).
        SlackCanvasOp::Create { .. }
        | SlackCanvasOp::Update { .. }
        | SlackCanvasOp::Append { .. } => {
            let canvas_id = result
                .get("canvas_id")
                .and_then(|v| v.as_str())
                .map(ocean_agent_sdk::slack_canvas::SlackCanvasId::new);
            SlackCanvasResult {
                ok: bridge_ok,
                op: op.op_name().to_string(),
                canvas_id,
                contents: None,
                canvases: None,
                fetch_status: ocean_agent_sdk::slack_canvas::CanvasFetchStatus::NotApplicable,
                bridged: true,
                metadata: result.get("raw").cloned().unwrap_or(Value::Null),
            }
        }
    }
}

/// `POST /v1/agent/canvas/fulfill` — receive a bridge-fulfilled `slack_canvas`
/// result (OCEAN-262).
///
/// Body (sent by the external Slack bridge fulfillment delivery):
/// ```json
/// {
///   "session_id": "uuid-of-session",
///   "op": { "op": "read", "canvas_id": "F0123ABCD" },
///   "result": { "ok": true, "op": "read", "canvas_id": "F0123ABCD",
///               "contents": "# live body", "bridged": true, "raw": { ... } }
/// }
/// ```
///
/// Stores the raw `result` keyed by `(session_id, canvas key)` for the `GET`
/// query, and re-emits `AgentTurnEvent::SlackCanvas` with a typed *fulfilled*
/// result so the originating session's SSE subscribers see the canvas resolve.
/// `op` is validated against the SDK [`SlackCanvasOp`] vocabulary; an unknown /
/// malformed op or a missing `session_id`/`result` is a `400`.
pub(super) async fn canvas_fulfillment_post(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    use ocean_agent_sdk::slack_canvas::SlackCanvasOp;

    // --- session_id: present and parseable ---
    let session_raw = match body.get("session_id").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "missing or empty 'session_id'" })),
            );
        }
    };
    let session_id: AgentSessionId = match serde_json::from_value(json!(session_raw)) {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid 'session_id': {e}") })),
            );
        }
    };

    // --- op: present and a valid SlackCanvasOp ---
    let op_value = match body.get("op") {
        Some(v) => v.clone(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "missing 'op'" })),
            );
        }
    };
    let op: SlackCanvasOp = match serde_json::from_value(op_value) {
        Ok(op) => op,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid 'op': {e}") })),
            );
        }
    };

    // --- result: present (object) ---
    let result = match body.get("result") {
        Some(v) if v.is_object() => v.clone(),
        Some(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "'result' must be an object" })),
            );
        }
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "missing 'result'" })),
            );
        }
    };

    let canvas_key = canvas_fulfillment_key_for_op(&op);
    // Normalized session-id string, formatted identically to the `session_id` the
    // runtime injects into `SessionContext` (both `AgentSessionId::to_string()`),
    // so the runtime registry key (OCEAN-271) matches what the `slack_canvas` tool
    // computes — independent of however the bridge formatted the raw `session_id`.
    let session_key = session_id.to_string();

    // 1. STORE (last-write-wins per (session, canvas key)).
    {
        let mut store = state
            .canvas_fulfillments
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        store.insert(
            (session_id, canvas_key.clone()),
            CanvasFulfillment {
                result: result.clone(),
                received_at: Utc::now(),
            },
        );
    }

    // 2. SSE RE-EMIT — fulfilled result back to the originating session.
    let fulfilled = fulfilled_result_from_bridge(&op, &result);

    // 2b. RUNTIME LOOKUP STORE (OCEAN-271) — feed the same typed fulfilled result
    // into the process-global `CANVAS_FULFILLMENT_REGISTRY` owned by `ocean-runtime`
    // (the daemon depends on the runtime, so supplying the impl is the normal
    // direction — no layering inversion). This is what lets a *subsequent*
    // `slack_canvas` `read`/`list` in the same session return the bridge's fetched
    // content instead of `pending_bridge`. We store under the daemon-identical
    // `(session_key, canvas_key)`; the tool keys its lookup the same way.
    ocean_runtime::tools::slack_canvas::CANVAS_FULFILLMENT_REGISTRY.put(
        session_key,
        canvas_key.clone(),
        fulfilled.clone(),
    );

    state.agent_events.emit(AgentTurnEvent::SlackCanvas {
        session_id,
        // The fulfillment is a standalone relay, not part of a live turn; mint a
        // fresh turn id (the SSE filter routes on `session_id`, not `turn_id`).
        turn_id: AgentTurnId::new_v4(),
        op,
        result: fulfilled,
    });

    tracing::info!(
        session = %session_raw,
        canvas_key = %canvas_key,
        "stored + re-emitted slack_canvas bridge fulfillment"
    );

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "stored": true,
            "session_id": session_raw,
            "canvas_key": canvas_key,
        })),
    )
}

/// Query string for `GET /v1/agent/canvas/fulfill` (OCEAN-262).
#[derive(Debug, serde::Deserialize)]
pub(super) struct CanvasFulfillmentQuery {
    /// The session the fulfillment belongs to (required).
    pub(super) session_id: AgentSessionId,
    /// The canvas key to look up. For `read`/`update`/`append` this is the Slack
    /// `canvas_id`; for `list`/`create` use the synthetic key
    /// (`list:{channel_id}` / `create:{title}`). Either `canvas_id` or `key` is
    /// accepted (alias) — `canvas_id` is the common case the agent knows.
    #[serde(default, alias = "key")]
    pub(super) canvas_id: Option<String>,
}

/// `GET /v1/agent/canvas/fulfill?session_id=&canvas_id=` — read back a stored
/// bridge fulfillment (OCEAN-262).
///
/// Returns the bridge's `result` body verbatim when a fulfillment is stored for
/// `(session_id, canvas_id)`, or `404` when none has arrived yet (the awareness
/// op is still `pending_bridge`). This is the pull-side companion to the SSE
/// re-emit — useful for a client/agent-adjacent poll or for tests.
pub(super) async fn canvas_fulfillment_get(
    State(state): State<AppState>,
    Query(q): Query<CanvasFulfillmentQuery>,
) -> (StatusCode, Json<Value>) {
    let canvas_id = match q.canvas_id {
        Some(c) if !c.trim().is_empty() => c,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "missing 'canvas_id' (or 'key') query param" })),
            );
        }
    };

    let found = {
        let store = state
            .canvas_fulfillments
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        store.get(&(q.session_id, canvas_id.clone())).cloned()
    };

    match found {
        Some(f) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "fulfilled": true,
                "canvas_id": canvas_id,
                "received_at": f.received_at.to_rfc3339(),
                "result": f.result,
            })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "ok": true,
                "fulfilled": false,
                "canvas_id": canvas_id,
                "reason": "no bridge fulfillment stored for this (session, canvas) yet",
            })),
        ),
    }
}
