//! `ocean-longhouse` — the real quorum engine + convening flow behind the
//! longhouse deck.
//!
//! This crate turns the *scripted* longhouse demo (a daemon endpoint that emits
//! fake `LonghouseEvent`s on a timer) into a **real** council:
//!
//! * a pure, daemon-computed [`QuorumEngine`](quorum::QuorumEngine) that counts
//!   credential-weighted, time-decaying, cross-inhibiting marks and decides when
//!   a proposal crosses quorum — **never an LLM**; and
//! * a [`convene`](convene::convene) flow that staffs a council with real LLM
//!   workers on **cheap** models (deepseek + kimi), runs a two-round propose →
//!   endorse/inhibit protocol, feeds every mark to the engine, and emits the
//!   **existing** `ocean_agent_sdk::LonghouseEvent`s so the deck renders a live
//!   council with zero deck changes.
//!
//! ## Architecture & the load-bearing separation
//!
//! ```text
//! ocean-agent-sdk   LonghouseEvent / Mark / Federation / AgentRole  (the wire vocab)
//!       ▲
//!       │  (this crate depends UP, never down)
//! ocean-longhouse
//!   ├── quorum.rs    QuorumEngine — PURE Rust, no LLM, fully unit-tested
//!   ├── agent.rs     ModelHandle — one cheap-model turn via stream_simple
//!   └── convene.rs   convene() — orchestrates rounds, feeds the engine, emits events
//!       ▲
//!       │
//! ocean-daemon      POST /v1/longhouse/convene → streams events on /v1/agent/events
//! ```
//!
//! The engine **counts**; LLMs only **produce** marks. That is the whole point:
//! convergence is deterministic arithmetic the daemon owns and can fully test
//! without any model in the loop (see the unit tests in [`quorum`]).

pub mod agent;
pub mod config;
pub mod convene;
pub mod longhouse_provider;
pub mod quorum;
pub mod registry;
pub mod replay;

pub use agent::ModelHandle;
pub use longhouse_provider::{LonghouseProvider, LonghouseRegistryHandle};
pub use config::{LonghouseConfig, LonghouseMode};
pub use convene::{
    claim_outcome, ClaimError, Clock, ConveneOutcome, ConveneRequest, SystemClock, convene,
};
pub use quorum::{QuorumConfig, QuorumEngine, QuorumOutcome, QuorumRule};
pub use registry::{LonghouseRegistry, TopicSnapshot, TopicState};
pub use replay::{replay, sweep, RecordedMark, RecordedMarkKind, Recording, ReplayResult};

// Re-export the SDK event vocabulary so daemon callers can `use
// ocean_longhouse::{LonghouseEvent, Federation, ...}` from one place.
pub use ocean_agent_sdk::{
    AbortReason, AgentRole, ConveneTrigger, Federation, LonghouseEvent, LonghouseMember, Mark,
    MarkKind, ProposalTally,
};
