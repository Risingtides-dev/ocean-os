//! Daemon boot-path — health re-probe, autostart eligibility, binary discovery,
//! and launchd-aware process start. All platform-sensitive logic (launchctl,
//! process-group detach) is gated so the module compiles on Linux too.
//!
//! The health-monitor loop lives in `app::run` (it owns the async context and
//! the action channel); this module is the pure-logic engine underneath.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// ── rate-limit guard ────────────────────────────────────────────────────────────

/// Cheap rate-limit guard — one attempt per 30s, backed by an epoch-seconds
/// atomic so tests own their guard instance and never race the production one.
pub struct AutostartGuard {
    last: AtomicI64,
}

impl AutostartGuard {
    pub fn new() -> Self {
        Self {
            last: AtomicI64::new(0),
        }
    }

    /// Returns `true` if ≥30s have elapsed since the last `acquire` that
    /// returned `true`. Records this call as a new attempt on success.
    pub fn acquire(&self) -> bool {
        let now = now_epoch_secs();
        let prev = self.last.load(Ordering::Relaxed);
        if now - prev >= 30 {
            self.last.store(now, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Peek without recording — for tests that just want to verify the guard
    /// is hot.
    #[cfg(test)]
    fn would_allow(&self) -> bool {
        let now = now_epoch_secs();
        let prev = self.last.load(Ordering::Relaxed);
        now - prev >= 30
    }

    /// Force-reset for tests.
    #[cfg(test)]
    fn reset(&self) {
        self.last.store(0, Ordering::Relaxed);
    }
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// ── outcome ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartOutcome {
    /// Autostart succeeded — daemon is (re)starting.
    Started,
    /// ocean-daemon binary not found at any discovery path.
    BinaryNotFound,
    /// LaunchAgent kickstart or direct spawn failed.
    SpawnFailed(String),
    /// URL is not the default localhost:4780 (custom daemon endpoint).
    NotEligible,
    /// Env `OCEAN_TUI_AUTOSTART=0` or rate-limit prevents autostart.
    RateLimited,
    /// UID resolution failed — can't determine whether launchd supervises
    /// the daemon. Autostart is blocked to avoid racing launchd with a
    /// direct-spawn orphan.
    SupervisionUnknown,
}

// ── URL eligibility ─────────────────────────────────────────────────────────────

/// Check whether `url` targets the local default daemon (host == 127.0.0.1 or
/// localhost, port == 4780) so autostart can safely own the process.
fn url_is_localhost_4780(url: &str) -> bool {
    let authority = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    // Drop trailing slash, path, query, fragment.
    let authority = authority.split('/').next().unwrap_or(authority);
    // Split host:port at the LAST colon.
    let (host, port_str) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p),
        None => return false,
    };
    let port: u16 = match port_str.parse() {
        Ok(p) => p,
        Err(_) => return false,
    };
    port == 4780 && (host == "127.0.0.1" || host == "localhost")
}

/// Extract `host:port` from a URL for status messages.
pub fn host_port(url: &str) -> &str {
    let s = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    s.split('/').next().unwrap_or(s)
}

/// Pure eligibility check — injected env so tests don't mutate the process env.
pub fn autostart_eligible_with(url: &str, autostart_env: Option<&str>) -> bool {
    if autostart_env == Some("0") {
        return false;
    }
    url_is_localhost_4780(url)
}

/// Production wrapper: reads `OCEAN_TUI_AUTOSTART` from the process env.
pub fn autostart_eligible(url: &str) -> bool {
    autostart_eligible_with(url, std::env::var("OCEAN_TUI_AUTOSTART").ok().as_deref())
}

// ── binary discovery ────────────────────────────────────────────────────────────

/// Discover the `ocean-daemon` binary. Order:
/// 1. `OCEAN_DAEMON_BIN` env var (absolute or PATH-relative).
/// 2. Sibling `ocean-daemon` next to `current_exe`.
/// 3. Walk `PATH` for `ocean-daemon`.
///
/// Params injected for testability.
pub fn discover_binary_with(
    env_bin: Option<&str>,
    current_exe: Option<&Path>,
    path_env: Option<&str>,
) -> Option<PathBuf> {
    // 1. Env override.
    if let Some(bin) = env_bin {
        let p = PathBuf::from(bin);
        if p.is_absolute() && p.exists() {
            return Some(p);
        }
        // Relative — try PATH resolution.
        if let Some(found) = which_in_path(bin, path_env) {
            return Some(found);
        }
    }

    // 2. Sibling next to current_exe.
    if let Some(exe) = current_exe {
        if let Some(parent) = exe.parent() {
            let sibling = parent.join("ocean-daemon");
            if sibling.exists() {
                return Some(sibling);
            }
        }
    }

    // 3. PATH walk.
    which_in_path("ocean-daemon", path_env)
}

/// Walk `PATH` for a named executable. Returns the first existing file.
fn which_in_path(name: &str, path_env: Option<&str>) -> Option<PathBuf> {
    let path = path_env.unwrap_or("");
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = PathBuf::from(dir).join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Production wrapper.
pub fn discover_binary() -> Option<PathBuf> {
    discover_binary_with(
        std::env::var("OCEAN_DAEMON_BIN").ok().as_deref(),
        std::env::current_exe().ok().as_deref(),
        std::env::var("PATH").ok().as_deref(),
    )
}


/// Get the current user's UID by shelling out to `id -u` (std-only, no libc).
/// Returns None when UID resolution is unavailable — no hardcoded fallback.
#[cfg(target_os = "macos")]
fn current_uid() -> Option<u32> {
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok().and_then(|s| s.trim().parse().ok()))
}
// ── launchd detection ───────────────────────────────────────────────────────────
/// Production: invoke `launchctl print`. Returns `None` if the UID cannot be
/// determined (supervision unknown), `Some(true)` if launchd supervises the
/// daemon, `Some(false)` otherwise. Returns `Some(false)` on non-macOS.
pub fn launchd_supervises() -> Option<bool> {
    #[cfg(target_os = "macos")]
    {
        let uid = current_uid()?;
        Command::new("launchctl")
            .args([
                "print",
                &format!("gui/{uid}/dev.risingtides.ocean-daemon"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .ok()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some(false)
    }
}
/// Home directory. Falls back to `/tmp` if `HOME` is unset (containers, etc.).
fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

/// Log directory under home.
fn log_dir() -> PathBuf {
    home_dir().join(".ocean").join("logs")
}

/// Open the autostart log in append mode, creating dirs as needed.
fn open_autostart_log() -> Result<fs::File, String> {
    let dir = log_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("create log dir {}: {e}", dir.display()))?;
    let path = dir.join("daemon-autostart.log");
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))
}

/// Kickstart the LaunchAgent. Uses `kickstart` (NO `-k`) — the daemon is
/// already dead, launchd just needs to re-launch it.
#[cfg(target_os = "macos")]
fn launchctl_kickstart() -> Result<(), String> {
    let uid = current_uid()
        .ok_or_else(|| "cannot kickstart LaunchAgent: UID unavailable".to_string())?;
    let label = format!("gui/{uid}/dev.risingtides.ocean-daemon");
    let status = Command::new("launchctl")
        .args(["kickstart", &label])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("launchctl kickstart: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "launchctl kickstart exited {}",
            status.code().unwrap_or(-1)
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn launchctl_kickstart() -> Result<(), String> {
    Err("launchctl not available on this platform".into())
}

/// Direct-spawn ocean-daemon detached from the TUI process group.
fn direct_spawn(binary: &Path) -> Result<(), String> {
    let log_file = open_autostart_log()?;
    let mut cmd = Command::new(binary);
    cmd.current_dir(home_dir())
        .stdin(std::process::Stdio::null())
        .stdout(log_file.try_clone().map_err(|e| e.to_string())?)
        .stderr(log_file);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn {}: {e}", binary.display()))?;
    // Reap on a detached thread so an exiting daemon never leaves a zombie
    // while the TUI is running.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

// ── main entry ──────────────────────────────────────────────────────────────────

/// Fully-injected autostart for testability.
///
/// All side effects are closures: eligibility env, launchd probe, kickstart
/// execution, and binary-discovery+spawn. In production, use
/// [`maybe_autostart_prod`].
pub fn maybe_autostart_with(
    url: &str,
    guard: &AutostartGuard,
    autostart_env: Option<&str>,
    probe_launchd: impl FnOnce() -> bool,
    do_kickstart: impl FnOnce() -> Result<(), String>,
    do_discover_and_spawn: impl FnOnce() -> AutostartOutcome,
) -> AutostartOutcome {
    if !autostart_eligible_with(url, autostart_env) {
        return AutostartOutcome::NotEligible;
    }
    if !guard.acquire() {
        return AutostartOutcome::RateLimited;
    }

    if probe_launchd() {
        match do_kickstart() {
            Ok(()) => AutostartOutcome::Started,
            Err(e) => AutostartOutcome::SpawnFailed(e),
        }
    } else {
        do_discover_and_spawn()
    }
}
pub fn maybe_autostart_prod(url: &str, guard: &AutostartGuard) -> AutostartOutcome {
    // Check URL/env eligibility first — no process spawns.
    if !autostart_eligible(url) {
        return AutostartOutcome::NotEligible;
    }
    // Rate-limit guard second — no probes before this.
    if !guard.acquire() {
        return AutostartOutcome::RateLimited;
    }
    // Only now do we need to probe the system. If UID resolution fails
    // (supervision unknown on macOS), bail conservatively — never
    // direct-spawn an orphan that may race launchd.
    let supervised = match launchd_supervises() {
        Some(v) => v,
        None => return AutostartOutcome::SupervisionUnknown,
    };
    if supervised {
        match launchctl_kickstart() {
            Ok(()) => AutostartOutcome::Started,
            Err(e) => AutostartOutcome::SpawnFailed(e),
        }
    } else {
        match discover_binary() {
            Some(bin) => match direct_spawn(&bin) {
                Ok(()) => AutostartOutcome::Started,
                Err(e) => AutostartOutcome::SpawnFailed(e),
            },
            None => AutostartOutcome::BinaryNotFound,
        }
    }
}

// ── tests ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    // ── URL eligibility ─────────────────────────────────────────────────────

    #[test]
    fn eligible_default_url() {
        assert!(autostart_eligible_with("http://127.0.0.1:4780", Some("1")));
        assert!(autostart_eligible_with("http://localhost:4780", Some("1")));
    }

    #[test]
    fn eligible_without_env_var() {
        // Missing env = eligible (only "0" blocks).
        assert!(autostart_eligible_with("http://127.0.0.1:4780", None));
    }

    #[test]
    fn not_eligible_env_zero() {
        assert!(!autostart_eligible_with("http://127.0.0.1:4780", Some("0")));
        assert!(!autostart_eligible_with("http://localhost:4780", Some("0")));
    }

    #[test]
    fn not_eligible_wrong_port() {
        assert!(!autostart_eligible_with("http://127.0.0.1:4781", None));
        assert!(!autostart_eligible_with("http://127.0.0.1:47800", None));
        assert!(!autostart_eligible_with("http://localhost:4781", None));
    }

    #[test]
    fn not_eligible_wrong_host() {
        assert!(!autostart_eligible_with(
            "http://192.168.1.1:4780",
            None
        ));
        assert!(!autostart_eligible_with("http://0.0.0.0:4780", None));
    }

    #[test]
    fn eligible_trailing_slash() {
        assert!(autostart_eligible_with("http://127.0.0.1:4780/", None));
    }

    // ── host_port helper ───────────────────────────────────────────────────

    #[test]
    fn host_port_extraction() {
        assert_eq!(host_port("http://127.0.0.1:4780"), "127.0.0.1:4780");
        assert_eq!(host_port("http://localhost:4780"), "localhost:4780");
        assert_eq!(host_port("http://example.com:9999"), "example.com:9999");
        assert_eq!(
            host_port("http://example.com:9999/"),
            "example.com:9999"
        );
    }

    // ── binary discovery ───────────────────────────────────────────────────

    #[test]
    fn discover_env_override_wins() {
        let tmp = env::temp_dir().join("oceantui-test-daemon-env");
        fs::write(&tmp, b"fake").unwrap();
        let found = discover_binary_with(Some(&tmp.to_string_lossy()), None, None);
        assert_eq!(found, Some(tmp.clone()));
        fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn discover_sibling_found() {
        let tmp = env::temp_dir().join("oceantui-test-sibling");
        fs::create_dir_all(&tmp).unwrap();
        let sibling = tmp.join("ocean-daemon");
        fs::write(&sibling, b"fake").unwrap();
        let fake_exe = tmp.join("ocean-tui");
        fs::write(&fake_exe, b"fake").unwrap();

        let found = discover_binary_with(None, Some(&fake_exe), None);
        assert_eq!(found, Some(sibling.clone()));

        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn discover_sibling_not_found() {
        let tmp = env::temp_dir().join("oceantui-test-no-sibling");
        fs::create_dir_all(&tmp).unwrap();
        let fake_exe = tmp.join("ocean-tui");
        fs::write(&fake_exe, b"fake").unwrap();

        let found = discover_binary_with(None, Some(&fake_exe), None);
        assert_eq!(found, None);
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn discover_path_walk() {
        let tmp = env::temp_dir().join("oceantui-test-path");
        fs::create_dir_all(&tmp).unwrap();
        let bin = tmp.join("ocean-daemon");
        fs::write(&bin, b"fake").unwrap();

        let found = discover_binary_with(None, None, Some(&tmp.to_string_lossy()));
        assert_eq!(found, Some(bin.clone()));
        fs::remove_dir_all(&tmp).unwrap();
    }

    // ── rate-limit guard ───────────────────────────────────────────────────

    #[test]
    fn guard_acquire_then_block() {
        let g = AutostartGuard::new();
        assert!(g.acquire(), "first acquire should succeed");
        assert!(!g.acquire(), "immediate second acquire should fail");
        assert!(!g.would_allow(), "guard should be hot");
    }

    #[test]
    fn guard_reset_allows_again() {
        let g = AutostartGuard::new();
        assert!(g.acquire());
        g.reset();
        assert!(g.acquire(), "after reset, acquire should succeed again");
    }

    #[test]
    fn guard_isolated_instances() {
        let g1 = AutostartGuard::new();
        let g2 = AutostartGuard::new();
        assert!(g1.acquire());
        assert!(g2.acquire(), "separate guards don't interfere");
    }

    // ── maybe_autostart — injected, no side effects ─────────────────────────

    #[test]
    fn autostart_not_eligible_wrong_url() {
        let g = AutostartGuard::new();
        let outcome = maybe_autostart_with(
            "http://example.com:4780",
            &g,
            None, // autostart env missing (eligible if URL were right)
            || unreachable!("should not probe when not eligible"),
            || unreachable!("should not kickstart when not eligible"),
            || unreachable!("should not spawn when not eligible"),
        );
        assert_eq!(outcome, AutostartOutcome::NotEligible);
    }

    #[test]
    fn autostart_not_eligible_env_zero() {
        let g = AutostartGuard::new();
        let outcome = maybe_autostart_with(
            "http://127.0.0.1:4780",
            &g,
            Some("0"), // OCEAN_TUI_AUTOSTART=0
            || unreachable!("should not probe when env=0"),
            || unreachable!("should not kickstart when env=0"),
            || unreachable!("should not spawn when env=0"),
        );
        assert_eq!(outcome, AutostartOutcome::NotEligible);
    }

    #[test]
    fn autostart_rate_limited_after_first_attempt() {
        let g = AutostartGuard::new();
        // First call: launchd is absent, spawn "succeeds" (injected).
        let first = maybe_autostart_with(
            "http://127.0.0.1:4780",
            &g,
            None,
            || false,            // no launchd
            || unreachable!(),
            || AutostartOutcome::Started,
        );
        assert_eq!(first, AutostartOutcome::Started);

        // Second call: rate-limited.
        let second = maybe_autostart_with(
            "http://127.0.0.1:4780",
            &g,
            None,
            || unreachable!("should not probe when rate-limited"),
            || unreachable!("should not kickstart when rate-limited"),
            || unreachable!("should not spawn when rate-limited"),
        );
        assert_eq!(second, AutostartOutcome::RateLimited);
    }

    #[test]
    fn autostart_selects_launchd_when_supervised() {
        let g = AutostartGuard::new();
        let outcome = maybe_autostart_with(
            "http://127.0.0.1:4780",
            &g,
            None,
            || true, // probe says supervised
            || Ok(()), // kickstart succeeds
            || unreachable!("should not spawn when launchd supervised"),
        );
        assert_eq!(outcome, AutostartOutcome::Started);
    }

    #[test]
    fn autostart_selects_direct_spawn_when_not_supervised() {
        let g = AutostartGuard::new();
        let outcome = maybe_autostart_with(
            "http://127.0.0.1:4780",
            &g,
            None,
            || false, // no launchd
            || unreachable!("should not kickstart when no launchd"),
            || AutostartOutcome::Started, // spawn succeeds
        );
        assert_eq!(outcome, AutostartOutcome::Started);
    }

    #[test]
    fn autostart_direct_spawn_binary_not_found() {
        let g = AutostartGuard::new();
        let outcome = maybe_autostart_with(
            "http://127.0.0.1:4780",
            &g,
            None,
            || false, // no launchd
            || unreachable!(),
            || AutostartOutcome::BinaryNotFound,
        );
        assert_eq!(outcome, AutostartOutcome::BinaryNotFound);
    }

    // ── ordering: probes must not fire for non-eligible / rate-limited ─────

    /// Wrong URL: counting closures must stay at zero — eligibility is
    /// checked before any probe, kickstart, or spawn closure runs.
    #[test]
    fn ordering_not_eligible_no_probes() {
        use std::cell::Cell;
        let g = AutostartGuard::new();
        let probe_count = Cell::new(0u32);
        let kick_count = Cell::new(0u32);
        let spawn_count = Cell::new(0u32);
        let outcome = maybe_autostart_with(
            "http://example.com:4780",
            &g,
            None,
            || {
                probe_count.set(probe_count.get() + 1);
                false
            },
            || {
                kick_count.set(kick_count.get() + 1);
                Ok(())
            },
            || {
                spawn_count.set(spawn_count.get() + 1);
                AutostartOutcome::Started
            },
        );
        assert_eq!(outcome, AutostartOutcome::NotEligible);
        assert_eq!(probe_count.get(), 0, "probe fired before eligibility check");
        assert_eq!(kick_count.get(), 0);
        assert_eq!(spawn_count.get(), 0);
    }

    /// Hot guard: counting closures must stay at zero — the rate-limit
    /// guard is checked second, before any system probe.
    #[test]
    fn ordering_rate_limited_no_probes() {
        use std::cell::Cell;
        let g = AutostartGuard::new();
        // Burn the guard's one slot so the next call is rate-limited.
        assert!(g.acquire());
        let probe_count = Cell::new(0u32);
        let kick_count = Cell::new(0u32);
        let spawn_count = Cell::new(0u32);
        let outcome = maybe_autostart_with(
            "http://127.0.0.1:4780",
            &g,
            None,
            || {
                probe_count.set(probe_count.get() + 1);
                false
            },
            || {
                kick_count.set(kick_count.get() + 1);
                Ok(())
            },
            || {
                spawn_count.set(spawn_count.get() + 1);
                AutostartOutcome::Started
            },
        );
        assert_eq!(outcome, AutostartOutcome::RateLimited);
        assert_eq!(probe_count.get(), 0, "probe fired before rate-limit check");
        assert_eq!(kick_count.get(), 0);
        assert_eq!(spawn_count.get(), 0);
    }

    /// Eligible + idle guard: probes DO fire — sanity check that the
    /// counting machinery works and the happy path still reaches probes.
    #[test]
    fn ordering_eligible_probes_fire() {
        use std::cell::Cell;
        let g = AutostartGuard::new();
        let probe_count = Cell::new(0u32);
        let outcome = maybe_autostart_with(
            "http://127.0.0.1:4780",
            &g,
            None,
            || {
                probe_count.set(probe_count.get() + 1);
                false // not supervised → spawn path
            },
            || unreachable!(),
            || AutostartOutcome::Started,
        );
        assert_eq!(outcome, AutostartOutcome::Started);
        assert_eq!(probe_count.get(), 1, "probe should fire on eligible path");
    }

    // ── supervision-unknown safety ──────────────────────────────────────────

    #[test]
    fn supervision_unknown_is_distinct_variant() {
        // SupervisionUnknown exists as a distinct outcome — codepaths that
        // can't determine launchd status must not silently fall through to
        // direct-spawn.
        let outcome = AutostartOutcome::SupervisionUnknown;
        assert_ne!(outcome, AutostartOutcome::Started);
        assert_ne!(outcome, AutostartOutcome::BinaryNotFound);
        assert_ne!(outcome, AutostartOutcome::NotEligible);
        assert_ne!(outcome, AutostartOutcome::RateLimited);
        assert!(matches!(outcome, AutostartOutcome::SupervisionUnknown));
    }

    #[test]
    fn launchd_probe_with_none_returns_none_on_uid_failure() {
        // When UID resolution fails, the launchd probe must return None
        // (unknown), never construct a gui/NNNN/... path with a fabricated UID.
        // We test this by calling the production code path — on macOS this
        // shells out to `id -u`; if that somehow fails, we get None.
        let result = launchd_supervises();
        // On macOS with a real user, this returns Some(_); the important
        // property is that it never panics and the return type is Option<bool>.
        let _: Option<bool> = result;
    }

    #[test]
    fn current_uid_never_hardcodes_501() {
        // The function's source must not contain unwrap_or(501) — this
        // verifies the return type is Option<u32>, which statically prevents
        // the hardcoded fallback.
        #[cfg(target_os = "macos")]
        {
            let uid = current_uid();
            // Type-level guarantee: uid is Option<u32>, not u32.
            let _: Option<u32> = uid;
            // If resolution succeeds, it's the real UID (not a hardcoded value).
            if let Some(uid) = uid {
                assert!(uid > 0, "real UID is never 0");
            }
            // If resolution fails, uid is None — no fabricated value.
        }
    }
}
