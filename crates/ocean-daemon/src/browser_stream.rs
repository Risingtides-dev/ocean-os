//! Browser-screencast backend for Ocean Desktop's "Browser" tab.
//!
//! The shipped wasm client streams the agent's LIVE Chrome over two endpoints:
//!
//! - `GET /v1/browser/screencast` → SSE: `event: frame` with
//!   `{b64, w, h, ts}` (base64 JPEG + device size + timestamp) while a browser
//!   is attached, or `event: status` with `{"state":"no-browser"}` (re-trying
//!   attach every 2s) while none is.
//! - `POST /v1/browser/input` → dispatches `click` / `scroll` / `key` / `type`
//!   to the same page, returning `200 {}` on dispatch or
//!   `200 {"error":"no-browser"}` when no Chrome is reachable.
//!
//! ## How it reaches the agent's Chrome
//!
//! The daemon NEVER launches its own Chrome and never touches
//! `ocean-runtime`'s private `LazyBrowser`. It attaches as a SECOND CDP client
//! to the SAME Chrome the agent is already driving, via
//! [`ocean_browser::launch::attach_running`]. That resolves the agent's profile
//! dir (`<config_dir>/chrome-profile`, where `config_dir` comes from
//! [`ocean_agent::config_dir_from_env`]), reads the live port from that
//! profile's `DevToolsActivePort`, probes `/json/version`, then
//! `Browser::connect` (http endpoint → ws url) + spawns the CDP handler task.
//!
//! The client contract below is FROZEN (the wasm client already ships against
//! it): JPEG quality ~60, `maxWidth` 1280, ACK every frame, frame-pixel
//! coordinates for input.

use std::convert::Infallible;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::sse::Event;
use axum::response::Sse;
use axum::Json;
use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::browser_protocol::page::{
    EventScreencastFrame, ScreencastFrameAckParams, ScreencastFrameMetadata, StartScreencastFormat,
    StartScreencastParams, StopScreencastParams,
};
use chromiumoxide::page::Page;
use ocean_browser::LaunchConfig;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

/// JPEG quality for the screencast (frozen client contract: ~60).
const CAST_QUALITY: i64 = 60;
/// Maximum frame width in CSS pixels (frozen client contract: 1280). Chrome
/// preserves aspect ratio, so `maxHeight` is left unset.
const CAST_MAX_WIDTH: i64 = 1280;
/// No-browser attach-retry cadence (frozen client contract: every 2s).
const ATTACH_RETRY: Duration = Duration::from_secs(2);
/// While attached, the per-client forwarder wakes this often to (a) emit an SSE
/// comment keep-alive and (b) probe whether the HTTP client has gone away (an
/// idle page stops producing frames, so this bounds stale-task lifetime).
const KEEPALIVE_PROBE: Duration = Duration::from_secs(5);
/// Per-SSE-client outbound queue. Bounded so a slow client backpressures instead
/// of buffering unbounded frames in the daemon.
const CLIENT_CHANNEL_DEPTH: usize = 64;
/// Broadcast fan-out depth for one screencast. Large enough to absorb a brief
/// frame burst; lagging clients get a `lag` event and catch up.
const CAST_CHANNEL_DEPTH: usize = 64;

// ── shared singleton state ──────────────────────────────────────────────────
//
// One process-wide attach + screencast. Held behind a `tokio::Mutex` because the
// attach / start-screencast / stop-screencast paths all await CDP round-trips
// under it. The daemon is a low-traffic surface here (a handful of Desktop
// tabs), so holding the lock across those awaits is correct and cheap.

#[derive(Default)]
struct StreamState {
    /// Cached attach to the agent's Chrome: the connected `Browser` + the cast
    /// `Page`. `None` until a first successful attach; cleared on connection
    /// drop so the next call re-probes the profile's `DevToolsActivePort` (which
    /// also covers an endpoint change after a Chrome restart).
    attach: Option<Attach>,
    /// The single live screencast broadcast shared by EVERY SSE client. One
    /// forward task listens for `Page.screencastFrame`, ACKs every frame, and
    /// fans each out as a JSON `{b64,w,h,ts}` payload. `None` while idle; the
    /// last disconnect stops the cast.
    cast: Option<Cast>,
}

struct Attach {
    /// Kept alive (owns the CDP websocket session); not otherwise used directly.
    _browser: Browser,
    page: Page,
}

struct Cast {
    tx: broadcast::Sender<Value>,
    /// The frame-forwarding task; aborted when the last client leaves.
    forward: tokio::task::JoinHandle<()>,
}

static STATE: LazyLock<Arc<Mutex<StreamState>>> =
    LazyLock::new(|| Arc::new(Mutex::new(StreamState::default())));

fn state() -> Arc<Mutex<StreamState>> {
    STATE.clone()
}

// ── public API (thin axum handlers in main.rs delegate here) ────────────────

/// SSE stream for `GET /v1/browser/screencast`.
///
/// Emits `frame` events while a browser is attached; emits
/// `status {"state":"no-browser"}` and re-tries attach every 2s while none is.
/// Multiple concurrent clients share ONE screencast broadcast — the cast is
/// started on first connect and stopped when the last client disconnects, so a
pub async fn screencast_stream() -> Sse<ReceiverStream<Result<Event, Infallible>>> {
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(CLIENT_CHANNEL_DEPTH);
    let state = state();
    // Per-request token: dropped when this SSE stream's rx ends on client
    // disconnect, letting run_client tear down its CDP listener + screencast.
    let shutdown = CancellationToken::new();
    tokio::spawn(run_client(state, tx, shutdown));
    Sse::new(ReceiverStream::new(rx))
}

/// `POST /v1/browser/input`. Dispatches the parsed event to the agent's page.
/// `200 {"error":"no-browser"}` when no Chrome is reachable; `200 {}` on
/// dispatch; `500 {"error": ".."}` on a CDP failure.
pub async fn input(Json(req): Json<BrowserInputRequest>) -> (StatusCode, Json<Value>) {
    let page = {
        let st = state();
        let mut s = st.lock().await;
        match ensure_page_locked(&mut s).await {
            Some(p) => p,
            None => return no_browser(),
        }
    };
    match dispatch_input(&page, &req).await {
        Ok(()) => (StatusCode::OK, Json(json!({}))),
        Err(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": msg })),
        ),
    }
}

/// Parsed `POST /v1/browser/input` body. Coordinates are frame-pixel space.
#[derive(Debug, Deserialize)]
pub struct BrowserInputRequest {
    pub kind: String,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub delta_y: Option<f64>,
    pub key: Option<String>,
    pub text: Option<String>,
}

// ── per-SSE-client loop ─────────────────────────────────────────────────────

async fn run_client(
    state: Arc<Mutex<StreamState>>,
    tx: mpsc::Sender<Result<Event, Infallible>>,
    shutdown: CancellationToken,
) {
    // Our share of the single screencast broadcast. `None` while we have no cast
    // (no browser yet, or the previous cast stopped underneath us).
    let mut sub: Option<broadcast::Receiver<Value>> = None;
    let mut tick = tokio::time::interval(KEEPALIVE_PROBE);
    // The first tick fires immediately; skip it so we don't emit a keep-alive
    // before the first real frame/status.
    tick.tick().await;

    loop {
        if let Some(rx) = sub.as_mut() {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                recv = rx.recv() => match recv {
                    Ok(payload) => {
                        let evt = Event::default()
                            .event("frame")
                            .data(payload.to_string());
                        if tx.send(Ok(evt)).await.is_err() {
                            break; // HTTP client gone
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // The forward task stopped (page/browser gone) — drop our
                        // receiver and loop back to re-establish the cast.
                        sub = None;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // We fell behind the ring; log a `lag` event and continue.
                        // The next frame arrives normally.
                        let _ = tx.send(Ok(Event::default()
                            .event("lag")
                            .data(json!({ "skipped": n }).to_string()))).await;
                    }
                },
                _ = tick.tick() => {
                    // Idle keep-alive + disconnect probe: a comment line keeps
                    // the SSE socket warm and surfaces a closed client promptly.
                    if tx.send(Ok(Event::default().comment("ocean"))).await.is_err() {
                        break;
                    }
                }
            }
        } else {
            match ensure_cast(&state).await {
                Some(receiver) => sub = Some(receiver),
                None => {
                    if tx
                        .send(Ok(Event::default()
                            .event("status")
                            .data(status_no_browser_payload().to_string())))
                        .await
                        .is_err()
                    {
                        break;
                    }
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => break,
                        _ = tokio::time::sleep(ATTACH_RETRY) => {}
                    }
                }
            }
        }
    }

    // Release our broadcast receiver BEFORE the idle-cast reaper checks the
    // subscriber count, so our own slot isn't counted.
    drop(sub);
    release_cast_if_idle(&state).await;
}

/// Idempotently ensure a live screencast is running and hand the caller a
/// broadcast receiver. `None` ⇔ no browser is currently reachable.
async fn ensure_cast(state: &Arc<Mutex<StreamState>>) -> Option<broadcast::Receiver<Value>> {
    let mut s = state.lock().await;
    // Already casting? Just hand out another receiver.
    if let Some(cast) = s.cast.as_ref() {
        return Some(cast.tx.subscribe());
    }
    let page = ensure_page_locked(&mut s).await?;

    // Start the cast + spawn the forwarder. Holding the lock across these CDP
    // round-trips makes start atomic: two concurrent clients can't race to
    // double-start `Page.startScreencast` on the same page.
    if page.execute(start_screencast_params()).await.is_err() {
        // The page/browser died under us — clear the attach so the next call
        // re-probes the endpoint instead of trusting a dead handle.
        s.attach = None;
        return None;
    }
    let (tx, _first_rx) = broadcast::channel::<Value>(CAST_CHANNEL_DEPTH);
    let forward = tokio::spawn(forward_frames(page.clone(), tx.clone()));
    let cast = Cast { tx, forward };
    s.cast = Some(cast);
    Some(s.cast.as_ref().unwrap().tx.subscribe())
}

/// One forwarder per active screencast. Listens for `Page.screencastFrame`,
/// ACKs EVERY frame (Chrome blocks on unacked frames), and fans each out to all
/// subscribers as the frozen `{b64,w,h,ts}` JSON payload. Ends when the frame
/// stream ends (page closed / browser gone) — the next client then re-attaches.
async fn forward_frames(page: Page, tx: broadcast::Sender<Value>) {
    let mut frames = match page.event_listener::<EventScreencastFrame>().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "screencast event listener failed");
            return;
        }
    };
    while let Some(frame) = frames.next().await {
        // ACK every frame — the frozen contract requires it, and Chrome throttles
        // new frames until the prior one is acked.
        let _ = page
            .execute(ScreencastFrameAckParams::new(frame.session_id))
            .await;
        let payload = frame_payload(
            frame.data.as_ref(),
            frame.metadata.device_width,
            frame.metadata.device_height,
            ts_of(&frame.metadata),
        );
        // No subscribers → send errors harmlessly; keep listening + ACKing so
        // Chrome's frame queue can't back up between clients.
        let _ = tx.send(payload);
    }
    tracing::debug!("screencast frame stream ended");
}

/// Stop the cast if every subscriber has gone (last-client-disconnect teardown).
async fn release_cast_if_idle(state: &Arc<Mutex<StreamState>>) {
    let mut s = state.lock().await;
    let idle = s.cast.as_ref().is_some_and(|c| c.tx.receiver_count() == 0);
    if idle {
        stop_cast_locked(&mut s).await;
    }
}

/// Stop the screencast (best-effort `Page.stopScreencast`) and abort the
/// forwarder. Caller already holds the state lock.
async fn stop_cast_locked(s: &mut StreamState) {
    if let Some(a) = s.attach.as_ref() {
        let _ = a.page.execute(StopScreencastParams::default()).await;
    }
    if let Some(cast) = s.cast.take() {
        cast.forward.abort();
    }
}

// ── attach / page selection ─────────────────────────────────────────────────

/// Resolve (or re-use) the agent's Chrome + a castable page. Re-attaches on
/// connection drop or endpoint change. Caller holds the state lock.
async fn ensure_page_locked(s: &mut StreamState) -> Option<Page> {
    if let Some(a) = s.attach.as_ref() {
        if attach_alive(&a.page).await {
            return Some(a.page.clone());
        }
        // Dead handle — drop it (and any stale cast pointed at the dead page).
        s.attach = None;
        stop_cast_locked(s).await;
    }
    let cfg = daemon_launch_config();
    let browser = ocean_browser::launch::attach_running(&cfg).await?;
    let page = pick_page(&browser).await?;
    let out = page.clone();
    s.attach = Some(Attach {
        _browser: browser,
        page,
    });
    Some(out)
}

/// Choose the most-recently-active non-devtools page (focused tab), falling back
/// to the first non-devtools page. Never creates a new tab — the daemon is a
/// second client and must not open windows in the user's Chrome.
async fn pick_page(browser: &Browser) -> Option<Page> {
    let pages = browser.pages().await.ok()?;
    let mut first: Option<Page> = None;
    for p in pages {
        let url = p.url().await.ok().flatten().unwrap_or_default();
        if is_devtools_or_internal(&url) {
            continue;
        }
        if first.is_none() {
            first = Some(p.clone());
        }
        let focused = p
            .evaluate("document.hasFocus()")
            .await
            .ok()
            .and_then(|r| r.value().and_then(|v| v.as_bool()))
            .unwrap_or(false);
        if focused {
            return Some(p);
        }
    }
    first
}

/// The cached handle is alive iff the page still answers CDP. A dead websocket
/// / closed tab / killed browser all surface here as an error.
async fn attach_alive(page: &Page) -> bool {
    page.url().await.is_ok()
}

fn is_devtools_or_internal(url: &str) -> bool {
    url.starts_with("devtools://")
        || url.starts_with("chrome-extension://")
        || url.starts_with("chrome://")
        || url.starts_with("chrome-untrusted://")
}

/// Build the SAME `LaunchConfig` the agent uses, so `running_cdp_endpoint` reads
/// the same `DevToolsActivePort`. Only `profile_dir` matters for attach — it
/// points at `<config_dir>/chrome-profile`, where `config_dir` is the agent's
/// env-resolved root (`OCEAN_CONFIG_DIR` → `XDG_CONFIG_HOME/ocean-rs` →
/// `~/.config/ocean-rs` → `./.ocean-rs`).
fn daemon_launch_config() -> LaunchConfig {
    let config_dir = ocean_agent::config_dir_from_env();
    LaunchConfig {
        profile_dir: config_dir.join("chrome-profile"),
        profile_directory: None,
        extension_dir: None,
        chrome_executable: None,
        headless: false,
        port: 0,
    }
}

// ── input dispatch ──────────────────────────────────────────────────────────

async fn dispatch_input(page: &Page, req: &BrowserInputRequest) -> Result<(), String> {
    match req.kind.as_str() {
        "click" => {
            let (x, y) = required_xy(req)?;
            page.execute(click_press(x, y)?).await.map_err(cdpe)?;
            page.execute(click_release(x, y)?).await.map_err(cdpe)?;
            Ok(())
        }
        "scroll" => {
            let x = req.x.unwrap_or(0.0);
            let y = req.y.unwrap_or(0.0);
            let delta_y = req.delta_y.unwrap_or(0.0);
            page.execute(scroll_wheel(x, y, delta_y)?)
                .await
                .map_err(cdpe)?;
            Ok(())
        }
        "key" => {
            let key = req.key.as_deref().ok_or("missing 'key'")?;
            let def = named_key_def(key).ok_or_else(|| format!("unknown named key: {key}"))?;
            page.execute(key_down(&def)).await.map_err(cdpe)?;
            page.execute(key_up(&def)).await.map_err(cdpe)?;
            Ok(())
        }
        "type" => {
            let text = req.text.as_deref().ok_or("missing 'text'")?;
            page.execute(InsertTextParams::new(text))
                .await
                .map_err(cdpe)?;
            Ok(())
        }
        other => Err(format!("unknown input kind: {other}")),
    }
}

fn required_xy(req: &BrowserInputRequest) -> Result<(f64, f64), String> {
    match (req.x, req.y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err("click requires 'x' and 'y'".to_string()),
    }
}

fn cdpe(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn no_browser() -> (StatusCode, Json<Value>) {
    (StatusCode::OK, Json(json!({ "error": "no-browser" })))
}

// ── CDP param builders (pure — unit tested against their serialized shape) ───

fn start_screencast_params() -> StartScreencastParams {
    StartScreencastParams::builder()
        .format(StartScreencastFormat::Jpeg)
        .quality(CAST_QUALITY)
        .max_width(CAST_MAX_WIDTH)
        .every_nth_frame(1)
        .build()
}

fn click_press(x: f64, y: f64) -> Result<DispatchMouseEventParams, String> {
    DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MousePressed)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
}

fn click_release(x: f64, y: f64) -> Result<DispatchMouseEventParams, String> {
    DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseReleased)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
}

fn scroll_wheel(x: f64, y: f64, delta_y: f64) -> Result<DispatchMouseEventParams, String> {
    DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseWheel)
        .x(x)
        .y(y)
        .delta_y(delta_y)
        .build()
}

/// A named-key definition: the DOM `key`, the physical `code`, and the Windows
/// virtual-key code (CDP wants both vk fields; Chrome on macOS maps them fine).
#[derive(Debug, PartialEq, Eq)]
struct KeyDef {
    key: &'static str,
    code: &'static str,
    vk: i64,
}

fn key_down(def: &KeyDef) -> DispatchKeyEventParams {
    DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::RawKeyDown)
        .key(def.key)
        .code(def.code)
        .windows_virtual_key_code(def.vk)
        .native_virtual_key_code(def.vk)
        .build()
        .expect("named-key params are statically valid")
}

fn key_up(def: &KeyDef) -> DispatchKeyEventParams {
    DispatchKeyEventParams::builder()
        .r#type(DispatchKeyEventType::KeyUp)
        .key(def.key)
        .code(def.code)
        .windows_virtual_key_code(def.vk)
        .native_virtual_key_code(def.vk)
        .build()
        .expect("named-key params are statically valid")
}

/// The frozen named-key → `(key, code, vk)` table the `key` input kind accepts.
/// KeyboardEvent `code` values + Windows virtual-key codes.
fn named_key_def(key: &str) -> Option<KeyDef> {
    Some(match key {
        "Enter" => KeyDef {
            key: "Enter",
            code: "Enter",
            vk: 13,
        },
        "Backspace" => KeyDef {
            key: "Backspace",
            code: "Backspace",
            vk: 8,
        },
        "Tab" => KeyDef {
            key: "Tab",
            code: "Tab",
            vk: 9,
        },
        "Escape" => KeyDef {
            key: "Escape",
            code: "Escape",
            vk: 27,
        },
        "ArrowUp" => KeyDef {
            key: "ArrowUp",
            code: "ArrowUp",
            vk: 38,
        },
        "ArrowDown" => KeyDef {
            key: "ArrowDown",
            code: "ArrowDown",
            vk: 40,
        },
        "ArrowLeft" => KeyDef {
            key: "ArrowLeft",
            code: "ArrowLeft",
            vk: 37,
        },
        "ArrowRight" => KeyDef {
            key: "ArrowRight",
            code: "ArrowRight",
            vk: 39,
        },
        _ => return None,
    })
}

// ── frame / status payload shaping (pure — unit tested) ─────────────────────

/// Frozen `frame` payload: `{b64, w, h, ts}`. `b64` is the base64 JPEG straight
/// off the wire (chromiumoxide deserializes CDP's `data` field verbatim), and
/// `w`/`h` are the device dimensions the client uses to map frame-pixel coords.
fn frame_payload(b64: &str, w: f64, h: f64, ts: f64) -> Value {
    json!({ "b64": b64, "w": w as u32, "h": h as u32, "ts": ts })
}

/// Frozen `status` payload for the no-browser state.
fn status_no_browser_payload() -> Value {
    json!({ "state": "no-browser" })
}

/// Frame timestamp: prefer CDP's frame-swap timestamp (a `TimeSinceEpoch`
/// seconds value); fall back to the daemon wall clock if Chrome omitted it.
fn ts_of(meta: &ScreencastFrameMetadata) -> f64 {
    meta.timestamp
        .as_ref()
        .map(|t| *t.inner())
        .unwrap_or_else(wall_secs)
}

fn wall_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── named-key code table ────────────────────────────────────────────────

    #[test]
    fn named_key_table_covers_contract_keys_and_rejects_unknown() {
        // Every key the frozen contract names.
        assert_eq!(
            named_key_def("Enter"),
            Some(KeyDef {
                key: "Enter",
                code: "Enter",
                vk: 13
            })
        );
        assert_eq!(
            named_key_def("Backspace"),
            Some(KeyDef {
                key: "Backspace",
                code: "Backspace",
                vk: 8
            })
        );
        assert_eq!(
            named_key_def("Tab"),
            Some(KeyDef {
                key: "Tab",
                code: "Tab",
                vk: 9
            })
        );
        assert_eq!(
            named_key_def("Escape"),
            Some(KeyDef {
                key: "Escape",
                code: "Escape",
                vk: 27
            })
        );
        assert_eq!(
            named_key_def("ArrowUp"),
            Some(KeyDef {
                key: "ArrowUp",
                code: "ArrowUp",
                vk: 38
            })
        );
        assert_eq!(
            named_key_def("ArrowDown"),
            Some(KeyDef {
                key: "ArrowDown",
                code: "ArrowDown",
                vk: 40
            })
        );
        assert_eq!(
            named_key_def("ArrowLeft"),
            Some(KeyDef {
                key: "ArrowLeft",
                code: "ArrowLeft",
                vk: 37
            })
        );
        assert_eq!(
            named_key_def("ArrowRight"),
            Some(KeyDef {
                key: "ArrowRight",
                code: "ArrowRight",
                vk: 39
            })
        );
        // Unknown keys (printable chars, function keys, …) are rejected — the
        // caller must use the `type` kind for text.
        assert_eq!(named_key_def("a"), None);
        assert_eq!(named_key_def("F5"), None);
        assert_eq!(named_key_def(""), None);
    }

    // ── kind → CDP param mapping (via the serialized wire shape) ────────────

    #[test]
    fn click_maps_to_left_press_then_release_clickcount_one() {
        let press = serde_json::to_value(click_press(10.0, 20.0).unwrap()).unwrap();
        assert_eq!(press["type"], "mousePressed");
        assert_eq!(press["x"], 10.0);
        assert_eq!(press["y"], 20.0);
        assert_eq!(press["button"], "left");
        assert_eq!(press["clickCount"], 1);

        let release = serde_json::to_value(click_release(10.0, 20.0).unwrap()).unwrap();
        assert_eq!(release["type"], "mouseReleased");
        assert_eq!(release["x"], 10.0);
        assert_eq!(release["y"], 20.0);
        assert_eq!(release["button"], "left");
        assert_eq!(release["clickCount"], 1);
    }

    #[test]
    fn scroll_maps_to_mousewheel_with_delta_y() {
        let wheel = serde_json::to_value(scroll_wheel(5.0, 6.0, -240.0).unwrap()).unwrap();
        assert_eq!(wheel["type"], "mouseWheel");
        assert_eq!(wheel["x"], 5.0);
        assert_eq!(wheel["y"], 6.0);
        assert_eq!(wheel["deltaY"], -240.0);
    }

    #[test]
    fn key_maps_to_rawkeydown_then_keyup_with_key_code_and_vk() {
        let def = named_key_def("ArrowUp").unwrap();
        let down = serde_json::to_value(key_down(&def)).unwrap();
        assert_eq!(down["type"], "rawKeyDown");
        assert_eq!(down["key"], "ArrowUp");
        assert_eq!(down["code"], "ArrowUp");
        assert_eq!(down["windowsVirtualKeyCode"], 38);
        assert_eq!(down["nativeVirtualKeyCode"], 38);

        let up = serde_json::to_value(key_up(&def)).unwrap();
        assert_eq!(up["type"], "keyUp");
        assert_eq!(up["key"], "ArrowUp");
        assert_eq!(up["code"], "ArrowUp");
    }

    #[test]
    fn click_without_xy_is_rejected_by_required_xy() {
        let req = BrowserInputRequest {
            kind: "click".into(),
            x: None,
            y: Some(3.0),
            delta_y: None,
            key: None,
            text: None,
        };
        assert!(required_xy(&req).is_err());
    }

    // ── frame / status payload shape ────────────────────────────────────────

    #[test]
    fn frame_payload_is_b64_w_h_ts_exactly() {
        let p = frame_payload("QkFTRTY0", 1280.0, 720.0, 12.5);
        assert_eq!(p["b64"], "QkFTRTY0");
        assert_eq!(p["w"], 1280);
        assert_eq!(p["h"], 720);
        assert_eq!(p["ts"], 12.5);
        // No extra fields beyond the frozen contract.
        assert_eq!(p.as_object().unwrap().len(), 4);
    }

    #[test]
    fn status_no_browser_payload_shape() {
        let p = status_no_browser_payload();
        assert_eq!(p["state"], "no-browser");
        assert_eq!(p.as_object().unwrap().len(), 1);
    }

    // ── screencast config honors the frozen contract ────────────────────────

    #[test]
    fn screencast_is_jpeg_quality_60_maxwidth_1280_every_frame() {
        let p = serde_json::to_value(start_screencast_params()).unwrap();
        assert_eq!(p["format"], "jpeg");
        assert_eq!(p["quality"], 60);
        assert_eq!(p["maxWidth"], 1280);
        // every_nth_frame=1 ⇒ every frame is delivered (the "ack every frame"
        // contract presupposes one frame at a time).
        assert_eq!(p["everyNthFrame"], 1);
    }
}
