# ocean-observatory — Observatory Schema, Store, and Auth

## Purpose

Own the metadata-only Observatory event schema, the SQLite/WAL durable store
(`observatory.db`), cursor semantics, retention, scoped observer auth, and the
extension admission/binding library governed by the Gate 1 implementation
manifest (`docs/specs/2026-07-17-observatory-gate1-implementation-manifest.md`).

## Ownership

- **Scope:** `crates/ocean-observatory/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Does not own:** daemon event adapters, HTTP routes, or the SSE tail
  (`ocean-daemon`), and Surface rendering

## Local Contracts

- The store schema is versioned by `PRAGMA user_version`
  (`STORE_SCHEMA_VERSION`, `src/migration.rs`). Every schema change is a new
  numbered step appended to `migrate`: it runs in its own `BEGIN IMMEDIATE`
  transaction, re-reads the version under the write lock, and bumps it in the
  same transaction. Never edit a shipped step, never change the shape outside
  `migrate`, and never make `open` depend on a table shape a step has not
  created. A database newer than the build is refused
  (`StoreError::UnsupportedSchema`), not guessed at.
- A migration preserves every event, node, edge, watermark, and archive row.
  Adding a constraint or `NOT NULL` column means SQLite's rebuild procedure:
  `foreign_keys` off outside the transaction, create `*_new`, copy, drop,
  rename, `pragma_foreign_key_check` must be empty, commit, `foreign_keys`
  back on. New columns are backfilled from `envelope_json`; a value no record
  holds is NULL or an explicit sentinel, never invented.
- The write path (`append_event`) populates every column in the same
  transaction as the event: event `kind`/`producer_id`/`visibility`/
  `schema_version`/`created_at`, node `last_cursor` on every event and
  `finished_at` exactly while the phase is terminal.
- No foreign key may reference `observatory_events`: retention prunes events
  while nodes, edges, and watermarks outlive them. The v2 deviations from
  manifest §4.1 (and why) are documented on `SCHEMA_V2` and in the Task 9
  review doc's "F2 migration (2026-09-25)" section.
- Retention never crosses the `first_cursor` of an admitted/running
  execution; a NULL `first_cursor` on such a row blocks pruning.
- `envelope_json` is the replay/tail source of truth; the scalar columns are
  indexes over it, not a second copy the routes read.

## Work Guidance

Test every migration step against a database written by hand with the old
schema (see `tests/migration.rs`), including idempotency and retention after
migrating. Route wire shapes live in `ocean-daemon` and the `snapshot` module;
a schema change must not change them.

## Verification

- `cargo test -p ocean-observatory`
- `cargo clippy -p ocean-observatory --all-targets -- -D warnings`
- `cargo test -p ocean-daemon observatory` for the route and adapter seams

## Child devlog Index

No child boundaries defined.
