//! Stage A3b route tests: operator authentication, strict bodies, the
//! pre-commit versus committed envelopes (including the committed 202 and its
//! retry/reinspect contract), active-service refusal, disable-reaps-before-200
//! over a real supervised no-op service, revision-serialized reconciliation,
//! reader busy mapping, and startup recovery wiring.

use std::collections::HashSet;
use std::fs;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, Request, StatusCode};
use axum::routing::{delete, get, post};
use axum::Router;
use serde_json::{json, Value};
use uuid::Uuid;

use super::super::transaction::{CrashPoint, MutationError, RegistryWriter};
use super::super::{read_locked_state, SupervisorReconcile};
use super::*;
use crate::tests::{fake_convene_state, TestEnvRestore, AUTO_CONVENE_ENV_LOCK};

const ID: &str = "example.noop";
const OTHER: &str = "example.other";
const OPERATOR: &str = "test-room-operator";

/// A no-op native service speaking the strict stdio protocol: hello, ready,
/// pong, and a clean shutdown. Every other resource path is an executable
/// canary; nothing but the enabled service may ever run.
const SERVICE: &str = "#!/bin/sh\nIFS= read -r hello\nprintf '%s\\n' '{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":[],\"resume\":null}'\nIFS= read -r ready\nwhile IFS= read -r frame; do\n case \"$frame\" in\n  *'\"frame\":\"ping\"'*) nonce=$(printf '%s' \"$frame\" | sed -n 's/.*\"nonce\":\"\\([^\"]*\\)\".*/\\1/p'); printf '{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"pong\",\"nonce\":\"%s\"}\\n' \"$nonce\" ;;\n  *'\"frame\":\"shutdown\"'*) printf '%s\\n' '{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"shutdown_complete\"}'; exit 0 ;;\n esac\ndone\n";

struct Fixture {
    config: tempfile::TempDir,
    sources: tempfile::TempDir,
    canary: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let sources = tempfile::tempdir().unwrap();
        let canary = sources.path().join("PACKAGE_CANARY_RAN");
        Self {
            config: tempfile::tempdir().unwrap(),
            sources,
            canary,
        }
    }

    fn root(&self) -> PathBuf {
        self.config.path().join("extensions")
    }

    /// A package with one native service and no capability request.
    fn package(&self, name: &str, id: &str, version: &str) -> String {
        let dir = self.sources.path().join(name);
        fs::create_dir_all(dir.join("services")).unwrap();
        fs::create_dir_all(dir.join("hooks")).unwrap();
        fs::write(
            dir.join("ocean-extension.toml"),
            format!(
                "schema_version = 1\nid = \"{id}\"\nname = \"Noop\"\nversion = \"{version}\"\nmin_ocean_version = \"0.1.0\"\n\n[[services]]\nid = \"lifecycle\"\nentry = \"services/lifecycle\"\nevents = []\n"
            ),
        )
        .unwrap();
        fs::write(dir.join("services/lifecycle"), SERVICE).unwrap();
        for canary in ["hooks/on-install", "run-me"] {
            fs::write(
                dir.join(canary),
                format!("#!/bin/sh\nprintf ran > '{}'\n", self.canary.display()),
            )
            .unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        for executable in ["services/lifecycle", "hooks/on-install", "run-me"] {
            fs::set_permissions(dir.join(executable), fs::Permissions::from_mode(0o755)).unwrap();
        }
        dir.to_str().unwrap().to_string()
    }

    fn revision(&self) -> u64 {
        read_locked_state(self.config.path())
            .unwrap()
            .snapshot
            .revision
    }

    fn assert_no_canary_ran(&self) {
        assert!(
            !self.canary.exists(),
            "package code executed outside activation"
        );
    }

    fn entries(&self, relative: &str) -> Vec<String> {
        fs::read_dir(self.root().join(relative))
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/extensions", get(super::super::list))
        .route("/v1/extensions/install", post(install))
        .route("/v1/extensions/{id}", delete(remove))
        .route("/v1/extensions/{id}/inspect", get(super::super::inspect))
        .route("/v1/extensions/{id}/status", get(super::super::status))
        .route("/v1/extensions/{id}/trust", post(trust))
        .route("/v1/extensions/{id}/enable", post(enable))
        .route("/v1/extensions/{id}/disable", post(disable))
        .route("/v1/extensions/{id}/update", post(update))
        .with_state(state)
}

#[derive(Clone, Copy)]
enum Auth {
    Operator,
    None,
    Wrong,
    Cookie,
}

async fn send(
    app: &Router,
    method: Method,
    uri: &str,
    body: Option<Value>,
    auth: Auth,
) -> (StatusCode, Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let mut request = Request::builder().method(method).uri(uri);
    match auth {
        Auth::Operator => request = request.header("x-ocean-operator", OPERATOR),
        Auth::None => {}
        Auth::Wrong => request = request.header("x-ocean-operator", "not-the-key"),
        Auth::Cookie => {
            request = request
                .header("x-ocean-operator", OPERATOR)
                .header("cookie", "session=1")
        }
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap(),
        None => request.body(axum::body::Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_json(app: &Router, uri: &str) -> (StatusCode, Value) {
    send(app, Method::GET, uri, None, Auth::None).await
}

async fn post_op(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
    send(app, Method::POST, uri, Some(body), Auth::Operator).await
}

fn assert_precommit(body: &Value, code: &str, revision: u64) {
    assert_eq!(body["ok"], false, "{body}");
    assert_eq!(body["mutation"]["committed"], false, "{body}");
    assert_eq!(body["mutation"]["state_revision"], revision, "{body}");
    assert!(body["mutation"]["operation_id"].as_str().is_some());
    assert_eq!(body["error"]["code"], code, "{body}");
    assert!(body["error"]["message"].as_str().is_some());
}

fn assert_committed(body: &Value, revision: u64) {
    assert_eq!(body["ok"], true, "{body}");
    assert_eq!(body["mutation"]["committed"], true, "{body}");
    assert_eq!(body["mutation"]["state_revision"], revision, "{body}");
    assert!(Uuid::parse_str(body["mutation"]["operation_id"].as_str().unwrap()).is_ok());
}

async fn install_local(app: &Router, expected: u64, path: &str) -> (StatusCode, Value) {
    post_op(
        app,
        "/v1/extensions/install",
        json!({"expected_state_revision": expected, "source": {"kind": "local-path", "path": path}}),
    )
    .await
}

/// Preview then apply the exact service grant; returns the apply response.
async fn trust_noop(app: &Router, id: &str, expected: u64, digest: &str) -> (StatusCode, Value) {
    let request = json!({
        "expected_state_revision": expected,
        "digest": digest,
        "capabilities": {},
        "service_grants": [{"service_id": "lifecycle", "native_process_ack": true, "secret_bindings": []}],
        "confirm_grant_diff": null
    });
    let (status, preview) =
        post_op(app, &format!("/v1/extensions/{id}/trust"), request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["committed"], false);
    assert_eq!(preview["state_revision"], expected);
    assert_eq!(
        preview["preview"]["native_authority_notice"],
        super::super::transaction::NATIVE_AUTHORITY_NOTICE
    );
    let mut apply = request;
    apply["confirm_grant_diff"] = preview["preview"]["confirmation"].clone();
    post_op(app, &format!("/v1/extensions/{id}/trust"), apply).await
}

async fn scope(app: &Router, id: &str, verb: &str, expected: u64) -> (StatusCode, Value) {
    post_op(
        app,
        &format!("/v1/extensions/{id}/{verb}"),
        json!({"expected_state_revision": expected, "scope": {"kind": "global"}}),
    )
    .await
}

async fn wait_for_status(app: &Router, id: &str, want: impl Fn(&Value) -> bool) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let (status, body) = get_json(app, &format!("/v1/extensions/{id}/status")).await;
        assert_eq!(status, StatusCode::OK);
        if want(&body) {
            return body;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "status never matched: {body}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn process_alive(pid: i64) -> bool {
    // Signal 0 probes existence without delivering anything.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

async fn with_supervisor(
    state: &mut AppState,
    config: &FsPath,
) -> Arc<crate::extension_service::ExtensionSupervisor> {
    let supervisor = crate::extension_service::ExtensionSupervisor::new_with_lifecycle(Arc::clone(
        &state.extension_lifecycle,
    ));
    supervisor.start(config.to_path_buf(), HashSet::new()).await;
    state.extension_supervisor = Some(Arc::clone(&supervisor));
    supervisor
}

// ---------------------------------------------------------------------------
// Pure envelope mapping.
// ---------------------------------------------------------------------------

#[test]
fn envelope_states_cover_every_reconciliation_outcome() {
    use SupervisorReconcile::*;
    let cases = [
        (
            Complete { reaped: true },
            true,
            true,
            false,
            Reconciliation::Complete,
            Reap::Complete,
        ),
        (
            Complete { reaped: false },
            true,
            false,
            false,
            Reconciliation::Complete,
            Reap::NotRequired,
        ),
        (
            CleanupIncomplete,
            false,
            false,
            false,
            Reconciliation::Pending,
            Reap::Pending,
        ),
        // Queued/blocked with a still-owned process for a no-longer-effective
        // package: the reap is outstanding.
        (
            Pending,
            false,
            false,
            false,
            Reconciliation::Pending,
            Reap::Pending,
        ),
        (
            Blocked,
            false,
            false,
            false,
            Reconciliation::Blocked,
            Reap::Pending,
        ),
        // Nothing owned, or the package is meant to run: nothing to reap.
        (
            Pending,
            false,
            true,
            false,
            Reconciliation::Pending,
            Reap::NotRequired,
        ),
        (
            Blocked,
            true,
            false,
            false,
            Reconciliation::Blocked,
            Reap::NotRequired,
        ),
        // Deferred retention cleanup is a pending reap even after a complete
        // reconciliation.
        (
            Complete { reaped: false },
            false,
            true,
            true,
            Reconciliation::Complete,
            Reap::Pending,
        ),
    ];
    for (reconcile, effective, stopped, retention, reconciliation, reap) in cases {
        assert_eq!(
            envelope_states(reconcile, effective, stopped, retention),
            (reconciliation, reap),
            "{reconcile:?} effective={effective} stopped={stopped} retention={retention}"
        );
    }
    assert_eq!(
        committed_status(Reconciliation::Complete, Reap::Complete),
        StatusCode::OK
    );
    assert_eq!(
        committed_status(Reconciliation::Complete, Reap::NotRequired),
        StatusCode::OK
    );
    assert_eq!(
        committed_status(Reconciliation::Complete, Reap::Pending),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        committed_status(Reconciliation::Pending, Reap::NotRequired),
        StatusCode::ACCEPTED
    );
    assert_eq!(
        committed_status(Reconciliation::Blocked, Reap::NotRequired),
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn committed_recovery_error_is_500_with_committed_revision_and_fixed_text() {
    use http_body_util::BodyExt;
    let operation_id = Uuid::new_v4();
    let response = mutation_error(MutationError {
        operation_id,
        committed: true,
        state_revision: 9,
        code: "registry_recovery_required",
    });
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        body,
        json!({
            "ok": false,
            "mutation": {"operation_id": operation_id, "committed": true, "state_revision": 9},
            "error": {"code": "registry_recovery_required", "message": "extension registry recovery is required"}
        })
    );
}

#[test]
fn every_writer_code_has_a_closed_status_and_fixed_message() {
    for (code, status) in [
        ("invalid_request", StatusCode::BAD_REQUEST),
        ("invalid_source", StatusCode::BAD_REQUEST),
        ("package_invalid", StatusCode::BAD_REQUEST),
        ("declaration_policy_rejected", StatusCode::BAD_REQUEST),
        ("extension_not_installed", StatusCode::NOT_FOUND),
        ("project_not_found", StatusCode::NOT_FOUND),
        ("already_installed", StatusCode::CONFLICT),
        ("state_revision_conflict", StatusCode::CONFLICT),
        ("extension_active", StatusCode::CONFLICT),
        ("grant_confirmation_mismatch", StatusCode::CONFLICT),
        ("trust_required", StatusCode::CONFLICT),
        ("acquisition_capacity", StatusCode::TOO_MANY_REQUESTS),
        ("extension_state_busy", StatusCode::SERVICE_UNAVAILABLE),
        (
            "operator_credential_missing",
            StatusCode::SERVICE_UNAVAILABLE,
        ),
        ("operator_credential_invalid", StatusCode::FORBIDDEN),
        (
            "git_connection_pinning_unavailable",
            StatusCode::NOT_IMPLEMENTED,
        ),
        (
            "extension_state_malformed",
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    ] {
        assert_eq!(precommit_status(code), status, "{code}");
        let code: &'static str = Box::leak(code.to_owned().into_boxed_str());
        let message = message_for(code);
        assert!(!message.is_empty());
        assert!(!message.contains('/'), "message must not look like a path");
    }
    assert_eq!(
        error_object("extension_state_busy")["retryable"],
        Value::Bool(true)
    );
    assert!(error_object("state_revision_conflict")
        .get("retryable")
        .is_none());
}

// ---------------------------------------------------------------------------
// Authentication and strict bodies.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_mutation_is_operator_authenticated_and_reads_stay_credential_free() {
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let fixture = Fixture::new();
    let app = router(fake_convene_state(&fixture.config));
    let path = fixture.package("noop", ID, "1.0.0");
    let mutations: [(Method, String, Value); 6] = [
        (
            Method::POST,
            "/v1/extensions/install".into(),
            json!({"expected_state_revision": 0, "source": {"kind": "local-path", "path": path}}),
        ),
        (
            Method::POST,
            format!("/v1/extensions/{ID}/update"),
            json!({"expected_state_revision": 0, "source": {"kind": "local-path", "path": path}}),
        ),
        (
            Method::POST,
            format!("/v1/extensions/{ID}/trust"),
            json!({"expected_state_revision": 0, "digest": "sha256:00", "confirm_grant_diff": null}),
        ),
        (
            Method::POST,
            format!("/v1/extensions/{ID}/enable"),
            json!({"expected_state_revision": 0, "scope": {"kind": "global"}}),
        ),
        (
            Method::POST,
            format!("/v1/extensions/{ID}/disable"),
            json!({"expected_state_revision": 0, "scope": {"kind": "global"}}),
        ),
        (
            Method::DELETE,
            format!("/v1/extensions/{ID}"),
            json!({"expected_state_revision": 0, "purge_state": false}),
        ),
    ];
    for (method, uri, body) in &mutations {
        for (auth, status, code) in [
            (
                Auth::None,
                StatusCode::SERVICE_UNAVAILABLE,
                "operator_credential_missing",
            ),
            (
                Auth::Wrong,
                StatusCode::FORBIDDEN,
                "operator_credential_invalid",
            ),
            (
                Auth::Cookie,
                StatusCode::FORBIDDEN,
                "ambient_credential_rejected",
            ),
        ] {
            let (got, response) = send(&app, method.clone(), uri, Some(body.clone()), auth).await;
            assert_eq!(got, status, "{method} {uri}: {response}");
            assert_precommit(&response, code, 0);
        }
        // Authentication precedes body validation: garbage without a
        // credential is refused as unauthenticated, not as malformed.
        let (got, response) = send(
            &app,
            method.clone(),
            uri,
            Some(json!({"unexpected": true})),
            Auth::None,
        )
        .await;
        assert_eq!(got, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response["error"]["code"], "operator_credential_missing");
    }
    assert!(
        !fixture.root().exists(),
        "a refused mutation wrote registry state"
    );

    let (status, body) = get_json(&app, "/v1/extensions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state_revision"], 0);
    assert_eq!(body["extensions"], json!([]));
    let (status, body) = get_json(&app, &format!("/v1/extensions/{ID}/status")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["services"], json!([]));
    assert_eq!(body["probe_run"], false);
    let (status, body) = get_json(&app, "/v1/extensions/not-valid/status").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid_extension_id");
    fixture.assert_no_canary_ran();
}

#[tokio::test]
async fn strict_bodies_and_git_sources_fail_before_any_acquisition() {
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let fixture = Fixture::new();
    let app = router(fake_convene_state(&fixture.config));
    let path = fixture.package("noop", ID, "1.0.0");

    for body in [
        json!({"expected_state_revision": 0, "source": {"kind": "local-path", "path": path}, "extra": 1}),
        json!({"expected_state_revision": 0, "source": {"kind": "local-path", "path": path, "subdir": "x"}}),
        json!({"expected_state_revision": 0, "source": {"kind": "git", "url": "https://example.com/r.git", "revision": "a", "subdir": "x"}}),
        json!({"expected_state_revision": 0, "source": {"kind": "tarball", "path": path}}),
        json!({"source": {"kind": "local-path", "path": path}}),
        json!("not an object"),
    ] {
        let (status, response) = post_op(&app, "/v1/extensions/install", body.clone()).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {response}");
        assert_precommit(&response, "invalid_request", 0);
    }
    let (status, response) = post_op(
        &app,
        &format!("/v1/extensions/{ID}/enable"),
        json!({"expected_state_revision": 0, "scope": {"kind": "project"}}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");

    // Git is A4: refused with the §13.2 fail-closed code before any permit,
    // quarantine, DNS, or process.
    let (status, response) = post_op(
        &app,
        "/v1/extensions/install",
        json!({"expected_state_revision": 0, "source": {"kind": "git", "url": "https://example.com/repo.git", "revision": "0123456789abcdef0123456789abcdef01234567"}}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_precommit(&response, "git_connection_pinning_unavailable", 0);
    assert!(!fixture.root().exists());
    let bootstrap_residue = fs::read_dir(fixture.config.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".extensions-bootstrap-")
        });
    assert!(
        !bootstrap_residue,
        "git refusal must not begin an acquisition"
    );

    // A relative path is refused by the writer's own source validation.
    let (status, response) = install_local(&app, 0, "relative/path").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_precommit(&response, "invalid_source", 0);
    fixture.assert_no_canary_ran();
}

// ---------------------------------------------------------------------------
// Committed 202, retry, and reinspection.
// ---------------------------------------------------------------------------

/// With no supervisor able to confirm reconciliation the commit still stands:
/// HTTP 202 carries the committed revision, a retry of the identical request
/// is a pre-commit `state_revision_conflict` (never a second commit), and
/// reinspection by the returned revision shows the committed state.
#[tokio::test]
async fn committed_202_is_authoritative_and_a_retry_conflicts_instead_of_recommitting() {
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let fixture = Fixture::new();
    let app = router(fake_convene_state(&fixture.config));
    let path = fixture.package("noop", ID, "1.0.0");

    let (status, installed) = install_local(&app, 0, &path).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{installed}");
    assert_committed(&installed, 1);
    assert_eq!(installed["mutation"]["id"], ID);
    assert_eq!(installed["mutation"]["effective"], false);
    assert_eq!(installed["mutation"]["reconciliation"], "blocked");
    assert_eq!(installed["mutation"]["reap"], "not_required");
    let digest = installed["mutation"]["digest"].as_str().unwrap().to_owned();

    // Retrying the same request is refused on revision, not recommitted.
    let (status, retry) = install_local(&app, 0, &path).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_precommit(&retry, "state_revision_conflict", 1);
    // Retrying at the returned revision is refused on state.
    let (status, retry) = install_local(&app, 1, &path).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_precommit(&retry, "already_installed", 1);
    assert!(fixture.entries("quarantine").is_empty());

    // Reinspection answers from the committed revision.
    let (status, inspected) = get_json(&app, &format!("/v1/extensions/{ID}/inspect")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(inspected["extension"]["state_revision"], 1);
    assert_eq!(inspected["extension"]["digest"], digest.as_str());
    assert_eq!(inspected["extension"]["trusted"], false);
    assert_eq!(inspected["runtime"], json!([]));
    let (status, listed) = get_json(&app, "/v1/extensions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["state_revision"], 1);
    assert_eq!(listed["extensions"][0]["extension"]["id"], ID);
    assert_eq!(listed["extensions"][0]["runtime"], json!([]));

    // A stale trust preview fails on revision and mutates nothing.
    let (status, stale) = post_op(
        &app,
        &format!("/v1/extensions/{ID}/trust"),
        json!({"expected_state_revision": 0, "digest": digest, "confirm_grant_diff": null}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_precommit(&stale, "state_revision_conflict", 1);
    // A wrong confirmation fails before any write.
    let (status, wrong) = post_op(
        &app,
        &format!("/v1/extensions/{ID}/trust"),
        json!({
            "expected_state_revision": 1,
            "digest": digest,
            "service_grants": [{"service_id": "lifecycle", "native_process_ack": true, "secret_bindings": []}],
            "confirm_grant_diff": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_precommit(&wrong, "grant_confirmation_mismatch", 1);
    // Enable without trust is refused.
    let (status, untrusted) = scope(&app, ID, "enable", 1).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_precommit(&untrusted, "trust_required", 1);
    assert_eq!(fixture.revision(), 1);
    fixture.assert_no_canary_ran();
}

/// Readers exceeding their lock bound while a mutation holds `.state.lock`
/// exclusively are told to retry, and a mutation that cannot take the lock is
/// a retryable pre-commit refusal that writes nothing.
#[tokio::test]
async fn busy_registry_is_retryable_for_readers_and_writers() {
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let fixture = Fixture::new();
    let app = router(fake_convene_state(&fixture.config));
    let path = fixture.package("noop", ID, "1.0.0");
    let (_, installed) = install_local(&app, 0, &path).await;
    assert_committed(&installed, 1);

    let lock = fs::File::open(fixture.root().join(".state.lock")).unwrap();
    fs2::FileExt::try_lock_exclusive(&lock).unwrap();
    for uri in [
        "/v1/extensions".to_owned(),
        format!("/v1/extensions/{ID}/inspect"),
    ] {
        let (status, body) = get_json(&app, &uri).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}: {body}");
        assert_eq!(body["error"], "extension_state_busy");
        assert_eq!(body["retryable"], true);
    }
    let (status, body) = scope(&app, ID, "disable", 1).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["mutation"]["committed"], false);
    assert_eq!(body["error"]["code"], "extension_state_busy");
    assert_eq!(body["error"]["retryable"], true);
    drop(lock);
    assert_eq!(fixture.revision(), 1);
    let (status, body) = scope(&app, ID, "disable", 1).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_committed(&body, 2);
}

// ---------------------------------------------------------------------------
// Live supervisor: activation, revision serialization, active-service
// refusal, disable-reaps-before-200, and remove.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_lifecycle_reconciles_supervisor_and_disable_reaps_before_200() {
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let fixture = Fixture::new();
    let mut state = fake_convene_state(&fixture.config);
    let supervisor = with_supervisor(&mut state, fixture.config.path()).await;
    let app = router(state);

    // 1. Install: nothing trusted, nothing started.
    let (status, installed) = install_local(&app, 0, &fixture.package("noop", ID, "1.0.0")).await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    assert_committed(&installed, 1);
    assert_eq!(installed["mutation"]["reconciliation"], "complete");
    assert_eq!(installed["mutation"]["reap"], "not_required");
    let digest = installed["mutation"]["digest"].as_str().unwrap().to_owned();

    // 2. Trust (preview, then exact apply): still not enabled or running.
    let (status, trusted) = trust_noop(&app, ID, 1, &digest).await;
    assert_eq!(status, StatusCode::OK, "{trusted}");
    assert_committed(&trusted, 2);
    assert_eq!(trusted["mutation"]["effective"], false);
    let (_, idle) = get_json(&app, &format!("/v1/extensions/{ID}/status")).await;
    assert_eq!(idle["services"], json!([]));

    // 3. Enable: reconciliation completes and one supervised service runs.
    let (status, enabled) = scope(&app, ID, "enable", 2).await;
    assert_eq!(status, StatusCode::OK, "{enabled}");
    assert_committed(&enabled, 3);
    assert_eq!(enabled["mutation"]["effective"], true);
    assert_eq!(enabled["mutation"]["reconciliation"], "complete");
    assert_eq!(enabled["mutation"]["reap"], "not_required");
    let healthy = wait_for_status(&app, ID, |body| {
        body["services"][0]["state"] == "healthy" && body["services"][0]["pid"].is_u64()
    })
    .await;
    let pid = healthy["services"][0]["pid"].as_i64().unwrap();
    let epoch = healthy["services"][0]["activation_epoch"].clone();
    assert_eq!(healthy["services"][0]["activation_revision"], 3);
    assert!(process_alive(pid));
    let (_, inspected) = get_json(&app, &format!("/v1/extensions/{ID}/inspect")).await;
    assert_eq!(inspected["runtime"][0]["pid"], pid);

    // 4. An unrelated committed mutation is reconciled in revision order
    //    without restarting this service: same pid and epoch, newer revision.
    let (status, other) = install_local(&app, 3, &fixture.package("other", OTHER, "1.0.0")).await;
    assert_eq!(status, StatusCode::OK, "{other}");
    assert_committed(&other, 4);
    let (_, after) = get_json(&app, &format!("/v1/extensions/{ID}/status")).await;
    assert_eq!(after["services"][0]["pid"], pid);
    assert_eq!(after["services"][0]["activation_epoch"], epoch);
    assert_eq!(after["services"][0]["activation_revision"], 4);

    // 5. Active update/remove refuse before any write and never detach it.
    let (status, active) = post_op(
        &app,
        &format!("/v1/extensions/{ID}/update"),
        json!({"expected_state_revision": 4, "source": {"kind": "local-path", "path": fixture.package("noop-v2", ID, "2.0.0")}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{active}");
    assert_precommit(&active, "extension_active", 4);
    let (status, active) = send(
        &app,
        Method::DELETE,
        &format!("/v1/extensions/{ID}"),
        Some(json!({"expected_state_revision": 4, "purge_state": false})),
        Auth::Operator,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{active}");
    assert_precommit(&active, "extension_active", 4);
    assert!(process_alive(pid));
    assert_eq!(fixture.revision(), 4);
    assert!(fixture.entries("quarantine").is_empty());

    // 6. Disable: the filter is removed, the service is shut down and its
    //    group reaped BEFORE the 200 is returned.
    let (status, disabled) = scope(&app, ID, "disable", 4).await;
    assert_eq!(status, StatusCode::OK, "{disabled}");
    assert_committed(&disabled, 5);
    assert_eq!(disabled["mutation"]["effective"], false);
    assert_eq!(disabled["mutation"]["reconciliation"], "complete");
    assert_eq!(disabled["mutation"]["reap"], "complete");
    assert!(!process_alive(pid), "disable returned 200 before reap");
    assert!(supervisor.activity().package_stopped(ID));
    let (_, stopped) = get_json(&app, &format!("/v1/extensions/{ID}/status")).await;
    assert_eq!(stopped["services"], json!([]));

    // 7. Disabled and reaped: update installs an untrusted digest and starts
    //    nothing; remove then revokes everything.
    let (status, updated) = post_op(
        &app,
        &format!("/v1/extensions/{ID}/update"),
        json!({"expected_state_revision": 5, "source": {"kind": "local-path", "path": fixture.package("noop-v3", ID, "3.0.0")}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_committed(&updated, 6);
    assert_ne!(updated["mutation"]["digest"], digest.as_str());
    assert_eq!(updated["mutation"]["effective"], false);
    let (_, inspected) = get_json(&app, &format!("/v1/extensions/{ID}/inspect")).await;
    assert_eq!(inspected["extension"]["trusted"], false);
    let (status, removed) = send(
        &app,
        Method::DELETE,
        &format!("/v1/extensions/{ID}"),
        Some(json!({"expected_state_revision": 6, "purge_state": true})),
        Auth::Operator,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert_committed(&removed, 7);
    assert_eq!(removed["mutation"]["digest"], Value::Null);
    assert_eq!(removed["mutation"]["reap"], "not_required");
    let (status, _) = get_json(&app, &format!("/v1/extensions/{ID}/inspect")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, listed) = get_json(&app, "/v1/extensions").await;
    assert_eq!(listed["state_revision"], 7);
    assert_eq!(listed["extensions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["extensions"][0]["extension"]["id"], OTHER);

    supervisor.shutdown().await;
    fixture.assert_no_canary_ran();
}

// ---------------------------------------------------------------------------
// Startup recovery wiring.
// ---------------------------------------------------------------------------

/// A mutation interrupted after the irreversible commit point is rolled
/// forward by the startup hook, and one interrupted before it is rolled back,
/// so the first reader after boot sees one coherent generation.
#[tokio::test]
async fn startup_recovery_rolls_journals_forward_and_back_before_readers() {
    let fixture = Fixture::new();
    let writer = RegistryWriter::new(fixture.config.path().to_path_buf());
    let path = fixture.package("noop", ID, "1.0.0");
    writer
        .install(0, writer.acquire_local(&path).unwrap())
        .unwrap();
    assert_eq!(fixture.revision(), 1);
    let registered = HashSet::new();

    // Past the commit point: a mixed generation until recovery.
    writer.crash_at(Some(CrashPoint::AfterStateRename(0)));
    assert!(writer
        .disable(
            ID,
            1,
            super::super::transaction::EnablementScope::Global,
            &registered
        )
        .is_err());
    writer.crash_at(None);
    assert!(read_locked_state(fixture.config.path()).is_err());
    super::super::recover_at_startup(fixture.config.path().to_path_buf()).await;
    assert_eq!(fixture.revision(), 2);
    assert!(fixture.entries("transactions").is_empty());

    // Before the commit point: rolled back to the old revision.
    writer.crash_at(Some(CrashPoint::AfterJournal));
    assert!(writer
        .disable(
            ID,
            2,
            super::super::transaction::EnablementScope::Global,
            &registered
        )
        .is_err());
    writer.crash_at(None);
    assert_eq!(fixture.entries("transactions").len(), 1);
    super::super::recover_at_startup(fixture.config.path().to_path_buf()).await;
    assert_eq!(fixture.revision(), 2);
    assert!(fixture.entries("transactions").is_empty());
    assert!(fixture.entries("staging").is_empty());
    fixture.assert_no_canary_ran();
}

/// The daemon's startup sequence recovers the registry before it creates the
/// supervisor or serves a route.
#[test]
fn daemon_startup_recovers_before_supervisor_and_router() {
    let source = include_str!("../../main.rs");
    let body = &source[source.find("#[tokio::main]").expect("daemon main")..];
    let recover = body
        .find("extension_registry::recover_at_startup(")
        .expect("startup recovery is wired");
    let supervisor = body
        .find("extension_service::ExtensionSupervisor::new_with_lifecycle(")
        .expect("supervisor is created at startup");
    let start = body
        .find(".start(config_dir.clone(), registered_extension_projects)")
        .expect("supervisor start");
    let serve = body.find("axum::serve(").expect("router is served");
    assert!(recover < supervisor && supervisor < start && start < serve);
}
