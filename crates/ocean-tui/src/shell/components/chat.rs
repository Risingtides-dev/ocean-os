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
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Frame,
};

use crate::shell::{action::Action, component::Component};

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
            .constraints([Constraint::Min(3), Constraint::Length(3)])
            .split(area);

        // Transcript (bottom-anchored via scroll offset).
        let mut lines: Vec<Line> = Vec::new();
        for turn in &self.turns {
            match turn {
                Turn::User(s) => {
                    lines.push(Line::from(Span::styled(
                        format!("› {s}"),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    )));
                }
                Turn::Assistant(s) => {
                    for l in s.lines() {
                        lines.push(Line::from(l.to_string()));
                    }
                }
                Turn::Thinking(s) => {
                    lines.push(Line::from(Span::styled(
                        format!("  thinking ({} chars)", s.len()),
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                    )));
                }
                Turn::Tool { name, status, .. } => {
                    let (mark, color) = match status {
                        ToolStatus::Running => ("◐", Color::Yellow),
                        ToolStatus::Ok => ("✓", Color::Green),
                        ToolStatus::Err => ("✗", Color::Red),
                    };
                    lines.push(Line::from(Span::styled(
                        format!("  {mark} {name}"),
                        Style::default().fg(color),
                    )));
                }
                Turn::Advisor {
                    note,
                    severity,
                    model,
                } => {
                    // Color the card by severity: info = blue/gray, concern =
                    // amber, blocker = red. Set it off with a rule + label so it
                    // reads as an aside, not the agent's own text.
                    let accent = match severity.as_str() {
                        "blocker" => Color::Red,
                        "concern" => Color::Rgb(255, 176, 0), // amber
                        _ => Color::Rgb(120, 144, 168),       // muted blue/gray
                    };
                    let mut header: Vec<Span> = vec![Span::styled(
                        format!("  ⚑ advisor ({severity})"),
                        Style::default().fg(accent).add_modifier(Modifier::BOLD),
                    )];
                    if !model.is_empty() {
                        header.push(Span::styled(
                            format!("  · {model}"),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::DIM),
                        ));
                    }
                    lines.push(Line::from(header));
                    for l in note.lines() {
                        lines.push(Line::from(vec![
                            Span::styled("  │ ", Style::default().fg(accent)),
                            Span::styled(l.to_string(), Style::default().fg(accent)),
                        ]));
                    }
                }
            }
            lines.push(Line::from(""));
        }
        let total = lines.len() as u16;
        let view_h = chunks[0].height.saturating_sub(2);
        let scroll = total.saturating_sub(view_h);
        let title = match &self.model {
            Some(m) => format!(" ocean · {m} "),
            None => " ocean ".to_string(),
        };
        frame.render_widget(
            Paragraph::new(lines)
                .block(Block::default().borders(Borders::ALL).title(title))
                .wrap(Wrap { trim: false })
                .scroll((scroll, 0)),
            chunks[0],
        );

        // Composer.
        let prompt_style = if self.busy {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::White)
        };
        let hint = if self.busy { " (streaming…) " } else { " compose " };
        frame.render_widget(
            Paragraph::new(format!("{}▏", self.input))
                .style(prompt_style)
                .block(Block::default().borders(Borders::ALL).title(hint)),
            chunks[1],
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
