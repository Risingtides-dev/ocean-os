//! Compact/expanded read-only renderer for Observatory execution topology.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent};
use ocean_observatory::ExecutionPhase;
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    symbols::Marker,
    widgets::{
        canvas::{Canvas, Context, Line as CanvasLine, Points},
        Block,
    },
    Frame,
};

use crate::shell::{
    action::{Action, Nav},
    component::Component,
    components::chat::sanitize_line,
    panel,
    spatial::{cell_aspect, Projection},
    theme,
    workflow_graph::{WorkflowGraph, WorkflowGraphCommand},
};

const ORBIT_STEP: f64 = 0.12;
const PAN_STEP: f64 = 0.15;
const DOLLY_STEP: f64 = 1.2;
const MAX_RENDER_NODES: usize = 256;
const MAX_RENDER_EDGES: usize = 1024;

#[derive(Default)]
pub struct WorkflowGraphComponent {
    pub graph: WorkflowGraph,
    pub focused: bool,
    /// True while drawn in the center; Enter from the compact rail sets this
    /// through the app rather than mutating layout from inside the component.
    pub expanded: bool,
}

impl WorkflowGraphComponent {
    pub fn state_label(&self) -> String {
        let active = self.graph.active_count();
        let hidden = self.graph.nodes.len().saturating_sub(MAX_RENDER_NODES);
        let connection = if self.graph.connected {
            ""
        } else {
            " · stale"
        };
        let hidden = if hidden > 0 {
            format!(" · +{hidden}")
        } else {
            String::new()
        };
        format!("{active} active{hidden}{connection}")
    }
}

impl Component for WorkflowGraphComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if !self.focused {
            return None;
        }
        let command = match key.code {
            KeyCode::Up | KeyCode::Right => WorkflowGraphCommand::MoveSelection(1),
            KeyCode::Down | KeyCode::Left => WorkflowGraphCommand::MoveSelection(-1),
            KeyCode::Char('h') => WorkflowGraphCommand::Orbit {
                yaw: -ORBIT_STEP,
                pitch: 0.0,
            },
            KeyCode::Char('l') => WorkflowGraphCommand::Orbit {
                yaw: ORBIT_STEP,
                pitch: 0.0,
            },
            KeyCode::Char('j') => WorkflowGraphCommand::Orbit {
                yaw: 0.0,
                pitch: ORBIT_STEP,
            },
            KeyCode::Char('k') => WorkflowGraphCommand::Orbit {
                yaw: 0.0,
                pitch: -ORBIT_STEP,
            },
            KeyCode::Char('H') => WorkflowGraphCommand::Pan {
                right: -PAN_STEP,
                up: 0.0,
            },
            KeyCode::Char('L') => WorkflowGraphCommand::Pan {
                right: PAN_STEP,
                up: 0.0,
            },
            KeyCode::Char('J') => WorkflowGraphCommand::Pan {
                right: 0.0,
                up: -PAN_STEP,
            },
            KeyCode::Char('K') => WorkflowGraphCommand::Pan {
                right: 0.0,
                up: PAN_STEP,
            },
            KeyCode::Char('+') | KeyCode::Char('=') => WorkflowGraphCommand::Zoom(1.0 / DOLLY_STEP),
            KeyCode::Char('-') => WorkflowGraphCommand::Zoom(DOLLY_STEP),
            KeyCode::Char('0') => WorkflowGraphCommand::ResetView,
            KeyCode::Char('f') if !self.expanded => return Some(Action::Navigate(Nav::Files)),
            KeyCode::Enter if !self.expanded => return Some(Action::ExpandWorkflowGraph),
            _ => return None,
        };
        Some(Action::WorkflowGraphCommand(command))
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let state = self.state_label();
        let body = panel::draw(frame, area, "FLOW", Some(&state), self.focused);
        if body.width == 0 || body.height == 0 {
            return;
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_DARK)),
            body,
        );
        if self.graph.nodes.is_empty() {
            panel::footer(frame, area, "waiting for execution");
            return;
        }

        let mut visible: Vec<usize> = (0..self.graph.nodes.len().min(MAX_RENDER_NODES)).collect();
        if self.graph.selected >= MAX_RENDER_NODES && self.graph.selected < self.graph.nodes.len() {
            visible.pop();
            visible.push(self.graph.selected);
        }
        let view = self.graph.camera.view();
        let projection = Projection::default();
        let aspect = cell_aspect(body);
        let projected: HashMap<usize, (f64, f64, f64)> = visible
            .iter()
            .filter_map(|&index| {
                let view_point = view.transform(self.graph.node_world(index)?);
                let (x, y) = projection.project(view_point)?;
                Some((index, (x, y, view_point.z)))
            })
            .collect();
        let mut order: Vec<(usize, f64)> = projected
            .iter()
            .map(|(&index, &(_, _, z))| (index, z))
            .collect();
        order.sort_by(|left, right| right.1.total_cmp(&left.1));

        let selected = self.graph.selected;
        let neighbors = self.graph.visible_neighbors_of_selected(&visible);
        let edges = self.graph.render_edges(&visible, MAX_RENDER_EDGES);
        let nodes = &self.graph.nodes;
        let width = body.width as usize;
        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([-1.0, 1.0])
            .y_bounds([-aspect, aspect])
            .paint(move |context: &mut Context| {
                for (parent, child) in &edges {
                    let (Some((ax, ay, _)), Some((bx, by, _))) =
                        (projected.get(parent), projected.get(child))
                    else {
                        continue;
                    };
                    context.draw(&CanvasLine {
                        x1: *ax,
                        y1: *ay,
                        x2: *bx,
                        y2: *by,
                        color: if *parent == selected || *child == selected {
                            theme::CYAN
                        } else {
                            theme::BG_HL
                        },
                    });
                }
                context.layer();
                for (index, _) in &order {
                    let Some((x, y, _)) = projected.get(index).copied() else {
                        continue;
                    };
                    let color = if *index == selected {
                        theme::YELLOW
                    } else if neighbors.contains(index) {
                        theme::CYAN
                    } else {
                        phase_color(nodes[*index].phase)
                    };
                    context.draw(&Points {
                        coords: &[(x, y)],
                        color,
                    });
                }
                if let Some((x, y, _)) = projected.get(&selected).copied() {
                    if let Some(node) = nodes.get(selected) {
                        let column = (((x + 1.0) / 2.0) * width as f64).round().max(0.0) as usize;
                        let budget = width.saturating_sub(column + 1);
                        let clean = sanitize_line(&node.title());
                        let title = panel::fit_cells(&clean, budget);
                        if !title.is_empty() {
                            context.print(
                                x,
                                y,
                                ratatui::text::Span::styled(
                                    format!(" {title}"),
                                    Style::default().fg(theme::YELLOW),
                                ),
                            );
                        }
                    }
                }
            });
        frame.render_widget(canvas, body);
        panel::footer(
            frame,
            area,
            if self.expanded {
                "arrows select · hjkl orbit · 0 reset"
            } else {
                "enter expand · f files"
            },
        );
    }
}

fn phase_color(phase: ExecutionPhase) -> Color {
    match phase {
        ExecutionPhase::Admitted => theme::YELLOW,
        ExecutionPhase::Running => theme::BLUE,
        ExecutionPhase::Finished => theme::GREEN,
        ExecutionPhase::Error => theme::RED,
        ExecutionPhase::Canceled => theme::COMMENT,
        ExecutionPhase::TimedOut => theme::MAGENTA,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn navigation_emits_an_action_without_mutating_component_state() {
        let mut component = WorkflowGraphComponent {
            focused: true,
            ..WorkflowGraphComponent::default()
        };
        let theta = component.graph.camera.theta;
        let action = component
            .handle_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE))
            .expect("workflow command");
        assert!(matches!(
            action,
            Action::WorkflowGraphCommand(WorkflowGraphCommand::Orbit { .. })
        ));
        assert_eq!(component.graph.camera.theta, theta);
    }
}
