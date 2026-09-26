//! Host allowlist: the daemon-wide refusal of requests addressed to a name the
//! daemon does not answer to, which is what DNS rebinding looks like.
//!
//! # Why
//!
//! Origin checks cannot stop DNS rebinding. An attacker page at
//! `http://evil.example:4780` re-resolves its own name to `127.0.0.1`; from the
//! browser's point of view every request is then SAME-origin, so it carries
//! `Origin: http://evil.example:4780` (or none at all on a GET), CORS never
//! applies, and the page can READ responses — `GET /v1/fs/file` would hand it
//! any file under `$HOME`. The one thing the attacker cannot change is the
//! `Host` header: the browser sends the name it resolved, `evil.example:4780`.
//!
//! # The policy
//!
//! Every request, GET and OPTIONS included, whose `Host` header (and, when the
//! request line is in absolute form, its URI authority) names a host outside
//! [`AllowedHosts`] is refused with `421 {"ok":false,"code":"host_not_allowed"}`
//! before CORS or route dispatch. Allowed, on any port:
//!
//! - `localhost` and every loopback IP literal (`127.0.0.0/8`, `[::1]`), which
//!   covers the default bind and the `[::1]` companion listener;
//! - the IP literal of the actual `OCEAN_BIND` address; when that address is
//!   unspecified (`0.0.0.0` / `[::]`), ANY IP literal, since the daemon then
//!   answers on every interface and an IP literal cannot be rebound;
//! - the host of every `OCEAN_ALLOWED_ORIGINS` entry (a tunnel hostname);
//! - every entry of `OCEAN_ALLOWED_HOSTS` (comma separated, e.g. a MagicDNS
//!   machine name for a tailnet bind).
//!
//! Any other DNS name is refused. A request with no `Host` at all passes: every
//! browser sends one, so its absence is a raw HTTP/1.0 tool, not a page.

use std::{net::IpAddr, net::SocketAddr, sync::Arc};

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::room_operator::origin_of;

/// Stable wire code for a refused `Host`.
pub(crate) const HOST_NOT_ALLOWED: &str = "host_not_allowed";

/// The set of hosts this daemon answers to. Built once at startup.
#[derive(Clone, Debug)]
pub(crate) struct AllowedHosts {
    /// Lowercased DNS names (no port, no trailing dot).
    names: Arc<[String]>,
    /// The IP the daemon is bound to.
    bind_ip: IpAddr,
}

impl Default for AllowedHosts {
    /// Loopback only: the binary's default `127.0.0.1` bind with no extras.
    fn default() -> Self {
        Self::new(SocketAddr::from(([127, 0, 0, 1], 4780)), &[], Vec::new())
    }
}

impl AllowedHosts {
    /// `origins` are the normalized `OCEAN_ALLOWED_ORIGINS` entries; their
    /// hosts are admitted so a tunnel origin's own requests are not refused.
    /// `extra_hosts` are `OCEAN_ALLOWED_HOSTS` entries (`host` or `host:port`).
    pub(crate) fn new(bind: SocketAddr, origins: &[String], extra_hosts: Vec<String>) -> Self {
        let mut names: Vec<String> = Vec::new();
        for origin in origins {
            let origin = origin_of(origin);
            let authority = origin.split_once("://").map_or(origin.as_str(), |(_, a)| a);
            if let Some((host, _)) = split_host(authority) {
                names.push(host);
            }
        }
        for raw in extra_hosts {
            if let Some((host, _)) = split_host(&raw.trim().to_ascii_lowercase()) {
                names.push(host);
            }
        }
        names.retain(|name| !name.is_empty());
        Self {
            names: names.into(),
            bind_ip: bind.ip(),
        }
    }

    /// Whether a `Host` header value (`host[:port]`) is one this daemon answers to.
    pub(crate) fn allows(&self, authority: &str) -> bool {
        let Some((host, is_ip)) = split_host(&authority.trim().to_ascii_lowercase()) else {
            return false;
        };
        if is_ip {
            let Ok(ip) = host.parse::<IpAddr>() else {
                return false;
            };
            return ip.is_loopback() || self.bind_ip.is_unspecified() || ip == self.bind_ip;
        }
        host == "localhost" || self.names.contains(&host)
    }
}

/// Split `host[:port]` / `[v6][:port]` into its lowercased host (brackets and a
/// single trailing dot removed) and whether it is an IP literal. `None` for
/// anything malformed: an empty host, a path, or a non-numeric port. Userinfo
/// (`a@localhost`) needs no special case: it never equals an allowed name and
/// never parses as an IP literal, so it is refused like any unknown name.
fn split_host(authority: &str) -> Option<(String, bool)> {
    if authority.is_empty() || authority.contains(['/', '?', '#', ' ']) {
        return None;
    }
    let (host, port, bracketed) = if let Some(rest) = authority.strip_prefix('[') {
        let (inside, after) = rest.split_once(']')?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':')?),
        };
        (inside, port, true)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port), false),
            None => (authority, None, false),
        }
    };
    if let Some(port) = port {
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    let is_ip = bracketed || host.parse::<std::net::Ipv4Addr>().is_ok();
    if bracketed && host.parse::<std::net::Ipv6Addr>().is_err() {
        return None;
    }
    Some((host.to_string(), is_ip))
}

fn refusal() -> Response {
    (
        StatusCode::MISDIRECTED_REQUEST,
        Json(json!({
            "ok": false,
            "code": HOST_NOT_ALLOWED,
            "error": "request Host is not an address this daemon answers to",
        })),
    )
        .into_response()
}

/// Axum middleware applying [`AllowedHosts`] to every request. Mounted once in
/// `app_router`, outside CORS, so a rebinding preflight is refused too.
pub(crate) async fn refuse_foreign_hosts(
    State(hosts): State<AllowedHosts>,
    request: Request,
    next: Next,
) -> Response {
    let mut presented = Vec::with_capacity(2);
    for value in request.headers().get_all(header::HOST) {
        match value.to_str() {
            Ok(raw) => presented.push(raw.to_string()),
            Err(_) => return refusal(),
        }
    }
    if let Some(authority) = request.uri().authority() {
        presented.push(authority.as_str().to_string());
    }
    if let Some(bad) = presented.iter().find(|host| !hosts.allows(host)) {
        tracing::warn!(
            host = %bad,
            path = %request.uri().path(),
            code = HOST_NOT_ALLOWED,
            "refused request for a foreign Host"
        );
        return refusal();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback() -> AllowedHosts {
        AllowedHosts::default()
    }

    #[test]
    fn loopback_names_and_literals_pass_on_any_port() {
        for host in [
            "127.0.0.1:4780",
            "127.0.0.1",
            "localhost:4780",
            "LOCALHOST:8790",
            "localhost.",
            "[::1]:4780",
            "[::1]",
            "127.0.0.2:4780",
        ] {
            assert!(loopback().allows(host), "{host} must pass");
        }
    }

    #[test]
    fn rebinding_names_and_malformed_hosts_are_refused() {
        for host in [
            "evil.example:4780",
            "evil.example",
            "localhost.evil.example:4780",
            "127.0.0.1.evil.example",
            "127.0.0.1.nip.io:4780",
            "attacker@127.0.0.1:4780",
            "127.0.0.1:47x0",
            "127.0.0.1:",
            "[::1",
            "[not-an-ip]:4780",
            "",
            ":4780",
            "100.64.0.7:4780",
            "192.168.1.20:4780",
            "mini.tailnet.ts.net:4780",
        ] {
            assert!(!loopback().allows(host), "{host} must be refused");
        }
    }

    #[test]
    fn the_actual_bind_ip_passes_and_nothing_else_off_loopback() {
        let hosts = AllowedHosts::new("100.64.0.7:4780".parse().unwrap(), &[], Vec::new());
        assert!(hosts.allows("100.64.0.7:4780"));
        assert!(hosts.allows("127.0.0.1:4780"), "loopback stays allowed");
        assert!(!hosts.allows("100.64.0.8:4780"));
        assert!(!hosts.allows("mini.tailnet.ts.net:4780"));
    }

    #[test]
    fn an_unspecified_bind_admits_any_ip_literal_but_no_dns_name() {
        for bind in ["0.0.0.0:4780", "[::]:4780"] {
            let hosts = AllowedHosts::new(bind.parse().unwrap(), &[], Vec::new());
            assert!(hosts.allows("100.119.217.76:4780"), "{bind}");
            assert!(hosts.allows("[fd7a:115c:a1e0::1]:4780"), "{bind}");
            assert!(!hosts.allows("evil.example:4780"), "{bind}");
        }
    }

    #[test]
    fn allowed_origin_hosts_and_explicit_hosts_pass() {
        let hosts = AllowedHosts::new(
            "127.0.0.1:4780".parse().unwrap(),
            &["https://Ocean.Tunnel.test".into()],
            vec![" Mini.Tailnet.ts.net:4780 ".into(), "studio".into()],
        );
        assert!(hosts.allows("ocean.tunnel.test"));
        assert!(hosts.allows("ocean.tunnel.test:443"));
        assert!(hosts.allows("mini.tailnet.ts.net:4780"));
        assert!(hosts.allows("studio:4780"));
        assert!(!hosts.allows("tunnel.test"));
        assert!(!hosts.allows("evil.example"));
    }

    /// End-to-end through the production router.
    mod through_the_router {
        use super::*;
        use crate::cors::BrowserOrigins;
        use crate::tests::{fake_convene_state, AUTO_CONVENE_ENV_LOCK};
        use axum::{body::Body, http::Method};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        async fn send(
            app: &axum::Router,
            method: Method,
            uri: &str,
            host: Option<&str>,
        ) -> (StatusCode, serde_json::Value) {
            let mut builder = axum::http::Request::builder()
                .method(method.clone())
                .uri(uri);
            if let Some(host) = host {
                builder = builder.header(header::HOST, host);
            }
            if method == Method::OPTIONS {
                // A real CORS preflight from a trusted origin: if the Host
                // check sat inside CORS, CORS would answer it 200 first.
                builder = builder
                    .header(header::ORIGIN, "http://localhost:8080")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST");
            }
            let response = app
                .clone()
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, json)
        }

        #[tokio::test]
        async fn a_rebinding_host_cannot_read_a_home_file() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            let app = crate::app_router(BrowserOrigins::default(), AllowedHosts::default())
                .with_state(state);

            let home = std::env::var("HOME").unwrap();
            let home = std::fs::canonicalize(&home).unwrap();
            let dir = tempfile::TempDir::new_in(&home).unwrap();
            let secret = dir.path().join("id_ed25519");
            std::fs::write(&secret, "PRIVATE KEY MATERIAL").unwrap();
            let uri = format!(
                "/v1/fs/file?path={}",
                secret.to_string_lossy().replace('/', "%2F")
            );

            let (status, body) = send(&app, Method::GET, &uri, Some("evil.example:4780")).await;
            assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{body}");
            assert_eq!(body["code"], json!(HOST_NOT_ALLOWED));
            assert!(!body.to_string().contains("PRIVATE KEY"));

            let (status, body) = send(&app, Method::GET, &uri, Some("127.0.0.1:4780")).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(body.to_string().contains("PRIVATE KEY MATERIAL"));

            let (status, _) = send(&app, Method::GET, &uri, Some("[::1]:4780")).await;
            assert_eq!(status, StatusCode::OK, "the v6 companion listener's host");
        }

        #[tokio::test]
        async fn every_method_is_host_checked_and_hostless_requests_pass() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            let app = crate::app_router(BrowserOrigins::default(), AllowedHosts::default())
                .with_state(state);
            for method in [Method::GET, Method::OPTIONS, Method::POST, Method::DELETE] {
                let (status, body) =
                    send(&app, method.clone(), "/health", Some("evil.example")).await;
                assert_eq!(status, StatusCode::MISDIRECTED_REQUEST, "{method}");
                assert_eq!(body["code"], json!(HOST_NOT_ALLOWED), "{method}");
            }
            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/health")
                        .header(
                            header::HOST,
                            axum::http::HeaderValue::from_bytes(b"127.0.0.1\xff").unwrap(),
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::MISDIRECTED_REQUEST,
                "a Host that is not visible ASCII fails closed"
            );
            let (status, _) = send(&app, Method::GET, "/health", None).await;
            assert_eq!(status, StatusCode::OK);
            let (status, _) = send(&app, Method::GET, "/health", Some("localhost:4780")).await;
            assert_eq!(status, StatusCode::OK);
        }

        #[tokio::test]
        async fn an_absolute_form_authority_is_checked_too() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            let app = crate::app_router(BrowserOrigins::default(), AllowedHosts::default())
                .with_state(state);
            let (status, _) = send(
                &app,
                Method::GET,
                "http://evil.example:4780/health",
                Some("127.0.0.1:4780"),
            )
            .await;
            assert_eq!(status, StatusCode::MISDIRECTED_REQUEST);
        }
    }
}
