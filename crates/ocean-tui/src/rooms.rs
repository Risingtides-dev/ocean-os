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
    draw_lines_pane(
        frame,
        area,
        "PM",
        app.transcript_lines(
            area.width.saturating_sub(2) as usize,
            area.height.saturating_sub(2) as usize,
        ),
    );
}

fn draw_room_writers(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let mesh = &app.support.mesh;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(cols[1]);

    draw_lines_pane(
        frame,
        cols[0],
        "NoteDash",
        vec![
            Line::from("# Notes / source queue"),
            Line::from("- Track-0 shell mirrors tmux layout"),
            Line::from("- Use fake data until live adapters are ready"),
            Line::from("- Keep room names exact"),
            Line::from(""),
            Line::from("# Drafts"),
            Line::from("- Operator proof checklist"),
            Line::from("- Ratatui room shell handoff"),
        ],
    );

    draw_lines_pane(
        frame,
        right[0],
        "π writer",
        vec![
            Line::from(format!("Henry: {}", crate::agent_presence(mesh, "Henry"))),
            Line::from("Role: writing / docs lane"),
            Line::from("Prompt: summarize room-shell proof after validation"),
            Line::from("Last: standing by for UI proof"),
        ],
    );

    draw_lines_pane(
        frame,
        right[1],
        "lynx / browser",
        vec![
            Line::from("Browser/context pane"),
            Line::from("  docs/OCEAN_TUI_TMUX_LAYOUT_MAP.md"),
            Line::from("  ratatui layout primitives"),
            Line::from("  tmux window snapshot"),
        ],
    );
}

fn draw_room_orch_mesh(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let mesh = &app.support.mesh;
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

    draw_lines_pane(
        frame,
        left[0],
        "Glyph",
        vec![
            Line::from("Glyph ledger / heartbeat"),
            Line::from(format!("tasks: {} total", mesh.tasks.len())),
            Line::from(format!("events: {} live", mesh.feed.len())),
            Line::from(format!(
                "agents: {} active / {} away / {} stale",
                mesh.agent_counts.active, mesh.agent_counts.away, mesh.agent_counts.stale
            )),
        ],
    );
    draw_agent_pane(frame, left[1], mesh, "KNOX", "review gate");
    draw_agent_pane(frame, left[2], mesh, "Charlotte", "research");

    let mut board_lines = vec![
        Line::from("TIDES-MESH Kanban / task / issues board"),
        Line::from(format!(
            "todo {} · active {} · review/block {} · done {}",
            mesh.counts.todo,
            mesh.counts.in_progress,
            mesh.counts.review + mesh.counts.blocked,
            mesh.counts.done
        )),
        Line::from(""),
    ];
    if mesh.feed.is_empty() {
        board_lines.extend([
            Line::from("fixture: BRICK patch running"),
            Line::from("fixture: KNOX review queued"),
            Line::from("fixture: PIXEL proving Ratatui shell"),
        ]);
    } else {
        board_lines.extend(
            mesh.feed
                .iter()
                .take(center[0].height.saturating_sub(5) as usize)
                .map(|event| Line::from(crate::render_feed_event(event, center[0].width as usize))),
        );
    }
    draw_lines_pane(frame, center[0], "Kanban / Tasks / Issues", board_lines);

    draw_lines_pane(
        frame,
        center[1],
        "Orchestrator pane %85",
        vec![
            Line::from("Restored Orchestrator control pane"),
            Line::from(format!("mesh agent: {}", app.mesh_agent)),
            Line::from(format!("checked: {}", app.checked_text())),
            Line::from(format!("room: {}", app.active_room.label())),
            Line::from(format!("status: {}", app.status)),
            Line::from("Input routes operator instruction here by default."),
        ],
    );

    draw_agent_pane(frame, right[0], mesh, "BRICK", "backend/runtime");
    draw_agent_pane(frame, right[1], mesh, "PIXEL", "frontend/tui");
}

fn draw_room_review(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let mesh = &app.support.mesh;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(47), Constraint::Percentage(53)])
        .split(area);
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(12), Constraint::Min(8)])
        .split(cols[1]);
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(right[1]);

    draw_lines_pane(
        frame,
        cols[0],
        "Rev Review",
        vec![
            Line::from(format!("Rev/KNOX: {}", crate::agent_presence(mesh, "KNOX"))),
            Line::from("Review workflow dominant pane"),
            Line::from("Current: Track-0 Ratatui shell"),
            Line::from("Expected: exact room names + pane proportions"),
            Line::from("No backend/protocol changes."),
        ],
    );

    let review_lines: Vec<Line<'static>> = mesh
        .tasks
        .iter()
        .filter(|task| matches!(task.status.as_str(), "review" | "blocked" | "milestone"))
        .take(right[0].height.saturating_sub(2) as usize)
        .map(|task| {
            Line::from(format!(
                "{} {} · {}",
                task.id,
                crate::compact_text(&task.title, 32),
                task.status
            ))
        })
        .collect();
    draw_lines_pane(
        frame,
        right[0],
        "WorkDash",
        if review_lines.is_empty() {
            vec![Line::from("fixture: review board waiting for KNOX gate")]
        } else {
            review_lines
        },
    );
    draw_lines_pane(
        frame,
        bottom[0],
        "lazygit",
        vec![
            Line::from("rev/ocean-runtime-tui-async"),
            Line::from(" M crates/ocean-tui/src/main.rs"),
            Line::from("?? crates/ocean-tui/src/rooms.rs"),
            Line::from("?? crates/ocean-tui/examples/rooms_demo.rs"),
        ],
    );
    draw_lines_pane(
        frame,
        bottom[1],
        "bash / context",
        vec![
            Line::from("$ cargo fmt --all"),
            Line::from("$ cargo clippy --all-targets --all-features"),
            Line::from("$ cargo test"),
            Line::from("manual: F1-F7 + typing smoke"),
        ],
    );
}

fn draw_room_tidedash(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let unified = &app.support.unified;
    draw_lines_pane(
        frame,
        area,
        "TideDash",
        vec![
            Line::from("TideDash python dashboard pane"),
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
        "python / ops board",
        vec![
            Line::from("WorkOps board"),
            Line::from(crate::value_summary_line(
                &unified.summary,
                "ops_attention",
                "ops attention",
            )),
            Line::from("Fixtures: services, jobs, queue health, operator alerts"),
            Line::from("Runtime health stays in the header support strip."),
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
        ],
    );
}

fn draw_room_worldmap(frame: &mut Frame<'_>, area: Rect, app: &DaemonApp) {
    let mesh = &app.support.mesh;
    let mut lines = vec![
        Line::from("WorldMap python pane"),
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
    draw_lines_pane(frame, area, "WorldMap", lines);
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
