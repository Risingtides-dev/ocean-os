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
- **Confirmed ingest requires an OPEN room.** `ingest_confirmed_event` guards
  on `room_is_open_on`, not on the room merely existing. It guarded on existence
  until the close route landed, and the gap was unreachable only because the one
  close production could reach was a call room's autoclose and `call:` rooms
  never federate. A route that closes any room makes it reachable, and the
  result is the worst transcript corruption available here: rows appended AFTER
  the close marker, into a room whose daemon SSE tails have ended and whose
  `/snapshot` says `closed: true`, so nothing watching can see them arrive —
  and, after a retention cut, the same path refilling from sequence 0 a
  transcript the operator was told was emptied. The daemon also stops the room's
  federation task on close so the refusal is not a reconnect loop, but the guard
  lives HERE too and not only there: a supervisor that is slow, restarted, or
  racing the close is exactly the case the invariant must survive, and only the
  store can decide it atomically with the write.
- **Retention eligibility is "a cut would remove something", never "closed long
  enough".** `rooms_closed_before` requires an EXISTS over the same four tables
  `cut_closed_room` empties. Without that clause a cut's own deliberate
  preservation of the `rooms` row and its `closed_at` makes every historical
  room eligible again on every sweep: an IMMEDIATE write transaction per room
  every interval forever, deleting nothing, with each empty no-op counted to the
  operator as another `rooms_cut`. The condition is derived from what the cut
  does rather than from a marker column, so the two cannot drift apart.
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
  transition and audit together, and local replies and sanitized local failure
  rows commit only when the binding still matches the admitted generation. A
  federated agent reply checks that same exact generation, allocates its durable
  producer sequence and Pending outbox row, and inserts the admission-correlated
  audit in one IMMEDIATE transaction. Never split these into a mutation followed
  by a best-effort audit or a generation check followed by an unguarded output
  write; provider stderr is not an input to the durable failure API.
- **Local Room ownership is not Agent ownership.** `room_local_roles` records
  the durable Local-room owner role and the non-secret operator principal that
  established it; `room_agent_owners` remains only Agent→Human ownership. The
  operator-authenticated bootstrap verifies a live Human, exact existing role,
  package-derived Agent participant, and Agent owner in one IMMEDIATE
  transaction. Its first applied mutation writes one content-minimal
  `room.agent.bootstrap` audit; exact replay writes no marker or audit. It never
  creates `room_agent_bindings` or consumes a decision, and federated rooms may
  never receive a local role row.
- **Durable Room history is backwards, bounded, and generation-bound.** The
  exact Active binding/generation check and newest-first room-scoped
  `seq < before_seq` query share one read transaction. Use a `limit + 1`
  sentinel for `has_more`; never count or load an unbounded transcript page.
- **A transcript page reads forward or backward, and the two are separate
  methods.** `transcript_page` walks `seq > after_seq ORDER BY seq`;
  `transcript_tail_page` walks `seq < before_seq ORDER BY seq DESC` and reverses
  the rows back to ascending, so a renderer never learns which one it got. Do NOT
  widen the forward signature to serve both — a fourth parameter there drags ten
  daemon call sites plus `room_summary.rs` in for a window only `/snapshot`
  serves. Both clamp through `clamp_transcript_limit` and both use the `limit + 1`
  sentinel; the cursors are named `next_seq` and `prev_seq` respectively so a
  backward cursor cannot be replayed as an `after_seq`. The `u64 -> i64` cursor
  guard is checked in both and lands opposite ways: forward, a cursor above
  `i64::MAX` is a terminal empty page, and backward it saturates to `i64::MAX`
  and is the newest page, because "before a number past the end" includes every
  row. `before_seq = 0` is empty — the first message's seq is 0. BOTH directions'
  soft-closed answers are the STORE's, through `transcript_page_including_closed`
  and `transcript_tail_page_including_closed` — each a `room_exists()` guard plus
  the private loader, gated on existence rather than openness, so an absent room
  is still `UnknownRoom` and openness never becomes a second paging contract. The
  daemon may NOT window `get_including_closed`'s record for either: that record is
  itself the oldest `MAX_TRANSCRIPT_LIMIT` rows, so windowing it answers the newest
  page of the first thousand and calls it the tail going backward, and going
  forward it can never report `has_more` at the cap — a frozen room past 1000 said
  `has_more: false, next_seq: None` on row 999 and every client paging forward
  stopped there. `RoomRecord::transcript_has_more` is not the repair for that and
  must not be OR-ed in: the same record still cannot produce row 1000, so a
  truthful flag over an unreachable page trades a silent stop for a loop that never
  advances. A marker says rows are missing; only a query returns them.
- **A `RoomRecord` says whether it is the whole log.** `load_record` builds the
  transcript from `load_transcript_page(key, None, MAX_TRANSCRIPT_LIMIT)` and
  keeps that page's `has_more` as `RoomRecord::transcript_has_more`, so both
  getters — `get` and the soft-closed audit `get_including_closed`, whose choice
  `/snapshot` serializes as its `closed` boolean — hand back a prefix that admits
  it is one. Populate the flag from the SAME page the rows came from; a second
  query, or a re-derivation, is a fact that can disagree with the rows beside it.
  `transcript.len() == MAX_TRANSCRIPT_LIMIT` is NOT the test and never was: a room
  that ENDS on the cap is indistinguishable by length from one cut at it, and only
  the `limit + 1` sentinel separates them. Do NOT add the resume cursor as a
  second field — it is `transcript.last().map(|m| m.seq)`, already on the struct.
  This is where the SQLite record deliberately stops mirroring
  `ocean_agent::rooms::RoomRecord`: that in-memory twin holds every row it was
  given, so the flag there could only ever be `false`. The flag's reader is the
  daemon's `room_get`, which serves the record's own transcript and derives
  `has_more`/`next_seq` from it rather than re-paging the identical rows — that
  route decodes up to the cap ONCE. It is not a substitute for a page anywhere the
  rows beyond the record are actually wanted; see the paging bullet above.
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
- **Every caller-supplied string a marker's PROSE quotes goes through
  `marker_prose`.** That is `ocean_core::bounded_prose` under this crate's
  `MARKER_FIELD_MAX_CHARS`, and the split is deliberate: the filter is a
  security rule shared with `ocean-daemon`'s workspace markers and lives in
  `ocean-core` because the dependency runs daemon -> store and neither crate
  can call the other; the bound is this crate's policy. Read the derivation
  there before widening what these lines drop, and do not re-inline a filter
  here — a second copy is exactly the drift the hoist removed. The same rule,
  same bound, now also guards `ocean_agent::rooms::RoomRegistry`, the dormant
  in-memory twin of this store.
- **The `room.agent.*` audit rows are records, not prose, and are NOT
  filtered.** They are `RoomMessageKind::System` bodies like the markers
  above, so the rule preceding this one would read as covering them; it
  deliberately does not. `bootstrap_local_room_agent`,
  `append_authorized_agent_*` and the admission/authority writers interpolate
  the ids that ARRIVED into a `serde_json` object, and an audit line that
  quietly repairs `owner_member_id` reports the attempt as something other
  than what was made — sanitizing a ledger to fix a rendering bug fixes it in
  the wrong crate and destroys the evidence on the way.
  `a_failure_marker_is_neutralized_and_the_audit_beside_it_is_not` pins both
  halves so neither side of the boundary drifts.
  Neutralizing an audit body belongs at the READ boundary, and the four routes
  that hand a transcript straight to a human client now do it. `ocean-daemon`'s
  `room_history_text` collapses the four audit `type` values that exist today —
  `room.agent.admission`, `.authority`, `.bootstrap`, `.output` — to
  `[room agent <kind> audit]`, and `projected_room_message` beside it applies
  that SAME function to `GET /v1/rooms/persistent/{key}`, `/transcript`,
  `/snapshot`, and the `/events` SSE tail. It was never one route: a client
  hydrates through `/snapshot` and then tails `/events`, so a fix covering the
  paged reads alone would have left the live path injectable while reading as
  closed. `audit_rows_reach_every_human_route_projected` drives a bootstrap
  `owner_member_id` — free-form, shape-checked nowhere — of
  `[click here](https://evil.co)` through all four and asserts this store still
  holds it verbatim: the rendering bug is fixed where it is read, and the ledger
  keeps the evidence.
  The two MODEL-facing reads of those rows are projected too, and closing only
  one of them would have moved the route rather than shut it. `POST
  /v1/rooms/persistent/{key}/summarize` shares `read_transcript_page`, and a
  convened agent's transcript tail comes from
  `authorized_room_transcript_context`, which pages this store raw and filters
  no message kind; both build a model prompt rather than a response, so both
  apply `room_history_text` themselves — in `summary_user_prompt` and in
  `build_room_prompt` — instead of going through `projected_room_message`.
  `read_transcript_page` stays the one raw paging implementation its consumers
  share, and the projection sits at each point a prompt or a response is SHAPED.
  That closes the model-laundered route: the audit metadata
  `crates/ocean-daemon/AGENTS.md` says never reaches a model reaches neither
  prompt, so it cannot ride back out through the summary artifact ocean-surface
  markdown-renders, nor through a convened reply `append_room_agent_reply`
  appends to the room and the same surface renders.
  `a_bootstrap_audit_row_reaches_the_model_as_a_label_not_as_its_ids` drives a
  real `bootstrap_local_room_agent` row through summarize and asserts the prompt
  carries `[room agent bootstrap audit]` and none of the package, principal, or
  `owner_member_id` strings the body interpolates;
  `a_convened_agents_transcript_tail_projects_an_audit_row` pins the same for the
  convened-agent prompt, off a tail read back through `transcript_page`.
  Two things that boundary still does NOT cover, named because a doc claiming
  otherwise is how the next one gets missed. First, the match is a closed
  whitelist of four literal strings and not a `room.agent.` prefix; its fallback
  arm hands anything else through raw on the human, agent, summarizer AND
  convened-agent paths with no test going red, so a new audit writer lands
  unprojected and silent — add its `type` there in the same commit that adds the
  row. Second, only the BODY is projected: all four renderers still interpolate
  `author_id` verbatim, which is the same unbounded caller-supplied identity,
  and it is bounded at the write side where the id is minted rather than in any
  one of them.
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
- **Closing a room is an act somebody did, and the transcript says so.**
  `close_with_marker` is the ROUTE's close and `RoomStore::close` is the CALL
  path's; they are deliberately separate rather than one with a flag. `CallEnded`
  fires with no actor to name, so its close soft-sets `closed_at`, returns the
  pre-close record, and mints nothing. A member or operator closing a room is an
  act, and a room that simply stops answering with nothing in its log saying why
  or at whose hand is the same evidence gap the join, artifact and attachment
  markers exist to close. `close_with_marker` therefore takes one IMMEDIATE
  transaction over the openness check, the marker insert (which reads
  `MAX(seq)+1`) and the `closed_at` write: those three are dependent, and a
  concurrent writer between the marker and the update leaves a room whose
  transcript announces a close that did not happen — a state no caller can
  detect from the return value, which is why the guards are
  `room_is_open_on`/`room_exists_on` against the TRANSACTION and not the
  `&self` reads beside them. A room that is not open is `UnknownRoom`, the
  answer every other mutation here already gives, so a second close is a 404
  and never a silent success. `RoomCloser` is an enum and not an id plus a
  boolean: a `Member` is roster-checked inside that transaction
  (`RoomCloserNotInRoster`) because the marker names them, and an `Operator` is
  deliberately NOT — operator authority is over the daemon rather than
  membership in one room — and a flag deciding which check runs is the shape
  that lets an unchecked member id through when a call site passes the wrong
  argument. Both ids go through `marker_prose` like every other caller-supplied
  string a marker's prose quotes; the store still accepts whatever an in-process
  caller hands it, so no read may assume the daemon's `validate_member_id` ran.
- **Retention cuts a CLOSED room's content and keeps its row.** `cut_closed_room`
  removes `messages`, `room_attachments`, both read-cursor tables and
  `federated_events` for one room in one IMMEDIATE transaction, refusing an OPEN
  room with `RoomNotClosed` having written nothing — eligibility is measured
  from the close, so a live room is never cuttable however old it is, and that
  guard is inside the transaction so a racing reopen cannot pass it. What stays
  is as load-bearing as what goes. The `rooms` row stays because deleting it
  would `ON DELETE CASCADE` the whole room away in one statement: the row is the
  only durable record that the room existed and when it closed, `/snapshot`
  answers `closed: true` off it, and a client holding a link to a cut room must
  learn it is frozen rather than that it never was. `federated_events` goes
  because a surviving index row is worse than none — `ingest_confirmed_event`
  cross-checks the index tuple against the TRANSCRIPT row it names, and an entry
  pointing at a removed `local_seq` makes that read fail closed and the room
  stops ingesting. Dropping the index does not reopen the dedup window: the
  ordering baseline is `max(last indexed sequence, persisted room_access
  cursor)` and the access cursor is deliberately retained. Blob bytes are NOT
  this crate's to delete: `cut_closed_room` returns the attachment ids and the
  caller unlinks them AFTER the commit, the same order `remove_attachment` uses.
  `rooms_closed_before` parses each `closed_at` in Rust rather than comparing
  RFC3339 TEXT in SQL — UTC RFC3339 happens to sort lexicographically, and
  "happens to" is the wrong footing for a query whose false positives delete
  transcripts — and a value it cannot parse is SKIPPED, never cut.
- **A declared content type is recorded and never trusted.**
  `room_attachments.content_type` is whatever the uploader claimed. It is stored
  verbatim and deliberately kept OUT of the transcript marker, whose body
  carries only the sanitized filename and a server-computed byte count —
  sanitized meaning `marker_prose`, above. A client-supplied string can
  otherwise forge a transcript line twice over: a newline forges a row in
  anything that splits on lines, and markdown's own link syntax — a bracketed
  label followed by a parenthesised destination — forges an anchor in
  ocean-surface, which puts system rows through a markdown tokenizer and is
  not the naive renderer this bullet used to name. Spelled out rather than
  written literally because `cargo xtask docs-check` reads the literal form as
  a local link and reds on it, backticks included. `byte_len` and `sha256`
  are what the server measured; a negative stored `byte_len` fails closed on
  read.

- **The room-metrics projection is read-only, transcript-free, and open-room
  only.** `room_metrics_projection` answers the daemon's §4.1 metrics sample in
  two aggregate queries: per-room access state (LEFT JOIN `room_access`, so an
  absent row projects `Local` — the same answer `room_access` gives for that
  absence, which is what stops the projection and the per-room read disagreeing
  about what "no row" means), and per-room outbox depth by state plus the
  `client_event_id` of the lowest-`position` row. It exists because the obvious
  enumeration cannot serve a scrape: `list`/`list_page` call `load_record` per
  room, and that loads the roster plus the oldest `MAX_TRANSCRIPT_LIMIT`
  transcript rows, so counting five access states over a hundred rooms would
  decode up to a hundred thousand messages. Both halves filter
  `closed_at IS NULL`: a closed room is not a live access state and its outbox
  is not a backlog anyone is draining. `MIN(position)`/`ORDER BY position` is
  legal numeric ordering here because `position` is a real INTEGER column — this
  is NOT an exception to the canonical-decimal u64 TEXT rule above, which still
  bans ordering and `MAX()` on those TEXT columns. Only the row's id travels,
  never its payload; an outbox payload is a room message.

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
