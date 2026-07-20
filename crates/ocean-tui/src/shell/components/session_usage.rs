//! Session-scoped provider context history for the mutable right-rail surface.
//!
//! This projection records only daemon-reported final-request context usage from
//! finished turns. It never substitutes cumulative input tokens or estimates,
//! and a stream gap is shown as partial rather than silently claiming complete
//! session history.

use std::collections::VecDeque;

use ocean_agent_sdk::{AgentSessionId, AgentTurnEvent, AgentTurnId};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{Block, Paragraph, Sparkline},
    Frame,
};

use crate::shell::{
    action::Action, component::Component, components::chat::sanitize_line, panel, theme,
};

const MAX_SAMPLES: usize = 32;

#[derive(Clone, Debug)]
struct UsageSample {
    turn_id: AgentTurnId,
    model: Option<String>,
    used_tokens: u64,
    context_window: u64,
}

impl UsageSample {
    fn percent(&self) -> u64 {
        self.used_tokens
            .min(self.context_window)
            .saturating_mul(100)
            .checked_div(self.context_window)
            .unwrap_or(0)
    }
}

#[derive(Default)]
pub struct SessionUsageComponent {
    session_id: Option<AgentSessionId>,
    turn_id: Option<AgentTurnId>,
    turn_model: Option<String>,
    samples: VecDeque<UsageSample>,
    partial: bool,
    pub focused: bool,
}

impl SessionUsageComponent {
    fn bind(&mut self, session_id: Option<AgentSessionId>) {
        if self.session_id != session_id {
            self.session_id = session_id;
            self.turn_id = None;
            self.turn_model = None;
            self.samples.clear();
            self.partial = false;
        }
    }

    fn apply_event(&mut self, event: &AgentTurnEvent) {
        let Some(bound) = self.session_id else {
            return;
        };
        if event.session_id() != Some(bound) {
            return;
        }
        match event {
            AgentTurnEvent::TurnStarted { turn_id, model, .. } => {
                if self.turn_id.is_some_and(|current| current != *turn_id) {
                    self.partial = true;
                }
                self.turn_id = Some(*turn_id);
                self.turn_model.clone_from(model);
            }
            AgentTurnEvent::ModelRerouted {
                turn_id, effective, ..
            } if self.turn_id == Some(*turn_id) => {
                self.turn_model = Some(effective.clone());
            }
            AgentTurnEvent::TurnFinished {
                turn_id,
                context_usage,
                ..
            } if self.turn_id == Some(*turn_id) => {
                if let Some(usage) = context_usage
                    .as_ref()
                    .filter(|usage| usage.context_window > 0)
                {
                    let sample = UsageSample {
                        turn_id: *turn_id,
                        model: self.turn_model.clone(),
                        used_tokens: usage.used_tokens,
                        context_window: usage.context_window,
                    };
                    if self.samples.back().map(|sample| sample.turn_id) == Some(*turn_id) {
                        self.samples.pop_back();
                    }
                    self.samples.push_back(sample);
                    while self.samples.len() > MAX_SAMPLES {
                        self.samples.pop_front();
                    }
                }
                self.turn_id = None;
                self.turn_model = None;
            }
            // A replayed terminal event for an already recorded turn is
            // idempotent. Any other finish without its start proves a gap,
            // but must not erase a different in-flight turn.
            AgentTurnEvent::TurnFinished { turn_id, .. }
                if !self.samples.iter().any(|sample| sample.turn_id == *turn_id) =>
            {
                self.partial = true;
            }
            _ => {}
        }
    }

    fn latest(&self) -> Option<&UsageSample> {
        self.samples.back()
    }

    #[cfg(test)]
    fn sample_count(&self) -> usize {
        self.samples.len()
    }
}

impl Component for SessionUsageComponent {
    fn update(&mut self, action: &Action) -> Option<Action> {
        match action {
            Action::SessionBound(id) => self.bind(Some(*id)),
            Action::ResumeSession { id, .. } => self.bind(Some(*id)),
            Action::NewSession | Action::NewSessionInProject { .. } => self.bind(None),
            Action::AgentStreamGap(id) if self.session_id == Some(*id) => {
                self.turn_id = None;
                self.turn_model = None;
                self.partial = true;
            }
            Action::AgentEvent(event) => self.apply_event(event),
            _ => {}
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let state = self
            .latest()
            .map(|sample| format!("{} turns · {}%", self.samples.len(), sample.percent()))
            .unwrap_or_else(|| "waiting".into());
        let body = panel::draw(frame, area, "USAGE", Some(&state), self.focused);
        if body.width == 0 || body.height == 0 {
            return;
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_DARK)),
            body,
        );

        let Some(latest) = self.latest() else {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        "No provider context sample yet.",
                        Style::default().fg(theme::COMMENT),
                    )),
                    Line::from(Span::styled(
                        "Waiting for a finished turn.",
                        Style::default().fg(theme::COMMENT),
                    )),
                ])
                .style(Style::default().bg(theme::BG_DARK)),
                body,
            );
            panel::footer(
                frame,
                area,
                if self.partial {
                    "partial · stream gap"
                } else {
                    "final requests · provider measured"
                },
            );
            return;
        };

        let detail_h = body.height.min(3);
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(detail_h)])
            .split(body);
        let data: Vec<u64> = self.samples.iter().map(UsageSample::percent).collect();
        frame.render_widget(
            Sparkline::default()
                .data(&data)
                .max(100)
                .bar_set(symbols::bar::NINE_LEVELS)
                .style(Style::default().fg(theme::CYAN).bg(theme::BG_DARK)),
            rows[0],
        );

        let model = latest
            .model
            .as_deref()
            .map(sanitize_line)
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| "model unknown".into());
        let detail = vec![
            Line::from(Span::styled(
                panel::fit_cells(&format!(" {}", model), rows[1].width as usize),
                Style::default()
                    .fg(theme::BLUE)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                panel::fit_cells(
                    &format!(" {}/{} tokens", latest.used_tokens, latest.context_window),
                    rows[1].width as usize,
                ),
                Style::default().fg(theme::FG),
            )),
            Line::from(Span::styled(
                panel::fit_cells(
                    &format!(" {}% · final request", latest.percent()),
                    rows[1].width as usize,
                ),
                Style::default().fg(theme::COMMENT),
            )),
        ];
        frame.render_widget(
            Paragraph::new(detail).style(Style::default().bg(theme::BG_DARK)),
            rows[1],
        );
        panel::footer(
            frame,
            area,
            if self.partial {
                "partial · stream gap"
            } else {
                "finished turns · provider measured"
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_agent_sdk::{AgentTurnStatus, ContextUsage};

    fn sid(value: u128) -> AgentSessionId {
        AgentSessionId(uuid::Uuid::from_u128(value))
    }

    fn tid(value: u128) -> AgentTurnId {
        AgentTurnId(uuid::Uuid::from_u128(value))
    }

    fn finished(session_id: AgentSessionId, turn_id: AgentTurnId) -> AgentTurnEvent {
        AgentTurnEvent::TurnFinished {
            session_id,
            turn_id,
            status: AgentTurnStatus::Completed,
            error: None,
            wall_ms: Some(100),
            output_tokens: Some(20),
            input_tokens: Some(40),
            cache_read_tokens: None,
            tokens_per_second: Some(200.0),
            context_usage: Some(ContextUsage {
                used_tokens: 50,
                context_window: 100,
                source: "provider_reported_final_round".into(),
                measured_at_ms: 1,
            }),
        }
    }

    #[test]
    fn records_only_correlated_provider_measurements() {
        let mut usage = SessionUsageComponent::default();
        let session_id = sid(1);
        usage.update(&Action::SessionBound(session_id));
        usage.update(&Action::AgentEvent(Box::new(finished(session_id, tid(2)))));
        assert_eq!(usage.sample_count(), 0);
        assert!(usage.partial);

        usage.update(&Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id,
            turn_id: tid(3),
            model: Some("model-a".into()),
        })));
        usage.update(&Action::AgentEvent(Box::new(finished(session_id, tid(3)))));
        assert_eq!(usage.sample_count(), 1);
        assert_eq!(usage.latest().map(UsageSample::percent), Some(50));
        assert_eq!(
            usage.latest().and_then(|sample| sample.model.as_deref()),
            Some("model-a")
        );
    }

    #[test]
    fn duplicate_finished_event_is_idempotent() {
        let mut usage = SessionUsageComponent::default();
        let session_id = sid(9);
        let turn_id = tid(10);
        usage.update(&Action::SessionBound(session_id));
        usage.update(&Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id,
            turn_id,
            model: None,
        })));
        usage.update(&Action::AgentEvent(Box::new(finished(session_id, turn_id))));
        usage.update(&Action::AgentEvent(Box::new(finished(session_id, turn_id))));

        assert_eq!(usage.sample_count(), 1);
        assert!(!usage.partial);
    }

    #[test]
    fn stream_gap_preserves_known_samples_but_marks_history_partial() {
        let mut usage = SessionUsageComponent::default();
        let session_id = sid(4);
        usage.update(&Action::SessionBound(session_id));
        usage.update(&Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id,
            turn_id: tid(5),
            model: None,
        })));
        usage.update(&Action::AgentEvent(Box::new(finished(session_id, tid(5)))));
        usage.update(&Action::AgentStreamGap(session_id));

        assert_eq!(usage.sample_count(), 1);
        assert!(usage.partial);
    }

    #[test]
    fn session_switch_clears_history() {
        let mut usage = SessionUsageComponent::default();
        let session_id = sid(6);
        usage.update(&Action::SessionBound(session_id));
        usage.update(&Action::AgentEvent(Box::new(AgentTurnEvent::TurnStarted {
            session_id,
            turn_id: tid(7),
            model: None,
        })));
        usage.update(&Action::AgentEvent(Box::new(finished(session_id, tid(7)))));
        usage.update(&Action::SessionBound(sid(8)));

        assert_eq!(usage.sample_count(), 0);
        assert!(!usage.partial);
    }
}
