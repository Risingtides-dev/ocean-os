//! Thin async client for the Ocean daemon's agent HTTP+SSE API.
//!
//! The daemon owns all agent logic and sessions. This client does two things:
//!   1. `submit_turn` — POST a prompt to `/v1/agent/turns`.
//!   2. `event_stream` — subscribe to the daemon's GLOBAL `/v1/agent/events`
//!      SSE feed and yield decoded [`AgentTurnEvent`]s.
//!
//! Note: `/v1/agent/events` is **not** session-scoped on the daemon side — it
//! is one broadcast bus for every session. Callers MUST filter by `session_id`
//! themselves (every event variant carries one). See
//! `crates/ocean-daemon/src/main.rs::agent_events`.

use anyhow::{Context, Result};
use futures::{Stream, StreamExt};
use ocean_agent_sdk::{AgentTurnEvent, AgentTurnRequest, AgentTurnResponse};
use serde::Deserialize;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio_util::io::StreamReader;

/// Default daemon bind address (matches `OCEAN_BIND` default in the daemon).
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:4780";

/// Client surface tag reported to the daemon so it can tailor responses.
const CLIENT_TYPE: &str = "acp-zed";

/// A boxed byte stream that yields `io::Result<Bytes>`, suitable for `StreamReader`.
type IoByteStream = Pin<Box<dyn Stream<Item = std::io::Result<bytes::Bytes>> + Send>>;

#[derive(Clone)]
pub struct DaemonClient {
    base_url: String,
    http: reqwest::Client,
}

impl DaemonClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            // No global timeout: the SSE stream is long-lived. Per-request
            // timeouts could be applied on the unary calls if needed.
            http: reqwest::Client::new(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Fetch the daemon's model roster + currently selected model.
    /// Mirrors `GET /v1/models` — the same endpoint the TUI/surfaces read.
    pub async fn list_models(&self) -> Result<ModelsResponse> {
        let url = format!("{}/v1/models", self.base_url);
        self.http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .context("daemon rejected models list")?
            .json::<ModelsResponse>()
            .await
            .context("decode models response")
    }

    /// Swap the daemon's active model. Mirrors `POST /v1/model { model }`.
    /// Returns the now-current `(provider, model)` on success.
    pub async fn set_model(&self, model: &str) -> Result<(String, String)> {
        let url = format!("{}/v1/model", self.base_url);
        let resp: ModelSetResponse = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "model": model }))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .context("daemon rejected model swap")?
            .json()
            .await
            .context("decode model-set response")?;

        if !resp.ok {
            anyhow::bail!(resp.error.unwrap_or_else(|| "model swap failed".into()));
        }
        Ok((
            resp.provider.unwrap_or_default(),
            resp.model.unwrap_or_else(|| model.to_string()),
        ))
    }

    /// Submit a turn. The daemon creates a session lazily when `session_id`
    /// is `None`; in that case the real id arrives via the SSE
    /// `session_created` / `turn_started` events and in the response body.
    pub async fn submit_turn(
        &self,
        prompt: String,
        cwd: String,
        session_id: Option<String>,
    ) -> Result<AgentTurnResponse> {
        let body = AgentTurnRequest {
            session_id: session_id
                .as_deref()
                .map(parse_session_id)
                .transpose()?
                .map(ocean_agent_sdk::AgentSessionId),
            prompt,
            cwd,
            guidance: None,
            room_id: None,
            project_id: None,
            client_type: Some(CLIENT_TYPE.to_string()),
        };

        let url = format!("{}/v1/agent/turns", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!(
                    "POST {url} (is the Ocean daemon running on {}?)",
                    self.base_url
                )
            })?
            .error_for_status()
            .context("daemon rejected the turn")?;

        resp.json::<AgentTurnResponse>()
            .await
            .context("decode AgentTurnResponse")
    }

    /// Open the daemon's global agent event stream. Yields every
    /// [`AgentTurnEvent`] for every session; filter by `session_id` downstream.
    pub async fn event_stream(&self) -> Result<EventStream> {
        let url = format!("{}/v1/agent/events", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .with_context(|| format!("GET {url} (SSE)"))?
            .error_for_status()
            .context("daemon rejected the event subscription")?;

        // reqwest byte stream -> io::Result -> AsyncRead -> line reader. SSE
        // frames are newline-delimited `field: value` lines; we only care
        // about `data:`.
        let byte_stream: IoByteStream = Box::pin(
            resp.bytes_stream()
                .map(|res| res.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))),
        );
        let lines = BufReader::new(StreamReader::new(byte_stream)).lines();
        Ok(EventStream { lines })
    }
}

/// A stream of decoded [`AgentTurnEvent`]s parsed from the daemon SSE feed.
///
/// The daemon frames each event as:
/// ```text
/// id: <uuid>
/// event: <type_name>
/// data: <json of AgentTurnEvent>
/// <blank line>
/// ```
/// The `data:` JSON is the fully self-describing tagged enum, so we ignore the
/// `event:` line and decode `data:` directly. Unparseable / unknown payloads
/// are skipped to stay forward-compatible.
pub struct EventStream {
    lines: Lines<BufReader<StreamReader<IoByteStream, bytes::Bytes>>>,
}

impl EventStream {
    /// Pull the next decoded event, or `Ok(None)` at end-of-stream.
    pub async fn next_event(&mut self) -> Result<Option<AgentTurnEvent>> {
        while let Some(line) = self.lines.next_line().await.context("read SSE line")? {
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = rest.trim();
            if payload.is_empty() {
                continue;
            }
            match serde_json::from_str::<AgentTurnEvent>(payload) {
                Ok(ev) => return Ok(Some(ev)),
                // Daemon emits `{"type":"error",...}` control frames and may add
                // new event kinds; skip anything we can't decode.
                Err(_) => continue,
            }
        }
        Ok(None)
    }
}

fn parse_session_id(s: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).with_context(|| format!("invalid session id: {s:?}"))
}

// --- model roster types (mirror of the daemon's /v1/models payload) ---------

/// One selectable model, as the daemon reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

impl ModelInfo {
    /// Human-facing name for the model picker: the daemon's label (else id),
    /// suffixed with the provider when it isn't already implied by the label.
    pub fn display_name(&self) -> String {
        let base = self.label.clone().unwrap_or_else(|| self.id.clone());
        match &self.provider {
            Some(p) if !base.to_lowercase().contains(&p.to_lowercase()) => format!("{base} · {p}"),
            _ => base,
        }
    }
}

/// The currently-selected model.
#[derive(Debug, Clone, Deserialize)]
pub struct CurrentModel {
    /// Provider backing the current model (kept for parity / future display).
    #[serde(default)]
    #[allow(dead_code)]
    pub provider: Option<String>,
    pub model: String,
}

/// Response body of `GET /v1/models`.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelsResponse {
    pub current: CurrentModel,
    #[serde(default)]
    pub models: Vec<ModelInfo>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelSetResponse {
    ok: bool,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    error: Option<String>,
}
