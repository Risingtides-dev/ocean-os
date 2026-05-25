use std::{
    io::{self, BufRead, BufReader},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ocean_core::{
    EventEnvelope, HealthResponse, OceanEvent, PromptRequest, RequestControlResponse,
    RequestCreateResponse, RequestId, RequestStatus, RequestsResponse, SessionSummary,
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

#[derive(Debug, Parser)]
#[command(name = "ocean-tui", about = "Ocean daemon steering TUI")]
struct Cli {
    #[arg(
        long,
        env = "OCEAN_DAEMON_URL",
        default_value = "http://127.0.0.1:4780"
    )]
    url: String,
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
enum StreamMessage {
    Status(String),
    Event(EventEnvelope),
}

struct App {
    url: String,
    health: HealthState,
    sessions: Vec<SessionSummary>,
    requests: Vec<RequestStatus>,
    active_request_id: Option<RequestId>,
    input: String,
    activity: Vec<String>,
    transcript: Vec<String>,
    status: String,
    stream_status: String,
    last_checked: Option<Instant>,
    refresh_every: Duration,
}

impl App {
    fn new(url: String) -> Self {
        Self {
            url,
            health: HealthState::Loading,
            sessions: Vec::new(),
            requests: Vec::new(),
            active_request_id: None,
            input: String::new(),
            activity: Vec::new(),
            transcript: vec![
                "Ocean TUI thin client".to_string(),
                "Type a prompt and press Enter. Press s to refresh sessions.".to_string(),
                "Press Ctrl-C to cancel the latest active request.".to_string(),
            ],
            status: "starting".to_string(),
            stream_status: "connecting".to_string(),
            last_checked: None,
            refresh_every: Duration::from_secs(5),
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
        match self.last_checked {
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

    fn push_activity(&mut self, line: String) {
        self.activity.push(line);
        const ACTIVITY_CAP: usize = 36;
        if self.activity.len() > ACTIVITY_CAP {
            let drain = self.activity.len() - ACTIVITY_CAP;
            self.activity.drain(0..drain);
        }
    }

    fn activity_lines(&self) -> Vec<Line<'static>> {
        if self.activity.is_empty() {
            return vec![Line::from("Waiting for daemon events...")];
        }
        self.activity
            .iter()
            .rev()
            .take(24)
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
        if self.sessions.is_empty() {
            return vec![Line::from("No sessions yet. Press s to refresh.")];
        }
        self.sessions
            .iter()
            .take(12)
            .map(|session| {
                Line::from(format!(
                    "{}  {} turns  {}",
                    session.id, session.turns, session.title
                ))
            })
            .collect()
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
                let message = request.message.as_deref().unwrap_or("");
                Line::from(format!(
                    "{marker} {}  {}  {}",
                    short_id(request.request_id),
                    state_label(request.state),
                    compact_text(message, 48)
                ))
            })
            .collect()
    }

    fn cancellable_request_id(&self) -> Option<RequestId> {
        self.requests
            .iter()
            .find(|request| request.state.is_cancellable())
            .map(|request| request.request_id)
    }
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
    run(cli)
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .context("build daemon client")?;

    let mut app = App::new(cli.url);
    let (stream_tx, stream_rx) = mpsc::channel();
    spawn_event_stream(app.url.clone(), stream_tx);
    let mut stdout = io::stdout();

    enable_raw_mode().context("enable raw mode")?;
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear().context("clear terminal")?;

    refresh_health(&client, &mut app);
    refresh_sessions(&client, &mut app);
    refresh_requests(&client, &mut app);
    let mut next_refresh = Instant::now() + app.refresh_every;

    loop {
        pump_stream(&stream_rx, &mut app);
        terminal.draw(|frame| draw_ui(frame, &app))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) => {
                    break;
                }
                Event::Key(key) if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) => {
                    refresh_health(&client, &mut app);
                    refresh_sessions(&client, &mut app);
                    refresh_requests(&client, &mut app);
                    next_refresh = Instant::now() + app.refresh_every;
                }
                Event::Key(key) if matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S')) => {
                    refresh_sessions(&client, &mut app);
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    cancel_active_request(&client, &mut app);
                    refresh_requests(&client, &mut app);
                }
                Event::Key(key) if key.code == KeyCode::Enter => {
                    send_prompt(&client, &mut app);
                    refresh_sessions(&client, &mut app);
                    refresh_requests(&client, &mut app);
                }
                Event::Key(key) if key.code == KeyCode::Backspace => {
                    app.input.pop();
                }
                Event::Key(key)
                    if key.code == KeyCode::Char('u')
                        && key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    app.input.clear();
                }
                Event::Key(key) => {
                    if let KeyCode::Char(ch) = key.code {
                        if !key.modifiers.contains(KeyModifiers::CONTROL) {
                            app.input.push(ch);
                        }
                    }
                }
                _ => {}
            }
        }

        if Instant::now() >= next_refresh {
            refresh_health(&client, &mut app);
            refresh_requests(&client, &mut app);
            next_refresh = Instant::now() + app.refresh_every;
        }
    }

    Ok(())
}

fn refresh_health(client: &reqwest::blocking::Client, app: &mut App) {
    let url = format!("{}/health", app.url.trim_end_matches('/'));
    match client.get(url).send() {
        Ok(response) => match response.error_for_status() {
            Ok(response) => match response.json::<HealthResponse>() {
                Ok(health) => {
                    app.health = HealthState::Ready(health);
                    app.status = "health refreshed".to_string();
                }
                Err(err) => app.health = HealthState::Error(err.to_string()),
            },
            Err(err) => app.health = HealthState::Error(err.to_string()),
        },
        Err(err) => app.health = HealthState::Error(err.to_string()),
    }

    app.last_checked = Some(Instant::now());
}

fn refresh_sessions(client: &reqwest::blocking::Client, app: &mut App) {
    let url = format!("{}/v1/sessions", app.url.trim_end_matches('/'));
    match client
        .get(url)
        .send()
        .and_then(|res| res.error_for_status())
    {
        Ok(response) => match response.json::<SessionsResponse>() {
            Ok(res) if res.ok => {
                app.sessions = res.sessions;
                app.status = format!("sessions refreshed: {}", app.sessions.len());
            }
            Ok(res) => {
                app.status = format!("sessions error: {}", res.error.unwrap_or_default());
            }
            Err(err) => app.status = format!("sessions parse error: {err}"),
        },
        Err(err) => app.status = format!("sessions request error: {err}"),
    }
}

fn refresh_requests(client: &reqwest::blocking::Client, app: &mut App) {
    let url = format!("{}/v1/requests", app.url.trim_end_matches('/'));
    match client
        .get(url)
        .send()
        .and_then(|res| res.error_for_status())
    {
        Ok(response) => match response.json::<RequestsResponse>() {
            Ok(res) if res.ok => {
                app.requests = res.requests;
                app.status = format!("requests refreshed: {}", app.requests.len());
            }
            Ok(res) => {
                app.status = format!("requests error: {}", res.error.unwrap_or_default());
            }
            Err(err) => app.status = format!("requests parse error: {err}"),
        },
        Err(err) => app.status = format!("requests request error: {err}"),
    }
}

fn cancel_active_request(client: &reqwest::blocking::Client, app: &mut App) {
    let Some(request_id) = app.cancellable_request_id() else {
        app.status = "no cancellable request".to_string();
        return;
    };

    let url = format!(
        "{}/v1/requests/{request_id}/cancel",
        app.url.trim_end_matches('/')
    );
    match client
        .post(url)
        .send()
        .and_then(|res| res.error_for_status())
    {
        Ok(response) => match response.json::<RequestControlResponse>() {
            Ok(res) => {
                app.status = format!(
                    "cancel requested ok={} request={} {}: {}",
                    res.ok,
                    short_id(res.request_id),
                    state_label(res.state),
                    compact_text(&res.message, 56)
                );
            }
            Err(err) => app.status = format!("cancel parse error: {err}"),
        },
        Err(err) => app.status = format!("cancel request error: {err}"),
    }
}

fn send_prompt(client: &reqwest::blocking::Client, app: &mut App) {
    let prompt = app.input.trim().to_string();
    if prompt.is_empty() {
        return;
    }
    app.input.clear();
    app.transcript.push(format!("> {prompt}"));
    app.status = "creating async request...".to_string();

    let url = format!("{}/v1/requests", app.url.trim_end_matches('/'));
    let request = PromptRequest {
        prompt,
        request_id: None,
        session_id: None,
        max_turns: None,
        yolo: false,
    };

    match client
        .post(url)
        .json(&request)
        .send()
        .and_then(|res| res.error_for_status())
    {
        Ok(response) => match response.json::<RequestCreateResponse>() {
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
            Err(err) => {
                app.status = format!("request parse error: {err}");
            }
        },
        Err(err) => {
            app.status = format!("request create error: {err}");
        }
    }
}

fn spawn_event_stream(url: String, tx: mpsc::Sender<StreamMessage>) {
    thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder().build() {
            Ok(client) => client,
            Err(err) => {
                let _ = tx.send(StreamMessage::Status(format!(
                    "stream client error: {}",
                    compact_text(&err.to_string(), 72)
                )));
                return;
            }
        };

        let events_url = format!("{}/v1/events", url.trim_end_matches('/'));

        loop {
            if tx
                .send(StreamMessage::Status("connecting".to_string()))
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
                        .send(StreamMessage::Status(format!(
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
                .send(StreamMessage::Status("connected".to_string()))
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
                                        if tx.send(StreamMessage::Event(envelope)).is_err() {
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
                                            .send(StreamMessage::Status(format!(
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
                            .send(StreamMessage::Status(format!(
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
                .send(StreamMessage::Status("reconnecting".to_string()))
                .is_err()
            {
                break;
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

fn pump_stream(rx: &mpsc::Receiver<StreamMessage>, app: &mut App) {
    while let Ok(message) = rx.try_recv() {
        match message {
            StreamMessage::Status(status) => app.stream_status = status,
            StreamMessage::Event(envelope) => app.push_activity(summarize_event(&envelope)),
        }
    }
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

fn state_label(state: ocean_core::RequestState) -> &'static str {
    match state {
        ocean_core::RequestState::Queued => "queued",
        ocean_core::RequestState::Running => "running",
        ocean_core::RequestState::WaitingForPermission => "waiting_permission",
        ocean_core::RequestState::Cancelling => "cancel_requested",
        ocean_core::RequestState::Cancelled => "cancelled",
        ocean_core::RequestState::Completed => "completed",
        ocean_core::RequestState::Errored => "errored",
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

    let mut result: String = compact.chars().take(limit).collect();
    if compact.chars().count() > limit {
        result.push('…');
    }
    result
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Length(6),
            Constraint::Min(7),
            Constraint::Length(8),
            Constraint::Length(4),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let (health_label, health_color) = app.status_label();
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "Ocean TUI",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(health_label, Style::default().fg(health_color)),
        ]),
        Line::from(format!("daemon URL: {}", app.url)),
        Line::from(app.health_summary()),
    ])
    .block(Block::default().title("Ocean").borders(Borders::ALL))
    .wrap(Wrap { trim: true });
    frame.render_widget(header, chunks[0]);

    let activity = Paragraph::new(app.activity_lines())
        .block(Block::default().title("Activity").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(activity, chunks[1]);

    let requests = Paragraph::new(app.request_lines())
        .block(Block::default().title("Requests").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(requests, chunks[2]);

    let transcript = Paragraph::new(app.transcript_lines())
        .block(Block::default().title("Transcript").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(transcript, chunks[3]);

    let sessions = Paragraph::new(app.session_lines())
        .block(Block::default().title("Sessions").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(sessions, chunks[4]);

    let composer = Paragraph::new(format!("> {}", app.input))
        .block(Block::default().title("Composer").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(composer, chunks[5]);

    let footer = Paragraph::new(format!(
        "Enter send | Ctrl-C cancel | s sessions | r refresh | q quit | stream {} | checked {} | {}",
        app.stream_status,
        app.checked_text(),
        app.status
    ))
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[6]);
}
