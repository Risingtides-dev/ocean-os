//! End-to-end OAuth flows through the public API.
//!
//! These tests drive `begin` / `finish` against a real bound callback server
//! and (for the happy path) a local mock token endpoint selected via the
//! `OCEAN_OAUTH_ANTHROPIC_TOKEN_URL` override. No real provider endpoints are
//! contacted and the real `~/.config/ocean-rs/auth.json` is never touched —
//! every flow targets a unique throwaway file under the system temp dir.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use parking_lot::Mutex;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use ocean_oauth::{begin, OAuthProvider};

// ---------------------------------------------------------------------------
// Minimal query helpers (the crate's `util` is pub(crate), so we reimplement
// the tiny slice we need here).
// ---------------------------------------------------------------------------

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn query_get(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let (raw_key, raw_value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if decode(raw_key) == key {
            let value = decode(raw_value);
            return if value.is_empty() { None } else { Some(value) };
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Test plumbing.
// ---------------------------------------------------------------------------

static UNIQUE: AtomicU64 = AtomicU64::new(0);

fn unique_suffix() -> u64 {
    UNIQUE.fetch_add(1, Ordering::SeqCst)
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Fresh, unique auth-file path under the temp dir. Never the real auth file.
fn fresh_auth_path(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "ocean-oauth-{label}-{}-{}",
        std::process::id(),
        unique_suffix()
    ));
    let auth = dir.join("auth.json");
    (dir, auth)
}

/// `http://localhost:{port}/launch` -> bound callback port.
fn port_of_launch(launch_url: &str) -> u16 {
    let rest = launch_url
        .strip_prefix("http://localhost:")
        .expect("launch_url is localhost");
    rest.split('/')
        .next()
        .expect("port segment")
        .parse()
        .expect("numeric port")
}

async fn raw_get(host_port: &str, target: &str) -> String {
    let mut stream = TcpStream::connect(host_port).await.expect("connect");
    let req = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("write");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    String::from_utf8_lossy(&buf).into_owned()
}

fn body_of(resp: &str) -> &str {
    resp.split("\r\n\r\n").nth(1).unwrap_or("")
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|l| {
            l.to_lowercase()
                .strip_prefix("content-length:")?
                .trim()
                .parse::<usize>()
                .ok()
        })
        .unwrap_or(0)
}

/// Bind a mock token endpoint on an ephemeral port. Accepts exactly one POST,
/// captures the raw request body for assertion, and replies once with
/// `reply_body` (a JSON string). Returns the URL to point the env override at
/// and a shared cell holding the captured POST body.
async fn spawn_mock_token(reply_body: String) -> (String, std::sync::Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let port = listener.local_addr().expect("addr").port();
    let captured = std::sync::Arc::new(Mutex::new(None::<String>));
    let cap = captured.clone();
    tokio::spawn(async move {
        let (mut stream, _) = match listener.accept().await {
            Ok(p) => p,
            Err(_) => return,
        };
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 4096];
        // Read the full request: headers + the Content-Length body.
        loop {
            match stream.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(_) => return,
            }
            if let Some(hdr_end) = find_header_end(&buf) {
                let cl = content_length(&buf[..hdr_end]);
                let have = buf.len().saturating_sub(hdr_end + 4);
                if have >= cl {
                    break;
                }
            }
        }
        let hdr_end = find_header_end(&buf).unwrap_or(buf.len());
        let body_start = (hdr_end + 4).min(buf.len());
        *cap.lock() = Some(String::from_utf8_lossy(&buf[body_start..]).into_owned());

        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            reply_body.len(),
            reply_body
        );
        let _ = stream.write_all(resp.as_bytes()).await;
        let _ = stream.flush().await;
    });
    (format!("http://127.0.0.1:{port}/token"), captured)
}

// ---------------------------------------------------------------------------
// #8 The money test: Claude full flow.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_full_flow_end_to_end() {
    let (auth_dir, auth_path) = fresh_auth_path("fullflow");

    // Mock token endpoint with the Anthropic response shape (incl. account).
    let reply = json!({
        "access_token": "at_FULLFLOW",
        "refresh_token": "rt_FULLFLOW",
        "expires_in": 3600,
        "account": { "uuid": "acct-fullflow" }
    })
    .to_string();
    let (token_url, captured) = spawn_mock_token(reply).await;

    // The override is read once in begin(); set it before starting the flow.
    // This is the only env-mutating test for this variable -> no cross-test race.
    std::env::set_var("OCEAN_OAUTH_ANTHROPIC_TOKEN_URL", &token_url);

    let session = begin(OAuthProvider::Claude, Some(auth_path.clone()))
        .await
        .expect("begin");

    // Pull the echoed state + challenge out of the authorize URL, and the real
    // bound port out of the launch URL (Claude may fall back from 54545 to an
    // ephemeral port — never assert the port number).
    let authorize_query = session
        .authorize_url
        .split_once('?')
        .expect("authorize url has a query")
        .1;
    let state = query_get(authorize_query, "state").expect("state param");
    let code_challenge = query_get(authorize_query, "code_challenge").expect("challenge param");
    let port = port_of_launch(&session.launch_url);

    // finish() blocks on the callback, then exchanges; run it concurrently.
    let finish_task = tokio::spawn(async move { session.finish().await });

    // Browser-side redirect: deliver code + the matching state to the real
    // callback the server is listening on.
    let resp = raw_get(
        &format!("127.0.0.1:{port}"),
        &format!("/callback?code=AUTHCODE_FF&state={state}"),
    )
    .await;
    assert!(
        resp.starts_with("HTTP/1.1 200"),
        "browser should get 200: {resp}"
    );
    assert!(
        body_of(&resp).contains("login complete"),
        "browser body: {}",
        body_of(&resp)
    );

    let outcome = finish_task.await.expect("join").expect("finish ok");
    assert_eq!(outcome.provider, OAuthProvider::Claude);
    assert_eq!(outcome.account_id.as_deref(), Some("acct-fullflow"));
    let before = now_millis();
    assert!(
        outcome.expires_ms > before,
        "expires {} must be in the future (> now {})",
        outcome.expires_ms,
        before
    );

    // Auth file holds the exact claude-code block.
    let written = std::fs::read_to_string(&auth_path).expect("auth file written");
    let v: Value = serde_json::from_str(&written).expect("auth file is JSON");
    let block = &v["claude-code"];
    assert_eq!(block["type"], "oauth", "block: {block}");
    assert_eq!(block["access"], "at_FULLFLOW");
    assert_eq!(block["refresh"], "rt_FULLFLOW");
    assert_eq!(block["accountId"], "acct-fullflow");
    assert_eq!(block["expires"], json!(outcome.expires_ms));

    // The mock token endpoint received a well-formed authorization_code grant.
    let posted = captured
        .lock()
        .clone()
        .expect("mock captured the POST body");
    let posted: Value = serde_json::from_str(&posted).expect("posted body is JSON: {posted}");
    assert_eq!(posted["grant_type"], "authorization_code");
    assert_eq!(posted["client_id"], "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
    assert_eq!(posted["code"], "AUTHCODE_FF");
    assert_eq!(posted["state"], state);
    assert_eq!(
        posted["redirect_uri"],
        format!("http://localhost:{port}/callback")
    );

    // PKCE linkage: the posted code_verifier's S256 must equal the challenge
    // that was advertised in the authorize URL — i.e. the right verifier.
    let verifier = posted["code_verifier"].as_str().expect("verifier present");
    assert!(!verifier.is_empty(), "verifier must not be empty");
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    assert_eq!(
        URL_SAFE_NO_PAD.encode(hasher.finalize()),
        code_challenge,
        "posted verifier's S256 must match the authorize-url challenge"
    );

    std::env::remove_var("OCEAN_OAUTH_ANTHROPIC_TOKEN_URL");
    let _ = std::fs::remove_dir_all(&auth_dir);
}

// ---------------------------------------------------------------------------
// #9 Negative: a provider-style error callback resolves finish() to an
// "authorization failed" error. (We do NOT wait out the 300s timeout.)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn callback_error_resolves_finish_to_authorization_failed() {
    let (auth_dir, auth_path) = fresh_auth_path("neg");

    // No token endpoint is needed: finish() bails at the callback, before any
    // exchange — so this test performs zero network egress.
    let session = begin(OAuthProvider::Claude, Some(auth_path.clone()))
        .await
        .expect("begin");
    let port = port_of_launch(&session.launch_url);
    let authorize_query = session
        .authorize_url
        .split_once('?')
        .expect("authorize url has a query")
        .1;
    let state = query_get(authorize_query, "state").expect("state param");

    let finish_task = tokio::spawn(async move { session.finish().await });

    let resp = raw_get(
        &format!("127.0.0.1:{port}"),
        &format!("/callback?error=access_denied&error_description=user%20canceled&state={state}"),
    )
    .await;
    assert!(
        resp.starts_with("HTTP/1.1 500"),
        "browser should get 500 on error: {resp}"
    );
    assert!(
        body_of(&resp).contains("authorization failed"),
        "browser body: {}",
        body_of(&resp)
    );

    let err = finish_task.await.expect("join").unwrap_err();
    assert!(
        err.to_string().contains("authorization failed"),
        "finish error: {err}"
    );

    // No credential must be written on failure.
    assert!(
        !auth_path.exists(),
        "auth file must not be written when the flow fails"
    );

    let _ = std::fs::remove_dir_all(&auth_dir);
}
