//! Rooms Phase 2 Stage 2c — local contributed-folder grants.
//!
//! See `docs/specs/2026-09-08-ocean-rooms-phase2-room-profile-and-contributed-folders-manifest.md`
//! §2.2, §2.3, §4, §8.
//!
//! A grant is the LOCAL half of the architecture's `LocalRoomResourceGrant`
//! (§7.3): it binds an opaque `resource_id` the room can see to a canonical
//! `local_root` only this node may ever read. `local_root` has one custody
//! rule — it never leaves this process in a projection, a message, or an audit
//! row — and this module is where that rule starts: the audit body is built
//! from the alias and the id, never the path.
//!
//! Authority is generation-bound (Gate 0 Decision 12). Every status change
//! that alters what the grant admits bumps `generation`, and a caller that
//! planned against an older generation is refused by the daemon before any
//! side effect. Revocation is terminal; expiry reads as revoked at every check
//! and is never silently renewed.

use chrono::{DateTime, Utc};
use ocean_core::{RoomKey, RoomMessage, RoomMessageKind, RoomParticipantKind};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use super::{
    fmt_ts, parse_canonical_u64_text, parse_ts, MessageDraft, Result, RoomStoreError,
    SqliteRoomStore,
};

/// The access ladder (manifest §4): `execute ⊃ write ⊃ read ⊃ list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceAccessMode {
    List,
    Read,
    Write,
    Execute,
}

impl ResourceAccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "list" => Some(Self::List),
            "read" => Some(Self::Read),
            "write" => Some(Self::Write),
            "execute" => Some(Self::Execute),
            _ => None,
        }
    }

    /// Whether a grant at `self` admits an operation needing `needed`.
    pub fn allows(self, needed: ResourceAccessMode) -> bool {
        self >= needed
    }
}

/// Manifest §2.3 status ladder. `Revoked` is terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceStatus {
    Available,
    Suspended,
    Revoked,
}

impl ResourceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Suspended => "suspended",
            Self::Revoked => "revoked",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "available" => Some(Self::Available),
            "suspended" => Some(Self::Suspended),
            "revoked" => Some(Self::Revoked),
            _ => None,
        }
    }
}

/// The durable local grant row. `local_root` is read by the daemon for
/// enforcement only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomResourceGrant {
    pub room_id: RoomKey,
    /// Opaque, daemon-minted. The only handle the room ever sees.
    pub resource_id: String,
    pub display_name: String,
    /// Canonical absolute directory. NEVER projected.
    pub local_root: String,
    /// `"folder"` is the only kind in Phase 2.
    pub resource_kind: String,
    pub access_mode: ResourceAccessMode,
    /// Empty means no agent may touch it.
    pub authorized_agent_member_ids: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub generation: u64,
    pub status: ResourceStatus,
    pub granted_by: String,
    pub granted_at: DateTime<Utc>,
    pub decision_id: String,
    pub request_digest: String,
    pub revoked_at: Option<DateTime<Utc>>,
    pub revoked_by: Option<String>,
}

impl RoomResourceGrant {
    /// The status every check must use: an expired grant IS revoked, whatever
    /// the row says, and is never renewed by the passage of time.
    pub fn effective_status(&self, now: DateTime<Utc>) -> ResourceStatus {
        match (self.status, self.expires_at) {
            (ResourceStatus::Revoked, _) => ResourceStatus::Revoked,
            (_, Some(expires)) if expires <= now => ResourceStatus::Revoked,
            (status, _) => status,
        }
    }

    pub fn authorizes_agent(&self, agent_member_id: &str) -> bool {
        self.authorized_agent_member_ids
            .iter()
            .any(|id| id == agent_member_id)
    }

    /// Whether this grant admits `agent` for an operation needing `needed`
    /// right now.
    pub fn admits(
        &self,
        agent_member_id: &str,
        needed: ResourceAccessMode,
        now: DateTime<Utc>,
    ) -> bool {
        self.effective_status(now) == ResourceStatus::Available
            && self.authorizes_agent(agent_member_id)
            && self.access_mode.allows(needed)
    }
}

/// One operator-approved grant. The daemon has already canonicalized
/// `local_root` and refused dangerous roots; the store only enforces
/// one-grant-per-root and the decision ledger.
#[derive(Debug, Clone)]
pub struct GrantRoomResourceInput {
    pub display_name: String,
    pub local_root: String,
    pub access_mode: ResourceAccessMode,
    /// Canonical (sorted, deduped) by the caller.
    pub authorized_agent_member_ids: Vec<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub granted_by: String,
    pub decision_id: String,
    pub request_digest: String,
}

/// One admitted resource operation fact (Phase 2d). Written by the daemon's
/// resource authority after every `room_list` / `room_read`, refused or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomResourceAuditInput {
    pub resource_id: String,
    pub agent_member_id: String,
    pub binding_generation: u64,
    pub grant_generation: Option<u64>,
    pub op: String,
    pub relative_path_digest: String,
    pub bytes: u64,
    pub entries: u64,
    pub outcome: String,
    /// `agent` for a tool call inside an admitted turn; `operator_preview`
    /// for the operator-authenticated preview routes.
    pub actor: String,
}

/// One stored audit row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomResourceAuditRow {
    pub seq: u64,
    pub resource_id: String,
    pub agent_member_id: String,
    pub binding_generation: u64,
    pub grant_generation: Option<u64>,
    pub op: String,
    pub relative_path_digest: String,
    pub bytes: u64,
    pub entries: u64,
    pub outcome: String,
    pub actor: String,
    pub recorded_at: DateTime<Utc>,
}

/// One replay-safe status decision (suspend / resume / revoke).
#[derive(Debug, Clone)]
pub struct SetResourceStatusInput {
    pub status: ResourceStatus,
    pub actor: String,
    pub decision_id: String,
    pub request_digest: String,
}

pub(super) const ROOM_RESOURCE_DDL: &str = r#"
    -- Rooms Phase 2c: local contributed-folder grants. local_root is the one
    -- column that never leaves this process.
    CREATE TABLE IF NOT EXISTS room_resource_grants (
        room_id                     TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
        resource_id                 TEXT NOT NULL,
        display_name                TEXT NOT NULL,
        local_root                  TEXT NOT NULL,   -- canonical absolute path; NEVER projected
        resource_kind               TEXT NOT NULL,   -- folder
        access_mode                 TEXT NOT NULL,   -- list|read|write|execute
        authorized_agent_member_ids TEXT NOT NULL,   -- JSON array, canonical order
        expires_at                  TEXT,            -- RFC3339
        generation                  TEXT NOT NULL,   -- canonical decimal u64
        status                      TEXT NOT NULL,   -- available|suspended|revoked
        granted_by                  TEXT NOT NULL,   -- operator principal id
        granted_at                  TEXT NOT NULL,
        decision_id                 TEXT NOT NULL,
        request_digest              TEXT NOT NULL,
        revoked_at                  TEXT,
        revoked_by                  TEXT,
        PRIMARY KEY (room_id, resource_id)
    );

    -- One LIVE grant per canonical root per room. A revoked row frees the root
    -- for a fresh grant (a new decision, a new id, generation 1 again).
    CREATE UNIQUE INDEX IF NOT EXISTS idx_room_resource_grants_live_root
        ON room_resource_grants(room_id, local_root) WHERE status <> 'revoked';

    -- Rooms Phase 2d: one row per admitted list/read (manifest §8, Decision
    -- 13). The relative path is a digest, never text; no content, no root.
    CREATE TABLE IF NOT EXISTS room_resource_audit (
        room_id              TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
        seq                  INTEGER PRIMARY KEY AUTOINCREMENT,
        resource_id          TEXT NOT NULL,
        agent_member_id      TEXT NOT NULL,
        binding_generation   TEXT NOT NULL,   -- canonical decimal u64
        grant_generation     TEXT,            -- canonical decimal u64; NULL when refused before resolution
        op                   TEXT NOT NULL,   -- list|read
        relative_path_digest TEXT NOT NULL,   -- sha256 hex of the normalized relative path
        bytes                TEXT NOT NULL,   -- canonical decimal u64
        entries              TEXT NOT NULL,   -- canonical decimal u64
        outcome              TEXT NOT NULL,   -- ok | typed code
        actor                TEXT NOT NULL,   -- agent | operator_preview
        recorded_at          TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_room_resource_audit_room
        ON room_resource_audit(room_id, seq);

    -- Immutable replay ledger for grant and status decisions.
    CREATE TABLE IF NOT EXISTS room_resource_decisions (
        room_id        TEXT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
        decision_id    TEXT NOT NULL,
        resource_id    TEXT NOT NULL,
        request_digest TEXT NOT NULL,
        consumed_at    TEXT NOT NULL,
        PRIMARY KEY (room_id, decision_id)
    );
"#;

/// Whether `decision_id` has been consumed in this room by ANY authority
/// ledger — agent bindings, profile writes, or resource grants — and with what
/// digest. The namespace is room-wide by design (manifest §3): one approval
/// authorizes one thing.
pub(super) fn consumed_decision_on(
    conn: &Connection,
    key: &RoomKey,
    decision_id: &str,
) -> Result<Option<String>> {
    for sql in [
        "SELECT request_digest FROM room_resource_decisions WHERE room_id = ?1 AND decision_id = ?2",
        "SELECT request_digest FROM room_profile_decisions WHERE room_id = ?1 AND decision_id = ?2",
        "SELECT request_digest FROM room_agent_decisions WHERE room_id = ?1 AND decision_id = ?2",
    ] {
        if let Some(digest) = conn
            .query_row(sql, params![key.as_str(), decision_id], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
        {
            return Ok(Some(digest));
        }
    }
    Ok(None)
}

fn require_room(conn: &Connection, key: &RoomKey) -> Result<()> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM rooms WHERE id = ?1",
            params![key.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if exists.is_none() {
        return Err(RoomStoreError::UnknownRoom(key.clone()));
    }
    Ok(())
}

const GRANT_COLUMNS: &str = "resource_id, display_name, local_root, resource_kind, access_mode,
    authorized_agent_member_ids, expires_at, generation, status, granted_by, granted_at,
    decision_id, request_digest, revoked_at, revoked_by";

type GrantRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
);

fn read_grant_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GrantRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
        row.get(13)?,
        row.get(14)?,
    ))
}

fn grant_from_row(key: &RoomKey, row: GrantRow) -> Result<RoomResourceGrant> {
    let (
        resource_id,
        display_name,
        local_root,
        resource_kind,
        access_mode,
        agents,
        expires_at,
        generation,
        status,
        granted_by,
        granted_at,
        decision_id,
        request_digest,
        revoked_at,
        revoked_by,
    ) = row;
    Ok(RoomResourceGrant {
        room_id: key.clone(),
        resource_id,
        display_name,
        local_root,
        resource_kind,
        access_mode: ResourceAccessMode::parse(&access_mode).ok_or_else(|| {
            RoomStoreError::Encode(format!("unknown resource access mode '{access_mode}'"))
        })?,
        authorized_agent_member_ids: serde_json::from_str(&agents)
            .map_err(|e| RoomStoreError::Encode(format!("bad agent list JSON: {e}")))?,
        expires_at: expires_at.as_deref().map(parse_ts).transpose()?,
        generation: parse_canonical_u64_text(&generation)?,
        status: ResourceStatus::parse(&status)
            .ok_or_else(|| RoomStoreError::Encode(format!("unknown resource status '{status}'")))?,
        granted_by,
        granted_at: parse_ts(&granted_at)?,
        decision_id,
        request_digest,
        revoked_at: revoked_at.as_deref().map(parse_ts).transpose()?,
        revoked_by,
    })
}

fn load_grant_on(
    conn: &Connection,
    key: &RoomKey,
    resource_id: &str,
) -> Result<Option<RoomResourceGrant>> {
    conn.query_row(
        &format!("SELECT {GRANT_COLUMNS} FROM room_resource_grants WHERE room_id = ?1 AND resource_id = ?2"),
        params![key.as_str(), resource_id],
        read_grant_row,
    )
    .optional()?
    .map(|row| grant_from_row(key, row))
    .transpose()
}

fn audit_on(
    conn: &Connection,
    key: &RoomKey,
    body: serde_json::Value,
    now: DateTime<Utc>,
) -> Result<RoomMessage> {
    let body = serde_json::to_string(&body).map_err(|e| RoomStoreError::Encode(e.to_string()))?;
    SqliteRoomStore::insert_message_on(
        conn,
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
    )
}

impl SqliteRoomStore {
    /// Every grant in the room, live and revoked, oldest first. Unknown room
    /// is an error, not an empty list.
    pub fn room_resource_grants(&self, key: &RoomKey) -> Result<Vec<RoomResourceGrant>> {
        require_room(&self.conn, key)?;
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {GRANT_COLUMNS} FROM room_resource_grants WHERE room_id = ?1
              ORDER BY granted_at, resource_id"
        ))?;
        let rows = stmt.query_map(params![key.as_str()], read_grant_row)?;
        rows.map(|row| grant_from_row(key, row?)).collect()
    }

    pub fn room_resource_grant(
        &self,
        key: &RoomKey,
        resource_id: &str,
    ) -> Result<Option<RoomResourceGrant>> {
        require_room(&self.conn, key)?;
        load_grant_on(&self.conn, key, resource_id)
    }

    /// Whether `decision_id` has been consumed by any of the room's three
    /// authority ledgers.
    pub fn room_decision_consumed(
        &self,
        key: &RoomKey,
        decision_id: &str,
    ) -> Result<Option<String>> {
        consumed_decision_on(&self.conn, key, decision_id)
    }

    /// Grant a folder to the room under one operator decision.
    ///
    /// Returns `(grant, created, audit)`. An exact replay returns the existing
    /// grant with `created == false` and no audit. A decision id consumed with
    /// different content, or by another ledger, is
    /// [`RoomStoreError::DecisionReplayMismatch`]. A root that already has a
    /// live grant in this room is [`RoomStoreError::ResourceRootAlreadyGranted`]
    /// — unless that grant has expired, in which case it is marked revoked in
    /// the same transaction and the new grant proceeds.
    pub fn grant_room_resource(
        &mut self,
        key: &RoomKey,
        input: GrantRoomResourceInput,
        now: DateTime<Utc>,
    ) -> Result<(RoomResourceGrant, bool, Option<RoomMessage>)> {
        if input.decision_id.trim().is_empty()
            || input.request_digest.trim().is_empty()
            || input.granted_by.trim().is_empty()
            || input.local_root.trim().is_empty()
            || input.display_name.trim().is_empty()
        {
            return Err(RoomStoreError::Encode(
                "display name, local root, decision id, request digest, and granted_by are required"
                    .into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !Self::room_is_open_on(&tx, key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }

        if let Some(prior) = consumed_decision_on(&tx, key, &input.decision_id)? {
            if prior != input.request_digest {
                return Err(RoomStoreError::DecisionReplayMismatch {
                    room: key.clone(),
                    decision_id: input.decision_id,
                });
            }
            let resource_id: Option<String> = tx
                .query_row(
                    "SELECT resource_id FROM room_resource_decisions
                      WHERE room_id = ?1 AND decision_id = ?2",
                    params![key.as_str(), input.decision_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(resource_id) = resource_id else {
                // Same digest but consumed by a different ledger: an approval
                // for one authority kind never mints another.
                return Err(RoomStoreError::DecisionReplayMismatch {
                    room: key.clone(),
                    decision_id: input.decision_id,
                });
            };
            let existing = load_grant_on(&tx, key, &resource_id)?.ok_or_else(|| {
                RoomStoreError::Encode("consumed grant decision without a grant row".into())
            })?;
            drop(tx);
            return Ok((existing, false, None));
        }

        // One live grant per root. An expired live row is revoked here rather
        // than blocking forever on a grant time already retired.
        let live: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT resource_id, status, expires_at FROM room_resource_grants
                  WHERE room_id = ?1 AND local_root = ?2 AND status <> 'revoked'",
                params![key.as_str(), input.local_root],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((existing_id, _, expires_at)) = live {
            let expired = expires_at
                .as_deref()
                .map(parse_ts)
                .transpose()?
                .is_some_and(|expires| expires <= now);
            if !expired {
                return Err(RoomStoreError::ResourceRootAlreadyGranted {
                    room: key.clone(),
                    resource_id: existing_id,
                });
            }
            tx.execute(
                "UPDATE room_resource_grants
                    SET status = 'revoked', revoked_at = ?3, revoked_by = 'expiry',
                        generation = CAST(CAST(generation AS INTEGER) + 1 AS TEXT)
                  WHERE room_id = ?1 AND resource_id = ?2",
                params![key.as_str(), existing_id, fmt_ts(now)],
            )?;
        }

        let resource_id: String =
            tx.query_row("SELECT 'res-' || lower(hex(randomblob(16)))", [], |row| {
                row.get(0)
            })?;
        let agents = serde_json::to_string(&input.authorized_agent_member_ids)
            .map_err(|e| RoomStoreError::Encode(e.to_string()))?;
        tx.execute(
            "INSERT INTO room_resource_grants (
                room_id, resource_id, display_name, local_root, resource_kind, access_mode,
                authorized_agent_member_ids, expires_at, generation, status, granted_by,
                granted_at, decision_id, request_digest
             ) VALUES (?1, ?2, ?3, ?4, 'folder', ?5, ?6, ?7, '1', 'available', ?8, ?9, ?10, ?11)",
            params![
                key.as_str(),
                resource_id,
                input.display_name,
                input.local_root,
                input.access_mode.as_str(),
                agents,
                input.expires_at.map(fmt_ts),
                input.granted_by,
                fmt_ts(now),
                input.decision_id,
                input.request_digest,
            ],
        )?;
        tx.execute(
            "INSERT INTO room_resource_decisions (room_id, decision_id, resource_id, request_digest, consumed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                key.as_str(),
                input.decision_id,
                resource_id,
                input.request_digest,
                fmt_ts(now)
            ],
        )?;
        // Content-minimal (manifest §8): alias, id, mode, generation, actor.
        // Never the root.
        let audit = audit_on(
            &tx,
            key,
            serde_json::json!({
                "type": "room.resource.granted",
                "room_id": key.as_str(),
                "resource_id": resource_id,
                "display_name": input.display_name,
                "resource_kind": "folder",
                "access_mode": input.access_mode.as_str(),
                "authorized_agents": input.authorized_agent_member_ids.len(),
                "generation": "1",
                "granted_by": input.granted_by,
                "decision_id": input.decision_id,
            }),
            now,
        )?;
        Self::touch_on(&tx, key, now)?;
        let grant = load_grant_on(&tx, key, &resource_id)?.ok_or_else(|| {
            RoomStoreError::Encode("grant vanished inside its own transaction".into())
        })?;
        tx.commit()?;
        Ok((grant, true, Some(audit)))
    }

    /// Append one Phase 2d operation fact. No room message is minted: these
    /// are high-volume facts for the audit table, not transcript lines.
    pub fn append_room_resource_audit(
        &mut self,
        key: &RoomKey,
        input: RoomResourceAuditInput,
        now: DateTime<Utc>,
    ) -> Result<u64> {
        require_room(&self.conn, key)?;
        self.conn.execute(
            "INSERT INTO room_resource_audit (
                room_id, resource_id, agent_member_id, binding_generation, grant_generation,
                op, relative_path_digest, bytes, entries, outcome, actor, recorded_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                key.as_str(),
                input.resource_id,
                input.agent_member_id,
                input.binding_generation.to_string(),
                input.grant_generation.map(|g| g.to_string()),
                input.op,
                input.relative_path_digest,
                input.bytes.to_string(),
                input.entries.to_string(),
                input.outcome,
                input.actor,
                fmt_ts(now),
            ],
        )?;
        Ok(self.conn.last_insert_rowid() as u64)
    }

    /// The newest `limit` audit rows for a room, newest first.
    pub fn room_resource_audit_recent(
        &self,
        key: &RoomKey,
        limit: usize,
    ) -> Result<Vec<RoomResourceAuditRow>> {
        require_room(&self.conn, key)?;
        let mut stmt = self.conn.prepare(
            "SELECT seq, resource_id, agent_member_id, binding_generation, grant_generation,
                    op, relative_path_digest, bytes, entries, outcome, actor, recorded_at
               FROM room_resource_audit WHERE room_id = ?1
              ORDER BY seq DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![key.as_str(), limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
            ))
        })?;
        rows.map(|row| {
            let (
                seq,
                resource_id,
                agent_member_id,
                binding_generation,
                grant_generation,
                op,
                relative_path_digest,
                bytes,
                entries,
                outcome,
                actor,
                recorded_at,
            ) = row?;
            Ok(RoomResourceAuditRow {
                seq: seq as u64,
                resource_id,
                agent_member_id,
                binding_generation: parse_canonical_u64_text(&binding_generation)?,
                grant_generation: grant_generation
                    .as_deref()
                    .map(parse_canonical_u64_text)
                    .transpose()?,
                op,
                relative_path_digest,
                bytes: parse_canonical_u64_text(&bytes)?,
                entries: parse_canonical_u64_text(&entries)?,
                outcome,
                actor,
                recorded_at: parse_ts(&recorded_at)?,
            })
        })
        .collect()
    }

    /// Suspend, resume, or revoke a grant under one decision. Any real change
    /// bumps the generation; revoked is terminal. Returns `(grant, changed,
    /// audit)`; an exact replay or a no-op transition consumes the decision
    /// without bumping.
    pub fn set_room_resource_status(
        &mut self,
        key: &RoomKey,
        resource_id: &str,
        input: SetResourceStatusInput,
        now: DateTime<Utc>,
    ) -> Result<(RoomResourceGrant, bool, Option<RoomMessage>)> {
        if input.decision_id.trim().is_empty()
            || input.request_digest.trim().is_empty()
            || input.actor.trim().is_empty()
        {
            return Err(RoomStoreError::Encode(
                "decision id, request digest, and actor are required".into(),
            ));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !Self::room_is_open_on(&tx, key)? {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let current = load_grant_on(&tx, key, resource_id)?.ok_or_else(|| {
            RoomStoreError::UnknownResourceGrant {
                room: key.clone(),
                resource_id: resource_id.to_string(),
            }
        })?;

        if let Some(prior) = consumed_decision_on(&tx, key, &input.decision_id)? {
            let same_resource: Option<String> = tx
                .query_row(
                    "SELECT resource_id FROM room_resource_decisions
                      WHERE room_id = ?1 AND decision_id = ?2",
                    params![key.as_str(), input.decision_id],
                    |row| row.get(0),
                )
                .optional()?;
            if prior != input.request_digest || same_resource.as_deref() != Some(resource_id) {
                return Err(RoomStoreError::DecisionReplayMismatch {
                    room: key.clone(),
                    decision_id: input.decision_id,
                });
            }
            drop(tx);
            return Ok((current, false, None));
        }

        let from = current.effective_status(now);
        let to = input.status;
        let allowed = matches!(
            (from, to),
            (ResourceStatus::Available, ResourceStatus::Suspended)
                | (ResourceStatus::Suspended, ResourceStatus::Available)
                | (ResourceStatus::Available, ResourceStatus::Revoked)
                | (ResourceStatus::Suspended, ResourceStatus::Revoked)
        );
        let noop = from == to && from != ResourceStatus::Revoked;
        if !allowed && !noop {
            return Err(RoomStoreError::ResourceStatusConflict {
                room: key.clone(),
                resource_id: resource_id.to_string(),
                from: from.as_str(),
                to: to.as_str(),
            });
        }
        tx.execute(
            "INSERT INTO room_resource_decisions (room_id, decision_id, resource_id, request_digest, consumed_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                key.as_str(),
                input.decision_id,
                resource_id,
                input.request_digest,
                fmt_ts(now)
            ],
        )?;
        if noop {
            tx.commit()?;
            return Ok((current, false, None));
        }
        let generation = current.generation.saturating_add(1);
        let (revoked_at, revoked_by) = if to == ResourceStatus::Revoked {
            (Some(fmt_ts(now)), Some(input.actor.clone()))
        } else {
            (None, None)
        };
        tx.execute(
            "UPDATE room_resource_grants
                SET status = ?3, generation = ?4, revoked_at = ?5, revoked_by = ?6
              WHERE room_id = ?1 AND resource_id = ?2",
            params![
                key.as_str(),
                resource_id,
                to.as_str(),
                generation.to_string(),
                revoked_at,
                revoked_by
            ],
        )?;
        let action = match to {
            ResourceStatus::Available => "room.resource.resumed",
            ResourceStatus::Suspended => "room.resource.suspended",
            ResourceStatus::Revoked => "room.resource.revoked",
        };
        let audit = audit_on(
            &tx,
            key,
            serde_json::json!({
                "type": action,
                "room_id": key.as_str(),
                "resource_id": resource_id,
                "display_name": current.display_name,
                "from": from.as_str(),
                "to": to.as_str(),
                "generation": generation.to_string(),
                "actor": input.actor,
                "decision_id": input.decision_id,
            }),
            now,
        )?;
        Self::touch_on(&tx, key, now)?;
        let updated = load_grant_on(&tx, key, resource_id)?.ok_or_else(|| {
            RoomStoreError::Encode("grant vanished inside its own transaction".into())
        })?;
        tx.commit()?;
        Ok((updated, true, Some(audit)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RoomStore;

    fn room() -> (SqliteRoomStore, RoomKey) {
        let mut s = SqliteRoomStore::open_in_memory().unwrap();
        let key = RoomKey::new("hq");
        s.create(key.clone(), "HQ", None, Utc::now()).unwrap();
        (s, key)
    }

    fn input(root: &str, decision: &str, digest: &str) -> GrantRoomResourceInput {
        GrantRoomResourceInput {
            display_name: "source".into(),
            local_root: root.into(),
            access_mode: ResourceAccessMode::Read,
            authorized_agent_member_ids: vec!["builder".into()],
            expires_at: None,
            granted_by: "operator-1".into(),
            decision_id: decision.into(),
            request_digest: digest.into(),
        }
    }

    fn status(to: ResourceStatus, decision: &str) -> SetResourceStatusInput {
        SetResourceStatusInput {
            status: to,
            actor: "operator-1".into(),
            decision_id: decision.into(),
            request_digest: format!("digest-{}", to.as_str()),
        }
    }

    #[test]
    fn closed_resources_refuse_grant_and_status_writes_even_after_retention() {
        let (mut s, key) = room();
        let (before, _, _) = s
            .grant_room_resource(&key, input("/tmp/root", "dec-1", "d1"), Utc::now())
            .unwrap();
        s.close_with_marker(&key, crate::RoomCloser::Operator("operator"), Utc::now())
            .unwrap();
        for cut in [false, true] {
            if cut {
                s.cut_closed_room(&key).unwrap();
            }
            let messages = s
                .get_including_closed(&key)
                .unwrap()
                .unwrap()
                .transcript
                .len();
            for request in [
                input("/tmp/root", "dec-1", "d1"),
                input("/tmp/another-root", "dec-2", "d2"),
            ] {
                assert!(matches!(
                    s.grant_room_resource(&key, request, Utc::now()),
                    Err(RoomStoreError::UnknownRoom(_))
                ));
            }
            for target in [
                ResourceStatus::Available,
                ResourceStatus::Suspended,
                ResourceStatus::Revoked,
            ] {
                assert!(matches!(
                    s.set_room_resource_status(
                        &key,
                        &before.resource_id,
                        status(target, "dec-status"),
                        Utc::now()
                    ),
                    Err(RoomStoreError::UnknownRoom(_))
                ));
            }
            assert_eq!(
                s.room_resource_grant(&key, &before.resource_id).unwrap(),
                Some(before.clone())
            );
            assert!(s
                .room_decision_consumed(&key, "dec-status")
                .unwrap()
                .is_none());
            assert_eq!(
                s.get_including_closed(&key)
                    .unwrap()
                    .unwrap()
                    .transcript
                    .len(),
                messages
            );
        }
    }

    #[test]
    fn the_access_ladder_is_ordered() {
        use ResourceAccessMode::*;
        assert!(Execute.allows(List) && Execute.allows(Write));
        assert!(Read.allows(List) && Read.allows(Read));
        assert!(!Read.allows(Write) && !List.allows(Read));
    }

    #[test]
    fn a_grant_is_generation_one_available_with_a_rootless_audit() {
        let (mut s, key) = room();
        let (g, created, audit) = s
            .grant_room_resource(&key, input("/tmp/secret-root", "dec-1", "d1"), Utc::now())
            .unwrap();
        assert!(created);
        assert_eq!(g.generation, 1);
        assert_eq!(g.status, ResourceStatus::Available);
        assert!(g.resource_id.starts_with("res-"));
        assert!(g.admits("builder", ResourceAccessMode::List, Utc::now()));
        assert!(!g.admits("builder", ResourceAccessMode::Write, Utc::now()));
        assert!(!g.admits("stranger", ResourceAccessMode::List, Utc::now()));
        let audit = audit.unwrap();
        assert!(!audit.body.contains("secret-root"), "{}", audit.body);
        assert!(audit.body.contains(&g.resource_id));
        assert_eq!(s.room_resource_grants(&key).unwrap(), vec![g.clone()]);
        assert_eq!(
            s.room_resource_grant(&key, &g.resource_id).unwrap(),
            Some(g)
        );
    }

    #[test]
    fn one_live_grant_per_root_until_revoked() {
        let (mut s, key) = room();
        let (g, ..) = s
            .grant_room_resource(&key, input("/tmp/root-a", "dec-1", "d1"), Utc::now())
            .unwrap();
        let err = s
            .grant_room_resource(&key, input("/tmp/root-a", "dec-2", "d2"), Utc::now())
            .unwrap_err();
        assert!(
            matches!(&err, RoomStoreError::ResourceRootAlreadyGranted { resource_id, .. } if resource_id == &g.resource_id),
            "{err:?}"
        );
        s.set_room_resource_status(
            &key,
            &g.resource_id,
            status(ResourceStatus::Revoked, "dec-3"),
            Utc::now(),
        )
        .unwrap();
        let (g2, created, _) = s
            .grant_room_resource(&key, input("/tmp/root-a", "dec-4", "d4"), Utc::now())
            .unwrap();
        assert!(created);
        assert_ne!(g2.resource_id, g.resource_id);
        assert_eq!(g2.generation, 1);
        assert_eq!(s.room_resource_grants(&key).unwrap().len(), 2);
    }

    #[test]
    fn an_expired_live_grant_reads_revoked_and_frees_its_root() {
        let (mut s, key) = room();
        let mut i = input("/tmp/root-x", "dec-1", "d1");
        i.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        let (g, ..) = s.grant_room_resource(&key, i, Utc::now()).unwrap();
        assert_eq!(
            g.status,
            ResourceStatus::Available,
            "the row says available"
        );
        assert_eq!(
            g.effective_status(Utc::now()),
            ResourceStatus::Revoked,
            "every check says revoked"
        );
        assert!(!g.admits("builder", ResourceAccessMode::List, Utc::now()));
        let (g2, created, _) = s
            .grant_room_resource(&key, input("/tmp/root-x", "dec-2", "d2"), Utc::now())
            .unwrap();
        assert!(created);
        let old = s
            .room_resource_grant(&key, &g.resource_id)
            .unwrap()
            .unwrap();
        assert_eq!(old.status, ResourceStatus::Revoked);
        assert_eq!(old.revoked_by.as_deref(), Some("expiry"));
        assert_eq!(
            old.generation, 2,
            "retiring an expired grant still bumps its generation"
        );
        assert_ne!(g2.resource_id, g.resource_id);
    }

    #[test]
    fn status_transitions_bump_generation_and_revoked_is_terminal() {
        let (mut s, key) = room();
        let (g, ..) = s
            .grant_room_resource(&key, input("/tmp/root-s", "dec-1", "d1"), Utc::now())
            .unwrap();
        let (g, changed, audit) = s
            .set_room_resource_status(
                &key,
                &g.resource_id,
                status(ResourceStatus::Suspended, "dec-2"),
                Utc::now(),
            )
            .unwrap();
        assert!(changed);
        assert_eq!(g.generation, 2);
        assert_eq!(g.status, ResourceStatus::Suspended);
        assert!(audit.unwrap().body.contains("room.resource.suspended"));
        // No-op transition consumes the decision without bumping.
        let (g, changed, audit) = s
            .set_room_resource_status(
                &key,
                &g.resource_id,
                status(ResourceStatus::Suspended, "dec-3"),
                Utc::now(),
            )
            .unwrap();
        assert!(!changed && audit.is_none());
        assert_eq!(g.generation, 2);
        let (g, ..) = s
            .set_room_resource_status(
                &key,
                &g.resource_id,
                status(ResourceStatus::Available, "dec-4"),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(g.generation, 3);
        let (g, ..) = s
            .set_room_resource_status(
                &key,
                &g.resource_id,
                status(ResourceStatus::Revoked, "dec-5"),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(g.generation, 4);
        assert_eq!(g.revoked_by.as_deref(), Some("operator-1"));
        let err = s
            .set_room_resource_status(
                &key,
                &g.resource_id,
                status(ResourceStatus::Available, "dec-6"),
                Utc::now(),
            )
            .unwrap_err();
        assert!(
            matches!(err, RoomStoreError::ResourceStatusConflict { .. }),
            "{err:?}"
        );
        let g = s
            .room_resource_grant(&key, &g.resource_id)
            .unwrap()
            .unwrap();
        assert_eq!(g.generation, 4, "a refused transition changes nothing");
    }

    #[test]
    fn replay_is_idempotent_and_cross_ledger_reuse_is_refused() {
        let (mut s, key) = room();
        let (g, ..) = s
            .grant_room_resource(&key, input("/tmp/root-r", "dec-1", "d1"), Utc::now())
            .unwrap();
        let (again, created, audit) = s
            .grant_room_resource(&key, input("/tmp/root-r", "dec-1", "d1"), Utc::now())
            .unwrap();
        assert!(!created && audit.is_none());
        assert_eq!(again, g);
        let err = s
            .grant_room_resource(&key, input("/tmp/root-r", "dec-1", "CHANGED"), Utc::now())
            .unwrap_err();
        assert!(matches!(err, RoomStoreError::DecisionReplayMismatch { .. }));
        // A status decision replayed against a different resource is refused.
        let (g2, ..) = s
            .grant_room_resource(&key, input("/tmp/root-r2", "dec-2", "d2"), Utc::now())
            .unwrap();
        s.set_room_resource_status(
            &key,
            &g.resource_id,
            status(ResourceStatus::Suspended, "dec-3"),
            Utc::now(),
        )
        .unwrap();
        let err = s
            .set_room_resource_status(
                &key,
                &g2.resource_id,
                status(ResourceStatus::Suspended, "dec-3"),
                Utc::now(),
            )
            .unwrap_err();
        assert!(matches!(err, RoomStoreError::DecisionReplayMismatch { .. }));
        // And a decision the profile ledger consumed cannot grant.
        s.put_room_profile(
            &key,
            crate::PutRoomProfileInput {
                repos: vec![],
                tools: vec![],
                credential_slots: vec![],
                default_resource_id: None,
                agent_defaults: Default::default(),
                updated_by: "operator-1".into(),
                decision_id: "dec-p".into(),
                request_digest: "dp".into(),
            },
            Utc::now(),
        )
        .unwrap();
        let err = s
            .grant_room_resource(&key, input("/tmp/root-p", "dec-p", "dp"), Utc::now())
            .unwrap_err();
        assert!(
            matches!(err, RoomStoreError::DecisionReplayMismatch { .. }),
            "{err:?}"
        );
        assert!(s.room_decision_consumed(&key, "dec-p").unwrap().is_some());
    }

    #[test]
    fn resource_audit_rows_round_trip_newest_first_and_carry_no_path_text() {
        let (mut s, key) = room();
        let fact = |outcome: &str, grant: Option<u64>| RoomResourceAuditInput {
            resource_id: "res-1".into(),
            agent_member_id: "builder".into(),
            binding_generation: 3,
            grant_generation: grant,
            op: "read".into(),
            relative_path_digest: "ab".repeat(32),
            bytes: 64,
            entries: 0,
            outcome: outcome.into(),
            actor: "agent".into(),
        };
        s.append_room_resource_audit(&key, fact("stale_generation", None), Utc::now())
            .unwrap();
        s.append_room_resource_audit(&key, fact("ok", Some(7)), Utc::now())
            .unwrap();
        let rows = s.room_resource_audit_recent(&key, 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].outcome, "ok");
        assert_eq!(rows[0].grant_generation, Some(7));
        assert_eq!(rows[0].binding_generation, 3);
        assert_eq!(rows[1].outcome, "stale_generation");
        assert_eq!(rows[1].grant_generation, None);
        assert!(rows[0].seq > rows[1].seq);
        assert!(matches!(
            s.room_resource_audit_recent(&RoomKey::new("nope"), 1)
                .unwrap_err(),
            RoomStoreError::UnknownRoom(_)
        ));
    }

    #[test]
    fn unknown_room_and_unknown_grant_are_errors() {
        let (mut s, key) = room();
        assert!(matches!(
            s.room_resource_grants(&RoomKey::new("nope")).unwrap_err(),
            RoomStoreError::UnknownRoom(_)
        ));
        assert!(matches!(
            s.set_room_resource_status(
                &key,
                "res-missing",
                status(ResourceStatus::Suspended, "d"),
                Utc::now()
            )
            .unwrap_err(),
            RoomStoreError::UnknownResourceGrant { .. }
        ));
    }
}
