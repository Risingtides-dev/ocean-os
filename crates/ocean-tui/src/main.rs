use std::{io, time::Duration, time::Instant};

use anyhow::Context;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ocean_core::HealthResponse;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};

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

struct App {
    url: String,
    health: HealthState,
    last_checked: Option<Instant>,
    refresh_every: Duration,
}

impl App {
    fn new(url: String) -> Self {
        Self {
            url,
            health: HealthState::Loading,
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

    fn line(label: &str, value: impl Into<String>, color: Color) -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("{label:<14}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(value.into(), Style::default().fg(color)),
        ])
    }

    fn health_lines(&self) -> Vec<Line<'static>> {
        let (status, color) = self.status_label();
        let mut lines = vec![Self::line("health", status, color)];

        match &self.health {
            HealthState::Loading => {
                lines.push(Self::line("service", "loading...", Color::DarkGray));
                lines.push(Self::line("version", "loading...", Color::DarkGray));
                lines.push(Self::line("backend/model", "loading...", Color::DarkGray));
            }
            HealthState::Ready(res) => {
                lines.push(Self::line("service", res.service.clone(), Color::White));
                lines.push(Self::line("version", res.version.clone(), Color::White));
                lines.push(Self::line(
                    "backend/model",
                    res.backend.clone(),
                    Color::White,
                ));
            }
            HealthState::Error(err) => {
                lines.push(Self::line("service", "unavailable", Color::DarkGray));
                lines.push(Self::line("version", "unavailable", Color::DarkGray));
                lines.push(Self::line("backend/model", "unavailable", Color::DarkGray));
                lines.push(Self::line("error", err.clone(), Color::Red));
            }
        }

        lines.push(Self::line(
            "last checked",
            self.checked_text(),
            Color::DarkGray,
        ));
        lines
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
        .timeout(Duration::from_secs(3))
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
                    next_refresh = Instant::now() + app.refresh_every;
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
                Ok(health) => app.health = HealthState::Ready(health),
                Err(err) => app.health = HealthState::Error(err.to_string()),
            },
            Err(err) => app.health = HealthState::Error(err.to_string()),
        },
        Err(err) => app.health = HealthState::Error(err.to_string()),
    }

    app.last_checked = Some(Instant::now());
}

fn draw_ui(frame: &mut ratatui::Frame<'_>, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(8),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let header = Paragraph::new(vec![
        Line::from(Span::styled(
            "Ocean TUI",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(format!("daemon URL: {}", app.url)),
        Line::from("thin client only: no agent, provider, tool, or session authority"),
    ])
    .block(Block::default().title("Ocean").borders(Borders::ALL))
    .wrap(Wrap { trim: true });
    frame.render_widget(header, chunks[0]);

    let health = Paragraph::new(app.health_lines())
        .block(
            Block::default()
                .title("Daemon health")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true });
    frame.render_widget(health, chunks[1]);

    let scope = Paragraph::new(vec![
        Line::from("Current screen: contact ocean-daemon and show health/status."),
        Line::from("Next screens will add prompt, sessions, events, and approvals."),
    ])
    .block(Block::default().title("Scope").borders(Borders::ALL))
    .wrap(Wrap { trim: true });
    frame.render_widget(scope, chunks[2]);

    let footer = Paragraph::new("q / Esc quit   r refresh   Ctrl-C quit")
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray));
    frame.render_widget(footer, chunks[3]);
}
