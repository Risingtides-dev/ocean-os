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
    FederatedActorType, FederatedRoomMemberProjection, FederatedRoomRole, MemberPresence,
    PublicAgentDescriptor, RoomAccessProjection, RoomAccessState, RoomKey, RoomMessageKind,
    RoomOutboxItem, RoomParticipantKind,
};
use ocean_store::{ConfirmedEvent, IngestOutcome, RoomCredential};
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
    publish_room_access_wake_on, publish_room_wake_on, with_rooms_handle, RoomAccessWakeBus,
    RoomStoreHandle, RoomWakeBus,
};

const FEDERATION_URL_ENV: &str = "OCEAN_FEDERATION_URL";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(35);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const BODY_LIMIT: usize = 64 * 1024;
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
        let ip_host = host.trim_start_matches('[').trim_end_matches(']');
        let loopback = host.eq_ignore_ascii_case("localhost")
            || ip_host
                .parse::<IpAddr>()
                .map(|ip| ip.is_loopback())
                .unwrap_or(false);
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
        Ok(Self { base, http })
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
}

#[derive(Clone)]
pub(super) struct FederationSupervisor {
    inner: Arc<SupervisorInner>,
}

struct SupervisorInner {
    client: Option<FederationClient>,
    invalid_config: bool,
    rooms: RoomStoreHandle,
    room_wakes: RoomWakeBus,
    access_wakes: RoomAccessWakeBus,
    shutdown: CancellationToken,
    slots: Mutex<HashMap<RoomKey, Arc<RoomSlot>>>,
    shutting_down: AtomicBool,
    next_generation: AtomicU64,
    scan_interval: Duration,
}

#[derive(Default)]
struct RoomSlot {
    state: Mutex<Option<RunningRoom>>,
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
        shutdown: CancellationToken,
    ) -> Self {
        let (client, invalid_config) = match FederationClient::from_env() {
            Ok(client) => (client, false),
            Err(_) => (None, true),
        };
        Self::new_inner(
            client,
            invalid_config,
            rooms,
            room_wakes,
            access_wakes,
            shutdown,
            SENDER_SCAN_INTERVAL,
        )
    }

    #[cfg(test)]
    fn for_test(
        base: &str,
        rooms: RoomStoreHandle,
        room_wakes: RoomWakeBus,
        access_wakes: RoomAccessWakeBus,
        shutdown: CancellationToken,
        scan_interval: Duration,
    ) -> Self {
        Self::new_inner(
            Some(FederationClient::new(base).expect("test URL")),
            false,
            rooms,
            room_wakes,
            access_wakes,
            shutdown,
            scan_interval,
        )
    }

    #[cfg(test)]
    pub(super) fn test_disabled(
        rooms: RoomStoreHandle,
        room_wakes: RoomWakeBus,
        access_wakes: RoomAccessWakeBus,
        shutdown: CancellationToken,
    ) -> Self {
        Self::new_inner(
            None,
            false,
            rooms,
            room_wakes,
            access_wakes,
            shutdown,
            SENDER_SCAN_INTERVAL,
        )
    }

    fn new_inner(
        client: Option<FederationClient>,
        invalid_config: bool,
        rooms: RoomStoreHandle,
        room_wakes: RoomWakeBus,
        access_wakes: RoomAccessWakeBus,
        shutdown: CancellationToken,
        scan_interval: Duration,
    ) -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                client,
                invalid_config,
                rooms,
                room_wakes,
                access_wakes,
                shutdown,
                slots: Mutex::new(HashMap::new()),
                shutting_down: AtomicBool::new(false),
                next_generation: AtomicU64::new(1),
                scan_interval,
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

    fn persist_lease_lost(&self, key: &RoomKey, state: RoomAccessState) -> Result<(), BridgeError> {
        persist_lease_lost(&self.inner, key, state)
    }
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

    let mut raw_bound = RawSseEventBound::new(BODY_LIMIT);
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
    let members = match fetch_roster(&inner, &client, &credential, true).await {
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
                        let Ok(row) = parse_sse_json::<WireLedgerRow>(&event.data) else {
                            break EpochOutcome::Recover;
                        };
                        let Ok(sequence) = parse_canonical_u64(&row.sequence) else {
                            break EpochOutcome::Recover;
                        };
                        if !wire_row_scope_is_exact(&row, &key)
                            || event.id != row.sequence
                            || sequence < last_accepted
                        {
                            break EpochOutcome::Recover;
                        }
                        let result = if row.event_type == "message" {
                            ingest_message_row(&inner, &client, &credential, row).await
                        } else {
                            advance_non_message(&inner, &key, sequence)
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
                        match fetch_roster(&inner, &client, &credential, true).await {
                            Ok(members) => {
                                let Ok(current_state) = durable_state(&inner.rooms, &key) else {
                                    break EpochOutcome::Recover;
                                };
                                if !commit_access(&inner, &key, current_state, Some(&members), None) {
                                    break EpochOutcome::Recover;
                                }
                            }
                            Err(outcome) => break outcome,
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
struct RevokedFrame {
    reason: String,
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

async fn read_bounded_json<T: DeserializeOwned>(
    response: reqwest::Response,
    cap: usize,
) -> Result<T, BridgeError> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| BridgeError::Transport)?;
        if bytes.len().saturating_add(chunk.len()) > cap {
            return Err(BridgeError::Protocol);
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| BridgeError::Protocol)
}

async fn fetch_roster(
    inner: &Arc<SupervisorInner>,
    client: &FederationClient,
    credential: &RoomCredential,
    lease_healthy: bool,
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
    let mut members = Vec::with_capacity(envelope.members.len());
    let mut member_ids = HashSet::with_capacity(envelope.members.len());
    for member in envelope.members {
        if member.member_id.is_empty()
            || member.display_name.is_empty()
            || member.joined_at.is_empty()
            || !member_ids.insert(member.member_id.clone())
        {
            return Err(EpochOutcome::Recover);
        }
        let binding = if member.actor_type == FederatedActorType::Agent {
            with_rooms_handle(&inner.rooms, |store| {
                store.resolve_room_agent(&credential.room_id, &member.member_id)
            })
            .map_err(|_| EpochOutcome::Recover)?
            .is_some()
        } else {
            false
        };
        let local_agent = member.owner_member_id.as_deref()
            == Some(credential.local_human_member_id.as_str())
            && binding;
        let local_human = member.actor_type == FederatedActorType::User
            && member.member_id == credential.local_human_member_id;
        let presence = if lease_healthy && (local_human || local_agent) {
            MemberPresence::Live
        } else {
            MemberPresence::Unavailable
        };
        members.push(FederatedRoomMemberProjection {
            member_id: member.member_id,
            owner_member_id: member.owner_member_id,
            actor_type: member.actor_type,
            role_in_room: member.role_in_room,
            display_name: member.display_name,
            public_agent_descriptor: member.public_agent_descriptor,
            joined_at: member.joined_at,
            derived_presence: Some(presence),
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
    let (author_kind, refreshed_roster) =
        match author_kind(inner, &credential.room_id, &payload.author_member_id) {
            Some(kind) => (kind, None),
            None => {
                // One immediate current-epoch roster fetch, then conservative
                // Human. Do NOT commit/wake yet: Duplicate must remain a total
                // no-op, while Ingested coalesces roster + message into one
                // access wake.
                let current_state = durable_state(&inner.rooms, &credential.room_id)?;
                let members = fetch_roster(inner, client, credential, true)
                    .await
                    .map_err(|outcome| match outcome {
                        EpochOutcome::Revoked => BridgeError::Revoked,
                        _ => BridgeError::Protocol,
                    })?;
                let kind = author_kind_from_members(&members, &payload.author_member_id)
                    .unwrap_or(RoomParticipantKind::Human);
                (kind, Some((current_state, members)))
            }
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
        trigger_targets: payload.mention_member_ids,
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
            // P2-B-era trigger claims are VOID BY DESIGN. P2-C owns dispatch
            // from its own boot forward and never backfills these claims.
            let _ = commit.claimed_trigger_targets;
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
) -> Option<RoomParticipantKind> {
    let projection = with_rooms_handle(&inner.rooms, |store| store.room_access(key)).ok()?;
    author_kind_from_members(&projection.members, member_id)
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
        member.derived_presence = Some(MemberPresence::Unavailable);
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
            member.derived_presence = Some(MemberPresence::Unavailable);
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
        sync::{atomic::AtomicU16, Mutex as StdMutex},
    };

    use axum::{
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
    struct FakeBedrock {
        bearer: Arc<String>,
        room: Arc<String>,
        sse_tx: Arc<Mutex<Option<FakeSseTx>>>,
        posts: Arc<Mutex<Vec<Value>>>,
        request_meta: Arc<Mutex<Vec<RequestMeta>>>,
        members: Arc<Mutex<Value>>,
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
                request_meta: Arc::new(Mutex::new(Vec::new())),
                members: Arc::new(Mutex::new(json!({"members":[]}))),
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
            let raw = format!("event: room_event\ndata: {}", "x".repeat(BODY_LIMIT + 128));
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
            .route("/api/v1/ledger/events", post(fake_ledger))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), task)
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
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let connected = fake.sse_tx.lock().await.is_some();
                let live = with_rooms_handle(&rooms, |s| s.room_access(&key))
                    .unwrap()
                    .members
                    .first()
                    .is_some_and(|m| m.derived_presence == Some(MemberPresence::Live));
                if connected && live {
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
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                if projection.state == RoomAccessState::Recovering
                    && projection.last_confirmed_global_sequence == Some(5)
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
        tokio::time::timeout(Duration::from_secs(1), access_rx.recv())
            .await
            .expect("atomic startup access wake")
            .expect("access bus open");
        assert!(access_rx.try_recv().is_err());
        supervisor.shutdown().await;
        fake.release_events_response.notify_waiters();
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
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(1), async {
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
        let supervisor = FederationSupervisor::new_inner(
            None,
            true,
            rooms.clone(),
            room_wakes,
            access_wakes,
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(projection.state, RoomAccessState::Recovering);
        assert_eq!(projection.last_confirmed_global_sequence, Some(9));
        assert!(projection
            .members
            .iter()
            .all(|m| m.derived_presence == Some(MemberPresence::Unavailable)));
        tokio::time::timeout(Duration::from_secs(1), access_rx.recv())
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
            shutdown,
            Duration::from_millis(20),
        );

        // No wake_sender call: periodic durable scan must find the Pending row.
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(2), async {
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

        // Roster committed before first event; healthy lease makes local human Live.
        let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
        assert_eq!(projection.state, RoomAccessState::Recovering);
        assert_eq!(
            projection.members[0].derived_presence,
            Some(MemberPresence::Live)
        );
        assert_eq!(
            projection.members[1].derived_presence,
            Some(MemberPresence::Live)
        );
        assert_eq!(projection.members[1].local_binding_available, Some(true));
        assert_eq!(
            projection.members[2].derived_presence,
            Some(MemberPresence::Unavailable)
        );
        assert_eq!(projection.members[2].local_binding_available, Some(false));
        assert_eq!(
            projection.members[3].derived_presence,
            Some(MemberPresence::Unavailable)
        );
        assert_eq!(projection.members[3].local_binding_available, None);

        // Ordered SSE is the ONLY confirmation rail.
        let tx = fake.sse_tx.lock().await.clone().expect("SSE connected");
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

        tokio::time::timeout(Duration::from_secs(2), async {
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
        tokio::time::timeout(Duration::from_secs(1), message_rx.recv())
            .await
            .expect("message wake")
            .expect("message wake open");
        // Startup + roster can precede this; assert at least one access wake.
        tokio::time::timeout(Duration::from_secs(1), access_rx.recv())
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

        // A non-message room row advances only the durable cursor/access rail.
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
        tokio::time::timeout(Duration::from_secs(1), async {
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
        tokio::time::timeout(Duration::from_secs(1), access_rx.recv())
            .await
            .expect("non-message access wake")
            .expect("access bus open");

        // Control-frame id may be inherited by eventsource-stream; branch by
        // event type and ignore it. Heartbeat refreshes roster, not cursor.
        tx.send(Ok(Event::default()
            .event("heartbeat")
            .id("2")
            .data(json!({"sequence":"2"}).to_string())))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), access_rx.recv())
            .await
            .expect("heartbeat roster wake")
            .expect("access bus open");
        assert_eq!(
            with_rooms_handle(&rooms, |s| s.room_access(&key))
                .unwrap()
                .last_confirmed_global_sequence,
            Some(2)
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
        tokio::time::timeout(Duration::from_secs(1), async {
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
        tokio::time::timeout(Duration::from_secs(1), message_rx.recv())
            .await
            .expect("unknown-author message wake")
            .expect("message bus open");
        tokio::time::timeout(Duration::from_secs(1), access_rx.recv())
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
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                if projection.state == RoomAccessState::Recovering
                    && projection
                        .members
                        .iter()
                        .all(|m| m.derived_presence == Some(MemberPresence::Unavailable))
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
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        restarted.startup().await;
        tokio::time::timeout(Duration::from_secs(1), async {
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
        tokio::time::timeout(Duration::from_secs(1), async {
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
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while fake.posts.lock().await.is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fake server received POST");
        tokio::time::timeout(Duration::from_secs(1), async {
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
                            && projection
                                .members
                                .iter()
                                .all(|m| m.derived_presence == Some(MemberPresence::Unavailable))
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
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(1), async {
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
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(1), async {
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
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        restarted.startup().await;
        tokio::time::timeout(Duration::from_secs(1), async {
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
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.start_room(key.clone()).await;
        tokio::time::timeout(Duration::from_secs(1), async {
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
        tokio::time::timeout(Duration::from_secs(1), async {
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
            CancellationToken::new(),
            Duration::from_millis(20),
        );
        supervisor.startup().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fake.sse_tx.lock().await.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("SSE connected");
        tokio::time::timeout(Duration::from_secs(1), async {
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

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let projection = with_rooms_handle(&rooms, |s| s.room_access(&key)).unwrap();
                if projection.state == RoomAccessState::Revoked
                    && projection
                        .outbox
                        .iter()
                        .all(|row| row.state == ocean_core::OutboxItemState::Failed)
                    && projection
                        .members
                        .iter()
                        .all(|m| m.derived_presence == Some(MemberPresence::Unavailable))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("revoke cleanup committed");
        tokio::time::timeout(Duration::from_secs(1), access_rx.recv())
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

        supervisor.shutdown().await;
        server.abort();
    }
}
