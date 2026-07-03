//! App — the thin coordinator. Owns the component tree and the action channel,
//! routes events to components, drains actions, and spawns network work off the
//! render loop. No business logic lives here beyond wiring.

use ocean_agent_sdk::{AgentSessionId, AgentTurnRequest};
use ratatui::layout::{Constraint, Direction, Layout};
use tokio::sync::mpsc;

use super::{
    action::Action,
    client::DaemonClient,
    component::Component,
    components::chat::ChatComponent,
    event::{Event, EventHandler},
    tui,
};

pub struct App {
    client: DaemonClient,
    workspace_root: String,
    chat: ChatComponent,
    session_id: Option<AgentSessionId>,
    status: String,
    should_quit: bool,
    actions_tx: mpsc::UnboundedSender<Action>,
    actions_rx: mpsc::UnboundedReceiver<Action>,
}

impl App {
    pub fn new(client: DaemonClient, workspace_root: String) -> Self {
        let (actions_tx, actions_rx) = mpsc::unbounded_channel();
        Self {
            client,
            workspace_root,
            chat: ChatComponent::default(),
            session_id: None,
            status: "connecting…".into(),
            should_quit: false,
            actions_tx,
            actions_rx,
        }
    }

    pub async fn run(mut self, terminal: &mut tui::Tui) -> anyhow::Result<()> {
        let mut events = EventHandler::new(8.0, 60.0);

        // Kick a health check so the status line reflects reality on boot.
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
                Event::Tick => {}
                Event::Crossterm(evt) => {
                    if let Some(action) = self.chat.handle_event(&evt) {
                        self.dispatch(action);
                    }
                }
            }
            // Drain everything the components / tasks queued this cycle.
            while let Ok(action) = self.actions_rx.try_recv() {
                self.dispatch(action);
            }
        }
        Ok(())
    }

    /// Apply one action: app-level effects first, then fan out to components.
    fn dispatch(&mut self, action: Action) {
        match &action {
            Action::Quit => {
                self.should_quit = true;
                return;
            }
            Action::Status(s) | Action::Error(s) => {
                self.status = s.clone();
            }
            Action::SessionBound(id) => {
                self.session_id = Some(*id);
            }
            Action::SubmitPrompt(text) => {
                self.submit_turn(text.clone());
            }
            _ => {}
        }
        if let Some(next) = self.chat.update(&action) {
            self.dispatch(next);
        }
    }

    /// Spawn the network turn off the render loop. On the first turn, mint the
    /// session and subscribe the event stream *before* submitting, so no early
    /// deltas are missed (OCEAN-305 eager scoping, async port).
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
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(1)])
            .split(frame.area());
        self.chat.draw(frame, chunks[0]);
        frame.render_widget(
            ratatui::widgets::Paragraph::new(format!(" {}", self.status))
                .style(ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray)),
            chunks[1],
        );
    }
}
