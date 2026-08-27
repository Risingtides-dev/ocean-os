//! Rooms Phase 1 — the local operator principal.
//!
//! See `docs/specs/2026-08-25-ocean-rooms-phase1-room-agent-authorization-manifest.md` §3.
//!
//! # Why this exists
//!
//! The daemon binds `127.0.0.1:4780` and has historically treated every caller
//! as the operator, with `OCEAN_YOLO=1` as the default. That is defensible for
//! a loopback tool. It is **not** defensible as the basis for a durable
//! authority record, because the surface proxy legitimately binds
//! `0.0.0.0:8790` and forwards here — so any tailnet peer, and any web page the
//! operator visits, can reach an authorization route unless one is explicitly
//! built to reject them.
//!
//! Gate 0 §8 therefore requires a **fail-closed stop when authenticated
//! authorizer identity is unavailable**. This module is that stop. It
//! establishes an identity rather than inferring one.
//!
//! # The three rules
//!
//! 1. **Header-only.** The credential is read from `X-Ocean-Operator` and from
//!    nowhere else — never a query string, cookie, or body. A credential that
//!    can ride in a URL will eventually be logged.
//! 2. **Fail closed.** If the key file is absent or unreadable, mutations are
//!    refused with [`OperatorAuthError::Unavailable`] (a 503). It never
//!    degrades to "assume the caller is the operator".
//! 3. **`OCEAN_YOLO` has no effect here.** That flag governs per-call tool
//!    gating for an already-authorized agent. Conflating the two would let the
//!    operator default silently disable the authority model.

#[cfg(unix)]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use axum::http::HeaderMap;

/// Header carrying the operator credential. Header-only, by rule.
pub const OPERATOR_HEADER: &str = "x-ocean-operator";

/// Filename under the Ocean config dir.
const KEY_FILE: &str = "operator.key";

/// 32 bytes of entropy, base64url-encoded.
#[cfg(unix)]
const KEY_BYTES: usize = 32;

/// Why an authorization attempt was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorAuthError {
    /// No operator key is configured or it could not be read. Mutations are
    /// refused; this is the required fail-closed stop, NOT a fallback to
    /// ambient trust. Maps to 503.
    Unavailable,
    /// The `X-Ocean-Operator` header was absent. Maps to 401.
    Missing,
    /// The presented credential did not match. Maps to 403.
    Invalid,
    /// The request carried a `Cookie` header. The daemon has no cookie auth,
    /// so its presence means a browser is being driven and ambient credentials
    /// must never authorize. Maps to 403.
    AmbientCredential,
    /// `Origin`/`Referer` was present and not allowlisted. Maps to 403.
    ForeignOrigin,
}

impl OperatorAuthError {
    /// Stable machine-readable code for the response body. Consumed by the
    /// authorization routes in the next slice; defined here so the error type
    /// and its wire vocabulary land together rather than drifting apart.
    #[allow(dead_code)]
    pub fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "operator_identity_unavailable",
            Self::Missing => "operator_credential_missing",
            Self::Invalid => "operator_credential_invalid",
            Self::AmbientCredential => "ambient_credential_rejected",
            Self::ForeignOrigin => "foreign_origin_rejected",
        }
    }
}

impl std::fmt::Display for OperatorAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msg = match self {
            Self::Unavailable => {
                "no operator identity is configured; room-agent authorization is unavailable"
            }
            Self::Missing => "missing X-Ocean-Operator credential",
            Self::Invalid => "invalid operator credential",
            Self::AmbientCredential => "cookie-bearing requests cannot authorize",
            Self::ForeignOrigin => "request origin is not allowed to authorize",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for OperatorAuthError {}

/// The verified authorizer. Its `id` is what lands in
/// `room_agent_bindings.authorized_by`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorPrincipal {
    id: String,
}

impl OperatorPrincipal {
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// Holds the operator key, if one exists.
///
/// Constructed once at daemon start. `secret` is `None` when no key could be
/// established, which is the fail-closed state rather than an error at boot —
/// a daemon with no operator key still serves everything except authorization.
pub struct OperatorIdentity {
    secret: Option<String>,
    id: String,
    allowed_origins: Vec<String>,
}

impl std::fmt::Debug for OperatorIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperatorIdentity")
            .field("id", &self.id)
            .field("secret", &self.secret.as_ref().map(|_| "[redacted]"))
            .field("allowed_origins", &self.allowed_origins)
            .finish()
    }
}

impl OperatorIdentity {
    /// Load the key from `<config_dir>/operator.key`, creating it on first run.
    ///
    /// A failure to create or read is **not** fatal to the daemon: it yields an
    /// identity with no secret, which refuses every authorization with
    /// [`OperatorAuthError::Unavailable`].
    pub fn load(config_dir: &Path) -> Self {
        let path = config_dir.join(KEY_FILE);
        let secret = read_or_create_key(&path).ok();
        if secret.is_none() {
            tracing::warn!(
                path = %path.display(),
                "no operator key; room-agent authorization is unavailable (fail-closed)"
            );
        }
        // The id is a non-reversible fingerprint of the key, safe to store in
        // the binding and to log. Rotating the key changes the id, which makes
        // "who authorized this" answerable across rotations.
        let id = secret
            .as_ref()
            .map(|s| format!("operator:{}", fingerprint(s)))
            .unwrap_or_else(|| "operator:unconfigured".to_string());
        Self {
            secret,
            id,
            allowed_origins: default_allowed_origins(),
        }
    }

    /// Construct directly. Tests only — production goes through [`Self::load`].
    #[cfg(test)]
    pub fn for_test(secret: Option<&str>, allowed_origins: Vec<String>) -> Self {
        let secret = secret.map(str::to_string);
        let id = secret
            .as_ref()
            .map(|s| format!("operator:{}", fingerprint(s)))
            .unwrap_or_else(|| "operator:unconfigured".to_string());
        Self {
            secret,
            id,
            allowed_origins,
        }
    }

    /// Whether authorization is possible at all. Read-only inspection routes
    /// stay available when this is false; mutations do not.
    pub fn is_configured(&self) -> bool {
        self.secret.is_some()
    }

    /// Verify a request may perform an authorization mutation.
    ///
    /// Check order is deliberate. The ambient-credential and origin checks run
    /// **before** the credential comparison so a cookie-driven or cross-origin
    /// request is refused on its shape, without its presented value ever being
    /// compared — a browser-driven request should never get a timing signal
    /// about credential correctness.
    pub fn authorize(&self, headers: &HeaderMap) -> Result<OperatorPrincipal, OperatorAuthError> {
        if headers.contains_key(axum::http::header::COOKIE) {
            return Err(OperatorAuthError::AmbientCredential);
        }
        self.check_origin(headers)?;

        let Some(expected) = self.secret.as_deref() else {
            return Err(OperatorAuthError::Unavailable);
        };

        let presented = headers
            .get(OPERATOR_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or(OperatorAuthError::Missing)?;

        if constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
            Ok(OperatorPrincipal {
                id: self.id.clone(),
            })
        } else {
            Err(OperatorAuthError::Invalid)
        }
    }

    /// A present `Origin`/`Referer` must be allowlisted. An absent one is
    /// permitted: non-browser callers (curl, the TUI) send neither, and the
    /// header requirement already defeats classical CSRF on its own. This
    /// check is defence in depth, not the primary boundary.
    fn check_origin(&self, headers: &HeaderMap) -> Result<(), OperatorAuthError> {
        for name in [axum::http::header::ORIGIN, axum::http::header::REFERER] {
            let Some(value) = headers.get(name).and_then(|v| v.to_str().ok()) else {
                continue;
            };
            let origin = origin_of(value);
            if !self
                .allowed_origins
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&origin))
            {
                return Err(OperatorAuthError::ForeignOrigin);
            }
        }
        Ok(())
    }
}

/// Scheme + authority of a URL-ish header value, lowercased. A `Referer`
/// carries a full path; only its origin is meaningful here.
fn origin_of(raw: &str) -> String {
    let raw = raw.trim();
    let Some((scheme, rest)) = raw.split_once("://") else {
        return raw.to_ascii_lowercase();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    )
}

/// Origins permitted to authorize. Defaults to the local Surface and the
/// daemon itself; `OCEAN_OPERATOR_ALLOWED_ORIGINS` (comma separated) replaces
/// the list for deployments that serve the Surface elsewhere.
fn default_allowed_origins() -> Vec<String> {
    if let Ok(raw) = std::env::var("OCEAN_OPERATOR_ALLOWED_ORIGINS") {
        let list: Vec<String> = raw
            .split(',')
            .map(|s| origin_of(s.trim()))
            .filter(|s| !s.is_empty())
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    vec![
        "http://127.0.0.1:8790".into(),
        "http://localhost:8790".into(),
        "http://127.0.0.1:4780".into(),
        "http://localhost:4780".into(),
    ]
}

/// Read the key, or create it with owner-only permissions on first run.
#[cfg(unix)]
fn read_or_create_key(path: &Path) -> std::io::Result<String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => return read_existing_private_key(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let key = generate_key()?;
    write_owner_only(path, &key)?;
    Ok(key)
}

/// Room-agent mutation authority is unavailable on platforms where this
/// module cannot prove descriptor ownership, link count, and owner-only ACLs.
/// Supporting such a platform requires an equivalent native verifier; a plain
/// regular-file check is not an authorization boundary.
#[cfg(not(unix))]
fn read_or_create_key(_path: &Path) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "operator key security verification is unavailable on this platform",
    ))
}

/// Open and validate the named key itself. On Unix `O_NOFOLLOW` closes the
/// check/open race for symlinks; metadata comes from the opened descriptor so
/// a rename between path inspection and read cannot substitute another file.
#[cfg(unix)]
fn read_existing_private_key(path: &Path) -> std::io::Result<String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    use std::os::unix::fs::OpenOptionsExt;
    opts.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = opts.open(path)?;
    validate_private_key_file(&file)?;
    let mut existing = String::new();
    file.read_to_string(&mut existing)?;
    let trimmed = existing.trim().to_string();
    if trimmed.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "operator key is empty",
        ));
    }
    Ok(trimmed)
}

#[cfg(unix)]
fn validate_private_key_file(file: &std::fs::File) -> std::io::Result<()> {
    let metadata = file.metadata()?;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o7777 != 0o600
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "operator key must be a single-link owner-owned mode-0600 regular file",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn generate_key() -> std::io::Result<String> {
    let mut bytes = [0u8; KEY_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|e| std::io::Error::other(format!("operator key entropy: {e}")))?;
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// Create at mode 0600. The file is created with the mode already set rather
/// than chmod'd afterwards, so there is no window in which the key is
/// world-readable.
#[cfg(unix)]
fn write_owner_only(path: &Path, key: &str) -> std::io::Result<()> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600).custom_flags(libc::O_CLOEXEC);
    let mut f = opts.open(path)?;
    validate_private_key_file(&f)?;
    f.write_all(key.as_bytes())?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    Ok(())
}

/// Short non-reversible fingerprint, used as the operator id.
fn fingerprint(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Length-independent comparison. Compares lengths first because that is not
/// the secret, then diffs every byte without early exit.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The path of the operator key, for diagnostics.
pub fn operator_key_path(config_dir: &Path) -> PathBuf {
    config_dir.join(KEY_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const SECRET: &str = "test-operator-secret";

    fn identity() -> OperatorIdentity {
        OperatorIdentity::for_test(Some(SECRET), vec!["http://127.0.0.1:8790".into()])
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_correct_credential_authorizes() {
        let id = identity();
        let p = id
            .authorize(&headers(&[(OPERATOR_HEADER, SECRET)]))
            .unwrap();
        assert!(p.id().starts_with("operator:"));
    }

    #[test]
    fn a_missing_credential_is_missing_not_invalid() {
        assert_eq!(
            identity().authorize(&headers(&[])).unwrap_err(),
            OperatorAuthError::Missing
        );
    }

    #[test]
    fn a_wrong_credential_is_refused() {
        assert_eq!(
            identity()
                .authorize(&headers(&[(OPERATOR_HEADER, "nope")]))
                .unwrap_err(),
            OperatorAuthError::Invalid
        );
    }

    #[test]
    fn an_unconfigured_key_fails_closed_and_never_falls_back() {
        let id = OperatorIdentity::for_test(None, vec!["http://127.0.0.1:8790".into()]);
        assert!(!id.is_configured());
        // Even presenting *something* cannot authorize: there is nothing to
        // compare against, and the answer is unavailable, not invalid.
        assert_eq!(
            id.authorize(&headers(&[(OPERATOR_HEADER, "anything")]))
                .unwrap_err(),
            OperatorAuthError::Unavailable
        );
        assert_eq!(
            id.authorize(&headers(&[])).unwrap_err(),
            OperatorAuthError::Unavailable
        );
    }

    #[test]
    fn a_cookie_bearing_request_cannot_authorize_even_with_the_right_key() {
        let err = identity()
            .authorize(&headers(&[
                (OPERATOR_HEADER, SECRET),
                ("cookie", "session=abc"),
            ]))
            .unwrap_err();
        assert_eq!(err, OperatorAuthError::AmbientCredential);
    }

    #[test]
    fn a_foreign_origin_is_refused_before_the_credential_is_compared() {
        let err = identity()
            .authorize(&headers(&[
                (OPERATOR_HEADER, SECRET),
                ("origin", "https://evil.example"),
            ]))
            .unwrap_err();
        assert_eq!(err, OperatorAuthError::ForeignOrigin);
    }

    #[test]
    fn an_allowlisted_origin_passes() {
        identity()
            .authorize(&headers(&[
                (OPERATOR_HEADER, SECRET),
                ("origin", "http://127.0.0.1:8790"),
            ]))
            .unwrap();
    }

    #[test]
    fn a_referer_is_reduced_to_its_origin() {
        identity()
            .authorize(&headers(&[
                (OPERATOR_HEADER, SECRET),
                ("referer", "http://127.0.0.1:8790/rooms/hq/agents?x=1"),
            ]))
            .unwrap();
    }

    #[test]
    fn an_absent_origin_is_allowed_because_non_browser_callers_send_none() {
        identity()
            .authorize(&headers(&[(OPERATOR_HEADER, SECRET)]))
            .unwrap();
    }

    #[test]
    fn the_credential_is_never_read_from_a_query_or_body_shaped_header() {
        // Only the exact header counts; a lookalike does not.
        let err = identity()
            .authorize(&headers(&[("x-ocean-operator-token", SECRET)]))
            .unwrap_err();
        assert_eq!(err, OperatorAuthError::Missing);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_created_owner_only_and_is_stable_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let first = OperatorIdentity::load(dir.path());
        assert!(first.is_configured());
        let path = operator_key_path(dir.path());
        assert!(path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "operator key must be owner-only");
        }

        // Reloading yields the same identity rather than rotating it.
        let second = OperatorIdentity::load(dir.path());
        assert_eq!(first.id, second.id);
        let p = second
            .authorize(&headers(&[(
                OPERATOR_HEADER,
                std::fs::read_to_string(&path).unwrap().trim(),
            )]))
            .unwrap();
        assert_eq!(p.id(), first.id);
    }

    #[cfg(not(unix))]
    #[test]
    fn unsupported_platform_leaves_operator_authority_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let identity = OperatorIdentity::load(dir.path());
        assert!(!identity.is_configured());
        assert!(!operator_key_path(dir.path()).exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_insecure_existing_key_fails_closed_without_being_rewritten() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = operator_key_path(dir.path());
        std::fs::write(&path, "restored-but-exposed\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let identity = OperatorIdentity::load(dir.path());
        assert!(!identity.is_configured());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "restored-but-exposed\n",
            "fail-closed load must not rotate or overwrite an unsafe restored key"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_or_hard_link_key_fails_closed() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.key");
        std::fs::write(&target, "shared-secret\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        let linked_dir = tempfile::tempdir().unwrap();
        symlink(&target, operator_key_path(linked_dir.path())).unwrap();
        assert!(!OperatorIdentity::load(linked_dir.path()).is_configured());

        let hard_linked_dir = tempfile::tempdir().unwrap();
        std::fs::hard_link(&target, operator_key_path(hard_linked_dir.path())).unwrap();
        assert!(!OperatorIdentity::load(hard_linked_dir.path()).is_configured());
    }

    #[test]
    fn a_non_file_key_path_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(operator_key_path(dir.path())).unwrap();
        assert!(!OperatorIdentity::load(dir.path()).is_configured());
    }

    #[test]
    fn constant_time_eq_matches_ordinary_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn origin_of_extracts_scheme_and_authority() {
        assert_eq!(origin_of("http://Host:80/a/b?c"), "http://host:80");
        assert_eq!(origin_of("https://X.example"), "https://x.example");
    }
}
