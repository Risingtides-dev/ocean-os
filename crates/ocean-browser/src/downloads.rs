//! **Downloads** — Layer-3 shell control. The existing tools can't see a
//! download at all; this makes downloads first-class: the agent enables
//! downloading to a known dir, triggers one (by clicking/navigating), and reads
//! back the completed file path. That turns "click a download link" into "fetch
//! that file and use it" — the file flows into the agent's hands.
//!
//! Mechanism: set the CDP download behavior to a known directory, then watch the
//! `Browser.downloadProgress` event stream and track each download by guid until
//! it reaches the `completed` state.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{BrowserError, BrowserHandle, Result};

/// State of a single download, keyed by its CDP guid.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum DownloadState {
    InProgress,
    Completed,
    Canceled,
}

/// A tracked download.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadInfo {
    pub guid: String,
    pub url: String,
    pub suggested_filename: String,
    pub state: DownloadState,
    /// Resolved absolute path. This is validated against disk in
    /// [`DownloadTracker::snapshot`]: it points at a file that actually exists
    /// (the CDP/derived path if present, otherwise the real browser-renamed
    /// collision variant), or is `None` when no matching file is on disk.
    pub file_path: Option<String>,
    /// Whether `file_path` was confirmed to exist on disk at snapshot time. When
    /// `false`, `file_path` is `None` and the agent should not try to read it.
    pub exists: bool,
    pub received_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Clone)]
pub struct DownloadTracker {
    dir: PathBuf,
    items: Arc<Mutex<HashMap<String, DownloadInfo>>>,
}

impl DownloadTracker {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            items: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn begin(&self, guid: String, url: String, suggested_filename: String) {
        self.items.lock().await.insert(
            guid.clone(),
            DownloadInfo {
                guid,
                url,
                suggested_filename,
                state: DownloadState::InProgress,
                file_path: None,
                exists: false,
                received_bytes: 0,
                total_bytes: 0,
            },
        );
    }

    async fn progress(
        &self,
        guid: &str,
        state: DownloadState,
        file_path: Option<String>,
        received: u64,
        total: u64,
    ) {
        let mut items = self.items.lock().await;
        if let Some(info) = items.get_mut(guid) {
            info.state = state;
            info.received_bytes = received;
            info.total_bytes = total;
            // Store CDP's explicit path verbatim as a *hint*. We deliberately do
            // NOT touch the filesystem here — this runs in the CDP event handler
            // while holding the items lock, so resolution/stat happens lazily in
            // `snapshot()` instead. A `None` from CDP leaves any prior hint alone.
            if let Some(p) = file_path {
                info.file_path = Some(p);
            }
        }
    }

    /// Clone out every tracked download, validating each one's `file_path`
    /// against disk so the agent gets a path it can actually read.
    ///
    /// Resolution rule, applied per item (cheap `read_dir`, no lock held):
    /// 1. CDP/derived hint exists on disk → use it, `exists = true`.
    /// 2. Otherwise derive `<dir>/<suggested_filename>` and re-check; if it
    ///    exists, use it.
    /// 3. Otherwise the browser likely renamed a collision (`name (1).ext`,
    ///    `name (2).ext`, …). Scan the dir for variants of the suggested name
    ///    and resolve to the highest-N match that exists.
    /// 4. Nothing on disk → `file_path = None`, `exists = false`.
    async fn snapshot(&self) -> Vec<DownloadInfo> {
        // Clone under the lock, resolve after releasing it: filesystem work
        // never happens while the items mutex is held.
        let mut items: Vec<DownloadInfo> = self.items.lock().await.values().cloned().collect();
        for info in &mut items {
            let (resolved, exists) = self.resolve_path(info);
            info.file_path = resolved;
            info.exists = exists;
        }
        items
    }

    /// Resolve an item's file path against disk. Returns `(path, exists)`.
    fn resolve_path(&self, info: &DownloadInfo) -> (Option<String>, bool) {
        // 1. Trust the hint if it actually exists.
        if let Some(hint) = &info.file_path {
            if std::path::Path::new(hint).is_file() {
                return (Some(hint.clone()), true);
            }
        }
        // 2. Try the plain derived path.
        let derived = self.dir.join(&info.suggested_filename);
        if derived.is_file() {
            return (Some(derived.to_string_lossy().into_owned()), true);
        }
        // 3. Hunt for a browser-renamed collision variant: `stem (N).ext`.
        if let Some(found) = self.find_collision_variant(&info.suggested_filename) {
            return (Some(found.to_string_lossy().into_owned()), true);
        }
        // 4. Nothing on disk.
        (None, false)
    }

    /// Search the download dir for Chrome-style collision renames of
    /// `suggested_filename` (`stem (1).ext`, `stem (2).ext`, …) and return the
    /// highest-numbered existing variant, if any.
    fn find_collision_variant(&self, suggested: &str) -> Option<PathBuf> {
        let suggested = std::path::Path::new(suggested);
        let stem = suggested.file_stem()?.to_str()?;
        // Extension as written, including the dot (or empty for no extension).
        let ext = suggested
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();

        let mut best: Option<(u64, PathBuf)> = None;
        let read = std::fs::read_dir(&self.dir).ok()?;
        for entry in read.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if let Some(n) = collision_index(name, stem, &ext) {
                if best.as_ref().map(|(bn, _)| n > *bn).unwrap_or(true) {
                    best = Some((n, path));
                }
            }
        }
        best.map(|(_, p)| p)
    }
}

/// Parse Chrome's collision-rename convention: given a directory entry `name`,
/// the base `stem`, and the `ext` (with leading dot, or empty), return `Some(N)`
/// if `name == "<stem> (N)<ext>"` for a positive integer N. The exact name
/// (`"<stem><ext>"`) is intentionally NOT matched here — callers check it first.
fn collision_index(name: &str, stem: &str, ext: &str) -> Option<u64> {
    let rest = name.strip_prefix(stem)?;
    let rest = rest.strip_prefix(" (")?;
    let rest = rest.strip_suffix(ext)?;
    let num = rest.strip_suffix(')')?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    num.parse::<u64>().ok()
}

impl BrowserHandle {
    /// Enable downloads to a known directory and start tracking them. Idempotent
    /// per call (resets the tracker). The dir defaults to `<profile>/downloads`
    /// when `dir` is None. Subsequent downloads (triggered by clicking a link or
    /// navigating to a file) are captured and readable via [`downloads`].
    pub async fn enable_downloads(&self, dir: Option<PathBuf>) -> Result<PathBuf> {
        use chromiumoxide::cdp::browser_protocol::browser::{
            DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin,
            SetDownloadBehaviorBehavior, SetDownloadBehaviorParams,
        };

        let dir = dir.unwrap_or_else(default_download_dir);
        std::fs::create_dir_all(&dir).map_err(|e| BrowserError::Cdp(e.to_string()))?;

        let page = self.active_page().await?;
        let params = SetDownloadBehaviorParams::builder()
            .behavior(SetDownloadBehaviorBehavior::Allow)
            .download_path(dir.to_string_lossy().into_owned())
            .build()
            .map_err(BrowserError::Cdp)?;
        page.execute(params)
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;

        let tracker = DownloadTracker::new(dir.clone());

        // downloadWillBegin → register the download.
        let begin_sink = tracker.clone();
        let mut begins = page
            .event_listener::<EventDownloadWillBegin>()
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(ev) = begins.next().await {
                begin_sink
                    .begin(ev.guid.clone(), ev.url.clone(), ev.suggested_filename.clone())
                    .await;
            }
        });

        // downloadProgress → update state until completed/canceled.
        let prog_sink = tracker.clone();
        let mut progs = page
            .event_listener::<EventDownloadProgress>()
            .await
            .map_err(|e| BrowserError::Cdp(e.to_string()))?;
        tokio::spawn(async move {
            use futures::StreamExt;
            while let Some(ev) = progs.next().await {
                let state = match ev.state {
                    DownloadProgressState::Completed => DownloadState::Completed,
                    DownloadProgressState::Canceled => DownloadState::Canceled,
                    DownloadProgressState::InProgress => DownloadState::InProgress,
                };
                prog_sink
                    .progress(
                        &ev.guid,
                        state,
                        ev.file_path.clone(),
                        ev.received_bytes as u64,
                        ev.total_bytes as u64,
                    )
                    .await;
            }
        });

        *self.downloads.lock().await = Some(tracker);
        Ok(dir)
    }

    /// List tracked downloads (state + resolved path). Empty if downloads weren't
    /// enabled.
    pub async fn download_list(&self) -> Vec<DownloadInfo> {
        match self.downloads.lock().await.as_ref() {
            Some(t) => t.snapshot().await,
            None => Vec::new(),
        }
    }
}

/// Default download directory when the caller doesn't pin one: a stable
/// Ocean-owned dir under the OS temp root.
fn default_download_dir() -> PathBuf {
    std::env::temp_dir().join("ocean-downloads")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch download dir under the OS temp root, created on demand.
    fn scratch_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ocean-dl-test-{tag}-{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn collision_index_parses_chrome_renames() {
        // "report (1).pdf" → 1, against stem "report" / ext ".pdf".
        assert_eq!(collision_index("report (1).pdf", "report", ".pdf"), Some(1));
        assert_eq!(collision_index("report (12).pdf", "report", ".pdf"), Some(12));
        // The exact name is not a collision variant.
        assert_eq!(collision_index("report.pdf", "report", ".pdf"), None);
        // Non-numeric / malformed.
        assert_eq!(collision_index("report (x).pdf", "report", ".pdf"), None);
        assert_eq!(collision_index("report (1).txt", "report", ".pdf"), None);
        assert_eq!(collision_index("other (1).pdf", "report", ".pdf"), None);
        // Extensionless suggested name.
        assert_eq!(collision_index("data (3)", "data", ""), Some(3));
    }

    #[tokio::test]
    async fn snapshot_resolves_collision_variant_when_exact_name_missing() {
        let dir = scratch_dir("collision");
        // The browser renamed the collision: only "report (1).pdf" is on disk,
        // NOT the suggested "report.pdf".
        std::fs::write(dir.join("report (1).pdf"), b"pdf-bytes").unwrap();

        let tracker = DownloadTracker::new(dir.clone());
        tracker
            .begin(
                "guid-1".into(),
                "https://example.com/report.pdf".into(),
                "report.pdf".into(),
            )
            .await;
        // Completion with NO explicit CDP path → only the suggested name is known.
        tracker
            .progress("guid-1", DownloadState::Completed, None, 9, 9)
            .await;

        let snap = tracker.snapshot().await;
        assert_eq!(snap.len(), 1);
        let info = &snap[0];
        assert!(info.exists, "resolved file should exist");
        let resolved = info.file_path.as_ref().expect("file_path resolved");
        assert_eq!(
            std::path::Path::new(resolved),
            dir.join("report (1).pdf"),
            "should resolve to the (1) collision variant that actually exists"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn snapshot_picks_highest_numbered_variant() {
        let dir = scratch_dir("highest");
        std::fs::write(dir.join("data (1).csv"), b"a").unwrap();
        std::fs::write(dir.join("data (2).csv"), b"b").unwrap();

        let tracker = DownloadTracker::new(dir.clone());
        tracker
            .begin("g".into(), "https://x/data.csv".into(), "data.csv".into())
            .await;
        tracker
            .progress("g", DownloadState::Completed, None, 1, 1)
            .await;

        let snap = tracker.snapshot().await;
        let info = &snap[0];
        assert!(info.exists);
        assert_eq!(
            std::path::Path::new(info.file_path.as_ref().unwrap()),
            dir.join("data (2).csv"),
            "should resolve to the highest-N variant"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn snapshot_prefers_exact_name_when_present() {
        let dir = scratch_dir("exact");
        std::fs::write(dir.join("file.txt"), b"x").unwrap();
        std::fs::write(dir.join("file (1).txt"), b"y").unwrap();

        let tracker = DownloadTracker::new(dir.clone());
        tracker
            .begin("g".into(), "https://x/file.txt".into(), "file.txt".into())
            .await;
        tracker
            .progress("g", DownloadState::Completed, None, 1, 1)
            .await;

        let snap = tracker.snapshot().await;
        let info = &snap[0];
        assert!(info.exists);
        assert_eq!(
            std::path::Path::new(info.file_path.as_ref().unwrap()),
            dir.join("file.txt"),
            "exact suggested name on disk wins over the (1) variant"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn snapshot_reports_missing_when_nothing_on_disk() {
        let dir = scratch_dir("missing");
        // Nothing written to dir.

        let tracker = DownloadTracker::new(dir.clone());
        tracker
            .begin("g".into(), "https://x/gone.bin".into(), "gone.bin".into())
            .await;
        tracker
            .progress("g", DownloadState::Completed, None, 0, 0)
            .await;

        let snap = tracker.snapshot().await;
        let info = &snap[0];
        assert!(!info.exists, "no file on disk → exists:false");
        assert!(
            info.file_path.is_none(),
            "file_path must be None when nothing is found, not a guess"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn snapshot_trusts_explicit_cdp_path_that_exists() {
        let dir = scratch_dir("explicit");
        let real = dir.join("explicit.bin");
        std::fs::write(&real, b"z").unwrap();

        let tracker = DownloadTracker::new(dir.clone());
        tracker
            .begin("g".into(), "https://x/explicit.bin".into(), "explicit.bin".into())
            .await;
        // CDP handed us the exact final path.
        tracker
            .progress(
                "g",
                DownloadState::Completed,
                Some(real.to_string_lossy().into_owned()),
                1,
                1,
            )
            .await;

        let snap = tracker.snapshot().await;
        let info = &snap[0];
        assert!(info.exists);
        assert_eq!(
            std::path::Path::new(info.file_path.as_ref().unwrap()),
            real
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
