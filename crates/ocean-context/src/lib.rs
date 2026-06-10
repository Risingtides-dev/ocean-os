//! ocean-context — the handoff as a context primitive.
//!
//! A handoff is a set of claims, each with provenance, that the receiving
//! session distrusts by default and reverifies against ground truth.
//! Spec: docs/specs/ocean-context-handoff-engine.md

pub mod claim;
pub mod extract;
pub mod seams;
pub mod store;

pub use claim::*;
pub use seams::{
    Borrowed, Borrower, ConfidenceRecencyTrust, FileExistsResolver, NoBorrow, Resolution,
    Resolver, Retriever, SubstringRetriever, TrustContext, TrustModel, WORKTREE,
};
