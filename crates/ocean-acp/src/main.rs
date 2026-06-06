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
    InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionId, PermissionOptionKind, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionId, SessionMode, SessionModeId,
    SessionModeState, SessionNotification, SessionUpdate, SetSessionModeRequest, StopReason,
    ToolCallUpdate, ToolCallUpdateFields,
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
        self.inner.lock().expect("sessions mutex poisoned").insert(
            acp_session_id,
            SessionState {
                cwd,
                daemon_id: None,
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
                    let known = sessions.set_model_id(&acp_session.0.to_string(), model_id.clone());
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
                            // session/load wasn't implemented to repopulate cwd;
                            // fall back to the process cwd so we never wedge.
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
                // waiting on a response.
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
    //   - first turn  → submit with `None`; the id arrives via the events.
    //   - later turns → resume with the stored daemon id.
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
    spawn_permission_bridge(
        client,
        conn,
        acp_session_id.to_string(),
        control_stream,
        request_id_rx,
    )?;

    // For a NEW session the daemon mints the session id and announces it with a
    // `SessionCreated` carrying the prompt's first 60 chars as `title` (see
    // `ocean-daemon::agent_turn`). We match on that title to lock onto OUR
    // session even if another fresh session is being created concurrently on the
    // global feed. (We deliberately do NOT match on cwd: the daemon's
    // SessionCreated reports the *resolved* cwd, which can differ from the raw
    // path we sent after path normalisation / workspace binding.)
    let expected_title: String = prompt.chars().take(60).collect();

    // Submit the turn OFF this task so a gated `prompt().await` inside the daemon
    // can block without wedging us: we keep reading the agent stream (to learn
    // the turn id) and the bridge keeps servicing permission prompts, which is
    // what releases the daemon's block and lets `submit_turn` finally return.
    let mut submit_handle = {
        let client = client.clone();
        let known_daemon_id = known_daemon_id.clone();
        Some(tokio::spawn(async move {
            client
                .submit_turn(prompt, cwd, known_daemon_id, model_id)
                .await
        }))
    };

    // ACP notifications are tagged with the ACP session id Zed knows; SSE
    // filtering uses the daemon's id the events actually carry.
    let acp_session = SessionId::new(acp_session_id.to_string());

    // We learn the turn's identity from the first event the daemon emits for
    // this turn (`SessionCreated` / `TurnStarted`), which it sends just before
    // it can block on a gate — so the bridge gets its `request_id` even on a
    // gated turn where `submit_turn` has not yet returned. Once known we lock
    // onto that turn id for the rest of the loop.
    let mut turn_id: Option<String> = None;
    let mut daemon_session_id: Option<String> = known_daemon_id.clone();
    let mut request_id_tx = Some(request_id_tx);

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

        // Lock onto OUR daemon session id. For a resumed session it's already
        // known and we filter strictly. For a fresh session we adopt the id from
        // the `SessionCreated` whose title matches our prompt — disambiguating
        // concurrent new sessions on the global feed.
        let ev_session = event_session_id(&event);
        if let Some(known) = &daemon_session_id {
            // Strict filter once locked: drop everything for other sessions.
            if ev_session.as_deref().is_some_and(|s| s != known) {
                continue;
            }
        } else {
            // Not locked yet: only a matching `SessionCreated` may adopt the id.
            match &event {
                ocean_agent_sdk::AgentTurnEvent::SessionCreated {
                    session_id, title, ..
                } if *title == expected_title => {
                    let seen = session_id.0.to_string();
                    daemon_session_id = Some(seen.clone());
                    sessions.set_daemon_id(acp_session_id, seen.clone());
                    tracing::info!(
                        %acp_session_id,
                        daemon_session_id = %seen,
                        "mapped ACP session to daemon session"
                    );
                }
                // Any other event before we've locked our session belongs to a
                // different session (or is a global SessionCreated for someone
                // else) — ignore it.
                _ => continue,
            }
        }

        // Learn the turn id (== request id) the first time we see a turn-scoped
        // event for our session, then hand it to the permission bridge and
        // record the cancel target. The daemon's per-turn request id IS the
        // turn id (see `ocean-daemon::agent_turn`).
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

        // If the daemon rejected the turn synchronously (e.g. bad session on
        // resume), `submit_turn` returns `Failed`/errors and no stream events
        // will arrive for it. Surface that instead of blocking forever. We only
        // check the handle opportunistically once it has finished, so a normal
        // long-running (or gated, still-blocked) turn is never disturbed.
        if let Some(handle) = submit_handle.as_mut() {
            if handle.is_finished() {
                let joined = submit_handle.take().unwrap().await;
                match joined {
                    Ok(Ok(resp)) => {
                        if matches!(resp.status, ocean_agent_sdk::AgentTurnStatus::Failed)
                            && turn_id.is_none()
                        {
                            let reason = resp
                                .error
                                .unwrap_or_else(|| "daemon rejected the turn".to_string());
                            anyhow::bail!(reason);
                        }
                    }
                    Ok(Err(err)) => return Err(err.context("submit turn")),
                    Err(join_err) => anyhow::bail!("submit task panicked: {join_err}"),
                }
            }
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
                            .decide_permission(permission_id, allow, deny_reason)
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
        | E::BrowserActivity { session_id, .. } => session_id.0.to_string(),
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
    fn permission_decision_serializes_to_daemon_shape() {
        use ocean_core::{PermissionDecision, PermissionDecisionRequest};

        let id = uuid::Uuid::new_v4();

        // Allow → flat `{ permission_id, decision: "allow" }`.
        let allow = PermissionDecisionRequest {
            permission_id: id,
            decision: PermissionDecision::Allow,
        };
        let v = serde_json::to_value(&allow).unwrap();
        assert_eq!(v["permission_id"], id.to_string());
        assert_eq!(v["decision"], "allow");

        // Deny carries the reason inline (flattened).
        let deny = PermissionDecisionRequest {
            permission_id: id,
            decision: PermissionDecision::Deny {
                reason: Some("nope".into()),
            },
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
        assert_eq!(sessions.model_id("acp-a").as_deref(), Some("claude-opus-4-7"));
        assert_eq!(sessions.model_id("acp-b").as_deref(), Some("deepseek-v4-pro"));
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
        assert_eq!(event_turn_id(&ev).as_deref(), Some(tid.0.to_string().as_str()));
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
