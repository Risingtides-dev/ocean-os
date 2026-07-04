//! `ocean-hashline` — a pure-Rust reimplementation of oh-my-pi's hashline edit
//! algorithm (MIT).
//!
//! Hashline binds every edit to a 4-hex content hash of the whole file. A read
//! mints the tag; an edit anchored at any line validates only while the live
//! file still hashes to that tag. When the file has drifted, a snapshot store
//! plus three zero-fuzz recovery strategies can still land the edit — or a
//! [`MismatchError`] tells the caller exactly why it could not.
//!
//! # Public API
//!
//! - [`compute_file_hash`] — the 4-hex section tag for a file's text.
//! - [`normalize_to_lf`] / [`restore_line_endings`] — BOM/CRLF canonicalization.
//! - [`Patch::parse`] — parse hashline patch text into structured ops.
//! - [`apply_patch`] / [`apply_section`] — strict (zero-drift) apply with stale
//!   detection.
//! - [`SnapshotStore`] — record what the model saw (`record`/`head`/`by_hash`).
//! - [`Recovery::try_recover`] — store-aware recovery of a stale edit.
//! - [`MismatchError`] — the hard stale-tag rejection.
//! - [`NoopLoopGuard`] — escalate a model stuck re-applying a no-op edit.
//!
//! # Scope
//!
//! The tree-sitter block verbs (`SWAP.BLK`/`DEL.BLK`/`INS.BLK.POST`) and the
//! file-level ops (`REM`/`MV`) are parsed and **rejected** — they are a later
//! wave. The model-mistake leniency repair passes from OMP (`apply.ts`
//! `repairReplacementBoundaries` / `repairAfterInsertLandings`) are also out of
//! scope for v1; the core apply engine is otherwise a faithful port.

pub mod format;
pub mod guard;
pub mod hash;
pub mod mismatch;
pub mod normalize;
pub mod patcher;
pub mod recovery;
pub mod snapshot;
pub mod tokenizer;

pub use format::{format_header, InsertPos, Op, Patch, Section};
pub use guard::{NoopLoopError, NoopLoopGuard};
pub use hash::{compute_file_hash, FILE_HASH_LENGTH};
pub use mismatch::MismatchError;
pub use normalize::{
    detect_line_ending, has_bom, normalize_to_lf, restore_line_endings, LineEnding,
};
pub use patcher::{apply_ops, apply_patch, apply_section, ApplyError};
pub use recovery::{
    Recovery, RECOVERY_EXTERNAL_WARNING, RECOVERY_LINE_REMAP_WARNING,
    RECOVERY_SESSION_CHAIN_WARNING, RECOVERY_SESSION_REPLAY_WARNING,
};
pub use snapshot::{Snapshot, SnapshotStore};
pub use tokenizer::ParseError;
