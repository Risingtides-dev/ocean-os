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
//! `execution_nodes` projection does not yet carry session/turn/request ids,
//! so those wire fields are empty strings, and the attention shelf is empty
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

/// Stable observatory identity: persisted across daemon restarts, reset only
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
fn observatory_headers(watermark: Cursor, instance: &str) -> HeaderMap {
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

fn error_response(status: StatusCode, headers: HeaderMap, error: &str, message: &str) -> Response {
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
    let boundary = match store.retention_boundary() {
        Ok(boundary) => boundary,
        Err(error) => {
            tracing::error!(%error, "observatory snapshot retention-boundary read failed");
            return store_unavailable(headers);
        }
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

    let projection = match store.snapshot_at(at) {
        Ok(projection) => projection,
        Err(error) => {
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

#[derive(Debug, Deserialize)]
pub(crate) struct EventsQuery {
    after: Option<String>,
    #[allow(dead_code)]
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
    let earliest = match store.earliest_available_cursor() {
        Ok(cursor) => cursor,
        Err(error) => {
            tracing::error!(%error, "observatory events earliest-cursor read failed");
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
            let batch = match store.events_after(last, Some(LIVE_READ_BATCH)) {
                Ok(events) => events,
                Err(error) => {
                    tracing::error!(%error, "observatory live tail read failed");
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
                    let reason = match store.earliest_available_cursor() {
                        Ok(boundary) if boundary > expected => "retention_boundary",
                        _ => "cursor_jump",
                    };
                    let gap = axum::response::sse::Event::default()
                        .event("message")
                        .id(envelope.cursor.as_string())
                        .data(
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

    let boundary = match store.retention_boundary() {
        Ok(boundary) => boundary,
        Err(error) => {
            tracing::error!(%error, "observatory replay retention-boundary read failed");
            return store_unavailable(headers);
        }
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

    let page = match store.replay_page(after, through, limit) {
        Ok(page) => page,
        Err(error) => {
            tracing::error!(%error, "observatory replay read failed");
            return store_unavailable(headers);
        }
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
            kind: serde_json::to_value(envelope.kind)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned()),
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
                url.push_str(&format!("&filter={raw}"));
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
        }
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
        // Seed two finished events, then force retention past them.
        let events = vec![
            envelope("e-1", EventKind::ExecutionFinished),
            envelope("e-2", EventKind::ExecutionFinished),
        ];
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Arc::new(
            ObservatoryStore::open(
                &dir.path().join("obs.db"),
                RetentionPolicy {
                    max_age_days: 7,
                    max_bytes: 1, // force pruning by size
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
}
