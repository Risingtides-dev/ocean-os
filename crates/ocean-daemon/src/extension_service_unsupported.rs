//! Stage A2a fail-closed supervisor projection for unsupported platforms.
//!
//! Exact effective native service grants are read coherently and cached as
//! `unsupported_platform`. No package artifact, secret, root, or process is
//! opened by this boundary, and cache reads never probe or start anything.

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

    #[test]
    fn exact_grant_projects_to_non_probing_unsupported_platform_status() {
        let cache = RuntimeStatusCache::default();
        cache.insert_unsupported(UnsupportedServiceActivation {
            package_id: "example.noop".to_owned(),
            package_version: "1.0.0".to_owned(),
            package_digest: format!("sha256:{}", "a".repeat(64)),
            service_id: "lifecycle".to_owned(),
            activation_revision: 9,
        });
        let snapshot = cache.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].state, RuntimeState::UnsupportedPlatform);
        assert_eq!(snapshot[0].reason, Some(RuntimeReason::UnsupportedPlatform));
        assert_eq!(snapshot[0].pid, None);
        assert_eq!(snapshot[0].started_at, None);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert!(encoded.contains("unsupported_platform"));
        assert!(!encoded.contains("args"));
        assert!(!encoded.contains("secret"));
    }
}
