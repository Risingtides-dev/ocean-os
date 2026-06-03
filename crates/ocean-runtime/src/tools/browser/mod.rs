//! Agent-facing browser tools. Thin wrappers over `ocean_browser::BrowserHandle`.
//! Every tool is permission-gated (except read-only perception/inspect) and
//! emits a `BrowserActivity { active: true }` side-effect so the daemon can
//! drive the side-panel handoff.

pub mod inspect;
pub mod input;
pub mod nav;
pub mod perceive;

use std::sync::Arc;

use ocean_browser::BrowserHandle;

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
