# ocean-daemon — HTTP Daemon

## Purpose

This crate owns the long-running Ocean HTTP service on `:4780`, including API routes, SSE event streaming, daemon runtime authority, and client-facing turn orchestration.

## Ownership

- **Scope:** `crates/ocean-daemon/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** daemon API, health endpoints, turn/event routes, runtime wiring, permission boundary enforcement

## Local Contracts

- Daemon health is `GET /health`, not `/v1/health`.
- Restart the daemon only by specific PID; do not use blind `pkill` sweeps.
- HTTP turn routes must resolve effective cwd from client cwd/project metadata and must never fall back to daemon process cwd.
- Do not bypass runtime permission gates from daemon route code.
- Global approval policy is exposed at `GET/POST /v1/settings/permissions` with
  manual, automatic (default), and skip-all modes; the legacy yolo endpoint
  remains a compatibility adapter and request-wire `yolo` remains inert.
- `call-voice` turns are always `HarnessProfile::Voice`, `yolo: false`, and
  `PromptControl::without_tools()`, regardless of global or persisted YOLO.
- `POST /v1/voice/stt` remains the credential-owning batch transcription seam
  for every first-party surface. Browser `application/octet-stream` retains the
  historical WebM multipart metadata; `audio/wav`/`audio/x-wav`/`audio/wave`
  selects bounded native WAV metadata for TUI dictation. Unknown content types
  fail soft to WebM for wire compatibility; no client receives the xAI key.
- `POST /v1/voice/realtime/client-secret` resolves only the dedicated Realtime
  voice credential (`OCEAN_OPENAI_REALTIME_API_KEY`,
  `OPENAI_REALTIME_API_KEY`, or auth-file `openai-realtime.api_key`). It must
  never inherit the agent `openai` API-key block or `openai-codex` OAuth.
- The daemon default Realtime model is `gpt-realtime-2.1`; preserve that exact
  ID in the upstream `session.model` body unless a surface explicitly supplies
  a compatible per-request override.
- Realtime `purpose: "planner"` is pre-session and propose-only: validate the
  registered project plus canonical live worktree before credential resolution,
  advertise only `propose_handoff`, and mutate only through the existing
  session/message/turn routes after an explicit Surface click.
- Session behavior lives in `ocean-agent`; route changes must not create a separate session model.
- `GET /v1/agent/history/search` is a bounded adapter over ocean-agent's persisted display-transcript search (default 20, clamp 1..50); it performs no provider or embedding calls, allows at most two concurrent scans, and returns a capacity response before deserialization when raw session files exceed the 64 MiB request budget.
- Subagent roles, dispatch, lifecycle, and orchestration are extension-owned. Do not add daemon-native `task`/`spawn_worker`/fleet machinery. The daemon may expose generic permission-gated turn, cancellation, capability-provider, and extension event/tool seams; current `/v1/subagents/spec` and folder-agent subagent metadata remain compatibility surfaces until a separately approved extension migration.
- Slack Socket Mode, API/credential access, reconnects, replies, files, and real Canvas delivery are `ocean-slack` extension concerns. Private `slack_canvas_fulfillment.rs` is only the temporary typed host ingress/readback, runtime lookup, scoped-event, and lifecycle-enforcement compatibility seam; do not grow it into a second Slack transport authority.
- `component_interaction.rs` is a leaf HTTP fulfillment adapter over the runtime-owned `COMPONENT_WAIT_REGISTRY`: preserve exact key scoping, remove-before-send semantics, poison/error responses, and runtime ownership of wait registration, timeout, and ordinary cleanup.
- `model_roles.rs` owns once-at-startup fail-open `[roles]` loading and pure turn/advisor alias precedence only. Keep `AppState`, warning call sites, provider routing/readiness, persisted model selection, and advisor execution in their existing owners; do not trim or validate role strings during extraction.
- Post-turn advisor execution is best-effort and isolated from the completed main turn: preserve activation/alias precedence, use only the dedicated fixed two-permit `AppState` limiter with immediate fail-open saturation, hold its owned permit across the provider call, and keep the call under a fixed 30-second timeout. Advisor Extension payloads retain `note`/`severity`/`model` and carry the authoritative originating `turn_id`; logs and Prometheus labels must never contain prompt, response, or note content, and metrics labels remain fixed-cardinality outcomes only.
- `request_control.rs` owns the private request/permission registry records, status-only snapshots, registration/handle mechanics, waiter cancellation, and bounded status transitions. Keep `AppState`, permission policy/orchestration, decision-token verification, HTTP/event mapping, GC scheduling, active-turn projection, and shutdown draining in composition; preserve sender/handle ownership and drop registry locks before signaling or awaiting.
- `recall_registry.rs` owns only the private in-memory `title_id -> RecallVote` store, first-cast tally construction, distinct-voter casting, poison recovery, and named-tally removal. Keep UUID/live-title validation, persisted title and daemon-held Revoker authority, carried-outcome execution, HTTP mapping, and successful-only cleanup ordering in composition. Preserve the existing memory-only, unbounded retention of abandoned tallies during behavior-neutral extraction.
- `persistent_rooms.rs` owns the private shared room-store handle/lock adapters, durable-room HTTP lifecycle and paging handlers, mention policy evaluation, and room-agent auto-convene helper path. Keep `AppState`, startup store opening, `room_routes()`, call persistence/retries, and LiveKit token authorization in composition. Preserve one concrete `SqliteRoomStore`, poison recovery, no guard across await/event/spawn, exact persisted-row → event → audit-attempt → spawn order, stable room-agent session identity, three-state permission authority, and closed-room audit replay.
- Persistent-room Agent participants bind to folder-as-agent definitions through the shared explicit-result resolver: joins reject unresolved names; mention handling resolves before any `room_trigger`/`auto-convene` footprint and emits only an honest System note on failure; execution re-resolves and applies instructions, model, tool allowlist, and subprocess capabilities, failing closed before request registration. A valid data-only definition with no overrides remains resolved. The non-room named-agent turn stays fail-open, and room cwd, permissions, YOLO, decision-token, and tool-availability posture remain unchanged.
- `GET /v1/rooms/persistent/{key}/events` is the open, non-call persistent-room SSE tail: `Last-Event-ID` (numeric or 400) wins over `after_seq`; frames are `room_message` with the per-room seq id and exact `RoomMessage` JSON. Subscribe before bounded SQLite replay, treat broadcast only as post-commit `(room, seq)` wake hints, and recover gaps or `Lagged` by paging from `last_sent_seq` in order with deduplication. Unknown/closed rooms return 404, `call:` rooms return the typed unsupported rejection, and the stream uses the shared 3-second keepalive plus shutdown wrapper. Every non-call production transcript writer must publish only after its allocating transaction commits. Tail tasks must select downstream `tx.closed()` against both the replay/live test seam and the idle broadcast wait so a disconnected HTTP client releases its `AppState` and wake receiver without requiring a new room hint.
- `longhouse_preparation.rs` owns only the state-free prepare/inspect/workflow HTTP request/projection adapters. Preserve exact Axum extractor/method/default envelopes, PR #292 exact-token evidence and redaction, cwd roots/cache choice, and all three `spawn_blocking` fail-open lanes. Keep route composition, librarian query/fetch, compatibility subagent spec, and all governance/title/escrow/recall state in `main.rs`; ranking/cache algorithms remain in `ocean-longhouse`. The deferred cached skill-path symlink-retarget finding must be resolved separately before any librarian extraction.
- `longhouse_turn_preparation.rs` owns only the fresh default-on opt-out gate, deterministic advisory rendering/application, fixed 250 ms deadline, and cached read-only `TurnPrep` selection inside one blocking closure. Keep all three call sites in `main.rs` with exact caller-cwd, request/permit/acknowledgement, event/runtime, raw-versus-guided prompt, and browser-layer order. Helper-owned warnings remain fixed-field, while delegated loader path logs, unsanitized advisory names/descriptions, and uncancelled timed-out work behind the process-wide cache lock remain documented separate risks rather than extraction scope.
- Agent SSE replay is globally bounded by both 2,048 events and 32 MiB of serialized event payload. Oldest envelopes evict until both limits hold; an individually oversized event remains live but is not replay-retained. Preserve full live delivery and the existing explicit subscriber-lag signal.
- Build provenance must follow normal branch commits and linked worktrees: `build.rs` watches Git `HEAD`, its resolved symbolic branch ref, and `packed-refs`; `/health` and `/ready` must report the exact main-built revision after deployment.
- `AgentEvent::TurnCheckpoint` is an internal persistence signal consumed by `ocean-agent`; daemon bridges must filter it rather than exposing transcript deltas on SSE.
- The Track-0 projection routes (`GET /v1/rooms`, detail, snapshot, events) are retired. Preserve `/v1/rooms/persistent/*` and `/v1/rooms/{room_id}/livekit-token`; these are separate durable-collaboration and media contracts.
- The explicit method/path set in `app_router`, `banner_routes()`, and the operator-guide HTTP quick reference must remain identical. Preserve Axum's default 404/405 fallback and the global layer order: HTTP tracing outside CORS outside route dispatch.
- `POST /v1/longhouse/inspect` is a read-only projection of the exact ordinary preparation ranking: preserve the shared request/cwd roots/cache/cap/exact-token scorer/tie-break path, path-redacted compact response, raw-prompt/session/cwd/body non-echo (only contributing prompt terms and the additive `exact_name_phrase` flag are returned), and separation from turn execution, capabilities, models, and automatic prompt injection.
- `harness_profile.rs` owns only the effective per-turn profile gates currently applied to `PromptControl`: hashline edits and artifact spill. LSP/memory remain globally registered, and stream rules, rich context, and minimization remain unavailable rather than logged-only profile claims. Preserve the unknown/missing → CLI fallback; `acp-zed` resolves explicitly with the same effective gates as its former fallback. New external surface classifications require a separate cross-repository policy decision.

## Work Guidance

- Keep HTTP/SSE contracts stable for both `ocean-tui` and `ocean-surface`.
- Caller-submitted and resumed turns execute in the caller's cwd; never pin them to the daemon launch cwd or the first session cwd. Internal auto-convene for a legacy persistent room with no workspace binding retains its existing neutral daemon cwd compatibility fallback until a separately approved migration; startup rejects repository cwd.
- Build from up-to-date `main` before daemon restarts when doing operator work.
- Prefer narrow route tests for API behavior and workspace checks before merge.

## Verification

- `cargo test -p ocean-daemon bus::tests::`
- `cargo test -p ocean-daemon fulfillment -- --nocapture`
- `cargo test -p ocean-daemon cors::tests:: -- --nocapture`
- `cargo test -p ocean-daemon component_event_ -- --nocapture`
- `cargo test -p ocean-daemon event_adapter::tests:: -- --nocapture`
- `cargo test -p ocean-daemon fs_ -- --nocapture`
- `cargo test -p ocean-daemon metrics::tests:: -- --nocapture`
- `cargo test -p ocean-daemon model_catalog_ -- --nocapture`
- `cargo test -p ocean-daemon model_roles_ -- --nocapture`
- `cargo test -p ocean-daemon role_resolution_ -- --nocapture`
- `cargo test -p ocean-daemon project -- --nocapture`
- `cargo test -p ocean-daemon recall_registry -- --nocapture`
- `cargo test -p ocean-daemon recall_route -- --nocapture`
- `cargo test -p ocean-daemon request_ -- --nocapture`
- `cargo test -p ocean-daemon permission_ -- --nocapture`
- `cargo test -p ocean-daemon persistent_room_http_ -- --nocapture`
- `cargo test -p ocean-daemon room_ -- --nocapture`
- `cargo test -p ocean-daemon at_mention_queues_turn_and_posts_reply_back -- --nocapture`
- `cargo test -p ocean-daemon closed_persistent_room_preserves_audit_http_asymmetry -- --nocapture`
- `cargo test -p ocean-daemon workspace_policy::tests:: -- --nocapture`
- `cargo test -p ocean-daemon yolo_settings_ -- --nocapture`
- `cargo test -p ocean-daemon longhouse_inspect -- --nocapture`
- `cargo test -p ocean-daemon longhouse_preparation_ -- --nocapture --test-threads=1`
- `cargo test -p ocean-daemon longhouse_turn_preparation_ -- --nocapture --test-threads=1`
- `cargo test -p ocean-daemon harness_profile -- --nocapture`
- `cargo test -p ocean-daemon router_contract -- --nocapture`
- `cargo test -p ocean-daemon`
- `cargo check --workspace`
- Manual daemon health check when route/startup behavior changes: `curl http://127.0.0.1:4780/health`

## Child devlog Index

No child boundaries defined within `ocean-daemon/` at this time.
