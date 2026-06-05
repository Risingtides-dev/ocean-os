//! Agent-facing browser tools. Thin wrappers over `ocean_browser::BrowserHandle`.
//! Every tool is permission-gated (except read-only perception/inspect) and
//! emits a `BrowserActivity { active: true }` side-effect so the daemon can
//! drive the side-panel handoff.

pub mod input;
pub mod inspect;
pub mod nav;
pub mod network;
pub mod perceive;
pub mod tabs;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ocean_browser::{BrowserHandle, LaunchConfig};
use tokio::sync::Mutex;

use crate::capability::{CapabilityProvider, ProviderHealth, SessionContext, SharedTool};
use crate::types::{AgentTool, AgentToolResult, ToolSideEffect};

/// A lazily-launched, shared Chrome handle. Tools hold this and call
/// `.get().await` only when they actually run — so merely LISTING the browser
/// tools (which happens on every turn) never launches Chrome. Chrome boots on
/// the first real browser action, and is reused after.
#[derive(Clone)]
pub struct LazyBrowser {
    cfg: Arc<LaunchConfig>,
    handle: Arc<Mutex<Option<Arc<BrowserHandle>>>>,
}

impl LazyBrowser {
    pub fn new(cfg: LaunchConfig) -> Self {
        Self {
            cfg: Arc::new(cfg),
            handle: Arc::new(Mutex::new(None)),
        }
    }

    /// Get-or-launch the shared browser. Called from inside a tool's execute().
    /// If the cached handle's browser process/websocket is dead we drop it and
    /// re-launch — this is the fix for "CDP receiver is gone" / stale handles
    /// persisting across turns.
    pub async fn get(&self) -> Result<Arc<BrowserHandle>, String> {
        let mut guard = self.handle.lock().await;
        if let Some(h) = guard.as_ref() {
            if h.is_alive().await {
                return Ok(h.clone());
            }
            tracing::warn!("cached browser handle is dead; dropping and re-launching");
        }
        let h = Arc::new(
            BrowserHandle::launch((*self.cfg).clone())
                .await
                .map_err(|e| format!("could not start browser: {e}"))?,
        );
        *guard = Some(h.clone());
        Ok(h)
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
        Arc::new(network::BrowserResponseBodyTool { ctx }),
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
