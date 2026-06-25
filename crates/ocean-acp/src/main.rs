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
//! Permissions: when the daemon gates a tool, it raises a `PermissionRequest`
//! on its control stream (`/v1/events`). The bridge forwards that to the editor
//! as a `session/request_permission` request, waits for Zed's allow/deny, and
//! POSTs the decision to `/v1/permissions/{id}/decision`. See [`run_turn`].
//!
//! Cancellation: a `session/cancel` notification from the editor is mapped to
//! `POST /v1/requests/{turn_id}/cancel` on the daemon (the daemon's per-turn
//! `request_id` IS the `turn_id` we get back from a turn submission). See the
//! `CancelNotification` handler below.

mod convert;
mod daemon;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::{
    AgentCapabilities, CancelNotification, CurrentModeUpdate, InitializeRequest,
    InitializeResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PermissionOption, PermissionOptionId, PermissionOptionKind, PromptRequest,
    PromptResponse, RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionMode,
    SessionModeId, SessionModeState, SessionNotification, SessionUpdate, SetSessionModeRequest,
    StopReason, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Dispatch, Result as AcpResult, Stdio};
use anyhow::Context;
use clap::Parser;

use convert::{event_to_update, stop_reason_for, text_block};
use daemon::{DaemonClient, DEFAULT_BASE_URL};

#[derive(Parser, Debug)]
#[command(
    name = "ocean-acp",
    about = "ACP bridge exposing the Ocean daemon to Zed and other ACP editors"
)]
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
///  - `daemon_id` — the daemon's real session id. OCEAN-213: `session/new` mints
///    the daemon session up front and returns its id AS the ACP id, so the two
///    are unified and `daemon_id` is set immediately (and restored on
///    `session/load` — which is what lets a resumed session actually resume the
///    persisted transcript). Legacy fallback: if up-front creation failed, the
///    ACP id is a local UUID and the daemon's id is learned lazily from the
///    first turn's event stream and claimed here; `None` until then.
#[derive(Clone, Default)]
struct Sessions {
    inner: Arc<Mutex<HashMap<String, SessionState>>>,
}

#[derive(Clone)]
struct SessionState {
    cwd: String,
    /// The daemon's real session id. Set up front at `session/new` (unified with
    /// the ACP id, OCEAN-213), restored at `session/load`, or — in the legacy
    /// fallback — learned from the first turn's event stream.
    daemon_id: Option<String>,
    /// The in-flight turn's daemon request id (== the turn id). Set while a turn
    /// is running so `session/cancel` can target `POST /v1/requests/{id}/cancel`,
    /// cleared when the turn ends. `None` between turns.
    active_request_id: Option<String>,
    /// Per-session model override chosen via `session/set_mode` (OCEAN-36).
    /// Sent as `model_id` on every turn for THIS session only, so two editor
    /// windows can each pin a different model without racing each other through
    /// the daemon's global model swap. `None` uses the daemon's global default.
    model_id: Option<String>,
}

impl Sessions {
    fn insert(&self, acp_session_id: String, cwd: String) {
        self.insert_with_daemon_id(acp_session_id, cwd, None);
    }

    /// Insert a session, optionally pre-binding its daemon id.
    ///
    /// OCEAN-213: when the bridge mints the daemon session up front (at
    /// `session/new`) or restores it (at `session/load`), the ACP id and daemon
    /// id are the SAME value, so we record `daemon_id` immediately. That makes
    /// the very first `run_turn` submit the daemon id — resuming the persisted
    /// session — instead of `None`, which would have forked a fresh transcript.
    fn insert_with_daemon_id(
        &self,
        acp_session_id: String,
        cwd: String,
        daemon_id: Option<String>,
    ) {
        self.inner.lock().expect("sessions mutex poisoned").insert(
            acp_session_id,
            SessionState {
                cwd,
                daemon_id,
                active_request_id: None,
                model_id: None,
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

    /// Atomically claim a freshly-minted daemon session id for an ACP session
    /// (OCEAN-146). On a NEW session the daemon mints the id and announces it via
    /// `SessionCreated`; the ACP bridge learns it from the global event feed
    /// rather than from the (gated-and-blocked) submit response. Because two
    /// editor windows can submit the SAME prompt concurrently, the
    /// `SessionCreated.title` is NOT unique — so the id, which IS unique, is the
    /// only safe key. This claims `daemon_id` for `acp_session_id` *iff* no other
    /// ACP session already holds it, all under one lock so concurrent first-turns
    /// each bind to a DISTINCT daemon session. Returns `true` if the claim took.
    fn try_claim_daemon_id(&self, acp_session_id: &str, daemon_id: &str) -> bool {
        let mut map = self.inner.lock().expect("sessions mutex poisoned");
        // Already owned by another ACP session → not ours; refuse.
        let taken = map
            .iter()
            .any(|(sid, st)| sid != acp_session_id && st.daemon_id.as_deref() == Some(daemon_id));
        if taken {
            return false;
        }
        if let Some(state) = map.get_mut(acp_session_id) {
            state.daemon_id = Some(daemon_id.to_string());
            true
        } else {
            false
        }
    }

    /// Mark the request id of the turn currently running for this session.
    fn set_active_request(&self, acp_session_id: &str, request_id: String) {
        if let Some(state) = self
            .inner
            .lock()
            .expect("sessions mutex poisoned")
            .get_mut(acp_session_id)
        {
            state.active_request_id = Some(request_id);
        }
    }

    /// Clear the active request id (turn ended).
    fn clear_active_request(&self, acp_session_id: &str) {
        if let Some(state) = self
            .inner
            .lock()
            .expect("sessions mutex poisoned")
            .get_mut(acp_session_id)
        {
            state.active_request_id = None;
        }
    }

    /// The request id of the turn currently running for this session, if any.
    fn active_request(&self, acp_session_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("sessions mutex poisoned")
            .get(acp_session_id)
            .and_then(|s| s.active_request_id.clone())
    }

    /// Pin a per-session model (OCEAN-36). Returns `false` if the ACP session is
    /// unknown (e.g. set_mode before session/new), so the caller can decide how
    /// to surface it.
    fn set_model_id(&self, acp_session_id: &str, model_id: String) -> bool {
        match self
            .inner
            .lock()
            .expect("sessions mutex poisoned")
            .get_mut(acp_session_id)
        {
            Some(state) => {
                state.model_id = Some(model_id);
                true
            }
            None => false,
        }
    }

    /// The per-session model override, if one was set via `session/set_mode`.
    fn model_id(&self, acp_session_id: &str) -> Option<String> {
        self.inner
            .lock()
            .expect("sessions mutex poisoned")
            .get(acp_session_id)
            .and_then(|s| s.model_id.clone())
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
                    // ACP needs a session id NOW. OCEAN-213: mint the DAEMON
                    // session up front (`POST /v1/agent/sessions`) and return the
                    // daemon's id AS the ACP session id, so the two id spaces are
                    // unified. This is what makes `session/load` actually resume
                    // after a bridge restart: the id the editor persists and
                    // replays IS the daemon id, so the cwd lookup hits the right
                    // key and the next turn resumes the persisted transcript.
                    //
                    // Pre-binding `daemon_id` here also means the first
                    // `run_turn` submits the real id (resume), not `None` (which
                    // would fork a fresh transcript). If the daemon is
                    // unreachable we fall back to a local id + lazy claim — the
                    // pre-OCEAN-213 behaviour — so `session/new` never wedges, at
                    // the cost of losing cross-restart resume for that session.
                    let req_cwd = req.cwd.to_string_lossy().to_string();
                    let (session_id, cwd) = match client.create_session(&req_cwd).await {
                        Ok(created) => {
                            let id = created.session_id.0.to_string();
                            sessions.insert_with_daemon_id(
                                id.clone(),
                                created.cwd.clone(),
                                Some(id.clone()),
                            );
                            (id, created.cwd)
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "session/new: up-front daemon session create failed; \
                                 falling back to a local id (no cross-restart resume)"
                            );
                            let id = uuid::Uuid::new_v4().to_string();
                            sessions.insert(id.clone(), req_cwd.clone());
                            (id, req_cwd)
                        }
                    };

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

                    tracing::info!(%session_id, %cwd, has_modes = modes.is_some(), "session/new");
                    let mut resp = NewSessionResponse::new(SessionId::new(session_id));
                    resp.modes = modes;
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // --- session/load (resume an existing session) ---------------------
        .on_receive_request(
            {
                let sessions = sessions.clone();
                let client = client.clone();
                async move |req: LoadSessionRequest, responder, _conn| {
                    // OCEAN-213: a real resume, not just a cwd restore. Because
                    // `session/new` now mints the daemon session up front and
                    // returns the daemon id AS the ACP id, the id the editor
                    // replays here IS the daemon id. So loading a session means:
                    //   1. confirm the daemon still has it (and get its bound cwd)
                    //   2. record `daemon_id = Some(acp_session_id)` so the next
                    //      `run_turn` submits that id and the daemon RESUMES the
                    //      persisted transcript — instead of submitting `None` and
                    //      forking a brand-new conversation.
                    // Codex P1 (#137): the earlier version restored cwd but left
                    // `daemon_id` None, so load silently started a fresh session.
                    let acp_session_id = req.session_id.0.to_string();
                    let req_cwd = req.cwd.to_string_lossy().to_string();

                    // Ask the daemon whether it knows this session, and for the
                    // workspace it bound. `Some(cwd)` ⇒ the session exists ⇒ we
                    // can resume it; `None` (404) ⇒ unknown to the daemon.
                    let daemon_cwd = match client.session_cwd(&acp_session_id).await {
                        Ok(found) => found,
                        Err(err) => {
                            tracing::warn!(
                                %acp_session_id, error = %err,
                                "session/load: daemon session lookup failed; \
                                 will restore cwd but cannot guarantee resume"
                            );
                            None
                        }
                    };

                    // cwd precedence: the daemon's bound workspace is
                    // authoritative; else the editor-supplied cwd; else the
                    // process cwd so turns never wedge.
                    let cwd = daemon_cwd.clone().unwrap_or_else(|| {
                        if !req_cwd.trim().is_empty() {
                            req_cwd.clone()
                        } else {
                            std::env::current_dir()
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_else(|_| ".".to_string())
                        }
                    });

                    // Only claim the daemon id when the daemon actually has the
                    // session. If it doesn't (legacy local-id session minted
                    // before OCEAN-213, or a session pruned off disk), we cannot
                    // honestly resume the transcript — leave `daemon_id` None so
                    // the next turn starts a fresh session rather than 404ing,
                    // and say so loudly.
                    let daemon_id = if daemon_cwd.is_some() {
                        Some(acp_session_id.clone())
                    } else {
                        tracing::warn!(
                            %acp_session_id,
                            "session/load: daemon has no such session; restoring cwd only — \
                             the next turn will start a FRESH session (no transcript resume). \
                             Likely a pre-OCEAN-213 local-id session or one pruned off disk."
                        );
                        None
                    };
                    sessions.insert_with_daemon_id(
                        acp_session_id.clone(),
                        cwd.clone(),
                        daemon_id.clone(),
                    );

                    // Mirror the model roster into session modes so the resumed
                    // session keeps its model picker, same as `session/new`.
                    let modes = match client.list_models().await {
                        Ok(roster) => Some(build_mode_state(&roster)),
                        Err(err) => {
                            tracing::warn!(error = %err, "session/load: could not fetch model roster; no picker");
                            None
                        }
                    };

                    tracing::info!(
                        %acp_session_id, %cwd,
                        resumes = daemon_id.is_some(),
                        has_modes = modes.is_some(),
                        "session/load"
                    );
                    let mut resp = LoadSessionResponse::new();
                    resp.modes = modes;
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // --- session/set_mode (per-session model selection) ----------------
        .on_receive_request(
            {
                let sessions = sessions.clone();
                async move |req: SetSessionModeRequest, responder, conn: ConnectionTo<Client>| {
                    // ACP mode id == Ocean model id. OCEAN-36: store the choice
                    // on THIS session and ride it on each turn as `model_id`,
                    // rather than swapping the daemon's GLOBAL model. The old
                    // global `set_model` made two editor windows clobber each
                    // other's model selection (a race); a per-session override
                    // is isolated, so each window keeps its own model.
                    let model_id = req.mode_id.0.to_string();
                    let acp_session = req.session_id.clone();
                    let known = sessions.set_model_id(acp_session.0.as_ref(), model_id.clone());
                    if !known {
                        // set_mode before session/new — record nothing, but still
                        // ack so the editor isn't left waiting. The session map is
                        // populated at session/new; a later set_mode will stick.
                        tracing::warn!(%model_id, "set_mode for unknown session; ack without pinning");
                    } else {
                        tracing::info!(%model_id, "model pinned for session (per-turn override)");
                    }
                    // Confirm the selection back to Zed regardless.
                    let _ = conn.send_notification(SessionNotification::new(
                        acp_session,
                        SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                            SessionModeId::new(model_id),
                        )),
                    ));
                    // SetSessionModeResponse is a unit-ish ack.
                    responder.respond(Default::default())
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
                            // Normally session/new (or session/load on resume)
                            // populates the cwd. If we still miss — a stray prompt
                            // for a session we never saw — fall back to the process
                            // cwd so we never wedge.
                            tracing::warn!(
                                %session_id,
                                "session/prompt for a session with no recorded cwd; \
                                 falling back to the process cwd"
                            );
                            std::env::current_dir()
                                .map(|p| p.to_string_lossy().to_string())
                                .unwrap_or_else(|_| ".".to_string())
                        }
                    };

                    // Run the turn OFF the event loop. A turn streams for the
                    // whole prompt; awaiting it inline would block the dispatch
                    // loop, so `session/cancel` and Zed's permission responses
                    // could never be processed mid-turn. Spawning frees the loop
                    // — `run_turn` itself calls `send_request(...).block_task()`
                    // for permission prompts, which is only safe in a spawned
                    // task. The `responder` is fulfilled when the turn ends.
                    let client = client.clone();
                    let sessions = sessions.clone();
                    conn.spawn({
                        let conn = conn.clone();
                        async move {
                            let stop = match run_turn(
                                &client, &sessions, &conn, &session_id, prompt, cwd,
                            )
                            .await
                            {
                                Ok(stop) => stop,
                                Err(err) => {
                                    tracing::error!(%session_id, error = %err, "turn failed");
                                    // Surface the failure to the editor as a
                                    // message, then end the turn so the UI isn't
                                    // left spinning.
                                    let _ = conn.send_notification(SessionNotification::new(
                                        SessionId::new(session_id.clone()),
                                        SessionUpdate::AgentMessageChunk(
                                            agent_client_protocol::schema::ContentChunk::new(
                                                text_block(format!("⚠️ ocean-acp: {err:#}")),
                                            ),
                                        ),
                                    ));
                                    StopReason::Refusal
                                }
                            };
                            // The turn is done; drop the cancel target.
                            sessions.clear_active_request(&session_id);
                            let _ = responder.respond(PromptResponse::new(stop));
                            Ok(())
                        }
                    })?;

                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // --- session/cancel ------------------------------------------------
        .on_receive_notification(
            {
                let sessions = sessions.clone();
                let client = client.clone();
                async move |notif: CancelNotification, conn: ConnectionTo<Client>| {
                    // Per-turn cancel. Map the ACP session to the in-flight
                    // turn's daemon request id and POST the cancel. The daemon's
                    // turn request_id IS the turn_id we recorded at submit time.
                    let acp_session = notif.session_id.0.to_string();
                    let Some(request_id) = sessions.active_request(&acp_session) else {
                        // No turn running for this session — nothing to cancel.
                        tracing::debug!(%acp_session, "session/cancel: no active turn");
                        return Ok(());
                    };
                    // Don't block the dispatch loop on the daemon round-trip.
                    conn.spawn({
                        let client = client.clone();
                        async move {
                            match uuid::Uuid::parse_str(&request_id) {
                                Ok(id) => {
                                    if let Err(err) = client.cancel_request(id).await {
                                        tracing::warn!(%request_id, error = %err, "cancel POST failed");
                                    } else {
                                        tracing::info!(%request_id, "per-turn cancel forwarded to daemon");
                                    }
                                }
                                Err(err) => {
                                    tracing::warn!(%request_id, error = %err, "cancel: bad request id");
                                }
                            }
                            Ok(())
                        }
                    })?;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // --- everything else (load, authenticate, …) ----------------------
        .on_receive_dispatch(
            async move |message: Dispatch, cx: ConnectionTo<Client>| {
                // Unhandled messages get a clean error so the client isn't left
                // waiting on a response. Log the method first (OCEAN-213): the
                // old silent `internal_error("unhandled message")` made a
                // missing/future ACP feature (e.g. `session/authenticate`,
                // `session/list`) invisible — you couldn't tell WHICH message
                // the editor sent that we didn't handle. `Dispatch::method()`
                // returns the JSON-RPC method for requests, notifications, and
                // responses alike.
                let method = message.method().to_string();
                tracing::warn!(%method, "ocean-acp: unhandled ACP message; responding internal_error");
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
///
/// # The permission race (OCEAN-146)
///
/// On a GATED daemon (`OCEAN_YOLO` unset = default) the daemon's
/// `/v1/agent/turns` handler does **not** return its HTTP response until the
/// whole turn finishes — `prompt(...).await` blocks INSIDE the handler while a
/// tool waits for a permission decision. So `submit_turn(...).await` does not
/// return for the entire duration of a gated turn.
///
/// The daemon raises `PermissionRequest` on its `/v1/events` control bus, which
/// is a `tokio::broadcast` channel: a subscriber only receives events emitted
/// **after** it subscribed; there is no replay. The old code awaited
/// `submit_turn` BEFORE spawning the permission bridge, so the bridge's
/// control-stream subscription was not even established when the daemon fired
/// the `PermissionRequest` — it never reached Zed and the turn hung forever.
///
/// The fix: establish the control-stream subscription (the permission bridge)
/// BEFORE `submit_turn`, alongside the agent event stream (which was already
/// subscribed first). We learn the turn's `request_id` (== `turn_id`) from the
/// agent stream's `TurnStarted`, which the daemon emits just BEFORE it can block
/// on a gate — not from `submit_turn`'s response, which would deadlock. The
/// submit runs concurrently in a spawned task so the permission round-trip can
/// unblock it.
///
/// Correlation is keyed on the daemon's AUTHORITATIVE, unique ids carried by
/// `TurnStarted` — the per-turn `turn_id` and, for a new session, the
/// `session_id` claimed atomically via [`Sessions::try_claim_daemon_id`]. We do
/// NOT key on `SessionCreated.title`, which is just the prompt prefix and is not
/// unique across concurrent same-prompt sessions.
///
/// Synchronous failures: a turn rejected up front (BAD_REQUEST on cwd / binding)
/// returns from `submit_turn` WITHOUT emitting any `AgentTurnEvent`. The event
/// loop selects over the SSE read AND the submit task so that immediate failure
/// surfaces as a failed turn instead of hanging on an event that never comes.
async fn run_turn(
    client: &DaemonClient,
    sessions: &Sessions,
    conn: &ConnectionTo<Client>,
    acp_session_id: &str,
    prompt: String,
    cwd: String,
) -> anyhow::Result<StopReason> {
    // Resolve the daemon's session id to submit with.
    //
    // OCEAN-213: `session/new` now mints the daemon session up front (via
    // `POST /v1/agent/sessions`) and records its id, so `daemon_id` is normally
    // `Some` from the very first turn — every turn (including the first) resumes
    // the already-persisted session. The legacy lazy path still applies as a
    // fallback: if up-front creation failed (daemon was unreachable at
    // `session/new`), `daemon_id` is `None`, we submit `None`, the daemon mints
    // an id, and we learn+claim it from the event stream below.
    let known_daemon_id = sessions.daemon_id(acp_session_id);
    // Per-session model override (OCEAN-36): if this session picked a model via
    // session/set_mode, ride it on the turn so the daemon drives just this turn
    // with it — no global swap, no cross-window race.
    let model_id = sessions.model_id(acp_session_id);

    // Subscribe to the agent event stream BEFORE submitting so we can't miss
    // early deltas (the daemon stream is global and live; there's no replay).
    let mut stream = client
        .event_stream()
        .await
        .context("open daemon event stream")?;

    // Subscribe to the control stream BEFORE submitting too (OCEAN-146). On a
    // gated turn the `PermissionRequest` fires while `submit_turn` is still
    // blocked; the control bus is a broadcast channel that only delivers to
    // subscribers connected before the event was emitted. Connecting here
    // guarantees the bridge is listening. We hand this already-connected stream
    // straight to the bridge below.
    let control_stream = client
        .ocean_event_stream()
        .await
        .context("open daemon control stream")?;

    // Spawn the permission bridge NOW, on the already-connected control stream.
    // It doesn't yet know our `request_id` (the daemon mints it server-side and
    // only reveals it via `TurnStarted` on the agent stream / the submit
    // response). We deliver the id over a oneshot the moment we learn it; the
    // bridge waits for it before forwarding any prompt, but its subscription is
    // already live so no `PermissionRequest` is missed in the meantime.
    let (request_id_tx, request_id_rx) = tokio::sync::oneshot::channel::<String>();

    // OCEAN-185 (P0): mint the per-turn permission secret. It rides the turn body
    // (authenticated submit path) and is replayed by the bridge on each decision
    // POST. The daemon binds the gate to it and never broadcasts it on
    // /v1/events, so a localhost page that sniffs the permission_id off the SSE
    // can't approve this turn's gated tools.
    let decision_token = ocean_core::mint_decision_token();
    spawn_permission_bridge(
        client,
        conn,
        acp_session_id.to_string(),
        control_stream,
        request_id_rx,
        decision_token.clone(),
    )?;

    // Submit the turn OFF this task so a gated `prompt().await` inside the daemon
    // can block without wedging us: we keep reading the agent stream (to learn
    // the turn id) and the bridge keeps servicing permission prompts, which is
    // what releases the daemon's block and lets `submit_turn` finally return.
    let mut submit_handle = {
        let client = client.clone();
        let known_daemon_id = known_daemon_id.clone();
        let decision_token = decision_token.clone();
        Some(tokio::spawn(async move {
            client
                .submit_turn(prompt, cwd, known_daemon_id, model_id, Some(decision_token))
                .await
        }))
    };

    // ACP notifications are tagged with the ACP session id Zed knows; SSE
    // filtering uses the daemon's id the events actually carry.
    let acp_session = SessionId::new(acp_session_id.to_string());

    // We learn the turn's identity from the first turn-scoped event the daemon
    // emits (`TurnStarted`), which it sends just before it can block on a gate —
    // so the bridge gets its `request_id` even on a gated turn where
    // `submit_turn` has not yet returned. The daemon's per-turn `request_id` IS
    // the `turn_id`, which is unique; we key everything off it. For a NEW session
    // we also learn the authoritative daemon session id from that same event's
    // `session_id` and claim it atomically (see `try_claim_daemon_id`).
    let mut turn_id: Option<String> = None;
    let mut daemon_session_id: Option<String> = known_daemon_id.clone();
    let mut request_id_tx = Some(request_id_tx);

    loop {
        // Race the SSE read against the submit task. A synchronously-rejected
        // turn (BAD_REQUEST on cwd resolution / workspace-binding guard) returns
        // an `Err`/`Failed` from `submit_turn` WITHOUT ever emitting an
        // `AgentTurnEvent` — so a plain `stream.next_event().await` would block
        // forever waiting for an event that never comes. Selecting over both
        // surfaces that immediate failure instead of hanging (OCEAN-146 P2).
        let event = tokio::select! {
            // Bias toward draining the stream first so a turn that DID start is
            // bound before we inspect a late-arriving submit result.
            biased;
            ev = stream.next_event() => match ev? {
                Some(ev) => ev,
                None => {
                    // Stream closed before the turn finished. Treat as
                    // end-of-turn so the editor isn't left hanging.
                    tracing::warn!(%acp_session_id, "event stream closed before turn_finished");
                    return Ok(StopReason::EndTurn);
                }
            },
            joined = async { submit_handle.as_mut().unwrap().await }, if submit_handle.is_some() => {
                submit_handle = None;
                match joined {
                    // Submit returned. If the turn already started (we have a
                    // turn_id), this is the NORMAL completion of a long/gated
                    // turn — keep pumping the stream for its `TurnFinished`.
                    Ok(Ok(resp)) => {
                        if turn_id.is_none() {
                            // No turn ever started → a synchronous rejection.
                            // Surface it instead of waiting on a phantom event.
                            let reason = resp
                                .error
                                .unwrap_or_else(|| "daemon rejected the turn".to_string());
                            anyhow::bail!(reason);
                        }
                        continue;
                    }
                    Ok(Err(err)) => {
                        if turn_id.is_none() {
                            return Err(err.context("submit turn"));
                        }
                        // The turn started but the HTTP request errored out
                        // afterward (e.g. transport drop); the stream still
                        // carries the authoritative `TurnFinished`, so log and
                        // keep reading rather than racing the stream to a verdict.
                        tracing::warn!(%acp_session_id, error = %err, "submit_turn errored after turn started; relying on stream");
                        continue;
                    }
                    Err(join_err) => anyhow::bail!("submit task panicked: {join_err}"),
                }
            }
        };

        // Lock onto OUR daemon session id. For a resumed session it's already
        // known and we filter strictly. For a fresh session we adopt the
        // authoritative, UNIQUE session id carried by our turn's `TurnStarted`
        // and claim it atomically — so two windows submitting the same prompt
        // concurrently can never bind to the same daemon session.
        let ev_session = event_session_id(&event);
        if let Some(known) = &daemon_session_id {
            // Strict filter once locked: drop everything for other sessions.
            if ev_session.as_deref().is_some_and(|s| s != known) {
                continue;
            }
        } else {
            // Not locked yet. Only a turn-scoped event (`TurnStarted`) binds us:
            // it carries BOTH the unique turn id and the authoritative session
            // id. A bare `SessionCreated` (no turn id) or any other session's
            // event is ignored until our turn announces itself.
            let Some(seen_session) = ev_session.as_deref() else {
                continue;
            };
            if event_turn_id(&event).is_none() {
                // e.g. a `SessionCreated` for some session — not yet our turn.
                continue;
            }
            if !sessions.try_claim_daemon_id(acp_session_id, seen_session) {
                // This session id is already owned by another ACP session (a
                // concurrent first turn claimed it). Not ours — keep looking.
                continue;
            }
            daemon_session_id = Some(seen_session.to_string());
            tracing::info!(
                %acp_session_id,
                daemon_session_id = %seen_session,
                "mapped ACP session to daemon session"
            );
        }

        // Learn the turn id (== request id) the first time we see a turn-scoped
        // event for our session, then hand it to the permission bridge and
        // record the cancel target.
        if turn_id.is_none() {
            if let Some(seen_turn) = event_turn_id(&event) {
                turn_id = Some(seen_turn.clone());
                sessions.set_active_request(acp_session_id, seen_turn.clone());
                if let Some(tx) = request_id_tx.take() {
                    // Bridge dropped early (e.g. control stream gone) → ignore.
                    let _ = tx.send(seen_turn.clone());
                }
                tracing::info!(%acp_session_id, turn_id = %seen_turn, "turn id learned from stream");
            }
        }

        // Is this the terminal event for our turn?
        if let ocean_agent_sdk::AgentTurnEvent::TurnFinished {
            turn_id: ev_turn,
            status,
            ..
        } = &event
        {
            let ev_turn = ev_turn.0.to_string();
            if turn_id.as_deref() == Some(ev_turn.as_str()) {
                tracing::info!(%acp_session_id, turn_id = %ev_turn, ?status, "turn finished");
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

/// Permission option ids the editor echoes back in its `Selected` outcome. The
/// kind drives the icon/treatment Zed shows; the id is what we match on.
const OPT_ALLOW: &str = "allow";
const OPT_DENY: &str = "deny";

/// Spawn a per-turn permission bridge.
///
/// The daemon surfaces tool approvals on its legacy control stream
/// (`/v1/events`) as `PermissionRequest` envelopes carrying a `permission_id`
/// and `request_id`. This task watches that stream for our turn's
/// `request_id`, forwards each pending approval to the editor as a
/// `session/request_permission` request, blocks for Zed's response, and POSTs
/// the resulting decision to `/v1/permissions/{id}/decision`.
///
/// OCEAN-146: the caller subscribes the control `stream` BEFORE submitting the
/// turn and hands it to us already connected, so the daemon's broadcast routes
/// the `PermissionRequest` to us even on a gated turn (where `submit_turn` stays
/// blocked the whole time). Our `request_id` isn't known yet at spawn — the
/// daemon mints it server-side and reveals it via `TurnStarted` on the agent
/// stream — so the caller delivers it over `request_id_rx` the moment it's
/// learned. We wait for it before forwarding any prompt; our subscription is
/// already live, so nothing is missed in the interim.
///
/// Note: the daemon decides the permission mode per turn via `yolo_enabled()`
/// (reads `OCEAN_YOLO`, default GATED — OCEAN-51). `AgentTurnRequest` carries no
/// `yolo` field, so ACP turns DO gate by default: a mutating tool call blocks
/// inside the daemon's `runtime.prompt(...)` and raises a `PermissionRequest` on
/// the control stream. The gating is real, and (OCEAN-146) delivery to Zed now
/// works because we subscribe the control stream before `submit_turn`.
///
/// Lifetime: the task self-terminates when it observes a terminal control
/// event (`TurnFinished` / `Cancelled` / `Error`) for our `request_id`, when the
/// control stream closes, or if the caller drops `request_id_rx` (turn never
/// established).
fn spawn_permission_bridge(
    client: &DaemonClient,
    conn: &ConnectionTo<Client>,
    acp_session_id: String,
    mut stream: daemon::OceanEventStream,
    request_id_rx: tokio::sync::oneshot::Receiver<String>,
    // OCEAN-185: the turn's per-turn secret, replayed on every decision POST so
    // the daemon binds the approval to this submitter.
    decision_token: String,
) -> anyhow::Result<()> {
    use ocean_core::OceanEvent;

    let client = client.clone();
    let bridge_conn = conn.clone();
    conn.spawn({
        let conn = bridge_conn.clone();
        async move {
            // Wait for our turn's request id, learned from the agent stream's
            // `TurnStarted`. We're already subscribed to the control bus, so any
            // `PermissionRequest` emitted before this resolves is buffered by the
            // broadcast receiver and read below once we know what to match.
            let request_id = match request_id_rx.await {
                Ok(id) => id,
                Err(_) => {
                    // Caller dropped the sender → turn never got a request id
                    // (e.g. synchronous rejection). Nothing to bridge.
                    tracing::debug!("permission bridge: turn id never delivered; exiting");
                    return Ok(());
                }
            };
            let acp_session = SessionId::new(acp_session_id.clone());

            loop {
                let envelope = match stream.next_event().await {
                    Ok(Some(ev)) => ev,
                    Ok(None) => return Ok(()), // stream closed
                    Err(err) => {
                        tracing::warn!(%request_id, error = %err, "permission bridge: stream read error");
                        return Ok(());
                    }
                };

                // Scope to OUR turn (the control feed is global).
                if envelope
                    .request_id
                    .is_some_and(|r| r.to_string() != request_id)
                {
                    continue;
                }

                match &envelope.event {
                    OceanEvent::PermissionRequest { tool, reason, .. } => {
                        let Some(permission_id) = envelope.permission_id else {
                            tracing::warn!(%request_id, "permission_request without permission_id; skipping");
                            continue;
                        };

                        // Describe the gated tool to the editor.
                        let mut fields = ToolCallUpdateFields::default();
                        fields.title = Some(format!("{tool}: {reason}"));
                        let tool_call = ToolCallUpdate::new(
                            agent_client_protocol::schema::ToolCallId::new(permission_id.to_string()),
                            fields,
                        );
                        let options = vec![
                            PermissionOption::new(
                                PermissionOptionId::new(OPT_ALLOW),
                                "Allow",
                                PermissionOptionKind::AllowOnce,
                            ),
                            PermissionOption::new(
                                PermissionOptionId::new(OPT_DENY),
                                "Reject",
                                PermissionOptionKind::RejectOnce,
                            ),
                        ];

                        // Ask Zed and wait. We're in a spawned task, so
                        // `block_task()` is safe (it does not block the event
                        // loop). A failed round-trip is treated as a denial so
                        // the daemon waiter is always released.
                        let outcome = conn
                            .send_request(RequestPermissionRequest::new(
                                acp_session.clone(),
                                tool_call,
                                options,
                            ))
                            .block_task()
                            .await;

                        let (allow, deny_reason) = match outcome {
                            Ok(resp) => match resp.outcome {
                                RequestPermissionOutcome::Selected(sel) => {
                                    (sel.option_id.0.as_ref() == OPT_ALLOW, None)
                                }
                                // Editor cancelled the turn before deciding.
                                RequestPermissionOutcome::Cancelled => (
                                    false,
                                    Some("permission request cancelled by editor".to_string()),
                                ),
                                // Forward-compatible: unknown outcome → deny.
                                _ => (false, Some("unknown permission outcome".to_string())),
                            },
                            Err(err) => {
                                tracing::warn!(%permission_id, error = %err, "request_permission failed; denying");
                                (false, Some(format!("editor permission request failed: {err}")))
                            }
                        };

                        if let Err(err) = client
                            .decide_permission(
                                permission_id,
                                allow,
                                deny_reason,
                                Some(decision_token.clone()),
                            )
                            .await
                        {
                            tracing::warn!(%permission_id, error = %err, "permission decision POST failed");
                        } else {
                            tracing::info!(%permission_id, allow, "permission decision forwarded to daemon");
                        }
                    }
                    // Terminal control events for our turn → stop watching.
                    OceanEvent::TurnFinished { .. }
                    | OceanEvent::Cancelled { .. }
                    | OceanEvent::Error { .. } => {
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
    })?;
    Ok(())
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

/// Extract the `turn_id` carried by a turn-scoped daemon event, as a String.
///
/// The daemon's per-turn `request_id` IS the `turn_id`, so this is also how the
/// permission bridge learns which `request_id` to match on the control stream
/// (OCEAN-146) — `TurnStarted` is the first turn-scoped event the daemon emits,
/// before it can block on a gate. Session-scoped-only events (e.g.
/// `SessionCreated`) and non-turn events return `None`.
fn event_turn_id(event: &ocean_agent_sdk::AgentTurnEvent) -> Option<String> {
    use ocean_agent_sdk::AgentTurnEvent as E;
    let id = match event {
        E::TurnStarted { turn_id, .. }
        | E::AssistantTextDelta { turn_id, .. }
        | E::ThinkingDelta { turn_id, .. }
        | E::ToolCallStarted { turn_id, .. }
        | E::ToolCallChunk { turn_id, .. }
        | E::ToolCallFinished { turn_id, .. }
        | E::TurnFinished { turn_id, .. } => turn_id.0.to_string(),
        _ => return None,
    };
    Some(id)
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
        | E::BrowserActivity { session_id, .. }
        | E::SurfacePatch { session_id, .. }
        | E::SlackCanvas { session_id, .. } => session_id.0.to_string(),
        E::Extension { .. } => return None,
    };
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_request_tracks_and_clears_per_session() {
        let sessions = Sessions::default();
        sessions.insert("acp-1".into(), "/tmp".into());

        // No turn yet → no cancel target.
        assert_eq!(sessions.active_request("acp-1"), None);

        sessions.set_active_request("acp-1", "req-abc".into());
        assert_eq!(sessions.active_request("acp-1"), Some("req-abc".into()));

        // A second session's turn must not leak across.
        assert_eq!(sessions.active_request("acp-2"), None);

        sessions.clear_active_request("acp-1");
        assert_eq!(sessions.active_request("acp-1"), None);
    }

    #[test]
    fn session_new_unifies_acp_and_daemon_ids_so_first_turn_resumes() {
        // OCEAN-213: `session/new` mints the daemon session up front and uses the
        // daemon id AS the ACP id, recording it as `daemon_id` immediately. The
        // VERY FIRST `run_turn` then submits that id (resume an existing
        // zero-turn session) rather than `None` (fork). Model the post-new state.
        let sessions = Sessions::default();
        // The daemon minted this id via POST /v1/agent/sessions; the bridge
        // returns it to the editor AND records it as the daemon id.
        let daemon_id = "11111111-1111-4111-8111-111111111111";
        sessions.insert_with_daemon_id(
            daemon_id.into(),
            "/work/repo".into(),
            Some(daemon_id.into()),
        );

        // run_turn submits `sessions.daemon_id(acp_id)` — it must be Some(the id)
        // so the daemon resumes instead of creating a fresh session.
        assert_eq!(
            sessions.daemon_id(daemon_id).as_deref(),
            Some(daemon_id),
            "first turn must submit the daemon id, not None"
        );
        assert_eq!(sessions.cwd(daemon_id), Some("/work/repo".into()));
    }

    #[test]
    fn session_load_restores_daemon_id_so_resume_actually_resumes() {
        // Codex P1 (#137): the earlier load handler restored cwd but left
        // `daemon_id` None — so the next turn forked a FRESH daemon session,
        // losing the loaded transcript. With unified ids, a session the daemon
        // still knows must come back with `daemon_id = Some(acp_id)`.
        let sessions = Sessions::default();
        // Bridge restarted: empty map, then session/load arrives. The ACP id IS
        // the daemon id (OCEAN-213 unification), and the daemon confirmed it has
        // the session (session_cwd returned Some), so the handler claims it.
        let acp_id = "22222222-2222-4222-8222-222222222222";
        assert_eq!(sessions.daemon_id(acp_id), None);

        sessions.insert_with_daemon_id(acp_id.into(), "/work/repo".into(), Some(acp_id.into()));

        // daemon_id is restored → run_turn submits it → daemon RESUMES.
        assert_eq!(
            sessions.daemon_id(acp_id).as_deref(),
            Some(acp_id),
            "session/load must restore daemon_id so the next turn resumes"
        );
        // …and the cwd lookup uses the same (correct) id space.
        assert_eq!(sessions.cwd(acp_id), Some("/work/repo".into()));
    }

    #[test]
    fn session_load_for_session_daemon_forgot_leaves_daemon_id_none() {
        // When the daemon has NO such session (legacy local-id session, or one
        // pruned off disk), the load handler honestly restores cwd only and
        // leaves `daemon_id` None — the next turn starts fresh rather than 404ing
        // on a resume of a session that no longer exists.
        let sessions = Sessions::default();
        let acp_id = "33333333-3333-4333-8333-333333333333";

        // session_cwd returned None ⇒ daemon doesn't know it ⇒ cwd-only restore.
        sessions.insert_with_daemon_id(acp_id.into(), "/work/repo".into(), None);

        assert_eq!(sessions.cwd(acp_id), Some("/work/repo".into()));
        assert_eq!(
            sessions.daemon_id(acp_id),
            None,
            "a session the daemon forgot must not claim a daemon id"
        );
    }

    #[test]
    fn initialize_advertises_load_session_capability() {
        // We honour session/load (it repopulates cwd), so the advertised
        // capability must stay `true`. If a future change drops the handler,
        // this guards against silently advertising an unhonored capability.
        let mut caps = agent_client_protocol::schema::AgentCapabilities::default();
        caps.load_session = true;
        assert!(
            caps.load_session,
            "ocean-acp implements session/load and must advertise it"
        );
    }

    #[test]
    fn permission_decision_serializes_to_daemon_shape() {
        use ocean_core::{PermissionDecision, PermissionDecisionRequest};

        let id = uuid::Uuid::new_v4();

        // Allow → flat `{ permission_id, decision: "allow", decision_token }`.
        let allow = PermissionDecisionRequest {
            permission_id: id,
            decision: PermissionDecision::Allow,
            decision_token: Some("secret-token".into()),
        };
        let v = serde_json::to_value(&allow).unwrap();
        assert_eq!(v["permission_id"], id.to_string());
        assert_eq!(v["decision"], "allow");
        // OCEAN-185: the per-turn secret travels on the decision body.
        assert_eq!(v["decision_token"], "secret-token");

        // Deny carries the reason inline (flattened).
        let deny = PermissionDecisionRequest {
            permission_id: id,
            decision: PermissionDecision::Deny {
                reason: Some("nope".into()),
            },
            decision_token: None,
        };
        let v = serde_json::to_value(&deny).unwrap();
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["reason"], "nope");
    }

    // OCEAN-36: two ACP sessions must keep independent model selections.
    // Before the fix, set_mode swapped the daemon's global model, so the second
    // window clobbered the first. Now each session pins its own `model_id`.
    #[test]
    fn per_session_model_is_isolated() {
        let sessions = Sessions::default();
        sessions.insert("acp-a".into(), "/proj/a".into());
        sessions.insert("acp-b".into(), "/proj/b".into());

        assert!(sessions.set_model_id("acp-a", "claude-opus-4-7".into()));
        assert!(sessions.set_model_id("acp-b", "deepseek-v4-pro".into()));

        // Neither selection bleeds into the other.
        assert_eq!(
            sessions.model_id("acp-a").as_deref(),
            Some("claude-opus-4-7")
        );
        assert_eq!(
            sessions.model_id("acp-b").as_deref(),
            Some("deepseek-v4-pro")
        );
    }

    #[test]
    fn model_defaults_to_none_until_set() {
        let sessions = Sessions::default();
        sessions.insert("acp-a".into(), "/proj/a".into());
        assert_eq!(sessions.model_id("acp-a"), None);
    }

    #[test]
    fn set_model_for_unknown_session_reports_false() {
        let sessions = Sessions::default();
        assert!(!sessions.set_model_id("missing", "kimi-k2.6".into()));
        assert_eq!(sessions.model_id("missing"), None);
    }

    // ---- OCEAN-146: unique daemon-session correlation -------------------------
    //
    // Codex P1: correlating a fresh session by `SessionCreated.title` (the prompt
    // prefix) is unsafe — two windows submitting the SAME prompt concurrently
    // share a title and would both adopt the SAME daemon session id, so one
    // session streams/cancels the other's turn. The fix keys on the daemon's
    // unique session id and claims it atomically. These tests pin that the claim
    // is mutually exclusive across ACP sessions.

    #[test]
    fn daemon_session_id_claim_is_unique_across_acp_sessions() {
        let sessions = Sessions::default();
        sessions.insert("acp-a".into(), "/proj".into());
        sessions.insert("acp-b".into(), "/proj".into());

        let daemon_sid = "11111111-1111-1111-1111-111111111111";

        // First ACP session claims the daemon id.
        assert!(sessions.try_claim_daemon_id("acp-a", daemon_sid));
        assert_eq!(sessions.daemon_id("acp-a").as_deref(), Some(daemon_sid));

        // A SECOND ACP session must NOT be able to claim the same daemon id —
        // this is the exact same-prompt collision that title-matching allowed.
        assert!(!sessions.try_claim_daemon_id("acp-b", daemon_sid));
        assert_eq!(sessions.daemon_id("acp-b"), None);
    }

    #[test]
    fn concurrent_same_prompt_sessions_bind_distinct_ids() {
        // Simulate two fresh windows submitting the same prompt: the daemon
        // mints two distinct session ids on the global feed. Each ACP session
        // claims a DIFFERENT one; neither steals the other's.
        let sessions = Sessions::default();
        sessions.insert("acp-a".into(), "/proj".into());
        sessions.insert("acp-b".into(), "/proj".into());

        let sid1 = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let sid2 = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

        // acp-a sees sid1 first and claims it.
        assert!(sessions.try_claim_daemon_id("acp-a", sid1));
        // acp-b sees sid1 too (global feed) but it's taken → must skip it...
        assert!(!sessions.try_claim_daemon_id("acp-b", sid1));
        // ...and claim the other one.
        assert!(sessions.try_claim_daemon_id("acp-b", sid2));

        assert_eq!(sessions.daemon_id("acp-a").as_deref(), Some(sid1));
        assert_eq!(sessions.daemon_id("acp-b").as_deref(), Some(sid2));
        assert_ne!(sessions.daemon_id("acp-a"), sessions.daemon_id("acp-b"));
    }

    #[test]
    fn reclaiming_own_daemon_id_is_idempotent() {
        let sessions = Sessions::default();
        sessions.insert("acp-a".into(), "/proj".into());
        let sid = "cccccccc-cccc-cccc-cccc-cccccccccccc";
        assert!(sessions.try_claim_daemon_id("acp-a", sid));
        // The same ACP session re-claiming the SAME id (e.g. a retried bind)
        // succeeds — only OTHER sessions are excluded.
        assert!(sessions.try_claim_daemon_id("acp-a", sid));
        assert_eq!(sessions.daemon_id("acp-a").as_deref(), Some(sid));
    }

    // ---- OCEAN-146: permission-bridge correlation key -------------------------
    //
    // The bridge subscribes to the control stream BEFORE submit_turn and learns
    // its `request_id` from the agent stream's `TurnStarted` (request_id ==
    // turn_id). These tests pin the helper that extracts that key, so a future
    // refactor can't silently break the subscribe-before-block fix by dropping a
    // turn-scoped variant from the correlation set.

    use ocean_agent_sdk::{
        AgentSessionId, AgentTurnEvent, AgentTurnId, AgentTurnStatus, ToolCall, ToolCallId,
        ToolResult,
    };

    fn sid() -> AgentSessionId {
        AgentSessionId(uuid::Uuid::new_v4())
    }

    #[test]
    fn turn_started_carries_the_correlation_id() {
        let tid = AgentTurnId(uuid::Uuid::new_v4());
        let ev = AgentTurnEvent::TurnStarted {
            turn_id: tid,
            session_id: sid(),
            model: None,
        };
        // This is THE event the bridge keys off — it must yield the turn id, and
        // that turn id IS the daemon's per-turn request id.
        assert_eq!(
            event_turn_id(&ev).as_deref(),
            Some(tid.0.to_string().as_str())
        );
    }

    #[test]
    fn all_turn_scoped_events_expose_the_turn_id() {
        let tid = AgentTurnId(uuid::Uuid::new_v4());
        let cases = vec![
            AgentTurnEvent::AssistantTextDelta {
                session_id: sid(),
                turn_id: tid,
                delta: "hi".into(),
            },
            AgentTurnEvent::ThinkingDelta {
                session_id: sid(),
                turn_id: tid,
                delta: "...".into(),
            },
            AgentTurnEvent::ToolCallStarted {
                session_id: sid(),
                turn_id: tid,
                call: ToolCall {
                    id: ToolCallId(uuid::Uuid::new_v4()),
                    name: "fs_write".into(),
                    args_json: serde_json::json!({}),
                },
            },
            AgentTurnEvent::ToolCallChunk {
                session_id: sid(),
                turn_id: tid,
                call_id: ToolCallId(uuid::Uuid::new_v4()),
                chunk: "x".into(),
            },
            AgentTurnEvent::ToolCallFinished {
                session_id: sid(),
                turn_id: tid,
                call_id: ToolCallId(uuid::Uuid::new_v4()),
                result: ToolResult {
                    ok: true,
                    output: "done".into(),
                    metadata_json: None,
                },
            },
            AgentTurnEvent::TurnFinished {
                session_id: sid(),
                turn_id: tid,
                status: AgentTurnStatus::Completed,
                error: None,
                wall_ms: None,
                output_tokens: None,
                input_tokens: None,
                cache_read_tokens: None,
                tokens_per_second: None,
            },
        ];
        for ev in cases {
            assert_eq!(
                event_turn_id(&ev).as_deref(),
                Some(tid.0.to_string().as_str()),
                "turn-scoped event must expose the turn id"
            );
        }
    }

    #[test]
    fn session_created_has_no_turn_id() {
        // SessionCreated is session-scoped, not turn-scoped: it announces the
        // daemon session id (which we lock onto by `title`) but carries no turn
        // id, so it must NOT be mistaken for the correlation key.
        let ev = AgentTurnEvent::SessionCreated {
            session_id: sid(),
            title: "do the thing".into(),
            cwd: "/proj".into(),
        };
        assert_eq!(event_turn_id(&ev), None);
        // It IS session-scoped, though — that's how a fresh ACP session adopts
        // the daemon id.
        assert!(event_session_id(&ev).is_some());
    }
}
