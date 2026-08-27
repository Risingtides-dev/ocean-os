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

- `POST /v1/ocean-buddy/events` is the deliberately narrow first Buddy ingress: it accepts only a typed mocked `attached` lifecycle event, carries attachment metadata but no image bytes, performs no camera/session/tool work, and returns a typed Watch result card. Watch approval remains in the Watch-to-iPhone adapter flow.
- Session behavior lives in `ocean-agent`; route changes must not create a separate session model.
- `extension_lifecycle.rs` is the metadata-only Stage A lifecycle authority: it owns deterministic source adaptation, synchronous registered-project classification, globally ordered non-blocking publication, bounded correlation, request-scoped exactly-once terminal authority, cleanup, and boot-local count/byte retention. `main.rs` wires only the nine ratified authoritative producers; `session_stopped` remains schema-only. This boundary must not gain process launch, transport, registry mutation, routes, persistence, Observatory coupling, or content-bearing fields.
- `extension_registry.rs` is the sole coherent read-only extension registry authority. It preserves the three accepted A0 schemas, treats absent `service-grants.json` as the one empty A0 upgrade form, and derives supported and unsupported activation authority only after the same shared-lock, descriptor/reparse-safe four-file plus artifact/service/capability/binding validation. A2a has no mutation or durable first-publication marker, so A3a must atomically introduce that marker and make marked companion absence fail closed; A2a must not preempt it. Inspect/doctor remain Phase 1-compatible and execute/probe nothing. The internal `registry-portability-check` feature may remove only daemon AppState/Axum route coupling so the isolated Windows harness can include this actual source; it must not weaken reader or platform validation.
- `extension_service.rs` is the Stage A2a–A2b macOS/Linux supervisor: exact acknowledged native services, strict hello/ready stdio, immutable activation scope/epoch and replay floor, bounded replay/live data plus coalesced prioritized controls, and a fixed-size delivered-sequence ACK ledger. Ready/reset/replay attach shares one cancellation-aware deadline and drains legal child frames concurrently; event ACK eligibility begins only after a successful complete write, with an exact in-flight ACK buffered but non-authoritative until then. Caller-owned frame-prefix storage makes fragmented child frames lossless across `select!` cancellation. At the full live ACK window, event dequeue pauses while child frames receive a bounded prioritized drain opportunity; only a genuinely undrained window fails the connection. Heartbeat timeout begins only after a successful ping write. Restart/backoff/circuit history survives scope-only epoch changes and resets only on disable→enable, trusted digest change, stable-health reset, or daemon restart. It preserves A2a's descriptor/file-id-bound executable and assigned roots, `env_clear` plus explicit ordinary/`env:` secret bindings, typed post-ready failure causes, bounded structurally redacted stderr counters, descriptor-relative temp cleanup accounting and retry ownership, retained-leader generation-safe group cleanup, and one immediate bounded exceptional-authority retry in the managed production task. Exceptional cleanup aborts/bounds stderr collection before returning authority; the managed owner retains a still-failed process/temp root for later reconciliation or shutdown, and supervisor shutdown executes cleanup ownership instead of abort-detaching it. Project reconciliation is checked and must complete epoch/filter/reap work before success. `extension_service_unsupported.rs` reprojects common validated authority on project changes, remains non-probing `unsupported_platform`, and opens no secret, assigned root, or process. Neither supervisor owns registry mutation, routes, durable replay, package acquisition, or child-originated commands.
- Product turns and legacy/call turns (legacy requests pin a session id before
  admission) take the shared non-blocking session operation lease before
  `TurnStarted`, invalidation, or request registration and retain it through
  persistence plus terminal publication. The registry's exact terminal
  transition is authoritative for the agent-rail `TurnFinished`: a cancel race
  must publish `Cancelled` with no failure error even when the runtime result
  was res-derived as failed or completed, and an orphan guard that settles a
  cancelling task must emit the matching cancelled terminal frame rather than
  leaving the agent rail open. Repeated settlement of an already-terminal
  cancelled request must not refire the terminal callback or emit another frame.
  Durable-room turns wait on the same
  lane after their durable queued footprint and before request registration, so
  a committed trigger is never dropped. An ordinary product turn registers its
  request, mints terminal authority, and constructs/captures the orphan guard
  before lifecycle admission; abort-before-first-poll therefore still settles
  exactly one terminal fact. The configured turn permit pool is clamped below
  terminal-authority capacity, and authority-mint failure rejects admission
  rather than running a turn without exactly-once terminal bookkeeping. Explicit project identity must match the resolved
  workspace/session scope; mismatches fail before lifecycle publication. Every
  mutation path emits a scoped agent-rail lifecycle event or
  `ocean.session_changed` invalidation while leased, making synchronized
  snapshots replay-safe even if execution aborts.
- `POST /v1/sessions/{id}/compact` takes a turn permit from the shared limiter
  (429 at capacity), rejects a busy session lease immediately with 409, emits a
  session-scoped replay fence while holding the lease, and delegates model work
  to `AgentRuntime::compact_session_with_lease`. Successful/no-op responses
  include the lease-protected visible transcript snapshot plus that fence.
  `GET /v1/sessions/{id}/sync` is refresh-only, returns the same bounded public
  snapshot/fence, and rejects a busy lease with 409. Only genuine absence maps
  to 404; unreadable/internal errors are sanitized 500s and provider failures
  remain `200 ok:false`. No model compaction logic belongs in the daemon.
- `POST /v1/agent/sessions` accepts an optional catalog model and persists that
  model/provider atomically with the new session at `config_revision: 1` before
  returning or publishing `SessionCreated`; unknown model ids fail 400 without
  creating a session. This is the first-turn pin path, not a create→PATCH pair.
- Session-config RPC v1 is `GET/PATCH /v1/agent/sessions/{id}/config`;
  PATCH accepts strict model-only JSON (malformed or extra-key bodies return
  exact `400 {"ok":false,"error":"invalid_request"}`), persists the catalog
  model/provider pair plus a monotonic legacy-default-zero `config_revision`,
  returns that revision from GET/PATCH and synchronized snapshots, and emits it
  on exactly one session-scoped `SessionConfigChanged`.
  Model metadata does not mutate transcript history, so this route must not also
  emit the generic `ocean.session_changed` sync invalidation. Permission state
  is read-only and `permission_mode.env_override` is a boolean
  presence flag. Only
  absent sessions map to 404; corrupt/internal reads and writes return sanitized
  500s. `model_source` comes from ocean-agent's persisted `config_revision`
  authority; model/global comparison is used only for revision-zero inherited
  or legacy records. Turn selection is explicit model > resolved role > named-agent model >
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
- `persistent_rooms.rs` owns the private shared room-store handle/lock adapters, durable-room HTTP lifecycle and paging handlers, Local/federated message routing, invite/redeem/agent-registration HTTP adapters, sovereign confirmed-trigger dispatch, room-agent auto-convene helpers, and the post-commit transcript/access wake helpers shared with federation. Keep `AppState`, startup store opening, `room_routes()`, call persistence/retries, and LiveKit token authorization in composition. Local messages preserve exact persisted-row → event → audit-attempt → spawn ordering; federated intent allocation is outbox-only under the stable admission/store linearization, confirmed claims revalidate a current safe locally-owned Agent roster member plus private binding before at-most-once dispatch without audit rows, and bound-agent replies return through the outbox. Preserve one concrete `SqliteRoomStore`, poison recovery, no store guard across network await/event/spawn, stable room-agent session identity, three-state permission authority, and closed-room audit replay. G3 integrity invariants: local post author classification is exact-roster and fail-closed (whitespace variants, non-roster ids, and client-claimed Agent/System kinds are typed 403s with no write); the message wire rejects any client-supplied `session_id` (`deny_unknown_fields`); agent replies mint their session attribution inside `append_room_agent_reply` from the (room, agent) pair — never from a caller parameter; and a convened answer threads under the resolved thread ROOT (a reply trigger resolves its own parent) with stale-parent degradation to top-level, never a dropped reply.
- `room_attachments.rs` owns the BYTES behind room context files and nothing else: the attachment blob root carried on `AppState` (resolved once at startup beside `rooms.db`, injected in tests rather than re-read from env), per-room directory derivation as `sha256(room key)` hex, server-minted `[0-9a-f]{32}` ids re-validated before every filesystem call, the 8 MiB cap enforced both by the route's mandatory `DefaultBodyLimit` layer and by a typed `attachment_too_large` handler rejection, display-only filename sanitization, the ported forged-author gate (`forged_attachment_author` for a client claiming an Agent/System identity), and the four upload/list/download/delete adapters. Load-bearing order: validate → cap → room pre-check → forged-author gate → write+fsync+rename the blob → commit the row, with a best-effort unlink on store failure, because an orphan blob is unreferenced garbage while an orphan row is a download that 500s forever. Downloads are ALWAYS `application/octet-stream` + `X-Content-Type-Options: nosniff` and re-verify length and sha256 against the row before serving: the declared content type is recorded and never echoed, never sniffed, and never quoted into a transcript line. Upload metadata travels in the query string, not custom headers, because `cors.rs` allows only `content-type`/`authorization` and an `X-Ocean-*` header would pass curl and fail the browser preflight. SQL and the transcript marker remain `ocean-store`; route composition, `AppState`, and the banner/operator-guide parity set remain `main.rs`. This module must NOT grow agent context assembly or prompt injection — that is Ocean Rooms v2 §7's `ContextPolicy`/`ContextMount` model, which the root `AGENTS.md` forbids implementing from the proposal alone. An agent reads a room's attachments over HTTP like any other client. Write authority matches the neighbouring room routes: `uploader_id`/`actor_id` are caller-asserted and only roster-checked, so any roster member (or anything that can reach loopback) can upload or delete — the existing deployment posture, now true of file bytes.
- `room_federation.rs` owns the restart-safe outbound Bedrock room client and AppState-owned `FederationSupervisor`: strict origin-only client construction, header-bearer SSE receive/reconnect, roster/presence projection, durable Pending sender scans, stable producer/control admission, owner bootstrap, idempotent redemption/self-join recovery, safe-agent registration, status/revoke policy, and post-commit wakes/dispatch hints. SQL remains `ocean-store`; HTTP handlers and local agent execution remain `persistent_rooms.rs`. Keep exactly one task tree per room, serialize stop/join before the next epoch, start existing credentials before the bounded-concurrency all-row recovery worker, select sender Notify + bounded periodic durable scan + cancellation, and never hold `RoomStoreHandle` across network I/O or `.await`. POST 201 never mutates transcript/outbox; ordered SSE is the only ingest/removal rail. Presence follows the authenticated SSE lease, not merely the access-state label. Bearers, registration keys, and invite codes (except intentional invite success) never enter logs/errors/debug output or surface projections.
- Persistent-room Agent participants bind to folder-as-agent definitions through the shared explicit-result resolver: joins reject unresolved names; mention handling resolves before any `room_trigger`/`auto-convene` footprint and emits only an honest System note on failure; execution re-resolves and applies instructions, model, tool allowlist, and subprocess capabilities, failing closed before request registration. A valid data-only definition with no overrides remains resolved. The non-room named-agent turn stays fail-open, and room cwd, permissions, YOLO, decision-token, and tool-availability posture remain unchanged.
- `GET /v1/rooms/persistent/{key}/events` is the open, non-call persistent-room merged SSE tail: it bootstraps full `room_access` and JS-safe decimal-string `room_read_cursor` projections without `event.id`, preserves id-bearing `room_message` replay via `Last-Event-ID` (numeric or 400; wins over `after_seq`), then tails three post-commit wake buses (`RoomAccessWakeBus`, `RoomReadCursorWakeBus`, `RoomWakeBus`). Wake hints are payload-free; relevant and lagged hints reread SQLite. Cursor tails use the daemon Local principal or the credential-owned federated human principal, suppress unsupported/transient projections rather than emitting false clears, and deduplicate the unified `{room_id,read_seq}` wire body. Message gap recovery pages ascending; access dedup compares the full projection. Unknown/closed rooms return 404, `call:` rooms return the typed unsupported rejection, and the stream uses the shared 3-second keepalive plus shutdown wrapper. Every non-call production transcript writer must publish only after its allocating transaction commits. Tail tasks must select downstream `tx.closed()` so disconnected clients release state and wake receivers without a new hint.
- `POST /v1/rooms/persistent/{key}/outbox/retry` is the strict retry adapter: accepts `{ "client_event_id": "<id>" }`, returns 202 on durable requeue, 403 revoked, 404 for an unknown room or item, 409 pending/local, 400 for malformed or non-object body, or sanitized store 500 on internal error. No network or provider calls; the adapter owns only HTTP validation, store lookup, and wake publication.
- `room_summary.rs` owns on-demand transcript summarization for `POST /v1/rooms/persistent/{key}/summarize`. It writes exactly ONE well-known artifact id (`room-summary`, a `Note`), created at v1 and amended in place under the existing compare-and-swap forever — never a new artifact per call. The model turn is a single `AgentRuntime::complete_once` on `roles["summarize"] → roles["fast"] → the bound model`; no new provider client, no session, no tools. Reads take the NEWEST rows by deriving a tail cursor from `room_latest_durable_seq` before the existing `read_transcript_page` — the store's ascending `LIMIT` query would otherwise summarize the room's OLDEST page, and `after_seq` is exclusive from `seq` 0, so `tail_cursor` must not drop message 0. Execution is strictly three phases (store read → provider `.await` → store write) and never holds `RoomStoreHandle` across an await; phase 3 does read-then-CAS-write in ONE closure so the daemon-wide mutex serializes concurrent summarize calls and no retry loop is needed. The roster-author invariant is preserved, not bypassed: `requested_by` must be a non-Agent/System roster member, and a forged author, an unknown room, and a soft-closed room are all refused BEFORE any model turn is paid for. No-messages, whitespace-only model output, and `ArtifactUnchanged` are clean 200 answers, not faults; provider failure and timeout are fixed-text 502/504 that never carry the provider's own message, and no prompt, transcript, or summary content reaches logs. A federated room's summary is local-only and is never enqueued to the outbox.
- `longhouse_preparation.rs` owns only the state-free prepare/inspect/workflow HTTP request/projection adapters. Preserve exact Axum extractor/method/default envelopes, PR #292 exact-token evidence and redaction, cwd roots/cache choice, and all three `spawn_blocking` fail-open lanes. Keep route composition, librarian query/fetch, compatibility subagent spec, and all governance/title/escrow/recall state in `main.rs`; ranking/cache algorithms remain in `ocean-longhouse`. The deferred cached skill-path symlink-retarget finding must be resolved separately before any librarian extraction.
- `longhouse_turn_preparation.rs` owns only the fresh default-on opt-out gate, deterministic advisory rendering/application, fixed 250 ms deadline, and cached read-only `TurnPrep` selection inside one blocking closure. Keep all three call sites in `main.rs` with exact caller-cwd, request/permit/acknowledgement, event/runtime, raw-versus-guided prompt, and browser-layer order. Helper-owned warnings remain fixed-field, while delegated loader path logs, unsanitized advisory names/descriptions, and uncancelled timed-out work behind the process-wide cache lock remain documented separate risks rather than extraction scope.
- `longhouse_topics.rs` owns only the detached scripted demo producer and read-only topic list/detail HTTP adapters over the existing `AppState::{agent_events,longhouse}` handles. Preserve the immediate acknowledgement, exact 17-event/delay/content/ID/tally sequence, projection-before-publication with no lock across publish/await, demo skip-on-poison/live-publication asymmetry, list/detail poison recovery, and exact UUID/error/envelope behavior. Keep `longhouse_routes()`, `AppState`, startup's one registry shared with runtime extensions, HTTP/SSE composition, real convene/model selection, and every title/escrow/revoker/recall/breach/board control path in `main.rs`; do not add a helper, registry, service seam, or broader governance authority here.
- `longhouse_governance_control.rs` owns only the exact 13-definition claim/revoke/recall/breach/board HTTP adapter boundary. Revoke/recall/breach/board have no caller-authentication extractor: local exposure and CORS are the current deployment posture, not authentication. Recall deduplicates caller-supplied voter UUIDs and omitted/zero threshold clamps to one; accepted live-title breach reports accrue persisted strikes, while a post-close report returns 200 with zero strikes rather than 409; board state is an in-memory projection whose poisoned second lock can skip mutation while still publishing and returning success. Preserve these characterized behaviors without endorsing them; keep `AppState`, route composition, Revoker/recall construction, title storage, real convene/title grant-bind, provider execution, and raw-token delivery in composition.
- Agent SSE replay is globally bounded by both 2,048 events and 32 MiB of serialized event payload. Oldest envelopes evict until both limits hold; an individually oversized event remains live but is not replay-retained. Empty, non-UTF-8, malformed, foreign-session, unknown, or evicted `Last-Event-ID`, and live broadcast lag, emit the existing `event:error` frame with a typed `AgentReplayGap` body and `reset_required:true`; never silently attach live-only after an unavailable anchor. Gap bounds are filtered to the requested session and remain diagnostic opaque UUIDs. Preserve full live delivery and first-party error-frame compatibility.
- A failed admitted agent turn must retain its sanitized terminal error in daemon
  logs with turn, request, and session correlation; the one-shot SSE error frame
  and an `ok=false` summary alone are insufficient postmortem evidence. Never log
  prompt, response, tool output, credentials, or authorization material.
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
- `cargo test -p ocean-daemon extension_ -- --nocapture --test-threads=1`
- `cargo zigbuild --manifest-path crates/ocean-daemon/tests/windows-portability/Cargo.toml --features registry-portability-check --target x86_64-pc-windows-gnu`
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
- `cargo test -p ocean-daemon longhouse_governance_control_ -- --nocapture --test-threads=1`
- `cargo test -p ocean-daemon harness_profile -- --nocapture`
- `cargo test -p ocean-daemon router_contract -- --nocapture`
- `cargo test -p ocean-daemon github::tests -- --test-threads=1`
- `cargo test -p ocean-daemon observatory_auth -- --nocapture`
- `cargo test -p ocean-daemon observatory:: -- --nocapture`
- `cargo test -p ocean-daemon persistent_rooms -- --test-threads=1`
- `cargo test -p ocean-daemon room_attachments -- --nocapture`
- `cargo test -p ocean-daemon`
- `cargo check --workspace`
- Manual daemon health check when route/startup behavior changes: `curl http://127.0.0.1:4780/health`

## Child devlog Index

- `tests/windows-portability/` — isolated source-inclusion cross-build for the actual Windows registry reader and unsupported supervisor → `tests/windows-portability/AGENTS.md`
