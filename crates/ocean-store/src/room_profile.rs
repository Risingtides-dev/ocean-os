//! Rooms Phase 2 Stage 2b — the durable room profile.
//!
//! See `docs/specs/2026-09-08-ocean-rooms-phase2-room-profile-and-contributed-folders-manifest.md` §2.1.
//!
//! A profile DECLARES what a room's work depends on: repository references,
//! tool references, and named credential slots with an ordered list of places
//! to look. Nothing in it is authority and nothing in it is a secret. The
//! `resolvers` on a slot say *where to look*; what was found is computed by the
//! daemon at read/admission time and never stored.
//!
//! Writes are operator decisions and replay-safe under the same rule as the
//! Phase 1 binding: a `decision_id` presented twice with identical content is
//! idempotent (no revision bump, no audit row); presented with different
//! content it is refused. The decision namespace is room-wide across BOTH
//! ledgers — a decision that authorized an agent can never be reused to write
//! a profile, and vice versa.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use ocean_core::{RoomKey, RoomMessage, RoomMessageKind, RoomParticipantKind};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use super::{
    fmt_ts, parse_canonical_u64_text, parse_ts, MessageDraft, Result, RoomStoreError,
    SqliteRoomStore,
};

/// One repository the room's work is about. `remote` is a URL, never a local
/// path; `resource_id` optionally names the contributed folder (Stage 2c) that
/// is this repo's checkout on this node. Ruling §11.2: a repo may have no
/// folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub alias: String,
    pub remote: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
}

/// Which kind of tool surface a [`ToolRef`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolRefKind {
    Mcp,
    Plugin,
    Builtin,
}

impl ToolRefKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::Plugin => "plugin",
            Self::Builtin => "builtin",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "mcp" => Some(Self::Mcp),
            "plugin" => Some(Self::Plugin),
            "builtin" => Some(Self::Builtin),
            _ => None,
        }
    }
}

/// One tool server or plugin the room's agents may be allowed to use.
/// `allowed` is an explicit tool-name allowlist; empty means none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRef {
    pub kind: ToolRefKind,
    pub name: String,
    #[serde(default)]
    pub allowed: Vec<String>,
}

/// One named credential the room's work needs. `resolvers` is an ordered list
/// of `scheme:target` strings (`oauth:claude-code`, `env:GH_TOKEN`,
/// `keychain:ocean/GH_TOKEN`); the daemon walks it on the executing node and
/// reports a STATUS, never a value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialSlot {
    pub name: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(default)]
    pub required: bool,
    pub resolvers: Vec<String>,
}

/// The durable profile row for one room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomProfile {
    pub room_id: RoomKey,
    /// Bumped on every write that changes content. An idempotent replay does
    /// not bump it.
    pub revision: u64,
    pub repos: Vec<RepoRef>,
    pub tools: Vec<ToolRef>,
    pub credential_slots: Vec<CredentialSlot>,
    /// Room-wide fallback folder for a turn's cwd (manifest §5).
    pub default_resource_id: Option<String>,
    /// Per-agent folder, consulted before `default_resource_id` (ruling §11.4).
    pub agent_defaults: BTreeMap<String, String>,
    /// Operator principal id that made the most recent write.
    pub updated_by: String,
    pub updated_at: DateTime<Utc>,
    pub decision_id: String,
    pub request_digest: String,
}

/// One operator-approved profile write. `request_digest` is computed by the
/// caller over the canonical approved content; the store compares it but never
/// derives it, exactly as [`super::AuthorizeAgentInput`] does.
#[derive(Debug, Clone)]
pub struct PutRoomProfileInput {
    pub repos: Vec<RepoRef>,
    pub tools: Vec<ToolRef>,
    pub credential_slots: Vec<CredentialSlot>,
    pub default_resource_id: Option<String>,
    pub agent_defaults: BTreeMap<String, String>,
    pub updated_by: String,
    pub decision_id: String,
    pub request_digest: String,
}

/// DDL for the two Phase 2b tables. Appended to the store's idempotent
/// `CREATE TABLE IF NOT EXISTS` batch by [`SqliteRoomStore::open`].
pub(super) const ROOM_PROFILE_DDL: &str = r#"
    -- Rooms Phase 2b: one declarative profile per room. Every JSON column is
    -- a canonical serde encoding of the matching ocean-store type; nothing in
    -- this table is a secret and nothing in it is federated.
    CREATE TABLE IF NOT EXISTS room_profiles (
        room_id             TEXT NOT NULL PRIMARY KEY REFERENCES rooms(id) ON DELETE CASCADE,
        revision            TEXT NOT NULL,   -- canonical decimal u64
        repos               TEXT NOT NULL,   -- JSON [RepoRef]
        tools               TEXT NOT NULL,   -- JSON [ToolRef]
        credential_slots    TEXT NOT NULL,   -- JSON [CredentialSlot]
        default_resource_id TEXT,
        agent_defaults      TEXT NOT NULL,   -- JSON {agent_member_id: resource_id}
        updated_by          TEXT NOT NULL,   -- operator principal id
        updated_at          TEXT NOT NULL,   -- RFC3339
        decision_id         TEXT NOT NULL,
        request_digest      TEXT NOT NULL
    );

    -- Immutable replay ledger for profile writes, mirroring
    -- room_agent_decisions. A consumed decision stays here forever so the
    -- same id can never approve different content later.
    CREATE TABLE IF NOT EXISTS room_profile_decisions (
        room_id        TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
        decision_id    TEXT NOT NULL,
        request_digest TEXT NOT NULL,
        consumed_at    TEXT NOT NULL,
        PRIMARY KEY (room_id, decision_id)
    );
"#;

fn encode<T: Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| RoomStoreError::Encode(e.to_string()))
}

fn decode<T: for<'de> Deserialize<'de>>(raw: &str, what: &str) -> Result<T> {
    serde_json::from_str(raw)
        .map_err(|e| RoomStoreError::Encode(format!("bad {what} JSON in room_profiles: {e}")))
}

impl SqliteRoomStore {
    /// The room's profile, or `None` when the room has never had one written.
    /// Unknown room is an error, not `None`, so a caller cannot mistake
    /// "no such room" for "no profile yet".
    pub fn room_profile(&self, key: &RoomKey) -> Result<Option<RoomProfile>> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        self.conn
            .query_row(
                "SELECT revision, repos, tools, credential_slots, default_resource_id,
                        agent_defaults, updated_by, updated_at, decision_id, request_digest
                   FROM room_profiles WHERE room_id = ?1",
                params![key.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(
                    revision,
                    repos,
                    tools,
                    slots,
                    default_resource_id,
                    agent_defaults,
                    updated_by,
                    updated_at,
                    decision_id,
                    request_digest,
                )| {
                    Ok(RoomProfile {
                        room_id: key.clone(),
                        revision: parse_canonical_u64_text(&revision)?,
                        repos: decode(&repos, "repos")?,
                        tools: decode(&tools, "tools")?,
                        credential_slots: decode(&slots, "credential_slots")?,
                        default_resource_id,
                        agent_defaults: decode(&agent_defaults, "agent_defaults")?,
                        updated_by,
                        updated_at: parse_ts(&updated_at)?,
                        decision_id,
                        request_digest,
                    })
                },
            )
            .transpose()
    }

    /// Whether `decision_id` has already been consumed in this room by EITHER
    /// authority ledger, and with what digest. The namespace is room-wide so
    /// one approval can never be replayed across authority kinds.
    pub fn room_profile_decision(
        &self,
        key: &RoomKey,
        decision_id: &str,
    ) -> Result<Option<String>> {
        if let Some(digest) = self
            .conn
            .query_row(
                "SELECT request_digest FROM room_profile_decisions
                  WHERE room_id = ?1 AND decision_id = ?2",
                params![key.as_str(), decision_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        {
            return Ok(Some(digest));
        }
        self.conn
            .query_row(
                "SELECT request_digest FROM room_agent_decisions
                  WHERE room_id = ?1 AND decision_id = ?2",
                params![key.as_str(), decision_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(RoomStoreError::from)
    }

    /// Write the room's profile under one operator decision.
    ///
    /// Returns `(profile, changed, audit)`. `changed` is false and `audit` is
    /// `None` for an exact replay. A replay with different content, or a
    /// decision id already consumed by the agent-binding ledger, is refused
    /// with [`RoomStoreError::DecisionReplayMismatch`] and writes nothing.
    pub fn put_room_profile(
        &mut self,
        key: &RoomKey,
        input: PutRoomProfileInput,
        now: DateTime<Utc>,
    ) -> Result<(RoomProfile, bool, Option<RoomMessage>)> {
        if input.decision_id.trim().is_empty()
            || input.request_digest.trim().is_empty()
            || input.updated_by.trim().is_empty()
        {
            return Err(RoomStoreError::Encode(
                "decision id, request digest, and updated_by are required".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists: Option<i64> = tx
            .query_row(
                "SELECT 1 FROM rooms WHERE id = ?1",
                params![key.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }

        // Replay check FIRST, across both ledgers, before any content is
        // compared: a consumed id with a different digest is a refusal even
        // if the current row happens to already carry the new content.
        let prior_profile: Option<String> = tx
            .query_row(
                "SELECT request_digest FROM room_profile_decisions
                  WHERE room_id = ?1 AND decision_id = ?2",
                params![key.as_str(), input.decision_id],
                |row| row.get(0),
            )
            .optional()?;
        let prior_agent: Option<String> = tx
            .query_row(
                "SELECT request_digest FROM room_agent_decisions
                  WHERE room_id = ?1 AND decision_id = ?2",
                params![key.as_str(), input.decision_id],
                |row| row.get(0),
            )
            .optional()?;
        if prior_agent.is_some() {
            return Err(RoomStoreError::DecisionReplayMismatch {
                room: key.clone(),
                decision_id: input.decision_id,
            });
        }
        if let Some(prior_digest) = prior_profile {
            if prior_digest != input.request_digest {
                return Err(RoomStoreError::DecisionReplayMismatch {
                    room: key.clone(),
                    decision_id: input.decision_id,
                });
            }
            drop(tx);
            let current = self.room_profile(key)?.ok_or_else(|| {
                RoomStoreError::Encode("consumed decision without a profile row".into())
            })?;
            return Ok((current, false, None));
        }

        let existing_revision: Option<String> = tx
            .query_row(
                "SELECT revision FROM room_profiles WHERE room_id = ?1",
                params![key.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let created = existing_revision.is_none();
        let revision = match existing_revision {
            Some(raw) => parse_canonical_u64_text(&raw)?.saturating_add(1),
            None => 1,
        };

        let repos = encode(&input.repos)?;
        let tools = encode(&input.tools)?;
        let slots = encode(&input.credential_slots)?;
        let agent_defaults = encode(&input.agent_defaults)?;
        let updated_at = fmt_ts(now);
        tx.execute(
            "INSERT INTO room_profiles (
                room_id, revision, repos, tools, credential_slots, default_resource_id,
                agent_defaults, updated_by, updated_at, decision_id, request_digest
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(room_id) DO UPDATE SET
                revision = excluded.revision,
                repos = excluded.repos,
                tools = excluded.tools,
                credential_slots = excluded.credential_slots,
                default_resource_id = excluded.default_resource_id,
                agent_defaults = excluded.agent_defaults,
                updated_by = excluded.updated_by,
                updated_at = excluded.updated_at,
                decision_id = excluded.decision_id,
                request_digest = excluded.request_digest",
            params![
                key.as_str(),
                revision.to_string(),
                repos,
                tools,
                slots,
                input.default_resource_id,
                agent_defaults,
                input.updated_by,
                updated_at,
                input.decision_id,
                input.request_digest,
            ],
        )?;
        tx.execute(
            "INSERT INTO room_profile_decisions (room_id, decision_id, request_digest, consumed_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                key.as_str(),
                input.decision_id,
                input.request_digest,
                fmt_ts(now)
            ],
        )?;

        // Content-minimal audit (manifest §8): actor, action, revision, and
        // COUNTS. No remote, no slot name, no resolver — the row is a ledger
        // of that a write happened, not a copy of what was written.
        let audit_body = serde_json::to_string(&serde_json::json!({
            "type": if created { "room.profile.created" } else { "room.profile.updated" },
            "room_id": key.as_str(),
            "revision": revision.to_string(),
            "updated_by": input.updated_by,
            "decision_id": input.decision_id,
            "repos": input.repos.len(),
            "tools": input.tools.len(),
            "credential_slots": input.credential_slots.len(),
        }))
        .map_err(|e| RoomStoreError::Encode(e.to_string()))?;
        let audit = Self::insert_message_on(
            &tx,
            key,
            MessageDraft {
                author_id: "system",
                author_kind: RoomParticipantKind::System,
                kind: RoomMessageKind::System,
                body: &audit_body,
                thread_parent_seq: None,
                session_id: None,
                attachment_id: None,
            },
            now,
        )?;
        Self::touch_on(&tx, key, now)?;
        tx.commit()?;

        let profile = RoomProfile {
            room_id: key.clone(),
            revision,
            repos: input.repos,
            tools: input.tools,
            credential_slots: input.credential_slots,
            default_resource_id: input.default_resource_id,
            agent_defaults: input.agent_defaults,
            updated_by: input.updated_by,
            updated_at: now,
            decision_id: input.decision_id,
            request_digest: input.request_digest,
        };
        Ok((profile, true, Some(audit)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActivationPolicy, AuthorizeAgentInput, ContextPolicy, MemoryScope, RoomStore};

    fn store() -> SqliteRoomStore {
        SqliteRoomStore::open_in_memory().unwrap()
    }

    fn room() -> (SqliteRoomStore, RoomKey) {
        let mut s = store();
        let key = RoomKey::new("hq");
        s.create(key.clone(), "HQ", None, Utc::now()).unwrap();
        (s, key)
    }

    fn input(decision: &str, digest: &str) -> PutRoomProfileInput {
        PutRoomProfileInput {
            repos: vec![RepoRef {
                alias: "source".into(),
                remote: "https://github.com/example/app.git".into(),
                default_branch: Some("main".into()),
                resource_id: None,
            }],
            tools: vec![ToolRef {
                kind: ToolRefKind::Mcp,
                name: "github".into(),
                allowed: vec!["list_prs".into()],
            }],
            credential_slots: vec![CredentialSlot {
                name: "GH_TOKEN".into(),
                purpose: "gh CLI".into(),
                required: true,
                resolvers: vec!["oauth:github".into(), "env:GH_TOKEN".into()],
            }],
            default_resource_id: None,
            agent_defaults: BTreeMap::new(),
            updated_by: "operator-1".into(),
            decision_id: decision.into(),
            request_digest: digest.into(),
        }
    }

    #[test]
    fn a_room_without_a_profile_reads_none_but_an_unknown_room_is_an_error() {
        let (s, key) = room();
        assert_eq!(s.room_profile(&key).unwrap(), None);
        let err = s.room_profile(&RoomKey::new("nope")).unwrap_err();
        assert!(matches!(err, RoomStoreError::UnknownRoom(_)), "{err:?}");
    }

    #[test]
    fn first_write_creates_revision_one_with_one_audit_row() {
        let (mut s, key) = room();
        let (p, changed, audit) = s
            .put_room_profile(&key, input("dec-1", "d1"), Utc::now())
            .unwrap();
        assert!(changed);
        assert_eq!(p.revision, 1);
        let audit = audit.expect("a real write leaves an audit row");
        assert_eq!(audit.kind, RoomMessageKind::System);
        let body: serde_json::Value = serde_json::from_str(&audit.body).unwrap();
        assert_eq!(body["type"], "room.profile.created");
        assert_eq!(body["revision"], "1");
        assert_eq!(body["credential_slots"], 1);
        // Content-minimal: no remote, no slot name, no resolver in the ledger.
        assert!(!audit.body.contains("github.com"));
        assert!(!audit.body.contains("GH_TOKEN"));
        // Round-trips exactly.
        assert_eq!(s.room_profile(&key).unwrap().unwrap(), p);
    }

    #[test]
    fn a_second_decision_bumps_revision_and_replaces_content() {
        let (mut s, key) = room();
        s.put_room_profile(&key, input("dec-1", "d1"), Utc::now())
            .unwrap();
        let mut next = input("dec-2", "d2");
        next.repos.clear();
        let (p, changed, audit) = s.put_room_profile(&key, next, Utc::now()).unwrap();
        assert!(changed);
        assert_eq!(p.revision, 2);
        assert!(p.repos.is_empty());
        let body: serde_json::Value = serde_json::from_str(&audit.unwrap().body).unwrap();
        assert_eq!(body["type"], "room.profile.updated");
    }

    #[test]
    fn replaying_a_decision_with_identical_content_is_idempotent() {
        let (mut s, key) = room();
        s.put_room_profile(&key, input("dec-1", "d1"), Utc::now())
            .unwrap();
        let (p, changed, audit) = s
            .put_room_profile(&key, input("dec-1", "d1"), Utc::now())
            .unwrap();
        assert!(!changed);
        assert!(audit.is_none(), "a replay must not mint a second audit row");
        assert_eq!(p.revision, 1);
    }

    #[test]
    fn replaying_a_decision_with_different_content_is_refused_and_writes_nothing() {
        let (mut s, key) = room();
        s.put_room_profile(&key, input("dec-1", "d1"), Utc::now())
            .unwrap();
        let err = s
            .put_room_profile(&key, input("dec-1", "d1-CHANGED"), Utc::now())
            .unwrap_err();
        assert!(
            matches!(err, RoomStoreError::DecisionReplayMismatch { .. }),
            "{err:?}"
        );
        let p = s.room_profile(&key).unwrap().unwrap();
        assert_eq!(p.revision, 1);
        assert_eq!(p.request_digest, "d1");
    }

    #[test]
    fn a_decision_consumed_by_the_agent_ledger_cannot_write_a_profile() {
        let (mut s, key) = room();
        s.add_participant(
            &key,
            ocean_core::RoomParticipant {
                id: "human-1".into(),
                kind: RoomParticipantKind::Human,
                display_name: "H".into(),
            },
            Utc::now(),
        )
        .unwrap();
        s.authorize_room_agent(
            &key,
            AuthorizeAgentInput {
                agent_member_id: "agent-1".into(),
                agent_package_id: "pkg".into(),
                agent_definition_digest: "sha256:x".into(),
                agent_definition_revision: None,
                display_name: "A".into(),
                owner_member_id: "human-1".into(),
                authorized_by: "operator-1".into(),
                activation_policy: ActivationPolicy::default(),
                context_policy: ContextPolicy::default(),
                memory_scope: MemoryScope::default(),
                requested_capabilities: vec![],
                room_capability_grants: vec![],
                decision_id: "dec-shared".into(),
                request_digest: "agent-digest".into(),
            },
            Utc::now(),
        )
        .unwrap();
        let err = s
            .put_room_profile(&key, input("dec-shared", "agent-digest"), Utc::now())
            .unwrap_err();
        assert!(
            matches!(err, RoomStoreError::DecisionReplayMismatch { .. }),
            "{err:?}"
        );
        assert_eq!(s.room_profile(&key).unwrap(), None);
        assert_eq!(
            s.room_profile_decision(&key, "dec-shared")
                .unwrap()
                .as_deref(),
            Some("agent-digest"),
            "the room-wide namespace reports the agent ledger's consumption"
        );
    }

    #[test]
    fn writing_to_an_unknown_room_is_an_error() {
        let mut s = store();
        let err = s
            .put_room_profile(&RoomKey::new("nope"), input("dec-1", "d1"), Utc::now())
            .unwrap_err();
        assert!(matches!(err, RoomStoreError::UnknownRoom(_)), "{err:?}");
    }

    #[test]
    fn agent_defaults_and_default_resource_round_trip() {
        let (mut s, key) = room();
        let mut i = input("dec-1", "d1");
        i.default_resource_id = Some("res-room".into());
        i.agent_defaults
            .insert("builder".into(), "res-builder".into());
        s.put_room_profile(&key, i, Utc::now()).unwrap();
        let p = s.room_profile(&key).unwrap().unwrap();
        assert_eq!(p.default_resource_id.as_deref(), Some("res-room"));
        assert_eq!(
            p.agent_defaults.get("builder").map(String::as_str),
            Some("res-builder")
        );
    }
}
