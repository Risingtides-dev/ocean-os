//! Deferred Axum extraction seam for scoped Observatory authentication.
//!
//! Task 4 constructs and mounts the typed auth state at daemon startup. Task 5
//! will consume this extractor from read-only Observatory data routes; no data
//! route or credential-issuance endpoint is introduced here.

use axum::extract::FromRequestParts;
use axum::http::{header, request::Parts, StatusCode};
use axum::response::{IntoResponse, Response};
use ocean_observatory::{
    verify_token, write_summary_observer_token, ObserverPrincipal, ObserverScope, ObserverSecret,
};
use std::path::{Path, PathBuf};

const OBSERVER_COOKIE_NAME: &str = "Authorization-Observer";
/// How often the daemon rotates the published summary token (`main.rs`).
pub(super) const ROTATION_INTERVAL_SECS: u64 = 10 * 60;

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

    /// One scheduled rotation (F11). A failure is counted on
    /// `ocean_observatory_token_rotation_failures_total` and logged at `error`,
    /// naming how long the last published token has left: it was minted for
    /// 30 minutes and rotation runs every 10, so the third consecutive failure
    /// means local observers are already getting 401s. Returns the new
    /// consecutive-failure count (0 after a success).
    pub(super) fn rotate_summary_token(
        &self,
        metrics: &crate::metrics::TurnMetrics,
        consecutive_failures: u64,
    ) -> u64 {
        match self.refresh_summary_token() {
            Ok(()) => {
                if consecutive_failures > 0 {
                    tracing::warn!(
                        consecutive_failures,
                        "observatory summary token rotation recovered"
                    );
                }
                0
            }
            Err(error) => {
                let consecutive = consecutive_failures + 1;
                let total = metrics.record_observer_token_rotation_failure();
                // Multiply rather than floor-divide: with a lifetime that is not
                // a multiple of the interval, division flags one failure early.
                let published_token_expired = consecutive * ROTATION_INTERVAL_SECS
                    >= ocean_observatory::DEFAULT_TOKEN_LIFETIME_SECS;
                tracing::error!(
                    %error,
                    consecutive_failures = consecutive,
                    rotation_failures_total = total,
                    published_token_expired,
                    "observatory summary token rotation failed; the published observer token is not being renewed"
                );
                consecutive
            }
        }
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
    type Rejection = ObservatoryUnauthorized;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let rejection = || ObservatoryUnauthorized::for_request(parts);
        let auth_state = parts
            .extensions
            .get::<ObservatoryAuthState>()
            .ok_or_else(rejection)?;
        let token = request_token(parts).ok_or_else(rejection)?;

        let principal = verify_token(token, &auth_state.secret, &auth_state.daemon_instance_id)
            .map_err(|_| rejection())?;
        // F3: every V1 Observatory route serves `observatory:summary` only
        // (manifest §3.3; content and extension scopes are future). A valid
        // token of any other scope is refused here, with the same 401 as any
        // credential failure (the manifest defines no 403), so no route can
        // serve a wider principal once a second mint path exists.
        if principal.scope != ObserverScope::Summary {
            return Err(rejection());
        }
        Ok(Self(principal))
    }
}

/// Every Observatory credential failure (G5): a 401 carrying the §7.4 headers
/// every Observatory response carries and the §7.1 `{error, message,
/// http_status}` body — not a bare status. One fixed message whatever failed,
/// so a caller learns nothing about which check refused it.
#[derive(Debug)]
pub(super) struct ObservatoryUnauthorized {
    cursor: ocean_observatory::Cursor,
    instance: String,
}

impl ObservatoryUnauthorized {
    fn for_request(parts: &Parts) -> Self {
        let (cursor, instance) = match parts
            .extensions
            .get::<crate::observatory::ObservatoryServices>()
        {
            Some(services) => services.header_facts(),
            None => (
                ocean_observatory::Cursor::new(0),
                parts
                    .extensions
                    .get::<ObservatoryAuthState>()
                    .map(|state| state.daemon_instance_id.clone())
                    .unwrap_or_else(|| "unknown".to_owned()),
            ),
        };
        Self { cursor, instance }
    }

    #[cfg(test)]
    pub(super) fn status(&self) -> StatusCode {
        StatusCode::UNAUTHORIZED
    }
}

impl IntoResponse for ObservatoryUnauthorized {
    fn into_response(self) -> Response {
        crate::observatory::error_response(
            StatusCode::UNAUTHORIZED,
            crate::observatory::observatory_headers(self.cursor, &self.instance),
            "unauthorized",
            "Missing or invalid observer token",
        )
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
    use ocean_observatory::{sign_token, ObserverToken};

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
        ObservatoryAuth::from_request_parts(&mut parts, &())
            .await
            .map_err(|rejection| rejection.status())
    }

    fn bearer(scope: ObserverScope) -> Request<axum::body::Body> {
        Request::builder()
            .header(header::AUTHORIZATION, format!("Bearer {}", token(scope)))
            .body(axum::body::Body::empty())
            .expect("request")
    }

    #[tokio::test]
    async fn observatory_auth_extracts_summary_bearer_header() {
        let auth = extract(bearer(ObserverScope::Summary))
            .await
            .expect("authenticated");
        assert_eq!(auth.0.scope, ObserverScope::Summary);
    }

    /// F3: a correctly signed, unexpired token for this daemon is still
    /// refused when its scope is not `observatory:summary`.
    #[tokio::test]
    async fn observatory_auth_rejects_non_summary_scopes_as_401() {
        for scope in [
            ObserverScope::Content,
            ObserverScope::ExtensionProducer("producer-a".to_owned()),
        ] {
            assert_eq!(
                extract(bearer(scope.clone())).await,
                Err(StatusCode::UNAUTHORIZED),
                "{scope}"
            );
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

    /// F11: a failed rotation is counted on the `/metrics` surface, not only
    /// logged, and the consecutive count resets once a rotation succeeds.
    #[test]
    fn failed_token_rotation_is_counted_on_metrics() {
        let directory = tempfile::tempdir().expect("tempdir");
        let metrics = crate::metrics::TurnMetrics::default();
        let state = ObservatoryAuthState::load(directory.path(), DAEMON_ID).expect("load");

        // Make the ocean dir unusable: a regular file where the directory was.
        let blocked = ObservatoryAuthState {
            ocean_dir: directory.path().join("observatory-secret"),
            ..state.clone()
        };
        assert_eq!(blocked.rotate_summary_token(&metrics, 0), 1);
        assert_eq!(blocked.rotate_summary_token(&metrics, 1), 2);
        let rendered = metrics.render_prometheus(0, 0, 0, 0);
        assert_eq!(
            crate::metrics::metric_value(
                &rendered,
                "ocean_observatory_token_rotation_failures_total"
            ),
            Some(2),
            "{rendered}"
        );

        assert_eq!(state.rotate_summary_token(&metrics, 2), 0);
        let rendered = metrics.render_prometheus(0, 0, 0, 0);
        assert_eq!(
            crate::metrics::metric_value(
                &rendered,
                "ocean_observatory_token_rotation_failures_total"
            ),
            Some(2),
            "a success does not reset the lifetime counter"
        );
    }
}
