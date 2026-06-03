//! Chrome discovery + launch flag assembly. The flag list is pure and unit
//! tested; the actual spawn is delegated to chromiumoxide.

use std::path::PathBuf;

use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;

use crate::error::BrowserError;

/// Inputs that determine how Chrome is launched.
#[derive(Debug, Clone)]
pub struct LaunchConfig {
    /// Persistent profile dir so logins survive restarts.
    pub profile_dir: PathBuf,
    /// Unpacked extension to preload (the Ocean cockpit). None in tests/headless.
    pub extension_dir: Option<PathBuf>,
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

/// Launch Chrome via chromiumoxide using our flag set. Spawns the required
/// CDP event-handler task internally.
pub async fn launch(cfg: &LaunchConfig) -> Result<LaunchedChrome, BrowserError> {
    // NOTE: chromiumoxide's `.arg()` parses a bare flag (no leading `--`); it
    // adds the dashes itself. And it injects `--disable-extensions` UNLESS you
    // register extensions via `.extension()`, which also emits `--load-extension`.
    let mut builder = BrowserConfig::builder()
        .user_data_dir(&cfg.profile_dir)
        .arg("no-first-run")
        .arg("no-default-browser-check");
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

    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| BrowserError::Launch(e.to_string()))?;

    // The handler future must be polled for CDP to make progress.
    tokio::spawn(async move {
        while let Some(ev) = handler.next().await {
            if ev.is_err() {
                break;
            }
        }
    });

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
            extension_dir: Some(Path::new("/tmp/ocean-ext").to_path_buf()),
            headless: false,
            port: 0,
        };
        let args = cfg.to_args();
        assert!(args.iter().any(|a| a == "--user-data-dir=/tmp/ocean-profile"));
        assert!(args.iter().any(|a| a == "--load-extension=/tmp/ocean-ext"));
        assert!(args.iter().any(|a| a.starts_with("--remote-debugging-port=")));
        assert!(!args.iter().any(|a| a == "--headless=new"));
    }

    #[test]
    fn headless_adds_flag() {
        let cfg = LaunchConfig {
            profile_dir: Path::new("/tmp/p").to_path_buf(),
            extension_dir: None,
            headless: true,
            port: 9333,
        };
        let args = cfg.to_args();
        assert!(args.iter().any(|a| a == "--headless=new"));
        assert!(args.iter().any(|a| a == "--remote-debugging-port=9333"));
        assert!(!args.iter().any(|a| a.starts_with("--load-extension")));
    }
}
