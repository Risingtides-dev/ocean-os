//! Browser-origin trust policy for the daemon's global CORS middleware.

use axum::http::{header, HeaderValue, Method};
use tower_http::cors::{AllowOrigin, CorsLayer};

/// HTTP methods advertised in the CORS preflight (`Access-Control-Allow-Methods`).
/// Must cover EVERY method the router actually serves, or the browser's OPTIONS
/// preflight fails and the real request never fires (OCEAN-87). The router serves
/// GET/POST plus `PATCH /v1/projects/{id}`, `DELETE /v1/projects/{id}`, and
/// `DELETE /v1/rooms/persistent/{key}/participants/{id}`; OPTIONS is the preflight
/// method itself. Keep this in sync with the `Router::route()` method set.
fn cors_allowed_methods() -> [Method; 5] {
    [
        Method::GET,
        Method::POST,
        Method::PATCH,
        Method::DELETE,
        Method::OPTIONS,
    ]
}

/// Parse a comma-separated `OCEAN_ALLOWED_ORIGINS` list into normalized origins.
/// Whitespace is trimmed, empty entries dropped, and a trailing slash removed so
/// `https://app.example.com/` and `https://app.example.com` both match the
/// browser-sent `Origin` header (which never has a trailing slash).
pub(super) fn parse_allowed_origins(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().trim_end_matches('/'))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// CORS gate (OCEAN-53). Returns true for origins the daemon trusts:
///
/// 1. Loopback web origins on any port: `http://localhost:*`,
///    `http://127.0.0.1:*`, `http://[::1]:*` (and their `https` forms). This
///    covers the browser PWA however it's served (`trunk serve` :8080, vite
///    :5173, the surface proxy :8790) without hardcoding ports.
/// 2. Any `chrome-extension://...` origin — the Ocean side-panel extension runs
///    from a per-install id we can't enumerate, and it already declares the
///    daemon in its MV3 `host_permissions`/CSP `connect-src`.
/// 3. The Tauri desktop shell's webview origin — `tauri://localhost` (macOS /
///    Linux custom protocol) and `https://tauri.localhost` (Windows). The
///    shell hosts the same Leptos bundle as the PWA but serves it from the
///    `tauri:` custom protocol, so unlike the proxy path its daemon calls are
///    cross-origin and WKWebView enforces CORS on them.
/// 4. Exact matches against operator-configured `OCEAN_ALLOWED_ORIGINS` (e.g. a
///    tunnel hostname for phone access).
///
/// Everything else (arbitrary public web pages) is rejected.
fn is_trusted_origin(origin: &HeaderValue, extra: &[String]) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    is_loopback_origin(origin)
        || origin.starts_with("chrome-extension://")
        || origin == "tauri://localhost"
        || origin == "https://tauri.localhost"
        || extra.iter().any(|allowed| allowed == origin)
}

/// True for `http(s)://localhost|127.0.0.1|[::1]` with any (or no) port. Matches
/// only the exact loopback hosts — `localhost.evil.com` and
/// `127.0.0.1.evil.com` do NOT match because the host segment must end right
/// after the loopback name (`:` for a port, or end-of-string).
fn is_loopback_origin(origin: &str) -> bool {
    let host = match origin.strip_prefix("http://") {
        Some(rest) => rest,
        None => match origin.strip_prefix("https://") {
            Some(rest) => rest,
            None => return false,
        },
    };
    // Strip the port (everything after the first ':'), if present. IPv6 `[::1]`
    // is handled explicitly below since it contains its own colons in brackets.
    if let Some(rest) = host.strip_prefix("[::1]") {
        return rest.is_empty() || rest.starts_with(':');
    }
    let host_only = host.split(':').next().unwrap_or(host);
    host_only == "localhost" || host_only == "127.0.0.1"
}

/// Build the daemon's CORS layer from an already-normalized operator allowlist.
///
/// Keeping layer construction outside `main` lets the production router and the
/// route-contract tests exercise the same origin, method, and header policy.
pub(super) fn cors_layer(extra_origins: Vec<String>) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _req| {
            is_trusted_origin(origin, &extra_origins)
        }))
        .allow_methods(cors_allowed_methods())
        .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin(s: &str) -> HeaderValue {
        HeaderValue::from_str(s).unwrap()
    }

    #[test]
    fn cors_allows_localhost_on_any_port() {
        let extra: Vec<String> = vec![];
        for o in [
            "http://localhost:8080",  // trunk serve (PWA)
            "http://localhost:5173",  // vite (canvas-web)
            "http://127.0.0.1:8790",  // surface proxy
            "http://127.0.0.1:4780",  // daemon itself
            "https://localhost:3000", // https loopback
            "http://[::1]:8080",      // ipv6 loopback
            "http://localhost",       // no explicit port
        ] {
            assert!(
                is_trusted_origin(&origin(o), &extra),
                "loopback origin {o} must be allowed"
            );
        }
    }

    #[test]
    fn cors_allows_chrome_extension_origin() {
        let extra: Vec<String> = vec![];
        assert!(
            is_trusted_origin(&origin("chrome-extension://abcdefghijklmnop"), &extra),
            "the Ocean side-panel extension origin must be allowed"
        );
    }

    #[test]
    fn cors_allows_tauri_shell_origin() {
        let extra: Vec<String> = vec![];
        for o in ["tauri://localhost", "https://tauri.localhost"] {
            assert!(
                is_trusted_origin(&origin(o), &extra),
                "the Tauri desktop shell origin {o} must be allowed"
            );
        }
    }

    #[test]
    fn cors_allows_configured_extra_origins() {
        let extra = parse_allowed_origins("https://ocean.example.com, https://tunnel.test/");
        assert!(is_trusted_origin(
            &origin("https://ocean.example.com"),
            &extra
        ));
        // trailing slash in config is normalized away to match the Origin header
        assert!(is_trusted_origin(&origin("https://tunnel.test"), &extra));
    }

    #[test]
    fn cors_rejects_untrusted_public_origins() {
        let extra = parse_allowed_origins("https://ocean.example.com");
        for o in [
            "https://evil.com",
            "http://localhost.evil.com", // not a real loopback host
            "http://127.0.0.1.evil.com", // not a real loopback host
            "https://notlocalhost",      // unrelated
            "http://ocean.example.com",  // http when only https was allowed
        ] {
            assert!(
                !is_trusted_origin(&origin(o), &extra),
                "untrusted origin {o} must be rejected"
            );
        }
    }

    #[test]
    fn parse_allowed_origins_trims_and_drops_empties() {
        let parsed = parse_allowed_origins("  https://a.com ,, https://b.com/ ,  ");
        assert_eq!(parsed, vec!["https://a.com", "https://b.com"]);
        assert!(parse_allowed_origins("").is_empty());
        assert!(parse_allowed_origins("   ,  , ").is_empty());
    }

    /// The router serves PATCH (`/v1/projects/{id}`) and DELETE
    /// (`/v1/projects/{id}`, room participants). Both MUST be in the preflight
    /// allow-list or the browser's OPTIONS check fails and the call never fires.
    #[test]
    fn cors_allow_methods_include_patch_and_delete() {
        let methods = cors_allowed_methods();
        for required in [
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ] {
            assert!(
                methods.contains(&required),
                "CORS allow_methods must advertise {required} (a route serves it)"
            );
        }
    }
}
