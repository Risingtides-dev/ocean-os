//! Rooms Phase 2 Stage 2a — `GET /v1/rooms/persistent/{key}/inspect`.
//!
//! See `docs/specs/2026-09-08-ocean-rooms-phase2-room-profile-and-contributed-folders-manifest.md` §7.1.
//!
//! # Why this exists
//!
//! Every fact an operator needs to understand a room's authority already lives
//! in the daemon: the room record, its access projection, the Phase 1 binding
//! table, the local owner role, and a deterministic session-id derivation the
//! convene path uses on every turn. None of them were served together, and the
//! session id was served nowhere, so the only way to learn where an agent's
//! transcript lived or whether a turn would even be admitted was to read the
//! source. This route projects those facts under ONE store lock so they cannot
//! disagree with each other, and it carries the Phase 2 records (`profile`,
//! `credential_slots`, `resources`) as empty slots so each later stage becomes
//! observable the moment it lands.
//!
//! # The three rules
//!
//! 1. **Read-only, credential-free.** The route consumes no operator header and
//!    performs no write. Every field is already served by a credential-free
//!    route or derived from one; the two new fields name a session the agent
//!    already owns.
//! 2. **No secret, no path we did not already serve.** `federated` is a bool
//!    derived from the presence of a `RoomCredential`; the bearer is never
//!    touched. `workspace_root` was already public on the room record. A
//!    future `local_root` (Phase 2c) never appears here.
//! 3. **Truthful execution.** `execution.cwd_source` reports the rule the
//!    convene path actually applies. Today a room without a live canonical
//!    workspace REFUSES an authorized turn (`workspace_unavailable`); it does
//!    not fall back to the daemon cwd. The projection says so rather than
//!    implying a fallback that does not exist.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use ocean_core::RoomKey;
use ocean_store::{RoomStore, RoomStoreError};
use serde_json::{json, Value};

use super::{core_sid, AppState};
use crate::persistent_rooms::{
    authorized_room_agent_session_id, persisted_room_workspace, room_store_error_response,
    with_rooms,
};
use crate::room_agent_authority::binding_projection;

/// The rule the convene path applies when choosing a turn's cwd. Only the
/// first variant admits a turn today; the second is what `workspace_unavailable`
/// looks like from the outside. Phase 2 §5 adds `resource_grant` ahead of both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CwdSource {
    /// `Room.workspace_root` is set, absolute, canonical, and a directory.
    RoomWorkspaceRoot,
    /// No usable workspace: the room's field is unset, or it no longer resolves
    /// to the canonical directory it named when it was bound.
    Unbound,
}

impl CwdSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::RoomWorkspaceRoot => "room_workspace_root",
            Self::Unbound => "unbound",
        }
    }
}

/// Resolve the cwd the convene path would use, with the rule that produced it.
/// Mirrors `persistent_rooms::start_room_agent_turn`: the stored string is only
/// authority if it still canonicalizes to itself and is a directory.
fn resolve_execution(workspace_root: Option<&str>) -> (Option<String>, CwdSource) {
    match workspace_root.and_then(persisted_room_workspace) {
        Some(cwd) => (Some(cwd), CwdSource::RoomWorkspaceRoot),
        None => (None, CwdSource::Unbound),
    }
}

pub(super) async fn room_inspect(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> (StatusCode, Json<Value>) {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({ "ok": false, "code": "invalid_room_key", "error": "invalid room key; must be non-empty" }),
            ),
        );
    }
    let key = RoomKey::new(trimmed);

    // One lock, one consistent picture. WHICH arm answered the metadata read is
    // the closedness signal, exactly as `room_snapshot` derives it.
    let result = with_rooms(&state, |store| {
        let (record, closed) = match store.get(&key) {
            Ok(Some(rec)) => (Some(rec), false),
            Ok(None) => (store.get_including_closed(&key)?, true),
            Err(e) => return Err(e),
        };
        let Some(record) = record else {
            return Ok(None);
        };
        let access = store.room_access(&key)?;
        let federated = store.room_credential(&key)?.is_some();
        let owner = store.local_room_owner(&key)?;
        let bindings = store.room_agent_bindings(&key)?;
        let profile = store.room_profile(&key)?;
        Ok::<_, RoomStoreError>(Some((
            record.room,
            closed,
            access,
            federated,
            owner,
            bindings,
            profile,
        )))
    });

    match result {
        Ok(Some((room, closed, access, federated, owner, bindings, profile))) => {
            let (cwd, cwd_source) = resolve_execution(room.workspace_root.as_deref());
            let (profile, credential_slots) =
                crate::room_profile::profile_with_slots(&state, profile.as_ref());
            // Session existence is a filesystem read outside the store lock; a
            // session file appearing between the two reads only moves this
            // flag from false to true, never the other way, so the projection
            // cannot claim a transcript that was later deleted.
            let agents = bindings
                .iter()
                .map(|binding| {
                    let sid = authorized_room_agent_session_id(
                        &key,
                        &binding.agent_member_id,
                        binding.generation,
                    );
                    let session_exists = state
                        .runtime
                        .session_detail_optional(core_sid(sid))
                        .ok()
                        .flatten()
                        .is_some();
                    let mut projection = binding_projection(binding);
                    projection["session_id"] = json!(sid.to_string());
                    projection["session_exists"] = json!(session_exists);
                    projection["execution_node"] = json!("local");
                    projection
                })
                .collect::<Vec<_>>();
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "room": {
                        "id": room.id,
                        "name": room.name,
                        "created_at": room.created_at,
                        "updated_at": room.updated_at,
                        "closed": closed,
                        "workspace_root": room.workspace_root,
                        "trigger_policy": room.trigger_policy,
                    },
                    "participants": room.participants,
                    "access": access,
                    "federated": federated,
                    "owner": owner.map(|o| json!({ "member_id": o.member_id, "eligible": o.eligible })),
                    "execution": {
                        "node": "local",
                        "cwd": cwd,
                        "cwd_source": cwd_source.as_str(),
                    },
                    "agents": agents,
                    // Stage 2b: the profile and each slot's STATUS on this
                    // node (never a value). Stage 2c fills `resources`; served
                    // empty now so a client written against the final shape
                    // needs no change when it arrives.
                    "profile": profile,
                    "credential_slots": credential_slots,
                    "resources": [],
                })),
            )
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(
                json!({ "ok": false, "code": "room_not_found", "error": format!("no room with key '{key}'") }),
            ),
        ),
        Err(e) => room_store_error_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_workspace_is_unbound_not_a_fallback() {
        let (cwd, source) = resolve_execution(None);
        assert_eq!(cwd, None);
        assert_eq!(source, CwdSource::Unbound);
        assert_eq!(source.as_str(), "unbound");
    }

    #[test]
    fn a_relative_or_missing_workspace_is_unbound() {
        assert_eq!(
            resolve_execution(Some("relative/dir")).1,
            CwdSource::Unbound
        );
        assert_eq!(
            resolve_execution(Some("/definitely/not/a/real/dir/ocean-inspect")).1,
            CwdSource::Unbound
        );
    }

    #[test]
    fn a_canonical_directory_is_the_room_workspace_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        let (cwd, source) = resolve_execution(canonical.to_str());
        assert_eq!(cwd.as_deref(), canonical.to_str());
        assert_eq!(source, CwdSource::RoomWorkspaceRoot);
    }
}
