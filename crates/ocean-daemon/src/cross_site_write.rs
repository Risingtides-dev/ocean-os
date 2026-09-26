//! Cross-site write guard: the daemon-wide refusal of browser-driven writes
//! from pages the daemon does not trust.
//!
//! # Why CORS is not enough
//!
//! The global CORS layer (`cors.rs`) only decides whether a browser may READ a
//! response and whether a preflighted request may be SENT. A "simple" request
//! (`GET`/`HEAD`/`POST` with no custom header and a body that is absent,
//! `text/plain`, form-encoded, or multipart) is sent by the browser with no
//! preflight at all, so the handler runs before CORS has any say. On a daemon
//! bound to `127.0.0.1:4780`, that let any web page the operator visited
//! perform member-lane writes — `POST /v1/rooms/persistent/{key}/close?actor_id=…`
//! has no body and no custom header — and every other route that accepts a
//! raw body, a query-only request, or tolerates a missing JSON content type.
//!
//! # The policy
//!
//! Every request whose method is not `GET`, `HEAD`, or `OPTIONS` passes through
//! [`check`], which mirrors `room_operator::OperatorIdentity::check_origin`
//! and its cookie tripwire exactly, differing only in the trust set:
//!
//! 1. A `Cookie` header refuses the request (`ambient_credential_rejected`).
//!    The daemon has no cookie authentication, so a cookie means a browser is
//!    replaying ambient state. Checked first, on shape alone.
//! 2. Every present `Origin` and every present `Referer` is reduced to its
//!    scheme+authority and must be trusted by [`BrowserOrigins`] — the same set
//!    the CORS layer trusts (loopback on any port, `chrome-extension://…`, the
//!    Tauri webview origins, and `OCEAN_ALLOWED_ORIGINS`). Anything else,
//!    including the opaque `null` origin and a header value that is not visible
//!    ASCII, refuses the request (`foreign_origin_rejected`).
//! 3. An ABSENT `Origin` and `Referer` pass. Non-browser callers (ocean-cli,
//!    ocean-mcp, the TUI, the surface proxy, which builds its upstream request
//!    without them, Tauri's Rust side) send neither, and a browser always
//!    attaches `Origin` to a cross-origin `POST`/`PUT`/`PATCH`/`DELETE`, so a
//!    missing one is not a browser-driven DIRECT cross-site write. The proxy
//!    stripping `Origin` is a compatibility fact, not a protection: a write a
//!    page launders through an auth-off surface proxy arrives here Origin-less
//!    and passes, and only the proxy's own gate (ocean-surface #230) stops it.
//!
//! Safe methods are left alone: the observatory's compatibility cookie rides
//! `GET`, and a cross-site `GET` cannot read the response under CORS.
//!
//! Refusals are a fixed `403` with the operator lane's own vocabulary
//! (`{"ok":false,"code":…,"error":…}`), so an operator route refused here
//! answers byte-for-byte the code it would have answered itself.

use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::cors::BrowserOrigins;
use crate::room_operator::origin_of;

/// Why a state-changing request was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CrossSiteRefusal {
    /// The request carried a `Cookie` header.
    AmbientCredential,
    /// `Origin` or `Referer` was present and not a trusted local origin.
    ForeignOrigin,
}

impl CrossSiteRefusal {
    /// Stable wire code. Identical to `OperatorAuthError::code` for the same
    /// two conditions, on purpose.
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::AmbientCredential => "ambient_credential_rejected",
            Self::ForeignOrigin => "foreign_origin_rejected",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::AmbientCredential => "cookie-bearing requests cannot write to the daemon",
            Self::ForeignOrigin => "request origin is not allowed to write to the daemon",
        }
    }
}

impl IntoResponse for CrossSiteRefusal {
    fn into_response(self) -> Response {
        (
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "code": self.code(),
                "error": self.message(),
            })),
        )
            .into_response()
    }
}

/// Whether the guard applies to a method. Everything except the safe methods
/// the CORS spec lets a browser send freely for reading.
pub(crate) fn is_state_changing(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// The policy itself, independent of the transport. See the module docs.
pub(crate) fn check(origins: &BrowserOrigins, headers: &HeaderMap) -> Result<(), CrossSiteRefusal> {
    if headers.contains_key(header::COOKIE) {
        return Err(CrossSiteRefusal::AmbientCredential);
    }
    for name in [header::ORIGIN, header::REFERER] {
        for value in headers.get_all(&name) {
            // A present value that is not visible ASCII is still present: it
            // fails closed rather than being skipped.
            let Ok(raw) = value.to_str() else {
                return Err(CrossSiteRefusal::ForeignOrigin);
            };
            if !origins.trusts(&origin_of(raw)) {
                return Err(CrossSiteRefusal::ForeignOrigin);
            }
        }
    }
    Ok(())
}

/// Axum middleware applying [`check`] to every state-changing request before
/// route dispatch. Mounted once in `app_router`, inside CORS.
pub(crate) async fn refuse_cross_site_writes(
    State(origins): State<BrowserOrigins>,
    request: Request,
    next: Next,
) -> Response {
    if is_state_changing(request.method()) {
        if let Err(refusal) = check(&origins, request.headers()) {
            tracing::warn!(
                method = %request.method(),
                path = %request.uri().path(),
                code = refusal.code(),
                "refused cross-site write"
            );
            return refusal.into_response();
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(header::HeaderName, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(name.clone(), HeaderValue::from_str(value).unwrap());
        }
        map
    }

    fn local() -> BrowserOrigins {
        BrowserOrigins::default()
    }

    #[test]
    fn an_absent_origin_and_referer_pass_because_native_callers_send_neither() {
        assert_eq!(check(&local(), &HeaderMap::new()), Ok(()));
        assert_eq!(
            check(
                &local(),
                &headers(&[(header::CONTENT_TYPE, "application/json")])
            ),
            Ok(())
        );
    }

    #[test]
    fn a_cookie_is_refused_even_from_a_trusted_origin() {
        let h = headers(&[
            (header::COOKIE, "session=abc"),
            (header::ORIGIN, "http://127.0.0.1:8790"),
        ]);
        assert_eq!(
            check(&local(), &h),
            Err(CrossSiteRefusal::AmbientCredential)
        );
        assert_eq!(
            check(&local(), &headers(&[(header::COOKIE, "")])),
            Err(CrossSiteRefusal::AmbientCredential),
            "an empty cookie header is still a cookie header"
        );
    }

    #[test]
    fn a_foreign_origin_is_refused() {
        for origin in [
            "https://evil.example",
            "http://localhost.evil.example",
            "http://127.0.0.1.evil.example:4780",
            "null",
            "file://",
            "http://100.64.0.7:8790",
        ] {
            assert_eq!(
                check(&local(), &headers(&[(header::ORIGIN, origin)])),
                Err(CrossSiteRefusal::ForeignOrigin),
                "{origin} must be refused"
            );
        }
    }

    #[test]
    fn a_foreign_referer_is_refused_even_without_an_origin() {
        let h = headers(&[(header::REFERER, "https://evil.example/page?x=1")]);
        assert_eq!(check(&local(), &h), Err(CrossSiteRefusal::ForeignOrigin));
    }

    #[test]
    fn userinfo_cannot_disguise_a_foreign_host_as_loopback() {
        for (name, value) in [
            (header::REFERER, "http://localhost:8080@evil.example/page"),
            (header::ORIGIN, "http://127.0.0.1:8790@evil.example"),
        ] {
            assert_eq!(
                check(&local(), &headers(&[(name.clone(), value)])),
                Err(CrossSiteRefusal::ForeignOrigin),
                "{name}: {value}"
            );
        }
    }

    #[test]
    fn a_foreign_referer_is_refused_even_beside_a_trusted_origin() {
        let h = headers(&[
            (header::ORIGIN, "http://localhost:8080"),
            (header::REFERER, "https://evil.example/"),
        ]);
        assert_eq!(check(&local(), &h), Err(CrossSiteRefusal::ForeignOrigin));
    }

    #[test]
    fn a_second_origin_header_cannot_hide_behind_a_trusted_first_one() {
        let h = headers(&[
            (header::ORIGIN, "http://localhost:8080"),
            (header::ORIGIN, "https://evil.example"),
        ]);
        assert_eq!(check(&local(), &h), Err(CrossSiteRefusal::ForeignOrigin));
    }

    #[test]
    fn a_non_ascii_origin_fails_closed() {
        let mut h = HeaderMap::new();
        h.insert(
            header::ORIGIN,
            HeaderValue::from_bytes(b"http://\xe2\x98\x83.example").unwrap(),
        );
        assert_eq!(check(&local(), &h), Err(CrossSiteRefusal::ForeignOrigin));
    }

    #[test]
    fn every_local_client_origin_passes() {
        for origin in [
            "http://localhost:8080",
            "http://127.0.0.1:8790",
            "http://127.0.0.1:4780",
            "http://[::1]:5173",
            "https://localhost",
            "tauri://localhost",
            "https://tauri.localhost",
            "chrome-extension://abcdefghijklmnop",
        ] {
            assert_eq!(
                check(&local(), &headers(&[(header::ORIGIN, origin)])),
                Ok(()),
                "{origin} must pass"
            );
        }
        let referer = headers(&[(header::REFERER, "http://localhost:8080/rooms/abc?x=1")]);
        assert_eq!(
            check(&local(), &referer),
            Ok(()),
            "a referer reduces to its origin"
        );
        let tauri_referer = headers(&[(header::REFERER, "tauri://localhost/index.html")]);
        assert_eq!(check(&local(), &tauri_referer), Ok(()));
    }

    #[test]
    fn configured_extra_origins_pass_case_insensitively() {
        let origins = BrowserOrigins::new(vec!["https://Ocean.Tunnel.test".into()]);
        let h = headers(&[(header::ORIGIN, "https://ocean.tunnel.test")]);
        assert_eq!(check(&origins, &h), Ok(()));
        assert_eq!(
            check(&local(), &h),
            Err(CrossSiteRefusal::ForeignOrigin),
            "an unconfigured tunnel host is foreign"
        );
    }

    #[test]
    fn only_unsafe_methods_are_guarded() {
        for method in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(!is_state_changing(&method), "{method}");
        }
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(is_state_changing(&method), "{method}");
        }
        assert!(is_state_changing(&Method::from_bytes(b"PROPFIND").unwrap()));
    }

    #[test]
    fn refusal_codes_match_the_operator_lane_vocabulary() {
        use crate::room_operator::OperatorAuthError;
        assert_eq!(
            CrossSiteRefusal::AmbientCredential.code(),
            OperatorAuthError::AmbientCredential.code()
        );
        assert_eq!(
            CrossSiteRefusal::ForeignOrigin.code(),
            OperatorAuthError::ForeignOrigin.code()
        );
    }

    /// End-to-end through the real production router and a real room.
    mod through_the_router {
        use super::*;
        use crate::persistent_rooms::with_rooms;
        use crate::tests::{fake_convene_state, AUTO_CONVENE_ENV_LOCK};
        use axum::body::Body;
        use chrono::Utc;
        use http_body_util::BodyExt;
        use ocean_core::{RoomKey, RoomParticipant, RoomParticipantKind};
        use ocean_store::RoomStore;
        use tower::ServiceExt;

        const CLOSE: &str = "/v1/rooms/persistent/csrf-room/close?actor_id=alice";

        fn room_with_alice(state: &crate::AppState) -> RoomKey {
            let key = RoomKey::new("csrf-room");
            with_rooms(state, |store| {
                store.create(key.clone(), "CSRF", None, Utc::now())?;
                store.add_participant(
                    &key,
                    RoomParticipant {
                        id: "alice".into(),
                        kind: RoomParticipantKind::Human,
                        display_name: "Alice".into(),
                    },
                    Utc::now(),
                )
            })
            .unwrap();
            key
        }

        async fn send(
            app: &axum::Router,
            method: Method,
            uri: &str,
            pairs: &[(header::HeaderName, &str)],
            body: Body,
        ) -> (StatusCode, serde_json::Value) {
            let mut builder = axum::http::Request::builder().method(method).uri(uri);
            for (name, value) in pairs {
                builder = builder.header(name.clone(), *value);
            }
            let response = app
                .clone()
                .oneshot(builder.body(body).unwrap())
                .await
                .unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
            (status, json)
        }

        fn still_open(state: &crate::AppState, key: &RoomKey) -> bool {
            with_rooms(state, |store| store.get(key)).unwrap().is_some()
        }

        #[tokio::test]
        async fn a_no_preflight_member_close_from_a_foreign_page_is_refused_and_writes_nothing() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            let key = room_with_alice(&state);
            let rows_before = with_rooms(&state, |store| store.get(&key))
                .unwrap()
                .unwrap()
                .transcript
                .len();
            let app = crate::app_router(
                BrowserOrigins::default(),
                crate::host_guard::AllowedHosts::default(),
            )
            .with_state(state.clone());

            for (pairs, code) in [
                (
                    vec![(header::ORIGIN, "https://evil.example")],
                    "foreign_origin_rejected",
                ),
                (
                    vec![(header::REFERER, "https://evil.example/attack.html")],
                    "foreign_origin_rejected",
                ),
                (vec![(header::ORIGIN, "null")], "foreign_origin_rejected"),
                (
                    vec![
                        (header::COOKIE, "sid=1"),
                        (header::ORIGIN, "http://localhost:8080"),
                    ],
                    "ambient_credential_rejected",
                ),
            ] {
                let (status, body) = send(&app, Method::POST, CLOSE, &pairs, Body::empty()).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{pairs:?}");
                assert_eq!(body["ok"], json!(false));
                assert_eq!(body["code"], json!(code), "{pairs:?}");
            }
            assert!(
                still_open(&state, &key),
                "a refused close must not close the room"
            );
            let rows_after = with_rooms(&state, |store| store.get(&key))
                .unwrap()
                .unwrap()
                .transcript
                .len();
            assert_eq!(rows_after, rows_before, "a refused close writes no marker");
        }

        /// N1: the guard is not limited to room routes. `POST /v1/calls/demo`
        /// takes no body and no header, and `POST /v1/voice/stt` takes a raw
        /// body: both are simple requests a page can send without preflight.
        #[tokio::test]
        async fn non_room_simple_request_writes_from_a_foreign_page_are_refused() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            let app = crate::app_router(
                BrowserOrigins::default(),
                crate::host_guard::AllowedHosts::default(),
            )
            .with_state(state);
            for (uri, content_type, body) in [
                ("/v1/calls/demo", None, ""),
                ("/v1/voice/stt", Some("text/plain"), "not audio"),
            ] {
                let mut pairs = vec![(header::ORIGIN, "https://evil.example")];
                if let Some(ct) = content_type {
                    pairs.push((header::CONTENT_TYPE, ct));
                }
                let (status, json) = send(&app, Method::POST, uri, &pairs, Body::from(body)).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {json}");
                assert_eq!(json["code"], json!("foreign_origin_rejected"), "{uri}");
            }
        }

        /// N2: DELETE is guarded, not only POST.
        #[tokio::test]
        async fn a_foreign_delete_of_a_participant_is_refused_and_the_roster_is_intact() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            let key = room_with_alice(&state);
            let app = crate::app_router(
                BrowserOrigins::default(),
                crate::host_guard::AllowedHosts::default(),
            )
            .with_state(state.clone());
            let uri = "/v1/rooms/persistent/csrf-room/participants/alice";
            let (status, json) = send(
                &app,
                Method::DELETE,
                uri,
                &[(header::ORIGIN, "https://evil.example")],
                Body::empty(),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{json}");
            assert_eq!(json["code"], json!("foreign_origin_rejected"));
            let roster = with_rooms(&state, |store| store.get(&key))
                .unwrap()
                .unwrap()
                .room
                .participants;
            assert!(
                roster.iter().any(|p| p.id == "alice"),
                "a refused DELETE must leave the participant on the roster"
            );
        }

        /// S4, behaviourally: the guard sits INSIDE CORS, so a refusal to a
        /// trusted origin still carries CORS headers and the local page can
        /// read why it was refused. Outside CORS the 403 would carry none.
        #[tokio::test]
        async fn a_refusal_to_a_trusted_origin_still_carries_cors_headers() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            room_with_alice(&state);
            let app = crate::app_router(
                BrowserOrigins::default(),
                crate::host_guard::AllowedHosts::default(),
            )
            .with_state(state);
            let response = app
                .oneshot(
                    axum::http::Request::builder()
                        .method(Method::POST)
                        .uri(CLOSE)
                        .header(header::ORIGIN, "http://localhost:8080")
                        .header(header::COOKIE, "sid=1")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(
                response
                    .headers()
                    .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .map(|v| v.to_str().unwrap()),
                Some("http://localhost:8080")
            );
        }

        #[tokio::test]
        async fn a_member_close_with_no_origin_still_works_for_native_callers() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            let key = room_with_alice(&state);
            let app = crate::app_router(
                BrowserOrigins::default(),
                crate::host_guard::AllowedHosts::default(),
            )
            .with_state(state.clone());
            let (status, body) = send(&app, Method::POST, CLOSE, &[], Body::empty()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert!(!still_open(&state, &key));
        }

        #[tokio::test]
        async fn a_member_close_from_a_trusted_local_page_still_works() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            for origin in ["http://localhost:8080", "tauri://localhost"] {
                let tmp = tempfile::tempdir().unwrap();
                let state = fake_convene_state(&tmp);
                let key = room_with_alice(&state);
                let app = crate::app_router(
                    BrowserOrigins::default(),
                    crate::host_guard::AllowedHosts::default(),
                )
                .with_state(state.clone());
                let (status, body) = send(
                    &app,
                    Method::POST,
                    CLOSE,
                    &[
                        (header::ORIGIN, origin),
                        (header::REFERER, &format!("{origin}/rooms/csrf-room")),
                    ],
                    Body::empty(),
                )
                .await;
                assert_eq!(status, StatusCode::OK, "{origin}: {body}");
                assert!(!still_open(&state, &key), "{origin}");
            }
        }

        #[tokio::test]
        async fn a_cli_shaped_json_write_still_works() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            let app = crate::app_router(
                BrowserOrigins::default(),
                crate::host_guard::AllowedHosts::default(),
            )
            .with_state(state.clone());
            // Exactly what reqwest sends for `.json(&body)`: a content type,
            // no Origin, no Referer, no Cookie.
            let (status, body) = send(
                &app,
                Method::POST,
                "/v1/rooms/persistent",
                &[(header::CONTENT_TYPE, "application/json")],
                Body::from(r#"{"key":"cli-room","name":"CLI"}"#),
            )
            .await;
            assert!(status.is_success(), "{status}: {body}");
            assert!(still_open(&state, &RoomKey::new("cli-room")));

            // The same write from a foreign page is refused before dispatch.
            let (status, body) = send(
                &app,
                Method::POST,
                "/v1/rooms/persistent",
                &[
                    (header::CONTENT_TYPE, "application/json"),
                    (header::ORIGIN, "https://evil.example"),
                ],
                Body::from(r#"{"key":"evil-room","name":"Evil"}"#),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert!(!still_open(&state, &RoomKey::new("evil-room")));
        }

        #[tokio::test]
        async fn a_raw_body_attachment_upload_from_a_foreign_page_is_refused() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            let key = room_with_alice(&state);
            let app = crate::app_router(
                BrowserOrigins::default(),
                crate::host_guard::AllowedHosts::default(),
            )
            .with_state(state.clone());
            let (status, body) = send(
                &app,
                Method::POST,
                "/v1/rooms/persistent/csrf-room/attachments?filename=x.txt&content_type=text/plain&uploader_id=alice",
                &[
                    (header::CONTENT_TYPE, "text/plain"),
                    (header::ORIGIN, "https://evil.example"),
                ],
                Body::from("planted"),
            )
            .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
            assert_eq!(body["code"], json!("foreign_origin_rejected"));
            let stored = with_rooms(&state, |store| store.attachments(&key)).unwrap();
            assert!(
                stored.is_empty(),
                "nothing may be stored by a refused upload"
            );
        }

        #[tokio::test]
        async fn reads_and_preflights_are_not_guarded() {
            let _env = AUTO_CONVENE_ENV_LOCK.lock().await;
            let tmp = tempfile::tempdir().unwrap();
            let state = fake_convene_state(&tmp);
            let app = crate::app_router(
                BrowserOrigins::default(),
                crate::host_guard::AllowedHosts::default(),
            )
            .with_state(state);
            let (status, _) = send(
                &app,
                Method::GET,
                "/health",
                &[
                    (header::ORIGIN, "https://evil.example"),
                    (header::COOKIE, "sid=1"),
                ],
                Body::empty(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "reads stay CORS-governed only");

            let response = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method(Method::OPTIONS)
                        .uri(CLOSE)
                        .header(header::ORIGIN, "http://localhost:8080")
                        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert!(response.status().is_success(), "{}", response.status());
            assert!(response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
        }
    }
}
