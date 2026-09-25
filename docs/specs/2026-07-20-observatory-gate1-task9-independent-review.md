# Ocean Observatory — Gate 1 Task 9 Independent Review

**Date:** 2026-07-20
**Target:** `main` @ `6ba2cef` (clean working tree)
**Scope:** Gate 1 manifest §8 Task 9 — independent security, protocol, and
architecture review of the Ocean Observatory implementation (tasks 2–6 plus the
consolidated daemon composition).
**Method:** Six independent fresh-context category passes (auth, redaction,
persistence, protocol, admission/binding, extension-invariant/Gate-0) run
read-only against source, followed by consolidation and spot-verification of
every gating claim by the recording agent. Per the review-independence norm
recorded in `events.md` (task-8 entry), no pass was run by the tasks 4/5/6
implementer session; every pass re-derived its conclusions from the code, and
each `file:line` claim below was re-checked before inclusion.
**References:**
[`2026-07-17-observatory-gate0-decisions.md`](2026-07-17-observatory-gate0-decisions.md),
[`2026-07-17-observatory-gate1-implementation-manifest.md`](2026-07-17-observatory-gate1-implementation-manifest.md)
(§8 Task 9 checklist, §10 non-acceptance conditions, §11 deviation D1),
[`2026-07-17-ocean-observatory-architecture.md`](2026-07-17-ocean-observatory-architecture.md).

## Verdict summary

| Category | Verdict | Gating findings |
|---|---|---|
| Auth | **PASS** | none |
| Redaction (D1 structural allow-list) | **PASS** | none |
| Persistence | **CONCERN** | G3, G4 |
| Protocol | **CONCERN** | G1, G2, G5 |
| Admission / binding | **PASS (library) — NOT WIRED (V1 scope)** | none for V1; gate conditions for the future turn path |
| Extension invariant + Gate 0 conformance | **PASS** | none |

**Overall: the review is complete; the implementation is not yet renderer-ready.**
Five gating findings (G1–G5) must be repaired and delta-reviewed before the
production Ocean Floor renderer consumes the snapshot/replay/live contracts.
Auth, redaction, admission library logic, and the extension-ownership invariant
are production-quality as landed.

## Gating findings (repair before Surface renderer work)

- **G1 — `snapshot_at` is not a point-in-time projection.**
  `crates/ocean-observatory/src/store.rs:219-248` reads `latest_cursor()` (or
  the caller's `at`) as the watermark, then selects **all current**
  `execution_nodes`/`execution_edges` rows with no cursor filter, and the
  watermark is read before/outside the DB lock. The projection is destructive
  (`ON CONFLICT DO UPDATE`, `store.rs:79`), so historical state is
  unrecoverable. Consequences: `GET /v1/observatory/snapshot?at=<old>` returns
  current state mislabeled with the old watermark, and even `at=None` can label
  a projection newer than its declared watermark. Snapshot + tail from the
  watermark double-applies events, breaking the manifest's
  snapshot-plus-tail ≡ full-replay contract (§4.2, §7.1). Verified by two
  independent passes and by direct re-read.
  **Repair:** either make the projection cursor-filtered (requires persisted
  `first_cursor`/`last_cursor` per node per §4.1) or pin Gate 1 semantics:
  read the watermark inside the DB lock and reject `at < latest` explicitly
  instead of mislabeling. Add a snapshot+tail equivalence test.

- **G2 — Replay wire shape diverges from manifest §7.3.**
  `ReplayEvent` (`crates/ocean-observatory/src/snapshot.rs:262-285`; daemon
  mapping `crates/ocean-daemon/src/observatory.rs:671-685`) emits only
  `cursor/event_id/schema_version/occurred_at/kind/payload`, omitting the
  specified `recorded_at`, `truth`, `producer`, `topology`, `correlation`, and
  `visibility` fields. This is an unapproved wire-contract deviation in
  manifest §10.7 territory (the task-5 landing already flagged the smaller
  snapshot `capabilities` omission for this review).
  **Repair:** emit the full §7.3 envelope (the truth/provenance fields matter
  for the attestation seam, see F8) or amend the manifest with an approved
  deviation. Also reconcile §7.2's stated 30 s keepalive with the implemented
  3 s keepalive (stricter; text fix).

- **G3 — Retention is never enforced at runtime, and its gate permits
  unbounded growth.** `apply_retention` (`store.rs:174-216`) has no production
  call site — only a daemon test (`observatory.rs:1115`). The Gate 0 7-day /
  1-GiB bounds are dead code in production. The gate is also all-or-nothing
  (any non-terminal execution blocks all pruning, `store.rs:181-183`) rather
  than the manifest §4.2 min-nonterminal-cursor cutoff, so one stuck `running`
  row would block pruning forever.
  **Repair:** schedule retention (e.g. hourly daemon task), replace the gate
  with the manifest cutoff, and measure real DB size rather than summing
  envelope lengths.

- **G4 — Startup cursor initialization ignores durable watermarks; cursor
  reuse after full prune + restart.** `ObservatoryStore::open`
  (`store.rs:50-58`) seeds from `MAX(cursor) FROM observatory_events` only. A
  retention pass that prunes every row (idle daemon, all events older than 7
  days) followed by a restart resets the cursor to 1 — exactly the §2.3
  violation the startup rule exists to prevent. Latent today only because G3
  keeps retention from ever running; fixing G3 activates this bug.
  **Repair:** seed from `MAX(MAX(events.cursor), watermarks.snapshot_watermark,
  retention_boundary)` and add a reopen-after-full-prune regression test. G3
  and G4 must land together.

- **G5 — 401 rejections lack the §7.4 headers and §7.1 error body.**
  `ObservatoryAuth`'s rejection is a bare `StatusCode`
  (`crates/ocean-daemon/src/observatory_auth.rs:67-76`): no
  `Cache-Control`/`X-Observatory-*` headers and no
  `{error, message, http_status}` JSON body, contrary to §7.4 ("all
  Observatory routes") and the §7.1 401 shape.
  **Repair:** implement a typed `IntoResponse` rejection carrying headers and
  body; extend the all-failures-401 test to assert both.

## Non-gating findings (schedule; none block the V1 record-only boundary)

Ordered by severity; category in brackets.

- **F1 (major, persistence):** blocking rusqlite calls run on the Tokio
  executor — durability pump (`crates/ocean-daemon/src/main.rs:893`), SSE tail
  poll every 150 ms per client (`observatory.rs:417`), snapshot/replay handlers
  (`observatory.rs:284,631`). No `spawn_blocking` anywhere in the observatory
  path. Move store calls off the executor or document the contract.
- **F2 (major, persistence):** schema diverges from §4.1 — missing
  `kind`/`producer_id`/`visibility`/`schema_version`/`created_at` event
  columns, all mandated indexes, FK constraints (the enabled
  `PRAGMA foreign_keys` is inert), and node `first_cursor`/`last_cursor` needed
  for the G1 repair. Migrate before Gate 2 consumers depend on the reduced
  shape.
- **F3 (minor, auth):** V1 routes discard the principal and never assert
  `ObserverScope::Summary` (`observatory.rs:218,347,524`). Not exploitable
  today (only the daemon mints, Summary only), but reject non-Summary scopes in
  the extractor before any future mint path lands.
- **F4 (minor, protocol):** `events_page` `complete` can never be true when
  `through < latest` (`store.rs:163-171`), contradicting §7.3; fix to `!more`
  and add a `through`-bounded test.
- **F5 (minor, protocol):** `stream.gap` frame reuses the post-gap event's
  cursor as its SSE `id:` (duplicate id at the seam,
  `observatory.rs:444-455`); give it a distinct or absent id.
- **F6 (minor, protocol):** `continuation_url` interpolates the raw `filter`
  unencoded (`observatory.rs:655-662`); charset-restrict or URL-encode filter
  values.
- **F7 (minor, admission):** re-admission leaves prior unconsumed binding
  tokens valid (`admission.rs:128-137`); idempotency lookup runs after
  parent-phase validation, so a legitimate retry after parent completion fails
  `InvalidParentPhase` instead of returning recorded IDs
  (`admission.rs:99-104`); idempotency records are memory-only, so a restart
  replay double-registers (manifest §5.3 sketched durable storage).
- **F8 (minor, invariant):** the snapshot route hardcodes
  `TruthProvenance::HostObserved` for all nodes/edges (`observatory.rs:341,359`).
  Harmless while admission is unwired; must be plumbed before the attestation
  seam connects or attested children will be mislabeled.
- **F9 (minor, redaction):** `forbidden_variants_are_skipped` pins 6 of 10
  skipped runtime variants; add `ComponentRender`, `SurfacePatch`,
  `SlackCanvas`, and `SessionConfigChanged` so a future refactor moving one to
  a mapped arm fails tests.
- **F10 (minor, persistence):** no `PRAGMA busy_timeout`; cursor is burned if
  `tx.commit()` fails after allocation (`store.rs:68-90`) with no persisted
  gap record; `retention_archive.from_cursor` hardcoded to 1 (`store.rs:208`);
  §4.3's 60 s/16 MiB checkpoint task unimplemented.
- **F11 (nit, auth):** no parent-directory fsync after secret hardlink / token
  rename; rotation failure only logs (add a counter + escalated log);
  `expires_at == issued_at` accepted; unix-only secret handling.
- **F12 (nit, invariant):** Gate 0 decision 7 says the restart sweep marks
  stale executions `interrupted`; the schema maps them to `Canceled`
  (`observatory_adapter.rs:273-303`). Semantically equivalent; reconcile the
  record. `?scope=` on the events route is silently ignored
  (`observatory.rs:355-359`) — validate or drop. `detail=full` on snapshot is a
  validated no-op (`observatory.rs:255-263`).

## Admission/binding: V1 wiring status (recorded, not failed)

The admission/binding library is strong: transitive cycle detection over the
full ancestor chain, depth limit 32 enforced before dedup, idempotent re-
admission with fresh tokens, 256-bit single-use 30 s binding tokens with
redacted `Debug`, and a correct `strip_binding`. However, as wired in V1 the
seam is record-only: **no daemon route or turn path calls `validate_admission`,
`consume_binding`, `strip_binding`, or `validate_topology_edge`** (the latter
has zero call sites; `_observation_binding` stripping has no
provider-serialization integration point). This matches the V1 read-only
boundary — extensions are not yet admitted — and is recorded here so the gate
is explicit: **when the extension turn path is built, admission consumption +
binding strip must be wired in one step before any provider serialization, with
an integration test that `_observation_binding` never appears on the wire, and
`validate_topology_edge` must be connected to attestation ingestion (emitting
`TopologyAttestationRejected`).**

## Non-acceptance conditions (manifest §10) — disposition

1. Compile-time redaction macro — superseded by approved deviation D1; the
   structural allow-list holds (closed payload types, exhaustive no-wildcard
   adapter, runtime `AgentEvent` has no `Serialize`).
2. Cursor monotonicity — holds under all tested paths **except** the G4
   full-prune + restart hole; repair required.
3. HMAC-SHA256 implementation — correct: known-vector test, sign-before-parse,
   constant-time `verify_slice`, fail-closed 32-byte mode-0600 secret with
   `O_NOFOLLOW` and atomic create.
4. Admission cycle detection / depth — complete in the library (transitive,
   self-loop, cross-authority, depth 32); unwired per V1 scope above.
5. Forbidden-field list — no missing category found; sentinel sweeps with real
   planted secrets all pass.
6. SQLite races/integrity — single-connection mutex discipline is sound and
   readers never see uncommitted appends; G1 projection race and F1 executor
   blocking are the exceptions.
7. Wire deviations without operator approval — **triggered**: G2 (§7.3 replay
   shape) plus the previously flagged snapshot `capabilities` omission and the
   3 s vs 30 s keepalive text. Reconcile code or manifest.
8. Test coverage — suites green (observatory 49/49, daemon observatory 27/27
   + auth 5/5 at review time); gaps: reopen-after-prune, `through`-bounded
   `complete`, 4 unpinned skip variants, snapshot+tail equivalence.

## Production-rollout recommendations (ordered)

1. Repair G1–G5 as one bounded fix wave (G3+G4 together), then a narrow delta
   review of those diffs; keep the ROADMAP renderer gate closed until it
   passes.
2. Reconcile the §7.3 wire shape and keepalive text (code or manifest
   deviation, operator-visible).
3. Land F1 (`spawn_blocking`) before any multi-client Surface usage; F2 schema
   migration before Gate 2 consumers.
4. Take F3/F4/F5/F6/F9 as cheap contract hardening in the same wave.
5. Record the admission-wiring gate conditions (above) in the manifest or the
   extension-architecture migration plan so they cannot be skipped.
6. Add the parent-dir fsync, rotation-failure metric, and a CI grep guard that
   no daemon code injects `OCEAN_OBSERVER_TOKEN` into child environments.
7. Surface-proxy owners must enforce the cookie attributes (`Secure`,
   `HttpOnly`, `SameSite=Strict`, `Path=/v1/observatory`) — the daemon
   deliberately cannot; make it a checklist item in the Surface proxy review.

## Verification performed for this review

- `cargo test -p ocean-observatory` — 49/49 pass (28 lib + 14 fixture +
  5 redaction + 2 store).
- `cargo test -p ocean-daemon observatory` — 27/27 pass; `observatory_auth`
  5/5 pass.
- Direct re-read of every gating claim against `6ba2cef` source (G1–G5
  spot-verified by the consolidating agent).

## Repair wave (2026-09-25)

G1–G5 landed together on branch `fix/observatory-g5-401`, each with its own
regression test:

- **G1** — `snapshot_at` reads the watermark inside the database lock that
  `append_event` holds for its whole transaction, and an `at` other than that
  watermark is `StoreError::HistoricalSnapshot` (daemon: 409
  `snapshot_not_historical`) instead of current state under an old label. This
  pins Gate 1 to current-only snapshots; historical projection stays future
  work. Tests: `snapshot_is_point_in_time_and_tail_from_its_watermark_is_disjoint`,
  `snapshot_refuses_a_historical_cursor`.
- **G2** — `ReplayEvent` carries the full §7.3 envelope (`recorded_at`,
  `truth`, `producer`, `topology`, `correlation`, `visibility`); the SSE tail
  already sent the whole envelope, so no new field reaches any caller. The
  §7.2 heartbeat text now says 3 seconds, matching `SSE_KEEPALIVE_INTERVAL`.
  Test: `replay_pages_events_with_continuation` asserts exactly the twelve
  fields.
- **G3** — `apply_retention` uses the manifest cutoff (never past the
  `first_cursor` of an admitted/running execution, now persisted per node by an
  additive migration) and measures live database size from SQLite's page
  accounting minus the freelist (a DELETE frees pages without shrinking the
  file, so page_count alone would read as over the bound forever); the daemon runs it one minute after boot and hourly on a blocking
  thread until shutdown (`observatory::run_retention`). Tests:
  `retention_prunes_old_events_but_keeps_a_live_executions_history`,
  `retention_enforces_the_size_bound_from_real_db_size`,
  `scheduled_retention_prunes_and_stops_on_shutdown`,
  `a_pass_after_the_size_prune_does_not_prune_again`.
- **G4** — `ObservatoryStore::open` seeds the cursor from the maximum of the
  surviving events and every `watermarks` row (snapshot watermark and
  retention boundary). Test: `reopen_after_full_prune_continues_the_cursor`.
- **G5** — `ObservatoryAuth` rejects with `ObservatoryUnauthorized`: the §7.4
  headers and the §7.1 `{error: "unauthorized", message, http_status: 401}`
  body. Test: `routes_require_observer_auth` asserts both.

**Delta review: PASS (2026-09-25, ocean-os PR #486)** — every G closed with
evidence. One non-gating finding was fixed in the same PR: `append_event`
now publishes its cursor only after commit, so a refused append never leaves
the in-memory watermark ahead of the durable log
(`a_failed_append_does_not_advance_the_cursor`). Open, non-gating:

- ~~The size loop reduces a page-measured excess by raw JSON length~~ —
  fixed 2026-09-25: the size bound now prunes in batches of 64 and re-measures
  live pages after each commit, stopping once under the bound. WAL size is
  still not counted.
- ocean-surface's Replay scrubber asks `snapshot?at=<earlier cursor>`, which
  now answers 409 (it previously got current state under the wrong label); it
  must move to `/replay` or be disabled before the renderer relies on it.

## Hardening wave (2026-09-25)

The cheap non-gating items landed on branch `fix/observatory-hardening`, each
with a regression test. F2, F7, F8, F11, F12 and the §4.3 checkpoint task stay
open. (F2 later closed; see "F2 migration (2026-09-25)" below. F11 and F12
later closed; see "F11/F12 (2026-09-25)" below.)

- **F1** — every Observatory store call except the in-memory `latest_cursor`
  runs on Tokio's blocking pool through `observatory::off_executor`
  (`spawn_blocking`): the snapshot and replay handlers, the SSE tail's
  per-poll read, and the durability pump, now
  `observatory_adapter::run_durability_pump`, which awaits each append before
  the next `recv` so durable order stays bus order. `spawn_blocking` over
  `block_in_place` because the latter panics on a current-thread runtime. The
  tail also stops polling once its client is gone. Tests:
  `store_calls_do_not_stall_the_executor`,
  `durability_pump_appends_off_the_executor` (both wedge the store behind a
  competing writer and fail with a 5 s executor stall if the calls run
  inline).
- **F3** — `ObservatoryAuth` refuses any verified principal whose scope is not
  `ObserverScope::Summary`, with the same 401 `ObservatoryUnauthorized` (§7.1
  defines no 403). Tests: `routes_reject_a_content_scope_token`,
  `observatory_auth_rejects_non_summary_scopes_as_401`.
- **F4** — `events_page` sets `complete` to `!has_more`, so a page that
  reaches `through` below the watermark is complete. Test:
  `a_through_bounded_page_that_reaches_through_is_complete`.
- **F5** — the `stream.gap` frame carries no SSE `id:`. The gap is not an
  event the client consumed, so it must not move Last-Event-ID; a client
  dropped between the gap and the next event resumes from the last real event
  and meets the gap (or a `reset`) again. Test:
  `stream_gap_frame_carries_no_event_id`.
- **F6** — `continuation_url` percent-encodes the `filter` value (everything
  outside RFC 3986 unreserved). Test:
  `replay_continuation_url_encodes_the_filter`.
- **F9** — `forbidden_variants_are_skipped` also pins `ComponentRender`,
  `SurfacePatch`, `SlackCanvas` and `SessionConfigChanged`.
- **F10 (partial)** — the store connection sets a 5 s `busy_timeout`, and
  `retention_archive.from_cursor` records the previous retention boundary + 1.
  Tests: `an_append_waits_out_a_competing_writer`,
  `retention_archive_records_each_passes_real_from_cursor`. (The burned cursor on
  a failed commit was already closed by the repair wave's cursor-after-commit
  fix.) The §4.3 checkpoint task remains open.

## F2 migration (2026-09-25)

Landed on branch `feat/observatory-f2-schema`. The store schema is now
versioned by `PRAGMA user_version` and migrated step by step from 0 by
`ocean_observatory::migrate` (`crates/ocean-observatory/src/migration.rs`),
which `ObservatoryStore::open` runs before anything else touches the tables.

- **v0 → v1** — the pre-F2 baseline: the Task 3 tables if absent, plus G3's
  additive `execution_nodes.first_cursor`. Every existing `observatory.db`
  reads `user_version` 0, whether it is fresh, Task 3 shape, or G3 shape.
- **v1 → v2** — the §4.1 shape, by SQLite's rebuild procedure for every table
  (`foreign_keys` off, create `*_new`, copy in rowid order, drop, rename,
  `pragma_foreign_key_check` must be empty, commit, `foreign_keys` on), all in
  one `BEGIN IMMEDIATE` transaction that also bumps the version, so a failure
  leaves the database untouched. Event `kind`, `producer_id`, `visibility`,
  `schema_version` and `created_at` (= `recorded_at`) are backfilled with
  `json_extract` from `envelope_json`; an unparseable envelope is kept with
  sentinel columns rather than dropped. Nodes gain `session_id`, `turn_id`,
  `request_id`, `producer_id` (from the node's newest surviving event, else
  empty — the value the snapshot route already showed), `last_cursor` (that
  event's cursor, else NULL) and `finished_at` (its `recorded_at` when the
  phase is terminal). Edges gain `root_execution_id` (from the child node).
  Watermarks gain `created_at` (the migration time), the archive gains
  `reason` (NULL for old passes; new passes write `age` or `size`). Nodes,
  edges, watermarks and the archive gain the §4.1 surrogate `id`. All twelve
  §4.1 indexes are created by their manifest names.
- **Write path** — `append_event` fills every new column in the same
  transaction as the event; `last_cursor` moves on every event for the node,
  `finished_at` is set exactly while the phase is terminal, and identity
  columns backfilled empty are filled by the next event that carries them.
- Each step re-reads the version under the write lock, so a second opener or
  a second run is a no-op; a database newer than the build is refused
  (`StoreError::UnsupportedSchema`) and left untouched. No route's wire shape
  changed.

Deviations from §4.1, each forced by a Gate 1 invariant:

- **No FK from `execution_nodes.first_cursor`/`last_cursor` to
  `observatory_events(cursor)`.** Retention prunes terminal executions'
  events while their nodes stay in the snapshot. `RESTRICT` would block every
  prune; `CASCADE` would delete projection rows; `SET NULL` would erase a
  terminal row's start, and a NULL `first_cursor` is exactly what G3 reads as
  "unknown start, block retention" if a late event reopens that execution.
  The cursors are kept as historical pointers that may name pruned events.
- **`first_cursor` and `last_cursor` are nullable** (§4.1: `NOT NULL`). Rows
  written before G3 have no recorded start and must keep blocking retention
  until the restart sweep closes them; a node whose events were all pruned
  before this migration has no recoverable last cursor. Every row written
  after the migration has both.
- **`execution_edges` enforces only `child_execution_id → execution_nodes`.**
  The child node is upserted in the same transaction as its edge, so that key
  always holds. The parent and root are usually a session the daemon may
  never have seen created (a session restored from disk, a lagged durability
  pump): enforcing them would refuse the whole append — losing every event
  of such a turn — and existing databases hold such edges, so
  `foreign_key_check` would fail the migration.
- **No FK from `watermarks.cursor`.** `retention_boundary` names a pruned
  cursor by definition, and G4 reseeds the cursor from the watermarks after a
  full prune.
- **`observatory_events` keeps `cursor INTEGER PRIMARY KEY` and
  `envelope_json`** instead of a surrogate `id` plus `UNIQUE(cursor)` and
  `payload_json`. The cursor is already the unique, monotonic row identity,
  and replay range scans stay on the table b-tree; replay and the SSE tail
  return the whole §7.3 envelope (truth, topology, correlation), which no
  §4.1 column holds. The manifest-named cursor index exists anyway.
- The `daemon_instance_id` watermark key is still not written (its value is a
  UUID and §4.1's `cursor` column is an integer); unchanged by F2.

The size-bound test `a_pass_after_the_size_prune_does_not_prune_again` moved
its bound from 96 KiB to 384 KiB: the never-pruned projection of its 400
executions grew to about 190 KiB with the §4.1 node columns and indexes, so
the old bound sat below the projection alone and emptied the log.

A copy of a real operator database (Task 3 shape, 166 MB, 115,697 events,
7,130 nodes, 6,425 edges) migrated in 2.5 s (release build) with every row
count preserved and an empty `foreign_key_check`.

Tests (`crates/ocean-observatory/tests/migration.rs`):
`migrating_a_g3_v0_database_preserves_every_row_and_backfills` and
`migrating_a_task3_v0_database_without_first_cursor_preserves_every_row`
(hand-built old-schema databases with a NULL `first_cursor` row, an edge
whose parent was never observed, a node whose events were already pruned,
watermarks and an archive row), `running_the_migration_twice_is_a_no_op`
(full schema-and-row dump unchanged after a second open and a direct
`migrate`), `a_database_from_a_newer_build_is_refused_untouched`,
`an_unparseable_envelope_is_kept_with_sentinel_columns`,
`new_appends_populate_every_column`,
`retention_still_prunes_a_migrated_database`, and
`every_manifest_index_and_the_edge_foreign_key_exist` (fresh and migrated;
the FK is enforced, not inert).

### F2 review follow-ups (2026-09-25)

Independent review requested changes; all four landed before merge:

- **Downgrade safety.** Every NOT NULL column F2 added carries a DEFAULT
  (`schema_version` 1, `kind` 'unknown', empty strings elsewhere), so an
  older daemon binary opening a v2 database keeps appending with its pre-F2
  column lists instead of failing every write
  (`a_pre_f2_writer_can_still_append_to_a_v2_database`).
- **No duplicate indexes.** `idx_observatory_events_cursor` (the cursor is the
  rowid) and `idx_observatory_events_event_id`,
  `idx_execution_nodes_execution_id`, `idx_execution_edges_edge_id` (UNIQUE
  columns already carry an autoindex) are not created — a documented
  deviation from §4.1's list; the remaining eight are
  (`no_index_duplicates_a_primary_or_unique_key`).
- **Orphan edges.** An edge whose child node does not exist cannot satisfy
  the new child FK, so the migration drops it rather than failing
  `foreign_key_check` on every boot and leaving the store permanently closed.
- **Projection floor.** Size retention runs only while the event log is what
  exceeds the bound; when the never-pruned projection alone is over it, the
  bound is unreachable and the log is left intact rather than emptied every
  pass (`a_bound_below_the_projection_floor_does_not_wipe_the_log`). The size
  tests now use single-execution heavy events so their bound sits above that
  floor.

Operational notes: the first open after upgrade rewrites the database in one
transaction (~2.5 s for the operator's 166 MB file) on the daemon's startup
path, and the WAL grows to roughly the database size until checkpoint, so
the volume needs that much headroom once.

## F11/F12 (2026-09-25)

Landed on branch `fix/observatory-f11-f12`, each item with a regression test.
No wire shape changed except that `events?scope=` now refuses what it used to
ignore.

- **F11 — directory fsync.** `ObserverSecret::load_or_generate` fsyncs
  `<ocean_dir>` after the hard link, and `write_summary_observer_token` after
  the rename, so neither new directory entry can be lost to a crash after the
  daemon started relying on it. A failed directory sync fails the call, like
  every other secret I/O error. Loading an existing secret syncs nothing.
  Test: `secret_link_and_token_rename_sync_the_parent_directory`.
- **F11 — rotation failure.** The ten-minute rotation now runs through
  `ObservatoryAuthState::rotate_summary_token`, which counts every failure on
  `ocean_observatory_token_rotation_failures_total` (`GET /metrics`, a
  `TurnMetrics` counter) and logs at `error` with the consecutive and
  lifetime counts and `published_token_expired` (true from the third
  consecutive failure: tokens live 30 minutes, rotation runs every 10). A
  success after failures logs the recovery at `warn`. Test:
  `failed_token_rotation_is_counted_on_metrics`.
- **F11 — zero-lifetime tokens.** `validate_claims` refuses
  `expires_at <= issued_at` as `InvalidClaims`; `expires_at == issued_at` used
  to pass. Test: `zero_lifetime_token_is_rejected`.
- **F11 — unix-only secret handling.** Left as is: the crate already imports
  `std::os::unix` unconditionally, so it does not build off unix, and a
  portable secret store is not a trivial fix.
- **F12 — `?scope=`.** Manifest §7.2 defines `scope` as `summary` (default)
  or future `content`, so it is validated rather than dropped: any value but
  `summary` (including `content`, which V1 does not serve) gets a single
  `event: error` frame with `{"error":"invalid_scope"}`. That is the
  manifest's form of a 400 on this SSE route, the same one `invalid_cursor`
  uses; the HTTP status stays 200 so an `EventSource` client can read the
  reason. Manifest §7.2, the operator guide, and the daemon AGENTS.md say so.
  Test: `events_rejects_an_unsupported_scope`.
- **F12 — `detail=full`.** §7.1 says `full` "includes metadata" but defines
  no field for it, and every fact the projection holds is already in the
  summary shape, so there is nothing to add. It is documented as reserved
  (route doc on `SnapshotQuery`, manifest §7.1, operator guide) and answers
  exactly what `summary` does; other values stay 400 `invalid_detail`. Test:
  `snapshot_detail_full_is_a_reserved_alias_of_summary`.
- **F12 — `interrupted` vs `Canceled`.** `ExecutionPhase` has no
  `Interrupted` variant, and adding one is a wire change, so the enum is
  unchanged and the record is reconciled instead: Gate 0 decisions R3 and the
  `mark_interrupted` doc say the restart sweep's "interrupted" is `canceled`
  on the wire. Operators tell a restart-closed execution from a cancelled turn
  by its terminal event: `execution_phase_changed` to `canceled` from the next
  boot's `daemon_instance_id`, versus `execution_finished` with
  `turn_cancelled`/`turn_abandoned`. Test:
  `restart_sweep_records_interrupted_as_a_phase_change_to_canceled` (also
  fails if an `interrupted` phase is ever added without updating R3).

### Rollout recommendation 6, guard (2026-09-25)

The CI guard the sixth recommendation asks for exists:
`crates/ocean-observatory/tests/observer_token_env_guard.rs` scans every
crate's production source and fails if `OCEAN_OBSERVER_TOKEN` is named
anywhere but `ocean-observatory/src/auth.rs`, or is passed to a child through
`.env(`, `.envs(`, or `set_var(` there. It was mutation-checked against a
planted `Command::env` in daemon code. Parent-directory fsync and the
rotation-failure metric, the other two parts of that recommendation, landed
with F11.

### Rollout recommendation 5 (2026-09-25)

The admission-wiring gate above is recorded as §9.4 of the Gate 1
implementation manifest and enforced by
`crates/ocean-observatory/tests/admission_wiring_gate.rs`.
