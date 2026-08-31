//! Restart-safe outbound federation bridge (Gate-2 S2 P2-B).
//!
//! Networking lives here; SQLite authority stays in `ocean-store`. No store
//! guard crosses an `.await`, POST responses never create transcript state,
//! and every public-surface wake is emitted only after the store commit.

use std::{
    collections::{HashMap, HashSet},
    future::{poll_fn, Future},
    net::IpAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    task::Poll,
    time::Duration,
};

use eventsource_stream::Eventsource;
use ocean_core::{
    evaluate_trigger_policy, FederatedActorType, FederatedRoomMemberProjection, FederatedRoomRole,
    InviteResponse, MemberPresence, PublicAgentDescriptor, RoomAccessProjection, RoomAccessState,
    RoomKey, RoomMessageKind, RoomOutboxItem, RoomParticipantKind, RoomReadCursorProjection,
    RoomRedeemResponse, RoomTriggerEvent,
};
use ocean_store::{ConfirmedEvent, IngestOutcome, PendingRedemption, RoomCredential, RoomStore};
use reqwest::{redirect::Policy, Client, StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    sync::{mpsc, Mutex, Notify},
    task::JoinHandle,
    time::{sleep, Instant},
};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::persistent_rooms::{
    publish_room_access_wake_on, publish_room_read_cursor_wake_on, publish_room_wake_on,
    with_rooms_handle, RoomAccessWakeBus, RoomReadCursorWakeBus, RoomStoreHandle, RoomWakeBus,
};

const FEDERATION_URL_ENV: &str = "OCEAN_FEDERATION_URL";
const FEDERATION_OWNER_TOKEN_ENV: &str = "OCEAN_FEDERATION_OWNER_TOKEN";
const RECOVERY_CONCURRENCY: usize = 4;
const REVOKED_STORE_SENTINEL: &str = "room access revoked";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(35);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Read-stall bound for the long room-scoped lane, and the ceiling on what
/// [`FederationSupervisor::send_room_scoped`] may be asked to wait for.
///
/// `READ_TIMEOUT` is a stall detector for the control plane and for the SSE
/// stream, where bytes keep arriving and 35s of silence means a dead peer. On a
/// workspace command nothing arrives at all until the container has finished
/// the command AND flushed itself back to Bedrock, so silence there is the
/// ordinary case. Bedrock runs a command for up to `EXEC_TIMEOUT_MAX_MS` — 900s
/// (`src/compute/driver.mjs`) — so a lane cutting in under that would refuse
/// work the upstream was still legally doing. Hence a second client rather than
/// a longer `READ_TIMEOUT`: raising the shared one would blind the SSE stream
/// to a peer that really has gone away.
pub(super) const ROOM_SCOPED_READ_TIMEOUT: Duration = Duration::from_secs(1_020);
const BODY_LIMIT: usize = 64 * 1024;
/// Ceiling on a single raw frame from a room's SSE stream.
///
/// This is a denial-of-service bound, not a correctness one, and it is
/// deliberately far above `BODY_LIMIT`. A frame between the two is still read
/// to completion, recognised as unrepresentable, and stepped over at the parse
/// level. If the raw bound tripped at `BODY_LIMIT` instead, an oversized row
/// would kill the byte stream before its sequence was ever visible — and with
/// no sequence there is nothing to advance past, so the room would reconnect
/// on the same cursor forever and never receive another message.
const SSE_EVENT_LIMIT: usize = 1024 * 1024;
/// The largest message body this daemon will put on the federated wire.
///
/// The receive side cannot represent a ledger row whose JSON exceeds
/// `BODY_LIMIT`, so the write side has to stay far enough under it that JSON
/// escaping still fits: one source byte can cost six on the wire (`\u001f`),
/// and the ledger envelope rides along too. 8 KiB of body is at most ~48 KiB
/// escaped, which leaves the envelope room to spare.
///
/// This cap is what stops this daemon from ever *creating* the poison row that
/// `SSE_EVENT_LIMIT` and the skip path exist to survive.
pub(super) const OUTBOUND_MESSAGE_BODY_LIMIT: usize = 8 * 1024;

// The two relationships the three constants above only work because of. Both
// are compile-time so a future edit to any one of them fails the build rather
// than quietly reintroducing an unreadable row.
const _: () = {
    // The write cap must survive the worst expansion JSON escaping can inflict
    // — six wire bytes for one source byte — and still fit under what the
    // receive side can parse.
    assert!(OUTBOUND_MESSAGE_BODY_LIMIT * 6 < BODY_LIMIT);
    // The raw frame bound must sit ABOVE the parse limit, so an oversized row
    // still arrives complete enough for its sequence to be read and stepped
    // over instead of killing the byte stream on the way in.
    assert!(SSE_EVENT_LIMIT > BODY_LIMIT);
};
const SENDER_SCAN_INTERVAL: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BridgeError {
    InvalidConfig,
    Transport,
    Protocol,
    Store,
    Revoked,
}

/// What one room-scoped call may spend: bytes off the wire, and seconds on the
/// clock.
///
/// One type rather than two arguments because it is one policy — what a
/// legitimate answer to THIS route costs — and only the caller that knows the
/// route can size either half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct RelayBudget {
    /// Ceiling on the reply read back. Nothing streams and nothing truncates;
    /// the JSON seam treats a reply over this as a protocol error, while the
    /// raw seam reports it as [`RawReply::OverCap`] for the caller to refuse
    /// in its own vocabulary.
    pub(super) body_limit: usize,
    /// How long to wait for the answer. Must not exceed
    /// [`ROOM_SCOPED_READ_TIMEOUT`], or this transport's own read bound cuts
    /// the call first and the number here is a fiction. The workspace lane
    /// checks that relationship at compile time.
    pub(super) timeout: Duration,
}

/// What [`FederationSupervisor::send_room_scoped_raw`] read off the wire.
///
/// Over-budget is a STATE here rather than an error because on the route this
/// seam exists for (`workspace/file`) nothing upstream bounds the body at all
/// — Bedrock buffers whole files — so a reply past the cap is an ordinary big
/// file the caller must refuse with its own typed code, not the
/// [`IntentError::Protocol`] a malformed peer earns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RawReply {
    /// The whole body, within the caller's budget.
    Body(Vec<u8>),
    /// The reply outgrew the budget; the bytes read so far were discarded.
    OverCap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IntentError {
    Invalid,
    NotFound,
    Conflict,
    Forbidden,
    InviteForbidden,
    Unavailable,
    Protocol,
    Store,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardedMutationError {
    Generation,
    NotFound,
    Store,
}

/// Upper bound on convergence retries in [`converge_room_read_cursor_mirror`].
/// Real contention is at most a handful of in-flight GET/PATCH/push-frame
/// writers per room/principal; this only guards against pathological churn
/// so the mutate-guarded write path can never spin unbounded.
const READ_CURSOR_MIRROR_CONVERGE_ATTEMPTS: usize = 8;

/// Apply a read-cursor mirror write with a bounded convergence retry (F2).
///
/// The store's `set_room_read_cursor_mirror` is a strict compare-and-swap:
/// a write is rejected as `Stale` whenever the on-disk mirror no longer
/// equals the caller's `expected_prior_mirror` snapshot, regardless of
/// whether the value this call is carrying is actually newer than what's
/// there. Two upstream round trips (GET poll, PATCH, or a `room_read_cursor`
/// push frame) can both succeed and race each other; without a retry, the
/// response that merely lands second is dropped outright even when its
/// `sequence` is the numerically newest one seen — a silent regression of
/// the mirror to whichever response happened to land first.
///
/// This wrapper retries a losing `Some(sequence)` write against the fresh
/// on-disk value as long as the value it carries is still strictly newer,
/// so concurrent successful writes converge to the newest authoritative
/// sequence instead of the loser being dropped. An authoritative clear
/// (`sequence: None`) is never retried — it stays strictly CAS-protected,
/// exactly as before: a clear that is stale relative to a fresher `Some`
/// mirror is rejected outright, and a losing `Some` write whose value is
/// not actually newer than what's already on disk is also left rejected
/// rather than forced.
fn converge_room_read_cursor_mirror(
    rooms: &RoomStoreHandle,
    key: &RoomKey,
    principal_id: &str,
    mut expected_prior_mirror: Option<u64>,
    sequence: Option<u64>,
) -> Result<ocean_store::RoomReadCursorMirrorCas, ocean_store::RoomStoreError> {
    for _ in 0..READ_CURSOR_MIRROR_CONVERGE_ATTEMPTS {
        let cas = with_rooms_handle(rooms, |store| {
            store.set_room_read_cursor_mirror(key, principal_id, expected_prior_mirror, sequence)
        })?;
        let current = match &cas {
            ocean_store::RoomReadCursorMirrorCas::Applied(_) => return Ok(cas),
            ocean_store::RoomReadCursorMirrorCas::Stale(projection) => {
                projection.mirrored_upstream_read_seq
            }
        };
        let candidate = match sequence {
            Some(candidate) => candidate,
            // A clear is never retried: staying strictly CAS-protected is
            // the whole point of an authoritative clear.
            None => return Ok(cas),
        };
        let is_newer = match current {
            Some(existing) => candidate > existing,
            None => true,
        };
        if !is_newer {
            // Our write is not actually newer than what already landed —
            // this is the ordinary, correct "we lost the race and that's
            // fine" outcome, not something to retry.
            return Ok(cas);
        }
        expected_prior_mirror = current;
    }
    // Retry budget exhausted under pathological contention: return the last
    // attempt's outcome (whatever it is) rather than looping forever.
    with_rooms_handle(rooms, |store| {
        store.set_room_read_cursor_mirror(key, principal_id, expected_prior_mirror, sequence)
    })
}

#[derive(Clone)]
pub(super) struct AgentRegistrationInput {
    pub(super) agent_name: String,
    pub(super) registration_key: String,
    pub(super) descriptor: PublicAgentDescriptor,
}

#[derive(Debug, Clone)]
pub(super) struct FederatedTriggerDispatch {
    pub(super) room: RoomKey,
    pub(super) ledger_event_id: String,
    pub(super) local_seq: u64,
    pub(super) target_member_id: String,
    /// The evaluator's wording for WHY this convene fired, quoted verbatim
    /// into the dispatcher's `room_trigger` payload — a build-failure convene
    /// must not log itself as a mention.
    pub(super) reason: String,
}

#[derive(Debug, Deserialize)]
struct ControlEnvelopeMember {
    member_id: String,
    #[serde(default)]
    owner_member_id: Option<String>,
    actor_type: FederatedActorType,
    role_in_room: FederatedRoomRole,
    display_name: String,
    #[serde(default)]
    public_agent_descriptor: Option<PublicAgentDescriptor>,
}

#[derive(Debug, Deserialize)]
struct RegisterEnvelope {
    room_id: String,
    owner: ControlEnvelopeMember,
}

#[derive(Deserialize)]
struct InviteEnvelope {
    code: String,
    invite: InviteRecord,
}

#[derive(Debug, Deserialize)]
struct InviteRecord {
    role: String,
    scopes: Vec<String>,
    #[serde(rename = "expiresAt")]
    expires_at: String,
}

#[derive(Debug, Deserialize)]
struct RedeemEnvelope {
    invite: InviteRecord,
    record: TokenRecord,
    /// Presence is forbidden even when Bedrock sends `"token": null`.
    #[serde(
        default,
        rename = "token",
        deserialize_with = "deserialize_present_field"
    )]
    token_present: bool,
}

fn deserialize_present_field<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let _ = Value::deserialize(deserializer)?;
    Ok(true)
}

#[derive(Debug, Deserialize)]
struct TokenRecord {
    role: String,
    scopes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct SelfJoinEnvelope {
    member: ControlEnvelopeMember,
}

#[derive(Debug, Deserialize)]
struct AgentMembersEnvelope {
    members: Vec<ControlEnvelopeMember>,
}

#[derive(Serialize)]
struct AgentRegistrationWire<'a> {
    registration_key: &'a str,
    display_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_alias: Option<&'a str>,
    skills_count: u32,
    subagent_names: &'a [String],
}

#[derive(Serialize)]
struct AgentRegistrationBatch<'a> {
    agents: Vec<AgentRegistrationWire<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawSseError {
    Transport,
    EventTooLarge,
}

struct RawSseEventBound {
    limit: usize,
    event_bytes: usize,
    line_content_bytes: usize,
}

impl RawSseEventBound {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            event_bytes: 0,
            line_content_bytes: 0,
        }
    }

    /// Inspect raw chunks BEFORE eventsource-stream appends them to its String
    /// buffer. A blank SSE line resets the per-event budget; an incomplete
    /// event can never grow beyond `limit` across chunk boundaries.
    fn accept(&mut self, bytes: &[u8]) -> Result<(), RawSseError> {
        for &byte in bytes {
            self.event_bytes = self.event_bytes.saturating_add(1);
            if self.event_bytes > self.limit {
                return Err(RawSseError::EventTooLarge);
            }
            match byte {
                b'\n' => {
                    if self.line_content_bytes == 0 {
                        self.event_bytes = 0;
                    }
                    self.line_content_bytes = 0;
                }
                b'\r' => {}
                _ => self.line_content_bytes = self.line_content_bytes.saturating_add(1),
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct FederationClient {
    base: Url,
    http: Client,
    /// The same origin and the same hardening, differing only in how long a
    /// silent socket is allowed to stay silent. See [`ROOM_SCOPED_READ_TIMEOUT`].
    room_scoped_http: Client,
}

impl FederationClient {
    fn from_env() -> Result<Option<Self>, BridgeError> {
        let raw = match std::env::var(FEDERATION_URL_ENV) {
            Ok(raw) if !raw.trim().is_empty() => raw,
            _ => return Ok(None),
        };
        Self::new(&raw).map(Some)
    }

    fn new(raw: &str) -> Result<Self, BridgeError> {
        let mut base = Url::parse(raw).map_err(|_| BridgeError::InvalidConfig)?;
        let authority_has_userinfo = raw
            .split_once("://")
            .and_then(|(_, rest)| rest.split('/').next())
            .is_some_and(|authority| authority.contains('@'));
        if authority_has_userinfo
            || base.username() != ""
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || (base.path() != "/" && !base.path().is_empty())
        {
            return Err(BridgeError::InvalidConfig);
        }
        let host = base.host_str().ok_or(BridgeError::InvalidConfig)?;
        let loopback = loopback_host(host);
        match base.scheme() {
            "https" => {}
            "http" if loopback => {}
            _ => return Err(BridgeError::InvalidConfig),
        }
        base.set_path("/");
        let http = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|_| BridgeError::InvalidConfig)?;
        let room_scoped_http = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(ROOM_SCOPED_READ_TIMEOUT)
            .build()
            .map_err(|_| BridgeError::InvalidConfig)?;
        Ok(Self {
            base,
            http,
            room_scoped_http,
        })
    }

    fn endpoint(&self, segments: &[&str]) -> Result<Url, BridgeError> {
        let mut url = self.base.clone();
        let mut path = url
            .path_segments_mut()
            .map_err(|_| BridgeError::InvalidConfig)?;
        path.pop_if_empty();
        path.extend(segments);
        drop(path);
        Ok(url)
    }

    fn room_endpoint(&self, key: &RoomKey, leaf: &str) -> Result<Url, BridgeError> {
        self.endpoint(&["api", "v1", "rooms", key.as_str(), leaf])
    }

    fn ledger_endpoint(&self) -> Result<Url, BridgeError> {
        self.endpoint(&["api", "v1", "ledger", "events"])
    }

    /// Bedrock's public onboarding manifest for a freshly minted invite code.
    ///
    /// `None` for a loopback base. `new` accepts `http://localhost:8787` so a
    /// dev daemon can federate against a Bedrock on the same machine, but that
    /// origin resolves on the INVITEE's machine, not the owner's: the link is
    /// either dead or it hands a bearer grant to whatever else is listening on
    /// their loopback. Sending no link is strictly better than either, and the
    /// owner still has `code`.
    ///
    /// `None` too if the base somehow refuses segments — the invite already
    /// exists on Bedrock by the time this runs, so failing to decorate it must
    /// never fail the mint.
    fn invite_onboard_url(&self, code: &str) -> Option<String> {
        if self.base.host_str().is_none_or(loopback_host) {
            return None;
        }
        self.endpoint(&["api", "v1", "invites", code, "onboard"])
            .ok()
            .map(String::from)
    }
}

/// Loopback by host alone, scheme irrelevant: an `https://127.0.0.1` origin is
/// no more reachable from an invitee's machine than an `http` one.
fn loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

#[derive(Clone)]
pub(super) struct FederationSupervisor {
    inner: Arc<SupervisorInner>,
}

struct SupervisorInit {
    client: Option<FederationClient>,
    owner_token: Option<String>,
    invalid_config: bool,
    rooms: RoomStoreHandle,
    room_wakes: RoomWakeBus,
    access_wakes: RoomAccessWakeBus,
    read_cursor_wakes: RoomReadCursorWakeBus,
    trigger_tx: mpsc::UnboundedSender<FederatedTriggerDispatch>,
    shutdown: CancellationToken,
    scan_interval: Duration,
}

struct SupervisorInner {
    client: Option<FederationClient>,
    owner_token: Option<String>,
    invalid_config: bool,
    rooms: RoomStoreHandle,
    room_wakes: RoomWakeBus,
    access_wakes: RoomAccessWakeBus,
    read_cursor_wakes: RoomReadCursorWakeBus,
    trigger_tx: mpsc::UnboundedSender<FederatedTriggerDispatch>,
    shutdown: CancellationToken,
    slots: Mutex<HashMap<RoomKey, Arc<RoomSlot>>>,
    recovery: Mutex<Option<JoinHandle<()>>>,
    shutting_down: AtomicBool,
    next_generation: AtomicU64,
    scan_interval: Duration,
}

struct RoomSlot {
    state: Mutex<Option<RunningRoom>>,
    control: Arc<AdmissionGate>,
    generation: AtomicU64,
    credential_lock: Mutex<()>,
}

impl Default for RoomSlot {
    fn default() -> Self {
        Self {
            state: Mutex::new(None),
            control: Arc::new(AdmissionGate::new()),
            generation: AtomicU64::new(0),
            credential_lock: Mutex::new(()),
        }
    }
}

struct AdmissionGate {
    open: Mutex<bool>,
}

impl AdmissionGate {
    fn new() -> Self {
        Self {
            open: Mutex::new(true),
        }
    }

    async fn close(&self) {
        *self.open.lock().await = false;
    }

    async fn mutate<T>(&self, mutate: impl FnOnce() -> T) -> Option<T> {
        let open = self.open.lock().await;
        if !*open {
            return None;
        }
        Some(mutate())
    }

    /// Linearize revoke close against request initiation. The gate lock stays
    /// held through the request future's FIRST poll; if close wins, the future
    /// is never polled and no POST starts. If admission wins, that first poll
    /// defines the pre-close in-flight request allowed by freeze v2.2.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        cancel: &CancellationToken,
    ) -> AdmittedSend {
        let open = self.open.lock().await;
        if !*open {
            return AdmittedSend::Closed;
        }
        let mut future = Box::pin(request.send());
        let first = poll_fn(|cx| {
            Poll::Ready(match future.as_mut().poll(cx) {
                Poll::Ready(result) => Some(result),
                Poll::Pending => None,
            })
        })
        .await;
        drop(open);
        if let Some(result) = first {
            return AdmittedSend::Response(result);
        }
        tokio::select! {
            _ = cancel.cancelled() => AdmittedSend::Cancelled,
            result = future => AdmittedSend::Response(result),
        }
    }
}

enum AdmittedSend {
    Closed,
    Cancelled,
    Response(Result<reqwest::Response, reqwest::Error>),
}

struct RunningRoom {
    cancel: CancellationToken,
    /// P2-C enqueue seam: retained before routes use it.
    #[allow(dead_code)]
    sender_notify: Arc<Notify>,
    join: JoinHandle<()>,
}

impl FederationSupervisor {
    pub(super) fn from_env(
        rooms: RoomStoreHandle,
        room_wakes: RoomWakeBus,
        access_wakes: RoomAccessWakeBus,
        read_cursor_wakes: RoomReadCursorWakeBus,
        trigger_tx: mpsc::UnboundedSender<FederatedTriggerDispatch>,
        shutdown: CancellationToken,
    ) -> Self {
        let (client, invalid_config) = match FederationClient::from_env() {
            Ok(client) => (client, false),
            Err(_) => (None, true),
        };
        let owner_token = std::env::var(FEDERATION_OWNER_TOKEN_ENV)
            .ok()
            .filter(|token| {
                !token.is_empty() && token == token.trim() && !token.chars().any(char::is_control)
            });
        Self::new_inner(SupervisorInit {
            client,
            owner_token,
            invalid_config,
            rooms,
            room_wakes,
            access_wakes,
            read_cursor_wakes,
            trigger_tx,
            shutdown,
            scan_interval: SENDER_SCAN_INTERVAL,
        })
    }

    #[cfg(test)]
    pub(super) fn for_test(
        base: &str,
        rooms: RoomStoreHandle,
        room_wakes: RoomWakeBus,
        access_wakes: RoomAccessWakeBus,
        read_cursor_wakes: RoomReadCursorWakeBus,
        shutdown: CancellationToken,
        scan_interval: Duration,
    ) -> Self {
        let (trigger_tx, _) = mpsc::unbounded_channel();
        Self::for_test_with_trigger(
            base,
            rooms,
            room_wakes,
            access_wakes,
            read_cursor_wakes,
            trigger_tx,
            shutdown,
            scan_interval,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn for_test_with_trigger(
        base: &str,
        rooms: RoomStoreHandle,
        room_wakes: RoomWakeBus,
        access_wakes: RoomAccessWakeBus,
        read_cursor_wakes: RoomReadCursorWakeBus,
        trigger_tx: mpsc::UnboundedSender<FederatedTriggerDispatch>,
        shutdown: CancellationToken,
        scan_interval: Duration,
    ) -> Self {
        Self::new_inner(SupervisorInit {
            client: Some(FederationClient::new(base).expect("test URL")),
            owner_token: Some("test-owner-token".into()),
            invalid_config: false,
            rooms,
            room_wakes,
            access_wakes,
            read_cursor_wakes,
            trigger_tx,
            shutdown,
            scan_interval,
        })
    }

    #[cfg(test)]
    pub(super) fn test_disabled(
        rooms: RoomStoreHandle,
        room_wakes: RoomWakeBus,
        access_wakes: RoomAccessWakeBus,
        read_cursor_wakes: RoomReadCursorWakeBus,
        shutdown: CancellationToken,
    ) -> Self {
        let (trigger_tx, _) = mpsc::unbounded_channel();
        Self::new_inner(SupervisorInit {
            client: None,
            owner_token: None,
            invalid_config: false,
            rooms,
            room_wakes,
            access_wakes,
            read_cursor_wakes,
            trigger_tx,
            shutdown,
            scan_interval: SENDER_SCAN_INTERVAL,
        })
    }

    fn new_inner(init: SupervisorInit) -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                client: init.client,
                owner_token: init.owner_token,
                invalid_config: init.invalid_config,
                rooms: init.rooms,
                room_wakes: init.room_wakes,
                access_wakes: init.access_wakes,
                read_cursor_wakes: init.read_cursor_wakes,
                trigger_tx: init.trigger_tx,
                shutdown: init.shutdown,
                slots: Mutex::new(HashMap::new()),
                recovery: Mutex::new(None),
                shutting_down: AtomicBool::new(false),
                next_generation: AtomicU64::new(1),
                scan_interval: init.scan_interval,
            }),
        }
    }

    /// Enumerate durable credentials and start one task tree per non-Revoked
    /// room. Missing/invalid config downgrades those rooms to Recovering so a
    /// previous process can never leave stale Live chrome behind.
    pub(super) async fn startup(&self) {
        let credentials =
            with_rooms_handle(&self.inner.rooms, |store| store.list_credentialed_rooms());
        let Ok(credentials) = credentials else {
            tracing::error!("federation startup could not enumerate credentialed rooms");
            return;
        };
        for credential in credentials {
            let state = with_rooms_handle(&self.inner.rooms, |store| {
                store.room_access(&credential.room_id).map(|p| p.state)
            })
            .map_err(|_| BridgeError::Store);
            match startup_should_start(state) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => {
                    tracing::warn!(
                        room = %credential.room_id,
                        outcome = "startup_projection_read_failed",
                        "federation room not started"
                    );
                    continue;
                }
            }
            if self.inner.client.is_none() {
                let _ = self.persist_lease_lost(&credential.room_id, RoomAccessState::Recovering);
                continue;
            }
            // A configured client is not an authenticated lease. Atomically
            // clear stale presence while entering pre-subscribe Connecting.
            if self
                .persist_lease_lost(&credential.room_id, RoomAccessState::Connecting)
                .is_err()
            {
                continue;
            }
            self.start_room(credential.room_id).await;
        }
        if self.inner.invalid_config {
            tracing::warn!("federation client configuration is invalid; rooms set Recovering");
        }
        let pending =
            with_rooms_handle(&self.inner.rooms, |store| store.list_pending_redemptions());
        if let Ok(pending) = pending {
            let supervisor = self.clone();
            let recovery = tokio::spawn(async move {
                let semaphore = Arc::new(tokio::sync::Semaphore::new(RECOVERY_CONCURRENCY));
                let mut joins = tokio::task::JoinSet::new();
                for row in pending {
                    let Ok(permit) = semaphore.clone().acquire_owned().await else {
                        break;
                    };
                    if supervisor.inner.shutdown.is_cancelled() {
                        break;
                    }
                    let task_supervisor = supervisor.clone();
                    joins.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = task_supervisor.recover_pending(row).await {
                            tracing::warn!(outcome = ?error, "pending federation redemption deferred");
                        }
                    });
                }
                while let Some(result) = joins.join_next().await {
                    if result.is_err() {
                        tracing::warn!(
                            outcome = "pending_redemption_task_failed",
                            "federation redemption recovery task ended unexpectedly"
                        );
                    }
                }
            });
            *self.inner.recovery.lock().await = Some(recovery);
        } else {
            tracing::warn!(
                outcome = "pending_redemption_enumeration_failed",
                "federation redemption recovery deferred"
            );
        }
    }

    /// Idempotently start one room. A start arriving while stop is joining
    /// waits on the slot lock, then starts the next epoch.
    pub(super) async fn start_room(&self, key: RoomKey) {
        if self.inner.shutting_down.load(Ordering::Acquire)
            || self.inner.shutdown.is_cancelled()
            || self.inner.client.is_none()
        {
            return;
        }
        let slot = {
            let mut slots = self.inner.slots.lock().await;
            slots
                .entry(key.clone())
                .or_insert_with(|| Arc::new(RoomSlot::default()))
                .clone()
        };
        let mut state = slot.state.lock().await;
        if state
            .as_ref()
            .is_some_and(|running| !running.join.is_finished())
        {
            return;
        }
        if let Some(stale) = state.take() {
            let _ = stale.join.await;
        }
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return;
        }
        let generation = self.inner.next_generation.fetch_add(1, Ordering::AcqRel);
        if slot
            .control
            .mutate(|| slot.generation.store(generation, Ordering::Release))
            .await
            .is_none()
        {
            return;
        }
        let cancel = self.inner.shutdown.child_token();
        let sender_notify = Arc::new(Notify::new());
        let inner = self.inner.clone();
        let task_key = key.clone();
        let task_cancel = cancel.clone();
        let task_notify = sender_notify.clone();
        let join = tokio::spawn(async move {
            run_room(inner, task_key, generation, task_notify, task_cancel).await;
        });
        *state = Some(RunningRoom {
            cancel,
            sender_notify,
            join,
        });
    }

    /// P2-C calls this after a local outbox commit. The periodic scan is still
    /// the correctness rail, so a missed notify only adds bounded latency.
    #[allow(dead_code)]
    pub(super) async fn wake_sender(&self, key: &RoomKey) {
        let slot = { self.inner.slots.lock().await.get(key).cloned() };
        if let Some(slot) = slot {
            if let Some(running) = slot.state.lock().await.as_ref() {
                running.sender_notify.notify_one();
            }
        }
    }

    /// P2-C calls this on explicit room teardown/revocation. Start waits on
    /// the same slot lock, so stopping and next-epoch start cannot overlap.
    #[allow(dead_code)]
    pub(super) async fn stop_room(&self, key: &RoomKey) {
        let slot = { self.inner.slots.lock().await.get(key).cloned() };
        let Some(slot) = slot else { return };
        let mut state = slot.state.lock().await;
        if let Some(running) = state.take() {
            running.cancel.cancel();
            let _ = running.join.await;
        }
    }

    pub(super) async fn shutdown(&self) {
        self.inner.shutting_down.store(true, Ordering::Release);
        self.inner.shutdown.cancel();
        let recovery = { self.inner.recovery.lock().await.take() };
        if let Some(mut recovery) = recovery {
            if tokio::time::timeout(Duration::from_secs(20), &mut recovery)
                .await
                .is_err()
            {
                recovery.abort();
                let _ = recovery.await;
            }
        }
        let slots: Vec<_> = self.inner.slots.lock().await.values().cloned().collect();
        let mut joins = Vec::new();
        // Cancel every room first so bounded network operations unwind in
        // parallel; then join against one process-wide deadline.
        for slot in slots {
            let mut state = slot.state.lock().await;
            if let Some(running) = state.take() {
                running.cancel.cancel();
                joins.push(running.join);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        for mut join in joins {
            if tokio::time::timeout_at(deadline, &mut join).await.is_err() {
                join.abort();
                let _ = join.await;
            }
        }
    }

    async fn send_unadmitted(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, IntentError> {
        if self.inner.shutting_down.load(Ordering::Acquire) || self.inner.shutdown.is_cancelled() {
            return Err(IntentError::Unavailable);
        }
        tokio::select! {
            biased;
            _ = self.inner.shutdown.cancelled() => Err(IntentError::Unavailable),
            response = request.send() => response.map_err(|_| IntentError::Unavailable),
        }
    }

    async fn slot_for(&self, key: &RoomKey) -> Arc<RoomSlot> {
        self.inner
            .slots
            .lock()
            .await
            .entry(key.clone())
            .or_insert_with(|| Arc::new(RoomSlot::default()))
            .clone()
    }

    /// Forward one ALREADY-AUTHORIZED room-scoped call to Bedrock on that
    /// room's own credential.
    ///
    /// Authorization belongs to the caller; this is transport, and the two
    /// things it guarantees are custody and confinement. Custody: the bearer is
    /// read out of the credential here and never leaves — not into a log, an
    /// error, or the returned value. Confinement: the URL is built from the
    /// credential's OWN room id, so no shape of `leaf_segments` can address
    /// another room's compute, and `endpoint` percent-encodes every segment so
    /// a leaf cannot climb out of the room prefix either.
    ///
    /// The [`RelayBudget`] is the caller's rather than `BODY_LIMIT` and
    /// `REQUEST_TIMEOUT` because callers differ in what a legitimate answer
    /// costs, in bytes and in seconds alike. A ledger row is bounded by what
    /// this daemon will put on the wire and answers in one round trip; a
    /// workspace command's answer is bounded by Bedrock's own output cap, and
    /// arrives only once a container has run the command and flushed itself
    /// back.
    pub(super) async fn send_room_scoped(
        &self,
        credential: &RoomCredential,
        method: reqwest::Method,
        leaf_segments: &[&str],
        query: &[(&str, String)],
        body: Option<&Value>,
        budget: RelayBudget,
    ) -> Result<(StatusCode, Value), IntentError> {
        let response = self
            .send_room_scoped_response(
                credential,
                method,
                leaf_segments,
                query,
                body,
                budget.timeout,
            )
            .await?;
        let status = response.status();
        let payload: Value = read_bounded_json(response, budget.body_limit)
            .await
            .map_err(|_| IntentError::Protocol)?;
        Ok((status, payload))
    }

    /// The raw sibling of [`Self::send_room_scoped`], for the one Bedrock room
    /// route that answers a 2xx in bytes instead of JSON (`workspace/file`).
    /// The request half is shared, so custody, confinement, and the admission
    /// gate a revoke closes are identical; the two seams differ only in how
    /// the reply is read. Refusals on that route are still ordinary JSON
    /// bodies, which is why this returns status plus bytes and leaves parsing
    /// to the caller — only the caller knows which statuses carry which shape.
    pub(super) async fn send_room_scoped_raw(
        &self,
        credential: &RoomCredential,
        method: reqwest::Method,
        leaf_segments: &[&str],
        query: &[(&str, String)],
        budget: RelayBudget,
    ) -> Result<(StatusCode, RawReply), IntentError> {
        let response = self
            .send_room_scoped_response(
                credential,
                method,
                leaf_segments,
                query,
                None,
                budget.timeout,
            )
            .await?;
        let status = response.status();
        match read_bounded_bytes(response, budget.body_limit).await {
            Ok(bytes) => Ok((status, RawReply::Body(bytes))),
            Err(BoundedReadError::OverCap) => Ok((status, RawReply::OverCap)),
            Err(BoundedReadError::Transport) => Err(IntentError::Protocol),
        }
    }

    /// The request half both room-scoped seams share: build, authenticate,
    /// admit, refuse a 1xx/3xx. One function so a hardening change cannot land
    /// on one seam and miss the other.
    async fn send_room_scoped_response(
        &self,
        credential: &RoomCredential,
        method: reqwest::Method,
        leaf_segments: &[&str],
        query: &[(&str, String)],
        body: Option<&Value>,
        timeout: Duration,
    ) -> Result<reqwest::Response, IntentError> {
        let client = self.inner.client.clone().ok_or(IntentError::Unavailable)?;
        let mut segments = vec!["api", "v1", "rooms", credential.room_id.as_str()];
        segments.extend_from_slice(leaf_segments);
        let url = client
            .endpoint(&segments)
            .map_err(|_| IntentError::Unavailable)?;
        let mut request = client
            .room_scoped_http
            .request(method, url)
            .timeout(timeout)
            .bearer_auth(&credential.bearer_token);
        if !query.is_empty() {
            request = request.query(query);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        // Admitted, not unadmitted: a revoked room closes its gate, and a
        // workspace command is exactly the kind of side effect that must not
        // start after the close a revoke already linearized.
        let slot = self.slot_for(&credential.room_id).await;
        let response = match slot.control.send(request, &self.inner.shutdown).await {
            AdmittedSend::Response(Ok(response)) => response,
            AdmittedSend::Response(Err(_)) | AdmittedSend::Cancelled => {
                return Err(IntentError::Unavailable)
            }
            AdmittedSend::Closed => return Err(IntentError::Forbidden),
        };
        let status = response.status();
        // Redirects are disabled on this client, so a 3xx here is an upstream
        // that is not the Bedrock we configured. There is no JSON to relay, and
        // following the Location would take the bearer somewhere unvetted.
        if status.is_informational() || status.is_redirection() {
            return Err(IntentError::Protocol);
        }
        Ok(response)
    }

    pub(super) async fn enqueue_federated_message(
        &self,
        key: &RoomKey,
        author_member_id: Option<&str>,
        body: &str,
    ) -> Result<RoomAccessProjection, IntentError> {
        // The one gate every federated row passes through — HTTP posts and
        // convened agent replies alike. A body past the cap cannot be read
        // back by any peer, and an unreadable row on the ledger used to stop
        // the room for everyone, so it is refused here rather than written.
        if body.len() > OUTBOUND_MESSAGE_BODY_LIMIT {
            return Err(IntentError::Invalid);
        }
        let slot = self.slot_for(key).await;
        let result = slot
            .control
            .mutate(|| {
                with_rooms_handle(&self.inner.rooms, |store| {
                    if store.get(key)?.is_none() {
                        return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()));
                    }
                    let credential = store.room_credential(key)?.ok_or_else(|| {
                        ocean_store::RoomStoreError::RoomNotFederated(key.clone())
                    })?;
                    let access = store.room_access(key)?;
                    if access.state == RoomAccessState::Revoked {
                        return Err(ocean_store::RoomStoreError::FederationCorruption(
                            REVOKED_STORE_SENTINEL.into(),
                        ));
                    }
                    let author = author_member_id.unwrap_or(&credential.local_human_member_id);
                    let member_ids: HashSet<_> = access
                        .members
                        .iter()
                        .map(|member| member.member_id.as_str())
                        .collect();
                    let mentions = crate::persistent_rooms::parse_mentions(body)
                        .into_iter()
                        .filter(|id| member_ids.contains(id.as_str()))
                        .collect();
                    let client_event_id = uuid::Uuid::new_v4().to_string();
                    store.allocate_outbox_pending(
                        key,
                        author,
                        &client_event_id,
                        "message",
                        serde_json::json!({"body": body}),
                        mentions,
                    )?;
                    store.room_access(key)
                })
            })
            .await
            .ok_or(IntentError::Forbidden)?;
        let projection = result.map_err(|error| match error {
            ocean_store::RoomStoreError::UnknownRoom(_) => IntentError::NotFound,
            ocean_store::RoomStoreError::RoomNotFederated(_) => IntentError::Conflict,
            ocean_store::RoomStoreError::FederationCorruption(ref message)
                if message == REVOKED_STORE_SENTINEL =>
            {
                IntentError::Forbidden
            }
            _ => IntentError::Store,
        })?;
        publish_room_access_wake_on(&self.inner.access_wakes, key);
        self.wake_sender(key).await;
        Ok(projection)
    }

    pub(super) async fn create_invite(
        &self,
        key: &RoomKey,
        recipient_name: Option<String>,
        ttl_minutes: u32,
    ) -> Result<InviteResponse, IntentError> {
        let open = with_rooms_handle(&self.inner.rooms, |store| {
            store.get(key).map(|record| record.is_some())
        })
        .map_err(|_| IntentError::Store)?;
        if !open {
            return Err(IntentError::NotFound);
        }
        let slot = self.slot_for(key).await;
        let _credential_guard = slot.credential_lock.lock().await;
        let (room, access, existing) = with_rooms_handle(&self.inner.rooms, |store| {
            let room = store
                .get(key)?
                .ok_or_else(|| ocean_store::RoomStoreError::UnknownRoom(key.clone()))?;
            let access = store.room_access(key)?;
            let credential = store.room_credential(key)?;
            Ok::<_, ocean_store::RoomStoreError>((room.room, access, credential))
        })
        .map_err(|error| match error {
            ocean_store::RoomStoreError::UnknownRoom(_) => IntentError::NotFound,
            _ => IntentError::Store,
        })?;
        if access.state == RoomAccessState::Revoked {
            return Err(IntentError::Forbidden);
        }
        if existing.is_none() {
            if access.state != RoomAccessState::Local {
                return Err(IntentError::Conflict);
            }
            if !canonical_room_key(key.as_str()) {
                return Err(IntentError::Invalid);
            }
        }
        let client = self.inner.client.clone().ok_or(IntentError::Unavailable)?;
        let credential = if let Some(credential) = existing {
            credential
        } else {
            let owner_token = self
                .inner
                .owner_token
                .clone()
                .ok_or(IntentError::Unavailable)?;
            let url = client
                .room_endpoint(key, "register")
                .map_err(|_| IntentError::Unavailable)?;
            let response = self
                .send_unadmitted(
                    client
                        .http
                        .post(url)
                        .timeout(REQUEST_TIMEOUT)
                        .bearer_auth(&owner_token)
                        .json(&serde_json::json!({"title": room.name})),
                )
                .await?;
            match response.status() {
                StatusCode::OK | StatusCode::CREATED => {}
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                    return Err(IntentError::Forbidden)
                }
                StatusCode::CONFLICT => return Err(IntentError::Conflict),
                s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
                    return Err(IntentError::Unavailable)
                }
                _ => return Err(IntentError::Protocol),
            }
            let envelope: RegisterEnvelope = read_bounded_json(response, BODY_LIMIT)
                .await
                .map_err(control_body_error)?;
            if envelope.room_id != key.as_str()
                || !valid_human_member(&envelope.owner, FederatedRoomRole::Owner)
            {
                return Err(IntentError::Protocol);
            }
            let credential = RoomCredential {
                room_id: key.clone(),
                bearer_token: owner_token,
                local_human_member_id: envelope.owner.member_id,
            };
            with_rooms_handle(&self.inner.rooms, |store| {
                if store.get(key)?.is_none() {
                    return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()));
                }
                match store.room_credential(key)? {
                    None => store.install_room_credential(
                        key,
                        &credential.bearer_token,
                        &credential.local_human_member_id,
                    )?,
                    Some(current)
                        if current.bearer_token == credential.bearer_token
                            && current.local_human_member_id
                                == credential.local_human_member_id => {}
                    Some(_) => {
                        return Err(ocean_store::RoomStoreError::FederationCorruption(
                            "credential conflict".into(),
                        ))
                    }
                }
                store.update_room_access_safe(
                    key,
                    Some(RoomAccessState::Connecting),
                    None,
                    None,
                )?;
                Ok::<_, ocean_store::RoomStoreError>(())
            })
            .map_err(|error| match error {
                ocean_store::RoomStoreError::UnknownRoom(_) => IntentError::NotFound,
                ocean_store::RoomStoreError::FederationCorruption(_) => IntentError::Conflict,
                _ => IntentError::Store,
            })?;
            publish_room_access_wake_on(&self.inner.access_wakes, key);
            self.start_room(key.clone()).await;
            credential
        };
        let generation = slot.generation.load(Ordering::Acquire);
        let url = client
            .endpoint(&["api", "v1", "invites"])
            .map_err(|_| IntentError::Unavailable)?;
        let mut body = serde_json::json!({"room_id": key.as_str(), "ttl_minutes": ttl_minutes});
        if let Some(name) = recipient_name {
            body["recipient_name"] = Value::String(name);
        }
        let response = match slot
            .control
            .send(
                client
                    .http
                    .post(url)
                    .timeout(REQUEST_TIMEOUT)
                    .bearer_auth(&credential.bearer_token)
                    .json(&body),
                &self.inner.shutdown,
            )
            .await
        {
            AdmittedSend::Response(Ok(response)) => response,
            AdmittedSend::Response(Err(_)) | AdmittedSend::Cancelled => {
                return Err(IntentError::Unavailable)
            }
            AdmittedSend::Closed => return Err(IntentError::Forbidden),
        };
        match response.status() {
            StatusCode::CREATED => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                self.revoke_control(key).await;
                return Err(IntentError::Forbidden);
            }
            StatusCode::CONFLICT => return Err(IntentError::Conflict),
            s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
                return Err(IntentError::Unavailable)
            }
            _ => return Err(IntentError::Protocol),
        }
        let envelope: InviteEnvelope = read_bounded_json(response, BODY_LIMIT)
            .await
            .map_err(control_body_error)?;
        if slot
            .control
            .mutate(|| slot.generation.load(Ordering::Acquire) == generation)
            .await
            != Some(true)
        {
            return Err(IntentError::Forbidden);
        }
        if envelope.code.is_empty()
            || envelope.invite.role != "contributor"
            || envelope.invite.scopes != vec![format!("/rooms/{}", key.as_str())]
            || envelope.invite.expires_at.is_empty()
        {
            return Err(IntentError::Protocol);
        }
        let onboard_url = client.invite_onboard_url(&envelope.code);
        Ok(InviteResponse {
            code: envelope.code,
            expires_at: envelope.invite.expires_at,
            room_key: key.as_str().into(),
            room_name: room.name,
            onboard_url,
        })
    }

    pub(super) async fn redeem_invite(
        &self,
        code: &str,
    ) -> Result<RoomRedeemResponse, IntentError> {
        let code = code.trim();
        if code.is_empty() {
            return Err(IntentError::Invalid);
        }
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|_| IntentError::Store)?;
        use base64::Engine as _;
        let bearer = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let redemption_id = uuid::Uuid::new_v4().to_string();
        let (pending, _) = with_rooms_handle(&self.inner.rooms, |store| {
            store.get_or_insert_pending_redemption(
                code,
                &redemption_id,
                &bearer,
                chrono::Utc::now(),
            )
        })
        .map_err(|_| IntentError::Store)?;
        self.recover_pending(pending).await
    }

    /// The room key is only ever derivable here — it comes off the redeemed
    /// invite's scope, and the caller holds nothing but an opaque code — so it
    /// leaves with the projection rather than being dropped for the redeemer
    /// to reconstruct by diffing its own room list.
    async fn recover_pending(
        &self,
        pending: PendingRedemption,
    ) -> Result<RoomRedeemResponse, IntentError> {
        let client = self.inner.client.clone().ok_or(IntentError::Unavailable)?;
        let url = client
            .endpoint(&["api", "v1", "invites", "redeem"])
            .map_err(|_| IntentError::Unavailable)?;
        let response = self
            .send_unadmitted(client.http.post(url).timeout(REQUEST_TIMEOUT).json(
                &serde_json::json!({
                    "code": pending.invite_code,
                    "redemption_id": pending.redemption_id,
                    "token": pending.bearer_token
                }),
            ))
            .await?;
        match response.status() {
            StatusCode::OK | StatusCode::CREATED => {}
            StatusCode::FORBIDDEN => {
                self.remove_pending(&pending)?;
                return Err(IntentError::InviteForbidden);
            }
            StatusCode::CONFLICT => return Err(IntentError::Conflict),
            s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
                return Err(IntentError::Unavailable)
            }
            _ => return Err(IntentError::Protocol),
        }
        let envelope: RedeemEnvelope = read_bounded_json(response, BODY_LIMIT)
            .await
            .map_err(control_body_error)?;
        if envelope.token_present
            || envelope.record.role != "contributor"
            || envelope.invite.role != "contributor"
            || envelope.record.scopes.len() != 1
            || envelope.invite.scopes != envelope.record.scopes
        {
            return Err(IntentError::Protocol);
        }
        let scope = &envelope.record.scopes[0];
        let room_id = scope
            .strip_prefix("/rooms/")
            .filter(|id| canonical_room_key(id))
            .ok_or(IntentError::Protocol)?;
        let key = RoomKey::new(room_id);
        let url = client
            .endpoint(&["api", "v1", "rooms", key.as_str(), "members", "self"])
            .map_err(|_| IntentError::Unavailable)?;
        let response = self
            .send_unadmitted(
                client
                    .http
                    .post(url)
                    .timeout(REQUEST_TIMEOUT)
                    .bearer_auth(&pending.bearer_token),
            )
            .await?;
        match response.status() {
            StatusCode::OK | StatusCode::CREATED => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                self.remove_pending(&pending)?;
                return Err(IntentError::InviteForbidden);
            }
            StatusCode::CONFLICT => return Err(IntentError::Conflict),
            s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
                return Err(IntentError::Unavailable)
            }
            _ => return Err(IntentError::Protocol),
        }
        let joined: SelfJoinEnvelope = read_bounded_json(response, BODY_LIMIT)
            .await
            .map_err(control_body_error)?;
        if !valid_human_member(&joined.member, FederatedRoomRole::Member) {
            return Err(IntentError::Protocol);
        }
        let slot = self.slot_for(&key).await;
        let _guard = slot.credential_lock.lock().await;
        with_rooms_handle(&self.inner.rooms, |store| {
            match (store.get(&key)?, store.get_including_closed(&key)?) {
                (None, Some(_)) => {
                    return Err(ocean_store::RoomStoreError::AlreadyExists(key.clone()))
                }
                (None, None) => {
                    store.create(key.clone(), key.as_str(), None, chrono::Utc::now())?;
                }
                (Some(_), _) => {}
            }
            if store.room_access(&key)?.state == RoomAccessState::Revoked {
                return Err(ocean_store::RoomStoreError::FederationCorruption(
                    REVOKED_STORE_SENTINEL.into(),
                ));
            }
            if let Some(current) = store.room_credential(&key)? {
                if current.bearer_token != pending.bearer_token
                    || current.local_human_member_id != joined.member.member_id
                {
                    return Err(ocean_store::RoomStoreError::FederationCorruption(
                        "credential conflict".into(),
                    ));
                }
            }
            store.promote_pending_redemption(
                &pending.redemption_id,
                &key,
                &pending.bearer_token,
                &joined.member.member_id,
            )?;
            store.update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
        })
        .map_err(|error| match error {
            ocean_store::RoomStoreError::FederationCorruption(ref message)
                if message == REVOKED_STORE_SENTINEL =>
            {
                IntentError::Forbidden
            }
            ocean_store::RoomStoreError::AlreadyExists(_)
            | ocean_store::RoomStoreError::FederationCorruption(_) => IntentError::Conflict,
            _ => IntentError::Store,
        })?;
        publish_room_access_wake_on(&self.inner.access_wakes, &key);
        self.start_room(key.clone()).await;
        let access = with_rooms_handle(&self.inner.rooms, |store| store.room_access(&key))
            .map_err(|_| IntentError::Store)?;
        Ok(RoomRedeemResponse {
            access,
            room_key: key.as_str().into(),
        })
    }

    fn remove_pending(&self, pending: &PendingRedemption) -> Result<(), IntentError> {
        with_rooms_handle(&self.inner.rooms, |store| {
            store.remove_pending_redemption(&pending.redemption_id)
        })
        .map(|_| ())
        .map_err(|_| IntentError::Store)
    }

    pub(super) async fn room_get_read_cursor(
        &self,
        key: &RoomKey,
    ) -> Result<RoomReadCursorProjection, IntentError> {
        let slot = self.slot_for(key).await;
        let (credential, access) = with_rooms_handle(&self.inner.rooms, |store| {
            if store.get(key)?.is_none() {
                return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()));
            }
            Ok::<_, ocean_store::RoomStoreError>((
                store.room_credential(key)?,
                store.room_access(key)?,
            ))
        })
        .map_err(|error| match error {
            ocean_store::RoomStoreError::UnknownRoom(_) => IntentError::NotFound,
            _ => IntentError::Store,
        })?;
        let credential = credential.ok_or(IntentError::Conflict)?;
        if access.state != RoomAccessState::Live {
            return Err(IntentError::Conflict);
        }
        let client = self.inner.client.clone().ok_or(IntentError::Unavailable)?;
        let url = client
            .endpoint(&["api", "v1", "rooms", key.as_str(), "read-cursor"])
            .map_err(|_| IntentError::Unavailable)?;
        let generation = slot.generation.load(Ordering::Acquire);
        // The store's mirror write is a compare-and-swap (M5): snapshot the
        // expected prior mirror at the SAME point we snapshot `generation`
        // (right before issuing the request) so a fresher response that
        // lands from a concurrent GET/PATCH/frame while this one is in
        // flight is detected and this write is rejected as stale instead of
        // clobbering it.
        let expected_prior_mirror = with_rooms_handle(&self.inner.rooms, |store| {
            store.room_read_cursor(key, &credential.local_human_member_id)
        })
        .map_err(|_| IntentError::Store)?
        .mirrored_upstream_read_seq;
        let response = match slot
            .control
            .send(
                client
                    .http
                    .get(url)
                    .timeout(REQUEST_TIMEOUT)
                    .bearer_auth(&credential.bearer_token),
                &self.inner.shutdown,
            )
            .await
        {
            AdmittedSend::Response(Ok(response)) => response,
            AdmittedSend::Response(Err(_)) | AdmittedSend::Cancelled => {
                return Err(IntentError::Unavailable)
            }
            AdmittedSend::Closed => return Err(IntentError::Forbidden),
        };
        match response.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                self.revoke_control(key).await;
                return Err(IntentError::Forbidden);
            }
            StatusCode::NOT_FOUND
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::NOT_IMPLEMENTED => return Err(IntentError::Conflict),
            s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
                return Err(IntentError::Unavailable)
            }
            _ => return Err(IntentError::Protocol),
        }
        let body: ReadCursorBody = read_bounded_json(response, BODY_LIMIT)
            .await
            .map_err(control_body_error)?;
        if body.room_id != key.as_str() {
            return Err(IntentError::Protocol);
        }
        let sequence = match body.sequence {
            Some(sequence) => {
                Some(parse_canonical_u64(&sequence).map_err(|_| IntentError::Protocol)?)
            }
            None => None,
        };
        let cas = slot
            .control
            .mutate(|| {
                if slot.generation.load(Ordering::Acquire) != generation {
                    return Err(GuardedMutationError::Generation);
                }
                let result = converge_room_read_cursor_mirror(
                    &self.inner.rooms,
                    key,
                    &credential.local_human_member_id,
                    expected_prior_mirror,
                    sequence,
                );
                match result {
                    Ok(cas) => Ok(cas),
                    Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
                        Err(GuardedMutationError::NotFound)
                    }
                    Err(_) => Err(GuardedMutationError::Store),
                }
            })
            .await
            .ok_or(IntentError::Forbidden)?;
        let cas = match cas {
            Ok(cas) => cas,
            Err(GuardedMutationError::Generation) => return Err(IntentError::Forbidden),
            Err(GuardedMutationError::NotFound) => return Err(IntentError::NotFound),
            Err(GuardedMutationError::Store) => return Err(IntentError::Store),
        };
        // A `Stale` result means a fresher write already landed concurrently
        // (another GET/PATCH, or a `room_read_cursor` push frame); that
        // write already published its own wake, so only re-publish when
        // THIS call's write was the one actually applied.
        let applied = cas.was_applied();
        let projection = cas.into_projection();
        if applied {
            publish_room_read_cursor_wake_on(&self.inner.read_cursor_wakes, key);
        }
        Ok(projection)
    }

    pub(super) async fn room_patch_read_cursor(
        &self,
        key: &RoomKey,
        read_seq: u64,
    ) -> Result<RoomReadCursorProjection, IntentError> {
        let slot = self.slot_for(key).await;
        let (credential, access) = with_rooms_handle(&self.inner.rooms, |store| {
            if store.get(key)?.is_none() {
                return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()));
            }
            Ok::<_, ocean_store::RoomStoreError>((
                store.room_credential(key)?,
                store.room_access(key)?,
            ))
        })
        .map_err(|error| match error {
            ocean_store::RoomStoreError::UnknownRoom(_) => IntentError::NotFound,
            _ => IntentError::Store,
        })?;
        let credential = credential.ok_or(IntentError::Conflict)?;
        if access.state != RoomAccessState::Live {
            return Err(IntentError::Conflict);
        }
        let client = self.inner.client.clone().ok_or(IntentError::Unavailable)?;
        let url = client
            .endpoint(&["api", "v1", "rooms", key.as_str(), "read-cursor"])
            .map_err(|_| IntentError::Unavailable)?;
        let generation = slot.generation.load(Ordering::Acquire);
        // Snapshot the CAS `expected_prior_mirror` (M5) at the same point as
        // `generation`, before the request goes out — see the matching
        // comment in `room_get_read_cursor`.
        let expected_prior_mirror = with_rooms_handle(&self.inner.rooms, |store| {
            store.room_read_cursor(key, &credential.local_human_member_id)
        })
        .map_err(|_| IntentError::Store)?
        .mirrored_upstream_read_seq;
        let response = match slot
            .control
            .send(
                client
                    .http
                    .patch(url)
                    .timeout(REQUEST_TIMEOUT)
                    .bearer_auth(&credential.bearer_token)
                    .json(&serde_json::json!({ "sequence": read_seq.to_string() })),
                &self.inner.shutdown,
            )
            .await
        {
            AdmittedSend::Response(Ok(response)) => response,
            AdmittedSend::Response(Err(_)) | AdmittedSend::Cancelled => {
                return Err(IntentError::Unavailable)
            }
            AdmittedSend::Closed => return Err(IntentError::Forbidden),
        };
        match response.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                self.revoke_control(key).await;
                return Err(IntentError::Forbidden);
            }
            StatusCode::NOT_FOUND
            | StatusCode::METHOD_NOT_ALLOWED
            | StatusCode::NOT_IMPLEMENTED => return Err(IntentError::Conflict),
            StatusCode::CONFLICT | StatusCode::BAD_REQUEST => return Err(IntentError::Protocol),
            s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
                return Err(IntentError::Unavailable)
            }
            _ => return Err(IntentError::Protocol),
        }
        let body: ReadCursorBody = read_bounded_json(response, BODY_LIMIT)
            .await
            .map_err(control_body_error)?;
        if body.room_id != key.as_str() {
            return Err(IntentError::Protocol);
        }
        let sequence = match body.sequence {
            Some(sequence) => {
                Some(parse_canonical_u64(&sequence).map_err(|_| IntentError::Protocol)?)
            }
            None => None,
        };
        // The upstream read-cursor store is authoritative for `read_seq` and
        // may clamp our request down to its own known high-water mark; it
        // signals that explicitly via `clamped: true` (H3). Trust that
        // signal instead of rejecting a truthfully clamped response as a
        // protocol violation — but only within the bound the signal claims
        // to describe: a genuine clamp can only ever return a sequence that
        // is `Some` and no greater than what we requested (F4). A response
        // that flags `clamped: true` yet reports no sequence, or a
        // sequence ABOVE what we asked for, is not a truthful clamp-down —
        // it is still rejected as a protocol violation, exactly like an
        // unflagged mismatch.
        let truthfully_clamped =
            body.clamped == Some(true) && matches!(sequence, Some(clamped) if clamped <= read_seq);
        if sequence != Some(read_seq) && !truthfully_clamped {
            return Err(IntentError::Protocol);
        }
        let cas = slot
            .control
            .mutate(|| {
                if slot.generation.load(Ordering::Acquire) != generation {
                    return Err(GuardedMutationError::Generation);
                }
                let result = converge_room_read_cursor_mirror(
                    &self.inner.rooms,
                    key,
                    &credential.local_human_member_id,
                    expected_prior_mirror,
                    sequence,
                );
                match result {
                    Ok(cas) => Ok(cas),
                    Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
                        Err(GuardedMutationError::NotFound)
                    }
                    Err(_) => Err(GuardedMutationError::Store),
                }
            })
            .await
            .ok_or(IntentError::Forbidden)?;
        let cas = match cas {
            Ok(cas) => cas,
            Err(GuardedMutationError::Generation) => return Err(IntentError::Forbidden),
            Err(GuardedMutationError::NotFound) => return Err(IntentError::NotFound),
            Err(GuardedMutationError::Store) => return Err(IntentError::Store),
        };
        // Same stale-write handling as `room_get_read_cursor`: only
        // re-publish the wake for a write this call actually applied.
        let applied = cas.was_applied();
        let projection = cas.into_projection();
        if applied {
            publish_room_read_cursor_wake_on(&self.inner.read_cursor_wakes, key);
        }
        Ok(projection)
    }

    pub(super) async fn register_agents(
        &self,
        key: &RoomKey,
        agents: Vec<AgentRegistrationInput>,
    ) -> Result<RoomAccessProjection, IntentError> {
        let slot = self.slot_for(key).await;
        let (credential, access) = with_rooms_handle(&self.inner.rooms, |store| {
            if store.get(key)?.is_none() {
                return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()));
            }
            Ok::<_, ocean_store::RoomStoreError>((
                store.room_credential(key)?,
                store.room_access(key)?,
            ))
        })
        .map_err(|error| match error {
            ocean_store::RoomStoreError::UnknownRoom(_) => IntentError::NotFound,
            _ => IntentError::Store,
        })?;
        let credential = credential.ok_or(IntentError::Conflict)?;
        if access.state == RoomAccessState::Revoked {
            return Err(IntentError::Forbidden);
        }
        let client = self.inner.client.clone().ok_or(IntentError::Unavailable)?;
        let request = AgentRegistrationBatch {
            agents: agents
                .iter()
                .map(|agent| AgentRegistrationWire {
                    registration_key: &agent.registration_key,
                    display_name: &agent.descriptor.display_name,
                    description: agent.descriptor.description.as_deref(),
                    model_alias: agent.descriptor.model_alias.as_deref(),
                    skills_count: agent.descriptor.skills_count,
                    subagent_names: &agent.descriptor.subagent_names,
                })
                .collect(),
        };
        let url = client
            .endpoint(&["api", "v1", "rooms", key.as_str(), "members", "agents"])
            .map_err(|_| IntentError::Unavailable)?;
        let generation = slot.generation.load(Ordering::Acquire);
        let response = match slot
            .control
            .send(
                client
                    .http
                    .post(url)
                    .timeout(REQUEST_TIMEOUT)
                    .bearer_auth(&credential.bearer_token)
                    .json(&request),
                &self.inner.shutdown,
            )
            .await
        {
            AdmittedSend::Response(Ok(response)) => response,
            AdmittedSend::Response(Err(_)) | AdmittedSend::Cancelled => {
                return Err(IntentError::Unavailable)
            }
            AdmittedSend::Closed => return Err(IntentError::Forbidden),
        };
        match response.status() {
            StatusCode::OK | StatusCode::CREATED => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                self.revoke_control(key).await;
                return Err(IntentError::Forbidden);
            }
            StatusCode::CONFLICT => return Err(IntentError::Conflict),
            s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
                return Err(IntentError::Unavailable)
            }
            _ => return Err(IntentError::Protocol),
        }
        let envelope: AgentMembersEnvelope = read_bounded_json(response, BODY_LIMIT)
            .await
            .map_err(control_body_error)?;
        if envelope.members.len() != agents.len() {
            return Err(IntentError::Protocol);
        }
        let mut ids = HashSet::new();
        for (member, agent) in envelope.members.iter().zip(&agents) {
            if !valid_agent_member(member, &credential.local_human_member_id, &agent.agent_name)
                || !ids.insert(member.member_id.clone())
            {
                return Err(IntentError::Protocol);
            }
        }
        let binding_result = slot
            .control
            .mutate(|| {
                if slot.generation.load(Ordering::Acquire) != generation {
                    return Err(GuardedMutationError::Generation);
                }
                let result = with_rooms_handle(&self.inner.rooms, |store| {
                    if store.get(key)?.is_none() {
                        return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()));
                    }
                    for (member, agent) in envelope.members.iter().zip(&agents) {
                        store.bind_room_agent(
                            key,
                            &member.member_id,
                            &agent.agent_name,
                            &agent.registration_key,
                        )?;
                    }
                    Ok::<_, ocean_store::RoomStoreError>(())
                });
                match result {
                    Ok(()) => Ok(()),
                    Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
                        Err(GuardedMutationError::NotFound)
                    }
                    Err(_) => Err(GuardedMutationError::Store),
                }
            })
            .await
            .ok_or(IntentError::Forbidden)?;
        match binding_result {
            Ok(()) => {}
            Err(GuardedMutationError::Generation) => return Err(IntentError::Forbidden),
            Err(GuardedMutationError::NotFound) => return Err(IntentError::NotFound),
            Err(GuardedMutationError::Store) => return Err(IntentError::Store),
        }
        let members = self
            .fetch_roster_control(&slot, &client, &credential, generation)
            .await?;
        let projection = slot
            .control
            .mutate(|| {
                if slot.generation.load(Ordering::Acquire) != generation {
                    return Err(GuardedMutationError::Generation);
                }
                let result = with_rooms_handle(&self.inner.rooms, |store| {
                    if store.get(key)?.is_none() {
                        return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()));
                    }
                    store.update_room_access_safe(key, None, Some(&members), None)
                });
                match result {
                    Ok(projection) => Ok(projection),
                    Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
                        Err(GuardedMutationError::NotFound)
                    }
                    Err(_) => Err(GuardedMutationError::Store),
                }
            })
            .await
            .ok_or(IntentError::Forbidden)?
            .map_err(|error| match error {
                GuardedMutationError::Generation => IntentError::Forbidden,
                GuardedMutationError::NotFound => IntentError::NotFound,
                GuardedMutationError::Store => IntentError::Store,
            })?;
        publish_room_access_wake_on(&self.inner.access_wakes, key);
        Ok(projection)
    }

    async fn fetch_roster_control(
        &self,
        slot: &Arc<RoomSlot>,
        client: &FederationClient,
        credential: &RoomCredential,
        generation: u64,
    ) -> Result<Vec<FederatedRoomMemberProjection>, IntentError> {
        let url = client
            .room_endpoint(&credential.room_id, "members")
            .map_err(|_| IntentError::Unavailable)?;
        let response = match slot
            .control
            .send(
                client
                    .http
                    .get(url)
                    .timeout(REQUEST_TIMEOUT)
                    .bearer_auth(&credential.bearer_token),
                &self.inner.shutdown,
            )
            .await
        {
            AdmittedSend::Response(Ok(response)) => response,
            AdmittedSend::Response(Err(_)) | AdmittedSend::Cancelled => {
                return Err(IntentError::Unavailable)
            }
            AdmittedSend::Closed => return Err(IntentError::Forbidden),
        };
        match response.status() {
            StatusCode::OK => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                self.revoke_control(&credential.room_id).await;
                return Err(IntentError::Forbidden);
            }
            s if s == StatusCode::TOO_MANY_REQUESTS || s.is_server_error() => {
                return Err(IntentError::Unavailable)
            }
            _ => return Err(IntentError::Protocol),
        }
        let envelope: MembersEnvelope = read_bounded_json(response, BODY_LIMIT)
            .await
            .map_err(control_body_error)?;
        if slot
            .control
            .mutate(|| slot.generation.load(Ordering::Acquire) == generation)
            .await
            != Some(true)
        {
            return Err(IntentError::Forbidden);
        }
        let members = project_roster(&self.inner, credential, envelope, &HashSet::new()).map_err(
            |error| {
                if error == BridgeError::Store {
                    IntentError::Store
                } else {
                    IntentError::Protocol
                }
            },
        )?;
        if slot
            .control
            .mutate(|| slot.generation.load(Ordering::Acquire) == generation)
            .await
            != Some(true)
        {
            return Err(IntentError::Forbidden);
        }
        Ok(members)
    }

    /// Remove one member from a federated room's bedrock roster, on explicit
    /// request — the single-member counterpart of
    /// `sweep_agent_from_federated_rosters`, and it shares that sweep's policy
    /// stance rather than `register_agents`': bedrock's 401/403 here is the
    /// owner-or-self removal policy answering "not yours to remove", NOT a
    /// credential event, so it surfaces as `Forbidden` WITHOUT revoking
    /// control — severing a healthy room's federation over a refused removal
    /// would trade a denied request for real data loss.
    pub(super) async fn remove_member(
        &self,
        key: &RoomKey,
        member_id: &str,
    ) -> Result<RoomAccessProjection, IntentError> {
        let slot = self.slot_for(key).await;
        let (credential, access) = with_rooms_handle(&self.inner.rooms, |store| {
            if store.get(key)?.is_none() {
                return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()));
            }
            Ok::<_, ocean_store::RoomStoreError>((
                store.room_credential(key)?,
                store.room_access(key)?,
            ))
        })
        .map_err(|error| match error {
            ocean_store::RoomStoreError::UnknownRoom(_) => IntentError::NotFound,
            _ => IntentError::Store,
        })?;
        let credential = credential.ok_or(IntentError::Conflict)?;
        if access.state == RoomAccessState::Revoked {
            return Err(IntentError::Forbidden);
        }
        let client = self.inner.client.clone().ok_or(IntentError::Unavailable)?;
        let url = client
            .endpoint(&["api", "v1", "rooms", key.as_str(), "members", member_id])
            .map_err(|_| IntentError::Unavailable)?;
        let generation = slot.generation.load(Ordering::Acquire);
        let response = match slot
            .control
            .send(
                client
                    .http
                    .delete(url)
                    .timeout(REQUEST_TIMEOUT)
                    .bearer_auth(&credential.bearer_token),
                &self.inner.shutdown,
            )
            .await
        {
            AdmittedSend::Response(Ok(response)) => response,
            AdmittedSend::Response(Err(_)) | AdmittedSend::Cancelled => {
                return Err(IntentError::Unavailable)
            }
            AdmittedSend::Closed => return Err(IntentError::Forbidden),
        };
        match response.status() {
            status if status.is_success() => {}
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(IntentError::Forbidden),
            StatusCode::NOT_FOUND => return Err(IntentError::NotFound),
            StatusCode::CONFLICT => return Err(IntentError::Conflict),
            status if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() => {
                return Err(IntentError::Unavailable)
            }
            _ => return Err(IntentError::Protocol),
        }
        // Forget the local agent binding only after bedrock confirmed, and
        // only under the epoch that sent the request — the sweep's rule. A
        // human or non-locally-bound member simply has nothing to unbind.
        slot.control
            .mutate(|| {
                if slot.generation.load(Ordering::Acquire) != generation {
                    return false;
                }
                with_rooms_handle(&self.inner.rooms, |store| {
                    store.unbind_room_agent(key, member_id)
                })
                .unwrap_or(false)
            })
            .await;
        // Refresh the projection now rather than at the next heartbeat, so
        // the caller's response already shows the member gone.
        let members = self
            .fetch_roster_control(&slot, &client, &credential, generation)
            .await?;
        let projection = slot
            .control
            .mutate(|| {
                if slot.generation.load(Ordering::Acquire) != generation {
                    return Err(GuardedMutationError::Generation);
                }
                let result = with_rooms_handle(&self.inner.rooms, |store| {
                    if store.get(key)?.is_none() {
                        return Err(ocean_store::RoomStoreError::UnknownRoom(key.clone()));
                    }
                    store.update_room_access_safe(key, None, Some(&members), None)
                });
                match result {
                    Ok(projection) => Ok(projection),
                    Err(ocean_store::RoomStoreError::UnknownRoom(_)) => {
                        Err(GuardedMutationError::NotFound)
                    }
                    Err(_) => Err(GuardedMutationError::Store),
                }
            })
            .await
            .ok_or(IntentError::Forbidden)?
            .map_err(|error| match error {
                GuardedMutationError::Generation => IntentError::Forbidden,
                GuardedMutationError::NotFound => IntentError::NotFound,
                GuardedMutationError::Store => IntentError::Store,
            })?;
        publish_room_access_wake_on(&self.inner.access_wakes, key);
        Ok(projection)
    }

    /// Push an agent-folder delete into every federated room the agent was
    /// registered in — the half of the delete that
    /// `sweep_agent_from_local_rosters` cannot do: bedrock owns a federated
    /// room's membership, so a locally swept row is rewritten by the next
    /// roster sync unless bedrock itself removes the member.
    ///
    /// Best-effort is load-bearing. The folder is already gone when this
    /// runs, so a room the sweep cannot reach keeps its ghost until a retry
    /// or a manual remove — but no per-room failure may surface as an error.
    /// In particular a 403 here is bedrock's owner-or-self removal policy
    /// answering "not yours to remove", NOT a credential event: the
    /// registration path's revoke-on-403 must not be copied, because
    /// severing a healthy room's federation over a cleanup it merely wasn't
    /// allowed to do would trade a cosmetic ghost for real data loss.
    ///
    /// Returns how many rooms bedrock confirmed removed, for the caller's
    /// completion log.
    pub(super) async fn sweep_agent_from_federated_rosters(&self, agent_name: &str) -> usize {
        let Some(client) = self.inner.client.clone() else {
            return 0;
        };
        // Collect (credential, member id) targets in one synchronous lock
        // hold — the store guard must never cross an await. A credential row
        // outlives revoke (`revoke_room` persists Revoked and keeps the
        // row), and the in-memory admission gate that enforces a revoke is
        // rebuilt open after a restart — so the durable access state is the
        // only filter that survives the process, the same fail-closed check
        // `register_agents` makes. The per-room binding resolve then narrows
        // to rooms that actually registered this agent.
        let targets = with_rooms_handle(&self.inner.rooms, |store| {
            let mut targets = Vec::new();
            for credential in store.list_credentialed_rooms()? {
                if store.room_access(&credential.room_id)?.state == RoomAccessState::Revoked {
                    continue;
                }
                if let Some(member_id) =
                    store.resolve_room_agent_member(&credential.room_id, agent_name)?
                {
                    targets.push((credential, member_id));
                }
            }
            Ok::<_, ocean_store::RoomStoreError>(targets)
        });
        let targets = match targets {
            Ok(targets) => targets,
            Err(error) => {
                tracing::warn!(agent = agent_name, error = %error,
                    "agent-delete federated sweep could not enumerate rooms");
                return 0;
            }
        };
        let mut removed = 0;
        for (credential, member_id) in targets {
            let key = credential.room_id.clone();
            let Ok(url) =
                client.endpoint(&["api", "v1", "rooms", key.as_str(), "members", &member_id])
            else {
                continue;
            };
            let slot = self.slot_for(&key).await;
            let generation = slot.generation.load(Ordering::Acquire);
            let response = match slot
                .control
                .send(
                    client
                        .http
                        .delete(url)
                        .timeout(REQUEST_TIMEOUT)
                        .bearer_auth(&credential.bearer_token),
                    &self.inner.shutdown,
                )
                .await
            {
                AdmittedSend::Response(Ok(response)) => response,
                AdmittedSend::Response(Err(error)) => {
                    tracing::warn!(agent = agent_name, room = %key, error = %error,
                        "agent-delete federated sweep skipped an unreachable room");
                    continue;
                }
                // Shutdown, or a revoke already closed this room's gate:
                // either way the room is no longer ours to clean.
                AdmittedSend::Cancelled | AdmittedSend::Closed => continue,
            };
            let status = response.status();
            if !status.is_success() {
                tracing::warn!(agent = agent_name, room = %key, status = %status,
                    "agent-delete federated sweep skipped a room that refused the removal");
                continue;
            }
            // Forget the binding only after bedrock confirmed, and only if
            // the epoch that sent the request is still current — a revoke
            // between send and confirm means this member id is no longer
            // ours to forget. No local projection surgery: the next
            // heartbeat roster refresh rewrites members from bedrock's
            // answer, which no longer contains the agent.
            let unbound = slot
                .control
                .mutate(|| {
                    if slot.generation.load(Ordering::Acquire) != generation {
                        return false;
                    }
                    with_rooms_handle(&self.inner.rooms, |store| {
                        store.unbind_room_agent(&key, &member_id)
                    })
                    .unwrap_or(false)
                })
                .await
                .unwrap_or(false);
            if unbound {
                removed += 1;
            }
        }
        removed
    }

    async fn revoke_control(&self, key: &RoomKey) {
        let slot = self.slot_for(key).await;
        slot.control.close().await;
        self.stop_room(key).await;
        revoke_room(&self.inner, key).await;
    }

    fn persist_lease_lost(&self, key: &RoomKey, state: RoomAccessState) -> Result<(), BridgeError> {
        persist_lease_lost(&self.inner, key, state)
    }
}

fn control_body_error(error: BridgeError) -> IntentError {
    match error {
        BridgeError::Transport => IntentError::Unavailable,
        _ => IntentError::Protocol,
    }
}

fn canonical_room_key(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 128
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
}

fn valid_human_member(member: &ControlEnvelopeMember, role: FederatedRoomRole) -> bool {
    member.actor_type == FederatedActorType::User
        && member.role_in_room == role
        && !member.member_id.is_empty()
        && !member.display_name.is_empty()
        && member.owner_member_id.is_none()
        && member.public_agent_descriptor.is_none()
}

fn valid_agent_member(member: &ControlEnvelopeMember, owner: &str, name: &str) -> bool {
    member.actor_type == FederatedActorType::Agent
        && member.role_in_room == FederatedRoomRole::Member
        && !member.member_id.is_empty()
        && member.display_name == name
        && member.owner_member_id.as_deref() == Some(owner)
        && member
            .public_agent_descriptor
            .as_ref()
            .is_some_and(|descriptor| descriptor.display_name == name)
}

async fn run_room(
    inner: Arc<SupervisorInner>,
    key: RoomKey,
    generation: u64,
    sender_notify: Arc<Notify>,
    cancel: CancellationToken,
) {
    let mut attempt = 0u32;
    loop {
        if cancel.is_cancelled() || inner.shutdown.is_cancelled() {
            return;
        }
        let Some(client) = inner.client.clone() else {
            let _ = persist_lease_lost(&inner, &key, RoomAccessState::Recovering);
            return;
        };
        let credential = with_rooms_handle(&inner.rooms, |store| {
            let credential = store.room_credential(&key)?;
            let state = store.room_access(&key)?.state;
            Ok::<_, ocean_store::RoomStoreError>((credential, state))
        });
        let Ok((Some(credential), state)) = credential else {
            return;
        };
        if state == RoomAccessState::Revoked {
            return;
        }
        let outcome = run_epoch(
            inner.clone(),
            client,
            credential,
            generation,
            sender_notify.clone(),
            cancel.child_token(),
        )
        .await;
        match outcome {
            EpochOutcome::Stopped => return,
            EpochOutcome::Revoked => {
                // Wire revocation closes the P2-C producer/control gate before
                // Pending cleanup, matching route-observed denial ordering.
                let slot = inner.slots.lock().await.get(&key).cloned();
                if let Some(slot) = slot {
                    slot.control.close().await;
                }
                revoke_room(&inner, &key).await;
                return;
            }
            EpochOutcome::Recover => {
                if persist_lease_lost(&inner, &key, RoomAccessState::Recovering).is_err() {
                    return;
                }
            }
        }
        attempt = attempt.saturating_add(1);
        let delay = reconnect_delay(attempt, generation);
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = inner.shutdown.cancelled() => return,
            _ = sleep(delay) => {}
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EpochOutcome {
    Recover,
    Revoked,
    Stopped,
}

async fn run_epoch(
    inner: Arc<SupervisorInner>,
    client: FederationClient,
    credential: RoomCredential,
    generation: u64,
    sender_notify: Arc<Notify>,
    cancel: CancellationToken,
) -> EpochOutcome {
    let mut live_human_member_ids = HashSet::<String>::new();
    let key = credential.room_id.clone();
    let cursor = match cursor_or_zero(durable_cursor(&inner.rooms, &key)) {
        Ok(cursor) => cursor,
        Err(_) => return EpochOutcome::Recover,
    };
    let url = match client.room_endpoint(&key, "events") {
        Ok(url) => url,
        Err(_) => return EpochOutcome::Recover,
    };
    let response = tokio::select! {
        _ = cancel.cancelled() => return EpochOutcome::Stopped,
        response = client.http.get(url)
            .bearer_auth(&credential.bearer_token)
            .header("last-event-id", cursor.to_string())
            .send() => response,
    };
    let response = match response {
        Ok(response) => response,
        Err(_) => return EpochOutcome::Recover,
    };
    match response.status() {
        StatusCode::OK => {}
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return EpochOutcome::Revoked,
        _ => return EpochOutcome::Recover,
    }

    let mut raw_bound = RawSseEventBound::new(SSE_EVENT_LIMIT);
    let bounded_bytes = response.bytes_stream().map(move |chunk| match chunk {
        Ok(bytes) => raw_bound.accept(&bytes).map(|()| bytes),
        Err(_) => Err(RawSseError::Transport),
    });
    let mut stream = bounded_bytes.eventsource();
    let hello = tokio::select! {
        _ = cancel.cancelled() => return EpochOutcome::Stopped,
        event = stream.next() => event,
    };
    let Some(Ok(hello)) = hello else {
        return EpochOutcome::Recover;
    };
    if hello.event != "hello" {
        return EpochOutcome::Recover;
    }
    let Ok(hello) = parse_sse_json::<HelloFrame>(&hello.data) else {
        return EpochOutcome::Recover;
    };
    if hello.room_id != key.as_str() {
        return EpochOutcome::Recover;
    }
    let Ok(high_water) = parse_canonical_u64(&hello.snapshot_high_water) else {
        return EpochOutcome::Recover;
    };

    // Roster is committed before the first room_event of every connection epoch.
    let members = match fetch_roster(&inner, &client, &credential, &live_human_member_ids).await {
        Ok(members) => members,
        Err(EpochOutcome::Revoked) => return EpochOutcome::Revoked,
        Err(outcome) => return outcome,
    };
    let state = access_state_for_hello(cursor, high_water);
    if !commit_access(&inner, &key, state, Some(&members), None) {
        return EpochOutcome::Recover;
    }

    let sender_cancel = cancel.child_token();
    // The epoch admission gate defines the local revoke boundary. Sender
    // checks it before request admission and before every post-response local
    // mutation; receiver closes it before cancelling/joining the child.
    let admission_open = Arc::new(AdmissionGate::new());
    let sender_credential = RoomCredential {
        room_id: credential.room_id.clone(),
        bearer_token: credential.bearer_token.clone(),
        local_human_member_id: credential.local_human_member_id.clone(),
    };
    let (fatal_tx, mut fatal_rx) = mpsc::channel(1);
    let sender = tokio::spawn(sender_loop(SenderContext {
        inner: inner.clone(),
        client: client.clone(),
        credential: sender_credential,
        notify: sender_notify,
        cancel: sender_cancel.clone(),
        admission_open: admission_open.clone(),
        generation,
        fatal: fatal_tx,
    }));

    let mut last_accepted = cursor;
    let outcome = loop {
        tokio::select! {
            _ = cancel.cancelled() => break EpochOutcome::Stopped,
            fatal = fatal_rx.recv() => {
                break match fatal {
                    Some(SenderFatal::Revoke) => EpochOutcome::Revoked,
                    Some(SenderFatal::Recover) => EpochOutcome::Recover,
                    None => EpochOutcome::Recover,
                };
            }
            next = stream.next() => {
                let Some(Ok(event)) = next else { break EpochOutcome::Recover };
                match event.event.as_str() {
                    "room_event" => {
                        // A row this daemon cannot represent must not become a
                        // permanent wall.
                        //
                        // `Recover` means "reconnect from the durable cursor",
                        // and the cursor still sits BEFORE this row — so a row
                        // that fails every time is served again on every
                        // reconnect, forever. Nothing downstream of it ever
                        // arrives, no retry or restart clears it, and because
                        // the row lives on the shared ledger it wedges every
                        // daemon federated to that room at once.
                        //
                        // So an unreadable or invalid row is stepped over
                        // instead. The SSE `id` carries the sequence even when
                        // the payload does not parse, which is the whole reason
                        // advancing is possible. The row stays durable on the
                        // server, which is the ledger's authority; only this
                        // local projection is missing it. Losing one message
                        // beats losing every message after it.
                        let (sequence, result) = match parse_sse_json::<WireLedgerRow>(&event.data) {
                            Err(_) => {
                                let Ok(sequence) = parse_canonical_u64(&event.id) else {
                                    // No usable sequence means nothing to
                                    // advance past. Reconnect and hope.
                                    break EpochOutcome::Recover;
                                };
                                if sequence < last_accepted {
                                    break EpochOutcome::Recover;
                                }
                                tracing::warn!(
                                    room = %key,
                                    sequence,
                                    bytes = event.data.len(),
                                    outcome = "unreadable_row_skipped",
                                    "federation stepped past a ledger row it cannot represent"
                                );
                                (sequence, advance_non_message(&inner, &key, sequence))
                            }
                            Ok(row) => {
                                let Ok(sequence) = parse_canonical_u64(&row.sequence) else {
                                    break EpochOutcome::Recover;
                                };
                                // Scope confusion is a different failure: a row
                                // for another room on this stream says the
                                // connection itself is wrong, not that this row
                                // is bad. Reconnect rather than advance.
                                if !wire_row_scope_is_exact(&row, &key)
                                    || event.id != row.sequence
                                    || sequence < last_accepted
                                {
                                    break EpochOutcome::Recover;
                                }
                                let result = if row.event_type == "message" {
                                    ingest_message_row(
                                        &inner,
                                        &client,
                                        &credential,
                                        row,
                                        &live_human_member_ids,
                                    )
                                    .await
                                } else if workspace_action_is_marker(&row.event_type) {
                                    ingest_workspace_row(&inner, &key, row)
                                } else {
                                    advance_non_message(&inner, &key, sequence)
                                };
                                match result {
                                    // A row that parses but fails validation is
                                    // poison in exactly the same way, so it is
                                    // stepped over too. Store and transport
                                    // faults are NOT: those are local or
                                    // transient, and reconnecting is the
                                    // correct answer to both.
                                    Err(BridgeError::Protocol) => {
                                        tracing::warn!(
                                            room = %key,
                                            sequence,
                                            outcome = "invalid_row_skipped",
                                            "federation stepped past a ledger row that failed validation"
                                        );
                                        (sequence, advance_non_message(&inner, &key, sequence))
                                    }
                                    other => (sequence, other),
                                }
                            }
                        };
                        match result {
                            Ok(IngestDisposition::Committed) => {
                                last_accepted = sequence;
                                if last_accepted >= high_water
                                    && ensure_live_with(
                                        durable_state(&inner.rooms, &key),
                                        || commit_state_only(
                                            &inner,
                                            &key,
                                            RoomAccessState::Live,
                                        ),
                                    )
                                    .is_err()
                                {
                                    break EpochOutcome::Recover;
                                }
                            }
                            Ok(IngestDisposition::Duplicate) => {
                                last_accepted = sequence;
                            }
                            Err(BridgeError::Revoked) => break EpochOutcome::Revoked,
                            Err(_) => break EpochOutcome::Recover,
                        }
                    }
                    "heartbeat" => {
                        let Ok(frame) = parse_sse_json::<HeartbeatFrame>(&event.data) else {
                            break EpochOutcome::Recover;
                        };
                        let Ok(sequence) = parse_canonical_u64(&frame.sequence) else {
                            break EpochOutcome::Recover;
                        };
                        if sequence != last_accepted {
                            break EpochOutcome::Recover;
                        }
                        match fetch_roster(&inner, &client, &credential, &live_human_member_ids).await {
                            Ok(members) => {
                                let Ok(current_state) = durable_state(&inner.rooms, &key) else {
                                    break EpochOutcome::Recover;
                                };
                                if !commit_presence_snapshot(&inner, &key, current_state, &members) {
                                    break EpochOutcome::Recover;
                                }
                            }
                            Err(outcome) => break outcome,
                        }
                    }
                    "room_presence" => {
                        let Ok(frame) = parse_sse_json::<PresenceFrame>(&event.data) else {
                            break EpochOutcome::Recover;
                        };
                        if frame.room_id != key.as_str() {
                            break EpochOutcome::Recover;
                        }
                        let Ok(current_state) = durable_state(&inner.rooms, &key) else {
                            break EpochOutcome::Recover;
                        };
                        if !apply_presence_frame(
                            &inner,
                            &key,
                            current_state,
                            &frame.members,
                            &mut live_human_member_ids,
                        ) {
                            break EpochOutcome::Recover;
                        }
                    }
                    "room_read_cursor" => {
                        let Ok(frame) = parse_sse_json::<ReadCursorFrame>(&event.data) else {
                            break EpochOutcome::Recover;
                        };
                        if frame.room_id != key.as_str() {
                            break EpochOutcome::Recover;
                        }
                        match apply_mirrored_read_cursor_frame(&inner, &credential, &key, frame.sequence) {
                            Ok(true) => publish_room_read_cursor_wake_on(&inner.read_cursor_wakes, &key),
                            Ok(false) => {}
                            Err(BridgeError::Revoked) => break EpochOutcome::Revoked,
                            Err(_) => break EpochOutcome::Recover,
                        }
                    }
                    "resync_required" => {
                        let Ok(frame) = parse_sse_json::<ResyncFrame>(&event.data) else {
                            break EpochOutcome::Recover;
                        };
                        let Ok(frame_cursor) = parse_canonical_u64(&frame.after_sequence) else {
                            break EpochOutcome::Recover;
                        };
                        if durable_cursor(&inner.rooms, &key).ok().flatten() != Some(frame_cursor) {
                            tracing::warn!(room = %key, outcome = "resync_cursor_mismatch", "federation resync uses durable cursor");
                        }
                        break EpochOutcome::Recover;
                    }
                    "revoked" => {
                        let Ok(frame) = parse_sse_json::<RevokedFrame>(&event.data) else {
                            break EpochOutcome::Recover;
                        };
                        if frame.reason != "membership_revoked" && frame.reason != "token_invalid" {
                            break EpochOutcome::Recover;
                        }
                        break EpochOutcome::Revoked;
                    }
                    _ => break EpochOutcome::Recover,
                }
            }
        }
    };

    // Close local admission before any revoke cleanup or epoch teardown.
    admission_open.close().await;
    sender_cancel.cancel();
    let _ = sender.await;
    outcome
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HelloFrame {
    room_id: String,
    snapshot_high_water: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HeartbeatFrame {
    sequence: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResyncFrame {
    after_sequence: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresenceFrame {
    room_id: String,
    members: Vec<PresenceWireMember>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresenceWireMember {
    member_id: String,
    actor_type: FederatedActorType,
    #[allow(dead_code)]
    role_in_room: FederatedRoomRole,
    #[allow(dead_code)]
    display_name: String,
    #[allow(dead_code)]
    joined_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadCursorFrame {
    room_id: String,
    sequence: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokedFrame {
    reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadCursorBody {
    room_id: String,
    sequence: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    changed: Option<bool>,
    /// The upstream read-cursor store clamped our requested `read_seq` down
    /// to its own authoritative high-water mark. `room_patch_read_cursor`
    /// (H3) trusts this signal instead of demanding the response echo the
    /// exact requested value.
    #[serde(default)]
    clamped: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct WireLedgerRow {
    id: String,
    sequence: String,
    event_type: String,
    correlation_id: String,
    virtual_path: String,
    #[serde(default)]
    actor_id: Option<String>,
    #[serde(default)]
    actor_member_id: Option<String>,
    #[serde(default)]
    source_id: Option<String>,
    #[serde(default)]
    source_sequence: Option<String>,
    #[serde(default)]
    payload: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MessagePayload {
    client_event_id: String,
    author_member_id: String,
    body: String,
    #[serde(default)]
    mention_member_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MembersEnvelope {
    members: Vec<WireMember>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireMember {
    member_id: String,
    #[serde(default)]
    owner_member_id: Option<String>,
    actor_type: FederatedActorType,
    role_in_room: FederatedRoomRole,
    display_name: String,
    #[serde(default)]
    public_agent_descriptor: Option<PublicAgentDescriptor>,
    joined_at: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableMessagePayload {
    body: String,
}

#[derive(Debug, Serialize)]
struct LedgerPost<'a> {
    event_type: &'static str,
    correlation_id: &'a str,
    virtual_path: String,
    actor_member_id: &'a str,
    source_id: &'a str,
    source_sequence: String,
    payload: LedgerPayload<'a>,
}

#[derive(Debug, Serialize)]
struct LedgerPayload<'a> {
    client_event_id: &'a str,
    author_member_id: &'a str,
    body: &'a str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mention_member_ids: &'a Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum SenderFatal {
    Recover,
    Revoke,
}

struct SenderContext {
    inner: Arc<SupervisorInner>,
    client: FederationClient,
    credential: RoomCredential,
    notify: Arc<Notify>,
    cancel: CancellationToken,
    admission_open: Arc<AdmissionGate>,
    generation: u64,
    fatal: mpsc::Sender<SenderFatal>,
}

async fn sender_loop(context: SenderContext) {
    let SenderContext {
        inner,
        client,
        credential,
        notify,
        cancel,
        admission_open,
        generation,
        fatal,
    } = context;
    let mut awaiting = HashSet::<String>::new();
    let mut scan_number = 0u64;
    let scan_sleep = sleep(sender_scan_delay(
        inner.scan_interval,
        generation,
        scan_number,
    ));
    tokio::pin!(scan_sleep);
    loop {
        let periodic = tokio::select! {
            _ = cancel.cancelled() => return,
            _ = notify.notified() => false,
            _ = &mut scan_sleep => true,
        };
        if periodic {
            scan_number = scan_number.saturating_add(1);
            scan_sleep.as_mut().reset(
                Instant::now() + sender_scan_delay(inner.scan_interval, generation, scan_number),
            );
        }
        if cancel.is_cancelled() {
            return;
        }
        let pending = with_rooms_handle(&inner.rooms, |store| {
            store.pending_outbox(&credential.room_id)
        });
        let Ok(pending) = pending else {
            let _ = fatal.send(SenderFatal::Recover).await;
            return;
        };
        let live_ids: HashSet<_> = pending
            .iter()
            .map(|item| item.client_event_id.clone())
            .collect();
        awaiting.retain(|id| live_ids.contains(id));
        for item in pending {
            if cancel.is_cancelled() {
                return;
            }
            if awaiting.contains(&item.client_event_id) {
                continue;
            }
            let body: DurableMessagePayload = match validate_outbox_item(&item) {
                Ok(body) => body,
                Err(()) => {
                    let _ = admission_open
                        .mutate(|| {
                            fail_row_and_wake(&inner, &credential.room_id, &item.client_event_id)
                        })
                        .await;
                    continue;
                }
            };
            let post = LedgerPost {
                event_type: "message",
                correlation_id: credential.room_id.as_str(),
                virtual_path: format!("/rooms/{}", credential.room_id.as_str()),
                actor_member_id: &item.author_member_id,
                source_id: &item.source_id,
                source_sequence: item.source_sequence.to_string(),
                payload: LedgerPayload {
                    client_event_id: &item.client_event_id,
                    author_member_id: &item.author_member_id,
                    body: &body.body,
                    mention_member_ids: &item.mention_member_ids,
                },
            };
            let url = match client.ledger_endpoint() {
                Ok(url) => url,
                Err(_) => {
                    let _ = fatal.send(SenderFatal::Recover).await;
                    return;
                }
            };
            let request = client
                .http
                .post(url)
                .timeout(REQUEST_TIMEOUT)
                .bearer_auth(&credential.bearer_token)
                .json(&post);
            let response = match admission_open.send(request, &cancel).await {
                AdmittedSend::Closed | AdmittedSend::Cancelled => return,
                AdmittedSend::Response(Ok(response)) => response,
                AdmittedSend::Response(Err(_)) => {
                    let _ = fatal.send(SenderFatal::Recover).await;
                    return;
                }
            };
            let status = response.status();
            let error = if status == StatusCode::CREATED {
                None
            } else {
                bounded_error_code(response).await
            };
            match classify_post_response(status, error.as_deref()) {
                PostAction::AwaitConfirmation => {
                    if admission_open
                        .mutate(|| awaiting.insert(item.client_event_id.clone()))
                        .await
                        .is_none()
                    {
                        return;
                    }
                }
                PostAction::FailRow => {
                    let _ = admission_open
                        .mutate(|| {
                            fail_row_and_wake(&inner, &credential.room_id, &item.client_event_id)
                        })
                        .await;
                }
                PostAction::Revoke => {
                    let _ = fatal.send(SenderFatal::Revoke).await;
                    return;
                }
                PostAction::Recover => {
                    let _ = fatal.send(SenderFatal::Recover).await;
                    return;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostAction {
    AwaitConfirmation,
    FailRow,
    Revoke,
    Recover,
}

fn classify_post_response(status: StatusCode, error: Option<&str>) -> PostAction {
    if status == StatusCode::CREATED {
        return PostAction::AwaitConfirmation;
    }
    if status == StatusCode::BAD_REQUEST || status == StatusCode::CONFLICT {
        return PostAction::FailRow;
    }
    if status == StatusCode::FORBIDDEN && error == Some("member_actor_mismatch") {
        return PostAction::FailRow;
    }
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        return PostAction::Revoke;
    }
    // Includes unexpected 2xx, 429, 5xx, and unknown statuses: preserve the
    // durable Pending row, mark the room Recovering, and retry by epoch.
    PostAction::Recover
}

fn validate_outbox_item(item: &RoomOutboxItem) -> Result<DurableMessagePayload, ()> {
    if item.event_type != "message" {
        return Err(());
    }
    serde_json::from_value(item.payload.clone()).map_err(|_| ())
}

async fn bounded_error_code(response: reqwest::Response) -> Option<String> {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: String,
    }
    read_bounded_json::<ErrorBody>(response, BODY_LIMIT)
        .await
        .ok()
        .map(|body| body.error)
}

/// Why a bounded body read stopped short. Overflow is its own variant because
/// the two readers below disagree about what it means: on a JSON lane a body
/// past the cap is a peer speaking something unrepresentable, while on the raw
/// file lane it is a legitimate file the caller refuses with a typed code.
enum BoundedReadError {
    Transport,
    OverCap,
}

async fn read_bounded_bytes(
    response: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, BoundedReadError> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BoundedReadError::Transport)?;
        if bytes.len().saturating_add(chunk.len()) > cap {
            return Err(BoundedReadError::OverCap);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

async fn read_bounded_json<T: DeserializeOwned>(
    response: reqwest::Response,
    cap: usize,
) -> Result<T, BridgeError> {
    let bytes = read_bounded_bytes(response, cap)
        .await
        .map_err(|error| match error {
            BoundedReadError::Transport => BridgeError::Transport,
            BoundedReadError::OverCap => BridgeError::Protocol,
        })?;
    serde_json::from_slice(&bytes).map_err(|_| BridgeError::Protocol)
}

async fn fetch_roster(
    inner: &Arc<SupervisorInner>,
    client: &FederationClient,
    credential: &RoomCredential,
    live_human_member_ids: &HashSet<String>,
) -> Result<Vec<FederatedRoomMemberProjection>, EpochOutcome> {
    let url = client
        .room_endpoint(&credential.room_id, "members")
        .map_err(|_| EpochOutcome::Recover)?;
    let response = client
        .http
        .get(url)
        .timeout(REQUEST_TIMEOUT)
        .bearer_auth(&credential.bearer_token)
        .send()
        .await
        .map_err(|_| EpochOutcome::Recover)?;
    match response.status() {
        StatusCode::OK => {}
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Err(EpochOutcome::Revoked),
        _ => return Err(EpochOutcome::Recover),
    }
    let envelope: MembersEnvelope = read_bounded_json(response, BODY_LIMIT)
        .await
        .map_err(|_| EpochOutcome::Recover)?;
    project_roster(inner, credential, envelope, live_human_member_ids)
        .map_err(|_| EpochOutcome::Recover)
}

fn project_roster(
    inner: &Arc<SupervisorInner>,
    credential: &RoomCredential,
    envelope: MembersEnvelope,
    live_human_member_ids: &HashSet<String>,
) -> Result<Vec<FederatedRoomMemberProjection>, BridgeError> {
    let mut members = Vec::with_capacity(envelope.members.len());
    let mut member_ids = HashSet::with_capacity(envelope.members.len());
    for member in envelope.members {
        if member.member_id.is_empty()
            || member.display_name.is_empty()
            || member.joined_at.is_empty()
            || !member_ids.insert(member.member_id.clone())
        {
            return Err(BridgeError::Protocol);
        }
        let binding = if member.actor_type == FederatedActorType::Agent {
            with_rooms_handle(&inner.rooms, |store| {
                store.resolve_room_agent(&credential.room_id, &member.member_id)
            })
            .map_err(|_| BridgeError::Store)?
            .is_some()
        } else {
            false
        };
        let derived_presence = match member.actor_type {
            FederatedActorType::User => {
                Some(if live_human_member_ids.contains(&member.member_id) {
                    MemberPresence::Live
                } else {
                    MemberPresence::Unavailable
                })
            }
            FederatedActorType::Agent => None,
        };
        members.push(FederatedRoomMemberProjection {
            member_id: member.member_id,
            owner_member_id: member.owner_member_id,
            actor_type: member.actor_type,
            role_in_room: member.role_in_room,
            display_name: member.display_name,
            public_agent_descriptor: member.public_agent_descriptor,
            joined_at: member.joined_at,
            derived_presence,
            local_binding_available: (member.actor_type == FederatedActorType::Agent)
                .then_some(binding),
        });
    }
    Ok(members)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngestDisposition {
    Committed,
    Duplicate,
}

async fn ingest_message_row(
    inner: &Arc<SupervisorInner>,
    client: &FederationClient,
    credential: &RoomCredential,
    row: WireLedgerRow,
    live_human_member_ids: &HashSet<String>,
) -> Result<IngestDisposition, BridgeError> {
    let sequence = parse_canonical_u64(&row.sequence)?;
    let source_id = row.source_id.ok_or(BridgeError::Protocol)?;
    let source_sequence = parse_canonical_u64(
        row.source_sequence
            .as_deref()
            .ok_or(BridgeError::Protocol)?,
    )?;
    let actor_member_id = row.actor_member_id.ok_or(BridgeError::Protocol)?;
    let origin_principal_id = row.actor_id.ok_or(BridgeError::Protocol)?;
    let payload: MessagePayload =
        serde_json::from_value(row.payload).map_err(|_| BridgeError::Protocol)?;
    let unique_mentions: HashSet<_> = payload.mention_member_ids.iter().collect();
    if row.id.is_empty()
        || source_id.is_empty()
        || actor_member_id.is_empty()
        || origin_principal_id.is_empty()
        || payload.client_event_id.is_empty()
        || payload.author_member_id != actor_member_id
        || unique_mentions.len() != payload.mention_member_ids.len()
    {
        return Err(BridgeError::Protocol);
    }
    let (author_kind, trigger_eligible, refreshed_roster) =
        match author_kind(inner, &credential.room_id, &payload.author_member_id)? {
            Some(kind) => (kind, kind == RoomParticipantKind::Human, None),
            None => {
                // One immediate current-epoch roster fetch, then conservative
                // Human. Do NOT commit/wake yet: Duplicate must remain a total
                // no-op, while Ingested coalesces roster + message into one
                // access wake. Reuse the epoch's live-human cache so this
                // out-of-band refresh derives the same presence a heartbeat
                // or presence frame would, instead of an empty set that
                // would mark every human member Unavailable.
                let current_state = durable_state(&inner.rooms, &credential.room_id)?;
                let members = fetch_roster(inner, client, credential, live_human_member_ids)
                    .await
                    .map_err(|outcome| match outcome {
                        EpochOutcome::Revoked => BridgeError::Revoked,
                        // Transport, NOT Protocol: the row is fine, the network
                        // was not. The receive loop steps past a Protocol
                        // failure, so mapping a transient fetch failure onto it
                        // would silently drop a legitimate message.
                        _ => BridgeError::Transport,
                    })?;
                let mapped = author_kind_from_members(&members, &payload.author_member_id);
                let kind = mapped.unwrap_or(RoomParticipantKind::Human);
                (
                    kind,
                    mapped == Some(RoomParticipantKind::Human),
                    Some((current_state, members)),
                )
            }
        };
    let trigger_targets = if trigger_eligible {
        let policy = with_rooms_handle(&inner.rooms, |store| {
            store.trigger_policy(&credential.room_id)
        })
        .map_err(|_| BridgeError::Store)?;
        payload
            .mention_member_ids
            .iter()
            .filter(|target| {
                evaluate_trigger_policy(
                    policy.as_ref(),
                    &RoomTriggerEvent::Mention {
                        participant_id: (*target).clone(),
                    },
                )
                .should_convene
            })
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let event = ConfirmedEvent {
        ledger_event_id: row.id,
        global_sequence: sequence,
        source_id,
        source_sequence,
        client_event_id: payload.client_event_id,
        origin_principal_id,
        origin_member_id: actor_member_id,
        author_id: payload.author_member_id,
        author_kind,
        kind: RoomMessageKind::Message,
        body: payload.body,
        trigger_targets,
    };
    let outcome = with_rooms_handle(&inner.rooms, |store| {
        store.ingest_confirmed_event(&credential.room_id, &event, chrono::Utc::now())
    })
    .map_err(|_| BridgeError::Store)?;
    match outcome {
        IngestOutcome::Duplicate => Ok(IngestDisposition::Duplicate),
        IngestOutcome::Ingested(commit) => {
            if let Some((state, members)) = refreshed_roster {
                // The message is already durable. Roster refresh is best-effort
                // here: one access wake below covers both committed changes; if
                // the roster write fails, a heartbeat retries it without hiding
                // or replaying the committed message.
                if with_rooms_handle(&inner.rooms, |store| {
                    store.update_room_access_safe(
                        &credential.room_id,
                        Some(state),
                        Some(&members),
                        None,
                    )
                })
                .is_err()
                {
                    tracing::warn!(
                        room = %credential.room_id,
                        outcome = "post_ingest_roster_refresh_failed",
                        "federation message committed; roster refresh deferred"
                    );
                }
            }
            publish_room_wake_on(&inner.room_wakes, &credential.room_id, &commit.message);
            publish_room_access_wake_on(&inner.access_wakes, &credential.room_id);
            for target_member_id in commit.claimed_trigger_targets {
                let reason = format!("on_mention: @{target_member_id} mentioned");
                if inner
                    .trigger_tx
                    .send(FederatedTriggerDispatch {
                        room: credential.room_id.clone(),
                        ledger_event_id: event.ledger_event_id.clone(),
                        local_seq: commit.message.seq,
                        target_member_id,
                        reason,
                    })
                    .is_err()
                {
                    tracing::warn!(room = %credential.room_id, outcome = "dispatch_receiver_closed", "federated trigger suppressed");
                }
            }
            Ok(IngestDisposition::Committed)
        }
    }
}

/// Synthetic `source_id` recorded on ingested workspace markers.
///
/// Real message rows carry a producer stream id shaped
/// `room:<room>:member:<member>:producer:<instance>`; this constant can never
/// collide with one, so the outbox-confirmation delete inside
/// `ingest_confirmed_event` (which matches the FULL producer tuple) is
/// guaranteed to touch nothing when a workspace marker commits.
const WORKSPACE_MARKER_SOURCE_ID: &str = "workspace";

/// Which `room.workspace.*` ledger rows become transcript markers.
///
/// The ledger deliberately records every workspace operation, but a transcript
/// is read by humans and fed to every convened agent, so only OUTCOMES belong
/// there. What stays out, and why:
/// - `file_written` / `file_deleted` are per-operation editor traffic, the
///   workspace equivalent of keystrokes — one row per single-file write or
///   delete route call, while the `flushed` row that follows carries the
///   outcome anyone in the room is waiting on. A container flush does NOT fan
///   out into these: its per-file rows are `flush_write`, which the paragraph
///   below shows never reaches this matcher at all.
/// - `exec_started` / `exec_finished` / `exec_failed` are arbitrary-command
///   chatter, most of it generated by agents in the room themselves — echoing
///   it back would feed each agent a transcript of its own typing.
///   `execs_purged` is the one exec row that DOES land: an owner
///   un-publishing output the room already saw is a destructive OUTCOME by
///   the same standard that promoted `repo_unbound`, and stored tails
///   vanishing with no transcript trace would leave humans and convened
///   agents trusting output that no longer exists.
/// - `*_started` (clone, build) would double every operation into a
///   started/finished pair; the finished row carries the outcome, and a
///   started row with no finished row is legible on the ledger, not here.
/// - `repo_bound` / `secrets_updated` are configuration bookkeeping, not
///   activity anyone waits on. `repo_unbound` used to sit with them — until
///   Bedrock's unbind started deleting the checkout, which made it a
///   destructive OUTCOME by this list's own standard. Its failure mode is
///   worse than silence: `rm_failed` leaves a live checkout the next flush
///   re-ingests as room files, and nobody learns that from the ledger.
///   `port_closed` left for the same reason: `port_exposed` told the room a
///   port was serving, nothing else ever says otherwise, and silence here
///   leaves humans and every convened agent reading a port back as live long
///   after it is gone. Where that precedent does NOT reach: `repo_unbound`
///   renders its own outcome from `checkout_removed`, while this marker still
///   reports the port's RECORDED state and never proof the route stopped
///   serving — Bedrock emits the row even when the driver's `unexposePort`
///   threw. The flag that would let the daemon narrow that claim now exists
///   upstream (`route_removed` / `route_removed_reason`, ocean-bedrock #65);
///   `WorkspaceEventPayload` ignores unknown fields, so reading and rendering
///   it is a daemon follow-on, no longer a Bedrock one.
///
/// `flush_write` and `file_write` read like omissions from this list and are
/// not. They are `appendAudit` action strings passed to
/// `writeDurableFileFromBuffer`, and the room stream is that same ledger
/// FILTERED to `correlation_id = <room>` plus the room-scoped path — two
/// fields only `emitWorkspaceEvent` stamps. Neither can reach this matcher,
/// so neither is an event this list forgot.
///
/// Everything not listed here — including future actions Bedrock grows —
/// advances the cursor exactly as before this allowlist existed. An unknown
/// action is dropped, and dropped SILENTLY, so a new upstream event reads as
/// nothing having happened.
/// `workspace_marker_allowlist_classifies_every_bedrock_event` turns that
/// silence into a failing test, but only once a human has re-derived the
/// pinned set against a newer Bedrock: nothing in ocean-os reads
/// ocean-bedrock, so no automated cross-repo check exists and the pin cannot
/// notice an addition by itself. Skipping that re-derivation is how `mkdir`
/// survived in this file as a phantom.
fn workspace_action_is_marker(event_type: &str) -> bool {
    matches!(
        event_type,
        "room.workspace.provisioned"
            | "room.workspace.destroyed"
            | "room.workspace.repo_cloned"
            | "room.workspace.repo_clone_failed"
            | "room.workspace.repo_unbound"
            | "room.workspace.build_finished"
            | "room.workspace.build_failed"
            | "room.workspace.port_exposed"
            | "room.workspace.port_closed"
            | "room.workspace.flushed"
            | "room.workspace.hydrated"
            | "room.workspace.ci_checked"
            | "room.workspace.execs_purged"
    )
}

/// The typed, bounded subset of a workspace audit payload the daemon is
/// willing to quote into a transcript marker. Everything else in the payload
/// is ignored; a payload whose fields exist but carry the wrong type is
/// malformed and stepped past like any other poison row.
#[derive(Debug, Default, Deserialize)]
struct WorkspaceEventPayload {
    #[serde(default)]
    driver: Option<String>,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    head_sha: Option<String>,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    exit_code: Option<i64>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    port: Option<u64>,
    #[serde(default)]
    hydrated_files: Option<u64>,
    #[serde(default)]
    changed_files: Option<u64>,
    #[serde(default)]
    flushed_files: Option<u64>,
    #[serde(default)]
    checkout_removed: Option<bool>,
    #[serde(default)]
    checkout_removed_reason: Option<String>,
    #[serde(default)]
    checks_new: Option<u64>,
    #[serde(default)]
    checks_total: Option<u64>,
    #[serde(default)]
    checks: Option<Vec<WorkspaceCiCheck>>,
    #[serde(default)]
    exec_id: Option<String>,
    #[serde(default)]
    purged_rows: Option<u64>,
}

/// One entry of a `ci_checked` payload's `checks` array. Every field is
/// lenient because Bedrock's scrubber emits null for descriptive fields it
/// cannot vouch for; a field that is PRESENT with the wrong type still
/// poisons the whole row, exactly like any other mistyped payload field.
#[derive(Debug, Default, Deserialize)]
struct WorkspaceCiCheck {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
}

/// Bound and neutralize a member-controlled string (branch, script name,
/// driver id) before quoting it into a transcript line.
///
/// Same threat as the attachment marker in `ocean-store`: transcripts are
/// rendered by clients and read by agents, and an upstream-controlled string
/// carrying a newline could forge an entire fake transcript row in a naive
/// renderer. Control characters are dropped rather than escaped, and the
/// result is hard-capped so a pathological branch name cannot balloon the
/// marker.
fn bounded_quotable(text: &str, max_chars: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect()
}

/// A short commit id for prose, accepted only when the upstream value
/// actually looks like one — anything else is omitted rather than quoted.
fn short_sha(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.len() < 7 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(trimmed.chars().take(12).collect())
}

fn fmt_duration_ms(ms: u64) -> String {
    format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
}

/// Compose the transcript marker for an allowlisted workspace row.
///
/// The prose is SERVER-DERIVED: event type plus typed, bounded payload fields
/// (integers, a validated short hash, control-stripped length-capped strings).
/// Missing fields degrade to a shorter sentence instead of failing the row —
/// the marker's job is "something happened in the workspace", not a faithful
/// replay of the audit record, which stays on the ledger.
fn compose_workspace_marker(event_type: &str, p: &WorkspaceEventPayload) -> String {
    let quoted = |value: &Option<String>| {
        value
            .as_deref()
            .map(|v| bounded_quotable(v, 64))
            .filter(|v| !v.is_empty())
    };
    match event_type {
        "room.workspace.provisioned" => {
            let mut line = String::from("workspace provisioned");
            let mut detail = Vec::new();
            if let Some(driver) = quoted(&p.driver) {
                detail.push(driver);
            }
            if let Some(n) = p.hydrated_files {
                detail.push(format!("{n} files hydrated"));
            }
            if !detail.is_empty() {
                line.push_str(&format!(" ({})", detail.join(", ")));
            }
            line
        }
        "room.workspace.destroyed" => match p.flushed_files {
            Some(n) => format!("workspace destroyed ({n} files flushed)"),
            None => "workspace destroyed".into(),
        },
        "room.workspace.repo_cloned" => {
            let mut line = String::from("workspace repo cloned");
            if let Some(branch) = quoted(&p.branch) {
                line.push_str(&format!(": '{branch}'"));
            }
            if let Some(sha) = p.head_sha.as_deref().and_then(short_sha) {
                line.push_str(&format!(" @ {sha}"));
            }
            line
        }
        "room.workspace.repo_clone_failed" => {
            let mut line = String::from("workspace repo clone failed");
            if let Some(branch) = quoted(&p.branch) {
                line.push_str(&format!(": '{branch}'"));
            }
            if let Some(code) = p.exit_code {
                line.push_str(&format!(" (exit {code})"));
            }
            line
        }
        "room.workspace.repo_unbound" => {
            let mut line = String::from("workspace repo unbound");
            if let Some(branch) = quoted(&p.branch) {
                line.push_str(&format!(": '{branch}'"));
            }
            match p.checkout_removed {
                Some(true) => line.push_str(" — checkout removed"),
                // `no_container` means nothing existed to remove, so the
                // plain sentence is the honest rendering. Any other false
                // left a live checkout the next flush re-ingests as room
                // files — the alarm this marker exists to raise.
                Some(false) if p.checkout_removed_reason.as_deref() != Some("no_container") => {
                    line.push_str(" — checkout removal failed")
                }
                _ => {}
            }
            line
        }
        "room.workspace.build_finished" | "room.workspace.build_failed" => {
            let outcome = if event_type == "room.workspace.build_finished" {
                "succeeded"
            } else {
                "failed"
            };
            let mut line = String::from("workspace build");
            if let Some(script) = quoted(&p.script) {
                line.push_str(&format!(" '{script}'"));
            }
            line.push(' ');
            line.push_str(outcome);
            let mut detail = Vec::new();
            if let Some(code) = p.exit_code {
                detail.push(format!("exit {code}"));
            }
            if let Some(ms) = p.duration_ms {
                detail.push(fmt_duration_ms(ms));
            }
            if !detail.is_empty() {
                line.push_str(&format!(" ({})", detail.join(", ")));
            }
            line
        }
        "room.workspace.port_exposed" => match p.port {
            Some(port) => format!("workspace port {port} exposed"),
            None => "workspace port exposed".into(),
        },
        // Reports the recorded close, not a torn-down route: Bedrock drops
        // the port row and emits this event even when the driver's unexpose
        // threw, and the payload does not distinguish the two.
        "room.workspace.port_closed" => match p.port {
            Some(port) => format!("workspace port {port} closed"),
            None => "workspace port closed".into(),
        },
        "room.workspace.flushed" => match p.changed_files {
            Some(n) => format!("workspace flushed ({n} files changed)"),
            None => "workspace flushed".into(),
        },
        "room.workspace.hydrated" => match p.hydrated_files {
            Some(n) => format!("workspace hydrated ({n} files)"),
            None => "workspace hydrated".into(),
        },
        "room.workspace.ci_checked" => {
            let mut line = String::from("workspace CI");
            if let Some(branch) = quoted(&p.branch) {
                line.push_str(&format!(" on '{branch}'"));
            }
            match (p.checks_new, p.checks_total) {
                (Some(new), Some(total)) => {
                    let noun = if new == 1 { "result" } else { "results" };
                    line.push_str(&format!(": {new} new {noun} ({total} total)"));
                }
                (Some(new), None) => {
                    let noun = if new == 1 { "result" } else { "results" };
                    line.push_str(&format!(": {new} new {noun}"));
                }
                _ => line.push_str(" checked"),
            }
            // The count says how much news there is; the conclusions ARE the
            // news. A marker is one line, so only the first few checks get
            // named — the full set stays on the ledger — and an in-progress
            // run (null conclusion) is skipped rather than rendered half-empty.
            let named: Vec<String> = p
                .checks
                .iter()
                .flatten()
                .filter_map(|check| {
                    let name = bounded_quotable(check.name.as_deref()?, 32);
                    let conclusion = bounded_quotable(check.conclusion.as_deref()?, 16);
                    (!name.is_empty() && !conclusion.is_empty())
                        .then(|| format!("{name}: {conclusion}"))
                })
                .take(3)
                .collect();
            if !named.is_empty() {
                line.push_str(&format!(" — {}", named.join(", ")));
            }
            line
        }
        "room.workspace.execs_purged" => {
            let mut line = String::from("workspace exec");
            // Bedrock spells a purge of every row as the sentinel id 'all';
            // rendering that as a name would read as an exec literally
            // called "all". A real id is quoted through the bounded filter
            // even though Bedrock validates it upstream — this lane never
            // trusts a payload string, however it got here.
            if let Some(exec_id) = quoted(&p.exec_id).filter(|id| id != "all") {
                line.push_str(&format!(" '{exec_id}'"));
            }
            line.push_str(" output purged");
            if let Some(n) = p.purged_rows {
                let noun = if n == 1 { "row" } else { "rows" };
                line.push_str(&format!(" ({n} {noun})"));
            }
            line
        }
        // Unreachable behind `workspace_action_is_marker`, but a total
        // function keeps the allowlist the single behavioral gate.
        other => format!("workspace event {}", bounded_quotable(other, 64)),
    }
}

/// Ingest one allowlisted `room.workspace.*` ledger row as a System
/// transcript marker.
///
/// Workspace rows ride the same hash-chained ledger as messages but carry
/// none of the message lane's producer fields, so they cannot go through
/// `ingest_message_row`'s validation. They still need exactly-once ingest
/// across SSE reconnect/replay, which `ingest_confirmed_event`'s dedup on
/// `ledger_event_id` already provides — so the marker is written as a
/// `ConfirmedEvent` whose meta is honest about what it is:
/// - `ledger_event_id` and `global_sequence` are the row's REAL ledger
///   identity — dedup and strict ordering run against the same authority as
///   messages.
/// - `source_id`, `source_sequence`, and `client_event_id` are SYNTHESIZED
///   relay fields (no producer stream exists for server-emitted workspace
///   events): the fixed [`WORKSPACE_MARKER_SOURCE_ID`], the global sequence,
///   and the ledger event id. All three are deterministic functions of the
///   row, so a replayed row rebuilds byte-identical meta and lands in the
///   store's Duplicate arm instead of its corruption arm.
///
/// `trigger_targets` is filled for exactly one row kind: `build_failed`, and
/// only in a room whose trigger policy opts in via `on_build_failure` (ruled
/// 2026-08-29: a build failure is a trigger event on the existing convene
/// path, not a new mechanism). Targets are the roster's Agent members; the
/// store's claim site keeps only the locally-bound ones and consumes each
/// (row, target) pair once, and the dispatcher re-validates ownership and
/// binding before queuing a turn — so a replayed row or a foreign agent can
/// never be convened from here. Every other workspace row keeps empty
/// targets: the marker reaching agents through the transcript on their NEXT
/// convened turn is the point of this lane.
fn ingest_workspace_row(
    inner: &Arc<SupervisorInner>,
    key: &RoomKey,
    row: WireLedgerRow,
) -> Result<IngestDisposition, BridgeError> {
    let sequence = parse_canonical_u64(&row.sequence)?;
    if row.id.is_empty() {
        return Err(BridgeError::Protocol);
    }
    let payload: WorkspaceEventPayload =
        serde_json::from_value(row.payload).map_err(|_| BridgeError::Protocol)?;
    let body = compose_workspace_marker(&row.event_type, &payload);
    // Only a failed build consults the policy; a green build and a CI result
    // stay pure markers. The over-broad roster read is deliberate — the store
    // and the dispatcher both re-filter (see the doc above).
    let (trigger_targets, trigger_reason) = if row.event_type == "room.workspace.build_failed" {
        let policy = with_rooms_handle(&inner.rooms, |store| store.trigger_policy(key))
            .map_err(|_| BridgeError::Store)?;
        let decision = evaluate_trigger_policy(policy.as_ref(), &RoomTriggerEvent::BuildFailed);
        let targets = if decision.should_convene {
            with_rooms_handle(&inner.rooms, |store| store.room_access(key))
                .map_err(|_| BridgeError::Store)?
                .members
                .into_iter()
                .filter(|member| member.actor_type == FederatedActorType::Agent)
                .map(|member| member.member_id)
                .collect()
        } else {
            Vec::new()
        };
        (targets, decision.reason)
    } else {
        (Vec::new(), String::new())
    };
    let non_empty = |value: Option<String>| value.filter(|v| !v.is_empty());
    let event = ConfirmedEvent {
        ledger_event_id: row.id.clone(),
        global_sequence: sequence,
        source_id: WORKSPACE_MARKER_SOURCE_ID.into(),
        source_sequence: sequence,
        client_event_id: row.id,
        // Bedrock attributes workspace activity to the member who caused it
        // on the ledger row itself; keep that attribution when present, and
        // record the lane's own name rather than an empty string when the
        // event is server-originated.
        origin_principal_id: non_empty(row.actor_id)
            .unwrap_or_else(|| WORKSPACE_MARKER_SOURCE_ID.into()),
        origin_member_id: non_empty(row.actor_member_id)
            .unwrap_or_else(|| WORKSPACE_MARKER_SOURCE_ID.into()),
        author_id: "system".into(),
        author_kind: RoomParticipantKind::System,
        kind: RoomMessageKind::System,
        body,
        trigger_targets,
    };
    let outcome = with_rooms_handle(&inner.rooms, |store| {
        store.ingest_confirmed_event(key, &event, chrono::Utc::now())
    })
    .map_err(|_| BridgeError::Store)?;
    match outcome {
        IngestOutcome::Duplicate => Ok(IngestDisposition::Duplicate),
        IngestOutcome::Ingested(commit) => {
            // Same post-commit wake pair as a message: the transcript wake is
            // what lets an open client repaint on workspace activity instead
            // of polling for it.
            publish_room_wake_on(&inner.room_wakes, key, &commit.message);
            publish_room_access_wake_on(&inner.access_wakes, key);
            // Same dispatch loop as the message path: a claim recorded in the
            // store without a matching send here would consume the row's
            // one convene and wake nobody.
            for target_member_id in commit.claimed_trigger_targets {
                if inner
                    .trigger_tx
                    .send(FederatedTriggerDispatch {
                        room: key.clone(),
                        ledger_event_id: event.ledger_event_id.clone(),
                        local_seq: commit.message.seq,
                        target_member_id,
                        reason: trigger_reason.clone(),
                    })
                    .is_err()
                {
                    tracing::warn!(room = %key, outcome = "dispatch_receiver_closed", "federated trigger suppressed");
                }
            }
            Ok(IngestDisposition::Committed)
        }
    }
}

fn advance_non_message(
    inner: &Arc<SupervisorInner>,
    key: &RoomKey,
    sequence: u64,
) -> Result<IngestDisposition, BridgeError> {
    let prior = durable_cursor(&inner.rooms, key).map_err(|_| BridgeError::Store)?;
    match prior {
        Some(current) if sequence < current => return Err(BridgeError::Protocol),
        Some(current) if sequence == current => return Ok(IngestDisposition::Duplicate),
        _ => {}
    }
    with_rooms_handle(&inner.rooms, |store| {
        store.update_room_access_safe(key, None, None, Some(sequence))
    })
    .map_err(|_| BridgeError::Store)?;
    publish_room_access_wake_on(&inner.access_wakes, key);
    Ok(IngestDisposition::Committed)
}

fn author_kind(
    inner: &Arc<SupervisorInner>,
    key: &RoomKey,
    member_id: &str,
) -> Result<Option<RoomParticipantKind>, BridgeError> {
    let projection = with_rooms_handle(&inner.rooms, |store| store.room_access(key))
        .map_err(|_| BridgeError::Store)?;
    Ok(author_kind_from_members(&projection.members, member_id))
}

fn author_kind_from_members(
    members: &[FederatedRoomMemberProjection],
    member_id: &str,
) -> Option<RoomParticipantKind> {
    members
        .iter()
        .find(|member| member.member_id == member_id)
        .map(|member| match member.actor_type {
            FederatedActorType::Agent => RoomParticipantKind::Agent,
            FederatedActorType::User => RoomParticipantKind::Human,
        })
}

fn commit_access(
    inner: &Arc<SupervisorInner>,
    key: &RoomKey,
    state: RoomAccessState,
    members: Option<&[FederatedRoomMemberProjection]>,
    cursor: Option<u64>,
) -> bool {
    let committed = with_rooms_handle(&inner.rooms, |store| {
        store.update_room_access_safe(key, Some(state), members, cursor)
    });
    if committed.is_ok() {
        publish_room_access_wake_on(&inner.access_wakes, key);
        true
    } else {
        false
    }
}

fn commit_state_only(inner: &Arc<SupervisorInner>, key: &RoomKey, state: RoomAccessState) -> bool {
    commit_access(inner, key, state, None, None)
}

fn commit_presence_snapshot(
    inner: &Arc<SupervisorInner>,
    key: &RoomKey,
    state: RoomAccessState,
    members: &[FederatedRoomMemberProjection],
) -> bool {
    commit_access(inner, key, state, Some(members), None)
}

fn apply_presence_frame(
    inner: &Arc<SupervisorInner>,
    key: &RoomKey,
    state: RoomAccessState,
    incoming: &[PresenceWireMember],
    live_human_member_ids: &mut HashSet<String>,
) -> bool {
    // `room_presence` frames report the currently-live human members; other
    // actor types carry no presence meaning here, so a mixed/additive frame
    // that also echoes non-User entries (M4) is handled by ignoring them
    // rather than discarding the whole update. The one thing that can't be
    // safely interpreted is a User entry with an empty `member_id` — that
    // alone still fails the frame.
    if incoming
        .iter()
        .any(|member| member.actor_type == FederatedActorType::User && member.member_id.is_empty())
    {
        return false;
    }
    let live_humans: HashSet<String> = incoming
        .iter()
        .filter(|member| member.actor_type == FederatedActorType::User)
        .map(|member| member.member_id.clone())
        .collect();
    // Read the current roster, derive presence, and commit the update under
    // a single lock acquisition (M3). Splitting this into a separate read
    // (`with_rooms_handle`) followed by a separate `commit_access` call
    // left a window for a concurrent writer — another presence frame, a
    // heartbeat roster refresh, a message ingest — to interleave between
    // the two lock acquisitions and have its update silently lost when this
    // stale read was written back.
    let committed = with_rooms_handle(&inner.rooms, |store| {
        let projection = store.room_access(key)?;
        let mut members = projection.members;
        for member in &mut members {
            member.derived_presence = match member.actor_type {
                FederatedActorType::User => {
                    Some(if live_humans.contains(member.member_id.as_str()) {
                        MemberPresence::Live
                    } else {
                        MemberPresence::Unavailable
                    })
                }
                FederatedActorType::Agent => None,
            };
        }
        store.update_room_access_safe(key, Some(state), Some(&members), None)
    });
    if committed.is_err() {
        return false;
    }
    // Only replace the epoch-local live-humans cache after the durable
    // commit has actually succeeded (M2). Clearing/repopulating it before
    // the write was confirmed left it out of sync with the store on any
    // failed commit (an UnknownRoom race, a poisoned-lock recovery that
    // still errors, etc.), silently wiping the in-memory presence view the
    // next roster fetch (`fetch_roster`/`project_roster`) derives from.
    *live_human_member_ids = live_humans;
    publish_room_access_wake_on(&inner.access_wakes, key);
    true
}

fn apply_mirrored_read_cursor_frame(
    inner: &Arc<SupervisorInner>,
    credential: &RoomCredential,
    key: &RoomKey,
    sequence: Option<String>,
) -> Result<bool, BridgeError> {
    let read_seq = match sequence {
        Some(sequence) => Some(parse_canonical_u64(&sequence)?),
        None => None,
    };
    let before = with_rooms_handle(&inner.rooms, |store| {
        store.room_read_cursor(key, credential.local_human_member_id.as_str())
    })
    .map_err(|_| BridgeError::Store)?;
    let cas = converge_room_read_cursor_mirror(
        &inner.rooms,
        key,
        credential.local_human_member_id.as_str(),
        before.mirrored_upstream_read_seq,
        read_seq,
    )
    .map_err(|_| BridgeError::Store)?;
    // F5: `was_applied()` only tells us the (possibly retried/converged)
    // write was accepted, not that it actually moved the value — writing
    // back the same sequence that was already mirrored (a duplicate/no-op
    // frame) is `Applied` (it's not stale) but is not a real change, and
    // must not trigger a wake. True change detection compares what
    // actually landed on disk against what was there immediately before
    // this write started.
    let changed = cas.was_applied()
        && cas.into_projection().mirrored_upstream_read_seq != before.mirrored_upstream_read_seq;
    Ok(changed)
}

fn persist_lease_lost(
    inner: &Arc<SupervisorInner>,
    key: &RoomKey,
    state: RoomAccessState,
) -> Result<(), BridgeError> {
    let projection = with_rooms_handle(&inner.rooms, |store| store.room_access(key))
        .map_err(|_| BridgeError::Store)?;
    lease_lost_transition(Ok(projection), |members| {
        commit_access(inner, key, state, Some(&members), None)
    })
}

fn lease_lost_transition(
    projection: Result<RoomAccessProjection, BridgeError>,
    commit: impl FnOnce(Vec<FederatedRoomMemberProjection>) -> bool,
) -> Result<(), BridgeError> {
    let mut members = projection?.members;
    for member in &mut members {
        match member.actor_type {
            FederatedActorType::User => {
                member.derived_presence = Some(MemberPresence::Unavailable);
            }
            FederatedActorType::Agent => {
                member.derived_presence = None;
            }
        }
    }
    if commit(members) {
        Ok(())
    } else {
        Err(BridgeError::Store)
    }
}

async fn revoke_room(inner: &Arc<SupervisorInner>, key: &RoomKey) {
    // Sender admission is already closed and its child joined by run_epoch.
    // Fail every durable Pending row first; persist Revoked last; one wake.
    let pending = match with_rooms_handle(&inner.rooms, |store| store.pending_outbox(key)) {
        Ok(pending) => pending,
        Err(_) => {
            // Never persist Revoked unless every Pending row was enumerated
            // for cleanup. A later boot/wire denial retries this fail-closed.
            let _ = persist_lease_lost(inner, key, RoomAccessState::Recovering);
            return;
        }
    };
    let committed = with_rooms_handle(&inner.rooms, |store| {
        for row in &pending {
            store.fail_outbox_pending(key, &row.client_event_id)?;
        }
        let projection = store.room_access(key)?;
        let mut members = projection.members;
        for member in &mut members {
            match member.actor_type {
                FederatedActorType::User => {
                    member.derived_presence = Some(MemberPresence::Unavailable);
                }
                FederatedActorType::Agent => {
                    member.derived_presence = None;
                }
            }
        }
        store.update_room_access_safe(key, Some(RoomAccessState::Revoked), Some(&members), None)?;
        Ok::<(), ocean_store::RoomStoreError>(())
    });
    if committed.is_ok() {
        publish_room_access_wake_on(&inner.access_wakes, key);
    }
}

fn fail_row_and_wake(inner: &Arc<SupervisorInner>, key: &RoomKey, client_event_id: &str) {
    let changed = with_rooms_handle(&inner.rooms, |store| {
        store.fail_outbox_pending(key, client_event_id)
    });
    if matches!(changed, Ok(true)) {
        publish_room_access_wake_on(&inner.access_wakes, key);
    }
}

fn startup_should_start(state: Result<RoomAccessState, BridgeError>) -> Result<bool, BridgeError> {
    state.map(|state| state != RoomAccessState::Revoked)
}

fn cursor_or_zero(cursor: Result<Option<u64>, BridgeError>) -> Result<u64, BridgeError> {
    cursor.map(|cursor| cursor.unwrap_or(0))
}

fn ensure_live_with(
    current: Result<RoomAccessState, BridgeError>,
    promote: impl FnOnce() -> bool,
) -> Result<(), BridgeError> {
    match current? {
        RoomAccessState::Live => Ok(()),
        _ if promote() => Ok(()),
        _ => Err(BridgeError::Store),
    }
}

fn durable_cursor(rooms: &RoomStoreHandle, key: &RoomKey) -> Result<Option<u64>, BridgeError> {
    with_rooms_handle(rooms, |store| store.room_access(key))
        .map(|projection| projection.last_confirmed_global_sequence)
        .map_err(|_| BridgeError::Store)
}

fn durable_state(rooms: &RoomStoreHandle, key: &RoomKey) -> Result<RoomAccessState, BridgeError> {
    with_rooms_handle(rooms, |store| store.room_access(key))
        .map(|projection| projection.state)
        .map_err(|_| BridgeError::Store)
}

fn access_state_for_hello(cursor: u64, high_water: u64) -> RoomAccessState {
    if cursor >= high_water {
        // cursor > H is intentional Live: H is a snapshot watermark and the
        // durable cursor must never regress to a lagging replica's view.
        RoomAccessState::Live
    } else {
        RoomAccessState::Recovering
    }
}

fn wire_row_scope_is_exact(row: &WireLedgerRow, key: &RoomKey) -> bool {
    row.correlation_id == key.as_str() && row.virtual_path == format!("/rooms/{}", key.as_str())
}

fn sender_scan_delay(base: Duration, generation: u64, scan: u64) -> Duration {
    let base_ms = base.as_millis().max(1) as u64;
    // Deterministic ±10% jitter; bounded periodicity remains testable.
    let bucket = generation
        .wrapping_mul(6364136223846793005)
        .wrapping_add(scan.wrapping_mul(1442695040888963407))
        % 21;
    Duration::from_millis((base_ms.saturating_mul(90 + bucket) / 100).max(1))
}

fn reconnect_delay(attempt: u32, salt: u64) -> Duration {
    let exponent = attempt.min(6);
    let base_ms = 1_000u64
        .saturating_mul(1u64 << exponent)
        .min(BACKOFF_MAX.as_millis() as u64);
    // Deterministic ±20% jitter: stable per generation/attempt and testable.
    let bucket = salt
        .wrapping_mul(6364136223846793005)
        .wrapping_add(attempt as u64)
        % 41;
    let percent = 80 + bucket;
    Duration::from_millis(base_ms.saturating_mul(percent) / 100)
}

fn parse_sse_json<T: DeserializeOwned>(data: &str) -> Result<T, BridgeError> {
    if data.len() > BODY_LIMIT {
        return Err(BridgeError::Protocol);
    }
    serde_json::from_str(data).map_err(|_| BridgeError::Protocol)
}

fn parse_canonical_u64(text: &str) -> Result<u64, BridgeError> {
    if text.is_empty()
        || (text.len() > 1 && text.starts_with('0'))
        || !text.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(BridgeError::Protocol);
    }
    let value = text.parse::<u64>().map_err(|_| BridgeError::Protocol)?;
    if value.to_string() != text {
        return Err(BridgeError::Protocol);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        convert::Infallible,
        sync::{
            atomic::{AtomicU16, AtomicUsize},
            Mutex as StdMutex,
        },
    };

    use axum::{
        body::Bytes,
        extract::{Path, State},
        http::{HeaderMap, Uri},
        response::{sse::Event, IntoResponse, Sse},
        routing::{get, post},
        Json, Router,
    };
    use ocean_store::RoomStore;
    use serde_json::json;
    use tokio_stream::wrappers::ReceiverStream;

    static ENV_LOCK: StdMutex<()> = StdMutex::new(());

    #[test]
    fn p2c_room_keys_and_control_member_invariants_are_strict() {
        assert!(canonical_room_key("room-a_1.test"));
        for bad in ["", "Room", "-room", "room/path", "room%2fchild"] {
            assert!(!canonical_room_key(bad), "accepted {bad}");
        }
        let human = ControlEnvelopeMember {
            member_id: "11111111-1111-4111-8111-111111111111".into(),
            owner_member_id: None,
            actor_type: FederatedActorType::User,
            role_in_room: FederatedRoomRole::Member,
            display_name: "Human".into(),
            public_agent_descriptor: None,
        };
        assert!(valid_human_member(&human, FederatedRoomRole::Member));
        assert!(!valid_human_member(&human, FederatedRoomRole::Owner));
        let clean: RedeemEnvelope = serde_json::from_value(json!({
            "invite":{"role":"contributor","scopes":["/rooms/room"],"expiresAt":"later"},
            "record":{"role":"contributor","scopes":["/rooms/room"]}
        }))
        .unwrap();
        assert!(!clean.token_present);
        let leaked: RedeemEnvelope = serde_json::from_value(json!({
            "invite":{"role":"contributor","scopes":["/rooms/room"],"expiresAt":"later"},
            "record":{"role":"contributor","scopes":["/rooms/room"]},
            "token":null
        }))
        .unwrap();
        assert!(leaked.token_present, "even a null token field is forbidden");
    }

    #[test]
    fn p2c_control_body_transport_is_unavailable_not_protocol() {
        assert_eq!(
            control_body_error(BridgeError::Transport),
            IntentError::Unavailable
        );
        assert_eq!(
            control_body_error(BridgeError::Protocol),
            IntentError::Protocol
        );
    }

    #[test]
    fn canonical_u64_is_lossless_and_strict() {
        assert_eq!(parse_canonical_u64("0").unwrap(), 0);
        assert_eq!(
            parse_canonical_u64("9007199254740993").unwrap(),
            9_007_199_254_740_993
        );
        assert_eq!(
            parse_canonical_u64("18446744073709551615").unwrap(),
            u64::MAX
        );
        for bad in [
            "",
            "00",
            "01",
            "+1",
            "-1",
            " 1",
            "1 ",
            "1.0",
            "18446744073709551616",
        ] {
            assert!(parse_canonical_u64(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn raw_and_parsed_sse_json_are_bounded() {
        let mut raw = RawSseEventBound::new(16);
        raw.accept(b"event: x\n").unwrap();
        assert_eq!(
            raw.accept(b"data: 123456789"),
            Err(RawSseError::EventTooLarge)
        );
        let mut split = RawSseEventBound::new(12);
        split.accept(b"data: 123").unwrap();
        assert_eq!(split.accept(b"456789"), Err(RawSseError::EventTooLarge));

        let mut reset = RawSseEventBound::new(16);
        reset.accept(b"data: x\n\n").unwrap();
        reset.accept(b"data: y\n\n").unwrap();

        // Parsed-size defense in depth remains behind the raw cap.
        let valid = json!({"sequence":"1"}).to_string();
        assert_eq!(
            parse_sse_json::<HeartbeatFrame>(&valid).unwrap().sequence,
            "1"
        );
        let oversized = format!("{{\"sequence\":\"{}\"}}", "1".repeat(BODY_LIMIT));
        assert!(parse_sse_json::<HeartbeatFrame>(&oversized).is_err());
    }

    #[test]
    fn federation_url_requires_origin_https_or_loopback_http() {
        for good in [
            "https://bedrock.example.com",
            "https://bedrock.example.com:8443/",
            "http://127.0.0.1:14780",
            "http://[::1]:14780",
            "http://localhost:14780",
        ] {
            assert!(FederationClient::new(good).is_ok(), "rejected {good}");
        }
        for bad in [
            "http://bedrock.example.com",
            "https://user@bedrock.example.com",
            "https://@bedrock.example.com",
            "https://bedrock.example.com/path",
            "https://bedrock.example.com?token=x",
            "https://bedrock.example.com#frag",
            "file:///tmp/x",
        ] {
            assert!(FederationClient::new(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn invite_onboard_url_composed_unless_the_base_is_loopback() {
        for (base, expected) in [
            (
                "https://bedrock.example.com",
                "https://bedrock.example.com/api/v1/invites/fake-code/onboard",
            ),
            (
                "https://bedrock.example.com/",
                "https://bedrock.example.com/api/v1/invites/fake-code/onboard",
            ),
            (
                "https://bedrock.example.com:8443",
                "https://bedrock.example.com:8443/api/v1/invites/fake-code/onboard",
            ),
        ] {
            let client = FederationClient::new(base).unwrap();
            assert_eq!(
                client.invite_onboard_url("fake-code").as_deref(),
                Some(expected),
                "bad onboard link for {base}"
            );
        }
        // A base only this daemon can resolve must not be handed to an invitee,
        // whatever scheme it wears.
        for base in [
            "http://127.0.0.1:14780",
            "http://localhost:14780",
            "http://[::1]:14780",
            "https://127.0.0.1:14780",
            "https://LOCALHOST:14780",
        ] {
            let client = FederationClient::new(base).unwrap();
            assert_eq!(
                client.invite_onboard_url("fake-code"),
                None,
                "loopback base {base} produced a link"
            );
        }
    }

    #[test]
    fn invite_onboard_url_escapes_the_code_into_one_segment() {
        // The code's shape is Bedrock's to decide, so it rides as a segment and
        // never as a path: a separator in it must not grow the URL a segment.
        let client = FederationClient::new("https://bedrock.example.com").unwrap();
        assert_eq!(
            client.invite_onboard_url("a/b?c").as_deref(),
            Some("https://bedrock.example.com/api/v1/invites/a%2Fb%3Fc/onboard")
        );
    }

    #[test]
    fn outbox_validation_denies_extra_fields_and_non_message() {
        let base = RoomOutboxItem {
            client_event_id: "e".into(),
            source_id: "s".into(),
            source_sequence: 1,
            author_member_id: "m".into(),
            event_type: "message".into(),
            payload: serde_json::json!({"body":"hi"}),
            mention_member_ids: vec![],
            state: ocean_core::OutboxItemState::Pending,
        };
        assert_eq!(validate_outbox_item(&base).unwrap().body, "hi");
        let mut extra = base.clone();
        extra.payload = serde_json::json!({"body":"hi", "token":"no"});
        assert!(validate_outbox_item(&extra).is_err());
        let mut other = base;
        other.event_type = "join".into();
        assert!(validate_outbox_item(&other).is_err());
    }

    #[test]
    fn env_missing_is_not_invalid_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let old = std::env::var_os(FEDERATION_URL_ENV);
        std::env::remove_var(FEDERATION_URL_ENV);
        assert!(FederationClient::from_env().unwrap().is_none());
        if let Some(old) = old {
            std::env::set_var(FEDERATION_URL_ENV, old);
        }
    }

    #[test]
    fn post_status_matrix_is_exact() {
        assert_eq!(
            classify_post_response(StatusCode::CREATED, None),
            PostAction::AwaitConfirmation
        );
        assert_eq!(
            classify_post_response(StatusCode::OK, None),
            PostAction::Recover,
            "unexpected 2xx is protocol failure"
        );
        assert_eq!(
            classify_post_response(StatusCode::BAD_REQUEST, Some("unknown_mention_member")),
            PostAction::FailRow
        );
        assert_eq!(
            classify_post_response(StatusCode::CONFLICT, None),
            PostAction::FailRow
        );
        assert_eq!(
            classify_post_response(StatusCode::FORBIDDEN, Some("member_actor_mismatch")),
            PostAction::FailRow
        );
        assert_eq!(
            classify_post_response(StatusCode::FORBIDDEN, Some("membership_revoked")),
            PostAction::Revoke
        );
        assert_eq!(
            classify_post_response(StatusCode::UNAUTHORIZED, None),
            PostAction::Revoke
        );
        assert_eq!(
            classify_post_response(StatusCode::TOO_MANY_REQUESTS, None),
            PostAction::Recover
        );
        assert_eq!(
            classify_post_response(StatusCode::INTERNAL_SERVER_ERROR, None),
            PostAction::Recover
        );
    }

    #[test]
    fn projection_error_never_commits_wakes_starts_or_substitutes_empty_roster() {
        assert_eq!(
            startup_should_start(Err(BridgeError::Store)),
            Err(BridgeError::Store)
        );
        assert!(!startup_should_start(Ok(RoomAccessState::Revoked)).unwrap());
        assert!(startup_should_start(Ok(RoomAccessState::Live)).unwrap());

        let commits = std::cell::Cell::new(0);
        assert_eq!(
            lease_lost_transition(Err(BridgeError::Store), |_| {
                commits.set(commits.get() + 1);
                true
            }),
            Err(BridgeError::Store)
        );
        assert_eq!(
            commits.get(),
            0,
            "projection error invokes no write/wake commit closure"
        );

        let projection = RoomAccessProjection {
            state: RoomAccessState::Live,
            last_confirmed_global_sequence: Some(4),
            members: vec![FederatedRoomMemberProjection {
                member_id: "member-preserved".into(),
                owner_member_id: None,
                actor_type: FederatedActorType::User,
                role_in_room: FederatedRoomRole::Owner,
                display_name: "Preserved".into(),
                public_agent_descriptor: None,
                joined_at: "2026-07-17T00:00:00Z".into(),
                derived_presence: Some(MemberPresence::Live),
                local_binding_available: None,
            }],
            self_member_id: None,
            outbox: vec![],
        };
        lease_lost_transition(Ok(projection), |members| {
            commits.set(commits.get() + 1);
            assert_eq!(members.len(), 1, "never substitutes an empty roster");
            assert_eq!(members[0].member_id, "member-preserved");
            assert_eq!(
                members[0].derived_presence,
                Some(MemberPresence::Unavailable)
            );
            true
        })
        .unwrap();
        assert_eq!(commits.get(), 1);
    }

    #[test]
    fn store_authority_errors_never_fall_back_or_promote() {
        assert_eq!(cursor_or_zero(Ok(None)).unwrap(), 0);
        assert_eq!(cursor_or_zero(Ok(Some(7))).unwrap(), 7);
        assert_eq!(
            cursor_or_zero(Err(BridgeError::Store)),
            Err(BridgeError::Store)
        );

        let promoted = std::cell::Cell::new(false);
        assert_eq!(
            ensure_live_with(Err(BridgeError::Store), || {
                promoted.set(true);
                true
            }),
            Err(BridgeError::Store)
        );
        assert!(!promoted.get(), "store read error never attempts promotion");
        assert_eq!(
            ensure_live_with(Ok(RoomAccessState::Recovering), || false),
            Err(BridgeError::Store),
            "failed Live commit ends the epoch"
        );
        assert!(ensure_live_with(Ok(RoomAccessState::Recovering), || true).is_ok());
        assert!(ensure_live_with(Ok(RoomAccessState::Live), || false).is_ok());
    }

    #[test]
    fn hello_cursor_policy_and_scan_jitter_are_bounded() {
        assert_eq!(access_state_for_hello(99, 100), RoomAccessState::Recovering);
        assert_eq!(access_state_for_hello(100, 100), RoomAccessState::Live);
        assert_eq!(access_state_for_hello(101, 100), RoomAccessState::Live);
        for scan in 0..100 {
            let delay = sender_scan_delay(Duration::from_secs(10), 7, scan);
            assert!((Duration::from_secs(9)..=Duration::from_secs(11)).contains(&delay));
        }
    }

    #[tokio::test]
    async fn room_read_cursor_frame_updates_mirror_without_http_or_confirmed_cursor() {
        let key = RoomKey::new("cursor-frame-room");
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Cursor", None, chrono::Utc::now())
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), Some(&[]), Some(9))
            .unwrap();
        store
            .install_room_credential(&key, "bearer", "principal")
            .unwrap();
        store
            .update_room_read_cursor(
                &key,
                "principal",
                ocean_core::RoomReadCursorUpdateRequest { read_seq: 9 },
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "bearer");
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(25),
        );
        let inner = supervisor.inner.clone();
        let credential = with_rooms_handle(&rooms, |store| store.room_credential(&key))
            .unwrap()
            .unwrap();
        apply_mirrored_read_cursor_frame(
            &inner,
            &credential,
            &key,
            Some("9007199254740993".into()),
        )
        .unwrap();
        let cursor =
            with_rooms_handle(&rooms, |store| store.room_read_cursor(&key, "principal")).unwrap();
        let access = with_rooms_handle(&rooms, |store| store.room_access(&key)).unwrap();
        assert_eq!(cursor.read_seq, None);
        assert_eq!(
            cursor.mirrored_upstream_read_seq,
            Some(9_007_199_254_740_993)
        );
        assert_eq!(access.last_confirmed_global_sequence, Some(9));
        assert!(fake.read_cursor_calls.lock().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn room_read_cursor_live_get_and_patch_use_exact_path_body_and_optional_clamped() {
        let key = RoomKey::new("cursor-live-room");
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Cursor", None, chrono::Utc::now())
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), Some(&[]), None)
            .unwrap();
        store
            .install_room_credential(&key, "bearer", "principal")
            .unwrap();
        store
            .update_room_read_cursor(
                &key,
                "principal",
                ocean_core::RoomReadCursorUpdateRequest { read_seq: 0 },
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "bearer");
        *fake.read_cursor_response.lock().await =
            json!({"room_id": key.as_str(), "sequence": null, "clamped": false});
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(25),
        );

        let got = supervisor.room_get_read_cursor(&key).await.unwrap();
        assert_eq!(got.mirrored_upstream_read_seq, None);

        *fake.read_cursor_response.lock().await = json!({
            "room_id": key.as_str(),
            "sequence": "9007199254740993",
            "changed": true,
            "clamped": false
        });
        let patched = supervisor
            .room_patch_read_cursor(&key, 9_007_199_254_740_993)
            .await
            .unwrap();
        assert_eq!(
            patched.mirrored_upstream_read_seq,
            Some(9_007_199_254_740_993)
        );

        let calls = fake.read_cursor_calls.lock().await.clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].method, "GET");
        assert_eq!(calls[0].path, format!("/api/v1/rooms/{}/read-cursor", key));
        assert_eq!(calls[0].authorization.as_deref(), Some("Bearer bearer"));
        assert!(calls[0].body.is_none());
        assert_eq!(calls[1].method, "PATCH");
        assert_eq!(calls[1].path, format!("/api/v1/rooms/{}/read-cursor", key));
        assert_eq!(calls[1].authorization.as_deref(), Some("Bearer bearer"));
        assert_eq!(
            calls[1].body.as_ref().unwrap(),
            &json!({"sequence":"9007199254740993"})
        );

        let cursor =
            with_rooms_handle(&rooms, |store| store.room_read_cursor(&key, "principal")).unwrap();
        assert_eq!(
            cursor.mirrored_upstream_read_seq,
            Some(9_007_199_254_740_993)
        );
        server.abort();
    }

    #[tokio::test]
    async fn absent_upstream_read_cursor_route_is_unsupported_not_a_missing_local_room() {
        let key = RoomKey::new("cursor-unsupported-room");
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Cursor", None, chrono::Utc::now())
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), Some(&[]), None)
            .unwrap();
        store
            .install_room_credential(&key, "bearer", "principal")
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "bearer");
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(25),
        );

        fake.read_cursor_status
            .store(StatusCode::NOT_FOUND.as_u16(), Ordering::Release);
        assert_eq!(
            supervisor.room_get_read_cursor(&key).await,
            Err(IntentError::Conflict),
            "an absent Bedrock route must not lie that the local room is absent"
        );

        fake.read_cursor_status
            .store(StatusCode::METHOD_NOT_ALLOWED.as_u16(), Ordering::Release);
        assert_eq!(
            supervisor.room_patch_read_cursor(&key, 8).await,
            Err(IntentError::Conflict)
        );
        let cursor =
            with_rooms_handle(&rooms, |store| store.room_read_cursor(&key, "principal")).unwrap();
        assert_eq!(cursor.mirrored_upstream_read_seq, None);
        server.abort();
    }

    #[tokio::test]
    async fn room_read_cursor_live_rejects_mismatch_without_mirror_or_wake() {
        let key = RoomKey::new("cursor-mismatch-room");
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Cursor", None, chrono::Utc::now())
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), Some(&[]), None)
            .unwrap();
        store
            .install_room_credential(&key, "bearer", "principal")
            .unwrap();
        store
            .update_room_read_cursor(
                &key,
                "principal",
                ocean_core::RoomReadCursorUpdateRequest { read_seq: 0 },
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let wakes = RoomReadCursorWakeBus::default();
        let mut rx = wakes.subscribe();
        let fake = FakeBedrock::new(key.as_str(), "bearer");
        *fake.read_cursor_response.lock().await = json!({
            "room_id": key.as_str(),
            "sequence": "7",
            "changed": true,
            "clamped": false
        });
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            wakes,
            CancellationToken::new(),
            Duration::from_millis(25),
        );

        assert_eq!(
            supervisor.room_patch_read_cursor(&key, 8).await,
            Err(IntentError::Protocol)
        );
        let cursor =
            with_rooms_handle(&rooms, |store| store.room_read_cursor(&key, "principal")).unwrap();
        assert_eq!(cursor.mirrored_upstream_read_seq, None);
        assert!(tokio::time::timeout(Duration::from_millis(50), rx.recv())
            .await
            .is_err());
        server.abort();
    }

    /// H3 regression: the upstream read-cursor store is authoritative and
    /// may clamp our requested `read_seq` down to its own high-water mark,
    /// signalling that explicitly via `"clamped": true`. That truthful,
    /// explicitly-flagged response must be accepted and mirrored — not
    /// treated as the same protocol violation as an unflagged mismatch.
    #[tokio::test]
    async fn room_read_cursor_live_patch_accepts_flagged_authoritative_clamp() {
        let key = RoomKey::new("cursor-clamped-room");
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Cursor", None, chrono::Utc::now())
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), Some(&[]), None)
            .unwrap();
        store
            .install_room_credential(&key, "bearer", "principal")
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let wakes = RoomReadCursorWakeBus::default();
        let mut rx = wakes.subscribe();
        let fake = FakeBedrock::new(key.as_str(), "bearer");
        *fake.read_cursor_response.lock().await = json!({
            "room_id": key.as_str(),
            "sequence": "7",
            "changed": true,
            "clamped": true
        });
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            wakes,
            CancellationToken::new(),
            Duration::from_millis(25),
        );

        let patched = supervisor
            .room_patch_read_cursor(&key, 8)
            .await
            .expect("an authoritative, explicitly-clamped response must be accepted");
        assert_eq!(patched.mirrored_upstream_read_seq, Some(7));
        let cursor =
            with_rooms_handle(&rooms, |store| store.room_read_cursor(&key, "principal")).unwrap();
        assert_eq!(cursor.mirrored_upstream_read_seq, Some(7));
        assert!(tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .is_ok());
        server.abort();
    }

    /// H3: a response that neither echoes the requested value NOR claims to
    /// be clamped is still rejected as a protocol violation — the fix only
    /// trusts an explicit `clamped: true` signal, it does not silently
    /// accept every non-matching sequence.
    #[tokio::test]
    async fn room_read_cursor_live_patch_still_rejects_unflagged_lower_sequence() {
        let key = RoomKey::new("cursor-unflagged-lower-room");
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Cursor", None, chrono::Utc::now())
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), Some(&[]), None)
            .unwrap();
        store
            .install_room_credential(&key, "bearer", "principal")
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "bearer");
        *fake.read_cursor_response.lock().await = json!({
            "room_id": key.as_str(),
            "sequence": "7",
            "changed": true,
            "clamped": false
        });
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(25),
        );

        assert_eq!(
            supervisor.room_patch_read_cursor(&key, 8).await,
            Err(IntentError::Protocol)
        );
        let cursor =
            with_rooms_handle(&rooms, |store| store.room_read_cursor(&key, "principal")).unwrap();
        assert_eq!(cursor.mirrored_upstream_read_seq, None);
        server.abort();
    }

    #[tokio::test]
    async fn revoke_admission_gate_blocks_post_response_mutation() {
        let gate = AdmissionGate::new();
        let mutations = std::sync::Mutex::new(0);
        assert!(gate
            .mutate(|| *mutations.lock().unwrap() += 1)
            .await
            .is_some());
        gate.close().await;
        assert!(gate
            .mutate(|| *mutations.lock().unwrap() += 1)
            .await
            .is_none());
        assert_eq!(*mutations.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn gate_closed_before_admission_sends_zero_requests() {
        let fake = FakeBedrock::new("barrier-room", "barrier-bearer");
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let client = FederationClient::new(&base).unwrap();
        let gate = AdmissionGate::new();
        gate.close().await;
        let request = client
            .http
            .post(client.ledger_endpoint().unwrap())
            .bearer_auth("barrier-bearer")
            .json(&json!({"event_type":"message"}));
        assert!(matches!(
            gate.send(request, &CancellationToken::new()).await,
            AdmittedSend::Closed
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(fake.posts.lock().await.is_empty());
        server.abort();
    }

    #[test]
    fn reconnect_delay_is_bounded() {
        for attempt in 0..100 {
            let delay = reconnect_delay(attempt, 7);
            assert!(delay <= Duration::from_secs(72));
            assert!(delay >= Duration::from_millis(800));
        }
    }

    type FakeSseTx = mpsc::Sender<Result<Event, Infallible>>;
    type RequestMeta = (String, Option<String>, Option<String>);

    #[derive(Clone)]
    struct ReadCursorCall {
        method: String,
        path: String,
        authorization: Option<String>,
        body: Option<Value>,
    }

    #[derive(Clone)]
    struct FakeBedrock {
        bearer: Arc<String>,
        room: Arc<String>,
        sse_tx: Arc<Mutex<Option<FakeSseTx>>>,
        posts: Arc<Mutex<Vec<Value>>>,
        read_cursor_calls: Arc<Mutex<Vec<ReadCursorCall>>>,
        request_meta: Arc<Mutex<Vec<RequestMeta>>>,
        members: Arc<Mutex<Value>>,
        read_cursor_status: Arc<AtomicU16>,
        read_cursor_response: Arc<Mutex<Value>>,
        members_status: Arc<AtomicU16>,
        events_status: Arc<AtomicU16>,
        hold_events_response: Arc<AtomicBool>,
        release_events_response: Arc<Notify>,
        oversized_incomplete_event: Arc<AtomicBool>,
        ledger_status: Arc<AtomicU16>,
        ledger_error: Arc<Mutex<String>>,
        hold_ledger_response: Arc<AtomicBool>,
        release_ledger_response: Arc<Notify>,
        posted: Arc<Notify>,
    }

    impl FakeBedrock {
        fn new(room: &str, bearer: &str) -> Self {
            Self {
                bearer: Arc::new(bearer.to_string()),
                room: Arc::new(room.to_string()),
                sse_tx: Arc::new(Mutex::new(None)),
                posts: Arc::new(Mutex::new(Vec::new())),
                read_cursor_calls: Arc::new(Mutex::new(Vec::new())),
                request_meta: Arc::new(Mutex::new(Vec::new())),
                members: Arc::new(Mutex::new(json!({"members":[]}))),
                read_cursor_status: Arc::new(AtomicU16::new(200)),
                read_cursor_response: Arc::new(Mutex::new(
                    json!({"room_id": room, "sequence": null}),
                )),
                members_status: Arc::new(AtomicU16::new(200)),
                events_status: Arc::new(AtomicU16::new(200)),
                hold_events_response: Arc::new(AtomicBool::new(false)),
                release_events_response: Arc::new(Notify::new()),
                oversized_incomplete_event: Arc::new(AtomicBool::new(false)),
                ledger_status: Arc::new(AtomicU16::new(201)),
                ledger_error: Arc::new(Mutex::new(String::new())),
                hold_ledger_response: Arc::new(AtomicBool::new(false)),
                release_ledger_response: Arc::new(Notify::new()),
                posted: Arc::new(Notify::new()),
            }
        }
    }

    fn bearer(headers: &HeaderMap) -> Option<String> {
        headers
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    }

    async fn fake_events(
        State(state): State<FakeBedrock>,
        Path(room): Path<String>,
        uri: Uri,
        headers: HeaderMap,
    ) -> axum::response::Response {
        let auth = bearer(&headers);
        let cursor = headers
            .get("last-event-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        state
            .request_meta
            .lock()
            .await
            .push((uri.to_string(), auth.clone(), cursor));
        if room != *state.room
            || auth.as_deref() != Some(format!("Bearer {}", state.bearer).as_str())
            || uri.query().is_some()
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let status = StatusCode::from_u16(state.events_status.load(Ordering::Acquire))
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status != StatusCode::OK {
            return status.into_response();
        }
        if state.hold_events_response.load(Ordering::Acquire) {
            state.release_events_response.notified().await;
        }
        if state.oversized_incomplete_event.load(Ordering::Acquire) {
            let raw = format!(
                "event: room_event\ndata: {}",
                "x".repeat(SSE_EVENT_LIMIT + 128)
            );
            return axum::response::Response::builder()
                .status(StatusCode::OK)
                .header(reqwest::header::CONTENT_TYPE, "text/event-stream")
                .body(axum::body::Body::from(raw))
                .unwrap();
        }
        let (tx, rx) = mpsc::channel(16);
        tx.send(Ok(Event::default().event("hello").data(
            json!({"room_id": room, "snapshot_high_water":"1"}).to_string(),
        )))
        .await
        .unwrap();
        *state.sse_tx.lock().await = Some(tx);
        Sse::new(ReceiverStream::new(rx)).into_response()
    }

    async fn fake_members(
        State(state): State<FakeBedrock>,
        Path(room): Path<String>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        if room != *state.room
            || bearer(&headers).as_deref() != Some(format!("Bearer {}", state.bearer).as_str())
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let status = StatusCode::from_u16(state.members_status.load(Ordering::Acquire))
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status != StatusCode::OK {
            return status.into_response();
        }
        Json(state.members.lock().await.clone()).into_response()
    }

    async fn fake_read_cursor_get(
        State(state): State<FakeBedrock>,
        Path(room): Path<String>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        state.read_cursor_calls.lock().await.push(ReadCursorCall {
            method: "GET".into(),
            path: format!("/api/v1/rooms/{room}/read-cursor"),
            authorization: bearer(&headers),
            body: None,
        });
        if room != *state.room
            || bearer(&headers).as_deref() != Some(format!("Bearer {}", state.bearer).as_str())
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let status = StatusCode::from_u16(state.read_cursor_status.load(Ordering::Acquire))
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status != StatusCode::OK {
            return status.into_response();
        }
        Json(state.read_cursor_response.lock().await.clone()).into_response()
    }

    async fn fake_read_cursor_patch(
        State(state): State<FakeBedrock>,
        Path(room): Path<String>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        state.read_cursor_calls.lock().await.push(ReadCursorCall {
            method: "PATCH".into(),
            path: format!("/api/v1/rooms/{room}/read-cursor"),
            authorization: bearer(&headers),
            body: Some(body.clone()),
        });
        if room != *state.room
            || bearer(&headers).as_deref() != Some(format!("Bearer {}", state.bearer).as_str())
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let status = StatusCode::from_u16(state.read_cursor_status.load(Ordering::Acquire))
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status != StatusCode::OK {
            return status.into_response();
        }
        Json(state.read_cursor_response.lock().await.clone()).into_response()
    }

    async fn fake_ledger(
        State(state): State<FakeBedrock>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        if bearer(&headers).as_deref() != Some(format!("Bearer {}", state.bearer).as_str()) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        state.posts.lock().await.push(body);
        state.posted.notify_waiters();
        if state.hold_ledger_response.load(Ordering::Acquire) {
            state.release_ledger_response.notified().await;
        }
        let status = StatusCode::from_u16(state.ledger_status.load(Ordering::Acquire))
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        if status == StatusCode::CREATED {
            (status, Json(json!({"id":"ledger-1"}))).into_response()
        } else {
            let error = state.ledger_error.lock().await.clone();
            (status, Json(json!({"ok":false,"error":error}))).into_response()
        }
    }

    async fn start_fake_bedrock(state: FakeBedrock) -> (String, JoinHandle<()>) {
        let app = Router::new()
            .route("/api/v1/rooms/{room}/events", get(fake_events))
            .route("/api/v1/rooms/{room}/members", get(fake_members))
            .route(
                "/api/v1/rooms/{room}/read-cursor",
                get(fake_read_cursor_get).patch(fake_read_cursor_patch),
            )
            .route("/api/v1/ledger/events", post(fake_ledger))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), task)
    }

    #[derive(Clone)]
    struct ControlCall {
        path: String,
        authorization: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    #[derive(Clone)]
    struct ControlBedrock {
        room: Arc<String>,
        calls: Arc<Mutex<Vec<ControlCall>>>,
        register_status: Arc<AtomicU16>,
        invite_status: Arc<AtomicU16>,
        redeem_status: Arc<AtomicU16>,
        self_status: Arc<AtomicU16>,
        agents_status: Arc<AtomicU16>,
        member_remove_status: Arc<AtomicU16>,
        redeem_active: Arc<AtomicUsize>,
        redeem_peak: Arc<AtomicUsize>,
        hold_redeem: Arc<AtomicBool>,
        release_redeem: Arc<tokio::sync::Semaphore>,
        hold_invite: Arc<AtomicBool>,
        release_invite: Arc<tokio::sync::Semaphore>,
        hold_agents: Arc<AtomicBool>,
        release_agents: Arc<tokio::sync::Semaphore>,
        hold_members: Arc<AtomicBool>,
        release_members: Arc<tokio::sync::Semaphore>,
        roster: Arc<Mutex<Value>>,
    }

    impl ControlBedrock {
        fn new(room: &str) -> Self {
            Self {
                room: Arc::new(room.to_string()),
                calls: Arc::new(Mutex::new(Vec::new())),
                register_status: Arc::new(AtomicU16::new(201)),
                invite_status: Arc::new(AtomicU16::new(201)),
                redeem_status: Arc::new(AtomicU16::new(201)),
                self_status: Arc::new(AtomicU16::new(201)),
                agents_status: Arc::new(AtomicU16::new(201)),
                member_remove_status: Arc::new(AtomicU16::new(200)),
                redeem_active: Arc::new(AtomicUsize::new(0)),
                redeem_peak: Arc::new(AtomicUsize::new(0)),
                hold_redeem: Arc::new(AtomicBool::new(false)),
                release_redeem: Arc::new(tokio::sync::Semaphore::new(0)),
                hold_invite: Arc::new(AtomicBool::new(false)),
                release_invite: Arc::new(tokio::sync::Semaphore::new(0)),
                hold_agents: Arc::new(AtomicBool::new(false)),
                release_agents: Arc::new(tokio::sync::Semaphore::new(0)),
                hold_members: Arc::new(AtomicBool::new(false)),
                release_members: Arc::new(tokio::sync::Semaphore::new(0)),
                roster: Arc::new(Mutex::new(json!({"members":[]}))),
            }
        }

        async fn record(&self, path: &str, headers: &HeaderMap, body: &Bytes) {
            self.calls.lock().await.push(ControlCall {
                path: path.to_string(),
                authorization: bearer(headers),
                content_type: headers
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned),
                body: body.to_vec(),
            });
        }
    }

    fn control_error(status: StatusCode) -> axum::response::Response {
        (status, Json(json!({"ok":false,"error":"control_error"}))).into_response()
    }

    async fn control_register(
        State(state): State<ControlBedrock>,
        Path(room): Path<String>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        state.record("register", &headers, &body).await;
        if room != *state.room || bearer(&headers).as_deref() != Some("Bearer test-owner-token") {
            return control_error(StatusCode::UNAUTHORIZED);
        }
        let status = StatusCode::from_u16(state.register_status.load(Ordering::Acquire)).unwrap();
        if !matches!(status, StatusCode::OK | StatusCode::CREATED) {
            return control_error(status);
        }
        (
            status,
            Json(json!({
                "room_id":room,
                "owner":{
                    "member_id":"11111111-1111-4111-8111-111111111111",
                    "actor_type":"user",
                    "role_in_room":"owner",
                    "display_name":"Owner"
                }
            })),
        )
            .into_response()
    }

    async fn control_invite(
        State(state): State<ControlBedrock>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        state.record("invite", &headers, &body).await;
        if state.hold_invite.load(Ordering::Acquire) {
            let permit = state.release_invite.acquire().await.unwrap();
            permit.forget();
        }
        if bearer(&headers).as_deref() != Some("Bearer test-owner-token") {
            return control_error(StatusCode::UNAUTHORIZED);
        }
        let status = StatusCode::from_u16(state.invite_status.load(Ordering::Acquire)).unwrap();
        if status != StatusCode::CREATED {
            return control_error(status);
        }
        (
            status,
            Json(json!({
                "code":"share-code",
                "invite":{
                    "role":"contributor",
                    "scopes":[format!("/rooms/{}", state.room)],
                    "expiresAt":"2026-07-18T00:00:00Z"
                }
            })),
        )
            .into_response()
    }

    async fn control_redeem(
        State(state): State<ControlBedrock>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        state.record("redeem", &headers, &body).await;
        let active = state.redeem_active.fetch_add(1, Ordering::AcqRel) + 1;
        state.redeem_peak.fetch_max(active, Ordering::AcqRel);
        if state.hold_redeem.load(Ordering::Acquire) {
            let permit = state.release_redeem.acquire().await.unwrap();
            permit.forget();
        }
        state.redeem_active.fetch_sub(1, Ordering::AcqRel);
        let status = StatusCode::from_u16(state.redeem_status.load(Ordering::Acquire)).unwrap();
        if !matches!(status, StatusCode::OK | StatusCode::CREATED) {
            return control_error(status);
        }
        let response = json!({
            "invite":{
                "role":"contributor",
                "scopes":[format!("/rooms/{}", state.room)],
                "expiresAt":"2026-07-18T00:00:00Z"
            },
            "record":{
                "role":"contributor",
                "scopes":[format!("/rooms/{}", state.room)]
            }
        });
        (status, Json(response)).into_response()
    }

    async fn control_self_join(
        State(state): State<ControlBedrock>,
        Path(room): Path<String>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        state.record("self", &headers, &body).await;
        let status = StatusCode::from_u16(state.self_status.load(Ordering::Acquire)).unwrap();
        if room != *state.room || !matches!(status, StatusCode::OK | StatusCode::CREATED) {
            return control_error(status);
        }
        (
            status,
            Json(json!({
                "member":{
                    "member_id":"22222222-2222-4222-8222-222222222222",
                    "actor_type":"user",
                    "role_in_room":"member",
                    "display_name":"Joined Human"
                }
            })),
        )
            .into_response()
    }

    async fn control_agents(
        State(state): State<ControlBedrock>,
        Path(room): Path<String>,
        headers: HeaderMap,
        body: Bytes,
    ) -> axum::response::Response {
        state.record("agents", &headers, &body).await;
        if state.hold_agents.load(Ordering::Acquire) {
            let permit = state.release_agents.acquire().await.unwrap();
            permit.forget();
        }
        if room != *state.room {
            return control_error(StatusCode::BAD_REQUEST);
        }
        let status = StatusCode::from_u16(state.agents_status.load(Ordering::Acquire)).unwrap();
        if !matches!(status, StatusCode::OK | StatusCode::CREATED) {
            return control_error(status);
        }
        let request: Value = serde_json::from_slice(&body).unwrap();
        let requested = request["agents"].as_array().unwrap();
        let members: Vec<Value> = requested
            .iter()
            .enumerate()
            .map(|(index, agent)| {
                json!({
                    "member_id":format!("33333333-3333-4333-8333-{index:012}"),
                    "owner_member_id":"22222222-2222-4222-8222-222222222222",
                    "actor_type":"agent",
                    "role_in_room":"member",
                    "display_name":agent["display_name"],
                    "public_agent_descriptor":{
                        "display_name":agent["display_name"],
                        "description":agent.get("description"),
                        "model_alias":agent.get("model_alias"),
                        "skills_count":agent["skills_count"],
                        "subagent_names":agent["subagent_names"]
                    },
                    "joined_at":"2026-07-17T00:00:00Z"
                })
            })
            .collect();
        let mut roster = vec![json!({
            "member_id":"22222222-2222-4222-8222-222222222222",
            "actor_type":"user",
            "role_in_room":"member",
            "display_name":"Joined Human",
            "joined_at":"2026-07-17T00:00:00Z"
        })];
        roster.extend(members.iter().cloned());
        *state.roster.lock().await = json!({"members":roster});
        (status, Json(json!({"members":members}))).into_response()
    }

    async fn control_members(
        State(state): State<ControlBedrock>,
        Path(room): Path<String>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        state.record("members", &headers, &Bytes::new()).await;
        if state.hold_members.load(Ordering::Acquire) {
            let permit = state.release_members.acquire().await.unwrap();
            permit.forget();
        }
        if room != *state.room {
            return control_error(StatusCode::BAD_REQUEST);
        }
        Json(state.roster.lock().await.clone()).into_response()
    }

    /// DELETE members/{id}, the route the agent-delete federated sweep
    /// dials. Records the target member id in the call path so tests can
    /// prove which member was addressed.
    async fn control_member_remove(
        State(state): State<ControlBedrock>,
        Path((room, member)): Path<(String, String)>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        state
            .record(&format!("members/{member}"), &headers, &Bytes::new())
            .await;
        if room != *state.room {
            return control_error(StatusCode::BAD_REQUEST);
        }
        let status =
            StatusCode::from_u16(state.member_remove_status.load(Ordering::Acquire)).unwrap();
        if !status.is_success() {
            return control_error(status);
        }
        // A confirmed removal must vanish from the roster this fake serves,
        // or the caller's post-removal roster refresh would test nothing.
        let mut roster = state.roster.lock().await;
        if let Some(members) = roster["members"].as_array_mut() {
            members.retain(|entry| entry["member_id"].as_str() != Some(member.as_str()));
        }
        (status, Json(json!({"removed":[member]}))).into_response()
    }

    async fn start_control_bedrock(state: ControlBedrock) -> (String, JoinHandle<()>) {
        let app = Router::new()
            .route("/api/v1/rooms/{room}/register", post(control_register))
            .route("/api/v1/invites", post(control_invite))
            .route("/api/v1/invites/redeem", post(control_redeem))
            .route("/api/v1/rooms/{room}/members/self", post(control_self_join))
            .route("/api/v1/rooms/{room}/members/agents", post(control_agents))
            .route("/api/v1/rooms/{room}/members", get(control_members))
            .route(
                "/api/v1/rooms/{room}/members/{member}",
                axum::routing::delete(control_member_remove),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), task)
    }

    // TASK-28 deadline policy: every timeout in this module is a POSITIVE wait
    // (the event must arrive; sub-second solo). 60s, not 1-2s — a saturated CI
    // runner sharing 500+ parallel tests starves these tasks and tight
    // deadlines fail PRs that touch unrelated crates (TASK-27 fired exactly
    // here). Progress-guaranteed waits lose nothing from a generous budget.
    // Negative nothing-arrives claims use try_recv asserts, never timeouts.
    async fn wait_for_control_call(fake: &ControlBedrock, path: &str) {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if fake.calls.lock().await.iter().any(|call| call.path == path) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("control call {path} did not arrive"));
    }

    fn test_control_supervisor(base: &str, rooms: RoomStoreHandle) -> FederationSupervisor {
        FederationSupervisor::for_test(
            base,
            rooms,
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        )
    }

    #[tokio::test]
    async fn p2c_enqueue_is_outbox_only_filters_mentions_and_rejects_closed() {
        let key = RoomKey::new("enqueue-room");
        let human = "11111111-1111-4111-8111-111111111111";
        let agent = "33333333-3333-4333-8333-333333333333";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Enqueue", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "enqueue-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[
                    FederatedRoomMemberProjection {
                        member_id: human.into(),
                        owner_member_id: None,
                        actor_type: FederatedActorType::User,
                        role_in_room: FederatedRoomRole::Owner,
                        display_name: "Owner".into(),
                        public_agent_descriptor: None,
                        joined_at: "2026-07-17T00:00:00Z".into(),
                        derived_presence: Some(MemberPresence::Live),
                        local_binding_available: None,
                    },
                    FederatedRoomMemberProjection {
                        member_id: agent.into(),
                        owner_member_id: Some(human.into()),
                        actor_type: FederatedActorType::Agent,
                        role_in_room: FederatedRoomRole::Member,
                        display_name: "Agent".into(),
                        public_agent_descriptor: Some(PublicAgentDescriptor {
                            display_name: "Agent".into(),
                            description: None,
                            model_alias: None,
                            skills_count: 0,
                            subagent_names: vec![],
                        }),
                        joined_at: "2026-07-17T00:00:00Z".into(),
                        derived_presence: Some(MemberPresence::Live),
                        local_binding_available: Some(true),
                    },
                ]),
                None,
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let access_wakes = RoomAccessWakeBus::default();
        let mut access_rx = access_wakes.subscribe();
        let supervisor = FederationSupervisor::test_disabled(
            rooms.clone(),
            RoomWakeBus::default(),
            access_wakes,
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
        );

        let projection = supervisor
            .enqueue_federated_message(&key, None, &format!("hello @{agent} @unknown @{agent}"))
            .await
            .unwrap();
        assert_eq!(projection.outbox.len(), 1);
        assert_eq!(projection.outbox[0].author_member_id, human);
        assert_eq!(projection.outbox[0].mention_member_ids, vec![agent]);
        assert_eq!(
            projection.outbox[0].payload,
            json!({"body":format!("hello @{agent} @unknown @{agent}")})
        );
        assert!(with_rooms_handle(&rooms, |s| s.transcript(&key, None))
            .unwrap()
            .is_empty());
        access_rx.recv().await.unwrap();

        with_rooms_handle(&rooms, |s| s.close(&key)).unwrap();
        assert_eq!(
            supervisor
                .enqueue_federated_message(&key, None, "after close")
                .await,
            Err(IntentError::NotFound)
        );
        assert!(access_rx.try_recv().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p2c_enqueue_revoke_race_leaves_no_pending_after_revoked() {
        let key = RoomKey::new("enqueue-revoke-race");
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Enqueue Revoke", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "race-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let supervisor = FederationSupervisor::test_disabled(
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
        );
        let slot = supervisor.slot_for(&key).await;
        let barrier = Arc::new(tokio::sync::Barrier::new(3));

        let enqueue = tokio::spawn({
            let supervisor = supervisor.clone();
            let key = key.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                supervisor
                    .enqueue_federated_message(&key, None, "racing intent")
                    .await
            }
        });
        let revoke = tokio::spawn({
            let inner = supervisor.inner.clone();
            let key = key.clone();
            let barrier = barrier.clone();
            async move {
                barrier.wait().await;
                slot.control.close().await;
                revoke_room(&inner, &key).await;
            }
        });
        barrier.wait().await;
        let (enqueue, revoke) = tokio::join!(enqueue, revoke);
        let enqueue = enqueue.unwrap();
        revoke.unwrap();
        assert!(matches!(enqueue, Ok(_) | Err(IntentError::Forbidden)));
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.room_access(&key))
                .unwrap()
                .state,
            RoomAccessState::Revoked
        );
        assert!(
            with_rooms_handle(&rooms, |s| s.pending_outbox(&key))
                .unwrap()
                .is_empty(),
            "an admitted pre-revoke row must be Failed and a post-close enqueue must be rejected"
        );
    }

    fn p2c_message_row(
        key: &RoomKey,
        id: &str,
        sequence: u64,
        author: &str,
        mentions: Vec<String>,
    ) -> WireLedgerRow {
        WireLedgerRow {
            id: id.into(),
            sequence: sequence.to_string(),
            event_type: "message".into(),
            correlation_id: key.as_str().into(),
            virtual_path: format!("/rooms/{}", key.as_str()),
            actor_id: Some("principal".into()),
            actor_member_id: Some(author.into()),
            source_id: Some(format!(
                "room:{}:member:{author}:producer:test",
                key.as_str()
            )),
            source_sequence: Some(sequence.to_string()),
            payload: json!({
                "client_event_id":format!("client-{sequence}"),
                "author_member_id":author,
                "body":"trigger test",
                "mention_member_ids":mentions
            }),
        }
    }

    fn p2c_projected_member(
        id: &str,
        actor_type: FederatedActorType,
        owner: Option<&str>,
    ) -> FederatedRoomMemberProjection {
        FederatedRoomMemberProjection {
            member_id: id.into(),
            owner_member_id: owner.map(str::to_owned),
            actor_type,
            role_in_room: if owner.is_some() {
                FederatedRoomRole::Member
            } else {
                FederatedRoomRole::Owner
            },
            display_name: id.into(),
            public_agent_descriptor: (actor_type == FederatedActorType::Agent).then(|| {
                PublicAgentDescriptor {
                    display_name: id.into(),
                    description: None,
                    model_alias: None,
                    skills_count: 0,
                    subagent_names: vec![],
                }
            }),
            joined_at: "2026-07-17T00:00:00Z".into(),
            derived_presence: Some(MemberPresence::Live),
            local_binding_available: (actor_type == FederatedActorType::Agent).then_some(true),
        }
    }

    #[tokio::test]
    async fn p2c_positive_user_dispatches_once_and_receiver_closure_suppresses_replay() {
        let key = RoomKey::new("trigger-user-room");
        let human = "11111111-1111-4111-8111-111111111111";
        let target = "33333333-3333-4333-8333-333333333333";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(
                key.clone(),
                "Trigger User",
                Some(ocean_core::RoomTriggerPolicy {
                    on_mention: true,
                    ..Default::default()
                }),
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .install_room_credential(&key, "bearer", human)
            .unwrap();
        store.bind_room_agent(&key, target, "sage", "key").unwrap();
        store
            .update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[
                    p2c_projected_member(human, FederatedActorType::User, None),
                    p2c_projected_member(target, FederatedActorType::Agent, Some(human)),
                ]),
                None,
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel();
        let supervisor = FederationSupervisor::for_test_with_trigger(
            "http://127.0.0.1:1",
            rooms,
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            trigger_tx,
            CancellationToken::new(),
            Duration::from_secs(60),
        );
        let credential = RoomCredential {
            room_id: key.clone(),
            bearer_token: "bearer".into(),
            local_human_member_id: human.into(),
        };
        let outcome = ingest_message_row(
            &supervisor.inner,
            supervisor.inner.client.as_ref().unwrap(),
            &credential,
            p2c_message_row(&key, "ledger-user", 1, human, vec![target.into()]),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, IngestDisposition::Committed);
        let dispatch = trigger_rx.try_recv().unwrap();
        assert_eq!(dispatch.target_member_id, target);
        assert!(trigger_rx.try_recv().is_err());
        drop(trigger_rx);
        let outcome = ingest_message_row(
            &supervisor.inner,
            supervisor.inner.client.as_ref().unwrap(),
            &credential,
            p2c_message_row(
                &key,
                "ledger-closed-receiver",
                2,
                human,
                vec![target.into()],
            ),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert_eq!(outcome, IngestDisposition::Committed);
        assert_eq!(
            with_rooms_handle(&supervisor.inner.rooms, |s| s.transcript(&key, None))
                .unwrap()
                .len(),
            2,
            "closed dispatch receiver suppresses execution but never rolls back confirmed ingest"
        );
    }

    #[tokio::test]
    async fn p2c_unknown_human_fallback_and_agent_author_claim_nothing() {
        let key = RoomKey::new("trigger-suppressed-room");
        let human = "11111111-1111-4111-8111-111111111111";
        let unknown = "22222222-2222-4222-8222-222222222222";
        let target = "33333333-3333-4333-8333-333333333333";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(
                key.clone(),
                "Trigger Suppressed",
                Some(ocean_core::RoomTriggerPolicy {
                    on_mention: true,
                    ..Default::default()
                }),
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .install_room_credential(&key, "bearer", human)
            .unwrap();
        store.bind_room_agent(&key, target, "sage", "key").unwrap();
        store
            .update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[
                    p2c_projected_member(human, FederatedActorType::User, None),
                    p2c_projected_member(target, FederatedActorType::Agent, Some(human)),
                ]),
                None,
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "bearer");
        *fake.members.lock().await = json!({"members":[
            {
                "member_id":human,
                "actor_type":"user",
                "role_in_room":"owner",
                "display_name":"Human",
                "joined_at":"2026-07-17T00:00:00Z"
            },
            {
                "member_id":target,
                "owner_member_id":human,
                "actor_type":"agent",
                "role_in_room":"member",
                "display_name":"sage",
                "public_agent_descriptor":{"display_name":"sage","skills_count":0},
                "joined_at":"2026-07-17T00:00:00Z"
            }
        ]});
        let (base, server) = start_fake_bedrock(fake).await;
        let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel();
        let supervisor = FederationSupervisor::for_test_with_trigger(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            trigger_tx,
            CancellationToken::new(),
            Duration::from_secs(60),
        );
        let credential = RoomCredential {
            room_id: key.clone(),
            bearer_token: "bearer".into(),
            local_human_member_id: human.into(),
        };

        ingest_message_row(
            &supervisor.inner,
            supervisor.inner.client.as_ref().unwrap(),
            &credential,
            p2c_message_row(&key, "ledger-unknown", 1, unknown, vec![target.into()]),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(trigger_rx.try_recv().is_err());
        let last = with_rooms_handle(&rooms, |s| s.transcript(&key, None))
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(last.author_kind, RoomParticipantKind::Human);

        ingest_message_row(
            &supervisor.inner,
            supervisor.inner.client.as_ref().unwrap(),
            &credential,
            p2c_message_row(&key, "ledger-agent", 2, target, vec![target.into()]),
            &HashSet::new(),
        )
        .await
        .unwrap();
        assert!(trigger_rx.try_recv().is_err());
        server.abort();
    }

    /// Regression for PR #366 review comment 3727657452: the immediate
    /// roster refresh triggered by an unknown-author message must derive
    /// human presence from the epoch's live-human cache, not an empty set.
    /// Passing an empty set would mark every human member Unavailable even
    /// though the caller's epoch already knows they are live, silently
    /// clobbering `derived_presence` on unrelated members as a side effect
    /// of an author-kind lookup.
    #[tokio::test]
    async fn p2c_unknown_author_roster_refresh_preserves_epoch_live_human_presence() {
        let key = RoomKey::new("unknown-author-presence-room");
        let human = "11111111-1111-4111-8111-111111111111";
        let unknown = "22222222-2222-4222-8222-222222222222";
        let target = "33333333-3333-4333-8333-333333333333";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(
                key.clone(),
                "Unknown Author Presence",
                None,
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .install_room_credential(&key, "bearer", human)
            .unwrap();
        store.bind_room_agent(&key, target, "sage", "key").unwrap();
        store
            .update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[
                    p2c_projected_member(human, FederatedActorType::User, None),
                    p2c_projected_member(target, FederatedActorType::Agent, Some(human)),
                ]),
                None,
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "bearer");
        // The roster the unknown-author refresh will fetch does not itself
        // carry presence; `human`'s liveness must come from the epoch's
        // cached live-human set passed into `ingest_message_row`, not the
        // server response.
        *fake.members.lock().await = json!({"members":[
            {
                "member_id":human,
                "actor_type":"user",
                "role_in_room":"owner",
                "display_name":"Human",
                "joined_at":"2026-07-17T00:00:00Z"
            },
            {
                "member_id":target,
                "owner_member_id":human,
                "actor_type":"agent",
                "role_in_room":"member",
                "display_name":"sage",
                "public_agent_descriptor":{"display_name":"sage","skills_count":0},
                "joined_at":"2026-07-17T00:00:00Z"
            }
        ]});
        let (base, server) = start_fake_bedrock(fake).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_secs(60),
        );
        let credential = RoomCredential {
            room_id: key.clone(),
            bearer_token: "bearer".into(),
            local_human_member_id: human.into(),
        };

        // The epoch already knows `human` is live (e.g. via an earlier
        // heartbeat or room_presence frame); a message from an unknown
        // author must not discard that knowledge.
        let live_human_member_ids: HashSet<String> = HashSet::from([human.to_string()]);
        let outcome = ingest_message_row(
            &supervisor.inner,
            supervisor.inner.client.as_ref().unwrap(),
            &credential,
            p2c_message_row(&key, "ledger-unknown", 1, unknown, vec![]),
            &live_human_member_ids,
        )
        .await
        .unwrap();
        assert_eq!(outcome, IngestDisposition::Committed);

        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        let human_member = projection
            .members
            .iter()
            .find(|member| member.member_id == human)
            .expect("human member present after roster refresh");
        assert_eq!(
            human_member.derived_presence,
            Some(MemberPresence::Live),
            "unknown-author roster refresh must preserve epoch live-human presence, not mark it Unavailable"
        );
        server.abort();
    }

    #[tokio::test]
    async fn p2c_policy_off_human_and_unbound_remote_targets_dispatch_nowhere() {
        let key = RoomKey::new("trigger-negative-room");
        let human = "11111111-1111-4111-8111-111111111111";
        let remote_human = "22222222-2222-4222-8222-222222222222";
        let bound_agent = "33333333-3333-4333-8333-333333333333";
        let remote_agent = "44444444-4444-4444-8444-444444444444";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Trigger Negatives", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "negative-bearer", human)
            .unwrap();
        store
            .bind_room_agent(&key, bound_agent, "sage", "bound-key")
            .unwrap();
        store
            .update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[
                    p2c_projected_member(human, FederatedActorType::User, None),
                    p2c_projected_member(remote_human, FederatedActorType::User, None),
                    p2c_projected_member(bound_agent, FederatedActorType::Agent, Some(human)),
                    p2c_projected_member(
                        remote_agent,
                        FederatedActorType::Agent,
                        Some(remote_human),
                    ),
                ]),
                None,
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel();
        let supervisor = FederationSupervisor::for_test_with_trigger(
            "http://127.0.0.1:1",
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            trigger_tx,
            CancellationToken::new(),
            Duration::from_secs(60),
        );
        let credential = RoomCredential {
            room_id: key.clone(),
            bearer_token: "negative-bearer".into(),
            local_human_member_id: human.into(),
        };

        ingest_message_row(
            &supervisor.inner,
            supervisor.inner.client.as_ref().unwrap(),
            &credential,
            p2c_message_row(&key, "policy-off", 1, human, vec![bound_agent.into()]),
            &HashSet::new(),
        )
        .await
        .unwrap();
        with_rooms_handle(&rooms, |store| {
            store.update(
                &key,
                None,
                Some(Some(ocean_core::RoomTriggerPolicy {
                    on_mention: true,
                    ..Default::default()
                })),
                chrono::Utc::now(),
            )
        })
        .unwrap();
        ingest_message_row(
            &supervisor.inner,
            supervisor.inner.client.as_ref().unwrap(),
            &credential,
            p2c_message_row(&key, "human-target", 2, human, vec![remote_human.into()]),
            &HashSet::new(),
        )
        .await
        .unwrap();
        ingest_message_row(
            &supervisor.inner,
            supervisor.inner.client.as_ref().unwrap(),
            &credential,
            p2c_message_row(
                &key,
                "remote-unbound-target",
                3,
                human,
                vec![remote_agent.into()],
            ),
            &HashSet::new(),
        )
        .await
        .unwrap();

        assert!(trigger_rx.try_recv().is_err());
        let transcript = with_rooms_handle(&rooms, |store| store.transcript(&key, None)).unwrap();
        assert_eq!(transcript.len(), 3);
        assert!(transcript
            .iter()
            .all(|message| message.kind == RoomMessageKind::Message));
    }

    #[tokio::test]
    async fn p2c_invite_bootstrap_uses_owner_bearer_and_exact_envelope() {
        let key = RoomKey::new("invite-room");
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Invite Room", None, chrono::Utc::now())
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        let invite = supervisor
            .create_invite(&key, Some("Peer".into()), 1440)
            .await
            .unwrap();
        assert!(invite.code == "share-code", "invite code mismatch");
        assert_eq!(invite.room_key, key.as_str());
        assert_eq!(invite.room_name, "Invite Room");
        assert_eq!(
            invite.onboard_url, None,
            "a loopback Bedrock must not hand the invitee a link"
        );
        let credential = with_rooms_handle(&rooms, |s| s.room_credential(&key))
            .unwrap()
            .unwrap();
        assert_eq!(credential.bearer_token, "test-owner-token");
        assert_eq!(
            credential.local_human_member_id,
            "11111111-1111-4111-8111-111111111111"
        );
        let calls = fake.calls.lock().await.clone();
        let register = calls.iter().find(|call| call.path == "register").unwrap();
        assert_eq!(
            register.authorization.as_deref(),
            Some("Bearer test-owner-token")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&register.body).unwrap(),
            json!({"title":"Invite Room"})
        );
        let invite_call = calls.iter().find(|call| call.path == "invite").unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&invite_call.body).unwrap(),
            json!({"room_id":"invite-room","recipient_name":"Peer","ttl_minutes":1440})
        );

        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p2c_bootstrap_and_redeem_install_race_never_overwrites_winner() {
        let key = RoomKey::new("credential-install-race");
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Install Race", None, chrono::Utc::now())
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        fake.hold_invite.store(true, Ordering::Release);
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        let invite = tokio::spawn({
            let supervisor = supervisor.clone();
            let key = key.clone();
            async move { supervisor.create_invite(&key, None, 1440).await }
        });
        wait_for_control_call(&fake, "invite").await;
        let installed = with_rooms_handle(&rooms, |store| store.room_credential(&key))
            .unwrap()
            .unwrap();
        assert_eq!(
            installed.local_human_member_id,
            "11111111-1111-4111-8111-111111111111"
        );
        assert!(
            installed.bearer_token == "test-owner-token",
            "owner bearer mismatch"
        );

        let redeem = tokio::spawn({
            let supervisor = supervisor.clone();
            async move { supervisor.redeem_invite("install-race-code").await }
        });
        wait_for_control_call(&fake, "self").await;
        assert_eq!(
            with_rooms_handle(&rooms, |store| store.list_pending_redemptions())
                .unwrap()
                .len(),
            1
        );

        fake.release_invite.add_permits(1);
        assert!(invite.await.unwrap().is_ok());
        assert_eq!(redeem.await.unwrap(), Err(IntentError::Conflict));
        let final_credential = with_rooms_handle(&rooms, |store| store.room_credential(&key))
            .unwrap()
            .unwrap();
        assert_eq!(
            final_credential.local_human_member_id,
            "11111111-1111-4111-8111-111111111111"
        );
        assert!(
            final_credential.bearer_token == "test-owner-token",
            "losing redemption must not overwrite the owner bearer"
        );
        assert_eq!(
            with_rooms_handle(&rooms, |store| store.list_pending_redemptions())
                .unwrap()
                .len(),
            1,
            "credential conflict retains the durable redemption triple"
        );
        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn p2c_missing_invite_keys_do_not_grow_room_slots() {
        let rooms = Arc::new(std::sync::Mutex::new(
            ocean_store::SqliteRoomStore::open_in_memory().unwrap(),
        ));
        let supervisor = FederationSupervisor::test_disabled(
            rooms,
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
        );
        for index in 0..64 {
            assert!(matches!(
                supervisor
                    .create_invite(&RoomKey::new(format!("missing-{index}")), None, 1440)
                    .await,
                Err(IntentError::NotFound)
            ));
        }
        assert!(
            supervisor.inner.slots.lock().await.is_empty(),
            "missing-key preflight must not allocate lifetime supervisor slots"
        );
    }

    #[tokio::test]
    async fn p2c_owner_register_denial_does_not_revoke_or_install() {
        let key = RoomKey::new("owner-denied-room");
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Owner Denied", None, chrono::Utc::now())
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        fake.register_status.store(403, Ordering::Release);
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        assert!(matches!(
            supervisor.create_invite(&key, None, 1440).await,
            Err(IntentError::Forbidden)
        ));
        assert!(with_rooms_handle(&rooms, |s| s.room_credential(&key))
            .unwrap()
            .is_none());
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.room_access(&key))
                .unwrap()
                .state,
            RoomAccessState::Local
        );
        assert!(!fake
            .calls
            .lock()
            .await
            .iter()
            .any(|call| call.path == "invite"));
        server.abort();
    }

    #[tokio::test]
    async fn p2c_installed_credential_denial_runs_revoke_cleanup() {
        let key = RoomKey::new("invite-revoked-room");
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Invite Revoked", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "test-owner-token", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        fake.invite_status.store(403, Ordering::Release);
        let (base, server) = start_control_bedrock(fake).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        assert!(matches!(
            supervisor.create_invite(&key, None, 1440).await,
            Err(IntentError::Forbidden)
        ));
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.room_access(&key))
                .unwrap()
                .state,
            RoomAccessState::Revoked
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p2c_invite_response_after_revoke_cannot_return_success() {
        let key = RoomKey::new("invite-response-race");
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(
                key.clone(),
                "Invite Response Race",
                None,
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .install_room_credential(&key, "test-owner-token", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        fake.hold_invite.store(true, Ordering::Release);
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        let request = tokio::spawn({
            let supervisor = supervisor.clone();
            let key = key.clone();
            async move { supervisor.create_invite(&key, None, 1440).await }
        });
        wait_for_control_call(&fake, "invite").await;
        supervisor.revoke_control(&key).await;
        fake.release_invite.add_permits(1);
        assert!(matches!(
            request.await.unwrap(),
            Err(IntentError::Forbidden)
        ));
        assert_eq!(
            with_rooms_handle(&rooms, |store| store.room_access(&key))
                .unwrap()
                .state,
            RoomAccessState::Revoked
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p2c_agent_response_after_revoke_binds_nothing() {
        let key = RoomKey::new("agent-response-race");
        let human = "22222222-2222-4222-8222-222222222222";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Agent Response Race", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "agent-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        fake.hold_agents.store(true, Ordering::Release);
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        let request = tokio::spawn({
            let supervisor = supervisor.clone();
            let key = key.clone();
            async move {
                supervisor
                    .register_agents(&key, vec![p2c_agent_input("sage", "race-key")])
                    .await
            }
        });
        wait_for_control_call(&fake, "agents").await;
        supervisor.revoke_control(&key).await;
        fake.release_agents.add_permits(1);
        assert_eq!(request.await.unwrap(), Err(IntentError::Forbidden));
        assert!(
            with_rooms_handle(&rooms, |store| {
                store.resolve_room_agent(&key, "33333333-3333-4333-8333-000000000000")
            })
            .unwrap()
            .is_none(),
            "post-revoke agent response must not bind locally"
        );
        assert_eq!(
            with_rooms_handle(&rooms, |store| store.room_access(&key))
                .unwrap()
                .state,
            RoomAccessState::Revoked
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p2c_roster_response_after_revoke_cannot_mutate_access() {
        let key = RoomKey::new("roster-response-race");
        let human = "22222222-2222-4222-8222-222222222222";
        let member = "33333333-3333-4333-8333-000000000000";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(
                key.clone(),
                "Roster Response Race",
                None,
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .install_room_credential(&key, "agent-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        fake.hold_members.store(true, Ordering::Release);
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        let request = tokio::spawn({
            let supervisor = supervisor.clone();
            let key = key.clone();
            async move {
                supervisor
                    .register_agents(&key, vec![p2c_agent_input("sage", "roster-key")])
                    .await
            }
        });
        wait_for_control_call(&fake, "members").await;
        assert_eq!(
            with_rooms_handle(&rooms, |store| store.resolve_room_agent(&key, member))
                .unwrap()
                .as_deref(),
            Some("sage"),
            "the pre-close binding mutation may commit"
        );
        supervisor.revoke_control(&key).await;
        fake.release_members.add_permits(1);
        assert_eq!(request.await.unwrap(), Err(IntentError::Forbidden));
        let projection = with_rooms_handle(&rooms, |store| store.room_access(&key)).unwrap();
        assert_eq!(projection.state, RoomAccessState::Revoked);
        assert!(
            projection.members.is_empty(),
            "late roster response must not repopulate revoked access"
        );
        server.abort();
    }

    #[tokio::test]
    async fn p2c_redeem_is_restart_safe_and_self_join_is_bodyless() {
        let key = RoomKey::new("redeem-room");
        let rooms = Arc::new(std::sync::Mutex::new(
            ocean_store::SqliteRoomStore::open_in_memory().unwrap(),
        ));
        let fake = ControlBedrock::new(key.as_str());
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        let redeemed = supervisor
            .redeem_invite("  share-code  ")
            .await
            .expect("restart-safe redeem succeeds");
        assert_eq!(
            redeemed.room_key,
            key.as_str(),
            "the reply must name the room the invite's scope resolved to"
        );
        let wire = serde_json::to_value(&redeemed).unwrap();
        assert_eq!(wire["room_key"], key.as_str());
        assert!(
            wire["state"].is_string() && wire.get("access").is_none(),
            "the projection stays at the top level of the redeem reply"
        );
        let calls = fake.calls.lock().await.clone();
        let redeem = calls.iter().find(|call| call.path == "redeem").unwrap();
        let redeem_body: Value = serde_json::from_slice(&redeem.body).unwrap();
        assert!(redeem_body["code"] == "share-code", "redeem code mismatch");
        let token = redeem_body["token"].as_str().unwrap();
        assert_eq!(token.len(), 43);
        assert!(token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')));
        let self_join = calls.iter().find(|call| call.path == "self").unwrap();
        assert!(self_join.body.is_empty());
        assert!(self_join.content_type.is_none());
        assert!(
            self_join
                .authorization
                .as_deref()
                .and_then(|value| value.strip_prefix("Bearer "))
                .is_some_and(|value| value == token),
            "self-join bearer header mismatch"
        );
        let credential = with_rooms_handle(&rooms, |s| s.room_credential(&key))
            .unwrap()
            .unwrap();
        assert!(
            credential.bearer_token == token,
            "stored bearer must equal the caller-generated token"
        );
        assert_eq!(
            credential.local_human_member_id,
            "22222222-2222-4222-8222-222222222222"
        );
        assert!(with_rooms_handle(&rooms, |s| s.list_pending_redemptions())
            .unwrap()
            .is_empty());

        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn p2c_redeem_and_self_join_conflicts_are_409_class_and_retain_pending() {
        let redeem_rooms = Arc::new(std::sync::Mutex::new(
            ocean_store::SqliteRoomStore::open_in_memory().unwrap(),
        ));
        let redeem_fake = ControlBedrock::new("conflict-room");
        redeem_fake.redeem_status.store(409, Ordering::Release);
        let (redeem_base, redeem_server) = start_control_bedrock(redeem_fake).await;
        let redeem_supervisor = test_control_supervisor(&redeem_base, redeem_rooms.clone());
        assert_eq!(
            redeem_supervisor.redeem_invite("redeem-conflict").await,
            Err(IntentError::Conflict)
        );
        assert_eq!(
            with_rooms_handle(&redeem_rooms, |s| s.list_pending_redemptions())
                .unwrap()
                .len(),
            1
        );
        redeem_server.abort();

        let self_rooms = Arc::new(std::sync::Mutex::new(
            ocean_store::SqliteRoomStore::open_in_memory().unwrap(),
        ));
        let self_fake = ControlBedrock::new("self-conflict-room");
        self_fake.self_status.store(409, Ordering::Release);
        let (self_base, self_server) = start_control_bedrock(self_fake).await;
        let self_supervisor = test_control_supervisor(&self_base, self_rooms.clone());
        assert_eq!(
            self_supervisor.redeem_invite("self-conflict").await,
            Err(IntentError::Conflict)
        );
        assert_eq!(
            with_rooms_handle(&self_rooms, |s| s.list_pending_redemptions())
                .unwrap()
                .len(),
            1
        );
        self_server.abort();
    }

    #[tokio::test]
    async fn p2c_terminal_self_join_denial_removes_pending() {
        let rooms = Arc::new(std::sync::Mutex::new(
            ocean_store::SqliteRoomStore::open_in_memory().unwrap(),
        ));
        let fake = ControlBedrock::new("denied-room");
        fake.self_status.store(401, Ordering::Release);
        let (base, server) = start_control_bedrock(fake).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        assert_eq!(
            supervisor.redeem_invite("denied-code").await,
            Err(IntentError::InviteForbidden)
        );
        assert!(with_rooms_handle(&rooms, |s| s.list_pending_redemptions())
            .unwrap()
            .is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn p2c_local_revoked_room_rejects_redeem_and_retains_pending() {
        let key = RoomKey::new("revoked-redeem-room");
        let member = "22222222-2222-4222-8222-222222222222";
        let bearer = "R".repeat(43);
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Revoked Redeem", None, chrono::Utc::now())
            .unwrap();
        let (pending, _) = store
            .get_or_insert_pending_redemption(
                "revoked-code",
                "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
                &bearer,
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .install_room_credential(&key, &bearer, member)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Revoked), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        let (base, server) = start_control_bedrock(fake).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        assert_eq!(
            supervisor.recover_pending(pending).await,
            Err(IntentError::Forbidden)
        );
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.list_pending_redemptions())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.room_access(&key))
                .unwrap()
                .state,
            RoomAccessState::Revoked
        );
        server.abort();
    }

    #[tokio::test]
    async fn p2c_exact_existing_credential_still_promotes_and_deletes_pending() {
        let key = RoomKey::new("promote-room");
        let member = "22222222-2222-4222-8222-222222222222";
        let bearer = "P".repeat(43);
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Promote", None, chrono::Utc::now())
            .unwrap();
        let (pending, _) = store
            .get_or_insert_pending_redemption(
                "promote-code",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                &bearer,
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .install_room_credential(&key, &bearer, member)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        let (base, server) = start_control_bedrock(fake).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        supervisor.recover_pending(pending).await.unwrap();
        assert!(with_rooms_handle(&rooms, |s| s.list_pending_redemptions())
            .unwrap()
            .is_empty());
        let credential = with_rooms_handle(&rooms, |s| s.room_credential(&key))
            .unwrap()
            .unwrap();
        assert_eq!(credential.bearer_token, bearer);
        assert_eq!(credential.local_human_member_id, member);
        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn p2c_missing_pending_with_exact_credential_is_prior_commit_noop() {
        let key = RoomKey::new("prior-promote-room");
        let member = "22222222-2222-4222-8222-222222222222";
        let bearer = "Q".repeat(43);
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Prior Promote", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, &bearer, member)
            .unwrap();
        let pending = PendingRedemption {
            redemption_id: "cccccccc-cccc-4ccc-8ccc-cccccccccccc".into(),
            bearer_token: bearer.clone(),
            invite_code: "prior-code".into(),
            created_at: chrono::Utc::now(),
        };
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        let (base, server) = start_control_bedrock(fake).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        supervisor.recover_pending(pending).await.unwrap();
        let credential = with_rooms_handle(&rooms, |s| s.room_credential(&key))
            .unwrap()
            .unwrap();
        assert_eq!(credential.bearer_token, bearer);
        assert_eq!(credential.local_human_member_id, member);
        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn p2c_different_existing_credential_is_conflict_and_retains_pending() {
        let key = RoomKey::new("conflict-room");
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Conflict", None, chrono::Utc::now())
            .unwrap();
        let (pending, _) = store
            .get_or_insert_pending_redemption(
                "conflict-code",
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                "new-bearer",
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .install_room_credential(&key, "old-bearer", "11111111-1111-4111-8111-111111111111")
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        let (base, server) = start_control_bedrock(fake).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        assert_eq!(
            supervisor.recover_pending(pending).await,
            Err(IntentError::Conflict)
        );
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.list_pending_redemptions())
                .unwrap()
                .len(),
            1
        );
        server.abort();
    }

    #[tokio::test]
    async fn p2c_agent_batch_omits_private_and_absent_fields_then_binds() {
        let key = RoomKey::new("agents-room");
        let member = "22222222-2222-4222-8222-222222222222";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Agents", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "agent-bearer", member)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());
        let input = AgentRegistrationInput {
            agent_name: "sage".into(),
            registration_key: "reg-key".into(),
            descriptor: PublicAgentDescriptor {
                display_name: "sage".into(),
                description: None,
                model_alias: None,
                skills_count: 2,
                subagent_names: vec![],
            },
        };

        let projection = supervisor.register_agents(&key, vec![input]).await.unwrap();
        let calls = fake.calls.lock().await.clone();
        let agent_call = calls.iter().find(|call| call.path == "agents").unwrap();
        let body: Value = serde_json::from_slice(&agent_call.body).unwrap();
        let wire = &body["agents"][0];
        assert!(
            wire["registration_key"] == "reg-key",
            "registration-key mismatch"
        );
        assert!(wire.get("description").is_none());
        assert!(wire.get("model_alias").is_none());
        assert!(wire.get("tools").is_none());
        assert!(wire.get("path").is_none());
        let bound_member = projection
            .members
            .iter()
            .find(|projected| projected.actor_type == FederatedActorType::Agent)
            .unwrap();
        assert_eq!(bound_member.local_binding_available, Some(true));
        assert_eq!(
            with_rooms_handle(&rooms, |s| {
                s.resolve_room_agent(&key, &bound_member.member_id)
            })
            .unwrap()
            .as_deref(),
            Some("sage")
        );
        server.abort();
    }

    #[tokio::test]
    async fn p2c_agent_batch_accepts_exact_maximum_of_32() {
        let key = RoomKey::new("agents-max-room");
        let human = "22222222-2222-4222-8222-222222222222";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Agents Max", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "agent-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms);
        let inputs = (0..32)
            .map(|index| p2c_agent_input(&format!("agent-{index}"), &format!("key-{index}")))
            .collect();

        let projection = supervisor.register_agents(&key, inputs).await.unwrap();
        assert_eq!(
            projection
                .members
                .iter()
                .filter(|member| member.actor_type == FederatedActorType::Agent)
                .count(),
            32
        );
        let calls = fake.calls.lock().await;
        let body: Value = serde_json::from_slice(
            &calls
                .iter()
                .find(|call| call.path == "agents")
                .unwrap()
                .body,
        )
        .unwrap();
        assert_eq!(body["agents"].as_array().unwrap().len(), 32);
        server.abort();
    }

    fn p2c_agent_input(name: &str, key: &str) -> AgentRegistrationInput {
        AgentRegistrationInput {
            agent_name: name.into(),
            registration_key: key.into(),
            descriptor: PublicAgentDescriptor {
                display_name: name.into(),
                description: None,
                model_alias: None,
                skills_count: 0,
                subagent_names: vec![],
            },
        }
    }

    /// One registered federated room ready for the agent-delete sweep:
    /// returns the store handle, the fake, its server handle, the
    /// supervisor, and the bound bedrock member id for "sage".
    async fn sweep_fixture(
        key: &RoomKey,
        bearer: &str,
    ) -> (
        RoomStoreHandle,
        ControlBedrock,
        JoinHandle<()>,
        FederationSupervisor,
        String,
    ) {
        let human = "22222222-2222-4222-8222-222222222222";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Sweep", None, chrono::Utc::now())
            .unwrap();
        store.install_room_credential(key, bearer, human).unwrap();
        store
            .update_room_access_safe(key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        let rooms: RoomStoreHandle = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());
        supervisor
            .register_agents(key, vec![p2c_agent_input("sage", "sweep-key")])
            .await
            .unwrap();
        let member = with_rooms_handle(&rooms, |s| s.resolve_room_agent_member(key, "sage"))
            .unwrap()
            .expect("registration bound a member id");
        (rooms, fake, server, supervisor, member)
    }

    #[tokio::test]
    async fn agent_delete_sweep_removes_unbinds_and_skips_unbound_rooms() {
        let key = RoomKey::new("sweep-room");
        let (rooms, fake, server, supervisor, member) = sweep_fixture(&key, "sweep-bearer").await;
        // A second credentialed room that never registered the agent must
        // not be dialed at all — the binding resolve is the target filter.
        let other = RoomKey::new("sweep-room-unbound");
        with_rooms_handle(&rooms, |s| {
            s.create(other.clone(), "Unbound", None, chrono::Utc::now())?;
            s.install_room_credential(
                &other,
                "other-bearer",
                "33333333-3333-4333-8333-000000000099",
            )
        })
        .unwrap();

        assert_eq!(
            supervisor.sweep_agent_from_federated_rosters("sage").await,
            1
        );
        let calls = fake.calls.lock().await.clone();
        let removals: Vec<_> = calls
            .iter()
            .filter(|call| call.path.starts_with("members/"))
            .collect();
        assert_eq!(removals.len(), 1, "only the bound room is dialed");
        assert_eq!(removals[0].path, format!("members/{member}"));
        assert_eq!(
            removals[0].authorization.as_deref(),
            Some("Bearer sweep-bearer"),
            "removal rides the room's own credential"
        );
        assert!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent_member(&key, "sage"))
                .unwrap()
                .is_none(),
            "confirmed removal unbinds the member"
        );
        assert!(with_rooms_handle(&rooms, |s| s.room_credential(&key))
            .unwrap()
            .is_some());
        server.abort();
    }

    #[tokio::test]
    async fn agent_delete_sweep_policy_403_skips_without_revoking_control() {
        let key = RoomKey::new("sweep-denied-room");
        let (rooms, fake, server, supervisor, _member) =
            sweep_fixture(&key, "sweep-denied-bearer").await;
        fake.member_remove_status.store(403, Ordering::Release);

        assert_eq!(
            supervisor.sweep_agent_from_federated_rosters("sage").await,
            0
        );
        // Nothing confirmed, so nothing forgotten — a later sweep can retry.
        assert!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent_member(&key, "sage"))
                .unwrap()
                .is_some()
        );
        // A policy denial is not a credential event: credential intact,
        // access not Revoked...
        assert!(with_rooms_handle(&rooms, |s| s.room_credential(&key))
            .unwrap()
            .is_some());
        assert_ne!(
            with_rooms_handle(&rooms, |s| s.room_access(&key))
                .unwrap()
                .state,
            RoomAccessState::Revoked
        );
        // ...and the admission gate still admits: once bedrock allows the
        // removal, the retried sweep goes through. A revoked gate would have
        // answered Closed and removed nothing.
        fake.member_remove_status.store(200, Ordering::Release);
        assert_eq!(
            supervisor.sweep_agent_from_federated_rosters("sage").await,
            1
        );
        assert!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent_member(&key, "sage"))
                .unwrap()
                .is_none()
        );
        server.abort();
    }

    #[tokio::test]
    async fn agent_delete_sweep_connection_failure_keeps_binding_and_credential() {
        let key = RoomKey::new("sweep-dark-room");
        let (rooms, _fake, server, supervisor, _member) =
            sweep_fixture(&key, "sweep-dark-bearer").await;
        // Take the peer down for real: abort and await so the listener is
        // closed before the sweep dials, making the refusal deterministic.
        server.abort();
        let _ = server.await;

        assert_eq!(
            supervisor.sweep_agent_from_federated_rosters("sage").await,
            0
        );
        assert!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent_member(&key, "sage"))
                .unwrap()
                .is_some(),
            "an unreachable room keeps its binding for a later retry"
        );
        assert!(with_rooms_handle(&rooms, |s| s.room_credential(&key))
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn agent_delete_sweep_never_dials_a_revoked_room() {
        let key = RoomKey::new("sweep-revoked-room");
        let (rooms, fake, server, supervisor, _member) =
            sweep_fixture(&key, "sweep-revoked-bearer").await;
        // Durable revoke with the credential row left in place — the exact
        // shape `revoke_room` persists, and what a restarted daemon wakes up
        // to: the closed in-memory gate is gone, so only this state can
        // still say no. The fixture's slot gate is open, which makes the
        // test fail if the enumeration ever leans on the gate alone.
        with_rooms_handle(&rooms, |s| {
            s.update_room_access_safe(&key, Some(RoomAccessState::Revoked), None, None)
        })
        .unwrap();

        assert_eq!(
            supervisor.sweep_agent_from_federated_rosters("sage").await,
            0
        );
        let calls = fake.calls.lock().await.clone();
        assert!(
            !calls.iter().any(|call| call.path.starts_with("members/")),
            "a Revoked room's stale bearer is never dialed"
        );
        assert!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent_member(&key, "sage"))
                .unwrap()
                .is_some(),
            "nothing confirmed, so the binding is retained"
        );
        server.abort();
    }

    #[tokio::test]
    async fn remove_member_removes_unbinds_and_refreshes_roster() {
        let key = RoomKey::new("remove-room");
        let (rooms, fake, server, supervisor, member) = sweep_fixture(&key, "remove-bearer").await;

        let projection = supervisor.remove_member(&key, &member).await.unwrap();
        assert!(
            projection
                .members
                .iter()
                .all(|projected| projected.member_id != member),
            "the returned projection already shows the member gone"
        );
        let calls = fake.calls.lock().await.clone();
        let removal = calls
            .iter()
            .find(|call| call.path == format!("members/{member}"))
            .expect("the DELETE dialed bedrock");
        assert_eq!(
            removal.authorization.as_deref(),
            Some("Bearer remove-bearer"),
            "removal rides the room's own credential"
        );
        assert!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent_member(&key, "sage"))
                .unwrap()
                .is_none(),
            "confirmed removal unbinds the local agent binding"
        );
        assert!(with_rooms_handle(&rooms, |s| s.room_credential(&key))
            .unwrap()
            .is_some());
        server.abort();
    }

    #[tokio::test]
    async fn remove_member_policy_403_is_forbidden_and_never_revokes_control() {
        let key = RoomKey::new("remove-denied-room");
        let (rooms, fake, server, supervisor, member) =
            sweep_fixture(&key, "remove-denied-bearer").await;
        fake.member_remove_status.store(403, Ordering::Release);

        assert_eq!(
            supervisor.remove_member(&key, &member).await.unwrap_err(),
            IntentError::Forbidden
        );
        // The register path's revoke-on-403 must not leak in here: credential
        // intact, access not Revoked, binding retained for a later retry...
        assert!(with_rooms_handle(&rooms, |s| s.room_credential(&key))
            .unwrap()
            .is_some());
        assert_ne!(
            with_rooms_handle(&rooms, |s| s.room_access(&key))
                .unwrap()
                .state,
            RoomAccessState::Revoked
        );
        assert!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent_member(&key, "sage"))
                .unwrap()
                .is_some()
        );
        // ...and the admission gate still admits: once bedrock allows the
        // removal, the same supervisor's retry goes through.
        fake.member_remove_status.store(200, Ordering::Release);
        supervisor.remove_member(&key, &member).await.unwrap();
        assert!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent_member(&key, "sage"))
                .unwrap()
                .is_none()
        );
        server.abort();
    }

    #[tokio::test]
    async fn remove_member_maps_bedrock_refusals_without_unbinding() {
        let key = RoomKey::new("remove-mapped-room");
        let (rooms, fake, server, supervisor, member) =
            sweep_fixture(&key, "remove-mapped-bearer").await;
        for (status, expected) in [
            (404u16, IntentError::NotFound),
            (409, IntentError::Conflict),
            (429, IntentError::Unavailable),
            (500, IntentError::Unavailable),
            (418, IntentError::Protocol),
        ] {
            fake.member_remove_status.store(status, Ordering::Release);
            assert_eq!(
                supervisor.remove_member(&key, &member).await.unwrap_err(),
                expected,
                "bedrock {status}"
            );
            assert!(
                with_rooms_handle(&rooms, |s| s.resolve_room_agent_member(&key, "sage"))
                    .unwrap()
                    .is_some(),
                "an unconfirmed removal must not unbind (bedrock {status})"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn remove_member_preflight_fails_closed_before_any_dial() {
        let key = RoomKey::new("remove-preflight-room");
        let (rooms, fake, server, supervisor, member) =
            sweep_fixture(&key, "remove-preflight-bearer").await;
        let dialed = fake.calls.lock().await.len();

        let missing = RoomKey::new("remove-missing-room");
        assert_eq!(
            supervisor
                .remove_member(&missing, &member)
                .await
                .unwrap_err(),
            IntentError::NotFound
        );
        let uncredentialed = RoomKey::new("remove-uncredentialed-room");
        with_rooms_handle(&rooms, |s| {
            s.create(
                uncredentialed.clone(),
                "Uncredentialed",
                None,
                chrono::Utc::now(),
            )
        })
        .unwrap();
        assert_eq!(
            supervisor
                .remove_member(&uncredentialed, &member)
                .await
                .unwrap_err(),
            IntentError::Conflict
        );
        with_rooms_handle(&rooms, |s| {
            s.update_room_access_safe(&key, Some(RoomAccessState::Revoked), None, None)
        })
        .unwrap();
        assert_eq!(
            supervisor.remove_member(&key, &member).await.unwrap_err(),
            IntentError::Forbidden
        );
        assert_eq!(
            fake.calls.lock().await.len(),
            dialed,
            "no preflight failure may reach the network"
        );
        server.abort();
    }

    #[tokio::test]
    async fn p2c_agent_mixed_retry_heals_and_binding_mismatch_fails_closed() {
        let key = RoomKey::new("agent-retry-room");
        let human = "22222222-2222-4222-8222-222222222222";
        let first_member = "33333333-3333-4333-8333-000000000000";
        let second_member = "33333333-3333-4333-8333-000000000001";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Agent Retry", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "agent-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        store
            .bind_room_agent(&key, first_member, "sage", "reg-sage")
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new(key.as_str());
        fake.agents_status.store(200, Ordering::Release);
        let (base, server) = start_control_bedrock(fake).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        let projection = supervisor
            .register_agents(
                &key,
                vec![
                    p2c_agent_input("sage", "reg-sage"),
                    p2c_agent_input("scout", "reg-scout"),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent(&key, first_member))
                .unwrap()
                .as_deref(),
            Some("sage")
        );
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent(&key, second_member))
                .unwrap()
                .as_deref(),
            Some("scout")
        );
        assert_eq!(
            projection
                .members
                .iter()
                .filter(|member| member.local_binding_available == Some(true))
                .count(),
            2
        );

        assert_eq!(
            supervisor
                .register_agents(&key, vec![p2c_agent_input("intruder", "reg-intruder")])
                .await,
            Err(IntentError::Store)
        );
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.resolve_room_agent(&key, first_member))
                .unwrap()
                .as_deref(),
            Some("sage"),
            "conflicting replay must not replace the private binding"
        );
        server.abort();
    }

    #[tokio::test]
    async fn p2c_startup_recovery_never_exceeds_four_in_flight() {
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        for index in 0..5 {
            store
                .get_or_insert_pending_redemption(
                    &format!("limited-code-{index}"),
                    &format!("{index:08x}-dddd-4ddd-8ddd-{index:012x}"),
                    &format!("{index:043}"),
                    chrono::Utc::now(),
                )
                .unwrap();
        }
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new("unused-room");
        fake.redeem_status.store(403, Ordering::Release);
        fake.hold_redeem.store(true, Ordering::Release);
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while fake.redeem_active.load(Ordering::Acquire) != 4 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("first four recovery calls admitted");
        assert_eq!(fake.redeem_peak.load(Ordering::Acquire), 4);
        assert_eq!(
            fake.calls
                .lock()
                .await
                .iter()
                .filter(|call| call.path == "redeem")
                .count(),
            4,
            "fifth row must wait for a recovery permit"
        );
        fake.release_redeem.add_permits(4);
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let calls = fake
                    .calls
                    .lock()
                    .await
                    .iter()
                    .filter(|call| call.path == "redeem")
                    .count();
                if calls == 5 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fifth row admitted after a permit releases");
        fake.release_redeem.add_permits(1);
        tokio::time::timeout(Duration::from_secs(60), async {
            while !with_rooms_handle(&rooms, |s| s.list_pending_redemptions())
                .unwrap()
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("all limited rows finish");
        assert_eq!(fake.redeem_peak.load(Ordering::Acquire), 4);
        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn p2c_shutdown_cancels_and_joins_recovery_before_starting_more_calls() {
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        for index in 0..5 {
            store
                .get_or_insert_pending_redemption(
                    &format!("shutdown-code-{index}"),
                    &format!("{index:08x}-eeee-4eee-8eee-{index:012x}"),
                    &format!("{index:043}"),
                    chrono::Utc::now(),
                )
                .unwrap();
        }
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new("unused-room");
        fake.hold_redeem.store(true, Ordering::Release);
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms);

        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while fake.redeem_active.load(Ordering::Acquire) != 4 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("four recovery requests started");
        tokio::time::timeout(Duration::from_secs(60), supervisor.shutdown())
            .await
            .expect("shutdown cancels and joins recovery requests");
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert_eq!(
            fake.calls
                .lock()
                .await
                .iter()
                .filter(|call| call.path == "redeem")
                .count(),
            4,
            "shutdown must prevent the waiting fifth row from starting"
        );
        fake.release_redeem.add_permits(4);
        server.abort();
    }

    #[tokio::test]
    async fn p2c_startup_attempts_every_pending_row_beyond_128() {
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        for index in 0..129 {
            store
                .get_or_insert_pending_redemption(
                    &format!("code-{index}"),
                    &format!("{index:08x}-aaaa-4aaa-8aaa-{index:012x}"),
                    &format!("{index:043}"),
                    chrono::Utc::now(),
                )
                .unwrap();
        }
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = ControlBedrock::new("unused-room");
        fake.redeem_status.store(403, Ordering::Release);
        let (base, server) = start_control_bedrock(fake.clone()).await;
        let supervisor = test_control_supervisor(&base, rooms.clone());

        supervisor.startup().await;
        // 60s, not 5s: 129 sequential redeem round-trips finish in well under a
        // second solo, but a saturated CI runner sharing 500+ parallel tests
        // starves this task — the tight deadline failed #331's ubuntu row on a
        // diff touching only ocean-runtime (TASK-27). The loop is
        // progress-guaranteed, so a generous deadline costs nothing when
        // healthy and only trades flake for latency under pathological load.
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let attempts = fake
                    .calls
                    .lock()
                    .await
                    .iter()
                    .filter(|call| call.path == "redeem")
                    .count();
                // `>= 129`, not `== 129` (TASK-33): a 403-failed redeem can be
                // re-attempted, so the count can climb past 129 — and even
                // absent retries, the exact-129 moment is a transient the 10ms
                // poll can skip under load, hanging until the 60s deadline. The
                // completeness invariant is "every one of the 129 rows was
                // attempted"; the pending-list-empty assertion AFTER this loop
                // is the real correctness check (it catches any row skipped).
                if attempts >= 129 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("all pending rows attempted once");
        // The attempt counter increments when the fake RECEIVES a call, but a
        // row leaves the pending store only after its 403 response is
        // processed — an instantaneous emptiness assert races that final
        // write under load (this fired on #339's ubuntu row). Poll the
        // observable end state under the same generous deadline.
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let pending = with_rooms_handle(&rooms, |s| s.list_pending_redemptions())
                    .unwrap()
                    .len();
                if pending == 0 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("every attempted row must drain from the pending store");
        supervisor.shutdown().await;
        server.abort();
    }

    fn test_supervisor_inner(rooms: RoomStoreHandle) -> Arc<SupervisorInner> {
        let (trigger_tx, _) = mpsc::unbounded_channel();
        test_supervisor_inner_with_trigger(rooms, trigger_tx)
    }

    fn test_supervisor_inner_with_trigger(
        rooms: RoomStoreHandle,
        trigger_tx: mpsc::UnboundedSender<FederatedTriggerDispatch>,
    ) -> Arc<SupervisorInner> {
        Arc::new(SupervisorInner {
            client: None,
            owner_token: None,
            invalid_config: false,
            rooms,
            room_wakes: RoomWakeBus::default(),
            access_wakes: RoomAccessWakeBus::default(),
            read_cursor_wakes: RoomReadCursorWakeBus::default(),
            trigger_tx,
            shutdown: CancellationToken::new(),
            slots: Mutex::new(HashMap::new()),
            recovery: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
            next_generation: AtomicU64::new(0),
            scan_interval: Duration::from_millis(20),
        })
    }

    async fn run_control_recovery(event_name: &str, data: Value) {
        let key = RoomKey::new(format!("control-{event_name}"));
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Control", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "control-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), None, Some(5))
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "control-bearer");
        *fake.members.lock().await = json!({"members":[{
            "member_id":human,
            "actor_type":"user",
            "role_in_room":"owner",
            "display_name":"Human",
            "joined_at":"2026-07-17T00:00:00Z"
        }]});
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let connected = fake.sse_tx.lock().await.is_some();
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                if connected && projection.state == RoomAccessState::Live {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("healthy lease established");
        fake.events_status.store(500, Ordering::Release);
        fake.sse_tx
            .lock()
            .await
            .clone()
            .unwrap()
            .send(Ok(Event::default()
                .event(event_name)
                .id("5")
                .data(data.to_string())))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                if projection.state == RoomAccessState::Recovering
                    && projection.last_confirmed_global_sequence == Some(5)
                    && projection.members.iter().all(|m| match m.actor_type {
                        FederatedActorType::User => {
                            m.derived_presence == Some(MemberPresence::Unavailable)
                        }
                        FederatedActorType::Agent => m.derived_presence.is_none(),
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("control divergence recovered from durable cursor");
        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn mismatched_and_unknown_control_frames_fail_closed() {
        run_control_recovery("heartbeat", json!({"sequence":"6"})).await;
        run_control_recovery("heartbeat", json!({"sequence":"4"})).await;
        run_control_recovery("resync_required", json!({"after_sequence":"4"})).await;
        run_control_recovery("future_control", json!({})).await;
        run_control_recovery(
            "room_event",
            json!({
                "id":"bad-id",
                "sequence":"6",
                "event_type":"message",
                "correlation_id":"control-room_event",
                "virtual_path":"/rooms/control-room_event",
                "actor_id":"principal",
                "actor_member_id":"11111111-1111-4111-8111-111111111111",
                "source_id":"room:control-room_event:member:m:producer:p",
                "source_sequence":"1",
                "payload":{
                    "client_event_id":"bad-event-id",
                    "author_member_id":"11111111-1111-4111-8111-111111111111",
                    "body":"must not commit"
                }
            }),
        )
        .await;
    }

    #[tokio::test]
    async fn room_presence_event_ignores_mixed_actor_types_and_preserves_cursor() {
        let key = RoomKey::new("presence-canonical");
        let human = "11111111-1111-4111-8111-111111111111";
        let remote_human = "22222222-2222-4222-8222-222222222222";
        let agent = "33333333-3333-4333-8333-333333333333";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Presence", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "presence-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[
                    FederatedRoomMemberProjection {
                        member_id: human.into(),
                        owner_member_id: None,
                        actor_type: FederatedActorType::User,
                        role_in_room: FederatedRoomRole::Owner,
                        display_name: "Human".into(),
                        public_agent_descriptor: None,
                        joined_at: "2026-07-17T00:00:00Z".into(),
                        derived_presence: Some(MemberPresence::Unavailable),
                        local_binding_available: None,
                    },
                    FederatedRoomMemberProjection {
                        member_id: remote_human.into(),
                        owner_member_id: None,
                        actor_type: FederatedActorType::User,
                        role_in_room: FederatedRoomRole::Member,
                        display_name: "Remote".into(),
                        public_agent_descriptor: None,
                        joined_at: "2026-07-17T00:00:01Z".into(),
                        derived_presence: Some(MemberPresence::Unavailable),
                        local_binding_available: None,
                    },
                    FederatedRoomMemberProjection {
                        member_id: agent.into(),
                        owner_member_id: Some(human.into()),
                        actor_type: FederatedActorType::Agent,
                        role_in_room: FederatedRoomRole::Member,
                        display_name: "Agent".into(),
                        public_agent_descriptor: Some(PublicAgentDescriptor {
                            display_name: "Agent".into(),
                            description: None,
                            model_alias: None,
                            skills_count: 0,
                            subagent_names: vec![],
                        }),
                        joined_at: "2026-07-17T00:00:02Z".into(),
                        derived_presence: None,
                        local_binding_available: Some(true),
                    },
                ]),
                Some(7),
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let mut live_human_member_ids: HashSet<String> = HashSet::new();

        assert!(apply_presence_frame(
            &test_supervisor_inner(rooms.clone()),
            &key,
            RoomAccessState::Live,
            &[PresenceWireMember {
                member_id: human.into(),
                actor_type: FederatedActorType::User,
                role_in_room: FederatedRoomRole::Owner,
                display_name: "Human".into(),
                joined_at: "2026-07-17T00:00:00Z".into(),
            }],
            &mut live_human_member_ids,
        ));
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(projection.last_confirmed_global_sequence, Some(7));
        assert_eq!(
            projection.members[0].derived_presence,
            Some(MemberPresence::Live)
        );
        assert_eq!(
            projection.members[1].derived_presence,
            Some(MemberPresence::Unavailable)
        );
        assert_eq!(projection.members[2].derived_presence, None);
        assert_eq!(
            live_human_member_ids,
            HashSet::from([human.to_string()]),
            "live-humans cache reflects the committed frame"
        );

        // M4: a mixed frame that additively echoes a non-User (agent) entry
        // alongside the live human must not be treated as a protocol
        // violation that tears down the epoch — the agent entry carries no
        // presence meaning here and is ignored, while the human's live
        // status is still applied.
        assert!(apply_presence_frame(
            &test_supervisor_inner(rooms.clone()),
            &key,
            RoomAccessState::Live,
            &[
                PresenceWireMember {
                    member_id: human.into(),
                    actor_type: FederatedActorType::User,
                    role_in_room: FederatedRoomRole::Owner,
                    display_name: "Human".into(),
                    joined_at: "2026-07-17T00:00:00Z".into(),
                },
                PresenceWireMember {
                    member_id: agent.into(),
                    actor_type: FederatedActorType::Agent,
                    role_in_room: FederatedRoomRole::Member,
                    display_name: "Agent".into(),
                    joined_at: "2026-07-17T00:00:02Z".into(),
                },
            ],
            &mut live_human_member_ids,
        ));
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(
            projection.members[0].derived_presence,
            Some(MemberPresence::Live)
        );
        assert_eq!(
            projection.members[1].derived_presence,
            Some(MemberPresence::Unavailable)
        );
        assert_eq!(projection.members[2].derived_presence, None);
        assert_eq!(live_human_member_ids, HashSet::from([human.to_string()]));

        // M2: a malformed frame (an empty `member_id` on a User entry can't
        // be matched against the roster) is rejected WITHOUT mutating
        // either the durable projection or the epoch-local live-humans
        // cache.
        let snapshot = projection.clone();
        let cache_before = live_human_member_ids.clone();
        assert!(!apply_presence_frame(
            &test_supervisor_inner(rooms.clone()),
            &key,
            RoomAccessState::Live,
            &[PresenceWireMember {
                member_id: String::new(),
                actor_type: FederatedActorType::User,
                role_in_room: FederatedRoomRole::Member,
                display_name: "Remote".into(),
                joined_at: "2026-07-17T00:00:01Z".into(),
            }],
            &mut live_human_member_ids,
        ));
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap(),
            snapshot
        );
        assert_eq!(live_human_member_ids, cache_before);
    }

    /// M2 regression: when the durable commit fails (here, an unknown
    /// room), `apply_presence_frame` must return `false` without touching
    /// the caller's `live_human_member_ids` cache at all. A prior
    /// implementation cleared and repopulated that cache from the incoming
    /// frame BEFORE the store commit was confirmed, so a failed write
    /// silently wiped the in-memory presence view out from under the next
    /// roster fetch even though nothing durable had changed.
    #[tokio::test]
    async fn apply_presence_frame_does_not_wipe_live_cache_when_commit_fails() {
        let store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let key = RoomKey::new("presence-unknown-room");
        let mut live_human_member_ids: HashSet<String> = HashSet::from(["stale-human".to_string()]);

        assert!(!apply_presence_frame(
            &test_supervisor_inner(rooms.clone()),
            &key,
            RoomAccessState::Live,
            &[PresenceWireMember {
                member_id: "11111111-1111-4111-8111-111111111111".into(),
                actor_type: FederatedActorType::User,
                role_in_room: FederatedRoomRole::Owner,
                display_name: "Human".into(),
                joined_at: "2026-07-17T00:00:00Z".into(),
            }],
            &mut live_human_member_ids,
        ));
        assert_eq!(
            live_human_member_ids,
            HashSet::from(["stale-human".to_string()])
        );
    }

    #[tokio::test]
    async fn valid_but_unreachable_startup_clears_stale_live_presence() {
        let key = RoomKey::new("fed-hanging-connect");
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Hanging", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "hanging-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[FederatedRoomMemberProjection {
                    member_id: human.into(),
                    owner_member_id: None,
                    actor_type: FederatedActorType::User,
                    role_in_room: FederatedRoomRole::Owner,
                    display_name: "Human".into(),
                    public_agent_descriptor: None,
                    joined_at: "2026-07-17T00:00:00Z".into(),
                    derived_presence: Some(MemberPresence::Live),
                    local_binding_available: None,
                }]),
                Some(4),
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let access_wakes = RoomAccessWakeBus::default();
        let mut access_rx = access_wakes.test_subscribe();
        let fake = FakeBedrock::new(key.as_str(), "hanging-bearer");
        fake.hold_events_response.store(true, Ordering::Release);
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            access_wakes,
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(projection.state, RoomAccessState::Connecting);
        assert_eq!(projection.last_confirmed_global_sequence, Some(4));
        assert!(projection
            .members
            .iter()
            .all(|m| m.derived_presence == Some(MemberPresence::Unavailable)));
        tokio::time::timeout(Duration::from_secs(60), access_rx.recv())
            .await
            .expect("atomic startup access wake")
            .expect("access bus open");
        assert!(access_rx.try_recv().is_err());
        supervisor.shutdown().await;
        fake.release_events_response.notify_waiters();
        server.abort();
    }

    #[tokio::test]
    async fn poison_ledger_row_is_stepped_over_instead_of_wedging_the_room() {
        // The failure this pins: a row the daemon cannot ingest used to force
        // `Recover`, which reconnects from the durable cursor — still sitting
        // BEFORE that row. The same row is served again, fails again, forever.
        // Every later message in the room stops arriving, for every daemon
        // federated to it, and no restart clears it.
        let key = RoomKey::new("fed-poison");
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Poison", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "poison-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "poison-bearer");
        *fake.members.lock().await = json!({
            "members": [{
                "member_id": human,
                "actor_type": "user",
                "role_in_room": "owner",
                "display_name": "Local Human",
                "joined_at": "2026-07-17T00:00:00Z"
            }]
        });
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if fake.sse_tx.lock().await.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("SSE connected");
        let tx = fake.sse_tx.lock().await.clone().expect("SSE connected");

        let good_row = |sequence: &str, client_event_id: &str| {
            json!({
                "id": format!("ledger-{sequence}"),
                "sequence": sequence,
                "event_type": "message",
                "correlation_id": "fed-poison",
                "virtual_path": "/rooms/fed-poison",
                "actor_id": "principal-1",
                "actor_member_id": human,
                "source_id": "source-1",
                "source_sequence": sequence,
                "payload": {
                    "client_event_id": client_event_id,
                    "author_member_id": human,
                    "body": "readable",
                    "mention_member_ids": []
                }
            })
        };

        // Poison 1: too large to parse at all. Only the SSE id is legible, and
        // that is exactly what the skip path leans on.
        let oversized = json!({
            "id": "ledger-1",
            "sequence": "1",
            "event_type": "message",
            "correlation_id": "fed-poison",
            "virtual_path": "/rooms/fed-poison",
            "actor_id": "principal-1",
            "actor_member_id": human,
            "source_id": "source-1",
            "source_sequence": "1",
            "payload": {
                "client_event_id": "client-1",
                "author_member_id": human,
                "body": "x".repeat(BODY_LIMIT + 4096),
                "mention_member_ids": []
            }
        });
        assert!(oversized.to_string().len() > BODY_LIMIT);
        tx.send(Ok(Event::default()
            .event("room_event")
            .id("1")
            .data(oversized.to_string())))
            .await
            .unwrap();

        // Poison 2: small and well-formed JSON, but the payload contradicts the
        // envelope, so ingest refuses it. Same wedge, different door.
        let mut forged = good_row("2", "client-2");
        forged["payload"]["author_member_id"] = json!("99999999-9999-4999-8999-999999999999");
        tx.send(Ok(Event::default()
            .event("room_event")
            .id("2")
            .data(forged.to_string())))
            .await
            .unwrap();

        // The message that used to be unreachable behind them.
        tx.send(Ok(Event::default()
            .event("room_event")
            .id("3")
            .data(good_row("3", "client-3").to_string())))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let done = with_rooms_handle(&rooms, |s| {
                    let transcript = s.get(&key).unwrap().unwrap().transcript;
                    let access = s.room_access(&key).unwrap();
                    transcript.len() == 1 && access.last_confirmed_global_sequence == Some(3)
                });
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("poison rows skipped and the later message ingested");

        let transcript = with_rooms_handle(&rooms, |s| s.get(&key))
            .unwrap()
            .unwrap()
            .transcript;
        assert_eq!(transcript.len(), 1, "only the readable row lands");
        assert_eq!(transcript[0].body, "readable");
        supervisor.shutdown().await;
        server.abort();
    }

    /// Every `room.workspace.*` event type ocean-bedrock actually emits.
    ///
    /// Pinned against ocean-bedrock `origin/master` @ 9efef97, checked
    /// 2026-08-30. The rule that decides membership, and the only one: a
    /// string belongs here if and only if it is the `action` of an
    /// `emitWorkspaceEvent` call in ocean-bedrock `src/server.mjs`. That
    /// helper is what stamps `correlation_id` and the room-scoped `path` onto
    /// the audit row, and the SSE stream this daemon ingests is that ledger
    /// filtered on exactly those two fields — so a `room.workspace.*` string
    /// reaching `appendAudit` by any other route cannot arrive on this rail,
    /// however much it reads like a room event.
    ///
    /// `build_finished` / `build_failed` are the two arms of one call site's
    /// ternary rather than two literals; a grep for the names misses them.
    const BEDROCK_ROOM_WORKSPACE_EVENTS: [&str; 22] = [
        "room.workspace.provisioned",
        "room.workspace.destroyed",
        "room.workspace.repo_bound",
        "room.workspace.repo_clone_started",
        "room.workspace.repo_cloned",
        "room.workspace.repo_clone_failed",
        "room.workspace.repo_unbound",
        "room.workspace.build_started",
        "room.workspace.build_finished",
        "room.workspace.build_failed",
        "room.workspace.ci_checked",
        "room.workspace.exec_started",
        "room.workspace.exec_finished",
        "room.workspace.exec_failed",
        "room.workspace.execs_purged",
        "room.workspace.file_written",
        "room.workspace.file_deleted",
        "room.workspace.flushed",
        "room.workspace.hydrated",
        "room.workspace.port_exposed",
        "room.workspace.port_closed",
        "room.workspace.secrets_updated",
    ];

    #[test]
    fn workspace_marker_allowlist_classifies_every_bedrock_event() {
        // The OUTCOME rows that become transcript markers.
        const ADMITTED: [&str; 13] = [
            "room.workspace.provisioned",
            "room.workspace.destroyed",
            "room.workspace.repo_cloned",
            "room.workspace.repo_clone_failed",
            "room.workspace.repo_unbound",
            "room.workspace.build_finished",
            "room.workspace.build_failed",
            "room.workspace.port_exposed",
            "room.workspace.port_closed",
            "room.workspace.flushed",
            "room.workspace.hydrated",
            "room.workspace.ci_checked",
            "room.workspace.execs_purged",
        ];
        // Rows Bedrock really emits that deliberately advance only the cursor.
        const DELIBERATE_NOISE: [&str; 9] = [
            "room.workspace.repo_bound",
            "room.workspace.repo_clone_started",
            "room.workspace.build_started",
            "room.workspace.exec_started",
            "room.workspace.exec_finished",
            "room.workspace.exec_failed",
            "room.workspace.file_written",
            "room.workspace.file_deleted",
            "room.workspace.secrets_updated",
        ];

        for event in BEDROCK_ROOM_WORKSPACE_EVENTS {
            let admitted = ADMITTED.contains(&event);
            let noise = DELIBERATE_NOISE.contains(&event);
            assert!(
                admitted != noise,
                "{event} must be classified exactly once — as a marker or as \
                 deliberate noise, never both and never neither"
            );
            assert_eq!(
                workspace_action_is_marker(event),
                admitted,
                "{event} is listed one way and matched the other"
            );
        }

        // The other direction, so neither partition may name a string Bedrock
        // does not emit: that is what caught `mkdir`, which sat in the old
        // noise list for as long as it existed without ever being emitted
        // anywhere in ocean-bedrock.
        for event in ADMITTED.iter().chain(DELIBERATE_NOISE.iter()) {
            assert!(
                BEDROCK_ROOM_WORKSPACE_EVENTS.contains(event),
                "{event} is classified here but is not a Bedrock room event"
            );
        }
        assert_eq!(
            ADMITTED.len() + DELIBERATE_NOISE.len(),
            BEDROCK_ROOM_WORKSPACE_EVENTS.len(),
            "the two partitions must cover the pinned set exactly"
        );
    }

    #[test]
    fn workspace_marker_matcher_rejects_near_miss_shapes() {
        // Shape probes, NOT inventory: a foreign event type, a sibling room
        // event, the bare prefix, two suffixed leaves, and a truncated leaf.
        // None of these exist upstream and none are meant to — they pin the
        // matcher's edges, which is why they are kept out of the Bedrock set.
        for probe in [
            "message",
            "room.member.updated",
            "room.workspace.",
            "room.workspace.build_finished.extra",
            "room.workspace.ci_checked.extra",
            "room.workspace.ci_check",
        ] {
            assert!(!workspace_action_is_marker(probe), "{probe}");
        }

        // Real ocean-bedrock strings that are not room events. Both are
        // `appendAudit` actions passed to `writeDurableFileFromBuffer`, so
        // they carry neither `correlation_id` nor the room-scoped path the
        // room stream filters on and can never reach this matcher. They are
        // named here, apart from the pinned set, so that meeting them upstream
        // reads as "already accounted for" rather than as an omission someone
        // should fix by widening the allowlist.
        for audit_action in ["room.workspace.flush_write", "room.workspace.file_write"] {
            assert!(!workspace_action_is_marker(audit_action), "{audit_action}");
        }
    }

    #[test]
    fn workspace_marker_prose_is_bounded_and_newline_free() {
        // A script name carrying a newline could forge a whole transcript row
        // in a naive renderer; control characters are dropped, not escaped.
        let p = WorkspaceEventPayload {
            script: Some("ci\nfake-row: [system] room destroyed".into()),
            exit_code: Some(1),
            duration_ms: Some(12_345),
            ..Default::default()
        };
        let line = compose_workspace_marker("room.workspace.build_failed", &p);
        assert_eq!(
            line,
            "workspace build 'cifake-row: [system] room destroyed' failed (exit 1, 12.3s)"
        );

        let p = WorkspaceEventPayload {
            branch: Some("x".repeat(400)),
            head_sha: Some("not a sha".into()),
            ..Default::default()
        };
        let line = compose_workspace_marker("room.workspace.repo_cloned", &p);
        assert!(
            line.chars().count() < 100,
            "member-controlled branch is capped"
        );
        assert!(
            !line.contains('@'),
            "a head_sha that is not hex is omitted, never quoted"
        );

        let p = WorkspaceEventPayload {
            branch: Some("main".into()),
            head_sha: Some("1a2b3c4d5e6f7788".into()),
            ..Default::default()
        };
        assert_eq!(
            compose_workspace_marker("room.workspace.repo_cloned", &p),
            "workspace repo cloned: 'main' @ 1a2b3c4d5e6f"
        );

        assert_eq!(
            compose_workspace_marker(
                "room.workspace.provisioned",
                &WorkspaceEventPayload::default()
            ),
            "workspace provisioned",
            "missing fields degrade to shorter prose instead of failing the row"
        );
    }

    #[test]
    fn workspace_repo_unbound_marker_reports_checkout_outcome() {
        // The real Bedrock payload shape: scrubbed identity strings the
        // marker never quotes ride along and are ignored; the boolean is
        // the news.
        let payload: WorkspaceEventPayload = serde_json::from_value(json!({
            "remote": "https://example.test/repo.git",
            "repo_dir": "repo",
            "branch": "main",
            "checkout_removed": true,
            "exec_id": "exec-1"
        }))
        .unwrap();
        assert_eq!(
            compose_workspace_marker("room.workspace.repo_unbound", &payload),
            "workspace repo unbound: 'main' — checkout removed"
        );

        // rm_failed leaves a live checkout the next flush re-ingests as
        // room files — the one unbind outcome a member must actually see.
        let payload: WorkspaceEventPayload = serde_json::from_value(json!({
            "branch": "main",
            "checkout_removed": false,
            "checkout_removed_reason": "rm_failed"
        }))
        .unwrap();
        assert_eq!(
            compose_workspace_marker("room.workspace.repo_unbound", &payload),
            "workspace repo unbound: 'main' — checkout removal failed"
        );

        // no_container is not a failure: there was nothing to remove, and
        // claiming either removal or trouble would lie about state.
        let payload: WorkspaceEventPayload = serde_json::from_value(json!({
            "branch": "main",
            "checkout_removed": false,
            "checkout_removed_reason": "no_container"
        }))
        .unwrap();
        assert_eq!(
            compose_workspace_marker("room.workspace.repo_unbound", &payload),
            "workspace repo unbound: 'main'"
        );

        assert_eq!(
            compose_workspace_marker(
                "room.workspace.repo_unbound",
                &WorkspaceEventPayload::default()
            ),
            "workspace repo unbound",
            "missing fields degrade to shorter prose instead of failing the row"
        );
    }

    #[test]
    fn workspace_port_markers_pair_an_exposure_with_its_retraction() {
        // The real Bedrock payload shape: `preview_url` rides the ledger row
        // for an exposure, but this struct does not deserialize it, so the
        // transcript names the port and never the link — the port integer is
        // the whole of what a reader has to match the pair by.
        let payload: WorkspaceEventPayload = serde_json::from_value(json!({
            "port": 8787,
            "preview_url": "https://8787-room.example.dev",
            "exec_id": "exec-1"
        }))
        .unwrap();
        assert_eq!(
            compose_workspace_marker("room.workspace.port_exposed", &payload),
            "workspace port 8787 exposed"
        );
        assert_eq!(
            compose_workspace_marker("room.workspace.port_closed", &payload),
            "workspace port 8787 closed"
        );

        assert_eq!(
            compose_workspace_marker(
                "room.workspace.port_closed",
                &WorkspaceEventPayload::default()
            ),
            "workspace port closed",
            "missing fields degrade to shorter prose instead of failing the row"
        );
    }

    #[test]
    fn workspace_execs_purged_marker_distinguishes_scope() {
        // The real Bedrock payload shape for a purge-all: 'all' is a
        // sentinel, not an exec name, and must not render as one.
        let payload: WorkspaceEventPayload = serde_json::from_value(json!({
            "exec_id": "all",
            "purged_rows": 7
        }))
        .unwrap();
        assert_eq!(
            compose_workspace_marker("room.workspace.execs_purged", &payload),
            "workspace exec output purged (7 rows)"
        );

        // A single-exec purge names its row so a reader can tie the
        // take-back to the output that vanished.
        let payload: WorkspaceEventPayload = serde_json::from_value(json!({
            "exec_id": "1f9d1c9e-1111-4222-8333-444455556666",
            "purged_rows": 1
        }))
        .unwrap();
        assert_eq!(
            compose_workspace_marker("room.workspace.execs_purged", &payload),
            "workspace exec '1f9d1c9e-1111-4222-8333-444455556666' output purged (1 row)"
        );

        // Bedrock validates exec ids upstream, but the bounded filter — not
        // Bedrock — is what this lane trusts.
        let payload: WorkspaceEventPayload = serde_json::from_value(json!({
            "exec_id": "abc\ndef",
            "purged_rows": 2
        }))
        .unwrap();
        assert_eq!(
            compose_workspace_marker("room.workspace.execs_purged", &payload),
            "workspace exec 'abcdef' output purged (2 rows)"
        );

        assert_eq!(
            compose_workspace_marker(
                "room.workspace.execs_purged",
                &WorkspaceEventPayload::default()
            ),
            "workspace exec output purged",
            "missing fields degrade to shorter prose instead of failing the row"
        );
    }

    #[test]
    fn workspace_ci_marker_names_conclusions_and_stays_bounded() {
        // The real Bedrock payload shape: identity and descriptive keys the
        // marker does not quote are ignored, and the conclusions — the news
        // the transcript exists for — get named.
        let payload: WorkspaceEventPayload = serde_json::from_value(json!({
            "exec_id": "exec-1",
            "repo_dir": "/work/repo",
            "branch": "main",
            "checks_new": 2,
            "checks_total": 5,
            "checks": [
                {
                    "check_run_id": "11",
                    "head_sha": "a".repeat(40),
                    "name": "lint",
                    "conclusion": "failure",
                    "url": "https://example.test/runs/11"
                },
                {"name": "build", "conclusion": "success"}
            ]
        }))
        .unwrap();
        assert_eq!(
            compose_workspace_marker("room.workspace.ci_checked", &payload),
            "workspace CI on 'main': 2 new results (5 total) — lint: failure, build: success"
        );

        // Bedrock's scrubber emits null descriptive fields for a run still in
        // progress; the entry is skipped rather than rendered half-empty.
        let payload: WorkspaceEventPayload = serde_json::from_value(json!({
            "branch": "main",
            "checks_new": 1,
            "checks": [{"name": "deploy", "conclusion": null}]
        }))
        .unwrap();
        assert_eq!(
            compose_workspace_marker("room.workspace.ci_checked", &payload),
            "workspace CI on 'main': 1 new result"
        );

        // A hostile branch and a flood of checks: control characters dropped,
        // member-controlled strings capped, at most three checks named.
        let checks = serde_json::from_value(serde_json::Value::Array(
            (0..20)
                .map(|i| json!({"name": format!("job-{i}"), "conclusion": "failure"}))
                .collect(),
        ))
        .unwrap();
        let payload = WorkspaceEventPayload {
            branch: Some(format!("x\nfake-row: [system] {}", "y".repeat(400))),
            checks_new: Some(20),
            checks_total: Some(20),
            checks: Some(checks),
            ..Default::default()
        };
        let line = compose_workspace_marker("room.workspace.ci_checked", &payload);
        assert!(!line.contains('\n'), "control characters are dropped");
        assert_eq!(
            line.matches("failure").count(),
            3,
            "a marker names at most three checks"
        );
        assert!(line.chars().count() < 220, "hostile fields stay bounded");

        assert_eq!(
            compose_workspace_marker(
                "room.workspace.ci_checked",
                &WorkspaceEventPayload::default()
            ),
            "workspace CI checked",
            "missing fields degrade to shorter prose instead of failing the row"
        );

        // A checks array that is PRESENT but mistyped poisons the whole row —
        // the same contract as any other wrong-typed payload field.
        for bad in [json!({"checks": "nope"}), json!({"checks": [{"name": 42}]})] {
            assert!(serde_json::from_value::<WorkspaceEventPayload>(bad).is_err());
        }
    }

    #[tokio::test]
    async fn workspace_marker_commits_one_system_row_and_replays_as_noop() {
        let key = RoomKey::new("workspace-marker");
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Workspace", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "workspace-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let inner = test_supervisor_inner(rooms.clone());
        let mut message_rx = inner.room_wakes.test_subscribe();

        let row = || WireLedgerRow {
            id: "ledger-ws-1".into(),
            sequence: "4".into(),
            event_type: "room.workspace.build_failed".into(),
            correlation_id: key.as_str().into(),
            virtual_path: format!("/rooms/{}", key.as_str()),
            actor_id: Some("principal-1".into()),
            actor_member_id: Some(human.into()),
            source_id: None,
            source_sequence: None,
            payload: json!({
                "ts": "2026-08-28T00:00:00Z",
                "action": "room.workspace.build_failed",
                "script": "ci",
                "outcome": "failed",
                "exit_code": 2,
                "duration_ms": 1500
            }),
        };

        let outcome = ingest_workspace_row(&inner, &key, row()).unwrap();
        assert_eq!(outcome, IngestDisposition::Committed);
        let transcript = with_rooms_handle(&rooms, |s| s.get(&key))
            .unwrap()
            .unwrap()
            .transcript;
        assert_eq!(transcript.len(), 1, "exactly one System row per marker");
        let marker = &transcript[0];
        assert_eq!(marker.kind, RoomMessageKind::System);
        assert_eq!(marker.author_kind, RoomParticipantKind::System);
        assert_eq!(marker.author_id, "system");
        assert_eq!(marker.body, "workspace build 'ci' failed (exit 2, 1.5s)");
        let meta = marker
            .federated
            .as_ref()
            .expect("marker keeps its real ledger identity");
        assert_eq!(meta.ledger_event_id, "ledger-ws-1");
        assert_eq!(meta.global_sequence, 4);
        assert_eq!(meta.source_id, WORKSPACE_MARKER_SOURCE_ID);
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.room_access(&key))
                .unwrap()
                .last_confirmed_global_sequence,
            Some(4),
            "marker ingest advances the durable cursor like any confirmed row"
        );
        message_rx
            .try_recv()
            .expect("marker commit wakes open transcripts");

        // SSE replay of the same ledger row rebuilds identical synthesized
        // meta and must be a total no-op: no row, no cursor move, no wake.
        let outcome = ingest_workspace_row(&inner, &key, row()).unwrap();
        assert_eq!(outcome, IngestDisposition::Duplicate);
        assert!(message_rx.try_recv().is_err(), "replay emits no wake");
        let transcript = with_rooms_handle(&rooms, |s| s.get(&key))
            .unwrap()
            .unwrap()
            .transcript;
        assert_eq!(transcript.len(), 1);

        // A payload whose fields carry the wrong types is poison for THIS row
        // only: Protocol, which the receive loop steps past — never a
        // reconnect loop.
        let mut bad = row();
        bad.id = "ledger-ws-2".into();
        bad.sequence = "5".into();
        bad.payload = json!({"exit_code": "not-a-number"});
        assert!(matches!(
            ingest_workspace_row(&inner, &key, bad),
            Err(BridgeError::Protocol)
        ));
    }

    fn workspace_trigger_row(
        key: &RoomKey,
        id: &str,
        sequence: &str,
        event_type: &str,
    ) -> WireLedgerRow {
        WireLedgerRow {
            id: id.into(),
            sequence: sequence.into(),
            event_type: event_type.into(),
            correlation_id: key.as_str().into(),
            virtual_path: format!("/rooms/{}", key.as_str()),
            actor_id: Some("principal-1".into()),
            actor_member_id: None,
            source_id: None,
            source_sequence: None,
            payload: json!({"script": "ci", "exit_code": 2, "duration_ms": 1500}),
        }
    }

    #[tokio::test]
    async fn build_failed_marker_convenes_bound_agent_once_when_policy_opts_in() {
        let key = RoomKey::new("workspace-build-trigger");
        let human = "11111111-1111-4111-8111-111111111111";
        let agent = "33333333-3333-4333-8333-333333333333";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(
                key.clone(),
                "Workspace",
                Some(ocean_core::RoomTriggerPolicy {
                    on_build_failure: true,
                    ..Default::default()
                }),
                chrono::Utc::now(),
            )
            .unwrap();
        store
            .install_room_credential(&key, "workspace-bearer", human)
            .unwrap();
        store.bind_room_agent(&key, agent, "sage", "key").unwrap();
        store
            .update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[
                    p2c_projected_member(human, FederatedActorType::User, None),
                    p2c_projected_member(agent, FederatedActorType::Agent, Some(human)),
                ]),
                None,
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel();
        let inner = test_supervisor_inner_with_trigger(rooms, trigger_tx);

        // The opt-in gates the ROW KIND, not the lane: a green build and a CI
        // result stay pure markers even with on_build_failure enabled.
        for (id, sequence, event_type) in [
            ("ledger-ok", "1", "room.workspace.build_finished"),
            ("ledger-ci", "2", "room.workspace.ci_checked"),
        ] {
            let outcome = ingest_workspace_row(
                &inner,
                &key,
                workspace_trigger_row(&key, id, sequence, event_type),
            )
            .unwrap();
            assert_eq!(outcome, IngestDisposition::Committed);
            assert!(
                trigger_rx.try_recv().is_err(),
                "{event_type} must not convene"
            );
        }

        let outcome = ingest_workspace_row(
            &inner,
            &key,
            workspace_trigger_row(&key, "ledger-fail", "3", "room.workspace.build_failed"),
        )
        .unwrap();
        assert_eq!(outcome, IngestDisposition::Committed);
        let dispatch = trigger_rx
            .try_recv()
            .expect("build failure convenes the bound agent");
        assert_eq!(dispatch.target_member_id, agent);
        assert_eq!(dispatch.ledger_event_id, "ledger-fail");
        assert_eq!(dispatch.reason, "on_build_failure: workspace build failed");
        assert!(
            trigger_rx.try_recv().is_err(),
            "only the bound Agent member is dispatched — never the human"
        );

        // SSE replay of the same row: the store's consume-once claim leaves
        // nothing to dispatch on top of the usual no-row/no-cursor noop.
        let outcome = ingest_workspace_row(
            &inner,
            &key,
            workspace_trigger_row(&key, "ledger-fail", "3", "room.workspace.build_failed"),
        )
        .unwrap();
        assert_eq!(outcome, IngestDisposition::Duplicate);
        assert!(
            trigger_rx.try_recv().is_err(),
            "a replayed row must not double-convene"
        );
    }

    #[tokio::test]
    async fn build_failed_marker_convenes_nobody_without_the_opt_in() {
        // Pins today's default. Two rooms that must behave byte-identically to
        // before on_build_failure existed: no policy at all, and a policy with
        // another flag on but this one off.
        for (suffix, policy) in [
            ("absent", None),
            (
                "off",
                Some(ocean_core::RoomTriggerPolicy {
                    on_mention: true,
                    ..Default::default()
                }),
            ),
        ] {
            let key = RoomKey::new(format!("workspace-no-optin-{suffix}"));
            let human = "11111111-1111-4111-8111-111111111111";
            let agent = "33333333-3333-4333-8333-333333333333";
            let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
            store
                .create(key.clone(), "Workspace", policy, chrono::Utc::now())
                .unwrap();
            store
                .install_room_credential(&key, "workspace-bearer", human)
                .unwrap();
            store.bind_room_agent(&key, agent, "sage", "key").unwrap();
            store
                .update_room_access_safe(
                    &key,
                    Some(RoomAccessState::Live),
                    Some(&[
                        p2c_projected_member(human, FederatedActorType::User, None),
                        p2c_projected_member(agent, FederatedActorType::Agent, Some(human)),
                    ]),
                    None,
                )
                .unwrap();
            let rooms = Arc::new(std::sync::Mutex::new(store));
            let (trigger_tx, mut trigger_rx) = mpsc::unbounded_channel();
            let inner = test_supervisor_inner_with_trigger(rooms.clone(), trigger_tx);

            let outcome = ingest_workspace_row(
                &inner,
                &key,
                workspace_trigger_row(&key, "ledger-fail", "1", "room.workspace.build_failed"),
            )
            .unwrap();
            assert_eq!(outcome, IngestDisposition::Committed);
            assert!(
                trigger_rx.try_recv().is_err(),
                "policy {suffix} must convene nobody"
            );
            let transcript = with_rooms_handle(&rooms, |s| s.get(&key))
                .unwrap()
                .unwrap()
                .transcript;
            assert_eq!(transcript.len(), 1, "the marker itself still lands");
        }
    }

    #[tokio::test]
    async fn workspace_rows_reach_the_transcript_without_flooding_it() {
        // Three workspace rows through the real receive loop:
        //   1. build_started      — allowlisted OUT: cursor advances, no row.
        //   2. build_finished     — mistyped payload: poison, stepped past.
        //   3. repo_cloned        — exactly one System marker lands.
        let key = RoomKey::new("fed-workspace");
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Workspace", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "workspace-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "workspace-bearer");
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if fake.sse_tx.lock().await.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("SSE connected");
        let tx = fake.sse_tx.lock().await.clone().expect("SSE connected");

        let workspace_row = |sequence: &str, id: &str, event_type: &str, payload: Value| {
            json!({
                "id": id,
                "sequence": sequence,
                "event_type": event_type,
                "correlation_id": key.as_str(),
                "virtual_path": format!("/rooms/{}", key.as_str()),
                "actor_id": "principal-1",
                "source_id": "longhouse",
                "source_sequence": null,
                "payload": payload
            })
        };
        let rows = [
            workspace_row(
                "1",
                "ledger-1",
                "room.workspace.build_started",
                json!({"script": "ci"}),
            ),
            workspace_row(
                "2",
                "ledger-2",
                "room.workspace.build_finished",
                json!({"duration_ms": "fast"}),
            ),
            workspace_row(
                "3",
                "ledger-3",
                "room.workspace.repo_cloned",
                json!({"branch": "main", "head_sha": "1a2b3c4d5e6f7788"}),
            ),
        ];
        for (sequence, row) in rows.iter().enumerate() {
            tx.send(Ok(Event::default()
                .event("room_event")
                .id((sequence + 1).to_string())
                .data(row.to_string())))
                .await
                .unwrap();
        }

        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let done = with_rooms_handle(&rooms, |s| {
                    let transcript = s.get(&key).unwrap().unwrap().transcript;
                    let access = s.room_access(&key).unwrap();
                    transcript.len() == 1 && access.last_confirmed_global_sequence == Some(3)
                });
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("noise advanced, poison skipped, marker landed");

        let transcript = with_rooms_handle(&rooms, |s| s.get(&key))
            .unwrap()
            .unwrap()
            .transcript;
        assert_eq!(transcript.len(), 1, "only the outcome row becomes a marker");
        assert_eq!(transcript[0].kind, RoomMessageKind::System);
        assert_eq!(
            transcript[0].body,
            "workspace repo cloned: 'main' @ 1a2b3c4d5e6f"
        );
        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn oversized_incomplete_raw_sse_event_hits_cap_and_recovers() {
        let key = RoomKey::new("fed-oversized-sse");
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Oversized", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "oversize-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(
                &key,
                Some(RoomAccessState::Live),
                Some(&[FederatedRoomMemberProjection {
                    member_id: human.into(),
                    owner_member_id: None,
                    actor_type: FederatedActorType::User,
                    role_in_room: FederatedRoomRole::Owner,
                    display_name: "Human".into(),
                    public_agent_descriptor: None,
                    joined_at: "2026-07-17T00:00:00Z".into(),
                    derived_presence: Some(MemberPresence::Live),
                    local_binding_available: None,
                }]),
                Some(7),
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "oversize-bearer");
        fake.oversized_incomplete_event
            .store(true, Ordering::Release);
        let (base, server) = start_fake_bedrock(fake).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                if projection.state == RoomAccessState::Recovering
                    && projection.last_confirmed_global_sequence == Some(7)
                    && projection
                        .members
                        .iter()
                        .all(|m| m.derived_presence == Some(MemberPresence::Unavailable))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("raw event cap forced Recovering");
        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn invalid_client_config_downgrades_stale_live_rooms() {
        let key = RoomKey::new("fed-invalid-config");
        let local_human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Federated", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "never-log-me", local_human)
            .unwrap();
        let members = vec![FederatedRoomMemberProjection {
            member_id: local_human.into(),
            owner_member_id: None,
            actor_type: FederatedActorType::User,
            role_in_room: FederatedRoomRole::Owner,
            display_name: "Local Human".into(),
            public_agent_descriptor: None,
            joined_at: "2026-07-17T00:00:00Z".into(),
            derived_presence: Some(MemberPresence::Live),
            local_binding_available: None,
        }];
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Live), Some(&members), Some(9))
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let room_wakes = RoomWakeBus::default();
        let access_wakes = RoomAccessWakeBus::default();
        let mut access_rx = access_wakes.test_subscribe();
        let (trigger_tx, _) = mpsc::unbounded_channel();
        let supervisor = FederationSupervisor::new_inner(SupervisorInit {
            client: None,
            owner_token: None,
            invalid_config: true,
            rooms: rooms.clone(),
            room_wakes,
            access_wakes,
            read_cursor_wakes: RoomReadCursorWakeBus::default(),
            trigger_tx,
            shutdown: CancellationToken::new(),
            scan_interval: Duration::from_millis(20),
        });
        supervisor.startup().await;
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(projection.state, RoomAccessState::Recovering);
        assert_eq!(projection.last_confirmed_global_sequence, Some(9));
        assert!(projection
            .members
            .iter()
            .all(|m| m.derived_presence == Some(MemberPresence::Unavailable)));
        tokio::time::timeout(Duration::from_secs(60), access_rx.recv())
            .await
            .expect("state+presence access wake")
            .expect("access bus open");
        assert!(
            access_rx.try_recv().is_err(),
            "one combined transition wake"
        );
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn sender_and_receiver_use_durable_sse_rail() {
        let key = RoomKey::new("fed-e2e");
        let local_human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Federated", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "secret-bearer", local_human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        let pending = store
            .allocate_outbox_pending(
                &key,
                local_human,
                "client-1",
                "message",
                json!({"body":"hello"}),
                vec![],
            )
            .unwrap();
        store
            .bind_room_agent(
                &key,
                "22222222-2222-4222-8222-222222222222",
                "bound-agent",
                "opaque-registration-key",
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let room_wakes = RoomWakeBus::default();
        let access_wakes = RoomAccessWakeBus::default();
        let mut message_rx = room_wakes.test_subscribe();
        let mut access_rx = access_wakes.test_subscribe();
        let fake = FakeBedrock::new(key.as_str(), "secret-bearer");
        *fake.members.lock().await = json!({
            "members": [
                {
                    "member_id": local_human,
                    "actor_type": "user",
                    "role_in_room": "owner",
                    "display_name": "Local Human",
                    "joined_at": "2026-07-17T00:00:00Z"
                },
                {
                    "member_id": "22222222-2222-4222-8222-222222222222",
                    "owner_member_id": local_human,
                    "actor_type": "agent",
                    "role_in_room": "member",
                    "display_name": "Bound Agent",
                    "joined_at": "2026-07-17T00:00:01Z"
                },
                {
                    "member_id": "33333333-3333-4333-8333-333333333333",
                    "owner_member_id": local_human,
                    "actor_type": "agent",
                    "role_in_room": "member",
                    "display_name": "Unbound Agent",
                    "joined_at": "2026-07-17T00:00:02Z"
                },
                {
                    "member_id": "44444444-4444-4444-8444-444444444444",
                    "actor_type": "user",
                    "role_in_room": "member",
                    "display_name": "Remote Human",
                    "joined_at": "2026-07-17T00:00:03Z"
                }
            ]
        });
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let shutdown = CancellationToken::new();
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            room_wakes,
            access_wakes,
            RoomReadCursorWakeBus::default(),
            shutdown,
            Duration::from_millis(20),
        );

        // No wake_sender call: periodic durable scan must find the Pending row.
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while fake.posts.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("periodic scan posted durable row");
        let posts = fake.posts.lock().await.clone();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0]["event_type"], "message");
        assert_eq!(posts[0]["source_sequence"], "1");
        assert_eq!(posts[0]["source_id"], pending.source_id);
        assert_eq!(posts[0]["payload"]["body"], "hello");
        assert!(posts[0].get("token").is_none());

        // POST 201 confirms Bedrock only: no local transcript/outbox mutation.
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert!(projection
            .outbox
            .iter()
            .any(|r| r.client_event_id == "client-1"));
        assert!(with_rooms_handle(&rooms, |s| s.get(&key))
            .unwrap()
            .unwrap()
            .transcript
            .is_empty());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            fake.posts.lock().await.len(),
            1,
            "201 suppresses instant repost"
        );

        let request_meta = fake.request_meta.lock().await.clone();
        assert_eq!(request_meta[0].0, "/api/v1/rooms/fed-e2e/events");
        assert_eq!(request_meta[0].1.as_deref(), Some("Bearer secret-bearer"));
        assert_eq!(request_meta[0].2.as_deref(), Some("0"));

        // Roster committed before first event; humans default Unavailable and agents None
        // until an exact room_presence frame arrives.
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(projection.state, RoomAccessState::Recovering);
        assert_eq!(
            projection.members[0].derived_presence,
            Some(MemberPresence::Unavailable)
        );
        assert_eq!(projection.members[1].derived_presence, None);
        assert_eq!(projection.members[1].local_binding_available, Some(true));
        assert_eq!(projection.members[2].derived_presence, None);
        assert_eq!(projection.members[2].local_binding_available, Some(false));
        assert_eq!(
            projection.members[3].derived_presence,
            Some(MemberPresence::Unavailable)
        );
        assert_eq!(projection.members[3].local_binding_available, None);

        let tx = fake.sse_tx.lock().await.clone().expect("SSE connected");
        tx.send(Ok(Event::default().event("room_presence").data(
            json!({
                "room_id":"fed-e2e",
                "members":[
                    {
                        "member_id": local_human,
                        "actor_type":"user",
                        "role_in_room":"owner",
                        "display_name":"Owner Human",
                        "joined_at":"2026-07-17T00:00:00Z"
                    },
                    {
                        "member_id": "44444444-4444-4444-8444-444444444444",
                        "actor_type":"user",
                        "role_in_room":"member",
                        "display_name":"Remote Human",
                        "joined_at":"2026-07-17T00:00:03Z"
                    }
                ]
            })
            .to_string(),
        )))
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                if projection.members[0].derived_presence == Some(MemberPresence::Live)
                    && projection.members[1].derived_presence.is_none()
                    && projection.members[2].derived_presence.is_none()
                    && projection.members[3].derived_presence == Some(MemberPresence::Live)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("presence projection applied");
        while access_rx.try_recv().is_ok() {}
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(
            projection.members[0].derived_presence,
            Some(MemberPresence::Live)
        );
        assert_eq!(projection.members[1].derived_presence, None);
        assert_eq!(projection.members[2].derived_presence, None);
        assert_eq!(
            projection.members[3].derived_presence,
            Some(MemberPresence::Live)
        );

        // Ordered SSE is the ONLY confirmation rail.
        let row = json!({
            "id":"ledger-1",
            "sequence":"1",
            "event_type":"message",
            "correlation_id":"fed-e2e",
            "virtual_path":"/rooms/fed-e2e",
            "actor_id":"principal-1",
            "actor_member_id":local_human,
            "source_id":pending.source_id,
            "source_sequence":"1",
            "payload":{
                "client_event_id":"client-1",
                "author_member_id":local_human,
                "body":"hello",
                "mention_member_ids":[]
            }
        });
        tx.send(Ok(Event::default()
            .event("room_event")
            .id("1")
            .data(row.to_string())))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let done = with_rooms_handle(&rooms, |s| {
                    let room = s.get(&key).unwrap().unwrap();
                    let access = s.room_access(&key).unwrap();
                    room.transcript.len() == 1 && access.outbox.is_empty()
                });
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("SSE confirmation committed");
        tokio::time::timeout(Duration::from_secs(60), message_rx.recv())
            .await
            .expect("message wake")
            .expect("message wake open");
        // Startup + roster can precede this; assert at least one access wake.
        tokio::time::timeout(Duration::from_secs(60), access_rx.recv())
            .await
            .expect("access wake")
            .expect("access wake open");
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.room_access(&key))
                .unwrap()
                .state,
            RoomAccessState::Live
        );

        // Drain startup/ingest/state wakes before precise replay assertions.
        while message_rx.try_recv().is_ok() {}
        while access_rx.try_recv().is_ok() {}

        // Exact SSE replay reaches store dedup: no transcript/access wake.
        tx.send(Ok(Event::default()
            .event("room_event")
            .id("1")
            .data(row.to_string())))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(
            message_rx.try_recv().is_err(),
            "Duplicate emits no message wake"
        );
        assert!(
            access_rx.try_recv().is_err(),
            "Duplicate emits no access wake"
        );

        // A room row that is neither a message nor an allowlisted workspace
        // marker advances only the durable cursor/access rail.
        let non_message = json!({
            "id":"ledger-2",
            "sequence":"2",
            "event_type":"room.member.updated",
            "correlation_id":"fed-e2e",
            "virtual_path":"/rooms/fed-e2e",
            "payload":{}
        });
        tx.send(Ok(Event::default()
            .event("room_event")
            .id("2")
            .data(non_message.to_string())))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if with_rooms_handle(&rooms, |s| s.room_access(&key))
                    .unwrap()
                    .last_confirmed_global_sequence
                    == Some(2)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("non-message cursor committed");
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.get(&key))
                .unwrap()
                .unwrap()
                .transcript
                .len(),
            1
        );
        assert!(message_rx.try_recv().is_err());
        tokio::time::timeout(Duration::from_secs(60), access_rx.recv())
            .await
            .expect("non-message access wake")
            .expect("access bus open");

        // Control-frame id may be inherited by eventsource-stream; branch by
        // event type and ignore it. Heartbeat refreshes roster using cached
        // live human ids, not cursor.
        tx.send(Ok(Event::default()
            .event("heartbeat")
            .id("2")
            .data(json!({"sequence":"2"}).to_string())))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), access_rx.recv())
            .await
            .expect("heartbeat roster wake")
            .expect("access bus open");
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.room_access(&key))
                .unwrap()
                .last_confirmed_global_sequence,
            Some(2)
        );
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(
            projection.members[0].derived_presence,
            Some(MemberPresence::Live)
        );
        assert_eq!(projection.members[1].derived_presence, None);
        assert_eq!(projection.members[2].derived_presence, None);
        assert_eq!(
            projection.members[3].derived_presence,
            Some(MemberPresence::Live)
        );

        // A previously unknown author triggers one immediate roster refresh;
        // the refreshed agent projection wins over the Human fallback.
        let remote_agent = "55555555-5555-4555-8555-555555555555";
        let mut roster = fake.members.lock().await.clone();
        roster["members"].as_array_mut().unwrap().push(json!({
            "member_id": remote_agent,
            "owner_member_id": "44444444-4444-4444-8444-444444444444",
            "actor_type":"agent",
            "role_in_room":"member",
            "display_name":"Remote Agent",
            "joined_at":"2026-07-17T00:00:04Z"
        }));
        *fake.members.lock().await = roster;
        let remote_row = json!({
            "id":"ledger-big",
            "sequence":"9007199254740993",
            "event_type":"message",
            "correlation_id":"fed-e2e",
            "virtual_path":"/rooms/fed-e2e",
            "actor_id":"principal-remote",
            "actor_member_id":remote_agent,
            "source_id":"room:fed-e2e:member:remote:producer:remote",
            "source_sequence":"1",
            "payload":{
                "client_event_id":"remote-1",
                "author_member_id":remote_agent,
                "body":"from remote agent",
                "mention_member_ids":[]
            }
        });
        tx.send(Ok(Event::default()
            .event("room_event")
            .id("9007199254740993")
            .data(remote_row.to_string())))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let is_agent = with_rooms_handle(&rooms, |s| {
                    s.get(&key)
                        .unwrap()
                        .unwrap()
                        .transcript
                        .last()
                        .map(|m| m.author_kind == RoomParticipantKind::Agent)
                        .unwrap_or(false)
                });
                if is_agent {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("unknown author refreshed and mapped as Agent");
        // Regression for PR #366 review comment 3727657452: the unknown-author
        // roster refresh must reuse this epoch's live-human cache (populated
        // above via the room_presence frame and reconfirmed by heartbeat), not
        // an empty set that would mark every human member Unavailable.
        let post_refresh = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(
            post_refresh.members[0].derived_presence,
            Some(MemberPresence::Live),
            "unknown-author roster refresh must preserve epoch live-human presence for local_human"
        );
        assert_eq!(
            post_refresh
                .members
                .iter()
                .find(|m| m.member_id == "44444444-4444-4444-8444-444444444444")
                .expect("remote human present after roster refresh")
                .derived_presence,
            Some(MemberPresence::Live),
            "unknown-author roster refresh must preserve epoch live-human presence for remote human"
        );
        tokio::time::timeout(Duration::from_secs(60), message_rx.recv())
            .await
            .expect("unknown-author message wake")
            .expect("message bus open");
        tokio::time::timeout(Duration::from_secs(60), access_rx.recv())
            .await
            .expect("unknown-author coalesced access wake")
            .expect("access bus open");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(message_rx.try_recv().is_err());
        assert!(
            access_rx.try_recv().is_err(),
            "unknown-author Ingested emits exactly one access wake"
        );

        // Remove the author only from durable projection, then replay the
        // exact ledger row. The one refresh may find it remotely, but Duplicate
        // commits no roster and emits no wake.
        with_rooms_handle(&rooms, |s| {
            let projection = s.room_access(&key).unwrap();
            let filtered: Vec<_> = projection
                .members
                .into_iter()
                .filter(|member| member.member_id != remote_agent)
                .collect();
            s.update_room_access_safe(&key, None, Some(&filtered), None)
                .unwrap();
        });
        tx.send(Ok(Event::default()
            .event("room_event")
            .id("9007199254740993")
            .data(remote_row.to_string())))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(message_rx.try_recv().is_err());
        assert!(
            access_rx.try_recv().is_err(),
            "unknown-author Duplicate is a total no-op"
        );

        // Lease loss atomically moves state+presence to Recovering/Unavailable.
        fake.events_status.store(500, Ordering::Release);
        drop(tx);
        fake.sse_tx.lock().await.take();
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                if projection.state == RoomAccessState::Recovering
                    && projection.members.iter().all(|m| match m.actor_type {
                        FederatedActorType::User => {
                            m.derived_presence == Some(MemberPresence::Unavailable)
                        }
                        FederatedActorType::Agent => m.derived_presence.is_none(),
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("lease loss downgraded state and presence");

        supervisor.shutdown().await;
        fake.events_status.store(200, Ordering::Release);
        let restarted = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        restarted.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while fake.request_meta.lock().await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("restart subscribed from durable cursor");
        let requests = fake.request_meta.lock().await.clone();
        assert_eq!(
            requests.last().unwrap().2.as_deref(),
            Some("9007199254740993")
        );
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                if projection.state == RoomAccessState::Live
                    && projection.last_confirmed_global_sequence == Some(9_007_199_254_740_993)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cursor > H went Live without regression");
        restarted.shutdown().await;
        server.abort();
    }

    async fn run_fake_sender_status(status: StatusCode, error: &str, expected: PostAction) {
        let key = RoomKey::new(format!("status-{}", status.as_u16()));
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Status", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "status-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        store
            .allocate_outbox_pending(
                &key,
                human,
                "status-row",
                "message",
                json!({"body":"status"}),
                vec![],
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "status-bearer");
        *fake.members.lock().await = json!({"members":[{
            "member_id":human,
            "actor_type":"user",
            "role_in_room":"owner",
            "display_name":"Human",
            "joined_at":"2026-07-17T00:00:00Z"
        }]});
        fake.ledger_status.store(status.as_u16(), Ordering::Release);
        *fake.ledger_error.lock().await = error.to_string();
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while fake.posts.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fake server received POST");
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                let row = projection.outbox.first().unwrap();
                let done = match expected {
                    PostAction::FailRow => {
                        row.state == ocean_core::OutboxItemState::Failed
                            && projection.state != RoomAccessState::Revoked
                    }
                    PostAction::Revoke => {
                        row.state == ocean_core::OutboxItemState::Failed
                            && projection.state == RoomAccessState::Revoked
                    }
                    PostAction::Recover => {
                        row.state == ocean_core::OutboxItemState::Pending
                            && projection.state == RoomAccessState::Recovering
                            && projection.members.iter().all(|m| match m.actor_type {
                                FederatedActorType::User => {
                                    m.derived_presence == Some(MemberPresence::Unavailable)
                                }
                                FederatedActorType::Agent => m.derived_presence.is_none(),
                            })
                    }
                    PostAction::AwaitConfirmation => false,
                };
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("sender status outcome committed");
        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn fake_server_exercises_sender_status_matrix() {
        run_fake_sender_status(
            StatusCode::BAD_REQUEST,
            "unknown_mention_member",
            PostAction::FailRow,
        )
        .await;
        run_fake_sender_status(
            StatusCode::CONFLICT,
            "source_sequence already used",
            PostAction::FailRow,
        )
        .await;
        run_fake_sender_status(
            StatusCode::FORBIDDEN,
            "member_actor_mismatch",
            PostAction::FailRow,
        )
        .await;
        run_fake_sender_status(
            StatusCode::FORBIDDEN,
            "membership_revoked",
            PostAction::Revoke,
        )
        .await;
        run_fake_sender_status(StatusCode::UNAUTHORIZED, "unauthorized", PostAction::Revoke).await;
        run_fake_sender_status(
            StatusCode::TOO_MANY_REQUESTS,
            "rate limited",
            PostAction::Recover,
        )
        .await;
        run_fake_sender_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            PostAction::Recover,
        )
        .await;
        run_fake_sender_status(StatusCode::OK, "unexpected success", PostAction::Recover).await;
    }

    async fn run_fake_members_status(status: StatusCode, revoke: bool) {
        let key = RoomKey::new(format!("members-{}", status.as_u16()));
        let human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Members", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "members-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        store
            .allocate_outbox_pending(
                &key,
                human,
                "members-row",
                "message",
                json!({"body":"blocked until roster"}),
                vec![],
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "members-bearer");
        fake.members_status
            .store(status.as_u16(), Ordering::Release);
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                let row = projection.outbox.first().unwrap();
                let done = if revoke {
                    projection.state == RoomAccessState::Revoked
                        && row.state == ocean_core::OutboxItemState::Failed
                } else {
                    projection.state == RoomAccessState::Recovering
                        && row.state == ocean_core::OutboxItemState::Pending
                };
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("members status outcome committed");
        assert!(
            fake.posts.lock().await.is_empty(),
            "sender waits for first roster"
        );
        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn fake_server_exercises_members_status_matrix() {
        run_fake_members_status(StatusCode::UNAUTHORIZED, true).await;
        run_fake_members_status(StatusCode::FORBIDDEN, true).await;
        run_fake_members_status(StatusCode::INTERNAL_SERVER_ERROR, false).await;
    }

    #[tokio::test]
    async fn two_producer_members_each_post_sequence_one_once() {
        let key = RoomKey::new("fed-two-producers");
        let human = "11111111-1111-4111-8111-111111111111";
        let agent = "22222222-2222-4222-8222-222222222222";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Two", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "two-bearer", human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        let a = store
            .allocate_outbox_pending(
                &key,
                human,
                "human-1",
                "message",
                json!({"body":"human"}),
                vec![],
            )
            .unwrap();
        let b = store
            .allocate_outbox_pending(
                &key,
                agent,
                "agent-1",
                "message",
                json!({"body":"agent"}),
                vec![],
            )
            .unwrap();
        assert_eq!(a.source_sequence, 1);
        assert_eq!(b.source_sequence, 1);
        assert_ne!(a.source_id, b.source_id);
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "two-bearer");
        *fake.members.lock().await = json!({"members":[{
            "member_id":human,
            "actor_type":"user",
            "role_in_room":"owner",
            "display_name":"Human",
            "joined_at":"2026-07-17T00:00:00Z"
        }]});
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while fake.posts.lock().await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("both producer rows posted");
        let posts = fake.posts.lock().await.clone();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0]["source_sequence"], "1");
        assert_eq!(posts[1]["source_sequence"], "1");
        assert_ne!(posts[0]["source_id"], posts[1]["source_id"]);
        assert!(with_rooms_handle(&rooms, |s| s.get(&key))
            .unwrap()
            .unwrap()
            .transcript
            .is_empty());
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.pending_outbox(&key))
                .unwrap()
                .len(),
            2
        );
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert_eq!(fake.posts.lock().await.len(), 2, "both 201 rows suppressed");

        // Restart drops only the in-memory suppression set. Durable Pending
        // rows replay the exact same tuple/content under S1B idempotency.
        supervisor.shutdown().await;
        let restarted = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        restarted.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while fake.posts.lock().await.len() < 4 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("restart retried both exact tuples");
        let replayed = fake.posts.lock().await.clone();
        assert_eq!(replayed[0], replayed[2]);
        assert_eq!(replayed[1], replayed[3]);
        restarted.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn start_during_stop_serializes_to_one_next_epoch() {
        let key = RoomKey::new("fed-epochs");
        let local_human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Epochs", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "epoch-bearer", local_human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let fake = FakeBedrock::new(key.as_str(), "epoch-bearer");
        *fake.members.lock().await = json!({"members":[{
            "member_id":local_human,
            "actor_type":"user",
            "role_in_room":"owner",
            "display_name":"Local Human",
            "joined_at":"2026-07-17T00:00:00Z"
        }]});
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms,
            RoomWakeBus::default(),
            RoomAccessWakeBus::default(),
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.start_room(key.clone()).await;
        tokio::time::timeout(Duration::from_secs(60), async {
            while fake.request_meta.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(supervisor.inner.next_generation.load(Ordering::Acquire), 2);

        let stopping = {
            let supervisor = supervisor.clone();
            let key = key.clone();
            tokio::spawn(async move { supervisor.stop_room(&key).await })
        };
        tokio::task::yield_now().await;
        let starting = {
            let supervisor = supervisor.clone();
            let key = key.clone();
            tokio::spawn(async move { supervisor.start_room(key).await })
        };
        stopping.await.unwrap();
        starting.await.unwrap();
        tokio::time::timeout(Duration::from_secs(60), async {
            while fake.request_meta.lock().await.len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("next epoch subscribed");
        assert_eq!(supervisor.inner.next_generation.load(Ordering::Acquire), 3);
        supervisor.start_room(key.clone()).await; // idempotent while running
        assert_eq!(supervisor.inner.next_generation.load(Ordering::Acquire), 3);
        let slot = supervisor
            .inner
            .slots
            .lock()
            .await
            .get(&key)
            .cloned()
            .unwrap();
        assert!(slot.state.lock().await.as_ref().is_some());

        supervisor.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn revoked_frame_fails_pending_before_revoked_with_one_wake() {
        let key = RoomKey::new("fed-revoke");
        let local_human = "11111111-1111-4111-8111-111111111111";
        let mut store = ocean_store::SqliteRoomStore::open_in_memory().unwrap();
        store
            .create(key.clone(), "Federated", None, chrono::Utc::now())
            .unwrap();
        store
            .install_room_credential(&key, "revoke-bearer", local_human)
            .unwrap();
        store
            .update_room_access_safe(&key, Some(RoomAccessState::Connecting), None, None)
            .unwrap();
        store
            .allocate_outbox_pending(
                &key,
                local_human,
                "client-r",
                "message",
                json!({"body":"may commit remotely"}),
                vec![],
            )
            .unwrap();
        let rooms = Arc::new(std::sync::Mutex::new(store));
        let room_wakes = RoomWakeBus::default();
        let access_wakes = RoomAccessWakeBus::default();
        let mut access_rx = access_wakes.test_subscribe();
        let fake = FakeBedrock::new(key.as_str(), "revoke-bearer");
        fake.hold_ledger_response.store(true, Ordering::Release);
        *fake.members.lock().await = json!({
            "members": [{
                "member_id": local_human,
                "actor_type":"user",
                "role_in_room":"owner",
                "display_name":"Local Human",
                "joined_at":"2026-07-17T00:00:00Z"
            }]
        });
        let (base, server) = start_fake_bedrock(fake.clone()).await;
        let supervisor = FederationSupervisor::for_test(
            &base,
            rooms.clone(),
            room_wakes,
            access_wakes,
            RoomReadCursorWakeBus::default(),
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if fake.sse_tx.lock().await.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("SSE connected");
        tokio::time::timeout(Duration::from_secs(60), async {
            while fake.posts.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("one POST admitted before revoke");
        while access_rx.try_recv().is_ok() {}
        let posts_before = fake.posts.lock().await.len();

        fake.sse_tx
            .lock()
            .await
            .clone()
            .unwrap()
            .send(Ok(Event::default()
                .event("revoked")
                .id("0") // inherited/control id is ignored
                .data(json!({"reason":"membership_revoked"}).to_string())))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                if projection.state == RoomAccessState::Revoked
                    && projection
                        .outbox
                        .iter()
                        .all(|row| row.state == ocean_core::OutboxItemState::Failed)
                    && projection.members.iter().all(|m| match m.actor_type {
                        FederatedActorType::User => {
                            m.derived_presence == Some(MemberPresence::Unavailable)
                        }
                        FederatedActorType::Agent => m.derived_presence.is_none(),
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("revoke cleanup committed");
        tokio::time::timeout(Duration::from_secs(60), access_rx.recv())
            .await
            .expect("one revoke access wake")
            .expect("access bus open");
        // The pre-gate request may finish remotely now, but its dead-epoch
        // response cannot mutate local state and no post-gate request starts.
        fake.release_ledger_response.notify_waiters();
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(
            access_rx.try_recv().is_err(),
            "revoke publishes one wake total"
        );
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(projection.state, RoomAccessState::Revoked);
        assert!(projection
            .outbox
            .iter()
            .all(|row| row.state == ocean_core::OutboxItemState::Failed));
        assert!(with_rooms_handle(&rooms, |s| s.get(&key))
            .unwrap()
            .unwrap()
            .transcript
            .is_empty());
        let stable_posts = fake.posts.lock().await.len();
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert_eq!(fake.posts.lock().await.len(), stable_posts);
        assert!(stable_posts >= posts_before);
        let slot = supervisor.slot_for(&key).await;
        assert!(
            slot.control.mutate(|| ()).await.is_none(),
            "wire revoke closes the stable P2-C producer/control gate"
        );
        assert_eq!(
            supervisor
                .enqueue_federated_message(&key, None, "post-revoke")
                .await,
            Err(IntentError::Forbidden)
        );

        supervisor.shutdown().await;
        server.abort();
    }
}
