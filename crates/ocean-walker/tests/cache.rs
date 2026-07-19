use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ocean_walker::{invalidate_path, WalkBackend, WalkFilter, WalkRank, WalkRequest};

struct TempTree(PathBuf);

impl TempTree {
    fn new(name: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after UNIX epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ocean-walker-cache-{name}-{unique}"));
        fs::create_dir(&root).expect("temporary root should be created");
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        invalidate_path(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn paths(request: &WalkRequest) -> (WalkBackend, Vec<String>) {
    let outcome = request.collect().expect("cached walk should succeed");
    let paths = outcome
        .entries
        .into_iter()
        .map(|entry| entry.display_path)
        .collect();
    (outcome.backend, paths)
}

fn run_isolated_probe(name: &str, ttl_ms: &str) {
    let output = Command::new(std::env::current_exe().expect("test executable should resolve"))
        .args([name, "--exact", "--nocapture"])
        .env("OCEAN_WALKER_CACHE_PROBE", "1")
        .env("FS_SCAN_CACHE_TTL_MS", ttl_ms)
        .output()
        .expect("isolated cache probe should run");

    assert!(
        output.status.success(),
        "isolated probe failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn pre_cancelled_empty_request_is_interrupted_before_success() {
    let tree = TempTree::new("empty-cancelled");
    let result = WalkRequest::new(tree.path()).collect_with_heartbeat(|| Err("cancelled"));
    assert!(result
        .expect_err("pre-cancelled empty request must fail")
        .to_string()
        .contains("cancelled"));
}

#[test]
fn cached_scan_stays_stale_until_mutated_path_is_invalidated() {
    run_isolated_probe("invalidation_probe", "5000");
}

#[test]
fn invalidation_probe() {
    if std::env::var_os("OCEAN_WALKER_CACHE_PROBE").is_none() {
        return;
    }

    let tree = TempTree::new("invalidate");
    let first = tree.path().join("first.txt");
    let second = tree.path().join("second.txt");
    fs::write(&first, "first").expect("first fixture should be written");
    let request = WalkRequest::new(tree.path())
        .cache(true)
        .filter(WalkFilter::files_only());

    let (backend, initial) = paths(&request);
    assert_eq!(backend, WalkBackend::Fresh);
    assert_eq!(initial, vec!["first.txt"]);

    let cancelled_hit = request.collect_with_heartbeat(|| Err("pre-cancelled"));
    assert!(cancelled_hit
        .expect_err("pre-cancelled cached request must fail")
        .to_string()
        .contains("pre-cancelled"));

    fs::write(&second, "second").expect("second fixture should be written");
    let (backend, stale) = paths(&request);
    assert_eq!(backend, WalkBackend::Cached);
    assert_eq!(stale, vec!["first.txt"]);

    invalidate_path(&second);
    let (backend, refreshed) = paths(&request);
    assert_eq!(backend, WalkBackend::Fresh);
    assert_eq!(refreshed, vec!["first.txt", "second.txt"]);
}

#[test]
fn cached_scan_expires_at_configured_ttl() {
    run_isolated_probe("ttl_expiry_probe", "10");
}

#[test]
fn ttl_expiry_probe() {
    if std::env::var_os("OCEAN_WALKER_CACHE_PROBE").is_none() {
        return;
    }

    let tree = TempTree::new("ttl");
    fs::write(tree.path().join("first.txt"), "first").expect("first fixture should be written");
    let request = WalkRequest::new(tree.path())
        .cache(true)
        .filter(WalkFilter::files_only());
    assert_eq!(paths(&request).0, WalkBackend::Fresh);

    fs::write(tree.path().join("second.txt"), "second").expect("second fixture should be written");
    assert_eq!(paths(&request).0, WalkBackend::Cached);

    std::thread::sleep(Duration::from_millis(20));
    let (backend, refreshed) = paths(&request);
    assert_eq!(backend, WalkBackend::Fresh);
    assert_eq!(refreshed, vec!["first.txt", "second.txt"]);
}

#[test]
fn mtime_rank_orders_newest_first_then_uses_path_as_tiebreak() {
    let tree = TempTree::new("mtime-rank");
    let alpha = tree.path().join("alpha.txt");
    let beta = tree.path().join("beta.txt");
    let gamma = tree.path().join("gamma.txt");
    fs::write(&alpha, "alpha").expect("alpha fixture should be written");
    fs::write(&beta, "beta").expect("beta fixture should be written");
    fs::write(&gamma, "gamma").expect("gamma fixture should be written");

    let older = UNIX_EPOCH + Duration::from_secs(1_000_000);
    let newer = UNIX_EPOCH + Duration::from_secs(2_000_000);
    for path in [&alpha, &beta] {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("fixture should open for timestamp update")
            .set_times(fs::FileTimes::new().set_modified(newer))
            .expect("newer fixture timestamp should be set");
    }
    fs::OpenOptions::new()
        .write(true)
        .open(&gamma)
        .expect("fixture should open for timestamp update")
        .set_times(fs::FileTimes::new().set_modified(older))
        .expect("older fixture timestamp should be set");

    let outcome = WalkRequest::new(tree.path())
        .filter(WalkFilter::files_only())
        .collect_ranked(WalkRank::MtimeDescPathAsc, 3)
        .expect("ranked walk should succeed");
    let ranked = outcome
        .entries
        .into_iter()
        .map(|entry| entry.display_path)
        .collect::<Vec<_>>();

    assert_eq!(ranked, vec!["alpha.txt", "beta.txt", "gamma.txt"]);
}
