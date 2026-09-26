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
use ocean_agent_sdk::{
    AgentSessionCreateRequest, AgentSessionCreateResponse, AgentSessionsResponse, AgentTurnEvent,
    AgentTurnRequest, AgentTurnResponse, ThinkingLevel,
};
use ocean_core::{EventEnvelope, PermissionDecision, PermissionDecisionRequest};
use serde::Deserialize;
use std::path::Path;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio_util::io::StreamReader;

/// Default daemon bind address (matches `OCEAN_BIND` default in the daemon).
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:4780";

/// Client surface tag reported to the daemon so it can tailor responses.
const CLIENT_TYPE: &str = "acp-zed";

// The daemon routes, query fields and literal keys this client uses. Each is
// held inside `docs/contracts/session-wire.json` by
// `daemon_routes_are_inside_the_published_session_wire`.
const MODELS_PATH: &str = "/v1/models";
const MODEL_SET_PATH: &str = "/v1/model";
const MODEL_SET_FIELD: &str = "model";
const SESSIONS_PATH: &str = "/v1/agent/sessions";
const SESSION_DETAIL_PATH: &str = "/v1/agent/sessions/{id}";
const SESSION_LIST_CWD_QUERY: &str = "cwd";
const SESSION_LIST_CURSOR_QUERY: &str = "cursor";
const AGENT_TURNS_PATH: &str = "/v1/agent/turns";
const AGENT_EVENTS_PATH: &str = "/v1/agent/events";
const AGENT_EVENTS_ALL_QUERY: &str = "all";
const EVENTS_PATH: &str = "/v1/events";
const PERMISSION_DECISION_PATH: &str = "/v1/permissions/{id}/decision";
const REQUEST_CANCEL_PATH: &str = "/v1/requests/{id}/cancel";

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
        let url = format!("{}{MODELS_PATH}", self.base_url);
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

    /// Swap the daemon's active *global* model. Mirrors `POST /v1/model
    /// { model }` and returns the now-current `(provider, model)` on success.
    ///
    /// OCEAN-36: ocean-acp no longer calls this on `session/set_mode` — that
    /// global swap raced two editor windows against each other. Model selection
    /// now rides per-turn via `AgentTurnRequest::model_id`. This method is kept
    /// as a thin client for the still-supported global endpoint (e.g. an
    /// operator CLI), hence `allow(dead_code)`.
    #[allow(dead_code)]
    pub async fn set_model(&self, model: &str) -> Result<(String, String)> {
        let url = format!("{}{MODEL_SET_PATH}", self.base_url);
        let resp: ModelSetResponse = self
            .http
            .post(&url)
            .json(&model_set_body(model))
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

    /// Create (mint + persist) a daemon session up front, binding it to `cwd`.
    /// Mirrors `POST /v1/agent/sessions`. Returns the daemon's freshly-minted
    /// session id and the workspace-bound cwd.
    ///
    /// OCEAN-213: the ACP bridge calls this at `session/new` so the ACP session
    /// id IT returns to the editor IS the daemon's id. Unifying the two id
    /// spaces is what makes `session/load` actually resume after a bridge
    /// restart — the id the editor replays is the daemon id, so the cwd lookup
    /// hits the right key and the next turn resumes the persisted transcript
    /// instead of silently forking a fresh one. Without this, the bridge minted
    /// a *local* id the daemon never knew, and the daemon→ACP mapping (learned
    /// lazily, held only in memory) was lost on restart.
    pub async fn create_session(&self, cwd: &str) -> Result<AgentSessionCreateResponse> {
        let url = format!("{}{SESSIONS_PATH}", self.base_url);
        let body = session_create_request(cwd);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .context("daemon rejected session create")?
            .json::<AgentSessionCreateResponse>()
            .await
            .context("decode session-create response")?;
        // The daemon returns an empty cwd + new id on a failed bind (it never
        // 4xxs the create on a bad cwd). Treat an empty cwd as a failure so the
        // caller falls back to its local id instead of binding to nothing.
        anyhow::ensure!(
            !resp.cwd.trim().is_empty(),
            "daemon session create returned an empty cwd (bind failed)"
        );
        Ok(resp)
    }

    /// Fetch a session's recorded working directory from the daemon.
    /// Mirrors `GET /v1/agent/sessions/{id}`, whose `AgentSession.cwd` the
    /// daemon resolves from the bound workspace root (falling back to the
    /// recorded cwd). Used by `session/load` to repopulate the bridge's
    /// per-session cwd after a bridge restart, instead of guessing with
    /// `env::current_dir()`. Returns `Ok(None)` if the session is unknown or
    /// carries no cwd.
    pub async fn session_cwd(&self, session_id: &str) -> Result<Option<String>> {
        let url = format!(
            "{}{}",
            self.base_url,
            SESSION_DETAIL_PATH.replace("{id}", session_id)
        );
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        // A missing session is not an error here — the caller can fall back to
        // the cwd the editor supplied on the load request.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp
            .error_for_status()
            .context("daemon rejected session detail read")?
            .json::<AgentSessionResponse>()
            .await
            .context("decode session detail response")?;
        Ok(resp
            .session
            .map(|s| s.cwd)
            .filter(|cwd| !cwd.trim().is_empty()))
    }

    /// List daemon sessions, optionally scoped to the ACP request cwd.
    /// Mirrors `GET /v1/agent/sessions`, preserving daemon pagination.
    pub async fn list_sessions(
        &self,
        cwd: Option<&Path>,
        cursor: Option<&str>,
    ) -> Result<AgentSessionsResponse> {
        let mut url = reqwest::Url::parse(&format!("{}{SESSIONS_PATH}", self.base_url))
            .context("build sessions URL")?;
        {
            let mut query = url.query_pairs_mut();
            // Filter an empty/whitespace cwd the same way the cursor is
            // filtered below: a type-legal `Some(PathBuf::from(""))` from an
            // ACP client must mean "no filter", not `?cwd=` (which the daemon
            // would treat as a real — and never-matching — workspace filter).
            if let Some(cwd) = cwd
                .map(|c| c.to_string_lossy().into_owned())
                .filter(|value| !value.trim().is_empty())
            {
                query.append_pair(SESSION_LIST_CWD_QUERY, &cwd);
            }
            if let Some(cursor) = cursor.filter(|value| !value.trim().is_empty()) {
                query.append_pair(SESSION_LIST_CURSOR_QUERY, cursor);
            }
        }

        let resp = self
            .http
            .get(url.clone())
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()
            .context("daemon rejected sessions list")?
            .json::<AgentSessionsResponse>()
            .await
            .context("decode sessions response")?;
        if !resp.ok {
            anyhow::bail!(
                "{}",
                resp.error
                    .unwrap_or_else(|| "daemon sessions list failed".to_string())
            );
        }
        Ok(resp)
    }

    /// Submit a turn. The daemon creates a session lazily when `session_id`
    /// is `None`; in that case the real id arrives via the SSE
    /// `session_created` / `turn_started` events and in the response body.
    pub async fn submit_turn(
        &self,
        prompt: String,
        cwd: String,
        session_id: Option<String>,
        model_id: Option<String>,
        thinking_level: Option<ThinkingLevel>,
        // OCEAN-185 (P0): per-turn permission secret minted by the bridge. The
        // daemon binds the gate to it and never broadcasts it; the bridge replays
        // it on each decision POST so the decision is bound to this submitter.
        decision_token: Option<String>,
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
            project_id: None,
            client_type: Some(CLIENT_TYPE.to_string()),
            // Per-turn reasoning override from ACP metadata; None preserves the
            // daemon's global thinking_level.
            thinking_level,
            // Per-session model override (OCEAN-36): drives this turn only.
            model_id,
            // ACP bridge does not use named model-role indirection.
            role: None,
            // ACP bridge does not attach images to turns (yet).
            images: None,
            decision_token,
            // ACP bridge does not select a named folder-as-agent.
            agent: None,
            // ACP bridge is not an in-browser surface (OCEAN-40).
            client_context: None,
            advisor: None,
        };

        let url = format!("{}{AGENT_TURNS_PATH}", self.base_url);
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
    ///
    /// The `?all=1` is required: the daemon's default `/v1/agent/events` scoping
    /// (OCEAN-15) drops every session-bearing event for a query-less subscriber
    /// so product surfaces can't adopt another surface's session. The ACP bridge
    /// is the explicit legacy/debug exception — it subscribes BEFORE it knows the
    /// daemon session id (that arrives in the submit response) and filters by
    /// session id downstream, so it must opt into the full firehose with `all=1`
    /// or it never sees its own turn's deltas / `TurnFinished` and the editor
    /// hangs. This is the daemon-sanctioned escape hatch and re-opens no bleed
    /// for first-party surfaces, which keep using a scoped `?session_id`.
    pub async fn event_stream(&self) -> Result<EventStream> {
        let url = format!(
            "{}{AGENT_EVENTS_PATH}?{AGENT_EVENTS_ALL_QUERY}=1",
            self.base_url
        );
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
                .map(|res| res.map_err(std::io::Error::other)),
        );
        let lines = BufReader::new(StreamReader::new(byte_stream)).lines();
        Ok(EventStream { lines })
    }

    /// Open the daemon's GLOBAL legacy control stream (`/v1/events`). Unlike
    /// `/v1/agent/events` (typed `AgentTurnEvent`s), this feed carries the
    /// permission lifecycle — `PermissionRequest` / `PermissionDecision` — that
    /// the agent stream omits. The bridge watches it during a turn so it can
    /// forward a pending tool approval to the editor. Yields decoded
    /// [`EventEnvelope`]s; filter by `request_id` / `session_id` downstream.
    pub async fn ocean_event_stream(&self) -> Result<OceanEventStream> {
        let url = format!("{}{EVENTS_PATH}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .with_context(|| format!("GET {url} (SSE)"))?
            .error_for_status()
            .context("daemon rejected the control event subscription")?;

        let byte_stream: IoByteStream = Box::pin(
            resp.bytes_stream()
                .map(|res| res.map_err(std::io::Error::other)),
        );
        let lines = BufReader::new(StreamReader::new(byte_stream)).lines();
        Ok(OceanEventStream { lines })
    }

    /// Resolve a pending permission by id. Mirrors
    /// `POST /v1/permissions/{id}/decision`. `allow == true` sends `Allow`,
    /// otherwise `Deny { reason }`. This releases the daemon-side waiter the
    /// agent loop is blocked on.
    pub async fn decide_permission(
        &self,
        permission_id: uuid::Uuid,
        allow: bool,
        reason: Option<String>,
        // OCEAN-185: the per-turn secret minted at submit time. Replayed here so
        // the daemon can bind this decision to the turn's submitter; a missing or
        // wrong token is rejected 403.
        decision_token: Option<String>,
    ) -> Result<()> {
        let body = permission_decision_body(permission_id, allow, reason, decision_token);
        let url = format!(
            "{}{}",
            self.base_url,
            PERMISSION_DECISION_PATH.replace("{id}", &permission_id.to_string())
        );
        self.http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .context("daemon rejected the permission decision")?;
        Ok(())
    }

    /// Cancel an in-flight request (a turn). Mirrors
    /// `POST /v1/requests/{id}/cancel`. The daemon's turn `request_id` IS the
    /// `turn_id` returned by `submit_turn`, so callers pass that through.
    pub async fn cancel_request(&self, request_id: uuid::Uuid) -> Result<()> {
        let url = format!(
            "{}{}",
            self.base_url,
            REQUEST_CANCEL_PATH.replace("{id}", &request_id.to_string())
        );
        self.http
            .post(&url)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?
            .error_for_status()
            .context("daemon rejected the cancel request")?;
        Ok(())
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
                // new event kinds; skip anything we can't decode — but log it
                // (OCEAN-101) so a new/unknown event variant isn't an invisible
                // drop that silently never reaches the editor.
                Err(e) => {
                    tracing::debug!(error = %e, payload, "skipping undecodable daemon AgentTurnEvent frame");
                    continue;
                }
            }
        }
        Ok(None)
    }
}

/// A stream of decoded [`EventEnvelope`]s parsed from the daemon's legacy
/// `/v1/events` SSE feed. Same line framing as [`EventStream`]; we decode the
/// `data:` payload as the flattened control envelope and skip anything that
/// isn't a recognisable envelope (e.g. `{"type":"error",…}` keep-alives).
pub struct OceanEventStream {
    lines: Lines<BufReader<StreamReader<IoByteStream, bytes::Bytes>>>,
}

impl OceanEventStream {
    /// Pull the next decoded control envelope, or `Ok(None)` at end-of-stream.
    pub async fn next_event(&mut self) -> Result<Option<EventEnvelope>> {
        while let Some(line) = self.lines.next_line().await.context("read SSE line")? {
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = rest.trim();
            if payload.is_empty() {
                continue;
            }
            match serde_json::from_str::<EventEnvelope>(payload) {
                Ok(ev) => return Ok(Some(ev)),
                Err(e) => {
                    tracing::debug!(error = %e, payload, "skipping undecodable daemon EventEnvelope frame");
                    continue;
                }
            }
        }
        Ok(None)
    }
}

/// The `POST /v1/agent/sessions` body the bridge sends. Its keys are held
/// inside `docs/contracts/session-wire.json` by
/// `session_create_is_inside_the_published_session_wire`.
fn session_create_request(cwd: &str) -> AgentSessionCreateRequest {
    AgentSessionCreateRequest {
        workspace_root: cwd.to_string(),
        project_id: None,
        model: None,
        client_type: Some(CLIENT_TYPE.to_string()),
        title: None,
    }
}

/// The `POST /v1/model` body the bridge sends.
fn model_set_body(model: &str) -> serde_json::Value {
    serde_json::json!({ MODEL_SET_FIELD: model })
}

/// The `POST /v1/permissions/{id}/decision` body the bridge sends.
fn permission_decision_body(
    permission_id: uuid::Uuid,
    allow: bool,
    reason: Option<String>,
    decision_token: Option<String>,
) -> PermissionDecisionRequest {
    PermissionDecisionRequest {
        permission_id,
        decision: if allow {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Deny { reason }
        },
        decision_token,
    }
}

fn parse_session_id(s: &str) -> Result<uuid::Uuid> {
    uuid::Uuid::parse_str(s).with_context(|| format!("invalid session id: {s:?}"))
}

// --- session detail types (subset of the daemon's /v1/agent/sessions/{id}) --

/// Just the `cwd` we need off `GET /v1/agent/sessions/{id}`'s `session`
/// object. Deliberately a partial mirror: the real payload carries title,
/// timestamps, turns, etc., but the ACP bridge only needs the working dir to
/// repopulate a resumed session, so we decode the one field and let serde
/// ignore the rest.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct SessionDetailSlice {
    #[serde(default)]
    cwd: String,
}

/// Subset of `GET /v1/agent/sessions/{id}`'s response body.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct AgentSessionResponse {
    #[serde(default)]
    session: Option<SessionDetailSlice>,
}

// --- model roster types (mirror of the daemon's /v1/models payload) ---------

/// One selectable model, as the daemon reports it.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
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
#[cfg_attr(test, derive(serde::Serialize))]
pub struct CurrentModel {
    /// Provider backing the current model (kept for parity / future display).
    #[serde(default)]
    #[allow(dead_code)]
    pub provider: Option<String>,
    pub model: String,
}

/// Response body of `GET /v1/models`.
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub struct ModelsResponse {
    pub current: CurrentModel,
    #[serde(default)]
    pub models: Vec<ModelInfo>,
}

/// Decoded body of `POST /v1/model`. Only used by [`DaemonClient::set_model`],
/// which OCEAN-36 left in place for the global endpoint but no longer calls on
/// the per-session path; hence `allow(dead_code)`.
#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct ModelSetResponse {
    ok: bool,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[cfg(test)]
mod contract_tests {
    use super::*;

    /// Consumer half of the session contract: the create body the bridge
    /// sends, its `client_type`, and the create response it decodes (it
    /// relies on `cwd`) stay inside the published contract.
    #[test]
    fn session_create_is_inside_the_published_session_wire() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/contracts/session-wire.json"))
                .expect("session-wire.json parses");
        let published = |field: &str| -> Vec<String> {
            wire[field]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect()
        };
        let request_fields = published("session_create_request_fields");
        let sent = serde_json::to_value(session_create_request("/w")).unwrap();
        for key in sent.as_object().unwrap().keys() {
            assert!(
                request_fields.contains(key),
                "the bridge sends unpublished session-create field {key}"
            );
        }
        let known = wire["session_create_client_types"]["known"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert!(
            known.contains(&CLIENT_TYPE.to_string()),
            "{CLIENT_TYPE} is a published client type"
        );
        let response_keys = published("session_create_response_keys");
        assert!(response_keys.contains(&"cwd".to_string()));
        let decoded = serde_json::to_value(AgentSessionCreateResponse {
            session_id: ocean_agent_sdk::AgentSessionId(uuid::Uuid::nil()),
            cwd: "/w".into(),
            client_type: Some(CLIENT_TYPE.into()),
        })
        .unwrap();
        for key in decoded.as_object().unwrap().keys() {
            assert!(
                response_keys.contains(key),
                "the bridge decodes unpublished session-create key {key}"
            );
        }
    }

    /// Records the field names a struct's `Deserialize` derive hands its
    /// deserializer, renames applied: the probe the daemon's contract tests
    /// use, so both sides read the names from the types.
    struct FieldNames<'a>(&'a mut Option<&'static [&'static str]>);

    impl<'de> serde::Deserializer<'de> for FieldNames<'_> {
        type Error = serde::de::value::Error;

        fn deserialize_any<V: serde::de::Visitor<'de>>(
            self,
            _: V,
        ) -> Result<V::Value, Self::Error> {
            Err(serde::de::Error::custom("serde name probe"))
        }

        fn deserialize_struct<V: serde::de::Visitor<'de>>(
            self,
            _: &'static str,
            fields: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Self::Error> {
            *self.0 = Some(fields);
            Err(serde::de::Error::custom("serde name probe"))
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map identifier ignored_any enum
        }
    }

    fn field_names<T: serde::de::DeserializeOwned>() -> Vec<String> {
        let mut captured = None;
        let _ = T::deserialize(FieldNames(&mut captured));
        captured
            .unwrap_or_else(|| panic!("{} exposes no field names", std::any::type_name::<T>()))
            .iter()
            .map(|name| (*name).to_string())
            .collect()
    }

    /// Consumer half of the rest of the session contract: every other daemon
    /// route the bridge calls is published, with the bodies and query fields
    /// it sends and the keys it decodes.
    #[test]
    fn daemon_routes_are_inside_the_published_session_wire() {
        let wire: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/contracts/session-wire.json"))
                .expect("session-wire.json parses");
        let inside = |section: &str, field: &str, names: Vec<String>| {
            let published: Vec<&str> = wire[section][field]
                .as_array()
                .unwrap_or_else(|| panic!("{section}.{field} is a list"))
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            for name in names {
                assert!(
                    published.contains(&name.as_str()),
                    "the bridge relies on unpublished {section}.{field} entry {name}"
                );
            }
        };
        let keys = |value: serde_json::Value| -> Vec<String> {
            value.as_object().unwrap().keys().cloned().collect()
        };
        let route = |section: &str, method: &str, path: &str| {
            assert_eq!(
                wire[section]["route"],
                format!("{method} {path}"),
                "{section}"
            );
        };

        assert_eq!(
            wire["session_create_route"],
            format!("POST {SESSIONS_PATH}")
        );
        route("models", "GET", MODELS_PATH);
        let decoded = serde_json::to_value(ModelsResponse {
            current: CurrentModel {
                provider: Some("p".into()),
                model: "m".into(),
            },
            models: vec![ModelInfo {
                id: "i".into(),
                label: Some("l".into()),
                provider: Some("p".into()),
            }],
        })
        .unwrap();
        inside("models", "response_fields", keys(decoded.clone()));
        inside("models", "current_fields", keys(decoded["current"].clone()));
        inside("models", "model_fields", keys(decoded["models"][0].clone()));

        route("model_set", "POST", MODEL_SET_PATH);
        inside("model_set", "request_fields", keys(model_set_body("m")));
        inside(
            "model_set",
            "response_fields",
            field_names::<ModelSetResponse>(),
        );

        route("session_detail", "GET", SESSION_DETAIL_PATH);
        inside(
            "session_detail",
            "response_fields",
            field_names::<AgentSessionResponse>(),
        );
        inside(
            "session_detail",
            "session_fields",
            field_names::<SessionDetailSlice>(),
        );

        route("session_list", "GET", SESSIONS_PATH);
        inside(
            "session_list",
            "query_fields",
            vec![
                SESSION_LIST_CWD_QUERY.to_string(),
                SESSION_LIST_CURSOR_QUERY.to_string(),
            ],
        );
        inside(
            "session_list",
            "response_fields",
            field_names::<AgentSessionsResponse>(),
        );
        inside(
            "session_list",
            "summary_fields",
            field_names::<ocean_agent_sdk::AgentSessionSummary>(),
        );

        route("agent_turn", "POST", AGENT_TURNS_PATH);
        inside(
            "agent_turn",
            "request_fields",
            field_names::<AgentTurnRequest>(),
        );
        inside(
            "agent_turn",
            "response_fields",
            field_names::<AgentTurnResponse>(),
        );

        assert_eq!(
            wire["agent_events_route"],
            format!("GET {AGENT_EVENTS_PATH}")
        );
        let query: Vec<&str> = wire["agent_events_query_fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            query.contains(&AGENT_EVENTS_ALL_QUERY),
            "{AGENT_EVENTS_ALL_QUERY} is published"
        );

        // GET /v1/events: the shared envelope decoder knows every published
        // event type, every event the bridge branches on is published, and
        // the envelope it decodes is inside the published fields.
        route("events", "GET", EVENTS_PATH);
        let bridge = include_str!("main.rs");
        let mut branched = Vec::new();
        for (at, needle) in bridge.match_indices("OceanEvent::") {
            let name: String = bridge[at + needle.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
                continue;
            }
            let mut snake = String::new();
            for (i, c) in name.chars().enumerate() {
                if c.is_ascii_uppercase() && i > 0 {
                    snake.push('_');
                }
                snake.push(c.to_ascii_lowercase());
            }
            branched.push(snake);
        }
        assert!(
            branched.iter().any(|name| name == "permission_request"),
            "the scan found the bridge's matches"
        );
        inside("events", "event_types", branched);
        for name in wire["events"]["event_types"].as_array().unwrap() {
            if let Err(error) = serde_json::from_value::<ocean_core::OceanEvent>(
                serde_json::json!({ "type": name }),
            ) {
                assert!(
                    !error.to_string().contains("unknown variant"),
                    "the bridge's event decoder does not know published type {name}: {error}"
                );
            }
        }
        let mut envelope = EventEnvelope::new(ocean_core::OceanEvent::SessionCreated);
        envelope.session_id = Some(uuid::Uuid::nil());
        envelope.request_id = Some(uuid::Uuid::nil());
        envelope.permission_id = Some(uuid::Uuid::nil());
        envelope.origin = Some("o".into());
        let mut decoded = serde_json::to_value(envelope).unwrap();
        decoded.as_object_mut().unwrap().remove("type");
        inside("events", "envelope_fields", keys(decoded));

        route("permission_decision", "POST", PERMISSION_DECISION_PATH);
        for allow in [true, false] {
            let body = serde_json::to_value(permission_decision_body(
                uuid::Uuid::nil(),
                allow,
                Some("r".into()),
                Some("t".into()),
            ))
            .unwrap();
            inside("permission_decision", "request_fields", keys(body.clone()));
            inside(
                "permission_decision",
                "decisions",
                vec![body["decision"].as_str().unwrap().to_string()],
            );
        }

        route("request_cancel", "POST", REQUEST_CANCEL_PATH);
    }
}
