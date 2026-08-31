# ocean-store — Durable Rooms + Federation Store

## Purpose

SQLite-backed durability for persistent rooms: rosters, transcripts, local
room-agent authority and approval decisions, room access projections, the
outbox, and the restart-safe federation core (S2 P2-A). One database file
(`rooms.db`), one owning crate.

## Ownership

- **Scope:** `crates/ocean-store/`
- **Parent contract:** `../AGENTS.md` — read it first
- **Owns:** room/roster/transcript persistence, `room_access` + `outbox`
  durability, local room-agent authority and approval decisions, local and
  mirrored room read cursors, federation credential custody, producer
  counters, confirmed ingest, trigger-claim journal
- **Does not own:** HTTP projection (daemon), federation network client,
  operator-key custody, agent sessions/memory, Longhouse titles

## Local Contracts

### Schema (private; consumers use APIs, never SQL)

- `rooms`, `room_participants`, `room_messages` — pre-federation durable rooms.
- `room_access` — per-room access projection (`state`, `confirmed_sequence`
  as canonical decimal u64 TEXT, `member_projection` JSON).
- `outbox` — locally-authored unconfirmed events with full producer tuple
  (`client_event_id`, `source_id`, `source_sequence`) and stable `position`.
- `room_attachments` — one row per room context file: server-minted
  `attachment_id`, display-only `filename`, the uploader's DECLARED
  `content_type`, the server-measured `byte_len` and `sha256`, `uploaded_by`,
  `uploaded_at`, and a snapshotted `on_behalf_of`. The BYTES are NOT here: the
  daemon owns them on disk (`ocean-daemon/src/room_attachments.rs`).
- `room_read_cursors` and `room_read_cursor_mirrors` — per-principal local and
  upstream-mirrored read positions as canonical decimal u64 TEXT. Mirror writes
  use `RoomReadCursorMirrorCas`: callers supply the previously observed mirror;
  mismatches return `Stale` without writing, including stale clears.
- `room_agent_bindings` — local, non-federated execution authority for one
  room-agent identity, including pinned definition digest, requested/granted
  capability intersection inputs, status, and canonical-decimal u64 TEXT
  generation.
- `room_agent_decisions` — immutable per-room replay ledger for every consumed
  operator decision id across authorization and status mutations.
  Re-authorization or a status decision may replace the binding's current
  decision but never makes an older approval id reusable.
- P2-A federation tables: `federation_instance` (singleton instance id),
  `room_federation` (bearer credential — PRIVATE), `room_member_bindings`
  (member→agent binding, `registration_key` PRIVATE, agent name unique per
  room), `producer_counters` (next source_sequence per room+member),
  `federated_events` (dedup + monotonic order index),
  `processed_room_triggers` (at-most-once trigger-claim journal), and
  `pending_redemptions` (v1.2 amendment table 7: pre-room `{redemption_id,
  bearer, invite_code}` custody, `invite_code` UNIQUE — bearer AND invite
  code are PRIVATE).

### Load-bearing invariants

- **u64 as canonical decimal TEXT.** Counters, cursors, and sequences are
  stored via `write_u64_text` and re-read only through
  `parse_canonical_u64_text`; noncanonical text fails closed. Never compare
  or `MAX()` these columns in SQL — lexicographic order is not numeric order.
- **Atomicity.** Every multi-row federation mutation runs in one IMMEDIATE
  transaction: `allocate_outbox_pending` (counter advance + outbox insert)
  and `ingest_confirmed_event` (dedup check, monotonic check, transcript
  append, dedup-index insert, full-tuple outbox removal, cursor advance,
  trigger claims) commit all-or-nothing.
- **Outbox removal requires the full producer tuple.** A confirmed event
  deletes an outbox row only when `client_event_id`, `source_id`, and
  `source_sequence` all match — never `client_event_id` alone.
- **Confirmed ingest is fail-closed.** Dedup cross-checks BOTH persisted
  copies: the `federated_events` index tuple must equal the parsed transcript
  `FederatedMessageMeta`, and that meta must equal the incoming event — every
  field, never raw JSON bytes or a column subset. Full three-way equality ⇒
  `IngestOutcome::Duplicate` no-op; any divergence (including index vs
  transcript), a missing/unreadable indexed transcript row, a
  `global_sequence` at or below the ordering baseline, or a missing access
  row ⇒ error and full rollback. The ordering baseline is
  max(last indexed sequence, persisted `room_access` cursor), so a
  bootstrap/recovery cursor set ahead of the local index rejects stale
  sequences and the cursor can never regress. Sequence gaps are accepted.
- **Trigger claims are at-most-once per (room, ledger event, target).**
  Claims commit inside the ingest transaction, only for locally-bound
  targets, and never for agent-authored rows.
- **Producer counters never reuse a sequence.** Allocation is transactional
  across connections; exhaustion at `u64::MAX` fails closed.
- **Credential custody.** `bearer_token`, `registration_key`, and pending
  redemption secrets (bearer + invite code) are never serialized into
  projections, transcripts, logs, or error messages. `RoomCredential` and
  `PendingRedemption` have redacting `Debug` and no `Serialize`. `open()`
  enforces owner-only (0600) mode on the DB and its sidecar files BEFORE any
  DB work and again after create/migration (Unix); filesystem errors fail
  closed except `NotFound`.
- **Pending redemptions never fork.** `get_or_insert_pending_redemption` is
  an atomic get-or-insert keyed by `invite_code`: an existing code returns
  the STORED triple and discards caller-supplied values. Promote takes exact
  inputs `(redemption_id, room, bearer, local_human_member_id)` and is
  all-or-nothing: install credential + delete pending in one transaction;
  exact replay after response loss is an idempotent no-op; every other state
  is corruption with no partial write.
- **Bindings are write-once per member.** A retried registration with the
  identical `(room, member, agent, key)` tuple is an idempotent no-op; the
  same member with a different agent or key fails closed — rebinding
  requires an explicit unbind. Registration-key derivation is frozen for
  P2-C (freeze v1.2 §3); this crate stores the column opaquely.
- **`update_room_access_safe` is the runtime refresh path**: it never touches
  the outbox and its cursor only advances. `replace_room_access` is
  destructive test seeding only.
- **Mirrored cursor writes are compare-and-swap.** `set_room_read_cursor_mirror`
  evaluates the expected prior mirror and write under one IMMEDIATE transaction.
  `Applied` returns the durable projection; `Stale` never mutates the row. Callers
  must handle newer concurrent `Some` values monotonically while reserving a
  clear for an expectation that still matches.
- **Room-agent authority is local and fail-closed.** Participant rows and
  federated descriptors are display data, never authorization. Only an active
  binding admits; stale authority can return active only through a fresh
  replay-safe authorization decision, never through a status transition.
  Authorization and status mutations share one immutable, room-wide decision
  namespace: exact retries are no-ops and cross-content reuse fails closed.
  They require an open room and keep replay validation, checked generation
  bump, mutation, returned projection, and commit in one IMMEDIATE transaction
  so a racing writer cannot change the authority a caller believes it
  approved. Closed rooms retain immutable audit history.
- **Room-agent audit and output commits use the authority transaction.**
  Authorization/status decisions persist their same-generation audit row in
  the mutation's IMMEDIATE transaction, stale detection records the checked
  transition and audit together, and a local agent reply commits only when the
  binding still matches the admitted generation and digest. Never split these
  into a mutation followed by a best-effort audit or a generation check
  followed by an unguarded output write.
- **Attachments are immutable, so the discipline is refusal, not CAS.** There is
  deliberately no `version` column on `room_attachments`: nothing amends an
  attachment, so a compare-and-swap guard would be decoration, and a decorative
  invariant is worse than an absent one. What holds instead: `attachment_id` is
  SERVER-minted so two uploads never contend for a row (which is also why there
  is no `AttachmentAlreadyExists` — a PK collision here means the daemon minted
  a duplicate UUID, a server fault that must surface as `Db`); the daemon writes
  and fsyncs the blob BEFORE `add_attachment` commits, so a row never points at
  bytes that do not exist; and `remove_attachment` treats zero rows affected as
  `UnknownAttachment`, never a silent success. `add_attachment` and
  `remove_attachment` each write their System transcript marker in the SAME
  transaction as the row.
- **An artifact title is refused blank, never stored blank.** `create_artifact`
  and `amend_artifact` both raise `ArtifactTitleBlank` on a whitespace-only
  title before any read, UPDATE, or transcript insert, so a refusal writes
  nothing and mints no System line — erasing a title is unrecoverable and the
  minted line would report the loss as an ordinary update (`bob updated '' (v2)`).
  Amend checks it AHEAD of the CAS: a malformed request is not a stale view, and
  winning the compare-and-swap would not make an untitled artifact acceptable. An
  ABSENT title still means untouched, which is how `room_summary`'s upsert amends
  a body alone. The guard lives here rather than on the route because
  `upsert_summary_artifact` reaches the store without passing one. A blank
  `artifact_id` is NOT checked here — that refusal is route-only in
  `persistent_rooms.rs`.
- **A declared content type is recorded and never trusted.**
  `room_attachments.content_type` is whatever the uploader claimed. It is stored
  verbatim and deliberately kept OUT of the transcript marker, whose body
  carries only the sanitized filename and a server-computed byte count — a
  client-supplied string with a newline in it can forge a transcript line in a
  naive renderer. `byte_len` and `sha256` are what the server measured; a
  negative stored `byte_len` fails closed on read.

## Work Guidance

- Add new durable state to this crate; do not let the daemon or a network
  client own SQL against `rooms.db`.
- New u64-valued columns must use the canonical-decimal TEXT helpers and get
  reopen + fail-closed tests.
- Keep new error variants coordinated with
  `ocean-daemon/src/persistent_rooms.rs::room_store_error_response`
  (exhaustive match).

## Verification

- `cargo test -p ocean-store`
- `cargo clippy -p ocean-store --all-targets -- -D warnings`
- Workspace impact: `cargo check --workspace` (daemon matches
  `RoomStoreError` exhaustively).

## Child devlog Index

- (none)
