//! Sole read-only authority for the daemon-owned extension registry.
//!
//! The accepted Phase 1 install/trust/enable schemas remain unchanged. Stage A2a
//! adds the strict `service-grants.json` companion, with the one ratified upgrade
//! exception: absence means an empty grant set for an otherwise coherent A0
//! three-file snapshot. State and package traversal remain descriptor-relative
//! under one shared lock; this module is also the only source of supervised
//! activation records consumed by `extension_service`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::{CStr, CString, OsStr};
use std::fmt;
use std::fs::{self, File};
use std::io::Read;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use ocean_agent_sdk::extension_lifecycle::LifecycleEventKind;
use ocean_extension::{
    validate_extension_id, ExtensionManifestError, ExternalKind, MetadataServiceResource,
    OceanExtensionMetadata, RawOceanExtensionManifest, RestartPolicy, SecretReference,
    ServiceHealthKind,
};
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use uuid::Uuid;

use super::AppState;

const STATE_SCHEMA_VERSION: u32 = 1;
const STATE_FILE_LIMIT: u64 = 1024 * 1024;
const MANIFEST_FILE_LIMIT: u64 = 1024 * 1024;
const MAX_STATE_ENTRIES: usize = 1024;
const MAX_SERVICE_GRANTS: usize = 1024;
const MAX_SECRET_BINDINGS: usize = 256;
const MAX_CAPABILITY_ITEMS: usize = 256;
const MAX_STRING_BYTES: usize = 4096;
const MAX_PACKAGE_ENTRIES: usize = 10_000;
const MAX_PACKAGE_DEPTH: usize = 64;
const MAX_PACKAGE_BYTES: u64 = 256 * 1024 * 1024;
const LOCK_WAIT: Duration = Duration::from_millis(250);
const INSPECTION_CONCURRENCY: usize = 4;
const TREE_DIGEST_DOMAIN: &[u8] = b"ocean-extension-tree-v1\0";
const FILE_DIGEST_DOMAIN: &[u8] = b"ocean-extension-file-v1\0";

static INSPECTION_LIMITER: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn inspection_limiter() -> Arc<Semaphore> {
    INSPECTION_LIMITER
        .get_or_init(|| Arc::new(Semaphore::new(INSPECTION_CONCURRENCY)))
        .clone()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallsFile {
    schema_version: u32,
    state_revision: u64,
    installs: Vec<InstalledArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustFile {
    schema_version: u32,
    state_revision: u64,
    grants: Vec<ArtifactTrustGrant>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnabledFile {
    schema_version: u32,
    state_revision: u64,
    extensions: Vec<ExtensionEnablement>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceGrantsFile {
    schema_version: u32,
    state_revision: u64,
    service_grants: Vec<ServiceGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceGrant {
    id: String,
    digest: String,
    service_id: String,
    native_process_ack: bool,
    secret_bindings: Vec<SecretBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SecretBinding {
    pub(crate) target_env: String,
    pub(crate) reference: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstalledArtifact {
    id: String,
    version: String,
    digest: String,
    source: InstallSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallSource {
    kind: InstallSourceKind,
    locator: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum InstallSourceKind {
    LocalPath,
    Git,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct CapabilitySet {
    #[serde(default)]
    network: Vec<String>,
    #[serde(default)]
    filesystem: Vec<String>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    secrets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactTrustGrant {
    id: String,
    digest: String,
    #[serde(default)]
    capabilities: CapabilitySet,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtensionEnablement {
    id: String,
    global: bool,
    #[serde(default)]
    projects: Vec<ProjectEnablement>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectEnablement {
    project_id: Uuid,
    enabled: bool,
}

#[derive(Debug, Clone)]
struct StateSnapshot {
    revision: u64,
    installs: Vec<InstalledArtifact>,
    grants: Vec<ArtifactTrustGrant>,
    enablement: Vec<ExtensionEnablement>,
    service_grants: Vec<ServiceGrant>,
}

/// One immutable, descriptor-anchored native service activation.
///
/// File descriptors keep the exact verified package and executable generation
/// alive after the shared registry lock is released. Secret values are never
/// part of this record.
pub(crate) struct ServiceActivation {
    pub(crate) package_id: String,
    pub(crate) package_version: String,
    pub(crate) package_digest: String,
    pub(crate) service_id: String,
    pub(crate) activation_revision: u64,
    pub(crate) config_dir: PathBuf,
    pub(crate) package_path: PathBuf,
    pub(crate) package_directory: File,
    pub(crate) executable: File,
    pub(crate) executable_path: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) events: Vec<LifecycleEventKind>,
    pub(crate) environment: Vec<String>,
    pub(crate) secret_bindings: Vec<SecretBinding>,
    pub(crate) startup_timeout: Duration,
}

struct LockedState {
    root: Option<File>,
    snapshot: StateSnapshot,
    // Dropping the file releases the shared advisory lock. It stays live while
    // the artifact is traversed so a compliant Phase 3 writer cannot publish a
    // mixed state/payload generation.
    _lock: Option<File>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize)]
struct ExtensionDiagnostic {
    severity: DiagnosticSeverity,
    code: &'static str,
    message: &'static str,
}

impl ExtensionDiagnostic {
    const fn info(code: &'static str, message: &'static str) -> Self {
        Self {
            severity: DiagnosticSeverity::Info,
            code,
            message,
        }
    }

    const fn warning(code: &'static str, message: &'static str) -> Self {
        Self {
            severity: DiagnosticSeverity::Warning,
            code,
            message,
        }
    }

    const fn error(code: &'static str, message: &'static str) -> Self {
        Self {
            severity: DiagnosticSeverity::Error,
            code,
            message,
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
struct ExtensionResources {
    plugins: Vec<IdPathResource>,
    services: Vec<ServiceResourceProjection>,
    agents: Vec<IdPathResource>,
    skills: Vec<IdPathResource>,
    profiles: Vec<ProfileResourceProjection>,
    external: Vec<ExternalResourceProjection>,
}

#[derive(Debug, Clone, Serialize)]
struct IdPathResource {
    id: String,
    path: String,
}

#[derive(Debug, Clone, Serialize)]
struct ServiceResourceProjection {
    id: String,
    entry: String,
    args_count: usize,
    events: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    restart: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    health: Option<ServiceHealthProjection>,
    capabilities: CapabilitySet,
}

#[derive(Debug, Clone, Serialize)]
struct ServiceHealthProjection {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    startup_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct ProfileResourceProjection {
    surface: String,
    path: String,
}

#[derive(Debug, Clone, Serialize)]
struct ExternalResourceProjection {
    kind: String,
    manifest: String,
}

#[derive(Debug, Clone, Serialize)]
struct StaticHealthProjection {
    probe_run: bool,
    last_observed: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ExtensionInspection {
    id: String,
    state_revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<Uuid>,
    installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_ocean_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<InstallSource>,
    artifact_verified: bool,
    manifest_valid: bool,
    compatible: bool,
    trusted: bool,
    global_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_override: Option<bool>,
    enabled: bool,
    effective: bool,
    requested_capabilities: CapabilitySet,
    granted_capabilities: CapabilitySet,
    resources: ExtensionResources,
    health: StaticHealthProjection,
    diagnostics: Vec<ExtensionDiagnostic>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorChecks {
    coherent_state: bool,
    artifact_digest: bool,
    manifest: bool,
    trust_binding: bool,
    enablement: bool,
    package_code_executed: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct ExtensionStateQuery {
    #[serde(default)]
    project_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StateError {
    MissingComponent(&'static str),
    InvalidComponent(&'static str),
    LockBusy,
    Read(&'static str),
    Oversized(&'static str),
    Parse(&'static str),
    UnsupportedSchema(&'static str),
    RevisionMismatch,
    InvalidRecord(&'static str),
}

impl StateError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::MissingComponent(_) => "extension_state_incomplete",
            Self::InvalidComponent(_) => "extension_state_unsafe_path",
            Self::LockBusy => "extension_state_busy",
            Self::Read(_) => "extension_state_unreadable",
            Self::Oversized(_) => "extension_state_oversized",
            Self::Parse(_) => "extension_state_malformed",
            Self::UnsupportedSchema(_) => "extension_state_schema_unsupported",
            Self::RevisionMismatch => "extension_state_revision_mismatch",
            Self::InvalidRecord(_) => "extension_state_invalid_record",
        }
    }
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingComponent(file) => write!(f, "missing extension state component {file}"),
            Self::InvalidComponent(file) => write!(f, "unsafe extension state component {file}"),
            Self::LockBusy => f.write_str("extension state lock is busy"),
            Self::Read(file) => write!(f, "could not read extension state component {file}"),
            Self::Oversized(file) => write!(f, "extension state component {file} is oversized"),
            Self::Parse(file) => write!(f, "could not parse extension state component {file}"),
            Self::UnsupportedSchema(file) => {
                write!(f, "unsupported extension state schema in {file}")
            }
            Self::RevisionMismatch => f.write_str("extension state revisions do not match"),
            Self::InvalidRecord(kind) => write!(f, "invalid extension state {kind} record"),
        }
    }
}

impl std::error::Error for StateError {}

fn empty_state() -> LockedState {
    LockedState {
        root: None,
        snapshot: StateSnapshot {
            revision: 0,
            installs: Vec::new(),
            grants: Vec::new(),
            enablement: Vec::new(),
            service_grants: Vec::new(),
        },
        _lock: None,
    }
}

fn read_locked_state(config_dir: &FsPath) -> Result<LockedState, StateError> {
    let Some(root) = open_extensions_root(config_dir)? else {
        return Ok(empty_state());
    };
    let lock = open_regular_file_at(&root, OsStr::new(".state.lock"), ".state.lock")?;
    acquire_shared_lock(&lock)?;

    let installs: InstallsFile = read_state_json_at(&root, "installs.json")?;
    let trust: TrustFile = read_state_json_at(&root, "trust.json")?;
    let enabled: EnabledFile = read_state_json_at(&root, "enabled.json")?;
    let service_grants = read_optional_service_grants(&root)?;

    for (file, schema) in [
        ("installs.json", installs.schema_version),
        ("trust.json", trust.schema_version),
        ("enabled.json", enabled.schema_version),
    ] {
        if schema != STATE_SCHEMA_VERSION {
            return Err(StateError::UnsupportedSchema(file));
        }
    }
    if service_grants
        .as_ref()
        .is_some_and(|grants| grants.schema_version != STATE_SCHEMA_VERSION)
    {
        return Err(StateError::UnsupportedSchema("service-grants.json"));
    }
    if installs.state_revision == 0
        || installs.state_revision != trust.state_revision
        || installs.state_revision != enabled.state_revision
        || service_grants
            .as_ref()
            .is_some_and(|grants| grants.state_revision != installs.state_revision)
    {
        return Err(StateError::RevisionMismatch);
    }

    let mut snapshot = StateSnapshot {
        revision: installs.state_revision,
        installs: installs.installs,
        grants: trust.grants,
        enablement: enabled.extensions,
        // The sole A0 upgrade exception is absence => empty. No file is written.
        service_grants: service_grants
            .map(|grants| grants.service_grants)
            .unwrap_or_default(),
    };
    validate_snapshot(&mut snapshot)?;
    validate_service_grants_against_artifacts(&root, &snapshot)?;

    Ok(LockedState {
        root: Some(root),
        snapshot,
        _lock: Some(lock),
    })
}

fn open_extensions_root(config_dir: &FsPath) -> Result<Option<File>, StateError> {
    let canonical_config = match fs::canonicalize(config_dir) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(StateError::Read("config directory")),
    };
    let config = File::open(&canonical_config).map_err(|_| StateError::Read("config directory"))?;
    if !config
        .metadata()
        .map_err(|_| StateError::Read("config directory"))?
        .is_dir()
    {
        return Err(StateError::InvalidComponent("config directory"));
    }
    match open_dir_at(&config, OsStr::new("extensions"), "extensions/") {
        Ok(root) => Ok(Some(root)),
        Err(StateError::MissingComponent(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn acquire_shared_lock(file: &File) -> Result<(), StateError> {
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match fs2::FileExt::try_lock_shared(file) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(StateError::LockBusy);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return Err(StateError::Read(".state.lock")),
        }
    }
}

fn read_state_json_at<T: for<'de> Deserialize<'de>>(
    root: &File,
    name: &'static str,
) -> Result<T, StateError> {
    let mut file = open_regular_file_at(root, OsStr::new(name), name)?;
    let bytes = read_capped(&mut file, STATE_FILE_LIMIT, name)?;
    serde_json::from_slice(&bytes).map_err(|_| StateError::Parse(name))
}

fn read_optional_service_grants(root: &File) -> Result<Option<ServiceGrantsFile>, StateError> {
    match read_state_json_at(root, "service-grants.json") {
        Ok(grants) => Ok(Some(grants)),
        Err(StateError::MissingComponent("service-grants.json")) => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_capped(file: &mut File, limit: u64, label: &'static str) -> Result<Vec<u8>, StateError> {
    let size = file.metadata().map_err(|_| StateError::Read(label))?.len();
    if size > limit {
        return Err(StateError::Oversized(label));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.by_ref()
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| StateError::Read(label))?;
    if bytes.len() as u64 > limit {
        return Err(StateError::Oversized(label));
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_dir_at(parent: &File, name: &OsStr, label: &'static str) -> Result<File, StateError> {
    open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK,
        label,
    )
}

#[cfg(unix)]
fn open_file_at(parent: &File, name: &OsStr, label: &'static str) -> Result<File, StateError> {
    open_at(parent, name, libc::O_RDONLY | libc::O_NONBLOCK, label)
}

fn open_regular_file_at(
    parent: &File,
    name: &OsStr,
    label: &'static str,
) -> Result<File, StateError> {
    let file = open_file_at(parent, name, label)?;
    if !file
        .metadata()
        .map_err(|_| StateError::Read(label))?
        .is_file()
    {
        return Err(StateError::InvalidComponent(label));
    }
    Ok(file)
}

#[cfg(unix)]
fn open_at(
    parent: &File,
    name: &OsStr,
    flags: libc::c_int,
    label: &'static str,
) -> Result<File, StateError> {
    let name = CString::new(name.as_bytes()).map_err(|_| StateError::InvalidComponent(label))?;
    // SAFETY: `parent` is a live directory descriptor, `name` is NUL-terminated,
    // and a successful descriptor is transferred exactly once into `File`.
    let descriptor = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        let error = std::io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOENT) => Err(StateError::MissingComponent(label)),
            Some(libc::ELOOP) | Some(libc::ENOTDIR) => Err(StateError::InvalidComponent(label)),
            _ => Err(StateError::Read(label)),
        };
    }
    // SAFETY: successful `openat` returned a fresh owned descriptor.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(not(unix))]
fn open_dir_at(_parent: &File, _name: &OsStr, label: &'static str) -> Result<File, StateError> {
    Err(StateError::InvalidComponent(label))
}

#[cfg(not(unix))]
fn open_file_at(_parent: &File, _name: &OsStr, label: &'static str) -> Result<File, StateError> {
    Err(StateError::InvalidComponent(label))
}

#[cfg(unix)]
struct DirectoryStream(*mut libc::DIR);

#[cfg(unix)]
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns the stream returned by fdopendir.
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: libc exposes the calling thread's errno slot.
    unsafe { libc::__error() }
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    // SAFETY: libc exposes the calling thread's errno slot.
    unsafe { libc::__errno_location() }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
unsafe fn errno_location() -> *mut libc::c_int {
    std::ptr::null_mut()
}

#[cfg(unix)]
fn directory_names(directory: &File, remaining: usize) -> Result<Vec<String>, StateError> {
    // F_DUPFD_CLOEXEC gives fdopendir its own descriptor; closedir must not close
    // the caller's anchored directory handle.
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(StateError::Read("extension payload"));
    }
    // SAFETY: `duplicate` is a fresh readable directory descriptor. fdopendir
    // takes ownership on success.
    let raw = unsafe { libc::fdopendir(duplicate) };
    if raw.is_null() {
        // SAFETY: fdopendir did not take ownership on failure.
        unsafe { libc::close(duplicate) };
        return Err(StateError::Read("extension payload"));
    }
    let stream = DirectoryStream(raw);
    let mut names = Vec::new();
    loop {
        let errno = unsafe { errno_location() };
        if !errno.is_null() {
            unsafe { *errno = 0 };
        }
        // SAFETY: `stream` owns a valid DIR pointer for this loop.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if !errno.is_null() && unsafe { *errno } != 0 {
                return Err(StateError::Read("extension payload"));
            }
            break;
        }
        // SAFETY: d_name is NUL-terminated for the lifetime of this readdir row.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        let name = std::str::from_utf8(bytes)
            .map_err(|_| StateError::InvalidComponent("extension payload"))?;
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(StateError::InvalidComponent("extension payload"));
        }
        names.push(name.to_string());
        if names.len() > remaining {
            return Err(StateError::Oversized("extension payload"));
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(not(unix))]
fn directory_names(_directory: &File, _remaining: usize) -> Result<Vec<String>, StateError> {
    Err(StateError::InvalidComponent("extension payload"))
}

fn validate_snapshot(snapshot: &mut StateSnapshot) -> Result<(), StateError> {
    if snapshot.installs.len() > MAX_STATE_ENTRIES
        || snapshot.grants.len() > MAX_STATE_ENTRIES
        || snapshot.enablement.len() > MAX_STATE_ENTRIES
    {
        return Err(StateError::InvalidRecord("entry-count"));
    }

    let mut install_ids = HashSet::new();
    for install in &snapshot.installs {
        validate_extension_id(&install.id).map_err(|_| StateError::InvalidRecord("install"))?;
        if !install_ids.insert(install.id.clone())
            || Version::parse(&install.version).is_err()
            || digest_hex(&install.digest).is_none()
        {
            return Err(StateError::InvalidRecord("install"));
        }
        validate_source(&install.source)?;
    }

    let mut trust_identities = HashSet::new();
    for grant in &snapshot.grants {
        validate_extension_id(&grant.id).map_err(|_| StateError::InvalidRecord("trust"))?;
        if digest_hex(&grant.digest).is_none()
            || !trust_identities.insert((grant.id.clone(), grant.digest.clone()))
        {
            return Err(StateError::InvalidRecord("trust"));
        }
        validate_capability_set(&grant.capabilities)?;
    }

    let mut enabled_ids = HashSet::new();
    for entry in &mut snapshot.enablement {
        validate_extension_id(&entry.id).map_err(|_| StateError::InvalidRecord("enablement"))?;
        if !enabled_ids.insert(entry.id.clone()) || entry.projects.len() > MAX_STATE_ENTRIES {
            return Err(StateError::InvalidRecord("enablement"));
        }
        let mut project_ids = HashSet::new();
        for project in &entry.projects {
            if !project_ids.insert(project.project_id) {
                return Err(StateError::InvalidRecord("project-override"));
            }
        }
        entry.projects.sort_by_key(|entry| entry.project_id);
    }

    snapshot
        .installs
        .sort_by(|left, right| left.id.cmp(&right.id));
    snapshot
        .grants
        .sort_by(|left, right| (&left.id, &left.digest).cmp(&(&right.id, &right.digest)));
    snapshot
        .enablement
        .sort_by(|left, right| left.id.cmp(&right.id));

    if snapshot.service_grants.len() > MAX_SERVICE_GRANTS {
        return Err(StateError::InvalidRecord("service-grant-count"));
    }
    let mut previous: Option<(&str, &str, &str)> = None;
    for grant in &snapshot.service_grants {
        validate_extension_id(&grant.id).map_err(|_| StateError::InvalidRecord("service-grant"))?;
        validate_resource_id(&grant.service_id)?;
        if digest_hex(&grant.digest).is_none() || !grant.native_process_ack {
            return Err(StateError::InvalidRecord("service-grant"));
        }
        let identity = (
            grant.id.as_str(),
            grant.digest.as_str(),
            grant.service_id.as_str(),
        );
        if previous.is_some_and(|prior| prior >= identity) {
            return Err(StateError::InvalidRecord("service-grant-order"));
        }
        previous = Some(identity);
        validate_secret_bindings(&grant.secret_bindings)?;
    }
    Ok(())
}

fn validate_resource_id(value: &str) -> Result<(), StateError> {
    let mut bytes = value.bytes();
    let valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        });
    if valid {
        Ok(())
    } else {
        Err(StateError::InvalidRecord("service-grant"))
    }
}

fn validate_secret_bindings(bindings: &[SecretBinding]) -> Result<(), StateError> {
    if bindings.len() > MAX_SECRET_BINDINGS {
        return Err(StateError::InvalidRecord("secret-binding-count"));
    }
    let mut previous: Option<(&str, &str)> = None;
    let mut targets = HashSet::new();
    let mut references = HashSet::new();
    for binding in bindings {
        let identity = (binding.target_env.as_str(), binding.reference.as_str());
        if previous.is_some_and(|prior| prior >= identity)
            || !targets.insert(binding.target_env.as_str())
            || !references.insert(binding.reference.as_str())
            || !valid_child_environment_name(&binding.target_env)
            || reserved_child_environment_name(&binding.target_env)
            || env_secret_source(&binding.reference).is_none()
        {
            return Err(StateError::InvalidRecord("secret-binding"));
        }
        previous = Some(identity);
    }
    Ok(())
}

pub(crate) fn valid_child_environment_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && matches!(bytes[0], b'A'..=b'Z' | b'_')
        && bytes[1..]
            .iter()
            .all(|byte| matches!(byte, b'A'..=b'Z' | b'0'..=b'9' | b'_'))
}

pub(crate) fn reserved_child_environment_name(value: &str) -> bool {
    matches!(
        value,
        "PATH" | "PWD" | "HOME" | "XDG_STATE_HOME" | "XDG_CACHE_HOME" | "TMPDIR"
    ) || value.starts_with("OCEAN_EXTENSION_")
}

pub(crate) fn env_secret_source(reference: &str) -> Option<&str> {
    let key = reference.strip_prefix("env:")?;
    valid_child_environment_name(key).then_some(key)
}

fn validate_service_grants_against_artifacts(
    root: &File,
    snapshot: &StateSnapshot,
) -> Result<(), StateError> {
    let mut metadata_by_artifact = HashMap::new();
    for grant in &snapshot.service_grants {
        let install = snapshot
            .installs
            .iter()
            .find(|install| install.id == grant.id && install.digest == grant.digest)
            .ok_or(StateError::InvalidRecord("service-grant-install"))?;
        let trust = snapshot
            .grants
            .iter()
            .find(|trust| trust.id == grant.id && trust.digest == grant.digest)
            .ok_or(StateError::InvalidRecord("service-grant-trust"))?;
        let key = (grant.id.clone(), grant.digest.clone());
        if !metadata_by_artifact.contains_key(&key) {
            let package = open_package(root, install)?;
            let package = snapshot_package(&package)?;
            if package.digest != install.digest {
                return Err(StateError::InvalidRecord("artifact-digest"));
            }
            let raw = RawOceanExtensionManifest::parse(&package.manifest)
                .map_err(|_| StateError::InvalidRecord("manifest"))?;
            let metadata = raw
                .validate_metadata(&Version::new(u64::MAX, u64::MAX, u64::MAX))
                .map_err(|_| StateError::InvalidRecord("manifest"))?;
            if metadata.id != install.id
                || metadata.version.to_string() != install.version
                || !metadata_paths_exist(&metadata, &package.entries)
            {
                return Err(StateError::InvalidRecord("manifest-identity"));
            }
            metadata_by_artifact.insert(key.clone(), metadata);
        }
        let metadata = metadata_by_artifact
            .get(&key)
            .ok_or(StateError::InvalidRecord("manifest"))?;
        let service = metadata
            .services
            .iter()
            .find(|service| service.id == grant.service_id)
            .ok_or(StateError::InvalidRecord("service-grant-service"))?;
        validate_binding_authority(grant, trust, service)?;
    }
    Ok(())
}

fn validate_binding_authority(
    grant: &ServiceGrant,
    trust: &ArtifactTrustGrant,
    service: &MetadataServiceResource,
) -> Result<(), StateError> {
    let requested_env: HashSet<&str> = service
        .capabilities
        .env
        .iter()
        .map(String::as_str)
        .collect();
    let requested_secrets: HashSet<String> = service
        .capabilities
        .secrets
        .iter()
        .map(ToString::to_string)
        .collect();
    let trusted_env: HashSet<&str> = trust.capabilities.env.iter().map(String::as_str).collect();
    let trusted_secrets: HashSet<&str> = trust
        .capabilities
        .secrets
        .iter()
        .map(String::as_str)
        .collect();
    for binding in &grant.secret_bindings {
        if !requested_env.contains(binding.target_env.as_str())
            || !requested_secrets.contains(&binding.reference)
            || !trusted_env.contains(binding.target_env.as_str())
            || !trusted_secrets.contains(binding.reference.as_str())
        {
            return Err(StateError::InvalidRecord("secret-binding-authority"));
        }
    }
    Ok(())
}

fn validate_source(source: &InstallSource) -> Result<(), StateError> {
    validate_bounded_string(&source.locator)?;
    if let Some(revision) = &source.revision {
        validate_bounded_string(revision)?;
    }
    match source.kind {
        InstallSourceKind::LocalPath => {
            let path = FsPath::new(&source.locator);
            let normalized = path.components().collect::<PathBuf>();
            if source.revision.is_some()
                || !path.is_absolute()
                || normalized.as_os_str() != path.as_os_str()
                || path.components().any(|component| {
                    !matches!(component, Component::RootDir | Component::Normal(_))
                })
            {
                return Err(StateError::InvalidRecord("source"));
            }
        }
        InstallSourceKind::Git => {
            let revision = source
                .revision
                .as_deref()
                .ok_or(StateError::InvalidRecord("source"))?;
            if !matches!(revision.len(), 40 | 64)
                || !revision
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                || !valid_git_locator(&source.locator)
            {
                return Err(StateError::InvalidRecord("source"));
            }
        }
    }
    Ok(())
}

fn valid_git_locator(locator: &str) -> bool {
    if locator.contains(['?', '#']) || locator.chars().any(char::is_whitespace) {
        return false;
    }
    if let Some(rest) = locator.strip_prefix("https://") {
        let Some((authority, path)) = rest.split_once('/') else {
            return false;
        };
        return !authority.is_empty() && !authority.contains('@') && !path.is_empty();
    }
    if let Some(rest) = locator.strip_prefix("ssh://git@") {
        let Some((authority, path)) = rest.split_once('/') else {
            return false;
        };
        return !authority.is_empty() && !authority.contains('@') && !path.is_empty();
    }
    if let Some(rest) = locator.strip_prefix("git@") {
        if rest.contains('@') {
            return false;
        }
        let Some((authority, path)) = rest.split_once(':') else {
            return false;
        };
        return !authority.is_empty() && !path.is_empty();
    }
    false
}

fn validate_capability_set(capabilities: &CapabilitySet) -> Result<(), StateError> {
    for values in [
        &capabilities.network,
        &capabilities.filesystem,
        &capabilities.env,
        &capabilities.secrets,
    ] {
        if values.len() > MAX_CAPABILITY_ITEMS {
            return Err(StateError::InvalidRecord("capability-grant"));
        }
        let mut unique = HashSet::new();
        for value in values {
            validate_bounded_string(value)?;
            if !unique.insert(value) {
                return Err(StateError::InvalidRecord("capability-grant"));
            }
        }
    }
    for secret in &capabilities.secrets {
        secret
            .parse::<SecretReference>()
            .map_err(|_| StateError::InvalidRecord("capability-grant"))?;
    }
    Ok(())
}

fn validate_bounded_string(value: &str) -> Result<(), StateError> {
    if value.is_empty() || value.len() > MAX_STRING_BYTES || value.chars().any(char::is_control) {
        return Err(StateError::InvalidRecord("string"));
    }
    Ok(())
}

fn digest_hex(digest: &str) -> Option<&str> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(hex)
}

fn open_package(root: &File, install: &InstalledArtifact) -> Result<File, StateError> {
    let hex = digest_hex(&install.digest).ok_or(StateError::InvalidRecord("install"))?;
    let store = open_dir_at(root, OsStr::new("store"), "extension store")?;
    let extension = open_dir_at(
        &store,
        OsStr::new(&install.id),
        "extension artifact identity",
    )?;
    open_dir_at(&extension, OsStr::new(hex), "extension artifact digest")
}

#[derive(Clone, Copy)]
enum PackageEntryKind {
    File,
    Directory,
}

struct PackageFileRecord {
    // Directory records use a trailing slash in the digest namespace so they
    // cannot collide with a regular file name. `lookup_path` remains slashless
    // for descriptor-relative revalidation.
    path: String,
    lookup_path: String,
    kind: PackageEntryKind,
    executable: bool,
    length: u64,
    content_digest: [u8; 32],
    fingerprint: fs::Metadata,
}

struct PackageSnapshot {
    digest: String,
    manifest: String,
    entries: BTreeSet<String>,
}

struct PackageWalk {
    records: Vec<PackageFileRecord>,
    entries: BTreeSet<String>,
    manifest: Option<Vec<u8>>,
    total_bytes: u64,
    total_entries: usize,
}

fn snapshot_package(package: &File) -> Result<PackageSnapshot, StateError> {
    let root_before = package
        .metadata()
        .map_err(|_| StateError::Read("extension payload"))?;
    let mut walk = PackageWalk {
        records: Vec::new(),
        entries: BTreeSet::new(),
        manifest: None,
        total_bytes: 0,
        total_entries: 0,
    };
    walk_package_dir(package, "", 0, &mut walk)?;
    revalidate_package(package, &walk.records)?;
    let root_after = package
        .metadata()
        .map_err(|_| StateError::Read("extension payload"))?;
    if metadata_changed(&root_before, &root_after) {
        return Err(StateError::Read("extension payload"));
    }
    walk.records
        .sort_by(|left, right| left.path.cmp(&right.path));

    let mut tree = Sha256::new();
    tree.update(TREE_DIGEST_DOMAIN);
    for record in walk.records {
        let path = record.path.as_bytes();
        tree.update((path.len() as u64).to_be_bytes());
        tree.update(path);
        tree.update([u8::from(record.executable)]);
        tree.update(record.length.to_be_bytes());
        tree.update(record.content_digest);
    }
    let digest = format!("sha256:{}", lowercase_hex(&tree.finalize()));
    let manifest = walk
        .manifest
        .ok_or(StateError::MissingComponent("ocean-extension.toml"))?;
    let manifest =
        String::from_utf8(manifest).map_err(|_| StateError::Parse("ocean-extension.toml"))?;
    Ok(PackageSnapshot {
        digest,
        manifest,
        entries: walk.entries,
    })
}

fn walk_package_dir(
    directory: &File,
    prefix: &str,
    depth: usize,
    walk: &mut PackageWalk,
) -> Result<(), StateError> {
    if depth > MAX_PACKAGE_DEPTH {
        return Err(StateError::Oversized("extension payload"));
    }
    let remaining = MAX_PACKAGE_ENTRIES.saturating_sub(walk.total_entries);
    for name in directory_names(directory, remaining)? {
        walk.total_entries += 1;
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let mut entry = open_file_at(directory, OsStr::new(&name), "extension payload")?;
        let before = entry
            .metadata()
            .map_err(|_| StateError::Read("extension payload"))?;
        walk.entries.insert(relative.clone());
        if before.is_dir() {
            walk_package_dir(&entry, &relative, depth + 1, walk)?;
            let after = entry
                .metadata()
                .map_err(|_| StateError::Read("extension payload"))?;
            if metadata_changed(&before, &after) {
                return Err(StateError::Read("extension payload"));
            }
            walk.records.push(PackageFileRecord {
                path: format!("{relative}/"),
                lookup_path: relative,
                kind: PackageEntryKind::Directory,
                executable: metadata_executable(&before),
                length: 0,
                content_digest: directory_content_digest(),
                fingerprint: before,
            });
            continue;
        }
        if !before.is_file() {
            return Err(StateError::InvalidComponent("extension payload"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if before.nlink() != 1 {
                return Err(StateError::InvalidComponent("extension payload"));
            }
        }
        let length = before.len();
        walk.total_bytes = walk
            .total_bytes
            .checked_add(length)
            .ok_or(StateError::Oversized("extension payload"))?;
        if walk.total_bytes > MAX_PACKAGE_BYTES {
            return Err(StateError::Oversized("extension payload"));
        }
        let executable = metadata_executable(&before);

        let capture_manifest = relative == "ocean-extension.toml";
        if capture_manifest && length > MANIFEST_FILE_LIMIT {
            return Err(StateError::Oversized("ocean-extension.toml"));
        }
        let mut content = capture_manifest.then(|| Vec::with_capacity(length as usize));
        let mut file_digest = Sha256::new();
        file_digest.update(FILE_DIGEST_DOMAIN);
        let mut observed = 0u64;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = entry
                .read(&mut buffer)
                .map_err(|_| StateError::Read("extension payload"))?;
            if count == 0 {
                break;
            }
            observed += count as u64;
            if observed > length {
                return Err(StateError::Read("extension payload"));
            }
            file_digest.update(&buffer[..count]);
            if let Some(content) = &mut content {
                content.extend_from_slice(&buffer[..count]);
            }
        }
        if observed != length {
            return Err(StateError::Read("extension payload"));
        }
        let after = entry
            .metadata()
            .map_err(|_| StateError::Read("extension payload"))?;
        if metadata_changed(&before, &after) {
            return Err(StateError::Read("extension payload"));
        }
        if capture_manifest && walk.manifest.replace(content.unwrap_or_default()).is_some() {
            return Err(StateError::InvalidRecord("manifest"));
        }
        walk.records.push(PackageFileRecord {
            path: relative.clone(),
            lookup_path: relative,
            kind: PackageEntryKind::File,
            executable,
            length,
            content_digest: file_digest.finalize().into(),
            fingerprint: before,
        });
    }
    Ok(())
}

fn revalidate_package(package: &File, records: &[PackageFileRecord]) -> Result<(), StateError> {
    for record in records {
        let mut entry = open_relative_entry(package, &record.lookup_path)?;
        let before = entry
            .metadata()
            .map_err(|_| StateError::Read("extension payload"))?;
        let kind_matches = match record.kind {
            PackageEntryKind::File => before.is_file(),
            PackageEntryKind::Directory => before.is_dir(),
        };
        if !kind_matches || metadata_changed(&record.fingerprint, &before) {
            return Err(StateError::Read("extension payload"));
        }
        if matches!(record.kind, PackageEntryKind::File) {
            let mut digest = Sha256::new();
            digest.update(FILE_DIGEST_DOMAIN);
            let mut observed = 0u64;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                let count = entry
                    .read(&mut buffer)
                    .map_err(|_| StateError::Read("extension payload"))?;
                if count == 0 {
                    break;
                }
                observed += count as u64;
                if observed > record.length {
                    return Err(StateError::Read("extension payload"));
                }
                digest.update(&buffer[..count]);
            }
            if observed != record.length
                || <[u8; 32]>::from(digest.finalize()) != record.content_digest
            {
                return Err(StateError::Read("extension payload"));
            }
            let after = entry
                .metadata()
                .map_err(|_| StateError::Read("extension payload"))?;
            if metadata_changed(&before, &after) {
                return Err(StateError::Read("extension payload"));
            }
        }
    }
    Ok(())
}

fn open_relative_entry(package: &File, relative: &str) -> Result<File, StateError> {
    let mut components = relative.split('/').peekable();
    let mut directory = package
        .try_clone()
        .map_err(|_| StateError::Read("extension payload"))?;
    while let Some(component) = components.next() {
        if component.is_empty() {
            return Err(StateError::InvalidComponent("extension payload"));
        }
        if components.peek().is_some() {
            directory = open_dir_at(&directory, OsStr::new(component), "extension payload")?;
        } else {
            return open_file_at(&directory, OsStr::new(component), "extension payload");
        }
    }
    Err(StateError::InvalidComponent("extension payload"))
}

fn directory_content_digest() -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(FILE_DIGEST_DOMAIN);
    digest.update(b"directory");
    digest.finalize().into()
}

#[cfg(unix)]
fn metadata_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn metadata_executable(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn metadata_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.nlink() != after.nlink()
        || before.len() != after.len()
        || before.mode() != after.mode()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
}

#[cfg(not(unix))]
fn metadata_changed(before: &fs::Metadata, after: &fs::Metadata) -> bool {
    before.len() != after.len() || before.permissions().readonly() != after.permissions().readonly()
}

fn lowercase_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("write to String");
    }
    hex
}

fn normalized_resource_path(raw: &str) -> Option<String> {
    let mut components = Vec::new();
    for component in FsPath::new(raw).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => components.push(value.to_str()?.to_string()),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(components.join("/"))
}

fn inventory_contains(entries: &BTreeSet<String>, raw: &str) -> bool {
    normalized_resource_path(raw).is_some_and(|path| path.is_empty() || entries.contains(&path))
}

fn metadata_paths_exist(metadata: &OceanExtensionMetadata, entries: &BTreeSet<String>) -> bool {
    metadata
        .plugins
        .iter()
        .chain(&metadata.agents)
        .chain(&metadata.skills)
        .all(|resource| inventory_contains(entries, &resource.path))
        && metadata
            .services
            .iter()
            .all(|resource| inventory_contains(entries, &resource.entry))
        && metadata
            .profiles
            .iter()
            .all(|resource| inventory_contains(entries, &resource.path))
        && metadata
            .external
            .iter()
            .all(|resource| inventory_contains(entries, &resource.manifest))
}

fn requested_capabilities(metadata: &OceanExtensionMetadata) -> CapabilitySet {
    let mut network = BTreeSet::new();
    let mut filesystem = BTreeSet::new();
    let mut env = BTreeSet::new();
    let mut secrets = BTreeSet::new();
    for service in &metadata.services {
        network.extend(service.capabilities.network.iter().cloned());
        filesystem.extend(service.capabilities.filesystem.iter().cloned());
        env.extend(service.capabilities.env.iter().cloned());
        secrets.extend(service.capabilities.secrets.iter().map(ToString::to_string));
    }
    CapabilitySet {
        network: network.into_iter().collect(),
        filesystem: filesystem.into_iter().collect(),
        env: env.into_iter().collect(),
        secrets: secrets.into_iter().collect(),
    }
}

fn project_resources(metadata: &OceanExtensionMetadata) -> ExtensionResources {
    ExtensionResources {
        plugins: metadata
            .plugins
            .iter()
            .map(|resource| IdPathResource {
                id: resource.id.clone(),
                path: resource.path.clone(),
            })
            .collect(),
        services: metadata
            .services
            .iter()
            .map(|service| ServiceResourceProjection {
                id: service.id.clone(),
                entry: service.entry.clone(),
                args_count: service.args.len(),
                events: service.events.clone(),
                restart: service.restart.map(|policy| match policy {
                    RestartPolicy::OnFailure => "on-failure".to_string(),
                }),
                health: service
                    .health
                    .as_ref()
                    .map(|health| ServiceHealthProjection {
                        kind: match health.kind {
                            ServiceHealthKind::Process => "process".to_string(),
                        },
                        startup_timeout_ms: health.startup_timeout_ms,
                    }),
                capabilities: CapabilitySet {
                    network: service.capabilities.network.clone(),
                    filesystem: service.capabilities.filesystem.clone(),
                    env: service.capabilities.env.clone(),
                    secrets: service
                        .capabilities
                        .secrets
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                },
            })
            .collect(),
        agents: metadata
            .agents
            .iter()
            .map(|resource| IdPathResource {
                id: resource.id.clone(),
                path: resource.path.clone(),
            })
            .collect(),
        skills: metadata
            .skills
            .iter()
            .map(|resource| IdPathResource {
                id: resource.id.clone(),
                path: resource.path.clone(),
            })
            .collect(),
        profiles: metadata
            .profiles
            .iter()
            .map(|resource| ProfileResourceProjection {
                surface: resource.surface.clone(),
                path: resource.path.clone(),
            })
            .collect(),
        external: metadata
            .external
            .iter()
            .map(|resource| ExternalResourceProjection {
                kind: match resource.kind {
                    ExternalKind::Herdr => "herdr".to_string(),
                },
                manifest: resource.manifest.clone(),
            })
            .collect(),
    }
}

fn grants_are_subset(granted: &CapabilitySet, requested: &CapabilitySet) -> bool {
    fn subset(granted: &[String], requested: &[String]) -> bool {
        let requested: HashSet<&str> = requested.iter().map(String::as_str).collect();
        granted.iter().all(|item| requested.contains(item.as_str()))
    }
    subset(&granted.network, &requested.network)
        && subset(&granted.filesystem, &requested.filesystem)
        && subset(&granted.env, &requested.env)
        && subset(&granted.secrets, &requested.secrets)
}

/// Read one coherent generation and derive only exact-grant, currently effective
/// native service activations. No package code or secret value is touched.
pub(crate) fn read_service_activations(
    config_dir: &FsPath,
    registered_projects: &HashSet<Uuid>,
) -> Result<Vec<ServiceActivation>, StateError> {
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (config_dir, registered_projects);
        return Ok(Vec::new());
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let state = read_locked_state(config_dir)?;
        let Some(root) = state.root.as_ref() else {
            return Ok(Vec::new());
        };
        let canonical_config =
            fs::canonicalize(config_dir).map_err(|_| StateError::Read("config directory"))?;
        let host_version = Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("daemon package version is valid SemVer");
        let mut activations = Vec::new();

        for install in &state.snapshot.installs {
            let Some(enablement) = state
                .snapshot
                .enablement
                .iter()
                .find(|entry| entry.id == install.id)
            else {
                continue;
            };
            if !enablement.global
                && !enablement.projects.iter().any(|project| {
                    project.enabled && registered_projects.contains(&project.project_id)
                })
            {
                continue;
            }
            let Some(trust) = state
                .snapshot
                .grants
                .iter()
                .find(|grant| grant.id == install.id && grant.digest == install.digest)
            else {
                continue;
            };
            let package_directory = open_package(root, install)?;
            let package = snapshot_package(&package_directory)?;
            if package.digest != install.digest {
                return Err(StateError::InvalidRecord("artifact-digest"));
            }
            let raw = RawOceanExtensionManifest::parse(&package.manifest)
                .map_err(|_| StateError::InvalidRecord("manifest"))?;
            let metadata = match raw.validate_metadata(&host_version) {
                Ok(metadata) => metadata,
                Err(ExtensionManifestError::IncompatibleHost { .. }) => continue,
                Err(_) => return Err(StateError::InvalidRecord("manifest")),
            };
            if metadata.id != install.id
                || metadata.version.to_string() != install.version
                || !metadata_paths_exist(&metadata, &package.entries)
            {
                return Err(StateError::InvalidRecord("manifest-identity"));
            }
            let requested = requested_capabilities(&metadata);
            if !grants_are_subset(&trust.capabilities, &requested)
                || metadata.services.iter().any(|service| {
                    !service.capabilities.network.is_empty()
                        || !service.capabilities.filesystem.is_empty()
                })
            {
                // Declaration policy is package-wide and is not represented as
                // containment. One broad service blocks every native service in
                // the package from A2a activation.
                continue;
            }

            let hex = digest_hex(&install.digest).ok_or(StateError::InvalidRecord("install"))?;
            let package_path = canonical_config
                .join("extensions/store")
                .join(&install.id)
                .join(hex);
            verify_open_directory_path(&package_directory, &package_path)?;

            for service in &metadata.services {
                let Some(service_grant) = state.snapshot.service_grants.iter().find(|grant| {
                    grant.id == install.id
                        && grant.digest == install.digest
                        && grant.service_id == service.id
                }) else {
                    continue;
                };
                if !service_is_fully_authorized(service, trust, service_grant) {
                    continue;
                }
                let executable = open_relative_entry(&package_directory, &service.entry)?;
                validate_service_executable(&executable)?;
                let executable_path = package_path.join(
                    normalized_resource_path(&service.entry)
                        .ok_or(StateError::InvalidRecord("service-entry"))?,
                );
                verify_open_file_path(&executable, &executable_path)?;
                let events = service
                    .events
                    .iter()
                    .map(|event| lifecycle_event_kind(event))
                    .collect::<Result<Vec<_>, _>>()?;
                let bound_targets: HashSet<&str> = service_grant
                    .secret_bindings
                    .iter()
                    .map(|binding| binding.target_env.as_str())
                    .collect();
                let environment = service
                    .capabilities
                    .env
                    .iter()
                    .filter(|name| !bound_targets.contains(name.as_str()))
                    .cloned()
                    .collect();
                let package_directory = package_directory
                    .try_clone()
                    .map_err(|_| StateError::Read("extension payload"))?;
                let startup_timeout = Duration::from_millis(
                    service
                        .health
                        .as_ref()
                        .and_then(|health| health.startup_timeout_ms)
                        .unwrap_or(5_000),
                );
                activations.push(ServiceActivation {
                    package_id: install.id.clone(),
                    package_version: install.version.clone(),
                    package_digest: install.digest.clone(),
                    service_id: service.id.clone(),
                    activation_revision: state.snapshot.revision,
                    config_dir: canonical_config.clone(),
                    package_path: package_path.clone(),
                    package_directory,
                    executable,
                    executable_path,
                    args: service.args.clone(),
                    events,
                    environment,
                    secret_bindings: service_grant.secret_bindings.clone(),
                    startup_timeout,
                });
            }
        }
        Ok(activations)
    }
}

fn service_is_fully_authorized(
    service: &MetadataServiceResource,
    trust: &ArtifactTrustGrant,
    grant: &ServiceGrant,
) -> bool {
    if !grant.native_process_ack
        || !service.capabilities.network.is_empty()
        || !service.capabilities.filesystem.is_empty()
        || service.capabilities.env.iter().any(|name| {
            !valid_child_environment_name(name) || reserved_child_environment_name(name)
        })
    {
        return false;
    }
    let requested_env: HashSet<&str> = service
        .capabilities
        .env
        .iter()
        .map(String::as_str)
        .collect();
    if requested_env.len() != service.capabilities.env.len() {
        return false;
    }
    let trusted_env: HashSet<&str> = trust.capabilities.env.iter().map(String::as_str).collect();
    let trusted_secrets: HashSet<&str> = trust
        .capabilities
        .secrets
        .iter()
        .map(String::as_str)
        .collect();
    if service
        .capabilities
        .env
        .iter()
        .any(|name| !trusted_env.contains(name.as_str()))
    {
        return false;
    }
    let requested_secrets: HashSet<String> = service
        .capabilities
        .secrets
        .iter()
        .map(ToString::to_string)
        .collect();
    let bound_secrets: HashSet<&str> = grant
        .secret_bindings
        .iter()
        .map(|binding| binding.reference.as_str())
        .collect();
    requested_secrets.len() == service.capabilities.secrets.len()
        && requested_secrets.len() == bound_secrets.len()
        && requested_secrets
            .iter()
            .all(|reference| trusted_secrets.contains(reference.as_str()))
        && requested_secrets
            .iter()
            .all(|reference| bound_secrets.contains(reference.as_str()))
}

#[cfg(unix)]
fn verify_open_directory_path(directory: &File, path: &FsPath) -> Result<(), StateError> {
    use std::os::unix::fs::MetadataExt;

    let opened = directory
        .metadata()
        .map_err(|_| StateError::Read("extension payload"))?;
    let named = fs::metadata(path).map_err(|_| StateError::Read("extension payload"))?;
    if !named.is_dir() || opened.dev() != named.dev() || opened.ino() != named.ino() {
        return Err(StateError::InvalidComponent("extension payload"));
    }
    Ok(())
}

#[cfg(unix)]
fn verify_open_file_path(file: &File, path: &FsPath) -> Result<(), StateError> {
    use std::os::unix::fs::MetadataExt;

    let opened = file
        .metadata()
        .map_err(|_| StateError::Read("service executable"))?;
    let named = fs::metadata(path).map_err(|_| StateError::Read("service executable"))?;
    if !named.is_file() || opened.dev() != named.dev() || opened.ino() != named.ino() {
        return Err(StateError::InvalidComponent("service executable"));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_service_executable(executable: &File) -> Result<(), StateError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = executable
        .metadata()
        .map_err(|_| StateError::Read("service executable"))?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.permissions().mode() & 0o111 == 0 {
        return Err(StateError::InvalidComponent("service executable"));
    }
    Ok(())
}

fn lifecycle_event_kind(value: &str) -> Result<LifecycleEventKind, StateError> {
    match value {
        "daemon_started" => Ok(LifecycleEventKind::DaemonStarted),
        "session_started" => Ok(LifecycleEventKind::SessionStarted),
        "turn_started" => Ok(LifecycleEventKind::TurnStarted),
        "permission_requested" => Ok(LifecycleEventKind::PermissionRequested),
        "permission_resolved" => Ok(LifecycleEventKind::PermissionResolved),
        "tool_started" => Ok(LifecycleEventKind::ToolStarted),
        "tool_finished" => Ok(LifecycleEventKind::ToolFinished),
        "turn_finished" => Ok(LifecycleEventKind::TurnFinished),
        "session_stopped" => Ok(LifecycleEventKind::SessionStopped),
        "daemon_stopping" => Ok(LifecycleEventKind::DaemonStopping),
        _ => Err(StateError::InvalidRecord("service-event")),
    }
}

fn inspect_extension(
    state: &LockedState,
    id: &str,
    project_id: Option<Uuid>,
    registered_projects: &HashSet<Uuid>,
    host_version: &Version,
) -> Option<ExtensionInspection> {
    let install = state
        .snapshot
        .installs
        .iter()
        .find(|install| install.id == id);
    let enabled_entry = state
        .snapshot
        .enablement
        .iter()
        .find(|entry| entry.id == id);
    let has_any_state = install.is_some()
        || enabled_entry.is_some()
        || state.snapshot.grants.iter().any(|grant| grant.id == id);
    if !has_any_state {
        return None;
    }

    let mut diagnostics = Vec::new();
    let mut artifact_verified = false;
    let mut manifest_valid = false;
    let mut compatible = false;
    let mut name = None;
    let mut min_ocean_version = None;
    let mut requested = CapabilitySet::default();
    let mut resources = ExtensionResources::default();

    if let (Some(root), Some(install)) = (&state.root, install) {
        let artifact_result = open_package(root, install)
            .and_then(|package| snapshot_package(&package))
            .and_then(|package| {
                if package.digest != install.digest {
                    return Err(StateError::InvalidRecord("artifact-digest"));
                }
                artifact_verified = true;
                let raw = RawOceanExtensionManifest::parse(&package.manifest)
                    .map_err(|_| StateError::Parse("ocean-extension.toml"))?;
                let (metadata, host_compatible) = match raw.clone().validate_metadata(host_version)
                {
                    Ok(metadata) => (metadata, true),
                    Err(ExtensionManifestError::IncompatibleHost { .. }) => {
                        let metadata = raw
                            .validate_metadata(&Version::new(u64::MAX, u64::MAX, u64::MAX))
                            .map_err(|_| StateError::InvalidRecord("manifest"))?;
                        (metadata, false)
                    }
                    Err(_) => return Err(StateError::InvalidRecord("manifest")),
                };
                if metadata.id != install.id || metadata.version.to_string() != install.version {
                    return Err(StateError::InvalidRecord("manifest-identity"));
                }
                if !metadata_paths_exist(&metadata, &package.entries) {
                    return Err(StateError::InvalidRecord("manifest-inventory"));
                }
                name = Some(metadata.name.clone());
                min_ocean_version = Some(metadata.min_ocean_version.to_string());
                requested = requested_capabilities(&metadata);
                resources = project_resources(&metadata);
                compatible = host_compatible;
                manifest_valid = true;
                if !host_compatible {
                    diagnostics.push(ExtensionDiagnostic::error(
                        "host_incompatible",
                        "extension requires a newer Ocean version",
                    ));
                }
                Ok(())
            });
        if let Err(error) = artifact_result {
            let (code, message) = match error {
                StateError::InvalidRecord("artifact-digest") => (
                    "artifact_digest_mismatch",
                    "stored package bytes do not match the installed artifact digest",
                ),
                StateError::InvalidRecord("manifest-identity") => (
                    "manifest_identity_mismatch",
                    "stored package manifest identity does not match the install record",
                ),
                StateError::InvalidRecord("manifest-inventory") => (
                    "manifest_resource_missing",
                    "stored package manifest declares a resource absent from the anchored artifact",
                ),
                StateError::Parse("ocean-extension.toml")
                | StateError::InvalidRecord("manifest") => (
                    "manifest_invalid",
                    "stored package manifest is structurally invalid",
                ),
                _ => (
                    "artifact_unavailable",
                    "stored package payload is missing, unsafe, unreadable, or over inspection limits",
                ),
            };
            diagnostics.push(ExtensionDiagnostic::error(code, message));
        }
    } else {
        diagnostics.push(ExtensionDiagnostic::error(
            "not_installed",
            "extension has trust or enablement state but no installed artifact",
        ));
    }

    let matching_grant = install.and_then(|install| {
        state
            .snapshot
            .grants
            .iter()
            .find(|grant| grant.id == id && grant.digest == install.digest)
    });
    let granted = matching_grant
        .map(|grant| grant.capabilities.clone())
        .unwrap_or_default();
    let grants_valid = matching_grant.is_some() && grants_are_subset(&granted, &requested);
    if matching_grant.is_some() && !grants_valid {
        diagnostics.push(ExtensionDiagnostic::error(
            "grant_widens_manifest",
            "operator capability grants exceed the package manifest request",
        ));
    } else if install.is_some() && matching_grant.is_none() {
        diagnostics.push(ExtensionDiagnostic::info(
            "artifact_untrusted",
            "installed artifact has no operator trust grant for its exact digest",
        ));
    }
    if let Some(install) = install {
        if state
            .snapshot
            .grants
            .iter()
            .any(|grant| grant.id == id && grant.digest != install.digest)
        {
            diagnostics.push(ExtensionDiagnostic::warning(
                "stale_trust_grant",
                "a trust grant exists for a different artifact digest",
            ));
        }
    }

    let global_enabled = enabled_entry.is_some_and(|entry| entry.global);
    let project_override = project_id.and_then(|wanted| {
        enabled_entry.and_then(|entry| {
            entry
                .projects
                .iter()
                .find(|override_| override_.project_id == wanted)
                .map(|override_| override_.enabled)
        })
    });
    if enabled_entry.is_some_and(|entry| {
        entry
            .projects
            .iter()
            .any(|override_| !registered_projects.contains(&override_.project_id))
    }) {
        diagnostics.push(ExtensionDiagnostic::warning(
            "stale_project_override",
            "enablement contains an override for an unregistered project and it was ignored",
        ));
    }
    let enabled = project_override.unwrap_or(global_enabled);
    let trusted = artifact_verified && manifest_valid && compatible && grants_valid;
    let effective = install.is_some() && trusted && enabled;

    Some(ExtensionInspection {
        id: id.to_string(),
        state_revision: state.snapshot.revision,
        project_id,
        installed: install.is_some(),
        name,
        version: install.map(|install| install.version.clone()),
        min_ocean_version,
        digest: install.map(|install| install.digest.clone()),
        source: install.map(|install| install.source.clone()),
        artifact_verified,
        manifest_valid,
        compatible,
        trusted,
        global_enabled,
        project_override,
        enabled,
        effective,
        requested_capabilities: requested,
        granted_capabilities: granted,
        resources,
        health: StaticHealthProjection {
            probe_run: false,
            last_observed: None,
        },
        diagnostics,
    })
}

#[derive(Debug)]
enum LoadError {
    BadExtensionId,
    BadProjectId,
    ProjectNotFound,
    ProjectRegistry,
    Capacity,
    State(StateError),
    Join,
}

async fn load_inspection(
    state: AppState,
    id: String,
    query: ExtensionStateQuery,
) -> Result<Option<ExtensionInspection>, LoadError> {
    validate_extension_id(&id).map_err(|_| LoadError::BadExtensionId)?;
    let project_id = query
        .project_id
        .as_deref()
        .map(Uuid::parse_str)
        .transpose()
        .map_err(|_| LoadError::BadProjectId)?;
    let projects = state
        .runtime
        .list_projects()
        .map_err(|_| LoadError::ProjectRegistry)?;
    let registered: HashSet<Uuid> = projects.into_iter().map(|project| project.id).collect();
    if project_id.is_some_and(|id| !registered.contains(&id)) {
        return Err(LoadError::ProjectNotFound);
    }
    let permit = inspection_limiter()
        .try_acquire_owned()
        .map_err(|_| LoadError::Capacity)?;
    let config_dir = state.runtime.config_dir().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let locked = read_locked_state(&config_dir).map_err(LoadError::State)?;
        let host_version = Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("daemon package version is valid SemVer");
        Ok(inspect_extension(
            &locked,
            &id,
            project_id,
            &registered,
            &host_version,
        ))
    })
    .await
    .map_err(|_| LoadError::Join)?
}

fn load_error_response(error: LoadError, doctor: bool) -> (StatusCode, Json<Value>) {
    match error {
        LoadError::BadExtensionId => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "invalid_extension_id"})),
        ),
        LoadError::BadProjectId => (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "invalid_project_id"})),
        ),
        LoadError::ProjectNotFound => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "project_not_found"})),
        ),
        LoadError::ProjectRegistry => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": "project_registry_unavailable"})),
        ),
        LoadError::Capacity => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"ok": false, "error": "extension_inspection_capacity"})),
        ),
        LoadError::State(error) if doctor => (
            StatusCode::OK,
            Json(json!({
                "ok": false,
                "extension": Value::Null,
                "checks": {
                    "coherent_state": false,
                    "artifact_digest": false,
                    "manifest": false,
                    "trust_binding": false,
                    "enablement": false,
                    "package_code_executed": false
                },
                "diagnostics": [{
                    "severity": "error",
                    "code": error.code(),
                    "message": "daemon-owned extension state is unavailable or incoherent"
                }]
            })),
        ),
        LoadError::State(_) | LoadError::Join => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": "extension_state_unavailable"})),
        ),
    }
}

/// Read one extension's separately persisted install/trust/enablement state.
pub(super) async fn inspect(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ExtensionStateQuery>,
) -> (StatusCode, Json<Value>) {
    match load_inspection(state, id, query).await {
        Ok(Some(inspection)) => (
            StatusCode::OK,
            Json(json!({"ok": true, "extension": inspection})),
        ),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "extension_not_found"})),
        ),
        Err(error) => load_error_response(error, false),
    }
}

/// Run static package/state diagnostics. No plugin, service, hook, Git, shell,
/// provider, health probe, or package executable is invoked.
pub(super) async fn doctor(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<ExtensionStateQuery>,
) -> (StatusCode, Json<Value>) {
    match load_inspection(state, id, query).await {
        Ok(Some(inspection)) => {
            let healthy = !inspection
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == DiagnosticSeverity::Error);
            let checks = DoctorChecks {
                coherent_state: true,
                artifact_digest: inspection.artifact_verified,
                manifest: inspection.manifest_valid,
                trust_binding: inspection.trusted,
                enablement: inspection.enabled,
                package_code_executed: false,
            };
            let diagnostics = inspection.diagnostics.clone();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": healthy,
                    "extension": inspection,
                    "checks": checks,
                    "diagnostics": diagnostics
                })),
            )
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "error": "extension_not_found"})),
        ),
        Err(error) => load_error_response(error, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    const ID: &str = "example.phase-one";
    const VERSION: &str = "0.1.0";

    struct Fixture {
        config: tempfile::TempDir,
        digest: String,
        marker: PathBuf,
    }

    fn write_json(path: &FsPath, value: &Value) {
        fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn package_manifest() -> &'static str {
        r#"schema_version = 1
id = "example.phase-one"
name = "Phase One"
version = "0.1.0"
min_ocean_version = "0.1.0"

[[plugins]]
id = "reader"
path = "plugins/reader"

[[services]]
id = "bridge"
entry = "services/bridge"
events = ["turn_started"]
restart = "on-failure"
[services.health]
kind = "process"
startup_timeout_ms = 5000
[services.capabilities]
network = ["api.example.com"]
env = ["EXAMPLE_ENV"]
"#
    }

    fn create_package(staging: &FsPath, marker: &FsPath) {
        fs::create_dir_all(staging.join("plugins/reader")).unwrap();
        fs::create_dir_all(staging.join("services")).unwrap();
        fs::write(staging.join("ocean-extension.toml"), package_manifest()).unwrap();
        fs::write(
            staging.join("services/bridge"),
            format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
        )
        .unwrap();
        fs::write(
            staging.join("run-me"),
            format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for executable in ["run-me", "services/bridge"] {
                let path = staging.join(executable);
                let mut permissions = fs::metadata(&path).unwrap().permissions();
                permissions.set_mode(0o755);
                fs::set_permissions(path, permissions).unwrap();
            }
        }
    }

    fn fixture(trusted: bool, enabled: bool) -> Fixture {
        let config = tempfile::tempdir().unwrap();
        let root = config.path().join("extensions");
        let id_root = root.join("store").join(ID);
        let staging = id_root.join("staging");
        let marker = config.path().join("PACKAGE_CODE_RAN");
        create_package(&staging, &marker);
        let staging_file = File::open(&staging).unwrap();
        let digest = snapshot_package(&staging_file).unwrap().digest;
        let hex = digest_hex(&digest).unwrap();
        fs::rename(&staging, id_root.join(hex)).unwrap();
        fs::write(root.join(".state.lock"), "").unwrap();
        let revision = 7;
        write_json(
            &root.join("installs.json"),
            &json!({
                "schema_version": 1,
                "state_revision": revision,
                "installs": [{
                    "id": ID,
                    "version": VERSION,
                    "digest": digest,
                    "source": {"kind": "local-path", "locator": "/tmp/example"}
                }]
            }),
        );
        write_json(
            &root.join("trust.json"),
            &json!({
                "schema_version": 1,
                "state_revision": revision,
                "grants": if trusted { json!([{
                    "id": ID,
                    "digest": digest,
                    "capabilities": {
                        "network": ["api.example.com"],
                        "env": ["EXAMPLE_ENV"]
                    }
                }]) } else { json!([]) }
            }),
        );
        write_json(
            &root.join("enabled.json"),
            &json!({
                "schema_version": 1,
                "state_revision": revision,
                "extensions": [{"id": ID, "global": enabled, "projects": []}]
            }),
        );
        Fixture {
            config,
            digest,
            marker,
        }
    }

    fn inspect_fixture(fixture: &Fixture) -> ExtensionInspection {
        let state = read_locked_state(fixture.config.path()).unwrap();
        inspect_extension(
            &state,
            ID,
            None,
            &HashSet::new(),
            &Version::parse("0.1.0").unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn absent_extensions_directory_is_empty_and_read_only() {
        let config = tempfile::tempdir().unwrap();
        let state = read_locked_state(config.path()).unwrap();
        assert_eq!(state.snapshot.revision, 0);
        assert!(state.snapshot.installs.is_empty());
        assert!(!config.path().join("extensions").exists());
    }

    fn service_grants_value(fixture: &Fixture, ack: bool) -> Value {
        json!({
            "schema_version": 1,
            "state_revision": 7,
            "service_grants": [{
                "id": ID,
                "digest": fixture.digest,
                "service_id": "bridge",
                "native_process_ack": ack,
                "secret_bindings": []
            }]
        })
    }

    #[test]
    fn hand_authored_service_grant_fixtures_cover_valid_invalid_and_bounds() {
        let valid: ServiceGrantsFile = serde_json::from_str(include_str!(
            "../tests/fixtures/extension-registry/valid-service-grants.json"
        ))
        .unwrap();
        assert_eq!(valid.schema_version, 1);
        assert_eq!(valid.service_grants.len(), 1);
        assert!(valid.service_grants[0].native_process_ack);
        assert!(validate_secret_bindings(&valid.service_grants[0].secret_bindings).is_ok());

        let invalid: ServiceGrantsFile = serde_json::from_str(include_str!(
            "../tests/fixtures/extension-registry/invalid-service-grants.json"
        ))
        .unwrap();
        assert!(!invalid.service_grants[0].native_process_ack);

        let duplicate_binding = vec![
            SecretBinding {
                target_env: "TOKEN_A".to_owned(),
                reference: "env:SOURCE_A".to_owned(),
            },
            SecretBinding {
                target_env: "TOKEN_A".to_owned(),
                reference: "env:SOURCE_B".to_owned(),
            },
        ];
        assert_eq!(
            validate_secret_bindings(&duplicate_binding),
            Err(StateError::InvalidRecord("secret-binding"))
        );
        for (target, reference) in [
            ("PATH", "env:SOURCE"),
            ("lowercase", "env:SOURCE"),
            ("TARGET", "vault:SOURCE"),
            ("TARGET", "env:lowercase"),
        ] {
            assert!(validate_secret_bindings(&[SecretBinding {
                target_env: target.to_owned(),
                reference: reference.to_owned(),
            }])
            .is_err());
        }
        let too_many = (0..=MAX_SECRET_BINDINGS)
            .map(|index| SecretBinding {
                target_env: format!("TARGET_{index:03}"),
                reference: format!("env:SOURCE_{index:03}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            validate_secret_bindings(&too_many),
            Err(StateError::InvalidRecord("secret-binding-count"))
        );
    }

    #[test]
    fn a0_absence_four_file_mixed_generation_and_downgrade_are_exact() {
        let fixture = fixture(true, true);
        let root = fixture.config.path().join("extensions");

        let a0 = read_locked_state(fixture.config.path()).unwrap();
        assert!(a0.snapshot.service_grants.is_empty());
        assert!(!root.join("service-grants.json").exists());

        write_json(
            &root.join("service-grants.json"),
            &service_grants_value(&fixture, true),
        );
        let upgraded = read_locked_state(fixture.config.path()).unwrap();
        assert_eq!(upgraded.snapshot.service_grants.len(), 1);

        fs::write(
            root.join("service-grants.json"),
            include_str!(
                "../tests/fixtures/extension-registry/mixed-generation-service-grants.json"
            ),
        )
        .unwrap();
        assert_eq!(
            read_locked_state(fixture.config.path()).err(),
            Some(StateError::RevisionMismatch)
        );

        fs::write(
            root.join("service-grants.json"),
            include_str!("../tests/fixtures/extension-registry/downgrade-service-grants.json"),
        )
        .unwrap();
        // Simulate the accepted A0 reader: it reads the unchanged strict three
        // files and ignores the additive companion entirely.
        let installs: InstallsFile =
            read_state_json_at(upgraded.root.as_ref().unwrap(), "installs.json").unwrap();
        let trust: TrustFile =
            read_state_json_at(upgraded.root.as_ref().unwrap(), "trust.json").unwrap();
        let enabled: EnabledFile =
            read_state_json_at(upgraded.root.as_ref().unwrap(), "enabled.json").unwrap();
        assert_eq!(
            (
                installs.state_revision,
                trust.state_revision,
                enabled.state_revision
            ),
            (7, 7, 7)
        );
        assert_eq!(
            read_locked_state(fixture.config.path())
                .unwrap()
                .snapshot
                .revision,
            7
        );
    }

    #[test]
    fn native_acknowledgement_is_exact_and_absence_executes_nothing() {
        let fixture = fixture(true, true);
        let root = fixture.config.path().join("extensions");
        assert!(
            read_service_activations(fixture.config.path(), &HashSet::new())
                .unwrap()
                .is_empty()
        );
        assert!(!fixture.marker.exists());

        write_json(
            &root.join("service-grants.json"),
            &service_grants_value(&fixture, false),
        );
        assert_eq!(
            read_locked_state(fixture.config.path()).err(),
            Some(StateError::InvalidRecord("service-grant"))
        );
        assert!(!fixture.marker.exists());
    }

    #[test]
    fn sole_reader_derives_exact_noop_activation_without_resolving_or_executing() {
        let fixture = fixture(true, true);
        let root = fixture.config.path().join("extensions");
        let package = root
            .join("store")
            .join(ID)
            .join(digest_hex(&fixture.digest).unwrap());
        let manifest = fs::read_to_string(package.join("ocean-extension.toml"))
            .unwrap()
            .replace("network = [\"api.example.com\"]\n", "")
            + "secrets = [\"env:OCEAN_EXAMPLE_SECRET\"]\n";
        fs::write(package.join("ocean-extension.toml"), manifest).unwrap();
        let package_file = File::open(&package).unwrap();
        let digest = snapshot_package(&package_file).unwrap().digest;
        drop(package_file);
        let new_package = package.parent().unwrap().join(digest_hex(&digest).unwrap());
        fs::rename(&package, &new_package).unwrap();
        write_json(
            &root.join("installs.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "installs": [{
                    "id": ID,
                    "version": VERSION,
                    "digest": digest,
                    "source": {"kind": "local-path", "locator": "/tmp/example"}
                }]
            }),
        );
        write_json(
            &root.join("trust.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "grants": [{
                    "id": ID,
                    "digest": digest,
                    "capabilities": {
                        "env": ["EXAMPLE_ENV"],
                        "secrets": ["env:OCEAN_EXAMPLE_SECRET"]
                    }
                }]
            }),
        );
        write_json(
            &root.join("service-grants.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "service_grants": [{
                    "id": ID,
                    "digest": digest,
                    "service_id": "bridge",
                    "native_process_ack": true,
                    "secret_bindings": [{
                        "target_env": "EXAMPLE_ENV",
                        "reference": "env:OCEAN_EXAMPLE_SECRET"
                    }]
                }]
            }),
        );

        let mut activations =
            read_service_activations(fixture.config.path(), &HashSet::new()).unwrap();
        assert_eq!(activations.len(), 1);
        let activation = activations.pop().unwrap();
        assert_eq!(activation.package_id, ID);
        assert_eq!(activation.service_id, "bridge");
        assert!(activation.environment.is_empty());
        assert_eq!(activation.secret_bindings[0].target_env, "EXAMPLE_ENV");
        assert!(!fixture.marker.exists());
    }

    #[test]
    fn service_grant_order_duplicates_unknown_fields_and_revision_fail_closed() {
        let fixture = fixture(true, true);
        let root = fixture.config.path().join("extensions");
        let grant = service_grants_value(&fixture, true)["service_grants"][0].clone();
        write_json(
            &root.join("service-grants.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "service_grants": [grant.clone(), grant]
            }),
        );
        assert_eq!(
            read_locked_state(fixture.config.path()).err(),
            Some(StateError::InvalidRecord("service-grant-order"))
        );
        write_json(
            &root.join("service-grants.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "service_grants": [],
                "unexpected": true
            }),
        );
        assert_eq!(
            read_locked_state(fixture.config.path()).err(),
            Some(StateError::Parse("service-grants.json"))
        );
    }

    #[test]
    fn install_trust_and_enablement_are_independent_and_inspectable() {
        for (trusted, enabled, effective) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, true),
        ] {
            let fixture = fixture(trusted, enabled);
            let inspection = inspect_fixture(&fixture);
            assert!(inspection.installed);
            assert_eq!(inspection.trusted, trusted);
            assert_eq!(inspection.enabled, enabled);
            assert_eq!(inspection.effective, effective);
            assert!(inspection.artifact_verified);
            assert!(inspection.manifest_valid);
            assert_eq!(inspection.name.as_deref(), Some("Phase One"));
            assert_eq!(inspection.resources.plugins[0].id, "reader");
            assert_eq!(inspection.resources.services[0].id, "bridge");
            assert!(!inspection.health.probe_run);
            assert!(!fixture.marker.exists(), "inspection executed package code");
        }
    }

    #[test]
    fn changed_payload_digest_invalidates_artifact_and_trust() {
        let fixture = fixture(true, true);
        let package = fixture
            .config
            .path()
            .join("extensions/store")
            .join(ID)
            .join(digest_hex(&fixture.digest).unwrap());
        fs::write(package.join("payload-change"), "changed").unwrap();
        let inspection = inspect_fixture(&fixture);
        assert!(!inspection.artifact_verified);
        assert!(!inspection.trusted);
        assert!(!inspection.effective);
        assert!(inspection
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "artifact_digest_mismatch"));
    }

    #[test]
    fn missing_declared_resource_fails_anchored_manifest_inventory() {
        let fixture = fixture(true, true);
        let package = fixture
            .config
            .path()
            .join("extensions/store")
            .join(ID)
            .join(digest_hex(&fixture.digest).unwrap());
        fs::remove_file(package.join("services/bridge")).unwrap();
        let package_dir = File::open(&package).unwrap();
        let changed_digest = snapshot_package(&package_dir).unwrap().digest;
        let changed_hex = digest_hex(&changed_digest).unwrap();
        fs::rename(&package, package.parent().unwrap().join(changed_hex)).unwrap();
        let root = fixture.config.path().join("extensions");
        let install = json!({
            "id": ID,
            "version": VERSION,
            "digest": changed_digest,
            "source": {"kind": "local-path", "locator": "/tmp/example"}
        });
        write_json(
            &root.join("installs.json"),
            &json!({"schema_version": 1, "state_revision": 7, "installs": [install]}),
        );
        let inspection = inspect_fixture(&fixture);
        assert!(inspection.artifact_verified);
        assert!(!inspection.manifest_valid);
        assert!(inspection
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "manifest_resource_missing"));
    }

    #[test]
    fn structurally_valid_incompatible_manifest_stays_inspectable_but_inactive() {
        let fixture = fixture(true, true);
        let root = fixture.config.path().join("extensions");
        let package = root
            .join("store")
            .join(ID)
            .join(digest_hex(&fixture.digest).unwrap());
        let manifest = fs::read_to_string(package.join("ocean-extension.toml"))
            .unwrap()
            .replace(
                "min_ocean_version = \"0.1.0\"",
                "min_ocean_version = \"9.0.0\"",
            );
        fs::write(package.join("ocean-extension.toml"), manifest).unwrap();
        let directory = File::open(&package).unwrap();
        let digest = snapshot_package(&directory).unwrap().digest;
        drop(directory);
        let new_path = package.parent().unwrap().join(digest_hex(&digest).unwrap());
        fs::rename(&package, new_path).unwrap();
        write_json(
            &root.join("installs.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "installs": [{
                    "id": ID,
                    "version": VERSION,
                    "digest": digest,
                    "source": {"kind": "local-path", "locator": "/tmp/example"}
                }]
            }),
        );
        write_json(
            &root.join("trust.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "grants": [{
                    "id": ID,
                    "digest": digest,
                    "capabilities": {
                        "network": ["api.example.com"],
                        "env": ["EXAMPLE_ENV"]
                    }
                }]
            }),
        );
        let inspection = inspect_fixture(&fixture);
        assert!(inspection.artifact_verified);
        assert!(inspection.manifest_valid);
        assert!(!inspection.compatible);
        assert_eq!(inspection.min_ocean_version.as_deref(), Some("9.0.0"));
        assert_eq!(inspection.resources.services[0].id, "bridge");
        assert!(!inspection.trusted);
        assert!(!inspection.effective);
        assert!(inspection
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "host_incompatible"));
    }

    #[test]
    fn overbroad_grant_never_widens_manifest_capabilities() {
        let fixture = fixture(true, true);
        let root = fixture.config.path().join("extensions");
        write_json(
            &root.join("trust.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "grants": [{
                    "id": ID,
                    "digest": fixture.digest,
                    "capabilities": {"env": ["UNREQUESTED_SECRET"]}
                }]
            }),
        );
        let inspection = inspect_fixture(&fixture);
        assert!(!inspection.trusted);
        assert!(!inspection.effective);
        assert!(inspection
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "grant_widens_manifest"));
    }

    #[test]
    fn registered_project_override_selects_enablement_but_not_trust() {
        let fixture = fixture(false, false);
        let root = fixture.config.path().join("extensions");
        let project = Uuid::new_v4();
        write_json(
            &root.join("enabled.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "extensions": [{
                    "id": ID,
                    "global": false,
                    "projects": [{"project_id": project, "enabled": true}]
                }]
            }),
        );
        let state = read_locked_state(fixture.config.path()).unwrap();
        let inspection = inspect_extension(
            &state,
            ID,
            Some(project),
            &HashSet::from([project]),
            &Version::parse("0.1.0").unwrap(),
        )
        .unwrap();
        assert_eq!(inspection.project_override, Some(true));
        assert!(inspection.enabled);
        assert!(!inspection.trusted);
        assert!(!inspection.effective);
    }

    #[test]
    fn stale_project_override_is_diagnosed_and_ignored() {
        let fixture = fixture(true, false);
        let root = fixture.config.path().join("extensions");
        write_json(
            &root.join("enabled.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "extensions": [{
                    "id": ID,
                    "global": false,
                    "projects": [{"project_id": Uuid::new_v4(), "enabled": true}]
                }]
            }),
        );
        let inspection = inspect_fixture(&fixture);
        assert!(!inspection.enabled);
        assert!(inspection
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "stale_project_override"));
    }

    #[test]
    fn partial_malformed_and_revision_mismatched_state_fail_closed() {
        let config = tempfile::tempdir().unwrap();
        let root = config.path().join("extensions");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".state.lock"), "").unwrap();
        fs::write(root.join("installs.json"), "{}").unwrap();
        assert_eq!(
            read_locked_state(config.path()).err(),
            Some(StateError::Parse("installs.json"))
        );

        let fixture = fixture(true, true);
        let root = fixture.config.path().join("extensions");
        write_json(
            &root.join("enabled.json"),
            &json!({"schema_version": 1, "state_revision": 8, "extensions": []}),
        );
        assert_eq!(
            read_locked_state(fixture.config.path()).err(),
            Some(StateError::RevisionMismatch)
        );
    }

    #[test]
    fn duplicate_state_identities_fail_closed() {
        let fixture = fixture(true, true);
        let root = fixture.config.path().join("extensions");
        let install = json!({
            "id": ID,
            "version": VERSION,
            "digest": fixture.digest,
            "source": {"kind": "local-path", "locator": "/tmp/example"}
        });
        write_json(
            &root.join("installs.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 7,
                "installs": [install.clone(), install]
            }),
        );
        assert_eq!(
            read_locked_state(fixture.config.path()).err(),
            Some(StateError::InvalidRecord("install"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_source_locators_must_be_lexically_canonical_absolute_paths() {
        let valid = InstallSource {
            kind: InstallSourceKind::LocalPath,
            locator: "/tmp/example".into(),
            revision: None,
        };
        assert!(validate_source(&valid).is_ok());

        for locator in [
            "tmp/example",
            "/tmp/../example",
            "/tmp/./example",
            "/tmp//example",
            "/tmp/example/",
        ] {
            let source = InstallSource {
                kind: InstallSourceKind::LocalPath,
                locator: locator.into(),
                revision: None,
            };
            assert_eq!(
                validate_source(&source),
                Err(StateError::InvalidRecord("source")),
                "accepted noncanonical local source {locator}"
            );
        }
    }

    #[test]
    fn source_locators_cannot_reflect_credentials_or_floating_git_revisions() {
        let valid_revision = "a".repeat(40);
        for locator in [
            "https://github.com/example/repo",
            "ssh://git@github.com/example/repo",
            "git@github.com:example/repo",
        ] {
            let valid = InstallSource {
                kind: InstallSourceKind::Git,
                locator: locator.into(),
                revision: Some(valid_revision.clone()),
            };
            assert!(validate_source(&valid).is_ok(), "rejected {locator}");
        }
        for locator in [
            "https://token@github.com/example/repo",
            "https://github.com/example/repo?token=secret",
            "https://github.com/example/repo#secret",
            "oauth2:TOKEN@github.com:example/repo",
            "TOKEN=secret",
            "git@TOKEN@github.com:example/repo",
        ] {
            let source = InstallSource {
                kind: InstallSourceKind::Git,
                locator: locator.into(),
                revision: Some(valid_revision.clone()),
            };
            assert_eq!(
                validate_source(&source),
                Err(StateError::InvalidRecord("source"))
            );
        }
        let floating = InstallSource {
            kind: InstallSourceKind::Git,
            locator: "git@github.com:example/repo".into(),
            revision: Some("main".into()),
        };
        assert_eq!(
            validate_source(&floating),
            Err(StateError::InvalidRecord("source"))
        );
    }

    #[test]
    fn busy_state_lock_is_bounded() {
        let fixture = fixture(true, true);
        let lock_path = fixture.config.path().join("extensions/.state.lock");
        let lock = File::open(lock_path).unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        let started = Instant::now();
        assert_eq!(
            read_locked_state(fixture.config.path()).err(),
            Some(StateError::LockBusy)
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        fs2::FileExt::unlock(&lock).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_state_file_is_rejected() {
        use std::os::unix::fs::symlink;

        let fixture = fixture(true, true);
        let root = fixture.config.path().join("extensions");
        fs::remove_file(root.join("trust.json")).unwrap();
        symlink(root.join("installs.json"), root.join("trust.json")).unwrap();
        assert_eq!(
            read_locked_state(fixture.config.path()).err(),
            Some(StateError::InvalidComponent("trust.json"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn fifo_state_and_package_entries_fail_without_blocking() {
        use std::os::unix::ffi::OsStrExt;

        fn fifo(path: &FsPath) {
            let path = CString::new(path.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }

        let fixture = fixture(true, true);
        let root = fixture.config.path().join("extensions");
        fs::remove_file(root.join("trust.json")).unwrap();
        fifo(&root.join("trust.json"));
        let started = Instant::now();
        assert_eq!(
            read_locked_state(fixture.config.path()).err(),
            Some(StateError::InvalidComponent("trust.json"))
        );
        assert!(started.elapsed() < Duration::from_secs(1));

        let package = fixture
            .config
            .path()
            .join("extensions/store")
            .join(ID)
            .join(digest_hex(&fixture.digest).unwrap());
        fifo(&package.join("blocked-fifo"));
        let directory = File::open(package).unwrap();
        let started = Instant::now();
        assert!(snapshot_package(&directory).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn directory_enumeration_stops_at_remaining_entry_budget() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..4 {
            fs::write(temp.path().join(format!("entry-{index}")), "x").unwrap();
        }
        let directory = File::open(temp.path()).unwrap();
        assert_eq!(
            directory_names(&directory, 3).err(),
            Some(StateError::Oversized("extension payload"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn anchored_state_root_survives_path_replacement_without_following_it() {
        use std::os::unix::fs::symlink;

        let fixture = fixture(true, true);
        let requested = fixture.config.path().join("extensions");
        let root = open_extensions_root(fixture.config.path())
            .unwrap()
            .unwrap();
        let moved = fixture.config.path().join("extensions-original");
        fs::rename(&requested, &moved).unwrap();
        let attacker = fixture.config.path().join("attacker");
        fs::create_dir(&attacker).unwrap();
        fs::write(attacker.join("installs.json"), "ATTACKER").unwrap();
        symlink(&attacker, &requested).unwrap();
        let installs: InstallsFile = read_state_json_at(&root, "installs.json").unwrap();
        assert_eq!(installs.installs[0].id, ID);
    }

    #[test]
    fn final_revalidation_detects_a_previously_hashed_file_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("marker");
        create_package(temp.path(), &marker);
        let package = File::open(temp.path()).unwrap();
        let mut walk = PackageWalk {
            records: Vec::new(),
            entries: BTreeSet::new(),
            manifest: None,
            total_bytes: 0,
            total_entries: 0,
        };
        walk_package_dir(&package, "", 0, &mut walk).unwrap();
        fs::write(temp.path().join("services/bridge"), "mutated after hash").unwrap();
        assert!(revalidate_package(&package, &walk.records).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn anchored_package_handle_survives_digest_path_replacement() {
        use std::os::unix::fs::symlink;

        let fixture = fixture(true, true);
        let state = read_locked_state(fixture.config.path()).unwrap();
        let install = &state.snapshot.installs[0];
        let package = open_package(state.root.as_ref().unwrap(), install).unwrap();
        let package_path = fixture
            .config
            .path()
            .join("extensions/store")
            .join(ID)
            .join(digest_hex(&fixture.digest).unwrap());
        let moved = package_path.with_extension("original");
        fs::rename(&package_path, &moved).unwrap();
        let attacker = fixture.config.path().join("attacker-package");
        fs::create_dir(&attacker).unwrap();
        fs::write(attacker.join("ocean-extension.toml"), "attacker").unwrap();
        symlink(&attacker, &package_path).unwrap();
        assert_eq!(snapshot_package(&package).unwrap().digest, fixture.digest);
    }

    #[cfg(unix)]
    #[test]
    fn tree_digest_v1_matches_frozen_known_answer() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("assets")).unwrap();
        fs::write(temp.path().join("bin"), b"abc").unwrap();
        fs::write(temp.path().join("ocean-extension.toml"), b"x").unwrap();
        fs::set_permissions(
            temp.path().join("assets"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::set_permissions(temp.path().join("bin"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(
            temp.path().join("ocean-extension.toml"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let directory = File::open(temp.path()).unwrap();
        assert_eq!(
            snapshot_package(&directory).unwrap().digest,
            "sha256:decdd28f2e885f4c79cf886e03512edcbb0a9f4148c8bfb1a693367d8dd1a94c"
        );
    }

    #[test]
    fn tree_digest_is_deterministic_and_covers_mode_path_bytes_and_empty_dirs() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("a"), "one").unwrap();
        fs::write(temp.path().join("b"), "two").unwrap();
        fs::write(temp.path().join("ocean-extension.toml"), package_manifest()).unwrap();
        let dir = File::open(temp.path()).unwrap();
        let first = snapshot_package(&dir).unwrap().digest;
        let dir = File::open(temp.path()).unwrap();
        assert_eq!(first, snapshot_package(&dir).unwrap().digest);
        fs::write(temp.path().join("b"), "three").unwrap();
        let dir = File::open(temp.path()).unwrap();
        assert_ne!(first, snapshot_package(&dir).unwrap().digest);
        fs::rename(temp.path().join("b"), temp.path().join("c")).unwrap();
        let dir = File::open(temp.path()).unwrap();
        let renamed = snapshot_package(&dir).unwrap().digest;
        assert_ne!(first, renamed);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = temp.path().join("a");
            let dir = File::open(temp.path()).unwrap();
            let before_mode = snapshot_package(&dir).unwrap().digest;
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&path, permissions).unwrap();
            let dir = File::open(temp.path()).unwrap();
            assert_ne!(before_mode, snapshot_package(&dir).unwrap().digest);
        }
        // Empty directories are both entry-budgeted and part of artifact
        // identity because a manifest may declare one as a resource.
        let dir = File::open(temp.path()).unwrap();
        let before_empty_dirs = snapshot_package(&dir).unwrap().digest;
        for index in 0..100 {
            fs::create_dir(temp.path().join(format!("empty-{index}"))).unwrap();
        }
        let dir = File::open(temp.path()).unwrap();
        assert_ne!(before_empty_dirs, snapshot_package(&dir).unwrap().digest);
    }

    async fn route_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
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

    #[tokio::test]
    async fn inspect_and_doctor_http_envelopes_are_exercised_end_to_end() {
        use super::super::tests::{fake_convene_state, TestEnvRestore, AUTO_CONVENE_ENV_LOCK};
        use axum::routing::get;

        let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
        let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
        let fixture = fixture(true, true);
        let app = axum::Router::new()
            .route("/v1/extensions/{id}/inspect", get(inspect))
            .route("/v1/extensions/{id}/doctor", get(doctor))
            .with_state(fake_convene_state(&fixture.config));

        let (status, body) =
            route_json(app.clone(), "/v1/extensions/example.phase-one/inspect").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["extension"]["effective"], true);
        assert_eq!(body["extension"]["health"]["probe_run"], false);

        let (status, body) =
            route_json(app.clone(), "/v1/extensions/example.phase-one/doctor").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["checks"]["package_code_executed"], false);

        let (status, body) = route_json(app.clone(), "/v1/extensions/not-valid/inspect").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_extension_id");

        let (status, body) = route_json(
            app.clone(),
            "/v1/extensions/example.phase-one/inspect?project_id=not-a-uuid",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "invalid_project_id");

        let (status, body) = route_json(
            app,
            &format!(
                "/v1/extensions/example.phase-one/inspect?project_id={}",
                Uuid::new_v4()
            ),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "project_not_found");
    }

    #[test]
    fn doctor_state_failure_is_structured_and_non_authorizing() {
        let (status, Json(body)) =
            load_error_response(LoadError::State(StateError::RevisionMismatch), true);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], false);
        assert_eq!(body["extension"], Value::Null);
        assert_eq!(
            body["diagnostics"][0]["code"],
            "extension_state_revision_mismatch"
        );
        assert_eq!(body["checks"]["package_code_executed"], false);
    }
}
