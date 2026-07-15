# Ocean Daemon Persistent Rooms Extraction Manifest

**Date:** 2026-07-15
**Status:** Characterization complete and independently reviewed; extraction authorized
**Owner:** Ocean OS
**Rollback point:** characterization commit `6f2fc9b`

## Purpose

Characterize and then move the daemon's durable `/v1/rooms/persistent/*` HTTP lifecycle, shared SQLite-handle adapters, transcript hydration/paging helpers, and room mention/auto-convene behavior into one private binary module without changing persistence, route methods or precedence, JSON envelopes, serde defaults, key/workspace normalization, roster/transcript ordering, trigger/event/audit ordering, stable room-agent session identity, caller cwd/project binding, permission posture, call-transcript persistence, LiveKit authorization, lock lifetime, poison recovery, or soft-closed audit replay.

This is a daemon adapter extraction, not a room model or persistence redesign. `ocean-store` remains the schema, migration, transaction, sequence, cap, cursor, soft-close, and durable record authority. `ocean-agent` remains session authority, `ocean-runtime` remains turn/permission authority, and `ocean-call` remains call-pipeline authority. Parent daemon composition retains `AppState`, startup opening/migration timing, the canonical route group, the independent LiveKit token handler, call persistence/retry accounting, global router/middleware, and top-level task orchestration.

## Current-upstream reconciliation

This checkpoint starts from fetched `origin/main` at merge `3e051c1`, which contains recall-registry PR #287 plus the concurrent three-state permission/TUI work from `827b65b`. The room auto-convene path therefore uses `effective_permission_mode()` and passes `PermissionMode` into `build_prompt_control`; this checkpoint must not restore the older boolean-only policy. PR #287 changed no persistent-room behavior beyond adding an unrelated private recall module and a Rust 1.88-compatible TUI test expression.

Before each characterization, extraction, documentation, and publication checkpoint: fetch/rebase onto current `origin/main`, reread the root/crates/daemon/docs contracts, inspect every upstream daemon/store diff, and rerun characterization when a room, call persistence, LiveKit, permission, `AppState`, route, or fixture seam changed.

## Characterization required before extraction

### Real-router lifecycle and extractor contracts

Add one compact full-room lifecycle test through `room_routes().with_state(...)` that freezes:

- create key trimming, verbatim name, blank-workspace normalization to `None`, optional policy, exact `201 {ok,room}` envelope;
- list's exact `200 {ok,rooms,next_cursor,has_more}` envelope and transcript exclusion;
- detail's exact `200 {ok,room,transcript}` envelope;
- omitted participant `kind` and message `author_kind` defaulting to `Human`;
- join/message/leave statuses and exact top-level envelope keys;
- message body/author attribution and returned message sequence;
- join → message → leave transcript ordering and dense ascending sequence.

Add one real-router error/extractor matrix that freezes custom room errors separately from Axum rejections:

- blank create key `400`, duplicate `409`, missing detail `404`, and missing participant `404`, with exact custom JSON bodies;
- malformed JSON and malformed numeric query current statuses/content types/body text;
- percent-encoded blank path behavior as currently observed;
- mapper coverage for exact `BadKey`, `UnknownRoom`, `UnknownParticipant`, `AlreadyExists`, `Db`, and `Encode` status/body shapes.

Do not duplicate `ocean-store`'s exhaustive transaction, limit, cursor, soft-close, concurrency, migration, or schema tests.

### Trigger/event/audit ordering

Strengthen the existing resolved-agent, non-agent, and agent-authored message tests rather than adding another async turn fixture:

- the returned `message` is the original persisted author row;
- the global `room_trigger` extension event carries exact room/target/reason/triggering sequence and `scope: None`;
- the synchronous audit row is exactly the next sequence after the triggering row and precedes the eventual agent reply;
- raw policy matches remain in `triggers_fired` even when no runnable Agent resolves;
- non-agent and agent-authored posts emit no trigger event, write no `auto-convene:` audit row, and queue no turn.

Keep the existing environment lock, fake runtime, workspace-bound/unbound assertions, stable session ID, reply persistence, and anti-loop behavior.

### Closed-room audit asymmetry and paging

Add one daemon-level matrix proving a soft-closed room:

- is absent from list and returns detail `404`;
- remains readable through transcript, snapshot, and events audit fallbacks;
- retains frozen room metadata/participants and identical ascending transcript rows;
- applies the daemon-owned closed-fallback `limit=0` floor-to-one behavior, `has_more`, `next_seq`, `last_seq`, and cursor replay without gaps or duplicates.

### Shared handle poison recovery

Add one test that poisons a single `RoomStoreHandle`, then proves both `with_rooms_handle` and `with_rooms` recover that same guard with `PoisonError::into_inner` for a synchronous create/read. No helper closure may await or spawn.

Keep and rerun existing open-room paging, restart durability, call transcript/read-after-close, route-retirement, full-router overlap, call-room token, and trigger tests.

## Exact symbols to move after characterization

Move from `crates/ocean-daemon/src/main.rs` into new private `crates/ocean-daemon/src/persistent_rooms.rs` as one domain boundary:

- `RoomStoreHandle` and its attached state/lock documentation;
- `room_db_path`;
- `with_rooms`;
- `with_rooms_handle`;
- `room_store_error_response`;
- `RoomCreateRequest`;
- `room_create`;
- `RoomsListQuery`;
- `rooms_list_persistent`;
- `room_get`;
- `RoomJoinRequest`;
- `default_participant_kind`;
- `room_join`;
- `room_leave`;
- `RoomMessageRequest`;
- `room_post_message`;
- `parse_mentions`;
- `ROOM_AGENT_SESSION_NS`;
- `ROOM_CONTEXT_TAIL`;
- `resolve_agent_participant`;
- `room_agent_session_id`;
- `build_room_prompt`;
- `spawn_room_agent_turn`;
- `TranscriptQuery`;
- `read_transcript_page`;
- `room_transcript`;
- `room_snapshot`;
- `room_events`.

Move attached symbol documentation with the symbols. The only permitted extraction changes are private-module imports, `pub(super)` visibility needed by retained parent composition/tests, test-only access adaptation, and rustfmt formatting.

## Visibility contract

The module remains private (`mod persistent_rooms`), never public outside the daemon binary.

Production parent consumers require `pub(super)` visibility for:

- `RoomStoreHandle`, `room_db_path`, `with_rooms`, `with_rooms_handle`;
- all nine persistent-room handlers used by retained `room_routes()`.

Retained parent tests require `pub(super)` visibility for:

- `RoomCreateRequest`, `RoomsListQuery`, `RoomJoinRequest`, `RoomMessageRequest`, `TranscriptQuery`, plus their directly constructed fields;
- `room_store_error_response`, `parse_mentions`, `resolve_agent_participant`, and `room_agent_session_id`.

Keep `default_participant_kind`, `ROOM_AGENT_SESSION_NS`, `ROOM_CONTEXT_TAIL`, `build_room_prompt`, `spawn_room_agent_turn`, and `read_transcript_page` module-private unless compiler-confirmed retained parent tests require narrower test-only exposure. No item becomes `pub` outside the crate.

## Dependencies

The child may depend on existing binary-private parent composition without introducing a service trait or substate:

- `AppState` and its existing `runtime`, `agent_events`, `requests`, and `rooms` fields;
- parent `build_prompt_control`, `record_prompt_result`, `core_sid`, and `sdk_sid`;
- private `request_control::register_running_request`;
- private `yolo_settings::effective_permission_mode`.

Expected existing external types include Axum `Path/Query/State/Json/StatusCode`, `chrono::Utc`, `uuid::Uuid`, SDK agent IDs/events, core room/permission/prompt types, `ocean_store::{RoomStore, RoomStoreError, SqliteRoomStore, TranscriptPage}`, and `serde_json::json`. No new Cargo dependency is permitted.

Rust's private item graph may legally contain `AppState -> persistent_rooms::RoomStoreHandle` and `persistent_rooms` handlers -> parent `AppState`. That compile-time relationship is the existing composition boundary; it must not be replaced with a public daemon library, `RoomSubstate`, `dyn RoomStore`, service trait, second store owner, or async mutex.

## Frozen invariants

### Persistence and startup

- Path precedence remains recognized `OCEAN_DB_PATH`, otherwise `ocean_agent::config_dir_from_env()/rooms.db`.
- Parent directory creation, error context strings, synchronous `SqliteRoomStore::open`, idempotent migration timing, readiness log, and state assembly remain in `main.rs` at the same startup position.
- Exactly one concrete `SqliteRoomStore` remains wrapped in one process-wide `Arc<std::sync::Mutex<_>>` and shared with persistent HTTP, call persistence/retries, and LiveKit authorization.
- `ocean-store` keeps all schema, SQL, transaction, sequence, limit/cursor, and soft-close authority.

### HTTP and routing

- `room_routes()` remains in `main.rs` with the exact eight persistent `.route(...)` declarations (nine method/path pairs) plus the independent LiveKit route.
- Static `/v1/rooms/persistent...` matching remains ahead of dynamic `/v1/rooms/{room_id}/livekit-token`; overlap GET/POST behavior remains exact.
- The eight Axum persistent `.route(...)` declarations continue representing exactly nine persistent method/path pairs, plus the independent LiveKit method/path pair.
- Retired Track-0 room routes remain `404`; default Axum `404/405`, implicit HEAD/Allow, global HTTP tracing/CORS order, banner, and operator-guide parity remain exact.
- Status codes, top-level JSON keys, typed store error strings, Axum extractor rejection bodies, serde defaults, key trimming, blank workspace normalization, participant/message attribution, paging metadata, and list/detail asymmetry do not change.

### Lock and ordering

- Both shared lock helpers recover poisoned `std::sync::Mutex` guards through `into_inner`.
- Every room-store closure remains synchronous; no guard crosses any `.await`, event publication, spawned turn, retry sleep, or runtime call.
- `room_post_message` retains append + trigger-policy lookup + roster read in one acquisition.
- Trigger processing happens after that guard drops. For a resolved Agent, order remains persisted user row → global extension event → persisted system audit row → spawned turn → eventual reply/failure row.
- Unresolved/non-agent policy matches remain visible in `triggers_fired` but produce no event/audit/turn. Agent-authored rows never evaluate triggers.
- `room_snapshot` retains metadata plus first transcript-page reads under one acquisition.
- Ignored/best-effort room errors in audit and async reply/failure appends remain ignored; no new retry or HTTP failure is introduced.

### Room-agent orchestration

- UUID-v5 namespace and `(room, participant)` seed remain byte-for-byte stable.
- Context tail remains 20 and oldest-to-newest after truncation.
- Bound room cwd/project resolution, unbound neutral-daemon-cwd fallback, strict resume/create-if-missing, internal request registration, client type `room`, no decision token, effective three-state permission mode, prompt/control construction, result recording, and agent/system reply attribution remain unchanged.
- The unbound fallback is an explicit compatibility exception for internal auto-convene that has no caller cwd. It is not permitted for caller-submitted HTTP or resumed turns, and the startup guard continues to reject repository cwd. Changing this legacy fallback requires a separate workspace-binding migration, not this behavior-neutral move.
- The module does not acquire session/runtime authority merely because these composition calls move with the cohesive room lifecycle.

### Closed audit and external consumers

- Open detail/list continue hiding soft-closed rooms.
- Transcript/snapshot/events continue using `get_including_closed` fallback and identical bounded paging envelopes for frozen call/room history.
- Call `PersistJob`, retry classification/backoff/drop accounting, `BusSink`, event ordering, and call feature paths remain in `main.rs` and keep using the same exported handle/lock seam.
- LiveKit signing/publish grants and `call:` existence/open checks remain in `main.rs` and use the same store; non-call IDs preserve current pass-through behavior.

## Composition anchors and exclusions

This checkpoint does not:

- move/change `AppState`, startup store opening, `app_router`, `room_routes`, banner/operator guide, call persistence/retries/metrics, `BusSink`, LiveKit token/signing/publish policy, or title DB policy;
- change `ocean-store` API/schema/SQL/migrations/transactions/caps/cursors/serialization;
- change room/session identity, prompts, cwd/project fallback, permission policy, event scope, turn cancellation, or session persistence;
- revive retired Track-0 routes or unify persistent rooms with sessions/calls;
- add a public API, daemon library, domain substate, service trait, trait object, async mutex, second store connection/owner, dependency, generated routing, rename, or opportunistic cleanup.

Any route, HTTP, serde, persistence, lock/await, ordering, event, permission, cwd, identity, call, LiveKit, or store behavior change stops the extraction and requires a separate decision.

## Files in scope

- `crates/ocean-daemon/src/main.rs`
- new `crates/ocean-daemon/src/persistent_rooms.rs`
- `crates/ocean-daemon/AGENTS.md`
- `docs/DAEMON_REFACTOR_MISSION.md`
- `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`
- this manifest
- the recall-registry manifest/status reconciliation
- root `events.md`

No `ocean-store`, `ocean-call`, shared protocol, deployment, or live daemon file is in scope.

## Validation

Characterization gate:

```bash
cargo test -p ocean-daemon persistent_room_http_ -- --nocapture
cargo test -p ocean-daemon room_store_helpers_recover_one_poisoned_handle -- --nocapture
cargo test -p ocean-daemon at_mention_queues_turn_and_posts_reply_back -- --nocapture
cargo test -p ocean-daemon mention_of_non_agent_queues_no_turn -- --nocapture
cargo test -p ocean-daemon agent_authored_message_does_not_self_trigger -- --nocapture
cargo test -p ocean-daemon closed_persistent_room_preserves_audit_http_asymmetry -- --nocapture
cargo test -p ocean-daemon room_transcript_is_bounded_and_pageable -- --nocapture
cargo test -p ocean-daemon rooms_list_is_bounded_and_pageable -- --nocapture
cargo test -p ocean-daemon router_contract_room_static_dynamic_precedence_matches_snapshot -- --nocapture
cargo test -p ocean-daemon room_router_retires_track0_gets_and_keeps_persistent_and_livekit_routes -- --nocapture
cargo test -p ocean-store
RUST_TEST_THREADS=1 cargo test -p ocean-daemon
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Extraction completion gate:

```bash
cargo test -p ocean-daemon persistent_room_http_ -- --nocapture
cargo test -p ocean-daemon room_store_helpers_recover_one_poisoned_handle -- --nocapture
cargo test -p ocean-daemon at_mention_queues_turn_and_posts_reply_back -- --nocapture
cargo test -p ocean-daemon mention_of_non_agent_queues_no_turn -- --nocapture
cargo test -p ocean-daemon agent_authored_message_does_not_self_trigger -- --nocapture
cargo test -p ocean-daemon closed_persistent_room_preserves_audit_http_asymmetry -- --nocapture
cargo test -p ocean-daemon room_ -- --nocapture
cargo test -p ocean-daemon persist_ -- --nocapture
cargo test -p ocean-daemon closed_call_room -- --nocapture
cargo test -p ocean-daemon router_contract -- --nocapture
cargo test -p ocean-store
RUST_TEST_THREADS=1 cargo test -p ocean-daemon
cargo check --workspace --tests
cargo check -p ocean-daemon --features livekit-tap
cargo check -p ocean-daemon --features deepgram-stt
cargo xtask ci --compatibility
cargo +1.88.0 xtask ci --msrv
cargo xtask ci
cargo fmt --all -- --check
cargo xtask docs-check
git diff --check
```

Use a dedicated `CARGO_TARGET_DIR` for this worktree. Run environment-mutating daemon tests serialized locally; default-parallel hosted CI remains required.

A fresh characterization reviewer must assess HTTP/extractor coverage, closed-audit paging, event/audit/turn ordering, poison recovery, and duplication against `ocean-store`. A separate fresh extraction reviewer must compare every moved production body against the characterization commit, inspect visibility/imports, retained call/LiveKit consumers, lock/await ordering, permission/cwd/session behavior, and report any unresolved medium-or-higher issue.

## Characterization result

Commit `6f2fc9b` added the approved daemon-only characterization without moving a production persistent-room body or changing `ocean-store`. The real `room_routes()` lifecycle freezes exact create/list/detail/join/message/leave success values, top-level keys, identity/attribution, serde defaults, key/workspace normalization, and transcript ordering. The error matrix freezes custom JSON errors separately from exact Axum JSON/query rejection text, and the mapper table covers every `RoomStoreError` variant. Resolved, unresolved, and agent-authored mention paths now freeze exact returned-row persistence, extension payload/scope, no-false-footprint behavior, and persisted-author → event → audit → spawn source order. Closed-room coverage recombines bounded pages and proves exact equality with snapshot/events plus frozen metadata and roster. One deliberately poisoned shared handle recovers through both lock adapters. Existing paging, restart, call persistence/read-after-close, LiveKit, route-retirement, and static/dynamic precedence coverage remains green.

All 343 daemon tests passed serialized in dedicated target `/tmp/ocean-target-persistent-rooms`; all 38 `ocean-store` tests passed. Focused room HTTP, trigger, poison, paging, and route groups, formatting, docs/index validation, and diff checks passed. Two fresh boundary/characterization re-reviews reported PASS with no unresolved medium-or-higher issue. The accepted low residuals are deliberate snapshot brittleness in exact Axum/rusqlite text and the source-order test's required `include_str!` redirection to `persistent_rooms.rs` during the mechanical move.

## Extraction result

Pending.

## Rollback

Revert the bounded extraction after the characterization commit. If reverting characterization too, remove only the added tests/helpers and restore no production behavior. There is no schema migration, wire version, persistent-data rewrite, or compatibility cleanup in this checkpoint.
