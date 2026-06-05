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
fn spawn_handler(mut handler: chromiumoxide::Handler) {
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
async fn running_cdp_endpoint(cfg: &LaunchConfig) -> Option<String> {
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
}
