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
//!   fail-closed 503 when no key is configured. Handlers extract only
//!   infallible parts (headers, the raw path result, raw body bytes), so
//!   authentication really runs before the path or body is parsed and an
//!   unauthenticated caller learns nothing about validation.
//! - Once authorized, the operation — acquisition, commit, and the
//!   post-commit reconciliation request — runs in one detached task the
//!   handler merely awaits. A client disconnect therefore can never commit a
//!   revision without also asking the supervisor to reconcile it.
//! - The writer is blocking filesystem code that may sleep on `.state.lock`
//!   (250 ms) or the acquisition gate: it is constructed and run only inside
//!   `spawn_blocking`, never on an async worker. The project registry read is
//!   off the executor too.
//! - A pre-commit failure always says `committed:false` with the previously
//!   effective revision. A commit is never reported as an error: when the
//!   supervisor cannot confirm reconciliation or reap, the response is the
//!   committed envelope at HTTP 202, and the committed revision is
//!   authoritative immediately. The supervisor itself re-runs a blocked or
//!   incomplete pass with backoff.
//! - No response carries a path, secret value, package byte, or stderr: codes
//!   are closed and messages are fixed text.
//! - Git sources (slice A4) acquire through the writer's pinned §13.2 path in
//!   the same detached blocking task as a local source: strict grammar before
//!   any permit, one DNS resolution checked for public-only answers, one
//!   pinned `git` process per attempt, and the same seal and publication.
//!   Where pinning is impossible the code is `git_connection_pinning_unavailable`
//!   and there is no unpinned fallback. No Git output reaches a response.

use std::collections::HashSet;

use axum::{
    body::Body,
    extract::{rejection::PathRejection, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use super::SupervisorReconcile;
#[cfg(unix)]
use crate::extension_service::ActivationReset;
use crate::AppState;

#[cfg(unix)]
use super::transaction::{
    ActivitySnapshot, EnablementScope, MutationError, MutationOutcome, RegistryWriter,
    ServiceActivity, TrustRequest, TrustResult, VerifiedQuarantine,
};

/// Codes a caller may simply retry after a short wait: nothing was written.
const RETRYABLE: [&str; 3] = [
    "extension_state_busy",
    "acquisition_capacity",
    "reconciliation_in_progress",
];

// ---------------------------------------------------------------------------
// Request bodies. Every object denies unknown fields (§15).
// ---------------------------------------------------------------------------

/// Install/update source: a local directory or one exact public Git commit
/// (no `subdir`, no extra field).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(crate) enum SourceRequest {
    #[serde(rename = "local-path")]
    LocalPath { path: String },
    #[serde(rename = "git")]
    Git { url: String, revision: String },
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

/// The trust body is the writer's own strict `TrustRequest` on Unix.
#[cfg(unix)]
type TrustBody = TrustRequest;
#[cfg(not(unix))]
type TrustBody = Value;

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

/// Map the supervisor's answer for THIS package onto the two envelope fields.
///
/// - `Complete`/`CleanupIncomplete` are what the pass itself did for the
///   package (stopped and reaped, or could not prove cleanup).
/// - When no pass answered (`Pending`/`Blocked`), a package that is no longer
///   registry-`effective` but still `owned` a process or temp root has a reap
///   outstanding; anything else has nothing to reap.
/// - A committed retention cleanup (payload/state-root deletion) that the
///   writer deferred to the next recovery is reported as a pending reap: the
///   registry generation is coherent, but the package's bytes are not yet
///   gone.
pub(crate) fn envelope_states(
    reconcile: SupervisorReconcile,
    effective: bool,
    retention_cleanup_pending: bool,
) -> (Reconciliation, Reap) {
    let outstanding = |owned: bool| {
        if !effective && owned {
            Reap::Pending
        } else {
            Reap::NotRequired
        }
    };
    let (reconciliation, reap) = match reconcile {
        SupervisorReconcile::Complete { reaped: true } => {
            (Reconciliation::Complete, Reap::Complete)
        }
        SupervisorReconcile::Complete { reaped: false } => {
            (Reconciliation::Complete, Reap::NotRequired)
        }
        SupervisorReconcile::CleanupIncomplete => (Reconciliation::Pending, Reap::Pending),
        SupervisorReconcile::Pending { owned } => (Reconciliation::Pending, outstanding(owned)),
        SupervisorReconcile::Blocked { owned } => (Reconciliation::Blocked, outstanding(owned)),
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
        "unsupported_media_type" => "mutation bodies must be application/json",
        "outcome_unknown" => {
            "the mutation outcome is unknown; reinspect by revision before retrying"
        }
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
        | "invalid_capability_grant"
        | "invalid_git_source"
        | "git_host_not_public"
        | "git_revision_mismatch"
        | "git_tree_unsupported"
        | "git_acquisition_limit" => StatusCode::BAD_REQUEST,
        "extension_not_installed" | "extension_not_found" | "project_not_found" => {
            StatusCode::NOT_FOUND
        }
        "already_installed"
        | "state_revision_conflict"
        | "extension_active"
        | "reconciliation_in_progress"
        | "grant_confirmation_mismatch"
        | "trust_required"
        | "service_grant_required"
        | "unresolved_bindings"
        | "host_incompatible"
        | "unsupported_platform" => StatusCode::CONFLICT,
        "acquisition_capacity" => StatusCode::TOO_MANY_REQUESTS,
        "unsupported_media_type" => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "extension_state_busy"
        | "operator_identity_unavailable"
        | "operator_credential_missing" => StatusCode::SERVICE_UNAVAILABLE,
        "operator_credential_invalid"
        | "ambient_credential_rejected"
        | "foreign_origin_rejected" => StatusCode::FORBIDDEN,
        "git_connection_pinning_unavailable" => StatusCode::NOT_IMPLEMENTED,
        "git_resolution_failed" | "git_fetch_failed" => StatusCode::BAD_GATEWAY,
        "git_acquisition_timeout" => StatusCode::GATEWAY_TIMEOUT,
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

/// A refusal decided before any registry read: a closed code that becomes the
/// pre-commit envelope with a fresh operation id and `state_revision: 0`.
type Refusal = &'static str;

fn refuse(code: Refusal) -> Response {
    precommit(Uuid::new_v4(), 0, code)
}

// ---------------------------------------------------------------------------
// Request intake: authenticate first, then parse.
// ---------------------------------------------------------------------------

/// Operator principal check. Runs before the path or body is examined.
fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), Refusal> {
    state
        .room_operator
        .authorize(headers)
        .map(|_| ())
        .map_err(|error| error.code())
}

/// Largest mutation body read. A trust body at the §12.1 limits (256
/// bindings per service) is far below this.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// `application/json` (parameters such as `charset` allowed), checked after
/// authentication and before a single body byte is read.
fn json_content_type(headers: &HeaderMap) -> Result<(), Refusal> {
    let is_json = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"));
    if is_json {
        Ok(())
    } else {
        Err("unsupported_media_type")
    }
}

async fn parse_body<T: DeserializeOwned>(body: Body) -> Result<T, Refusal> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| "invalid_request")?;
    serde_json::from_slice(&bytes).map_err(|_| "invalid_request")
}

fn parse_path(path: Result<Path<String>, PathRejection>) -> Result<String, Refusal> {
    path.map(|Path(id)| id).map_err(|_| "invalid_extension_id")
}

/// Authenticate, then parse. The returned values are only ever produced for an
/// authorized caller.
async fn intake<T: DeserializeOwned>(
    state: &AppState,
    headers: &HeaderMap,
    path: Option<Result<Path<String>, PathRejection>>,
    body: Body,
) -> Result<(Option<String>, T), Refusal> {
    // Order is the contract: credential, then media type, then path, and only
    // then is the body read at all.
    authorize(state, headers)?;
    json_content_type(headers)?;
    let id = path.map(parse_path).transpose()?;
    let body = parse_body(body).await?;
    Ok((id, body))
}

/// The task running a mutation died (panicked) where it may already have
/// crossed the commit point. Nothing about the outcome can be asserted, so the
/// §15 envelope says so: `committed: null`, fixed code `outcome_unknown`. The
/// caller must reinspect by revision before deciding anything; the CLI never
/// retries it.
fn outcome_unknown() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "ok": false,
            "mutation": {"operation_id": null, "committed": null, "state_revision": null},
            "error": error_object("outcome_unknown"),
        })),
    )
        .into_response()
}

/// Run the authorized operation in a detached task and await it. Dropping the
/// handler future (client disconnect) cannot cancel the commit or the
/// post-commit reconciliation request that follows it.
async fn detached(work: impl std::future::Future<Output = Response> + Send + 'static) -> Response {
    tokio::spawn(work)
        .await
        .unwrap_or_else(|_| outcome_unknown())
}

// ---------------------------------------------------------------------------
// Writer execution and post-commit reconciliation (Unix: the writer is
// compiled only where its descriptor-relative primitives exist).
// ---------------------------------------------------------------------------

/// The real supervisor's synchronous activity projection, bound to the
/// writer's update/remove guard. With no supervisor there is no process a
/// package could own and no pass in flight.
#[cfg(unix)]
struct SupervisorActivity(Option<crate::extension_service::ServiceActivityLedger>);

#[cfg(unix)]
impl ServiceActivity for SupervisorActivity {
    fn package_stopped(&self, package_id: &str) -> bool {
        self.0
            .as_ref()
            .is_none_or(|ledger| ledger.package_stopped(package_id))
    }

    fn reconciliation_in_progress(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|ledger| ledger.reconciliation_in_progress())
    }
    fn snapshot(&self, package_id: &str) -> ActivitySnapshot {
        let (stopped, reconciling) = self
            .0
            .as_ref()
            .map_or((true, false), |ledger| ledger.snapshot(package_id));
        ActivitySnapshot {
            stopped,
            reconciling,
        }
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

/// Construct the writer and run one blocking operation on the blocking pool.
#[cfg(unix)]
async fn blocking<T: Send + 'static>(
    state: &AppState,
    operation: impl FnOnce(RegistryWriter) -> Result<T, MutationError> + Send + 'static,
) -> Result<T, Response> {
    let config_dir = state.runtime.config_dir().to_path_buf();
    match tokio::task::spawn_blocking(move || operation(RegistryWriter::new(config_dir))).await {
        Ok(result) => result.map_err(mutation_error),
        // A panic inside the writer may have happened after the commit
        // point; never claim `committed: false` for it.
        Err(_) => Err(outcome_unknown()),
    }
}

/// Registered project ids, read off the async executor.
#[cfg(unix)]
async fn registered_projects(state: &AppState) -> Result<HashSet<Uuid>, Response> {
    let runtime = std::sync::Arc::clone(&state.runtime);
    match tokio::task::spawn_blocking(move || runtime.list_projects()).await {
        Ok(Ok(projects)) => Ok(projects.into_iter().map(|project| project.id).collect()),
        _ => Err(refuse("project_registry_unavailable")),
    }
}

/// Reconcile the supervisor to the committed revision and answer with the
/// common committed envelope. Never retried here and never turned into an
/// error: the commit already happened. `reset` marks commits that
/// changed the package's trust or may have ended its effectiveness, so the
/// supervisor mints a new activation epoch even if a later commit restored an
/// identical descriptor before any pass ran.
#[cfg(unix)]
async fn committed(
    state: &AppState,
    outcome: MutationOutcome,
    reset: Option<ActivationReset>,
) -> Response {
    let reconcile = match &state.extension_supervisor {
        Some(supervisor) => {
            supervisor
                .reconcile_registry(outcome.state_revision, &outcome.id, reset)
                .await
        }
        None => SupervisorReconcile::Blocked { owned: false },
    };
    let (reconciliation, reap) = envelope_states(
        reconcile,
        outcome.effective,
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

/// Quarantine one install/update source without `.state.lock`: a local
/// directory copy, or the pinned public Git acquisition (§13.2).
#[cfg(unix)]
fn acquire(
    writer: &RegistryWriter,
    source: &SourceRequest,
) -> Result<VerifiedQuarantine, MutationError> {
    match source {
        SourceRequest::LocalPath { path } => writer.acquire_local(path),
        SourceRequest::Git { url, revision } => writer.acquire_git(url, revision),
    }
}

#[cfg(unix)]
fn enablement_scope(scope: ScopeRequest) -> EnablementScope {
    match scope {
        ScopeRequest::Global => EnablementScope::Global,
        ScopeRequest::Project { project_id } => EnablementScope::Project(project_id),
    }
}

/// Commit one writer operation, then reconcile, inside the caller's detached
/// task.
#[cfg(unix)]
async fn commit_and_reconcile(
    state: AppState,
    reset: Option<ActivationReset>,
    operation: impl FnOnce(RegistryWriter) -> Result<MutationOutcome, MutationError> + Send + 'static,
) -> Response {
    match blocking(&state, operation).await {
        Ok(outcome) => committed(&state, outcome, reset).await,
        Err(refusal) => refusal,
    }
}

#[cfg(not(unix))]
fn unsupported() -> Response {
    refuse("unsupported_platform")
}

// ---------------------------------------------------------------------------
// Handlers. Each extracts only infallible parts, authenticates, then parses.
// ---------------------------------------------------------------------------

/// `POST /v1/extensions/install`: acquire into quarantine WITHOUT the state
/// lock, then publish under it. Install grants nothing and starts nothing.
pub(crate) async fn install(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let request: InstallRequest = match intake(&state, &headers, None, body).await {
        Ok((_, request)) => request,
        Err(code) => return refuse(code),
    };
    #[cfg(unix)]
    {
        let source = request.source;
        let expected = request.expected_state_revision;
        detached(commit_and_reconcile(state, None, move |writer| {
            let quarantine = acquire(&writer, &source)?;
            writer.install(expected, quarantine)
        }))
        .await
    }
    #[cfg(not(unix))]
    {
        let _ = (state, request);
        unsupported()
    }
}

/// `POST /v1/extensions/{id}/update`: requires the package fully disabled and
/// stopped; the replacement digest is untrusted until a separate trust.
pub(crate) async fn update(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    body: Body,
) -> Response {
    let (id, request): (Option<String>, UpdateRequest) =
        match intake(&state, &headers, Some(path), body).await {
            Ok(parsed) => parsed,
            Err(code) => return refuse(code),
        };
    let id = id.unwrap_or_default();
    #[cfg(unix)]
    {
        if ocean_extension::validate_extension_id(&id).is_err() {
            return refuse("invalid_extension_id");
        }
        let source = request.source;
        let expected = request.expected_state_revision;
        let activity = activity(&state);
        detached(commit_and_reconcile(
            state,
            Some(ActivationReset::Reconfigured),
            move |writer| {
                let quarantine = acquire(&writer, &source)?;
                writer.update(&id, expected, quarantine, &activity)
            },
        ))
        .await
    }
    #[cfg(not(unix))]
    {
        let _ = (state, id, request);
        unsupported()
    }
}

/// `POST /v1/extensions/{id}/trust`: preview without `confirm_grant_diff`
/// (mutates nothing, HTTP 200 `applied:false`), apply with the exact
/// confirmation (common committed envelope).
pub(crate) async fn trust(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    body: Body,
) -> Response {
    let (id, request): (Option<String>, TrustBody) =
        match intake(&state, &headers, Some(path), body).await {
            Ok(parsed) => parsed,
            Err(code) => return refuse(code),
        };
    let id = id.unwrap_or_default();
    #[cfg(unix)]
    {
        detached(async move {
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
                Ok(TrustResult::Applied(outcome)) => {
                    committed(&state, outcome, Some(ActivationReset::Reconfigured)).await
                }
                Err(refusal) => refusal,
            }
        })
        .await
    }
    #[cfg(not(unix))]
    {
        let _ = (state, id, request);
        unsupported()
    }
}

/// `POST /v1/extensions/{id}/enable`.
pub(crate) async fn enable(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    body: Body,
) -> Response {
    scope_mutation(state, headers, path, body, true).await
}

/// `POST /v1/extensions/{id}/disable`: commits the filter removal, then
/// answers 200 only after the supervisor has stopped and reaped any service
/// that is no longer effective; a bounded reap failure is the committed 202.
pub(crate) async fn disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    body: Body,
) -> Response {
    scope_mutation(state, headers, path, body, false).await
}

async fn scope_mutation(
    state: AppState,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    body: Body,
    enable: bool,
) -> Response {
    let (id, request): (Option<String>, ScopeMutationRequest) =
        match intake(&state, &headers, Some(path), body).await {
            Ok(parsed) => parsed,
            Err(code) => return refuse(code),
        };
    let id = id.unwrap_or_default();
    #[cfg(unix)]
    {
        detached(async move {
            let projects = match registered_projects(&state).await {
                Ok(projects) => projects,
                Err(refusal) => return refusal,
            };
            let expected = request.expected_state_revision;
            let scope = enablement_scope(request.scope);
            // Disable may end effectiveness; enable never needs a reset (a
            // restart it requires shows up as a changed descriptor).
            let reset = (!enable).then_some(ActivationReset::Disabled);
            commit_and_reconcile(state, reset, move |writer| {
                if enable {
                    writer.enable(&id, expected, scope, &projects)
                } else {
                    writer.disable(&id, expected, scope, &projects)
                }
            })
            .await
        })
        .await
    }
    #[cfg(not(unix))]
    {
        let _ = (state, id, request, enable);
        unsupported()
    }
}

/// `DELETE /v1/extensions/{id}`: requires every scope disabled and the
/// package already reaped; revokes install, trust, acknowledgement,
/// bindings, and enablement.
pub(crate) async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<String>, PathRejection>,
    body: Body,
) -> Response {
    let (id, request): (Option<String>, RemoveRequest) =
        match intake(&state, &headers, Some(path), body).await {
            Ok(parsed) => parsed,
            Err(code) => return refuse(code),
        };
    let id = id.unwrap_or_default();
    #[cfg(unix)]
    {
        let expected = request.expected_state_revision;
        let purge = request.purge_state;
        let activity = activity(&state);
        detached(commit_and_reconcile(
            state,
            Some(ActivationReset::Disabled),
            move |writer| writer.remove(&id, expected, purge, &activity),
        ))
        .await
    }
    #[cfg(not(unix))]
    {
        let _ = (state, id, request);
        unsupported()
    }
}

#[cfg(test)]
mod tests;
