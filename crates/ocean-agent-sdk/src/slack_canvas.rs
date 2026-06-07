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

/// Structured result returned by the `slack_canvas` tool, echoing which op ran and
/// carrying the data the agent reasons over.
///
/// In Phase 2 the runtime emits a **well-formed contract** result so the agent
/// loop and tests work end-to-end; the bridge (a later phase) is what fills
/// [`contents`](Self::contents) (for `read`) and [`canvases`](Self::canvases)
/// (for `list`) with real Slack data and stamps the authoritative `canvas_id` for
/// `create`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlackCanvasResult {
    pub ok: bool,
    /// Which op ran (`create`/`read`/`update`/`append`/`list`).
    pub op: String,
    /// The canvas this op targeted, when it names a single canvas. `None` for
    /// `list`, and for `create` until the bridge mints the real id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas_id: Option<SlackCanvasId>,
    /// **Awareness payload** for `read`: the current markdown contents of the
    /// canvas the agent can reason over. `None` for non-read ops, and a contracted
    /// placeholder until the bridge populates live contents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contents: Option<String>,
    /// Result of a `list` op: the canvases visible in the channel. Empty/`None`
    /// for non-list ops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvases: Option<Vec<SlackCanvasSummary>>,
    /// Whether the bridge round-trip to the Slack API has happened yet. `false`
    /// in Phase 2 (the tool emits a contracted op the bridge has not yet fulfilled
    /// against the live Slack Canvas API). The bridge flips this to `true`.
    pub bridged: bool,
    /// Free-form passthrough metadata, reserved for the bridge.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
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
    #[test]
    fn result_omits_empty_optionals() {
        let res = SlackCanvasResult {
            ok: true,
            op: "read".into(),
            canvas_id: Some(SlackCanvasId::new("F1")),
            contents: Some("# current".into()),
            canvases: None,
            bridged: false,
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
                "bridged": false
            })
        );
    }
}
