//! Agent-facing browser tools. Thin wrappers over `ocean_browser::BrowserHandle`.
//!
//! **Permission contract:** actuation tools (navigate, click, type, key, eval_js,
//! tab open/switch/close, capture start, enable-downloads) are permission-gated.
//! Read-only perception/inspect (read_page, screenshot, console, network, scroll)
//! and the read-only shell listers are permission-free.
//!
//! **BrowserActivity contract:** every tool that performs a *live browser action*
//! (a CDP round-trip to the running Chrome) emits a `BrowserActivity { active: true }`
//! side-effect so the daemon can drive the side-panel handoff. This includes the
//! read-only-but-live tools `browser_list_tabs` (enumerates tabs over CDP) and
//! `browser_response_body` (issues `Network.getResponseBody`).
//!
//! Two tools are *exempt* because they read a purely in-memory buffer with no CDP
//! round-trip — the live action that populated the buffer already flagged activity:
//!   - `browser_captured_requests` (reads the netcap snapshot)
//!   - `browser_downloads` (reads the download-tracking snapshot)

pub mod downloads;
pub mod input;
pub mod inspect;
pub mod nav;
pub mod network;
pub mod perceive;
pub mod tabs;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ocean_browser::{BrowserHandle, LaunchConfig};
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration, Instant};

use crate::capability::{CapabilityProvider, ProviderHealth, SessionContext, SharedTool};
use crate::types::{AgentTool, AgentToolResult, ToolSideEffect};

const BROWSER_SINGLE_FLIGHT_TIMEOUT: Duration = Duration::from_secs(40);
const BROWSER_LIVENESS_TIMEOUT: Duration = Duration::from_secs(3);
const BROWSER_LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
struct BrowserDeadlines {
    single_flight: Duration,
    liveness: Duration,
    launch: Duration,
}

const BROWSER_DEADLINES: BrowserDeadlines = BrowserDeadlines {
    single_flight: BROWSER_SINGLE_FLIGHT_TIMEOUT,
    liveness: BROWSER_LIVENESS_TIMEOUT,
    launch: BROWSER_LAUNCH_TIMEOUT,
};

struct BrowserSlot<H> {
    handle: Option<Arc<H>>,
    /// Completion time of the last successful liveness probe or launch. A
    /// caller that began before this instant can consume that completed flight
    /// without serially probing the same handle again.
    validated_at: Option<Instant>,
}

impl<H> Default for BrowserSlot<H> {
    fn default() -> Self {
        Self {
            handle: None,
            validated_at: None,
        }
    }
}

/// Testable single-flight state machine shared by the production browser and
/// deterministic fake handles. The mutex deliberately spans probe + launch so
/// concurrent callers cannot launch duplicate Chrome instances. Every awaited
/// phase is bounded, and cancellation drops the guard/futures without caching a
/// partial launch, leaving the slot retryable.
async fn get_or_launch_with<H, IsAlive, IsAliveFuture, Launch, LaunchFuture>(
    slot: &Mutex<BrowserSlot<H>>,
    deadlines: BrowserDeadlines,
    is_alive: IsAlive,
    launch: Launch,
) -> Result<Arc<H>, String>
where
    H: Send + Sync + 'static,
    IsAlive: FnOnce(Arc<H>) -> IsAliveFuture + Send,
    IsAliveFuture: Future<Output = bool> + Send,
    Launch: FnOnce() -> LaunchFuture + Send,
    LaunchFuture: Future<Output = Result<H, String>> + Send,
{
    let requested_at = Instant::now();
    let mut guard = timeout(deadlines.single_flight, slot.lock())
        .await
        .map_err(|_| {
            format!(
                "browser startup single-flight wait timed out after {}ms",
                deadlines.single_flight.as_millis()
            )
        })?;

    if let (Some(handle), Some(validated_at)) = (guard.handle.as_ref().cloned(), guard.validated_at)
    {
        if validated_at >= requested_at {
            return Ok(handle);
        }
    }

    if let Some(handle) = guard.handle.as_ref().cloned() {
        match timeout(deadlines.liveness, is_alive(handle.clone())).await {
            Ok(true) => {
                guard.validated_at = Some(Instant::now());
                return Ok(handle);
            }
            Ok(false) => {
                tracing::warn!("cached browser handle is dead; dropping and re-launching");
            }
            Err(_) => {
                return Err(format!(
                    "browser liveness check timed out after {}ms",
                    deadlines.liveness.as_millis()
                ));
            }
        }
    }

    let launched = timeout(deadlines.launch, launch()).await.map_err(|_| {
        format!(
            "browser launch timed out after {}ms",
            deadlines.launch.as_millis()
        )
    })??;
    let handle = Arc::new(launched);
    guard.handle = Some(handle.clone());
    guard.validated_at = Some(Instant::now());
    Ok(handle)
}

/// A lazily-launched, shared Chrome handle. Tools hold this and call
/// `.get().await` only when they actually run — so merely LISTING the browser
/// tools (which happens on every turn) never launches Chrome. Chrome boots on
/// the first real browser action, and is reused after.
#[derive(Clone)]
pub struct LazyBrowser {
    cfg: Arc<LaunchConfig>,
    handle: Arc<Mutex<BrowserSlot<BrowserHandle>>>,
}

impl LazyBrowser {
    pub fn new(cfg: LaunchConfig) -> Self {
        Self {
            cfg: Arc::new(cfg),
            handle: Arc::new(Mutex::new(BrowserSlot::default())),
        }
    }

    /// Get-or-launch the shared browser. Called from inside a tool's execute().
    /// If the cached handle's browser process/websocket is dead we drop it and
    /// re-launch — this is the fix for "CDP receiver is gone" / stale handles
    /// persisting across turns.
    pub async fn get(&self) -> Result<Arc<BrowserHandle>, String> {
        let cfg = self.cfg.clone();
        get_or_launch_with(
            &self.handle,
            BROWSER_DEADLINES,
            |handle| async move { handle.is_alive().await },
            move || async move {
                BrowserHandle::launch((*cfg).clone())
                    .await
                    .map_err(|error| format!("could not start browser: {error}"))
            },
        )
        .await
    }
}

/// Shared dependency injected into every browser tool. Holds a LAZY browser —
/// listing tools never launches Chrome; the first tool that runs does.
#[derive(Clone)]
pub struct BrowserToolCtx {
    pub lazy: LazyBrowser,
}

/// Build a text result that also flags browser activity for the handoff.
fn active_result(text: impl Into<String>) -> AgentToolResult {
    let mut r = AgentToolResult::text(text);
    r.side_effects
        .push(ToolSideEffect::BrowserActivity { active: true });
    r
}

/// Construct the full browser tool suite bound to a live handle.
pub fn browser_tools(ctx: BrowserToolCtx) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(nav::BrowserNavigateTool { ctx: ctx.clone() }),
        Arc::new(perceive::BrowserReadPageTool { ctx: ctx.clone() }),
        Arc::new(perceive::BrowserScreenshotTool { ctx: ctx.clone() }),
        Arc::new(input::BrowserClickTool { ctx: ctx.clone() }),
        Arc::new(input::BrowserTypeTool { ctx: ctx.clone() }),
        Arc::new(input::BrowserKeyTool { ctx: ctx.clone() }),
        Arc::new(input::BrowserScrollTool { ctx: ctx.clone() }),
        Arc::new(inspect::BrowserEvalJsTool { ctx: ctx.clone() }),
        Arc::new(inspect::BrowserConsoleTool { ctx: ctx.clone() }),
        Arc::new(inspect::BrowserNetworkTool { ctx: ctx.clone() }),
        // Shell layer — tab control (the Layer-3 jump).
        Arc::new(tabs::BrowserListTabsTool { ctx: ctx.clone() }),
        Arc::new(tabs::BrowserOpenTabTool { ctx: ctx.clone() }),
        Arc::new(tabs::BrowserSwitchTabTool { ctx: ctx.clone() }),
        Arc::new(tabs::BrowserCloseTabTool { ctx: ctx.clone() }),
        // Network capture — the scraping unlock (read real response bodies).
        Arc::new(network::BrowserCaptureNetworkTool { ctx: ctx.clone() }),
        Arc::new(network::BrowserCapturedRequestsTool { ctx: ctx.clone() }),
        Arc::new(network::BrowserResponseBodyTool { ctx: ctx.clone() }),
        // Downloads — file flows into the agent's hands (Layer 3).
        Arc::new(downloads::BrowserEnableDownloadsTool { ctx: ctx.clone() }),
        Arc::new(downloads::BrowserDownloadsTool { ctx }),
    ]
}

/// Capability provider that advertises the browser tools WITHOUT launching
/// Chrome. The tools share one [`LazyBrowser`]; Chrome boots on the first
/// actual browser action and is reused afterward. Listing tools (which happens
/// on every single turn) is therefore free — a "what's 2+2" turn never starts
/// a browser.
pub struct BrowserProvider {
    lazy: LazyBrowser,
}

impl BrowserProvider {
    /// Build a provider. `profile_dir` is Chrome's user-data dir;
    /// `profile_directory` is the sub-profile (e.g. "Default"); `extension_dir`
    /// (if it exists) preloads the Ocean cockpit extension; `chrome_executable`
    /// should point at Chrome for Testing so the extension auto-loads.
    pub fn new(
        profile_dir: PathBuf,
        profile_directory: Option<String>,
        extension_dir: Option<PathBuf>,
        chrome_executable: Option<PathBuf>,
    ) -> Self {
        Self {
            lazy: LazyBrowser::new(LaunchConfig {
                profile_dir,
                profile_directory,
                extension_dir,
                chrome_executable,
                headless: false,
                port: 0,
            }),
        }
    }
}

#[async_trait]
impl CapabilityProvider for BrowserProvider {
    fn id(&self) -> &str {
        "browser"
    }

    async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
        // No launch here — just hand the tools a clone of the lazy browser.
        browser_tools(BrowserToolCtx {
            lazy: self.lazy.clone(),
        })
    }

    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::oneshot;

    #[derive(Debug)]
    struct FakeHandle {
        id: usize,
        alive: bool,
    }

    impl FakeHandle {
        fn new(id: usize, alive: bool) -> Self {
            Self { id, alive }
        }
    }

    #[derive(Clone)]
    struct LaunchCounters {
        attempts: Arc<AtomicUsize>,
        active: Arc<AtomicUsize>,
        dropped: Arc<AtomicUsize>,
    }

    impl LaunchCounters {
        fn new() -> Self {
            Self {
                attempts: Arc::new(AtomicUsize::new(0)),
                active: Arc::new(AtomicUsize::new(0)),
                dropped: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    struct LaunchLease {
        counters: LaunchCounters,
    }

    impl LaunchLease {
        fn new(counters: LaunchCounters) -> Self {
            counters.active.fetch_add(1, Ordering::SeqCst);
            Self { counters }
        }
    }

    impl Drop for LaunchLease {
        fn drop(&mut self) {
            self.counters.active.fetch_sub(1, Ordering::SeqCst);
            self.counters.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn test_deadlines() -> BrowserDeadlines {
        BrowserDeadlines {
            single_flight: Duration::from_secs(1),
            liveness: Duration::from_millis(100),
            launch: Duration::from_millis(250),
        }
    }

    fn fake_slot(handle: Option<Arc<FakeHandle>>) -> Mutex<BrowserSlot<FakeHandle>> {
        Mutex::new(BrowserSlot {
            handle,
            validated_at: None,
        })
    }

    #[tokio::test]
    async fn lazy_browser_healthy_handle_skips_launch() {
        let existing = Arc::new(FakeHandle::new(7, true));
        let slot = fake_slot(Some(existing.clone()));
        let launches = Arc::new(AtomicUsize::new(0));
        let launch_count = launches.clone();

        let returned = get_or_launch_with(
            &slot,
            test_deadlines(),
            |handle| async move { handle.alive },
            move || async move {
                launch_count.fetch_add(1, Ordering::SeqCst);
                Ok(FakeHandle::new(8, true))
            },
        )
        .await
        .expect("healthy cached handle should be reused");

        assert!(Arc::ptr_eq(&returned, &existing));
        assert_eq!(launches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn lazy_browser_dead_handle_launches_once_and_replaces_cache() {
        let existing = Arc::new(FakeHandle::new(1, false));
        let slot = fake_slot(Some(existing));
        let launches = Arc::new(AtomicUsize::new(0));
        let launch_count = launches.clone();

        let returned = get_or_launch_with(
            &slot,
            test_deadlines(),
            |handle| async move { handle.alive },
            move || async move {
                launch_count.fetch_add(1, Ordering::SeqCst);
                Ok(FakeHandle::new(2, true))
            },
        )
        .await
        .expect("dead handle should be replaced");

        assert_eq!(returned.id, 2);
        assert_eq!(launches.load(Ordering::SeqCst), 1);
        let cached = slot
            .lock()
            .await
            .handle
            .as_ref()
            .cloned()
            .expect("cache filled");
        assert!(Arc::ptr_eq(&returned, &cached));
    }

    #[tokio::test]
    async fn lazy_browser_concurrent_callers_observe_exactly_one_launch() {
        let slot = Arc::new(fake_slot(None));
        let launches = Arc::new(AtomicUsize::new(0));
        let mut callers = Vec::new();

        for _ in 0..8 {
            let slot = slot.clone();
            let launches = launches.clone();
            callers.push(tokio::spawn(async move {
                get_or_launch_with(
                    &slot,
                    test_deadlines(),
                    |handle: Arc<FakeHandle>| async move { handle.alive },
                    move || async move {
                        let id = launches.fetch_add(1, Ordering::SeqCst) + 1;
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok(FakeHandle::new(id, true))
                    },
                )
                .await
            }));
        }

        for caller in callers {
            let handle = caller
                .await
                .expect("caller task did not panic")
                .expect("caller got shared handle");
            assert_eq!(handle.id, 1);
        }
        assert_eq!(launches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lazy_browser_waiters_consume_completed_flight_without_serial_reprobe() {
        let slot = Arc::new(fake_slot(None));
        let mut held = slot.lock().await;
        let liveness_checks = Arc::new(AtomicUsize::new(0));
        let launches = Arc::new(AtomicUsize::new(0));
        let mut callers = Vec::new();

        for _ in 0..8 {
            let slot = slot.clone();
            let liveness_checks = liveness_checks.clone();
            let launches = launches.clone();
            callers.push(tokio::spawn(async move {
                get_or_launch_with(
                    &slot,
                    BrowserDeadlines {
                        single_flight: Duration::from_millis(200),
                        liveness: Duration::from_millis(100),
                        launch: Duration::from_millis(150),
                    },
                    move |_handle: Arc<FakeHandle>| async move {
                        liveness_checks.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(80)).await;
                        true
                    },
                    move || async move {
                        launches.fetch_add(1, Ordering::SeqCst);
                        Ok(FakeHandle::new(99, true))
                    },
                )
                .await
            }));
        }

        // All callers record their request time before blocking on this held
        // flight. Publish its successful result near their waiter deadline.
        tokio::time::sleep(Duration::from_millis(120)).await;
        held.handle = Some(Arc::new(FakeHandle::new(42, true)));
        held.validated_at = Some(Instant::now());
        drop(held);

        for caller in callers {
            let handle = timeout(Duration::from_millis(80), caller)
                .await
                .expect("completed-flight waiter returned promptly")
                .expect("waiter task did not panic")
                .expect("waiter consumed completed flight");
            assert_eq!(handle.id, 42);
        }
        assert_eq!(liveness_checks.load(Ordering::SeqCst), 0);
        assert_eq!(launches.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn lazy_browser_stalled_single_flight_wait_is_bounded() {
        let slot = fake_slot(None);
        let held = slot.lock().await;
        let launches = Arc::new(AtomicUsize::new(0));
        let launch_count = launches.clone();
        let deadlines = BrowserDeadlines {
            single_flight: Duration::from_millis(40),
            ..test_deadlines()
        };

        let error = get_or_launch_with(
            &slot,
            deadlines,
            |handle: Arc<FakeHandle>| async move { handle.alive },
            move || async move {
                launch_count.fetch_add(1, Ordering::SeqCst);
                Ok(FakeHandle::new(3, true))
            },
        )
        .await
        .expect_err("stalled single-flight owner must hit the waiter deadline");

        assert!(error.contains("single-flight wait timed out"), "{error}");
        assert_eq!(launches.load(Ordering::SeqCst), 0);
        drop(held);
    }

    #[tokio::test]
    async fn lazy_browser_stalled_liveness_is_bounded_and_preserves_cache() {
        let existing = Arc::new(FakeHandle::new(3, true));
        let slot = fake_slot(Some(existing.clone()));
        let launches = Arc::new(AtomicUsize::new(0));
        let launch_count = launches.clone();
        let deadlines = BrowserDeadlines {
            liveness: Duration::from_millis(40),
            ..test_deadlines()
        };

        let error = get_or_launch_with(
            &slot,
            deadlines,
            |_handle| async move {
                std::future::pending::<()>().await;
                true
            },
            move || async move {
                launch_count.fetch_add(1, Ordering::SeqCst);
                Ok(FakeHandle::new(4, true))
            },
        )
        .await
        .expect_err("stalled liveness must hit its deadline");

        assert!(error.contains("liveness check timed out"), "{error}");
        assert_eq!(launches.load(Ordering::SeqCst), 0);
        let cached = slot
            .lock()
            .await
            .handle
            .as_ref()
            .cloned()
            .expect("cache kept");
        assert!(Arc::ptr_eq(&cached, &existing));

        let retry = get_or_launch_with(
            &slot,
            test_deadlines(),
            |handle| async move { handle.alive },
            || async move { Ok(FakeHandle::new(5, true)) },
        )
        .await
        .expect("state remains retryable after liveness timeout");
        assert!(Arc::ptr_eq(&retry, &existing));
    }

    #[tokio::test]
    async fn lazy_browser_stalled_launch_is_bounded_dropped_and_retryable() {
        let slot = fake_slot(None);
        let counters = LaunchCounters::new();
        let launch_counters = counters.clone();
        let deadlines = BrowserDeadlines {
            launch: Duration::from_millis(40),
            ..test_deadlines()
        };

        let error = get_or_launch_with(
            &slot,
            deadlines,
            |handle: Arc<FakeHandle>| async move { handle.alive },
            move || async move {
                launch_counters.attempts.fetch_add(1, Ordering::SeqCst);
                let _lease = LaunchLease::new(launch_counters);
                std::future::pending::<()>().await;
                Ok(FakeHandle::new(6, true))
            },
        )
        .await
        .expect_err("stalled launch must hit its deadline");

        assert!(error.contains("launch timed out"), "{error}");
        assert_eq!(counters.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(counters.active.load(Ordering::SeqCst), 0);
        assert_eq!(counters.dropped.load(Ordering::SeqCst), 1);
        assert!(
            slot.lock().await.handle.is_none(),
            "partial launch was not cached"
        );

        let retry_counters = counters.clone();
        let retry = get_or_launch_with(
            &slot,
            test_deadlines(),
            |handle: Arc<FakeHandle>| async move { handle.alive },
            move || async move {
                retry_counters.attempts.fetch_add(1, Ordering::SeqCst);
                Ok(FakeHandle::new(7, true))
            },
        )
        .await
        .expect("launch state remains retryable after timeout");
        assert_eq!(retry.id, 7);
        assert_eq!(counters.attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn lazy_browser_cancelled_launch_drops_inflight_work_and_unlocks_retry() {
        let slot = Arc::new(fake_slot(None));
        let counters = LaunchCounters::new();
        let (started_tx, started_rx) = oneshot::channel();
        let task_slot = slot.clone();
        let task_counters = counters.clone();

        let task = tokio::spawn(async move {
            get_or_launch_with(
                &task_slot,
                BrowserDeadlines {
                    launch: Duration::from_millis(500),
                    ..test_deadlines()
                },
                |handle: Arc<FakeHandle>| async move { handle.alive },
                move || async move {
                    task_counters.attempts.fetch_add(1, Ordering::SeqCst);
                    let _lease = LaunchLease::new(task_counters);
                    let _ = started_tx.send(());
                    std::future::pending::<()>().await;
                    Ok(FakeHandle::new(8, true))
                },
            )
            .await
        });

        timeout(Duration::from_secs(1), started_rx)
            .await
            .expect("launch started before test deadline")
            .expect("launch start sender was not dropped");
        task.abort();
        let join = timeout(Duration::from_secs(1), task)
            .await
            .expect("cancelled caller joins before outer deadline")
            .expect_err("cancelled caller must abort");
        assert!(join.is_cancelled());
        assert_eq!(counters.active.load(Ordering::SeqCst), 0);
        assert_eq!(counters.dropped.load(Ordering::SeqCst), 1);
        assert!(
            slot.lock().await.handle.is_none(),
            "cancelled launch was not cached"
        );

        let retry_counters = counters.clone();
        let retry = get_or_launch_with(
            &slot,
            test_deadlines(),
            |handle: Arc<FakeHandle>| async move { handle.alive },
            move || async move {
                retry_counters.attempts.fetch_add(1, Ordering::SeqCst);
                Ok(FakeHandle::new(9, true))
            },
        )
        .await
        .expect("mutex and cache remain retryable after cancellation");
        assert_eq!(retry.id, 9);
        assert_eq!(counters.attempts.load(Ordering::SeqCst), 2);
    }
}
