//! Scoped observer principals and HMAC-SHA256 observer tokens.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use sha2::Sha256;
use thiserror::Error;

const SECRET_FILE: &str = "observatory-secret";
const OBSERVER_TOKEN_FILE: &str = "observatory-token";
const OBSERVER_TOKEN_ENV: &str = "OCEAN_OBSERVER_TOKEN";
const SECRET_LEN: usize = 32;
const SECRET_MODE: u32 = 0o600;
const NONCE_BYTES: usize = 16;
/// Default observer-token lifetime (30 minutes).
pub const DEFAULT_TOKEN_LIFETIME_SECS: u64 = 30 * 60;
const MIN_TOKEN_LIFETIME_SECS: u64 = 15 * 60;
const MAX_TOKEN_LIFETIME_SECS: u64 = 60 * 60;

type HmacSha256 = Hmac<Sha256>;

/// Read-only visibility granted to an observer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObserverScope {
    /// Metadata-safe whole-daemon summary visibility.
    Summary,
    /// Reserved future content visibility; not implemented by V1 routes.
    Content,
    /// Reserved future visibility isolated to one extension producer.
    ExtensionProducer(String),
}

impl ObserverScope {
    fn wire_value(&self) -> String {
        match self {
            Self::Summary => "observatory:summary".to_owned(),
            Self::Content => "observatory:content".to_owned(),
            Self::ExtensionProducer(producer_id) => format!("extension:{producer_id}"),
        }
    }
}

impl fmt::Display for ObserverScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.wire_value())
    }
}

impl Serialize for ObserverScope {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.wire_value())
    }
}

impl<'de> Deserialize<'de> for ObserverScope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "observatory:summary" => Ok(Self::Summary),
            "observatory:content" => Ok(Self::Content),
            _ => value
                .strip_prefix("extension:")
                .filter(|producer_id| !producer_id.is_empty())
                .map(|producer_id| Self::ExtensionProducer(producer_id.to_owned()))
                .ok_or_else(|| de::Error::custom("unknown observer scope")),
        }
    }
}

/// Authenticated observer identity extracted from token claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverPrincipal {
    /// Exact token principal. V1 accepts only `observer`.
    pub principal_id: String,
    /// Read-only visibility granted by the token.
    pub scope: ObserverScope,
}

/// Exact JSON claims signed into an observer token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserverToken {
    /// V1 observer principal (`observer`).
    pub principal: String,
    /// Namespaced read-only observer scope.
    pub scope: ObserverScope,
    /// Daemon boot instance to which this token is bound.
    pub daemon_instance_id: String,
    /// Unix-second issuance time.
    pub issued_at: u64,
    /// Unix-second expiry time.
    pub expires_at: u64,
    /// Random 128-bit nonce encoded as 32 lowercase hexadecimal characters.
    pub nonce: String,
}

impl ObserverToken {
    /// Issue V1 observer claims using the current Unix time and a random nonce.
    ///
    /// # Errors
    ///
    /// Returns an error if the system clock is before the Unix epoch or the
    /// expiry calculation overflows.
    pub fn issue(
        scope: ObserverScope,
        daemon_instance_id: impl Into<String>,
        lifetime_secs: u64,
    ) -> Result<Self, AuthError> {
        if matches!(&scope, ObserverScope::ExtensionProducer(producer_id) if producer_id.is_empty())
            || !(MIN_TOKEN_LIFETIME_SECS..=MAX_TOKEN_LIFETIME_SECS).contains(&lifetime_secs)
        {
            return Err(AuthError::InvalidClaims);
        }
        let issued_at = unix_time_now()?;
        let expires_at = issued_at
            .checked_add(lifetime_secs)
            .ok_or(AuthError::InvalidClaims)?;
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        rand::thread_rng().fill_bytes(&mut nonce_bytes);

        Ok(Self {
            principal: "observer".to_owned(),
            scope,
            daemon_instance_id: daemon_instance_id.into(),
            issued_at,
            expires_at,
            nonce: encode_hex(&nonce_bytes),
        })
    }
}

/// A validated, exactly 32-byte daemon-local signing secret.
#[derive(Clone)]
pub struct ObserverSecret([u8; SECRET_LEN]);

impl ObserverSecret {
    /// Required secret size in bytes.
    pub const LEN: usize = SECRET_LEN;

    /// Construct a secret from an exact-size key.
    #[doc(hidden)]
    pub const fn from_raw_key(key: [u8; SECRET_LEN]) -> Self {
        Self(key)
    }

    /// Load or securely create `<ocean_dir>/observatory-secret`.
    ///
    /// Creation writes a mode-0600 temporary inode completely, syncs it, and
    /// atomically links it to the final path without replacing an existing file.
    /// Concurrent creators therefore converge on the one winning file. Existing
    /// files are opened with `O_NOFOLLOW` and must be regular, exactly 32 bytes,
    /// and mode 0600.
    ///
    /// # Errors
    ///
    /// Fails closed for I/O errors, symlinks, non-regular files, wrong mode, or
    /// any size other than exactly 32 bytes.
    pub fn load_or_generate(ocean_dir: &Path) -> Result<Self, AuthError> {
        fs::create_dir_all(ocean_dir).map_err(AuthError::secret_io)?;
        let dir_metadata = fs::symlink_metadata(ocean_dir).map_err(AuthError::secret_io)?;
        if dir_metadata.file_type().is_symlink() || !dir_metadata.is_dir() {
            return Err(AuthError::SecretValidation(
                "observatory directory must be a real directory".to_owned(),
            ));
        }

        let secret_path = ocean_dir.join(SECRET_FILE);
        match Self::load(&secret_path) {
            Ok(secret) => return Ok(secret),
            Err(AuthError::SecretIo(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let mut key = [0_u8; SECRET_LEN];
        rand::thread_rng().fill_bytes(&mut key);
        let temp_path = ocean_dir.join(format!(".{SECRET_FILE}.{}.tmp", random_suffix()));
        let creation = (|| -> Result<(), AuthError> {
            let mut temp = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(SECRET_MODE)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&temp_path)
                .map_err(AuthError::secret_io)?;
            temp.write_all(&key).map_err(AuthError::secret_io)?;
            temp.sync_all().map_err(AuthError::secret_io)?;
            fs::hard_link(&temp_path, &secret_path).map_err(AuthError::secret_io)?;
            Ok(())
        })();
        let _ = fs::remove_file(&temp_path);

        match creation {
            Ok(()) => Self::load(&secret_path),
            Err(AuthError::SecretIo(error))
                if error.kind() == std::io::ErrorKind::AlreadyExists =>
            {
                Self::load(&secret_path)
            }
            Err(error) => Err(error),
        }
    }

    /// Borrow the exact signing key.
    pub const fn key(&self) -> &[u8; SECRET_LEN] {
        &self.0
    }

    fn load(path: &Path) -> Result<Self, AuthError> {
        let link_metadata = fs::symlink_metadata(path).map_err(AuthError::secret_io)?;
        if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
            return Err(AuthError::SecretValidation(
                "observatory secret must be a regular file".to_owned(),
            ));
        }

        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(AuthError::secret_io)?;
        let metadata = file.metadata().map_err(AuthError::secret_io)?;
        if !metadata.is_file() {
            return Err(AuthError::SecretValidation(
                "observatory secret must be a regular file".to_owned(),
            ));
        }
        if metadata.mode() & 0o777 != SECRET_MODE {
            return Err(AuthError::SecretValidation(
                "observatory secret mode must be 0600".to_owned(),
            ));
        }
        if metadata.len() != SECRET_LEN as u64 {
            return Err(AuthError::SecretValidation(
                "observatory secret must be exactly 32 bytes".to_owned(),
            ));
        }

        let mut key = [0_u8; SECRET_LEN];
        file.read_exact(&mut key).map_err(AuthError::secret_io)?;
        Ok(Self(key))
    }
}

/// Observer authentication and secret-storage failures.
#[derive(Debug, Error)]
pub enum AuthError {
    /// The supplied HMAC did not authenticate the payload.
    #[error("invalid token signature")]
    InvalidSignature,
    /// The token is past its Unix-second expiry.
    #[error("token has expired")]
    TokenExpired,
    /// The token belongs to another daemon boot instance.
    #[error("token is not valid for this daemon instance")]
    WrongInstance,
    /// The token framing, base64, JSON, or claims are malformed.
    #[error("malformed token")]
    MalformedToken,
    /// The claims violate the V1 observer contract.
    #[error("invalid token claims")]
    InvalidClaims,
    /// Secret storage failed at the filesystem boundary.
    #[error("observer secret I/O failed: {0}")]
    SecretIo(#[source] std::io::Error),
    /// An existing secret path failed closed validation.
    #[error("observer secret validation failed: {0}")]
    SecretValidation(String),
    /// A persisted child observer credential failed closed validation.
    #[error("observer credential config validation failed: {0}")]
    CredentialConfig(String),
}

impl AuthError {
    fn secret_io(error: std::io::Error) -> Self {
        Self::SecretIo(error)
    }
}

/// Mint and atomically publish a boot-bound summary observer credential at
/// `<ocean_dir>/observatory-token` with mode 0600.
///
/// The proxy and other first-party local clients read this file immediately
/// before opening an Observatory request or stream. Replacing the file rotates
/// credentials without exposing the daemon's signing secret.
///
/// # Errors
///
/// Fails closed for token issuance or any filesystem error.
pub fn write_summary_observer_token(
    ocean_dir: &Path,
    daemon_instance_id: &str,
    secret: &ObserverSecret,
) -> Result<String, AuthError> {
    fs::create_dir_all(ocean_dir).map_err(AuthError::secret_io)?;
    let directory = fs::symlink_metadata(ocean_dir).map_err(AuthError::secret_io)?;
    if directory.file_type().is_symlink() || !directory.is_dir() {
        return Err(AuthError::CredentialConfig(
            "observer token directory must be a real directory".to_owned(),
        ));
    }

    let claims = ObserverToken::issue(
        ObserverScope::Summary,
        daemon_instance_id,
        DEFAULT_TOKEN_LIFETIME_SECS,
    )?;
    let token = sign_token(&claims, secret);
    let final_path = ocean_dir.join(OBSERVER_TOKEN_FILE);
    let temporary = ocean_dir.join(format!(".{OBSERVER_TOKEN_FILE}.{}.tmp", random_suffix()));
    let result = (|| -> Result<(), AuthError> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(SECRET_MODE)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&temporary)
            .map_err(AuthError::secret_io)?;
        file.write_all(token.as_bytes())
            .map_err(AuthError::secret_io)?;
        file.write_all(b"\n").map_err(AuthError::secret_io)?;
        file.sync_all().map_err(AuthError::secret_io)?;
        fs::rename(&temporary, &final_path).map_err(AuthError::secret_io)?;
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result?;
    Ok(token)
}

/// Load the child-process observer credential from the environment or
/// `<ocean_dir>/observatory-token`, in that precedence order.
///
/// The token file's complete trimmed contents are the token. When present it
/// must be a non-symlink regular file with mode 0600. This helper does not log
/// or validate the token against a daemon instance; the receiving daemon owns
/// cryptographic validation.
///
/// # Errors
///
/// Fails closed for a non-Unicode/empty environment value or for a config path
/// with an unsafe type, mode, or unreadable/empty content.
pub fn observer_token_for_child(ocean_dir: &Path) -> Result<Option<String>, AuthError> {
    if let Some(value) = std::env::var_os(OBSERVER_TOKEN_ENV) {
        let token = value.into_string().map_err(|_| AuthError::InvalidClaims)?;
        if token.is_empty() {
            return Err(AuthError::InvalidClaims);
        }
        return Ok(Some(token));
    }

    let path = ocean_dir.join(OBSERVER_TOKEN_FILE);
    let link_metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AuthError::secret_io(error)),
    };
    if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
        return Err(AuthError::CredentialConfig(
            "observer config must be a regular file".to_owned(),
        ));
    }

    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(AuthError::secret_io)?;
    let metadata = file.metadata().map_err(AuthError::secret_io)?;
    if !metadata.is_file() || metadata.mode() & 0o777 != SECRET_MODE {
        return Err(AuthError::CredentialConfig(
            "observer config must be a regular mode-0600 file".to_owned(),
        ));
    }
    let mut token = String::new();
    file.read_to_string(&mut token)
        .map_err(AuthError::secret_io)?;
    let token = token.trim();
    if token.is_empty() {
        return Err(AuthError::CredentialConfig(
            "observer config token must not be empty".to_owned(),
        ));
    }
    Ok(Some(token.to_owned()))
}

/// Sign claims as `signature_base64.payload_base64`.
///
/// HMAC-SHA256 covers the raw JSON payload bytes, not its base64 text.
#[must_use]
pub fn sign_token(token: &ObserverToken, secret: &ObserverSecret) -> String {
    let payload = serde_json::to_vec(token).expect("ObserverToken serialization is infallible");
    let mut mac = HmacSha256::new_from_slice(secret.key()).expect("32-byte HMAC key is valid");
    mac.update(&payload);
    let signature = mac.finalize().into_bytes();
    format!("{}.{}", BASE64.encode(signature), BASE64.encode(payload))
}

/// Verify a signed token and extract its typed observer principal.
///
/// Signature verification occurs over the decoded raw JSON bytes before claims
/// are parsed. All malformed credential material fails closed.
///
/// # Errors
///
/// Returns a typed authentication failure for malformed, tampered, expired,
/// wrong-instance, or otherwise invalid claims.
pub fn verify_token(
    token: &str,
    secret: &ObserverSecret,
    daemon_instance_id: &str,
) -> Result<ObserverPrincipal, AuthError> {
    let (signature_b64, payload_b64) = token
        .split_once('.')
        .filter(|(_, payload)| !payload.contains('.'))
        .ok_or(AuthError::MalformedToken)?;
    let signature = BASE64
        .decode(signature_b64)
        .map_err(|_| AuthError::MalformedToken)?;
    let payload = BASE64
        .decode(payload_b64)
        .map_err(|_| AuthError::MalformedToken)?;

    let mut mac = HmacSha256::new_from_slice(secret.key()).expect("32-byte HMAC key is valid");
    mac.update(&payload);
    mac.verify_slice(&signature)
        .map_err(|_| AuthError::InvalidSignature)?;

    let claims: ObserverToken =
        serde_json::from_slice(&payload).map_err(|_| AuthError::MalformedToken)?;
    validate_claims(&claims, daemon_instance_id, unix_time_now()?)?;

    Ok(ObserverPrincipal {
        principal_id: claims.principal,
        scope: claims.scope,
    })
}

fn validate_claims(
    claims: &ObserverToken,
    daemon_instance_id: &str,
    now: u64,
) -> Result<(), AuthError> {
    if claims.principal != "observer"
        || claims.nonce.len() != NONCE_BYTES * 2
        || !claims.nonce.bytes().all(|byte| byte.is_ascii_hexdigit())
        || claims.expires_at < claims.issued_at
    {
        return Err(AuthError::InvalidClaims);
    }
    if now > claims.expires_at {
        return Err(AuthError::TokenExpired);
    }
    if claims.daemon_instance_id != daemon_instance_id {
        return Err(AuthError::WrongInstance);
    }
    Ok(())
}

fn unix_time_now() -> Result<u64, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| AuthError::InvalidClaims)
}

fn random_suffix() -> String {
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    encode_hex(&bytes)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::sync::{Arc, Barrier, Mutex};
    use std::thread;

    use super::*;

    const DAEMON_ID: &str = "a9f38dc1-fb42-46c4-9c64-0bf09aff3037";
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn secret() -> ObserverSecret {
        ObserverSecret::from_raw_key([0x42; SECRET_LEN])
    }

    fn valid_claims(scope: ObserverScope) -> ObserverToken {
        let now = unix_time_now().expect("clock");
        ObserverToken {
            principal: "observer".to_owned(),
            scope,
            daemon_instance_id: DAEMON_ID.to_owned(),
            issued_at: now,
            expires_at: now + 3_600,
            nonce: "3a2f456b9cfe4e3fa10c2d3567e8c92b".to_owned(),
        }
    }

    #[test]
    fn known_vector_signs_signature_first_over_raw_json() {
        let claims = ObserverToken {
            principal: "observer".to_owned(),
            scope: ObserverScope::Summary,
            daemon_instance_id: DAEMON_ID.to_owned(),
            issued_at: 1_721_233_351,
            expires_at: 1_721_233_951,
            nonce: "3a2f456b9cfe4e3fa10c2d3567e8c92b".to_owned(),
        };
        let key = std::array::from_fn(|index| u8::try_from(index).expect("index fits"));
        let signed = sign_token(&claims, &ObserverSecret::from_raw_key(key));

        assert_eq!(
            signed,
            "IAzbB6f+LsWs2OjtyscPFeHfQzXuvJIAC9xIOoBDaVw=.eyJwcmluY2lwYWwiOiJvYnNlcnZlciIsInNjb3BlIjoib2JzZXJ2YXRvcnk6c3VtbWFyeSIsImRhZW1vbl9pbnN0YW5jZV9pZCI6ImE5ZjM4ZGMxLWZiNDItNDZjNC05YzY0LTBiZjA5YWZmMzAzNyIsImlzc3VlZF9hdCI6MTcyMTIzMzM1MSwiZXhwaXJlc19hdCI6MTcyMTIzMzk1MSwibm9uY2UiOiIzYTJmNDU2YjljZmU0ZTNmYTEwYzJkMzU2N2U4YzkyYiJ9"
        );
    }

    #[test]
    fn issue_uses_unix_seconds_random_nonce_and_requested_expiry() {
        let first = ObserverToken::issue(
            ObserverScope::Summary,
            DAEMON_ID,
            DEFAULT_TOKEN_LIFETIME_SECS,
        )
        .expect("issue");
        let second = ObserverToken::issue(
            ObserverScope::Summary,
            DAEMON_ID,
            DEFAULT_TOKEN_LIFETIME_SECS,
        )
        .expect("issue");

        assert_eq!(
            first.expires_at - first.issued_at,
            DEFAULT_TOKEN_LIFETIME_SECS
        );
        assert_eq!(first.nonce.len(), 32);
        assert!(first.nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first.nonce, second.nonce);
        assert!(matches!(
            ObserverToken::issue(ObserverScope::Summary, DAEMON_ID, 899),
            Err(AuthError::InvalidClaims)
        ));
        assert!(matches!(
            ObserverToken::issue(ObserverScope::Summary, DAEMON_ID, 3_601),
            Err(AuthError::InvalidClaims)
        ));
    }

    #[test]
    fn round_trip_preserves_all_namespaced_scopes() {
        for scope in [
            ObserverScope::Summary,
            ObserverScope::Content,
            ObserverScope::ExtensionProducer("producer-a".to_owned()),
        ] {
            let claims = valid_claims(scope.clone());
            let principal = verify_token(&sign_token(&claims, &secret()), &secret(), DAEMON_ID)
                .expect("valid token");
            assert_eq!(principal.scope, scope);
        }
    }

    #[test]
    fn tampered_payload_and_signature_are_rejected() {
        let signed = sign_token(&valid_claims(ObserverScope::Summary), &secret());
        let (signature, payload) = signed.split_once('.').expect("two parts");
        let mut payload_bytes = BASE64.decode(payload).expect("payload base64");
        payload_bytes[0] ^= 1;
        let tampered_payload = format!("{signature}.{}", BASE64.encode(payload_bytes));
        assert!(matches!(
            verify_token(&tampered_payload, &secret(), DAEMON_ID),
            Err(AuthError::InvalidSignature)
        ));

        let mut signature_bytes = BASE64.decode(signature).expect("signature base64");
        signature_bytes[0] ^= 1;
        let tampered_signature = format!("{}.{payload}", BASE64.encode(signature_bytes));
        assert!(matches!(
            verify_token(&tampered_signature, &secret(), DAEMON_ID),
            Err(AuthError::InvalidSignature)
        ));
    }

    #[test]
    fn expired_and_wrong_instance_tokens_are_rejected() {
        let now = unix_time_now().expect("clock");
        let mut claims = valid_claims(ObserverScope::Summary);
        claims.issued_at = now - 20;
        claims.expires_at = now - 1;
        assert!(matches!(
            verify_token(&sign_token(&claims, &secret()), &secret(), DAEMON_ID),
            Err(AuthError::TokenExpired)
        ));

        let claims = valid_claims(ObserverScope::Summary);
        assert!(matches!(
            verify_token(&sign_token(&claims, &secret()), &secret(), "other-daemon"),
            Err(AuthError::WrongInstance)
        ));
    }

    #[test]
    fn malformed_framing_base64_json_and_scope_are_rejected() {
        for malformed in ["", "one-part", "a.b.c", "%%%.%%%", "YQ==.e30="] {
            assert!(verify_token(malformed, &secret(), DAEMON_ID).is_err());
        }

        let payload = br#"{"principal":"observer","scope":"observatory:control","daemon_instance_id":"a9f38dc1-fb42-46c4-9c64-0bf09aff3037","issued_at":1,"expires_at":9999999999,"nonce":"3a2f456b9cfe4e3fa10c2d3567e8c92b"}"#;
        let mut mac = HmacSha256::new_from_slice(secret().key()).expect("HMAC key");
        mac.update(payload);
        let token = format!(
            "{}.{}",
            BASE64.encode(mac.finalize().into_bytes()),
            BASE64.encode(payload)
        );
        assert!(matches!(
            verify_token(&token, &secret(), DAEMON_ID),
            Err(AuthError::MalformedToken)
        ));
    }

    #[test]
    fn secret_first_creation_is_exact_and_mode_0600() {
        let directory = tempfile::tempdir().expect("tempdir");
        let loaded = ObserverSecret::load_or_generate(directory.path()).expect("create secret");
        let path = directory.path().join(SECRET_FILE);
        let metadata = fs::metadata(path).expect("metadata");

        assert_eq!(loaded.key().len(), SECRET_LEN);
        assert_eq!(metadata.len(), SECRET_LEN as u64);
        assert_eq!(metadata.permissions().mode() & 0o777, SECRET_MODE);
    }

    #[test]
    fn secret_load_rejects_wrong_size_mode_type_and_symlink() {
        let wrong_size = tempfile::tempdir().expect("tempdir");
        fs::write(wrong_size.path().join(SECRET_FILE), [0_u8; 31]).expect("write");
        fs::set_permissions(
            wrong_size.path().join(SECRET_FILE),
            fs::Permissions::from_mode(SECRET_MODE),
        )
        .expect("chmod");
        assert!(matches!(
            ObserverSecret::load_or_generate(wrong_size.path()),
            Err(AuthError::SecretValidation(_))
        ));

        let wrong_mode = tempfile::tempdir().expect("tempdir");
        fs::write(wrong_mode.path().join(SECRET_FILE), [0_u8; SECRET_LEN]).expect("write");
        fs::set_permissions(
            wrong_mode.path().join(SECRET_FILE),
            fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        assert!(matches!(
            ObserverSecret::load_or_generate(wrong_mode.path()),
            Err(AuthError::SecretValidation(_))
        ));

        let wrong_type = tempfile::tempdir().expect("tempdir");
        fs::create_dir(wrong_type.path().join(SECRET_FILE)).expect("mkdir");
        assert!(matches!(
            ObserverSecret::load_or_generate(wrong_type.path()),
            Err(AuthError::SecretValidation(_))
        ));

        let symlink_dir = tempfile::tempdir().expect("tempdir");
        let target = symlink_dir.path().join("target");
        fs::write(&target, [0_u8; SECRET_LEN]).expect("write target");
        fs::set_permissions(&target, fs::Permissions::from_mode(SECRET_MODE)).expect("chmod");
        symlink(&target, symlink_dir.path().join(SECRET_FILE)).expect("symlink");
        assert!(matches!(
            ObserverSecret::load_or_generate(symlink_dir.path()),
            Err(AuthError::SecretValidation(_))
        ));
    }

    #[test]
    fn summary_token_file_is_mode_0600_boot_bound_and_readable() {
        let _guard = ENV_LOCK.lock().expect("environment lock");
        let prior = std::env::var_os(OBSERVER_TOKEN_ENV);
        std::env::remove_var(OBSERVER_TOKEN_ENV);

        let directory = tempfile::tempdir().expect("tempdir");
        let secret = ObserverSecret::from_raw_key([0x37; SECRET_LEN]);
        let token = write_summary_observer_token(directory.path(), "daemon-boot", &secret)
            .expect("mint token");
        let path = directory.path().join(OBSERVER_TOKEN_FILE);
        assert_eq!(
            fs::metadata(&path).expect("metadata").mode() & 0o777,
            SECRET_MODE
        );
        assert_eq!(
            observer_token_for_child(directory.path()).expect("read token"),
            Some(token.clone())
        );
        let principal = verify_token(&token, &secret, "daemon-boot").expect("verify token");
        assert_eq!(principal.scope, ObserverScope::Summary);

        match prior {
            Some(value) => std::env::set_var(OBSERVER_TOKEN_ENV, value),
            None => std::env::remove_var(OBSERVER_TOKEN_ENV),
        }
    }

    #[test]
    fn child_token_environment_precedes_secure_token_file() {
        let _guard = ENV_LOCK.lock().expect("environment lock");
        let prior = std::env::var_os(OBSERVER_TOKEN_ENV);
        let directory = tempfile::tempdir().expect("tempdir");
        let config = directory.path().join(OBSERVER_TOKEN_FILE);
        fs::write(&config, "config-token\n").expect("write config");
        fs::set_permissions(&config, fs::Permissions::from_mode(SECRET_MODE)).expect("chmod");

        std::env::remove_var(OBSERVER_TOKEN_ENV);
        assert_eq!(
            observer_token_for_child(directory.path()).expect("config token"),
            Some("config-token".to_owned())
        );

        std::env::set_var(OBSERVER_TOKEN_ENV, "environment-token");
        assert_eq!(
            observer_token_for_child(directory.path()).expect("environment token"),
            Some("environment-token".to_owned())
        );

        match prior {
            Some(value) => std::env::set_var(OBSERVER_TOKEN_ENV, value),
            None => std::env::remove_var(OBSERVER_TOKEN_ENV),
        }
    }

    #[test]
    fn child_token_file_rejects_unsafe_mode_and_symlink() {
        let _guard = ENV_LOCK.lock().expect("environment lock");
        let prior = std::env::var_os(OBSERVER_TOKEN_ENV);
        std::env::remove_var(OBSERVER_TOKEN_ENV);

        let wrong_mode = tempfile::tempdir().expect("tempdir");
        let config = wrong_mode.path().join(OBSERVER_TOKEN_FILE);
        fs::write(&config, "token").expect("write config");
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).expect("chmod");
        assert!(matches!(
            observer_token_for_child(wrong_mode.path()),
            Err(AuthError::CredentialConfig(_))
        ));

        let symlink_dir = tempfile::tempdir().expect("tempdir");
        let target = symlink_dir.path().join("target");
        fs::write(&target, "token").expect("write target");
        symlink(&target, symlink_dir.path().join(OBSERVER_TOKEN_FILE)).expect("symlink");
        assert!(matches!(
            observer_token_for_child(symlink_dir.path()),
            Err(AuthError::CredentialConfig(_))
        ));

        match prior {
            Some(value) => std::env::set_var(OBSERVER_TOKEN_ENV, value),
            None => std::env::remove_var(OBSERVER_TOKEN_ENV),
        }
    }

    #[test]
    fn concurrent_load_or_create_converges_on_one_secret() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = Arc::new(directory.path().to_path_buf());
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    *ObserverSecret::load_or_generate(path.as_path())
                        .expect("load or create")
                        .key()
                })
            })
            .collect();
        let keys: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().expect("thread"))
            .collect();

        assert!(keys.windows(2).all(|pair| pair[0] == pair[1]));
    }
}
