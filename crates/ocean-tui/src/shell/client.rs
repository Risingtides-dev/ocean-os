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
//! ponytail: no Last-Event-ID reconnect/replay yet — a dropped stream is
//! re-subscribed fresh on the next turn. Add replay when mid-turn resilience
//! matters (the blocking path's OCEAN-305 logic is the reference).

use std::time::Duration;

use futures::StreamExt;
use ocean_agent_sdk::{
    AgentSessionCreateRequest, AgentSessionCreateResponse, AgentSessionId, AgentTurnEvent,
    AgentTurnRequest, AgentTurnResponse,
};
use ocean_core::{
    EventEnvelope, HealthResponse, PermissionDecision, PermissionDecisionRequest, PermissionId,
};
use tokio::sync::mpsc;

use super::action::{Action, HealthSource};

/// Submission certainty matters because a turn may contain side-effecting
/// tools. Only `DefinitelyUnsent` is safe to retry/restore automatically;
/// `OutcomeUnknown` means the daemon may already be executing the prompt.
#[derive(Debug)]
pub enum TurnSubmitError {
    DefinitelyUnsent(String),
    Rejected(String),
    OutcomeUnknown(String),
}

impl std::fmt::Display for TurnSubmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DefinitelyUnsent(message)
            | Self::Rejected(message)
            | Self::OutcomeUnknown(message) => f.write_str(message),
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
        let turn_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(1800))
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
                Err(error) => {
                    return Err(TurnSubmitError::OutcomeUnknown(error.to_string()));
                }
            };

            let status = response.status();
            // Ocean uses HTTP 408 only after the runtime has actually executed
            // and emitted a failed TurnFinished. Its JSON body is therefore a
            // normal known terminal response, not an admission rejection.
            if status_proves_turn_rejection(status) {
                return Err(TurnSubmitError::Rejected(
                    response
                        .error_for_status()
                        .expect_err("4xx response must be an error")
                        .to_string(),
                ));
            }
            if status.is_server_error() {
                return Err(TurnSubmitError::OutcomeUnknown(
                    response
                        .error_for_status()
                        .expect_err("5xx response must be an error")
                        .to_string(),
                ));
            }
            return response
                .json::<AgentTurnResponse>()
                .await
                .map_err(|error| TurnSubmitError::OutcomeUnknown(error.to_string()));
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
            let mut last_event_id: Option<String> = None;
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
                match req.send().await.and_then(|r| r.error_for_status()) {
                    Ok(resp) => {
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
                                if let Some(evt) = parse_sse_frame(&frame) {
                                    let _ = actions.send(Action::AgentEvent(Box::new(evt)));
                                }
                            }
                        }
                    }
                    Err(_) => {}
                }
                // Dropped (or failed to connect): mark the SSE source degraded,
                // brief backoff, then resubscribe with the last seen id so the
                // daemon replays what we missed. The typed transition persists
                // until THIS source reconnects — unrelated notices can't clear it.
                let _ = actions.send(Action::HealthDegraded {
                    source: HealthSource::Sse,
                    condition: "stream reconnecting".into(),
                });
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
    }
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
        let _stream = client.spawn_event_stream(sess.session_id, tx, true);

        let req = AgentTurnRequest {
            session_id: Some(sess.session_id),
            prompt: "reply with exactly: ok".into(),
            cwd: ws.into(),
            guidance: None,
            room_id: None,
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
                Action::AgentEvent(evt) => match *evt {
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
