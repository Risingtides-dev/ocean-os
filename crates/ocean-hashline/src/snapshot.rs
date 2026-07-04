//! Per-session snapshot store: binds hashline section tags to the exact file
//! content that minted them, so a stale edit can be validated / recovered.
//!
//! Faithful to oh-my-pi `packages/hashline/src/snapshots.ts` (the in-memory
//! LRU store). Two intentional deviations, both documented on the API:
//!  * `recorded_at` is a monotonic counter, not wall-clock — the workspace
//!    forbids `Instant::now`/`Date.now` in some contexts and recency ordering
//!    only needs monotonicity.
//!  * The 64 MiB global byte ceiling is omitted; the path-count (30) and
//!    versions-per-path (4) bounds are kept.

use crate::hash::compute_file_hash;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Maximum number of distinct paths tracked at once (LRU eviction).
pub const DEFAULT_MAX_PATHS: usize = 30;
/// Maximum full-file versions retained per path (oldest dropped first).
pub const DEFAULT_MAX_VERSIONS_PER_PATH: usize = 4;

/// One full-file version observed at a point in time.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// Canonicalized path this version belongs to.
    pub path: PathBuf,
    /// Full normalized (LF, no BOM) file text as observed.
    pub text: String,
    /// Content-derived tag for [`Snapshot::text`].
    pub hash: String,
    /// Monotonic recency counter (higher = more recent). NOT wall-clock.
    pub recorded_at: u64,
    /// 1-indexed line ranges a producer actually displayed under this tag,
    /// stored as merged `(start, end)` inclusive pairs. Empty means "no
    /// provenance recorded".
    pub seen_lines: Vec<(usize, usize)>,
}

impl Snapshot {
    /// Whether line `n` (1-indexed) was recorded as seen.
    pub fn has_seen(&self, n: usize) -> bool {
        self.seen_lines.iter().any(|&(s, e)| s <= n && n <= e)
    }

    /// Whether every line in `start..=end` was recorded as seen.
    pub fn has_seen_range(&self, start: usize, end: usize) -> bool {
        (start..=end).all(|n| self.has_seen(n))
    }
}

/// Canonicalize a path so `/tmp` vs symlinked spellings fuse. Falls back to the
/// raw path when the filesystem lookup fails (e.g. the file does not exist yet).
fn canonical_key(path: &str) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path))
}

/// Merge inclusive `(start, end)` ranges into `ranges`, coalescing overlaps.
fn merge_ranges(
    ranges: &mut Vec<(usize, usize)>,
    incoming: impl IntoIterator<Item = (usize, usize)>,
) {
    for (s, e) in incoming {
        if s == 0 || e < s {
            continue;
        }
        ranges.push((s, e));
    }
    ranges.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(s, e) in ranges.iter() {
        if let Some(last) = merged.last_mut() {
            if s <= last.1 + 1 {
                last.1 = last.1.max(e);
                continue;
            }
        }
        merged.push((s, e));
    }
    *ranges = merged;
}

/// In-memory snapshot store backed by an LRU of per-path version histories.
///
/// Recording byte-identical content refreshes recency and reuses the existing
/// tag (read fusion); recording new content pushes a fresh version onto the
/// front of the path history. Two distinct texts that collide on the short
/// 4-hex tag are retained as separate versions.
pub struct SnapshotStore {
    max_paths: usize,
    max_versions_per_path: usize,
    /// Per-path version history; index 0 is the head (most recent).
    versions: std::collections::HashMap<PathBuf, Vec<Snapshot>>,
    /// LRU recency order: front = most-recently used.
    recency: VecDeque<PathBuf>,
    /// Monotonic counter feeding `recorded_at`.
    counter: u64,
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_PATHS, DEFAULT_MAX_VERSIONS_PER_PATH)
    }
}

impl SnapshotStore {
    /// Create a store with explicit bounds.
    pub fn new(max_paths: usize, max_versions_per_path: usize) -> Self {
        SnapshotStore {
            max_paths: max_paths.max(1),
            max_versions_per_path: max_versions_per_path.max(1),
            versions: std::collections::HashMap::new(),
            recency: VecDeque::new(),
            counter: 0,
        }
    }

    fn touch_recency(&mut self, key: &Path) {
        if let Some(pos) = self.recency.iter().position(|p| p == key) {
            self.recency.remove(pos);
        }
        self.recency.push_front(key.to_path_buf());
        while self.recency.len() > self.max_paths {
            if let Some(evicted) = self.recency.pop_back() {
                self.versions.remove(&evicted);
            }
        }
    }

    /// Record the full normalized text of `path` and return its content tag.
    /// `seen_lines` (optional) are inclusive 1-indexed ranges the producer
    /// displayed; they merge into the version's provenance across reads of
    /// identical text.
    pub fn record(
        &mut self,
        path: &str,
        full_text: &str,
        seen_lines: impl IntoIterator<Item = (usize, usize)>,
    ) -> String {
        let key = canonical_key(path);
        let hash = compute_file_hash(full_text);
        self.counter += 1;
        let stamp = self.counter;

        let seen: Vec<(usize, usize)> = seen_lines.into_iter().collect();
        self.touch_recency(&key);
        let history = self.versions.entry(key.clone()).or_default();

        // Dedup requires full-text equality, not just tag equality.
        if let Some(pos) = history
            .iter()
            .position(|v| v.hash == hash && v.text == full_text)
        {
            let mut existing = history.remove(pos);
            existing.recorded_at = stamp;
            merge_ranges(&mut existing.seen_lines, seen);
            history.insert(0, existing);
            return hash;
        }

        let mut snap = Snapshot {
            path: key,
            text: full_text.to_string(),
            hash: hash.clone(),
            recorded_at: stamp,
            seen_lines: Vec::new(),
        };
        merge_ranges(&mut snap.seen_lines, seen);
        history.insert(0, snap);
        history.truncate(self.max_versions_per_path);
        hash
    }

    /// Most-recently recorded version for `path`, or `None`.
    pub fn head(&self, path: &str) -> Option<&Snapshot> {
        let key = canonical_key(path);
        self.versions.get(&key).and_then(|h| h.first())
    }

    /// Recorded version for `path` whose tag equals `hash`, or `None`. When two
    /// distinct texts collide on the tag, returns the most-recently recorded.
    pub fn by_hash(&self, path: &str, hash: &str) -> Option<&Snapshot> {
        let key = canonical_key(path);
        self.versions
            .get(&key)
            .and_then(|h| h.iter().find(|v| v.hash == hash))
    }

    /// Recorded version for `path` whose text equals `full_text`, or `None`.
    pub fn by_content(&self, path: &str, full_text: &str) -> Option<&Snapshot> {
        let key = canonical_key(path);
        self.versions
            .get(&key)
            .and_then(|h| h.iter().find(|v| v.text == full_text))
    }

    /// Whether the store has ever seen `hash` for `path` (drives
    /// `hash_recognized` in a mismatch).
    pub fn recognizes(&self, path: &str, hash: &str) -> bool {
        self.by_hash(path, hash).is_some()
    }

    /// Drop the version history for a single path.
    pub fn invalidate(&mut self, path: &str) {
        let key = canonical_key(path);
        self.versions.remove(&key);
        if let Some(pos) = self.recency.iter().position(|p| p == &key) {
            self.recency.remove(pos);
        }
    }

    /// Drop every version history.
    pub fn clear(&mut self) {
        self.versions.clear();
        self.recency.clear();
    }

    /// Number of distinct paths currently tracked.
    pub fn tracked_paths(&self) -> usize {
        self.versions.len()
    }
}
