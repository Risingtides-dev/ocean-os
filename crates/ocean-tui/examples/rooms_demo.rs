//! Room Shell Demo — Ratatui room layouts matching tmux system
//!
//! Run: cargo run -p ocean-tui --example rooms_demo
//!
//! Keys: F1-F7 jump rooms, Tab cycles, q/Esc quits.

use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Room {
    Orchestrator,
    Writers,
    Rev,
    TideDash,
    WorkOps,
    WorldMap,
    PM,
}

impl Room {
    fn label(self) -> &'static str {
        match self {
            Self::PM => "PM",
            Self::Writers => "Writers Room",
            Self::Orchestrator => "ORCH + MESH",
            Self::Rev => "Review Room",
            Self::TideDash => "TideDash",
            Self::WorkOps => "WorkOps",
            Self::WorldMap => "WorldMap",
        }
    }

    fn all() -> [Self; 7] {
        [
            Self::PM,
            Self::Writers,
            Self::Orchestrator,
            Self::Rev,
            Self::TideDash,
            Self::WorkOps,
            Self::WorldMap,
        ]
    }

    fn next(self) -> Self {
        match self {
            Self::PM => Self::Writers,
            Self::Writers => Self::Orchestrator,
            Self::Orchestrator => Self::Rev,
            Self::Rev => Self::TideDash,
            Self::TideDash => Self::WorkOps,
            Self::WorkOps => Self::WorldMap,
            Self::WorldMap => Self::PM,
        }
    }

    fn from_f(key: KeyCode) -> Option<Self> {
        match key {
            KeyCode::F(1) => Some(Self::PM),
            KeyCode::F(2) => Some(Self::Writers),
            KeyCode::F(3) => Some(Self::Orchestrator),
            KeyCode::F(4) => Some(Self::Rev),
            KeyCode::F(5) => Some(Self::TideDash),
            KeyCode::F(6) => Some(Self::WorkOps),
            KeyCode::F(7) => Some(Self::WorldMap),
            _ => None,
        }
    }
}

struct App {
    active_room: Room,
}

fn main() -> anyhow::Result<()> {
    let mut app = App {
        active_room: Room::Orchestrator,
    };

    let mut stdout = std::io::stdout();
    enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    let mut last_draw = Instant::now();
    let redraw_every = Duration::from_millis(250);

    loop {
        if last_draw.elapsed() >= redraw_every {
            terminal.draw(|frame| draw_ui(frame, &app))?;
            last_draw = Instant::now();
        }

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Tab => app.active_room = app.active_room.next(),
                    code => {
                        if let Some(room) = Room::from_f(code) {
                            app.active_room = room;
                        }
                    }
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .split(area);

    draw_header(frame, layout[0], app);
    draw_room_body(frame, layout[1], app);
    draw_footer(frame, layout[2], app);
}

fn draw_header(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    let tabs: Vec<Span> = Room::all()
        .iter()
        .enumerate()
        .flat_map(|(idx, room)| {
            let label = format!("F{} {}", idx + 1, room.label());
            let style = if *room == app.active_room {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            vec![Span::styled(label, style), Span::raw("  ")]
        })
        .collect();

    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "◉ Ocean TUI",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  —  "),
            Span::styled("Room Shell Demo", Style::default().fg(Color::White)),
        ]),
        Line::from(tabs),
        Line::from(format!(
            "Active room: {}  |  Tab cycle  |  F1-F7 jump  |  q quit",
            app.active_room.label()
        )),
    ])
    .block(Block::default().title("Header").borders(Borders::ALL));
    frame.render_widget(header, area);
}

fn draw_footer(frame: &mut ratatui::Frame<'_>, area: Rect, _app: &App) {
    let footer = Paragraph::new("Tab/F1-F7 rooms · q/Esc quit · Fake data demo")
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, area);
}

fn draw_room_body(frame: &mut ratatui::Frame<'_>, area: Rect, app: &App) {
    match app.active_room {
        Room::Orchestrator => draw_room_orchestrator(frame, area),
        Room::Writers => draw_room_writers(frame, area),
        Room::Rev => draw_room_rev(frame, area),
        Room::TideDash => draw_room_tidedash(frame, area),
        Room::WorkOps => draw_room_workops(frame, area),
        Room::WorldMap => draw_room_worldmap(frame, area),
        Room::PM => draw_room_pm(frame, area),
    }
}

// ─── Orchestrator Room — 3-column mesh floor ───────────────────────────────
fn draw_room_orchestrator(frame: &mut ratatui::Frame<'_>, area: Rect) {
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

    draw_pane(
        frame,
        left[0],
        "Glyph",
        &[
            "◉ Glyph / ledger",
            "tasks: 29 total",
            "events: 12 live",
            "agents: 5 active / 2 away / 0 stale",
        ],
    );
    draw_pane(
        frame,
        left[1],
        "KNOX",
        &[
            "role: review gate",
            "presence: active",
            "last: 2s ago",
            "task-29 docs review PASS",
        ],
    );
    draw_pane(
        frame,
        left[2],
        "Charlotte",
        &[
            "role: research",
            "presence: active",
            "last: 45s ago",
            "delivered ratatui brief",
        ],
    );

    draw_pane(
        frame,
        center[0],
        "Board / Events",
        &[
            "14:32  task_done  Henry → task-29",
            "14:30  task_start PIXEL → task-32",
            "14:28  message   Charlotte → Henry",
            "14:25  review    KNOX → task-29 PASS",
            "14:20  plan      Orchestrator → wave 4",
        ],
    );
    draw_pane(
        frame,
        center[1],
        "Orchestrator Control",
        &[
            "agent: Orchestrator",
            "checked: 3s ago",
            "status: all tasks complete",
            "done: 29  active: 0",
        ],
    );

    draw_pane(
        frame,
        right[0],
        "BRICK",
        &[
            "role: backend/runtime",
            "presence: active",
            "last: 8s ago",
            "runtime cancellation wired",
        ],
    );
    draw_pane(
        frame,
        right[1],
        "PIXEL",
        &[
            "role: frontend/tui",
            "presence: active",
            "last: 12s ago",
            "mesh floor layout active",
        ],
    );
}

// ─── Writers Room — NoteDash + Henry + shell ───────────────────────────────
fn draw_room_writers(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(cols[1]);

    draw_pane(
        frame,
        cols[0],
        "NoteDash",
        &[
            "# Drafts",
            "- Ocean Runtime Operator Guide (editing)",
            "- TUI Room Shell PRD (review)",
            "",
            "# Sources",
            "- docs/OCEAN_TUI_TMUX_LAYOUT_MAP.md",
            "- docs/research/ratatui-impl-brief.md",
            "",
            "# Actions",
            "- [ ] Sanitize docs task-29",
            "- [x] KNOX review PASS",
        ],
    );
    draw_pane(
        frame,
        right[0],
        "Henry",
        &[
            "Henry: active",
            "Working on task-29 docs sanitation",
            "Reserved: OCEAN_RUNTIME_OPERATOR_GUIDE.md",
            "",
            "Last: sent handoff to Orchestrator",
        ],
    );
    draw_pane(
        frame,
        right[1],
        "Shell / Context",
        &[
            "$ cargo check -p ocean-tui",
            "    Checking ocean-tui v0.1.0",
            "    Finished dev [unoptimized]",
            "",
            "$ ",
        ],
    );
}

// ─── Rev Room — reviewer + board + git + context ───────────────────────────
fn draw_room_rev(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(47), Constraint::Percentage(53)])
        .split(area);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(12), Constraint::Min(8)])
        .split(cols[1]);
    let right_bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(right[1]);

    draw_pane(
        frame,
        cols[0],
        "Rev Reviewer",
        &[
            "Rev: active",
            "Review queue: 1 pending",
            "",
            "Current: PR #42 — ratatui room shell",
            "Author: PIXEL",
            "Branch: feature/room-shell",
            "",
            "Status: awaiting KNOX sign-off",
        ],
    );
    draw_pane(
        frame,
        right[0],
        "Review Board",
        &[
            "PR #42  ratatui room shell  ·  PIXEL  ·  review",
            "PR #41  telegram voice fix   ·  BRICK  ·  done",
            "PR #40  provider auth        ·  BRICK  ·  done",
        ],
    );
    draw_pane(
        frame,
        right_bottom[0],
        "Git UI",
        &[
            "feature/room-shell",
            " M crates/ocean-tui/src/main.rs",
            "?? crates/ocean-tui/src/rooms.rs",
            "",
            "3 files changed, 142 insertions",
        ],
    );
    draw_pane(
        frame,
        right_bottom[1],
        "Review Context",
        &[
            "Files changed: 3",
            "+142 -8 lines",
            "",
            "Risk: low",
            "No protocol changes.",
        ],
    );
}

// ─── TideDash Room — single pane board ─────────────────────────────────────
fn draw_room_tidedash(frame: &mut ratatui::Frame<'_>, area: Rect) {
    draw_pane(
        frame,
        area,
        "TideDash",
        &[
            "Campaigns        3 active",
            "Deals            2 in progress",
            "Revenue          $12.4K MTD",
            "Pipeline         $48K",
            "",
            "Status Snapshots",
            "  ocean-tui      ████████░░  80%",
            "  ocean-core     ██████████  100%",
            "  telegram-bridge ██████░░░░  60%",
            "",
            "Errors: 0",
        ],
    );
}

// ─── WorkOps Room — symmetric board / shell split ──────────────────────────
fn draw_room_workops(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    draw_pane(
        frame,
        cols[0],
        "Ops Board",
        &[
            "Systems",
            "  daemon    ● ok     port 4780",
            "  tui       ● ok     pid 18422",
            "  mesh      ● ok     7 agents",
            "",
            "Ports",
            "  4780  ocean-daemon",
            "  8080  tide-net",
            "",
            "Cloud Health",
            "  api       ● ok     23ms",
            "  database  ● ok     8ms",
        ],
    );
    draw_pane(
        frame,
        cols[1],
        "Ops Shell / Output",
        &[
            "$ ocean status",
            "daemon: ok",
            "tui: running",
            "mesh: 7 agents active",
            "",
            "$ ocean logs --tail",
            "[14:32:01] task_done: task-29",
            "[14:30:45] task_start: task-32",
            "[14:28:12] message: Charlotte → Henry",
        ],
    );
}

// ─── WorldMap Room — map / presence board ──────────────────────────────────
fn draw_room_worldmap(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    draw_pane(
        frame,
        rows[0],
        "World Map / Presence",
        &[
            "  UTC  14:32  ● Orchestrator  ● KNOX      ● BRICK",
            "  PST  06:32  ○ Charlotte     ○ Henry",
            "  JST  23:32  ◉ Rev           ◉ PIXEL",
            "  CET  15:32  ○ PM",
            "",
            "● active  ◐ working  ○ idle",
        ],
    );
    draw_pane(
        frame,
        bottom[0],
        "Active Agents",
        &[
            "Orchestrator  active  3s",
            "KNOX          active  2s",
            "BRICK         active  8s",
            "PIXEL         active  12s",
            "Rev           away    45s",
        ],
    );
    draw_pane(
        frame,
        bottom[1],
        "Team Timezones",
        &[
            "Orchestrator  UTC",
            "Charlotte     PST",
            "Henry         PST",
            "Rev           JST",
            "PIXEL         JST",
            "KNOX          UTC",
            "BRICK         UTC",
            "PM            CET",
        ],
    );
}

// ─── PM Room — product task board ──────────────────────────────────────────
fn draw_room_pm(frame: &mut ratatui::Frame<'_>, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    draw_pane(
        frame,
        cols[0],
        "PM Task Board",
        &[
            "TO DO",
            "  - Telegram voice-note validation",
            "  - Release gate human decision",
            "",
            "IN PROGRESS",
            "  - TUI room shell (PIXEL)",
            "  - Docs sanitation (Henry)",
            "",
            "DONE",
            "  - Model mapping hotfix (BRICK)",
            "  - MeshFloor layout (PIXEL)",
        ],
    );
    draw_pane(
        frame,
        cols[1],
        "Integrations",
        &[
            "Telegram    ● connected",
            "GitHub      ● connected",
            "Linear      ○ pending",
            "Stripe      ○ pending",
            "",
            "Readiness",
            "  api       ok",
            "  tui       ok",
            "  daemon    ok",
        ],
    );
}

// ─── Helper ────────────────────────────────────────────────────────────────
fn draw_pane(frame: &mut ratatui::Frame<'_>, area: Rect, title: &str, lines: &[&str]) {
    let text: Vec<Line> = lines.iter().map(|l| Line::from(*l)).collect();
    let paragraph = Paragraph::new(text)
        .block(Block::default().title(title).borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}
