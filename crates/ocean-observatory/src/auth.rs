//! Scoped observer principals with cryptographic token verification (HMAC-SHA256).

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use subtle::ConstantTimeEq;
use thiserror::Error;

/// Type alias for HMAC-SHA256.
type HmacSha256 = Hmac<Sha256>;

/// Cryptographic scope defining observer permissions within the Observatory.
///
/// Scopes determine what data an observer can read:
/// - **Metadata**: Only metadata-level redacted event fields (no content/secrets)
/// - **Content**: Full event content including non-secret fields
/// - **ExtensionProducer**: Content plus extension-producer-scoped data
///
/// No scope implies control/mutation capabilities — all observers are read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverScope {
    #[serde(rename = "summary")]
    Summary,
    Content,
    ExtensionProducer,
}

impl fmt::Display for ObserverScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ObserverScope::Summary => write!(f, "summary"),
            ObserverScope::Content => write!(f, "content"),
            ObserverScope::ExtensionProducer => write!(f, "extension_producer"),
        }
    }
}

/// An observer principal with cryptographic identity and scope constraints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserverPrincipal {
    /// Unique identifier for this principal (e.g., service name, user ID).
    pub principal_id: String,
    /// The scope/permission level this principal can access.
    pub scope: ObserverScope,
    /// Optional constraints or metadata about this principal.
    pub constraints: Option<String>,
}

impl ObserverPrincipal {
    /// Create a new observer principal.
    pub fn new(principal_id: impl Into<String>, scope: ObserverScope) -> Self {
        Self {
            principal_id: principal_id.into(),
            scope,
            constraints: None,
        }
    }

    /// Add constraints to this principal.
    pub fn with_constraints(mut self, constraints: impl Into<String>) -> Self {
        self.constraints = Some(constraints.into());
        self
    }
}

/// An observer authentication token with scope and daemon instance binding.
///
/// Tokens are HMAC-SHA256 signed and include:
/// - Scope information
/// - Daemon instance ID (prevents token replay across daemon instances)
/// - Issue and expiration timestamps
/// - Principal ID (extracted during verification)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserverToken {
    /// Principal ID this token represents.
    pub principal_id: String,
    /// The scope this token grants.
    pub scope: ObserverScope,
    /// Daemon instance ID this token is bound to (prevents cross-instance replay).
    pub daemon_instance_id: String,
    /// When the token was issued (RFC 3339).
    pub issued_at: String,
    /// When the token expires (RFC 3339).
    pub expires_at: String,
}

impl ObserverToken {
    /// Check if this token is expired (as of now).
    pub fn is_expired(&self) -> bool {
        if let Ok(expires) = DateTime::parse_from_rfc3339(&self.expires_at) {
            expires < Utc::now()
        } else {
            // Unparseable expiration → treat as expired (fail closed)
            true
        }
    }

    /// Check if this token is valid for a specific daemon instance.
    pub fn is_valid_for_daemon(&self, daemon_instance_id: &str) -> bool {
        self.daemon_instance_id == daemon_instance_id
    }
}

/// Daemon-local secret key for signing and verifying observer tokens.
///
/// On first boot, generates a random 32-byte key and persists it to `.ocean/observatory-secret`
/// with mode 0600. Subsequent boots load the same key for consistent verification.
/// This prevents token replay across daemon restarts.
#[derive(Clone)]
pub struct ObserverSecret {
    key: Vec<u8>,
}

impl ObserverSecret {
    /// Minimum key size in bytes (32 bytes = 256 bits for SHA256).
    pub const MIN_KEY_SIZE: usize = 32;

    /// Create a secret from a raw key (test helper).
    #[doc(hidden)]
    pub fn from_raw_key(key: Vec<u8>) -> Self {
        Self { key }
    }

    /// Load or generate the daemon secret from `.ocean/observatory-secret`.
    ///
    /// If the file does not exist, generates a new random 32-byte key and saves it.
    /// If the file exists, loads and validates it.
    ///
    /// File permissions are set to 0600 (read/write by owner only).
    pub fn load_or_generate(ocean_dir: &PathBuf) -> Result<Self, AuthError> {
        let secret_path = ocean_dir.join("observatory-secret");

        // Try to load existing secret.
        if secret_path.exists() {
            let mut file = OpenOptions::new()
                .read(true)
                .open(&secret_path)
                .map_err(|e| {
                    AuthError::SecretIo(format!("Failed to open observatory-secret: {}", e))
                })?;

            let mut key = Vec::new();
            file.read_to_end(&mut key)
                .map_err(|e| AuthError::SecretIo(format!("Failed to read observatory-secret: {}", e)))?;

            if key.len() < Self::MIN_KEY_SIZE {
                return Err(AuthError::SecretValidation(
                    format!("Secret key too small: {} bytes (min {})", key.len(), Self::MIN_KEY_SIZE)
                ));
            }

            return Ok(Self { key });
        }

        // Generate a new random secret if it doesn't exist.
        let key = Self::generate_key()?;
        let key_clone = key.clone();

        // Create the .ocean directory if it doesn't exist.
        fs::create_dir_all(ocean_dir).map_err(|e| {
            AuthError::SecretIo(format!("Failed to create .ocean directory: {}", e))
        })?;

        // Write the secret to file with 0600 permissions.
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&secret_path)
            .map_err(|e| {
                AuthError::SecretIo(format!("Failed to create observatory-secret: {}", e))
            })?;

        file.write_all(&key_clone)
            .map_err(|e| AuthError::SecretIo(format!("Failed to write observatory-secret: {}", e)))?;

        // Set permissions to 0600 (owner read/write only).
        #[cfg(unix)]
        {
            let perms = fs::Permissions::from_mode(0o600);
            fs::set_permissions(&secret_path, perms).map_err(|e| {
                AuthError::SecretIo(format!("Failed to set observatory-secret permissions: {}", e))
            })?;
        }

        Ok(Self { key })
    }

    /// Generate a new random 32-byte key.
    fn generate_key() -> Result<Vec<u8>, AuthError> {
        use rand::RngCore;

        let mut key = vec![0u8; Self::MIN_KEY_SIZE];
        let mut rng = rand::thread_rng();
        rng.fill_bytes(&mut key);
        Ok(key)
    }

    /// Get a reference to the underlying key.
    pub fn key(&self) -> &[u8] {
        &self.key
    }
}

/// Errors related to observer authentication and secret management.
#[derive(Debug, Error)]
pub enum AuthError {
    #[error("Invalid token signature")]
    InvalidSignature,

    #[error("Token has expired")]
    TokenExpired,

    #[error("Token is not valid for this daemon instance")]
    TokenInstanceMismatch,

    #[error("Malformed token: {0}")]
    MalformedToken(String),

    #[error("Secret IO error: {0}")]
    SecretIo(String),

    #[error("Secret validation error: {0}")]
    SecretValidation(String),
}

/// Sign an observer token with the daemon secret.
///
/// Encodes the token as JSON, computes an HMAC-SHA256 signature using the secret,
/// and returns the signature as a base64-encoded string appended to the JSON payload.
///
/// Format: `<base64(json)>.<base64(hmac_sha256_signature)>`
pub fn sign_token(token: &ObserverToken, secret: &ObserverSecret) -> String {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;

    let payload = serde_json::to_string(token).expect("token serialization failed");
    let payload_b64 = engine.encode(&payload);

    let mut mac = HmacSha256::new_from_slice(secret.key()).expect("key size valid");
    mac.update(payload_b64.as_bytes());
    let signature = mac.finalize();
    let signature_b64 = engine.encode(signature.into_bytes());

    format!("{}.{}", payload_b64, signature_b64)
}

/// Verify and extract an observer principal from a signed token.
///
/// Validates:
/// 1. Token format (must be `<payload>.<signature>`)
/// 2. HMAC-SHA256 signature (constant-time comparison)
/// 3. Expiration timestamp
/// 4. Daemon instance ID (must match provided instance ID)
///
/// Returns the extracted `ObserverPrincipal` on success, or an `AuthError` otherwise.
pub fn verify_token(
    token_string: &str,
    secret: &ObserverSecret,
    daemon_instance_id: &str,
) -> Result<ObserverPrincipal, AuthError> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;

    // Parse token format: `<payload>.<signature>`
    let parts: Vec<&str> = token_string.split('.').collect();
    if parts.len() != 2 {
        return Err(AuthError::MalformedToken(
            "Token must contain exactly one dot separator".to_string(),
        ));
    }

    let payload_b64 = parts[0];
    let signature_b64_provided = parts[1];

    // Decode payload from base64.
    let payload_bytes = engine.decode(payload_b64)
        .map_err(|e| AuthError::MalformedToken(format!("Invalid base64 payload: {}", e)))?;

    let payload_str =
        String::from_utf8(payload_bytes).map_err(|e| {
            AuthError::MalformedToken(format!("Invalid UTF-8 in payload: {}", e))
        })?;

    // Deserialize token.
    let token: ObserverToken = serde_json::from_str(&payload_str)
        .map_err(|e| AuthError::MalformedToken(format!("Invalid token JSON: {}", e)))?;

    // Verify HMAC signature (constant-time comparison).
    let mut mac = HmacSha256::new_from_slice(secret.key()).expect("key size valid");
    mac.update(payload_b64.as_bytes());
    let signature_computed = mac.finalize();
    let signature_computed_b64 = engine.encode(signature_computed.into_bytes());

    // Use constant-time comparison to prevent timing attacks.
    if signature_computed_b64.as_bytes().ct_eq(signature_b64_provided.as_bytes()).into() {
        // Signature is valid, proceed with other checks.
    } else {
        return Err(AuthError::InvalidSignature);
    }

    // Check expiration.
    if token.is_expired() {
        return Err(AuthError::TokenExpired);
    }

    // Check daemon instance ID.
    if !token.is_valid_for_daemon(daemon_instance_id) {
        return Err(AuthError::TokenInstanceMismatch);
    }

    // Extract and return principal.
    Ok(ObserverPrincipal::new(token.principal_id, token.scope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn create_test_secret() -> ObserverSecret {
        ObserverSecret {
            key: vec![0x42; ObserverSecret::MIN_KEY_SIZE],
        }
    }

    fn create_test_token(principal_id: &str, scope: ObserverScope) -> ObserverToken {
        let now = Utc::now();
        ObserverToken {
            principal_id: principal_id.to_string(),
            scope,
            daemon_instance_id: "test-daemon-1".to_string(),
            issued_at: now.to_rfc3339(),
            expires_at: (now + Duration::hours(1)).to_rfc3339(),
        }
    }

    #[test]
    fn test_sign_and_verify_token() {
        let secret = create_test_secret();
        let token = create_test_token("observer-1", ObserverScope::Summary);

        let signed = sign_token(&token, &secret);
        assert!(signed.contains('.'), "Signed token should contain a dot separator");

        let principal = verify_token(&signed, &secret, "test-daemon-1")
            .expect("Token verification should succeed");

        assert_eq!(principal.principal_id, "observer-1");
        assert_eq!(principal.scope, ObserverScope::Summary);
    }

    #[test]
    fn test_verify_expired_token() {
        let secret = create_test_secret();
        let now = Utc::now();
        let expired_token = ObserverToken {
            principal_id: "observer-1".to_string(),
            scope: ObserverScope::Content,
            daemon_instance_id: "test-daemon-1".to_string(),
            issued_at: (now - Duration::hours(2)).to_rfc3339(),
            expires_at: (now - Duration::hours(1)).to_rfc3339(),
        };

        let signed = sign_token(&expired_token, &secret);
        let result = verify_token(&signed, &secret, "test-daemon-1");

        assert!(
            matches!(result, Err(AuthError::TokenExpired)),
            "Expired token should be rejected"
        );
    }

    #[test]
    fn test_verify_malformed_token() {
        let secret = create_test_secret();

        let result = verify_token("invalid-token-no-dot", &secret, "test-daemon-1");
        assert!(
            matches!(result, Err(AuthError::MalformedToken(_))),
            "Malformed token should be rejected"
        );

        let result = verify_token("not.valid.base64.", &secret, "test-daemon-1");
        assert!(
            matches!(result, Err(AuthError::MalformedToken(_))),
            "Invalid base64 should be rejected"
        );
    }

    #[test]
    fn test_scope_extraction() {
        let secret = create_test_secret();

        for (scope, expected_scope) in &[
            (ObserverScope::Summary, ObserverScope::Summary),
            (ObserverScope::Content, ObserverScope::Content),
            (ObserverScope::ExtensionProducer, ObserverScope::ExtensionProducer),
        ] {
            let token = create_test_token("observer-1", *scope);
            let signed = sign_token(&token, &secret);
            let principal = verify_token(&signed, &secret, "test-daemon-1")
                .expect("Token verification should succeed");
            assert_eq!(principal.scope, *expected_scope);
        }
    }

    #[test]
    fn test_cross_principal_isolation() {
        let secret = create_test_secret();

        let token1 = create_test_token("principal-1", ObserverScope::Summary);
        let token2 = create_test_token("principal-2", ObserverScope::Content);

        let signed1 = sign_token(&token1, &secret);
        let signed2 = sign_token(&token2, &secret);

        let principal1 = verify_token(&signed1, &secret, "test-daemon-1")
            .expect("Token 1 should verify");
        let principal2 = verify_token(&signed2, &secret, "test-daemon-1")
            .expect("Token 2 should verify");

        assert_ne!(principal1.principal_id, principal2.principal_id);
        assert_ne!(principal1.scope, principal2.scope);
    }

    #[test]
    fn test_instance_mismatch() {
        let secret = create_test_secret();
        let token = create_test_token("observer-1", ObserverScope::Summary);
        let signed = sign_token(&token, &secret);

        let result = verify_token(&signed, &secret, "different-daemon-id");
        assert!(
            matches!(result, Err(AuthError::TokenInstanceMismatch)),
            "Token for different daemon instance should be rejected"
        );
    }

    #[test]
    fn test_observer_principal_with_constraints() {
        let principal = ObserverPrincipal::new("observer-1", ObserverScope::Content)
            .with_constraints("read-only");

        assert_eq!(principal.principal_id, "observer-1");
        assert_eq!(principal.scope, ObserverScope::Content);
        assert_eq!(principal.constraints, Some("read-only".to_string()));
    }

    #[test]
    fn test_no_control_scope_assertion() {
        // Observers have no mutation/control scope — they are always read-only.
        // This test documents that no scope variant implies control capabilities.
        // The three scopes are read-only variants representing different visibility levels.
        let secret = create_test_secret();

        for scope in &[
            ObserverScope::Summary,
            ObserverScope::Content,
            ObserverScope::ExtensionProducer,
        ] {
            let token = create_test_token("observer-1", *scope);
            let principal = verify_token(&sign_token(&token, &secret), &secret, "test-daemon-1")
                .expect("Token should verify");

            // All observers are read-only — the scope is only about visibility/data-access level.
            // Enforcement of read-only behavior happens at the API level, not in the token.
            // This test just documents the invariant by checking that principals are created.
            assert_eq!(principal.principal_id, "observer-1");
        }
    }
}
