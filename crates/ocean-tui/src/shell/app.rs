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
//! ⌃⌥6 terminal · Tab cycles focus · ⌃Q quits (⌃C passes to the PTY).

use std::path::PathBuf;

use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers};
use ocean_agent_sdk::{AgentSessionId, AgentTurnRequest};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use tokio::sync::mpsc;

use super::{
    action::Action,
    client::DaemonClient,
    component::Component,
    components::{
        chat::ChatComponent, editor::EditorComponent, file_tree::FileTreeComponent,
        graph::GraphComponent, pty_pane::PtyComponent, session_rail::SessionRailComponent,
    },
    event::{Event, EventHandler},
    theme::{self, g},
    tui,
};

const SESS_W: u16 = 30;
const TREE_W: u16 = 30;
const TERM_H: u16 = 14;

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
    status: String,
    should_quit: bool,
    actions_tx: mpsc::UnboundedSender<Action>,
    actions_rx: mpsc::UnboundedReceiver<Action>,
}

impl App {
    pub fn new(client: DaemonClient, workspace_root: String) -> Self {
        let (actions_tx, actions_rx) = mpsc::unbounded_channel();
        let root = PathBuf::from(&workspace_root);
        let mut app = Self {
            client,
            workspace_root,
            rail: SessionRailComponent::new(root.clone()),
            tree: FileTreeComponent::new(root.clone()),
            chat: ChatComponent::default(),
            pty: PtyComponent::default(),
            editor: EditorComponent::new(root.clone()),
            graph: GraphComponent::new(root),
            center: Center::Chat,
            focus: Focus::Sessions,
            session_id: None,
            status: "connecting…".into(),
            should_quit: false,
            actions_tx,
            actions_rx,
        };
        app.apply_focus();
        app
    }

    pub async fn run(mut self, terminal: &mut tui::Tui) -> anyhow::Result<()> {
        let mut events = EventHandler::new(30.0, 60.0);

        {
            let client = self.client.clone();
            let tx = self.actions_tx.clone();
            tokio::spawn(async move {
                match client.health().await {
                    Ok(h) => {
                        let _ = tx.send(Action::Status(format!("connected · {}", h.backend)));
                    }
                    Err(e) => {
                        let _ = tx.send(Action::Error(format!("daemon: {e}")));
                    }
                }
            });
        }

        while !self.should_quit {
            let Some(event) = events.next().await else { break };
            match event {
                Event::Render => {
                    terminal.draw(|f| self.draw(f))?;
                }
                Event::Tick => {
                    if let Some(a) = self.pty.tick() {
                        self.dispatch(a);
                    }
                    if let Some(a) = self.editor.tick() {
                        self.dispatch(a);
                    }
                }
                Event::Crossterm(evt) => self.on_crossterm(evt),
            }
            while let Ok(action) = self.actions_rx.try_recv() {
                self.dispatch(action);
            }
        }
        Ok(())
    }

    fn on_crossterm(&mut self, evt: CrosstermEvent) {
        if let CrosstermEvent::Key(k) = evt {
            if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('q') {
                self.should_quit = true;
                return;
            }
            // Tab cycles focus across the VISIBLE panes (never hides anything).
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
                    _ => {}
                }
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
            Action::Status(s) | Action::Error(s) => self.status = s.clone(),
            Action::SessionBound(id) => self.session_id = Some(*id),
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
            Action::ResumeSession { id, path } => {
                self.chat
                    .load_history(crate::shell::sessions::load_transcript(path));
                self.session_id = Some(*id);
                self.rail.live_id = Some(id.0.to_string());
                self.client.spawn_event_stream(*id, self.actions_tx.clone());
                self.status = format!("resumed session {:.8}", id.0.to_string());
                self.center = Center::Chat;
                self.focus_to(Focus::Center);
            }
            Action::CycleFocus => self.cycle_focus(),
            _ => {}
        }
        if let Some(next) = self.chat.update(&action) {
            self.dispatch(next);
        }
    }

    fn focus_to(&mut self, focus: Focus) {
        self.focus = focus;
        self.apply_focus();
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

        tokio::spawn(async move {
            let session_id = match existing {
                Some(id) => id,
                None => match client.create_agent_session(&workspace).await {
                    Ok(resp) => {
                        client.spawn_event_stream(resp.session_id, tx.clone());
                        let _ = tx.send(Action::SessionBound(resp.session_id));
                        resp.session_id
                    }
                    Err(e) => {
                        let _ = tx.send(Action::Error(format!("session: {e}")));
                        return;
                    }
                },
            };
            let req = AgentTurnRequest {
                session_id: Some(session_id),
                prompt,
                cwd: workspace,
                guidance: None,
                room_id: None,
                project_id: None,
                client_type: Some("tui".into()),
                agent: None,
                role: None,
                thinking_level: None,
                model_id: None,
                images: None,
                decision_token: None,
                client_context: None,
            };
            if let Err(e) = client.agent_turn(&req).await {
                let _ = tx.send(Action::Error(format!("turn: {e}")));
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
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(SESS_W),
                Constraint::Length(1),
                Constraint::Min(40),
                Constraint::Length(1),
                Constraint::Length(TREE_W),
            ])
            .split(body);
        let (r_sessions, r_split_a, center, r_split_b, r_tree) =
            (cols[0], cols[1], cols[2], cols[3], cols[4]);

        // center: main surface + terminal docked at the bottom when live.
        let (r_center, r_split_term, r_term) = if self.pty.is_active() {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),
                    Constraint::Length(1),
                    Constraint::Length(TERM_H),
                ])
                .split(center);
            (rows[0], rows[1], rows[2])
        } else {
            (center, Rect::default(), Rect::default())
        };

        // deep chrome first
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG)),
            full,
        );

        // panels — all visible, always.
        self.rail.draw(frame, r_sessions);
        match self.center {
            Center::Chat => self.chat.draw(frame, r_center),
            Center::Editor => self.editor.draw(frame, r_center),
            Center::Graph => self.graph.draw(frame, r_center),
        }
        self.tree.draw(frame, r_tree);
        if self.pty.is_active() {
            self.pty.draw(frame, r_term);
            splitter(frame, r_split_term, false);
        }
        splitter(frame, r_split_a, true);
        splitter(frame, r_split_b, true);

        self.draw_title(frame, title_row);
        self.draw_status(frame, status_row);
    }

    fn draw_title(&self, frame: &mut ratatui::Frame, area: Rect) {
        let spans = vec![
            Span::styled(
                format!(" {} OCEAN ", g("◇", "*")),
                Style::default()
                    .fg(theme::CYAN)
                    .bg(theme::BG_DARK)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                self.workspace_root.clone(),
                Style::default().fg(theme::COMMENT).bg(theme::BG_DARK),
            ),
        ];
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG_DARK)),
            area,
        );
    }

    fn draw_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        let mut spans: Vec<Span> = vec![
            Span::styled(
                format!(" {} ", self.status),
                Style::default().fg(theme::COMMENT),
            ),
            Span::raw("  "),
        ];
        for (key, name, active) in [
            ("⌃⌥1", "sessions", self.focus == Focus::Sessions),
            ("⌃⌥2", "files", self.focus == Focus::Tree),
            (
                "⌃⌥3",
                "chat",
                self.focus == Focus::Center && self.center == Center::Chat,
            ),
            (
                "⌃⌥4",
                "editor",
                self.focus == Focus::Center && self.center == Center::Editor,
            ),
            (
                "⌃⌥5",
                "graph",
                self.focus == Focus::Center && self.center == Center::Graph,
            ),
            ("⌃⌥6", "term", self.focus == Focus::Term),
        ] {
            let (kf, nf) = if active {
                (theme::CYAN, theme::FG)
            } else {
                (theme::COMMENT, theme::COMMENT)
            };
            spans.push(Span::styled(format!("{key} "), Style::default().fg(kf)));
            spans.push(Span::styled(format!("{name}  "), Style::default().fg(nf)));
        }
        spans.push(Span::styled(
            "⇥ cycle · ⌃Q quit",
            Style::default().fg(theme::COMMENT),
        ));
        frame.render_widget(
            Paragraph::new(Line::from(spans)).style(Style::default().bg(theme::BG_DARK)),
            area,
        );
    }
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
