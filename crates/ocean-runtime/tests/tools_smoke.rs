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

#[cfg(unix)]
struct AbortTaskOnDrop<T> {
    handle: Option<tokio::task::JoinHandle<T>>,
}

#[cfg(unix)]
impl<T> AbortTaskOnDrop<T> {
    fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    fn abort(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }

    async fn join(&mut self) -> Result<T, tokio::task::JoinError> {
        self.handle
            .take()
            .expect("test task handle can only be joined once")
            .await
    }
}

#[cfg(unix)]
impl<T> Drop for AbortTaskOnDrop<T> {
    fn drop(&mut self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }
}

#[cfg(unix)]
struct PidCleanup {
    process_group: i32,
    pids: Vec<i32>,
    armed: bool,
}

#[cfg(unix)]
impl PidCleanup {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(unix)]
impl Drop for PidCleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // SAFETY: kill only receives positive PIDs read from this test's own
        // marker files and their negated process-group id. ESRCH is harmless.
        unsafe {
            libc::kill(-self.process_group, libc::SIGKILL);
            for pid in &self.pids {
                libc::kill(*pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(unix)]
async fn wait_for_pid(path: &std::path::Path) -> i32 {
    // A saturated hosted runner can take more than two seconds to schedule the
    // just-spawned shell. Keep the probe finite, but separate startup slack
    // from the stricter post-Halt termination deadline below.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    return pid;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("PID marker was not written: {}", path.display()))
}

#[cfg(unix)]
fn process_is_running(pid: i32) -> bool {
    // SAFETY: signal 0 performs existence/permission probing only.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    let output = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output();
    match output {
        Ok(output) if output.status.success() => !String::from_utf8_lossy(&output.stdout)
            .trim_start()
            .starts_with('Z'),
        _ => false,
    }
}

#[cfg(unix)]
async fn wait_until_not_running(pid: i32) -> bool {
    for _ in 0..100 {
        if !process_is_running(pid) {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
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

/// Dropping the BashTool future is the runtime's Halt boundary. The direct
/// command process must die promptly rather than outliving its cancelled turn.
#[cfg(unix)]
#[tokio::test]
async fn bash_halt_kills_direct_child_by_pid() {
    let dir = scratch_dir();
    let pid_file = dir.join("direct.pid");
    let command = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
    let mut task = AbortTaskOnDrop::new(tokio::spawn(async move {
        bash::BashTool::for_cwd(dir)
            .execute("halt-direct", json!({"command": command}))
            .await
    }));
    let pid = wait_for_pid(&pid_file).await;
    let mut cleanup = PidCleanup {
        process_group: pid,
        pids: vec![pid],
        armed: true,
    };

    task.abort();
    let join = task
        .join()
        .await
        .expect_err("aborted tool future must not complete");
    assert!(join.is_cancelled());
    assert!(
        wait_until_not_running(pid).await,
        "direct bash child {pid} survived Halt cancellation"
    );
    cleanup.disarm();
}

/// A shell may launch grandchildren that survive a PID-only kill. Halt must
/// terminate the complete command process group on supported Unix platforms.
#[cfg(unix)]
#[tokio::test]
async fn bash_halt_kills_descendant_process_tree_by_pid() {
    let dir = scratch_dir();
    let parent_file = dir.join("parent.pid");
    let child_file = dir.join("child.pid");
    let command = format!(
        "echo $$ > '{}'; (trap '' HUP TERM; sleep 30) & child=$!; echo $child > '{}'; wait $child",
        parent_file.display(),
        child_file.display()
    );
    let mut task = AbortTaskOnDrop::new(tokio::spawn(async move {
        bash::BashTool::for_cwd(dir)
            .execute("halt-tree", json!({"command": command}))
            .await
    }));
    let parent_pid = wait_for_pid(&parent_file).await;
    let child_pid = wait_for_pid(&child_file).await;
    let mut cleanup = PidCleanup {
        process_group: parent_pid,
        pids: vec![parent_pid, child_pid],
        armed: true,
    };

    task.abort();
    let join = task
        .join()
        .await
        .expect_err("aborted tool future must not complete");
    assert!(join.is_cancelled());
    assert!(
        wait_until_not_running(parent_pid).await,
        "bash parent {parent_pid} survived Halt cancellation"
    );
    assert!(
        wait_until_not_running(child_pid).await,
        "bash descendant {child_pid} survived Halt cancellation"
    );
    cleanup.disarm();
}

/// stdin is closed, not inherited: a command that reads stdin terminates
/// (EOF) instead of hanging until the timeout.
///
/// The elapsed budget proves "did not hang until timeout_ms", NOT "was fast":
/// `bash -l` sources the host's profile scripts, which cost multiple seconds
/// on GitHub's ubuntu runners (nvm/sdkman et al). A hang pins elapsed at
/// timeout_ms, so any comfortable margin under it is decisive.
#[tokio::test]
async fn bash_stdin_is_closed_so_reads_terminate() {
    const TIMEOUT_MS: u64 = 15_000;
    const NO_HANG_BUDGET_MS: u64 = 8_000;
    let start = std::time::Instant::now();
    let res = bash::BashTool::new()
        .execute(
            "1",
            json!({ "command": "cat; echo done-after-cat", "timeout_ms": TIMEOUT_MS }),
        )
        .await
        .expect("cat on closed stdin returns immediately");
    let text = res.content[0].as_text().unwrap();
    assert!(text.contains("done-after-cat"));
    assert!(
        start.elapsed() < std::time::Duration::from_millis(NO_HANG_BUDGET_MS),
        "must not hang waiting for stdin (elapsed {:?} of the {TIMEOUT_MS}ms timeout)",
        start.elapsed()
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
