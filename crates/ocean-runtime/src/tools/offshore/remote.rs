//! HTTP-plane offshore tools: health, dispatch, sessions, events, cancel.
//!
//! All of them talk to the remote Ocean daemon at `OffshoreConfig::remote_url`.
//! Dispatch is the odd one out: the daemon runs turns synchronously — `POST
//! /v1/agent/turns` responds only when the turn finishes — so it runs under the
//! long configured turn timeout instead of the 30s control-plane deadline.

use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

use super::{OffshoreToolCtx, CONTROL_TIMEOUT};
use crate::types::{AgentTool, AgentToolResult, Concurrency};

/// Default listening window for `offshore_events`.
const EVENTS_DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Hard cap on the listening window — the stream is live-only, so a tool call
/// must never camp on it for longer than this.
const EVENTS_MAX_TIMEOUT_SECS: u64 = 300;
/// Cap on collected event bytes per call, so a chatty session can't flood the
/// transcript (the registry's artifact spill is not always on).
const EVENTS_MAX_BYTES: usize = 256 * 1024;

/// The `client_type` stamped on every dispatched turn, matching the harness.
const CLIENT_TYPE: &str = "cli";

pub struct OffshoreHealthTool {
    pub ctx: OffshoreToolCtx,
}

#[async_trait]
impl AgentTool for OffshoreHealthTool {
    fn name(&self) -> &str {
        "offshore_health"
    }
    fn description(&self) -> &str {
        "Liveness check for the offshore Ocean daemon (GET /health on the remote box over the tailnet). Run it before starting offshore work, or to distinguish 'daemon down' from 'job failed'."
    }
    fn parameters(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn execute(&self, _id: &str, _args: Value) -> Result<AgentToolResult, String> {
        let body = self
            .ctx
            .api(reqwest::Method::GET, "/health", None, CONTROL_TIMEOUT)
            .await?;
        Ok(AgentToolResult::text(body))
    }
}

pub struct OffshoreDispatchTool {
    pub ctx: OffshoreToolCtx,
}

#[async_trait]
impl AgentTool for OffshoreDispatchTool {
    fn name(&self) -> &str {
        "offshore_dispatch"
    }
    fn description(&self) -> &str {
        "Run one agent turn on the offshore Ocean daemon, in a remote working directory. SYNCHRONOUS: the call returns only when the turn finishes, which can take many minutes (bounded by the configured offshore turn timeout) — watch progress with offshore_events from another turn if needed. Only pass a cwd that offshore_workspace returned; the remote daemon does NOT validate cwd. Keep one session per job: omit session_id on the job's first dispatch (the daemon mints one — note it from the result) and reuse it on every follow-up/steering dispatch. The remote agent's work leaves the box only via git, so the prompt MUST tell it to COMMIT its work — offshore_ship/offshore_fetch move committed work only."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "cwd": { "type": "string", "description": "Remote working directory for the turn — must be a cwd returned by offshore_workspace" },
                "prompt": { "type": "string", "description": "The task for the remote agent; must tell it to commit its work" },
                "session_id": { "type": "string", "description": "Existing remote session id (omit on a job's first dispatch to mint one)" },
                "model_id": { "type": "string", "description": "Remote model_id override" },
                "thinking_level": { "type": "string", "description": "Remote thinking_level override" }
            },
            "required": ["cwd", "prompt"]
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let cwd = args
            .get("cwd")
            .and_then(|v| v.as_str())
            .ok_or("missing 'cwd'")?;
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or("missing 'prompt'")?;
        let mut body = json!({
            "prompt": prompt,
            "cwd": cwd,
            "client_type": CLIENT_TYPE,
        });
        for key in ["session_id", "model_id", "thinking_level"] {
            if let Some(v) = args.get(key).and_then(|v| v.as_str()) {
                body[key] = json!(v);
            }
        }
        let text = self
            .ctx
            .api(
                reqwest::Method::POST,
                "/v1/agent/turns",
                Some(&body),
                Duration::from_secs(self.ctx.cfg.turn_timeout_secs),
            )
            .await?;
        Ok(AgentToolResult::text(text))
    }
}

pub struct OffshoreSessionsTool {
    pub ctx: OffshoreToolCtx,
}

#[async_trait]
impl AgentTool for OffshoreSessionsTool {
    fn name(&self) -> &str {
        "offshore_sessions"
    }
    fn description(&self) -> &str {
        "List the offshore daemon's agent sessions (no args) or inspect one ('id'). Use it to recover a job's session_id or to check state after the fact — the event stream has no replay."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Session id to inspect; omit to list all sessions" }
            }
        })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let path = match args.get("id").and_then(|v| v.as_str()) {
            Some(id) => format!("/v1/agent/sessions/{id}"),
            None => "/v1/agent/sessions".to_string(),
        };
        let body = self
            .ctx
            .api(reqwest::Method::GET, &path, None, CONTROL_TIMEOUT)
            .await?;
        Ok(AgentToolResult::text(body))
    }
}

pub struct OffshoreEventsTool {
    pub ctx: OffshoreToolCtx,
}

#[async_trait]
impl AgentTool for OffshoreEventsTool {
    fn name(&self) -> &str {
        "offshore_events"
    }
    fn description(&self) -> &str {
        "Watch an offshore session's live SSE event stream for up to timeout_secs (default 30, max 300) and return the collected events. LIVE-ONLY: there is no replay — events emitted while nobody is connected are gone — so start watching while a dispatched turn runs; use offshore_sessions for after-the-fact state."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string", "description": "Remote session to watch" },
                "timeout_secs": { "type": "integer", "description": "Seconds to listen before returning (default 30, max 300)", "default": EVENTS_DEFAULT_TIMEOUT_SECS }
            },
            "required": ["session_id"]
        })
    }
    fn concurrency(&self) -> Concurrency {
        Concurrency::Shared
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let session_id = args
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'session_id'")?;
        let secs = effective_events_timeout(args.get("timeout_secs").and_then(|v| v.as_u64()));

        let path = format!("/v1/agent/events?session_id={session_id}");
        let client = self.ctx.http_client(None)?;
        let resp = client
            .get(self.ctx.url(&path))
            .send()
            .await
            .map_err(|e| format!("offshore daemon unreachable on {path}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let raw = resp.text().await.unwrap_or_default();
            return Err(format!(
                "daemon {} on {path}: {}",
                status.as_u16(),
                super::head(&raw, 400)
            ));
        }

        // Collect `data:` payloads until the deadline, the stream closing, or
        // the byte cap — whichever comes first. The deadline bounds each READ,
        // so a silent-but-open stream ends the call on time.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut events: Vec<String> = Vec::new();
        let mut note = format!("listened {secs}s");
        loop {
            match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => break, // deadline reached
                Ok(None) => {
                    note = "stream closed by the daemon".to_string();
                    break;
                }
                Ok(Some(Err(e))) => {
                    if events.is_empty() {
                        return Err(format!("reading event stream on {path}: {e}"));
                    }
                    note = format!("stream error after {} events: {e}", events.len());
                    break;
                }
                Ok(Some(Ok(chunk))) => {
                    buf.extend_from_slice(&chunk);
                    drain_sse_data(&mut buf, &mut events);
                    let collected: usize = events.iter().map(|e| e.len()).sum();
                    if collected > EVENTS_MAX_BYTES {
                        note = format!("stopped early at the {EVENTS_MAX_BYTES}-byte cap");
                        break;
                    }
                }
            }
        }

        if events.is_empty() {
            return Ok(AgentToolResult::text(format!(
                "no events within {secs}s (the stream is live-only — events emitted before this call are not replayed)"
            )));
        }
        Ok(AgentToolResult::text(format!(
            "{}\n[{} events · {note}]",
            events.join("\n"),
            events.len()
        )))
    }
}

pub struct OffshoreCancelTool {
    pub ctx: OffshoreToolCtx,
}

#[async_trait]
impl AgentTool for OffshoreCancelTool {
    fn name(&self) -> &str {
        "offshore_cancel"
    }
    fn description(&self) -> &str {
        "Cancel an in-flight offshore turn by its request_id (from the dispatch result or the event stream). Older offshore daemon builds lack this endpoint; the error says so and the turn must run out."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "request_id": { "type": "string", "description": "The in-flight request to cancel" }
            },
            "required": ["request_id"]
        })
    }
    fn requires_permission(&self) -> bool {
        true
    }
    async fn execute(&self, _id: &str, args: Value) -> Result<AgentToolResult, String> {
        let request_id = args
            .get("request_id")
            .and_then(|v| v.as_str())
            .ok_or("missing 'request_id'")?;
        let path = format!("/v1/requests/{request_id}/cancel");
        let body = self
            .ctx
            .api(reqwest::Method::POST, &path, None, CONTROL_TIMEOUT)
            .await
            .map_err(|e| {
                if e.starts_with("daemon 404") {
                    format!("{e} (this remote daemon build predates /v1/requests cancel — upgrade it or let the turn run out)")
                } else {
                    e
                }
            })?;
        Ok(AgentToolResult::text(body))
    }
}

/// The effective `offshore_events` listening window: requested seconds, default
/// [`EVENTS_DEFAULT_TIMEOUT_SECS`], clamped to `1..=`[`EVENTS_MAX_TIMEOUT_SECS`].
fn effective_events_timeout(requested: Option<u64>) -> u64 {
    requested
        .unwrap_or(EVENTS_DEFAULT_TIMEOUT_SECS)
        .clamp(1, EVENTS_MAX_TIMEOUT_SECS)
}

/// Drain complete lines out of `buf`, appending the payload of every SSE
/// `data:` line to `out` (prefix stripped, whitespace trimmed — exactly the
/// harness's `text[5:].strip()`). Non-`data:` lines (event names, comments,
/// blank keep-alives) are dropped; a trailing partial line stays buffered.
pub(crate) fn drain_sse_data(buf: &mut Vec<u8>, out: &mut Vec<String>) {
    while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = buf.drain(..=pos).collect();
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\n', '\r']);
        if let Some(data) = text.strip_prefix("data:") {
            out.push(data.trim().to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_ctx;
    use super::*;

    #[test]
    fn sse_drain_collects_data_lines_across_chunks() {
        let mut buf = Vec::new();
        let mut out = Vec::new();

        // A partial line stays buffered until its newline arrives.
        buf.extend_from_slice(b"data: {\"a\":");
        drain_sse_data(&mut buf, &mut out);
        assert!(out.is_empty());
        assert!(!buf.is_empty());

        // Completing the line (CRLF) yields the trimmed payload; event-name
        // lines and blank keep-alives are dropped.
        buf.extend_from_slice(b"1}\r\nevent: turn\n\ndata:{\"b\":2}\n");
        drain_sse_data(&mut buf, &mut out);
        assert_eq!(out, vec![r#"{"a":1}"#, r#"{"b":2}"#]);
        assert!(buf.is_empty());
    }

    #[test]
    fn events_timeout_defaults_and_caps() {
        assert_eq!(effective_events_timeout(None), 30);
        assert_eq!(effective_events_timeout(Some(120)), 120);
        assert_eq!(effective_events_timeout(Some(9_999)), 300);
        assert_eq!(effective_events_timeout(Some(0)), 1);
    }

    #[tokio::test]
    async fn dispatch_requires_cwd_and_prompt() {
        let tool = OffshoreDispatchTool { ctx: test_ctx() };
        let err = tool
            .execute("t", json!({ "prompt": "do it" }))
            .await
            .expect_err("missing cwd must error before any network call");
        assert!(err.contains("cwd"), "names the missing arg: {err}");
        let err = tool
            .execute("t", json!({ "cwd": "/home/x/offshore/jobs/j/work" }))
            .await
            .expect_err("missing prompt must error before any network call");
        assert!(err.contains("prompt"), "names the missing arg: {err}");
    }

    #[tokio::test]
    async fn events_requires_session_id() {
        let tool = OffshoreEventsTool { ctx: test_ctx() };
        let err = tool
            .execute("t", json!({}))
            .await
            .expect_err("missing session_id must error before any network call");
        assert!(err.contains("session_id"), "{err}");
    }

    #[tokio::test]
    async fn cancel_requires_request_id() {
        let tool = OffshoreCancelTool { ctx: test_ctx() };
        let err = tool
            .execute("t", json!({}))
            .await
            .expect_err("missing request_id must error before any network call");
        assert!(err.contains("request_id"), "{err}");
    }
}
