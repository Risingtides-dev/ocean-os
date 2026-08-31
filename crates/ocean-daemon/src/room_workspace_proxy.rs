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
//! It owns exactly four things and deliberately no more:
//!
//! 1. **The allowlist.** `resolve_workspace_call` is a total function over
//!    `(method, leaf)`. Anything it does not name is refused with a typed code
//!    and no request leaves the daemon. Bedrock's compute surface is much wider
//!    than what is named here — file write/delete, mkdir, flush and hydrate all
//!    exist upstream and are all absent on purpose. Secrets were absent too,
//!    under a recorded objection — even the NAME list is room configuration,
//!    and a caller-asserted lane is not where that belongs — whose first
//!    premise the identity map below has since dissolved: the lane is no
//!    longer caller-asserted, and an owner row
//!    forwards only for the actor that RESOLVES to the credential's own
//!    principal. So the owner's SET now rides the lane (`secrets/set` — how
//!    GH_TOKEN reaches a room so `gh` can authenticate its CI pulls), and it
//!    is the set ALONE that the dissolution covers: a set answers
//!    `{set, removed, total}`, names the owner itself just asserted, and
//!    Bedrock deliberately has no route anywhere that returns a secret VALUE.
//!    The objection's second premise still stands and still binds — which
//!    secrets a room holds IS room configuration — so the member-gated name
//!    list stays off the lane, and the manifest tripwire pins every secrets
//!    row to the owner-gated set so a read-back cannot slip in as an ordinary
//!    row.
//!    The OWNER verbs — repo bind/unbind, and workspace provision/destroy —
//!    are owner-only upstream (`requireRoomOwner`) and shape infrastructure
//!    every other member then shares; they were excluded as wanting an
//!    operator path until the 2026-08-29 operator ruling opened them through
//!    this lane, owner-gated by the identity map below: they ride daemon-side
//!    POST leaves (`repo/bind`, `repo/unbind`, `provision`, `destroy` — an
//!    upstream PUT cannot arrive as a browser PUT, `cors.rs` does not
//!    advertise the method, and the workspace routes register no wire DELETE)
//!    and forward only for the actor that RESOLVES to the credential's own
//!    principal, which is exactly the principal Bedrock's `requireRoomOwner`
//!    will judge, since this daemon always presents the CREDENTIAL's bearer.
//!    Without that gate a row for bind would hand every roster participant
//!    the authority to repoint the whole room's compute at an arbitrary
//!    remote, and a row for destroy would let any participant retire the
//!    container everyone else was working in. Cloning and building what an
//!    owner already bound remains a member act. The exec ledger's take-back
//!    rides the same gate: `execs/purge` blanks stored exec tails — the
//!    recovery for a token that leaked BEFORE the write-time scrub could
//!    know it was a secret, or was rotated after a leak — and it is
//!    owner-only for Bedrock's own reason, with no admin bypass: the tails
//!    are the room's output, and only the room's owner decides they cannot
//!    be un-published. `workspace/file` is the one
//!    row whose upstream 2xx is raw bytes rather than JSON; it rides this
//!    lane as a bounded JSON PROJECTION — text-vs-binary derived from the
//!    bytes in hand, never from Bedrock's extension-derived `content-type` —
//!    so no byte ever reaches a browser as a document and no type is ever
//!    declared to it, which is why the download discipline
//!    `room_attachments.rs` worked out is not needed here. File WRITE and
//!    DELETE stay absent.
//!    PORT EXPOSURE is the one pair this table narrows on MERIT rather than by
//!    mirroring upstream. Bedrock gates expose and close at member WRITE, but
//!    the preview token it mints is derived from the room and the port and is,
//!    in its own words, "a routing label, not a credential: whatever the room
//!    serves on that port is served to anyone holding the URL". Publishing a
//!    room's compute to the open internet is an owner act, so both rows carry
//!    the owner gate instead. Narrowing is this lane's stated licence, but a
//!    narrowing that is not written down reads as a mis-set flag, so it is
//!    written down here and pinned by the manifest tripwire. Close is also the
//!    only row whose UPSTREAM PATH carries an identifier; see
//!    [`WorkspaceCall::path_from_body`].
//! 2. **The membership gate.** The caller asserts a room participant in
//!    `?actor_id=`; that claim is checked against the roster inside the SAME
//!    store guard that reads the credential, so a concurrent roster replacement
//!    cannot race the authorization. Write verbs additionally refuse an
//!    Agent/System identity, exactly as `enforce_client_artifact_author` does —
//!    an agent's command is run by the daemon, not by a client wearing its name.
//! 3. **The identity map.** `?actor_id=` is a LOCAL roster id and Bedrock
//!    speaks opaque member ids; on any route that needs one, the daemon
//!    DERIVES it and never trusts one off the wire. A Human resolves to the
//!    credential's `local_human_member_id` — this daemon serves exactly one
//!    human principal, so every browser session on it IS that principal. An
//!    Agent resolves through `room_member_bindings`, the map
//!    `register_agents` persisted when Bedrock's member envelope came back —
//!    an Agent roster id is the folder-agent name (`persistent_rooms.rs`
//!    requires it to resolve), and that name keys the binding. Everything
//!    else — Bot, Tool, System, an agent never federation-registered —
//!    resolves to nothing and is refused with a typed code, never silently
//!    attributed to the human. Owner verbs add one comparison on top: the
//!    resolved id must BE the credential's principal, closing the gap between
//!    who asserted the call and who Bedrock will believe made it.
//! 4. **Actor attribution.** Bedrock's write routes demand `actor_member_id`
//!    and prove the calling principal owns it. What gets sent is the RESOLVED
//!    id from the map above — today always the credential's
//!    `local_human_member_id`, because write verbs refuse every non-Human
//!    actor before resolution. A client-supplied `actor_member_id` is STRIPPED
//!    from every forwarded body before anything else happens, unconditionally
//!    rather than only where it is replaced: every authored Bedrock payload is
//!    strict deny-extra, so a claim surviving onto a row this table later adds
//!    would turn a legal call into a 400 instead of an attribution.
//!
//! What this module does not own: SQL (that is `ocean-store`), compute
//! semantics (that is Bedrock, whose `gateWorkspaceAccess` still runs on every
//! forwarded call — this lane narrows, it never substitutes), and the HTTP
//! client (that is `room_federation.rs`, through the sibling `send_room_scoped`
//! and `send_room_scoped_raw` seams — one shared request path — whose
//! longer-waiting client is built beside the control-plane one and hardened
//! identically). There is no `reqwest::Client` here and there must not be one.
//!
//! The room key never becomes a path component locally, but it does become one
//! upstream; both seams build that path from the CREDENTIAL's room id rather
//! than from anything on the wire, so the confinement holds even if this
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
use crate::room_federation::{IntentError, RawReply, RelayBudget, ROOM_SCOPED_READ_TIMEOUT};
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

/// Ceiling on the file bytes one `workspace/file` relay will read, and the
/// ONLY bound in the chain: Bedrock's handler buffers the whole file, the
/// local driver `readFile`s it whole, and the runtime's `/v1/files/read`
/// base64-encodes it whole — none of them cap anything. A file past this is
/// therefore a legitimate workspace state rather than an upstream fault; it
/// earns the typed `workspace_file_too_large` refusal, never the 502 a broken
/// peer earns, and it is never truncated — a panel showing most of a file as
/// if it were all of it would be lying. 1 MiB is generous for the source files
/// a panel opens, and worth bounding well under the exec relay bound: the
/// UTF-8 projection is JSON-escaped, so one file byte can cost six on the
/// browser-facing wire.
const WORKSPACE_FILE_LIMIT: usize = 1024 * 1024;

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
    /// Upstream method, typed rather than converted from the daemon-side verb,
    /// because the table is where a divergence has to be written down — and
    /// most owner rows carry one: `cors.rs`'s `cors_allowed_methods` does not
    /// advertise PUT, so Bedrock's PUT bind arrives on the wire as a POST leaf,
    /// and its DELETE unbind, destroy and port close ride the same shape rather
    /// than registering a wire DELETE. Provision is the owner verb with no
    /// divergence at all — Bedrock serves it as a POST already — which is why
    /// the tripwire pins "translated method implies owner" and not the iff it
    /// once could.
    upstream: UpstreamMethod,
    /// Path segments appended after `/api/v1/rooms/{room}/`.
    segments: &'static [&'static str],
    /// How long the daemon will wait for this particular call. The split is
    /// not "does it touch the container" — a file read does — but what an
    /// abandoned call leaves behind, which the budget test states in full. The
    /// binding owner verbs are prompt like reads — bind and unbind land in
    /// Bedrock's own table, no container runs — while the lifecycle pair
    /// carries the command budget: provision builds and hydrates the
    /// container, destroy flushes it back, before either answers. The ports
    /// pair carries it too, because an expose the daemon abandons is still a
    /// port published and a marker in the room.
    timeout: Duration,
    /// A write verb: refuses a claimed Agent/System identity.
    write: bool,
    /// An owner verb: `requireRoomOwner` upstream is judged against the
    /// principal the presented bearer speaks for, not the asserted actor — so
    /// the daemon forwards only when the actor RESOLVES to that principal,
    /// keeping a non-owner roster member from riding the owner's bearer into
    /// an owner-only route. The ports pair is the one place this gate is
    /// NARROWER than upstream rather than a mirror of it — Bedrock lets any
    /// member expose a port and this lane does not, because the preview URL
    /// that comes back is world-readable by construction — which is why the
    /// module doc rules on it and the manifest tripwire pins it.
    owner: bool,
    /// Bedrock requires `actor_member_id` on this route and proves ownership of
    /// it; the daemon supplies the actor's RESOLVED member id, never the
    /// client's claim.
    attributed: bool,
    /// Query keys relayed upstream. Everything else on the wire is dropped —
    /// including `actor_id`, which is this daemon's parameter and means nothing
    /// to Bedrock.
    query: &'static [&'static str],
    /// What this route's 2xx body is made of, and therefore which transport
    /// seam carries it. Bedrock's refusals are ordinary JSON `HttpError`
    /// bodies on EVERY route, which is what lets the raw arm still relay
    /// `workspace_absent` and friends verbatim.
    reply: UpstreamReply,
    /// A body key whose value becomes the FINAL upstream path segment, for the
    /// one upstream route that carries an identifier in its path
    /// (`DELETE workspace/ports/{port}`). `segments` cannot hold it — the
    /// table is `&'static` and the value is the caller's — so the row names
    /// the key instead and [`forward`] re-proves the value before it may
    /// become a segment. Nothing else in this module builds a path out of
    /// anything a caller sent.
    path_from_body: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpstreamMethod {
    Get,
    Post,
    Put,
    /// Carries no body upstream: Bedrock's DELETE handlers read none, so a
    /// daemon POST leaf mapping here still demands the lane's JSON object —
    /// one uniform POST contract — and then forwards nothing of it.
    Delete,
}

impl UpstreamMethod {
    fn as_reqwest(self) -> reqwest::Method {
        match self {
            Self::Get => reqwest::Method::GET,
            Self::Post => reqwest::Method::POST,
            Self::Put => reqwest::Method::PUT,
            Self::Delete => reqwest::Method::DELETE,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UpstreamReply {
    /// JSON: Bedrock's status and body relay verbatim.
    Json,
    /// Raw file bytes on a 2xx: read bounded and projected into JSON here.
    /// Whether the content rides as text or base64 is derived from the BYTES
    /// (`std::str::from_utf8`) — Bedrock's `content-type` on this route comes
    /// from the file EXTENSION (`contentTypeFor`), and this daemon acts on
    /// bytes, never declarations (`room_context.rs`: text is DERIVED, never
    /// declared).
    FileProjection,
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
            owner: false,
            attributed: false,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
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
            owner: false,
            attributed: false,
            query: &["path"],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    (
        "GET",
        "file",
        WorkspaceCall {
            upstream: UpstreamMethod::Get,
            segments: &["workspace", "file"],
            timeout: WORKSPACE_READ_TIMEOUT,
            write: false,
            owner: false,
            attributed: false,
            // `path` only: Bedrock's `inline` key steers the
            // content-disposition of a raw download, and this lane never
            // answers one.
            query: &["path"],
            reply: UpstreamReply::FileProjection,
            path_from_body: None,
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
            owner: false,
            attributed: false,
            query: &["limit"],
            reply: UpstreamReply::Json,
            path_from_body: None,
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
            owner: false,
            attributed: false,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    // The recorded CI state, answered out of Bedrock's own table — no
    // container, no gh — which is why it is a read and its sibling POST below
    // is not.
    (
        "GET",
        "repo/ci",
        WorkspaceCall {
            upstream: UpstreamMethod::Get,
            segments: &["workspace", "repo", "ci"],
            timeout: WORKSPACE_READ_TIMEOUT,
            write: false,
            owner: false,
            attributed: false,
            query: &["limit"],
            reply: UpstreamReply::Json,
            path_from_body: None,
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
            owner: false,
            attributed: true,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    // Clone and build run against the remote an owner already chose, which is
    // why they are member acts and the binding verbs below are owner-gated.
    (
        "POST",
        "repo/clone",
        WorkspaceCall {
            upstream: UpstreamMethod::Post,
            segments: &["workspace", "repo", "clone"],
            timeout: WORKSPACE_COMMAND_TIMEOUT,
            write: true,
            owner: false,
            attributed: true,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
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
            owner: false,
            attributed: true,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    // A CI pull reads too — but it reads by running gh INSIDE the container
    // on the exec path, so it carries the command budget, the daemon's
    // attribution, and the write gate like any other exec.
    (
        "POST",
        "repo/ci",
        WorkspaceCall {
            upstream: UpstreamMethod::Post,
            segments: &["workspace", "repo", "ci"],
            timeout: WORKSPACE_COMMAND_TIMEOUT,
            write: true,
            owner: false,
            attributed: true,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    // The two owner verbs the table used to exclude, opened by the 2026-08-29
    // operator ruling and gated on the identity map: forward only for the
    // actor that resolves to the credential's own principal, because that is
    // the principal Bedrock's `requireRoomOwner` judges no matter what the
    // caller asserted. `write: false` is deliberate — the owner gate is
    // strictly narrower than the write gate's Agent/System refusal, and it is
    // what makes an unmapped agent and a mapped non-principal each earn their
    // own typed code instead of one blanket forgery answer. Both answer out of
    // Bedrock's own table — no container runs — so they carry the read budget.
    // Bind's body (`remote`, `branch?`, `dir?`) is validated upstream by
    // `validateRepoBinding`, strict deny-extra, which the unconditional actor
    // strip already respects; unbind's upstream DELETE reads no body at all.
    (
        "POST",
        "repo/bind",
        WorkspaceCall {
            upstream: UpstreamMethod::Put,
            segments: &["workspace", "repo"],
            timeout: WORKSPACE_READ_TIMEOUT,
            write: false,
            owner: true,
            attributed: false,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    (
        "POST",
        "repo/unbind",
        WorkspaceCall {
            upstream: UpstreamMethod::Delete,
            segments: &["workspace", "repo"],
            timeout: WORKSPACE_READ_TIMEOUT,
            write: false,
            owner: true,
            attributed: false,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    // The workspace lifecycle, opened by the same ruling and gated the same
    // way. Unlike the binding pair these run container work — provision
    // creates the container, hydrates it from Bedrock, and restores the bound
    // checkout; destroy flushes the workspace back to Bedrock before the
    // driver tears it down (`?flush=0` skips the save) — so both carry the
    // command budget a 15s bound would 503. Provision's upstream body is
    // strict deny-extra (`spec` only), which the unconditional actor strip
    // keeps legal; destroy's upstream DELETE reads no body at all.
    (
        "POST",
        "provision",
        WorkspaceCall {
            upstream: UpstreamMethod::Post,
            segments: &["workspace"],
            timeout: WORKSPACE_COMMAND_TIMEOUT,
            write: false,
            owner: true,
            attributed: false,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    (
        "POST",
        "destroy",
        WorkspaceCall {
            upstream: UpstreamMethod::Delete,
            segments: &["workspace"],
            timeout: WORKSPACE_COMMAND_TIMEOUT,
            write: false,
            owner: true,
            attributed: false,
            query: &["flush"],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    // The owner's secrets SET — the module doc's recorded objection, answered:
    // set-only, owner-gated, and no value ever travels back (upstream has no
    // route anywhere that returns one; the reply is `{set, removed, total}`,
    // names the owner itself just asserted). The member-gated name list stays
    // off the lane. It rides a POST leaf translating Bedrock's PUT for the
    // same reason bind does — `cors.rs` does not advertise PUT — and carries
    // the read budget because the upstream is a table write behind
    // `requireLiveWorkspace`, no container run. The body (`secrets` only,
    // NAME: value-or-null, null deletes) is validated upstream strict
    // deny-extra, which the unconditional actor strip already respects; a
    // host without OCEAN_ROOM_SECRET_KEY answers 501 `secrets_unconfigured`,
    // and it relays verbatim like every other upstream refusal.
    (
        "POST",
        "secrets/set",
        WorkspaceCall {
            upstream: UpstreamMethod::Put,
            segments: &["workspace", "secrets"],
            timeout: WORKSPACE_READ_TIMEOUT,
            write: false,
            owner: true,
            attributed: false,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    // The exec ledger's take-back. Bedrock's write-time scrub redacts only
    // the values the room held when a tail was STORED, so a token that
    // leaked before it was stored — or was rotated after a leak — sits in
    // stored exec tails until this call blanks them. Owner-gated for
    // Bedrock's own reason (`requireRoomOwner`, no admin bypass): the tails
    // are the room's output, and only the room's owner decides they cannot
    // be un-published. The body (`exec_id` only, optional; omitted means
    // purge-all) is validated upstream strict deny-extra — malformed 400,
    // well-formed but absent 404, still-running 409 `exec_running` — and
    // the handler reads NO `actor_member_id`, which the unconditional actor
    // strip respects. The reply `{purged, exec_id}` and the audit event
    // carry counts and ids, never content. A prompt table write like the
    // binding pair — no container runs — so it carries the read budget.
    (
        "POST",
        "execs/purge",
        WorkspaceCall {
            upstream: UpstreamMethod::Post,
            segments: &["workspace", "execs", "purge"],
            timeout: WORKSPACE_READ_TIMEOUT,
            write: false,
            owner: true,
            attributed: false,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    // Port exposure, opened by the same ruling and gated MORE tightly than
    // Bedrock gates it. Upstream both calls sit behind member write; here both
    // are owner verbs, because the preview URL expose returns is a routing
    // label rather than a credential — whatever the room serves on that port
    // is served to anyone holding it — and publishing a room's compute to the
    // open internet is the owner's call to make. BOTH run container work, and
    // that is why they carry the COMMAND budget: each handler drives the
    // compute driver (`exposePort`/`unexposePort`), which for the cloudflare
    // driver is a fetch to the room-runtime Worker on a 60s budget of its own,
    // and upstream only checks that the workspace ROW says ready — so a first
    // expose after an idle stop pays for a cold container start inside it. At
    // 15s the daemon would abort while Bedrock went on to register the route,
    // record the port and emit `room.workspace.port_exposed`: the caller reads
    // a failure, never receives the `preview_url`, and the port is published
    // regardless. Expose's upstream body is strict deny-extra (`port` only),
    // which the unconditional actor strip keeps legal. Close
    // names its port in the BODY and the daemon moves it into the PATH, which
    // is what `path_from_body` is for; the upstream DELETE then reads no body
    // at all, so nothing of the shaped object travels.
    (
        "POST",
        "ports",
        WorkspaceCall {
            upstream: UpstreamMethod::Post,
            segments: &["workspace", "ports"],
            timeout: WORKSPACE_COMMAND_TIMEOUT,
            write: false,
            owner: true,
            attributed: false,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: None,
        },
    ),
    (
        "POST",
        "ports/close",
        WorkspaceCall {
            upstream: UpstreamMethod::Delete,
            segments: &["workspace", "ports"],
            timeout: WORKSPACE_COMMAND_TIMEOUT,
            write: false,
            owner: true,
            attributed: false,
            query: &[],
            reply: UpstreamReply::Json,
            path_from_body: Some("port"),
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
    /// The route needs a Bedrock member id and the asserted actor resolves to
    /// none: a Bot/Tool/System participant, or an agent that was never
    /// federation-registered. Fail closed — never attribute them to the human.
    UnmappedActor,
    /// An owner verb asserted by an actor that resolves to a member id other
    /// than the credential's principal — the one Bedrock would actually judge.
    NotPrincipal,
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
        GateError::UnmappedActor => (
            StatusCode::FORBIDDEN,
            "workspace_actor_unmapped",
            "the asserted actor resolves to no Bedrock member id on this daemon",
        ),
        GateError::NotPrincipal => (
            StatusCode::FORBIDDEN,
            "workspace_not_owner_principal",
            "an owner verb forwards only for the principal this room's credential speaks for",
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

/// The refusal for a body that names no usable port on the one row whose
/// upstream path carries one. It shares `invalid_request` with the lane's other
/// local shape refusals on purpose: this says the body was the wrong SHAPE, and
/// which ports a room may actually expose is Bedrock's policy, answered by
/// Bedrock and relayed verbatim like every other upstream code.
fn invalid_port_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "ok": false,
            "code": "invalid_request",
            "error": "this workspace call must name its port as an integer in 1-65535",
        })),
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

/// What the gate hands the transport: the bearer, and — on routes that need
/// one — the Bedrock member id the asserted actor RESOLVES to.
struct GatedCall {
    credential: RoomCredential,
    /// `Some` exactly when the row is attributed or owner-gated; those are the
    /// routes where a member id matters, and the gate has already refused any
    /// actor it could not derive one for. Reads carry `None` on purpose — an
    /// unregistered agent may still look, it just cannot be spoken for.
    actor_member_id: Option<String>,
}

/// Roster-check the asserted actor, read the room's credential, and derive the
/// actor's Bedrock member id under ONE store guard, so a roster edit or a
/// binding change landing between the reads cannot authorize a call the roster
/// no longer permits or attribute it to an id the map no longer holds.
///
/// The returned [`RoomCredential`] carries the bearer. It goes straight to
/// `send_room_scoped` and nowhere else: it is never logged, never rendered into
/// an error, and the type deliberately has no `Serialize`.
fn gate_workspace_call(
    state: &AppState,
    key: &RoomKey,
    actor_id: &str,
    call: &WorkspaceCall,
) -> Result<GatedCall, GateError> {
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
        if call.write
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
            Ok(access) if access.state == RoomAccessState::Revoked => {
                return Err(GateError::Revoked)
            }
            Ok(_) => {}
            Err(_) => return Err(GateError::Store),
        }
        // The identity map, daemon-derived only. A Human is the credential's
        // principal — this daemon serves exactly one human principal, and
        // every browser session on it IS that principal. An Agent roster id
        // is its folder-agent name, which keys the binding `register_agents`
        // wrote when Bedrock's member envelope came back. Everything else has
        // no member id to resolve to, and a route that needs one fails closed
        // rather than quietly wearing the human's.
        let actor_member_id = if call.attributed || call.owner {
            let resolved = match kind {
                RoomParticipantKind::Human => Some(credential.local_human_member_id.clone()),
                RoomParticipantKind::Agent => {
                    match store.resolve_room_agent_member(key, actor_id) {
                        Ok(member) => member,
                        Err(_) => return Err(GateError::Store),
                    }
                }
                RoomParticipantKind::Bot
                | RoomParticipantKind::Tool
                | RoomParticipantKind::System => None,
            };
            let resolved = resolved.ok_or(GateError::UnmappedActor)?;
            if call.owner && resolved != credential.local_human_member_id {
                return Err(GateError::NotPrincipal);
            }
            Some(resolved)
        } else {
            None
        };
        Ok(GatedCall {
            credential,
            actor_member_id,
        })
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

    let gated = match gate_workspace_call(&state, &key, actor_id, &call) {
        Ok(gated) => gated,
        Err(error) => return gate_error_response(error),
    };
    let credential = gated.credential;

    let body = match shape_body(body, &call, gated.actor_member_id.as_deref()) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    // Read before the DELETE arm below discards the body it lives in: the row
    // that names a path key is also the row whose upstream reads no body, so
    // the value has exactly this one moment to be taken and proven.
    let dynamic_segment = match call.path_from_body {
        None => None,
        Some(key) => {
            let proven = body
                .as_ref()
                .and_then(|body| body.get(key))
                .and_then(port_path_segment);
            let Some(proven) = proven else {
                return invalid_port_response();
            };
            Some(proven)
        }
    };
    // An upstream DELETE reads no body; the shaped object was still demanded
    // and still size-checked so the POST contract stays uniform, and now it
    // stays here.
    let body = match call.upstream {
        UpstreamMethod::Delete => None,
        _ => body,
    };
    let mut segments: Vec<&str> = call.segments.to_vec();
    if let Some(segment) = dynamic_segment.as_deref() {
        segments.push(segment);
    }
    let query: Vec<(&str, String)> = call
        .query
        .iter()
        .filter_map(|name| params.get(*name).map(|value| (*name, value.clone())))
        .collect();

    match call.reply {
        UpstreamReply::Json => {
            match state
                .room_federation
                .send_room_scoped(
                    &credential,
                    call.upstream.as_reqwest(),
                    &segments,
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
                // `workspace_absent` / `repo_not_cloned` / `repo_cloning` codes
                // are the whole reason a UI can say something useful, and
                // re-coding them here would only lose information. No upstream
                // HEADER is relayed.
                Ok((status, payload)) => (status, Json(payload)).into_response(),
                Err(error) => intent_error_response(error),
            }
        }
        UpstreamReply::FileProjection => {
            let path = params.get("path").cloned().unwrap_or_default();
            relay_file_projection(&state, &credential, &call, &segments, &query, &path).await
        }
    }
}

/// The raw arm of [`forward`]: read `workspace/file`'s mixed-mode answer — raw
/// bytes on a 2xx, an ordinary JSON refusal otherwise — and turn the bytes
/// into the JSON a browser is allowed to see.
async fn relay_file_projection(
    state: &AppState,
    credential: &RoomCredential,
    call: &WorkspaceCall,
    segments: &[&str],
    query: &[(&str, String)],
    path: &str,
) -> Response {
    match state
        .room_federation
        .send_room_scoped_raw(
            credential,
            call.upstream.as_reqwest(),
            segments,
            query,
            RelayBudget {
                body_limit: WORKSPACE_FILE_LIMIT,
                timeout: call.timeout,
            },
        )
        .await
    {
        Ok((status, RawReply::Body(bytes))) if status.is_success() => {
            (status, Json(project_file(path, &bytes))).into_response()
        }
        // A refusal on this route is a JSON `HttpError` body like every other
        // route's, and it relays verbatim for the same reason theirs do:
        // `workspace_absent` and Bedrock's own path 400s are what let a panel
        // say something useful.
        Ok((status, RawReply::Body(bytes))) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(payload) => (status, Json(payload)).into_response(),
            Err(_) => intent_error_response(IntentError::Protocol),
        },
        // Over the cap on a SUCCESS is the legitimate big file the bound
        // exists for; over the cap on a refusal body is a peer this daemon
        // does not recognise as Bedrock.
        Ok((status, RawReply::OverCap)) if status.is_success() => (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "ok": false,
                "code": "workspace_file_too_large",
                "error": "this file is larger than the 1 MiB the daemon will relay; nothing was truncated",
            })),
        )
            .into_response(),
        Ok((_, RawReply::OverCap)) => intent_error_response(IntentError::Protocol),
        Err(error) => intent_error_response(error),
    }
}

/// What a workspace file becomes on this lane. Text-vs-binary is decided by
/// decoding the bytes in hand, never by the extension-derived `content-type`
/// Bedrock sent; binary rides as base64 rather than being refused, because
/// which files a member may open is Bedrock's call and this lane only decides
/// representation. `size` is the byte count BEFORE encoding, so a client can
/// show it without decoding, and `path` echoes what the caller asked —
/// Bedrock's normalized form of it lives in its own `list` rows. There is no
/// `truncated` field on purpose: a file past the bound is refused whole, never
/// clipped.
fn project_file(path: &str, bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => json!({
            "ok": true,
            "path": path,
            "size": bytes.len(),
            "encoding": "utf8",
            "content": text,
        }),
        Err(_) => {
            use base64::Engine as _;
            json!({
                "ok": true,
                "path": path,
                "size": bytes.len(),
                "encoding": "base64",
                "content": base64::engine::general_purpose::STANDARD.encode(bytes),
            })
        }
    }
}

/// Strip the client's actor claim, install the gate's RESOLVED member id where
/// the route needs one, and refuse a body too large to forward.
fn shape_body(
    body: Option<Value>,
    call: &WorkspaceCall,
    actor_member_id: Option<&str>,
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
    // attributed route the RESOLVED id must be the one that lands, and on every
    // other route Bedrock rejects the key as stray.
    object.remove(ACTOR_MEMBER_ID);
    if call.attributed {
        // The gate resolves an id for every attributed row before this runs;
        // if that contract ever breaks, refuse rather than send a call Bedrock
        // would mis-attribute or reject.
        let Some(actor_member_id) = actor_member_id else {
            return Err(Box::new(gate_error_response(GateError::UnmappedActor)));
        };
        object.insert(
            ACTOR_MEMBER_ID.into(),
            Value::String(actor_member_id.to_string()),
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

/// A caller's value re-proven as a port NUMBER and rendered as the path
/// segment it is then allowed to become. `None` refuses, and nothing else in
/// this module turns caller input into a path.
///
/// The proof is deliberately about SHAPE and not policy. Bedrock's own
/// `validatePort` owns the 1024 floor and the reserved-port list, and its
/// refusals relay verbatim like every other upstream code, so re-deciding them
/// here would only give the two copies somewhere to drift apart. What this
/// cannot leave to upstream is the segment: a path built out of unproven
/// caller text is how a lane stops being a lane, so the value crosses only
/// after it is known to be an integer inside the port space — a string, a
/// float, a traversal, and a number outside the range all stop here.
fn port_path_segment(value: &Value) -> Option<String> {
    let port = value.as_u64()?;
    (1..=u64::from(u16::MAX))
        .contains(&port)
        .then(|| port.to_string())
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
/// binding. Every allowlisted POST leaf demands a JSON object, so the body is
/// required rather than optional; the unbind leaf's travels no further than
/// the daemon, because its upstream DELETE reads none.
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
        routing::{delete, get, post},
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

    fn record(seen: &Seen, method: &Method, uri: &Uri, headers: &HeaderMap, body: &Bytes) {
        seen.calls.lock().unwrap().push(SeenCall {
            method: method.to_string(),
            path: uri.path().to_string(),
            query: uri.query().unwrap_or_default().to_string(),
            authorization: headers
                .get(axum::http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            body: serde_json::from_slice(body).unwrap_or(Value::Null),
        });
    }

    async fn record_call(
        State(seen): State<Seen>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Json<Value> {
        record(&seen, &method, &uri, &headers, &body);
        Json(json!({"ok": true}))
    }

    /// What the fake's file leaf serves. The DECLARED types below contradict
    /// the bytes on purpose — the text file claims `application/octet-stream`
    /// and the binary one claims `text/plain` — so a projection that consults
    /// the declaration instead of the bytes fails these tests.
    const FILE_TEXT: &str = "# Ocean\n\nthe workspace panel opens this\n";
    const FILE_BINARY: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF];

    /// Bedrock's file route is MIXED-MODE — a 2xx is raw bytes with an
    /// extension-derived content-type and a content-disposition, every refusal
    /// an ordinary JSON `HttpError` body — so its stand-in is too, keyed on
    /// the requested path.
    async fn file_read_call(
        State(seen): State<Seen>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        record(&seen, &method, &uri, &headers, &body);
        let query = uri.query().unwrap_or_default().to_string();
        let raw = |content_type: &str, bytes: Vec<u8>| {
            (
                StatusCode::OK,
                [
                    (axum::http::header::CONTENT_TYPE, content_type.to_string()),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        "attachment; filename=\"x\"".to_string(),
                    ),
                ],
                bytes,
            )
                .into_response()
        };
        if query.contains("path=readme.md") {
            raw("application/octet-stream", FILE_TEXT.as_bytes().to_vec())
        } else if query.contains("path=logo.png") {
            raw("text/plain; charset=utf-8", FILE_BINARY.to_vec())
        } else if query.contains("path=huge.bin") {
            raw(
                "application/octet-stream",
                vec![b'x'; WORKSPACE_FILE_LIMIT + 1],
            )
        } else if query.contains("path=missing.txt") {
            // The shape Bedrock's response boundary gives every HttpError.
            (
                StatusCode::CONFLICT,
                Json(json!({
                    "ok": false,
                    "error": "This room has no workspace. Provision one first.",
                    "details": {"code": "workspace_absent"},
                })),
            )
                .into_response()
        } else {
            raw("text/plain; charset=utf-8", b"default".to_vec())
        }
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
    /// it was asked. It deliberately ALSO serves GET `workspace/secrets`, so a
    /// test asserting the name list is unreachable is proving the daemon's
    /// allowlist rather than an upstream 404; the secrets PUT beside it is the
    /// owner set's real upstream, the repo PUT and DELETE the binding verbs',
    /// the workspace POST and DELETE the lifecycle pair's, and the
    /// `execs/purge` POST the take-back's, and the `ports` POST beside a
    /// `ports/{port}` DELETE the exposure pair's — the DELETE registered with
    /// the port as a captured segment, so a daemon that failed to move the
    /// caller's port out of the body and into the path would 404 here rather
    /// than pass. The `file`
    /// leaf answers the way the real route does — raw bytes on a 2xx, a JSON
    /// `HttpError` body on a refusal — with declared content-types that
    /// contradict the bytes; see [`file_read_call`].
    async fn start_fake_bedrock(seen: Seen) -> (String, JoinHandle<()>) {
        let app = Router::new()
            .route(
                "/api/v1/rooms/{room}/workspace",
                get(record_call).post(record_call).delete(record_call),
            )
            .route("/api/v1/rooms/{room}/workspace/list", get(record_call))
            .route("/api/v1/rooms/{room}/workspace/execs", get(record_call))
            .route(
                "/api/v1/rooms/{room}/workspace/execs/purge",
                post(record_call),
            )
            .route("/api/v1/rooms/{room}/workspace/exec", post(record_call))
            .route(
                "/api/v1/rooms/{room}/workspace/secrets",
                get(record_call).put(record_call),
            )
            .route("/api/v1/rooms/{room}/workspace/ports", post(record_call))
            .route(
                "/api/v1/rooms/{room}/workspace/ports/{port}",
                delete(record_call),
            )
            .route("/api/v1/rooms/{room}/workspace/file", get(file_read_call))
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
            .route(
                "/api/v1/rooms/{room}/workspace/repo/ci",
                get(record_call).post(record_call),
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
    /// The bare `secrets` leaf is the case that matters most: the name list
    /// exists upstream, the fake Bedrock here serves it, and it is still
    /// unreachable both ways — opening the owner's `secrets/set` did not open
    /// a read-back (`GET secrets/set` refuses too, pinning the set as
    /// POST-only). `GET exec` and `POST list` prove the METHOD half of the
    /// key refuses too — a leaf being allowlisted for one verb does not open
    /// it for another — and `POST file` pins that opening the file READ did
    /// not open a write. `GET provision` and `GET destroy` pin the same for
    /// the lifecycle pair, `GET execs/purge` for the take-back, and
    /// `GET ports` / `GET ports/close` for the exposure pair: owner leaves
    /// opened for POST alone.
    ///
    /// Mutation: make `resolve_workspace_call` fall through to a constructed
    /// `WorkspaceCall` for unknown keys -> RED.
    #[tokio::test]
    async fn a_call_the_allowlist_does_not_name_is_refused_and_nothing_is_forwarded() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        for leaf in [
            "secrets",
            "secrets/set",
            "exec",
            "repo/../secrets",
            "provision",
            "destroy",
            "execs/purge",
            "ports",
            "ports/close",
        ] {
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

        for leaf in ["secrets", "list", "file"] {
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

    /// The lane's first non-JSON upstream: a 2xx on `workspace/file` is raw
    /// bytes, and what the browser receives is the daemon's JSON projection of
    /// them — never the bytes as a document, never Bedrock's content-type. The
    /// fake declares `application/octet-stream` for these bytes on purpose;
    /// deciding text-vs-binary from that declaration instead of from the bytes
    /// fails here.
    ///
    /// Mutation: relay the upstream body or content-type verbatim -> RED;
    /// derive the encoding from the declared type -> RED.
    #[tokio::test]
    async fn a_text_file_is_projected_as_utf8_json_never_as_a_document() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_read(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "file".to_string())),
            query(&[
                ("actor_id", "alice"),
                ("path", "readme.md"),
                // Presentation for the raw route; meaningless on this lane and
                // it must not reach Bedrock.
                ("inline", "1"),
            ]),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            content_type.starts_with("application/json"),
            "the browser gets a projection, not a typed document: {content_type}"
        );
        let (_, body) = body_of(response).await;
        assert_eq!(body["ok"], json!(true));
        assert_eq!(body["encoding"], json!("utf8"));
        assert_eq!(body["content"], json!(FILE_TEXT));
        assert_eq!(body["size"], json!(FILE_TEXT.len()));
        assert_eq!(body["path"], json!("readme.md"));

        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 1);
        let expected_authorization = format!("Bearer {BEARER}");
        assert_eq!(calls[0].path, "/api/v1/rooms/workspace-room/workspace/file");
        assert_eq!(
            calls[0].authorization.as_deref(),
            Some(expected_authorization.as_str()),
            "the room's own credential authenticates the read"
        );
        assert!(
            calls[0].query.contains("path=readme.md") && !calls[0].query.contains("inline"),
            "`path` relays and `inline` does not: {}",
            calls[0].query
        );

        fixture.close();
    }

    /// Binary is derived from the bytes and carried as base64 — the fake
    /// declares these bytes `text/plain`, and believing that would put invalid
    /// UTF-8 into a JSON string.
    ///
    /// Mutation: decide the encoding from the upstream content-type -> RED.
    #[tokio::test]
    async fn a_binary_file_is_projected_as_base64_from_the_bytes_not_the_declared_type() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_read(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "file".to_string())),
            query(&[("actor_id", "alice"), ("path", "logo.png")]),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["encoding"], json!("base64"));
        assert_eq!(body["size"], json!(FILE_BINARY.len()));
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(body["content"].as_str().expect("base64 content"))
            .expect("valid base64");
        assert_eq!(decoded, FILE_BINARY, "the bytes round-trip exactly");

        fixture.close();
    }

    /// Nothing upstream bounds a file read — Bedrock buffers the whole file —
    /// so the daemon's cap is the only bound in the chain and a file past it
    /// is a legitimate workspace state. It earns the typed refusal, never the
    /// 502 `workspace_upstream_protocol` an actually-broken upstream earns,
    /// and never a truncated "success".
    ///
    /// Mutation: read the file reply through the JSON seam, or map the raw
    /// seam's OverCap to `IntentError::Protocol` -> RED.
    #[tokio::test]
    async fn a_file_over_the_cap_is_a_typed_refusal_not_an_upstream_fault() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_read(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "file".to_string())),
            query(&[("actor_id", "alice"), ("path", "huge.bin")]),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body["code"], json!("workspace_file_too_large"));

        fixture.close();
    }

    /// The file route is mixed-mode: a 2xx is bytes, a refusal is Bedrock's
    /// ordinary JSON `HttpError` body — and the refusal relays verbatim, code
    /// and all, exactly as the JSON rows' do. `workspace_absent` is why a
    /// panel can say "provision one first" instead of shrugging.
    ///
    /// Mutation: project every status, or re-code the refusal -> RED.
    #[tokio::test]
    async fn bedrocks_own_file_refusal_is_relayed_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_read(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "file".to_string())),
            query(&[("actor_id", "alice"), ("path", "missing.txt")]),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["ok"], json!(false));
        assert_eq!(body["details"]["code"], json!("workspace_absent"));

        fixture.close();
    }

    /// The binding verbs opened by the 2026-08-29 ruling forward as what
    /// Bedrock actually serves — bind is the upstream PUT on `workspace/repo`,
    /// unbind the DELETE — on the room's own bearer. Bind's body crosses with
    /// the client's actor claim stripped and NOTHING inserted in its place,
    /// because `validateRepoBinding` upstream is strict deny-extra; unbind
    /// forwards no body at all, because the upstream DELETE reads none.
    ///
    /// Mutation: mark either row `attributed` -> RED (the stray key would be
    /// asserted here before Bedrock could 400 it); forward the unbind body ->
    /// RED.
    #[tokio::test]
    async fn an_owner_bind_forwards_as_the_upstream_put_and_unbind_as_the_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;
        let expected_authorization = format!("Bearer {BEARER}");

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "repo/bind".to_string())),
            query(&[("actor_id", "alice")]),
            Json(json!({
                "remote": "https://github.com/example/repo.git",
                "branch": "main",
                // The forgery attempt again; on this row it must vanish
                // entirely rather than be replaced.
                "actor_member_id": "member-somebody-else",
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.method, "PUT");
        assert_eq!(call.path, "/api/v1/rooms/workspace-room/workspace/repo");
        assert_eq!(
            call.authorization.as_deref(),
            Some(expected_authorization.as_str()),
            "the room's own credential authenticates the bind"
        );
        assert_eq!(
            call.body["remote"],
            json!("https://github.com/example/repo.git")
        );
        assert_eq!(call.body["branch"], json!("main"));
        assert!(
            call.body.get(ACTOR_MEMBER_ID).is_none(),
            "a binding body carries no actor claim, the client's or the daemon's"
        );

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "repo/unbind".to_string())),
            query(&[("actor_id", "alice")]),
            Json(json!({})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 2);
        let call = &calls[1];
        assert_eq!(call.method, "DELETE");
        assert_eq!(call.path, "/api/v1/rooms/workspace-room/workspace/repo");
        assert_eq!(
            call.authorization.as_deref(),
            Some(expected_authorization.as_str()),
            "the room's own credential authenticates the unbind"
        );
        assert_eq!(call.body, Value::Null, "an upstream DELETE carries no body");

        fixture.close();
    }

    /// The identity map fails closed on the owner verbs, with a distinct code
    /// for each way an actor is not the principal: an agent never
    /// federation-registered resolves to NOTHING, and one that IS registered
    /// resolves to its own member id — still not the id Bedrock's
    /// `requireRoomOwner` will judge, because the daemon presents the
    /// credential's bearer. Neither refusal forwards anything, and neither
    /// quietly attributes the agent to the human — the exact failure the
    /// ruling names.
    ///
    /// Mutation: resolve an Agent to `local_human_member_id` -> RED; drop the
    /// owner comparison -> RED (the bound agent would forward).
    #[tokio::test]
    async fn an_agent_on_an_owner_verb_is_refused_mapped_or_not() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;
        let bind_body = json!({"remote": "https://github.com/example/repo.git"});

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "repo/bind".to_string())),
            query(&[("actor_id", "researcher")]),
            Json(bind_body.clone()),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("workspace_actor_unmapped"));

        with_rooms(&fixture.state, |store| {
            store
                .bind_room_agent(&fixture.key, "member-researcher", "researcher", "reg-key")
                .expect("binding fixture");
        });
        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "repo/bind".to_string())),
            query(&[("actor_id", "researcher")]),
            Json(bind_body),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("workspace_not_owner_principal"));

        assert!(
            fixture.seen.calls().is_empty(),
            "neither refusal may spend the bearer"
        );

        fixture.close();
    }

    /// The lifecycle verbs opened with the binding pair forward as what
    /// Bedrock actually serves — provision is the upstream POST on
    /// `workspace`, destroy the DELETE — on the room's own bearer.
    /// Provision's body crosses with the client's actor claim stripped and
    /// NOTHING inserted in its place, because the upstream body is strict
    /// deny-extra (`spec` only); destroy forwards no body at all, because the
    /// upstream DELETE reads none, and `flush` is the one query key it
    /// relays.
    ///
    /// Mutation: mark either row `attributed` -> RED (the stray key would be
    /// asserted here before Bedrock could 400 it); forward the destroy body,
    /// or drop `flush` from its row's query -> RED.
    #[tokio::test]
    async fn an_owner_provision_forwards_as_the_upstream_post_and_destroy_as_the_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;
        let expected_authorization = format!("Bearer {BEARER}");

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "provision".to_string())),
            query(&[("actor_id", "alice")]),
            Json(json!({
                "spec": {"image": "default"},
                // The forgery attempt again; on this row it must vanish
                // entirely rather than be replaced.
                "actor_member_id": "member-somebody-else",
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, "/api/v1/rooms/workspace-room/workspace");
        assert_eq!(
            call.authorization.as_deref(),
            Some(expected_authorization.as_str()),
            "the room's own credential authenticates the provision"
        );
        assert_eq!(call.body["spec"], json!({"image": "default"}));
        assert!(
            call.body.get(ACTOR_MEMBER_ID).is_none(),
            "a provision body carries no actor claim, the client's or the daemon's"
        );

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "destroy".to_string())),
            query(&[("actor_id", "alice"), ("flush", "0")]),
            Json(json!({})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 2);
        let call = &calls[1];
        assert_eq!(call.method, "DELETE");
        assert_eq!(call.path, "/api/v1/rooms/workspace-room/workspace");
        assert_eq!(
            call.authorization.as_deref(),
            Some(expected_authorization.as_str()),
            "the room's own credential authenticates the destroy"
        );
        assert!(
            call.query.contains("flush=0") && !call.query.contains("actor_id"),
            "`flush` relays and `actor_id` does not: {}",
            call.query
        );
        assert_eq!(call.body, Value::Null, "an upstream DELETE carries no body");

        fixture.close();
    }

    /// The identity map fails closed on the lifecycle pair exactly as it does
    /// on the binding pair: an agent never federation-registered resolves to
    /// nothing, a registered one resolves to its own member id — still not
    /// the principal Bedrock's `requireRoomOwner` will judge — and neither
    /// refusal forwards anything. Pinned separately from the binding test
    /// because these rows carry the command budget: a gate that leaked here
    /// would leak on the verb that builds infrastructure.
    ///
    /// Mutation: resolve an Agent to `local_human_member_id` -> RED; drop the
    /// owner comparison -> RED.
    #[tokio::test]
    async fn an_agent_on_a_lifecycle_verb_is_refused_mapped_or_not() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        for leaf in ["provision", "destroy"] {
            let response = room_workspace_command(
                State(fixture.state.clone()),
                Path((fixture.key.as_str().to_string(), leaf.to_string())),
                query(&[("actor_id", "researcher")]),
                Json(json!({})),
            )
            .await;
            let (status, body) = body_of(response).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "unmapped {leaf}");
            assert_eq!(
                body["code"],
                json!("workspace_actor_unmapped"),
                "unmapped {leaf}"
            );
        }

        with_rooms(&fixture.state, |store| {
            store
                .bind_room_agent(&fixture.key, "member-researcher", "researcher", "reg-key")
                .expect("binding fixture");
        });
        for leaf in ["provision", "destroy"] {
            let response = room_workspace_command(
                State(fixture.state.clone()),
                Path((fixture.key.as_str().to_string(), leaf.to_string())),
                query(&[("actor_id", "researcher")]),
                Json(json!({})),
            )
            .await;
            let (status, body) = body_of(response).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "mapped {leaf}");
            assert_eq!(
                body["code"],
                json!("workspace_not_owner_principal"),
                "mapped {leaf}"
            );
        }

        assert!(
            fixture.seen.calls().is_empty(),
            "no refusal may spend the bearer"
        );

        fixture.close();
    }

    /// The owner's secrets set forwards as the upstream PUT on the room's own
    /// bearer, with the client's actor claim stripped and NOTHING inserted in
    /// its place — the upstream body is strict deny-extra (`secrets` only) —
    /// and an agent asserting the leaf is refused with nothing forwarded,
    /// which is what makes this the owner-gated set the module doc's answered
    /// objection permits and not a member route. The fixture value is fake on
    /// purpose and asserted only as "what the daemon relayed", never named as
    /// a credential; the null entry rides untouched too, because null is the
    /// upstream's documented delete form.
    ///
    /// Mutation: key the row `"secrets"` instead of `"secrets/set"` -> RED
    /// (refusal test above); drop `owner` from the row -> RED here (the agent
    /// would forward).
    #[tokio::test]
    async fn an_owner_secrets_set_forwards_as_the_upstream_put_and_an_agent_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "secrets/set".to_string())),
            query(&[("actor_id", "alice")]),
            Json(json!({
                "secrets": {"GH_TOKEN": "fake-fixture-value", "STALE_NAME": null},
                // The forgery attempt again; on this row it must vanish
                // entirely rather than be replaced.
                "actor_member_id": "member-somebody-else",
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.method, "PUT");
        assert_eq!(call.path, "/api/v1/rooms/workspace-room/workspace/secrets");
        assert_eq!(
            call.authorization.as_deref(),
            Some(format!("Bearer {BEARER}").as_str()),
            "the room's own credential authenticates the set"
        );
        assert_eq!(
            call.body["secrets"],
            json!({"GH_TOKEN": "fake-fixture-value", "STALE_NAME": null})
        );
        assert!(
            call.body.get(ACTOR_MEMBER_ID).is_none(),
            "a secrets body carries no actor claim, the client's or the daemon's"
        );

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "secrets/set".to_string())),
            query(&[("actor_id", "researcher")]),
            Json(json!({"secrets": {"GH_TOKEN": "fake-fixture-value"}})),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("workspace_actor_unmapped"));
        assert_eq!(
            fixture.seen.calls().len(),
            1,
            "the refusal spent no bearer and added no upstream call"
        );

        fixture.close();
    }

    /// The exec ledger's take-back. The owner's purge forwards as the
    /// upstream POST with the body relayed verbatim — Bedrock's handler is
    /// strict deny-extra and reads no `actor_member_id`, so the daemon's
    /// only edit is stripping the client's claim, never replacing it — and
    /// an actor that does not resolve to the credential's principal is
    /// refused before the bearer is spent.
    ///
    /// Mutation: give the row `attributed: true` -> RED (an injected
    /// actor_member_id turns a legal purge into an upstream 400); drop the
    /// owner gate -> RED.
    #[tokio::test]
    async fn an_owner_execs_purge_forwards_as_the_upstream_post_and_an_agent_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "execs/purge".to_string())),
            query(&[("actor_id", "alice")]),
            Json(json!({
                "exec_id": "3f9d3c6a-8f0f-4d5c-9a34-1f2ab8f0c9d1",
                // The forgery attempt again; the upstream reads no actor
                // claim, so it must vanish rather than be replaced.
                "actor_member_id": "member-somebody-else",
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.method, "POST");
        assert_eq!(
            call.path,
            "/api/v1/rooms/workspace-room/workspace/execs/purge"
        );
        assert_eq!(
            call.authorization.as_deref(),
            Some(format!("Bearer {BEARER}").as_str()),
            "the room's own credential authenticates the purge"
        );
        assert_eq!(
            call.body["exec_id"],
            json!("3f9d3c6a-8f0f-4d5c-9a34-1f2ab8f0c9d1")
        );
        assert!(
            call.body.get(ACTOR_MEMBER_ID).is_none(),
            "a purge body carries no actor claim, the client's or the daemon's"
        );

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "execs/purge".to_string())),
            query(&[("actor_id", "researcher")]),
            Json(json!({})),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("workspace_actor_unmapped"));
        assert_eq!(
            fixture.seen.calls().len(),
            1,
            "the refusal spent no bearer and added no upstream call"
        );

        fixture.close();
    }

    /// The exposure pair. Expose carries its port in the BODY and forwards as
    /// the upstream POST; close carries it in the body too and the daemon
    /// moves it into the PATH, because that is where Bedrock's DELETE route
    /// keeps it and the table cannot hold a value the caller supplies.
    ///
    /// Both are owner-gated even though Bedrock gates them at member WRITE.
    /// That is the narrowing the module doc rules on — the preview URL expose
    /// hands back is world-readable by construction — so an agent with no
    /// binding and a mapped agent that is not the credential's principal each
    /// earn their own refusal, and neither spends the bearer.
    ///
    /// Mutation: drop the owner gate -> RED; drop `path_from_body` from the
    /// close row -> RED (the DELETE would land on the collection path, which
    /// Bedrock does not serve and this fixture does not route).
    #[tokio::test]
    async fn an_owner_exposes_a_port_by_body_and_closes_it_by_path() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;
        let expected_authorization = format!("Bearer {BEARER}");

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "ports".to_string())),
            query(&[("actor_id", "alice")]),
            Json(json!({
                "port": 8080,
                // The forgery attempt: expose's upstream body is strict
                // deny-extra, so the claim must vanish rather than be replaced.
                "actor_member_id": "member-somebody-else",
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 1);
        let expose = &calls[0];
        assert_eq!(expose.method, "POST");
        assert_eq!(expose.path, "/api/v1/rooms/workspace-room/workspace/ports");
        assert_eq!(
            expose.authorization.as_deref(),
            Some(expected_authorization.as_str()),
            "the room's own credential authenticates the exposure"
        );
        assert_eq!(expose.body["port"], json!(8080));
        assert!(
            expose.body.get(ACTOR_MEMBER_ID).is_none(),
            "an expose body carries no actor claim, the client's or the daemon's"
        );

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "ports/close".to_string())),
            query(&[("actor_id", "alice")]),
            Json(json!({"port": 8080})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 2);
        let close = &calls[1];
        assert_eq!(close.method, "DELETE");
        assert_eq!(
            close.path, "/api/v1/rooms/workspace-room/workspace/ports/8080",
            "the port the caller named is what becomes the upstream path segment"
        );
        assert_eq!(
            close.body,
            Value::Null,
            "the upstream DELETE reads no body, so nothing of the shaped object travels"
        );

        for leaf in ["ports", "ports/close"] {
            let response = room_workspace_command(
                State(fixture.state.clone()),
                Path((fixture.key.as_str().to_string(), leaf.to_string())),
                query(&[("actor_id", "researcher")]),
                Json(json!({"port": 8080})),
            )
            .await;
            let (status, body) = body_of(response).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{leaf}");
            assert_eq!(body["code"], json!("workspace_actor_unmapped"), "{leaf}");
        }

        with_rooms(&fixture.state, |store| {
            store
                .bind_room_agent(&fixture.key, "member-researcher", "researcher", "reg-key")
                .expect("binding fixture");
        });
        for leaf in ["ports", "ports/close"] {
            let response = room_workspace_command(
                State(fixture.state.clone()),
                Path((fixture.key.as_str().to_string(), leaf.to_string())),
                query(&[("actor_id", "researcher")]),
                Json(json!({"port": 8080})),
            )
            .await;
            let (status, body) = body_of(response).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{leaf}");
            assert_eq!(
                body["code"],
                json!("workspace_not_owner_principal"),
                "{leaf}"
            );
        }

        assert_eq!(
            fixture.seen.calls().len(),
            2,
            "no refusal may spend the bearer"
        );

        fixture.close();
    }

    /// A port that is not a port never becomes a path segment. `ports/close`
    /// is the one row where a caller's value reaches the upstream PATH, so it
    /// is re-proven here rather than trusted, and every shape that is not a
    /// port number is refused with nothing forwarded — the string spelling of
    /// a real port included, because a lane that accepts one string has no
    /// reason left to refuse another.
    ///
    /// WHICH ports a room may expose stays Bedrock's policy: 8080 clears this
    /// gate, and the 1024 floor and the reserved-port list relay verbatim like
    /// any other upstream refusal, so the two copies have nowhere to drift.
    ///
    /// Mutation: push the raw value into the path, or accept `as_u64()`
    /// without the range check -> RED.
    #[tokio::test]
    async fn a_port_that_is_not_a_port_never_becomes_a_path_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        for sent in [
            json!({}),
            json!({"port": null}),
            json!({"port": "8080"}),
            json!({"port": 0}),
            json!({"port": -1}),
            json!({"port": 65536}),
            json!({"port": 8080.5}),
            json!({"port": "../../other-room/workspace"}),
        ] {
            let response = room_workspace_command(
                State(fixture.state.clone()),
                Path((fixture.key.as_str().to_string(), "ports/close".to_string())),
                query(&[("actor_id", "alice")]),
                Json(sent.clone()),
            )
            .await;
            let (status, answered) = body_of(response).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{sent}");
            assert_eq!(answered["code"], json!("invalid_request"), "{sent}");
        }
        assert!(
            fixture.seen.calls().is_empty(),
            "a body naming no port must not cause an upstream request"
        );

        fixture.close();
    }

    /// An attributed route sends the actor's RESOLVED member id, and an actor
    /// with none — a Bot here, and Tool and System alike — is refused rather
    /// than silently attributed to the human, which is what this lane did
    /// before the map existed.
    ///
    /// Mutation: fall back to `local_human_member_id` for a non-Human -> RED.
    #[tokio::test]
    async fn an_actor_with_no_member_id_is_refused_not_attributed_as_the_human() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;
        with_rooms(&fixture.state, |store| {
            store
                .add_participant(
                    &fixture.key,
                    ocean_core::RoomParticipant {
                        id: "webhook".into(),
                        kind: RoomParticipantKind::Bot,
                        display_name: "Webhook".into(),
                    },
                    Utc::now(),
                )
                .expect("roster fixture");
        });

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "exec".to_string())),
            query(&[("actor_id", "webhook")]),
            Json(json!({"command": "npm test"})),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], json!("workspace_actor_unmapped"));
        assert!(
            fixture.seen.calls().is_empty(),
            "an unmappable actor must not cause an upstream request"
        );

        fixture.close();
    }

    /// What did NOT open with the binding verbs: the upstream PUT and DELETE
    /// travel as POST leaves, so a bare `POST repo` is still no route and a
    /// wire DELETE still dies at the router — the wildcard registers GET and
    /// POST only, and `cors.rs` does not advertise PUT at all. Through the
    /// real router, because that is the whole claim.
    ///
    /// Mutation: register DELETE on the wildcard in `main.rs` -> RED.
    #[tokio::test]
    async fn the_binding_verbs_ride_post_leaves_not_their_own_wire_verbs() {
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
            "neither wire shape may reach Bedrock"
        );

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

    /// Which rows may outlast Bedrock's own ceiling. The write verbs, the
    /// lifecycle pair (provision hydrates the container and restores the
    /// checkout before it answers, destroy flushes the workspace back first),
    /// and the ports pair, whose handlers drive the compute driver on a 60s
    /// budget of its own before recording a row and emitting a room marker.
    ///
    /// The line is NOT "reaches the container", which would be the obvious
    /// reading and is wrong: `GET list` and `GET file` reach it too — upstream
    /// they are `computeDriver.listFiles`/`readFile` on that same 60s budget —
    /// and they stay on the read budget deliberately. The line is what an
    /// abort LEAVES BEHIND. A read that the daemon gives up on wrote nothing,
    /// recorded nothing and emitted nothing, so the retry is free and the
    /// caller's error is honest. An expose the daemon gives up on has already
    /// published the port, written the row and put `port_exposed` in the
    /// transcript, and the caller was told it failed — a divergence no retry
    /// repairs, which is what buys the ports rows the longer budget.
    ///
    /// The table is where that policy is declared, so the table is where it is
    /// asserted.
    ///
    /// Mutation: give a write or a ports row `WORKSPACE_READ_TIMEOUT` -> RED;
    /// give an owner row `write: true` or `attributed: true` -> RED.
    #[test]
    fn a_command_may_outlast_bedrocks_ceiling_and_a_read_may_not() {
        for (method, leaf, call) in WORKSPACE_ALLOWLIST {
            let runs_container_work =
                call.write || matches!(*leaf, "provision" | "destroy" | "ports" | "ports/close");
            if runs_container_work {
                assert!(
                    call.timeout > BEDROCK_EXEC_TIMEOUT_MAX,
                    "{method} {leaf} would refuse a call Bedrock was still legally serving"
                );
            } else {
                assert_eq!(
                    call.timeout, WORKSPACE_READ_TIMEOUT,
                    "{method} {leaf} leaves nothing behind when it is abandoned, \
                     so it should answer promptly or not at all"
                );
            }
            if call.owner {
                assert!(
                    !call.write && !call.attributed,
                    "{method} {leaf} is owner-gated: the principal comparison admits Humans \
                     only, which subsumes the write gate's Agent/System refusal, and its \
                     upstream reads no actor_member_id"
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
                "GET file -> Get [\"workspace\", \"file\"]",
                "GET list -> Get [\"workspace\", \"list\"]",
                "GET repo -> Get [\"workspace\", \"repo\"]",
                "GET repo/ci -> Get [\"workspace\", \"repo\", \"ci\"]",
                "POST destroy -> Delete [\"workspace\"]",
                "POST exec -> Post [\"workspace\", \"exec\"]",
                "POST execs/purge -> Post [\"workspace\", \"execs\", \"purge\"]",
                "POST ports -> Post [\"workspace\", \"ports\"]",
                "POST ports/close -> Delete [\"workspace\", \"ports\"]",
                "POST provision -> Post [\"workspace\"]",
                "POST repo/bind -> Put [\"workspace\", \"repo\"]",
                "POST repo/build -> Post [\"workspace\", \"repo\", \"build\"]",
                "POST repo/ci -> Post [\"workspace\", \"repo\", \"ci\"]",
                "POST repo/clone -> Post [\"workspace\", \"repo\", \"clone\"]",
                "POST repo/unbind -> Delete [\"workspace\", \"repo\"]",
                "POST secrets/set -> Put [\"workspace\", \"secrets\"]",
            ],
            "the Bedrock surface this lane exposes changed; review the manifest"
        );
        // The owner verbs entered by the 2026-08-29 operator ruling. Method
        // DIVERGENCE exists only so an owner verb can ride a wire method the
        // router registers, so a translated method must always mean an owner
        // row. The converse stopped holding when the lifecycle pair joined:
        // provision is owner-gated on an upstream POST, no translation needed.
        for (method, leaf, call) in WORKSPACE_ALLOWLIST {
            assert!(
                call.owner || matches!(call.upstream, UpstreamMethod::Get | UpstreamMethod::Post),
                "{method} {leaf}: only an owner verb may translate the wire method"
            );
        }
        // `file` left this assertion deliberately: the GET row that now
        // carries it is a bounded JSON PROJECTION, not a raw-bytes relay — the
        // browser never receives the bytes as a document and no content-type
        // is ever declared to it — and file WRITE and DELETE remain absent.
        // The row must also stay a projection: an allowlist entry that relayed
        // this route's bytes verbatim would put an uploader-controlled
        // document on a browser origin, which is the exact thing
        // `room_attachments.rs` exists to prevent.
        assert!(
            WORKSPACE_ALLOWLIST.iter().all(|(_, _, call)| {
                call.segments.contains(&"file")
                    == matches!(call.reply, UpstreamReply::FileProjection)
            }),
            "raw file bytes are relayed only as a projection, and only on the file row"
        );
        // Weakened ONE-WAY when the owner's set arrived: what the old blanket
        // absence protected on secrets is now pinned by shape — every secrets
        // row is the owner-gated upstream Put. A member-level row, or a GET
        // that would relay the name list or a value, cannot enter this table
        // without turning this red.
        for (method, leaf, call) in WORKSPACE_ALLOWLIST {
            if call.segments.contains(&"secrets") {
                assert!(
                    call.owner && call.upstream == UpstreamMethod::Put,
                    "{method} {leaf}: a secrets row is the owner-gated set, and nothing else"
                );
            }
        }
        // Ports were blanket-absent for the same reason and are now pinned the
        // same way, and this pin carries more weight than the secrets one: the
        // narrowing it protects is NOT visible upstream. Bedrock gates expose
        // and close at member write, so a member-level ports row is the easy
        // thing to add later and would look like it was matching upstream
        // while quietly handing every roster participant a world-readable
        // preview URL onto the room's compute.
        for (method, leaf, call) in WORKSPACE_ALLOWLIST {
            if call.segments.contains(&"ports") {
                assert!(
                    call.owner
                        && matches!(call.upstream, UpstreamMethod::Post | UpstreamMethod::Delete),
                    "{method} {leaf}: a ports row is the owner-gated expose or close, and nothing else"
                );
            }
        }
    }

    /// The operator guide's lane section is the OTHER copy of this table, and
    /// a hand-maintained copy has already rotted once — #391's land note
    /// records correcting a false "NOT exposed" paragraph manually. This pins
    /// the section between the guide's `# Room workspace` and `# Room media`
    /// headings to the table: every named leaf must appear backticked, the
    /// bare status route must keep its own METHOD-first quick-reference line
    /// (the shape main.rs's banner parity parse consumes), and the spelled
    /// call count must move with `WORKSPACE_ALLOWLIST.len()`.
    ///
    /// A backticked-leaf substring cannot see WHICH method line names it
    /// (`repo/ci` rides both GET and POST), and that is fine: the manifest
    /// above pins method+upstream exactly; this gate's only job is that the
    /// guide cannot stop naming a call that exists.
    ///
    /// Mutation: delete any leaf's mention from the lane section -> RED; add
    /// a nineteenth allowlist row without documenting it -> RED.
    #[test]
    fn the_operator_guide_names_every_allowlisted_call() {
        let guide = include_str!("../../../docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md");
        let start = guide
            .find("# Room workspace")
            .expect("guide lost its workspace lane section");
        let tail = &guide[start..];
        let end = tail
            .find("# Room media")
            .expect("guide lost the heading that closes the workspace lane section");
        let section = &tail[..end];

        for (method, leaf, _) in WORKSPACE_ALLOWLIST {
            if leaf.is_empty() {
                assert!(
                    section.lines().any(|line| {
                        let mut parts = line.split_whitespace();
                        parts.next() == Some(*method)
                            && parts.next() == Some("/v1/rooms/persistent/{key}/workspace")
                    }),
                    "the guide's lane section lost the bare {method} status route line"
                );
            } else {
                assert!(
                    section.contains(&format!("`{leaf}`")),
                    "the guide's lane section does not name allowlisted leaf {method} {leaf}"
                );
            }
        }

        let spelled = match WORKSPACE_ALLOWLIST.len() {
            18 => "eighteen",
            n => panic!("the lane now carries {n} calls; respell the guide's count and this match"),
        };
        assert!(
            section.contains(&format!("carry {spelled} upstream calls")),
            "the guide's lane section no longer states the call count as {spelled:?}"
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
            (
                format!("/v1/rooms/persistent/{room}/workspace/repo/ci?actor_id=alice&limit=5"),
                "/api/v1/rooms/workspace-room/workspace/repo/ci",
            ),
            (
                format!("/v1/rooms/persistent/{room}/workspace/file?actor_id=alice&path=readme.md"),
                "/api/v1/rooms/workspace-room/workspace/file",
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

        // The exposure pair, and with it the only leaf whose upstream path is
        // not fully known to the table: `ports/close` arrives as one joined
        // leaf like `repo/clone`, and the port it names has to travel from the
        // body into the path before the request is built.
        for (leaf, expected_upstream) in [
            ("ports", "/api/v1/rooms/workspace-room/workspace/ports"),
            (
                "ports/close",
                "/api/v1/rooms/workspace-room/workspace/ports/8080",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!(
                            "/v1/rooms/persistent/{room}/workspace/{leaf}?actor_id=alice"
                        ))
                        .header(axum::http::header::CONTENT_TYPE, "application/json")
                        .body(Body::from("{\"port\":8080}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{leaf}");
            let seen = fixture.seen.calls();
            assert_eq!(
                seen.last().map(|call| call.path.as_str()),
                Some(expected_upstream),
                "{leaf}"
            );
        }

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
            8,
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

    /// A `{"command": ...}` body padded so its SHAPED encoding — after the
    /// daemon strips the client's actor claim and installs its own — is
    /// exactly `target` bytes. The daemon's insert counts against the budget,
    /// which is why the padding is computed net of it rather than net of the
    /// wire bytes the client sent.
    fn body_shaped_to(target: usize) -> Value {
        let overhead = serde_json::to_vec(&json!({
            "command": "",
            ACTOR_MEMBER_ID: LOCAL_MEMBER,
        }))
        .expect("overhead encoding")
        .len();
        json!({"command": "x".repeat(target - overhead)})
    }

    /// The 32 KiB cap is wire contract — the operator guide names the 413 and
    /// its code — and it bounds the shaped body, so the attribution a client
    /// cannot control still spends the budget it can. One byte past the cap is
    /// the typed refusal with nothing forwarded, so an oversized body never
    /// spends the bearer; AT the cap the call forwards whole, so the bound is
    /// a limit and not a fencepost.
    ///
    /// Mutation: delete the `encoded > WORKSPACE_REQUEST_LIMIT` arm from
    /// `shape_body`, or relax `>` to `>=` -> RED.
    #[tokio::test]
    async fn a_body_over_the_cap_is_refused_and_a_body_at_the_cap_is_forwarded() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "exec".to_string())),
            query(&[("actor_id", "alice")]),
            Json(body_shaped_to(WORKSPACE_REQUEST_LIMIT + 1)),
        )
        .await;
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body["code"], json!("workspace_request_too_large"));
        assert!(
            fixture.seen.calls().is_empty(),
            "an oversized body must not cause an upstream request"
        );

        let response = room_workspace_command(
            State(fixture.state.clone()),
            Path((fixture.key.as_str().to_string(), "exec".to_string())),
            query(&[("actor_id", "alice")]),
            Json(body_shaped_to(WORKSPACE_REQUEST_LIMIT)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let calls = fixture.seen.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            serde_json::to_vec(&calls[0].body)
                .expect("forwarded body")
                .len(),
            WORKSPACE_REQUEST_LIMIT,
            "the largest legal body arrives whole"
        );

        fixture.close();
    }

    /// The documented 413 through the REAL router, on both sides of the slack.
    ///
    /// `BODY_LIMIT_SLACK` exists so a body a little over the cap still reaches
    /// `shape_body` and gets the typed JSON, while the route's
    /// `DefaultBodyLimit` — without which axum-core's 2 MiB default would make
    /// both numbers fiction — refuses anything past cap + slack before
    /// buffering it, with axum's own untyped 413. One request either side of
    /// the layer's bound pins both that it is wired and the size it is wired
    /// to; neither request may cost an upstream call.
    ///
    /// Mutation: drop the route's `DefaultBodyLimit` layer in `main.rs` -> RED
    /// (the past-slack body would reach the handler and answer with the typed
    /// code); size the layer at the bare cap without slack -> RED (the
    /// in-slack body would get the untyped refusal instead of the typed one).
    #[tokio::test]
    async fn the_slack_window_gives_the_typed_413_and_the_layer_refuses_past_it() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tmp = tempfile::tempdir().unwrap();
        let fixture = federated_room(&tmp).await;
        let app = crate::room_routes().with_state(fixture.state.clone());
        let room = fixture.key.as_str();
        let uri = format!("/v1/rooms/persistent/{room}/workspace/exec?actor_id=alice");

        // Built raw rather than through `body_shaped_to`: the layer judges the
        // WIRE length while the handler judges the shaped one, and the actor
        // insert makes the shaped length 39 bytes larger — a body shaped to
        // cap + 1 sits under the bare cap on the wire and would reach the
        // handler with no slack at all. This one is cap + 14 on the wire,
        // inside the window, and cap + 53 shaped, still past the handler's
        // bound. The premise assert keeps it inside the window if either
        // constant moves.
        let in_slack = format!(
            "{{\"command\":\"{}\"}}",
            "x".repeat(WORKSPACE_REQUEST_LIMIT)
        );
        assert!(
            in_slack.len() > WORKSPACE_REQUEST_LIMIT
                && in_slack.len() <= WORKSPACE_REQUEST_LIMIT + BODY_LIMIT_SLACK,
            "the in-slack body must land inside the slack window"
        );
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&uri)
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(in_slack))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            body["code"],
            json!("workspace_request_too_large"),
            "inside the slack the handler answers, typed"
        );

        let past_slack = format!(
            "{{\"command\":\"{}\"}}",
            "x".repeat(WORKSPACE_REQUEST_LIMIT + BODY_LIMIT_SLACK)
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&uri)
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(past_slack))
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, body) = body_of(response).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            body,
            Value::Null,
            "past the slack the layer answers before the handler; a typed body here would mean the layer was not wired"
        );

        assert!(
            fixture.seen.calls().is_empty(),
            "neither refusal may cause an upstream request"
        );

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
