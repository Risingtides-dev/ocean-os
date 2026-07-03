//! A single editable text buffer / tab.
use crate::shell::git::Mark;
use crate::shell::highlight::{Highlighter, StyledLine};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct EditorTab {
    pub path: PathBuf,
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub scroll: usize,
    pub dirty: bool,
    pub highlighted: Vec<StyledLine>,
    pub git_lines: HashMap<usize, Mark>,
    // incremental-highlight bookkeeping: edits re-highlight only the touched
    // line(s); a full pass runs once you pause (fixes multi-line constructs).
    needs_full_hl: bool,
    last_edit: Option<Instant>,
}

impl EditorTab {
    pub fn open(path: PathBuf, hl: &Highlighter) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(|s| s.to_string()).collect()
        };
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string();
        let mut highlighted = hl.highlight(&text, &ext);
        // keep the highlight cache row-aligned with `lines` (a file ending in
        // \n yields one fewer syntect row than split lines)
        highlighted.resize(lines.len().max(1), Vec::new());
        Ok(Self {
            path,
            lines,
            cursor_row: 0,
            cursor_col: 0,
            scroll: 0,
            dirty: false,
            highlighted,
            git_lines: HashMap::new(),
            needs_full_hl: false,
            last_edit: None,
        })
    }

    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled".into())
    }

    pub fn ext(&self) -> String {
        self.path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_string()
    }

    fn rehighlight(&mut self, hl: &Highlighter) {
        let text = self.lines.join("\n");
        self.highlighted = hl.highlight(&text, &self.ext());
        self.highlighted.resize(self.lines.len().max(1), Vec::new());
        self.needs_full_hl = false;
    }

    /// Re-highlight just one line (O(line)) — used on every keystroke so typing
    /// stays instant even in large files.
    fn highlight_line(&mut self, row: usize, hl: &Highlighter) {
        if row >= self.lines.len() {
            return;
        }
        let one = hl.highlight(&self.lines[row], &self.ext());
        let styled = one.into_iter().next().unwrap_or_default();
        if row < self.highlighted.len() {
            self.highlighted[row] = styled;
        } else {
            self.highlighted.push(styled);
        }
    }

    /// Mark an edit: dirty + schedule a debounced full re-highlight.
    fn mark_edited(&mut self) {
        self.dirty = true;
        self.needs_full_hl = true;
        self.last_edit = Some(Instant::now());
    }

    /// Run a deferred full re-highlight once typing has paused (fixes multi-line
    /// strings/comments). Returns true if it re-highlighted (caller redraws).
    pub fn settle(&mut self, hl: &Highlighter) -> bool {
        if self.needs_full_hl
            && self
                .last_edit
                .map(|t| t.elapsed() >= Duration::from_millis(140))
                .unwrap_or(false)
        {
            self.rehighlight(hl);
            true
        } else {
            false
        }
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        std::fs::write(&self.path, self.lines.join("\n"))?;
        self.dirty = false;
        Ok(())
    }

    /// Load per-line git gutter marks for this file from `git diff HEAD`.
    pub fn load_git(&mut self, root: &std::path::Path) {
        self.git_lines = crate::shell::git::changed_lines(root, &self.path);
    }

    fn clamp_col(&mut self) {
        let len = self
            .lines
            .get(self.cursor_row)
            .map(|l| l.chars().count())
            .unwrap_or(0);
        if self.cursor_col > len {
            self.cursor_col = len;
        }
    }

    pub fn move_cursor(&mut self, drow: isize, dcol: isize, viewport_h: usize) {
        if drow != 0 {
            let n = self.lines.len() as isize;
            let mut r = self.cursor_row as isize + drow;
            r = r.clamp(0, (n - 1).max(0));
            self.cursor_row = r as usize;
            self.clamp_col();
        }
        if dcol != 0 {
            let c = self.cursor_col as isize + dcol;
            if c < 0 {
                if self.cursor_row > 0 {
                    self.cursor_row -= 1;
                    self.cursor_col = self.lines[self.cursor_row].chars().count();
                }
            } else {
                let len = self.lines[self.cursor_row].chars().count() as isize;
                self.cursor_col = c.min(len) as usize;
            }
        }
        // keep cursor in view
        if self.cursor_row < self.scroll {
            self.scroll = self.cursor_row;
        } else if viewport_h > 0 && self.cursor_row >= self.scroll + viewport_h {
            self.scroll = self.cursor_row + 1 - viewport_h;
        }
    }

    fn byte_idx(line: &str, col: usize) -> usize {
        line.char_indices()
            .nth(col)
            .map(|(i, _)| i)
            .unwrap_or(line.len())
    }

    pub fn insert_char(&mut self, ch: char, hl: &Highlighter) {
        let bi = Self::byte_idx(&self.lines[self.cursor_row], self.cursor_col);
        self.lines[self.cursor_row].insert(bi, ch);
        self.cursor_col += 1;
        self.mark_edited();
        let row = self.cursor_row;
        self.highlight_line(row, hl); // O(line), not O(file)
    }

    pub fn insert_newline(&mut self, hl: &Highlighter) {
        let bi = Self::byte_idx(&self.lines[self.cursor_row], self.cursor_col);
        let rest = self.lines[self.cursor_row].split_off(bi);
        self.lines.insert(self.cursor_row + 1, rest);
        // keep the highlight cache structurally in sync, then re-light both lines
        if self.cursor_row < self.highlighted.len() {
            self.highlighted.insert(self.cursor_row + 1, Vec::new());
        }
        let r = self.cursor_row;
        self.cursor_row += 1;
        self.cursor_col = 0;
        self.mark_edited();
        self.highlight_line(r, hl);
        self.highlight_line(r + 1, hl);
    }

    pub fn delete_forward(&mut self, hl: &Highlighter) {
        let len = self.lines[self.cursor_row].chars().count();
        if self.cursor_col < len {
            let bi = Self::byte_idx(&self.lines[self.cursor_row], self.cursor_col);
            self.lines[self.cursor_row].remove(bi);
        } else if self.cursor_row + 1 < self.lines.len() {
            // join: pull the next line up onto this one
            let next = self.lines.remove(self.cursor_row + 1);
            if self.cursor_row + 1 < self.highlighted.len() {
                self.highlighted.remove(self.cursor_row + 1);
            }
            self.lines[self.cursor_row].push_str(&next);
        } else {
            return;
        }
        self.mark_edited();
        let row = self.cursor_row;
        self.highlight_line(row, hl);
    }

    pub fn backspace(&mut self, hl: &Highlighter) {
        if self.cursor_col > 0 {
            let bi = Self::byte_idx(&self.lines[self.cursor_row], self.cursor_col - 1);
            self.lines[self.cursor_row].remove(bi);
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            let cur = self.lines.remove(self.cursor_row);
            if self.cursor_row < self.highlighted.len() {
                self.highlighted.remove(self.cursor_row);
            }
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].chars().count();
            self.lines[self.cursor_row].push_str(&cur);
        } else {
            return;
        }
        self.mark_edited();
        let row = self.cursor_row;
        self.highlight_line(row, hl);
    }
}
