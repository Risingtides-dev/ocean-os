//! GraphComponent — the project wikilink/import graph. Wraps `shell::graph::ProjectGraph`
//! (harvested from CTRL). Scans lazily on first view; renders the unit-radius
//! node cloud through the `shell::spatial` pipeline (world → view → perspective
//! NDC → braille Canvas). Arrows move selection, hjkl orbit the camera, HJKL
//! pan it along its basis, +/- dolly, 0 resets, Enter opens the file.

use std::path::PathBuf;

use crossterm::event::{KeyCode, KeyEvent};
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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::shell::{
    action::Action,
    component::Component,
    components::chat::sanitize_line,
    graph::ProjectGraph,
    panel,
    spatial::{cell_aspect, Projection},
    theme,
};

/// Orbit step per keypress, radians.
const ORBIT_STEP: f64 = 0.12;
/// Pan step per keypress, world units (the node cloud has unit radius).
const PAN_STEP: f64 = 0.15;
/// Dolly factor per keypress; `+` divides (closer), `-` multiplies (farther).
const DOLLY_STEP: f64 = 1.2;

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

    /// Re-root the graph at a new project directory; drops the cached scan so it
    /// lazily re-scans the new root on the next draw.
    pub fn set_root(&mut self, root: PathBuf) {
        if self.root != root {
            self.root = root;
            self.graph = None;
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
            // Orbit: h/l yaw, j/k pitch (polar-clamped in Camera::orbit).
            KeyCode::Char('h') => g.camera.orbit(-ORBIT_STEP, 0.0),
            KeyCode::Char('l') => g.camera.orbit(ORBIT_STEP, 0.0),
            KeyCode::Char('j') => g.camera.orbit(0.0, ORBIT_STEP),
            KeyCode::Char('k') => g.camera.orbit(0.0, -ORBIT_STEP),
            // Pan the camera target along its current right/up basis.
            KeyCode::Char('H') => g.camera.pan(-PAN_STEP, 0.0),
            KeyCode::Char('L') => g.camera.pan(PAN_STEP, 0.0),
            KeyCode::Char('J') => g.camera.pan(0.0, -PAN_STEP),
            KeyCode::Char('K') => g.camera.pan(0.0, PAN_STEP),
            // Dolly in/out within the camera's distance clamp.
            KeyCode::Char('+') | KeyCode::Char('=') => g.camera.zoom(1.0 / DOLLY_STEP),
            KeyCode::Char('-') => g.camera.zoom(DOLLY_STEP),
            KeyCode::Char('0') => g.reset_view(),
            KeyCode::Enter => return g.selected_path().map(Action::OpenFile),
            _ => {}
        }
        None
    }

    fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.ensure();
        let body = panel::draw(frame, area, "GRAPH", None, self.focused);
        if body.width == 0 {
            return;
        }
        // Constellation floats on the dark void.
        frame.render_widget(
            Block::default().style(Style::default().bg(theme::BG_DARK)),
            body,
        );

        let Some(g) = self.graph.as_ref() else { return };
        if g.bounds3d().is_none() {
            return;
        }

        let view = g.camera.view();
        let proj = Projection::default();
        let aspect = cell_aspect(body);

        // Project every node once: NDC position + view-space depth. `None`
        // marks a node culled at/behind the near plane — never drawn.
        let projected: Vec<Option<(f64, f64, f64)>> = (0..g.nodes.len())
            .map(|i| {
                let v = view.transform(g.node_world(i)?);
                let (x, y) = proj.project(v)?;
                Some((x, y, v.z))
            })
            .collect();

        // Painter's algorithm: visible nodes sorted far-to-near by view depth,
        // so nearer nodes overwrite farther ones in the braille grid.
        let mut order: Vec<(usize, f64)> = projected
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.map(|(_, _, z)| (i, z)))
            .collect();
        order.sort_by(|a, b| b.1.total_cmp(&a.1));

        let neighbors = g.neighbors_of_selected();
        let sel = g.selected;
        let width = body.width;

        let canvas = Canvas::default()
            .marker(Marker::Braille)
            .x_bounds([-1.0, 1.0])
            .y_bounds([-aspect, aspect])
            .paint(move |ctx: &mut Context| {
                // Edges first (beneath nodes), only when both endpoints
                // survive near-plane culling.
                for &(a, b) in &g.edges {
                    let (Some(&Some((ax, ay, _))), Some(&Some((bx, by, _)))) =
                        (projected.get(a), projected.get(b))
                    else {
                        continue;
                    };
                    let lit = a == sel || b == sel;
                    ctx.draw(&CanvasLine {
                        x1: ax,
                        y1: ay,
                        x2: bx,
                        y2: by,
                        color: if lit { theme::CYAN } else { theme::BG_HL },
                    });
                }
                ctx.layer();
                // Nodes far-to-near.
                for &(i, _) in &order {
                    let Some(Some((x, y, _))) = projected.get(i).copied() else {
                        continue;
                    };
                    let color = if i == sel {
                        theme::YELLOW
                    } else if neighbors.contains(&i) {
                        theme::CYAN
                    } else {
                        g.nodes
                            .get(i)
                            .map(|n| node_color(n.kind))
                            .unwrap_or(theme::COMMENT)
                    };
                    ctx.draw(&Points {
                        coords: &[(x, y)],
                        color,
                    });
                }
                // Selected node's title — the only text on this surface, and
                // only while the node itself projects visibly. Titles are
                // file-derived (untrusted for layout): sanitized and clamped
                // to the cells remaining right of the node.
                if let Some(Some((x, y, _))) = projected.get(sel).copied() {
                    if let Some(n) = g.nodes.get(sel) {
                        let col = (((x + 1.0) / 2.0) * f64::from(width)).round().max(0.0) as usize;
                        let budget = (width as usize).saturating_sub(col + 1);
                        let title = clamp_title(&n.title, budget);
                        if !title.is_empty() {
                            ctx.print(
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
    }
}

fn node_color(kind: crate::shell::graph::NodeKind) -> Color {
    use crate::shell::graph::NodeKind::*;
    match kind {
        Markdown => theme::GREEN,
        Source => theme::BLUE,
        Config => theme::MAGENTA,
        Other => theme::COMMENT,
    }
}

/// File-derived titles are untrusted for layout: strip control characters via
/// the chat-surface sanitizer, then hard-clamp by display-cell width so wide
/// glyphs cannot overrun the panel row.
fn clamp_title(raw: &str, max_cells: usize) -> String {
    let clean = sanitize_line(raw);
    if UnicodeWidthStr::width(clean.as_str()) <= max_cells {
        return clean;
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in clean.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > max_cells {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}
