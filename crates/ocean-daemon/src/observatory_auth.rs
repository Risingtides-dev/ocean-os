//! Axum extractor for verifying scoped observer authentication tokens.
//!
//! This module provides the `ObservatoryAuth` extractor which reads the `Authorization: Bearer`
//! header (or optionally a secure cookie) and validates the token using HMAC-SHA256.
//!
//! The extractor is crate-private to ocean-daemon — it is NOT exported for external use.

use std::future::Future;
use std::pin::Pin;
use axum::extract::FromRequestParts;
use axum::http::{request::Parts, StatusCode};
use ocean_observatory::{verify_token, AuthError, ObserverPrincipal, ObserverSecret};

/// Extractor that verifies an Observer authentication token and extracts the principal.
///
/// Reads the `Authorization: Bearer <token>` header and validates it using the daemon secret.
/// Returns `ObserverPrincipal` on success (200), or `401 Unauthorized` on failure.
///
/// Security:
/// - Uses constant-time HMAC comparison to prevent timing attacks
/// - Validates token expiration
/// - Verifies daemon instance ID to prevent cross-daemon token replay
/// - No support for query-string tokens (header-only)
///
/// # Example
///
/// ```rust,ignore
/// async fn protected_endpoint(
///     ObservatoryAuth(principal): ObservatoryAuth,
/// ) -> impl IntoResponse {
///     format!("Welcome, {}. Scope: {:?}", principal.principal_id, principal.scope)
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservatoryAuth(pub ObserverPrincipal);

impl<S> FromRequestParts<S> for ObservatoryAuth
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    fn from_request_parts<'a>(
        parts: &'a mut Parts,
        _state: &S,
    ) -> Pin<Box<dyn Future<Output = Result<Self, Self::Rejection>> + Send + 'a>> {
        Box::pin(async {
        // Extract Authorization header (Bearer token).
        let auth_header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .ok_or(StatusCode::UNAUTHORIZED)?;

        // Parse "Bearer <token>".
        let token_string = auth_header
            .strip_prefix("Bearer ")
            .ok_or(StatusCode::UNAUTHORIZED)?;

        // Get daemon secret and instance ID from extension state.
        // These are expected to be inserted via `.layer()` during router setup.
        let secret = parts
            .extensions
            .get::<ObserverSecret>()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

        let daemon_instance_id = parts
            .extensions
            .get::<String>()
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;

        // Verify token and extract principal.
        match verify_token(token_string, secret, daemon_instance_id) {
            Ok(principal) => Ok(ObservatoryAuth(principal)),
            Err(AuthError::TokenExpired) => Err(StatusCode::UNAUTHORIZED),
            Err(AuthError::InvalidSignature) => Err(StatusCode::UNAUTHORIZED),
            Err(AuthError::TokenInstanceMismatch) => Err(StatusCode::UNAUTHORIZED),
            Err(AuthError::MalformedToken(_)) => Err(StatusCode::BAD_REQUEST),
            Err(AuthError::SecretIo(_) | AuthError::SecretValidation(_)) => {
                Err(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::FromRequestParts,
        http::{Request, StatusCode},
    };
    use chrono::Utc;
    use ocean_observatory::{sign_token, ObserverScope, ObserverToken};

    fn create_test_secret() -> ObserverSecret {
        // Create a test secret using a fixed key
        let secret_key = vec![0x42; ObserverSecret::MIN_KEY_SIZE];
        ObserverSecret::from_raw_key(secret_key)
    }

    fn create_test_token(principal_id: &str, scope: ObserverScope, daemon_id: &str) -> String {
        let now = Utc::now();
        let token = ObserverToken {
            principal_id: principal_id.to_string(),
            scope,
            daemon_instance_id: daemon_id.to_string(),
            issued_at: now.to_rfc3339(),
            expires_at: (now + chrono::Duration::hours(1)).to_rfc3339(),
        };
        sign_token(&token, &create_test_secret())
    }

    #[tokio::test]
    async fn test_observatory_auth_valid_token() {
        let secret = create_test_secret();
        let daemon_id = "test-daemon-1";
        let token = create_test_token("observer-1", ObserverScope::Summary, daemon_id);

        let mut parts = Request::builder()
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {}", token))
            .body(axum::body::Body::empty())
            .unwrap()
            .into_parts()
            .0;

        // Insert extensions (normally done by router middleware).
        parts.extensions.insert(secret);
        parts.extensions.insert(daemon_id.to_string());

        let result = ObservatoryAuth::from_request_parts(&mut parts, &()).await;
        assert!(result.is_ok(), "Valid token should extract principal");

        let auth = result.unwrap();
        assert_eq!(auth.0.principal_id, "observer-1");
        assert_eq!(auth.0.scope, ObserverScope::Summary);
    }

    #[tokio::test]
    async fn test_observatory_auth_missing_header() {
        let secret = create_test_secret();
        let daemon_id = "test-daemon-1";

        let mut parts = Request::builder()
            .body(axum::body::Body::empty())
            .unwrap()
            .into_parts()
            .0;

        parts.extensions.insert(secret);
        parts.extensions.insert(daemon_id.to_string());

        let result = ObservatoryAuth::from_request_parts(&mut parts, &()).await;
        assert_eq!(result, Err(StatusCode::UNAUTHORIZED), "Missing header should return 401");
    }

    #[tokio::test]
    async fn test_observatory_auth_invalid_bearer_format() {
        let secret = create_test_secret();
        let daemon_id = "test-daemon-1";

        let mut parts = Request::builder()
            .header(axum::http::header::AUTHORIZATION, "InvalidFormat")
            .body(axum::body::Body::empty())
            .unwrap()
            .into_parts()
            .0;

        parts.extensions.insert(secret);
        parts.extensions.insert(daemon_id.to_string());

        let result = ObservatoryAuth::from_request_parts(&mut parts, &()).await;
        assert_eq!(result, Err(StatusCode::UNAUTHORIZED), "Invalid Bearer format should return 401");
    }

    #[tokio::test]
    async fn test_observatory_auth_daemon_mismatch() {
        let secret = create_test_secret();
        let token = create_test_token("observer-1", ObserverScope::Summary, "daemon-1");

        let mut parts = Request::builder()
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {}", token))
            .body(axum::body::Body::empty())
            .unwrap()
            .into_parts()
            .0;

        parts.extensions.insert(secret);
        parts.extensions.insert("daemon-2".to_string()); // Different daemon ID

        let result = ObservatoryAuth::from_request_parts(&mut parts, &()).await;
        assert_eq!(result, Err(StatusCode::UNAUTHORIZED), "Token for different daemon should return 401");
    }
}
