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
        let req = AgentSessionCreateRequest {
            workspace_root: workspace_root.to_string(),
            project_id: None,
            client_type: Some("tui".into()),
        };
        self.http
            .post(format!("{}/v1/agent/sessions", self.base))
            .json(&req)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?
            .json::<AgentSessionCreateResponse>()
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn agent_turn(
        &self,
        req: &AgentTurnRequest,
    ) -> Result<AgentTurnResponse, String> {
        self.http
            .post(format!("{}/v1/agent/turns", self.base))
            .json(req)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| e.to_string())?
            .json::<AgentTurnResponse>()
            .await
            .map_err(|e| e.to_string())
    }

    /// Subscribe to `/v1/agent/events` scoped to `session_id`, forwarding each
    /// decoded `AgentTurnEvent` onto the action channel until the stream ends.
    /// Spawned as a background task; returns immediately.
    pub fn spawn_event_stream(
        &self,
        session_id: AgentSessionId,
        actions: mpsc::UnboundedSender<Action>,
    ) {
        let http = self.http.clone();
        let url = format!(
            "{}/v1/agent/events?session_id={}",
            self.base, session_id
        );
        tokio::spawn(async move {
            let resp = match http.get(&url).send().await.and_then(|r| r.error_for_status()) {
                Ok(r) => r,
                Err(e) => {
                    let _ = actions.send(Action::Error(format!("stream: {e}")));
                    return;
                }
            };
            let _ = actions.send(Action::Status("stream connected".into()));
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();
            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(_) => break, // ponytail: drop on read error; next turn re-subscribes
                };
                buf.push_str(&String::from_utf8_lossy(&bytes));
                // SSE frames are separated by a blank line.
                while let Some(idx) = buf.find("\n\n") {
                    let frame = buf[..idx].to_string();
                    buf.drain(..idx + 2);
                    if let Some(evt) = parse_sse_frame(&frame) {
                        let _ = actions.send(Action::AgentEvent(Box::new(evt)));
                    }
                }
            }
            let _ = actions.send(Action::Status("stream ended".into()));
        });
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
            let resp = match http.get(&url).send().await.and_then(|r| r.error_for_status()) {
                Ok(r) => r,
                Err(e) => {
                    let _ = actions.send(Action::Error(format!("events: {e}")));
                    return;
                }
            };
            let mut stream = resp.bytes_stream();
            let mut buf = String::new();
            while let Some(chunk) = stream.next().await {
                let bytes = match chunk {
                    Ok(b) => b,
                    Err(_) => break,
                };
                buf.push_str(&String::from_utf8_lossy(&bytes));
                while let Some(idx) = buf.find("\n\n") {
                    let frame = buf[..idx].to_string();
                    buf.drain(..idx + 2);
                    if let Some(env) = parse_sse_data::<EventEnvelope>(&frame) {
                        let _ = actions.send(Action::OceanEvent(Box::new(env)));
                    }
                }
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
        client.spawn_event_stream(sess.session_id, tx);

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
