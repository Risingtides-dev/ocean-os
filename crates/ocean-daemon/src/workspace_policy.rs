/// Outcome of binding a turn's requested cwd against the session it claims to
/// resume. See [`resolve_bound_cwd`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CwdBindingError {
    /// The caller's declared workspace does not match the session's bound
    /// workspace on a read-scoped request. The read path keeps the existing
    /// workspace boundary intact even when the turn path is allowed to rebind.
    WorkspaceMismatch {
        requested_workspace: String,
        session_workspace: String,
    },
    /// The requested cwd contains a parent-dir (`..`) traversal component, so it
    /// could escape its intended workspace into an arbitrary filesystem location
    /// (OCEAN-52b). Legit cwds are already-resolved absolute paths.
    PathTraversal { cwd: String },
}

impl CwdBindingError {
    pub(super) fn message(&self) -> String {
        match self {
            CwdBindingError::WorkspaceMismatch {
                requested_workspace,
                session_workspace,
            } => format!(
                "session/workspace mismatch: this session is bound to workspace \
                 {session_workspace}, but the request resolves to {requested_workspace}."
            ),
            CwdBindingError::PathTraversal { cwd } => format!(
                "rejected cwd {cwd}: a working directory must be an absolute, \
                 already-resolved path with no parent-directory ('..') components."
            ),
        }
    }
}

/// True if `cwd` contains a parent-directory (`..`) component, which could let a
/// forged path escape its intended workspace boundary. We check lexically (not
/// via `canonicalize`) so the guard is deterministic and does not depend on the
/// path existing on disk — a resolved turn cwd should never contain `..`.
fn cwd_has_traversal(cwd: &str) -> bool {
    std::path::Path::new(cwd)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// Resolve the working directory a turn will actually execute in. The caller's
/// cwd always wins; the only guard here is path traversal.
///
/// - `requested_cwd`: the cwd already resolved by `resolve_cwd_for_turn`
///   (non-empty: the client's cwd, or a project's workspace_root).
/// - `requested_workspace_root`: kept for call-site compatibility; the resolver
///   no longer compares against it.
/// - `session_binding`: kept for call-site compatibility; the resolver no longer
///   compares against it.
///
/// Returns the cwd to run in. A resumed turn always uses the caller's cwd; the
/// session binding is refreshed separately when the turn is saved.
pub(super) fn resolve_bound_cwd(
    requested_cwd: &str,
    _requested_workspace_root: &str,
    _session_binding: Option<(&str, &str)>,
) -> Result<String, CwdBindingError> {
    // Path-traversal guard applies to every turn: the resolved cwd must not
    // contain `..` components that could escape into a parent / arbitrary dir.
    if cwd_has_traversal(requested_cwd) {
        return Err(CwdBindingError::PathTraversal {
            cwd: requested_cwd.to_string(),
        });
    }

    Ok(requested_cwd.to_string())
}

/// Workspace-scoping guard for the session-DETAIL read path (`GET
/// /v1/agent/sessions/{id}`), mirroring the turn path's session↔workspace
/// binding (OCEAN-52) so a caller cannot read another workspace's session by id
/// alone (OCEAN-74).
///
/// - `requested_workspace`: the workspace the caller declared via `?cwd=` /
///   `?workspace=`, already resolved to a workspace root. `None` = the caller
///   declared no scope.
/// - `session_workspace`: the session's bound workspace root. `None` = a legacy
///   session with no recorded workspace.
///
/// A cross-workspace read is rejected ONLY when BOTH are present and differ.
/// When either is absent the read is allowed (an unscoped caller, or a legacy
/// session with no boundary to enforce), preserving backward-compatible reads.
pub(super) fn session_detail_scope_check(
    requested_workspace: Option<&str>,
    session_workspace: Option<&str>,
) -> Result<(), CwdBindingError> {
    match (requested_workspace, session_workspace) {
        (Some(requested), Some(bound)) if requested != bound => {
            Err(CwdBindingError::WorkspaceMismatch {
                requested_workspace: requested.to_string(),
                session_workspace: bound.to_string(),
            })
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_runs_in_requested_cwd() {
        // A brand-new session (no prior binding) legitimately sets its own cwd.
        let out = resolve_bound_cwd("/work/repo/sub", "/work/repo", None)
            .expect("new session cwd should be accepted");
        assert_eq!(
            out, "/work/repo/sub",
            "a new session runs in exactly the requested cwd"
        );
    }

    #[test]
    fn resumed_turn_in_same_workspace_rebinds_to_requested_cwd() {
        // Behavior changed in 18ba9a9 ("fix(runtime): bind tools to
        // SessionContext cwd"): cwd binding moved into SessionContext, and a
        // resumed turn now REBINDS to the requested cwd rather than being pinned
        // to the session's original sub-dir ("resumed sessions rebind workspace
        // metadata when the caller crosses projects"). `resolve_bound_cwd` is now
        // just the traversal guard + pass-through. This test was left asserting
        // the old pinning contract and only surfaced once the daemon test build
        // was un-broken (#233) — updated here to the current contract.
        let out = resolve_bound_cwd(
            "/work/repo/another-sub",
            "/work/repo",
            Some(("/work/repo/sub", "/work/repo")),
        )
        .expect("matching workspace should be accepted");
        assert_eq!(
            out, "/work/repo/another-sub",
            "a resumed turn rebinds to the requested cwd (traversal-guarded)"
        );
    }

    #[test]
    fn bound_cwd_still_rejects_traversal_on_any_turn() {
        // The one invariant resolve_bound_cwd still enforces: no `..` escape,
        // resumed or not.
        assert!(matches!(
            resolve_bound_cwd(
                "/work/repo/../etc",
                "/work/repo",
                Some(("/work/repo", "/work/repo"))
            ),
            Err(CwdBindingError::PathTraversal { .. })
        ));
    }

    #[test]
    fn resumed_turn_rebinds_when_workspace_changes() {
        // A resumed turn from a different workspace should follow the caller's
        // cwd so the session can rebind to the new project.
        let out = resolve_bound_cwd(
            "/other/project",
            "/other/project",
            Some(("/work/repo/sub", "/work/repo")),
        )
        .expect("cross-workspace resume should rebind");
        assert_eq!(out, "/other/project");
    }

    #[test]
    fn path_traversal_cwd_is_rejected_for_new_session() {
        let err = resolve_bound_cwd("/work/repo/../../etc", "/work/repo", None)
            .expect_err("traversal cwd must be rejected");
        assert!(matches!(err, CwdBindingError::PathTraversal { .. }));
    }

    #[test]
    fn path_traversal_cwd_is_rejected_for_resumed_session() {
        // Even when the (lexical) workspace strings would match, a `..` in the
        // requested cwd is rejected before any binding comparison.
        let err = resolve_bound_cwd(
            "/work/repo/../repo",
            "/work/repo",
            Some(("/work/repo", "/work/repo")),
        )
        .expect_err("traversal cwd must be rejected on resume too");
        assert!(matches!(err, CwdBindingError::PathTraversal { .. }));
    }

    #[test]
    fn cwd_has_traversal_detects_parent_components_only() {
        assert!(cwd_has_traversal("/a/../b"));
        assert!(cwd_has_traversal("../b"));
        assert!(!cwd_has_traversal("/a/b/c"));
        // A literal dir literally named "..something" is not a parent ref.
        assert!(!cwd_has_traversal("/a/..b/c"));
        assert!(!cwd_has_traversal("/work/repo"));
    }

    /// A caller declaring workspace A must NOT read a session bound to workspace
    /// B: the detail read path enforces the same boundary as the turn path.
    #[test]
    fn session_detail_rejects_cross_workspace_read() {
        let err = session_detail_scope_check(Some("/work/repo-a"), Some("/work/repo-b"))
            .expect_err("a cross-workspace detail read must be rejected");
        match err {
            CwdBindingError::WorkspaceMismatch {
                requested_workspace,
                session_workspace,
            } => {
                assert_eq!(requested_workspace, "/work/repo-a");
                assert_eq!(session_workspace, "/work/repo-b");
            }
            other => panic!("expected WorkspaceMismatch, got {other:?}"),
        }
    }

    /// A caller in the same workspace, an unscoped caller, and a legacy session
    /// with no bound workspace all read successfully (backward compatible).
    #[test]
    fn session_detail_allows_matching_or_unscoped_read() {
        // Same workspace → allowed.
        assert!(
            session_detail_scope_check(Some("/work/repo"), Some("/work/repo")).is_ok(),
            "a same-workspace read must be allowed"
        );
        // No declared scope → allowed (legacy first-party caller).
        assert!(
            session_detail_scope_check(None, Some("/work/repo")).is_ok(),
            "an unscoped read must remain allowed"
        );
        // Legacy session with no bound workspace → allowed (no boundary to enforce).
        assert!(
            session_detail_scope_check(Some("/work/repo"), None).is_ok(),
            "a session with no bound workspace has no boundary to enforce"
        );
    }
}
