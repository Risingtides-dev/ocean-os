//! Stage A5 closures for §19.4 writer rows the A3a suite left partial:
//! expected-revision on enable/update/remove, and the `filesystem`
//! declaration refused the same way as `network`.

use super::*;

#[test]
fn a5_enable_update_and_remove_check_expected_revision_before_any_write() {
    let h = Harness::new();
    let digest = h.install(0, &h.noop("v1", "1.0.0")).digest.unwrap();
    h.trust(1, &digest);
    assert_eq!(h.revision(), 2);
    let before = h.bytes();
    let projects = HashSet::new();
    assert_eq!(
        code(h.writer.enable(ID, 1, EnablementScope::Global, &projects)),
        REVISION_CONFLICT
    );
    let replacement = h.acquire(&h.noop("v2", "2.0.0"));
    assert_eq!(
        code(h.writer.update(ID, 1, replacement, &Stopped)),
        REVISION_CONFLICT
    );
    assert_eq!(
        code(h.writer.remove(ID, 1, false, &Stopped)),
        REVISION_CONFLICT
    );
    assert_eq!(
        code(h.writer.remove(ID, 3, true, &Stopped)),
        REVISION_CONFLICT
    );
    assert_eq!(h.bytes(), before, "a stale mutation wrote state");
    h.assert_no_transaction_residue();
    // Control: the current revision is accepted.
    h.writer
        .enable(ID, 2, EnablementScope::Global, &projects)
        .unwrap();
    h.assert_no_code_ran();
}

#[test]
fn a5_filesystem_declarations_are_refused_like_network() {
    let h = Harness::new();
    let source = h.package(
        "filesystem",
        "1.0.0",
        "0.1.0",
        "filesystem = [\"project-root\"]\nenv = [\"NOOP_TOKEN\"]\n",
    );
    let digest = h.install(0, &source).digest.unwrap();
    let mut request = h.trust_request(1, &digest);
    request.service_grants[0].secret_bindings.clear();
    request.capabilities = CapabilitySet {
        filesystem: vec!["project-root".into()],
        ..CapabilitySet::default()
    };
    assert_eq!(
        code(h.preview(request.clone())),
        "declaration_policy_rejected"
    );
    request.capabilities = CapabilitySet {
        env: vec!["NOOP_TOKEN".into()],
        ..CapabilitySet::default()
    };
    h.apply(request).unwrap();
    assert_eq!(
        code(
            h.writer
                .enable(ID, 2, EnablementScope::Global, &HashSet::new())
        ),
        "declaration_policy_rejected"
    );
    h.assert_no_code_ran();
}

// ---------------------------------------------------------------------------
// Stage A open items (evidence record §6), closed after A5 merged.
// ---------------------------------------------------------------------------

/// O-7 (§12.3, §19.4): with all four acquisition permits held open at once,
/// `.state.lock` is still free, `list`, `inspect`, and `doctor` answer over
/// HTTP from coherent reads, a mutation commits, the reads then see its
/// revision, and a fifth acquisition is refused rather than queued. Every held
/// quarantine survives all of it and is released only when its lease drops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a5_open_four_held_acquisitions_leave_the_lock_and_every_read_free() {
    use crate::tests::{fake_convene_state, TestEnvRestore, AUTO_CONVENE_ENV_LOCK};
    use axum::http::StatusCode;
    use axum::routing::get;

    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let h = Harness::new();
    h.install(0, &h.noop("v1", "1.0.0"));
    let registry = super::super::super::list;
    let app = axum::Router::new()
        .route("/v1/extensions", get(registry))
        .route(
            "/v1/extensions/{id}/inspect",
            get(super::super::super::inspect),
        )
        .route(
            "/v1/extensions/{id}/doctor",
            get(super::super::super::doctor),
        )
        .with_state(fake_convene_state(&h.config));

    // Four fetches held open at once, each on its own thread, as a real
    // acquisition would be: each keeps its permit and quarantine until
    // released.
    let mut releases = Vec::new();
    let mut fetches = Vec::new();
    let mut held = Vec::new();
    for _ in 0..4 {
        let (release, wait) = std::sync::mpsc::channel::<()>();
        let (started, ready) = std::sync::mpsc::channel::<Uuid>();
        let config = h.config.path().to_path_buf();
        fetches.push(std::thread::spawn(move || {
            let writer = RegistryWriter::new(config);
            let lease = writer.begin_acquisition().unwrap();
            started.send(lease.operation_id()).unwrap();
            wait.recv_timeout(Duration::from_secs(60)).unwrap();
            drop(lease);
        }));
        held.push(ready.recv_timeout(Duration::from_secs(10)).unwrap());
        releases.push(release);
    }
    let quarantines: Vec<PathBuf> = held
        .iter()
        .map(|id| h.root().join("quarantine").join(id.to_string()))
        .collect();
    assert!(quarantines.iter().all(|dir| dir.is_dir()));
    assert_eq!(h.entries("quarantine").len(), 4);
    // The cap is shared across writer instances: a fifth is refused.
    assert_eq!(
        h.writer.begin_acquisition().err().map(|error| error.code),
        Some("acquisition_capacity")
    );

    let reads = |revision: u64| {
        let app = app.clone();
        let state_lock = h.root().join(".state.lock");
        async move {
            // Nobody holds `.state.lock`: it can be taken exclusively.
            let lock = File::open(state_lock).unwrap();
            fs2::FileExt::try_lock_exclusive(&lock).unwrap();
            drop(lock);
            let (status, body) = route_json(app.clone(), "/v1/extensions").await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["state_revision"], revision);
            assert_eq!(body["extensions"].as_array().unwrap().len(), 1);
            let (status, body) =
                route_json(app.clone(), &format!("/v1/extensions/{ID}/inspect")).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["extension"]["state_revision"], revision);
            let (status, body) =
                route_json(app.clone(), &format!("/v1/extensions/{ID}/doctor")).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["checks"]["coherent_state"], true);
            assert_eq!(body["checks"]["package_code_executed"], false);
        }
    };
    reads(1).await;
    // A mutation commits while all four fetches are open.
    h.writer
        .disable(ID, 1, EnablementScope::Global, &HashSet::new())
        .unwrap();
    reads(2).await;
    assert!(
        quarantines.iter().all(|dir| dir.is_dir()),
        "a held fetch lost its quarantine"
    );

    for release in releases {
        release.send(()).unwrap();
    }
    for fetch in fetches {
        fetch.join().unwrap();
    }
    assert!(h.entries("quarantine").is_empty());
    assert!(h.writer.begin_acquisition().is_ok());
    h.assert_no_code_ran();
}

/// O-8 (§19.4 "diffs are stable"): for every diff shape, a repeated preview
/// is identical, a request that differs only in list order previews
/// identically, and the preview's confirmation applies. Shapes: native-ack
/// only (a service row with no binding), widening an existing grant, a
/// binding-only change, and narrowing.
#[test]
fn a5_open_every_grant_diff_shape_previews_stably() {
    let h = Harness::new();
    let package = h.package(
        "wide",
        "1.0.0",
        "0.1.0",
        "env = [\"A_TOKEN\", \"B_TOKEN\", \"C_TOKEN\"]\nsecrets = [\"env:OCEAN_A\", \"env:OCEAN_B\", \"env:OCEAN_C\"]\n",
    );
    let digest = h.install(0, &package).digest.unwrap();
    let request = |expected: u64,
                   env: &[&str],
                   secrets: &[&str],
                   rows: Option<&[(&str, &str)]>|
     -> TrustRequest {
        TrustRequest {
            expected_state_revision: expected,
            digest: digest.clone(),
            capabilities: CapabilitySet {
                env: env.iter().map(|value| (*value).to_owned()).collect(),
                secrets: secrets.iter().map(|value| (*value).to_owned()).collect(),
                ..CapabilitySet::default()
            },
            service_grants: rows
                .map(|bindings| {
                    vec![ServiceGrantRequest {
                        service_id: "lifecycle".into(),
                        native_process_ack: true,
                        secret_bindings: bindings
                            .iter()
                            .map(|(target, reference)| SecretBinding {
                                target_env: (*target).to_owned(),
                                reference: (*reference).to_owned(),
                            })
                            .collect(),
                    }]
                })
                .unwrap_or_default(),
            confirm_grant_diff: None,
        }
    };
    // Preview the request twice, and its permuted twin once: all three are
    // identical. Then apply with that confirmation.
    let stable = |shape: &str, request: TrustRequest, permuted: TrustRequest| -> GrantPreview {
        let first = h.preview(request.clone()).unwrap();
        assert_eq!(
            first,
            h.preview(request.clone()).unwrap(),
            "{shape}: repeat"
        );
        assert_eq!(first, h.preview(permuted).unwrap(), "{shape}: order");
        let revision = h.revision();
        let mut apply = request;
        apply.confirm_grant_diff = Some(first.confirmation.clone());
        assert!(
            matches!(h.writer.trust(ID, apply), Ok(TrustResult::Applied(_))),
            "{shape}: the stable confirmation did not apply"
        );
        assert_eq!(h.revision(), revision + 1, "{shape}");
        first
    };

    // Each shape is previewed with its lists in one order and in reverse.
    let ab = ["A_TOKEN", "B_TOKEN"];
    let ba = ["B_TOKEN", "A_TOKEN"];
    let abc = ["A_TOKEN", "B_TOKEN", "C_TOKEN"];
    let cba = ["C_TOKEN", "B_TOKEN", "A_TOKEN"];
    let secrets_abc = ["env:OCEAN_A", "env:OCEAN_B", "env:OCEAN_C"];
    let secrets_cba = ["env:OCEAN_C", "env:OCEAN_B", "env:OCEAN_A"];
    let secrets_ab = ["env:OCEAN_A", "env:OCEAN_B"];
    let secrets_ba = ["env:OCEAN_B", "env:OCEAN_A"];
    let bind_abc = [
        ("A_TOKEN", "env:OCEAN_A"),
        ("B_TOKEN", "env:OCEAN_B"),
        ("C_TOKEN", "env:OCEAN_C"),
    ];
    let bind_cba = [
        ("C_TOKEN", "env:OCEAN_C"),
        ("B_TOKEN", "env:OCEAN_B"),
        ("A_TOKEN", "env:OCEAN_A"),
    ];
    let bind_ab = [("A_TOKEN", "env:OCEAN_A"), ("B_TOKEN", "env:OCEAN_B")];
    let bind_ba = [("B_TOKEN", "env:OCEAN_B"), ("A_TOKEN", "env:OCEAN_A")];

    // Baseline: capabilities only, no service row yet.
    stable(
        "baseline",
        request(1, &ab, &[], None),
        request(1, &ba, &[], None),
    );
    // Native-ack only: the one change is a service row with no binding.
    let preview = stable(
        "native-ack only",
        request(2, &ab, &[], Some(&[][..])),
        request(2, &ba, &[], Some(&[][..])),
    );
    assert_eq!(preview.added.capabilities, CapabilitySet::default());
    assert_eq!(preview.added.service_grants.len(), 1);
    assert!(preview.added.service_grants[0].secret_bindings.is_empty());
    assert_eq!(preview.removed, GrantSide::default());
    // Widening an existing grant: one more env and all three secrets.
    let preview = stable(
        "widening",
        request(3, &abc, &secrets_abc, Some(&[][..])),
        request(3, &cba, &secrets_cba, Some(&[][..])),
    );
    assert_eq!(preview.added.capabilities.env, ["C_TOKEN"]);
    assert_eq!(preview.added.capabilities.secrets, secrets_abc);
    assert!(preview.added.service_grants.is_empty());
    assert_eq!(preview.removed, GrantSide::default());
    // Binding only: capabilities unchanged, three bindings added.
    let preview = stable(
        "binding only",
        request(4, &abc, &secrets_abc, Some(&bind_abc[..])),
        request(4, &cba, &secrets_cba, Some(&bind_cba[..])),
    );
    assert_eq!(preview.added.capabilities, CapabilitySet::default());
    assert_eq!(preview.removed.capabilities, CapabilitySet::default());
    assert_eq!(preview.added.service_grants[0].secret_bindings.len(), 3);
    assert!(preview.removed.service_grants[0].secret_bindings.is_empty());
    // Narrowing: drop C's env, secret, and binding.
    let preview = stable(
        "narrowing",
        request(5, &ab, &secrets_ab, Some(&bind_ab[..])),
        request(5, &ba, &secrets_ba, Some(&bind_ba[..])),
    );
    assert_eq!(preview.removed.capabilities.env, ["C_TOKEN"]);
    assert_eq!(preview.removed.capabilities.secrets, ["env:OCEAN_C"]);
    assert_eq!(preview.added.capabilities, CapabilitySet::default());
    let state = h.state().unwrap();
    let bindings: Vec<&str> = state.snapshot.service_grants[0]
        .secret_bindings
        .iter()
        .map(|binding| binding.target_env.as_str())
        .collect();
    assert_eq!(
        bindings,
        ["A_TOKEN", "B_TOKEN"],
        "the narrowed binding survived"
    );
    assert_eq!(state.snapshot.grants[0].capabilities.env, ab);
    h.assert_no_code_ran();
}

/// Every regular file under `root` whose bytes contain `needle`.
fn files_containing(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.map(Result::unwrap) {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && String::from_utf8_lossy(&fs::read(&path).unwrap()).contains(needle)
            {
                found.push(path);
            }
        }
    }
    found
}

/// O-5: a secret-bound grant leaves no secret value in any file the writer
/// has on disk at any crash point, journals included. The bound source
/// variable holds a sentinel for the whole run. Every scenario of the crash
/// matrix (each of which but `Install` carries the secret binding, as a
/// trust, as prior state, or both) is stopped at each of the eleven journal
/// steps and the complete config tree is scanned before recovery removes
/// anything. Positive control: a trust stopped right after its journal is
/// written has a file outside the published state and the store that names
/// the binding's reference, so the scan does read the transaction's own files.
#[test]
fn a5_open_no_crash_point_leaves_a_secret_value_on_disk() {
    use crate::tests::{TestEnvRestore, AUTO_CONVENE_ENV_LOCK};

    let _guard = AUTO_CONVENE_ENV_LOCK.blocking_lock();
    let _restore = TestEnvRestore::capture(&["OCEAN_NOOP_TOKEN"]);
    let sentinel = format!("a5-journal-secret-{}", Uuid::new_v4());
    std::env::set_var("OCEAN_NOOP_TOKEN", &sentinel);
    let mut transaction_files_named_the_reference = false;
    for scenario in [
        Scenario::Install,
        Scenario::Update,
        Scenario::Trust,
        Scenario::Enable,
        Scenario::Disable,
        Scenario::Remove,
    ] {
        for point in CRASH_POINTS {
            let context = format!("{scenario:?} at {point:?}");
            let mut prepared = prepare(scenario);
            let old = prepared.h.revision();
            prepared.h.writer.crash_at(Some(point));
            assert_eq!(
                code(perform(&mut prepared, scenario, old)),
                "simulated_crash",
                "{context}"
            );
            prepared.h.writer.crash_at(None);
            let h = &prepared.h;
            assert!(
                files_containing(h.config.path(), &sentinel).is_empty(),
                "{context}: a secret value is on disk"
            );
            if scenario == Scenario::Trust && point == CrashPoint::AfterJournal {
                let root = h.root();
                transaction_files_named_the_reference =
                    files_containing(h.config.path(), "env:OCEAN_NOOP_TOKEN")
                        .iter()
                        .any(|path| {
                            !path.starts_with(root.join("store"))
                                && !STATE_FILES
                                    .iter()
                                    .chain([&PUBLICATION_MARKER])
                                    .any(|name| *path == root.join(name))
                        });
            }
            h.writer.recover().unwrap();
            assert!(
                files_containing(h.config.path(), &sentinel).is_empty(),
                "{context}: a secret value is on disk after recovery"
            );
        }
    }
    assert!(
        transaction_files_named_the_reference,
        "the scan never saw a journaled binding"
    );
}
