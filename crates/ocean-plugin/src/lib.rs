//! `ocean-plugin` — a subprocess-first plugin runtime for agent skill packs.
//!
//! A *plugin* is a self-contained skill pack that declares one or more tools and
//! knows how to run them. The runtime treats a plugin as a black box behind the
//! [`Plugin`] trait: it asks for the plugin's name/version, lists the tools it
//! offers ([`PluginTool`]), and invokes a tool by name with JSON arguments. How
//! the tool actually runs — out-of-process today, in a WASM sandbox tomorrow —
//! is the plugin implementation's concern, never the runtime's.
//!
//! ## What ships today
//!
//! - [`Plugin`] — the capability seam. One trait, two intended implementors:
//!   [`SubprocessPlugin`] (here, now) and a future `WasmPlugin` (deferred).
//! - [`PluginManifest`] — the parsed `plugin.toml` (name, version, entry
//!   executable, declared tools), with a parser ([`PluginManifest::parse`]).
//! - [`SubprocessPlugin`] — launches the manifest's `entry` executable and
//!   speaks **JSON-RPC 2.0 over stdio**, mirroring `ocean-mcp`'s proven stdio
//!   transport (newline-delimited JSON, request/response correlation by id,
//!   kill-on-drop shutdown). It does **not** depend on `ocean-mcp`; it reuses the
//!   approach, not the crate.
//!
//! ## Relationship to the runtime's `AgentTool`
//!
//! `ocean-runtime`'s [`AgentTool`] trait describes a tool as `name()`,
//! `description()`, `parameters() -> Value` (its JSON-Schema input), and
//! `execute(id, args) -> Result`. A [`PluginTool`] is deliberately expressible in
//! exactly those terms — `name`, `description`, `input_schema` — so the adapter
//! that exposes a plugin tool as a runtime tool reshapes nothing.
//!
//! [`AgentTool`]: ocean_runtime::types::AgentTool
//!
//! ## The runtime adapter (OCEAN-95 — shipped, behind the `runtime` feature)
//!
//! [`PluginProvider`] is the runtime integration OCEAN-91 deferred. It implements
//! `ocean-runtime`'s `CapabilityProvider` seam, so a plugin's tools compose into
//! the **same** `CapabilityRegistry` as the built-ins and the MCP tools — one
//! unified tool surface, no special-casing in the agent loop. This mirrors how
//! `ocean-mcp`'s `McpProvider` plugs an MCP server into that same seam.
//!
//! It is gated behind the on-by-default `runtime` feature: disabling default
//! features gives you the plain plugin transport with **no `ocean-runtime`
//! dependency**, keeping the dependency graph flat for transport-only consumers.
//!
//! How a host wires it up (additive — no daemon/runtime edit required):
//!
//! 1. **Discover** plugin packs (e.g. a `plugins/<name>/plugin.toml` directory),
//!    parse each with [`PluginManifest::parse`].
//! 2. **Launch** each as a [`SubprocessPlugin`] via [`SubprocessPlugin::launch`].
//! 3. **Adapt** each plugin with [`PluginProvider::connect`], which wraps every
//!    [`PluginTool`] as an `AgentTool` whose `execute` forwards to
//!    [`Plugin::invoke_tool`]. Tools are namespaced `plugin__<name>__<tool>`.
//! 4. **Register** the `PluginProvider`(s) in the runtime's `CapabilityRegistry`
//!    alongside `BuiltinProvider` and any `McpProvider`s.
//!
//! Permission gating stays in the daemon's existing `PermissionPolicy` — plugin
//! tools report `requires_permission() == true`, so **plugins never bypass the
//! permission gate.** The final registration step lives in `ocean-agent` /
//! `ocean-daemon`; this crate provides the provider it will register.
//!
//! ## WASM-ready seam (deferred follow-up)
//!
//! The [`Plugin`] trait is the stable contract. A future `WasmPlugin` —
//! instantiating a `wasmtime` module and calling guest exports for `list_tools` /
//! `invoke_tool` — implements the **same trait** with **no change to it**: the
//! trait speaks only in `name`/`version`/[`PluginTool`]/`serde_json::Value`,
//! none of which assume a subprocess. `wasmtime` is intentionally **not** a
//! dependency of this crate yet; adding it (behind a `wasm` feature, with a
//! `WasmPlugin` type) is the documented follow-up. Keeping the trait subprocess-
//! agnostic now is what makes that follow-up additive rather than a rewrite.

mod manifest;
mod plugin;
mod subprocess;

#[cfg(feature = "runtime")]
mod provider;

pub mod jsonrpc;
pub mod transport;

#[cfg(feature = "runtime")]
pub use provider::PluginProvider;

pub use manifest::{ManifestError, PluginManifest, ToolDecl};
pub use plugin::{Plugin, PluginError, PluginTool};
pub use subprocess::SubprocessPlugin;
