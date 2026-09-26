//! Stage A5 closures for §19.4 reader rows the A0/A2a/A3a suites left
//! partial: service-grant order, trust, service, and binding authority read
//! through the coherent reader; oversized state and manifest files; and a
//! hardlinked file inside an immutable payload.

use super::*;

fn with_grants(fixture: &Fixture, rows: Value) -> Result<LockedState, StateError> {
    let root = fixture.config.path().join("extensions");
    write_json(
        &root.join("service-grants.json"),
        &json!({"schema_version": 1, "state_revision": 7, "service_grants": rows}),
    );
    read_locked_state(fixture.config.path())
}

fn row(fixture: &Fixture, service: &str, bindings: Value) -> Value {
    json!({
        "id": ID,
        "digest": fixture.digest,
        "service_id": service,
        "native_process_ack": true,
        "secret_bindings": bindings
    })
}

#[test]
fn a5_service_grant_reader_rejects_order_trust_service_and_binding_authority() {
    let fixture = fixture(true, true);
    // Control: the one exact row reads.
    assert!(with_grants(&fixture, json!([row(&fixture, "bridge", json!([]))])).is_ok());
    // Distinct rows out of ascending (id, digest, service_id) order.
    assert_eq!(
        with_grants(
            &fixture,
            json!([
                row(&fixture, "zeta", json!([])),
                row(&fixture, "bridge", json!([]))
            ])
        )
        .err(),
        Some(StateError::InvalidRecord("service-grant-order"))
    );
    // A service the artifact does not declare.
    assert_eq!(
        with_grants(&fixture, json!([row(&fixture, "ghost", json!([]))])).err(),
        Some(StateError::InvalidRecord("service-grant-service"))
    );
    // A binding whose reference the manifest never requested and trust never
    // granted.
    assert_eq!(
        with_grants(
            &fixture,
            json!([row(
                &fixture,
                "bridge",
                json!([{"target_env": "EXAMPLE_ENV", "reference": "env:OCEAN_UNGRANTED"}])
            )])
        )
        .err(),
        Some(StateError::InvalidRecord("secret-binding-authority"))
    );
    // Two targets sharing one reference, and a reserved host name.
    for bindings in [
        json!([
            {"target_env": "ALPHA", "reference": "env:SHARED"},
            {"target_env": "BRAVO", "reference": "env:SHARED"}
        ]),
        json!([{"target_env": "OCEAN_EXTENSION_ID", "reference": "env:SOURCE"}]),
        json!([{"target_env": "TMPDIR", "reference": "env:SOURCE"}]),
    ] {
        assert_eq!(
            with_grants(&fixture, json!([row(&fixture, "bridge", bindings.clone())])).err(),
            Some(StateError::InvalidRecord("secret-binding")),
            "{bindings}"
        );
    }

    // A row with no matching exact-digest trust grant.
    let untrusted = super::fixture(false, true);
    assert_eq!(
        with_grants(&untrusted, json!([row(&untrusted, "bridge", json!([]))])).err(),
        Some(StateError::InvalidRecord("service-grant-trust"))
    );
    assert!(!fixture.marker.exists() && !untrusted.marker.exists());
}

#[test]
fn a5_oversized_state_file_and_manifest_fail_closed() {
    let fixture = fixture(true, true);
    let root = fixture.config.path().join("extensions");
    let mut trust: Value =
        serde_json::from_slice(&fs::read(root.join("trust.json")).unwrap()).unwrap();
    // Valid JSON, just past the 1 MiB state-file cap.
    trust["padding"] = json!("x".repeat(STATE_FILE_LIMIT as usize));
    fs::write(root.join("trust.json"), serde_json::to_vec(&trust).unwrap()).unwrap();
    assert_eq!(
        read_locked_state(fixture.config.path()).err(),
        Some(StateError::Oversized("trust.json"))
    );

    let package = tempfile::tempdir().unwrap();
    let manifest = format!(
        "{}# {}\n",
        package_manifest(),
        "x".repeat(MANIFEST_FILE_LIMIT as usize)
    );
    fs::write(package.path().join("ocean-extension.toml"), manifest).unwrap();
    let directory = File::open(package.path()).unwrap();
    assert_eq!(
        snapshot_package(&directory).err(),
        Some(StateError::Oversized("ocean-extension.toml"))
    );
}

#[cfg(unix)]
#[test]
fn a5_hardlinked_payload_file_invalidates_the_artifact() {
    let fixture = fixture(true, true);
    let payload = fixture
        .config
        .path()
        .join("extensions/store")
        .join(ID)
        .join(digest_hex(&fixture.digest).unwrap());
    // Control: the untouched payload verifies.
    assert!(snapshot_package(&File::open(&payload).unwrap()).is_ok());
    let outside = fixture.config.path().join("outside-link");
    fs::hard_link(payload.join("run-me"), &outside).unwrap();
    assert_eq!(
        snapshot_package(&File::open(&payload).unwrap()).err(),
        Some(StateError::InvalidComponent("extension payload"))
    );
    assert!(!fixture.marker.exists());
}
