//! Ocean session discovery — harvested from CTRL's `sessions.rs` and stripped
//! to Ocean only. Reads session records from `~/.config/ocean-rs/sessions/*/*.json`
//! and surfaces the ones whose workspace/cwd falls under the current project
//! root (main checkout or a worktree beneath it).
//!
//! Pure filesystem reads, no daemon round-trip — this is the data behind the
//! left session rail.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone)]
pub struct Session {
    pub id: String,
    pub title: String,
    /// Directory to resume in (workspace_root, else cwd, else project root).
    pub cwd: PathBuf,
    /// "main" or the worktree folder name beneath the project root.
    pub worktree: String,
    pub mtime: u64, // unix seconds
    pub path: PathBuf,
}

impl Session {
    /// The command that resumes this session, run in `cwd`.
    ///
    /// Always forces `--legacy`: this spawns inside the workbench's own
    /// terminal dock, and once this branch merges, `ocean` (no flags) IS the
    /// workbench. Without `--legacy` here, resuming a session from the dock
    /// would recursively launch a whole second workbench inside its own PTY
    /// pane. `--legacy` is the escape hatch main.rs already wires up
    /// (`OCEAN_TUI_LEGACY` / `--legacy`, see `main.rs`) to force the old,
    /// non-workbench TUI — exactly what a nested dock session needs.
    pub fn resume_command(&self) -> (String, Vec<String>) {
        (
            "ocean".into(),
            vec![
                "--project".into(),
                self.cwd.display().to_string(),
                "--session".into(),
                self.id.clone(),
                "--legacy".into(),
            ],
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Worktree,
    Date,
}

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
}

fn mtime_secs(p: &Path) -> u64 {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn json_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

/// Filter out agent-injected context so titles reflect what the operator typed.
fn is_real_message(s: &str) -> bool {
    let t = s.trim_start();
    if t.is_empty() {
        return false;
    }
    const NOISE: [&str; 6] = [
        "<environment_context",
        "<user_instructions",
        "<cwd>",
        "# AGENTS.md",
        "<system",
        "Caveat:",
    ];
    !NOISE.iter().any(|n| t.starts_with(n))
}

fn trim_title(s: &str) -> String {
    let s = s.trim().replace('\n', " ");
    if s.chars().count() > 60 {
        format!("{}…", s.chars().take(59).collect::<String>())
    } else {
        s
    }
}

fn discover_ocean(root: &Path, out: &mut Vec<Session>) {
    let base = home().join(".config/ocean-rs/sessions");
    let Ok(dirs) = fs::read_dir(&base) else {
        return;
    };
    for d in dirs.flatten() {
        let dir = d.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(dir) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&p) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            if let Some(session) = ocean_session_from_value(root, &p, &v) {
                out.push(session);
            }
        }
    }
}

fn ocean_session_from_value(root: &Path, path: &Path, v: &serde_json::Value) -> Option<Session> {
    let id = json_str(v, "id")?.to_string();
    let cwd = json_str(v, "cwd").map(PathBuf::from);
    let workspace = json_str(v, "workspace_root").map(PathBuf::from);
    let match_path = workspace.as_ref().or(cwd.as_ref())?;
    let worktree = worktree_label(root, match_path)?;
    let run_cwd = workspace.clone().or(cwd).unwrap_or_else(|| root.to_path_buf());
    let mtime = v
        .get("updated_ms")
        .and_then(|x| x.as_u64())
        .map(|ms| ms / 1000)
        .unwrap_or_else(|| mtime_secs(path));
    let title = ocean_title(v).unwrap_or_else(|| {
        let model = json_str(v, "model").unwrap_or("model");
        let provider = json_str(v, "provider").unwrap_or("ocean");
        format!("{provider}/{model}")
    });
    Some(Session {
        id,
        title,
        cwd: run_cwd,
        worktree,
        mtime,
        path: path.to_path_buf(),
    })
}

fn ocean_title(v: &serde_json::Value) -> Option<String> {
    let messages = v.get("messages")?.as_array()?;
    for m in messages {
        if json_str(m, "role") != Some("user") {
            continue;
        }
        if let Some(c) = m.get("content") {
            if let Some(s) = c.as_str() {
                if let Some(t) = ocean_clean_user_text(s) {
                    return Some(t);
                }
            }
            if let Some(arr) = c.as_array() {
                for b in arr {
                    if let Some(t) = json_str(b, "text") {
                        if let Some(t) = ocean_clean_user_text(t) {
                            return Some(t);
                        }
                    }
                }
            }
        }
    }
    None
}

fn ocean_clean_user_text(s: &str) -> Option<String> {
    let mut t = s.trim();
    if t.starts_with("[TUI]") {
        if let Some((_, rest)) = t.split_once("\n\n") {
            t = rest.trim();
        }
    }
    t = t.trim_start_matches('`').trim();
    if is_real_message(t) {
        Some(trim_title(t))
    } else {
        None
    }
}

/// Returns the worktree label if `cwd` is `root` or a path under it.
fn worktree_label(root: &Path, cwd: &Path) -> Option<String> {
    if cwd == root {
        return Some("main".into());
    }
    cwd.strip_prefix(root)
        .ok()
        .map(|rel| rel.to_string_lossy().trim_start_matches('/').to_string())
}

/// Discover all Ocean sessions for `root`, sorted.
pub fn discover(root: &Path, sort: Sort) -> Vec<Session> {
    let mut out = Vec::new();
    discover_ocean(root, &mut out);
    sort_sessions(&mut out, sort);
    out
}

pub fn sort_sessions(v: &mut [Session], sort: Sort) {
    match sort {
        Sort::Date => v.sort_by_key(|s| std::cmp::Reverse(s.mtime)),
        Sort::Worktree => v.sort_by(|a, b| {
            let ka = (a.worktree != "main", a.worktree.as_str(), std::cmp::Reverse(a.mtime));
            let kb = (b.worktree != "main", b.worktree.as_str(), std::cmp::Reverse(b.mtime));
            ka.cmp(&kb)
        }),
    }
}

/// A single transcript message loaded from a session's on-disk record.
pub struct HistoryMsg {
    pub role: String,
    pub text: String,
}

/// Load a session's transcript from its JSON record. Concatenates the text of
/// each user/assistant message; tool/system noise is skipped. Used to rehydrate
/// the chat view when a session is resumed natively.
pub fn load_transcript(path: &Path) -> Vec<HistoryMsg> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(messages) = v.get("messages").and_then(|m| m.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for m in messages {
        let role = json_str(m, "role").unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }
        let text = message_text(m);
        if text.trim().is_empty() {
            continue;
        }
        out.push(HistoryMsg {
            role: role.to_string(),
            text,
        });
    }
    out
}

/// Extract the plain text of a message whose `content` is either a string or an
/// array of `{type,text}` blocks.
fn message_text(m: &serde_json::Value) -> String {
    let Some(c) = m.get("content") else {
        return String::new();
    };
    if let Some(s) = c.as_str() {
        return clean_history_text(s);
    }
    if let Some(arr) = c.as_array() {
        let mut buf = String::new();
        for b in arr {
            if let Some(t) = json_str(b, "text") {
                if !buf.is_empty() {
                    buf.push('\n');
                }
                buf.push_str(t);
            }
        }
        return clean_history_text(&buf);
    }
    String::new()
}

/// Client-type flags the daemon stamps onto every user turn (see
/// `ocean-agent`'s `surface_flag`, the single source of truth this mirrors:
/// `TUI`, `BRWSR`, `WEB`, `GUI`, `CLI`, `VOX`, `ACP`, `SLACK`, `CNVS`, `MOBL`,
/// and `?` for an unrecognised surface). Matched case-insensitively so
/// `[tui]`/`[Tui]`/`[TUI]` are all caught.
const CLIENT_TAGS: [&str; 11] = [
    "tui", "brwsr", "web", "gui", "cli", "vox", "acp", "slack", "cnvs", "mobl", "?",
];

/// Strip a leading client-type tag (`[TUI]`, `[ACP]`, ...) the daemon writes
/// ahead of the real prompt text, returning the text with the tag (and its
/// brackets) removed.
fn strip_client_tag(t: &str) -> Option<&str> {
    let inner = t.strip_prefix('[')?;
    let end = inner.find(']')?;
    let tag = inner[..end].to_ascii_lowercase();
    if CLIENT_TAGS.contains(&tag.as_str()) {
        Some(inner[end + 1..].trim_start())
    } else {
        None
    }
}

/// Clean a transcript line of its client-type tag. Handles both the
/// multi-line shape (tag + a one-line notice, blank line, then the real
/// body — the notice line is discarded along with the tag) and the
/// single-line shape (tag directly followed by the text on the same line,
/// nothing else to discard). The single-line shape used to leak the raw
/// `[TUI]` tag into resumed history because the old check only fired when a
/// `"\n\n"` separator was present.
fn clean_history_text(s: &str) -> String {
    let t = s.trim();
    match strip_client_tag(t) {
        Some(rest) => match rest.split_once("\n\n") {
            Some((_, after_blank)) => after_blank.trim().to_string(),
            None => rest.to_string(),
        },
        None => t.to_string(),
    }
}

/// Human "2h", "3d", "now" from a unix-seconds mtime.
pub fn ago(mtime: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(mtime);
    let d = now.saturating_sub(mtime);
    if d < 60 {
        "now".into()
    } else if d < 3600 {
        format!("{}m", d / 60)
    } else if d < 86_400 {
        format!("{}h", d / 3600)
    } else if d < 7 * 86_400 {
        format!("{}d", d / 86_400)
    } else {
        format!("{}w", d / (7 * 86_400))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_ocean_session_summary() {
        let root = PathBuf::from("/tmp/ocean-tui-root");
        let v = json!({
            "id": "7f99e2ec-2d8e-44d4-b61c-8c4d40bd850b",
            "updated_ms": 1781151304817u64,
            "model": "deepseek-v4-flash",
            "provider": "deepseek",
            "workspace_root": "/tmp/ocean-tui-root",
            "cwd": "/tmp/ocean-tui-root",
            "messages": [{
                "role": "user",
                "content": [{ "type": "text", "text": "[TUI] PM room.\n\n```make sessions visible" }]
            }]
        });
        let s = ocean_session_from_value(&root, Path::new("/tmp/s.json"), &v)
            .expect("ocean session should match project root");
        assert_eq!(s.worktree, "main");
        assert_eq!(s.title, "make sessions visible");
        assert_eq!(s.mtime, 1_781_151_304);
        let (cmd, args) = s.resume_command();
        assert_eq!(cmd, "ocean");
        assert_eq!(args.first().map(String::as_str), Some("--project"));
        assert_eq!(args.get(3).map(String::as_str), Some(s.id.as_str()));
    }

    #[test]
    fn ignores_sessions_outside_the_project_root() {
        let root = PathBuf::from("/tmp/project-a");
        let v = json!({ "id": "x", "workspace_root": "/tmp/project-b", "messages": [] });
        assert!(ocean_session_from_value(&root, Path::new("/tmp/s.json"), &v).is_none());
    }

    /// Defect 5: resuming a session from the dock must never re-launch the
    /// workbench recursively. Once `ocean` (no flags) IS the workbench, the
    /// resume command MUST force `--legacy` so the hydrated PTY runs the old
    /// TUI, not another copy of the shell it's embedded in.
    #[test]
    fn resume_command_forces_legacy_flag() {
        let s = Session {
            id: "7f99e2ec-2d8e-44d4-b61c-8c4d40bd850b".into(),
            title: "t".into(),
            cwd: PathBuf::from("/tmp/ocean-tui-root"),
            worktree: "main".into(),
            mtime: 0,
            path: PathBuf::from("/tmp/s.json"),
        };
        let (cmd, args) = s.resume_command();
        assert_eq!(cmd, "ocean");
        assert!(
            args.iter().any(|a| a == "--legacy"),
            "resume command must pass --legacy to avoid launching the workbench inside its own dock: {args:?}"
        );
    }

    /// Defect 4: the `[TUI]` client-type tag must be stripped from
    /// single-line resumed history entries too, not just multi-line ones
    /// (which already worked via the `"\n\n"` separator check).
    #[test]
    fn clean_history_text_strips_tag_from_single_line_entry() {
        assert_eq!(clean_history_text("[TUI] hello there"), "hello there");
        // Case-insensitive, and other bracketed client-type tags the daemon
        // may emit (see `ocean-agent::surface_flag`) are stripped the same way.
        assert_eq!(clean_history_text("[tui] hello there"), "hello there");
        assert_eq!(clean_history_text("[ACP] fix the bug"), "fix the bug");
        assert_eq!(clean_history_text("[WEB] ship it"), "ship it");
        // Multi-line shape (tag + notice line, blank line, then body) keeps
        // working exactly as before.
        assert_eq!(
            clean_history_text("[TUI] PM room.\n\nmake sessions visible"),
            "make sessions visible"
        );
        // Untagged text is left untouched.
        assert_eq!(clean_history_text("no tag here"), "no tag here");
    }

    #[test]
    fn load_transcript_strips_tag_from_single_line_history_message() {
        let dir = std::env::temp_dir().join(format!(
            "ocean-tui-sessions-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("session.json");
        let v = json!({
            "id": "x",
            "messages": [{
                "role": "user",
                "content": "[TUI] hello there"
            }]
        });
        fs::write(&path, v.to_string()).expect("write fixture");

        let hist = load_transcript(&path);
        assert_eq!(hist.len(), 1);
        assert_eq!(hist[0].role, "user");
        assert_eq!(hist[0].text, "hello there");
        assert!(!hist[0].text.contains("[TUI]"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// Live check against the real session store. Ignored by default (machine-
    /// specific); run with `cargo test -- --ignored --nocapture live_discovery`.
    #[test]
    #[ignore]
    fn live_discovery_dump() {
        let root = std::env::var("OCEAN_TUI_LIST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/Users/risingtidesdev/dev/ocean-os"));
        let found = discover(&root, Sort::Date);
        println!("discovered {} ocean sessions under {}", found.len(), root.display());
        for s in found.iter().take(15) {
            println!("  [{}] {} · {}", s.worktree, ago(s.mtime), s.title);
        }
    }
}

#[cfg(test)]
mod live_transcript {
    use super::*;
    #[test]
    #[ignore]
    fn dump_one_transcript() {
        // Newest ocean-os session on disk.
        let root = PathBuf::from("/Users/risingtidesdev/dev/ocean-os");
        let sessions = discover(&root, Sort::Date);
        let Some(s) = sessions.first() else { println!("no sessions"); return; };
        let hist = load_transcript(&s.path);
        println!("session '{}' → {} messages", s.title, hist.len());
        for m in hist.iter().take(6) {
            let preview: String = m.text.chars().take(70).collect();
            println!("  {}: {}", m.role, preview);
        }
    }
}
