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
//! exactly those terms — `name`, `description`, `input_schema` — so that when the
//! daemon adopts this crate (see below) it can wrap each `PluginTool` in a thin
//! `AgentTool` adapter whose `execute` forwards to [`Plugin::invoke_tool`]. This
//! crate stops short of that adapter on purpose: it has **no dependency on
//! `ocean-runtime`**, keeping the seam additive and the dependency graph flat.
//!
//! [`AgentTool`]: https://docs.rs/ocean-runtime
//!
//! ## How the runtime would adopt this (deferred — not wired here)
//!
//! This crate is library-only. Nothing in the daemon or runtime is touched. When
//! the runtime is ready to load plugins, the integration is small and additive:
//!
//! 1. **Discover** plugin packs (e.g. a `plugins/<name>/plugin.toml` directory),
//!    parse each with [`PluginManifest::parse`].
//! 2. **Launch** each as a [`SubprocessPlugin`] via [`SubprocessPlugin::launch`]
//!    (or hold them lazily and launch on first use).
//! 3. **Adapt** each [`PluginTool`] into an `ocean_runtime::AgentTool`: the
//!    adapter's `name()`/`description()`/`parameters()` come straight from the
//!    [`PluginTool`]; its `execute(id, args)` calls
//!    [`Plugin::invoke_tool`]`(name, args)` and maps the returned `Value` into an
//!    `AgentToolResult`. Permission gating stays in the daemon's existing
//!    `PermissionPolicy` — **plugins never bypass the permission gate.**
//! 4. **Register** the adapted tools alongside the built-in ones in
//!    `AgentConfig.tools`.
//!
//! That wiring lives in `ocean-agent`/`ocean-daemon` in a follow-up ticket; doing
//! it here would mean editing daemon code this PR is explicitly forbidden from
//! touching.
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

pub mod jsonrpc;
pub mod transport;

pub use manifest::{ManifestError, PluginManifest, ToolDecl};
pub use plugin::{Plugin, PluginError, PluginTool};
pub use subprocess::SubprocessPlugin;
