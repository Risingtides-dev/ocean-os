//! Minimal localhost HTTP/1.1 callback server.
//!
//! Mirrors OMP's `registry/oauth/callback-server.ts`: bind `127.0.0.1` first,
//! parse only the GET request line, route between the callback path, a `/launch`
//! 302 shortcut, and a 404 for everything else (browser favicons must not
//! consume the flow). Keeps serving until the callback resolves or the
//! [`super::FLOW_TIMEOUT_SECS`] grace period elapses (enforced by the caller).
//! The browser always receives its response before the flow is resolved.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const LAUNCH_PATH: &str = "/launch";

/// Outcome of a single callback hit.
#[derive(Debug)]
pub(crate) enum CallbackResult {
    /// A valid authorization code + echoed state.
    Ok { code: String, state: String },
    /// A terminal error detected at the callback (provider `error`, missing
    /// code, or state mismatch).
    Err(String),
}

/// How and where to bind the callback server for a given provider.
pub(crate) struct BindSpec {
    pub callback_path: &'static str,
    pub preferred_port: u16,
    pub allow_fallback: bool,
    /// When set, this exact URI is advertised to the provider and no port
    /// fallback is permitted (Codex only allows its registered URI).
    pub fixed_redirect_uri: Option<&'static str>,
    pub label: &'static str,
}

struct ServerShared {
    callback_path: String,
    expected_state: String,
    /// Pending authorize URL served by `/launch`; `None` ⇒ 503.
    pending: Mutex<Option<String>>,
    /// One-shot resolver, taken on the first callback resolution.
    tx: Mutex<Option<oneshot::Sender<CallbackResult>>>,
}

pub(crate) struct CallbackServer {
    pub redirect_uri: String,
    pub launch_url: String,
    shared: Arc<ServerShared>,
    rx: Option<oneshot::Receiver<CallbackResult>>,
    accept_task: Option<JoinHandle<()>>,
}

impl CallbackServer {
    pub async fn bind(spec: &BindSpec, expected_state: String) -> Result<Self> {
        let listener = bind_loopback(spec).await?;
        let port = listener.local_addr()?.port();
        let redirect_uri = spec
            .fixed_redirect_uri
            .map(str::to_string)
            .unwrap_or_else(|| format!("http://localhost:{port}{}", spec.callback_path));
        let launch_url = format!("http://localhost:{port}{LAUNCH_PATH}");

        let (tx, rx) = oneshot::channel();
        let shared = Arc::new(ServerShared {
            callback_path: spec.callback_path.to_string(),
            expected_state,
            pending: Mutex::new(None),
            tx: Mutex::new(Some(tx)),
        });

        let accept_shared = shared.clone();
        let accept_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _addr)) => {
                        let s = accept_shared.clone();
                        tokio::spawn(async move {
                            handle_connection(stream, s).await;
                        });
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "callback accept error; stopping loop");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            redirect_uri,
            launch_url,
            shared,
            rx: Some(rx),
            accept_task: Some(accept_task),
        })
    }

    pub fn set_pending_url(&self, url: String) {
        *self.shared.pending.lock() = Some(url);
    }

    pub fn clear_pending(&self) {
        *self.shared.pending.lock() = None;
    }

    /// Await the one and only callback resolution.
    pub async fn next_result(&mut self) -> Result<CallbackResult> {
        let rx = self
            .rx
            .take()
            .context("callback receiver already consumed")?;
        match rx.await {
            Ok(result) => Ok(result),
            Err(_) => Err(anyhow!(
                "callback server stopped without resolving the login"
            )),
        }
    }
}

impl Drop for CallbackServer {
    fn drop(&mut self) {
        if let Some(task) = self.accept_task.take() {
            task.abort();
        }
    }
}

async fn bind_loopback(spec: &BindSpec) -> Result<TcpListener> {
    match TcpListener::bind(("127.0.0.1", spec.preferred_port)).await {
        Ok(listener) => Ok(listener),
        Err(err) => {
            if spec.allow_fallback {
                let listener = TcpListener::bind(("127.0.0.1", 0))
                    .await
                    .with_context(|| {
                        format!(
                            "callback port {} unavailable and ephemeral bind failed: {err}",
                            spec.preferred_port
                        )
                    })?;
                let ephemeral = listener.local_addr()?.port();
                tracing::warn!(
                    label = spec.label,
                    preferred = spec.preferred_port,
                    ephemeral,
                    "preferred callback port unavailable; using ephemeral port"
                );
                Ok(listener)
            } else {
                Err(anyhow!(
                    "port {} busy (another {} login running?) — free it and retry: {err}",
                    spec.preferred_port,
                    spec.label
                ))
            }
        }
    }
}

async fn handle_connection(mut stream: TcpStream, shared: Arc<ServerShared>) {
    // Minimal HTTP/1.1: the request line is all we need. Read until a newline
    // appears (it arrives first) or 8 KiB, then ignore the rest.
    let mut data = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        match stream.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => data.extend_from_slice(&tmp[..n]),
            Err(_) => return,
        }
        if data.contains(&b'\n') || data.len() > 8192 {
            break;
        }
    }

    let newline = data.iter().position(|&b| b == b'\n').unwrap_or(data.len());
    let request_line = std::str::from_utf8(&data[..newline])
        .unwrap_or("")
        .trim_end_matches('\r');
    let mut parts = request_line.split_whitespace();
    let _method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    if path == shared.callback_path.as_str() {
        handle_callback(&mut stream, query, &shared).await;
    } else if path == LAUNCH_PATH {
        let pending = shared.pending.lock().clone();
        match pending {
            Some(url) => write_redirect(&mut stream, &url).await,
            None => write_response(
                &mut stream,
                503,
                "text/plain; charset=utf-8",
                "OAuth launch URL is no longer active",
            )
            .await,
        }
    } else {
        write_response(&mut stream, 404, "text/plain; charset=utf-8", "Not Found").await;
    }
}

async fn handle_callback(stream: &mut TcpStream, query: &str, shared: &ServerShared) {
    let code = crate::util::query_get(query, "code");
    let state = crate::util::query_get(query, "state").unwrap_or_default();
    let error = crate::util::query_get(query, "error");
    let error_description = crate::util::query_get(query, "error_description");

    let result = if let Some(err) = error.as_deref() {
        let detail = error_description.unwrap_or_else(|| err.to_string());
        CallbackResult::Err(format!("authorization failed: {detail}"))
    } else if code.is_none() {
        CallbackResult::Err("missing authorization code".to_string())
    } else if state != shared.expected_state {
        CallbackResult::Err("state mismatch - possible CSRF attack".to_string())
    } else {
        CallbackResult::Ok {
            code: code.unwrap(),
            state,
        }
    };

    let (status, body) = match &result {
        CallbackResult::Ok { .. } => (200u16, success_html()),
        CallbackResult::Err(msg) => (500u16, error_html(msg)),
    };
    // Respond to the browser BEFORE resolving the flow / performing exchange.
    write_response(stream, status, "text/html; charset=utf-8", &body).await;

    let sender = shared.tx.lock().take();
    if let Some(sender) = sender {
        let _ = sender.send(result);
    }
}

async fn write_response(stream: &mut TcpStream, status: u16, content_type: &str, body: &str) {
    let reason = reason_phrase(status);
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

async fn write_redirect(stream: &mut TcpStream, location: &str) {
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

fn success_html() -> String {
    "<!doctype html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"><title>Ocean — login</title></head>\n<body><h1>Ocean — login complete.</h1><p>You can close this tab.</p></body>\n</html>\n".to_string()
}

fn error_html(message: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head><meta charset=\"utf-8\"><title>Ocean — login</title></head>\n<body><h1>Ocean — login failed.</h1><p>{}</p></body>\n</html>\n",
        html_escape(message)
    )
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{BindSpec, CallbackResult, CallbackServer};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// A Claude-shaped spec bound to an OS-assigned ephemeral port (preferred
    /// port 0), so parallel tests never collide on 54545.
    fn ephemeral_spec() -> BindSpec {
        BindSpec {
            callback_path: "/callback",
            preferred_port: 0,
            allow_fallback: true,
            fixed_redirect_uri: None,
            label: "test",
        }
    }

    /// Parse the bound port out of the advertised redirect_uri.
    fn port_of(server: &CallbackServer) -> u16 {
        let rest = server
            .redirect_uri
            .strip_prefix("http://localhost:")
            .expect("redirect_uri is localhost");
        rest.split('/').next().expect("port").parse().expect("numeric")
    }

    async fn get(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        let req = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.expect("write");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        String::from_utf8_lossy(&buf).into_owned()
    }

    fn status_and_body(resp: &str) -> (u16, &str) {
        let status = resp
            .split_whitespace()
            .nth(1)
            .expect("status token")
            .parse()
            .expect("numeric status");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
        (status, body)
    }

    #[tokio::test]
    async fn success_callback_resolves_and_serves_200_first() {
        let mut server = CallbackServer::bind(&ephemeral_spec(), "STATE".to_string())
            .await
            .expect("bind");
        let port = port_of(&server);
        // The browser receives its full 200 + success page...
        let resp = get(port, "/callback?code=THECODE&state=STATE").await;
        let (status, body) = status_and_body(&resp);
        assert_eq!(status, 200);
        assert!(body.contains("login complete"), "body: {body}");
        // ...BEFORE the flow is resolved (we only await the resolution now),
        // proving the browser response is not gated on resolution/exchange.
        match server.next_result().await.expect("result") {
            CallbackResult::Ok { code, state } => {
                assert_eq!(code, "THECODE");
                assert_eq!(state, "STATE");
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wrong_state_is_http_500_and_csrf_error() {
        let mut server = CallbackServer::bind(&ephemeral_spec(), "GOOD".to_string())
            .await
            .expect("bind");
        let port = port_of(&server);
        let resp = get(port, "/callback?code=x&state=BAD").await;
        let (status, body) = status_and_body(&resp);
        assert_eq!(status, 500);
        assert!(body.contains("state mismatch"), "body: {body}");
        match server.next_result().await.expect("result") {
            CallbackResult::Err(msg) => assert!(msg.contains("state mismatch"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn provider_error_is_authorization_failed() {
        let mut server = CallbackServer::bind(&ephemeral_spec(), "S".to_string())
            .await
            .expect("bind");
        let port = port_of(&server);
        let resp =
            get(port, "/callback?error=access_denied&error_description=user%20declined").await;
        let (status, body) = status_and_body(&resp);
        assert_eq!(status, 500);
        assert!(
            body.contains("authorization failed: user declined"),
            "body: {body}"
        );
        match server.next_result().await.expect("result") {
            CallbackResult::Err(msg) => assert!(msg.contains("authorization failed"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn missing_code_is_error() {
        let mut server = CallbackServer::bind(&ephemeral_spec(), "S".to_string())
            .await
            .expect("bind");
        let port = port_of(&server);
        let resp = get(port, "/callback?state=S").await;
        let (status, body) = status_and_body(&resp);
        assert_eq!(status, 500);
        assert!(body.contains("missing authorization code"), "body: {body}");
        match server.next_result().await.expect("result") {
            CallbackResult::Err(msg) => assert!(msg.contains("missing authorization code"), "msg: {msg}"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn favicon_returns_404_without_consuming_flow() {
        let mut server = CallbackServer::bind(&ephemeral_spec(), "S".to_string())
            .await
            .expect("bind");
        let port = port_of(&server);
        // Browser favicon probe: 404.
        assert_eq!(status_and_body(&get(port, "/favicon.ico").await).0, 404);
        // The flow must still be live — a subsequent valid callback resolves.
        let resp = get(port, "/callback?code=C&state=S").await;
        let (status, _body) = status_and_body(&resp);
        assert_eq!(status, 200);
        match server.next_result().await.expect("result") {
            CallbackResult::Ok { code, state } => {
                assert_eq!(code, "C");
                assert_eq!(state, "S");
            }
            other => panic!("expected Ok after favicon, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn launch_redirects_then_503_after_clear() {
        let server = CallbackServer::bind(&ephemeral_spec(), "S".to_string())
            .await
            .expect("bind");
        server.set_pending_url("https://example.test/authorize?x=1".to_string());
        let port = port_of(&server);
        let resp = get(port, "/launch").await;
        assert!(resp.starts_with("HTTP/1.1 302"), "expected 302, got: {resp}");
        assert!(
            resp.contains("Location: https://example.test/authorize?x=1"),
            "missing Location: {resp}"
        );
        server.clear_pending();
        let resp = get(port, "/launch").await;
        assert!(
            resp.starts_with("HTTP/1.1 503"),
            "expected 503 after clear_pending, got: {resp}"
        );
    }

    #[tokio::test]
    async fn browser_response_is_independent_of_resolution() {
        // Never consume next_result: the browser must still receive its full
        // response, proving exchange/resolution does not gate the browser page.
        let server = CallbackServer::bind(&ephemeral_spec(), "S".to_string())
            .await
            .expect("bind");
        let port = port_of(&server);
        let resp = get(port, "/callback?code=C&state=S").await;
        let (status, body) = status_and_body(&resp);
        assert_eq!(status, 200);
        assert!(body.contains("login complete"));
        // Intentionally drop without resolving.
        drop(server);
    }
}
