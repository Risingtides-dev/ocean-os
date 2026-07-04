//! ChatComponent — the native agent surface. Re-houses the PM room's streaming
//! model (structured blocks: text, thinking, tool calls) onto the component
//! architecture, plus: permission approval cards (⌃Y allow / ⌃N deny, the
//! OCEAN-185 gated flow), markdown-lite rendering (headings, fences, bullets,
//! inline code), multi-line input (⌃J newline), and wheel/PageUp scrollback.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ocean_agent_sdk::{AgentTurnEvent, ToolCallId};
use ocean_core::{OceanEvent, PermissionId};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph, Wrap},
    Frame,
};

use crate::shell::{
    action::Action,
    component::Component,
    panel,
    theme::{self, g},
};

/// One rendered unit of transcript.
enum Turn {
    /// Operator's prompt.
    User(String),
    /// Assistant visible text (accumulates deltas).
    Assistant(String),
    /// Extended-thinking text (accumulates deltas).
    Thinking(String),
    /// A tool call: keyed by call id, with name + streamed output + status.
    Tool {
        id: ToolCallId,
        name: String,
        output: String,
        status: ToolStatus,
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

#[derive(Default)]
pub struct ChatComponent {
    turns: Vec<Turn>,
    input: String,
    model: Option<String>,
    busy: bool,
    /// Scrollback offset in lines from the bottom (0 = stick to live tail).
    scroll_back: usize,
    pub focused: bool,
}

impl ChatComponent {
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
}

/// Markdown-lite: style one source line. Fence state is carried by the caller
/// so code blocks render on the dark bed with plain text.
fn md_line(l: &str, in_fence: &mut bool) -> Line<'static> {
    let t = l.trim_start();
    if t.starts_with("```") {
        *in_fence = !*in_fence;
        return Line::from(Span::styled(
            l.to_string(),
            Style::default().fg(theme::COMMENT).bg(theme::BG_DARK),
        ));
    }
    if *in_fence {
        return Line::from(Span::styled(
            l.to_string(),
            Style::default().fg(theme::CYAN).bg(theme::BG_DARK),
        ));
    }
    if t.starts_with('#') {
        return Line::from(Span::styled(
            l.to_string(),
            Style::default()
                .fg(theme::BLUE)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if t.starts_with("- ") || t.starts_with("* ") {
        return Line::from(vec![
            Span::styled(
                l[..l.len() - t.len()].to_string(),
                Style::default().fg(theme::FG),
            ),
            Span::styled(format!("{} ", g("•", "-")), Style::default().fg(theme::CYAN)),
            Span::styled(t[2..].to_string(), Style::default().fg(theme::FG)),
        ]);
    }
    // inline `code` runs
    if l.contains('`') {
        let mut spans = Vec::new();
        for (i, seg) in l.split('`').enumerate() {
            if i % 2 == 1 {
                spans.push(Span::styled(
                    seg.to_string(),
                    Style::default().fg(theme::CYAN).bg(theme::BG_DARK),
                ));
            } else {
                spans.push(Span::styled(seg.to_string(), Style::default().fg(theme::FG)));
            }
        }
        return Line::from(spans);
    }
    Line::from(Span::styled(l.to_string(), Style::default().fg(theme::FG)))
}

impl Component for ChatComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
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
            (KeyCode::Enter, _) => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                self.input.clear();
                self.scroll_back = 0; // sending snaps to the live tail
                self.turns.push(Turn::User(text.clone()));
                self.busy = true;
                Some(Action::SubmitPrompt(text))
            }
            (KeyCode::Backspace, _) => {
                self.input.pop();
                None
            }
            (KeyCode::Char(c), m) if m == KeyModifiers::NONE || m == KeyModifiers::SHIFT => {
                self.input.push(c);
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
                    self.turns.push(Turn::Tool {
                        id: call.id.clone(),
                        name: call.name.to_string(),
                        output: String::new(),
                        status: ToolStatus::Running,
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

        // ── transcript panel in the CTRL skin ────────────────────────────────
        let pill = self.model.clone();
        let body = panel::draw(frame, chunks[0], "OCEAN", pill.as_deref(), self.focused);

        // Transcript lines (bottom-anchored via scroll offset).
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
                    let mut in_fence = false;
                    for l in s.lines() {
                        lines.push(md_line(l, &mut in_fence));
                    }
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
                Turn::Tool { name, status, .. } => {
                    let (mark, color) = match status {
                        ToolStatus::Running => (g("◐", "*"), theme::YELLOW),
                        ToolStatus::Ok => (g("✓", "+"), theme::GREEN),
                        ToolStatus::Err => (g("✗", "x"), theme::RED),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {mark} "), Style::default().fg(color)),
                        Span::styled(name.clone(), Style::default().fg(theme::COMMENT)),
                    ]));
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
        let footer_hint = if self.scroll_back > 0 {
            format!(" ↑{} lines back · PgDn to tail", self.scroll_back)
        } else if self.busy {
            " streaming…".to_string()
        } else {
            " ⏎ send · ⌃J newline".to_string()
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn md_fence_toggles_and_headings_style() {
        let mut fence = false;
        md_line("```rust", &mut fence);
        assert!(fence, "opening fence enters code mode");
        md_line("let x = 1;", &mut fence);
        assert!(fence, "code lines keep fence state");
        md_line("```", &mut fence);
        assert!(!fence, "closing fence exits code mode");
        let heading = md_line("# Title", &mut fence);
        assert!(!fence);
        assert_eq!(heading.spans.len(), 1);
    }

    #[test]
    fn foreign_extension_is_ignored() {
        let mut chat = ChatComponent::default();
        chat.update(&extension("longhouse", json!({ "note": "not for us" })));
        chat.update(&extension("advisor", json!({ "note": 42 })));
        assert!(chat.turns.is_empty());
    }
}
