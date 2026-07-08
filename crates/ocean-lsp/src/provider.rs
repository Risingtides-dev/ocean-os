//! `LspProvider` — offers the `lsp` tool through the runtime's capability seam.
//!
//! The tool appears only when the turn's workspace actually has a usable
//! language server (root marker present + binary on `$PATH`) — oh-my-pi's
//! `createIf` pattern. A workspace with no servers gets no tool, no schema
//! bytes, no dead surface area.

use std::sync::Arc;

use async_trait::async_trait;
use ocean_runtime::capability::{CapabilityProvider, ProviderHealth, SessionContext, SharedTool};

use crate::servers::detect;
use crate::tool::LspTool;

pub struct LspProvider;

#[async_trait]
impl CapabilityProvider for LspProvider {
    fn id(&self) -> &str {
        "lsp"
    }

    async fn tools(&self, ctx: &SessionContext) -> Vec<SharedTool> {
        // Cheap detection: a handful of stats + a PATH scan. No server is
        // started here — clients spawn lazily on first use.
        if detect(&ctx.cwd).is_empty() {
            return Vec::new();
        }
        vec![Arc::new(LspTool::new(ctx.cwd.clone(), ctx.session_id.clone())) as SharedTool]
    }

    async fn health(&self) -> ProviderHealth {
        ProviderHealth::Ready
    }
}
