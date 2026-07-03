//! App — the thin coordinator. Owns the workbench component tree (left rail:
//! sessions|files; main view: chat|pty|editor|graph), routes events, drains the
//! action channel, and spawns network work off the render loop.
//!
//! Navigation is function-key based so nothing collides with editor typing or
//! PTY input: F1 sessions · F2 files · F3 chat · F4 editor · F5 graph · F6 term.
//! Ctrl-Q quits (Ctrl-C is left free to reach the PTY as SIGINT). Enter on a
//! session opens the PTY; Enter on a file opens the editor.

use std::path::PathBuf;

use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers};
use ocean_agent_sdk::{AgentSessionId, AgentTurnRequest};
use ratatui::layout::{Constraint, Direction, Layout};
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
    tui,
};

#[derive(Clone, Copy, PartialEq)]
enum Left {
    Sessions,
    Files,
}

#[derive(Clone, Copy, PartialEq)]
enum Main {
    Chat,
    Pty,
    Editor,
    Graph,
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Left,
    Main,
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
    left: Left,
    main: Main,
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
            editor: EditorComponent::default(),
            graph: GraphComponent::new(root),
            left: Left::Sessions,
            main: Main::Chat,
            focus: Focus::Left,
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
                    // Panes with off-band state (PTY output, debounced highlight).
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
            // Global navigation / quit — function keys never collide with input.
            if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('q') {
                self.should_quit = true;
                return;
            }
            match k.code {
                KeyCode::F(1) => return self.go(Focus::Left, Some(Left::Sessions), None),
                KeyCode::F(2) => return self.go(Focus::Left, Some(Left::Files), None),
                KeyCode::F(3) => return self.go(Focus::Main, None, Some(Main::Chat)),
                KeyCode::F(4) => return self.go(Focus::Main, None, Some(Main::Editor)),
                KeyCode::F(5) => return self.go(Focus::Main, None, Some(Main::Graph)),
                KeyCode::F(6) => return self.go(Focus::Main, None, Some(Main::Pty)),
                _ => {}
            }
        }
        let action = match (self.focus, self.left, self.main) {
            (Focus::Left, Left::Sessions, _) => self.rail.handle_event(&evt),
            (Focus::Left, Left::Files, _) => self.tree.handle_event(&evt),
            (Focus::Main, _, Main::Chat) => self.chat.handle_event(&evt),
            (Focus::Main, _, Main::Pty) => self.pty.handle_event(&evt),
            (Focus::Main, _, Main::Editor) => self.editor.handle_event(&evt),
            (Focus::Main, _, Main::Graph) => self.graph.handle_event(&evt),
        };
        if let Some(a) = action {
            self.dispatch(a);
        }
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
                self.pty.open(cwd, line);
                self.go(Focus::Main, None, Some(Main::Pty));
            }
            Action::OpenFile(path) => {
                self.editor.open(path.clone());
                self.go(Focus::Main, None, Some(Main::Editor));
            }
            Action::CycleFocus => {
                let next = match self.focus {
                    Focus::Left => Focus::Main,
                    Focus::Main => Focus::Left,
                };
                self.go(next, None, None);
            }
            _ => {}
        }
        // Chat folds in streamed agent events regardless of focus.
        if let Some(next) = self.chat.update(&action) {
            self.dispatch(next);
        }
    }

    /// Switch focus and optionally the active left/main surface, then sync each
    /// component's `focused` flag for border styling + input gating.
    fn go(&mut self, focus: Focus, left: Option<Left>, main: Option<Main>) {
        self.focus = focus;
        if let Some(l) = left {
            self.left = l;
        }
        if let Some(m) = main {
            self.main = m;
        }
        self.apply_focus();
    }

    fn apply_focus(&mut self) {
        let left_focused = self.focus == Focus::Left;
        let main_focused = self.focus == Focus::Main;
        self.rail.focused = left_focused && self.left == Left::Sessions;
        self.tree.focused = left_focused && self.left == Left::Files;
        self.pty.focused = main_focused && self.main == Main::Pty;
        self.editor.focused = main_focused && self.main == Main::Editor;
        self.graph.focused = main_focused && self.main == Main::Graph;
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

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(34), Constraint::Min(20)])
            .split(frame.area());

        match self.left {
            Left::Sessions => self.rail.draw(frame, cols[0]),
            Left::Files => self.tree.draw(frame, cols[0]),
        }

        let main = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(cols[1]);

        match self.main {
            Main::Chat => self.chat.draw(frame, main[0]),
            Main::Pty => self.pty.draw(frame, main[0]),
            Main::Editor => self.editor.draw(frame, main[0]),
            Main::Graph => self.graph.draw(frame, main[0]),
        }

        frame.render_widget(
            ratatui::widgets::Paragraph::new(format!(
                " {}   F1 sessions · F2 files · F3 chat · F4 editor · F5 graph · F6 term · ^Q quit",
                self.status
            ))
            .style(ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray)),
            main[1],
        );
    }
}
