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

#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub title: String,
    /// Directory to resume in (workspace_root, else cwd, else project root).
    pub cwd: PathBuf,
    /// "main" or the worktree folder name beneath the project root.
    pub worktree: String,
    /// Git branch stamped onto the record at session creation (daemon-side
    /// `bind_workspace`). Older records predate the field → `None`.
    pub branch: Option<String>,
    pub mtime: u64, // unix seconds
    pub path: PathBuf,
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
    let run_cwd = workspace
        .clone()
        .or(cwd)
        .unwrap_or_else(|| root.to_path_buf());
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
        branch: json_str(v, "git_branch").map(str::to_string),
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
    // Strip the daemon's client tag ([TUI]/[ACP]/[?]/…) the same way resumed
    // history is cleaned. The rail preview used to leak the raw tag on
    // single-line first messages because the old check matched only a literal
    // `[TUI]` and only when a `"\n\n"` notice separator was present.
    if let Some(rest) = strip_client_tag(t) {
        t = match rest.split_once("\n\n") {
            Some((_, after_blank)) => after_blank.trim(),
            None => rest,
        };
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
pub fn discover(root: &Path) -> Vec<Session> {
    let mut out = Vec::new();
    discover_ocean(root, &mut out);
    sort_sessions(&mut out);
    out
}

/// Resolve an exact session id or unambiguous prefix across every persisted
/// Ocean workspace. Exact duplicate files choose the newest record; prefixes
/// must identify one distinct session id.
pub fn resolve(query: &str) -> Result<Session, String> {
    resolve_in(&home().join(".config/ocean-rs/sessions"), query)
}

fn resolve_in(base: &Path, query: &str) -> Result<Session, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("session id cannot be empty".into());
    }

    let mut matches = Vec::new();
    if let Ok(dirs) = fs::read_dir(base) {
        for dir in dirs
            .flatten()
            .map(|entry| entry.path())
            .filter(|p| p.is_dir())
        {
            let Ok(files) = fs::read_dir(dir) else {
                continue;
            };
            for path in files.flatten().map(|entry| entry.path()) {
                if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                    continue;
                }
                let Ok(text) = fs::read_to_string(&path) else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let Some(id) = json_str(&value, "id") else {
                    continue;
                };
                if id != query && !id.starts_with(query) {
                    continue;
                }
                let Some(root) = json_str(&value, "workspace_root")
                    .or_else(|| json_str(&value, "cwd"))
                    .map(PathBuf::from)
                else {
                    continue;
                };
                if let Some(session) = ocean_session_from_value(&root, &path, &value) {
                    matches.push(session);
                }
            }
        }
    }

    let exact = matches
        .iter()
        .filter(|session| session.id == query)
        .max_by_key(|session| session.mtime)
        .cloned();
    if let Some(session) = exact {
        return Ok(session);
    }

    matches.sort_by(|a, b| a.id.cmp(&b.id).then_with(|| b.mtime.cmp(&a.mtime)));
    matches.dedup_by(|a, b| a.id == b.id);
    match matches.len() {
        0 => Err(format!("session not found: {query}")),
        1 => Ok(matches.remove(0)),
        _ => Err(format!(
            "session prefix '{query}' is ambiguous ({} matches)",
            matches.len()
        )),
    }
}

pub fn sort_sessions(v: &mut [Session]) {
    v.sort_by_key(|session| std::cmp::Reverse(session.mtime));
}

/// A single display-ready user/assistant transcript message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryMsg {
    pub role: String,
    pub text: String,
}

/// Project raw persisted daemon messages through the same visible-text path as
/// native disk resume. Only `{type:"text", text:…}` blocks are shown, so
/// provider thinking blocks never become visible prose after a refresh.
pub fn history_from_messages(messages: &[serde_json::Value]) -> Vec<HistoryMsg> {
    messages
        .iter()
        .filter_map(|message| {
            let role = json_str(message, "role").unwrap_or("");
            if role != "user" && role != "assistant" {
                return None;
            }
            let text = message_text(message);
            (!text.trim().is_empty()).then(|| HistoryMsg {
                role: role.to_string(),
                text,
            })
        })
        .collect()
}

/// Validate and project the daemon's bounded public synchronization snapshot
/// into native chat history. Any private-shaped row or response-bound violation
/// rejects the entire snapshot; silently filtering it would not be fail closed.
pub fn history_from_sync_snapshot(
    snapshot: &ocean_core::SessionSyncSnapshot,
) -> Result<Vec<HistoryMsg>, String> {
    if snapshot.transcript.len() > ocean_core::SESSION_SYNC_MAX_VISIBLE_MESSAGES {
        return Err("session sync snapshot exceeded the visible-message bound".into());
    }
    let mut visible_bytes = 0usize;
    let mut history = Vec::with_capacity(snapshot.transcript.len());
    for message in &snapshot.transcript {
        if message.role != "user" && message.role != "assistant" {
            return Err("session sync snapshot contained a non-visible role".into());
        }
        if !message.images.is_empty()
            || message.tool_call_id.is_some()
            || message.tool_name.is_some()
            || message.is_error.is_some()
        {
            return Err("session sync snapshot contained private-shaped metadata".into());
        }
        visible_bytes = visible_bytes
            .checked_add(message.text.len())
            .ok_or_else(|| "session sync snapshot text bound overflowed".to_string())?;
        if visible_bytes > ocean_core::SESSION_SYNC_MAX_VISIBLE_TEXT_BYTES {
            return Err("session sync snapshot exceeded the visible-text bound".into());
        }
        let text = clean_history_text(&message.text);
        if !text.trim().is_empty() {
            history.push(HistoryMsg {
                role: message.role.clone(),
                text,
            });
        }
    }
    Ok(history)
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
    history_from_messages(messages)
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
            if matches!(json_str(b, "type"), Some(kind) if kind != "text") {
                continue;
            }
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

    fn resolver_fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ocean-tui-resolver-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("bucket")).expect("create resolver fixture");
        dir
    }

    fn write_session(base: &Path, file: &str, id: &str, updated_ms: u64, title: &str) {
        let workspace = base.join("workspace");
        let value = json!({
            "id": id,
            "updated_ms": updated_ms,
            "model": "fake-ok",
            "provider": "fake",
            "workspace_root": workspace,
            "cwd": workspace,
            "messages": [{
                "role": "user",
                "content": [{ "type": "text", "text": title }]
            }]
        });
        fs::write(
            base.join("bucket").join(file),
            serde_json::to_vec(&value).expect("encode session"),
        )
        .expect("write session");
    }

    #[test]
    fn resolver_prefers_newest_exact_duplicate() {
        let base = resolver_fixture("exact");
        let id = "a1111111-1111-4111-8111-111111111111";
        write_session(&base, "old.json", id, 1_000, "old");
        write_session(&base, "new.json", id, 2_000, "new");

        let resolved = resolve_in(&base, id).expect("resolve exact duplicate");
        assert_eq!(resolved.title, "new");
        assert_eq!(resolved.mtime, 2);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn resolver_accepts_unique_prefix_and_rejects_ambiguous_or_missing() {
        let base = resolver_fixture("prefix");
        write_session(
            &base,
            "one.json",
            "a1111111-1111-4111-8111-111111111111",
            1_000,
            "one",
        );
        write_session(
            &base,
            "two.json",
            "a2222222-2222-4222-8222-222222222222",
            1_000,
            "two",
        );

        assert_eq!(resolve_in(&base, "a111").unwrap().title, "one");
        assert!(resolve_in(&base, "a").unwrap_err().contains("ambiguous"));
        assert!(resolve_in(&base, "missing")
            .unwrap_err()
            .contains("not found"));
        let _ = fs::remove_dir_all(base);
    }

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
            "git_branch": "feat/sessions",
            "messages": [{
                "role": "user",
                "content": [{ "type": "text", "text": "[TUI] PM room.\n\n```make sessions visible" }]
            }]
        });
        let s = ocean_session_from_value(&root, Path::new("/tmp/s.json"), &v)
            .expect("ocean session should match project root");
        assert_eq!(s.worktree, "main");
        assert_eq!(s.branch.as_deref(), Some("feat/sessions"));
        assert_eq!(s.title, "make sessions visible");
        assert_eq!(s.mtime, 1_781_151_304);
    }

    #[test]
    fn rail_title_strips_single_line_client_tags() {
        // The rail preview leaked the raw tag on single-line first messages
        // ([TUI]/[ACP]/[?]/…) because the old cleaner needed a "\n\n" separator.
        assert_eq!(
            ocean_clean_user_text("[TUI] hey there").as_deref(),
            Some("hey there")
        );
        assert_eq!(ocean_clean_user_text("[ACP] hey").as_deref(), Some("hey"));
        assert_eq!(
            ocean_clean_user_text("[?] say pong").as_deref(),
            Some("say pong")
        );
        // Case-insensitive, and the multi-line notice shape still resolves to body.
        assert_eq!(
            ocean_clean_user_text("[tui] notice\n\nreal body").as_deref(),
            Some("real body")
        );
        // An untagged message is untouched.
        assert_eq!(
            ocean_clean_user_text("plain message").as_deref(),
            Some("plain message")
        );
    }

    #[test]
    fn ignores_sessions_outside_the_project_root() {
        let root = PathBuf::from("/tmp/project-a");
        let v = json!({ "id": "x", "workspace_root": "/tmp/project-b", "messages": [] });
        assert!(ocean_session_from_value(&root, Path::new("/tmp/s.json"), &v).is_none());
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
    fn daemon_transcript_refresh_matches_native_history_projection() {
        let messages = vec![
            json!({"role":"user","content":"[TUI] hello again"}),
            json!({"role":"tool","content":[{"type":"text","text":"private tool output"}]}),
            json!({
                "role":"assistant",
                "content":[
                    {"type":"thinking","thinking":"hidden chain of thought"},
                    {"type":"thinking","text":"also hidden even with a text-shaped field"},
                    {"type":"text","text":"compacted summary"}
                ]
            }),
        ];

        assert_eq!(
            history_from_messages(&messages),
            vec![
                HistoryMsg {
                    role: "user".into(),
                    text: "hello again".into(),
                },
                HistoryMsg {
                    role: "assistant".into(),
                    text: "compacted summary".into(),
                },
            ]
        );
    }

    #[test]
    fn synchronized_snapshot_projection_is_visible_text_only_and_strips_client_tag() {
        let entry = |role: &str, text: &str| ocean_core::SessionTranscriptEntry {
            role: role.into(),
            timestamp_ms: None,
            text: text.into(),
            images: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            is_error: None,
        };
        let mut snapshot = ocean_core::SessionSyncSnapshot {
            session_id: uuid::Uuid::nil(),
            model: "test".into(),
            provider: "fake".into(),
            config_revision: 0,
            transcript: vec![
                entry("user", "[TUI] hello"),
                entry("assistant", "visible answer"),
            ],
            truncated_messages: 2,
            truncated_text_bytes: 0,
        };

        assert_eq!(
            history_from_sync_snapshot(&snapshot).expect("valid public snapshot"),
            vec![
                HistoryMsg {
                    role: "user".into(),
                    text: "hello".into(),
                },
                HistoryMsg {
                    role: "assistant".into(),
                    text: "visible answer".into(),
                },
            ]
        );

        snapshot
            .transcript
            .push(entry("tool", "private tool output"));
        assert!(history_from_sync_snapshot(&snapshot).is_err());
        snapshot.transcript.pop();
        snapshot.transcript[0].images.push(ocean_core::ImageMeta {
            mime_type: "image/png".into(),
        });
        assert!(history_from_sync_snapshot(&snapshot).is_err());

        snapshot.transcript = (0..=ocean_core::SESSION_SYNC_MAX_VISIBLE_MESSAGES)
            .map(|_| entry("user", "x"))
            .collect();
        assert!(history_from_sync_snapshot(&snapshot).is_err());
        snapshot.transcript = vec![entry(
            "assistant",
            &"x".repeat(ocean_core::SESSION_SYNC_MAX_VISIBLE_TEXT_BYTES + 1),
        )];
        assert!(history_from_sync_snapshot(&snapshot).is_err());
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
        let found = discover(&root);
        println!(
            "discovered {} ocean sessions under {}",
            found.len(),
            root.display()
        );
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
        let sessions = discover(&root);
        let Some(s) = sessions.first() else {
            println!("no sessions");
            return;
        };
        let hist = load_transcript(&s.path);
        println!("session '{}' → {} messages", s.title, hist.len());
        for m in hist.iter().take(6) {
            let preview: String = m.text.chars().take(70).collect();
            println!("  {}: {}", m.role, preview);
        }
    }
}
