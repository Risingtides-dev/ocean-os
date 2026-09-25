//! Coding-plan logins over HTTP — web identity program M3.
//!
//! See `docs/specs/2026-09-25-ocean-web-identity-and-node-linking-program.md`.
//! The TUI's `/login` was the only way to sign this node into a Claude or
//! Codex subscription; these routes let any operator-authenticated surface do
//! the same, so a coworker can manage their node's plans from the web.
//!
//! | Route | Answers |
//! |---|---|
//! | `GET /v1/auth/providers` | status of every OAuth provider, never a token |
//! | `POST /v1/auth/providers/{provider}/login` | starts a login: `attempt_id`, `authorize_url` |
//! | `GET /v1/auth/providers/{provider}/login/{attempt_id}` | `pending` / `succeeded` / `failed` / `cancelled` |
//! | `DELETE /v1/auth/providers/{provider}/login/{attempt_id}` | cancels a pending attempt |
//! | `POST /v1/auth/providers/{provider}/logout` | removes the provider's auth-file block |
//!
//! Every route is operator-authenticated through the same fail-closed
//! principal as room authority mutations ([`crate::room_operator`]): a Cookie
//! header or foreign Origin is refused on shape, a missing key file is 503.
//! Status is gated too — whether a node holds a subscription is the owner's
//! business.
//!
//! The browser flow itself is unchanged from the TUI: `ocean-oauth` binds the
//! provider's localhost callback on THIS machine, so one-click completion
//! works when the browser that opens `authorize_url` runs on the same machine
//! as the daemon. A browser elsewhere reaches GitHub-style consent but its
//! redirect to `localhost` lands on the wrong computer; the attempt then times
//! out (300s) as `failed`. The remote code-paste fallback is an open question
//! in the program spec, not something this module pretends to do.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use ocean_oauth::OAuthProvider;
use ocean_providers::{CredentialSource, ProviderId};
use serde_json::{json, Value};

use crate::room_agent_authority::ApiError;
use crate::room_operator::OperatorIdentity;
use crate::AppState;

type RouteResult = Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum AttemptState {
    Pending,
    Succeeded,
    Failed(String),
    Cancelled,
}

impl AttemptState {
    fn wire(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed(_) => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

struct Attempt {
    id: String,
    state: AttemptState,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// The daemon's in-memory record of provider login attempts: at most one per
/// provider, the latest. Restarting the daemon forgets them, which is correct:
/// the callback server they were waiting on died with it.
#[derive(Default)]
pub(crate) struct ProviderLogins {
    /// `None` resolves the auth file per request exactly as `ocean-oauth` does
    /// (`OCEAN_AUTH_FILE`, then the default config path). Tests pin a temp file.
    auth_file: Option<PathBuf>,
    attempts: Mutex<HashMap<&'static str, Attempt>>,
}

impl ProviderLogins {
    #[cfg(test)]
    pub(crate) fn with_auth_file(auth_file: PathBuf) -> Self {
        Self {
            auth_file: Some(auth_file),
            attempts: Mutex::default(),
        }
    }

    fn attempts(&self) -> std::sync::MutexGuard<'_, HashMap<&'static str, Attempt>> {
        // A poisoned map only means a panic mid-update of plain data; the
        // entries are still meaningful, so recover rather than wedge logins.
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn login_projection(&self, provider: OAuthProvider) -> Value {
        match self.attempts().get(provider.label()) {
            Some(attempt) => attempt_json(provider, attempt),
            None => Value::Null,
        }
    }

    /// Cancel whatever attempt is pending for `provider`. Returns the aborted
    /// task so a caller about to bind the same callback port can wait for the
    /// old listener to actually close.
    fn cancel_pending(&self, provider: OAuthProvider) -> Option<tokio::task::JoinHandle<()>> {
        let mut attempts = self.attempts();
        let attempt = attempts.get_mut(provider.label())?;
        if attempt.state != AttemptState::Pending {
            return None;
        }
        attempt.state = AttemptState::Cancelled;
        let task = attempt.task.take()?;
        task.abort();
        Some(task)
    }
}

fn attempt_json(provider: OAuthProvider, attempt: &Attempt) -> Value {
    let mut body = json!({
        "provider": provider.label(),
        "attempt_id": attempt.id,
        "state": attempt.state.wire(),
    });
    if let AttemptState::Failed(reason) = &attempt.state {
        body["error"] = json!(reason);
    }
    body
}

fn mint_attempt_id() -> String {
    let mut bytes = [0_u8; 16];
    // A non-random id would only let one attempt's poll read another's state;
    // it authorizes nothing, so a zeroed fallback is not a security failure.
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn provider_from_path(raw: &str) -> Result<OAuthProvider, ApiError> {
    OAuthProvider::from_label(raw).ok_or(ApiError::not_found("unknown_provider"))
}

fn provider_id(provider: OAuthProvider) -> ProviderId {
    match provider {
        OAuthProvider::Claude => ProviderId::ClaudeCode,
        OAuthProvider::Codex => ProviderId::OpenAiCodex,
    }
}

fn display_label(provider: OAuthProvider) -> &'static str {
    match provider {
        OAuthProvider::Claude => "Claude (Pro/Max plan)",
        OAuthProvider::Codex => "Codex (ChatGPT plan)",
    }
}

/// Status of one provider, token-free. The Ocean auth-file block is the
/// primary answer; when it is absent the provider resolver's source label says
/// whether an env credential or the Codex CLI's own login covers it.
fn provider_status(logins: &ProviderLogins, provider: OAuthProvider, now_ms: i64) -> Value {
    let block = ocean_oauth::oauth_block_status(provider, logins.auth_file.clone());
    let (status, source, expires_ms) = match block {
        Ok(block) if block.present => {
            let expired = block.expires_ms.is_some_and(|ms| ms <= now_ms);
            let status = if expired && !block.refreshable {
                "expired"
            } else {
                "signed_in"
            };
            (status, Some("auth_file"), block.expires_ms)
        }
        Ok(_) => match fallback_source(logins, provider) {
            Some(source) => ("signed_in", Some(source), None),
            None => ("signed_out", None, None),
        },
        Err(error) => {
            tracing::warn!(provider = provider.label(), %error, "auth file unreadable");
            ("unknown", None, None)
        }
    };
    json!({
        "provider": provider.label(),
        "label": display_label(provider),
        "kind": "oauth",
        "status": status,
        "source": source,
        "expires_at_ms": expires_ms,
        "login": logins.login_projection(provider),
    })
}

/// Non-auth-file credentials the runtime would actually use. Only consulted
/// when the daemon resolves the auth file itself: a pinned test file must not
/// pick up the developer's real Codex CLI login.
fn fallback_source(logins: &ProviderLogins, provider: OAuthProvider) -> Option<&'static str> {
    if logins.auth_file.is_some() {
        return None;
    }
    match ocean_providers::resolve_credential_from_env(&provider_id(provider)) {
        Ok(Some(credential)) => match credential.source {
            CredentialSource::Env { .. } => Some("env"),
            CredentialSource::CodexCliAuthFile { .. } => Some("codex_cli"),
            CredentialSource::OceanAuthFile { .. } => Some("auth_file"),
            CredentialSource::NotRequired => None,
        },
        _ => None,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

fn authorize(operator: &OperatorIdentity, headers: &HeaderMap) -> Result<(), ApiError> {
    operator
        .authorize(headers)
        .map(|_| ())
        .map_err(ApiError::from)
}

pub(crate) fn list_inner(
    operator: &OperatorIdentity,
    logins: &ProviderLogins,
    headers: &HeaderMap,
) -> RouteResult {
    authorize(operator, headers).map_err(ApiError::response)?;
    let now = now_ms();
    let providers: Vec<Value> = OAuthProvider::ALL
        .into_iter()
        .map(|provider| provider_status(logins, provider, now))
        .collect();
    Ok((
        StatusCode::OK,
        Json(json!({"ok": true, "providers": providers})),
    ))
}

pub(crate) async fn start_inner(
    operator: &OperatorIdentity,
    logins: &Arc<ProviderLogins>,
    headers: &HeaderMap,
    raw_provider: &str,
) -> RouteResult {
    authorize(operator, headers).map_err(ApiError::response)?;
    let provider = provider_from_path(raw_provider).map_err(ApiError::response)?;

    // A new login replaces a pending one. Abort it and wait for the task to
    // unwind, which drops its callback listener: Codex's port is fixed at
    // 1455, so binding before the old one closes would fail.
    if let Some(previous) = logins.cancel_pending(provider) {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), previous).await;
    }
    let session = match ocean_oauth::begin(provider, logins.auth_file.clone()).await {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(provider = provider.label(), %error, "provider login could not start");
            return Err(ApiError::conflict("login_unavailable").response());
        }
    };
    let authorize_url = session.authorize_url.clone();
    let id = mint_attempt_id();

    // Register the attempt BEFORE the task can finish, so a fast completion
    // always finds its row.
    logins.attempts().insert(
        provider.label(),
        Attempt {
            id: id.clone(),
            state: AttemptState::Pending,
            task: None,
        },
    );
    let task_logins = Arc::clone(logins);
    let task_id = id.clone();
    let task = tokio::spawn(async move {
        let outcome = session.finish().await;
        let mut attempts = task_logins.attempts();
        let Some(attempt) = attempts.get_mut(provider.label()) else {
            return;
        };
        // A newer attempt owns the row now; this result is stale.
        if attempt.id != task_id || attempt.state != AttemptState::Pending {
            return;
        }
        attempt.task = None;
        attempt.state = match outcome {
            Ok(_) => {
                tracing::info!(provider = provider.label(), "provider login completed");
                AttemptState::Succeeded
            }
            Err(error) => {
                tracing::warn!(provider = provider.label(), %error, "provider login failed");
                AttemptState::Failed(error.to_string())
            }
        };
    });
    if let Some(attempt) = logins.attempts().get_mut(provider.label()) {
        if attempt.id == id && attempt.state == AttemptState::Pending {
            attempt.task = Some(task);
        }
    }

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "ok": true,
            "provider": provider.label(),
            "attempt_id": id,
            "state": "pending",
            "authorize_url": authorize_url,
            "same_machine_required": true,
        })),
    ))
}

fn attempt_for(
    logins: &ProviderLogins,
    provider: OAuthProvider,
    attempt_id: &str,
) -> Result<Value, ApiError> {
    let attempts = logins.attempts();
    let attempt = attempts
        .get(provider.label())
        .filter(|attempt| attempt.id == attempt_id)
        .ok_or(ApiError::not_found("unknown_attempt"))?;
    Ok(attempt_json(provider, attempt))
}

pub(crate) fn poll_inner(
    operator: &OperatorIdentity,
    logins: &ProviderLogins,
    headers: &HeaderMap,
    raw_provider: &str,
    attempt_id: &str,
) -> RouteResult {
    authorize(operator, headers).map_err(ApiError::response)?;
    let provider = provider_from_path(raw_provider).map_err(ApiError::response)?;
    let mut body = attempt_for(logins, provider, attempt_id).map_err(ApiError::response)?;
    body["ok"] = json!(true);
    Ok((StatusCode::OK, Json(body)))
}

pub(crate) fn cancel_inner(
    operator: &OperatorIdentity,
    logins: &ProviderLogins,
    headers: &HeaderMap,
    raw_provider: &str,
    attempt_id: &str,
) -> RouteResult {
    authorize(operator, headers).map_err(ApiError::response)?;
    let provider = provider_from_path(raw_provider).map_err(ApiError::response)?;
    attempt_for(logins, provider, attempt_id).map_err(ApiError::response)?;
    logins.cancel_pending(provider);
    let mut body = attempt_for(logins, provider, attempt_id).map_err(ApiError::response)?;
    body["ok"] = json!(true);
    Ok((StatusCode::OK, Json(body)))
}

pub(crate) fn logout_inner(
    operator: &OperatorIdentity,
    logins: &ProviderLogins,
    headers: &HeaderMap,
    raw_provider: &str,
) -> RouteResult {
    authorize(operator, headers).map_err(ApiError::response)?;
    let provider = provider_from_path(raw_provider).map_err(ApiError::response)?;
    logins.cancel_pending(provider);
    let removed = ocean_oauth::logout(provider, logins.auth_file.clone()).map_err(|error| {
        tracing::warn!(provider = provider.label(), %error, "provider logout failed");
        ApiError::internal("logout_failed").response()
    })?;
    Ok((
        StatusCode::OK,
        Json(json!({"ok": true, "provider": provider.label(), "removed": removed})),
    ))
}

fn flatten(result: RouteResult) -> (StatusCode, Json<Value>) {
    result.unwrap_or_else(|error| error)
}

/// `GET /v1/auth/providers`.
pub(crate) async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    flatten(list_inner(
        &state.room_operator,
        &state.provider_logins,
        &headers,
    ))
}

/// `POST /v1/auth/providers/{provider}/login`.
pub(crate) async fn start(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    flatten(
        start_inner(
            &state.room_operator,
            &state.provider_logins,
            &headers,
            &provider,
        )
        .await,
    )
}

/// `GET /v1/auth/providers/{provider}/login/{attempt_id}`.
pub(crate) async fn poll(
    State(state): State<AppState>,
    Path((provider, attempt_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    flatten(poll_inner(
        &state.room_operator,
        &state.provider_logins,
        &headers,
        &provider,
        &attempt_id,
    ))
}

/// `DELETE /v1/auth/providers/{provider}/login/{attempt_id}`.
pub(crate) async fn cancel(
    State(state): State<AppState>,
    Path((provider, attempt_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    flatten(cancel_inner(
        &state.room_operator,
        &state.provider_logins,
        &headers,
        &provider,
        &attempt_id,
    ))
}

/// `POST /v1/auth/providers/{provider}/logout`.
pub(crate) async fn logout(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    flatten(logout_inner(
        &state.room_operator,
        &state.provider_logins,
        &headers,
        &provider,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const KEY: &str = "test-operator-key";

    fn operator() -> OperatorIdentity {
        OperatorIdentity::for_test(Some(KEY), vec!["http://127.0.0.1:8790".into()])
    }

    fn authed() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            crate::room_operator::OPERATOR_HEADER,
            HeaderValue::from_static(KEY),
        );
        headers
    }

    fn logins(body: Option<&str>) -> (tempfile::TempDir, Arc<ProviderLogins>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        if let Some(body) = body {
            std::fs::write(&path, body).unwrap();
        }
        (dir, Arc::new(ProviderLogins::with_auth_file(path)))
    }

    fn body(result: RouteResult) -> (StatusCode, Value) {
        let (status, Json(value)) = flatten(result);
        (status, value)
    }

    #[test]
    fn every_route_fails_closed_without_the_operator_key() {
        let (_dir, logins) = logins(None);
        let operator = operator();

        let (status, value) = body(list_inner(&operator, &logins, &HeaderMap::new()));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(value["error"], "operator_credential_missing");

        let mut wrong = HeaderMap::new();
        wrong.insert(
            crate::room_operator::OPERATOR_HEADER,
            HeaderValue::from_static("nope"),
        );
        let (status, _) = body(logout_inner(&operator, &logins, &wrong, "claude"));
        assert_eq!(status, StatusCode::FORBIDDEN);

        // A browser's ambient cookie is refused on shape, even with the key.
        let mut cookie = authed();
        cookie.insert(axum::http::header::COOKIE, HeaderValue::from_static("a=b"));
        let (status, value) = body(list_inner(&operator, &logins, &cookie));
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(value["error"], "ambient_credential_rejected");

        let unconfigured = OperatorIdentity::for_test(None, Vec::new());
        let (status, _) = body(list_inner(&unconfigured, &logins, &authed()));
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn status_reports_blocks_without_tokens() {
        let far_future = now_ms() + 3_600_000;
        let (_dir, logins) = logins(Some(&format!(
            r#"{{"claude-code":{{"type":"oauth","access":"secret-access","refresh":"secret-refresh","expires":{far_future}}},
                "openai-codex":{{"type":"oauth","access":"old","expires":1000}}}}"#
        )));
        let (status, value) = body(list_inner(&operator(), &logins, &authed()));
        assert_eq!(status, StatusCode::OK);
        let text = value.to_string();
        assert!(!text.contains("secret-access") && !text.contains("secret-refresh"));
        let providers = value["providers"].as_array().unwrap();
        assert_eq!(providers[0]["provider"], "claude");
        assert_eq!(providers[0]["status"], "signed_in");
        assert_eq!(providers[0]["source"], "auth_file");
        assert_eq!(providers[0]["expires_at_ms"], far_future);
        // Expired with no refresh token: the plan needs a new browser login.
        assert_eq!(providers[1]["provider"], "codex");
        assert_eq!(providers[1]["status"], "expired");
    }

    #[test]
    fn signed_out_when_the_file_is_absent_and_logout_is_idempotent() {
        let (_dir, logins) = logins(Some(r#"{"claude-code":{"type":"oauth","access":"a"}}"#));
        let (_, value) = body(logout_inner(&operator(), &logins, &authed(), "claude"));
        assert_eq!(value["removed"], true);
        let (_, value) = body(logout_inner(&operator(), &logins, &authed(), "claude"));
        assert_eq!(value["removed"], false);
        let (_, value) = body(list_inner(&operator(), &logins, &authed()));
        assert_eq!(value["providers"][0]["status"], "signed_out");
        assert_eq!(value["providers"][0]["login"], Value::Null);
    }

    #[test]
    fn unknown_providers_and_attempts_are_404() {
        let (_dir, logins) = logins(None);
        let (status, value) = body(logout_inner(&operator(), &logins, &authed(), "gemini"));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(value["error"], "unknown_provider");
        let (status, value) = body(poll_inner(
            &operator(),
            &logins,
            &authed(),
            "claude",
            "no-such-attempt",
        ));
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(value["error"], "unknown_attempt");
    }

    #[tokio::test]
    async fn a_claude_login_starts_pending_is_pollable_and_cancels() {
        let (_dir, logins) = logins(None);
        let (status, started) = body(start_inner(&operator(), &logins, &authed(), "claude").await);
        assert_eq!(status, StatusCode::ACCEPTED, "{started}");
        let attempt = started["attempt_id"].as_str().unwrap().to_string();
        assert!(started["authorize_url"]
            .as_str()
            .unwrap()
            .starts_with("https://"));
        assert_eq!(started["same_machine_required"], true);

        let (_, polled) = body(poll_inner(
            &operator(),
            &logins,
            &authed(),
            "claude",
            &attempt,
        ));
        assert_eq!(polled["state"], "pending");
        let (_, listed) = body(list_inner(&operator(), &logins, &authed()));
        assert_eq!(listed["providers"][0]["login"]["attempt_id"], attempt);

        // A second start supersedes the first; the old id stops resolving.
        let (_, restarted) = body(start_inner(&operator(), &logins, &authed(), "claude").await);
        let newer = restarted["attempt_id"].as_str().unwrap().to_string();
        assert_ne!(newer, attempt);
        let (status, _) = body(poll_inner(
            &operator(),
            &logins,
            &authed(),
            "claude",
            &attempt,
        ));
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (_, cancelled) = body(cancel_inner(
            &operator(),
            &logins,
            &authed(),
            "claude",
            &newer,
        ));
        assert_eq!(cancelled["state"], "cancelled");
    }
}
