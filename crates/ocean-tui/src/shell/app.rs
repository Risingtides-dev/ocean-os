//! App — the thin coordinator. Owns the component tree (session rail + main
//! surface) and the action channel, routes events, drains actions, and spawns
//! network work off the render loop. No business logic beyond wiring.

use std::path::PathBuf;

use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyModifiers};
use ocean_agent_sdk::{AgentSessionId, AgentTurnRequest};
use ratatui::layout::{Constraint, Direction, Layout};
use tokio::sync::mpsc;

use super::{
    action::Action,
    client::DaemonClient,
    component::Component,
    components::{chat::ChatComponent, pty_pane::PtyComponent, session_rail::SessionRailComponent},
    event::{Event, EventHandler},
    tui,
};

/// Which pane has the keyboard. `Main` routes to the PTY when a session is
/// live, else to the native chat.
#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Rail,
    Main,
}

pub struct App {
    client: DaemonClient,
    workspace_root: String,
    rail: SessionRailComponent,
    chat: ChatComponent,
    pty: PtyComponent,
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
        let mut rail = SessionRailComponent::new(PathBuf::from(&workspace_root));
        rail.focused = true;
        Self {
            client,
            workspace_root,
            rail,
            chat: ChatComponent::default(),
            pty: PtyComponent::default(),
            focus: Focus::Rail,
            session_id: None,
            status: "connecting…".into(),
            should_quit: false,
            actions_tx,
            actions_rx,
        }
    }

    pub async fn run(mut self, terminal: &mut tui::Tui) -> anyhow::Result<()> {
        let mut events = EventHandler::new(30.0, 60.0);

        // Health check so the status line reflects reality on boot.
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
            let Some(event) = events.next().await else {
                break;
            };
            match event {
                Event::Render => {
                    terminal.draw(|f| self.draw(f))?;
                }
                Event::Tick => {
                    if let Some(a) = self.pty.tick() {
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

    /// Global keys first (quit, focus cycle), then route to the focused pane.
    fn on_crossterm(&mut self, evt: CrosstermEvent) {
        if let CrosstermEvent::Key(k) = evt {
            if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                self.should_quit = true;
                return;
            }
            if k.code == KeyCode::Tab {
                self.dispatch(Action::CycleFocus);
                return;
            }
        }
        let action = match self.focus {
            Focus::Rail => self.rail.handle_event(&evt),
            Focus::Main if self.pty.is_active() => self.pty.handle_event(&evt),
            Focus::Main => self.chat.handle_event(&evt),
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
                self.set_focus(Focus::Main);
            }
            Action::CycleFocus => {
                let next = match self.focus {
                    Focus::Rail => Focus::Main,
                    Focus::Main => Focus::Rail,
                };
                self.set_focus(next);
            }
            _ => {}
        }
        // Fan out to components that react to streamed events.
        if let Some(next) = self.chat.update(&action) {
            self.dispatch(next);
        }
    }

    fn set_focus(&mut self, focus: Focus) {
        self.focus = focus;
        self.rail.focused = focus == Focus::Rail;
        self.pty.focused = focus == Focus::Main && self.pty.is_active();
    }

    /// Spawn the network turn off the render loop. On the first turn, mint the
    /// session and subscribe the event stream *before* submitting (OCEAN-305
    /// eager scoping) so no early deltas are missed.
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

        self.rail.draw(frame, cols[0]);

        let main = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(cols[1]);

        if self.pty.is_active() {
            self.pty.draw(frame, main[0]);
        } else {
            self.chat.draw(frame, main[0]);
        }

        frame.render_widget(
            ratatui::widgets::Paragraph::new(format!(
                " {}   [Tab] switch pane · [Ctrl-C] quit",
                self.status
            ))
            .style(ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray)),
            main[1],
        );
    }
}
