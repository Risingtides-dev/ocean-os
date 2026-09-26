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
