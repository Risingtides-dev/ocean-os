use super::*;
use std::io::{Read, Write};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub created_ms: i64,
    pub updated_ms: i64,
    pub model: String,
    pub provider: String,
    pub messages: Vec<Message>,
    /// Workspace anchor — git toplevel if the cwd is inside a repo,
    /// else the cwd itself. Used to bucket sessions per project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    /// The cwd this session was started in. May differ from
    /// `workspace_root` if cwd was inside a subdirectory of a repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Git branch captured at session creation, when in a repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Git short-commit captured at session creation, when in a repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// The client surface this session is currently bound to (the last
    /// `client_type` seen on a turn). Lets the runtime detect a
    /// surface switch between turns (Fix 3) and re-inject the right
    /// surface profile on resume (Fix 5). Old session files predate this
    /// field and deserialize as `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_type: Option<String>,
    /// Human-facing session label, captured from the ORIGINAL user prompt of the
    /// first turn — BEFORE any turn-prep composition (the surface flag, room /
    /// operator guidance, folder-as-agent instructions, the Longhouse advisory
    /// block, browser context). Set once on the first user turn and never
    /// overwritten, so the switcher label stays the user's own words even though
    /// the persisted first message carries the fully-composed prompt the model
    /// saw. `None` on legacy files (predate this field) and on turns whose caller
    /// supplied no display title; for those the read side derives and cleans the
    /// label from the first user message. See [`session_display_title`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl Session {
    /// Mint a session with a fresh random id. Only used by tests today
    /// (production always mints the id at the daemon layer and calls
    /// `new_with_id`), hence `cfg(test)` to keep the non-test build clean.
    #[cfg(test)]
    pub fn new(model: &Model) -> Self {
        Self::new_with_id(SessionId::new_v4(), model)
    }

    pub fn new_with_id(id: SessionId, model: &Model) -> Self {
        let now = ocean_protocol::now_ms();
        Self {
            id,
            created_ms: now,
            updated_ms: now,
            model: model.id.clone(),
            provider: model.provider.clone(),
            messages: Vec::new(),
            workspace_root: None,
            cwd: None,
            git_branch: None,
            git_commit: None,
            client_type: None,
            title: None,
        }
    }

    /// Tag this session with workspace metadata derived from the caller's cwd.
    /// First bind is unconditional. On resume, rebinds when the incoming cwd
    /// resolves to a different workspace root — so `cd /project-b && ocean
    /// --resume <id>` picks up project-b's context instead of staying stale.
    pub fn bind_workspace(&mut self, cwd: &Path) {
        let new_root = workspace_root(cwd);
        let same_root = self
            .workspace_root
            .as_deref()
            .map(|r| Path::new(r) == new_root.as_path())
            .unwrap_or(false);
        self.cwd = Some(cwd.to_string_lossy().into_owned());
        if same_root {
            // Same project — keep the workspace root and git metadata, but
            // refresh the recorded cwd to the latest launch directory.
            return;
        }
        // Different project (or first bind) — write all fields unconditionally.
        self.workspace_root = Some(new_root.to_string_lossy().into_owned());
        let (branch, commit) = probe_git(cwd);
        self.git_branch = branch;
        self.git_commit = commit;
    }

    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.messages = messages;
        self.updated_ms = ocean_protocol::now_ms();
    }

    /// Pin this session to a model + provider (session-config RPC v1). The pair
    /// always moves together — `provider` is derived from the model catalog by
    /// the caller, never trusted independently.
    pub fn set_model(&mut self, model: String, provider: String) {
        self.model = model;
        self.provider = provider;
        self.updated_ms = ocean_protocol::now_ms();
    }

    /// Adopt `original` as the session's display [`Session::title`] if unset.
    /// Called on the first user turn with the PRE-composition prompt (the user's
    /// own words, before the surface flag / guidance / Longhouse advisory are
    /// layered on), so the switcher label never collapses to an injected prefix.
    /// First write wins — later turns and resumes never relabel — and blank input
    /// is ignored so an empty turn can't erase the label. Stored already
    /// truncated so the persisted label matches what the read side would render.
    pub fn ensure_title(&mut self, original: &str) {
        if self.title.is_some() {
            return;
        }
        let trimmed = original.trim();
        if !trimmed.is_empty() {
            self.title = Some(truncate_title(trimmed));
        }
    }
}

/// Resolve the workspace root for `cwd`. Tries `git rev-parse --show-toplevel`
/// first; falls back to the cwd itself when not in a repo or git is absent.
pub fn workspace_root(cwd: &Path) -> PathBuf {
    match std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
    {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                PathBuf::from(s)
            } else {
                cwd.to_path_buf()
            }
        }
        _ => cwd.to_path_buf(),
    }
}

/// Encode a workspace path as a filesystem-safe slug. Leading slash
/// dropped, remaining slashes turned into dashes, then prefixed with a
/// leading dash so directory listings sort intuitively.
pub fn workspace_slug(root: &Path) -> String {
    let s = root.to_string_lossy();
    let trimmed = s.trim_start_matches('/');
    let slug: String = trimmed
        .chars()
        .map(|c| match c {
            '/' => '-',
            '\\' => '-',
            ':' => '-',
            ' ' => '-',
            c => c,
        })
        .collect();
    format!("-{slug}")
}

pub(crate) fn probe_git(cwd: &Path) -> (Option<String>, Option<String>) {
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("rev-parse")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() || s == "HEAD" {
                    None
                } else {
                    Some(s)
                }
            } else {
                None
            }
        });
    let commit = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("rev-parse")
        .arg("--short")
        .arg("HEAD")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            } else {
                None
            }
        });
    (branch, commit)
}

pub fn sessions_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("sessions")
}

/// Per-workspace bucket: `<config>/sessions/<workspace-slug>/`.
fn workspace_dir(config_dir: &Path, workspace_root: &str) -> PathBuf {
    sessions_dir(config_dir).join(workspace_slug(Path::new(workspace_root)))
}

/// Migrate any loose `sessions/<uuid>.json` files into
/// `sessions/legacy/<uuid>.json`. Idempotent — running twice is a
/// no-op once the loose files are gone. Failures here are best-effort;
/// we don't crash the daemon over a stuck migration.
pub fn migrate_legacy_sessions(config_dir: &Path) {
    let dir = sessions_dir(config_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let legacy = dir.join("legacy");
    let mut made_dir = false;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if !made_dir {
            if std::fs::create_dir_all(&legacy).is_err() {
                return;
            }
            made_dir = true;
        }
        if let Some(name) = path.file_name() {
            let _ = std::fs::rename(&path, legacy.join(name));
        }
    }
}

/// Env knob for the session-file TTL, in days. Default 90; `0` disables
/// pruning entirely. Read once per GC pass.
const SESSION_TTL_ENV: &str = "OCEAN_SESSION_TTL_DAYS";
const DEFAULT_SESSION_TTL_DAYS: u64 = 90;

/// Resolve the session-file TTL from `OCEAN_SESSION_TTL_DAYS` into the
/// `Option<Duration>` that [`session_file_gc`] consumes. Read ONCE here, off
/// the GC function itself, so the GC stays a pure function of its arguments
/// (no process-global env reads — that keeps its tests free of env races).
///
/// Semantics:
/// - unset / unparseable → default 90 days
/// - `0` → `None` (pruning disabled)
/// - any other `n` → `Some(n days)`
///
/// Overflow safety (the OCEAN-211 bug): `days * 86_400` can overflow `u64`
/// for huge inputs — debug builds panicked at startup (GC runs in
/// `from_env`), release wrapped to a tiny TTL that would delete sessions the
/// operator meant to keep. See [`ttl_from_days`] for the overflow-safe
/// conversion.
pub(crate) fn ttl_from_env() -> Option<std::time::Duration> {
    ttl_from_days(ttl_days_from_env())
}

/// The raw day count from the env, before conversion. Split out so the
/// conversion logic (the overflow-prone part, [`ttl_from_days`]) is
/// unit-testable without touching the process-global env.
fn ttl_days_from_env() -> u64 {
    match std::env::var(SESSION_TTL_ENV) {
        Ok(v) => v.trim().parse().unwrap_or(DEFAULT_SESSION_TTL_DAYS),
        Err(_) => DEFAULT_SESSION_TTL_DAYS,
    }
}

/// Convert a TTL in days to a `Duration`, overflow-safely.
///
/// - `0` → `None` (pruning disabled).
/// - overflow on `days * 86_400` → SATURATE to `Duration::MAX`
///   ("never prune") rather than panicking (debug) or wrapping down to a
///   tiny TTL (release) that would delete sessions the operator meant to
///   keep. A TTL that large can't be exceeded by any real file age, so
///   nothing is pruned — the safe direction for an absurd input.
/// - otherwise → `Some(days as a Duration)`.
pub(crate) fn ttl_from_days(days: u64) -> Option<std::time::Duration> {
    if days == 0 {
        return None; // disabled
    }
    let ttl = days
        .checked_mul(24 * 60 * 60)
        .map_or(std::time::Duration::MAX, std::time::Duration::from_secs);
    Some(ttl)
}

/// Prune on-disk session files older than the configured TTL.
///
/// Background: the store caps message *count* per session
/// (`MAX_SESSION_MESSAGES`) but never deleted old session *files*, so a
/// long-lived daemon accumulated `sessions/<workspace>/*.json` unbounded —
/// and [`list`] deserializes every one of them on each call, so the dir's
/// growth slows listing too. This is distinct from OCEAN-182's in-memory
/// per-session *lock* prune (that's the registry; this is the files).
///
/// Age signal: file **mtime** via `metadata()`, not the persisted
/// `updated_ms`. mtime is cheaper (no read + JSON parse per file) and the
/// save path rewrites the file on every turn, so mtime tracks last-touch
/// faithfully. GC is opportunistic, but there's no reason to deserialize
/// thousands of files when a stat suffices.
///
/// Safety: a file whose mtime is older than the TTL (default 90d) cannot
/// belong to an active session — an in-flight turn rewrites the file on
/// save, refreshing its mtime — so the TTL gate alone protects live
/// sessions. No need to consult the in-memory session/lock map.
///
/// `ttl` is the resolved age threshold (`None` = pruning disabled / never
/// prune); the caller reads `OCEAN_SESSION_TTL_DAYS` once via [`ttl_from_env`]
/// and passes the result in. Keeping the env OUT of this function makes it a
/// pure function of its arguments — its tests pass an explicit `ttl` and
/// never mutate the process-global env, so they can't race each other
/// (OCEAN-211).
///
/// Returns the number of files pruned. Logs a single info summary when it
/// prunes anything; silent otherwise. Best-effort: I/O errors on individual
/// entries are skipped, never fatal — GC must never wedge startup.
pub fn session_file_gc(config_dir: &Path, ttl: Option<std::time::Duration>) -> usize {
    let Some(ttl) = ttl else {
        return 0; // disabled
    };
    let now = std::time::SystemTime::now();
    let root = sessions_dir(config_dir);

    let mut pruned = 0usize;
    let mut oldest_age: Option<std::time::Duration> = None;

    // Walk one level of workspace buckets plus any loose top-level files.
    let mut json_files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Ok(bucket) = std::fs::read_dir(&path) {
                    for f in bucket.flatten() {
                        json_files.push(f.path());
                    }
                }
            } else {
                json_files.push(path);
            }
        }
    }

    for path in json_files {
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(mtime) else {
            continue; // mtime in the future — leave it alone
        };
        if age > ttl && std::fs::remove_file(&path).is_ok() {
            pruned += 1;
            if oldest_age.is_none_or(|o| age > o) {
                oldest_age = Some(age);
            }
        }
    }

    if pruned > 0 {
        let oldest_days = oldest_age
            .map(|d| d.as_secs() / (24 * 60 * 60))
            .unwrap_or(0);
        let ttl_days = ttl.as_secs() / (24 * 60 * 60);
        tracing::info!(
            pruned,
            oldest_age_days = oldest_days,
            ttl_days,
            "session_file_gc: pruned old session files"
        );
    }
    pruned
}

pub fn save(config_dir: &Path, session: &Session) -> anyhow::Result<PathBuf> {
    let dir = match session.workspace_root.as_deref() {
        Some(root) => workspace_dir(config_dir, root),
        None => sessions_dir(config_dir).join("legacy"),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let path = dir.join(format!("{}.json", session.id));
    let json = serde_json::to_string_pretty(session)?;
    // Atomic + durable write: a crash mid-write must never corrupt an
    // existing good transcript. Write to a temp sibling, fsync it, then
    // rename over the target.
    let tmp = dir.join(format!(".{}.json.tmp", session.id));
    {
        let mut file =
            std::fs::File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        file.write_all(json.as_bytes())
            .with_context(|| format!("write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("fsync {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;

    // Split-brain invariant (one id == one file): a prior
    // `bind_workspace` rebind may have left this session's `<id>.json` in
    // another bucket. Remove every other copy so the loader can never
    // resolve a stale empty stub over this canonical file. Best-effort —
    // a failed unlink is warned, not fatal.
    purge_duplicate_session_files(config_dir, session.id, &path);

    Ok(path)
}

/// Remove every `<id>.json` for `id` except `keep`. Best-effort: failed
/// unlinks are warned, never fatal. Enforces the one-id-one-file invariant
/// both on save (clearing the pre-rebind orphan) and as the loader's
/// self-heal (collapsing duplicates it just resolved).
fn purge_duplicate_session_files(config_dir: &Path, id: SessionId, keep: &Path) {
    let target = format!("{id}.json");
    for candidate in candidate_session_paths(config_dir, &target) {
        if candidate.as_path() == keep || !candidate.exists() {
            continue;
        }
        if let Err(e) = std::fs::remove_file(&candidate) {
            tracing::warn!(
                path = %candidate.display(),
                error = %e,
                "session: failed to remove orphaned duplicate session file"
            );
        }
    }
}

pub fn load(config_dir: &Path, id: SessionId) -> anyhow::Result<Session> {
    match load_resumable(config_dir, id)? {
        Some(session) => Ok(session),
        None => anyhow::bail!("session {id} not found"),
    }
}

/// Load a session for resumption, distinguishing "no session file exists"
/// (Ok(None) — safe to start fresh) from "a session file exists but could
/// not be read or parsed" (Err — must NOT be treated as a fresh session,
/// or the entire prior transcript is silently discarded mid-chat).
pub fn load_resumable(config_dir: &Path, id: SessionId) -> anyhow::Result<Option<Session>> {
    let target = format!("{id}.json");
    // Gather every existing copy across all workspace buckets + top-level.
    let existing: Vec<PathBuf> = candidate_session_paths(config_dir, &target)
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    if existing.is_empty() {
        return Ok(None);
    }

    // Parse each copy, partitioning successes from errors so we can both
    // pick a deterministic winner (split-brain: a rebind left >1 file) and
    // still honor the corrupt→Err contract below.
    let mut parsed: Vec<(PathBuf, Session)> = Vec::new();
    let mut errors: Vec<(PathBuf, anyhow::Error)> = Vec::new();
    for p in &existing {
        match std::fs::read_to_string(p)
            .with_context(|| format!("read {}", p.display()))
            .and_then(|text| {
                serde_json::from_str::<Session>(&text)
                    .with_context(|| format!("parse {}", p.display()))
            }) {
            Ok(s) => parsed.push((p.clone(), s)),
            Err(e) => errors.push((p.clone(), e)),
        }
    }

    if parsed.is_empty() {
        // At least one file existed but none parsed — propagate the first
        // error so the daemon resume path never treats a corrupt-on-disk
        // transcript as a fresh session.
        let (_, e) = errors
            .into_iter()
            .next()
            .expect("existing non-empty with no parsed copies implies an error");
        return Err(e);
    }

    // Deterministic winner: newest updated_ms, then most messages, then
    // lexicographically largest path — a stable, FS-order-independent
    // tiebreak so even two truly-identical copies resolve to one.
    parsed.sort_by(|(pa, a), (pb, b)| {
        b.updated_ms
            .cmp(&a.updated_ms)
            .then_with(|| b.messages.len().cmp(&a.messages.len()))
            .then_with(|| pb.cmp(pa))
    });
    let (winner_path, winner) = parsed.into_iter().next().expect("parsed non-empty");

    // Self-heal: delete every other copy so the next load is deterministic
    // even without a follow-up save. The winner supersedes stale valid
    // copies and any corrupt ones alike.
    let had_duplicates = existing.len() > 1;
    purge_duplicate_session_files(config_dir, id, &winner_path);
    if had_duplicates {
        tracing::warn!(
            session_id = %id,
            winner = %winner_path.display(),
            "session: multiple files found for one id; loaded the newest and removed the rest"
        );
    }

    Ok(Some(winner))
}

fn candidate_session_paths(config_dir: &Path, filename: &str) -> Vec<PathBuf> {
    let root = sessions_dir(config_dir);
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.push(p.join(filename));
            }
        }
    }
    // Final fallback: a loose file in the legacy layout we haven't migrated yet.
    out.push(root.join(filename));
    out
}

pub fn detail(config_dir: &Path, id: SessionId) -> anyhow::Result<SessionDetail> {
    let session = load(config_dir, id)?;
    Ok(session_detail(session))
}

fn history_search_capacity_error(observed_bytes: u64, max_bytes: u64) -> anyhow::Error {
    HistorySearchCapacityError {
        observed_bytes,
        max_bytes,
    }
    .into()
}

fn enforce_history_search_store_budget(paths: &[PathBuf], max_bytes: u64) -> anyhow::Result<()> {
    let mut observed_bytes = 0u64;
    for path in paths {
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        observed_bytes = observed_bytes.saturating_add(metadata.len());
        if observed_bytes > max_bytes {
            return Err(history_search_capacity_error(observed_bytes, max_bytes));
        }
    }
    Ok(())
}

fn read_history_file_bounded(
    path: &Path,
    observed_bytes: &mut u64,
    max_bytes: u64,
) -> anyhow::Result<Option<String>> {
    let Ok(file) = std::fs::File::open(path) else {
        return Ok(None);
    };
    let remaining = max_bytes.saturating_sub(*observed_bytes);
    let mut bytes = Vec::new();
    if file
        .take(remaining.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Ok(None);
    }
    let next_observed = observed_bytes.saturating_add(bytes.len() as u64);
    if next_observed > max_bytes {
        return Err(history_search_capacity_error(next_observed, max_bytes));
    }
    *observed_bytes = next_observed;
    Ok(String::from_utf8(bytes).ok())
}

/// Search persisted display transcript text for user and assistant entries only.
/// Tool results and non-display content blocks are never considered. The store's
/// cumulative file size is bounded before any raw session is deserialized.
pub fn search_history(
    config_dir: &Path,
    query: &str,
    limit: usize,
) -> anyhow::Result<Vec<HistorySearchHit>> {
    let root = sessions_dir(config_dir);
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&root)
        .with_context(|| format!("read {}", root.display()))?
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(children) = std::fs::read_dir(&path) {
                paths.extend(children.flatten().map(|child| child.path()));
            }
        } else {
            paths.push(path);
        }
    }
    paths.retain(|path| path.extension().and_then(|value| value.to_str()) == Some("json"));
    paths.sort();
    enforce_history_search_store_budget(&paths, MAX_HISTORY_SEARCH_STORE_BYTES)?;

    // Preserve the one-id-one-session invariant even if a stale duplicate is
    // present on disk. Selection mirrors the resumable loader, without mutating
    // files on this read-only path.
    let mut sessions: HashMap<SessionId, (PathBuf, Session)> = HashMap::new();
    let mut observed_bytes = 0u64;
    for path in paths {
        let Some(text) =
            read_history_file_bounded(&path, &mut observed_bytes, MAX_HISTORY_SEARCH_STORE_BYTES)?
        else {
            continue;
        };
        let Ok(candidate) = serde_json::from_str::<Session>(&text) else {
            continue;
        };
        let replace = sessions
            .get(&candidate.id)
            .is_none_or(|(winner_path, winner)| {
                candidate.updated_ms > winner.updated_ms
                    || (candidate.updated_ms == winner.updated_ms
                        && (candidate.messages.len() > winner.messages.len()
                            || (candidate.messages.len() == winner.messages.len()
                                && path > *winner_path)))
            });
        if replace {
            sessions.insert(candidate.id, (path, candidate));
        }
    }

    let mut hits = Vec::new();
    for (_, session) in sessions.into_values() {
        let title = session_display_title(&session);
        for (message_index, message) in session.messages.iter().enumerate() {
            let entry = transcript_entry(message);
            if !matches!(entry.role.as_str(), "user" | "assistant") || entry.text.trim().is_empty()
            {
                continue;
            }
            let squashed = entry.text.split_whitespace().collect::<Vec<_>>().join(" ");
            let Some((match_kind, score, match_span)) = score_match(query, &squashed) else {
                continue;
            };
            hits.push(HistorySearchHit {
                hit_id: format!("{}:{message_index}", session.id),
                session_id: session.id,
                session_title: title.clone(),
                role: entry.role,
                excerpt: bounded_excerpt(&squashed, match_span),
                timestamp_ms: entry.timestamp_ms,
                workspace_root: session.workspace_root.clone(),
                score,
                match_kind,
            });
            // Keep memory proportional to the requested result bound rather
            // than the number of matching turns in the complete session store.
            let prune_at = limit.saturating_mul(4).max(limit.saturating_add(1));
            if hits.len() >= prune_at {
                hits.sort_by(compare_history_hits);
                hits.truncate(limit.saturating_mul(2).max(limit));
            }
        }
    }

    hits.sort_by(compare_history_hits);
    hits.truncate(limit);
    Ok(hits)
}

fn compare_history_hits(a: &HistorySearchHit, b: &HistorySearchHit) -> std::cmp::Ordering {
    match_rank(b.match_kind)
        .cmp(&match_rank(a.match_kind))
        .then_with(|| b.score.total_cmp(&a.score))
        .then_with(|| {
            b.timestamp_ms
                .unwrap_or(0)
                .cmp(&a.timestamp_ms.unwrap_or(0))
        })
        .then_with(|| a.hit_id.cmp(&b.hit_id))
}

fn match_rank(kind: HistoryMatchKind) -> u8 {
    match kind {
        HistoryMatchKind::Exact => 3,
        HistoryMatchKind::Lexical => 2,
        HistoryMatchKind::Fuzzy => 1,
    }
}

fn normalized_phrase(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[derive(Debug, Clone, Copy)]
struct MatchSpan {
    start: usize,
    end: usize,
}

fn lexical_tokens(value: &str) -> Vec<(String, MatchSpan)> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut start = 0usize;
    let mut end = 0usize;
    for (index, character) in value.chars().enumerate() {
        if character.is_alphanumeric() {
            if token.is_empty() {
                start = index;
            }
            token.extend(character.to_lowercase());
            end = index + 1;
        } else if !token.is_empty() {
            tokens.push((std::mem::take(&mut token), MatchSpan { start, end }));
        }
    }
    if !token.is_empty() {
        tokens.push((token, MatchSpan { start, end }));
    }
    tokens
}

fn folded_chars(value: &str, alphanumeric_only: bool) -> (Vec<char>, Vec<usize>) {
    let mut folded = Vec::new();
    let mut source_positions = Vec::new();
    for (source_index, character) in value.chars().enumerate() {
        if alphanumeric_only && !character.is_alphanumeric() {
            continue;
        }
        for lowercase in character.to_lowercase() {
            folded.push(lowercase);
            source_positions.push(source_index);
        }
    }
    (folded, source_positions)
}

fn score_match(query: &str, text: &str) -> Option<(HistoryMatchKind, f64, MatchSpan)> {
    let normalized_query = normalized_phrase(query);
    let (needle_phrase, _) = folded_chars(&normalized_query, false);
    let (haystack_phrase, phrase_positions) = folded_chars(text, false);
    if !needle_phrase.is_empty() {
        if let Some(start) = haystack_phrase
            .windows(needle_phrase.len())
            .position(|window| window == needle_phrase)
        {
            let end = start + needle_phrase.len() - 1;
            let ratio = needle_phrase.len() as f64 / haystack_phrase.len().max(1) as f64;
            return Some((
                HistoryMatchKind::Exact,
                3.0 + ratio.min(1.0) * 0.5,
                MatchSpan {
                    start: phrase_positions[start],
                    end: phrase_positions[end] + 1,
                },
            ));
        }
    }

    let query_tokens = lexical_tokens(query);
    let text_tokens = lexical_tokens(text);
    let lexical_spans = query_tokens
        .iter()
        .map(|(query_token, _)| {
            text_tokens
                .iter()
                .find(|(text_token, _)| text_token == query_token)
                .map(|(_, span)| *span)
        })
        .collect::<Option<Vec<_>>>();
    if !query_tokens.is_empty() {
        if let Some(lexical_spans) = lexical_spans {
            let ratio = query_tokens.len() as f64 / text_tokens.len().max(1) as f64;
            return Some((
                HistoryMatchKind::Lexical,
                2.0 + ratio.min(1.0) * 0.5,
                MatchSpan {
                    start: lexical_spans.iter().map(|span| span.start).min().unwrap(),
                    end: lexical_spans.iter().map(|span| span.end).max().unwrap(),
                },
            ));
        }
    }

    let (needle, _) = folded_chars(query, true);
    let (haystack, positions) = folded_chars(text, true);
    if needle.is_empty() || haystack.is_empty() {
        return None;
    }
    let mut matched = 0usize;
    let mut start = 0usize;
    for (index, character) in haystack.iter().enumerate() {
        if *character == needle[matched] {
            if matched == 0 {
                start = index;
            }
            matched += 1;
            if matched == needle.len() {
                let span = index - start + 1;
                let compactness = needle.len() as f64 / span as f64;
                let coverage = needle.len() as f64 / haystack.len() as f64;
                return Some((
                    HistoryMatchKind::Fuzzy,
                    1.0 + compactness * 0.75 + coverage.min(1.0) * 0.24,
                    MatchSpan {
                        start: positions[start],
                        end: positions[index] + 1,
                    },
                ));
            }
        }
    }
    None
}

const HISTORY_EXCERPT_CHARS: usize = 240;

fn bounded_excerpt(text: &str, match_span: MatchSpan) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= HISTORY_EXCERPT_CHARS {
        return text.to_string();
    }

    // Reserve room for both ellipses, then center the window on the match span.
    // This stays Unicode-safe because every boundary is a character index.
    let content_chars = HISTORY_EXCERPT_CHARS - 2;
    let center = match_span.start + (match_span.end.saturating_sub(match_span.start) / 2);
    let start = center
        .saturating_sub(content_chars / 2)
        .min(chars.len() - content_chars);
    let end = start + content_chars;
    let mut excerpt = String::new();
    if start > 0 {
        excerpt.push('…');
    }
    excerpt.extend(chars[start..end].iter());
    if end < chars.len() {
        excerpt.push('…');
    }
    excerpt
}

/// List sessions, optionally scoped to a single workspace root.
/// `workspace_root = None` returns every session across every bucket.
pub fn list(
    config_dir: &Path,
    workspace_root: Option<&str>,
) -> anyhow::Result<Vec<SessionSummary>> {
    let dir = sessions_dir(config_dir);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for bucket in std::fs::read_dir(&dir)?.flatten() {
        let bucket_path = bucket.path();
        if !bucket_path.is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(&bucket_path)?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(session) = serde_json::from_str::<Session>(&text) else {
                continue;
            };
            if let Some(filter) = workspace_root {
                if session.workspace_root.as_deref() != Some(filter) {
                    continue;
                }
            }
            let title = session_display_title(&session);
            out.push(SessionSummary {
                id: session.id,
                model: session.model,
                turns: session.messages.len() as u32,
                title,
                workspace_root: session.workspace_root,
                git_branch: session.git_branch,
                updated_ms: Some(session.updated_ms),
            });
        }
    }
    // Newest first by updated_ms; fall back to id ordering when missing.
    out.sort_by(|a, b| {
        b.updated_ms
            .unwrap_or(0)
            .cmp(&a.updated_ms.unwrap_or(0))
            .then_with(|| b.id.cmp(&a.id))
    });
    // Cluster by workspace_root so a project's sessions stay together in the
    // rail, while preserving the newest-first recency order WITHIN each
    // cluster (sort_by is stable). Rootless sessions sort last. Group order
    // is by root path so the same project always lands in the same place.
    out.sort_by(|a, b| match (&a.workspace_root, &b.workspace_root) {
        (Some(x), Some(y)) => x.cmp(y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    Ok(out)
}

/// Untruncated text of the first user message, if any. The display-title
/// derivation cleans and truncates this itself, so it needs the raw text — the
/// legacy Longhouse-block strip keys off the `\n\n` separator that
/// [`truncate_title`]'s whitespace-squash would otherwise erase.
fn first_user_raw(messages: &[Message]) -> Option<String> {
    messages.iter().find_map(|message| match message {
        Message::User { content, .. } => content
            .iter()
            .find_map(|content| content.as_text().map(str::to_string)),
        _ => None,
    })
}

/// Strip a leading surface flag (`[WEB] `, `[TUI] `, …) if present. The runtime
/// prefixes every persisted user message with the surface flag; it is not part
/// of the user's words, so the label derivation drops it. Matched against the
/// closed [`system_prompt::surface_flag`] taxonomy (kept honest by
/// `surface_flag_taxonomy_is_canonical`) rather than a loose all-caps pattern,
/// so a user prompt that opens with its OWN bracketed token (`[TODO] …`) is left
/// intact — only a genuine surface flag is removed.
fn strip_surface_flag(text: &str) -> &str {
    // The full set of flags `surface_flag` can emit, including the `?` unknown.
    const KNOWN_FLAGS: &[&str] = &[
        "BRWSR", "TUI", "WEB", "TAURI", "GUI", "CLI", "VOX", "ACP", "SLACK", "CNVS", "MOBL", "?",
    ];
    let Some(rest) = text.strip_prefix('[') else {
        return text;
    };
    let Some(close) = rest.find(']') else {
        return text;
    };
    if !KNOWN_FLAGS.contains(&&rest[..close]) {
        return text;
    }
    rest[close + 1..].trim_start()
}

/// Strip a leading Longhouse advisory block for LEGACY sessions whose stored
/// first message was composed before the [`Session::title`] field existed. The
/// daemon prepends the block as `{block}\n\n{prompt}`, so when the (flag-
/// stripped) text opens with the advisory marker, drop through the first blank
/// line to recover the user's prompt. New sessions carry an explicit `title`
/// and never reach this path. Deliberately narrow — a marker-anchored fallback
/// for the OCEAN-318 pollution this field was added to fix, not a general
/// prompt un-composer.
fn strip_longhouse_block(text: &str) -> &str {
    const MARKER: &str = "Longhouse consult (advisory";
    if text.starts_with(MARKER) {
        if let Some(idx) = text.find("\n\n") {
            return text[idx + 2..].trim_start();
        }
    }
    text
}

/// Strip a leading operator-guidance block. `apply_turn_guidance` in the daemon
/// prepends the per-turn operator hints as `{block}\n\n{prompt}` with the header
/// `Operator guidance for this turn:`; same marker-anchored, `\n\n`-terminated
/// shape as [`strip_longhouse_block`] (the block's own lines use single `\n`, so
/// the first blank line bounds it). Kept in sync with the daemon renderer by
/// `render_turn_guidance_block_anchors_display_strip_marker` in ocean-daemon.
fn strip_operator_guidance_block(text: &str) -> &str {
    const MARKER: &str = "Operator guidance for this turn:";
    if text.starts_with(MARKER) {
        if let Some(idx) = text.find("\n\n") {
            return text[idx + 2..].trim_start();
        }
    }
    text
}

/// Peel the daemon-injected turn-prep layers off the front of a stored
/// user-message body, for DISPLAY only (TASK-50). The runtime persists the fully
/// composed prompt the model saw — the surface flag plus whatever steering blocks
/// the daemon prepended plus the user's words — but a product transcript must
/// render the user's own words, not the injected preamble. Reuses the same
/// marker-anchored primitives the session-label derivation uses
/// ([`strip_surface_flag`], [`strip_longhouse_block`],
/// [`strip_operator_guidance_block`]) so each marker has a single source of
/// truth. The daemon STACKS layers (`[FLAG] {longhouse}\n\n{guidance}\n\n
/// {prompt}`), so this peels in a loop: each pass removes at most one leading
/// layer and repeats until the front no longer matches a known marker. A body
/// that never opened with a known marker is returned byte-for-byte (no false
/// strips) — the anchoring is exact-prefix, so genuine user text that merely
/// mentions a marker mid-line is untouched.
///
/// DEFERRED (documented, intentionally not stripped): the folder-as-agent
/// instruction prefix has no marker to anchor on, and the `## Browser context`
/// block carries an internal blank line so the shared `\n\n` terminator cannot
/// bound it. Both are left intact rather than risk truncating genuine content;
/// when either leads (folder-agent / surface-extension turns) it also shadows an
/// inner Longhouse block from this peel. See the TASK-50 report.
fn strip_injected_turn_prep(text: &str) -> &str {
    let mut cur = text;
    loop {
        // Each sub-strip returns either `cur` unchanged or a strictly shorter
        // suffix, so an unchanged length is a sound fixpoint signal.
        let peeled = strip_operator_guidance_block(strip_longhouse_block(strip_surface_flag(cur)));
        if peeled.len() == cur.len() {
            return peeled;
        }
        cur = peeled;
    }
}

/// The switcher/label title for a session: the explicit [`Session::title`]
/// captured from the original prompt when present (new sessions), otherwise the
/// first user message with the surface flag and any legacy Longhouse advisory
/// prefix stripped, then truncated (legacy sessions predating the field).
pub(crate) fn session_display_title(session: &Session) -> String {
    if let Some(title) = session.title.as_deref() {
        let trimmed = title.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    match first_user_raw(&session.messages) {
        Some(raw) => truncate_title(strip_longhouse_block(strip_surface_flag(&raw))),
        None => String::new(),
    }
}

pub(crate) fn session_detail(session: Session) -> SessionDetail {
    let title = session_display_title(&session);
    let transcript = session
        .messages
        .iter()
        .map(transcript_entry)
        .collect::<Vec<_>>();
    let tool_context = session
        .messages
        .iter()
        .flat_map(tool_context_entries)
        .collect::<Vec<_>>();
    let messages = session
        .messages
        .iter()
        .map(|message| serde_json::to_value(message).unwrap_or(Value::Null))
        .collect::<Vec<_>>();

    SessionDetail {
        id: session.id,
        created_ms: session.created_ms,
        updated_ms: session.updated_ms,
        model: session.model,
        provider: session.provider,
        turns: session.messages.len() as u32,
        title,
        state: SessionRunState::Stored,
        resumable: true,
        active_requests: Vec::new(),
        pending_permissions: Vec::new(),
        transcript,
        tool_context,
        messages,
        workspace_root: session.workspace_root,
        cwd: session.cwd,
        git_branch: session.git_branch,
        git_commit: session.git_commit,
        client_type: session.client_type,
        // Resolved by the daemon's `enrich_session_detail` from
        // `workspace_root` (it owns the project store path); the agent layer
        // has no project index, so it leaves the binding unresolved here.
        owning_project: None,
    }
}

fn sync_message_is_projectable(message: &Message) -> bool {
    let content = match message {
        Message::User { content, .. } => content,
        Message::Assistant(assistant) => &assistant.content,
        Message::ToolResult(_) => return false,
    };
    content.iter().any(|block| match block {
        Content::Text { text } => !text.is_empty(),
        Content::Image { .. } => true,
        _ => false,
    })
}

pub(crate) fn session_sync_snapshot(session: &Session) -> ocean_core::SessionSyncSnapshot {
    let visible_count = session
        .messages
        .iter()
        .filter(|message| sync_message_is_projectable(message))
        .count();
    let mut transcript = Vec::new();
    let mut used_text_bytes = 0usize;
    let mut truncated_text_bytes = 0u64;

    for message in session
        .messages
        .iter()
        .rev()
        .filter(|message| sync_message_is_projectable(message))
        .take(ocean_core::SESSION_SYNC_MAX_VISIBLE_MESSAGES)
    {
        let remaining =
            ocean_core::SESSION_SYNC_MAX_VISIBLE_TEXT_BYTES.saturating_sub(used_text_bytes);
        if remaining == 0 {
            break;
        }
        let (role, content, timestamp_ms, is_error) = match message {
            Message::User { content, timestamp } => ("user", content, Some(*timestamp), None),
            Message::Assistant(assistant) => (
                "assistant",
                &assistant.content,
                Some(assistant.timestamp),
                assistant.error_message.as_ref().map(|_| true),
            ),
            Message::ToolResult(_) => unreachable!("tool rows filtered above"),
        };
        let (mut text, omitted) = visible_text_tail(content, remaining);
        // The compaction/refresh sync transcript is a visible transcript clients
        // replace from, so it gets the same TASK-50 display strip as the detail
        // projection. Only the leading (front) prefix carries the injected block;
        // a row truncated from the front no longer opens with a marker, so the
        // strip is a byte-for-byte no-op there.
        if role == "user" {
            text = strip_injected_turn_prep(&text).to_string();
        }
        if text.is_empty()
            && content
                .iter()
                .any(|block| matches!(block, Content::Image { .. }))
        {
            const IMAGE_MARKER: &str = "[image attachment]";
            text.push_str(&IMAGE_MARKER[..IMAGE_MARKER.len().min(remaining)]);
        }
        if text.is_empty() {
            continue;
        }
        used_text_bytes = used_text_bytes.saturating_add(text.len());
        truncated_text_bytes = truncated_text_bytes.saturating_add(omitted as u64);
        transcript.push(SessionTranscriptEntry {
            role: role.into(),
            timestamp_ms,
            text,
            images: Vec::new(),
            tool_call_id: None,
            tool_name: None,
            is_error,
        });
        if omitted > 0 {
            break;
        }
    }
    transcript.reverse();

    ocean_core::SessionSyncSnapshot {
        session_id: session.id,
        model: session.model.clone(),
        provider: session.provider.clone(),
        truncated_messages: visible_count.saturating_sub(transcript.len()) as u64,
        truncated_text_bytes,
        transcript,
    }
}

fn visible_text_tail(content: &[Content], max_bytes: usize) -> (String, usize) {
    let total = content.iter().fold(0usize, |total, block| match block {
        Content::Text { text } => total.saturating_add(text.len()),
        _ => total,
    });
    let mut skip = total.saturating_sub(max_bytes);
    let mut output = String::with_capacity(total.min(max_bytes));
    for block in content {
        let Content::Text { text } = block else {
            continue;
        };
        if skip >= text.len() {
            skip -= text.len();
            continue;
        }
        let mut start = skip;
        while start < text.len() && !text.is_char_boundary(start) {
            start += 1;
        }
        skip = 0;
        output.push_str(&text[start..]);
    }
    if output.len() > max_bytes {
        let mut start = output.len() - max_bytes;
        while start < output.len() && !output.is_char_boundary(start) {
            start += 1;
        }
        output = output[start..].to_string();
    }
    let omitted = total.saturating_sub(output.len());
    (output, omitted)
}

pub(super) fn transcript_entry(message: &Message) -> SessionTranscriptEntry {
    match message {
        Message::User { content, timestamp } => {
            // Persisted user bodies carry the fully composed prompt (surface flag
            // + injected steering blocks + the user's words); render only the
            // user's words (TASK-50). Applied to every user turn, not just the
            // first — the advisory is injected per turn.
            let composed = text_from_content(content);
            SessionTranscriptEntry {
                role: "user".into(),
                timestamp_ms: Some(*timestamp),
                text: strip_injected_turn_prep(&composed).to_string(),
                images: images_from_content(content),
                tool_call_id: None,
                tool_name: None,
                is_error: None,
            }
        }
        Message::Assistant(assistant) => SessionTranscriptEntry {
            role: "assistant".into(),
            timestamp_ms: Some(assistant.timestamp),
            text: text_from_content(&assistant.content),
            images: images_from_content(&assistant.content),
            tool_call_id: None,
            tool_name: None,
            is_error: assistant.error_message.as_ref().map(|_| true),
        },
        Message::ToolResult(tool) => SessionTranscriptEntry {
            role: "tool".into(),
            timestamp_ms: Some(tool.timestamp),
            text: text_from_content(&tool.content),
            images: images_from_content(&tool.content),
            tool_call_id: Some(tool.tool_call_id.clone()),
            tool_name: Some(tool.tool_name.clone()),
            is_error: Some(tool.is_error),
        },
    }
}

fn tool_context_entries(message: &Message) -> Vec<SessionToolContext> {
    match message {
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .filter_map(|content| match content {
                Content::ToolCall {
                    id,
                    name,
                    arguments,
                } => Some(SessionToolContext {
                    kind: "call".into(),
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    arguments: Some(arguments.clone()),
                    is_error: None,
                    text: String::new(),
                }),
                _ => None,
            })
            .collect(),
        Message::ToolResult(tool) => vec![SessionToolContext {
            kind: "result".into(),
            tool_call_id: tool.tool_call_id.clone(),
            tool_name: tool.tool_name.clone(),
            arguments: None,
            is_error: Some(tool.is_error),
            text: text_from_content(&tool.content),
        }],
        Message::User { .. } => Vec::new(),
    }
}

/// Public/display transcript text. Provider reasoning remains in raw persisted
/// messages for same-provider replay, but is never projected to clients or
/// indexed by history search.
fn text_from_content(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Project `Content::Image` blocks to lightweight `ImageMeta` (mime_type
/// only — never the base64 `data`). `text_from_content` drops Image blocks,
/// so without this an image-bearing turn would render as empty text and a
/// replaying client would lose all evidence an image was attached
/// (OCEAN-177). The raw bytes remain in `SessionDetail::messages`.
fn images_from_content(content: &[Content]) -> Vec<ImageMeta> {
    content
        .iter()
        .filter_map(|content| match content {
            Content::Image { mime_type, .. } => Some(ImageMeta {
                mime_type: mime_type.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn truncate_title(text: &str) -> String {
    let squashed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if squashed.chars().count() <= 70 {
        squashed
    } else {
        format!("{}…", squashed.chars().take(70).collect::<String>())
    }
}

#[cfg(test)]
mod history_search_tests {
    use super::*;

    fn stored_session(messages: Vec<Message>) -> Session {
        Session {
            id: SessionId::new_v4(),
            created_ms: 10,
            updated_ms: 100,
            model: "fake".into(),
            provider: "fake".into(),
            messages,
            workspace_root: Some("/tmp/history-workspace".into()),
            cwd: Some("/tmp/history-workspace".into()),
            git_branch: None,
            git_commit: None,
            client_type: None,
            title: None,
        }
    }

    fn assistant_text(text: &str, timestamp: i64) -> Message {
        Message::Assistant(AssistantMessage {
            content: vec![Content::text(text)],
            api: "messages".into(),
            provider: "fake".into(),
            model: "fake".into(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp,
        })
    }

    #[test]
    fn history_search_store_budget_rejects_before_deserialization() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first.json");
        let second = tmp.path().join("second.json");
        std::fs::write(&first, "123456").unwrap();
        std::fs::write(&second, "abcdef").unwrap();

        let error = enforce_history_search_store_budget(&[first, second], 10).unwrap_err();
        let capacity = error
            .downcast_ref::<HistorySearchCapacityError>()
            .expect("typed capacity error");
        assert_eq!(capacity.observed_bytes, 12);
        assert_eq!(capacity.max_bytes, 10);
    }

    #[test]
    fn history_search_bounded_read_catches_growth_after_preflight() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first.json");
        let second = tmp.path().join("second.json");
        std::fs::write(&first, "1234").unwrap();
        std::fs::write(&second, "abcd").unwrap();
        let paths = vec![first, second.clone()];
        enforce_history_search_store_budget(&paths, 10).unwrap();

        // Simulate an atomic replacement or growth between metadata preflight
        // and the later open/read pass.
        std::fs::write(&second, "abcdefgh").unwrap();
        let mut observed = 0;
        assert!(read_history_file_bounded(&paths[0], &mut observed, 10)
            .unwrap()
            .is_some());
        let error = read_history_file_bounded(&paths[1], &mut observed, 10).unwrap_err();
        let capacity = error
            .downcast_ref::<HistorySearchCapacityError>()
            .expect("typed capacity error");
        assert_eq!(capacity.observed_bytes, 11);
        assert_eq!(capacity.max_bytes, 10);
    }

    #[test]
    fn history_search_ranks_display_text_and_excludes_tool_payloads() {
        let tmp = tempfile::tempdir().unwrap();
        let mut lexical = Message::user_text("Ocean is calm today; the blue horizon is clear.");
        if let Message::User { timestamp, .. } = &mut lexical {
            *timestamp = 150;
        }
        let mut assistant = assistant_text("The blue ocean deployment is ready.", 200);
        if let Message::Assistant(message) = &mut assistant {
            message.content.push(Content::Thinking {
                thinking: "private reasoning sentinel must never be searched".into(),
                thinking_signature: None,
            });
        }
        let session = stored_session(vec![
            lexical,
            assistant,
            Message::ToolResult(ocean_protocol::ToolResultMessage {
                tool_call_id: "secret-call".into(),
                tool_name: "provider_payload".into(),
                content: vec![Content::text("blue ocean must never be searched")],
                is_error: false,
                timestamp: 300,
            }),
        ]);
        save(tmp.path(), &session).unwrap();

        let hits = search_history(tmp.path(), "blue ocean", 20).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].match_kind, HistoryMatchKind::Exact);
        assert_eq!(hits[0].role, "assistant");
        assert_eq!(hits[1].match_kind, HistoryMatchKind::Lexical);
        assert!(hits
            .iter()
            .all(|hit| !hit.excerpt.contains("never be searched")));
        assert!(hits
            .iter()
            .all(|hit| hit.hit_id.starts_with(&session.id.to_string())));
        assert!(
            search_history(tmp.path(), "private reasoning sentinel", 20)
                .unwrap()
                .is_empty(),
            "provider thinking must not enter history search"
        );
    }

    #[test]
    fn history_hit_serializes_null_workspace_for_legacy_sessions() {
        let hit = HistorySearchHit {
            hit_id: "session:0".into(),
            session_id: SessionId::new_v4(),
            session_title: "Legacy".into(),
            role: "user".into(),
            excerpt: "remember this".into(),
            timestamp_ms: None,
            workspace_root: None,
            score: 3.0,
            match_kind: HistoryMatchKind::Exact,
        };
        let value = serde_json::to_value(hit).unwrap();
        assert!(value.get("workspace_root").is_some());
        assert!(value["workspace_root"].is_null());
    }

    #[test]
    fn history_search_fuzzy_is_deterministic_limited_and_unicode_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let long_text = format!(
            "{} blazing nightly cargo hold {}",
            "🌊".repeat(300),
            "🌊".repeat(300)
        );
        let session = stored_session(vec![Message::user_text(long_text)]);
        save(tmp.path(), &session).unwrap();
        let legacy = sessions_dir(tmp.path()).join("legacy");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("malformed.json"), "not json").unwrap();

        let first = search_history(tmp.path(), "blnch", 1).unwrap();
        let second = search_history(tmp.path(), "blnch", 1).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].match_kind, HistoryMatchKind::Fuzzy);
        assert!(first[0].excerpt.chars().count() <= HISTORY_EXCERPT_CHARS);
        assert!(first[0].excerpt.contains("blazing nightly cargo h"));
        assert!(first[0].excerpt.starts_with('…'));
        assert!(first[0].excerpt.ends_with('…'));
    }
}

#[cfg(test)]
mod title_tests {
    use super::*;

    /// The first byte-for-byte prefix of the daemon's real Longhouse advisory
    /// block (see `render_longhouse_prep` in ocean-daemon). The strip fallback
    /// anchors on this marker, so the test uses the real text.
    const LONGHOUSE_MARKER: &str =
        "Longhouse consult (advisory — relevant skills/SOPs for this turn; \
         recommendations only, not granted capabilities; you still route every \
         action through the normal permission gates):";

    fn session_with(title: Option<&str>, first_user: &str) -> Session {
        let mut s = Session::new_with_id(
            SessionId::new_v4(),
            &ocean_protocol::Model::anthropic_claude_sonnet_4_6(),
        );
        s.title = title.map(str::to_string);
        s.messages = vec![
            Message::user_text(first_user),
            Message::Assistant(AssistantMessage {
                content: vec![Content::text("ok")],
                api: "messages".into(),
                provider: "fake".into(),
                model: "fake".into(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 1,
            }),
        ];
        s
    }

    /// Prompt-path shape: the persisted first message is the composed prompt
    /// (surface flag + Longhouse advisory + the user's words), but the explicit
    /// `title` was captured from the ORIGINAL prompt, so the label is the user's
    /// words — not the injected boilerplate every session would otherwise share.
    #[test]
    fn explicit_title_wins_over_longhouse_composed_first_message() {
        let composed = format!("[WEB] {LONGHOUSE_MARKER}\n- skill\n\nfix the parser bug");
        let s = session_with(Some("fix the parser bug"), &composed);
        assert_eq!(session_display_title(&s), "fix the parser bug");
    }

    /// agent_turn-path shape: the composed prompt stacks room/operator guidance
    /// AND the Longhouse advisory above the user's words. The explicit title
    /// (original prompt) still wins, proving the fix is layer-agnostic — guidance
    /// layering has the same prepend shape and would pollute the title too.
    #[test]
    fn explicit_title_wins_over_guidance_and_longhouse_layers() {
        let composed = format!(
            "[TUI] Room guidance: keep replies terse.\n\n{LONGHOUSE_MARKER}\n- skill\n\nship the release",
        );
        let s = session_with(Some("ship the release"), &composed);
        assert_eq!(session_display_title(&s), "ship the release");
    }

    /// Legacy fallback (write-time case): sessions created before the `title`
    /// field carry `None`, so the label derives from the composed first message.
    /// The surface flag and the leading Longhouse advisory block are stripped to
    /// recover the user's words, retroactively cleaning polluted legacy titles.
    #[test]
    fn legacy_session_strips_surface_flag_and_longhouse_block() {
        let composed =
            format!("[WEB] {LONGHOUSE_MARKER}\n- skill-a\n- skill-b\n\nfix the parser bug");
        let s = session_with(None, &composed);
        assert_eq!(session_display_title(&s), "fix the parser bug");
    }

    /// A legacy first message with only the surface flag (no Longhouse block —
    /// consult opted out or empty) still drops the flag to the user's words.
    #[test]
    fn legacy_session_strips_surface_flag_without_longhouse() {
        let s = session_with(None, "[TUI] just a normal prompt");
        assert_eq!(session_display_title(&s), "just a normal prompt");
    }

    /// A user prompt that merely opens with a bracketed non-flag token (mixed
    /// case, not a surface flag) is left intact — the strip only removes an
    /// all-uppercase flag tag.
    #[test]
    fn non_flag_bracket_prefix_is_preserved() {
        let s = session_with(None, "[TODO] wire up the button");
        assert_eq!(session_display_title(&s), "[TODO] wire up the button");
    }

    /// `ensure_title` captures the first turn's prompt and never relabels on
    /// later turns; blank input is ignored so an empty turn can't erase it.
    #[test]
    fn ensure_title_first_write_wins_and_ignores_blank() {
        let mut s = Session::new_with_id(
            SessionId::new_v4(),
            &ocean_protocol::Model::anthropic_claude_sonnet_4_6(),
        );
        assert_eq!(s.title, None);
        s.ensure_title("   ");
        assert_eq!(s.title, None, "blank input must not set a title");
        s.ensure_title("original prompt");
        assert_eq!(s.title.as_deref(), Some("original prompt"));
        s.ensure_title("a later turn's prompt");
        assert_eq!(
            s.title.as_deref(),
            Some("original prompt"),
            "first write wins; resumes never relabel"
        );
    }

    /// `ensure_title` stores the truncated/whitespace-squashed form so the
    /// persisted label matches what the read side renders.
    #[test]
    fn ensure_title_truncates_like_the_read_side() {
        let long = "word ".repeat(40);
        let mut s = Session::new_with_id(
            SessionId::new_v4(),
            &ocean_protocol::Model::anthropic_claude_sonnet_4_6(),
        );
        s.ensure_title(&long);
        let expected = truncate_title(&long);
        assert_eq!(s.title.as_deref(), Some(expected.as_str()));
        assert!(expected.ends_with('…'));
    }
}

/// TASK-50: the persisted user message body carries the fully composed prompt the
/// model saw (surface flag + injected steering blocks + the user's words). The
/// read-side transcript projections must render the user's words alone — the
/// injected advisory/guidance preamble must not leak into product user bubbles.
#[cfg(test)]
mod body_strip_tests {
    use super::*;

    /// The real daemon Longhouse advisory header (see `render_longhouse_prep`);
    /// the body strip anchors on its `Longhouse consult (advisory` prefix.
    const LONGHOUSE_MARKER: &str =
        "Longhouse consult (advisory — relevant skills/SOPs for this turn; \
         recommendations only, not granted capabilities; you still route every \
         action through the normal permission gates):";
    /// The real daemon operator-guidance header (see `render_turn_guidance`).
    const GUIDANCE_MARKER: &str = "Operator guidance for this turn:";

    fn user_and_reply(bodies: &[&str]) -> Session {
        let mut s = Session::new_with_id(
            SessionId::new_v4(),
            &ocean_protocol::Model::anthropic_claude_sonnet_4_6(),
        );
        let mut messages = Vec::new();
        for body in bodies {
            messages.push(Message::user_text(*body));
            messages.push(Message::Assistant(AssistantMessage {
                content: vec![Content::text("ok")],
                api: "messages".into(),
                provider: "fake".into(),
                model: "fake".into(),
                usage: Usage::default(),
                stop_reason: StopReason::Stop,
                error_message: None,
                timestamp: 1,
            }));
        }
        s.messages = messages;
        s
    }

    fn user_texts(detail: &SessionDetail) -> Vec<String> {
        detail
            .transcript
            .iter()
            .filter(|e| e.role == "user")
            .map(|e| e.text.clone())
            .collect()
    }

    /// The reported bug: a real composed body (surface flag + Longhouse advisory
    /// + the user's words) renders in the detail projection as the user's words
    /// alone. Reaching the advisory requires the flag strip first (the advisory
    /// is never leading), exactly as the session-label derivation does.
    #[test]
    fn detail_transcript_strips_flag_and_longhouse_from_user_body() {
        let composed =
            format!("[WEB] {LONGHOUSE_MARKER}\n- some-skill\n\nfix the flaky parser test");
        let detail = session_detail(user_and_reply(&[&composed]));
        assert_eq!(user_texts(&detail), vec!["fix the flaky parser test"]);
    }

    /// The advisory is injected on EVERY turn, so every user body is stripped,
    /// not just the first.
    #[test]
    fn detail_transcript_strips_every_user_turn() {
        let first = format!("[WEB] {LONGHOUSE_MARKER}\n- s\n\nfirst question");
        let second = format!("[WEB] {LONGHOUSE_MARKER}\n- s\n\nsecond question");
        let detail = session_detail(user_and_reply(&[&first, &second]));
        assert_eq!(
            user_texts(&detail),
            vec!["first question", "second question"]
        );
    }

    /// A body with no injected prefix at all is byte-identical (no false strips).
    #[test]
    fn detail_transcript_leaves_unadorned_body_byte_identical() {
        let plain = "just fix the bug, there is no advisory or flag here";
        let detail = session_detail(user_and_reply(&[plain]));
        assert_eq!(user_texts(&detail), vec![plain]);
    }

    /// Exact-prefix anchoring: genuine user text that merely MENTIONS the
    /// advisory marker mid-line (e.g. asking about a log line) is not mangled —
    /// only a body that OPENS with the marker (after the flag) is stripped.
    #[test]
    fn detail_transcript_preserves_midline_marker_mention() {
        let body = "[WEB] why does the log say \"Longhouse consult (advisory\" on cold start?";
        let detail = session_detail(user_and_reply(&[body]));
        // The flag is peeled (injected metadata), but the user's words — marker
        // mention and all — survive intact.
        assert_eq!(
            user_texts(&detail),
            vec!["why does the log say \"Longhouse consult (advisory\" on cold start?"]
        );
    }

    /// A flag-only body (consult opted out / empty) still drops the flag to the
    /// user's words.
    #[test]
    fn strip_peels_flag_only_body() {
        assert_eq!(
            strip_injected_turn_prep("[TUI] just a normal prompt"),
            "just a normal prompt"
        );
    }

    /// The operator-guidance block is marker-anchored and cleanly `\n\n`-bounded,
    /// so a standalone guidance body strips to the user's words.
    #[test]
    fn strip_peels_operator_guidance_block() {
        let body = format!("[TUI] {GUIDANCE_MARKER}\n- be terse\n\nship the release");
        assert_eq!(strip_injected_turn_prep(&body), "ship the release");
    }

    /// Stacked layers peel in one call: flag, then the Longhouse advisory, then
    /// the operator-guidance block (their real agent_turn stacking order), down
    /// to the user's words.
    #[test]
    fn strip_peels_stacked_flag_longhouse_and_guidance() {
        let body = format!(
            "[WEB] {LONGHOUSE_MARKER}\n- skill-a\n\n{GUIDANCE_MARKER}\n- terse\n\nland the migration"
        );
        assert_eq!(strip_injected_turn_prep(&body), "land the migration");
    }

    /// A body with no known marker is returned byte-for-byte, including one that
    /// opens with a NON-flag bracket token (`[TODO] …`).
    #[test]
    fn strip_leaves_non_marker_bodies_unchanged() {
        assert_eq!(
            strip_injected_turn_prep("[TODO] wire up the button"),
            "[TODO] wire up the button"
        );
        assert_eq!(strip_injected_turn_prep("plain words"), "plain words");
    }

    /// DEFERRED layers stay intact (documented limitation): a folder-as-agent
    /// instruction prefix (no marker) and a `## Browser context` block (internal
    /// blank line) are not stripped, and when a browser block leads it shadows
    /// the inner Longhouse block too. This pins the deferral so a future change
    /// that starts stripping them is a conscious one.
    #[test]
    fn strip_defers_unmarked_and_multiparagraph_layers() {
        let folder_agent = "[TUI] You are the deploy agent. Follow the runbook.\n\nship it";
        assert_eq!(
            strip_injected_turn_prep(folder_agent),
            folder_agent.trim_start_matches("[TUI] ")
        );
        let browser = format!(
            "[TAURI] ## Browser context\n\nThe operator's browser surface reported this live state:\n\
             - Active tab: https://x (X)\n\n{LONGHOUSE_MARKER}\n- s\n\ndo the thing"
        );
        // Flag peeled; the browser block (and the Longhouse block it shadows)
        // remain — the strip stops at the multi-paragraph browser block.
        assert!(strip_injected_turn_prep(&browser).starts_with("## Browser context"));
    }
}
