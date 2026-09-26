//! Stage A3b HTTP mutation surfaces over the A3a registry writer.
//!
//! This module is the §15 wire contract and nothing else: operator
//! authentication, strict request bodies, the common pre-commit and committed
//! envelopes, and the post-commit supervisor reconciliation handshake that
//! decides between HTTP 200 and the committed 202. Registry authority stays in
//! [`super::transaction`]; process ownership stays in the supervisor.
//!
//! Load-bearing rules:
//!
//! - Every mutation (and trust preview, which shares its route) requires the
//!   local operator principal from [`crate::room_operator`]: header-only
//!   `X-Ocean-Operator`, cookie/foreign-origin refusal before comparison,
//!   fail-closed 503 when no key is configured. Authentication runs before the
//!   body is parsed, so an unauthenticated caller learns nothing about
//!   validation.
//! - The writer is blocking filesystem code that may sleep on `.state.lock`
//!   (250 ms) or the acquisition gate: it runs only inside `spawn_blocking`,
//!   never on an async worker.
//! - A pre-commit failure always says `committed:false` with the previously
//!   effective revision. A commit is never reported as an error: when the
//!   supervisor cannot confirm reconciliation or reap, the response is the
//!   committed envelope at HTTP 202, and the committed revision is
//!   authoritative immediately.
//! - No response carries a path, secret value, package byte, or stderr: codes
//!   are closed and messages are fixed text.
//! - Git sources are part of the §15 body grammar but belong to slice A4. Until
//!   A4 lands they fail closed as `git_connection_pinning_unavailable` before
//!   any acquisition, exactly the code §13.2 reserves for "pinned Git is not
//!   available here"; there is no unpinned fallback.

use std::collections::HashSet;

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::SupervisorReconcile;
use crate::room_operator::OperatorAuthError;
use crate::AppState;

#[cfg(unix)]
use super::transaction::{
    EnablementScope, MutationError, MutationOutcome, RegistryWriter, ServiceActivity, TrustRequest,
    TrustResult,
};

/// Codes a caller may simply retry after a short wait: nothing was written.
const RETRYABLE: [&str; 2] = ["extension_state_busy", "acquisition_capacity"];

// ---------------------------------------------------------------------------
// Request bodies. Every object denies unknown fields (§15).
// ---------------------------------------------------------------------------

/// Install/update source. `local-path` is live in A3b; `git` parses so the
/// grammar is exact (no `subdir`, no extra field) but is refused until A4.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(crate) enum SourceRequest {
    #[serde(rename = "local-path")]
    LocalPath { path: String },
    #[serde(rename = "git")]
    Git {
        #[allow(dead_code)]
        url: String,
        #[allow(dead_code)]
        revision: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InstallRequest {
    expected_state_revision: u64,
    source: SourceRequest,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateRequest {
    expected_state_revision: u64,
    source: SourceRequest,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ScopeRequest {
    Global,
    Project { project_id: Uuid },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScopeMutationRequest {
    expected_state_revision: u64,
    scope: ScopeRequest,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoveRequest {
    expected_state_revision: u64,
    #[serde(default)]
    purge_state: bool,
}

// ---------------------------------------------------------------------------
// Envelopes.
// ---------------------------------------------------------------------------

/// `reconciliation` field of the committed envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reconciliation {
    Complete,
    Pending,
    Blocked,
}

/// `reap` field of the committed envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reap {
    NotRequired,
    Complete,
    Pending,
}

/// Map the supervisor's answer onto the two envelope fields.
///
/// - `effective` is the registry-derived post-commit effectiveness of the
///   package; `stopped` is the supervisor's synchronous activity projection.
///   When the supervisor could not answer, a package that is no longer
///   effective but still owns a process or temp root has a reap outstanding;
///   anything else has nothing to reap.
/// - A committed retention cleanup (payload/state-root deletion) that the
///   writer deferred to the next recovery is reported as a pending reap: the
///   registry generation is coherent, but the package's bytes are not yet
///   gone.
pub(crate) fn envelope_states(
    reconcile: SupervisorReconcile,
    effective: bool,
    stopped: bool,
    retention_cleanup_pending: bool,
) -> (Reconciliation, Reap) {
    let outstanding = if !effective && !stopped {
        Reap::Pending
    } else {
        Reap::NotRequired
    };
    let (reconciliation, reap) = match reconcile {
        SupervisorReconcile::Complete { reaped: true } => {
            (Reconciliation::Complete, Reap::Complete)
        }
        SupervisorReconcile::Complete { reaped: false } => {
            (Reconciliation::Complete, Reap::NotRequired)
        }
        SupervisorReconcile::CleanupIncomplete => (Reconciliation::Pending, Reap::Pending),
        SupervisorReconcile::Pending => (Reconciliation::Pending, outstanding),
        SupervisorReconcile::Blocked => (Reconciliation::Blocked, outstanding),
    };
    let reap = if retention_cleanup_pending {
        Reap::Pending
    } else {
        reap
    };
    (reconciliation, reap)
}

/// HTTP 200 only when the reconciliation the operation required is complete
/// and nothing remains to reap; otherwise the committed 202.
pub(crate) fn committed_status(reconciliation: Reconciliation, reap: Reap) -> StatusCode {
    if reconciliation == Reconciliation::Complete && reap != Reap::Pending {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    }
}

/// Fixed safe text for the codes this route layer mints itself. Writer codes
/// fall through to the writer's own fixed table.
fn route_message(code: &str) -> Option<&'static str> {
    Some(match code {
        "invalid_request" => "request body is malformed or has unknown fields",
        "extension_state_busy" => "extension registry is busy; retry shortly",
        "git_connection_pinning_unavailable" => {
            "pinned public Git acquisition is not available in this daemon"
        }
        "project_registry_unavailable" => "project registry is unavailable",
        "unsupported_platform" => "extension package management is unsupported on this platform",
        "operator_identity_unavailable" => {
            "no operator identity is configured; extension management is unavailable"
        }
        "operator_credential_missing" => "missing X-Ocean-Operator credential",
        "operator_credential_invalid" => "invalid operator credential",
        "ambient_credential_rejected" => "cookie-bearing requests cannot authorize",
        "foreign_origin_rejected" => "request origin is not allowed to authorize",
        _ => return None,
    })
}

#[cfg(unix)]
fn message_for(code: &'static str) -> &'static str {
    route_message(code).unwrap_or_else(|| {
        MutationError {
            operation_id: Uuid::nil(),
            committed: false,
            state_revision: 0,
            code,
        }
        .message()
    })
}

#[cfg(not(unix))]
fn message_for(code: &'static str) -> &'static str {
    route_message(code).unwrap_or("daemon-owned extension state is unavailable or incoherent")
}

/// HTTP status for a closed pre-commit code.
pub(crate) fn precommit_status(code: &str) -> StatusCode {
    match code {
        "invalid_request"
        | "invalid_extension_id"
        | "invalid_source"
        | "package_invalid"
        | "package_identity_mismatch"
        | "digest_mismatch"
        | "grant_widens_manifest"
        | "declaration_policy_rejected"
        | "native_process_ack_required"
        | "invalid_secret_binding"
        | "unknown_service"
        | "invalid_capability_grant" => StatusCode::BAD_REQUEST,
        "extension_not_installed" | "extension_not_found" | "project_not_found" => {
            StatusCode::NOT_FOUND
        }
        "already_installed"
        | "state_revision_conflict"
        | "extension_active"
        | "grant_confirmation_mismatch"
        | "trust_required"
        | "service_grant_required"
        | "unresolved_bindings"
        | "host_incompatible"
        | "unsupported_platform" => StatusCode::CONFLICT,
        "acquisition_capacity" => StatusCode::TOO_MANY_REQUESTS,
        "extension_state_busy"
        | "operator_identity_unavailable"
        | "operator_credential_missing" => StatusCode::SERVICE_UNAVAILABLE,
        "operator_credential_invalid"
        | "ambient_credential_rejected"
        | "foreign_origin_rejected" => StatusCode::FORBIDDEN,
        "git_connection_pinning_unavailable" => StatusCode::NOT_IMPLEMENTED,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

fn error_object(code: &'static str) -> Value {
    let mut error = json!({"code": code, "message": message_for(code)});
    if RETRYABLE.contains(&code) {
        error["retryable"] = Value::Bool(true);
    }
    error
}

/// §15 pre-commit envelope. `state_revision` is the still-effective revision
/// (0 when the refusal happened before any registry read, the writer's own
/// convention).
pub(crate) fn precommit(operation_id: Uuid, state_revision: u64, code: &'static str) -> Response {
    (
        precommit_status(code),
        Json(json!({
            "ok": false,
            "mutation": {
                "operation_id": operation_id,
                "committed": false,
                "state_revision": state_revision,
            },
            "error": error_object(code),
        })),
    )
        .into_response()
}

/// §15 committed recovery-error envelope: the irreversible commit point was
/// crossed but roll-forward could not restore a coherent generation.
fn committed_recovery_error(
    operation_id: Uuid,
    state_revision: u64,
    code: &'static str,
) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "ok": false,
            "mutation": {
                "operation_id": operation_id,
                "committed": true,
                "state_revision": state_revision,
            },
            "error": error_object(code),
        })),
    )
        .into_response()
}

#[cfg(unix)]
fn mutation_error(error: MutationError) -> Response {
    if error.committed {
        committed_recovery_error(error.operation_id, error.state_revision, error.code)
    } else {
        precommit(error.operation_id, error.state_revision, error.code)
    }
}

fn operator_code(error: OperatorAuthError) -> &'static str {
    error.code()
}

/// A refusal decided before any registry read: a closed code that becomes the
/// pre-commit envelope with a fresh operation id and `state_revision: 0`.
type Refusal = &'static str;

fn refuse(code: Refusal) -> Response {
    precommit(Uuid::new_v4(), 0, code)
}

/// Operator principal check shared by every route here. Runs before the body
/// is examined.
fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), Refusal> {
    state
        .room_operator
        .authorize(headers)
        .map(|_| ())
        .map_err(operator_code)
}

fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, Refusal> {
    body.map(|Json(value)| value).map_err(|_| "invalid_request")
}

fn registered_projects(state: &AppState) -> Result<HashSet<Uuid>, Refusal> {
    state
        .runtime
        .list_projects()
        .map(|projects| projects.into_iter().map(|project| project.id).collect())
        .map_err(|_| "project_registry_unavailable")
}

// ---------------------------------------------------------------------------
// Writer execution and post-commit reconciliation (Unix: the writer is
// compiled only where its descriptor-relative primitives exist).
// ---------------------------------------------------------------------------

/// The real supervisor's synchronous activity projection, bound to the
/// writer's update/remove guard. With no supervisor there is no process a
/// package could own.
#[cfg(unix)]
struct SupervisorActivity(Option<crate::extension_service::ServiceActivityLedger>);

#[cfg(unix)]
impl ServiceActivity for SupervisorActivity {
    fn package_stopped(&self, package_id: &str) -> bool {
        self.0
            .as_ref()
            .is_none_or(|ledger| ledger.package_stopped(package_id))
    }
}

#[cfg(unix)]
fn activity(state: &AppState) -> SupervisorActivity {
    SupervisorActivity(
        state
            .extension_supervisor
            .as_ref()
            .map(|supervisor| supervisor.activity()),
    )
}

/// Run one blocking writer operation off the async workers.
#[cfg(unix)]
async fn blocking<T: Send + 'static>(
    state: &AppState,
    operation: impl FnOnce(RegistryWriter) -> Result<T, MutationError> + Send + 'static,
) -> Result<T, Response> {
    let writer = RegistryWriter::new(state.runtime.config_dir().to_path_buf());
    match tokio::task::spawn_blocking(move || operation(writer)).await {
        Ok(result) => result.map_err(mutation_error),
        Err(_) => Err(precommit(Uuid::new_v4(), 0, "extension_state_unavailable")),
    }
}

/// Reconcile the supervisor to the committed revision and answer with the
/// common committed envelope. Never retried here and never turned into an
/// error: the commit already happened.
#[cfg(unix)]
async fn committed(state: &AppState, outcome: MutationOutcome) -> Response {
    let reconcile = match &state.extension_supervisor {
        Some(supervisor) => {
            supervisor
                .reconcile_registry(outcome.state_revision, &outcome.id)
                .await
        }
        None => SupervisorReconcile::Blocked,
    };
    let stopped = activity(state).package_stopped(&outcome.id);
    let (reconciliation, reap) = envelope_states(
        reconcile,
        outcome.effective,
        stopped,
        outcome.retention_cleanup_pending,
    );
    (
        committed_status(reconciliation, reap),
        Json(json!({
            "ok": true,
            "mutation": {
                "operation_id": outcome.operation_id,
                "committed": true,
                "state_revision": outcome.state_revision,
                "id": outcome.id,
                "digest": outcome.digest,
                "effective": outcome.effective,
                "reconciliation": reconciliation,
                "reap": reap,
            }
        })),
    )
        .into_response()
}

#[cfg(unix)]
fn local_path(source: SourceRequest) -> Result<String, Refusal> {
    match source {
        SourceRequest::LocalPath { path } => Ok(path),
        // Slice A4 owns pinned public Git acquisition (§13.2). Refuse before
        // any permit, quarantine, DNS, or process.
        SourceRequest::Git { .. } => Err("git_connection_pinning_unavailable"),
    }
}

#[cfg(unix)]
fn enablement_scope(scope: ScopeRequest) -> EnablementScope {
    match scope {
        ScopeRequest::Global => EnablementScope::Global,
        ScopeRequest::Project { project_id } => EnablementScope::Project(project_id),
    }
}

// ---------------------------------------------------------------------------
// Handlers.
// ---------------------------------------------------------------------------

/// `POST /v1/extensions/install`: acquire into quarantine WITHOUT the state
/// lock, then publish under it. Install grants nothing and starts nothing.
pub(crate) async fn install(
    State(state): State<AppState>,
    headers: HeaderMap,
    request: Result<Json<InstallRequest>, JsonRejection>,
) -> Response {
    if let Err(code) = authorize(&state, &headers) {
        return refuse(code);
    }
    let request = match body(request) {
        Ok(request) => request,
        Err(code) => return refuse(code),
    };
    #[cfg(unix)]
    {
        let path = match local_path(request.source) {
            Ok(path) => path,
            Err(code) => return refuse(code),
        };
        let expected = request.expected_state_revision;
        match blocking(&state, move |writer| {
            let quarantine = writer.acquire_local(&path)?;
            writer.install(expected, quarantine)
        })
        .await
        {
            Ok(outcome) => committed(&state, outcome).await,
            Err(refusal) => refusal,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = request;
        precommit(Uuid::new_v4(), 0, "unsupported_platform")
    }
}

/// `POST /v1/extensions/{id}/update`: requires the package fully disabled and
/// stopped; the replacement digest is untrusted until a separate trust.
pub(crate) async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<UpdateRequest>, JsonRejection>,
) -> Response {
    if let Err(code) = authorize(&state, &headers) {
        return refuse(code);
    }
    let request = match body(request) {
        Ok(request) => request,
        Err(code) => return refuse(code),
    };
    #[cfg(unix)]
    {
        if ocean_extension::validate_extension_id(&id).is_err() {
            return precommit(Uuid::new_v4(), 0, "invalid_extension_id");
        }
        let path = match local_path(request.source) {
            Ok(path) => path,
            Err(code) => return refuse(code),
        };
        let expected = request.expected_state_revision;
        let activity = activity(&state);
        match blocking(&state, move |writer| {
            let quarantine = writer.acquire_local(&path)?;
            writer.update(&id, expected, quarantine, &activity)
        })
        .await
        {
            Ok(outcome) => committed(&state, outcome).await,
            Err(refusal) => refusal,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (id, request);
        precommit(Uuid::new_v4(), 0, "unsupported_platform")
    }
}

/// `POST /v1/extensions/{id}/trust`: preview without `confirm_grant_diff`
/// (mutates nothing, HTTP 200 `applied:false`), apply with the exact
/// confirmation (common committed envelope).
pub(crate) async fn trust(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<TrustBody>, JsonRejection>,
) -> Response {
    if let Err(code) = authorize(&state, &headers) {
        return refuse(code);
    }
    let request = match body(request) {
        Ok(request) => request,
        Err(code) => return refuse(code),
    };
    #[cfg(unix)]
    {
        let digest = request.digest.clone();
        let target = id.clone();
        match blocking(&state, move |writer| writer.trust(&target, request)).await {
            Ok(TrustResult::Preview {
                state_revision,
                preview,
            }) => (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "applied": false,
                    "committed": false,
                    "state_revision": state_revision,
                    "id": id,
                    "digest": digest,
                    "preview": preview,
                })),
            )
                .into_response(),
            Ok(TrustResult::Applied(outcome)) => committed(&state, outcome).await,
            Err(refusal) => refusal,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (id, request);
        precommit(Uuid::new_v4(), 0, "unsupported_platform")
    }
}

/// The trust body is the writer's own strict `TrustRequest` on Unix.
#[cfg(unix)]
pub(crate) type TrustBody = TrustRequest;
#[cfg(not(unix))]
pub(crate) type TrustBody = Value;

/// `POST /v1/extensions/{id}/enable`.
pub(crate) async fn enable(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<ScopeMutationRequest>, JsonRejection>,
) -> Response {
    scope_mutation(state, id, headers, request, true).await
}

/// `POST /v1/extensions/{id}/disable`: commits the filter removal, then
/// answers 200 only after the supervisor has stopped and reaped any service
/// that is no longer effective; a bounded reap failure is the committed 202.
pub(crate) async fn disable(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<ScopeMutationRequest>, JsonRejection>,
) -> Response {
    scope_mutation(state, id, headers, request, false).await
}

async fn scope_mutation(
    state: AppState,
    id: String,
    headers: HeaderMap,
    request: Result<Json<ScopeMutationRequest>, JsonRejection>,
    enable: bool,
) -> Response {
    if let Err(code) = authorize(&state, &headers) {
        return refuse(code);
    }
    let request = match body(request) {
        Ok(request) => request,
        Err(code) => return refuse(code),
    };
    #[cfg(unix)]
    {
        let projects = match registered_projects(&state) {
            Ok(projects) => projects,
            Err(code) => return refuse(code),
        };
        let expected = request.expected_state_revision;
        let scope = enablement_scope(request.scope);
        match blocking(&state, move |writer| {
            if enable {
                writer.enable(&id, expected, scope, &projects)
            } else {
                writer.disable(&id, expected, scope, &projects)
            }
        })
        .await
        {
            Ok(outcome) => committed(&state, outcome).await,
            Err(refusal) => refusal,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (id, request, enable, registered_projects(&state));
        precommit(Uuid::new_v4(), 0, "unsupported_platform")
    }
}

/// `DELETE /v1/extensions/{id}`: requires every scope disabled and the
/// package already reaped; revokes install, trust, acknowledgement,
/// bindings, and enablement.
pub(crate) async fn remove(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    request: Result<Json<RemoveRequest>, JsonRejection>,
) -> Response {
    if let Err(code) = authorize(&state, &headers) {
        return refuse(code);
    }
    let request = match body(request) {
        Ok(request) => request,
        Err(code) => return refuse(code),
    };
    #[cfg(unix)]
    {
        let expected = request.expected_state_revision;
        let purge = request.purge_state;
        let activity = activity(&state);
        match blocking(&state, move |writer| {
            writer.remove(&id, expected, purge, &activity)
        })
        .await
        {
            Ok(outcome) => committed(&state, outcome).await,
            Err(refusal) => refusal,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (id, request);
        precommit(Uuid::new_v4(), 0, "unsupported_platform")
    }
}

#[cfg(test)]
mod tests;
