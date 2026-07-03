//! SessionRailComponent — the left rail listing Ocean sessions, wearing CTRL's
//! SESSIONS-panel skin: slate bed with edge/shadow columns, `◆ SESSIONS` title
//! with a sort pill, OC badge pills with a live dot, breathing accent bar on the
//! selected row, expand-on-select resume preview, and a footer tally.
//!
//! Enter resumes the selected session natively into the chat; `t` opens it in
//! the embedded terminal instead; `r` rescans the disk.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ocean_agent_sdk::AgentSessionId;
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::shell::{
    action::Action,
    component::Component,
    panel,
    sessions::{ago, discover, Session, Sort},
    theme::{self, g},
};

pub struct SessionRailComponent {
    root: PathBuf,
    sessions: Vec<Session>,
    selected: usize,
    scroll: usize,
    /// Session id currently open in the chat/PTY — gets the live dot.
    pub live_id: Option<String>,
    pub focused: bool,
}

impl SessionRailComponent {
    pub fn new(root: PathBuf) -> Self {
        let sessions = discover(&root, Sort::Date);
        Self {
            root,
            sessions,
            selected: 0,
            scroll: 0,
            live_id: None,
            focused: true,
        }
    }

    pub fn refresh(&mut self) {
        self.sessions = discover(&self.root, Sort::Date);
        if self.selected >= self.sessions.len() {
            self.selected = self.sessions.len().saturating_sub(1);
        }
    }

    fn resume_selected(&self) -> Option<Action> {
        let s = self.sessions.get(self.selected)?;
        match uuid::Uuid::parse_str(&s.id) {
            Ok(uuid) => Some(Action::ResumeSession {
                id: AgentSessionId(uuid),
                path: s.path.clone(),
            }),
            Err(_) => self.open_in_terminal(),
        }
    }

    fn open_in_terminal(&self) -> Option<Action> {
        let s = self.sessions.get(self.selected)?;
        let (cmd, args) = s.resume_command();
        let mut line = format!("{} {}", cmd, args.join(" "));
        if s.cwd != self.root {
            line = format!("cd {} && {}", s.cwd.display(), line);
        }
        line.push('\n');
        Some(Action::OpenSession {
            line,
            cwd: s.cwd.clone(),
        })
    }

    /// Keep the selection visible: selected row is 2 rows tall (expanded).
    fn clamp_scroll(&mut self, view_h: usize) {
        if view_h == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected + 2 > self.scroll + view_h {
            self.scroll = (self.selected + 2).saturating_sub(view_h);
        }
    }
}

impl Component for SessionRailComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if !self.focused {
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.sessions.len() {
                    self.selected += 1;
                }
                None
            }
            KeyCode::Char('r') => {
                self.refresh();
                None
            }
            KeyCode::Enter => self.resume_selected(),
            KeyCode::Char('t') => self.open_in_terminal(),
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let body = panel::draw(
            frame,
            area,
            "SESSIONS",
            Some(g("◷ date", "[date]")),
            self.focused,
        );
        if body.width == 0 {
            return;
        }

        if self.sessions.is_empty() {
            let msg = "no ocean sessions for this project";
            let y = body.y + body.height / 2;
            let x = body.x + (body.width.saturating_sub(msg.len() as u16)) / 2;
            frame.render_widget(
                Paragraph::new(Span::styled(msg, Style::default().fg(theme::COMMENT)))
                    .style(Style::default().bg(theme::SLATE)),
                Rect::new(x.min(body.x + body.width), y, msg.len() as u16, 1),
            );
            panel::footer(frame, area, " ocean 0");
            return;
        }

        let view_h = body.height as usize;
        self.clamp_scroll(view_h);

        let mut y = body.y;
        let bottom = body.y + body.height;
        for (i, s) in self.sessions.iter().enumerate().skip(self.scroll) {
            if y >= bottom {
                break;
            }
            let selected = i == self.selected;
            let live = self.live_id.as_deref() == Some(s.id.as_str());
            let row_bg = if selected { theme::BG_HL } else { theme::SLATE };

            // accent bar (1 col): cyan breathing bar on the selected row.
            let bar = if selected { g("▎", "|") } else { " " };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    bar,
                    Style::default().fg(theme::CYAN).bg(row_bg),
                )),
                Rect::new(body.x, y, 1, 1),
            );

            // OC badge pill, with the green live dot overlay when hydrated.
            let badge_span = if live {
                Span::styled(
                    format!("{}O", g("●", "*")),
                    Style::default()
                        .fg(theme::GREEN)
                        .bg(theme::BADGE_OCEAN_BG)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(
                    "OC",
                    Style::default()
                        .fg(theme::CYAN)
                        .bg(theme::BADGE_OCEAN_BG)
                        .add_modifier(Modifier::BOLD),
                )
            };

            let ago_s = ago(s.mtime);
            // budget: bar(1) + badge(2) + sp(1) + title + sp(1) + ago
            let used = 1 + 2 + 1 + 1 + ago_s.chars().count();
            let title_max = (body.width as usize).saturating_sub(used);
            let title_txt = truncate(&s.title, title_max);
            let title_mod = if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            };
            let mid_pad = (body.width as usize)
                .saturating_sub(1 + 2 + 1 + title_txt.chars().count() + ago_s.chars().count());
            let line = Line::from(vec![
                badge_span,
                Span::raw(" "),
                Span::styled(
                    title_txt,
                    Style::default().fg(theme::FG).add_modifier(title_mod),
                ),
                Span::raw(" ".repeat(mid_pad)),
                Span::styled(ago_s, Style::default().fg(theme::COMMENT)),
            ]);
            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(row_bg)),
                Rect::new(body.x + 1, y, body.width.saturating_sub(1), 1),
            );
            y += 1;

            // expand-on-select: resume preview line on the highlight bed.
            if selected && y < bottom {
                let id8 = &s.id[..s.id.len().min(8)];
                let preview = format!(" {} resume {}…  ·  ⏎ chat · t term", g("❯", ">"), id8);
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        g("▎", "|"),
                        Style::default().fg(theme::CYAN).bg(theme::BG_HL),
                    )),
                    Rect::new(body.x, y, 1, 1),
                );
                frame.render_widget(
                    Paragraph::new(Span::styled(
                        panel::pad_to(&preview, body.width.saturating_sub(1) as usize),
                        Style::default().fg(theme::CYAN),
                    ))
                    .style(Style::default().bg(theme::BG_HL)),
                    Rect::new(body.x + 1, y, body.width.saturating_sub(1), 1),
                );
                y += 1;
            }
        }

        panel::footer(frame, area, &format!(" ocean {}", self.sessions.len()));
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        format!(
            "{}…",
            s.chars().take(max.saturating_sub(1)).collect::<String>()
        )
    }
}
