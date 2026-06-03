//! `ocean-acp` — an Agent Client Protocol (ACP) bridge for the Ocean daemon.
//!
//! Zed (and any other ACP editor) spawns this binary and speaks ACP to it over
//! stdio. The bridge holds **no** agent logic and **no** sessions of its own —
//! it translates ACP requests into calls against the Ocean daemon's existing
//! HTTP+SSE API (`POST /v1/agent/turns`, `GET /v1/agent/events`) and streams the
//! daemon's events back as ACP `session/update` notifications.
//!
//! ```text
//!   Zed ──ACP/stdio──▶ ocean-acp ──HTTP+SSE──▶ ocean-daemon (:4780)
//! ```
//!
//! Requirements:
//! - The Ocean daemon must already be running (default `http://127.0.0.1:4780`,
//!   override with `--daemon-url` or `OCEAN_ACP_DAEMON_URL`).
//!
//! Permissions: v1 relies on the daemon's own permission policy (it does not
//! surface tool approvals to the editor). The seam to forward
//! `session/request_permission` to Zed is noted in `prompt` below.

mod convert;
mod daemon;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::{
    AgentCapabilities, CurrentModeUpdate, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, SessionId, SessionMode, SessionModeId,
    SessionModeState, SessionNotification, SessionUpdate, SetSessionModeRequest, StopReason,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Result as AcpResult, Stdio};
use anyhow::Context;
use clap::Parser;

use convert::{event_to_update, stop_reason_for, text_block};
use daemon::{DaemonClient, DEFAULT_BASE_URL};

#[derive(Parser, Debug)]
#[command(name = "ocean-acp", about = "ACP bridge exposing the Ocean daemon to Zed and other ACP editors")]
struct Cli {
    /// Base URL of the running Ocean daemon.
    #[arg(long, env = "OCEAN_ACP_DAEMON_URL", default_value = DEFAULT_BASE_URL)]
    daemon_url: String,
}

/// Per-session state the bridge tracks locally.
///
/// The daemon owns the transcript. We track two things ACP needs but the daemon
/// doesn't hand us up front:
///  - `cwd` — ACP supplies it once at `session/new`, but the daemon wants it on
///    every turn.
///  - `daemon_id` — the daemon mints its real session id lazily on the FIRST
///    turn (it rejects client-invented ids on resume). So the ACP session id we
///    return at `session/new` is ours; we map it to the daemon's id once the
///    first turn establishes it. `None` until then.
#[derive(Clone, Default)]
struct Sessions {
    inner: Arc<Mutex<HashMap<String, SessionState>>>,
}

#[derive(Clone)]
struct SessionState {
    cwd: String,
    /// The daemon's real session id, learned from the first turn's response.
    daemon_id: Option<String>,
}

impl Sessions {
    fn insert(&self, acp_session_id: String, cwd: String) {
        self.inner.lock().expect("sessions mutex poisoned").insert(
            acp_session_id,
            SessionState {
                cwd,
                daemon_id: None,
            },
        );
    }

    fn cwd(&self, acp_session_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("sessions mutex poisoned")
            .get(acp_session_id)
            .map(|s| s.cwd.clone())
    }

    /// The daemon session id for this ACP session, if a turn has established it.
    fn daemon_id(&self, acp_session_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("sessions mutex poisoned")
            .get(acp_session_id)
            .and_then(|s| s.daemon_id.clone())
    }

    /// Record the daemon id learned from the first turn.
    fn set_daemon_id(&self, acp_session_id: &str, daemon_id: String) {
        if let Some(state) = self
            .inner
            .lock()
            .expect("sessions mutex poisoned")
            .get_mut(acp_session_id)
        {
            state.daemon_id = Some(daemon_id);
        }
    }
}

#[tokio::main]
async fn main() -> AcpResult<()> {
    // Logs go to stderr — stdout is the ACP JSON-RPC channel and must stay clean.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OCEAN_ACP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let client = DaemonClient::new(cli.daemon_url);
    let sessions = Sessions::default();

    tracing::info!(daemon = %client.base_url(), "ocean-acp starting; speaking ACP over stdio");

    Agent
        .builder()
        .name("ocean")
        // --- initialize ----------------------------------------------------
        .on_receive_request(
            {
                async move |req: InitializeRequest, responder, _conn| {
                    // Advertise the protocol version the client asked for (the
                    // SDK negotiates; echoing the request version is correct for
                    // a v1 agent). Capabilities: we can load sessions by id.
                    let mut caps = AgentCapabilities::default();
                    caps.load_session = true;
                    responder.respond(
                        InitializeResponse::new(req.protocol_version).agent_capabilities(caps),
                    )
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // --- session/new ---------------------------------------------------
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let client = client.clone();
                async move |req: NewSessionRequest, responder, _conn| {
                    // The daemon creates sessions lazily on first turn, but ACP
                    // needs a session id NOW. Generate one and reuse it as the
                    // daemon session_id on the first (and every) turn.
                    let session_id = uuid::Uuid::new_v4().to_string();
                    let cwd = req.cwd.to_string_lossy().to_string();
                    sessions.insert(session_id.clone(), cwd);

                    // Mirror the daemon's live model roster into ACP "session
                    // modes" so Zed renders a model picker. Same source the TUI
                    // and other surfaces read (`GET /v1/models`). Best-effort:
                    // if the daemon is unreachable we still open the session.
                    let modes = match client.list_models().await {
                        Ok(roster) => Some(build_mode_state(&roster)),
                        Err(err) => {
                            tracing::warn!(error = %err, "could not fetch model roster; no picker");
                            None
                        }
                    };

                    tracing::info!(%session_id, has_modes = modes.is_some(), "session/new");
                    let mut resp = NewSessionResponse::new(SessionId::new(session_id));
                    resp.modes = modes;
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // --- session/set_mode (model swap from Zed's picker) ---------------
        .on_receive_request(
            {
                let client = client.clone();
                async move |req: SetSessionModeRequest, responder, conn: ConnectionTo<Client>| {
                    // ACP mode id == Ocean model id. Swap on the daemon, then
                    // confirm back to Zed with a current-mode update.
                    let model_id = req.mode_id.0.to_string();
                    let acp_session = req.session_id.clone();
                    match client.set_model(&model_id).await {
                        Ok((provider, model)) => {
                            tracing::info!(%model_id, %provider, %model, "model swapped via session/set_mode");
                            let _ = conn.send_notification(SessionNotification::new(
                                acp_session,
                                SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                                    SessionModeId::new(model_id),
                                )),
                            ));
                            // SetSessionModeResponse is a unit-ish ack.
                            responder.respond(Default::default())
                        }
                        Err(err) => {
                            tracing::error!(%model_id, error = %err, "model swap failed");
                            responder.respond_with_error(agent_client_protocol::util::internal_error(
                                format!("model swap failed: {err:#}"),
                            ))
                        }
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // --- session/prompt ------------------------------------------------
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let client = client.clone();
                async move |req: PromptRequest, responder, conn: ConnectionTo<Client>| {
                    let session_id = req.session_id.0.to_string();
                    let prompt = flatten_prompt(&req);

                    let cwd = match sessions.cwd(&session_id) {
                        Some(cwd) => cwd,
                        None => {
                            // session/load wasn't implemented to repopulate cwd;
                            // fall back to the process cwd so we never wedge.
                            std::env::current_dir()
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_else(|_| ".".to_string())
                        }
                    };

                    let stop = match run_turn(&client, &sessions, &conn, &session_id, prompt, cwd)
                        .await
                    {
                        Ok(stop) => stop,
                        Err(err) => {
                            tracing::error!(%session_id, error = %err, "turn failed");
                            // Surface the failure to the editor as a message,
                            // then end the turn so the UI isn't left spinning.
                            let _ = conn.send_notification(SessionNotification::new(
                                SessionId::new(session_id.clone()),
                                agent_client_protocol::schema::SessionUpdate::AgentMessageChunk(
                                    agent_client_protocol::schema::ContentChunk::new(text_block(
                                        format!("⚠️ ocean-acp: {err:#}"),
                                    )),
                                ),
                            ));
                            StopReason::Refusal
                        }
                    };

                    responder.respond(PromptResponse::new(stop))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // --- everything else (cancel, load, authenticate, …) ---------------
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                // session/cancel arrives here as a notification. The daemon turn
                // will end on its own; we simply don't have a per-turn cancel
                // wired yet. Other unhandled messages get a clean error so the
                // client isn't left waiting on a response.
                message.respond_with_error(
                    agent_client_protocol::util::internal_error("unhandled message"),
                    cx,
                )
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_to(Stdio::new())
        .await
}

/// Run one turn end-to-end: subscribe to daemon events, submit the turn, pump
/// matching events to the editor as `session/update`s until the turn finishes.
async fn run_turn(
    client: &DaemonClient,
    sessions: &Sessions,
    conn: &ConnectionTo<Client>,
    acp_session_id: &str,
    prompt: String,
    cwd: String,
) -> anyhow::Result<StopReason> {
    // Resolve the daemon's session id. The daemon mints it lazily on the FIRST
    // turn (it rejects client-invented ids on resume), so:
    //   - first turn  → submit with `None`; the response carries the real id.
    //   - later turns → resume with the stored daemon id.
    let known_daemon_id = sessions.daemon_id(acp_session_id);

    // Subscribe BEFORE submitting so we can't miss early deltas (the daemon
    // stream is global and live; there's no replay).
    let mut stream = client
        .event_stream()
        .await
        .context("open daemon event stream")?;

    let submitted = client
        .submit_turn(prompt, cwd, known_daemon_id.clone())
        .await
        .context("submit turn")?;

    // The daemon's authoritative session id for this turn. On the first turn
    // this is freshly minted; persist it so later turns resume correctly.
    let daemon_session_id = submitted.session_id.0.to_string();
    if known_daemon_id.is_none() {
        sessions.set_daemon_id(acp_session_id, daemon_session_id.clone());
        tracing::info!(
            %acp_session_id,
            daemon_session_id = %daemon_session_id,
            "mapped ACP session to daemon session"
        );
    }

    let turn_id = submitted.turn_id.0.to_string();
    tracing::info!(%acp_session_id, %turn_id, status = ?submitted.status, "turn submitted");

    // If the daemon failed the turn synchronously, there will be no stream
    // events for it — surface the reason now instead of blocking forever.
    if matches!(submitted.status, ocean_agent_sdk::AgentTurnStatus::Failed) {
        let reason = submitted
            .error
            .unwrap_or_else(|| "daemon rejected the turn".to_string());
        anyhow::bail!(reason);
    }

    // ACP notifications are tagged with the ACP session id Zed knows; SSE
    // filtering uses the daemon's id the events actually carry.
    let acp_session = SessionId::new(acp_session_id.to_string());

    loop {
        let event = match stream.next_event().await? {
            Some(ev) => ev,
            None => {
                // Stream closed before the turn finished. Treat as end-of-turn
                // so the editor isn't left hanging.
                tracing::warn!(%acp_session_id, "event stream closed before turn_finished");
                return Ok(StopReason::EndTurn);
            }
        };

        // Filter to OUR session (the daemon feed is global).
        if event_session_id(&event).is_some_and(|s| s != daemon_session_id) {
            continue;
        }

        // Is this the terminal event for our turn?
        if let ocean_agent_sdk::AgentTurnEvent::TurnFinished {
            turn_id: ev_turn,
            status,
            ..
        } = &event
        {
            if ev_turn.0.to_string() == turn_id {
                tracing::info!(%acp_session_id, %turn_id, ?status, "turn finished");
                return Ok(stop_reason_for(status));
            }
            // A different turn finished on this session — ignore.
            continue;
        }

        if let Some(update) = event_to_update(&event) {
            conn.send_notification(SessionNotification::new(acp_session.clone(), update))
                .map_err(|e| anyhow::anyhow!("send session/update: {e}"))?;
        }
    }
}

/// Build an ACP [`SessionModeState`] from the daemon's model roster, so Zed
/// renders a model picker. Each Ocean model becomes an ACP "mode": the mode id
/// is the model id (what we send back on swap), the display name is the label.
fn build_mode_state(roster: &daemon::ModelsResponse) -> SessionModeState {
    let available_modes: Vec<SessionMode> = roster
        .models
        .iter()
        .map(|m| SessionMode::new(SessionModeId::new(m.id.clone()), m.display_name()))
        .collect();

    SessionModeState::new(
        SessionModeId::new(roster.current.model.clone()),
        available_modes,
    )
}

/// Concatenate the text of an ACP prompt's content blocks. Non-text blocks
/// (images, embedded resources) are summarized inline; the daemon's turn API
/// takes a single prompt string.
fn flatten_prompt(req: &PromptRequest) -> String {
    use agent_client_protocol::schema::ContentBlock;
    let mut out = String::new();
    for block in &req.prompt {
        match block {
            ContentBlock::Text(t) => out.push_str(&t.text),
            ContentBlock::ResourceLink(r) => {
                out.push_str(&format!("\n[resource: {}]\n", r.uri));
            }
            ContentBlock::Resource(_) => out.push_str("\n[embedded resource]\n"),
            ContentBlock::Image(_) => out.push_str("\n[image]\n"),
            ContentBlock::Audio(_) => out.push_str("\n[audio]\n"),
            // `ContentBlock` is #[non_exhaustive]; tolerate future variants.
            _ => out.push_str("\n[unsupported content]\n"),
        }
    }
    out
}

/// Extract the `session_id` carried by any daemon event variant, as a String.
fn event_session_id(event: &ocean_agent_sdk::AgentTurnEvent) -> Option<String> {
    use ocean_agent_sdk::AgentTurnEvent as E;
    let id = match event {
        E::TurnStarted { session_id, .. }
        | E::AssistantTextDelta { session_id, .. }
        | E::ThinkingDelta { session_id, .. }
        | E::ToolCallStarted { session_id, .. }
        | E::ToolCallChunk { session_id, .. }
        | E::ToolCallFinished { session_id, .. }
        | E::TurnFinished { session_id, .. }
        | E::SessionCreated { session_id, .. }
        | E::ComponentRender { session_id, .. }
        | E::ComponentUnmount { session_id, .. }
        | E::BrowserActivity { session_id, .. } => session_id.0.to_string(),
        E::Extension { .. } => return None,
    };
    Some(id)
}
