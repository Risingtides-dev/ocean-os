use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use ocean_longhouse::{RecallOutcome, RecallVote};
use uuid::Uuid;

/// Open recall tallies keyed by the firekeeper `title_id` (OCEAN-302). Each value
/// is a pure [`ocean_longhouse::RecallVote`] counting distinct caller-supplied
/// voter UUIDs; this registry does not authenticate those identities. Held behind
/// a std `Mutex` like the other longhouse stores: every access is a quick
/// synchronous read/insert and the guard is dropped before any `await`.
pub(super) type RecallRegistryHandle = Arc<Mutex<HashMap<Uuid, RecallVote>>>;

pub(super) fn new_recall_registry() -> RecallRegistryHandle {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Run a closure with the locked recall registry, recovering a poisoned lock the
/// same way the other longhouse handlers do. Synchronous: the guard drops before
/// this returns, so no `await` is held across it.
fn with_recalls<T>(
    recalls: &RecallRegistryHandle,
    f: impl FnOnce(&mut HashMap<Uuid, RecallVote>) -> T,
) -> T {
    let mut guard = match recalls.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    f(&mut guard)
}

pub(super) fn cast_recall_vote(
    recalls: &RecallRegistryHandle,
    title_id: Uuid,
    voter_id: Uuid,
    threshold: usize,
) -> RecallOutcome {
    with_recalls(recalls, |recalls| {
        let recall = recalls
            .entry(title_id)
            .or_insert_with(|| RecallVote::new(title_id, threshold));
        recall.cast(voter_id)
    })
}

pub(super) fn remove_recall_tally(recalls: &RecallRegistryHandle, title_id: Uuid) {
    with_recalls(recalls, |recalls| {
        recalls.remove(&title_id);
    });
}
