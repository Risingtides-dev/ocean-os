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
