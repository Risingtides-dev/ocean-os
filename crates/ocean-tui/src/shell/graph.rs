//! Project graph data model for ctrl's main-pane graph view.
//!
//! This is intentionally lightweight and self-contained. It borrows the *idea*
//! of graf/Obsidian-style wikilink graphs, but implements ctrl's own scanner so
//! the feature can live in the editor pane without pulling in another app.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::shell::spatial::{Camera, Vec3};

const MAX_NODES: usize = 320;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Markdown,
    Source,
    Config,
    Other,
}

/// Axis-aligned 3D bounding box of the laid-out graph (min/max corners).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds3d {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

#[derive(Clone, Debug)]
pub struct GraphNode {
    pub path: PathBuf,
    pub rel: String,
    pub title: String,
    pub kind: NodeKind,
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Clone, Debug)]
pub struct ProjectGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(usize, usize)>,
    pub selected: usize,
    /// Orbit camera (yaw/pitch/distance) around the origin-centered,
    /// unit-radius normalized node cloud; the default pose frames the whole
    /// cloud with margin.
    pub camera: Camera,
    pub truncated: bool,
}

#[derive(Clone)]
struct FileDraft {
    path: PathBuf,
    rel: String,
    title: String,
    kind: NodeKind,
    aliases: Vec<String>,
    wikilinks: Vec<String>,
    imports: Vec<PathBuf>,
}

impl ProjectGraph {
    pub fn scan(root: &Path) -> Self {
        let root_abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut drafts = Vec::new();
        let mut truncated = false;

        let walker = ignore::WalkBuilder::new(&root_abs)
            .hidden(false)
            .max_depth(Some(10))
            .build();

        for entry in walker.flatten() {
            let path = entry.path();
            if path == root_abs || skip_path(path) {
                continue;
            }
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let Some(kind) = node_kind(path) else {
                continue;
            };
            if drafts.len() >= MAX_NODES {
                truncated = true;
                break;
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            // Avoid spending time parsing huge generated-ish text files.
            if content.len() > 768 * 1024 {
                continue;
            }

            let path_abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            let rel = path_abs
                .strip_prefix(&root_abs)
                .unwrap_or(&path_abs)
                .to_string_lossy()
                .replace('\\', "/");
            let title = title_for(path, &content, kind);
            let mut aliases = vec![title.clone(), rel.clone()];
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                aliases.push(stem.to_string());
            }
            if let Some(no_ext) = rel.rsplit_once('.') {
                aliases.push(no_ext.0.to_string());
            }

            let parent = path_abs.parent().unwrap_or(&root_abs).to_path_buf();
            let wikilinks = if kind == NodeKind::Markdown {
                parse_wikilinks(&content)
            } else {
                Vec::new()
            };
            let imports = parse_imports(&content, kind, &parent);

            drafts.push(FileDraft {
                path: path_abs,
                rel,
                title,
                kind,
                aliases,
                wikilinks,
                imports,
            });
        }

        drafts.sort_by(|a, b| a.rel.cmp(&b.rel));

        let mut nodes: Vec<GraphNode> = drafts
            .iter()
            .map(|d| GraphNode {
                path: d.path.clone(),
                rel: d.rel.clone(),
                title: d.title.clone(),
                kind: d.kind,
                x: 0.0,
                y: 0.0,
                z: 0.0,
            })
            .collect();

        let mut alias_to_idx: HashMap<String, usize> = HashMap::new();
        let mut path_to_idx: HashMap<PathBuf, usize> = HashMap::new();
        for (i, d) in drafts.iter().enumerate() {
            path_to_idx.insert(d.path.clone(), i);
            for alias in &d.aliases {
                alias_to_idx.entry(norm_alias(alias)).or_insert(i);
            }
        }

        let mut edge_set: HashSet<(usize, usize)> = HashSet::new();
        for (i, d) in drafts.iter().enumerate() {
            for target in &d.wikilinks {
                if let Some(&j) = alias_to_idx.get(&norm_alias(target)) {
                    if i != j {
                        edge_set.insert((i, j));
                    }
                }
            }
            for target in &d.imports {
                if let Some(j) = resolve_import(&path_to_idx, target) {
                    if i != j {
                        edge_set.insert((i, j));
                    }
                }
            }
        }
        let mut edges: Vec<(usize, usize)> = edge_set.into_iter().collect();
        edges.sort_unstable();

        layout(&mut nodes, &edges);
        let selected = best_start_node(nodes.len(), &edges);

        Self {
            nodes,
            edges,
            selected,
            camera: Camera::new(),
            truncated,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.nodes.get(self.selected).map(|n| n.path.clone())
    }

    pub fn selected_label(&self) -> String {
        self.nodes
            .get(self.selected)
            .map(|n| n.title.clone())
            .unwrap_or_else(|| "no nodes".to_string())
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.nodes.is_empty() {
            self.selected = 0;
            return;
        }
        let n = self.nodes.len() as isize;
        let mut s = self.selected as isize + delta;
        if s < 0 {
            s = n - 1;
        }
        if s >= n {
            s = 0;
        }
        self.selected = s as usize;
    }

    pub fn reset_view(&mut self) {
        self.camera.reset();
    }

    /// Axis-aligned 3D bounds of the laid-out (normalized) node cloud.
    pub fn bounds3d(&self) -> Option<Bounds3d> {
        let first = self.nodes.first()?;
        let mut b = Bounds3d {
            min: [first.x, first.y, first.z],
            max: [first.x, first.y, first.z],
        };
        for n in &self.nodes {
            let p = [n.x, n.y, n.z];
            for axis in 0..3 {
                b.min[axis] = b.min[axis].min(p[axis]);
                b.max[axis] = b.max[axis].max(p[axis]);
            }
        }
        Some(b)
    }

    /// World-space position of node `i` for the camera/projection pipeline.
    pub fn node_world(&self, i: usize) -> Option<Vec3> {
        self.nodes
            .get(i)
            .map(|n| Vec3::new(n.x as f64, n.y as f64, n.z as f64))
    }

    pub fn neighbors_of_selected(&self) -> HashSet<usize> {
        let mut out = HashSet::new();
        for &(a, b) in &self.edges {
            if a == self.selected {
                out.insert(b);
            }
            if b == self.selected {
                out.insert(a);
            }
        }
        out
    }
}

fn node_kind(path: &Path) -> Option<NodeKind> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "md" | "mdx" => Some(NodeKind::Markdown),
        "rs" | "py" | "js" | "jsx" | "ts" | "tsx" | "go" | "java" | "c" | "cc" | "cpp" | "h"
        | "hpp" | "swift" | "kt" | "rb" | "php" | "sh" | "bash" | "zsh" => Some(NodeKind::Source),
        "toml" | "json" | "yaml" | "yml" | "lock" => Some(NodeKind::Config),
        "txt" | "org" => Some(NodeKind::Other),
        _ => None,
    }
}

fn skip_path(path: &Path) -> bool {
    path.components().any(|c| {
        let s = c.as_os_str().to_string_lossy();
        matches!(
            s.as_ref(),
            ".git"
                | ".ctrl"
                | "target"
                | "node_modules"
                | ".next"
                | "dist"
                | "build"
                | ".venv"
                | "venv"
                | "__pycache__"
        )
    })
}

fn title_for(path: &Path, content: &str, kind: NodeKind) -> String {
    if kind == NodeKind::Markdown {
        if let Some(t) = frontmatter_title(content) {
            return t;
        }
        if let Some(h) = first_heading(content) {
            return h;
        }
    }
    path.file_name()
        .or_else(|| path.file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string())
}

fn frontmatter_title(content: &str) -> Option<String> {
    let mut lines = content.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    for line in lines {
        let t = line.trim();
        if t == "---" {
            break;
        }
        if let Some(rest) = t.strip_prefix("title:") {
            let title = rest.trim().trim_matches(['\'', '"']);
            if !title.is_empty() {
                return Some(title.to_string());
            }
        }
    }
    None
}

fn first_heading(content: &str) -> Option<String> {
    for line in content.lines() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("# ") {
            let h = rest.trim();
            if !h.is_empty() {
                return Some(h.to_string());
            }
        }
    }
    None
}

fn parse_wikilinks(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find("[[") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find("]]") else {
            break;
        };
        let inside = &rest[..end];
        let target = inside
            .split('|')
            .next()
            .unwrap_or(inside)
            .split('#')
            .next()
            .unwrap_or(inside)
            .trim();
        if !target.is_empty() {
            out.push(target.to_string());
        }
        rest = &rest[end + 2..];
    }
    out
}

fn parse_imports(content: &str, kind: NodeKind, parent: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    match kind {
        NodeKind::Source => {
            for line in content.lines().take(1500) {
                let t = line.trim_start();
                if let Some(name) = rust_mod_name(t) {
                    out.push(parent.join(format!("{name}.rs")));
                    out.push(parent.join(name).join("mod.rs"));
                }
                if let Some(rel) = quoted_relative_path(t) {
                    out.push(parent.join(rel));
                }
            }
        }
        NodeKind::Markdown => {
            // Also support plain markdown links to nearby files: [text](./note.md)
            for line in content.lines().take(1500) {
                if let Some(rel) = markdown_link_path(line) {
                    out.push(parent.join(rel));
                }
            }
        }
        _ => {}
    }
    out
}

fn rust_mod_name(line: &str) -> Option<String> {
    let rest = line
        .strip_prefix("mod ")
        .or_else(|| line.strip_prefix("pub mod "))?;
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    let after = &rest[name.len()..];
    if after.trim_start().starts_with(';') {
        Some(name)
    } else {
        None
    }
}

fn quoted_relative_path(line: &str) -> Option<String> {
    // JS/TS/Swift/PHP-ish imports commonly carry a quoted relative path. This
    // intentionally ignores package imports like "react".
    for quote in ['"', '\''] {
        let mut rest = line;
        while let Some(start) = rest.find(quote) {
            rest = &rest[start + quote.len_utf8()..];
            let Some(end) = rest.find(quote) else {
                break;
            };
            let candidate = rest[..end].trim();
            if candidate.starts_with("./") || candidate.starts_with("../") {
                return Some(candidate.to_string());
            }
            rest = &rest[end + quote.len_utf8()..];
        }
    }
    None
}

fn markdown_link_path(line: &str) -> Option<String> {
    let start = line.find("](")? + 2;
    let rest = &line[start..];
    let end = rest.find(')')?;
    let candidate = rest[..end].trim();
    if candidate.starts_with("./") || candidate.starts_with("../") || candidate.ends_with(".md") {
        Some(candidate.to_string())
    } else {
        None
    }
}

fn resolve_import(path_to_idx: &HashMap<PathBuf, usize>, target: &Path) -> Option<usize> {
    let mut candidates = Vec::new();
    candidates.push(target.to_path_buf());
    if target.extension().is_none() {
        for ext in [
            "rs", "ts", "tsx", "js", "jsx", "py", "md", "mdx", "json", "toml",
        ] {
            candidates.push(target.with_extension(ext));
        }
        candidates.push(target.join("mod.rs"));
        candidates.push(target.join("index.ts"));
        candidates.push(target.join("index.tsx"));
        candidates.push(target.join("index.js"));
        candidates.push(target.join("index.jsx"));
    }
    for c in candidates {
        if let Ok(abs) = c.canonicalize() {
            if let Some(&idx) = path_to_idx.get(&abs) {
                return Some(idx);
            }
        }
    }
    None
}

fn norm_alias(s: &str) -> String {
    let mut out = s
        .trim()
        .trim_matches(['[', ']', '(', ')', '"', '\''])
        .replace('\\', "/")
        .to_ascii_lowercase();
    if out.ends_with(".mdx") {
        out.truncate(out.len() - 4);
    } else if out.ends_with(".md") {
        out.truncate(out.len() - 3);
    }
    out
}

fn best_start_node(n: usize, edges: &[(usize, usize)]) -> usize {
    if n == 0 {
        return 0;
    }
    let mut degree = vec![0usize; n];
    for &(a, b) in edges {
        if a < n {
            degree[a] += 1;
        }
        if b < n {
            degree[b] += 1;
        }
    }
    degree
        .iter()
        .enumerate()
        .max_by_key(|(_, d)| **d)
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn layout(nodes: &mut [GraphNode], edges: &[(usize, usize)]) {
    let n = nodes.len();
    if n == 0 {
        return;
    }
    if n == 1 {
        nodes[0].x = 0.0;
        nodes[0].y = 0.0;
        nodes[0].z = 0.0;
        return;
    }

    // Deterministic 3D seed: golden-angle (Fibonacci-sphere) directions pushed
    // outward on a growing spiral radius — the 3D analogue of the previous 2D
    // golden-angle spiral. Stable snapshots, no rand dependency.
    let golden = 2.3999632_f32;
    for (i, node) in nodes.iter_mut().enumerate() {
        let r = 2.0 + (i as f32).sqrt() * 2.4;
        let t = (i as f32 + 0.5) / n as f32;
        let y = 1.0 - 2.0 * t;
        let ring = (1.0 - y * y).max(0.0).sqrt();
        let a = i as f32 * golden;
        node.x = a.cos() * ring * r;
        node.y = y * r;
        node.z = a.sin() * ring * r;
    }

    if !edges.is_empty() && n <= 220 {
        let iters = if n <= 80 { 90 } else { 45 };
        let area = (n as f32).sqrt() * 18.0;
        let k = (area * area / n as f32).sqrt().max(3.0);
        for _ in 0..iters {
            let mut disp = vec![[0.0f32; 3]; n];
            for i in 0..n {
                for j in i + 1..n {
                    let dx = nodes[i].x - nodes[j].x;
                    let dy = nodes[i].y - nodes[j].y;
                    let dz = nodes[i].z - nodes[j].z;
                    let dist = (dx * dx + dy * dy + dz * dz).sqrt().max(0.05);
                    let force = (k * k / dist).min(8.0);
                    let fx = dx / dist * force;
                    let fy = dy / dist * force;
                    let fz = dz / dist * force;
                    disp[i][0] += fx;
                    disp[i][1] += fy;
                    disp[i][2] += fz;
                    disp[j][0] -= fx;
                    disp[j][1] -= fy;
                    disp[j][2] -= fz;
                }
            }
            for &(a, b) in edges {
                let dx = nodes[a].x - nodes[b].x;
                let dy = nodes[a].y - nodes[b].y;
                let dz = nodes[a].z - nodes[b].z;
                let dist = (dx * dx + dy * dy + dz * dz).sqrt().max(0.05);
                let force = (dist * dist / k * 0.035).min(6.0);
                let fx = dx / dist * force;
                let fy = dy / dist * force;
                let fz = dz / dist * force;
                disp[a][0] -= fx;
                disp[a][1] -= fy;
                disp[a][2] -= fz;
                disp[b][0] += fx;
                disp[b][1] += fy;
                disp[b][2] += fz;
            }
            let temp = 0.18;
            for i in 0..n {
                nodes[i].x += disp[i][0].clamp(-10.0, 10.0) * temp;
                nodes[i].y += disp[i][1].clamp(-10.0, 10.0) * temp;
                nodes[i].z += disp[i][2].clamp(-10.0, 10.0) * temp;
            }
        }
    }

    // Center all three axes, then normalize the greatest EUCLIDEAN radius to a
    // unit sphere (not a unit cube): the camera's default framing distance
    // assumes max |position| == 1.
    let mut min = [nodes[0].x, nodes[0].y, nodes[0].z];
    let mut max = min;
    for node in nodes.iter() {
        let p = [node.x, node.y, node.z];
        for axis in 0..3 {
            min[axis] = min[axis].min(p[axis]);
            max[axis] = max[axis].max(p[axis]);
        }
    }
    let c = [
        (min[0] + max[0]) / 2.0,
        (min[1] + max[1]) / 2.0,
        (min[2] + max[2]) / 2.0,
    ];
    let mut max_r2 = 0.0f32;
    for node in nodes.iter_mut() {
        node.x -= c[0];
        node.y -= c[1];
        node.z -= c[2];
        max_r2 = max_r2.max(node.x * node.x + node.y * node.y + node.z * node.z);
    }
    let scale = 1.0 / max_r2.sqrt().max(1e-6);
    for node in nodes.iter_mut() {
        node.x *= scale;
        node.y *= scale;
        node.z *= scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wikilinks() {
        assert_eq!(
            parse_wikilinks("[[A]] [[B|bee]] [[C#x]]"),
            vec!["A", "B", "C"]
        );
    }

    #[test]
    fn parses_rust_mods() {
        assert_eq!(rust_mod_name("mod graph;"), Some("graph".into()));
        assert_eq!(rust_mod_name("pub mod app;"), Some("app".into()));
        assert_eq!(rust_mod_name("mod inline {"), None);
    }

    fn make_nodes(n: usize) -> Vec<GraphNode> {
        (0..n)
            .map(|i| GraphNode {
                path: PathBuf::from(format!("n{i}.md")),
                rel: format!("n{i}.md"),
                title: format!("n{i}"),
                kind: NodeKind::Other,
                x: 0.0,
                y: 0.0,
                z: 0.0,
            })
            .collect()
    }

    /// The default camera pose (spatial.rs `Camera::new`, distance 4.5) frames
    /// the cloud assuming layout() normalizes to max EUCLIDEAN radius == 1.0
    /// around the origin. Per-axis (unit-cube) scaling would push corner nodes
    /// out to radius ~sqrt(3) and break framing; this test pins the sphere
    /// contract on both layout paths (with and without the FR relaxation).
    fn assert_unit_sphere(nodes: &[GraphNode]) {
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        let mut max_r = 0.0f32;
        for node in nodes {
            let p = [node.x, node.y, node.z];
            for axis in 0..3 {
                assert!(p[axis].is_finite(), "non-finite coordinate {p:?}");
                min[axis] = min[axis].min(p[axis]);
                max[axis] = max[axis].max(p[axis]);
            }
            max_r = max_r.max((p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt());
        }
        assert!(
            (max_r - 1.0).abs() <= 1e-3,
            "max Euclidean radius {max_r} not within 1e-3 of 1.0"
        );
        for axis in 0..3 {
            let mid = (min[axis] + max[axis]) / 2.0;
            assert!(
                mid.abs() <= 1e-3,
                "bounds midpoint {mid} on axis {axis} not at origin"
            );
        }
    }

    #[test]
    fn layout_normalizes_to_unit_radius() {
        // FR path: edges present and n <= 220.
        let mut nodes = make_nodes(20);
        let edges: Vec<(usize, usize)> = vec![(0, 1), (1, 2), (2, 3), (0, 5), (4, 9), (10, 17)];
        layout(&mut nodes, &edges);
        assert_unit_sphere(&nodes);

        // No-FR path: small N, no edges — seed spiral only, then normalize.
        let mut small = make_nodes(3);
        layout(&mut small, &[]);
        assert_unit_sphere(&small);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Live scan of a real repo. Ignored by default (machine-specific).
    #[test]
    #[ignore]
    fn live_scan_dump() {
        let root = std::env::var("OCEAN_TUI_LIST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from("/Users/risingtidesdev/dev/ocean-os/crates/ocean-tui")
            });
        let g = ProjectGraph::scan(&root);
        println!(
            "graph: {} nodes, {} edges (truncated={})",
            g.nodes.len(),
            g.edges.len(),
            g.truncated
        );
        println!("start node: {}", g.selected_label());
    }
}
