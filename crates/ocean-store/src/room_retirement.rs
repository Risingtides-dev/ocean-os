//! Rooms S0 — participant retirement and aliases.
//!
//! See `docs/specs/2026-09-09-ocean-rooms-participant-retirement.md`.
//!
//! Before the surface learned who a person was, it minted placeholder humans:
//! `surface-operator` ("Operator") and random `web-<hex>` ids. Those rows own
//! rooms, own agents, and authored messages. The ledger cannot be rewritten,
//! so retirement is a MERGE: the placeholder's authority moves to a real
//! member (the room owner role, every agent it owns), its roster row is
//! removed, and an alias row records `from -> to` durably so anything that
//! still names the old id — a binding's `owner_member_id`, an old message's
//! author — resolves to the person who now holds it.
//!
//! Retirement is an operator decision, replay-safe under the room-wide
//! decision namespace shared with agent bindings, profiles, and grants. It is
//! deliberately narrow: the daemon's route decides WHICH ids are retirable
//! (placeholders only); the store only enforces that both ends are humans.

use chrono::{DateTime, Utc};
use ocean_core::{RoomKey, RoomMessage, RoomMessageKind, RoomParticipantKind};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::{fmt_ts, MessageDraft, Result, RoomStoreError, SqliteRoomStore};

/// The hop bound when following `from -> to` chains. A placeholder retired
/// into a member that is itself later retired resolves two hops; anything
/// deeper is a bug, not a use case.
const MAX_ALIAS_HOPS: usize = 8;

pub(super) const ROOM_RETIREMENT_DDL: &str = r#"
    -- Rooms S0: durable `from -> to` merges of placeholder humans. One row per
    -- retired id; the `to` side may itself appear as a `from` later.
    CREATE TABLE IF NOT EXISTS room_participant_aliases (
        room_id        TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
        from_id        TEXT NOT NULL,
        to_id          TEXT NOT NULL,
        retired_at     TEXT NOT NULL,
        retired_by     TEXT NOT NULL,   -- operator principal id
        decision_id    TEXT NOT NULL,
        request_digest TEXT NOT NULL,
        PRIMARY KEY (room_id, from_id)
    );

    -- Immutable replay ledger for retirement decisions; part of the room-wide
    -- decision namespace (see room_resources::consumed_decision_on).
    CREATE TABLE IF NOT EXISTS room_retirement_decisions (
        room_id        TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
        decision_id    TEXT NOT NULL,
        from_id        TEXT NOT NULL,
        request_digest TEXT NOT NULL,
        consumed_at    TEXT NOT NULL,
        PRIMARY KEY (room_id, decision_id)
    );
"#;

/// One durable merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParticipantAlias {
    pub from_id: String,
    pub to_id: String,
    pub retired_at: DateTime<Utc>,
    pub retired_by: String,
    pub decision_id: String,
}

/// One operator-approved retirement.
#[derive(Debug, Clone)]
pub struct RetireParticipantInput {
    pub from_id: String,
    pub successor_id: String,
    pub actor: String,
    pub decision_id: String,
    pub request_digest: String,
}

/// What a retirement moved, for the route's response and the audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetiredParticipant {
    pub from_id: String,
    pub to_id: String,
    /// The room owner role moved from `from` to `to`.
    pub owner_moved: bool,
    /// How many `room_agent_owners` rows now name `to`.
    pub agents_moved: u64,
}

/// Follow aliases from `id` to the member that holds its authority today.
/// Returns `id` itself when it was never retired.
pub(super) fn resolve_alias_on(conn: &Connection, key: &RoomKey, id: &str) -> Result<String> {
    let mut current = id.to_string();
    for _ in 0..MAX_ALIAS_HOPS {
        let next: Option<String> = conn
            .query_row(
                "SELECT to_id FROM room_participant_aliases WHERE room_id = ?1 AND from_id = ?2",
                params![key.as_str(), current],
                |row| row.get(0),
            )
            .optional()?;
        match next {
            Some(next) if next != current => current = next,
            _ => return Ok(current),
        }
    }
    Ok(current)
}

impl SqliteRoomStore {
    /// Every alias in the room, oldest first.
    pub fn room_participant_aliases(&self, key: &RoomKey) -> Result<Vec<ParticipantAlias>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_id, to_id, retired_at, retired_by, decision_id
               FROM room_participant_aliases WHERE room_id = ?1
              ORDER BY retired_at, from_id",
        )?;
        let rows = stmt.query_map(params![key.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (from_id, to_id, retired_at, retired_by, decision_id) = row?;
            Ok(ParticipantAlias {
                from_id,
                to_id,
                retired_at: super::parse_ts(&retired_at)?,
                retired_by,
                decision_id,
            })
        })
        .collect()
    }

    /// The member that holds `id`'s authority today (`id` when never retired).
    pub fn resolve_participant_alias(&self, key: &RoomKey, id: &str) -> Result<String> {
        resolve_alias_on(&self.conn, key, id)
    }

    /// Merge one human participant into another under one operator decision.
    ///
    /// Returns `(retired, changed, audit)`. An exact replay returns the
    /// recorded merge with `changed == false` and no audit. Refusals write
    /// nothing: unknown `from` is [`RoomStoreError::UnknownParticipant`];
    /// a non-human `from`, a missing or non-human successor, or `from ==
    /// successor` is [`RoomStoreError::ParticipantKindConflict`]; a consumed
    /// decision with different content is
    /// [`RoomStoreError::DecisionReplayMismatch`].
    pub fn retire_participant(
        &mut self,
        key: &RoomKey,
        input: RetireParticipantInput,
        now: DateTime<Utc>,
    ) -> Result<(RetiredParticipant, bool, Option<RoomMessage>)> {
        if input.decision_id.trim().is_empty()
            || input.request_digest.trim().is_empty()
            || input.actor.trim().is_empty()
            || input.from_id.trim().is_empty()
            || input.successor_id.trim().is_empty()
        {
            return Err(RoomStoreError::Encode(
                "from, successor, actor, decision id, and request digest are required".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !Self::room_is_open_on(&tx, key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }

        // Replay first, across every ledger.
        if let Some(prior) =
            super::room_resources::consumed_decision_on(&tx, key, &input.decision_id)?
        {
            let same: Option<String> = tx
                .query_row(
                    "SELECT from_id FROM room_retirement_decisions
                      WHERE room_id = ?1 AND decision_id = ?2",
                    params![key.as_str(), input.decision_id],
                    |row| row.get(0),
                )
                .optional()?;
            if prior != input.request_digest || same.as_deref() != Some(input.from_id.as_str()) {
                return Err(RoomStoreError::DecisionReplayMismatch {
                    room: key.clone(),
                    decision_id: input.decision_id,
                });
            }
            let alias: Option<(String, String)> = tx
                .query_row(
                    "SELECT from_id, to_id FROM room_participant_aliases
                      WHERE room_id = ?1 AND from_id = ?2",
                    params![key.as_str(), input.from_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((from_id, to_id)) = alias else {
                return Err(RoomStoreError::Encode(
                    "consumed retirement decision without an alias row".into(),
                ));
            };
            drop(tx);
            return Ok((
                RetiredParticipant {
                    from_id,
                    to_id,
                    owner_moved: false,
                    agents_moved: 0,
                },
                false,
                None,
            ));
        }

        let kind_of = |id: &str| -> Result<Option<String>> {
            Ok(tx
                .query_row(
                    "SELECT kind FROM participants WHERE room_id = ?1 AND id = ?2",
                    params![key.as_str(), id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        };
        let Some(from_kind) = kind_of(&input.from_id)? else {
            return Err(RoomStoreError::UnknownParticipant {
                room: key.clone(),
                participant: input.from_id,
            });
        };
        if from_kind != "human" {
            return Err(RoomStoreError::ParticipantKindConflict {
                room: key.clone(),
                participant: input.from_id,
                existing: from_kind,
                offered: "retirable human".into(),
            });
        }
        if input.from_id == input.successor_id {
            return Err(RoomStoreError::ParticipantKindConflict {
                room: key.clone(),
                participant: input.successor_id,
                existing: "human".into(),
                offered: "successor of itself".into(),
            });
        }
        match kind_of(&input.successor_id)? {
            Some(kind) if kind == "human" => {}
            Some(kind) => {
                return Err(RoomStoreError::ParticipantKindConflict {
                    room: key.clone(),
                    participant: input.successor_id,
                    existing: kind,
                    offered: "human successor".into(),
                })
            }
            None => {
                return Err(RoomStoreError::UnknownParticipant {
                    room: key.clone(),
                    participant: input.successor_id,
                })
            }
        }

        // 1. Room owner role. The partial unique index allows one owner per
        //    room, so the placeholder's row goes before the successor's is
        //    promoted (or inserted).
        let from_role: Option<String> = tx
            .query_row(
                "SELECT role FROM room_local_roles WHERE room_id = ?1 AND member_id = ?2",
                params![key.as_str(), input.from_id],
                |row| row.get(0),
            )
            .optional()?;
        let owner_moved = from_role.as_deref() == Some("owner");
        tx.execute(
            "DELETE FROM room_local_roles WHERE room_id = ?1 AND member_id = ?2",
            params![key.as_str(), input.from_id],
        )?;
        if owner_moved {
            tx.execute(
                "INSERT INTO room_local_roles (room_id, member_id, role, established_at, established_by)
                 VALUES (?1, ?2, 'owner', ?3, ?4)
                 ON CONFLICT(room_id, member_id) DO UPDATE SET
                    role = 'owner', established_at = excluded.established_at,
                    established_by = excluded.established_by",
                params![
                    key.as_str(),
                    input.successor_id,
                    fmt_ts(now),
                    input.actor
                ],
            )?;
        }

        // 2. Every agent the placeholder owned.
        let agents_moved = tx.execute(
            "UPDATE room_agent_owners SET owner_id = ?3 WHERE room_id = ?1 AND owner_id = ?2",
            params![key.as_str(), input.from_id, input.successor_id],
        )? as u64;

        // 3. The roster row, then the alias that outlives it.
        tx.execute(
            "DELETE FROM participants WHERE room_id = ?1 AND id = ?2",
            params![key.as_str(), input.from_id],
        )?;
        tx.execute(
            "INSERT INTO room_participant_aliases
                (room_id, from_id, to_id, retired_at, retired_by, decision_id, request_digest)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                key.as_str(),
                input.from_id,
                input.successor_id,
                fmt_ts(now),
                input.actor,
                input.decision_id,
                input.request_digest,
            ],
        )?;
        tx.execute(
            "INSERT INTO room_retirement_decisions
                (room_id, decision_id, from_id, request_digest, consumed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                key.as_str(),
                input.decision_id,
                input.from_id,
                input.request_digest,
                fmt_ts(now),
            ],
        )?;

        // 4. One System row. Content-minimal: ids and counts, no digest.
        let body = serde_json::to_string(&serde_json::json!({
            "type": "room.participant.retired",
            "room_id": key.as_str(),
            "from": input.from_id,
            "to": input.successor_id,
            "owner_moved": owner_moved,
            "agents_moved": agents_moved,
            "actor": input.actor,
            "decision_id": input.decision_id,
        }))
        .map_err(|e| RoomStoreError::Encode(e.to_string()))?;
        let audit = Self::insert_message_on(
            &tx,
            key,
            MessageDraft {
                author_id: "system",
                author_kind: RoomParticipantKind::System,
                kind: RoomMessageKind::System,
                body: &body,
                thread_parent_seq: None,
                session_id: None,
                attachment_id: None,
            },
            now,
        )?;
        Self::touch_on(&tx, key, now)?;
        tx.commit()?;
        Ok((
            RetiredParticipant {
                from_id: input.from_id,
                to_id: input.successor_id,
                owner_moved,
                agents_moved,
            },
            true,
            Some(audit),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActivationPolicy, AuthorizeAgentInput, ContextPolicy, MemoryScope, RoomStore};
    use ocean_core::RoomParticipant;

    fn human(id: &str) -> RoomParticipant {
        RoomParticipant {
            id: id.into(),
            kind: RoomParticipantKind::Human,
            display_name: id.into(),
        }
    }

    fn agent(id: &str) -> RoomParticipant {
        RoomParticipant {
            id: id.into(),
            kind: RoomParticipantKind::Agent,
            display_name: id.into(),
        }
    }

    /// A room the way the old surface left it: the placeholder owns the room
    /// and the agent, and the real human is just on the roster.
    fn legacy_room() -> (SqliteRoomStore, RoomKey) {
        let mut s = SqliteRoomStore::open_in_memory().unwrap();
        let key = RoomKey::new("campaigns");
        s.create(key.clone(), "Campaigns", None, Utc::now())
            .unwrap();
        s.add_participant(&key, human("surface-operator"), Utc::now())
            .unwrap();
        s.add_participant(&key, human("smaths"), Utc::now())
            .unwrap();
        s.add_participant(&key, human("web-18c11f5d551e63f8"), Utc::now())
            .unwrap();
        s.bootstrap_local_room_agent(
            &key,
            "surface-operator",
            agent("room-builder"),
            "room-builder",
            "op",
            Utc::now(),
        )
        .unwrap();
        s.authorize_room_agent(
            &key,
            AuthorizeAgentInput {
                agent_member_id: "room-builder".into(),
                agent_package_id: "room-builder".into(),
                agent_definition_digest: "sha256:x".into(),
                agent_definition_revision: None,
                display_name: "Builder".into(),
                owner_member_id: "surface-operator".into(),
                authorized_by: "op".into(),
                activation_policy: ActivationPolicy::default(),
                context_policy: ContextPolicy::default(),
                memory_scope: MemoryScope::default(),
                requested_capabilities: vec![],
                room_capability_grants: vec![],
                decision_id: "dec-auth".into(),
                request_digest: "d-auth".into(),
            },
            Utc::now(),
        )
        .unwrap();
        (s, key)
    }

    fn input(from: &str, to: &str, decision: &str) -> RetireParticipantInput {
        RetireParticipantInput {
            from_id: from.into(),
            successor_id: to.into(),
            actor: "op".into(),
            decision_id: decision.into(),
            request_digest: format!("digest-{from}-{to}"),
        }
    }

    #[test]
    fn retiring_the_placeholder_moves_owner_role_agents_roster_and_writes_an_alias() {
        let (mut s, key) = legacy_room();
        assert_eq!(
            s.local_room_owner(&key).unwrap().unwrap().member_id,
            "surface-operator"
        );
        let (retired, changed, audit) = s
            .retire_participant(
                &key,
                input("surface-operator", "smaths", "dec-1"),
                Utc::now(),
            )
            .unwrap();
        assert!(changed);
        assert_eq!(retired.to_id, "smaths");
        assert!(retired.owner_moved);
        assert_eq!(retired.agents_moved, 1);
        // Owner role moved and is eligible (smaths is a live human).
        let owner = s.local_room_owner(&key).unwrap().unwrap();
        assert_eq!(owner.member_id, "smaths");
        assert!(owner.eligible);
        // The agent is now owned by smaths and the owner is present.
        let owners = s.agent_owners(&key).unwrap();
        assert_eq!(
            owners,
            vec![("room-builder".to_string(), "smaths".to_string(), true)]
        );
        // The placeholder is off the roster; smaths and the web id remain.
        let ids: Vec<String> = s
            .get(&key)
            .unwrap()
            .unwrap()
            .room
            .participants
            .iter()
            .map(|p| p.id.clone())
            .collect();
        assert!(!ids.contains(&"surface-operator".to_string()));
        assert!(ids.contains(&"smaths".to_string()));
        // Alias resolves, and the binding's frozen owner id resolves through it.
        assert_eq!(
            s.resolve_participant_alias(&key, "surface-operator")
                .unwrap(),
            "smaths"
        );
        assert_eq!(
            s.resolve_participant_alias(&key, "smaths").unwrap(),
            "smaths"
        );
        assert_eq!(
            s.resolve_participant_alias(&key, "nobody").unwrap(),
            "nobody"
        );
        let binding = s.room_agent_binding(&key, "room-builder").unwrap().unwrap();
        assert_eq!(
            binding.owner_member_id, "surface-operator",
            "the ledger is not rewritten"
        );
        // Audit row is content-minimal and typed.
        let audit = audit.unwrap();
        let body: serde_json::Value = serde_json::from_str(&audit.body).unwrap();
        assert_eq!(body["type"], "room.participant.retired");
        assert_eq!(body["from"], "surface-operator");
        assert_eq!(body["to"], "smaths");
        assert!(!audit.body.contains("digest-"));
        let aliases = s.room_participant_aliases(&key).unwrap();
        assert_eq!(aliases.len(), 1);
        assert_eq!(aliases[0].decision_id, "dec-1");
    }

    #[test]
    fn a_second_retirement_into_the_same_member_and_chains_resolve() {
        let (mut s, key) = legacy_room();
        s.retire_participant(
            &key,
            input("surface-operator", "smaths", "dec-1"),
            Utc::now(),
        )
        .unwrap();
        let (retired, changed, _) = s
            .retire_participant(
                &key,
                input("web-18c11f5d551e63f8", "smaths", "dec-2"),
                Utc::now(),
            )
            .unwrap();
        assert!(changed);
        assert!(!retired.owner_moved, "the web id never owned the room");
        assert_eq!(retired.agents_moved, 0);
        assert_eq!(s.room_participant_aliases(&key).unwrap().len(), 2);
        // A chain: retire smaths into a new member later; the old alias follows.
        s.add_participant(&key, human("john"), Utc::now()).unwrap();
        s.retire_participant(&key, input("smaths", "john", "dec-3"), Utc::now())
            .unwrap();
        assert_eq!(
            s.resolve_participant_alias(&key, "surface-operator")
                .unwrap(),
            "john"
        );
        assert_eq!(s.local_room_owner(&key).unwrap().unwrap().member_id, "john");
    }

    #[test]
    fn replay_is_idempotent_and_changed_content_or_other_ledgers_are_refused() {
        let (mut s, key) = legacy_room();
        s.retire_participant(
            &key,
            input("surface-operator", "smaths", "dec-1"),
            Utc::now(),
        )
        .unwrap();
        let (again, changed, audit) = s
            .retire_participant(
                &key,
                input("surface-operator", "smaths", "dec-1"),
                Utc::now(),
            )
            .unwrap();
        assert!(!changed && audit.is_none());
        assert_eq!(again.to_id, "smaths");
        // Same decision, different content (a different successor): refused.
        let mut other = input("web-18c11f5d551e63f8", "smaths", "dec-1");
        other.request_digest = "digest-other".into();
        let err = s.retire_participant(&key, other, Utc::now()).unwrap_err();
        assert!(
            matches!(err, RoomStoreError::DecisionReplayMismatch { .. }),
            "{err:?}"
        );
        // A decision the agent ledger consumed cannot retire anyone.
        let mut reused = input("web-18c11f5d551e63f8", "smaths", "dec-auth");
        reused.request_digest = "d-auth".into();
        let err = s.retire_participant(&key, reused, Utc::now()).unwrap_err();
        assert!(
            matches!(err, RoomStoreError::DecisionReplayMismatch { .. }),
            "{err:?}"
        );
        assert_eq!(s.room_participant_aliases(&key).unwrap().len(), 1);
    }

    #[test]
    fn refusals_write_nothing() {
        let (mut s, key) = legacy_room();
        let before = s.transcript(&key, None).unwrap().len();
        // Unknown from.
        assert!(matches!(
            s.retire_participant(&key, input("ghost", "smaths", "d1"), Utc::now())
                .unwrap_err(),
            RoomStoreError::UnknownParticipant { .. }
        ));
        // Agent as from.
        assert!(matches!(
            s.retire_participant(&key, input("room-builder", "smaths", "d2"), Utc::now())
                .unwrap_err(),
            RoomStoreError::ParticipantKindConflict { .. }
        ));
        // Agent as successor.
        assert!(matches!(
            s.retire_participant(
                &key,
                input("surface-operator", "room-builder", "d3"),
                Utc::now()
            )
            .unwrap_err(),
            RoomStoreError::ParticipantKindConflict { .. }
        ));
        // Missing successor.
        assert!(matches!(
            s.retire_participant(&key, input("surface-operator", "nobody", "d4"), Utc::now())
                .unwrap_err(),
            RoomStoreError::UnknownParticipant { .. }
        ));
        // Self.
        assert!(matches!(
            s.retire_participant(&key, input("smaths", "smaths", "d5"), Utc::now())
                .unwrap_err(),
            RoomStoreError::ParticipantKindConflict { .. }
        ));
        assert_eq!(s.transcript(&key, None).unwrap().len(), before);
        assert!(s.room_participant_aliases(&key).unwrap().is_empty());
        assert_eq!(
            s.local_room_owner(&key).unwrap().unwrap().member_id,
            "surface-operator"
        );
    }
}
