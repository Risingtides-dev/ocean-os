//! ChatComponent — the native agent surface. Re-houses the PM room's streaming
//! model (structured blocks: text, thinking, tool calls) onto the component
//! architecture, plus: permission approval cards (⌃Y allow / ⌃N deny, the
//! OCEAN-185 gated flow), streaming markdown with prefix-freeze (via
//! `shell::markdown` — headings, syntax-highlighted fences, lists, blockquotes,
//! inline `code`/**bold**/*italic*), tool cards with ⌃O collapse/expand,
//! multi-line input (⌃J newline), and wheel/PageUp scrollback.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ocean_agent_sdk::{AgentTurnEvent, ToolCallId};
use ocean_core::{OceanEvent, PermissionId};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};

use crate::shell::{
    action::{Action, Nav},
    component::Component,
    diff::{self, DiffKind, DiffRow},
    history::PromptHistory,
    markdown::Markdown,
    panel, slash,
    theme::{self, g},
};

/// Collapsed tool cards show at most this many trailing output lines; ⌃O
/// expands to the full output.
const TOOL_TAIL_ROWS: usize = 3;

/// Collapsed diff cards show at most this many rows; ⌃O (the same global expand
/// toggle as tool output) reveals the full hunk.
const DIFF_TAIL_ROWS: usize = 12;

/// Kill-ring depth (⌃U / ⌃K push, ⌃Y yanks the newest).
const KILL_RING_CAP: usize = 10;

/// One rendered unit of transcript.
enum Turn {
    /// Operator's prompt.
    User(String),
    /// Assistant visible text (accumulates deltas).
    Assistant(String),
    /// Extended-thinking text (accumulates deltas).
    Thinking(String),
    /// A tool call: keyed by call id, with name + a one-line args summary +
    /// streamed output + status. Rendered as a card (⌃O toggles collapse). When
    /// the tool is an edit tool, `diff` carries the pre-computed diff-card rows
    /// and the card renders those instead of raw output.
    Tool {
        id: ToolCallId,
        name: String,
        args: String,
        output: String,
        status: ToolStatus,
        diff: Option<Vec<DiffRow>>,
    },
    /// An advisor aside — a note from the observer/advisor extension. Rendered as
    /// a set-off amber card, clearly not the agent's own output.
    Advisor {
        note: String,
        severity: String,
        model: String,
    },
    /// A gated tool waiting on the operator (OCEAN-185). `resolved` is `None`
    /// while waiting, then Some(allowed).
    Permission {
        permission_id: PermissionId,
        tool: String,
        reason: String,
        resolved: Option<bool>,
    },
}

#[derive(PartialEq)]
enum ToolStatus {
    Running,
    Ok,
    Err,
}

/// The ⌃R fuzzy history-search overlay state (present only while open).
#[derive(Default)]
struct HistorySearch {
    /// Typed query, fuzzy-matched against history entries.
    query: String,
    /// Highlighted row in the match list.
    sel: usize,
}

#[derive(Default)]
pub struct ChatComponent {
    turns: Vec<Turn>,
    input: String,
    model: Option<String>,
    busy: bool,
    /// Scrollback offset in lines from the bottom (0 = stick to live tail).
    scroll_back: usize,
    /// Highlighted row in the `/` command palette (see `slash_matches`).
    menu_sel: usize,
    /// Streaming markdown renderer + frozen-block cache for assistant text.
    md: Markdown,
    /// When true, tool cards render their full output instead of a tail window.
    tools_expanded: bool,
    /// Persisted prompt history (↑/↓ recall, ⌃R search). Loaded at startup.
    history: PromptHistory,
    /// Cursor into `history` while navigating with ↑/↓; `None` when editing a
    /// fresh line rather than walking history.
    history_idx: Option<usize>,
    /// The in-progress composer text saved when ↑ first enters history, restored
    /// when ↓ walks back past the newest entry.
    draft: String,
    /// Kill ring: ⌃U / ⌃K push, ⌃Y yanks the newest (cap [`KILL_RING_CAP`]).
    kill_ring: Vec<String>,
    /// The ⌃R history-search overlay, when open.
    search: Option<HistorySearch>,
    /// Project root for `@`-file mentions; set by the app (follows the active
    /// project). `None` until first set.
    mention_root: Option<std::path::PathBuf>,
    /// Lazy, per-project file index for the `@` picker (scanned on first `@`,
    /// invalidated when `mention_root` changes).
    mention_index: Option<Vec<String>>,
    /// Highlighted row in the `@` mention picker.
    mention_sel: usize,
    pub focused: bool,
}

/// Collapse a tool's `args_json` into a single-line summary for the card header:
/// prefer a well-known primary key, else compact-serialize the whole object;
/// newlines flattened and truncated so the header never wraps.
fn summarize_args(v: &serde_json::Value) -> String {
    use serde_json::Value;
    const KEYS: &[&str] = &[
        "command",
        "cmd",
        "path",
        "file_path",
        "pattern",
        "query",
        "url",
        "content",
    ];
    let raw = match v {
        Value::Object(map) => KEYS
            .iter()
            .find_map(|k| map.get(*k))
            .map(inline_value)
            .unwrap_or_else(|| v.to_string()),
        Value::Null => String::new(),
        other => inline_value(other),
    };
    one_line(&raw, 72)
}

fn inline_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Render one diff-card row: a coloured gutter sigil + the (possibly word-diffed)
/// body on the dark bed. Changed word runs carry `Modifier::REVERSED` (SGR
/// inverse), matching OMP's intra-line diff highlight.
fn diff_line(row: &DiffRow) -> Line<'static> {
    let (gutter, gutter_fg, body_fg, dim) = match row.kind {
        DiffKind::Del => (g("-", "-"), theme::RED, theme::RED, false),
        DiffKind::Add => (g("+", "+"), theme::GREEN, theme::GREEN, false),
        DiffKind::Context => (" ", theme::EDGE, theme::COMMENT, false),
        DiffKind::Header => (g("┆", ":"), theme::COMMENT, theme::COMMENT, true),
    };
    let mut spans = vec![Span::styled(
        format!("    {gutter} "),
        Style::default().fg(gutter_fg),
    )];
    for seg in &row.segs {
        let mut style = Style::default().fg(body_fg).bg(theme::BG_DARK);
        if dim {
            style = style.add_modifier(Modifier::DIM);
        }
        if seg.changed {
            style = style.add_modifier(Modifier::REVERSED);
        }
        spans.push(Span::styled(seg.text.clone(), style));
    }
    Line::from(spans)
}

/// Flatten whitespace to single spaces and truncate to `max` chars with an
/// ellipsis.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max {
        let head: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{head}{}", g("…", "..."))
    } else {
        flat
    }
}

impl ChatComponent {
    /// Construct the chat surface with prompt history loaded from disk (for
    /// ↑/↓ recall and ⌃R search). `Default` leaves history empty — used in
    /// tests that don't touch disk.
    pub fn new() -> Self {
        Self {
            history: PromptHistory::load(),
            ..Default::default()
        }
    }

    // ── prompt history: ↑/↓ recall ───────────────────────────────────────────

    /// Leave history-navigation mode (any edit or submit resets it).
    fn reset_history_nav(&mut self) {
        self.history_idx = None;
        self.draft.clear();
    }

    /// ↑ — walk one step back through history. Only fires when the composer is
    /// empty or already navigating, so it never hijacks an in-progress draft.
    fn history_prev(&mut self) {
        if self.history_idx.is_none() && !self.input.is_empty() {
            return; // non-history text in the composer — don't hijack ↑
        }
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_idx {
            None => {
                self.draft = self.input.clone(); // stash the (empty) draft
                self.history.len() - 1
            }
            Some(0) => 0, // already at the oldest entry — stay put
            Some(i) => i - 1,
        };
        self.history_idx = Some(idx);
        self.input = self.history.get(idx).unwrap_or("").to_string();
        self.menu_sel = 0;
    }

    /// ↓ — walk one step forward through history. Only fires while navigating;
    /// walking past the newest entry restores the stashed draft.
    fn history_next(&mut self) {
        let Some(i) = self.history_idx else {
            return; // not navigating — don't hijack ↓
        };
        if i + 1 < self.history.len() {
            self.history_idx = Some(i + 1);
            self.input = self.history.get(i + 1).unwrap_or("").to_string();
        } else {
            self.input = std::mem::take(&mut self.draft);
            self.history_idx = None;
        }
        self.menu_sel = 0;
    }

    // ── kill ring: ⌃U / ⌃K / ⌃Y ───────────────────────────────────────────────

    /// Push killed text onto the ring (newest last), capped at [`KILL_RING_CAP`].
    fn push_kill(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        self.kill_ring.push(text);
        if self.kill_ring.len() > KILL_RING_CAP {
            self.kill_ring.remove(0);
        }
    }

    /// ⌃U — kill the whole composer (cursor is pinned to the end, so kill-to-
    /// start is the entire line) onto the ring.
    fn kill_to_start(&mut self) {
        if !self.input.is_empty() {
            let killed = std::mem::take(&mut self.input);
            self.push_kill(killed);
            self.reset_history_nav();
            self.menu_sel = 0;
        }
    }

    /// ⌃K — kill the current (last) line's content onto the ring. With the
    /// cursor at the end, "kill-to-end" is empty, so this is adapted to trim the
    /// final line (equals ⌃U on a single-line composer, trims just the tail line
    /// on a multi-line one). Judgment call — the composer has no horizontal
    /// cursor.
    fn kill_to_end(&mut self) {
        let killed = match self.input.rfind('\n') {
            Some(nl) => self.input.split_off(nl + 1),
            None => std::mem::take(&mut self.input),
        };
        if !killed.is_empty() {
            self.push_kill(killed);
            self.reset_history_nav();
            self.menu_sel = 0;
        }
    }

    /// ⌃Y — yank the newest kill at the cursor (end). NOTE: a pending permission
    /// takes precedence over yank (see `handle_key`); this only runs when none
    /// is pending.
    fn yank(&mut self) {
        if let Some(kill) = self.kill_ring.last().cloned() {
            self.input.push_str(&kill);
            self.reset_history_nav();
            self.menu_sel = 0;
        }
    }

    // ── ⌃R history search ──────────────────────────────────────────────────────

    /// History entries matching the current search query, best-first then newest-
    /// first, as indices into the history ring. Empty query matches everything.
    fn search_matches(&self) -> Vec<usize> {
        let Some(s) = self.search.as_ref() else {
            return Vec::new();
        };
        let mut scored: Vec<(usize, i32)> = self
            .history
            .entries()
            .iter()
            .enumerate()
            .filter_map(|(i, e)| slash::subseq_score(&s.query, e).map(|sc| (i, sc)))
            .collect();
        // Best score first; ties break to the newer entry (higher index).
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.0.cmp(&a.0)));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    /// Drive the ⌃R search overlay. Enter inserts the selection into the
    /// composer; Esc dismisses; ↑/↓ move the cursor; typing filters.
    fn handle_search_key(&mut self, key: KeyEvent) -> Option<Action> {
        let matches = self.search_matches();
        match key.code {
            KeyCode::Esc => self.search = None,
            KeyCode::Enter => {
                let sel = self.search.as_ref().map(|s| s.sel).unwrap_or(0);
                if let Some(&idx) = matches.get(sel) {
                    if let Some(entry) = self.history.get(idx) {
                        self.input = entry.to_string();
                    }
                }
                self.search = None;
                self.reset_history_nav();
                self.menu_sel = 0;
            }
            KeyCode::Up => {
                if let Some(s) = self.search.as_mut() {
                    s.sel = s.sel.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some(s) = self.search.as_mut() {
                    if !matches.is_empty() {
                        s.sel = (s.sel + 1).min(matches.len() - 1);
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(s) = self.search.as_mut() {
                    s.query.pop();
                    s.sel = 0;
                }
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(s) = self.search.as_mut() {
                    s.query.push(c);
                    s.sel = 0;
                }
            }
            _ => {}
        }
        None
    }

    /// Replace the transcript with a resumed session's history (from disk).
    pub fn load_history(&mut self, msgs: Vec<crate::shell::sessions::HistoryMsg>) {
        self.turns = msgs
            .into_iter()
            .map(|m| {
                if m.role == "user" {
                    Turn::User(m.text)
                } else {
                    Turn::Assistant(m.text)
                }
            })
            .collect();
        self.md.clear();
        self.busy = false;
    }

    /// Append an assistant text delta, coalescing into the trailing Assistant
    /// block when the last turn is already assistant text.
    fn push_assistant(&mut self, delta: &str) {
        match self.turns.last_mut() {
            Some(Turn::Assistant(s)) => s.push_str(delta),
            _ => self.turns.push(Turn::Assistant(delta.to_string())),
        }
    }

    fn push_thinking(&mut self, delta: &str) {
        match self.turns.last_mut() {
            Some(Turn::Thinking(s)) => s.push_str(delta),
            _ => self.turns.push(Turn::Thinking(delta.to_string())),
        }
    }

    fn tool_by_id(&mut self, id: &ToolCallId) -> Option<&mut Turn> {
        self.turns
            .iter_mut()
            .rev()
            .find(|t| matches!(t, Turn::Tool { id: tid, .. } if tid == id))
    }

    /// Fold an `advisor` extension event into an [`Turn::Advisor`]. Tolerates
    /// missing fields (sensible defaults), skips empty notes, and never panics
    /// on a malformed payload.
    fn push_advisor(&mut self, payload: &serde_json::Value) {
        let note = payload.get("note").and_then(|v| v.as_str()).unwrap_or("");
        if note.trim().is_empty() {
            return;
        }
        let severity = payload
            .get("severity")
            .and_then(|v| v.as_str())
            .unwrap_or("info")
            .to_string();
        let model = payload
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        self.turns.push(Turn::Advisor {
            note: note.to_string(),
            severity,
            model,
        });
    }

    /// The newest unresolved permission card, if any — the ⌃Y/⌃N target.
    fn pending_permission(&self) -> Option<PermissionId> {
        self.turns.iter().rev().find_map(|t| match t {
            Turn::Permission {
                permission_id,
                resolved: None,
                ..
            } => Some(*permission_id),
            _ => None,
        })
    }

    fn resolve_permission(&mut self, id: PermissionId, allowed: bool) {
        for t in self.turns.iter_mut().rev() {
            if let Turn::Permission {
                permission_id,
                resolved,
                ..
            } = t
            {
                if *permission_id == id {
                    *resolved = Some(allowed);
                    return;
                }
            }
        }
    }

    /// The ranked command palette for the current composer text, or empty when
    /// the composer isn't in `/`-command mode. Active only when the input starts
    /// with `/` and the query (everything after it) has no whitespace — so a
    /// normal message that merely mentions a slash never triggers the menu.
    fn slash_matches(&self) -> Vec<&'static slash::SlashCommand> {
        match self.input.strip_prefix('/') {
            Some(q) if !q.contains(char::is_whitespace) => {
                slash::filter(q).into_iter().map(|(c, _)| c).collect()
            }
            _ => Vec::new(),
        }
    }

    // ── `@` file mentions ────────────────────────────────────────────────────

    /// Point `@`-mentions at a project root. Invalidates the file index when the
    /// root actually changes (the app calls this on every project re-root).
    pub fn set_mention_root(&mut self, root: std::path::PathBuf) {
        if self.mention_root.as_deref() != Some(root.as_path()) {
            self.mention_root = Some(root);
            self.mention_index = None; // rescan lazily on next `@`
        }
    }

    /// The active `@` mention in the composer: the trailing token, when it
    /// starts with `@`. Returns (byte offset of the `@`, query after it).
    /// Token = everything after the last whitespace, so `fix @src/ma` matches
    /// and `email me a@b.com` does not (the `@` must LEAD the token).
    fn mention_query(&self) -> Option<(usize, &str)> {
        let start = self
            .input
            .rfind(char::is_whitespace)
            .map(|i| i + 1)
            .unwrap_or(0);
        let token = &self.input[start..];
        token.starts_with('@').then(|| (start, &token[1..]))
    }

    /// Ranked file matches for the current `@` query (empty when not in mention
    /// mode). Scans the project lazily on first use.
    fn mention_matches(&mut self) -> Vec<String> {
        let Some((_, query)) = self.mention_query() else {
            return Vec::new();
        };
        let query = query.to_string();
        if self.mention_index.is_none() {
            let root = self
                .mention_root
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            self.mention_index = Some(crate::shell::mentions::scan(&root));
        }
        let index = self.mention_index.as_deref().unwrap_or(&[]);
        crate::shell::mentions::filter(index, &query, 8)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// Replace the active `@token` with the picked path (plus a trailing space)
    /// so the composer reads `… @src/main.rs `.
    fn insert_mention(&mut self, path: &str) {
        if let Some((at, _)) = self.mention_query() {
            self.input.truncate(at);
            self.input.push('@');
            self.input.push_str(path);
            self.input.push(' ');
            self.mention_sel = 0;
        }
    }

    /// Execute a slash command by name, with any trailing `args` (empty for a
    /// palette pick; the tail of a typed `/name args` line otherwise). Clears
    /// the composer, then either mutates the transcript locally (`/clear`,
    /// `/help`), emits an [`Action`], or — for a `soon` roadmap command —
    /// surfaces an honest "not wired on this branch" hint. Pane focus rides
    /// [`Action::Navigate`]; chat never reaches into the app's Focus/Center.
    fn run_slash(&mut self, name: &str, args: &str) -> Option<Action> {
        self.input.clear();
        self.menu_sel = 0;
        let args = args.trim();
        match name {
            "/quit" => Some(Action::Quit),
            "/clear" => {
                self.turns.clear();
                self.md.clear();
                self.scroll_back = 0;
                self.busy = false;
                None
            }
            "/help" => {
                self.push_help();
                self.scroll_back = 0;
                None
            }
            "/new" => {
                // Fresh session: wipe the transcript locally, then let the app
                // unbind so the next turn mints a new session id.
                self.turns.clear();
                self.md.clear();
                self.scroll_back = 0;
                self.busy = false;
                Some(Action::NewSession)
            }
            "/model" => {
                if args.is_empty() {
                    Some(Action::Status(
                        "usage: /model <provider/model> (e.g. anthropic/claude-opus-4-8)".into(),
                    ))
                } else {
                    Some(Action::SetModel(args.to_string()))
                }
            }
            "/copy" => match self.last_reply() {
                Some(text) => Some(Action::CopyToClipboard(text)),
                None => Some(Action::Status("nothing to copy yet".into())),
            },
            // Pane/center navigation — the app owns Focus/Center, so emit a
            // targeted Navigate and let it move there.
            "/sessions" | "/resume" => Some(Action::Navigate(Nav::Sessions)),
            "/files" => Some(Action::Navigate(Nav::Files)),
            "/graph" => Some(Action::Navigate(Nav::Graph)),
            "/terminal" => Some(Action::Navigate(Nav::Terminal)),
            // Roadmap commands: present in the palette as a discoverability map,
            // but honest that the backend isn't on this branch yet.
            _ => {
                let hint = slash::COMMANDS
                    .iter()
                    .find(|c| c.name == name)
                    .filter(|c| c.soon)
                    .map(|c| format!("{} — {} · not wired on this branch yet", c.name, c.desc))
                    .unwrap_or_else(|| format!("unknown command: {name}"));
                Some(Action::Status(hint))
            }
        }
    }

    /// The text of the newest assistant reply, for `/copy`.
    fn last_reply(&self) -> Option<String> {
        self.turns.iter().rev().find_map(|t| match t {
            Turn::Assistant(s) if !s.trim().is_empty() => Some(s.clone()),
            _ => None,
        })
    }

    /// Push `/help` output into the transcript as an assistant block — the
    /// markdown-lite renderer styles the heading, bullets, and inline `code`.
    fn push_help(&mut self) {
        let mut body = String::from("# commands\n");
        for c in slash::COMMANDS {
            body.push_str(&format!("- `{}` — {}\n", c.name, c.desc));
        }
        self.turns.push(Turn::Assistant(body));
    }

    /// Render the floating command palette just above the composer, listing the
    /// ranked matches with the selection on a `BG_HL` bed. Overlaid last so it
    /// sits on top of the transcript.
    fn draw_menu(&self, frame: &mut Frame, composer: Rect, matches: &[&slash::SlashCommand]) {
        if matches.is_empty() {
            return;
        }
        // Show as many as fit in the space above the composer, capped so the
        // roadmap is visible without the popup running off the top of the frame.
        let cap = (composer.y as usize).saturating_sub(3).clamp(1, 20);
        let shown = matches.len().min(cap);
        let sel = self.menu_sel.min(shown - 1);

        // `soon` rows carry a right-aligned " · soon" badge (7 cols). Width fits
        // the widest ACTUAL row, capped to the composer width. Each row renders
        // as ` {marker} ` (3) + `{:<11}` name (min 11) + ` — ` (3) + desc, plus
        // the badge on roadmap rows — mirror that exactly or long descriptions
        // clip mid-word against the composer cap.
        const BADGE_W: usize = 7; // " · soon"
        let content_w = matches
            .iter()
            .take(shown)
            .map(|c| {
                3 + c.name.chars().count().max(11)
                    + 3
                    + c.desc.chars().count()
                    + if c.soon { BADGE_W } else { 0 }
            })
            .max()
            .unwrap_or(24);
        let width = ((content_w as u16) + 2 /* borders */)
            .min(composer.width)
            .max(24);
        let height = shown as u16 + 3; // top+bottom border + footer row
        let y = composer.y.saturating_sub(height);
        let area = Rect::new(composer.x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                format!(" {} commands ", g("◆", "*")),
                Style::default().fg(theme::BLUE).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        for (i, c) in matches.iter().take(shown).enumerate() {
            let selected = i == sel;
            let bed = if selected { theme::BG_HL } else { theme::SLATE };
            // Live rows read bold in FG/CYAN; roadmap (soon) rows are muted so
            // the working commands stay visually dominant.
            let (name_fg, name_mod) = match (selected, c.soon) {
                (true, false) => (theme::CYAN, Modifier::BOLD),
                (true, true) => (theme::YELLOW, Modifier::empty()),
                (false, false) => (theme::FG, Modifier::BOLD),
                (false, true) => (theme::COMMENT, Modifier::empty()),
            };
            let marker = if selected { g("❯", ">") } else { " " };
            let mut spans = vec![
                Span::styled(format!(" {marker} "), Style::default().fg(theme::CYAN)),
                Span::styled(
                    format!("{:<11}", c.name),
                    Style::default().fg(name_fg).add_modifier(name_mod),
                ),
                Span::styled(
                    format!(" {} {}", g("—", "-"), c.desc),
                    Style::default().fg(theme::COMMENT),
                ),
            ];
            if c.soon {
                let left_w = 3 + c.name.chars().count().max(11) + 3 + c.desc.chars().count();
                let target = inner.width as usize;
                if target > left_w + BADGE_W {
                    spans.push(Span::raw(" ".repeat(target - left_w - BADGE_W)));
                }
                spans.push(Span::styled(
                    format!(" {} soon", g("·", "-")),
                    Style::default().fg(theme::YELLOW),
                ));
            }
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(bed)),
                Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
            );
        }
        // Footer hint on the last inner row; note the truncation when the list
        // is longer than what fits.
        let more = if shown < matches.len() {
            format!(" · {shown}/{}", matches.len())
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(" {} select · ⏎ run · esc dismiss{more}", g("↑↓", "^v")),
                Style::default().fg(theme::COMMENT),
            ))
            .style(Style::default().bg(theme::SLATE)),
            Rect::new(inner.x, inner.y + shown as u16, inner.width, 1),
        );
    }

    /// Render the `@` file-mention picker just above the composer: ranked file
    /// paths with the basename emphasized, selection on a `BG_HL` bed. Same
    /// floating idiom as the `/` palette.
    fn draw_mentions(&self, frame: &mut Frame, composer: Rect, matches: &[String]) {
        if matches.is_empty() {
            return;
        }
        let cap = (composer.y as usize).saturating_sub(3).clamp(1, 8);
        let shown = matches.len().min(cap);
        let sel = self.mention_sel.min(shown - 1);

        let content_w = matches
            .iter()
            .take(shown)
            .map(|p| 3 + 1 + p.chars().count()) // " ❯ " + "@" + path
            .max()
            .unwrap_or(24);
        let width = ((content_w as u16) + 2).min(composer.width).max(24);
        let height = shown as u16 + 3;
        let y = composer.y.saturating_sub(height);
        let area = Rect::new(composer.x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                format!(" {} files ", g("◆", "*")),
                Style::default().fg(theme::BLUE).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        for (i, p) in matches.iter().take(shown).enumerate() {
            let selected = i == sel;
            let bed = if selected { theme::BG_HL } else { theme::SLATE };
            let marker = if selected { g("❯", ">") } else { " " };
            // dir prefix muted, basename bright — you pick by basename.
            let (dir, base) = match p.rsplit_once('/') {
                Some((d, b)) => (format!("{d}/"), b.to_string()),
                None => (String::new(), p.clone()),
            };
            let spans = vec![
                Span::styled(format!(" {marker} "), Style::default().fg(theme::CYAN)),
                Span::styled("@", Style::default().fg(theme::CYAN)),
                Span::styled(dir, Style::default().fg(theme::COMMENT)),
                Span::styled(
                    base,
                    Style::default()
                        .fg(if selected { theme::CYAN } else { theme::FG })
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            frame.render_widget(
                Paragraph::new(Line::from(spans)).style(Style::default().bg(bed)),
                Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
            );
        }
        let more = if shown < matches.len() {
            format!(" · {shown}/{}", matches.len())
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(" {} select · ⏎ insert · esc dismiss{more}", g("↑↓", "^v")),
                Style::default().fg(theme::COMMENT),
            ))
            .style(Style::default().bg(theme::SLATE)),
            Rect::new(inner.x, inner.y + shown as u16, inner.width, 1),
        );
    }

    /// Render the ⌃R history-search overlay above the composer: a title row
    /// carrying the live query, then fuzzy-matched history entries (newest
    /// first) with the selection on a `BG_HL` bed. Same popup skin as the `/`
    /// palette.
    fn draw_search(&self, frame: &mut Frame, composer: Rect) {
        let matches = self.search_matches();
        let query = self.search.as_ref().map(|s| s.query.as_str()).unwrap_or("");
        let sel = self.search.as_ref().map(|s| s.sel).unwrap_or(0);
        let shown = matches.len().min(8);
        let sel = sel.min(shown.saturating_sub(1));

        // Width fits the widest entry (capped), with a sane floor.
        let content_w = matches
            .iter()
            .take(shown)
            .filter_map(|&i| self.history.get(i))
            .map(|e| e.chars().take(60).count())
            .max()
            .unwrap_or(24)
            .max(query.chars().count() + 4);
        let width = ((content_w as u16) + 6).min(composer.width).max(24);
        let height = shown.max(1) as u16 + 3; // top+bottom border + footer row
        let y = composer.y.saturating_sub(height);
        let area = Rect::new(composer.x, y, width, height);

        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme::EDGE).bg(theme::SLATE))
            .style(Style::default().bg(theme::SLATE))
            .title(Span::styled(
                format!(" {} history: {query}", g("⌕", "?")),
                Style::default().fg(theme::BLUE).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        if matches.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    " no matching history",
                    Style::default().fg(theme::COMMENT),
                ))
                .style(Style::default().bg(theme::SLATE)),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
        }
        for (i, &hidx) in matches.iter().take(shown).enumerate() {
            let selected = i == sel;
            let bed = if selected { theme::BG_HL } else { theme::SLATE };
            let fg = if selected { theme::CYAN } else { theme::FG };
            let marker = if selected { g("❯", ">") } else { " " };
            // Flatten newlines (multi-line prompts) into the one-row preview.
            let preview = self
                .history
                .get(hidx)
                .map(|e| one_line(e, inner.width.saturating_sub(4) as usize))
                .unwrap_or_default();
            let row = Line::from(vec![
                Span::styled(format!(" {marker} "), Style::default().fg(theme::CYAN)),
                Span::styled(preview, Style::default().fg(fg)),
            ]);
            frame.render_widget(
                Paragraph::new(row).style(Style::default().bg(bed)),
                Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
            );
        }
        // Footer hint on the last inner row.
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(" {} select · ⏎ insert · esc dismiss", g("↑↓", "^v")),
                Style::default().fg(theme::COMMENT),
            ))
            .style(Style::default().bg(theme::SLATE)),
            Rect::new(inner.x, inner.y + shown.max(1) as u16, inner.width, 1),
        );
    }
}

impl Component for ChatComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        // The ⌃R history-search overlay is modal: while open, all keys drive it.
        if self.search.is_some() {
            return self.handle_search_key(key);
        }
        // Permission decisions work even mid-stream (legacy ⌃Y/⌃N bindings).
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('y') => {
                    // Permission-pending takes precedence over yank; ⌃Y only
                    // yanks the kill ring when no permission is waiting.
                    if let Some(id) = self.pending_permission() {
                        return Some(Action::PermissionDecided {
                            permission_id: id,
                            allow: true,
                        });
                    }
                    self.yank();
                    return None;
                }
                KeyCode::Char('n') => {
                    if let Some(id) = self.pending_permission() {
                        return Some(Action::PermissionDecided {
                            permission_id: id,
                            allow: false,
                        });
                    }
                    return None;
                }
                // ⌃J: newline in the composer (legacy binding).
                KeyCode::Char('j') => {
                    self.input.push('\n');
                    return None;
                }
                // ⌃O: toggle tool-card expansion (full output/diff ↔ tail window).
                KeyCode::Char('o') => {
                    self.tools_expanded = !self.tools_expanded;
                    return None;
                }
                // ⌃R: open the fuzzy prompt-history search overlay.
                KeyCode::Char('r') => {
                    self.search = Some(HistorySearch::default());
                    return None;
                }
                // ⌃U: kill the whole composer to the ring.
                KeyCode::Char('u') => {
                    self.kill_to_start();
                    return None;
                }
                // ⌃K: kill the current line's tail to the ring.
                KeyCode::Char('k') => {
                    self.kill_to_end();
                    return None;
                }
                _ => {}
            }
        }
        // ── `/` command palette: nav + execute intercept ─────────────────────
        // Active only while the composer is in command mode (input starts with
        // `/`). These keys drive the menu instead of the composer. NOTE: Tab is
        // swallowed by the app for focus-cycling before it reaches us, so Enter
        // is the primary execute key; Tab-to-complete is handled defensively in
        // case that routing ever changes.
        let matches = self.slash_matches();
        if !matches.is_empty() {
            match key.code {
                KeyCode::Up => {
                    self.menu_sel = self.menu_sel.saturating_sub(1);
                    return None;
                }
                KeyCode::Down => {
                    self.menu_sel = (self.menu_sel + 1).min(matches.len() - 1);
                    return None;
                }
                KeyCode::Tab => {
                    let sel = self.menu_sel.min(matches.len() - 1);
                    self.input = matches[sel].name.to_string();
                    self.menu_sel = 0;
                    return None;
                }
                KeyCode::Enter => {
                    let sel = self.menu_sel.min(matches.len() - 1);
                    let name = matches[sel].name;
                    return self.run_slash(name, "");
                }
                KeyCode::Esc => {
                    self.input.clear();
                    self.menu_sel = 0;
                    return None;
                }
                _ => {}
            }
        }
        // ── `@` file-mention picker: nav + insert intercept ──────────────────
        // Active while the trailing token starts with `@` and files match. Enter
        // / Tab insert the selected path into the composer (send stays a second
        // Enter); Esc drops the `@` sigil, closing the picker but keeping the
        // typed text.
        let mentions = self.mention_matches();
        if !mentions.is_empty() {
            match key.code {
                KeyCode::Up => {
                    self.mention_sel = self.mention_sel.saturating_sub(1);
                    return None;
                }
                KeyCode::Down => {
                    self.mention_sel = (self.mention_sel + 1).min(mentions.len() - 1);
                    return None;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    let sel = self.mention_sel.min(mentions.len() - 1);
                    let path = mentions[sel].clone();
                    self.insert_mention(&path);
                    return None;
                }
                KeyCode::Esc => {
                    if let Some((at, _)) = self.mention_query() {
                        self.input.remove(at); // drop the `@`, keep the text
                    }
                    self.mention_sel = 0;
                    return None;
                }
                _ => {}
            }
        }
        match (key.code, key.modifiers) {
            (KeyCode::PageUp, _) => {
                self.scroll_back += 10;
                None
            }
            (KeyCode::PageDown, _) => {
                self.scroll_back = self.scroll_back.saturating_sub(10);
                None
            }
            // ↑/↓ recall history when the composer is empty or already
            // navigating; otherwise they no-op (scrolling is wheel/PgUp).
            (KeyCode::Up, KeyModifiers::NONE) => {
                self.history_prev();
                None
            }
            (KeyCode::Down, KeyModifiers::NONE) => {
                self.history_next();
                None
            }
            (KeyCode::Enter, _) => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                // A typed `/name args…` line (the palette closes once a space is
                // typed) is a command invocation when the first token is a known
                // command — e.g. `/model anthropic/claude-opus-4-8`. Anything
                // else starting with `/` (a path, say) falls through and sends.
                if text.starts_with('/') {
                    let (name, args) = match text.split_once(char::is_whitespace) {
                        Some((n, a)) => (n, a),
                        None => (text.as_str(), ""),
                    };
                    if slash::is_command(name) {
                        return self.run_slash(name, args);
                    }
                }
                self.history.push(&text); // persist for ↑/↓ + ⌃R recall
                self.reset_history_nav();
                self.input.clear();
                self.scroll_back = 0; // sending snaps to the live tail
                self.turns.push(Turn::User(text.clone()));
                self.busy = true;
                Some(Action::SubmitPrompt(text))
            }
            (KeyCode::Backspace, _) => {
                self.input.pop();
                self.history_idx = None; // editing exits history navigation
                self.menu_sel = 0; // query narrowed → reset palette cursor
                self.mention_sel = 0;
                None
            }
            (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
                self.input.push(c);
                self.history_idx = None; // editing exits history navigation
                self.menu_sel = 0; // query changed → reset palette cursor
                self.mention_sel = 0;
                None
            }
            _ => None,
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) -> Option<Action> {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_back += 3,
            MouseEventKind::ScrollDown => self.scroll_back = self.scroll_back.saturating_sub(3),
            _ => {}
        }
        None
    }

    fn update(&mut self, action: &Action) -> Option<Action> {
        // Permission traffic rides the GLOBAL event stream, not the agent one.
        if let Action::OceanEvent(env) = action {
            match &env.event {
                OceanEvent::PermissionRequest { tool, reason, .. } => {
                    if let Some(pid) = env.permission_id {
                        self.turns.push(Turn::Permission {
                            permission_id: pid,
                            tool: tool.clone(),
                            reason: reason.clone(),
                            resolved: None,
                        });
                        self.scroll_back = 0; // surface the prompt immediately
                    }
                }
                OceanEvent::PermissionDecision { allowed, .. } => {
                    if let Some(pid) = env.permission_id {
                        self.resolve_permission(pid, *allowed);
                    }
                }
                _ => {}
            }
            return None;
        }
        if let Action::AgentEvent(evt) = action {
            match evt.as_ref() {
                AgentTurnEvent::TurnStarted { model, .. } => {
                    if let Some(m) = model {
                        self.model = Some(m.clone());
                    }
                    self.busy = true;
                }
                AgentTurnEvent::AssistantTextDelta { delta, .. } => self.push_assistant(delta),
                AgentTurnEvent::ThinkingDelta { delta, .. } => self.push_thinking(delta),
                AgentTurnEvent::ToolCallStarted { call, .. } => {
                    let name = call.name.to_string();
                    // Edit tools render as diff cards; a malformed payload yields
                    // `None` and falls back to the plain output card.
                    let diff = diff::is_edit_tool(&name)
                        .then(|| diff::edit_tool_diff(&name, &call.args_json))
                        .flatten();
                    self.turns.push(Turn::Tool {
                        id: call.id.clone(),
                        name,
                        args: summarize_args(&call.args_json),
                        output: String::new(),
                        status: ToolStatus::Running,
                        diff,
                    });
                }
                AgentTurnEvent::ToolCallChunk { call_id, chunk, .. } => {
                    if let Some(Turn::Tool { output, .. }) = self.tool_by_id(call_id) {
                        output.push_str(chunk);
                    }
                }
                AgentTurnEvent::ToolCallFinished { call_id, result, .. } => {
                    let ok = result.ok;
                    if let Some(Turn::Tool { status, output, .. }) = self.tool_by_id(call_id) {
                        *status = if ok { ToolStatus::Ok } else { ToolStatus::Err };
                        if output.is_empty() {
                            *output = result.output.clone();
                        }
                    }
                }
                AgentTurnEvent::TurnFinished { .. } => self.busy = false,
                AgentTurnEvent::Extension {
                    extension, payload, ..
                } if extension == "advisor" => self.push_advisor(payload),
                _ => {}
            }
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        // Composer grows with its content (multi-line via ⌃J), capped at 5.
        let input_lines = (self.input.split('\n').count().max(1) as u16).min(5);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(input_lines + 1)])
            .split(area);

        // Command palette state for this frame (empty unless in `/`-mode, and
        // suppressed while the ⌃R search overlay owns the screen). Clamp the
        // selection cursor in case the match set shrank since last keypress.
        let menu = if self.search.is_some() {
            Vec::new()
        } else {
            self.slash_matches()
        };
        if !menu.is_empty() {
            self.menu_sel = self.menu_sel.min(menu.len() - 1);
        }

        // ── transcript panel in the CTRL skin ────────────────────────────────
        let pill = self.model.clone();
        let body = panel::draw(frame, chunks[0], "OCEAN", pill.as_deref(), self.focused);

        // Transcript lines (bottom-anchored via scroll offset). Split the
        // borrow: the markdown cache (`md`) is a distinct field from `turns`, so
        // the loop can read turns while `md.render` mutates its cache.
        let md = &mut self.md;
        let tools_expanded = self.tools_expanded;
        let mut lines: Vec<Line> = Vec::new();
        for turn in &self.turns {
            match turn {
                Turn::User(s) => {
                    lines.push(Line::from(vec![
                        Span::styled(g("❯ ", "> "), Style::default().fg(theme::CYAN)),
                        Span::styled(
                            s.clone(),
                            Style::default().fg(theme::CYAN).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
                Turn::Assistant(s) => {
                    // Streaming markdown with prefix-freeze: frozen head blocks
                    // are served from cache, only the growing tail re-renders.
                    lines.extend(md.render(s));
                }
                Turn::Permission {
                    tool,
                    reason,
                    resolved,
                    ..
                } => {
                    // Approval card: loud while pending, quiet once decided.
                    match resolved {
                        None => {
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("  {} approval needed: ", g("⚠", "!")),
                                    Style::default()
                                        .fg(theme::YELLOW)
                                        .add_modifier(Modifier::BOLD),
                                ),
                                Span::styled(
                                    tool.clone(),
                                    Style::default()
                                        .fg(theme::FG)
                                        .add_modifier(Modifier::BOLD),
                                ),
                            ]));
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("  {} ", g("▎", "|")),
                                    Style::default().fg(theme::YELLOW),
                                ),
                                Span::styled(reason.clone(), Style::default().fg(theme::FG)),
                            ]));
                            lines.push(Line::from(Span::styled(
                                "  ⌃Y allow · ⌃N deny",
                                Style::default().fg(theme::YELLOW),
                            )));
                        }
                        Some(true) => {
                            lines.push(Line::from(Span::styled(
                                format!("  {} allowed: {tool}", g("✓", "+")),
                                Style::default().fg(theme::GREEN),
                            )));
                        }
                        Some(false) => {
                            lines.push(Line::from(Span::styled(
                                format!("  {} denied: {tool}", g("✗", "x")),
                                Style::default().fg(theme::RED),
                            )));
                        }
                    }
                }
                Turn::Thinking(s) => {
                    lines.push(Line::from(Span::styled(
                        format!("  {} thinking ({} chars)", g("◌", "~"), s.len()),
                        Style::default()
                            .fg(theme::COMMENT)
                            .add_modifier(Modifier::ITALIC),
                    )));
                }
                Turn::Tool {
                    name,
                    args,
                    output,
                    status,
                    diff,
                    ..
                } => {
                    // Card header: status glyph + tool name + one-line args.
                    let (mark, color) = match status {
                        ToolStatus::Running => (g("◐", "*"), theme::YELLOW),
                        ToolStatus::Ok => (g("✓", "+"), theme::GREEN),
                        ToolStatus::Err => (g("✗", "x"), theme::RED),
                    };
                    let mut header = vec![
                        Span::styled(format!("  {mark} "), Style::default().fg(color)),
                        Span::styled(
                            name.clone(),
                            Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
                        ),
                    ];
                    if !args.is_empty() {
                        header.push(Span::styled(
                            format!("  {args}"),
                            Style::default().fg(theme::COMMENT),
                        ));
                    }
                    lines.push(Line::from(header));

                    match diff {
                        // Edit tools render a diff card: removed/added gutters in
                        // theme colours, word-level intra-line changes reversed,
                        // truncated to DIFF_TAIL_ROWS unless expanded (⌃O).
                        Some(rows) => {
                            let total = rows.len();
                            let shown = if tools_expanded {
                                total
                            } else {
                                total.min(DIFF_TAIL_ROWS)
                            };
                            for row in &rows[..shown] {
                                lines.push(diff_line(row));
                            }
                            if shown < total {
                                lines.push(Line::from(Span::styled(
                                    format!(
                                        "    {} +{} more · ⌃O expand",
                                        g("┄", ".."),
                                        total - shown
                                    ),
                                    Style::default().fg(theme::COMMENT),
                                )));
                            }
                        }
                        // Plain card: full output when expanded (⌃O), else a tail
                        // window with a "+N more" hint on the rail.
                        None => {
                            let body: Vec<&str> = output.lines().collect();
                            if !body.is_empty() {
                                let hidden = if tools_expanded {
                                    0
                                } else {
                                    body.len().saturating_sub(TOOL_TAIL_ROWS)
                                };
                                for l in &body[hidden..] {
                                    lines.push(Line::from(vec![
                                        Span::styled(
                                            format!("    {} ", g("│", "|")),
                                            Style::default().fg(theme::EDGE),
                                        ),
                                        Span::styled(
                                            l.to_string(),
                                            Style::default().fg(theme::COMMENT).bg(theme::BG_DARK),
                                        ),
                                    ]));
                                }
                                if hidden > 0 {
                                    lines.push(Line::from(Span::styled(
                                        format!("    {} +{hidden} more · ⌃O expand", g("┄", "..")),
                                        Style::default().fg(theme::COMMENT),
                                    )));
                                }
                            }
                        }
                    }
                }
                Turn::Advisor {
                    note,
                    severity,
                    model,
                } => {
                    // Severity → theme accent: blocker red, concern amber,
                    // info muted. Rendered as a set-off card with a │ gutter.
                    let accent = match severity.as_str() {
                        "blocker" => theme::RED,
                        "concern" => theme::YELLOW,
                        _ => theme::COMMENT,
                    };
                    let mut header: Vec<Span> = vec![Span::styled(
                        format!("  {} advisor ({severity})", g("⚑", "!")),
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    )];
                    if !model.is_empty() {
                        header.push(Span::styled(
                            format!("  · {model}"),
                            Style::default()
                                .fg(theme::COMMENT)
                                .add_modifier(Modifier::DIM),
                        ));
                    }
                    lines.push(Line::from(header));
                    for l in note.lines() {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {} ", g("▎", "|")),
                                Style::default().fg(accent),
                            ),
                            Span::styled(l.to_string(), Style::default().fg(theme::FG)),
                        ]));
                    }
                }
            }
            lines.push(Line::from(""));
        }
        // Bottom-anchor on the WRAPPED row count, not the raw line count — long
        // streamed lines reflow into multiple rows, and Paragraph's scroll
        // offset is in wrapped rows. Counting unwrapped lines made the live
        // tail jitter/scroll off as text arrived. `line_count` uses the exact
        // same wrap algorithm the render will.
        let para = Paragraph::new(lines)
            .style(Style::default().bg(theme::SLATE))
            .wrap(Wrap { trim: false });
        let wrapped = para.line_count(body.width) as u16;
        let max_back = wrapped.saturating_sub(body.height) as usize;
        self.scroll_back = self.scroll_back.min(max_back);
        let scroll = wrapped
            .saturating_sub(body.height)
            .saturating_sub(self.scroll_back as u16);
        frame.render_widget(para.scroll((scroll, 0)), body);
        let footer_hint = if self.search.is_some() {
            " history search · ⏎ insert · esc dismiss".to_string()
        } else if !menu.is_empty() {
            " command palette · esc to dismiss".to_string()
        } else if self.scroll_back > 0 {
            format!(" ↑{} lines back · PgDn to tail", self.scroll_back)
        } else if self.busy {
            " streaming…".to_string()
        } else {
            " ⏎ send · ⌃J newline · ⌃O tools · ⌃R history · / commands".to_string()
        };
        panel::footer(frame, chunks[0], &footer_hint);

        // ── composer: highlight bed, accent bar, multi-line, block cursor ────
        let comp = chunks[1];
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_HL)),
            comp,
        );
        for k in 0..comp.height {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    g("▎", "|"),
                    Style::default().fg(if self.busy { theme::COMMENT } else { theme::CYAN }),
                ))
                .style(Style::default().bg(theme::BG_HL)),
                Rect::new(comp.x, comp.y + k, 1, 1),
            );
        }
        let input_fg = if self.busy { theme::COMMENT } else { theme::FG };
        let mut input_render: Vec<Line> = self
            .input
            .lines()
            .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(input_fg))))
            .collect();
        if input_render.is_empty() || self.input.ends_with('\n') {
            input_render.push(Line::from(""));
        }
        // Block cursor rides the last line.
        if let Some(last) = input_render.last_mut() {
            last.spans
                .push(Span::styled(g("▏", "_"), Style::default().fg(theme::CYAN)));
        }
        frame.render_widget(
            Paragraph::new(input_render).style(Style::default().bg(theme::BG_HL)),
            Rect::new(
                comp.x + 2,
                comp.y,
                comp.width.saturating_sub(2),
                comp.height,
            ),
        );

        // ── `/` command palette overlay, floated just above the composer ─────
        if !menu.is_empty() {
            self.draw_menu(frame, comp, &menu);
        }
        // ── `@` file-mention picker, floated just above the composer ─────────
        let mentions = self.mention_matches();
        if !mentions.is_empty() {
            self.draw_mentions(frame, comp, &mentions);
        }
        // ── ⌃R history-search overlay, floated just above the composer ───────
        if self.search.is_some() {
            self.draw_search(frame, comp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A chat with the composer pre-filled — avoids the `field_reassign_with_default`
    /// clippy lint that fires on `let mut c = default(); c.input = …`.
    fn chat_with(input: &str) -> ChatComponent {
        ChatComponent {
            input: input.to_string(),
            ..Default::default()
        }
    }

    fn extension(extension: &str, payload: serde_json::Value) -> Action {
        Action::AgentEvent(Box::new(AgentTurnEvent::Extension {
            extension: extension.to_string(),
            payload,
            scope: None,
        }))
    }

    #[test]
    fn advisor_extension_appends_advisor_turn() {
        let mut chat = ChatComponent::default();
        chat.update(&extension(
            "advisor",
            json!({ "note": "consider a smaller diff", "severity": "concern", "model": "opus" }),
        ));
        assert_eq!(chat.turns.len(), 1);
        match &chat.turns[0] {
            Turn::Advisor {
                note,
                severity,
                model,
            } => {
                assert_eq!(note, "consider a smaller diff");
                assert_eq!(severity, "concern");
                assert_eq!(model, "opus");
            }
            _ => panic!("expected an advisor turn"),
        }
    }

    #[test]
    fn advisor_defaults_missing_fields() {
        let mut chat = ChatComponent::default();
        chat.update(&extension("advisor", json!({ "note": "heads up" })));
        assert_eq!(chat.turns.len(), 1);
        match &chat.turns[0] {
            Turn::Advisor {
                severity, model, ..
            } => {
                assert_eq!(severity, "info");
                assert_eq!(model, "");
            }
            _ => panic!("expected an advisor turn"),
        }
    }

    #[test]
    fn empty_note_is_skipped() {
        let mut chat = ChatComponent::default();
        chat.update(&extension("advisor", json!({ "note": "   ", "severity": "info" })));
        chat.update(&extension("advisor", json!({ "severity": "blocker" })));
        assert!(chat.turns.is_empty());
    }

    fn perm_envelope(
        pid: PermissionId,
        event: OceanEvent,
    ) -> Action {
        Action::OceanEvent(Box::new(ocean_core::EventEnvelope {
            id: ocean_core::EventId::new_v4(),
            at: chrono::Utc::now(),
            session_id: None,
            request_id: Some(ocean_core::RequestId::new_v4()),
            permission_id: Some(pid),
            origin: None,
            event,
        }))
    }

    #[test]
    fn permission_request_then_decision_resolves_card() {
        let mut chat = ChatComponent::default();
        let pid = PermissionId::new_v4();
        chat.update(&perm_envelope(
            pid,
            OceanEvent::PermissionRequest {
                tool: "bash".into(),
                reason: "rm -rf build".into(),
                args: json!({}),
            },
        ));
        assert_eq!(chat.pending_permission(), Some(pid), "card should be pending");
        // ⌃Y targets the pending card.
        let act = chat.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL));
        assert!(matches!(
            act,
            Some(Action::PermissionDecided { permission_id, allow: true }) if permission_id == pid
        ));
        // The daemon's decision event resolves it.
        chat.update(&perm_envelope(
            pid,
            OceanEvent::PermissionDecision {
                allowed: true,
                reason: None,
            },
        ));
        assert_eq!(chat.pending_permission(), None, "card should be resolved");
    }

    #[test]
    fn ctrl_o_toggles_tool_expansion() {
        let mut chat = ChatComponent::default();
        assert!(!chat.tools_expanded);
        chat.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(chat.tools_expanded);
        chat.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        assert!(!chat.tools_expanded);
    }

    #[test]
    fn summarize_args_is_single_line_and_prefers_primary_key() {
        let s = summarize_args(&json!({ "command": "echo hi\nrm x", "cwd": "/tmp" }));
        assert!(!s.contains('\n'), "args summary must be one line");
        assert!(s.contains("echo hi"));
        // No recognised key → compact-serialize the whole object.
        let s2 = summarize_args(&json!({ "foo": 1 }));
        assert!(s2.contains("foo"));
    }

    #[test]
    fn slash_prefix_opens_palette() {
        let chat = chat_with("/mod");
        assert!(ChatComponent::default().slash_matches().is_empty());
        let m = chat.slash_matches();
        assert_eq!(m.first().map(|c| c.name), Some("/model"));
        // A message that merely contains a slash mid-text is not command mode.
        assert!(chat_with("/model with args").slash_matches().is_empty());
    }

    #[test]
    fn enter_on_quit_command_emits_quit() {
        let mut chat = chat_with("/quit");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Some(Action::Quit)));
        assert!(chat.input.is_empty(), "composer clears after a command");
    }

    #[test]
    fn enter_on_clear_command_clears_transcript_and_does_not_submit() {
        let mut chat = chat_with("/clear");
        chat.turns.push(Turn::User("hello".into()));
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // No SubmitPrompt — the menu intercepts Enter.
        assert!(!matches!(act, Some(Action::SubmitPrompt(_))));
        assert!(chat.turns.is_empty(), "/clear empties the transcript");
    }

    #[test]
    fn help_command_pushes_transcript_block() {
        let mut chat = chat_with("/help");
        chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(chat.turns.len(), 1);
        assert!(matches!(&chat.turns[0], Turn::Assistant(s) if s.contains("/quit")));
    }

    #[test]
    fn arrows_move_palette_selection() {
        let mut chat = chat_with("/"); // all commands shown
        chat.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(chat.menu_sel, 1);
        chat.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(chat.menu_sel, 0);
    }

    #[test]
    fn esc_dismisses_palette() {
        let mut chat = chat_with("/quit");
        chat.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(chat.input.is_empty());
    }

    #[test]
    fn slash_new_clears_and_requests_new_session() {
        let mut chat = ChatComponent::default();
        chat.turns.push(Turn::User("hi".into()));
        let act = chat.run_slash("/new", "");
        assert!(chat.turns.is_empty(), "/new wipes the transcript");
        assert!(matches!(act, Some(Action::NewSession)));
    }

    #[test]
    fn slash_model_routes_arg_and_usage() {
        let mut chat = ChatComponent::default();
        match chat.run_slash("/model", "anthropic/claude-opus-4-8") {
            Some(Action::SetModel(id)) => assert_eq!(id, "anthropic/claude-opus-4-8"),
            other => panic!("expected SetModel, got {other:?}"),
        }
        // Bare `/model` is a usage hint, not a model change.
        assert!(matches!(chat.run_slash("/model", ""), Some(Action::Status(_))));
    }

    #[test]
    fn slash_copy_uses_last_reply() {
        let mut chat = ChatComponent::default();
        assert!(matches!(chat.run_slash("/copy", ""), Some(Action::Status(_)))); // nothing yet
        chat.turns.push(Turn::Assistant("the answer".into()));
        match chat.run_slash("/copy", "") {
            Some(Action::CopyToClipboard(t)) => assert_eq!(t, "the answer"),
            other => panic!("expected CopyToClipboard, got {other:?}"),
        }
    }

    #[test]
    fn soon_command_surfaces_honest_hint() {
        let mut chat = ChatComponent::default();
        match chat.run_slash("/compact", "") {
            Some(Action::Status(s)) => assert!(s.contains("not wired"), "got: {s}"),
            other => panic!("expected a Status hint, got {other:?}"),
        }
    }

    #[test]
    fn typed_command_line_routes_not_submits() {
        // A typed `/model <id>\n` line invokes the command instead of sending.
        let mut chat = chat_with("/model anthropic/claude-opus-4-8");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act, Some(Action::SetModel(_))));
        assert!(chat.turns.is_empty(), "command line must not become a User turn");
        // A `/`-path that is NOT a command still sends as a normal message.
        let mut chat2 = chat_with("/etc/hosts is a file");
        let act2 = chat2.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act2, Some(Action::SubmitPrompt(_))));
    }

    #[test]
    fn foreign_extension_is_ignored() {
        let mut chat = ChatComponent::default();
        chat.update(&extension("longhouse", json!({ "note": "not for us" })));
        chat.update(&extension("advisor", json!({ "note": 42 })));
        assert!(chat.turns.is_empty());
    }

    // ── composer history + kill ring (W2 slice) ───────────────────────────────

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    /// A chat seeded with an in-memory prompt history (no disk I/O: the default
    /// `PromptHistory` has no backing file, so `push` never persists).
    fn chat_with_hist(entries: &[&str]) -> ChatComponent {
        let mut chat = ChatComponent::default();
        for e in entries {
            chat.history.push(e);
        }
        chat
    }

    #[test]
    fn up_down_walks_history_when_composer_empty() {
        let mut chat = chat_with_hist(&["first", "second"]);
        chat.handle_key(key(KeyCode::Up)); // → newest
        assert_eq!(chat.input, "second");
        chat.handle_key(key(KeyCode::Up)); // → older
        assert_eq!(chat.input, "first");
        chat.handle_key(key(KeyCode::Up)); // clamps at oldest
        assert_eq!(chat.input, "first");
        chat.handle_key(key(KeyCode::Down)); // → newer
        assert_eq!(chat.input, "second");
        chat.handle_key(key(KeyCode::Down)); // past newest → restore draft (empty)
        assert_eq!(chat.input, "");
        assert!(chat.history_idx.is_none());
    }

    #[test]
    fn up_down_do_not_hijack_a_draft() {
        let mut chat = chat_with_hist(&["recall me"]);
        chat.input = "in-progress draft".into();
        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "in-progress draft", "↑ must not clobber a draft");
        chat.handle_key(key(KeyCode::Down));
        assert_eq!(chat.input, "in-progress draft", "↓ no-ops when not navigating");
    }

    #[test]
    fn history_dedupes_and_persists_on_submit() {
        let mut chat = chat_with("build it");
        chat.handle_key(key(KeyCode::Enter));
        chat.input = "build it".into(); // consecutive repeat
        chat.handle_key(key(KeyCode::Enter));
        assert_eq!(chat.history.len(), 1, "consecutive repeat is deduped");
        // ↑ recalls the submitted prompt.
        chat.handle_key(key(KeyCode::Up));
        assert_eq!(chat.input, "build it");
    }

    #[test]
    fn ctrl_r_opens_fuzzy_history_search_reusing_slash_scorer() {
        let mut chat = chat_with_hist(&["cargo build", "git status", "cargo test"]);
        chat.handle_key(ctrl('r'));
        assert!(chat.search.is_some(), "⌃R opens the search overlay");
        for c in "crg".chars() {
            chat.handle_key(key(KeyCode::Char(c)));
        }
        // Fuzzy subsequence "crg" hits both cargo entries, newest first; the
        // ranking reuses slash::subseq_score.
        let texts: Vec<&str> = chat
            .search_matches()
            .iter()
            .map(|&i| chat.history.get(i).unwrap())
            .collect();
        assert_eq!(texts, vec!["cargo test", "cargo build"]);
        // Enter inserts the selected (top) match into the composer and closes.
        chat.handle_key(key(KeyCode::Enter));
        assert_eq!(chat.input, "cargo test");
        assert!(chat.search.is_none());
    }

    #[test]
    fn ctrl_r_search_esc_dismisses_without_touching_composer() {
        let mut chat = chat_with_hist(&["something"]);
        chat.input = "keep me".into();
        chat.handle_key(ctrl('r'));
        chat.handle_key(key(KeyCode::Esc));
        assert!(chat.search.is_none());
        assert_eq!(chat.input, "keep me");
    }

    #[test]
    fn kill_to_start_then_yank_roundtrips() {
        let mut chat = chat_with("hello world");
        chat.handle_key(ctrl('u'));
        assert_eq!(chat.input, "");
        assert_eq!(chat.kill_ring.last().map(String::as_str), Some("hello world"));
        chat.handle_key(ctrl('y'));
        assert_eq!(chat.input, "hello world", "⌃Y yanks the newest kill at the end");
    }

    #[test]
    fn kill_to_end_trims_the_last_line() {
        let mut chat = chat_with("line one\nline two");
        chat.handle_key(ctrl('k'));
        assert_eq!(chat.input, "line one\n");
        assert_eq!(chat.kill_ring.last().map(String::as_str), Some("line two"));
    }

    #[test]
    fn kill_ring_is_capped() {
        let mut chat = chat_with("");
        for i in 0..(KILL_RING_CAP + 3) {
            chat.input = format!("kill {i}");
            chat.handle_key(ctrl('u'));
        }
        assert_eq!(chat.kill_ring.len(), KILL_RING_CAP);
        // Newest survives; oldest was dropped from the front.
        assert_eq!(
            chat.kill_ring.last().map(String::as_str),
            Some(&format!("kill {}", KILL_RING_CAP + 2)[..])
        );
    }

    #[test]
    fn ctrl_y_precedence_permission_beats_yank() {
        let mut chat = chat_with("yankable");
        chat.handle_key(ctrl('u')); // ring = ["yankable"], composer empty
        let pid = PermissionId::new_v4();
        chat.update(&perm_envelope(
            pid,
            OceanEvent::PermissionRequest {
                tool: "bash".into(),
                reason: "rm -rf x".into(),
                args: json!({}),
            },
        ));
        // Permission pending → ⌃Y allows, does NOT yank.
        let act = chat.handle_key(ctrl('y'));
        assert!(matches!(
            act,
            Some(Action::PermissionDecided { permission_id, allow: true }) if permission_id == pid
        ));
        assert_eq!(chat.input, "", "yank must not fire while a permission is pending");
        // Resolve the permission; now ⌃Y yanks.
        chat.update(&perm_envelope(
            pid,
            OceanEvent::PermissionDecision {
                allowed: true,
                reason: None,
            },
        ));
        chat.handle_key(ctrl('y'));
        assert_eq!(chat.input, "yankable");
    }
}
