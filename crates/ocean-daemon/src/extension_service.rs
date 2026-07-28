//! Stage A native extension-service supervisor.
//!
//! This boundary consumes coherent activation records, owns strict stdio
//! transport, bounded lifecycle replay/live delivery, heartbeat health,
//! deterministic restart policy, sanitized stderr diagnostics, and
//! generation-safe Unix process-group cleanup. It has no registry mutation,
//! route, package acquisition, or extension-originated command authority.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
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
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};

use chrono::{SecondsFormat, Utc};
use ocean_agent_sdk::extension_lifecycle::{
    decode_frame, encode_frame, Ack, HostHello, HostHelloFrame, Lag, LagFrame, LifecycleEvent,
    LifecycleEventKind, Ping, PingFrame, Pong, ProtocolName, ProtocolV1, Ready, ReadyFrame,
    ReplayMode, Reset, ResetFrame, ResetReason, ResumeCursor, Sequence, ServiceHello,
    ServiceIdentity, ServiceLimits, ServiceStatus, ServiceStatusCode, ServiceStatusState, Shutdown,
    ShutdownComplete, ShutdownFrame, ShutdownReason,
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
    sync::{broadcast, mpsc, oneshot, Mutex, Notify},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    extension_lifecycle::{ActivationScope, LifecycleAttach, LifecycleDispatcher},
    extension_registry::{
        env_secret_source, read_service_activations, reserved_child_environment_name,
        valid_child_environment_name, SecretBinding, ServiceActivation,
    },
};

const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
const GRACEFUL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(2);
const DIAGNOSTIC_JOIN_TIMEOUT: Duration = Duration::from_millis(250);
const ACK_WINDOW_MAX: usize = OUTBOUND_MAX_MESSAGES;
const GROUP_TERM_TIMEOUT: Duration = Duration::from_secs(2);
const GROUP_KILL_TIMEOUT: Duration = Duration::from_secs(2);
const REGISTRY_LOAD_TIMEOUT: Duration = Duration::from_secs(5);
const PROJECT_RECONCILE_TIMEOUT: Duration = Duration::from_secs(15);
const SUPERVISOR_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const LEADER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_FRAME_BYTES: usize = ocean_agent_sdk::extension_lifecycle::MAX_FRAME_BYTES;
const OUTBOUND_MAX_MESSAGES: usize = 256;
const OUTBOUND_MAX_BYTES: usize = 1024 * 1024;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(5);
const HEARTBEAT_MAX_MISSES: u8 = 3;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const CIRCUIT_FAILURES: usize = 5;
const STABLE_RESET: Duration = Duration::from_secs(5 * 60);
const BACKOFF: [Duration; 8] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];
const STDERR_MAX_LINE: usize = 8 * 1024;
const STDERR_RING_BYTES: usize = 64 * 1024;
const STDERR_RATE_PER_SECOND: u32 = 20;
const STDERR_BURST: u32 = 40;

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
    PingTimeout,
    UnexpectedExit,
    Shutdown,
    CleanupFailed,
    ExternalUnavailable,
    ConfigurationMissing,
    RateLimited,
    ChildUnknown,
    UnsupportedPlatform,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    stderr_bytes: u64,
    stderr_lines: u64,
    stderr_discarded_bytes: u64,
    stderr_truncated_lines: u64,
    stderr_redactions: u64,
    temp_cleanup_failures: u64,
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

    fn insert_starting(
        &self,
        activation: &ServiceActivation,
        epoch: Uuid,
        replay_floor: Sequence,
        restart_count: u64,
    ) {
        let mut status = RuntimeStatus {
            package_id: activation.package_id.clone(),
            package_version: activation.package_version.clone(),
            package_digest: activation.package_digest.clone(),
            service_id: activation.service_id.clone(),
            activation_revision: activation.activation_revision,
            activation_epoch: epoch,
            replay_floor,
            state: RuntimeState::Starting,
            pid: None,
            started_at: None,
            observed_at: now_string(),
            restart_count,
            negotiated_subscriptions: Vec::new(),
            last_acknowledged_sequence: None,
            lag_count: 0,
            stderr_bytes: 0,
            stderr_lines: 0,
            stderr_discarded_bytes: 0,
            stderr_truncated_lines: 0,
            stderr_redactions: 0,
            temp_cleanup_failures: 0,
            reason: None,
        };
        let key = ServiceKey {
            package_id: activation.package_id.clone(),
            service_id: activation.service_id.clone(),
        };
        let mut statuses = self.write();
        if let Some(previous) = statuses
            .get(&key)
            .filter(|previous| previous.activation_epoch == epoch)
        {
            // Process-failure restarts retain the activation epoch and its
            // connection-independent progress projection. A scope/grant/digest
            // change mints a new epoch and must not carry an old ACK or lag
            // posture across that replay boundary.
            status.last_acknowledged_sequence = previous.last_acknowledged_sequence;
            status.lag_count = previous.lag_count;
            status.stderr_bytes = previous.stderr_bytes;
            status.stderr_lines = previous.stderr_lines;
            status.stderr_discarded_bytes = previous.stderr_discarded_bytes;
            status.stderr_truncated_lines = previous.stderr_truncated_lines;
            status.stderr_redactions = previous.stderr_redactions;
            status.temp_cleanup_failures = previous.temp_cleanup_failures;
        }
        statuses.insert(key, status);
    }

    fn key(activation: &ServiceActivation) -> ServiceKey {
        ServiceKey {
            package_id: activation.package_id.clone(),
            service_id: activation.service_id.clone(),
        }
    }

    fn clear_ack(&self, activation: &ServiceActivation) {
        if let Some(status) = self.write().get_mut(&Self::key(activation)) {
            status.last_acknowledged_sequence = None;
            status.observed_at = now_string();
        }
    }

    fn update_ack(&self, activation: &ServiceActivation, sequence: Sequence) {
        if let Some(status) = self.write().get_mut(&Self::key(activation)) {
            status.last_acknowledged_sequence = Some(sequence);
            status.observed_at = now_string();
        }
    }

    fn add_lag(&self, activation: &ServiceActivation, count: u64) {
        if let Some(status) = self.write().get_mut(&Self::key(activation)) {
            status.lag_count = status.lag_count.saturating_add(count);
            status.observed_at = now_string();
        }
    }

    fn retain_only(&self, desired: &HashSet<ServiceKey>) {
        self.write().retain(|key, _| desired.contains(key));
    }

    fn record_temp_cleanup_failure(&self, activation: &ServiceActivation) {
        if let Some(status) = self.write().get_mut(&Self::key(activation)) {
            status.temp_cleanup_failures = status.temp_cleanup_failures.saturating_add(1);
            status.observed_at = now_string();
        }
    }

    fn update_diagnostics(&self, activation: &ServiceActivation, diagnostics: &DiagnosticStats) {
        if let Some(status) = self.write().get_mut(&Self::key(activation)) {
            status.stderr_bytes = status.stderr_bytes.saturating_add(diagnostics.input_bytes);
            status.stderr_lines = status
                .stderr_lines
                .saturating_add(diagnostics.retained_lines);
            status.stderr_discarded_bytes = status
                .stderr_discarded_bytes
                .saturating_add(diagnostics.discarded_bytes);
            status.stderr_truncated_lines = status
                .stderr_truncated_lines
                .saturating_add(diagnostics.truncated_lines);
            status.stderr_redactions = status
                .stderr_redactions
                .saturating_add(diagnostics.redactions);
            status.observed_at = now_string();
        }
    }

    fn update(
        &self,
        activation: &ServiceActivation,
        state: RuntimeState,
        pid: Option<u32>,
        subscriptions: Option<&[LifecycleEventKind]>,
        reason: Option<RuntimeReason>,
    ) {
        if let Some(status) = self.write().get_mut(&Self::key(activation)) {
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

#[derive(Clone, PartialEq, Eq)]
struct ServiceDescriptor {
    package_digest: String,
    activation_revision: u64,
    args: Vec<String>,
    events: Vec<LifecycleEventKind>,
    environment: Vec<String>,
    secret_bindings: Vec<SecretBinding>,
    restart_on_failure: bool,
    effective_global: bool,
    effective_projects: HashSet<Uuid>,
}

impl ServiceDescriptor {
    fn from_activation(activation: &ServiceActivation) -> Self {
        Self {
            package_digest: activation.package_digest.clone(),
            activation_revision: activation.activation_revision,
            args: activation.args.clone(),
            events: activation.events.clone(),
            environment: activation.environment.clone(),
            secret_bindings: activation.secret_bindings.clone(),
            restart_on_failure: activation.restart_on_failure,
            effective_global: activation.effective_global,
            effective_projects: activation.effective_projects.clone(),
        }
    }
}

#[derive(Default)]
struct RestartHistory {
    failures: VecDeque<tokio::time::Instant>,
    backoff_index: usize,
    restart_count: u64,
    circuit_open: bool,
    blocked_reason: Option<RuntimeReason>,
}

struct ManagedService {
    descriptor: ServiceDescriptor,
    cancel: CancellationToken,
    stop_reason: Arc<std::sync::Mutex<ShutdownReason>>,
    history: Arc<std::sync::Mutex<RestartHistory>>,
    task: JoinHandle<Option<CleanupAuthority>>,
}

fn requested_stop_reason(reason: &std::sync::Mutex<ShutdownReason>) -> ShutdownReason {
    *reason
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn request_service_stop(managed: &ManagedService, reason: ShutdownReason) {
    *managed
        .stop_reason
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = reason;
    managed.cancel.cancel();
}

struct ProjectSnapshotCommand {
    projects: HashSet<Uuid>,
    completed: oneshot::Sender<Result<(), ProjectSnapshotError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectSnapshotError {
    SupervisorUnavailable,
    RegistryUnavailable,
    CleanupIncomplete,
}

/// One boot-local supervisor. Startup is fail-soft and asynchronous; project
/// reconciliation is serialized, and shutdown explicitly joins every process
/// owner under a hard wall-clock bound.
pub(crate) struct ExtensionSupervisor {
    lifecycle: Arc<LifecycleDispatcher>,
    cancel: CancellationToken,
    status: RuntimeStatusCache,
    root_task: Mutex<Option<JoinHandle<()>>>,
    project_tx: mpsc::Sender<ProjectSnapshotCommand>,
    project_rx: Mutex<Option<mpsc::Receiver<ProjectSnapshotCommand>>>,
    retained_cleanup: Mutex<Vec<CleanupAuthority>>,
}

impl ExtensionSupervisor {
    pub(crate) fn new_with_lifecycle(lifecycle: Arc<LifecycleDispatcher>) -> Arc<Self> {
        let (project_tx, project_rx) = mpsc::channel(8);
        Arc::new(Self {
            lifecycle,
            cancel: CancellationToken::new(),
            status: RuntimeStatusCache::default(),
            root_task: Mutex::new(None),
            project_tx,
            project_rx: Mutex::new(Some(project_rx)),
            retained_cleanup: Mutex::new(Vec::new()),
        })
    }

    pub(crate) fn status_cache(&self) -> RuntimeStatusCache {
        self.status.clone()
    }

    pub(crate) async fn update_project_snapshot(
        &self,
        projects: HashSet<Uuid>,
    ) -> Result<(), ProjectSnapshotError> {
        let update = async {
            let (completed, wait) = oneshot::channel();
            self.project_tx
                .send(ProjectSnapshotCommand {
                    projects,
                    completed,
                })
                .await
                .map_err(|_| ProjectSnapshotError::SupervisorUnavailable)?;
            wait.await
                .map_err(|_| ProjectSnapshotError::SupervisorUnavailable)?
        };
        tokio::time::timeout(PROJECT_RECONCILE_TIMEOUT, update)
            .await
            .map_err(|_| ProjectSnapshotError::SupervisorUnavailable)?
    }

    pub(crate) async fn start(
        self: &Arc<Self>,
        config_dir: PathBuf,
        registered_projects: HashSet<Uuid>,
    ) {
        self.lifecycle
            .update_registered_projects(registered_projects.clone());
        let Some(project_rx) = self.project_rx.lock().await.take() else {
            return;
        };
        let supervisor = Arc::clone(self);
        let task = tokio::spawn(async move {
            supervisor
                .run_reconciliation(config_dir, registered_projects, project_rx)
                .await;
        });
        *self.root_task.lock().await = Some(task);
    }

    async fn load_activations(
        config_dir: PathBuf,
        projects: HashSet<Uuid>,
    ) -> Result<Vec<ServiceActivation>, ProjectSnapshotError> {
        let load =
            tokio::task::spawn_blocking(move || read_service_activations(&config_dir, &projects));
        match tokio::time::timeout(REGISTRY_LOAD_TIMEOUT, load).await {
            Ok(Ok(Ok(activations))) => Ok(activations),
            Ok(Ok(Err(error))) => {
                tracing::warn!(reason = error.code(), "extension reconciliation blocked");
                Err(ProjectSnapshotError::RegistryUnavailable)
            }
            Ok(Err(_)) => {
                tracing::warn!(
                    reason = "registry_reader_failed",
                    "extension reconciliation blocked"
                );
                Err(ProjectSnapshotError::RegistryUnavailable)
            }
            Err(_) => {
                tracing::warn!(
                    reason = "registry_reader_timeout",
                    "extension reconciliation blocked"
                );
                Err(ProjectSnapshotError::RegistryUnavailable)
            }
        }
    }

    async fn stop_managed(
        &self,
        managed: ManagedService,
        reason: ShutdownReason,
    ) -> Result<(), ProjectSnapshotError> {
        request_service_stop(&managed, reason);
        match managed.task.await {
            Ok(Some(mut authority)) => {
                if authority.cleanup().await {
                    Ok(())
                } else {
                    self.retained_cleanup.lock().await.push(authority);
                    Err(ProjectSnapshotError::CleanupIncomplete)
                }
            }
            Ok(None) => Ok(()),
            Err(_) => {
                tracing::warn!(
                    reason = "service_task_failed",
                    "extension service task failed"
                );
                Err(ProjectSnapshotError::CleanupIncomplete)
            }
        }
    }

    async fn retry_retained_cleanup(&self) -> Result<(), ProjectSnapshotError> {
        let retained = std::mem::take(&mut *self.retained_cleanup.lock().await);
        if retained.is_empty() {
            return Ok(());
        }
        let mut failed = Vec::new();
        for mut authority in retained {
            if !authority.cleanup().await {
                failed.push(authority);
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            self.retained_cleanup.lock().await.extend(failed);
            Err(ProjectSnapshotError::CleanupIncomplete)
        }
    }

    async fn reconcile(
        &self,
        config_dir: &Path,
        projects: &HashSet<Uuid>,
        services: &mut BTreeMap<ServiceKey, ManagedService>,
    ) -> Result<(), ProjectSnapshotError> {
        // Never activate a replacement generation while an obsolete process
        // group still has retained cleanup authority.
        self.retry_retained_cleanup().await?;
        let activations =
            Self::load_activations(config_dir.to_path_buf(), projects.clone()).await?;
        let mut desired = BTreeMap::new();
        for activation in activations {
            let key = ServiceKey {
                package_id: activation.package_id.clone(),
                service_id: activation.service_id.clone(),
            };
            desired.insert(key, activation);
        }

        let stale: Vec<ServiceKey> = services
            .iter()
            .filter_map(|(key, managed)| {
                desired
                    .get(key)
                    .is_none_or(|activation| {
                        managed.descriptor != ServiceDescriptor::from_activation(activation)
                    })
                    .then_some(key.clone())
            })
            .collect();
        let mut preserved_histories = BTreeMap::new();
        for key in stale {
            if let Some(managed) = services.remove(&key) {
                if desired.get(&key).is_some_and(|activation| {
                    managed.descriptor.package_digest == activation.package_digest
                }) {
                    preserved_histories.insert(key.clone(), Arc::clone(&managed.history));
                }
                self.stop_managed(managed, ShutdownReason::Reconfigure)
                    .await?;
            }
        }

        let desired_keys = desired.keys().cloned().collect::<HashSet<_>>();
        self.status.retain_only(&desired_keys);

        for (key, activation) in desired {
            if services.contains_key(&key) {
                continue;
            }
            let descriptor = ServiceDescriptor::from_activation(&activation);
            let history = preserved_histories.remove(&key).unwrap_or_default();
            let cancel = CancellationToken::new();
            let task_cancel = cancel.clone();
            let stop_reason = Arc::new(std::sync::Mutex::new(ShutdownReason::DaemonStopping));
            let task_stop_reason = Arc::clone(&stop_reason);
            let lifecycle = Arc::clone(&self.lifecycle);
            let status = self.status.clone();
            // Mint the immutable epoch and floor before reconciliation
            // acknowledges a project-snapshot change.
            let epoch = Uuid::new_v4();
            let replay_floor = lifecycle.current_sequence();
            let scope = activation_scope(&activation);
            let task_history = Arc::clone(&history);
            let task = tokio::spawn(async move {
                run_service_with_epoch(
                    activation,
                    lifecycle,
                    task_cancel,
                    status,
                    epoch,
                    replay_floor,
                    scope,
                    task_stop_reason,
                    task_history,
                )
                .await
            });
            services.insert(
                key,
                ManagedService {
                    descriptor,
                    cancel,
                    stop_reason,
                    history,
                    task,
                },
            );
        }
        Ok(())
    }

    async fn run_reconciliation(
        self: Arc<Self>,
        config_dir: PathBuf,
        mut projects: HashSet<Uuid>,
        mut project_rx: mpsc::Receiver<ProjectSnapshotCommand>,
    ) {
        let mut services = BTreeMap::new();
        let _ = self.reconcile(&config_dir, &projects, &mut services).await;
        loop {
            tokio::select! {
                _ = self.cancel.cancelled() => break,
                command = project_rx.recv() => {
                    let Some(command) = command else { break };
                    projects = command.projects;
                    let result = self.reconcile(&config_dir, &projects, &mut services).await;
                    let _ = command.completed.send(result);
                }
            }
        }
        // Broadcast cancellation to every owner before awaiting any one group.
        // Each bounded cleanup then progresses concurrently; a slow first
        // service cannot consume the whole daemon shutdown budget while later
        // services remain running.
        for managed in services.values() {
            request_service_stop(managed, ShutdownReason::DaemonStopping);
        }
        for (_, managed) in services {
            let _ = self
                .stop_managed(managed, ShutdownReason::DaemonStopping)
                .await;
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.cancel.cancel();
        let deadline = tokio::time::Instant::now() + SUPERVISOR_SHUTDOWN_TIMEOUT;
        if let Some(mut task) = self.root_task.lock().await.take() {
            if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
                tracing::warn!(
                    reason = "supervisor_shutdown_timeout",
                    "extension supervisor shutdown exceeded its deadline; retaining cleanup ownership"
                );
                // Never abort the owner task: dropping its nested JoinHandles
                // would detach native processes. Attach and cleanup paths are
                // independently bounded, so retain and join authority here.
                let _ = task.await;
            }
        }

        let retained = std::mem::take(&mut *self.retained_cleanup.lock().await);
        let mut retries = JoinSet::new();
        for mut authority in retained {
            retries.spawn(async move { authority.cleanup().await });
        }
        while retries.join_next().await.is_some() {}
        // Each authority performs one independently bounded group retry. Never
        // wrap this owner set in a timeout whose cancellation would drop and
        // detach the remaining native children.
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

    fn duplicate_for_redaction(&self) -> Self {
        Self(self.0.clone())
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

fn cleanup_temp_root_accounted(
    roots: &AssignedRoots,
    status: &RuntimeStatusCache,
    activation: &ServiceActivation,
) -> bool {
    let cleaned = cleanup_temp_root(roots);
    if !cleaned {
        status.record_temp_cleanup_failure(activation);
    }
    cleaned
}

#[derive(Debug, Clone, Default)]
struct DiagnosticStats {
    input_bytes: u64,
    retained_lines: u64,
    discarded_bytes: u64,
    truncated_lines: u64,
    redactions: u64,
}

#[derive(Default)]
struct DiagnosticState {
    lines: VecDeque<Vec<u8>>,
    retained_bytes: usize,
    stats: DiagnosticStats,
}

type SharedDiagnostics = Arc<std::sync::Mutex<DiagnosticState>>;

#[derive(Debug)]
struct QueuedEvent {
    event: LifecycleEvent,
    encoded_bytes: usize,
}

#[derive(Debug)]
enum HostControl {
    Shutdown(Shutdown),
    Reset(Reset),
    Lag(Lag),
    Ping(Ping),
}

#[derive(Default)]
struct PendingControls {
    shutdown: Option<Shutdown>,
    reset: Option<Reset>,
    lag: Option<Lag>,
    ping: Option<Ping>,
}

#[derive(Default)]
struct ControlLane {
    pending: std::sync::Mutex<PendingControls>,
    notify: Notify,
    lag_total: AtomicU64,
}

impl ControlLane {
    fn lag(&self, first: Sequence, last: Sequence, count: u64, replay_available: bool) {
        self.lag_total.fetch_add(count, Ordering::Relaxed);
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.shutdown.is_some() || pending.reset.is_some() {
            return;
        }
        match pending.lag.as_mut() {
            Some(lag) => {
                lag.first_lost = Sequence(lag.first_lost.0.min(first.0));
                lag.last_lost = Sequence(lag.last_lost.0.max(last.0));
                lag.lost_count = lag.lost_count.saturating_add(count);
                lag.replay_available |= replay_available;
            }
            None => {
                pending.lag = Some(Lag {
                    protocol: ProtocolName,
                    version: ProtocolV1,
                    frame: LagFrame,
                    first_lost: first,
                    last_lost: last,
                    lost_count: count,
                    replay_available,
                });
            }
        }
        drop(pending);
        self.notify.notify_one();
    }

    #[allow(dead_code)]
    fn reset(&self, reset: Reset) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.shutdown.is_none() {
            pending.reset = Some(reset);
            pending.lag = None;
        }
        drop(pending);
        self.notify.notify_one();
    }

    fn ping(&self, nonce: Uuid) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.shutdown.is_none() {
            pending.ping = Some(Ping {
                protocol: ProtocolName,
                version: ProtocolV1,
                frame: PingFrame,
                nonce,
            });
        }
        drop(pending);
        self.notify.notify_one();
    }

    fn shutdown(&self, reason: ShutdownReason) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending.shutdown = Some(Shutdown {
            protocol: ProtocolName,
            version: ProtocolV1,
            frame: ShutdownFrame,
            reason,
        });
        pending.reset = None;
        pending.lag = None;
        pending.ping = None;
        drop(pending);
        self.notify.notify_one();
    }

    fn pop(&self) -> Option<HostControl> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending
            .shutdown
            .take()
            .map(HostControl::Shutdown)
            .or_else(|| pending.reset.take().map(HostControl::Reset))
            .or_else(|| pending.lag.take().map(HostControl::Lag))
            .or_else(|| pending.ping.take().map(HostControl::Ping))
    }
}

fn pong_deadline_after_ping_write() -> tokio::time::Instant {
    tokio::time::Instant::now() + HEARTBEAT_TIMEOUT
}

fn activation_scope(activation: &ServiceActivation) -> ActivationScope {
    ActivationScope {
        global: activation.effective_global,
        projects: activation.effective_projects.clone(),
    }
}

fn eligible_events(
    attach: &LifecycleAttach,
    scope: &ActivationScope,
    subscriptions: &[LifecycleEventKind],
    replay_floor: Sequence,
) -> Vec<LifecycleEvent> {
    attach
        .retained
        .iter()
        .filter(|event| {
            scope.eligible(event)
                && subscriptions.contains(&event.kind)
                && (event.sequence.0 > replay_floor.0
                    || event.kind == LifecycleEventKind::DaemonStarted)
        })
        .cloned()
        .collect()
}

fn reset_frame(reason: ResetReason, eligible: &[LifecycleEvent]) -> Reset {
    Reset {
        protocol: ProtocolName,
        version: ProtocolV1,
        frame: ResetFrame,
        reason,
        oldest_available: eligible.first().map(|event| event.sequence),
        latest_available: eligible.last().map(|event| event.sequence),
    }
}

fn replay_plan(
    resume: Option<&ResumeCursor>,
    boot_id: Uuid,
    epoch: Uuid,
    replay_floor: Sequence,
    eligible: &[LifecycleEvent],
) -> (Option<Reset>, Vec<LifecycleEvent>) {
    let Some(resume) = resume else {
        return (None, eligible.to_vec());
    };
    let reason = if resume.daemon_boot_id != boot_id {
        Some(ResetReason::BootChanged)
    } else if resume.activation_epoch != epoch || resume.after_sequence.0 < replay_floor.0 {
        Some(ResetReason::ActivationChanged)
    } else if !eligible
        .iter()
        .any(|event| event.sequence == resume.after_sequence)
    {
        let oldest = eligible
            .first()
            .map_or(attach_floor(replay_floor), |event| event.sequence.0);
        Some(if resume.after_sequence.0 < oldest {
            ResetReason::RetentionExceeded
        } else {
            ResetReason::InvalidCursor
        })
    } else {
        None
    };
    match reason {
        Some(reason) => (Some(reset_frame(reason, eligible)), Vec::new()),
        None => (
            None,
            eligible
                .iter()
                .filter(|event| event.sequence.0 > resume.after_sequence.0)
                .cloned()
                .collect(),
        ),
    }
}

const fn attach_floor(floor: Sequence) -> u64 {
    floor.0.saturating_add(1)
}

#[derive(Default)]
struct AckLedger {
    pending: VecDeque<u64>,
    last_ack: Option<u64>,
}

impl AckLedger {
    fn record_sent(&mut self, sequence: Sequence) -> Result<(), ()> {
        if self.pending.len() >= ACK_WINDOW_MAX
            || self
                .pending
                .back()
                .is_some_and(|previous| *previous >= sequence.0)
        {
            return Err(());
        }
        self.pending.push_back(sequence.0);
        Ok(())
    }

    fn acknowledge(&mut self, sequence: Sequence) -> Result<(), ()> {
        if self.last_ack.is_some_and(|last| sequence.0 <= last)
            || !self.pending.contains(&sequence.0)
        {
            return Err(());
        }
        self.last_ack = Some(sequence.0);
        while self
            .pending
            .front()
            .is_some_and(|pending| *pending <= sequence.0)
        {
            self.pending.pop_front();
        }
        Ok(())
    }

    fn is_full(&self) -> bool {
        self.pending.len() >= ACK_WINDOW_MAX
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachError {
    Cancelled,
    Timeout,
    ProtocolViolation,
}

type ChildStatusProjection = (RuntimeState, Option<RuntimeReason>);

fn child_status_projection(child_status: &ServiceStatus) -> ChildStatusProjection {
    let state = match child_status.state {
        ServiceStatusState::Ready => RuntimeState::Healthy,
        ServiceStatusState::Degraded => RuntimeState::Degraded,
    };
    let reason = (state == RuntimeState::Degraded).then_some(match child_status.code {
        ServiceStatusCode::ExternalUnavailable => RuntimeReason::ExternalUnavailable,
        ServiceStatusCode::ConfigurationMissing => RuntimeReason::ConfigurationMissing,
        ServiceStatusCode::RateLimited => RuntimeReason::RateLimited,
        ServiceStatusCode::Unknown => RuntimeReason::ChildUnknown,
    });
    (state, reason)
}

fn validate_attach_child_frame(
    frame: ChildFrame,
    ledger: &mut AckLedger,
    child_status: &mut Option<ChildStatusProjection>,
    status: &RuntimeStatusCache,
    activation: &ServiceActivation,
) -> Result<(), AttachError> {
    match frame {
        ChildFrame::Ack(ack) => {
            ledger
                .acknowledge(ack.sequence)
                .map_err(|_| AttachError::ProtocolViolation)?;
            status.update_ack(activation, ack.sequence);
            Ok(())
        }
        // Status is legal immediately after ready. Keep only the newest closed
        // projection and publish it once replay/live attach is complete, so a
        // long replay neither lies about readiness nor loses degraded state.
        ChildFrame::Status(frame) => {
            *child_status = Some(child_status_projection(&frame));
            Ok(())
        }
        ChildFrame::Pong(_) | ChildFrame::ShutdownComplete(_) => {
            Err(AttachError::ProtocolViolation)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn wait_attach_child_frame<R: AsyncBufRead + Unpin>(
    stdout: &mut R,
    ledger: &mut AckLedger,
    child_status: &mut Option<ChildStatusProjection>,
    status: &RuntimeStatusCache,
    activation: &ServiceActivation,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
) -> Result<(), AttachError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(AttachError::Cancelled),
        _ = tokio::time::sleep_until(deadline) => Err(AttachError::Timeout),
        frame = read_frame::<ChildFrame, _>(stdout) => {
            let frame = frame.map_err(|_| AttachError::ProtocolViolation)?;
            validate_attach_child_frame(frame, ledger, child_status, status, activation)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn write_attach_frame<W, R, T>(
    stdin: &mut W,
    stdout: &mut R,
    frame: &T,
    sent_sequence: Option<Sequence>,
    ledger: &mut AckLedger,
    child_status: &mut Option<ChildStatusProjection>,
    status: &RuntimeStatusCache,
    activation: &ServiceActivation,
    cancel: &CancellationToken,
    deadline: tokio::time::Instant,
) -> Result<(), AttachError>
where
    W: AsyncWrite + Unpin,
    R: AsyncBufRead + Unpin,
    T: Serialize,
{
    if let Some(sequence) = sent_sequence {
        while ledger.is_full() {
            wait_attach_child_frame(
                stdout,
                ledger,
                child_status,
                status,
                activation,
                cancel,
                deadline,
            )
            .await?;
        }
        // Record before polling the write. Once the complete frame reaches the
        // pipe, a fast child may ACK before Tokio next polls the write future.
        ledger
            .record_sent(sequence)
            .map_err(|_| AttachError::ProtocolViolation)?;
    }
    let write = write_frame(stdin, frame);
    tokio::pin!(write);
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(AttachError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => return Err(AttachError::Timeout),
            result = &mut write => return result.map_err(|_| AttachError::ProtocolViolation),
            child = read_frame::<ChildFrame, _>(stdout) => {
                let child = child.map_err(|_| AttachError::ProtocolViolation)?;
                validate_attach_child_frame(child, ledger, child_status, status, activation)?;
            }
        }
    }
}

fn try_reserve_bytes(bytes: &AtomicUsize, amount: usize) -> bool {
    let mut current = bytes.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount) else {
            return false;
        };
        if next > OUTBOUND_MAX_BYTES {
            return false;
        }
        match bytes.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn feed_live_events(
    dispatcher: Arc<LifecycleDispatcher>,
    mut live: broadcast::Receiver<LifecycleEvent>,
    boundary: Sequence,
    scope: ActivationScope,
    subscriptions: Vec<LifecycleEventKind>,
    tx: mpsc::Sender<QueuedEvent>,
    queued_bytes: Arc<AtomicUsize>,
    controls: Arc<ControlLane>,
    cancel: CancellationToken,
) {
    let mut last_seen = boundary.0;
    loop {
        let event = tokio::select! {
            _ = cancel.cancelled() => break,
            event = live.recv() => event,
        };
        match event {
            Ok(event) => {
                last_seen = event.sequence.0;
                if event.sequence.0 <= boundary.0
                    || !scope.eligible(&event)
                    || !subscriptions.contains(&event.kind)
                {
                    continue;
                }
                let Ok(encoded) = encode_frame(&event) else {
                    continue;
                };
                let encoded_bytes = encoded.len();
                if !try_reserve_bytes(&queued_bytes, encoded_bytes) {
                    let replay_available = dispatcher.replay_available(
                        &scope,
                        &subscriptions,
                        event.sequence,
                        event.sequence,
                    );
                    controls.lag(event.sequence, event.sequence, 1, replay_available);
                    continue;
                }
                match tx.try_send(QueuedEvent {
                    event,
                    encoded_bytes,
                }) {
                    Ok(()) => {}
                    Err(error) => {
                        queued_bytes.fetch_sub(encoded_bytes, Ordering::AcqRel);
                        let event = error.into_inner().event;
                        let replay_available = dispatcher.replay_available(
                            &scope,
                            &subscriptions,
                            event.sequence,
                            event.sequence,
                        );
                        controls.lag(event.sequence, event.sequence, 1, replay_available);
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                let first = Sequence(last_seen.saturating_add(1));
                let last = Sequence(last_seen.saturating_add(skipped));
                last_seen = last.0;
                let replay_available =
                    dispatcher.replay_available(&scope, &subscriptions, first, last);
                controls.lag(first, last, skipped, replay_available);
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

async fn write_control(
    writer: &mut (impl AsyncWrite + Unpin),
    control: &HostControl,
) -> Result<(), ()> {
    match control {
        HostControl::Shutdown(frame) => write_frame(writer, frame).await,
        HostControl::Reset(frame) => write_frame(writer, frame).await,
        HostControl::Lag(frame) => write_frame(writer, frame).await,
        HostControl::Ping(frame) => write_frame(writer, frame).await,
    }
}

fn redact_exact(mut input: Vec<u8>, values: &[SensitiveValue]) -> (Vec<u8>, u64) {
    let mut redactions = 0_u64;
    for value in values.iter().filter(|value| !value.0.is_empty()) {
        let mut cursor = 0;
        while cursor + value.0.len() <= input.len() {
            let Some(relative) = input[cursor..]
                .windows(value.0.len())
                .position(|window| window == value.0)
            else {
                break;
            };
            let start = cursor + relative;
            input.splice(start..start + value.0.len(), b"<redacted>".iter().copied());
            redactions = redactions.saturating_add(1);
            cursor = start + b"<redacted>".len();
        }
    }
    (input, redactions)
}

fn retain_diagnostic_line(state: &mut DiagnosticState, line: Vec<u8>, values: &[SensitiveValue]) {
    let (line, redactions) = redact_exact(line, values);
    state.stats.redactions = state.stats.redactions.saturating_add(redactions);
    let sanitized: Vec<u8> = String::from_utf8_lossy(&line)
        .bytes()
        .filter(|byte| !byte.is_ascii_control())
        .collect();
    while state.retained_bytes.saturating_add(sanitized.len()) > STDERR_RING_BYTES {
        let Some(evicted) = state.lines.pop_front() else {
            break;
        };
        state.retained_bytes = state.retained_bytes.saturating_sub(evicted.len());
        state.stats.discarded_bytes = state
            .stats
            .discarded_bytes
            .saturating_add(u64::try_from(evicted.len()).unwrap_or(u64::MAX));
    }
    if sanitized.len() > STDERR_RING_BYTES {
        state.stats.discarded_bytes = state
            .stats
            .discarded_bytes
            .saturating_add(u64::try_from(sanitized.len()).unwrap_or(u64::MAX));
        return;
    }
    state.retained_bytes = state.retained_bytes.saturating_add(sanitized.len());
    state.stats.retained_lines = state.stats.retained_lines.saturating_add(1);
    state.lines.push_back(sanitized);
}

async fn capture_stderr(
    mut stderr: ChildStderr,
    values: Vec<SensitiveValue>,
    shared: SharedDiagnostics,
) {
    let mut buffer = [0_u8; 4096];
    let mut line = Vec::with_capacity(STDERR_MAX_LINE);
    let mut dropping_line = false;
    let mut tokens_milli = u64::from(STDERR_BURST) * 1000;
    let mut replenished_at = tokio::time::Instant::now();
    loop {
        let read = match stderr.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        {
            let mut state = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.stats.input_bytes = state
                .stats
                .input_bytes
                .saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
        }
        for byte in &buffer[..read] {
            if *byte == b'\n' {
                let now = tokio::time::Instant::now();
                let elapsed_ms = u64::try_from(now.duration_since(replenished_at).as_millis())
                    .unwrap_or(u64::MAX);
                tokens_milli = tokens_milli
                    .saturating_add(elapsed_ms.saturating_mul(u64::from(STDERR_RATE_PER_SECOND)))
                    .min(u64::from(STDERR_BURST) * 1000);
                replenished_at = now;
                let mut state = shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if dropping_line {
                    state.stats.truncated_lines = state.stats.truncated_lines.saturating_add(1);
                    dropping_line = false;
                    line.clear();
                } else if tokens_milli >= 1000 {
                    tokens_milli -= 1000;
                    retain_diagnostic_line(&mut state, std::mem::take(&mut line), &values);
                } else {
                    state.stats.discarded_bytes = state
                        .stats
                        .discarded_bytes
                        .saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX));
                    line.clear();
                }
            } else if dropping_line {
                let mut state = shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.stats.discarded_bytes = state.stats.discarded_bytes.saturating_add(1);
            } else if line.len() == STDERR_MAX_LINE {
                let mut state = shared
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state.stats.discarded_bytes = state.stats.discarded_bytes.saturating_add(
                    u64::try_from(line.len())
                        .unwrap_or(u64::MAX)
                        .saturating_add(1),
                );
                dropping_line = true;
                line.clear();
            } else {
                line.push(*byte);
            }
        }
    }
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if dropping_line {
        state.stats.truncated_lines = state.stats.truncated_lines.saturating_add(1);
    } else if !line.is_empty() {
        retain_diagnostic_line(&mut state, line, &values);
    }
}

async fn finish_diagnostics(
    mut task: JoinHandle<()>,
    diagnostics: &SharedDiagnostics,
) -> DiagnosticStats {
    if tokio::time::timeout(DIAGNOSTIC_JOIN_TIMEOUT, &mut task)
        .await
        .is_err()
    {
        task.abort();
        let _ = task.await;
    }
    diagnostics
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .stats
        .clone()
}

struct AttemptResult {
    reason: RuntimeReason,
    retained: Option<CleanupAuthority>,
    healthy_for: Duration,
}

#[cfg(test)]
async fn run_service_a2b(
    activation: ServiceActivation,
    lifecycle: Arc<LifecycleDispatcher>,
    cancel: CancellationToken,
    status: RuntimeStatusCache,
) -> Option<CleanupAuthority> {
    let epoch = Uuid::new_v4();
    let replay_floor = lifecycle.current_sequence();
    let scope = activation_scope(&activation);
    run_service_with_epoch(
        activation,
        lifecycle,
        cancel,
        status,
        epoch,
        replay_floor,
        scope,
        Arc::new(std::sync::Mutex::new(ShutdownReason::DaemonStopping)),
        Arc::new(std::sync::Mutex::new(RestartHistory::default())),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_service_with_epoch(
    activation: ServiceActivation,
    lifecycle: Arc<LifecycleDispatcher>,
    cancel: CancellationToken,
    status: RuntimeStatusCache,
    epoch: Uuid,
    replay_floor: Sequence,
    scope: ActivationScope,
    stop_reason: Arc<std::sync::Mutex<ShutdownReason>>,
    history: Arc<std::sync::Mutex<RestartHistory>>,
) -> Option<CleanupAuthority> {
    loop {
        let (restart_count, circuit_open, blocked_reason) = {
            let history = history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (
                history.restart_count,
                history.circuit_open,
                history.blocked_reason,
            )
        };
        status.insert_starting(&activation, epoch, replay_floor, restart_count);
        if let Some(reason) = blocked_reason {
            status.update(
                &activation,
                if circuit_open {
                    RuntimeState::CircuitOpen
                } else {
                    RuntimeState::Unhealthy
                },
                None,
                None,
                Some(reason),
            );
            return None;
        }
        let attempt = run_service_once(
            &activation,
            Arc::clone(&lifecycle),
            epoch,
            replay_floor,
            scope.clone(),
            cancel.clone(),
            Arc::clone(&stop_reason),
            &status,
        )
        .await;
        if let Some(authority) = attempt.retained {
            return Some(authority);
        }
        if cancel.is_cancelled() || attempt.reason == RuntimeReason::Shutdown {
            return None;
        }
        if !activation.restart_on_failure {
            history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .blocked_reason = Some(attempt.reason);
            status.update(
                &activation,
                RuntimeState::Unhealthy,
                None,
                None,
                Some(attempt.reason),
            );
            return None;
        }

        let now = tokio::time::Instant::now();
        let (delay, circuit_open) = {
            let mut history = history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if attempt.healthy_for >= STABLE_RESET {
                history.failures.clear();
                history.backoff_index = 0;
                history.circuit_open = false;
                history.blocked_reason = None;
            }
            history.failures.push_back(now);
            while history
                .failures
                .front()
                .is_some_and(|failure| now.duration_since(*failure) > FAILURE_WINDOW)
            {
                history.failures.pop_front();
            }
            if history.failures.len() >= CIRCUIT_FAILURES {
                history.circuit_open = true;
                history.blocked_reason = Some(attempt.reason);
            }
            let delay = BACKOFF[history.backoff_index.min(BACKOFF.len() - 1)];
            history.backoff_index = history.backoff_index.saturating_add(1);
            history.restart_count = history.restart_count.saturating_add(1);
            (delay, history.circuit_open)
        };
        if circuit_open {
            status.update(
                &activation,
                RuntimeState::CircuitOpen,
                None,
                None,
                Some(attempt.reason),
            );
            return None;
        }
        status.update(
            &activation,
            RuntimeState::Backoff,
            None,
            None,
            Some(attempt.reason),
        );
        tokio::select! {
            _ = cancel.cancelled() => return None,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_service_once(
    activation: &ServiceActivation,
    lifecycle: Arc<LifecycleDispatcher>,
    epoch: Uuid,
    replay_floor: Sequence,
    scope: ActivationScope,
    cancel: CancellationToken,
    stop_reason: Arc<std::sync::Mutex<ShutdownReason>>,
    status: &RuntimeStatusCache,
) -> AttemptResult {
    let immediate = |reason| AttemptResult {
        reason,
        retained: None,
        healthy_for: Duration::ZERO,
    };
    if !matches!(activation.startup_timeout.as_millis(), 100..=30_000) {
        status.update(
            activation,
            RuntimeState::Unhealthy,
            None,
            None,
            Some(RuntimeReason::ProtocolViolation),
        );
        return immediate(RuntimeReason::ProtocolViolation);
    }
    if cancel.is_cancelled() {
        status.update(
            activation,
            RuntimeState::Inactive,
            None,
            None,
            Some(RuntimeReason::Shutdown),
        );
        return immediate(RuntimeReason::Shutdown);
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
                activation,
                RuntimeState::Unhealthy,
                None,
                None,
                Some(RuntimeReason::RootUnavailable),
            );
            return immediate(RuntimeReason::RootUnavailable);
        }
    };
    let environment = match resolve_environment(activation, |name| std::env::var_os(name)) {
        Ok(environment) => environment,
        Err(error) => {
            let reason = match error {
                EnvironmentError::MissingOrdinary => RuntimeReason::EnvironmentMissing,
                EnvironmentError::MissingSecret => RuntimeReason::SecretMissing,
                EnvironmentError::InvalidName => RuntimeReason::InvalidEnvironment,
            };
            status.update(
                activation,
                RuntimeState::Unhealthy,
                None,
                None,
                Some(reason),
            );
            cleanup_temp_root_accounted(&roots, status, activation);
            return immediate(reason);
        }
    };
    let mut redaction_values: Vec<SensitiveValue> = environment
        .iter()
        .map(|(_, value)| value.duplicate_for_redaction())
        .collect();
    redaction_values.extend(
        [
            OsStr::new("/usr/bin:/bin"),
            activation.package_path.as_os_str(),
            roots.data.as_os_str(),
            roots.cache.as_os_str(),
            roots.temp.as_os_str(),
        ]
        .into_iter()
        .map(|value| SensitiveValue(value.as_bytes().to_vec())),
    );
    let mut child = match spawn_service(activation, &roots, &environment) {
        Ok(child) => child,
        Err(_) => {
            status.update(
                activation,
                RuntimeState::Unhealthy,
                None,
                None,
                Some(RuntimeReason::SpawnFailed),
            );
            cleanup_temp_root_accounted(&roots, status, activation);
            return immediate(RuntimeReason::SpawnFailed);
        }
    };
    drop(environment);
    let Some(pid) = child.id() else {
        status.update(
            activation,
            RuntimeState::Unhealthy,
            None,
            None,
            Some(RuntimeReason::SpawnFailed),
        );
        cleanup_temp_root_accounted(&roots, status, activation);
        return immediate(RuntimeReason::SpawnFailed);
    };
    let mut owner = ProcessGroupOwner::new(pid);
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let cleaned = terminate_process_group(&mut child, &mut owner).await;
        if cleaned {
            cleanup_temp_root_accounted(&roots, status, activation);
        }
        let reason = if cleaned {
            RuntimeReason::SpawnFailed
        } else {
            RuntimeReason::CleanupFailed
        };
        status.update(
            activation,
            RuntimeState::Unhealthy,
            None,
            None,
            Some(reason),
        );
        return AttemptResult {
            reason,
            retained: (!cleaned).then_some(CleanupAuthority {
                child,
                owner,
                roots: Some(roots),
            }),
            healthy_for: Duration::ZERO,
        };
    };
    let diagnostics = Arc::new(std::sync::Mutex::new(DiagnosticState::default()));
    let diagnostics_task = tokio::spawn(capture_stderr(
        stderr,
        redaction_values,
        Arc::clone(&diagnostics),
    ));
    let mut stdin = stdin;
    let mut stdout = BufReader::new(stdout);

    let service_hello = {
        let handshake = tokio::time::timeout(
            activation.startup_timeout,
            perform_handshake_a2b(
                activation,
                lifecycle.daemon_boot_id(),
                connection_id,
                epoch,
                replay_floor,
                &mut stdin,
                &mut stdout,
            ),
        );
        tokio::pin!(handshake);
        tokio::select! {
            _ = cancel.cancelled() => None,
            result = &mut handshake => Some(result),
        }
    };
    let (shutdown_reason, runtime_reason, subscriptions, attach) = match service_hello {
        None => (
            requested_stop_reason(&stop_reason),
            RuntimeReason::Shutdown,
            None,
            None,
        ),
        Some(Err(_)) => (
            ShutdownReason::Unhealthy,
            RuntimeReason::StartupTimeout,
            None,
            None,
        ),
        Some(Ok(Err(_))) => (
            ShutdownReason::Unhealthy,
            RuntimeReason::ProtocolViolation,
            None,
            None,
        ),
        Some(Ok(Ok(service_hello))) => {
            let subscriptions = service_hello.subscriptions.clone();
            let attach = lifecycle.attach();
            let ready = Ready {
                protocol: ProtocolName,
                version: ProtocolV1,
                frame: ReadyFrame,
                subscriptions: subscriptions.clone(),
                replay: ReplayMode::BootLocal,
                activation_epoch: epoch,
                replay_floor,
            };
            let eligible = eligible_events(&attach, &scope, &subscriptions, replay_floor);
            let (reset, replay) = replay_plan(
                service_hello.resume.as_ref(),
                lifecycle.daemon_boot_id(),
                epoch,
                replay_floor,
                &eligible,
            );
            let attach_deadline = tokio::time::Instant::now() + activation.startup_timeout;
            let mut ledger = AckLedger::default();
            let mut child_status = None;
            let attach_result = async {
                write_attach_frame(
                    &mut stdin,
                    &mut stdout,
                    &ready,
                    None,
                    &mut ledger,
                    &mut child_status,
                    status,
                    activation,
                    &cancel,
                    attach_deadline,
                )
                .await?;
                if let Some(reset) = reset.as_ref() {
                    status.clear_ack(activation);
                    ledger = AckLedger::default();
                    write_attach_frame(
                        &mut stdin,
                        &mut stdout,
                        reset,
                        None,
                        &mut ledger,
                        &mut child_status,
                        status,
                        activation,
                        &cancel,
                        attach_deadline,
                    )
                    .await?;
                }
                for event in &replay {
                    write_attach_frame(
                        &mut stdin,
                        &mut stdout,
                        event,
                        Some(event.sequence),
                        &mut ledger,
                        &mut child_status,
                        status,
                        activation,
                        &cancel,
                        attach_deadline,
                    )
                    .await?;
                }
                Ok::<(), AttachError>(())
            }
            .await;
            match attach_result {
                Ok(()) => (
                    ShutdownReason::Unhealthy,
                    RuntimeReason::ChildUnknown,
                    Some((subscriptions, ledger, child_status)),
                    Some(attach),
                ),
                Err(AttachError::Cancelled) => (
                    requested_stop_reason(&stop_reason),
                    RuntimeReason::Shutdown,
                    None,
                    None,
                ),
                Err(AttachError::Timeout | AttachError::ProtocolViolation) => (
                    ShutdownReason::Unhealthy,
                    RuntimeReason::ProtocolViolation,
                    None,
                    None,
                ),
            }
        }
    };

    let started_healthy = tokio::time::Instant::now();
    let mut shutdown_already_sent = false;
    let runtime_reason = if let (
        Some((subscriptions, mut ack_ledger, child_status)),
        Some(attach),
    ) = (subscriptions, attach)
    {
        status.update(
            activation,
            RuntimeState::Healthy,
            Some(pid),
            Some(&subscriptions),
            None,
        );
        if let Some((state, reason)) = child_status {
            status.update(activation, state, Some(pid), None, reason);
        }
        let controls = Arc::new(ControlLane::default());
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let (data_tx, mut data_rx) = mpsc::channel(OUTBOUND_MAX_MESSAGES);
        let feeder_cancel = CancellationToken::new();
        let feeder = tokio::spawn(feed_live_events(
            Arc::clone(&lifecycle),
            attach.live,
            attach.boundary,
            scope,
            subscriptions,
            data_tx,
            Arc::clone(&queued_bytes),
            Arc::clone(&controls),
            feeder_cancel.clone(),
        ));
        let mut pending_ping = None::<(Uuid, tokio::time::Instant)>;
        let mut next_ping = tokio::time::Instant::now() + HEARTBEAT_INTERVAL;
        let mut misses = 0_u8;
        let mut leader_poll = tokio::time::interval(LEADER_POLL_INTERVAL);
        leader_poll.tick().await;

        let reason = loop {
            if cancel.is_cancelled() {
                controls.shutdown(requested_stop_reason(&stop_reason));
            }
            if let Some(control) = controls.pop() {
                let is_shutdown = matches!(control, HostControl::Shutdown(_));
                let ping_nonce = match &control {
                    HostControl::Ping(ping) => Some(ping.nonce),
                    _ => None,
                };
                if write_control(&mut stdin, &control).await.is_err() {
                    break RuntimeReason::ProtocolViolation;
                }
                if let Some(nonce) = ping_nonce {
                    // The child receives the full contracted timeout. Queueing
                    // or a slow-but-successful pipe write consumes none of it.
                    pending_ping = Some((nonce, pong_deadline_after_ping_write()));
                }
                if is_shutdown {
                    shutdown_already_sent = true;
                    break RuntimeReason::Shutdown;
                }
                continue;
            }

            let health_deadline =
                pending_ping.map_or(next_ping, |(_, deadline)| deadline.min(next_ping));
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    controls.shutdown(requested_stop_reason(&stop_reason));
                }
                _ = controls.notify.notified() => {}
                _ = tokio::time::sleep_until(health_deadline) => {
                    let now = tokio::time::Instant::now();
                    if pending_ping.is_some_and(|(_, deadline)| now >= deadline) {
                        pending_ping = None;
                        misses = misses.saturating_add(1);
                        if misses >= HEARTBEAT_MAX_MISSES {
                            break RuntimeReason::PingTimeout;
                        }
                    }
                    if pending_ping.is_none() && now >= next_ping {
                        controls.ping(Uuid::new_v4());
                        next_ping = now + HEARTBEAT_INTERVAL;
                    }
                }
                _ = leader_poll.tick() => {
                    if owner.leader_exited() {
                        break RuntimeReason::UnexpectedExit;
                    }
                }
                frame = read_frame::<ChildFrame, _>(&mut stdout) => {
                    match frame {
                        Ok(ChildFrame::Status(child_status)) => {
                            let state = match child_status.state {
                                ServiceStatusState::Ready => RuntimeState::Healthy,
                                ServiceStatusState::Degraded => RuntimeState::Degraded,
                            };
                            let reason = (state == RuntimeState::Degraded).then_some(match child_status.code {
                                ServiceStatusCode::ExternalUnavailable => RuntimeReason::ExternalUnavailable,
                                ServiceStatusCode::ConfigurationMissing => RuntimeReason::ConfigurationMissing,
                                ServiceStatusCode::RateLimited => RuntimeReason::RateLimited,
                                ServiceStatusCode::Unknown => RuntimeReason::ChildUnknown,
                            });
                            status.update(activation, state, Some(pid), None, reason);
                        }
                        Ok(ChildFrame::Ack(ack)) => {
                            if ack_ledger.acknowledge(ack.sequence).is_err() {
                                break RuntimeReason::ProtocolViolation;
                            }
                            status.update_ack(activation, ack.sequence);
                        }
                        Ok(ChildFrame::Pong(pong)) => {
                            if pending_ping.is_some_and(|(nonce, _)| nonce == pong.nonce) {
                                pending_ping = None;
                                misses = 0;
                            } else {
                                break RuntimeReason::ProtocolViolation;
                            }
                        }
                        Ok(ChildFrame::ShutdownComplete(_)) => break RuntimeReason::ProtocolViolation,
                        Err(error) => break post_ready_frame_failure_reason(error),
                    }
                }
                event = data_rx.recv() => {
                    let Some(event) = event else {
                        break RuntimeReason::UnexpectedExit;
                    };
                    queued_bytes.fetch_sub(event.encoded_bytes, Ordering::AcqRel);
                    if ack_ledger.is_full()
                        || write_frame(&mut stdin, &event.event).await.is_err()
                        || ack_ledger.record_sent(event.event.sequence).is_err()
                    {
                        // A child may choose not to ACK, but it cannot make
                        // daemon bookkeeping unbounded. Reconnect/reset starts
                        // a fresh connection ledger.
                        break RuntimeReason::ProtocolViolation;
                    }
                }
            }
        };
        feeder_cancel.cancel();
        let _ = feeder.await;
        status.add_lag(activation, controls.lag_total.swap(0, Ordering::AcqRel));
        reason
    } else {
        runtime_reason
    };

    let effective_shutdown_reason = if runtime_reason == RuntimeReason::Shutdown {
        requested_stop_reason(&stop_reason)
    } else {
        shutdown_reason
    };
    let cleaned = finish_process(
        activation,
        status,
        &mut child,
        &mut owner,
        stdin,
        stdout,
        &roots,
        effective_shutdown_reason,
        runtime_reason,
        shutdown_already_sent,
    )
    .await;
    let diagnostic_stats = finish_diagnostics(diagnostics_task, &diagnostics).await;
    status.update_diagnostics(activation, &diagnostic_stats);
    AttemptResult {
        reason: runtime_reason,
        retained: (!cleaned).then_some(CleanupAuthority {
            child,
            owner,
            roots: Some(roots),
        }),
        healthy_for: started_healthy.elapsed(),
    }
}

#[cfg(test)]
async fn run_service(
    activation: ServiceActivation,
    boot_id: Uuid,
    cancel: CancellationToken,
    status: RuntimeStatusCache,
) -> Option<CleanupAuthority> {
    let epoch = Uuid::new_v4();
    status.insert_starting(&activation, epoch, Sequence(0), 0);
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
        return (!cleaned).then_some(CleanupAuthority {
            child,
            owner,
            roots: Some(roots),
        });
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
            false,
        )
        .await;
        return (!cleaned).then_some(CleanupAuthority {
            child,
            owner,
            roots: Some(roots),
        });
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
                false,
            )
            .await;
            return (!cleaned).then_some(CleanupAuthority {
                child,
                owner,
                roots: Some(roots),
            });
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
                false,
            )
            .await;
            return (!cleaned).then_some(CleanupAuthority {
                child,
                owner,
                roots: Some(roots),
            });
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
        false,
    )
    .await;
    (!cleaned).then_some(CleanupAuthority {
        child,
        owner,
        roots: Some(roots),
    })
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
        // Stderr is diagnostics only and is consumed by the bounded structural
        // redactor; it is never interpreted as protocol or logged raw.
        .stderr(Stdio::piped());
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

async fn perform_handshake_a2b<W, R>(
    activation: &ServiceActivation,
    boot_id: Uuid,
    connection_id: Uuid,
    epoch: Uuid,
    replay_floor: Sequence,
    stdin: &mut W,
    stdout: &mut R,
) -> Result<ServiceHello, ()>
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
            replay_floor,
        },
        limits: ServiceLimits {
            max_frame_bytes: 65_536,
            outbound_messages: u64::try_from(OUTBOUND_MAX_MESSAGES).unwrap_or(u64::MAX),
            outbound_bytes: u64::try_from(OUTBOUND_MAX_BYTES).unwrap_or(u64::MAX),
            heartbeat_interval_ms: u64::try_from(HEARTBEAT_INTERVAL.as_millis())
                .unwrap_or(u64::MAX),
            heartbeat_timeout_ms: u64::try_from(HEARTBEAT_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
        },
    };
    write_frame(stdin, &hello).await?;
    let service_hello: ServiceHello = read_frame(stdout).await.map_err(|_| ())?;
    service_hello
        .validate_subscriptions(&activation.events)
        .map_err(|_| ())?;
    Ok(service_hello)
}

#[allow(dead_code)]
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
    shutdown_already_sent: bool,
) -> bool {
    let pid = child.id();
    status.update(
        activation,
        RuntimeState::Stopping,
        pid,
        None,
        Some(runtime_reason),
    );
    if !shutdown_already_sent {
        let shutdown = Shutdown {
            protocol: ProtocolName,
            version: ProtocolV1,
            frame: ShutdownFrame,
            reason,
        };
        let _ = write_frame(&mut stdin, &shutdown).await;
    }
    let response = async {
        loop {
            match read_frame::<ChildFrame, _>(&mut stdout)
                .await
                .map_err(|_| ())?
            {
                ChildFrame::ShutdownComplete(_) => return Ok::<(), ()>(()),
                ChildFrame::Status(_) => {}
                // A previously delivered event or ping can race the prioritized
                // shutdown write. Both frames are valid but irrelevant once
                // shutdown begins; continue waiting for shutdown_complete.
                ChildFrame::Ack(_) | ChildFrame::Pong(_) => {}
            }
        }
    };
    let _ = tokio::time::timeout(GRACEFUL_RESPONSE_TIMEOUT, response).await;
    drop(stdin);

    let group_cleaned = terminate_process_group(child, owner).await;
    let roots_cleaned = group_cleaned && cleanup_temp_root(roots);
    if group_cleaned && !roots_cleaned {
        status.record_temp_cleanup_failure(activation);
    }
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
        if matches!(
            reason,
            ShutdownReason::Disabled | ShutdownReason::DaemonStopping | ShutdownReason::Reconfigure
        ) && fully_cleaned
        {
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
    roots: Option<AssignedRoots>,
}

impl CleanupAuthority {
    async fn cleanup(&mut self) -> bool {
        if !self.owner.reaped && !terminate_process_group(&mut self.child, &mut self.owner).await {
            return false;
        }
        let roots_cleaned = self.roots.as_ref().is_none_or(cleanup_temp_root);
        if roots_cleaned {
            self.roots = None;
        }
        roots_cleaned
    }
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
    use crate::extension_lifecycle::LifecycleSource;
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
                startup_timeout: Duration::from_secs(10),
                restart_on_failure: false,
                effective_global: true,
                effective_projects: HashSet::new(),
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

    async fn wait_for_runtime_state(
        status: &RuntimeStatusCache,
        wanted: RuntimeState,
    ) -> RuntimeStatus {
        for _ in 0..1_000 {
            if let Some(found) = status
                .snapshot()
                .into_iter()
                .find(|entry| entry.state == wanted)
            {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("runtime state {wanted:?} was not observed")
    }

    #[test]
    fn prioritized_control_lane_coalesces_without_an_unbounded_control_queue() {
        let controls = ControlLane::default();
        controls.ping(Uuid::new_v4());
        controls.lag(Sequence(10), Sequence(12), 3, true);
        controls.lag(Sequence(8), Sequence(20), 5, false);
        controls.reset(reset_frame(ResetReason::InvalidCursor, &[]));
        assert!(matches!(controls.pop(), Some(HostControl::Reset(_))));
        assert!(matches!(controls.pop(), Some(HostControl::Ping(_))));
        assert!(controls.pop().is_none());

        controls.ping(Uuid::new_v4());
        controls.lag(Sequence(21), Sequence(21), 1, true);
        controls.shutdown(ShutdownReason::DaemonStopping);
        assert!(matches!(controls.pop(), Some(HostControl::Shutdown(_))));
        assert!(controls.pop().is_none());
        assert_eq!(controls.lag_total.load(Ordering::Relaxed), 9);
    }

    #[test]
    fn replay_floor_cursor_requires_an_actual_retained_epoch_eligible_event() {
        let boot = Uuid::new_v4();
        let epoch = Uuid::new_v4();
        let floor = Sequence(41);
        let (reset, replay) = replay_plan(
            Some(&ResumeCursor {
                daemon_boot_id: boot,
                activation_epoch: epoch,
                after_sequence: floor,
            }),
            boot,
            epoch,
            floor,
            &[],
        );
        assert!(matches!(
            reset.map(|frame| frame.reason),
            Some(ResetReason::RetentionExceeded | ResetReason::InvalidCursor)
        ));
        assert!(replay.is_empty());
    }

    #[test]
    fn ack_ledger_is_strictly_bounded_and_releases_only_delivered_order() {
        let mut ledger = AckLedger::default();
        for sequence in 1..=ACK_WINDOW_MAX {
            ledger.record_sent(Sequence(sequence as u64)).unwrap();
        }
        assert!(ledger.is_full());
        assert!(ledger
            .record_sent(Sequence(ACK_WINDOW_MAX as u64 + 1))
            .is_err());
        assert!(ledger.acknowledge(Sequence(17)).is_ok());
        assert!(!ledger.is_full());
        assert!(ledger.acknowledge(Sequence(17)).is_err());
        assert!(ledger.acknowledge(Sequence(999_999)).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn pong_deadline_starts_only_after_a_slow_successful_ping_write() {
        let (mut writer, mut reader) = tokio::io::duplex(1);
        let ping = HostControl::Ping(Ping {
            protocol: ProtocolName,
            version: ProtocolV1,
            frame: PingFrame,
            nonce: Uuid::new_v4(),
        });
        let started = tokio::time::Instant::now();
        let drain = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let mut bytes = Vec::new();
            loop {
                let mut byte = [0_u8; 1];
                if reader.read_exact(&mut byte).await.is_err() || byte[0] == b'\n' {
                    break;
                }
                bytes.push(byte[0]);
            }
            bytes
        });
        write_control(&mut writer, &ping).await.unwrap();
        let deadline = pong_deadline_after_ping_write();
        assert!(deadline.duration_since(started) >= Duration::from_secs(6));
        drop(writer);
        let _ = drain.await.unwrap();
    }

    #[test]
    fn activation_epoch_replay_rejects_widening_old_boot_and_ineligible_cursors() {
        let boot = Uuid::new_v4();
        let project_a = Uuid::new_v4();
        let project_b = Uuid::new_v4();
        let dispatcher = LifecycleDispatcher::new(boot, HashSet::from([project_a, project_b]));
        dispatcher.publish(LifecycleSource::DaemonStarted {
            daemon_version: "0.1.0".to_owned(),
            stamp: super::super::extension_lifecycle::event_stamp(),
        });
        let floor = dispatcher.current_sequence();
        for (project, session) in [(project_a, Uuid::new_v4()), (project_b, Uuid::new_v4())] {
            dispatcher.publish(LifecycleSource::ExplicitSessionCreated {
                succeeded: true,
                scope: dispatcher.source_scope(Some(project), Some(session), None, None, None),
                stamp: super::super::extension_lifecycle::event_stamp(),
                title: "forbidden-title".to_owned(),
                cwd: "/forbidden/path".to_owned(),
            });
        }
        let attach = dispatcher.attach();
        let scope_a = ActivationScope {
            global: false,
            projects: HashSet::from([project_a]),
        };
        let subscriptions = vec![
            LifecycleEventKind::DaemonStarted,
            LifecycleEventKind::SessionStarted,
        ];
        let eligible = eligible_events(&attach, &scope_a, &subscriptions, floor);
        assert_eq!(
            eligible
                .iter()
                .filter(|event| event.kind == LifecycleEventKind::SessionStarted)
                .count(),
            1
        );
        let epoch = Uuid::new_v4();
        let (reset, replay) = replay_plan(
            Some(&ResumeCursor {
                daemon_boot_id: boot,
                activation_epoch: epoch,
                after_sequence: floor,
            }),
            boot,
            epoch,
            floor,
            &eligible,
        );
        assert!(reset.is_none());
        assert_eq!(replay.len(), 1);

        let (reset, replay) = replay_plan(
            Some(&ResumeCursor {
                daemon_boot_id: Uuid::new_v4(),
                activation_epoch: epoch,
                after_sequence: floor,
            }),
            boot,
            epoch,
            floor,
            &eligible,
        );
        assert!(matches!(
            reset.map(|frame| frame.reason),
            Some(ResetReason::BootChanged)
        ));
        assert!(replay.is_empty());

        // Widening creates a floor at the current sequence. The older project-B
        // event is therefore ineligible even though the new scope contains B.
        let widened_floor = dispatcher.current_sequence();
        let widened = eligible_events(
            &dispatcher.attach(),
            &ActivationScope {
                global: false,
                projects: HashSet::from([project_a, project_b]),
            },
            &subscriptions,
            widened_floor,
        );
        assert!(widened
            .iter()
            .all(|event| event.kind == LifecycleEventKind::DaemonStarted));
    }

    #[tokio::test]
    async fn maximum_replay_concurrently_drains_an_ack_for_every_event() {
        let script = r#"#!/bin/sh
IFS= read -r hello || exit 10
printf ready > "$HOME/host-hello-read"
while [ ! -f "$HOME/release-hello" ]; do sleep 0.01; done
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":["session_started"],"resume":null}'
IFS= read -r ready || exit 11
while IFS= read -r frame; do
 case "$frame" in
  *'"frame":"event"'*) seq=${frame#*\"sequence\":\"}; seq=${seq%%\"*}; printf '{"protocol":"ocean.extension.service","version":1,"frame":"ack","sequence":"%s"}\n' "$seq" ;;
  *'"frame":"ping"'*) nonce=${frame#*\"nonce\":\"}; nonce=${nonce%%\"*}; printf '{"protocol":"ocean.extension.service","version":1,"frame":"pong","nonce":"%s"}\n' "$nonce" ;;
  *'"frame":"shutdown"'*) printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"shutdown_complete"}'; exit 0 ;;
 esac
done
"#;
        let (temp, mut activation) = executable_fixture(script);
        install_fixture_store(&temp, &mut activation);
        activation.events = vec![LifecycleEventKind::SessionStarted];
        activation.startup_timeout = Duration::from_secs(30);
        let marker = activation
            .config_dir
            .join("extensions/state/example.noop/data/host-hello-read");
        let release = activation
            .config_dir
            .join("extensions/state/example.noop/data/release-hello");
        let lifecycle = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
        let cancel = CancellationToken::new();
        let status = RuntimeStatusCache::default();
        let task = tokio::spawn(run_service_a2b(
            activation,
            Arc::clone(&lifecycle),
            cancel.clone(),
            status.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(20), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child did not enter delayed hello");
        for _ in 0..super::super::extension_lifecycle::BOOT_RING_MAX_EVENTS {
            lifecycle.publish(LifecycleSource::ExplicitSessionCreated {
                succeeded: true,
                scope: lifecycle.source_scope(None, Some(Uuid::new_v4()), None, None, None),
                stamp: super::super::extension_lifecycle::event_stamp(),
                title: "discarded".into(),
                cwd: "/discarded".into(),
            });
        }
        let expected = lifecycle.current_sequence();
        std::fs::write(release, b"release").unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if status.snapshot().first().is_some_and(|row| {
                    row.state == RuntimeState::Healthy
                        && row.last_acknowledged_sequence == Some(expected)
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("maximum replay did not drain ACK-every-event traffic");
        cancel.cancel();
        assert!(task.await.expect("service task").is_none());
    }

    #[tokio::test]
    async fn sustained_replay_without_acks_fails_at_the_bounded_ack_window() {
        let script = r#"#!/bin/sh
IFS= read -r hello || exit 10
printf ready > "$HOME/host-hello-read"
while [ ! -f "$HOME/release-hello" ]; do sleep 0.01; done
printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":["session_started"],"resume":null}'
IFS= read -r ready || exit 11
while IFS= read -r frame; do
 case "$frame" in
  *'"frame":"shutdown"'*) printf '%s\n' '{"protocol":"ocean.extension.service","version":1,"frame":"shutdown_complete"}'; exit 0 ;;
 esac
done
"#;
        let (temp, mut activation) = executable_fixture(script);
        install_fixture_store(&temp, &mut activation);
        activation.events = vec![LifecycleEventKind::SessionStarted];
        activation.startup_timeout = Duration::from_secs(15);
        let marker = activation
            .config_dir
            .join("extensions/state/example.noop/data/host-hello-read");
        let release = activation
            .config_dir
            .join("extensions/state/example.noop/data/release-hello");
        let lifecycle = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
        let status = RuntimeStatusCache::default();
        let task = tokio::spawn(run_service_a2b(
            activation,
            Arc::clone(&lifecycle),
            CancellationToken::new(),
            status.clone(),
        ));
        tokio::time::timeout(Duration::from_secs(20), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child did not enter delayed hello");
        for _ in 0..ACK_WINDOW_MAX + 32 {
            lifecycle.publish(LifecycleSource::ExplicitSessionCreated {
                succeeded: true,
                scope: lifecycle.source_scope(None, Some(Uuid::new_v4()), None, None, None),
                stamp: super::super::extension_lifecycle::event_stamp(),
                title: "discarded".into(),
                cwd: "/discarded".into(),
            });
        }
        std::fs::write(release, b"release").unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(25), task)
            .await
            .expect("bounded ACK policy did not terminate")
            .expect("service task")
            .is_none());
        assert_eq!(
            status.snapshot()[0].reason,
            Some(RuntimeReason::ProtocolViolation)
        );
    }

    #[tokio::test]
    async fn live_a2b_process_replays_acks_redacts_stderr_and_cleans_on_cancel() {
        const SENTINEL: &str = "stage-a2b-secret-sentinel";
        #[allow(clippy::useless_format)]
        let script = format!(
            "#!/bin/sh\nIFS= read -r hello\nprintf '%s\\n' '{{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[\"daemon_started\"],\"resume\":null}}'\nIFS= read -r ready\nprintf '%s\\n' \"$A2B_SENTINEL\" >&2\nwhile IFS= read -r frame; do\n case \"$frame\" in\n  *'\"frame\":\"event\"'*) seq=$(printf '%s' \"$frame\" | sed -n 's/.*\"sequence\":\"\\([^\"]*\\)\".*/\\1/p'); printf '{{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"ack\",\"sequence\":\"%s\"}}\\n' \"$seq\" ;;\n  *'\"frame\":\"ping\"'*) nonce=$(printf '%s' \"$frame\" | sed -n 's/.*\"nonce\":\"\\([^\"]*\\)\".*/\\1/p'); printf '{{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"pong\",\"nonce\":\"%s\"}}\\n' \"$nonce\" ;;\n  *'\"frame\":\"shutdown\"'*) printf '%s\\n' '{{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"shutdown_complete\"}}'; exit 0 ;;\n esac\ndone\n",
        );
        let (temp, mut activation) = executable_fixture(&script);
        install_fixture_store(&temp, &mut activation);
        activation.environment = vec!["A2B_SENTINEL".to_owned()];
        let previous = std::env::var_os("A2B_SENTINEL");
        std::env::set_var("A2B_SENTINEL", SENTINEL);

        let boot = Uuid::new_v4();
        let lifecycle = LifecycleDispatcher::new(boot, HashSet::new());
        lifecycle.publish(LifecycleSource::DaemonStarted {
            daemon_version: "0.1.0".to_owned(),
            stamp: super::super::extension_lifecycle::event_stamp(),
        });
        let cancel = CancellationToken::new();
        let status = RuntimeStatusCache::default();
        let task = tokio::spawn(run_service_a2b(
            activation,
            Arc::clone(&lifecycle),
            cancel.clone(),
            status.clone(),
        ));
        let healthy = wait_for_runtime_state(&status, RuntimeState::Healthy).await;
        assert!(healthy.pid.is_some());
        for _ in 0..100 {
            if status.snapshot()[0].last_acknowledged_sequence == Some(Sequence(1)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            status.snapshot()[0].last_acknowledged_sequence,
            Some(Sequence(1))
        );
        cancel.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(8), task)
            .await
            .expect("service cleanup timeout")
            .expect("service task")
            .is_none());
        let final_status = status.snapshot().pop().expect("status");
        assert_eq!(final_status.state, RuntimeState::Inactive);
        assert!(final_status.stderr_redactions >= 1);
        assert!(!serde_json::to_string(&final_status)
            .unwrap()
            .contains(SENTINEL));
        match previous {
            Some(value) => std::env::set_var("A2B_SENTINEL", value),
            None => std::env::remove_var("A2B_SENTINEL"),
        }
    }

    #[tokio::test]
    async fn three_missed_pongs_trigger_ping_timeout_and_full_cleanup() {
        let script = "#!/bin/sh\nIFS= read -r hello\nprintf '%s\\n' '{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null}'\nIFS= read -r ready\nwhile IFS= read -r frame; do\n case \"$frame\" in\n  *'\"frame\":\"shutdown\"'*) printf '%s\\n' '{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"shutdown_complete\"}'; exit 0 ;;\n esac\ndone\n";
        let (temp, mut activation) = executable_fixture(script);
        install_fixture_store(&temp, &mut activation);
        activation.events.clear();
        let lifecycle = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
        let status = RuntimeStatusCache::default();
        let task = tokio::spawn(run_service_a2b(
            activation,
            lifecycle,
            CancellationToken::new(),
            status.clone(),
        ));
        let healthy = wait_for_runtime_state(&status, RuntimeState::Healthy).await;
        assert_eq!(healthy.state, RuntimeState::Healthy);
        // Freeze time only after the real child has completed startup. Pausing
        // from test entry lets Tokio auto-advance the startup deadline while an
        // OS process is merely waiting for scheduler time under workspace load.
        tokio::time::pause();
        for delta in [10_u64, 5, 5, 5, 5, 5] {
            tokio::time::advance(Duration::from_secs(delta)).await;
            tokio::task::yield_now().await;
        }
        // The child is a real OS process while Tokio's clock is paused. Keep
        // advancing bounded cleanup timers and yielding real scheduler time so
        // the shell can consume shutdown and close its pipes; a paused Tokio
        // timeout alone can otherwise remain pending on external process I/O.
        for _ in 0..500 {
            if task.is_finished() {
                break;
            }
            tokio::time::advance(Duration::from_millis(100)).await;
            std::thread::sleep(Duration::from_millis(1));
            tokio::task::yield_now().await;
        }
        assert!(task.is_finished(), "health cleanup did not finish");
        assert!(task.await.expect("service task").is_none());
        let final_status = status.snapshot().pop().expect("status");
        assert_eq!(final_status.reason, Some(RuntimeReason::PingTimeout));
        assert_eq!(final_status.pid, None);
    }

    #[tokio::test]
    async fn stderr_binary_newline_free_and_rate_flood_stay_bounded_and_redacted() {
        const SENTINEL: &str = "stderr-secret-sentinel";
        let script = "#!/bin/sh\nIFS= read -r hello\nprintf '%s\\n' '{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null}'\nIFS= read -r ready\nhead -c 10000 /dev/zero >&2\ni=0; while [ $i -lt 100 ]; do printf '%s\\n' \"$STDERR_SENTINEL\" >&2; i=$((i+1)); done\nprintf done > \"$HOME/stderr-done\"\nwhile IFS= read -r frame; do\n case \"$frame\" in\n  *'\"frame\":\"ping\"'*) nonce=$(printf '%s' \"$frame\" | sed -n 's/.*\"nonce\":\"\\([^\"]*\\)\".*/\\1/p'); printf '{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"pong\",\"nonce\":\"%s\"}\\n' \"$nonce\" ;;\n  *'\"frame\":\"shutdown\"'*) printf '%s\\n' '{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"shutdown_complete\"}'; exit 0 ;;\n esac\ndone\n";
        let (temp, mut activation) = executable_fixture(script);
        install_fixture_store(&temp, &mut activation);
        activation.events.clear();
        activation.environment = vec!["STDERR_SENTINEL".to_owned()];
        let stderr_done = activation
            .config_dir
            .join("extensions/state/example.noop/data/stderr-done");
        let previous = std::env::var_os("STDERR_SENTINEL");
        std::env::set_var("STDERR_SENTINEL", SENTINEL);
        let lifecycle = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
        let cancel = CancellationToken::new();
        let status = RuntimeStatusCache::default();
        let task = tokio::spawn(run_service_a2b(
            activation,
            lifecycle,
            cancel.clone(),
            status.clone(),
        ));
        let _ = wait_for_runtime_state(&status, RuntimeState::Healthy).await;
        tokio::time::timeout(Duration::from_secs(15), async {
            while !stderr_done.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("stderr fixture did not finish under load");
        cancel.cancel();
        assert!(task.await.expect("service task").is_none());
        let final_status = status.snapshot().pop().expect("status");
        assert!(final_status.stderr_bytes >= 10_000);
        assert!(final_status.stderr_truncated_lines >= 1);
        assert!(final_status.stderr_discarded_bytes > 0);
        assert!(final_status.stderr_lines <= u64::from(STDERR_BURST));
        assert!(final_status.stderr_redactions > 0);
        assert!(!serde_json::to_string(&final_status)
            .expect("status json")
            .contains(SENTINEL));
        match previous {
            Some(value) => std::env::set_var("STDERR_SENTINEL", value),
            None => std::env::remove_var("STDERR_SENTINEL"),
        }
    }

    #[tokio::test]
    async fn slow_reader_hits_bounded_data_queue_and_records_coalesced_lag() {
        let script = "#!/bin/sh\nIFS= read -r hello\nprintf '%s\\n' '{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[\"session_started\"],\"resume\":null}'\nIFS= read -r ready\nsleep 10\n";
        let (temp, mut activation) = executable_fixture(script);
        install_fixture_store(&temp, &mut activation);
        activation.events = vec![LifecycleEventKind::SessionStarted];
        let lifecycle = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
        let status = RuntimeStatusCache::default();
        let task = tokio::spawn(run_service_a2b(
            activation,
            Arc::clone(&lifecycle),
            CancellationToken::new(),
            status.clone(),
        ));
        let _ = wait_for_runtime_state(&status, RuntimeState::Healthy).await;
        for _ in 0..1_000 {
            lifecycle.publish(LifecycleSource::ExplicitSessionCreated {
                succeeded: true,
                scope: lifecycle.source_scope(None, Some(Uuid::new_v4()), None, None, None),
                stamp: super::super::extension_lifecycle::event_stamp(),
                title: "never serialized".to_owned(),
                cwd: "/never/serialized".to_owned(),
            });
        }
        assert!(tokio::time::timeout(Duration::from_secs(8), task)
            .await
            .expect("blocked writer did not fail within its deadline")
            .expect("service task")
            .is_none());
        let final_status = status.snapshot().pop().expect("status");
        assert!(final_status.lag_count > 0);
        assert_eq!(final_status.reason, Some(RuntimeReason::ProtocolViolation));
    }

    #[test]
    fn restart_backoff_schedule_and_circuit_threshold_match_the_ratified_policy() {
        assert_eq!(
            BACKOFF,
            [
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
                Duration::from_secs(16),
                Duration::from_secs(30),
            ]
        );
        assert_eq!(FAILURE_WINDOW, Duration::from_secs(60));
        assert_eq!(CIRCUIT_FAILURES, 5);
        assert_eq!(STABLE_RESET, Duration::from_secs(5 * 60));
    }

    #[tokio::test]
    async fn on_failure_crash_loop_opens_circuit_after_exact_threshold() {
        let script = "#!/bin/sh\nIFS= read -r hello\nprintf '%s\\n' '{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null}'\nIFS= read -r ready\nexit 17\n";
        let (temp, mut activation) = executable_fixture(script);
        install_fixture_store(&temp, &mut activation);
        activation.events.clear();
        activation.restart_on_failure = true;
        let lifecycle = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
        let status = RuntimeStatusCache::default();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            run_service_a2b(
                activation,
                lifecycle,
                CancellationToken::new(),
                status.clone(),
            ),
        )
        .await
        .expect("circuit did not open");
        assert!(result.is_none());
        let circuit = wait_for_runtime_state(&status, RuntimeState::CircuitOpen).await;
        assert_eq!(circuit.restart_count, 4);
        assert_eq!(circuit.reason, Some(RuntimeReason::UnexpectedExit));
    }

    #[tokio::test]
    async fn scope_only_epoch_change_preserves_open_circuit_and_does_not_respawn() {
        let (temp, mut activation) =
            executable_fixture("#!/bin/sh\nprintf spawned > \"$HOME/should-not-exist\"\n");
        install_fixture_store(&temp, &mut activation);
        activation.restart_on_failure = true;
        activation.effective_global = false;
        activation.effective_projects = HashSet::from([Uuid::new_v4()]);
        let marker = activation
            .config_dir
            .join("extensions/state/example.noop/data/should-not-exist");
        let history = Arc::new(std::sync::Mutex::new(RestartHistory {
            circuit_open: true,
            blocked_reason: Some(RuntimeReason::UnexpectedExit),
            ..RestartHistory::default()
        }));
        let lifecycle = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
        let status = RuntimeStatusCache::default();
        let result = run_service_with_epoch(
            activation,
            lifecycle,
            CancellationToken::new(),
            status.clone(),
            Uuid::new_v4(),
            Sequence(99),
            ActivationScope {
                global: false,
                projects: HashSet::from([Uuid::new_v4()]),
            },
            Arc::new(std::sync::Mutex::new(ShutdownReason::Reconfigure)),
            history,
        )
        .await;
        assert!(result.is_none());
        assert!(!marker.exists());
        assert_eq!(status.snapshot()[0].state, RuntimeState::CircuitOpen);
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
    fn runtime_status_retains_ack_only_for_process_restart_in_the_same_epoch() {
        let (_temp, activation) = executable_fixture("#!/bin/sh\nexit 0\n");
        let cache = RuntimeStatusCache::default();
        let first_epoch = Uuid::new_v4();
        cache.insert_starting(&activation, first_epoch, Sequence(7), 0);
        cache.update_ack(&activation, Sequence(11));
        cache.add_lag(&activation, 3);

        cache.insert_starting(&activation, first_epoch, Sequence(7), 1);
        let same_epoch = cache.snapshot().pop().expect("same epoch status");
        assert_eq!(same_epoch.last_acknowledged_sequence, Some(Sequence(11)));
        assert_eq!(same_epoch.lag_count, 3);

        cache.insert_starting(&activation, Uuid::new_v4(), Sequence(20), 0);
        let new_epoch = cache.snapshot().pop().expect("new epoch status");
        assert_eq!(new_epoch.last_acknowledged_sequence, None);
        assert_eq!(new_epoch.lag_count, 0);
        assert_eq!(new_epoch.replay_floor, Sequence(20));
    }

    #[test]
    fn runtime_status_cache_prunes_removed_service_keys() {
        let (_temp_a, activation_a) = executable_fixture("#!/bin/sh\nexit 0\n");
        let (_temp_b, mut activation_b) = executable_fixture("#!/bin/sh\nexit 0\n");
        activation_b.package_id = "example.other".into();
        let cache = RuntimeStatusCache::default();
        cache.insert_starting(&activation_a, Uuid::new_v4(), Sequence(0), 0);
        cache.insert_starting(&activation_b, Uuid::new_v4(), Sequence(0), 0);
        cache.retain_only(&HashSet::from([RuntimeStatusCache::key(&activation_a)]));
        let rows = cache.snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].package_id, activation_a.package_id);
    }

    #[test]
    fn runtime_status_has_no_argv_environment_secret_or_diagnostic_text_fields() {
        let (_temp, activation) = executable_fixture("#!/bin/sh\nexit 0\n");
        let cache = RuntimeStatusCache::default();
        cache.insert_starting(&activation, Uuid::nil(), Sequence(0), 0);
        cache.record_temp_cleanup_failure(&activation);
        let encoded = serde_json::to_string(&cache.snapshot()).unwrap();
        for forbidden in ["args", "environment", "secret-sentinel", "diagnostic_text"] {
            assert!(!encoded.contains(forbidden));
        }
        assert!(encoded.contains("stderr_bytes"));
        assert!(encoded.contains("\"temp_cleanup_failures\":1"));
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
        tokio::time::timeout(Duration::from_secs(15), async {
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
        activation.startup_timeout = Duration::from_secs(10);
        let config = activation.config_dir.clone();
        let status = RuntimeStatusCache::default();
        let retained = tokio::time::timeout(
            Duration::from_secs(20),
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
        tokio::time::timeout(Duration::from_secs(20), async {
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

        // Repeat under the same multi-threaded process load used by the
        // workspace gate so EOF and the leader poll may race without relying on
        // a narrow cleanup wall clock.
        for _ in 0..16 {
            let (temp, mut activation) = executable_fixture(script);
            install_fixture_store(&temp, &mut activation);
            activation.events.clear();
            let status = RuntimeStatusCache::default();
            let retained = tokio::time::timeout(
                Duration::from_secs(15),
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
        let gone = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                // SAFETY: signal 0 performs an existence check only.
                if unsafe { libc::kill(grandchild, 0) } != 0 {
                    break;
                }
                // A killed descendant may remain briefly visible as a
                // reparented zombie while the OS reaper is under parallel load.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .is_ok();
        assert!(gone, "surviving grandchild was not cleaned");
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
        let mut authority = CleanupAuthority {
            child,
            owner,
            roots: None,
        };
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

        // Model a surviving descendant that inherited stderr: diagnostics must
        // be aborted/bounded before retained process authority is retried.
        let diagnostics = Arc::new(std::sync::Mutex::new(DiagnosticState::default()));
        let diagnostics_task = tokio::spawn(std::future::pending::<()>());
        let diagnostics_started = tokio::time::Instant::now();
        let _ = finish_diagnostics(diagnostics_task, &diagnostics).await;
        assert!(diagnostics_started.elapsed() < Duration::from_secs(1));

        assert!(authority.cleanup().await);
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
