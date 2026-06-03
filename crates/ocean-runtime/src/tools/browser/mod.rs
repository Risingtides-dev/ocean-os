//! Agent-facing browser tools. Thin wrappers over `ocean_browser::BrowserHandle`.
//! Every tool is permission-gated (except read-only perception/inspect) and
//! emits a `BrowserActivity { active: true }` side-effect so the daemon can
//! drive the side-panel handoff.

pub mod inspect;
pub mod input;
pub mod nav;
pub mod perceive;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ocean_browser::{BrowserHandle, LaunchConfig};
use tokio::sync::Mutex;

use crate::capability::{CapabilityProvider, ProviderHealth, SessionContext, SharedTool};
use crate::types::{AgentTool, AgentToolResult, ToolSideEffect};

/// Shared dependency injected into every browser tool.
#[derive(Clone)]
pub struct BrowserToolCtx {
    pub handle: Arc<BrowserHandle>,
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
        Arc::new(inspect::BrowserNetworkTool { ctx }),
    ]
}

/// Capability provider that lazily launches Chrome the first time a turn asks
/// for tools, then serves the browser tool suite. The first `tools()` call
/// pays the launch cost (a few hundred ms); subsequent turns reuse the handle.
///
/// If Chrome fails to launch the provider serves **no** tools and logs a
/// warning — the agent simply won't have browser tools that session, rather
/// than the daemon failing to start or the turn erroring.
pub struct BrowserProvider {
    cfg: LaunchConfig,
    handle: Mutex<Option<Arc<BrowserHandle>>>,
}

impl BrowserProvider {
    /// Build a provider. `profile_dir` is Chrome's user-data dir (point at the
    /// real Chrome data dir to inherit the user's logins); `profile_directory`
    /// is the sub-profile (e.g. "Default"); `extension_dir` (if it exists)
    /// preloads the Ocean cockpit extension.
    pub fn new(
        profile_dir: PathBuf,
        profile_directory: Option<String>,
        extension_dir: Option<PathBuf>,
        chrome_executable: Option<PathBuf>,
    ) -> Self {
        Self {
            cfg: LaunchConfig {
                profile_dir,
                profile_directory,
                extension_dir,
                chrome_executable,
                headless: false,
                port: 0,
            },
            handle: Mutex::new(None),
        }
    }

    /// Get-or-launch the shared handle.
    async fn ensure(&self) -> Option<Arc<BrowserHandle>> {
        let mut guard = self.handle.lock().await;
        if let Some(h) = guard.as_ref() {
            return Some(h.clone());
        }
        match BrowserHandle::launch(self.cfg.clone()).await {
            Ok(h) => {
                let h = Arc::new(h);
                *guard = Some(h.clone());
                Some(h)
            }
            Err(e) => {
                tracing::warn!(error = %e, "browser launch failed; browser tools unavailable");
                None
            }
        }
    }
}

#[async_trait]
impl CapabilityProvider for BrowserProvider {
    fn id(&self) -> &str {
        "browser"
    }

    async fn tools(&self, _ctx: &SessionContext) -> Vec<SharedTool> {
        match self.ensure().await {
            Some(handle) => browser_tools(BrowserToolCtx { handle }),
            None => Vec::new(),
        }
    }

    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }
}
