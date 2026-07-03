//! ChatComponent — the native agent surface. Re-houses the PM room's streaming
//! model (structured blocks: text, thinking, tool calls) onto the component
//! architecture. Phase 1 covers the core loop: compose a prompt, submit it,
//! render streamed assistant text / thinking / tool-call status.
//!
//! ponytail: permission prompts, collapsible thinking pills, diff snippets, and
//! markdown/syntax rendering land in later phases — noted, not built yet.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ocean_agent_sdk::{AgentTurnEvent, ToolCallId};
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
}

impl Component for ChatComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        match (key.code, key.modifiers) {
            (KeyCode::Enter, _) => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                self.input.clear();
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

    fn update(&mut self, action: &Action) -> Option<Action> {
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
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(2)])
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
                    for l in s.lines() {
                        lines.push(Line::from(Span::styled(
                            l.to_string(),
                            Style::default().fg(theme::FG),
                        )));
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
        let total = lines.len() as u16;
        let scroll = total.saturating_sub(body.height);
        frame.render_widget(
            Paragraph::new(lines)
                .style(Style::default().bg(theme::SLATE))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            body,
        );
        let footer_hint = if self.busy {
            " streaming…"
        } else {
            " ⏎ send"
        };
        panel::footer(frame, chunks[0], footer_hint);

        // ── composer: highlight bed with an accent bar + block cursor ────────
        let comp = chunks[1];
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_HL)),
            comp,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                g("▎", "|"),
                Style::default().fg(if self.busy { theme::COMMENT } else { theme::CYAN }),
            ))
            .style(Style::default().bg(theme::BG_HL)),
            Rect::new(comp.x, comp.y, 1, comp.height.min(1)),
        );
        let input_fg = if self.busy { theme::COMMENT } else { theme::FG };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(self.input.clone(), Style::default().fg(input_fg)),
                Span::styled(g("▏", "_"), Style::default().fg(theme::CYAN)),
            ]))
            .style(Style::default().bg(theme::BG_HL)),
            Rect::new(
                comp.x + 2,
                comp.y,
                comp.width.saturating_sub(2),
                comp.height.min(1),
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

    #[test]
    fn foreign_extension_is_ignored() {
        let mut chat = ChatComponent::default();
        chat.update(&extension("longhouse", json!({ "note": "not for us" })));
        chat.update(&extension("advisor", json!({ "note": 42 })));
        assert!(chat.turns.is_empty());
    }
}
