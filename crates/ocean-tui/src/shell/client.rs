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

use super::action::Action;

#[derive(Clone)]
pub struct DaemonClient {
    http: reqwest::Client,
    base: String,
}

impl DaemonClient {
    pub fn new(base_url: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        Ok(Self {
            http,
            base: base_url.trim_end_matches('/').to_string(),
        })
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

    pub async fn create_agent_session(
        &self,
        workspace_root: &str,
    ) -> Result<AgentSessionCreateResponse, String> {
        self.create_agent_session_retrying(workspace_root, |_, _| {})
            .await
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

    pub async fn agent_turn(
        &self,
        req: &AgentTurnRequest,
    ) -> Result<AgentTurnResponse, String> {
        self.agent_turn_retrying(req, |_, _| {}).await
    }

    /// Submit a turn, riding out a daemon blip (restart/redeploy) instead of
    /// failing the operator's prompt with a hard error. Retries ONLY
    /// connect-class failures — connection refused/reset means the request
    /// never reached a daemon, so a retry can't double-submit a turn. HTTP
    /// status errors, timeouts, and decode errors surface immediately (the
    /// daemon spoke, or may have started processing — retrying those is not
    /// idempotent-safe). `on_retry(attempt, total)` fires before each backoff
    /// sleep so the caller can show "retrying…" progress. The schedule spans
    /// ~15.5s, comfortably covering the ~8s launchd respawn.
    pub async fn agent_turn_retrying(
        &self,
        req: &AgentTurnRequest,
        on_retry: impl FnMut(usize, usize),
    ) -> Result<AgentTurnResponse, String> {
        let url = format!("{}/v1/agent/turns", self.base);
        self.post_json_retrying(&url, req, on_retry).await
    }

    /// POST `body` as JSON and decode a JSON response, retrying connect-class
    /// transport failures on [`RETRY_DELAYS_MS`] backoff. Shared engine for the
    /// turn + session-mint paths.
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
                // Connection refused/reset: the daemon is down or mid-restart
                // and the request never landed — safe to retry.
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
            let mut first = true;
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
                        let _ = actions.send(Action::Status(if first {
                            "stream connected".into()
                        } else {
                            "stream reconnected".into()
                        }));
                        first = false;
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
                    Err(e) => {
                        if first {
                            let _ = actions.send(Action::Error(format!("stream: {e}")));
                            first = false;
                        }
                    }
                }
                // Dropped (or failed to connect): brief backoff, then resubscribe
                // with the last seen id so the daemon replays what we missed.
                let _ = actions.send(Action::Status("stream reconnecting…".into()));
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
        let sess = client.create_agent_session(ws).await.expect("mint session");
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
        };
        let client2 = client.clone();
        let turn = tokio::spawn(async move { client2.agent_turn(&req).await });

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
