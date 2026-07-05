//! `ocean-runtime` — Agent runtime with tool calling.
//!
//! Provides:
//! - [`AgentTool`] / [`AgentToolResult`] for defining tools
//! - [`AgentConfig`] for configuring a run, plus a [`PermissionPolicy`] hook
//! - [`run_agent`] / [`run_agent_with_history`] — the agent loop
//! - Builtin tools under [`tools`]

pub mod agent_loop;
pub mod artifacts;
pub mod capability;
pub mod error;
pub mod fake_tool_provider;
pub mod tools;
pub mod types;

pub use agent_loop::{run_agent, run_agent_with_history, AgentRun};
pub use artifacts::{Artifact, ArtifactStore, SharedArtifacts};
pub use capability::{
    BuiltinProvider, CapabilityProvider, CapabilityRegistry, ProviderHealth, SessionContext,
    SharedTool,
};
pub use error::{AgentError, Result};
pub use fake_tool_provider::{
    FakeToolProvider, FAKE_SURFACE_CALL_ID, FAKE_SURFACE_CANVAS_ID, FAKE_SURFACE_MODEL,
    FAKE_TOOL_CALL_ID, FAKE_TOOL_CONTENT, FAKE_TOOL_MODEL, FAKE_TOOL_TARGET_PATH,
};
pub use types::{
    tool_def, AgentConfig, AgentEvent, AgentTool, AgentToolResult, AllowAllPolicy,
    PermissionDecision, PermissionPolicy,
};
