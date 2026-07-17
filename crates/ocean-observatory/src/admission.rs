use crate::{BindingRegistry, BindingToken, ObservatoryStore, Snapshot, StoreError};
use std::collections::{HashMap, HashSet};
use thiserror::Error;
use uuid::Uuid;

pub const DEFAULT_MAX_EXECUTION_DEPTH: usize = 32;
pub const MAX_SAFE_LABELS: usize = 32;
pub const MAX_SAFE_LABEL_BYTES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmissionRequest {
    pub parent_execution_id: String,
    pub root_execution_id: String,
    pub labels: Vec<String>,
    pub producer_id: String,
    pub lease_duration_seconds: u32,
    pub idempotency_key: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct AdmissionResult {
    pub execution_id: String,
    pub root_execution_id: String,
    pub edge_id: String,
    pub binding_token: BindingToken,
    pub token_ttl_seconds: u32,
}

impl std::fmt::Debug for AdmissionResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdmissionResult")
            .field("execution_id", &self.execution_id)
            .field("root_execution_id", &self.root_execution_id)
            .field("edge_id", &self.edge_id)
            .field("binding_token", &"[REDACTED]")
            .field("token_ttl_seconds", &self.token_ttl_seconds)
            .finish()
    }
}

#[derive(Debug, Clone)]
struct AdmissionIdentity {
    execution_id: String,
    root_execution_id: String,
    edge_id: String,
    parent_execution_id: String,
    producer_id: String,
}

/// Volatile admission identities. Binding credentials live separately in `BindingRegistry`.
#[derive(Debug, Default)]
pub struct AdmissionRegistry {
    by_idempotency_key: HashMap<String, AdmissionIdentity>,
}

#[derive(Debug, Clone, Copy)]
pub struct AdmissionConfig {
    pub max_execution_depth: usize,
}

impl Default for AdmissionConfig {
    fn default() -> Self {
        Self {
            max_execution_depth: DEFAULT_MAX_EXECUTION_DEPTH,
        }
    }
}

#[derive(Debug, Error)]
pub enum AdmissionError {
    #[error("parent execution was not found")]
    InvalidParent,
    #[error("parent execution is not admitted or running")]
    InvalidParentPhase,
    #[error("admission would create a topology cycle")]
    CycleDetected,
    #[error("execution tree depth limit exceeded")]
    DepthExceeded,
    #[error("parent belongs to another root authority")]
    CrossAuthority,
    #[error("idempotency key was reused with different admission parameters")]
    IdempotencyConflict,
    #[error("producer id, idempotency key, or labels are not safe metadata")]
    UnsafeMetadata,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Validates and records topology intent only. It never starts work or grants capability.
pub fn validate_admission(
    request: &AdmissionRequest,
    store: &ObservatoryStore,
    admissions: &mut AdmissionRegistry,
    bindings: &mut BindingRegistry,
    config: AdmissionConfig,
) -> Result<AdmissionResult, AdmissionError> {
    validate_safe_metadata(request)?;
    let snapshot = store.snapshot_at(None)?;
    validate_against_snapshot(request, &snapshot, config.max_execution_depth)?;

    if let Some(previous) = admissions.by_idempotency_key.get(&request.idempotency_key) {
        if previous.parent_execution_id != request.parent_execution_id
            || previous.root_execution_id != request.root_execution_id
            || previous.producer_id != request.producer_id
        {
            return Err(AdmissionError::IdempotencyConflict);
        }
        return Ok(result_with_fresh_binding(previous, bindings));
    }

    let identity = AdmissionIdentity {
        execution_id: Uuid::new_v4().to_string(),
        root_execution_id: request.root_execution_id.clone(),
        edge_id: Uuid::new_v4().to_string(),
        parent_execution_id: request.parent_execution_id.clone(),
        producer_id: request.producer_id.clone(),
    };
    let result = result_with_fresh_binding(&identity, bindings);
    admissions
        .by_idempotency_key
        .insert(request.idempotency_key.clone(), identity);
    Ok(result)
}

fn result_with_fresh_binding(
    identity: &AdmissionIdentity,
    bindings: &mut BindingRegistry,
) -> AdmissionResult {
    AdmissionResult {
        execution_id: identity.execution_id.clone(),
        root_execution_id: identity.root_execution_id.clone(),
        edge_id: identity.edge_id.clone(),
        binding_token: bindings.issue(identity.execution_id.clone()),
        token_ttl_seconds: 30,
    }
}

fn validate_safe_metadata(request: &AdmissionRequest) -> Result<(), AdmissionError> {
    let identifier_is_safe = |value: &str| {
        !value.is_empty()
            && value.len() <= 128
            && value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
            })
    };
    let labels_are_safe = request.labels.len() <= MAX_SAFE_LABELS
        && request.labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= MAX_SAFE_LABEL_BYTES
                && label.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
                })
        });
    if identifier_is_safe(&request.producer_id)
        && identifier_is_safe(&request.idempotency_key)
        && labels_are_safe
    {
        Ok(())
    } else {
        Err(AdmissionError::UnsafeMetadata)
    }
}

fn validate_against_snapshot(
    request: &AdmissionRequest,
    snapshot: &Snapshot,
    max_depth: usize,
) -> Result<(), AdmissionError> {
    let parent = snapshot
        .nodes
        .iter()
        .find(|node| node.execution_id == request.parent_execution_id)
        .ok_or(AdmissionError::InvalidParent)?;
    if !matches!(parent.phase.as_str(), "admitted" | "running") {
        return Err(AdmissionError::InvalidParentPhase);
    }
    if parent.root_execution_id != request.root_execution_id {
        return Err(AdmissionError::CrossAuthority);
    }
    let depth = topology_depth(snapshot, &parent.execution_id)?;
    if depth >= max_depth {
        return Err(AdmissionError::DepthExceeded);
    }
    Ok(())
}

/// Validates an asserted edge independently so extension-attested A→B→A cycles are rejected.
pub fn validate_topology_edge(
    snapshot: &Snapshot,
    child_execution_id: &str,
    parent_execution_id: &str,
    max_depth: usize,
) -> Result<(), AdmissionError> {
    if child_execution_id == parent_execution_id {
        return Err(AdmissionError::CycleDetected);
    }
    let mut current = Some(parent_execution_id);
    let mut visited = HashSet::new();
    let mut depth = 0_usize;
    while let Some(execution_id) = current {
        if execution_id == child_execution_id || !visited.insert(execution_id) {
            return Err(AdmissionError::CycleDetected);
        }
        depth = depth.saturating_add(1);
        // `depth` counts the proposed child's ancestors; admitting the child at
        // this point would make the tree one level deeper.
        if depth >= max_depth {
            return Err(AdmissionError::DepthExceeded);
        }
        current = snapshot
            .nodes
            .iter()
            .find(|node| node.execution_id == execution_id)
            .and_then(|node| node.parent_execution_id.as_deref());
    }
    Ok(())
}

fn topology_depth(snapshot: &Snapshot, execution_id: &str) -> Result<usize, AdmissionError> {
    let mut current = Some(execution_id);
    let mut visited = HashSet::new();
    let mut depth = 0_usize;
    while let Some(id) = current {
        if !visited.insert(id) {
            return Err(AdmissionError::CycleDetected);
        }
        depth = depth.saturating_add(1);
        current = snapshot
            .nodes
            .iter()
            .find(|node| node.execution_id == id)
            .and_then(|node| node.parent_execution_id.as_deref());
    }
    Ok(depth)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cursor, ExecutionNode};

    fn node(id: &str, root: &str, parent: Option<&str>) -> ExecutionNode {
        ExecutionNode {
            execution_id: id.into(),
            root_execution_id: root.into(),
            parent_execution_id: parent.map(str::to_owned),
            phase: "running".into(),
            created_at: String::new(),
        }
    }

    fn snapshot(nodes: Vec<ExecutionNode>) -> Snapshot {
        Snapshot {
            watermark_cursor: Cursor::new(0),
            earliest_available_cursor: Cursor::new(0),
            nodes,
            edges: Vec::new(),
        }
    }

    #[test]
    fn rejects_a_to_b_to_a_cycle() {
        let state = snapshot(vec![node("a", "a", None), node("b", "a", Some("a"))]);
        assert!(matches!(
            validate_topology_edge(&state, "a", "b", 32),
            Err(AdmissionError::CycleDetected)
        ));
    }

    #[test]
    fn rejects_depth_at_limit() {
        let nodes = (0..32)
            .map(|index| {
                let id = format!("n{index}");
                let parent = (index > 0).then(|| format!("n{}", index - 1));
                ExecutionNode {
                    execution_id: id,
                    root_execution_id: "n0".into(),
                    parent_execution_id: parent,
                    phase: "running".into(),
                    created_at: String::new(),
                }
            })
            .collect();
        assert!(matches!(
            validate_topology_edge(&snapshot(nodes), "new", "n31", 32),
            Err(AdmissionError::DepthExceeded)
        ));
    }

    #[test]
    fn rejects_cross_authority_parent() {
        let request = AdmissionRequest {
            parent_execution_id: "parent".into(),
            root_execution_id: "other-root".into(),
            labels: vec!["safe".into()],
            producer_id: "extension:test".into(),
            lease_duration_seconds: 30,
            idempotency_key: "request-1".into(),
        };
        let state = snapshot(vec![node("parent", "actual-root", None)]);
        assert!(matches!(
            validate_against_snapshot(&request, &state, 32),
            Err(AdmissionError::CrossAuthority)
        ));
    }

    #[test]
    fn duplicate_idempotency_returns_same_ids_and_fresh_token() {
        let mut admissions = AdmissionRegistry::default();
        let mut bindings = BindingRegistry::new();
        let identity = AdmissionIdentity {
            execution_id: "execution".into(),
            root_execution_id: "root".into(),
            edge_id: "edge".into(),
            parent_execution_id: "parent".into(),
            producer_id: "producer".into(),
        };
        admissions
            .by_idempotency_key
            .insert("key".into(), identity.clone());
        let first = result_with_fresh_binding(&identity, &mut bindings);
        let second = result_with_fresh_binding(&identity, &mut bindings);
        assert_eq!(first.execution_id, second.execution_id);
        assert_eq!(first.edge_id, second.edge_id);
        assert_ne!(first.binding_token, second.binding_token);
        assert_eq!(bindings.len(), 2);
    }
}
