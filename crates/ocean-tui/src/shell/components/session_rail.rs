//! SessionRailComponent — the left rail listing Ocean sessions for the current
//! project as a two-level tree, breadcrumbed like the file explorer:
//! DIRECTORY nodes (the main checkout or a worktree dir beneath it) contain
//! BRANCH nodes (the git branch stamped on each session record at creation)
//! which contain the sessions. Wears CTRL's SESSIONS-panel skin: slate bed,
//! plain SESSIONS title, dir headers in blue and branch headers in cyan with
//! ▸/▾ carets + a session count, sessions indented beneath with their title +
//! relative age, a cyan accent bar on the selected row, and a live `●` dot on
//! the session currently open in the chat/PTY.
//!
//! Enter on a header toggles the node; Enter on a session resumes it natively
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

/// A branch node under a directory: the sessions created on one git branch,
/// newest-first. `key` is the recorded branch name (or [`NO_BRANCH`] for
/// records that predate the `git_branch` field); `cwd` is where a `+ new`
/// session in this branch roots — the dir the branch's newest session actually
/// ran in.
struct BranchGroup {
    key: String,
    label: String,
    cwd: PathBuf,
    sessions: Vec<Session>,
    expanded: bool,
}

/// A directory node: the main checkout ("main" worktree label) or a worktree
/// dir beneath the project root, holding its branches. `key` is the worktree
/// label (stable across rescans — expansion is preserved on it); `label` is
/// the display form (the project folder name for the root checkout, the
/// worktree leaf otherwise).
struct DirGroup {
    key: String,
    label: String,
    cwd: PathBuf,
    branches: Vec<BranchGroup>,
    expanded: bool,
}

impl DirGroup {
    fn session_count(&self) -> usize {
        self.branches.iter().map(|b| b.sessions.len()).sum()
    }
}

/// Pseudo-branch bucket for legacy records that predate `git_branch` stamping.
/// Distinct from any real branch name (parentheses are illegal in git refs),
/// so an old record never falsely merges into a real `main` branch group.
const NO_BRANCH: &str = "(no branch)";

/// A reference to a currently-visible row: a directory header, a branch header
/// nested under one, or a session nested under a branch. Rebuilt each draw
/// from `groups` + expand state, exactly like the file tree's flattened
/// `entries`.
#[derive(Clone, Copy)]
enum Row {
    Dir(usize),
    Branch(usize, usize),
    Session(usize, usize, usize),
}

/// Width of the clickable "＋ (n)" button zone at the right edge of a header
/// row — a left-click landing here starts a new session in that dir/branch.
/// Covers "＋ (999)" comfortably.
const PLUS_ZONE: u16 = 8;

pub struct SessionRailComponent {
    root: PathBuf,
    groups: Vec<DirGroup>,
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
        let groups = build_groups(&root, discover(&root, Sort::Date));
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

    /// Flatten `groups` into the ordered list of visible rows: every dir
    /// header; the branch headers of expanded dirs; the sessions of expanded
    /// branches. Selection/scroll index into this. Cheap enough to rebuild per
    /// draw (matches the file tree).
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for (di, dir) in self.groups.iter().enumerate() {
            rows.push(Row::Dir(di));
            if !dir.expanded {
                continue;
            }
            for (bi, branch) in dir.branches.iter().enumerate() {
                rows.push(Row::Branch(di, bi));
                if branch.expanded {
                    for si in 0..branch.sessions.len() {
                        rows.push(Row::Session(di, bi, si));
                    }
                }
            }
        }
        rows
    }

    fn branch_at(&self, di: usize, bi: usize) -> Option<&BranchGroup> {
        self.groups.get(di).and_then(|d| d.branches.get(bi))
    }

    fn session_at(&self, row: Row) -> Option<&Session> {
        match row {
            Row::Session(di, bi, si) => self.branch_at(di, bi).and_then(|b| b.sessions.get(si)),
            _ => None,
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
        // Preserve expansion across a rescan: dir keys, and dir+branch
        // composite keys (a NUL join — neither side can contain one).
        let dirs_open: Vec<String> = self
            .groups
            .iter()
            .filter(|d| d.expanded)
            .map(|d| d.key.clone())
            .collect();
        let branches_open: Vec<String> = self
            .groups
            .iter()
            .flat_map(|d| {
                d.branches
                    .iter()
                    .filter(|b| b.expanded)
                    .map(move |b| format!("{}\u{0}{}", d.key, b.key))
            })
            .collect();
        self.groups = build_groups(&self.root, discover(&self.root, Sort::Date));
        if !dirs_open.is_empty() || !branches_open.is_empty() {
            for dir in &mut self.groups {
                dir.expanded = dirs_open.contains(&dir.key);
                for branch in &mut dir.branches {
                    branch.expanded =
                        branches_open.contains(&format!("{}\u{0}{}", dir.key, branch.key));
                }
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
            Row::Dir(di) => {
                if let Some(dir) = self.groups.get_mut(di) {
                    dir.expanded = !dir.expanded;
                }
                let n = self.rows().len();
                if self.selected >= n {
                    self.selected = n.saturating_sub(1);
                }
                None
            }
            Row::Branch(di, bi) => {
                if let Some(branch) = self.groups.get_mut(di).and_then(|d| d.branches.get_mut(bi)) {
                    branch.expanded = !branch.expanded;
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
        let uuid = uuid::Uuid::parse_str(&s.id).ok()?;
        Some(Action::ResumeSession {
            id: AgentSessionId(uuid),
            path: s.path.clone(),
            cwd: s.cwd.clone(),
        })
    }

    /// `+ new` in the node the given row belongs to: a dir header roots at the
    /// dir itself; a branch header (or a session under one) roots where that
    /// branch's newest session ran.
    fn new_session_in(&self, row: Row) -> Option<Action> {
        let cwd = match row {
            Row::Dir(di) => self.groups.get(di)?.cwd.clone(),
            Row::Branch(di, bi) | Row::Session(di, bi, _) => self.branch_at(di, bi)?.cwd.clone(),
        };
        Some(Action::NewSessionInProject { cwd })
    }

    /// The most-recently-active session for this project that can be resumed
    /// natively (its id parses as a session UUID). Used by the shell at launch
    /// so `ocean` / `cd project && ocean` drops back into the last conversation.
    /// Scans all groups by mtime rather than trusting group order, and skips
    /// legacy/non-UUID records (nothing to bind natively).
    pub fn latest_resumable(&self) -> Option<(AgentSessionId, PathBuf)> {
        self.groups
            .iter()
            .flat_map(|d| d.branches.iter())
            .flat_map(|b| b.sessions.iter())
            .filter_map(|s| {
                uuid::Uuid::parse_str(&s.id)
                    .ok()
                    .map(|u| (AgentSessionId(u), s.path.clone(), s.mtime))
            })
            .max_by_key(|(_, _, mtime)| *mtime)
            .map(|(id, path, _)| (id, path))
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
            // `n` (or `+`): new session in the selected row's dir/branch.
            KeyCode::Char('n') | KeyCode::Char('+') => {
                self.selected_row().and_then(|r| self.new_session_in(r))
            }
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
                let row = self.rows().get(i).copied();
                // Click on the rightmost "＋" zone of a header = new session in
                // that dir/branch (the button John wants), regardless of
                // selection.
                if let Some(r @ (Row::Dir(_) | Row::Branch(..))) = row {
                    let plus_x = self.body_rect.x + self.body_rect.width.saturating_sub(PLUS_ZONE);
                    if mouse.column >= plus_x {
                        self.selected = i;
                        return self.new_session_in(r);
                    }
                }
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
            panel::footer(frame, area, "");
            return;
        }

        let view_h = body.height as usize;
        self.clamp_scroll(view_h);
        let rows = self.rows();
        let inner = body.width.saturating_sub(1) as usize; // width after the accent bar
        let bottom = body.y + body.height;

        // Header line shared by dir + branch rows: `indent caret label … ＋ (n)`.
        // The label truncates to fit; ＋ and count keep their own colors.
        let header_line = |indent: &str,
                           expanded: bool,
                           label: &str,
                           count: usize,
                           color: ratatui::style::Color,
                           selected: bool| {
            let caret = if expanded {
                g("▾ ", "v ")
            } else {
                g("▸ ", "> ")
            };
            let count = format!("({count})");
            let plus = g("＋", "+");
            let text = format!("{indent}{caret}{label}");
            let right_w = plus.chars().count() + 1 + count.chars().count();
            let left = truncate(&text, inner.saturating_sub(right_w + 1));
            let pad = inner.saturating_sub(left.chars().count() + right_w);
            let label_style = Style::default().fg(color).add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            });
            Line::from(vec![
                Span::styled(left, label_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(
                    plus,
                    Style::default()
                        .fg(theme::GREEN)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(count, Style::default().fg(theme::COMMENT)),
            ])
        };

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
                Row::Dir(di) => {
                    let dir = &self.groups[di];
                    header_line(
                        "",
                        dir.expanded,
                        &dir.label,
                        dir.session_count(),
                        theme::BLUE,
                        selected,
                    )
                }
                Row::Branch(di, bi) => {
                    let branch = &self.groups[di].branches[bi];
                    header_line(
                        "  ",
                        branch.expanded,
                        &branch.label,
                        branch.sessions.len(),
                        theme::CYAN,
                        selected,
                    )
                }
                Row::Session(di, bi, si) => {
                    let s = &self.groups[di].branches[bi].sessions[si];
                    let live = self.live_id.as_deref() == Some(s.id.as_str());
                    let ago_s = ago(s.mtime);
                    // two levels of indent + a live-dot slot, matching the
                    // tree's "  " depth step.
                    let dot = if live { g("● ", "* ") } else { "  " };
                    let dot_style = if live {
                        Style::default().fg(theme::GREEN)
                    } else {
                        Style::default().fg(theme::COMMENT)
                    };
                    // budget: indent(4) + dot(2) + title + sp + ago
                    let left_cols = 4 + 2;
                    let title_max = inner.saturating_sub(left_cols + 1 + ago_s.chars().count());
                    let title = truncate(&s.title, title_max);
                    let title_style = Style::default().fg(theme::FG).add_modifier(if selected {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    });
                    // Build: "    " indent, dot, title … right-justified ago.
                    let used = left_cols + title.chars().count();
                    let pad = inner.saturating_sub(used + ago_s.chars().count());
                    Line::from(vec![
                        Span::styled("    ", Style::default()),
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

        panel::footer(frame, area, "");
    }
}

/// Group `sessions` (newest-first from `discover`) into the two-level tree:
/// directory nodes (the physical checkout/worktree a session ran in) holding
/// branch nodes (the git branch the daemon stamped on the record at creation)
/// holding the sessions. Two sessions run in the same checkout on different
/// branches land under the same dir but different branch nodes — which matches
/// how the work actually forked. Records that predate `git_branch` bucket
/// under [`NO_BRANCH`] within their dir, so they never falsely merge into a
/// real `main` branch group.
///
/// Order at every level: most recent activity first; sessions newest-first.
/// The most-recently-active dir AND its most-recently-active branch start
/// expanded, the rest collapsed — so `ocean` opens on your latest work without
/// the whole history sprawling open.
fn build_groups(root: &std::path::Path, sessions: Vec<Session>) -> Vec<DirGroup> {
    let mut by_dir: HashMap<String, Vec<Session>> = HashMap::new();
    for s in sessions {
        by_dir.entry(s.worktree.clone()).or_default().push(s);
    }
    let mut dirs: Vec<DirGroup> = by_dir
        .into_iter()
        .map(|(key, sessions)| {
            let mut by_branch: HashMap<String, Vec<Session>> = HashMap::new();
            for s in sessions {
                let bkey = s.branch.clone().unwrap_or_else(|| NO_BRANCH.to_string());
                by_branch.entry(bkey).or_default().push(s);
            }
            let mut branches: Vec<BranchGroup> = by_branch
                .into_iter()
                .map(|(bkey, mut sessions)| {
                    sessions.sort_by_key(|s| std::cmp::Reverse(s.mtime));
                    // `+ new` roots where this branch's latest session actually
                    // ran — the branch's live checkout — not a path guessed
                    // from the label.
                    let cwd = sessions
                        .first()
                        .map(|s| s.cwd.clone())
                        .unwrap_or_else(|| root.to_path_buf());
                    BranchGroup {
                        label: bkey.clone(),
                        key: bkey,
                        cwd,
                        sessions,
                        expanded: false,
                    }
                })
                .collect();
            // Most-recently-active branch first (by its newest session).
            branches.sort_by(|a, b| {
                let am = a.sessions.first().map(|s| s.mtime).unwrap_or(0);
                let bm = b.sessions.first().map(|s| s.mtime).unwrap_or(0);
                bm.cmp(&am).then_with(|| a.label.cmp(&b.label))
            });
            // The worktree key IS the cwd's path relative to root ("main" ==
            // root itself), so a `+ new` session on the dir header roots at
            // exactly that dir.
            let cwd = if key == "main" {
                root.to_path_buf()
            } else {
                root.join(&key)
            };
            // Display: the project folder name for the root checkout (calling
            // it "main" reads as the branch, which it no longer is); the
            // worktree leaf otherwise.
            let label = if key == "main" {
                root.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "main".to_string())
            } else {
                display_worktree(&key).to_string()
            };
            DirGroup {
                key,
                label,
                cwd,
                branches,
                expanded: false,
            }
        })
        .collect();
    // Most-recently-active dir first (by its newest session anywhere within).
    let newest = |d: &DirGroup| {
        d.branches
            .iter()
            .filter_map(|b| b.sessions.first().map(|s| s.mtime))
            .max()
            .unwrap_or(0)
    };
    dirs.sort_by(|a, b| {
        newest(b)
            .cmp(&newest(a))
            .then_with(|| a.label.cmp(&b.label))
    });
    if let Some(first) = dirs.first_mut() {
        first.expanded = true;
        if let Some(branch) = first.branches.first_mut() {
            branch.expanded = true;
        }
    }
    dirs
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str, branch: Option<&str>, worktree: &str, cwd: &str, mtime: u64) -> Session {
        Session {
            id: id.into(),
            title: format!("session {id}"),
            cwd: PathBuf::from(cwd),
            worktree: worktree.into(),
            branch: branch.map(str::to_string),
            mtime,
            path: PathBuf::from(format!("/tmp/{id}.json")),
        }
    }

    #[test]
    fn branches_nest_under_their_directory() {
        // Two sessions ran in the SAME checkout dir but on different branches
        // (John's actual workflow: work a branch, merge, forget to switch).
        // One dir node, two branch nodes nested inside it.
        let root = PathBuf::from("/repo");
        let dirs = build_groups(
            &root,
            vec![
                sess("a", Some("feat/x"), "main", "/repo", 100),
                sess("b", Some("main"), "main", "/repo", 50),
            ],
        );
        assert_eq!(dirs.len(), 1);
        // Root checkout displays as the project folder, not "main" (which now
        // reads as the branch).
        assert_eq!(dirs[0].label, "repo");
        assert_eq!(dirs[0].branches.len(), 2);
        assert_eq!(dirs[0].branches[0].label, "feat/x"); // newest floats up
        assert_eq!(dirs[0].branches[1].label, "main");
        // Branch names keep their slash — "feat/x" is not leafed to "x".
        assert_eq!(dirs[0].branches[0].key, "feat/x");
    }

    #[test]
    fn worktree_dirs_get_their_own_node() {
        // Same branch, two physical checkouts → two dir nodes, each holding
        // that branch.
        let root = PathBuf::from("/repo");
        let dirs = build_groups(
            &root,
            vec![
                sess("a", Some("feat/x"), "wt/feat-x", "/repo/wt/feat-x", 200),
                sess("b", Some("feat/x"), "main", "/repo", 100),
            ],
        );
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].label, "feat-x"); // worktree leaf, newest first
        assert_eq!(dirs[1].label, "repo");
        assert!(dirs.iter().all(|d| d.branches.len() == 1));
        // A branch's `+ new` roots where its newest session ran.
        assert_eq!(dirs[0].branches[0].cwd, PathBuf::from("/repo/wt/feat-x"));
        // A dir's `+ new` roots at the dir itself.
        assert_eq!(dirs[1].cwd, PathBuf::from("/repo"));
    }

    #[test]
    fn legacy_records_bucket_as_no_branch_without_merging_into_main() {
        // A pre-git_branch record must NOT merge into a real `main` branch
        // group — it buckets under the NO_BRANCH pseudo-branch in its dir.
        let root = PathBuf::from("/repo");
        let dirs = build_groups(
            &root,
            vec![
                sess("new", Some("main"), "main", "/repo", 100),
                sess("old", None, "main", "/repo", 50),
            ],
        );
        assert_eq!(dirs.len(), 1);
        let labels: Vec<&str> = dirs[0].branches.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, vec!["main", NO_BRANCH]);
        assert_eq!(dirs[0].branches[0].sessions.len(), 1);
        assert_eq!(dirs[0].branches[1].sessions.len(), 1);
    }

    #[test]
    fn most_recent_dir_and_branch_start_expanded_rest_collapsed() {
        let root = PathBuf::from("/repo");
        let dirs = build_groups(
            &root,
            vec![
                sess("a", Some("feat/x"), "main", "/repo", 300),
                sess("b", Some("main"), "main", "/repo", 200),
                sess("c", Some("feat/y"), "wt/old", "/repo/wt/old", 100),
            ],
        );
        assert_eq!(dirs.len(), 2);
        assert!(dirs[0].expanded);
        assert!(dirs[0].branches[0].expanded); // feat/x — the newest
        assert!(!dirs[0].branches[1].expanded);
        assert!(!dirs[1].expanded);
        assert!(dirs[1].branches.iter().all(|b| !b.expanded));
    }
}
