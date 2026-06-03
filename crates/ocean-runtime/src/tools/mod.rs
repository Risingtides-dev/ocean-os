//! Builtin tools: read, write, edit, bash, ls, grep, glob, component render.
//!
//! Mirrors the core toolset shipped in `packages/coding-agent/src/core/...`.

pub mod bash;
pub mod browser;
pub mod component;
pub mod edit;
pub mod glob_tool;
pub mod grep;
pub mod ls;
pub mod read;
pub mod todo;
pub mod web_fetch;
pub mod write;

use std::sync::Arc;

use crate::types::AgentTool;

/// Returns the default suite of builtin tools used by the coding agent.
///
/// Includes `component_wait`, which uses a global [`ComponentWaitRegistry`]
/// shared with the daemon's `/v1/component/event` route.
pub fn default_tools() -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(read::ReadTool),
        Arc::new(write::WriteTool),
        Arc::new(edit::EditTool),
        Arc::new(bash::BashTool),
        Arc::new(ls::LsTool),
        Arc::new(grep::GrepTool),
        Arc::new(glob_tool::GlobTool),
        Arc::new(web_fetch::WebFetchTool),
        Arc::new(todo::TodoTool::new()),
        Arc::new(component::ComponentRenderTool),
        Arc::new(component::ComponentUnmountTool),
        Arc::new(component::ComponentWaitTool),
    ]
}
