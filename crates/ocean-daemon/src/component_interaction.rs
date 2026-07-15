use axum::{http::StatusCode, Json};
use ocean_runtime::tools::component::COMPONENT_WAIT_REGISTRY;
use serde_json::{json, Value};

/// Receive a user interaction event for a rendered component and deliver it
/// to the waiting `component_wait` tool call.
///
/// Request body:
/// ```json
/// {
///   "session_id": "uuid-of-session",
///   "component_id": "agent-chosen-id",
///   "event": { "type": "submit", "data": { ... } }
/// }
/// ```
///
/// Returns 200 if the event was delivered, 404 if nobody is waiting on that
/// component, 400 on missing fields.
pub(super) async fn component_event(Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
    let session_id = match body.get("session_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "missing 'session_id'" })),
            );
        }
    };
    let component_id = match body.get("component_id").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "missing 'component_id'" })),
            );
        }
    };
    let event = body.get("event").cloned().unwrap_or(json!({}));

    let sender = {
        let mut pending = match COMPONENT_WAIT_REGISTRY.pending.lock() {
            Ok(g) => g,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("registry lock: {e}") })),
                );
            }
        };
        pending.remove(&(session_id.clone(), component_id.clone()))
    };

    match sender {
        Some(tx) => {
            if tx.send(event).is_err() {
                // Receiver dropped (timeout or cancellation) — not an error for the caller.
                (
                    StatusCode::GONE,
                    Json(json!({ "status": "nobody waiting" })),
                )
            } else {
                (StatusCode::OK, Json(json!({ "status": "delivered" })))
            }
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "error": "no pending wait for component", "session_id": session_id, "component_id": component_id }),
            ),
        ),
    }
}
