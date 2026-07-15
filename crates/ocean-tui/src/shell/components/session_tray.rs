//! SessionComponentTray — session-bound components that live beneath FILES.
//!
//! The tray is a shell-level host, not part of the file tree. Its first adapter
//! projects confirmed `todo` tool effects for the currently bound session. The source
//! tool is intentionally session-scoped and non-durable: confirmed items stay
//! pinned across turns while the same session is bound, then clear on an
//! explicit todo clear, session switch/new session, daemon restart, or gap
//! invalidation.

use std::collections::HashMap;

use crate::shell::{
    action::Action,
    component::Component,
    components::chat::sanitize_line,
    panel,
    theme::{self, g},
};
use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent, AgentTurnId, ContextUsage, ToolCallId};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
    Frame,
};

const MIN_PANEL_H: u16 = 6;
const MAX_PANEL_H: u16 = 14;

#[derive(Debug, Clone, PartialEq, Eq)]
struct TodoItem {
    text: String,
    done: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TodoCommand {
    Add(String),
    Complete(usize),
    List,
    Clear,
}

impl TodoCommand {
    fn from_args(args: &serde_json::Value) -> Option<Self> {
        match args.get("action")?.as_str()? {
            "add" => Some(Self::Add(args.get("text")?.as_str()?.to_string())),
            "complete" => Some(Self::Complete(
                usize::try_from(args.get("index")?.as_u64()?).ok()?,
            )),
            "list" => Some(Self::List),
            "clear" => Some(Self::Clear),
            _ => None,
        }
    }
}

#[derive(Default)]
struct TodoProjection {
    items: Vec<TodoItem>,
    pending: HashMap<uuid::Uuid, TodoCommand>,
    uncertain: bool,
}

impl TodoProjection {
    fn clear(&mut self) {
        self.items.clear();
        self.pending.clear();
        self.uncertain = false;
    }

    fn is_visible(&self) -> bool {
        !self.items.is_empty() || !self.pending.is_empty() || self.uncertain
    }

    fn invalidate(&mut self) {
        let had_state = self.is_visible();
        self.items.clear();
        self.pending.clear();
        self.uncertain |= had_state;
    }

    fn finish(&mut self, call_id: &ToolCallId, ok: bool) -> bool {
        let Some(command) = self.pending.remove(&call_id.0) else {
            return false;
        };
        if !ok {
            return true;
        }
        match command {
            TodoCommand::Add(text) => self.items.push(TodoItem { text, done: false }),
            TodoCommand::Complete(index) => {
                let Some(item) = index.checked_sub(1).and_then(|i| self.items.get_mut(i)) else {
                    self.uncertain = true;
                    return true;
                };
                item.done = true;
            }
            TodoCommand::Clear => self.clear(),
            // The current tool returns an ambiguous human-formatted list. Do
            // not parse it into invented structured state.
            TodoCommand::List => {}
        }
        true
    }
}

pub struct SessionComponentTray {
    session_id: Option<AgentSessionId>,
    turn_id: Option<AgentTurnId>,
    todo: TodoProjection,
    /// Finished events omit the tool name, so remember whether each observed
    /// call was todo or non-todo before deciding that an orphan implies a gap.
    observed_calls: HashMap<uuid::Uuid, bool>,
    continuity_uncertain: bool,
    context_usage: Option<ContextUsage>,
}

impl SessionComponentTray {
    pub fn new() -> Self {
        Self {
            session_id: None,
            turn_id: None,
            todo: TodoProjection::default(),
            observed_calls: HashMap::new(),
            continuity_uncertain: false,
            context_usage: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.todo.is_visible() || self.context_usage.is_some()
    }

    pub fn desired_height(&self) -> u16 {
        let status_rows = u16::from(!self.todo.pending.is_empty() || self.todo.uncertain);
        let todo_rows = if self.todo.is_visible() {
            1u16.saturating_add(self.todo.items.len().min(u16::MAX as usize) as u16)
                .saturating_add(status_rows)
        } else {
            0
        };
        let context_rows = if self.context_usage.is_some() { 3 } else { 0 };
        todo_rows
            .saturating_add(context_rows)
            .saturating_add(3)
            .clamp(MIN_PANEL_H, MAX_PANEL_H)
    }

    fn bind(&mut self, session_id: Option<AgentSessionId>) {
        if self.session_id != session_id {
            self.session_id = session_id;
            self.turn_id = None;
            self.todo.clear();
            self.observed_calls.clear();
            self.continuity_uncertain = false;
            self.context_usage = None;
        }
    }

    fn begin_turn(&mut self, session_id: AgentSessionId, turn_id: AgentTurnId) {
        if self.session_id != Some(session_id) {
            return;
        }
        self.turn_id = Some(turn_id);
        // Confirmed todo items are session-scoped. Preserve them across turns,
        // but discard unresolved calls from the prior turn and clear an
        // uncertainty marker once a fresh authoritative boundary arrives.
        self.todo.pending.clear();
        self.todo.uncertain = false;
        self.observed_calls.clear();
        self.continuity_uncertain = false;
        // Keep the last completed measurement visible while the next turn runs.
        // It is timestamped and explicitly final-round provenance, so it cannot be
        // mistaken for a live estimate; replacing it only on a finished event also
        // avoids a distracting panel collapse/reopen cycle.
    }

    fn apply_event(&mut self, event: &AgentTurnEvent) {
        let Some(bound) = self.session_id else {
            return;
        };
        if event.session_id() != Some(bound) {
            return;
        }
        match event {
            AgentTurnEvent::TurnStarted {
                session_id,
                turn_id,
                ..
            } => self.begin_turn(*session_id, *turn_id),
            AgentTurnEvent::ToolCallStarted { turn_id, call, .. } => {
                // A replay gap may omit TurnStarted. Adopt the run so later
                // finishes correlate, but never present a partial projection
                // as complete.
                if self.turn_id != Some(*turn_id) {
                    self.turn_id = Some(*turn_id);
                    self.todo.clear();
                    self.todo.uncertain = true;
                    self.observed_calls.clear();
                    self.continuity_uncertain = true;
                }
                if self.continuity_uncertain {
                    self.todo.uncertain = true;
                }
                let is_todo = call.name == "todo";
                self.observed_calls.insert(call.id.0, is_todo);
                if is_todo {
                    if let Some(command) = TodoCommand::from_args(&call.args_json) {
                        self.todo.pending.insert(call.id.0, command);
                    }
                }
            }
            AgentTurnEvent::ToolCallFinished {
                turn_id,
                call_id,
                result,
                ..
            } => {
                if self.turn_id != Some(*turn_id) {
                    self.turn_id = Some(*turn_id);
                    self.todo.clear();
                    self.observed_calls.clear();
                    self.continuity_uncertain = true;
                    self.todo.uncertain = true;
                } else {
                    match self.observed_calls.remove(&call_id.0) {
                        Some(true) => {
                            if !self.todo.finish(call_id, result.ok) && result.ok {
                                self.todo.uncertain = true;
                            }
                        }
                        Some(false) => {}
                        None => {
                            self.continuity_uncertain = true;
                            self.todo.invalidate();
                            self.todo.uncertain = true;
                        }
                    }
                }
            }
            AgentTurnEvent::TurnFinished {
                turn_id,
                context_usage,
                ..
            } if self.turn_id == Some(*turn_id) => {
                self.context_usage.clone_from(context_usage);
                if !self.todo.pending.is_empty() {
                    self.todo.pending.clear();
                    self.todo.uncertain = true;
                    self.observed_calls.clear();
                }
            }
            _ => {}
        }
    }
}

impl Default for SessionComponentTray {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for SessionComponentTray {
    fn update(&mut self, action: &Action) -> Option<Action> {
        match action {
            Action::SessionBound(id) => self.bind(Some(*id)),
            Action::ResumeSession { id, .. } => self.bind(Some(*id)),
            Action::NewSession | Action::NewSessionInProject { .. } => self.bind(None),
            Action::AgentStreamGap(id) if self.session_id == Some(*id) => {
                self.continuity_uncertain = true;
                self.observed_calls.clear();
                self.todo.invalidate();
            }
            Action::AgentEvent(event) => self.apply_event(event),
            _ => {}
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let body = panel::draw(frame, area, "SESSION COMPONENT", None, false);
        if body.width == 0 || body.height == 0 {
            return;
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::SLATE)),
            body,
        );

        let mut lines = Vec::with_capacity(body.height as usize);
        if let Some(usage) = &self.context_usage {
            let used = usage.used_tokens.min(usage.context_window);
            let pct = used
                .saturating_mul(100)
                .checked_div(usage.context_window)
                .unwrap_or(0);
            lines.push(Line::from(Span::styled(
                fit_cells(" CONTEXT", body.width as usize),
                Style::default()
                    .fg(theme::BLUE)
                    .add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                fit_cells(
                    &format!(
                        " {}  {}/{}",
                        context_bar(pct, 10),
                        compact_tokens(usage.used_tokens),
                        compact_tokens(usage.context_window)
                    ),
                    body.width as usize,
                ),
                Style::default().fg(context_color(pct)),
            )));
            lines.push(Line::from(Span::styled(
                fit_cells(&format!(" {pct}% · provider measured"), body.width as usize),
                Style::default().fg(theme::COMMENT),
            )));
        }

        if self.todo.is_visible() && lines.len() < body.height as usize {
            lines.push(Line::from(Span::styled(
                fit_cells(" TODOS", body.width as usize),
                Style::default()
                    .fg(theme::BLUE)
                    .add_modifier(Modifier::BOLD),
            )));
        }
        let status = if self.todo.uncertain {
            Some((" state incomplete", theme::YELLOW))
        } else if !self.todo.pending.is_empty() {
            Some((" … updating", theme::COMMENT))
        } else {
            None
        };
        let used_body_rows = lines.len();
        let remaining = (body.height as usize).saturating_sub(used_body_rows);
        let mut item_rows = remaining.saturating_sub(usize::from(status.is_some()));
        let overflow = self.todo.items.len() > item_rows;
        if overflow {
            item_rows = item_rows.saturating_sub(1);
        }
        for item in self.todo.items.iter().take(item_rows) {
            let mark = if item.done { g("✓", "x") } else { "·" };
            let text = format!(" {mark} {}", item.text);
            let fg = if item.done { theme::COMMENT } else { theme::FG };
            lines.push(Line::from(Span::styled(
                fit_cells(&text, body.width as usize),
                Style::default().fg(fg),
            )));
        }
        if overflow {
            let hidden = self.todo.items.len().saturating_sub(item_rows);
            lines.push(Line::from(Span::styled(
                fit_cells(&format!(" … {hidden} more"), body.width as usize),
                Style::default().fg(theme::COMMENT),
            )));
        }
        if let Some((text, color)) = status {
            lines.push(Line::from(Span::styled(
                fit_cells(text, body.width as usize),
                Style::default().fg(color),
            )));
        }
        frame.render_widget(
            Paragraph::new(lines).style(Style::default().bg(theme::SLATE)),
            body,
        );
        panel::footer(frame, area, " session-scoped");
    }
}

fn context_bar(pct: u64, width: usize) -> String {
    let filled = ((pct.min(100) as usize) * width).div_ceil(100);
    format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(width.saturating_sub(filled))
    )
}

fn compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.0}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn context_color(pct: u64) -> ratatui::style::Color {
    if pct >= 90 {
        theme::RED
    } else if pct >= 75 {
        theme::YELLOW
    } else {
        theme::CYAN
    }
}

fn fit_cells(raw: &str, width: usize) -> String {
    panel::fit_cells(&sanitize_line(raw), width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_agent_sdk::{AgentTurnStatus, ToolCall, ToolResult};
    use serde_json::json;
    use unicode_width::UnicodeWidthStr;
    use uuid::Uuid;

    fn session(n: u128) -> AgentSessionId {
        AgentSessionId(Uuid::from_u128(n))
    }

    fn turn(n: u128) -> AgentTurnId {
        AgentTurnId(Uuid::from_u128(n))
    }

    fn call(n: u128) -> ToolCallId {
        ToolCallId(Uuid::from_u128(n))
    }

    fn started(
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        id: ToolCallId,
        args: serde_json::Value,
    ) -> Action {
        Action::AgentEvent(Box::new(AgentTurnEvent::ToolCallStarted {
            session_id,
            turn_id,
            call: ToolCall {
                id,
                name: "todo".into(),
                args_json: args,
            },
        }))
    }

    fn finished(
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        id: ToolCallId,
        ok: bool,
    ) -> Action {
        Action::AgentEvent(Box::new(AgentTurnEvent::ToolCallFinished {
            session_id,
            turn_id,
            call_id: id,
            result: ToolResult {
                ok,
                output: String::new(),
                metadata_json: None,
            },
        }))
    }

    fn begin(session_id: AgentSessionId, turn_id: AgentTurnId) -> Action {
        Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id,
            turn_id,
            model: None,
        }))
    }

    fn turn_finished(
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        context_usage: Option<ContextUsage>,
        status: AgentTurnStatus,
    ) -> Action {
        Action::AgentEvent(Box::new(AgentTurnEvent::TurnFinished {
            session_id,
            turn_id,
            status,
            error: None,
            wall_ms: Some(10),
            output_tokens: Some(5),
            input_tokens: Some(100),
            cache_read_tokens: None,
            tokens_per_second: Some(500.0),
            context_usage,
        }))
    }

    fn render(tray: &mut SessionComponentTray) -> String {
        let backend = ratatui::backend::TestBackend::new(30, MAX_PANEL_H);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| tray.draw(frame, frame.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in buf.area.top()..buf.area.bottom() {
            for x in buf.area.left()..buf.area.right() {
                out.push_str(buf.cell((x, y)).unwrap().symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn context_usage_is_truthful_and_absent_when_unknown() {
        let mut tray = SessionComponentTray::new();
        let sid = session(1);
        let tid = turn(2);
        tray.update(&Action::SessionBound(sid));
        tray.update(&begin(sid, tid));
        tray.update(&turn_finished(
            sid,
            tid,
            Some(ContextUsage {
                used_tokens: 150_000,
                context_window: 200_000,
                source: "provider_reported_final_round".into(),
                measured_at_ms: 123,
            }),
            AgentTurnStatus::Completed,
        ));
        let screen = render(&mut tray);
        assert!(screen.contains("CONTEXT"));
        assert!(screen.contains("150k/200k"));
        assert!(screen.contains("75% · provider measured"));

        tray.update(&begin(sid, turn(3)));
        tray.update(&turn_finished(
            sid,
            turn(3),
            None,
            AgentTurnStatus::Completed,
        ));
        assert!(
            !tray.is_visible(),
            "unknown measurement renders no estimate"
        );
    }

    #[test]
    fn context_helpers_clamp_and_use_semantic_thresholds() {
        assert_eq!(context_bar(0, 5), "░░░░░");
        assert_eq!(context_bar(101, 5), "█████");
        assert_eq!(compact_tokens(999), "999");
        assert_eq!(compact_tokens(128_000), "128k");
        assert_eq!(context_color(74), theme::CYAN);
        assert_eq!(context_color(75), theme::YELLOW);
        assert_eq!(context_color(90), theme::RED);
    }

    #[test]
    fn todo_projection_changes_only_after_confirmed_finish() {
        let mut tray = SessionComponentTray::new();
        let sid = session(1);
        let tid = turn(2);
        let cid = call(3);
        tray.update(&Action::SessionBound(sid));
        tray.update(&begin(sid, tid));
        tray.update(&started(
            sid,
            tid,
            cid.clone(),
            json!({"action": "add", "text": "ship it"}),
        ));
        assert!(tray.todo.items.is_empty());
        assert!(tray.is_visible(), "pending todo reveals the tray");

        tray.update(&finished(sid, tid, cid, true));
        assert_eq!(
            tray.todo.items,
            vec![TodoItem {
                text: "ship it".into(),
                done: false,
            }]
        );
    }

    #[test]
    fn failed_clear_does_not_mutate_confirmed_items() {
        let mut tray = SessionComponentTray::new();
        let sid = session(1);
        let tid = turn(2);
        tray.update(&Action::SessionBound(sid));
        tray.update(&started(
            sid,
            tid,
            call(1),
            json!({"action": "add", "text": "keep"}),
        ));
        tray.update(&finished(sid, tid, call(1), true));
        tray.update(&started(sid, tid, call(2), json!({"action": "clear"})));
        tray.update(&finished(sid, tid, call(2), false));
        assert_eq!(tray.todo.items.len(), 1);
    }

    #[test]
    fn new_turn_keeps_confirmed_todos_and_session_switch_clears_them() {
        let mut tray = SessionComponentTray::new();
        let sid = session(1);
        tray.update(&Action::SessionBound(sid));
        tray.update(&started(
            sid,
            turn(2),
            call(1),
            json!({"action": "add", "text": "old"}),
        ));
        tray.update(&finished(sid, turn(2), call(1), true));
        tray.update(&Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id: sid,
            turn_id: turn(3),
            model: None,
        })));
        assert_eq!(
            tray.todo.items,
            vec![TodoItem {
                text: "old".into(),
                done: false,
            }],
            "confirmed todo remains pinned for the bound session"
        );
        assert!(tray.is_visible());

        tray.update(&started(
            sid,
            turn(3),
            call(2),
            json!({"action": "add", "text": "new"}),
        ));
        tray.update(&finished(sid, turn(3), call(2), true));
        tray.update(&Action::SessionBound(session(9)));
        assert!(!tray.is_visible());
        assert_eq!(
            tray.todo.items,
            vec![],
            "session switch clears the projection"
        );

        // Switch back to session 1: the tray is a live projection, not
        // reconstructed history, so it must remain empty after re-bind.
        tray.update(&Action::SessionBound(sid));
        assert!(
            tray.todo.items.is_empty(),
            "re-binding session 1 must not reconstruct old todo state"
        );
        assert!(!tray.is_visible());
    }

    #[test]
    fn foreign_session_events_are_ignored() {
        let mut tray = SessionComponentTray::new();
        tray.update(&Action::SessionBound(session(1)));
        tray.update(&started(
            session(2),
            turn(3),
            call(4),
            json!({"action": "add", "text": "leak"}),
        ));
        assert!(!tray.is_visible());
    }

    #[test]
    fn incomplete_call_is_marked_uncertain_at_turn_end() {
        let mut tray = SessionComponentTray::new();
        let sid = session(1);
        let tid = turn(2);
        tray.update(&Action::SessionBound(sid));
        tray.update(&started(
            sid,
            tid,
            call(3),
            json!({"action": "add", "text": "maybe"}),
        ));
        tray.update(&Action::AgentEvent(Box::new(
            AgentTurnEvent::TurnFinished {
                session_id: sid,
                turn_id: tid,
                status: AgentTurnStatus::Completed,
                error: None,
                wall_ms: None,
                output_tokens: None,
                input_tokens: None,
                cache_read_tokens: None,
                tokens_per_second: None,
                context_usage: None,
            },
        )));
        assert!(tray.todo.uncertain);
        assert!(tray.is_visible());
    }

    #[test]
    fn stream_gap_discards_stale_items_until_fresh_turn_boundary() {
        let mut tray = SessionComponentTray::new();
        let sid = session(1);
        let tid = turn(2);
        tray.update(&Action::SessionBound(sid));
        tray.update(&begin(sid, tid));
        tray.update(&started(
            sid,
            tid,
            call(3),
            json!({"action": "add", "text": "stale"}),
        ));
        tray.update(&finished(sid, tid, call(3), true));
        tray.update(&Action::AgentStreamGap(sid));
        assert!(tray.todo.items.is_empty());
        assert!(tray.todo.uncertain);

        tray.update(&begin(sid, turn(4)));
        assert!(!tray.is_visible());
    }

    #[test]
    fn missing_turn_started_and_orphan_finish_mark_projection_incomplete() {
        let mut tray = SessionComponentTray::new();
        let sid = session(1);
        let tid = turn(2);
        tray.update(&Action::SessionBound(sid));
        tray.update(&started(
            sid,
            tid,
            call(3),
            json!({"action": "add", "text": "partial"}),
        ));
        tray.update(&finished(sid, tid, call(3), true));
        assert!(tray.todo.uncertain, "missing turn boundary is partial");

        tray.update(&finished(sid, tid, call(99), true));
        assert!(tray.todo.items.is_empty(), "orphan invalidates stale items");
        assert!(tray.todo.uncertain);
    }

    #[test]
    fn unknown_turn_orphan_finish_surfaces_incomplete_state() {
        let mut tray = SessionComponentTray::new();
        let sid = session(1);
        tray.update(&Action::SessionBound(sid));
        tray.update(&finished(sid, turn(8), call(9), true));
        assert!(tray.is_visible());
        assert!(tray.todo.uncertain);
    }

    #[test]
    fn ordinary_non_todo_finish_does_not_invalidate_projection() {
        let mut tray = SessionComponentTray::new();
        let sid = session(1);
        let tid = turn(2);
        tray.update(&Action::SessionBound(sid));
        tray.update(&begin(sid, tid));
        tray.update(&Action::AgentEvent(Box::new(
            AgentTurnEvent::ToolCallStarted {
                session_id: sid,
                turn_id: tid,
                call: ToolCall {
                    id: call(7),
                    name: "read".into(),
                    args_json: json!({}),
                },
            },
        )));
        tray.update(&finished(sid, tid, call(7), true));
        assert!(!tray.is_visible());
    }

    #[test]
    fn capped_panel_reserves_overflow_and_incomplete_rows() {
        let mut tray = SessionComponentTray::new();
        tray.todo.items = (0..12)
            .map(|i| TodoItem {
                text: format!("item {i}"),
                done: false,
            })
            .collect();
        tray.todo.uncertain = true;
        let screen = render(&mut tray);
        assert!(
            screen.contains("more"),
            "overflow remains visible: {screen}"
        );
        assert!(
            screen.contains("state incomplete"),
            "honesty state remains visible: {screen}"
        );
    }

    #[test]
    fn fit_cells_sanitizes_controls_and_clamps_contextual_text() {
        let fitted = fit_cells("a\tb\u{1b}界tail", 8);
        assert!(!fitted.contains('\t') && !fitted.contains('\u{1b}'));
        assert!(UnicodeWidthStr::width(fitted.as_str()) <= 8);
        assert_eq!(fit_cells(" 👩‍💻 tail", 4), " 👩‍💻 ");
    }
}
