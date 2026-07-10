//! ChatComponent — the native agent surface. Re-houses the PM room's streaming
//! model (structured blocks: text, thinking, tool calls) onto the component
//! architecture, plus: permission approval cards (⌃Y allow / ⌃N deny, the
//! OCEAN-185 gated flow), streaming markdown with prefix-freeze (via
//! `shell::markdown` — headings, syntax-highlighted fences, lists, blockquotes,
//! inline `code`/**bold**/*italic*), tool cards with ⌃O collapse/expand,
//! multi-line input (⌃J newline), and wheel/PageUp scrollback.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ocean_agent_sdk::{AgentTurnEvent, ThinkingLevel, ToolCallId};
use ocean_core::{OceanEvent, PermissionId};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use unicode_width::UnicodeWidthStr;

use crate::shell::{
    action::{Action, LoginTarget, Nav},
    component::Component,
    diff::{self, DiffKind, DiffRow},
    errfmt,
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

/// Collapsed mode shows at most this many ok/running tool one-liners per
/// consecutive tool run; older ones compact into one "· N earlier tools" line.
/// Without this a 30-tool turn floods the whole transcript with cards even in
/// the minimized mode. Errors are never hidden; ⌃O expands everything.
const BURST_TAIL_TOOLS: usize = 3;

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
    /// A terminal-level notice about a turn that failed or was cancelled.
    /// Rendered as a plain notice line ("✗ turn failed — …"), NOT as an advisor
    /// card — no severity label, no model attribution.
    ErrorNotice { note: String },
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
    /// Byte-index cursor position in the composer. `None` means trailing
    /// (at `input.len()`), which is also the Default so `chat_with` and
    /// direct `input = …` writes all self-seat at the end.
    cursor: Option<usize>,
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
    /// Optional provider-status line shown in the welcome empty-state only.
    /// Set by the app after construction.
    pub welcome_provider_line: Option<String>,
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

/// Make a line of tool/diff output terminal-safe. ratatui does NOT expand
/// tabs: a raw `\t` makes the terminal jump to its own tab stop, every cell
/// after it paints misaligned, and ratatui's diffing (which believes its own
/// cell math) leaves smeared "bleed" that never clears. Other control chars
/// (ESC sequences, `\r`) can recolor or move the cursor under ratatui's feet
/// the same way. Tabs become 4 spaces; other control chars drop.
pub(crate) fn sanitize_line(s: &str) -> String {
    if !s.chars().any(|c| c.is_control()) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
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
        spans.push(Span::styled(sanitize_line(&seg.text), style));
    }
    Line::from(spans)
}

/// Flatten whitespace to single spaces and truncate to `max` chars with an
/// ellipsis.
fn one_line(s: &str, max: usize) -> String {
    // Flatten whitespace, then drop any remaining control bytes: this string
    // lands in a ratatui `Span`, and one raw ESC/CSI sequence repaints the
    // terminal underneath ratatui's cell math (the collapsed-card screen
    // smear). `split_whitespace` already eats tabs/newlines/CRs; ESC is NOT
    // whitespace and would sail through without the filter.
    let flat: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    if flat.chars().count() > max {
        let head: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{head}{}", g("…", "..."))
    } else {
        flat
    }
}

/// Truncate a single line to `max` chars with an ellipsis, preserving internal
/// whitespace (unlike [`one_line`], which flattens it — wrong for tool-output
/// previews where indentation is signal). Used by collapsed tool cards so each
/// tail line costs exactly one screen row: without this, a tool returning one
/// giant single-line blob (JSON from recall/lsp/MCP results) wraps into
/// hundreds of rows and defeats the tail window entirely.
fn clamp_line(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}{}", g("…", "..."))
    } else {
        s.to_string()
    }
}

/// Normalize a bracketed paste for the composer: CRLF/CR become plain
/// newlines (kept — the composer is multi-line via ⌃J), tabs become four
/// spaces, and every other control byte drops (an ESC sequence must never
/// reach a `Span`). Pasted newlines are CONTENT, never synthetic Enter
/// presses — the pre-bracketed-paste terminal replayed them as real key
/// events and auto-submitted mid-paste.
pub(crate) fn paste_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
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
        self.cursor = None;
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
            self.cursor = None;
        } else {
            self.input = std::mem::take(&mut self.draft);
            self.cursor = None;
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

    /// ⌃U — kill from the start of the current line to the cursor onto the ring.
    fn kill_to_start(&mut self) {
        if self.input.is_empty() {
            return;
        }
        let c = self.cursor_byte();
        let line_start = self.input[..c].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if line_start == c {
            return;
        }
        let killed = self.input[line_start..c].to_string();
        self.input.drain(line_start..c);
        self.push_kill(killed);
        self.cursor = (line_start < self.input.len()).then_some(line_start);
        self.reset_history_nav();
        self.menu_sel = 0;
    }

    /// ⌃K — kill from the cursor to the end of the current line. At end of
    /// buffer → no-op. At a non-final line end (cursor sits on `\n`) the
    /// newline is killed, joining the lines. Mid-line kills through end of
    /// line but preserves the newline.
    fn kill_to_end(&mut self) {
        if self.input.is_empty() {
            return;
        }
        let c = self.cursor_byte();
        if c == self.input.len() {
            return;
        } // at buffer end → no-op
        let line_end = self.input[c..]
            .find('\n')
            .map(|i| c + i)
            .unwrap_or(self.input.len());
        if c == line_end {
            // Cursor sits on a newline (non-final line). Kill the newline to join lines.
            let end = c + 1; // skip the '\n'
            let killed = self.input[c..end].to_string();
            self.input.drain(c..end);
            self.push_kill(killed);
        } else {
            let killed = self.input[c..line_end].to_string();
            self.input.drain(c..line_end);
            self.push_kill(killed);
        }
        if self.cursor.map_or(false, |cur| cur >= self.input.len()) {
            self.cursor = None;
        }
        self.reset_history_nav();
        self.menu_sel = 0;
    }

    /// ⌃Y — yank the newest kill at the cursor.
    fn yank(&mut self) {
        if let Some(kill) = self.kill_ring.last().cloned() {
            let c = self.cursor_byte();
            self.input.insert_str(c, &kill);
            let new_cursor = c + kill.len();
            self.cursor = if new_cursor == self.input.len() {
                None
            } else {
                Some(new_cursor)
            };
            self.reset_history_nav();
            self.menu_sel = 0;
        }
    }

    // ── cursor movement / helpers ──────────────────────────────────────────

    /// Effective byte offset of the cursor. `None` → input end.
    fn cursor_byte(&self) -> usize {
        self.cursor
            .map_or(self.input.len(), |c| c.min(self.input.len()))
    }

    /// Move the cursor one Unicode scalar to the left.
    fn cursor_left(&mut self) {
        let c = self.cursor_byte();
        if c > 0 {
            let mut prev = c - 1;
            while prev > 0 && !self.input.is_char_boundary(prev) {
                prev -= 1;
            }
            self.cursor = Some(prev);
        }
    }

    /// Move the cursor one Unicode scalar to the right.
    fn cursor_right(&mut self) {
        let c = self.cursor_byte();
        if c < self.input.len() {
            let mut next = c + 1;
            while next < self.input.len() && !self.input.is_char_boundary(next) {
                next += 1;
            }
            if next == self.input.len() {
                self.cursor = None;
            } else {
                self.cursor = Some(next);
            }
        }
    }

    /// Move the cursor to the start of the current line.
    fn cursor_line_start(&mut self) {
        let c = self.cursor_byte();
        let line_start = self.input[..c].rfind('\n').map(|i| i + 1).unwrap_or(0);
        if line_start != c {
            self.cursor = Some(line_start);
        }
    }

    /// Move the cursor to the end of the current line.
    fn cursor_line_end(&mut self) {
        let c = self.cursor_byte();
        let line_end = self.input[c..]
            .find('\n')
            .map(|i| c + i)
            .unwrap_or(self.input.len());
        if line_end == self.input.len() {
            self.cursor = None;
        } else {
            self.cursor = Some(line_end);
        }
    }

    /// Insert a `&str` at the cursor and advance past it. Leaves cursor at
    /// `None` when the insertion reaches the end.
    fn insert_at_cursor(&mut self, s: &str) {
        let c = self.cursor_byte();
        self.input.insert_str(c, s);
        let new_cursor = c + s.len();
        self.cursor = if new_cursor == self.input.len() {
            None
        } else {
            Some(new_cursor)
        };
    }

    // ── word-kill helper (⌃W) ──────────────────────────────────────────────

    /// Kill the word before the cursor onto the ring. Two-phase: first consume
    /// the trailing whitespace run, then the preceding non-whitespace word.
    /// Each phase uses its own reversed iterator so no char is lost at the
    /// phase boundary. Both phases step by whole UTF-8 scalars. The killed
    /// region includes trailing whitespace so yank reconstructs cleanly.
    fn kill_word_backward(&mut self) {
        let c = self.cursor_byte();
        if c == 0 {
            return;
        }
        // Phase 1: consume trailing whitespace run.
        let mut start = c;
        for ch in self.input[..c].chars().rev() {
            if ch.is_whitespace() {
                start -= ch.len_utf8();
            } else {
                break;
            }
        }
        // Phase 2: consume the preceding non-whitespace word.
        // Fresh iterator on the remaining prefix — no shared state with phase 1.
        for ch in self.input[..start].chars().rev() {
            if !ch.is_whitespace() {
                start -= ch.len_utf8();
            } else {
                break;
            }
        }
        if start == c {
            return;
        }
        let killed = self.input[start..c].to_string();
        self.input.drain(start..c);
        self.push_kill(killed);
        self.cursor = (start < self.input.len()).then_some(start);
        self.reset_history_nav();
        self.menu_sel = 0;
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
                        self.cursor = None;
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

    /// Whether Tab should route to the composer instead of cycling focus — true
    /// when the `/` palette or `@` mention picker is open (both handle Tab for
    /// completion, but app.rs swallows it for focus-cycling first).
    pub fn wants_tab(&self) -> bool {
        // `/` palette: input starts with `/`, no whitespace in query.
        if let Some(q) = self.input.strip_prefix('/') {
            if !q.contains(char::is_whitespace) {
                return true;
            }
        }
        // `@` mention picker: the cursor-relative token starts with `@`.
        self.mention_query().is_some()
    }

    /// Whether a turn is currently streaming (submit → TurnFinished). The app's
    /// render loop animates at tick rate only while this (or a live PTY) is
    /// true, instead of redrawing 60Hz forever.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// The model driving turns (the header pill), for the status bar. `None`
    /// until the first `TurnStarted` names it.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Newest image path referenced anywhere in the transcript (user or
    /// assistant text), for the bare `/image` viewer. Scans turns newest-first,
    /// then lines within a turn newest-first. Uses the same `![](path)` parse
    /// as the markdown card so what shows as an image is what `/image` opens.
    pub fn latest_image(&self) -> Option<String> {
        self.turns.iter().rev().find_map(|t| {
            let text = match t {
                Turn::Assistant(s) | Turn::User(s) => s.as_str(),
                _ => return None,
            };
            text.lines().rev().find_map(|l| {
                crate::shell::markdown::parse_image_ref(l.trim_start()).map(|(_, p)| p)
            })
        })
    }

    /// Whether tool/diff cards render fully expanded (the ⌃O toggle). Exposed
    /// for the settings overlay.
    pub fn tools_expanded(&self) -> bool {
        self.tools_expanded
    }

    /// Flip the ⌃O expand toggle from outside (settings overlay row).
    pub fn toggle_tools_expanded(&mut self) {
        self.tools_expanded = !self.tools_expanded;
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
    /// Token = whitespace-delimited word containing the character immediately
    /// left of the cursor (`cursor_byte()`). Cursor at 0 closes the picker
    /// (nothing left of the caret to match). Returns `(at, end, query)` where
    /// `at..end` is the byte range of the `@` token (exclusive end) and
    /// `query` is the text after the `@` sigil.
    fn mention_query(&self) -> Option<(usize, usize, &str)> {
        let cursor = self.cursor_byte();
        if cursor == 0 {
            return None;
        }
        // Walk back from just left of cursor to find the last whitespace,
        // then skip past it — that's where this token starts.
        let start = self.input[..cursor]
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(i, ch)| i + ch.len_utf8())
            .unwrap_or(0);
        // Walk forward from cursor to find the next whitespace — that's
        // where this token ends.
        let end = self.input[cursor..]
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(i, _)| cursor + i)
            .unwrap_or(self.input.len());
        let token = &self.input[start..end];
        token.starts_with('@').then(|| (start, end, &token[1..]))
    }

    /// Ranked file matches for the current `@` query (empty when not in mention
    /// mode). Scans the project lazily on first use.
    fn mention_matches(&mut self) -> Vec<String> {
        let Some((_, _, query)) = self.mention_query() else {
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

    /// Replace the active `@token` with the picked path, preserving any text
    /// and whitespace that follows it. Cursor lands after the suffix's leading
    /// whitespace so the next keystroke inserts before its content; when the
    /// completion ends the buffer, one trailing space is appended and cursor
    /// normalizes to `None`.
    fn insert_mention(&mut self, path: &str) {
        if let Some((at, end, _)) = self.mention_query() {
            let suffix = self.input[end..].to_string();
            self.input.truncate(at);
            self.input.push('@');
            self.input.push_str(path);
            if suffix.is_empty() {
                self.input.push(' ');
                self.cursor = None;
            } else {
                let leading_whitespace = suffix
                    .chars()
                    .take_while(|ch| ch.is_whitespace())
                    .map(char::len_utf8)
                    .sum::<usize>();
                let cursor_pos = self.input.len() + leading_whitespace;
                self.input.push_str(&suffix);
                self.cursor = (cursor_pos < self.input.len()).then_some(cursor_pos);
            }
            self.mention_sel = 0;
        }
    }

    fn login_target(args: &str) -> Result<LoginTarget, String> {
        let target = args.trim();
        if target.is_empty() {
            return Ok(LoginTarget::Claude);
        }
        match target.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" | "anthropic" => Ok(LoginTarget::Claude),
            "codex" | "openai-codex" | "chatgpt" | "openai" => Ok(LoginTarget::Codex),
            _ => Err("usage: /login [claude|codex]".into()),
        }
    }

    fn parse_thinking(args: &str) -> Result<Option<ThinkingLevel>, String> {
        let level = args.trim();
        if level.is_empty() {
            return Err("usage: /thinking default|off|minimal|low|medium|high|xhigh".into());
        }
        match level.to_ascii_lowercase().as_str() {
            "default" | "auto" | "daemon" => Ok(None),
            "off" | "none" => Ok(Some(ThinkingLevel::Off)),
            "minimal" | "min" => Ok(Some(ThinkingLevel::Minimal)),
            "low" => Ok(Some(ThinkingLevel::Low)),
            "medium" | "med" => Ok(Some(ThinkingLevel::Medium)),
            "high" => Ok(Some(ThinkingLevel::High)),
            "xhigh" | "x-high" | "extra-high" => Ok(Some(ThinkingLevel::Xhigh)),
            _ => Err("usage: /thinking default|off|minimal|low|medium|high|xhigh".into()),
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
        self.cursor = None;
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
                    // Bare `/model` opens the picker — nobody memorizes ids.
                    Some(Action::OpenModels)
                } else {
                    Some(Action::SetModel(args.to_string()))
                }
            }
            "/thinking" => match Self::parse_thinking(args) {
                Ok(level) => Some(Action::SetThinking(level)),
                Err(usage) => Some(Action::Status(usage)),
            },
            "/models" => Some(Action::OpenModels),
            "/advisor" => Some(Action::OpenAdvisor),
            "/memory" => Some(Action::OpenMemory),
            "/lsp" => Some(Action::OpenLsp),
            "/image" => {
                let path = if args.trim().is_empty() {
                    self.latest_image()
                } else {
                    Some(args.trim().to_string())
                };
                match path {
                    Some(p) => Some(Action::ViewImage(p)),
                    None => Some(Action::Status(
                        "no image in this chat yet (agents show one with ![](path))".into(),
                    )),
                }
            }
            "/login" => {
                // Bare `/login` opens the provider popup (OAuth + API keys);
                // `/login claude|codex` keeps the direct browser flow.
                if args.trim().is_empty() {
                    Some(Action::OpenProviders)
                } else {
                    match Self::login_target(args) {
                        Ok(target) => Some(Action::Login(target)),
                        Err(usage) => Some(Action::Status(usage)),
                    }
                }
            }
            "/providers" => Some(Action::OpenProviders),
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
            "/settings" => Some(Action::OpenSettings),
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
    /// markdown-lite renderer styles the headings, bullets, and inline `code`.
    /// Sections follow the registry's breadcrumb groups.
    fn push_help(&mut self) {
        let mut body = String::from("# commands\n");
        let mut last_group = "";
        for c in slash::COMMANDS {
            if c.group != last_group {
                body.push_str(&format!("\n## {}\n", c.group));
                last_group = c.group;
            }
            body.push_str(&format!("- `{}` — {}\n", c.name, c.desc));
        }
        body.push_str("\n# keys\n");
        body.push_str("• ⏎ — send\n");
        body.push_str("• ⌃J — newline in composer\n");
        body.push_str("• ⌃O — toggle tool-card expansion\n");
        body.push_str("• ⌃R — fuzzy history search\n");
        body.push_str("• ⌃U — kill composer line\n");
        body.push_str("• ⌃K — kill to end of line\n");
        body.push_str("• ⌃Y — yank / allow permission\n");
        body.push_str("• ⌃N — deny permission\n");
        body.push_str("• PgUp — scroll transcript up\n");
        body.push_str("• PgDn — scroll transcript down\n");
        body.push_str("• ↑ — history prev (when composer empty)\n");
        body.push_str("• ↓ — history next (when composer empty)\n");
        body.push_str("• / — command palette\n");
        body.push_str("• ↑↓ — palette select\n");
        body.push_str("• ⏎ — palette run\n");
        body.push_str("• ⇥ — palette complete\n");
        body.push_str("• esc — palette dismiss\n");
        body.push_str("• ⌃Q — quit\n");
        body.push_str("• Tab — cycle focus (except in palette)\n");
        body.push_str("• ⌃⌥1 — sessions pane\n");
        body.push_str("• ⌃⌥2 — files pane\n");
        body.push_str("• ⌃⌥3 — chat pane\n");
        body.push_str("• ⌃⌥4 — editor pane\n");
        body.push_str("• ⌃⌥5 — graph pane\n");
        body.push_str("• ⌃⌥6 — terminal pane\n");
        body.push_str("• ⌃⌥↑↓ — resize terminal dock\n");
        body.push_str("• esc — back to chat (from other panes)\n");
        self.turns.push(Turn::Assistant(body));
    }

    /// Render the floating command palette just above the composer. On the bare
    /// `/` the registry renders as breadcrumbed group SECTIONS (session /
    /// workspace / chat / roadmap groups) with muted headers, like the file
    /// tree's grouping; while filtering, rows are flat ranked matches carrying a
    /// muted `group ›` breadcrumb so you still see where a command lives. The
    /// selection rides on a `BG_HL` bed. Overlaid last so it sits on top of the
    /// transcript.
    fn draw_menu(&self, frame: &mut Frame, composer: Rect, matches: &[&slash::SlashCommand]) {
        if matches.is_empty() {
            return;
        }
        // Grouped mode on the bare `/` (matches are in registry order there).
        let grouped = self.input == "/";

        // Build the visible row list: command rows indexed into `matches`, plus
        // (in grouped mode) a header row at each group boundary.
        enum Row<'a> {
            Header(&'static str),
            Cmd(usize, &'a slash::SlashCommand),
        }
        let mut rows: Vec<Row> = Vec::new();
        let mut last_group = "";
        for (i, c) in matches.iter().enumerate() {
            if grouped && c.group != last_group {
                rows.push(Row::Header(c.group));
                last_group = c.group;
            }
            rows.push(Row::Cmd(i, c));
        }

        let cap = (composer.y as usize).saturating_sub(3).clamp(1, 26);
        let shown = rows.len().min(cap);
        let sel = self.menu_sel.min(matches.len() - 1);

        // Width fits the widest ACTUAL row, capped to the composer width. A
        // command row renders as ` {marker} ` (3) + breadcrumb (filtered mode)
        // + `{:<11}` name + ` — ` (3) + desc + the soon badge; mirror that
        // exactly or long descriptions clip mid-word.
        const BADGE_W: usize = 7; // " · soon"
        let crumb_w = |c: &slash::SlashCommand| {
            if grouped {
                0
            } else {
                c.group.chars().count() + 3 // "group › "
            }
        };
        let content_w = rows
            .iter()
            .take(shown)
            .map(|r| match r {
                Row::Header(gname) => 2 + gname.chars().count(),
                Row::Cmd(_, c) => {
                    3 + crumb_w(c)
                        + c.name.chars().count().max(11)
                        + 3
                        + c.desc.chars().count()
                        + if c.soon { BADGE_W } else { 0 }
                }
            })
            .max()
            .unwrap_or(24);
        let width = ((content_w as u16) + 2/* borders */)
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
                Style::default()
                    .fg(theme::BLUE)
                    .add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Scroll the row window to keep the SELECTED row visible — with the
        // registry grown past the overlay cap, the cursor used to walk off the
        // bottom (or the top groups were simply unreachable) while the window
        // stayed pinned to the first rows.
        let sel_row = rows
            .iter()
            .position(|r| matches!(r, Row::Cmd(i, _) if *i == sel))
            .unwrap_or(0);
        let scroll = sel_row
            .saturating_sub(shown.saturating_sub(1))
            .min(rows.len().saturating_sub(shown));

        for (row_i, row) in rows.iter().enumerate().skip(scroll).take(shown) {
            let row_y = (row_i - scroll) as u16;
            match row {
                Row::Header(gname) => {
                    // Muted section header, file-tree style: `▾ group`.
                    frame.render_widget(
                        Paragraph::new(Line::from(vec![
                            Span::styled(
                                format!(" {} ", g("▾", "v")),
                                Style::default().fg(theme::DEEPBLUE),
                            ),
                            Span::styled(
                                gname.to_string(),
                                Style::default()
                                    .fg(theme::BLUE)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]))
                        .style(Style::default().bg(theme::SLATE)),
                        Rect::new(inner.x, inner.y + row_y, inner.width, 1),
                    );
                }
                Row::Cmd(i, c) => {
                    let selected = *i == sel;
                    let bed = if selected { theme::BG_HL } else { theme::SLATE };
                    // Live rows read bold in FG/CYAN; roadmap (soon) rows are
                    // muted so the working commands stay visually dominant.
                    let (name_fg, name_mod) = match (selected, c.soon) {
                        (true, false) => (theme::CYAN, Modifier::BOLD),
                        (true, true) => (theme::YELLOW, Modifier::empty()),
                        (false, false) => (theme::FG, Modifier::BOLD),
                        (false, true) => (theme::COMMENT, Modifier::empty()),
                    };
                    let marker = if selected { g("❯", ">") } else { " " };
                    let mut spans = vec![Span::styled(
                        format!(" {marker} "),
                        Style::default().fg(theme::CYAN),
                    )];
                    if !grouped {
                        // Filtered rows carry their breadcrumb: `group › /name`.
                        spans.push(Span::styled(
                            format!("{} {} ", c.group, g("›", ">")),
                            Style::default().fg(theme::DEEPBLUE),
                        ));
                    }
                    spans.push(Span::styled(
                        format!("{:<11}", c.name),
                        Style::default().fg(name_fg).add_modifier(name_mod),
                    ));
                    spans.push(Span::styled(
                        format!(" {} {}", g("—", "-"), c.desc),
                        Style::default().fg(theme::COMMENT),
                    ));
                    if c.soon {
                        let left_w = 3
                            + crumb_w(c)
                            + c.name.chars().count().max(11)
                            + 3
                            + c.desc.chars().count();
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
                        Rect::new(inner.x, inner.y + row_y, inner.width, 1),
                    );
                }
            }
        }
        // Footer hint on the last inner row; note the truncation when the list
        // is longer than what fits.
        let shown_cmds = rows
            .iter()
            .take(shown)
            .filter(|r| matches!(r, Row::Cmd(..)))
            .count();
        let more = if shown_cmds < matches.len() {
            format!(" · {shown_cmds}/{}", matches.len())
        } else {
            String::new()
        };
        frame.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    " ⇥ complete · {} select · ⏎ run · esc dismiss{more}",
                    g("↑↓", "^v")
                ),
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
                Style::default()
                    .fg(theme::BLUE)
                    .add_modifier(Modifier::BOLD),
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
                Style::default()
                    .fg(theme::BLUE)
                    .add_modifier(Modifier::BOLD),
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
    /// Bracketed paste into the chat surface. While the ⌃R search overlay is
    /// open the paste feeds its single-line query; otherwise it inserts into
    /// the composer verbatim (newlines included) and NEVER submits.
    fn handle_paste(&mut self, text: &str) -> Option<Action> {
        if let Some(s) = self.search.as_mut() {
            s.query.extend(text.chars().filter(|c| !c.is_control()));
            s.sel = 0;
            return None;
        }
        let clean = paste_text(text);
        if clean.is_empty() {
            return None;
        }
        self.insert_at_cursor(&clean);
        self.reset_history_nav();
        self.menu_sel = 0;
        self.mention_sel = 0;
        None
    }

    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        // The ⌃R history-search overlay is modal: while open, all keys drive it.
        if self.search.is_some() {
            return self.handle_search_key(key);
        }
        // Permission decisions work even mid-stream (legacy ⌃Y/⌃N bindings).
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('y') => {
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
                // ⌃J: newline in the composer at the cursor.
                KeyCode::Char('j') => {
                    self.insert_at_cursor("\n");
                    return None;
                }
                // ⌃O: toggle tool-card expansion.
                KeyCode::Char('o') => {
                    self.tools_expanded = !self.tools_expanded;
                    return None;
                }
                // ⌃R: open the fuzzy prompt-history search overlay.
                KeyCode::Char('r') => {
                    self.search = Some(HistorySearch::default());
                    return None;
                }
                // ⌃U: kill from line start to cursor.
                KeyCode::Char('u') => {
                    self.kill_to_start();
                    return None;
                }
                // ⌃K: kill from cursor to line end.
                KeyCode::Char('k') => {
                    self.kill_to_end();
                    return None;
                }
                // ⌃W: kill word before cursor.
                KeyCode::Char('w') => {
                    self.kill_word_backward();
                    return None;
                }
                // ⌃B / ⌃F: cursor left / right.
                KeyCode::Char('b') => {
                    self.cursor_left();
                    return None;
                }
                KeyCode::Char('f') => {
                    self.cursor_right();
                    return None;
                }
                // ⌃A / ⌃E: line start / line end.
                KeyCode::Char('a') => {
                    self.cursor_line_start();
                    return None;
                }
                KeyCode::Char('e') => {
                    self.cursor_line_end();
                    return None;
                }
                // ⌃D: delete the char at cursor (or end-of-input no-op).
                KeyCode::Char('d') => {
                    let c = self.cursor_byte();
                    if c < self.input.len() {
                        let ch = self.input[c..].chars().next().unwrap();
                        let end = c + ch.len_utf8();
                        self.input.drain(c..end);
                        if self.cursor.map_or(false, |cur| cur >= self.input.len()) {
                            self.cursor = None;
                        }
                        self.reset_history_nav();
                        self.menu_sel = 0;
                        self.mention_sel = 0;
                    }
                    return None;
                }
                // ⌃L: clear transcript when idle, no-op while busy. Preserves
                //      composer text and cursor position.
                KeyCode::Char('l') => {
                    if !self.busy {
                        self.turns.clear();
                        self.md.clear();
                        self.scroll_back = 0;
                    }
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
                    self.cursor = None;
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
                    self.cursor = None;
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
                    if let Some((at, _, _)) = self.mention_query() {
                        self.input.remove(at); // drop the `@`, keep the text
                                               // Shift cursor left if it was past the removed sigil.
                        if let Some(c) = self.cursor {
                            if c > at {
                                self.cursor = Some(c - 1);
                            }
                        }
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
            // Left / Right: cursor movement.
            (KeyCode::Left, _) => {
                self.cursor_left();
                self.history_idx = None;
                self.menu_sel = 0;
                self.mention_sel = 0;
                None
            }
            (KeyCode::Right, _) => {
                self.cursor_right();
                self.history_idx = None;
                self.menu_sel = 0;
                self.mention_sel = 0;
                None
            }
            // Home / End: line boundaries.
            (KeyCode::Home, _) => {
                self.cursor_line_start();
                None
            }
            (KeyCode::End, _) => {
                self.cursor_line_end();
                None
            }
            (KeyCode::Enter, _) => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                if text.starts_with('/') {
                    let (name, args) = match text.split_once(char::is_whitespace) {
                        Some((n, a)) => (n, a),
                        None => (text.as_str(), ""),
                    };
                    if slash::is_command(name) {
                        return self.run_slash(name, args);
                    }
                    let looks_like_cmd = name
                        .strip_prefix('/')
                        .map(|n| {
                            !n.is_empty()
                                && n.chars().all(|c| {
                                    c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
                                })
                        })
                        .unwrap_or(false);
                    if looks_like_cmd {
                        let hint = if let Some(nearest) = slash::nearest(name) {
                            format!("unknown command {name} — did you mean {nearest}?  /help lists commands")
                        } else {
                            format!("unknown command {name} — /help lists commands")
                        };
                        self.turns.push(Turn::Assistant(hint.clone()));
                        return Some(Action::Status(hint));
                    }
                }
                self.history.push(&text);
                self.reset_history_nav();
                self.input.clear();
                self.cursor = None;
                self.scroll_back = 0;
                self.turns.push(Turn::User(text.clone()));
                self.busy = true;
                Some(Action::SubmitPrompt(text))
            }
            (KeyCode::Backspace, _) => {
                let c = self.cursor_byte();
                if c > 0 {
                    let ch = self.input[..c].chars().next_back().unwrap();
                    let start = c - ch.len_utf8();
                    self.input.drain(start..c);
                    self.cursor = (start < self.input.len()).then_some(start);
                }
                self.history_idx = None;
                self.menu_sel = 0;
                self.mention_sel = 0;
                None
            }
            (KeyCode::Delete, _) => {
                let c = self.cursor_byte();
                if c < self.input.len() {
                    let ch = self.input[c..].chars().next().unwrap();
                    let end = c + ch.len_utf8();
                    self.input.drain(c..end);
                    if self.cursor.map_or(false, |cur| cur >= self.input.len()) {
                        self.cursor = None;
                    }
                }
                self.history_idx = None;
                self.menu_sel = 0;
                self.mention_sel = 0;
                None
            }
            (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
                self.insert_at_cursor(c.encode_utf8(&mut [0u8; 4]));
                self.history_idx = None;
                self.menu_sel = 0;
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
        // The turn (or its session mint) never reached the daemon, even after
        // the blip-retry window: unwind the spinner, say so in the transcript,
        // and put the prompt back in the composer so nothing typed is lost.
        if let Action::TurnSendFailed { prompt, err } = action {
            self.busy = false;
            let msg = errfmt::humanize(err);
            let prefix = if errfmt::is_connect_shaped(err) {
                "couldn't reach the daemon"
            } else {
                "turn could not start"
            };
            self.turns.push(Turn::Assistant(format!(
                "{} {prefix} — {msg}\n\nYour prompt is back in the composer; press ⏎ to retry.",
                g("⚠", "!")
            )));
            if self.input.is_empty() {
                self.input = prompt.clone();
            }
            self.scroll_back = 0;
            return None;
        }
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
                // Failover honesty (OCEAN-275): the daemon rerouted this turn to
                // a different model than requested. Render it as a concern card
                // in the transcript (the status line alone is easy to miss) and
                // update the pill to the model actually answering.
                AgentTurnEvent::ModelRerouted {
                    requested,
                    effective,
                    reason,
                    ..
                } => {
                    self.model = Some(effective.clone());
                    self.turns.push(Turn::Advisor {
                        note: format!(
                            "{requested} unavailable — turn running on {effective}. {reason}"
                        ),
                        severity: "concern".into(),
                        model: effective.clone(),
                    });
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
                AgentTurnEvent::ToolCallFinished {
                    call_id, result, ..
                } => {
                    let ok = result.ok;
                    if let Some(Turn::Tool { status, output, .. }) = self.tool_by_id(call_id) {
                        *status = if ok { ToolStatus::Ok } else { ToolStatus::Err };
                        if output.is_empty() {
                            *output = result.output.clone();
                        }
                    }
                }
                AgentTurnEvent::TurnFinished { status, error, .. } => {
                    self.busy = false;
                    let is_failure = matches!(
                        status,
                        ocean_agent_sdk::AgentTurnStatus::Failed
                            | ocean_agent_sdk::AgentTurnStatus::Cancelled
                    );
                    if is_failure || error.is_some() {
                        let note = if let Some(e) = error {
                            format!("{} turn failed — {}", g("✗", "X"), errfmt::humanize(e))
                        } else {
                            format!("{} turn failed — no error detail", g("✗", "X"))
                        };
                        self.turns.push(Turn::ErrorNotice { note });
                    }
                }
                AgentTurnEvent::Extension {
                    extension, payload, ..
                } if extension == "advisor" => self.push_advisor(payload),
                _ => {}
            }
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        // Composer grows with its content — explicit ⌃J lines AND soft-wrapped
        // rows of long lines (typing past the right edge used to just clip:
        // the words kept landing but never showed). Capped so the transcript
        // keeps the room; past the cap the composer scrolls to the cursor.
        let usable = (area.width.saturating_sub(2)).max(1) as usize;
        let c = self.cursor_byte();
        let cursor_line_idx = self.input[..c].matches('\n').count();
        let input_rows: u16 = self
            .input
            .split('\n')
            .enumerate()
            .map(|(i, l)| {
                // The block cursor occupies one extra cell on its logical line, so
                // a line exactly at the width still wraps its cursor visibly.
                let visual_w = UnicodeWidthStr::width(l) + usize::from(i == cursor_line_idx);
                (visual_w.max(1)).div_ceil(usable) as u16
            })
            .sum::<u16>()
            .max(1);
        let input_lines = input_rows.min(8).min((area.height / 2).max(1));
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
        // Title-less chrome: the app title bar + crumb already identify this
        // pane; a third "◆ OCEAN" was pure redundancy. The model pill keeps
        // the top row.
        let pill = self.model.clone();
        let body = panel::draw(frame, chunks[0], "", pill.as_deref(), self.focused);

        // Transcript lines (bottom-anchored via scroll offset). Split the
        // borrow: the markdown cache (`md`) is a distinct field from `turns`, so
        // the loop can read turns while `md.render` mutates its cache.
        let md = &mut self.md;
        let tools_expanded = self.tools_expanded;
        // Collapsed tool-card tail lines clamp to one screen row each (gutter
        // "    │ " = 6 cols + 1 spare); ⌃O opts into full wrapped output.
        let clamp_w = (body.width as usize).saturating_sub(7);
        let busy = self.busy;
        let n_turns = self.turns.len();
        let mut lines: Vec<Line> = Vec::new();
        // ── welcome empty-state: friendly hints until the first message ──────
        if self.turns.is_empty() {
            // Center vertically with some top padding.
            let vpad = (body.height.saturating_sub(8)) / 2;
            for _ in 0..vpad {
                lines.push(Line::from(""));
            }
            // "OCEAN" title
            lines.push(Line::from(Span::styled(
                "  OCEAN",
                Style::default()
                    .fg(theme::CYAN)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            // Provider line
            if let Some(pline) = &self.welcome_provider_line {
                lines.push(Line::from(Span::styled(
                    format!("  {pline}"),
                    Style::default().fg(theme::YELLOW),
                )));
                lines.push(Line::from(""));
            }
            // Hints
            let hints = [
                "⏎ send · ⌃J newline · / commands",
                "/login — connect a provider",
                "/models — pick a model + thinking",
                "/help — everything else",
            ];
            for hint in &hints {
                lines.push(Line::from(Span::styled(
                    format!("  {hint}"),
                    Style::default().fg(theme::COMMENT),
                )));
            }
        }
        // Collapsed mode: a long consecutive tool run must not flood the
        // screen. Hide all but the newest BURST_TAIL_TOOLS ok/running cards of
        // each run behind one "· N earlier tools" line; errors always render.
        let mut tool_hidden: Vec<bool> = vec![false; n_turns];
        let mut elide_count_at: Vec<usize> = vec![0; n_turns];
        if !tools_expanded {
            let mut i = 0;
            while i < n_turns {
                if !matches!(self.turns[i], Turn::Tool { .. } | Turn::Thinking(_)) {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < n_turns && matches!(self.turns[i], Turn::Tool { .. } | Turn::Thinking(_))
                {
                    i += 1;
                }
                let hideable: Vec<usize> = (start..i)
                    .filter(|&t| {
                        matches!(
                            &self.turns[t],
                            Turn::Tool { status, .. } if !matches!(status, ToolStatus::Err)
                        )
                    })
                    .collect();
                if hideable.len() > BURST_TAIL_TOOLS {
                    let cut = hideable.len() - BURST_TAIL_TOOLS;
                    for &t in &hideable[..cut] {
                        tool_hidden[t] = true;
                    }
                    elide_count_at[hideable[0]] = cut;
                }
            }
        }
        for (ti, turn) in self.turns.iter().enumerate() {
            match turn {
                Turn::User(s) => {
                    lines.push(Line::from(vec![
                        Span::styled(g("❯ ", "> "), Style::default().fg(theme::CYAN)),
                        Span::styled(
                            sanitize_line(s),
                            Style::default()
                                .fg(theme::CYAN)
                                .add_modifier(Modifier::BOLD),
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
                                    sanitize_line(tool),
                                    Style::default().fg(theme::FG).add_modifier(Modifier::BOLD),
                                ),
                            ]));
                            lines.push(Line::from(vec![
                                Span::styled(
                                    format!("  {} ", g("▎", "|")),
                                    Style::default().fg(theme::YELLOW),
                                ),
                                Span::styled(sanitize_line(reason), Style::default().fg(theme::FG)),
                            ]));
                            lines.push(Line::from(Span::styled(
                                "  ⌃Y allow · ⌃N deny",
                                Style::default().fg(theme::YELLOW),
                            )));
                        }
                        Some(true) => {
                            let st = sanitize_line(tool);
                            lines.push(Line::from(Span::styled(
                                format!("  {} allowed: {st}", g("✓", "+")),
                                Style::default().fg(theme::GREEN),
                            )));
                        }
                        Some(false) => {
                            let st = sanitize_line(tool);
                            lines.push(Line::from(Span::styled(
                                format!("  {} denied: {st}", g("✗", "x")),
                                Style::default().fg(theme::RED),
                            )));
                        }
                    }
                }
                Turn::Thinking(s) => {
                    // Collapsed mode shows thinking ONLY while it's the live
                    // tail of a busy turn (feedback that the model is working).
                    // Historical thinking markers between every tool call were
                    // half the transcript spam; ⌃O brings them all back.
                    if tools_expanded || (busy && ti + 1 == n_turns) {
                        lines.push(Line::from(Span::styled(
                            format!("  {} thinking ({} chars)", g("◌", "~"), s.len()),
                            Style::default()
                                .fg(theme::COMMENT)
                                .add_modifier(Modifier::ITALIC),
                        )));
                    } else {
                        continue; // no separator row either
                    }
                }
                Turn::Tool {
                    name,
                    args,
                    output,
                    status,
                    diff,
                    ..
                } => {
                    // Burst compaction (collapsed mode): this card is hidden
                    // behind the run's "· N earlier tools" line.
                    if tool_hidden[ti] {
                        if elide_count_at[ti] > 0 {
                            lines.push(Line::from(Span::styled(
                                format!(
                                    "  {} {} earlier tools · ⌃O expand",
                                    g("·", "-"),
                                    elide_count_at[ti]
                                ),
                                Style::default()
                                    .fg(theme::COMMENT)
                                    .add_modifier(Modifier::ITALIC),
                            )));
                        }
                        continue; // no separator — the burst reads as one block
                    }
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

                    // Collapsed, healthy cards are ONE line: the header plus a
                    // dim outcome summary. Diff cards join the one-line rule
                    // except at the transcript tail — the just-finished edit
                    // keeps its hunk visible for live feedback; ⌃O restores
                    // full output for everything. Errors keep their tail (red
                    // matters).
                    let is_tail = ti + 1 == n_turns;
                    let collapsed_plain = !tools_expanded
                        && !matches!(status, ToolStatus::Err)
                        && (diff.is_none() || !is_tail);
                    if collapsed_plain {
                        if let Some(rows) = diff {
                            let adds = rows
                                .iter()
                                .filter(|r| matches!(r.kind, DiffKind::Add))
                                .count();
                            let dels = rows
                                .iter()
                                .filter(|r| matches!(r.kind, DiffKind::Del))
                                .count();
                            header.push(Span::styled(
                                format!("  {} diff +{adds} −{dels} · ⌃O", g("·", "-")),
                                Style::default()
                                    .fg(theme::COMMENT)
                                    .add_modifier(Modifier::ITALIC),
                            ));
                        } else {
                            let out = output.trim();
                            if !out.is_empty() {
                                let total = out.lines().count();
                                let first = one_line(out.lines().next().unwrap_or(""), 48);
                                let summary = if total == 1 && first.chars().count() <= 48 {
                                    format!("  {} {first}", g("·", "-"))
                                } else if total == 1 {
                                    format!("  {} 1 line", g("·", "-"))
                                } else {
                                    format!("  {} {total} lines", g("·", "-"))
                                };
                                header.push(Span::styled(
                                    summary,
                                    Style::default()
                                        .fg(theme::COMMENT)
                                        .add_modifier(Modifier::ITALIC),
                                ));
                            }
                        }
                        lines.push(Line::from(header));
                        // Skip the separator when the next row is another tool
                        // call — a burst of one-liners reads as one block.
                        if matches!(
                            self.turns.get(ti + 1),
                            Some(Turn::Tool { .. }) | Some(Turn::Thinking(_))
                        ) {
                            continue;
                        }
                        lines.push(Line::from(""));
                        continue;
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
                                    // Terminal-safe first (tabs/controls smear
                                    // cells — see `sanitize_line`), then when
                                    // collapsed: one screen row per line, hard
                                    // stop — a single giant line (JSON blobs)
                                    // must not wrap into a wall. ⌃O shows all.
                                    let l = sanitize_line(l);
                                    let text = if tools_expanded {
                                        l
                                    } else {
                                        clamp_line(&l, clamp_w)
                                    };
                                    lines.push(Line::from(vec![
                                        Span::styled(
                                            format!("    {} ", g("│", "|")),
                                            Style::default().fg(theme::EDGE),
                                        ),
                                        Span::styled(
                                            text,
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
                    let sev = sanitize_line(severity);
                    let mut header: Vec<Span> = vec![Span::styled(
                        format!("  {} advisor ({sev})", g("⚑", "!")),
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    )];
                    if !model.is_empty() {
                        let san_model = sanitize_line(model);
                        header.push(Span::styled(
                            format!("  · {san_model}"),
                            Style::default()
                                .fg(theme::COMMENT)
                                .add_modifier(Modifier::DIM),
                        ));
                    }
                    lines.push(Line::from(header));
                    let notice_w = (body.width as usize).saturating_sub(2);
                    for l in note.lines() {
                        let sane = sanitize_line(l);
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {} ", g("▎", "|")),
                                Style::default().fg(accent),
                            ),
                            Span::styled(
                                clamp_line(&sane, notice_w),
                                Style::default().fg(theme::FG),
                            ),
                        ]));
                    }
                }
                Turn::ErrorNotice { note } => {
                    let notice_w = (body.width as usize).saturating_sub(2);
                    for l in note.lines() {
                        let sane = sanitize_line(l);
                        lines.push(Line::from(Span::styled(
                            format!("  {}", clamp_line(&sane, notice_w)),
                            Style::default().fg(theme::RED).add_modifier(Modifier::BOLD),
                        )));
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
                    Style::default().fg(if self.busy {
                        theme::COMMENT
                    } else {
                        theme::CYAN
                    }),
                ))
                .style(Style::default().bg(theme::BG_HL)),
                Rect::new(comp.x, comp.y + k, 1, 1),
            );
        }
        let input_fg = if self.busy { theme::COMMENT } else { theme::FG };
        let cursor_glyph = g("▏", "_");
        let cursor_style = Style::default().fg(theme::CYAN);

        // Cursor position within input
        let c = self.cursor_byte();
        let cursor_line_idx = self.input[..c].matches('\n').count();
        let line_start = self.input[..c].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let after_cursor_line_end = self.input[c..]
            .find('\n')
            .map(|i| c + i)
            .unwrap_or(self.input.len());

        // Build render lines: cursor line gets before+glyph+after spans
        let mut input_render: Vec<Line> = Vec::new();
        for (li, line_text) in self.input.split('\n').enumerate() {
            if li == cursor_line_idx {
                let before = &self.input[line_start..c];
                let after = &self.input[c..after_cursor_line_end];
                let mut spans: Vec<Span> = Vec::with_capacity(3);
                if !before.is_empty() {
                    spans.push(Span::styled(
                        before.to_string(),
                        Style::default().fg(input_fg),
                    ));
                }
                spans.push(Span::styled(cursor_glyph, cursor_style));
                if !after.is_empty() {
                    spans.push(Span::styled(
                        after.to_string(),
                        Style::default().fg(input_fg),
                    ));
                }
                input_render.push(Line::from(spans));
            } else {
                input_render.push(Line::from(Span::styled(
                    line_text.to_string(),
                    Style::default().fg(input_fg),
                )));
            }
        }
        // Preserve trailing empty line when input ends with '\n'
        if input_render.is_empty() || self.input.ends_with('\n') {
            input_render.push(Line::from(""));
        }

        // Wrap and scroll: keep the cursor's visual row visible, not always bottom.
        let input_w = (comp.width.saturating_sub(2)).max(1) as usize;
        let before_cursor = &self.input[line_start..c];
        let vis_before = UnicodeWidthStr::width(before_cursor);
        // Visual rows occupied by lines before the cursor's logical line
        let prior_rows: u16 = self
            .input
            .split('\n')
            .take(cursor_line_idx)
            .map(|l| {
                let vw = UnicodeWidthStr::width(l).max(1);
                vw.div_ceil(input_w) as u16
            })
            .sum();
        // Cursor's visual row within its own line (0-indexed)
        let cursor_row_in_line = if input_w > 0 {
            (vis_before / input_w) as u16
        } else {
            0
        };
        let cursor_vis_row = prior_rows + cursor_row_in_line;

        let input_para = Paragraph::new(input_render)
            .style(Style::default().bg(theme::BG_HL))
            .wrap(Wrap { trim: false });
        let wrapped_rows = input_para.line_count(input_w as u16) as u16;
        let max_scroll = wrapped_rows.saturating_sub(comp.height);
        // When cursor is below the visible area, scroll so it's on the last visible row
        let input_scroll = cursor_vis_row
            .saturating_sub(comp.height.saturating_sub(1))
            .min(max_scroll);
        frame.render_widget(
            input_para.scroll((input_scroll, 0)),
            Rect::new(comp.x + 2, comp.y, input_w as u16, comp.height),
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
    use crate::shell::action::LoginTarget;
    use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent, AgentTurnId, AgentTurnStatus};
    use serde_json::json;
    use uuid::Uuid;

    /// A chat with the composer pre-filled — avoids the `field_reassign_with_default`
    /// clippy lint that fires on `let mut c = default(); c.input = …`.
    fn chat_with(input: &str) -> ChatComponent {
        ChatComponent {
            input: input.to_string(),
            ..Default::default()
        }
    }
    fn render_chat_to_string(chat: &mut ChatComponent, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| chat.draw(frame, frame.area()))
            .expect("draw chat");
        let buf = terminal.backend().buffer();
        let area = buf.area;
        let mut out = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buf.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
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
        chat.update(&extension(
            "advisor",
            json!({ "note": "   ", "severity": "info" }),
        ));
        chat.update(&extension("advisor", json!({ "severity": "blocker" })));
        assert!(chat.turns.is_empty());
    }

    fn perm_envelope(pid: PermissionId, event: OceanEvent) -> Action {
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
        assert_eq!(
            chat.pending_permission(),
            Some(pid),
            "card should be pending"
        );
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
    fn sanitize_line_expands_tabs_and_drops_controls() {
        // Raw tabs make the terminal jump to ITS tab stops while ratatui
        // paints at its own cell math — the smeared-bleed bug John hit with
        // the read tool's `<lineno>\t<code>` output lines.
        assert_eq!(sanitize_line("1108\t}"), "1108    }");
        // ESC/CR and friends drop; plain text passes through untouched.
        assert_eq!(sanitize_line("a\x1b[31mred\rb"), "a[31mredb");
        assert_eq!(sanitize_line("plain text"), "plain text");
    }

    #[test]
    fn clamp_line_truncates_but_preserves_whitespace() {
        // Unlike one_line, indentation must survive — it's signal in tool
        // output previews.
        assert_eq!(clamp_line("  indented ok", 40), "  indented ok");
        // A giant single line hard-stops at max chars (one screen row), with
        // an ellipsis marking the cut.
        let blob = "x".repeat(500);
        let clamped = clamp_line(&blob, 40);
        assert_eq!(clamped.chars().count(), 40);
        assert!(clamped.ends_with('…'));
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
        // Bare `/model` (and `/models`) open the picker — nobody memorizes ids.
        assert!(matches!(
            chat.run_slash("/model", ""),
            Some(Action::OpenModels)
        ));
        assert!(matches!(
            chat.run_slash("/models", ""),
            Some(Action::OpenModels)
        ));
    }

    #[test]
    fn slash_thinking_routes_levels_and_usage() {
        let mut chat = ChatComponent::default();
        match chat.run_slash("/thinking", "high") {
            Some(Action::SetThinking(Some(ThinkingLevel::High))) => {}
            other => panic!("expected SetThinking(high), got {other:?}"),
        }
        match chat.run_slash("/thinking", "default") {
            Some(Action::SetThinking(None)) => {}
            other => panic!("expected SetThinking(default), got {other:?}"),
        }
        match chat.run_slash("/thinking", "") {
            Some(Action::Status(s)) => assert!(s.contains("usage: /thinking"), "got: {s}"),
            other => panic!("expected usage Status, got {other:?}"),
        }
    }

    #[test]
    fn slash_login_without_args_opens_providers_popup() {
        // Bare `/login` opens the provider popup (OAuth + API keys); the
        // direct browser flow is still reachable via `/login claude|codex`.
        let mut chat = ChatComponent::default();

        let act = chat.run_slash("/login", "");

        assert!(matches!(act, Some(Action::OpenProviders)));
    }

    #[test]
    fn slash_providers_opens_popup() {
        let mut chat = ChatComponent::default();

        let act = chat.run_slash("/providers", "");

        assert!(matches!(act, Some(Action::OpenProviders)));
    }

    #[test]
    fn slash_copy_uses_last_reply() {
        let mut chat = ChatComponent::default();
        assert!(matches!(
            chat.run_slash("/copy", ""),
            Some(Action::Status(_))
        )); // nothing yet
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
        assert!(
            chat.turns.is_empty(),
            "command line must not become a User turn"
        );
        // A `/`-path that is NOT a command still sends as a normal message.
        let mut chat2 = chat_with("/etc/hosts is a file");
        let act2 = chat2.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(act2, Some(Action::SubmitPrompt(_))));
    }

    #[test]
    fn typed_login_codex_routes_login_action_not_user_turn() {
        let mut chat = chat_with("/login codex");

        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(matches!(act, Some(Action::Login(LoginTarget::Codex))));
        assert!(
            chat.turns.is_empty(),
            "command line must not become a User turn"
        );
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

    fn type_chars(chat: &mut ChatComponent, text: &str) {
        for c in text.chars() {
            chat.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn press_left(chat: &mut ChatComponent, count: usize) {
        for _ in 0..count {
            chat.handle_key(key(KeyCode::Left));
        }
    }

    fn composer_rows(chat: &mut ChatComponent, width: u16, height: u16) -> Vec<String> {
        let screen = render_chat_to_string(chat, width, height);
        let bar = g("▎", "|");
        let mut rows: Vec<String> = screen
            .lines()
            .rev()
            .take_while(|line| line.starts_with(bar))
            .map(|line| line.chars().skip(2).collect::<String>())
            .map(|line| line.trim_end().to_string())
            .collect();
        rows.reverse();
        rows
    }

    #[test]
    fn review_regression_model_command_trims_extra_whitespace() {
        let mut chat = chat_with("/model  anthropic/claude-opus-4-8");

        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        match act {
            Some(Action::SetModel(id)) => assert_eq!(id, "anthropic/claude-opus-4-8"),
            other => panic!("expected SetModel, got {other:?}"),
        }
    }

    #[test]
    fn review_regression_slash_tab_completion_resets_cursor_to_command_end() {
        let mut chat = chat_with("/mod");

        press_left(&mut chat, 1);
        chat.handle_key(key(KeyCode::Tab));
        type_chars(&mut chat, "!");

        assert_eq!(chat.input, "/model!");
    }

    #[test]
    fn review_regression_mention_tab_completion_uses_token_at_cursor_and_preserves_suffix() {
        let mut chat = chat_with("fix @src now");
        chat.mention_index = Some(vec!["src/main.rs".to_string()]);

        press_left(&mut chat, 4);
        let wants_tab = chat.wants_tab();
        chat.handle_key(key(KeyCode::Tab));
        type_chars(&mut chat, "X");

        assert_eq!(
            (wants_tab, chat.input.as_str()),
            (true, "fix @src/main.rs Xnow")
        );
    }

    #[test]
    fn review_regression_mention_after_multibyte_whitespace_is_utf8_safe() {
        let mut chat = chat_with("fix\u{00a0}@src now");
        chat.mention_index = Some(vec!["src/main.rs".to_string()]);

        press_left(&mut chat, 4);
        let wants_tab = chat.wants_tab();
        chat.handle_key(key(KeyCode::Tab));

        assert_eq!(
            (wants_tab, chat.input.as_str()),
            (true, "fix\u{00a0}@src/main.rs now")
        );
    }

    #[test]
    fn review_regression_mention_preserves_multiline_suffix_whitespace() {
        let mut chat = chat_with("fix @src\n  now");
        chat.mention_index = Some(vec!["src/main.rs".to_string()]);

        press_left(&mut chat, 6);
        chat.handle_key(key(KeyCode::Tab));
        type_chars(&mut chat, "X");

        assert_eq!(chat.input, "fix @src/main.rs\n  Xnow");
    }

    #[test]
    fn cursor_readline_left_right_insert_at_cursor() {
        let mut chat = chat_with("abcd");

        press_left(&mut chat, 2);
        type_chars(&mut chat, "X");
        chat.handle_key(key(KeyCode::Right));
        type_chars(&mut chat, "Y");

        assert_eq!(chat.input, "abXcYd");
    }

    #[test]
    fn cursor_readline_ctrl_b_ctrl_f_move_by_character() {
        let mut chat = chat_with("rust");

        chat.handle_key(ctrl('b'));
        chat.handle_key(ctrl('b'));
        type_chars(&mut chat, "X");
        chat.handle_key(ctrl('f'));
        type_chars(&mut chat, "Y");

        assert_eq!(chat.input, "ruXsYt");
    }

    #[test]
    fn cursor_readline_backspace_delete_and_ctrl_d_edit_at_cursor() {
        let mut chat = chat_with("abcd");

        press_left(&mut chat, 2);
        chat.handle_key(key(KeyCode::Backspace));
        assert_eq!(chat.input, "acd", "backspace removes the char before point");

        chat.handle_key(key(KeyCode::Delete));
        assert_eq!(chat.input, "ad", "delete removes the char at point");

        chat.handle_key(ctrl('d'));
        assert_eq!(chat.input, "a", "⌃D also deletes the char at point");
    }

    #[test]
    fn cursor_readline_home_end_and_ctrl_a_ctrl_e_render_cursor_position() {
        let cursor = g("▏", "_");
        let mut chat = chat_with("abc");

        chat.handle_key(key(KeyCode::Home));
        assert_eq!(composer_rows(&mut chat, 20, 8)[0], format!("{cursor}abc"));

        chat.handle_key(key(KeyCode::End));
        assert_eq!(composer_rows(&mut chat, 20, 8)[0], format!("abc{cursor}"));

        chat.handle_key(ctrl('a'));
        assert_eq!(composer_rows(&mut chat, 20, 8)[0], format!("{cursor}abc"));

        chat.handle_key(ctrl('e'));
        assert_eq!(composer_rows(&mut chat, 20, 8)[0], format!("abc{cursor}"));
    }

    #[test]
    fn cursor_readline_multiline_home_end_are_current_line_boundaries() {
        let mut chat = chat_with("one\ntwo three");

        press_left(&mut chat, 5);
        chat.handle_key(key(KeyCode::Home));
        type_chars(&mut chat, ">");
        chat.handle_key(key(KeyCode::End));
        type_chars(&mut chat, "<");

        assert_eq!(chat.input, "one\n>two three<");
    }

    #[test]
    fn cursor_readline_ctrl_k_u_w_and_yank_use_cursor_position() {
        let mut word_kill = chat_with("alpha beta gamma");
        press_left(&mut word_kill, 5);
        word_kill.handle_key(ctrl('w'));
        assert_eq!(word_kill.input, "alpha gamma");
        assert_eq!(
            word_kill.kill_ring.last().map(String::as_str),
            Some("beta ")
        );

        word_kill.handle_key(ctrl('y'));
        assert_eq!(word_kill.input, "alpha beta gamma");

        let mut prefix_kill = chat_with("alpha beta gamma");
        press_left(&mut prefix_kill, 5);
        prefix_kill.handle_key(ctrl('u'));
        assert_eq!(prefix_kill.input, "gamma");
        assert_eq!(
            prefix_kill.kill_ring.last().map(String::as_str),
            Some("alpha beta ")
        );

        let mut tail_kill = chat_with("alpha beta gamma");
        press_left(&mut tail_kill, 5);
        tail_kill.handle_key(ctrl('k'));
        assert_eq!(tail_kill.input, "alpha beta ");
        assert_eq!(
            tail_kill.kill_ring.last().map(String::as_str),
            Some("gamma")
        );
    }

    #[test]
    fn cursor_readline_multiline_ctrl_u_kills_only_current_line_prefix() {
        let mut chat = chat_with("first line\nsecond part");

        press_left(&mut chat, 4);
        chat.handle_key(ctrl('u'));

        assert_eq!(chat.input, "first line\npart");
        assert_eq!(chat.kill_ring.last().map(String::as_str), Some("second "));
    }

    #[test]
    fn cursor_readline_ctrl_l_clears_idle_transcript_preserving_composer_and_cursor() {
        let mut chat = chat_with("abc");
        chat.turns.push(Turn::User("old prompt".into()));

        chat.handle_key(key(KeyCode::Left));
        chat.handle_key(ctrl('l'));

        assert!(chat.turns.is_empty(), "⌃L clears an idle transcript");
        assert_eq!(chat.input, "abc", "⌃L preserves composer text");

        type_chars(&mut chat, "X");
        assert_eq!(chat.input, "abXc", "⌃L preserves cursor position");
    }

    #[test]
    fn cursor_readline_ctrl_l_noops_while_busy() {
        let mut chat = chat_with("abc");
        chat.busy = true;
        chat.turns.push(Turn::User("streaming prompt".into()));

        chat.handle_key(ctrl('l'));

        assert_eq!(chat.turns.len(), 1, "busy transcript is not cleared");
        assert_eq!(chat.input, "abc", "busy ⌃L preserves composer text");
    }

    #[test]
    fn cursor_readline_unicode_editing_treats_wide_chars_as_characters() {
        let mut chat = chat_with("a你🙂b");

        chat.handle_key(key(KeyCode::Left));
        chat.handle_key(key(KeyCode::Backspace));
        assert_eq!(chat.input, "a你b", "backspace removes one Unicode scalar");

        chat.handle_key(key(KeyCode::Delete));
        assert_eq!(
            chat.input, "a你",
            "delete removes the Unicode char at point"
        );
    }

    #[test]
    fn cursor_readline_render_wraps_cjk_emoji_before_cursor_by_visual_width() {
        let cursor = g("▏", "_");
        let mut chat = chat_with("你🙂ab");

        chat.handle_key(key(KeyCode::Left));

        assert_eq!(
            composer_rows(&mut chat, 8, 8),
            vec![format!("你 🙂 a{cursor}"), "b".to_string(), String::new()]
        );
    }

    #[test]
    fn cursor_readline_render_can_place_cursor_on_wrapped_line_before_later_input() {
        let cursor = g("▏", "_");
        let mut chat = chat_with("abcdefghijk\nzz");

        press_left(&mut chat, 3);

        assert_eq!(
            composer_rows(&mut chat, 12, 10),
            vec![
                "abcdefghij".to_string(),
                format!("k{cursor}"),
                "zz".to_string(),
                String::new()
            ]
        );
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
        assert_eq!(
            chat.input, "in-progress draft",
            "↑ must not clobber a draft"
        );
        chat.handle_key(key(KeyCode::Down));
        assert_eq!(
            chat.input, "in-progress draft",
            "↓ no-ops when not navigating"
        );
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
        assert_eq!(
            chat.kill_ring.last().map(String::as_str),
            Some("hello world")
        );
        chat.handle_key(ctrl('y'));
        assert_eq!(
            chat.input, "hello world",
            "⌃Y yanks the newest kill at the end"
        );
    }

    #[test]
    fn cursor_readline_ctrl_k_at_end_is_noop_and_mid_line_kills_tail() {
        let mut chat = chat_with("line one\nline two");

        chat.handle_key(ctrl('k'));
        assert_eq!(
            chat.input, "line one\nline two",
            "⌃K at end has no tail to kill"
        );
        assert!(chat.kill_ring.is_empty(), "empty kills are not recorded");

        press_left(&mut chat, 3);
        chat.handle_key(ctrl('k'));

        assert_eq!(chat.input, "line one\nline ");
        assert_eq!(chat.kill_ring.last().map(String::as_str), Some("two"));
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
        assert_eq!(
            chat.input, "",
            "yank must not fire while a permission is pending"
        );
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

    // ── unknown-command feedback ────────────────────────────────────────────

    #[test]
    fn unknown_command_does_not_submit_as_prompt() {
        let mut chat = chat_with("/notacommand");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // Must be a Status, not SubmittedPrompt.
        assert!(
            matches!(&act, Some(Action::Status(s)) if s.contains("unknown command /notacommand")),
            "expected Status with 'unknown command', got {act:?}"
        );
        // No user turn was pushed to the transcript.
        assert!(
            !chat.turns.iter().any(|t| matches!(t, Turn::User(_))),
            "unknown command must not create a user turn"
        );
    }
    #[test]
    fn unknown_command_near_match_suggests_correction() {
        // "/provder xyz" — whitespace closes the palette, so Enter reaches the
        // unknown-command branch instead of palette-run. The near-match path
        // only fires for the `/cmd args` form, not single-token palette use.
        let mut chat = chat_with("/provder xyz");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(&act, Some(Action::Status(s)) if s.contains("did you mean /providers")),
            "fuzzy near-match should suggest /providers, got {act:?}"
        );
        // Also surfaced in the transcript as an Assistant block.
        assert!(
            chat.turns.iter().any(|t| {
                matches!(t, Turn::Assistant(s) if s.contains("did you mean /providers"))
            }),
            "near-match hint should appear in transcript"
        );
    }
    #[test]
    fn path_like_slash_still_submits_as_prompt() {
        // "/etc/passwd hi" — looks like a path (has a second slash), not a
        // command name. Must fall through to normal chat submission.
        let mut chat = chat_with("/etc/passwd hi");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            matches!(&act, Some(Action::SubmitPrompt(s)) if s == "/etc/passwd hi"),
            "path-like slash input must submit as prompt, got {act:?}"
        );
    }

    #[test]
    fn slash_known_command_still_works() {
        // Regression: /help must still work as before.
        let mut chat = chat_with("/help");
        let act = chat.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        // /help pushes to transcript, returns None (not a Status or Submit).
        assert!(
            act.is_none(),
            "/help should return None after pushing output, got {act:?}"
        );
        assert!(
            chat.turns
                .iter()
                .any(|t| matches!(t, Turn::Assistant(s) if s.contains("/quit"))),
            "/help should list commands in transcript"
        );
    }

    // ── wants_tab ────────────────────────────────────────────────────────────

    #[test]
    fn wants_tab_true_when_palette_open() {
        let chat = chat_with("/mod");
        assert!(
            chat.wants_tab(),
            "palette is open, Tab should route to chat"
        );
    }

    #[test]
    fn wants_tab_false_when_palette_closed() {
        let chat = chat_with("hello");
        assert!(!chat.wants_tab(), "no palette, Tab should cycle focus");
    }

    #[test]
    fn wants_tab_true_when_mention_picker_open() {
        let chat = chat_with("read @src/main.rs");
        assert!(
            chat.wants_tab(),
            "mention picker is open, Tab should route to chat"
        );
    }

    #[test]
    fn wants_tab_false_when_slash_has_whitespace() {
        // "/model gpt" — palette closed (space typed), Tab should cycle focus.
        let chat = chat_with("/model gpt");
        assert!(!chat.wants_tab());
    }

    #[test]
    fn welcome_empty_state_disappears_after_first_transcript_turn() {
        let mut chat = ChatComponent {
            welcome_provider_line: Some("provider sentinel".into()),
            ..Default::default()
        };

        let empty = render_chat_to_string(&mut chat, 80, 24);
        assert!(
            empty.contains("OCEAN"),
            "empty transcript should render the welcome title"
        );
        assert!(
            empty.contains("provider sentinel"),
            "empty transcript should show the provider status line"
        );

        chat.turns.push(Turn::User("hello ocean".into()));
        let filled = render_chat_to_string(&mut chat, 80, 24);

        assert!(
            filled.contains("hello ocean"),
            "existing transcript should render the user turn"
        );
        assert!(
            !filled.contains("provider sentinel"),
            "welcome provider line must not appear once transcript is non-empty"
        );
    }
    // ── turn-terminal paths ──────────────────────────────────────────────────

    fn turn_finished(status: AgentTurnStatus, error: Option<&str>) -> Action {
        Action::AgentEvent(Box::new(AgentTurnEvent::TurnFinished {
            session_id: AgentSessionId(Uuid::nil()),
            turn_id: AgentTurnId(Uuid::nil()),
            status,
            error: error.map(|s| s.to_string()),
            wall_ms: None,
            output_tokens: None,
            input_tokens: None,
            cache_read_tokens: None,
            tokens_per_second: None,
        }))
    }

    #[test]
    fn turn_finished_with_error_pushes_error_notice_and_clears_busy() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&turn_finished(
            ocean_agent_sdk::AgentTurnStatus::Failed,
            Some("HTTP 401 Unauthorized"),
        ));
        assert!(!chat.busy, "busy must be cleared on TurnFinished");
        assert!(
            chat.turns
                .iter()
                .any(|t| matches!(t, Turn::ErrorNotice { note } if note.contains("turn failed"))),
            "failed turn with error should push an ErrorNotice"
        );
    }

    #[test]
    fn turn_finished_provider_auth_error_uses_login_recovery_notice() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };

        chat.update(&turn_finished(
            ocean_agent_sdk::AgentTurnStatus::Failed,
            Some("{\"error\":{\"message\":\"token_invalidated\"}}"),
        ));

        assert!(!chat.busy, "terminal failed event must clear busy");
        let notice = chat.turns.iter().find_map(|t| match t {
            Turn::ErrorNotice { note } => Some(note.as_str()),
            _ => None,
        });
        assert!(
            matches!(notice, Some(note) if note.contains("run /login to reconnect")),
            "provider credential failures should render /login recovery guidance, got {notice:?}"
        );
        assert!(
            !chat.turns.iter().any(|t| matches!(t, Turn::Advisor { .. })),
            "turn-terminal failures should render as ErrorNotice, not advisor cards"
        );
    }

    #[test]
    fn turn_finished_failed_no_error_renders_fallback() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&turn_finished(
            ocean_agent_sdk::AgentTurnStatus::Failed,
            None,
        ));
        assert!(!chat.busy);
        assert!(
            chat.turns.iter().any(
                |t| matches!(t, Turn::ErrorNotice { note } if note.contains("no error detail"))
            ),
            "failed turn without error should push ErrorNotice"
        );
    }

    #[test]
    fn turn_finished_cancelled_pushes_error_notice() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&turn_finished(
            ocean_agent_sdk::AgentTurnStatus::Cancelled,
            None,
        ));
        assert!(!chat.busy);
        assert!(
            chat.turns
                .iter()
                .any(|t| matches!(t, Turn::ErrorNotice { note } if note.contains("turn failed"))),
            "cancelled turn should push an ErrorNotice"
        );
    }

    #[test]
    fn turn_finished_completed_clears_busy_without_notice() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&turn_finished(
            ocean_agent_sdk::AgentTurnStatus::Completed,
            None,
        ));
        assert!(!chat.busy);
        assert!(
            !chat
                .turns
                .iter()
                .any(|t| matches!(t, Turn::Advisor { .. } | Turn::ErrorNotice { .. })),
            "successful turns should push neither advisor nor error notice"
        );
    }

    // ── SSE reconnect does not clear busy ────────────────────────────────────

    #[test]
    fn stream_reconnect_does_not_clear_busy() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&Action::Status("stream reconnected".into()));
        assert!(
            chat.busy,
            "reconnect must not clear busy — only terminal events do"
        );
        assert!(chat.turns.is_empty(), "reconnect must not push turns");
    }

    #[test]
    fn stream_reconnecting_during_busy_does_not_clear_busy() {
        let mut chat = ChatComponent {
            busy: true,
            ..Default::default()
        };
        chat.update(&Action::Status("stream reconnecting…".into()));
        assert!(chat.busy, "reconnecting must not clear busy");
        assert!(chat.turns.is_empty(), "reconnecting must not push turns");
    }

    // ── sanitize_line at error-notice boundary ───────────────────────────────

    #[test]
    fn sanitize_line_strips_control_chars() {
        let input = "hello\x1b[31mworld\x07\t tab\r\nnew";
        let got = sanitize_line(input);
        assert!(!got.contains('\x1b'), "ESC stripped");
        assert!(!got.contains('\x07'), "BEL stripped");
        assert!(!got.contains('\r'), "CR stripped");
        assert!(!got.contains('\n'), "LF stripped");
        assert!(got.contains("    "), "tab expanded to spaces");
        assert!(got.contains("hello"), "normal chars preserved");
        assert!(got.contains("world"), "normal chars preserved");
        assert!(got.contains(" tab"), "normal chars preserved");
        assert!(got.contains("new"), "normal chars preserved");
    }

    // ── bracketed paste into the composer ─────────────────────────────────

    #[test]
    fn paste_inserts_multiline_without_submitting() {
        let mut chat = ChatComponent::default();
        let act = chat.handle_event(&crossterm::event::Event::Paste(
            "one\ntwo\r\nthree".to_string(),
        ));
        assert!(act.is_none(), "paste must never submit");
        assert_eq!(chat.input, "one\ntwo\nthree");
        assert!(!chat.busy, "paste must not mark the chat busy");
    }

    #[test]
    fn paste_strips_controls_and_expands_tabs() {
        let mut chat = ChatComponent::default();
        chat.handle_event(&crossterm::event::Event::Paste(
            "a\tb\x1b[31mc\x07".to_string(),
        ));
        assert_eq!(chat.input, "a    b[31mc");
    }

    #[test]
    fn paste_feeds_search_query_while_overlay_open() {
        let mut chat = ChatComponent::default();
        chat.handle_key(ctrl('r'));
        assert!(chat.search.is_some(), "⌃R opens the search overlay");
        chat.handle_event(&crossterm::event::Event::Paste("cargo test\n".to_string()));
        assert_eq!(
            chat.search.as_ref().map(|s| s.query.as_str()),
            Some("cargo test"),
            "paste feeds the query, newline dropped"
        );
        assert!(
            chat.input.is_empty(),
            "composer untouched while search open"
        );
    }

    // ── collapsed tool cards: sanitized summaries + burst compaction ───────

    fn ok_tool(name: &str, output: &str) -> Turn {
        Turn::Tool {
            id: ocean_agent_sdk::ToolCallId::new_v4(),
            name: name.to_string(),
            args: String::new(),
            output: output.to_string(),
            status: ToolStatus::Ok,
            diff: None,
        }
    }

    #[test]
    fn one_line_drops_escape_bytes() {
        let s = one_line("ok \x1b[2J\x1b[H wiped", 64);
        assert!(!s.contains('\x1b'), "ESC must not reach a Span: {s:?}");
        assert!(s.contains("ok"), "printable text survives");
    }

    #[test]
    fn collapsed_summary_never_paints_control_bytes() {
        let mut chat = ChatComponent::default();
        chat.turns.push(ok_tool("bash", "done\x1b[2J\x07"));
        let screen = render_chat_to_string(&mut chat, 80, 12);
        assert!(!screen.contains('\x1b'), "collapsed summary leaked ESC");
        assert!(!screen.contains('\x07'), "collapsed summary leaked BEL");
        assert!(screen.contains("bash"), "card header renders");
    }

    #[test]
    fn collapsed_burst_elides_earlier_tools() {
        let mut chat = ChatComponent::default();
        for i in 0..6 {
            chat.turns.push(ok_tool(&format!("tool{i}"), "ok"));
        }
        let screen = render_chat_to_string(&mut chat, 90, 16);
        assert!(
            screen.contains("3 earlier tools"),
            "burst must compact: {screen:?}"
        );
        assert!(!screen.contains("tool0"), "oldest card hidden");
        assert!(!screen.contains("tool2"), "hidden up to the tail window");
        assert!(screen.contains("tool3"), "tail window starts here");
        assert!(screen.contains("tool5"), "newest card visible");
    }

    #[test]
    fn collapsed_burst_keeps_errors_visible() {
        let mut chat = ChatComponent::default();
        for i in 0..5 {
            chat.turns.push(ok_tool(&format!("tool{i}"), "ok"));
        }
        chat.turns.insert(
            1,
            Turn::Tool {
                id: ocean_agent_sdk::ToolCallId::new_v4(),
                name: "boom".to_string(),
                args: String::new(),
                output: "failed".to_string(),
                status: ToolStatus::Err,
                diff: None,
            },
        );
        let screen = render_chat_to_string(&mut chat, 90, 20);
        assert!(screen.contains("boom"), "error card must never hide");
        assert!(screen.contains("earlier tools"), "ok cards still compact");
    }

    #[test]
    fn collapsed_diff_body_renders_only_at_tail() {
        let edit_turn = || Turn::Tool {
            id: ocean_agent_sdk::ToolCallId::new_v4(),
            name: "edit".to_string(),
            args: String::new(),
            output: String::new(),
            status: ToolStatus::Ok,
            diff: Some(crate::shell::diff::string_rows("old_alpha\n", "new_beta\n")),
        };
        // Tail edit: the hunk stays visible for live feedback.
        let mut tail = ChatComponent::default();
        tail.turns.push(edit_turn());
        let screen = render_chat_to_string(&mut tail, 90, 16);
        assert!(screen.contains("new_beta"), "tail diff shows its rows");
        // Followed by assistant text: the card compacts to a one-line summary.
        let mut done = ChatComponent::default();
        done.turns.push(edit_turn());
        done.turns.push(Turn::Assistant("done".to_string()));
        let screen = render_chat_to_string(&mut done, 90, 16);
        assert!(
            !screen.contains("new_beta"),
            "non-tail diff must compact: {screen:?}"
        );
        assert!(
            screen.contains("diff +1"),
            "compacted card summarizes the hunk: {screen:?}"
        );
    }

    #[test]
    fn error_notice_render_strips_terminal_control_chars() {
        let mut chat = ChatComponent::default();
        chat.turns.push(Turn::ErrorNotice {
            note: "✗ turn failed — raw\tbad\x1b[31m\rline".into(),
        });

        let screen = render_chat_to_string(&mut chat, 80, 12);

        assert!(
            !screen.contains('\t'),
            "rendered notice must not contain tabs"
        );
        assert!(
            !screen.contains('\x1b'),
            "rendered notice must not contain ESC"
        );
        assert!(
            !screen.contains('\r'),
            "rendered notice must not contain CR"
        );
        assert!(
            screen.contains("raw    bad[31mline"),
            "sanitized notice should preserve readable text and expand tabs, got: {screen:?}"
        );
    }

    // ── sanitize_line on user prompt render path ─────────────────────────

    #[test]
    fn user_prompt_render_strips_control_chars() {
        let mut chat = ChatComponent::default();
        chat.turns
            .push(Turn::User("hey\x1b[31mthere\tbad\rline".into()));

        let screen = render_chat_to_string(&mut chat, 80, 12);

        assert!(!screen.contains('\t'), "user prompt must not contain tabs");
        assert!(!screen.contains('\x1b'), "user prompt must not contain ESC");
        assert!(!screen.contains('\r'), "user prompt must not contain CR");
        assert!(
            screen.contains("hey[31mthere    badline"),
            "sanitized user prompt should preserve readable text, got: {screen:?}"
        );
    }

    // ── TurnSendFailed prefix ────────────────────────────────────────────

    #[test]
    fn turn_send_failed_connect_uses_daemon_prefix() {
        let mut chat = ChatComponent::default();
        chat.update(&Action::TurnSendFailed {
            prompt: "hi".into(),
            err: "tcp connect error: Connection refused (os error 61)".into(),
        });
        assert_eq!(chat.turns.len(), 1, "should push one Assistant turn");
        let Turn::Assistant(msg) = &chat.turns[0] else {
            panic!("expected Assistant turn");
        };
        assert!(
            msg.contains("couldn't reach the daemon"),
            "connect error should use daemon prefix, got: {msg}"
        );
    }

    #[test]
    fn turn_send_failed_non_connect_uses_neutral_prefix() {
        let mut chat = ChatComponent::default();
        chat.update(&Action::TurnSendFailed {
            prompt: "hi".into(),
            err: "turn: HTTP 401 Unauthorized".into(),
        });
        assert_eq!(chat.turns.len(), 1, "should push one Assistant turn");
        let Turn::Assistant(msg) = &chat.turns[0] else {
            panic!("expected Assistant turn");
        };
        assert!(
            msg.contains("turn could not start"),
            "non-connect error should use neutral prefix, got: {msg}"
        );
        assert!(
            !msg.contains("couldn't reach the daemon"),
            "non-connect error must not use daemon prefix, got: {msg}"
        );
    }
}
