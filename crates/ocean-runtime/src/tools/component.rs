//! Tools that let the agent render interactive UI components in the client.
//!
//! `component_render` mounts or updates a component (kanban, form, table, etc.).
//! `component_unmount` removes a previously rendered component.
//! `component_wait` blocks the turn until the user interacts with a component.
//!
//! These tools emit side-effect events (`Render` / `Unmount`) that get forwarded
//! to the SSE event bus and picked up by connected clients (web surface, TUI).
//! The component data itself lives in the tool result text so the model can
//! reference it in subsequent turns.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::types::{AgentTool, AgentToolResult, ToolSideEffect};

/// Global component-wait registry, accessible from both the tool and the
/// daemon's HTTP route without plumbing through `AgentRuntime`.
pub static COMPONENT_WAIT_REGISTRY: std::sync::LazyLock<ComponentWaitRegistry> =
    std::sync::LazyLock::new(ComponentWaitRegistry::new);

/// A set of known component kinds the agent can render. Used to validate
/// the `kind` parameter before emitting the render event. Unknown kinds are
/// still forwarded (forward-compat for new clients) but a warning is returned.
const VALID_KINDS: &[&str] = &[
    "kanban",
    "form",
    "table",
    "progress",
    "markdown",
    "dashboard",
    "chart",
    "timeline",
    "stat",
    "file_tree",
    "diff",
    "code",
    "callout",
    "gallery",
    "confirm",
    "map",
    "video",
];

// ---------------------------------------------------------------------------
// component_render
// ---------------------------------------------------------------------------

pub struct ComponentRenderTool;

#[async_trait]
impl AgentTool for ComponentRenderTool {
    fn name(&self) -> &str {
        "component_render"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Mount or update a rich, interactive UI component in the client surface. \
         On the web surface these render as live HTML — USE THEM LIBERALLY instead of \
         hand-typing markdown for anything structured. `id` is an agent-chosen opaque \
         string scoped to the session; reuse the same id with replace:true to update a \
         component in place (e.g. advance a progress bar, flip a timeline step to done).\n\
         \
         WHEN TO REACH FOR WHICH KIND:\n\
         • Rows/columns of data → 'table' (NEVER markdown pipe tables).\n\
         • Task/status board → 'kanban'. Collecting input → 'form' (then component_wait).\n\
         • A running task → 'progress'. A multi-step plan → 'timeline'.\n\
         • KPIs / metrics (views, plays, saves) → 'stat'. Numeric series to visualize → 'chart'.\n\
         • Project structure / file list → 'file_tree'. Showing code edits → 'diff'.\n\
         • A code snippet to copy → 'code'. An important note/warning → 'callout'.\n\
         • Images / screenshots / generated art → 'gallery'. A yes/no before something \
         destructive → 'confirm' (then component_wait for the answer).\n\
         • Anything geographic — locations, routes, where creators/streams/markets are → \
         'map' (a live, pannable Google Map). Far better than describing coordinates in text.\n\
         • A video to watch inline — a TikTok/Reel/YouTube link, or a direct video file → \
         'video' (embeds and plays in the chat). Use for campaign clips, sound previews, references.\n\
         • Several of the above at once → 'dashboard'. Long prose → 'markdown' (or plain text).\n\
         \
         PROPS SCHEMA BY KIND (props must match exactly):\n\
         • table — { columns: [\"Name\",\"Status\"], rows: [[\"Fix login\",\"open\"],[\"Add tests\",\"done\"]] }. Emits row_clicked { row_index }.\n\
         • kanban — { columns: [{id,title}], cards: [{id, column, title, description?}] }. Emits card_clicked.\n\
         • form — { title, fields: [{name, label, type, required?, options?}], submit_label? }. type is text|textarea|select|number. Emits form_submit { <name>: value }.\n\
         • progress — { label, value, max, indeterminate? }. value/max numbers (0.6/1.0). Display only.\n\
         • markdown — { content: \"## md\" }. Rich prose; supports tables, bold, lists.\n\
         • dashboard — { children: [{ id, width, kind?, props? }] }. Grid; width is fr units; inline kind+props renders that component in the cell.\n\
         • chart — { title?, type: \"bar\"|\"line\", series: [{label, value}] }. value is numeric. Display only.\n\
         • timeline — { steps: [{label, status: \"done\"|\"active\"|\"pending\"|\"error\", detail?}] }. Re-render with replace:true to advance.\n\
         • stat — { stats: [{label, value, delta?, trend: \"up\"|\"down\"|\"flat\"}] }. value is string or number.\n\
         • file_tree — { root?, entries: [{name, type: \"file\"|\"dir\", path?, children?}] }. Dirs nest via children. Files emit file_clicked { path }.\n\
         • diff — { filename?, lines: [{kind: \"add\"|\"del\"|\"ctx\", text}] }  OR  { filename?, unified: \"+new\\n-old\" }.\n\
         • code — { language?, filename?, code }. Renders a copy-able code block.\n\
         • callout — { variant: \"info\"|\"success\"|\"warn\"|\"error\", title?, body? }. body supports markdown.\n\
         • gallery — { images: [{src, caption?}] }. src is a URL or data: URI.\n\
         • confirm — { title, body?, confirm_label?, cancel_label?, variant? }. Emits confirm_response { confirmed: bool }.\n\
         • map — { center: {lat, lng}, zoom?, markers?: [{lat, lng, title?, label?}], fit_markers? }. \
         Live Google Map. `zoom` 1–20 (default ~10); `fit_markers:true` auto-frames all markers. \
         Emits marker_clicked { index, title }.\n\
         • video — { url, title?, autoplay?, start? }. `url` is a TikTok / Instagram Reel / \
         YouTube / Vimeo link OR a direct .mp4/.webm/.m3u8 URL — the surface picks the right \
         embed automatically. `start` is seconds offset (YouTube/file). Display only.\n\
         \
         Set replace:true to overwrite an existing component with the same id. \
         Full reference: docs/AGENT_RENDER_PROTOCOL.md."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Opaque component id, agent-chosen, scoped to the session"
                },
                "kind": {
                    "type": "string",
                    "enum": [
                        "kanban", "form", "table", "progress", "markdown", "dashboard",
                        "chart", "timeline", "stat", "file_tree", "diff", "code",
                        "callout", "gallery", "confirm", "map", "video"
                    ],
                    "description": "Component type. Defines the shape of `props`."
                },
                "props": {
                    "type": "object",
                    "description": "Component-specific JSON props; shape MUST match the chosen kind. \
                        table: {columns:[str], rows:[[cell,..]]}. kanban: {columns:[{id,title}], cards:[{id,column,title,description?}]}. \
                        form: {title, fields:[{name,label,type,required?,options?}], submit_label?}. \
                        progress: {label, value, max, indeterminate?}. markdown: {content}. \
                        dashboard: {children:[{id,width,kind?,props?}]}. \
                        chart: {title?, type:'bar'|'line', series:[{label,value}]}. \
                        timeline: {steps:[{label,status,detail?}]}. stat: {stats:[{label,value,delta?,trend?}]}. \
                        file_tree: {root?, entries:[{name,type,path?,children?}]}. \
                        diff: {filename?, lines:[{kind,text}]} or {filename?, unified:str}. \
                        code: {language?, filename?, code}. callout: {variant,title?,body?}. \
                        gallery: {images:[{src,caption?}]}. confirm: {title,body?,confirm_label?,cancel_label?,variant?}."
                },
                "replace": {
                    "type": "boolean",
                    "default": false,
                    "description": "If true, overwrite existing component with this id"
                }
            },
            "required": ["id", "kind", "props"]
        })
    }

    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let component_id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'id'")?
            .to_string();

        let kind = args
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or("missing 'kind'")?
            .to_string();

        let props = args
            .get("props")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));

        let replace = args
            .get("replace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Warn but don't reject unknown kinds — forward-compat for new clients.
        let mut warnings = String::new();
        if !VALID_KINDS.contains(&kind.as_str()) {
            warnings = format!(
                " (warning: unknown component kind '{kind}', clients may ignore it)"
            );
        }

        let summary = format!(
            "rendered component '{component_id}' of kind '{kind}'{warnings}",
        );

        Ok(AgentToolResult {
            content: vec![ocean_protocol::Content::text(summary)],
            details: json!({
                "component_id": component_id,
                "kind": kind,
            }),
            terminate: false,
            side_effects: vec![ToolSideEffect::Render {
                id: component_id,
                kind,
                props,
                replace,
            }],
        })
    }
}

// ---------------------------------------------------------------------------
// component_unmount
// ---------------------------------------------------------------------------

pub struct ComponentUnmountTool;

#[async_trait]
impl AgentTool for ComponentUnmountTool {
    fn name(&self) -> &str {
        "component_unmount"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "Remove a previously rendered UI component by id. The client unmounts \
         the component and reclaims any resources."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "description": "Component id to unmount"
                }
            },
            "required": ["id"]
        })
    }

    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let component_id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'id'")?
            .to_string();

        Ok(AgentToolResult {
            content: vec![ocean_protocol::Content::text(format!(
                "unmounted component '{component_id}'"
            ))],
            details: json!({
                "component_id": component_id,
            }),
            terminate: false,
            side_effects: vec![ToolSideEffect::Unmount {
                id: component_id,
            }],
        })
    }
}

// ---------------------------------------------------------------------------
// component_wait
// ---------------------------------------------------------------------------

/// In-memory registry of pending wait requests. The `component_wait` tool
/// creates an entry here, and the `/v1/component/event` route resolves it.
/// This is global state, but each entry is scoped by (session_id, component_id).

/// Global component-wait registry that both the tool and the
/// `/v1/component/event` route access.
///
/// Entries are keyed by `(session_id, component_id)` using the session id
/// passed in the tool args (the agent must include `session_id` when calling
/// `component_wait`).
#[derive(Default)]
pub struct ComponentWaitRegistry {
    /// Map of `(session_id, component_id)` → oneshot sender for the interaction event.
    /// The agent loop's tool execution blocks on the receiver side.
    pub pending: Mutex<HashMap<(String, String), tokio::sync::oneshot::Sender<Value>>>,
}

impl ComponentWaitRegistry {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Tool that blocks the agent turn until the user interacts with a rendered
/// component. Uses the global [`COMPONENT_WAIT_REGISTRY`] shared with the
/// daemon's `/v1/component/event` route.
pub struct ComponentWaitTool;

#[async_trait]
impl AgentTool for ComponentWaitTool {
    fn name(&self) -> &str {
        "component_wait"
    }

    fn requires_permission(&self) -> bool {
        true
    }

    fn description(&self) -> &str {
        "Block the current turn and wait for the user to interact with a \
         rendered component (e.g. click a kanban card, submit a form). \
         Returns the interaction event as JSON. Use this to build conversational \
         workflows around live UI: render a form, wait for submit, process the data. \
         The agent must pass the current `session_id` in args."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Current agent session id (the agent must inject this from context)"
                },
                "id": {
                    "type": "string",
                    "description": "Component id to wait for interaction on"
                },
                "timeout_ms": {
                    "type": "integer",
                    "default": 60000,
                    "description": "Max wait time in milliseconds"
                }
            },
            "required": ["session_id", "id"]
        })
    }

    async fn execute(&self, _tool_call_id: &str, args: Value) -> Result<AgentToolResult, String> {
        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'session_id' (agent must pass the current session id)")?
            .to_string();

        let component_id = args
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'id'")?
            .to_string();

        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(60_000);

        let (tx, rx) = tokio::sync::oneshot::channel::<Value>();
        {
            let mut pending = COMPONENT_WAIT_REGISTRY
                .pending
                .lock()
                .map_err(|e| e.to_string())?;
            pending.insert((session_id.clone(), component_id.clone()), tx);
        }

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            rx,
        )
        .await;

        {
            let mut pending = COMPONENT_WAIT_REGISTRY
                .pending
                .lock()
                .map_err(|e| e.to_string())?;
            pending.remove(&(session_id, component_id.clone()));
        }

        match result {
            Ok(Ok(event)) => Ok(AgentToolResult {
                content: vec![ocean_protocol::Content::text(
                    serde_json::to_string(&event).unwrap_or_else(|_| r#"{"type":"unknown"}"#.into())
                )],
                details: event,
                terminate: false,
                side_effects: Vec::new(),
            }),
            Ok(Err(_)) => Err("component interaction channel closed unexpectedly".into()),
            Err(_) => Err(format!("timed out waiting for interaction on '{component_id}'")),
        }
    }
}
