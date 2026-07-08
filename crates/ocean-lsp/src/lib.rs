//! `ocean-lsp` — code intelligence for the agent loop (W5 of the OMP port).
//!
//! One `lsp` tool, action-dispatched, backed by the workspace's own language
//! servers (rust-analyzer, typescript-language-server, pyright, gopls),
//! auto-detected by root marker + binary-on-`$PATH`. Clients are shared
//! process-wide per `(server, root)`; diagnostics dedupe through a
//! session-scoped [`DiagnosticsLedger`] so the model only reads NEW problems.
//!
//! Registered through the same [`CapabilityProvider`] seam as MCP —
//! `ocean-lsp` depends UP into `ocean-runtime`, never the reverse.
//!
//! [`CapabilityProvider`]: ocean_runtime::capability::CapabilityProvider

pub mod client;
pub mod framing;
pub mod ledger;
pub mod provider;
pub mod servers;
pub mod tool;

pub use client::LspClient;
pub use ledger::DiagnosticsLedger;
pub use provider::LspProvider;
pub use servers::{detect, ServerDef, SERVERS};
pub use tool::LspTool;
