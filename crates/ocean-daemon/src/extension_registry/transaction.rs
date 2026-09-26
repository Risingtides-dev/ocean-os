//! Stage A3a transactional extension registry writer.
//!
//! This is the internal mutation authority from the Stage A implementation
//! manifest §12–§14 and §17. It owns:
//!
//! - local-path acquisition into `quarantine/<operation-id>/` **without**
//!   `.state.lock`, bounded by four daemon-wide acquisition permits;
//! - one exclusive-lock writer that recovers any prior journal, rechecks
//!   `expected_state_revision`, adopts quarantine as staging, writes a
//!   `prepared` journal, publishes the immutable artifact, and renames the four
//!   complete next-revision state files;
//! - the durable first-Stage-A-publication marker, created immediately after
//!   the first state-file rename (the irreversible commit point) and ensured by
//!   every roll-forward, after which a missing `service-grants.json` fails
//!   closed in the shared reader;
//! - journal-proven rollback before the commit point and roll-forward after
//!   it, failing closed as `registry_recovery_required` when a staged file is
//!   missing or corrupt after the commit point;
//! - the exact §12.4 retention transitions and §14 grant preview/apply.
//!
//! It exposes no HTTP/CLI route itself and performs no supervisor
//! reconciliation: A3b's sibling `mutation` module composes it into the §15
//! routes (always through `spawn_blocking`) and reconciles the supervisor after
//! each commit, and daemon startup calls [`RegistryWriter::recover`] before any
//! reader or service starts. Stage A4's child [`git`] module fills the same
//! quarantine from one pinned public Git commit (§13.2). Nothing here executes
//! package code: acquisition copies and hashes bytes only, and Git acquisition
//! runs only the stripped-environment host `git` tool.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::io::{self, Write as _};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::sync::{Condvar, Mutex};

use super::*;

const JOURNAL_SCHEMA_VERSION: u32 = 1;
const ACQUISITION_PERMITS: usize = 4;
const MAX_REMOVAL_DEPTH: usize = 128;
const GRANT_DIFF_DOMAIN: &[u8] = b"ocean-extension-grant-diff-v1\0";
const BOOTSTRAP_PREFIX: &str = ".extensions-bootstrap-";

/// Publication order. `service-grants.json` is renamed first so that, in every
/// interrupted post-commit state, the companion already carries the new
/// revision while at least one A0 file still carries the old one: the shared
/// reader therefore sees a revision mismatch, never an A0-shaped snapshot
/// whose companion absence could be mistaken for the empty upgrade form.
const STATE_FILES: [&str; 4] = [
    "service-grants.json",
    "installs.json",
    "trust.json",
    "enabled.json",
];

/// Exact §2/§14 native-authority notice. It is part of the confirmation hash so
/// acknowledgement cannot be detached from the diff it was shown with.
pub(crate) const NATIVE_AUTHORITY_NOTICE: &str = "Stage A native activation grants daemon-user-equivalent authority. The process may attempt network access, read or modify any daemon-user-accessible filesystem data including registry/store bytes, and act outside assigned roots regardless of declared capabilities; declarations and assigned paths are not containment.";

/// Stage A4 pinned public Git acquisition (§13.2) into this writer's
/// quarantine. A child of the writer so it shares the descriptor-relative
/// primitives and lease without widening their visibility.
pub(crate) mod git;

const STATE_UNAVAILABLE: &str = "extension_state_unavailable";
const RECOVERY_REQUIRED: &str = "registry_recovery_required";
const PACKAGE_INVALID: &str = "package_invalid";
const REVISION_CONFLICT: &str = "state_revision_conflict";

// Serializes only the absent-registry bootstrap inside this daemon; the
// cross-process guard is the no-replace directory rename itself.
static BOOTSTRAP_LOCK: Mutex<()> = Mutex::new(());

/// Named interruption points for crash fixtures. Outside tests every
/// checkpoint is a no-op.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrashPoint {
    BeforeJournal,
    AfterJournal,
    AfterStorePublish,
    /// After the state file at this index of the publication order is renamed.
    /// Index 0 is the irreversible commit point.
    AfterStateRename(usize),
    AfterMarker,
    AfterDirectoryFsync,
    AfterRetentionCleanup,
    AfterJournalCommitted,
    BeforeRootPublish,
    AfterRootPublish,
}

#[derive(Debug)]
enum Fail {
    Reject(&'static str),
    /// Test-only simulated process death: skip every in-process cleanup,
    /// rollback, and roll-forward, exactly as a crash would.
    #[cfg_attr(not(test), allow(dead_code))]
    Crash,
}

impl From<StateError> for Fail {
    fn from(error: StateError) -> Self {
        Self::Reject(error.code())
    }
}

type Step<T> = Result<T, Fail>;

fn unavailable(_: io::Error) -> Fail {
    Fail::Reject(STATE_UNAVAILABLE)
}

fn invalid_package<E>(_: E) -> Fail {
    Fail::Reject(PACKAGE_INVALID)
}

/// Pre-commit or committed mutation failure. `committed == false` means the
/// previously effective revision (`state_revision`) is still authoritative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MutationError {
    pub(crate) operation_id: Uuid,
    pub(crate) committed: bool,
    pub(crate) state_revision: u64,
    pub(crate) code: &'static str,
}

impl MutationError {
    /// Fixed safe text. Never contains a path, value, or package byte.
    pub(crate) fn message(&self) -> &'static str {
        match self.code {
            "invalid_extension_id" => "extension id is invalid",
            "invalid_source" => {
                "install source must be an absolute lexically canonical local directory"
            }
            PACKAGE_INVALID => {
                "package tree is unsafe, over acquisition limits, or has an invalid manifest"
            }
            "package_identity_mismatch" => "package manifest id does not match the extension",
            "acquisition_capacity" => "extension acquisition capacity is exhausted",
            "invalid_git_source" => {
                "Git source must be an HTTPS URL on a public host at an exact 40- or 64-hex commit id"
            }
            "git_connection_pinning_unavailable" => {
                "pinned public Git acquisition is not available in this daemon"
            }
            "git_host_not_public" => "Git host resolved to a non-public address",
            "git_resolution_failed" => "Git host could not be resolved",
            "git_fetch_failed" => "pinned fetch of the exact Git revision failed",
            "git_acquisition_timeout" => "Git acquisition exceeded its deadline",
            "git_acquisition_limit" => "Git acquisition exceeded its object and temp size ceiling",
            "git_revision_mismatch" => "fetched object is not the exact requested commit",
            "git_tree_unsupported" => {
                "Git tree has a symlink, submodule, LFS or filter attribute, or unsafe path"
            }
            "git_process_cleanup_failed" => "Git process-group cleanup could not be proven",
            REVISION_CONFLICT => "extension registry revision changed; inspect and retry",
            "already_installed" => "extension is already installed",
            "extension_not_installed" => "extension is not installed",
            "extension_not_found" => "extension has no registry state",
            "extension_active" => "extension must be fully disabled and stopped",
            "reconciliation_in_progress" => {
                "extension service reconciliation is in progress; retry shortly"
            }
            "digest_mismatch" => "digest does not match the installed artifact",
            "artifact_unavailable" => "installed artifact is missing or does not verify",
            "artifact_store_conflict" => "stored payload for this digest does not verify",
            "grant_widens_manifest" => "requested grant exceeds the package manifest request",
            "declaration_policy_rejected" => {
                "network and filesystem service declarations cannot be granted or enabled in Stage A"
            }
            "native_process_ack_required" => {
                "every native service grant requires an explicit acknowledgement"
            }
            "invalid_secret_binding" => "secret binding is invalid or not requested",
            "unknown_service" => "service is not declared by the installed package",
            "invalid_capability_grant" => "capability grant is malformed",
            "grant_confirmation_mismatch" => {
                "grant confirmation does not match the current preview"
            }
            "trust_required" => "installed digest has no trust grant",
            "service_grant_required" => "a native service has no acknowledged service grant",
            "unresolved_bindings" => "a native service has unresolved grants or bindings",
            "host_incompatible" => "extension requires a newer Ocean version",
            "unsupported_platform" => "native services are unsupported on this platform",
            "project_not_found" => "project is not registered",
            RECOVERY_REQUIRED => "extension registry recovery is required",
            "simulated_crash" => "simulated crash",
            _ => "daemon-owned extension state is unavailable or incoherent",
        }
    }
}

/// The post-commit envelope fields A3b serializes (§15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct MutationOutcome {
    pub(crate) operation_id: Uuid,
    pub(crate) committed: bool,
    pub(crate) state_revision: u64,
    pub(crate) id: String,
    pub(crate) digest: Option<String>,
    /// Registry-derived: installed, exact-digest trust row, and some enabled
    /// scope. Runtime activation remains the supervisor's (A3b) concern.
    pub(crate) effective: bool,
    /// A committed retention step (payload/state-root deletion) will be
    /// retried by the next recovery. The registry generation is coherent.
    pub(crate) retention_cleanup_pending: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationKind {
    Install,
    Update,
    Trust,
    Enable,
    Disable,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnablementScope {
    Global,
    Project(Uuid),
}

/// Supervisor projection consumed by update/remove. A3b binds the real
/// supervisor; a package is stopped when no process/temp root it owns remains.
pub(crate) trait ServiceActivity {
    fn package_stopped(&self, package_id: &str) -> bool;

    /// A reconciliation pass is in flight and may still spawn from a
    /// generation it read before this writer's commit. Update/remove refuse
    /// with the retryable `reconciliation_in_progress` rather than race it.
    fn reconciliation_in_progress(&self) -> bool {
        false
    }

    /// Both facts as ONE consistent reading, which is what the guard uses.
    /// Two independent reads leave a gap: a pass in flight during the
    /// `package_stopped` read could spawn the package and finish before the
    /// `reconciliation_in_progress` read, and both would then say "go". A
    /// ledger overrides this with a single-lock snapshot. The default reads
    /// in-progress FIRST: a pass that registers after that read necessarily
    /// reads the registry after this writer's exclusive-lock commit, so it can
    /// only spawn from the committed generation.
    fn snapshot(&self, package_id: &str) -> ActivitySnapshot {
        let reconciling = self.reconciliation_in_progress();
        ActivitySnapshot {
            stopped: self.package_stopped(package_id),
            reconciling,
        }
    }
}

/// One consistent reading of a package's supervisor activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ActivitySnapshot {
    /// No managed task or retained cleanup authority of the package remains.
    pub(crate) stopped: bool,
    /// Some reconciliation pass is registered and not yet finished.
    pub(crate) reconciling: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceGrantRequest {
    pub(crate) service_id: String,
    pub(crate) native_process_ack: bool,
    pub(crate) secret_bindings: Vec<SecretBinding>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrustRequest {
    pub(crate) expected_state_revision: u64,
    pub(crate) digest: String,
    #[serde(default)]
    pub(crate) capabilities: CapabilitySet,
    #[serde(default)]
    pub(crate) service_grants: Vec<ServiceGrantRequest>,
    pub(crate) confirm_grant_diff: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub(crate) struct GrantSide {
    pub(crate) capabilities: CapabilitySet,
    pub(crate) service_grants: Vec<ServiceGrantRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct GrantPreview {
    pub(crate) added: GrantSide,
    pub(crate) removed: GrantSide,
    pub(crate) native_authority_notice: &'static str,
    pub(crate) confirmation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TrustResult {
    Preview {
        state_revision: u64,
        preview: Box<GrantPreview>,
    },
    Applied(MutationOutcome),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RecoveryReport {
    pub(crate) rolled_back: usize,
    pub(crate) rolled_forward: usize,
    pub(crate) cleanup_pending: usize,
    pub(crate) orphans_removed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalPhase {
    Prepared,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalFile {
    name: String,
    sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalArtifact {
    digest: String,
    newly_published: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalCleanup {
    purge_store_payloads: bool,
    remove_cache: bool,
    remove_tmp: bool,
    remove_data: bool,
}

impl JournalCleanup {
    fn any(self) -> bool {
        self.purge_store_payloads || self.remove_cache || self.remove_tmp || self.remove_data
    }
}

/// Journal: operation identity, old/new revision, non-secret source
/// provenance, staged names/hashes, and the intended artifact. Secret values
/// are structurally absent; bindings live only in the staged state files as
/// names.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionJournal {
    schema_version: u32,
    operation_id: Uuid,
    operation: OperationKind,
    phase: JournalPhase,
    extension_id: String,
    old_state_revision: u64,
    new_state_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<InstallSource>,
    files: Vec<JournalFile>,
    artifact: Option<JournalArtifact>,
    cleanup: JournalCleanup,
}

struct Plan {
    next: StateSnapshot,
    digest: Option<String>,
    cleanup: JournalCleanup,
}

struct Failure {
    committed: bool,
    revision: u64,
    fail: Fail,
}

impl Failure {
    fn pre(revision: u64) -> impl Fn(Fail) -> Failure + Copy {
        move |fail| Failure {
            committed: false,
            revision,
            fail,
        }
    }
}

/// Acquisition/sweep gate for one registry, shared by every `RegistryWriter`
/// in the process that names the same canonical config directory. Keying the
/// gate by registry (not by writer instance) is what makes the four-permit cap
/// daemon-wide and lets an orphan sweep prove no acquisition is live: a second
/// writer instance cannot hide its quarantine from another's recovery.
#[derive(Default)]
struct RegistryGate {
    active: usize,
    sweeping: bool,
}

static REGISTRY_GATES: Mutex<BTreeMap<PathBuf, RegistryGate>> = Mutex::new(BTreeMap::new());
static REGISTRY_GATE_SIGNAL: Condvar = Condvar::new();

fn registry_gates() -> std::sync::MutexGuard<'static, BTreeMap<PathBuf, RegistryGate>> {
    REGISTRY_GATES
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// One of four daemon-wide acquisition permits (§12.3 step 1).
struct AcquisitionPermit(PathBuf);

impl Drop for AcquisitionPermit {
    fn drop(&mut self) {
        let mut gates = registry_gates();
        if let Some(gate) = gates.get_mut(&self.0) {
            gate.active = gate.active.saturating_sub(1);
        }
        drop(gates);
        REGISTRY_GATE_SIGNAL.notify_all();
    }
}

/// Exclusive orphan-sweep claim: while held, no acquisition can begin, and it
/// is only granted when none is live. Released on drop.
struct SweepClaim(PathBuf);

impl SweepClaim {
    /// `None` when an acquisition is live: the sweep is skipped, never raced.
    fn try_claim(key: &FsPath) -> Option<Self> {
        let mut gates = registry_gates();
        let gate = gates.entry(key.to_path_buf()).or_default();
        if gate.active > 0 || gate.sweeping {
            return None;
        }
        gate.sweeping = true;
        Some(Self(key.to_path_buf()))
    }
}

impl Drop for SweepClaim {
    fn drop(&mut self) {
        let mut gates = registry_gates();
        if let Some(gate) = gates.get_mut(&self.0) {
            gate.sweeping = false;
        }
        drop(gates);
        REGISTRY_GATE_SIGNAL.notify_all();
    }
}

enum QuarantineHome {
    /// `extensions/quarantine/<op>` under an existing registry root.
    Root,
    /// `<config>/.extensions-bootstrap-<op>/quarantine/<op>` while no registry
    /// root exists. The bootstrap directory is the candidate root, published
    /// whole by one no-replace rename so the reader's absent-root empty state
    /// is never disturbed by an in-flight acquisition.
    Bootstrap {
        config: File,
        name: CString,
        directory: File,
    },
}

/// An acquisition in progress. It holds a permit and a same-filesystem
/// quarantine directory but never `.state.lock`. Dropping an unconsumed lease
/// deletes its quarantine.
pub(crate) struct AcquisitionLease {
    operation_id: Uuid,
    home: QuarantineHome,
    /// Canonical path of `quarantine` for tools that take paths (A4's host
    /// `git`). It is proven to name the retained descriptor before use.
    path: PathBuf,
    parent: File,
    quarantine: File,
    artifact: File,
    source: Option<InstallSource>,
    consumed: bool,
    _permit: AcquisitionPermit,
}

impl AcquisitionLease {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn operation_id(&self) -> Uuid {
        self.operation_id
    }

    fn operation_name(&self) -> CString {
        CString::new(self.operation_id.to_string()).expect("uuid has no NUL")
    }
}

impl Drop for AcquisitionLease {
    fn drop(&mut self) {
        if self.consumed {
            return;
        }
        let _ = remove_tree_at(&self.parent, &self.operation_name(), 0);
        if let QuarantineHome::Bootstrap { config, name, .. } = &self.home {
            let _ = remove_tree_at(config, name, 0);
        }
    }
}

/// A fully acquired, hashed, manifest-validated, non-executable quarantine.
pub(crate) struct VerifiedQuarantine {
    lease: AcquisitionLease,
    digest: String,
    id: String,
    version: String,
}

impl VerifiedQuarantine {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn digest(&self) -> &str {
        &self.digest
    }
}

/// The daemon-owned registry writer.
///
/// Intended as one instance per daemon (A3b holds it in `AppState`), but the
/// invariants do not depend on that: `.state.lock` serializes publication
/// across instances and processes, and the acquisition/sweep gate is shared by
/// every instance naming the same canonical config directory.
pub(crate) struct RegistryWriter {
    config_dir: PathBuf,
    /// The process-wide gate key, resolved ONCE here so this writer's permit
    /// and sweep paths can never disagree about which registry they guard
    /// (a transient canonicalize failure between the two would otherwise
    /// split them onto different keys).
    gate_key: PathBuf,
    #[cfg(test)]
    crash_at: Mutex<Option<CrashPoint>>,
}

impl RegistryWriter {
    pub(crate) fn new(config_dir: PathBuf) -> Self {
        let gate_key = fs::canonicalize(&config_dir).unwrap_or_else(|_| config_dir.clone());
        Self {
            config_dir,
            gate_key,
            #[cfg(test)]
            crash_at: Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(crate) fn crash_at(&self, point: Option<CrashPoint>) {
        *self.crash_at.lock().unwrap() = point;
    }

    fn checkpoint(&self, point: CrashPoint) -> Step<()> {
        #[cfg(test)]
        if *self.crash_at.lock().unwrap() == Some(point) {
            return Err(Fail::Crash);
        }
        let _ = point;
        Ok(())
    }

    fn error(&self, operation_id: Uuid, failure: Failure) -> MutationError {
        MutationError {
            operation_id,
            committed: failure.committed,
            state_revision: failure.revision,
            code: match failure.fail {
                Fail::Reject(code) => code,
                Fail::Crash => "simulated_crash",
            },
        }
    }

    fn open_config(&self) -> Step<File> {
        let canonical = fs::canonicalize(&self.config_dir).map_err(unavailable)?;
        let config = open_config_directory(&canonical)?;
        if !config.metadata().map_err(unavailable)?.is_dir() {
            return Err(Fail::Reject(STATE_UNAVAILABLE));
        }
        Ok(config)
    }

    fn gate_key(&self) -> PathBuf {
        self.gate_key.clone()
    }

    /// Waits only while an orphan sweep holds the gate (bounded by that
    /// sweep), then takes one of the four permits or refuses.
    fn try_permit(&self) -> Option<AcquisitionPermit> {
        let key = self.gate_key();
        let mut gates = registry_gates();
        loop {
            let gate = gates.entry(key.clone()).or_default();
            if !gate.sweeping {
                if gate.active >= ACQUISITION_PERMITS {
                    return None;
                }
                gate.active += 1;
                return Some(AcquisitionPermit(key));
            }
            gates = REGISTRY_GATE_SIGNAL
                .wait(gates)
                .unwrap_or_else(|poison| poison.into_inner());
        }
    }

    // ---------------------------------------------------------------------
    // §12.3 step 1 / §13.1: acquisition without `.state.lock`.
    // ---------------------------------------------------------------------

    /// Reserve a permit and an empty same-filesystem quarantine. Never takes
    /// `.state.lock`, so registry readers and the writer stay available.
    pub(crate) fn begin_acquisition(&self) -> Result<AcquisitionLease, MutationError> {
        let operation_id = Uuid::new_v4();
        let permit = self.try_permit().ok_or(MutationError {
            operation_id,
            committed: false,
            state_revision: 0,
            code: "acquisition_capacity",
        })?;
        self.begin_acquisition_inner(operation_id, permit)
            .map_err(|fail| self.error(operation_id, Failure::pre(0)(fail)))
    }

    fn begin_acquisition_inner(
        &self,
        operation_id: Uuid,
        permit: AcquisitionPermit,
    ) -> Step<AcquisitionLease> {
        let canonical = fs::canonicalize(&self.config_dir).map_err(unavailable)?;
        let config = self.open_config()?;
        let operation = CString::new(operation_id.to_string()).expect("uuid has no NUL");
        let (home, parent, path) =
            match open_dir_at(&config, OsStr::new("extensions"), "extensions/") {
                Ok(root) => {
                    let parent =
                        mkdir_open(&root, c"quarantine", 0o700, true).map_err(unavailable)?;
                    let path = canonical.join("extensions").join("quarantine");
                    (QuarantineHome::Root, parent, path)
                }
                Err(StateError::MissingComponent(_)) => {
                    let bootstrap = format!("{BOOTSTRAP_PREFIX}{operation_id}");
                    let path = canonical.join(&bootstrap).join("quarantine");
                    let name = CString::new(bootstrap).expect("uuid has no NUL");
                    let directory =
                        mkdir_open(&config, &name, 0o700, false).map_err(unavailable)?;
                    let parent =
                        mkdir_open(&directory, c"quarantine", 0o700, false).map_err(unavailable)?;
                    (
                        QuarantineHome::Bootstrap {
                            config,
                            name,
                            directory,
                        },
                        parent,
                        path,
                    )
                }
                Err(error) => return Err(error.into()),
            };
        let discard_home = |home: &QuarantineHome| {
            if let QuarantineHome::Bootstrap { config, name, .. } = home {
                let _ = remove_tree_at(config, name, 0);
            }
        };
        let quarantine = match mkdir_open(&parent, &operation, 0o700, false) {
            Ok(quarantine) => quarantine,
            Err(error) => {
                discard_home(&home);
                return Err(unavailable(error));
            }
        };
        let artifact = match mkdir_open(&quarantine, c"artifact", 0o755, false) {
            Ok(artifact) => artifact,
            Err(error) => {
                let _ = remove_tree_at(&parent, &operation, 0);
                discard_home(&home);
                return Err(unavailable(error));
            }
        };
        Ok(AcquisitionLease {
            operation_id,
            home,
            path: path.join(operation_id.to_string()),
            parent,
            quarantine,
            artifact,
            source: None,
            consumed: false,
            _permit: permit,
        })
    }

    /// Copy one local directory into the lease quarantine: descriptor-relative
    /// no-follow traversal, regular files and directories only, `nlink == 1`,
    /// no sparse file, and the A0 depth/entry/byte limits. No network, Git,
    /// shell, or package code is involved.
    pub(crate) fn fill_local(
        &self,
        lease: &mut AcquisitionLease,
        path: &str,
    ) -> Result<(), MutationError> {
        let operation_id = lease.operation_id;
        self.fill_local_inner(lease, path)
            .map_err(|fail| self.error(operation_id, Failure::pre(0)(fail)))
    }

    fn fill_local_inner(&self, lease: &mut AcquisitionLease, path: &str) -> Step<()> {
        if lease.source.is_some() {
            return Err(Fail::Reject("invalid_request"));
        }
        let source = InstallSource {
            kind: InstallSourceKind::LocalPath,
            locator: path.to_string(),
            revision: None,
        };
        validate_source(&source).map_err(|_| Fail::Reject("invalid_source"))?;
        let path = FsPath::new(path);
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(Fail::Reject("invalid_source"));
        };
        // Intermediate components resolve through the kernel (for example the
        // macOS `/var -> /private/var` alias); the package root itself and
        // everything beneath it are opened without following symlinks.
        let parent = File::open(parent).map_err(|_| Fail::Reject("invalid_source"))?;
        let root = match open_dir_at(&parent, name, "package source") {
            Ok(root) => root,
            Err(StateError::MissingComponent(_)) => return Err(Fail::Reject("invalid_source")),
            Err(_) => return Err(Fail::Reject(PACKAGE_INVALID)),
        };
        let mut budget = CopyBudget::default();
        copy_tree(&root, &lease.artifact, 0, &mut budget)?;
        fsync_dir(&lease.artifact).map_err(unavailable)?;
        fsync_dir(&lease.quarantine).map_err(unavailable)?;
        lease.source = Some(source);
        Ok(())
    }

    /// Hash the complete quarantined tree with the frozen `sha256-tree-v1`
    /// digest and validate its manifest/identity/inventory.
    pub(crate) fn seal(
        &self,
        lease: AcquisitionLease,
    ) -> Result<VerifiedQuarantine, MutationError> {
        let operation_id = lease.operation_id;
        seal_inner(lease).map_err(|fail| self.error(operation_id, Failure::pre(0)(fail)))
    }

    /// Convenience: permit, quarantine, copy, and seal one local directory.
    pub(crate) fn acquire_local(&self, path: &str) -> Result<VerifiedQuarantine, MutationError> {
        let mut lease = self.begin_acquisition()?;
        self.fill_local(&mut lease, path)?;
        self.seal(lease)
    }

    // ---------------------------------------------------------------------
    // Operations (§12.2, §12.4, §13.3, §14).
    // ---------------------------------------------------------------------

    pub(crate) fn install(
        &self,
        expected_state_revision: u64,
        quarantine: VerifiedQuarantine,
    ) -> Result<MutationOutcome, MutationError> {
        let id = quarantine.id.clone();
        let row = InstalledArtifact {
            id: id.clone(),
            version: quarantine.version.clone(),
            digest: quarantine.digest.clone(),
            source: quarantine
                .lease
                .source
                .clone()
                .expect("sealed quarantine records its source"),
        };
        self.run(
            OperationKind::Install,
            &id,
            expected_state_revision,
            Some(quarantine),
            move |_, current| {
                if current.installs.iter().any(|install| install.id == row.id) {
                    return Err(Fail::Reject("already_installed"));
                }
                let mut next = current.clone();
                // Install grants nothing: the result has exactly one install
                // row and no trust, service-grant, or enablement row for id.
                next.grants.retain(|grant| grant.id != row.id);
                next.service_grants.retain(|grant| grant.id != row.id);
                next.enablement.retain(|entry| entry.id != row.id);
                let digest = Some(row.digest.clone());
                next.installs.push(row);
                Ok(Plan {
                    next,
                    digest,
                    cleanup: JournalCleanup::default(),
                })
            },
        )
    }

    pub(crate) fn update(
        &self,
        id: &str,
        expected_state_revision: u64,
        quarantine: VerifiedQuarantine,
        activity: &dyn ServiceActivity,
    ) -> Result<MutationOutcome, MutationError> {
        let row = InstalledArtifact {
            id: quarantine.id.clone(),
            version: quarantine.version.clone(),
            digest: quarantine.digest.clone(),
            source: quarantine
                .lease
                .source
                .clone()
                .expect("sealed quarantine records its source"),
        };
        let target = id.to_string();
        self.run(
            OperationKind::Update,
            id,
            expected_state_revision,
            Some(quarantine),
            move |_, current| {
                if row.id != target {
                    return Err(Fail::Reject("package_identity_mismatch"));
                }
                if !current.installs.iter().any(|install| install.id == target) {
                    return Err(Fail::Reject("extension_not_installed"));
                }
                require_disabled_and_stopped(current, &target, activity)?;
                let mut next = current.clone();
                // §12.4: replace the row, delete every trust and service-grant
                // row for the id, retain (ineffective) enablement scopes.
                next.grants.retain(|grant| grant.id != target);
                next.service_grants.retain(|grant| grant.id != target);
                let digest = Some(row.digest.clone());
                for install in &mut next.installs {
                    if install.id == target {
                        *install = row.clone();
                    }
                }
                Ok(Plan {
                    next,
                    digest,
                    cleanup: JournalCleanup::default(),
                })
            },
        )
    }

    /// §14: without `confirm_grant_diff` this is a shared-lock preview that
    /// mutates nothing; with it, the exact preview hash at the current
    /// revision must match or the apply fails before any write.
    pub(crate) fn trust(
        &self,
        id: &str,
        request: TrustRequest,
    ) -> Result<TrustResult, MutationError> {
        let operation_id = Uuid::new_v4();
        if validate_extension_id(id).is_err() {
            return Err(self.error(
                operation_id,
                Failure::pre(0)(Fail::Reject("invalid_extension_id")),
            ));
        }
        let Some(confirmation) = request.confirm_grant_diff.clone() else {
            let state = read_locked_state(&self.config_dir)
                .map_err(|error| self.error(operation_id, Failure::pre(0)(error.into())))?;
            let revision = state.snapshot.revision;
            if revision != request.expected_state_revision {
                return Err(self.error(
                    operation_id,
                    Failure::pre(revision)(Fail::Reject(REVISION_CONFLICT)),
                ));
            }
            let root = state.root.as_ref().ok_or_else(|| {
                self.error(
                    operation_id,
                    Failure::pre(revision)(Fail::Reject("extension_not_installed")),
                )
            })?;
            let (preview, _) = plan_trust(root, &state.snapshot, id, &request)
                .map_err(|fail| self.error(operation_id, Failure::pre(revision)(fail)))?;
            return Ok(TrustResult::Preview {
                state_revision: revision,
                preview: Box::new(preview),
            });
        };
        let target = id.to_string();
        self.run_with_id(
            operation_id,
            OperationKind::Trust,
            id,
            request.expected_state_revision,
            None,
            move |root, current| {
                let (preview, next) = plan_trust(root, current, &target, &request)?;
                if preview.confirmation != confirmation {
                    return Err(Fail::Reject("grant_confirmation_mismatch"));
                }
                Ok(Plan {
                    next,
                    digest: Some(request.digest.clone()),
                    cleanup: JournalCleanup::default(),
                })
            },
        )
        .map(TrustResult::Applied)
    }

    pub(crate) fn enable(
        &self,
        id: &str,
        expected_state_revision: u64,
        scope: EnablementScope,
        registered_projects: &HashSet<Uuid>,
    ) -> Result<MutationOutcome, MutationError> {
        let target = id.to_string();
        self.run(
            OperationKind::Enable,
            id,
            expected_state_revision,
            None,
            move |root, current| {
                let install = current
                    .installs
                    .iter()
                    .find(|install| install.id == target)
                    .ok_or(Fail::Reject("extension_not_installed"))?;
                if let EnablementScope::Project(project) = scope {
                    if !registered_projects.contains(&project) {
                        return Err(Fail::Reject("project_not_found"));
                    }
                }
                require_enableable(root, current, install)?;
                let mut next = current.clone();
                let entry = enablement_entry(&mut next, &target);
                match scope {
                    EnablementScope::Global => entry.global = true,
                    EnablementScope::Project(project) => set_override(entry, project, true),
                }
                Ok(Plan {
                    next,
                    digest: Some(install.digest.clone()),
                    cleanup: JournalCleanup::default(),
                })
            },
        )
    }

    /// Always allowed by trust state. Supervisor filter removal/reap ordering
    /// before the HTTP 200 is A3b's reconciliation contract.
    pub(crate) fn disable(
        &self,
        id: &str,
        expected_state_revision: u64,
        scope: EnablementScope,
        registered_projects: &HashSet<Uuid>,
    ) -> Result<MutationOutcome, MutationError> {
        let target = id.to_string();
        self.run(
            OperationKind::Disable,
            id,
            expected_state_revision,
            None,
            move |_, current| {
                let existing = current.enablement.iter().find(|entry| entry.id == target);
                let installed = current.installs.iter().find(|install| install.id == target);
                if existing.is_none() && installed.is_none() {
                    return Err(Fail::Reject("extension_not_found"));
                }
                if let EnablementScope::Project(project) = scope {
                    let has_override = existing.is_some_and(|entry| {
                        entry
                            .projects
                            .iter()
                            .any(|override_| override_.project_id == project)
                    });
                    if !registered_projects.contains(&project) && !has_override {
                        return Err(Fail::Reject("project_not_found"));
                    }
                }
                let digest = installed.map(|install| install.digest.clone());
                let mut next = current.clone();
                let entry = enablement_entry(&mut next, &target);
                match scope {
                    EnablementScope::Global => entry.global = false,
                    EnablementScope::Project(project) => set_override(entry, project, false),
                }
                Ok(Plan {
                    next,
                    digest,
                    cleanup: JournalCleanup::default(),
                })
            },
        )
    }

    /// §12.4 remove. Requires every scope disabled, the package stopped, and
    /// no leftover connection temp root. Revokes install, trust,
    /// acknowledgement, bindings, and enablement; post-commit it deletes every
    /// unreferenced payload for the id, `cache/`, `tmp/`, and with
    /// `purge_state` also `data/`.
    pub(crate) fn remove(
        &self,
        id: &str,
        expected_state_revision: u64,
        purge_state: bool,
        activity: &dyn ServiceActivity,
    ) -> Result<MutationOutcome, MutationError> {
        let target = id.to_string();
        self.run(
            OperationKind::Remove,
            id,
            expected_state_revision,
            None,
            move |root, current| {
                let has_state = current.installs.iter().any(|entry| entry.id == target)
                    || current.grants.iter().any(|entry| entry.id == target)
                    || current
                        .service_grants
                        .iter()
                        .any(|entry| entry.id == target)
                    || current.enablement.iter().any(|entry| entry.id == target);
                if !has_state {
                    return Err(Fail::Reject("extension_not_found"));
                }
                require_disabled_and_stopped(current, &target, activity)?;
                if connection_temp_remains(root, &target)? {
                    return Err(Fail::Reject("extension_active"));
                }
                let mut next = current.clone();
                next.installs.retain(|entry| entry.id != target);
                next.grants.retain(|entry| entry.id != target);
                next.service_grants.retain(|entry| entry.id != target);
                next.enablement.retain(|entry| entry.id != target);
                Ok(Plan {
                    next,
                    digest: None,
                    cleanup: JournalCleanup {
                        purge_store_payloads: true,
                        remove_cache: true,
                        remove_tmp: true,
                        remove_data: purge_state,
                    },
                })
            },
        )
    }

    /// Startup recovery: journal-proven rollback/roll-forward under the
    /// exclusive lock, then removal of orphan staging, quarantine, and
    /// bootstrap directories that no journal or live acquisition references.
    pub(crate) fn recover(&self) -> Result<RecoveryReport, MutationError> {
        let operation_id = Uuid::new_v4();
        self.recover_inner()
            .map_err(|fail| self.error(operation_id, Failure::pre(0)(fail)))
    }

    fn recover_inner(&self) -> Step<RecoveryReport> {
        let config = self.open_config()?;
        // Held across both sweeps and the journal recovery between them, so an
        // acquisition cannot begin (and have its quarantine or bootstrap
        // directory deleted mid-copy) after the no-live-acquisition check.
        let sweep = SweepClaim::try_claim(&self.gate_key());
        let mut report = RecoveryReport::default();
        if sweep.is_some() {
            for name in raw_names(&config).map_err(unavailable)? {
                if name.to_bytes().starts_with(BOOTSTRAP_PREFIX.as_bytes()) {
                    remove_tree_at(&config, &name, 0).map_err(unavailable)?;
                    report.orphans_removed += 1;
                }
            }
        }
        let root = match open_dir_at(&config, OsStr::new("extensions"), "extensions/") {
            Ok(root) => root,
            Err(StateError::MissingComponent(_)) => return Ok(report),
            Err(error) => return Err(error.into()),
        };
        let lock = open_regular_file_at(&root, OsStr::new(".state.lock"), ".state.lock")?;
        acquire_exclusive_lock(&lock)?;
        let journals = recover_locked(&root)?;
        report.rolled_back += journals.rolled_back;
        report.rolled_forward += journals.rolled_forward;
        report.cleanup_pending += journals.cleanup_pending;
        report.orphans_removed += journals.orphans_removed;
        if sweep.is_some() {
            if let Some(quarantine) = open_optional_dir(&root, c"quarantine")? {
                for name in raw_names(&quarantine).map_err(unavailable)? {
                    remove_tree_at(&quarantine, &name, 0).map_err(unavailable)?;
                    report.orphans_removed += 1;
                }
            }
        }
        Ok(report)
    }

    // ---------------------------------------------------------------------
    // §12.3 steps 2–8.
    // ---------------------------------------------------------------------

    fn run(
        &self,
        operation: OperationKind,
        id: &str,
        expected: u64,
        quarantine: Option<VerifiedQuarantine>,
        plan: impl FnOnce(&File, &StateSnapshot) -> Step<Plan>,
    ) -> Result<MutationOutcome, MutationError> {
        let operation_id = quarantine
            .as_ref()
            .map_or_else(Uuid::new_v4, |quarantine| quarantine.lease.operation_id);
        self.run_with_id(operation_id, operation, id, expected, quarantine, plan)
    }

    fn run_with_id(
        &self,
        operation_id: Uuid,
        operation: OperationKind,
        id: &str,
        expected: u64,
        quarantine: Option<VerifiedQuarantine>,
        plan: impl FnOnce(&File, &StateSnapshot) -> Step<Plan>,
    ) -> Result<MutationOutcome, MutationError> {
        if validate_extension_id(id).is_err() {
            return Err(self.error(
                operation_id,
                Failure::pre(0)(Fail::Reject("invalid_extension_id")),
            ));
        }
        self.transact(operation_id, operation, id, expected, quarantine, plan)
            .map_err(|failure| self.error(operation_id, failure))
    }

    fn transact(
        &self,
        operation_id: Uuid,
        operation: OperationKind,
        id: &str,
        expected: u64,
        quarantine: Option<VerifiedQuarantine>,
        plan: impl FnOnce(&File, &StateSnapshot) -> Step<Plan>,
    ) -> Result<MutationOutcome, Failure> {
        let config = self.open_config().map_err(Failure::pre(0))?;
        let root = match open_dir_at(&config, OsStr::new("extensions"), "extensions/") {
            Ok(root) => root,
            Err(StateError::MissingComponent(_)) => {
                if expected != 0 {
                    return Err(Failure::pre(0)(Fail::Reject(REVISION_CONFLICT)));
                }
                return match (operation, quarantine) {
                    (OperationKind::Install, Some(quarantine)) => {
                        self.bootstrap_install(config, quarantine, plan)
                    }
                    _ => Err(Failure::pre(0)(Fail::Reject("extension_not_installed"))),
                };
            }
            Err(error) => return Err(Failure::pre(0)(error.into())),
        };

        // Step 2: exclusive lock, prior-journal recovery, precondition recheck.
        let lock = open_regular_file_at(&root, OsStr::new(".state.lock"), ".state.lock")
            .map_err(|error| Failure::pre(0)(error.into()))?;
        acquire_exclusive_lock(&lock).map_err(|error| Failure::pre(0)(error.into()))?;
        recover_locked(&root).map_err(Failure::pre(0))?;
        let (current, _) =
            read_coherent_generation(&root).map_err(|error| Failure::pre(0)(error.into()))?;
        let old_revision = current.revision;
        let pre = Failure::pre(old_revision);
        if old_revision != expected {
            return Err(pre(Fail::Reject(REVISION_CONFLICT)));
        }
        let Plan {
            mut next,
            digest,
            cleanup,
        } = match plan(&root, &current) {
            Ok(plan) => plan,
            Err(fail) => return Err(Failure::pre(old_revision)(fail)),
        };
        let new_revision = old_revision
            .checked_add(1)
            .ok_or(Fail::Reject(STATE_UNAVAILABLE))
            .map_err(Failure::pre(old_revision))?;
        next.revision = new_revision;

        // Step 3: adopt quarantine, construct and validate all four files.
        let rendered = validate_and_render(&root, &next).map_err(Failure::pre(old_revision))?;
        let staging_root = mkdir_open(&root, c"staging", 0o700, true)
            .map_err(|error| Failure::pre(old_revision)(unavailable(error)))?;
        let operation_name = CString::new(operation_id.to_string()).expect("uuid has no NUL");
        let source = quarantine
            .as_ref()
            .and_then(|quarantine| quarantine.lease.source.clone());
        let (staging, artifact) = match quarantine {
            Some(quarantine) => {
                let (staging, artifact) = adopt(&root, &staging_root, id, quarantine)
                    .map_err(Failure::pre(old_revision))?;
                (staging, Some(artifact))
            }
            None => (
                mkdir_open(&staging_root, &operation_name, 0o700, false)
                    .map_err(|error| Failure::pre(old_revision)(unavailable(error)))?,
                None,
            ),
        };
        // §12.3: the adopted (or created) `staging/<op>` entry and the
        // `staging/` directory itself are durable before the prepared journal
        // that names them.
        if let Err(error) = fsync_dir(&staging_root).and_then(|()| fsync_dir(&root)) {
            let _ = remove_tree_at(&staging_root, &operation_name, 0);
            return Err(Failure::pre(old_revision)(unavailable(error)));
        }
        durability_trace("staging-root-synced");
        let files = match write_staged_files(&staging, &rendered) {
            Ok(files) => files,
            Err(fail) => {
                let _ = remove_tree_at(&staging_root, &operation_name, 0);
                return Err(Failure::pre(old_revision)(fail));
            }
        };
        let journal = TransactionJournal {
            schema_version: JOURNAL_SCHEMA_VERSION,
            operation_id,
            operation,
            phase: JournalPhase::Prepared,
            extension_id: id.to_string(),
            old_state_revision: old_revision,
            new_state_revision: new_revision,
            source,
            files,
            artifact,
            cleanup,
        };
        let pre_commit = |fail: Fail| -> Failure {
            if !matches!(fail, Fail::Crash) {
                // Journal-proven rollback in process; a crash leaves this to
                // the next recovery.
                let _ = rollback(&root, &journal);
            }
            Failure::pre(old_revision)(fail)
        };
        self.checkpoint(CrashPoint::BeforeJournal)
            .map_err(|fail| Failure::pre(old_revision)(fail))?;

        // Step 4: prepared journal.
        write_journal(&root, &journal).map_err(pre_commit)?;
        self.checkpoint(CrashPoint::AfterJournal)
            .map_err(pre_commit)?;

        // Step 5: publish the immutable artifact.
        if let Some(artifact) = &journal.artifact {
            if artifact.newly_published {
                publish_artifact(&root, &staging, id, &artifact.digest).map_err(pre_commit)?;
            }
        }
        self.checkpoint(CrashPoint::AfterStorePublish)
            .map_err(pre_commit)?;

        // Steps 6–7: commit point, roll-forward, cleanup, journal completion.
        match roll_forward(&root, &journal, &|point| self.checkpoint(point)) {
            Ok(result) => Ok(MutationOutcome {
                operation_id,
                committed: true,
                state_revision: new_revision,
                id: id.to_string(),
                digest,
                effective: registry_effective(&next, id),
                retention_cleanup_pending: result.cleanup_pending,
            }),
            Err(Fail::Crash) => Err(Failure {
                committed: false,
                revision: old_revision,
                fail: Fail::Crash,
            }),
            Err(fail) => {
                if !journal_committed(&root, &journal) {
                    // The first rename itself failed: still pre-commit.
                    let _ = rollback(&root, &journal);
                    return Err(Failure::pre(old_revision)(fail));
                }
                // Past the commit point: one more journal-proven attempt
                // before responding (§12.3), else committed recovery error.
                match roll_forward(&root, &journal, &|_| Ok(())) {
                    Ok(result) => Ok(MutationOutcome {
                        operation_id,
                        committed: true,
                        state_revision: new_revision,
                        id: id.to_string(),
                        digest,
                        effective: registry_effective(&next, id),
                        retention_cleanup_pending: result.cleanup_pending,
                    }),
                    Err(_) => Err(Failure {
                        committed: true,
                        revision: new_revision,
                        fail: Fail::Reject(RECOVERY_REQUIRED),
                    }),
                }
            }
        }
    }

    /// First publication into a config directory with no registry root. The
    /// complete generation (lock file, artifact, four files, marker) is built
    /// inside the private bootstrap directory and published by one no-replace
    /// rename, which is both the commit point and the first publication.
    fn bootstrap_install(
        &self,
        config: File,
        quarantine: VerifiedQuarantine,
        plan: impl FnOnce(&File, &StateSnapshot) -> Step<Plan>,
    ) -> Result<MutationOutcome, Failure> {
        let pre = Failure::pre(0);
        let _guard = BOOTSTRAP_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if open_dir_at(&config, OsStr::new("extensions"), "extensions/").is_ok() {
            return Err(pre(Fail::Reject(REVISION_CONFLICT)));
        }
        let VerifiedQuarantine {
            mut lease,
            digest,
            id,
            ..
        } = quarantine;
        let QuarantineHome::Bootstrap {
            config: _,
            name,
            directory,
        } = &lease.home
        else {
            return Err(Failure::pre(0)(Fail::Reject(STATE_UNAVAILABLE)));
        };
        let empty = StateSnapshot {
            revision: 0,
            installs: Vec::new(),
            grants: Vec::new(),
            enablement: Vec::new(),
            service_grants: Vec::new(),
        };
        let Plan {
            mut next,
            digest: outcome_digest,
            ..
        } = plan(directory, &empty).map_err(Failure::pre(0))?;
        next.revision = 1;
        // From here the bootstrap directory, not the lease, owns the bytes; a
        // simulated crash must leave it behind exactly as a real one would.
        lease.consumed = true;
        let result = (|| -> Step<()> {
            let bootstrap = directory;
            create_file_at(bootstrap, c".state.lock", 0o600).map_err(unavailable)?;
            let store = mkdir_open(bootstrap, c"store", 0o755, false).map_err(unavailable)?;
            let id_name = CString::new(id.as_str()).map_err(invalid_package)?;
            let id_dir = mkdir_open(&store, &id_name, 0o755, false).map_err(unavailable)?;
            let hex = CString::new(digest_hex(&digest).ok_or(Fail::Reject(PACKAGE_INVALID))?)
                .expect("hex has no NUL");
            renameat_at(&lease.quarantine, c"artifact", &id_dir, &hex).map_err(unavailable)?;
            let artifact = open_dir_nofollow(&id_dir, &hex).map_err(unavailable)?;
            if snapshot_package(&artifact)?.digest != digest {
                return Err(Fail::Reject(PACKAGE_INVALID));
            }
            remove_tree_at(bootstrap, c"quarantine", 0).map_err(unavailable)?;
            let rendered = validate_and_render(bootstrap, &next)?;
            write_staged_files(bootstrap, &rendered)?;
            write_marker(bootstrap, bootstrap, 1)?;
            fsync_dir(&id_dir).map_err(unavailable)?;
            fsync_dir(&store).map_err(unavailable)?;
            fsync_dir(bootstrap).map_err(unavailable)?;
            // The candidate root must read as one coherent generation before
            // it can become the registry.
            let (snapshot, _) = read_coherent_generation(bootstrap)?;
            if snapshot.revision != 1 {
                return Err(Fail::Reject(STATE_UNAVAILABLE));
            }
            Ok(())
        })();
        if let Err(fail) = result {
            if !matches!(fail, Fail::Crash) {
                let _ = remove_tree_at(&config, name, 0);
            }
            return Err(pre(fail));
        }
        self.checkpoint(CrashPoint::BeforeRootPublish)
            .map_err(pre)?;
        if let Err(error) = renameat_noreplace(&config, name, &config, c"extensions") {
            let _ = remove_tree_at(&config, name, 0);
            return Err(pre(match error.raw_os_error() {
                Some(libc::EEXIST) | Some(libc::ENOTEMPTY) => Fail::Reject(REVISION_CONFLICT),
                _ => Fail::Reject(STATE_UNAVAILABLE),
            }));
        }
        let committed = |fail: Fail| Failure {
            committed: true,
            revision: 1,
            fail,
        };
        fsync_dir(&config).map_err(|error| committed(unavailable(error)))?;
        self.checkpoint(CrashPoint::AfterRootPublish)
            .map_err(committed)?;
        Ok(MutationOutcome {
            operation_id: lease.operation_id,
            committed: true,
            state_revision: 1,
            id: id.clone(),
            digest: outcome_digest,
            effective: registry_effective(&next, &id),
            retention_cleanup_pending: false,
        })
    }
}

// -------------------------------------------------------------------------
// Planning helpers.
// -------------------------------------------------------------------------

fn enablement_entry<'a>(next: &'a mut StateSnapshot, id: &str) -> &'a mut ExtensionEnablement {
    if let Some(index) = next.enablement.iter().position(|entry| entry.id == id) {
        return &mut next.enablement[index];
    }
    next.enablement.push(ExtensionEnablement {
        id: id.to_string(),
        global: false,
        projects: Vec::new(),
    });
    next.enablement.last_mut().expect("just pushed")
}

fn set_override(entry: &mut ExtensionEnablement, project: Uuid, enabled: bool) {
    match entry
        .projects
        .iter_mut()
        .find(|override_| override_.project_id == project)
    {
        Some(override_) => override_.enabled = enabled,
        None => entry.projects.push(ProjectEnablement {
            project_id: project,
            enabled,
        }),
    }
}

fn fully_disabled(snapshot: &StateSnapshot, id: &str) -> bool {
    snapshot
        .enablement
        .iter()
        .find(|entry| entry.id == id)
        .is_none_or(|entry| !entry.global && entry.projects.iter().all(|project| !project.enabled))
}

fn require_disabled_and_stopped(
    snapshot: &StateSnapshot,
    id: &str,
    activity: &dyn ServiceActivity,
) -> Step<()> {
    // One reading of both facts (see `ServiceActivity::snapshot`): never two
    // separately locked reads a pass could spawn between.
    let now = activity.snapshot(id);
    if !fully_disabled(snapshot, id) || !now.stopped {
        return Err(Fail::Reject("extension_active"));
    }
    if now.reconciling {
        return Err(Fail::Reject("reconciliation_in_progress"));
    }
    Ok(())
}

fn registry_effective(snapshot: &StateSnapshot, id: &str) -> bool {
    let Some(install) = snapshot.installs.iter().find(|install| install.id == id) else {
        return false;
    };
    let trusted = snapshot
        .grants
        .iter()
        .any(|grant| grant.id == id && grant.digest == install.digest);
    let enabled = snapshot.enablement.iter().any(|entry| {
        entry.id == id && (entry.global || entry.projects.iter().any(|project| project.enabled))
    });
    trusted && enabled
}

/// Verify the installed artifact under the held lock and return its metadata
/// plus host compatibility. Never executes package code.
fn load_artifact(root: &File, install: &InstalledArtifact) -> Step<(OceanExtensionMetadata, bool)> {
    let missing = |_: StateError| Fail::Reject("artifact_unavailable");
    let package = open_package(root, install).map_err(missing)?;
    let package = snapshot_package(&package).map_err(missing)?;
    if package.digest != install.digest {
        return Err(Fail::Reject("artifact_unavailable"));
    }
    let raw = RawOceanExtensionManifest::parse(&package.manifest)
        .map_err(|_| Fail::Reject("artifact_unavailable"))?;
    let host_version =
        Version::parse(env!("CARGO_PKG_VERSION")).expect("daemon package version is valid SemVer");
    let host_compatible = match raw.clone().validate_metadata(&host_version) {
        Ok(_) => true,
        Err(ExtensionManifestError::IncompatibleHost { .. }) => false,
        Err(_) => return Err(Fail::Reject("artifact_unavailable")),
    };
    let metadata = raw
        .validate_metadata(&Version::new(u64::MAX, u64::MAX, u64::MAX))
        .map_err(|_| Fail::Reject("artifact_unavailable"))?;
    if metadata.id != install.id
        || metadata.version.to_string() != install.version
        || !metadata_paths_exist(&metadata, &package.entries)
    {
        return Err(Fail::Reject("artifact_unavailable"));
    }
    Ok((metadata, host_compatible))
}

fn declares_network_or_filesystem(metadata: &OceanExtensionMetadata) -> bool {
    metadata.services.iter().any(|service| {
        !service.capabilities.network.is_empty() || !service.capabilities.filesystem.is_empty()
    })
}

/// §12.2 step 3 / §15 enable preconditions: exact-digest trust within the
/// manifest request, host compatibility, no network/filesystem declaration,
/// supported native platform, and every native service fully acknowledged
/// and bound. A project scope never supplies any of these.
fn require_enableable(
    root: &File,
    snapshot: &StateSnapshot,
    install: &InstalledArtifact,
) -> Step<()> {
    let (metadata, host_compatible) = load_artifact(root, install)?;
    if !host_compatible {
        return Err(Fail::Reject("host_incompatible"));
    }
    if declares_network_or_filesystem(&metadata) {
        return Err(Fail::Reject("declaration_policy_rejected"));
    }
    if !metadata.services.is_empty() && !cfg!(any(target_os = "macos", target_os = "linux")) {
        return Err(Fail::Reject("unsupported_platform"));
    }
    let trust = snapshot
        .grants
        .iter()
        .find(|grant| grant.id == install.id && grant.digest == install.digest)
        .ok_or(Fail::Reject("trust_required"))?;
    if !grants_are_subset(&trust.capabilities, &requested_capabilities(&metadata)) {
        return Err(Fail::Reject("grant_widens_manifest"));
    }
    for service in &metadata.services {
        let grant = snapshot
            .service_grants
            .iter()
            .find(|grant| {
                grant.id == install.id
                    && grant.digest == install.digest
                    && grant.service_id == service.id
            })
            .ok_or(Fail::Reject("service_grant_required"))?;
        if !service_is_fully_authorized(service, trust, grant) {
            return Err(Fail::Reject("unresolved_bindings"));
        }
    }
    Ok(())
}

fn sorted_unique(values: &[String]) -> Step<Vec<String>> {
    let set: BTreeSet<String> = values.iter().cloned().collect();
    if set.len() != values.len() {
        return Err(Fail::Reject("invalid_capability_grant"));
    }
    Ok(set.into_iter().collect())
}

fn canonical_capabilities(capabilities: &CapabilitySet) -> Step<CapabilitySet> {
    Ok(CapabilitySet {
        network: sorted_unique(&capabilities.network)?,
        filesystem: sorted_unique(&capabilities.filesystem)?,
        env: sorted_unique(&capabilities.env)?,
        secrets: sorted_unique(&capabilities.secrets)?,
    })
}

fn difference(left: &[String], right: &[String]) -> Vec<String> {
    let right: BTreeSet<&String> = right.iter().collect();
    let mut values: Vec<String> = left
        .iter()
        .filter(|value| !right.contains(value))
        .cloned()
        .collect();
    values.sort();
    values
}

fn capability_difference(left: &CapabilitySet, right: &CapabilitySet) -> CapabilitySet {
    CapabilitySet {
        network: difference(&left.network, &right.network),
        filesystem: difference(&left.filesystem, &right.filesystem),
        env: difference(&left.env, &right.env),
        secrets: difference(&left.secrets, &right.secrets),
    }
}

/// Validate a trust request against the verified installed artifact and
/// return its canonical preview plus the next snapshot it would publish.
fn plan_trust(
    root: &File,
    current: &StateSnapshot,
    id: &str,
    request: &TrustRequest,
) -> Step<(GrantPreview, StateSnapshot)> {
    let install = current
        .installs
        .iter()
        .find(|install| install.id == id)
        .ok_or(Fail::Reject("extension_not_installed"))?;
    if request.digest != install.digest {
        return Err(Fail::Reject("digest_mismatch"));
    }
    let (metadata, _) = load_artifact(root, install)?;
    let capabilities = canonical_capabilities(&request.capabilities)?;
    validate_capability_set(&capabilities).map_err(|_| Fail::Reject("invalid_capability_grant"))?;
    if !capabilities.network.is_empty() || !capabilities.filesystem.is_empty() {
        return Err(Fail::Reject("declaration_policy_rejected"));
    }
    if !grants_are_subset(&capabilities, &requested_capabilities(&metadata)) {
        return Err(Fail::Reject("grant_widens_manifest"));
    }
    let trust = ArtifactTrustGrant {
        id: install.id.clone(),
        digest: install.digest.clone(),
        capabilities: capabilities.clone(),
    };

    let mut rows = Vec::with_capacity(request.service_grants.len());
    let mut seen = HashSet::new();
    for row in &request.service_grants {
        if !seen.insert(row.service_id.as_str()) {
            return Err(Fail::Reject("invalid_request"));
        }
        let service = metadata
            .services
            .iter()
            .find(|service| service.id == row.service_id)
            .ok_or(Fail::Reject("unknown_service"))?;
        if !row.native_process_ack {
            return Err(Fail::Reject("native_process_ack_required"));
        }
        let mut bindings = row.secret_bindings.clone();
        bindings.sort();
        validate_secret_bindings(&bindings).map_err(|_| Fail::Reject("invalid_secret_binding"))?;
        let grant = ServiceGrant {
            id: install.id.clone(),
            digest: install.digest.clone(),
            service_id: row.service_id.clone(),
            native_process_ack: true,
            secret_bindings: bindings.clone(),
        };
        // Bindings cannot float between services: each must match this
        // service's requested env/secret and the granted capability set.
        validate_binding_authority(&grant, &trust, service)
            .map_err(|_| Fail::Reject("invalid_secret_binding"))?;
        rows.push(ServiceGrantRequest {
            service_id: row.service_id.clone(),
            native_process_ack: true,
            secret_bindings: bindings,
        });
    }
    rows.sort();

    let existing_capabilities = current
        .grants
        .iter()
        .find(|grant| grant.id == install.id && grant.digest == install.digest)
        .map(|grant| canonical_capabilities(&grant.capabilities))
        .transpose()?
        .unwrap_or_default();
    let mut existing_rows: Vec<ServiceGrantRequest> = current
        .service_grants
        .iter()
        .filter(|grant| grant.id == install.id && grant.digest == install.digest)
        .map(|grant| {
            let mut bindings = grant.secret_bindings.clone();
            bindings.sort();
            ServiceGrantRequest {
                service_id: grant.service_id.clone(),
                native_process_ack: grant.native_process_ack,
                secret_bindings: bindings,
            }
        })
        .collect();
    existing_rows.sort();

    let added = GrantSide {
        capabilities: capability_difference(&capabilities, &existing_capabilities),
        service_grants: rows
            .iter()
            .filter(|row| !existing_rows.contains(row))
            .cloned()
            .collect(),
    };
    let removed = GrantSide {
        capabilities: capability_difference(&existing_capabilities, &capabilities),
        service_grants: existing_rows
            .iter()
            .filter(|row| !rows.contains(row))
            .cloned()
            .collect(),
    };
    let confirmation = grant_confirmation(
        &install.id,
        &install.digest,
        current.revision,
        &added,
        &removed,
    )?;

    let mut next = current.clone();
    next.grants
        .retain(|grant| !(grant.id == install.id && grant.digest == install.digest));
    next.grants.push(trust);
    next.service_grants
        .retain(|grant| !(grant.id == install.id && grant.digest == install.digest));
    next.service_grants
        .extend(rows.into_iter().map(|row| ServiceGrant {
            id: install.id.clone(),
            digest: install.digest.clone(),
            service_id: row.service_id,
            native_process_ack: true,
            secret_bindings: row.secret_bindings,
        }));
    Ok((
        GrantPreview {
            added,
            removed,
            native_authority_notice: NATIVE_AUTHORITY_NOTICE,
            confirmation,
        },
        next,
    ))
}

/// `sha256:` over a domain-separated canonical JSON object of id, digest,
/// current revision, the canonical diff, and the exact notice text.
fn grant_confirmation(
    id: &str,
    digest: &str,
    state_revision: u64,
    added: &GrantSide,
    removed: &GrantSide,
) -> Step<String> {
    #[derive(Serialize)]
    struct ConfirmationInput<'a> {
        id: &'a str,
        digest: &'a str,
        state_revision: u64,
        added: &'a GrantSide,
        removed: &'a GrantSide,
        native_authority_notice: &'a str,
    }
    let bytes = serde_json::to_vec(&ConfirmationInput {
        id,
        digest,
        state_revision,
        added,
        removed,
        native_authority_notice: NATIVE_AUTHORITY_NOTICE,
    })
    .map_err(|_| Fail::Reject(STATE_UNAVAILABLE))?;
    let mut hash = Sha256::new();
    hash.update(GRANT_DIFF_DOMAIN);
    hash.update(&bytes);
    Ok(format!("sha256:{}", lowercase_hex(&hash.finalize())))
}

/// Canonicalize, bound, and fully validate the next generation (including its
/// service grants against the anchored store) and render the four files.
fn validate_and_render(root: &File, next: &StateSnapshot) -> Step<Vec<(&'static str, Vec<u8>)>> {
    let mut canonical = next.clone();
    for grant in &mut canonical.grants {
        grant.capabilities = canonical_capabilities(&grant.capabilities)?;
    }
    for grant in &mut canonical.service_grants {
        grant.secret_bindings.sort();
    }
    canonical.service_grants.sort_by(|left, right| {
        (&left.id, &left.digest, &left.service_id).cmp(&(
            &right.id,
            &right.digest,
            &right.service_id,
        ))
    });
    // Sorts installs/grants/enablement/projects and enforces every A0/A2a
    // bound, order, and identity rule the reader will enforce.
    validate_snapshot(&mut canonical)?;
    validate_service_grants_against_artifacts(root, &canonical)?;
    let revision = canonical.revision;
    fn render<T: Serialize>(value: &T) -> Step<Vec<u8>> {
        let mut bytes =
            serde_json::to_vec_pretty(value).map_err(|_| Fail::Reject(STATE_UNAVAILABLE))?;
        bytes.push(b'\n');
        if bytes.len() as u64 > STATE_FILE_LIMIT {
            return Err(Fail::Reject("extension_state_oversized"));
        }
        Ok(bytes)
    }
    let service_grants = render(&ServiceGrantsFile {
        schema_version: STATE_SCHEMA_VERSION,
        state_revision: revision,
        service_grants: canonical.service_grants,
    })?;
    let installs = render(&InstallsFile {
        schema_version: STATE_SCHEMA_VERSION,
        state_revision: revision,
        installs: canonical.installs,
    })?;
    let trust = render(&TrustFile {
        schema_version: STATE_SCHEMA_VERSION,
        state_revision: revision,
        grants: canonical.grants,
    })?;
    let enabled = render(&EnabledFile {
        schema_version: STATE_SCHEMA_VERSION,
        state_revision: revision,
        extensions: canonical.enablement,
    })?;
    Ok(vec![
        (STATE_FILES[0], service_grants),
        (STATE_FILES[1], installs),
        (STATE_FILES[2], trust),
        (STATE_FILES[3], enabled),
    ])
}

fn connection_temp_remains(root: &File, id: &str) -> Step<bool> {
    let id = CString::new(id).map_err(|_| Fail::Reject("invalid_extension_id"))?;
    let Some(state) = open_optional_dir(root, c"state")? else {
        return Ok(false);
    };
    let Some(package) = open_optional_dir(&state, &id)? else {
        return Ok(false);
    };
    let Some(tmp) = open_optional_dir(&package, c"tmp")? else {
        return Ok(false);
    };
    for service in raw_names(&tmp).map_err(unavailable)? {
        match open_dir_nofollow(&tmp, &service) {
            Ok(service) => {
                if !raw_names(&service).map_err(unavailable)?.is_empty() {
                    return Ok(true);
                }
            }
            // A non-directory row is not a live connection root; remove's
            // descriptor-relative cleanup unlinks it without following it.
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOTDIR) | Some(libc::ELOOP) | Some(libc::ENOENT)
                ) => {}
            Err(error) => return Err(unavailable(error)),
        }
    }
    Ok(false)
}

// -------------------------------------------------------------------------
// Acquisition.
// -------------------------------------------------------------------------

#[derive(Default)]
struct CopyBudget {
    entries: usize,
    bytes: u64,
}

fn copy_tree(source: &File, target: &File, depth: usize, budget: &mut CopyBudget) -> Step<()> {
    if depth > MAX_PACKAGE_DEPTH {
        return Err(Fail::Reject(PACKAGE_INVALID));
    }
    let remaining = MAX_PACKAGE_ENTRIES.saturating_sub(budget.entries);
    let names = directory_names(source, remaining).map_err(invalid_package)?;
    for name in names {
        budget.entries += 1;
        if budget.entries > MAX_PACKAGE_ENTRIES {
            return Err(Fail::Reject(PACKAGE_INVALID));
        }
        // O_NOFOLLOW | O_NONBLOCK: symlinks fail, FIFOs cannot block, sockets
        // cannot be opened, and devices fail the type check below.
        let mut entry =
            open_file_at(source, OsStr::new(&name), "package source").map_err(invalid_package)?;
        let before = entry.metadata().map_err(invalid_package)?;
        let target_name = CString::new(name.as_str()).map_err(invalid_package)?;
        if before.is_dir() {
            let child = mkdir_open(target, &target_name, 0o755, false).map_err(unavailable)?;
            copy_tree(&entry, &child, depth + 1, budget)?;
            let after = entry.metadata().map_err(invalid_package)?;
            if metadata_changed(&before, &after) {
                return Err(Fail::Reject(PACKAGE_INVALID));
            }
            fsync_dir(&child).map_err(unavailable)?;
            continue;
        }
        if !before.is_file() || before.nlink() != 1 {
            return Err(Fail::Reject(PACKAGE_INVALID));
        }
        let length = before.len();
        // Reject files with a hole, detected from the filesystem's own hole
        // map rather than allocated blocks, which undercount legitimately
        // compressed files (ZFS/btrfs compression, APFS decmpfs).
        if has_hole(&entry, length).map_err(invalid_package)? {
            return Err(Fail::Reject(PACKAGE_INVALID));
        }
        budget.bytes = budget
            .bytes
            .checked_add(length)
            .ok_or(Fail::Reject(PACKAGE_INVALID))?;
        if budget.bytes > MAX_PACKAGE_BYTES {
            return Err(Fail::Reject(PACKAGE_INVALID));
        }
        let mode = if before.mode() & 0o111 != 0 {
            0o755
        } else {
            0o644
        };
        let mut output = create_file_at(target, &target_name, mode).map_err(unavailable)?;
        let mut observed = 0u64;
        // Heap buffer: recursion depth reaches MAX_PACKAGE_DEPTH, so a stack
        // array per frame would overflow a 2 MiB thread stack.
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let count = entry.read(&mut buffer).map_err(invalid_package)?;
            if count == 0 {
                break;
            }
            observed += count as u64;
            if observed > length {
                return Err(Fail::Reject(PACKAGE_INVALID));
            }
            output.write_all(&buffer[..count]).map_err(unavailable)?;
        }
        let after = entry.metadata().map_err(invalid_package)?;
        if observed != length || metadata_changed(&before, &after) {
            return Err(Fail::Reject(PACKAGE_INVALID));
        }
        output.sync_all().map_err(unavailable)?;
    }
    Ok(())
}

/// True when `SEEK_HOLE` reports a hole before end of file. A filesystem
/// without hole tracking reports only the implicit hole at EOF, so dense
/// files are never rejected; the read position is restored to 0.
fn has_hole(file: &File, length: u64) -> io::Result<bool> {
    if length == 0 {
        return Ok(false);
    }
    // SAFETY: file is a live descriptor; lseek does not touch memory.
    let hole = unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_HOLE) };
    if hole < 0 {
        let error = io::Error::last_os_error();
        // No hole-map support (EINVAL on most filesystems, ENOTSUP on some
        // network mounts): nothing to detect, not a failure.
        if matches!(
            error.raw_os_error(),
            Some(libc::EINVAL) | Some(libc::ENOTSUP)
        ) {
            return Ok(false);
        }
        return Err(error);
    }
    // SAFETY: as above.
    if unsafe { libc::lseek(file.as_raw_fd(), 0, libc::SEEK_SET) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((hole as u64) < length)
}

fn seal_inner(lease: AcquisitionLease) -> Step<VerifiedQuarantine> {
    if lease.source.is_none() {
        return Err(Fail::Reject("invalid_request"));
    }
    let package = snapshot_package(&lease.artifact).map_err(invalid_package)?;
    let raw = RawOceanExtensionManifest::parse(&package.manifest).map_err(invalid_package)?;
    // Host-incompatible but structurally valid packages remain installable
    // and inspectable; enable rejects them.
    let metadata = raw
        .validate_metadata(&Version::new(u64::MAX, u64::MAX, u64::MAX))
        .map_err(invalid_package)?;
    if !metadata_paths_exist(&metadata, &package.entries) {
        return Err(Fail::Reject(PACKAGE_INVALID));
    }
    let id = metadata.id.clone();
    let version = metadata.version.to_string();
    // The lease stays unconsumed until adoption; dropping a sealed quarantine
    // still deletes it.
    Ok(VerifiedQuarantine {
        lease,
        digest: package.digest,
        id,
        version,
    })
}

/// §12.3 step 3: atomically rename quarantine into `staging/<op>`, prove the
/// renamed directory is the retained handle, rehash the artifact, and decide
/// whether the store already holds these verified bytes.
fn adopt(
    root: &File,
    staging_root: &File,
    id: &str,
    quarantine: VerifiedQuarantine,
) -> Step<(File, JournalArtifact)> {
    let VerifiedQuarantine {
        mut lease,
        digest,
        id: package_id,
        ..
    } = quarantine;
    if package_id != id {
        return Err(Fail::Reject("package_identity_mismatch"));
    }
    let operation = lease.operation_name();
    renameat_at(&lease.parent, &operation, staging_root, &operation).map_err(unavailable)?;
    lease.consumed = true;
    // The rename's source directory; the destination is synced by the caller
    // before the prepared journal.
    fsync_dir(&lease.parent).map_err(unavailable)?;
    if let QuarantineHome::Bootstrap { config, name, .. } = &lease.home {
        let _ = remove_tree_at(config, name, 0);
    }
    let staging = open_dir_nofollow(staging_root, &operation).map_err(unavailable)?;
    if !same_file(&staging, &lease.quarantine).map_err(unavailable)? {
        return Err(Fail::Reject(STATE_UNAVAILABLE));
    }
    let artifact = open_dir_nofollow(&staging, c"artifact").map_err(unavailable)?;
    if !same_file(&artifact, &lease.artifact).map_err(unavailable)?
        || snapshot_package(&artifact).map_err(invalid_package)?.digest != digest
    {
        return Err(Fail::Reject(PACKAGE_INVALID));
    }
    let hex = CString::new(digest_hex(&digest).ok_or(Fail::Reject(PACKAGE_INVALID))?)
        .expect("hex has no NUL");
    let id_name = CString::new(id).map_err(|_| Fail::Reject("invalid_extension_id"))?;
    let existing = match open_optional_dir(root, c"store")? {
        Some(store) => match open_optional_dir(&store, &id_name)? {
            Some(id_dir) => open_optional_dir(&id_dir, &hex)?,
            None => None,
        },
        None => None,
    };
    let newly_published = match existing {
        // Reinstall/rollback of retained verified bytes deduplicates; a
        // stored payload that no longer verifies is never adopted or replaced.
        Some(existing) => {
            if snapshot_package(&existing)
                .map(|snapshot| snapshot.digest != digest)
                .unwrap_or(true)
            {
                return Err(Fail::Reject("artifact_store_conflict"));
            }
            false
        }
        None => true,
    };
    Ok((
        staging,
        JournalArtifact {
            digest,
            newly_published,
        },
    ))
}

fn publish_artifact(root: &File, staging: &File, id: &str, digest: &str) -> Step<()> {
    let store = mkdir_open(root, c"store", 0o755, true).map_err(unavailable)?;
    let id_name = CString::new(id).map_err(|_| Fail::Reject("invalid_extension_id"))?;
    let id_dir = mkdir_open(&store, &id_name, 0o755, true).map_err(unavailable)?;
    let hex = CString::new(digest_hex(digest).ok_or(Fail::Reject(PACKAGE_INVALID))?)
        .expect("hex has no NUL");
    renameat_noreplace(staging, c"artifact", &id_dir, &hex).map_err(unavailable)?;
    fsync_dir(&id_dir).map_err(unavailable)?;
    fsync_dir(&store).map_err(unavailable)?;
    fsync_dir(root).map_err(unavailable)?;
    Ok(())
}

// -------------------------------------------------------------------------
// Journal, publication, recovery.
// -------------------------------------------------------------------------

fn write_staged_files(
    directory: &File,
    rendered: &[(&'static str, Vec<u8>)],
) -> Step<Vec<JournalFile>> {
    let mut files = Vec::with_capacity(rendered.len());
    for (name, bytes) in rendered {
        let c_name = CString::new(*name).expect("static name");
        let mut file = create_file_at(directory, &c_name, 0o600).map_err(unavailable)?;
        file.write_all(bytes).map_err(unavailable)?;
        file.sync_all().map_err(unavailable)?;
        files.push(JournalFile {
            name: (*name).to_string(),
            sha256: sha256_hex(bytes),
        });
    }
    fsync_dir(directory).map_err(unavailable)?;
    Ok(files)
}

fn sha256_hex(bytes: &[u8]) -> String {
    lowercase_hex(&Sha256::digest(bytes))
}

fn journal_name(journal: &TransactionJournal) -> CString {
    CString::new(format!("{}.json", journal.operation_id)).expect("uuid has no NUL")
}

fn write_journal(root: &File, journal: &TransactionJournal) -> Step<()> {
    let transactions = mkdir_open(root, c"transactions", 0o700, true).map_err(unavailable)?;
    let bytes = serde_json::to_vec_pretty(journal).map_err(|_| Fail::Reject(STATE_UNAVAILABLE))?;
    let temporary =
        CString::new(format!("{}.json.tmp", journal.operation_id)).expect("uuid has no NUL");
    let _ = unlink_at(&transactions, &temporary);
    let mut file = create_file_at(&transactions, &temporary, 0o600).map_err(unavailable)?;
    file.write_all(&bytes).map_err(unavailable)?;
    file.sync_all().map_err(unavailable)?;
    renameat_at(
        &transactions,
        &temporary,
        &transactions,
        &journal_name(journal),
    )
    .map_err(unavailable)?;
    fsync_dir(&transactions).map_err(unavailable)?;
    fsync_dir(root).map_err(unavailable)?;
    if journal.phase == JournalPhase::Prepared {
        durability_trace("journal-prepared-durable");
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static DURABILITY_TRACE: std::cell::RefCell<Vec<&'static str>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Test-only ordering evidence for fsync barriers; a no-op in production.
fn durability_trace(event: &'static str) {
    #[cfg(test)]
    DURABILITY_TRACE.with(|trace| trace.borrow_mut().push(event));
    let _ = event;
}

fn live_hash(root: &File, name: &'static str) -> Step<Option<String>> {
    match open_regular_file_at(root, OsStr::new(name), name) {
        Ok(mut file) => Ok(Some(sha256_hex(&read_capped(
            &mut file,
            STATE_FILE_LIMIT,
            name,
        )?))),
        Err(StateError::MissingComponent(_)) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn state_file_name(name: &str) -> Option<&'static str> {
    STATE_FILES
        .iter()
        .copied()
        .find(|candidate| *candidate == name)
}

/// The commit point was crossed iff any live state file already holds its
/// journaled next-revision bytes (every next file carries the new revision,
/// so no old file can collide).
fn journal_committed(root: &File, journal: &TransactionJournal) -> bool {
    journal.phase == JournalPhase::Committed
        || journal.files.iter().any(|file| {
            state_file_name(&file.name)
                .and_then(|name| live_hash(root, name).ok().flatten())
                .is_some_and(|hash| hash == file.sha256)
        })
}

fn write_marker(root: &File, scratch: &File, revision: u64) -> Step<()> {
    let bytes = serde_json::to_vec_pretty(&PublicationMarker {
        schema_version: STATE_SCHEMA_VERSION,
        first_state_revision: revision,
    })
    .map_err(|_| Fail::Reject(STATE_UNAVAILABLE))?;
    let temporary =
        CString::new(format!("{PUBLICATION_MARKER}.{}.tmp", Uuid::new_v4())).expect("no NUL");
    let mut file = create_file_at(scratch, &temporary, 0o600).map_err(unavailable)?;
    file.write_all(&bytes).map_err(unavailable)?;
    file.write_all(b"\n").map_err(unavailable)?;
    file.sync_all().map_err(unavailable)?;
    let marker = CString::new(PUBLICATION_MARKER).expect("static name");
    match renameat_noreplace(scratch, &temporary, root, &marker) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {
            let _ = unlink_at(scratch, &temporary);
        }
        Err(error) => return Err(unavailable(error)),
    }
    fsync_dir(root).map_err(unavailable)?;
    Ok(())
}

/// Durable first-publication marker: never removed, created at most once.
fn ensure_marker(root: &File, revision: u64) -> Step<()> {
    match open_regular_file_at(root, OsStr::new(PUBLICATION_MARKER), PUBLICATION_MARKER) {
        Ok(_) => Ok(()),
        Err(StateError::MissingComponent(_)) => {
            let transactions =
                mkdir_open(root, c"transactions", 0o700, true).map_err(unavailable)?;
            write_marker(root, &transactions, revision)
        }
        Err(error) => Err(error.into()),
    }
}

struct RollForward {
    cleanup_pending: bool,
}

/// Roll a journal forward from wherever it stopped. Every step is idempotent:
/// a live file already holding its journaled hash is skipped, any other must
/// come from the verified staged copy, else the committed registry fails
/// closed as `registry_recovery_required`.
fn roll_forward(
    root: &File,
    journal: &TransactionJournal,
    checkpoint: &dyn Fn(CrashPoint) -> Step<()>,
) -> Step<RollForward> {
    let operation = CString::new(journal.operation_id.to_string()).expect("uuid has no NUL");
    let staging = match open_optional_dir(root, c"staging")? {
        Some(staging_root) => open_optional_dir(&staging_root, &operation)?,
        None => None,
    };
    if journal.phase == JournalPhase::Prepared {
        for (index, file) in journal.files.iter().enumerate() {
            let name = state_file_name(&file.name).ok_or(Fail::Reject(RECOVERY_REQUIRED))?;
            if live_hash(root, name)?.as_deref() != Some(file.sha256.as_str()) {
                let staging = staging.as_ref().ok_or(Fail::Reject(RECOVERY_REQUIRED))?;
                let staged = match open_regular_file_at(staging, OsStr::new(name), name) {
                    Ok(mut staged) => sha256_hex(
                        &read_capped(&mut staged, STATE_FILE_LIMIT, name)
                            .map_err(|_| Fail::Reject(RECOVERY_REQUIRED))?,
                    ),
                    Err(_) => return Err(Fail::Reject(RECOVERY_REQUIRED)),
                };
                if staged != file.sha256 {
                    return Err(Fail::Reject(RECOVERY_REQUIRED));
                }
                let c_name = CString::new(name).expect("static name");
                renameat_at(staging, &c_name, root, &c_name)
                    .map_err(|_| Fail::Reject(RECOVERY_REQUIRED))?;
            }
            checkpoint(CrashPoint::AfterStateRename(index))?;
            if index == 0 {
                ensure_marker(root, journal.new_state_revision)?;
                checkpoint(CrashPoint::AfterMarker)?;
            }
        }
    } else {
        ensure_marker(root, journal.new_state_revision)?;
    }
    fsync_dir(root).map_err(unavailable)?;
    checkpoint(CrashPoint::AfterDirectoryFsync)?;

    if journal.operation == OperationKind::Install {
        retire_superseded_cleanups(root, journal)?;
    }
    let cleanup_complete = apply_cleanup(root, journal);
    checkpoint(CrashPoint::AfterRetentionCleanup)?;

    let mut committed = journal.clone();
    committed.phase = JournalPhase::Committed;
    if journal.phase != JournalPhase::Committed {
        write_journal(root, &committed)?;
    }
    checkpoint(CrashPoint::AfterJournalCommitted)?;
    if let Some(staging_root) = open_optional_dir(root, c"staging")? {
        remove_tree_at(&staging_root, &operation, 0).map_err(unavailable)?;
        fsync_dir(&staging_root).map_err(unavailable)?;
    }
    if cleanup_complete {
        if let Some(transactions) = open_optional_dir(root, c"transactions")? {
            match unlink_at(&transactions, &journal_name(journal)) {
                Ok(()) => {}
                // Already retired by a superseding install.
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
                Err(error) => return Err(unavailable(error)),
            }
            fsync_dir(&transactions).map_err(unavailable)?;
        }
    }
    Ok(RollForward {
        cleanup_pending: !cleanup_complete,
    })
}

/// Pre-commit rollback: remove staging and a just-published payload the old
/// generation provably does not reference, then retire the journal.
fn rollback(root: &File, journal: &TransactionJournal) -> Step<()> {
    let operation = CString::new(journal.operation_id.to_string()).expect("uuid has no NUL");
    if let Some(artifact) = journal
        .artifact
        .as_ref()
        .filter(|artifact| artifact.newly_published)
    {
        let installs: Option<InstallsFile> = read_state_json_at(root, "installs.json").ok();
        let referenced = installs.as_ref().is_none_or(|installs| {
            installs.installs.iter().any(|install| {
                install.id == journal.extension_id && install.digest == artifact.digest
            })
        });
        // Without reference proof (unreadable installs) the payload stays: it
        // is non-executable and unreferenced by any journal after this one.
        if !referenced {
            let id_name = CString::new(journal.extension_id.as_str())
                .map_err(|_| Fail::Reject(RECOVERY_REQUIRED))?;
            if let Some(store) = open_optional_dir(root, c"store")? {
                if let Some(id_dir) = open_optional_dir(&store, &id_name)? {
                    if let Some(hex) = digest_hex(&artifact.digest) {
                        let hex = CString::new(hex).expect("hex has no NUL");
                        remove_tree_at(&id_dir, &hex, 0).map_err(unavailable)?;
                        fsync_dir(&id_dir).map_err(unavailable)?;
                    }
                    if raw_names(&id_dir).map_err(unavailable)?.is_empty() {
                        let _ = unlink_dir_at(&store, &id_name);
                        fsync_dir(&store).map_err(unavailable)?;
                    }
                }
            }
        }
    }
    if let Some(staging_root) = open_optional_dir(root, c"staging")? {
        remove_tree_at(&staging_root, &operation, 0).map_err(unavailable)?;
        fsync_dir(&staging_root).map_err(unavailable)?;
    }
    if let Some(transactions) = open_optional_dir(root, c"transactions")? {
        match unlink_at(&transactions, &journal_name(journal)) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
            Err(error) => return Err(unavailable(error)),
        }
        fsync_dir(&transactions).map_err(unavailable)?;
    }
    Ok(())
}

/// Post-commit §12.4 retention. Failures are retried by the next recovery;
/// they never make a coherent registry generation incoherent.
///
/// Retention belongs to the removal that journaled it, never to a later
/// generation of the same id. A retry is therefore gated on the live
/// generation still not installing that id: once the id is installed again
/// (reinstall after a remove whose cleanup stayed pending), its `data/`,
/// `cache/`, `tmp/`, and store payloads belong to the new install, and the
/// stale cleanup is retired without touching them. Unreadable installs defer
/// the retry rather than guessing.
fn apply_cleanup(root: &File, journal: &TransactionJournal) -> bool {
    let cleanup = journal.cleanup;
    if !cleanup.any() {
        return true;
    }
    match read_state_json_at::<InstallsFile>(root, "installs.json") {
        Ok(installs)
            if installs
                .installs
                .iter()
                .any(|install| install.id == journal.extension_id) =>
        {
            return true;
        }
        Ok(_) => {}
        Err(_) => return false,
    }
    let Ok(id) = CString::new(journal.extension_id.as_str()) else {
        return false;
    };
    let mut complete = true;
    if cleanup.purge_store_payloads {
        complete &= purge_unreferenced_payloads(root, &journal.extension_id, &id).is_ok();
    }
    let state = match open_optional_dir(root, c"state") {
        Ok(state) => state,
        Err(_) => return false,
    };
    if let Some(state) = state {
        if cleanup.remove_data {
            complete &= remove_tree_at(&state, &id, 0).is_ok();
        } else {
            match open_optional_dir(&state, &id) {
                Ok(Some(package)) => {
                    if cleanup.remove_cache {
                        complete &= remove_tree_at(&package, c"cache", 0).is_ok();
                    }
                    if cleanup.remove_tmp {
                        complete &= remove_tree_at(&package, c"tmp", 0).is_ok();
                    }
                    complete &= fsync_dir(&package).is_ok();
                }
                Ok(None) => {}
                // A planted non-directory at the package state name is
                // unlinked without being followed.
                Err(_) => complete &= remove_tree_at(&state, &id, 0).is_ok(),
            }
        }
        complete &= fsync_dir(&state).is_ok();
    }
    complete
}

fn purge_unreferenced_payloads(root: &File, id: &str, id_name: &CStr) -> Step<()> {
    let installs: InstallsFile = read_state_json_at(root, "installs.json")?;
    let referenced: HashSet<String> = installs
        .installs
        .iter()
        .filter(|install| install.id == id)
        .filter_map(|install| digest_hex(&install.digest).map(str::to_string))
        .collect();
    let Some(store) = open_optional_dir(root, c"store")? else {
        return Ok(());
    };
    let Some(id_dir) = open_optional_dir(&store, id_name)? else {
        return Ok(());
    };
    for name in raw_names(&id_dir).map_err(unavailable)? {
        let keep = name.to_str().is_ok_and(|name| referenced.contains(name));
        if !keep {
            remove_tree_at(&id_dir, &name, 0).map_err(unavailable)?;
        }
    }
    fsync_dir(&id_dir).map_err(unavailable)?;
    if referenced.is_empty() {
        unlink_dir_at(&store, id_name).map_err(unavailable)?;
    }
    fsync_dir(&store).map_err(unavailable)?;
    Ok(())
}

#[derive(Default)]
struct LockedRecovery {
    rolled_back: usize,
    rolled_forward: usize,
    cleanup_pending: usize,
    orphans_removed: usize,
}

fn parse_journal(transactions: &File, name: &CStr) -> Step<TransactionJournal> {
    let text = name.to_str().map_err(|_| Fail::Reject(RECOVERY_REQUIRED))?;
    let stem = text
        .strip_suffix(".json")
        .ok_or(Fail::Reject(RECOVERY_REQUIRED))?;
    let mut file = open_regular_file_at(transactions, OsStr::new(text), "transaction journal")
        .map_err(|_| Fail::Reject(RECOVERY_REQUIRED))?;
    let bytes = read_capped(&mut file, STATE_FILE_LIMIT, "transaction journal")
        .map_err(|_| Fail::Reject(RECOVERY_REQUIRED))?;
    let journal: TransactionJournal =
        serde_json::from_slice(&bytes).map_err(|_| Fail::Reject(RECOVERY_REQUIRED))?;
    let files_match = journal.files.len() == STATE_FILES.len()
        && journal
            .files
            .iter()
            .zip(STATE_FILES)
            .all(|(file, expected)| file.name == expected && file.sha256.len() == 64);
    if journal.schema_version != JOURNAL_SCHEMA_VERSION
        || journal.operation_id.to_string() != stem
        || validate_extension_id(&journal.extension_id).is_err()
        || journal.new_state_revision != journal.old_state_revision.saturating_add(1)
        || !files_match
        || journal
            .artifact
            .as_ref()
            .is_some_and(|artifact| digest_hex(&artifact.digest).is_none())
    {
        return Err(Fail::Reject(RECOVERY_REQUIRED));
    }
    Ok(journal)
}

#[derive(Debug, PartialEq, Eq)]
enum TransactionEntry {
    Journal,
    Temporary,
    Foreign,
}

fn canonical_uuid(text: &str) -> bool {
    Uuid::parse_str(text).is_ok_and(|uuid| uuid.to_string() == text)
}

/// Only names this writer creates are interpreted: `<uuid>.json` journals,
/// `<uuid>.json.tmp` journal drafts, and `stage-a-publication.json.<uuid>.tmp`
/// marker drafts.
fn classify_transaction_entry(name: &CStr) -> TransactionEntry {
    let Ok(text) = name.to_str() else {
        return TransactionEntry::Foreign;
    };
    if text.strip_suffix(".json.tmp").is_some_and(canonical_uuid)
        || text
            .strip_prefix(PUBLICATION_MARKER)
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.strip_suffix(".tmp"))
            .is_some_and(canonical_uuid)
    {
        return TransactionEntry::Temporary;
    }
    if text.strip_suffix(".json").is_some_and(canonical_uuid) {
        return TransactionEntry::Journal;
    }
    TransactionEntry::Foreign
}

/// A committed install supersedes every earlier committed journal whose
/// retention for the same id is still pending: that retention belonged to a
/// removed generation, and must never later act on this install's state or on
/// a subsequent removal with a different `purge_state` choice.
fn retire_superseded_cleanups(root: &File, journal: &TransactionJournal) -> Step<()> {
    let Some(transactions) = open_optional_dir(root, c"transactions")? else {
        return Ok(());
    };
    for name in raw_names(&transactions).map_err(unavailable)? {
        if classify_transaction_entry(&name) != TransactionEntry::Journal {
            continue;
        }
        let Ok(prior) = parse_journal(&transactions, &name) else {
            // Left for recovery, which fails closed on it.
            continue;
        };
        if prior.operation_id != journal.operation_id
            && prior.phase == JournalPhase::Committed
            && prior.extension_id == journal.extension_id
            && prior.cleanup.any()
        {
            match unlink_at(&transactions, &name) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
                Err(error) => return Err(unavailable(error)),
            }
        }
    }
    fsync_dir(&transactions).map_err(unavailable)?;
    Ok(())
}

/// Runs under the exclusive lock before any mutation and at startup.
fn recover_locked(root: &File) -> Step<LockedRecovery> {
    let mut report = LockedRecovery::default();
    if let Some(transactions) = open_optional_dir(root, c"transactions")? {
        let mut journals = Vec::new();
        for name in raw_names(&transactions).map_err(unavailable)? {
            match classify_transaction_entry(&name) {
                // Never renamed into place: no journal was proven.
                TransactionEntry::Temporary => {
                    unlink_at(&transactions, &name).map_err(unavailable)?
                }
                // A journal-shaped name is authority: malformed content fails
                // closed as `registry_recovery_required`.
                TransactionEntry::Journal => journals.push(parse_journal(&transactions, &name)?),
                // Foreign rows (`.DS_Store`, editor droppings) are not journals
                // and carry no authority; they never block the registry.
                TransactionEntry::Foreign => {}
            }
        }
        journals.sort_by_key(|journal| (journal.new_state_revision, journal.operation_id));
        for journal in journals {
            if journal_committed(root, &journal) {
                let result = roll_forward(root, &journal, &|_| Ok(()))?;
                report.rolled_forward += 1;
                if result.cleanup_pending {
                    report.cleanup_pending += 1;
                }
            } else {
                rollback(root, &journal)?;
                report.rolled_back += 1;
            }
        }
        fsync_dir(&transactions).map_err(unavailable)?;
    }
    // Every journal is resolved, so any remaining staging directory predates
    // its journal (crash before step 4) and is referenced by nothing.
    if let Some(staging) = open_optional_dir(root, c"staging")? {
        for name in raw_names(&staging).map_err(unavailable)? {
            remove_tree_at(&staging, &name, 0).map_err(unavailable)?;
            report.orphans_removed += 1;
        }
        fsync_dir(&staging).map_err(unavailable)?;
    }
    Ok(report)
}

fn acquire_exclusive_lock(file: &File) -> Result<(), StateError> {
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match fs2::FileExt::try_lock_exclusive(file) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(StateError::LockBusy);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return Err(StateError::Read(".state.lock")),
        }
    }
}

// -------------------------------------------------------------------------
// Descriptor-relative filesystem primitives (Unix).
// -------------------------------------------------------------------------

fn cvt(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn open_dir_nofollow(parent: &File, name: &CStr) -> io::Result<File> {
    // SAFETY: parent is a live directory descriptor, name is NUL-terminated,
    // and a successful descriptor is transferred exactly once into File.
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh owned descriptor.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn open_optional_dir(parent: &File, name: &CStr) -> Step<Option<File>> {
    match open_dir_nofollow(parent, name) {
        Ok(directory) => Ok(Some(directory)),
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(error) => Err(unavailable(error)),
    }
}

fn mkdir_open(
    parent: &File,
    name: &CStr,
    mode: libc::mode_t,
    allow_existing: bool,
) -> io::Result<File> {
    // SAFETY: parent is a live directory descriptor and name is NUL-terminated.
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), mode) } == 0;
    if !created {
        let error = io::Error::last_os_error();
        if !(allow_existing && error.raw_os_error() == Some(libc::EEXIST)) {
            return Err(error);
        }
    }
    let directory = open_dir_nofollow(parent, name)?;
    if created {
        // Exact mode regardless of umask.
        // SAFETY: directory is a live descriptor.
        cvt(unsafe { libc::fchmod(directory.as_raw_fd(), mode) })?;
    }
    Ok(directory)
}

fn create_file_at(parent: &File, name: &CStr, mode: libc::mode_t) -> io::Result<File> {
    // SAFETY: parent is live, name is NUL-terminated, and ownership of a
    // successful descriptor transfers exactly once into File.
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            libc::c_uint::from(mode),
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh owned descriptor.
    let file = unsafe { File::from_raw_fd(descriptor) };
    // SAFETY: file is a live descriptor.
    cvt(unsafe { libc::fchmod(file.as_raw_fd(), mode) })?;
    Ok(file)
}

fn renameat_at(from_dir: &File, from: &CStr, to_dir: &File, to: &CStr) -> io::Result<()> {
    // SAFETY: both directory descriptors are live and both names are
    // NUL-terminated components relative to them.
    cvt(unsafe {
        libc::renameat(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
        )
    })
}

#[cfg(target_os = "linux")]
fn renameat_noreplace(from_dir: &File, from: &CStr, to_dir: &File, to: &CStr) -> io::Result<()> {
    // SAFETY: as renameat; RENAME_NOREPLACE refuses an existing target.
    cvt(unsafe {
        libc::renameat2(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    })
}

#[cfg(target_os = "macos")]
fn renameat_noreplace(from_dir: &File, from: &CStr, to_dir: &File, to: &CStr) -> io::Result<()> {
    // SAFETY: as renameat; RENAME_EXCL refuses an existing target.
    cvt(unsafe {
        libc::renameatx_np(
            from_dir.as_raw_fd(),
            from.as_ptr(),
            to_dir.as_raw_fd(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn renameat_noreplace(_: &File, _: &CStr, _: &File, _: &CStr) -> io::Result<()> {
    // No reviewed exclusive-rename primitive: fail closed.
    Err(io::Error::from(io::ErrorKind::Unsupported))
}

fn unlink_at(parent: &File, name: &CStr) -> io::Result<()> {
    // SAFETY: parent is live and name is a NUL-terminated component.
    cvt(unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) })
}

fn unlink_dir_at(parent: &File, name: &CStr) -> io::Result<()> {
    // SAFETY: parent is live and name is a NUL-terminated component.
    cvt(unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) })
}

fn fsync_dir(directory: &File) -> io::Result<()> {
    // SAFETY: directory is a live descriptor.
    if unsafe { libc::fsync(directory.as_raw_fd()) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        // Filesystems without directory fsync support.
        Some(libc::EINVAL) | Some(libc::ENOTSUP) => Ok(()),
        _ => Err(error),
    }
}

fn same_file(left: &File, right: &File) -> io::Result<bool> {
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

struct RawDirectoryStream(*mut libc::DIR);

impl Drop for RawDirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns the stream returned by fdopendir.
        unsafe { libc::closedir(self.0) };
    }
}

/// Every entry name except `.`/`..`, as raw bytes; unlike the package reader it
/// tolerates non-UTF-8 names so cleanup can remove whatever a child planted.
fn raw_names(directory: &File) -> io::Result<Vec<CString>> {
    // SAFETY: F_DUPFD_CLOEXEC gives fdopendir its own descriptor.
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: duplicate is a fresh descriptor; fdopendir owns it on success.
    let raw = unsafe { libc::fdopendir(duplicate) };
    if raw.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir did not take ownership on failure.
        unsafe { libc::close(duplicate) };
        return Err(error);
    }
    let stream = RawDirectoryStream(raw);
    let mut names = Vec::new();
    loop {
        // SAFETY: stream owns a valid DIR pointer.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            break;
        }
        // SAFETY: d_name is NUL-terminated for this row and copied before the
        // next readdir call.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_owned();
        if !matches!(name.to_bytes(), b"." | b"..") {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Descriptor-relative recursive removal that never follows a symlink: a
/// non-directory row (including a symlink) is unlinked itself.
fn remove_tree_at(parent: &File, name: &CStr, depth: usize) -> io::Result<()> {
    if depth > MAX_REMOVAL_DEPTH {
        return Err(io::Error::other("removal depth exceeded"));
    }
    let mut status = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: parent/name are live; status is writable.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            status.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(error)
        };
    }
    // SAFETY: fstatat initialized status on success.
    let status = unsafe { status.assume_init() };
    if status.st_mode & libc::S_IFMT == libc::S_IFDIR {
        let child = open_dir_nofollow(parent, name)?;
        for entry in raw_names(&child)? {
            remove_tree_at(&child, &entry, depth + 1)?;
        }
        match unlink_dir_at(parent, name) {
            Err(error) if error.raw_os_error() != Some(libc::ENOENT) => return Err(error),
            _ => {}
        }
    } else {
        match unlink_at(parent, name) {
            Err(error) if error.raw_os_error() != Some(libc::ENOENT) => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::path::Path;
    use std::sync::atomic::Ordering;

    const ID: &str = "example.noop";
    const DEFAULT_CAPABILITIES: &str =
        "env = [\"NOOP_TOKEN\"]\nsecrets = [\"env:OCEAN_NOOP_TOKEN\"]\n";

    struct Stopped;
    impl ServiceActivity for Stopped {
        fn package_stopped(&self, _: &str) -> bool {
            true
        }
    }

    struct Running;
    impl ServiceActivity for Running {
        fn package_stopped(&self, _: &str) -> bool {
            false
        }
    }

    struct Harness {
        config: tempfile::TempDir,
        sources: tempfile::TempDir,
        marker: PathBuf,
        writer: RegistryWriter,
    }

    impl Harness {
        fn new() -> Self {
            let config = tempfile::tempdir().unwrap();
            let sources = tempfile::tempdir().unwrap();
            let marker = sources.path().join("PACKAGE_CODE_RAN");
            let writer = RegistryWriter::new(config.path().to_path_buf());
            Self {
                config,
                sources,
                marker,
                writer,
            }
        }

        fn root(&self) -> PathBuf {
            self.config.path().join("extensions")
        }

        /// A no-op package whose every resource path holds an executable
        /// canary. Nothing in A3a may ever run one.
        fn package(&self, name: &str, version: &str, min: &str, capabilities: &str) -> String {
            let dir = self.sources.path().join(name);
            fs::create_dir_all(dir.join("services")).unwrap();
            fs::create_dir_all(dir.join("plugins/reader")).unwrap();
            fs::create_dir_all(dir.join("hooks")).unwrap();
            fs::create_dir_all(dir.join("empty")).unwrap();
            fs::write(
                dir.join("ocean-extension.toml"),
                format!(
                    "schema_version = 1\nid = \"{ID}\"\nname = \"Noop\"\nversion = \"{version}\"\nmin_ocean_version = \"{min}\"\n\n[[plugins]]\nid = \"reader\"\npath = \"plugins/reader\"\n\n[[services]]\nid = \"lifecycle\"\nentry = \"services/lifecycle\"\nevents = [\"turn_started\"]\n[services.capabilities]\n{capabilities}"
                ),
            )
            .unwrap();
            for canary in [
                "services/lifecycle",
                "plugins/reader/run",
                "hooks/on-install",
                "run-me",
            ] {
                let path = dir.join(canary);
                fs::write(
                    &path,
                    format!("#!/bin/sh\nprintf ran > '{}'\n", self.marker.display()),
                )
                .unwrap();
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            }
            dir.to_str().unwrap().to_string()
        }

        fn noop(&self, name: &str, version: &str) -> String {
            self.package(name, version, "0.1.0", DEFAULT_CAPABILITIES)
        }

        fn acquire(&self, path: &str) -> VerifiedQuarantine {
            self.writer.acquire_local(path).unwrap()
        }

        fn install(&self, expected: u64, path: &str) -> MutationOutcome {
            let quarantine = self.acquire(path);
            self.writer.install(expected, quarantine).unwrap()
        }

        fn trust_request(&self, expected: u64, digest: &str) -> TrustRequest {
            TrustRequest {
                expected_state_revision: expected,
                digest: digest.to_string(),
                capabilities: CapabilitySet {
                    env: vec!["NOOP_TOKEN".into()],
                    secrets: vec!["env:OCEAN_NOOP_TOKEN".into()],
                    ..CapabilitySet::default()
                },
                service_grants: vec![ServiceGrantRequest {
                    service_id: "lifecycle".into(),
                    native_process_ack: true,
                    secret_bindings: vec![SecretBinding {
                        target_env: "NOOP_TOKEN".into(),
                        reference: "env:OCEAN_NOOP_TOKEN".into(),
                    }],
                }],
                confirm_grant_diff: None,
            }
        }

        fn preview(&self, request: TrustRequest) -> Result<GrantPreview, MutationError> {
            match self.writer.trust(ID, request)? {
                TrustResult::Preview { preview, .. } => Ok(*preview),
                TrustResult::Applied(_) => panic!("preview applied"),
            }
        }

        fn apply(&self, mut request: TrustRequest) -> Result<MutationOutcome, MutationError> {
            let preview = self.preview(request.clone())?;
            request.confirm_grant_diff = Some(preview.confirmation);
            match self.writer.trust(ID, request)? {
                TrustResult::Applied(outcome) => Ok(outcome),
                TrustResult::Preview { .. } => panic!("apply previewed"),
            }
        }

        fn trust(&self, expected: u64, digest: &str) -> MutationOutcome {
            self.apply(self.trust_request(expected, digest)).unwrap()
        }

        fn state(&self) -> Result<LockedState, StateError> {
            read_locked_state(self.config.path())
        }

        fn revision(&self) -> u64 {
            self.state().unwrap().snapshot.revision
        }

        fn inspect(&self) -> ExtensionInspection {
            let state = self.state().unwrap();
            inspect_extension(
                &state,
                ID,
                None,
                &HashSet::new(),
                &Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
            )
            .unwrap()
        }

        /// Exact bytes of the four state files plus the marker.
        fn bytes(&self) -> BTreeMap<&'static str, Option<Vec<u8>>> {
            STATE_FILES
                .iter()
                .copied()
                .chain([PUBLICATION_MARKER])
                .map(|name| (name, fs::read(self.root().join(name)).ok()))
                .collect()
        }

        fn entries(&self, relative: &str) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(self.root().join(relative))
                .map(|entries| {
                    entries
                        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }

        fn store_payload(&self, digest: &str) -> PathBuf {
            self.root()
                .join("store")
                .join(ID)
                .join(digest_hex(digest).unwrap())
        }

        fn assert_no_transaction_residue(&self) {
            assert!(self.entries("staging").is_empty(), "staging residue");
            assert!(self.entries("transactions").is_empty(), "journal residue");
            assert!(self.entries("quarantine").is_empty(), "quarantine residue");
        }

        fn assert_no_code_ran(&self) {
            assert!(!self.marker.exists(), "package code executed");
        }

        /// Accepted A0 three-file generation with no companion and no marker.
        fn write_a0(&self, revision: u64) {
            let root = self.root();
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join(".state.lock"), "").unwrap();
            for (name, key) in [
                ("installs.json", "installs"),
                ("trust.json", "grants"),
                ("enabled.json", "extensions"),
            ] {
                fs::write(
                    root.join(name),
                    serde_json::to_vec_pretty(&json!({
                        "schema_version": 1,
                        "state_revision": revision,
                        key: []
                    }))
                    .unwrap(),
                )
                .unwrap();
            }
        }

        fn seed_state_roots(&self) {
            let package = self.root().join("state").join(ID);
            fs::create_dir_all(package.join("data")).unwrap();
            fs::create_dir_all(package.join("cache")).unwrap();
            fs::create_dir_all(package.join("tmp/lifecycle")).unwrap();
            fs::write(package.join("data/kept"), "data").unwrap();
            fs::write(package.join("cache/purgeable"), "cache").unwrap();
        }
    }

    fn code<T: std::fmt::Debug>(result: Result<T, MutationError>) -> &'static str {
        result.expect_err("mutation must fail").code
    }

    #[test]
    fn a3a_install_trust_enable_are_distinct_and_execute_no_package_code() {
        let h = Harness::new();
        let source = h.noop("v1", "1.0.0");
        let installed = h.install(0, &source);
        assert_eq!(installed.state_revision, 1);
        assert!(installed.committed && !installed.effective);
        let digest = installed.digest.clone().unwrap();
        let inspection = h.inspect();
        assert!(inspection.installed && inspection.artifact_verified);
        assert!(!inspection.trusted && !inspection.enabled && !inspection.effective);

        // Preview mutates nothing; a wrong confirmation fails pre-commit.
        let before = h.bytes();
        let preview = h.preview(h.trust_request(1, &digest)).unwrap();
        assert_eq!(preview.native_authority_notice, NATIVE_AUTHORITY_NOTICE);
        assert_eq!(h.bytes(), before);
        let mut wrong = h.trust_request(1, &digest);
        wrong.confirm_grant_diff = Some(format!("sha256:{}", "0".repeat(64)));
        let error = h.writer.trust(ID, wrong).unwrap_err();
        assert_eq!(
            (error.code, error.committed, error.state_revision),
            ("grant_confirmation_mismatch", false, 1)
        );
        assert_eq!(h.bytes(), before);

        let trusted = h.trust(1, &digest);
        assert_eq!(trusted.state_revision, 2);
        assert!(!trusted.effective);
        let inspection = h.inspect();
        assert!(inspection.trusted && !inspection.enabled);
        assert_eq!(h.state().unwrap().service_authority.len(), 1);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        assert!(read_service_activations(h.config.path(), &HashSet::new())
            .unwrap()
            .is_empty());

        let enabled = h
            .writer
            .enable(ID, 2, EnablementScope::Global, &HashSet::new())
            .unwrap();
        assert!(enabled.effective);
        assert_eq!(enabled.state_revision, 3);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            let activations = read_service_activations(h.config.path(), &HashSet::new()).unwrap();
            assert_eq!(activations.len(), 1);
            assert_eq!(activations[0].secret_bindings[0].target_env, "NOOP_TOKEN");
        }

        // The merged Phase 1 strict schemas still parse every written file.
        let root = File::open(h.root()).unwrap();
        let installs: InstallsFile = read_state_json_at(&root, "installs.json").unwrap();
        let trust: TrustFile = read_state_json_at(&root, "trust.json").unwrap();
        let enabled: EnabledFile = read_state_json_at(&root, "enabled.json").unwrap();
        assert_eq!(
            (
                installs.state_revision,
                trust.state_revision,
                enabled.state_revision
            ),
            (3, 3, 3)
        );
        assert!(installs.installs[0].source.revision.is_none());
        h.assert_no_transaction_residue();
        h.assert_no_code_ran();
    }

    #[test]
    fn a3a_first_publication_marker_makes_later_companion_absence_fail_closed() {
        let h = Harness::new();
        h.write_a0(5);
        // Existing accepted A0 registries are untouched: absence is the empty
        // grant set and nothing is written by reading.
        let state = h.state().unwrap();
        assert_eq!(state.snapshot.revision, 5);
        assert!(state.snapshot.service_grants.is_empty());
        drop(state);
        assert!(!h.root().join("service-grants.json").exists());
        assert!(!h.root().join(PUBLICATION_MARKER).exists());

        let source = h.noop("v1", "1.0.0");
        let outcome = h.install(5, &source);
        assert_eq!(outcome.state_revision, 6);
        let marker: Value =
            serde_json::from_slice(&fs::read(h.root().join(PUBLICATION_MARKER)).unwrap()).unwrap();
        assert_eq!(
            marker,
            json!({"schema_version": 1, "first_state_revision": 6})
        );
        let grants: Value =
            serde_json::from_slice(&fs::read(h.root().join("service-grants.json")).unwrap())
                .unwrap();
        assert_eq!(
            grants,
            json!({"schema_version": 1, "state_revision": 6, "service_grants": []})
        );

        // After the first publication, absence is incomplete state, not the
        // A0 upgrade form, for both the reader and the writer.
        let saved = fs::read(h.root().join("service-grants.json")).unwrap();
        fs::remove_file(h.root().join("service-grants.json")).unwrap();
        assert_eq!(
            h.state().err(),
            Some(StateError::MissingComponent("service-grants.json"))
        );
        assert_eq!(
            code(
                h.writer
                    .disable(ID, 6, EnablementScope::Global, &HashSet::new())
            ),
            "extension_state_incomplete"
        );
        fs::write(h.root().join("service-grants.json"), &saved).unwrap();
        assert_eq!(h.revision(), 6);

        // A second publication never rewrites the durable marker.
        h.writer
            .disable(ID, 6, EnablementScope::Global, &HashSet::new())
            .unwrap();
        let marker: Value =
            serde_json::from_slice(&fs::read(h.root().join(PUBLICATION_MARKER)).unwrap()).unwrap();
        assert_eq!(marker["first_state_revision"], 6);

        // The marker itself is strict.
        for invalid in [
            json!({"schema_version": 1, "first_state_revision": 0}),
            json!({"schema_version": 1, "first_state_revision": 99}),
            json!({"schema_version": 2, "first_state_revision": 6}),
            json!({"schema_version": 1, "first_state_revision": 6, "extra": true}),
        ] {
            fs::write(
                h.root().join(PUBLICATION_MARKER),
                serde_json::to_vec(&invalid).unwrap(),
            )
            .unwrap();
            assert!(h.state().is_err(), "accepted marker {invalid}");
        }
    }

    #[test]
    fn a3a_bootstrap_publication_is_one_atomic_rename() {
        let h = Harness::new();
        let source = h.noop("v1", "1.0.0");

        // An acquisition in flight never creates a partial registry root.
        let lease = h.writer.begin_acquisition().unwrap();
        assert!(!h.root().exists());
        assert_eq!(h.revision(), 0);
        let bootstrap = h
            .config
            .path()
            .join(format!("{BOOTSTRAP_PREFIX}{}", lease.operation_id()));
        assert!(bootstrap.is_dir());
        drop(lease);
        assert!(!bootstrap.exists());

        h.writer.crash_at(Some(CrashPoint::BeforeRootPublish));
        let quarantine = h.acquire(&source);
        assert_eq!(code(h.writer.install(0, quarantine)), "simulated_crash");
        assert!(!h.root().exists());
        assert_eq!(h.revision(), 0);
        h.writer.crash_at(None);
        let report = h.writer.recover().unwrap();
        assert_eq!(report.orphans_removed, 1);
        assert!(fs::read_dir(h.config.path()).unwrap().next().is_none());

        h.writer.crash_at(Some(CrashPoint::AfterRootPublish));
        let quarantine = h.acquire(&source);
        assert_eq!(code(h.writer.install(0, quarantine)), "simulated_crash");
        h.writer.crash_at(None);
        assert_eq!(h.revision(), 1);
        assert!(h.root().join(PUBLICATION_MARKER).is_file());
        h.writer.recover().unwrap();
        assert_eq!(h.revision(), 1);
        assert_eq!(
            code(h.writer.install(0, h.acquire(&source))),
            REVISION_CONFLICT
        );
        assert_eq!(
            code(h.writer.install(1, h.acquire(&source))),
            "already_installed"
        );
        h.assert_no_transaction_residue();
        h.assert_no_code_ran();
    }

    #[test]
    fn a3a_bootstrap_acquisitions_adopt_into_a_root_published_meanwhile() {
        let h = Harness::new();
        let first = h.noop("first", "1.0.0");
        let other = h.noop("other", "1.0.0");
        let manifest = Path::new(&other).join("ocean-extension.toml");
        fs::write(
            &manifest,
            fs::read_to_string(&manifest)
                .unwrap()
                .replace("example.noop", "example.other"),
        )
        .unwrap();
        // Both acquisitions start before any registry root exists.
        let first = h.acquire(&first);
        let other = h.acquire(&other);
        assert!(!h.root().exists());
        h.writer.install(0, first).unwrap();
        // The second is adopted from its bootstrap home into the new root.
        let outcome = h.writer.install(1, other).unwrap();
        assert_eq!(outcome.state_revision, 2);
        assert_eq!(h.state().unwrap().snapshot.installs.len(), 2);
        assert!(h.root().join("store/example.other").is_dir());
        let leftovers: Vec<_> = fs::read_dir(h.config.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().starts_with(BOOTSTRAP_PREFIX))
            .collect();
        assert!(leftovers.is_empty());
        h.assert_no_transaction_residue();
        h.assert_no_code_ran();
    }

    #[test]
    fn a3a_expected_revision_and_exclusive_lock_serialize_writers() {
        let h = Harness::new();
        h.install(0, &h.noop("v1", "1.0.0"));
        let before = h.bytes();
        let error = h
            .writer
            .disable(ID, 0, EnablementScope::Global, &HashSet::new())
            .unwrap_err();
        assert_eq!(
            (error.code, error.committed, error.state_revision),
            (REVISION_CONFLICT, false, 1)
        );
        assert_eq!(h.bytes(), before);

        // A coherent shared reader holds off the exclusive writer (bounded).
        let reader = h.state().unwrap();
        assert_eq!(
            code(
                h.writer
                    .disable(ID, 1, EnablementScope::Global, &HashSet::new())
            ),
            "extension_state_busy"
        );
        drop(reader);

        // The writer's exclusive lock holds off readers (bounded).
        let lock = File::open(h.root().join(".state.lock")).unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        assert_eq!(h.state().err(), Some(StateError::LockBusy));
        assert_eq!(
            code(
                h.writer
                    .disable(ID, 1, EnablementScope::Global, &HashSet::new())
            ),
            "extension_state_busy"
        );
        drop(lock);
        assert_eq!(h.bytes(), before);
        h.writer
            .disable(ID, 1, EnablementScope::Global, &HashSet::new())
            .unwrap();
        assert_eq!(h.revision(), 2);
    }

    async fn route_json(app: axum::Router, uri: &str) -> (axum::http::StatusCode, Value) {
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let response = app
            .oneshot(
                axum::http::Request::get(uri)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// §12.3 / §19.4: a long-running fetch holds a permit and quarantine for
    /// the whole interval while list, inspect, and doctor keep answering from
    /// coherent shared-lock reads, a mutation commits, and three further
    /// acquisitions proceed. The fetch is released only after every read.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a3a_acquisition_runs_outside_the_state_lock_with_four_permits() {
        use crate::tests::{fake_convene_state, TestEnvRestore, AUTO_CONVENE_ENV_LOCK};
        use axum::routing::get;

        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
        let h = Harness::new();
        h.install(0, &h.noop("v1", "1.0.0"));
        let second = h.noop("v2", "2.0.0");
        let third = h.noop("v3", "3.0.0");
        let app = axum::Router::new()
            .route("/v1/extensions/{id}/inspect", get(super::super::inspect))
            .route("/v1/extensions/{id}/doctor", get(super::super::doctor))
            .with_state(fake_convene_state(&h.config));

        // The held-open fetch: begun on its own thread, it keeps its permit
        // and quarantine until explicitly released (60 s ceiling).
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (started, ready) = std::sync::mpsc::channel::<Uuid>();
        let (finished, joined) = std::sync::mpsc::channel::<()>();
        let config = h.config.path().to_path_buf();
        let fetch = std::thread::spawn(move || {
            let writer = RegistryWriter::new(config);
            let lease = writer.begin_acquisition().unwrap();
            started.send(lease.operation_id()).unwrap();
            wait.recv_timeout(Duration::from_secs(60)).unwrap();
            drop(lease);
            finished.send(()).unwrap();
        });
        let held = ready.recv().unwrap();
        let quarantined = h.root().join("quarantine").join(held.to_string());
        assert!(quarantined.is_dir());

        // Nobody holds `.state.lock`: it can be taken exclusively.
        let lock = File::open(h.root().join(".state.lock")).unwrap();
        fs2::FileExt::try_lock_exclusive(&lock).unwrap();
        drop(lock);
        // list: the coherent reader list will serve (A3b adds its route).
        let listed = h.state().unwrap();
        assert_eq!(listed.snapshot.installs.len(), 1);
        drop(listed);
        let (status, body) = route_json(app.clone(), &format!("/v1/extensions/{ID}/inspect")).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body["extension"]["state_revision"], 1);
        let (status, body) = route_json(app.clone(), &format!("/v1/extensions/{ID}/doctor")).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body["checks"]["coherent_state"], true);
        assert_eq!(body["checks"]["package_code_executed"], false);
        // A mutation commits while the fetch is still open.
        h.writer
            .disable(ID, 1, EnablementScope::Global, &HashSet::new())
            .unwrap();
        // Three further acquisitions proceed; a fifth is refused, not queued.
        let a = h.acquire(&second);
        let b = h.acquire(&third);
        let c = h.writer.begin_acquisition().unwrap();
        assert_eq!(
            h.writer.begin_acquisition().err().map(|error| error.code),
            Some("acquisition_capacity")
        );
        assert_eq!(h.entries("quarantine").len(), 4);
        // Reads after the mutation still answer while the fetch is held.
        let (status, body) = route_json(app.clone(), &format!("/v1/extensions/{ID}/doctor")).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body["extension"]["state_revision"], 2);
        assert!(quarantined.is_dir(), "held fetch must survive every read");
        assert!(joined.try_recv().is_err(), "fetch released early");

        release.send(()).unwrap();
        joined.recv_timeout(Duration::from_secs(10)).unwrap();
        fetch.join().unwrap();
        drop((a, b, c));
        assert!(h.entries("quarantine").is_empty());
        assert!(h.writer.begin_acquisition().is_ok());
        h.assert_no_code_ran();
    }

    #[test]
    fn a3a_recovery_sweep_and_acquisitions_share_one_registry_gate() {
        // A second writer instance for the same registry sees the first
        // one's live acquisitions: its sweep skips them and the permit cap is
        // shared.
        let h = Harness::new();
        let other = RegistryWriter::new(h.config.path().to_path_buf());
        let bootstrap = h.writer.begin_acquisition().unwrap();
        let bootstrap_dir = h
            .config
            .path()
            .join(format!("{BOOTSTRAP_PREFIX}{}", bootstrap.operation_id()));
        assert_eq!(other.recover().unwrap().orphans_removed, 0);
        assert!(bootstrap_dir.is_dir());
        drop(bootstrap);

        h.install(0, &h.noop("v1", "1.0.0"));
        let leases: Vec<_> = (0..4)
            .map(|_| h.writer.begin_acquisition().unwrap())
            .collect();
        assert_eq!(
            other.begin_acquisition().err().map(|error| error.code),
            Some("acquisition_capacity")
        );
        assert_eq!(other.recover().unwrap().orphans_removed, 0);
        assert_eq!(h.entries("quarantine").len(), 4);
        drop(leases);

        // While a sweep holds the gate, an acquisition waits rather than
        // creating a quarantine the sweep could delete mid-copy.
        let claim = SweepClaim::try_claim(&h.writer.gate_key()).unwrap();
        let (done, begun) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let lease = other.begin_acquisition().unwrap();
                let alive = h
                    .root()
                    .join("quarantine")
                    .join(lease.operation_id().to_string())
                    .is_dir();
                done.send(alive).unwrap();
            });
            assert!(begun.recv_timeout(Duration::from_millis(200)).is_err());
            assert!(h.entries("quarantine").is_empty());
            drop(claim);
            assert!(begun.recv_timeout(Duration::from_secs(10)).unwrap());
        });
        // A sweep cannot be claimed while an acquisition is live.
        let lease = h.writer.begin_acquisition().unwrap();
        assert!(SweepClaim::try_claim(&h.writer.gate_key()).is_none());
        drop(lease);
        assert!(SweepClaim::try_claim(&h.writer.gate_key()).is_some());
    }

    fn running_as_root() -> bool {
        // SAFETY: geteuid has no preconditions.
        unsafe { libc::geteuid() == 0 }
    }

    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn a3a_pending_cleanup_never_touches_a_reinstalled_generation() {
        if running_as_root() {
            return; // Permission obstacles do not bind root.
        }
        let h = Harness::new();
        let v1 = h.noop("v1", "1.0.0");
        h.install(0, &v1);
        h.seed_state_roots();
        let package = h.root().join("state").join(ID);
        let cache = package.join("cache");
        set_mode(&cache, 0o500);

        // remove(purge_state=true) commits, but its retention is blocked.
        let removed = h.writer.remove(ID, 1, true, &Stopped).unwrap();
        assert!(removed.committed && removed.retention_cleanup_pending);
        assert_eq!(h.entries("transactions").len(), 1);
        assert!(package.join("data/kept").is_file());
        // Still blocked: each recovery retries and keeps the journal.
        assert_eq!(h.writer.recover().unwrap().cleanup_pending, 1);

        // Reinstall, trust, and enable the same id. The install retires the
        // superseded retention; the reinstalled generation owns state/<id>.
        let digest = h.install(2, &v1).digest.unwrap();
        assert!(h.entries("transactions").is_empty());
        h.trust(3, &digest);
        h.writer
            .enable(ID, 4, EnablementScope::Global, &HashSet::new())
            .unwrap();
        fs::write(package.join("data/new-generation"), "live").unwrap();
        fs::create_dir_all(package.join("tmp/lifecycle/live-connection")).unwrap();

        // The obstacle clears; later mutations must not purge live state.
        set_mode(&cache, 0o700);
        h.writer
            .disable(ID, 5, EnablementScope::Global, &HashSet::new())
            .unwrap();
        h.writer.recover().unwrap();
        assert!(package.join("data/new-generation").is_file());
        assert!(package.join("data/kept").is_file());
        assert!(package.join("tmp/lifecycle/live-connection").is_dir());
        assert!(cache.is_dir());

        // A later remove(purge_state=false) keeps data even though the first
        // remove had asked for a purge.
        fs::remove_dir_all(package.join("tmp/lifecycle/live-connection")).unwrap();
        h.writer.remove(ID, 6, false, &Stopped).unwrap();
        h.writer.recover().unwrap();
        assert!(package.join("data/new-generation").is_file());
        assert!(!cache.exists());
        h.assert_no_transaction_residue();
    }

    #[test]
    fn a3a_pending_cleanup_is_retired_when_the_id_is_installed_again() {
        // The gate alone: an install that crashed after its renames but
        // before retiring superseded cleanups still protects the new state.
        if running_as_root() {
            return;
        }
        let h = Harness::new();
        let v1 = h.noop("v1", "1.0.0");
        h.install(0, &v1);
        h.seed_state_roots();
        let package = h.root().join("state").join(ID);
        set_mode(&package.join("cache"), 0o500);
        assert!(
            h.writer
                .remove(ID, 1, false, &Stopped)
                .unwrap()
                .retention_cleanup_pending
        );

        h.writer.crash_at(Some(CrashPoint::AfterDirectoryFsync));
        assert_eq!(code(h.writer.install(2, h.acquire(&v1))), "simulated_crash");
        h.writer.crash_at(None);
        assert_eq!(h.entries("transactions").len(), 2);
        fs::create_dir_all(package.join("tmp/lifecycle/live-connection")).unwrap();
        set_mode(&package.join("cache"), 0o700);

        // The older remove journal is recovered first: the id is installed
        // again, so it retires without touching the new generation.
        h.writer.recover().unwrap();
        assert_eq!(h.revision(), 3);
        assert!(package.join("tmp/lifecycle/live-connection").is_dir());
        assert!(package.join("cache").is_dir());
        h.assert_no_transaction_residue();
    }

    #[test]
    fn a3a_holes_are_rejected_by_hole_map_not_block_count() {
        let h = Harness::new();
        // A dense, highly compressible file is legitimate on every
        // filesystem, compressed or not.
        let dense = h.noop("dense", "1.0.0");
        fs::write(Path::new(&dense).join("zeros"), vec![0u8; 1024 * 1024]).unwrap();
        drop(h.writer.acquire_local(&dense).unwrap());

        let sparse = h.noop("sparse", "1.0.0");
        let path = Path::new(&sparse).join("holey");
        {
            // Writing past EOF leaves a real hole (APFS, ext4, tmpfs, ...);
            // extending with set_len alone may be materialized as zeros.
            use std::io::{Seek, SeekFrom};
            let mut file = File::create(&path).unwrap();
            file.write_all(b"start").unwrap();
            file.seek(SeekFrom::Start(64 * 1024 * 1024)).unwrap();
            file.write_all(b"end").unwrap();
        }
        let probe = File::open(&path).unwrap();
        let holey = has_hole(&probe, probe.metadata().unwrap().len()).unwrap();
        if cfg!(any(target_os = "macos", target_os = "linux")) {
            assert!(holey, "fixture must contain a hole on supported platforms");
        } else if !holey {
            return;
        }
        assert_eq!(
            h.writer
                .acquire_local(&sparse)
                .err()
                .map(|error| error.code),
            Some(PACKAGE_INVALID)
        );
        assert!(h.entries("quarantine").is_empty());
    }

    #[test]
    fn a3a_staging_is_durable_before_the_prepared_journal() {
        let trace = || DURABILITY_TRACE.with(|trace| trace.borrow().clone());
        let h = Harness::new();
        h.write_a0(1);
        for step in 0..2 {
            DURABILITY_TRACE.with(|trace| trace.borrow_mut().clear());
            if step == 0 {
                // Adopted quarantine.
                h.install(1, &h.noop("v1", "1.0.0"));
            } else {
                // Created staging directory.
                h.writer
                    .disable(ID, 2, EnablementScope::Global, &HashSet::new())
                    .unwrap();
            }
            let events = trace();
            let synced = events
                .iter()
                .position(|event| *event == "staging-root-synced");
            let journal = events
                .iter()
                .position(|event| *event == "journal-prepared-durable");
            assert!(
                synced.is_some() && synced < journal,
                "step {step}: {events:?}"
            );
        }
    }

    #[test]
    fn a3a_foreign_transaction_rows_are_ignored_but_journal_names_fail_closed() {
        let h = Harness::new();
        h.install(0, &h.noop("v1", "1.0.0"));
        let transactions = h.root().join("transactions");
        fs::create_dir_all(&transactions).unwrap();
        for foreign in [
            ".DS_Store",
            "notes.json",
            "editor.swp.tmp",
            "not-a-uuid.json.tmp",
        ] {
            fs::write(transactions.join(foreign), "foreign").unwrap();
        }
        let draft = transactions.join(format!("{}.json.tmp", Uuid::new_v4()));
        fs::write(&draft, "{").unwrap();
        h.writer
            .disable(ID, 1, EnablementScope::Global, &HashSet::new())
            .unwrap();
        assert!(!draft.exists(), "journal drafts are discarded");
        assert_eq!(h.entries("transactions").len(), 4);
        assert_eq!(h.writer.recover().unwrap().rolled_back, 0);

        let journal = transactions.join(format!("{}.json", Uuid::new_v4()));
        fs::write(&journal, "{\"schema_version\":1}").unwrap();
        assert_eq!(
            code(
                h.writer
                    .disable(ID, 2, EnablementScope::Global, &HashSet::new())
            ),
            RECOVERY_REQUIRED
        );
        fs::remove_file(journal).unwrap();
        h.writer
            .disable(ID, 2, EnablementScope::Global, &HashSet::new())
            .unwrap();
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Scenario {
        Install,
        Update,
        Trust,
        Enable,
        Disable,
        Remove,
    }

    struct Prepared {
        h: Harness,
        next_digest: Option<String>,
        old_digest: Option<String>,
        pending: Option<VerifiedQuarantine>,
    }

    fn prepare(scenario: Scenario) -> Prepared {
        let h = Harness::new();
        let v1 = h.noop("v1", "1.0.0");
        let mut old_digest = None;
        let mut pending = None;
        let mut next_digest = None;
        match scenario {
            Scenario::Install => {
                // First Stage A publication over an accepted A0 registry.
                h.write_a0(3);
                let quarantine = h.acquire(&v1);
                next_digest = Some(quarantine.digest().to_string());
                pending = Some(quarantine);
            }
            _ => {
                let digest = h.install(0, &v1).digest.unwrap();
                old_digest = Some(digest.clone());
                match scenario {
                    Scenario::Update => {
                        h.trust(1, &digest);
                        let quarantine = h.acquire(&h.noop("v2", "2.0.0"));
                        next_digest = Some(quarantine.digest().to_string());
                        pending = Some(quarantine);
                    }
                    Scenario::Enable => {
                        h.trust(1, &digest);
                    }
                    Scenario::Disable => {
                        h.trust(1, &digest);
                        h.writer
                            .enable(ID, 2, EnablementScope::Global, &HashSet::new())
                            .unwrap();
                    }
                    Scenario::Remove => h.seed_state_roots(),
                    Scenario::Trust | Scenario::Install => {}
                }
            }
        }
        Prepared {
            h,
            next_digest,
            old_digest,
            pending,
        }
    }

    fn perform(
        prepared: &mut Prepared,
        scenario: Scenario,
        revision: u64,
    ) -> Result<MutationOutcome, MutationError> {
        let h = &prepared.h;
        match scenario {
            Scenario::Install => h.writer.install(revision, prepared.pending.take().unwrap()),
            Scenario::Update => {
                h.writer
                    .update(ID, revision, prepared.pending.take().unwrap(), &Stopped)
            }
            Scenario::Trust => {
                let request = h.trust_request(revision, prepared.old_digest.as_ref().unwrap());
                let preview = h.preview(request.clone()).unwrap();
                h.writer
                    .trust(
                        ID,
                        TrustRequest {
                            confirm_grant_diff: Some(preview.confirmation),
                            ..request
                        },
                    )
                    .map(|result| match result {
                        TrustResult::Applied(outcome) => outcome,
                        TrustResult::Preview { .. } => unreachable!(),
                    })
            }
            Scenario::Enable => {
                h.writer
                    .enable(ID, revision, EnablementScope::Global, &HashSet::new())
            }
            Scenario::Disable => {
                h.writer
                    .disable(ID, revision, EnablementScope::Global, &HashSet::new())
            }
            Scenario::Remove => h.writer.remove(ID, revision, false, &Stopped),
        }
    }

    const CRASH_POINTS: [CrashPoint; 11] = [
        CrashPoint::BeforeJournal,
        CrashPoint::AfterJournal,
        CrashPoint::AfterStorePublish,
        CrashPoint::AfterStateRename(0),
        CrashPoint::AfterMarker,
        CrashPoint::AfterStateRename(1),
        CrashPoint::AfterStateRename(2),
        CrashPoint::AfterStateRename(3),
        CrashPoint::AfterDirectoryFsync,
        CrashPoint::AfterRetentionCleanup,
        CrashPoint::AfterJournalCommitted,
    ];

    fn pre_commit(point: CrashPoint) -> bool {
        matches!(
            point,
            CrashPoint::BeforeJournal | CrashPoint::AfterJournal | CrashPoint::AfterStorePublish
        )
    }

    fn mixed_on_disk(point: CrashPoint) -> bool {
        matches!(
            point,
            CrashPoint::AfterStateRename(0)
                | CrashPoint::AfterMarker
                | CrashPoint::AfterStateRename(1)
                | CrashPoint::AfterStateRename(2)
        )
    }

    #[test]
    fn a3a_crash_at_every_journal_step_rolls_back_before_commit_and_forward_after() {
        for scenario in [
            Scenario::Install,
            Scenario::Update,
            Scenario::Trust,
            Scenario::Enable,
            Scenario::Disable,
            Scenario::Remove,
        ] {
            for point in CRASH_POINTS {
                let context = format!("{scenario:?} at {point:?}");
                let mut prepared = prepare(scenario);
                let old = prepared.h.revision();
                let before = prepared.h.bytes();
                prepared.h.writer.crash_at(Some(point));
                assert_eq!(
                    code(perform(&mut prepared, scenario, old)),
                    "simulated_crash",
                    "{context}"
                );
                prepared.h.writer.crash_at(None);
                let h = &prepared.h;

                // Readers never observe a mixed generation as coherent.
                match h.state() {
                    Ok(state) if pre_commit(point) => {
                        assert_eq!(state.snapshot.revision, old, "{context}")
                    }
                    Ok(state) if !mixed_on_disk(point) => {
                        assert_eq!(state.snapshot.revision, old + 1, "{context}")
                    }
                    Err(StateError::RevisionMismatch) if mixed_on_disk(point) => {}
                    other => panic!("{context}: unexpected reader result {:?}", other.err()),
                }

                h.writer.recover().unwrap();
                h.assert_no_transaction_residue();
                h.assert_no_code_ran();
                let state = h.state().unwrap();
                if pre_commit(point) {
                    assert_eq!(state.snapshot.revision, old, "{context}");
                    assert_eq!(h.bytes(), before, "{context}");
                    if let Some(next) = &prepared.next_digest {
                        assert!(!h.store_payload(next).exists(), "{context}");
                    }
                    if scenario == Scenario::Remove {
                        assert!(h.root().join("state").join(ID).join("cache").exists());
                        assert!(h
                            .store_payload(prepared.old_digest.as_ref().unwrap())
                            .exists());
                    }
                    continue;
                }

                assert_eq!(state.snapshot.revision, old + 1, "{context}");
                assert!(h.root().join(PUBLICATION_MARKER).is_file(), "{context}");
                let installed = state
                    .snapshot
                    .installs
                    .iter()
                    .find(|install| install.id == ID);
                match scenario {
                    Scenario::Install => {
                        let digest = prepared.next_digest.as_ref().unwrap();
                        assert_eq!(installed.unwrap().digest, *digest, "{context}");
                        assert!(h.store_payload(digest).exists(), "{context}");
                        assert!(state.snapshot.grants.is_empty());
                    }
                    Scenario::Update => {
                        let digest = prepared.next_digest.as_ref().unwrap();
                        assert_eq!(installed.unwrap().digest, *digest, "{context}");
                        assert!(h.store_payload(digest).exists());
                        // Prior payload retained as a rollback candidate.
                        assert!(h
                            .store_payload(prepared.old_digest.as_ref().unwrap())
                            .exists());
                        assert!(state.snapshot.grants.is_empty());
                        assert!(state.snapshot.service_grants.is_empty());
                    }
                    Scenario::Trust => {
                        assert_eq!(state.snapshot.grants.len(), 1);
                        assert_eq!(state.snapshot.service_grants.len(), 1);
                    }
                    Scenario::Enable => assert!(state.snapshot.enablement[0].global),
                    Scenario::Disable => assert!(!state.snapshot.enablement[0].global),
                    Scenario::Remove => {
                        assert!(installed.is_none());
                        let package = h.root().join("state").join(ID);
                        assert!(!h.root().join("store").join(ID).exists(), "{context}");
                        assert!(!package.join("cache").exists(), "{context}");
                        assert!(!package.join("tmp").exists(), "{context}");
                        assert!(package.join("data/kept").is_file(), "{context}");
                    }
                }
            }
        }
    }

    #[test]
    fn a3a_next_mutation_recovers_an_interrupted_journal_before_planning() {
        // Post-commit interruption: the next writer rolls forward first, so
        // the committed revision is the one it must name.
        let h = Harness::new();
        let digest = h.install(0, &h.noop("v1", "1.0.0")).digest.unwrap();
        h.trust(1, &digest);
        h.writer.crash_at(Some(CrashPoint::AfterStateRename(1)));
        assert_eq!(
            code(
                h.writer
                    .enable(ID, 2, EnablementScope::Global, &HashSet::new())
            ),
            "simulated_crash"
        );
        h.writer.crash_at(None);
        assert_eq!(
            code(
                h.writer
                    .disable(ID, 2, EnablementScope::Global, &HashSet::new())
            ),
            REVISION_CONFLICT
        );
        assert_eq!(h.revision(), 3);
        assert!(h.state().unwrap().snapshot.enablement[0].global);
        h.writer
            .disable(ID, 3, EnablementScope::Global, &HashSet::new())
            .unwrap();
        h.assert_no_transaction_residue();

        // Pre-commit interruption: rolled back, so the old revision stands.
        h.writer.crash_at(Some(CrashPoint::AfterJournal));
        assert_eq!(
            code(
                h.writer
                    .enable(ID, 4, EnablementScope::Global, &HashSet::new())
            ),
            "simulated_crash"
        );
        h.writer.crash_at(None);
        h.writer
            .enable(ID, 4, EnablementScope::Global, &HashSet::new())
            .unwrap();
        assert_eq!(h.revision(), 5);
        h.assert_no_transaction_residue();
    }

    #[test]
    fn a3a_committed_generation_without_its_staged_files_fails_closed() {
        let h = Harness::new();
        let digest = h.install(0, &h.noop("v1", "1.0.0")).digest.unwrap();
        h.writer.crash_at(Some(CrashPoint::AfterStateRename(0)));
        assert_eq!(
            code(h.apply(h.trust_request(1, &digest))),
            "simulated_crash"
        );
        h.writer.crash_at(None);
        let staging = h.entries("staging");
        assert_eq!(staging.len(), 1);
        fs::remove_file(
            h.root()
                .join("staging")
                .join(&staging[0])
                .join("trust.json"),
        )
        .unwrap();

        let error = h.writer.recover().unwrap_err();
        assert_eq!(error.code, RECOVERY_REQUIRED);
        assert_eq!(error.message(), "extension registry recovery is required");
        // The mixed generation stays unreadable and no later mutation can
        // claim the old revision is effective.
        assert_eq!(h.state().err(), Some(StateError::RevisionMismatch));
        assert_eq!(h.entries("transactions").len(), 1);
        assert_eq!(
            code(
                h.writer
                    .disable(ID, 1, EnablementScope::Global, &HashSet::new())
            ),
            RECOVERY_REQUIRED
        );

        // A corrupt journal is never guessed around either.
        let h = Harness::new();
        h.install(0, &h.noop("v1", "1.0.0"));
        fs::create_dir_all(h.root().join("transactions")).unwrap();
        fs::write(
            h.root()
                .join("transactions")
                .join(format!("{}.json", Uuid::new_v4())),
            "{\"schema_version\":1}",
        )
        .unwrap();
        assert_eq!(h.writer.recover().unwrap_err().code, RECOVERY_REQUIRED);
    }

    #[test]
    fn a3a_retention_matrix_update_rollback_remove_reinstall_and_purge() {
        let h = Harness::new();
        let project = Uuid::new_v4();
        let projects = HashSet::from([project]);
        let v1 = h.noop("v1", "1.0.0");
        let v2 = h.noop("v2", "2.0.0");
        let digest1 = h.install(0, &v1).digest.unwrap();
        h.trust(1, &digest1);
        h.writer
            .enable(ID, 2, EnablementScope::Global, &projects)
            .unwrap();
        h.writer
            .enable(ID, 3, EnablementScope::Project(project), &projects)
            .unwrap();
        h.writer
            .disable(ID, 4, EnablementScope::Global, &projects)
            .unwrap();
        h.writer
            .disable(ID, 5, EnablementScope::Project(project), &projects)
            .unwrap();

        // Update: new row, all trust/service grants deleted, scopes retained
        // but ineffective, prior payload retained.
        let update = h.writer.update(ID, 6, h.acquire(&v2), &Stopped).unwrap();
        let digest2 = update.digest.clone().unwrap();
        assert_ne!(digest1, digest2);
        assert!(!update.effective);
        let state = h.state().unwrap();
        assert_eq!(state.snapshot.installs[0].version, "2.0.0");
        assert!(state.snapshot.grants.is_empty() && state.snapshot.service_grants.is_empty());
        assert_eq!(state.snapshot.enablement[0].projects.len(), 1);
        drop(state);
        assert!(h.store_payload(&digest1).exists() && h.store_payload(&digest2).exists());
        assert!(!h.inspect().trusted);

        // Explicit rollback: retained verified bytes deduplicate, untrusted.
        let rollback = h.writer.update(ID, 7, h.acquire(&v1), &Stopped).unwrap();
        assert_eq!(rollback.digest.as_deref(), Some(digest1.as_str()));
        assert!(!h.inspect().trusted);
        assert!(h.store_payload(&digest2).exists());

        // Remove without purge: revoke everything, delete every payload for
        // the id plus cache/tmp, retain data.
        h.seed_state_roots();
        h.writer.remove(ID, 8, false, &Stopped).unwrap();
        let state = h.state().unwrap();
        assert!(state.snapshot.installs.is_empty());
        assert!(state.snapshot.grants.is_empty());
        assert!(state.snapshot.service_grants.is_empty());
        assert!(state.snapshot.enablement.is_empty());
        drop(state);
        let package = h.root().join("state").join(ID);
        assert!(!h.root().join("store").join(ID).exists());
        assert!(package.join("data/kept").is_file());
        assert!(!package.join("cache").exists() && !package.join("tmp").exists());

        // Identical reinstall: installed but untrusted; retained data reused.
        let reinstall = h.install(9, &v1);
        assert_eq!(reinstall.digest.as_deref(), Some(digest1.as_str()));
        let inspection = h.inspect();
        assert!(inspection.installed && !inspection.trusted && !inspection.enabled);
        assert!(package.join("data/kept").is_file());

        // Remove with purge deletes the data root after (proven) stop.
        h.writer.remove(ID, 10, true, &Stopped).unwrap();
        assert!(!package.exists());
        assert!(!h.root().join("store").join(ID).exists());
        h.assert_no_transaction_residue();
        h.assert_no_code_ran();
    }

    #[test]
    fn a3a_active_update_and_remove_refuse_without_mutation() {
        let h = Harness::new();
        let digest = h.install(0, &h.noop("v1", "1.0.0")).digest.unwrap();
        h.trust(1, &digest);
        h.writer
            .enable(ID, 2, EnablementScope::Global, &HashSet::new())
            .unwrap();
        let v2 = h.noop("v2", "2.0.0");
        let before = h.bytes();
        assert_eq!(
            code(h.writer.update(ID, 3, h.acquire(&v2), &Stopped)),
            "extension_active"
        );
        assert_eq!(
            code(h.writer.remove(ID, 3, false, &Stopped)),
            "extension_active"
        );
        assert_eq!(h.bytes(), before);

        h.writer
            .disable(ID, 3, EnablementScope::Global, &HashSet::new())
            .unwrap();
        assert_eq!(
            code(h.writer.update(ID, 4, h.acquire(&v2), &Running)),
            "extension_active"
        );
        assert_eq!(
            code(h.writer.remove(ID, 4, true, &Running)),
            "extension_active"
        );

        // A leftover connection temp root means reap is not proven.
        h.seed_state_roots();
        fs::create_dir_all(
            h.root()
                .join("state")
                .join(ID)
                .join("tmp/lifecycle")
                .join(Uuid::new_v4().to_string()),
        )
        .unwrap();
        assert_eq!(
            code(h.writer.remove(ID, 4, false, &Stopped)),
            "extension_active"
        );
        assert_eq!(h.revision(), 4);
        assert!(h.entries("quarantine").is_empty());
    }

    #[test]
    fn a3a_enable_rejects_untrusted_incompatible_declared_and_unbound_packages() {
        let project = Uuid::new_v4();
        let projects = HashSet::from([project]);

        let h = Harness::new();
        let digest = h.install(0, &h.noop("v1", "1.0.0")).digest.unwrap();
        assert_eq!(
            code(h.writer.enable(ID, 1, EnablementScope::Global, &projects)),
            "trust_required"
        );
        // Project enablement can never supply or widen trust.
        assert_eq!(
            code(
                h.writer
                    .enable(ID, 1, EnablementScope::Project(project), &projects)
            ),
            "trust_required"
        );
        assert_eq!(
            code(
                h.writer
                    .enable(ID, 1, EnablementScope::Project(Uuid::new_v4()), &projects)
            ),
            "project_not_found"
        );
        // Trust without the native acknowledgement row cannot be enabled.
        let mut capabilities_only = h.trust_request(1, &digest);
        capabilities_only.service_grants.clear();
        h.apply(capabilities_only).unwrap();
        assert_eq!(
            code(h.writer.enable(ID, 2, EnablementScope::Global, &projects)),
            "service_grant_required"
        );
        // A binding-less acknowledgement leaves the requested secret unbound.
        let mut unbound = h.trust_request(2, &digest);
        unbound.service_grants[0].secret_bindings.clear();
        h.apply(unbound).unwrap();
        assert_eq!(
            code(h.writer.enable(ID, 3, EnablementScope::Global, &projects)),
            "unresolved_bindings"
        );
        h.trust(3, &digest);
        h.writer
            .enable(ID, 4, EnablementScope::Project(project), &projects)
            .unwrap();

        let h = Harness::new();
        let source = h.package("future", "1.0.0", "99.0.0", DEFAULT_CAPABILITIES);
        let digest = h.install(0, &source).digest.unwrap();
        h.trust(1, &digest);
        assert!(!h.inspect().compatible);
        assert_eq!(
            code(h.writer.enable(ID, 2, EnablementScope::Global, &projects)),
            "host_incompatible"
        );

        let h = Harness::new();
        let source = h.package(
            "network",
            "1.0.0",
            "0.1.0",
            "network = [\"api.example.com\"]\nenv = [\"NOOP_TOKEN\"]\n",
        );
        let digest = h.install(0, &source).digest.unwrap();
        let mut request = h.trust_request(1, &digest);
        request.capabilities = CapabilitySet {
            network: vec!["api.example.com".into()],
            ..CapabilitySet::default()
        };
        request.service_grants[0].secret_bindings.clear();
        assert_eq!(
            code(h.preview(request.clone())),
            "declaration_policy_rejected"
        );
        request.capabilities = CapabilitySet {
            env: vec!["NOOP_TOKEN".into()],
            ..CapabilitySet::default()
        };
        h.apply(request).unwrap();
        assert_eq!(
            code(h.writer.enable(ID, 2, EnablementScope::Global, &projects)),
            "declaration_policy_rejected"
        );
        h.assert_no_code_ran();
    }

    #[test]
    fn a3a_trust_requests_are_validated_before_any_preview() {
        let h = Harness::new();
        let digest = h.install(0, &h.noop("v1", "1.0.0")).digest.unwrap();
        let base = h.trust_request(1, &digest);

        let mut wrong_digest = base.clone();
        wrong_digest.digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(code(h.preview(wrong_digest)), "digest_mismatch");

        let mut widened = base.clone();
        widened.capabilities.env.push("EXTRA".into());
        assert_eq!(code(h.preview(widened)), "grant_widens_manifest");

        let mut unacknowledged = base.clone();
        unacknowledged.service_grants[0].native_process_ack = false;
        assert_eq!(
            code(h.preview(unacknowledged)),
            "native_process_ack_required"
        );

        let mut unknown = base.clone();
        unknown.service_grants[0].service_id = "other".into();
        assert_eq!(code(h.preview(unknown)), "unknown_service");

        let mut duplicate = base.clone();
        duplicate
            .service_grants
            .push(base.service_grants[0].clone());
        assert_eq!(code(h.preview(duplicate)), "invalid_request");

        for (target, reference) in [
            ("PATH", "env:OCEAN_NOOP_TOKEN"),
            ("NOOP_TOKEN", "env:OTHER_SOURCE"),
            ("OTHER_TARGET", "env:OCEAN_NOOP_TOKEN"),
            ("NOOP_TOKEN", "vault:OCEAN_NOOP_TOKEN"),
        ] {
            let mut binding = base.clone();
            binding.service_grants[0].secret_bindings = vec![SecretBinding {
                target_env: target.into(),
                reference: reference.into(),
            }];
            assert_eq!(
                code(h.preview(binding)),
                "invalid_secret_binding",
                "{target}"
            );
        }

        let mut stale = base.clone();
        stale.expected_state_revision = 0;
        assert_eq!(code(h.preview(stale)), REVISION_CONFLICT);
        assert_eq!(h.revision(), 1);
        h.assert_no_code_ran();
    }

    #[test]
    fn a3a_grant_diff_is_stable_and_confirmation_binds_revision_and_notice() {
        let h = Harness::new();
        let digest = h.install(0, &h.noop("v1", "1.0.0")).digest.unwrap();
        let request = h.trust_request(1, &digest);
        let first = h.preview(request.clone()).unwrap();
        assert_eq!(first, h.preview(request.clone()).unwrap());
        assert_eq!(
            serde_json::to_value(&first.added).unwrap(),
            json!({
                "capabilities": {"network": [], "filesystem": [], "env": ["NOOP_TOKEN"], "secrets": ["env:OCEAN_NOOP_TOKEN"]},
                "service_grants": [{"service_id": "lifecycle", "native_process_ack": true, "secret_bindings": [{"target_env": "NOOP_TOKEN", "reference": "env:OCEAN_NOOP_TOKEN"}]}]
            })
        );
        assert_eq!(first.removed, GrantSide::default());
        assert_eq!(
            first.confirmation,
            grant_confirmation(ID, &digest, 1, &first.added, &first.removed).unwrap()
        );

        // A concurrent commit changes the revision the hash is bound to.
        h.writer
            .disable(ID, 1, EnablementScope::Global, &HashSet::new())
            .unwrap();
        let mut stale = h.trust_request(2, &digest);
        stale.confirm_grant_diff = Some(first.confirmation.clone());
        assert_eq!(
            code(h.writer.trust(ID, stale)),
            "grant_confirmation_mismatch"
        );
        let mut old_revision = request.clone();
        old_revision.confirm_grant_diff = Some(first.confirmation);
        assert_eq!(code(h.writer.trust(ID, old_revision)), REVISION_CONFLICT);
        h.trust(2, &digest);

        // Narrowing and revocation are diffs too and require confirmation.
        let mut narrowed = h.trust_request(3, &digest);
        narrowed.capabilities = CapabilitySet {
            env: vec!["NOOP_TOKEN".into()],
            ..CapabilitySet::default()
        };
        narrowed.service_grants.clear();
        let preview = h.preview(narrowed.clone()).unwrap();
        assert_eq!(preview.added, GrantSide::default());
        assert_eq!(
            preview.removed.capabilities.secrets,
            ["env:OCEAN_NOOP_TOKEN"]
        );
        assert_eq!(preview.removed.service_grants.len(), 1);
        assert_eq!(h.revision(), 3);
        h.apply(narrowed).unwrap();
        let state = h.state().unwrap();
        assert!(state.snapshot.service_grants.is_empty());
        assert!(state.snapshot.grants[0].capabilities.secrets.is_empty());
    }

    #[test]
    fn a3a_local_acquisition_rejects_unsafe_trees_and_leaves_nothing_behind() {
        let h = Harness::new();
        h.install(0, &h.noop("base", "1.0.0"));
        let before = h.bytes();
        let reject = |path: &str| -> &'static str {
            let error = h
                .writer
                .acquire_local(path)
                .err()
                .expect("acquisition must fail");
            assert!(h.entries("quarantine").is_empty(), "{path}");
            error.code
        };

        let symlink = h.noop("symlink", "1.0.0");
        std::os::unix::fs::symlink("/etc/hosts", Path::new(&symlink).join("link")).unwrap();
        assert_eq!(reject(&symlink), PACKAGE_INVALID);

        let hardlink = h.noop("hardlink", "1.0.0");
        fs::hard_link(
            Path::new(&hardlink).join("run-me"),
            Path::new(&hardlink).join("run-me-too"),
        )
        .unwrap();
        assert_eq!(reject(&hardlink), PACKAGE_INVALID);

        let fifo = h.noop("fifo", "1.0.0");
        let fifo_path = CString::new(format!("{fifo}/pipe")).unwrap();
        // SAFETY: fifo_path is NUL-terminated.
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        assert_eq!(reject(&fifo), PACKAGE_INVALID);

        let missing_manifest = h.noop("missing-manifest", "1.0.0");
        fs::remove_file(Path::new(&missing_manifest).join("ocean-extension.toml")).unwrap();
        assert_eq!(reject(&missing_manifest), PACKAGE_INVALID);

        let invalid_manifest = h.noop("invalid-manifest", "1.0.0");
        fs::write(
            Path::new(&invalid_manifest).join("ocean-extension.toml"),
            "schema_version = 1\nid = \"example.noop\"\nunknown = true\n",
        )
        .unwrap();
        assert_eq!(reject(&invalid_manifest), PACKAGE_INVALID);

        let absent_resource = h.noop("absent-resource", "1.0.0");
        fs::remove_file(Path::new(&absent_resource).join("services/lifecycle")).unwrap();
        assert_eq!(reject(&absent_resource), PACKAGE_INVALID);

        let too_deep = h.noop("too-deep", "1.0.0");
        let mut deep = PathBuf::from(&too_deep);
        for _ in 0..=MAX_PACKAGE_DEPTH + 1 {
            deep.push("d");
        }
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(reject(&too_deep), PACKAGE_INVALID);

        // The exact depth limit is legal and must not exhaust a 2 MiB stack.
        let deepest = h.noop("deepest", "1.0.0");
        let mut deep = PathBuf::from(&deepest);
        for _ in 0..MAX_PACKAGE_DEPTH {
            deep.push("d");
        }
        fs::create_dir_all(&deep).unwrap();
        drop(h.writer.acquire_local(&deepest).unwrap());

        let aliased = h.sources.path().join("alias");
        std::os::unix::fs::symlink(h.noop("target", "1.0.0"), &aliased).unwrap();
        assert_eq!(reject(aliased.to_str().unwrap()), PACKAGE_INVALID);

        for invalid in [
            "relative/pkg",
            "/",
            "/tmp/../etc",
            "/definitely/absent/package",
        ] {
            assert_eq!(reject(invalid), "invalid_source", "{invalid}");
        }
        // Every failed acquisition released its permit and touched no state.
        let permits: Vec<_> = (0..4)
            .map(|_| h.writer.begin_acquisition().unwrap())
            .collect();
        drop(permits);
        assert_eq!(h.bytes(), before);
        h.assert_no_code_ran();
    }

    #[test]
    fn a3a_readers_never_observe_a_mixed_generation_during_commits() {
        let h = Harness::new();
        h.install(0, &h.noop("v1", "1.0.0"));
        let done = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let reader = scope.spawn(|| {
                let mut last = 0;
                let mut observed = 0;
                while !done.load(Ordering::Acquire) {
                    match read_locked_state(h.config.path()) {
                        Ok(state) => {
                            assert!(state.snapshot.revision >= last);
                            last = state.snapshot.revision;
                            observed += 1;
                        }
                        Err(StateError::LockBusy) => {}
                        Err(error) => panic!("mixed or corrupt generation observed: {error}"),
                    }
                }
                observed
            });
            for revision in 1..=20 {
                loop {
                    match h
                        .writer
                        .disable(ID, revision, EnablementScope::Global, &HashSet::new())
                    {
                        Ok(outcome) => {
                            assert_eq!(outcome.state_revision, revision + 1);
                            break;
                        }
                        Err(error) if error.code == "extension_state_busy" => continue,
                        Err(error) => panic!("unexpected {error:?}"),
                    }
                }
            }
            done.store(true, Ordering::Release);
            assert!(reader.join().unwrap() > 0);
        });
        assert_eq!(h.revision(), 21);
        h.assert_no_transaction_residue();
    }
}
