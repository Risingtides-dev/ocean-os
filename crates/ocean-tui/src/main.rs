use std::{io, time::Duration, time::Instant};

use anyhow::Context;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ocean_core::{HealthResponse, PromptRequest, PromptResponse, SessionSummary};
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

struct App {
    url: String,
    health: HealthState,
    sessions: Vec<SessionSummary>,
    input: String,
    transcript: Vec<String>,
    status: String,
    last_checked: Option<Instant>,
    refresh_every: Duration,
}

impl App {
    fn new(url: String) -> Self {
        Self {
            url,
            health: HealthState::Loading,
            sessions: Vec::new(),
            input: String::new(),
            transcript: vec![
                "Ocean TUI thin client".to_string(),
                "Type a prompt and press Enter. Press s to refresh sessions.".to_string(),
            ],
            status: "starting".to_string(),
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
    let mut stdout = io::stdout();

    enable_raw_mode().context("enable raw mode")?;
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;
    terminal.clear().context("clear terminal")?;

    refresh_health(&client, &mut app);
    refresh_sessions(&client, &mut app);
    let mut next_refresh = Instant::now() + app.refresh_every;

    loop {
        terminal.draw(|frame| draw_ui(frame, &app))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key)
                    if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)) =>
                {
                    break;
                }
                Event::Key(key) if matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) => {
                    refresh_health(&client, &mut app);
                    refresh_sessions(&client, &mut app);
                    next_refresh = Instant::now() + app.refresh_every;
                }
                Event::Key(key) if matches!(key.code, KeyCode::Char('s') | KeyCode::Char('S')) => {
                    refresh_sessions(&client, &mut app);
                }
                Event::Key(key) if key.code == KeyCode::Enter => {
                    send_prompt(&client, &mut app);
                    refresh_sessions(&client, &mut app);
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

fn send_prompt(client: &reqwest::blocking::Client, app: &mut App) {
    let prompt = app.input.trim().to_string();
    if prompt.is_empty() {
        return;
    }
    app.input.clear();
    app.transcript.push(format!("> {prompt}"));
    app.status = "sending prompt...".to_string();

    let url = format!("{}/v1/prompt", app.url.trim_end_matches('/'));
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
        Ok(response) => match response.json::<PromptResponse>() {
            Ok(res) => {
                if !res.stdout.trim().is_empty() {
                    app.transcript.push(res.stdout.trim_end().to_string());
                }
                if !res.stderr.trim().is_empty() {
                    app.transcript
                        .push(format!("stderr: {}", res.stderr.trim_end()));
                }
                app.status = format!(
                    "prompt ok={} request={} session={} wall={}ms",
                    res.ok,
                    res.request_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    res.session_id
                        .map(|id| id.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    res.wall_ms
                );
            }
            Err(err) => {
                app.status = format!("prompt parse error: {err}");
            }
        },
        Err(err) => {
            app.status = format!("prompt request error: {err}");
        }
    }
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(5),
            Constraint::Min(8),
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

    let transcript = Paragraph::new(app.transcript_lines())
        .block(Block::default().title("Transcript").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(transcript, chunks[1]);

    let sessions = Paragraph::new(app.session_lines())
        .block(Block::default().title("Sessions").borders(Borders::ALL))
        .wrap(Wrap { trim: true });
    frame.render_widget(sessions, chunks[2]);

    let composer = Paragraph::new(format!("> {}", app.input))
        .block(Block::default().title("Composer").borders(Borders::ALL))
        .wrap(Wrap { trim: false });
    frame.render_widget(composer, chunks[3]);

    let footer = Paragraph::new(format!(
        "Enter send | s sessions | r refresh | q quit | checked {} | {}",
        app.checked_text(),
        app.status
    ))
    .alignment(Alignment::Center)
    .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[4]);
}
