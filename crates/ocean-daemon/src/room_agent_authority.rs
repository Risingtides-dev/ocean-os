//! Rooms Phase 1 local room-agent authorization and admission boundary.
//!
//! Participant rows and federated descriptors remain display/compatibility
//! data. Every executable room-agent turn passes through [`admit_room_agent`]
//! before transcript, attachment, request, or runtime work.

use std::collections::{BTreeSet, HashMap};

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::Utc;
use ocean_core::{
    FederatedActorType, FederatedRoomRole, RequestId, RoomAccessState, RoomKey,
    RoomParticipantKind, RoomTriggerEvent,
};
use ocean_store::{
    ActivationPolicy, AgentBindingStatus, AuthorizeAgentInput, ContextPolicy, MemoryScope,
    RoomAgentAdmissionAuditInput, RoomAgentBinding, RoomStore, RoomStoreError,
    SetAgentBindingStatusInput,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    persistent_rooms::{publish_room_wake, with_rooms},
    AppState,
};
use crate::request_control::{cancel_permission_waiter, RequestControl};
use crate::room_operator::{OperatorAuthError, OperatorPrincipal};

const DEFINITION_DIGEST_DOMAIN: &[u8] = b"ocean-room-agent-definition-v1\0";
const DECISION_DIGEST_DOMAIN: &[u8] = b"ocean-room-agent-decision-v1\0";

// Phase 1 has no resource-grant/sandbox boundary. Every current ambient tool
// class can escape the room memory boundary directly or indirectly (filesystem,
// process, loopback HTTP, browser/MCP/plugin). The only truthful local node
// policy ceiling is therefore empty: authorized room agents are conversational
// until Stage 2 supplies confined resources. This is an additional intersection
// term, never a widening substitute.
const PHASE1_SAFE_CAPABILITIES: &[&str] = &[];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AuthorizeAgentBody {
    agent_member_id: String,
    agent_package_id: String,
    owner_member_id: String,
    decision_id: String,
    #[serde(default = "default_activation_policy")]
    activation_policy: String,
    #[serde(default = "default_context_policy")]
    context_policy: String,
    #[serde(default = "default_memory_scope")]
    memory_scope: String,
    #[serde(default)]
    room_capability_grants: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReauthorizeAgentBody {
    decision_id: String,
    #[serde(default = "default_activation_policy")]
    activation_policy: String,
    #[serde(default = "default_context_policy")]
    context_policy: String,
    #[serde(default = "default_memory_scope")]
    memory_scope: String,
    #[serde(default)]
    room_capability_grants: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StatusDecisionBody {
    decision_id: String,
}

fn default_activation_policy() -> String {
    "explicit_only".into()
}

fn default_context_policy() -> String {
    "invocation_only".into()
}

fn default_memory_scope() -> String {
    "none".into()
}

#[derive(Debug, Clone)]
pub(super) struct ResolvedPackage {
    pub(super) package_id: String,
    pub(super) display_name: String,
    pub(super) definition_digest: String,
    pub(super) definition_revision: Option<String>,
    pub(super) requested_capabilities: Vec<String>,
    pub(super) instructions_layer: Option<String>,
    pub(super) tool_allowlist: Vec<String>,
    pub(super) model: Option<String>,
    pub(super) root: std::path::PathBuf,
    pub(super) subprocess_capabilities: Vec<ocean_agent::agentdir::SubprocessCapability>,
}

#[derive(Debug, Clone)]
pub(super) struct RoomAgentAdmission {
    pub(super) admission_id: String,
    pub(super) room: RoomKey,
    pub(super) agent_member_id: String,
    pub(super) package: ResolvedPackage,
    pub(super) generation: u64,
    pub(super) decision_id: String,
    pub(super) context_policy: ContextPolicy,
    pub(super) effective_capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum AdmissionTrigger {
    Explicit,
    Mention,
    ThreadReply,
    Unknown,
}

impl AdmissionTrigger {
    pub(super) fn from_room_event(event: &RoomTriggerEvent) -> Self {
        match event {
            RoomTriggerEvent::Mention { .. } => Self::Mention,
            RoomTriggerEvent::ThreadReply { .. } => Self::ThreadReply,
            RoomTriggerEvent::BuildFailed
            | RoomTriggerEvent::CiFailure
            | RoomTriggerEvent::ComponentEvent { .. }
            | RoomTriggerEvent::Schedule => Self::Unknown,
        }
    }

    fn permits(self, policy: ActivationPolicy) -> bool {
        matches!(
            (self, policy),
            (Self::Explicit, _)
                | (
                    Self::Mention,
                    ActivationPolicy::Mention | ActivationPolicy::TaskAndThread
                )
                | (Self::ThreadReply, ActivationPolicy::TaskAndThread)
        )
    }
}

#[derive(Serialize)]
struct AuthorityDecisionDigestInput<'a> {
    room_id: &'a str,
    agent_member_id: &'a str,
    agent_package_id: &'a str,
    agent_definition_digest: &'a str,
    activation_policy: &'a str,
    context_policy: &'a str,
    memory_scope: &'a str,
    room_capability_grants: &'a [String],
}

#[derive(Serialize)]
struct StatusDecisionDigestInput<'a> {
    room_id: &'a str,
    agent_member_id: &'a str,
    target_status: &'a str,
}

pub(super) fn resolve_package(package_id: &str) -> Result<ResolvedPackage, ApiError> {
    let package_id = package_id.trim();
    if package_id.is_empty() {
        return Err(ApiError::bad_request("invalid_agent_package_id"));
    }
    let definition =
        ocean_agent::agentdir::resolve(&super::agents_root(), package_id).map_err(|error| {
            match error {
                ocean_agent::agentdir::ResolveError::InvalidName(_) => {
                    ApiError::bad_request("invalid_agent_package_id")
                }
                ocean_agent::agentdir::ResolveError::NotFound(_) => {
                    ApiError::not_found("agent_package_not_found")
                }
                ocean_agent::agentdir::ResolveError::Config(_, _)
                | ocean_agent::agentdir::ResolveError::Io(_, _) => {
                    ApiError::conflict("agent_package_unavailable")
                }
            }
        })?;

    let mut tools = definition.effective_tools();
    canonicalize(&mut tools)?;
    let mut declared = definition.config.capabilities.clone();
    canonicalize(&mut declared)?;
    let definition_digest = digest_package_tree(&definition.root)?;

    // Empty legacy allowlists request nothing in a room. Room authority never
    // interprets an omitted declaration as the process-wide registry.
    let mut requested = BTreeSet::new();
    requested.extend(tools.iter().cloned());
    requested.extend(declared);
    requested.extend(
        definition
            .config
            .subprocess_capabilities
            .iter()
            .map(|capability| format!("subprocess:{}", capability.effective_name())),
    );

    let instructions_layer = definition.system_prompt().map(str::to_owned);
    Ok(ResolvedPackage {
        package_id: definition.name.clone(),
        display_name: definition.name,
        definition_digest,
        definition_revision: None,
        requested_capabilities: requested.into_iter().collect(),
        instructions_layer,
        tool_allowlist: tools,
        model: definition.config.model,
        root: definition.root,
        subprocess_capabilities: definition.config.subprocess_capabilities,
    })
}

/// Digest every steering/executable byte in a package under deterministic
/// relative paths. This includes skill bodies, tool-file contents, and nested
/// subagent definitions rather than trusting the top-level TOML declarations
/// to describe all code that can run. Absolute root metadata is never hashed.
fn digest_package_tree(root: &std::path::Path) -> Result<String, ApiError> {
    fn collect(
        root: &std::path::Path,
        dir: &std::path::Path,
        files: &mut Vec<(String, Vec<u8>)>,
    ) -> Result<(), ApiError> {
        let mut entries = std::fs::read_dir(dir)
            .map_err(|_| ApiError::conflict("agent_package_unavailable"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ApiError::conflict("agent_package_unavailable"))?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .map_err(|_| ApiError::conflict("agent_package_unavailable"))?;
            if metadata.file_type().is_symlink() {
                return Err(ApiError::conflict("agent_package_symlink_refused"));
            }
            if metadata.is_dir() {
                collect(root, &path, files)?;
            } else if metadata.is_file() {
                let relative = path
                    .strip_prefix(root)
                    .map_err(|_| ApiError::internal("definition_digest_failed"))?;
                let relative = relative
                    .to_str()
                    .ok_or_else(|| ApiError::conflict("agent_package_path_unavailable"))?
                    .replace(std::path::MAIN_SEPARATOR, "/");
                let bytes = std::fs::read(&path)
                    .map_err(|_| ApiError::conflict("agent_package_unavailable"))?;
                files.push((relative, bytes));
            } else {
                return Err(ApiError::conflict("agent_package_entry_refused"));
            }
        }
        Ok(())
    }

    let mut files = Vec::new();
    collect(root, root, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    digest.update(DEFINITION_DIGEST_DOMAIN);
    for (relative, bytes) in files {
        digest.update((relative.len() as u64).to_be_bytes());
        digest.update(relative.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn canonicalize(values: &mut Vec<String>) -> Result<(), ApiError> {
    if values.iter().any(|value| {
        let trimmed = value.trim();
        trimmed.is_empty() || trimmed.len() > 256 || trimmed != value
    }) {
        return Err(ApiError::bad_request("invalid_capability"));
    }
    values.sort();
    values.dedup();
    Ok(())
}

fn validate_capability_grants(
    package: &ResolvedPackage,
    grants: &[String],
) -> Result<(), ApiError> {
    for grant in grants {
        if !package.requested_capabilities.contains(grant) {
            return Err(ApiError::bad_request("capability_not_requested"));
        }
        if !PHASE1_SAFE_CAPABILITIES.contains(&grant.as_str()) {
            return Err(ApiError::conflict("capability_not_grantable"));
        }
    }
    Ok(())
}

fn parse_activation(raw: &str) -> Result<ActivationPolicy, ApiError> {
    match raw {
        "explicit_only" => Ok(ActivationPolicy::ExplicitOnly),
        "mention" => Ok(ActivationPolicy::Mention),
        "task_and_thread" => Ok(ActivationPolicy::TaskAndThread),
        _ => Err(ApiError::bad_request("invalid_activation_policy")),
    }
}

fn parse_context(raw: &str) -> Result<ContextPolicy, ApiError> {
    match raw {
        "invocation_only" => Ok(ContextPolicy::InvocationOnly),
        "room_recent" => Ok(ContextPolicy::RoomRecent),
        "room_history" => Ok(ContextPolicy::RoomHistory),
        _ => Err(ApiError::bad_request("invalid_context_policy")),
    }
}

fn parse_memory(raw: &str) -> Result<MemoryScope, ApiError> {
    match raw {
        "none" => Ok(MemoryScope::None),
        "room" => Ok(MemoryScope::Room),
        _ => Err(ApiError::bad_request("invalid_memory_scope")),
    }
}

fn validate_decision_id(raw: &str) -> Result<String, ApiError> {
    let parsed =
        Uuid::parse_str(raw.trim()).map_err(|_| ApiError::bad_request("invalid_decision_id"))?;
    if parsed.is_nil() {
        return Err(ApiError::bad_request("invalid_decision_id"));
    }
    Ok(parsed.to_string())
}

fn decision_digest(input: &impl Serialize) -> Result<String, ApiError> {
    let bytes =
        serde_json::to_vec(input).map_err(|_| ApiError::internal("decision_digest_failed"))?;
    let mut digest = Sha256::new();
    digest.update(DECISION_DIGEST_DOMAIN);
    digest.update(bytes);
    Ok(format!("sha256:{:x}", digest.finalize()))
}

fn operator(state: &AppState, headers: &HeaderMap) -> Result<OperatorPrincipal, ApiError> {
    state
        .room_operator
        .authorize(headers)
        .map_err(ApiError::from)
}

#[derive(Debug)]
struct TargetProof {
    agent_member_id: String,
    owner_member_id: String,
    owner_eligible: bool,
}

/// Resolve package identity to the room-scoped member without accepting a
/// browser-nominated label as authority.
fn target_proof(
    store: &mut ocean_store::SqliteRoomStore,
    room: &RoomKey,
    package_id: &str,
) -> Result<Option<TargetProof>, RoomStoreError> {
    let record = store
        .get(room)?
        .ok_or_else(|| RoomStoreError::UnknownRoom(room.clone()))?;
    let access = store.room_access(room)?;
    if access.state == RoomAccessState::Local {
        let target = record.room.participants.iter().find(|participant| {
            participant.id == package_id && participant.kind == RoomParticipantKind::Agent
        });
        let Some(target) = target else {
            return Ok(None);
        };
        let owner = store
            .agent_owners(room)?
            .into_iter()
            .find(|(agent, _, _)| agent == &target.id);
        return Ok(
            owner.map(|(_, owner_member_id, owner_present)| TargetProof {
                agent_member_id: target.id.clone(),
                owner_member_id,
                owner_eligible: owner_present,
            }),
        );
    }

    let Some(agent_member_id) = store.resolve_room_agent_member(room, package_id)? else {
        return Ok(None);
    };
    let Some(member) = access.members.iter().find(|member| {
        member.member_id == agent_member_id && member.actor_type == FederatedActorType::Agent
    }) else {
        return Ok(None);
    };
    let Some(owner_member_id) = member.owner_member_id.clone() else {
        return Ok(None);
    };
    let owner_eligible = store.room_credential(room)?.is_some_and(|credential| {
        credential.local_human_member_id == owner_member_id
            && access.members.iter().any(|candidate| {
                candidate.member_id == owner_member_id
                    && candidate.actor_type == FederatedActorType::User
                    && candidate.role_in_room == FederatedRoomRole::Owner
            })
    });
    Ok(Some(TargetProof {
        agent_member_id,
        owner_member_id,
        owner_eligible,
    }))
}

fn prove_owner_and_target(
    store: &mut ocean_store::SqliteRoomStore,
    room: &RoomKey,
    owner_member_id: &str,
    agent_member_id: &str,
    package_id: &str,
) -> Result<bool, RoomStoreError> {
    Ok(target_proof(store, room, package_id)?.is_some_and(|proof| {
        proof.agent_member_id == agent_member_id
            && proof.owner_member_id == owner_member_id
            && proof.owner_eligible
    }))
}

fn binding_owner_eligible(
    store: &mut ocean_store::SqliteRoomStore,
    room: &RoomKey,
    binding: &RoomAgentBinding,
) -> Result<bool, RoomStoreError> {
    prove_owner_and_target(
        store,
        room,
        &binding.owner_member_id,
        &binding.agent_member_id,
        &binding.agent_package_id,
    )
}

fn binding_projection(binding: &RoomAgentBinding) -> Value {
    let operator_intersection = binding.effective_capabilities();
    let effective_capabilities = operator_intersection
        .iter()
        .filter(|capability| PHASE1_SAFE_CAPABILITIES.contains(&capability.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    json!({
        "room_id": binding.room_id,
        "agent_member_id": binding.agent_member_id,
        "agent_package_id": binding.agent_package_id,
        "agent_definition_digest": binding.agent_definition_digest,
        "agent_definition_revision": binding.agent_definition_revision,
        "display_name": binding.display_name,
        "owner_member_id": binding.owner_member_id,
        "authorized_at": binding.authorized_at,
        "activation_policy": binding.activation_policy.as_str(),
        "context_policy": binding.context_policy.as_str(),
        "memory_scope": binding.memory_scope.as_str(),
        "requested_capabilities": binding.requested_capabilities,
        "room_capability_grants": binding.room_capability_grants,
        "operator_intersection_capabilities": operator_intersection,
        "effective_capabilities": effective_capabilities,
        "status": binding.status.as_str(),
        "generation": binding.generation.to_string(),
        "revoked_at": binding.revoked_at,
    })
}

pub(super) async fn room_agent_preview(
    State(state): State<AppState>,
    Path((key, package_id)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    let room = RoomKey::new(key.trim());
    let result = (|| {
        let package = resolve_package(&package_id)?;
        let (proof, binding) = with_rooms(&state, |store| -> Result<_, ApiError> {
            let proof = target_proof(store, &room, &package.package_id).map_err(ApiError::from)?;
            let binding = match proof.as_ref() {
                Some(proof) => store
                    .room_agent_binding(&room, &proof.agent_member_id)
                    .map_err(ApiError::from)?,
                None => None,
            };
            Ok((proof, binding))
        })?;
        Ok((
            StatusCode::OK,
            json!({
                "ok": true,
                "package_id": package.package_id,
                "display_name": package.display_name,
                "definition_digest": package.definition_digest,
                "definition_revision": package.definition_revision,
                "requested_capabilities": package.requested_capabilities.clone(),
                "grantable_capabilities": PHASE1_SAFE_CAPABILITIES,
                "unavailable_capabilities": package.requested_capabilities.iter().map(|capability| {
                    json!({
                        "capability": capability,
                        "reason": "phase1_resource_confinement_unavailable",
                    })
                }).collect::<Vec<_>>(),
                "agent_member_id": proof.as_ref().map(|value| value.agent_member_id.clone()),
                "owner_member_id": proof.as_ref().map(|value| value.owner_member_id.clone()),
                "owner_eligible": proof.as_ref().is_some_and(|value| value.owner_eligible),
                "binding": binding.as_ref().map(binding_projection),
            }),
        ))
    })();
    into_response(result)
}

pub(super) async fn room_agent_bindings(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> (StatusCode, Json<Value>) {
    let room = RoomKey::new(key.trim());
    match with_rooms(&state, |store| {
        if store.get(&room)?.is_none() {
            return Err(RoomStoreError::UnknownRoom(room.clone()));
        }
        store
            .room_agent_bindings(&room)?
            .into_iter()
            .map(|binding| {
                let owner_eligible = binding_owner_eligible(store, &room, &binding)?;
                Ok((binding, owner_eligible))
            })
            .collect::<Result<Vec<_>, RoomStoreError>>()
    }) {
        Ok(bindings) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "owner_eligible": !bindings.is_empty() && bindings.iter().all(|(_, eligible)| *eligible),
                "bindings": bindings.iter().map(|(binding, owner_eligible)| {
                    let mut projection = binding_projection(binding);
                    projection["owner_eligible"] = json!(owner_eligible);
                    projection
                }).collect::<Vec<_>>()
            })),
        ),
        Err(error) => ApiError::from(error).response(),
    }
}

pub(super) async fn room_agent_binding(
    State(state): State<AppState>,
    Path((key, agent_member_id)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    let room = RoomKey::new(key.trim());
    match with_rooms(&state, |store| {
        if store.get(&room)?.is_none() {
            return Err(RoomStoreError::UnknownRoom(room.clone()));
        }
        let binding = store.room_agent_binding(&room, agent_member_id.trim())?;
        binding
            .map(|binding| {
                let owner_eligible = binding_owner_eligible(store, &room, &binding)?;
                Ok((binding, owner_eligible))
            })
            .transpose()
    }) {
        Ok(Some((binding, owner_eligible))) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "owner_member_id": binding.owner_member_id,
                "owner_eligible": owner_eligible,
                "binding": binding_projection(&binding),
            })),
        ),
        Ok(None) => ApiError::not_found("agent_binding_not_found").response(),
        Err(error) => ApiError::from(error).response(),
    }
}

pub(super) async fn room_agent_authorize(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
    body: Result<Json<AuthorizeAgentBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    let requests = state.requests.write().await;
    let result = (|| {
        let principal = operator(&state, &headers)?;
        let Json(mut body) = body.map_err(|_| ApiError::bad_request("invalid_request"))?;
        let room = RoomKey::new(key.trim());
        let agent_member_id = body.agent_member_id.trim().to_string();
        let owner_member_id = body.owner_member_id.trim().to_string();
        if room.as_str().is_empty() || agent_member_id.is_empty() || owner_member_id.is_empty() {
            return Err(ApiError::bad_request("invalid_request"));
        }
        let decision_id = validate_decision_id(&body.decision_id)?;
        canonicalize(&mut body.room_capability_grants)?;
        let package = resolve_package(&body.agent_package_id)?;
        validate_capability_grants(&package, &body.room_capability_grants)?;
        let activation = parse_activation(&body.activation_policy)?;
        let context = parse_context(&body.context_policy)?;
        let memory = parse_memory(&body.memory_scope)?;
        let digest = decision_digest(&AuthorityDecisionDigestInput {
            room_id: room.as_str(),
            agent_member_id: &agent_member_id,
            agent_package_id: &package.package_id,
            agent_definition_digest: &package.definition_digest,
            activation_policy: activation.as_str(),
            context_policy: context.as_str(),
            memory_scope: memory.as_str(),
            room_capability_grants: &body.room_capability_grants,
        })?;
        let (binding, created, audit) = with_rooms(&state, |store| -> Result<_, ApiError> {
            let existing = store
                .room_agent_binding(&room, &agent_member_id)
                .map_err(ApiError::from)?;
            let consumed = store
                .room_agent_decision(&room, &decision_id)
                .map_err(ApiError::from)?;
            if existing.is_some() && consumed.is_none() {
                return Err(ApiError::conflict("agent_binding_exists"));
            }
            if consumed.is_none()
                && !prove_owner_and_target(
                    store,
                    &room,
                    &owner_member_id,
                    &agent_member_id,
                    &package.package_id,
                )
                .map_err(ApiError::from)?
            {
                return Err(ApiError::forbidden("room_owner_required"));
            }
            store
                .authorize_room_agent(
                    &room,
                    AuthorizeAgentInput {
                        agent_member_id,
                        agent_package_id: package.package_id,
                        agent_definition_digest: package.definition_digest,
                        agent_definition_revision: package.definition_revision,
                        display_name: package.display_name,
                        owner_member_id,
                        authorized_by: principal.id().to_string(),
                        activation_policy: activation,
                        context_policy: context,
                        memory_scope: memory,
                        requested_capabilities: package.requested_capabilities,
                        room_capability_grants: body.room_capability_grants,
                        decision_id,
                        request_digest: digest,
                    },
                    Utc::now(),
                )
                .map_err(ApiError::from)
        })?;
        if let Some(audit) = audit.as_ref() {
            publish_room_wake(&state, &room, audit);
        }
        Ok((
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            json!({"ok": true, "created": created, "binding": binding_projection(&binding)}),
            room,
            binding.agent_member_id,
            binding.generation,
        ))
    })();
    finish_mutation_with_cancellation(&state, requests, result).await
}

pub(super) async fn room_agent_reauthorize(
    State(state): State<AppState>,
    Path((key, agent_member_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<ReauthorizeAgentBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    let requests = state.requests.write().await;
    let result = (|| {
        let principal = operator(&state, &headers)?;
        let Json(mut body) = body.map_err(|_| ApiError::bad_request("invalid_request"))?;
        let room = RoomKey::new(key.trim());
        let agent_member_id = agent_member_id.trim().to_string();
        let current = with_rooms(&state, |store| {
            store.room_agent_binding(&room, &agent_member_id)
        })
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("agent_binding_not_found"))?;
        let decision_id = validate_decision_id(&body.decision_id)?;
        canonicalize(&mut body.room_capability_grants)?;
        let package = resolve_package(&current.agent_package_id)?;
        validate_capability_grants(&package, &body.room_capability_grants)?;
        let activation = parse_activation(&body.activation_policy)?;
        let context = parse_context(&body.context_policy)?;
        let memory = parse_memory(&body.memory_scope)?;
        let previous_definition_digest = current.agent_definition_digest.clone();
        let digest = decision_digest(&AuthorityDecisionDigestInput {
            room_id: room.as_str(),
            agent_member_id: &agent_member_id,
            agent_package_id: &package.package_id,
            agent_definition_digest: &package.definition_digest,
            activation_policy: activation.as_str(),
            context_policy: context.as_str(),
            memory_scope: memory.as_str(),
            room_capability_grants: &body.room_capability_grants,
        })?;
        let owner_member_id = current.owner_member_id.clone();
        let (binding, applied, audit) = with_rooms(&state, |store| -> Result<_, ApiError> {
            let consumed = store
                .room_agent_decision(&room, &decision_id)
                .map_err(ApiError::from)?;
            if consumed.is_none()
                && !prove_owner_and_target(
                    store,
                    &room,
                    &owner_member_id,
                    &agent_member_id,
                    &package.package_id,
                )
                .map_err(ApiError::from)?
            {
                return Err(ApiError::forbidden("room_owner_required"));
            }
            store
                .authorize_room_agent(
                    &room,
                    AuthorizeAgentInput {
                        agent_member_id,
                        agent_package_id: package.package_id,
                        agent_definition_digest: package.definition_digest,
                        agent_definition_revision: package.definition_revision,
                        display_name: package.display_name,
                        owner_member_id,
                        authorized_by: principal.id().to_string(),
                        activation_policy: activation,
                        context_policy: context,
                        memory_scope: memory,
                        requested_capabilities: package.requested_capabilities,
                        room_capability_grants: body.room_capability_grants,
                        decision_id,
                        request_digest: digest,
                    },
                    Utc::now(),
                )
                .map_err(ApiError::from)
        })?;
        if let Some(audit) = audit.as_ref() {
            publish_room_wake(&state, &room, audit);
        }
        Ok((
            StatusCode::OK,
            json!({
                "ok": true,
                "applied": applied,
                "previous_definition_digest": previous_definition_digest,
                "definition_changed": previous_definition_digest != binding.agent_definition_digest,
                "binding": binding_projection(&binding),
            }),
            room,
            binding.agent_member_id,
            binding.generation,
        ))
    })();
    finish_mutation_with_cancellation(&state, requests, result).await
}

pub(super) async fn room_agent_suspend(
    State(state): State<AppState>,
    Path(path): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<StatusDecisionBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    status_mutation(state, path, headers, body, AgentBindingStatus::Suspended).await
}

pub(super) async fn room_agent_resume(
    State(state): State<AppState>,
    Path(path): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<StatusDecisionBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    status_mutation(state, path, headers, body, AgentBindingStatus::Active).await
}

pub(super) async fn room_agent_revoke(
    State(state): State<AppState>,
    Path(path): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<StatusDecisionBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    status_mutation(state, path, headers, body, AgentBindingStatus::Revoked).await
}

async fn status_mutation(
    state: AppState,
    (key, agent_member_id): (String, String),
    headers: HeaderMap,
    body: Result<Json<StatusDecisionBody>, JsonRejection>,
    target: AgentBindingStatus,
) -> (StatusCode, Json<Value>) {
    let requests = state.requests.write().await;
    let result = (|| {
        let principal = operator(&state, &headers)?;
        let Json(body) = body.map_err(|_| ApiError::bad_request("invalid_request"))?;
        let room = RoomKey::new(key.trim());
        let agent_member_id = agent_member_id.trim().to_string();
        let decision_id = validate_decision_id(&body.decision_id)?;
        let digest = decision_digest(&StatusDecisionDigestInput {
            room_id: room.as_str(),
            agent_member_id: &agent_member_id,
            target_status: target.as_str(),
        })?;
        let (binding, applied, audit) = with_rooms(&state, |store| -> Result<_, ApiError> {
            let current = store
                .room_agent_binding(&room, &agent_member_id)
                .map_err(ApiError::from)?
                .ok_or_else(|| ApiError::not_found("agent_binding_not_found"))?;
            let consumed = store
                .room_agent_decision(&room, &decision_id)
                .map_err(ApiError::from)?;
            if consumed.is_none()
                && !prove_owner_and_target(
                    store,
                    &room,
                    &current.owner_member_id,
                    &agent_member_id,
                    &current.agent_package_id,
                )
                .map_err(ApiError::from)?
            {
                return Err(ApiError::forbidden("room_owner_required"));
            }
            store
                .set_room_agent_binding_status(
                    &room,
                    &agent_member_id,
                    SetAgentBindingStatusInput {
                        status: target,
                        actor: principal.id().to_string(),
                        decision_id,
                        request_digest: digest,
                    },
                    Utc::now(),
                )
                .map_err(ApiError::from)
        })?;
        if let Some(audit) = audit.as_ref() {
            publish_room_wake(&state, &room, audit);
        }
        Ok((
            StatusCode::OK,
            json!({
                "ok": true,
                "applied": applied,
                "binding": binding_projection(&binding),
            }),
            room,
            binding.agent_member_id,
            binding.generation,
        ))
    })();
    finish_mutation_with_cancellation(&state, requests, result).await
}

async fn finish_mutation_with_cancellation(
    state: &AppState,
    mut requests: tokio::sync::RwLockWriteGuard<'_, HashMap<RequestId, RequestControl>>,
    result: Result<(StatusCode, Value, RoomKey, String, u64), ApiError>,
) -> (StatusCode, Json<Value>) {
    let (status, body, room, member, generation) = match result {
        Ok(value) => value,
        Err(error) => {
            drop(requests);
            return error.response();
        }
    };
    let cancelled = cancel_superseded_locked(&mut requests, &room, &member, generation);
    drop(requests);
    cleanup_cancelled(state, cancelled).await;
    (status, Json(body))
}

fn cancel_superseded_locked(
    requests: &mut HashMap<RequestId, RequestControl>,
    room: &RoomKey,
    member: &str,
    generation: u64,
) -> Vec<(RequestId, Option<ocean_core::PermissionId>)> {
    let mut cancelled = Vec::new();
    let now = Utc::now();
    for (request_id, control) in requests.iter_mut() {
        let Some(authority) = control.room_agent_authority.as_ref() else {
            continue;
        };
        if &authority.room != room
            || authority.agent_member_id != member
            || authority.generation == generation
            || !control.status.state.is_cancellable()
        {
            continue;
        }
        control.status.state = ocean_core::RequestState::Cancelling;
        control.status.message = Some("room-agent authority generation changed".into());
        control.status.updated_at = Some(now);
        control.cancel.cancel();
        cancelled.push((*request_id, control.status.permission_id));
    }
    cancelled
}

async fn cleanup_cancelled(
    state: &AppState,
    cancelled: Vec<(RequestId, Option<ocean_core::PermissionId>)>,
) {
    for (request_id, permission_id) in cancelled {
        if let Some(permission_id) = permission_id {
            cancel_permission_waiter(&state.permissions, permission_id, request_id).await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn append_admission_audit(
    state: &AppState,
    room: &RoomKey,
    admission_id: &str,
    package: &ResolvedPackage,
    member_id: &str,
    binding: Option<&RoomAgentBinding>,
    outcome: &str,
    reason_code: &str,
) -> Result<(), ApiError> {
    let message = with_rooms(state, |store| {
        store.append_room_agent_admission_audit(
            room,
            RoomAgentAdmissionAuditInput {
                admission_id: admission_id.to_string(),
                agent_member_id: member_id.to_string(),
                agent_package_id: package.package_id.clone(),
                approved_definition_digest: binding
                    .map(|value| value.agent_definition_digest.clone()),
                observed_definition_digest: package.definition_digest.clone(),
                generation: binding.map(|value| value.generation),
                operator_principal_id: binding.map(|value| value.authorized_by.clone()),
                decision_id: binding.map(|value| value.decision_id.clone()),
                outcome: outcome.to_string(),
                reason_code: reason_code.to_string(),
            },
            Utc::now(),
        )
    })
    .map_err(ApiError::from)?;
    publish_room_wake(state, room, &message);
    Ok(())
}

fn append_unresolved_package_audit(
    state: &AppState,
    room: &RoomKey,
    admission_id: &str,
    package_id: &str,
    member_id: &str,
    reason_code: &str,
) -> Result<(), ApiError> {
    let message = with_rooms(state, |store| {
        store.append_room_agent_admission_audit(
            room,
            RoomAgentAdmissionAuditInput {
                admission_id: admission_id.to_string(),
                agent_member_id: member_id.to_string(),
                agent_package_id: package_id.to_string(),
                approved_definition_digest: None,
                observed_definition_digest: "unavailable".into(),
                generation: None,
                operator_principal_id: None,
                decision_id: None,
                outcome: "refused".into(),
                reason_code: reason_code.to_string(),
            },
            Utc::now(),
        )
    })
    .map_err(ApiError::from)?;
    publish_room_wake(state, room, &message);
    Ok(())
}

/// Resolve and durably audit one room-agent admission before any context read.
pub(super) async fn admit_room_agent(
    state: &AppState,
    room: &RoomKey,
    agent_member_id: &str,
    package_id: &str,
    trigger: AdmissionTrigger,
) -> Result<RoomAgentAdmission, ApiError> {
    let admission_id = Uuid::new_v4().to_string();
    let package = match resolve_package(package_id) {
        Ok(package) => package,
        Err(error) => {
            append_unresolved_package_audit(
                state,
                room,
                &admission_id,
                package_id,
                agent_member_id,
                error.code(),
            )?;
            return Err(error);
        }
    };
    let current = with_rooms(state, |store| -> Result<_, RoomStoreError> {
        let binding = store.room_agent_binding(room, agent_member_id)?;
        binding
            .map(|binding| {
                let owner_eligible = binding_owner_eligible(store, room, &binding)?;
                Ok((binding, owner_eligible))
            })
            .transpose()
    })
    .map_err(ApiError::from)?;
    let Some((binding, owner_eligible)) = current else {
        append_admission_audit(
            state,
            room,
            &admission_id,
            &package,
            agent_member_id,
            None,
            "refused",
            "binding_missing",
        )?;
        return Err(ApiError::conflict("agent_binding_required"));
    };
    if !owner_eligible {
        append_admission_audit(
            state,
            room,
            &admission_id,
            &package,
            agent_member_id,
            Some(&binding),
            "refused",
            "owner_ineligible",
        )?;
        return Err(ApiError::conflict("room_owner_required"));
    }
    if binding.agent_package_id != package.package_id {
        append_admission_audit(
            state,
            room,
            &admission_id,
            &package,
            agent_member_id,
            Some(&binding),
            "refused",
            "package_identity_mismatch",
        )?;
        return Err(ApiError::conflict("agent_binding_package_mismatch"));
    }
    if !binding.status.admits() {
        append_admission_audit(
            state,
            room,
            &admission_id,
            &package,
            agent_member_id,
            Some(&binding),
            "refused",
            binding.status.as_str(),
        )?;
        return Err(ApiError::conflict(match binding.status {
            AgentBindingStatus::Suspended => "binding_suspended",
            AgentBindingStatus::Stale => "binding_stale",
            AgentBindingStatus::Revoked => "binding_revoked",
            AgentBindingStatus::Active => "binding_inactive",
        }));
    }
    if binding.agent_definition_digest != package.definition_digest {
        let mut requests = state.requests.write().await;
        let (stale, _changed, audit) = with_rooms(state, |store| {
            store.mark_room_agent_stale(
                room,
                agent_member_id,
                binding.generation,
                &binding.agent_definition_digest,
                &package.definition_digest,
                &admission_id,
                Utc::now(),
            )
        })
        .map_err(ApiError::from)?;
        if let Some(audit) = audit.as_ref() {
            publish_room_wake(state, room, audit);
        }
        let cancelled =
            cancel_superseded_locked(&mut requests, room, agent_member_id, stale.generation);
        drop(requests);
        cleanup_cancelled(state, cancelled).await;
        return Err(ApiError::conflict("binding_stale"));
    }
    if !trigger.permits(binding.activation_policy) {
        append_admission_audit(
            state,
            room,
            &admission_id,
            &package,
            agent_member_id,
            Some(&binding),
            "refused",
            "activation_policy_refused",
        )?;
        return Err(ApiError::conflict("activation_policy_refused"));
    }
    // Room-partitioned memory does not exist yet. Refuse instead of exposing
    // the hard-coded operator namespace under a room-shaped label.
    if binding.memory_scope == MemoryScope::Room {
        append_admission_audit(
            state,
            room,
            &admission_id,
            &package,
            agent_member_id,
            Some(&binding),
            "refused",
            "room_memory_unavailable",
        )?;
        return Err(ApiError::service_unavailable("room_memory_unavailable"));
    }
    let effective_capabilities = binding
        .effective_capabilities()
        .into_iter()
        .filter(|capability| PHASE1_SAFE_CAPABILITIES.contains(&capability.as_str()))
        .collect();
    Ok(RoomAgentAdmission {
        admission_id,
        room: room.clone(),
        agent_member_id: agent_member_id.to_string(),
        package,
        generation: binding.generation,
        decision_id: binding.decision_id,
        context_policy: binding.context_policy,
        effective_capabilities,
    })
}

pub(super) fn append_admission_allow(
    state: &AppState,
    admission: &RoomAgentAdmission,
) -> Result<(), ApiError> {
    let current = with_rooms(state, |store| -> Result<_, RoomStoreError> {
        let binding = store.room_agent_binding(&admission.room, &admission.agent_member_id)?;
        binding
            .map(|binding| {
                let owner_eligible = binding_owner_eligible(store, &admission.room, &binding)?;
                Ok((binding, owner_eligible))
            })
            .transpose()
    })
    .map_err(ApiError::from)?
    .ok_or_else(|| ApiError::conflict("agent_binding_required"))?;
    let (current, owner_eligible) = current;
    if !owner_eligible
        || current.status != AgentBindingStatus::Active
        || current.generation != admission.generation
        || current.agent_definition_digest != admission.package.definition_digest
    {
        return Err(ApiError::conflict("authority_changed_before_registration"));
    }
    append_admission_audit(
        state,
        &admission.room,
        &admission.admission_id,
        &admission.package,
        &admission.agent_member_id,
        Some(&current),
        "admitted",
        "active_binding",
    )
}

pub(super) fn append_remote_output_outcome(
    state: &AppState,
    admission: &RoomAgentAdmission,
    outcome: &str,
    reason_code: &str,
) {
    let current = with_rooms(state, |store| {
        store.room_agent_binding(&admission.room, &admission.agent_member_id)
    })
    .ok()
    .flatten();
    let _ = append_admission_audit(
        state,
        &admission.room,
        &admission.admission_id,
        &admission.package,
        &admission.agent_member_id,
        current.as_ref(),
        outcome,
        reason_code,
    );
}

pub(super) fn admission_generation_is_current(
    state: &AppState,
    admission: &RoomAgentAdmission,
) -> bool {
    with_rooms(state, |store| {
        store.room_agent_generation_is_active(
            &admission.room,
            &admission.agent_member_id,
            admission.generation,
        )
    })
    .unwrap_or(false)
}

pub(super) fn apply_admission_to_control(
    mut control: ocean_agent::PromptControl,
    admission: &RoomAgentAdmission,
) -> ocean_agent::PromptControl {
    let effective: BTreeSet<&str> = admission
        .effective_capabilities
        .iter()
        .map(String::as_str)
        .collect();
    let subprocess = admission
        .package
        .subprocess_capabilities
        .iter()
        .filter(|capability| {
            effective.contains(format!("subprocess:{}", capability.effective_name()).as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    if !admission.package.tool_allowlist.is_empty() {
        control = control.with_tool_allowlist(admission.package.tool_allowlist.clone());
    }
    control = control.with_agent_model(admission.package.model.clone());
    if !subprocess.is_empty() {
        control = control.with_agent_capabilities(admission.package.root.clone(), subprocess);
    }
    control
        .with_authorized_capabilities(admission.effective_capabilities.clone())
        .without_operator_memory()
}

fn into_response(result: Result<(StatusCode, Value), ApiError>) -> (StatusCode, Json<Value>) {
    match result {
        Ok((status, body)) => (status, Json(body)),
        Err(error) => error.response(),
    }
}

#[derive(Debug, Clone)]
pub(super) struct ApiError {
    status: StatusCode,
    code: &'static str,
}

impl ApiError {
    fn bad_request(code: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
        }
    }

    fn forbidden(code: &'static str) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code,
        }
    }

    fn not_found(code: &'static str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code,
        }
    }

    fn conflict(code: &'static str) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code,
        }
    }

    fn service_unavailable(code: &'static str) -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code,
        }
    }

    fn internal(code: &'static str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code,
        }
    }

    pub(super) fn code(&self) -> &'static str {
        self.code
    }

    fn response(self) -> (StatusCode, Json<Value>) {
        (self.status, Json(json!({"ok": false, "error": self.code})))
    }
}

impl From<OperatorAuthError> for ApiError {
    fn from(error: OperatorAuthError) -> Self {
        let status = match error {
            // The accepted manifest freezes missing mutation authority as 503,
            // whether the file or the header is absent. Neither condition may
            // fall back to loopback ambient trust.
            OperatorAuthError::Unavailable | OperatorAuthError::Missing => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            OperatorAuthError::Invalid
            | OperatorAuthError::AmbientCredential
            | OperatorAuthError::ForeignOrigin => StatusCode::FORBIDDEN,
        };
        Self {
            status,
            code: error.code(),
        }
    }
}

impl From<RoomStoreError> for ApiError {
    fn from(error: RoomStoreError) -> Self {
        match error {
            RoomStoreError::UnknownRoom(_) => Self::not_found("room_not_found"),
            RoomStoreError::UnknownAgentBinding { .. } => {
                Self::not_found("agent_binding_not_found")
            }
            RoomStoreError::DecisionReplayMismatch { .. } => {
                Self::conflict("decision_replay_mismatch")
            }
            RoomStoreError::AgentBindingStatusConflict { .. } => {
                Self::conflict("agent_binding_status_conflict")
            }
            RoomStoreError::Encode(_) => Self::bad_request("invalid_request"),
            _ => Self::internal("room_store_error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_digest_covers_nested_executable_and_steering_bytes() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for root in [first.path(), second.path()] {
            std::fs::create_dir_all(root.join("skills/reviewer")).unwrap();
            std::fs::create_dir_all(root.join("tools")).unwrap();
            std::fs::create_dir_all(root.join("subagents/worker")).unwrap();
            std::fs::write(root.join("agent.toml"), "name = 'builder'\n").unwrap();
            std::fs::write(root.join("skills/reviewer/SKILL.md"), "review v1\n").unwrap();
            std::fs::write(root.join("tools/check.sh"), "check v1\n").unwrap();
            std::fs::write(root.join("subagents/worker/instructions.md"), "work v1\n").unwrap();
        }
        let original = digest_package_tree(first.path()).unwrap();
        assert_eq!(original, digest_package_tree(second.path()).unwrap());
        std::fs::write(
            first.path().join("subagents/worker/instructions.md"),
            "work v2\n",
        )
        .unwrap();
        assert_ne!(original, digest_package_tree(first.path()).unwrap());
    }

    #[test]
    fn activation_policy_distinguishes_explicit_mention_thread_and_unknown() {
        assert!(AdmissionTrigger::Explicit.permits(ActivationPolicy::ExplicitOnly));
        assert!(!AdmissionTrigger::Mention.permits(ActivationPolicy::ExplicitOnly));
        assert!(AdmissionTrigger::Mention.permits(ActivationPolicy::Mention));
        assert!(!AdmissionTrigger::ThreadReply.permits(ActivationPolicy::Mention));
        assert!(AdmissionTrigger::ThreadReply.permits(ActivationPolicy::TaskAndThread));
        assert!(!AdmissionTrigger::Unknown.permits(ActivationPolicy::TaskAndThread));
    }

    #[test]
    fn phase1_resource_ceiling_rejects_every_ambient_grant() {
        let package = ResolvedPackage {
            package_id: "builder".into(),
            display_name: "Builder".into(),
            definition_digest: "sha256:test".into(),
            definition_revision: None,
            requested_capabilities: vec!["read".into(), "bash".into(), "web_fetch".into()],
            instructions_layer: None,
            tool_allowlist: vec!["read".into()],
            model: None,
            root: std::path::PathBuf::from("builder"),
            subprocess_capabilities: Vec::new(),
        };
        for capability in &package.requested_capabilities {
            let error =
                validate_capability_grants(&package, std::slice::from_ref(capability)).unwrap_err();
            assert_eq!(error.code(), "capability_not_grantable");
        }
    }

    #[test]
    fn missing_operator_header_maps_to_service_unavailable() {
        let error = ApiError::from(OperatorAuthError::Missing);
        assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn public_binding_projection_omits_operator_and_replay_secrets() {
        let binding = RoomAgentBinding {
            room_id: RoomKey::new("room-a"),
            agent_member_id: "agent-a".into(),
            agent_package_id: "builder".into(),
            agent_definition_digest: "sha256:test".into(),
            agent_definition_revision: None,
            display_name: "Builder".into(),
            owner_member_id: "human-a".into(),
            authorized_by: "operator-secret-fingerprint".into(),
            authorized_at: Utc::now(),
            activation_policy: ActivationPolicy::ExplicitOnly,
            context_policy: ContextPolicy::InvocationOnly,
            memory_scope: MemoryScope::None,
            requested_capabilities: Vec::new(),
            room_capability_grants: Vec::new(),
            status: AgentBindingStatus::Active,
            generation: 1,
            decision_id: "replay-secret".into(),
            request_digest: "request-secret".into(),
            revoked_at: None,
            revoked_by: None,
        };
        let encoded = binding_projection(&binding).to_string();
        assert!(!encoded.contains("operator-secret-fingerprint"));
        assert!(!encoded.contains("replay-secret"));
        assert!(!encoded.contains("request-secret"));
    }

    #[test]
    fn local_binding_owner_eligibility_tracks_live_persisted_ownership() {
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        let room = RoomKey::new("room-owner-proof");
        store
            .create(room.clone(), "Owner Proof", None, Utc::now())
            .unwrap();
        store
            .add_participant(
                &room,
                ocean_core::RoomParticipant {
                    id: "human-a".into(),
                    kind: RoomParticipantKind::Human,
                    display_name: "Human A".into(),
                },
                Utc::now(),
            )
            .unwrap();
        store
            .add_agent_participant_with_owner(
                &room,
                ocean_core::RoomParticipant {
                    id: "builder".into(),
                    kind: RoomParticipantKind::Agent,
                    display_name: "Builder".into(),
                },
                "human-a",
                Utc::now(),
            )
            .unwrap();
        let binding = RoomAgentBinding {
            room_id: room.clone(),
            agent_member_id: "builder".into(),
            agent_package_id: "builder".into(),
            agent_definition_digest: "sha256:test".into(),
            agent_definition_revision: None,
            display_name: "Builder".into(),
            owner_member_id: "human-a".into(),
            authorized_by: "operator".into(),
            authorized_at: Utc::now(),
            activation_policy: ActivationPolicy::ExplicitOnly,
            context_policy: ContextPolicy::InvocationOnly,
            memory_scope: MemoryScope::None,
            requested_capabilities: Vec::new(),
            room_capability_grants: Vec::new(),
            status: AgentBindingStatus::Active,
            generation: 1,
            decision_id: "decision".into(),
            request_digest: "digest".into(),
            revoked_at: None,
            revoked_by: None,
        };
        assert!(binding_owner_eligible(&mut store, &room, &binding).unwrap());
        store
            .remove_participant(&room, "human-a", Utc::now())
            .unwrap();
        assert!(!binding_owner_eligible(&mut store, &room, &binding).unwrap());
    }
}
