use std::{
    collections::HashMap,
    env, fs,
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ocean_core::{
    EventEnvelope, HealthResponse, OceanEvent, PermissionControlResponse,
    PermissionDecision as CorePermissionDecision, PermissionDecisionRequest, PermissionId,
    PermissionStatus, PermissionsResponse, PromptRequest, RequestControlResponse,
    RequestCreateResponse, RequestId, RequestState, RequestStatus, RequestsResponse, SessionDetail,
    SessionId, SessionResponse, SessionRunState, SessionSummary,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:4780";
const DEFAULT_MESH_REFRESH_MS: u64 = 1000;
const DEFAULT_DAEMON_REFRESH: Duration = Duration::from_secs(5);
const ACTION_POLL: Duration = Duration::from_millis(100);
const REDRAW_MAX_IDLE: Duration = Duration::from_millis(250);
const ACTIVITY_CAP: usize = 36;
const TRANSCRIPT_CAP: usize = 120;
const REQUEST_CAP: usize = 64;
const SESSION_CAP: usize = 64;
const FEED_CAP: usize = 300;
const INBOX_CAP: usize = 120;
const ACTIVE_AGENT_MS: i64 = 2 * 60 * 1000;
const AWAY_AGENT_MS: i64 = 15 * 60 * 1000;

#[derive(Debug, Parser)]
#[command(name = "ocean-tui", about = "Ocean daemon steering + TIDES-MESH TUI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(
        long,
        env = "OCEAN_DAEMON_URL",
        default_value = DEFAULT_DAEMON_URL
    )]
    url: String,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read-only TIDES-MESH parity view over file-backed state.
    Mesh(MeshCli),
}

#[derive(Debug, Parser, Clone)]
struct MeshCli {
    #[arg(long, default_value = ".")]
    root: PathBuf,

    #[arg(long, env = "TIDES_MESH_AGENT")]
    agent: Option<String>,

    #[arg(long, env = "PIMESH_TAB", value_enum, default_value_t = MeshTab::Board)]
    tab: MeshTab,

    #[arg(long, env = "PIMESH_REFRESH_MS", default_value_t = DEFAULT_MESH_REFRESH_MS)]
    refresh_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MeshTab {
    Board,
    Events,
    Inbox,
    Agents,
}

impl MeshTab {
    fn label(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::Events => "events",
            Self::Inbox => "inbox",
            Self::Agents => "agents",
        }
    }

    fn all() -> [Self; 4] {
        [Self::Board, Self::Events, Self::Inbox, Self::Agents]
    }

    fn next(self) -> Self {
        match self {
            Self::Board => Self::Events,
            Self::Events => Self::Inbox,
            Self::Inbox => Self::Agents,
            Self::Agents => Self::Board,
        }
    }

    fn from_digit(ch: char) -> Option<Self> {
        match ch {
            '1' => Some(Self::Board),
            '2' => Some(Self::Events),
            '3' => Some(Self::Inbox),
            '4' => Some(Self::Agents),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
enum HealthState {
    Loading,
    Ready(HealthResponse),
    Error(String),
}

#[derive(Debug, Deserialize)]
struct SessionsResponse {
    ok: bool,
    #[serde(default)]
    sessions: Vec<SessionSummary>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug)]
enum Action {
    Quit,
    Tick,
    RefreshAll,
    RefreshSessions,
    SendPrompt,
    CancelActive,
    ApproveLatestPermission,
    DenyLatestPermission,
    ComposerNewline,
    ToggleHelp,
    NextRoom,
    SelectRoom(WorkspaceRoom),
    SessionPrev,
    SessionNext,
    InputChar(char),
    Backspace,
    ClearInput,
    StreamStatus(String),
    StreamEvent(EventEnvelope),
    MeshRefresh,
    MeshTogglePause,
    MeshNextTab,
    MeshSelectTab(MeshTab),
}

#[derive(Debug)]
enum UiMode {
    Daemon(Box<DaemonApp>),
    Mesh(MeshApp, Box<MeshState>),
}

#[derive(Debug)]
struct DaemonApp {
    url: String,
    root: PathBuf,
    mesh_agent: String,
    active_room: WorkspaceRoom,
    show_help: bool,
    health: HealthState,
    sessions: Vec<SessionSummary>,
    selected_session_index: usize,
    selected_session_detail: Option<SessionDetail>,
    requests: Vec<RequestStatus>,
    pending_permissions: Vec<PendingPermission>,
    active_request_id: Option<RequestId>,
    streaming_request_id: Option<RequestId>,
    input: String,
    activity: Vec<String>,
    transcript: Vec<String>,
    tool_timeline: Vec<ToolTimelineEntry>,
    diff_snippets: Vec<String>,
    support: WorkspaceSupportState,
    status: String,
    stream_status: String,
    last_checked: Option<Instant>,
    refresh_every: Duration,
}

impl DaemonApp {
    fn new(url: String, root: PathBuf) -> Self {
        Self {
            url,
            root,
            mesh_agent: default_mesh_agent(),
            active_room: WorkspaceRoom::Orchestrator,
            show_help: false,
            health: HealthState::Loading,
            sessions: Vec::new(),
            selected_session_index: 0,
            selected_session_detail: None,
            requests: Vec::new(),
            pending_permissions: Vec::new(),
            active_request_id: None,
            streaming_request_id: None,
            input: String::new(),
            activity: Vec::new(),
            transcript: vec![
                "Ocean TUI coding-agent workspace".to_string(),
                "Tab cycles TIDES-MESH rooms; F1-F7 jumps directly to a room.".to_string(),
                "Enter sends. Ctrl-J inserts a newline in the composer.".to_string(),
                "Shift-Y / Shift-N handle tool approvals; Up/Down picks the session to inspect or resume."
                    .to_string(),
            ],
            tool_timeline: Vec::new(),
            diff_snippets: Vec::new(),
            support: WorkspaceSupportState::default(),
            status: "starting workspace shell".to_string(),
            stream_status: "connecting".to_string(),
            last_checked: None,
            refresh_every: DEFAULT_DAEMON_REFRESH,
        }
    }

    fn status_label(&self) -> (&'static str, Color) {
        match &self.health {
            HealthState::Loading => ("checking", Color::Yellow),
            HealthState::Ready(res) if res.ok => ("ok", Color::Green),
            HealthState::Ready(_) => ("error", Color::Red),
            HealthState::Error(_) => ("offline", Color::Red),
        }
    }

    fn health_summary(&self) -> String {
        match &self.health {
            HealthState::Loading => "health: checking".to_string(),
            HealthState::Ready(res) => format!(
                "health: {}  service: {}  backend: {}",
                if res.ok { "ok" } else { "error" },
                res.service,
                res.backend
            ),
            HealthState::Error(err) => format!("health: offline  error: {err}"),
        }
    }

    fn checked_text(&self) -> String {
        checked_text(self.last_checked)
    }

    fn selected_session_slot(&self) -> usize {
        self.selected_session_index.min(self.sessions.len())
    }

    fn selected_session(&self) -> Option<&SessionSummary> {
        self.selected_session_slot()
            .checked_sub(1)
            .and_then(|idx| self.sessions.get(idx))
    }

    fn selected_session_id(&self) -> Option<SessionId> {
        self.selected_session().map(|session| session.id)
    }

    fn selected_session_label(&self) -> String {
        match self.selected_session() {
            Some(session) => format!(
                "resume {} · {} turns · {}",
                short_id(session.id),
                session.turns,
                compact_text(&session.title, 20)
            ),
            None => "new session".to_string(),
        }
    }

    fn cycle_session(&mut self, delta: isize) {
        let slots = self.sessions.len() + 1;
        if slots == 0 {
            self.selected_session_index = 0;
            return;
        }
        let current = self.selected_session_slot() as isize;
        let next = (current + delta).clamp(0, self.sessions.len() as isize);
        self.selected_session_index = next as usize;
        self.status = format!("session target: {}", self.selected_session_label());
    }

    fn push_activity(&mut self, line: String) {
        push_bounded(&mut self.activity, line, ACTIVITY_CAP);
    }

    fn push_transcript(&mut self, line: String) {
        push_bounded(&mut self.transcript, line, TRANSCRIPT_CAP);
    }

    fn push_transcript_if_new(&mut self, line: String) {
        if self.transcript.last() != Some(&line) {
            self.push_transcript(line);
        }
    }

    fn append_assistant_delta(&mut self, request_id: Option<RequestId>, text: &str) {
        let chunk = text.replace(['\r', '\n'], " ");
        if chunk.trim().is_empty() {
            return;
        }
        let prefix = match request_id {
            Some(request_id) => format!("← [{}] ", short_id(request_id)),
            None => "← ".to_string(),
        };
        if self.streaming_request_id == request_id {
            if let Some(last) = self.transcript.last_mut() {
                if last.starts_with(&prefix) {
                    last.push_str(&chunk);
                    return;
                }
            }
        }
        self.streaming_request_id = request_id;
        self.push_transcript(format!("{prefix}{chunk}"));
    }

    fn set_sessions(&mut self, sessions: Vec<SessionSummary>) {
        self.sessions = sessions.into_iter().take(SESSION_CAP).collect();
        self.selected_session_index = self.selected_session_index.min(self.sessions.len());
        self.selected_session_detail = None;
    }

    fn set_requests(&mut self, requests: Vec<RequestStatus>) {
        self.requests = requests.into_iter().take(REQUEST_CAP).collect();
        self.active_request_id = self
            .requests
            .iter()
            .find(|request| !request.state.is_terminal())
            .map(|request| request.request_id);
        self.reconcile_pending_permissions();
    }

    fn event_lines(&self, limit: usize) -> Vec<Line<'static>> {
        if self.activity.is_empty() {
            return vec![Line::from("Waiting for live events...")];
        }
        self.activity
            .iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|line| Line::from(line.clone()))
            .collect()
    }

    fn transcript_lines(&self) -> Vec<Line<'static>> {
        self.transcript
            .iter()
            .rev()
            .take(80)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|line| Line::from(line.clone()))
            .collect()
    }

    fn session_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(format!(
            "{} new session",
            if self.selected_session_slot() == 0 {
                ">"
            } else {
                " "
            }
        ))];
        if self.sessions.is_empty() {
            lines.push(Line::from("  no saved sessions yet"));
            lines.push(Line::from(
                "  Up/Down selects a session; detail stays in sync below",
            ));
            return lines;
        }
        lines.extend(
            self.sessions
                .iter()
                .take(10)
                .enumerate()
                .map(|(idx, session)| {
                    Line::from(format!(
                        "{} [{}] {} turns {}",
                        if self.selected_session_slot() == idx + 1 {
                            ">"
                        } else {
                            " "
                        },
                        short_id(session.id),
                        session.turns,
                        compact_text(&session.title, 34)
                    ))
                }),
        );
        if let Some(detail) = self.selected_session_detail.as_ref() {
            lines.push(Line::from(""));
            lines.extend(
                Self::session_detail_lines(detail)
                    .into_iter()
                    .map(Line::from),
            );
        } else if self.selected_session_slot() > 0 {
            lines.push(Line::from("  session detail loading..."));
        } else {
            lines.push(Line::from(
                "  select a session with Up/Down to inspect detail",
            ));
        }
        lines
    }

    fn session_detail_lines(detail: &SessionDetail) -> Vec<String> {
        let mut lines = vec![
            format!(
                "  selected [{}] {}",
                short_id(detail.id),
                compact_text(&detail.title, 38)
            ),
            format!(
                "  {} · {} turns · {} · resumable {}",
                compact_text(&format!("{}/{}", detail.provider, detail.model), 24),
                detail.turns,
                session_run_state_label(detail.state),
                if detail.resumable { "yes" } else { "no" }
            ),
            format!(
                "  transcript {} · messages {} · tools {} · active {} · pending {}",
                detail.transcript.len(),
                detail.messages.len(),
                detail.tool_context.len(),
                detail.active_requests.len(),
                detail.pending_permissions.len()
            ),
        ];
        if let Some(entry) = detail
            .transcript
            .iter()
            .rev()
            .find(|entry| !entry.text.trim().is_empty() || entry.tool_name.is_some())
        {
            let preview = if !entry.text.trim().is_empty() {
                compact_text(&entry.text, 52)
            } else if let Some(tool_name) = entry.tool_name.as_deref() {
                compact_text(tool_name, 52)
            } else {
                compact_text(&entry.role, 52)
            };
            lines.push(format!(
                "  recent {}: {}",
                compact_text(&entry.role, 10),
                preview
            ));
        }
        lines
    }

    fn request_lines(&self) -> Vec<Line<'static>> {
        if self.requests.is_empty() {
            return vec![Line::from("No requests yet.")];
        }
        self.requests
            .iter()
            .take(8)
            .map(|request| {
                let marker = if Some(request.request_id) == self.active_request_id {
                    "*"
                } else {
                    " "
                };
                let permission = request
                    .permission_id
                    .map(|id| format!(" perm:{}", short_id(id)))
                    .unwrap_or_default();
                let message = request.message.as_deref().unwrap_or("");
                Line::from(format!(
                    "{marker} {}  {}{}  {}",
                    short_id(request.request_id),
                    state_label(request.state),
                    permission,
                    compact_text(message, 42)
                ))
            })
            .collect()
    }

    fn tool_timeline_lines(&self, limit: usize) -> Vec<Line<'static>> {
        if self.tool_timeline.is_empty() {
            return vec![Line::from(
                "No tool calls yet. Waiting for SSE tool events.",
            )];
        }
        self.tool_timeline
            .iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|entry| {
                let left = format!(
                    "{:>4} [{}] {} {}",
                    time_ago(Some(&entry.at.to_rfc3339())),
                    entry
                        .request_id
                        .map(short_id)
                        .unwrap_or_else(|| "stream".to_string()),
                    compact_text(&entry.tool, 12),
                    compact_text(&entry.phase, 10)
                );
                Line::from(format!("{} {}", left, compact_text(&entry.message, 54)))
            })
            .collect()
    }

    fn diff_lines(&self, limit: usize) -> Vec<Line<'static>> {
        if self.diff_snippets.is_empty() {
            return vec![
                Line::from("No diff/edit payloads captured yet."),
                Line::from(
                    "Waiting for tool output that looks like git diff, patch, or exact file edits.",
                ),
            ];
        }
        self.diff_snippets
            .iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|line| Line::from(line.clone()))
            .collect()
    }

    fn composer_lines(&self) -> Vec<Line<'static>> {
        let prompt = if self.input.is_empty() {
            "".to_string()
        } else {
            self.input.clone()
        };
        let mut lines = Vec::new();
        for (idx, line) in prompt.split('\n').enumerate() {
            lines.push(Line::from(format!(
                "{} {}",
                if idx == 0 { ">" } else { "·" },
                line
            )));
        }
        if lines.is_empty() {
            lines.push(Line::from("> "));
        }
        lines.push(Line::from(format!(
            "session target: {}",
            self.selected_session_label()
        )));
        lines
    }

    fn room_lines(&self, width: usize) -> Vec<Line<'static>> {
        render_workspace_room_lines(self, width)
    }

    fn support_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(format!(
            "daemon {} · stream {} · checked {}",
            self.status_label().0,
            self.stream_status,
            self.checked_text()
        ))];
        if let Some(generated) = self.support.unified.generated_at.as_deref() {
            lines.push(Line::from(format!(
                "unified cache {}",
                time_ago(Some(generated))
            )));
        } else {
            lines.push(Line::from("unified cache unavailable"));
        }
        lines.push(Line::from(format!(
            "mesh tasks {} · feed {} · agents {}",
            self.support.mesh.tasks.len(),
            self.support.mesh.feed.len(),
            self.support.mesh.agents.len()
        )));
        if let Some(source_count) = self
            .support
            .unified
            .sources
            .as_object()
            .map(|map| map.len())
        {
            lines.push(Line::from(format!("sidecar sources {}", source_count)));
        }
        lines
    }

    fn pending_permission_lines(&self) -> Vec<Line<'static>> {
        if self.pending_permissions.is_empty() {
            return vec![
                Line::from("No approvals waiting."),
                Line::from("Shift-Y approve / Shift-N deny when a request appears."),
            ];
        }
        let mut lines = vec![Line::from("Latest (*) uses Shift-Y approve · Shift-N deny")];
        lines.extend(
            self.pending_permissions
                .iter()
                .take(2)
                .enumerate()
                .flat_map(|(idx, pending)| {
                    let marker = if idx == 0 { "*" } else { " " };
                    let age = pending
                        .updated_at
                        .as_ref()
                        .map(|ts| {
                            let stamp = ts.to_rfc3339();
                            time_ago(Some(&stamp))
                        })
                        .unwrap_or_else(|| "—".to_string());
                    let request = pending
                        .request_id
                        .map(|id| format!(" req:{}", short_id(id)))
                        .unwrap_or_default();
                    [
                        Line::from(format!(
                            "{marker} {} [{}]{} {}",
                            compact_text(&pending.tool, 14),
                            short_id(pending.permission_id),
                            request,
                            age
                        )),
                        Line::from(format!("  {}", compact_text(&pending.reason, 56))),
                        Line::from(format!(
                            "  args {}",
                            compact_text(&pending.args_preview, 52)
                        )),
                    ]
                }),
        );
        lines
    }

    fn latest_pending_permission(&self) -> Option<&PendingPermission> {
        self.pending_permissions.first()
    }

    fn set_permissions(&mut self, permissions: Vec<PermissionStatus>) {
        self.pending_permissions = permissions
            .into_iter()
            .map(PendingPermission::from_status)
            .collect();
        self.sort_pending_permissions();
    }

    fn upsert_pending_permission(&mut self, pending: PendingPermission) {
        if let Some(existing) = self
            .pending_permissions
            .iter_mut()
            .find(|item| item.permission_id == pending.permission_id)
        {
            *existing = pending;
        } else {
            self.pending_permissions.push(pending);
        }
        self.sort_pending_permissions();
    }

    fn remove_pending_permission(&mut self, permission_id: PermissionId) {
        self.pending_permissions
            .retain(|pending| pending.permission_id != permission_id);
    }

    fn reconcile_pending_permissions(&mut self) {
        let waiting: Vec<&RequestStatus> = self
            .requests
            .iter()
            .filter(|request| request.state == RequestState::WaitingForPermission)
            .collect();
        self.pending_permissions.retain(|pending| {
            waiting
                .iter()
                .any(|request| request.permission_id == Some(pending.permission_id))
        });
        for request in waiting {
            let Some(permission_id) = request.permission_id else {
                continue;
            };
            if let Some(existing) = self
                .pending_permissions
                .iter_mut()
                .find(|pending| pending.permission_id == permission_id)
            {
                existing.request_id = Some(request.request_id);
                existing.updated_at = request.updated_at;
                if existing.reason.is_empty() {
                    existing.reason = request
                        .message
                        .clone()
                        .unwrap_or_else(|| "waiting for operator decision".to_string());
                }
                if existing.args_preview.is_empty() {
                    existing.args_preview = request.message.clone().unwrap_or_default();
                }
            } else {
                self.pending_permissions.push(PendingPermission {
                    permission_id,
                    request_id: Some(request.request_id),
                    tool: "permission request".to_string(),
                    reason: request
                        .message
                        .clone()
                        .unwrap_or_else(|| "waiting for operator decision".to_string()),
                    args_preview: request.message.clone().unwrap_or_default(),
                    updated_at: request.updated_at,
                });
            }
        }
        self.sort_pending_permissions();
    }

    fn sort_pending_permissions(&mut self) {
        self.pending_permissions.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then(b.permission_id.cmp(&a.permission_id))
        });
    }

    fn push_tool_timeline(
        &mut self,
        request_id: Option<RequestId>,
        tool: &str,
        phase: &str,
        message: String,
        at: DateTime<Utc>,
    ) {
        push_bounded(
            &mut self.tool_timeline,
            ToolTimelineEntry {
                at,
                request_id,
                tool: tool.to_string(),
                phase: phase.to_string(),
                message,
            },
            FEED_CAP,
        );
    }

    fn capture_diff_excerpt(&mut self, text: &str) {
        if let Some(snippet) = extract_diff_excerpt(text) {
            push_bounded(&mut self.diff_snippets, snippet, ACTIVITY_CAP);
        }
    }

    fn refresh_support_state(&mut self) {
        self.support = load_workspace_support_state(&self.root, &self.mesh_agent)
            .unwrap_or_else(|_| WorkspaceSupportState::default());
    }

    fn cancellable_request_id(&self) -> Option<RequestId> {
        self.requests
            .iter()
            .find(|request| request.state.is_cancellable())
            .map(|request| request.request_id)
    }
}

#[derive(Debug, Clone)]
struct PendingPermission {
    permission_id: PermissionId,
    request_id: Option<RequestId>,
    tool: String,
    reason: String,
    args_preview: String,
    updated_at: Option<DateTime<Utc>>,
}

impl PendingPermission {
    fn from_status(status: PermissionStatus) -> Self {
        Self {
            permission_id: status.permission_id,
            request_id: Some(status.request_id),
            tool: status.tool,
            reason: status.reason,
            args_preview: serde_json::to_string(&status.args)
                .map(|json| compact_text(&json, 56))
                .unwrap_or_else(|_| "{}".to_string()),
            updated_at: Some(status.created_at),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkspaceRoom {
    Orchestrator,
    Writers,
    Rev,
    TideDash,
    WorkOps,
    WorldMap,
    PM,
}

impl WorkspaceRoom {
    fn label(self) -> &'static str {
        match self {
            Self::Orchestrator => "Orchestrator",
            Self::Writers => "Writers",
            Self::Rev => "Rev",
            Self::TideDash => "TideDash",
            Self::WorkOps => "WorkOps",
            Self::WorldMap => "WorldMap",
            Self::PM => "PM",
        }
    }

    fn all() -> [Self; 7] {
        [
            Self::Orchestrator,
            Self::Writers,
            Self::Rev,
            Self::TideDash,
            Self::WorkOps,
            Self::WorldMap,
            Self::PM,
        ]
    }

    fn next(self) -> Self {
        match self {
            Self::Orchestrator => Self::Writers,
            Self::Writers => Self::Rev,
            Self::Rev => Self::TideDash,
            Self::TideDash => Self::WorkOps,
            Self::WorkOps => Self::WorldMap,
            Self::WorldMap => Self::PM,
            Self::PM => Self::Orchestrator,
        }
    }

    fn from_function_key(code: KeyCode) -> Option<Self> {
        match code {
            KeyCode::F(1) => Some(Self::Orchestrator),
            KeyCode::F(2) => Some(Self::Writers),
            KeyCode::F(3) => Some(Self::Rev),
            KeyCode::F(4) => Some(Self::TideDash),
            KeyCode::F(5) => Some(Self::WorkOps),
            KeyCode::F(6) => Some(Self::WorldMap),
            KeyCode::F(7) => Some(Self::PM),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct ToolTimelineEntry {
    at: DateTime<Utc>,
    request_id: Option<RequestId>,
    tool: String,
    phase: String,
    message: String,
}

#[derive(Debug, Clone, Default)]
struct WorkspaceSupportState {
    mesh: MeshState,
    unified: UnifiedSnapshot,
}

#[derive(Debug, Clone)]
struct UnifiedSnapshot {
    generated_at: Option<String>,
    summary: Value,
    sources: Value,
    events: Vec<UnifiedEvent>,
}

impl Default for UnifiedSnapshot {
    fn default() -> Self {
        Self {
            generated_at: None,
            summary: Value::Null,
            sources: Value::Null,
            events: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct UnifiedStateFile {
    #[serde(default)]
    generated_at: Option<String>,
    #[serde(default)]
    summary: Value,
    #[serde(default)]
    sources: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct UnifiedEvent {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    summary: Option<Value>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug)]
struct MeshApp {
    root: PathBuf,
    agent: String,
    active_tab: MeshTab,
    paused: bool,
    refresh_every: Duration,
    last_refresh: Option<Instant>,
    status: String,
}

impl MeshApp {
    fn new(cli: MeshCli) -> Self {
        Self {
            root: cli.root,
            agent: cli.agent.unwrap_or_else(default_mesh_agent),
            active_tab: cli.tab,
            paused: false,
            refresh_every: Duration::from_millis(cli.refresh_ms.max(100)),
            last_refresh: None,
            status: "starting".to_string(),
        }
    }

    fn checked_text(&self) -> String {
        checked_text(self.last_refresh)
    }
}

#[derive(Debug)]
struct AppState {
    mode: UiMode,
    last_draw: Instant,
    dirty: bool,
}

impl AppState {
    fn new(mode: UiMode) -> Self {
        Self {
            mode,
            last_draw: Instant::now() - REDRAW_MAX_IDLE,
            dirty: true,
        }
    }

    fn draw_due(&self) -> bool {
        self.dirty || self.last_draw.elapsed() >= REDRAW_MAX_IDLE
    }

    fn mark_drawn(&mut self) {
        self.last_draw = Instant::now();
        self.dirty = false;
    }
}

struct DaemonClient {
    http: reqwest::blocking::Client,
}

impl DaemonClient {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::blocking::Client::builder()
                .timeout(Duration::from_secs(120))
                .build()
                .context("build daemon client")?,
        })
    }

    fn health(&self, base_url: &str) -> Result<HealthResponse, String> {
        let url = format!("{}/health", base_url.trim_end_matches('/'));
        self.http
            .get(url)
            .send()
            .and_then(|res| res.error_for_status())
            .map_err(|err| err.to_string())?
            .json::<HealthResponse>()
            .map_err(|err| err.to_string())
    }

    fn sessions(&self, base_url: &str) -> Result<SessionsResponse, String> {
        let url = format!("{}/v1/sessions", base_url.trim_end_matches('/'));
        self.http
            .get(url)
            .send()
            .and_then(|res| res.error_for_status())
            .map_err(|err| err.to_string())?
            .json::<SessionsResponse>()
            .map_err(|err| err.to_string())
    }

    fn session_detail(
        &self,
        base_url: &str,
        session_id: SessionId,
    ) -> Result<SessionResponse, String> {
        let url = format!(
            "{}/v1/sessions/{session_id}",
            base_url.trim_end_matches('/')
        );
        self.http
            .get(url)
            .send()
            .and_then(|res| res.error_for_status())
            .map_err(|err| err.to_string())?
            .json::<SessionResponse>()
            .map_err(|err| err.to_string())
    }

    fn requests(&self, base_url: &str) -> Result<RequestsResponse, String> {
        let url = format!("{}/v1/requests", base_url.trim_end_matches('/'));
        self.http
            .get(url)
            .send()
            .and_then(|res| res.error_for_status())
            .map_err(|err| err.to_string())?
            .json::<RequestsResponse>()
            .map_err(|err| err.to_string())
    }

    fn permissions(&self, base_url: &str) -> Result<PermissionsResponse, String> {
        let url = format!("{}/v1/permissions", base_url.trim_end_matches('/'));
        self.http
            .get(url)
            .send()
            .and_then(|res| res.error_for_status())
            .map_err(|err| err.to_string())?
            .json::<PermissionsResponse>()
            .map_err(|err| err.to_string())
    }

    fn create_request(
        &self,
        base_url: &str,
        request: &PromptRequest,
    ) -> Result<RequestCreateResponse, String> {
        let url = format!("{}/v1/requests", base_url.trim_end_matches('/'));
        self.http
            .post(url)
            .json(request)
            .send()
            .and_then(|res| res.error_for_status())
            .map_err(|err| err.to_string())?
            .json::<RequestCreateResponse>()
            .map_err(|err| err.to_string())
    }

    fn cancel_request(
        &self,
        base_url: &str,
        request_id: RequestId,
    ) -> Result<RequestControlResponse, String> {
        let url = format!(
            "{}/v1/requests/{request_id}/cancel",
            base_url.trim_end_matches('/')
        );
        self.http
            .post(url)
            .send()
            .and_then(|res| res.error_for_status())
            .map_err(|err| err.to_string())?
            .json::<RequestControlResponse>()
            .map_err(|err| err.to_string())
    }

    fn permission_decision(
        &self,
        base_url: &str,
        permission_id: PermissionId,
        decision: CorePermissionDecision,
    ) -> Result<PermissionControlResponse, String> {
        let url = format!(
            "{}/v1/permissions/{permission_id}/decision",
            base_url.trim_end_matches('/')
        );
        let body = PermissionDecisionRequest {
            permission_id,
            decision,
        };
        self.http
            .post(url)
            .json(&body)
            .send()
            .and_then(|res| res.error_for_status())
            .map_err(|err| err.to_string())?
            .json::<PermissionControlResponse>()
            .map_err(|err| err.to_string())
    }
}

#[derive(Debug, Default, Clone)]
struct MeshCounts {
    todo: usize,
    in_progress: usize,
    blocked: usize,
    review: usize,
    done: usize,
    other: usize,
}

#[derive(Debug, Default, Clone)]
struct AgentCounts {
    total: usize,
    active: usize,
    away: usize,
    stale: usize,
}

#[derive(Debug, Clone)]
struct MeshTask {
    id: String,
    title: String,
    status: String,
    assigned_to: Option<String>,
    owner: Option<String>,
    depends_on: Vec<String>,
    external: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct FeedEvent {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    preview: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct InboxMessage {
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    ts: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AgentRecord {
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pid: Option<i32>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    preview: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "lastEvent")]
    last_event: Option<String>,
    #[serde(default)]
    lifecycle: Option<String>,
    #[serde(default)]
    status_message: Option<String>,
    #[serde(default)]
    activity: Option<AgentActivity>,
}

#[derive(Debug, Clone, Deserialize)]
struct AgentActivity {
    #[serde(default, rename = "lastActivityAt")]
    last_activity_at: Option<String>,
    #[serde(default, rename = "lastToolCall")]
    last_tool_call: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentView {
    agent: String,
    pid: Option<i32>,
    updated_at: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    preview: Option<String>,
    cwd: Option<String>,
    last_event: Option<String>,
    lifecycle: Option<String>,
    presence: AgentPresence,
    presence_reasons: Vec<String>,
    pid_exists: bool,
    zombie: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AgentPresence {
    Active,
    Away,
    Stale,
}

#[derive(Debug, Clone, Default)]
struct MeshState {
    tasks: Vec<MeshTask>,
    feed: Vec<FeedEvent>,
    inbox: Vec<InboxMessage>,
    agents: Vec<AgentView>,
    counts: MeshCounts,
    agent_counts: AgentCounts,
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen);
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Mesh(mesh)) => run_mesh(mesh),
        None => run_daemon(cli.url),
    }
}

fn run_daemon(url: String) -> anyhow::Result<()> {
    let client = DaemonClient::new()?;
    let root = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut state = AppState::new(UiMode::Daemon(Box::new(DaemonApp::new(url, root))));
    let (stream_tx, action_rx) = mpsc::channel();

    if let UiMode::Daemon(app) = &state.mode {
        spawn_event_stream(app.url.clone(), stream_tx);
    }

    let mut stdout = io::stdout();
    enable_raw_mode().context("enable raw mode")?;
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear().context("clear terminal")?;

    daemon_refresh_all(&client, &mut state);

    loop {
        while let Ok(action) = action_rx.try_recv() {
            if !handle_daemon_action(&client, &mut state, action) {
                return Ok(());
            }
        }

        if state.draw_due() {
            terminal.draw(|frame| draw_state(frame, &state))?;
            state.mark_drawn();
        }

        if event::poll(ACTION_POLL)? {
            if let Some(action) = daemon_key_action(event::read()?) {
                if !handle_daemon_action(&client, &mut state, action) {
                    break;
                }
            }
        }

        if daemon_tick_due(&state) && !handle_daemon_action(&client, &mut state, Action::Tick) {
            break;
        }
    }

    Ok(())
}

fn run_mesh(cli: MeshCli) -> anyhow::Result<()> {
    let app = MeshApp::new(cli.clone());
    let mesh_state = load_mesh_state(&app.root, &app.agent).unwrap_or_else(|_| empty_mesh_state());
    let mut state = AppState::new(UiMode::Mesh(app, Box::new(mesh_state)));

    if let UiMode::Mesh(app, mesh) = &mut state.mode {
        match load_mesh_state(&app.root, &app.agent) {
            Ok(next) => {
                **mesh = next;
                app.status = "loaded".to_string();
                app.last_refresh = Some(Instant::now());
            }
            Err(err) => {
                app.status = format!("load error: {err}");
                app.last_refresh = Some(Instant::now());
            }
        }
    }

    let mut stdout = io::stdout();
    enable_raw_mode().context("enable raw mode")?;
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear().context("clear terminal")?;

    loop {
        if state.draw_due() {
            terminal.draw(|frame| draw_state(frame, &state))?;
            state.mark_drawn();
        }

        if event::poll(ACTION_POLL)? {
            if let Some(action) = mesh_key_action(event::read()?) {
                if !handle_mesh_action(&mut state, action) {
                    break;
                }
            }
        }

        if mesh_tick_due(&state) && !handle_mesh_action(&mut state, Action::Tick) {
            break;
        }
    }

    Ok(())
}

fn draw_state(frame: &mut ratatui::Frame<'_>, state: &AppState) {
    match &state.mode {
        UiMode::Daemon(app) => draw_daemon_ui(frame, app),
        UiMode::Mesh(app, mesh) => draw_mesh_ui(frame, app, mesh),
    }
}

fn daemon_tick_due(state: &AppState) -> bool {
    matches!(&state.mode, UiMode::Daemon(app) if app.last_checked.unwrap_or_else(Instant::now).elapsed() >= app.refresh_every)
}

fn mesh_tick_due(state: &AppState) -> bool {
    matches!(&state.mode, UiMode::Mesh(app, _) if !app.paused && app.last_refresh.unwrap_or_else(Instant::now).elapsed() >= app.refresh_every)
}

fn daemon_key_action(event: Event) -> Option<Action> {
    match event {
        Event::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) => {
            Some(Action::Quit)
        }
        Event::Key(key) if key.code == KeyCode::Tab => Some(Action::NextRoom),
        Event::Key(key) if key.code == KeyCode::Up => Some(Action::SessionPrev),
        Event::Key(key) if key.code == KeyCode::Down => Some(Action::SessionNext),
        Event::Key(key) if key.code == KeyCode::F(10) => Some(Action::ToggleHelp),
        Event::Key(key) if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) => {
            Some(Action::RefreshAll)
        }
        Event::Key(key) if matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S')) => {
            Some(Action::RefreshSessions)
        }
        Event::Key(key)
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(Action::CancelActive)
        }
        Event::Key(key)
            if key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(Action::ComposerNewline)
        }
        Event::Key(key) if key.code == KeyCode::Char('Y') => Some(Action::ApproveLatestPermission),
        Event::Key(key) if key.code == KeyCode::Char('N') => Some(Action::DenyLatestPermission),
        Event::Key(key) if key.code == KeyCode::Enter => Some(Action::SendPrompt),
        Event::Key(key) if key.code == KeyCode::Backspace => Some(Action::Backspace),
        Event::Key(key)
            if key.code == KeyCode::Char('u') && key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            Some(Action::ClearInput)
        }
        Event::Key(key) => {
            if let Some(room) = WorkspaceRoom::from_function_key(key.code) {
                return Some(Action::SelectRoom(room));
            }
            match key.code {
                KeyCode::Char('?') => Some(Action::ToggleHelp),
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    Some(Action::InputChar(ch))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

fn mesh_key_action(event: Event) -> Option<Action> {
    match event {
        Event::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) => {
            Some(Action::Quit)
        }
        Event::Key(key) if key.code == KeyCode::Tab => Some(Action::MeshNextTab),
        Event::Key(key) if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) => {
            Some(Action::MeshRefresh)
        }
        Event::Key(key) if matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P')) => {
            Some(Action::MeshTogglePause)
        }
        Event::Key(key) => match key.code {
            KeyCode::Char(ch) => MeshTab::from_digit(ch).map(Action::MeshSelectTab),
            _ => None,
        },
        _ => None,
    }
}

fn handle_daemon_action(client: &DaemonClient, state: &mut AppState, action: Action) -> bool {
    let UiMode::Daemon(app) = &mut state.mode else {
        return true;
    };

    match action {
        Action::Quit => return false,
        Action::Tick | Action::RefreshAll => daemon_refresh_all(client, state),
        Action::RefreshSessions => daemon_refresh_sessions(client, app),
        Action::SendPrompt => daemon_send_prompt(client, app),
        Action::CancelActive => daemon_cancel_active(client, app),
        Action::ApproveLatestPermission => daemon_decide_latest_permission(client, app, true),
        Action::DenyLatestPermission => daemon_decide_latest_permission(client, app, false),
        Action::ComposerNewline => app.input.push('\n'),
        Action::ToggleHelp => app.show_help = !app.show_help,
        Action::NextRoom => app.active_room = app.active_room.next(),
        Action::SelectRoom(room) => app.active_room = room,
        Action::SessionPrev => {
            app.cycle_session(-1);
            daemon_refresh_selected_session_detail(client, app);
        }
        Action::SessionNext => {
            app.cycle_session(1);
            daemon_refresh_selected_session_detail(client, app);
        }
        Action::InputChar(ch) => app.input.push(ch),
        Action::Backspace => {
            app.input.pop();
        }
        Action::ClearInput => app.input.clear(),
        Action::StreamStatus(status) => app.stream_status = status,
        Action::StreamEvent(envelope) => daemon_apply_stream_event(app, envelope),
        _ => {}
    }
    state.dirty = true;
    true
}

fn handle_mesh_action(state: &mut AppState, action: Action) -> bool {
    let UiMode::Mesh(app, mesh) = &mut state.mode else {
        return true;
    };

    match action {
        Action::Quit => return false,
        Action::Tick | Action::MeshRefresh => match load_mesh_state(&app.root, &app.agent) {
            Ok(next) => {
                **mesh = next;
                app.status = if matches!(action, Action::Tick) {
                    "auto-refresh".to_string()
                } else {
                    "refreshed".to_string()
                };
                app.last_refresh = Some(Instant::now());
            }
            Err(err) => {
                app.status = format!("refresh error: {err}");
                app.last_refresh = Some(Instant::now());
            }
        },
        Action::MeshTogglePause => {
            app.paused = !app.paused;
            app.status = if app.paused {
                "paused".to_string()
            } else {
                "resumed".to_string()
            };
        }
        Action::MeshNextTab => {
            app.active_tab = app.active_tab.next();
        }
        Action::MeshSelectTab(tab) => {
            app.active_tab = tab;
        }
        _ => {}
    }
    state.dirty = true;
    true
}

fn daemon_refresh_all(client: &DaemonClient, state: &mut AppState) {
    let UiMode::Daemon(app) = &mut state.mode else {
        return;
    };
    daemon_refresh_health(client, app);
    daemon_refresh_sessions(client, app);
    daemon_refresh_requests(client, app);
    daemon_refresh_permissions(client, app);
    app.refresh_support_state();
}

fn daemon_refresh_health(client: &DaemonClient, app: &mut DaemonApp) {
    match client.health(&app.url) {
        Ok(health) => {
            app.health = HealthState::Ready(health);
            app.status = "health refreshed".to_string();
        }
        Err(err) => app.health = HealthState::Error(err),
    }
    app.last_checked = Some(Instant::now());
}

fn daemon_refresh_sessions(client: &DaemonClient, app: &mut DaemonApp) {
    match client.sessions(&app.url) {
        Ok(res) if res.ok => {
            app.set_sessions(res.sessions);
            app.status = format!(
                "sessions refreshed: {} ({})",
                app.sessions.len(),
                app.selected_session_label()
            );
            daemon_refresh_selected_session_detail(client, app);
        }
        Ok(res) => app.status = format!("sessions error: {}", res.error.unwrap_or_default()),
        Err(err) => app.status = format!("sessions request error: {err}"),
    }
}

fn daemon_refresh_selected_session_detail(client: &DaemonClient, app: &mut DaemonApp) {
    let Some(session_id) = app.selected_session_id() else {
        app.selected_session_detail = None;
        return;
    };

    match client.session_detail(&app.url, session_id) {
        Ok(res) if res.ok => {
            if let Some(session) = res.session {
                app.status = format!(
                    "session detail: {} {} · {} turns · {} messages",
                    short_id(session.id),
                    compact_text(&session.title, 36),
                    session.turns,
                    session.messages.len()
                );
                app.selected_session_detail = Some(session);
            } else {
                app.selected_session_detail = None;
                app.status = format!("session detail missing for {}", short_id(session_id));
            }
        }
        Ok(res) => {
            app.selected_session_detail = None;
            app.status = format!("session detail error: {}", res.error.unwrap_or_default());
        }
        Err(err) => {
            app.selected_session_detail = None;
            app.status = format!("session detail request error: {err}");
        }
    }
}

fn daemon_refresh_requests(client: &DaemonClient, app: &mut DaemonApp) {
    match client.requests(&app.url) {
        Ok(res) if res.ok => {
            app.set_requests(res.requests);
            app.status = format!("requests refreshed: {}", app.requests.len());
        }
        Ok(res) => app.status = format!("requests error: {}", res.error.unwrap_or_default()),
        Err(err) => app.status = format!("requests request error: {err}"),
    }
}

fn daemon_refresh_permissions(client: &DaemonClient, app: &mut DaemonApp) {
    match client.permissions(&app.url) {
        Ok(res) if res.ok => {
            app.set_permissions(res.permissions);
            app.status = format!("permissions refreshed: {}", app.pending_permissions.len());
        }
        Ok(res) => app.status = format!("permissions error: {}", res.error.unwrap_or_default()),
        Err(err) => app.status = format!("permissions request error: {err}"),
    }
}

fn daemon_cancel_active(client: &DaemonClient, app: &mut DaemonApp) {
    let Some(request_id) = app.cancellable_request_id() else {
        app.status = "no cancellable request".to_string();
        return;
    };
    match client.cancel_request(&app.url, request_id) {
        Ok(res) => {
            app.status = format!(
                "cancel requested ok={} request={} {}: {}",
                res.ok,
                short_id(res.request_id),
                state_label(res.state),
                compact_text(&res.message, 56)
            );
        }
        Err(err) => app.status = format!("cancel request error: {err}"),
    }
}

fn daemon_send_prompt(client: &DaemonClient, app: &mut DaemonApp) {
    let prompt = app.input.trim().to_string();
    if prompt.is_empty() {
        return;
    }
    app.input.clear();
    app.streaming_request_id = None;
    app.push_transcript(format!("> {prompt}"));
    app.status = "creating async request...".to_string();

    let request = PromptRequest {
        prompt,
        request_id: None,
        session_id: app.selected_session_id(),
        max_turns: None,
        yolo: false,
    };

    match client.create_request(&app.url, &request) {
        Ok(res) if res.ok => {
            app.active_request_id = Some(res.request_id);
            app.status = format!(
                "request accepted {} {}: {}",
                short_id(res.request_id),
                state_label(res.state),
                compact_text(&res.message, 56)
            );
            app.push_activity(format!(
                "request_created [{}] {}",
                short_id(res.request_id),
                state_label(res.state)
            ));
        }
        Ok(res) => {
            app.status = format!(
                "request rejected {} {}: {}",
                short_id(res.request_id),
                state_label(res.state),
                compact_text(&res.message, 56)
            );
        }
        Err(err) => app.status = format!("request create error: {err}"),
    }
}

fn daemon_decide_latest_permission(client: &DaemonClient, app: &mut DaemonApp, allow: bool) {
    let Some(pending) = app.latest_pending_permission().cloned() else {
        app.status = "no pending permission request".to_string();
        return;
    };
    let decision = if allow {
        CorePermissionDecision::Allow
    } else {
        CorePermissionDecision::Deny {
            reason: Some("operator denied from ocean-tui".to_string()),
        }
    };
    match client.permission_decision(&app.url, pending.permission_id, decision) {
        Ok(res) if res.ok => {
            app.remove_pending_permission(res.permission_id);
            app.status = format!(
                "permission {} {}",
                short_id(res.permission_id),
                compact_text(&res.message, 56)
            );
            app.push_transcript(format!(
                "{} permission [{}] {}",
                if allow { "✓ allowed" } else { "✗ denied" },
                short_id(res.permission_id),
                compact_text(&pending.reason, 52)
            ));
            daemon_refresh_requests(client, app);
            daemon_refresh_permissions(client, app);
        }
        Ok(res) => {
            app.status = format!(
                "permission {} rejected: {}",
                short_id(res.permission_id),
                compact_text(&res.message, 56)
            );
        }
        Err(err) => app.status = format!("permission decision error: {err}"),
    }
}

fn daemon_apply_stream_event(app: &mut DaemonApp, envelope: EventEnvelope) {
    let request_id = envelope.request_id;
    let permission_id = envelope.permission_id;
    app.push_activity(summarize_event(&envelope));

    match &envelope.event {
        OceanEvent::SessionCreated => {
            app.status = "session created".to_string();
        }
        OceanEvent::UserMessage { text } => {
            app.push_transcript_if_new(format!("> {text}"));
        }
        OceanEvent::AssistantDelta { text } => {
            app.append_assistant_delta(request_id, text);
            app.capture_diff_excerpt(text);
        }
        OceanEvent::ToolStarted { tool, args } => {
            let args_text = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
            app.push_tool_timeline(
                request_id,
                tool,
                "started",
                compact_text(&args_text, 72),
                envelope.at,
            );
            app.capture_diff_excerpt(&args_text);
        }
        OceanEvent::ToolOutput {
            tool,
            text,
            is_error,
        } => {
            app.push_tool_timeline(
                request_id,
                tool,
                if *is_error { "stderr" } else { "output" },
                compact_text(text, 72),
                envelope.at,
            );
            app.capture_diff_excerpt(text);
        }
        OceanEvent::ToolEnded { tool, is_error } => {
            app.push_tool_timeline(
                request_id,
                tool,
                "ended",
                if *is_error {
                    "tool exited with error".to_string()
                } else {
                    "tool completed".to_string()
                },
                envelope.at,
            );
        }
        OceanEvent::PermissionRequest { tool, reason, args } => {
            app.streaming_request_id = None;
            if let Some(permission_id) = permission_id {
                app.upsert_pending_permission(PendingPermission {
                    permission_id,
                    request_id,
                    tool: tool.clone(),
                    reason: reason.clone(),
                    args_preview: serde_json::to_string(args)
                        .map(|json| compact_text(&json, 56))
                        .unwrap_or_else(|_| "{}".to_string()),
                    updated_at: Some(envelope.at),
                });
            }
            app.push_tool_timeline(
                request_id,
                tool,
                "approval",
                compact_text(reason, 72),
                envelope.at,
            );
            app.push_transcript(format!(
                "⚠ approval needed: {} {}",
                tool,
                compact_text(reason, 56)
            ));
            if let Some(request_id) = request_id {
                app.active_request_id = Some(request_id);
            }
            app.status = format!("approval waiting for {}", tool);
        }
        OceanEvent::PermissionDecision { allowed, reason } => {
            app.streaming_request_id = None;
            if let Some(permission_id) = permission_id {
                app.remove_pending_permission(permission_id);
                app.push_transcript(format!(
                    "{} permission [{}] {}",
                    if *allowed {
                        "✓ allowed"
                    } else {
                        "✗ denied"
                    },
                    short_id(permission_id),
                    compact_text(reason.as_deref().unwrap_or("decision recorded"), 56)
                ));
            }
        }
        OceanEvent::TurnFinished { ok, wall_ms } => {
            app.streaming_request_id = None;
            if app.active_request_id == request_id {
                app.active_request_id = None;
            }
            app.push_transcript(format!(
                "{} request {} finished in {}ms",
                if *ok { "✓" } else { "✗" },
                request_id
                    .map(short_id)
                    .unwrap_or_else(|| "stream".to_string()),
                wall_ms
            ));
        }
        OceanEvent::Cancelled { reason } => {
            app.streaming_request_id = None;
            if app.active_request_id == request_id {
                app.active_request_id = None;
            }
            app.push_transcript(format!(
                "✗ cancelled {}",
                compact_text(reason.as_deref().unwrap_or("request cancelled"), 56)
            ));
        }
        OceanEvent::Error { message } => {
            app.streaming_request_id = None;
            app.push_transcript(format!("✗ error {}", compact_text(message, 60)));
            app.status = compact_text(message, 72);
        }
    }
}

fn spawn_event_stream(url: String, tx: mpsc::Sender<Action>) {
    thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder().build() {
            Ok(client) => client,
            Err(err) => {
                let _ = tx.send(Action::StreamStatus(format!(
                    "stream client error: {}",
                    compact_text(&err.to_string(), 72)
                )));
                return;
            }
        };

        let events_url = format!("{}/v1/events", url.trim_end_matches('/'));

        loop {
            if tx
                .send(Action::StreamStatus("connecting".to_string()))
                .is_err()
            {
                break;
            }

            let response = match client
                .get(&events_url)
                .header("Accept", "text/event-stream")
                .send()
                .and_then(|res| res.error_for_status())
            {
                Ok(response) => response,
                Err(err) => {
                    if tx
                        .send(Action::StreamStatus(format!(
                            "reconnecting: {}",
                            compact_text(&err.to_string(), 72)
                        )))
                        .is_err()
                    {
                        break;
                    }
                    thread::sleep(Duration::from_secs(2));
                    continue;
                }
            };

            if tx
                .send(Action::StreamStatus("connected".to_string()))
                .is_err()
            {
                break;
            }

            let mut reader = BufReader::new(response);
            let mut line = String::new();
            let mut event_name = String::new();
            let mut data_lines: Vec<String> = Vec::new();

            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let line = line.trim_end_matches(['\r', '\n']);
                        if line.is_empty() {
                            if !data_lines.is_empty() {
                                let payload = data_lines.join("\n");
                                match serde_json::from_str::<EventEnvelope>(&payload) {
                                    Ok(envelope) => {
                                        if tx.send(Action::StreamEvent(envelope)).is_err() {
                                            return;
                                        }
                                    }
                                    Err(err) => {
                                        let label = if event_name.is_empty() {
                                            "event parse error".to_string()
                                        } else {
                                            format!("event parse error ({event_name})")
                                        };
                                        if tx
                                            .send(Action::StreamStatus(format!(
                                                "{label}: {}",
                                                compact_text(&err.to_string(), 72)
                                            )))
                                            .is_err()
                                        {
                                            return;
                                        }
                                    }
                                }
                                data_lines.clear();
                                event_name.clear();
                            }
                            continue;
                        }

                        if line.starts_with(':') {
                            continue;
                        }

                        if let Some(rest) = line.strip_prefix("event:") {
                            event_name = rest.trim().to_string();
                            continue;
                        }

                        if let Some(rest) = line.strip_prefix("data:") {
                            data_lines.push(rest.trim_start().to_string());
                        }
                    }
                    Err(err) => {
                        if tx
                            .send(Action::StreamStatus(format!(
                                "stream read error: {}",
                                compact_text(&err.to_string(), 72)
                            )))
                            .is_err()
                        {
                            return;
                        }
                        break;
                    }
                }
            }

            if tx
                .send(Action::StreamStatus("reconnecting".to_string()))
                .is_err()
            {
                break;
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

fn summarize_event(envelope: &EventEnvelope) -> String {
    let request = envelope
        .request_id
        .map(|id| format!("[{}] ", short_id(id)))
        .unwrap_or_default();

    match &envelope.event {
        OceanEvent::SessionCreated => format!("{request}session_created"),
        OceanEvent::UserMessage { text } => {
            format!("{request}user_message: {}", compact_text(text, 72))
        }
        OceanEvent::AssistantDelta { text } => {
            format!("{request}assistant_delta: {}", compact_text(text, 72))
        }
        OceanEvent::ToolStarted { tool, args } => format!(
            "{request}tool_started: {tool} args={}",
            compact_text(
                &serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string()),
                48
            )
        ),
        OceanEvent::ToolOutput {
            tool,
            text,
            is_error,
        } => format!(
            "{request}tool_output: {tool} {}{}",
            if *is_error { "error " } else { "" },
            compact_text(text, 72)
        ),
        OceanEvent::ToolEnded { tool, is_error } => format!(
            "{request}tool_ended: {tool}{}",
            if *is_error { " error" } else { "" }
        ),
        OceanEvent::PermissionRequest { tool, reason, args } => format!(
            "{request}permission_request: {tool} reason={} args={}",
            compact_text(reason, 40),
            compact_text(
                &serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string()),
                40
            )
        ),
        OceanEvent::PermissionDecision { allowed, reason } => {
            let state = if *allowed { "allowed" } else { "denied" };
            match reason {
                Some(reason) => format!("{request}permission_decision: {state} {reason}"),
                None => format!("{request}permission_decision: {state}"),
            }
        }
        OceanEvent::TurnFinished { ok, wall_ms } => {
            format!("{request}turn_finished: ok={ok} wall={wall_ms}ms")
        }
        OceanEvent::Cancelled { reason } => match reason {
            Some(reason) => format!("{request}cancelled: {}", compact_text(reason, 72)),
            None => format!("{request}cancelled"),
        },
        OceanEvent::Error { message } => {
            format!("{request}error: {}", compact_text(message, 72))
        }
    }
}

fn load_workspace_support_state(root: &Path, agent: &str) -> anyhow::Result<WorkspaceSupportState> {
    Ok(WorkspaceSupportState {
        mesh: load_mesh_state(root, agent).unwrap_or_else(|_| empty_mesh_state()),
        unified: load_unified_snapshot(root).unwrap_or_default(),
    })
}

fn load_unified_snapshot(root: &Path) -> anyhow::Result<UnifiedSnapshot> {
    let state_path = root.join(".pi/unified/state.json");
    let events_path = root.join(".pi/unified/events.jsonl");
    let state: UnifiedStateFile = if state_path.exists() {
        read_json_file(&state_path)?
    } else {
        UnifiedStateFile {
            generated_at: None,
            summary: Value::Null,
            sources: Value::Null,
        }
    };
    let mut events = read_jsonl::<UnifiedEvent>(&events_path)?;
    trim_front(&mut events, FEED_CAP);
    events.reverse();
    Ok(UnifiedSnapshot {
        generated_at: state.generated_at,
        summary: state.summary,
        sources: state.sources,
        events,
    })
}

fn render_workspace_room_lines(app: &DaemonApp, width: usize) -> Vec<Line<'static>> {
    let mesh = &app.support.mesh;
    let unified = &app.support.unified;
    let mut lines = match app.active_room {
        WorkspaceRoom::Orchestrator => vec![
            Line::from("Orchestrator shell"),
            Line::from(format!(
                "tasks {} done / {} active / {} blocked",
                mesh.counts.done,
                mesh.counts.in_progress,
                mesh.counts.blocked + mesh.counts.review
            )),
            Line::from(format!(
                "inbox {} · feed {} · agents {}",
                mesh.inbox.len(),
                mesh.feed.len(),
                mesh.agent_counts.active
            )),
            Line::from(compact_text(
                mesh.feed
                    .first()
                    .map(|event| render_feed_event(event, width.saturating_sub(4)))
                    .as_deref()
                    .unwrap_or("No mesh event yet."),
                width.saturating_sub(4),
            )),
        ],
        WorkspaceRoom::Writers => vec![
            Line::from("WritersRoom lane"),
            Line::from(format!(
                "Henry {} · Charlotte {}",
                agent_presence(mesh, "Henry"),
                agent_presence(mesh, "Charlotte")
            )),
            Line::from(format!(
                "writing/research tasks {}",
                room_task_count(mesh, &["henry", "writer", "writers", "charlotte"])
            )),
            Line::from("placeholder: NoteDash/filetree internals are not yet embedded; room tab shows mesh/task visibility only."),
        ],
        WorkspaceRoom::Rev => vec![
            Line::from("Rev review lane"),
            Line::from(format!(
                "KNOX/Rev {} · review queue {}",
                agent_presence(mesh, "KNOX"),
                mesh.tasks
                    .iter()
                    .filter(|task| matches!(task.status.as_str(), "review" | "blocked" | "milestone"))
                    .count()
            )),
            Line::from("placeholder: PR/file diff review detail remains local-TUI only until a richer review surface is wired."),
        ],
        WorkspaceRoom::TideDash => vec![
            Line::from("TideDash room"),
            Line::from(value_summary_line(
                &unified.summary,
                "tidedash_statuses",
                "status snapshots",
            )),
            Line::from(value_summary_line(
                &unified.summary,
                "tidedash_errors",
                "error snapshots",
            )),
            Line::from("placeholder: full TideDash panes remain separate until unified cache slices are promoted into native widgets."),
        ],
        WorkspaceRoom::WorkOps => vec![
            Line::from("WorkOps room"),
            Line::from(value_summary_line(&unified.summary, "ops_attention", "ops attention")),
            Line::from(value_summary_line(&unified.summary, "workdash", "workdash summary")),
            Line::from("placeholder: workdash/opsdash split is represented as cache summaries, not embedded tables yet."),
        ],
        WorkspaceRoom::WorldMap => vec![
            Line::from("WorldMap room"),
            Line::from(format!(
                "active {} · away {} · stale {}",
                mesh.agent_counts.active, mesh.agent_counts.away, mesh.agent_counts.stale
            )),
            Line::from("placeholder: geospatial clock/map widget is not embedded yet; using live agent-presence as the room heartbeat."),
        ],
        WorkspaceRoom::PM => vec![
            Line::from("PM room"),
            Line::from(format!(
                "product-facing tasks {}",
                room_task_count(mesh, &["pm", "product", "charlotte", "pixel", "owl"])
            )),
            Line::from(value_summary_line(
                &unified.summary,
                "integrations",
                "integration readiness",
            )),
            Line::from("placeholder: PM routing/brief layer is not embedded; this room currently exposes task + integration context only."),
        ],
    };

    let tail = unified
        .events
        .first()
        .map(|event| render_unified_event(event, width.saturating_sub(4)))
        .unwrap_or_else(|| "No unified sidecar event yet.".to_string());
    lines.push(Line::from(""));
    lines.push(Line::from("Latest sidecar:"));
    lines.push(Line::from(compact_text(&tail, width.saturating_sub(4))));
    lines
}

fn load_mesh_state(root: &Path, agent: &str) -> anyhow::Result<MeshState> {
    let tasks = read_tasks(root)?;
    let feed = read_feed(root)?;
    let inbox = read_inbox(root, agent)?;
    let agents = read_agents(root)?;
    Ok(MeshState {
        counts: count_tasks(&tasks),
        agent_counts: count_agents(&agents),
        tasks,
        feed,
        inbox,
        agents,
    })
}

fn empty_mesh_state() -> MeshState {
    MeshState {
        tasks: Vec::new(),
        feed: Vec::new(),
        inbox: Vec::new(),
        agents: Vec::new(),
        counts: MeshCounts::default(),
        agent_counts: AgentCounts::default(),
    }
}

fn read_tasks(root: &Path) -> anyhow::Result<Vec<MeshTask>> {
    let mut tasks = read_crew_tasks(root)?;
    tasks.extend(read_external_tasks(root)?);
    tasks.sort_by(|a, b| {
        task_sort_key(&a.id)
            .cmp(&task_sort_key(&b.id))
            .then(a.id.cmp(&b.id))
    });
    Ok(tasks)
}

fn read_crew_tasks(root: &Path) -> anyhow::Result<Vec<MeshTask>> {
    let dir = root.join(".pi/messenger/crew/tasks");
    let mut out = Vec::new();
    for entry in read_dir_safe(&dir)? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let json: Value = read_json_file(&path)?;
        let id = json
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let md_path = dir.join(format!("{id}.md"));
        let spec = fs::read_to_string(&md_path).unwrap_or_default();
        let title = json
            .get("title")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| title_from_markdown(&spec))
            .unwrap_or_else(|| id.clone());
        out.push(MeshTask {
            id,
            title,
            status: json
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("todo")
                .to_string(),
            assigned_to: json
                .get("assigned_to")
                .or_else(|| json.get("assignedTo"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            owner: json
                .get("owner")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            depends_on: json
                .get("depends_on")
                .or_else(|| json.get("dependsOn"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            external: false,
        });
    }
    Ok(out)
}

fn read_external_tasks(root: &Path) -> anyhow::Result<Vec<MeshTask>> {
    if env::var("PIMESH_EXTERNAL_TASKS").as_deref() == Ok("0") {
        return Ok(Vec::new());
    }
    let path = root.join(".pi/workdash/external-tasks.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let json: Value = read_json_file(&path)?;
    let Some(tasks) = json.get("tasks").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(tasks
        .iter()
        .filter_map(|task| {
            let id = task.get("id")?.as_str()?.to_string();
            let title = task.get("title")?.as_str()?.to_string();
            Some(MeshTask {
                id,
                title,
                status: task
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("todo")
                    .to_string(),
                assigned_to: task
                    .get("assigned_to")
                    .or_else(|| task.get("assignedTo"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                owner: task
                    .get("owner")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                depends_on: task
                    .get("depends_on")
                    .or_else(|| task.get("dependsOn"))
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                external: true,
            })
        })
        .collect())
}

fn read_feed(root: &Path) -> anyhow::Result<Vec<FeedEvent>> {
    let path = root.join(".pi/messenger/feed.jsonl");
    let mut items = read_jsonl::<FeedEvent>(&path)?;
    trim_front(&mut items, FEED_CAP);
    items.reverse();
    Ok(items)
}

fn read_inbox(root: &Path, agent: &str) -> anyhow::Result<Vec<InboxMessage>> {
    let path = root.join(format!(".pi/messenger/mailboxes/by-agent/{agent}.jsonl"));
    let mut items = read_jsonl::<InboxMessage>(&path)?;
    trim_front(&mut items, INBOX_CAP);
    items.reverse();
    Ok(items)
}

fn read_agents(root: &Path) -> anyhow::Result<Vec<AgentView>> {
    let mut records = Vec::new();
    for dir in agent_source_dirs(root) {
        for entry in read_dir_safe(&dir)? {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let record: AgentRecord = read_json_file(&path)?;
            if let Some(agent) = classify_agent(record) {
                records.push(agent);
            }
        }
    }
    apply_agent_record_hygiene(&mut records);
    records.sort_by(|a, b| {
        a.presence
            .cmp(&b.presence)
            .then(parse_ts(&b.updated_at).cmp(&parse_ts(&a.updated_at)))
    });
    Ok(records)
}

fn classify_agent(record: AgentRecord) -> Option<AgentView> {
    let agent_name = record.agent.or(record.name)?;
    let updated_at = record.updated_at.or_else(|| {
        record
            .activity
            .as_ref()
            .and_then(|activity| activity.last_activity_at.clone())
    });
    let last_event = record.last_event.or_else(|| {
        record
            .activity
            .as_ref()
            .and_then(|activity| activity.last_tool_call.clone())
    });
    let preview = record.preview.or(record.status_message.clone());
    let pid_state = pid_state(record.pid);
    let age_ms = age_ms(updated_at.as_deref());
    let mut reasons = Vec::new();
    if updated_at.is_none() {
        reasons.push("no heartbeat".to_string());
    }
    if !pid_state.exists {
        reasons.push(if pid_state.invalid {
            "invalid pid".to_string()
        } else {
            "pid missing".to_string()
        });
    } else if pid_state.zombie {
        reasons.push("zombie pid".to_string());
    }
    if age_ms > AWAY_AGENT_MS {
        reasons.push("old heartbeat".to_string());
    }
    let presence = if reasons.is_empty() && age_ms <= ACTIVE_AGENT_MS {
        AgentPresence::Active
    } else if reasons.is_empty() && age_ms <= AWAY_AGENT_MS {
        AgentPresence::Away
    } else {
        AgentPresence::Stale
    };
    Some(AgentView {
        agent: agent_name,
        pid: record.pid,
        updated_at,
        model: record.model,
        provider: record.provider,
        preview,
        cwd: record.cwd,
        last_event,
        lifecycle: record.lifecycle,
        presence,
        presence_reasons: reasons,
        pid_exists: pid_state.exists,
        zombie: pid_state.zombie,
    })
}

fn apply_agent_record_hygiene(agents: &mut [AgentView]) {
    let mut by_pid: HashMap<i32, Vec<usize>> = HashMap::new();
    for (idx, agent) in agents.iter().enumerate() {
        if let Some(pid) = agent.pid {
            if agent.pid_exists && !agent.zombie {
                by_pid.entry(pid).or_default().push(idx);
            }
        }
    }

    for (pid, group) in by_pid {
        if group.len() < 2 {
            continue;
        }
        let mut keep = group[0];
        for idx in group.iter().copied() {
            let better_name = agents[idx].agent != "agent" && agents[keep].agent == "agent";
            let newer = parse_ts(&agents[idx].updated_at) > parse_ts(&agents[keep].updated_at);
            if better_name || newer {
                keep = idx;
            }
        }
        let keep_name = agents[keep].agent.clone();
        for idx in group {
            if idx == keep {
                continue;
            }
            agents[idx].presence = AgentPresence::Stale;
            agents[idx].presence_reasons.push(format!(
                "duplicate record for pid {pid}; keeping {keep_name}"
            ));
        }
    }
}

fn count_tasks(tasks: &[MeshTask]) -> MeshCounts {
    let mut counts = MeshCounts::default();
    for task in tasks {
        match task.status.as_str() {
            "todo" | "pending" | "ready" => counts.todo += 1,
            "in_progress" | "progress" => counts.in_progress += 1,
            "blocked" => counts.blocked += 1,
            "review" | "milestone" => counts.review += 1,
            "done" => counts.done += 1,
            _ => counts.other += 1,
        }
    }
    counts
}

fn count_agents(agents: &[AgentView]) -> AgentCounts {
    let mut counts = AgentCounts::default();
    for agent in agents {
        counts.total += 1;
        match agent.presence {
            AgentPresence::Active => counts.active += 1,
            AgentPresence::Away => counts.away += 1,
            AgentPresence::Stale => counts.stale += 1,
        }
    }
    counts
}

fn draw_daemon_ui(frame: &mut ratatui::Frame<'_>, app: &DaemonApp) {
    if frame.area().width < 118 || frame.area().height < 32 {
        draw_daemon_ui_compact(frame, app);
        return;
    }

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(6),
            Constraint::Min(16),
            Constraint::Length(6),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(52),
            Constraint::Percentage(24),
            Constraint::Percentage(24),
        ])
        .split(layout[1]);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(12), Constraint::Length(8)])
        .split(body[0]);
    let center = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(body[1]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(11),
            Constraint::Length(8),
            Constraint::Min(8),
        ])
        .split(body[2]);

    draw_daemon_header(frame, layout[0], app);
    frame.render_widget(
        Paragraph::new(app.transcript_lines())
            .block(
                Block::default()
                    .title("Agent Session Transcript")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        left[0],
    );
    frame.render_widget(
        Paragraph::new(app.event_lines(12))
            .block(Block::default().title("Event Rail").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
        left[1],
    );
    frame.render_widget(
        Paragraph::new(app.tool_timeline_lines(center[0].height.saturating_sub(2) as usize))
            .block(
                Block::default()
                    .title("Tool Timeline")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        center[0],
    );
    frame.render_widget(
        Paragraph::new(app.diff_lines(center[1].height.saturating_sub(2) as usize))
            .block(
                Block::default()
                    .title("Diffs / Edits")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        center[1],
    );
    frame.render_widget(
        Paragraph::new(app.room_lines(right[0].width.saturating_sub(4) as usize))
            .block(
                Block::default()
                    .title(format!("{} Room", app.active_room.label()))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        right[0],
    );
    frame.render_widget(
        Paragraph::new(app.pending_permission_lines())
            .block(
                Block::default()
                    .title(format!("Approvals ({})", app.pending_permissions.len()))
                    .borders(Borders::ALL)
                    .border_style(if app.pending_permissions.is_empty() {
                        Style::default()
                    } else {
                        Style::default().fg(Color::Yellow)
                    }),
            )
            .wrap(Wrap { trim: true }),
        right[1],
    );
    frame.render_widget(
        Paragraph::new(workspace_status_lines(
            app,
            right[2].height.saturating_sub(2) as usize,
        ))
        .block(
            Block::default()
                .title("Sessions / Support / Help")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true }),
        right[2],
    );
    frame.render_widget(
        Paragraph::new(app.composer_lines())
            .block(Block::default().title("Composer").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        layout[2],
    );
    frame.render_widget(daemon_footer(app), layout[3]);
}

fn draw_daemon_ui_compact(frame: &mut ratatui::Frame<'_>, app: &DaemonApp) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(6),
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Length(8),
            Constraint::Min(8),
            Constraint::Length(6),
            Constraint::Length(2),
        ])
        .split(frame.area());

    draw_daemon_header(frame, chunks[0], app);
    frame.render_widget(
        Paragraph::new(app.room_lines(chunks[1].width.saturating_sub(4) as usize))
            .block(
                Block::default()
                    .title(format!("{} Room", app.active_room.label()))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        chunks[1],
    );
    frame.render_widget(
        Paragraph::new(app.tool_timeline_lines(chunks[2].height.saturating_sub(2) as usize))
            .block(
                Block::default()
                    .title("Tool Timeline")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true }),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(app.pending_permission_lines())
            .block(
                Block::default()
                    .title(format!("Approvals ({})", app.pending_permissions.len()))
                    .borders(Borders::ALL)
                    .border_style(if app.pending_permissions.is_empty() {
                        Style::default()
                    } else {
                        Style::default().fg(Color::Yellow)
                    }),
            )
            .wrap(Wrap { trim: true }),
        chunks[3],
    );
    frame.render_widget(
        Paragraph::new(app.transcript_lines())
            .block(
                Block::default()
                    .title("Transcript / Diff / Sessions")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false }),
        chunks[4],
    );
    frame.render_widget(
        Paragraph::new(app.composer_lines())
            .block(Block::default().title("Composer").borders(Borders::ALL))
            .wrap(Wrap { trim: false }),
        chunks[5],
    );
    frame.render_widget(daemon_footer(app), chunks[6]);
}

fn draw_daemon_header(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    app: &DaemonApp,
) {
    let (health_label, health_color) = app.status_label();
    let root_label = app
        .root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| app.root.display().to_string());
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "◉",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(health_label, Style::default().fg(health_color)),
            Span::raw("  "),
            Span::styled(
                "Ocean TUI Coding Workspace",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(format!(
            "root {} · room {} · session {}",
            root_label,
            app.active_room.label(),
            app.selected_session_label()
        )),
        Line::from(workspace_room_tabs_line(app.active_room)),
        Line::from(format!(
            "daemon {} · stream {} · approvals {} · active request {}",
            app.health_summary(),
            app.stream_status,
            app.pending_permissions.len(),
            app.active_request_id
                .map(short_id)
                .unwrap_or_else(|| "none".to_string())
        )),
    ])
    .block(Block::default().title("Ocean").borders(Borders::ALL))
    .wrap(Wrap { trim: true });
    frame.render_widget(header, area);
}

fn workspace_room_tabs_line(active: WorkspaceRoom) -> String {
    WorkspaceRoom::all()
        .iter()
        .enumerate()
        .map(|(idx, room)| {
            if *room == active {
                format!("[F{} {}]", idx + 1, room.label())
            } else {
                format!("F{} {}", idx + 1, room.label())
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn workspace_status_lines(app: &DaemonApp, limit: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.extend(app.request_lines());
    lines.push(Line::from(""));
    lines.extend(app.session_lines());
    lines.push(Line::from(""));
    lines.extend(app.support_lines());
    if app.show_help {
        lines.push(Line::from(""));
        lines.push(Line::from("Help"));
        lines.push(Line::from("Tab/F1-F7 rooms · Up/Down session picker"));
        lines.push(Line::from("Enter send · Ctrl-J newline · Ctrl-U clear"));
        lines.push(Line::from("Ctrl-C cancel · Shift-Y/N approval · r refresh"));
    } else {
        lines.push(Line::from(""));
        lines.push(Line::from("F10 or ? for help"));
    }
    lines.into_iter().take(limit.max(1)).collect()
}

fn daemon_footer(app: &DaemonApp) -> Paragraph<'static> {
    Paragraph::new(format!(
        "workspace first | Tab cycle rooms | F1-F7 jump | Up/Down session picker | Enter send | Ctrl-J newline | Ctrl-C cancel | Shift-Y/Shift-N approvals | F10/? help | checked {} | {}",
        app.checked_text(),
        app.status
    ))
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::DarkGray))
}

fn draw_mesh_ui(frame: &mut ratatui::Frame<'_>, app: &MeshApp, state: &MeshState) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(area);

    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "TIDES-MESH",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" // "),
            Span::styled(
                app.root
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| app.root.display().to_string()),
                Style::default().fg(Color::White),
            ),
            Span::raw(" // "),
            Span::styled(app.agent.as_str(), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(format!(
            "{} done  {} active-task  {} blocked  {} active/{}/{} stale",
            state.counts.done,
            state.counts.in_progress,
            state.counts.blocked + state.counts.review,
            state.agent_counts.active,
            state.agent_counts.away,
            state.agent_counts.stale
        )),
        Line::from(mesh_tabs_line(
            active_center_tab(app.active_tab),
            app.paused,
        )),
    ])
    .block(
        Block::default()
            .title("Ocean TUI Mesh")
            .borders(Borders::ALL),
    )
    .wrap(Wrap { trim: true });
    frame.render_widget(header, layout[0]);

    if area.width >= 160 && area.height >= 28 {
        draw_mesh_floor(frame, layout[1], app, state);
    } else {
        match app.active_tab {
            MeshTab::Board => draw_mesh_board(frame, layout[1], state),
            MeshTab::Events => draw_mesh_events(frame, layout[1], state),
            MeshTab::Inbox => draw_mesh_inbox(frame, layout[1], state, &app.agent),
            MeshTab::Agents => draw_mesh_agents(frame, layout[1], state),
        }
    }

    let footer = Paragraph::new(format!(
        "1-4 tabs | Tab cycle | r refresh | p pause | q quit | checked {} | {}",
        app.checked_text(),
        app.status
    ))
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, layout[2]);
}

fn draw_mesh_floor(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    app: &MeshApp,
    state: &MeshState,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(31),
            Constraint::Percentage(49),
            Constraint::Percentage(20),
        ])
        .split(area);

    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(14),
            Constraint::Min(18),
            Constraint::Min(12),
        ])
        .split(cols[0]);
    let center = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Min(20)])
        .split(cols[1]);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(49), Constraint::Min(20)])
        .split(cols[2]);

    draw_mesh_glyph(frame, left[0], state);
    draw_mesh_knox(frame, left[1], state);
    draw_mesh_charlotte(frame, left[2], state);
    draw_mesh_center(frame, center[0], app, state);
    draw_mesh_orchestrator(frame, center[1], app, state);
    draw_mesh_brick(frame, right[0], state);
    draw_mesh_pixel(frame, right[1], state);
}

fn draw_mesh_glyph(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, state: &MeshState) {
    let lines = vec![
        Line::from("◉ Glyph / ledger"),
        Line::from(format!("tasks: {} total", state.tasks.len())),
        Line::from(format!("events: {} live", state.feed.len())),
        Line::from(format!(
            "agents: {} active / {} away / {} stale",
            state.agent_counts.active, state.agent_counts.away, state.agent_counts.stale
        )),
    ];
    let panel = Paragraph::new(lines)
        .block(Block::default().title("Glyph").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(panel, area);
}

fn draw_mesh_knox(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, state: &MeshState) {
    draw_mesh_agent_status_panel(frame, area, state, "KNOX", "review gate");
}

fn draw_mesh_charlotte(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    state: &MeshState,
) {
    draw_mesh_agent_status_panel(frame, area, state, "Charlotte", "research");
}

fn draw_mesh_brick(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, state: &MeshState) {
    draw_mesh_agent_status_panel(frame, area, state, "BRICK", "backend/runtime");
}

fn draw_mesh_pixel(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, state: &MeshState) {
    draw_mesh_agent_status_panel(frame, area, state, "PIXEL", "frontend/tui");
}

fn draw_mesh_center(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    app: &MeshApp,
    state: &MeshState,
) {
    match app.active_tab {
        MeshTab::Events => draw_mesh_events(frame, area, state),
        MeshTab::Inbox => draw_mesh_inbox(frame, area, state, &app.agent),
        _ => draw_mesh_board(frame, area, state),
    }
}

fn draw_mesh_orchestrator(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    app: &MeshApp,
    state: &MeshState,
) {
    let lines = vec![
        Line::from("Orchestrator control"),
        Line::from(format!("agent: {}", app.agent)),
        Line::from(format!("checked: {}", app.checked_text())),
        Line::from(format!("status: {}", app.status)),
        Line::from(format!(
            "done: {}  active: {}",
            state.counts.done, state.counts.in_progress
        )),
    ];
    let panel = Paragraph::new(lines)
        .block(Block::default().title("Orchestrator").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(panel, area);
}

fn draw_mesh_agent_status_panel(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    state: &MeshState,
    agent_name: &str,
    role: &str,
) {
    let agent = state
        .agents
        .iter()
        .find(|agent| agent.agent.eq_ignore_ascii_case(agent_name));
    let (presence, last, preview) = if let Some(agent) = agent {
        (
            match agent.presence {
                AgentPresence::Active => "active",
                AgentPresence::Away => "away",
                AgentPresence::Stale => "stale",
            },
            time_ago(agent.updated_at.as_deref()),
            agent
                .preview
                .as_deref()
                .or(agent.last_event.as_deref())
                .unwrap_or("no preview"),
        )
    } else {
        ("offline", "—".into(), "no live agent record")
    };

    let lines = vec![
        Line::from(format!("role: {role}")),
        Line::from(format!("presence: {presence}")),
        Line::from(format!("last: {last}")),
        Line::from(compact_text(preview, area.width.saturating_sub(4) as usize)),
    ];
    let panel = Paragraph::new(lines)
        .block(Block::default().title(agent_name).borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(panel, area);
}

fn active_center_tab(tab: MeshTab) -> MeshTab {
    match tab {
        MeshTab::Agents => MeshTab::Board,
        other => other,
    }
}

fn agent_source_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![
        root.join(".pi/messenger/live/agents"),
        root.join(".pi/messenger/registry"),
    ];
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".pi/agent/messenger/registry"));
        dirs.push(home.join(".pi/messenger/registry"));
    }
    dirs
}

fn draw_mesh_board(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, state: &MeshState) {
    let columns: [(&str, &[&str]); 4] = [
        ("TO DO", &["todo", "pending", "ready"]),
        ("ACTIVE", &["in_progress", "progress"]),
        ("BLOCKED / REVIEW", &["blocked", "review", "milestone"]),
        ("DONE", &["done"]),
    ];
    let layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    for (idx, (title, statuses)) in columns.iter().enumerate() {
        let items: Vec<String> = state
            .tasks
            .iter()
            .filter(|task| statuses.contains(&task.status.as_str()))
            .map(board_card)
            .collect();
        let lines = if items.is_empty() {
            vec![Line::from("No tasks.")]
        } else {
            items.into_iter().map(Line::from).collect()
        };
        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(
                        "{} {}",
                        title,
                        count_bucket(&state.tasks, statuses)
                    ))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, layout[idx]);
    }
}

fn draw_mesh_events(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    state: &MeshState,
) {
    let lines: Vec<Line<'static>> = if state.feed.is_empty() {
        vec![Line::from("No events yet.")]
    } else {
        state
            .feed
            .iter()
            .take(area.height.saturating_sub(2) as usize)
            .map(|event| Line::from(render_feed_event(event, area.width as usize)))
            .collect()
    };
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title("TIDES-MESH Events")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn draw_mesh_inbox(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    state: &MeshState,
    agent: &str,
) {
    let lines: Vec<Line<'static>> = if state.inbox.is_empty() {
        vec![Line::from(format!("No mailbox found for {agent}."))]
    } else {
        state
            .inbox
            .iter()
            .take(area.height.saturating_sub(2) as usize)
            .map(|msg| Line::from(render_inbox_message(msg, area.width as usize)))
            .collect()
    };
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(format!("{} Inbox", agent))
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn draw_mesh_agents(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    state: &MeshState,
) {
    let mut lines = vec![Line::from(
        "state    agent              pid     age   model/provider        last        preview",
    )];
    for agent in state
        .agents
        .iter()
        .take(area.height.saturating_sub(3) as usize)
    {
        lines.push(Line::from(render_agent_line(agent, area.width as usize)));
    }
    if state.agents.is_empty() {
        lines.push(Line::from("No live agent records."));
    }
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title("TIDES-MESH Agents")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn mesh_tabs_line(active: MeshTab, paused: bool) -> String {
    let mut parts = Vec::new();
    for (idx, tab) in MeshTab::all().iter().enumerate() {
        let label = format!("{}:{}", idx + 1, tab.label().to_uppercase());
        if *tab == active {
            parts.push(format!("[{label}]"));
        } else {
            parts.push(label);
        }
    }
    let mut line = parts.join(" ");
    if paused {
        line.push_str("  PAUSED");
    } else {
        line.push_str("  Tab cycle · r refresh · p pause · q quit");
    }
    line
}

fn board_card(task: &MeshTask) -> String {
    let owner = task
        .assigned_to
        .as_deref()
        .or(task.owner.as_deref())
        .unwrap_or("unassigned");
    let deps = if task.depends_on.is_empty() {
        String::new()
    } else {
        format!(" ←{}", task.depends_on.join(","))
    };
    let external = if task.external { " ↗" } else { "" };
    format!(
        "{}{} {} · {}{}",
        task.id,
        external,
        compact_text(&task.title, 28),
        owner,
        compact_text(&deps, 18)
    )
}

fn count_bucket(tasks: &[MeshTask], statuses: &[&str]) -> usize {
    tasks
        .iter()
        .filter(|task| statuses.contains(&task.status.as_str()))
        .count()
}

fn render_feed_event(event: &FeedEvent, width: usize) -> String {
    let kind = event
        .r#type
        .as_deref()
        .or(event.event.as_deref())
        .unwrap_or("event");
    let actor = event
        .agent
        .as_deref()
        .or(event.actor.as_deref())
        .or(event.from.as_deref())
        .unwrap_or("mesh");
    let target = event
        .target
        .as_deref()
        .or(event.to.as_deref())
        .unwrap_or("");
    let message = event
        .preview
        .as_deref()
        .or(event.message.as_deref())
        .or(event.text.as_deref())
        .or(event.summary.as_deref())
        .unwrap_or("");
    let stamp = time_ago(
        event
            .ts
            .as_deref()
            .or(event.created_at.as_deref())
            .or(event.timestamp.as_deref()),
    );
    let left = if target.is_empty() {
        format!(
            "{:>4} {} {}",
            stamp,
            compact_text(kind, 18),
            compact_text(actor, 14)
        )
    } else {
        format!(
            "{:>4} {} {} → {}",
            stamp,
            compact_text(kind, 18),
            compact_text(actor, 14),
            compact_text(target, 14)
        )
    };
    format!(
        "{} {}",
        left,
        compact_text(message, width.saturating_sub(left.len() + 1))
    )
}

fn render_inbox_message(message: &InboxMessage, width: usize) -> String {
    let outbound = message.dir.as_deref() == Some("out");
    let dir = if outbound { "OUT" } else { "IN" };
    let peer = if outbound {
        message.to.as_deref().unwrap_or("?")
    } else {
        message.from.as_deref().unwrap_or("?")
    };
    let stamp = time_ago(message.ts.as_deref());
    let left = format!("{:>4} {} {}", stamp, dir, compact_text(peer, 16));
    format!(
        "{} {}",
        left,
        compact_text(
            message.text.as_deref().unwrap_or(""),
            width.saturating_sub(left.len() + 1)
        )
    )
}

fn render_agent_line(agent: &AgentView, width: usize) -> String {
    let state = match agent.presence {
        AgentPresence::Active => "active",
        AgentPresence::Away => "away",
        AgentPresence::Stale => "stale",
    };
    let reason = if agent.presence_reasons.is_empty() {
        String::new()
    } else {
        format!(" · {}", agent.presence_reasons.join(", "))
    };
    let preview = agent
        .preview
        .as_deref()
        .or(agent.cwd.as_deref())
        .unwrap_or("");
    let model = agent
        .model
        .as_deref()
        .or(agent.provider.as_deref())
        .unwrap_or("—");
    let last = agent
        .last_event
        .as_deref()
        .or(agent.lifecycle.as_deref())
        .unwrap_or("—");
    let base = format!(
        "{:<8} {:<18} {:<7} {:<5} {:<20} {:<10}",
        state,
        compact_text(&agent.agent, 18),
        agent
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "—".to_string()),
        time_ago(agent.updated_at.as_deref()),
        compact_text(model, 20),
        compact_text(last, 10)
    );
    format!(
        "{} {}",
        base,
        compact_text(
            &format!("{}{}", preview, reason),
            width.saturating_sub(base.len() + 1)
        )
    )
}

fn agent_presence(state: &MeshState, agent_name: &str) -> &'static str {
    state
        .agents
        .iter()
        .find(|agent| agent.agent.eq_ignore_ascii_case(agent_name))
        .map(|agent| match agent.presence {
            AgentPresence::Active => "active",
            AgentPresence::Away => "away",
            AgentPresence::Stale => "stale",
        })
        .unwrap_or("offline")
}

fn room_task_count(state: &MeshState, owners: &[&str]) -> usize {
    state
        .tasks
        .iter()
        .filter(|task| {
            let haystack = [
                task.assigned_to.as_deref().unwrap_or(""),
                task.owner.as_deref().unwrap_or(""),
                task.title.as_str(),
            ]
            .join(" ")
            .to_ascii_lowercase();
            owners
                .iter()
                .any(|needle| haystack.contains(&needle.to_ascii_lowercase()))
        })
        .count()
}

fn value_summary_line(summary: &Value, key: &str, label: &str) -> String {
    match summary.get(key) {
        Some(Value::Array(items)) => format!("{} {}", label, items.len()),
        Some(Value::Object(map)) => format!("{} {} keys", label, map.len()),
        Some(Value::String(text)) => format!("{} {}", label, compact_text(text, 32)),
        Some(other) => format!("{} {}", label, compact_text(&other.to_string(), 32)),
        None => format!("{} unavailable", label),
    }
}

fn render_unified_event(event: &UnifiedEvent, width: usize) -> String {
    let stamp = time_ago(event.ts.as_deref());
    let source = event.source.as_deref().unwrap_or("unified");
    let kind = event.r#type.as_deref().unwrap_or("event");
    let ident = event
        .id
        .as_deref()
        .map(|id| compact_text(id, 6))
        .unwrap_or_default();
    let summary = event
        .message
        .as_deref()
        .map(ToOwned::to_owned)
        .or_else(|| event.summary.as_ref().map(Value::to_string))
        .unwrap_or_default();
    let left = format!(
        "{:>4} {} {} {}",
        stamp,
        compact_text(source, 12),
        compact_text(kind, 18),
        ident
    );
    format!(
        "{} {}",
        left,
        compact_text(&summary, width.saturating_sub(left.len() + 1))
    )
}

fn extract_diff_excerpt(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let looks_like_diff = trimmed.contains("diff --git")
        || trimmed.contains("@@")
        || trimmed.contains("+++ ")
        || trimmed.contains("--- ")
        || trimmed.contains("Successfully replaced")
        || trimmed.contains("apply_patch")
        || trimmed.contains("cargo fmt")
        || trimmed.lines().any(|line| {
            let line = line.trim_start();
            (line.starts_with('+') || line.starts_with('-')) && line.len() > 4
        });
    if !looks_like_diff {
        return None;
    }
    let excerpt = trimmed
        .lines()
        .take(4)
        .map(|line| compact_text(line, 70))
        .collect::<Vec<_>>()
        .join(" | ");
    Some(excerpt)
}

fn state_label(state: RequestState) -> &'static str {
    match state {
        RequestState::Queued => "queued",
        RequestState::Running => "running",
        RequestState::WaitingForPermission => "waiting_permission",
        RequestState::Cancelling => "cancel_requested",
        RequestState::Cancelled => "cancelled",
        RequestState::Completed => "completed",
        RequestState::Errored => "errored",
    }
}

fn session_run_state_label(state: SessionRunState) -> &'static str {
    match state {
        SessionRunState::Stored => "stored",
        SessionRunState::Running => "running",
        SessionRunState::WaitingForPermission => "waiting_permission",
        SessionRunState::Cancelling => "cancel_requested",
        SessionRunState::Cancelled => "cancelled",
        SessionRunState::Completed => "completed",
        SessionRunState::Errored => "errored",
    }
}

fn short_id(id: RequestId) -> String {
    id.to_string().chars().take(8).collect()
}

fn compact_text(text: &str, limit: usize) -> String {
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return "(empty)".to_string();
    }
    let char_count = compact.chars().count();
    if char_count <= limit {
        return compact;
    }
    let mut result: String = compact.chars().take(limit.saturating_sub(1)).collect();
    result.push('…');
    result
}

fn checked_text(last: Option<Instant>) -> String {
    match last {
        Some(last_checked) => {
            let elapsed = last_checked.elapsed();
            if elapsed < Duration::from_secs(1) {
                "just now".to_string()
            } else {
                format!("{}s ago", elapsed.as_secs())
            }
        }
        None => "never".to_string(),
    }
}

fn default_mesh_agent() -> String {
    env::var("TIDES_MESH_AGENT")
        .or_else(|_| env::var("PI_AGENT_NAME"))
        .unwrap_or_else(|_| "Orchestrator".to_string())
}

fn push_bounded<T>(items: &mut Vec<T>, value: T, cap: usize) {
    items.push(value);
    trim_front(items, cap);
}

fn trim_front<T>(items: &mut Vec<T>, cap: usize) {
    if items.len() > cap {
        let drain = items.len() - cap;
        items.drain(0..drain);
    }
}

fn read_dir_safe(path: &Path) -> anyhow::Result<Vec<fs::DirEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read dir {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("collect dir {}", path.display()))?;
    entries.sort_by_key(|entry| entry.path());
    Ok(entries)
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str(line)
            .with_context(|| format!("parse {} line {}", path.display(), idx + 1))?;
        out.push(value);
    }
    Ok(out)
}

fn title_from_markdown(markdown: &str) -> Option<String> {
    markdown
        .lines()
        .find_map(|line| line.strip_prefix("# ").map(|s| s.trim().to_string()))
}

fn task_sort_key(id: &str) -> (usize, String) {
    let n = id
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .parse::<usize>()
        .unwrap_or(0);
    (n, id.to_string())
}

fn parse_ts(value: &Option<String>) -> i64 {
    value
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn age_ms(value: Option<&str>) -> i64 {
    value
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| {
            Utc::now()
                .signed_duration_since(dt.with_timezone(&Utc))
                .num_milliseconds()
        })
        .unwrap_or(i64::MAX)
}

fn time_ago(value: Option<&str>) -> String {
    let ms = age_ms(value);
    if ms == i64::MAX {
        return "—".to_string();
    }
    let secs = (ms / 1000).max(0);
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 172800 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

struct PidState {
    exists: bool,
    zombie: bool,
    invalid: bool,
}

fn pid_state(pid: Option<i32>) -> PidState {
    let Some(pid) = pid else {
        return PidState {
            exists: false,
            zombie: false,
            invalid: true,
        };
    };
    if pid <= 0 {
        return PidState {
            exists: false,
            zombie: false,
            invalid: true,
        };
    }
    pid_state_from_reader(pid, |path| fs::read_to_string(path).ok())
}

fn pid_state_from_reader<F>(pid: i32, read_stat: F) -> PidState
where
    F: FnOnce(&Path) -> Option<String>,
{
    let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
    let Some(stat) = read_stat(&stat_path) else {
        return PidState {
            exists: false,
            zombie: false,
            invalid: false,
        };
    };
    let zombie = stat.split(") ").nth(1).and_then(|rest| rest.chars().next()) == Some('Z');
    PidState {
        exists: true,
        zombie,
        invalid: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_core::EventEnvelope;
    use serde_json::json;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn summarize_event_includes_request_prefix() {
        let request_id = RequestId::nil();
        let mut envelope = EventEnvelope::new(OceanEvent::AssistantDelta {
            text: "hello from stream".to_string(),
        });
        envelope.request_id = Some(request_id);

        let summary = summarize_event(&envelope);

        assert!(summary.contains("assistant_delta: hello from stream"));
        assert!(summary.contains(&short_id(request_id)));
    }

    #[test]
    fn daemon_tracks_pending_permissions_from_request_snapshots() {
        let request_id = RequestId::new_v4();
        let permission_id = PermissionId::new_v4();
        let mut app = DaemonApp::new("http://127.0.0.1:4780".to_string(), PathBuf::from("."));

        app.set_requests(vec![RequestStatus {
            request_id,
            session_id: None,
            state: RequestState::WaitingForPermission,
            permission_id: Some(permission_id),
            message: Some("waiting on permission for bash".to_string()),
            started_at: None,
            updated_at: Some(Utc::now()),
            finished_at: None,
        }]);

        assert_eq!(app.pending_permissions.len(), 1);
        assert_eq!(app.pending_permissions[0].permission_id, permission_id);
        assert_eq!(app.pending_permissions[0].request_id, Some(request_id));
    }

    #[test]
    fn daemon_sets_pending_permissions_from_permissions_endpoint_payload() {
        let request_id = RequestId::new_v4();
        let permission_id = PermissionId::new_v4();
        let mut app = DaemonApp::new("http://127.0.0.1:4780".to_string(), PathBuf::from("."));

        app.set_permissions(vec![PermissionStatus {
            permission_id,
            request_id,
            session_id: Some(SessionId::new_v4()),
            tool: "bash".to_string(),
            reason: "permission required for bash".to_string(),
            args: json!({"command": "touch /tmp/ocean-demo"}),
            created_at: Utc::now(),
        }]);

        assert_eq!(app.pending_permissions.len(), 1);
        assert_eq!(app.pending_permissions[0].permission_id, permission_id);
        assert_eq!(app.pending_permissions[0].request_id, Some(request_id));
        assert_eq!(app.pending_permissions[0].tool, "bash");
        assert!(app.pending_permissions[0]
            .args_preview
            .contains("/tmp/ocean-demo"));
        let rendered = app
            .pending_permission_lines()
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Shift-Y approve"));
        assert!(rendered.contains("Shift-N deny"));

        app.set_permissions(Vec::new());
        assert!(app.pending_permissions.is_empty());
    }

    #[test]
    fn daemon_approval_keys_map_to_permission_actions() {
        let approve = daemon_key_action(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('Y'),
            KeyModifiers::SHIFT,
        )));
        let deny = daemon_key_action(Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('N'),
            KeyModifiers::SHIFT,
        )));

        assert!(matches!(approve, Some(Action::ApproveLatestPermission)));
        assert!(matches!(deny, Some(Action::DenyLatestPermission)));
    }

    #[test]
    fn daemon_stream_event_updates_permission_queue_and_transcript() {
        let request_id = RequestId::new_v4();
        let permission_id = PermissionId::new_v4();
        let mut app = DaemonApp::new("http://127.0.0.1:4780".to_string(), PathBuf::from("."));
        let mut request = EventEnvelope::new(OceanEvent::PermissionRequest {
            tool: "bash".to_string(),
            reason: "permission required for bash".to_string(),
            args: json!({"command": "rm -rf /tmp/demo"}),
        });
        request.request_id = Some(request_id);
        request.permission_id = Some(permission_id);

        daemon_apply_stream_event(&mut app, request);
        assert_eq!(app.pending_permissions.len(), 1);
        assert!(app
            .transcript
            .iter()
            .any(|line| line.contains("approval needed")));

        let mut decision = EventEnvelope::new(OceanEvent::PermissionDecision {
            allowed: false,
            reason: Some("operator denied".to_string()),
        });
        decision.request_id = Some(request_id);
        decision.permission_id = Some(permission_id);

        daemon_apply_stream_event(&mut app, decision);
        assert!(app.pending_permissions.is_empty());
        assert!(app.transcript.iter().any(|line| line.contains("denied")));
    }

    #[test]
    fn workspace_room_tabs_mark_active_room() {
        let line = workspace_room_tabs_line(WorkspaceRoom::Rev);
        assert!(line.contains("[F3 Rev]"));
        assert!(line.contains("F1 Orchestrator"));
    }

    #[test]
    fn daemon_session_cycle_keeps_new_session_slot() {
        let mut app = DaemonApp::new("http://127.0.0.1:4780".to_string(), PathBuf::from("."));
        let session_id = SessionId::new_v4();
        app.set_sessions(vec![SessionSummary {
            id: session_id,
            model: "test-model".to_string(),
            turns: 3,
            title: "Existing session".to_string(),
        }]);

        assert_eq!(app.selected_session_id(), None);
        app.cycle_session(1);
        assert_eq!(app.selected_session_id(), Some(session_id));
        app.cycle_session(-1);
        assert_eq!(app.selected_session_id(), None);
    }

    #[test]
    fn session_detail_lines_render_summary() {
        let session_id = SessionId::new_v4();
        let detail = SessionDetail {
            id: session_id,
            created_ms: 1,
            updated_ms: 2,
            model: "test-model".to_string(),
            provider: "test-provider".to_string(),
            turns: 7,
            title: "Selected session title".to_string(),
            state: SessionRunState::Running,
            resumable: true,
            active_requests: vec![RequestId::new_v4()],
            pending_permissions: vec![],
            transcript: vec![ocean_core::SessionTranscriptEntry {
                role: "assistant".to_string(),
                timestamp_ms: None,
                text: "assistant summary line".to_string(),
                tool_call_id: None,
                tool_name: None,
                is_error: None,
            }],
            tool_context: vec![],
            messages: vec![Value::String("raw message".to_string())],
        };

        let lines = DaemonApp::session_detail_lines(&detail).join("\n");
        assert!(lines.contains("Selected session title"));
        assert!(lines.contains("7 turns"));
        assert!(lines.contains("messages 1"));
        assert!(lines.contains("assistant summary line"));
    }

    #[test]
    fn mesh_tabs_line_marks_active_and_paused_state() {
        let line = mesh_tabs_line(MeshTab::Events, true);
        assert!(line.contains("[2:EVENTS]"));
        assert!(line.contains("PAUSED"));
    }

    #[test]
    fn active_center_tab_preserves_agents_board_mapping() {
        assert_eq!(active_center_tab(MeshTab::Agents), MeshTab::Board);
        assert_eq!(active_center_tab(MeshTab::Inbox), MeshTab::Inbox);
    }

    #[test]
    fn agent_source_dirs_preserve_project_then_home_precedence() {
        let root = Path::new("/tmp/ocean-test-root");
        let dirs = agent_source_dirs(root);

        assert_eq!(dirs[0], root.join(".pi/messenger/live/agents"));
        assert_eq!(dirs[1], root.join(".pi/messenger/registry"));
        assert!(dirs
            .iter()
            .any(|dir| dir.ends_with(".pi/agent/messenger/registry")));
        assert!(dirs
            .iter()
            .any(|dir| dir.ends_with(".pi/messenger/registry")));
    }

    #[test]
    fn classify_agent_uses_fallback_fields_from_registry_schema() {
        let record = AgentRecord {
            agent: None,
            name: Some("BRICK".to_string()),
            pid: Some(123),
            updated_at: None,
            model: Some("gpt-5.5".to_string()),
            provider: Some("openai".to_string()),
            preview: None,
            cwd: Some("/repo".to_string()),
            last_event: None,
            lifecycle: None,
            status_message: Some("ready".to_string()),
            activity: Some(AgentActivity {
                last_activity_at: Some("2026-05-25T13:13:49.262Z".to_string()),
                last_tool_call: Some("test: passed".to_string()),
            }),
        };

        let agent = classify_agent(record).expect("agent should classify");

        assert_eq!(agent.agent, "BRICK");
        assert_eq!(agent.preview.as_deref(), Some("ready"));
        assert_eq!(agent.last_event.as_deref(), Some("test: passed"));
    }

    #[test]
    fn pid_state_reader_detects_zombie_process() {
        let state = pid_state_from_reader(42, |_| Some("42 (pi) Z 1 2 3 4".to_string()));
        assert!(state.exists);
        assert!(state.zombie);
        assert!(!state.invalid);
    }

    #[test]
    fn read_agents_falls_back_to_home_registry_when_project_live_agents_missing() {
        let unique = format!(
            "ocean-tui-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        );
        let temp = env::temp_dir().join(unique);
        let root = temp.join("repo");
        let home = temp.join("home");
        fs::create_dir_all(root.join(".pi/messenger/registry")).expect("root registry");
        fs::create_dir_all(root.join(".pi/messenger/live/agents")).expect("root live agents");
        fs::create_dir_all(home.join(".pi/agent/messenger/registry")).expect("home registry");

        fs::write(
            home.join(".pi/agent/messenger/registry/PIXEL.json"),
            json!({
                "name": "PIXEL",
                "pid": 999,
                "cwd": "/repo",
                "statusMessage": "active",
                "activity": {"lastActivityAt": "2026-05-25T13:17:52.861Z", "lastToolCall": "test: passed"}
            })
            .to_string(),
        )
        .expect("write registry");

        let original_home = env::var_os("HOME");
        unsafe { env::set_var("HOME", &home) };
        let agents = read_agents(&root).expect("read agents");
        match original_home {
            Some(value) => unsafe { env::set_var("HOME", value) },
            None => unsafe { env::remove_var("HOME") },
        }
        fs::remove_dir_all(&temp).ok();

        assert!(agents.iter().any(|agent| agent.agent == "PIXEL"));
    }
}
