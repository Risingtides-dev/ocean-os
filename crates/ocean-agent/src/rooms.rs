//! Persistent Room lifecycle store (OCEAN-65).
//!
//! The data-model half — the `Room` struct, `RoomParticipant`, `RoomTriggerPolicy`,
//! the transcript message types, and trigger evaluation — lives in `ocean-core`
//! (OCEAN-39 + OCEAN-65). This module is the *lifecycle*: an in-memory store that
//! owns the rooms, their rosters, and their transcripts, and a small set of
//! mutating operations over them.
//!
//! It deliberately mirrors `ocean_longhouse::LonghouseRegistry`: a plain struct
//! held behind the daemon's `std::sync::Mutex`, with synchronous methods that
//! return cloned snapshots. The guard is always dropped before any `await` in the
//! daemon, so a std `Mutex` is correct and never blocks the async scheduler.
//!
//! In-memory is intentional for this ticket. SQLite-backed persistence (via
//! `ocean-store`) is a separate future ticket; nothing here assumes durability
//! beyond process lifetime, and the public API is shaped so a persistent backend
//! can be slotted in behind it without callers changing.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use ocean_core::{
    Room, RoomKey, RoomMessage, RoomMessageKind, RoomParticipant, RoomParticipantKind,
    RoomTriggerPolicy,
};

/// A persistent room plus its transcript. The transcript is held here rather
/// than on `ocean_core::Room` so the OCEAN-39 `Room` serde contract (and its
/// roundtrip test) stays untouched — additive only.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomRecord {
    /// The persistent room entity (id, name, roster, timestamps, trigger policy).
    pub room: Room,
    /// Append-only transcript of room events, in `seq` order.
    pub transcript: Vec<RoomMessage>,
}

/// Error returned by store operations that can fail on caller input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomStoreError {
    /// A room key was empty or otherwise malformed.
    BadKey(String),
    /// No room exists for the given key.
    UnknownRoom(RoomKey),
    /// A room with this key already exists (on create).
    AlreadyExists(RoomKey),
    /// No participant with the given id is in the room (on remove).
    UnknownParticipant { room: RoomKey, participant: String },
}

impl std::fmt::Display for RoomStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadKey(k) => write!(f, "invalid room key '{k}'; must be non-empty"),
            Self::UnknownRoom(k) => write!(f, "no room with key '{k}'"),
            Self::AlreadyExists(k) => write!(f, "room '{k}' already exists"),
            Self::UnknownParticipant { room, participant } => {
                write!(f, "room '{room}' has no participant '{participant}'")
            }
        }
    }
}

impl std::error::Error for RoomStoreError {}

/// An in-memory, daemon-owned store of persistent [`Room`]s and their
/// transcripts (OCEAN-65). Held behind a `std::sync::Mutex` on the daemon's
/// `AppState`, exactly like `LonghouseRegistry`.
#[derive(Debug, Default)]
pub struct RoomRegistry {
    rooms: HashMap<RoomKey, RoomRecord>,
    /// Per-room monotonically increasing transcript sequence counter. Survives
    /// transcript reads; only `append_message` advances it.
    next_seq: HashMap<RoomKey, u64>,
}

impl RoomRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many rooms are tracked.
    pub fn len(&self) -> usize {
        self.rooms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rooms.is_empty()
    }

    /// Create a new persistent room. Fails if the key is empty or already taken.
    /// The roster starts empty (callers join participants explicitly) and
    /// `created_at == updated_at == now`.
    pub fn create(
        &mut self,
        key: RoomKey,
        name: impl Into<String>,
        trigger_policy: Option<RoomTriggerPolicy>,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord, RoomStoreError> {
        if key.as_str().trim().is_empty() {
            return Err(RoomStoreError::BadKey(key.0));
        }
        if self.rooms.contains_key(&key) {
            return Err(RoomStoreError::AlreadyExists(key));
        }
        let mut room = Room::new(key.clone(), name, now);
        room.trigger_policy = trigger_policy;
        let record = RoomRecord {
            room,
            transcript: Vec::new(),
        };
        self.rooms.insert(key.clone(), record.clone());
        self.next_seq.insert(key, 0);
        Ok(record)
    }

    /// One room record (room + transcript) by key, cloned out.
    pub fn get(&self, key: &RoomKey) -> Option<RoomRecord> {
        self.rooms.get(key).cloned()
    }

    /// All rooms (without transcripts duplicated — the full record is cloned),
    /// most-recently-updated first, ties broken by key for determinism.
    pub fn list(&self) -> Vec<Room> {
        let mut out: Vec<Room> = self.rooms.values().map(|r| r.room.clone()).collect();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.0.cmp(&b.id.0)));
        out
    }

    /// Update a room's mutable metadata (name and/or trigger policy). Bumps
    /// `updated_at`. Pass `None` to leave a field unchanged.
    pub fn update(
        &mut self,
        key: &RoomKey,
        name: Option<String>,
        trigger_policy: Option<Option<RoomTriggerPolicy>>,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord, RoomStoreError> {
        let record = self
            .rooms
            .get_mut(key)
            .ok_or_else(|| RoomStoreError::UnknownRoom(key.clone()))?;
        if let Some(name) = name {
            record.room.name = name;
        }
        if let Some(policy) = trigger_policy {
            record.room.trigger_policy = policy;
        }
        record.room.updated_at = now;
        Ok(record.clone())
    }

    /// Close (remove) a room and its transcript. Returns the removed record so
    /// the caller can emit a closing event. In-memory only: a future persistent
    /// backend would soft-close instead.
    pub fn close(&mut self, key: &RoomKey) -> Result<RoomRecord, RoomStoreError> {
        self.next_seq.remove(key);
        self.rooms
            .remove(key)
            .ok_or_else(|| RoomStoreError::UnknownRoom(key.clone()))
    }

    /// Add a participant to a room's roster and append a `ParticipantJoined`
    /// transcript marker. Idempotent on id: re-adding an existing id replaces
    /// the entry rather than duplicating it. Bumps `updated_at`.
    pub fn add_participant(
        &mut self,
        key: &RoomKey,
        participant: RoomParticipant,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord, RoomStoreError> {
        // Resolve existence first; transcript append re-borrows the entry.
        if !self.rooms.contains_key(key) {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let body = format!("{} joined", participant.display_name);
        {
            let record = self.rooms.get_mut(key).expect("checked above");
            record.room.participants.retain(|p| p.id != participant.id);
            record.room.participants.push(participant.clone());
            record.room.updated_at = now;
        }
        self.append_message_inner(
            key,
            participant.id.clone(),
            participant.kind,
            RoomMessageKind::ParticipantJoined,
            body,
            now,
        )?;
        Ok(self.rooms.get(key).expect("checked above").clone())
    }

    /// Remove a participant from a room's roster by id and append a
    /// `ParticipantLeft` marker. Fails if the participant isn't present. Bumps
    /// `updated_at`.
    pub fn remove_participant(
        &mut self,
        key: &RoomKey,
        participant_id: &str,
        now: DateTime<Utc>,
    ) -> Result<RoomRecord, RoomStoreError> {
        let (kind, display) = {
            let record = self
                .rooms
                .get_mut(key)
                .ok_or_else(|| RoomStoreError::UnknownRoom(key.clone()))?;
            let Some(pos) = record
                .room
                .participants
                .iter()
                .position(|p| p.id == participant_id)
            else {
                return Err(RoomStoreError::UnknownParticipant {
                    room: key.clone(),
                    participant: participant_id.to_string(),
                });
            };
            let removed = record.room.participants.remove(pos);
            record.room.updated_at = now;
            (removed.kind, removed.display_name)
        };
        self.append_message_inner(
            key,
            participant_id.to_string(),
            kind,
            RoomMessageKind::ParticipantLeft,
            format!("{display} left"),
            now,
        )?;
        Ok(self.rooms.get(key).expect("checked above").clone())
    }

    /// Append a chat/system message to a room's transcript, assigning the next
    /// room-scoped `seq`. Returns the stored message (with its assigned seq).
    /// Bumps the room's `updated_at`.
    pub fn append_message(
        &mut self,
        key: &RoomKey,
        author_id: impl Into<String>,
        author_kind: RoomParticipantKind,
        kind: RoomMessageKind,
        body: impl Into<String>,
        now: DateTime<Utc>,
    ) -> Result<RoomMessage, RoomStoreError> {
        if !self.rooms.contains_key(key) {
            return Err(RoomStoreError::UnknownRoom(key.clone()));
        }
        let msg =
            self.append_message_inner(key, author_id.into(), author_kind, kind, body.into(), now)?;
        if let Some(record) = self.rooms.get_mut(key) {
            record.room.updated_at = now;
        }
        Ok(msg)
    }

    /// Read a room's transcript, optionally only entries with `seq > after_seq`
    /// (for live-tail clients). Returns entries in `seq` order.
    pub fn transcript(
        &self,
        key: &RoomKey,
        after_seq: Option<u64>,
    ) -> Result<Vec<RoomMessage>, RoomStoreError> {
        let record = self
            .rooms
            .get(key)
            .ok_or_else(|| RoomStoreError::UnknownRoom(key.clone()))?;
        let out = match after_seq {
            Some(after) => record
                .transcript
                .iter()
                .filter(|m| m.seq > after)
                .cloned()
                .collect(),
            None => record.transcript.clone(),
        };
        Ok(out)
    }

    /// The room's current trigger policy, if any — convenience for the daemon's
    /// trigger evaluation wiring point.
    pub fn trigger_policy(&self, key: &RoomKey) -> Option<RoomTriggerPolicy> {
        self.rooms
            .get(key)
            .and_then(|r| r.room.trigger_policy.clone())
    }

    /// Internal: append without bumping `updated_at` (callers that already bumped
    /// it for a roster change reuse this so we don't double-touch). Assumes the
    /// room exists.
    fn append_message_inner(
        &mut self,
        key: &RoomKey,
        author_id: String,
        author_kind: RoomParticipantKind,
        kind: RoomMessageKind,
        body: String,
        now: DateTime<Utc>,
    ) -> Result<RoomMessage, RoomStoreError> {
        let seq = {
            let counter = self.next_seq.entry(key.clone()).or_insert(0);
            let s = *counter;
            *counter += 1;
            s
        };
        let msg = RoomMessage {
            seq,
            author_id,
            author_kind,
            kind,
            body,
            created_at: now,
            federated: None,
        };
        let record = self
            .rooms
            .get_mut(key)
            .ok_or_else(|| RoomStoreError::UnknownRoom(key.clone()))?;
        record.transcript.push(msg.clone());
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ocean_core::{evaluate_trigger_policy, RoomTriggerEvent};

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn human(id: &str, name: &str) -> RoomParticipant {
        RoomParticipant {
            id: id.into(),
            kind: RoomParticipantKind::Human,
            display_name: name.into(),
        }
    }

    #[test]
    fn create_get_list() {
        let mut reg = RoomRegistry::new();
        assert!(reg.is_empty());
        let key = RoomKey::new("map-fix");
        let rec = reg.create(key.clone(), "Map Fix", None, now()).unwrap();
        assert_eq!(rec.room.name, "Map Fix");
        assert!(rec.transcript.is_empty());

        let got = reg.get(&key).unwrap();
        assert_eq!(got.room.id, key);

        // Duplicate create fails.
        assert!(matches!(
            reg.create(key.clone(), "Dup", None, now()),
            Err(RoomStoreError::AlreadyExists(_))
        ));

        // A second room shows up in the list.
        reg.create(RoomKey::new("docs"), "Docs", None, now())
            .unwrap();
        assert_eq!(reg.list().len(), 2);
    }

    #[test]
    fn empty_key_rejected() {
        let mut reg = RoomRegistry::new();
        assert!(matches!(
            reg.create(RoomKey::new("   "), "x", None, now()),
            Err(RoomStoreError::BadKey(_))
        ));
    }

    #[test]
    fn participant_join_leave_writes_transcript() {
        let mut reg = RoomRegistry::new();
        let key = RoomKey::new("r1");
        reg.create(key.clone(), "R1", None, now()).unwrap();

        let rec = reg
            .add_participant(&key, human("john", "John"), now())
            .unwrap();
        assert_eq!(rec.room.participants.len(), 1);
        // join wrote a ParticipantJoined marker at seq 0.
        assert_eq!(rec.transcript.len(), 1);
        assert_eq!(rec.transcript[0].kind, RoomMessageKind::ParticipantJoined);

        // Re-adding the same id does not duplicate the roster entry.
        reg.add_participant(&key, human("john", "John"), now())
            .unwrap();
        assert_eq!(reg.get(&key).unwrap().room.participants.len(), 1);

        let rec = reg.remove_participant(&key, "john", now()).unwrap();
        assert!(rec.room.participants.is_empty());
        assert_eq!(
            rec.transcript.last().unwrap().kind,
            RoomMessageKind::ParticipantLeft
        );

        // Removing a non-member fails.
        assert!(matches!(
            reg.remove_participant(&key, "ghost", now()),
            Err(RoomStoreError::UnknownParticipant { .. })
        ));
    }

    #[test]
    fn transcript_append_read_and_seq_ordering() {
        let mut reg = RoomRegistry::new();
        let key = RoomKey::new("r1");
        reg.create(key.clone(), "R1", None, now()).unwrap();

        let m0 = reg
            .append_message(
                &key,
                "john",
                RoomParticipantKind::Human,
                RoomMessageKind::Message,
                "hello",
                now(),
            )
            .unwrap();
        let m1 = reg
            .append_message(
                &key,
                "ocean",
                RoomParticipantKind::Agent,
                RoomMessageKind::Message,
                "on it",
                now(),
            )
            .unwrap();
        assert_eq!(m0.seq, 0);
        assert_eq!(m1.seq, 1);

        let all = reg.transcript(&key, None).unwrap();
        assert_eq!(all.len(), 2);
        // after_seq tail returns only later entries.
        let tail = reg.transcript(&key, Some(0)).unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 1);
    }

    #[test]
    fn unknown_room_errors() {
        let reg = RoomRegistry::new();
        assert!(matches!(
            reg.transcript(&RoomKey::new("nope"), None),
            Err(RoomStoreError::UnknownRoom(_))
        ));
    }

    #[test]
    fn close_removes_room() {
        let mut reg = RoomRegistry::new();
        let key = RoomKey::new("r1");
        reg.create(key.clone(), "R1", None, now()).unwrap();
        reg.close(&key).unwrap();
        assert!(reg.get(&key).is_none());
        assert!(matches!(
            reg.close(&key),
            Err(RoomStoreError::UnknownRoom(_))
        ));
    }

    #[test]
    fn trigger_policy_fires_on_matching_mention_event() {
        // End-to-end: store a room with on_mention, then evaluate a mention
        // event against its policy — proves the wiring point's inputs line up.
        let mut reg = RoomRegistry::new();
        let key = RoomKey::new("r1");
        reg.create(
            key.clone(),
            "R1",
            Some(RoomTriggerPolicy {
                on_mention: true,
                ..Default::default()
            }),
            now(),
        )
        .unwrap();

        let policy = reg.trigger_policy(&key);
        let decision = evaluate_trigger_policy(
            policy.as_ref(),
            &RoomTriggerEvent::Mention {
                participant_id: "ocean".into(),
            },
        );
        assert!(decision.should_convene);
        assert_eq!(decision.target_participant.as_deref(), Some("ocean"));
    }
}
