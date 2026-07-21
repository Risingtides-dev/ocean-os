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
