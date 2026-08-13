# ocean-board — Standalone Card Envelopes and Board Projection

## Purpose

Own the pure library that encodes Kanban card events into room message bodies
and folds a room transcript into a board.

## Ownership

- **Scope:** `crates/ocean-board/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Does not own:** room durability (`ocean-store`), transport or routes
  (`ocean-daemon`), federation (`ocean-daemon/room_federation.rs`), rendering
  (`ocean-surface`, `ocean-tui`), or card authorization

## Local Contracts

- A board is a **projection over the existing room transcript**. Do not add a
  cards table, a parallel room-event log, or a second durable authority; the
  per-room transcript and its `seq` remain the only durable room-event log.
- Cards are keyed by a client-generated `card_id` carried inside the envelope.
  **Never key a card on `thread_parent_seq` or on message `seq` linkage.**
  `ocean-store`'s federated append writes `thread_parent_seq` and `session_id`
  as `NULL`, and the federated wire payload is a closed struct of
  `client_event_id` / `author_member_id` / `body` / `mention_member_ids`, so
  thread structure does not survive federation.
- Order events by [`EventClock`]. Bedrock's `global_sequence` is the confirmed
  display order and is identical on every seat; local `seq` is assigned
  per-daemon and diverges across seats, so it is a pending-only fallback.
- Keep the fold order-independent: resolve last-writer-wins per field, not per
  card, so a hydrate plus a live tail equals a full replay.
- A tagged envelope this build cannot apply is counted as unsupported, never
  silently treated as chat. An untagged body is chat and is never promoted.
- Validate on encode, not on decode: an event that already reached the
  transcript is history, and dropping it mid-fold would make seats disagree.
- Keep the crate I/O-free and dependent only on `serde`/`serde_json`.

## Work Guidance

The envelope is a cross-repo contract. `ocean-surface` does not depend on
`ocean-core` or this crate — it redefines room types locally — so any schema
change must land a matching encoder there plus an executable drift check,
per the root roadmap item on cross-repository contract drift. Bump
`ENVELOPE_VERSION` rather than repurposing an existing field; older seats
degrade to counting unsupported events, which is the intended failure mode.

`tests/envelope_fixtures.rs` pins the exact encoded bytes for every op. The
reciprocal test in `ocean-surface` pins the identical strings. A fixture
failure means the two encoders have diverged — fix the encoder, never the
expected string. Decoding stays field-order independent so a twin encoder
cannot break this side by reordering.

Runtime wiring (daemon routes, UI, agent triggers) is a separate reviewed
checkpoint and must not be smuggled into projection work.

## Verification

- `cargo test -p ocean-board`
- `cargo clippy -p ocean-board --all-targets -- -D warnings`

## Child devlog Index

No child boundaries defined.
