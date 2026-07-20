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
        // TASK-26: profile gate. Voice turns never get code intelligence —
        // definitions, references, and diagnostics are dense structured text
        // that cannot be spoken usefully, so offering the tool only invites a
        // model to produce an unreadable answer. Checked before detection so a
        // gated turn does no filesystem work at all.
        if !ctx.code_intelligence {
            return Vec::new();
        }
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

#[cfg(test)]
mod task26_tests {
    use super::*;

    fn ctx(cwd: &std::path::Path, code_intelligence: bool) -> SessionContext {
        SessionContext {
            cwd: cwd.to_path_buf(),
            session_id: Some("s".into()),
            hashline: false,
            artifacts: false,
            code_intelligence,
        }
    }

    /// TASK-26: a voice-profile turn is offered no `lsp` tool even in a
    /// workspace where a language server is ready — the gate is the profile,
    /// not the workspace.
    #[tokio::test]
    async fn voice_profile_is_offered_no_lsp_tool() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root");
        let gated = LspProvider.tools(&ctx(repo, false)).await;
        assert!(
            gated.is_empty(),
            "voice must be offered no code-intelligence tool"
        );
    }

    /// The gate must not become a blanket disable: an ungated turn in a
    /// detected workspace still gets the tool, so this stays a profile gate
    /// rather than a silent feature removal.
    #[tokio::test]
    async fn ungated_profile_still_gets_lsp_when_workspace_is_detected() {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root");
        if detect(repo).is_empty() {
            // No server installed on this machine: the detection path, not the
            // gate, is what withholds the tool. Nothing to assert.
            return;
        }
        let offered = LspProvider.tools(&ctx(repo, true)).await;
        assert_eq!(
            offered.len(),
            1,
            "an ungated turn in a detected workspace still gets `lsp`"
        );
    }
}
