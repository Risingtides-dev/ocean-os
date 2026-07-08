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

/// A timed-out bash command must not survive as an orphan process. The child
/// writes a marker file AFTER a sleep longer than the tool timeout; if the
/// process were leaked (the old `timeout(fut)`-drops-the-future bug), the
/// marker would appear shortly after the timeout returned. With kill_on_drop
/// the child dies with the future and the marker never lands.
#[tokio::test]
async fn bash_timeout_kills_the_child_no_orphan() {
    let dir = scratch_dir();
    let marker = dir.join("orphan-marker");
    let marker_s = marker.to_string_lossy().to_string();

    let start = std::time::Instant::now();
    let err = bash::BashTool::for_cwd(dir.clone())
        .execute(
            "1",
            json!({
                "command": format!("sleep 2 && touch '{marker_s}'"),
                "timeout_ms": 300
            }),
        )
        .await
        .expect_err("the command must time out");
    assert!(err.contains("timed out"), "timeout error, got: {err}");
    assert!(
        start.elapsed() < std::time::Duration::from_millis(1500),
        "timeout must return promptly"
    );

    // Give a leaked child ample time to reach the `touch`. If the kill worked,
    // the marker never appears.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert!(
        !marker.exists(),
        "the timed-out child kept running and touched the marker — orphan process leak"
    );
}

/// stdin is closed, not inherited: a command that reads stdin terminates
/// immediately (EOF) instead of hanging until the timeout.
#[tokio::test]
async fn bash_stdin_is_closed_so_reads_terminate() {
    let start = std::time::Instant::now();
    let res = bash::BashTool::new()
        .execute(
            "1",
            json!({ "command": "cat; echo done-after-cat", "timeout_ms": 5000 }),
        )
        .await
        .expect("cat on closed stdin returns immediately");
    let text = res.content[0].as_text().unwrap();
    assert!(text.contains("done-after-cat"));
    assert!(
        start.elapsed() < std::time::Duration::from_millis(2000),
        "must not hang waiting for stdin"
    );
}

/// Output capture is bounded: a command that floods stdout is capped at the
/// capture limit with an explicit notice, while still running to completion
/// (exit code preserved).
#[tokio::test]
async fn bash_output_capture_is_capped() {
    // ~8MiB of zeros, well over the 2MiB cap.
    let res = bash::BashTool::new()
        .execute(
            "1",
            json!({ "command": "head -c 8388608 /dev/zero | tr '\\0' 'x'; echo; echo tail-marker >&2", "timeout_ms": 30000 }),
        )
        .await
        .expect("flooding command still completes");
    let text = res.content[0].as_text().unwrap();
    assert!(
        text.contains("[stdout capped at 2MiB"),
        "cap notice must be present"
    );
    assert!(
        text.len() < 3 * 1024 * 1024,
        "captured output must be bounded, got {} bytes",
        text.len()
    );
    assert!(
        text.contains("tail-marker"),
        "stderr still captured; command ran to completion"
    );
    assert!(text.contains("[exit 0]"));
}

/// grep is regex-first: a pattern with regex syntax matches structurally.
#[tokio::test]
async fn grep_matches_regex_patterns() {
    let dir = scratch_dir();
    std::fs::write(
        dir.join("code.rs"),
        "fn run_agent() {}\nfn   run_helper() {}\nlet x = 1;\n",
    )
    .unwrap();

    let res = grep::GrepTool::for_cwd(dir.clone())
        .execute("1", json!({ "pattern": r"fn\s+run_\w+" }))
        .await
        .unwrap();
    let text = res.content[0].as_text().unwrap();
    assert!(text.contains("run_agent"), "regex must match, got: {text}");
    assert!(
        text.contains("run_helper"),
        "multi-space regex must match, got: {text}"
    );
    assert!(!text.contains("let x"), "non-matches excluded");
}

/// An invalid regex falls back to literal substring search with an explicit
/// note — a model that meant `foo(` literally still gets its matches.
#[tokio::test]
async fn grep_invalid_regex_falls_back_to_literal() {
    let dir = scratch_dir();
    std::fs::write(dir.join("code.rs"), "call foo( now\nother line\n").unwrap();

    let res = grep::GrepTool::for_cwd(dir.clone())
        .execute("1", json!({ "pattern": "foo(" }))
        .await
        .unwrap();
    let text = res.content[0].as_text().unwrap();
    assert!(
        text.contains("not valid regex"),
        "fallback note present, got: {text}"
    );
    assert!(text.contains("call foo( now"), "literal match found");
}

/// A matched line is clipped, never dumped whole: one giant minified line must
/// not balloon the output.
#[tokio::test]
async fn grep_clips_enormous_matched_lines() {
    let dir = scratch_dir();
    let giant = format!("needle {}", "x".repeat(100_000));
    std::fs::write(dir.join("min.js"), &giant).unwrap();

    let res = grep::GrepTool::for_cwd(dir.clone())
        .execute("1", json!({ "pattern": "needle" }))
        .await
        .unwrap();
    let text = res.content[0].as_text().unwrap();
    assert!(text.contains("[line clipped]"), "clip marker present");
    assert!(
        text.len() < 2_000,
        "output stays small, got {} bytes",
        text.len()
    );
}
