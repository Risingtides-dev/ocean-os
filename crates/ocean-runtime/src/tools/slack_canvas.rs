//! `slack_canvas` — the agent's Slack-canvas-as-playground tool (OCEAN-214 ph2).
//!
//! Slack canvases become a persistent, **bidirectional** surface the agent owns:
//! structured writes plus a ledger read path. Through this tool the agent can:
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

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ocean_agent_sdk::slack_canvas::{CanvasFetchStatus, SlackCanvasOp, SlackCanvasResult};
use serde_json::{json, Value};

use crate::types::{AgentTool, AgentToolResult, ToolSideEffect};

// ---------------------------------------------------------------------------
// Bridge fulfillment lookup (OCEAN-271) — closing the read loop
// ---------------------------------------------------------------------------
//
// OCEAN-235 made `read`/`list` return an honest *pending* result and forwarded
// the op as a side effect; OCEAN-262 (#183) made the daemon STORE the bridge's
// fulfilled content (in `AppState.canvas_fulfillments`) and re-emit it on SSE.
// But a *second* `read` of the same canvas still returned `pending_bridge`,
// because this tool is a stateless unit struct in `ocean-runtime` and holds no
// handle to the daemon's `AppState` — and threading daemon state into the
// runtime crate would invert the layering (runtime is a dependency of the
// daemon, not vice-versa).
//
// The fix mirrors the `component_wait` precedent exactly: a process-global
// registry **owned by `ocean-runtime`** that both the tool and the daemon
// reach. The tool *reads* it on a `read`/`list`; the daemon *writes* into it
// from `POST /v1/agent/canvas/fulfill` (it already depends on `ocean-runtime`,
// so supplying the impl is the normal direction — no inversion). On a read, if
// a fulfilled result exists for `(session, canvas key)` the tool returns the
// real fetched content (`fulfilled_read`/`fulfilled_list`); otherwise it stays
// honest and returns `pending_bridge`.

/// Process-global store of bridge-fulfilled `slack_canvas` awareness results,
/// owned by `ocean-runtime` and shared with the daemon's
/// `POST /v1/agent/canvas/fulfill` route — the same shape as
/// [`COMPONENT_WAIT_REGISTRY`](crate::tools::component::COMPONENT_WAIT_REGISTRY).
///
/// The daemon depends on `ocean-runtime`, so it writes fulfillments here; the
/// `slack_canvas` tool reads them on a later `read`/`list`. Nothing in
/// `ocean-runtime` depends back on the daemon — the layering stays one-way.
pub static CANVAS_FULFILLMENT_REGISTRY: std::sync::LazyLock<CanvasFulfillmentRegistry> =
    std::sync::LazyLock::new(CanvasFulfillmentRegistry::new);

/// A fulfilled result is only useful until the agent reads it back (reads are
/// non-destructive, so nothing removes the entry on read). Past this age the GC
/// sweep evicts it — generous enough for a read-back within a turn or two, far
/// shorter than the daemon's lifetime so the map can't grow without bound
/// (OCEAN-273). Kept equal to the daemon's `CANVAS_FULFILLMENT_TTL` so both
/// halves of the same `(session, canvas)` slot expire on the same schedule.
pub const CANVAS_FULFILLMENT_TTL: chrono::Duration = chrono::Duration::minutes(30);

/// Hard cap on stored fulfillments (OCEAN-273), mirroring the daemon's
/// `REGISTRY_MAX_ENTRIES`. A TTL bounds age; this bounds a burst of ops that
/// arrive faster than one GC interval. On overflow the oldest entries (by
/// `received_at`) are evicted first.
pub const CANVAS_FULFILLMENT_MAX_ENTRIES: usize = 10_000;

/// One stored fulfillment: the typed result plus when it was inserted, so the GC
/// sweep can evict by age (OCEAN-273). `SlackCanvasResult` itself carries no
/// timestamp, hence the wrapper.
struct StoredFulfillment {
    result: SlackCanvasResult,
    received_at: DateTime<Utc>,
}

/// Registry of fulfilled `slack_canvas` awareness results, keyed by
/// `(session_id, canvas key)`.
///
/// The **canvas key** is identical to the daemon's `canvas_fulfillment_key_for_op`
/// (OCEAN-262), produced here by [`canvas_fulfillment_key_for_op`] so a value the
/// daemon stores under a given key is found by the tool computing the same key:
/// `read`/`update`/`append` key on the Slack `canvas_id`; `list` on
/// `list:{channel_id}`; `create` on `create:{title}`.
///
/// Entries are write-once-read-many (a read never removes them), so without a
/// sweep the map would grow one entry per fulfilled op for the daemon's whole
/// lifetime. [`gc`](Self::gc) — driven from the daemon's registry GC task —
/// bounds it by TTL and a hard cap (OCEAN-273).
#[derive(Default)]
pub struct CanvasFulfillmentRegistry {
    /// Map of `(session_id, canvas key)` → the fulfilled, typed result the bridge
    /// produced (with its insertion time). Last-write-wins per key, matching the
    /// daemon's store semantics.
    fulfilled: Mutex<HashMap<(String, String), StoredFulfillment>>,
}

impl CanvasFulfillmentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a fulfilled result for `(session_id, canvas key)`. Called by the
    /// daemon when the bridge POSTs a fulfillment, so a later `read`/`list` of
    /// the same canvas in the same session surfaces real content. Stamps the
    /// entry with the current time for TTL eviction (OCEAN-273).
    pub fn put(
        &self,
        session_id: impl Into<String>,
        canvas_key: impl Into<String>,
        result: SlackCanvasResult,
    ) {
        self.put_at(session_id, canvas_key, result, Utc::now());
    }

    /// As [`put`](Self::put) but with an explicit insertion time, so GC behaviour
    /// is deterministic in tests (OCEAN-273).
    pub fn put_at(
        &self,
        session_id: impl Into<String>,
        canvas_key: impl Into<String>,
        result: SlackCanvasResult,
        received_at: DateTime<Utc>,
    ) {
        let mut map = self
            .fulfilled
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.insert(
            (session_id.into(), canvas_key.into()),
            StoredFulfillment {
                result,
                received_at,
            },
        );
    }

    /// Look up a fulfilled result for `(session_id, canvas key)`. Returns `None`
    /// when the bridge has not fulfilled this awareness op yet — the tool then
    /// stays honest and returns `pending_bridge`.
    pub fn get(&self, session_id: &str, canvas_key: &str) -> Option<SlackCanvasResult> {
        let map = self
            .fulfilled
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.get(&(session_id.to_string(), canvas_key.to_string()))
            .map(|stored| stored.result.clone())
    }

    /// One GC sweep (OCEAN-273): drop entries older than `ttl`, then enforce the
    /// `max_entries` cap by evicting the oldest (by `received_at`) first. `now` is
    /// injected so the sweep is deterministic in tests, matching the daemon's
    /// `gc_registries(now)` pattern. Driven from the daemon's registry GC task on
    /// the same interval — nothing in `ocean-runtime` self-schedules.
    pub fn gc(&self, now: DateTime<Utc>, ttl: chrono::Duration, max_entries: usize) {
        let mut map = self
            .fulfilled
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.retain(|_, stored| (now - stored.received_at) <= ttl);
        if map.len() <= max_entries {
            return;
        }
        let overflow = map.len() - max_entries;
        // Oldest first: rank by insertion time and drop exactly `overflow` keys.
        let mut ranked: Vec<((String, String), DateTime<Utc>)> = map
            .iter()
            .map(|(k, v)| (k.clone(), v.received_at))
            .collect();
        ranked.sort_by_key(|(_, received_at)| *received_at);
        for (key, _) in ranked.into_iter().take(overflow) {
            map.remove(&key);
        }
    }

    /// Current number of stored fulfillments. Used by the daemon's bounded-growth
    /// test and exposed for observability (OCEAN-273).
    pub fn len(&self) -> usize {
        self.fulfilled
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
    }

    /// Whether the registry holds no fulfillments.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The store key for a `slack_canvas` op — **kept byte-for-byte identical** to the
/// daemon's `canvas_fulfillment_key_for_op` (OCEAN-262) so the daemon's writes and
/// the tool's reads address the same slot. `read`/`update`/`append` key on the
/// Slack `canvas_id`; `list` on `list:{channel_id}`; `create` on `create:{title}`.
pub fn canvas_fulfillment_key_for_op(op: &SlackCanvasOp) -> String {
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

/// The agent's `slack_canvas` tool.
///
/// `session_id` is the authoritative session id injected from the turn's
/// [`SessionContext`](crate::capability::SessionContext) when the tool is built
/// (mirroring `component_wait`, OCEAN-60). It is the session half of the
/// fulfillment-registry key, so a `read`/`list` only ever surfaces content the
/// bridge fulfilled for *this* session — never one a model-supplied id could
/// point at. `None` (ad-hoc/test construction, or a turn with no session) means
/// the tool can't match a stored fulfillment and stays `pending_bridge`.
#[derive(Default)]
pub struct SlackCanvasTool {
    session_id: Option<String>,
}

impl SlackCanvasTool {
    /// Construct with no bound session — awareness ops always return
    /// `pending_bridge` (no session to key a fulfillment lookup on). Used by
    /// `default_tools()` and ad-hoc/test paths.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct bound to a session. A `read`/`list` consults
    /// [`CANVAS_FULFILLMENT_REGISTRY`] under this session id and returns the
    /// bridge's fetched content when present.
    pub fn for_session(session_id: Option<String>) -> Self {
        Self { session_id }
    }

    /// Look up a bridge-fulfilled result for this awareness op in the
    /// process-global [`CANVAS_FULFILLMENT_REGISTRY`], scoped to the tool's bound
    /// session. Returns `None` when no session is bound (so the lookup can't be
    /// scoped) or when the bridge has not fulfilled this `(session, canvas)` yet —
    /// in both cases the caller falls back to the honest `pending_*` result.
    fn lookup_fulfilled(&self, op: &SlackCanvasOp) -> Option<SlackCanvasResult> {
        let session_id = self.session_id.as_deref()?;
        let key = canvas_fulfillment_key_for_op(op);
        CANVAS_FULFILLMENT_REGISTRY.get(session_id, &key)
    }
}

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
        // OCEAN-235: the runtime cannot itself *fetch* live Slack canvas content —
        // it holds no Slack token and no Slack API client (by design: all Slack
        // I/O lives in the `ocean-agents` Python bridge transport). So for the
        // awareness ops (`read`/`list`) the runtime emits an **honest** pending
        // result and forwards the op onto the event bus; the Slack bridge fetches
        // the live content and stamps it back through
        // `SlackCanvasResult::fulfilled_read` / `fulfilled_list`.
        //
        // OCEAN-271: but once the bridge HAS fulfilled an awareness op, a later
        // `read`/`list` of the same canvas should surface that fetched content
        // instead of pending forever. The daemon stores each fulfillment in the
        // process-global `CANVAS_FULFILLMENT_REGISTRY` (owned by this crate). So
        // here, before falling back to pending, we consult that registry under the
        // injected session id + the daemon-identical canvas key: a hit returns the
        // bridge's real `fulfilled_read`/`fulfilled_list` result; a miss stays
        // honest.
        //
        // Crucially, a *pending* `read` still carries **no** `contents` (not an
        // empty string) and is marked `CanvasFetchStatus::PendingBridge`, so the
        // agent can never mistake an un-fulfilled read for a genuinely empty canvas.
        let fulfilled = self.lookup_fulfilled(&op);
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
            SlackCanvasOp::Read { canvas_id } => fulfilled
                .filter(|f| f.fetch_status.is_fetched())
                .unwrap_or_else(|| SlackCanvasResult::pending_read(canvas_id.clone())),
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
            SlackCanvasOp::List { .. } => fulfilled
                .filter(|f| f.fetch_status.is_fetched())
                .unwrap_or_else(SlackCanvasResult::pending_list),
        };

        let summary = match &op {
            SlackCanvasOp::Create { title, .. } => match title {
                Some(t) => format!("queued create of Slack canvas '{t}'"),
                None => "queued create of a new Slack canvas".to_string(),
            },
            SlackCanvasOp::Read { canvas_id } => {
                // OCEAN-271: report the fetched body when the bridge has already
                // fulfilled this read, so the model's summary line matches the
                // `contents` it can now see — not a misleading "requested".
                if result.fetch_status.is_fetched() {
                    format!("fetched current contents of Slack canvas '{canvas_id}'")
                } else {
                    format!("read-back requested for Slack canvas '{canvas_id}'")
                }
            }
            SlackCanvasOp::Update {
                canvas_id, mode, ..
            } => {
                format!("queued {mode:?} update of Slack canvas '{canvas_id}'")
            }
            SlackCanvasOp::Append { canvas_id, .. } => {
                format!("queued append to Slack canvas '{canvas_id}'")
            }
            SlackCanvasOp::List { channel_id } => {
                if result.fetch_status.is_fetched() {
                    let n = result.canvases.as_ref().map(|c| c.len()).unwrap_or(0);
                    format!("listed {n} Slack canvas(es) in channel '{channel_id}'")
                } else {
                    format!("listing Slack canvases in channel '{channel_id}'")
                }
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
    use ocean_agent_sdk::slack_canvas::{SlackCanvasId, SlackCanvasSummary};

    /// A valid `create` (title + markdown) is accepted, the result echoes the op,
    /// and the side effect carries the parsed op.
    #[tokio::test]
    async fn slack_canvas_accepts_create() {
        let tool = SlackCanvasTool::new();
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
        let tool = SlackCanvasTool::new();
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
        let tool = SlackCanvasTool::new();
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
        let tool = SlackCanvasTool::new();
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
        let tool = SlackCanvasTool::new();
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
        let tool = SlackCanvasTool::new();
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
        let tool = SlackCanvasTool::new();
        for args in [
            json!({ "op": "create", "title": "T" }),
            json!({ "op": "update", "canvas_id": "F1", "markdown": "x" }),
            json!({ "op": "append", "canvas_id": "F1", "markdown": "x" }),
        ] {
            let op = args["op"].as_str().unwrap().to_string();
            let res = tool
                .execute("call-na", args)
                .await
                .expect("valid mutating op");
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
        let tool = SlackCanvasTool::new();
        let err = tool
            .execute("call-7", json!({ "canvas_id": "F1" }))
            .await
            .expect_err("missing op must be rejected");
        assert!(err.contains("op"), "unexpected error: {err}");
    }

    /// An unknown op is rejected.
    #[tokio::test]
    async fn slack_canvas_rejects_unknown_op() {
        let tool = SlackCanvasTool::new();
        let err = tool
            .execute("call-8", json!({ "op": "obliterate" }))
            .await
            .expect_err("unknown op must be rejected");
        assert!(err.contains("obliterate"), "unexpected error: {err}");
    }

    /// `read` without `canvas_id` is rejected (serde enforces the required field).
    #[tokio::test]
    async fn slack_canvas_rejects_read_without_canvas_id() {
        let tool = SlackCanvasTool::new();
        let err = tool
            .execute("call-9", json!({ "op": "read" }))
            .await
            .expect_err("read without canvas_id must be rejected");
        assert!(err.contains("read"), "unexpected error: {err}");
    }

    /// `update` without `canvas_id` is rejected.
    #[tokio::test]
    async fn slack_canvas_rejects_update_without_canvas_id() {
        let tool = SlackCanvasTool::new();
        let err = tool
            .execute("call-10", json!({ "op": "update", "markdown": "x" }))
            .await
            .expect_err("update without canvas_id must be rejected");
        assert!(err.contains("update"), "unexpected error: {err}");
    }

    /// `update`/`append` with empty markdown is rejected.
    #[tokio::test]
    async fn slack_canvas_rejects_empty_markdown() {
        let tool = SlackCanvasTool::new();
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
        let tool = SlackCanvasTool::new();
        let err = tool
            .execute("call-12", json!({ "op": "list" }))
            .await
            .expect_err("list without channel_id must be rejected");
        assert!(err.contains("list"), "unexpected error: {err}");
    }

    /// The tool is permission-gated (mutating ops dominate).
    #[test]
    fn slack_canvas_requires_permission() {
        assert!(SlackCanvasTool::new().requires_permission());
    }

    // -----------------------------------------------------------------------
    // OCEAN-271: a read returns bridge-fulfilled content on the next read.
    //
    // The daemon stores fulfilled results in the process-global
    // CANVAS_FULFILLMENT_REGISTRY keyed by (session, canvas key). A
    // session-bound tool consults it on a read/list. These tests use **unique
    // session ids per test** because the registry is process-global and tests
    // run concurrently.
    // -----------------------------------------------------------------------

    /// The canvas key the tool computes matches the documented daemon scheme.
    #[test]
    fn canvas_key_matches_daemon_scheme() {
        assert_eq!(
            canvas_fulfillment_key_for_op(&SlackCanvasOp::Read {
                canvas_id: "F0123ABCD".into()
            }),
            "F0123ABCD"
        );
        assert_eq!(
            canvas_fulfillment_key_for_op(&SlackCanvasOp::List {
                channel_id: "C9".into()
            }),
            "list:C9"
        );
        assert_eq!(
            canvas_fulfillment_key_for_op(&SlackCanvasOp::Create {
                title: Some("Plan".into()),
                markdown: None,
                channel_id: None,
            }),
            "create:Plan"
        );
    }

    /// THE CORE OCEAN-271 GUARANTEE. With a fulfillment stored for
    /// `(session, canvas_id)` — as the daemon does when the bridge POSTs back —
    /// a session-bound `read` returns the **real fetched content**
    /// (`fetch_status: fetched`, `bridged: true`, `contents` present), not
    /// `pending_bridge`.
    #[tokio::test]
    async fn read_returns_fulfilled_content_when_bridge_has_fulfilled() {
        let session = "sess-271-read-fulfilled";
        let canvas_id = SlackCanvasId::new("F_FULFILLED_1");

        // The daemon would have written this when the bridge POSTed its fetch.
        CANVAS_FULFILLMENT_REGISTRY.put(
            session,
            canvas_fulfillment_key_for_op(&SlackCanvasOp::Read {
                canvas_id: canvas_id.clone(),
            }),
            SlackCanvasResult::fulfilled_read(
                canvas_id.clone(),
                "# Real canvas body\n- fetched from Slack",
                json!({ "slack_file_id": "F_FULFILLED_1", "revision": 7 }),
            ),
        );

        let tool = SlackCanvasTool::for_session(Some(session.to_string()));
        let res = tool
            .execute(
                "call-271-a",
                json!({ "op": "read", "canvas_id": "F_FULFILLED_1" }),
            )
            .await
            .expect("valid read");

        // Real content surfaced — the loop is closed.
        assert_eq!(res.details["op"], "read");
        assert_eq!(res.details["canvas_id"], "F_FULFILLED_1");
        assert_eq!(res.details["fetch_status"], "fetched");
        assert_eq!(res.details["bridged"], true);
        assert_eq!(
            res.details["contents"],
            "# Real canvas body\n- fetched from Slack"
        );
        // Bridge passthrough metadata rode along.
        assert_eq!(res.details["metadata"]["revision"], 7);
        // The summary reflects the fetched state, not "requested".
        let summary = res.content[0].as_text().unwrap_or_default();
        assert!(
            summary.contains("fetched"),
            "summary should report fetched content: {summary}"
        );

        // The op is still forwarded as a side effect (idempotent, harmless).
        assert_eq!(res.side_effects.len(), 1);
    }

    /// The honest-pending path is preserved: a session-bound `read` with **no**
    /// stored fulfillment still returns `pending_bridge` and fabricates no
    /// `contents` — exactly the OCEAN-235 contract.
    #[tokio::test]
    async fn read_stays_pending_when_no_fulfillment_stored() {
        let session = "sess-271-read-pending";
        let tool = SlackCanvasTool::for_session(Some(session.to_string()));
        let res = tool
            .execute(
                "call-271-b",
                json!({ "op": "read", "canvas_id": "F_NEVER_FETCHED" }),
            )
            .await
            .expect("valid read");

        assert_eq!(res.details["fetch_status"], "pending_bridge");
        assert_eq!(res.details["bridged"], false);
        assert!(
            res.details.get("contents").is_none(),
            "an unfulfilled read must not fabricate contents: {}",
            res.details
        );
    }

    /// A fulfillment is scoped to its session: the same canvas id under a
    /// *different* session does not leak a fulfilled result. This is why the
    /// session id is injected (never model-supplied) — content can't cross
    /// sessions.
    #[tokio::test]
    async fn fulfillment_is_scoped_to_its_session() {
        let canvas_id = SlackCanvasId::new("F_SCOPED");
        CANVAS_FULFILLMENT_REGISTRY.put(
            "sess-271-owner",
            canvas_fulfillment_key_for_op(&SlackCanvasOp::Read {
                canvas_id: canvas_id.clone(),
            }),
            SlackCanvasResult::fulfilled_read(canvas_id, "secret body", Value::Null),
        );

        // A different session reading the same canvas id sees only pending.
        let tool = SlackCanvasTool::for_session(Some("sess-271-other".to_string()));
        let res = tool
            .execute(
                "call-271-c",
                json!({ "op": "read", "canvas_id": "F_SCOPED" }),
            )
            .await
            .expect("valid read");
        assert_eq!(res.details["fetch_status"], "pending_bridge");
        assert!(res.details.get("contents").is_none());
    }

    /// An *unbound* tool (no session — the `default_tools()` / test path) can't
    /// scope a lookup, so it always returns `pending_bridge` even if a
    /// fulfillment exists under some session.
    #[tokio::test]
    async fn unbound_tool_cannot_match_a_fulfillment() {
        let canvas_id = SlackCanvasId::new("F_UNBOUND");
        CANVAS_FULFILLMENT_REGISTRY.put(
            "sess-271-somewhere",
            canvas_fulfillment_key_for_op(&SlackCanvasOp::Read {
                canvas_id: canvas_id.clone(),
            }),
            SlackCanvasResult::fulfilled_read(canvas_id, "body", Value::Null),
        );

        let tool = SlackCanvasTool::new(); // unbound
        let res = tool
            .execute(
                "call-271-d",
                json!({ "op": "read", "canvas_id": "F_UNBOUND" }),
            )
            .await
            .expect("valid read");
        assert_eq!(res.details["fetch_status"], "pending_bridge");
        assert!(res.details.get("contents").is_none());
    }

    /// A `list` is fulfilled the same way as a `read`: with a stored
    /// `fulfilled_list` under `list:{channel_id}`, a session-bound `list`
    /// returns the resolved canvases instead of `pending_bridge`.
    #[tokio::test]
    async fn list_returns_fulfilled_canvases_when_stored() {
        let session = "sess-271-list-fulfilled";
        CANVAS_FULFILLMENT_REGISTRY.put(
            session,
            canvas_fulfillment_key_for_op(&SlackCanvasOp::List {
                channel_id: "C_LIST".into(),
            }),
            SlackCanvasResult::fulfilled_list(
                vec![SlackCanvasSummary {
                    canvas_id: SlackCanvasId::new("F9"),
                    title: Some("Plan".into()),
                }],
                Value::Null,
            ),
        );

        let tool = SlackCanvasTool::for_session(Some(session.to_string()));
        let res = tool
            .execute(
                "call-271-e",
                json!({ "op": "list", "channel_id": "C_LIST" }),
            )
            .await
            .expect("valid list");

        assert_eq!(res.details["fetch_status"], "fetched");
        assert_eq!(res.details["bridged"], true);
        let canvases = res.details["canvases"]
            .as_array()
            .expect("fulfilled list carries canvases");
        assert_eq!(canvases.len(), 1);
        assert_eq!(canvases[0]["canvas_id"], "F9");
    }

    // -----------------------------------------------------------------------
    // OCEAN-273: the process-global registry is bounded by TTL + a hard cap.
    //
    // These exercise `gc` against a *local* `CanvasFulfillmentRegistry` (not the
    // process-global static), so they're isolated from other concurrently-running
    // tests that write into `CANVAS_FULFILLMENT_REGISTRY`.
    // -----------------------------------------------------------------------

    fn fulfilled(body: &str) -> SlackCanvasResult {
        SlackCanvasResult::fulfilled_read(SlackCanvasId::new("F_GC"), body, Value::Null)
    }

    /// A sweep drops entries past the TTL and keeps fresh ones — by age alone,
    /// since a fulfillment has no terminal state (reads don't consume it).
    #[test]
    fn gc_evicts_entries_past_ttl_keeps_fresh() {
        let reg = CanvasFulfillmentRegistry::new();
        let now = Utc::now();
        reg.put_at(
            "sess",
            "F_OLD",
            fulfilled("stale"),
            now - CANVAS_FULFILLMENT_TTL - chrono::Duration::seconds(1),
        );
        reg.put_at(
            "sess",
            "F_FRESH",
            fulfilled("fresh"),
            now - chrono::Duration::minutes(1),
        );

        reg.gc(now, CANVAS_FULFILLMENT_TTL, CANVAS_FULFILLMENT_MAX_ENTRIES);

        assert!(
            reg.get("sess", "F_OLD").is_none(),
            "entry older than the TTL must be evicted"
        );
        assert!(
            reg.get("sess", "F_FRESH").is_some(),
            "a fresh fulfillment survives the sweep and stays readable"
        );
        assert_eq!(reg.len(), 1);
    }

    /// An entry exactly at the TTL boundary is kept (eviction is strictly
    /// older-than), so a read-back right at the edge still resolves.
    #[test]
    fn gc_keeps_entry_exactly_at_ttl_boundary() {
        let reg = CanvasFulfillmentRegistry::new();
        let now = Utc::now();
        reg.put_at(
            "sess",
            "F_EDGE",
            fulfilled("edge"),
            now - CANVAS_FULFILLMENT_TTL,
        );
        reg.gc(now, CANVAS_FULFILLMENT_TTL, CANVAS_FULFILLMENT_MAX_ENTRIES);
        assert!(
            reg.get("sess", "F_EDGE").is_some(),
            "boundary entry retained"
        );
    }

    /// The hard cap bounds a burst that arrives within a single TTL window:
    /// with a cap of 2 and three fresh entries, the oldest is evicted first.
    #[test]
    fn gc_cap_evicts_oldest_first_under_burst() {
        let reg = CanvasFulfillmentRegistry::new();
        let now = Utc::now();
        // All within TTL, so only the cap can evict. Distinct ages so "oldest" is
        // unambiguous.
        reg.put_at(
            "sess",
            "F_OLDEST",
            fulfilled("a"),
            now - chrono::Duration::minutes(3),
        );
        reg.put_at(
            "sess",
            "F_MID",
            fulfilled("b"),
            now - chrono::Duration::minutes(2),
        );
        reg.put_at(
            "sess",
            "F_NEWEST",
            fulfilled("c"),
            now - chrono::Duration::minutes(1),
        );

        reg.gc(now, CANVAS_FULFILLMENT_TTL, 2);

        assert_eq!(reg.len(), 2, "cap holds the map at max_entries");
        assert!(
            reg.get("sess", "F_OLDEST").is_none(),
            "oldest evicted by the cap"
        );
        assert!(reg.get("sess", "F_MID").is_some());
        assert!(reg.get("sess", "F_NEWEST").is_some());
    }

    /// Repeated sweeps keep the map bounded under sustained churn: 5x the cap of
    /// fresh ops, swept once, lands exactly at the cap (not unbounded).
    #[test]
    fn gc_bounds_growth_under_many_ops() {
        let reg = CanvasFulfillmentRegistry::new();
        let now = Utc::now();
        let cap = 50usize;
        for i in 0..(cap * 5) {
            // Staggered, all within TTL, unique keys.
            reg.put_at(
                "sess",
                format!("F{i}"),
                fulfilled("x"),
                now - chrono::Duration::seconds(i as i64),
            );
        }
        assert_eq!(reg.len(), cap * 5, "all inserted before any sweep");

        reg.gc(now, CANVAS_FULFILLMENT_TTL, cap);
        assert_eq!(reg.len(), cap, "sweep bounds the map to the cap");
    }
}
