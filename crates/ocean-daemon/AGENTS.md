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
  advertise only bounded read-only workspace inspection tools plus
  `propose_handoff`, and mutate only through the existing session/message/turn
  routes after an explicit Surface click.
- Realtime `purpose: "conversation"` may advertise bounded read-only
  `list_workspace` / `read_workspace_file` tools only when the daemon resolves
  the supplied session's persisted `workspace_root`/`cwd` back to a registered project
  or live linked worktree. Return that canonical root with the secret for frozen
  Surface fulfillment; never accept a browser/model-nominated conversation root,
  and keep project-less/session-less conversations on render + handoff only.

- Session behavior lives in `ocean-agent`; route changes must not create a separate session model.
- Product turns and legacy/call turns (legacy requests pin a session id before
  admission) take the shared non-blocking session operation lease before
  `TurnStarted`, invalidation, or request registration and retain it through
  persistence plus terminal publication. Durable-room turns wait on the same
  lane after their durable queued footprint and before request registration, so
  a committed trigger is never dropped. Every mutation path emits a scoped
  agent-rail lifecycle event or `ocean.session_changed` invalidation while
  leased, making synchronized snapshots replay-safe even if execution aborts.
- `POST /v1/sessions/{id}/compact` takes a turn permit from the shared limiter
  (429 at capacity), rejects a busy session lease immediately with 409, emits a
  session-scoped replay fence while holding the lease, and delegates model work
  to `AgentRuntime::compact_session_with_lease`. Successful/no-op responses
  include the lease-protected visible transcript snapshot plus that fence.
  `GET /v1/sessions/{id}/sync` is refresh-only, returns the same bounded public
  snapshot/fence, and rejects a busy lease with 409. Only genuine absence maps
  to 404; unreadable/internal errors are sanitized 500s and provider failures
  remain `200 ok:false`. No model compaction logic belongs in the daemon.
- Session-config RPC v1 is `GET/PATCH /v1/agent/sessions/{id}/config`;
  PATCH accepts strict model-only JSON (malformed or extra-key bodies return
  exact `400 {"ok":false,"error":"invalid_request"}`), persists the catalog
  model/provider pair, and emits one session-scoped `SessionConfigChanged`.
  Permission state is read-only and `permission_mode.env_override` is a boolean
  presence flag. Only
  absent sessions map to 404; corrupt/internal reads and writes return sanitized
  500s. Turn selection is explicit model > resolved role > named-agent model >
  session pin > global; an explicitly named unresolved role stops at global,
  and `TurnStarted` announces the model passed to execution before any separately
  announced provider reroute.
- `GET /v1/agent/history/search` is a bounded adapter over ocean-agent's persisted display-transcript search (default 20, clamp 1..50); it performs no provider or embedding calls, allows at most two concurrent scans, and returns a capacity response before deserialization when raw session files exceed the 64 MiB request budget.
- Subagent roles, dispatch, lifecycle, and orchestration are extension-owned. Do not add daemon-native `task`/`spawn_worker`/fleet machinery. The daemon may expose generic permission-gated turn, cancellation, capability-provider, and extension event/tool seams; current `/v1/subagents/spec` and folder-agent subagent metadata remain compatibility surfaces until a separately approved extension migration.
- Slack Socket Mode, API/credential access, reconnects, replies, files, and real Canvas delivery are `ocean-slack` extension concerns. Private `slack_canvas_fulfillment.rs` is only the temporary typed host ingress/readback, runtime lookup, scoped-event, and lifecycle-enforcement compatibility seam; do not grow it into a second Slack transport authority.
- `component_interaction.rs` is a leaf HTTP fulfillment adapter over the runtime-owned `COMPONENT_WAIT_REGISTRY`: preserve exact key scoping, remove-before-send semantics, poison/error responses, and runtime ownership of wait registration, timeout, and ordinary cleanup.
- `model_roles.rs` owns once-at-startup fail-open `[roles]` loading and pure turn/advisor alias precedence only. Keep `AppState`, warning call sites, provider routing/readiness, persisted model selection, and advisor execution in their existing owners; do not trim or validate role strings during extraction.
- Post-turn advisor execution is best-effort and isolated from the completed main turn: preserve activation/alias precedence, use only the dedicated fixed two-permit `AppState` limiter with immediate fail-open saturation, hold its owned permit across the provider call, and keep the call under a fixed 30-second timeout. Advisor Extension payloads retain `note`/`severity`/`model` and carry the authoritative originating `turn_id`; logs and Prometheus labels must never contain prompt, response, or note content, and metrics labels remain fixed-cardinality outcomes only.
- `request_control.rs` owns the private request/permission registry records, status-only snapshots, registration/handle mechanics, waiter cancellation, and bounded status transitions. Keep `AppState`, permission policy/orchestration, decision-token verification, HTTP/event mapping, GC scheduling, active-turn projection, and shutdown draining in composition; preserve sender/handle ownership and drop registry locks before signaling or awaiting.
- `recall_registry.rs` owns only the private in-memory `title_id -> RecallVote` store, first-cast tally construction, distinct-voter casting, poison recovery, and named-tally removal. Keep UUID/live-title validation, persisted title and daemon-held Revoker authority, carried-outcome execution, HTTP mapping, and successful-only cleanup ordering in composition. Preserve the existing memory-only, unbounded retention of abandoned tallies during behavior-neutral extraction.
- `persistent_rooms.rs` owns the private shared room-store handle/lock adapters, durable-room HTTP lifecycle and paging handlers, Local/federated message routing, invite/redeem/agent-registration HTTP adapters, sovereign confirmed-trigger dispatch, room-agent auto-convene helpers, and the post-commit transcript/access wake helpers shared with federation. Keep `AppState`, startup store opening, `room_routes()`, call persistence/retries, and LiveKit token authorization in composition. Local messages preserve exact persisted-row → event → audit-attempt → spawn ordering; federated intent allocation is outbox-only under the stable admission/store linearization, confirmed claims revalidate a current safe locally-owned Agent roster member plus private binding before at-most-once dispatch without audit rows, and bound-agent replies return through the outbox. Preserve one concrete `SqliteRoomStore`, poison recovery, no store guard across network await/event/spawn, stable room-agent session identity, three-state permission authority, and closed-room audit replay.
- `room_federation.rs` owns the restart-safe outbound Bedrock room client and AppState-owned `FederationSupervisor`: strict origin-only client construction, header-bearer SSE receive/reconnect, roster/presence projection, durable Pending sender scans, stable producer/control admission, owner bootstrap, idempotent redemption/self-join recovery, safe-agent registration, status/revoke policy, and post-commit wakes/dispatch hints. SQL remains `ocean-store`; HTTP handlers and local agent execution remain `persistent_rooms.rs`. Keep exactly one task tree per room, serialize stop/join before the next epoch, start existing credentials before the bounded-concurrency all-row recovery worker, select sender Notify + bounded periodic durable scan + cancellation, and never hold `RoomStoreHandle` across network I/O or `.await`. POST 201 never mutates transcript/outbox; ordered SSE is the only ingest/removal rail. Presence follows the authenticated SSE lease, not merely the access-state label. Bearers, registration keys, and invite codes (except intentional invite success) never enter logs/errors/debug output or surface projections.
- Persistent-room Agent participants bind to folder-as-agent definitions through the shared explicit-result resolver: joins reject unresolved names; mention handling resolves before any `room_trigger`/`auto-convene` footprint and emits only an honest System note on failure; execution re-resolves and applies instructions, model, tool allowlist, and subprocess capabilities, failing closed before request registration. A valid data-only definition with no overrides remains resolved. The non-room named-agent turn stays fail-open, and room cwd, permissions, YOLO, decision-token, and tool-availability posture remain unchanged.
- `GET /v1/rooms/persistent/{key}/events` is the open, non-call persistent-room merged SSE tail: initial full `room_access` projection frame (no `event.id`, whole committed state), unchanged id-bearing `room_message` replay via `Last-Event-ID` (numeric or 400; wins over `after_seq`), then post-commit dual-bus tail (`room_access` access-update frames on `RoomAccessWakeBus`, `room_message` frames on `RoomWakeBus`). Both bus wake hints are payload-free; relevant and lagged hints reread from SQLite for durability. Message gap recovery pages ascending; access dedup compares the full projection. Unknown/closed rooms return 404, `call:` rooms return the typed unsupported rejection, and the stream uses the shared 3-second keepalive plus shutdown wrapper. Every non-call production transcript writer must publish only after its allocating transaction commits. Tail tasks must select downstream `tx.closed()` against both the replay/live test seam and the idle broadcast wait so a disconnected HTTP client releases its `AppState` and wake receiver without requiring a new room hint. Access-wake cleanup follows the same pattern on a dedicated `RoomAccessWakeBus` so heavy transcript tails never back-pressure access subscribers.
- `POST /v1/rooms/persistent/{key}/outbox/retry` is the strict retry adapter: accepts `{ "client_event_id": "<id>" }`, returns 202 on durable requeue, 403 revoked, 404 for an unknown room or item, 409 pending/local, 400 for malformed or non-object body, or sanitized store 500 on internal error. No network or provider calls; the adapter owns only HTTP validation, store lookup, and wake publication.
- `longhouse_preparation.rs` owns only the state-free prepare/inspect/workflow HTTP request/projection adapters. Preserve exact Axum extractor/method/default envelopes, PR #292 exact-token evidence and redaction, cwd roots/cache choice, and all three `spawn_blocking` fail-open lanes. Keep route composition, librarian query/fetch, compatibility subagent spec, and all governance/title/escrow/recall state in `main.rs`; ranking/cache algorithms remain in `ocean-longhouse`. The deferred cached skill-path symlink-retarget finding must be resolved separately before any librarian extraction.
- `longhouse_turn_preparation.rs` owns only the fresh default-on opt-out gate, deterministic advisory rendering/application, fixed 250 ms deadline, and cached read-only `TurnPrep` selection inside one blocking closure. Keep all three call sites in `main.rs` with exact caller-cwd, request/permit/acknowledgement, event/runtime, raw-versus-guided prompt, and browser-layer order. Helper-owned warnings remain fixed-field, while delegated loader path logs, unsanitized advisory names/descriptions, and uncancelled timed-out work behind the process-wide cache lock remain documented separate risks rather than extraction scope.
- `longhouse_topics.rs` owns only the detached scripted demo producer and read-only topic list/detail HTTP adapters over the existing `AppState::{agent_events,longhouse}` handles. Preserve the immediate acknowledgement, exact 17-event/delay/content/ID/tally sequence, projection-before-publication with no lock across publish/await, demo skip-on-poison/live-publication asymmetry, list/detail poison recovery, and exact UUID/error/envelope behavior. Keep `longhouse_routes()`, `AppState`, startup's one registry shared with runtime extensions, HTTP/SSE composition, real convene/model selection, and every title/escrow/revoker/recall/breach/board control path in `main.rs`; do not add a helper, registry, service seam, or broader governance authority here.
- Agent SSE replay is globally bounded by both 2,048 events and 32 MiB of serialized event payload. Oldest envelopes evict until both limits hold; an individually oversized event remains live but is not replay-retained. Empty, non-UTF-8, malformed, foreign-session, unknown, or evicted `Last-Event-ID`, and live broadcast lag, emit the existing `event:error` frame with a typed `AgentReplayGap` body and `reset_required:true`; never silently attach live-only after an unavailable anchor. Gap bounds are filtered to the requested session and remain diagnostic opaque UUIDs. Preserve full live delivery and first-party error-frame compatibility.
- Build provenance must follow normal branch commits and linked worktrees: `build.rs` watches Git `HEAD`, its resolved symbolic branch ref, and `packed-refs`; `/health` and `/ready` must report the exact main-built revision after deployment.
- `AgentEvent::TurnCheckpoint` is an internal persistence signal consumed by `ocean-agent`; daemon bridges must filter it rather than exposing transcript deltas on SSE.
- The Track-0 projection routes (`GET /v1/rooms`, detail, snapshot, events) are retired. Preserve `/v1/rooms/persistent/*` and `/v1/rooms/{room_id}/livekit-token`; these are separate durable-collaboration and media contracts.
- The explicit method/path set in `app_router`, `banner_routes()`, and the operator-guide HTTP quick reference must remain identical. Preserve Axum's default 404/405 fallback and the global layer order: HTTP tracing outside CORS outside route dispatch.
- `github.rs` owns exactly five public, read-only `GET /v1/repo/github/{project_id}/*` projections: pulls, one pull, full-head-SHA checks, reviews, and commits. Resolve only the registered workspace-root `origin`; accept only exact GitHub remote forms; never send credentials or `Authorization`; and add no aggregate or write route. Preserve route-owned sanitized extractor errors, two-phase byte/field/vector bounds, Link-only pagination, full-SHA Moka singleflight caches (256 entries/60s), pinned GitHub headers, and bounded kill-on-drop git stdout handling.
- `POST /v1/longhouse/inspect` is a read-only projection of the exact ordinary preparation ranking: preserve the shared request/cwd roots/cache/cap/exact-token scorer/tie-break path, path-redacted compact response, raw-prompt/session/cwd/body non-echo (only contributing prompt terms and the additive `exact_name_phrase` flag are returned), and separation from turn execution, capabilities, models, and automatic prompt injection.
- `harness_profile.rs` owns only the effective per-turn profile gates currently applied to `PromptControl`: hashline edits and artifact spill. LSP/memory remain globally registered, and stream rules, rich context, and minimization remain unavailable rather than logged-only profile claims. Preserve the unknown/missing → CLI fallback; `acp-zed` resolves explicitly with the same effective gates as its former fallback. New external surface classifications require a separate cross-repository policy decision.
- `observatory_auth.rs` owns the typed Axum auth-state/extractor seam: accept `Authorization: Bearer` or the `Authorization-Observer` compatibility cookie, never query credentials, and map every credential failure to 401. Startup loads the secure secret, atomically mints the mode-0600 boot-bound `.ocean/observatory-token`, mounts typed extension state, and rotates the token file every ten minutes; no public token-creation route exists, the daemon never emits `Set-Cookie`, and the signing secret is never distributed. Observer tokens are stateless bearers replayable within scope until expiry or daemon restart; their nonce provides issuance uniqueness, not one-time consumption. Any compatibility-cookie issuance and its `Secure`/`HttpOnly`/`SameSite=Strict`/scoped-Path attributes belong to the authenticated Ocean Surface proxy.
- `observatory_adapter.rs` owns the one-way bridge from the runtime `AgentTurnEvent` stream to redacted Observatory facts: content-bearing variants (text/thinking deltas, tool chunks, component/canvas/extension payloads) return `None`, tool args/output bodies and free-text errors/titles/paths are stripped by construction, and reroute/error reasons are classified to fixed codes. Startup runs the restart interruption sweep (nonterminal executions close as canceled), emits `DaemonStarted`, and spawns the append pump off `AgentEventBus::subscribe_with_full_replay`; graceful shutdown appends `DaemonStopping`. Pump lag is `warn!`-loud because the pump is the durability path.
- `observatory.rs` owns the read-only Observatory data routes (`GET /v1/observatory/snapshot|events|replay`) behind the `ObservatoryAuth` extractor: snapshot/replay answer 410 only against the durable retention-boundary watermark (a natural log start at cursor 1 is not a crossing), the SSE tail always replays from the durable store before live attach and emits explicit `reset`/`error`/`stream.gap` frames instead of silently skipping, and every response carries the manifest §7.4 no-store/X-Observatory headers. The store is optional at runtime: open failure degrades routes to explicit 503, never daemon startup. V1 projection gaps (empty session/turn/request ids, empty attention shelf) stay empty until Task 6 wires real daemon facts.

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
- `cargo test -p ocean-daemon session_config_ -- --nocapture`
- `cargo test -p ocean-daemon unresolved_role_executes_global_and_turn_started_matches_it_despite_session_pin -- --nocapture`
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
- `cargo test -p ocean-daemon longhouse_topic_projection_ -- --nocapture --test-threads=1`
- `cargo test -p ocean-daemon harness_profile -- --nocapture`
- `cargo test -p ocean-daemon router_contract -- --nocapture`
- `cargo test -p ocean-daemon github::tests -- --test-threads=1`
- `cargo test -p ocean-daemon observatory_auth -- --nocapture`
- `cargo test -p ocean-daemon observatory:: -- --nocapture`
- `cargo test -p ocean-daemon persistent_rooms -- --test-threads=1`
- `cargo test -p ocean-daemon`
- `cargo check --workspace`
- Manual daemon health check when route/startup behavior changes: `curl http://127.0.0.1:4780/health`

## Child devlog Index

No child boundaries defined within `ocean-daemon/` at this time.
