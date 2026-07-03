//! GraphComponent — the project wikilink/import graph. Wraps `shell::graph::ProjectGraph`
//! (harvested from CTRL). Scans lazily on first view; renders nodes + edges on a
//! ratatui Canvas. Arrows move selection, +/- zoom, hjkl pan, Enter opens the file.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::Rect,
    style::{Color, Style},
    symbols::Marker,
    widgets::{
        canvas::{Canvas, Context, Line as CanvasLine, Points},
        Block, Borders,
    },
    Frame,
};

use crate::shell::{action::Action, component::Component, graph::ProjectGraph};

pub struct GraphComponent {
    root: PathBuf,
    graph: Option<ProjectGraph>,
    pub focused: bool,
}

impl GraphComponent {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            graph: None,
            focused: false,
        }
    }

    /// Scan on first draw so the graph doesn't cost anything until viewed.
    fn ensure(&mut self) {
        if self.graph.is_none() {
            self.graph = Some(ProjectGraph::scan(&self.root));
        }
    }
}

impl Component for GraphComponent {
    fn handle_key(&mut self, key: KeyEvent) -> Option<Action> {
        if !self.focused {
            return None;
        }
        let g = self.graph.as_mut()?;
        match key.code {
            KeyCode::Up | KeyCode::Right => g.move_sel(1),
            KeyCode::Down | KeyCode::Left => g.move_sel(-1),
            KeyCode::Char('+') | KeyCode::Char('=') => g.zoom_by(1.2),
            KeyCode::Char('-') => g.zoom_by(1.0 / 1.2),
            KeyCode::Char('h') => g.pan(-4.0, 0.0),
            KeyCode::Char('l') => g.pan(4.0, 0.0),
            KeyCode::Char('j') => g.pan(0.0, -4.0),
            KeyCode::Char('k') => g.pan(0.0, 4.0),
            KeyCode::Char('0') => g.reset_view(),
            KeyCode::Enter => return g.selected_path().map(Action::OpenFile),
            _ => {}
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.ensure();
        let border = if self.focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let g = self.graph.as_ref();
        let label = g.map(|g| g.selected_label()).unwrap_or_default();
        let count = g.map(|g| g.nodes.len()).unwrap_or(0);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(format!(" graph · {count} nodes · {label} "));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let Some(g) = self.graph.as_ref() else { return };
        let Some((min_x, max_x, min_y, max_y)) = g.bounds() else {
            return;
        };
        // Auto-fit the logical bounds to the canvas, then apply user zoom.
        let pad = 2.0f64;
        let span_x = ((max_x - min_x) as f64).max(1.0);
        let span_y = ((max_y - min_y) as f64).max(1.0);
        let zoom = g.zoom as f64;
        let (cx, cy) = ((min_x + max_x) as f64 / 2.0, (min_y + max_y) as f64 / 2.0);
        let half_x = (span_x / 2.0 + pad) / zoom;
        let half_y = (span_y / 2.0 + pad) / zoom;
        let pan_x = g.pan_x as f64;
        let pan_y = g.pan_y as f64;

        let neighbors = g.neighbors_of_selected();
        let sel = g.selected;

        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([cx - half_x - pan_x, cx + half_x - pan_x])
            .y_bounds([cy - half_y - pan_y, cy + half_y - pan_y])
            .paint(move |ctx: &mut Context| {
                // edges
                for &(a, b) in &g.edges {
                    let (Some(na), Some(nb)) = (g.nodes.get(a), g.nodes.get(b)) else {
                        continue;
                    };
                    let lit = a == sel || b == sel;
                    ctx.draw(&CanvasLine {
                        x1: na.x as f64,
                        y1: na.y as f64,
                        x2: nb.x as f64,
                        y2: nb.y as f64,
                        color: if lit { Color::Cyan } else { Color::DarkGray },
                    });
                }
                ctx.layer();
                // nodes
                for (i, n) in g.nodes.iter().enumerate() {
                    let color = if i == sel {
                        Color::Yellow
                    } else if neighbors.contains(&i) {
                        Color::Cyan
                    } else {
                        node_color(n.kind)
                    };
                    ctx.draw(&Points {
                        coords: &[(n.x as f64, n.y as f64)],
                        color,
                    });
                }
                // selected label
                if let Some(n) = g.nodes.get(sel) {
                    ctx.print(
                        n.x as f64,
                        n.y as f64,
                        ratatui::text::Span::styled(
                            format!(" {}", n.title),
                            Style::default().fg(Color::Yellow),
                        ),
                    );
                }
            });
        frame.render_widget(canvas, inner);
    }
}

fn node_color(kind: crate::shell::graph::NodeKind) -> Color {
    use crate::shell::graph::NodeKind::*;
    match kind {
        Markdown => Color::Green,
        Source => Color::Blue,
        Config => Color::Magenta,
        Other => Color::Gray,
    }
}
