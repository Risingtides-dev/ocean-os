//! Stage A2a minimum native extension-service supervisor.
//!
//! This boundary consumes only coherent activation records from
//! `extension_registry`, launches one exact trusted executable with assigned
//! roots and a cleared environment, performs the strict v1 hello/ready exchange,
//! and owns generation-safe Unix process-group cleanup. It deliberately has no
//! lifecycle delivery/replay, ping health, restart/backoff, mutation, or route.

use std::{
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    fs::{self, File},
    io,
    os::fd::AsRawFd,
    os::unix::ffi::{OsStrExt, OsStringExt},
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
}

impl ExtensionSupervisor {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            boot_id: Uuid::new_v4(),
            cancel: CancellationToken::new(),
            status: RuntimeStatusCache::default(),
            root_task: Mutex::new(None),
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
            services.spawn(async move {
                run_service(activation, boot_id, cancel, status).await;
            });
        }
        while services.join_next().await.is_some() {}
    }

    pub(crate) async fn shutdown(&self) {
        self.cancel.cancel();
        let Some(mut task) = self.root_task.lock().await.take() else {
            return;
        };
        if tokio::time::timeout(SUPERVISOR_SHUTDOWN_TIMEOUT, &mut task)
            .await
            .is_err()
        {
            // Every launched service has a shorter internal cleanup bound. A
            // timeout here can therefore only be stuck pre-spawn registry I/O;
            // aborting the async waiter cannot launch package code later.
            task.abort();
            tracing::warn!(
                reason = "supervisor_shutdown_timeout",
                "extension supervisor shutdown timed out"
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
    // Keeping descriptors live closes rename/replacement races through spawn.
    _data_handle: File,
    _cache_handle: File,
    _temp_handle: File,
}

#[cfg(unix)]
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
    let service_tmp = open_or_create_private_dir_at(&tmp, service_id)?;
    let connection = connection_id.to_string();
    let temp_handle = open_or_create_private_dir_at(&service_tmp, &connection)?;

    let state_path = canonical_config.join("extensions/state");
    let data = state_path.join(package_id).join("data");
    let cache = state_path.join(package_id).join("cache");
    let temp = state_path
        .join(package_id)
        .join("tmp")
        .join(service_id)
        .join(connection);
    for path in [&data, &cache, &temp] {
        let canonical = fs::canonicalize(path)?;
        if !canonical.starts_with(&state_path) || canonical != *path {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe assigned root",
            ));
        }
    }
    Ok(AssignedRoots {
        data,
        cache,
        temp,
        _data_handle: data_handle,
        _cache_handle: cache_handle,
        _temp_handle: temp_handle,
    })
}

#[cfg(unix)]
fn open_existing_dir_at(parent: &File, name: &str) -> io::Result<File> {
    open_dir_at(parent, name).and_then(validate_private_or_registry_dir)
}

#[cfg(unix)]
fn open_or_create_private_dir_at(parent: &File, name: &str) -> io::Result<File> {
    let name = std::ffi::CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid directory name"))?;
    // SAFETY: parent is a live directory descriptor and name is NUL-terminated.
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    let directory = open_dir_at_cstr(parent, &name)?;
    validate_private_dir(directory)
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
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

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

async fn run_service(
    activation: ServiceActivation,
    boot_id: Uuid,
    cancel: CancellationToken,
    status: RuntimeStatusCache,
) {
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
        return;
    }
    if cancel.is_cancelled() {
        status.update(
            &activation,
            RuntimeState::Inactive,
            None,
            None,
            Some(RuntimeReason::Shutdown),
        );
        return;
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
            return;
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
            let _ = fs::remove_dir_all(&roots.temp);
            return;
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
            let _ = fs::remove_dir_all(&roots.temp);
            return;
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
        return;
    };
    let mut owner = ProcessGroupOwner::new(pid);
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let cleaned = terminate_process_group(&mut child, &mut owner).await;
        if cleaned {
            let _ = fs::remove_dir_all(&roots.temp);
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
        return;
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
        finish_process(
            &activation,
            &status,
            &mut child,
            &mut owner,
            stdin,
            stdout,
            &roots.temp,
            ShutdownReason::DaemonStopping,
        )
        .await;
        return;
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
            finish_process(
                &activation,
                &status,
                &mut child,
                &mut owner,
                stdin,
                stdout,
                &roots.temp,
                ShutdownReason::Unhealthy,
            )
            .await;
            return;
        }
        Err(_) => {
            status.update(
                &activation,
                RuntimeState::Unhealthy,
                Some(pid),
                None,
                Some(RuntimeReason::StartupTimeout),
            );
            finish_process(
                &activation,
                &status,
                &mut child,
                &mut owner,
                stdin,
                stdout,
                &roots.temp,
                ShutdownReason::Unhealthy,
            )
            .await;
            return;
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
    let reason = loop {
        tokio::select! {
            _ = cancel.cancelled() => break ShutdownReason::DaemonStopping,
            _ = leader_poll.tick() => {
                if owner.leader_exited() {
                    break ShutdownReason::Unhealthy;
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
                        break ShutdownReason::Unhealthy;
                    }
                    Ok(ChildFrame::Pong(pong)) => {
                        let _unexpected_nonce = pong.nonce;
                        break ShutdownReason::Unhealthy;
                    }
                    Ok(ChildFrame::ShutdownComplete(_)) | Err(_) => {
                        break ShutdownReason::Unhealthy;
                    }
                }
            }
        }
    };
    finish_process(
        &activation,
        &status,
        &mut child,
        &mut owner,
        stdin,
        stdout,
        &roots.temp,
        reason,
    )
    .await;
}

fn spawn_service(
    activation: &ServiceActivation,
    roots: &AssignedRoots,
    environment: &[(String, SensitiveValue)],
) -> io::Result<Child> {
    let executable_fd = activation.executable.as_raw_fd();
    let package_fd = activation.package_directory.as_raw_fd();
    #[cfg(target_os = "linux")]
    let mut command = Command::new(format!("/proc/self/fd/{executable_fd}"));
    #[cfg(target_os = "macos")]
    let mut command = Command::new(&activation.executable_path);
    #[cfg(target_os = "macos")]
    let executable_path = std::ffi::CString::new(activation.executable_path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid executable path"))?;
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
    // SAFETY: the descriptors stay owned by activation through spawn. The child
    // changes only its copied descriptor flags/cwd/process group before exec.
    unsafe {
        command.pre_exec(move || {
            #[cfg(target_os = "linux")]
            if libc::fcntl(executable_fd, libc::F_SETFD, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "macos")]
            {
                let mut opened = std::mem::MaybeUninit::<libc::stat>::zeroed();
                let mut named = std::mem::MaybeUninit::<libc::stat>::zeroed();
                if libc::fstat(executable_fd, opened.as_mut_ptr()) != 0
                    || libc::stat(executable_path.as_ptr(), named.as_mut_ptr()) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                let opened = opened.assume_init();
                let named = named.assume_init();
                if opened.st_dev != named.st_dev || opened.st_ino != named.st_ino {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "executable generation changed",
                    ));
                }
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
    let service_hello: ServiceHello = read_frame(stdout).await?;
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

async fn read_frame<T: for<'de> Deserialize<'de>, R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<T, ()> {
    let mut encoded = Vec::with_capacity(1024);
    loop {
        let available = reader.fill_buf().await.map_err(|_| ())?;
        if available.is_empty() {
            return Err(());
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let next = encoded.len().checked_add(count).ok_or(())?;
        if next > MAX_FRAME_BYTES {
            return Err(());
        }
        encoded.extend_from_slice(&available[..count]);
        reader.consume(count);
        if encoded.last() == Some(&b'\n') {
            return decode_frame(&encoded).map_err(|_| ());
        }
    }
}

#[allow(clippy::too_many_arguments)] // Explicitly owns child, PGID token, both pipes, temp root, and reason.
async fn finish_process(
    activation: &ServiceActivation,
    status: &RuntimeStatusCache,
    child: &mut Child,
    owner: &mut ProcessGroupOwner,
    mut stdin: ChildStdin,
    mut stdout: BufReader<ChildStdout>,
    temp: &Path,
    reason: ShutdownReason,
) {
    let pid = child.id();
    status.update(activation, RuntimeState::Stopping, pid, None, None);
    let shutdown = Shutdown {
        protocol: ProtocolName,
        version: ProtocolV1,
        frame: ShutdownFrame,
        reason,
    };
    let _ = write_frame(&mut stdin, &shutdown).await;
    let response = async {
        loop {
            match read_frame::<ChildFrame, _>(&mut stdout).await? {
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

    let cleaned = terminate_process_group(child, owner).await;
    let terminal_reason = if cleaned {
        let _ = fs::remove_dir_all(temp);
        if reason == ShutdownReason::DaemonStopping {
            RuntimeReason::Shutdown
        } else {
            RuntimeReason::UnexpectedExit
        }
    } else {
        RuntimeReason::CleanupFailed
    };
    status.update(
        activation,
        if reason == ShutdownReason::DaemonStopping && cleaned {
            RuntimeState::Inactive
        } else {
            RuntimeState::Unhealthy
        },
        if cleaned { None } else { pid },
        None,
        Some(terminal_reason),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalError {
    LostOwnership,
    Os,
}

struct ProcessGroupOwner {
    leader: libc::pid_t,
    pgid: libc::pid_t,
    reaped: bool,
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

    fn leader_owned(&self) -> bool {
        if self.reaped {
            return false;
        }
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: info points to writable siginfo storage. WNOWAIT explicitly
        // preserves the child identity and zombie until group cleanup is proven.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.leader as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        result == 0
    }

    fn leader_exited(&self) -> bool {
        if self.reaped {
            return true;
        }
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: same retained-identity wait as leader_owned.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.leader as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return true;
        }
        // SAFETY: waitid initialized siginfo on success.
        unsafe { info.assume_init().si_pid() == self.leader }
    }

    fn signal(&self, signal: libc::c_int) -> Result<(), SignalError> {
        if !self.leader_owned() {
            return Err(SignalError::LostOwnership);
        }
        // SAFETY: a negative owned PGID targets only the retained generation.
        let result = unsafe { libc::kill(-self.pgid, signal) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH)
            || (error.raw_os_error() == Some(libc::EPERM)
                && matches!(group_has_live_members(self.pgid), Ok(false)))
        {
            Ok(())
        } else {
            Err(SignalError::Os)
        }
    }
}

async fn terminate_process_group(child: &mut Child, owner: &mut ProcessGroupOwner) -> bool {
    if owner.signal(libc::SIGTERM).is_err() {
        return false;
    }
    if !wait_for_group_exit(owner.pgid, GROUP_TERM_TIMEOUT).await {
        if owner.signal(libc::SIGKILL).is_err() {
            return false;
        }
        if !wait_for_group_exit(owner.pgid, GROUP_KILL_TIMEOUT).await {
            return false;
        }
    }
    if !owner.leader_owned() || child.wait().await.is_err() {
        return false;
    }
    owner.reaped = true;
    true
}

async fn wait_for_group_exit(pgid: libc::pid_t, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match group_has_live_members(pgid) {
            Ok(false) => return true,
            Ok(true) => {}
            Err(_) => return false,
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
                executable_path: entry,
                args: Vec::new(),
                events: vec![LifecycleEventKind::DaemonStarted],
                environment: Vec::new(),
                secret_bindings: Vec::new(),
                startup_timeout: Duration::from_secs(2),
            },
        )
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
        async fn decode_service_hello(bytes: Vec<u8>) -> Result<ServiceHello, ()> {
            let capacity = bytes.len().saturating_add(1);
            let (mut writer, reader) = tokio::io::duplex(capacity);
            let write = tokio::spawn(async move { writer.write_all(&bytes).await.unwrap() });
            let result = read_frame(&mut BufReader::new(reader)).await;
            write.await.unwrap();
            result
        }

        let oversized = vec![b'x'; MAX_FRAME_BYTES + 1];
        assert!(decode_service_hello(oversized).await.is_err());
        assert!(decode_service_hello(
            b"{\"protocol\":\"ocean.extension.service\",\"version\":1,\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null}\n"
                .to_vec()
        )
        .await
        .is_err());
        assert!(decode_service_hello(
            b"{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null,\"identity\":\"override\"}\n"
                .to_vec()
        )
        .await
        .is_err());

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
        activation.executable_path = store.join("service");
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
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .unwrap()
            .unwrap();
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
        activation.executable_path = store.join("service");
        activation.package_path = store;
        let status = RuntimeStatusCache::default();
        tokio::time::timeout(
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
        assert_eq!(status.snapshot()[0].state, RuntimeState::Unhealthy);
        assert_eq!(
            status.snapshot()[0].reason,
            Some(RuntimeReason::UnexpectedExit)
        );
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
        activation.executable_path = store.join("service");
        activation.package_path = store;
        let status = RuntimeStatusCache::default();
        tokio::time::timeout(
            Duration::from_secs(5),
            run_service(activation, Uuid::new_v4(), CancellationToken::new(), status),
        )
        .await
        .unwrap();
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
    async fn lost_leader_authority_never_signals_a_reused_group() {
        let mut unrelated = Command::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = unrelated.id().unwrap();
        let mut owner = ProcessGroupOwner::new(pid);
        owner.reaped = true;
        assert_eq!(owner.signal(libc::SIGKILL), Err(SignalError::LostOwnership));
        // SAFETY: signal 0 performs an existence check only.
        assert_eq!(unsafe { libc::kill(pid as libc::pid_t, 0) }, 0);
        unrelated.kill().await.unwrap();
        unrelated.wait().await.unwrap();
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
