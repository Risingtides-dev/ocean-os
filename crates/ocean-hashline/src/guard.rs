//! No-op loop guard: detect a model stuck re-applying the same byte-identical
//! edit that changes nothing, and escalate to an error so it stops looping.

use crate::hash::compute_file_hash;
use std::collections::HashMap;
use std::fmt;

/// Default number of consecutive identical no-op edits tolerated before the
/// guard trips.
pub const DEFAULT_MAX_NOOP_REPEATS: usize = 2;

/// Raised when the same `(path, patch)` no-op has been observed too many times.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoopLoopError {
    pub path: String,
    pub repeats: usize,
}

impl fmt::Display for NoopLoopError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "This exact edit to {} has now been applied {} times with no change to the file. Stop re-issuing it; re-read the file or move on.",
            self.path, self.repeats
        )
    }
}

impl std::error::Error for NoopLoopError {}

/// Tracks consecutive byte-identical no-op edits per path.
pub struct NoopLoopGuard {
    max_repeats: usize,
    /// path -> (patch fingerprint, consecutive no-op count).
    state: HashMap<String, (String, usize)>,
}

impl Default for NoopLoopGuard {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_NOOP_REPEATS)
    }
}

impl NoopLoopGuard {
    /// Create a guard that trips after `max_repeats` consecutive identical
    /// no-op edits.
    pub fn new(max_repeats: usize) -> Self {
        NoopLoopGuard {
            max_repeats: max_repeats.max(1),
            state: HashMap::new(),
        }
    }

    /// Record that applying `patch_text` to `path` produced NO change to the
    /// file. Returns `Err` once the same no-op edit repeats `max_repeats` times
    /// in a row.
    ///
    /// A different patch for the same path resets the counter.
    pub fn observe_noop(&mut self, path: &str, patch_text: &str) -> Result<(), NoopLoopError> {
        let fingerprint = compute_file_hash(patch_text);
        let entry = self
            .state
            .entry(path.to_string())
            .or_insert_with(|| (fingerprint.clone(), 0));
        if entry.0 == fingerprint {
            entry.1 += 1;
        } else {
            entry.0 = fingerprint;
            entry.1 = 1;
        }
        if entry.1 >= self.max_repeats {
            return Err(NoopLoopError {
                path: path.to_string(),
                repeats: entry.1,
            });
        }
        Ok(())
    }

    /// A successful (changing) edit clears the guard state for `path`.
    pub fn reset(&mut self, path: &str) {
        self.state.remove(path);
    }
}
