//! Stage A2a minimum native extension-service supervisor.
//!
//! This boundary consumes only coherent activation records from
//! `extension_registry`, launches one exact trusted executable with assigned
//! roots and a cleared environment, performs the strict v1 hello/ready exchange,
//! and owns generation-safe Unix process-group cleanup. It deliberately has no
//! lifecycle delivery/replay, ping health, restart/backoff, mutation, or route.

use std::{
    collections::{BTreeMap, HashSet},
    ffi::{CStr, CString, OsStr, OsString},
    fs::{self, File},
    io,
    os::fd::AsRawFd,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock},
    time::Duration,
};

use chrono::{SecondsFormat, Utc};
use ocean_agent_sdk::extension_lifecycle::{
    decode_frame, encode_frame, Ack, HostHello, HostHelloFrame, LifecycleEventKind, Pong,
    ProtocolName, ProtocolV1, Ready, ReadyFrame, ReplayMode, Sequence, ServiceHello,
    ServiceIdentity, ServiceLimits, ServiceStatus, ServiceStatusCode, ServiceStatusState, Shutdown,
    ShutdownComplete, ShutdownFrame, ShutdownReason,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::extension_registry::{
    env_secret_source, read_service_activations, reserved_child_environment_name,
    valid_child_environment_name, SecretBinding, ServiceActivation,
};

const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const GRACEFUL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const GROUP_TERM_TIMEOUT: Duration = Duration::from_secs(2);
const GROUP_KILL_TIMEOUT: Duration = Duration::from_secs(2);
const REGISTRY_LOAD_TIMEOUT: Duration = Duration::from_secs(5);
const SUPERVISOR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const LEADER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_FRAME_BYTES: usize = ocean_agent_sdk::extension_lifecycle::MAX_FRAME_BYTES;

#[allow(dead_code)] // A2b owns transitions for the already-ratified full state vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeState {
    Inactive,
    Starting,
    Healthy,
    Degraded,
    Backoff,
    CircuitOpen,
    Stopping,
    Unhealthy,
    UnsupportedPlatform,
}

#[allow(dead_code)] // Unsupported-platform projection is exercised on non-Unix builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeReason {
    EnvironmentMissing,
    SecretMissing,
    InvalidEnvironment,
    RootUnavailable,
    SpawnFailed,
    StartupTimeout,
    ProtocolViolation,
    UnexpectedExit,
    Shutdown,
    CleanupFailed,
    ExternalUnavailable,
    ConfigurationMissing,
    RateLimited,
    ChildUnknown,
    UnsupportedPlatform,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ServiceKey {
    package_id: String,
    service_id: String,
}

/// Read-only in-memory projection. It contains names and fixed codes only;
/// argv values, environment values, secret values, and child output have no
/// representable field.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RuntimeStatus {
    package_id: String,
    package_version: String,
    package_digest: String,
    service_id: String,
    activation_revision: u64,
    activation_epoch: Uuid,
    replay_floor: Sequence,
    state: RuntimeState,
    pid: Option<u32>,
    started_at: Option<String>,
    observed_at: String,
    restart_count: u64,
    negotiated_subscriptions: Vec<LifecycleEventKind>,
    last_acknowledged_sequence: Option<Sequence>,
    lag_count: u64,
    reason: Option<RuntimeReason>,
}

#[derive(Clone, Default)]
pub(crate) struct RuntimeStatusCache {
    inner: Arc<RwLock<BTreeMap<ServiceKey, RuntimeStatus>>>,
}

impl RuntimeStatusCache {
    pub(crate) fn snapshot(&self) -> Vec<RuntimeStatus> {
        self.read().values().cloned().collect()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<ServiceKey, RuntimeStatus>> {
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<ServiceKey, RuntimeStatus>> {
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn insert_starting(&self, activation: &ServiceActivation, epoch: Uuid) {
        let status = RuntimeStatus {
            package_id: activation.package_id.clone(),
            package_version: activation.package_version.clone(),
            package_digest: activation.package_digest.clone(),
            service_id: activation.service_id.clone(),
            activation_revision: activation.activation_revision,
            activation_epoch: epoch,
            replay_floor: Sequence(0),
            state: RuntimeState::Starting,
            pid: None,
            started_at: None,
            observed_at: now_string(),
            restart_count: 0,
            negotiated_subscriptions: Vec::new(),
            last_acknowledged_sequence: None,
            lag_count: 0,
            reason: None,
        };
        self.write().insert(
            ServiceKey {
                package_id: activation.package_id.clone(),
                service_id: activation.service_id.clone(),
            },
            status,
        );
    }

    fn update(
        &self,
        activation: &ServiceActivation,
        state: RuntimeState,
        pid: Option<u32>,
        subscriptions: Option<&[LifecycleEventKind]>,
        reason: Option<RuntimeReason>,
    ) {
        let key = ServiceKey {
            package_id: activation.package_id.clone(),
            service_id: activation.service_id.clone(),
        };
        if let Some(status) = self.write().get_mut(&key) {
            status.state = state;
            status.pid = pid;
            if status.started_at.is_none() && pid.is_some() {
                status.started_at = Some(now_string());
            }
            if let Some(subscriptions) = subscriptions {
                status.negotiated_subscriptions = subscriptions.to_vec();
            }
            status.observed_at = now_string();
            status.reason = reason;
        }
    }
}

fn now_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// One boot-local supervisor. Startup is fail-soft and asynchronous; shutdown
/// is explicitly joined under a hard wall-clock bound.
pub(crate) struct ExtensionSupervisor {
    boot_id: Uuid,
    cancel: CancellationToken,
    status: RuntimeStatusCache,
    root_task: Mutex<Option<JoinHandle<()>>>,
    // Exceptional signal or membership-proof failures keep the unreaped child
    // identity and PGID owner alive for one bounded shutdown retry. A2a never
    // turns a cleanup uncertainty into permission to signal a reused group.
    retained_cleanup: Mutex<Vec<CleanupAuthority>>,
}

impl ExtensionSupervisor {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            boot_id: Uuid::new_v4(),
            cancel: CancellationToken::new(),
            status: RuntimeStatusCache::default(),
            root_task: Mutex::new(None),
            retained_cleanup: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn status_cache(&self) -> RuntimeStatusCache {
        self.status.clone()
    }

    pub(crate) async fn start(
        self: &Arc<Self>,
        config_dir: PathBuf,
        registered_projects: HashSet<Uuid>,
    ) {
        let supervisor = Arc::clone(self);
        let task = tokio::spawn(async move {
            supervisor
                .run_reconciliation(config_dir, registered_projects)
                .await;
        });
        *self.root_task.lock().await = Some(task);
    }

    async fn run_reconciliation(
        self: Arc<Self>,
        config_dir: PathBuf,
        registered_projects: HashSet<Uuid>,
    ) {
        let load = tokio::task::spawn_blocking(move || {
            read_service_activations(&config_dir, &registered_projects)
        });
        let activations = match tokio::time::timeout(REGISTRY_LOAD_TIMEOUT, load).await {
            Ok(Ok(Ok(activations))) => activations,
            Ok(Ok(Err(error))) => {
                tracing::warn!(
                    reason = error.code(),
                    "extension startup reconciliation blocked"
                );
                return;
            }
            Ok(Err(_)) => {
                tracing::warn!(
                    reason = "registry_reader_failed",
                    "extension startup reconciliation blocked"
                );
                return;
            }
            Err(_) => {
                tracing::warn!(
                    reason = "registry_reader_timeout",
                    "extension startup reconciliation blocked"
                );
                return;
            }
        };
        if self.cancel.is_cancelled() {
            return;
        }

        let mut services = JoinSet::new();
        for activation in activations {
            if self.cancel.is_cancelled() {
                break;
            }
            let cancel = self.cancel.clone();
            let status = self.status.clone();
            let boot_id = self.boot_id;
            services.spawn(async move { run_service(activation, boot_id, cancel, status).await });
        }
        while let Some(result) = services.join_next().await {
            match result {
                Ok(Some(authority)) => self.retained_cleanup.lock().await.push(authority),
                Ok(None) => {}
                Err(_) => tracing::warn!(
                    reason = "service_task_failed",
                    "extension service task failed"
                ),
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.cancel.cancel();
        let deadline = tokio::time::Instant::now() + SUPERVISOR_SHUTDOWN_TIMEOUT;
        if let Some(mut task) = self.root_task.lock().await.take() {
            if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
                task.abort();
                tracing::warn!(
                    reason = "supervisor_shutdown_timeout",
                    "extension supervisor shutdown timed out"
                );
                return;
            }
        }

        let retained = std::mem::take(&mut *self.retained_cleanup.lock().await);
        let retry = async move {
            let mut retries = JoinSet::new();
            for mut authority in retained {
                retries.spawn(async move {
                    terminate_process_group(&mut authority.child, &mut authority.owner).await
                });
            }
            while retries.join_next().await.is_some() {}
        };
        if tokio::time::timeout_at(deadline, retry).await.is_err() {
            tracing::warn!(
                reason = "retained_cleanup_timeout",
                "retained extension cleanup timed out"
            );
        }
    }
}

struct SensitiveValue(Vec<u8>);

impl std::fmt::Debug for SensitiveValue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl SensitiveValue {
    fn as_os_str(&self) -> &OsStr {
        OsStr::from_bytes(&self.0)
    }
}

impl Drop for SensitiveValue {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EnvironmentError {
    MissingOrdinary,
    MissingSecret,
    InvalidName,
}

fn resolve_environment(
    activation: &ServiceActivation,
    mut source: impl FnMut(&OsStr) -> Option<OsString>,
) -> Result<Vec<(String, SensitiveValue)>, EnvironmentError> {
    let mut resolved =
        Vec::with_capacity(activation.environment.len() + activation.secret_bindings.len());
    for name in &activation.environment {
        if !valid_child_environment_name(name) || reserved_child_environment_name(name) {
            return Err(EnvironmentError::InvalidName);
        }
        let value = source(OsStr::new(name)).ok_or(EnvironmentError::MissingOrdinary)?;
        resolved.push((name.clone(), SensitiveValue(value.into_vec())));
    }
    for SecretBinding {
        target_env,
        reference,
    } in &activation.secret_bindings
    {
        if !valid_child_environment_name(target_env) || reserved_child_environment_name(target_env)
        {
            return Err(EnvironmentError::InvalidName);
        }
        let source_name = env_secret_source(reference).ok_or(EnvironmentError::InvalidName)?;
        let value = source(OsStr::new(source_name)).ok_or(EnvironmentError::MissingSecret)?;
        resolved.push((target_env.clone(), SensitiveValue(value.into_vec())));
    }
    Ok(resolved)
}

struct AssignedRoots {
    data: PathBuf,
    cache: PathBuf,
    temp: PathBuf,
    _data_handle: File,
    _cache_handle: File,
    temp_handle: File,
    service_temp_handle: File,
    connection_name: CString,
}

#[cfg(target_os = "linux")]
fn same_device_identity(opened: u64, named: libc::dev_t) -> bool {
    opened == named
}

#[cfg(target_os = "macos")]
fn same_device_identity(opened: u64, named: libc::dev_t) -> bool {
    opened == named as u64
}

fn assigned_roots(
    config_dir: &Path,
    package_id: &str,
    service_id: &str,
    connection_id: Uuid,
) -> io::Result<AssignedRoots> {
    let canonical_config = fs::canonicalize(config_dir)?;
    let config = File::open(&canonical_config)?;
    let extensions = open_existing_dir_at(&config, "extensions")?;
    let state = open_or_create_private_dir_at(&extensions, "state")?;
    let package = open_or_create_private_dir_at(&state, package_id)?;
    let data_handle = open_or_create_private_dir_at(&package, "data")?;
    let cache_handle = open_or_create_private_dir_at(&package, "cache")?;
    let tmp = open_or_create_private_dir_at(&package, "tmp")?;
    let service_temp_handle = open_or_create_private_dir_at(&tmp, service_id)?;
    let connection_name = CString::new(connection_id.to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid connection id"))?;
    let temp_handle = open_or_create_private_dir_at_cstr(&service_temp_handle, &connection_name)?;

    let state_path = canonical_config.join("extensions/state");
    let data_path = state_path.join(package_id).join("data");
    let cache_path = state_path.join(package_id).join("cache");
    let temp_path = state_path
        .join(package_id)
        .join("tmp")
        .join(service_id)
        .join(connection_name.to_string_lossy().as_ref());
    for (path, handle) in [
        (&data_path, &data_handle),
        (&cache_path, &cache_handle),
        (&temp_path, &temp_handle),
    ] {
        let canonical = fs::canonicalize(path)?;
        if !canonical.starts_with(&state_path)
            || canonical != *path
            || !same_open_generation(handle, path)?
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe assigned root",
            ));
        }
    }

    // Linux names inherited descriptors; macOS names the retained device/file
    // identities through volfs. Neither child path reopens the mutable registry
    // pathname after this validation, so replacement cannot redirect a root.
    Ok(AssignedRoots {
        data: assigned_directory_path(&data_handle)?,
        cache: assigned_directory_path(&cache_handle)?,
        temp: assigned_directory_path(&temp_handle)?,
        _data_handle: data_handle,
        _cache_handle: cache_handle,
        temp_handle,
        service_temp_handle,
        connection_name,
    })
}

fn open_or_create_private_dir_at_cstr(parent: &File, name: &CStr) -> io::Result<File> {
    // SAFETY: parent is a live directory descriptor and name is NUL-terminated.
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    let directory = open_dir_at_cstr(parent, name)?;
    validate_private_dir(directory)
}

fn assigned_directory_path(directory: &File) -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        Ok(PathBuf::from(format!(
            "/proc/self/fd/{}",
            directory.as_raw_fd()
        )))
    }
    #[cfg(target_os = "macos")]
    {
        macos_file_id_path(directory)
    }
}

fn same_open_generation(handle: &File, path: &Path) -> io::Result<bool> {
    let opened = handle.metadata()?;
    let named = fs::metadata(path)?;
    Ok(opened.dev() == named.dev() && opened.ino() == named.ino())
}

#[cfg(unix)]
fn open_existing_dir_at(parent: &File, name: &str) -> io::Result<File> {
    open_dir_at(parent, name).and_then(validate_private_or_registry_dir)
}

fn open_or_create_private_dir_at(parent: &File, name: &str) -> io::Result<File> {
    let name = CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid directory name"))?;
    open_or_create_private_dir_at_cstr(parent, &name)
}

#[cfg(unix)]
fn open_dir_at(parent: &File, name: &str) -> io::Result<File> {
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid directory name"))?;
    open_dir_at_cstr(parent, &name)
}

#[cfg(unix)]
fn open_dir_at_cstr(parent: &File, name: &std::ffi::CStr) -> io::Result<File> {
    // SAFETY: parent is live, name is NUL-terminated, and successful ownership
    // transfers exactly once into File.
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
    Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn validate_private_or_registry_dir(directory: File) -> io::Result<File> {
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a directory",
        ));
    }
    Ok(directory)
}

#[cfg(unix)]
fn validate_private_dir(directory: File) -> io::Result<File> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = directory.metadata()?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe assigned root",
        ));
    }
    Ok(directory)
}

struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns the stream returned by fdopendir.
        unsafe { libc::closedir(self.0) };
    }
}

fn remove_directory_contents(directory: &File) -> io::Result<()> {
    let duplicate = unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: duplicate is a fresh readable directory descriptor. fdopendir
    // takes ownership on success.
    let raw = unsafe { libc::fdopendir(duplicate) };
    if raw.is_null() {
        // SAFETY: fdopendir did not take ownership on failure.
        unsafe { libc::close(duplicate) };
        return Err(io::Error::last_os_error());
    }
    let stream = DirectoryStream(raw);
    loop {
        // SAFETY: stream owns a valid DIR pointer for this loop.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            break;
        }
        // SAFETY: d_name is NUL-terminated for this readdir row and is copied
        // before the next call can reuse the storage.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_owned();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: directory and name are live; metadata is writable.
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                continue;
            }
            return Err(error);
        }
        // SAFETY: fstatat initialized metadata on success.
        let metadata = unsafe { metadata.assume_init() };
        if metadata.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = open_dir_at_cstr(directory, &name)?;
            remove_directory_contents(&child)?;
            let opened = child.metadata()?;
            let mut named = std::mem::MaybeUninit::<libc::stat>::zeroed();
            // SAFETY: same validated descriptor-relative lookup as above.
            if unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    named.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fstatat initialized named on success.
            let named = unsafe { named.assume_init() };
            if !same_device_identity(opened.dev(), named.st_dev) || opened.ino() != named.st_ino {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "temp generation changed during cleanup",
                ));
            }
            // SAFETY: the verified name is relative to the retained parent.
            if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) }
                != 0
            {
                return Err(io::Error::last_os_error());
            }
        } else {
            // SAFETY: unlinkat removes only this non-directory row beneath the
            // retained temp descriptor and never follows a symlink.
            if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ENOENT) {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

fn cleanup_temp_root(roots: &AssignedRoots) -> bool {
    if remove_directory_contents(&roots.temp_handle).is_err() {
        return false;
    }
    let opened = match roots.temp_handle.metadata() {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    let mut named = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: the parent descriptor and connection name remain live.
    if unsafe {
        libc::fstatat(
            roots.service_temp_handle.as_raw_fd(),
            roots.connection_name.as_ptr(),
            named.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return false;
    }
    // SAFETY: fstatat initialized named on success.
    let named = unsafe { named.assume_init() };
    if !same_device_identity(opened.dev(), named.st_dev) || opened.ino() != named.st_ino {
        return false;
    }
    // SAFETY: the verified empty connection root is removed relative to its
    // retained parent descriptor.
    unsafe {
        libc::unlinkat(
            roots.service_temp_handle.as_raw_fd(),
            roots.connection_name.as_ptr(),
            libc::AT_REMOVEDIR,
        ) == 0
    }
}

async fn run_service(
    activation: ServiceActivation,
    boot_id: Uuid,
    cancel: CancellationToken,
    status: RuntimeStatusCache,
) -> Option<CleanupAuthority> {
    let epoch = Uuid::new_v4();
    status.insert_starting(&activation, epoch);
    if !matches!(activation.startup_timeout.as_millis(), 100..=30_000) {
        status.update(
            &activation,
            RuntimeState::Unhealthy,
            None,
            None,
            Some(RuntimeReason::ProtocolViolation),
        );
        return None;
    }
    if cancel.is_cancelled() {
        status.update(
            &activation,
            RuntimeState::Inactive,
            None,
            None,
            Some(RuntimeReason::Shutdown),
        );
        return None;
    }

    let connection_id = Uuid::new_v4();
    let roots = match assigned_roots(
        &activation.config_dir,
        &activation.package_id,
        &activation.service_id,
        connection_id,
    ) {
        Ok(roots) => roots,
        Err(_) => {
            status.update(
                &activation,
                RuntimeState::Unhealthy,
                None,
                None,
                Some(RuntimeReason::RootUnavailable),
            );
            return None;
        }
    };
    let environment = match resolve_environment(&activation, |name| std::env::var_os(name)) {
        Ok(environment) => environment,
        Err(error) => {
            let reason = match error {
                EnvironmentError::MissingOrdinary => RuntimeReason::EnvironmentMissing,
                EnvironmentError::MissingSecret => RuntimeReason::SecretMissing,
                EnvironmentError::InvalidName => RuntimeReason::InvalidEnvironment,
            };
            status.update(
                &activation,
                RuntimeState::Unhealthy,
                None,
                None,
                Some(reason),
            );
            let _ = cleanup_temp_root(&roots);
            return None;
        }
    };

    let mut child = match spawn_service(&activation, &roots, &environment) {
        Ok(child) => child,
        Err(_) => {
            status.update(
                &activation,
                RuntimeState::Unhealthy,
                None,
                None,
                Some(RuntimeReason::SpawnFailed),
            );
            let _ = cleanup_temp_root(&roots);
            return None;
        }
    };
    drop(environment);
    let Some(pid) = child.id() else {
        status.update(
            &activation,
            RuntimeState::Unhealthy,
            None,
            None,
            Some(RuntimeReason::SpawnFailed),
        );
        let _ = cleanup_temp_root(&roots);
        return None;
    };
    let mut owner = ProcessGroupOwner::new(pid);
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let cleaned = terminate_process_group(&mut child, &mut owner).await;
        if cleaned {
            let _ = cleanup_temp_root(&roots);
        }
        status.update(
            &activation,
            RuntimeState::Unhealthy,
            None,
            None,
            Some(if cleaned {
                RuntimeReason::SpawnFailed
            } else {
                RuntimeReason::CleanupFailed
            }),
        );
        return (!cleaned).then_some(CleanupAuthority { child, owner });
    };
    let mut stdin = stdin;
    let mut stdout = BufReader::new(stdout);

    let handshake = {
        let handshake = tokio::time::timeout(
            activation.startup_timeout,
            perform_handshake(
                &activation,
                boot_id,
                connection_id,
                epoch,
                &mut stdin,
                &mut stdout,
            ),
        );
        tokio::pin!(handshake);
        tokio::select! {
            result = &mut handshake => Some(result),
            _ = cancel.cancelled() => None,
        }
    };
    let Some(handshake) = handshake else {
        let cleaned = finish_process(
            &activation,
            &status,
            &mut child,
            &mut owner,
            stdin,
            stdout,
            &roots,
            ShutdownReason::DaemonStopping,
            RuntimeReason::Shutdown,
        )
        .await;
        return (!cleaned).then_some(CleanupAuthority { child, owner });
    };
    let subscriptions = match handshake {
        Ok(Ok(subscriptions)) => subscriptions,
        Ok(Err(_)) => {
            status.update(
                &activation,
                RuntimeState::Unhealthy,
                Some(pid),
                None,
                Some(RuntimeReason::ProtocolViolation),
            );
            let cleaned = finish_process(
                &activation,
                &status,
                &mut child,
                &mut owner,
                stdin,
                stdout,
                &roots,
                ShutdownReason::Unhealthy,
                RuntimeReason::ProtocolViolation,
            )
            .await;
            return (!cleaned).then_some(CleanupAuthority { child, owner });
        }
        Err(_) => {
            status.update(
                &activation,
                RuntimeState::Unhealthy,
                Some(pid),
                None,
                Some(RuntimeReason::StartupTimeout),
            );
            let cleaned = finish_process(
                &activation,
                &status,
                &mut child,
                &mut owner,
                stdin,
                stdout,
                &roots,
                ShutdownReason::Unhealthy,
                RuntimeReason::StartupTimeout,
            )
            .await;
            return (!cleaned).then_some(CleanupAuthority { child, owner });
        }
    };
    status.update(
        &activation,
        RuntimeState::Healthy,
        Some(pid),
        Some(&subscriptions),
        None,
    );

    let mut leader_poll = tokio::time::interval(LEADER_POLL_INTERVAL);
    leader_poll.tick().await;
    let (shutdown_reason, runtime_reason) = loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                break (ShutdownReason::DaemonStopping, RuntimeReason::Shutdown);
            }
            _ = leader_poll.tick() => {
                if owner.leader_exited() {
                    break (ShutdownReason::Unhealthy, RuntimeReason::UnexpectedExit);
                }
            }
            frame = read_frame::<ChildFrame, _>(&mut stdout) => {
                match frame {
                    Ok(ChildFrame::Status(child_status)) => {
                        let state = match child_status.state {
                            ServiceStatusState::Ready => RuntimeState::Healthy,
                            ServiceStatusState::Degraded => RuntimeState::Degraded,
                        };
                        let reason = (state == RuntimeState::Degraded).then_some(
                            match child_status.code {
                                ServiceStatusCode::ExternalUnavailable => RuntimeReason::ExternalUnavailable,
                                ServiceStatusCode::ConfigurationMissing => RuntimeReason::ConfigurationMissing,
                                ServiceStatusCode::RateLimited => RuntimeReason::RateLimited,
                                ServiceStatusCode::Unknown => RuntimeReason::ChildUnknown,
                            },
                        );
                        status.update(&activation, state, Some(pid), None, reason);
                    }
                    Ok(ChildFrame::Ack(ack)) => {
                        let _invalid_sequence = ack.sequence;
                        break (ShutdownReason::Unhealthy, RuntimeReason::ProtocolViolation);
                    }
                    Ok(ChildFrame::Pong(pong)) => {
                        let _unexpected_nonce = pong.nonce;
                        break (ShutdownReason::Unhealthy, RuntimeReason::ProtocolViolation);
                    }
                    Ok(ChildFrame::ShutdownComplete(_)) => {
                        break (ShutdownReason::Unhealthy, RuntimeReason::ProtocolViolation);
                    }
                    Err(error) => {
                        break (
                            ShutdownReason::Unhealthy,
                            post_ready_frame_failure_reason(error),
                        );
                    }
                }
            }
        }
    };
    let cleaned = finish_process(
        &activation,
        &status,
        &mut child,
        &mut owner,
        stdin,
        stdout,
        &roots,
        shutdown_reason,
        runtime_reason,
    )
    .await;
    (!cleaned).then_some(CleanupAuthority { child, owner })
}

fn spawn_service(
    activation: &ServiceActivation,
    roots: &AssignedRoots,
    environment: &[(String, SensitiveValue)],
) -> io::Result<Child> {
    #[cfg(target_os = "linux")]
    let executable_fd = activation.executable.as_raw_fd();
    let package_fd = activation.package_directory.as_raw_fd();
    #[cfg(target_os = "linux")]
    let data_fd = roots._data_handle.as_raw_fd();
    #[cfg(target_os = "linux")]
    let cache_fd = roots._cache_handle.as_raw_fd();
    #[cfg(target_os = "linux")]
    let temp_fd = roots.temp_handle.as_raw_fd();
    #[cfg(target_os = "linux")]
    let executable = PathBuf::from(format!("/proc/self/fd/{executable_fd}"));
    #[cfg(target_os = "macos")]
    let executable = macos_file_id_path(&activation.executable)?;
    let mut command = Command::new(executable);
    command
        .args(&activation.args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PWD", &activation.package_path)
        .env("HOME", &roots.data)
        .env("XDG_STATE_HOME", &roots.data)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("TMPDIR", &roots.temp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Detailed bounded diagnostic capture belongs to A2b. A2a discards
        // stderr structurally, so secret bytes cannot reach logs or status.
        .stderr(Stdio::null());
    for (name, value) in environment {
        command.env(name, value.as_os_str());
    }
    // SAFETY: every descriptor stays owned in the parent through spawn. Linux
    // clears CLOEXEC only for descriptors named in the environment or executable
    // path; macOS volfs paths need no inherited descriptor. Both platforms then
    // change cwd and process group. The package cwd descriptor stays CLOEXEC.
    unsafe {
        command.pre_exec(move || {
            #[cfg(target_os = "linux")]
            for descriptor in [data_fd, cache_fd, temp_fd, executable_fd] {
                inherit_descriptor(descriptor)?;
            }
            if libc::fchdir(package_fd) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

#[cfg(target_os = "macos")]
fn macos_file_id_path(file: &File) -> io::Result<PathBuf> {
    let metadata = file.metadata()?;
    // macOS volfs resolves this stable (device,file-id) tuple directly. The
    // retained descriptor prevents file-id reuse; replacement of the verified
    // store pathname cannot redirect this lookup, and unlink makes it fail
    // closed rather than selecting another generation.
    let path = PathBuf::from(format!("/.vol/{}/{}", metadata.dev(), metadata.ino()));
    let resolved = File::open(&path)?;
    let resolved_metadata = resolved.metadata()?;
    if metadata.dev() != resolved_metadata.dev() || metadata.ino() != resolved_metadata.ino() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "file-id execution primitive changed generation",
        ));
    }
    Ok(path)
}

#[cfg(target_os = "linux")]
unsafe fn inherit_descriptor(descriptor: libc::c_int) -> io::Result<()> {
    // SAFETY: called after fork in pre_exec with a live inherited descriptor.
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: only the descriptor-local CLOEXEC bit is cleared.
    if unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ChildFrame {
    Ack(Ack),
    Pong(Pong),
    Status(ServiceStatus),
    ShutdownComplete(ShutdownComplete),
}

async fn perform_handshake<W, R>(
    activation: &ServiceActivation,
    boot_id: Uuid,
    connection_id: Uuid,
    epoch: Uuid,
    stdin: &mut W,
    stdout: &mut R,
) -> Result<Vec<LifecycleEventKind>, ()>
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
{
    let hello = HostHello {
        protocol: ProtocolName,
        version: ProtocolV1,
        frame: HostHelloFrame,
        connection_id,
        daemon_boot_id: boot_id,
        identity: ServiceIdentity {
            package_id: activation.package_id.clone(),
            package_version: activation.package_version.clone(),
            package_digest: activation.package_digest.clone(),
            service_id: activation.service_id.clone(),
            activation_revision: activation.activation_revision,
            activation_epoch: epoch,
            replay_floor: Sequence(0),
        },
        limits: ServiceLimits {
            max_frame_bytes: 65_536,
            outbound_messages: 256,
            outbound_bytes: 1_048_576,
            heartbeat_interval_ms: 10_000,
            heartbeat_timeout_ms: 5_000,
        },
    };
    write_frame(stdin, &hello).await?;
    let service_hello: ServiceHello = read_frame(stdout).await.map_err(|_| ())?;
    if service_hello.resume.is_some()
        || service_hello
            .validate_subscriptions(&activation.events)
            .is_err()
    {
        return Err(());
    }
    let subscriptions = service_hello.subscriptions;
    let ready = Ready {
        protocol: ProtocolName,
        version: ProtocolV1,
        frame: ReadyFrame,
        subscriptions: subscriptions.clone(),
        replay: ReplayMode::BootLocal,
        activation_epoch: epoch,
        replay_floor: Sequence(0),
    };
    write_frame(stdin, &ready).await?;
    Ok(subscriptions)
}

async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &impl Serialize,
) -> Result<(), ()> {
    let encoded = encode_frame(frame).map_err(|_| ())?;
    tokio::time::timeout(WRITE_TIMEOUT, writer.write_all(&encoded))
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameReadError {
    Eof,
    Io,
    ProtocolViolation,
}

fn post_ready_frame_failure_reason(error: FrameReadError) -> RuntimeReason {
    match error {
        FrameReadError::Eof | FrameReadError::Io => RuntimeReason::UnexpectedExit,
        FrameReadError::ProtocolViolation => RuntimeReason::ProtocolViolation,
    }
}

async fn read_frame<T: for<'de> Deserialize<'de>, R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<T, FrameReadError> {
    let mut encoded = Vec::with_capacity(1024);
    loop {
        let available = reader.fill_buf().await.map_err(|_| FrameReadError::Io)?;
        if available.is_empty() {
            return Err(if encoded.is_empty() {
                FrameReadError::Eof
            } else {
                FrameReadError::ProtocolViolation
            });
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let next = encoded
            .len()
            .checked_add(count)
            .ok_or(FrameReadError::ProtocolViolation)?;
        if next > MAX_FRAME_BYTES {
            return Err(FrameReadError::ProtocolViolation);
        }
        encoded.extend_from_slice(&available[..count]);
        reader.consume(count);
        if encoded.last() == Some(&b'\n') {
            return decode_frame(&encoded).map_err(|_| FrameReadError::ProtocolViolation);
        }
    }
}

#[allow(clippy::too_many_arguments)] // Explicitly owns child, PGID token, pipes, roots, and both fixed reasons.
async fn finish_process(
    activation: &ServiceActivation,
    status: &RuntimeStatusCache,
    child: &mut Child,
    owner: &mut ProcessGroupOwner,
    mut stdin: ChildStdin,
    mut stdout: BufReader<ChildStdout>,
    roots: &AssignedRoots,
    reason: ShutdownReason,
    runtime_reason: RuntimeReason,
) -> bool {
    let pid = child.id();
    status.update(
        activation,
        RuntimeState::Stopping,
        pid,
        None,
        Some(runtime_reason),
    );
    let shutdown = Shutdown {
        protocol: ProtocolName,
        version: ProtocolV1,
        frame: ShutdownFrame,
        reason,
    };
    let _ = write_frame(&mut stdin, &shutdown).await;
    let response = async {
        loop {
            match read_frame::<ChildFrame, _>(&mut stdout)
                .await
                .map_err(|_| ())?
            {
                ChildFrame::ShutdownComplete(_) => return Ok::<(), ()>(()),
                ChildFrame::Status(_) => {}
                ChildFrame::Ack(ack) => {
                    let _invalid_sequence = ack.sequence;
                    return Err(());
                }
                ChildFrame::Pong(pong) => {
                    let _unexpected_nonce = pong.nonce;
                    return Err(());
                }
            }
        }
    };
    let _ = tokio::time::timeout(GRACEFUL_RESPONSE_TIMEOUT, response).await;
    drop(stdin);

    let group_cleaned = terminate_process_group(child, owner).await;
    let roots_cleaned = group_cleaned && cleanup_temp_root(roots);
    let fully_cleaned = group_cleaned && roots_cleaned;
    let terminal_reason = if !fully_cleaned && runtime_reason == RuntimeReason::Shutdown {
        RuntimeReason::CleanupFailed
    } else {
        // Cleanup must not destroy the operator-visible cause of a handshake,
        // startup, or protocol failure.
        runtime_reason
    };
    status.update(
        activation,
        if reason == ShutdownReason::DaemonStopping && fully_cleaned {
            RuntimeState::Inactive
        } else {
            RuntimeState::Unhealthy
        },
        if group_cleaned { None } else { pid },
        None,
        Some(terminal_reason),
    );
    group_cleaned
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalError {
    LostOwnership,
    Os,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetainedLeaderState {
    Running,
    Exited,
}

/// Narrow syscall/identity seam for deterministic generation-safety review.
/// Production uses retained-child `waitid(WNOWAIT)` before every group signal;
/// tests can model numeric reuse without asking the kernel PID allocator to wrap.
trait ProcessGroupSyscalls {
    fn retained_leader_state(
        &self,
        leader: libc::pid_t,
    ) -> Result<RetainedLeaderState, SignalError>;
    fn signal_group(&self, pgid: libc::pid_t, signal: libc::c_int) -> io::Result<()>;
    fn group_has_live_members(&self, pgid: libc::pid_t) -> io::Result<bool>;
}

struct OsProcessGroupSyscalls;

impl ProcessGroupSyscalls for OsProcessGroupSyscalls {
    fn retained_leader_state(
        &self,
        leader: libc::pid_t,
    ) -> Result<RetainedLeaderState, SignalError> {
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: info points to writable siginfo storage. WNOWAIT preserves the
        // child identity and zombie until group cleanup is proven complete.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                leader as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return if io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                Err(SignalError::LostOwnership)
            } else {
                Err(SignalError::Os)
            };
        }
        // SAFETY: waitid initialized siginfo on success. A zero si_pid means the
        // retained child is still running; the exact leader pid means zombie.
        let exited = unsafe { info.assume_init().si_pid() == leader };
        Ok(if exited {
            RetainedLeaderState::Exited
        } else {
            RetainedLeaderState::Running
        })
    }

    fn signal_group(&self, pgid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
        // SAFETY: caller proved the exact leader remains an unreaped child, so a
        // negative PGID cannot name a later unrelated generation.
        if unsafe { libc::kill(-pgid, signal) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn group_has_live_members(&self, pgid: libc::pid_t) -> io::Result<bool> {
        group_has_live_members(pgid)
    }
}

struct ProcessGroupOwner {
    leader: libc::pid_t,
    pgid: libc::pid_t,
    reaped: bool,
}

struct CleanupAuthority {
    child: Child,
    owner: ProcessGroupOwner,
}

impl ProcessGroupOwner {
    fn new(pid: u32) -> Self {
        let leader = libc::pid_t::try_from(pid).unwrap_or(libc::pid_t::MAX);
        Self {
            leader,
            pgid: leader,
            reaped: false,
        }
    }

    fn leader_owned_with(&self, syscalls: &impl ProcessGroupSyscalls) -> bool {
        !self.reaped && syscalls.retained_leader_state(self.leader).is_ok()
    }

    fn leader_owned(&self) -> bool {
        self.leader_owned_with(&OsProcessGroupSyscalls)
    }

    fn leader_exited(&self) -> bool {
        if self.reaped {
            return true;
        }
        !matches!(
            OsProcessGroupSyscalls.retained_leader_state(self.leader),
            Ok(RetainedLeaderState::Running)
        )
    }

    fn signal_with(
        &self,
        signal: libc::c_int,
        syscalls: &impl ProcessGroupSyscalls,
    ) -> Result<(), SignalError> {
        if self.reaped {
            return Err(SignalError::LostOwnership);
        }
        syscalls.retained_leader_state(self.leader)?;
        match syscalls.signal_group(self.pgid, signal) {
            Ok(()) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            Err(error) if error.raw_os_error() == Some(libc::EPERM) => {
                match syscalls.group_has_live_members(self.pgid) {
                    Ok(false) => Ok(()),
                    Ok(true) | Err(_) => Err(SignalError::Os),
                }
            }
            Err(_) => Err(SignalError::Os),
        }
    }
}

async fn terminate_process_group(child: &mut Child, owner: &mut ProcessGroupOwner) -> bool {
    terminate_process_group_with(child, owner, &OsProcessGroupSyscalls).await
}

async fn terminate_process_group_with(
    child: &mut Child,
    owner: &mut ProcessGroupOwner,
    syscalls: &impl ProcessGroupSyscalls,
) -> bool {
    let term_deadline = tokio::time::Instant::now() + GROUP_TERM_TIMEOUT;
    if !signal_until(owner, libc::SIGTERM, term_deadline, syscalls).await {
        return false;
    }
    if !wait_for_group_exit_until(owner.pgid, term_deadline, syscalls).await {
        let kill_deadline = tokio::time::Instant::now() + GROUP_KILL_TIMEOUT;
        if !signal_until(owner, libc::SIGKILL, kill_deadline, syscalls).await
            || !wait_for_group_exit_until(owner.pgid, kill_deadline, syscalls).await
        {
            return false;
        }
    }
    if !owner.leader_owned() || child.wait().await.is_err() {
        return false;
    }
    owner.reaped = true;
    true
}

async fn signal_until(
    owner: &ProcessGroupOwner,
    signal: libc::c_int,
    deadline: tokio::time::Instant,
    syscalls: &impl ProcessGroupSyscalls,
) -> bool {
    loop {
        match owner.signal_with(signal, syscalls) {
            Ok(()) => return true,
            Err(SignalError::LostOwnership) => return false,
            Err(SignalError::Os) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_group_exit_until(
    pgid: libc::pid_t,
    deadline: tokio::time::Instant,
    syscalls: &impl ProcessGroupSyscalls,
) -> bool {
    loop {
        if let Ok(false) = syscalls.group_has_live_members(pgid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(target_os = "linux")]
fn group_has_live_members(pgid: libc::pid_t) -> io::Result<bool> {
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some(after_name) = stat.rsplit_once(") ").map(|(_, rest)| rest) else {
            continue;
        };
        let mut fields = after_name.split_whitespace();
        let Some(state) = fields.next() else { continue };
        let _ppid = fields.next();
        let Some(group) = fields
            .next()
            .and_then(|value| value.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        if group == pgid && state != "Z" {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(target_os = "macos")]
fn group_has_live_members(pgid: libc::pid_t) -> io::Result<bool> {
    const PROC_PGRP_ONLY: u32 = 2;
    let mut pids = vec![0_i32; 256];
    loop {
        let bytes = i32::try_from(pids.len() * std::mem::size_of::<i32>())
            .map_err(|_| io::Error::other("process list too large"))?;
        // SAFETY: pids is writable for exactly bytes bytes.
        let used = unsafe {
            libc::proc_listpids(PROC_PGRP_ONLY, pgid as u32, pids.as_mut_ptr().cast(), bytes)
        };
        if used < 0 {
            return Err(io::Error::last_os_error());
        }
        let count = usize::try_from(used).unwrap_or_default() / std::mem::size_of::<i32>();
        if count < pids.len() {
            pids.truncate(count);
            break;
        }
        pids.resize(pids.len().saturating_mul(2), 0);
        if pids.len() > 65_536 {
            return Err(io::Error::other("process group is unexpectedly large"));
        }
    }
    for pid in pids.into_iter().filter(|pid| *pid > 0) {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        // SAFETY: info is writable for the declared proc_bsdinfo size.
        let size = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).unwrap_or(i32::MAX),
            )
        };
        if usize::try_from(size).unwrap_or_default() != std::mem::size_of::<libc::proc_bsdinfo>() {
            continue;
        }
        // SAFETY: proc_pidinfo returned the full structure.
        let info = unsafe { info.assume_init() };
        if info.pbi_pgid == pgid as u32 && info.pbi_status != libc::SZOMB {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_agent_sdk::extension_lifecycle::LifecycleEventKind;
    use std::os::unix::fs::PermissionsExt;

    fn executable_fixture(script: &str) -> (tempfile::TempDir, ServiceActivation) {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("package");
        fs::create_dir(&package).unwrap();
        let entry = package.join("service");
        fs::write(&entry, script).unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o700)).unwrap();
        let package_directory = File::open(&package).unwrap();
        let executable = File::open(&entry).unwrap();
        let config_dir = temp.path().join("config");
        (
            temp,
            ServiceActivation {
                package_id: "example.noop".to_owned(),
                package_version: "1.0.0".to_owned(),
                package_digest: format!("sha256:{}", "a".repeat(64)),
                service_id: "lifecycle".to_owned(),
                activation_revision: 7,
                config_dir,
                package_path: package,
                package_directory,
                executable,
                args: Vec::new(),
                events: vec![LifecycleEventKind::DaemonStarted],
                environment: Vec::new(),
                secret_bindings: Vec::new(),
                startup_timeout: Duration::from_secs(2),
            },
        )
    }

    fn install_fixture_store(temp: &tempfile::TempDir, activation: &mut ServiceActivation) {
        let config = temp.path().join("config");
        fs::create_dir(&config).unwrap();
        activation.config_dir = config.clone();
        fs::create_dir(config.join("extensions")).unwrap();
        let store = config
            .join("extensions/store/example.noop")
            .join("a".repeat(64));
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        fs::rename(&activation.package_path, &store).unwrap();
        activation.package_path = store;
    }

    #[test]
    fn environment_resolution_is_explicit_and_secret_values_are_debug_redacted() {
        let (_temp, mut activation) = executable_fixture("#!/bin/sh\nexit 0\n");
        activation.environment = vec!["PUBLIC_NAME".to_owned()];
        activation.secret_bindings = vec![SecretBinding {
            target_env: "SECRET_TARGET".to_owned(),
            reference: "env:SECRET_SOURCE".to_owned(),
        }];
        let values = resolve_environment(&activation, |name| match name.to_str() {
            Some("PUBLIC_NAME") => Some(OsString::from("ordinary-sentinel")),
            Some("SECRET_SOURCE") => Some(OsString::from("secret-sentinel")),
            _ => None,
        })
        .unwrap();
        assert_eq!(values.len(), 2);
        assert_eq!(format!("{:?}", values[1].1), "<redacted>");
        assert_eq!(values[1].0, "SECRET_TARGET");
        assert!(!format!("{:?}", values[1].1).contains("secret-sentinel"));
    }

    #[test]
    fn missing_reserved_and_unsupported_bindings_fail_before_spawn() {
        let (_temp, mut activation) = executable_fixture("#!/bin/sh\nexit 0\n");
        activation.environment = vec!["MISSING".to_owned()];
        assert_eq!(
            resolve_environment(&activation, |_| None).unwrap_err(),
            EnvironmentError::MissingOrdinary
        );
        activation.environment.clear();
        activation.secret_bindings = vec![SecretBinding {
            target_env: "PATH".to_owned(),
            reference: "env:SOURCE".to_owned(),
        }];
        assert_eq!(
            resolve_environment(&activation, |_| Some(OsString::from("x"))).unwrap_err(),
            EnvironmentError::InvalidName
        );
        activation.secret_bindings[0] = SecretBinding {
            target_env: "TARGET".to_owned(),
            reference: "vault:item".to_owned(),
        };
        assert_eq!(
            resolve_environment(&activation, |_| Some(OsString::from("x"))).unwrap_err(),
            EnvironmentError::InvalidName
        );
    }

    #[test]
    fn runtime_status_has_no_argv_environment_secret_or_diagnostic_text_fields() {
        let (_temp, activation) = executable_fixture("#!/bin/sh\nexit 0\n");
        let cache = RuntimeStatusCache::default();
        cache.insert_starting(&activation, Uuid::nil());
        let encoded = serde_json::to_string(&cache.snapshot()).unwrap();
        for forbidden in ["args", "environment", "secret", "stderr", "diagnostic"] {
            assert!(!encoded.contains(forbidden));
        }
        assert_eq!(cache.snapshot()[0].state, RuntimeState::Starting);
    }

    #[tokio::test]
    async fn transport_rejects_oversize_duplicate_unknown_and_resume_frames() {
        async fn decode_service_hello(bytes: Vec<u8>) -> Result<ServiceHello, FrameReadError> {
            let capacity = bytes.len().saturating_add(1);
            let (mut writer, reader) = tokio::io::duplex(capacity);
            let write = tokio::spawn(async move { writer.write_all(&bytes).await.unwrap() });
            let result = read_frame(&mut BufReader::new(reader)).await;
            write.await.unwrap();
            result
        }

        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert_eq!(
            decode_service_hello(oversized).await,
            Err(FrameReadError::ProtocolViolation)
        );
        assert_eq!(
            decode_service_hello(
                b"{\"protocol\":\"ocean.extension.service\",\"version\":1,\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null}\n"
                    .to_vec()
            )
            .await,
            Err(FrameReadError::ProtocolViolation)
        );
        assert_eq!(
            decode_service_hello(
                b"{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null,\"identity\":\"override\"}\n"
                    .to_vec()
            )
            .await,
            Err(FrameReadError::ProtocolViolation)
        );

        let (_temp, activation) = executable_fixture("#!/bin/sh\nexit 0\n");
        let resume = format!(
            "{{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":{{\"daemon_boot_id\":\"{}\",\"activation_epoch\":\"{}\",\"after_sequence\":\"0\"}}}}\n",
            Uuid::new_v4(),
            Uuid::new_v4()
        );
        let (host_stream, child_stream) = tokio::io::duplex(MAX_FRAME_BYTES);
        let (host_read, host_write) = tokio::io::split(host_stream);
        let (child_read, mut child_write) = tokio::io::split(child_stream);
        let child = tokio::spawn(async move {
            let mut child_read = BufReader::new(child_read);
            let _: HostHello = read_frame(&mut child_read).await.unwrap();
            child_write.write_all(resume.as_bytes()).await.unwrap();
        });
        let mut host_read = BufReader::new(host_read);
        let mut host_write = host_write;
        assert!(perform_handshake(
            &activation,
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            &mut host_write,
            &mut host_read,
        )
        .await
        .is_err());
        child.await.unwrap();
    }

    #[tokio::test]
    async fn frame_read_failures_separate_exit_io_and_protocol_causes() {
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };
        use tokio::io::{AsyncRead, ReadBuf};

        struct FailingReader;

        impl AsyncRead for FailingReader {
            fn poll_read(
                self: Pin<&mut Self>,
                _context: &mut Context<'_>,
                _buffer: &mut ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                Poll::Ready(Err(io::Error::other("injected read failure")))
            }
        }

        let mut eof = BufReader::new(tokio::io::empty());
        assert!(matches!(
            read_frame::<ChildFrame, _>(&mut eof).await,
            Err(FrameReadError::Eof)
        ));

        let mut io_failure = BufReader::new(FailingReader);
        assert!(matches!(
            read_frame::<ChildFrame, _>(&mut io_failure).await,
            Err(FrameReadError::Io)
        ));

        for bytes in [
            b"{truncated".as_slice(),
            b"{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"unknown\"}\n"
                .as_slice(),
        ] {
            let mut invalid = BufReader::new(bytes);
            assert!(matches!(
                read_frame::<ChildFrame, _>(&mut invalid).await,
                Err(FrameReadError::ProtocolViolation)
            ));
        }

        assert_eq!(
            post_ready_frame_failure_reason(FrameReadError::Eof),
            RuntimeReason::UnexpectedExit
        );
        assert_eq!(
            post_ready_frame_failure_reason(FrameReadError::Io),
            RuntimeReason::UnexpectedExit
        );
        assert_eq!(
            post_ready_frame_failure_reason(FrameReadError::ProtocolViolation),
            RuntimeReason::ProtocolViolation
        );
    }

    #[tokio::test]
    async fn strict_hello_ready_transport_and_minimal_environment_succeed() {
        let script = r#"#!/bin/sh
IFS= read -r hello || exit 10
case "$hello" in *'"frame":"host_hello"'*'"package_id":"example.noop"'*) ;; *) exit 11;; esac
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":["daemon_started"],"resume":null}'
IFS= read -r ready || exit 12
case "$ready" in *'"frame":"ready"'*) ;; *) exit 13;; esac
env | LC_ALL=C sort > "$HOME/environment"
IFS= read -r shutdown || exit 14
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"shutdown_complete"}'
"#;
        let (temp, mut activation) = executable_fixture(script);
        let config = temp.path().join("config");
        fs::create_dir(&config).unwrap();
        activation.config_dir = config.clone();
        fs::create_dir(config.join("extensions")).unwrap();
        let package_path = activation.package_path.clone();
        // assigned_roots derives config from the canonical store-shaped path.
        let store = config
            .join("extensions/store/example.noop")
            .join("a".repeat(64));
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        fs::rename(&package_path, &store).unwrap();
        activation.package_path = store;
        let cancel = CancellationToken::new();
        let status = RuntimeStatusCache::default();
        let task = tokio::spawn(run_service(
            activation,
            Uuid::new_v4(),
            cancel.clone(),
            status.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if status
                    .snapshot()
                    .first()
                    .is_some_and(|row| row.state == RuntimeState::Healthy)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        let environment =
            fs::read_to_string(config.join("extensions/state/example.noop/data/environment"))
                .unwrap();
        let mut names: HashSet<&str> = environment
            .lines()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            .collect();
        // POSIX shells synthesize these after exec; neither is inherited from
        // the daemon. All other names are the exact host baseline.
        names.remove("SHLVL");
        names.remove("_");
        assert_eq!(
            names,
            HashSet::from([
                "HOME",
                "PATH",
                "PWD",
                "TMPDIR",
                "XDG_CACHE_HOME",
                "XDG_STATE_HOME"
            ])
        );
        assert!(!config
            .join("extensions/state/example.noop/tmp/lifecycle")
            .read_dir()
            .unwrap()
            .any(|_| true));
    }

    #[tokio::test]
    async fn verified_executable_generation_survives_concurrent_path_replacement() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (temp, activation) =
            executable_fixture("#!/bin/sh\nprintf '%s\\n' trusted >> \"$OUTPUT\"\n");
        let config = temp.path().join("config");
        fs::create_dir_all(config.join("extensions")).unwrap();
        let roots = assigned_roots(
            &config,
            &activation.package_id,
            &activation.service_id,
            Uuid::new_v4(),
        )
        .unwrap();
        let output = temp.path().join("executed");
        let environment = vec![(
            "OUTPUT".to_owned(),
            SensitiveValue(output.as_os_str().as_bytes().to_vec()),
        )];
        let entry = activation.package_path.join("service");
        let retained = activation.package_path.join("verified-generation");
        fs::hard_link(&entry, &retained).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let replacer_stop = Arc::clone(&stop);
        let replacer = std::thread::spawn(move || {
            let mut generation = 0_u64;
            while !replacer_stop.load(Ordering::Acquire) {
                let candidate = entry.with_extension(format!("candidate-{generation}"));
                fs::write(
                    &candidate,
                    "#!/bin/sh\nprintf '%s\\n' malicious >> \"$OUTPUT\"\n",
                )
                .unwrap();
                fs::set_permissions(&candidate, fs::Permissions::from_mode(0o700)).unwrap();
                fs::rename(&candidate, &entry).unwrap();
                generation = generation.wrapping_add(1);
            }
        });

        for _ in 0..32 {
            let mut child = spawn_service(&activation, &roots, &environment).unwrap();
            assert!(child.wait().await.unwrap().success());
        }
        stop.store(true, Ordering::Release);
        replacer.join().unwrap();
        let lines = fs::read_to_string(output).unwrap();
        assert_eq!(lines.lines().count(), 32);
        assert!(lines.lines().all(|line| line == "trusted"));
        assert!(retained.exists());
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn unlinked_verified_executable_fails_closed_without_selecting_replacement() {
        let (temp, activation) = executable_fixture("#!/bin/sh\nprintf trusted > \"$OUTPUT\"\n");
        let config = temp.path().join("config");
        fs::create_dir_all(config.join("extensions")).unwrap();
        let roots = assigned_roots(
            &config,
            &activation.package_id,
            &activation.service_id,
            Uuid::new_v4(),
        )
        .unwrap();
        let output = temp.path().join("executed");
        let environment = vec![(
            "OUTPUT".to_owned(),
            SensitiveValue(output.as_os_str().as_bytes().to_vec()),
        )];
        let entry = activation.package_path.join("service");
        fs::remove_file(&entry).unwrap();
        fs::write(&entry, "#!/bin/sh\nprintf replacement > \"$OUTPUT\"\n").unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o700)).unwrap();

        assert!(spawn_service(&activation, &roots, &environment).is_err());
        assert!(!output.exists());
    }

    #[tokio::test]
    async fn assigned_root_descriptors_survive_leaf_and_state_replacement_through_spawn() {
        let (temp, mut activation) = executable_fixture(
            r#"#!/bin/sh
printf data > "$HOME/data-proof"
printf cache > "$XDG_CACHE_HOME/cache-proof"
printf temp > "$TMPDIR/temp-proof"
"#,
        );
        install_fixture_store(&temp, &mut activation);
        let roots = assigned_roots(
            &activation.config_dir,
            &activation.package_id,
            &activation.service_id,
            Uuid::new_v4(),
        )
        .unwrap();
        let state = activation.config_dir.join("extensions/state");
        let retained_state = activation.config_dir.join("extensions/state-retained");
        fs::rename(&state, &retained_state).unwrap();
        let replacement = state.join("example.noop");
        fs::create_dir_all(replacement.join("data")).unwrap();
        fs::create_dir_all(replacement.join("cache")).unwrap();
        fs::create_dir_all(replacement.join("tmp/lifecycle/replacement")).unwrap();

        let mut child = spawn_service(&activation, &roots, &[]).unwrap();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(
            fs::read_to_string(retained_state.join("example.noop/data/data-proof")).unwrap(),
            "data"
        );
        assert_eq!(
            fs::read_to_string(retained_state.join("example.noop/cache/cache-proof")).unwrap(),
            "cache"
        );
        let retained_temp = retained_state.join("example.noop/tmp/lifecycle");
        let proof = fs::read_dir(&retained_temp)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("temp-proof"))
            .find(|path| path.exists())
            .unwrap();
        assert_eq!(fs::read_to_string(proof).unwrap(), "temp");
        assert!(!replacement.join("data/data-proof").exists());
        assert!(!replacement.join("cache/cache-proof").exists());
    }

    #[test]
    fn temp_cleanup_is_descriptor_relative_and_refuses_a_replacement_generation() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        fs::create_dir_all(config.join("extensions")).unwrap();
        let connection = Uuid::new_v4();
        let roots = assigned_roots(&config, "example.noop", "lifecycle", connection).unwrap();
        let named = config
            .join("extensions/state/example.noop/tmp/lifecycle")
            .join(connection.to_string());
        fs::write(roots.temp.join("owned"), "owned").unwrap();
        let retained = named.with_extension("retained");
        fs::rename(&named, &retained).unwrap();
        fs::create_dir(&named).unwrap();
        fs::write(named.join("replacement"), "replacement").unwrap();

        assert!(!cleanup_temp_root(&roots));
        assert!(!retained.join("owned").exists());
        assert_eq!(
            fs::read_to_string(named.join("replacement")).unwrap(),
            "replacement"
        );
    }

    #[tokio::test]
    async fn blocked_stdin_fails_at_the_two_second_connection_deadline() {
        let (mut writer, _reader) = tokio::io::duplex(1);
        let frame = serde_json::json!({"payload": "x".repeat(1024)});
        let started = tokio::time::Instant::now();
        assert!(write_frame(&mut writer, &frame).await.is_err());
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(1_900));
        assert!(elapsed < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn startup_timeout_cleans_process_group_and_preserves_reason() {
        let (temp, mut activation) =
            executable_fixture("#!/bin/sh\nprintf '%s' $$ > \"$HOME/leader.pid\"\nsleep 30\n");
        install_fixture_store(&temp, &mut activation);
        activation.events.clear();
        activation.startup_timeout = Duration::from_millis(150);
        let config = activation.config_dir.clone();
        let status = RuntimeStatusCache::default();
        let retained = tokio::time::timeout(
            Duration::from_secs(8),
            run_service(
                activation,
                Uuid::new_v4(),
                CancellationToken::new(),
                status.clone(),
            ),
        )
        .await
        .unwrap();
        assert!(retained.is_none());
        let leader: libc::pid_t =
            fs::read_to_string(config.join("extensions/state/example.noop/data/leader.pid"))
                .unwrap()
                .parse()
                .unwrap();
        // SAFETY: signal 0 performs an existence check only.
        assert_ne!(unsafe { libc::kill(leader, 0) }, 0);
        let row = &status.snapshot()[0];
        assert_eq!(row.state, RuntimeState::Unhealthy);
        assert_eq!(row.reason, Some(RuntimeReason::StartupTimeout));
        assert!(!config
            .join("extensions/state/example.noop/tmp/lifecycle")
            .read_dir()
            .unwrap()
            .any(|_| true));
    }

    #[tokio::test]
    async fn spawned_secret_target_is_exact_and_sentinel_never_reaches_status_surfaces() {
        struct EnvGuard(String);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                // SAFETY: this unique test-only name is not read by another test.
                unsafe { std::env::remove_var(&self.0) };
            }
        }

        let source = format!(
            "A2A_SECRET_SOURCE_{}",
            Uuid::new_v4().simple().to_string().to_uppercase()
        );
        let sentinel = format!("secret-sentinel-{}", Uuid::new_v4());
        // SAFETY: the source name is unique to this test and is removed by the guard.
        unsafe { std::env::set_var(&source, &sentinel) };
        let _guard = EnvGuard(source.clone());
        let script = format!(
            r#"#!/bin/sh
IFS= read -r hello || exit 10
printf '%s\n' '{{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":[],"resume":null}}'
IFS= read -r ready || exit 11
[ "$SECRET_TARGET" = "{sentinel}" ] || exit 12
[ -z "${{{source}+present}}" ] || exit 13
printf ok > "$HOME/secret-proof"
printf '%s\n' "$SECRET_TARGET" >&2
IFS= read -r shutdown || exit 14
printf '%s\n' '{{"protocol":"ocean.extension.service","version":1,"frame":"shutdown_complete"}}'
"#
        );
        let (temp, mut activation) = executable_fixture(&script);
        install_fixture_store(&temp, &mut activation);
        activation.events.clear();
        activation.secret_bindings = vec![SecretBinding {
            target_env: "SECRET_TARGET".to_owned(),
            reference: format!("env:{source}"),
        }];
        let config = activation.config_dir.clone();
        let cancel = CancellationToken::new();
        let status = RuntimeStatusCache::default();
        let task = tokio::spawn(run_service(
            activation,
            Uuid::new_v4(),
            cancel.clone(),
            status.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if status.snapshot().first().is_some_and(|row| {
                    matches!(row.state, RuntimeState::Healthy | RuntimeState::Unhealthy)
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            status.snapshot()[0].state,
            RuntimeState::Healthy,
            "status: {:?}",
            status.snapshot()
        );
        cancel.cancel();
        assert!(task.await.unwrap().is_none());
        assert_eq!(
            fs::read_to_string(config.join("extensions/state/example.noop/data/secret-proof"))
                .unwrap(),
            "ok"
        );
        let projected = serde_json::to_string(&status.snapshot()).unwrap();
        assert!(!projected.contains(&sentinel));
        assert!(!projected.contains(&source));
        assert_eq!(status.snapshot()[0].reason, Some(RuntimeReason::Shutdown));
    }

    #[tokio::test]
    async fn ack_without_a_delivered_event_is_a_protocol_violation() {
        let script = r#"#!/bin/sh
IFS= read -r hello || exit 10
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":[],"resume":null}'
IFS= read -r ready || exit 11
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"ack","sequence":"1"}'
IFS= read -r shutdown || exit 12
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"shutdown_complete"}'
"#;
        let (temp, mut activation) = executable_fixture(script);
        activation.events.clear();
        let config = temp.path().join("config");
        fs::create_dir(&config).unwrap();
        activation.config_dir = config.clone();
        fs::create_dir(config.join("extensions")).unwrap();
        let store = config
            .join("extensions/store/example.noop")
            .join("a".repeat(64));
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        fs::rename(&activation.package_path, &store).unwrap();
        activation.package_path = store;
        let status = RuntimeStatusCache::default();
        let retained = tokio::time::timeout(
            Duration::from_secs(5),
            run_service(
                activation,
                Uuid::new_v4(),
                CancellationToken::new(),
                status.clone(),
            ),
        )
        .await
        .unwrap();
        assert!(retained.is_none());
        assert_eq!(status.snapshot()[0].state, RuntimeState::Unhealthy);
        assert_eq!(
            status.snapshot()[0].reason,
            Some(RuntimeReason::ProtocolViolation)
        );
    }

    #[tokio::test]
    async fn malformed_post_ready_frame_is_a_protocol_violation() {
        let script = r#"#!/bin/sh
IFS= read -r hello || exit 10
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":[],"resume":null}'
IFS= read -r ready || exit 11
printf '%s\n' '{malformed'
IFS= read -r shutdown || exit 12
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"shutdown_complete"}'
"#;
        let (temp, mut activation) = executable_fixture(script);
        install_fixture_store(&temp, &mut activation);
        activation.events.clear();
        let status = RuntimeStatusCache::default();
        let retained = tokio::time::timeout(
            Duration::from_secs(5),
            run_service(
                activation,
                Uuid::new_v4(),
                CancellationToken::new(),
                status.clone(),
            ),
        )
        .await
        .unwrap();
        assert!(retained.is_none());
        assert_eq!(status.snapshot()[0].state, RuntimeState::Unhealthy);
        assert_eq!(
            status.snapshot()[0].reason,
            Some(RuntimeReason::ProtocolViolation)
        );
    }

    #[tokio::test]
    async fn post_ready_clean_eof_racing_leader_poll_is_always_unexpected_exit() {
        let script = r#"#!/bin/sh
IFS= read -r hello || exit 10
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":[],"resume":null}'
IFS= read -r ready || exit 11
exit 0
"#;

        // Repeat the fixture so both stdout EOF and the 50 ms leader poll can
        // win scheduling without changing the fixed operator-visible cause.
        for _ in 0..8 {
            let (temp, mut activation) = executable_fixture(script);
            install_fixture_store(&temp, &mut activation);
            activation.events.clear();
            let status = RuntimeStatusCache::default();
            let retained = tokio::time::timeout(
                Duration::from_secs(5),
                run_service(
                    activation,
                    Uuid::new_v4(),
                    CancellationToken::new(),
                    status.clone(),
                ),
            )
            .await
            .unwrap();
            assert!(retained.is_none());
            assert_eq!(status.snapshot()[0].state, RuntimeState::Unhealthy);
            assert_eq!(
                status.snapshot()[0].reason,
                Some(RuntimeReason::UnexpectedExit)
            );
        }
    }

    #[tokio::test]
    async fn abrupt_leader_exit_cleans_surviving_grandchild_before_reap() {
        let script = r#"#!/bin/sh
IFS= read -r hello || exit 10
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":[],"resume":null}'
IFS= read -r ready || exit 11
sleep 30 &
printf '%s' "$!" > "$HOME/grandchild.pid"
exit 0
"#;
        let (temp, mut activation) = executable_fixture(script);
        activation.events.clear();
        let config = temp.path().join("config");
        fs::create_dir(&config).unwrap();
        activation.config_dir = config.clone();
        fs::create_dir(config.join("extensions")).unwrap();
        let store = config
            .join("extensions/store/example.noop")
            .join("a".repeat(64));
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        fs::rename(&activation.package_path, &store).unwrap();
        activation.package_path = store;
        let status = RuntimeStatusCache::default();
        let retained = tokio::time::timeout(
            Duration::from_secs(5),
            run_service(activation, Uuid::new_v4(), CancellationToken::new(), status),
        )
        .await
        .unwrap();
        assert!(retained.is_none());
        let grandchild: libc::pid_t =
            fs::read_to_string(config.join("extensions/state/example.noop/data/grandchild.pid"))
                .unwrap()
                .parse()
                .unwrap();
        // SAFETY: signal 0 performs an existence check only.
        let exists = unsafe { libc::kill(grandchild, 0) } == 0;
        assert!(!exists, "surviving grandchild was not cleaned");
    }

    #[tokio::test]
    async fn exceptional_signal_error_preserves_authority_for_bounded_retry() {
        use std::cell::Cell;

        struct FailingSignalSyscalls {
            signal_calls: Cell<usize>,
        }

        impl ProcessGroupSyscalls for FailingSignalSyscalls {
            fn retained_leader_state(
                &self,
                leader: libc::pid_t,
            ) -> Result<RetainedLeaderState, SignalError> {
                OsProcessGroupSyscalls.retained_leader_state(leader)
            }

            fn signal_group(&self, _pgid: libc::pid_t, _signal: libc::c_int) -> io::Result<()> {
                self.signal_calls.set(self.signal_calls.get() + 1);
                Err(io::Error::from_raw_os_error(libc::EPERM))
            }

            fn group_has_live_members(&self, _pgid: libc::pid_t) -> io::Result<bool> {
                Err(io::Error::from_raw_os_error(libc::EIO))
            }
        }

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the child changes only its process group before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        let owner = ProcessGroupOwner::new(child.id().unwrap());
        let mut authority = CleanupAuthority { child, owner };
        let syscalls = FailingSignalSyscalls {
            signal_calls: Cell::new(0),
        };
        let started = tokio::time::Instant::now();
        assert!(
            !terminate_process_group_with(&mut authority.child, &mut authority.owner, &syscalls,)
                .await
        );
        assert!(started.elapsed() >= GROUP_TERM_TIMEOUT);
        assert!(started.elapsed() < GROUP_TERM_TIMEOUT + Duration::from_secs(1));
        assert!(syscalls.signal_calls.get() > 1);
        assert!(authority.owner.leader_owned());
        assert!(authority.child.try_wait().unwrap().is_none());

        assert!(terminate_process_group(&mut authority.child, &mut authority.owner).await);
    }

    #[tokio::test]
    async fn reaped_real_leader_and_simulated_reused_pgid_issue_no_signal_syscall() {
        use std::cell::Cell;

        struct ReusedPgidSyscalls {
            pgid: libc::pid_t,
            signal_calls: Cell<usize>,
        }

        impl ProcessGroupSyscalls for ReusedPgidSyscalls {
            fn retained_leader_state(
                &self,
                leader: libc::pid_t,
            ) -> Result<RetainedLeaderState, SignalError> {
                assert_eq!(leader, self.pgid);
                // Consult the real kernel identity: the fixture leader was
                // reaped, so the numeric slot no longer names our child.
                OsProcessGroupSyscalls.retained_leader_state(leader)
            }

            fn signal_group(&self, pgid: libc::pid_t, _signal: libc::c_int) -> io::Result<()> {
                assert_eq!(pgid, self.pgid);
                self.signal_calls.set(self.signal_calls.get() + 1);
                Ok(())
            }

            fn group_has_live_members(&self, pgid: libc::pid_t) -> io::Result<bool> {
                assert_eq!(pgid, self.pgid);
                // Deterministically model the same numeric PGID now belonging
                // to an unrelated generation; forcing kernel PID wrap is not a
                // reliable or safe test fixture.
                Ok(true)
            }
        }

        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: the child changes only its process group before exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut leader = command.spawn().unwrap();
        let pid = leader.id().unwrap();
        let owner = ProcessGroupOwner::new(pid);
        leader.wait().await.unwrap();

        let syscalls = ReusedPgidSyscalls {
            pgid: owner.pgid,
            signal_calls: Cell::new(0),
        };
        assert_eq!(
            owner.signal_with(libc::SIGKILL, &syscalls),
            Err(SignalError::LostOwnership)
        );
        assert_eq!(syscalls.signal_calls.get(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn assigned_roots_reject_symlink_replacement() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("extensions")).unwrap();
        let attacker = temp.path().join("attacker");
        fs::create_dir(&attacker).unwrap();
        symlink(&attacker, temp.path().join("extensions/state")).unwrap();
        assert!(assigned_roots(temp.path(), "example.noop", "lifecycle", Uuid::new_v4()).is_err());
    }
}
