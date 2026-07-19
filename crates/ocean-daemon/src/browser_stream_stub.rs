//! No-engine stub for the browser-screencast backend, compiled when the
//! default-off `legacy-chromium` feature is disabled and this build has no
//! browser engine (the OceanWebKit browser host is not connected yet).
//!
//! It preserves the FROZEN wasm-client contract documented in
//! [`crate::browser_stream`] (the legacy-chromium module):
//!
//! - `GET /v1/browser/screencast` → SSE: while no browser is reachable the
//!   stream emits `event: status` with `{"state":"no-browser"}` and stays open
//!   with comment keep-alives; the shipped client already renders this state
//!   and keeps waiting (the legacy backend emits the same event while the
//!   agent's Chrome is down).
//! - `POST /v1/browser/input` → `200 {"error":"no-browser"}`, exactly the
//!   legacy "no Chrome reachable" response.
//!
//! Routes therefore never 404 across build modes and the client needs no
//! build-mode detection. No CDP client, no Chrome discovery, no chromiumoxide.

use std::convert::Infallible;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::sse::Event;
use axum::response::Sse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Keep-alive cadence while the no-browser SSE stream is parked. Matches the
/// legacy module's probe interval so proxies/clients see the same liveness
/// shape in both build modes.
const KEEPALIVE: Duration = Duration::from_secs(5);

/// Spawn the stub producer: one frozen `status {"state":"no-browser"}` event,
/// then comment keep-alives until the client disconnects.
fn stub_channel() -> mpsc::Receiver<Result<Event, Infallible>> {
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(8);
    tokio::spawn(async move {
        let status = Event::default()
            .event("status")
            .data(json!({ "state": "no-browser" }).to_string());
        if tx.send(Ok(status)).await.is_err() {
            return; // client already gone
        }
        loop {
            tokio::time::sleep(KEEPALIVE).await;
            // SSE comment keep-alive (no client-visible event).
            if tx
                .send(Ok(Event::default().comment("keep-alive")))
                .await
                .is_err()
            {
                break; // client disconnected
            }
        }
    });
    rx
}

/// SSE stream for `GET /v1/browser/screencast`. Emits the frozen
/// `status {"state":"no-browser"}` event once, then parks the connection with
/// SSE comment keep-alives until the client disconnects.
pub async fn screencast_stream() -> Sse<ReceiverStream<Result<Event, Infallible>>> {
    Sse::new(ReceiverStream::new(stub_channel()))
}

/// `POST /v1/browser/input`. Frozen no-browser response: `200
/// {"error":"no-browser"}` — identical to the legacy backend's answer when no
/// Chrome is reachable.
pub async fn input(Json(req): Json<BrowserInputRequest>) -> (StatusCode, Json<Value>) {
    let _ = req;
    (StatusCode::OK, Json(json!({ "error": "no-browser" })))
}

/// Parsed `POST /v1/browser/input` body. Mirrors the legacy module's request
/// shape so the route contract is identical across build modes.
#[derive(Debug, Deserialize)]
#[allow(dead_code)] // fields exist for wire compatibility; the stub never dispatches
pub struct BrowserInputRequest {
    pub kind: String,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub delta_y: Option<f64>,
    pub key: Option<String>,
    pub text: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn screencast_stub_emits_frozen_no_browser_status() {
        use axum::response::IntoResponse;
        use http_body_util::BodyExt;
        let body = screencast_stream().await.into_response().into_body();
        tokio::pin!(body);
        let first = tokio::time::timeout(Duration::from_secs(2), body.frame())
            .await
            .expect("status frame arrives promptly")
            .expect("stream is open")
            .expect("frame ok")
            .into_data()
            .expect("data frame");
        let text = String::from_utf8(first.to_vec()).expect("utf8");
        assert!(text.contains("status"), "missing event name: {text}");
        assert!(
            text.contains(r#"{"state":"no-browser"}"#),
            "missing frozen payload: {text}"
        );
    }

    #[tokio::test]
    async fn input_stub_returns_frozen_no_browser_error() {
        let (code, Json(body)) = input(Json(BrowserInputRequest {
            kind: "click".to_string(),
            x: Some(1.0),
            y: Some(2.0),
            delta_y: None,
            key: None,
            text: None,
        }))
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(body, json!({ "error": "no-browser" }));
    }
}
