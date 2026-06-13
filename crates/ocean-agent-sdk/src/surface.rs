//! Ocean **surface patch protocol** — the shared wire contract for agent-native
//! canvas mutation.
//!
//! This module is Slice 1 of the GPUI Masterbuild epic (OCEAN-147). It defines
//! the typed, serde-stable vocabulary that three independent consumers must all
//! agree on:
//!
//! - the **GPUI native canvas** (`ocean-surface/crates/ocean-gui`), which applies
//!   patches to its `CanvasLedger` and renders them,
//! - the **runtime `surface_patch` tool** (Slice 2), which an agent calls to emit
//!   structured patches, and
//! - the **daemon event stream** (Slice 3), which fans patches out to the right
//!   session over `/v1/agent/events`.
//!
//! # Wire contract rules
//!
//! - Identifiers are string-backed `serde(transparent)` newtypes. On the wire a
//!   `ComponentId` is just `"brief-1"`, never `{ "0": "brief-1" }`.
//! - [`SurfacePatch`] is **internally tagged** on `"op"` with `snake_case`
//!   rename, so `{ "op": "upsert_component", "component": { … } }` from §6 of the
//!   masterbuild plan deserializes directly.
//! - Geometry (`x`/`y`/`w`/`h`/`width`/`height`) is `f32` and roundtrips as JSON
//!   numbers, never strings.
//! - Every type carrying free-form metadata uses `serde_json::Value`, so unknown
//!   fields from richer producers survive a roundtrip untouched.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::AgentSessionId;

// ---------------------------------------------------------------------------
// Identifiers — string-backed, transparent on the wire
// ---------------------------------------------------------------------------

/// Macro to define a string-backed, serde-transparent newtype with the small
/// set of ergonomic conversions every Ocean id wants.
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
    /// Identifies a *surface* — one client face onto a session (e.g. `gpui:local`).
    SurfaceId
);
string_id!(
    /// Identifies a *canvas* within a surface (e.g. `canvas:main`).
    CanvasId
);
string_id!(
    /// Identifies a *component* (card, node, frame, …) on a canvas.
    ComponentId
);
string_id!(
    /// Identifies a single emitted *patch*.
    PatchId
);
string_id!(
    /// Identifies an *edge* between two endpoints.
    EdgeId
);

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

/// Axis-aligned rectangle in canvas space. All fields roundtrip as JSON numbers.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

/// Pan/zoom state of a canvas viewport.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub x: f32,
    pub y: f32,
    #[serde(default = "Viewport::default_zoom")]
    pub zoom: f32,
}

impl Viewport {
    fn default_zoom() -> f32 {
        1.0
    }
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            zoom: 1.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Actor — who originated a patch
// ---------------------------------------------------------------------------

/// Reference to the actor that originated a patch. Kept deliberately loose so the
/// daemon, an agent turn, or a human can all be named without a closed enum that
/// the runtime would have to keep in lockstep.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    /// Coarse actor class, e.g. `"agent"`, `"human"`, `"system"`.
    pub kind: String,
    /// Optional stable id for the actor (agent name, user id, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Optional human-friendly label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl ActorRef {
    /// An agent actor with an optional name.
    pub fn agent(id: impl Into<Option<String>>) -> Self {
        Self {
            kind: "agent".to_string(),
            id: id.into(),
            label: None,
        }
    }

    /// A human actor with an optional id.
    pub fn human(id: impl Into<Option<String>>) -> Self {
        Self {
            kind: "human".to_string(),
            id: id.into(),
            label: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Patch payloads
// ---------------------------------------------------------------------------

/// Upsert payload for a component. Position (`rect`) and `content` are optional so
/// an agent can create a component and let the app allocate placement (see the
/// placement rules in §6). Unknown structure on `content`/`metadata` passes
/// through verbatim as `serde_json::Value`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanvasComponentPatch {
    pub id: ComponentId,
    /// Component kind or template name, e.g. `"card"`, `"brief_card"`.
    pub kind: String,
    /// Requested placement. If omitted the app allocates a slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<Rect>,
    /// Optional stacking order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub z_index: Option<i32>,
    /// Free-form content payload (title/body/etc). Defaults to `null`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub content: Value,
    /// Free-form metadata that survives a roundtrip untouched.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
}

/// Endpoint of an edge — either a bare component or a specific port on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub component_id: ComponentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<String>,
}

/// Create/update payload for an edge between two endpoints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanvasEdgePatch {
    pub id: EdgeId,
    pub from: Endpoint,
    pub to: Endpoint,
    /// Edge kind/semantic, e.g. `"dependency"`, `"flow"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub metadata: Value,
}

/// Target of a [`SurfacePatch::Focus`] operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FocusTarget {
    /// Focus a single component.
    Component { component_id: ComponentId },
    /// Focus an edge.
    Edge { edge_id: EdgeId },
    /// Focus the whole canvas / fit to content.
    Canvas,
}

/// Target of a [`SurfacePatch::Layout`] operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutTarget {
    /// Lay out the entire canvas.
    Canvas,
    /// Lay out the children of one container component.
    Component { component_id: ComponentId },
    /// Lay out an explicit set of components.
    Components { ids: Vec<ComponentId> },
}

/// Layout strategy for a [`SurfacePatch::Layout`] operation. Open string set so
/// new strategies (ELK/Dagre/etc.) can be added without breaking the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LayoutStrategy {
    Grid,
    Stack,
    Row,
    Column,
    Tree,
    Graph,
    /// Any strategy not in the known set, carried as its raw name.
    #[serde(untagged)]
    Other(String),
}

// ---------------------------------------------------------------------------
// Surface patch operation
// ---------------------------------------------------------------------------

/// A single structured mutation to an Ocean surface canvas.
///
/// Internally tagged on `"op"` with `snake_case` discriminants, so the §6 minimal
/// JSON shape `{ "op": "upsert_component", "component": { … } }` deserializes
/// directly into [`SurfacePatch::UpsertComponent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SurfacePatch {
    /// Create or update a component.
    UpsertComponent { component: CanvasComponentPatch },
    /// Move a component to an absolute position.
    MoveComponent {
        component_id: ComponentId,
        x: f32,
        y: f32,
    },
    /// Resize a component.
    ResizeComponent {
        component_id: ComponentId,
        width: f32,
        height: f32,
    },
    /// Delete a component.
    DeleteComponent { component_id: ComponentId },
    /// Create or update an edge between two endpoints.
    Connect { edge: CanvasEdgePatch },
    /// Remove an edge.
    Disconnect { edge_id: EdgeId },
    /// Focus a target (component/edge/canvas).
    Focus { target: FocusTarget },
    /// Replace the current selection.
    Select { ids: Vec<ComponentId> },
    /// Set the viewport pan/zoom.
    SetViewport { viewport: Viewport },
    /// Run a layout strategy over a target.
    Layout {
        target: LayoutTarget,
        strategy: LayoutStrategy,
    },
    /// Group components under a frame.
    Group {
        frame_id: ComponentId,
        children: Vec<ComponentId>,
    },
}

impl SurfacePatch {
    /// The single component this patch *contends on* for the convergent merge
    /// (OCEAN-258), if any. This is the key a ledger's `CanvasMergeState` merges
    /// the patch's version under, so that two concurrent writes to the **same**
    /// component are resolved deterministically while writes to **different**
    /// components both land.
    ///
    /// Returns `Some(id)` for the per-component mutations — `UpsertComponent`,
    /// `MoveComponent`, `ResizeComponent`, `DeleteComponent`. Returns `None` for
    /// ops that don't last-write-wins a single component:
    ///
    /// - `Connect` / `Disconnect` mutate an *edge*, not a component register;
    /// - `Select` / `Focus` / `SetViewport` are view state, not durable component
    ///   state — they intentionally don't participate in component LWW;
    /// - `Layout` / `Group` touch *many* components at once (a layout is applied
    ///   as a unit, not merged per-component).
    ///
    /// A `None` here means the patch is not gated by the per-component merge and
    /// is applied directly (its effect is either idempotent or naturally
    /// last-writer-wins on a different axis).
    pub fn target_component(&self) -> Option<&ComponentId> {
        match self {
            SurfacePatch::UpsertComponent { component } => Some(&component.id),
            SurfacePatch::MoveComponent { component_id, .. }
            | SurfacePatch::ResizeComponent { component_id, .. }
            | SurfacePatch::DeleteComponent { component_id } => Some(component_id),
            SurfacePatch::Connect { .. }
            | SurfacePatch::Disconnect { .. }
            | SurfacePatch::Focus { .. }
            | SurfacePatch::Select { .. }
            | SurfacePatch::SetViewport { .. }
            | SurfacePatch::Layout { .. }
            | SurfacePatch::Group { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope + tool result
// ---------------------------------------------------------------------------

/// A patch plus the session/surface/canvas/actor context needed to route and
/// persist it. This is what gets appended to a canvas patch log and what the
/// daemon streams to clients.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfacePatchEnvelope {
    pub patch_id: PatchId,
    pub session_id: AgentSessionId,
    pub surface_id: SurfaceId,
    pub canvas_id: CanvasId,
    pub actor: ActorRef,
    pub created_at_ms: i64,
    pub patch: SurfacePatch,
    /// Logical version stamp for the **convergent merge** (OCEAN-258). When two
    /// writers (operator + agent) mutate the *same* component, this `(rev, actor)`
    /// [`ComponentVersion`] — not the wall-clock `created_at_ms` — is what a
    /// ledger's [`CanvasMergeState`] uses to pick a deterministic winner
    /// regardless of arrival order.
    ///
    /// **Additive / optional.** Producers that predate the merge layer (and the
    /// `ocean-surface` mirror until it adopts versioning) omit it, so it is
    /// `skip_serializing_if = None` and absent on the wire for them. A `None`
    /// envelope is treated by a merging ledger as "unversioned" — it may fall back
    /// to legacy arrival-order application. Mutations that don't target a single
    /// component (e.g. `Select`, `SetViewport`, `Layout`) leave this `None`.
    ///
    /// [`ComponentVersion`]: crate::surface_merge::ComponentVersion
    /// [`CanvasMergeState`]: crate::surface_merge::CanvasMergeState
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<crate::surface_merge::ComponentVersion>,
}

/// Structured result returned by the `surface_patch` tool (§7). Mirrors the
/// minimal JSON shape exactly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfacePatchResponse {
    pub ok: bool,
    /// Number of patches applied.
    pub applied: u32,
    pub canvas_id: CanvasId,
    /// New canvas revision after applying.
    pub revision: u64,
    /// Ids of the components touched by the applied patches.
    pub component_ids: Vec<ComponentId>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The EXACT `upsert_component` JSON from gpui_masterbuild.md §6 must
    /// deserialize into `SurfacePatch::UpsertComponent`.
    #[test]
    fn deserializes_section_6_upsert_component() {
        let raw = json!({
            "op": "upsert_component",
            "component": {
                "id": "brief-1",
                "kind": "brief_card",
                "rect": { "x": 420, "y": 120, "w": 320, "h": 220 },
                "content": {
                    "title": "Sales Brief",
                    "body": "Draft brief for the Warner campaign"
                },
                "metadata": {
                    "source": "longhouse.sales"
                }
            }
        });

        let patch: SurfacePatch = serde_json::from_value(raw).expect("should deserialize §6 shape");

        let SurfacePatch::UpsertComponent { component } = patch else {
            panic!("expected UpsertComponent, got {patch:?}");
        };

        assert_eq!(component.id, ComponentId::new("brief-1"));
        assert_eq!(component.kind, "brief_card");

        // x/y/w/h roundtrip as numbers, not strings.
        let rect = component.rect.expect("rect present");
        assert_eq!(rect.x, 420.0);
        assert_eq!(rect.y, 120.0);
        assert_eq!(rect.w, 320.0);
        assert_eq!(rect.h, 220.0);

        // content survives.
        assert_eq!(component.content["title"], "Sales Brief");
        // unknown metadata survives.
        assert_eq!(component.metadata["source"], "longhouse.sales");
    }

    /// Geometry must serialize as JSON numbers, never strings.
    #[test]
    fn geometry_is_numeric_on_the_wire() {
        let rect = Rect::new(1.5, 2.0, 3.0, 4.0);
        let v = serde_json::to_value(rect).unwrap();
        assert!(v["x"].is_number(), "x must be a number: {v}");
        assert!(v["w"].is_number(), "w must be a number: {v}");
        assert_eq!(v["x"], 1.5);

        let move_patch = SurfacePatch::MoveComponent {
            component_id: ComponentId::new("n1"),
            x: 10.0,
            y: 20.0,
        };
        let mv = serde_json::to_value(&move_patch).unwrap();
        assert_eq!(mv["op"], "move_component");
        assert!(mv["x"].is_number());
        assert!(mv["y"].is_number());
    }

    /// Newtype ids are transparent strings on the wire.
    #[test]
    fn ids_are_transparent_strings() {
        let id = ComponentId::new("brief-1");
        assert_eq!(serde_json::to_value(&id).unwrap(), json!("brief-1"));
        let back: CanvasId = serde_json::from_value(json!("canvas:main")).unwrap();
        assert_eq!(back, CanvasId::new("canvas:main"));
    }

    /// `SurfacePatchResponse` serializes to the §7 shape exactly.
    #[test]
    fn response_matches_section_7_shape() {
        let resp = SurfacePatchResponse {
            ok: true,
            applied: 3,
            canvas_id: CanvasId::new("canvas:main"),
            revision: 12,
            component_ids: vec![ComponentId::new("brief-1"), ComponentId::new("proposal-1")],
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            v,
            json!({
                "ok": true,
                "applied": 3,
                "canvas_id": "canvas:main",
                "revision": 12,
                "component_ids": ["brief-1", "proposal-1"]
            })
        );
    }

    /// Each op discriminant round-trips through snake_case.
    #[test]
    fn all_ops_roundtrip_snake_case() {
        let cases = vec![
            (
                SurfacePatch::ResizeComponent {
                    component_id: ComponentId::new("c1"),
                    width: 100.0,
                    height: 50.0,
                },
                "resize_component",
            ),
            (
                SurfacePatch::DeleteComponent {
                    component_id: ComponentId::new("c1"),
                },
                "delete_component",
            ),
            (
                SurfacePatch::Disconnect {
                    edge_id: EdgeId::new("e1"),
                },
                "disconnect",
            ),
            (
                SurfacePatch::Select {
                    ids: vec![ComponentId::new("c1"), ComponentId::new("c2")],
                },
                "select",
            ),
            (
                SurfacePatch::SetViewport {
                    viewport: Viewport::default(),
                },
                "set_viewport",
            ),
            (
                SurfacePatch::Focus {
                    target: FocusTarget::Component {
                        component_id: ComponentId::new("c1"),
                    },
                },
                "focus",
            ),
            (
                SurfacePatch::Layout {
                    target: LayoutTarget::Canvas,
                    strategy: LayoutStrategy::Grid,
                },
                "layout",
            ),
            (
                SurfacePatch::Group {
                    frame_id: ComponentId::new("f1"),
                    children: vec![ComponentId::new("c1")],
                },
                "group",
            ),
        ];

        for (patch, op) in cases {
            let v = serde_json::to_value(&patch).unwrap();
            assert_eq!(v["op"], op, "op tag mismatch for {patch:?}");
            let back: SurfacePatch = serde_json::from_value(v).unwrap();
            assert_eq!(back, patch, "roundtrip mismatch for {op}");
        }
    }

    /// Connect carries a full edge payload under `edge`.
    #[test]
    fn connect_roundtrips_with_edge() {
        let patch = SurfacePatch::Connect {
            edge: CanvasEdgePatch {
                id: EdgeId::new("e1"),
                from: Endpoint {
                    component_id: ComponentId::new("a"),
                    port: Some("out".to_string()),
                },
                to: Endpoint {
                    component_id: ComponentId::new("b"),
                    port: None,
                },
                kind: Some("flow".to_string()),
                label: Some("then".to_string()),
                metadata: json!({ "weight": 2 }),
            },
        };
        let v = serde_json::to_value(&patch).unwrap();
        assert_eq!(v["op"], "connect");
        assert_eq!(v["edge"]["from"]["port"], "out");
        let back: SurfacePatch = serde_json::from_value(v).unwrap();
        assert_eq!(back, patch);
    }

    /// Unknown metadata survives a full envelope roundtrip untouched.
    #[test]
    fn unknown_metadata_survives_envelope_roundtrip() {
        let env = SurfacePatchEnvelope {
            patch_id: PatchId::new("patch-1"),
            session_id: AgentSessionId::new_v4(),
            surface_id: SurfaceId::new("gpui:local"),
            canvas_id: CanvasId::new("canvas:main"),
            actor: ActorRef::agent(Some("sage".to_string())),
            created_at_ms: 1_725_000_000_000,
            patch: SurfacePatch::UpsertComponent {
                component: CanvasComponentPatch {
                    id: ComponentId::new("brief-1"),
                    kind: "brief_card".to_string(),
                    rect: Some(Rect::new(420.0, 120.0, 320.0, 220.0)),
                    z_index: None,
                    content: json!({ "title": "Sales Brief" }),
                    metadata: json!({
                        "source": "longhouse.sales",
                        "nested": { "future_field": [1, 2, 3] }
                    }),
                },
            },
            // Legacy / unversioned producer: `version` absent on the wire.
            version: None,
        };

        let s = serde_json::to_string(&env).unwrap();
        // The optional merge `version` must be omitted entirely when None, so
        // pre-OCEAN-258 consumers and the ocean-surface mirror still parse it.
        assert!(
            !s.contains("version"),
            "unversioned envelope must not emit a version field"
        );
        let back: SurfacePatchEnvelope = serde_json::from_str(&s).unwrap();
        assert_eq!(back, env);

        // Metadata is preserved structurally, including fields this crate has no
        // typed knowledge of.
        let SurfacePatch::UpsertComponent { component } = &back.patch else {
            panic!("expected UpsertComponent");
        };
        assert_eq!(component.metadata["nested"]["future_field"][2], 3);
    }

    /// Unknown layout strategy degrades to `Other` rather than failing.
    #[test]
    fn unknown_layout_strategy_is_other() {
        let raw = json!({
            "op": "layout",
            "target": "canvas",
            "strategy": "elk_layered"
        });
        let patch: SurfacePatch = serde_json::from_value(raw).unwrap();
        let SurfacePatch::Layout { strategy, .. } = patch else {
            panic!("expected Layout");
        };
        assert_eq!(strategy, LayoutStrategy::Other("elk_layered".to_string()));
    }
}
