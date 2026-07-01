//! OCEAN-146 — integration proof that the ACP permission bridge subscribes to
//! the daemon's control stream BEFORE a gated turn can block on a permission
//! decision.
//!
//! ## Why this test exists
//!
//! The daemon's `/v1/agent/turns` handler does not return its HTTP response
//! until `prompt(...).await` finishes. On a GATED daemon (`OCEAN_YOLO` unset),
//! that future blocks INSIDE the handler while a tool waits for a permission
//! decision. Meanwhile the daemon raises `PermissionRequest` on its `/v1/events`
//! control bus — a `tokio::broadcast` channel that only delivers to subscribers
//! that connected BEFORE the event was emitted (no replay).
//!
//! The old `run_turn` awaited `submit_turn` before spawning the permission
//! bridge, so the bridge's control-stream subscription wasn't established when
//! the `PermissionRequest` fired → the prompt never reached the editor → the
//! turn hung forever. The fix subscribes the control stream before submit.
//!
//! ## What this proves, honestly
//!
//! A full ACP+Zed+real-LLM harness is impractical headless (a real gated tool
//! call needs a live model with credentials). Instead this drives a MOCK daemon
//! that faithfully reproduces the two behaviours that cause the bug:
//!   1. `/v1/events` is a real broadcast channel (subscribe-after-emit misses).
//!   2. `/v1/agent/turns` blocks until a decision is POSTed to
//!      `/v1/permissions/{id}/decision`, exactly like a gated `prompt().await`.
//!
//! Against that daemon we exercise the REAL `DaemonClient` (the same HTTP/SSE
//! client `run_turn`/`spawn_permission_bridge` use):
//!   - `old_order_misses_permission_and_would_hang` — subscribe AFTER submit:
//!     the `PermissionRequest` is missed and the turn never unblocks (the bug).
//!   - `new_order_receives_permission_and_turn_completes` — subscribe BEFORE
//!     submit: the bridge receives the `PermissionRequest`, posts allow, and the
//!     turn completes. This is the fixed ordering `run_turn` now uses.
//!   - `synchronous_submit_failure_does_not_hang` — a BAD_REQUEST reject emits
//!     NO events; the `select!` over (SSE read, submit task) surfaces the failure
//!     instead of hanging on an event that never comes (OCEAN-146 P2).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use ocean_agent_sdk::{
    AgentSessionId, AgentTurnEvent, AgentTurnId, AgentTurnResponse, AgentTurnStatus,
};
use ocean_core::{EventEnvelope, OceanEvent};
use serde_json::Value;
use tokio::sync::{broadcast, oneshot, Mutex};
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt as _;
use uuid::Uuid;

// The bridge's HTTP/SSE client lives in the binary crate, so re-declare the
// `daemon` module here against the same source to drive the real client code.
// (Only a subset of the client surface is used here; the rest is dead in this
// compilation unit.)
#[path = "../src/daemon.rs"]
#[allow(dead_code)]
mod daemon;
use daemon::DaemonClient;

/// Shared state for the mock gated daemon.
struct MockState {
    /// Control bus (`/v1/events`) — real broadcast, so subscribe timing matters.
    control: broadcast::Sender<EventEnvelope>,
    /// Agent bus (`/v1/agent/events`) — typed turn events.
    agent: broadcast::Sender<AgentTurnEvent>,
    /// Set when a decision is POSTed; releases the blocked turn.
    decision: Mutex<Option<oneshot::Sender<bool>>>,
    session_id: Uuid,
    turn_id: Uuid,
    permission_id: Uuid,
}

async fn control_sse(
    State(state): State<Arc<MockState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let rx = state.control.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| {
        res.ok()
            .map(|env| Ok(SseEvent::default().data(serde_json::to_string(&env).unwrap())))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn agent_sse(
    State(state): State<Arc<MockState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<SseEvent, std::convert::Infallible>>> {
    let rx = state.agent.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|res| {
        res.ok()
            .map(|ev| Ok(SseEvent::default().data(serde_json::to_string(&ev).unwrap())))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// The gated turn: announce the turn, raise a permission request, then BLOCK
/// until a decision arrives — exactly like the daemon's `prompt().await` on a
/// gated tool call. The HTTP response is only sent after the block clears.
async fn agent_turn(
    State(state): State<Arc<MockState>>,
    Json(_req): Json<Value>,
) -> Json<AgentTurnResponse> {
    let session_id = AgentSessionId(state.session_id);
    let turn_id = AgentTurnId(state.turn_id);

    // Emit the turn-scoped events the bridge uses to learn its request id. These
    // fire BEFORE the block, mirroring the real daemon.
    let _ = state.agent.send(AgentTurnEvent::SessionCreated {
        session_id,
        title: "mutate a file".into(),
        cwd: "/proj".into(),
    });
    let _ = state.agent.send(AgentTurnEvent::TurnStarted {
        turn_id,
        session_id,
        model: None,
    });

    // Raise the permission request on the CONTROL bus. A subscriber that
    // connected before now receives it; one that connects later does not.
    let mut env = EventEnvelope::new(OceanEvent::PermissionRequest {
        tool: "fs_write".into(),
        reason: "write /proj/out.txt".into(),
        args: serde_json::json!({ "path": "/proj/out.txt" }),
    });
    env.session_id = Some(state.session_id);
    env.request_id = Some(state.turn_id);
    env.permission_id = Some(state.permission_id);
    let _ = state.control.send(env);

    // BLOCK until a decision is posted (or give up after a bounded wait so a
    // missed-permission run fails fast instead of hanging the test process).
    let (tx, rx) = oneshot::channel::<bool>();
    *state.decision.lock().await = Some(tx);
    let allowed = match tokio::time::timeout(Duration::from_secs(3), rx).await {
        Ok(Ok(allowed)) => allowed,
        _ => {
            // No decision arrived → the turn would hang forever in the real
            // daemon. Report a failed turn so the caller can assert the bug.
            let _ = state.agent.send(AgentTurnEvent::TurnFinished {
                session_id,
                turn_id,
                status: AgentTurnStatus::Failed,
                error: Some("permission never decided (turn would hang)".into()),
                wall_ms: None,
                output_tokens: None,
                input_tokens: None,
                cache_read_tokens: None,
                tokens_per_second: None,
            });
            return Json(AgentTurnResponse {
                ok: false,
                turn_id,
                session_id,
                status: AgentTurnStatus::Failed,
                event_id_prefix: String::new(),
                error: Some("permission never decided".into()),
                output_tokens: None,
                input_tokens: None,
                cache_read_tokens: None,
                tokens_per_second: None,
                wall_ms: None,
            });
        }
    };

    // Decision arrived → the turn unblocks and completes.
    let _ = state.agent.send(AgentTurnEvent::TurnFinished {
        session_id,
        turn_id,
        status: AgentTurnStatus::Completed,
        error: None,
        wall_ms: Some(1),
        output_tokens: Some(1),
        input_tokens: None,
        cache_read_tokens: None,
        tokens_per_second: None,
    });
    Json(AgentTurnResponse {
        ok: allowed,
        turn_id,
        session_id,
        status: AgentTurnStatus::Completed,
        event_id_prefix: String::new(),
        error: None,
        output_tokens: None,
        input_tokens: None,
        cache_read_tokens: None,
        tokens_per_second: None,
        wall_ms: None,
    })
}

async fn decide(
    State(state): State<Arc<MockState>>,
    Path(_id): Path<String>,
    Json(_body): Json<Value>,
) -> Json<Value> {
    if let Some(tx) = state.decision.lock().await.take() {
        let _ = tx.send(true);
    }
    Json(serde_json::json!({ "ok": true }))
}

/// A turn the daemon rejects SYNCHRONOUSLY, before emitting any AgentTurnEvent —
/// e.g. cwd resolution / workspace-binding guard failures return BAD_REQUEST
/// straight from the handler. No `SessionCreated`/`TurnStarted` ever hits the
/// agent stream. This is the precondition for the OCEAN-146 P2 hang: a loop that
/// only advances on stream events would wait forever. `submit_turn` surfaces it
/// as an `Err` (non-2xx), which `run_turn`'s `tokio::select!` arm turns into a
/// failed turn instead of a hang.
async fn agent_turn_reject() -> (axum::http::StatusCode, Json<Value>) {
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(serde_json::json!({
            "ok": false,
            "error": "cwd resolves outside the session workspace"
        })),
    )
}

/// Boot the mock gated daemon on an ephemeral port (NEVER :4780). Returns its
/// base URL and the shared state.
async fn spawn_mock_daemon() -> (String, Arc<MockState>) {
    let (control, _) = broadcast::channel(64);
    let (agent, _) = broadcast::channel(64);
    let state = Arc::new(MockState {
        control,
        agent,
        decision: Mutex::new(None),
        session_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        permission_id: Uuid::new_v4(),
    });

    let app = Router::new()
        .route("/v1/events", get(control_sse))
        .route("/v1/agent/events", get(agent_sse))
        .route("/v1/agent/turns", post(agent_turn))
        .route("/v1/permissions/{id}/decision", post(decide))
        .with_state(state.clone());

    // Bind to an OS-assigned port on loopback — guaranteed not to be :4780.
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let bound = listener.local_addr().unwrap();
    assert_ne!(bound.port(), 4780, "test daemon must never use :4780");
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{bound}"), state)
}

/// Boot a mock daemon whose `/v1/agent/turns` rejects synchronously (no events).
async fn spawn_rejecting_daemon() -> String {
    let (control, _) = broadcast::channel::<EventEnvelope>(8);
    let (agent, _) = broadcast::channel::<AgentTurnEvent>(8);
    let state = Arc::new(MockState {
        control,
        agent,
        decision: Mutex::new(None),
        session_id: Uuid::new_v4(),
        turn_id: Uuid::new_v4(),
        permission_id: Uuid::new_v4(),
    });
    let app = Router::new()
        .route("/v1/events", get(control_sse))
        .route("/v1/agent/events", get(agent_sse))
        .route("/v1/agent/turns", post(agent_turn_reject))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let bound = listener.local_addr().unwrap();
    assert_ne!(bound.port(), 4780, "test daemon must never use :4780");
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{bound}")
}

/// Wait until an SSE control subscriber is actually registered on the broadcast
/// channel — i.e. the GET has connected and the daemon's `subscribe()` ran.
async fn await_control_subscriber(state: &MockState) {
    for _ in 0..200 {
        if state.control.receiver_count() > 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("control subscriber never registered");
}

/// THE BUG: subscribe to the control stream AFTER submitting the turn. Because
/// the daemon blocks inside `/v1/agent/turns` and the `PermissionRequest` is a
/// broadcast emitted at submit time, a later subscriber misses it — and with
/// nobody to decide the permission, the turn never unblocks (it would hang).
#[tokio::test]
async fn old_order_misses_permission_and_would_hang() {
    let (base, _state) = spawn_mock_daemon().await;
    let client = DaemonClient::new(base);

    // Fire the (blocking) turn first. We DO NOT await it — it blocks on the gate.
    let submit = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .submit_turn(
                    "mutate a file".into(),
                    "/proj".into(),
                    None,
                    None,
                    None,
                    Some(ocean_core::mint_decision_token()),
                )
                .await
        })
    };

    // Let the turn handler run far enough to emit the PermissionRequest.
    tokio::time::sleep(Duration::from_millis(150)).await;

    // NOW subscribe to the control stream (the OLD, buggy ordering).
    let mut control = client.ocean_event_stream().await.expect("control stream");

    // We must not receive the already-emitted PermissionRequest: a broadcast
    // subscriber gets nothing emitted before it joined.
    let got = tokio::time::timeout(Duration::from_millis(500), control.next_event()).await;
    assert!(
        got.is_err() || matches!(got, Ok(Ok(None))),
        "buggy ordering must MISS the permission request (broadcast has no replay)"
    );

    // Nobody decided the permission → the turn cannot complete. Confirm it is
    // still blocked well past when a fixed run would have finished.
    assert!(
        !submit.is_finished(),
        "with the permission missed, the gated turn must still be hung"
    );

    submit.abort();
}

/// THE FIX: subscribe to the control stream BEFORE submitting. The bridge is
/// listening when the daemon raises the PermissionRequest, so it receives it,
/// posts an allow decision, and the gated turn unblocks to completion.
#[tokio::test]
async fn new_order_receives_permission_and_turn_completes() {
    let (base, state) = spawn_mock_daemon().await;
    let client = DaemonClient::new(base);

    // 1) Subscribe to BOTH streams BEFORE submit — exactly what `run_turn` does.
    let mut agent = client.event_stream().await.expect("agent stream");
    let mut control = client.ocean_event_stream().await.expect("control stream");
    // Make sure the control subscription is actually live before we submit so
    // the broadcast can route the PermissionRequest to us.
    await_control_subscriber(&state).await;

    // 2) Submit the (blocking) gated turn off-task.
    let submit = {
        let client = client.clone();
        tokio::spawn(async move {
            client
                .submit_turn(
                    "mutate a file".into(),
                    "/proj".into(),
                    None,
                    None,
                    None,
                    Some(ocean_core::mint_decision_token()),
                )
                .await
        })
    };

    // 3) The bridge would learn its request id from the agent stream's
    //    TurnStarted. Drain until we see it (proves the correlation key arrives
    //    BEFORE the turn returns).
    let mut learned_request_id: Option<String> = None;
    for _ in 0..50 {
        if let Ok(Some(AgentTurnEvent::TurnStarted { turn_id, .. })) =
            tokio::time::timeout(Duration::from_millis(500), agent.next_event())
                .await
                .unwrap()
        {
            learned_request_id = Some(turn_id.0.to_string());
            break;
        }
    }
    let request_id = learned_request_id.expect("must learn request id from TurnStarted pre-block");

    // 4) The bridge receives the PermissionRequest on the control stream,
    //    scoped to our request id.
    let mut got_permission: Option<Uuid> = None;
    for _ in 0..50 {
        let ev = tokio::time::timeout(Duration::from_secs(1), control.next_event())
            .await
            .expect("permission must arrive (fix established the subscription first)")
            .expect("control stream read")
            .expect("control envelope");
        if ev.request_id.map(|r| r.to_string()).as_deref() != Some(request_id.as_str()) {
            continue;
        }
        if let OceanEvent::PermissionRequest { tool, .. } = &ev.event {
            assert_eq!(tool, "fs_write");
            got_permission = ev.permission_id;
            break;
        }
    }
    let permission_id = got_permission.expect("bridge must RECEIVE the permission request");

    // 5) Post the allow decision back — this releases the daemon's block.
    client
        // OCEAN-185: the mock daemon does not enforce the token; `None` keeps
        // this ordering test focused on the subscribe-before-submit behaviour.
        .decide_permission(permission_id, true, None, None)
        .await
        .expect("decision POST");

    // 6) The previously-blocked turn now completes (does NOT hang).
    let resp = tokio::time::timeout(Duration::from_secs(2), submit)
        .await
        .expect("turn must complete after allow (no hang)")
        .expect("submit task")
        .expect("turn response");
    assert!(resp.ok, "allowed gated turn completes successfully");
    assert!(matches!(resp.status, AgentTurnStatus::Completed));
}

/// OCEAN-146 P2: a synchronously-rejected turn must NOT hang. The daemon returns
/// BAD_REQUEST from `/v1/agent/turns` without emitting any AgentTurnEvent, so a
/// loop that only advances on `stream.next_event()` would wait forever. This
/// reproduces `run_turn`'s `tokio::select!` over (SSE read, submit task): the
/// submit task resolves with an `Err` while the stream stays silent, and the
/// select surfaces the failure promptly instead of blocking on a phantom event.
#[tokio::test]
async fn synchronous_submit_failure_does_not_hang() {
    let base = spawn_rejecting_daemon().await;
    let client = DaemonClient::new(base);

    // Subscribe to the agent stream first, like run_turn — it will stay SILENT
    // because the rejected turn emits no events.
    let mut agent = client.event_stream().await.expect("agent stream");

    // Submit off-task, exactly as run_turn does.
    let mut submit_handle = Some(tokio::spawn({
        let client = client.clone();
        async move {
            client
                .submit_turn(
                    "bad cwd".into(),
                    "/etc".into(),
                    None,
                    None,
                    None,
                    Some(ocean_core::mint_decision_token()),
                )
                .await
        }
    }));

    let turn_id: Option<String> = None; // no turn ever starts

    // This is the shape of run_turn's loop arm. Without the submit-task arm this
    // `select!` would only ever resolve on `agent.next_event()`, which never
    // fires → hang. With it, the Err is surfaced. We bound the whole thing in a
    // timeout so a regression (reverting to event-only) FAILS loudly.
    let outcome = tokio::time::timeout(Duration::from_secs(3), async {
        // Deliberately mirrors `run_turn`'s `loop { select! { ... } }` shape so a
        // regression (reverting to event-only) hangs and fails the timeout. Each
        // arm returns on its first resolution, so clippy's deny-by-default
        // `never_loop` fires; that's the intended shape, not a bug. (OCEAN-191:
        // this `#[allow]` lets the new `cargo clippy --all-targets` CI gate pass,
        // since the lint is deny-by-default and would hard-error the job.)
        #[allow(clippy::never_loop)]
        loop {
            tokio::select! {
                biased;
                ev = agent.next_event() => {
                    // Should never produce an event for the rejected turn.
                    match ev.expect("agent stream read") {
                        Some(_) => panic!("rejected turn must emit no events"),
                        None => return Err::<(), String>("stream closed".into()),
                    }
                }
                joined = async { submit_handle.as_mut().unwrap().await }, if submit_handle.is_some() => {
                    submit_handle = None;
                    match joined.expect("submit task") {
                        Ok(_resp) => {
                            // mock returns non-2xx → client yields Err, not Ok.
                            return Ok(());
                        }
                        Err(err) => {
                            // Turn never started → surface the failure (no hang).
                            assert!(turn_id.is_none());
                            return Err(format!("submit failed: {err}"));
                        }
                    }
                }
            }
        }
    })
    .await
    .expect("must NOT hang on a synchronous submit failure");

    let err = outcome.expect_err("a 400 reject must surface as a failed turn");
    assert!(
        err.contains("submit failed"),
        "failure should come from the submit arm, got: {err}"
    );
}
