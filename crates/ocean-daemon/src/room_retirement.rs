//! Rooms S0 — retire a placeholder human into a real member.
//!
//! See `docs/specs/2026-09-09-ocean-rooms-participant-retirement.md`.
//!
//! `POST /v1/rooms/persistent/{key}/participants/{id}/retire
//!  {decision_id, successor_id}` — operator lane, replay-safe.
//!
//! The daemon, not the store, decides WHICH ids may be retired: only the two
//! placeholder shapes the old surface minted (`surface-operator`, and
//! `web-` followed by sixteen hex digits). Anything else — a real person, an
//! agent — is `participant_not_retirable`, whatever the operator asks. This is
//! the guard against the route becoming an identity-takeover primitive: the
//! only ids it can fold into someone are ids nobody was.

use axum::{
    extract::{rejection::JsonRejection, Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::Utc;
use ocean_core::RoomKey;
use ocean_store::{RetireParticipantInput, RoomStoreError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::AppState;
use crate::persistent_rooms::{publish_room_wake, with_rooms};
use crate::room_agent_authority::{
    decision_digest, operator, validate_decision_id, validate_member_id, ApiError,
};

pub(super) const PLACEHOLDER_OPERATOR_ID: &str = "surface-operator";

/// Only the two placeholder shapes are retirable.
pub(super) fn is_retirable_placeholder(id: &str) -> bool {
    if id == PLACEHOLDER_OPERATOR_ID {
        return true;
    }
    let Some(hex) = id.strip_prefix("web-") else {
        return false;
    };
    hex.len() == 16
        && hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RetireBody {
    decision_id: String,
    successor_id: String,
}

#[derive(Serialize)]
struct RetireDigestInput<'a> {
    room_id: &'a str,
    from_id: &'a str,
    successor_id: &'a str,
}

pub(super) fn aliases_projection(
    store: &mut ocean_store::SqliteRoomStore,
    room: &RoomKey,
) -> Result<Value, RoomStoreError> {
    let aliases = store.room_participant_aliases(room)?;
    Ok(json!(aliases
        .iter()
        .map(|a| json!({ "from": a.from_id, "to": a.to_id, "retired_at": a.retired_at }))
        .collect::<Vec<_>>()))
}

pub(super) async fn room_participant_retire(
    State(state): State<AppState>,
    Path((key, participant_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Json<RetireBody>, JsonRejection>,
) -> (StatusCode, Json<Value>) {
    let result = (|| -> Result<(StatusCode, Value), ApiError> {
        let principal = operator(&state, &headers)?;
        let Json(body) = body.map_err(|_| ApiError::bad_request("invalid_request"))?;
        let room = RoomKey::new(key.trim());
        if room.as_str().is_empty() {
            return Err(ApiError::bad_request("invalid_room_key"));
        }
        let from_id = participant_id.trim().to_string();
        if !is_retirable_placeholder(&from_id) {
            return Err(ApiError::bad_request("participant_not_retirable"));
        }
        let successor_id = validate_member_id(&body.successor_id, "invalid_successor_id")?;
        if successor_id.is_empty()
            || successor_id == from_id
            || is_retirable_placeholder(&successor_id)
        {
            return Err(ApiError::bad_request("invalid_successor_id"));
        }
        let decision_id = validate_decision_id(&body.decision_id)?;
        let digest = decision_digest(&RetireDigestInput {
            room_id: room.as_str(),
            from_id: &from_id,
            successor_id: &successor_id,
        })?;
        let outcome = with_rooms(&state, |store| {
            store.retire_participant(
                &room,
                RetireParticipantInput {
                    from_id: from_id.clone(),
                    successor_id: successor_id.clone(),
                    actor: principal.id().to_string(),
                    decision_id,
                    request_digest: digest,
                },
                Utc::now(),
            )
        });
        let (retired, changed, audit) = match outcome {
            Ok(value) => value,
            Err(RoomStoreError::UnknownParticipant { participant, .. })
                if participant == from_id =>
            {
                return Err(ApiError::not_found("participant_not_found"))
            }
            Err(RoomStoreError::UnknownParticipant { .. }) => {
                return Err(ApiError::conflict("successor_not_human"))
            }
            Err(RoomStoreError::ParticipantKindConflict { participant, .. })
                if participant == from_id =>
            {
                return Err(ApiError::bad_request("participant_not_retirable"))
            }
            Err(RoomStoreError::ParticipantKindConflict { .. }) => {
                return Err(ApiError::conflict("successor_not_human"))
            }
            Err(error) => return Err(ApiError::from(error)),
        };
        if let Some(audit) = audit.as_ref() {
            publish_room_wake(&state, &room, audit);
        }
        let (owner, aliases) = with_rooms(&state, |store| {
            let owner = store.local_room_owner(&room)?.map(|o| o.member_id);
            let aliases = aliases_projection(store, &room)?;
            Ok::<_, RoomStoreError>((owner, aliases))
        })
        .map_err(ApiError::from)?;
        Ok((
            StatusCode::OK,
            json!({
                "ok": true,
                "changed": changed,
                "alias": { "from": retired.from_id, "to": retired.to_id },
                "owner_moved": retired.owner_moved,
                "agents_moved": retired.agents_moved.to_string(),
                "owner_member_id": owner,
                "aliases": aliases,
            }),
        ))
    })();
    match result {
        Ok((status, body)) => (status, Json(body)),
        Err(error) => error.response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_two_placeholder_shapes_are_retirable() {
        assert!(is_retirable_placeholder("surface-operator"));
        assert!(is_retirable_placeholder("web-18c11f5d551e63f8"));
        for not in [
            "smaths",
            "ecfromthedc",
            "room-builder",
            "web-",
            "web-18c11f5d551e63f",
            "web-18c11f5d551e63f8a",
            "web-18C11F5D551E63F8",
            "web-zz11f5d551e63f8x",
            "Operator",
            "",
        ] {
            assert!(!is_retirable_placeholder(not), "{not}");
        }
    }
}
