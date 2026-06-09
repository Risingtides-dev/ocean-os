//! Ocean **Slack canvas protocol** — the shared wire contract for the agent's
//! Slack-canvas-as-playground tool (OCEAN-214).
//!
//! This is the typed, serde-stable vocabulary for the `slack_canvas` runtime tool
//! (Phase 2). It mirrors the role [`crate::surface`] plays for the GPUI canvas:
//! a small set of structured **ops** the agent emits to drive a persistent,
//! bidirectional surface — except the surface here is a Slack Canvas.
//!
//! Two independent consumers must agree on these types:
//!
//! - the **runtime `slack_canvas` tool** (Phase 2, this slice), which an agent
//!   calls to create/read/update/append/list canvases, and
//! - the **Slack canvas bridge** (`ocean-agents`, a later phase), which round-trips
//!   each op to the real Slack Canvas API (`canvases.create`, `canvases.edit`,
//!   `canvases.access.*`) and populates read results with real contents.
//!
//! # Bidirectional by design
//!
//! Like the GPUI canvas (`surface_patch` to write + a ledger to read back), the
//! Slack canvas is meant to be a surface the agent *owns*: it can create canvases,
//! mutate them (`update`/`append`), and **read back** their current contents
//! ([`SlackCanvasOp::Read`]) to reason over what they hold. The read op's result
//! ([`SlackCanvasResult::contents`]) is the awareness channel — Phase 2 contracts
//! its shape; the bridge fills it with live Slack content later.
//!
//! # Wire contract rules
//!
//! - Identifiers are string-backed `serde(transparent)` newtypes. On the wire a
//!   [`SlackCanvasId`] is just `"F0123ABCD"`, never `{ "0": "F0123ABCD" }`.
//! - [`SlackCanvasOp`] is **internally tagged** on `"op"` with `snake_case`
//!   rename, so `{ "op": "create", "title": "…", "markdown": "…" }` deserializes
//!   directly.
//! - Free-form fields use `serde_json::Value` so richer producers' unknown fields
//!   survive a roundtrip untouched.
//!
//! # Bridge fetch contract (OCEAN-235) — what each side owns
//!
//! Reading a canvas's live contents spans **two repos**, because all Slack I/O
//! (and the Slack token) lives in the `ocean-agents` Python bridge, not in this
//! runtime. The split:
//!
//! - **This runtime (ocean-os) owns the seam.** The `slack_canvas` tool returns an
//!   honest *pending* result for `read`/`list` ([`SlackCanvasResult::pending_read`]
//!   / [`SlackCanvasResult::pending_list`]) — `fetch_status: pending_bridge`, no
//!   fabricated content — and emits the op as a `ToolSideEffect`. The daemon relays
//!   it as `AgentTurnEvent::SlackCanvas` over `/v1/agent/events`, scoped to the
//!   session. The typed fulfillment constructors
//!   ([`SlackCanvasResult::fulfilled_read`] / [`SlackCanvasResult::fulfilled_list`])
//!   are the entry points the bridge stamps live content into.
//!
//! - **The `ocean-agents` bridge must still provide the fetch.** To complete a
//!   `read`/`list` it must: (1) consume the `slack_canvas` event from the daemon
//!   SSE stream, (2) call the Slack API for the live canvas body — note the
//!   transport (`couriers/transport/slack.py`) currently has only `create_canvas`,
//!   so a **read method is new work there** (resolve the canvas file via
//!   `files.info`/lookup and pull its markdown), and (3) surface the fetched
//!   content back to the agent as a fulfilled [`SlackCanvasResult`]. Until the
//!   bridge ships that, the agent correctly sees `pending_bridge`, never a fake
//!   empty canvas.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Identifiers — string-backed, transparent on the wire
// ---------------------------------------------------------------------------

/// Macro to define a string-backed, serde-transparent newtype with the small set
/// of ergonomic conversions every Ocean id wants. (Local copy mirroring
/// [`crate::surface`] so this module stays self-contained.)
macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Construct from anything string-like.
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            /// Borrow the underlying string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }
            /// Consume into the owned `String`.
            pub fn into_inner(self) -> String {
                self.0
            }
        }

        impl From<String> for $name {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(
    /// Identifies a *Slack canvas* (Slack's `canvas_id`, e.g. `F0123ABCD`).
    SlackCanvasId
);
string_id!(
    /// Identifies a *Slack channel* a canvas may be created in / scoped to
    /// (e.g. `C0123ABCD`).
    SlackChannelId
);

// ---------------------------------------------------------------------------
// Edit mode for update/append
// ---------------------------------------------------------------------------

/// How an [`SlackCanvasOp::Update`] applies its markdown to the existing canvas.
/// Maps onto Slack's `canvases.edit` change-set semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CanvasEditMode {
    /// Replace the entire canvas body with the supplied markdown.
    #[default]
    Replace,
    /// Append the supplied markdown to the end of the canvas.
    Append,
    /// Prepend the supplied markdown to the start of the canvas.
    Prepend,
}

// ---------------------------------------------------------------------------
// Slack canvas operation
// ---------------------------------------------------------------------------

/// A single structured operation the agent emits to drive a Slack canvas.
///
/// Internally tagged on `"op"` with `snake_case` discriminants, so the minimal
/// JSON shape `{ "op": "create", "title": "Plan", "markdown": "# Plan" }`
/// deserializes directly into [`SlackCanvasOp::Create`].
///
/// `read` and `list` are **non-mutating** (the awareness side); `create`,
/// `update`, and `append` **mutate** the canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SlackCanvasOp {
    /// Create a new canvas, optionally with an initial title + markdown body, and
    /// optionally scoped to a channel. The bridge calls `canvases.create`.
    Create {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        markdown: Option<String>,
        /// Channel to attach/scope the canvas to, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        channel_id: Option<SlackChannelId>,
    },
    /// Read back the current contents of an existing canvas — the **awareness**
    /// op. The bridge calls `canvases.read` (or fetches the canvas file) and
    /// populates [`SlackCanvasResult::contents`] with the live markdown so the
    /// agent can reason over what the canvas currently holds.
    Read { canvas_id: SlackCanvasId },
    /// Modify an existing canvas. `mode` controls replace/append/prepend; the
    /// bridge calls `canvases.edit`.
    Update {
        canvas_id: SlackCanvasId,
        markdown: String,
        #[serde(default)]
        mode: CanvasEditMode,
    },
    /// Convenience mutation: append markdown to the end of an existing canvas.
    /// Equivalent to [`SlackCanvasOp::Update`] with [`CanvasEditMode::Append`],
    /// surfaced as its own op so the agent can express intent directly.
    Append {
        canvas_id: SlackCanvasId,
        markdown: String,
    },
    /// List the canvases visible in a channel (non-mutating). The bridge resolves
    /// this against the channel's canvas/file listing.
    List { channel_id: SlackChannelId },
}

impl SlackCanvasOp {
    /// The `op` discriminants accepted by the tool, mirrored into the JSON schema
    /// so the model sees the closed set up front.
    pub const VALID_OPS: &'static [&'static str] =
        &["create", "read", "update", "append", "list"];

    /// Whether this op mutates the canvas. `create`/`update`/`append` mutate;
    /// `read`/`list` are read-only (the awareness side). The runtime uses this to
    /// inform permission-gating decisions.
    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            SlackCanvasOp::Create { .. }
                | SlackCanvasOp::Update { .. }
                | SlackCanvasOp::Append { .. }
        )
    }

    /// The `op` discriminant as it appears on the wire (`snake_case`).
    pub fn op_name(&self) -> &'static str {
        match self {
            SlackCanvasOp::Create { .. } => "create",
            SlackCanvasOp::Read { .. } => "read",
            SlackCanvasOp::Update { .. } => "update",
            SlackCanvasOp::Append { .. } => "append",
            SlackCanvasOp::List { .. } => "list",
        }
    }
}

// ---------------------------------------------------------------------------
// Result
// ---------------------------------------------------------------------------

/// A canvas entry in a [`SlackCanvasOp::List`] result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlackCanvasSummary {
    pub canvas_id: SlackCanvasId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Fulfillment state of a `read`/`list` awareness op — whether the Slack bridge
/// has actually fetched live content yet.
///
/// This is the **honesty marker** the agent reasons over. The critical case is a
/// `read` whose bridge fetch has not completed: the result must *not* hand the
/// agent an empty string (indistinguishable from "the canvas is genuinely
/// empty"). Instead [`SlackCanvasResult::contents`] is left `None` and the status
/// is [`CanvasFetchStatus::PendingBridge`], so the agent knows it is looking at an
/// un-fulfilled awareness op rather than real, empty content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CanvasFetchStatus {
    /// Not an awareness op (`create`/`update`/`append`) — fetch status is N/A.
    #[default]
    NotApplicable,
    /// An awareness op (`read`/`list`) whose live fetch the Slack bridge has not
    /// fulfilled yet. The runtime emits the op onto the event bus and returns this
    /// status synchronously; `contents`/`canvases` carry **no** live data. The
    /// agent must treat the awareness payload as *unknown*, not as empty.
    PendingBridge,
    /// The bridge fetched live Slack content and stamped it into this result.
    /// `contents` (for `read`) / `canvases` (for `list`) are authoritative.
    Fetched,
}

impl CanvasFetchStatus {
    /// Whether live content has actually been fetched. `false` for both
    /// [`Self::NotApplicable`] and [`Self::PendingBridge`].
    pub fn is_fetched(&self) -> bool {
        matches!(self, CanvasFetchStatus::Fetched)
    }
}

/// Structured result returned by the `slack_canvas` tool, echoing which op ran and
/// carrying the data the agent reasons over.
///
/// The runtime emits a **well-formed contract** result so the agent loop and tests
/// work end-to-end. For the mutating ops (`create`/`update`/`append`) that result
/// is complete on its own. For the **awareness** ops (`read`/`list`) the live
/// Slack content is fetched by the Slack bridge (`ocean-agents`), which round-trips
/// the op to the Slack Canvas API and stamps the content back in via
/// [`SlackCanvasResult::fulfilled_read`] / [`SlackCanvasResult::fulfilled_list`].
///
/// Until that fetch lands, an awareness result is marked
/// [`CanvasFetchStatus::PendingBridge`] and carries **no** fabricated content —
/// the agent is told plainly that the read is not yet fulfilled rather than being
/// handed an empty string that looks like a genuinely empty canvas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlackCanvasResult {
    pub ok: bool,
    /// Which op ran (`create`/`read`/`update`/`append`/`list`).
    pub op: String,
    /// The canvas this op targeted, when it names a single canvas. `None` for
    /// `list`, and for `create` until the bridge mints the real id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas_id: Option<SlackCanvasId>,
    /// **Awareness payload** for `read`: the live markdown contents of the canvas
    /// the agent reasons over. `None` for non-read ops **and** for a `read` whose
    /// bridge fetch is still [`CanvasFetchStatus::PendingBridge`] — absence here is
    /// deliberate, so an unfulfilled read is never mistaken for an empty canvas.
    /// `Some(..)` only once the bridge has fetched real content
    /// ([`CanvasFetchStatus::Fetched`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<String>,
    /// Result of a `list` op: the canvases visible in the channel. `None` for
    /// non-list ops and for a `list` still pending the bridge fetch; `Some(..)`
    /// (possibly empty) once the bridge has resolved the channel listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvases: Option<Vec<SlackCanvasSummary>>,
    /// Fetch state of an awareness op — the honesty marker the agent reads.
    /// [`CanvasFetchStatus::PendingBridge`] for an un-fulfilled `read`/`list`,
    /// [`CanvasFetchStatus::Fetched`] once the bridge stamps live content, and
    /// [`CanvasFetchStatus::NotApplicable`] for the mutating ops.
    #[serde(default)]
    pub fetch_status: CanvasFetchStatus,
    /// Whether the bridge round-trip to the Slack API has happened for this result.
    /// `false` when the runtime emits the contracted op the bridge has not yet
    /// fulfilled; the bridge flips it to `true` when it stamps live content back in.
    pub bridged: bool,
    /// Free-form passthrough metadata, reserved for the bridge.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
}

impl SlackCanvasResult {
    /// The contracted result the **runtime** returns for an awareness `read` before
    /// the bridge has fetched anything: `contents` is `None`, the fetch is marked
    /// [`CanvasFetchStatus::PendingBridge`], and `bridged` is `false`. This is the
    /// honest placeholder — the agent sees an explicitly *unfulfilled* read, not an
    /// empty canvas.
    pub fn pending_read(canvas_id: SlackCanvasId) -> Self {
        Self {
            ok: true,
            op: "read".to_string(),
            canvas_id: Some(canvas_id),
            contents: None,
            canvases: None,
            fetch_status: CanvasFetchStatus::PendingBridge,
            bridged: false,
            metadata: Value::Null,
        }
    }

    /// The contracted result the **runtime** returns for a `list` before the bridge
    /// has resolved the channel's canvases: `canvases` is `None`, the fetch is
    /// [`CanvasFetchStatus::PendingBridge`], and `bridged` is `false`.
    pub fn pending_list() -> Self {
        Self {
            ok: true,
            op: "list".to_string(),
            canvas_id: None,
            contents: None,
            canvases: None,
            fetch_status: CanvasFetchStatus::PendingBridge,
            bridged: false,
            metadata: Value::Null,
        }
    }

    /// The fulfillment seam for a `read`: the **bridge** calls this with the live
    /// markdown it fetched from Slack to turn a [`Self::pending_read`] into an
    /// authoritative awareness result — `contents` populated,
    /// [`CanvasFetchStatus::Fetched`], `bridged: true`.
    ///
    /// This is the typed entry point the `ocean-agents` Slack bridge fills; the
    /// runtime defines it so the seam is real and the content flows through the
    /// moment the bridge provides it. `metadata` carries any bridge passthrough
    /// (e.g. Slack file/revision ids); pass [`Value::Null`] for none.
    pub fn fulfilled_read(
        canvas_id: SlackCanvasId,
        contents: impl Into<String>,
        metadata: Value,
    ) -> Self {
        Self {
            ok: true,
            op: "read".to_string(),
            canvas_id: Some(canvas_id),
            contents: Some(contents.into()),
            canvases: None,
            fetch_status: CanvasFetchStatus::Fetched,
            bridged: true,
            metadata,
        }
    }

    /// The fulfillment seam for a `list`: the **bridge** calls this with the
    /// canvases it resolved for the channel, turning a [`Self::pending_list`] into
    /// an authoritative listing — [`CanvasFetchStatus::Fetched`], `bridged: true`.
    pub fn fulfilled_list(canvases: Vec<SlackCanvasSummary>, metadata: Value) -> Self {
        Self {
            ok: true,
            op: "list".to_string(),
            canvas_id: None,
            contents: None,
            canvases: Some(canvases),
            fetch_status: CanvasFetchStatus::Fetched,
            bridged: true,
            metadata,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The minimal `create` JSON deserializes into `SlackCanvasOp::Create`.
    #[test]
    fn deserializes_minimal_create() {
        let raw = json!({
            "op": "create",
            "title": "Campaign Plan",
            "markdown": "# Plan\n- step one"
        });
        let op: SlackCanvasOp = serde_json::from_value(raw).expect("create deserializes");
        let SlackCanvasOp::Create {
            title,
            markdown,
            channel_id,
        } = op
        else {
            panic!("expected Create");
        };
        assert_eq!(title.as_deref(), Some("Campaign Plan"));
        assert_eq!(markdown.as_deref(), Some("# Plan\n- step one"));
        assert!(channel_id.is_none());
    }

    /// A bare `create` (no title/markdown/channel) is valid — the app can fill in.
    #[test]
    fn create_allows_empty_body() {
        let op: SlackCanvasOp =
            serde_json::from_value(json!({ "op": "create" })).expect("bare create ok");
        assert!(matches!(op, SlackCanvasOp::Create { .. }));
        assert!(op.is_mutating());
    }

    /// `read` carries just the canvas id and is non-mutating (the awareness op).
    #[test]
    fn read_is_non_mutating() {
        let op: SlackCanvasOp =
            serde_json::from_value(json!({ "op": "read", "canvas_id": "F0123ABCD" }))
                .expect("read deserializes");
        let SlackCanvasOp::Read { canvas_id } = &op else {
            panic!("expected Read");
        };
        assert_eq!(canvas_id.as_str(), "F0123ABCD");
        assert!(!op.is_mutating());
    }

    /// `update` defaults its mode to `replace`.
    #[test]
    fn update_mode_defaults_to_replace() {
        let op: SlackCanvasOp = serde_json::from_value(json!({
            "op": "update",
            "canvas_id": "F1",
            "markdown": "new body"
        }))
        .expect("update deserializes");
        let SlackCanvasOp::Update { mode, .. } = op else {
            panic!("expected Update");
        };
        assert_eq!(mode, CanvasEditMode::Replace);
    }

    /// Every op roundtrips through its snake_case discriminant.
    #[test]
    fn all_ops_roundtrip_snake_case() {
        let cases = vec![
            (
                SlackCanvasOp::Create {
                    title: Some("t".into()),
                    markdown: None,
                    channel_id: None,
                },
                "create",
            ),
            (
                SlackCanvasOp::Read {
                    canvas_id: SlackCanvasId::new("F1"),
                },
                "read",
            ),
            (
                SlackCanvasOp::Update {
                    canvas_id: SlackCanvasId::new("F1"),
                    markdown: "x".into(),
                    mode: CanvasEditMode::Append,
                },
                "update",
            ),
            (
                SlackCanvasOp::Append {
                    canvas_id: SlackCanvasId::new("F1"),
                    markdown: "x".into(),
                },
                "append",
            ),
            (
                SlackCanvasOp::List {
                    channel_id: SlackChannelId::new("C1"),
                },
                "list",
            ),
        ];
        for (op, name) in cases {
            let v = serde_json::to_value(&op).unwrap();
            assert_eq!(v["op"], name, "op tag mismatch for {op:?}");
            assert_eq!(op.op_name(), name);
            let back: SlackCanvasOp = serde_json::from_value(v).unwrap();
            assert_eq!(back, op, "roundtrip mismatch for {name}");
        }
    }

    /// `is_mutating` splits the ops correctly.
    #[test]
    fn mutating_split_is_correct() {
        assert!(SlackCanvasOp::Create {
            title: None,
            markdown: None,
            channel_id: None
        }
        .is_mutating());
        assert!(SlackCanvasOp::Update {
            canvas_id: SlackCanvasId::new("F1"),
            markdown: "x".into(),
            mode: CanvasEditMode::Replace
        }
        .is_mutating());
        assert!(SlackCanvasOp::Append {
            canvas_id: SlackCanvasId::new("F1"),
            markdown: "x".into()
        }
        .is_mutating());
        assert!(!SlackCanvasOp::Read {
            canvas_id: SlackCanvasId::new("F1")
        }
        .is_mutating());
        assert!(!SlackCanvasOp::List {
            channel_id: SlackChannelId::new("C1")
        }
        .is_mutating());
    }

    /// Ids are transparent strings on the wire.
    #[test]
    fn ids_are_transparent_strings() {
        let id = SlackCanvasId::new("F0123ABCD");
        assert_eq!(serde_json::to_value(&id).unwrap(), json!("F0123ABCD"));
        let back: SlackCanvasId = serde_json::from_value(json!("F999")).unwrap();
        assert_eq!(back, SlackCanvasId::new("F999"));
    }

    /// The result serializes to the contracted shape; `None` fields are omitted.
    /// `fetch_status` is always present (the honesty marker is never dropped).
    #[test]
    fn result_omits_empty_optionals() {
        let res = SlackCanvasResult {
            ok: true,
            op: "read".into(),
            canvas_id: Some(SlackCanvasId::new("F1")),
            contents: Some("# current".into()),
            canvases: None,
            fetch_status: CanvasFetchStatus::Fetched,
            bridged: true,
            metadata: Value::Null,
        };
        let v = serde_json::to_value(&res).unwrap();
        assert_eq!(
            v,
            json!({
                "ok": true,
                "op": "read",
                "canvas_id": "F1",
                "contents": "# current",
                "fetch_status": "fetched",
                "bridged": true
            })
        );
    }

    /// A pending `read` is **honest**: no `contents` key at all (not an empty
    /// string), `fetch_status: "pending_bridge"`, `bridged: false`. This is the
    /// core OCEAN-235 guarantee — an un-fulfilled read can't be mistaken for an
    /// empty canvas.
    #[test]
    fn pending_read_carries_no_contents_and_is_marked_pending() {
        let res = SlackCanvasResult::pending_read(SlackCanvasId::new("F0123ABCD"));
        assert!(res.ok);
        assert_eq!(res.op, "read");
        assert_eq!(res.fetch_status, CanvasFetchStatus::PendingBridge);
        assert!(!res.bridged);
        assert!(res.contents.is_none(), "pending read must not fabricate contents");
        assert!(!res.fetch_status.is_fetched());

        let v = serde_json::to_value(&res).unwrap();
        assert!(
            v.get("contents").is_none(),
            "the `contents` key must be ABSENT for a pending read, not empty: {v}"
        );
        assert_eq!(v["fetch_status"], "pending_bridge");
        assert_eq!(v["bridged"], false);
    }

    /// The bridge fulfillment seam stamps live content and flips the markers.
    #[test]
    fn fulfilled_read_carries_live_contents() {
        let res = SlackCanvasResult::fulfilled_read(
            SlackCanvasId::new("F1"),
            "# Real canvas body\n- fetched from Slack",
            json!({ "slack_file_id": "F1", "revision": 7 }),
        );
        assert_eq!(res.fetch_status, CanvasFetchStatus::Fetched);
        assert!(res.fetch_status.is_fetched());
        assert!(res.bridged);
        assert_eq!(
            res.contents.as_deref(),
            Some("# Real canvas body\n- fetched from Slack")
        );
        assert_eq!(res.metadata["slack_file_id"], "F1");
    }

    /// A pending `list` carries no `canvases` and is marked pending; the
    /// fulfillment seam stamps the resolved listing.
    #[test]
    fn list_pending_then_fulfilled() {
        let pending = SlackCanvasResult::pending_list();
        assert_eq!(pending.op, "list");
        assert_eq!(pending.fetch_status, CanvasFetchStatus::PendingBridge);
        assert!(pending.canvases.is_none());
        assert!(!pending.bridged);

        let fulfilled = SlackCanvasResult::fulfilled_list(
            vec![SlackCanvasSummary {
                canvas_id: SlackCanvasId::new("F9"),
                title: Some("Plan".into()),
            }],
            Value::Null,
        );
        assert_eq!(fulfilled.fetch_status, CanvasFetchStatus::Fetched);
        assert!(fulfilled.bridged);
        assert_eq!(fulfilled.canvases.as_ref().unwrap().len(), 1);
    }

    /// `fetch_status` defaults to `not_applicable` when absent on the wire, so a
    /// mutating-op result (or an older producer) deserializes cleanly.
    #[test]
    fn fetch_status_defaults_to_not_applicable() {
        let res: SlackCanvasResult = serde_json::from_value(json!({
            "ok": true,
            "op": "create",
            "bridged": false
        }))
        .expect("result without fetch_status deserializes");
        assert_eq!(res.fetch_status, CanvasFetchStatus::NotApplicable);
        assert!(!res.fetch_status.is_fetched());
    }
}
