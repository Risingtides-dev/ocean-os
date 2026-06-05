use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ocean_protocol::Error),

    #[error("tool '{tool}' failed: {message}")]
    Tool { tool: String, message: String },

    #[error("agent cancelled")]
    Cancelled,

    #[error("agent reached max turns ({0})")]
    MaxTurns(u32),

    #[error("agent turn exceeded {secs}s timeout")]
    Timeout { secs: u32 },

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AgentError>;
