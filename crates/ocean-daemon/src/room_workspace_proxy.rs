//! The room's Bedrock workspace, reachable from a browser without the browser
//! ever holding the room's bearer.
//!
//! Bedrock serves a room's container workspace and its repo binding under
//! `/api/v1/rooms/{room}/workspace...`, authenticated by a per-room bearer that
//! lives in `ocean-store`'s `RoomCredential` and must never leave this process.
//! Until now nothing in the daemon spoke those routes, so the only way a UI
//! could reach them was to be handed the bearer — which is the one thing that
//! cannot happen. This module is the lane that removes the need.
//!
//! It owns exactly three things and deliberately no more:
//!
//! 1. **The allowlist.** `resolve_workspace_call` is a total function over
//!    `(method, leaf)`. Anything it does not name is refused with a typed code
//!    and no request leaves the daemon. Bedrock's compute surface is much wider
//!    than what is named here — provision, destroy, repo bind, repo unbind,
//!    file read/write/delete, mkdir, flush, hydrate, port exposure, and
//!    `workspace/secrets` all exist upstream and are all absent on purpose.
//!    Secrets in particular: even the NAME list is room configuration, and a
//!    caller-asserted lane is not where that belongs. Provision, destroy, and
//!    the two repo BINDING verbs are owner-only upstream (`requireRoomOwner`)
//!    and shape infrastructure every other member then shares; they want an
//!    operator path, not this one. That exclusion is not a preference: this
//!    daemon always presents the CREDENTIAL's bearer, so Bedrock evaluates its
//!    owner check against the local human rather than against the asserted
//!    `?actor_id=`. On a room whose local human is the owner — the ordinary
//!    case — an allowlist row for bind would hand every roster participant the
//!    authority to repoint the whole room's compute at an arbitrary remote.
//!    Cloning and building what an owner already bound is a member act and
//!    stays; choosing what the room builds is not. `workspace/file` answers
//!    with raw bytes rather than JSON and needs the content-type discipline
//!    `room_attachments.rs` already worked out for downloads — its own slice,
//!    not a line in this table.
//! 2. **The membership gate.** The caller asserts a room participant in
//!    `?actor_id=`; that claim is checked against the roster inside the SAME
//!    store guard that reads the credential, so a concurrent roster replacement
//!    cannot race the authorization. Write verbs additionally refuse an
//!    Agent/System identity, exactly as `enforce_client_artifact_author` does —
//!    an agent's command is run by the daemon, not by a client wearing its name.
//! 3. **Actor attribution.** Bedrock's write routes demand `actor_member_id`
//!    and prove the calling principal owns it. The only member id this daemon
//!    can honestly claim is the credential's `local_human_member_id`, so that
//!    is what gets sent. A client-supplied `actor_member_id` is STRIPPED from
//!    every forwarded body before anything else happens, unconditionally rather
//!    than only where it is replaced: every authored Bedrock payload is strict
//!    deny-extra, so a claim surviving onto a row this table later adds would
//!    turn a legal call into a 400 instead of an attribution.
//!
//! What this module does not own: SQL (that is `ocean-store`), compute
//! semantics (that is Bedrock, whose `gateWorkspaceAccess` still runs on every
//! forwarded call — this lane narrows, it never substitutes), and the HTTP
//! client (that is `room_federation.rs`, through the single `send_room_scoped`
//! seam, whose longer-waiting client is built beside the control-plane one and
//! hardened identically). There is no `reqwest::Client` here and there must not
//! be one.
//!
//! The room key never becomes a path component locally, but it does become one
//! upstream; `send_room_scoped` builds that path from the CREDENTIAL's room id
//! rather than from anything on the wire, so the confinement holds even if this
//! module were wrong about which room it is talking about.

use std::{collections::HashMap, time::Duration};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use ocean_core::{RoomAccessState, RoomKey, RoomParticipantKind};
use ocean_store::{RoomCredential, RoomStore};
use serde_json::{json, Value};

use crate::persistent_rooms::with_rooms;
use crate::room_federation::{IntentError, RelayBudget, ROOM_SCOPED_READ_TIMEOUT};
use crate::AppState;

/// Ceiling on a Bedrock workspace reply the daemon will relay.
///
/// Deliberately far above `room_federation`'s 64 KiB `BODY_LIMIT`, and for a
/// concrete reason rather than comfort: Bedrock caps exec stdout and stderr at
/// 256 KiB EACH (`src/compute/driver.mjs`, `EXEC_OUTPUT_CAP_BYTES`), so a
/// perfectly ordinary `npm test` answer is already several times the bound that
/// fits a ledger row. Relaying at 64 KiB would 502 the success case of the
/// route this lane exists to serve.
///
/// The raw cap is not the number to size against, though. What crosses this
/// wire is that output JSON-ESCAPED, and one source byte can cost six
/// (`\u001b` — an ESC, which is precisely what ANSI-coloured test and build
/// output is made of). So the bound has to clear SIX times both caps, plus the
/// envelope; that is the same factor and the same reasoning
/// `room_federation`'s own `OUTBOUND_MESSAGE_BODY_LIMIT` is sized by, and the
/// relationship is checked below rather than remembered. It is still a bound —
/// nothing streams.
const WORKSPACE_RESPONSE_LIMIT: usize = 4 * 1024 * 1024;

/// Ceiling on the JSON body this lane will forward.
///
/// A workspace request is a command line, a repo remote, or a build script
/// name. 32 KiB is generous for all of them and keeps a forwarded body far
/// under what the upstream reply bound allows, so the two limits can never be
/// confused for each other.
///
/// Enforced twice, the same way `room_attachments.rs` enforces its cap: the
/// route's `DefaultBodyLimit` refuses anything far over it without buffering,
/// and [`shape_body`] refuses anything over it with a typed
/// `workspace_request_too_large`.
pub(super) const WORKSPACE_REQUEST_LIMIT: usize = 32 * 1024;

/// How far above [`WORKSPACE_REQUEST_LIMIT`] the route's body limit sits, so a
/// body a little over the cap still reaches the handler and gets the typed JSON
/// rejection instead of axum's untyped 413.
pub(super) const BODY_LIMIT_SLACK: usize = 4096;

/// Bedrock's own exec output cap, applied to stdout AND stderr separately
/// (`EXEC_OUTPUT_CAP_BYTES` in ocean-bedrock's `src/compute/driver.mjs`).
/// Recorded here so the relationship below is checked rather than remembered.
const BEDROCK_EXEC_OUTPUT_CAP: usize = 2 * 256 * 1024;

/// The largest command budget Bedrock's driver will honour
/// (`EXEC_TIMEOUT_MAX_MS`, same file). `repo/build` overrides the 120s exec
/// DEFAULT with 600s of its own (`src/room-build.mjs`), and this ceiling still
/// applies on top of it, so this one number bounds every command on the lane.
const BEDROCK_EXEC_TIMEOUT_MAX: Duration = Duration::from_secs(900);

/// How long the daemon waits for a workspace READ. These answer out of
/// Bedrock's own state in one round trip; nothing runs in a container first.
const WORKSPACE_READ_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the daemon waits for a workspace COMMAND.
///
/// The one number this module could not inherit. `room_federation`'s 15s
/// `REQUEST_TIMEOUT` is right for a control-plane call that answers out of a
/// database; an exec runs an arbitrary command in a container and then flushes
/// the workspace back to Bedrock before it answers, and Bedrock will wait
/// [`BEDROCK_EXEC_TIMEOUT_MAX`] for the command alone. Relaying at 15s would
/// 503 `npm test` — the exact call this lane exists to carry — 8x under
/// Bedrock's DEFAULT exec budget, and a client's natural retry of that 503
/// would run the command a second time, because `recordExecStart` upstream
/// takes no idempotency key. Above the upstream ceiling, a timeout here means
/// Bedrock itself failed to answer within a window larger than anything it
/// will run, rather than "your command is probably still going".
const WORKSPACE_COMMAND_TIMEOUT: Duration = Duration::from_secs(960);

// The relationships the bounds above only work because of, compile-time so a
// future edit to any of them fails the build rather than quietly turning the
// lane's most important success case into a 502 or a 503.
const _: () = {
    // Six wire bytes for one source byte, worst case, on BOTH of Bedrock's
    // output caps — and the envelope still has to fit after that.
    assert!(BEDROCK_EXEC_OUTPUT_CAP * 6 < WORKSPACE_RESPONSE_LIMIT);
    assert!(WORKSPACE_REQUEST_LIMIT < WORKSPACE_RESPONSE_LIMIT);
    // A command must be able to outlast the longest thing Bedrock will run for
    // it, or the lane refuses work the upstream was still legally doing.
    assert!(WORKSPACE_COMMAND_TIMEOUT.as_secs() > BEDROCK_EXEC_TIMEOUT_MAX.as_secs());
    // And nothing may be asked of the transport that its client's own read
    // bound would cut first, which would make every number above a fiction.
    assert!(WORKSPACE_COMMAND_TIMEOUT.as_secs() <= ROOM_SCOPED_READ_TIMEOUT.as_secs());
    assert!(WORKSPACE_READ_TIMEOUT.as_secs() <= ROOM_SCOPED_READ_TIMEOUT.as_secs());
};

/// The key a client is never allowed to assert on this lane.
const ACTOR_MEMBER_ID: &str = "actor_member_id";

/// One entry in the allowlist: the upstream call a daemon route is permitted to
/// become, and the handling it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkspaceCall {
    /// Upstream method, typed. It matches the daemon-side verb on every row
    /// this table currently carries; it stays a field rather than a conversion
    /// because the table is where a divergence has to be written down, and
    /// `cors.rs` makes one likely — its `cors_allowed_methods` does not
    /// advertise PUT, so any upstream PUT this lane ever exposes has to arrive
    /// on the wire as something else.
    upstream: UpstreamMethod,
    /// Path segments appended after `/api/v1/rooms/{room}/`.
    segments: &'static [&'static str],
    /// How long the daemon will wait for this particular call. A read answers
    /// out of Bedrock's state; a command runs in a container first.
    timeout: Duration,
    /// A write verb: refuses a claimed Agent/System identity.
    write: bool,
    /// Bedrock requires `actor_member_id` on this route and proves ownership of
    /// it; the daemon supplies its own, never the client's.
    attributed: bool,
    /// Query keys relayed upstream. Everything else on the wire is dropped —
    /// including `actor_id`, which is this daemon's parameter and means nothing
    /// to Bedrock.
    query: &'static [&'static str],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpstreamMethod {
    Get,
    Post,
}

impl UpstreamMethod {
    fn as_reqwest(self) -> reqwest::Method {
        match self {
            Self::Get => reqwest::Method::GET,
            Self::Post => reqwest::Method::POST,
        }
    }
}

/// The allowlist, as a table so the whole reachable surface is one screen.
///
/// Keyed by the DAEMON-side `(method, leaf)`, where the leaf is what follows
/// `/v1/rooms/persistent/{key}/workspace/` and is empty for the status route.
/// A tuple absent from this table is unreachable; see
/// `the_allowlist_is_the_whole_reachable_surface` for the tripwire that makes
/// growing it a deliberate act.
const WORKSPACE_ALLOWLIST: &[(&str, &str, WorkspaceCall)] = &[
    (
        "GET",
        "",
        WorkspaceCall {
            upstream: UpstreamMethod::Get,
            segments: &["workspace"],
            timeout: WORKSPACE_READ_TIMEOUT,
            write: false,
            attributed: false,
            query: &[],
        },
    ),
    (
        "GET",
        "list",
        WorkspaceCall {
            upstream: UpstreamMethod::Get,
            segments: &["workspace", "list"],
            timeout: WORKSPACE_READ_TIMEOUT,
            write: false,
            attributed: false,
            query: &["path"],
        },
    ),
    (
        "GET",
        "execs",
        WorkspaceCall {
            upstream: UpstreamMethod::Get,
            segments: &["workspace", "execs"],
            timeout: WORKSPACE_READ_TIMEOUT,
            write: false,
            attributed: false,
            query: &["limit"],
        },
    ),
    (
        "GET",
        "repo",
        WorkspaceCall {
            upstream: UpstreamMethod::Get,
            segments: &["workspace", "repo"],
            timeout: WORKSPACE_READ_TIMEOUT,
            write: false,
            attributed: false,
            query: &[],
        },
    ),
    (
        "POST",
        "exec",
        WorkspaceCall {
            upstream: UpstreamMethod::Post,
            segments: &["workspace", "exec"],
            timeout: WORKSPACE_COMMAND_TIMEOUT,
            write: true,
            attributed: true,
            query: &[],
        },
    ),
    // Clone and build run against the remote an owner already chose, which is
    // why they are member acts and the two binding verbs this table excludes
    // are not.
    (
        "POST",
        "repo/clone",
        WorkspaceCall {
            upstream: UpstreamMethod::Post,
            segments: &["workspace", "repo", "clone"],
            timeout: WORKSPACE_COMMAND_TIMEOUT,
            write: true,
            attributed: true,
            query: &[],
        },
    ),
    (
        "POST",
        "repo/build",
        WorkspaceCall {
            upstream: UpstreamMethod::Post,
            segments: &["workspace", "repo", "build"],
            timeout: WORKSPACE_COMMAND_TIMEOUT,
            write: true,
            attributed: true,
            query: &[],
        },
    ),
];

/// The allowlist lookup. `None` is the refusal, and it is the default for
/// everything the table does not name.
fn resolve_workspace_call(method: &str, leaf: &str) -> Option<WorkspaceCall> {
    WORKSPACE_ALLOWLIST
        .iter()
        .find(|(allowed_method, allowed_leaf, _)| {
            *allowed_method == method && *allowed_leaf == leaf
        })
        .map(|(_, _, call)| *call)
}

/// Why a call was refused before anything left the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateError {
    /// The asserted `actor_id` is missing or empty.
    MissingActor,
    /// No such room.
    RoomNotFound,
    /// The asserted participant is not on this room's roster.
    NotMember,
    /// A client claimed an Agent/System identity for a write.
    ForgedActor,
    /// The room has no `RoomCredential` — it is local-only, so there is no
    /// Bedrock workspace to reach and no bearer to reach it with.
    NotFederated,
    /// Access was revoked; the room's federation gate is closed.
    Revoked,
    Store,
}

fn gate_error_response(error: GateError) -> Response {
    let (status, code, message) = match error {
        GateError::MissingActor => (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "a workspace call must assert the room participant making it (?actor_id=)",
        ),
        GateError::RoomNotFound => (StatusCode::NOT_FOUND, "room_not_found", "unknown room"),
        GateError::NotMember => (
            StatusCode::FORBIDDEN,
            "not_a_room_member",
            "the asserted actor is not on this room's roster",
        ),
        GateError::ForgedActor => (
            StatusCode::FORBIDDEN,
            "forged_workspace_actor",
            "an agent's workspace command is run by the daemon, not by a client claiming its identity",
        ),
        GateError::NotFederated => (
            StatusCode::CONFLICT,
            "room_not_federated",
            "this room has no Bedrock credential, so it has no workspace",
        ),
        GateError::Revoked => (
            StatusCode::FORBIDDEN,
            "room_access_revoked",
            "this room's federation access was revoked",
        ),
        GateError::Store => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "store_error",
            "the room store could not be read",
        ),
    };
    (
        status,
        Json(json!({"ok": false, "code": code, "error": message})),
    )
        .into_response()
}

fn route_not_allowed_response(leaf: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "ok": false,
            "code": "workspace_route_not_allowed",
            "error": "this workspace route is not exposed through the daemon",
            // The leaf, not the method+leaf pair: a client debugging a typo
            // wants to see what it asked for, and echoing the daemon's own
            // verb adds nothing it did not already know.
            "leaf": leaf,
        })),
    )
        .into_response()
}

fn intent_error_response(error: IntentError) -> Response {
    let (status, code) = match error {
        // The room has a credential and passed the roster gate, so a local
        // "invalid"/"not found" cannot originate below this point; both mean
        // the bridge said something this daemon cannot represent.
        IntentError::Invalid | IntentError::NotFound | IntentError::Protocol => {
            (StatusCode::BAD_GATEWAY, "workspace_upstream_protocol")
        }
        IntentError::Conflict => (StatusCode::CONFLICT, "workspace_conflict"),
        IntentError::Forbidden | IntentError::InviteForbidden => {
            (StatusCode::FORBIDDEN, "workspace_forbidden")
        }
        IntentError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "workspace_unavailable"),
        IntentError::Store => (StatusCode::INTERNAL_SERVER_ERROR, "store_error"),
    };
    (
        status,
        Json(
            json!({"ok": false, "code": code, "error": "the room workspace could not be reached"}),
        ),
    )
        .into_response()
}

/// Roster-check the asserted actor and read the room's credential under ONE
/// store guard, so a roster edit landing between the two cannot authorize a
/// call the roster no longer permits.
///
/// The returned [`RoomCredential`] carries the bearer. It goes straight to
/// `send_room_scoped` and nowhere else: it is never logged, never rendered into
/// an error, and the type deliberately has no `Serialize`.
fn gate_workspace_call(
    state: &AppState,
    key: &RoomKey,
    actor_id: &str,
    write: bool,
) -> Result<RoomCredential, GateError> {
    with_rooms(state, |store| {
        let record = match store.get(key) {
            Ok(Some(record)) => record,
            Ok(None) => return Err(GateError::RoomNotFound),
            Err(_) => return Err(GateError::Store),
        };
        let kind = record
            .room
            .participants
            .iter()
            .find(|participant| participant.id == actor_id)
            .map(|participant| participant.kind)
            .ok_or(GateError::NotMember)?;
        if write
            && matches!(
                kind,
                RoomParticipantKind::Agent | RoomParticipantKind::System
            )
        {
            return Err(GateError::ForgedActor);
        }
        let credential = match store.room_credential(key) {
            Ok(Some(credential)) => credential,
            Ok(None) => return Err(GateError::NotFederated),
            Err(_) => return Err(GateError::Store),
        };
        match store.room_access(key) {
            Ok(access) if access.state == RoomAccessState::Revoked => Err(GateError::Revoked),
            Ok(_) => Ok(credential),
            Err(_) => Err(GateError::Store),
        }
    })
}

/// The one path every workspace call takes: allowlist, gate, shape, forward.
///
/// Ordered so that nothing reaches the network until every local refusal has
/// had its say — an unknown leaf never touches the store, and a non-member
/// never causes a request.
async fn forward(
    state: AppState,
    key: String,
    leaf: &str,
    method: &str,
    params: HashMap<String, String>,
    body: Option<Value>,
) -> Response {
    let Some(call) = resolve_workspace_call(method, leaf) else {
        return route_not_allowed_response(leaf);
    };
    let key = RoomKey::new(key.trim());
    let actor_id = params
        .get("actor_id")
        .map(|actor| actor.trim())
        .filter(|actor| !actor.is_empty());
    let Some(actor_id) = actor_id else {
        return gate_error_response(GateError::MissingActor);
    };

    let credential = match gate_workspace_call(&state, &key, actor_id, call.write) {
        Ok(credential) => credential,
        Err(error) => return gate_error_response(error),
    };

    let body = match shape_body(body, &call, &credential) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let query: Vec<(&str, String)> = call
        .query
        .iter()
        .filter_map(|name| params.get(*name).map(|value| (*name, value.clone())))
        .collect();

    match state
        .room_federation
        .send_room_scoped(
            &credential,
            call.upstream.as_reqwest(),
            call.segments,
            &query,
            body.as_ref(),
            RelayBudget {
                body_limit: WORKSPACE_RESPONSE_LIMIT,
                timeout: call.timeout,
            },
        )
        .await
    {
        // Bedrock's own status and typed body are relayed verbatim: its
        // `workspace_absent` / `repo_not_cloned` / `repo_cloning` codes are the
        // whole reason a UI can say something useful, and re-coding them here
        // would only lose information. No upstream HEADER is relayed.
        Ok((status, payload)) => (status, Json(payload)).into_response(),
        Err(error) => intent_error_response(error),
    }
}

/// Strip the client's actor claim, install the daemon's where the route needs
/// one, and refuse a body too large to forward.
fn shape_body(
    body: Option<Value>,
    call: &WorkspaceCall,
    credential: &RoomCredential,
) -> Result<Option<Value>, Box<Response>> {
    let Some(mut body) = body else {
        return Ok(None);
    };
    let Some(object) = body.as_object_mut() else {
        return Err(Box::new(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "ok": false,
                    "code": "invalid_request",
                    "error": "a workspace request body must be a JSON object",
                })),
            )
                .into_response(),
        ));
    };
    // Unconditional, and before the insert rather than instead of it: on an
    // attributed route the daemon's id must be the one that lands, and on every
    // other route Bedrock rejects the key as stray.
    object.remove(ACTOR_MEMBER_ID);
    if call.attributed {
        object.insert(
            ACTOR_MEMBER_ID.into(),
            Value::String(credential.local_human_member_id.clone()),
        );
    }
    let encoded = serde_json::to_vec(&body)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    if encoded > WORKSPACE_REQUEST_LIMIT {
        return Err(Box::new(
            (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({
                    "ok": false,
                    "code": "workspace_request_too_large",
                    "error": "a workspace request body must fit in 32 KiB",
                })),
            )
                .into_response(),
        ));
    }
    Ok(Some(body))
}

/// `GET /v1/rooms/persistent/{key}/workspace` — the room's container status.
///
/// Its own registration because axum's `{*leaf}` never matches an empty
/// segment; it resolves through the same allowlist under the empty leaf, so
/// there is exactly one place a route becomes an upstream call.
pub(super) async fn room_workspace_status(
    State(state): State<AppState>,
    Path(key): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    forward(state, key, "", "GET", params, None).await
}

/// `GET /v1/rooms/persistent/{key}/workspace/{*leaf}` — reads.
pub(super) async fn room_workspace_read(
    State(state): State<AppState>,
    Path((key, leaf)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    forward(state, key, &leaf, "GET", params, None).await
}

/// `POST /v1/rooms/persistent/{key}/workspace/{*leaf}` — commands and repo
/// binding. Every allowlisted POST leaf carries a JSON object upstream, so the
/// body is required rather than optional.
pub(super) async fn room_workspace_command(
    State(state): State<AppState>,
    Path((key, leaf)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    Json(body): Json<Value>,
) -> Response {
    forward(state, key, &leaf, "POST", params, Some(body)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistent_rooms::RoomStoreHandle;
    use crate::room_federation::FederationSupervisor;
    use crate::tests::fake_convene_state;
    use axum::{
        body::Bytes,
        http::{HeaderMap, Method, Uri},
        routing::{get, post},
        Router,
    };
    use chrono::Utc;
    use http_body_util::BodyExt;
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    const BEARER: &str = "room-bearer-under-test";
    const LOCAL_MEMBER: &str = "member-local-human";

    #[derive(Clone, Debug)]
    struct SeenCall {
        method: String,
        path: String,
        query: String,
        authorization: Option<String>,
        body: Value,
    }

    /// Every call the fake Bedrock saw, so a refusal test can assert the
    /// stronger property — that nothing was forwarded at all — rather than
    /// merely that the daemon answered 403.
    #[derive(Clone, Default)]
    struct Seen {
        calls: Arc<StdMutex<Vec<SeenCall>>>,
    }

    impl Seen {
        fn calls(&self) -> Vec<SeenCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    async fn record_call(
        State(seen): State<Seen>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Json<Value> {
        seen.calls.lock().unwrap().push(SeenCall {
            method: method.to_string(),
            path: uri.path().to_string(),
            query: uri.query().unwrap_or_default().to_string(),
            authorization: headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        });
        Json(json!({"ok": true}))
    }

    /// How long the deliberately slow leaf takes to answer, and the two
    /// budgets either side of it. Small enough to keep the suite fast, and the
    /// only thing under test is that the caller's number reaches the wire —
    /// the real budgets are checked against Bedrock's own at compile time.
    const SLOW_CALL: Duration = Duration::from_millis(1_200);
    const UNDER_SLOW_CALL: Duration = Duration::from_millis(250);
    const OVER_SLOW_CALL: Duration = Duration::from_secs(10);

    /// A leaf that answers late. Nothing else here does, which is exactly why
    /// the relay bound went unnoticed: a fake that always answers instantly
    /// cannot see a timeout at all.
    async fn slow_call() -> Json<Value> {
        tokio::time::sleep(SLOW_CALL).await;
        Json(json!({"ok": true}))
    }

    /// A Bedrock stand-in that answers every allowlisted leaf and records what
    /// it was asked. It deliberately ALSO serves `workspace/secrets`,
    /// `workspace/file`, and repo bind/unbind, so a test asserting those are
    /// unreachable is proving the daemon's allowlist rather than an upstream
    /// 404.
    async fn start_fake_bedrock(seen: Seen) -> (String, JoinHandle<()>) {
        let app = Router::new()
            .route("/api/v1/rooms/{room}/workspace", get(record_call))
            .route("/api/v1/rooms/{room}/workspace/list", get(record_call))
            .route("/api/v1/rooms/{room}/workspace/execs", get(record_call))
            .route("/api/v1/rooms/{room}/workspace/exec", post(record_call))
            .route(
                "/api/v1/rooms/{room}/workspace/secrets",
                get(record_call).put(record_call),
            )
            .route("/api/v1/rooms/{room}/workspace/file", get(record_call))
            .route("/api/v1/rooms/{room}/workspace/slow", get(slow_call))
            .route(
                "/api/v1/rooms/{room}/workspace/repo",
                get(record_call).put(record_call).delete(record_call),
            )
            .route(
                "/api/v1/rooms/{room}/workspace/repo/clone",
                post(record_call),
            )
            .route(
                "/api/v1/rooms/{room}/workspace/repo/build",
                post(record_call),
            )
            .with_state(seen);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), task)
    }

    /// Everything a test needs to drive the lane, plus the handles that shut it
    /// down. Named rather than a bare tuple because five of them is where a
    /// tuple stops being readable.
    struct Fixture {
        state: AppState,
        key: RoomKey,
        seen: Seen,
        server: JoinHandle<()>,
        shutdown: CancellationToken,
    }

    impl Fixture {
        fn close(self) {
            self.shutdown.cancel();
            self.server.abort();
        }
    }

    /// A federated room with a Human and an Agent on the roster, a credential,
    /// and a supervisor pointed at the fake Bedrock. Written straight through
    /// the store: these tests are about the proxy, and routing the fixture
    /// through the join/register endpoints would make a change there fail here.
    async fn federated_room(tmp: &tempfile::TempDir) -> Fixture {
        let mut state = fake_convene_state(tmp);
        let key = RoomKey::new("workspace-room");
        with_rooms(&state, |store| {
            store
                .create(key.clone(), key.as_str(), None, Utc::now())
                .expect("room fixture");
            for (id, name, kind) in [
                ("alice", "Alice", RoomParticipantKind::Human),
                ("researcher", "Researcher", RoomParticipantKind::Agent),
            ] {
                store
                    .add_participant(
                        &key,
                        ocean_core::RoomParticipant {
                            id: id.into(),
                            kind,
                            display_name: name.into(),
                        },
                        Utc::now(),
                    )
                    .expect("roster fixture");
            }
            store
                .install_room_credential(&key, BEARER, LOCAL_MEMBER)
                .expect("credential fixture");
        });

        let seen = Seen::default();
        let (base, server) = start_fake_bedrock(seen.clone()).await;
        let shutdown = CancellationToken::new();
        let rooms: RoomStoreHandle = state.rooms.clone();
        state.room_federation = FederationSupervisor::for_test(
            &base,
            rooms,
            state.room_wakes.clone(),
            state.room_access_wakes.clone(),
            state.room_read_cursor_wakes.clone(),
            shutdown.clone(),
            std::time::Duration::from_secs(3600),
        );
        Fixture {
            state,
            key,
            seen,
            server,
            shutdown,
        }
    }

    fn query(pairs: &[(&str, &str)]) -> Query<HashMap<String, String>> {
        Query(
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
        )
    }

    async fn body_of(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    /// Refusal 1 of 3. A caller asserting an id that is not on the roster is
    /// refused, and — the part that matters — nothing is forwarded, so the
    /// bearer is never spent on their behalf.
    ///
    /// Mutation: drop the `NotMember` arm from `gate_workspace_call` -> RED.
    #[tokio::test]
    async fn a_non_member_is_refused_and_nothing_is_forwarded() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_status(
            State(fixture.state.clone()),
            Path(fixture.key.as_str().to_string()),
            query(&[("actor_id", "mallory")]),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("not_a_room_member"));
        assert!(
            fixture.seen.calls().is_empty(),
            "a refused caller must not cause an upstream request"
        );

        // The same route works for a roster member, so the refusal above is the
        // gate and not a broken fixture.
        let response = room_workspace_status(
            State(fixture.state.clone()),
            Path(fixture.key.as_str().to_string()),
            query(&[("actor_id", "alice")]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(fixture.seen.calls().len(), 1);

        fixture.close();
    }

    /// Refusal 2 of 3. A room with no `RoomCredential` is local-only: there is
    /// no workspace to reach and no bearer to reach it with, so the lane fails
    /// closed rather than reaching for the bootstrap owner token.
    ///
    /// Mutation: fall back to any other credential source -> RED.
    #[tokio::test]
    async fn a_room_without_a_credential_is_refused_and_nothing_is_forwarded() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;
        let local = RoomKey::new("local-only-room");
        with_rooms(&fixture.state, |store| {
            store
                .create(local.clone(), local.as_str(), None, Utc::now())
                .expect("room fixture");
            store
                .add_participant(
                    &local,
                    ocean_core::RoomParticipant {
                        id: "alice".into(),
                        kind: RoomParticipantKind::Human,
                        display_name: "Alice".into(),
                    },
                    Utc::now(),
                )
                .expect("roster fixture");
        });

        let response = room_workspace_status(
            State(fixture.state.clone()),
            Path(local.as_str().to_string()),
            query(&[("actor_id", "alice")]),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], json!("room_not_federated"));
        assert!(
            fixture.seen.calls().is_empty(),
            "a room with no credential must not cause an upstream request"
        );

        fixture.close();
    }

    /// Refusal 3 of 3. The allowlist is what makes this a lane and not a proxy.
    /// `workspace/secrets` is the case that matters most: it exists upstream,
    /// the fake Bedrock here serves it, and it is still unreachable. `GET exec`
    /// and `POST list` prove the METHOD half of the key refuses too — a leaf
    /// being allowlisted for one verb does not open it for another.
    ///
    /// Mutation: make `resolve_workspace_call` fall through to a constructed
    /// `WorkspaceCall` for unknown keys -> RED.
    #[tokio::test]
    async fn a_call_the_allowlist_does_not_name_is_refused_and_nothing_is_forwarded() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        for leaf in ["secrets", "file", "exec", "repo/../secrets"] {
            let response = room_workspace_read(
                State(fixture.state.clone()),
                Path((fixture.key.as_str().to_string(), leaf.to_string())),
                query(&[("actor_id", "alice")]),
            )
            .await;
            let (status, body) = body_of(response).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "GET {leaf}");
            assert_eq!(
                body["code"],
                json!("workspace_route_not_allowed"),
                "GET {leaf}"
            );
        }

        for leaf in ["secrets", "list"] {
            let response = room_workspace_command(
                State(fixture.state.clone()),
                Path((fixture.key.as_str().to_string(), leaf.to_string())),
                query(&[("actor_id", "alice")]),
                Json(json!({})),
            )
            .await;
            let (status, body) = body_of(response).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "POST {leaf}");
            assert_eq!(
                body["code"],
                json!("workspace_route_not_allowed"),
                "POST {leaf}"
            );
        }

        assert!(
            fixture.seen.calls().is_empty(),
            "an unlisted call must not cause an upstream request"
        );

        fixture.close();
    }

    /// The bearer stays here and the actor id is the daemon's, not the
    /// client's. Both halves of the slice's reason to exist, in one assertion
    /// set, on the route that carries the most authority.
    ///
    /// Mutation: forward `body["actor_member_id"]` untouched -> RED.
    #[tokio::test]
    async fn an_exec_carries_the_daemon_bearer_and_the_daemon_actor_id() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "exec".to_string())),
            query(&[("actor_id", "alice")]),
            Json(json!({
                "command": "npm test",
                // The forgery attempt: a client naming a member id that is not
                // its own. Bedrock would 403 it, but it must never get there.
                "actor_member_id": "member-somebody-else",
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        let expected_authorization = format!("Bearer {BEARER}");
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, "/api/v1/rooms/workspace-room/workspace/exec");
        assert_eq!(
            call.authorization.as_deref(),
            Some(expected_authorization.as_str()),
            "the room's own credential authenticates the call"
        );
        assert_eq!(call.body["command"], json!("npm test"));
        assert_eq!(
            call.body[ACTOR_MEMBER_ID],
            json!(LOCAL_MEMBER),
            "the daemon's own member id replaces whatever the client claimed"
        );

        fixture.close();
    }

    /// A write verb refuses a claimed Agent identity even though that identity
    /// IS on the roster — the `enforce_client_artifact_author` rule, ported.
    /// The same identity may still read, which is what makes this a forgery
    /// gate rather than a second roster check.
    ///
    /// Mutation: drop the `write &&` guard, or the `ForgedActor` arm -> RED.
    #[tokio::test]
    async fn a_claimed_agent_identity_may_read_but_never_command() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "exec".to_string())),
            query(&[("actor_id", "researcher")]),
            Json(json!({"command": "rm -rf /"})),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("forged_workspace_actor"));
        assert!(
            fixture.seen.calls().is_empty(),
            "a forged command must not run"
        );

        let response = room_workspace_read(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "execs".to_string())),
            query(&[("actor_id", "researcher")]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(fixture.seen.calls().len(), 1);

        fixture.close();
    }

    /// Only the query keys a route declares are relayed. `actor_id` is this
    /// daemon's parameter and means nothing upstream; anything else on the wire
    /// is a client trying to steer a Bedrock handler this lane does not model.
    ///
    /// Mutation: forward `params` wholesale instead of the declared keys -> RED.
    #[tokio::test]
    async fn only_declared_query_keys_reach_bedrock() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_read(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "list".to_string())),
            query(&[
                ("actor_id", "alice"),
                ("path", "/workspace/src"),
                ("inline", "1"),
            ]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].query.contains("path=%2Fworkspace%2Fsrc"),
            "a declared key is relayed, encoded: {}",
            calls[0].query
        );
        assert!(
            !calls[0].query.contains("actor_id") && !calls[0].query.contains("inline"),
            "undeclared keys are dropped: {}",
            calls[0].query
        );

        fixture.close();
    }

    /// Repo BIND and UNBIND are owner-only upstream, and the daemon presents
    /// the credential's own bearer — so Bedrock's `requireRoomOwner` would be
    /// answered by the local human no matter which roster id the caller
    /// asserted. A row for either would hand every participant the authority to
    /// repoint the whole room's compute. The fake Bedrock here serves both, so
    /// this is the daemon refusing, not an upstream 404.
    ///
    /// Reading the binding, cloning it, and building it stay: those act on a
    /// remote an owner already chose.
    ///
    /// Mutation: put either row back in the table -> RED.
    #[tokio::test]
    async fn owner_only_repo_binding_is_not_on_this_lane() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "repo".to_string())),
            query(&[("actor_id", "alice")]),
            Json(json!({"remote": "https://github.com/example/repo.git"})),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["code"], json!("workspace_route_not_allowed"));

        // Unbind has no daemon-side verb at all: the wildcard route registers
        // GET and POST only, so axum answers the DELETE itself. Through the
        // real router, because that is the whole claim.
        let app = crate::room_routes().with_state(fixture.state.clone());
        let response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .method("DELETE")
                .uri(format!(
                    "/v1/rooms/persistent/{}/workspace/repo?actor_id=alice",
                    fixture.key.as_str()
                ))
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);

        assert!(
            fixture.seen.calls().is_empty(),
            "neither binding verb may reach Bedrock"
        );

        // The reads and the member acts around them still work, so the two
        // refusals above are the exclusion and not a broken fixture.
        let response = room_workspace_read(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "repo".to_string())),
            query(&[("actor_id", "alice")]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(fixture.seen.calls().len(), 1);

        fixture.close();
    }

    /// The relay bound is the caller's, not `room_federation`'s 15s
    /// `REQUEST_TIMEOUT` — the defect this parameter exists to close. Bedrock
    /// runs an exec for up to 900s by contract and `npm test` routinely takes
    /// minutes, so a lane that inherited the control-plane bound would 503 the
    /// success case of its most important route while every test here, against
    /// a fake that answers instantly, stayed green.
    ///
    /// Wall-clock proof that the number reaches the wire, at test speed. The
    /// relationship between the REAL budgets and Bedrock's own is a
    /// compile-time assertion beside the constants, since nothing that runs in
    /// a second can observe a 960s bound.
    ///
    /// Mutation: pass `REQUEST_TIMEOUT` (or any fixed value) instead of the
    /// caller's -> RED.
    #[tokio::test]
    async fn the_transport_waits_the_budget_its_caller_declares() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;
        let credential = with_rooms(&fixture.state, |store| store.room_credential(&fixture.key))
            .expect("credential read")
            .expect("federated fixture");

        let refused = fixture
            .state
            .room_federation
            .send_room_scoped(
                &credential,
                reqwest::Method::GET,
                &["workspace", "slow"],
                &[],
                None,
                RelayBudget {
                    body_limit: WORKSPACE_RESPONSE_LIMIT,
                    timeout: UNDER_SLOW_CALL,
                },
            )
            .await;
        assert_eq!(
            refused,
            Err(IntentError::Unavailable),
            "a budget under the answer must give up"
        );

        let (status, _) = fixture
            .state
            .room_federation
            .send_room_scoped(
                &credential,
                reqwest::Method::GET,
                &["workspace", "slow"],
                &[],
                None,
                RelayBudget {
                    body_limit: WORKSPACE_RESPONSE_LIMIT,
                    timeout: OVER_SLOW_CALL,
                },
            )
            .await
            .expect("a budget over the answer must wait for it");
        assert_eq!(status, StatusCode::OK);

        fixture.close();
    }

    /// Every command on the lane must be able to outlast Bedrock's own ceiling,
    /// and every read must not. The table is where that policy is declared, so
    /// the table is where it is asserted.
    ///
    /// Mutation: give a write row `WORKSPACE_READ_TIMEOUT` -> RED.
    #[test]
    fn a_command_may_outlast_bedrocks_ceiling_and_a_read_may_not() {
        for (method, leaf, call) in WORKSPACE_ALLOWLIST {
            if call.write {
                assert!(
                    call.timeout > BEDROCK_EXEC_TIMEOUT_MAX,
                    "{method} {leaf} would refuse a command Bedrock was still running"
                );
            } else {
                assert_eq!(
                    call.timeout, WORKSPACE_READ_TIMEOUT,
                    "{method} {leaf} is a read and should answer promptly or not at all"
                );
            }
            assert!(
                call.timeout <= ROOM_SCOPED_READ_TIMEOUT,
                "{method} {leaf} declares more than the transport will wait"
            );
        }
    }

    /// The tripwire. The allowlist IS the reachable surface, so growing it is
    /// the moment to look — the same reason the route banner carries a count.
    #[test]
    fn the_allowlist_is_the_whole_reachable_surface() {
        let mut named: Vec<String> = WORKSPACE_ALLOWLIST
            .iter()
            .map(|(method, leaf, call)| {
                format!("{method} {leaf} -> {:?} {:?}", call.upstream, call.segments)
            })
            .collect();
        named.sort();
        assert_eq!(
            named,
            vec![
                "GET  -> Get [\"workspace\"]",
                "GET execs -> Get [\"workspace\", \"execs\"]",
                "GET list -> Get [\"workspace\", \"list\"]",
                "GET repo -> Get [\"workspace\", \"repo\"]",
                "POST exec -> Post [\"workspace\", \"exec\"]",
                "POST repo/build -> Post [\"workspace\", \"repo\", \"build\"]",
                "POST repo/clone -> Post [\"workspace\", \"repo\", \"clone\"]",
            ],
            "the Bedrock surface this lane exposes changed; review the manifest"
        );
        assert!(
            !WORKSPACE_ALLOWLIST.iter().any(|(_, _, call)| {
                call.segments.contains(&"secrets")
                    || call.segments.contains(&"ports")
                    || call.segments.contains(&"file")
            }),
            "secrets, port exposure, and raw file bytes are deliberately absent"
        );
    }

    /// Through the REAL router, not a handler call.
    ///
    /// Every other test here invokes a handler directly, which proves the gate
    /// but says nothing about what axum's `{*leaf}` actually hands it — and if
    /// the capture arrived with a leading slash, every allowlist key would miss
    /// and the whole lane would 404 while these tests stayed green. It also
    /// pins the precedence question the two registrations raise: `/workspace`
    /// must reach the status route, not the wildcard.
    ///
    /// Mutation: key the allowlist on `/list` instead of `list` -> RED here,
    /// green everywhere else.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_wildcard_capture_matches_the_allowlist_keys_through_the_router() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;
        let app = crate::room_routes().with_state(fixture.state.clone());
        let room = fixture.key.as_str();

        for (uri, expected_upstream) in [
            (
                format!("/v1/rooms/persistent/{room}/workspace?actor_id=alice"),
                "/api/v1/rooms/workspace-room/workspace",
            ),
            (
                format!("/v1/rooms/persistent/{room}/workspace/execs?actor_id=alice"),
                "/api/v1/rooms/workspace-room/workspace/execs",
            ),
            (
                format!("/v1/rooms/persistent/{room}/workspace/repo?actor_id=alice"),
                "/api/v1/rooms/workspace-room/workspace/repo",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let seen = fixture.seen.calls();
            assert_eq!(
                seen.last().map(|call| call.path.as_str()),
                Some(expected_upstream),
                "{uri}"
            );
        }

        // A multi-segment leaf: `repo/clone` is one allowlist key, not a nested
        // route, so the wildcard has to deliver both segments joined.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/v1/rooms/persistent/{room}/workspace/repo/clone?actor_id=alice"
                    ))
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let seen = fixture.seen.calls();
        let call = seen.last().expect("clone forwarded");
        assert_eq!(
            call.path,
            "/api/v1/rooms/workspace-room/workspace/repo/clone"
        );
        assert_eq!(call.body[ACTOR_MEMBER_ID], json!(LOCAL_MEMBER));

        // And a leaf the allowlist does not name still gets the typed refusal
        // rather than an axum 404, which is what proves the wildcard matched.
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/rooms/persistent/{room}/workspace/secrets?actor_id=alice"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let (_, body) = body_of(response).await;
        assert_eq!(body["code"], json!("workspace_route_not_allowed"));
        assert_eq!(
            fixture.seen.calls().len(),
            4,
            "the refusal added no upstream call"
        );

        fixture.close();
    }

    /// A body that is not a JSON object is refused locally: every allowlisted
    /// POST leaf sends an object upstream, and there is nowhere to put an actor
    /// id in an array.
    #[tokio::test]
    async fn a_non_object_body_is_refused_before_anything_is_forwarded() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "exec".to_string())),
            query(&[("actor_id", "alice")]),
            Json(json!(["npm", "test"])),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], json!("invalid_request"));
        assert!(fixture.seen.calls().is_empty());

        fixture.close();
    }

    /// An unattributed caller is refused; a whitespace-only claim is the same
    /// as none, so the gate cannot be satisfied with a blank.
    #[tokio::test]
    async fn a_blank_actor_id_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_status(
            State(fixture.state.clone()),
            Path(fixture.key.as_str().to_string()),
            query(&[("actor_id", "   ")]),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], json!("invalid_request"));
        assert!(fixture.seen.calls().is_empty());

        fixture.close();
    }

    /// A revoked room's federation gate is closed, and a workspace command is
    /// exactly the side effect that must not start after it.
    #[tokio::test]
    async fn a_revoked_room_is_refused_and_nothing_is_forwarded() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;
        with_rooms(&fixture.state, |store| {
            store
                .update_room_access_safe(&fixture.key, Some(RoomAccessState::Revoked), None, None)
                .expect("revoke fixture");
        });

        let response = room_workspace_status(
            State(fixture.state.clone()),
            Path(fixture.key.as_str().to_string()),
            query(&[("actor_id", "alice")]),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("room_access_revoked"));
        assert!(fixture.seen.calls().is_empty());

        fixture.close();
    }
}
