//! Stage A2a fail-closed supervisor projection for unsupported platforms.
//!
//! Exact effective native service grants are read through the sole coherent
//! descriptor-safe registry/artifact validator and cached as
//! `unsupported_platform`. Projection opens no secret, assigned root, or
//! process, and cache reads never probe or start anything.

#![cfg_attr(test, allow(dead_code))]

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::{Arc, RwLock},
};

use chrono::{SecondsFormat, Utc};
use ocean_agent_sdk::extension_lifecycle::Sequence;
use serde::Serialize;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::extension_registry::{
    read_unsupported_service_activations, UnsupportedServiceActivation,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeState {
    UnsupportedPlatform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuntimeReason {
    UnsupportedPlatform,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ServiceKey {
    package_id: String,
    service_id: String,
}

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
    negotiated_subscriptions: Vec<String>,
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
        self.inner
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .cloned()
            .collect()
    }

    fn insert_unsupported(&self, activation: UnsupportedServiceActivation) {
        let key = ServiceKey {
            package_id: activation.package_id.clone(),
            service_id: activation.service_id.clone(),
        };
        self.inner
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(
                key,
                RuntimeStatus {
                    package_id: activation.package_id,
                    package_version: activation.package_version,
                    package_digest: activation.package_digest,
                    service_id: activation.service_id,
                    activation_revision: activation.activation_revision,
                    activation_epoch: Uuid::new_v4(),
                    replay_floor: Sequence(0),
                    state: RuntimeState::UnsupportedPlatform,
                    pid: None,
                    started_at: None,
                    observed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
                    restart_count: 0,
                    negotiated_subscriptions: Vec::new(),
                    last_acknowledged_sequence: None,
                    lag_count: 0,
                    reason: Some(RuntimeReason::UnsupportedPlatform),
                },
            );
    }
}

pub(crate) struct ExtensionSupervisor {
    status: RuntimeStatusCache,
    root_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ExtensionSupervisor {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
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
        let status = self.status.clone();
        let task = tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                read_unsupported_service_activations(&config_dir, &registered_projects)
            })
            .await;
            match result {
                Ok(Ok(activations)) => {
                    for activation in activations {
                        status.insert_unsupported(activation);
                    }
                }
                Ok(Err(error)) => tracing::warn!(
                    reason = error.code(),
                    "unsupported extension startup reconciliation blocked"
                ),
                Err(_) => tracing::warn!(
                    reason = "registry_reader_failed",
                    "unsupported extension startup reconciliation blocked"
                ),
            }
        });
        *self.root_task.lock().await = Some(task);
    }

    pub(crate) async fn shutdown(&self) {
        if let Some(task) = self.root_task.lock().await.take() {
            let _ = task.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension_registry::{
        read_unsupported_service_activations, test_snapshot_package_digest, StateError,
    };
    use serde_json::{json, Value};
    use std::{fs, path::Path};

    const ID: &str = "example.unsupported";

    #[derive(Clone, Copy)]
    enum FixtureKind {
        Valid,
        Malformed,
        MissingService,
        UnauthorizedBinding,
    }

    struct Fixture {
        config: tempfile::TempDir,
        marker: PathBuf,
    }

    fn write_json(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn fixture(kind: FixtureKind) -> Fixture {
        let config = tempfile::tempdir().unwrap();
        let root = config.path().join("extensions");
        let staging = root.join("store").join(ID).join("staging");
        fs::create_dir_all(staging.join("services")).unwrap();
        let marker = config.path().join("CHILD_STARTED");
        let capabilities = if matches!(kind, FixtureKind::UnauthorizedBinding) {
            "[services.capabilities]\nenv = [\"TARGET_TOKEN\"]\nsecrets = [\"env:SOURCE_TOKEN\"]\n"
        } else {
            ""
        };
        fs::write(
            staging.join("ocean-extension.toml"),
            format!(
                "schema_version = 1\nid = \"{ID}\"\nname = \"Unsupported\"\nversion = \"1.0.0\"\nmin_ocean_version = \"0.1.0\"\n\n[[services]]\nid = \"lifecycle\"\nentry = \"services/lifecycle\"\nevents = []\n{capabilities}"
            ),
        )
        .unwrap();
        fs::write(
            staging.join("services/lifecycle"),
            format!("#!/bin/sh\nprintf started > '{}'\n", marker.display()),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(
                staging.join("services/lifecycle"),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
        }
        let digest = test_snapshot_package_digest(&staging).unwrap();
        fs::rename(
            &staging,
            staging
                .parent()
                .unwrap()
                .join(digest.strip_prefix("sha256:").unwrap()),
        )
        .unwrap();
        fs::write(root.join(".state.lock"), "").unwrap();
        #[cfg(windows)]
        let source_locator = r"C:\unsupported";
        #[cfg(not(windows))]
        let source_locator = "/tmp/unsupported";
        write_json(
            &root.join("installs.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 9,
                "installs": [{
                    "id": ID,
                    "version": "1.0.0",
                    "digest": digest,
                    "source": {"kind": "local-path", "locator": source_locator}
                }]
            }),
        );
        write_json(
            &root.join("trust.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 9,
                "grants": [{
                    "id": ID,
                    "digest": digest,
                    "capabilities": if matches!(kind, FixtureKind::UnauthorizedBinding) {
                        json!({"env": ["TARGET_TOKEN"], "secrets": []})
                    } else {
                        json!({})
                    }
                }]
            }),
        );
        write_json(
            &root.join("enabled.json"),
            &json!({
                "schema_version": 1,
                "state_revision": 9,
                "extensions": [{"id": ID, "global": true, "projects": []}]
            }),
        );
        if matches!(kind, FixtureKind::Malformed) {
            fs::write(root.join("service-grants.json"), b"{not-json").unwrap();
        } else {
            let service_id = if matches!(kind, FixtureKind::MissingService) {
                "missing"
            } else {
                "lifecycle"
            };
            let bindings = if matches!(kind, FixtureKind::UnauthorizedBinding) {
                json!([{"target_env": "TARGET_TOKEN", "reference": "env:SOURCE_TOKEN"}])
            } else {
                json!([])
            };
            write_json(
                &root.join("service-grants.json"),
                &json!({
                    "schema_version": 1,
                    "state_revision": 9,
                    "service_grants": [{
                        "id": ID,
                        "digest": digest,
                        "service_id": service_id,
                        "native_process_ack": true,
                        "secret_bindings": bindings
                    }]
                }),
            );
        }
        Fixture { config, marker }
    }

    async fn start_and_snapshot(fixture: &Fixture) -> Vec<RuntimeStatus> {
        let supervisor = ExtensionSupervisor::new();
        supervisor
            .start(fixture.config.path().to_path_buf(), HashSet::new())
            .await;
        supervisor.shutdown().await;
        supervisor.status_cache().snapshot()
    }

    #[tokio::test]
    async fn real_startup_projects_only_common_validated_state_and_never_starts_a_child() {
        let valid = fixture(FixtureKind::Valid);
        let snapshot = start_and_snapshot(&valid).await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].state, RuntimeState::UnsupportedPlatform);
        assert_eq!(snapshot[0].reason, Some(RuntimeReason::UnsupportedPlatform));
        assert_eq!(snapshot[0].pid, None);
        assert_eq!(snapshot[0].started_at, None);
        assert!(!valid.marker.exists());
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(encoded.contains("unsupported_platform"));
        assert!(!encoded.contains("args"));
        assert!(!encoded.contains("secret"));

        let production = include_str!("extension_service_unsupported.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for child_api in ["std::process", "tokio::process", "Command::new"] {
            assert!(
                !production.contains(child_api),
                "unsupported supervisor gained child API {child_api}"
            );
        }
    }

    #[tokio::test]
    async fn real_startup_rejects_malformed_nonexistent_service_and_unauthorized_binding() {
        for (kind, expected) in [
            (
                FixtureKind::Malformed,
                StateError::Parse("service-grants.json"),
            ),
            (
                FixtureKind::MissingService,
                StateError::InvalidRecord("service-grant-service"),
            ),
            (
                FixtureKind::UnauthorizedBinding,
                StateError::InvalidRecord("secret-binding-authority"),
            ),
        ] {
            let fixture = fixture(kind);
            assert_eq!(
                read_unsupported_service_activations(fixture.config.path(), &HashSet::new())
                    .unwrap_err(),
                expected
            );
            assert!(start_and_snapshot(&fixture).await.is_empty());
            assert!(!fixture.marker.exists());
        }
    }
}
