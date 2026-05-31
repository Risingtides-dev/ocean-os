//! `ocean-runtime` — Agent runtime with tool calling.
//!
//! Provides:
//! - [`AgentTool`] / [`AgentToolResult`] for defining tools
//! - [`AgentConfig`] for configuring a run, plus a [`PermissionPolicy`] hook
//! - [`run_agent`] / [`run_agent_with_history`] — the agent loop
//! - Builtin tools under [`tools`]

pub mod agent_loop;
pub mod capability;
pub mod error;
pub mod tools;
pub mod types;

pub use agent_loop::{run_agent, run_agent_with_history, AgentRun};
pub use capability::{
    BuiltinProvider, CapabilityProvider, CapabilityRegistry, ProviderHealth, SessionContext,
    SharedTool,
};
pub use error::{AgentError, Result};
pub use types::{
    tool_def, AgentConfig, AgentEvent, AgentTool, AgentToolResult, AllowAllPolicy,
    PermissionDecision, PermissionPolicy,
};
