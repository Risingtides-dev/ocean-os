//! Track-0 room shell renderer.
//!
//! This module is render-only. It mirrors the live Tides Mesh tmux room/window
//! layout with fixture-friendly pane content. No backend/protocol behavior lives
//! here.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::Line,
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use ocean_core::RoomPanelSnapshot;

use crate::{DaemonApp, WorkspaceRoom};

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

fn draw_room_tidedash(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let unified = &app.support.unified;
    draw_lines_pane(
        frame,
        area,
        "TideDash — coming soon",
        vec![
            Line::from("TideDash campaign dashboard (placeholder · coming soon)"),
            Line::from("Campaigns        3 active"),
            Line::from("Deals            2 in progress"),
            Line::from("Pipeline         $48K"),
            Line::from(""),
            Line::from(crate::value_summary_line(
                &unified.summary,
                "tidedash_statuses",
                "status snapshots",
            )),
            Line::from(crate::value_summary_line(
                &unified.summary,
                "tidedash_errors",
                "error snapshots",
            )),
        ],
    );
}

fn draw_room_workops(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let unified = &app.support.unified;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    draw_lines_pane(
        frame,
        cols[0],
        "WorkOps board — coming soon",
        vec![
            Line::from("WorkOps board (placeholder · coming soon)"),
            Line::from(crate::value_summary_line(
                &unified.summary,
                "ops_attention",
                "ops attention",
            )),
            Line::from("Fixtures: services, jobs, queue health, operator alerts"),
            Line::from("WorkOps remains provisional / out-of-scope."),
        ],
    );
    draw_lines_pane(
        frame,
        cols[1],
        "bash / ops shell",
        vec![
            Line::from("$ ./scripts/operator-layout.sh"),
            Line::from("fixture: tailing workops.log"),
            Line::from("fixture: all watched jobs idle"),
            Line::from(format!("operator status: {}", app.status)),
            Line::from("WorkOps room is provisional."),
        ],
    );
}

fn draw_room_worldmap(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let mesh = &app.support.mesh;
    let mut lines = vec![
        Line::from("WorldMap presence pane (placeholder · coming soon)"),
        Line::from(""),
        Line::from("UTC  14:32  ● Orchestrator  ● KNOX  ● BRICK"),
        Line::from("PST  06:32  ○ Charlotte     ○ Henry"),
        Line::from("JST  23:32  ◉ Rev           ◉ PIXEL"),
        Line::from("CET  15:32  ○ PM"),
        Line::from(""),
        Line::from("Live agent records:"),
    ];
    if mesh.agents.is_empty() {
        lines.push(Line::from("fixture: no live registry rows loaded"));
    } else {
        lines.extend(mesh.agents.iter().take(10).map(|agent| {
            let state = match agent.presence {
                crate::AgentPresence::Active => "active",
                crate::AgentPresence::Away => "away",
                crate::AgentPresence::Stale => "stale",
            };
            Line::from(format!(
                "{} · {} · {}",
                crate::compact_text(&agent.agent, 16),
                state,
                crate::time_ago(agent.updated_at.as_deref())
            ))
        }));
    }
    draw_lines_pane(frame, area, "WorldMap — coming soon", lines);
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
