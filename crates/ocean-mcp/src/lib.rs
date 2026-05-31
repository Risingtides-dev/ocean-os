//! `ocean-mcp` — Ocean as an MCP **client**.
//!
//! Connects to external [Model Context Protocol](https://modelcontextprotocol.io)
//! servers and exposes their tools to the Ocean agent through the runtime's
//! [`CapabilityProvider`](ocean_runtime::capability::CapabilityProvider) seam.
//! This is how the keys in `tools.env` (Brave, Slack, Linear, …) become agent
//! tools — as MCP servers, not hardcoded native Rust tools.
//!
//! Dependency direction is one-way: this crate depends on `ocean-runtime` for
//! the trait it implements. `ocean-runtime` must never depend back on it.
//!
//! Layout:
//! - [`jsonrpc`] — JSON-RPC 2.0 message types
//! - [`transport`] — the [`Transport`](transport::Transport) trait + stdio impl
//! - [`client`] — [`McpClient`](client::McpClient): lifecycle, discovery, calls
//! - [`config`] — [`McpServerConfig`](config::McpServerConfig) for `[[mcp.server]]`
//! - [`provider`] — [`McpProvider`](provider::McpProvider): the registry adapter

pub mod client;
pub mod config;
pub mod jsonrpc;
pub mod provider;
pub mod transport;

pub use client::{McpCallResult, McpClient, McpToolDef, PROTOCOL_VERSION};
pub use config::{McpServerConfig, McpTransportKind};
pub use provider::McpProvider;
pub use transport::{StdioTransport, Transport};
