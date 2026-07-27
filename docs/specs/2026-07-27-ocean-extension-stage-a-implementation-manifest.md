# Ocean Extension Host Stage A Implementation Manifest

**Date:** 2026-07-27

**Status:** proposed — awaiting operator ratification

**Program:** Ocean Crew Stage A / Ocean Extension Phases 2–3

**Implementation authority:** none until operator ratification

**Parents:** [`2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`](2026-07-14-ocean-extensions-architecture-and-migration-manifest.md), [`2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md`](2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md)
**Evidence baseline:** `ocean-os` `571c331d31db20b8327f23bd1b85fc1602c4260b`; accepted Phase 1 content `069d6af954a5e9d62b3399cb5e33742263aca9e3`

## 1. Decision requested

Ratify one exact implementation contract for Crew Stage A: rescue the accepted
Extension Phase 1 state reader onto current `main`, add a metadata-only
lifecycle protocol and supervised native-service host, then add daemon-owned
local/pinned-Git package mutations. The gate is a no-op service installed,
trusted, enabled, supervised, restarted, disabled, and removed without affecting
ordinary sessions.

Ratification of this document selects the recommended choices in §2 and
authorizes only slices A0–A5 in §18. It does not accept their implementation in
advance. Every slice still requires its tests, review, commit, upstream
reconciliation, and clean worktree.

## 2. Ratification choices

These choices close gaps that the parent manifests deliberately left open.
Ratification selects **Recommended** for each item.

| ID | Choice | Recommended selection | Rejected alternative |
| --- | --- | --- | --- |
| R1 | Service transport/auth | Host-supervised bidirectional stdio NDJSON v1. Pipe ownership plus the supervisor process record authenticates the child; the host injects identity in `host_hello`. | Child-facing bearer token or unauthenticated localhost HTTP/SSE. |
| R2 | Event replay | Boot-local, bounded, non-durable replay with explicit `lag`/`reset`; no Stage A cursor survives daemon restart. | Durable lifecycle log or silent live-only recovery. |
| R3 | `session_stopped` | Reserved v1 kind with **no Stage A producer** because current session authority has no stop/delete fact. Never infer it from idle, turn completion, client disconnect, switch, or daemon shutdown. | Inventing a terminal session event from a nearby but different fact. |
| R4 | Native authority | Service execution is allowed only after exact-digest trust plus an explicit native-process acknowledgement. Assigned cwd/state/env are confined, but Stage A does not claim a kernel sandbox. Packages declaring `network` or `filesystem` service capabilities cannot be enabled in v1. | Treating declared network/filesystem names as enforced sandbox grants. |
| R5 | Platforms | Service supervision is supported on macOS and Linux using a new process group and group termination. Windows may inspect/manage packages but returns `unsupported_platform` before service activation. | Claiming descendant cleanup from direct-child kill, or adding an unreviewed Windows job-object implementation. |
| R6 | Secret resolver | V1 resolves only `env:<SOURCE_NAME>` references, through an explicit operator grant binding to a requested child environment name. | Ambient inheritance, positional pairing, values in manifests/state/argv, or implicit provider credential lookup. |
| R7 | Active mutation safety | Disable must synchronously stop a service before update/remove; active update/remove returns `extension_active`. | Detaching, orphaning, or silently killing a service as a side effect of package replacement. |

R4 is an honest limitation, not a sandbox claim. A trusted native process still
runs as the daemon user and can attempt operating-system access outside its
assigned roots. Phase 4 reference extensions and broad third-party activation
remain blocked if they require network/filesystem enforcement not supplied by a
separately ratified sandbox lane. The Stage A no-op fixture declares neither.

## 3. Scope

Included:

- Phase 1 rescue A0;
- versioned lifecycle/service wire types and fixtures;
- structural redaction and authoritative production of the ten declared event
  kinds, including the explicit non-production rule for `session_stopped`;
- macOS/Linux native service supervision, health, bounded diagnostics,
  restart/backoff/circuit breaking, and process-group cleanup;
- daemon-confined extension mutable state/cache/temp roots;
- list/install/trust/enable/disable/remove/update through daemon HTTP and the thin
  `ocean-rs` CLI;
- offline local-path and exact-revision public HTTPS Git sources;
- one exclusive-lock registry writer, transaction journal, atomic publication,
  crash recovery, grant-diff confirmation, and rollback;
- cached runtime status in list/inspect/status while doctor remains static and
  non-probing;
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
- changing session JSON, current client SSE contracts, Observatory persistence,
  or Observatory authentication.

## 5. Authority and prerequisite A0

The accepted Phase 1 implementation exists at `069d6af9`, but it is absent from
this manifest's `origin/main` baseline. It provides strict installed/trusted/
enabled reads, descriptor-anchored digest verification, static inspect/doctor,
and the CLI read path. Stage A code must not be built on the stale commit or the
unrelated `feat/extension-phase2-3` branch.

A0 must replay only the accepted Phase 1 delta onto a freshly verified current
`origin/main`, preserve later main changes, rerun its full recorded gate, and
merge before A1 code. If replay changes accepted Phase 1 semantics, stop for a
new decision. A0 may update stale status prose but introduces no Phase 2–3
design or code.

## 6. Source ownership and exact boundaries

| Path | Stage A responsibility | Forbidden responsibility |
| --- | --- | --- |
| `crates/ocean-extension/src/lib.rs`, `tests/manifest.rs` | Existing package/service declaration validation; only additive validation required by this contract. | Process launch, registry state, wire I/O, secret values. |
| `crates/ocean-agent-sdk/src/extension_lifecycle.rs` (new), `src/lib.rs` | Public `ocean.extension.service` v1 NDJSON DTOs, lifecycle envelope, closed metadata enums, limits, and golden fixtures. | Supervisor state, daemon buses, package mutations. |
| `crates/ocean-daemon/src/extension_registry.rs` (refactored from accepted `extension_state.rs`) | Sole coherent reader/writer for install/trust/enable state, immutable store, journal recovery, static inspection, mutations, and source acquisition staging. | Service process ownership or event adaptation. |
| `crates/ocean-daemon/src/extension_lifecycle.rs` (new) | Dedicated metadata-only adapter, sequence/ring, scope filtering, and authoritative event emissions. | Full `AgentEventBus` forwarding, persistence, process launch, Observatory writes. |
| `crates/ocean-daemon/src/extension_service.rs` (new) | Reconciliation, stdio connection, health, queues, restart policy, process groups, environment/secret injection, diagnostics, and runtime status cache. | Package-file parsing authority, session execution, Crew RPC. |
| `crates/ocean-daemon/src/main.rs` | Thin route composition, `AppState` handles, exact event call sites, startup reconciliation, and shutdown ordering. | A second registry, inline supervisor implementation, orchestration policy. |
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
  "activation_revision": 7
}
```

The child cannot override that record. The first frame is:

```json
{"protocol":"ocean.extension.service","version":1,"frame":"host_hello","connection_id":"<uuid>","daemon_boot_id":"<uuid>","identity":{"package_id":"example.noop","package_version":"1.0.0","package_digest":"sha256:<hex>","service_id":"lifecycle","activation_revision":7},"limits":{"max_frame_bytes":65536,"outbound_messages":256,"outbound_bytes":1048576,"heartbeat_interval_ms":10000,"heartbeat_timeout_ms":5000}}
```

Within the manifest `startup_timeout_ms` (default 5,000; accepted range
100–30,000), the child replies:

```json
{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":["daemon_started","turn_started"],"resume":null}
```

`subscriptions` must be a duplicate-free subset of the manifest's declared
`events`; it may narrow but never expand them. `resume`, when present, is
`{"daemon_boot_id":"<uuid>","after_sequence":"<u64 decimal>"}`. The host then
sends exactly one:

```json
{"protocol":"ocean.extension.service","version":1,"frame":"ready","subscriptions":["daemon_started","turn_started"],"replay":"boot_local"}
```

After `ready`, a null resume receives the retained matching `daemon_started`
fact and then live events; it does not receive pre-activation session/turn
history. A valid resume receives retained matching events strictly after its
cursor and then live events. A reset is sent before live attach when resume
cannot be honored.

Readiness means the handshake succeeded and the process is alive; it does not
mean an external integration is reachable. Pipe ownership authenticates the
child. No bearer, daemon decision token, Observatory token, provider credential,
or secret identity value is sent for authentication.

Allowed child frames after readiness are only:

- `ack {"sequence":"<u64 decimal>"}` — highest contiguous processed sequence;
- `pong {"nonce":"<uuid>"}` — response to the host's current ping;
- `status {"state":"ready|degraded","code":"<closed-code>"}` — optional,
  code from `external_unavailable|configuration_missing|rate_limited|unknown`;
- `shutdown_complete {}` — response during graceful shutdown.

Allowed host frames after readiness are only `event`, `lag`, `reset`, `ping`,
and `shutdown`. A child command, RPC method, event publication, subscription
expansion, or arbitrary payload is a protocol violation. Stage B must version
and ratify any mutation/RPC extension of this channel.

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
host-derived. `project_id` is present only when the daemon maps the session to a
registered project; no path-derived project identity is minted.

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

### 8.2 Authoritative source mapping for all ten events

| Declared event | Authoritative Stage A source | Exact rule |
| --- | --- | --- |
| `daemon_started` | Daemon startup after lifecycle dispatcher creation and before service reconciliation; existing `ObservatoryAdapter::daemon_started` is precedent, not the delivery source. | Sequence 1 and first retained boot fact. A late/restarted service can replay it while retained. |
| `session_started` | Every daemon call site that emits `AgentTurnEvent::SessionCreated`, including the ordinary new-session turn path. | Adapt only the fact and session id; strip title and cwd. Emit once for the created session fact. |
| `turn_started` | Ordinary turn admission immediately after the session-operation lease succeeds, at the existing `AgentTurnEvent::TurnStarted` emission. | Rejected/busy turns emit nothing. |
| `permission_requested` | `DaemonPermissionPolicy::check` after the waiter is inserted and before the policy waits. | Strip args and free-text reason; retain host ids and tool name. |
| `permission_resolved` | The single terminal branch of `DaemonPermissionPolicy::check`, covering an authorized decision, request cancellation, or closed waiter. The HTTP decision route remains one input, not a second event producer. | Exactly one resolution for each emitted request; strip denial/approval reason text. |
| `tool_started` | Runtime bridge `AgentEvent::ToolExecutionStart`. | Strip args. Do **not** translate the compatibility `PermissionDenied` Started/Finished pair into execution facts because no tool ran. |
| `tool_finished` | Runtime bridge `AgentEvent::ToolExecutionEnd`, correlated with its runtime tool-call id/name/start instant. | Compute fixed outcome, duration, and rendered output byte count; discard content/details. |
| `turn_finished` | Existing terminal `AgentTurnEvent::TurnFinished` handed to `record_prompt_result` after the runtime bridge drains. | Strip error/output; map status to the closed outcome. |
| `session_stopped` | **None exists on the baseline.** | Never emitted in Stage A. It remains declared for schema compatibility. Adding an explicit daemon session-stop/delete authority later requires a separate contract update and then becomes this event's only source. |
| `daemon_stopping` | Graceful daemon shutdown immediately before supervisor drain begins. | Best effort before `shutdown`; absent after crash/SIGKILL. It is never used to synthesize `session_stopped`. |

### 8.3 Ordering

Within one turn the dispatcher preserves authoritative call-site order:
`session_started? → turn_started → (permission_requested →
permission_resolved)? → tool_started → tool_finished → turn_finished`.
Multiple tools repeat the inner sequence; concurrent sessions may interleave.
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
project. At delivery time the host recomputes effective enablement at the
current registry revision:

- daemon lifecycle facts are delivered when the service has at least one
  effective global or registered-project activation;
- a session/turn fact is delivered only when the package is effective for that
  session's registered project under global default plus project override;
- project-less/unregistered sessions receive events only under effective global
  enablement;
- a project override cannot add trust or widen the manifest subscription;
- enablement changes update the filter before their HTTP mutation returns.

The child receives only kinds in both its manifest declaration and negotiated
subscription. It never receives another package's identity or grant state.

### 9.2 Bounds

- Global boot ring: at most 2,048 event frames and 8 MiB encoded bytes; evict
  oldest until both hold. An individually oversized event is rejected before
  publication and records only a fixed host diagnostic.
- Per-service outbound data queue: at most 256 frames and 1 MiB encoded bytes,
  plus one reserved control slot for `lag`, `reset`, `ping`, or `shutdown`.
- A slow/full service never backpressures the dispatcher or daemon request path.
- Acknowledgements are monotonic and cannot exceed the highest sent sequence.
  They acknowledge the highest received event, not a numerically contiguous
  global range: gaps are normal when other projects/packages are filtered out.

When data must be discarded, the host coalesces the lost range and sends:

```json
{"protocol":"ocean.extension.service","version":1,"frame":"lag","first_lost":"10","last_lost":"18","lost_count":9,"replay_available":true}
```

The child may reconnect and request resume in `service_hello`. If the boot id
matches and `after_sequence` remains retained, the host replays strictly after
it before live attach. Otherwise it sends:

```json
{"protocol":"ocean.extension.service","version":1,"frame":"reset","reason":"boot_changed|retention_exceeded|invalid_cursor","oldest_available":"<u64-or-null>","latest_available":"<u64-or-null>"}
```

Then it attaches live. Stage A never promises gap-free, exactly-once, or durable
delivery. Services deduplicate by `(daemon_boot_id, sequence)` if desired. No
transcript or tool payload is retained in the ring.

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
revision, state, pid when live, started/observed timestamps, restart count,
negotiated subscriptions, last acknowledged sequence, lag count, and one fixed
reason code. It contains no argv values beyond manifest metadata, environment,
secret, stderr text, prompt, or payload. Status is in-memory runtime projection,
not session JSON or immutable store.

`GET /v1/extensions/{id}/status` reads this cache and starts/probes nothing.
List and inspect include the same summary. Doctor may report the cached summary
but remains a static no-execution/no-probe read.

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

On disable or graceful daemon shutdown the host:

1. stops new event enqueue;
2. sends `shutdown {"reason":"disabled|daemon_stopping|reconfigure"}`;
3. closes stdin after `shutdown_complete` or 2 seconds;
4. sends `SIGTERM` to the Unix process group;
5. waits 2 seconds;
6. sends `SIGKILL` to the group and reaps the direct child;
7. removes the connection temp directory and publishes terminal status.

The child is spawned as leader of a new process group on macOS/Linux. Tests must
prove a normal child and grandchild die on disable, crash reconciliation, and
daemon shutdown. Packages must not daemonize into another session/process group;
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
sandbox.

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

A0's accepted v1 state remains authoritative and gains explicit mutation
support:

```text
<config_dir>/extensions/
├── installs.json
├── trust.json
├── enabled.json
├── service-grants.json                   # native ack + secret bindings; no values
├── store/<extension-id>/<digest>/        # immutable verified payload
├── state/<extension-id>/...              # mutable, §11.2
├── staging/<transaction-id>/             # never executable
├── transactions/<transaction-id>.json   # no secret values
└── .state.lock
```

The CLI/TUI never write these paths. All reads hold a shared lock across state
and artifact inspection. Every mutation and recovery holds the exclusive lock.
`service-grants.json` is an additive Stage A companion because accepted A0's
strict `trust.json` schema cannot safely absorb new fields. It carries the same
`state_revision`; absence on an A0-only registry means an empty service-grant
set at the three accepted files' revision. Once created, absence or revision
mismatch fails closed. An A0 binary ignores the extra file and can still parse
all three accepted schemas, but cannot activate services because A0 has no
supervisor. Existing A0 limits remain: each state/manifest file 1 MiB, 1,024
records, 10,000 package entries, depth 64, 256 MiB final artifact, and 250 ms
lock wait.

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
Disable is always allowed and returns only after its effective filter is removed
and any now-unneeded service is reaped. Update/remove require the package fully
disabled and stopped.

### 12.3 Journaled publication and recovery

For each mutation the daemon:

1. acquires `.state.lock` exclusively and recovers any prior journal;
2. verifies `expected_state_revision` and all preconditions;
3. stages bounded bytes and all four complete next-revision state files on the
   same filesystem; computes and records their hashes;
4. writes/fsyncs a `prepared` journal containing operation id/type, old/new
   revision, non-secret source/grant metadata, staged names/hashes, and intended
   immutable-store destination; atomically renames and fsyncs it;
5. atomically publishes the immutable artifact if applicable;
6. renames `installs.json`, `trust.json`, `enabled.json`, and
   `service-grants.json` to the complete staged files while the exclusive lock
   prevents a reader from observing the intermediate set;
7. fsyncs each file and the extensions directory, marks the journal `committed`,
   fsyncs, then removes staging/journal and fsyncs their parent directories;
8. releases the lock and reconciles the supervisor to the committed revision.

All four documents carry the same nonzero revision after the first Stage A
mutation; the A0-only absence exception for `service-grants.json` is defined in
§12.1. Recovery validates journal and staged hashes. If no state file was
replaced, it removes staging and
an unreferenced just-published artifact. If any state file reached the new
revision, it rolls forward all remaining verified new files; it never guesses a
rollback from a mixed generation. A corrupt/missing required staged file fails
closed with `registry_recovery_required` and starts no extension. Orphan store
payloads are non-executable and may be removed only after proving no install or
journal references them.

Failed acquisition, validation, Git/local staging, grant confirmation, publish,
fsync, or reconciliation never returns success. Transaction failure leaves the
old coherent revision effective. Supervisor reconciliation failure leaves the
registry committed but reports `reconciliation_pending`; it never rolls state
back while a process might have observed the new filter.

## 13. Install sources

### 13.1 Local path

HTTP accepts an absolute UTF-8 directory path. The CLI canonicalizes a relative
operand against its own cwd before sending it. The daemon opens the directory
descriptor-relative, rejects symlinks, hardlinked regular files (`nlink != 1`),
FIFOs, sockets, devices, sparse/oversize files, escapes, depth/entry/byte limit
violations, and a missing/invalid manifest. It copies verified regular-file
bytes into staging and re-hashes the staged tree before publication. Local
install performs no network access and works offline.

### 13.2 Pinned public Git v1

Accepted grammar:

```text
source.kind = "git"
url         = "https://<public-host>/<nonempty-path>[.git]"
revision    = "<exact 40- or 64-character lowercase hex object id>"
subdir      = "<optional confined relative UTF-8 path>"
```

Rules:

- HTTPS only; no SSH/scp/file/git schemes, userinfo, password, query, fragment,
  IP literal, non-443 explicit port, control character, or local/loopback/private
  host after DNS resolution;
- revision is an object id, never branch/tag/HEAD/short SHA; fetched commit id
  must equal it exactly and must be a commit;
- `subdir` uses normal relative components only; no empty, `.`, `..`, absolute,
  backslash, NUL, or symlink traversal;
- 60-second total acquisition deadline, 512 MiB Git object/temp ceiling, 256 MiB
  extracted package ceiling, 10,000 entries, depth 64, and four concurrent
  acquisitions daemon-wide;
- redirects disabled; every resolved address is rechecked before connection;
- no submodules, Git LFS, worktree checkout, smudge/clean filter, hooks, or build
  scripts.

Implementation invokes the host `git` binary only as an acquisition tool in a
new process group with an empty environment plus fixed `PATH`, `HOME` pointing
to an empty mode-0700 temp directory, `GIT_CONFIG_NOSYSTEM=1`,
`GIT_CONFIG_GLOBAL=/dev/null`, `GIT_TERMINAL_PROMPT=0`, and
`GIT_ASKPASS=/usr/bin/false`. Every command also sets empty credential helper,
disabled hooks, disabled LFS/filter processing, no optional locks, no redirects,
and no tags. It initializes an empty temp repository, fetches only the exact
object with depth 1, verifies `FETCH_HEAD`, and extracts the commit tree through
an archive/read-tree path that never checks out or executes content. Timeout or
size excess terminates the Git process group and deletes staging.

Git is host code, not package code. Nevertheless its exact argv contains no
secret, credential helper, SSH command, or inherited proxy/auth environment.
A server that requires credentials, LFS, a submodule, a redirect, or a named ref
is unsupported in v1.

### 13.3 Update

Update requires the package disabled/stopped and an explicit new local or Git
source. There is no `latest`. It stages and validates the replacement, requires
the same extension id, and atomically changes the install record. The new digest
is untrusted even if version/source match; enablement records may remain but are
ineffective until a separate trust transition. The previous immutable payload
and trust row are retained for audit/explicit pinned rollback until ordinary
orphan cleanup proves them unreferenced.

## 14. Grant diff confirmation

A trust request supplies the exact installed digest, a subset of manifest
requests, secret bindings, and `native_process_ack=true` for any native service.
The daemon canonicalizes sorted sets and returns a diff against the current
exact-digest grant:

```json
{
  "added": {"network":[],"filesystem":[],"env":["SLACK_APP_TOKEN"],"secrets":["env:OCEAN_SLACK_APP_TOKEN"],"secret_bindings":[{"target_env":"SLACK_APP_TOKEN","reference":"env:OCEAN_SLACK_APP_TOKEN"}]},
  "removed": {"network":[],"filesystem":[],"env":[],"secrets":[],"secret_bindings":[]},
  "native_process_ack_changed": true,
  "confirmation": "sha256:<hash-of-id,digest,current-revision,canonical-diff>"
}
```

Without the matching `confirm_grant_diff`, the request is preview-only and
mutates nothing. Apply requires the same digest and state revision used in the
hash; races return `state_revision_conflict` and require a new preview. No
`--yes`, wildcard, `all`, or manifest-self-grant exists. Network/filesystem
nonempty grants are rejected under R4 rather than merely confirmed. Narrowing
and revocation also use a diff confirmation because they may restart/stop a
service.

## 15. Exact HTTP and CLI contract

All JSON request types deny unknown fields. Mutations include
`expected_state_revision`; the CLI obtains it from list/inspect and never
silently retries a conflict. Errors use
`{"ok":false,"error":{"code":"<closed-code>","message":"<fixed-safe-text>"}}`
and never include secret values or raw Git/stderr output.

| Operation | Daemon HTTP | `ocean-rs` CLI |
| --- | --- | --- |
| list | `GET /v1/extensions?project_id=<uuid?>` | `ocean-rs extension list [--project-id UUID]` |
| inspect | `GET /v1/extensions/{id}/inspect?project_id=<uuid?>` | `ocean-rs extension inspect ID [--project-id UUID]` |
| doctor | `GET /v1/extensions/{id}/doctor?project_id=<uuid?>` | `ocean-rs extension doctor ID [--project-id UUID]` |
| runtime status | `GET /v1/extensions/{id}/status` | `ocean-rs extension status ID` |
| local install | `POST /v1/extensions/install` with `{"expected_state_revision":N,"source":{"kind":"local-path","path":"/absolute/path"}}` | `ocean-rs extension install --path PATH` |
| Git install | same route with `{"source":{"kind":"git","url":"https://…","revision":"<hex>","subdir":null}}` | `ocean-rs extension install --git URL --rev HEX [--subdir REL]` |
| trust preview/apply | `POST /v1/extensions/{id}/trust` with `{"expected_state_revision":N,"digest":"sha256:…","capabilities":{…},"secret_bindings":[…],"native_process_ack":true,"confirm_grant_diff":null|"sha256:…"}` | `ocean-rs extension trust ID --digest DIGEST [--grant-env NAME] [--grant-secret REF] [--bind-secret TARGET=REF] [--ack-native-process] [--confirm-grant-diff HASH]` |
| enable | `POST /v1/extensions/{id}/enable` with `{"expected_state_revision":N,"scope":{"kind":"global"}}` or `{"scope":{"kind":"project","project_id":"uuid"}}` | `ocean-rs extension enable ID [--project-id UUID]` |
| disable | `POST /v1/extensions/{id}/disable` with the same scope shape | `ocean-rs extension disable ID [--project-id UUID]` |
| remove | `DELETE /v1/extensions/{id}` with JSON `{"expected_state_revision":N,"purge_state":false}` | `ocean-rs extension remove ID [--purge-state]` |
| local update | `POST /v1/extensions/{id}/update` with expected revision plus local source | `ocean-rs extension update ID --path PATH` |
| Git update | same route plus Git source | `ocean-rs extension update ID --git URL --rev HEX [--subdir REL]` |

`--path` and `--git` are mutually exclusive; `--rev/--subdir` require `--git`.
URL path ids are percent-encoded by the CLI and revalidated by the daemon.
Install rejects an already installed id with `already_installed`; update rejects
an absent id. Enable rejects missing trust, unresolved bindings, declared
network/filesystem capabilities, incompatible host, or unsupported platform.
Remove/update return `extension_active` until every scope is disabled and the
service is reaped. Remove never deletes another digest/package state.

Successful mutation responses include `state_revision`, affected id/digest,
`effective`, and a bounded reconciliation summary. `202` is used only when a
committed revision is waiting for asynchronous startup; disable/remove return
`200` only after stop/reap. Trust preview is `200` with `applied:false`; apply is
`200` with `applied:true`.

## 16. No-code-execution boundary

The following operations must not spawn a package entry, plugin, hook, build
script, health probe, shell, language package manager, or provider call:

- list, inspect, doctor, status;
- local/Git install acquisition and validation;
- trust preview/apply;
- update staging/publication;
- disabled discovery and registry recovery.

Git acquisition may execute the confined host `git` command described in §13.2,
but never package content. The first possible package execution is supervisor
reconciliation after a separately committed trust grant and enablement. Tests
use executable canaries in every resource path and prove their markers remain
absent through all operations above.

## 17. Active-service safety, rollback, and recovery

- Disable removes delivery eligibility before shutdown begins and returns only
  after reap. A project disable that leaves another effective scope does not
  stop the shared service but immediately removes that project's events.
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
  A2 can be disabled operationally by disabling every service; legacy plugins
  and hooks remain untouched. A3–A4 state remains schema v1 and readable by A0;
  no slice may make A0 unable to inspect a committed registry generation.
- If an A2–A4 change cannot preserve that downgrade/read compatibility, stop and
  ratify a state schema migration before merge.

## 18. Ordered PR slices

### A0 — rescue accepted Phase 1

Replay `069d6af9` content onto current `origin/main`, resolve only main drift,
restore truthful Phase 1 accepted status, rerun the accepted gate, and merge.
No Phase 2–3 behavior.

### A1 — protocol and pure lifecycle adapter

Land the ratified version of this manifest; add SDK v1 DTOs/golden fixtures,
closed metadata types, exhaustive pure adapter, boot ring, scoping/order tests,
and all ten event mappings. Do not spawn a service or add mutations. The
`session_stopped` non-emission test is mandatory.

### A2 — supervised no-op service host

Add stdio handshake, queue/replay, environment/secret binding consumption from
hand-authored Stage A registry fixtures, state/cache/temp roots, status cache,
health, restart/circuit
breaker, stderr handling, process groups, startup/shutdown reconciliation, and
a no-op fixture. Wire authoritative event call sites. No registry mutations;
tests use state fixtures written before daemon start.

### A3 — local registry mutations

Refactor the accepted reader into the single registry authority. Add journaled
list/local-install/trust/enable/disable/remove/update, grant preview/apply,
routes/CLI, static status projection, and active-service reconciliation. No Git
network acquisition.

### A4 — pinned public Git acquisition

Add only §13.2 Git source handling to install/update, with DNS/URL, credential,
process-group, timeout, byte, revision, subdir, no-submodule/LFS/filter/script,
and rollback tests.

### A5 — integrated Stage A gate and closeout

Add/run the E2E matrix in §19 against local and pinned Git no-op packages,
complete independent security/correctness/architecture review, full CI/MSRV/
compatibility, operator acceptance record, and docs/devlog closeout. No Stage B
code, reference integration, deployment, or extension repository creation.

A1–A5 are strict order. A later slice may not be smuggled into an earlier PR to
avoid its review boundary.

## 19. Acceptance matrix and precise test gates

### 19.1 Protocol and lifecycle

- Golden encode/decode and unknown-field/version/frame rejection at 65,536/
  65,537-byte boundaries.
- Handshake identity cannot be overridden; subscription is an exact subset;
  missing/late/duplicate hello fails.
- All ten event kinds have schema fixtures and source-table tests.
- New session exact order; resumed session omits `session_started`; rejected
  admission emits neither turn nor session facts.
- Permission request resolves exactly once for allow, allow-session, deny,
  cancellation, and waiter closure; args/reasons absent.
- Runtime permission denial does not fabricate tool execution in extension
  lifecycle.
- Tool/turn outcome/duration/count metadata is correct; raw data absent.
- `session_stopped` is never inferred or emitted in Stage A.
- Concurrent-session ordering is per authoritative sequence and project scope;
  one project/package cannot observe a disabled scope.
- Sentinel prompts, paths, args, results, errors, headers, env, secrets, canvas,
  and arbitrary extension payloads cannot serialize or appear on the wire.
- Observatory store/cursors/tokens/routes are unchanged and observer delivery
  cannot mutate, cancel, or publish.

### 19.2 Queue/replay/failure isolation

- Boot ring count/byte eviction and per-service queue count/byte limits.
- Slow reader produces coalesced `lag`, never delays a turn/permission/session;
  retained resume replays, old/wrong-boot resume resets explicitly.
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
- Child and grandchild are gone after disable, health failure, daemon shutdown,
  and cancellation of a timed-out Git acquisition.
- Windows status is `unsupported_platform` and no child starts.
- State/cache/temp roots reject symlink/path replacement; temp is cleaned;
  data persists or purges exactly as requested.

### 19.4 Registry/package management

- Entire accepted A0 suite remains green: absent/coherent state, equal revision,
  lock contention, descriptor/no-follow traversal, digest replacement, symlink,
  hardlink, FIFO/special file, depth/count/byte/manifest limits, registered
  project, grant subset, and no-execution.
- Every mutation checks expected revision and exclusive lock; readers never see
  mixed generations, including A0-only three-file state and Stage A four-file
  state.
- Crash injection before journal, after journal, after store publish, after each
  state rename, and after directory fsync proves rollback/roll-forward rules.
- Install ≠ trust ≠ enable; changed digest loses effective trust; project enable
  cannot widen global trust.
- Grant preview mutates nothing; stale/wrong confirmation fails; exact
  confirmation applies; widening/narrowing/binding/native ack diffs are stable.
- Enable rejects untrusted/incompatible/unsupported/network-or-filesystem
  service packages.
- Active update/remove refuses; disable removes scope and reaps; retained/purged
  state behavior is exact.
- Local install is offline. Every inspect/install/trust/update code-execution
  canary remains untouched.
- Git rejects branch/tag/short/uppercase revision, redirect, credentials,
  userinfo, private/loopback resolution, SSH, helper/config inheritance,
  submodule, LFS/filter, special file, bad subdir, timeout, object/extract limits,
  and commit mismatch; exact public commit succeeds.

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
3. trust preview then exact confirmation with native acknowledgement; prove not
   enabled and not running;
4. enable; prove host-injected identity, assigned roots, minimal env, readiness,
   and one live process group;
5. run new/resumed ordinary sessions with permission and tools; prove exact
   scoped metadata events, forbidden-payload absence, and ordinary client/SSE
   compatibility;
6. force lag then retained replay and reset; ordinary turns stay responsive;
7. crash repeatedly; prove numeric backoff/circuit-open, then disable→enable
   recovery;
8. restart daemon; prove new boot id, preserved package data, paused/non-durable
   replay posture, single reconciled service, and no orphan descendant;
9. disable one project scope and global scope; prove filter update, shutdown,
   child/grandchild reap, and data retention;
10. active remove/update refusal; disabled exact-revision update installs an
    untrusted digest and does not start until separate trust+enable;
11. remove preserving state, reinstall, then disable/remove with purge;
12. repeat install/update from one exact public HTTPS Git commit with a pinned
    revision; reject an adjacent branch/tag and credential-requiring source;
13. rerun ordinary sessions with the extension absent and confirm no observable
    behavior change;
14. complete full gates and independent security/correctness/architecture
    review; operator separately accepts Stage A.

Passing A5 does not authorize Stage B until its own implementation manifest is
ratified, as required by the accepted Crew parent.

## 21. Stop conditions

Stop and request a design amendment if implementation would:

- start before A0 is merged and accepted on the current base;
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
- the ten events map honestly to current authority and `session_stopped` is not
  fabricated;
- stdio identity/auth, handshake, framing, queue, replay, health, restart,
  process, environment, secret, and diagnostic behavior is bounded;
- state/store/session ownership is unambiguous;
- install, trust, and enable are distinct daemon-owned transitions;
- HTTP/CLI, Git grammar/limits, journal recovery, grant diff, active-service
  safety, rollback, and no-execution rules are exact;
- A0–A5 boundaries, stop conditions, validation, and the E2E gate prevent
  premature Crew/UI/integration work;
- Observer and Observatory remain read-only and Ocean core remains free of Crew
  vocabulary.

Until ratification, this document is a proposal and authorizes no code, PR,
merge, deployment, daemon restart, or package activation.
