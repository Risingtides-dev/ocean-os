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

use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers, MouseEventKind};
use ocean_agent_sdk::{AgentSessionId, AgentTurnRequest};
use ocean_core::RequestId;
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
    /// Panel visibility (CTRL's collapsible rails + terminal dock).
    show_sessions: bool,
    show_tree: bool,
    show_term: bool,
    /// Clickable title-bar buttons (CTRL's upper model): (rect, action).
    buttons: Vec<(Rect, Btn)>,
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
        let mut app = Self {
            client,
            workspace_root,
            rail: SessionRailComponent::new(root.clone()),
            tree: FileTreeComponent::new(root.clone()),
            chat: ChatComponent::default(),
            pty: PtyComponent::default(),
            editor: EditorComponent::new(root.clone()),
            graph: GraphComponent::new(root),
            // Land in the chat, typing-ready — the rail is one click away.
            center: Center::Chat,
            focus: Focus::Center,
            session_id: None,
            status: "connecting…".into(),
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
            show_sessions: true,
            show_tree: true,
            show_term: true,
            buttons: Vec::new(),
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
            // Global /v1/events: permission requests/decisions live here.
            self.client
                .spawn_global_event_stream(self.actions_tx.clone());
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
        // Mouse: a click focuses the pane under the cursor; wheel + clicks are
        // forwarded to whichever pane the cursor is over (CTRL behavior).
        if let CrosstermEvent::Mouse(m) = evt {
            let pos = (m.column, m.row);
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
                    self.pty
                        .open(&PathBuf::from(&self.workspace_root), "");
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
        // OCEAN-185: mint the per-turn permission secret; the turn's first
        // permission request claims it (see Action::OceanEvent above).
        let decision_token = ocean_core::mint_decision_token();
        self.pending_submit_token = Some(decision_token.clone());

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
                decision_token: Some(decision_token),
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
        let (r_crumb, r_center, r_split_term, r_term) = if term_visible {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1),
                    Constraint::Min(5),
                    Constraint::Length(1),
                    Constraint::Length(TERM_H),
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

        // deep chrome first
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG)),
            full,
        );

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
            Rect::new(area.x, area.y, (proj.chars().count() as u16).min(area.width), 1),
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
            (g("⊞", "[S]"), Btn::Sessions, self.show_sessions, theme::BLUE),
            (g("❯", "[C]"), Btn::Chat, self.center == Center::Chat, theme::CYAN),
            (g("✎", "[F]"), Btn::Editor, self.center == Center::Editor, theme::YELLOW),
            (g("⟠", "[G]"), Btn::Graph, self.center == Center::Graph, theme::MAGENTA),
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
        // CTRL-style status: what's happening + where focus is, not a mode menu.
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
        let spans: Vec<Span> = vec![
            Span::styled(
                format!(" {} ", focus_name),
                Style::default()
                    .fg(theme::CYAN)
                    .bg(theme::BG_HL)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  {}", self.status),
                Style::default().fg(theme::COMMENT),
            ),
            Span::styled(
                "   ⇥ move · ⌃Q quit",
                Style::default().fg(theme::COMMENT),
            ),
        ];
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
