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
    /// Resolved absolute path once known (set as progress/completion lands).
    pub file_path: Option<String>,
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
            // CDP gives an explicit path on completion; otherwise derive it from
            // the download dir + suggested name so the agent has a path to use.
            if let Some(p) = file_path {
                info.file_path = Some(p);
            } else if info.file_path.is_none() {
                info.file_path = Some(self.dir.join(&info.suggested_filename).to_string_lossy().into_owned());
            }
        }
    }

    async fn snapshot(&self) -> Vec<DownloadInfo> {
        self.items.lock().await.values().cloned().collect()
    }
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
