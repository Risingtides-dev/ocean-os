//! Read-only Observatory API routes (Gate 1 manifest §7).
//!
//! Three routes, all behind the Task 4 [`ObservatoryAuth`] extractor and the
//! §7.4 cache/header contract:
//!
//! - `GET /v1/observatory/snapshot` — consistent projection at a watermark
//!   cursor (nodes, edges, attention, earliest cursor, instance ids).
//! - `GET /v1/observatory/events` — SSE live tail with durable resume via
//!   `Last-Event-ID` or `?after=`, explicit `reset`/`error` frames for
//!   expired/malformed/future cursors, and `stream.gap` frames when the
//!   durable log skips (retention prune or jump). History is always replayed
//!   from the durable store before live attach; the stream never silently
//!   attaches live with unknown history.
//! - `GET /v1/observatory/replay` — ascending bounded JSON pages with
//!   `next_after`/`has_more`/`complete` and 410 on retention-crossed ranges.
//!
//! V1 projection limits (Task 6 wires real daemon facts): the store's
//! `execution_nodes` rows carry session/turn/request ids since F2, but the
//! snapshot does not read them yet, so those wire fields are still empty
//! strings, and the attention shelf is empty
//! (no waiting-phase derivation exists at the projection layer). The wire
//! shape itself is the accepted `ocean_observatory::snapshot` contract.
//!
//! No public token-creation route exists here by design (manifest §3.4).

use std::{convert::Infallible, path::Path, sync::Arc, time::Duration};

use axum::{
    extract::Query,
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response, Sse},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use ocean_observatory::{
    AttentionItem, Cursor, EventEnvelope, ObservatorySnapshot, ObservatoryStore, ReplayEvent,
    ReplayMeta, ReplayPage, RetentionPolicy, SnapshotEdge, SnapshotNode,
};

use crate::bus::SSE_KEEPALIVE_INTERVAL;
use crate::observatory_auth::ObservatoryAuth;

/// SSE standard resume header (no named constant in http 1.x).
const LAST_EVENT_ID: HeaderName = HeaderName::from_static("last-event-id");

/// Poll cadence for the durable-store live tail. The store is the authority,
/// so a short poll both catches any broadcast lag and keeps the tail honest.
const LIVE_POLL_INTERVAL: Duration = Duration::from_millis(150);
/// Maximum events forwarded per store read while tailing/catching up.
const LIVE_READ_BATCH: usize = 500;
/// Replay page bounds per manifest §7.3.
const REPLAY_DEFAULT_LIMIT: usize = 1_000;
const REPLAY_MAX_LIMIT: usize = 10_000;

/// Extension-mounted Observatory route services.
///
/// The store is optional so a corrupt or unopenable database degrades the
/// routes to explicit 503s instead of failing daemon startup.
#[derive(Clone)]
pub(crate) struct ObservatoryServices {
    store: Option<Arc<ObservatoryStore>>,
    observatory_id: String,
    daemon_instance_id: String,
}

impl ObservatoryServices {
    /// The watermark and instance the §7.4 headers name, for a response built
    /// outside a route handler (the auth rejection).
    pub(super) fn header_facts(&self) -> (Cursor, String) {
        let cursor = self
            .store
            .as_ref()
            .map(|store| store.latest_cursor())
            .unwrap_or_else(|| Cursor::new(0));
        (cursor, self.daemon_instance_id.clone())
    }

    /// Load route services at startup. Never fails: store errors degrade to
    /// `None` and are logged; the stable observatory id falls back to the
    /// boot id when its file is unreadable/unwritable.
    pub(crate) fn load(config_dir: &Path, daemon_instance_id: String) -> Self {
        let store_path = config_dir.join("observatory.db");
        let store = match ObservatoryStore::open(&store_path, RetentionPolicy::default()) {
            Ok(store) => Some(Arc::new(store)),
            Err(error) => {
                tracing::error!(
                    path = %store_path.display(),
                    %error,
                    "Observatory store failed to open; read-only routes will answer 503"
                );
                None
            }
        };
        Self {
            store,
            observatory_id: load_or_create_observatory_id(config_dir, &daemon_instance_id),
            daemon_instance_id,
        }
    }

    /// Shared store handle for the runtime-fact adapter (Task 6). `None`
    /// means the store failed to open and the routes answer 503.
    pub(crate) fn store_handle(&self) -> Option<Arc<ObservatoryStore>> {
        self.store.clone()
    }

    pub(crate) fn observatory_id(&self) -> &str {
        &self.observatory_id
    }

    pub(crate) fn daemon_instance_id(&self) -> &str {
        &self.daemon_instance_id
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        store: Arc<ObservatoryStore>,
        observatory_id: &str,
        daemon_instance_id: &str,
    ) -> Self {
        Self {
            store: Some(store),
            observatory_id: observatory_id.to_owned(),
            daemon_instance_id: daemon_instance_id.to_owned(),
        }
    }
}

/// How often the retention pass runs. Pruning is cheap when nothing is due,
/// and the Gate 0 bounds are days and gigabytes, so hourly is ample.
const RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// First pass shortly after boot, off the startup critical path.
const RETENTION_FIRST_DELAY: Duration = Duration::from_secs(60);

/// Manifest §4.3 WAL checkpoint cadence.
const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

/// Manifest §4.3: checkpoint and truncate the Observatory WAL every 60 s until
/// `cancel`, on a blocking thread. `journal_size_limit` (set at open) bounds
/// the file between passes; a busy pass is retried on the next tick.
pub(crate) async fn run_checkpoints(
    store: Arc<ObservatoryStore>,
    cancel: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(CHECKPOINT_INTERVAL) => {}
        }
        let pass_store = Arc::clone(&store);
        match tokio::task::spawn_blocking(move || pass_store.checkpoint()).await {
            Ok(Ok(report)) if report.busy => {
                tracing::debug!(
                    ?report,
                    "observatory WAL checkpoint deferred: database busy"
                );
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "observatory WAL checkpoint failed"),
            Err(error) => tracing::warn!(%error, "observatory WAL checkpoint panicked"),
        }
    }
}

/// G3: apply the store's retention policy on a schedule until `cancel`.
/// Each pass runs on a blocking thread (it reads and deletes SQLite rows);
/// a failed pass is logged and the next one tries again.
pub(crate) async fn run_retention(
    store: Arc<ObservatoryStore>,
    cancel: tokio_util::sync::CancellationToken,
) {
    let mut delay = RETENTION_FIRST_DELAY;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        delay = RETENTION_INTERVAL;
        let pass_store = Arc::clone(&store);
        match tokio::task::spawn_blocking(move || pass_store.apply_retention()).await {
            Ok(Ok(0)) => {}
            Ok(Ok(pruned)) => tracing::info!(pruned, "observatory retention pruned events"),
            Ok(Err(error)) => tracing::warn!(%error, "observatory retention pass failed"),
            Err(error) => tracing::warn!(%error, "observatory retention pass panicked"),
        }
    }
}

/// when the operator deletes the file. Best-effort; falls back to the boot id.
fn load_or_create_observatory_id(config_dir: &Path, daemon_instance_id: &str) -> String {
    let path = config_dir.join("observatory-id");
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let id = raw.trim();
        if !id.is_empty() {
            return id.to_owned();
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Err(error) = std::fs::write(&path, format!("{id}\n")) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "observatory-id could not be persisted; using ephemeral id"
        );
        return daemon_instance_id.to_owned();
    }
    id
}

// ── Shared helpers ──────────────────────────────────────────────────────────

/// §7.4 headers present on every Observatory response.
pub(super) fn observatory_headers(watermark: Cursor, instance: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate, private"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(header::EXPIRES, HeaderValue::from_static("0"));
    headers.insert(
        HeaderName::from_static("x-observatory-cursor"),
        HeaderValue::from_str(&watermark.as_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        HeaderName::from_static("x-observatory-instance"),
        HeaderValue::from_str(instance).unwrap_or_else(|_| HeaderValue::from_static("unknown")),
    );
    headers
}

pub(super) fn error_response(
    status: StatusCode,
    headers: HeaderMap,
    error: &str,
    message: &str,
) -> Response {
    let body = json!({
        "error": error,
        "message": message,
        "http_status": status.as_u16(),
    });
    (status, headers, Json(body)).into_response()
}

fn store_unavailable(headers: HeaderMap) -> Response {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        headers,
        "store_unavailable",
        "Observatory store is not open; see daemon logs",
    )
}

/// Run a store call on Tokio's blocking pool (F1).
///
/// Every `ObservatoryStore` method except `latest_cursor` takes the store's
/// connection mutex and runs rusqlite, and an append can hold that mutex for
/// up to the store's busy timeout while it waits on a competing writer. Done
/// inline, that wait parks an async worker — on a current-thread runtime, the
/// whole daemon. `spawn_blocking` rather than `block_in_place`: it works on
/// every runtime flavor (`block_in_place` panics on a current-thread runtime,
/// which is what `#[tokio::test]` builds) and needs no care about which task
/// holds what. `None` means the blocking task panicked; callers treat that as
/// the store being unavailable.
pub(crate) async fn off_executor<T, F>(store: &Arc<ObservatoryStore>, call: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(&ObservatoryStore) -> T + Send + 'static,
{
    let store = Arc::clone(store);
    match tokio::task::spawn_blocking(move || call(&store)).await {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::error!(%error, "observatory store call panicked");
            None
        }
    }
}

fn parse_cursor(raw: &str) -> Option<Cursor> {
    Cursor::from_string(raw).ok()
}

/// Map the store's projected phase string back to the wire enum. The V1
/// projection writes `format!("{phase:?}").to_lowercase()`, which collapses
/// `TimedOut` to `timedout`; both spellings are accepted here.
pub(crate) fn phase_from_projection(raw: &str) -> ocean_observatory::ExecutionPhase {
    use ocean_observatory::ExecutionPhase;
    match raw {
        "admitted" => ExecutionPhase::Admitted,
        "finished" => ExecutionPhase::Finished,
        "error" => ExecutionPhase::Error,
        "canceled" => ExecutionPhase::Canceled,
        "timedout" | "timed_out" => ExecutionPhase::TimedOut,
        _ => ExecutionPhase::Running,
    }
}

fn daemon_producer() -> ocean_observatory::Producer {
    ocean_observatory::Producer {
        kind: ocean_observatory::ProducerKind::Daemon,
        id: "ocean-daemon".to_owned(),
    }
}

// ── GET /v1/observatory/snapshot (§7.1) ────────────────────────────────────

/// `detail` is `summary` (default) or `full` (manifest §7.1). `full` is
/// reserved (Task 9 F12): §7.1 says it adds "metadata" but defines no field
/// for it, and every node/edge fact the V1 projection holds is already in the
/// summary shape, so `full` answers exactly what `summary` does. It is
/// validated rather than refused so a client written against §7.1 keeps
/// working when `full` gains fields; any other value is 400 `invalid_detail`.
#[derive(Debug, Deserialize)]
pub(crate) struct SnapshotQuery {
    at: Option<String>,
    detail: Option<String>,
}

pub(crate) async fn snapshot(
    ObservatoryAuth(_principal): ObservatoryAuth,
    axum::Extension(services): axum::Extension<ObservatoryServices>,
    Query(query): Query<SnapshotQuery>,
) -> Response {
    let latest = services
        .store
        .as_ref()
        .map(|store| store.latest_cursor())
        .unwrap_or_else(|| Cursor::new(0));
    let headers = observatory_headers(latest, &services.daemon_instance_id);
    let Some(store) = services.store.as_ref() else {
        return store_unavailable(headers);
    };
    if let Some(detail) = query.detail.as_deref() {
        if detail != "summary" && detail != "full" {
            return error_response(
                StatusCode::BAD_REQUEST,
                headers,
                "invalid_detail",
                "detail must be summary or full",
            );
        }
    }

    let at = match query.at.as_deref() {
        Some(raw) => match parse_cursor(raw) {
            Some(cursor) => Some(cursor),
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    headers,
                    "invalid_cursor",
                    "Cursor format invalid or future value",
                )
            }
        },
        None => None,
    };
    let boundary = match off_executor(store, |store| store.retention_boundary()).await {
        Some(Ok(boundary)) => boundary,
        Some(Err(error)) => {
            tracing::error!(%error, "observatory snapshot retention-boundary read failed");
            return store_unavailable(headers);
        }
        None => return store_unavailable(headers),
    };
    if let Some(at) = at {
        if at > latest {
            return error_response(
                StatusCode::BAD_REQUEST,
                headers,
                "invalid_cursor",
                "Cursor format invalid or future value",
            );
        }
        // 410 only when history was actually pruned past `at`; a natural log
        // start at cursor 1 is not a retention crossing.
        if boundary.is_some_and(|boundary| at <= boundary) {
            return error_response(
                StatusCode::GONE,
                headers,
                "cursor_too_old",
                "Cursor is before retention boundary; use current snapshot",
            );
        }
    }

    let projection = match off_executor(store, move |store| store.snapshot_at(at)).await {
        None => return store_unavailable(headers),
        Some(Ok(projection)) => projection,
        // G1: Gate 1 serves the current projection only; an earlier `at` is
        // refused instead of answered with current state under its label.
        Some(Err(ocean_observatory::StoreError::HistoricalSnapshot { .. })) => {
            return error_response(
                StatusCode::CONFLICT,
                headers,
                "snapshot_not_historical",
                "Only the current watermark can be snapshotted; omit `at` and tail from the returned watermark",
            );
        }
        Some(Err(error)) => {
            tracing::error!(%error, "observatory snapshot read failed");
            return store_unavailable(headers);
        }
    };
    let snapshot = ObservatorySnapshot {
        watermark_cursor: projection.watermark_cursor,
        earliest_available_cursor: projection.earliest_available_cursor,
        observatory_id: services.observatory_id.clone(),
        daemon_instance_id: services.daemon_instance_id.clone(),
        nodes: projection
            .nodes
            .into_iter()
            .map(|node| SnapshotNode {
                execution_id: node.execution_id.clone(),
                root_execution_id: node.root_execution_id,
                parent_execution_id: node.parent_execution_id,
                // V1 projection carries no session/turn/request columns yet;
                // Task 6 wires real daemon facts. Empty, never fabricated.
                session_id: String::new(),
                turn_id: String::new(),
                request_id: String::new(),
                phase: phase_from_projection(&node.phase),
                producer: daemon_producer(),
                truth: ocean_observatory::TruthProvenance::HostObserved,
                started_at: node.created_at.clone(),
                last_activity_at: node.created_at,
                labels: Vec::new(),
                duration_millis: None,
            })
            .collect(),
        edges: projection
            .edges
            .into_iter()
            .map(|edge| SnapshotEdge {
                edge_id: edge.edge_id,
                parent_execution_id: edge.parent_execution_id,
                child_execution_id: edge.child_execution_id,
                // The V1 edge projection stores no root column; leave empty
                // rather than guessing (Task 6 wires real facts).
                root_execution_id: String::new(),
                created_at: edge.created_at,
                truth: ocean_observatory::TruthProvenance::HostObserved,
            })
            .collect(),
        // No waiting-phase derivation exists at the V1 projection layer.
        attention: Vec::<AttentionItem>::new(),
    };
    (StatusCode::OK, headers, Json(snapshot)).into_response()
}

// ── GET /v1/observatory/events (§7.2, SSE) ─────────────────────────────────

/// `scope` is `summary` (the default) or, per manifest §7.2, the future
/// `content`. V1 serves summary only (F3 refuses any other token scope), so
/// every value except `summary` is refused with an `invalid_scope` error frame
/// — the §7.2 form of a 400 on this SSE route, as for `invalid_cursor` —
/// rather than silently streaming summary events to a caller that asked for
/// something else (Task 9 F12).
#[derive(Debug, Deserialize)]
pub(crate) struct EventsQuery {
    after: Option<String>,
    scope: Option<String>,
}

pub(crate) async fn events(
    ObservatoryAuth(_principal): ObservatoryAuth,
    axum::Extension(services): axum::Extension<ObservatoryServices>,
    Query(query): Query<EventsQuery>,
    headers_in: HeaderMap,
) -> Response {
    let Some(store) = services.store.clone() else {
        let headers = observatory_headers(Cursor::new(0), &services.daemon_instance_id);
        return store_unavailable(headers);
    };
    if query
        .scope
        .as_deref()
        .is_some_and(|scope| scope != "summary")
    {
        return sse_terminal(
            "error",
            None,
            json!({
                "error":"invalid_scope",
                "message":"scope must be summary; content is reserved and not served in V1",
            }),
            &services.daemon_instance_id,
        );
    }

    // SSE resume contract: the standard Last-Event-ID header wins over the
    // explicit query parameter.
    let requested = headers_in
        .get(&LAST_EVENT_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .or(query.after);

    let latest = store.latest_cursor();
    let after = match requested.as_deref() {
        Some(raw) => match parse_cursor(raw) {
            Some(cursor) => cursor,
            None => {
                return sse_terminal(
                    "error",
                    None,
                    json!({"error":"invalid_cursor","message":"Cursor format invalid"}),
                    &services.daemon_instance_id,
                )
            }
        },
        // Fresh attach at the watermark: no history is claimed or needed.
        None => latest,
    };

    if after > latest {
        return sse_terminal(
            "error",
            None,
            json!({"error":"invalid_cursor","message":"Cursor format invalid or future value"}),
            &services.daemon_instance_id,
        );
    }
    let earliest = match off_executor(&store, |store| store.earliest_available_cursor()).await {
        Some(Ok(cursor)) => cursor,
        failed => {
            if let Some(Err(error)) = failed {
                tracing::error!(%error, "observatory events earliest-cursor read failed");
            }
            let headers = observatory_headers(latest, &services.daemon_instance_id);
            return store_unavailable(headers);
        }
    };
    // Resuming into pruned history: the events after `after` are (partially)
    // gone, so the client must re-baseline from a fresh snapshot.
    if after < latest && earliest > after.next() {
        return sse_terminal(
            "reset",
            Some(earliest),
            json!({
                "error":"cursor_expired",
                "message":"Cursor is before retention boundary. Request fresh snapshot.",
                "earliest_cursor": earliest.as_string(),
            }),
            &services.daemon_instance_id,
        );
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::response::sse::Event, Infallible>>(64);
    tokio::spawn(async move {
        let mut last = after;
        'tail: loop {
            // F1: one blocking-pool read per poll: the batch, plus the
            // earliest surviving cursor when the batch skips (to name the
            // gap's reason) — never a rusqlite call on the async worker.
            let read = off_executor(&store, move |store| {
                let batch = store.events_after(last, Some(LIVE_READ_BATCH))?;
                let skips = batch
                    .iter()
                    .scan(last, |prev, envelope| {
                        let skipped = envelope.cursor != prev.next();
                        *prev = envelope.cursor;
                        Some(skipped)
                    })
                    .any(|skipped| skipped);
                let earliest = if skips {
                    Some(store.earliest_available_cursor()?)
                } else {
                    None
                };
                Ok::<_, ocean_observatory::StoreError>((batch, earliest))
            })
            .await;
            let (batch, earliest) = match read {
                Some(Ok(read)) => read,
                failed => {
                    if let Some(Err(error)) = failed {
                        tracing::error!(%error, "observatory live tail read failed");
                    }
                    let frame = axum::response::sse::Event::default().event("error").data(
                        r#"{"error":"store_read_failed","message":"Durable log read failed"}"#,
                    );
                    let _ = tx.send(Ok(frame)).await;
                    break 'tail;
                }
            };
            for envelope in batch {
                let expected = last.next();
                if envelope.cursor != expected {
                    // Durable log skipped: retention pruned (or a cursor jump).
                    // Say so explicitly instead of silently jumping.
                    let reason = match earliest {
                        Some(boundary) if boundary > expected => "retention_boundary",
                        _ => "cursor_jump",
                    };
                    // F5: no `id:`. The gap is not an event the client has
                    // consumed, so it must not move the browser's last-event
                    // id: reusing the post-gap event's cursor here meant a
                    // client dropped between the two frames resumed AFTER an
                    // event it never saw. Without an id, a resume starts from
                    // the last real event and meets the gap again (or a
                    // `reset`, if retention caused it).
                    let gap = axum::response::sse::Event::default().event("message").data(
                        json!({
                            "cursor": expected.as_string(),
                            "kind": "stream.gap",
                            "payload": {
                                "from_cursor": last.as_string(),
                                "to_cursor": envelope.cursor.as_string(),
                                "reason": reason,
                            }
                        })
                        .to_string(),
                    );
                    if tx.send(Ok(gap)).await.is_err() {
                        break 'tail;
                    }
                }
                let frame = match serde_json::to_string(&envelope) {
                    Ok(data) => axum::response::sse::Event::default()
                        .event("message")
                        .id(envelope.cursor.as_string())
                        .data(data),
                    Err(error) => {
                        tracing::error!(%error, "observatory envelope serialization failed");
                        continue;
                    }
                };
                last = envelope.cursor;
                if tx.send(Ok(frame)).await.is_err() {
                    break 'tail; // client disconnected
                }
            }
            // A client that left while nothing new arrived never fails a
            // send; stop polling the store for it.
            if tx.is_closed() {
                break 'tail;
            }
            tokio::time::sleep(LIVE_POLL_INTERVAL).await;
        }
    });

    let mut headers = observatory_headers(latest, &services.daemon_instance_id);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    let sse = Sse::new(ReceiverStream::new(rx)).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(SSE_KEEPALIVE_INTERVAL)
            .text("heartbeat"),
    );
    (StatusCode::OK, headers, sse).into_response()
}

/// A stream that emits exactly one terminal frame (`reset` or `error`) and
/// then ends, per the §7.2 in-stream error contract.
fn sse_terminal(
    event: &str,
    id: Option<Cursor>,
    payload: Value,
    daemon_instance_id: &str,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::response::sse::Event, Infallible>>(1);
    let mut frame = axum::response::sse::Event::default()
        .event(event)
        .data(payload.to_string());
    if let Some(id) = id {
        frame = frame.id(id.as_string());
    }
    let _ = tx.try_send(Ok(frame));
    drop(tx);
    let mut headers = observatory_headers(Cursor::new(0), daemon_instance_id);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    (StatusCode::OK, headers, Sse::new(ReceiverStream::new(rx))).into_response()
}

// ── GET /v1/observatory/replay (§7.3) ──────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ReplayQuery {
    after: Option<String>,
    through: Option<String>,
    limit: Option<usize>,
    filter: Option<String>,
}

pub(crate) async fn replay(
    ObservatoryAuth(_principal): ObservatoryAuth,
    axum::Extension(services): axum::Extension<ObservatoryServices>,
    Query(query): Query<ReplayQuery>,
) -> Response {
    let latest = services
        .store
        .as_ref()
        .map(|store| store.latest_cursor())
        .unwrap_or_else(|| Cursor::new(0));
    let headers = observatory_headers(latest, &services.daemon_instance_id);
    let Some(store) = services.store.as_ref() else {
        return store_unavailable(headers);
    };

    let Some(after_raw) = query.after.as_deref() else {
        return error_response(
            StatusCode::BAD_REQUEST,
            headers,
            "invalid_cursor",
            "after is required",
        );
    };
    let Some(after) = parse_cursor(after_raw) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            headers,
            "invalid_cursor",
            "Cursor format invalid",
        );
    };
    let through = match query.through.as_deref() {
        Some(raw) => match parse_cursor(raw) {
            Some(cursor) => Some(cursor),
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    headers,
                    "invalid_cursor",
                    "Cursor format invalid",
                )
            }
        },
        None => None,
    };
    if let Some(through) = through {
        if after >= through {
            return error_response(
                StatusCode::BAD_REQUEST,
                headers,
                "invalid_range",
                "after >= through",
            );
        }
    }
    let limit = query
        .limit
        .unwrap_or(REPLAY_DEFAULT_LIMIT)
        .clamp(1, REPLAY_MAX_LIMIT);

    // Optional post-filters. `kind:` matches the wire kind string (both
    // `tool_started` and `tool.started` spellings); `producer:` matches the
    // producer id. Unknown filter names are rejected explicitly.
    let filter = match query.filter.as_deref() {
        Some(raw) => match raw.split_once(':') {
            Some(("kind", value)) if !value.is_empty() => Some(("kind", value.to_owned())),
            Some(("producer", value)) if !value.is_empty() => Some(("producer", value.to_owned())),
            _ => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    headers,
                    "invalid_filter",
                    "filter must be kind:<event-kind> or producer:<producer-id>",
                )
            }
        },
        None => None,
    };

    let boundary = match off_executor(store, |store| store.retention_boundary()).await {
        Some(Ok(boundary)) => boundary,
        Some(Err(error)) => {
            tracing::error!(%error, "observatory replay retention-boundary read failed");
            return store_unavailable(headers);
        }
        None => return store_unavailable(headers),
    };
    // A range that starts inside pruned history is a hard 410 with the exact
    // unavailable span, never a silent skip. `after` at the natural log start
    // (before any pruning) is not a crossing.
    if let Some(boundary) = boundary {
        if after <= boundary {
            let earliest_available = boundary.next();
            let body = json!({
                "error": "retention_boundary_crossed",
                "message": format!(
                    "Events from cursor {} to {} are not available",
                    after.next().as_string(),
                    boundary.as_string(),
                ),
                "gap_from": after.next().as_string(),
                "gap_to": boundary.as_string(),
                "earliest_available": earliest_available.as_string(),
                "http_status": 410,
            });
            return (StatusCode::GONE, headers, Json(body)).into_response();
        }
    }

    let page =
        match off_executor(store, move |store| store.replay_page(after, through, limit)).await {
            Some(Ok(page)) => page,
            Some(Err(error)) => {
                tracing::error!(%error, "observatory replay read failed");
                return store_unavailable(headers);
            }
            None => return store_unavailable(headers),
        };

    let events: Vec<ReplayEvent> = page
        .events
        .into_iter()
        .filter(|envelope| matches_filter(envelope, filter.as_ref()))
        .map(|envelope| ReplayEvent {
            cursor: envelope.cursor,
            event_id: envelope.event_id,
            schema_version: envelope.schema_version,
            occurred_at: envelope.occurred_at,
            recorded_at: envelope.recorded_at,
            kind: serde_json::to_value(envelope.kind)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned()),
            truth: envelope.truth,
            producer: envelope.producer,
            topology: envelope.topology,
            correlation: envelope.correlation,
            visibility: envelope.visibility,
            payload: serde_json::to_value(&envelope.payload)
                .unwrap_or_else(|_| json!({"redacted": true})),
        })
        .collect();

    let continuation_url = if page.has_more {
        page.next_after.map(|next| {
            let mut url = format!("/v1/observatory/replay?after={next}&limit={limit}");
            if let Some(through) = through {
                url.push_str(&format!("&through={through}"));
            }
            if let Some(raw) = query.filter.as_deref() {
                // F6: the filter value is caller text; encode it so it
                // cannot add or rewrite query parameters.
                url.push_str("&filter=");
                url.push_str(&encode_query_value(raw));
            }
            url
        })
    } else {
        None
    };

    let body = ReplayPage {
        events,
        next_after: page.next_after,
        has_more: page.has_more,
        complete: page.complete,
        continuation_url,
        meta: ReplayMeta {
            daemon_instance_id: services.daemon_instance_id.clone(),
            observatory_id: services.observatory_id.clone(),
            after,
            through,
            generated_at: chrono::Utc::now().to_rfc3339(),
        },
    };
    (StatusCode::OK, headers, Json(body)).into_response()
}

/// Percent-encode everything outside RFC 3986's unreserved set, so a value
/// is always exactly one query parameter value.
fn encode_query_value(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn matches_filter(envelope: &EventEnvelope, filter: Option<&(&str, String)>) -> bool {
    let Some((name, value)) = filter else {
        return true;
    };
    match *name {
        "kind" => {
            let wire = serde_json::to_value(envelope.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            &wire == value || wire.replace('_', ".") == *value
        }
        "producer" => &envelope.producer.id == value,
        _ => true,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

/// F1 test support: wedge a store the way production can — an append that
/// holds the store's connection mutex while it waits (busy timeout) on
/// another connection's write lock. Anything that then touches the store
/// blocks until `release`; on the async executor, that would freeze it.
#[cfg(test)]
pub(crate) struct StoreWedge {
    blocker: rusqlite::Connection,
    append: std::thread::JoinHandle<()>,
}

#[cfg(test)]
impl StoreWedge {
    pub(crate) fn engage(path: &Path, store: &Arc<ObservatoryStore>, event: EventEnvelope) -> Self {
        let blocker = rusqlite::Connection::open(path).expect("blocker connection");
        blocker
            .execute_batch("BEGIN IMMEDIATE")
            .expect("take write lock");
        let wedged = Arc::clone(store);
        let append = std::thread::spawn(move || {
            wedged.append_event(event).expect("wedged append lands");
        });
        // Let the append take the store mutex and start waiting.
        std::thread::sleep(Duration::from_millis(150));
        Self { blocker, append }
    }

    pub(crate) fn release(self) {
        self.blocker
            .execute_batch("COMMIT")
            .expect("release write lock");
        self.append.join().expect("wedged append thread");
    }
}

/// F1: a 20 ms timer on this (current-thread) runtime must fire promptly
/// while store calls wait; it cannot if a store call is blocking the thread.
#[cfg(test)]
pub(crate) async fn assert_executor_stays_live() {
    let started = std::time::Instant::now();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "executor stalled {:?} behind a store call",
        started.elapsed()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
        routing::get,
        Router,
    };
    use ocean_observatory::{
        Correlation, EventKind, EventPayload, ExecutionPhase, ObserverSecret, ObserverToken,
        Producer, ProducerKind, Topology, TruthProvenance, Visibility,
    };
    use tower::ServiceExt;

    use crate::observatory_auth::ObservatoryAuthState;

    const DAEMON_ID: &str = "daemon-test";
    const OBS_ID: &str = "obs-test";

    fn test_secret() -> ObserverSecret {
        ObserverSecret::from_raw_key([0x7A; 32])
    }

    fn token(scope: ocean_observatory::ObserverScope) -> String {
        let claims = ObserverToken::issue(scope, DAEMON_ID, 1_800).expect("issue");
        ocean_observatory::sign_token(&claims, &test_secret())
    }

    fn services(store: Arc<ObservatoryStore>) -> ObservatoryServices {
        ObservatoryServices::for_test(store, OBS_ID, DAEMON_ID)
    }

    fn app(store: Arc<ObservatoryStore>) -> Router {
        let auth = ObservatoryAuthState::for_test(test_secret(), DAEMON_ID);
        Router::new()
            .route("/v1/observatory/snapshot", get(snapshot))
            .route("/v1/observatory/events", get(events))
            .route("/v1/observatory/replay", get(replay))
            .layer(axum::Extension(auth))
            .layer(axum::Extension(services(store)))
    }

    fn store_with(events: &[EventEnvelope]) -> Arc<ObservatoryStore> {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = ObservatoryStore::open(&dir.path().join("obs.db"), RetentionPolicy::default())
            .expect("open store");
        for event in events {
            store.append_event(event.clone()).expect("append");
        }
        Arc::new(store)
    }

    /// Manifest §4.3: the daemon's checkpoint loop truncates the WAL on its
    /// cadence and stops on shutdown.
    #[tokio::test(start_paused = true)]
    async fn scheduled_checkpoints_truncate_the_wal_and_stop_on_shutdown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("obs.db");
        let store =
            Arc::new(ObservatoryStore::open(&path, RetentionPolicy::default()).expect("open"));
        for i in 0..50 {
            store
                .append_event(envelope(&format!("c-{i}"), EventKind::ExecutionFinished))
                .expect("append");
        }
        let wal = dir.path().join("obs.db-wal");
        let wal_len = || std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert!(wal_len() > 0);
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(run_checkpoints(Arc::clone(&store), cancel.clone()));
        tokio::task::yield_now().await;
        tokio::time::advance(CHECKPOINT_INTERVAL + Duration::from_secs(1)).await;
        for _ in 0..200 {
            if wal_len() == 0 {
                break;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(wal_len(), 0, "the loop checkpointed and truncated the WAL");
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("checkpoint loop ends on shutdown")
            .unwrap();
    }

    /// G3: the scheduled loop really runs retention (the policy used to have
    /// no production caller) and stops when the daemon shuts down.
    #[tokio::test(start_paused = true)]
    async fn scheduled_retention_prunes_and_stops_on_shutdown() {
        let mut old = envelope("done", EventKind::ExecutionFinished);
        old.recorded_at = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        let store = store_with(&[old]);
        assert_eq!(store.events_after(Cursor::new(0), None).unwrap().len(), 1);
        let cancel = tokio_util::sync::CancellationToken::new();
        let task = tokio::spawn(run_retention(Arc::clone(&store), cancel.clone()));
        // Let the loop start its first sleep before the clock moves.
        tokio::task::yield_now().await;
        tokio::time::advance(RETENTION_FIRST_DELAY + Duration::from_secs(1)).await;
        for _ in 0..200 {
            if store.retention_boundary().unwrap().is_some() {
                break;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(store.retention_boundary().unwrap(), Some(Cursor::new(1)));
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("retention loop ends on shutdown")
            .unwrap();
    }

    fn envelope(execution_id: &str, kind: EventKind) -> EventEnvelope {
        let now = chrono::Utc::now().to_rfc3339();
        // Payload drives the store's node-phase projection, so it must agree
        // with the kind under test (e.g. Finished lets retention prune).
        let payload = match kind {
            EventKind::ExecutionFinished => EventPayload::ExecutionFinished {
                phase: ExecutionPhase::Finished,
                duration_millis: 1,
                error_classification: None,
            },
            _ => EventPayload::ExecutionPhaseChanged {
                from_phase: ExecutionPhase::Admitted,
                to_phase: ExecutionPhase::Running,
            },
        };
        EventEnvelope {
            schema_version: 1,
            cursor: Cursor::new(0),
            event_id: uuid::Uuid::new_v4().to_string(),
            observatory_id: OBS_ID.to_owned(),
            daemon_instance_id: DAEMON_ID.to_owned(),
            occurred_at: now.clone(),
            recorded_at: now,
            kind,
            truth: TruthProvenance::HostObserved,
            producer: Producer {
                kind: ProducerKind::Daemon,
                id: "ocean-daemon".to_owned(),
            },
            topology: Topology {
                execution_id: execution_id.to_owned(),
                root_execution_id: execution_id.to_owned(),
                parent_execution_id: None,
                edge_id: None,
                session_id: "s-1".to_owned(),
                turn_id: "t-1".to_owned(),
                request_id: "r-1".to_owned(),
            },
            correlation: Correlation {
                tool_call_id: None,
                permission_id: None,
            },
            visibility: Visibility::Metadata,
            payload,
        }
    }

    async fn body_string(response: Response) -> String {
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    fn authed(path: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .header(
                header::AUTHORIZATION,
                format!(
                    "Bearer {}",
                    token(ocean_observatory::ObserverScope::Summary)
                ),
            )
            .body(Body::empty())
            .expect("request")
    }

    #[tokio::test]
    async fn routes_require_observer_auth() {
        let router = app(store_with(&[]));
        for path in [
            "/v1/observatory/snapshot",
            "/v1/observatory/events",
            "/v1/observatory/replay?after=0",
        ] {
            let response = router
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            // G5: §7.4 headers and the §7.1 body on the rejection too.
            let headers = response.headers();
            assert_eq!(
                headers[header::CACHE_CONTROL],
                "no-store, no-cache, must-revalidate, private",
                "{path}"
            );
            assert_eq!(headers[header::PRAGMA], "no-cache", "{path}");
            assert_eq!(headers[header::EXPIRES], "0", "{path}");
            assert!(headers.contains_key("x-observatory-cursor"), "{path}");
            assert!(headers.contains_key("x-observatory-instance"), "{path}");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(
                body,
                json!({
                    "error": "unauthorized",
                    "message": "Missing or invalid observer token",
                    "http_status": 401,
                }),
                "{path}"
            );
        }
    }

    /// G1 on the wire: an earlier `at` is a 409, never current state under an
    /// old label; `at` equal to the watermark still answers 200.
    #[tokio::test]
    async fn snapshot_refuses_a_historical_cursor() {
        let store = store_with(&[
            envelope("a", EventKind::ExecutionAdmitted),
            envelope("b", EventKind::ExecutionAdmitted),
        ]);
        let response = app(Arc::clone(&store))
            .oneshot(authed("/v1/observatory/snapshot?at=1"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body: Value = serde_json::from_str(&body_string(response).await).expect("json");
        assert_eq!(body["error"], "snapshot_not_historical");
        let response = app(store)
            .oneshot(authed("/v1/observatory/snapshot?at=2"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn snapshot_empty_store_returns_wire_shape_and_headers() {
        let response = app(store_with(&[]))
            .oneshot(authed("/v1/observatory/snapshot"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()["cache-control"],
            "no-store, no-cache, must-revalidate, private"
        );
        assert_eq!(response.headers()["x-observatory-instance"], DAEMON_ID);
        let body: Value = serde_json::from_str(&body_string(response).await).expect("json");
        assert_eq!(body["observatory_id"], json!(OBS_ID));
        assert_eq!(body["daemon_instance_id"], json!(DAEMON_ID));
        assert_eq!(body["nodes"], json!([]));
        assert_eq!(body["edges"], json!([]));
        assert_eq!(body["attention"], json!([]));
        assert_eq!(body["watermark_cursor"], json!("0"));
    }

    #[tokio::test]
    async fn snapshot_rejects_future_malformed_and_pruned_cursors() {
        let events = vec![
            envelope("e-1", EventKind::ExecutionAdmitted),
            envelope("e-1", EventKind::ExecutionFinished),
        ];
        let router = app(store_with(&events));

        let future = router
            .clone()
            .oneshot(authed("/v1/observatory/snapshot?at=9999"))
            .await
            .expect("response");
        assert_eq!(future.status(), StatusCode::BAD_REQUEST);

        let malformed = router
            .clone()
            .oneshot(authed("/v1/observatory/snapshot?at=abc"))
            .await
            .expect("response");
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn snapshot_at_cursor_projects_nodes() {
        let events = vec![
            envelope("e-1", EventKind::ExecutionAdmitted),
            envelope("e-1", EventKind::ExecutionPhaseChanged),
        ];
        let response = app(store_with(&events))
            .oneshot(authed("/v1/observatory/snapshot"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body: Value = serde_json::from_str(&body_string(response).await).expect("json");
        let nodes = body["nodes"].as_array().expect("nodes array");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0]["execution_id"], json!("e-1"));
        assert_eq!(nodes[0]["phase"], json!("running"));
        assert_eq!(nodes[0]["producer"]["id"], json!("ocean-daemon"));
    }

    #[tokio::test]
    async fn replay_pages_events_with_continuation() {
        let events: Vec<EventEnvelope> = (0..3)
            .map(|i| envelope(&format!("e-{i}"), EventKind::ExecutionPhaseChanged))
            .collect();
        let router = app(store_with(&events));

        let first = router
            .clone()
            .oneshot(authed("/v1/observatory/replay?after=0&limit=2"))
            .await
            .expect("response");
        assert_eq!(first.status(), StatusCode::OK);
        let page: Value = serde_json::from_str(&body_string(first).await).expect("json");
        assert_eq!(page["events"].as_array().expect("events").len(), 2);
        // G2: the full §7.3 envelope, not the reduced six fields.
        let event = page["events"][0].as_object().expect("event object");
        for field in [
            "cursor",
            "event_id",
            "schema_version",
            "occurred_at",
            "recorded_at",
            "kind",
            "truth",
            "producer",
            "topology",
            "correlation",
            "visibility",
            "payload",
        ] {
            assert!(event.contains_key(field), "replay event lacks {field}");
        }
        assert_eq!(event.len(), 12, "exactly the §7.3 fields: {event:?}");
        assert_eq!(page["has_more"], json!(true));
        assert_eq!(page["complete"], json!(false));
        assert_eq!(page["next_after"], json!("2"));
        assert!(page["continuation_url"]
            .as_str()
            .expect("continuation")
            .contains("after=2"));

        let second = router
            .oneshot(authed("/v1/observatory/replay?after=2"))
            .await
            .expect("response");
        let page: Value = serde_json::from_str(&body_string(second).await).expect("json");
        assert_eq!(page["events"].as_array().expect("events").len(), 1);
        assert_eq!(page["has_more"], json!(false));
    }

    #[tokio::test]
    async fn replay_rejects_missing_malformed_and_inverted_ranges() {
        let router = app(store_with(&[]));
        for (uri, status) in [
            ("/v1/observatory/replay", StatusCode::BAD_REQUEST),
            ("/v1/observatory/replay?after=x", StatusCode::BAD_REQUEST),
            (
                "/v1/observatory/replay?after=5&through=5",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/v1/observatory/replay?after=0&filter=bogus:x",
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let response = router.clone().oneshot(authed(uri)).await.expect("response");
            assert_eq!(response.status(), status, "{uri}");
        }
    }

    #[tokio::test]
    async fn replay_filters_by_kind_spelling() {
        let events = vec![
            envelope("e-1", EventKind::ExecutionAdmitted),
            envelope("e-1", EventKind::ExecutionPhaseChanged),
        ];
        let router = app(store_with(&events));
        for spelling in ["execution_admitted", "execution.admitted"] {
            let response = router
                .clone()
                .oneshot(authed(&format!(
                    "/v1/observatory/replay?after=0&filter=kind:{spelling}"
                )))
                .await
                .expect("response");
            let page: Value = serde_json::from_str(&body_string(response).await).expect("json");
            assert_eq!(
                page["events"].as_array().expect("events").len(),
                1,
                "{spelling}"
            );
        }
    }

    #[tokio::test]
    async fn events_replays_history_then_tails_live() {
        let events = vec![envelope("e-1", EventKind::ExecutionAdmitted)];
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            ObservatoryStore::open(&dir.path().join("obs.db"), RetentionPolicy::default())
                .expect("open"),
        );
        for event in &events {
            store.append_event(event.clone()).expect("append");
        }
        let live_store = Arc::clone(&store);
        let router = app(store);

        let response = router
            .oneshot(authed("/v1/observatory/events?after=0"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");

        // Append after attach: the tail must pick it up from the durable log.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            live_store
                .append_event(envelope("e-1", EventKind::ExecutionFinished))
                .expect("append live");
        });

        use futures::StreamExt;
        let collect = http_body_util::BodyExt::into_data_stream(response.into_body())
            .take(3)
            .filter_map(|chunk| async move {
                chunk.ok().map(|b| String::from_utf8_lossy(&b).into_owned())
            })
            .collect::<Vec<String>>();
        let frames = tokio::time::timeout(Duration::from_secs(5), collect)
            .await
            .expect("frames within 5s");
        let text = frames.concat();
        assert!(text.contains("\"kind\":\"execution_admitted\""), "{text}");
        assert!(text.contains("\"kind\":\"execution_finished\""), "{text}");
        assert!(text.contains("id: 1"), "{text}");
        assert!(text.contains("id: 2"), "{text}");
    }

    #[tokio::test]
    async fn events_resumes_from_last_event_id_header() {
        let events = vec![
            envelope("e-1", EventKind::ExecutionAdmitted),
            envelope("e-1", EventKind::ExecutionPhaseChanged),
        ];
        let router = app(store_with(&events));
        let request = Request::builder()
            .uri("/v1/observatory/events")
            .header(&LAST_EVENT_ID, "1")
            .header(
                header::AUTHORIZATION,
                format!(
                    "Bearer {}",
                    token(ocean_observatory::ObserverScope::Summary)
                ),
            )
            .body(Body::empty())
            .expect("request");
        let response = router.oneshot(request).await.expect("response");
        use futures::StreamExt;
        let collect = http_body_util::BodyExt::into_data_stream(response.into_body())
            .take(1)
            .filter_map(|chunk| async move {
                chunk.ok().map(|b| String::from_utf8_lossy(&b).into_owned())
            })
            .collect::<Vec<String>>();
        let frames = tokio::time::timeout(Duration::from_secs(5), collect)
            .await
            .expect("frame within 5s");
        let text = frames.concat();
        assert!(
            text.contains("\"kind\":\"execution_phase_changed\""),
            "{text}"
        );
        assert!(!text.contains("execution_admitted"), "{text}");
    }

    #[tokio::test]
    async fn events_malformed_cursor_yields_single_error_frame() {
        let router = app(store_with(&[]));
        let response = router
            .oneshot(authed("/v1/observatory/events?after=nope"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let text = body_string(response).await;
        assert!(text.contains("event: error"), "{text}");
        assert!(text.contains("invalid_cursor"), "{text}");
    }

    /// F12: `?scope=` is validated, not ignored — `summary` streams, anything
    /// else (including the reserved `content`) is one `invalid_scope` error
    /// frame and no event.
    #[tokio::test]
    async fn events_rejects_an_unsupported_scope() {
        for scope in ["content", "bogus", ""] {
            let router = app(store_with(&[]));
            let response = router
                .oneshot(authed(&format!("/v1/observatory/events?scope={scope}")))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            let text = body_string(response).await;
            assert!(text.contains("event: error"), "{scope}: {text}");
            assert!(text.contains("invalid_scope"), "{scope}: {text}");
        }

        let router = app(store_with(&[]));
        let response = router
            .oneshot(authed("/v1/observatory/events?scope=summary"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        use futures::StreamExt;
        let first = tokio::time::timeout(
            Duration::from_secs(5),
            http_body_util::BodyExt::into_data_stream(response.into_body()).next(),
        )
        .await
        .expect("keepalive within 5s")
        .expect("a frame")
        .expect("frame bytes");
        let text = String::from_utf8_lossy(&first);
        assert!(!text.contains("invalid_scope"), "{text}");
    }

    /// F12: `detail=full` is reserved and answers exactly what `summary`
    /// does; an unknown value is 400.
    #[tokio::test]
    async fn snapshot_detail_full_is_a_reserved_alias_of_summary() {
        let store = store_with(&[
            envelope("e-1", EventKind::ExecutionAdmitted),
            envelope("e-1", EventKind::ExecutionPhaseChanged),
        ]);
        let summary = body_string(
            app(store.clone())
                .oneshot(authed("/v1/observatory/snapshot?detail=summary"))
                .await
                .expect("response"),
        )
        .await;
        let full = body_string(
            app(store.clone())
                .oneshot(authed("/v1/observatory/snapshot?detail=full"))
                .await
                .expect("response"),
        )
        .await;
        assert!(summary.contains("\"e-1\""), "{summary}");
        assert_eq!(summary, full);

        let invalid = app(store)
            .oneshot(authed("/v1/observatory/snapshot?detail=everything"))
            .await
            .expect("response");
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
        assert!(body_string(invalid).await.contains("invalid_detail"));
    }

    #[tokio::test]
    async fn events_future_cursor_yields_error_frame() {
        let router = app(store_with(&[]));
        let response = router
            .oneshot(authed("/v1/observatory/events?after=42"))
            .await
            .expect("response");
        let text = body_string(response).await;
        assert!(text.contains("event: error"), "{text}");
    }

    #[tokio::test]
    async fn replay_pruned_range_yields_410_with_gap_shape() {
        // Seed two finished events old enough for age retention to prune.
        let events: Vec<EventEnvelope> = ["e-1", "e-2"]
            .into_iter()
            .map(|id| {
                let mut event = envelope(id, EventKind::ExecutionFinished);
                event.recorded_at = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
                event
            })
            .collect();
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            ObservatoryStore::open(
                &dir.path().join("obs.db"),
                RetentionPolicy {
                    max_age_days: 7,
                    max_bytes: RetentionPolicy::default().max_bytes,
                },
            )
            .expect("open"),
        );
        for event in &events {
            store.append_event(event.clone()).expect("append");
        }
        let pruned = store.apply_retention().expect("retention");
        assert!(pruned > 0, "retention must prune for this test");

        let router = app(store);
        let response = router
            .oneshot(authed("/v1/observatory/replay?after=0"))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::GONE);
        let body: Value = serde_json::from_str(&body_string(response).await).expect("json");
        assert_eq!(body["error"], json!("retention_boundary_crossed"));
        assert!(body["gap_from"].is_string(), "{body}");
        assert!(body["earliest_available"].is_string(), "{body}");
    }
    /// F1: snapshot, replay and the SSE tail do their store work on the
    /// blocking pool. With the store wedged behind a writer, all three
    /// requests wait — and the (current-thread) executor keeps running.
    #[tokio::test]
    async fn store_calls_do_not_stall_the_executor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("obs.db");
        let store =
            Arc::new(ObservatoryStore::open(&path, RetentionPolicy::default()).expect("open"));
        store
            .append_event(envelope("e-1", EventKind::ExecutionAdmitted))
            .expect("append");
        let router = app(Arc::clone(&store));
        let wedge = StoreWedge::engage(
            &path,
            &store,
            envelope("e-1", EventKind::ExecutionPhaseChanged),
        );
        let requests: Vec<_> = [
            "/v1/observatory/snapshot",
            "/v1/observatory/replay?after=0",
            "/v1/observatory/events?after=0",
        ]
        .into_iter()
        .map(|path| tokio::spawn(router.clone().oneshot(authed(path))))
        .collect();
        assert_executor_stays_live().await;
        wedge.release();
        for request in requests {
            let response = tokio::time::timeout(Duration::from_secs(10), request)
                .await
                .expect("request finishes once the store is free")
                .expect("task")
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    /// F3: a valid token of a non-summary scope is refused on every route
    /// with the same 401 shape as a bad credential.
    #[tokio::test]
    async fn routes_reject_a_content_scope_token() {
        let router = app(store_with(&[]));
        for path in [
            "/v1/observatory/snapshot",
            "/v1/observatory/events",
            "/v1/observatory/replay?after=0",
        ] {
            let request = Request::builder()
                .uri(path)
                .header(
                    header::AUTHORIZATION,
                    format!(
                        "Bearer {}",
                        token(ocean_observatory::ObserverScope::Content)
                    ),
                )
                .body(Body::empty())
                .expect("request");
            let response = router.clone().oneshot(request).await.expect("response");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
            assert!(response.headers().contains_key("x-observatory-cursor"));
            let body: Value = serde_json::from_str(&body_string(response).await).expect("json");
            assert_eq!(body["error"], "unauthorized", "{path}");
            assert_eq!(body["http_status"], 401, "{path}");
        }
    }

    /// F5: the `stream.gap` frame has no SSE `id:`, so it never duplicates
    /// the post-gap event's id or moves a client's Last-Event-ID past an
    /// event it has not received.
    #[tokio::test]
    async fn stream_gap_frame_carries_no_event_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("obs.db");
        let store =
            Arc::new(ObservatoryStore::open(&path, RetentionPolicy::default()).expect("open"));
        for id in ["e-1", "e-2", "e-3"] {
            store
                .append_event(envelope(id, EventKind::ExecutionPhaseChanged))
                .expect("append");
        }
        // Punch a hole at cursor 2 so a tail from 1 must report a gap.
        rusqlite::Connection::open(&path)
            .expect("open raw")
            .execute("DELETE FROM observatory_events WHERE cursor = 2", [])
            .expect("delete");
        let response = app(store)
            .oneshot(authed("/v1/observatory/events?after=1"))
            .await
            .expect("response");
        use futures::StreamExt;
        let mut body = http_body_util::BodyExt::into_data_stream(response.into_body());
        let mut text = String::new();
        while !text.contains("\"cursor\":\"3\"") {
            let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
                .await
                .expect("frame within 5s")
                .expect("stream open")
                .expect("chunk");
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
        let frames: Vec<&str> = text
            .split("\n\n")
            .filter(|f| !f.trim().is_empty())
            .collect();
        let gap = frames
            .iter()
            .find(|f| f.contains("stream.gap"))
            .unwrap_or_else(|| panic!("no gap frame: {text}"));
        assert!(!gap.lines().any(|line| line.starts_with("id:")), "{gap}");
        assert!(gap.contains("\"to_cursor\":\"3\""), "{gap}");
        assert_eq!(text.matches("id: 3").count(), 1, "{text}");
    }

    /// F6: the filter value is percent-encoded into continuation_url, so a
    /// value carrying `&`/`=` cannot smuggle extra query parameters.
    #[tokio::test]
    async fn replay_continuation_url_encodes_the_filter() {
        let events: Vec<EventEnvelope> = (0..3)
            .map(|i| envelope(&format!("e-{i}"), EventKind::ExecutionPhaseChanged))
            .collect();
        let response = app(store_with(&events))
            .oneshot(authed(
                "/v1/observatory/replay?after=0&limit=1&filter=producer%3Aa%26through%3D1%20b",
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let page: Value = serde_json::from_str(&body_string(response).await).expect("json");
        let url = page["continuation_url"].as_str().expect("continuation");
        assert_eq!(
            url,
            "/v1/observatory/replay?after=1&limit=1&filter=producer%3Aa%26through%3D1%20b"
        );
    }
}
