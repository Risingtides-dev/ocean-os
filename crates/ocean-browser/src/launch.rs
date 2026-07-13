//! Chrome discovery + launch flag assembly. The flag list is pure and unit
//! tested; the actual spawn is delegated to chromiumoxide.

use std::path::PathBuf;

use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;

use crate::error::BrowserError;

/// Inputs that determine how Chrome is launched.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// Persistent profile dir so logins survive restarts. Point this at the
    /// user's real Chrome data dir to inherit all existing logins.
    pub profile_dir: PathBuf,
    /// Chrome's `--profile-directory` (e.g. "Default") — which profile *inside*
    /// the data dir to use. None lets Chrome pick its default.
    pub profile_directory: Option<String>,
    /// Unpacked extension to preload (the Ocean cockpit). None in tests/headless.
    pub extension_dir: Option<PathBuf>,
    /// Explicit Chrome binary to launch. Set this to Chrome for Testing — current
    /// stable Chrome (137+) removed `--load-extension`, so the cockpit extension
    /// will NOT auto-load there; CfT still honors it. None lets chromiumoxide
    /// auto-detect a system Chrome.
    pub chrome_executable: Option<PathBuf>,
    pub headless: bool,
    /// 0 lets the OS pick a free port.
    pub port: u16,
}

impl LaunchConfig {
    /// Assemble the raw chrome CLI args. Pure — unit tested.
    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            format!("--user-data-dir={}", self.profile_dir.display()),
            format!("--remote-debugging-port={}", self.port),
            "--no-first-run".to_string(),
            "--no-default-browser-check".to_string(),
        ];
        if let Some(profile) = &self.profile_directory {
            args.push(format!("--profile-directory={profile}"));
        }
        if self.headless {
            args.push("--headless=new".to_string());
        }
        if let Some(ext) = &self.extension_dir {
            args.push(format!("--load-extension={}", ext.display()));
            // Extensions are disabled in headless; only meaningful headful.
            args.push(format!("--disable-extensions-except={}", ext.display()));
        }
        args
    }
}

/// A launched Chrome plus its CDP handler task. Dropping this kills Chrome.
pub struct LaunchedChrome {
    pub browser: Browser,
}

/// Spawn the CDP handler-polling task that keeps a Browser making progress.
pub(crate) fn spawn_handler(mut handler: chromiumoxide::Handler) {
    tokio::spawn(async move {
        while let Some(ev) = handler.next().await {
            if ev.is_err() {
                break;
            }
        }
    });
}

/// If a Chrome is already running on this profile, return its CDP HTTP endpoint
/// (e.g. "http://127.0.0.1:NNNN"). Chrome writes the live port to
/// `<user-data-dir>/DevToolsActivePort` (first line). We verify it actually
/// responds before trusting it — a stale file from a dead Chrome is common.
pub(crate) async fn running_cdp_endpoint(cfg: &LaunchConfig) -> Option<String> {
    let port_file = cfg.profile_dir.join("DevToolsActivePort");
    let contents = std::fs::read_to_string(&port_file).ok()?;
    let port: u16 = contents.lines().next()?.trim().parse().ok()?;
    let url = format!("http://127.0.0.1:{port}");
    // Probe /json/version — only return the endpoint if Chrome answers.
    let ok = reqwest::Client::new()
        .get(format!("{url}/json/version"))
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    ok.then_some(url)
}

/// Attach as a SECOND CDP client to an already-running Chrome on this profile,
/// without owning or launching a process. Returns the connected [`Browser`]
/// (handler task spawned), or `None` if no live Chrome is reachable on the
/// profile's `DevToolsActivePort`.
///
/// This is the seam the daemon's `/v1/browser/screencast` + `/v1/browser/input`
/// endpoints use to mirror the agent's live browser: they attach to the SAME
/// Chrome the agent is already driving, never launch their own (a second
/// instance would fight the profile's `SingletonLock` and wouldn't be the page
/// the agent controls). Because [`Browser::connect`] owns no child process,
/// dropping the returned [`Browser`] closes only the CDP websocket — it does
/// NOT kill the agent's Chrome.
pub async fn attach_running(cfg: &LaunchConfig) -> Option<Browser> {
    let endpoint = running_cdp_endpoint(cfg).await?;
    match Browser::connect(endpoint.clone()).await {
        Ok((browser, handler)) => {
            spawn_handler(handler);
            tracing::info!(%endpoint, "attached to running Chrome (screencast/input)");
            Some(browser)
        }
        Err(e) => {
            tracing::warn!(error = %e, %endpoint, "attach to running Chrome failed");
            None
        }
    }
}

/// Connect to (attach) or launch Chrome. ATTACH-FIRST: if a Chrome is already
/// alive on this profile we connect to it over CDP rather than launching a
/// second instance (which would fail on the profile's SingletonLock). Only when
/// nothing is running do we launch fresh.
pub async fn launch(cfg: &LaunchConfig) -> Result<LaunchedChrome, BrowserError> {
    if let Some(endpoint) = running_cdp_endpoint(cfg).await {
        match Browser::connect(endpoint.clone()).await {
            Ok((browser, handler)) => {
                spawn_handler(handler);
                tracing::info!(%endpoint, "attached to already-running Chrome");
                return Ok(LaunchedChrome { browser });
            }
            Err(e) => {
                tracing::warn!(error = %e, "attach to running Chrome failed; launching fresh");
            }
        }
    }
    launch_fresh(cfg).await
}

/// Launch a brand-new Chrome via chromiumoxide using our flag set.
async fn launch_fresh(cfg: &LaunchConfig) -> Result<LaunchedChrome, BrowserError> {
    // NOTE: chromiumoxide's `.arg()` parses a bare flag (no leading `--`); it
    // adds the dashes itself. And it injects `--disable-extensions` UNLESS you
    // register extensions via `.extension()`, which also emits `--load-extension`.
    //
    // We `.disable_default_args()` to strip chromiumoxide's automation tells —
    // notably `enable-automation` (sites like Google detect it and refuse
    // sign-in) and `disable-sync` (logs the profile out of Chrome sync). Then we
    // re-add ONLY the harmless flags we actually want.
    let mut builder = BrowserConfig::builder()
        .user_data_dir(&cfg.profile_dir)
        .disable_default_args();
    if let Some(exe) = &cfg.chrome_executable {
        builder = builder.chrome_executable(exe);
    }
    let mut builder = builder
        .arg("no-first-run")
        .arg("no-default-browser-check")
        // Hides the navigator.webdriver tell so login flows behave normally.
        .arg("disable-blink-features=AutomationControlled");
    if let Some(profile) = &cfg.profile_directory {
        builder = builder.arg(format!("profile-directory={profile}"));
    }
    if cfg.headless {
        builder = builder.arg("headless=new");
    } else {
        builder = builder.with_head();
    }
    if let Some(ext) = &cfg.extension_dir {
        builder = builder.extension(ext.display().to_string());
        // Recent Chrome (127+) ignores --load-extension unless this feature
        // kill-switch is set. Without it the extension silently never loads.
        builder = builder.arg("disable-features=DisableLoadExtensionCommandLineSwitch");
    }
    let config = builder
        .build()
        .map_err(|e| BrowserError::Launch(e.to_string()))?;

    let (browser, handler) = Browser::launch(config)
        .await
        .map_err(|e| BrowserError::Launch(e.to_string()))?;
    spawn_handler(handler);
    Ok(LaunchedChrome { browser })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

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
        pid: i32,
        identity: String,
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
            if self.armed
                && process_command(self.pid).is_some_and(|command| command.contains(&self.identity))
            {
                // SAFETY: the PID still identifies this test's uniquely-named
                // executable; a reused unrelated PID is never signalled.
                unsafe {
                    libc::kill(self.pid, libc::SIGKILL);
                }
            }
        }
    }

    #[cfg(unix)]
    fn scratch_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ocean-browser-launch-{nanos:x}-{count}"));
        std::fs::create_dir_all(&dir).expect("create browser launch scratch dir");
        dir
    }

    #[cfg(unix)]
    async fn wait_for_pid(path: &Path) -> i32 {
        for _ in 0..100 {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    return pid;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!(
            "fake browser PID marker was not written: {}",
            path.display()
        );
    }

    #[cfg(unix)]
    fn process_command(pid: i32) -> Option<String> {
        let output = std::process::Command::new("ps")
            .args(["-o", "command=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    #[cfg(unix)]
    fn process_is_running(pid: i32) -> bool {
        // SAFETY: signal 0 probes process existence without sending a signal.
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

    #[test]
    fn flags_include_profile_and_extension() {
        let cfg = LaunchConfig {
            profile_dir: Path::new("/tmp/ocean-profile").to_path_buf(),
            profile_directory: None,
            extension_dir: Some(Path::new("/tmp/ocean-ext").to_path_buf()),
            chrome_executable: None,
            headless: false,
            port: 0,
        };
        let args = cfg.to_args();
        assert!(args
            .iter()
            .any(|a| a == "--user-data-dir=/tmp/ocean-profile"));
        assert!(args.iter().any(|a| a == "--load-extension=/tmp/ocean-ext"));
        assert!(args
            .iter()
            .any(|a| a.starts_with("--remote-debugging-port=")));
        assert!(!args.iter().any(|a| a == "--headless=new"));
        assert!(!args.iter().any(|a| a.starts_with("--profile-directory")));
    }

    #[test]
    fn headless_adds_flag() {
        let cfg = LaunchConfig {
            profile_dir: Path::new("/tmp/p").to_path_buf(),
            profile_directory: None,
            extension_dir: None,
            chrome_executable: None,
            headless: true,
            port: 9333,
        };
        let args = cfg.to_args();
        assert!(args.iter().any(|a| a == "--headless=new"));
        assert!(args.iter().any(|a| a == "--remote-debugging-port=9333"));
        assert!(!args.iter().any(|a| a.starts_with("--load-extension")));
    }

    #[test]
    fn flags_include_profile_directory_when_set() {
        let cfg = LaunchConfig {
            profile_dir: Path::new("/tmp/real-chrome").to_path_buf(),
            profile_directory: Some("Default".to_string()),
            extension_dir: None,
            chrome_executable: None,
            headless: false,
            port: 0,
        };
        let args = cfg.to_args();
        assert!(args.iter().any(|a| a == "--user-data-dir=/tmp/real-chrome"));
        assert!(args.iter().any(|a| a == "--profile-directory=Default"));
    }

    /// Chromiumoxide owns a kill-on-drop child from the moment it spawns the
    /// executable. Cancelling Ocean's launch future must therefore terminate a
    /// process that stalls before publishing its DevTools websocket endpoint.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_browser_launch_does_not_orphan_spawned_process() {
        use std::os::unix::fs::PermissionsExt;

        let dir = scratch_dir();
        let pid_file = dir.join("browser.pid");
        let executable = dir.join("fake-browser");
        let fifo = dir.join("block.fifo");
        let profile = dir.join("profile");
        std::fs::create_dir_all(&profile).expect("create fake browser profile");
        let mkfifo = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo for fake browser");
        assert!(mkfifo.success(), "mkfifo must create the launch blocker");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"{}\"\nread _ < \"{}\"\n",
                pid_file.display(),
                fifo.display()
            ),
        )
        .expect("write fake browser executable");
        let mut permissions = std::fs::metadata(&executable)
            .expect("stat fake browser executable")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions)
            .expect("make fake browser executable runnable");

        let cfg = LaunchConfig {
            profile_dir: profile,
            profile_directory: None,
            extension_dir: None,
            chrome_executable: Some(executable.clone()),
            headless: true,
            port: 0,
        };
        let mut task = AbortTaskOnDrop::new(tokio::spawn(async move { launch(&cfg).await }));
        let pid = wait_for_pid(&pid_file).await;
        let identity = executable
            .canonicalize()
            .expect("canonicalize fake browser identity")
            .to_string_lossy()
            .to_string();
        let mut cleanup = PidCleanup {
            pid,
            identity: identity.clone(),
            armed: true,
        };
        assert!(
            process_is_running(pid),
            "fake browser must be live before cancellation"
        );
        assert!(
            process_command(pid).is_some_and(|command| command.contains(&identity)),
            "PID {pid} must still identify the fake browser before cancellation"
        );

        task.abort();
        let join_result = tokio::time::timeout(std::time::Duration::from_secs(1), task.join())
            .await
            .expect("cancelled browser launch joins before outer deadline");
        let join = match join_result {
            Err(error) => error,
            Ok(_) => panic!("cancelled browser launch task must abort"),
        };
        assert!(join.is_cancelled());
        assert!(
            wait_until_not_running(pid).await,
            "fake browser process {pid} survived cancellation"
        );
        cleanup.disarm();
    }
}
