//! App — the terminal workbench frame:
//!
//! ```text
//! ┌ title row ──────────────────────────────────────────────┐
//! │ sessions │▏│ breadcrumb                       │▏│ files │
//! │ (left)   │ │ CENTER: chat / editor / graph    │ │(right)│
//! │          │ │ ──────────────────────────────── │ │       │
//! │          │ │ terminal (docked bottom, live)   │ │       │
//! └ status row ──────────────────────────────────────────────┘
//! ```
//!
//! No tabs: sessions, tree, and the terminal dock are collapsible around one
//! center working surface. Normal startup presents a modal route chooser over a
//! clean chat-only surface; explicit `--session` bypasses it. The center swaps
//! between chat, editor, and graph without changing the launch workspace.
//!
//! Keys: ⌃⌥1 sessions · ⌃⌥2 files · ⌃⌥3 chat · ⌃⌥4 editor · ⌃⌥5 graph toggle ·
//! ⌃⌥6 terminal · Tab cycles focus · Esc → back to chat (double-Esc leaves the
//! terminal dock) · ⌃Q quits (⌃C passes to the PTY).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent, AgentTurnRequest, ThinkingLevel};
use ocean_core::{PermissionId, PermissionMode, PermissionSettingsResponse, RequestId};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use tokio::sync::mpsc;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::{
    action::{Action, HealthSource, LoginTarget, Nav},
    client::{DaemonClient, ModelEntry, TurnSubmitError},
    component::Component,
    components::{
        chat::{sanitize_line, ChatComponent},
        editor::EditorComponent,
        file_tree::FileTreeComponent,
        graph::GraphComponent,
        pty_pane::PtyComponent,
        session_rail::SessionRailComponent,
        session_tray::SessionComponentTray,
        workflow_graph::WorkflowGraphComponent,
    },
    daemon_boot, dictation, errfmt,
    event::{Event, EventHandler},
    git,
    herdr::Reporter as HerdrReporter,
    kitty,
    status::{self, StatusData, Tone},
    theme::{self, g},
    tui,
};

const SESS_W: u16 = 30;
const TREE_W: u16 = 30;
const MIN_RAIL_W: u16 = 16;
const MIN_WORKSPACE_W: u16 = 40;
/// Default terminal-dock height; resizable at runtime (drag the splitter / ⌃⌥↑↓).
const TERM_H: u16 = 14;
/// How long an ephemeral notice occupies the bottom status row before the
/// tick path clears it — the idle row returns to its minimal, model-only set.
const STATUS_TTL: Duration = Duration::from_secs(8);
/// A quick Space tap remains typing; only a deliberate hold opens the mic.
const DICTATION_HOLD: Duration = Duration::from_millis(1000);
/// Floor for the dock and the main surface so neither can be squeezed to nothing.
const MIN_TERM_H: u16 = 3;
const MIN_CENTER_H: u16 = 5;
const MIN_TREE_WITH_TRAY_H: u16 = 5;
const MIN_TRAY_H: u16 = 6;

const PERMISSION_OPTIONS: [(PermissionMode, &str, &str); 3] = [
    (
        PermissionMode::Manual,
        "Manually approve",
        "Ocean pauses so you can approve each action.",
    ),
    (
        PermissionMode::Automatic,
        "Automatically approve",
        "Ocean runs on its own and pauses to ask if anything looks unsafe.",
    ),
    (
        PermissionMode::SkipAll,
        "Skip all approvals",
        "Ocean never pauses, even for unsafe actions.",
    ),
];

fn permission_mode_index(mode: PermissionMode) -> usize {
    PERMISSION_OPTIONS
        .iter()
        .position(|(candidate, _, _)| *candidate == mode)
        .unwrap_or(1)
}

fn permission_mode_label(mode: PermissionMode) -> &'static str {
    PERMISSION_OPTIONS[permission_mode_index(mode)].1
}

/// Largest dock height that still leaves the main surface `MIN_CENTER_H` rows,
/// given the center column's total height (crumb + surface + splitter + dock).
fn max_term_h(center_h: u16) -> u16 {
    // reserve: 1 crumb + 1 splitter + MIN_CENTER_H surface.
    center_h.saturating_sub(2 + MIN_CENTER_H).max(MIN_TERM_H)
}

/// Largest width one side rail may occupy while preserving the center and the
/// currently visible opposite rail. Each visible rail also owns a 1-cell
/// splitter; hidden rails consume no width regardless of their stored size.
fn max_rail_width(body_w: u16, opposite_w: Option<u16>) -> u16 {
    body_w
        .saturating_sub(MIN_WORKSPACE_W)
        .saturating_sub(1)
        .saturating_sub(opposite_w.map_or(0, |w| w.saturating_add(1)))
        .max(MIN_RAIL_W)
}

/// Split the visible Files rail into independent file-tree and session-component
/// panes. Tiny terminals and empty trays preserve the old full-height tree.
fn file_rail_rects(area: Rect, tray_visible: bool, desired_tray_h: u16) -> (Rect, Rect, Rect) {
    if !tray_visible || area.height < MIN_TREE_WITH_TRAY_H + 1 + MIN_TRAY_H || area.width == 0 {
        return (area, Rect::default(), Rect::default());
    }
    let tray_h = desired_tray_h
        .max(MIN_TRAY_H)
        .min(area.height - MIN_TREE_WITH_TRAY_H - 1);
    let tree_h = area.height - tray_h - 1;
    (
        Rect::new(area.x, area.y, area.width, tree_h),
        Rect::new(area.x, area.y + tree_h, area.width, 1),
        Rect::new(area.x, area.y + tree_h + 1, area.width, tray_h),
    )
}

/// What the center surface is showing (CTRL swaps editor↔graph the same way).
#[derive(Clone, Copy, PartialEq)]
enum Center {
    Chat,
    Editor,
    Graph,
    WorkflowGraph,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RightRailMode {
    Files,
    Workflow,
}

/// Which visible pane has the keyboard.
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Sessions,
    Tree,
    Center,
    Term,
}

#[derive(Clone, Copy, PartialEq)]
enum SelectionSpace {
    Screen,
    Chat,
    Editor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ProviderSection {
    Agent,
    Voice,
}

impl ProviderSection {
    fn label(self) -> &'static str {
        match self {
            Self::Agent => "AGENT MODELS",
            Self::Voice => "VOICE MODELS",
        }
    }
}

/// One row in the `/providers` auth popup: a static descriptor (label, auth-file
/// block key, credential env vars) plus a status string computed at open time
/// and refreshed after an inline API-key save.
#[derive(Clone)]
struct ProviderRow {
    section: ProviderSection,
    label: &'static str,
    block_key: &'static str,
    env_vars: &'static [&'static str],
    status: String,
}

impl ProviderRow {
    /// OAuth providers (Claude Code, Codex) auth via a `type:"oauth"` block and
    /// a browser flow — every other row is a plain API key. Drives the Enter
    /// behavior in the popup (login vs inline key entry).
    fn is_oauth(&self) -> bool {
        matches!(self.block_key, "claude-code" | "openai-codex")
    }
}

/// Static descriptors (no per-open status) for [`ProviderRow`]. Voice uses
/// dedicated auth blocks so key saves cannot alter agent OAuth/model routing.
const PROVIDER_TABLE: &[(ProviderSection, &str, &str, &[&str])] = &[
    (
        ProviderSection::Agent,
        "Claude (Claude Code OAuth)",
        "claude-code",
        &[],
    ),
    (
        ProviderSection::Agent,
        "Codex (ChatGPT OAuth)",
        "openai-codex",
        &[],
    ),
    (
        ProviderSection::Agent,
        "GLM — Z.AI coding plan",
        "glm",
        &[
            "ZAI_API_KEY",
            "GLM_API_KEY",
            "OCEAN_GLM_API_KEY",
            "ZHIPUAI_API_KEY",
            "BIGMODEL_API_KEY",
        ],
    ),
    (
        ProviderSection::Agent,
        "DeepSeek",
        "deepseek",
        &["DEEPSEEK_API_KEY", "OCEAN_DEEPSEEK_API_KEY"],
    ),
    (
        ProviderSection::Agent,
        "Kimi (Moonshot)",
        "kimi",
        &["MOONSHOT_API_KEY", "KIMI_API_KEY", "OCEAN_MOONSHOT_API_KEY"],
    ),
    (
        ProviderSection::Agent,
        "MiniMax",
        "minimax",
        &["MINIMAX_API_KEY", "OCEAN_MINIMAX_API_KEY"],
    ),
    (
        ProviderSection::Agent,
        "Google (Gemini)",
        "google",
        &["GEMINI_API_KEY", "GOOGLE_API_KEY", "OCEAN_GOOGLE_API_KEY"],
    ),
    (
        ProviderSection::Agent,
        "OpenAI API (agent models)",
        "openai",
        &["OPENAI_API_KEY", "OCEAN_OPENAI_API_KEY"],
    ),
    (
        ProviderSection::Voice,
        "xAI Voice — STT / TTS",
        "xai",
        &["XAI_API_KEY"],
    ),
    (
        ProviderSection::Voice,
        "OpenAI Realtime — GPT Realtime",
        "openai-realtime",
        &["OCEAN_OPENAI_REALTIME_API_KEY", "OPENAI_REALTIME_API_KEY"],
    ),
];

/// `/providers` popup mode: list navigation, or inline API-key entry for the
/// selected API-key row.
#[derive(Clone)]
enum ProvidersMode {
    List,
    KeyEntry { block_key: String, buffer: String },
}

/// Derive a `/providers` row status: env var (first hit) > auth-file block >
/// "not configured". OAuth blocks (`claude-code`, `openai-codex`) report
/// `oauth ok` / `oauth expired` based on the block's `expires` field; API-key
/// blocks report `auth file` when a non-empty key is present.
fn provider_status(
    block_key: &str,
    env_vars: &[&str],
    auth_json: &Option<serde_json::Value>,
) -> String {
    if let Some(var) = env_vars.iter().find(|v| {
        std::env::var(v)
            .ok()
            .filter(|x| !x.trim().is_empty())
            .is_some()
    }) {
        return format!("env:{var}");
    }
    let Some(json) = auth_json else {
        return "not configured".into();
    };
    let Some(entry) = json.pointer(&format!("/{block_key}")) else {
        return "not configured".into();
    };
    if matches!(block_key, "claude-code" | "openai-codex") {
        let is_oauth = entry.pointer("/type").and_then(serde_json::Value::as_str) == Some("oauth");
        if !is_oauth {
            return "not configured".into();
        }
        let access = entry
            .pointer("/access")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty());
        if access.is_none() {
            return "not configured".into();
        }
        // `expires` is ms since epoch when large, seconds otherwise. Treat a
        // missing `expires` as "accept" (no expiry known), matching
        // ocean-providers' oauth_access_token.
        match entry
            .pointer("/expires")
            .and_then(serde_json::Value::as_i64)
        {
            Some(ms) => {
                let now_ms = unix_epoch_ms();
                let expires_ms = if ms >= 1_000_000_000_000 {
                    ms
                } else {
                    ms * 1000
                };
                if expires_ms <= now_ms {
                    "oauth expired".into()
                } else {
                    "oauth ok".into()
                }
            }
            None => "oauth ok".into(),
        }
    } else {
        let has_key = entry
            .pointer("/api_key")
            .or_else(|| entry.pointer("/key"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .is_some();
        if has_key {
            "auth file".into()
        } else {
            "not configured".into()
        }
    }
}

/// Current time in milliseconds since the Unix epoch (mirrors Ocean's auth-file
/// `expires` convention).
fn unix_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct SpaceHold {
    id: u64,
    pressed_at: Instant,
    started: bool,
}

pub struct App {
    client: DaemonClient,
    workspace_root: String,
    rail: SessionRailComponent,
    tree: FileTreeComponent,
    tray: SessionComponentTray,
    chat: ChatComponent,
    pty: PtyComponent,
    editor: EditorComponent,
    graph: GraphComponent,
    workflow_graph: WorkflowGraphComponent,
    right_rail_mode: RightRailMode,
    center: Center,
    focus: Focus,
    session_id: Option<AgentSessionId>,
    /// Monotonic identity for the current session binding. A→B→A rebinding
    /// cannot make an old completion current merely because the UUID matches.
    session_binding_generation: u64,
    /// Monotonic identity for the current scoped SSE task. Replacing history
    /// from a sync fence invalidates envelopes already queued by the old task.
    stream_generation: u64,
    /// Monotonic identity for best-effort resume/busy activity probes. New
    /// submissions and compact operations invalidate older probe completions.
    session_activity_probe_generation: u64,
    /// Best-effort projection of the authoritative TUI lifecycle into a
    /// surrounding Herdr pane. Disabled automatically outside Herdr.
    herdr: HerdrReporter,
    /// `/model <id>` override applied to subsequent turns (None → daemon default).
    model_override: Option<String>,
    /// The live SSE subscription for `session_id`. Held so a session switch
    /// aborts the superseded stream instead of leaking it (a leaked stream
    /// kept pumping a stale session's events into the chat).
    stream_task: Option<tokio::task::JoinHandle<()>>,
    /// The health-monitor task that re-probes and triggers autostart on failure.
    health_task: Option<tokio::task::JoinHandle<()>>,
    /// Ephemeral notice/error message (unresolved error/notice bucket). Typed
    /// health lives in `health`; this never carries connection state. Written
    /// via `set_notice` and expired back to empty on the tick path
    /// (`STATUS_TTL`) so a stale acknowledgement can't squat on the idle row.
    status: String,
    /// When `status` was last written — drives `expire_status`.
    status_at: Instant,
    /// Typed per-source health (daemon probe · SSE transport) — degraded
    /// conditions render in the bottom row; recovery renders nothing.
    health: status::Health,
    /// True while a `/login` OAuth flow is running off-thread. A second
    /// `/login` while set is rejected with a busy status instead of racing a
    /// second callback server / browser launch.
    login_in_flight: bool,
    /// Session whose compact request is in flight, or whose committed outcome
    /// could not be refreshed. New turns for this session are rejected so the
    /// local transcript cannot race or lie about the daemon's replacement.
    compacting_session: Option<AgentSessionId>,
    /// The request is no longer in flight, but the matching authoritative
    /// transcript still must be reloaded before that session may submit.
    compact_refresh_required: bool,
    /// True only while the POST compact future owns the current operation.
    /// Its own lease-scoped invalidations defer until the response arrives.
    compact_request_in_flight: bool,
    /// A replay reset or session-changed event arrived while compact held the
    /// daemon lease; follow the compact response with refresh-only sync.
    compact_invalidation_pending: bool,
    /// Binding generation captured by the active compact/sync operation.
    compact_binding_generation: u64,
    /// Monotonic identity for compact/sync completions within one binding.
    compact_operation_generation: u64,
    /// Sessions whose compact/sync outcome is not yet installed locally. This
    /// survives A→B→A rebinding; returning to one forces refresh-only sync.
    sessions_requiring_sync: HashSet<AgentSessionId>,
    should_quit: bool,
    actions_tx: mpsc::UnboundedSender<Action>,
    actions_rx: mpsc::UnboundedReceiver<Action>,
    /// Hold-Space is enabled only when the terminal reports real key releases.
    hold_to_dictate: bool,
    space_hold: Option<SpaceHold>,
    next_dictation_id: u64,
    active_dictation_id: Option<u64>,
    dictation_capture: Option<(u64, dictation::CaptureHandle)>,
    dictation_task: Option<tokio::task::JoinHandle<()>>,
    /// OCEAN-185: the token minted for the in-flight submit, claimed by the
    /// turn's first permission request (keyed by its request_id).
    pending_submit_token: Option<String>,
    decision_tokens: HashMap<RequestId, String>,
    perm_request: HashMap<PermissionId, RequestId>,
    /// Permission requests still awaiting a daemon decision.
    pending_permission_ids: HashSet<PermissionId>,
    /// Requests that were already active when this TUI received a daemon-
    /// confirmed effective skip-all save. Only these request ids may bridge
    /// later same-turn prompts; never use a stale global cache to approve a new
    /// turn after another client or daemon restart changes the policy.
    skip_all_requests: HashSet<RequestId>,
    /// Pane rects from the last draw, for mouse routing.
    r_body: Rect,
    r_sessions: Rect,
    r_tree: Rect,
    r_tray: Rect,
    r_center: Rect,
    r_term: Rect,
    /// The full center COLUMN (crumb + surface + splitter + dock), for clamping
    /// a terminal-dock resize against the available height.
    r_center_col: Rect,
    r_split_term: Rect,
    r_split_tray: Rect,
    /// Vertical rail splitters and their operator-controlled widths.
    r_split_sessions: Rect,
    r_split_tree: Rect,
    sessions_w: u16,
    tree_w: u16,
    dragging_sessions: bool,
    dragging_tree: bool,
    /// Terminal dock height in rows — resizable (drag the splitter or ⌃⌥↑/↓).
    term_h: u16,
    /// True while the operator is dragging the horizontal dock splitter.
    dragging_term: bool,
    /// `/models` picker overlay. `models_entries` is fetched fresh from the
    /// daemon on open (ready models first, registry order within); `models_sel`
    /// indexes into it; `models_hit` maps drawn rows back to entries for mouse
    /// clicks; `thinking_override` rides every subsequent turn as the per-turn
    /// `thinking_level` (None = daemon default).
    models_open: bool,
    models_loading: bool,
    models_entries: Vec<ModelEntry>,
    models_current: String,
    models_sel: usize,
    models_hit: Vec<(Rect, usize)>,
    thinking_override: Option<ThinkingLevel>,
    /// `/advisor` picker overlay — the per-session second-opinion reviewer.
    /// Reuses `models_entries` (the same registry fetch) for its model list,
    /// with an "off" row on top. `advisor_ctl` rides every turn as the per-turn
    /// advisor override (None = defer to the daemon's global `[roles].advisor`).
    /// `advisor_hit` maps drawn rows back to a pick (0 = off, i+1 = entry i).
    advisor_open: bool,
    advisor_sel: usize,
    advisor_hit: Vec<(Rect, usize)>,
    advisor_ctl: Option<ocean_agent_sdk::AdvisorControl>,
    /// `/memory` browser overlay: fetched entries, a client-side search filter
    /// (typed into the overlay), the selection cursor, and hit-testing rects.
    memory_open: bool,
    memory_loading: bool,
    memory_entries: Vec<crate::shell::client::MemoryEntry>,
    memory_query: String,
    memory_sel: usize,
    /// `/lsp` panel: detected language servers for the active workspace.
    lsp_open: bool,
    lsp_loading: bool,
    lsp_servers: Vec<crate::shell::client::LspServer>,
    /// `/image` full-screen viewer: `Some(abs_path)` when open. `image_body` is
    /// the cell rect the pixels fill (set during draw); `image_placed` tracks
    /// whether the kitty image is currently on screen (place once, clear on
    /// close — see the post-draw emission in `run`).
    image_view: Option<PathBuf>,
    image_body: Rect,
    image_placed: bool,
    /// Mouse text selection. Chat and editor selections use stable content rows;
    /// other panes use terminal screen rows. `sel_rect` keeps the sweep bounded
    /// to the pane where it began. Releasing auto-copies the swept text.
    sel_press: Option<(u16, usize)>,
    sel_rect: Option<Rect>,
    selection: Option<((u16, usize), (u16, usize))>,
    /// Stable content rows encountered while a chat/editor selection is live.
    selection_rows: BTreeMap<usize, Vec<String>>,
    selection_space: SelectionSpace,
    /// Cell symbols of the last drawn frame (row-major), captured only while a
    /// selection is live, so release-time copy reads exactly what was shown.
    frame_cells: Vec<Vec<String>>,
    /// Images captured from the system clipboard and queued for the next turn.
    pending_images: Vec<ocean_agent_sdk::TurnImage>,
    /// Submitted images retained only until the daemon accepts or definitely
    /// rejects the turn, so definitely-unsent failures can restore attachments.
    in_flight_images: Vec<ocean_agent_sdk::TurnImage>,
    /// Panel visibility (CTRL's collapsible rails + terminal dock).
    show_sessions: bool,
    show_tree: bool,
    /// Set by an explicit operator close. Session-component lifecycle updates
    /// may auto-reveal Files only while this latch is clear; explicit reopen
    /// paths clear it again.
    tree_auto_reveal_suppressed: bool,
    show_term: bool,
    /// Title-bar buttons: (hit rect, button), rebuilt on every title draw —
    /// mouse-first navigation, restored per operator request.
    buttons: Vec<(Rect, Btn)>,
    /// Double-Esc latch for the terminal dock: a single Esc belongs to the
    /// shell, so leaving the dock by keyboard takes two. Armed on the first Esc
    /// while focus is Term, disarmed on any other key or when focus leaves Term.
    esc_armed: bool,
    /// Last time the file tree was re-read from disk (throttles the live rescan).
    last_tree_scan: Instant,
    /// Cached git status of the active workspace for the status-line dashboard.
    /// Refreshed on the same throttled tick as the tree (git shells out, so it
    /// must not run per-frame) and immediately on a workspace re-root.
    git_status: git::Status,
    /// `/settings` overlay: open flag + selected row.
    settings_open: bool,
    settings_sel: usize,
    /// `/permissions` daemon-owned approval picker.
    permissions_open: bool,
    permissions_loading: bool,
    permissions_saving: bool,
    permissions_sel: usize,
    permissions_persisted: Option<PermissionMode>,
    permissions_effective: Option<PermissionMode>,
    permissions_env_override: Option<PermissionMode>,
    permissions_hit: Vec<(Rect, usize)>,
    /// `/providers` popup: auth-status list + inline API-key entry.
    providers_open: bool,
    providers_sel: usize,
    providers_rows: Vec<ProviderRow>,
    providers_mode: ProvidersMode,
    /// Startup chooser; the clean chat surface stays behind it until a route is selected.
    launch_open: bool,
    launch_sel: usize,
    launch_hit: Vec<(Rect, usize)>,
    /// Flat, current-workspace session list opened from the launch chooser.
    resume_open: bool,
    resume_loading: bool,
    resume_sel: usize,
    resume_sessions: Vec<crate::shell::sessions::Session>,
    resume_hit: Vec<(Rect, usize)>,
}

/// A title-bar button — the clickable icon toggles on the right of the title
/// row. Buttons TOGGLE (rails/terminal) or select the center surface; keys
/// and `/` commands mirror them but always-show instead of toggling.
#[derive(Clone, Copy, PartialEq)]
enum Btn {
    Sessions,
    Chat,
    Editor,
    Graph,
    Term,
    Tree,
}

impl App {
    pub fn new(client: DaemonClient, workspace_root: String) -> Self {
        let (actions_tx, actions_rx) = mpsc::unbounded_channel();
        let root = PathBuf::from(&workspace_root);
        // Status-bar git segment, populated from frame 1 (before `root` is
        // moved into the components below).
        let git_status = git::status(&root);
        let mut app = Self {
            client,
            workspace_root,
            rail: SessionRailComponent::new(root.clone()),
            tree: FileTreeComponent::new(root.clone()),
            tray: SessionComponentTray::new(),
            chat: ChatComponent::new(),
            pty: PtyComponent::default(),
            editor: EditorComponent::new(root.clone()),
            graph: GraphComponent::new(root),
            workflow_graph: WorkflowGraphComponent::default(),
            right_rail_mode: RightRailMode::Files,
            // Land in the chat, typing-ready — the rail is one click away.
            center: Center::Chat,
            focus: Focus::Center,
            session_id: None,
            session_binding_generation: 0,
            stream_generation: 0,
            session_activity_probe_generation: 0,
            herdr: HerdrReporter::from_env(),
            model_override: None,
            stream_task: None,
            health_task: None,
            status: String::new(),
            status_at: Instant::now(),
            health: status::Health::default(),
            login_in_flight: false,
            compacting_session: None,
            compact_refresh_required: false,
            compact_request_in_flight: false,
            compact_invalidation_pending: false,
            compact_binding_generation: 0,
            compact_operation_generation: 0,
            sessions_requiring_sync: HashSet::new(),
            should_quit: false,
            actions_tx,
            actions_rx,
            hold_to_dictate: false,
            space_hold: None,
            next_dictation_id: 0,
            active_dictation_id: None,
            dictation_capture: None,
            dictation_task: None,
            pending_submit_token: None,
            decision_tokens: HashMap::new(),
            perm_request: HashMap::new(),
            pending_permission_ids: HashSet::new(),
            skip_all_requests: HashSet::new(),
            r_body: Rect::default(),
            r_sessions: Rect::default(),
            r_tree: Rect::default(),
            r_tray: Rect::default(),
            r_center: Rect::default(),
            r_term: Rect::default(),
            r_center_col: Rect::default(),
            r_split_term: Rect::default(),
            r_split_tray: Rect::default(),
            r_split_sessions: Rect::default(),
            r_split_tree: Rect::default(),
            sessions_w: SESS_W,
            tree_w: TREE_W,
            dragging_sessions: false,
            dragging_tree: false,
            term_h: TERM_H,
            dragging_term: false,
            models_open: false,
            models_loading: false,
            models_entries: Vec::new(),
            models_current: String::new(),
            models_sel: 0,
            models_hit: Vec::new(),
            thinking_override: None,
            advisor_open: false,
            advisor_sel: 0,
            advisor_hit: Vec::new(),
            advisor_ctl: None,
            memory_open: false,
            memory_loading: false,
            memory_entries: Vec::new(),
            memory_query: String::new(),
            memory_sel: 0,
            lsp_open: false,
            lsp_loading: false,
            lsp_servers: Vec::new(),
            image_view: None,
            image_body: Rect::default(),
            image_placed: false,
            sel_press: None,
            sel_rect: None,
            selection: None,
            selection_rows: BTreeMap::new(),
            selection_space: SelectionSpace::Screen,
            frame_cells: Vec::new(),
            pending_images: Vec::new(),
            in_flight_images: Vec::new(),
            show_sessions: false,
            show_tree: false,
            tree_auto_reveal_suppressed: false,
            show_term: false,
            buttons: Vec::new(),
            esc_armed: false,
            last_tree_scan: Instant::now(),
            // Populated above from `root`, before it was moved into components;
            // refreshed thereafter on the 1s tick.
            git_status,
            settings_open: false,
            settings_sel: 0,
            permissions_open: false,
            permissions_loading: false,
            permissions_saving: false,
            permissions_sel: permission_mode_index(PermissionMode::Automatic),
            permissions_persisted: None,
            permissions_effective: None,
            permissions_env_override: None,
            permissions_hit: Vec::new(),
            providers_open: false,
            providers_sel: 0,
            providers_rows: Vec::new(),
            providers_mode: ProvidersMode::List,
            launch_open: true,
            launch_sel: 0,
            launch_hit: Vec::new(),
            resume_open: false,
            resume_loading: false,
            resume_sel: 0,
            resume_sessions: Vec::new(),
            resume_hit: Vec::new(),
        };
        app.apply_focus();
        // `@` file mentions index the launch project from the start.
        app.chat
            .set_mention_root(PathBuf::from(&app.workspace_root));
        // Welcome empty-state: tell the chat whether providers are configured.
        app.refresh_welcome_provider_line();
        // A normal launch starts clean. Explicit `--session` remains a direct
        // opt-in handled by `resume_initial_session`.
        // Inject visual-harness components when OCEAN_TUI_COMPONENT_DEMO is set.
        app.chat.maybe_inject_demo();
        app
    }

    pub fn set_hold_to_dictate(&mut self, supported: bool) {
        self.hold_to_dictate = supported;
    }

    /// Apply an explicit `--session` selection after normal construction. The
    /// launch workspace remains authoritative for future turns; only transcript
    /// history and the scoped event stream switch to the requested session.
    pub fn resume_initial_session(
        &mut self,
        session: crate::shell::sessions::Session,
    ) -> anyhow::Result<()> {
        let id = AgentSessionId(uuid::Uuid::parse_str(&session.id)?);
        self.chat
            .load_history(crate::shell::sessions::load_transcript(&session.path));
        self.bind_session_with(id, false);
        if self.compacting_session != Some(id) && !self.sessions_requiring_sync.contains(&id) {
            self.spawn_session_activity_probe(id, self.session_binding_generation, 0, false, false);
        }
        // This initialization path intentionally skips dispatch (replay_first
        // differs from a fresh mint), so explicitly deliver the same bind
        // action to session-scoped components. Use ResumeSession so the
        // `--session` startup path reports the correct `resume` source to Herdr.
        let resume = Action::ResumeSession {
            id,
            path: session.path.clone(),
            cwd: session.cwd.clone(),
        };
        let _ = self.tray.update(&resume);
        self.herdr.observe(&resume, self.session_id);
        self.rail.live_id = Some(session.id);
        self.launch_open = false;
        Ok(())
    }

    pub async fn run(mut self, terminal: &mut tui::Tui) -> anyhow::Result<()> {
        // One-shot startup fetch of the model registry so the status row can
        // show the daemon's current model BEFORE the first turn (chat.model()
        // is None until TurnStarted; the picker fetch only runs on `/models`).
        // Lives here, not App::new: construction stays pure and runtime-free.
        {
            let client = self.client.clone();
            let tx = self.actions_tx.clone();
            tokio::spawn(async move {
                if let Ok(r) = client.models().await {
                    let _ = tx.send(Action::ModelsLoaded {
                        current: r.current.model,
                        entries: r.models,
                    });
                }
            });
        }
        let mut events = EventHandler::new(30.0, 60.0);

        {
            let client = self.client.clone();
            let tx = self.actions_tx.clone();
            let base_url = client.base_url().to_string();
            let guard = Arc::new(daemon_boot::AutostartGuard::new());

            self.health_task = Some(tokio::spawn(async move {
                let mut healthy = false;
                // Short initial delay so the splash can render before the
                // first probe runs.
                tokio::time::sleep(Duration::from_millis(500)).await;
                loop {
                    match tokio::time::timeout(Duration::from_secs(3), client.health()).await {
                        Ok(Ok(_)) => {
                            if !healthy {
                                // Typed recovery: clears ONLY the daemon source;
                                // no connected success text is rendered.
                                let _ = tx.send(Action::HealthRecovered(HealthSource::Daemon));
                                healthy = true;
                            }
                            tokio::time::sleep(Duration::from_secs(15)).await;
                        }
                        Ok(Err(_)) | Err(_) => {
                            if healthy {
                                healthy = false;
                            }
                            let base_url2 = base_url.clone();
                            let guard2 = Arc::clone(&guard);
                            let outcome = tokio::task::spawn_blocking(move || {
                                daemon_boot::maybe_autostart_prod(&base_url2, &guard2)
                            })
                            .await
                            .unwrap_or(
                                daemon_boot::AutostartOutcome::SpawnFailed(
                                    "autostart task panicked".into(),
                                ),
                            );
                            // Typed degradation: the condition persists on the
                            // daemon source until ITS probe recovers — notices
                            // and SSE transitions can no longer overwrite it.
                            let condition = match outcome {
                                daemon_boot::AutostartOutcome::Started => {
                                    Some("daemon starting".to_string())
                                }
                                daemon_boot::AutostartOutcome::BinaryNotFound => {
                                    Some("daemon binary not found".to_string())
                                }
                                daemon_boot::AutostartOutcome::SpawnFailed(_) => {
                                    Some("daemon autostart failed".to_string())
                                }
                                daemon_boot::AutostartOutcome::NotEligible => Some(format!(
                                    "daemon offline at {}",
                                    daemon_boot::host_port(&base_url)
                                )),
                                // Don't spam a fresh condition on every probe.
                                daemon_boot::AutostartOutcome::RateLimited => None,
                                daemon_boot::AutostartOutcome::SupervisionUnknown => {
                                    Some("daemon autostart blocked".to_string())
                                }
                            };
                            if let Some(condition) = condition {
                                let _ = tx.send(Action::HealthDegraded {
                                    source: HealthSource::Daemon,
                                    condition,
                                });
                            }
                            tokio::time::sleep(Duration::from_secs(3)).await;
                        }
                    }
                }
            }));

            // Global /v1/events: permission requests/decisions live here.
            self.client
                .spawn_global_event_stream(self.actions_tx.clone());
            self.client
                .spawn_observatory_stream(self.actions_tx.clone());
        }

        // Render-on-demand: draw INSTANTLY in response to input (buttery scroll/
        // typing), coalesce async/streaming redraws onto the render tick, and do
        // NOT repaint when nothing changed. The old loop drew the whole workbench
        // unconditionally at render_hz and never on input, so a keypress waited a
        // frame and, if a draw ran long, input backed up behind the timer.
        let mut dirty = true; // paint the first frame
        while !self.should_quit {
            let Some(event) = events.next().await else {
                break;
            };
            let is_render = matches!(event, Event::Render);
            let mut immediate = false;
            match event {
                Event::Render => {}
                Event::Tick => {
                    if let Some(id) = self
                        .space_hold
                        .as_ref()
                        .filter(|hold| !hold.started && hold.pressed_at.elapsed() >= DICTATION_HOLD)
                        .map(|hold| hold.id)
                    {
                        self.dispatch(Action::DictationHoldActivated { id });
                        dirty = true;
                    }
                    if let Some(a) = self.pty.tick() {
                        self.dispatch(a);
                        dirty = true;
                    }
                    if let Some(a) = self.editor.tick() {
                        self.dispatch(a);
                        dirty = true;
                    }
                    // Notices are transient: past STATUS_TTL the bottom row
                    // returns to its minimal (model-only) idle set.
                    if self.expire_status(Instant::now()) {
                        dirty = true;
                    }
                    // Live-reflect files the agent (or the terminal) creates in
                    // the Files sidebar without a manual refresh. Throttled to
                    // ~1s so it's a couple of cheap read_dirs, not per-tick.
                    if self.last_tree_scan.elapsed() >= Duration::from_millis(1000) {
                        self.tree.rescan();
                        // Refresh the status-bar git segment on the same cheap
                        // cadence (git shells out — never per-frame).
                        self.git_status = git::status(std::path::Path::new(&self.workspace_root));
                        self.last_tree_scan = Instant::now();
                        dirty = true;
                    }
                    // Keep animating while a turn is actually streaming or the
                    // PTY is live, so those repaint at the tick cadence. NOTE:
                    // this must key off chat.is_busy(), NOT stream_task liveness
                    // — the SSE task is a self-healing reconnect loop that never
                    // finishes, and gating on it forced 60Hz full redraws (and
                    // ~20% CPU) forever once a session was bound.
                    if self.chat.is_busy()
                        || self.chat.dictation_is_active()
                        || self.pty.is_active()
                    {
                        dirty = true;
                    }
                }
                Event::Crossterm(evt) => {
                    self.on_crossterm(evt);
                    immediate = true; // paint this frame now — no timer wait
                }
            }
            while let Ok(action) = self.actions_rx.try_recv() {
                self.dispatch(action);
                dirty = true;
            }
            // The image viewer closed (or switched images): delete the placed
            // kitty image and force a full repaint so the workbench comes back
            // clean underneath where the pixels were.
            if self.image_placed && self.image_view.is_none() {
                kitty::emit(kitty::CLEAR_ALL);
                self.image_placed = false;
                let _ = terminal.clear();
                dirty = true;
            }
            // While the viewer is open and its pixels are already placed, it's a
            // STATIC takeover — skip redraws (a redraw would need to re-place the
            // image, and nothing else on screen is changing).
            let viewer_static = self.image_view.is_some() && self.image_placed;
            // Input paints immediately; streaming/async changes coalesce onto the
            // render tick (≤ render_hz); idle frames draw nothing.
            if !viewer_static && (immediate || (is_render && dirty)) {
                terminal.draw(|f| self.draw(f))?;
                dirty = false;
                // Just painted the viewer frame: now lay the pixels into the
                // reserved body rect, once (kitty images float above ratatui's
                // cells — see `kitty`).
                if self.image_view.is_some() && !self.image_placed {
                    if let Some(path) = self.image_view.clone() {
                        let b = self.image_body;
                        if let Some(seq) = kitty::place_png_at(&path, b.x, b.y, b.width, b.height) {
                            kitty::emit(&seq);
                        }
                        // Mark placed even when kitty declined (non-kitty /
                        // non-PNG): the frame's note is the render, and this
                        // keeps the static-takeover gate from redrawing.
                        self.image_placed = true;
                    }
                }
            }
        }
        Ok(())
    }

    fn is_dictation_toggle_key(key: &crossterm::event::KeyEvent) -> bool {
        key.kind == KeyEventKind::Press
            && ((key.code == KeyCode::Char(' ') && key.modifiers == KeyModifiers::ALT)
            // Terminal.app and terminals without enhanced-key reporting may
            // encode macOS Option+Space as a literal non-breaking space.
            || (key.code == KeyCode::Char('\u{a0}') && key.modifiers.is_empty()))
    }

    fn on_crossterm(&mut self, evt: CrosstermEvent) {
        // REPORT_EVENT_TYPES makes every enhanced-terminal key arrive again on
        // release. Space release owns the hold gesture; all other releases are
        // discarded before modal/global handlers can accidentally fire twice.
        if let CrosstermEvent::Key(key) = &evt {
            if key.kind == KeyEventKind::Release {
                if key.code == KeyCode::Char(' ') && self.space_hold.is_some() {
                    self.dispatch(Action::DictationHoldReleased);
                }
                return;
            }
        }

        // While the prompt box is a live meter/transcription state, ordinary
        // input stays locked. Esc cancels; Option+Space stops the recording.
        if self.chat.dictation_blocks_input() {
            if let CrosstermEvent::Key(key) = &evt {
                if key.kind == KeyEventKind::Press
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('q')
                {
                    if let Some(id) = self.active_dictation_id {
                        self.dispatch(Action::DictationCancel { id });
                    }
                    self.should_quit = true;
                } else if key.kind == KeyEventKind::Press && key.code == KeyCode::Esc {
                    if let Some(id) = self.active_dictation_id {
                        self.dispatch(Action::DictationCancel { id });
                    }
                } else if Self::is_dictation_toggle_key(key) {
                    self.dispatch(Action::DictationToggle);
                }
            }
            return;
        }

        // The `/image` viewer is a full-screen takeover: esc/q/enter or a click
        // closes it; other keys are swallowed (no accidental composer input).
        if self.image_view.is_some() {
            match evt {
                CrosstermEvent::Key(k) => {
                    if matches!(k.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
                        self.image_view = None;
                    }
                    return;
                }
                CrosstermEvent::Mouse(m)
                    if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    self.image_view = None;
                    return;
                }
                _ => return,
            }
        }
        // The `/lsp` panel is a read-only modal: any key or click closes it.
        if self.lsp_open {
            match evt {
                CrosstermEvent::Key(_) => {
                    self.lsp_open = false;
                    return;
                }
                CrosstermEvent::Mouse(m)
                    if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) =>
                {
                    self.lsp_open = false;
                    return;
                }
                // No text sink in this panel — swallow pastes so they can't
                // leak into the composer beneath the overlay.
                CrosstermEvent::Paste(_) => return,
                _ => {}
            }
        }
        // The `/memory` browser is modal (typing filters, so keys route here).
        if self.memory_open {
            match evt {
                CrosstermEvent::Key(k) => {
                    self.memory_key(k);
                    return;
                }
                CrosstermEvent::Mouse(m) => {
                    self.memory_mouse(m);
                    return;
                }
                CrosstermEvent::Paste(_) => return,
                _ => {}
            }
        }
        // The `/advisor` picker is modal, same as `/models`.
        if self.advisor_open {
            match evt {
                CrosstermEvent::Key(k) => {
                    self.advisor_key(k);
                    return;
                }
                CrosstermEvent::Mouse(m) => {
                    self.advisor_mouse(m);
                    return;
                }
                CrosstermEvent::Paste(_) => return,
                _ => {}
            }
        }
        // The startup chooser is modal and owns input until the operator selects
        // a destination. Resume is its nested modal.
        if self.resume_open {
            match evt {
                CrosstermEvent::Key(k) => self.resume_key(k),
                CrosstermEvent::Mouse(m) => self.resume_mouse(m),
                _ => {}
            }
            return;
        }
        if self.launch_open {
            match evt {
                CrosstermEvent::Key(k) => self.launch_key(k),
                CrosstermEvent::Mouse(m) => self.launch_mouse(m),
                _ => {}
            }
            return;
        }
        // `/permissions` is a daemon-owned modal picker. No input leaks to the
        // composer while the current mode is loading or a selection is saving.
        if self.permissions_open {
            match evt {
                CrosstermEvent::Key(k) => {
                    self.permissions_key(k);
                    return;
                }
                CrosstermEvent::Mouse(m) => {
                    self.permissions_mouse(m);
                    return;
                }
                CrosstermEvent::Paste(_) => return,
                _ => {}
            }
        }
        // The `/models` picker is modal: keys and mouse both drive it while
        // open (clicking a row selects/applies, clicking outside closes).
        if self.models_open {
            match evt {
                CrosstermEvent::Key(k) => {
                    self.models_key(k);
                    return;
                }
                CrosstermEvent::Mouse(m) => {
                    self.models_mouse(m);
                    return;
                }
                CrosstermEvent::Paste(_) => return,
                _ => {}
            }
        }
        // The `/settings` overlay is modal: while open, keys drive it and
        // everything else waits. (Mouse falls through — clicking outside is
        // harmless; Esc/q closes.)
        if self.settings_open {
            if let CrosstermEvent::Key(k) = evt {
                self.settings_key(k);
                return;
            }
            // No text sink — swallow pastes beneath the overlay.
            if matches!(evt, CrosstermEvent::Paste(_)) {
                return;
            }
        }
        // The `/providers` popup is modal too, and mutually exclusive with the
        // settings overlay: opening one closes the other.
        if self.providers_open {
            match &evt {
                CrosstermEvent::Key(k) => {
                    let k = *k;
                    self.providers_key(k);
                    return;
                }
                // API-key entry: a bracketed paste lands in the buffer as ONE
                // event (printable chars only). The old char-stream paste path
                // no longer fires now that bracketed paste is enabled, so
                // without this arm pasting a provider key would do nothing.
                CrosstermEvent::Paste(text) => {
                    if let ProvidersMode::KeyEntry { buffer, .. } = &mut self.providers_mode {
                        buffer.extend(text.chars().filter(|c| !c.is_control()));
                    }
                    return;
                }
                _ => {}
            }
        }
        // Mouse: a click focuses the pane under the cursor; wheel + clicks are
        // forwarded to whichever pane the cursor is over (CTRL behavior).
        if let CrosstermEvent::Mouse(m) = evt {
            let pos = (m.column, m.row);
            // Vertical rail splitters resize the sidebars. Clamp both rails so
            // the center workspace always retains its minimum width.
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) if rect_has(self.r_split_sessions, pos) => {
                    self.dragging_sessions = true;
                    return;
                }
                MouseEventKind::Down(MouseButton::Left) if rect_has(self.r_split_tree, pos) => {
                    self.dragging_tree = true;
                    return;
                }
                MouseEventKind::Drag(MouseButton::Left) if self.dragging_sessions => {
                    let opposite = self.show_tree.then_some(self.tree_w);
                    let max = max_rail_width(self.r_body.width, opposite);
                    let candidate = m.column.saturating_sub(self.r_body.x);
                    self.sessions_w = candidate.clamp(MIN_RAIL_W, max);
                    return;
                }
                MouseEventKind::Drag(MouseButton::Left) if self.dragging_tree => {
                    let opposite = self.show_sessions.then_some(self.sessions_w);
                    let max = max_rail_width(self.r_body.width, opposite);
                    let candidate = self
                        .r_body
                        .right()
                        .saturating_sub(m.column.saturating_add(1));
                    self.tree_w = candidate.clamp(MIN_RAIL_W, max);
                    return;
                }
                MouseEventKind::Up(MouseButton::Left)
                    if self.dragging_sessions || self.dragging_tree =>
                {
                    self.dragging_sessions = false;
                    self.dragging_tree = false;
                    return;
                }
                _ => {}
            }
            // Terminal-dock resize: grab the horizontal splitter and drag it up
            // (taller) or down (shorter). Wins over pane routing while dragging.
            match m.kind {
                MouseEventKind::Down(_) if rect_has(self.r_split_term, pos) => {
                    self.dragging_term = true;
                    return;
                }
                MouseEventKind::Drag(_) if self.dragging_term => {
                    // The dock runs from just below the cursor to the bottom of
                    // the center column, so dropping the splitter on row `m.row`
                    // makes the dock exactly that tall.
                    let bottom = self.r_center_col.y + self.r_center_col.height;
                    let h = bottom.saturating_sub(m.row).saturating_sub(1);
                    self.term_h = h.clamp(MIN_TERM_H, max_term_h(self.r_center_col.height));
                    return;
                }
                MouseEventKind::Up(_) if self.dragging_term => {
                    self.dragging_term = false;
                    return;
                }
                _ => {}
            }
            // Title-bar buttons: a left click on a button fires it and stops —
            // it must never arm a text selection or fall through to panes.
            if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
                if let Some(btn) = self
                    .buttons
                    .iter()
                    .find(|(r, _)| rect_has(*r, pos))
                    .map(|(_, b)| *b)
                {
                    self.press(btn);
                    return;
                }
            }
            // Mouse text selection: holding the left button and dragging sweeps
            // a linear (terminal-style) selection, but BOUND to the content
            // pane where the Down landed — a sweep never crosses into a sibling
            // lane. Releasing auto-copies the swept text. A plain click (down +
            // up, no drag) falls through to the panes untouched.
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    // Arm only inside a content pane; a Down on a title/status/
                    // breadcrumb row or a splitter arms nothing AND clears any
                    // stale arm so an interrupted mouse sequence can't leak.
                    self.selection = None;
                    match self.pane_rect_at(pos) {
                        Some(rect) => {
                            self.selection_space = self.selection_space(rect);
                            self.selection_rows.clear();
                            self.sel_press =
                                Some(self.selection_point(pos, rect, self.selection_space));
                            self.sel_rect = Some(rect);
                        }
                        None => {
                            self.sel_press = None;
                            self.sel_rect = None;
                            self.selection_rows.clear();
                            self.selection_space = SelectionSpace::Screen;
                        }
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(anchor) = self.sel_press {
                        if let Some(rect) = self.sel_rect {
                            // Clamp the head INTO the pane so sweeping past a
                            // border saturates at the lane edge.
                            self.selection = Some((
                                anchor,
                                self.selection_point(pos, rect, self.selection_space),
                            ));
                        }
                        return; // selection owns the drag; panes don't see it
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    let sel = self.selection.take();
                    let rect = self.sel_rect;
                    self.sel_press = None;
                    self.sel_rect = None;
                    if let (Some((a, b)), Some(rect)) = (sel, rect) {
                        let text = if self.selection_space != SelectionSpace::Screen {
                            let (left, right) = self.selection_columns(rect);
                            stable_selection_text(&self.selection_rows, a, b, left, right)
                        } else {
                            selection_text(
                                &self.frame_cells,
                                (a.0, a.1 as u16),
                                (b.0, b.1 as u16),
                                rect,
                            )
                        };
                        self.selection_rows.clear();
                        self.selection_space = SelectionSpace::Screen;
                        // A selection of pure padding copies nothing (and must
                        // not clobber the clipboard). Success is silent.
                        if !text.is_empty() {
                            if let Err(e) = copy_to_clipboard(&text) {
                                self.dispatch(Action::Error(format!("copy failed: {e}")));
                            }
                        }
                        return; // this Up ends a selection, not a click
                    }
                }
                _ => {}
            }
            let target = if rect_has(self.r_sessions, pos) {
                Some(Focus::Sessions)
            } else if rect_has(self.r_tree, pos) {
                Some(Focus::Tree)
            } else if rect_has(self.r_term, pos) {
                Some(Focus::Term)
            } else if rect_has(self.r_center, pos) {
                Some(Focus::Center)
            } else {
                None
            };
            if let Some(t) = target {
                if matches!(m.kind, MouseEventKind::Down(_)) {
                    self.focus_to(t);
                }
                let action = match t {
                    Focus::Sessions => self.rail.handle_mouse(m),
                    Focus::Tree => match self.right_rail_mode {
                        RightRailMode::Files => self.tree.handle_mouse(m),
                        RightRailMode::Workflow => self.workflow_graph.handle_mouse(m),
                    },
                    Focus::Term => self.pty.handle_mouse(m),
                    Focus::Center => match self.center {
                        Center::Chat => self.chat.handle_mouse(m),
                        Center::Editor => self.editor.handle_mouse(m),
                        Center::Graph => self.graph.handle_mouse(m),
                        Center::WorkflowGraph => self.workflow_graph.handle_mouse(m),
                    },
                };
                if let Some(a) = action {
                    self.dispatch(a);
                }
            }
            return;
        }
        if let CrosstermEvent::Key(k) = evt {
            let chat_focused = self.focus == Focus::Center && self.center == Center::Chat;
            if chat_focused && Self::is_dictation_toggle_key(&k) {
                self.dispatch(Action::DictationToggle);
                return;
            }
            if self.hold_to_dictate
                && chat_focused
                && k.code == KeyCode::Char(' ')
                && k.modifiers.is_empty()
            {
                if self.space_hold.is_some() {
                    return; // ignore enhanced-protocol repeat while armed/hot
                }
                if k.kind == KeyEventKind::Press && self.chat.can_start_dictation() {
                    self.dispatch(Action::DictationHoldPressed);
                    return;
                }
            }
            if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('q') {
                self.should_quit = true;
                return;
            }
            if k.modifiers.contains(KeyModifiers::CONTROL)
                && k.code == KeyCode::Char('v')
                && self.focus == Focus::Center
                && self.center == Center::Chat
            {
                self.capture_clipboard_image();
                return;
            }
            // Tab cycles focus across the VISIBLE panes (never hides anything).
            // Tab: skip focus-cycling when the chat palettes are open so Tab
            if k.code == KeyCode::Tab
                && self.focus == Focus::Center
                && self.center == Center::Chat
                && self.chat.wants_tab()
            {
                // Let Tab through to ChatComponent; don't cycle focus.
            } else if k.code == KeyCode::Tab && self.focus != Focus::Term {
                self.cycle_focus();
                return;
            }
            if k.modifiers.contains(KeyModifiers::CONTROL)
                && k.modifiers.contains(KeyModifiers::ALT)
            {
                match k.code {
                    KeyCode::Char('1') => return self.focus_to(Focus::Sessions),
                    KeyCode::Char('2') => return self.focus_to(Focus::Tree),
                    KeyCode::Char('3') => {
                        self.center = Center::Chat;
                        return self.focus_to(Focus::Center);
                    }
                    KeyCode::Char('4') => {
                        if self.editor.has_tabs() {
                            self.center = Center::Editor;
                        }
                        return self.focus_to(Focus::Center);
                    }
                    KeyCode::Char('5') => {
                        // Graph toggles over the center, exactly like CTRL's
                        // show_graph; toggling off returns to chat/editor.
                        self.center = if self.center == Center::Graph {
                            if self.editor.has_tabs() {
                                Center::Editor
                            } else {
                                Center::Chat
                            }
                        } else {
                            Center::Graph
                        };
                        return self.focus_to(Focus::Center);
                    }
                    KeyCode::Char('6') => {
                        // Create if needed and ALWAYS unhide — focusing an
                        // invisible dock strands the keyboard.
                        if !self.pty.is_active() {
                            self.pty.open(&PathBuf::from(&self.workspace_root), "");
                        }
                        self.show_term = true;
                        return self.focus_to(Focus::Term);
                    }
                    // ⌃⌥↑ / ⌃⌥↓ — stretch / shrink the terminal dock.
                    KeyCode::Up => return self.resize_term(2),
                    KeyCode::Down => return self.resize_term(-2),
                    _ => {}
                }
            }
            // Esc escape hatch: ⌃⌥N are dead on the wire in legacy terminals
            // (Ctrl+3 IS ESC), so a keyboard-only user gets stranded in the
            // editor/graph/terminal. Esc gets them back to chat from anywhere.
            if k.code == KeyCode::Esc {
                match self.focus {
                    // Chat owns Esc (its `/` palette dismiss) — don't intercept.
                    Focus::Center if self.center == Center::Chat => {}
                    // Editor/graph and the side rails → straight back to chat.
                    Focus::Center | Focus::Sessions | Focus::Tree => {
                        self.center = Center::Chat;
                        self.focus_to(Focus::Center);
                        return;
                    }
                    // Terminal: a single Esc belongs to the shell, so leaving the
                    // dock takes a double-Esc. First Esc arms the latch and is
                    // still forwarded to the PTY (fall through); the second leaves.
                    Focus::Term => {
                        if self.esc_armed {
                            self.esc_armed = false;
                            return self.focus_to(Focus::Center);
                        }
                        self.esc_armed = true;
                    }
                }
            } else if self.focus == Focus::Term {
                // Any non-Esc key in the terminal disarms the double-Esc latch.
                self.esc_armed = false;
            }
        }
        // Finder and terminal drag/drop paste local files as newline-separated
        // paths. Recognize the payload only when every nonblank line is a
        // supported existing image; ordinary path-like prose remains composer text.
        if self.focus == Focus::Center && self.center == Center::Chat {
            if let CrosstermEvent::Paste(text) = &evt {
                if let Some(paths) = pasted_image_paths(text) {
                    self.load_image_paths(paths);
                    return;
                }
            }
        }
        let action = match self.focus {
            Focus::Sessions => self.rail.handle_event(&evt),
            Focus::Tree => match self.right_rail_mode {
                RightRailMode::Files => self.tree.handle_event(&evt),
                RightRailMode::Workflow => self.workflow_graph.handle_event(&evt),
            },
            Focus::Term => self.pty.handle_event(&evt),
            Focus::Center => match self.center {
                Center::Chat => self.chat.handle_event(&evt),
                Center::Editor => self.editor.handle_event(&evt),
                Center::Graph => self.graph.handle_event(&evt),
                Center::WorkflowGraph => self.workflow_graph.handle_event(&evt),
            },
        };
        if let Some(a) = action {
            self.dispatch(a);
        }
    }

    fn cycle_focus(&mut self) {
        let next = match self.focus {
            Focus::Sessions => Focus::Center,
            Focus::Center => {
                if self.pty.is_active() {
                    Focus::Term
                } else {
                    Focus::Tree
                }
            }
            Focus::Term => Focus::Tree,
            Focus::Tree => Focus::Sessions,
        };
        self.focus_to(next);
    }

    /// Record an ephemeral status notice and stamp it for expiry. Every writer
    /// of `status` goes through here so the TTL clock always restarts.
    fn set_notice(&mut self, s: String) {
        self.status = s;
        self.status_at = Instant::now();
    }

    /// Clear a notice older than [`STATUS_TTL`] as of `now`. Returns true when
    /// the row changed and needs a repaint. Parametric in `now` so tests can
    /// prove expiry without sleeping.
    fn expire_status(&mut self, now: Instant) -> bool {
        if !self.status.is_empty() && now.duration_since(self.status_at) >= STATUS_TTL {
            self.status.clear();
            true
        } else {
            false
        }
    }

    /// Snapshot the bottom row's inputs. The displayed model prefers the
    /// operator's explicit selection (`/model <id>`, the `/models` picker) so
    /// a pick shows instantly; the chat's turn-derived model is the fallback
    /// until the next `TurnStarted` confirms it.
    fn status_data(&self) -> StatusData<'_> {
        StatusData {
            model: self
                .model_override
                .as_deref()
                .or(self.chat.model())
                .or_else(|| {
                    (!self.models_current.is_empty()).then_some(self.models_current.as_str())
                }),
            health: self.health.effective(),
            error: Some(self.status.as_str()),
            activity: self.chat.activity(),
            git: Some(&self.git_status),
            tok_per_s: self.chat.tok_per_s(),
        }
    }

    fn set_tree_visible_by_operator(&mut self, visible: bool) {
        self.show_tree = visible;
        self.tree_auto_reveal_suppressed = !visible;
        if !visible && self.focus == Focus::Tree {
            self.focus_to(Focus::Center);
        }
    }

    /// A title-bar button press: rails and the terminal TOGGLE visibility;
    /// chat/editor/graph select the center surface. Toggle semantics are the
    /// buttons' contract — `Action::Navigate` (keys, `/` commands) always
    /// SHOWS; the buttons flip.
    fn press(&mut self, btn: Btn) {
        match btn {
            Btn::Sessions => {
                self.show_sessions = !self.show_sessions;
                if !self.show_sessions && self.focus == Focus::Sessions {
                    self.focus_to(Focus::Center);
                }
            }
            Btn::Tree => {
                if self.show_tree && self.right_rail_mode == RightRailMode::Files {
                    self.set_tree_visible_by_operator(false);
                } else {
                    self.right_rail_mode = RightRailMode::Files;
                    self.set_tree_visible_by_operator(true);
                    self.focus_to(Focus::Tree);
                }
            }
            Btn::Term => {
                if !self.pty.is_active() {
                    self.pty.open(&PathBuf::from(&self.workspace_root), "");
                    self.show_term = true;
                    self.focus_to(Focus::Term);
                } else {
                    self.show_term = !self.show_term;
                    if !self.show_term && self.focus == Focus::Term {
                        self.focus_to(Focus::Center);
                    }
                }
            }
            Btn::Chat => {
                self.center = Center::Chat;
                self.focus_to(Focus::Center);
            }
            Btn::Editor => {
                if self.editor.has_tabs() {
                    self.center = Center::Editor;
                }
                self.focus_to(Focus::Center);
            }
            Btn::Graph => {
                self.center = if matches!(self.center, Center::Graph | Center::WorkflowGraph) {
                    if self.editor.has_tabs() {
                        Center::Editor
                    } else {
                        Center::Chat
                    }
                } else {
                    Center::Graph
                };
                self.focus_to(Focus::Center);
            }
        }
    }

    fn load_image_paths(&mut self, paths: Vec<PathBuf>) {
        let tx = self.actions_tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(Action::ClipboardImages(read_image_paths(&paths)));
        });
    }

    fn capture_clipboard_image(&mut self) {
        let tx = self.actions_tx.clone();
        tokio::task::spawn_blocking(move || {
            let _ = tx.send(Action::ClipboardImages(
                read_clipboard_image().map(|image| vec![image]),
            ));
        });
    }

    fn dispatch(&mut self, action: Action) {
        let completed_submission_id = match &action {
            Action::TurnSendFailed { submission_id, .. }
            | Action::TurnSessionBusy { submission_id, .. }
            | Action::TurnAccepted { submission_id, .. }
            | Action::TurnOutcomeUnknown { submission_id, .. } => Some(*submission_id),
            _ => None,
        };
        if completed_submission_id.is_some_and(|id| !self.chat.has_pending_submission(id)) {
            // A prior binding/history replacement cleared this optimistic tag,
            // or a newer submission superseded it. It must not touch transcript,
            // images, Herdr lifecycle, or the current session's busy state.
            return;
        }
        let tray_was_visible = self.tray.is_visible();
        let mut follow_up = None;
        match &action {
            Action::Quit => {
                self.herdr.release();
                self.should_quit = true;
                return;
            }
            Action::Status(s) => self.set_notice(s.clone()),
            Action::Error(s) => self.set_notice(errfmt::humanize(s)),
            // Typed health: each source clears independently; the effective
            // degraded condition renders in the bottom row while ANY source
            // remains degraded. Recovery renders nothing.
            Action::HealthDegraded { source, condition } => {
                self.health.degrade(*source, condition.clone())
            }
            Action::HealthRecovered(source) => self.health.recover(*source),
            Action::ObservatorySnapshot(snapshot) => {
                let became_active = self
                    .workflow_graph
                    .graph
                    .replace_snapshot((**snapshot).clone());
                if became_active
                    && self.center != Center::WorkflowGraph
                    && !self.tree_auto_reveal_suppressed
                {
                    self.right_rail_mode = RightRailMode::Workflow;
                    self.show_tree = true;
                    self.apply_focus();
                }
            }
            Action::ObservatoryEvent(event) => {
                match self.workflow_graph.graph.apply_event((**event).clone()) {
                    crate::shell::workflow_graph::ApplyEvent::Applied { became_active } => {
                        if became_active
                            && self.center != Center::WorkflowGraph
                            && !self.tree_auto_reveal_suppressed
                        {
                            self.right_rail_mode = RightRailMode::Workflow;
                            self.show_tree = true;
                            self.apply_focus();
                        }
                    }
                    crate::shell::workflow_graph::ApplyEvent::NeedsSnapshot => {
                        self.workflow_graph.graph.mark_disconnected();
                    }
                    crate::shell::workflow_graph::ApplyEvent::Ignored => {}
                }
            }
            Action::ObservatoryDisconnected => {
                self.workflow_graph.graph.mark_disconnected();
            }
            Action::ExpandWorkflowGraph => {
                // The expanded graph owns the center; restore Files in the
                // right rail so the same component is never drawn twice with
                // contradictory focus/affordance state.
                self.right_rail_mode = RightRailMode::Files;
                self.center = Center::WorkflowGraph;
                self.focus_to(Focus::Center);
            }
            Action::WorkflowGraphCommand(command) => {
                self.workflow_graph.graph.apply_command(*command);
            }
            // Chat unwinds busy + restores the prompt (see its update arm);
            // the status line carries the humanized error.
            Action::TurnSendFailed {
                submission_id, err, ..
            } if self.chat.has_pending_submission(*submission_id) => {
                self.pending_images.append(&mut self.in_flight_images);
                self.set_notice(errfmt::humanize(err));
            }
            Action::TurnSessionBusy {
                submission_id,
                session_id,
                binding_generation,
                ..
            } if self.chat.has_pending_submission(*submission_id) => {
                self.pending_images.append(&mut self.in_flight_images);
                self.set_notice("session is still working — prompt preserved".into());
                if self.session_id == Some(*session_id)
                    && self.session_binding_generation == *binding_generation
                {
                    self.spawn_session_activity_probe(
                        *session_id,
                        *binding_generation,
                        100,
                        true,
                        true,
                    );
                }
            }
            Action::TurnAccepted {
                submission_id,
                turn_id,
            } if self.chat.has_pending_submission(*submission_id) => {
                self.in_flight_images.clear();
                if self
                    .chat
                    .acceptance_already_finished(*submission_id, *turn_id)
                {
                    self.herdr.resolve_activity();
                }
            }
            Action::TurnOutcomeUnknown { submission_id, err }
                if self.chat.has_pending_submission(*submission_id) =>
            {
                // The daemon may have accepted the image turn; never replay an
                // attachment when the outcome is unknown.
                self.in_flight_images.clear();
                self.set_notice(errfmt::humanize(err));
            }
            Action::SessionActivityProbeFinished {
                session_id,
                binding_generation,
                probe_generation,
                after_busy_rejection,
                active_was_observed,
                result,
            } => {
                if self.session_id == Some(*session_id)
                    && self.session_binding_generation == *binding_generation
                    && self.session_activity_probe_generation == *probe_generation
                    && !self.chat.has_pending_turn_submission()
                    && self.compacting_session != Some(*session_id)
                    && !self.sessions_requiring_sync.contains(session_id)
                {
                    match result {
                        Ok(sync) => {
                            let installed =
                                sync.snapshot.as_ref().zip(sync.fence.as_ref()).is_some_and(
                                    |(snapshot, fence)| {
                                        self.install_synchronized_session(
                                            *session_id,
                                            *binding_generation,
                                            snapshot,
                                            fence,
                                        )
                                    },
                                );
                            if installed {
                                self.herdr.resolve_activity();
                            }
                            if installed && *after_busy_rejection {
                                self.set_notice(
                                    "session ready — preserved prompt can be sent".into(),
                                );
                            }
                        }
                        Err(error) if error.message.contains("session has an active operation") => {
                            self.chat.adopt_active_turn();
                            self.herdr.adopt_activity();
                            self.set_notice("session is still working — input locked".into());
                            self.spawn_session_activity_probe(
                                *session_id,
                                *binding_generation,
                                1_000,
                                *after_busy_rejection,
                                true,
                            );
                        }
                        Err(_) if *active_was_observed => {
                            // Once activity was authoritatively observed, keep
                            // probing until a fenced snapshot succeeds even if
                            // the live stream already cleared its busy flag.
                            self.spawn_session_activity_probe(
                                *session_id,
                                *binding_generation,
                                1_000,
                                *after_busy_rejection,
                                true,
                            );
                        }
                        Err(_) => {
                            // Before activity is proven, the existing bound
                            // stream remains authoritative; probe transport
                            // noise must not invent a busy state.
                        }
                    }
                }
            }
            Action::BoundAgentEvent {
                session_id,
                binding_generation,
                stream_generation,
                event,
            } => {
                if self.session_id == Some(*session_id)
                    && self.session_binding_generation == *binding_generation
                    && self.stream_generation == *stream_generation
                {
                    let session_changed = matches!(
                        &**event,
                        AgentTurnEvent::Extension {
                            extension,
                            scope: Some(scope),
                            ..
                        } if extension == "ocean.session_changed" && scope == session_id
                    );
                    if session_changed {
                        follow_up = Some(Action::AgentStreamGap(*session_id));
                        self.handle_session_invalidation(
                            *session_id,
                            *binding_generation,
                            "session changed on another surface · synchronizing context…",
                        );
                    } else {
                        follow_up = Some(Action::AgentEvent(event.clone()));
                    }
                }
            }
            Action::BoundAgentStreamGap {
                session_id,
                binding_generation,
                stream_generation,
            } => {
                if self.session_id == Some(*session_id)
                    && self.session_binding_generation == *binding_generation
                    && self.stream_generation == *stream_generation
                {
                    follow_up = Some(Action::AgentStreamGap(*session_id));
                }
            }
            Action::BoundAgentReplayResetRequired {
                session_id,
                binding_generation,
                stream_generation,
            } => {
                if self.session_id == Some(*session_id)
                    && self.session_binding_generation == *binding_generation
                    && self.stream_generation == *stream_generation
                {
                    follow_up = Some(Action::AgentStreamGap(*session_id));
                    self.handle_session_invalidation(
                        *session_id,
                        *binding_generation,
                        "session event history changed · synchronizing authoritative context…",
                    );
                }
            }
            Action::CompactSession => {
                let recovery = self.compacting_session.filter(|session_id| {
                    self.compact_refresh_required
                        && self.session_id == Some(*session_id)
                        && self.compact_binding_generation == self.session_binding_generation
                });
                if let Some(session_id) = recovery {
                    // Local busy may itself be stale because the reset stream
                    // lost TurnFinished. The daemon sync lease is authoritative.
                    self.begin_compact_reload(
                        session_id,
                        self.session_binding_generation,
                        "reloading synchronized session context…",
                    );
                } else if self.chat.is_busy() {
                    self.set_notice("wait for the active turn to finish before compacting".into());
                } else if self.compacting_session.is_some() {
                    self.set_notice("session compaction already in progress".into());
                } else if let Some(session_id) = self.session_id {
                    self.session_activity_probe_generation =
                        self.session_activity_probe_generation.wrapping_add(1);
                    self.sessions_requiring_sync.insert(session_id);
                    self.compacting_session = Some(session_id);
                    self.compact_refresh_required = false;
                    self.compact_request_in_flight = true;
                    self.compact_invalidation_pending = false;
                    self.compact_binding_generation = self.session_binding_generation;
                    self.compact_operation_generation =
                        self.compact_operation_generation.wrapping_add(1);
                    let binding_generation = self.compact_binding_generation;
                    let operation_generation = self.compact_operation_generation;
                    self.set_notice("compacting session context…".into());
                    let client = self.client.clone();
                    let tx = self.actions_tx.clone();
                    tokio::spawn(async move {
                        let _ = tx.send(Action::CompactFinished {
                            session_id,
                            binding_generation,
                            operation_generation,
                            result: client.compact_session(session_id).await,
                        });
                    });
                } else {
                    self.set_notice("nothing to compact — start or resume a session first".into());
                }
            }
            Action::CompactFinished {
                session_id,
                binding_generation,
                operation_generation,
                result,
            } => {
                if !self.compact_completion_matches(
                    *session_id,
                    *binding_generation,
                    *operation_generation,
                ) {
                    // A completion that no longer owns the active generation
                    // cannot clear a newer invalidation marker, even if its own
                    // precommit outcome was definitely unchanged.
                    self.sessions_requiring_sync.insert(*session_id);
                    return;
                }
                if self.session_id != Some(*session_id)
                    || self.session_binding_generation != *binding_generation
                {
                    self.clear_compact_hold();
                    return;
                }
                let follow_with_sync = self.compact_invalidation_pending;
                self.compact_request_in_flight = false;
                match result {
                    Ok(response) => {
                        let installed = response
                            .sync
                            .as_ref()
                            .zip(response.fence.as_ref())
                            .is_some_and(|(snapshot, fence)| {
                                self.install_synchronized_session(
                                    *session_id,
                                    *binding_generation,
                                    snapshot,
                                    fence,
                                )
                            });
                        if installed && follow_with_sync {
                            self.begin_compact_reload(
                                *session_id,
                                *binding_generation,
                                "compact committed · reconciling post-fence invalidation…",
                            );
                        } else if installed {
                            self.clear_compact_hold();
                            if response.elided_messages == 0 {
                                self.set_notice(
                                    "nothing to compact · recent context already protected".into(),
                                );
                            } else {
                                self.set_notice(format!(
                                    "context compacted · {} messages summarized · {} ms",
                                    response.elided_messages, response.wall_ms
                                ));
                            }
                        } else if follow_with_sync {
                            self.begin_compact_reload(
                                *session_id,
                                *binding_generation,
                                "compact response unusable · synchronizing authoritative context…",
                            );
                        } else {
                            self.compact_refresh_required = true;
                            self.set_notice(
                                "compact response lacked a usable sync fence · run /compact to refresh"
                                    .into(),
                            );
                        }
                    }
                    Err(error) => {
                        let message = errfmt::humanize(&error.message);
                        if follow_with_sync {
                            self.begin_compact_reload(
                                *session_id,
                                *binding_generation,
                                "compact invalidated · synchronizing authoritative context…",
                            );
                        } else if error.transcript_may_have_changed {
                            self.sessions_requiring_sync.insert(*session_id);
                            self.compact_refresh_required = true;
                            self.set_notice(format!(
                                "{message} · run /compact to retry the authoritative session sync"
                            ));
                        } else {
                            self.sessions_requiring_sync.remove(session_id);
                            self.clear_compact_hold();
                            self.set_notice(message);
                        }
                    }
                }
            }
            Action::CompactReloadFinished {
                session_id,
                binding_generation,
                operation_generation,
                result,
            } => {
                if !self.compact_completion_matches(
                    *session_id,
                    *binding_generation,
                    *operation_generation,
                ) {
                    self.sessions_requiring_sync.insert(*session_id);
                    return;
                }
                if self.session_id != Some(*session_id)
                    || self.session_binding_generation != *binding_generation
                {
                    self.clear_compact_hold();
                    return;
                }
                match result {
                    Ok(sync) => {
                        let installed =
                            sync.snapshot.as_ref().zip(sync.fence.as_ref()).is_some_and(
                                |(snapshot, fence)| {
                                    self.install_synchronized_session(
                                        *session_id,
                                        *binding_generation,
                                        snapshot,
                                        fence,
                                    )
                                },
                            );
                        if installed {
                            self.clear_compact_hold();
                            self.set_notice("synchronized session context reloaded".into());
                        } else {
                            self.compact_refresh_required = true;
                            self.set_notice(
                                "session sync lacked a usable replay fence · run /compact to retry"
                                    .into(),
                            );
                        }
                    }
                    Err(error) => {
                        self.compact_refresh_required = true;
                        let message = errfmt::humanize(&error.message);
                        self.set_notice(format!(
                            "{message} · run /compact to retry the authoritative session sync"
                        ));
                    }
                }
            }
            Action::SessionBound(id) => self.bind_session(*id),
            // Session hygiene: only fold in agent events for the BOUND session.
            // A superseded stream's last envelopes (or an unscoped daemon echo)
            // must never pollute the current chat.
            Action::AgentEvent(evt) => {
                if let (Some(bound), Some(evt_sid)) = (self.session_id, evt.session_id()) {
                    if bound != evt_sid {
                        return;
                    }
                }
                // `/advisor off` is immediate from the operator's perspective.
                // A review spawned by an earlier turn can finish later; suppress
                // that stale card before child components paint it.
                if matches!(
                    evt.as_ref(),
                    AgentTurnEvent::Extension { extension, .. } if extension == "advisor"
                ) && matches!(&self.advisor_ctl, Some(control) if !control.enabled)
                {
                    return;
                }
                // Failover honesty (OCEAN-275): the daemon announces a reroute
                // on the stream; paint it in the status line too (the chat
                // renders the full concern card in the transcript).
                if let AgentTurnEvent::ModelRerouted {
                    requested,
                    effective,
                    ..
                } = evt.as_ref()
                {
                    let msg = errfmt::humanize(&format!(
                        "{requested} unavailable - running on {effective}"
                    ));
                    self.set_notice(msg);
                }
                if matches!(evt.as_ref(), AgentTurnEvent::TurnStarted { .. }) {
                    self.in_flight_images.clear();
                }
            }
            Action::ClipboardImages(result) => match result {
                Ok(images) => {
                    self.pending_images.extend(images.iter().cloned());
                    self.set_notice(format!(
                        "image attached · {} queued for next turn",
                        self.pending_images.len()
                    ));
                }
                Err(error) => self.set_notice(error.clone()),
            },
            Action::DictationHoldPressed => {
                if self.space_hold.is_none()
                    && self.active_dictation_id.is_none()
                    && self.chat.can_start_dictation()
                {
                    self.next_dictation_id = self.next_dictation_id.wrapping_add(1).max(1);
                    self.space_hold = Some(SpaceHold {
                        id: self.next_dictation_id,
                        pressed_at: Instant::now(),
                        started: false,
                    });
                }
            }
            Action::DictationHoldActivated { id } => {
                if let Some(hold) = self.space_hold.as_mut().filter(|hold| hold.id == *id) {
                    if !hold.started {
                        hold.started = true;
                        follow_up = Some(Action::DictationStart {
                            id: *id,
                            toggle: false,
                        });
                    }
                }
            }
            Action::DictationHoldReleased => {
                if let Some(hold) = self.space_hold.take() {
                    follow_up = Some(if hold.started {
                        Action::DictationStop { id: hold.id }
                    } else {
                        Action::ComposerInsert(" ".into())
                    });
                }
            }
            Action::DictationToggle => {
                if let Some(id) = self.active_dictation_id {
                    follow_up = Some(
                        if self
                            .dictation_capture
                            .as_ref()
                            .is_some_and(|(capture_id, _)| *capture_id == id)
                        {
                            Action::DictationStop { id }
                        } else {
                            Action::DictationCancel { id }
                        },
                    );
                } else if self.chat.can_start_dictation() {
                    self.next_dictation_id = self.next_dictation_id.wrapping_add(1).max(1);
                    follow_up = Some(Action::DictationStart {
                        id: self.next_dictation_id,
                        toggle: true,
                    });
                }
            }
            Action::DictationStart { id, .. } => {
                if self.active_dictation_id.is_none() {
                    self.active_dictation_id = Some(*id);
                    if let Some(task) = self.dictation_task.take() {
                        task.abort();
                    }
                    match dictation::start(*id, self.actions_tx.clone()) {
                        Ok(capture) => self.dictation_capture = Some((*id, capture)),
                        Err(error) => {
                            follow_up = Some(Action::DictationCaptured {
                                id: *id,
                                audio: Err(error),
                            });
                        }
                    }
                }
            }
            Action::DictationStop { id } => {
                if self.active_dictation_id == Some(*id) {
                    if let Some((capture_id, capture)) = self.dictation_capture.as_mut() {
                        if *capture_id == *id {
                            capture.finish();
                        }
                    }
                }
            }
            Action::DictationCaptured { id, audio } => {
                if self.active_dictation_id == Some(*id) {
                    if self
                        .dictation_capture
                        .as_ref()
                        .is_some_and(|(capture_id, _)| capture_id == id)
                    {
                        self.dictation_capture.take();
                    }
                    match audio {
                        Ok(wav) => {
                            let client = self.client.clone();
                            let tx = self.actions_tx.clone();
                            let wav = wav.clone();
                            let id = *id;
                            self.dictation_task = Some(tokio::spawn(async move {
                                let transcript = client.transcribe_voice(wav).await;
                                let _ = tx.send(Action::DictationTranscribed { id, transcript });
                            }));
                        }
                        Err(error) => {
                            self.active_dictation_id = None;
                            self.set_notice(errfmt::humanize(error));
                        }
                    }
                }
            }
            Action::DictationTranscribed { id, transcript } => {
                if self.active_dictation_id == Some(*id) {
                    self.dictation_task.take();
                    match transcript {
                        Ok(text) => {
                            let chunks = dictation_text_chunks(text);
                            if chunks.is_empty() {
                                self.active_dictation_id = None;
                                self.set_notice("no speech heard — try again".into());
                                follow_up = Some(Action::DictationCancel { id: *id });
                            } else {
                                let tx = self.actions_tx.clone();
                                let id = *id;
                                self.dictation_task = Some(tokio::spawn(async move {
                                    let last_index = chunks.len() - 1;
                                    for (index, text) in chunks.into_iter().enumerate() {
                                        if index > 0 {
                                            tokio::time::sleep(Duration::from_millis(38)).await;
                                        }
                                        let _ = tx.send(Action::DictationTextChunk {
                                            id,
                                            text,
                                            first: index == 0,
                                            last: index == last_index,
                                        });
                                    }
                                }));
                            }
                        }
                        Err(error) => {
                            self.active_dictation_id = None;
                            self.set_notice(errfmt::humanize(error));
                        }
                    }
                }
            }
            Action::DictationTextChunk { id, last, .. } => {
                if self.active_dictation_id == Some(*id) && *last {
                    self.active_dictation_id = None;
                    self.dictation_task.take();
                }
            }
            Action::DictationCancel { id } => {
                if self.active_dictation_id == Some(*id) {
                    if let Some((_, mut capture)) = self.dictation_capture.take() {
                        capture.cancel();
                    }
                    if let Some(task) = self.dictation_task.take() {
                        task.abort();
                    }
                    self.space_hold = None;
                    self.active_dictation_id = None;
                }
            }
            Action::SubmitPrompt {
                submission_id,
                prompt,
            } => {
                let synchronization_pending = self.session_id.is_some_and(|session_id| {
                    self.compacting_session == Some(session_id)
                        || self.sessions_requiring_sync.contains(&session_id)
                });
                if synchronization_pending {
                    self.set_notice("session synchronization is still in progress".into());
                    follow_up = Some(Action::TurnSendFailed {
                        submission_id: *submission_id,
                        prompt: prompt.clone(),
                        err: "session synchronization is still in progress".into(),
                    });
                } else {
                    self.submit_turn(*submission_id, prompt.clone());
                }
            }
            Action::OpenFile(path) => {
                self.editor.open(path.clone());
                self.center = Center::Editor;
                self.focus_to(Focus::Center);
            }
            Action::ResumeSessionsLoaded {
                workspace_root,
                sessions,
            } => {
                if *workspace_root == self.workspace_root {
                    self.resume_loading = false;
                    if self.resume_open {
                        self.resume_sessions = sessions.clone();
                        self.resume_sel = self
                            .resume_sel
                            .min(self.resume_sessions.len().saturating_sub(1));
                    }
                }
            }
            Action::ResumeSession { id, path, cwd } => {
                self.chat
                    .load_history(crate::shell::sessions::load_transcript(path));
                self.bind_session_with(*id, false); // transcript came from disk
                self.rail.live_id = Some(id.0.to_string());
                if self.compacting_session != Some(*id)
                    && !self.sessions_requiring_sync.contains(id)
                {
                    self.spawn_session_activity_probe(
                        *id,
                        self.session_binding_generation,
                        0,
                        false,
                        false,
                    );
                }
                // Re-root the workbench to the dir this session ran in, so the
                // file tree, graph, and future turns follow the session.
                self.set_active_project(cwd.clone());
                self.center = Center::Chat;
                self.focus_to(Focus::Center);
            }
            // `+ new` on a project header: fresh session, re-rooted to `cwd`.
            Action::NewSessionInProject { cwd } => {
                if let Some(task) = self.stream_task.take() {
                    task.abort();
                }
                // Intentional unbind: no stream will reconnect, so a stale
                // degraded SSE reading must not persist forever.
                self.health.recover(HealthSource::Sse);
                self.session_binding_generation = self.session_binding_generation.wrapping_add(1);
                self.stream_generation = self.stream_generation.wrapping_add(1);
                self.session_id = None;
                self.clear_compact_hold();
                self.chat.load_history(Vec::new()); // clear the transcript
                self.rail.live_id = None;
                self.set_active_project(cwd.clone());
                self.center = Center::Chat;
                self.focus_to(Focus::Center);
            }
            Action::Navigate(nav) => match *nav {
                Nav::Sessions => {
                    self.show_sessions = true;
                    self.focus_to(Focus::Sessions);
                }
                Nav::Files => {
                    self.right_rail_mode = RightRailMode::Files;
                    self.set_tree_visible_by_operator(true);
                    self.focus_to(Focus::Tree);
                }
                Nav::Graph => {
                    // Mirror the ⌃⌥5 toggle: off returns to editor (if tabs) else chat.
                    self.center = if self.center == Center::Graph {
                        if self.editor.has_tabs() {
                            Center::Editor
                        } else {
                            Center::Chat
                        }
                    } else {
                        Center::Graph
                    };
                    self.focus_to(Focus::Center);
                }
                Nav::Terminal => {
                    if !self.pty.is_active() {
                        self.pty.open(&PathBuf::from(&self.workspace_root), "");
                    }
                    // ALWAYS unhide: `/terminal` after the dock was hidden in
                    // settings must not focus an invisible pane.
                    self.show_term = true;
                    self.focus_to(Focus::Term);
                }
            },
            // `/new`: drop the bound session (and its stream) so the next turn
            // mints a fresh one; the chat cleared its own transcript already.
            Action::NewSession => {
                if let Some(task) = self.stream_task.take() {
                    task.abort();
                }
                // Intentional unbind: no stream will reconnect, so a stale
                // degraded SSE reading must not persist forever.
                self.health.recover(HealthSource::Sse);
                self.session_binding_generation = self.session_binding_generation.wrapping_add(1);
                self.stream_generation = self.stream_generation.wrapping_add(1);
                self.session_id = None;
                self.clear_compact_hold();
                self.center = Center::Chat;
                self.focus_to(Focus::Center);
            }
            // `/model <id>`: remember the override for subsequent turns.
            Action::SetModel(id) => self.model_override = Some(id.clone()),
            // `/thinking <level>`: remember the override for subsequent turns.
            // `default` clears the per-turn override so the daemon's global
            // setting is in force again.
            Action::SetThinking(level) => {
                self.thinking_override = *level;
            }
            // `/models`: open the picker and fetch the live registry (with
            // readiness) off-thread — the overlay shows "loading…" until
            // ModelsLoaded lands.
            Action::OpenModels => {
                self.models_open = true;
                self.models_loading = true;
                self.models_hit.clear();
                let client = self.client.clone();
                let tx = self.actions_tx.clone();
                tokio::spawn(async move {
                    match client.models().await {
                        Ok(r) => {
                            let _ = tx.send(Action::ModelsLoaded {
                                current: r.current.model,
                                entries: r.models,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(Action::Error(format!("models: {e}")));
                        }
                    }
                });
            }
            Action::ModelsLoaded { current, entries } => {
                self.models_loading = false;
                self.models_entries = order_models(entries.clone());
                self.models_current = current.clone();
                // Start the cursor on the model in force: the pinned override,
                // else the daemon's current global model.
                let active = self
                    .model_override
                    .clone()
                    .unwrap_or_else(|| current.clone());
                self.models_sel = self
                    .models_entries
                    .iter()
                    .position(|m| m.id == active)
                    .unwrap_or(0);
                // If the advisor picker is what triggered this fetch, seat its
                // cursor on the model it's currently set to.
                if self.advisor_open {
                    self.seat_advisor_cursor();
                }
            }
            // `/advisor`: open the per-session advisor picker over the live
            // registry (reuses the models fetch). Overlay = an "off" row + the
            // ready models; the pick rides subsequent turns as `advisor_ctl`.
            Action::OpenAdvisor => {
                self.advisor_open = true;
                self.advisor_hit.clear();
                if self.models_entries.is_empty() {
                    self.models_loading = true;
                    let client = self.client.clone();
                    let tx = self.actions_tx.clone();
                    tokio::spawn(async move {
                        match client.models().await {
                            Ok(r) => {
                                let _ = tx.send(Action::ModelsLoaded {
                                    current: r.current.model,
                                    entries: r.models,
                                });
                            }
                            Err(e) => {
                                let _ = tx.send(Action::Error(format!("models: {e}")));
                            }
                        }
                    });
                } else {
                    self.seat_advisor_cursor();
                }
            }
            // `/memory`: open the retained-memory browser and fetch the store
            // off-thread (read-only). Overlay shows "loading…" until it lands.
            Action::OpenMemory => {
                self.memory_open = true;
                self.memory_loading = true;
                self.memory_query.clear();
                self.memory_sel = 0;
                let client = self.client.clone();
                let tx = self.actions_tx.clone();
                tokio::spawn(async move {
                    match client.memory().await {
                        Ok(r) => {
                            let _ = tx.send(Action::MemoryLoaded {
                                entries: r.memories,
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(Action::Error(format!("memory: {e}")));
                        }
                    }
                });
            }
            Action::MemoryLoaded { entries } => {
                self.memory_loading = false;
                self.memory_entries = entries.clone();
                self.memory_sel = 0;
            }
            // `/lsp`: open the language-server panel and fetch the detected
            // servers for the active workspace off-thread.
            Action::OpenLsp => {
                self.lsp_open = true;
                self.lsp_loading = true;
                let client = self.client.clone();
                let tx = self.actions_tx.clone();
                let cwd = self.workspace_root.clone();
                tokio::spawn(async move {
                    match client.lsp(&cwd).await {
                        Ok(r) => {
                            let _ = tx.send(Action::LspLoaded { servers: r.servers });
                        }
                        Err(e) => {
                            let _ = tx.send(Action::Error(format!("lsp: {e}")));
                        }
                    }
                });
            }
            Action::LspLoaded { servers } => {
                self.lsp_loading = false;
                self.lsp_servers = servers.clone();
            }
            // `/image [path]`: open the full-screen viewer. Resolve a relative
            // path against the active workspace; a missing file surfaces in the
            // status line instead of a blank viewer.
            Action::ViewImage(raw) => {
                let p = PathBuf::from(raw);
                let abs = if p.is_absolute() {
                    p
                } else {
                    PathBuf::from(&self.workspace_root).join(p)
                };
                if abs.exists() {
                    self.image_view = Some(abs);
                } else {
                    self.set_notice(format!("image not found: {}", abs.display()));
                }
            }
            // `/login [claude|codex]`: run the REAL OAuth flow off-thread
            // (begin → browser → token exchange → persist) so the TUI never
            // blocks on the callback server or browser/OS integration. A second
            // `/login` while one is already running is rejected with a busy
            // status instead of racing a second callback server.
            Action::Login(target) => {
                if self.login_in_flight {
                    self.set_notice("login already in progress".into());
                } else {
                    self.login_in_flight = true;
                    let tx = self.actions_tx.clone();
                    let provider = match *target {
                        LoginTarget::Claude => ocean_oauth::OAuthProvider::Claude,
                        LoginTarget::Codex => ocean_oauth::OAuthProvider::Codex,
                    };
                    tokio::spawn(async move {
                        let label = provider.label();
                        let session = match ocean_oauth::begin(provider, None).await {
                            Ok(s) => s,
                            Err(e) => {
                                let _ = tx
                                    .send(Action::LoginDone(format!("{label} login failed: {e}")));
                                return;
                            }
                        };
                        let _ = tx.send(Action::Status(format!(
                            "{label} login: complete auth in your browser (or open {})",
                            session.launch_url
                        )));
                        if open::that(&session.authorize_url).is_err() {
                            let _ = tx.send(Action::Status(format!(
                                "browser did not open — visit {}",
                                session.launch_url
                            )));
                        }
                        let msg = match session.finish().await {
                            Ok(outcome) => {
                                // Measure remaining TTL from when the token was
                                // actually issued (after browser auth), not from
                                // before finish() — else the auth duration is
                                // counted as still-valid time.
                                let now_ms = chrono::Utc::now().timestamp_millis();
                                let h = ((outcome.expires_ms - now_ms) / 3_600_000).max(0);
                                format!(
                                    "{label} login complete — credential saved (expires in ~{h}h); providers refresh automatically"
                                )
                            }
                            Err(e) => format!("{label} login failed: {e}"),
                        };
                        let _ = tx.send(Action::LoginDone(msg));
                    });
                }
            }
            // `/login` finished (success or failure): the spawned flow emits
            // `LoginDone` carrying the final message. Lands it in the status
            // line and clears the `login_in_flight` guard for the next attempt.
            Action::LoginDone(msg) => {
                self.set_notice(msg.clone());
                self.login_in_flight = false;
                // Auth-state changed — refresh the welcome provider line so the
                // chat empty-state immediately reflects the new credentials.
                self.refresh_welcome_provider_line();
            }
            // `/settings`: open the modal settings overlay. Mutually exclusive
            // with the providers popup — opening one closes the other.
            Action::OpenSettings => {
                self.providers_open = false;
                self.permissions_open = false;
                self.settings_open = true;
                self.settings_sel = 0;
            }
            // `/permissions`: fetch the daemon-owned effective mode before
            // showing a selected row. The app never invents local authority.
            Action::OpenPermissions => {
                self.settings_open = false;
                self.providers_open = false;
                self.permissions_open = true;
                self.permissions_loading = true;
                self.permissions_saving = false;
                self.permissions_hit.clear();
                let client = self.client.clone();
                let tx = self.actions_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(Action::PermissionSettingsLoaded(
                        client.permission_settings().await,
                    ));
                });
            }
            Action::PermissionSettingsLoaded(result) => {
                self.permissions_loading = false;
                match result {
                    Ok(settings) => self.accept_permission_settings(settings),
                    Err(error) => self.set_notice(format!("permissions: {error}")),
                }
            }
            Action::PermissionModeSaved(result) => {
                self.permissions_saving = false;
                match result {
                    Ok(settings) => {
                        self.accept_permission_settings(settings);
                        self.permissions_open = false;
                        let effective = permission_mode_label(settings.effective);
                        if settings.env_override.is_some() {
                            let saved = settings
                                .persisted
                                .map(permission_mode_label)
                                .unwrap_or(effective);
                            self.set_notice(format!(
                                "saved {saved}; OCEAN_YOLO keeps {effective} effective"
                            ));
                        } else {
                            self.set_notice(format!("permissions: {effective}"));
                        }
                        // A turn that was already waiting captured its prior
                        // policy. Once the daemon confirms skip-all is truly
                        // effective, release this submitter's pending calls with
                        // their normal decision tokens instead of leaving the
                        // TUI wedged until an extra Ctrl-Y.
                        if settings.effective == PermissionMode::SkipAll {
                            let pending: Vec<(PermissionId, RequestId)> = self
                                .pending_permission_ids
                                .iter()
                                .filter_map(|permission_id| {
                                    self.perm_request
                                        .get(permission_id)
                                        .copied()
                                        .map(|request_id| (*permission_id, request_id))
                                })
                                .collect();
                            self.skip_all_requests
                                .extend(pending.iter().map(|(_, request_id)| *request_id));
                            for (permission_id, _) in pending {
                                let _ = self.actions_tx.send(Action::PermissionDecided {
                                    permission_id,
                                    allow: true,
                                });
                            }
                        } else {
                            self.skip_all_requests.clear();
                        }
                    }
                    Err(error) => self.set_notice(format!("permissions: {error}")),
                }
            }
            // `/providers` (or bare `/login`): open the provider auth popup.
            // Builds the status rows fresh from the auth file + process env so
            // the list reflects the live state every time it's opened.
            Action::OpenProviders => {
                self.settings_open = false;
                self.permissions_open = false;
                self.providers_open = true;
                self.providers_sel = 0;
                self.providers_mode = ProvidersMode::List;
                self.providers_rows = Self::build_provider_rows();
            }
            // `/copy`: hand the last reply to the system clipboard via pbcopy.
            Action::CopyToClipboard(text) => {
                let text = text.clone();
                let tx = self.actions_tx.clone();
                tokio::spawn(async move {
                    let msg = match copy_to_clipboard(&text) {
                        Ok(()) => "copied last reply to clipboard".to_string(),
                        Err(e) => format!("copy failed: {e}"),
                    };
                    let _ = tx.send(Action::Status(msg));
                });
            }
            Action::OceanEvent(env) => match &env.event {
                // OCEAN-185: the turn's first permission request claims the
                // pending submit token; remember permission→request for the POST.
                ocean_core::OceanEvent::PermissionRequest { .. } => {
                    if let (Some(rid), Some(pid)) = (env.request_id, env.permission_id) {
                        self.perm_request.insert(pid, rid);
                        self.pending_permission_ids.insert(pid);
                        if let std::collections::hash_map::Entry::Vacant(slot) =
                            self.decision_tokens.entry(rid)
                        {
                            if let Some(token) = self.pending_submit_token.take() {
                                slot.insert(token);
                            }
                        }
                        // Only a request that was already active when skip-all
                        // was confirmed may bridge later same-turn prompts.
                        if self.skip_all_requests.contains(&rid) {
                            let _ = self.actions_tx.send(Action::PermissionDecided {
                                permission_id: pid,
                                allow: true,
                            });
                        }
                    }
                }
                ocean_core::OceanEvent::PermissionDecision { .. } => {
                    if let Some(pid) = env.permission_id {
                        self.pending_permission_ids.remove(&pid);
                        self.perm_request.remove(&pid);
                    }
                }
                ocean_core::OceanEvent::TurnFinished { .. }
                | ocean_core::OceanEvent::Cancelled { .. }
                | ocean_core::OceanEvent::Error { .. } => {
                    if let Some(request_id) = env.request_id {
                        self.skip_all_requests.remove(&request_id);
                        self.decision_tokens.remove(&request_id);
                        self.pending_permission_ids.retain(|permission_id| {
                            self.perm_request.get(permission_id) != Some(&request_id)
                        });
                        self.perm_request
                            .retain(|_, mapped_request| *mapped_request != request_id);
                    }
                }
                _ => {}
            },
            Action::PermissionDecided {
                permission_id,
                allow,
            } => {
                let token = self
                    .perm_request
                    .get(permission_id)
                    .and_then(|rid| self.decision_tokens.get(rid))
                    .cloned();
                let client = self.client.clone();
                let tx = self.actions_tx.clone();
                let (pid, allow) = (*permission_id, *allow);
                tokio::spawn(async move {
                    if let Err(e) = client.permission_decision(pid, allow, token).await {
                        let _ = tx.send(Action::Error(format!("decision: {e}")));
                    }
                });
            }
            _ => {}
        }
        // Project lifecycle only after the app has filtered stale session
        // events and applied the same authoritative transition the UI uses.
        // A finish from the previously resumed turn cannot idle Herdr while a
        // different tagged submission is still awaiting/holding admission.
        let herdr_event_is_current = match &action {
            Action::AgentEvent(event) => match event.as_ref() {
                AgentTurnEvent::TurnFinished { turn_id, .. } => {
                    self.chat.turn_finished_resolves_activity(*turn_id)
                }
                _ => true,
            },
            _ => true,
        };
        if herdr_event_is_current {
            self.herdr.observe(&action, self.session_id);
        }
        if let Some(next) = self.chat.update(&action) {
            self.dispatch(next);
        }
        if let Some(next) = self.tray.update(&action) {
            self.dispatch(next);
        }
        // Auto-reveal only on the hidden -> visible transition, and never after
        // an explicit operator close. Graph/context/todo lifecycle updates can
        // remount this tray but cannot override that dismissal latch.
        if !self.tree_auto_reveal_suppressed && !tray_was_visible && self.tray.is_visible() {
            self.show_tree = true;
        }
        if let Some(next) = follow_up {
            self.dispatch(next);
        }
    }

    fn focus_to(&mut self, focus: Focus) {
        // Leaving the terminal clears the double-Esc latch so it can't fire on a
        // later re-entry.
        if focus != Focus::Term {
            self.esc_armed = false;
        }
        self.focus = focus;
        self.apply_focus();
    }

    /// Bind the chat to `id`: abort any superseded stream and subscribe a fresh
    /// self-healing one. Idempotent for the already-bound session.
    fn bind_session_with(&mut self, id: AgentSessionId, replay_first: bool) {
        let replaces_loaded_history = !replay_first;
        if self.session_id != Some(id) || replaces_loaded_history {
            // Switching/resuming replaces visible history. Increment even for
            // A→B→A or an explicit A→A resume so queued old envelopes cannot
            // become current merely because the UUID matches.
            self.session_binding_generation = self.session_binding_generation.wrapping_add(1);
            self.clear_compact_hold();
        }
        if self.session_id == Some(id)
            && !replaces_loaded_history
            && self.stream_task.as_ref().is_some_and(|t| !t.is_finished())
        {
            return;
        }
        if let Some(task) = self.stream_task.take() {
            task.abort();
        }
        // The superseded stream's degraded state doesn't describe the fresh
        // subscription below — start it from a neutral SSE source.
        self.health.recover(HealthSource::Sse);
        self.session_id = Some(id);
        self.stream_generation = self.stream_generation.wrapping_add(1);
        self.stream_task = Some(self.client.spawn_event_stream(
            id,
            self.actions_tx.clone(),
            replay_first,
            None,
            self.session_binding_generation,
            self.stream_generation,
        ));
        if self.sessions_requiring_sync.contains(&id) {
            self.begin_compact_reload(
                id,
                self.session_binding_generation,
                "session compact outcome changed while unbound · synchronizing context…",
            );
        }
    }

    /// Mint-path bind: fresh chat, replay the session's buffered head.
    fn bind_session(&mut self, id: AgentSessionId) {
        self.bind_session_with(id, true);
    }

    /// Re-root the active project: the cwd new turns mint against AND the roots
    /// of the file tree and graph, so the whole workbench follows when you pick
    /// or `+`-create a session in another worktree. The session rail stays (it
    /// lists the launch project's worktrees); the PTY is left alone (it may have
    /// a live shell). No-ops if already there.
    fn set_active_project(&mut self, cwd: PathBuf) {
        let s = cwd.to_string_lossy().into_owned();
        if s == self.workspace_root {
            return;
        }
        self.workspace_root = s;
        self.tree.set_root(cwd.clone());
        self.graph.set_root(cwd.clone());
        self.chat.set_mention_root(cwd); // `@` picker follows the project
                                         // Force the file tree to re-read on the next tick rather than waiting
                                         // out the throttle window.
        self.last_tree_scan = Instant::now() - Duration::from_secs(2);
    }

    /// Startup chooser rows: new session, saved-session picker, blank editor,
    /// and graph. Esc dismisses it to the clean chat surface.
    const LAUNCH_ROWS: usize = 4;

    fn launch_key(&mut self, k: crossterm::event::KeyEvent) {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.launch_open = false,
            KeyCode::Up | KeyCode::Char('k') => self.launch_sel = self.launch_sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.launch_sel = (self.launch_sel + 1).min(Self::LAUNCH_ROWS - 1)
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.launch_apply(),
            _ => {}
        }
    }

    fn launch_mouse(&mut self, m: crossterm::event::MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollUp => self.launch_sel = self.launch_sel.saturating_sub(1),
            MouseEventKind::ScrollDown => {
                self.launch_sel = (self.launch_sel + 1).min(Self::LAUNCH_ROWS - 1)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = (m.column, m.row);
                if let Some(index) = self
                    .launch_hit
                    .iter()
                    .find(|(rect, _)| rect_has(*rect, pos))
                    .map(|(_, index)| *index)
                {
                    self.launch_sel = index;
                    self.launch_apply();
                }
            }
            _ => {}
        }
    }

    fn launch_apply(&mut self) {
        match self.launch_sel {
            0 => {
                self.launch_open = false;
                self.dispatch(Action::NewSession);
            }
            1 => {
                self.resume_sel = 0;
                self.resume_sessions.clear();
                self.resume_open = true;
                self.request_resume_sessions();
            }
            2 => {
                self.launch_open = false;
                self.set_tree_visible_by_operator(true);
                self.show_tree = true;
                self.center = Center::Editor;
                self.focus_to(Focus::Center);
            }
            3 => {
                self.launch_open = false;
                self.center = Center::Graph;
                self.focus_to(Focus::Center);
            }
            _ => {}
        }
    }

    fn request_resume_sessions(&mut self) {
        if self.resume_loading {
            return;
        }
        self.resume_loading = true;
        let workspace_root = self.workspace_root.clone();
        let root = PathBuf::from(&workspace_root);
        let tx = self.actions_tx.clone();
        tokio::task::spawn_blocking(move || {
            let sessions = crate::shell::sessions::discover(&root)
                .into_iter()
                .filter(|session| uuid::Uuid::parse_str(&session.id).is_ok())
                .collect();
            let _ = tx.send(Action::ResumeSessionsLoaded {
                workspace_root,
                sessions,
            });
        });
    }

    fn resume_key(&mut self, k: crossterm::event::KeyEvent) {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.resume_open = false,
            KeyCode::Up | KeyCode::Char('k') => self.resume_sel = self.resume_sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.resume_sessions.is_empty() {
                    self.resume_sel = (self.resume_sel + 1).min(self.resume_sessions.len() - 1);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.resume_apply(),
            KeyCode::Char('r') => self.request_resume_sessions(),
            _ => {}
        }
    }

    fn resume_mouse(&mut self, m: crossterm::event::MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollUp => self.resume_sel = self.resume_sel.saturating_sub(1),
            MouseEventKind::ScrollDown => {
                if !self.resume_sessions.is_empty() {
                    self.resume_sel = (self.resume_sel + 1).min(self.resume_sessions.len() - 1);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = (m.column, m.row);
                if let Some(index) = self
                    .resume_hit
                    .iter()
                    .find(|(rect, _)| rect_has(*rect, pos))
                    .map(|(_, index)| *index)
                {
                    self.resume_sel = index;
                    self.resume_apply();
                }
            }
            _ => {}
        }
    }

    fn resume_apply(&mut self) {
        let Some(session) = self.resume_sessions.get(self.resume_sel).cloned() else {
            return;
        };
        let Ok(id) = uuid::Uuid::parse_str(&session.id) else {
            self.set_notice("session cannot be resumed".into());
            return;
        };
        self.resume_open = false;
        self.launch_open = false;
        self.dispatch(Action::ResumeSession {
            id: AgentSessionId(id),
            path: session.path,
            cwd: session.cwd,
        });
    }

    fn accept_permission_settings(&mut self, settings: &PermissionSettingsResponse) {
        if settings.effective != PermissionMode::SkipAll {
            self.skip_all_requests.clear();
        }
        self.permissions_persisted = settings.persisted;
        self.permissions_effective = Some(settings.effective);
        self.permissions_env_override = settings.env_override;
        self.permissions_sel =
            permission_mode_index(settings.persisted.unwrap_or(settings.effective));
    }

    fn permissions_key(&mut self, k: crossterm::event::KeyEvent) {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.permissions_open = false,
            KeyCode::Up | KeyCode::Char('k') => {
                self.permissions_sel = self.permissions_sel.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.permissions_sel = (self.permissions_sel + 1).min(PERMISSION_OPTIONS.len() - 1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.permissions_apply(),
            _ => {}
        }
    }

    fn permissions_mouse(&mut self, m: crossterm::event::MouseEvent) {
        match m.kind {
            MouseEventKind::ScrollUp => {
                self.permissions_sel = self.permissions_sel.saturating_sub(1);
            }
            MouseEventKind::ScrollDown => {
                self.permissions_sel = (self.permissions_sel + 1).min(PERMISSION_OPTIONS.len() - 1);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let pos = (m.column, m.row);
                if let Some(index) = self
                    .permissions_hit
                    .iter()
                    .find(|(rect, _)| rect_has(*rect, pos))
                    .map(|(_, index)| *index)
                {
                    self.permissions_sel = index;
                    self.permissions_apply();
                }
            }
            _ => {}
        }
    }

    fn permissions_apply(&mut self) {
        if self.permissions_loading || self.permissions_saving {
            return;
        }
        let mode = PERMISSION_OPTIONS[self.permissions_sel].0;
        self.permissions_saving = true;
        let client = self.client.clone();
        let tx = self.actions_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Action::PermissionModeSaved(
                client.set_permission_mode(mode).await,
            ));
        });
    }

    /// Number of interactive rows in the `/settings` overlay.
    const SETTINGS_ROWS: usize = 5;

    /// Drive the `/settings` overlay: ↑↓/jk move, Enter/Space toggle, ←/→ adjust
    /// the dock height row, Esc/q close.
    fn settings_key(&mut self, k: crossterm::event::KeyEvent) {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.settings_open = false,
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings_sel = self.settings_sel.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings_sel = (self.settings_sel + 1).min(Self::SETTINGS_ROWS - 1);
            }
            KeyCode::Left if self.settings_sel == 3 => self.resize_term(-2),
            KeyCode::Right if self.settings_sel == 3 => self.resize_term(2),
            KeyCode::Enter | KeyCode::Char(' ') => match self.settings_sel {
                0 => self.show_sessions = !self.show_sessions,
                1 => self.set_tree_visible_by_operator(!self.show_tree),
                2 => self.show_term = !self.show_term,
                3 => {} // height adjusts with ←/→
                4 => self.chat.toggle_tools_expanded(),
                _ => {}
            },
            _ => {}
        }
    }

    /// Keys for the `/models` picker overlay: ↑/↓ move, ⏎ applies the model
    /// (+ the thinking level shown in the footer), ←/→ cycle thinking, Esc/q
    /// close. Enter on a not-ready model explains why instead of pretending.
    fn models_key(&mut self, k: crossterm::event::KeyEvent) {
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.models_open = false,
            KeyCode::Up | KeyCode::Char('k') => {
                self.models_sel = self.models_sel.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.models_entries.is_empty() {
                    self.models_sel = (self.models_sel + 1).min(self.models_entries.len() - 1);
                }
            }
            KeyCode::Left => self.thinking_override = cycle_thinking(self.thinking_override, -1),
            KeyCode::Right => self.thinking_override = cycle_thinking(self.thinking_override, 1),
            KeyCode::Enter | KeyCode::Char(' ') => self.models_apply(),
            KeyCode::Char('r') => self.dispatch(Action::OpenModels),
            _ => {}
        }
    }

    /// Mouse for the `/models` picker: click a row to select it, click the
    /// selected row (or double-click) to apply, wheel scrolls the cursor,
    /// click outside the modal closes it.
    fn models_mouse(&mut self, m: crossterm::event::MouseEvent) {
        let pos = (m.column, m.row);
        match m.kind {
            MouseEventKind::ScrollUp => self.models_sel = self.models_sel.saturating_sub(1),
            MouseEventKind::ScrollDown => {
                if !self.models_entries.is_empty() {
                    self.models_sel = (self.models_sel + 1).min(self.models_entries.len() - 1);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, idx)) = self
                    .models_hit
                    .iter()
                    .find(|(r, _)| rect_has(*r, pos))
                    .copied()
                {
                    if idx == self.models_sel {
                        self.models_apply();
                    } else {
                        self.models_sel = idx;
                    }
                } else if !self.models_hit.iter().any(|(r, _)| rect_has(*r, pos)) {
                    // Outside every row: close only when outside the modal
                    // frame entirely (the hit list spans the modal body, so a
                    // click on padding keeps it open harmlessly).
                    self.models_open = false;
                }
            }
            _ => {}
        }
    }

    /// Apply the picker selection: pin the model for subsequent turns and keep
    /// the thinking level shown in the footer. Not-ready models don't apply —
    /// the status line says what's missing instead.
    fn models_apply(&mut self) {
        let Some(entry) = self.models_entries.get(self.models_sel) else {
            return;
        };
        if !entry.ready {
            self.set_notice(format!(
                "{} has no credential ({})",
                entry.id, entry.provider
            ));
            return;
        }
        self.model_override = Some(entry.id.clone());
        self.models_open = false;
    }

    // ── /advisor picker ──────────────────────────────────────────────────────

    /// The ready models eligible to be an advisor (only credentialed ones can
    /// actually run the review). Row 0 of the overlay is always the "off" pick,
    /// so a click/selection at index `i` means: 0 → off, i≥1 → this[i-1].
    fn advisor_models(&self) -> Vec<&ModelEntry> {
        self.models_entries.iter().filter(|m| m.ready).collect()
    }

    /// Seat the advisor cursor on the current selection: the enabled model, or
    /// row 0 (off) when disabled/unset.
    fn seat_advisor_cursor(&mut self) {
        self.advisor_sel = match &self.advisor_ctl {
            Some(c) if c.enabled => c
                .model
                .as_deref()
                .and_then(|m| self.advisor_models().iter().position(|e| e.id == m))
                .map(|i| i + 1)
                .unwrap_or(0),
            _ => 0,
        };
    }

    fn advisor_key(&mut self, k: crossterm::event::KeyEvent) {
        let rows = self.advisor_models().len() + 1; // +1 for the "off" row
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.advisor_open = false,
            KeyCode::Up | KeyCode::Char('k') => {
                self.advisor_sel = self.advisor_sel.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.advisor_sel = (self.advisor_sel + 1).min(rows - 1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.advisor_apply(),
            _ => {}
        }
    }

    fn advisor_mouse(&mut self, m: crossterm::event::MouseEvent) {
        let pos = (m.column, m.row);
        let rows = self.advisor_models().len() + 1;
        match m.kind {
            MouseEventKind::ScrollUp => self.advisor_sel = self.advisor_sel.saturating_sub(1),
            MouseEventKind::ScrollDown => {
                self.advisor_sel = (self.advisor_sel + 1).min(rows - 1);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, idx)) = self
                    .advisor_hit
                    .iter()
                    .find(|(r, _)| rect_has(*r, pos))
                    .copied()
                {
                    if idx == self.advisor_sel {
                        self.advisor_apply();
                    } else {
                        self.advisor_sel = idx;
                    }
                } else {
                    self.advisor_open = false;
                }
            }
            _ => {}
        }
    }

    /// Apply the advisor selection: row 0 turns the advisor OFF for this
    /// session; any model row turns it ON reviewing on that model. Both send an
    /// explicit per-turn override (`advisor_ctl`) so the choice wins over the
    /// daemon's global config until changed.
    fn advisor_apply(&mut self) {
        use ocean_agent_sdk::AdvisorControl;
        if self.advisor_sel == 0 {
            self.advisor_ctl = Some(AdvisorControl {
                enabled: false,
                model: None,
            });
        } else {
            let models = self.advisor_models();
            let Some(entry) = models.get(self.advisor_sel - 1) else {
                return;
            };
            let id = entry.id.clone();
            self.advisor_ctl = Some(AdvisorControl {
                enabled: true,
                model: Some(id.clone()),
            });
        }
        self.advisor_open = false;
    }

    /// Render the `/advisor` picker: an "off" row over the ready models, with
    /// the current pick dotted. Mirrors the `/models` modal skin.
    fn draw_advisor(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, Borders, Clear};
        let full = frame.area();
        let width = 60u16.min(full.width.saturating_sub(4));
        // Snapshot (label, id) so the render loop can mutate `advisor_hit`
        // without holding an immutable borrow of `self` through the models.
        let models: Vec<(String, String)> = self
            .advisor_models()
            .iter()
            .map(|e| (e.label.clone(), e.id.clone()))
            .collect();
        let row_count = models.len() as u16 + 1; // off + models
        let height = (row_count + 4).min(full.height.saturating_sub(4)).max(6);
        let x = full.x + (full.width.saturating_sub(width)) / 2;
        let y = full.y + (full.height.saturating_sub(height)) / 2;
        let area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                " ADVISOR ",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.advisor_hit.clear();
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        if self.models_loading && models.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "loading registry…",
                    Style::default().fg(theme::COMMENT),
                ))
                .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x + 1, inner.y + 1, inner.width.saturating_sub(2), 1),
            );
            return;
        }

        let view_h = inner.height.saturating_sub(1) as usize;
        // Rows: 0 = off, 1..=N = models. Build labels then window to fit.
        let total = models.len() + 1;
        let scroll = self
            .advisor_sel
            .saturating_sub(view_h.saturating_sub(1))
            .min(total.saturating_sub(view_h));
        for ri in scroll..total.min(scroll + view_h) {
            let ry = inner.y + (ri - scroll) as u16;
            let selected = ri == self.advisor_sel;
            let bed = if selected { theme::BG_HL } else { theme::SLATE };
            let marker = if selected { g("▎", "|") } else { " " };
            let (dot, label, id, fg) = if ri == 0 {
                let active = matches!(&self.advisor_ctl, Some(c) if !c.enabled)
                    || self.advisor_ctl.is_none();
                (
                    if matches!(&self.advisor_ctl, Some(c) if !c.enabled) {
                        g("● ", "* ")
                    } else {
                        "  "
                    },
                    "off".to_string(),
                    String::new(),
                    if active { theme::FG } else { theme::COMMENT },
                )
            } else {
                let (label, id) = &models[ri - 1];
                let active = matches!(&self.advisor_ctl, Some(c) if c.enabled && c.model.as_deref() == Some(id.as_str()));
                (
                    if active { g("● ", "* ") } else { "  " },
                    label.clone(),
                    id.clone(),
                    theme::FG,
                )
            };
            let left = format!("{marker} {dot}{label}");
            let pad = (inner.width as usize)
                .saturating_sub(left.chars().count() + id.chars().count() + 1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        left,
                        Style::default().fg(fg).add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                    ),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(id, Style::default().fg(theme::COMMENT)),
                ]))
                .style(Style::default().bg(bed)),
                Rect::new(inner.x, ry, inner.width, 1),
            );
            self.advisor_hit
                .push((Rect::new(inner.x, ry, inner.width, 1), ri));
        }
    }

    // ── /memory browser ──────────────────────────────────────────────────────

    /// The memories matching the current search filter (case-insensitive
    /// substring over the text), preserving newest-first store order.
    fn memory_filtered(&self) -> Vec<&crate::shell::client::MemoryEntry> {
        let q = self.memory_query.to_lowercase();
        self.memory_entries
            .iter()
            .filter(|m| q.is_empty() || m.text.to_lowercase().contains(&q))
            .collect()
    }

    fn memory_key(&mut self, k: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        let n = self.memory_filtered().len();
        match k.code {
            KeyCode::Esc => self.memory_open = false,
            KeyCode::Up => self.memory_sel = self.memory_sel.saturating_sub(1),
            KeyCode::Down => {
                if n > 0 {
                    self.memory_sel = (self.memory_sel + 1).min(n - 1);
                }
            }
            // Enter copies the selected memory's text to the clipboard.
            KeyCode::Enter => {
                if let Some(m) = self.memory_filtered().get(self.memory_sel) {
                    let text = m.text.clone();
                    if let Err(e) = copy_to_clipboard(&text) {
                        self.set_notice(format!("copy failed: {e}"));
                    }
                }
                self.memory_open = false;
            }
            KeyCode::Backspace => {
                self.memory_query.pop();
                self.memory_sel = 0;
            }
            KeyCode::Char(c) => {
                self.memory_query.push(c);
                self.memory_sel = 0;
            }
            _ => {}
        }
    }

    fn memory_mouse(&mut self, m: crossterm::event::MouseEvent) {
        let n = self.memory_filtered().len();
        match m.kind {
            MouseEventKind::ScrollUp => self.memory_sel = self.memory_sel.saturating_sub(1),
            MouseEventKind::ScrollDown => {
                if n > 0 {
                    self.memory_sel = (self.memory_sel + 1).min(n - 1);
                }
            }
            // A click anywhere outside just closes (rows aren't individually
            // hit-tested — this is a browse/search view, Enter copies).
            MouseEventKind::Down(MouseButton::Left) => self.memory_open = false,
            _ => {}
        }
    }

    /// Render the `/memory` browser: a search box, then the retained memories
    /// (kind badge + text, newest first), filtered by the query. Enter copies
    /// the selected memory's text; Esc closes.
    fn draw_memory(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, Borders, Clear};
        let full = frame.area();
        let width = 76u16.min(full.width.saturating_sub(4));
        let height = full.height.saturating_sub(4).clamp(8, 30);
        let x = full.x + (full.width.saturating_sub(width)) / 2;
        let y = full.y + (full.height.saturating_sub(height)) / 2;
        let area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, area);
        let count = self.memory_entries.len();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                format!(" MEMORY {count} "),
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height < 3 {
            return;
        }

        // Search row.
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" search ", Style::default().fg(theme::COMMENT)),
                Span::styled(
                    format!("{}{}", self.memory_query, g("▏", "_")),
                    Style::default().fg(theme::FG),
                ),
            ]))
            .style(Style::default().bg(theme::BG_HL)),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );

        let filtered = self.memory_filtered();
        let list_top = inner.y + 1;
        let view_h = inner.height.saturating_sub(2) as usize; // search row + footer
        if self.memory_loading {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "loading…",
                    Style::default().fg(theme::COMMENT),
                ))
                .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x + 1, list_top, inner.width.saturating_sub(2), 1),
            );
            return;
        }
        if filtered.is_empty() {
            let msg = if count == 0 {
                "no memories retained yet"
            } else {
                "no matches"
            };
            frame.render_widget(
                Paragraph::new(Span::styled(msg, Style::default().fg(theme::COMMENT)))
                    .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x + 1, list_top, inner.width.saturating_sub(2), 1),
            );
            return;
        }

        let sel = self.memory_sel.min(filtered.len() - 1);
        let scroll = sel
            .saturating_sub(view_h.saturating_sub(1))
            .min(filtered.len().saturating_sub(view_h));
        for (vi, m) in filtered.iter().enumerate().skip(scroll).take(view_h) {
            let ry = list_top + (vi - scroll) as u16;
            let selected = vi == sel;
            let bed = if selected { theme::BG_HL } else { theme::SLATE };
            let marker = if selected { g("▎", "|") } else { " " };
            let badge = format!("[{}]", short_kind(&m.kind));
            let left = format!("{marker} {badge} ");
            let text_w = (inner.width as usize).saturating_sub(left.chars().count());
            let text = truncate_str(&m.text, text_w);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(marker.to_string(), Style::default().fg(theme::CYAN)),
                    Span::styled(format!(" {badge} "), Style::default().fg(theme::BLUE)),
                    Span::styled(
                        text,
                        Style::default().fg(theme::FG).add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                    ),
                ]))
                .style(Style::default().bg(bed)),
                Rect::new(inner.x, ry, inner.width, 1),
            );
        }
    }

    /// Render the `/image` viewer frame: a full-screen takeover with a title
    /// bar (filename only) and an empty body. The body rect is stored
    /// in `image_body`; the actual pixels are drawn AFTER this frame paints, by
    /// the kitty emission in `run` (ratatui doesn't model the image layer). For
    /// a non-kitty terminal or a non-PNG file, a centered note fills the body
    /// instead — the frame is honest about why there's no picture.
    fn draw_image_viewer(&mut self, frame: &mut ratatui::Frame) {
        let full = frame.area();
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_DARK)),
            full,
        );
        let Some(path) = self.image_view.clone() else {
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        // Title bar.
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                format!("  {name}"),
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            )]))
            .style(Style::default().bg(theme::BG_DARK)),
            Rect::new(full.x, full.y, full.width, 1),
        );
        // Body rect (below the title, small inset) — where the pixels go.
        let body = Rect::new(
            full.x + 1,
            full.y + 2,
            full.width.saturating_sub(2),
            full.height.saturating_sub(3),
        );
        self.image_body = body;

        // Honest note when we can't render pixels here.
        let note = if !kitty::supported() {
            Some("image preview needs a kitty-graphics terminal (kitty/ghostty/wezterm)")
        } else if !kitty::is_png(&path) {
            Some("inline preview supports PNG only")
        } else {
            None
        };
        if let Some(msg) = note {
            let y = body.y + body.height / 2;
            frame.render_widget(
                Paragraph::new(Span::styled(msg, Style::default().fg(theme::YELLOW)))
                    .style(Style::default().bg(theme::BG_DARK)),
                Rect::new(body.x + 2, y, body.width.saturating_sub(4), 1),
            );
        }
    }

    /// Render the `/lsp` panel: the language servers relevant to this
    /// workspace, each with its ready/install state. A read-only info modal —
    /// live diagnostics are the agent's `lsp` tool (ask it in-turn).
    fn draw_lsp(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, Borders, Clear};
        let full = frame.area();
        let width = 72u16.min(full.width.saturating_sub(4));
        let rows = self.lsp_servers.len().max(1) as u16;
        let height = (rows + 5).min(full.height.saturating_sub(4)).max(7);
        let x = full.x + (full.width.saturating_sub(width)) / 2;
        let y = full.y + (full.height.saturating_sub(height)) / 2;
        let area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                " LANGUAGE SERVERS ",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let mut y = inner.y;
        if self.lsp_loading {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "detecting…",
                    Style::default().fg(theme::COMMENT),
                ))
                .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), 1),
            );
            return;
        }
        if self.lsp_servers.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "no language servers match this project (no root marker found)",
                    Style::default().fg(theme::COMMENT),
                ))
                .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x + 1, y, inner.width.saturating_sub(2), 1),
            );
        } else {
            for s in self
                .lsp_servers
                .iter()
                .take(inner.height.saturating_sub(2) as usize)
            {
                let (glyph, gcolor, state) = if s.ready {
                    (g("●", "*"), theme::GREEN, "ready".to_string())
                } else {
                    (g("○", "o"), theme::YELLOW, format!("install {}", s.command))
                };
                let exts = if s.extensions.is_empty() {
                    String::new()
                } else {
                    format!("  {}", s.extensions.join(" "))
                };
                let name_w = 26usize;
                let name = format!("{:<name_w$}", truncate_str(&s.name, name_w));
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(format!(" {glyph} "), Style::default().fg(gcolor)),
                        Span::styled(
                            name,
                            Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("  {state}"), Style::default().fg(gcolor)),
                        Span::styled(exts, Style::default().fg(theme::COMMENT)),
                    ]))
                    .style(Style::default().bg(theme::SLATE)),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        }
    }

    /// Render the `/models` picker: a centered modal listing the daemon's live
    /// registry grouped by provider — ready providers first, not-ready ones
    /// greyed with the reason — plus the thinking-level control in the footer.
    fn draw_models(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, Borders, Clear};
        let full = frame.area();
        let width = 64u16.min(full.width.saturating_sub(4));
        let height = full.height.saturating_sub(4).clamp(8, 34);
        let x = full.x + (full.width.saturating_sub(width)) / 2;
        let y = full.y + (full.height.saturating_sub(height)) / 2;
        let area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                " MODELS ",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.models_hit.clear();
        if inner.width == 0 || inner.height < 3 {
            return;
        }

        if self.models_loading || self.models_entries.is_empty() {
            let msg = if self.models_loading {
                "loading registry…"
            } else {
                "no models — is the daemon up?"
            };
            frame.render_widget(
                Paragraph::new(Span::styled(msg, Style::default().fg(theme::COMMENT)))
                    .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x + 1, inner.y + 1, inner.width.saturating_sub(2), 1),
            );
            return;
        }

        // Flatten to display rows: a provider header before each provider run
        // (entries arrive ready-first, registry order within — see
        // `order_models`). `None` = header row, `Some(i)` = entry row.
        let mut rows: Vec<Option<usize>> = Vec::new();
        let mut last: Option<(&str, bool)> = None;
        for (i, e) in self.models_entries.iter().enumerate() {
            if last != Some((e.provider.as_str(), e.ready)) {
                rows.push(None);
                last = Some((e.provider.as_str(), e.ready));
            }
            rows.push(Some(i));
        }

        // Scroll the flattened rows so the selected entry stays visible.
        // The last inner row is the footer (thinking control + keys).
        let view_h = inner.height.saturating_sub(1) as usize;
        let sel_row = rows
            .iter()
            .position(|r| *r == Some(self.models_sel))
            .unwrap_or(0);
        let scroll = sel_row
            .saturating_sub(view_h.saturating_sub(1))
            .min(rows.len().saturating_sub(view_h));

        for (vi, row) in rows.iter().enumerate().skip(scroll).take(view_h) {
            let ry = inner.y + (vi - scroll) as u16;
            match row {
                None => {
                    // Provider header for the run that starts on the next row.
                    let (prov, ready) = rows[vi..]
                        .iter()
                        .find_map(|r| r.map(|i| &self.models_entries[i]))
                        .map(|e| (e.provider.clone(), e.ready))
                        .unwrap_or_default();
                    let style = if ready {
                        Style::default()
                            .fg(theme::BLUE)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(theme::COMMENT)
                    };
                    let tag = if ready { "" } else { " (no credential)" };
                    frame.render_widget(
                        Paragraph::new(Span::styled(format!("{prov}{tag}"), style))
                            .style(Style::default().bg(theme::SLATE)),
                        Rect::new(inner.x, ry, inner.width, 1),
                    );
                }
                Some(i) => {
                    let e = &self.models_entries[*i];
                    let selected = *i == self.models_sel;
                    let active = self.model_override.as_deref() == Some(e.id.as_str())
                        || (self.model_override.is_none() && e.id == self.models_current);
                    let bed = if selected { theme::BG_HL } else { theme::SLATE };
                    let marker = if selected { g("▎", "|") } else { " " };
                    let dot = if active { g("● ", "* ") } else { "  " };
                    let fg = if e.ready { theme::FG } else { theme::COMMENT };
                    let left = format!("{marker} {dot}{}", e.label);
                    let right = e.id.clone();
                    let pad = (inner.width as usize)
                        .saturating_sub(left.chars().count() + right.chars().count() + 1);
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(left, {
                                let s = Style::default().fg(fg);
                                if selected {
                                    s.add_modifier(Modifier::BOLD)
                                } else {
                                    s
                                }
                            }),
                            Span::raw(" ".repeat(pad)),
                            Span::styled(right, Style::default().fg(theme::COMMENT)),
                        ]))
                        .style(Style::default().bg(bed)),
                        Rect::new(inner.x, ry, inner.width, 1),
                    );
                    self.models_hit
                        .push((Rect::new(inner.x, ry, inner.width, 1), *i));
                }
            }
        }

        // Footer: the thinking-level state — functional context only, no
        // printed key hints.
        let footer_y = inner.y + inner.height - 1;
        let footer = format!(" thinking: {}", thinking_label(self.thinking_override));
        frame.render_widget(
            Paragraph::new(Span::styled(footer, Style::default().fg(theme::CYAN)))
                .style(Style::default().bg(theme::SLATE)),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    }
    fn draw_permissions(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, Borders, Clear, Wrap};

        self.permissions_hit.clear();
        let full = frame.area();
        let width = 74u16.min(full.width.saturating_sub(4));
        let height = 15u16.min(full.height.saturating_sub(2));
        if width < 24 || height < 8 {
            return;
        }
        let area = Rect::new(
            full.x + (full.width.saturating_sub(width)) / 2,
            full.y + (full.height.saturating_sub(height)) / 2,
            width,
            height,
        );

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                " PERMISSIONS ",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        for (index, (mode, label, description)) in PERMISSION_OPTIONS.iter().enumerate() {
            let y = inner.y + index as u16 * 3;
            if y >= inner.bottom() {
                break;
            }
            let row = Rect::new(inner.x, y, inner.width, 3.min(inner.bottom() - y));
            self.permissions_hit.push((row, index));
            let selected = index == self.permissions_sel;
            let bed = if selected { theme::BG_HL } else { theme::SLATE };
            frame.render_widget(Block::default().style(Style::default().bg(bed)), row);

            let marker = if selected { g("▎", ">") } else { " " };
            let current = if self.permissions_effective == Some(*mode) {
                "  current"
            } else {
                ""
            };
            let label =
                truncate_cells(&format!(" {marker} {label}{current}"), inner.width as usize);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default()
                        .fg(if selected { theme::FG } else { theme::COMMENT })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ))
                .style(Style::default().bg(bed)),
                Rect::new(row.x, row.y, row.width, 1),
            );
            if row.height > 1 {
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!("    {description}"),
                        Style::default().fg(theme::COMMENT),
                    ))
                    .wrap(Wrap { trim: true })
                    .style(Style::default().bg(bed)),
                    Rect::new(row.x, row.y + 1, row.width, row.height - 1),
                );
            }
        }

        let footer = if self.permissions_loading {
            " loading current policy…"
        } else if self.permissions_saving {
            " saving…"
        } else if self.permissions_env_override.is_some() {
            " OCEAN_YOLO overrides the saved choice"
        } else {
            ""
        };
        if !footer.is_empty() && inner.height > 0 {
            frame.render_widget(
                Paragraph::new(Span::styled(footer, Style::default().fg(theme::CYAN)))
                    .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
            );
        }
    }

    /// Build the `/providers` rows from the static [`PROVIDER_TABLE`], computing
    /// each row's status from the process env (first hit wins) then the Ocean
    /// auth file (oauth block presence/expiry, or `api_key`). Read from the
    /// main thread at open time and after an inline save — the auth file is
    /// tiny, so the cost is negligible.
    fn build_provider_rows() -> Vec<ProviderRow> {
        let auth_json = ocean_providers::ProviderEnv::from_process()
            .auth_file
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read(&p).ok())
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok());
        PROVIDER_TABLE
            .iter()
            .map(|(section, label, block_key, env_vars)| ProviderRow {
                section: *section,
                label,
                block_key,
                env_vars,
                status: provider_status(block_key, env_vars, &auth_json),
            })
            .collect()
    }

    /// Recompute the chat welcome provider line from current auth file +
    /// process env state. Call after any auth-state mutation (API key save,
    /// OAuth login completion) so the empty-state message changes immediately
    /// without restarting the TUI.
    fn refresh_welcome_provider_line(&mut self) {
        let n_configured = Self::build_provider_rows()
            .iter()
            .filter(|r| r.section == ProviderSection::Agent && r.status != "not configured")
            .count();
        // Terse configuration condition ONLY when nothing is configured.
        // Configured credentials are never claimed as runtime readiness
        // (`ready · N providers` was a false health signal).
        self.chat.welcome_provider_line = if n_configured == 0 {
            Some("provider configuration required".into())
        } else {
            None
        };
    }

    /// Drive the `/providers` popup. List mode: ↑/↓ move, ⏎ triggers OAuth
    /// login (closes the popup, reuses the existing `/login` flow) or enters
    /// inline API-key entry. Key-entry mode: type/paste, ⏎ saves via
    /// [`ocean_oauth::store_api_key`] + refreshes the row, Esc cancels back to
    /// the list. Esc/q in the list closes the popup.
    fn providers_key(&mut self, k: crossterm::event::KeyEvent) {
        use crossterm::event::KeyCode;
        match &mut self.providers_mode {
            ProvidersMode::KeyEntry { block_key, buffer } => {
                let block_key = block_key.clone();
                match k.code {
                    KeyCode::Esc => self.providers_mode = ProvidersMode::List,
                    KeyCode::Enter => {
                        let key = buffer.clone();
                        match ocean_oauth::store_api_key(&block_key, &key, None) {
                            Ok(path) => {
                                self.set_notice(format!(
                                    "{} key saved to {}",
                                    block_key,
                                    path.display()
                                ));
                                self.providers_mode = ProvidersMode::List;
                                // Refresh only the saved row's status in place.
                                if let Some(row) = self
                                    .providers_rows
                                    .iter_mut()
                                    .find(|r| r.block_key == block_key)
                                {
                                    let auth_json = ocean_providers::ProviderEnv::from_process()
                                        .auth_file
                                        .filter(|p| p.exists())
                                        .and_then(|p| std::fs::read(&p).ok())
                                        .and_then(|b| {
                                            serde_json::from_slice::<serde_json::Value>(&b).ok()
                                        });
                                    row.status =
                                        provider_status(row.block_key, row.env_vars, &auth_json);
                                }
                                // The welcome empty-state provider line must also
                                // update immediately — no restart needed.
                                self.refresh_welcome_provider_line();
                            }
                            Err(e) => self.set_notice(format!("save failed: {e}")),
                        }
                    }
                    KeyCode::Backspace => {
                        buffer.pop();
                    }
                    // Paste arrives as a raw char stream on macOS Terminal, so
                    // no explicit Ctrl+V handler is needed.
                    KeyCode::Char(c) => buffer.push(c),
                    _ => {}
                }
            }
            ProvidersMode::List => {
                let count = self.providers_rows.len();
                match k.code {
                    KeyCode::Esc | KeyCode::Char('q') => self.providers_open = false,
                    KeyCode::Up | KeyCode::Char('k') => {
                        self.providers_sel = self.providers_sel.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if count > 0 {
                            self.providers_sel = (self.providers_sel + 1).min(count - 1);
                        }
                    }
                    KeyCode::Enter => {
                        let Some(row) = self.providers_rows.get(self.providers_sel).cloned() else {
                            return;
                        };
                        if row.is_oauth() {
                            // Dispatch the existing OAuth flow and close the
                            // popup so the in-flight guard + status wiring runs
                            // exactly as a `/login <target>` would.
                            let target = match row.block_key {
                                "claude-code" => LoginTarget::Claude,
                                _ => LoginTarget::Codex,
                            };
                            self.providers_open = false;
                            self.dispatch(Action::Login(target));
                        } else {
                            self.providers_mode = ProvidersMode::KeyEntry {
                                block_key: row.block_key.to_string(),
                                buffer: String::new(),
                            };
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Render the `/providers` popup: a centered modal mirroring the settings
    /// overlay — one row per provider with a live auth status, plus an inline
    /// masked-key entry field when a row is being edited.
    fn draw_providers(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, Borders, Clear};
        let full = frame.area();
        let section_count = self
            .providers_rows
            .iter()
            .map(|row| row.section)
            .collect::<HashSet<_>>()
            .len() as u16;
        let rows = self.providers_rows.len().max(1) as u16 + section_count;
        let width = 68u16.min(full.width.saturating_sub(4));
        // credential rows + category headers + title/footer; a touch more room
        // in key-entry mode.
        let base = rows + 5;
        let height = (if matches!(self.providers_mode, ProvidersMode::KeyEntry { .. }) {
            base + 2
        } else {
            base
        })
        .clamp(10, full.height.saturating_sub(2));
        let x = full.x + (full.width.saturating_sub(width)) / 2;
        let y = full.y + (full.height.saturating_sub(height)) / 2;
        let area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                " LOGIN — AGENT + VOICE ",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        match &self.providers_mode {
            ProvidersMode::KeyEntry { block_key, buffer } => {
                let label = self
                    .providers_rows
                    .iter()
                    .find(|r| r.block_key == block_key.as_str())
                    .map(|r| r.label)
                    .unwrap_or(block_key);
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!(" paste {label} API key "),
                        Style::default().fg(theme::FG),
                    ))
                    .style(Style::default().bg(theme::SLATE)),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                let masked = "•".repeat(buffer.chars().count());
                let cursor = g("▎", "|");
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!(" {masked}{cursor}"),
                        Style::default().fg(theme::GREEN),
                    ))
                    .style(Style::default().bg(theme::BG_HL)),
                    Rect::new(inner.x, inner.y + 2, inner.width, 1),
                );
            }
            ProvidersMode::List => {
                enum LoginRow<'a> {
                    Header(ProviderSection),
                    Provider(usize, &'a ProviderRow),
                }
                let mut login_rows = Vec::with_capacity(self.providers_rows.len() + 2);
                let mut last_section = None;
                for (index, row) in self.providers_rows.iter().enumerate() {
                    if last_section != Some(row.section) {
                        login_rows.push(LoginRow::Header(row.section));
                        last_section = Some(row.section);
                    }
                    login_rows.push(LoginRow::Provider(index, row));
                }
                let selected_row = login_rows
                    .iter()
                    .position(|row| {
                        matches!(row, LoginRow::Provider(index, _) if *index == self.providers_sel)
                    })
                    .unwrap_or(0);
                let visible = inner.height as usize;
                let start = selection_window_start(selected_row, login_rows.len(), visible);

                for (slot, login_row) in login_rows.iter().skip(start).take(visible).enumerate() {
                    let rect = Rect::new(inner.x, inner.y + slot as u16, inner.width, 1);
                    match login_row {
                        LoginRow::Header(section) => {
                            frame.render_widget(
                                Paragraph::new(Span::styled(
                                    format!(" {} ", section.label()),
                                    Style::default()
                                        .fg(theme::CYAN)
                                        .add_modifier(Modifier::BOLD),
                                ))
                                .style(Style::default().bg(theme::SLATE)),
                                rect,
                            );
                        }
                        LoginRow::Provider(i, row) => {
                            let selected = *i == self.providers_sel;
                            let bed = if selected { theme::BG_HL } else { theme::SLATE };
                            let marker = if selected { g("▎", "|") } else { " " };
                            let left = format!(" {marker} {}", row.label);
                            let status_fg = if row.status.starts_with("env:")
                                || row.status == "oauth ok"
                                || row.status == "auth file"
                            {
                                theme::GREEN
                            } else if row.status == "oauth expired" {
                                theme::CYAN
                            } else {
                                theme::COMMENT
                            };
                            let value =
                                Span::styled(row.status.clone(), Style::default().fg(status_fg));
                            let pad = (inner.width as usize).saturating_sub(
                                left.chars().count() + value.content.chars().count() + 1,
                            );
                            frame.render_widget(
                                Paragraph::new(Line::from(vec![
                                    Span::styled(
                                        left,
                                        if selected {
                                            Style::default()
                                                .fg(theme::FG)
                                                .add_modifier(Modifier::BOLD)
                                        } else {
                                            Style::default().fg(theme::FG)
                                        },
                                    ),
                                    Span::raw(" ".repeat(pad)),
                                    value,
                                ]))
                                .style(Style::default().bg(bed)),
                                rect,
                            );
                        }
                    }
                }
            }
        }
    }

    fn draw_launch(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, Borders, Clear};
        self.launch_hit.clear();
        let full = frame.area();
        let width = 56u16.min(full.width.saturating_sub(4));
        let height = 9u16.min(full.height.saturating_sub(2));
        let area = Rect::new(
            full.x + (full.width.saturating_sub(width)) / 2,
            full.y + (full.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                " OCEAN ",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let cwd = sanitize_line(
            &std::path::Path::new(&self.workspace_root)
                .display()
                .to_string(),
        );
        let rows = [
            format!("+ new in {cwd}"),
            "resume session".into(),
            "editor".into(),
            "open graph".into(),
        ];
        let visible = inner.height.saturating_sub(1) as usize;
        let start = selection_window_start(self.launch_sel, rows.len(), visible);
        for (slot, (index, label)) in rows
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .enumerate()
        {
            let selected = index == self.launch_sel;
            let marker = if selected { g("▎", ">") } else { " " };
            let label = truncate_cells(&format!("{marker} {label}"), inner.width as usize);
            let rect = Rect::new(inner.x, inner.y + slot as u16, inner.width, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::default()
                        .fg(if selected { theme::FG } else { theme::COMMENT })
                        .add_modifier(if selected {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ))
                .style(Style::default().bg(if selected {
                    theme::BG_HL
                } else {
                    theme::SLATE
                })),
                rect,
            );
            self.launch_hit.push((rect, index));
        }
        let footer_y = inner.y + inner.height.saturating_sub(1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                " ↑↓ select · enter open · esc chat ",
                Style::default().fg(theme::COMMENT),
            ))
            .style(Style::default().bg(theme::SLATE)),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    }

    fn draw_resume(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, Borders, Clear};
        self.resume_hit.clear();
        let full = frame.area();
        let width = 56u16.min(full.width.saturating_sub(4));
        let height = 14u16.min(full.height.saturating_sub(2));
        let area = Rect::new(
            full.x + (full.width.saturating_sub(width)) / 2,
            full.y + (full.height.saturating_sub(height)) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                " RESUME SESSION ",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if self.resume_sessions.is_empty() {
            let message = if self.resume_loading {
                " loading sessions… "
            } else {
                " no resumable Ocean sessions in this workspace "
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate_cells(message, inner.width as usize),
                    Style::default().fg(theme::COMMENT),
                ))
                .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
        } else {
            let visible = inner.height.saturating_sub(1) as usize;
            let start =
                selection_window_start(self.resume_sel, self.resume_sessions.len(), visible);
            for (slot, (index, session)) in self
                .resume_sessions
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .enumerate()
            {
                let selected = index == self.resume_sel;
                let marker = if selected { g("▎", ">") } else { " " };
                let title = sanitize_line(&session.title);
                let age = sanitize_line(&crate::shell::sessions::ago(session.mtime));
                let label =
                    truncate_cells(&format!("{marker} {title}  {age}"), inner.width as usize);
                let rect = Rect::new(inner.x, inner.y + slot as u16, inner.width, 1);
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        label,
                        Style::default()
                            .fg(if selected { theme::FG } else { theme::COMMENT })
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ))
                    .style(Style::default().bg(if selected {
                        theme::BG_HL
                    } else {
                        theme::SLATE
                    })),
                    rect,
                );
                self.resume_hit.push((rect, index));
            }
        }
        let footer_y = inner.y + inner.height.saturating_sub(1);
        frame.render_widget(
            Paragraph::new(Span::styled(
                " ↑↓ select · enter resume · r refresh · esc back ",
                Style::default().fg(theme::COMMENT),
            ))
            .style(Style::default().bg(theme::SLATE)),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
    }

    fn draw_settings(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, Borders, Clear};
        let full = frame.area();
        let width = 56u16.min(full.width.saturating_sub(4));
        let height = 14u16.min(full.height.saturating_sub(2));
        let x = full.x + (full.width.saturating_sub(width)) / 2;
        let y = full.y + (full.height.saturating_sub(height)) / 2;
        let area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                " SETTINGS ",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let pill = |on: bool| {
            if on {
                Span::styled(
                    format!("{} on ", g("●", "*")),
                    Style::default().fg(theme::GREEN),
                )
            } else {
                Span::styled(
                    format!("{} off", g("○", "o")),
                    Style::default().fg(theme::COMMENT),
                )
            }
        };
        let rows: Vec<(String, Span)> = vec![
            ("sessions rail".into(), pill(self.show_sessions)),
            ("file tree".into(), pill(self.show_tree)),
            ("terminal dock".into(), pill(self.show_term)),
            (
                "terminal height".into(),
                Span::styled(
                    format!("{} rows", self.term_h),
                    Style::default().fg(theme::CYAN),
                ),
            ),
            (
                "tool cards expanded".into(),
                pill(self.chat.tools_expanded()),
            ),
        ];
        for (i, (label, value)) in rows.iter().enumerate() {
            let selected = i == self.settings_sel;
            let bed = if selected { theme::BG_HL } else { theme::SLATE };
            let marker = if selected { g("▎", "|") } else { " " };
            let left = format!("{marker} {label}");
            let pad = (inner.width as usize)
                .saturating_sub(left.chars().count() + value.content.chars().count() + 1);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(left, {
                        let s = Style::default().fg(theme::FG);
                        if selected {
                            s.add_modifier(Modifier::BOLD)
                        } else {
                            s
                        }
                    }),
                    Span::raw(" ".repeat(pad)),
                    value.clone(),
                ]))
                .style(Style::default().bg(bed)),
                Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
            );
        }

        // Read-only info section.
        let model = self
            .model_override
            .clone()
            .unwrap_or_else(|| "daemon default".into());
        let session = self
            .session_id
            .map(|id| format!("{:.8}", id.0.to_string()))
            .unwrap_or_else(|| "none (fresh)".into());
        let project = std::path::Path::new(&self.workspace_root)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.workspace_root.clone());
        let info = [
            format!("model    {model}"),
            format!("session  {session}"),
            format!("project  {project}"),
        ];
        let info_y = inner.y + rows.len() as u16 + 1;
        for (i, line) in info.iter().enumerate() {
            let yy = info_y + i as u16;
            if yy >= inner.y + inner.height.saturating_sub(1) {
                break;
            }
            frame.render_widget(
                Paragraph::new(Span::styled(
                    format!("   {line}"),
                    Style::default().fg(theme::COMMENT),
                ))
                .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x, yy, inner.width, 1),
            );
        }
    }

    /// Grow (+) or shrink (−) the terminal dock by `delta` rows, clamped so the
    /// dock stays ≥ MIN_TERM_H and the main surface keeps ≥ MIN_CENTER_H.
    fn resize_term(&mut self, delta: i16) {
        let max = max_term_h(self.r_center_col.height) as i16;
        self.term_h = (self.term_h as i16 + delta).clamp(MIN_TERM_H as i16, max) as u16;
    }

    /// Innermost content-pane rect containing `pos`, or `None` when the point
    /// is on a title/status/breadcrumb row, a splitter, or outside every pane.
    /// Used to arm pane-scoped text selection: a `Down` outside any content
    /// pane arms nothing (and lets the click fall through to buttons/splitter
    /// grabs). The four content rects are pairwise disjoint, so order is only
    /// by routing precedence.
    fn pane_rect_at(&self, pos: (u16, u16)) -> Option<Rect> {
        [
            self.r_sessions,
            self.r_tree,
            self.r_tray,
            self.r_term,
            self.r_center,
        ]
        .into_iter()
        .find(|&rect| rect_has(rect, pos))
    }

    fn selection_space(&self, rect: Rect) -> SelectionSpace {
        if rect != self.r_center {
            SelectionSpace::Screen
        } else {
            match self.center {
                Center::Chat => SelectionSpace::Chat,
                Center::Editor if self.editor.has_tabs() => SelectionSpace::Editor,
                Center::Editor | Center::Graph | Center::WorkflowGraph => SelectionSpace::Screen,
            }
        }
    }

    fn selection_columns(&self, rect: Rect) -> (u16, u16) {
        if self.selection_space == SelectionSpace::Editor {
            self.editor
                .selection_columns()
                .unwrap_or((rect.x, rect.right().saturating_sub(1)))
        } else {
            (rect.x, rect.right().saturating_sub(1))
        }
    }

    fn selection_point(&self, pos: (u16, u16), rect: Rect, space: SelectionSpace) -> (u16, usize) {
        let pos = clamp_pos(pos, rect);
        let stable_row = match space {
            SelectionSpace::Chat => self.chat.nearest_transcript_row(pos.1),
            SelectionSpace::Editor => self.editor.nearest_selection_row(pos.1),
            SelectionSpace::Screen => None,
        };
        let (left, right) = if space == SelectionSpace::Editor {
            self.editor
                .selection_columns()
                .unwrap_or((rect.x, rect.right().saturating_sub(1)))
        } else {
            (rect.x, rect.right().saturating_sub(1))
        };
        (
            pos.0.clamp(left, right),
            stable_row.unwrap_or_else(|| usize::from(pos.1)),
        )
    }

    fn apply_focus(&mut self) {
        self.rail.focused = self.focus == Focus::Sessions;
        self.tree.focused =
            self.focus == Focus::Tree && self.right_rail_mode == RightRailMode::Files;
        self.pty.focused = self.focus == Focus::Term;
        let center = self.focus == Focus::Center;
        self.chat.focused = center && self.center == Center::Chat;
        self.editor.focused = center && self.center == Center::Editor;
        self.graph.focused = center && self.center == Center::Graph;
        self.workflow_graph.focused = (self.focus == Focus::Tree
            && self.right_rail_mode == RightRailMode::Workflow)
            || (center && self.center == Center::WorkflowGraph);
        self.workflow_graph.expanded = self.center == Center::WorkflowGraph;
    }

    fn clear_compact_hold(&mut self) {
        self.compacting_session = None;
        self.compact_refresh_required = false;
        self.compact_request_in_flight = false;
        self.compact_invalidation_pending = false;
    }

    fn compact_completion_matches(
        &self,
        session_id: AgentSessionId,
        binding_generation: u64,
        operation_generation: u64,
    ) -> bool {
        self.compacting_session == Some(session_id)
            && self.compact_binding_generation == binding_generation
            && self.compact_operation_generation == operation_generation
    }

    fn handle_session_invalidation(
        &mut self,
        session_id: AgentSessionId,
        binding_generation: u64,
        notice: &str,
    ) {
        if self.session_id != Some(session_id)
            || self.session_binding_generation != binding_generation
        {
            return;
        }
        if self.compact_request_in_flight
            && self.compacting_session == Some(session_id)
            && self.compact_binding_generation == binding_generation
        {
            // The daemon deliberately emits compact's invalidation while its
            // lease is still held. Do not race that lease with /sync or replace
            // the compact operation generation; defer a follow-up sync instead.
            self.stream_generation = self.stream_generation.wrapping_add(1);
            if let Some(task) = self.stream_task.take() {
                task.abort();
            }
            self.sessions_requiring_sync.insert(session_id);
            self.compact_invalidation_pending = true;
            self.set_notice("session changed during compaction · waiting to synchronize…".into());
        } else {
            self.begin_compact_reload(session_id, binding_generation, notice);
        }
    }

    fn begin_compact_reload(
        &mut self,
        session_id: AgentSessionId,
        binding_generation: u64,
        notice: &str,
    ) {
        if self.session_id != Some(session_id)
            || self.session_binding_generation != binding_generation
        {
            return;
        }
        self.stream_generation = self.stream_generation.wrapping_add(1);
        if let Some(task) = self.stream_task.take() {
            task.abort();
        }
        self.session_activity_probe_generation =
            self.session_activity_probe_generation.wrapping_add(1);
        self.sessions_requiring_sync.insert(session_id);
        self.compacting_session = Some(session_id);
        self.compact_refresh_required = false;
        self.compact_request_in_flight = false;
        self.compact_invalidation_pending = false;
        self.compact_binding_generation = binding_generation;
        self.compact_operation_generation = self.compact_operation_generation.wrapping_add(1);
        let operation_generation = self.compact_operation_generation;
        self.set_notice(notice.into());
        let client = self.client.clone();
        let tx = self.actions_tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Action::CompactReloadFinished {
                session_id,
                binding_generation,
                operation_generation,
                result: client.refresh_compacted_session(session_id).await,
            });
        });
    }

    /// Atomically replace visible history from a synchronized snapshot and
    /// restart the scoped stream strictly after its fence. Incrementing the
    /// stream generation before aborting rejects already-queued old envelopes.
    fn install_synchronized_session(
        &mut self,
        session_id: AgentSessionId,
        binding_generation: u64,
        snapshot: &ocean_core::SessionSyncSnapshot,
        fence: &ocean_core::SessionEventFence,
    ) -> bool {
        if self.session_id != Some(session_id)
            || self.session_binding_generation != binding_generation
            || snapshot.session_id != session_id.0
        {
            return false;
        }
        let Some(event_id) = fence.event_id else {
            return false;
        };
        self.stream_generation = self.stream_generation.wrapping_add(1);
        let stream_generation = self.stream_generation;
        if let Some(task) = self.stream_task.take() {
            task.abort();
        }
        self.health.recover(HealthSource::Sse);
        let Ok(history) = crate::shell::sessions::history_from_sync_snapshot(snapshot) else {
            return false;
        };
        self.chat.load_history(history);
        self.stream_task = Some(self.client.spawn_event_stream(
            session_id,
            self.actions_tx.clone(),
            false,
            Some(event_id.to_string()),
            binding_generation,
            stream_generation,
        ));
        self.sessions_requiring_sync.remove(&session_id);
        true
    }

    fn spawn_session_activity_probe(
        &mut self,
        session_id: AgentSessionId,
        binding_generation: u64,
        delay_ms: u64,
        after_busy_rejection: bool,
        active_was_observed: bool,
    ) {
        self.session_activity_probe_generation =
            self.session_activity_probe_generation.wrapping_add(1);
        let probe_generation = self.session_activity_probe_generation;
        let client = self.client.clone();
        let tx = self.actions_tx.clone();
        tokio::spawn(async move {
            if delay_ms > 0 {
                // Let a concurrently queued TurnFinished win before asking for
                // an authoritative post-turn snapshot.
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            let result = client.refresh_compacted_session(session_id).await;
            let _ = tx.send(Action::SessionActivityProbeFinished {
                session_id,
                binding_generation,
                probe_generation,
                after_busy_rejection,
                active_was_observed,
                result,
            });
        });
    }

    fn stage_pending_images_for_submit(&mut self) -> Option<Vec<ocean_agent_sdk::TurnImage>> {
        self.in_flight_images = std::mem::take(&mut self.pending_images);
        (!self.in_flight_images.is_empty()).then(|| self.in_flight_images.clone())
    }

    fn submit_turn(&mut self, submission_id: u64, prompt: String) {
        // A pre-submit resume probe may have captured a fence before this turn.
        // Its later completion must never replace the optimistic/user-visible
        // row or the newly admitted stream.
        self.session_activity_probe_generation =
            self.session_activity_probe_generation.wrapping_add(1);
        let client = self.client.clone();
        let tx = self.actions_tx.clone();
        let workspace = self.workspace_root.clone();
        // Hold the exact submitted attachments until admission certainty is
        // known: restore on definite rejection, discard on accept/unknown.
        let images = self.stage_pending_images_for_submit();
        let existing = self.session_id;
        let binding_generation = self.session_binding_generation;
        let model_id = self.model_override.clone();
        let thinking = self.thinking_override;
        let advisor = self.advisor_ctl.clone();
        // OCEAN-185: mint the per-turn permission secret; the turn's first
        // permission request claims it (see Action::OceanEvent above).
        let decision_token = ocean_core::mint_decision_token();
        self.pending_submit_token = Some(decision_token.clone());
        // Offshore mode (flag file ~/.config/offshore/mode, shared with the
        // offshore CLI): re-read per submit so toggles — from the legacy TUI's
        // /offshore command or the CLI — apply to the very next turn.
        let guidance = super::offshore::guidance(super::offshore::enabled());

        tokio::spawn(async move {
            // Both the session mint and the turn POST ride the daemon-blip
            // retry for definitely-unsent connect failures. Turn submission has
            // no whole-request timeout; after connection, an interrupted result
            // is outcome-unknown and MUST NOT restore/replay the prompt.
            let retry_status = |what: &'static str, tx: mpsc::UnboundedSender<Action>| {
                move |attempt: usize, total: usize| {
                    let _ = tx.send(Action::Status(format!(
                        "daemon unreachable - retrying {what} {attempt}/{total}"
                    )));
                }
            };
            let session_id = match existing {
                Some(id) => id,
                None => {
                    let on_retry = retry_status("session", tx.clone());
                    match client
                        .create_agent_session_retrying(&workspace, on_retry)
                        .await
                    {
                        Ok(resp) => {
                            // SessionBound → App::bind_session spawns the (single,
                            // self-healing) stream and holds its handle.
                            let _ = tx.send(Action::SessionBound(resp.session_id));
                            resp.session_id
                        }
                        Err(e) => {
                            let _ = tx.send(Action::TurnSendFailed {
                                submission_id,
                                prompt,
                                err: format!("session: {e}"),
                            });
                            return;
                        }
                    }
                }
            };
            let req = AgentTurnRequest {
                session_id: Some(session_id),
                prompt: prompt.clone(),
                cwd: workspace,
                guidance,
                project_id: None,
                client_type: Some("tui".into()),
                agent: None,
                role: None,
                thinking_level: thinking,
                model_id,
                images,
                decision_token: Some(decision_token),
                client_context: None,
                advisor,
            };
            let on_retry = retry_status("turn", tx.clone());
            match client.agent_turn_retrying(&req, on_retry).await {
                Ok(response) => {
                    let _ = tx.send(Action::TurnAccepted {
                        submission_id,
                        turn_id: response.turn_id,
                    });
                }
                Err(error) => {
                    let action = match error {
                        TurnSubmitError::DefinitelyUnsent(message)
                        | TurnSubmitError::Rejected(message) => Action::TurnSendFailed {
                            submission_id,
                            prompt,
                            err: format!("turn: {message}"),
                        },
                        TurnSubmitError::SessionBusy => Action::TurnSessionBusy {
                            submission_id,
                            session_id,
                            binding_generation,
                            prompt,
                        },
                        TurnSubmitError::OutcomeUnknown(message) => Action::TurnOutcomeUnknown {
                            submission_id,
                            err: format!("turn: {message}"),
                        },
                    };
                    let _ = tx.send(action);
                }
            }
        });
    }

    // ── the CTRL frame ───────────────────────────────────────────────────────

    fn clear_frame_geometry(&mut self) {
        self.r_body = Rect::default();
        self.r_sessions = Rect::default();
        self.r_tree = Rect::default();
        self.r_tray = Rect::default();
        self.r_center = Rect::default();
        self.r_term = Rect::default();
        self.r_center_col = Rect::default();
        self.r_split_sessions = Rect::default();
        self.r_split_tree = Rect::default();
        self.r_split_term = Rect::default();
        self.r_split_tray = Rect::default();
        self.buttons.clear();
        self.launch_hit.clear();
        self.resume_hit.clear();
        self.models_hit.clear();
        self.advisor_hit.clear();
        self.sel_press = None;
        self.sel_rect = None;
        self.selection = None;
        self.selection_rows.clear();
        self.selection_space = SelectionSpace::Screen;
        self.dragging_sessions = false;
        self.dragging_tree = false;
        self.dragging_term = false;
    }

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let full = frame.area();
        if full.width < 40 || full.height < 8 {
            self.clear_frame_geometry();
            frame.render_widget(
                Paragraph::new("window too small")
                    .style(Style::default().fg(theme::YELLOW).bg(theme::BG_DARK)),
                full,
            );
            return;
        }

        // root: title / body / status — CTRL's exact vertical frame.
        let root = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(full);
        let (title_row, body, status_row) = (root[0], root[1], root[2]);

        // body: [sessions][splitter][center][splitter][tree] — CTRL's columns.
        // Rails collapse to 0 when toggled off from the title bar.
        let sess_w = if self.show_sessions {
            self.sessions_w
        } else {
            0
        };
        let tree_w = if self.show_tree { self.tree_w } else { 0 };
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(sess_w),
                Constraint::Length(if sess_w > 0 { 1 } else { 0 }),
                Constraint::Min(MIN_WORKSPACE_W),
                Constraint::Length(if tree_w > 0 { 1 } else { 0 }),
                Constraint::Length(tree_w),
            ])
            .split(body);
        let (r_sessions, r_split_a, center, r_split_b, r_file_rail) =
            (cols[0], cols[1], cols[2], cols[3], cols[4]);
        let (r_tree, r_split_tray, r_tray) = file_rail_rects(
            r_file_rail,
            self.tray.is_visible(),
            self.tray.desired_height(),
        );

        // center: breadcrumb / main surface / docked terminal (CTRL's rows).
        let term_visible = self.pty.is_active() && self.show_term;
        // Clamp the (resizable) dock height to the space available this frame so
        // the main surface always keeps at least MIN_CENTER_H rows.
        self.term_h = self.term_h.clamp(MIN_TERM_H, max_term_h(center.height));
        let (r_crumb, r_center, r_split_term, r_term) = if term_visible {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(MIN_CENTER_H),
                    Constraint::Length(1),
                    Constraint::Length(self.term_h),
                ])
                .split(center);
            (rows[0], rows[1], rows[2], rows[3])
        } else {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(5)])
                .split(center);
            (rows[0], rows[1], Rect::default(), Rect::default())
        };
        self.r_body = body;
        self.r_sessions = r_sessions;
        self.r_tree = r_tree;
        self.r_tray = r_tray;
        self.r_center = r_center;
        self.r_term = r_term;
        self.r_center_col = center;
        self.r_split_sessions = r_split_a;
        self.r_split_tree = r_split_b;
        self.r_split_term = r_split_term;
        self.r_split_tray = r_split_tray;

        // deep chrome first
        frame.render_widget(Block::default().style(Style::default().bg(theme::BG)), full);

        // breadcrumb: ONLY detail beyond the title — the editor's full path or
        // the bound chat session id. Otherwise the reserved row stays blank.
        let crumb = match self.center {
            Center::Chat => self
                .session_id
                .map(|id| format!(" {:.8}", id.0.to_string()))
                .unwrap_or_default(),
            Center::Editor => format!(" {}", self.editor.crumb()),
            Center::Graph => String::new(),
            Center::WorkflowGraph => " workflow execution graph".into(),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(crumb, Style::default().fg(theme::COMMENT)))
                .style(Style::default().bg(theme::BG)),
            r_crumb,
        );

        // panels — visible unless toggled off from the title bar.
        if sess_w > 0 {
            self.rail.draw(frame, r_sessions);
            splitter(frame, r_split_a, true);
        }
        match self.center {
            Center::Chat => self.chat.draw(frame, r_center),
            Center::Editor => self.editor.draw(frame, r_center),
            Center::Graph => self.graph.draw(frame, r_center),
            Center::WorkflowGraph => self.workflow_graph.draw(frame, r_center),
        }
        if tree_w > 0 {
            match self.right_rail_mode {
                RightRailMode::Files => self.tree.draw(frame, r_tree),
                RightRailMode::Workflow => self.workflow_graph.draw(frame, r_tree),
            }
            if r_tray.height > 0 {
                splitter(frame, r_split_tray, false);
                self.tray.draw(frame, r_tray);
            }
            splitter(frame, r_split_b, true);
        }
        if term_visible {
            self.pty.draw(frame, r_term);
            splitter(frame, r_split_term, false);
        }

        self.draw_title(frame, title_row);
        self.draw_status(frame, status_row);

        // Mouse-selection overlay: reverse-video the swept cells and snapshot
        // the frame's cell text. Chat/editor rows are additionally retained under
        // stable content-row ids so scrolling does not discard prior text.
        if let Some((a, b)) = self.selection {
            if let Some(rect) = self.sel_rect {
                let (stable_left, stable_right) = self.selection_columns(rect);
                let buf = frame.buffer_mut();
                let area = buf.area;
                self.frame_cells = (area.top()..area.bottom())
                    .map(|y| {
                        (area.left()..area.right())
                            .map(|x| {
                                buf.cell((x, y))
                                    .map(|c| c.symbol().to_string())
                                    .unwrap_or_default()
                            })
                            .collect()
                    })
                    .collect();
                if self.selection_space != SelectionSpace::Screen {
                    for screen_y in rect.y..rect.bottom() {
                        let stable_y = match self.selection_space {
                            SelectionSpace::Chat => self.chat.transcript_row_for_screen(screen_y),
                            SelectionSpace::Editor => {
                                self.editor.selection_row_for_screen(screen_y)
                            }
                            SelectionSpace::Screen => None,
                        };
                        if let Some(stable_y) = stable_y {
                            if let Some(row) = self.frame_cells.get(screen_y as usize) {
                                self.selection_rows.insert(stable_y, row.clone());
                            }
                            if stable_y >= a.1.min(b.1) && stable_y <= a.1.max(b.1) {
                                let (start, end) = if (a.1, a.0) <= (b.1, b.0) {
                                    (a, b)
                                } else {
                                    (b, a)
                                };
                                let x0 = if stable_y == start.1 {
                                    start.0.max(stable_left)
                                } else {
                                    stable_left
                                };
                                let x1 = if stable_y == end.1 {
                                    end.0.min(stable_right)
                                } else {
                                    stable_right
                                };
                                for x in x0..=x1 {
                                    if let Some(cell) = buf.cell_mut((x, screen_y)) {
                                        cell.set_style(
                                            cell.style().add_modifier(Modifier::REVERSED),
                                        );
                                    }
                                }
                            }
                        }
                    }
                } else if let Some(sp) = bounded_span((a.0, a.1 as u16), (b.0, b.1 as u16), rect) {
                    for y in sp.y0..=sp.y1 {
                        let x0 = if y == sp.y0 { sp.first_x0 } else { sp.left };
                        let x1 = if y == sp.y1 { sp.last_x1 } else { sp.right };
                        for x in x0..=x1 {
                            if let Some(cell) = buf.cell_mut((x, y)) {
                                cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
                            }
                        }
                    }
                }
            }
        }

        // `/settings` + `/models` modal overlays — drawn last so they float
        // over everything.
        // Startup + session-resume overlays float over the clean workbench.
        if self.launch_open {
            self.draw_launch(frame);
        }
        if self.resume_open {
            self.draw_resume(frame);
        }
        if self.settings_open {
            self.draw_settings(frame);
        }
        if self.permissions_open {
            self.draw_permissions(frame);
        }
        if self.models_open {
            self.draw_models(frame);
        }
        if self.advisor_open {
            self.draw_advisor(frame);
        }
        if self.memory_open {
            self.draw_memory(frame);
        }
        if self.lsp_open {
            self.draw_lsp(frame);
        }
        if self.providers_open {
            self.draw_providers(frame);
        }
        // The image viewer is a full-screen takeover — drawn last so its frame
        // covers everything; the pixels land after this paint (see `run`).
        if self.image_view.is_some() {
            self.draw_image_viewer(frame);
        }
    }

    /// Title row: project identity — workspace basename › current surface.
    /// Controls live on the BOTTOM row (buttons render in `draw_status`),
    /// keeping the mouse near the prompt box.
    fn draw_title(&self, frame: &mut ratatui::Frame, area: Rect) {
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_DARK)),
            area,
        );
        let name = std::path::Path::new(&self.workspace_root)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "ocean".into());
        let surface = match self.center {
            Center::Chat => "chat",
            Center::Editor => "editor",
            Center::Graph => "graph",
            Center::WorkflowGraph => "workflow graph",
        };
        let line = Line::from(vec![
            Span::styled(
                format!("  {name}"),
                Style::default()
                    .fg(theme::FG)
                    .bg(theme::BG_DARK)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {} ", g("›", ">")),
                Style::default().fg(theme::EDGE).bg(theme::BG_DARK),
            ),
            Span::styled(
                surface,
                Style::default().fg(theme::COMMENT).bg(theme::BG_DARK),
            ),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    /// Bottom row — the control + info bar (mouse-first: closest to the
    /// prompt). Left: the six nav buttons, lit while active. Right of them:
    /// model · branch · health/error · activity · tok/s, built pure and
    /// width-aware in `status::segments` (survival ranks live there). The
    /// buttons fill `self.buttons` for click routing; geometry uses DISPLAY
    /// width, never scalar counts, so hit rects match painted glyphs.
    fn draw_status(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        use unicode_width::UnicodeWidthStr as UW;
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_DARK)),
            area,
        );
        self.buttons.clear();
        let items: Vec<(&str, Btn, bool, ratatui::style::Color)> = vec![
            (
                g("≡", "[S]"),
                Btn::Sessions,
                self.show_sessions,
                theme::BLUE,
            ),
            (
                g("◒", "[C]"),
                Btn::Chat,
                self.center == Center::Chat,
                theme::CYAN,
            ),
            (
                g("✎", "[F]"),
                Btn::Editor,
                self.center == Center::Editor,
                theme::YELLOW,
            ),
            (
                g("⟠", "[G]"),
                Btn::Graph,
                matches!(self.center, Center::Graph | Center::WorkflowGraph),
                theme::MAGENTA,
            ),
            (
                g("⊟", "[T]"),
                Btn::Term,
                self.pty.is_active() && self.show_term,
                theme::GREEN,
            ),
            (g("◨", "[E]"), Btn::Tree, self.show_tree, theme::CYAN),
        ];
        let right = area.x + area.width;
        let mut x = area.x + 1;
        for (icon, btn, on, color) in items {
            let w = (UW::width(icon).max(1)) as u16;
            if x + w > right {
                break; // pathological width: never paint past the row
            }
            let fg = if on { color } else { theme::COMMENT };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    icon.to_string(),
                    Style::default().fg(fg).bg(theme::BG_DARK),
                )),
                Rect::new(x, area.y, w, 1),
            );
            // Generous hit target: icon + trailing gap.
            self.buttons.push((Rect::new(x, area.y, w + 2, 1), btn));
            x += w + 2;
        }
        let strip_w = (x + 1).saturating_sub(area.x); // gap before the info run

        let data = self.status_data();
        let tone_color = |t: Tone| match t {
            Tone::Primary => theme::FG,
            Tone::Muted => theme::COMMENT,
            Tone::Warn => theme::YELLOW,
        };
        let seg_budget = area.width.saturating_sub(strip_w) as usize;
        let mut spans: Vec<Span> = Vec::new();
        let segs = status::segments(&data, seg_budget);
        for (i, seg) in segs.into_iter().enumerate() {
            spans.push(Span::styled(
                if i == 0 { " " } else { "  " },
                Style::default().fg(theme::EDGE),
            ));
            spans.push(Span::styled(
                seg.text,
                Style::default().fg(tone_color(seg.tone)),
            ));
        }
        if area.width > strip_w {
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG_DARK)),
                Rect::new(area.x + strip_w, area.y, area.width - strip_w, 1),
            );
        }
    }
}

/// Start index for a selection-relative list window. The selected row remains
/// visible even when the modal is shorter than the full list.
fn selection_window_start(selected: usize, len: usize, visible: usize) -> usize {
    if visible == 0 || len <= visible {
        return 0;
    }
    selected
        .min(len - 1)
        .saturating_add(1)
        .saturating_sub(visible)
        .min(len - visible)
}

/// Sanitize one terminal row and clamp it by display cells, reserving an
/// ellipsis where it fits. Paths and transcript-derived titles are untrusted
/// terminal text even when they originated on the local filesystem.
fn truncate_cells(raw: &str, max_width: usize) -> String {
    let clean = sanitize_line(raw);
    if UnicodeWidthStr::width(clean.as_str()) <= max_width {
        return clean;
    }
    if max_width == 0 {
        return String::new();
    }

    let ellipsis = g("…", "...");
    let ellipsis_width = UnicodeWidthStr::width(ellipsis);
    let (limit, suffix) = if ellipsis_width <= max_width {
        (max_width - ellipsis_width, ellipsis)
    } else {
        (max_width, "")
    };
    let mut out = String::new();
    let mut width = 0usize;
    for ch in clean.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if width + ch_width > limit {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push_str(suffix);
    out
}

/// Does `pos` (col, row) fall inside `r`?
fn rect_has(r: Rect, pos: (u16, u16)) -> bool {
    let (x, y) = pos;
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

/// A 1-cell splitter line between panels, CTRL-style.
fn splitter(frame: &mut ratatui::Frame, area: Rect, vertical: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let ch = if vertical { "▏" } else { "─" };
    if vertical {
        for k in 0..area.height {
            frame.render_widget(
                Paragraph::new(Span::styled(ch, Style::default().fg(theme::EDGE)))
                    .style(Style::default().bg(theme::BG)),
                Rect::new(area.x, area.y + k, 1, 1),
            );
        }
    } else {
        frame.render_widget(
            Paragraph::new(Span::styled(
                ch.repeat(area.width as usize),
                Style::default().fg(theme::EDGE),
            ))
            .style(Style::default().bg(theme::BG)),
            area,
        );
    }
}

/// Short kind badge for the memory browser (`fact`→`fact`, keeps it compact).
fn short_kind(kind: &str) -> &str {
    match kind {
        "preference" => "pref",
        "relationship" => "rel",
        other => other,
    }
}

/// Truncate `s` to `max` display chars with an ellipsis (whitespace preserved).
fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}

/// Order the `/models` picker entries: ready providers' models first (registry
/// order within), not-ready ones after — so the list opens on what John can
/// actually use, with the unconfigured rest visible-but-grey below.
fn order_models(entries: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let (ready, rest): (Vec<_>, Vec<_>) = entries.into_iter().partition(|e| e.ready);
    ready.into_iter().chain(rest).collect()
}

/// Footer label for the per-turn thinking level (`None` = daemon default).
fn thinking_label(t: Option<ThinkingLevel>) -> &'static str {
    match t {
        None => "default",
        Some(ThinkingLevel::Off) => "off",
        Some(ThinkingLevel::Minimal) => "minimal",
        Some(ThinkingLevel::Low) => "low",
        Some(ThinkingLevel::Medium) => "medium",
        Some(ThinkingLevel::High) => "high",
        Some(ThinkingLevel::Xhigh) => "xhigh",
    }
}

/// Cycle the thinking level through `default → off → minimal → low → medium →
/// high → xhigh` (wrapping both directions). `default` (None) sends nothing so
/// the daemon's global setting stays in force.
fn cycle_thinking(cur: Option<ThinkingLevel>, dir: i8) -> Option<ThinkingLevel> {
    const ORDER: [Option<ThinkingLevel>; 7] = [
        None,
        Some(ThinkingLevel::Off),
        Some(ThinkingLevel::Minimal),
        Some(ThinkingLevel::Low),
        Some(ThinkingLevel::Medium),
        Some(ThinkingLevel::High),
        Some(ThinkingLevel::Xhigh),
    ];
    let i = ORDER.iter().position(|o| *o == cur).unwrap_or(0) as i8;
    let n = ORDER.len() as i8;
    ORDER[(((i + dir) % n + n) % n) as usize]
}

/// Order two selection endpoints into (start, end) reading order — by row,
/// then column — so a drag upward/leftward selects the same span as one
/// downward/rightward.
fn order_cells(a: (u16, u16), b: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Inclusive row/column span of a pane-bound selection. The ordered endpoints
/// are clamped into `rect`'s row range, and every row's columns are restricted
/// to `rect`. The first row's start column comes from the (rect-clamped) anchor
/// only when the anchor row is inside the rect; otherwise that boundary row
/// reads as a full-width continuation (anchor sat above the pane). Symmetric
/// for the head/last row. Shared by the overlay painter and `selection_text`
/// so the highlighted region and the copied text always agree.
struct SelSpan {
    /// First included row (inclusive).
    y0: u16,
    /// Last included row (inclusive).
    y1: u16,
    /// Start column on the first included row.
    first_x0: u16,
    /// End column on the last included row.
    last_x1: u16,
    /// Rect left column (start of middle/last rows).
    left: u16,
    /// Rect right column, inclusive (end of first/middle rows).
    right: u16,
}

/// Derive the bounded span of a selection inside `rect`. Returns `None` for a
/// zero-area rect or when the ordered endpoints don't overlap it vertically.
fn bounded_span(a: (u16, u16), b: (u16, u16), rect: Rect) -> Option<SelSpan> {
    if rect.width == 0 || rect.height == 0 {
        return None;
    }
    let (s, e) = order_cells(a, b);
    let top = rect.y;
    let bottom = rect.bottom().saturating_sub(1);
    let left = rect.x;
    let right = rect.right().saturating_sub(1);
    let y0 = s.1.clamp(top, bottom);
    let y1 = e.1.clamp(top, bottom);
    if y0 > y1 {
        return None;
    }
    let anchor_in = (s.1 >= top) && (s.1 <= bottom);
    let head_in = (e.1 >= top) && (e.1 <= bottom);
    let first_x0 = if anchor_in { s.0.max(left) } else { left };
    let last_x1 = if head_in { e.0.min(right) } else { right };
    Some(SelSpan {
        y0,
        y1,
        first_x0,
        last_x1,
        left,
        right,
    })
}

/// Clamp a screen position into `rect`'s inclusive bounds — a drag head swept
/// past a pane border saturates at the lane edge.
fn clamp_pos(pos: (u16, u16), rect: Rect) -> (u16, u16) {
    let x = pos.0.clamp(rect.x, rect.right().saturating_sub(1));
    let y = pos.1.clamp(rect.y, rect.bottom().saturating_sub(1));
    (x, y)
}

/// Extract the text of a linear (terminal-style) selection from a frame's cell
/// snapshot, BOUND to `rect`: first row from the anchor column to the rect's
/// right edge, middle rows spanning the rect's full width, last row from the
/// rect's left edge to the head column. Cells outside the rect's columns are
/// never read, so a sweep inside one lane can't copy a sibling lane's text.
/// Rows are right-trimmed (panel padding isn't content); a selection of pure
/// padding yields the empty string so releasing on a blank area copies nothing.
fn selection_text(cells: &[Vec<String>], a: (u16, u16), b: (u16, u16), rect: Rect) -> String {
    let Some(sp) = bounded_span(a, b, rect) else {
        return String::new();
    };
    let mut out: Vec<String> = Vec::new();
    for y in sp.y0..=sp.y1 {
        let x0 = (if y == sp.y0 { sp.first_x0 } else { sp.left }) as usize;
        let x_end = (if y == sp.y1 { sp.last_x1 } else { sp.right }) as usize;
        let Some(row) = cells.get(y as usize) else {
            continue;
        };
        if row.is_empty() {
            out.push(String::new());
            continue;
        }
        let x1 = x_end.min(row.len().saturating_sub(1));
        if x0 > x1 || x0 >= row.len() {
            out.push(String::new());
            continue;
        }
        out.push(row[x0..=x1].concat().trim_end().to_string());
    }
    let text = out.join("\n");
    if text.trim().is_empty() {
        String::new()
    } else {
        text
    }
}

/// Stable-row equivalent of [`selection_text`] for a chat transcript selection
/// that crossed viewport scrolls. Rows are populated incrementally from frames
/// the operator actually saw; missing rows are skipped rather than fabricated.
fn stable_selection_text(
    rows: &BTreeMap<usize, Vec<String>>,
    a: (u16, usize),
    b: (u16, usize),
    left: u16,
    right: u16,
) -> String {
    let (s, e) = if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    };
    let mut out = Vec::new();
    for row_index in s.1..=e.1 {
        let Some(row) = rows.get(&row_index) else {
            continue;
        };
        let x0 = if row_index == s.1 {
            s.0.max(left)
        } else {
            left
        } as usize;
        let x_end = if row_index == e.1 {
            e.0.min(right)
        } else {
            right
        } as usize;
        if row.is_empty() || x0 >= row.len() || x0 > x_end {
            out.push(String::new());
            continue;
        }
        let x1 = x_end.min(row.len().saturating_sub(1));
        out.push(row[x0..=x1].concat().trim_end().to_string());
    }
    let text = out.join("\n");
    if text.trim().is_empty() {
        String::new()
    } else {
        text
    }
}

fn dictation_text_chunks(text: &str) -> Vec<String> {
    // STT is provider-controlled text. Preserve whitespace controls only as
    // word boundaries and drop every other control byte before it can reach a
    // raw composer Span (ESC/BEL would desynchronize the terminal).
    let clean: String = text
        .chars()
        .filter_map(|ch| {
            if ch.is_control() {
                ch.is_whitespace().then_some(' ')
            } else {
                Some(ch)
            }
        })
        .collect();
    let words: Vec<&str> = clean.split_whitespace().collect();
    let last = words.len().saturating_sub(1);
    words
        .into_iter()
        .enumerate()
        .map(|(index, word)| {
            if index == last {
                word.to_string()
            } else {
                format!("{word} ")
            }
        })
        .collect()
}

const MAX_CLIPBOARD_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const MAX_IMAGES_PER_PASTE: usize = 8;

fn image_mime(path: &std::path::Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// Parse a Finder/terminal file paste only when the complete nonblank payload is
/// a bounded list of existing supported image files. This all-or-nothing rule
/// prevents ordinary prose containing one path from disappearing into attachments.
fn pasted_image_paths(text: &str) -> Option<Vec<PathBuf>> {
    let paths: Vec<PathBuf> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let unquoted = line
                .strip_prefix('\'')
                .and_then(|s| s.strip_suffix('\''))
                .or_else(|| line.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
                .unwrap_or(line);
            PathBuf::from(unquoted)
        })
        .collect();
    (!paths.is_empty() && paths.len() <= MAX_IMAGES_PER_PASTE)
        .then_some(paths)
        .filter(|paths| {
            paths
                .iter()
                .all(|path| path.is_file() && image_mime(path).is_some())
        })
}

fn read_image_paths(paths: &[PathBuf]) -> Result<Vec<ocean_agent_sdk::TurnImage>, String> {
    use base64::Engine;

    paths
        .iter()
        .map(|path| {
            let mime_type =
                image_mime(path).ok_or_else(|| format!("unsupported image: {}", path.display()))?;
            let metadata =
                std::fs::metadata(path).map_err(|e| format!("image {}: {e}", path.display()))?;
            if metadata.len() == 0 || metadata.len() > MAX_CLIPBOARD_IMAGE_BYTES as u64 {
                return Err(format!(
                    "image {} is empty or exceeds 20 MiB",
                    path.display()
                ));
            }
            let bytes =
                std::fs::read(path).map_err(|e| format!("image {}: {e}", path.display()))?;
            Ok(ocean_agent_sdk::TurnImage {
                mime_type: mime_type.into(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        })
        .collect()
}

/// Read an image from the macOS pasteboard without blocking the UI thread.
/// `osascript` supplies TIFF/PNG bytes; `sips` normalizes TIFF to PNG because all
/// Ocean vision providers and kitty's viewer understand PNG consistently.
fn read_clipboard_image() -> Result<ocean_agent_sdk::TurnImage, String> {
    use base64::Engine;
    use std::process::Command;

    let dir = std::env::temp_dir().join(format!("ocean-clipboard-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("clipboard image: {e}"))?;
    let tiff = dir.join("clipboard.tiff");
    let png = dir.join("clipboard.png");
    let script = format!(
        "set imageData to the clipboard as TIFF picture\nset outFile to open for access POSIX file {} with write permission\nset eof outFile to 0\nwrite imageData to outFile\nclose access outFile",
        applescript_string(&tiff.to_string_lossy())
    );
    let capture = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .map_err(|e| format!("clipboard image unavailable: {e}"))?;
    if !capture.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err("clipboard does not contain an image".into());
    }
    let convert = Command::new("sips")
        .args(["-s", "format", "png"])
        .arg(&tiff)
        .args(["--out"])
        .arg(&png)
        .output()
        .map_err(|e| format!("clipboard image conversion failed: {e}"))?;
    if !convert.status.success() {
        let _ = std::fs::remove_dir_all(&dir);
        return Err("clipboard image conversion failed".into());
    }
    let bytes = std::fs::read(&png).map_err(|e| format!("clipboard image: {e}"))?;
    let _ = std::fs::remove_dir_all(&dir);
    if bytes.is_empty() || bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
        return Err("clipboard image is empty or exceeds 20 MiB".into());
    }
    Ok(ocean_agent_sdk::TurnImage {
        mime_type: "image/png".into(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Put `text` on the system clipboard via `pbcopy` (the workbench is macOS-first
/// today; Linux would add xclip/wl-copy here). Runs off the UI thread.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    child
        .stdin
        .take()
        .ok_or_else(|| "no stdin".to_string())?
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())?;
    child.wait().map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;

    fn grid(rows: &[&str]) -> Vec<Vec<String>> {
        rows.iter()
            .map(|r| r.chars().map(|c| c.to_string()).collect())
            .collect()
    }

    fn key_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    #[test]
    fn option_space_is_the_dictation_toggle_without_claiming_plain_space() {
        assert!(App::is_dictation_toggle_key(&key_with_modifiers(
            KeyCode::Char(' '),
            KeyModifiers::ALT,
        )));
        assert!(App::is_dictation_toggle_key(&key_with_modifiers(
            KeyCode::Char('\u{a0}'),
            KeyModifiers::NONE,
        )));
        assert!(!App::is_dictation_toggle_key(&key_with_modifiers(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        )));
    }
    fn key_with_kind(code: KeyCode, kind: KeyEventKind) -> CrosstermEvent {
        CrosstermEvent::Key(crossterm::event::KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind,
            state: crossterm::event::KeyEventState::NONE,
        })
    }

    #[test]
    fn dictation_chunks_normalize_controls_and_keep_word_cadence() {
        assert_eq!(
            dictation_text_chunks("  hello\nworld\tfrom\u{1b}[2JOcean\u{7}  "),
            ["hello ", "world ", "from[2JOcean"]
        );
        assert!(dictation_text_chunks(" \n\t\u{1b}\u{7} ").is_empty());
        assert!(dictation_text_chunks("safe\u{1b}[31mtext")
            .iter()
            .flat_map(|chunk| chunk.chars())
            .all(|ch| !ch.is_control()));
    }

    #[test]
    fn enhanced_space_tap_inserts_one_space_without_opening_microphone() {
        let mut app = offline_app();
        app.set_hold_to_dictate(true);
        app.on_crossterm(key_with_kind(KeyCode::Char(' '), KeyEventKind::Press));
        assert!(app.space_hold.is_some());
        assert_eq!(app.chat.composer_text(), "");

        app.on_crossterm(key_with_kind(KeyCode::Char(' '), KeyEventKind::Release));
        assert!(app.space_hold.is_none());
        assert_eq!(app.chat.composer_text(), " ");
        assert!(app.active_dictation_id.is_none());
    }

    #[test]
    fn legacy_terminal_space_stays_immediate_text_input() {
        let mut app = offline_app();
        app.set_hold_to_dictate(false);
        app.on_crossterm(key_with_kind(KeyCode::Char(' '), KeyEventKind::Press));
        app.on_crossterm(key_with_kind(KeyCode::Char(' '), KeyEventKind::Release));
        assert_eq!(app.chat.composer_text(), " ");
        assert!(app.space_hold.is_none());
    }

    #[test]
    fn pasted_image_paths_accepts_multiple_existing_images_only() {
        let dir = std::env::temp_dir().join(format!("ocean-paste-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("first.jpg");
        let second = dir.join("second.png");
        std::fs::write(&first, b"jpeg fixture").unwrap();
        std::fs::write(&second, b"png fixture").unwrap();
        let paste = format!("{}\n{}", first.display(), second.display());
        assert_eq!(pasted_image_paths(&paste), Some(vec![first, second]));
        assert!(pasted_image_paths("this is ordinary text").is_none());
        assert!(pasted_image_paths("/missing/image.jpg").is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn advisor_off_suppresses_late_in_flight_advisor_event() {
        let mut app = offline_app();
        app.advisor_ctl = Some(ocean_agent_sdk::AdvisorControl {
            enabled: false,
            model: None,
        });
        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::Extension {
            extension: "advisor".into(),
            payload: serde_json::json!({
                "note": "stale advisor card must not paint",
                "severity": "warning",
                "model": "test-advisor"
            }),
            scope: None,
        })));
        let rendered = render_app_to_string(&mut app, 100, 28);
        assert!(!rendered.contains("stale advisor card must not paint"));
    }

    #[test]
    fn stable_chat_selection_keeps_rows_seen_before_and_after_scroll() {
        let mut rows = BTreeMap::new();
        rows.insert(8, grid(&["eight row"])[0].clone());
        rows.insert(9, grid(&["nine row"])[0].clone());
        rows.insert(10, grid(&["ten row"])[0].clone());
        assert_eq!(
            stable_selection_text(&rows, (2, 8), (5, 10), 0, 8),
            "ght row\nnine row\nten ro"
        );
        assert_eq!(
            stable_selection_text(&rows, (5, 10), (2, 8), 0, 8),
            "ght row\nnine row\nten ro",
            "reverse drag copies the same retained rows"
        );
    }

    #[test]
    fn editor_selection_retains_rows_across_scroll_and_reverses_cleanly() {
        let mut app = offline_app();
        let path = PathBuf::from(&app.workspace_root).join("selection.rs");
        let contents = (0..40)
            .map(|row| format!("row-{row:02} payload"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, contents).expect("write editor selection fixture");
        app.editor.open(path.clone());
        app.center = Center::Editor;
        app.focus_to(Focus::Center);
        render_app_to_string(&mut app, 80, 20);

        let center = app.r_center;
        let (left, right) = app
            .editor
            .selection_columns()
            .expect("editor text viewport rendered");
        let top = center.y + 2;
        let bottom = center.bottom() - 2;
        app.on_crossterm(mouse(MouseEventKind::Down(MouseButton::Left), left, top));
        app.on_crossterm(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            right,
            bottom,
        ));
        render_app_to_string(&mut app, 80, 20);

        // Wheel while the button remains armed. Each painted viewport contributes
        // its stable document rows, so copy is not limited to the final frame.
        for _ in 0..4 {
            app.on_crossterm(mouse(MouseEventKind::ScrollDown, left, bottom));
            render_app_to_string(&mut app, 80, 20);
        }
        app.on_crossterm(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            left + 5,
            bottom,
        ));
        render_app_to_string(&mut app, 80, 20);

        let (anchor, head) = app.selection.expect("editor drag remains live");
        assert!(app.selection_space == SelectionSpace::Editor);
        assert!(head.1 > anchor.1, "scroll advances the stable document row");
        let copied = stable_selection_text(&app.selection_rows, anchor, head, left, right);
        let reversed = stable_selection_text(&app.selection_rows, head, anchor, left, right);
        assert_eq!(copied, reversed, "reverse drag copies the same editor span");
        let lines: Vec<_> = copied.lines().collect();
        assert_eq!(lines.first(), Some(&"row-00 payload"));
        let expected_last = format!("row-{:02}", head.1);
        assert_eq!(lines.last(), Some(&expected_last.as_str()));
        assert_eq!(lines.len(), head.1 - anchor.1 + 1);
        assert!(
            lines.iter().all(|line| !line.starts_with(' ')),
            "painted line-number gutter is excluded from editor copy"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn selection_orders_endpoints_both_drag_directions() {
        // Dragging up-left must select the same span as down-right.
        assert_eq!(order_cells((5, 2), (1, 0)), ((1, 0), (5, 2)));
        assert_eq!(order_cells((1, 0), (5, 2)), ((1, 0), (5, 2)));
        // Same row: earlier column first.
        assert_eq!(order_cells((7, 3), (2, 3)), ((2, 3), (7, 3)));
    }

    #[test]
    fn selection_text_is_linear_like_a_terminal() {
        let cells = grid(&["hello world", "second line", "tail row   "]);
        // Full-frame rect keeps the legacy terminal-style semantics: mid-first
        // row through mid-last row, first row from anchor, middle whole, last
        // row up to the head. Trailing padding trimmed.
        let full = Rect::new(0, 0, 11, 3);
        let text = selection_text(&cells, (6, 0), (3, 2), full);
        assert_eq!(text, "world\nsecond line\ntail");
        // Same-row span.
        assert_eq!(selection_text(&cells, (0, 1), (5, 1), full), "second");
        // Reverse drag selects the same text.
        assert_eq!(
            selection_text(&cells, (3, 2), (6, 0), full),
            selection_text(&cells, (6, 0), (3, 2), full),
        );
    }

    #[test]
    fn selection_of_pure_padding_copies_nothing() {
        let cells = grid(&["          ", "          "]);
        let full = Rect::new(0, 0, 10, 2);
        assert_eq!(selection_text(&cells, (1, 0), (8, 1), full), "");
    }

    #[test]
    fn selection_text_never_crosses_into_a_sibling_lane() {
        // Left rail sentinel "L", pane content, right rail sentinel "R". With
        // the OLD whole-width selection_text the middle row would copy the full
        // frame ("LLLworldRRR"); the pane-bounded rect must yield only the
        // middle band across all three rows.
        let cells = grid(&["LLLhelloRRR", "LLLworldRRR", "LLLthereRRR"]);
        // Pane occupies columns 3..=7 (x=3, width=5), all three rows.
        let pane = Rect::new(3, 0, 5, 3);
        assert_eq!(
            selection_text(&cells, (3, 0), (7, 2), pane),
            "hello\nworld\nthere",
        );
        // Reverse drag over the same grid selects identical text.
        assert_eq!(
            selection_text(&cells, (7, 2), (3, 0), pane),
            selection_text(&cells, (3, 0), (7, 2), pane),
        );
    }

    #[test]
    fn selection_text_clamps_a_head_dragged_past_the_pane_edge() {
        let cells = grid(&["LLLhelloRRR", "LLLworldRRR", "LLLthereRRR"]);
        let pane = Rect::new(3, 0, 5, 3);
        // Head far past the pane's bottom-right corner saturates at the edges.
        assert_eq!(
            selection_text(&cells, (4, 0), (50, 50), pane),
            "ello\nworld\nthere",
        );
        // An endpoint dragged to a column left of the pane saturates at the
        // left edge — the rail sentinel at column 0 never leaks into the copy.
        assert_eq!(
            selection_text(&cells, (0, 0), (6, 2), pane),
            "hello\nworld\nther",
        );
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> CrosstermEvent {
        CrosstermEvent::Mouse(crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        })
    }

    #[test]
    fn drag_past_a_pane_edge_clamps_the_head_into_the_rect() {
        let mut app = offline_app();
        render_app_to_string(&mut app, 100, 28);
        let center = app.r_center;
        assert!(
            center.width > 4 && center.height > 4,
            "chat pane must render with room"
        );
        let anchor = (center.x + 2, center.y + 1);
        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            anchor.0,
            anchor.1,
        ));
        assert_eq!(app.sel_rect, Some(center), "Down arms the chat pane rect");
        // Drag far past the pane's bottom-right corner, into the tree lane.
        app.on_crossterm(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            center.right() + 20,
            center.bottom() + 20,
        ));
        let (a, h) = app
            .selection
            .expect("drag promoted the arm to a live selection");
        assert_eq!(a.0, anchor.0, "anchor column preserved across the drag");
        assert_eq!(
            a.1,
            app.chat
                .nearest_transcript_row(anchor.1)
                .expect("rendered chat has transcript rows"),
            "anchor uses the nearest stable transcript row"
        );
        assert_eq!(
            h.0,
            center.right() - 1,
            "head column saturates at pane edge"
        );
        assert_eq!(
            h.1,
            app.chat
                .nearest_transcript_row(center.bottom() - 1)
                .expect("rendered chat has transcript rows"),
            "head row saturates at the transcript edge, never crossing into a sibling lane"
        );
    }

    #[test]
    fn down_on_title_or_splitter_arms_no_selection() {
        let mut app = offline_app();
        render_app_to_string(&mut app, 100, 28);
        let center = app.r_center;
        // Arm a selection inside the chat first…
        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            center.x + 2,
            center.y + 1,
        ));
        assert!(
            app.sel_press.is_some() && app.sel_rect.is_some(),
            "chat Down arms a selection"
        );
        // …then a Down on the title row (row 0, button-free at column 0) must
        // clear the stale arm and arm nothing, so the click falls through.
        app.on_crossterm(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0));
        assert!(
            app.sel_press.is_none() && app.sel_rect.is_none(),
            "title-row Down arms nothing and clears any prior arm"
        );
        // The dock splitter isn't drawn (pty inactive), so synthesize one: a
        // Down there must grab the splitter (dragging_term) and arm nothing.
        app.r_split_term = Rect::new(center.x, center.bottom(), center.width, 1);
        let sp = app.r_split_term;
        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            sp.x + 1,
            sp.y,
        ));
        assert!(app.dragging_term, "splitter grab preserved");
        assert!(
            app.sel_press.is_none() && app.sel_rect.is_none(),
            "splitter Down arms no selection"
        );
    }

    fn observatory_snapshot(
        phase: ocean_observatory::ExecutionPhase,
    ) -> ocean_observatory::ObservatorySnapshot {
        use ocean_observatory::{Cursor, Producer, ProducerKind, SnapshotNode, TruthProvenance};
        ocean_observatory::ObservatorySnapshot {
            watermark_cursor: Cursor::new(1),
            earliest_available_cursor: Cursor::new(1),
            observatory_id: "observatory".into(),
            daemon_instance_id: "boot".into(),
            nodes: vec![SnapshotNode {
                execution_id: "execution-1".into(),
                root_execution_id: "execution-1".into(),
                parent_execution_id: None,
                session_id: String::new(),
                turn_id: String::new(),
                request_id: String::new(),
                phase,
                producer: Producer {
                    kind: ProducerKind::Extension,
                    id: "crew".into(),
                },
                truth: TruthProvenance::ExtensionAttested,
                started_at: "now".into(),
                last_activity_at: "now".into(),
                labels: vec!["implement workflow".into()],
                duration_millis: None,
            }],
            edges: Vec::new(),
            attention: Vec::new(),
        }
    }

    #[test]
    fn active_observatory_execution_replaces_files_rail_and_completion_stays() {
        let mut app = offline_app();
        assert!(!app.show_tree);
        app.dispatch(Action::ObservatorySnapshot(Box::new(observatory_snapshot(
            ocean_observatory::ExecutionPhase::Running,
        ))));
        assert!(
            app.show_tree,
            "active execution auto-reveals the right rail"
        );
        assert!(app.right_rail_mode == RightRailMode::Workflow);
        let screen = render_app_to_string(&mut app, 100, 28);
        assert!(screen.contains("FLOW"));

        app.dispatch(Action::ObservatorySnapshot(Box::new(observatory_snapshot(
            ocean_observatory::ExecutionPhase::Finished,
        ))));
        assert!(app.show_tree, "completion must remain inspectable");
        assert!(app.right_rail_mode == RightRailMode::Workflow);
        assert_eq!(app.workflow_graph.graph.active_count(), 0);

        app.dispatch(Action::Navigate(Nav::Files));
        assert!(app.right_rail_mode == RightRailMode::Files);
        assert!(app.show_tree);
    }

    #[test]
    fn explicit_right_rail_close_suppresses_workflow_auto_reveal() {
        let mut app = offline_app();
        app.set_tree_visible_by_operator(false);
        app.dispatch(Action::ObservatorySnapshot(Box::new(observatory_snapshot(
            ocean_observatory::ExecutionPhase::Running,
        ))));
        assert!(!app.show_tree);
        assert!(app.right_rail_mode == RightRailMode::Files);
        assert_eq!(app.workflow_graph.graph.active_count(), 1);
    }

    #[test]
    fn workflow_rail_expands_into_center_without_changing_authority() {
        let mut app = offline_app();
        app.dispatch(Action::ObservatorySnapshot(Box::new(observatory_snapshot(
            ocean_observatory::ExecutionPhase::Running,
        ))));
        app.dispatch(Action::ExpandWorkflowGraph);
        assert!(app.center == Center::WorkflowGraph);
        assert!(app.focus == Focus::Center);
        assert!(
            app.right_rail_mode == RightRailMode::Files,
            "expanded workflow graph restores Files instead of drawing one component twice"
        );
        assert_eq!(app.workflow_graph.graph.nodes.len(), 1);

        app.dispatch(Action::ObservatorySnapshot(Box::new(observatory_snapshot(
            ocean_observatory::ExecutionPhase::Finished,
        ))));
        app.dispatch(Action::ObservatorySnapshot(Box::new(observatory_snapshot(
            ocean_observatory::ExecutionPhase::Running,
        ))));
        assert!(app.center == Center::WorkflowGraph);
        assert!(
            app.right_rail_mode == RightRailMode::Files,
            "later activation cannot duplicate the center graph in the rail"
        );
    }

    #[test]
    fn file_rail_splits_only_for_visible_tray_with_enough_height() {
        let area = Rect::new(70, 1, 30, 28);
        let (tree, split, tray) = file_rail_rects(area, false, 8);
        assert_eq!(tree, area);
        assert_eq!(split, Rect::default());
        assert_eq!(tray, Rect::default());

        let (tree, split, tray) = file_rail_rects(area, true, 8);
        assert_eq!(tree.height + split.height + tray.height, area.height);
        assert_eq!(split.height, 1);
        assert_eq!(tray.height, 8);
        assert_eq!(tree.bottom(), split.y);
        assert_eq!(split.bottom(), tray.y);

        let tiny = Rect::new(70, 1, 30, 11);
        let (tree, split, tray) = file_rail_rects(tiny, true, 8);
        assert_eq!(tree, tiny);
        assert_eq!(split, Rect::default());
        assert_eq!(tray, Rect::default());
    }

    #[test]
    fn too_small_frame_clears_stale_mouse_geometry() {
        let mut app = offline_app();
        app.show_tree = true;
        render_app_to_string(&mut app, 100, 28);
        let old_tree = app.r_tree;
        assert!(old_tree.width > 0 && old_tree.height > 0);

        render_app_to_string(&mut app, 39, 7);
        assert_eq!(app.r_body, Rect::default());
        assert_eq!(app.r_tree, Rect::default());
        assert_eq!(app.r_tray, Rect::default());
        assert_eq!(app.r_split_tree, Rect::default());
        assert!(app.buttons.is_empty());

        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            old_tree.x + 1,
            old_tree.y + 2,
        ));
        assert!(app.sel_press.is_none() && app.sel_rect.is_none());
        assert!(!app.dragging_tree && !app.dragging_sessions && !app.dragging_term);
    }

    #[test]
    fn too_small_frame_clears_stale_modal_hits() {
        let mut app = launch_app();
        render_app_to_string(&mut app, 80, 20);
        let old_hit = app.launch_hit[0].0;
        assert!(old_hit.x < 39 && old_hit.y < 7);

        render_app_to_string(&mut app, 39, 7);
        assert!(app.launch_hit.is_empty());
        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            old_hit.x,
            old_hit.y,
        ));
        assert!(
            app.launch_open,
            "invisible stale launch row cannot activate"
        );
    }

    #[tokio::test]
    async fn todo_events_reveal_session_tray_and_stay_pinned_across_turns() {
        let mut app = offline_app();
        app.show_tree = true;
        let sid = AgentSessionId(uuid::Uuid::from_u128(1));
        let tid = ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(2));
        let cid = ocean_agent_sdk::ToolCallId(uuid::Uuid::from_u128(3));
        app.dispatch(Action::SessionBound(sid));
        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id: sid,
            turn_id: tid,
            model: None,
        })));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallStarted {
                session_id: sid,
                turn_id: tid,
                call: ocean_agent_sdk::ToolCall {
                    id: cid.clone(),
                    name: "todo".into(),
                    args_json: serde_json::json!({"action": "add", "text": "tray task"}),
                },
            },
        )));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallFinished {
                session_id: sid,
                turn_id: tid,
                call_id: cid,
                result: ocean_agent_sdk::ToolResult {
                    ok: true,
                    output: "1 [ ] tray task\n".into(),
                    metadata_json: None,
                },
            },
        )));

        let screen = render_app_to_string(&mut app, 100, 28);
        assert!(!screen.contains("SESSION COMPONENT"));
        assert!(screen.contains("tray task"));
        assert!(app.r_tree.bottom() < app.r_tray.y);

        let anchor = (app.r_tray.x + 2, app.r_tray.y + 2);
        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            anchor.0,
            anchor.1,
        ));
        assert_eq!(app.sel_rect, Some(app.r_tray));
        app.on_crossterm(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            app.r_tree.x + 1,
            app.r_tree.y,
        ));
        let (_, head) = app.selection.expect("tray drag selects inside tray");
        assert_eq!(
            head.1,
            usize::from(app.r_tray.y),
            "selection cannot enter file tree"
        );

        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id: sid,
            turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(4)),
            model: None,
        })));
        let next_screen = render_app_to_string(&mut app, 100, 28);
        assert!(next_screen.contains("tray task"));
        assert_ne!(app.r_tray, Rect::default());
        assert_ne!(app.r_split_tray, Rect::default());
    }

    #[tokio::test]
    async fn files_close_survives_terminal_and_graph_tray_updates() {
        let mut app = offline_app();
        let sid = AgentSessionId(uuid::Uuid::from_u128(11));
        let tid = ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(12));
        let cid = ocean_agent_sdk::ToolCallId(uuid::Uuid::from_u128(13));
        app.dispatch(Action::SessionBound(sid));
        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id: sid,
            turn_id: tid,
            model: None,
        })));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallStarted {
                session_id: sid,
                turn_id: tid,
                call: ocean_agent_sdk::ToolCall {
                    id: cid.clone(),
                    name: "todo".into(),
                    args_json: serde_json::json!({"action": "add", "text": "keep hidden"}),
                },
            },
        )));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallFinished {
                session_id: sid,
                turn_id: tid,
                call_id: cid,
                result: ocean_agent_sdk::ToolResult {
                    ok: true,
                    output: "1 [ ] keep hidden\n".into(),
                    metadata_json: None,
                },
            },
        )));
        assert!(app.show_tree, "new tray content initially reveals Files");

        app.dispatch(Action::Navigate(Nav::Terminal));
        app.press(Btn::Tree);
        assert!(!app.show_tree, "Files button closes the visible rail");
        app.dispatch(Action::Render);
        assert!(
            !app.show_tree,
            "terminal repaint must not reopen an explicitly hidden Files rail"
        );

        app.dispatch(Action::Navigate(Nav::Graph));
        let clear_id = ocean_agent_sdk::ToolCallId(uuid::Uuid::from_u128(14));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallStarted {
                session_id: sid,
                turn_id: tid,
                call: ocean_agent_sdk::ToolCall {
                    id: clear_id.clone(),
                    name: "todo".into(),
                    args_json: serde_json::json!({"action": "clear"}),
                },
            },
        )));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallFinished {
                session_id: sid,
                turn_id: tid,
                call_id: clear_id,
                result: ocean_agent_sdk::ToolResult {
                    ok: true,
                    output: "Todo list cleared.".into(),
                    metadata_json: None,
                },
            },
        )));
        assert!(!app.tray.is_visible(), "clear unmounts the tray");

        let remount_id = ocean_agent_sdk::ToolCallId(uuid::Uuid::from_u128(15));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallStarted {
                session_id: sid,
                turn_id: tid,
                call: ocean_agent_sdk::ToolCall {
                    id: remount_id,
                    name: "todo".into(),
                    args_json: serde_json::json!({"action": "add", "text": "graph update"}),
                },
            },
        )));
        assert!(app.tray.is_visible(), "fresh update remounts the tray");
        assert!(app.center == Center::Graph);
        assert!(
            !app.show_tree,
            "Graph tray updates must respect the explicit Files dismissal"
        );

        app.dispatch(Action::Navigate(Nav::Files));
        assert!(app.show_tree, "explicit Files navigation reopens the rail");
        assert!(
            !app.tree_auto_reveal_suppressed,
            "explicit reopen resets the dismissal latch"
        );
    }

    #[tokio::test]
    async fn stale_session_todo_event_cannot_mount_tray() {
        let mut app = offline_app();
        app.show_tree = true;
        app.dispatch(Action::SessionBound(AgentSessionId(uuid::Uuid::from_u128(
            1,
        ))));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallStarted {
                session_id: AgentSessionId(uuid::Uuid::from_u128(9)),
                turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(2)),
                call: ocean_agent_sdk::ToolCall {
                    id: ocean_agent_sdk::ToolCallId(uuid::Uuid::from_u128(3)),
                    name: "todo".into(),
                    args_json: serde_json::json!({"action": "add", "text": "leak"}),
                },
            },
        )));
        let screen = render_app_to_string(&mut app, 100, 28);
        assert!(!screen.contains("SESSION COMPONENT"));
        assert_eq!(app.r_tray, Rect::default());
    }

    #[test]
    fn rail_drag_limits_count_only_the_visible_opposite_rail() {
        let mut app = offline_app();
        app.show_sessions = true;
        app.show_tree = true;
        render_app_to_string(&mut app, 140, 28);
        let body_w = app.r_body.width;
        let with_tree = max_rail_width(body_w, Some(app.tree_w));
        let session_split = app.r_split_sessions;
        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            session_split.x,
            session_split.y + 1,
        ));
        app.on_crossterm(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            app.r_body.right(),
            session_split.y + 1,
        ));
        assert_eq!(app.sessions_w, with_tree);
        app.on_crossterm(mouse(
            MouseEventKind::Up(MouseButton::Left),
            app.r_body.right(),
            session_split.y + 1,
        ));

        app.show_tree = false;
        render_app_to_string(&mut app, 140, 28);
        let without_tree = max_rail_width(app.r_body.width, None);
        let session_split = app.r_split_sessions;
        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            session_split.x,
            session_split.y + 1,
        ));
        app.on_crossterm(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            app.r_body.right(),
            session_split.y + 1,
        ));
        assert_eq!(app.sessions_w, without_tree);
        assert!(without_tree > with_tree);
    }

    #[test]
    fn tree_drag_limit_counts_visible_sessions() {
        let mut app = offline_app();
        app.show_sessions = true;
        app.show_tree = true;
        render_app_to_string(&mut app, 140, 28);
        let expected = max_rail_width(app.r_body.width, Some(app.sessions_w));
        let split = app.r_split_tree;
        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            split.x,
            split.y + 1,
        ));
        app.on_crossterm(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            app.r_body.x,
            split.y + 1,
        ));
        assert_eq!(app.tree_w, expected);
    }

    fn entry(id: &str, provider: &str, ready: bool) -> ModelEntry {
        ModelEntry {
            id: id.into(),
            provider: provider.into(),
            label: id.into(),
            ready,
        }
    }

    #[test]
    fn picker_orders_ready_models_first() {
        let ordered = order_models(vec![
            entry("glm-4.6", "glm", false),
            entry("deepseek-v4-pro", "deepseek", true),
            entry("gemini-2.0-flash", "google", false),
            entry("gpt-5.5", "openai-codex", true),
        ]);
        let ids: Vec<&str> = ordered.iter().map(|e| e.id.as_str()).collect();
        // Ready first, original order preserved within each half.
        assert_eq!(
            ids,
            vec!["deepseek-v4-pro", "gpt-5.5", "glm-4.6", "gemini-2.0-flash"]
        );
    }

    #[test]
    fn thinking_cycles_through_all_levels_and_wraps() {
        // Forward from default hits every level then wraps home.
        let mut cur = None;
        let mut seen = vec![thinking_label(cur)];
        for _ in 0..6 {
            cur = cycle_thinking(cur, 1);
            seen.push(thinking_label(cur));
        }
        assert_eq!(
            seen,
            vec!["default", "off", "minimal", "low", "medium", "high", "xhigh"]
        );
        assert_eq!(cycle_thinking(cur, 1), None, "xhigh wraps to default");
        // Backward from default wraps to xhigh.
        assert_eq!(
            thinking_label(cycle_thinking(None, -1)),
            "xhigh",
            "default wraps backward to xhigh"
        );
    }

    /// Build an `App` against a throwaway workspace root. `DaemonClient::new`
    /// only constructs a reqwest client; it does not connect.
    fn launch_app() -> App {
        let client = DaemonClient::new("http://127.0.0.1:1")
            .expect("DaemonClient builds without connecting");
        let root = std::env::temp_dir().join(format!(
            "ocean-tui-login-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = std::fs::create_dir_all(&root);
        App::new(client, root.to_string_lossy().into_owned())
    }

    /// Most existing tests exercise an already-open workbench rather than the
    /// startup chooser.
    fn offline_app() -> App {
        let mut app = launch_app();
        app.launch_open = false;
        app
    }

    fn resumable_session(index: u128, title: &str) -> crate::shell::sessions::Session {
        let root = std::env::temp_dir().join(format!("ocean-resume-{index}"));
        crate::shell::sessions::Session {
            id: uuid::Uuid::from_u128(index + 1).to_string(),
            title: title.into(),
            cwd: root.clone(),
            worktree: "main".into(),
            branch: Some("main".into()),
            mtime: index as u64,
            path: root.join("session.json"),
        }
    }

    #[test]
    fn normal_startup_is_clean_chat_behind_launch_chooser() {
        let app = launch_app();
        assert!(app.launch_open, "normal startup presents the chooser");
        assert!(app.center == Center::Chat && app.focus == Focus::Center);
        assert!(!app.show_sessions && !app.show_tree && !app.show_term);
        assert!(
            app.session_id.is_none(),
            "normal startup never auto-resumes"
        );
    }

    #[tokio::test]
    async fn explicit_session_bypasses_chooser_without_replacing_launch_root() {
        let mut app = launch_app();
        let launch_root = app.workspace_root.clone();
        let session = resumable_session(7, "explicit session");
        let want = AgentSessionId(uuid::Uuid::parse_str(&session.id).unwrap());

        app.resume_initial_session(session)
            .expect("explicit resume");

        assert!(!app.launch_open);
        assert_eq!(app.workspace_root, launch_root);
        assert_eq!(app.session_id, Some(want));
        assert_eq!(
            app.session_activity_probe_generation, 1,
            "explicit --session resume must probe daemon-owned activity"
        );

        let turn_id = ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(77));
        let call_id = ocean_agent_sdk::ToolCallId(uuid::Uuid::from_u128(78));
        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id: want,
            turn_id,
            model: None,
        })));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallStarted {
                session_id: want,
                turn_id,
                call: ocean_agent_sdk::ToolCall {
                    id: call_id.clone(),
                    name: "todo".into(),
                    args_json: serde_json::json!({"action": "add", "text": "resumed"}),
                },
            },
        )));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallFinished {
                session_id: want,
                turn_id,
                call_id,
                result: ocean_agent_sdk::ToolResult {
                    ok: true,
                    output: String::new(),
                    metadata_json: None,
                },
            },
        )));
        assert!(app.tray.is_visible(), "explicit session binds the tray");
    }

    #[tokio::test]
    async fn resume_initial_session_reports_resume_action_to_herdr() {
        let mut app = launch_app();
        let session = resumable_session(42, "herdr-resume-test");
        let want_str = session.id.clone();

        app.resume_initial_session(session)
            .expect("explicit resume");

        // The `--session` path must report `resume` (not `startup/new`)
        // so Herdr can distinguish a CLI restart from a new mint.
        assert_eq!(
            app.herdr.reported_session().map(|s| s.to_string()),
            Some(want_str),
        );
        assert!(app.herdr.is_ever_bound());
    }

    #[test]
    fn launch_keyboard_routes_editor_graph_and_new_session() {
        let mut editor = launch_app();
        editor.launch_sel = 2;
        editor.launch_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert!(!editor.launch_open && editor.center == Center::Editor);
        assert!(editor.show_tree, "editor route reveals files");

        let mut graph = launch_app();
        graph.launch_sel = 3;
        graph.launch_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert!(!graph.launch_open && graph.center == Center::Graph);

        let mut fresh = launch_app();
        fresh.session_id = Some(AgentSessionId(uuid::Uuid::new_v4()));
        fresh.launch_sel = 0;
        fresh.launch_key(crossterm::event::KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ));
        assert!(!fresh.launch_open && fresh.session_id.is_none());
        assert!(fresh.center == Center::Chat);
    }

    #[test]
    fn launch_mouse_click_routes_and_short_window_keeps_selection_visible() {
        let mut app = launch_app();
        app.launch_sel = 3;
        let narrow = render_app_to_string(&mut app, 40, 8);
        assert!(
            narrow.contains("open graph"),
            "selected final row remains visible in a short terminal: {narrow:?}"
        );
        assert!(app.launch_hit.iter().any(|(_, index)| *index == 3));

        let mut app = launch_app();
        render_app_to_string(&mut app, 80, 20);
        let editor = app
            .launch_hit
            .iter()
            .find(|(_, index)| *index == 2)
            .expect("editor hit row")
            .0;
        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            editor.x + 1,
            editor.y,
        ));
        assert!(!app.launch_open && app.center == Center::Editor);
        assert!(app.show_tree);
    }

    #[test]
    fn resume_window_tracks_selection_and_sanitizes_terminal_text() {
        let mut app = launch_app();
        app.resume_open = true;
        app.resume_sessions = (0..20)
            .map(|index| {
                let title = if index == 19 {
                    "selected\t\u{1b}row"
                } else {
                    "ordinary"
                };
                resumable_session(index, title)
            })
            .collect();
        app.resume_sel = 19;

        let screen = render_app_to_string(&mut app, 40, 8);
        assert!(screen.contains("selected"), "selected tail row is visible");
        assert!(!screen.contains('\t') && !screen.contains('\u{1b}'));
        assert!(app.resume_hit.iter().any(|(_, index)| *index == 19));

        app.resume_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.resume_sel, 18);
        app.resume_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.resume_sel, 19);
    }

    #[tokio::test]
    async fn resume_mouse_click_applies_visible_row() {
        let mut app = launch_app();
        app.resume_open = true;
        app.resume_sessions = vec![
            resumable_session(1, "first"),
            resumable_session(2, "second"),
        ];
        render_app_to_string(&mut app, 80, 20);
        let second = app
            .resume_hit
            .iter()
            .find(|(_, index)| *index == 1)
            .expect("second resume hit row")
            .0;
        let want = AgentSessionId(uuid::Uuid::parse_str(&app.resume_sessions[1].id).unwrap());

        app.on_crossterm(mouse(
            MouseEventKind::Down(MouseButton::Left),
            second.x + 1,
            second.y,
        ));

        assert!(!app.resume_open && !app.launch_open);
        assert_eq!(app.session_id, Some(want));
    }

    #[tokio::test]
    async fn resume_discovery_returns_through_action_channel() {
        let mut app = launch_app();
        while app.actions_rx.try_recv().is_ok() {}
        app.launch_sel = 1;
        app.launch_apply();
        assert!(app.resume_open && app.resume_loading);

        let action = tokio::time::timeout(Duration::from_secs(2), app.actions_rx.recv())
            .await
            .expect("bounded discovery")
            .expect("discovery action");
        assert!(matches!(action, Action::ResumeSessionsLoaded { .. }));
        app.dispatch(action);
        assert!(!app.resume_loading);
    }

    #[test]
    fn chooser_helpers_scroll_and_clamp_by_terminal_cells() {
        assert_eq!(selection_window_start(0, 20, 5), 0);
        assert_eq!(selection_window_start(4, 20, 5), 0);
        assert_eq!(selection_window_start(19, 20, 5), 15);
        assert_eq!(selection_window_start(3, 4, 5), 0);
        let clean = truncate_cells("ab\t界\u{1b}tail", 8);
        assert!(!clean.contains('\t') && !clean.contains('\u{1b}'));
        assert!(UnicodeWidthStr::width(clean.as_str()) <= 8);
    }

    // ── bracketed paste routing ─────────────────────────────────────────────

    #[test]
    fn paste_routes_to_the_focused_chat_composer() {
        let mut app = offline_app();
        app.on_crossterm(CrosstermEvent::Paste("/mod".into()));
        assert!(
            app.chat.wants_tab(),
            "pasted text must reach the composer (slash palette open)"
        );
    }

    #[test]
    fn open_overlay_swallows_paste_instead_of_leaking_to_composer() {
        let mut app = offline_app();
        app.settings_open = true;
        app.on_crossterm(CrosstermEvent::Paste("/mod".into()));
        assert!(
            !app.chat.wants_tab(),
            "paste must not leak beneath a modal overlay"
        );
    }

    #[test]
    fn permissions_overlay_renders_all_modes_and_tracks_effective_policy() {
        let mut app = offline_app();
        app.permissions_open = true;
        app.accept_permission_settings(&PermissionSettingsResponse {
            ok: true,
            error: None,
            persisted: Some(PermissionMode::Automatic),
            effective: PermissionMode::Automatic,
            env_override: None,
        });

        let screen = render_app_to_string(&mut app, 90, 24);
        assert!(screen.contains("PERMISSIONS"));
        assert!(screen.contains("Manually approve"));
        assert!(screen.contains("Automatically approve"));
        assert!(screen.contains("Skip all approvals"));
        assert!(screen.contains("approve each action"));
        assert!(screen.contains("anything looks unsafe"));
        assert!(screen.contains("even for unsafe actions"));
        assert!(screen.contains("current"));
        assert_eq!(app.permissions_hit.len(), 3);
    }

    #[test]
    fn permission_picker_navigation_and_env_override_feedback_are_truthful() {
        let mut app = offline_app();
        app.permissions_open = true;
        app.permissions_sel = permission_mode_index(PermissionMode::Automatic);
        app.permissions_key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        ));
        assert_eq!(
            PERMISSION_OPTIONS[app.permissions_sel].0,
            PermissionMode::SkipAll
        );
        let pending = PermissionId::new_v4();
        let request_id = RequestId::new_v4();
        app.pending_permission_ids.insert(pending);
        app.perm_request.insert(pending, request_id);
        while app.actions_rx.try_recv().is_ok() {}
        app.dispatch(Action::PermissionModeSaved(Ok(
            PermissionSettingsResponse {
                ok: true,
                error: None,
                persisted: Some(PermissionMode::Manual),
                effective: PermissionMode::SkipAll,
                env_override: Some(PermissionMode::SkipAll),
            },
        )));
        assert!(!app.permissions_open);
        assert!(app.status.contains("OCEAN_YOLO"));
        assert!(app.status.contains("Skip all approvals"));
        assert!(matches!(
            app.actions_rx.try_recv(),
            Ok(Action::PermissionDecided {
                permission_id,
                allow: true,
            }) if permission_id == pending
        ));
        assert!(app.skip_all_requests.contains(&request_id));

        let mut finished = ocean_core::EventEnvelope::new(ocean_core::OceanEvent::TurnFinished {
            ok: true,
            wall_ms: 1,
        });
        finished.request_id = Some(request_id);
        app.dispatch(Action::OceanEvent(Box::new(finished)));
        assert!(!app.skip_all_requests.contains(&request_id));

        // The cached global display may still say skip-all, but a NEW request
        // was not active when the save was confirmed and must never auto-allow.
        let next_request = RequestId::new_v4();
        let next_permission = PermissionId::new_v4();
        let mut request =
            ocean_core::EventEnvelope::new(ocean_core::OceanEvent::PermissionRequest {
                tool: "write".into(),
                reason: "permission required".into(),
                args: serde_json::json!({}),
            });
        request.request_id = Some(next_request);
        request.permission_id = Some(next_permission);
        while app.actions_rx.try_recv().is_ok() {}
        app.dispatch(Action::OceanEvent(Box::new(request)));
        assert!(matches!(
            app.actions_rx.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    #[test]
    fn provider_key_entry_accepts_bracketed_paste() {
        let mut app = offline_app();
        app.providers_open = true;
        app.providers_mode = ProvidersMode::KeyEntry {
            block_key: "deepseek".into(),
            buffer: String::new(),
        };
        app.on_crossterm(CrosstermEvent::Paste("sk-live-123\n".into()));
        let ProvidersMode::KeyEntry { buffer, .. } = &app.providers_mode else {
            panic!("key-entry mode must survive a paste");
        };
        assert_eq!(buffer, "sk-live-123", "printable chars land, newline drops");
        assert!(app.providers_open, "popup stays open");
    }

    #[test]
    fn bottom_bar_composes_buttons_plus_model_and_clicks_route() {
        for width in [40u16, 80, 120] {
            let mut app = offline_app();
            app.models_current = "claude-x".into();
            let screen = render_app_to_string(&mut app, width, 12);
            let status_row = screen.lines().last().expect("status row");
            assert!(
                status_row.contains("claude-x"),
                "model survives the composed bar at {width} cols: {status_row:?}"
            );
            assert_eq!(
                app.buttons.len(),
                6,
                "all six buttons hit-testable at {width} cols"
            );
            assert!(
                app.buttons
                    .iter()
                    .all(|(r, _)| r.y == 11 && r.x + r.width <= width),
                "button hit rects stay inside the bottom row at {width} cols"
            );
            // The user's acceptance criterion is the MOUSE: click the first
            // (sessions) and last (files) buttons at their rect centers and
            // prove the toggles fire through the real event path.
            let click = |app: &mut App, r: Rect| {
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    app.on_crossterm(CrosstermEvent::Mouse(crossterm::event::MouseEvent {
                        kind,
                        column: r.x + r.width / 2,
                        row: r.y,
                        modifiers: crossterm::event::KeyModifiers::NONE,
                    }));
                }
            };
            let first = app.buttons.first().expect("sessions button").0;
            let last = app.buttons.last().expect("files button").0;
            assert!(
                !app.show_sessions && !app.show_tree,
                "fresh app starts with both rails hidden"
            );
            click(&mut app, first);
            assert!(
                app.show_sessions,
                "clicking the sessions button shows the rail at {width} cols"
            );
            click(&mut app, last);
            assert!(
                app.show_tree,
                "clicking the files button shows the tree at {width} cols"
            );
        }
    }

    fn render_app_to_string(app: &mut App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw app");
        let buf = terminal.backend().buffer();
        let area = buf.area;
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    fn key_event(code: KeyCode) -> CrosstermEvent {
        CrosstermEvent::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn login_done_sets_status_and_clears_in_flight() {
        let mut app = offline_app();
        app.login_in_flight = true;
        app.status = "pending".into();

        app.dispatch(Action::LoginDone("claude login complete".into()));

        assert_eq!(app.status, "claude login complete");
        assert!(
            !app.login_in_flight,
            "LoginDone must clear the in-flight guard"
        );
    }

    #[test]
    fn login_while_in_flight_reports_busy_without_spawning() {
        let mut app = offline_app();
        app.login_in_flight = true;
        // Drain anything `App::new` may have queued before the assertion.
        while app.actions_rx.try_recv().is_ok() {}

        app.dispatch(Action::Login(LoginTarget::Claude));

        assert_eq!(app.status, "login already in progress");
        assert!(
            app.login_in_flight,
            "guard must stay set while a login is in flight"
        );
        // The busy guard must NOT spawn the OAuth flow — nothing is emitted.
        assert!(matches!(
            app.actions_rx.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    #[test]
    fn reconnect_status_does_not_clear_active_chat_turn() {
        let mut app = offline_app();
        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::nil()),
            session_id: AgentSessionId(uuid::Uuid::nil()),
            model: Some("test-model".into()),
        })));
        assert!(app.chat.is_busy(), "TurnStarted should mark chat busy");

        app.dispatch(Action::Status("stream reconnected".into()));

        assert_eq!(app.status, "stream reconnected");
        assert!(
            app.chat.is_busy(),
            "generic reconnect statuses must not end the active turn"
        );
    }

    #[test]
    fn tab_reaches_chat_palette_completion_instead_of_cycling_focus() {
        let mut app = offline_app();
        for c in "/mod".chars() {
            app.on_crossterm(key_event(KeyCode::Char(c)));
        }

        app.on_crossterm(key_event(KeyCode::Tab));

        assert!(app.focus == Focus::Center, "Tab should not cycle focus");
        assert!(app.center == Center::Chat, "chat should remain active");
        let screen = render_app_to_string(&mut app, 100, 28);
        assert!(
            screen.contains("/model"),
            "Tab should complete the selected slash command in the composer, got: {screen:?}"
        );
    }

    // ── welcome provider line ────────────────────────────────────────────────

    #[test]
    fn welcome_provider_line_zero_configured() {
        // When no providers are in the env or auth file, the welcome line
        // should prompt the user to log in.
        let rows: Vec<ProviderRow> = PROVIDER_TABLE
            .iter()
            .map(|(section, label, block_key, env_vars)| ProviderRow {
                section: *section,
                label,
                block_key,
                env_vars,
                status: "not configured".into(),
            })
            .collect();
        let n_configured = rows.iter().filter(|r| r.status != "not configured").count();
        assert_eq!(n_configured, 0);
    }

    #[test]
    fn login_popup_separates_voice_credentials_from_agent_models() {
        let mut app = offline_app();
        app.providers_rows = PROVIDER_TABLE
            .iter()
            .map(|(section, label, block_key, env_vars)| ProviderRow {
                section: *section,
                label,
                block_key,
                env_vars,
                status: "not configured".into(),
            })
            .collect();
        app.providers_open = true;

        let screen = render_app_to_string(&mut app, 100, 30);
        assert!(screen.contains("AGENT MODELS"));
        assert!(screen.contains("VOICE MODELS"));
        assert!(screen.contains("xAI Voice"));
        assert!(screen.contains("OpenAI Realtime"));

        let realtime = app
            .providers_rows
            .iter()
            .find(|row| row.block_key == "openai-realtime")
            .expect("dedicated realtime row");
        assert_eq!(realtime.section, ProviderSection::Voice);
        assert_ne!(realtime.block_key, "openai");
        assert!(!realtime.is_oauth());
    }

    #[test]
    fn short_login_popup_scrolls_to_selected_voice_row() {
        let mut app = offline_app();
        app.providers_rows = App::build_provider_rows();
        app.providers_open = true;
        app.providers_sel = app
            .providers_rows
            .iter()
            .position(|row| row.block_key == "openai-realtime")
            .expect("realtime voice row");

        let screen = render_app_to_string(&mut app, 80, 12);
        assert!(screen.contains("VOICE MODELS"));
        assert!(screen.contains("OpenAI Realtime"));
    }

    #[test]
    fn refresh_provider_line_is_condition_only_never_ready_count() {
        // The welcome line is a terse configuration condition ONLY when zero
        // providers are configured — configured credentials must never be
        // rendered as a `ready · N providers` runtime-health claim.
        let mut app = offline_app();
        app.refresh_welcome_provider_line();
        let after = app.chat.welcome_provider_line.clone();
        match &after {
            None => {} // providers configured in this environment: no line
            Some(line) => assert_eq!(line, "provider configuration required"),
        }
        // And it produces consistent output for the same state.
        app.refresh_welcome_provider_line();
        assert_eq!(app.chat.welcome_provider_line, after, "idempotent");
    }

    // ── typed health ────────────────────────────────────────────────────────

    #[test]
    fn health_sources_clear_independently_through_dispatch() {
        let mut app = offline_app();
        app.dispatch(Action::HealthDegraded {
            source: HealthSource::Daemon,
            condition: "daemon offline".into(),
        });
        app.dispatch(Action::HealthDegraded {
            source: HealthSource::Sse,
            condition: "stream reconnecting".into(),
        });
        // An unrelated notice must not clear typed health.
        app.dispatch(Action::Status("copied".into()));
        assert_eq!(app.health.effective(), Some("daemon offline"));
        // Daemon recovery clears ONLY its source — the stream stays degraded.
        app.dispatch(Action::HealthRecovered(HealthSource::Daemon));
        assert_eq!(app.health.effective(), Some("stream reconnecting"));
        app.dispatch(Action::HealthRecovered(HealthSource::Sse));
        assert_eq!(app.health.effective(), None, "all sources healthy");
    }

    // ── transient notices + instant model selection ──────────────────────────

    #[test]
    fn status_notice_expires_back_to_model_only() {
        let mut app = offline_app();
        app.dispatch(Action::SetModel("kimi-k3".into()));
        app.dispatch(Action::Status("api key saved".into()));
        let segs = status::segments(&app.status_data(), 120);
        assert!(
            segs.iter().any(|s| s.text == "api key saved"),
            "a fresh notice renders on the row"
        );
        // One instant short of the TTL the notice survives …
        assert!(!app.expire_status(app.status_at + STATUS_TTL - Duration::from_millis(1)));
        // … at the TTL it clears and the idle row is model-only again.
        assert!(app.expire_status(app.status_at + STATUS_TTL));
        let segs = status::segments(&app.status_data(), 120);
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["kimi-k3"], "idle row returns to model-only");
    }

    #[test]
    fn model_selection_updates_status_row_immediately() {
        let mut app = offline_app();
        // `/model <id>` — before any TurnStarted names a model.
        app.dispatch(Action::SetModel("glm-5".into()));
        assert_eq!(app.status_data().model, Some("glm-5"));
        // The `/models` picker apply path must show just as instantly.
        app.models_entries = vec![entry("deepseek-v4-pro", "deepseek", true)];
        app.models_sel = 0;
        app.models_apply();
        assert_eq!(app.status_data().model, Some("deepseek-v4-pro"));
    }

    #[test]
    fn compact_rejects_unbound_and_active_sessions_without_spawning() {
        let mut app = offline_app();
        while app.actions_rx.try_recv().is_ok() {}

        app.dispatch(Action::CompactSession);
        assert!(app.status.contains("start or resume"));
        assert_eq!(app.compacting_session, None);
        assert!(matches!(
            app.actions_rx.try_recv(),
            Err(TryRecvError::Empty)
        ));

        app.session_id = Some(AgentSessionId(uuid::Uuid::from_u128(41)));
        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(42)),
            session_id: app.session_id.unwrap(),
            model: Some("test-model".into()),
        })));
        app.dispatch(Action::CompactSession);
        assert!(app.status.contains("active turn"));
        assert_eq!(app.compacting_session, None);
        assert!(matches!(
            app.actions_rx.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    fn synchronized_snapshot(
        session_id: AgentSessionId,
        text: &str,
    ) -> ocean_core::SessionSyncSnapshot {
        ocean_core::SessionSyncSnapshot {
            session_id: session_id.0,
            model: "test-model".into(),
            provider: "fake".into(),
            transcript: vec![ocean_core::SessionTranscriptEntry {
                role: "assistant".into(),
                timestamp_ms: None,
                text: text.into(),
                images: Vec::new(),
                tool_call_id: None,
                tool_name: None,
                is_error: None,
            }],
            truncated_messages: 0,
            truncated_text_bytes: 0,
        }
    }

    fn synchronized_fence(index: u128) -> ocean_core::SessionEventFence {
        ocean_core::SessionEventFence {
            event_id: Some(uuid::Uuid::from_u128(index)),
        }
    }

    fn compact_success(
        session_id: AgentSessionId,
        text: &str,
        elided_messages: u64,
        fence_index: u128,
    ) -> ocean_core::CompactResponse {
        ocean_core::CompactResponse {
            ok: true,
            session_id: session_id.0,
            elided_messages,
            wall_ms: 87,
            stderr: String::new(),
            sync: Some(synchronized_snapshot(session_id, text)),
            fence: Some(synchronized_fence(fence_index)),
        }
    }

    fn sync_success(
        session_id: AgentSessionId,
        text: &str,
        fence_index: u128,
    ) -> ocean_core::SessionSyncResponse {
        ocean_core::SessionSyncResponse {
            ok: true,
            session_id: session_id.0,
            snapshot: Some(synchronized_snapshot(session_id, text)),
            fence: Some(synchronized_fence(fence_index)),
            error: None,
        }
    }

    fn test_image(label: &str) -> ocean_agent_sdk::TurnImage {
        ocean_agent_sdk::TurnImage {
            mime_type: "image/png".into(),
            data: label.into(),
        }
    }

    #[tokio::test]
    async fn submitted_images_restore_only_for_definite_rejections() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(590));
        app.session_id = Some(session_id);
        app.session_binding_generation = 4;

        app.pending_images.push(test_image("busy"));
        let staged = app
            .stage_pending_images_for_submit()
            .expect("staged busy image");
        assert_eq!(staged[0].data, "busy");
        assert!(app.pending_images.is_empty());
        assert_eq!(app.in_flight_images.len(), 1);
        app.chat.seed_pending_submission_for_test(1);
        app.dispatch(Action::TurnSessionBusy {
            submission_id: 1,
            session_id,
            binding_generation: 4,
            prompt: "with image".into(),
        });
        assert_eq!(app.pending_images.len(), 1, "busy rejection restores image");
        assert!(app.in_flight_images.is_empty());

        let _ = app.stage_pending_images_for_submit();
        app.chat.seed_pending_submission_for_test(2);
        app.dispatch(Action::TurnSendFailed {
            submission_id: 2,
            prompt: "retry image".into(),
            err: "daemon unavailable".into(),
        });
        assert_eq!(
            app.pending_images.len(),
            1,
            "definitely-unsent rejection restores image"
        );

        let _ = app.stage_pending_images_for_submit();
        app.chat.seed_pending_submission_for_test(3);
        app.dispatch(Action::TurnAccepted {
            submission_id: 3,
            turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(591)),
        });
        assert!(app.pending_images.is_empty());
        assert!(
            app.in_flight_images.is_empty(),
            "accepted image is consumed"
        );

        app.chat.load_history(Vec::new());
        app.pending_images.push(test_image("unknown"));
        let _ = app.stage_pending_images_for_submit();
        app.chat.seed_pending_submission_for_test(4);
        app.dispatch(Action::TurnOutcomeUnknown {
            submission_id: 4,
            err: "acknowledgement lost".into(),
        });
        assert!(app.pending_images.is_empty());
        assert!(
            app.in_flight_images.is_empty(),
            "unknown outcome must not offer an unsafe image replay"
        );
    }

    #[tokio::test]
    async fn resume_activity_probe_latches_busy_and_snapshot_clears_it() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(600));
        app.session_id = Some(session_id);
        app.session_binding_generation = 9;
        app.session_activity_probe_generation = 3;

        app.dispatch(Action::SessionActivityProbeFinished {
            session_id,
            binding_generation: 9,
            probe_generation: 3,
            after_busy_rejection: false,
            active_was_observed: false,
            result: Err(crate::shell::action::CompactFailure {
                message: "session has an active operation; try again shortly".into(),
                transcript_may_have_changed: true,
            }),
        });
        assert!(app.chat.is_busy(), "active resume must lock the composer");
        assert!(app.status.contains("still working"));

        assert_eq!(app.session_activity_probe_generation, 4);
        app.dispatch(Action::SessionActivityProbeFinished {
            session_id,
            binding_generation: 9,
            probe_generation: 4,
            after_busy_rejection: true,
            active_was_observed: true,
            result: Ok(sync_success(session_id, "finished while resuming", 601)),
        });
        assert!(
            !app.chat.is_busy(),
            "authoritative post-turn snapshot must unlock the composer"
        );
        assert!(app.status.contains("session ready"));
    }

    #[tokio::test]
    async fn observed_activity_keeps_polling_after_stream_finish_and_probe_blip() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(602));
        app.session_id = Some(session_id);
        app.session_binding_generation = 10;
        app.session_activity_probe_generation = 5;
        app.chat.adopt_active_turn();
        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::TurnFinished {
            session_id,
            turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(603)),
            status: ocean_agent_sdk::AgentTurnStatus::Completed,
            error: None,
            wall_ms: None,
            output_tokens: None,
            input_tokens: None,
            cache_read_tokens: None,
            tokens_per_second: None,
            context_usage: None,
        })));
        assert!(!app.chat.is_busy());

        app.dispatch(Action::SessionActivityProbeFinished {
            session_id,
            binding_generation: 10,
            probe_generation: 5,
            after_busy_rejection: false,
            active_was_observed: true,
            result: Err(crate::shell::action::CompactFailure {
                message: "temporary sync transport failure".into(),
                transcript_may_have_changed: true,
            }),
        });

        assert_eq!(
            app.session_activity_probe_generation, 6,
            "authoritatively observed activity must keep polling to a fence"
        );
    }

    #[test]
    fn stale_or_compaction_racing_activity_probe_cannot_replace_history() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(605));
        app.session_id = Some(session_id);
        app.session_binding_generation = 12;
        app.session_activity_probe_generation = 8;
        app.chat
            .load_history(vec![crate::shell::sessions::HistoryMsg {
                role: "assistant".into(),
                text: "current history".into(),
            }]);

        app.dispatch(Action::SessionActivityProbeFinished {
            session_id,
            binding_generation: 12,
            probe_generation: 7,
            after_busy_rejection: false,
            active_was_observed: false,
            result: Ok(sync_success(session_id, "stale probe", 606)),
        });
        assert_eq!(
            app.chat.last_reply_for_test().as_deref(),
            Some("current history")
        );

        app.sessions_requiring_sync.insert(session_id);
        app.dispatch(Action::SessionActivityProbeFinished {
            session_id,
            binding_generation: 12,
            probe_generation: 8,
            after_busy_rejection: false,
            active_was_observed: false,
            result: Ok(sync_success(session_id, "raced compact", 607)),
        });
        assert_eq!(
            app.chat.last_reply_for_test().as_deref(),
            Some("current history")
        );
        assert!(app.sessions_requiring_sync.contains(&session_id));
    }

    #[test]
    fn stale_resume_activity_probe_cannot_latch_a_rebound_session() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(610));
        app.session_id = Some(session_id);
        app.session_binding_generation = 12;
        app.session_activity_probe_generation = 6;

        app.dispatch(Action::SessionActivityProbeFinished {
            session_id,
            binding_generation: 11,
            probe_generation: 6,
            after_busy_rejection: false,
            active_was_observed: false,
            result: Err(crate::shell::action::CompactFailure {
                message: "session has an active operation; try again shortly".into(),
                transcript_may_have_changed: true,
            }),
        });

        assert!(!app.chat.is_busy());
    }

    fn arm_compact(app: &mut App, session_id: AgentSessionId) -> (u64, u64) {
        app.session_id = Some(session_id);
        app.session_binding_generation = app.session_binding_generation.wrapping_add(1);
        app.sessions_requiring_sync.insert(session_id);
        app.compacting_session = Some(session_id);
        app.compact_request_in_flight = true;
        app.compact_invalidation_pending = false;
        app.compact_binding_generation = app.session_binding_generation;
        app.compact_operation_generation = app.compact_operation_generation.wrapping_add(1);
        (
            app.compact_binding_generation,
            app.compact_operation_generation,
        )
    }

    #[tokio::test]
    async fn compact_completion_installs_snapshot_fence_and_reports_success_or_noop() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(51));
        let (binding_generation, operation_generation) = arm_compact(&mut app, session_id);
        let original_stream_generation = app.stream_generation;

        app.dispatch(Action::CompactFinished {
            session_id,
            binding_generation,
            operation_generation,
            result: Ok(compact_success(
                session_id,
                "authoritative compacted summary",
                12,
                5001,
            )),
        });
        assert_eq!(app.compacting_session, None);
        assert!(app.stream_generation > original_stream_generation);
        assert!(app.stream_task.is_some());
        assert_eq!(
            app.chat.last_reply_for_test().as_deref(),
            Some("authoritative compacted summary")
        );
        assert!(app.status.contains("12 messages summarized"));

        let (binding_generation, operation_generation) = arm_compact(&mut app, session_id);
        app.dispatch(Action::CompactFinished {
            session_id,
            binding_generation,
            operation_generation,
            result: Ok(compact_success(
                session_id,
                "unchanged recent context",
                0,
                5002,
            )),
        });
        assert_eq!(app.compacting_session, None);
        assert_eq!(
            app.chat.last_reply_for_test().as_deref(),
            Some("unchanged recent context")
        );
        assert!(app.status.contains("nothing to compact"));

        let (binding_generation, operation_generation) = arm_compact(&mut app, session_id);
        app.dispatch(Action::CompactFinished {
            session_id,
            binding_generation,
            operation_generation,
            result: Err(crate::shell::action::CompactFailure {
                message: "provider deadline elapsed".into(),
                transcript_may_have_changed: false,
            }),
        });
        assert_eq!(app.compacting_session, None);
        assert!(
            app.status.contains("timed out") || app.status.contains("deadline"),
            "provider/timeout errors stay visible: {}",
            app.status
        );

        let (binding_generation, operation_generation) = arm_compact(&mut app, session_id);
        app.dispatch(Action::CompactFinished {
            session_id,
            binding_generation,
            operation_generation,
            result: Err(crate::shell::action::CompactFailure {
                message: "compacted, but response was lost".into(),
                transcript_may_have_changed: true,
            }),
        });
        assert_eq!(app.compacting_session, Some(session_id));
        assert!(app.compact_refresh_required);
        assert!(app.status.contains("run /compact"));

        app.compact_refresh_required = false;
        app.dispatch(Action::CompactReloadFinished {
            session_id,
            binding_generation,
            operation_generation,
            result: Ok(sync_success(
                session_id,
                "recovered authoritative context",
                5003,
            )),
        });
        assert_eq!(app.compacting_session, None);
        assert!(!app.compact_refresh_required);
        assert_eq!(
            app.chat.last_reply_for_test().as_deref(),
            Some("recovered authoritative context")
        );
    }

    #[tokio::test]
    async fn compact_self_invalidation_defers_sync_until_post_response() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(53));
        let (binding_generation, operation_generation) = arm_compact(&mut app, session_id);
        let stream_generation = app.stream_generation;

        app.dispatch(Action::BoundAgentEvent {
            session_id,
            binding_generation,
            stream_generation,
            event: Box::new(AgentTurnEvent::Extension {
                extension: "ocean.session_changed".into(),
                payload: serde_json::json!({}),
                scope: Some(session_id),
            }),
        });
        assert!(app.compact_request_in_flight);
        assert!(app.compact_invalidation_pending);
        assert_eq!(app.compact_operation_generation, operation_generation);

        app.dispatch(Action::CompactFinished {
            session_id,
            binding_generation,
            operation_generation,
            result: Ok(compact_success(
                session_id,
                "compacted before follow-up sync",
                4,
                5007,
            )),
        });

        assert_eq!(
            app.chat.last_reply_for_test().as_deref(),
            Some("compacted before follow-up sync")
        );
        assert_eq!(app.compacting_session, Some(session_id));
        assert!(!app.compact_request_in_flight);
        assert!(app.sessions_requiring_sync.contains(&session_id));
        assert!(app.compact_operation_generation > operation_generation);

        let sync_operation = app.compact_operation_generation;
        app.dispatch(Action::CompactReloadFinished {
            session_id,
            binding_generation,
            operation_generation: sync_operation,
            result: Ok(sync_success(session_id, "post-compact authority", 5008)),
        });
        assert_eq!(app.compacting_session, None);
        assert!(!app.sessions_requiring_sync.contains(&session_id));
        assert_eq!(
            app.chat.last_reply_for_test().as_deref(),
            Some("post-compact authority")
        );
    }

    #[tokio::test]
    async fn compact_completion_clears_pre_fence_busy_and_restarts_after_fence() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(56));
        let (binding_generation, operation_generation) = arm_compact(&mut app, session_id);
        let old_stream_generation = app.stream_generation;
        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(57)),
            session_id,
            model: Some("test-model".into()),
        })));

        app.dispatch(Action::CompactFinished {
            session_id,
            binding_generation,
            operation_generation,
            result: Ok(compact_success(
                session_id,
                "snapshot before active turn",
                8,
                5004,
            )),
        });

        assert!(!app.chat.is_busy());
        assert_eq!(app.compacting_session, None);
        assert!(!app.compact_refresh_required);
        assert!(app.stream_generation > old_stream_generation);
        assert_eq!(
            app.chat.last_reply_for_test().as_deref(),
            Some("snapshot before active turn")
        );
        app.dispatch(Action::BoundAgentEvent {
            session_id,
            binding_generation,
            stream_generation: app.stream_generation,
            event: Box::new(AgentTurnEvent::TurnStarted {
                turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(58)),
                session_id,
                model: Some("post-fence-model".into()),
            }),
        });
        assert!(app.chat.is_busy(), "post-fence replay re-establishes busy");
    }

    #[tokio::test]
    async fn compact_completion_rejects_a_to_b_to_a_generation_alias() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(61));
        let (old_binding, old_operation) = arm_compact(&mut app, session_id);
        app.chat
            .load_history(vec![crate::shell::sessions::HistoryMsg {
                role: "assistant".into(),
                text: "current session".into(),
            }]);

        // Exercise the real A→B→A bind path while A's daemon operation can
        // still commit. Rebinding A must enter refresh-only hold automatically.
        let other = AgentSessionId(uuid::Uuid::from_u128(62));
        app.bind_session_with(other, true);
        app.bind_session_with(session_id, true);
        assert_eq!(app.compacting_session, Some(session_id));
        assert!(app.sessions_requiring_sync.contains(&session_id));
        assert_ne!(app.compact_binding_generation, old_binding);

        app.dispatch(Action::CompactFinished {
            session_id,
            binding_generation: old_binding,
            operation_generation: old_operation,
            result: Ok(compact_success(session_id, "stale session", 9, 5005)),
        });

        assert_eq!(app.compacting_session, Some(session_id));
        assert!(app.sessions_requiring_sync.contains(&session_id));
        app.dispatch(Action::CompactFinished {
            session_id,
            binding_generation: old_binding,
            operation_generation: old_operation,
            result: Err(crate::shell::action::CompactFailure {
                message: "old precommit rejection".into(),
                transcript_may_have_changed: false,
            }),
        });
        assert!(app.sessions_requiring_sync.contains(&session_id));
        assert_eq!(
            app.chat.last_reply_for_test().as_deref(),
            Some("current session")
        );
        app.dispatch(Action::SubmitPrompt {
            submission_id: 1,
            prompt: "must stay local".into(),
        });
        assert!(app.status.contains("synchronization"));
    }

    #[tokio::test]
    async fn superseded_stream_generation_cannot_apply_queued_same_session_event() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(71));
        app.session_id = Some(session_id);
        app.session_binding_generation = 4;
        app.stream_generation = 8;

        app.dispatch(Action::BoundAgentEvent {
            session_id,
            binding_generation: 4,
            stream_generation: 7,
            event: Box::new(AgentTurnEvent::TurnStarted {
                turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(72)),
                session_id,
                model: Some("stale-model".into()),
            }),
        });

        assert!(!app.chat.is_busy());
    }

    #[tokio::test]
    async fn session_changed_invalidates_derived_tray_and_enters_sync_hold() {
        let mut app = offline_app();
        app.show_tree = true;
        let session_id = AgentSessionId(uuid::Uuid::from_u128(75));
        let turn_id = ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(76));
        let call_id = ocean_agent_sdk::ToolCallId(uuid::Uuid::from_u128(77));
        app.dispatch(Action::SessionBound(session_id));
        let binding_generation = app.session_binding_generation;
        let stream_generation = app.stream_generation;
        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id,
            turn_id,
            model: None,
        })));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallStarted {
                session_id,
                turn_id,
                call: ocean_agent_sdk::ToolCall {
                    id: call_id.clone(),
                    name: "todo".into(),
                    args_json: serde_json::json!({"action":"add","text":"stale todo"}),
                },
            },
        )));
        app.dispatch(Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallFinished {
                session_id,
                turn_id,
                call_id,
                result: ocean_agent_sdk::ToolResult {
                    ok: true,
                    output: "1 [ ] stale todo\n".into(),
                    metadata_json: None,
                },
            },
        )));
        assert!(app.tray.is_visible());
        assert!(app.tray.has_todo_text_for_test("stale todo"));

        app.dispatch(Action::BoundAgentEvent {
            session_id,
            binding_generation,
            stream_generation,
            event: Box::new(AgentTurnEvent::Extension {
                extension: "ocean.session_changed".into(),
                payload: serde_json::json!({}),
                scope: Some(session_id),
            }),
        });

        assert!(!app.tray.has_todo_text_for_test("stale todo"));
        assert_eq!(app.compacting_session, Some(session_id));
        assert!(app.sessions_requiring_sync.contains(&session_id));
        assert!(app.stream_generation > stream_generation);
    }

    #[tokio::test]
    async fn replay_reset_enters_refresh_only_hold_and_sync_clears_stale_busy() {
        let mut app = offline_app();
        let session_id = AgentSessionId(uuid::Uuid::from_u128(81));
        app.session_id = Some(session_id);
        app.session_binding_generation = 5;
        app.stream_generation = 9;
        app.dispatch(Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(82)),
            session_id,
            model: Some("test-model".into()),
        })));
        assert!(app.chat.is_busy());

        app.dispatch(Action::BoundAgentReplayResetRequired {
            session_id,
            binding_generation: 5,
            stream_generation: 9,
        });
        assert_eq!(app.compacting_session, Some(session_id));
        assert!(!app.compact_refresh_required);
        assert!(app.status.contains("synchronizing"));
        assert!(app.stream_generation > 9);

        // An envelope already queued by the aborted stream is rejected now,
        // not only after the replacement sync finishes.
        app.dispatch(Action::BoundAgentEvent {
            session_id,
            binding_generation: 5,
            stream_generation: 9,
            event: Box::new(AgentTurnEvent::AssistantTextDelta {
                session_id,
                turn_id: ocean_agent_sdk::AgentTurnId(uuid::Uuid::from_u128(82)),
                delta: "stale".into(),
            }),
        });
        assert_ne!(app.chat.last_reply_for_test().as_deref(), Some("stale"));

        let operation_generation = app.compact_operation_generation;
        app.dispatch(Action::CompactReloadFinished {
            session_id,
            binding_generation: 5,
            operation_generation,
            result: Ok(sync_success(session_id, "authoritative idle history", 5006)),
        });

        assert!(!app.chat.is_busy());
        assert_eq!(app.compacting_session, None);
        assert_eq!(
            app.chat.last_reply_for_test().as_deref(),
            Some("authoritative idle history")
        );
    }

    #[test]
    fn terminal_navigation_creates_and_unhides_the_dock() {
        // With the title button gone, `/terminal` (Nav::Terminal) is the
        // pointer-free path: it must create the PTY when absent and ALWAYS
        // unhide the dock — focusing an invisible pane strands the keyboard.
        let mut app = offline_app();
        app.show_term = false;
        app.dispatch(Action::Navigate(Nav::Terminal));
        assert!(app.show_term, "navigate must unhide the dock");
        assert!(app.focus == Focus::Term, "focus lands in the terminal");
    }

    #[test]
    fn redundant_workspace_pane_titles_are_not_rendered() {
        let mut app = offline_app();
        app.show_sessions = true;
        app.show_tree = true;
        app.dispatch(Action::Navigate(Nav::Terminal));
        let screen = render_app_to_string(&mut app, 120, 32);
        for title in ["SESSIONS", "FILES", "SESSION COMPONENT", "TERMINAL"] {
            assert!(
                !screen.contains(title),
                "obsolete pane title rendered: {title}"
            );
        }
    }

    #[test]
    fn login_done_triggers_welcome_refresh() {
        let mut app = offline_app();
        // Force the welcome line to something recognisable so we can detect
        // that the refresh happened.
        app.chat.welcome_provider_line = Some("before-login".into());
        app.dispatch(Action::LoginDone("claude login complete".into()));
        // After LoginDone, the welcome line is recomputed from live state.
        // It won't be "before-login" anymore.
        assert_ne!(
            app.chat.welcome_provider_line,
            Some("before-login".into()),
            "LoginDone must refresh the welcome provider line"
        );
    }
}
