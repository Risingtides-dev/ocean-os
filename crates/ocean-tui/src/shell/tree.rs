//! File tree: a flattened, expandable view of the working directory.
use std::fs;
use std::path::{Path, PathBuf};

pub struct Entry {
    pub path: PathBuf,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
}

pub struct Tree {
    pub root: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    expanded: std::collections::HashSet<PathBuf>,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        let mut t = Tree {
            root: root.clone(),
            entries: Vec::new(),
            selected: 0,
            expanded: std::collections::HashSet::new(),
        };
        t.expanded.insert(root);
        t.rebuild();
        t
    }

    fn read_dir_sorted(path: &Path) -> Vec<(PathBuf, bool)> {
        let mut items: Vec<(PathBuf, bool)> = match fs::read_dir(path) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    n != ".git" && n != "target" && n != "node_modules"
                })
                .map(|e| (e.path(), e.path().is_dir()))
                .collect(),
            Err(_) => Vec::new(),
        };
        items.sort_by(|a, b| match (a.1, b.1) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.file_name().cmp(&b.0.file_name()),
        });
        items
    }

    pub fn rebuild(&mut self) {
        let mut out = Vec::new();
        self.walk(&self.root.clone(), 0, &mut out);
        self.entries = out;
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len().saturating_sub(1);
        }
    }

    fn walk(&self, dir: &Path, depth: usize, out: &mut Vec<Entry>) {
        for (path, is_dir) in Self::read_dir_sorted(dir) {
            let expanded = self.expanded.contains(&path);
            out.push(Entry {
                path: path.clone(),
                depth,
                is_dir,
                expanded,
            });
            if is_dir && expanded {
                self.walk(&path, depth + 1, out);
            }
        }
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    pub fn move_sel(&mut self, delta: isize) {
        if self.entries.is_empty() {
            return;
        }
        let n = self.entries.len() as isize;
        let mut s = self.selected as isize + delta;
        if s < 0 {
            s = 0;
        }
        if s >= n {
            s = n - 1;
        }
        self.selected = s as usize;
    }

    /// Toggle a directory; returns Some(path) if a file should be opened.
    pub fn activate(&mut self) -> Option<PathBuf> {
        let (path, is_dir) = match self.selected_entry() {
            Some(e) => (e.path.clone(), e.is_dir),
            None => return None,
        };
        if is_dir {
            if self.expanded.contains(&path) {
                self.expanded.remove(&path);
            } else {
                self.expanded.insert(path);
            }
            self.rebuild();
            None
        } else {
            Some(path)
        }
    }
}
