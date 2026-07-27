# Ocean Extension Host Stage A Implementation Manifest

**Date:** 2026-07-27

**Status:** proposed — awaiting operator ratification

**Program:** Ocean Crew Stage A / Ocean Extension Phases 2–3

**Implementation authority:** none until operator ratification

**Parents:** [`2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`](2026-07-14-ocean-extensions-architecture-and-migration-manifest.md), [`2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md`](2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md)
**Evidence baseline:** `ocean-os` `77630bdaccb2829c45b0f137e3750d2ec089d212`; accepted Phase 1 implementation `399713b9fa1927a56a2cfaf4018cf5fa5c81658b`

## 1. Decision requested

Ratify one exact implementation contract for Crew Stage A on the accepted
Extension Phase 1 state reader now merged on `main`: add a metadata-only
lifecycle protocol and supervised native-service host, then add daemon-owned
local/pinned-Git package mutations. The gate is a no-op service installed,
trusted, enabled, supervised, restarted, disabled, and removed without affecting
ordinary sessions.

Ratification of this document selects the recommended choices in §2 and
authorizes only slices A1–A5 in §18; A0 is the already-merged prerequisite, not
new authority. It does not accept implementation in advance. Every authorized
slice still requires its tests, review, commit, upstream reconciliation, and
clean worktree.

## 2. Ratification choices

These choices close gaps that the parent manifests deliberately left open.
Ratification selects **Recommended** for each item.

| ID | Choice | Recommended selection | Rejected alternative |
| --- | --- | --- | --- |
| R1 | Service transport/auth | Host-supervised bidirectional stdio NDJSON v1. Pipe ownership plus the supervisor process record authenticates the child; the host injects identity in `host_hello`. | Child-facing bearer token or unauthenticated localhost HTTP/SSE. |
| R2 | Event replay | Boot-local, bounded, non-durable replay with explicit `lag`/`reset`; no Stage A cursor survives daemon restart. | Durable lifecycle log or silent live-only recovery. |
| R3 | `session_stopped` | Reserved v1 kind with **no Stage A producer** because current session authority has no stop/delete fact. Never infer it from idle, turn completion, client disconnect, switch, or daemon shutdown. | Inventing a terminal session event from a nearby but different fact. |
| R4 | Native authority | Service execution is allowed only after exact-digest trust plus an explicit acknowledgement that Stage A grants the child daemon-user-equivalent native authority. Cwd/state/env are assigned, not confined. Packages declaring `network` or `filesystem` service capabilities cannot be enabled in v1 as declaration policy only. | Treating declared names or assigned paths as enforced sandbox grants. |
| R5 | Platforms | Service supervision is supported on macOS and Linux using a new process group and group termination. Windows may inspect/manage packages but returns `unsupported_platform` before service activation. | Claiming descendant cleanup from direct-child kill, or adding an unreviewed Windows job-object implementation. |
| R6 | Secret resolver | V1 resolves only `env:<SOURCE_NAME>` references, through an explicit operator grant binding to a requested child environment name. | Ambient inheritance, positional pairing, values in manifests/state/argv, or implicit provider credential lookup. |
| R7 | Active mutation safety | Disable must synchronously stop a service before update/remove; active update/remove returns `extension_active`. | Detaching, orphaning, or silently killing a service as a side effect of package replacement. |

R4 is an honest limitation, not a sandbox claim. The acknowledgement and every
trust preview containing a native service must state: **activation grants this
process daemon-user-equivalent native authority; it may attempt network access,
read or modify any daemon-user-accessible filesystem data (including registry
and store bytes), and act outside assigned roots regardless of declared
capabilities.** Rejecting declared `network` or `filesystem` items is declaration
policy, not containment. Phase 4 reference extensions and broad third-party
activation remain blocked if they require enforcement not supplied by a
separately ratified sandbox lane. The Stage A no-op fixture declares neither.

## 3. Scope

Included:

- verification of the merged Phase 1 prerequisite A0 and its exact four-file
  Stage A upgrade rule;
- versioned lifecycle/service wire types and fixtures;
- structural redaction and authoritative production of nine event kinds plus
  one reserved schema-only `session_stopped` kind;
- macOS/Linux native service supervision, health, bounded diagnostics,
  restart/backoff/circuit breaking, and generation-safe process-group cleanup;
- daemon-assigned extension mutable state/cache/temp roots;
- list/install/trust/enable/disable/remove/update through daemon HTTP and the thin
  `ocean-rs` CLI;
- offline local-path and exact-revision public HTTPS Git sources;
- one exclusive-lock registry writer, transaction journal, atomic publication,
  crash recovery, grant-diff confirmation, and rollback;
- cached runtime status in list/inspect/status while doctor remains read-only,
  no-execution, and non-probing;
- the Stage A no-op-service E2E gate.

## 4. Non-goals and exclusions

This manifest explicitly excludes:

- Crew Stages B–D: generic execution requests, cancellation by host execution
  id, continuations, durable extension effects, interactive artifacts, Crew
  graphs, roles, members, joins, Undertow/Offshore adapters, and the Crew engine;
- Stage C's UI artifact lane, TUI/Surface component transport, workflow control,
  and Ocean Floor rendering;
- Telegram, Slack, Herdr, or any other integration-specific behavior;
- a Telegram sidecar, special secret scheme, special event, or daemon route;
- any core `Crew`, worker, subagent, task graph, `task`, `spawn_worker`, lane,
  scheduler, continuation, acceptance-ledger, or budget-ladder vocabulary;
- changes to `ocean-hooks`; its live Stop interceptor remains a separate legacy
  compatibility surface and is not the observer API;
- interceptors, prompt/transcript/provider mutation, mid-turn injection, or
  extension-originated control frames;
- durable lifecycle replay, exactly-once delivery, a marketplace, registry
  discovery, signatures, build steps, submodules, Git LFS, or package scripts;
- changing session JSON, current client SSE wire shapes, other client event
  outcomes, Observatory persistence, or Observatory authentication. The sole
  existing-client ordering correction authorized here is deferring the ordinary
  `SessionCreated` publication until successful admission (§8.2), without a
  wire-shape change.

## 5. Authority and prerequisite A0

Phase 1 is accepted and merged by PR #354 at `77630bda` (implementation commit
`399713b9`). It provides strict installed/trusted/enabled reads,
descriptor-anchored digest verification, read-only/no-execution inspect/doctor,
and the CLI read path. This satisfies prerequisite A0; it is evidence, not an
implementation slice authorized by this still-proposed document.

A1 starts only from a verified descendant of `77630bda`. A2a owns the bounded
read-only upgrade from accepted three-file state to the strict four-file Stage A
snapshot in §12.1. If that upgrade changes any accepted Phase 1 schema or makes
the three accepted files unreadable by the merged Phase 1 binary, stop for a
separate state-schema decision.

## 6. Source ownership and exact boundaries

| Path | Stage A responsibility | Forbidden responsibility |
| --- | --- | --- |
| `crates/ocean-extension/src/lib.rs`, `tests/manifest.rs` | Existing package/service declaration validation; only additive validation required by this contract. | Process launch, registry state, wire I/O, secret values. |
| `crates/ocean-agent-sdk/src/extension_lifecycle.rs` (new), `src/lib.rs` | Public `ocean.extension.service` v1 NDJSON DTOs, lifecycle envelope, closed metadata enums, limits, and golden fixtures. | Supervisor state, daemon buses, package mutations. |
| `crates/ocean-daemon/src/extension_registry.rs` (refactored from accepted `extension_state.rs` in A2a) | Sole coherent reader/writer for install/trust/enable/service-grant state, immutable store, journal recovery, read-only inspection, mutations, and quarantine adoption. | Service process ownership, event adaptation, or network acquisition while holding the registry lock. |
| `crates/ocean-daemon/src/extension_lifecycle.rs` (new) | Dedicated metadata-only adapter, sequence/ring, scope filtering, and authoritative event emissions. | Full `AgentEventBus` forwarding, persistence, process launch, Observatory writes. |
| `crates/ocean-daemon/src/extension_service.rs` (new) | Reconciliation, stdio connection, health, queues, restart policy, process groups, environment/secret injection, diagnostics, and runtime status cache. | Package-file parsing authority, session execution, Crew RPC. |
| `crates/ocean-daemon/src/main.rs` | Thin route composition, `AppState` handles, exact event call sites, startup reconciliation, and shutdown ordering. | A second registry, inline supervisor implementation, orchestration policy. |
| `crates/ocean-daemon/src/project_registry.rs` | Publish a committed registered-project snapshot/change hook; extension lifecycle consumes it synchronously before create/delete returns. | Extension enablement policy, per-event filesystem lookup, or package state. |
| `crates/ocean-cli/src/main.rs` | Thin HTTP client and exact commands in §15. | Direct writes under `<config_dir>/extensions`. |
| `crates/ocean-plugin` | No required change. Primitive minimal-environment ideas may be extracted only if dependency direction stays clean. | Recasting a long-lived service as a model-callable `Plugin`. |
| `crates/ocean-hooks` | No change; compatibility tests only. | Public observer delivery or service supervision. |
| `crates/ocean-observatory`, daemon Observatory adapter/routes | No behavior change; reference only for structural redaction patterns. | Extension service control, shared cursor/store/token, or child auth. |

If implementation needs a new crate, public route family, session event variant,
or hook-runner change outside this table, stop and amend this manifest first.

## 7. Service wire protocol

### 7.1 Transport and framing

- Protocol name: `ocean.extension.service`.
- Protocol version: integer `1`.
- Host writes child stdin; child writes stdout; both are piped and exclusively
  owned by one connection task. Child stderr is diagnostics only (§12.5).
- Each frame is one UTF-8 JSON object followed by `\n`. Blank lines, arrays,
  scalars, invalid UTF-8, duplicate keys, unknown fields, and trailing bytes
  after the JSON object are protocol violations.
- Maximum encoded frame including newline: 65,536 bytes. Read buffers stop at
  65,537 bytes and terminate the connection; no unbounded `read_line`.
- At most one frame is written at a time. A blocked stdin write has a 2-second
  deadline and fails the connection.
- Version 1 is strict. An unsupported version fails before readiness; there is
  no downgrade.

### 7.2 Host-injected identity and handshake

The host binds pipe handles to this immutable supervisor record:

```json
{
  "package_id": "example.noop",
  "package_version": "1.0.0",
  "package_digest": "sha256:<64-lowercase-hex>",
  "service_id": "lifecycle",
  "activation_revision": 7,
  "activation_epoch": "<uuid>",
  "replay_floor": "41"
}
```

The child cannot override that record. The first frame is:

```json
{"protocol":"ocean.extension.service","version":1,"frame":"host_hello","connection_id":"<uuid>","daemon_boot_id":"<uuid>","identity":{"package_id":"example.noop","package_version":"1.0.0","package_digest":"sha256:<hex>","service_id":"lifecycle","activation_revision":7,"activation_epoch":"<uuid>","replay_floor":"41"},"limits":{"max_frame_bytes":65536,"outbound_messages":256,"outbound_bytes":1048576,"heartbeat_interval_ms":10000,"heartbeat_timeout_ms":5000}}
```

Within the manifest `startup_timeout_ms` (default 5,000; accepted range
100–30,000), the child replies:

```json
{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":["daemon_started","turn_started"],"resume":null}
```

`subscriptions` must be a duplicate-free subset of the manifest's declared
`events`; it may narrow but never expand them. `resume`, when present, is
`{"daemon_boot_id":"<uuid>","activation_epoch":"<uuid>","after_sequence":"<u64 decimal>"}`.
The host then sends exactly one:

```json
{"protocol":"ocean.extension.service","version":1,"frame":"ready","subscriptions":["daemon_started","turn_started"],"replay":"boot_local","activation_epoch":"<uuid>","replay_floor":"41"}
```

An activation epoch is an in-memory UUID bound to one unchanged package digest,
service-grant set, negotiated manifest scope ceiling, and effective activation
scope. Its `replay_floor` is the global sequence current when the epoch is
created. Disable/re-enable, digest or grant change, or any project/global scope
change (including widening) creates a new epoch and advances the floor to the
then-current sequence before the new scope becomes deliverable. A supervisor
restart caused only by process failure retains the epoch; a daemon restart
changes the boot id and invalidates it.

After `ready`, a null resume receives only activation-eligible retained facts
strictly above `replay_floor` and then live events. `daemon_started` is replayed
only when it is eligible for the epoch; an epoch created after activation begins
may use the retained boot fact as the sole explicit exception, never session or
turn history. A resume is valid only when boot id and activation epoch match,
`after_sequence >= replay_floor`, and the cursor remains in actual retained,
epoch-eligible history. Otherwise the host sends `reset` before live attach.
Replay uses the project identity and registered/unregistered classification
captured when the event was published plus the epoch's immutable eligibility
snapshot; it never re-evaluates old frames solely against current wider scope.

Readiness means the handshake succeeded and the process is alive; it does not
mean an external integration is reachable. Pipe ownership authenticates the
child. No bearer, daemon decision token, Observatory token, provider credential,
or secret identity value is sent for authentication.

Allowed child frames after readiness are only:

- `ack {"sequence":"<u64 decimal>"}` — the highest event processed in this
  connection's delivered order. It is monotonic by delivered sequence, may skip
  globally filtered sequence numbers, and cannot exceed the highest event sent;
  this is the sole ACK definition;
- `pong {"nonce":"<uuid>"}` — response to the host's current ping;
- `status {"state":"ready|degraded","code":"<closed-code>"}` — optional,
  code from `external_unavailable|configuration_missing|rate_limited|unknown`;
- `shutdown_complete {}` — response during graceful shutdown.

Allowed host frames after readiness are only `event`, `lag`, `reset`, `ping`,
and `shutdown`. `lag` does not advance or invalidate the last ACK; after `reset`
the child discards its prior cursor and may ACK only events delivered after the
reset/live boundary. A child command, RPC method, event publication,
subscription expansion, or arbitrary payload is a protocol violation. Stage B
must version and ratify any mutation/RPC extension of this channel.

## 8. Lifecycle envelope and event mapping

### 8.1 Envelope

Every event frame has this shape and denies unknown fields:

```json
{
  "protocol": "ocean.extension.service",
  "version": 1,
  "frame": "event",
  "daemon_boot_id": "<uuid>",
  "sequence": "42",
  "event_id": "<uuid>",
  "occurred_at": "2026-07-27T18:00:00.000Z",
  "kind": "tool_finished",
  "scope": {
    "project_id": "<registered-project-uuid-or-null>",
    "session_id": "<uuid-or-null>",
    "turn_id": "<uuid-or-null>",
    "request_id": "<uuid-or-null>",
    "tool_call_id": "<uuid-or-null>",
    "permission_id": "<uuid-or-null>"
  },
  "metadata": {"tool_name":"read","outcome":"success","duration_ms":4,"output_bytes":128}
}
```

`sequence` is daemon-allocated, strictly increasing within one boot, and shared
by the lifecycle dispatcher. It is not an Observatory cursor. `event_id` is a
UUID v4. Timestamps are UTC RFC3339 with millisecond precision. Identifiers are
host-derived. Public `tool_call_id` is specifically the UUID minted by the
daemon runtime bridge; the runtime's opaque string id exists only in a private
per-turn correlation map. An End without a mapped host UUID records the fixed
`unmatched_tool_end` diagnostic and emits no fabricated lifecycle event.
`project_id` is present only when the daemon maps the session to a registered
project; no path-derived project identity is minted.

Closed metadata variants:

| Kind | Metadata |
| --- | --- |
| `daemon_started` | `{ "daemon_version": "<semver>" }` |
| `session_started` | `{}` |
| `turn_started` | `{}` |
| `permission_requested` | `{ "tool_name": "<bounded-tool-id>" }` |
| `permission_resolved` | `{ "outcome": "allowed|denied|cancelled" }` |
| `tool_started` | `{ "tool_name": "<bounded-tool-id>" }` |
| `tool_finished` | `{ "tool_name": "<bounded-tool-id>", "outcome": "success|error|cancelled", "duration_ms": <u64>, "output_bytes": <u64> }` |
| `turn_finished` | `{ "outcome": "completed|failed|cancelled|abandoned", "duration_ms": <u64>, "input_tokens": <u64-or-null>, "output_tokens": <u64-or-null>, "cache_read_tokens": <u64-or-null> }` |
| `session_stopped` | `{ "reason": "explicit_stop" }` — schema fixture only; no Stage A producer |
| `daemon_stopping` | `{ "reason": "graceful_shutdown" }` |

Tool names are capped at 256 UTF-8 bytes and must match the already admitted
runtime tool identifier. No other free text is allowed.

### 8.2 Authoritative source mapping for nine produced kinds and one reserved kind

| Declared event | Authoritative Stage A source | Exact rule |
| --- | --- | --- |
| `daemon_started` | Daemon startup after lifecycle dispatcher creation and before service reconciliation; existing `ObservatoryAdapter::daemon_started` is precedent, not the delivery source. | Sequence 1 and first retained boot fact. A late/restarted service can replay it while retained. |
| `session_started` | Both successful producers of `AgentTurnEvent::SessionCreated`: the explicit-create route and the ordinary new-session turn path after admission. | Explicit create publishes only after creation succeeds. In the ordinary path, defer the existing `SessionCreated` publication until the session-operation lease/admission succeeds; rejected/busy admission publishes no session id. Adapt only the fact and session id; strip title/cwd and emit once. |
| `turn_started` | Ordinary turn admission immediately after the session-operation lease succeeds, at the existing `AgentTurnEvent::TurnStarted` emission. | Rejected/busy turns emit nothing. |
| `permission_requested` | `DaemonPermissionPolicy::check` after the waiter is inserted and before the policy waits. | Strip args and free-text reason; retain host ids and tool name. |
| `permission_resolved` | The single terminal branch of `DaemonPermissionPolicy::check`, covering an authorized decision, request cancellation, or closed waiter. The HTTP decision route remains one input, not a second event producer. | Exactly one resolution for each emitted request; strip denial/approval reason text. |
| `tool_started` | Runtime bridge `AgentEvent::ToolExecutionStart`. | Strip args. Do **not** translate the compatibility `PermissionDenied` Started/Finished pair into execution facts because no tool ran. |
| `tool_finished` | Runtime bridge `AgentEvent::ToolExecutionEnd`, correlated privately from the opaque runtime id to the host UUID/name/start instant. | `details.cancelled == true` maps to `cancelled`; otherwise `is_error` maps to `error`, else `success`. Compute duration/rendered output byte count and discard content plus all other details. An unmatched End emits only `unmatched_tool_end`. |
| `turn_finished` | One lifecycle terminal finalizer called by both authorities: (a) normal completion after bridge drain on the `record_prompt_result` path and (b) `terminate_orphaned_turn` for panic/orphan settlement. | A per-request atomic terminal guard makes the competing paths exactly once. Derive from final request state, not the prebuilt SDK status: `Completed → completed`, `Cancelled` (including waiter/request cancellation races) `→ cancelled`, orphan/panic termination `→ abandoned`, all other terminal failures `→ failed`. Existing SDK/client `TurnFinished.status` behavior is not changed by Stage A. |
| `session_stopped` | **None exists on the baseline.** | Never emitted in Stage A. It remains declared for schema compatibility. Adding an explicit daemon session-stop/delete authority later requires a separate contract update and then becomes this event's only source. |
| `daemon_stopping` | Graceful daemon shutdown immediately before supervisor drain begins. | Best effort before `shutdown`; absent after crash/SIGKILL. It is never used to synthesize `session_stopped`. |

### 8.3 Ordering

Within one admitted turn the dispatcher preserves authoritative call-site order:
`session_started? → turn_started → (permission_requested →
permission_resolved)? → tool_started → tool_finished → turn_finished`.
The ordinary new-session producer is moved behind successful admission so a
rejected lease emits neither session nor turn facts; the explicit-create route
is independent and emits only its successful session fact. Multiple tools
repeat the inner sequence; concurrent sessions may interleave. Normal,
cancellation, waiter-cancellation, and panic/orphan terminal paths converge on
the one guarded finalizer; no request publishes two `turn_finished` facts.
`daemon_started` is first in a boot. On graceful shutdown `daemon_stopping` is
the last accepted lifecycle event. No event waits for or can block a turn,
permission decision, session persistence, or daemon shutdown.

### 8.4 Structural payload exclusions

The SDK lifecycle types contain no field for prompts, system text, transcript,
thinking, assistant output, tool args/results/chunks/details, cwd/path/title,
permission reasons, error strings, headers, environment values, secret values,
component/canvas/extension payloads, or arbitrary JSON. The adapter is a
separate exhaustive match over authoritative facts; it must not subscribe a
service directly to `AgentEventBus`, legacy `EventBus`, or Observatory SSE.
Golden fixtures and sentinel-property tests prove forbidden strings cannot
serialize.

## 9. Scope, subscription, replay, and backpressure

### 9.1 Delivery scope

There is one process per effective `(package digest, service id)`, not one per
project. The project registry publishes an in-memory registered-project snapshot
to lifecycle state; project create/delete updates that snapshot and any affected
activation epoch before the project mutation returns. Thin `main.rs` call-site
integration supplies the session's already-resolved project identity from daemon
session/AppState context; lifecycle intersects it with the in-memory snapshot.
Event publication captures the result without blocking filesystem or project-
registry I/O.

Each activation epoch freezes the effective global/project eligibility used for
historical delivery:

- daemon lifecycle facts are eligible when the epoch has at least one effective
  global or registered-project activation;
- a session/turn fact is eligible only when its publication-time project
  classification is effective in that epoch's immutable scope snapshot;
- project-less/unregistered facts require effective global enablement;
- current filter removal happens before disable/project mutation returns;
- any scope change mints a new epoch at the current replay floor, so newly added
  or re-enabled scope cannot replay earlier facts and removed scope cannot use an
  old cursor;
- a project override cannot add trust or widen the manifest subscription.

Live delivery uses the current epoch; replay requires the same epoch and its
publication-time eligibility. The child receives only kinds in both its manifest
declaration and negotiated subscription. It never receives another package's
identity or grant state.

### 9.2 Bounds

- Global boot ring: at most 2,048 event frames and 8 MiB encoded bytes; evict
  oldest until both hold. An individually oversized event is rejected before
  publication and records only a fixed host diagnostic.
- Per-service outbound data queue: at most 256 event frames and 1 MiB encoded
  bytes. Control state is separate and bounded to one coalesced pending value of
  each class (`lag`, `reset`, `ping`, `shutdown`), not a shared single slot.
- Control write priority is `shutdown > reset > lag > ping`. `shutdown` evicts
  every lower-priority pending control; `reset` subsumes pending `lag`; lost
  ranges coalesce; only the newest ping nonce remains. A blocked highest-priority
  write still hits the 2-second connection deadline, after which process-group
  cleanup proceeds without waiting for the child.
- A slow/full service never backpressures the dispatcher or daemon request path.
- ACK validation uses the sole definition in §7.2; no second contiguous-range
  interpretation exists.

When data must be discarded, the host coalesces the lost range and sends:

```json
{"protocol":"ocean.extension.service","version":1,"frame":"lag","first_lost":"10","last_lost":"18","lost_count":9,"replay_available":true}
```

`replay_available` is computed, never hard-coded: it is true only when the lost
range intersects actual retained frames eligible for that activation epoch and
subscription. The child may reconnect and request resume in `service_hello`.
The host honors it only under the complete validity rule in §7.2 and replays
eligible events strictly after the cursor before live attach. Otherwise it sends:

```json
{"protocol":"ocean.extension.service","version":1,"frame":"reset","reason":"boot_changed|activation_changed|retention_exceeded|invalid_cursor","oldest_available":"<u64-or-null>","latest_available":"<u64-or-null>"}
```

`oldest_available`/`latest_available` describe actual retained, epoch-eligible
history, not the global ring. After reset, the host snapshots the live boundary,
clears connection ACK state, and attaches live; the child cannot ACK its old
cursor. Stage A never promises gap-free, exactly-once, or durable delivery.
Services deduplicate by `(daemon_boot_id, activation_epoch, sequence)` if
desired. No transcript or tool payload is retained in the ring.

## 10. Supervisor lifecycle and health

### 10.1 Reconciliation

At startup, after coherent registry load and lifecycle dispatcher creation, the
supervisor computes effective services and starts them asynchronously. An
absent, corrupt, incompatible, untrusted, disabled, unsupported, timed-out, or
unhealthy optional extension never fails daemon startup or ordinary turns.
Registry corruption remains fail-closed for extension activation and visible in
static diagnostics.

Every trust/enable/disable/update/remove transaction produces a new
`state_revision`. Reconciliation is serialized by revision. Stale work may
finish cleanup but cannot publish a newer status. A service is restarted when
its digest, args, granted environment/secret binding, or activation identity
changes.

### 10.2 Runtime states and status

Closed supervisor states:

`inactive | starting | healthy | degraded | backoff | circuit_open | stopping | unhealthy | unsupported_platform`.

Cached status contains package id/digest/version, service id, activation
revision/epoch/replay floor, state, pid when live, started/observed timestamps,
restart count, negotiated subscriptions, last acknowledged sequence, lag count,
and one fixed reason code. It contains no argv values beyond manifest metadata, environment,
secret, stderr text, prompt, or payload. Status is in-memory runtime projection,
not session JSON or immutable store.

`GET /v1/extensions/{id}/status` reads this cache and starts/probes nothing.
List and inspect include the same summary. Doctor may report the cached summary
but remains a read-only, no-execution, non-probing read.

### 10.3 Ping health

After readiness the host sends a `ping` every 10 seconds. The child must return
the matching `pong` within 5 seconds. Three consecutive misses change state to
`unhealthy` and trigger the configured failure policy. A valid pong resets the
miss count. Child `status:degraded` is operator-visible but does not itself
restart the process. EOF, invalid frame, oversize frame, startup timeout,
nonzero exit, or ping failure is a service failure. Exit zero without a host
shutdown is also terminal; it restarts only when `restart = "on-failure"` is
present because unexpected early exit is classified `unexpected_exit`.

### 10.4 Restart/backoff/circuit breaker

For `restart = "on-failure"` the delay sequence is 250 ms, 500 ms, 1 s, 2 s,
4 s, 8 s, 16 s, then 30 s maximum, without jitter. Five failures in a rolling
60-second window open the circuit. A process healthy for 5 continuous minutes
clears failure history and resets backoff. An open circuit never closes on a
timer; disable→enable, a new trusted digest, or daemon restart explicitly
retries it. Without `on-failure`, the first failure becomes `unhealthy` until
one of those operator/configuration transitions.

### 10.5 Shutdown and descendants

On disable, health/circuit failure, reconfigure, or graceful daemon shutdown the
host:

1. stops new event enqueue;
2. sends `shutdown {"reason":"disabled|daemon_stopping|reconfigure|unhealthy"}`;
3. closes stdin after `shutdown_complete` or 2 seconds;
4. sends `SIGTERM` to the owned Unix process group;
5. waits 2 seconds;
6. sends `SIGKILL` to that group when members remain;
7. reaps the direct child only after group termination is proven;
8. removes the connection temp directory and publishes terminal status.

The child is spawned as leader of a new process group on macOS/Linux. The
supervisor keeps generation-safe authority even if the leader exits before a
grandchild: it observes leader exit without reaping it (for example
`waitid(..., WNOWAIT)` or an independently reviewed equivalent), retains the
unreaped leader identity while signaling/waiting for the group, and reaps only
after cleanup. It must never call `killpg` after reaping the leader or otherwise
losing generation-safe ownership, because the PGID may have been reused. A
restart may spawn only after the old group is proven gone and the old leader is
reaped; inability to establish this primitive is `unsupported_platform` and
fails before spawn.

Tests on macOS and Linux cover normal exit, abrupt leader exit with a surviving
grandchild, startup/health/restart/circuit failure, disable, and daemon shutdown,
plus a forced PGID-reuse/unrelated-process fixture proving no unrelated process
is signaled. Packages must not daemonize into another session/process group;
Stage A does not claim containment of a hostile process that deliberately
escapes the group. Windows activation fails before spawn.

## 11. Process context, mutable roots, and secrets

### 11.1 Executable and cwd

The executable is the descriptor-anchored canonical service entry in the exact
trusted immutable artifact. It is invoked directly; no shell and no PATH lookup.
Manifest args are passed exactly after validation. The cwd is the immutable
package root. No manifest path, argument, or environment value may replace it.
Secrets never enter argv.

### 11.2 Mutable roots

All package mutable files live under the new daemon-owned subtree:

```text
<config_dir>/extensions/state/<package-id>/
├── data/
├── cache/
└── tmp/<service-id>/<connection-id>/
```

`data/` survives disable, restart, and remove by default. `cache/` survives
disable/restart but may be purged on explicit removal. A connection temp root is
new per launch and deleted after reap. Directories are created mode 0700 on
Unix, opened without symlink traversal, and verified beneath the descriptor-
anchored `state/` root. Package ids are validated components. Mutable data never
enters `store/`, session JSON, repository `.ocean/`, or `projects.json`.

Remove preserves `data/` unless `purge_state=true`; purge is allowed only after
all package services are stopped. The host reports retained-state paths only to
local operator inspection. The native process receives canonical assigned paths
but writes are not brokered; this is path assignment, not a kernel quota or
sandbox. Its required acknowledgement explicitly covers its ability, as the
daemon user, to inspect or alter files outside these roots and to tamper with
registry/store bytes; digest checks detect compliant drift but do not contain a
malicious native child.

### 11.3 Minimal environment

The host calls `env_clear`. On supported Unix platforms it sets only:

- `PATH=/usr/bin:/bin` (process-resolution baseline; entry itself is absolute);
- `PWD=<canonical immutable package root>`;
- `HOME=<canonical package data dir>`;
- `XDG_STATE_HOME=<data>`;
- `XDG_CACHE_HOME=<cache>`;
- `TMPDIR=<connection temp dir>`;
- explicitly granted environment bindings and secret bindings.

No daemon/provider/auth/Tailscale/Git/SSH/cloud environment is inherited. The
host reserves `PATH`, `PWD`, `HOME`, `XDG_STATE_HOME`, `XDG_CACHE_HOME`,
`TMPDIR`, and every `OCEAN_EXTENSION_*` name; a package cannot request or bind
them.

A granted ordinary env name copies only that exact same-named value captured by
the daemon; absent values fail activation with `environment_missing`. All
injected values, including ordinary env values, are treated as sensitive by the
redactor.

### 11.4 Explicit secret-reference binding

The manifest separately requests secret references and environment names. The
operator trust grant supplies the only association:

```json
{"target_env":"SLACK_APP_TOKEN","reference":"env:OCEAN_SLACK_APP_TOKEN"}
```

V1 rules:

- `reference` must exactly match a manifest-requested secret;
- `target_env` must exactly match a manifest-requested env name and must not be
  reserved;
- each target and reference appears at most once;
- a bound target is populated only from its secret reference and is excluded
  from ordinary same-name environment copying;
- V1 supports only scheme `env`; its key must match
  `[A-Z_][A-Z0-9_]{0,127}`;
- resolution occurs immediately before spawn from the daemon's exact named
  value; absence fails activation;
- state/journal/status store only reference and target names, never values;
- the value exists only in the spawn environment/redaction set and is zeroized
  where the chosen Rust value type permits;
- values never enter manifest, state, digest, argv, event frames, HTTP bodies or
  responses, debug formatting, errors, metrics, or logs.

Install and inspect never resolve a secret. Doctor verifies binding syntax and
availability by name only; it does not read the value. Binding changes require a
new grant-diff confirmation and service restart.

### 11.5 Stderr diagnostics

Stderr is piped separately and never interpreted as protocol. The host reads at
most 8 KiB per line, 20 lines/second with burst 40, and 64 KiB total sanitized
text per service in memory. Longer/extra data is discarded with counters. Before
retention, control characters are removed and every injected env/secret value
is replaced with `<redacted>` using exact byte matching. Raw bytes are never
logged or persisted. Normal logs contain only package/service identity,
byte/line counts, truncation/redaction counts, and fixed reason codes. Inspect
may expose the bounded sanitized ring only with an explicit future operator
option; Stage A status exposes counters, not text.

## 12. Registry state and transactions

### 12.1 Layout

Accepted A0's three strict schema-v1 files remain byte-shape compatible and gain
one additive companion:

```text
<config_dir>/extensions/
├── installs.json
├── trust.json
├── enabled.json
├── service-grants.json                   # exact schema below; no values
├── store/<extension-id>/<digest>/        # immutable verified payload
├── state/<extension-id>/...              # mutable, §11.2
├── quarantine/<operation-id>/            # acquired/verified without state lock; never executable
├── staging/<transaction-id>/             # adopted quarantine + next state; never executable
├── transactions/<transaction-id>.json   # no secret values
└── .state.lock
```

The exact persisted file is:

```json
{
  "schema_version": 1,
  "state_revision": 8,
  "service_grants": [
    {
      "id": "example.noop",
      "digest": "sha256:<64-lowercase-hex>",
      "service_id": "lifecycle",
      "native_process_ack": true,
      "secret_bindings": [
        {"target_env":"SLACK_APP_TOKEN","reference":"env:OCEAN_SLACK_APP_TOKEN"}
      ]
    }
  ]
}
```

Every object denies unknown fields. `schema_version` is exactly `1` and
`state_revision` is the same nonzero revision as the other three files. The
array is capped at 1,024 rows and is in strict ascending bytewise
`(id,digest,service_id)` order; duplicate tuples are rejected. Each tuple is the
service-grant identity and binds to exactly one accepted `trust.json` grant with
the same `(id,digest)` and to that service in the descriptor-anchored artifact.
Ids, digest, service existence, capability subsets, and the accepted A0 string
limits are revalidated on every read. `native_process_ack` must be `true`; revoke
by removing the row, never by persisting `false`.

Each `secret_bindings` array is capped at 256 and sorted by
`(target_env,reference)`. Duplicate targets, duplicate references, non-requested
names, reserved names, unsupported references, or a binding not covered by the
matching exact-digest trust capability set fail closed. Only target/reference
names are stored; values and availability results are forbidden. A native
service is activatable only with its exact row, acknowledgement, and complete
bindings. Non-service trust needs no row.

The CLI/TUI never write these paths. Coherent reads hold a shared lock across
state and artifact inspection. Publication/recovery holds the exclusive lock;
network/local acquisition does not. On an accepted A0 snapshot, absence of
`service-grants.json` means exactly the empty array at the coherent three-file
revision and activates no service. A2a reads that form without writing it. The
first successful Stage A mutation publishes all four complete files at the next
revision, even when the array is empty; after that first publication, absence,
revision mismatch, malformed content, noncanonical order, or duplicate content
fails closed. The merged Phase 1 binary ignores the extra file and continues to
parse its unchanged three schemas. Existing A0 limits remain: each
state/manifest file 1 MiB, 10,000 package entries, depth 64, 256 MiB final
artifact, and 250 ms lock wait.

### 12.2 Transition model

The transitions are separate and never implied:

1. **install/update** publishes immutable bytes and one installed record; it
   grants no trust and starts nothing;
2. **trust** records an exact-digest capability grant in `trust.json` plus the
   explicit native-process acknowledgement and secret bindings in
   `service-grants.json`; it does not enable;
3. **enable** records global or registered-project enablement and is rejected
   unless the currently installed digest has a sufficient trust grant;
4. effective service activation requires all three plus compatibility and a
   supported platform.

A changed digest never reuses trust. Project enablement cannot create trust.
Disable is always allowed and commits only after its effective filter is removed.
It returns HTTP 200 only after any now-unneeded service is reaped; bounded cleanup
failure uses §15's committed 202. Update/remove require the package fully disabled
and actually stopped.

### 12.3 Journaled publication and recovery

Install/update uses two distinct phases; other mutations begin at step 2:

1. **Without `.state.lock`**, reserve one of four acquisition permits, create a
   same-filesystem descriptor-anchored `quarantine/<operation-id>`, acquire local
   or Git bytes, enforce all limits, validate the manifest/id/source, and hash
   the complete non-executable artifact. Failure deletes quarantine and cannot
   affect registry reads or revision state.
2. Acquire `.state.lock` exclusively, recover any prior journal, then recheck
   `expected_state_revision`, package active/stopped state, current install/trust/
   enablement, and every operation precondition. A race loses here with
   `state_revision_conflict`; acquisition is never silently replayed.
3. For install/update, atomically adopt quarantine as transaction staging while
   retaining its anchored handles and revalidate its recorded identity/hashes;
   perform no DNS, network, source-tree read, or Git process under the lock.
   Construct and hash all four complete next-revision state files.
4. Write/fsync a `prepared` journal containing operation id/type, old/new
   revision, non-secret source/grant metadata, staged names/hashes, and intended
   immutable-store destination; atomically rename and fsync it.
5. Atomically publish the immutable artifact if applicable.
6. Rename `installs.json`, `trust.json`, `enabled.json`, and
   `service-grants.json` to the complete staged files while the exclusive lock
   prevents a reader from observing the intermediate set.
7. Fsync each file and the extensions directory, mark the journal `committed`,
   fsync, then remove staging/journal and fsync their parent directories.
8. Release the lock and reconcile the supervisor to the committed revision.

All four documents carry the same nonzero revision after the first Stage A
mutation; the A0-only absence exception is only §12.1's read upgrade. Recovery
validates journal and staged hashes. If no state file was replaced, it removes
staging and an unreferenced just-published artifact. If any state file reached
the new revision, it rolls forward all remaining verified new files; it never
guesses a rollback from a mixed generation. A corrupt/missing required staged
file fails closed with `registry_recovery_required` and starts no extension.
Orphan quarantine/staging/store payloads are non-executable and may be removed
only after proving no install or journal references them.

Any failure through step 7 is pre-commit and leaves the old coherent revision
effective. Reconciliation happens after durable commit and therefore cannot
make that claim; its exact committed response is §15. Acquisition concurrency
is four daemon-wide, while registry publication remains one writer. Tests hold a
60-second fake acquisition open while list/inspect/doctor continue coherent
shared-lock reads and three other acquisitions proceed.

### 12.4 Exact install/update/remove/reinstall retention

| State/artifact | Install | Update while disabled/stopped | Remove (`purge_state=false`) | Remove (`purge_state=true`) | Reinstall identical digest |
| --- | --- | --- | --- | --- | --- |
| current install + source provenance | create one current row | replace with new source/digest row; no separate durable source/audit log exists in v1 | delete | delete | create a new row from the supplied source |
| trust rows for package id | none | delete all; replacement is untrusted | delete all current/historical rows | same | none; operator must trust again |
| service-grant rows | none | delete all with package id | delete all | delete all | none; acknowledgement/bindings must be confirmed again |
| enablement rows | none | retain scopes but they are ineffective until separately retrusted | delete all | delete all | none |
| immutable payloads | publish current digest | publish new and retain prior payloads only as unreferenced rollback candidates | delete every unreferenced payload for the id after journal/reference proof | same | publish/deduplicate verified bytes, never trust them |
| `data/` | create on first activation, not install | retain | retain | delete after reap | reuse only when retained |
| `cache/` | create on first activation | retain | delete | delete | recreated empty |
| `tmp/` | none until activation | every connection cleans after reap | require empty/delete | require empty/delete | new per connection |
| transaction/source audit | journal only until completed recovery | journal only; removed after commit | journal only; removed after commit | same | no prior trust or audit confers authority |

Thus remove always revokes enablement, trust, acknowledgement, and secret
bindings. Reinstalling byte-identical content is installed-but-untrusted. Update
also clears all trust/service grants so explicit rollback to an old retained
payload still requires a fresh grant preview/confirmation. Fixtures cover both
remove modes, update→rollback, remove→identical reinstall, and crash recovery at
each table transition.

## 13. Install sources

### 13.1 Local path

HTTP accepts an absolute UTF-8 directory path. The CLI canonicalizes a relative
operand against its own cwd before sending it. The daemon opens the directory
descriptor-relative, rejects symlinks, hardlinked regular files (`nlink != 1`),
FIFOs, sockets, devices, sparse/oversize files, escapes, depth/entry/byte limit
violations, and a missing/invalid manifest. It copies verified regular-file
bytes into quarantine and hashes the quarantined tree before lock acquisition;
§12.3 adopts and revalidates it for publication. Local install performs no
network access and works offline.

### 13.2 Pinned public Git v1

Accepted grammar deliberately matches accepted A0 `InstallSource` exactly:

```text
source.kind = "git"
url         = "https://<public-host>/<nonempty-path>[.git]"
revision    = "<exact 40- or 64-character lowercase hex object id>"
```

V1 installs the repository root only. Git `subdir`, query/fragment encoding, and
any extra persisted source field are excluded so every A3–A4 generation remains
readable by accepted A0's strict `{kind,locator,revision}` source schema.

Rules:

- HTTPS only; no SSH/scp/file/git schemes, userinfo, password, query, fragment,
  IP literal, non-443 explicit port, control character, or local/loopback/private
  host;
- revision is an object id, never branch/tag/HEAD/short SHA; fetched commit id
  must equal it exactly and must be a commit;
- 60-second total acquisition deadline, 512 MiB Git object/temp ceiling, 256 MiB
  extracted package ceiling, 10,000 entries, depth 64, and four concurrent
  acquisitions daemon-wide;
- redirects disabled; no submodules, Git LFS, worktree checkout, smudge/clean
  filter, hooks, package scripts, or build scripts.

The host resolves the URL hostname once per acquisition through an injectable
resolver, canonicalizes/deduplicates the complete address set, and rejects the
entire acquisition if any answer is loopback, link-local, private, multicast,
unspecified, documentation-only, or otherwise non-public. The hostname URL
remains the TLS URL so certificate and SNI verification are unchanged. For each
connection attempt, the host launches Git with exactly one already-checked
address pinned by
`-c http.curloptResolve=+<host>:443:<address>` (Git's
`CURLOPT_RESOLVE` binding); it never gives Git an unpinned hostname attempt.
Fallback tries another member of the originally checked set under the same total
deadline and a fresh pinned process. DNS is not consulted by Git/libcurl for the
pinned host, redirects are disabled, and a resolved/redirected second host is
never accepted.

Pinned Git requires host Git **2.37.0 or newer**, the first release carrying
`http.curloptResolve`, and the normal libcurl HTTPS transport. Before acquisition
the daemon resolves the executable without package influence, parses
`git --version`, and rejects older/unparseable versions. Unsupported platform,
missing Git, missing HTTPS/libcurl transport, an unrecognized
`http.curloptResolve`, or any indication that the pin was not honored fails
closed as `git_connection_pinning_unavailable` with fixed safe text; there is no
unpinned fallback. Conformance uses an injectable DNS/HTTP harness to prove the
socket reaches only the selected checked address while hostname/SNI/certificate
checks remain enabled.

Git runs only as an acquisition tool in a generation-safe new process group with
an empty environment plus fixed `PATH`, `HOME` pointing to an empty mode-0700
temp directory, `GIT_CONFIG_NOSYSTEM=1`, `GIT_CONFIG_GLOBAL=/dev/null`,
`GIT_TERMINAL_PROMPT=0`, and `GIT_ASKPASS=/usr/bin/false`; all proxy/auth/SSH
variables are absent. Every command sets `http.proxy=`,
`remote.origin.proxy=`, empty credential helper, disabled hooks/LFS/filters,
`http.followRedirects=false`, no optional locks, and no tags. It initializes an
empty quarantine repository, fetches only the exact object with depth 1,
verifies `FETCH_HEAD`, and extracts the root commit tree through an archive/
read-tree path that never checks out or executes content. Timeout or size excess
uses §10.5 cleanup and deletes quarantine. Exact argv contains no secret,
credential, SSH command, or inherited proxy/auth setting. A source requiring
credentials, proxy, LFS, submodule, redirect, named ref, or subdirectory is
unsupported in v1.

### 13.3 Update

Update requires the package disabled/stopped and an explicit new local or Git
source. There is no `latest`. It quarantines and validates the replacement,
requires the same extension id, and atomically changes the install record. Per
§12.4, all package trust/service-grant rows are removed, so the new digest and
any later rollback are untrusted until a separate trust transition. Enablement
rows may remain but are ineffective. Prior immutable payloads are retained only
as unreferenced rollback candidates until proven safe for orphan cleanup; v1 has
no separate durable source/audit ledger.

## 14. Grant diff confirmation

A trust request supplies the exact installed digest, a subset of manifest
capabilities, and canonical per-service rows shaped as
`{"service_id":"…","native_process_ack":true,"secret_bindings":[…]}`. The
daemon fills persisted id/digest and canonicalizes the exact §12.1 order. Every
native service in the requested grant needs its own acknowledgement; bindings
cannot float between services. The preview returns:

```json
{
  "added": {
    "capabilities":{"network":[],"filesystem":[],"env":["SLACK_APP_TOKEN"],"secrets":["env:OCEAN_SLACK_APP_TOKEN"]},
    "service_grants":[{"service_id":"lifecycle","native_process_ack":true,"secret_bindings":[{"target_env":"SLACK_APP_TOKEN","reference":"env:OCEAN_SLACK_APP_TOKEN"}]}]
  },
  "removed": {"capabilities":{"network":[],"filesystem":[],"env":[],"secrets":[]},"service_grants":[]},
  "native_authority_notice": "Stage A native activation grants daemon-user-equivalent authority. The process may attempt network access, read or modify any daemon-user-accessible filesystem data including registry/store bytes, and act outside assigned roots regardless of declared capabilities; declarations and assigned paths are not containment.",
  "confirmation": "sha256:<hash-of-id,digest,current-revision,canonical-diff-and-notice>"
}
```

The apply request must repeat the exact service rows, set every shown
`native_process_ack` to true, and provide the matching confirmation; the notice
text is part of the hash so acknowledgement cannot be detached from the diff.
Without `confirm_grant_diff`, the request is preview-only and mutates nothing.
Apply requires the same digest and state revision used in the hash; races return
`state_revision_conflict` and require a new preview. No `--yes`, wildcard,
`all`, or manifest-self-grant exists. Network/filesystem nonempty grants are
rejected as declaration policy under R4 rather than represented as containment.
Narrowing and revocation also require confirmation because they mint a new
activation epoch and may restart/stop a service.

## 15. Exact HTTP and CLI contract

All JSON request types deny unknown fields. Mutations include
`expected_state_revision`; the CLI obtains it from list/inspect and never
silently retries a conflict. Pre-commit errors use
`{"ok":false,"mutation":{"operation_id":"<uuid>","committed":false,"state_revision":N},"error":{"code":"<closed-code>","message":"<fixed-safe-text>"}}`
and never include secret values or raw Git/stderr output. Reads retain their
accepted Phase 1 response/error shapes.

| Operation | Daemon HTTP | `ocean-rs` CLI |
| --- | --- | --- |
| list | `GET /v1/extensions?project_id=<uuid?>` | `ocean-rs extension list [--project-id UUID]` |
| inspect | `GET /v1/extensions/{id}/inspect?project_id=<uuid?>` | `ocean-rs extension inspect ID [--project-id UUID]` |
| doctor | `GET /v1/extensions/{id}/doctor?project_id=<uuid?>` | `ocean-rs extension doctor ID [--project-id UUID]` |
| runtime status | `GET /v1/extensions/{id}/status` | `ocean-rs extension status ID` |
| local install | `POST /v1/extensions/install` with `{"expected_state_revision":N,"source":{"kind":"local-path","path":"/absolute/path"}}` | `ocean-rs extension install --path PATH` |
| Git install | same route with `{"source":{"kind":"git","url":"https://…","revision":"<hex>"}}` | `ocean-rs extension install --git URL --rev HEX` |
| trust preview/apply | `POST /v1/extensions/{id}/trust` with `{"expected_state_revision":N,"digest":"sha256:…","capabilities":{…},"service_grants":[{"service_id":"…","native_process_ack":true,"secret_bindings":[…]}],"confirm_grant_diff":null|"sha256:…"}` | `ocean-rs extension trust ID --digest DIGEST [--grant-env NAME] [--grant-secret REF] [--ack-native-process SERVICE] [--bind-secret SERVICE:TARGET=REF] [--confirm-grant-diff HASH]` |
| enable | `POST /v1/extensions/{id}/enable` with `{"expected_state_revision":N,"scope":{"kind":"global"}}` or `{"scope":{"kind":"project","project_id":"uuid"}}` | `ocean-rs extension enable ID [--project-id UUID]` |
| disable | `POST /v1/extensions/{id}/disable` with the same scope shape | `ocean-rs extension disable ID [--project-id UUID]` |
| remove | `DELETE /v1/extensions/{id}` with JSON `{"expected_state_revision":N,"purge_state":false}` | `ocean-rs extension remove ID [--purge-state]` |
| local update | `POST /v1/extensions/{id}/update` with expected revision plus local source | `ocean-rs extension update ID --path PATH` |
| Git update | same route plus Git source | `ocean-rs extension update ID --git URL --rev HEX` |

`--path` and `--git` are mutually exclusive; `--rev` requires `--git`. V1 has no
`--subdir` option or HTTP field.
URL path ids are percent-encoded by the CLI and revalidated by the daemon.
Install rejects an already installed id with `already_installed`; update rejects
an absent id. Enable rejects missing trust, unresolved bindings, declared
network/filesystem capabilities, incompatible host, or unsupported platform.
Remove/update return `extension_active` until every scope is disabled and the
service is reaped. Remove never deletes another digest/package state.

Every applied mutation returns the same post-commit envelope:

```json
{"ok":true,"mutation":{"operation_id":"<uuid>","committed":true,"state_revision":9,"id":"example.noop","digest":"sha256:…","effective":false,"reconciliation":"complete|pending|blocked","reap":"not_required|complete|pending"}}
```

HTTP `200` means reconciliation required by the operation is complete. HTTP
`202` means registry commit succeeded but startup reconciliation or reap remains
`pending|blocked`; it is never encoded as a pre-commit error and the committed
revision is immediately authoritative. Startup failure after enable is runtime
status (`unhealthy|circuit_open`), not mutation rollback. Disable commits filter
removal first and returns `200` only after required reap; a bounded reap failure
returns `202`, leaves update/remove guarded by `extension_active`, and remains
operator-visible until cleanup. Remove itself requires already-reaped state.
The CLI always prints `committed`, operation id, revision, reconciliation, and
reap; it exits 0 for `200`, dedicated exit 3 for committed `202`, and never
retries either response. The operator uses status/inspect by returned revision.
Trust preview is `200` with `applied:false,committed:false`; confirmed apply uses
the common envelope.

## 16. No-code-execution boundary

The following operations must not spawn a package entry, plugin, hook, build
script, health probe, shell, language package manager, or provider call:

- list, inspect, doctor, status;
- local/Git install acquisition and validation;
- trust preview/apply;
- update staging/publication;
- disabled discovery and registry recovery.

Git acquisition may execute the assigned, stripped-environment host `git` command described in §13.2,
but never package content. The first possible package execution is supervisor
reconciliation after a separately committed trust grant and enablement. Tests
use executable canaries in every resource path and prove their markers remain
absent through all operations above.

## 17. Active-service safety, rollback, and recovery

- Disable removes delivery eligibility and mints the new activation epoch before
  shutdown begins. HTTP 200 is returned only after reap; a bounded cleanup
  failure uses the explicit committed-202 contract in §15. A project disable
  that leaves another effective scope does not stop the shared service but
  immediately removes that project's events and resets its replay epoch.
- Update/remove never operate on an active service. They do not detach or leave
  an invisible orphan.
- Failed service startup does not roll back install/trust/enable state; status is
  unhealthy/circuit-open and ordinary Ocean remains available. Operator
  rollback is disable, inspect/doctor, then exact pinned update/remove.
- Failed registry mutation preserves the old coherent revision through §12.3.
- A successful update can be rolled back only by an explicit update to the old
  local tree or exact Git revision, followed by separate trust and enable. There
  is no implicit `latest` or auto-trust rollback.
- A1 is additive DTO/adapter code and can be reverted without state migration.
  A2a–A2b can be disabled operationally by disabling every service; legacy
  plugins and hooks remain untouched. A3a–A4 preserve accepted A0's three strict
  schemas and source shape; no slice may make the merged Phase 1 binary unable
  to inspect a committed registry generation.
- If A2a–A4 cannot preserve that downgrade/read compatibility, stop and ratify a
  state schema migration before merge.

## 18. Ordered PR slices

### A0 — accepted Phase 1 prerequisite (already complete)

PR #354 is merged at `77630bda`; its implementation commit is `399713b9`. Before
A1, verify the branch descends from that merge and rerun the accepted narrow
Phase 1 gate if upstream changes touch its files. A0 authorizes no new code.

### A1 — protocol and pure lifecycle adapter

Land the ratified version of this manifest; add SDK v1 DTOs/golden fixtures,
closed metadata types, exhaustive pure adapter, boot ring, scoping/order tests,
and nine produced mappings plus the reserved `session_stopped` schema/non-
emission fixture. Include normal/cancelled/orphan exactly-once finalizer tests.
Do not spawn a service or add mutations.

### A2a — read-only registry upgrade and minimum supervised transport

Refactor accepted `extension_state.rs` into the sole read-only
`extension_registry.rs` authority; implement the exact A0-absence/four-file
`service-grants.json` reader and hand-authored fixtures. Add only the no-op
process spawn, strict hello/ready transport, assigned roots/minimal environment,
secret consumption, generation-safe process-group ownership, bounded shutdown,
and read-only/non-probing runtime status cache. Startup reconciliation consumes
that one reader. No live lifecycle call-site wiring, replay delivery, health
restart policy, or registry mutation. Its schema, acknowledgement, secret, and
leader-exit cleanup security tests land here.

### A2b — lifecycle, replay, health, and project-scope integration

Add prioritized/coalesced controls, activation-epoch replay, event queues,
health/ping, restart/backoff/circuit breaker, bounded stderr, and startup/shutdown
reconciliation over A2a. Wire every authoritative lifecycle call site, guarded
terminal finalizer, host-UUID tool correlation, and synchronous project-registry
snapshot hook. No registry mutations. Its replay/scope widening, failure,
redaction, ordering, cancellation, and live process cleanup tests land here.

### A3a — transactional local registry engine and recovery

Add the internal mutation engine for local quarantine/install/trust/enable/
disable/remove/update, strict four-file journal/publication/recovery, exact
retention transitions, grant preview/apply, expected-revision concurrency, and
crash fixtures. No HTTP/CLI mutation routes, live supervisor reconciliation, or
Git network acquisition. Tests invoke the internal authority and include
acquisition outside the state lock.

### A3b — HTTP/CLI mutation surfaces and supervisor reconciliation

Add only §15's HTTP/CLI mutation surfaces, common committed response envelope,
and revision-serialized supervisor reconciliation/reap behavior over A3a.
Status exposure is read-only/non-probing cached runtime projection, never a
package probe. Its committed-202, CLI exit, active-service, and retry/reinspect
tests land here. No Git network acquisition.

### A4 — pinned public Git acquisition

Add only §13.2 Git source handling to install/update, with URL/DNS-to-connection
pinning, Git capability/version fail-closed behavior, credential/proxy isolation,
generation-safe process groups, timeout/byte/revision/root-tree/no-submodule/LFS/
filter/script constraints, and rollback tests.

### A5 — integrated Stage A gate and closeout

Add/run the E2E matrix in §19 against local and pinned Git no-op packages,
complete independent security/correctness/architecture review, full CI/MSRV/
compatibility, operator acceptance record, and docs/devlog closeout. No Stage B
code, reference integration, deployment, or extension repository creation.

The strict order is A0 evidence → A1 → A2a → A2b → A3a → A3b → A4 → A5.
Each named sub-slice is a separate commit/PR and receives fresh independent
review with its owning security tests before the next begins; A5 does not defer
those tests. A later slice may not be smuggled into an earlier review boundary.

## 19. Acceptance matrix and precise test gates

### 19.1 Protocol and lifecycle

- Golden encode/decode and unknown-field/version/frame rejection at 65,536/
  65,537-byte boundaries.
- Handshake identity cannot be overridden; subscription is an exact subset;
  missing/late/duplicate hello fails.
- Nine produced event kinds have source-table tests; all ten schema variants
  have fixtures, and reserved `session_stopped` has a non-emission fixture.
- Both explicit-create and ordinary new-session producers are covered; ordinary
  `SessionCreated` follows successful admission, resumed sessions omit it, and
  rejected admission emits neither turn nor session facts.
- Permission request resolves exactly once for allow, allow-session, deny,
  cancellation, and waiter closure; args/reasons absent.
- Runtime permission denial does not fabricate tool execution in extension
  lifecycle.
- Tool cancellation derives from `details.cancelled`; unmatched End emits only a
  fixed diagnostic. Normal, request/waiter cancellation race, failure, and
  panic/orphan paths emit exactly one source-backed turn outcome; raw data is
  absent and existing SDK client status is unchanged.
- `session_stopped` is never inferred or emitted in Stage A.
- Concurrent-session ordering is per authoritative sequence and project scope;
  one project/package cannot observe a disabled scope.
- Sentinel prompts, paths, args, results, errors, headers, env, secrets, canvas,
  and arbitrary extension payloads cannot serialize or appear on the wire.
- Observatory store/cursors/tokens/routes are unchanged and observer delivery
  cannot mutate, cancel, or publish.

### 19.2 Queue/replay/failure isolation

- Boot ring count/byte eviction and per-service queue count/byte limits.
- Slow reader produces coalesced `lag`; shutdown/reset/lag/ping priority remains
  bounded and never delays a turn/permission/session.
- Retained resume requires matching activation epoch and actual eligible history;
  wrong boot/epoch/old cursor resets explicitly. Disable→events→re-enable and
  project-scope widening cannot replay ineligible interval events.
- Abrupt service exit, invalid/oversize stdout, blocked stdin, stderr flood,
  startup timeout, ping timeout, crash loop, and open circuit are fail-soft.
- Backoff sequence, rolling-window threshold, stable reset, and explicit retry
  transitions use paused Tokio time where possible.
- Daemon crash produces no false `daemon_stopping`; restart uses a new boot id.

### 19.3 Process/security

- `env_clear` baseline is exact; forbidden ambient variables are absent.
- Cwd/executable/digest bind to the same descriptor-anchored artifact.
- Ordinary and secret bindings inject only confirmed requested names; missing,
  duplicate, unsupported, reserved, or stale bindings fail before spawn.
- Secret sentinel absent from state, journal, argv, HTTP, status, diagnostics,
  events, logs, panic/debug strings, and crash output.
- Stderr cap/rate/redaction counters hold under newline-free and binary input.
- Child and grandchild are gone after abrupt leader exit, restart/circuit
  failure, disable, health failure, daemon shutdown, and timed-out Git
  acquisition on macOS/Linux; PGID-reuse fixtures prove an unrelated process is
  never signaled.
- Windows status is `unsupported_platform` and no child starts.
- State/cache/temp roots reject symlink/path replacement; temp is cleaned;
  data persists or purges exactly as requested.

### 19.4 Registry/package management

- Entire accepted A0 suite remains green: absent/coherent state, equal revision,
  lock contention, descriptor/no-follow traversal, digest replacement, symlink,
  hardlink, FIFO/special file, depth/count/byte/manifest limits, registered
  project, grant subset, and no-execution.
- Strict `service-grants.json` unknown-field/order/duplicate/bounds/binding/
  revision fixtures pass, including exact A0 absence upgrade and post-upgrade
  missing-file failure.
- Every mutation checks expected revision and exclusive lock; readers never see
  mixed generations. Four concurrent quarantined acquisitions do not hold the
  registry lock, and list/inspect/doctor remain available during a 60-second
  fake fetch.
- Crash injection before journal, after journal, after store publish, after each
  state rename, and after directory fsync proves rollback/roll-forward rules.
- Install ≠ trust ≠ enable; changed digest loses effective trust; project enable
  cannot widen global trust.
- Grant preview mutates nothing; stale/wrong confirmation fails; exact
  confirmation applies; widening/narrowing/binding/native ack diffs are stable.
- Enable rejects untrusted/incompatible/unsupported/network-or-filesystem
  service packages.
- Pre-commit versus committed-unreconciled HTTP/CLI responses are unambiguous;
  committed retries/reinspection use the returned revision.
- Active update/remove refuses; disable removes scope and reaps; every cell of
  §12.4 passes update/rollback/remove/reinstall retention fixtures.
- Local install is offline. Every inspect/install/trust/update code-execution
  canary remains untouched.
- Git rejects branch/tag/short/uppercase revision, redirect, credentials,
  userinfo, any non-public DNS answer, SSH, helper/proxy/config inheritance,
  submodule, LFS/filter, special file, timeout, object/extract limits, commit
  mismatch, and any subdir field. Git <2.37/missing pin capability fails closed;
  an injected connection test proves only the checked pinned address is used,
  and a separately recorded exact public-commit smoke succeeds.

### 19.5 Required commands by slice

Narrow gates as applicable:

```text
cargo test -p ocean-extension
cargo clippy -p ocean-extension --all-targets -- -D warnings
cargo test -p ocean-agent-sdk
cargo test -p ocean-hooks
cargo test -p ocean-plugin                 # only if a helper changes
cargo test -p ocean-agent                  # authoritative event composition fanout
cargo test -p ocean-cli
cargo test -p ocean-daemon extension_
cargo test -p ocean-daemon permission_
cargo test -p ocean-daemon
cargo check --workspace --tests
cargo fmt --all -- --check
cargo xtask docs-check
cargo xtask ci --compatibility
cargo +1.88.0 xtask ci --msrv
cargo xtask ci
git diff --check
```

Every feature/logic/security/protocol slice requires fresh independent review.
A5 additionally records exact base/head SHAs and proves the tree clean.

## 20. Stage A E2E no-op-service gate

The fixture is a minimal Rust service with no network/filesystem capability and
no integration-specific code. It handshakes, persists its last acknowledged
boot/sequence under assigned `data/`, answers ping, records only event-kind and
identifier counts, can spawn a cooperative grandchild for kill tests, and emits
secret-sentinel stderr only in a dedicated redaction case.

The gate, in order:

1. local install while offline; prove no fixture marker/process exists;
2. inspect/doctor/list/status; prove no execution;
3. trust preview shows the complete daemon-user-equivalent authority notice;
   exact per-service confirmation publishes strict grants but proves not enabled
   and not running;
4. enable; prove host-injected identity, assigned roots, minimal env, readiness,
   and one live process group;
5. run new/resumed ordinary sessions with permission and tools; prove exact
   scoped metadata events, forbidden-payload absence, and ordinary client/SSE
   compatibility;
6. force lag then retained activation-eligible replay and reset; ordinary turns
   stay responsive;
7. disable, emit events, re-enable with a stale cursor, and separately widen a
   project scope; prove epoch reset/floor prevents both interval disclosures;
8. crash repeatedly; prove numeric backoff/circuit-open and generation-safe old-
   group cleanup before retry;
9. restart daemon; prove new boot id, preserved package data, paused/non-durable
   replay posture, single reconciled service, and no orphan descendant;
10. disable one project scope and global scope; prove filter/epoch update,
    shutdown, child/grandchild reap, and data retention;
11. active remove/update refusal; disabled exact-revision update installs an
    untrusted digest and does not start until separate trust+enable;
12. remove preserving state, identical-digest reinstall/retrust, then disable/
    remove with purge; verify every §12.4 retention cell;
13. repeat install/update from one exact public HTTPS Git root commit with a
    pinned checked address; reject adjacent branch/tag, subdir, unpinned Git,
    and credential-requiring sources;
14. rerun ordinary sessions with the extension absent and confirm no observable
    behavior change;
15. complete full gates and independent security/correctness/architecture
    review; operator separately accepts Stage A.

Passing A5 does not authorize Stage B until its own implementation manifest is
ratified, as required by the accepted Crew parent.

## 21. Stop conditions

Stop and request a design amendment if implementation would:

- start from a base that does not descend from accepted Phase 1 merge `77630bda`;
- invent a `session_stopped` source or alter session lifetime semantics;
- hand a bearer/auth token to the child or expose an unauthenticated service
  HTTP route;
- persist lifecycle events or reuse Observatory storage/auth/control;
- expose prompt/transcript/tool/permission/error/path/secret payloads;
- block a daemon turn, permission, persistence, startup, or shutdown on a
  service queue;
- inherit ambient environment, put a secret in state/argv/logs, or resolve an
  unbound secret reference;
- represent native network/filesystem declarations as enforced grants;
- claim process-tree cleanup on an unsupported platform or require blind
  process killing;
- let CLI/TUI write registry files, publish a mixed revision, or recover without
  journal proof;
- execute package code during acquisition, install, inspect, doctor, trust, or
  update;
- update/remove an active service, auto-trust a changed digest, accept an
  unpinned/private/credentialed Git source, or enable a project to grant trust;
- add Telegram/provider/integration special cases, UI artifacts, Stage B–D
  seams, or core orchestration vocabulary;
- require a public wire/session/tool compatibility change outside this
  versioned channel.

## 22. Acceptance criteria

The operator may ratify this implementation manifest when an independent review
confirms:

- every requested design gap has one exact contract or an explicit recommended
  ratification choice;
- nine produced kinds map honestly to current authority and the tenth,
  `session_stopped`, remains a reserved schema-only kind that is not fabricated;
- stdio identity/auth, handshake, framing, queue, replay, health, restart,
  process, environment, secret, and diagnostic behavior is bounded;
- state/store/session ownership is unambiguous;
- install, trust, and enable are distinct daemon-owned transitions;
- HTTP/CLI, Git grammar/limits, journal recovery, grant diff, active-service
  safety, rollback, and no-execution rules are exact;
- the A0 prerequisite and A1/A2a/A2b/A3a/A3b/A4/A5 boundaries, stop conditions,
  validation, and E2E gate prevent premature Crew/UI/integration work;
- Observer and Observatory remain read-only and Ocean core remains free of Crew
  vocabulary.

Until ratification, this document is a proposal and authorizes no code, PR,
merge, deployment, daemon restart, or package activation.
