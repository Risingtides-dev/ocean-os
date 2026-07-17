//! Deferred Axum extraction seam for scoped Observatory authentication.
//!
//! Task 4 constructs and mounts the typed auth state at daemon startup. Task 5
//! will consume this extractor from read-only Observatory data routes; no data
//! route or credential-issuance endpoint is introduced here.

use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts, StatusCode};
use ocean_observatory::{
    verify_token, write_summary_observer_token, ObserverPrincipal, ObserverSecret,
};
use std::path::{Path, PathBuf};

const OBSERVER_COOKIE_NAME: &str = "Authorization-Observer";

/// Dedicated request-extension state for Observatory authentication.
#[derive(Clone)]
pub(super) struct ObservatoryAuthState {
    secret: ObserverSecret,
    daemon_instance_id: String,
    ocean_dir: PathBuf,
}

impl ObservatoryAuthState {
    /// Construct real startup auth state without adding Task 5 data routes.
    pub(super) fn load(
        ocean_dir: &Path,
        daemon_instance_id: impl Into<String>,
    ) -> Result<Self, ocean_observatory::AuthError> {
        let daemon_instance_id = daemon_instance_id.into();
        let secret = ObserverSecret::load_or_generate(ocean_dir)?;
        write_summary_observer_token(ocean_dir, &daemon_instance_id, &secret)?;
        Ok(Self {
            secret,
            daemon_instance_id,
            ocean_dir: ocean_dir.to_path_buf(),
        })
    }

    /// Rotate the boot-bound summary credential consumed by first-party local
    /// proxies. Previously issued tokens remain valid only until their normal
    /// short expiry and never survive a daemon restart.
    pub(super) fn refresh_summary_token(&self) -> Result<(), ocean_observatory::AuthError> {
        write_summary_observer_token(&self.ocean_dir, &self.daemon_instance_id, &self.secret)
            .map(|_| ())
    }

    #[cfg(test)]
    pub(super) fn for_test(secret: ObserverSecret, daemon_instance_id: impl Into<String>) -> Self {
        Self {
            secret,
            daemon_instance_id: daemon_instance_id.into(),
            ocean_dir: PathBuf::new(),
        }
    }
}

/// Verified, typed observer identity for future read-only Observatory routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObservatoryAuth(pub ObserverPrincipal);

impl<S> FromRequestParts<S> for ObservatoryAuth
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let auth_state = parts
            .extensions
            .get::<ObservatoryAuthState>()
            .ok_or(StatusCode::UNAUTHORIZED)?;
        let token = request_token(parts).ok_or(StatusCode::UNAUTHORIZED)?;

        verify_token(token, &auth_state.secret, &auth_state.daemon_instance_id)
            .map(Self)
            .map_err(|_| StatusCode::UNAUTHORIZED)
    }
}

fn request_token(parts: &Parts) -> Option<&str> {
    if let Some(authorization) = parts.headers.get(header::AUTHORIZATION) {
        return authorization
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")
            .filter(|token| !token.is_empty());
    }

    parts
        .headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|cookie| {
            let (name, value) = cookie.split_once('=')?;
            (name == OBSERVER_COOKIE_NAME && !value.is_empty()).then_some(value)
        })
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use ocean_observatory::{sign_token, ObserverScope, ObserverToken};

    use super::*;

    const DAEMON_ID: &str = "test-daemon-1";

    fn auth_state() -> ObservatoryAuthState {
        ObservatoryAuthState::for_test(ObserverSecret::from_raw_key([0x42; 32]), DAEMON_ID)
    }

    fn token(scope: ObserverScope) -> String {
        let claims = ObserverToken::issue(scope, DAEMON_ID, 3_600).expect("issue token");
        sign_token(&claims, &ObserverSecret::from_raw_key([0x42; 32]))
    }

    async fn extract(request: Request<axum::body::Body>) -> Result<ObservatoryAuth, StatusCode> {
        let (mut parts, _) = request.into_parts();
        parts.extensions.insert(auth_state());
        ObservatoryAuth::from_request_parts(&mut parts, &()).await
    }

    #[tokio::test]
    async fn observatory_auth_extracts_bearer_header_for_all_scopes() {
        for scope in [
            ObserverScope::Summary,
            ObserverScope::Content,
            ObserverScope::ExtensionProducer("producer-a".to_owned()),
        ] {
            let request = Request::builder()
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", token(scope.clone())),
                )
                .body(axum::body::Body::empty())
                .expect("request");
            let auth = extract(request).await.expect("authenticated");
            assert_eq!(auth.0.scope, scope);
        }
    }

    #[tokio::test]
    async fn observatory_auth_extracts_authorization_observer_cookie() {
        let expected = token(ObserverScope::Summary);
        let request = Request::builder()
            .header(
                header::COOKIE,
                format!("other=value; {OBSERVER_COOKIE_NAME}={expected}"),
            )
            .body(axum::body::Body::empty())
            .expect("request");

        let auth = extract(request).await.expect("authenticated cookie");
        assert_eq!(auth.0.scope, ObserverScope::Summary);
    }

    #[tokio::test]
    async fn observatory_auth_rejects_every_credential_failure_as_401() {
        let malformed_cookie = Request::builder()
            .header(
                header::COOKIE,
                format!("{OBSERVER_COOKIE_NAME}=not-a-token"),
            )
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(
            extract(malformed_cookie).await,
            Err(StatusCode::UNAUTHORIZED)
        );

        let malformed_header = Request::builder()
            .header(header::AUTHORIZATION, "Basic nope")
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(
            extract(malformed_header).await,
            Err(StatusCode::UNAUTHORIZED)
        );

        let missing = Request::builder()
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(extract(missing).await, Err(StatusCode::UNAUTHORIZED));

        let wrong_instance_claims =
            ObserverToken::issue(ObserverScope::Summary, "other-daemon", 3_600)
                .expect("issue token");
        let wrong_instance = sign_token(
            &wrong_instance_claims,
            &ObserverSecret::from_raw_key([0x42; 32]),
        );
        let request = Request::builder()
            .header(header::AUTHORIZATION, format!("Bearer {wrong_instance}"))
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(extract(request).await, Err(StatusCode::UNAUTHORIZED));
    }

    #[tokio::test]
    async fn observatory_auth_never_reads_query_string_credentials() {
        let request = Request::builder()
            .uri(format!(
                "/v1/observatory/events?token={}",
                token(ObserverScope::Summary)
            ))
            .body(axum::body::Body::empty())
            .expect("request");
        assert_eq!(extract(request).await, Err(StatusCode::UNAUTHORIZED));
    }

    #[test]
    fn observatory_auth_state_loads_persistent_secret_at_startup() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first = ObservatoryAuthState::load(directory.path(), "daemon-one").expect("first");
        let second = ObservatoryAuthState::load(directory.path(), "daemon-two").expect("second");

        assert_eq!(first.secret.key(), second.secret.key());
        assert_eq!(first.daemon_instance_id, "daemon-one");
        assert_eq!(second.daemon_instance_id, "daemon-two");
    }
}
