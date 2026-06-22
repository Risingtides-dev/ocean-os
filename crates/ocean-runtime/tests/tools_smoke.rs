//! Smoke tests for builtin tools. No LLM is required — these exercise the
//! tool implementations directly against a tempfile-backed scratch dir.

use std::path::PathBuf;

use ocean_runtime::tools::{bash, edit, glob_tool, grep, ls, read, write};
use ocean_runtime::types::AgentTool;
use serde_json::json;

fn scratch_dir() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tid = format!("{:?}", std::thread::current().id());
    let tid_filtered: String = tid.chars().filter(|c| c.is_alphanumeric()).collect();
    let dir = std::env::temp_dir().join(format!("pi-rs-test-{n:x}-{c}-{tid_filtered}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn write_then_read_roundtrips() {
    let dir = scratch_dir();
    let path = dir.join("hello.txt");
    let path_s = path.to_string_lossy().to_string();

    let res = write::WriteTool::new()
        .execute("1", json!({"path": path_s, "content": "hello\nworld\n"}))
        .await
        .unwrap();
    assert!(matches!(
        res.content[0],
        ocean_protocol::Content::Text { .. }
    ));

    let res = read::ReadTool::new()
        .execute("2", json!({"path": path_s}))
        .await
        .unwrap();
    let text = res.content[0].as_text().unwrap().to_string();
    assert!(text.contains("hello"));
    assert!(text.contains("world"));
}

#[tokio::test]
async fn edit_replaces_single_occurrence() {
    let dir = scratch_dir();
    let path = dir.join("a.txt");
    let path_s = path.to_string_lossy().to_string();
    std::fs::write(&path, "foo bar baz").unwrap();

    let res = edit::EditTool::new()
        .execute(
            "1",
            json!({"path": path_s, "old_string": "bar", "new_string": "BAR"}),
        )
        .await
        .unwrap();
    assert!(res.content[0].as_text().unwrap().contains("edited"));
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(after, "foo BAR baz");
}

#[tokio::test]
async fn ls_lists_dir() {
    let dir = scratch_dir();
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    std::fs::create_dir(dir.join("sub")).unwrap();

    let res = ls::LsTool::new()
        .execute("1", json!({"path": dir.to_string_lossy()}))
        .await
        .unwrap();
    let text = res.content[0].as_text().unwrap();
    assert!(text.contains("a.txt"));
    assert!(text.contains("sub"));
}

#[tokio::test]
async fn grep_finds_pattern() {
    let dir = scratch_dir();
    std::fs::write(dir.join("a.txt"), "needle\nhaystack\n").unwrap();
    std::fs::write(dir.join("b.txt"), "nothing here\n").unwrap();

    let res = grep::GrepTool::new()
        .execute(
            "1",
            json!({"pattern": "needle", "path": dir.to_string_lossy()}),
        )
        .await
        .unwrap();
    let text = res.content[0].as_text().unwrap();
    assert!(text.contains("a.txt"));
    assert!(text.contains("needle"));
}

#[tokio::test]
async fn glob_finds_files() {
    let dir = scratch_dir();
    std::fs::write(dir.join("a.rs"), "").unwrap();
    std::fs::write(dir.join("b.rs"), "").unwrap();
    std::fs::write(dir.join("c.txt"), "").unwrap();

    let pattern = format!("{}/*.rs", dir.to_string_lossy());
    let res = glob_tool::GlobTool::new()
        .execute("1", json!({"pattern": pattern}))
        .await
        .unwrap();
    let text = res.content[0].as_text().unwrap();
    assert!(text.contains("a.rs"));
    assert!(text.contains("b.rs"));
    assert!(!text.contains("c.txt"));
}

#[tokio::test]
async fn bash_runs_simple_command() {
    let res = bash::BashTool::new()
        .execute("1", json!({"command": "echo hi-from-bash"}))
        .await
        .unwrap();
    let text = res.content[0].as_text().unwrap();
    assert!(text.contains("hi-from-bash"));
    assert!(text.contains("[exit 0]"));
}
