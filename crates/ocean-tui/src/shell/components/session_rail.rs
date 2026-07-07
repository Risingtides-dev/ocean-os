//! SessionRailComponent — the left rail listing Ocean sessions for the current
//! project, grouped into collapsible WORKTREE nodes so a busy repo reads as a
//! tree (like the file explorer) instead of a flat sprawl. Wears CTRL's
//! SESSIONS-panel skin: slate bed, `◆ SESSIONS` title, worktree headers in blue
//! with ▸/▾ carets + a session count, sessions indented one level with their
//! title + relative age, a cyan accent bar on the selected row, and a live `●`
//! dot on the session currently open in the chat/PTY.
//!
//! Enter on a header toggles the group; Enter on a session resumes it natively
//! into the chat; `t` opens the session in the embedded terminal; `r` rescans.
//!
//! (The old per-row `OC` badge was a CTRL remnant from when the rail organized
//! by harness — Ocean only serves itself now, so it's gone.)

use std::collections::HashMap;
use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
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

/// One worktree group: a header ("main", "feat/x", …) over its sessions,
/// newest-first, with a collapse flag.
struct Group {
    worktree: String,
    sessions: Vec<Session>,
    expanded: bool,
}

/// A reference to a currently-visible row: either a worktree header or a
/// session nested under one. Rebuilt each draw from `groups` + expand state,
/// exactly like the file tree's flattened `entries`.
#[derive(Clone, Copy)]
enum Row {
    Header(usize),
    Session(usize, usize),
}

pub struct SessionRailComponent {
    root: PathBuf,
    groups: Vec<Group>,
    selected: usize,
    scroll: usize,
    /// Session id currently open in the chat/PTY — gets the live dot.
    pub live_id: Option<String>,
    pub focused: bool,
    /// Body rect from the last draw, for mouse hit-testing.
    body_rect: Rect,
}

impl SessionRailComponent {
    pub fn new(root: PathBuf) -> Self {
        let groups = build_groups(discover(&root, Sort::Date));
        Self {
            root,
            groups,
            selected: 0,
            scroll: 0,
            live_id: None,
            focused: true,
            body_rect: Rect::default(),
        }
    }

    /// Flatten `groups` into the ordered list of visible rows: every header,
    /// plus the sessions of any expanded group. Selection/scroll index into
    /// this. Cheap enough to rebuild per draw (matches the file tree).
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (gi, group) in self.groups.iter().enumerate() {
            rows.push(Row::Header(gi));
            if group.expanded {
                for si in 0..group.sessions.len() {
                    rows.push(Row::Session(gi, si));
                }
            }
        }
        rows
    }

    fn session_at(&self, row: Row) -> Option<&Session> {
        match row {
            Row::Session(gi, si) => self.groups.get(gi).and_then(|g| g.sessions.get(si)),
            Row::Header(_) => None,
        }
    }

    /// Which visible row a screen position lands on (rows are 1 line each now).
    fn row_at(&self, pos: (u16, u16)) -> Option<usize> {
        let body = self.body_rect;
        if body.width == 0 || pos.1 < body.y || pos.1 >= body.y + body.height {
            return None;
        }
        let i = self.scroll + (pos.1 - body.y) as usize;
        (i < self.rows().len()).then_some(i)
    }

    pub fn refresh(&mut self) {
        // Preserve which worktrees are expanded across a rescan.
        let expanded: Vec<String> = self
            .groups
            .iter()
            .filter(|g| g.expanded)
            .map(|g| g.worktree.clone())
            .collect();
        self.groups = build_groups(discover(&self.root, Sort::Date));
        if !expanded.is_empty() {
            for group in &mut self.groups {
                group.expanded = expanded.contains(&group.worktree);
            }
        }
        let n = self.rows().len();
        if self.selected >= n {
            self.selected = n.saturating_sub(1);
        }
    }

    fn selected_row(&self) -> Option<Row> {
        self.rows().get(self.selected).copied()
    }

    /// Enter: toggle a header, or resume a session.
    fn activate(&mut self) -> Option<Action> {
        match self.selected_row()? {
            Row::Header(gi) => {
                if let Some(group) = self.groups.get_mut(gi) {
                    group.expanded = !group.expanded;
                }
                let n = self.rows().len();
                if self.selected >= n {
                    self.selected = n.saturating_sub(1);
                }
                None
            }
            row @ Row::Session(..) => self.resume(row),
        }
    }

    fn resume(&self, row: Row) -> Option<Action> {
        let s = self.session_at(row)?;
        match uuid::Uuid::parse_str(&s.id) {
            Ok(uuid) => Some(Action::ResumeSession {
                id: AgentSessionId(uuid),
                path: s.path.clone(),
            }),
            Err(_) => self.open_in_terminal(row),
        }
    }

    fn open_in_terminal(&self, row: Row) -> Option<Action> {
        let s = self.session_at(row)?;
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

    fn total_sessions(&self) -> usize {
        self.groups.iter().map(|g| g.sessions.len()).sum()
    }

    fn move_sel(&mut self, delta: isize) {
        let n = self.rows().len();
        if n == 0 {
            return;
        }
        let cur = self.selected as isize;
        self.selected = (cur + delta).clamp(0, n as isize - 1) as usize;
    }

    fn clamp_scroll(&mut self, view_h: usize) {
        if view_h == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + view_h {
            self.scroll = self.selected + 1 - view_h;
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
                self.move_sel(-1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_sel(1);
                None
            }
            KeyCode::Char('r') => {
                self.refresh();
                None
            }
            KeyCode::Enter => self.activate(),
            KeyCode::Char('t') => self.selected_row().and_then(|r| self.open_in_terminal(r)),
            _ => None,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.move_sel(-1);
                None
            }
            MouseEventKind::ScrollDown => {
                self.move_sel(1);
                None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let i = self.row_at((mouse.column, mouse.row))?;
                if i == self.selected {
                    // click the already-selected row: toggle header / open session
                    self.activate()
                } else {
                    self.selected = i;
                    None
                }
            }
            _ => None,
        }
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let body = panel::draw(frame, area, "SESSIONS", None, self.focused);
        if body.width == 0 {
            return;
        }
        self.body_rect = body;

        if self.groups.is_empty() {
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
        let rows = self.rows();
        let inner = body.width.saturating_sub(1) as usize; // width after the accent bar
        let bottom = body.y + body.height;

        for (i, row) in rows.iter().enumerate().skip(self.scroll).take(view_h) {
            let y = body.y + (i - self.scroll) as u16;
            if y >= bottom {
                break;
            }
            let selected = i == self.selected;
            let row_bg = if selected { theme::BG_HL } else { theme::SLATE };

            // accent bar (1 col): cyan bar on the selected row.
            let bar = if selected { g("▎", "|") } else { " " };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    bar,
                    Style::default().fg(theme::CYAN).bg(row_bg),
                )),
                Rect::new(body.x, y, 1, 1),
            );

            let line = match *row {
                Row::Header(gi) => {
                    let group = &self.groups[gi];
                    let caret = if group.expanded {
                        g("▾ ", "v ")
                    } else {
                        g("▸ ", "> ")
                    };
                    let count = format!("({})", group.sessions.len());
                    let label = format!("{caret}{}", display_worktree(&group.worktree));
                    justified(
                        &label,
                        &count,
                        inner,
                        Style::default()
                            .fg(theme::BLUE)
                            .add_modifier(if selected {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                        Style::default().fg(theme::COMMENT),
                    )
                }
                Row::Session(gi, si) => {
                    let s = &self.groups[gi].sessions[si];
                    let live = self.live_id.as_deref() == Some(s.id.as_str());
                    let ago_s = ago(s.mtime);
                    // one level of indent + a live-dot slot, matching the tree's
                    // "  " depth step.
                    let dot = if live { g("● ", "* ") } else { "  " };
                    let dot_style = if live {
                        Style::default().fg(theme::GREEN)
                    } else {
                        Style::default().fg(theme::COMMENT)
                    };
                    // budget: indent(2) + dot(2) + title + sp + ago
                    let left_cols = 2 + 2;
                    let title_max = inner.saturating_sub(left_cols + 1 + ago_s.chars().count());
                    let title = truncate(&s.title, title_max);
                    let title_style = Style::default().fg(theme::FG).add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    });
                    // Build: "  " indent, dot, title … right-justified ago.
                    let used = left_cols + title.chars().count();
                    let pad = inner.saturating_sub(used + ago_s.chars().count());
                    Line::from(vec![
                        Span::styled("  ", Style::default()),
                        Span::styled(dot, dot_style),
                        Span::styled(title, title_style),
                        Span::raw(" ".repeat(pad)),
                        Span::styled(ago_s, Style::default().fg(theme::COMMENT)),
                    ])
                }
            };

            frame.render_widget(
                Paragraph::new(line).style(Style::default().bg(row_bg)),
                Rect::new(body.x + 1, y, body.width.saturating_sub(1), 1),
            );
        }

        panel::footer(frame, area, &format!(" ocean {}", self.total_sessions()));
    }
}

/// Group `sessions` (newest-first from `discover`) into collapsible worktree
/// nodes. Group order: the worktree with the most recent activity floats to the
/// top; within a group, sessions stay newest-first. The most-recently-active
/// group starts expanded, the rest collapsed — so `ocean` opens on your latest
/// work without the whole history sprawling open.
fn build_groups(sessions: Vec<Session>) -> Vec<Group> {
    let mut map: HashMap<String, Vec<Session>> = HashMap::new();
    for s in sessions {
        map.entry(s.worktree.clone()).or_default().push(s);
    }
    let mut groups: Vec<Group> = map
        .into_iter()
        .map(|(worktree, mut sessions)| {
            sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
            Group {
                worktree,
                sessions,
                expanded: false,
            }
        })
        .collect();
    // Most-recently-active worktree first (by its newest session).
    groups.sort_by(|a, b| {
        let am = a.sessions.first().map(|s| s.mtime).unwrap_or(0);
        let bm = b.sessions.first().map(|s| s.mtime).unwrap_or(0);
        bm.cmp(&am).then_with(|| a.worktree.cmp(&b.worktree))
    });
    if let Some(first) = groups.first_mut() {
        first.expanded = true;
    }
    groups
}

/// A `left … right` line padded to `width`, with `left` truncated (…) if the
/// two would collide. Used for the worktree header's "name (count)".
fn justified(
    left: &str,
    right: &str,
    width: usize,
    left_style: Style,
    right_style: Style,
) -> Line<'static> {
    let r = right.chars().count();
    let max_left = width.saturating_sub(r + 1);
    let left = truncate(left, max_left);
    let pad = width.saturating_sub(left.chars().count() + r);
    Line::from(vec![
        Span::styled(left, left_style),
        Span::raw(" ".repeat(pad)),
        Span::styled(right.to_string(), right_style),
    ])
}

/// Header label for a worktree. Git worktrees for this repo live under
/// `.claude/worktrees/<name>` (see the worktree tooling); show just the leaf so
/// the rail reads `feat-x` instead of the full internal path. "main" and other
/// short labels pass through unchanged.
fn display_worktree(worktree: &str) -> &str {
    worktree
        .rsplit_once('/')
        .map(|(_, leaf)| leaf)
        .unwrap_or(worktree)
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
