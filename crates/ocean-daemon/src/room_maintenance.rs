//! Room maintenance: the two sweeps that stop `rooms.db` and the attachment
//! blob tree from growing without bound.
//!
//! Durable rooms have always been append-only in practice. A transcript row is
//! written and never removed; an attachment's bytes are written, fsynced, and
//! removed only when somebody explicitly deletes the row that names them.
//! `room_attachments.rs` has said since it was written that a crash between the
//! blob write and the row commit "leaves it for a future GC sweep" — this is
//! that sweep, and the retention half beside it.
//!
//! Two jobs, one loop:
//!
//! 1. **Transcript retention.** A room CLOSED longer than the operator's window
//!    loses its transcript, its attachment rows and blobs, its read cursors and
//!    its federation dedup index, in one IMMEDIATE store transaction per room.
//!    Never an open room, at any age: the window is measured from the close, so
//!    a live room is not eligible however long it has been running. Off unless
//!    the operator turns it on — see [`DEFAULT_ROOM_RETENTION_DAYS`].
//! 2. **Attachment orphan GC.** Bytes on disk that no `room_attachments` row
//!    claims. This is the residue the upload path's deliberate write-then-commit
//!    order produces, plus whatever a retention cut left behind if the daemon
//!    died between the commit and the unlink.
//!
//! They share one loop because they share one shape — walk the store, touch the
//! filesystem, report — and because two independent timers over the same lock
//! and the same directory tree buy nothing but a way for them to interleave.
//!
//! **The report is the point of the operator half.** A sweep nobody can see is
//! indistinguishable from a sweep that is not running, and the failure this
//! module is most likely to have (a window set to the wrong unit, a blob root
//! that moved, a permissions error on one room's directory) is silent by
//! nature: disk simply keeps growing. So every run writes one `tracing::info!`
//! line and updates the `room_maintenance` card on `GET /health`, and the card
//! carries the CONFIGURATION as well as the counts — an operator reading
//! "retention_days: 0" learns why nothing was cut without going to the process
//! environment to find out.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{extract::State, http::HeaderMap, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use ocean_core::RoomKey;
use serde_json::json;

use crate::persistent_rooms::{with_rooms_handle, RoomStoreHandle};
use crate::AppState;

/// Environment variable naming the transcript retention window, in days.
pub(super) const RETENTION_DAYS_ENV: &str = "OCEAN_ROOM_RETENTION_DAYS";

/// The retention window a daemon that names none of its own runs with: **zero,
/// and zero means never**.
///
/// This is the one default in this module chosen for what it must not do rather
/// than for what it does. A transcript cut is unrecoverable — the rows are gone,
/// there is no tombstone holding the bodies, and the blobs are unlinked — and
/// the population that would inherit a nonzero default is every daemon that
/// upgrades into this code with rooms already in its store. A daemon that starts
/// deleting a member's history because it was restarted is a data-loss incident,
/// not a feature landing, and no window short enough to be useful is also short
/// enough to be safe to impose on somebody who never asked for it.
///
/// So retention is opt-in: unset or `0` keeps every closed room forever, exactly
/// as today. The number an operator who wants it should reach for is in the
/// operator guide beside `OCEAN_DB_PATH`, where it can be argued in prose rather
/// than imposed by a constant.
pub(super) const DEFAULT_ROOM_RETENTION_DAYS: u32 = 0;

/// How often the maintenance loop sweeps.
///
/// Six hours. Both jobs are bounded by how much the store grew since the last
/// run rather than by its size, so the interval is not a performance knob; what
/// it actually sets is the worst-case lag between an operator's window elapsing
/// and the disk coming back. Four sweeps a day makes "cut within a day of the
/// window" true without a sweep ever being the thing an operator waits on, and
/// keeps the orphan GC's directory walk — the only part that touches every room
/// — off the daemon's hot hours by not running it every few minutes.
pub(super) const ROOM_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// How new a blob has to be for the orphan sweep to leave it alone.
///
/// One hour, and it is a correctness bound rather than caution. `room_attachments`
/// writes and fsyncs the bytes BEFORE the row commits — deliberately, because an
/// orphan blob is collectable garbage while an orphan row is a download that
/// 500s forever — so there is a window in every successful upload during which
/// the file on disk is genuinely unreferenced. A sweep with no grace would race
/// that window and delete the bytes of an upload that was about to succeed,
/// leaving exactly the orphan row the write order exists to prevent.
///
/// An hour is far longer than that window (the row commits within one store
/// transaction of the write) and far longer than the 8 MiB upload that produced
/// it can take. It also covers the `.tmp` file `write_blob` renames from: a
/// crash mid-upload leaves one, and an hour is long enough that a live upload's
/// temp file is never mistaken for that crash's.
pub(super) const ATTACHMENT_ORPHAN_GRACE: Duration = Duration::from_secs(60 * 60);

/// Read the retention window once, at startup.
///
/// Once because the whole daemon reads its environment once: a value re-read per
/// sweep could change under a running loop, and a retention window that changes
/// without a restart is a window nobody can reason about from `/health`.
///
/// `0`, unset, empty, and anything that does not parse as a non-negative integer
/// all mean **never**. Refusing to guess is the point — a typo in a variable
/// whose job is deleting transcripts must not become a shorter window than the
/// operator wrote, and the one safe direction to fail is "keep everything". A
/// value that was present and unusable is logged at `warn`, because silence
/// there would let an operator believe retention was on.
pub(super) fn retention_days_from_env() -> u32 {
    match std::env::var(RETENTION_DAYS_ENV) {
        Err(_) => DEFAULT_ROOM_RETENTION_DAYS,
        Ok(raw) => parse_retention_days(&raw),
    }
}

/// The parse [`retention_days_from_env`] applies, split out so it is testable
/// without writing process environment.
fn parse_retention_days(raw: &str) -> u32 {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return DEFAULT_ROOM_RETENTION_DAYS;
    }
    match trimmed.parse::<u32>() {
        Ok(days) => days,
        Err(_) => {
            tracing::warn!(
                var = RETENTION_DAYS_ENV,
                "room retention window is not a non-negative integer number of days; \
                 retention stays OFF rather than guessing a shorter window"
            );
            DEFAULT_ROOM_RETENTION_DAYS
        }
    }
}

/// Everything one sweep needs to know, resolved at startup and never re-read.
#[derive(Debug, Clone, Copy)]
pub(super) struct MaintenanceConfig {
    /// Days a room may stay closed before its transcript is cut. `0` = never.
    pub(super) retention_days: u32,
    /// How new a blob has to be to survive the orphan sweep.
    pub(super) orphan_grace: Duration,
    /// How often the loop runs.
    pub(super) interval: Duration,
}

impl Default for MaintenanceConfig {
    /// The policy with retention OFF and the module's own grace and interval.
    ///
    /// This is what every test daemon gets, and it is the right thing for one to
    /// get: a fixture that inherited a live retention window would delete its
    /// own closed rooms out from under whatever it was actually asserting. The
    /// retention tests build their window explicitly and drive
    /// [`run_sweep`] directly with a fixed clock.
    fn default() -> Self {
        Self {
            retention_days: DEFAULT_ROOM_RETENTION_DAYS,
            orphan_grace: ATTACHMENT_ORPHAN_GRACE,
            interval: ROOM_MAINTENANCE_INTERVAL,
        }
    }
}

impl MaintenanceConfig {
    pub(super) fn from_env() -> Self {
        Self {
            retention_days: retention_days_from_env(),
            ..Self::default()
        }
    }
}

/// What one sweep did, plus the configuration it did it under.
///
/// Serialized wholesale as the `room_maintenance` object on `GET /health`. Every
/// field is a count, a duration, or a fixed error string — no room key, no
/// filename, no transcript body — because this card is scraped and logged, and
/// a maintenance report is not a place to publish the content it just deleted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(super) struct RoomMaintenanceReport {
    /// Sweep interval in seconds. Fixed at startup.
    pub(super) interval_secs: u64,
    /// Configured retention window in days. `0` means retention is off, which
    /// is why this is reported even though it never changes: a card showing
    /// zero rooms cut is ambiguous until you can see whether cutting is on.
    pub(super) retention_days: u32,
    /// Orphan grace window in seconds. Fixed at startup.
    pub(super) orphan_grace_secs: u64,
    /// When the last sweep finished. `None` before the first one.
    pub(super) last_run_at: Option<String>,
    /// How long the last sweep took.
    pub(super) last_run_ms: u64,
    /// Sweeps completed since daemon start, successful or not.
    pub(super) runs_total: u64,
    /// Rooms whose transcript was cut by the last sweep.
    pub(super) rooms_cut: u64,
    /// Transcript rows the last sweep removed.
    pub(super) messages_removed: u64,
    /// `room_attachments` rows the last sweep removed.
    pub(super) attachment_rows_removed: u64,
    /// Read-cursor rows (local + mirrored) the last sweep removed.
    pub(super) cursors_removed: u64,
    /// `federated_events` index rows the last sweep removed.
    pub(super) federated_index_rows_removed: u64,
    /// Unreferenced blob FILES the last sweep unlinked.
    pub(super) orphan_files_removed: u64,
    /// Whole room DIRECTORIES matching no room the store knows, removed by the
    /// last sweep.
    pub(super) orphan_dirs_removed: u64,
    /// Blob unlinks that FAILED in the last sweep, across both jobs.
    ///
    /// Its own counter rather than only an `last_error` string, because this is
    /// the one failure here that is invisible by construction: the rows commit,
    /// every count looks healthy, and disk simply never comes back. A nonzero
    /// value with `bytes_reclaimed` short of what was cut is the signature of a
    /// blob tree the daemon can read but not write.
    pub(super) blobs_unlink_failed: u64,
    /// Bytes the last sweep ACTUALLY reclaimed — a blob counts only once its
    /// file is gone, never merely because its row was deleted.
    pub(super) bytes_reclaimed: u64,
    /// The last sweep's error, or `None` if it was clean. A fixed, bounded
    /// string — never a path, a room key, or a store message that could carry
    /// one. It is deliberately NOT cleared by a later clean run's absence of
    /// errors being reported elsewhere: a clean sweep sets it to `None`, so a
    /// non-null value always describes the most recent sweep.
    pub(super) last_error: Option<String>,
}

impl RoomMaintenanceReport {
    fn new(config: &MaintenanceConfig) -> Self {
        Self {
            interval_secs: config.interval.as_secs(),
            retention_days: config.retention_days,
            orphan_grace_secs: config.orphan_grace.as_secs(),
            last_run_at: None,
            last_run_ms: 0,
            runs_total: 0,
            rooms_cut: 0,
            messages_removed: 0,
            attachment_rows_removed: 0,
            cursors_removed: 0,
            federated_index_rows_removed: 0,
            orphan_files_removed: 0,
            orphan_dirs_removed: 0,
            blobs_unlink_failed: 0,
            bytes_reclaimed: 0,
            last_error: None,
        }
    }
}

/// The live report, shared between the sweep loop, the on-demand route, and
/// `/health`.
pub(super) type MaintenanceHandle = Arc<Mutex<RoomMaintenanceReport>>;

/// Build the shared report in its pre-first-sweep state.
pub(super) fn new_handle(config: &MaintenanceConfig) -> MaintenanceHandle {
    Arc::new(Mutex::new(RoomMaintenanceReport::new(config)))
}

/// Read the current report, recovering a poisoned lock the way every other
/// registry in this daemon does. A poisoned maintenance mutex must never take
/// `/health` down with it — the card exists to make failure visible.
pub(super) fn report_snapshot(handle: &MaintenanceHandle) -> RoomMaintenanceReport {
    match handle.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// The counts one sweep produced, before they are folded into the report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct SweepOutcome {
    pub(super) rooms_cut: u64,
    pub(super) messages_removed: u64,
    pub(super) attachment_rows_removed: u64,
    pub(super) cursors_removed: u64,
    pub(super) federated_index_rows_removed: u64,
    pub(super) orphan_files_removed: u64,
    pub(super) orphan_dirs_removed: u64,
    pub(super) blobs_unlink_failed: u64,
    pub(super) bytes_reclaimed: u64,
    pub(super) error: Option<String>,
}

// ---- The sweep --------------------------------------------------------------

/// Run both jobs once.
///
/// Synchronous and blocking: it takes the store lock repeatedly and walks a
/// directory tree, so callers put it on a blocking thread rather than a runtime
/// worker. It never holds the store guard across filesystem work — every
/// `with_rooms_handle` closure returns before a file is touched, which is the
/// same rule the rest of the room code follows about awaits.
///
/// `now` is a parameter and not `Utc::now()` so retention is testable against a
/// fixed clock: a test that had to sleep past a real window could only ever
/// exercise a window of zero days, which is the one value that means "off".
pub(super) fn run_sweep(
    rooms: &RoomStoreHandle,
    blob_root: &Path,
    config: &MaintenanceConfig,
    now: DateTime<Utc>,
) -> SweepOutcome {
    let mut outcome = SweepOutcome::default();
    if let Err(error) = run_retention(rooms, blob_root, config, now, &mut outcome) {
        outcome.error = Some(error);
    }
    if let Err(error) = run_orphan_gc(rooms, blob_root, config, &mut outcome) {
        // First error wins: the retention failure is the more consequential of
        // the two to report, and a card that can hold one string should hold
        // the one an operator acts on.
        outcome.error.get_or_insert(error);
    }
    outcome
}

/// Cut every room closed longer than the window.
fn run_retention(
    rooms: &RoomStoreHandle,
    blob_root: &Path,
    config: &MaintenanceConfig,
    now: DateTime<Utc>,
    outcome: &mut SweepOutcome,
) -> Result<(), String> {
    if config.retention_days == 0 {
        return Ok(());
    }
    let Some(window) = chrono::Duration::try_days(i64::from(config.retention_days)) else {
        return Err("retention window does not fit a duration".to_string());
    };
    let cutoff = now - window;
    let eligible = with_rooms_handle(rooms, |store| store.rooms_closed_before(cutoff))
        .map_err(|_| "retention could not list closed rooms".to_string())?;

    for key in eligible {
        // One transaction per room, not one for all of them. A single sweeping
        // transaction would hold the write lock across every room's deletes and
        // block live traffic for the whole sweep, and a failure on room 40 would
        // roll back the 39 cuts that were already correct.
        let cut = match with_rooms_handle(rooms, |store| store.cut_closed_room(&key)) {
            Ok(cut) => cut,
            Err(_) => {
                // Deliberately not the store's message: it names the room, and
                // this string is published on `/health`.
                outcome
                    .error
                    .get_or_insert_with(|| "retention failed to cut a closed room".to_string());
                continue;
            }
        };
        outcome.rooms_cut += 1;
        outcome.messages_removed += cut.messages_removed;
        outcome.attachment_rows_removed += cut.attachment_rows_removed;
        outcome.cursors_removed += cut.cursors_removed;
        outcome.federated_index_rows_removed += cut.federated_index_rows_removed;

        // Bytes AFTER the commit, exactly as `DELETE .../attachments/{id}` does
        // it. A blob whose unlink fails is still collectable — the row is gone,
        // so the orphan pass below sees an unreferenced file in a directory the
        // store still expects — but "a later sweep gets it" is not a reason to
        // discard the error, and the byte total is not allowed to claim it.
        //
        // `bytes_reclaimed` counts a blob only when the file is actually gone
        // (unlinked here, or already absent). Adding the row's recorded
        // `byte_len` unconditionally would let the report announce reclaimed
        // disk on a tree that never gave any back, which is the exact failure
        // this card exists to make visible rather than to paper over.
        let dir = crate::room_attachments::room_dir(blob_root, &key);
        for (id, byte_len) in cut.attachment_blobs {
            let Some(path) = crate::room_attachments::blob_path(blob_root, &key, &id) else {
                outcome.blobs_unlink_failed += 1;
                outcome.error.get_or_insert_with(|| {
                    "retention refused a malformed stored attachment id".to_string()
                });
                continue;
            };
            match std::fs::remove_file(&path) {
                Ok(()) => outcome.bytes_reclaimed += byte_len,
                // Already gone: nothing to reclaim, and nothing wrong.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    outcome.blobs_unlink_failed += 1;
                    outcome.error.get_or_insert_with(|| {
                        "retention could not unlink a cut room's attachment bytes".to_string()
                    });
                }
            }
        }
        // The room's directory is empty now and no room will ever file anything
        // under it again. `remove_dir` (not `remove_dir_all`) so a file the
        // unlinks above failed to remove keeps the directory alive for the
        // orphan sweep to look at rather than being deleted unexamined.
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(())
}

/// Unlink blob bytes no `room_attachments` row claims.
fn run_orphan_gc(
    rooms: &RoomStoreHandle,
    blob_root: &Path,
    config: &MaintenanceConfig,
    outcome: &mut SweepOutcome,
) -> Result<(), String> {
    // The room directory is a ONE-WAY hash of the room key and is never stored,
    // so there is no way to read a directory name and learn whose it is. The
    // only question the tree can answer is the one asked in the other
    // direction: derive the expected directory of every room the store knows —
    // closed rooms included, because a frozen room still owns its files and
    // `/snapshot` still serves the transcript naming them — and anything the
    // tree holds that is not in that set belongs to nobody.
    let keys = with_rooms_handle(rooms, |store| store.room_keys_including_closed())
        .map_err(|_| "orphan GC could not list rooms".to_string())?;

    let mut expected: HashMap<PathBuf, RoomKey> = HashMap::new();
    for key in keys {
        expected.insert(crate::room_attachments::room_dir(blob_root, &key), key);
    }

    let entries = match std::fs::read_dir(blob_root) {
        Ok(entries) => entries,
        // No tree yet is the ordinary state of a daemon nobody has attached a
        // file to. Not an error, and not something to report every six hours.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("orphan GC could not read the attachment root".to_string()),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // Only directories are ours. A stray file at the root is left alone
        // rather than deleted: this sweep's authority is "the tree this daemon
        // writes", and this daemon writes only room directories here.
        if !file_type.is_dir() {
            continue;
        }
        match expected.get(&path) {
            None => {
                // A directory matching no room the store knows. Nothing can ever
                // reach it again — the only way back in is the hash of a key the
                // store no longer holds.
                if !older_than_grace(&path, config.orphan_grace) {
                    continue;
                }
                remove_orphan_dir_with(&path, outcome, |path| std::fs::remove_dir_all(path));
            }
            Some(key) => {
                let referenced = match with_rooms_handle(rooms, |store| store.attachments(key)) {
                    Ok(rows) => rows.into_iter().map(|row| row.id).collect::<HashSet<_>>(),
                    Err(_) => {
                        // Fail CLOSED: a room whose rows could not be read has
                        // no known references, and treating "unknown" as
                        // "unreferenced" would delete a live room's files on a
                        // transient store error.
                        outcome.error.get_or_insert_with(|| {
                            "orphan GC could not read a room's attachment rows".to_string()
                        });
                        continue;
                    }
                };
                sweep_room_dir(&path, &referenced, config.orphan_grace, outcome);
            }
        }
    }
    Ok(())
}

/// Remove one whole directory the store cannot name and attribute only bytes
/// the filesystem confirms are gone.
///
/// The remover is injected so the failure path has a deterministic regression:
/// permission fixtures are not reliable when the test runner is root.
fn remove_orphan_dir_with(
    path: &Path,
    outcome: &mut SweepOutcome,
    remove_dir_all: impl FnOnce(&Path) -> std::io::Result<()>,
) {
    let bytes = directory_bytes(path);
    match remove_dir_all(path) {
        Ok(()) => {
            outcome.orphan_dirs_removed += 1;
            outcome.bytes_reclaimed += bytes;
        }
        // Somebody else removed it between the listing and here. Nothing was
        // reclaimed by this sweep, but the intended state already holds.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            outcome.error.get_or_insert_with(|| {
                "orphan GC could not remove an unrecognized room directory".to_string()
            });
        }
    }
}

/// Unlink every file in one room's directory that its row set does not name.
///
/// This is where the `.tmp` residue goes too: `write_blob` renames from
/// `<id>.tmp`, which is not an attachment id, so a crash mid-upload leaves a
/// file no row can ever claim and the same rule collects it.
fn sweep_room_dir(
    dir: &Path,
    referenced: &HashSet<String>,
    grace: Duration,
    outcome: &mut SweepOutcome,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        outcome
            .error
            .get_or_insert_with(|| "orphan GC could not read a room directory".to_string());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_referenced(name, referenced) {
            continue;
        }
        if !older_than_grace(&path, grace) {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                outcome.orphan_files_removed += 1;
                outcome.bytes_reclaimed += size;
            }
            // Somebody else removed it between the listing and here. Not a
            // failure: the file is gone, which is what was wanted.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // A directory that refuses deletion — wrong permissions, a
            // read-only mount — is the failure mode this whole module exists to
            // make visible: disk keeps growing while every sweep reports a
            // clean run. Counted and surfaced, never swallowed.
            Err(_) => {
                outcome.blobs_unlink_failed += 1;
                outcome
                    .error
                    .get_or_insert_with(|| "orphan GC could not unlink a blob".to_string());
            }
        }
    }
}

/// Does a row claim the file under this name?
///
/// Its own function so the GC test can MUTATE exactly this decision — force it
/// to say referenced for everything, or for nothing — and watch the assertions
/// fall the two opposite ways. A sweep whose reference check is inlined into its
/// loop can only be tested by outcome, and an outcome test passes just as well
/// against a sweep that deletes nothing at all.
fn is_referenced(file_name: &str, referenced: &HashSet<String>) -> bool {
    referenced.contains(file_name)
}

/// Is this path older than the grace window?
///
/// Fail CLOSED on every uncertainty: an unreadable mtime, a clock that puts the
/// file in the future, an elapsed-time error — all answer `false`, meaning "do
/// not delete". The cost of a false negative is that a genuine orphan survives
/// until the next sweep; the cost of a false positive is somebody's file.
fn older_than_grace(path: &Path, grace: Duration) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    modified.elapsed().map(|age| age >= grace).unwrap_or(false)
}

/// Total bytes of the regular files directly inside a directory.
///
/// Used only to report what removing an orphan directory reclaimed. Non-
/// recursive because the tree is flat by construction: a room directory holds
/// blob files and nothing else.
fn directory_bytes(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .map(|metadata| metadata.len())
        .sum()
}

// ---- Loop + on-demand route -------------------------------------------------

/// Fold one sweep's counts into the shared report and log the line an operator
/// greps for.
fn record_sweep(
    handle: &MaintenanceHandle,
    outcome: &SweepOutcome,
    finished_at: DateTime<Utc>,
    elapsed: Duration,
) {
    let mut guard = match handle.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.last_run_at = Some(finished_at.to_rfc3339());
    guard.last_run_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
    guard.runs_total = guard.runs_total.saturating_add(1);
    guard.rooms_cut = outcome.rooms_cut;
    guard.messages_removed = outcome.messages_removed;
    guard.attachment_rows_removed = outcome.attachment_rows_removed;
    guard.cursors_removed = outcome.cursors_removed;
    guard.federated_index_rows_removed = outcome.federated_index_rows_removed;
    guard.orphan_files_removed = outcome.orphan_files_removed;
    guard.orphan_dirs_removed = outcome.orphan_dirs_removed;
    guard.blobs_unlink_failed = outcome.blobs_unlink_failed;
    guard.bytes_reclaimed = outcome.bytes_reclaimed;
    guard.last_error = outcome.error.clone();

    // One line per run, at info, with every number the card carries. The card
    // says what is true NOW; this line is the history, and it is what an
    // operator has when the question is "when did the disk actually come back".
    tracing::info!(
        rooms_cut = outcome.rooms_cut,
        messages_removed = outcome.messages_removed,
        attachment_rows_removed = outcome.attachment_rows_removed,
        cursors_removed = outcome.cursors_removed,
        federated_index_rows_removed = outcome.federated_index_rows_removed,
        orphan_files_removed = outcome.orphan_files_removed,
        orphan_dirs_removed = outcome.orphan_dirs_removed,
        blobs_unlink_failed = outcome.blobs_unlink_failed,
        bytes_reclaimed = outcome.bytes_reclaimed,
        retention_days = guard.retention_days,
        elapsed_ms = guard.last_run_ms,
        error = outcome.error.as_deref().unwrap_or("none"),
        "room maintenance sweep finished"
    );
}

/// [`record_sweep`] for a test that needs a card without running a sweep.
///
/// Exposed rather than letting `main.rs`'s health test build a
/// [`RoomMaintenanceReport`] by hand: a hand-built card would keep passing after
/// the real bookkeeping stopped filling a field, which is the only thing that
/// test is for.
#[cfg(test)]
pub(super) fn record_sweep_for_test(handle: &MaintenanceHandle, outcome: SweepOutcome) {
    record_sweep(handle, &outcome, Utc::now(), Duration::from_millis(1));
}

/// Run one sweep off the runtime workers and record it.
///
/// `spawn_blocking` because the sweep takes a `std::sync::Mutex` and walks a
/// directory tree; doing that on a runtime worker is how a large blob tree
/// becomes a stalled daemon.
async fn sweep_once(
    rooms: RoomStoreHandle,
    blob_root: Arc<PathBuf>,
    config: MaintenanceConfig,
    handle: MaintenanceHandle,
) {
    let started = std::time::Instant::now();
    let outcome = tokio::task::spawn_blocking(move || {
        run_sweep(&rooms, blob_root.as_path(), &config, Utc::now())
    })
    .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(join_error) => SweepOutcome {
            error: Some(if join_error.is_panic() {
                "room maintenance sweep panicked".to_string()
            } else {
                "room maintenance sweep was cancelled".to_string()
            }),
            ..SweepOutcome::default()
        },
    };
    record_sweep(&handle, &outcome, Utc::now(), started.elapsed());
}

/// Start the maintenance loop.
///
/// One task per sweep, the shape `gc_registries`' loop already uses and for the
/// same reason: a panic inside a sweep comes back as a `JoinError` that this
/// loop records and keeps going from, instead of killing the loop and leaving
/// the store to grow silently for the daemon's whole lifetime.
///
/// The first sweep happens one interval in, not at startup: a daemon restarting
/// under load should not open by taking the room-store write lock for every
/// closed room it holds.
pub(super) fn spawn_maintenance_loop(state: &AppState) {
    let rooms = state.rooms.clone();
    let blob_root = state.room_attachments_root.clone();
    let config = state.room_maintenance_config;
    let handle = state.room_maintenance.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(config.interval);
        ticker.tick().await;
        loop {
            ticker.tick().await;
            sweep_once(rooms.clone(), blob_root.clone(), config, handle.clone()).await;
        }
    });
}

/// `POST /v1/rooms/maintenance/run` — sweep now, operator only.
///
/// Operator-authenticated for the same reason the room-agent mutations are: this
/// route deletes durable content on demand, and the local trust boundary this
/// daemon actually has is the `X-Ocean-Operator` key — not membership in any one
/// room, since the sweep is store-wide and belongs to no room.
///
/// It answers with the report the sweep just wrote, so an operator running it by
/// hand does not then have to go to `/health` to find out what happened.
pub(super) async fn room_maintenance_run(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> (StatusCode, Json<serde_json::Value>) {
    // No member lane here, unlike `POST .../close`: there is no room to be a
    // member of. An absent header is therefore a refusal and not a fallback,
    // which is exactly the mapping `room_agent_authority` uses.
    let principal = match state.room_operator.authorize(&headers) {
        Ok(principal) => principal,
        Err(error) => return crate::room_agent_authority::ApiError::from(error).response(),
    };
    tracing::info!(
        operator = principal.id(),
        "room maintenance sweep requested on demand"
    );
    sweep_once(
        state.rooms.clone(),
        state.room_attachments_root.clone(),
        state.room_maintenance_config,
        state.room_maintenance.clone(),
    )
    .await;
    let report = report_snapshot(&state.room_maintenance);
    (
        StatusCode::OK,
        Json(json!({ "ok": true, "room_maintenance": report })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_core::{RoomMessageKind, RoomParticipant, RoomParticipantKind};
    use ocean_store::{RoomCloser, RoomStore, SqliteRoomStore};

    /// A store handle plus the blob root that indexes into it, laid out the way
    /// the daemon lays them out: `<dir>/rooms.db` beside `<dir>/room-attachments`.
    fn fixture(dir: &Path) -> (RoomStoreHandle, PathBuf) {
        let store = SqliteRoomStore::open(dir.join("rooms.db")).expect("open store");
        (Arc::new(Mutex::new(store)), dir.join("room-attachments"))
    }

    /// Create an open room with one Human on the roster and one ordinary
    /// message in it.
    fn seed_room(rooms: &RoomStoreHandle, key: &RoomKey, at: DateTime<Utc>) {
        with_rooms_handle(rooms, |store| {
            store.create(key.clone(), "Fixture", None, at)?;
            store.add_participant(
                key,
                RoomParticipant {
                    id: "alice".into(),
                    kind: RoomParticipantKind::Human,
                    display_name: "Alice".into(),
                },
                at,
            )?;
            store.append_message(
                key,
                "alice",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "a durable line",
                at,
            )?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .expect("seed room");
    }

    /// Index an attachment AND write its bytes, exactly as an upload does.
    fn seed_attachment(
        rooms: &RoomStoreHandle,
        blob_root: &Path,
        key: &RoomKey,
        id: &str,
        bytes: &[u8],
        at: DateTime<Utc>,
    ) {
        crate::room_attachments::write_blob_for_test(blob_root, key, id, bytes);
        with_rooms_handle(rooms, |store| {
            store.add_attachment(
                key,
                id,
                "notes.txt",
                "text/plain",
                bytes.len() as u64,
                "0".repeat(64).as_str(),
                "alice",
                at,
            )
        })
        .expect("index attachment");
    }

    /// Retention cuts a room closed longer than the window, and NOTHING else.
    ///
    /// A fixed clock rather than a real one, because the only window a test
    /// could reach by waiting is zero days — and zero is the value that means
    /// "retention is off". Three rooms, one for each way the window is decided:
    /// closed before the cutoff, closed after it, and never closed at all. The
    /// open room is the assertion that matters most: retention is measured from
    /// the CLOSE, so a room that has been running for a year is not eligible,
    /// and a sweep that measured from `created_at` or `updated_at` instead would
    /// pass the first two assertions and delete a live room's history.
    #[test]
    fn retention_cuts_only_a_room_closed_longer_than_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let (rooms, blob_root) = fixture(tmp.path());
        let now = DateTime::parse_from_rfc3339("2026-09-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let long_ago = now - chrono::Duration::days(40);
        let recently = now - chrono::Duration::days(2);

        let cut = RoomKey::new("closed-long-ago");
        let keep = RoomKey::new("closed-recently");
        let open = RoomKey::new("never-closed");
        for key in [&cut, &keep, &open] {
            seed_room(&rooms, key, long_ago);
        }
        seed_attachment(
            &rooms,
            &blob_root,
            &cut,
            &"a".repeat(32),
            b"cut me",
            long_ago,
        );
        seed_attachment(
            &rooms,
            &blob_root,
            &keep,
            &"b".repeat(32),
            b"keep me",
            long_ago,
        );
        with_rooms_handle(&rooms, |store| {
            store.close_with_marker(&cut, RoomCloser::Member("alice"), long_ago)?;
            store.close_with_marker(&keep, RoomCloser::Member("alice"), recently)?;
            Ok::<_, ocean_store::RoomStoreError>(())
        })
        .unwrap();

        let config = MaintenanceConfig {
            retention_days: 30,
            // Zero grace so the orphan half of the same sweep cannot be what
            // removed the cut room's blob: the retention path unlinks it by id.
            orphan_grace: Duration::ZERO,
            ..MaintenanceConfig::default()
        };
        let outcome = run_sweep(&rooms, &blob_root, &config, now);
        assert_eq!(outcome.error, None, "clean sweep");
        assert_eq!(outcome.rooms_cut, 1, "only the room past the window");

        // The cut room: transcript gone, attachment row gone, bytes gone. Its
        // `rooms` row survives, which is what keeps `/snapshot` able to say
        // `closed: true` instead of 404ing as a room that never existed.
        with_rooms_handle(&rooms, |store| {
            let page = store
                .transcript_page_including_closed(&cut, None, None)
                .expect("the room row still exists");
            assert!(page.messages.is_empty(), "cut room keeps no transcript");
            assert!(store
                .attachments(&cut)
                .expect("attachments read")
                .is_empty());
            assert!(store
                .get_including_closed(&cut)
                .expect("record read")
                .is_some());
        });
        assert!(
            !crate::room_attachments::room_dir(&blob_root, &cut)
                .join("a".repeat(32))
                .exists(),
            "the cut room's blob bytes must be unlinked after the commit"
        );

        // The room closed INSIDE the window keeps everything, bytes included.
        with_rooms_handle(&rooms, |store| {
            let page = store
                .transcript_page_including_closed(&keep, None, None)
                .unwrap();
            assert_eq!(
                page.messages.len(),
                4,
                "join marker, message, attachment marker and close marker all survive"
            );
            assert_eq!(store.attachments(&keep).unwrap().len(), 1);
        });
        assert_eq!(
            std::fs::read(
                crate::room_attachments::room_dir(&blob_root, &keep).join("b".repeat(32))
            )
            .unwrap(),
            b"keep me",
            "a room inside the window keeps its bytes byte-for-byte"
        );

        // The OPEN room is untouched and still open, however old it is.
        with_rooms_handle(&rooms, |store| {
            let record = store
                .get(&open)
                .unwrap()
                .expect("an open room is never eligible for retention");
            // Join marker plus the message. No attachment and no close marker.
            assert_eq!(record.transcript.len(), 2);
        });
    }

    /// Retention off — the default — cuts nothing at all.
    #[test]
    fn a_zero_window_is_never_and_cuts_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let (rooms, blob_root) = fixture(tmp.path());
        let now = DateTime::parse_from_rfc3339("2026-09-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let key = RoomKey::new("ancient");
        seed_room(&rooms, &key, now - chrono::Duration::days(4000));
        with_rooms_handle(&rooms, |store| {
            store.close_with_marker(
                &key,
                RoomCloser::Member("alice"),
                now - chrono::Duration::days(4000),
            )
        })
        .unwrap();

        let outcome = run_sweep(&rooms, &blob_root, &MaintenanceConfig::default(), now);
        assert_eq!(outcome.rooms_cut, 0);
        assert_eq!(outcome.messages_removed, 0);
        with_rooms_handle(&rooms, |store| {
            assert_eq!(
                store
                    .transcript_page_including_closed(&key, None, None)
                    .unwrap()
                    .messages
                    .len(),
                // Join marker, message, close marker: nothing was cut.
                3
            );
        });
    }

    /// The orphan sweep takes the unreferenced blob and leaves the referenced
    /// one intact — and this test PROVES that by mutation rather than by
    /// outcome.
    ///
    /// MUTATION RESULT, recorded here because an outcome-only assertion passes
    /// against a sweep that deletes nothing: [`is_referenced`] is the whole
    /// decision, so the test drives `sweep_room_dir` three times against the
    /// same directory with three reference sets.
    ///
    /// * Say **referenced for everything** (the mutant that never deletes): the
    ///   orphan survives, and `orphan_files_removed` stays 0 — the first
    ///   assertion below fails.
    /// * Say **nothing is referenced** (the mutant that deletes indiscriminately):
    ///   the live attachment's bytes go too — the last assertion fails.
    /// * Say the truth: only the orphan goes, and the referenced blob still
    ///   reads back byte-for-byte.
    ///
    /// Both mutants are exercised for real below, not described: each is a
    /// doctored `HashSet` handed to the same function the real sweep calls, so
    /// neither can pass while the true run also passes.
    #[test]
    fn orphan_gc_takes_the_unreferenced_blob_and_only_that_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (rooms, blob_root) = fixture(tmp.path());
        let now = Utc::now();
        let key = RoomKey::new("gc-room");
        seed_room(&rooms, &key, now);

        let live = "c".repeat(32);
        let orphan = "d".repeat(32);
        seed_attachment(&rooms, &blob_root, &key, &live, b"referenced bytes", now);
        // Bytes with no row: exactly the residue the upload path's
        // write-then-commit order leaves when the commit never happens.
        crate::room_attachments::write_blob_for_test(&blob_root, &key, &orphan, b"orphan bytes");
        let dir = crate::room_attachments::room_dir(&blob_root, &key);

        // MUTANT 1 — "referenced for everything". Nothing may be deleted.
        let everything: HashSet<String> = [live.clone(), orphan.clone()].into_iter().collect();
        let mut mutant = SweepOutcome::default();
        sweep_room_dir(&dir, &everything, Duration::ZERO, &mut mutant);
        assert_eq!(
            mutant.orphan_files_removed, 0,
            "a sweep that believes everything is referenced must remove nothing"
        );
        assert!(dir.join(&orphan).exists(), "mutant 1 left the orphan");

        // The TRUE run, through the real entry point, with the real row set.
        let config = MaintenanceConfig {
            orphan_grace: Duration::ZERO,
            ..MaintenanceConfig::default()
        };
        let outcome = run_sweep(&rooms, &blob_root, &config, now);
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.orphan_files_removed, 1, "exactly the orphan");
        assert_eq!(outcome.bytes_reclaimed, b"orphan bytes".len() as u64);
        assert!(!dir.join(&orphan).exists(), "the orphan is gone");
        assert_eq!(
            std::fs::read(dir.join(&live)).unwrap(),
            b"referenced bytes",
            "the referenced blob's bytes are untouched, byte-for-byte"
        );
        with_rooms_handle(&rooms, |store| {
            assert_eq!(
                store.attachments(&key).unwrap().len(),
                1,
                "the GC never touches the index; it only reads it"
            );
        });

        // MUTANT 2 — "nothing is referenced". The live blob goes too, which is
        // the failure the true run above must not have.
        let nothing: HashSet<String> = HashSet::new();
        let mut mutant = SweepOutcome::default();
        sweep_room_dir(&dir, &nothing, Duration::ZERO, &mut mutant);
        assert_eq!(
            mutant.orphan_files_removed, 1,
            "with no references the live blob is what is left to take"
        );
        assert!(
            !dir.join(&live).exists(),
            "mutant 2 deletes the referenced blob — the behaviour the true run must not have"
        );
    }

    /// A blob younger than the grace window survives.
    ///
    /// This is the correctness half of the grace, not caution: `room_attachments`
    /// fsyncs bytes BEFORE the row commits, so every successful upload spends a
    /// moment as an unreferenced file. A sweep with no grace races that window.
    #[test]
    fn a_blob_inside_the_grace_window_is_never_collected() {
        let tmp = tempfile::tempdir().unwrap();
        let (rooms, blob_root) = fixture(tmp.path());
        let key = RoomKey::new("grace-room");
        seed_room(&rooms, &key, Utc::now());
        let in_flight = "e".repeat(32);
        crate::room_attachments::write_blob_for_test(&blob_root, &key, &in_flight, b"mid-upload");

        let outcome = run_sweep(
            &rooms,
            &blob_root,
            &MaintenanceConfig::default(),
            Utc::now(),
        );
        assert_eq!(
            outcome.orphan_files_removed, 0,
            "an unreferenced blob inside the grace window is an upload in progress"
        );
        assert!(crate::room_attachments::room_dir(&blob_root, &key)
            .join(&in_flight)
            .exists());
    }

    /// A directory belonging to no room the store knows is collected whole —
    /// and a directory belonging to a CLOSED room is not.
    ///
    /// The closed half is the one worth a test. The blob path is a one-way hash
    /// of the room key, so the sweep can only ask "is this directory expected?"
    /// by re-deriving every room's; deriving only OPEN rooms would make every
    /// finished call's attachment directory unexpected, and the sweep would
    /// delete the files of every frozen room in the store.
    #[test]
    fn an_unknown_directory_goes_and_a_closed_rooms_directory_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let (rooms, blob_root) = fixture(tmp.path());
        let now = Utc::now();
        let closed = RoomKey::new("frozen-room");
        seed_room(&rooms, &closed, now);
        seed_attachment(&rooms, &blob_root, &closed, &"f".repeat(32), b"frozen", now);
        with_rooms_handle(&rooms, |store| {
            store.close_with_marker(&closed, RoomCloser::Member("alice"), now)
        })
        .unwrap();

        // A directory under a hash of a key the store has never held.
        let stranger = crate::room_attachments::room_dir(&blob_root, &RoomKey::new("no-such-room"));
        std::fs::create_dir_all(&stranger).unwrap();
        std::fs::write(stranger.join("9".repeat(32)), b"nobody's").unwrap();

        let config = MaintenanceConfig {
            orphan_grace: Duration::ZERO,
            ..MaintenanceConfig::default()
        };
        let outcome = run_sweep(&rooms, &blob_root, &config, now);
        assert_eq!(outcome.error, None);
        assert_eq!(outcome.orphan_dirs_removed, 1);
        assert_eq!(outcome.bytes_reclaimed, b"nobody's".len() as u64);
        assert!(!stranger.exists(), "a directory no room claims is removed");
        assert_eq!(
            std::fs::read(
                crate::room_attachments::room_dir(&blob_root, &closed).join("f".repeat(32))
            )
            .unwrap(),
            b"frozen",
            "a soft-closed room still owns its files"
        );
    }

    /// A failed whole-directory removal is visible and never credited as
    /// reclaimed bytes. The injected error keeps this deterministic under root.
    #[test]
    fn an_unknown_directory_that_cannot_be_removed_reclaims_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let stranger = tmp.path().join("unrecognized-room");
        std::fs::create_dir_all(&stranger).unwrap();
        std::fs::write(stranger.join("blob"), b"still here").unwrap();
        let mut outcome = SweepOutcome::default();

        remove_orphan_dir_with(&stranger, &mut outcome, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected removal refusal",
            ))
        });

        assert_eq!(outcome.orphan_dirs_removed, 0);
        assert_eq!(outcome.bytes_reclaimed, 0);
        assert_eq!(
            outcome.error.as_deref(),
            Some("orphan GC could not remove an unrecognized room directory")
        );
        assert!(stranger.exists(), "the failed removal left bytes on disk");
    }

    /// The window parse refuses to guess. Every unusable value is OFF, because
    /// the one safe direction to fail on a knob that deletes transcripts is
    /// "keep everything" — a typo must never become a SHORTER window.
    #[test]
    fn an_unusable_retention_value_is_off_and_never_a_guess() {
        assert_eq!(parse_retention_days("30"), 30);
        assert_eq!(parse_retention_days("  30  "), 30);
        assert_eq!(parse_retention_days("0"), 0);
        for unusable in [
            "",
            "   ",
            "thirty",
            "30d",
            "-1",
            "1.5",
            "99999999999999999999",
        ] {
            assert_eq!(
                parse_retention_days(unusable),
                DEFAULT_ROOM_RETENTION_DAYS,
                "{unusable:?} must leave retention off"
            );
        }
    }

    /// A recorded sweep is what `/health` reads, and the card carries the
    /// CONFIGURATION beside the counts.
    #[test]
    fn the_report_carries_the_configuration_and_the_last_runs_counts() {
        let config = MaintenanceConfig {
            retention_days: 45,
            orphan_grace: Duration::from_secs(60),
            interval: Duration::from_secs(3600),
        };
        let handle = new_handle(&config);
        let before = report_snapshot(&handle);
        assert_eq!(before.retention_days, 45);
        assert_eq!(before.interval_secs, 3600);
        assert_eq!(before.orphan_grace_secs, 60);
        assert_eq!(before.last_run_at, None, "no sweep has run yet");
        assert_eq!(before.runs_total, 0);

        record_sweep(
            &handle,
            &SweepOutcome {
                rooms_cut: 2,
                messages_removed: 17,
                bytes_reclaimed: 4096,
                error: Some("retention failed to cut a closed room".into()),
                ..SweepOutcome::default()
            },
            Utc::now(),
            Duration::from_millis(12),
        );
        let after = report_snapshot(&handle);
        assert_eq!(after.rooms_cut, 2);
        assert_eq!(after.messages_removed, 17);
        assert_eq!(after.bytes_reclaimed, 4096);
        assert_eq!(after.runs_total, 1);
        assert!(after.last_run_at.is_some());
        assert_eq!(
            after.last_error.as_deref(),
            Some("retention failed to cut a closed room"),
            "a sweep error must be readable on the card, not only in the log"
        );
        // Configuration survives a run: an operator reading `rooms_cut: 0` on a
        // later clean sweep can still see whether cutting is even on.
        assert_eq!(after.retention_days, 45);

        // A clean sweep CLEARS the error, so a non-null value always describes
        // the most recent run rather than the worst one ever seen.
        record_sweep(
            &handle,
            &SweepOutcome::default(),
            Utc::now(),
            Duration::from_millis(3),
        );
        let clean = report_snapshot(&handle);
        assert_eq!(clean.last_error, None);
        assert_eq!(clean.runs_total, 2);
    }

    /// A blob that cannot be unlinked is COUNTED and SURFACED, and its bytes
    /// are not claimed as reclaimed.
    ///
    /// This is the failure the whole operated half exists to catch, and it is
    /// invisible by construction: the rows commit, every count looks healthy,
    /// and disk simply never comes back. Discarding the `remove_file` error let
    /// a sweep report `last_error: null` and a `bytes_reclaimed` figure taken
    /// from the INDEX while the bytes were still on the filesystem — a report
    /// that is worse than none, because it actively says the opposite of what
    /// happened.
    ///
    /// The failure is induced by putting a DIRECTORY where the blob file should
    /// be, so `remove_file` fails with `EISDIR`. A read-only parent would not
    /// do: these tests can run as root, where mode bits are advisory and the
    /// unlink would succeed anyway.
    #[test]
    fn a_blob_that_cannot_be_unlinked_is_reported_not_swallowed() {
        let tmp = tempfile::tempdir().unwrap();
        let (rooms, blob_root) = fixture(tmp.path());
        let now = DateTime::parse_from_rfc3339("2026-09-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let long_ago = now - chrono::Duration::days(40);
        let key = RoomKey::new("unlinkable");
        seed_room(&rooms, &key, long_ago);

        // The row says there are 9 bytes; the path is a directory, so the
        // unlink cannot succeed.
        let id = "a".repeat(32);
        let stuck = crate::room_attachments::room_dir(&blob_root, &key).join(&id);
        std::fs::create_dir_all(&stuck).unwrap();
        with_rooms_handle(&rooms, |store| {
            store.add_attachment(
                &key,
                &id,
                "notes.txt",
                "text/plain",
                9,
                "0".repeat(64).as_str(),
                "alice",
                long_ago,
            )
        })
        .unwrap();
        with_rooms_handle(&rooms, |store| {
            store.close_with_marker(&key, RoomCloser::Member("alice"), long_ago)
        })
        .unwrap();

        let config = MaintenanceConfig {
            retention_days: 30,
            orphan_grace: Duration::ZERO,
            ..MaintenanceConfig::default()
        };
        let outcome = run_sweep(&rooms, &blob_root, &config, now);

        assert_eq!(outcome.rooms_cut, 1, "the row-level cut still happened");
        assert_eq!(
            outcome.blobs_unlink_failed, 1,
            "the failed unlink is counted, not discarded"
        );
        assert_eq!(
            outcome.bytes_reclaimed, 0,
            "bytes still on disk are never reported as reclaimed"
        );
        assert!(
            outcome.error.is_some(),
            "a sweep that could not free what it deleted is not a clean sweep"
        );
        assert!(stuck.exists(), "the fixture really did block the unlink");

        // The report an operator reads carries both.
        let handle = new_handle(&config);
        record_sweep(&handle, &outcome, now, Duration::from_millis(1));
        let card = report_snapshot(&handle);
        assert_eq!(card.blobs_unlink_failed, 1);
        assert_eq!(card.bytes_reclaimed, 0);
        assert!(card.last_error.is_some());
    }

    /// A corrupt/imported row is untrusted even though the HTTP upload path
    /// mints safe ids. Retention must never turn its stored id into an arbitrary
    /// filesystem path after the row-level cut has committed.
    #[test]
    fn a_malformed_stored_attachment_id_cannot_escape_the_blob_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (rooms, blob_root) = fixture(tmp.path());
        let now = DateTime::parse_from_rfc3339("2026-09-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let long_ago = now - chrono::Duration::days(40);
        let key = RoomKey::new("malformed-retained-id");
        seed_room(&rooms, &key, long_ago);

        let outside = tmp.path().join("must-survive.txt");
        std::fs::write(&outside, b"outside").unwrap();
        let malformed = outside.to_string_lossy().into_owned();
        with_rooms_handle(&rooms, |store| {
            store.add_attachment(
                &key,
                &malformed,
                "notes.txt",
                "text/plain",
                7,
                "0".repeat(64).as_str(),
                "alice",
                long_ago,
            )
        })
        .unwrap();
        with_rooms_handle(&rooms, |store| {
            store.close_with_marker(&key, RoomCloser::Member("alice"), long_ago)
        })
        .unwrap();

        let config = MaintenanceConfig {
            retention_days: 30,
            orphan_grace: Duration::ZERO,
            ..MaintenanceConfig::default()
        };
        let outcome = run_sweep(&rooms, &blob_root, &config, now);

        assert_eq!(outcome.rooms_cut, 1);
        assert_eq!(outcome.blobs_unlink_failed, 1);
        assert_eq!(outcome.bytes_reclaimed, 0);
        assert_eq!(
            outcome.error.as_deref(),
            Some("retention refused a malformed stored attachment id")
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    }

    /// A clean sweep still reports zero failures, so the counter above means
    /// something when it is nonzero.
    #[test]
    fn a_clean_sweep_reports_no_unlink_failures() {
        let tmp = tempfile::tempdir().unwrap();
        let (rooms, blob_root) = fixture(tmp.path());
        let now = DateTime::parse_from_rfc3339("2026-09-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let long_ago = now - chrono::Duration::days(40);
        let key = RoomKey::new("clean-cut");
        seed_room(&rooms, &key, long_ago);
        seed_attachment(
            &rooms,
            &blob_root,
            &key,
            &"b".repeat(32),
            b"12345678",
            long_ago,
        );
        with_rooms_handle(&rooms, |store| {
            store.close_with_marker(&key, RoomCloser::Member("alice"), long_ago)
        })
        .unwrap();

        let config = MaintenanceConfig {
            retention_days: 30,
            orphan_grace: Duration::ZERO,
            ..MaintenanceConfig::default()
        };
        let outcome = run_sweep(&rooms, &blob_root, &config, now);
        assert_eq!(outcome.blobs_unlink_failed, 0);
        assert_eq!(outcome.error, None);
        assert_eq!(
            outcome.bytes_reclaimed, 8,
            "bytes are claimed exactly when the file is actually gone"
        );
    }

    /// A second sweep over an already-cut archive does nothing and says so.
    ///
    /// The store's eligibility query is what makes this true; asserted from the
    /// sweep because that is where the misleading number would have shown up —
    /// every historical room recounted as another `rooms_cut`, on every run,
    /// forever.
    #[test]
    fn a_second_sweep_over_a_cut_archive_is_a_genuine_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let (rooms, blob_root) = fixture(tmp.path());
        let now = DateTime::parse_from_rfc3339("2026-09-02T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let long_ago = now - chrono::Duration::days(40);
        let key = RoomKey::new("archive");
        seed_room(&rooms, &key, long_ago);
        with_rooms_handle(&rooms, |store| {
            store.close_with_marker(&key, RoomCloser::Member("alice"), long_ago)
        })
        .unwrap();

        let config = MaintenanceConfig {
            retention_days: 30,
            orphan_grace: Duration::ZERO,
            ..MaintenanceConfig::default()
        };
        let first = run_sweep(&rooms, &blob_root, &config, now);
        assert_eq!(first.rooms_cut, 1);
        assert!(first.messages_removed > 0);

        let second = run_sweep(&rooms, &blob_root, &config, now);
        assert_eq!(
            second.rooms_cut, 0,
            "an emptied room must not be recounted on every future sweep"
        );
        assert_eq!(second.messages_removed, 0);
        assert_eq!(second.error, None);
    }
}
