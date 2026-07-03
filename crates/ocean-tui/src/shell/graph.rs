//! Project graph data model for ctrl's main-pane graph view.
//!
//! This is intentionally lightweight and self-contained. It borrows the *idea*
//! of graf/Obsidian-style wikilink graphs, but implements ctrl's own scanner so
//! the feature can live in the editor pane without pulling in another app.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const MAX_NODES: usize = 320;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Markdown,
    Source,
    Config,
    Other,
}

#[derive(Clone, Debug)]
pub struct GraphNode {
    pub path: PathBuf,
    pub rel: String,
    pub title: String,
    pub kind: NodeKind,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug)]
pub struct ProjectGraph {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<(usize, usize)>,
    pub selected: usize,
    /// Screen-space pan in terminal cells.
    pub pan_x: f32,
    /// Screen-space pan in terminal cells.
    pub pan_y: f32,
    /// User zoom multiplier; rendering computes an auto-fit base scale first.
    pub zoom: f32,
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
            pan_x: 0.0,
            pan_y: 0.0,
            zoom: 1.0,
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

    pub fn zoom_by(&mut self, factor: f32) {
        self.zoom = (self.zoom * factor).clamp(0.25, 4.0);
    }

    pub fn pan(&mut self, dx: f32, dy: f32) {
        self.pan_x += dx;
        self.pan_y += dy;
    }

    pub fn reset_view(&mut self) {
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.zoom = 1.0;
    }

    pub fn bounds(&self) -> Option<(f32, f32, f32, f32)> {
        let first = self.nodes.first()?;
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (first.x, first.x, first.y, first.y);
        for n in &self.nodes {
            min_x = min_x.min(n.x);
            max_x = max_x.max(n.x);
            min_y = min_y.min(n.y);
            max_y = max_y.max(n.y);
        }
        Some((min_x, max_x, min_y, max_y))
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
        return;
    }

    // Deterministic seed positions on a golden-angle spiral. This gives stable
    // snapshots and avoids needing rand.
    let golden = 2.3999632_f32;
    for (i, node) in nodes.iter_mut().enumerate() {
        let r = 2.0 + (i as f32).sqrt() * 2.4;
        let a = i as f32 * golden;
        node.x = a.cos() * r;
        node.y = a.sin() * r * 0.62;
    }

    if !edges.is_empty() && n <= 220 {
        let iters = if n <= 80 { 90 } else { 45 };
        let area = (n as f32).sqrt() * 18.0;
        let k = (area * area / n as f32).sqrt().max(3.0);
        for _ in 0..iters {
            let mut disp = vec![(0.0f32, 0.0f32); n];
            for i in 0..n {
                for j in i + 1..n {
                    let dx = nodes[i].x - nodes[j].x;
                    let dy = nodes[i].y - nodes[j].y;
                    let dist = (dx * dx + dy * dy).sqrt().max(0.05);
                    let force = (k * k / dist).min(8.0);
                    let fx = dx / dist * force;
                    let fy = dy / dist * force;
                    disp[i].0 += fx;
                    disp[i].1 += fy;
                    disp[j].0 -= fx;
                    disp[j].1 -= fy;
                }
            }
            for &(a, b) in edges {
                let dx = nodes[a].x - nodes[b].x;
                let dy = nodes[a].y - nodes[b].y;
                let dist = (dx * dx + dy * dy).sqrt().max(0.05);
                let force = (dist * dist / k * 0.035).min(6.0);
                let fx = dx / dist * force;
                let fy = dy / dist * force;
                disp[a].0 -= fx;
                disp[a].1 -= fy;
                disp[b].0 += fx;
                disp[b].1 += fy;
            }
            let temp = 0.18;
            for i in 0..n {
                nodes[i].x += disp[i].0.clamp(-10.0, 10.0) * temp;
                nodes[i].y += disp[i].1.clamp(-10.0, 10.0) * temp;
            }
        }
    }

    // Center and scale to a predictable logical canvas. The renderer does the
    // final fit to the current pane size.
    let (mut min_x, mut max_x, mut min_y, mut max_y) =
        (nodes[0].x, nodes[0].x, nodes[0].y, nodes[0].y);
    for node in nodes.iter() {
        min_x = min_x.min(node.x);
        max_x = max_x.max(node.x);
        min_y = min_y.min(node.y);
        max_y = max_y.max(node.y);
    }
    let cx = (min_x + max_x) / 2.0;
    let cy = (min_y + max_y) / 2.0;
    let span = (max_x - min_x).max((max_y - min_y) * 1.8).max(1.0);
    let target = (n as f32).sqrt() * 8.0 + 12.0;
    let scale = target / span;
    for node in nodes.iter_mut() {
        node.x = (node.x - cx) * scale;
        node.y = (node.y - cy) * scale * 0.68;
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
            .unwrap_or_else(|_| PathBuf::from("/Users/risingtidesdev/dev/ocean-os/crates/ocean-tui"));
        let g = ProjectGraph::scan(&root);
        println!("graph: {} nodes, {} edges (truncated={})", g.nodes.len(), g.edges.len(), g.truncated);
        println!("start node: {}", g.selected_label());
    }
}
