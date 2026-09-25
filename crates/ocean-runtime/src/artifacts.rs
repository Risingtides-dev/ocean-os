//! Session-scoped artifact store for output-meta + artifact spill (W3).
//!
//! When a tool returns an output larger than the spill threshold, the agent
//! only keeps a truncated HEAD in context (see `capability::SpillingTool`) and
//! the FULL output is *spilled* here as an artifact. The model reads it back on
//! demand via `read artifact://<id>` (resolved in `tools::read`). Nothing is
//! ever lost; context stays small.
//!
//! The store lives on `BuiltinProvider`, keyed by session id, exactly like the
//! hashline `SnapshotStore` — a `read` in one turn and a spill in another share
//! one store for the life of the runtime.
//!
//! Bounds: the store is capped by total bytes (`MAX_TOTAL_BYTES`, ~8 MiB) and by
//! entry count (`MAX_ENTRIES`, 64). When either is exceeded the oldest artifact
//! is evicted first. A single artifact larger than the byte cap is kept anyway
//! (the model must be able to read back the thing the notice pointed it at) —
//! the byte cap only evicts *older* entries, never the sole/just-added one.
//!
//! Turn-pinned entries (minimizer M2): an active agent run may pin the exact raw
//! text behind a provider-only minimized projection through a [`PinBudget`].
//! Pinned entries are skipped by ordinary eviction until their
//! [`ArtifactLease`] drops, bounded per run by [`MAX_PINNED_ENTRIES_PER_RUN`] and
//! [`MAX_PINNED_BYTES_PER_RUN`]; dropping a lease immediately reapplies normal
//! eviction. Pins never outlive the in-memory store and are never persisted.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

/// Total bytes retained across all artifacts in one session before the oldest
/// are evicted. ~8 MiB — generous for a handful of big tool dumps, bounded so a
/// runaway session can't grow the store without limit.
pub const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

/// Max number of artifacts retained in one session before the oldest is
/// evicted, independent of byte size.
pub const MAX_ENTRIES: usize = 64;

/// Max artifacts one active run may hold pinned at once (minimizer M2).
pub const MAX_PINNED_ENTRIES_PER_RUN: usize = 32;

/// Max original bytes one active run may hold pinned at once (minimizer M2).
pub const MAX_PINNED_BYTES_PER_RUN: usize = 1024 * 1024;

/// One spilled tool output.
#[derive(Debug, Clone)]
pub struct Artifact {
    /// Short opaque id (e.g. `"a1"`), the token in `read artifact://<id>`.
    pub id: String,
    /// Name of the tool whose output this is (`read`, `bash`, …). Diagnostic.
    pub tool: String,
    /// Monotonic creation counter (NOT wall-clock) — the eviction/order key.
    pub created_seq: u64,
    /// The full, untruncated output text.
    pub text: String,
}

/// Session-scoped, bounded, in-memory store of spilled outputs.
#[derive(Debug)]
pub struct ArtifactStore {
    /// id → artifact.
    entries: HashMap<String, Artifact>,
    /// Insertion order of live ids, oldest at the front — the eviction queue.
    order: VecDeque<String>,
    /// Monotonic id/order counter. Never reused, so ids stay stable even as
    /// older entries are evicted.
    seq: u64,
    /// Running sum of `text.len()` across live entries.
    total_bytes: usize,
    max_total_bytes: usize,
    max_entries: usize,
    /// Ids held by a live [`ArtifactLease`]; ordinary eviction skips them.
    pinned: HashSet<String>,
}

impl Default for ArtifactStore {
    fn default() -> Self {
        Self::new(MAX_TOTAL_BYTES, MAX_ENTRIES)
    }
}

impl ArtifactStore {
    /// Build a store with explicit bounds. `new(MAX_TOTAL_BYTES, MAX_ENTRIES)`
    /// is the production shape; tests use tight bounds to exercise eviction.
    pub fn new(max_total_bytes: usize, max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            seq: 0,
            total_bytes: 0,
            max_total_bytes,
            max_entries,
            pinned: HashSet::new(),
        }
    }

    /// Spill `text` produced by `tool`, returning the new artifact id. Enforces
    /// the count + byte bounds afterwards, evicting oldest-first.
    pub fn put(&mut self, tool: impl Into<String>, text: impl Into<String>) -> String {
        self.insert(tool.into(), text.into(), false)
    }

    fn insert(&mut self, tool: String, text: String, pin: bool) -> String {
        self.seq += 1;
        let id = format!("a{}", self.seq);
        let bytes = text.len();
        let artifact = Artifact {
            id: id.clone(),
            tool,
            created_seq: self.seq,
            text,
        };
        self.total_bytes += bytes;
        self.entries.insert(id.clone(), artifact);
        self.order.push_back(id.clone());
        if pin {
            self.pinned.insert(id.clone());
        }
        self.evict();
        id
    }

    /// Whether `id` is currently held by a live [`ArtifactLease`].
    pub fn is_pinned(&self, id: &str) -> bool {
        self.pinned.contains(id)
    }

    /// Number of artifacts currently held by live leases.
    pub fn pinned_len(&self) -> usize {
        self.pinned.len()
    }

    fn unpin(&mut self, id: &str) {
        if self.pinned.remove(id) {
            self.evict();
        }
    }

    /// Fetch an artifact by id.
    pub fn get(&self, id: &str) -> Option<&Artifact> {
        self.entries.get(id)
    }

    /// All live artifacts, oldest first.
    pub fn list(&self) -> Vec<&Artifact> {
        self.order
            .iter()
            .filter_map(|id| self.entries.get(id))
            .collect()
    }

    /// Number of live artifacts.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store holds no artifacts.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Total retained bytes across live artifacts.
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Enforce bounds: drop oldest entries until under the count cap, then under
    /// the byte cap. The byte cap never evicts the last remaining entry — a lone
    /// artifact bigger than the cap is kept so the model can still read it back.
    /// Pinned entries are skipped; they may hold the store above its ordinary
    /// targets only within the explicit per-run pin bounds.
    fn evict(&mut self) {
        while self.entries.len() > self.max_entries {
            if !self.pop_oldest() {
                break;
            }
        }
        while self.total_bytes > self.max_total_bytes && self.entries.len() > 1 {
            if !self.pop_oldest() {
                break;
            }
        }
    }

    /// Remove the oldest live, unpinned artifact other than the newest one.
    /// Returns false if nothing is evictable (only the newest entry remains or
    /// every older entry is pinned). Never evicting the newest entry keeps the
    /// just-added artifact reachable even when pins hold older bytes.
    fn pop_oldest(&mut self) -> bool {
        let mut index = 0;
        while index + 1 < self.order.len() {
            if self.pinned.contains(&self.order[index]) {
                index += 1;
                continue;
            }
            let id = self.order.remove(index).expect("index is in bounds");
            if let Some(a) = self.entries.remove(&id) {
                self.total_bytes = self.total_bytes.saturating_sub(a.text.len());
                return true;
            }
            // Stale id (shouldn't happen — order and entries stay in lockstep);
            // skip and keep looking.
        }
        false
    }
}

/// Shared, session-scoped handle to an [`ArtifactStore`]. The spill decorator
/// writes into it; the `read` tool reads `artifact://` back out of it.
pub type SharedArtifacts = Arc<Mutex<ArtifactStore>>;

/// Create a fresh shared store with production bounds.
pub fn new_shared() -> SharedArtifacts {
    Arc::new(Mutex::new(ArtifactStore::default()))
}

#[derive(Debug, Default)]
struct PinUsage {
    entries: usize,
    bytes: usize,
}

/// Per-run accounting for turn-pinned artifacts (minimizer M2).
///
/// Counts only pins whose [`ArtifactLease`] is still alive, so the bounds are
/// "at most N entries / B bytes pinned by this run at any moment". A lease drop
/// returns its share. Cloning shares the same accounting.
#[derive(Debug, Clone)]
pub struct PinBudget {
    usage: Arc<Mutex<PinUsage>>,
    max_entries: usize,
    max_bytes: usize,
}

impl Default for PinBudget {
    fn default() -> Self {
        Self::new(MAX_PINNED_ENTRIES_PER_RUN, MAX_PINNED_BYTES_PER_RUN)
    }
}

impl PinBudget {
    /// Budget with explicit bounds; production uses [`PinBudget::default`].
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            usage: Arc::new(Mutex::new(PinUsage::default())),
            max_entries,
            max_bytes,
        }
    }

    /// Atomically store `text` in `store` and pin it for the lease's lifetime.
    ///
    /// Fails open (`None`, and no artifact is created) when the budget cannot
    /// accept the entry or either lock is poisoned.
    pub fn pin(&self, store: &SharedArtifacts, tool: &str, text: String) -> Option<ArtifactLease> {
        let bytes = text.len();
        {
            let mut usage = self.usage.lock().ok()?;
            if usage.entries + 1 > self.max_entries || usage.bytes + bytes > self.max_bytes {
                return None;
            }
            usage.entries += 1;
            usage.bytes += bytes;
        }
        let id = match store.lock() {
            Ok(mut store) => store.insert(tool.to_string(), text, true),
            Err(_) => {
                self.release(bytes);
                return None;
            }
        };
        Some(ArtifactLease {
            store: store.clone(),
            id,
            bytes,
            budget: self.clone(),
        })
    }

    /// Live pinned entries and bytes charged to this budget.
    pub fn usage(&self) -> (usize, usize) {
        self.usage
            .lock()
            .map(|usage| (usage.entries, usage.bytes))
            .unwrap_or((0, 0))
    }

    fn release(&self, bytes: usize) {
        let mut usage = match self.usage.lock() {
            Ok(usage) => usage,
            Err(poisoned) => poisoned.into_inner(),
        };
        usage.entries = usage.entries.saturating_sub(1);
        usage.bytes = usage.bytes.saturating_sub(bytes);
    }
}

/// Keeps one turn-pinned artifact reachable. Dropping it unpins the entry,
/// reapplies ordinary eviction, and returns the budget share.
#[derive(Debug)]
pub struct ArtifactLease {
    store: SharedArtifacts,
    id: String,
    bytes: usize,
    budget: PinBudget,
}

impl ArtifactLease {
    /// Opaque artifact id, the token in `read artifact://<id>`.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for ArtifactLease {
    fn drop(&mut self) {
        let mut store = match self.store.lock() {
            Ok(store) => store,
            Err(poisoned) => poisoned.into_inner(),
        };
        store.unpin(&self.id);
        drop(store);
        self.budget.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_roundtrip() {
        let mut s = ArtifactStore::default();
        let id = s.put("bash", "hello world");
        assert_eq!(id, "a1");
        let a = s.get(&id).expect("present");
        assert_eq!(a.tool, "bash");
        assert_eq!(a.text, "hello world");
        assert_eq!(a.created_seq, 1);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn ids_are_monotonic_and_not_reused() {
        let mut s = ArtifactStore::new(usize::MAX, 2);
        let a = s.put("t", "1");
        let b = s.put("t", "2");
        let c = s.put("t", "3"); // evicts a1 (count cap = 2)
        assert_eq!((a, b.as_str(), c.as_str()), ("a1".into(), "a2", "a3"));
        assert!(s.get("a1").is_none(), "oldest evicted by count cap");
        assert!(s.get("a2").is_some());
        assert!(s.get("a3").is_some());
    }

    #[test]
    fn evicts_oldest_on_count_cap() {
        let mut s = ArtifactStore::new(usize::MAX, 3);
        for i in 0..5 {
            s.put("t", format!("v{i}"));
        }
        assert_eq!(s.len(), 3, "count capped at 3");
        let ids: Vec<_> = s.list().iter().map(|a| a.id.clone()).collect();
        assert_eq!(
            ids,
            vec!["a3", "a4", "a5"],
            "oldest two evicted, order preserved"
        );
    }

    #[test]
    fn evicts_oldest_on_byte_cap() {
        // Cap at 10 bytes; each entry is 4 bytes.
        let mut s = ArtifactStore::new(10, usize::MAX);
        s.put("t", "aaaa"); // a1: 4
        s.put("t", "bbbb"); // a2: 8
        s.put("t", "cccc"); // a3: 12 > 10 → evict a1 → 8
        assert!(s.get("a1").is_none(), "oldest evicted by byte cap");
        assert!(s.get("a2").is_some());
        assert!(s.get("a3").is_some());
        assert_eq!(s.total_bytes(), 8);
    }

    #[test]
    fn keeps_lone_oversize_artifact() {
        // A single artifact bigger than the whole byte cap is retained — the
        // model must be able to read back what the notice pointed at.
        let mut s = ArtifactStore::new(4, usize::MAX);
        let id = s.put("t", "this is way bigger than four bytes");
        assert_eq!(s.len(), 1, "lone oversize entry kept");
        assert!(s.get(&id).is_some());
    }

    #[test]
    fn pinned_entries_survive_count_and_byte_pressure_until_the_lease_drops() {
        let store: SharedArtifacts = Arc::new(Mutex::new(ArtifactStore::new(8, 2)));
        let budget = PinBudget::default();
        let lease = budget.pin(&store, "bash", "pinned".into()).expect("pin");
        assert_eq!(lease.id(), "a1");
        for text in ["aaaa", "bbbb", "cccc"] {
            store.lock().unwrap().put("t", text);
        }
        {
            let s = store.lock().unwrap();
            assert!(s.get("a1").is_some(), "pinned entry skipped by eviction");
            assert!(s.is_pinned("a1"));
            assert_eq!(budget.usage(), (1, 6));
        }
        drop(lease);
        let s = store.lock().unwrap();
        assert!(
            s.get("a1").is_none(),
            "unpin reapplies eviction immediately"
        );
        assert_eq!(s.pinned_len(), 0);
        assert_eq!(budget.usage(), (0, 0));
    }

    #[test]
    fn pin_budget_fails_open_without_creating_an_artifact() {
        let store = new_shared();
        let budget = PinBudget::new(1, 10);
        let _held = budget.pin(&store, "bash", "12345".into()).expect("fits");
        assert!(
            budget.pin(&store, "bash", "x".into()).is_none(),
            "entry cap"
        );
        let bytes = PinBudget::new(4, 10);
        assert!(
            bytes.pin(&store, "bash", "x".repeat(11)).is_none(),
            "byte cap"
        );
        assert_eq!(store.lock().unwrap().len(), 1, "no artifact on refusal");
    }

    #[test]
    fn production_pin_bounds_are_fixed() {
        assert_eq!(MAX_PINNED_ENTRIES_PER_RUN, 32);
        assert_eq!(MAX_PINNED_BYTES_PER_RUN, 1024 * 1024);
    }

    #[test]
    fn poisoned_store_fails_open_and_returns_budget() {
        let store = new_shared();
        let poisoner = store.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock().unwrap();
            panic!("poison the store");
        })
        .join();
        let budget = PinBudget::default();
        assert!(budget.pin(&store, "bash", "x".into()).is_none());
        assert_eq!(budget.usage(), (0, 0));
    }

    #[test]
    fn list_is_oldest_first() {
        let mut s = ArtifactStore::default();
        s.put("t", "1");
        s.put("t", "2");
        s.put("t", "3");
        let ids: Vec<_> = s.list().iter().map(|a| a.id.clone()).collect();
        assert_eq!(ids, vec!["a1", "a2", "a3"]);
    }
}
