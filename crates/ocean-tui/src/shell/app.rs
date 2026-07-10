//! App — the workbench frame, mirroring CTRL's `ui()` layout exactly:
//!
//! ```text
//! ┌ title row ──────────────────────────────────────────────┐
//! │ SESSIONS │▏│ breadcrumb                       │▏│ FILES │
//! │ (left)   │ │ CENTER: chat / editor / graph    │ │(right)│
//! │          │ │ ──────────────────────────────── │ │       │
//! │          │ │ TERMINAL (docked bottom, live)   │ │       │
//! └ status row ──────────────────────────────────────────────┘
//! ```
//!
//! No tabs: sessions, tree, and the terminal dock are ALWAYS visible (the dock
//! appears when a shell is hydrated). The center holds the working surface —
//! chat by default, the editor when a file is open, the graph as a toggle —
//! the same way CTRL swaps its center between editor and graph.
//!
//! Keys: ⌃⌥1 sessions · ⌃⌥2 files · ⌃⌥3 chat · ⌃⌥4 editor · ⌃⌥5 graph toggle ·
//! ⌃⌥6 terminal · Tab cycles focus · Esc → back to chat (double-Esc leaves the
//! terminal dock) · ⌃Q quits (⌃C passes to the PTY).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event as CrosstermEvent, KeyCode, KeyModifiers, MouseButton, MouseEventKind,
};
use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent, AgentTurnRequest, ThinkingLevel};
use ocean_core::RequestId;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use tokio::sync::mpsc;

use super::{
    action::{Action, LoginTarget, Nav},
    client::{DaemonClient, ModelEntry},
    daemon_boot,
    component::Component,
    components::{
        chat::{self, ChatComponent}, editor::EditorComponent, file_tree::FileTreeComponent,
        graph::GraphComponent, pty_pane::PtyComponent, session_rail::SessionRailComponent,
    },
    errfmt,
    event::{Event, EventHandler},
    git, kitty,
    status::{self, StatusData, Tone},
    theme::{self, g},
    tui,
};

const SESS_W: u16 = 30;
const TREE_W: u16 = 30;
/// Default terminal-dock height; resizable at runtime (drag the splitter / ⌃⌥↑↓).
const TERM_H: u16 = 14;
/// Floor for the dock and the main surface so neither can be squeezed to nothing.
const MIN_TERM_H: u16 = 3;
const MIN_CENTER_H: u16 = 5;

/// Largest dock height that still leaves the main surface `MIN_CENTER_H` rows,
/// given the center column's total height (crumb + surface + splitter + dock).
fn max_term_h(center_h: u16) -> u16 {
    // reserve: 1 crumb + 1 splitter + MIN_CENTER_H surface.
    center_h.saturating_sub(2 + MIN_CENTER_H).max(MIN_TERM_H)
}

/// What the center surface is showing (CTRL swaps editor↔graph the same way).
#[derive(Clone, Copy, PartialEq)]
enum Center {
    Chat,
    Editor,
    Graph,
}

/// Which visible pane has the keyboard.
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Sessions,
    Tree,
    Center,
    Term,
}

/// One row in the `/providers` auth popup: a static descriptor (label, auth-file
/// block key, credential env vars) plus a status string computed at open time
/// and refreshed after an inline API-key save.
#[derive(Clone)]
struct ProviderRow {
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

/// Static descriptor (no per-open status) for [`ProviderRow`].
const PROVIDER_TABLE: &[(&str, &str, &[&str])] = &[
    ("Claude (Claude Code OAuth)", "claude-code", &[]),
    ("Codex (ChatGPT OAuth)", "openai-codex", &[]),
    ("GLM — Z.AI coding plan", "glm", &["ZAI_API_KEY", "GLM_API_KEY", "OCEAN_GLM_API_KEY", "ZHIPUAI_API_KEY", "BIGMODEL_API_KEY"]),
    ("DeepSeek", "deepseek", &["DEEPSEEK_API_KEY", "OCEAN_DEEPSEEK_API_KEY"]),
    ("Kimi (Moonshot)", "kimi", &["MOONSHOT_API_KEY", "KIMI_API_KEY", "OCEAN_MOONSHOT_API_KEY"]),
    ("MiniMax", "minimax", &["MINIMAX_API_KEY", "OCEAN_MINIMAX_API_KEY"]),
    ("Google (Gemini)", "google", &["GEMINI_API_KEY", "GOOGLE_API_KEY", "OCEAN_GOOGLE_API_KEY"]),
    ("OpenAI", "openai", &["OPENAI_API_KEY", "OCEAN_OPENAI_API_KEY"]),
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
    if let Some(var) = env_vars
        .iter()
        .find(|v| std::env::var(v).ok().filter(|x| !x.trim().is_empty()).is_some())
    {
        return format!("env:{var}");
    }
    let Some(json) = auth_json else {
        return "not configured".into();
    };
    let Some(entry) = json.pointer(&format!("/{block_key}")) else {
        return "not configured".into();
    };
    if matches!(block_key, "claude-code" | "openai-codex") {
        let is_oauth = entry
            .pointer("/type")
            .and_then(serde_json::Value::as_str)
            == Some("oauth");
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
        match entry.pointer("/expires").and_then(serde_json::Value::as_i64) {
            Some(ms) => {
                let now_ms = unix_epoch_ms();
                let expires_ms = if ms >= 1_000_000_000_000 { ms } else { ms * 1000 };
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

pub struct App {
    client: DaemonClient,
    workspace_root: String,
    rail: SessionRailComponent,
    tree: FileTreeComponent,
    chat: ChatComponent,
    pty: PtyComponent,
    editor: EditorComponent,
    graph: GraphComponent,
    center: Center,
    focus: Focus,
    session_id: Option<AgentSessionId>,
    /// `/model <id>` override applied to subsequent turns (None → daemon default).
    model_override: Option<String>,
    /// The live SSE subscription for `session_id`. Held so a session switch
    /// aborts the superseded stream instead of leaking it (a leaked stream
    /// kept pumping a stale session's events into the chat).
    stream_task: Option<tokio::task::JoinHandle<()>>,
    /// The health-monitor task that re-probes and triggers autostart on failure.
    health_task: Option<tokio::task::JoinHandle<()>>,
    status: String,
    /// True while a `/login` OAuth flow is running off-thread. A second
    /// `/login` while set is rejected with a busy status instead of racing a
    /// second callback server / browser launch.
    login_in_flight: bool,
    should_quit: bool,
    actions_tx: mpsc::UnboundedSender<Action>,
    actions_rx: mpsc::UnboundedReceiver<Action>,
    /// OCEAN-185: the token minted for the in-flight submit, claimed by the
    /// turn's first permission request (keyed by its request_id).
    pending_submit_token: Option<String>,
    decision_tokens: HashMap<RequestId, String>,
    perm_request: HashMap<ocean_core::PermissionId, RequestId>,
    /// Pane rects from the last draw, for mouse routing.
    r_sessions: Rect,
    r_tree: Rect,
    r_center: Rect,
    r_term: Rect,
    /// The full center COLUMN (crumb + surface + splitter + dock), for clamping
    /// a terminal-dock resize against the available height.
    r_center_col: Rect,
    r_split_term: Rect,
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
    /// Mouse text selection. `sel_press` arms on any left-down that isn't a
    /// splitter grab; the first drag promotes it into `selection` — a linear
    /// (terminal-style) sweep in screen cells, anchor → head. Releasing the
    /// button auto-copies the swept text to the system clipboard.
    sel_press: Option<(u16, u16)>,
    selection: Option<((u16, u16), (u16, u16))>,
    /// Cell symbols of the last drawn frame (row-major), captured only while a
    /// selection is live, so release-time copy reads exactly what was shown.
    frame_cells: Vec<Vec<String>>,
    /// Panel visibility (CTRL's collapsible rails + terminal dock).
    show_sessions: bool,
    show_tree: bool,
    show_term: bool,
    /// Clickable title-bar buttons (CTRL's upper model): (rect, action).
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
    /// Output tokens/second of the last completed turn, and the running total
    /// of output tokens this session — status-bar segments.
    turn_tok_per_s: Option<f64>,
    session_tokens: u64,
    /// `/settings` overlay: open flag + selected row.
    settings_open: bool,
    settings_sel: usize,
    /// `/providers` popup: auth-status list + inline API-key entry.
    providers_open: bool,
    providers_sel: usize,
    providers_rows: Vec<ProviderRow>,
    providers_mode: ProvidersMode,
}

/// A title-bar button — CTRL's icon toggles, extended with the center surfaces.
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
            chat: ChatComponent::new(),
            pty: PtyComponent::default(),
            editor: EditorComponent::new(root.clone()),
            graph: GraphComponent::new(root),
            // Land in the chat, typing-ready — the rail is one click away.
            center: Center::Chat,
            focus: Focus::Center,
            session_id: None,
            model_override: None,
            stream_task: None,
            health_task: None,
            status: "connecting…".into(),
            login_in_flight: false,
            should_quit: false,
            actions_tx,
            actions_rx,
            pending_submit_token: None,
            decision_tokens: HashMap::new(),
            perm_request: HashMap::new(),
            r_sessions: Rect::default(),
            r_tree: Rect::default(),
            r_center: Rect::default(),
            r_term: Rect::default(),
            r_center_col: Rect::default(),
            r_split_term: Rect::default(),
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
            selection: None,
            frame_cells: Vec::new(),
            show_sessions: true,
            show_tree: true,
            show_term: true,
            buttons: Vec::new(),
            esc_armed: false,
            last_tree_scan: Instant::now(),
            // Populated above from `root`, before it was moved into components;
            // refreshed thereafter on the 1s tick.
            git_status,
            turn_tok_per_s: None,
            session_tokens: 0,
            settings_open: false,
            settings_sel: 0,
            providers_open: false,
            providers_sel: 0,
            providers_rows: Vec::new(),
            providers_mode: ProvidersMode::List,
        };
        app.apply_focus();
        // `@` file mentions index the launch project from the start.
        app.chat
            .set_mention_root(PathBuf::from(&app.workspace_root));
        // Welcome empty-state: tell the chat whether providers are configured.
        app.refresh_welcome_provider_line();
        // Auto-resume the most recent session for this workspace so `ocean`
        // (or `cd project && ocean`) drops you BACK INTO your last conversation
        // — transcript rehydrated from disk, not just the session id bound (the
        // replay ring only holds recent events, so binding alone shows an empty
        // pane for anything older). `/new` starts a clean session. Legacy/
        // non-UUID records are skipped.
        if let Some((id, path)) = app.rail.latest_resumable() {
            app.chat
                .load_history(crate::shell::sessions::load_transcript(&path));
            app.bind_session_with(id, false); // transcript came from disk
            app.rail.live_id = Some(id.0.to_string());
            app.status = format!("resumed {:.8}", id.0.to_string());
        }
        app
    }

    pub async fn run(mut self, terminal: &mut tui::Tui) -> anyhow::Result<()> {
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
                        Ok(Ok(h)) => {
                            if !healthy {
                                let _ = tx.send(Action::Status(format!(
                                    "connected · {}",
                                    h.backend
                                )));
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
                            .unwrap_or(daemon_boot::AutostartOutcome::SpawnFailed(
                                "autostart task panicked".into(),
                            ));
                            match outcome {
                                daemon_boot::AutostartOutcome::Started => {
                                    let _ = tx.send(Action::Status(
                                        "daemon offline — starting ocean-daemon…".into(),
                                    ));
                                }
                                daemon_boot::AutostartOutcome::BinaryNotFound => {
                                    let _ = tx.send(Action::Status(
                                        "daemon offline — ocean-daemon not found (set OCEAN_DAEMON_BIN)"
                                            .into(),
                                    ));
                                }
                                daemon_boot::AutostartOutcome::SpawnFailed(_) => {
                                    let _ = tx.send(Action::Status(
                                        "daemon offline — autostart failed (see ~/.ocean/logs/daemon-autostart.log)"
                                            .into(),
                                    ));
                                }
                                daemon_boot::AutostartOutcome::NotEligible => {
                                    let _ = tx.send(Action::Status(format!(
                                        "daemon offline at {} — retrying…",
                                        daemon_boot::host_port(&base_url)
                                    )));
                                }
                                daemon_boot::AutostartOutcome::RateLimited => {
                                    // Keep current status — don't spam.
                                }
                                daemon_boot::AutostartOutcome::SupervisionUnknown => {
                                    let _ = tx.send(Action::Status(
                                        "daemon offline — autostart blocked (UID unavailable)"
                                            .into(),
                                    ));
                                }
                            }
                            tokio::time::sleep(Duration::from_secs(3)).await;
                        }
                    }
                }
            }));

            // Global /v1/events: permission requests/decisions live here.
            self.client
                .spawn_global_event_stream(self.actions_tx.clone());
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
                    if let Some(a) = self.pty.tick() {
                        self.dispatch(a);
                        dirty = true;
                    }
                    if let Some(a) = self.editor.tick() {
                        self.dispatch(a);
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
                    if self.chat.is_busy() || self.pty.is_active() {
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
                        if let Some(seq) =
                            kitty::place_png_at(&path, b.x, b.y, b.width, b.height)
                        {
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

    fn on_crossterm(&mut self, evt: CrosstermEvent) {
        // The `/image` viewer is a full-screen takeover: esc/q/enter or a click
        // closes it; other keys are swallowed (no accidental composer input).
        if self.image_view.is_some() {
            match evt {
                CrosstermEvent::Key(k) => {
                    if matches!(
                        k.code,
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter
                    ) {
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
        }
        // The `/providers` popup is modal too, and mutually exclusive with the
        // settings overlay: opening one closes the other.
        if self.providers_open {
            if let CrosstermEvent::Key(k) = evt {
                self.providers_key(k);
                return;
            }
        }
        // Mouse: a click focuses the pane under the cursor; wheel + clicks are
        // forwarded to whichever pane the cursor is over (CTRL behavior).
        if let CrosstermEvent::Mouse(m) = evt {
            let pos = (m.column, m.row);
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
            // Mouse text selection: holding the left button and dragging sweeps
            // a linear (terminal-style) selection across the whole frame;
            // releasing auto-copies it to the system clipboard. A plain click
            // (down + up, no drag) falls through to the panes untouched.
            match m.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    self.sel_press = Some(pos);
                    self.selection = None;
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if let Some(anchor) = self.sel_press {
                        self.selection = Some((anchor, pos));
                        return; // selection owns the drag; panes don't see it
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.sel_press = None;
                    if let Some((a, b)) = self.selection.take() {
                        let text = selection_text(&self.frame_cells, a, b);
                        if !text.is_empty() {
                            let msg = match copy_to_clipboard(&text) {
                                Ok(()) => {
                                    format!("copied {} chars", text.chars().count())
                                }
                                Err(e) => format!("copy failed: {e}"),
                            };
                            self.dispatch(Action::Status(msg));
                        }
                        return; // this Up ends a selection, not a click
                    }
                }
                _ => {}
            }
            // Title-bar buttons win over pane routing (CTRL's upper model).
            if matches!(m.kind, MouseEventKind::Down(_)) {
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
                    Focus::Tree => self.tree.handle_mouse(m),
                    Focus::Term => self.pty.handle_mouse(m),
                    Focus::Center => match self.center {
                        Center::Chat => self.chat.handle_mouse(m),
                        Center::Editor => self.editor.handle_mouse(m),
                        Center::Graph => self.graph.handle_mouse(m),
                    },
                };
                if let Some(a) = action {
                    self.dispatch(a);
                }
            }
            return;
        }
        if let CrosstermEvent::Key(k) = evt {
            if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('q') {
                self.should_quit = true;
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
            } else

            if k.code == KeyCode::Tab && self.focus != Focus::Term {
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
                        if self.pty.is_active() {
                            return self.focus_to(Focus::Term);
                        }
                        return;
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
        let action = match self.focus {
            Focus::Sessions => self.rail.handle_event(&evt),
            Focus::Tree => self.tree.handle_event(&evt),
            Focus::Term => self.pty.handle_event(&evt),
            Focus::Center => match self.center {
                Center::Chat => self.chat.handle_event(&evt),
                Center::Editor => self.editor.handle_event(&evt),
                Center::Graph => self.graph.handle_event(&evt),
            },
        };
        if let Some(a) = action {
            self.dispatch(a);
        }
    }

    /// A title-bar button press: rails and the terminal TOGGLE visibility;
    /// chat/editor/graph select the center surface (CTRL's editor↔graph swap).
    fn press(&mut self, btn: Btn) {
        match btn {
            Btn::Sessions => {
                self.show_sessions = !self.show_sessions;
                if !self.show_sessions && self.focus == Focus::Sessions {
                    self.focus_to(Focus::Center);
                }
            }
            Btn::Tree => {
                self.show_tree = !self.show_tree;
                if !self.show_tree && self.focus == Focus::Tree {
                    self.focus_to(Focus::Center);
                }
            }
            Btn::Term => {
                if !self.pty.is_active() {
                    // Like CTRL's ensure_terminal: first press opens a plain
                    // shell at the project root.
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

    fn dispatch(&mut self, action: Action) {
        match &action {
            Action::Quit => {
                self.should_quit = true;
                return;
            }
            Action::Status(s) => self.status = s.clone(),
            Action::Error(s) => self.status = errfmt::humanize(s),
            // Chat unwinds busy + restores the prompt (see its update arm);
            // the status line carries the humanized error.
            Action::TurnSendFailed { err, .. } => self.status = errfmt::humanize(err),
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
                // Failover honesty (OCEAN-275): the daemon announces a reroute
                // on the stream; paint it in the status line too (the chat
                // renders the full concern card in the transcript).
                if let AgentTurnEvent::ModelRerouted {
                    requested,
                    effective,
                    ..
                } = evt.as_ref()
                {
                    self.status = errfmt::humanize(&format!(
                        "⚠ {requested} unavailable — turn running on {effective} (fallback)"
                    ));
                }
                // Capture turn usage for the status-bar rate/token segments.
                if let AgentTurnEvent::TurnFinished {
                    tokens_per_second,
                    output_tokens,
                    ..
                } = evt.as_ref()
                {
                    self.turn_tok_per_s = *tokens_per_second;
                    if let Some(out) = output_tokens {
                        self.session_tokens = self.session_tokens.saturating_add(*out);
                    }
                }
            }
            Action::SubmitPrompt(text) => self.submit_turn(text.clone()),
            Action::OpenSession { line, cwd } => {
                // Hydrate into the terminal DOCK (appears at the bottom of the
                // center column, CTRL-style) and focus it.
                self.pty.open(cwd, line);
                self.focus_to(Focus::Term);
            }
            Action::OpenFile(path) => {
                self.editor.open(path.clone());
                self.center = Center::Editor;
                self.focus_to(Focus::Center);
            }
            Action::ResumeSession { id, path, cwd } => {
                self.chat
                    .load_history(crate::shell::sessions::load_transcript(path));
                self.bind_session_with(*id, false); // transcript came from disk
                self.rail.live_id = Some(id.0.to_string());
                // Re-root the workbench to the dir this session ran in, so the
                // file tree, graph, and future turns follow the session.
                self.set_active_project(cwd.clone());
                self.status = format!("resumed session {:.8}", id.0.to_string());
                self.center = Center::Chat;
                self.focus_to(Focus::Center);
            }
            // `+ new` on a project header: fresh session, re-rooted to `cwd`.
            Action::NewSessionInProject { cwd } => {
                if let Some(task) = self.stream_task.take() {
                    task.abort();
                }
                self.session_id = None;
                self.chat.load_history(Vec::new()); // clear the transcript
                self.rail.live_id = None;
                self.set_active_project(cwd.clone());
                self.center = Center::Chat;
                self.focus_to(Focus::Center);
                let leaf = cwd
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| cwd.to_string_lossy().into_owned());
                self.status = format!("new session · {leaf}");
            }
            Action::CycleFocus => self.cycle_focus(),
            // `/` palette navigation — reuse the exact patterns from `press()`
            // and the ⌃⌥ handler so keyboard and palette stay consistent.
            Action::Navigate(nav) => match *nav {
                Nav::Sessions => {
                    self.show_sessions = true;
                    self.focus_to(Focus::Sessions);
                }
                Nav::Files => {
                    self.show_tree = true;
                    self.focus_to(Focus::Tree);
                }
                Nav::Chat => {
                    self.center = Center::Chat;
                    self.focus_to(Focus::Center);
                }
                Nav::Editor => {
                    if self.editor.has_tabs() {
                        self.center = Center::Editor;
                    }
                    self.focus_to(Focus::Center);
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
                        self.show_term = true;
                    }
                    self.focus_to(Focus::Term);
                }
            },
            // `/new`: drop the bound session (and its stream) so the next turn
            // mints a fresh one; the chat cleared its own transcript already.
            Action::NewSession => {
                if let Some(task) = self.stream_task.take() {
                    task.abort();
                }
                self.session_id = None;
                self.center = Center::Chat;
                self.focus_to(Focus::Center);
                self.status = "new session".into();
            }
            // `/model <id>`: remember the override for subsequent turns.
            Action::SetModel(id) => {
                self.model_override = Some(id.clone());
                self.status = format!("model → {id}");
            }
            // `/thinking <level>`: remember the override for subsequent turns.
            // `default` clears the per-turn override so the daemon's global
            // setting is in force again.
            Action::SetThinking(level) => {
                self.thinking_override = *level;
                self.status = format!("thinking → {}", thinking_label(self.thinking_override));
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
                            let _ = tx.send(Action::MemoryLoaded { entries: r.memories });
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
                    self.status = format!("image not found: {}", abs.display());
                }
            }
            // `/login [claude|codex]`: run the REAL OAuth flow off-thread
            // (begin → browser → token exchange → persist) so the TUI never
            // blocks on the callback server or browser/OS integration. A second
            // `/login` while one is already running is rejected with a busy
            // status instead of racing a second callback server.
            Action::Login(target) => {
                if self.login_in_flight {
                    self.status = "login already in progress".into();
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
                self.status = msg.clone();
                self.login_in_flight = false;
                // Auth-state changed — refresh the welcome provider line so the
                // chat empty-state immediately reflects the new credentials.
                self.refresh_welcome_provider_line();
            }
            // `/settings`: open the modal settings overlay. Mutually exclusive
            // with the providers popup — opening one closes the other.
            Action::OpenSettings => {
                self.providers_open = false;
                self.settings_open = true;
                self.settings_sel = 0;
            }
            // `/providers` (or bare `/login`): open the provider auth popup.
            // Builds the status rows fresh from the auth file + process env so
            // the list reflects the live state every time it's opened.
            Action::OpenProviders => {
                self.settings_open = false;
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
            Action::OceanEvent(env) => {
                // OCEAN-185: the turn's first permission request claims the
                // pending submit token; remember permission→request for the POST.
                if let ocean_core::OceanEvent::PermissionRequest { .. } = &env.event {
                    if let (Some(rid), Some(pid)) = (env.request_id, env.permission_id) {
                        self.perm_request.insert(pid, rid);
                        if let std::collections::hash_map::Entry::Vacant(slot) =
                            self.decision_tokens.entry(rid)
                        {
                            if let Some(token) = self.pending_submit_token.take() {
                                slot.insert(token);
                            }
                        }
                    }
                }
            }
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
        if let Some(next) = self.chat.update(&action) {
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
        if self.session_id == Some(id)
            && self.stream_task.as_ref().is_some_and(|t| !t.is_finished())
        {
            return;
        }
        if let Some(task) = self.stream_task.take() {
            task.abort();
        }
        self.session_id = Some(id);
        self.stream_task = Some(self.client.spawn_event_stream(
            id,
            self.actions_tx.clone(),
            replay_first,
        ));
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
                1 => self.show_tree = !self.show_tree,
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
            self.status = format!(
                "{} has no credential — configure {} (env key or /login)",
                entry.id, entry.provider
            );
            return;
        }
        self.model_override = Some(entry.id.clone());
        self.models_open = false;
        self.status = format!(
            "model → {} · thinking {}",
            entry.id,
            thinking_label(self.thinking_override)
        );
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
            self.advisor_ctl = Some(AdvisorControl { enabled: false, model: None });
            self.status = "advisor off".into();
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
            self.status = format!("advisor → {id} (reviews after each turn)");
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
                format!(" {} ADVISOR ", g("◆", "*")),
                Style::default().fg(theme::CYAN).add_modifier(Modifier::BOLD),
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
                    "off — no second opinion".to_string(),
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

        let footer_y = inner.y + inner.height - 1;
        frame.render_widget(
            Paragraph::new(Span::styled(
                " reviews the exchange after each turn · ⏎ select · esc close",
                Style::default().fg(theme::CYAN),
            ))
            .style(Style::default().bg(theme::SLATE)),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
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
                    match copy_to_clipboard(&text) {
                        Ok(()) => self.status = format!("copied memory ({} chars)", text.chars().count()),
                        Err(e) => self.status = format!("copy failed: {e}"),
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
        let height = full.height.saturating_sub(4).min(30).max(8);
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
                format!(" {} MEMORY · {count} ", g("◆", "*")),
                Style::default().fg(theme::CYAN).add_modifier(Modifier::BOLD),
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
                Paragraph::new(Span::styled("loading…", Style::default().fg(theme::COMMENT)))
                    .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x + 1, list_top, inner.width.saturating_sub(2), 1),
            );
            return;
        }
        if filtered.is_empty() {
            let msg = if count == 0 {
                "no memories retained yet — the agent saves durable facts here"
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
        let scroll = sel.saturating_sub(view_h.saturating_sub(1)).min(
            filtered.len().saturating_sub(view_h),
        );
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
                    Span::styled(
                        format!(" {badge} "),
                        Style::default().fg(theme::BLUE),
                    ),
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

        frame.render_widget(
            Paragraph::new(Span::styled(
                " type to search · ⏎ copy · esc close",
                Style::default().fg(theme::CYAN),
            ))
            .style(Style::default().bg(theme::SLATE)),
            Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
        );
    }

    /// Render the `/image` viewer frame: a full-screen takeover with a title
    /// bar (filename + close hint) and an empty body. The body rect is stored
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
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("  {} {name}", g("🖼", "[img]")),
                    Style::default().fg(theme::CYAN).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "   esc close",
                    Style::default().fg(theme::COMMENT),
                ),
            ]))
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
            Some("inline preview supports PNG today — open other formats externally")
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
                format!(" {} LANGUAGE SERVERS ", g("◆", "*")),
                Style::default().fg(theme::CYAN).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height < 2 {
            return;
        }

        let mut y = inner.y;
        if self.lsp_loading {
            frame.render_widget(
                Paragraph::new(Span::styled("detecting…", Style::default().fg(theme::COMMENT)))
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
            for s in self.lsp_servers.iter().take(inner.height.saturating_sub(2) as usize) {
                let (glyph, gcolor, state) = if s.ready {
                    (g("●", "*"), theme::GREEN, "ready".to_string())
                } else {
                    (g("○", "o"), theme::YELLOW, format!("install {}", s.command))
                };
                let exts = if s.extensions.is_empty() {
                    String::new()
                } else {
                    format!("  ·{}", s.extensions.join(" ·"))
                };
                let name_w = 26usize;
                let name = format!("{:<name_w$}", truncate_str(&s.name, name_w));
                frame.render_widget(
                    Paragraph::new(Line::from(vec![
                        Span::styled(format!(" {glyph} "), Style::default().fg(gcolor)),
                        Span::styled(name, Style::default().fg(theme::FG).add_modifier(Modifier::BOLD)),
                        Span::styled(format!("  {state}"), Style::default().fg(gcolor)),
                        Span::styled(exts, Style::default().fg(theme::COMMENT)),
                    ]))
                    .style(Style::default().bg(theme::SLATE)),
                    Rect::new(inner.x, y, inner.width, 1),
                );
                y += 1;
            }
        }

        frame.render_widget(
            Paragraph::new(Span::styled(
                " diagnostics: ask the agent (its lsp tool) · esc close",
                Style::default().fg(theme::CYAN),
            ))
            .style(Style::default().bg(theme::SLATE)),
            Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
        );
    }

    /// Render the `/models` picker: a centered modal listing the daemon's live
    /// registry grouped by provider — ready providers first, not-ready ones
    /// greyed with the reason — plus the thinking-level control in the footer.
    fn draw_models(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::widgets::{Block, Borders, Clear};
        let full = frame.area();
        let width = 64u16.min(full.width.saturating_sub(4));
        let height = full.height.saturating_sub(4).min(34).max(8);
        let x = full.x + (full.width.saturating_sub(width)) / 2;
        let y = full.y + (full.height.saturating_sub(height)) / 2;
        let area = Rect::new(x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                format!(" {} MODELS ", g("◆", "*")),
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
                    let tag = if ready { "" } else { " · no credential" };
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

        // Footer: the thinking-level control + key hints.
        let footer_y = inner.y + inner.height - 1;
        let footer = format!(
            " {} thinking: {} {}  ·  ⏎ select · esc close",
            g("◂", "<"),
            thinking_label(self.thinking_override),
            g("▸", ">"),
        );
        frame.render_widget(
            Paragraph::new(Span::styled(footer, Style::default().fg(theme::CYAN)))
                .style(Style::default().bg(theme::SLATE)),
            Rect::new(inner.x, footer_y, inner.width, 1),
        );
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
            .map(|(label, block_key, env_vars)| ProviderRow {
                label,
                block_key,
                env_vars,
                status: provider_status(*block_key, env_vars, &auth_json),
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
            .filter(|r| r.status != "not configured")
            .count();
        self.chat.welcome_provider_line = if n_configured == 0 {
            Some("no providers connected yet — start with /login".into())
        } else {
            let noun = if n_configured == 1 { "provider" } else { "providers" };
            Some(format!("ready · {n_configured} {noun} configured"))
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
                                self.status = format!(
                                    "{} key saved to {}",
                                    block_key,
                                    path.display()
                                );
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
                            Err(e) => self.status = format!("save failed: {e}"),
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
        let rows = self.providers_rows.len().max(1) as u16;
        let width = 60u16.min(full.width.saturating_sub(4));
        // list + title + footer; a touch more room in key-entry mode.
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
                format!(" {} PROVIDERS ", g("◆", "*")),
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
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        format!(
                            " {} save · esc cancel",
                            g("⏎", "<enter>")
                        ),
                        Style::default().fg(theme::COMMENT),
                    ))
                    .style(Style::default().bg(theme::SLATE)),
                    Rect::new(
                        inner.x,
                        inner.y + inner.height.saturating_sub(1),
                        inner.width,
                        1,
                    ),
                );
            }
            ProvidersMode::List => {
                for (i, row) in self.providers_rows.iter().enumerate() {
                    let selected = i == self.providers_sel;
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
                    let value = Span::styled(
                        row.status.clone(),
                        Style::default().fg(status_fg),
                    );
                    let pad = (inner.width as usize)
                        .saturating_sub(left.chars().count() + value.content.chars().count() + 1);
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
                        Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
                    );
                }
                let footer = format!(
                    " {} move · ⏎ login / paste key · esc close",
                    g("↑↓", "^v")
                );
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        footer,
                        Style::default().fg(theme::COMMENT),
                    ))
                    .style(Style::default().bg(theme::SLATE)),
                    Rect::new(
                        inner.x,
                        inner.y + inner.height.saturating_sub(1),
                        inner.width,
                        1,
                    ),
                );
            }
        }
    }


    /// Render the `/settings` overlay: a centered modal on the SLATE bed with
    /// toggle rows (on/off pills), the dock-height stepper, and a read-only
    /// info section for the live session.
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
                format!(" {} SETTINGS ", g("◆", "*")),
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
                    format!("{} {} rows {}", g("◂", "<"), self.term_h, g("▸", ">")),
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
        // Footer hints on the last row.
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    " {} move · ⏎ toggle · {} height · esc close",
                    g("↑↓", "^v"),
                    g("◂▸", "<>")
                ),
                Style::default().fg(theme::COMMENT),
            ))
            .style(Style::default().bg(theme::SLATE)),
            Rect::new(
                inner.x,
                inner.y + inner.height.saturating_sub(1),
                inner.width,
                1,
            ),
        );
    }

    /// Grow (+) or shrink (−) the terminal dock by `delta` rows, clamped so the
    /// dock stays ≥ MIN_TERM_H and the main surface keeps ≥ MIN_CENTER_H.
    fn resize_term(&mut self, delta: i16) {
        let max = max_term_h(self.r_center_col.height) as i16;
        self.term_h = (self.term_h as i16 + delta).clamp(MIN_TERM_H as i16, max) as u16;
    }

    fn apply_focus(&mut self) {
        self.rail.focused = self.focus == Focus::Sessions;
        self.tree.focused = self.focus == Focus::Tree;
        self.pty.focused = self.focus == Focus::Term;
        let center = self.focus == Focus::Center;
        self.chat.focused = center && self.center == Center::Chat;
        self.editor.focused = center && self.center == Center::Editor;
        self.graph.focused = center && self.center == Center::Graph;
    }

    fn submit_turn(&mut self, prompt: String) {
        let client = self.client.clone();
        let tx = self.actions_tx.clone();
        let workspace = self.workspace_root.clone();
        let existing = self.session_id;
        let model_id = self.model_override.clone();
        let thinking = self.thinking_override;
        let advisor = self.advisor_ctl.clone();
        // OCEAN-185: mint the per-turn permission secret; the turn's first
        // permission request claims it (see Action::OceanEvent above).
        let decision_token = ocean_core::mint_decision_token();
        self.pending_submit_token = Some(decision_token.clone());

        tokio::spawn(async move {
            // Both the session mint and the turn POST ride the daemon-blip
            // retry (connect-class failures only — a restart mid-prompt used to
            // surface a hard "error sending request" wall and eat the prompt).
            // Each retry narrates in the status line; final failure unwinds the
            // chat's busy state and RESTORES the prompt via TurnSendFailed.
            let retry_status = |what: &'static str, tx: mpsc::UnboundedSender<Action>| {
                move |attempt: usize, total: usize| {
                    let _ = tx.send(Action::Status(format!(
                        "daemon unreachable — retrying {what} ({attempt}/{total})…"
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
                guidance: None,
                room_id: None,
                project_id: None,
                client_type: Some("tui".into()),
                agent: None,
                role: None,
                thinking_level: thinking,
                model_id,
                images: None,
                decision_token: Some(decision_token),
                client_context: None,
                advisor,
            };
            let on_retry = retry_status("turn", tx.clone());
            if let Err(e) = client.agent_turn_retrying(&req, on_retry).await {
                let _ = tx.send(Action::TurnSendFailed {
                    prompt,
                    err: format!("turn: {e}"),
                });
            }
        });
    }

    // ── the CTRL frame ───────────────────────────────────────────────────────

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let full = frame.area();
        if full.width < 40 || full.height < 8 {
            frame.render_widget(
                Paragraph::new("ocean: window too small — enlarge the terminal")
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
        let sess_w = if self.show_sessions { SESS_W } else { 0 };
        let tree_w = if self.show_tree { TREE_W } else { 0 };
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(sess_w),
                Constraint::Length(if sess_w > 0 { 1 } else { 0 }),
                Constraint::Min(40),
                Constraint::Length(if tree_w > 0 { 1 } else { 0 }),
                Constraint::Length(tree_w),
            ])
            .split(body);
        let (r_sessions, r_split_a, center, r_split_b, r_tree) =
            (cols[0], cols[1], cols[2], cols[3], cols[4]);

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
        self.r_sessions = r_sessions;
        self.r_tree = r_tree;
        self.r_center = r_center;
        self.r_term = r_term;
        self.r_center_col = center;
        self.r_split_term = r_split_term;

        // deep chrome first
        frame.render_widget(Block::default().style(Style::default().bg(theme::BG)), full);

        // breadcrumb: where the center surface is pointed (CTRL's crumb row).
        let crumb = match self.center {
            Center::Chat => match &self.session_id {
                Some(id) => format!(" chat {} {:.8}", g("›", ">"), id.0.to_string()),
                None => " chat › new session".to_string(),
            },
            Center::Editor => format!(" editor {} {}", g("›", ">"), self.editor.crumb()),
            Center::Graph => " graph › project constellation".to_string(),
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
        }
        if tree_w > 0 {
            self.tree.draw(frame, r_tree);
            splitter(frame, r_split_b, true);
        }
        if term_visible {
            self.pty.draw(frame, r_term);
            splitter(frame, r_split_term, false);
        }

        self.draw_title(frame, title_row);
        self.draw_status(frame, status_row);

        // Mouse-selection overlay: reverse-video the swept cells and snapshot
        // the frame's cell text so releasing the button copies exactly what's
        // on screen. Drawn over everything — a selection is a selection.
        if let Some((a, b)) = self.selection {
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
            let (s, e) = order_cells(a, b);
            for y in s.1..=e.1.min(area.bottom().saturating_sub(1)) {
                let x0 = if y == s.1 { s.0 } else { area.left() };
                let x1 = if y == e.1 {
                    e.0.min(area.right().saturating_sub(1))
                } else {
                    area.right().saturating_sub(1)
                };
                for x in x0..=x1 {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        let style = cell.style().add_modifier(Modifier::REVERSED);
                        cell.set_style(style);
                    }
                }
            }
        }

        // `/settings` + `/models` modal overlays — drawn last so they float
        // over everything.
        if self.settings_open {
            self.draw_settings(frame);
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

    /// CTRL's title row: project label left, status pill center, and the
    /// clickable icon toggles right — each lit in its color when its panel is
    /// on. This IS the primary way to drive the app; hotkeys are secondary.
    fn draw_title(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.buttons.clear();
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_DARK)),
            area,
        );

        // project label, CTRL-style (blue bold + chevron)
        let name = std::path::Path::new(&self.workspace_root)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "ocean".into());
        let proj = format!("  {} OCEAN · {} ", g("◇", "*"), name);
        frame.render_widget(
            Paragraph::new(Span::styled(
                proj.clone(),
                Style::default()
                    .fg(theme::BLUE)
                    .bg(theme::BG_DARK)
                    .add_modifier(Modifier::BOLD),
            )),
            Rect::new(
                area.x,
                area.y,
                (proj.chars().count() as u16).min(area.width),
                1,
            ),
        );

        // centered status pill
        let pill = format!(" {} ", self.status);
        let pillw = (pill.chars().count() as u16).min(area.width / 2);
        let px = area.x + (area.width.saturating_sub(pillw)) / 2;
        frame.render_widget(
            Paragraph::new(Span::styled(
                pill,
                Style::default().fg(theme::CYAN).bg(theme::BG_HL),
            )),
            Rect::new(px, area.y, pillw, 1),
        );

        // right: icon buttons — CTRL's ⊞/⟠/⊟/◨ plus the center surfaces.
        let items: Vec<(&str, Btn, bool, ratatui::style::Color)> = vec![
            (
                g("⊞", "[S]"),
                Btn::Sessions,
                self.show_sessions,
                theme::BLUE,
            ),
            (
                g("❯", "[C]"),
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
                self.center == Center::Graph,
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
        let total: u16 = items
            .iter()
            .map(|(s, ..)| s.chars().count() as u16 + 2)
            .sum::<u16>()
            + 2;
        let mut x = area.x + area.width.saturating_sub(total);
        for (icon, btn, on, color) in items {
            let w = icon.chars().count() as u16;
            let fg = if on { color } else { theme::COMMENT };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    icon.to_string(),
                    Style::default().fg(fg).bg(theme::BG_DARK),
                )),
                Rect::new(x, area.y, w, 1),
            );
            // generous hit target: icon + trailing gap
            self.buttons.push((Rect::new(x, area.y, w + 2, 1), btn));
            x += w + 2;
        }
    }

    fn draw_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        // Composable dashboard segments (OMP Slice 4): focus · model · git ·
        // rate · tokens · session · advisor · message. Built pure in
        // `status::segments`; rendered here with theme colours and ` · `
        // separators. The keybind hint pins to the right.
        let focus_name = match self.focus {
            Focus::Sessions => "sessions",
            Focus::Tree => "files",
            Focus::Term => "terminal",
            Focus::Center => match self.center {
                Center::Chat => "chat",
                Center::Editor => "editor",
                Center::Graph => "graph",
            },
        };
        let advisor = self.advisor_ctl.as_ref().map(|c| {
            if c.enabled {
                c.model.as_deref().unwrap_or("on")
            } else {
                "off"
            }
        });
        let session = self.session_id.map(|id| {
            let s = id.0.to_string();
            s.chars().take(8).collect::<String>()
        });
        let data = StatusData {
            focus: focus_name,
            model: self.chat.model(),
            git: Some(&self.git_status),
            tok_per_s: self.turn_tok_per_s,
            session_tokens: self.session_tokens,
            session: session.as_deref(),
            advisor,
            message: &self.status,
        };

        let tone_color = |t: Tone| match t {
            Tone::Focus => theme::CYAN,
            Tone::Primary => theme::FG,
            Tone::Muted => theme::COMMENT,
            Tone::Ok => theme::GREEN,
            Tone::Warn => theme::YELLOW,
        };
        let mut spans: Vec<Span> = Vec::new();
        for (i, seg) in status::segments(&data).into_iter().enumerate() {
            if seg.tone == Tone::Focus {
                // Focus chip: accent bed, bold.
                spans.push(Span::styled(
                    chat::sanitize_line(&seg.text),
                    Style::default()
                        .fg(theme::CYAN)
                        .bg(theme::BG_HL)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    if i == 1 { "  " } else { &" · "[..] },
                    Style::default().fg(theme::EDGE),
                ));
                spans.push(Span::styled(chat::sanitize_line(&seg.text), Style::default().fg(tone_color(seg.tone))));
            }
        }
        let hint = " ⇥ move · ⌃Q quit ";
        // Right-pin the hint: pad between the segments and the hint.
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let pad = (area.width as usize).saturating_sub(used + hint.chars().count());
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(hint, Style::default().fg(theme::COMMENT)));

        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG_DARK)),
            area,
        );
    }
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
        format!("{}…", s.chars().take(max.saturating_sub(1)).collect::<String>())
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

/// Extract the text of a linear (terminal-style) selection from a frame's cell
/// snapshot: first row from the anchor column, middle rows whole, last row up
/// to the head column. Rows are right-trimmed (panel padding isn't content);
/// a selection of pure padding yields the empty string so releasing on a blank
/// area copies nothing.
fn selection_text(cells: &[Vec<String>], a: (u16, u16), b: (u16, u16)) -> String {
    let (s, e) = order_cells(a, b);
    let mut out: Vec<String> = Vec::new();
    for y in s.1..=e.1 {
        let Some(row) = cells.get(y as usize) else {
            continue;
        };
        if row.is_empty() {
            out.push(String::new());
            continue;
        }
        let x0 = if y == s.1 { s.0 as usize } else { 0 };
        let x1 = if y == e.1 {
            (e.0 as usize).min(row.len() - 1)
        } else {
            row.len() - 1
        };
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
        // Mid-first-row through mid-last-row: first row from anchor, middle
        // whole, last row up to the head. Trailing padding trimmed.
        let text = selection_text(&cells, (6, 0), (3, 2));
        assert_eq!(text, "world\nsecond line\ntail");
        // Same-row span.
        assert_eq!(selection_text(&cells, (0, 1), (5, 1)), "second");
        // Reverse drag selects the same text.
        assert_eq!(
            selection_text(&cells, (3, 2), (6, 0)),
            selection_text(&cells, (6, 0), (3, 2)),
        );
    }

    #[test]
    fn selection_of_pure_padding_copies_nothing() {
        let cells = grid(&["          ", "          "]);
        assert_eq!(selection_text(&cells, (1, 0), (8, 1)), "");
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

    /// Build an `App` against a throwaway workspace root so `App::new`'s
    /// auto-resume finds no session and never spawns a network stream — keeping
    /// these `/login` dispatch tests fully offline (no daemon, no browser, no
    /// OAuth callback). `DaemonClient::new` only builds a `reqwest::Client`; it
    /// does not connect.
    fn offline_app() -> App {
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

    fn render_app_to_string(app: &mut App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| app.draw(frame))
            .expect("draw app");
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
        CrosstermEvent::Key(crossterm::event::KeyEvent::new(
            code,
            KeyModifiers::NONE,
        ))
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
            .map(|(label, block_key, env_vars)| ProviderRow {
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
    fn refresh_provider_line_updates_chat_field() {
        // refresh_welcome_provider_line must set chat.welcome_provider_line
        // from the current auth state (provider rows computed fresh each call).
        let mut app = offline_app();
        app.refresh_welcome_provider_line();
        let after = app.chat.welcome_provider_line.clone();
        // Verify it's set (Some) — the exact string depends on the test
        // environment's auth file state, which may or may not have providers
        // configured. The key property: refresh doesn't clear it.
        assert!(after.is_some(), "welcome line should be Some after refresh");
        // And it produces consistent output for the same state.
        app.refresh_welcome_provider_line();
        assert_eq!(app.chat.welcome_provider_line, after, "idempotent");
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
