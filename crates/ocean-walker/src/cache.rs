//! Shared walker scan cache used by owned-entry collection.

use std::{
    borrow::Cow,
    collections::HashMap,
    fmt,
    path::{Component, Path, PathBuf},
    sync::LazyLock,
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use rayon::{prelude::*, ThreadPool};

use crate::{CollectedEntries, CollectedEntry, FileType, WalkBackend, WalkError, WalkOptions};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    root: PathBuf,
    options: WalkOptions,
}

#[derive(Clone)]
struct CacheEntry {
    created_at: Instant,
    entries: Vec<CollectedEntry>,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<CacheKey, CacheEntry>,
    invalidation_generation: u64,
    next_scan_generation: u64,
    latest_scan_by_key: HashMap<CacheKey, u64>,
}

#[derive(Clone, Copy)]
struct ScanGeneration {
    invalidation: u64,
    scan: u64,
}

struct WalkPool {
    pool: Option<ThreadPool>,
    effective_workers: usize,
}

static CACHE_TTL_MS: LazyLock<u64> =
    LazyLock::new(|| env_uint("FS_SCAN_CACHE_TTL_MS", 1_000, 0, u64::MAX));
static EMPTY_RECHECK_MS: LazyLock<u64> =
    LazyLock::new(|| env_uint("FS_SCAN_EMPTY_RECHECK_MS", 200, 0, u64::MAX));
static MAX_CACHE_ENTRIES: LazyLock<usize> =
    LazyLock::new(|| env_uint("FS_SCAN_CACHE_MAX_ENTRIES", 16, 0, usize::MAX));
const DEFAULT_WALK_WORKERS: usize = 4;
const MAX_WALK_WORKERS: usize = 32;

static WALK_POOL: LazyLock<WalkPool> = LazyLock::new(|| {
    let configured = env_uint(
        "OCEAN_WALK_WORKERS",
        DEFAULT_WALK_WORKERS,
        0,
        MAX_WALK_WORKERS,
    );
    let workers = normalize_worker_count(configured);
    build_walk_pool_with(workers, |workers| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("ocean-walker-{index}"))
            .build()
    })
});
static SCAN_CACHE: LazyLock<Mutex<CacheState>> =
    LazyLock::new(|| Mutex::new(CacheState::default()));

fn env_uint<T>(name: &str, default: T, min: T, max: T) -> T
where
    T: Copy + Ord + std::str::FromStr,
{
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
        .clamp(min, max)
}

fn normalize_worker_count_with_available(configured: usize, available: usize) -> usize {
    if configured == 0 {
        available.clamp(1, MAX_WALK_WORKERS)
    } else {
        configured.clamp(1, MAX_WALK_WORKERS)
    }
}

fn available_worker_count() -> usize {
    std::thread::available_parallelism().map_or(DEFAULT_WALK_WORKERS, usize::from)
}

fn normalize_worker_count(configured: usize) -> usize {
    normalize_worker_count_with_available(configured, available_worker_count())
}

fn build_walk_pool_with<E>(
    workers: usize,
    build: impl FnOnce(usize) -> Result<ThreadPool, E>,
) -> WalkPool {
    if workers <= 1 {
        return WalkPool {
            pool: None,
            effective_workers: 1,
        };
    }
    match build(workers) {
        Ok(pool) => WalkPool {
            pool: Some(pool),
            effective_workers: workers,
        },
        Err(_) => WalkPool {
            pool: None,
            effective_workers: 1,
        },
    }
}

/// Configured cache TTL in milliseconds.
pub fn cache_ttl_ms() -> u64 {
    *CACHE_TTL_MS
}

/// Configured empty-result recheck threshold in milliseconds.
pub fn empty_recheck_ms() -> u64 {
    *EMPTY_RECHECK_MS
}

/// Configured maximum number of cache entries.
pub fn max_cache_entries() -> usize {
    *MAX_CACHE_ENTRIES
}

/// Effective worker count for filesystem traversal and related parallel work.
///
/// `OCEAN_WALK_WORKERS=0` means auto-detect, values are capped at 32, and `1`
/// forces serial work. If the dedicated pool cannot be constructed, this
/// reports `1` and all walker work uses the explicit serial path.
pub fn walk_workers() -> usize {
    WALK_POOL.effective_workers
}

/// Run work on the dedicated walker pool.
///
/// Callers only reach this helper after checking [`walk_workers`], so the
/// fallback executes ordinary serial code rather than entering Rayon's global
/// pool.
pub(crate) fn with_walk_pool<R>(operation: impl FnOnce() -> R + Send) -> R
where
    R: Send,
{
    if let Some(pool) = WALK_POOL.pool.as_ref() {
        pool.install(operation)
    } else {
        operation()
    }
}

const PARALLEL_MIN_FILES: usize = 256;

/// Return whether traversal-adjacent work should run in parallel.
pub fn should_parallelize(item_count: usize) -> bool {
    walk_workers() > 1 && item_count >= PARALLEL_MIN_FILES
}

/// Run traversal-adjacent work serially or on the centralized walker pool.
pub fn parallel_for_each<T, E>(
    items: &[T],
    operation: impl Fn(&T) -> std::result::Result<(), E> + Send + Sync,
) -> std::result::Result<(), E>
where
    T: Sync,
    E: Send,
{
    if !should_parallelize(items.len()) {
        return items.iter().try_for_each(operation);
    }
    with_walk_pool(|| items.par_iter().try_for_each(operation))
}

/// Run traversal-adjacent work with per-worker state on the centralized walker
/// pool.
pub fn parallel_for_each_init<T, S, E>(
    items: &[T],
    init: impl Fn() -> S + Send + Sync,
    operation: impl Fn(&mut S, &T) -> std::result::Result<(), E> + Send + Sync,
) -> std::result::Result<(), E>
where
    T: Sync,
    S: Send,
    E: Send,
{
    if !should_parallelize(items.len()) {
        let mut state = init();
        return items
            .iter()
            .try_for_each(|item| operation(&mut state, item));
    }
    with_walk_pool(|| items.par_iter().try_for_each_init(init, operation))
}

fn evict_to_make_room(state: &mut CacheState, max_entries: usize) {
    while state.entries.len() >= max_entries {
        let Some(oldest_key) = state
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.created_at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        state.entries.remove(&oldest_key);
    }
}

fn try_absolute_lexical_path(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

fn try_cache_key(root: &Path, mut options: WalkOptions) -> std::io::Result<CacheKey> {
    options.cache = false;
    Ok(CacheKey {
        root: try_absolute_lexical_path(root)?,
        options,
    })
}

#[cfg(test)]
fn absolute_lexical_path(path: &Path) -> PathBuf {
    try_absolute_lexical_path(path).expect("test cwd should resolve")
}

#[cfg(test)]
fn cache_key(root: &Path, options: WalkOptions) -> CacheKey {
    try_cache_key(root, options).expect("test cache root should resolve")
}

/// Project a filesystem path to a lossy normalized UTF-8 display string.
pub fn normalize_display_relative_path<'a>(root: &Path, path: &'a Path) -> Cow<'a, str> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    if cfg!(windows) {
        let relative = relative.to_string_lossy();
        if relative.contains('\\') {
            Cow::Owned(relative.replace('\\', "/"))
        } else {
            relative
        }
    } else {
        relative.to_string_lossy()
    }
}

/// Return whether a path contains the exact component name.
pub fn contains_component(path: &Path, target: &str) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|value| value == target)
    })
}

/// Return whether user-facing discovery should skip a relative path.
pub fn should_skip_path(path: &Path, mentions_node_modules: bool) -> bool {
    if contains_component(path, ".git") {
        return true;
    }
    if !mentions_node_modules && contains_component(path, "node_modules") {
        return true;
    }
    false
}

fn file_type_from_std(file_type: std::fs::FileType) -> Option<FileType> {
    if file_type.is_symlink() {
        Some(FileType::Symlink)
    } else if file_type.is_dir() {
        Some(FileType::Dir)
    } else if file_type.is_file() {
        Some(FileType::File)
    } else {
        None
    }
}

fn mtime_ms(metadata: &std::fs::Metadata) -> Option<f64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as f64)
}

/// Classify an existing filesystem path, skipping unsupported special files.
pub fn classify_file_type(path: &Path) -> Option<(FileType, Option<f64>, Option<u64>)> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let file_type = file_type_from_std(metadata.file_type())?;
    let size = if file_type == FileType::File {
        Some(metadata.len())
    } else {
        None
    };
    Some((file_type, mtime_ms(&metadata), size))
}

/// Resolve a search path string to a canonical directory path.
pub fn resolve_search_path(path: &str) -> Result<PathBuf, WalkError<String>> {
    let candidate = PathBuf::from(path);
    let root = if candidate.is_absolute() {
        candidate
    } else {
        let cwd = std::env::current_dir().map_err(|err| WalkError::InvalidData {
            path: PathBuf::from(path),
            message: format!("Failed to resolve cwd: {err}"),
        })?;
        cwd.join(candidate)
    };
    let metadata = std::fs::metadata(&root).map_err(|err| WalkError::InvalidData {
        path: root.clone(),
        message: format!("Path not found: {err}"),
    })?;
    if !metadata.is_dir() {
        return Err(WalkError::InvalidData {
            path: root,
            message: "Search path must be a directory".to_string(),
        });
    }
    Ok(std::fs::canonicalize(&root).unwrap_or(root))
}

fn collect_entries_uncached<H, E>(
    root: &Path,
    mut options: WalkOptions,
    heartbeat: &H,
) -> Result<CollectedEntries, WalkError<String>>
where
    H: Fn() -> std::result::Result<(), E> + Sync,
    E: fmt::Display,
{
    options.cache = false;
    crate::collect_entries_native(root, options, || heartbeat().map_err(|err| err.to_string()))
}

fn begin_scan(
    state: &Mutex<CacheState>,
    key: &CacheKey,
    ttl: Duration,
) -> Result<CollectedEntries, ScanGeneration> {
    let now = Instant::now();
    let mut state = state.lock();
    if let Some(entry) = state.entries.get(key) {
        let age = now.duration_since(entry.created_at);
        if age < ttl {
            return Ok(CollectedEntries {
                entries: entry.entries.clone(),
                cache_age_ms: age.as_millis() as u64,
                backend: WalkBackend::Cached,
            });
        }
        state.entries.remove(key);
    }
    state.next_scan_generation = state.next_scan_generation.wrapping_add(1);
    let generation = ScanGeneration {
        invalidation: state.invalidation_generation,
        scan: state.next_scan_generation,
    };
    state
        .latest_scan_by_key
        .insert(key.clone(), generation.scan);
    Err(generation)
}

fn publish_scan(
    state: &Mutex<CacheState>,
    key: CacheKey,
    generation: ScanGeneration,
    max_entries: usize,
    entries: &[CollectedEntry],
) {
    let mut state = state.lock();
    let is_latest = state.latest_scan_by_key.get(&key) == Some(&generation.scan);
    if !is_latest {
        return;
    }
    state.latest_scan_by_key.remove(&key);
    if state.invalidation_generation != generation.invalidation {
        return;
    }
    if max_entries == 0 {
        return;
    }
    evict_to_make_room(&mut state, max_entries);
    state.entries.insert(
        key,
        CacheEntry {
            created_at: Instant::now(),
            entries: entries.to_vec(),
        },
    );
}

fn get_or_scan_with<S>(
    state: &Mutex<CacheState>,
    key: CacheKey,
    ttl_ms: u64,
    max_entries: usize,
    scan: S,
) -> Result<CollectedEntries, WalkError<String>>
where
    S: FnOnce() -> Result<CollectedEntries, WalkError<String>>,
{
    if ttl_ms == 0 || max_entries == 0 {
        return scan();
    }
    let generation = match begin_scan(state, &key, Duration::from_millis(ttl_ms)) {
        Ok(hit) => return Ok(hit),
        Err(generation) => generation,
    };
    let mut fresh = match scan() {
        Ok(fresh) => fresh,
        Err(error) => {
            let mut state = state.lock();
            if state.latest_scan_by_key.get(&key) == Some(&generation.scan) {
                state.latest_scan_by_key.remove(&key);
            }
            return Err(error);
        }
    };
    fresh.backend = WalkBackend::Fresh;
    fresh.cache_age_ms = 0;
    publish_scan(state, key, generation, max_entries, &fresh.entries);
    Ok(fresh)
}

fn get_or_scan<H, E>(
    root: &Path,
    options: WalkOptions,
    heartbeat: &H,
) -> Result<CollectedEntries, WalkError<String>>
where
    H: Fn() -> std::result::Result<(), E> + Sync,
    E: fmt::Display,
{
    let key = match try_cache_key(root, options) {
        Ok(key) => key,
        Err(_) => return collect_entries_uncached(root, options, heartbeat),
    };
    get_or_scan_with(&SCAN_CACHE, key, *CACHE_TTL_MS, *MAX_CACHE_ENTRIES, || {
        collect_entries_uncached(root, options, heartbeat)
    })
}

pub fn collect_entries<H, E>(
    root: &Path,
    options: WalkOptions,
    heartbeat: H,
) -> Result<CollectedEntries, WalkError<String>>
where
    H: Fn() -> std::result::Result<(), E> + Sync,
    E: fmt::Display,
{
    heartbeat().map_err(|err| WalkError::Interrupted(err.to_string()))?;
    if options.cache {
        get_or_scan(root, options, &heartbeat)
    } else {
        collect_entries_uncached(root, options, &heartbeat)
    }
}

fn invalidate_path_in(state: &Mutex<CacheState>, target: &Path) {
    let target = match try_absolute_lexical_path(target) {
        Ok(target) => target,
        Err(_) => {
            invalidate_all_in(state);
            return;
        }
    };
    let mut state = state.lock();
    state.invalidation_generation = state.invalidation_generation.wrapping_add(1);
    state
        .entries
        .retain(|key, _| !target.starts_with(&key.root));
    state
        .latest_scan_by_key
        .retain(|key, _| !target.starts_with(&key.root));
}

/// Invalidate cache entries whose normalized absolute root contains `target`.
pub fn invalidate_path(target: &Path) {
    invalidate_path_in(&SCAN_CACHE, target);
}

/// Resolve a possibly relative path lexically and invalidate matching cache roots.
pub fn invalidate_path_string(path: &str) {
    invalidate_path(Path::new(path));
}

fn invalidate_all_in(state: &Mutex<CacheState>) {
    let mut state = state.lock();
    state.invalidation_generation = state.invalidation_generation.wrapping_add(1);
    state.entries.clear();
    state.latest_scan_by_key.clear();
}

/// Clear the entire scan cache and prevent older in-flight scans from publishing.
pub fn invalidate_all() {
    invalidate_all_in(&SCAN_CACHE);
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicU64, AtomicUsize, Ordering},
            Arc, Barrier,
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    #[cfg(unix)]
    use super::classify_file_type;
    use crate::{CollectedEntry, FileType};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard(PathBuf);

    impl TempDirGuard {
        fn new() -> Self {
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time is after UNIX_EPOCH")
                .as_nanos();
            let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("ocean-walker-cache-test-{timestamp}-{counter}"));
            fs::create_dir_all(&path).expect("create temp test directory");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn make_fifo(path: &Path) {
        let fifo_path =
            CString::new(path.as_os_str().as_bytes()).expect("fifo path has no NUL bytes");
        // SAFETY: `fifo_path` is a valid CString (NUL-terminated, no interior NULs),
        // so `as_ptr()` yields a valid C string pointer. `0o600` is a valid mode.
        // The CString is alive for the duration of the call.
        let rc = unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "create fifo: {}", std::io::Error::last_os_error());
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "test heartbeat helper matches production callback signature"
    )]
    fn ok_heartbeat() -> std::result::Result<(), String> {
        Ok(())
    }

    #[test]
    fn worker_count_is_capped_and_zero_uses_available_parallelism() {
        assert_eq!(super::normalize_worker_count_with_available(0, 8), 8);
        assert_eq!(super::normalize_worker_count_with_available(0, 0), 1);
        assert_eq!(super::normalize_worker_count_with_available(0, 100), 32);
        assert_eq!(super::normalize_worker_count_with_available(1, 8), 1);
        assert_eq!(super::normalize_worker_count_with_available(4, 8), 4);
        assert_eq!(super::normalize_worker_count_with_available(100, 8), 32);
    }

    #[test]
    fn pool_build_failure_reports_explicit_serial_fallback() {
        let pool = super::build_walk_pool_with(8, |_| Err::<rayon::ThreadPool, _>(()));
        assert_eq!(pool.effective_workers, 1);
        assert!(pool.pool.is_none());
    }

    fn scan_options(
        include_hidden: bool,
        use_gitignore: bool,
        detail: crate::WalkDetail,
    ) -> crate::WalkOptions {
        crate::WalkOptions {
            include_hidden,
            use_gitignore,
            skip_git: true,
            skip_node_modules: true,
            follow_links: crate::FollowLinks::Never,
            detail,
            directory_errors: crate::DirectoryErrorMode::SkipSkippable,
            ..crate::WalkOptions::default()
        }
    }

    fn assert_file_entry(entries: &[CollectedEntry], path: &str, size: f64) {
        let entry = entries
            .iter()
            .find(|entry| entry.display_path == path)
            .unwrap_or_else(|| panic!("expected file entry {path}, got {}", entry_paths(entries)));
        assert_eq!(entry.file_type, FileType::File);
        assert!(
            entry.mtime.is_some(),
            "full scan should include mtime for {path}"
        );
        assert_eq!(entry.size, Some(size));
    }

    fn assert_dir_entry(entries: &[CollectedEntry], path: &str) {
        let entry = entries
            .iter()
            .find(|entry| entry.display_path == path)
            .unwrap_or_else(|| panic!("expected dir entry {path}, got {}", entry_paths(entries)));
        assert_eq!(entry.file_type, FileType::Dir);
        assert!(
            entry.mtime.is_some(),
            "full scan should include mtime for {path}"
        );
        assert_eq!(entry.size, None);
    }

    fn entry_paths(entries: &[CollectedEntry]) -> String {
        let paths: Vec<&str> = entries
            .iter()
            .map(|entry| entry.display_path.as_str())
            .collect();
        format!("{paths:?}")
    }

    #[cfg(unix)]
    #[test]
    fn classify_file_type_skips_fifo() {
        let root = TempDirGuard::new();
        let fifo = root.path().join("skip-me.fifo");
        make_fifo(&fifo);

        assert_eq!(classify_file_type(&fifo), None);
    }

    #[test]
    fn collect_entries_skips_node_modules() {
        let root = TempDirGuard::new();
        fs::create_dir_all(root.path().join("node_modules/pkg")).unwrap();
        fs::write(root.path().join("node_modules/pkg/index.js"), "nm").unwrap();
        fs::write(root.path().join("real.txt"), "ok").unwrap();

        let entries = super::collect_entries(
            root.path(),
            scan_options(true, false, crate::WalkDetail::Full),
            ok_heartbeat,
        )
        .unwrap();
        let entries = entries.entries;
        let paths: Vec<&str> = entries
            .iter()
            .map(|entry| entry.display_path.as_str())
            .collect();
        assert!(
            !paths.iter().any(|path| path.contains("node_modules")),
            "expected no node_modules entries, got: {paths:?}"
        );
        assert!(
            paths.iter().any(|path| path == &"real.txt"),
            "expected real.txt, got: {paths:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn collect_entries_follow_links_always() {
        let root = TempDirGuard::new();
        fs::create_dir_all(root.path().join("target")).unwrap();
        fs::write(root.path().join("target/linked.txt"), "linked").unwrap();
        std::os::unix::fs::symlink(root.path().join("target"), root.path().join("link")).unwrap();

        let mut options = scan_options(true, false, crate::WalkDetail::Minimal);
        options.follow_links = crate::FollowLinks::Always;

        let entries = super::collect_entries(root.path(), options, ok_heartbeat).unwrap();
        let paths: Vec<&str> = entries
            .entries
            .iter()
            .map(|entry| entry.display_path.as_str())
            .collect();
        assert!(
            paths.iter().any(|path| path == &"link/linked.txt"),
            "follow-links always should yield symlink descendants, got: {paths:?}"
        );
    }

    #[test]
    fn traversal_gitignore_excludes_files() {
        let root = TempDirGuard::new();
        fs::create_dir_all(root.path().join(".git")).unwrap();
        fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.path().join("ignored.txt"), "ignored").unwrap();
        fs::write(root.path().join("kept.txt"), "keep").unwrap();

        let collected = super::collect_entries(
            root.path(),
            scan_options(true, true, crate::WalkDetail::Full),
            ok_heartbeat,
        )
        .unwrap();
        let collected = collected.entries;
        assert!(
            !collected
                .iter()
                .any(|entry| entry.display_path == "ignored.txt"),
            "collect_entries returned gitignored file: {}",
            entry_paths(&collected)
        );
        assert_file_entry(&collected, "kept.txt", 4.0);
    }

    #[test]
    fn traversal_hidden_disabled_excludes_files_and_descendants() {
        let root = TempDirGuard::new();
        fs::create_dir_all(root.path().join(".hidden-dir")).unwrap();
        fs::write(root.path().join(".hidden-dir/child.txt"), "child").unwrap();
        fs::write(root.path().join(".hidden-file"), "secret").unwrap();
        fs::write(root.path().join("visible.txt"), "visible").unwrap();

        let entries = super::collect_entries(
            root.path(),
            scan_options(false, false, crate::WalkDetail::Full),
            ok_heartbeat,
        )
        .unwrap();
        let entries = entries.entries;
        assert_eq!(
            entries.len(),
            1,
            "only visible.txt should be returned when hidden entries are disabled, got {}",
            entry_paths(&entries)
        );
        assert_file_entry(&entries, "visible.txt", 7.0);
        assert!(
            !entries
                .iter()
                .any(|entry| entry.display_path.starts_with(".hidden")),
            "hidden entries should be pruned before yielding files or descendants, got {}",
            entry_paths(&entries)
        );
    }

    #[test]
    fn traversal_hidden_enabled_includes_non_ignored_hidden_entries() {
        let root = TempDirGuard::new();
        fs::create_dir_all(root.path().join(".git")).unwrap();
        fs::write(root.path().join(".gitignore"), ".ignored-hidden\n").unwrap();
        fs::create_dir_all(root.path().join(".hidden-dir")).unwrap();
        fs::write(root.path().join(".hidden-dir/child.txt"), "child").unwrap();
        fs::write(root.path().join(".hidden-file"), "secret").unwrap();
        fs::write(root.path().join(".ignored-hidden"), "ignored").unwrap();

        let entries = super::collect_entries(
            root.path(),
            scan_options(true, true, crate::WalkDetail::Full),
            ok_heartbeat,
        )
        .unwrap();
        let entries = entries.entries;
        assert_file_entry(&entries, ".hidden-file", 6.0);
        assert_dir_entry(&entries, ".hidden-dir");
        assert_file_entry(&entries, ".hidden-dir/child.txt", 5.0);
        assert!(
            !entries
                .iter()
                .any(|entry| entry.display_path == ".ignored-hidden"),
            "gitignore should still exclude matching hidden files, got {}",
            entry_paths(&entries)
        );
    }

    #[test]
    fn collect_entries_respects_pre_cancelled_token() {
        let root = TempDirGuard::new();
        fs::write(root.path().join("real.txt"), "ok").unwrap();

        let result = super::collect_entries(
            root.path(),
            scan_options(true, false, crate::WalkDetail::Minimal),
            || Err("Timeout".to_string()),
        );

        let Err(err) = result else {
            panic!("pre-cancelled scans should fail before returning entries");
        };
        assert!(
            err.to_string().contains("Timeout"),
            "expected timeout cancellation error, got: {err}"
        );
    }

    fn test_entry(name: &str) -> CollectedEntry {
        CollectedEntry {
            native_relative_path: PathBuf::from(name),
            display_path: name.to_string(),
            file_type: FileType::File,
            mtime: None,
            size: None,
        }
    }

    fn test_scan(name: &str) -> Result<crate::CollectedEntries, crate::WalkError<String>> {
        Ok(crate::CollectedEntries {
            entries: vec![test_entry(name)],
            cache_age_ms: 0,
            backend: crate::WalkBackend::Fresh,
        })
    }

    #[test]
    fn cache_hit_provenance_is_explicit_even_when_age_rounds_to_zero() {
        let state = super::Mutex::new(super::CacheState::default());
        let key = super::cache_key(Path::new("cache-hit"), crate::WalkOptions::default());
        let first = super::get_or_scan_with(&state, key.clone(), 5_000, 16, || test_scan("one"))
            .expect("fresh scan succeeds");
        let second = super::get_or_scan_with(&state, key, 5_000, 16, || {
            panic!("immediate cache hit must not scan")
        })
        .expect("cache hit succeeds");

        assert_eq!(first.backend, crate::WalkBackend::Fresh);
        assert_eq!(second.backend, crate::WalkBackend::Cached);
    }

    #[test]
    fn invalidation_prevents_in_flight_scan_publication() {
        let state = Arc::new(super::Mutex::new(super::CacheState::default()));
        let root = super::absolute_lexical_path(Path::new("in-flight-invalidate"));
        let key = super::cache_key(&root, crate::WalkOptions::default());
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let thread = {
            let state = Arc::clone(&state);
            let key = key.clone();
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                super::get_or_scan_with(&state, key, 5_000, 16, || {
                    entered.wait();
                    release.wait();
                    test_scan("stale")
                })
            })
        };

        entered.wait();
        super::invalidate_path_in(&state, &root.join("mutated.txt"));
        release.wait();
        let returned = thread
            .join()
            .expect("scan thread joins")
            .expect("scan succeeds");
        assert_eq!(returned.entries[0].display_path, "stale");
        assert!(state.lock().entries.is_empty());

        let rescanned = super::get_or_scan_with(&state, key, 5_000, 16, || test_scan("new"))
            .expect("post-invalidation scan succeeds");
        assert_eq!(rescanned.backend, crate::WalkBackend::Fresh);
        assert_eq!(rescanned.entries[0].display_path, "new");
    }

    #[test]
    fn older_concurrent_scan_cannot_overwrite_newer_publication() {
        let state = Arc::new(super::Mutex::new(super::CacheState::default()));
        let key = super::cache_key(Path::new("scan-order"), crate::WalkOptions::default());
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let older = {
            let state = Arc::clone(&state);
            let key = key.clone();
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                super::get_or_scan_with(&state, key, 5_000, 16, || {
                    entered.wait();
                    release.wait();
                    test_scan("older")
                })
            })
        };

        entered.wait();
        let newer = super::get_or_scan_with(&state, key.clone(), 5_000, 16, || test_scan("newer"))
            .expect("newer scan succeeds");
        assert_eq!(newer.entries[0].display_path, "newer");
        release.wait();
        older
            .join()
            .expect("older scan thread joins")
            .expect("older scan succeeds");

        let hit = super::get_or_scan_with(&state, key, 5_000, 16, || {
            panic!("newer publication should remain cached")
        })
        .expect("cache hit succeeds");
        assert_eq!(hit.backend, crate::WalkBackend::Cached);
        assert_eq!(hit.entries[0].display_path, "newer");
    }

    #[test]
    fn cache_bound_is_strict_and_zero_disables_publication() {
        let state = super::Mutex::new(super::CacheState::default());
        for index in 0..3 {
            let key = super::cache_key(
                Path::new(&format!("bounded-{index}")),
                crate::WalkOptions::default(),
            );
            super::get_or_scan_with(&state, key, 5_000, 2, || test_scan("entry"))
                .expect("bounded scan succeeds");
            assert!(state.lock().entries.len() <= 2);
        }

        let disabled = super::Mutex::new(super::CacheState::default());
        let scans = AtomicUsize::new(0);
        let key = super::cache_key(Path::new("zero-bound"), crate::WalkOptions::default());
        for _ in 0..2 {
            super::get_or_scan_with(&disabled, key.clone(), 5_000, 0, || {
                scans.fetch_add(1, Ordering::SeqCst);
                test_scan("entry")
            })
            .expect("uncached scan succeeds");
        }
        assert_eq!(scans.load(Ordering::SeqCst), 2);
        assert!(disabled.lock().entries.is_empty());
    }

    #[test]
    fn failed_scan_is_not_cached() {
        let state = super::Mutex::new(super::CacheState::default());
        let key = super::cache_key(Path::new("failed-scan"), crate::WalkOptions::default());
        let failed = super::get_or_scan_with(&state, key.clone(), 5_000, 16, || {
            Err(crate::WalkError::Interrupted("cancelled".to_string()))
        });
        assert!(matches!(failed, Err(crate::WalkError::Interrupted(_))));
        assert!(state.lock().entries.is_empty());

        let retry = super::get_or_scan_with(&state, key, 5_000, 16, || test_scan("retry"))
            .expect("retry scan succeeds");
        assert_eq!(retry.backend, crate::WalkBackend::Fresh);
    }

    #[test]
    fn lexical_cache_namespace_normalizes_dot_segments_without_resolving_links() {
        let options = crate::WalkOptions::default();
        let base = super::absolute_lexical_path(Path::new("namespace-root"));
        assert_eq!(
            super::cache_key(&base.join("child/.."), options).root,
            super::cache_key(&base, options).root
        );

        #[cfg(unix)]
        {
            let tree = TempDirGuard::new();
            fs::create_dir(tree.path().join("target")).expect("target directory created");
            std::os::unix::fs::symlink(tree.path().join("target"), tree.path().join("link"))
                .expect("symlink created");
            assert_ne!(
                super::cache_key(&tree.path().join("target"), options).root,
                super::cache_key(&tree.path().join("link"), options).root,
                "lexical normalization must not canonicalize symlinks"
            );
        }
    }

    #[test]
    fn scan_detail_controls_metadata_collection() {
        let root = TempDirGuard::new();
        fs::write(root.path().join("real.txt"), "ok").unwrap();

        let minimal = super::collect_entries(
            root.path(),
            scan_options(true, false, crate::WalkDetail::Minimal),
            ok_heartbeat,
        )
        .unwrap();
        let minimal_file = minimal
            .entries
            .iter()
            .find(|entry| entry.display_path == "real.txt")
            .expect("minimal scan includes file");
        assert_eq!(minimal_file.mtime, None);
        assert_eq!(minimal_file.size, None);

        let full = super::collect_entries(
            root.path(),
            scan_options(true, false, crate::WalkDetail::Full),
            ok_heartbeat,
        )
        .unwrap();
        let full_file = full
            .entries
            .iter()
            .find(|entry| entry.display_path == "real.txt")
            .expect("full scan includes file");
        assert!(full_file.mtime.is_some(), "full scan should include mtime");
        assert_eq!(full_file.size, Some(2.0));
    }
}
