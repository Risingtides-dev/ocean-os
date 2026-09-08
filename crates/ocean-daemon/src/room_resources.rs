//! Rooms Phase 2 Stage 2c — local contributed folders.
//!
//! See `docs/specs/2026-09-08-ocean-rooms-phase2-room-profile-and-contributed-folders-manifest.md`
//! §2.2, §4, §5, §7.
//!
//! # What this module owns
//!
//! 1. **The grant flow.** An operator names a folder on THIS node and the
//!    daemon refuses the dangerous roots Decision 5 and manifest §4 fix — the
//!    filesystem root, the home directory itself, a root that canonicalizes
//!    elsewhere through a symlink, one it cannot open as a directory
//!    descriptor — before the store ever sees the path.
//! 2. **Custody of `local_root`.** The path is read from the store for two
//!    purposes only: to become a turn's cwd, and to confine a relative path.
//!    [`resource_projection`] is the single projection every route and
//!    `inspect` share, and it does not carry the root. Tests pin that no
//!    response body contains it.
//! 3. **Path confinement** ([`confine`]). Authorization is rooted in the
//!    canonical root, not a string prefix: the relative path is normalized,
//!    joined, canonicalized again, and refused unless the RESULT is under the
//!    root. A symlink inside the folder that points outside is an escape,
//!    whatever its name says.
//! 4. **The cwd rule** ([`resolve_turn_cwd`], manifest §5 as ruled in §11.4):
//!    the agent's own `agent_defaults` entry, then the profile's
//!    `default_resource_id`, then `Room.workspace_root`, else refused. The
//!    convene path and `inspect` call the same function so what the operator
//!    sees is what the turn gets.
//!
//! Phase 2c admits `list` and `read` grants for use by the 2d tools. `write`
//! and `execute` may be RECORDED so intent is visible, and every operation
//! needing them is refused with `phase_not_open` until Phases 4 and 5.

use std::collections::BTreeSet;
use std::path::{Component, Path as FsPath, PathBuf};

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, Utc};
use ocean_core::RoomKey;
use ocean_store::{
    GrantRoomResourceInput, ResourceAccessMode, ResourceStatus, RoomResourceGrant, RoomStore,
    RoomStoreError, SetResourceStatusInput,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::AppState;
use crate::persistent_rooms::{persisted_room_workspace, publish_room_wake, with_rooms};
use crate::room_agent_authority::{
    decision_digest, operator, validate_decision_id, validate_member_id, ApiError,
};

const MAX_AGENTS_PER_GRANT: usize = 64;
const MAX_DISPLAY_NAME_CHARS: usize = 64;

// ── Request bodies ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GrantBody {
    decision_id: String,
    display_name: String,
    local_root: String,
    access_mode: String,
    #[serde(default)]
    authorized_agent_member_ids: Vec<String>,
    #[serde(default)]
    expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct StatusBody {
    decision_id: String,
}

#[derive(Serialize)]
struct GrantDecisionDigestInput<'a> {
    room_id: &'a str,
    display_name: &'a str,
    local_root: &'a str,
    access_mode: &'a str,
    authorized_agent_member_ids: &'a [String],
    expires_at: Option<&'a str>,
}

#[derive(Serialize)]
struct StatusDecisionDigestInput<'a> {
    room_id: &'a str,
    resource_id: &'a str,
    status: &'a str,
}

// ── Dangerous roots and canonical handles ─────────────────────────────────────

/// Why a submitted root was refused. Each maps to its own typed code so the
/// Surface can say which rule fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RootRefusal {
    /// Not an absolute path.
    NotAbsolute,
    /// Does not exist.
    NotFound,
    /// Exists but is not a directory, or cannot be opened as one.
    NotDirectory,
    /// The filesystem root, the home directory itself, or a root that
    /// canonicalizes somewhere other than itself.
    Dangerous,
}

impl RootRefusal {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::NotAbsolute => "invalid_local_root",
            Self::NotFound => "local_root_not_found",
            Self::NotDirectory => "local_root_not_directory",
            Self::Dangerous => "dangerous_root",
        }
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|home| std::fs::canonicalize(home).ok())
}

/// Open the root as a directory descriptor (O_DIRECTORY, no symlink follow at
/// the leaf). Proves the root is a real directory the daemon can hold a stable
/// confined handle to, which is what Decision 5 asks of a grant root.
#[cfg(unix)]
fn open_directory_handle(path: &FsPath) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(not(unix))]
fn open_directory_handle(path: &FsPath) -> std::io::Result<std::fs::File> {
    let meta = std::fs::metadata(path)?;
    if meta.is_dir() {
        std::fs::File::open(path)
    } else {
        Err(std::io::Error::other("not a directory"))
    }
}

/// Validate a submitted root against the manifest §4 list and return its
/// canonical form. The canonical form MUST equal the submitted form: a root
/// that reaches its real directory through a symlink is refused, because the
/// operator approved the name they typed and the grant would bind somewhere
/// else.
pub(super) fn canonical_grant_root(submitted: &str) -> Result<PathBuf, RootRefusal> {
    let path = FsPath::new(submitted.trim());
    if !path.is_absolute() || submitted.trim().is_empty() {
        return Err(RootRefusal::NotAbsolute);
    }
    if path
        .components()
        .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(RootRefusal::NotAbsolute);
    }
    let canonical = match std::fs::canonicalize(path) {
        Ok(canonical) => canonical,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(RootRefusal::NotFound)
        }
        Err(_) => return Err(RootRefusal::NotDirectory),
    };
    if canonical != path {
        return Err(RootRefusal::Dangerous);
    }
    if canonical.parent().is_none() {
        return Err(RootRefusal::Dangerous);
    }
    if home_dir().is_some_and(|home| home == canonical) {
        return Err(RootRefusal::Dangerous);
    }
    match open_directory_handle(&canonical) {
        Ok(handle) => drop(handle),
        Err(_) => return Err(RootRefusal::NotDirectory),
    }
    Ok(canonical)
}

/// Why a relative path was refused under a root.
///
/// Constructed only by [`confine`], whose first route callers are the Stage
/// 2d `list`/`read` tools; both ship in 2c so the escape tests pin the rule
/// before any route can reach a file.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConfineRefusal {
    /// Absolute, or contains `..`/`.`, or a component the OS treats as
    /// special. Refused lexically before touching the filesystem.
    NotRelative,
    NotFound,
    /// Canonicalized outside the root (a symlink escape).
    Escapes,
}

impl ConfineRefusal {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::NotRelative => "invalid_relative_path",
            Self::NotFound => "path_not_found",
            Self::Escapes => "path_escapes_root",
        }
    }
}

/// Resolve `relative` under `canonical_root` and prove the result stays
/// inside. `relative` may be empty (the root itself). The check is on the
/// canonicalized RESULT, after symlink resolution — a prefix check on the
/// joined string would accept `link-to-home/.ssh` when `link-to-home` points
/// out of the folder.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn confine(canonical_root: &FsPath, relative: &str) -> Result<PathBuf, ConfineRefusal> {
    let rel = FsPath::new(relative);
    if rel.is_absolute() {
        return Err(ConfineRefusal::NotRelative);
    }
    for component in rel.components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err(ConfineRefusal::NotRelative),
        }
    }
    let joined = canonical_root.join(rel);
    let resolved = match std::fs::canonicalize(&joined) {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConfineRefusal::NotFound)
        }
        Err(_) => return Err(ConfineRefusal::Escapes),
    };
    if resolved != canonical_root && !resolved.starts_with(canonical_root) {
        return Err(ConfineRefusal::Escapes);
    }
    Ok(resolved)
}

// ── Projection ────────────────────────────────────────────────────────────────

/// The architecture's §7.4 safe projection. No `local_root`, no digest.
pub(super) fn resource_projection(grant: &RoomResourceGrant, now: DateTime<Utc>) -> Value {
    json!({
        "room_id": grant.room_id,
        "resource_id": grant.resource_id,
        "display_name": grant.display_name,
        "resource_kind": grant.resource_kind,
        "access_mode": grant.access_mode.as_str(),
        "authorized_agent_member_ids": grant.authorized_agent_member_ids,
        "status": grant.effective_status(now).as_str(),
        "generation": grant.generation.to_string(),
        "expires_at": grant.expires_at,
        "granted_by": grant.granted_by,
        "granted_at": grant.granted_at,
        "revoked_at": grant.revoked_at,
        "owner_node": "local",
        "decision_id": grant.decision_id,
    })
}

pub(super) fn resources_projection(grants: &[RoomResourceGrant]) -> Value {
    let now = Utc::now();
    json!(grants
        .iter()
        .map(|g| resource_projection(g, now))
        .collect::<Vec<_>>())
}

// ── The cwd rule ──────────────────────────────────────────────────────────────

/// Which manifest §5 rule chose a turn's cwd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TurnCwd {
    /// A live grant that authorizes the agent; the root stays private.
    ResourceGrant {
        resource_id: String,
        generation: u64,
        cwd: String,
    },
    /// `Room.workspace_root`, already public on the room record.
    RoomWorkspaceRoot { cwd: String },
    /// Nothing usable: the turn is refused with `workspace_unavailable`.
    Unbound,
}

impl TurnCwd {
    pub(super) fn cwd(&self) -> Option<&str> {
        match self {
            Self::ResourceGrant { cwd, .. } | Self::RoomWorkspaceRoot { cwd } => Some(cwd),
            Self::Unbound => None,
        }
    }

    /// The projection an agent row in `inspect` carries. A grant's cwd is
    /// never projected; the workspace root already is.
    pub(super) fn projection(&self) -> Value {
        match self {
            Self::ResourceGrant {
                resource_id,
                generation,
                ..
            } => json!({
                "cwd_source": "resource_grant",
                "resource_id": resource_id,
                "grant_generation": generation.to_string(),
            }),
            Self::RoomWorkspaceRoot { cwd } => json!({
                "cwd_source": "room_workspace_root",
                "cwd": cwd,
            }),
            Self::Unbound => json!({ "cwd_source": "unbound" }),
        }
    }
}

/// Manifest §5, as ruled in §11.4. A grant is usable as a cwd only if it is
/// `available`, authorizes the agent, and its root still canonicalizes to
/// itself as a directory — the same liveness bar `Room.workspace_root` meets.
pub(super) fn resolve_turn_cwd(
    store: &mut ocean_store::SqliteRoomStore,
    room: &RoomKey,
    agent_member_id: &str,
) -> Result<TurnCwd, RoomStoreError> {
    let now = Utc::now();
    let profile = store.room_profile(room)?;
    let candidates = profile
        .as_ref()
        .map(|p| {
            p.agent_defaults
                .get(agent_member_id)
                .cloned()
                .into_iter()
                .chain(p.default_resource_id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for resource_id in candidates {
        let Some(grant) = store.room_resource_grant(room, &resource_id)? else {
            continue;
        };
        if !grant.admits(agent_member_id, ResourceAccessMode::List, now) {
            continue;
        }
        if let Some(cwd) = persisted_room_workspace(&grant.local_root) {
            return Ok(TurnCwd::ResourceGrant {
                resource_id: grant.resource_id,
                generation: grant.generation,
                cwd,
            });
        }
    }
    let workspace = store
        .get(room)?
        .and_then(|record| record.room.workspace_root)
        .as_deref()
        .and_then(persisted_room_workspace);
    Ok(match workspace {
        Some(cwd) => TurnCwd::RoomWorkspaceRoot { cwd },
        None => TurnCwd::Unbound,
    })
}

/// Every `resource_id` a profile references must name a grant in the room
/// that is not revoked. Called by the profile `PUT` once grants exist.
pub(super) fn check_profile_resource_refs(
    store: &mut ocean_store::SqliteRoomStore,
    room: &RoomKey,
    refs: impl IntoIterator<Item = String>,
) -> Result<(), ApiError> {
    let now = Utc::now();
    for resource_id in refs {
        let grant = store
            .room_resource_grant(room, &resource_id)
            .map_err(ApiError::from)?;
        match grant {
            Some(grant) if grant.effective_status(now) != ResourceStatus::Revoked => {}
            _ => return Err(ApiError::bad_request("resource_not_found")),
        }
    }
    Ok(())
}

// ── Routes ────────────────────────────────────────────────────────────────────

fn room_key(key: &str) -> Result<RoomKey, ApiError> {
    let room = RoomKey::new(key.trim());
    if room.as_str().is_empty() {
        return Err(ApiError::bad_request("invalid_room_key"));
    }
    Ok(room)
}

pub(super) async fn room_resources_list(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> (StatusCode, Json<Value>) {
    let result = (|| -> Result<Value, ApiError> {
        let room = room_key(&key)?;
        let grants = with_rooms(&state, |store| store.room_resource_grants(&room))
            .map_err(ApiError::from)?;
        Ok(json!({ "ok": true, "resources": resources_projection(&grants) }))
    })();
    match result {
        Ok(body) => (StatusCode::OK, Json(body)),
        Err(error) => error.response(),
    }
}

pub(super) async fn room_resource_get(
    State(state): State<AppState>,
    Path((key, resource_id)): Path<(String, String)>,
) -> (StatusCode, Json<Value>) {
    let result = (|| -> Result<Value, ApiError> {
        let room = room_key(&key)?;
        let grant = with_rooms(&state, |store| {
            store.room_resource_grant(&room, resource_id.trim())
        })
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::not_found("resource_not_found"))?;
        Ok(json!({ "ok": true, "resource": resource_projection(&grant, Utc::now()) }))
    })();
    match result {
        Ok(body) => (StatusCode::OK, Json(body)),
        Err(error) => error.response(),
    }
}

pub(super) async fn room_resource_grant(
    State(state): State<AppState>,
    Path(key): Path<String>,
    headers: HeaderMap,
    body: Result<Json<GrantBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    let result = (|| {
        let principal = operator(&state, &headers)?;
        let Json(body) = body.map_err(|_| ApiError::bad_request("invalid_request"))?;
        let room = room_key(&key)?;
        let decision_id = validate_decision_id(&body.decision_id)?;

        let display_name = body.display_name.trim();
        let ok_name = display_name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && display_name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            && display_name.chars().count() <= MAX_DISPLAY_NAME_CHARS;
        if !ok_name {
            return Err(ApiError::bad_request("invalid_display_name"));
        }
        let access_mode = ResourceAccessMode::parse(body.access_mode.trim())
            .ok_or_else(|| ApiError::bad_request("invalid_access_mode"))?;
        if body.authorized_agent_member_ids.len() > MAX_AGENTS_PER_GRANT {
            return Err(ApiError::bad_request("too_many_agents"));
        }
        let mut agents = body
            .authorized_agent_member_ids
            .iter()
            .map(|id| validate_member_id(id, "invalid_agent_member_id"))
            .collect::<Result<BTreeSet<_>, _>>()?;
        agents.retain(|id| !id.is_empty());
        let agents = agents.into_iter().collect::<Vec<_>>();
        let expires_at = body
            .expires_at
            .as_deref()
            .map(|raw| {
                DateTime::parse_from_rfc3339(raw.trim())
                    .map(|t| t.with_timezone(&Utc))
                    .map_err(|_| ApiError::bad_request("invalid_expires_at"))
                    .and_then(|t| {
                        if t <= Utc::now() {
                            Err(ApiError::bad_request("invalid_expires_at"))
                        } else {
                            Ok(t)
                        }
                    })
            })
            .transpose()?;

        // The root is validated LAST so a body with a bad shape never
        // touches the filesystem, and canonicalized ONCE so the digest, the
        // store row, and the one-grant-per-root index all see the same bytes.
        let canonical_root = canonical_grant_root(&body.local_root)
            .map_err(|refusal| ApiError::bad_request(refusal.code()))?;
        let local_root = canonical_root
            .to_str()
            .ok_or_else(|| ApiError::bad_request("invalid_local_root"))?
            .to_string();
        let expires_text = expires_at.map(|t| t.to_rfc3339());
        let digest = decision_digest(&GrantDecisionDigestInput {
            room_id: room.as_str(),
            display_name,
            local_root: &local_root,
            access_mode: access_mode.as_str(),
            authorized_agent_member_ids: &agents,
            expires_at: expires_text.as_deref(),
        })?;
        let (grant, created, audit) = with_rooms(&state, |store| {
            store.grant_room_resource(
                &room,
                GrantRoomResourceInput {
                    display_name: display_name.to_string(),
                    local_root,
                    access_mode,
                    authorized_agent_member_ids: agents,
                    expires_at,
                    granted_by: principal.id().to_string(),
                    decision_id,
                    request_digest: digest,
                },
                Utc::now(),
            )
        })
        .map_err(ApiError::from)?;
        if let Some(audit) = audit.as_ref() {
            publish_room_wake(&state, &room, audit);
        }
        Ok((
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            json!({
                "ok": true,
                "created": created,
                "resource": resource_projection(&grant, Utc::now()),
            }),
        ))
    })();
    match result {
        Ok((status, body)) => (status, Json(body)),
        Err(error) => error.response(),
    }
}

async fn set_status(
    state: AppState,
    key: String,
    resource_id: String,
    headers: HeaderMap,
    body: Result<Json<StatusBody>, JsonRejection>,
    status: ResourceStatus,
) -> (StatusCode, Json<Value>) {
    let result = (|| {
        let principal = operator(&state, &headers)?;
        let Json(body) = body.map_err(|_| ApiError::bad_request("invalid_request"))?;
        let room = room_key(&key)?;
        let resource_id = resource_id.trim().to_string();
        if resource_id.is_empty() {
            return Err(ApiError::bad_request("invalid_request"));
        }
        let decision_id = validate_decision_id(&body.decision_id)?;
        let digest = decision_digest(&StatusDecisionDigestInput {
            room_id: room.as_str(),
            resource_id: &resource_id,
            status: status.as_str(),
        })?;
        let (grant, changed, audit) = with_rooms(&state, |store| {
            store.set_room_resource_status(
                &room,
                &resource_id,
                SetResourceStatusInput {
                    status,
                    actor: principal.id().to_string(),
                    decision_id,
                    request_digest: digest,
                },
                Utc::now(),
            )
        })
        .map_err(ApiError::from)?;
        if let Some(audit) = audit.as_ref() {
            publish_room_wake(&state, &room, audit);
        }
        Ok(json!({
            "ok": true,
            "changed": changed,
            "resource": resource_projection(&grant, Utc::now()),
        }))
    })();
    match result {
        Ok(body) => (StatusCode::OK, Json(body)),
        Err(error) => error.response(),
    }
}

pub(super) async fn room_resource_suspend(
    State(state): State<AppState>,
    Path((key, resource_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<StatusBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    set_status(
        state,
        key,
        resource_id,
        headers,
        body,
        ResourceStatus::Suspended,
    )
    .await
}

pub(super) async fn room_resource_resume(
    State(state): State<AppState>,
    Path((key, resource_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<StatusBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    set_status(
        state,
        key,
        resource_id,
        headers,
        body,
        ResourceStatus::Available,
    )
    .await
}

pub(super) async fn room_resource_revoke(
    State(state): State<AppState>,
    Path((key, resource_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<StatusBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    set_status(
        state,
        key,
        resource_id,
        headers,
        body,
        ResourceStatus::Revoked,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(p: &FsPath) -> PathBuf {
        std::fs::canonicalize(p).unwrap()
    }

    #[test]
    fn every_dangerous_root_in_the_manifest_list_is_refused_by_name() {
        // §4.1 the filesystem root
        assert_eq!(
            canonical_grant_root("/").unwrap_err(),
            RootRefusal::Dangerous
        );
        // §4.2 the home directory itself (subdirectories are fine)
        if let Some(home) = home_dir() {
            assert_eq!(
                canonical_grant_root(home.to_str().unwrap()).unwrap_err(),
                RootRefusal::Dangerous
            );
        }
        // §4.3 a root that canonicalizes elsewhere through a symlink
        let tmp = tempfile::TempDir::new().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = tmp.path().join("link");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, &link).unwrap();
            assert_eq!(
                canonical_grant_root(link.to_str().unwrap()).unwrap_err(),
                RootRefusal::Dangerous
            );
        }
        // §4.4 a root the daemon cannot open as a directory
        let file = tmp.path().join("file.txt");
        std::fs::write(&file, b"x").unwrap();
        assert_eq!(
            canonical_grant_root(canon(&file).to_str().unwrap()).unwrap_err(),
            RootRefusal::NotDirectory
        );
        // Not absolute, dot components, missing
        assert_eq!(
            canonical_grant_root("relative/dir").unwrap_err(),
            RootRefusal::NotAbsolute
        );
        assert_eq!(
            canonical_grant_root("/tmp/../etc").unwrap_err(),
            RootRefusal::NotAbsolute
        );
        assert_eq!(
            canonical_grant_root("").unwrap_err(),
            RootRefusal::NotAbsolute
        );
        assert_eq!(
            canonical_grant_root("/definitely/not/here/ocean-2c").unwrap_err(),
            RootRefusal::NotFound
        );
        // And a plain canonical directory is accepted as itself.
        let good = canon(&real);
        assert_eq!(canonical_grant_root(good.to_str().unwrap()).unwrap(), good);
    }

    #[test]
    fn confinement_is_on_the_canonical_result_not_the_string() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/file.txt"), b"ok").unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"no").unwrap();
        let root = canon(&root);

        assert_eq!(confine(&root, "").unwrap(), root);
        assert_eq!(confine(&root, "sub").unwrap(), root.join("sub"));
        assert_eq!(
            confine(&root, "sub/file.txt").unwrap(),
            root.join("sub/file.txt")
        );
        assert_eq!(
            confine(&root, "/etc/passwd").unwrap_err(),
            ConfineRefusal::NotRelative
        );
        assert_eq!(
            confine(&root, "../outside/secret").unwrap_err(),
            ConfineRefusal::NotRelative
        );
        assert_eq!(
            confine(&root, "sub/../../outside").unwrap_err(),
            ConfineRefusal::NotRelative
        );
        assert_eq!(
            confine(&root, "./sub").unwrap_err(),
            ConfineRefusal::NotRelative
        );
        assert_eq!(
            confine(&root, "missing.txt").unwrap_err(),
            ConfineRefusal::NotFound
        );
        assert_eq!(ConfineRefusal::NotRelative.code(), "invalid_relative_path");
        assert_eq!(ConfineRefusal::NotFound.code(), "path_not_found");
        assert_eq!(ConfineRefusal::Escapes.code(), "path_escapes_root");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
            // The joined STRING is under the root; the resolved path is not.
            assert_eq!(
                confine(&root, "escape").unwrap_err(),
                ConfineRefusal::Escapes
            );
            assert_eq!(
                confine(&root, "escape/secret").unwrap_err(),
                ConfineRefusal::Escapes
            );
            // A symlink that stays inside is fine.
            std::os::unix::fs::symlink(root.join("sub"), root.join("alias")).unwrap();
            assert_eq!(
                confine(&root, "alias/file.txt").unwrap(),
                root.join("sub/file.txt")
            );
        }
    }

    #[test]
    fn the_projection_never_carries_the_root() {
        let grant = RoomResourceGrant {
            room_id: RoomKey::new("r"),
            resource_id: "res-1".into(),
            display_name: "source".into(),
            local_root: "/Users/private/secret-root".into(),
            resource_kind: "folder".into(),
            access_mode: ResourceAccessMode::Read,
            authorized_agent_member_ids: vec!["builder".into()],
            expires_at: None,
            generation: 3,
            status: ResourceStatus::Available,
            granted_by: "op".into(),
            granted_at: Utc::now(),
            decision_id: "d".into(),
            request_digest: "sha".into(),
            revoked_at: None,
            revoked_by: None,
        };
        let rendered = resource_projection(&grant, Utc::now()).to_string();
        assert!(!rendered.contains("secret-root"), "{rendered}");
        assert!(!rendered.contains("\"sha\""), "{rendered}");
        assert!(rendered.contains("\"generation\":\"3\""));
        let mut expired = grant.clone();
        expired.expires_at = Some(Utc::now() - chrono::Duration::seconds(5));
        assert_eq!(
            resource_projection(&expired, Utc::now())["status"],
            json!("revoked")
        );
    }

    #[test]
    fn a_grant_cwd_is_never_in_the_agent_projection_but_a_workspace_root_is() {
        let grant = TurnCwd::ResourceGrant {
            resource_id: "res-1".into(),
            generation: 2,
            cwd: "/Users/private/secret-root".into(),
        };
        let rendered = grant.projection().to_string();
        assert!(!rendered.contains("secret-root"));
        assert_eq!(grant.projection()["cwd_source"], json!("resource_grant"));
        assert_eq!(grant.projection()["grant_generation"], json!("2"));
        let ws = TurnCwd::RoomWorkspaceRoot {
            cwd: "/public/ws".into(),
        };
        assert_eq!(ws.projection()["cwd"], json!("/public/ws"));
        assert_eq!(
            TurnCwd::Unbound.projection(),
            json!({ "cwd_source": "unbound" })
        );
    }
}
