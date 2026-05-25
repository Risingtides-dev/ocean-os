use std::{
    collections::HashMap,
    env, fs,
    io::{self, BufRead, BufReader},
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ocean_core::{
    EventEnvelope, HealthResponse, OceanEvent, PromptRequest, RequestControlResponse,
    RequestCreateResponse, RequestId, RequestState, RequestStatus, RequestsResponse,
    SessionSummary,
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
use serde_json::Value;

#[derive(Debug, Parser)]
#[command(name = "ocean-tui", about = "Ocean daemon steering + TIDES-MESH TUI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(
        long,
        env = "OCEAN_DAEMON_URL",
        default_value = "http://127.0.0.1:4780"
    )]
    url: String,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Read-only TIDES-MESH parity view over file-backed state.
    Mesh(MeshCli),
}

#[derive(Debug, Parser)]
struct MeshCli {
    #[arg(long, default_value = ".")]
    root: PathBuf,

    #[arg(long, env = "TIDES_MESH_AGENT")]
    agent: Option<String>,

    #[arg(long, env = "PIMESH_TAB", value_enum, default_value_t = MeshTab::Board)]
    tab: MeshTab,

    #[arg(long, env = "PIMESH_REFRESH_MS", default_value_t = 1000)]
    refresh_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum MeshTab {
    Board,
    Events,
    Inbox,
    Agents,
}

impl MeshTab {
    fn label(self) -> &'static str {
        match self {
            Self::Board => "board",
            Self::Events => "events",
            Self::Inbox => "inbox",
            Self::Agents => "agents",
        }
    }

    fn all() -> [Self; 4] {
        [Self::Board, Self::Events, Self::Inbox, Self::Agents]
    }

    fn next(self) -> Self {
        match self {
            Self::Board => Self::Events,
            Self::Events => Self::Inbox,
            Self::Inbox => Self::Agents,
            Self::Agents => Self::Board,
        }
    }

    fn from_digit(ch: char) -> Option<Self> {
        match ch {
            '1' => Some(Self::Board),
            '2' => Some(Self::Events),
            '3' => Some(Self::Inbox),
            '4' => Some(Self::Agents),
            _ => None,
        }
    }
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

struct MeshApp {
    root: PathBuf,
    agent: String,
    active_tab: MeshTab,
    paused: bool,
    refresh_every: Duration,
    last_refresh: Option<Instant>,
    status: String,
}

impl MeshApp {
    fn new(cli: MeshCli) -> Self {
        let agent = cli.agent.unwrap_or_else(default_mesh_agent);
        Self {
            root: cli.root,
            agent,
            active_tab: cli.tab,
            paused: false,
            refresh_every: Duration::from_millis(cli.refresh_ms.max(100)),
            last_refresh: None,
            status: "starting".to_string(),
        }
    }

    fn checked_text(&self) -> String {
        match self.last_refresh {
            Some(last) => {
                let elapsed = last.elapsed();
                if elapsed < Duration::from_secs(1) {
                    "just now".to_string()
                } else {
                    format!("{}s ago", elapsed.as_secs())
                }
            }
            None => "never".to_string(),
        }
    }
}

#[derive(Debug, Default, Clone)]
struct MeshCounts {
    todo: usize,
    in_progress: usize,
    blocked: usize,
    review: usize,
    done: usize,
    other: usize,
}

#[derive(Debug, Default, Clone)]
struct AgentCounts {
    total: usize,
    active: usize,
    away: usize,
    stale: usize,
}

#[derive(Debug, Clone)]
struct MeshTask {
    id: String,
    title: String,
    status: String,
    assigned_to: Option<String>,
    owner: Option<String>,
    depends_on: Vec<String>,
    external: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct FeedEvent {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    actor: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    preview: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    ts: Option<String>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct InboxMessage {
    #[serde(default)]
    dir: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    ts: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct AgentRecord {
    agent: String,
    #[serde(default)]
    pid: Option<i32>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    preview: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default, rename = "lastEvent")]
    last_event: Option<String>,
    #[serde(default)]
    lifecycle: Option<String>,
}

#[derive(Debug, Clone)]
struct AgentView {
    agent: String,
    pid: Option<i32>,
    updated_at: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    preview: Option<String>,
    cwd: Option<String>,
    last_event: Option<String>,
    lifecycle: Option<String>,
    presence: AgentPresence,
    presence_reasons: Vec<String>,
    pid_exists: bool,
    zombie: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AgentPresence {
    Active,
    Away,
    Stale,
}

#[derive(Debug, Clone)]
struct MeshState {
    tasks: Vec<MeshTask>,
    feed: Vec<FeedEvent>,
    inbox: Vec<InboxMessage>,
    agents: Vec<AgentView>,
    counts: MeshCounts,
    agent_counts: AgentCounts,
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
    match cli.command {
        Some(Command::Mesh(mesh)) => run_mesh(mesh),
        None => run_daemon(App::new(cli.url)),
    }
}

fn run_daemon(mut app: App) -> anyhow::Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .context("build daemon client")?;

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
        terminal.draw(|frame| draw_daemon_ui(frame, &app))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) => break,
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

fn run_mesh(cli: MeshCli) -> anyhow::Result<()> {
    let mut app = MeshApp::new(cli);
    let mut stdout = io::stdout();

    enable_raw_mode().context("enable raw mode")?;
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear().context("clear terminal")?;

    let mut state = load_mesh_state(&app.root, &app.agent).unwrap_or_else(|err| {
        app.status = format!("load error: {err}");
        empty_mesh_state()
    });
    app.last_refresh = Some(Instant::now());
    let mut next_refresh = Instant::now() + app.refresh_every;

    loop {
        terminal.draw(|frame| draw_mesh_ui(frame, &app, &state))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) => break,
                Event::Key(key) if key.code == KeyCode::Tab => {
                    app.active_tab = app.active_tab.next();
                }
                Event::Key(key) if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) => {
                    match load_mesh_state(&app.root, &app.agent) {
                        Ok(next) => {
                            state = next;
                            app.status = "refreshed".to_string();
                            app.last_refresh = Some(Instant::now());
                            next_refresh = Instant::now() + app.refresh_every;
                        }
                        Err(err) => app.status = format!("refresh error: {err}"),
                    }
                }
                Event::Key(key) if matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P')) => {
                    app.paused = !app.paused;
                    app.status = if app.paused {
                        "paused".to_string()
                    } else {
                        "resumed".to_string()
                    };
                }
                Event::Key(key) => {
                    if let KeyCode::Char(ch) = key.code {
                        if let Some(tab) = MeshTab::from_digit(ch) {
                            app.active_tab = tab;
                        }
                    }
                }
                _ => {}
            }
        }

        if !app.paused && Instant::now() >= next_refresh {
            match load_mesh_state(&app.root, &app.agent) {
                Ok(next) => {
                    state = next;
                    app.status = "auto-refresh".to_string();
                    app.last_refresh = Some(Instant::now());
                }
                Err(err) => app.status = format!("auto-refresh error: {err}"),
            }
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
            Ok(res) => app.status = format!("sessions error: {}", res.error.unwrap_or_default()),
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
            Ok(res) => app.status = format!("requests error: {}", res.error.unwrap_or_default()),
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
            Err(err) => app.status = format!("request parse error: {err}"),
        },
        Err(err) => app.status = format!("request create error: {err}"),
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

fn load_mesh_state(root: &Path, agent: &str) -> anyhow::Result<MeshState> {
    let tasks = read_tasks(root)?;
    let feed = read_feed(root)?;
    let inbox = read_inbox(root, agent)?;
    let agents = read_agents(root)?;
    Ok(MeshState {
        counts: count_tasks(&tasks),
        agent_counts: count_agents(&agents),
        tasks,
        feed,
        inbox,
        agents,
    })
}

fn empty_mesh_state() -> MeshState {
    MeshState {
        tasks: Vec::new(),
        feed: Vec::new(),
        inbox: Vec::new(),
        agents: Vec::new(),
        counts: MeshCounts::default(),
        agent_counts: AgentCounts::default(),
    }
}

fn read_tasks(root: &Path) -> anyhow::Result<Vec<MeshTask>> {
    let mut tasks = read_crew_tasks(root)?;
    tasks.extend(read_external_tasks(root)?);
    tasks.sort_by(|a, b| {
        task_sort_key(&a.id)
            .cmp(&task_sort_key(&b.id))
            .then(a.id.cmp(&b.id))
    });
    Ok(tasks)
}

fn read_crew_tasks(root: &Path) -> anyhow::Result<Vec<MeshTask>> {
    let dir = root.join(".pi/messenger/crew/tasks");
    let mut out = Vec::new();
    for entry in read_dir_safe(&dir)? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let json: Value = read_json_file(&path)?;
        let id = json
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                path.file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let md_path = dir.join(format!("{id}.md"));
        let spec = fs::read_to_string(&md_path).unwrap_or_default();
        let title = json
            .get("title")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| title_from_markdown(&spec))
            .unwrap_or_else(|| id.clone());
        out.push(MeshTask {
            id,
            title,
            status: json
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("todo")
                .to_string(),
            assigned_to: json
                .get("assigned_to")
                .or_else(|| json.get("assignedTo"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            owner: json
                .get("owner")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            depends_on: json
                .get("depends_on")
                .or_else(|| json.get("dependsOn"))
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToOwned::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            external: false,
        });
    }
    Ok(out)
}

fn read_external_tasks(root: &Path) -> anyhow::Result<Vec<MeshTask>> {
    if env::var("PIMESH_EXTERNAL_TASKS").as_deref() == Ok("0") {
        return Ok(Vec::new());
    }
    let path = root.join(".pi/workdash/external-tasks.json");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let json: Value = read_json_file(&path)?;
    let Some(tasks) = json.get("tasks").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(tasks
        .iter()
        .filter_map(|task| {
            let id = task.get("id")?.as_str()?.to_string();
            let title = task.get("title")?.as_str()?.to_string();
            Some(MeshTask {
                id,
                title,
                status: task
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("todo")
                    .to_string(),
                assigned_to: task
                    .get("assigned_to")
                    .or_else(|| task.get("assignedTo"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                owner: task
                    .get("owner")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                depends_on: task
                    .get("depends_on")
                    .or_else(|| task.get("dependsOn"))
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
                external: true,
            })
        })
        .collect())
}

fn read_feed(root: &Path) -> anyhow::Result<Vec<FeedEvent>> {
    let path = root.join(".pi/messenger/feed.jsonl");
    let mut items = read_jsonl::<FeedEvent>(&path)?;
    if items.len() > 300 {
        items = items.split_off(items.len() - 300);
    }
    items.reverse();
    Ok(items)
}

fn read_inbox(root: &Path, agent: &str) -> anyhow::Result<Vec<InboxMessage>> {
    let path = root.join(format!(".pi/messenger/mailboxes/by-agent/{agent}.jsonl"));
    let mut items = read_jsonl::<InboxMessage>(&path)?;
    if items.len() > 120 {
        items = items.split_off(items.len() - 120);
    }
    items.reverse();
    Ok(items)
}

fn read_agents(root: &Path) -> anyhow::Result<Vec<AgentView>> {
    let dir = root.join(".pi/messenger/live/agents");
    let mut records = Vec::new();
    for entry in read_dir_safe(&dir)? {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let record: AgentRecord = read_json_file(&path)?;
        records.push(classify_agent(record));
    }
    apply_agent_record_hygiene(&mut records);
    records.sort_by(|a, b| {
        a.presence
            .cmp(&b.presence)
            .then(parse_ts(&b.updated_at).cmp(&parse_ts(&a.updated_at)))
    });
    Ok(records)
}

fn classify_agent(record: AgentRecord) -> AgentView {
    let pid_state = pid_state(record.pid);
    let age_ms = age_ms(record.updated_at.as_deref());
    let mut reasons = Vec::new();
    if record.updated_at.is_none() {
        reasons.push("no heartbeat".to_string());
    }
    if !pid_state.exists {
        reasons.push(if pid_state.invalid {
            "invalid pid".to_string()
        } else {
            "pid missing".to_string()
        });
    } else if pid_state.zombie {
        reasons.push("zombie pid".to_string());
    }
    if age_ms > 15 * 60 * 1000 {
        reasons.push("old heartbeat".to_string());
    }
    let presence = if reasons.is_empty() && age_ms <= 2 * 60 * 1000 {
        AgentPresence::Active
    } else if reasons.is_empty() && age_ms <= 15 * 60 * 1000 {
        AgentPresence::Away
    } else {
        AgentPresence::Stale
    };
    AgentView {
        agent: record.agent,
        pid: record.pid,
        updated_at: record.updated_at,
        model: record.model,
        provider: record.provider,
        preview: record.preview,
        cwd: record.cwd,
        last_event: record.last_event,
        lifecycle: record.lifecycle,
        presence,
        presence_reasons: reasons,
        pid_exists: pid_state.exists,
        zombie: pid_state.zombie,
    }
}

fn apply_agent_record_hygiene(agents: &mut [AgentView]) {
    let mut by_pid: HashMap<i32, Vec<usize>> = HashMap::new();
    for (idx, agent) in agents.iter().enumerate() {
        if let Some(pid) = agent.pid {
            if agent.pid_exists && !agent.zombie {
                by_pid.entry(pid).or_default().push(idx);
            }
        }
    }

    for (pid, group) in by_pid {
        if group.len() < 2 {
            continue;
        }
        let mut keep = group[0];
        for idx in group.iter().copied() {
            let better_name = agents[idx].agent != "agent" && agents[keep].agent == "agent";
            let newer = parse_ts(&agents[idx].updated_at) > parse_ts(&agents[keep].updated_at);
            if better_name || newer {
                keep = idx;
            }
        }
        let keep_name = agents[keep].agent.clone();
        for idx in group {
            if idx == keep {
                continue;
            }
            agents[idx].presence = AgentPresence::Stale;
            agents[idx].presence_reasons.push(format!(
                "duplicate record for pid {pid}; keeping {keep_name}"
            ));
        }
    }
}

fn count_tasks(tasks: &[MeshTask]) -> MeshCounts {
    let mut counts = MeshCounts::default();
    for task in tasks {
        match task.status.as_str() {
            "todo" | "pending" | "ready" => counts.todo += 1,
            "in_progress" | "progress" => counts.in_progress += 1,
            "blocked" => counts.blocked += 1,
            "review" | "milestone" => counts.review += 1,
            "done" => counts.done += 1,
            _ => counts.other += 1,
        }
    }
    counts
}

fn count_agents(agents: &[AgentView]) -> AgentCounts {
    let mut counts = AgentCounts::default();
    for agent in agents {
        counts.total += 1;
        match agent.presence {
            AgentPresence::Active => counts.active += 1,
            AgentPresence::Away => counts.away += 1,
            AgentPresence::Stale => counts.stale += 1,
        }
    }
    counts
}

fn draw_daemon_ui(frame: &mut ratatui::Frame<'_>, app: &App) {
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

fn draw_mesh_ui(frame: &mut ratatui::Frame<'_>, app: &MeshApp, state: &MeshState) {
    let area = frame.area();
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(4),
            Constraint::Min(8),
            Constraint::Length(2),
        ])
        .split(area);

    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled(
                "TIDES-MESH",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" // "),
            Span::styled(
                app.root
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| app.root.display().to_string()),
                Style::default().fg(Color::White),
            ),
            Span::raw(" // "),
            Span::styled(app.agent.as_str(), Style::default().fg(Color::Yellow)),
        ]),
        Line::from(format!(
            "{} done  {} active-task  {} blocked  {} active/{}/{} stale",
            state.counts.done,
            state.counts.in_progress,
            state.counts.blocked + state.counts.review,
            state.agent_counts.active,
            state.agent_counts.away,
            state.agent_counts.stale
        )),
        Line::from(mesh_tabs_line(app.active_tab, app.paused)),
    ])
    .block(
        Block::default()
            .title("Ocean TUI Mesh")
            .borders(Borders::ALL),
    )
    .wrap(Wrap { trim: true });
    frame.render_widget(header, layout[0]);

    match app.active_tab {
        MeshTab::Board => draw_mesh_board(frame, layout[1], state),
        MeshTab::Events => draw_mesh_events(frame, layout[1], state),
        MeshTab::Inbox => draw_mesh_inbox(frame, layout[1], state, &app.agent),
        MeshTab::Agents => draw_mesh_agents(frame, layout[1], state),
    }

    let footer = Paragraph::new(format!(
        "1-4 tabs | Tab cycle | r refresh | p pause | q quit | checked {} | {}",
        app.checked_text(),
        app.status
    ))
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, layout[2]);
}

fn draw_mesh_board(frame: &mut ratatui::Frame<'_>, area: ratatui::layout::Rect, state: &MeshState) {
    let columns: [(&str, &[&str]); 4] = [
        ("TO DO", &["todo", "pending", "ready"]),
        ("ACTIVE", &["in_progress", "progress"]),
        ("BLOCKED / REVIEW", &["blocked", "review", "milestone"]),
        ("DONE", &["done"]),
    ];
    let layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);

    for (idx, (title, statuses)) in columns.iter().enumerate() {
        let items: Vec<String> = state
            .tasks
            .iter()
            .filter(|task| statuses.contains(&task.status.as_str()))
            .map(board_card)
            .collect();
        let lines = if items.is_empty() {
            vec![Line::from("No tasks.")]
        } else {
            items.into_iter().map(Line::from).collect()
        };
        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .title(format!(
                        "{} {}",
                        title,
                        count_bucket(&state.tasks, statuses)
                    ))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(paragraph, layout[idx]);
    }
}

fn draw_mesh_events(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    state: &MeshState,
) {
    let lines: Vec<Line<'static>> = if state.feed.is_empty() {
        vec![Line::from("No events yet.")]
    } else {
        state
            .feed
            .iter()
            .take(area.height.saturating_sub(2) as usize)
            .map(|event| Line::from(render_feed_event(event, area.width as usize)))
            .collect()
    };
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title("TIDES-MESH Events")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn draw_mesh_inbox(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    state: &MeshState,
    agent: &str,
) {
    let lines: Vec<Line<'static>> = if state.inbox.is_empty() {
        vec![Line::from(format!("No mailbox found for {agent}."))]
    } else {
        state
            .inbox
            .iter()
            .take(area.height.saturating_sub(2) as usize)
            .map(|msg| Line::from(render_inbox_message(msg, area.width as usize)))
            .collect()
    };
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title(format!("{} Inbox", agent))
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn draw_mesh_agents(
    frame: &mut ratatui::Frame<'_>,
    area: ratatui::layout::Rect,
    state: &MeshState,
) {
    let mut lines = vec![Line::from(
        "state    agent              pid     age   model/provider        last        preview",
    )];
    for agent in state
        .agents
        .iter()
        .take(area.height.saturating_sub(3) as usize)
    {
        lines.push(Line::from(render_agent_line(agent, area.width as usize)));
    }
    if state.agents.is_empty() {
        lines.push(Line::from("No live agent records."));
    }
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .title("TIDES-MESH Agents")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn mesh_tabs_line(active: MeshTab, paused: bool) -> String {
    let mut parts = Vec::new();
    for (idx, tab) in MeshTab::all().iter().enumerate() {
        let label = format!("{}:{}", idx + 1, tab.label().to_uppercase());
        if *tab == active {
            parts.push(format!("[{label}]"));
        } else {
            parts.push(label);
        }
    }
    let mut line = parts.join(" ");
    if paused {
        line.push_str("  PAUSED");
    } else {
        line.push_str("  Tab cycle · r refresh · p pause · q quit");
    }
    line
}

fn board_card(task: &MeshTask) -> String {
    let owner = task
        .assigned_to
        .as_deref()
        .or(task.owner.as_deref())
        .unwrap_or("unassigned");
    let deps = if task.depends_on.is_empty() {
        String::new()
    } else {
        format!(" ←{}", task.depends_on.join(","))
    };
    let external = if task.external { " ↗" } else { "" };
    format!(
        "{}{} {} · {}{}",
        task.id,
        external,
        compact_text(&task.title, 28),
        owner,
        compact_text(&deps, 18)
    )
}

fn count_bucket(tasks: &[MeshTask], statuses: &[&str]) -> usize {
    tasks
        .iter()
        .filter(|task| statuses.contains(&task.status.as_str()))
        .count()
}

fn render_feed_event(event: &FeedEvent, width: usize) -> String {
    let kind = event
        .r#type
        .as_deref()
        .or(event.event.as_deref())
        .unwrap_or("event");
    let actor = event
        .agent
        .as_deref()
        .or(event.actor.as_deref())
        .or(event.from.as_deref())
        .unwrap_or("mesh");
    let target = event
        .target
        .as_deref()
        .or(event.to.as_deref())
        .unwrap_or("");
    let message = event
        .preview
        .as_deref()
        .or(event.message.as_deref())
        .or(event.text.as_deref())
        .or(event.summary.as_deref())
        .unwrap_or("");
    let stamp = time_ago(
        event
            .ts
            .as_deref()
            .or(event.created_at.as_deref())
            .or(event.timestamp.as_deref()),
    );
    let left = if target.is_empty() {
        format!(
            "{:>4} {} {}",
            stamp,
            compact_text(kind, 18),
            compact_text(actor, 14)
        )
    } else {
        format!(
            "{:>4} {} {} → {}",
            stamp,
            compact_text(kind, 18),
            compact_text(actor, 14),
            compact_text(target, 14)
        )
    };
    format!(
        "{} {}",
        left,
        compact_text(message, width.saturating_sub(left.len() + 1))
    )
}

fn render_inbox_message(message: &InboxMessage, width: usize) -> String {
    let outbound = message.dir.as_deref() == Some("out");
    let dir = if outbound { "OUT" } else { "IN" };
    let peer = if outbound {
        message.to.as_deref().unwrap_or("?")
    } else {
        message.from.as_deref().unwrap_or("?")
    };
    let stamp = time_ago(message.ts.as_deref());
    let left = format!("{:>4} {} {}", stamp, dir, compact_text(peer, 16));
    format!(
        "{} {}",
        left,
        compact_text(
            message.text.as_deref().unwrap_or(""),
            width.saturating_sub(left.len() + 1)
        )
    )
}

fn render_agent_line(agent: &AgentView, width: usize) -> String {
    let state = match agent.presence {
        AgentPresence::Active => "active",
        AgentPresence::Away => "away",
        AgentPresence::Stale => "stale",
    };
    let reason = if agent.presence_reasons.is_empty() {
        String::new()
    } else {
        format!(" · {}", agent.presence_reasons.join(", "))
    };
    let preview = agent
        .preview
        .as_deref()
        .or(agent.cwd.as_deref())
        .unwrap_or("");
    let model = agent
        .model
        .as_deref()
        .or(agent.provider.as_deref())
        .unwrap_or("—");
    let last = agent
        .last_event
        .as_deref()
        .or(agent.lifecycle.as_deref())
        .unwrap_or("—");
    let base = format!(
        "{:<8} {:<18} {:<7} {:<5} {:<20} {:<10}",
        state,
        compact_text(&agent.agent, 18),
        agent
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "—".to_string()),
        time_ago(agent.updated_at.as_deref()),
        compact_text(model, 20),
        compact_text(last, 10)
    );
    format!(
        "{} {}",
        base,
        compact_text(
            &format!("{}{}", preview, reason),
            width.saturating_sub(base.len() + 1)
        )
    )
}

fn state_label(state: RequestState) -> &'static str {
    match state {
        RequestState::Queued => "queued",
        RequestState::Running => "running",
        RequestState::WaitingForPermission => "waiting_permission",
        RequestState::Cancelling => "cancel_requested",
        RequestState::Cancelled => "cancelled",
        RequestState::Completed => "completed",
        RequestState::Errored => "errored",
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
    let char_count = compact.chars().count();
    if char_count <= limit {
        return compact;
    }
    let mut result: String = compact.chars().take(limit.saturating_sub(1)).collect();
    result.push('…');
    result
}

fn default_mesh_agent() -> String {
    env::var("TIDES_MESH_AGENT")
        .or_else(|_| env::var("PI_AGENT_NAME"))
        .unwrap_or_else(|_| "Orchestrator".to_string())
}

fn read_dir_safe(path: &Path) -> anyhow::Result<Vec<fs::DirEntry>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("read dir {}", path.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("collect dir {}", path.display()))?;
    entries.sort_by_key(|entry| entry.path());
    Ok(entries)
}

fn read_json_file<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let value = serde_json::from_str(line)
            .with_context(|| format!("parse {} line {}", path.display(), idx + 1))?;
        out.push(value);
    }
    Ok(out)
}

fn title_from_markdown(markdown: &str) -> Option<String> {
    markdown
        .lines()
        .find_map(|line| line.strip_prefix("# ").map(|s| s.trim().to_string()))
}

fn task_sort_key(id: &str) -> (usize, String) {
    let n = id
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .parse::<usize>()
        .unwrap_or(0);
    (n, id.to_string())
}

fn parse_ts(value: &Option<String>) -> i64 {
    value
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

fn age_ms(value: Option<&str>) -> i64 {
    value
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| {
            Utc::now()
                .signed_duration_since(dt.with_timezone(&Utc))
                .num_milliseconds()
        })
        .unwrap_or(i64::MAX)
}

fn time_ago(value: Option<&str>) -> String {
    let ms = age_ms(value);
    if ms == i64::MAX {
        return "—".to_string();
    }
    let secs = (ms / 1000).max(0);
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 172800 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

struct PidState {
    exists: bool,
    zombie: bool,
    invalid: bool,
}

fn pid_state(pid: Option<i32>) -> PidState {
    let Some(pid) = pid else {
        return PidState {
            exists: false,
            zombie: false,
            invalid: true,
        };
    };
    if pid <= 0 {
        return PidState {
            exists: false,
            zombie: false,
            invalid: true,
        };
    }
    let stat_path = PathBuf::from(format!("/proc/{pid}/stat"));
    let Ok(stat) = fs::read_to_string(&stat_path) else {
        return PidState {
            exists: false,
            zombie: false,
            invalid: false,
        };
    };
    let zombie = stat.split(") ").nth(1).and_then(|rest| rest.chars().next()) == Some('Z');
    PidState {
        exists: true,
        zombie,
        invalid: false,
    }
}
