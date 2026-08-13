//! Async daemon client for the shell. Mirrors the wire contract the blocking
//! `DaemonClient` in `main.rs` uses, but non-blocking so the render loop never
//! stalls on the network.
//!
//! Endpoints used in Phase 1:
//!   GET  /health                       liveness
//!   POST /v1/agent/sessions            eager session mint (scopes the stream)
//!   POST /v1/agent/turns               submit a turn (streams over SSE)
//!   GET  /v1/agent/events?session_id=  SSE of AgentTurnEvent
//!
//! Scoped event streams reconnect from Last-Event-ID. Synchronized session
//! replacement restarts the stream strictly after the daemon-issued fence.

use std::{path::PathBuf, time::Duration};

use futures::StreamExt;
use ocean_agent_sdk::{
    AgentSessionCreateRequest, AgentSessionCreateResponse, AgentSessionId, AgentTurnEvent,
    AgentTurnRequest, AgentTurnResponse,
};
use ocean_core::{
    CompactResponse, EventEnvelope, HealthResponse, PermissionDecision, PermissionDecisionRequest,
    PermissionId, PermissionMode, PermissionSettingsRequest, PermissionSettingsResponse, RequestId,
    SessionSyncResponse,
};
use ocean_observatory::{
    observer_token_for_child, observer_token_from_file, EventEnvelope as ObservatoryEventEnvelope,
    EventPayload, ObservatorySnapshot,
};
use tokio::sync::mpsc;

use super::action::{Action, CompactFailure, HealthSource};

/// Submission certainty matters because a turn may contain side-effecting
/// tools. Only `DefinitelyUnsent` is safe to retry/restore automatically;
/// `OutcomeUnknown` means the daemon may already be executing the prompt.
#[derive(Debug)]
pub enum TurnSubmitError {
    DefinitelyUnsent(String),
    Rejected(String),
    SessionBusy,
    OutcomeUnknown(String),
}

impl std::fmt::Display for TurnSubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DefinitelyUnsent(message)
            | Self::Rejected(message)
            | Self::OutcomeUnknown(message) => f.write_str(message),
            Self::SessionBusy => f.write_str("session is still working"),
        }
    }
}

#[derive(Clone)]
pub struct DaemonClient {
    /// Short-lived health/control/session requests.
    http: reqwest::Client,
    /// Fire-and-ack turn submission. This client has a long deadman timeout for
    /// a wedged daemon, but normal turns return as soon as they are accepted and
    /// continue over SSE.
    turn_http: reqwest::Client,
    base: String,
}

impl DaemonClient {
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(1800))
            .build()?;
        // Turn submission is infrequent and non-idempotent. Reusing an idle
        // HTTP/1 connection lets a server-side keep-alive close race the next
        // POST: the daemon can accept the turn while reqwest reports that the
        // acknowledgement connection closed. A fresh localhost connection per
        // submission removes that false outcome-unknown path without retrying
        // any request that may have reached the daemon.
        let turn_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(1800))
            .pool_max_idle_per_host(0)
            .build()?;
        Ok(Self {
            http,
            turn_http,
            base: base_url.trim_end_matches('/').to_string(),
        })
    }
    /// The daemon's base URL (e.g. `http://127.0.0.1:4780`).
    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub async fn health(&self) -> Result<HealthResponse, String> {
        self.http
            .get(format!("{}/health", self.base))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?
            .json::<HealthResponse>()
            .await
            .map_err(|e| e.to_string())
    }

    /// Session mint with mid-blip retry — see [`Self::agent_turn_retrying`] for
    /// the policy (connect-class failures only).
    pub async fn create_agent_session_retrying(
        &self,
        workspace_root: &str,
        on_retry: impl FnMut(usize, usize),
    ) -> Result<AgentSessionCreateResponse, String> {
        let req = AgentSessionCreateRequest {
            workspace_root: workspace_root.to_string(),
            project_id: None,
            client_type: Some("tui".into()),
        };
        let url = format!("{}/v1/agent/sessions", self.base);
        self.post_json_retrying(&url, &req, on_retry).await
    }

    /// Submit a fire-and-ack turn, riding out a daemon restart without risking
    /// duplicate side effects. Only connect failures are retried; they prove no
    /// HTTP connection existed. Once connected, any timeout, transport, decode,
    /// or 5xx failure has an unknown outcome and must not be replayed. The daemon
    /// normally acknowledges immediately while output continues over SSE; the
    /// long timeout is only a deadman for a wedged acknowledgement path.
    pub async fn agent_turn_retrying(
        &self,
        req: &AgentTurnRequest,
        mut on_retry: impl FnMut(usize, usize),
    ) -> Result<AgentTurnResponse, TurnSubmitError> {
        const RETRY_DELAYS_MS: &[u64] = &[500, 1000, 2000, 3000, 4000, 5000];
        let total = RETRY_DELAYS_MS.len() + 1;
        let url = format!("{}/v1/agent/turns", self.base);
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            let response = match self.turn_http.post(&url).json(req).send().await {
                Ok(response) => response,
                Err(error) if error.is_connect() && attempt < total => {
                    on_retry(attempt, total);
                    tokio::time::sleep(Duration::from_millis(RETRY_DELAYS_MS[attempt - 1])).await;
                    continue;
                }
                Err(error) if error.is_connect() => {
                    return Err(TurnSubmitError::DefinitelyUnsent(format!(
                        "daemon unreachable after {attempt} attempts: {error}"
                    )));
                }
                Err(_) => {
                    return Err(TurnSubmitError::OutcomeUnknown(
                        "turn response was interrupted after the request connected".into(),
                    ));
                }
            };

            let status = response.status();
            // Ocean uses HTTP 408 only after the runtime has actually executed
            // and emitted a failed TurnFinished. Its JSON body is therefore a
            // normal known terminal response, not an admission rejection.
            if status_proves_turn_rejection(status) {
                let rejection = response.json::<AgentTurnResponse>().await.ok();
                if status == reqwest::StatusCode::CONFLICT
                    && rejection.as_ref().and_then(|body| body.error.as_deref())
                        == Some("session has an active operation; try again shortly")
                {
                    return Err(TurnSubmitError::SessionBusy);
                }
                let message = match status {
                    reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => {
                        "turn authorization was rejected".into()
                    }
                    reqwest::StatusCode::TOO_MANY_REQUESTS => {
                        "too many concurrent turns; try again shortly".into()
                    }
                    reqwest::StatusCode::CONFLICT => {
                        "turn conflicted with current session state".into()
                    }
                    reqwest::StatusCode::BAD_REQUEST
                    | reqwest::StatusCode::UNPROCESSABLE_ENTITY => {
                        "turn request was rejected".into()
                    }
                    _ => format!("turn rejected ({})", status.as_u16()),
                };
                return Err(TurnSubmitError::Rejected(message));
            }
            if status.is_server_error() {
                return Err(TurnSubmitError::OutcomeUnknown(format!(
                    "turn service returned {} after receiving the request",
                    status.as_u16()
                )));
            }
            return response.json::<AgentTurnResponse>().await.map_err(|_| {
                TurnSubmitError::OutcomeUnknown("turn acknowledgement could not be decoded".into())
            });
        }
    }

    /// POST `body` as JSON and decode a JSON response, retrying connect-class
    /// transport failures on [`RETRY_DELAYS_MS`] backoff. Used for session mint.
    async fn post_json_retrying<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        body: &B,
        mut on_retry: impl FnMut(usize, usize),
    ) -> Result<T, String> {
        /// Backoff before attempts 2..=N (attempt 1 fires immediately).
        const RETRY_DELAYS_MS: &[u64] = &[500, 1000, 2000, 3000, 4000, 5000];
        let total = RETRY_DELAYS_MS.len() + 1;
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            let sent = self.http.post(url).json(body).send().await;
            match sent.and_then(|r| r.error_for_status()) {
                Ok(resp) => return resp.json::<T>().await.map_err(|e| e.to_string()),
                // No connection was established: the daemon is down or
                // mid-restart and the request never landed — safe to retry.
                Err(e) if e.is_connect() && attempt < total => {
                    on_retry(attempt, total);
                    tokio::time::sleep(Duration::from_millis(RETRY_DELAYS_MS[attempt - 1])).await;
                }
                Err(e) => {
                    return Err(if e.is_connect() {
                        format!("daemon unreachable after {attempt} attempts: {e}")
                    } else {
                        e.to_string()
                    });
                }
            }
        }
    }

    /// Subscribe to `/v1/agent/events` scoped to `session_id`, forwarding each
    /// decoded `AgentTurnEvent` onto the action channel. SELF-HEALING: when the
    /// stream drops (daemon restart, idle timeout, network blip), it reconnects
    /// with the daemon's `Last-Event-ID` replay (OCEAN-129) so no deltas are
    /// lost — a dead stream was silently eating turn output before this.
    /// Returns the task handle so a superseding subscription (session switch)
    /// can abort this one.
    pub fn spawn_event_stream(
        &self,
        session_id: AgentSessionId,
        actions: mpsc::UnboundedSender<Action>,
        replay_first: bool,
        initial_last_event_id: Option<String>,
        binding_generation: u64,
        stream_generation: u64,
    ) -> tokio::task::JoinHandle<()> {
        let http = self.http.clone();
        // `replay=1` (OCEAN-305): a scoped subscriber with no Last-Event-ID gets
        // the session's buffered history replayed — closing the race where the
        // subscription lands moments after the turn's first deltas.
        // Replay only for a FRESH chat (mint path): it closes the race where
        // the subscription lands after the turn's first deltas. A resumed chat
        // already loaded its transcript from disk — replay would duplicate it.
        let url = format!(
            "{}/v1/agent/events?session_id={}{}",
            self.base,
            session_id,
            if replay_first { "&replay=1" } else { "" }
        );
        tokio::spawn(async move {
            let mut last_event_id = initial_last_event_id;
            loop {
                // The client's default 120s TOTAL timeout would kill a live
                // SSE body after 2 minutes (the original silent-stream-death
                // bug) — override per-request with an effectively-infinite cap.
                let mut req = http
                    .get(&url)
                    .timeout(std::time::Duration::from_secs(60 * 60 * 24 * 365));
                if let Some(id) = &last_event_id {
                    req = req.header("Last-Event-ID", id.clone());
                }
                if let Ok(resp) = req.send().await.and_then(|r| r.error_for_status()) {
                    // Typed recovery: clears ONLY the SSE source — no
                    // connected/reconnected success text is rendered.
                    let _ = actions.send(Action::HealthRecovered(HealthSource::Sse));
                    let mut stream = resp.bytes_stream();
                    let mut buf = String::new();
                    while let Some(chunk) = stream.next().await {
                        let Ok(bytes) = chunk else { break };
                        buf.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(idx) = buf.find("\n\n") {
                            let frame = buf[..idx].to_string();
                            buf.drain(..idx + 2);
                            if let Some(id) = parse_sse_id(&frame) {
                                last_event_id = Some(id);
                            }
                            if parse_sse_event(&frame) == Some("error") {
                                let _ = actions.send(Action::BoundAgentReplayResetRequired {
                                    session_id,
                                    binding_generation,
                                    stream_generation,
                                });
                                return;
                            } else if let Some(evt) = parse_sse_frame(&frame) {
                                let _ = actions.send(Action::BoundAgentEvent {
                                    session_id,
                                    binding_generation,
                                    stream_generation,
                                    event: Box::new(evt),
                                });
                            }
                        }
                    }
                }
                // Dropped (or failed to connect): mark the SSE source degraded,
                // brief backoff, then resubscribe with the last seen id so the
                // daemon replays what we missed. The typed transition persists
                // until THIS source reconnects — unrelated notices can't clear it.
                let _ = actions.send(Action::BoundAgentStreamGap {
                    session_id,
                    binding_generation,
                    stream_generation,
                });
                let _ = actions.send(Action::HealthDegraded {
                    source: HealthSource::Sse,
                    condition: "stream reconnecting".into(),
                });
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
    }

    /// Maintain a truthful daemon-wide Observatory projection: load the
    /// boot-bound local summary token, fetch an authoritative snapshot, then
    /// tail cursor-ordered lifecycle events. Any auth failure, daemon restart,
    /// reset, gap, or cursor discontinuity retains the last UI graph as stale
    /// and restarts from a fresh token + snapshot rather than guessing.
    pub fn spawn_observatory_stream(
        &self,
        actions: mpsc::UnboundedSender<Action>,
    ) -> tokio::task::JoinHandle<()> {
        let http = self.http.clone();
        let base = self.base.clone();
        let config_dir = ocean_config_dir();
        tokio::spawn(async move {
            // An environment token is a boot-bound launch credential. If it
            // receives 401 after daemon restart, permanently fall back to the
            // rotating secure token file for this TUI process.
            let mut allow_environment_token = true;
            loop {
                let token_dir = config_dir.clone();
                let use_environment = allow_environment_token;
                let token = match tokio::task::spawn_blocking(move || {
                    if use_environment {
                        observer_token_for_child(&token_dir)
                    } else {
                        observer_token_from_file(&token_dir)
                    }
                })
                .await
                {
                    Ok(Ok(Some(token))) => token,
                    _ => {
                        let _ = actions.send(Action::ObservatoryDisconnected);
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };

                let snapshot_response = http
                    .get(format!("{base}/v1/observatory/snapshot?detail=summary"))
                    .bearer_auth(&token)
                    .send()
                    .await;
                let snapshot = match snapshot_response {
                    Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                        allow_environment_token = false;
                        let _ = actions.send(Action::ObservatoryDisconnected);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    Ok(response) => match response.error_for_status() {
                        Ok(response) => match response.json::<ObservatorySnapshot>().await {
                            Ok(snapshot) => snapshot,
                            Err(_) => {
                                let _ = actions.send(Action::ObservatoryDisconnected);
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                continue;
                            }
                        },
                        Err(_) => {
                            let _ = actions.send(Action::ObservatoryDisconnected);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    },
                    Err(_) => {
                        let _ = actions.send(Action::ObservatoryDisconnected);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let mut cursor = snapshot.watermark_cursor;
                let daemon_instance_id = snapshot.daemon_instance_id.clone();
                let _ = actions.send(Action::ObservatorySnapshot(Box::new(snapshot)));

                let response = http
                    .get(format!(
                        "{base}/v1/observatory/events?after={}&scope=summary",
                        cursor.as_string()
                    ))
                    .bearer_auth(&token)
                    .header("Last-Event-ID", cursor.as_string())
                    .timeout(Duration::from_secs(60 * 60 * 24 * 365))
                    .send()
                    .await;
                let response = match response {
                    Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
                        allow_environment_token = false;
                        let _ = actions.send(Action::ObservatoryDisconnected);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    Ok(response) => match response.error_for_status() {
                        Ok(response) => response,
                        Err(_) => {
                            let _ = actions.send(Action::ObservatoryDisconnected);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    },
                    Err(_) => {
                        let _ = actions.send(Action::ObservatoryDisconnected);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };

                let mut stream = response.bytes_stream();
                let mut buffer = String::new();
                let mut rebaseline = false;
                while let Some(chunk) = stream.next().await {
                    let Ok(bytes) = chunk else { break };
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(index) = buffer.find("\n\n") {
                        let frame = buffer[..index].to_string();
                        buffer.drain(..index + 2);
                        let event = match parse_observatory_frame(&frame) {
                            ObservatoryFrame::KeepAlive => continue,
                            ObservatoryFrame::Rebaseline => {
                                rebaseline = true;
                                break;
                            }
                            ObservatoryFrame::Event(event) => event,
                        };
                        if event.daemon_instance_id != daemon_instance_id
                            || !event.cursor.is_consecutive_after(cursor)
                            || matches!(
                                event.payload,
                                EventPayload::StreamGap { .. } | EventPayload::StreamReset { .. }
                            )
                        {
                            rebaseline = true;
                            break;
                        }
                        cursor = event.cursor;
                        let _ = actions.send(Action::ObservatoryEvent(event));
                    }
                    if rebaseline {
                        break;
                    }
                }
                let _ = actions.send(Action::ObservatoryDisconnected);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    }
}

/// Match daemon config resolution without depending on the session-owning
/// `ocean-agent` crate: explicit override, XDG, HOME, then cwd fallback.
fn ocean_config_dir() -> PathBuf {
    if let Some(path) = std::env::var_os("OCEAN_CONFIG_DIR") {
        return PathBuf::from(path);
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("ocean-rs");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".config").join("ocean-rs");
    }
    PathBuf::from(".ocean-rs")
}

impl DaemonClient {
    /// Subscribe to the GLOBAL `/v1/events` stream (permission requests and
    /// decisions ride here, not on the agent stream), forwarding each decoded
    /// `EventEnvelope` onto the action channel. Fire-and-forget task.
    pub fn spawn_global_event_stream(&self, actions: mpsc::UnboundedSender<Action>) {
        let http = self.http.clone();
        let url = format!("{}/v1/events", self.base);
        tokio::spawn(async move {
            loop {
                let req = http
                    .get(&url)
                    .timeout(std::time::Duration::from_secs(60 * 60 * 24 * 365));
                let resp = match req.send().await.and_then(|r| r.error_for_status()) {
                    Ok(r) => r,
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        continue; // self-heal: permissions must keep flowing
                    }
                };
                let mut stream = resp.bytes_stream();
                let mut buf = String::new();
                while let Some(chunk) = stream.next().await {
                    let Ok(bytes) = chunk else { break };
                    buf.push_str(&String::from_utf8_lossy(&bytes));
                    while let Some(idx) = buf.find("\n\n") {
                        let frame = buf[..idx].to_string();
                        buf.drain(..idx + 2);
                        if let Some(env) = parse_sse_data::<EventEnvelope>(&frame) {
                            let _ = actions.send(Action::OceanEvent(Box::new(env)));
                        }
                    }
                }
                // Dropped: brief backoff, then resubscribe.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }

    /// Reload a synchronized, identity-checked session baseline. This is the
    /// only recovery path after a compact outcome or replay anchor is uncertain.
    pub async fn refresh_compacted_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<SessionSyncResponse, CompactFailure> {
        let response = self
            .http
            .get(format!("{}/v1/sessions/{}/sync", self.base, session_id.0))
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|error| CompactFailure {
                message: format!("session sync failed: {error}"),
                transcript_may_have_changed: true,
            })?;
        let status = response.status();
        let sync = response
            .json::<SessionSyncResponse>()
            .await
            .map_err(|error| CompactFailure {
                message: format!("session sync failed: {error}"),
                transcript_may_have_changed: true,
            })?;
        if sync.session_id != session_id.0 {
            return Err(CompactFailure {
                message: "session sync response named a different session".into(),
                transcript_may_have_changed: true,
            });
        }
        if !status.is_success() || !sync.ok {
            return Err(CompactFailure {
                message: sync
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("session sync failed ({status})")),
                transcript_may_have_changed: true,
            });
        }
        let snapshot = sync.snapshot.as_ref().ok_or_else(|| CompactFailure {
            message: "session sync response omitted its authoritative snapshot".into(),
            transcript_may_have_changed: true,
        })?;
        if snapshot.session_id != session_id.0 {
            return Err(CompactFailure {
                message: "session sync snapshot named a different session".into(),
                transcript_may_have_changed: true,
            });
        }
        crate::shell::sessions::history_from_sync_snapshot(snapshot).map_err(|message| {
            CompactFailure {
                message,
                transcript_may_have_changed: true,
            }
        })?;
        if sync
            .fence
            .as_ref()
            .and_then(|fence| fence.event_id)
            .is_none()
        {
            return Err(CompactFailure {
                message: "session sync response omitted its replay fence".into(),
                transcript_may_have_changed: true,
            });
        }
        Ok(sync)
    }

    /// Compact a bound session. The daemon returns the replacement transcript
    /// and replay fence from inside the same per-session operation lease; no
    /// independent GET is allowed across that synchronization seam.
    pub async fn compact_session(
        &self,
        session_id: AgentSessionId,
    ) -> Result<CompactResponse, CompactFailure> {
        let response = self
            .http
            .post(format!(
                "{}/v1/sessions/{}/compact",
                self.base, session_id.0
            ))
            .send()
            .await
            .map_err(|error| CompactFailure {
                message: error.to_string(),
                // A pure connection failure proves the POST was not accepted;
                // every later transport failure has an unknown commit outcome.
                transcript_may_have_changed: !error.is_connect(),
            })?;
        let status = response.status();
        let compact = response
            .json::<CompactResponse>()
            .await
            .map_err(|error| CompactFailure {
                message: error.to_string(),
                // Without the typed body there is no proof the daemon rejected
                // before commit, regardless of the proxy/status code observed.
                transcript_may_have_changed: true,
            })?;
        if compact.session_id != session_id.0 {
            return Err(CompactFailure {
                message: "compact response named a different session".into(),
                transcript_may_have_changed: true,
            });
        }
        if !status.is_success() {
            let documented_precommit_rejection = !compact.ok
                && matches!(
                    status,
                    reqwest::StatusCode::NOT_FOUND
                        | reqwest::StatusCode::CONFLICT
                        | reqwest::StatusCode::TOO_MANY_REQUESTS
                );
            return Err(CompactFailure {
                message: if compact.stderr.trim().is_empty() {
                    format!("compact failed ({status})")
                } else {
                    compact.stderr.clone()
                },
                transcript_may_have_changed: !documented_precommit_rejection,
            });
        }
        if !compact.ok {
            return Err(CompactFailure {
                message: if compact.stderr.trim().is_empty() {
                    "compact failed".into()
                } else {
                    compact.stderr.clone()
                },
                transcript_may_have_changed: false,
            });
        }
        let snapshot = compact.sync.as_ref().ok_or_else(|| CompactFailure {
            message: "compact response omitted its authoritative snapshot".into(),
            transcript_may_have_changed: true,
        })?;
        if snapshot.session_id != session_id.0 {
            return Err(CompactFailure {
                message: "compact snapshot named a different session".into(),
                transcript_may_have_changed: true,
            });
        }
        crate::shell::sessions::history_from_sync_snapshot(snapshot).map_err(|message| {
            CompactFailure {
                message,
                transcript_may_have_changed: true,
            }
        })?;
        if compact
            .fence
            .as_ref()
            .and_then(|fence| fence.event_id)
            .is_none()
        {
            return Err(CompactFailure {
                message: "compact response omitted its replay fence".into(),
                transcript_may_have_changed: true,
            });
        }
        Ok(compact)
    }

    /// Cancel the daemon request backing the active turn. This mirrors the
    /// working MacBook TUI path from `5f3fddd6`: the agent turn id is also the
    /// daemon request id.
    pub async fn cancel_request(&self, request_id: RequestId) -> Result<String, String> {
        let response = self
            .http
            .post(format!("{}/v1/requests/{request_id}/cancel", self.base))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let value = response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| error.to_string())?;
        let ok = value
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let message = value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("cancel request failed")
            .to_string();
        if status.is_success() && ok {
            Ok(message)
        } else {
            Err(message)
        }
    }

    /// `POST /v1/permissions/{id}/decision`, replaying the turn's decision
    /// token (OCEAN-185) so the daemon accepts the approval.
    pub async fn permission_decision(
        &self,
        permission_id: PermissionId,
        allow: bool,
        decision_token: Option<String>,
    ) -> Result<(), String> {
        let decision = if allow {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny { reason: None }
        };
        let body = PermissionDecisionRequest {
            permission_id,
            decision,
            decision_token,
        };
        self.http
            .post(format!(
                "{}/v1/permissions/{permission_id}/decision",
                self.base
            ))
            .json(&body)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Fetch the daemon-owned global approval policy for `/permissions`.
    pub async fn permission_settings(&self) -> Result<PermissionSettingsResponse, String> {
        let response = self
            .http
            .get(format!("{}/v1/settings/permissions", self.base))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?
            .json::<PermissionSettingsResponse>()
            .await
            .map_err(|e| e.to_string())?;
        if response.ok {
            Ok(response)
        } else {
            Err(response
                .error
                .unwrap_or_else(|| "daemon rejected permission settings read".into()))
        }
    }

    /// Persist a new daemon-owned approval policy. The response is authoritative
    /// because `OCEAN_YOLO` may mask the saved choice.
    pub async fn set_permission_mode(
        &self,
        mode: PermissionMode,
    ) -> Result<PermissionSettingsResponse, String> {
        let response = self
            .http
            .post(format!("{}/v1/settings/permissions", self.base))
            .json(&PermissionSettingsRequest { mode })
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?
            .json::<PermissionSettingsResponse>()
            .await
            .map_err(|e| e.to_string())?;
        if response.ok {
            Ok(response)
        } else {
            Err(response
                .error
                .unwrap_or_else(|| "daemon rejected permission mode save".into()))
        }
    }
}

/// One entry from `GET /v1/models` — the daemon's model registry plus the
/// readiness stamp (is the provider's credential visible to the daemon?).
/// `ready` defaults to true so a pre-readiness daemon still yields a fully
/// selectable menu instead of an all-grey one.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModelEntry {
    pub id: String,
    pub provider: String,
    pub label: String,
    #[serde(default = "bool_true")]
    pub ready: bool,
}

fn bool_true() -> bool {
    true
}

/// The daemon's currently-selected global model.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CurrentModel {
    pub model: String,
}

/// Response shape of `GET /v1/models`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ModelsResponse {
    pub current: CurrentModel,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}
/// Authoritative model pin for one daemon-owned session.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct SessionConfigResponse {
    pub session_id: AgentSessionId,
    pub model: String,
}

#[derive(serde::Serialize)]
struct SessionModelPatch<'a> {
    model: &'a str,
}

/// One retained memory from `GET /v1/memory`, for the `/memory` browser.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MemoryEntry {
    pub kind: String,
    pub text: String,
}

/// Response shape of `GET /v1/memory`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MemoryResponse {
    #[serde(default)]
    pub memories: Vec<MemoryEntry>,
}

impl DaemonClient {
    /// `GET /v1/models` — the registry with per-model readiness, for the
    /// `/models` picker overlay.
    pub async fn models(&self) -> Result<ModelsResponse, String> {
        self.http
            .get(format!("{}/v1/models", self.base))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?
            .json::<ModelsResponse>()
            .await
            .map_err(|e| e.to_string())
    }

    /// Read the model actually pinned to a session. The TUI never infers this
    /// from the daemon-wide `/v1/models.current` fallback.
    pub async fn session_config(
        &self,
        session_id: AgentSessionId,
    ) -> Result<SessionConfigResponse, String> {
        self.http
            .get(format!(
                "{}/v1/agent/sessions/{session_id}/config",
                self.base
            ))
            .send()
            .await
            .and_then(|response| response.error_for_status())
            .map_err(|error| error.to_string())?
            .json::<SessionConfigResponse>()
            .await
            .map_err(|error| error.to_string())
    }

    /// Persist a model selection on the daemon-owned session and return the
    /// authoritative catalog-resolved model/provider pair.
    pub async fn set_session_model(
        &self,
        session_id: AgentSessionId,
        model: &str,
    ) -> Result<SessionConfigResponse, String> {
        let response = self
            .http
            .patch(format!(
                "{}/v1/agent/sessions/{session_id}/config",
                self.base
            ))
            .json(&SessionModelPatch { model })
            .send()
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        let payload = response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| format!("invalid session model response: {error}"))?;
        if !status.is_success() {
            return Err(payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("daemon rejected session model")
                .to_string());
        }
        serde_json::from_value(payload)
            .map_err(|error| format!("invalid session model response: {error}"))
    }

    /// `GET /v1/memory` — the operator's retained memories, for `/memory`.
    pub async fn memory(&self) -> Result<MemoryResponse, String> {
        self.http
            .get(format!("{}/v1/memory", self.base))
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?
            .json::<MemoryResponse>()
            .await
            .map_err(|e| e.to_string())
    }

    /// `POST /v1/voice/stt` — daemon-owned xAI batch transcription. The TUI
    /// sends a bounded WAV and never handles provider credentials itself.
    pub async fn transcribe_voice(&self, wav: Vec<u8>) -> Result<String, String> {
        let response = self
            .http
            .post(format!("{}/v1/voice/stt", self.base))
            .header(reqwest::header::CONTENT_TYPE, "audio/wav")
            .body(wav)
            .send()
            .await
            .map_err(|error| format!("dictation could not reach the daemon: {error}"))?;
        let status = response.status();
        let payload = response
            .json::<serde_json::Value>()
            .await
            .map_err(|error| format!("dictation response was not valid JSON: {error}"))?;
        if !status.is_success() {
            return Err(payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("dictation transcription failed")
                .to_string());
        }
        let text = payload
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            Err("no speech heard — try again".into())
        } else {
            Ok(text)
        }
    }

    /// `GET /v1/lsp?cwd=<workspace>` — language servers relevant to the
    /// workspace + their install/ready state, for the `/lsp` panel.
    pub async fn lsp(&self, cwd: &str) -> Result<LspResponse, String> {
        self.http
            .get(format!("{}/v1/lsp", self.base))
            .query(&[("cwd", cwd)])
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?
            .json::<LspResponse>()
            .await
            .map_err(|e| e.to_string())
    }

    // ── persistent rooms (board projection surface) ──────────────────────
    //
    // The board view is a projection over a room transcript. These are the
    // only room endpoints the TUI consumes: hydrate reads (`snapshot` +
    // `transcript` paging), the live tail (`events` SSE), and the single
    // existing write path (`messages` POST). There is no board-specific write
    // authority anywhere in this lane.

    /// `GET /v1/rooms/persistent/{key}/snapshot` — roster + first bounded
    /// transcript page + cursors, the board hydrate's first read.
    pub async fn room_snapshot(&self, room_key: &str) -> Result<RoomSnapshot, String> {
        let payload = self
            .http
            .get(format!(
                "{}/v1/rooms/persistent/{room_key}/snapshot",
                self.base
            ))
            .send()
            .await
            .map_err(|e| format!("room snapshot could not reach the daemon: {e}"))?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| format!("room snapshot response was not valid JSON: {e}"))?;
        RoomSnapshot::from_payload(&payload)
    }

    /// `GET /v1/rooms/persistent/{key}/transcript?after_seq=` — one bounded
    /// page of the transcript for the hydrate page walk.
    pub async fn room_transcript_page(
        &self,
        room_key: &str,
        after_seq: u64,
    ) -> Result<TranscriptPage, String> {
        let payload = self
            .http
            .get(format!(
                "{}/v1/rooms/persistent/{room_key}/transcript",
                self.base
            ))
            .query(&[("after_seq", after_seq)])
            .send()
            .await
            .map_err(|e| format!("room transcript could not reach the daemon: {e}"))?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| format!("room transcript response was not valid JSON: {e}"))?;
        TranscriptPage::from_payload(&payload)
    }

    /// `POST /v1/rooms/persistent/{key}/participants` — join the roster. The
    /// caller joins only when the snapshot roster lacks the id (join-if-absent);
    /// the daemon classifies post authors by exact id+kind roster match.
    pub async fn join_room(
        &self,
        room_key: &str,
        id: &str,
        display_name: &str,
    ) -> Result<(), String> {
        let payload = self
            .http
            .post(format!(
                "{}/v1/rooms/persistent/{room_key}/participants",
                self.base
            ))
            .json(&serde_json::json!({
                "id": id,
                "display_name": display_name,
                "kind": "human",
            }))
            .send()
            .await
            .map_err(|e| format!("room join could not reach the daemon: {e}"))?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| format!("room join response was not valid JSON: {e}"))?;
        ok_or_payload_error(&payload)
    }

    /// `POST /v1/rooms/persistent/{key}/messages` — the only write path. For
    /// card ops the body is the entire encoded `CardEnvelope`; the resulting
    /// card change arrives back over the events SSE tail, never from this
    /// response.
    pub async fn post_room_message(
        &self,
        room_key: &str,
        author_id: &str,
        body: &str,
    ) -> Result<(), String> {
        let payload = self
            .http
            .post(format!(
                "{}/v1/rooms/persistent/{room_key}/messages",
                self.base
            ))
            .json(&serde_json::json!({
                "author_id": author_id,
                "author_kind": "human",
                "body": body,
            }))
            .send()
            .await
            .map_err(|e| format!("room post could not reach the daemon: {e}"))?
            .json::<serde_json::Value>()
            .await
            .map_err(|e| format!("room post response was not valid JSON: {e}"))?;
        ok_or_payload_error(&payload)
    }

    /// `GET /v1/rooms/persistent/{key}/events?after_seq=` — the room's live
    /// tail. Mirrors [`Self::spawn_event_stream`]: the default total timeout
    /// would silently kill a long-lived SSE body, so the request overrides it;
    /// reconnects resume from `Last-Event-ID` (the daemon replays strictly
    /// after it — no gaps, no duplicates). `room_access` frames are roster
    /// projections the board view does not consume.
    pub fn spawn_room_event_stream(
        &self,
        room_key: String,
        after_seq: u64,
        generation: u64,
        actions: mpsc::UnboundedSender<Action>,
    ) -> tokio::task::JoinHandle<()> {
        let http = self.http.clone();
        let url = format!(
            "{}/v1/rooms/persistent/{room_key}/events?after_seq={after_seq}",
            self.base
        );
        tokio::spawn(async move {
            let mut last_event_id: Option<String> = None;
            loop {
                let mut req = http
                    .get(&url)
                    .timeout(std::time::Duration::from_secs(60 * 60 * 24 * 365));
                if let Some(id) = &last_event_id {
                    req = req.header("Last-Event-ID", id.clone());
                }
                if let Ok(resp) = req.send().await.and_then(|r| r.error_for_status()) {
                    let mut stream = resp.bytes_stream();
                    let mut buf = String::new();
                    while let Some(chunk) = stream.next().await {
                        let Ok(bytes) = chunk else { break };
                        buf.push_str(&String::from_utf8_lossy(&bytes));
                        while let Some(idx) = buf.find("\n\n") {
                            let frame = buf[..idx].to_string();
                            buf.drain(..idx + 2);
                            if let Some(id) = parse_sse_id(&frame) {
                                last_event_id = Some(id);
                            }
                            if parse_sse_event(&frame) != Some("room_message") {
                                continue;
                            }
                            if let Some(message) =
                                parse_sse_data::<ocean_core::RoomMessage>(&frame)
                            {
                                let _ = actions.send(Action::BoardRoomMessage {
                                    generation,
                                    message: Box::new(message),
                                });
                            }
                        }
                    }
                }
                // Dropped (or failed to connect): report the gap honestly, brief
                // backoff, then resubscribe from the last seen seq.
                let _ = actions.send(Action::BoardStreamGap { generation });
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
    }
}

/// The pieces of `GET .../snapshot` the board hydrate needs.
#[derive(Debug, Clone)]
pub struct RoomSnapshot {
    pub participants: Vec<ocean_core::RoomParticipant>,
    pub transcript: Vec<ocean_core::RoomMessage>,
    pub next_seq: Option<u64>,
    pub has_more: bool,
    /// Highest transcript seq in this snapshot (0 for an empty room) — the
    /// resume point for both paging and the events tail.
    pub last_seq: u64,
}

impl RoomSnapshot {
    fn from_payload(payload: &serde_json::Value) -> Result<Self, String> {
        ok_or_payload_error(payload)?;
        Ok(Self {
            participants: serde_json::from_value(payload["participants"].clone())
                .map_err(|e| format!("invalid room roster in snapshot: {e}"))?,
            transcript: serde_json::from_value(payload["transcript"].clone())
                .map_err(|e| format!("invalid room transcript in snapshot: {e}"))?,
            next_seq: payload["next_seq"].as_u64(),
            has_more: payload["has_more"].as_bool().unwrap_or(false),
            last_seq: payload["last_seq"].as_u64().unwrap_or(0),
        })
    }
}

/// One bounded page of `GET .../transcript`.
#[derive(Debug, Clone)]
pub struct TranscriptPage {
    pub messages: Vec<ocean_core::RoomMessage>,
    pub next_seq: Option<u64>,
    pub has_more: bool,
}

impl TranscriptPage {
    fn from_payload(payload: &serde_json::Value) -> Result<Self, String> {
        ok_or_payload_error(payload)?;
        Ok(Self {
            messages: serde_json::from_value(payload["transcript"].clone())
                .map_err(|e| format!("invalid room transcript page: {e}"))?,
            next_seq: payload["next_seq"].as_u64(),
            has_more: payload["has_more"].as_bool().unwrap_or(false),
        })
    }
}

/// Room endpoints answer errors as `{ "ok": false, "error": ... }` with a
/// non-2xx status; treat a missing/false `ok` as the failure it is.
fn ok_or_payload_error(payload: &serde_json::Value) -> Result<(), String> {
    if payload["ok"].as_bool().unwrap_or(false) {
        Ok(())
    } else {
        Err(payload["error"]
            .as_str()
            .unwrap_or("the daemon rejected the room request")
            .to_string())
    }
}

/// One language server from `GET /v1/lsp`, for the `/lsp` panel.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LspServer {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub ready: bool,
}

/// Response shape of `GET /v1/lsp`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LspResponse {
    #[serde(default)]
    pub servers: Vec<LspServer>,
}

/// Pull the `id:` line out of one SSE frame (for Last-Event-ID replay).
fn parse_sse_id(frame: &str) -> Option<String> {
    frame.lines().find_map(|l| {
        l.strip_prefix("id:")
            .map(|rest| rest.trim().to_string())
            .filter(|s| !s.is_empty())
    })
}

fn parse_sse_event(frame: &str) -> Option<&str> {
    frame.lines().find_map(|line| {
        line.strip_prefix("event:")
            .map(str::trim)
            .filter(|event| !event.is_empty())
    })
}

enum ObservatoryFrame {
    KeepAlive,
    Event(Box<ObservatoryEventEnvelope>),
    Rebaseline,
}

fn parse_observatory_frame(frame: &str) -> ObservatoryFrame {
    if matches!(parse_sse_event(frame), Some("reset" | "error")) {
        return ObservatoryFrame::Rebaseline;
    }
    // Keepalive/comment frames carry no data. Any data frame on this typed
    // stream that fails schema decoding is a continuity break: rebaseline now
    // rather than waiting for a later cursor gap to expose it.
    if !frame.lines().any(|line| line.starts_with("data:")) {
        return ObservatoryFrame::KeepAlive;
    }
    parse_sse_data::<ObservatoryEventEnvelope>(frame)
        .map(Box::new)
        .map(ObservatoryFrame::Event)
        .unwrap_or(ObservatoryFrame::Rebaseline)
}

/// Decode one SSE frame's `data:` payload as `T`.
fn parse_sse_data<T: serde::de::DeserializeOwned>(frame: &str) -> Option<T> {
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
        }
    }
    if data.is_empty() {
        return None;
    }
    serde_json::from_str::<T>(&data).ok()
}

/// Pull the `data:` payload(s) out of one SSE frame and decode the JSON
/// `AgentTurnEvent`. Ignores `id:`/`event:`/comment lines.
fn status_proves_turn_rejection(status: reqwest::StatusCode) -> bool {
    status.is_client_error() && status != reqwest::StatusCode::REQUEST_TIMEOUT
}

fn parse_sse_frame(frame: &str) -> Option<AgentTurnEvent> {
    let mut data = String::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push_str(rest.trim_start());
        }
    }
    if data.is_empty() {
        return None;
    }
    serde_json::from_str::<AgentTurnEvent>(&data).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_pre_execution_4xx_statuses_are_rejections() {
        assert!(status_proves_turn_rejection(
            reqwest::StatusCode::BAD_REQUEST
        ));
        assert!(status_proves_turn_rejection(
            reqwest::StatusCode::TOO_MANY_REQUESTS
        ));
        assert!(
            !status_proves_turn_rejection(reqwest::StatusCode::REQUEST_TIMEOUT),
            "Ocean 408 is emitted only after turn execution"
        );
        assert!(!status_proves_turn_rejection(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[test]
    fn parses_data_frame_ignoring_id_and_comments() {
        // A TurnFinished event framed the way the daemon emits it: an `id:`
        // line, a comment, then the `data:` JSON payload.
        let frame = ": keep-alive\nid: abc-123\ndata: {\"type\":\"turn_started\",\"turn_id\":\"00000000-0000-0000-0000-000000000001\",\"session_id\":\"00000000-0000-0000-0000-000000000002\"}";
        let evt = parse_sse_frame(frame).expect("should decode data payload");
        assert!(matches!(evt, AgentTurnEvent::TurnStarted { .. }));
    }

    #[test]
    fn none_when_no_data_line() {
        assert!(parse_sse_frame(": just a comment\nid: x").is_none());
    }

    #[test]
    fn recognizes_agent_stream_error_event() {
        let frame = "event: error\ndata: {\"error\":\"subscriber lagged\"}";
        assert_eq!(parse_sse_event(frame), Some("error"));
        assert!(parse_sse_frame(frame).is_none());
    }

    #[test]
    fn observatory_frames_rebaseline_on_reset_or_malformed_typed_data() {
        assert!(matches!(
            parse_observatory_frame(": keep-alive"),
            ObservatoryFrame::KeepAlive
        ));
        assert!(matches!(
            parse_observatory_frame("event: reset\ndata: {}"),
            ObservatoryFrame::Rebaseline
        ));
        assert!(matches!(
            parse_observatory_frame("event: message\ndata: {\"schema_version\":1}"),
            ObservatoryFrame::Rebaseline
        ));
    }

    #[tokio::test]
    async fn session_model_round_trip_uses_authoritative_config_routes() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind session config server");
        let address = listener.local_addr().expect("mock address");
        let session_id = AgentSessionId(uuid::Uuid::from_u128(31));
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for model in ["claude-opus-4-6", "kimi-k3"] {
                let (mut socket, _) = listener.accept().await.expect("accept config request");
                let mut request = vec![0u8; 8192];
                let read = socket
                    .read(&mut request)
                    .await
                    .expect("read config request");
                requests.push(String::from_utf8_lossy(&request[..read]).to_string());
                let body = serde_json::json!({
                    "session_id": session_id,
                    "model": model,
                    "provider": if model == "kimi-k3" { "kimi" } else { "anthropic" },
                    "model_source": "session"
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write config response");
            }
            requests
        });

        let client = DaemonClient::new(&format!("http://{address}")).expect("client");
        let loaded = client
            .session_config(session_id)
            .await
            .expect("load config");
        assert_eq!(loaded.session_id, session_id);
        assert_eq!(loaded.model, "claude-opus-4-6");
        let saved = client
            .set_session_model(session_id, "kimi-k3")
            .await
            .expect("save config");
        assert_eq!(saved.session_id, session_id);
        assert_eq!(saved.model, "kimi-k3");

        let requests = server.await.expect("mock server completed");
        assert!(requests[0].starts_with(&format!(
            "GET /v1/agent/sessions/{session_id}/config HTTP/1.1"
        )));
        assert!(requests[1].starts_with(&format!(
            "PATCH /v1/agent/sessions/{session_id}/config HTTP/1.1"
        )));
        assert!(requests[1].contains(r#"{"model":"kimi-k3"}"#));
    }

    #[tokio::test]
    async fn busy_409_decodes_typed_body_without_exposing_raw_http_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock turn server");
        let address = listener.local_addr().expect("mock address");
        let session_id = AgentSessionId(uuid::Uuid::from_u128(41));
        let turn_id = uuid::Uuid::from_u128(42);
        let body = serde_json::json!({
            "ok": false,
            "turn_id": turn_id,
            "session_id": session_id,
            "status": "failed",
            "event_id_prefix": turn_id,
            "error": "session has an active operation; try again shortly"
        })
        .to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept turn request");
            let mut request = [0u8; 8192];
            let _ = socket.read(&mut request).await.expect("read turn request");
            let response = format!(
                "HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write busy response");
        });

        let client = DaemonClient::new(&format!("http://{address}")).expect("client");
        let request = AgentTurnRequest {
            session_id: Some(session_id),
            prompt: "second prompt".into(),
            cwd: "/tmp".into(),
            guidance: None,
            project_id: None,
            client_type: Some("tui".into()),
            agent: None,
            role: None,
            thinking_level: None,
            model_id: None,
            images: None,
            decision_token: None,
            client_context: None,
            advisor: None,
        };
        let error = client
            .agent_turn_retrying(&request, |_, _| {})
            .await
            .expect_err("busy session must reject the second turn");

        assert!(matches!(error, TurnSubmitError::SessionBusy));
        assert_eq!(error.to_string(), "session is still working");
        server.await.expect("mock server completed");
    }

    #[tokio::test]
    async fn turn_posts_do_not_reuse_idle_connections() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock turn server");
        let address = listener.local_addr().expect("mock address");
        let session_id = AgentSessionId(uuid::Uuid::from_u128(51));
        let server = tokio::spawn(async move {
            // Keep the first socket open after its response. A pooled client
            // would reuse it for the second POST and hang here waiting for a
            // new accept; the turn client must instead open a fresh socket.
            let mut open_sockets = Vec::new();
            for turn in 0..2u128 {
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(2), listener.accept())
                        .await
                        .expect("turn client opened a fresh connection")
                        .expect("accept turn request");
                let mut request = [0u8; 8192];
                let _ = socket.read(&mut request).await.expect("read turn request");
                let turn_id = uuid::Uuid::from_u128(52 + turn);
                let body = serde_json::json!({
                    "ok": true,
                    "turn_id": turn_id,
                    "session_id": session_id,
                    "status": "running",
                    "event_id_prefix": turn_id.to_string()[..8].to_string()
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 202 Accepted\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write turn acknowledgement");
                open_sockets.push(socket);
            }
        });

        let client = DaemonClient::new(&format!("http://{address}")).expect("client");
        let request = AgentTurnRequest {
            session_id: Some(session_id),
            prompt: "ack test".into(),
            cwd: "/tmp".into(),
            guidance: None,
            project_id: None,
            client_type: Some("tui".into()),
            agent: None,
            role: None,
            thinking_level: None,
            model_id: None,
            images: None,
            decision_token: None,
            client_context: None,
            advisor: None,
        };
        for _ in 0..2 {
            tokio::time::timeout(
                Duration::from_secs(2),
                client.agent_turn_retrying(&request, |_, _| {}),
            )
            .await
            .expect("turn acknowledgement arrived")
            .expect("turn accepted");
        }
        server.await.expect("mock server completed");
    }

    #[tokio::test]
    async fn fenced_stream_sends_last_event_id_and_routes_error_to_reset() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock SSE server");
        let address = listener.local_addr().expect("mock address");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept SSE request");
            let mut request = vec![0u8; 8192];
            let mut used = 0usize;
            loop {
                let read = socket
                    .read(&mut request[used..])
                    .await
                    .expect("read request");
                if read == 0 {
                    break;
                }
                used += read;
                if request[..used]
                    .windows(4)
                    .any(|window| window == b"\r\n\r\n")
                {
                    break;
                }
            }
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nevent: error\ndata: {\"code\":\"anchor_unavailable\",\"reset_required\":true}\n\n",
                )
                .await
                .expect("write SSE response");
            String::from_utf8_lossy(&request[..used]).to_ascii_lowercase()
        });

        let client = DaemonClient::new(&format!("http://{address}")).expect("client");
        let session_id = AgentSessionId(uuid::Uuid::from_u128(9001));
        let fence = uuid::Uuid::from_u128(9002).to_string();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stream = client.spawn_event_stream(session_id, tx, false, Some(fence.clone()), 7, 11);

        let reset = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(Action::BoundAgentReplayResetRequired {
                    session_id: got,
                    binding_generation,
                    stream_generation,
                }) = rx.recv().await
                {
                    break (got, binding_generation, stream_generation);
                }
            }
        })
        .await
        .expect("reset action arrived");
        assert_eq!(reset, (session_id, 7, 11));
        let request = server.await.expect("mock server completed");
        assert!(
            request.contains(&format!("last-event-id: {fence}")),
            "request carried fence header: {request}"
        );
        stream.abort();
    }

    /// Live end-to-end against the local daemon: mint a session, subscribe its
    /// stream, submit a tiny turn, and require streamed events to arrive and
    /// decode. Ignored by default (needs the daemon on :4780).
    /// Run: cargo test -p ocean-tui -- --ignored --nocapture live_turn_streams
    #[tokio::test]
    #[ignore]
    async fn live_turn_streams_events() {
        let client = DaemonClient::new("http://127.0.0.1:4780").expect("client");
        client.health().await.expect("daemon must be up");

        let ws = "/tmp/ocean-tui-live-test";
        std::fs::create_dir_all(ws).unwrap();
        let sess = client
            .create_agent_session_retrying(ws, |_, _| {})
            .await
            .expect("mint session");
        println!("session: {}", sess.session_id);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _stream = client.spawn_event_stream(sess.session_id, tx, true, None, 1, 1);

        let req = AgentTurnRequest {
            session_id: Some(sess.session_id),
            prompt: "reply with exactly: ok".into(),
            cwd: ws.into(),
            guidance: None,
            project_id: None,
            client_type: Some("tui".into()),
            agent: None,
            role: None,
            thinking_level: None,
            model_id: None,
            images: None,
            decision_token: None,
            client_context: None,
            advisor: None,
        };
        let client2 = client.clone();
        let turn = tokio::spawn(async move { client2.agent_turn_retrying(&req, |_, _| {}).await });

        let mut got_started = false;
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            let ev = tokio::time::timeout_at(deadline, rx.recv()).await;
            let Ok(Some(action)) = ev else { break };
            match action {
                Action::BoundAgentEvent { event: evt, .. } => match *evt {
                    AgentTurnEvent::TurnStarted { .. } => got_started = true,
                    AgentTurnEvent::AssistantTextDelta { ref delta, .. } => {
                        text.push_str(delta);
                    }
                    AgentTurnEvent::TurnFinished { .. } => break,
                    _ => {}
                },
                Action::Status(s) => println!("status: {s}"),
                Action::Error(e) => panic!("stream error: {e}"),
                _ => {}
            }
        }
        let resp = turn.await.unwrap().expect("turn HTTP ok");
        println!("turn ok={} streamed text: {text:?}", resp.ok);
        assert!(got_started, "no TurnStarted arrived on the session stream");
        assert!(!text.is_empty(), "no assistant text streamed");
    }
}
