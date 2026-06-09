//! Track-0 room shell renderer.
//!
//! This module is render-only — it projects state the [`crate::DaemonApp`]
//! already holds (sessions, requests, tool timeline, daemon health, provider
//! readiness, the mesh agent registry, and the unified event feed) into the
//! per-room pane layout. The TideDash / WorkOps / WorldMap rooms render live
//! daemon data rather than fixtures (OCEAN-233). No backend/protocol behavior
//! lives here.

use chrono::Utc;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use ocean_core::RoomPanelSnapshot;

use crate::{DaemonApp, WorkspaceRoom};

/// Render an epoch-millisecond timestamp as a compact relative age ("12s",
/// "4m", "3h", "2d"), matching the bucketing used by [`crate::time_ago`] so the
/// room panes read consistently with the rest of the TUI.
fn relative_age_ms(updated_ms: i64) -> String {
    let delta = Utc::now().timestamp_millis().saturating_sub(updated_ms);
    if delta < 0 {
        return "now".to_string();
    }
    let secs = delta / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 172_800 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Render the selected Track-0 room body into the provided area.
pub fn draw_daemon_room_body(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    match app.active_room {
        WorkspaceRoom::PM => draw_room_pm(frame, area, app),
        WorkspaceRoom::Writers => draw_room_writers(frame, area, app),
        WorkspaceRoom::Orchestrator => draw_room_orch_mesh(frame, area, app),
        WorkspaceRoom::Rev => draw_room_review(frame, area, app),
        WorkspaceRoom::TideDash => draw_room_tidedash(frame, area, app),
        WorkspaceRoom::WorkOps => draw_room_workops(frame, area, app),
        WorkspaceRoom::WorldMap => draw_room_worldmap(frame, area, app),
    }
}

fn draw_room_pm(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    if app.active_room == crate::WorkspaceRoom::PM {
        draw_runtime_room_panes(frame, area, app);
    } else {
        draw_lines_pane(
            frame,
            area,
            "PM",
            vec![Line::from(
                "PM room is provisional until runtime room selected",
            )],
        );
    }
}

fn draw_room_writers(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    if app.active_room == crate::WorkspaceRoom::Writers {
        draw_runtime_room_panes(frame, area, app);
        return;
    }

    draw_lines_pane(
        frame,
        area,
        "Writers Room",
        vec![Line::from(
            "writers room is provisional without runtime room selection",
        )],
    );
}

fn draw_room_orch_mesh(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    if app.active_room == crate::WorkspaceRoom::Orchestrator {
        draw_runtime_room_panes(frame, area, app);
        return;
    }

    draw_lines_pane(
        frame,
        area,
        "ORCH + MESH",
        vec![Line::from(
            "orchestrator room is provisional without runtime room selection",
        )],
    );
}

fn draw_room_review(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    if app.active_room == crate::WorkspaceRoom::Rev {
        draw_runtime_room_panes(frame, area, app);
        return;
    }

    draw_lines_pane(
        frame,
        area,
        "Review Room",
        vec![Line::from(
            "review room is provisional without runtime room selection",
        )],
    );
}

/// TideDash = activity / flow. Renders the live tide of work moving through the
/// daemon: a session-flow rollup on the left (sessions, turns, requests in
/// flight) and a real activity tail on the right (the unified event feed when
/// present, else the in-process SSE activity buffer). No fixtures — every line
/// is derived from daemon state the TUI already holds (OCEAN-233).
fn draw_room_tidedash(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
        .split(area);

    draw_lines_pane(frame, cols[0], "Tide — flow", tidedash_flow_lines(app));

    let feed_width = cols[1].width.saturating_sub(2) as usize;
    draw_lines_pane(
        frame,
        cols[1],
        "Tide — recent activity",
        tidedash_activity_lines(app, feed_width),
    );
}

/// Left TideDash pane: rolled-up counts of what is flowing through the daemon
/// right now — sessions, total turns across them, and live request states.
fn tidedash_flow_lines(app: &DaemonApp) -> Vec<Line<'static>> {
    let total_turns: u32 = app.sessions.iter().map(|session| session.turns).sum();
    let running = app
        .requests
        .iter()
        .filter(|request| matches!(request.state, ocean_core::RequestState::Running))
        .count();
    let queued = app
        .requests
        .iter()
        .filter(|request| matches!(request.state, ocean_core::RequestState::Queued))
        .count();
    let waiting = app.pending_permissions.len();

    let mut lines = vec![
        Line::from(format!("sessions          {}", app.sessions.len())),
        Line::from(format!("turns (total)     {total_turns}")),
        Line::from(format!("requests          {}", app.requests.len())),
        Line::from(format!("  running         {running}")),
        Line::from(format!("  queued          {queued}")),
        Line::from(format!("  awaiting ok     {waiting}")),
        Line::from(format!("tool events       {}", app.tool_timeline.len())),
        Line::from(""),
        Line::from("Most recent sessions:"),
    ];

    if app.sessions.is_empty() {
        lines.push(Line::from("  no sessions yet — start a turn to see flow"));
    } else {
        lines.extend(app.sessions.iter().take(6).map(|session| {
            let age = session
                .updated_ms
                .map(relative_age_ms)
                .unwrap_or_else(|| "—".to_string());
            Line::from(format!(
                "  [{}] {:>3}t {:>4} {}",
                crate::short_id(session.id),
                session.turns,
                age,
                crate::compact_text(&session.title, 22)
            ))
        }));
    }

    lines
}

/// Right TideDash pane: the activity tail. Prefers the unified event feed
/// (`.pi/unified/events.jsonl`) when populated, otherwise falls back to the
/// in-process SSE activity buffer so the pane is never empty when the daemon
/// is live.
fn tidedash_activity_lines(app: &DaemonApp, width: usize) -> Vec<Line<'static>> {
    let unified = &app.support.unified;
    if !unified.events.is_empty() {
        let mut lines = Vec::new();
        if let Some(generated) = unified.generated_at.as_deref() {
            lines.push(Line::from(format!(
                "unified feed · updated {}",
                crate::time_ago(Some(generated))
            )));
        }
        lines.extend(
            unified
                .events
                .iter()
                .take(12)
                .map(|event| Line::from(crate::render_unified_event(event, width))),
        );
        return lines;
    }

    if app.activity.is_empty() {
        return vec![
            Line::from("No activity yet."),
            Line::from("Live turns + tool calls stream here as they run."),
            Line::from(format!("daemon: {}", app.stream_status)),
        ];
    }

    app.activity
        .iter()
        .rev()
        .take(12)
        .map(|line| Line::from(crate::compact_text(line, width)))
        .collect()
}

/// WorkOps = work / ops. Left pane is the live work queue (requests in flight +
/// the most recent tool calls the runtime executed); right pane is the ops
/// console (daemon health, provider/model readiness, mesh task rollup, pending
/// approvals). All real daemon state — no fixtures (OCEAN-233).
fn draw_room_workops(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let queue_width = cols[0].width.saturating_sub(2) as usize;
    draw_lines_pane(
        frame,
        cols[0],
        "WorkOps — work queue",
        workops_queue_lines(app, queue_width),
    );

    draw_lines_pane(frame, cols[1], "WorkOps — ops console", workops_console_lines(app));
}

/// Left WorkOps pane: requests currently in flight, then a tail of the most
/// recent tool-call events from the runtime so the operator can see what work
/// is actually moving.
fn workops_queue_lines(app: &DaemonApp, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from("Requests:")];
    if app.requests.is_empty() {
        lines.push(Line::from("  no requests yet"));
    } else {
        lines.extend(app.requests.iter().take(6).map(|request| {
            let marker = if request.state.is_cancellable() {
                "•"
            } else {
                " "
            };
            let message = request
                .message
                .as_deref()
                .map(|m| crate::compact_text(m, width.saturating_sub(28)))
                .unwrap_or_else(|| "(no message)".to_string());
            Line::from(format!(
                "{marker} [{}] {:<18} {}",
                crate::short_id(request.request_id),
                crate::state_label(request.state),
                message
            ))
        }));
    }

    lines.push(Line::from(""));
    lines.push(Line::from("Recent tool calls:"));
    if app.tool_timeline.is_empty() {
        lines.push(Line::from("  none yet — waiting on SSE tool events"));
    } else {
        lines.extend(
            app.tool_timeline
                .iter()
                .rev()
                .take(6)
                .map(|entry| {
                    Line::from(format!(
                        "  {:>4} {:<12} {:<9} {}",
                        crate::time_ago(Some(&entry.at.to_rfc3339())),
                        crate::compact_text(&entry.tool, 12),
                        crate::compact_text(&entry.phase, 9),
                        crate::compact_text(&entry.message, width.saturating_sub(32))
                    ))
                }),
        );
    }

    lines
}

/// Right WorkOps pane: the ops console — daemon health/backend/version, the
/// active provider+model and credential readiness, a mesh task rollup, and any
/// permission approvals waiting on the operator.
fn workops_console_lines(app: &DaemonApp) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(crate::compact_text(&app.health_summary(), 58)),
        Line::from(format!("checked: {}", app.checked_text())),
        Line::from(""),
        Line::from(format!(
            "provider: {}",
            crate::compact_text(&app.model_config.provider, 40)
        )),
        Line::from(format!(
            "model:    {}",
            crate::compact_text(&app.model_config.model, 40)
        )),
        Line::from(format!("api key:  {}", app.model_config.credential)),
        Line::from(""),
    ];

    let counts = &app.support.mesh.counts;
    let total = app.support.mesh.tasks.len();
    if total == 0 {
        lines.push(Line::from("mesh tasks: none loaded"));
    } else {
        lines.push(Line::from(format!(
            "mesh tasks: {total} total · {} in-progress · {} blocked · {} done",
            counts.in_progress, counts.blocked, counts.done
        )));
    }

    let pending = app.pending_permissions.len();
    if pending == 0 {
        lines.push(Line::from("approvals:  none waiting"));
    } else {
        lines.push(Line::from(format!(
            "approvals:  {pending} waiting (Shift-Y allow · Shift-N deny)"
        )));
        if let Some(next) = app.pending_permissions.first() {
            lines.push(Line::from(format!(
                "  → {} · {}",
                crate::compact_text(&next.tool, 18),
                crate::compact_text(&next.reason, 30)
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "status: {}",
        crate::compact_text(&app.status, 50)
    )));

    lines
}

/// WorldMap = system topology. Renders the live shape of the system the TUI is
/// steering: the daemon endpoint + health/backend/version, the active
/// provider/model, runtime counts (sessions/requests/agents), and the live
/// agent-registry presence map. Replaces the old fixed-timezone fixture with
/// real daemon topology (OCEAN-233).
fn draw_room_worldmap(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(48), Constraint::Percentage(52)])
        .split(area);

    draw_lines_pane(frame, cols[0], "WorldMap — daemon topology", worldmap_topology_lines(app));

    let width = cols[1].width.saturating_sub(2) as usize;
    draw_lines_pane(
        frame,
        cols[1],
        "WorldMap — agent presence",
        worldmap_presence_lines(app, width),
    );
}

/// Left WorldMap pane: where the daemon is, whether it is healthy, what it is
/// running on, and the live runtime counts the TUI is tracking.
fn worldmap_topology_lines(app: &DaemonApp) -> Vec<Line<'static>> {
    let (label, _) = app.status_label();
    let agents = &app.support.mesh.agents;
    let active_agents = agents
        .iter()
        .filter(|agent| matches!(agent.presence, crate::AgentPresence::Active))
        .count();

    vec![
        Line::from(format!("daemon:   {}", crate::compact_text(&app.url, 40))),
        Line::from(format!("status:   {label}")),
        Line::from(crate::compact_text(&app.health_summary(), 58)),
        Line::from(format!("checked:  {}", app.checked_text())),
        Line::from(""),
        Line::from(format!(
            "provider: {}",
            crate::compact_text(&app.model_config.provider, 40)
        )),
        Line::from(format!(
            "model:    {}",
            crate::compact_text(&app.model_config.model, 40)
        )),
        Line::from(""),
        Line::from("Runtime counts:"),
        Line::from(format!("  sessions   {}", app.sessions.len())),
        Line::from(format!("  requests   {}", app.requests.len())),
        Line::from(format!("  rooms      {}", app.room_snapshots.len())),
        Line::from(format!(
            "  agents     {} ({} active)",
            agents.len(),
            active_agents
        )),
    ]
}

/// Right WorldMap pane: the live agent-registry presence map — who is online,
/// their presence state, last-seen age, and a short preview of what they are
/// doing. Reads the same mesh registry the dedicated mesh view uses.
fn worldmap_presence_lines(app: &DaemonApp, width: usize) -> Vec<Line<'static>> {
    let agents = &app.support.mesh.agents;
    if agents.is_empty() {
        return vec![
            Line::from("No live agent records."),
            Line::from("Populated from the mesh registry under the project root."),
        ];
    }

    agents
        .iter()
        .take(12)
        .map(|agent| {
            let dot = match agent.presence {
                crate::AgentPresence::Active => '●',
                crate::AgentPresence::Away => '◐',
                crate::AgentPresence::Stale => '○',
            };
            let state = match agent.presence {
                crate::AgentPresence::Active => "active",
                crate::AgentPresence::Away => "away",
                crate::AgentPresence::Stale => "stale",
            };
            let preview = agent
                .preview
                .as_deref()
                .or(agent.last_event.as_deref())
                .unwrap_or("—");
            Line::from(format!(
                "{dot} {:<14} {:<6} {:>4}  {}",
                crate::compact_text(&agent.agent, 14),
                state,
                crate::time_ago(agent.updated_at.as_deref()),
                crate::compact_text(preview, width.saturating_sub(34))
            ))
        })
        .collect()
}

fn draw_lines_pane(frame: &mut Frame<'_>, area: Rect, title: &str, lines: Vec<Line<'static>>) {
    let paragraph = Paragraph::new(lines)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn draw_agent_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    mesh: &crate::MeshState,
    name: &str,
    role: &str,
) {
    let agent = mesh
        .agents
        .iter()
        .find(|agent| agent.agent.eq_ignore_ascii_case(name));
    let (presence, last, preview) = if let Some(agent) = agent {
        (
            match agent.presence {
                crate::AgentPresence::Active => "active",
                crate::AgentPresence::Away => "away",
                crate::AgentPresence::Stale => "stale",
            },
            crate::time_ago(agent.updated_at.as_deref()),
            agent
                .preview
                .as_deref()
                .or(agent.last_event.as_deref())
                .unwrap_or("no preview"),
        )
    } else {
        (
            "fixture/offline",
            "—".to_string(),
            "pane preserved from tmux layout",
        )
    };
    draw_lines_pane(
        frame,
        area,
        name,
        vec![
            Line::from(format!("role: {role}")),
            Line::from(format!("presence: {presence}")),
            Line::from(format!("last: {last}")),
            Line::from(crate::compact_text(
                preview,
                area.width.saturating_sub(4) as usize,
            )),
        ],
    );
}

fn draw_runtime_room_panes(frame: &mut Frame<'_>, area: Rect, app: &crate::DaemonApp) {
    let Some(snapshot) = app.active_room_snapshot() else {
        draw_lines_pane(
            frame,
            area,
            "runtime snapshot",
            vec![
                Line::from("No room snapshot data yet."),
                Line::from("Retry refresh to load /v1/rooms."),
            ],
        );
        return;
    };

    match app.active_room {
        crate::WorkspaceRoom::PM => draw_room_pm_runtime(frame, area, snapshot),
        crate::WorkspaceRoom::Writers => draw_room_writers_runtime(frame, area, snapshot),
        crate::WorkspaceRoom::Orchestrator => {
            draw_room_orch_mesh_runtime(frame, area, snapshot, app)
        }
        crate::WorkspaceRoom::Rev => draw_room_review_runtime(frame, area, snapshot),
        _ => draw_lines_pane(
            frame,
            area,
            "runtime room",
            vec![
                Line::from("room has no runtime renderer"),
                Line::from(snapshot.title.clone()),
            ],
        ),
    }
}

fn draw_room_pm_runtime(frame: &mut Frame<'_>, area: Rect, snapshot: &crate::RoomSnapshot) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    let left_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(cols[0]);

    draw_panel_pane(
        frame,
        left_rows[0],
        snapshot.panels.first().map(|panel| panel.title.as_str()),
        snapshot.panels.first(),
    );
    draw_panel_pane(
        frame,
        left_rows[1],
        snapshot.panels.get(1).map(|panel| panel.title.as_str()),
        snapshot.panels.get(1),
    );

    draw_panel_pane(
        frame,
        cols[1],
        snapshot.panels.get(2).map(|panel| panel.title.as_str()),
        snapshot.panels.get(2),
    );
}

fn draw_room_writers_runtime(frame: &mut Frame<'_>, area: Rect, snapshot: &crate::RoomSnapshot) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    draw_panel_pane(
        frame,
        cols[0],
        Some("Writers / Drafts"),
        snapshot.panels.first(),
    );

    let right_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(cols[1]);

    draw_panel_pane(
        frame,
        right_rows[0],
        snapshot.panels.get(1).map(|panel| panel.title.as_str()),
        snapshot.panels.get(1),
    );
    draw_panel_pane(
        frame,
        right_rows[1],
        snapshot.panels.get(2).map(|panel| panel.title.as_str()),
        snapshot.panels.get(2),
    );
}

fn draw_room_orch_mesh_runtime(
    frame: &mut Frame<'_>,
    area: Rect,
    snapshot: &crate::RoomSnapshot,
    app: &crate::DaemonApp,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(31),
            Constraint::Percentage(49),
            Constraint::Percentage(20),
        ])
        .split(area);

    let left_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(33),
            Constraint::Percentage(34),
        ])
        .split(cols[0]);

    draw_panel_pane(
        frame,
        left_rows[0],
        snapshot.panels.first().map(|panel| panel.title.as_str()),
        snapshot.panels.first(),
    );
    draw_panel_pane(
        frame,
        left_rows[1],
        snapshot.panels.get(1).map(|panel| panel.title.as_str()),
        snapshot.panels.get(1),
    );
    draw_panel_pane(
        frame,
        left_rows[2],
        snapshot.panels.get(2).map(|panel| panel.title.as_str()),
        snapshot.panels.get(2),
    );

    // Split the center column: runtime control rail on top, the live Longhouse
    // council (with quorum meter) on the bottom (OCEAN-42).
    let center_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(cols[1]);

    if snapshot.panels.len() > 3 {
        draw_panel_pane(
            frame,
            center_rows[0],
            snapshot.panels.get(3).map(|panel| panel.title.as_str()),
            snapshot.panels.get(3),
        );
    } else {
        draw_panel_pane(frame, center_rows[0], Some("Runtime control rail"), None);
    }

    draw_lines_pane(
        frame,
        center_rows[1],
        "Longhouse council",
        app.longhouse_lines(center_rows[1].width.saturating_sub(2) as usize),
    );

    draw_panel_pane(
        frame,
        cols[2],
        Some("Orch + Mesh live state"),
        snapshot.panels.first(),
    );
}

fn draw_room_review_runtime(frame: &mut Frame<'_>, area: Rect, snapshot: &crate::RoomSnapshot) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let right_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(cols[1]);

    draw_panel_pane(
        frame,
        cols[0],
        Some("Review queue"),
        snapshot.panels.first(),
    );
    draw_panel_pane(
        frame,
        right_rows[0],
        Some("Evidence"),
        snapshot.panels.get(1),
    );
    draw_panel_pane(
        frame,
        right_rows[1],
        Some("Release gate"),
        snapshot.panels.get(2),
    );
}

fn draw_panel_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    title: Option<&str>,
    panel: Option<&RoomPanelSnapshot>,
) {
    let mut lines = Vec::new();
    if let Some(panel) = panel {
        lines.push(Line::from(format!(
            "{} [{}] {}",
            panel.title, panel.kind, panel.status
        )));
        lines.extend(panel.lines.iter().map(|line| {
            Line::from(crate::compact_text(
                line,
                area.width.saturating_sub(2) as usize,
            ))
        }));
    } else {
        lines.push(Line::from("No panel data"));
    }

    draw_lines_pane(frame, area, title.unwrap_or("Room panel"), lines);
}
