# Ocean OS Code Health and Agent Readiness Plan

**Date:** 2026-07-12
**Status:** Approved program complete through build compatibility — all planned foundation, characterization, extraction, and compatibility checkpoints passed local, independent-review, and hosted gates
**Owner:** Smaths / Ocean OS
**Primary goal:** Make Ocean OS easier for humans and agents to understand, navigate, modify, and verify without destabilizing its behavior or turning cleanup into a rewrite.

## 1. North-star goal

A cold agent should be able to enter Ocean OS, identify the correct owner and entry point, understand the critical invariants, make one bounded change, and run the right validation without rediscovering the architecture from scratch.

The codebase should remain the same product while becoming:

- easier to route work into;
- easier to review in small units;
- harder to change across an unsafe boundary accidentally;
- better protected by compiler, test, documentation, and performance gates;
- measurably easier for a fresh agent to use.

## 2. Principles

1. **Preserve behavior before improving design.** Mechanical moves and architectural redesign never share a change set.
2. **Map before moving.** Each extraction starts with ownership, callers, invariants, tests, and rollback points.
3. **Tests before fixes for suspected bugs.** A risk becomes a behavior change only after a focused regression or benchmark proves it.
4. **Profile before optimizing.** Clone counts and file sizes identify places to inspect, not defects by themselves.
5. **Stable contracts beat elegant rewrites.** HTTP/SSE shapes, session files, permission gates, cwd binding, public Rust imports, and TUI event semantics remain stable during extraction.
6. **One writer, independent reviewers.** Read-only analysis can fan out; each active worktree has one implementation writer.
7. **Prefer durable indexes over doc sprawl.** Add local `AGENTS.md` files only where a real boundary has unique rules.
8. **Every wave is independently shippable and reversible.** Small reviewable commits, narrow tests first, full gates at wave close.

## 3. Evidence baseline

The baseline was collected on `main` at commit `b5d564169f3f7034c2794007ed9795be3e6bb498` on 2026-07-12 with `rustc 1.97.0` and `cargo 1.97.0`. The worktree contained pre-existing non-source changes (`events.md`, `.pi-subagents/`, and `target3/`), which were not used as source evidence. Cold-agent routing evidence is recorded in `docs/specs/2026-07-12-ocean-agent-readiness-baseline.md`.

### 3.1 What is already strong

- Approximately 136k lines of Rust across 25 workspace packages: 24 product crates plus `xtask`.
- Normal `cargo clippy --workspace --all-targets` emitted no warnings; CI runs all-target Clippy with `-D warnings`.
- CI builds and tests the workspace on macOS and Linux, runs all-target Clippy, checks formatting, and runs `cargo deny`.
- The strict default-feature inventory used compiler-selected library, binary, and example targets while excluding test targets:

  ```bash
  cargo clippy --workspace --lib --bins --examples --message-format=short -- \
    -W clippy::unwrap_used \
    -W clippy::expect_used \
    -W clippy::panic \
    -W clippy::unreachable \
    -W clippy::await_holding_lock
  ```

  It found 17 `unwrap()` warnings, 48 `expect()` warnings, 4 `unreachable!()` warnings, 0 `panic!()` warnings, and 0 `await_holding_lock` warnings. Store the raw output and counting rules when this baseline is automated; it does not cover test targets, non-default feature combinations, or expanded macro internals.
- Those counts are an invariant inventory, not a bug count. Many sites are locally proven conditions such as piped child stdio or map entries inserted immediately above.
- The reviewed compiler-selected production paths contain one direct `unsafe` block: a documented `RawWaker` construction in the daemon persistence helper.
- Permission, cwd, cancellation, session-compatibility, event-ordering, and TUI mutation invariants are unusually well documented in the crate contracts that exist.

### 3.2 Structural pressure

| File | Total lines | Primary issue |
|---|---:|---|
| `crates/ocean-daemon/src/main.rs` | 21,636 | About 11.8k production lines plus a large inline test module; many route and domain responsibilities share one compilation unit. |
| `crates/ocean-tui/src/main.rs` | 9,304 | Binary routing, active mesh support, and retained legacy UI coexist; the current `shell/` is already modular and should not be rebuilt. |
| `crates/ocean-agent/src/lib.rs` | 7,354 | Runtime facade, turn pipeline, session store, compatibility logic, prompt shaping, and large tests share one root. |

File size is a navigation and review signal. It is not evidence of runtime slowness by itself.

### 3.3 Pre-Phase-0A agent-readiness gaps

- `crates/AGENTS.md` indexes only 8 of the 24 product crates represented in the workspace.
- Root and bootstrap crate maps are incomplete relative to `Cargo.toml`.
- `OCEAN.md` describes a two-repo system while `docs/OCEAN_PROJECT_MAP.md` defines the active four-repo system.
- Root `HANDOFF.md` is transient lane/deployment state from 2026-07-09 but appears as evergreen onboarding material.
- The root verification contract names `cargo check --workspace`, while CI enforces a broader merge gate.
- Active docs contain references into `docs/.agentarchive`, despite that archive being opt-in and excluded from normal agent context.
- `docs/OCEAN_PROJECT_MAP.md` references an untracked/missing `docs/OCEAN_PROJECT_MAP_ART.html` artifact.
- There is no single local command for CI parity or documentation/index integrity.

### 3.4 Reliability and performance risks to characterize

#### Runtime-to-daemon event payloads

`agent_turn` uses an unbounded runtime event channel, while `ToolExecutionEnd` carries cloned full `content` and structured `details` before the transcript copy is capped. Several native tools already cap output, but external/plugin/MCP/browser capabilities may not share one global byte ceiling. The risk is unbounded retained bytes, not merely message count.

This needs a checked event-policy table, payload inventory, and stress tests before choosing among:

- capping display-event content at the runtime boundary;
- spilling large content to artifacts and sending references;
- bounded async backpressure;
- a byte-aware bridge.

#### Shell cancellation

Direct verification rejected a review false positive: `BashTool` already sets `kill_on_drop(true)`, and `tools_smoke.rs` covers a direct child on timeout. It does not yet prove Halt cancellation or descendant-tree termination. Add OS-specific Halt tests for both a direct child and a descendant tree before changing process-group handling.

#### Lazy browser startup

`LazyBrowser::get` at `crates/ocean-runtime/src/tools/browser/mod.rs:56-72` intentionally holds a Tokio mutex while probing and launching Chrome. This provides single-flight behavior but can serialize all browser tools behind slow external I/O. Characterize healthy, dead, stalled, and cancelled launch cases through injected/fake probe and launch seams with explicit deadlines before redesigning the state machine.

#### Repeated transcript work

The agent loop serializes messages to estimate size and clones retained messages each provider round. The current harness benchmark already shows turn-cost and latency sensitivity. Add allocation/time benchmarks before changing ownership or caching.

#### CI coverage

At baseline, CI did not compile the release profile or verify the declared Rust 1.80 MSRV. Characterization later proved 1.80 incompatible with the resolved graph, established 1.88 as the truthful floor, and added release/feature/MSRV manifests; see Phase 1B-4.

## 4. Success criteria

### 4.1 Agent navigation

- All 25 workspace packages appear in one canonical workspace index; root/bootstrap maps become generated views or concise pointers rather than competing inventories.
- A fixed ten-case cold-agent benchmark covers session persistence, provider wire format, runtime tools, TUI event mutation, daemon cwd/routes, calls, Longhouse, MCP/plugins, context/hashline, and workspace membership. Each fresh-context run starts at the repo root without prior search/session context and returns owner repo/crate, entry point, invariant, narrow command, and elapsed time. Record a baseline and repeat after Phase 1A; target at least 9/10 correct within five minutes, with the task corpus and raw outputs retained.
- Active docs contain no contradictory system-boundary statements, broken local links, or required links into `.agentarchive`.
- CI and the documented merge gate stay mechanically aligned through one command manifest.
- A new crate or workspace-member change fails automation if the canonical index is not updated.

### 4.2 Reliability

- Every `AgentEvent` cross-layer payload has a checked policy covering ownership/cloning, maximum inline bytes, retention lifetime, overflow behavior, and durable-evidence behavior, even when characterization concludes no code change is needed.
- Halt/cancellation behavior is tested for direct shell children and descendant process trees on each explicitly supported platform.
- Browser launch has tested single-flight, timeout, cancellation, and retry behavior.
- No new permission, cwd, session, event-ordering, or persistence regressions.

### 4.3 Structure

Navigation targets, not hard stylistic limits:

- `ocean-daemon/src/main.rs` becomes composition-only and preferably under 500 lines.
- `ocean-tui/src/main.rs` becomes parse-and-route only and preferably under 300 lines.
- `ocean-agent/src/lib.rs` becomes a facade/re-export root and preferably under 500 lines.
- Production modules should generally stay under 1,500 lines unless cohesion and invariants justify otherwise.
- Existing public import paths, route method/path sets, middleware order, serialized data, session layout, and SSE semantics remain unchanged during mechanical extraction.

### 4.4 Performance

- Agent-loop/context improvements require before/after benchmark evidence.
- Benchmarks report wall time, allocations/bytes where practical, peak RSS, provider rounds, and token/cache behavior.
- No optimization is accepted solely because it reduces `.clone()` count.

## 5. Program sequence

### Phase 0 — Ground truth and characterization

**Purpose:** Make the next edits safer before moving code.

#### 0A. Canonical navigation contract — docs-only

1. Make `crates/AGENTS.md` the canonical checked workspace index for all 25 packages. Root `AGENTS.md`, README, and `OCEAN.md` become concise curated maps or pointers rather than separately maintained competing inventories.
2. Make the four-repo project map canonical and reconcile `OCEAN.md` and README system-boundary language.
3. Move transient root `HANDOFF.md` into the archive or replace it with a short pointer to current contracts and `events.md`.
4. Align root verification guidance with CI, explicitly separating:
   - fast edit-loop checks;
   - crate-local completion checks;
   - portable local merge gates;
   - host-specific and CI-only gates.
5. Give every canonical workspace-index row:
   - owns;
   - does not own;
   - primary entry point;
   - local contract when one exists;
   - narrow test command;
   - non-default-member rationale when applicable.
6. Add a compact cross-crate change-impact matrix for events, sessions, tools, models, provider wire changes, routes, and persistence.
7. Remove broken or required active-doc links into `.agentarchive`; fix or remove the missing project-map artifact link.
8. Run and retain the fixed cold-agent routing benchmark before and after the navigation changes.
9. Do not create 16 boilerplate crate contracts. Add a crate-local `AGENTS.md` only when the crate has unique invariants or a meaningful safe-edit boundary.

**Gate:** docs link/index review, Cargo metadata parity, benchmark artifacts, no source changes, independent reviewer acknowledgement.

**Completion (2026-07-12): PASS.** `crates/AGENTS.md` now indexes all 25 packages; competing bootstrap maps point to it; the four-repo boundary, current handoff, contributor guide, CI-aligned gates, archive policy, non-default-member rationale, and active links are reconciled. The before/after cold-agent benchmark improved from 28/30 to 30/30 and eliminated the repeated legacy-TUI routing miss; see `docs/specs/2026-07-12-ocean-agent-readiness-baseline.md`.

#### 0B. Independent characterization checkpoints

These are separate changes with separate owners and artifacts. Browser and performance characterization do not block unrelated docs or intact module moves unless they expose a blocker in the files being moved.

##### 0B-1. Event payload policy and stress

1. Produce a checked table for every `AgentEvent` variant: payload fields, ownership/cloning points, maximum inline bytes, queue/replay retention lifetime, overflow behavior, and durable-evidence behavior.
2. Inventory maximum `content` and `details` payloads from built-in, MCP, plugin, and browser tools.
3. Run oversized-output stress in an isolated child process with a hard timeout/RSS ceiling, finite deterministic payload/concurrency limits, a slow or disconnected consumer, and assertions for drain/replay behavior.

**Result (2026-07-12): RED baseline; smallest fix PASS.** The checked 17-variant/lifecycle/tool-source inventory and finite child tests proved that the per-turn runtime queue is unbounded and the 2,048-event replay ring had no byte ceiling. Eight/nine 1 MiB events passed deterministic drain/lag/disconnect/replay assertions at 18.5/26.8 MiB maximum RSS under 30-second/256-MiB limits. The daemon now also enforces a 32-MiB serialized-payload replay ceiling while preserving full live delivery; focused/full gates and independent security review passed. See `docs/specs/2026-07-12-ocean-event-payload-characterization.md`.

##### 0B-2. Shell Halt behavior

1. Keep the existing direct-child timeout test.
2. Add PID-based Halt characterization for a direct child and a descendant tree on macOS and Linux, with cleanup that still runs when assertions fail.
3. Record unsupported-platform behavior explicitly.

**Result (2026-07-12): RED baseline; smallest Unix fix PASS on macOS/Linux.** PID tests proved direct-child Halt passed while a signal-resistant background descendant survived. `BashTool` now owns a Unix process group and kills it on future drop/timeout, retaining direct-child `kill_on_drop`; direct and descendant tests, the full local gate, independent security review, and GitHub's macOS/Ubuntu lanes pass. Non-Unix tree termination and deliberately re-sessioned descendants are explicitly outside this contract. See `docs/specs/2026-07-12-ocean-shell-halt-characterization.md`.

##### 0B-3. Browser single-flight behavior

1. Introduce test seams for liveness probe and launcher operations without changing production semantics.
2. Characterize healthy, dead, stalled, and cancelled cases under explicit deadlines.
3. Assert concurrent callers observe exactly one launch and cancellation leaves the state retryable without an orphan browser process.

**Result (2026-07-13): single-flight/cancellation PASS; stalled-phase boundedness RED then focused fix PASS.** The mutex already guaranteed exactly one launch and cancellation released it without caching partial state, but lock wait, liveness, and the full attach/launch path lacked LazyBrowser-level deadlines. A private injected state-machine seam now preserves the mutex design while bounding those phases at 40/3/30 seconds. Eight deterministic runtime cases, a real chromiumoxide fake-executable PID cancellation test, the full local gate, independent review, and GitHub's macOS/Ubuntu lanes pass. See `docs/specs/2026-07-13-ocean-browser-single-flight-characterization.md`.

##### 0B-4. Agent-loop history cost

1. Add a reproducible benchmark for 10/100/1,000-message histories across 1/5/20 rounds.
2. Record command, toolchain, machine metadata, warm-up/sample policy, allocations/bytes where practical, wall time, and threshold for meaningful regression.

**Baseline (2026-07-13): complete.** A release-only, dependency-free example measures the real trim/JSON-estimation/provider-validity/clone kernel over all nine matrix cells, with a process-global allocation counter, five warm-ups, thirty samples, raw distributions, machine/toolchain metadata, and a 20% + 10µs timing review threshold. The largest median is 9.316 ms for 1,000 messages × 20 rounds, with 29.4 MB cumulative allocation traffic; no performance redesign is justified by wall time alone. See `docs/specs/2026-07-13-ocean-agent-loop-history-cost-benchmark.md`.

##### 0B-5. Strict lint inventory

Store the exact command from §3.1, toolchain, raw output, feature/target exclusions, and machine-readable counts without enabling blanket `unwrap_used`/`expect_used` denial.

**Inventory (2026-07-13): complete.** Default-feature library/binary/example targets report 16 `unwrap_used`, 57 `expect_used`, 0 `panic`, 6 `unreachable`, and 0 `await_holding_lock` diagnostics (79 total). Exact output, all source sites, scope, toolchain/machine metadata, and counting rules are retained in `docs/specs/2026-07-13-ocean-strict-lint-inventory.md` and its JSON/raw artifacts. The sites remain an invariant inventory, not a bug count; no blanket denial is enabled.

**Gate:** each checkpoint records observed pass/failure honestly. A red regression proving a bug lands with its corresponding fix in the same safety PR; no ignored or nondeterministic test lands without an explicit tracked disposition.

### Phase 1 — Automated guardrails and proven safety fixes

#### 1A. Agent-facing automation

Define one machine-readable repository command manifest, then make both `xtask` and CI consume it for repository-owned commands. GitHub Actions remains responsible for runner/tool installation and the OS matrix.

Add discoverable `xtask` commands:

- `cargo xtask docs-check`
  - workspace/index parity;
  - indexed contract paths exist;
  - active local Markdown file targets resolve for inline and reference-style links;
  - active docs do not depend on `.agentarchive` unless explicitly allowlisted;
  - every non-default workspace member has a rationale and explicit check.
- `cargo xtask ci --dry-run`
  - prints the portable commands applicable to the current host;
  - separately lists omitted host-specific and CI-only matrix/setup lanes.
- `cargo xtask ci`
  - runs the portable local merge gate from the shared manifest;
  - reports platform-dependent omissions rather than claiming full CI equivalence.

Add `docs-check` to CI after its own tests pass. Add parity tests so the workflow and command manifest cannot silently diverge.

**Completion (2026-07-12): PASS.** Dependency-free `xtask` modules now validate the 25-package canonical index, non-default rationale, active repo-local Markdown file targets (inline and reference-style), and archive boundaries; heading fragments remain an explicit manual check. `cargo xtask ci` owns the executable gate manifest and reports CI-only setup/matrix lanes. GitHub Actions consumes that manifest on macOS and Ubuntu, with `cargo-deny` retained as a separate Ubuntu job. `cargo test -p xtask` and the full `cargo xtask ci` gate pass.

#### 1B. Safety fixes proven by Phase 0B

Implement only findings demonstrated by tests:

1. **Complete:** the checked event byte/lifetime policy proved replay retention risk, so the daemon now enforces both 2,048-event and 32-MiB serialized-payload replay ceilings while preserving full live delivery. Focused/full gates and security review passed. Runtime-channel/live-payload redesign and artifact-backed large results remain deferred.
2. **Complete:** descendant Halt failed while direct-child Halt passed, so Unix Bash commands now run in a child-owned process group killed by an RAII guard on cancellation/timeout; macOS and Ubuntu gates pass.
3. **Complete:** characterization preserved the correct mutex single-flight pattern and cancellation semantics, but proved missing stall bounds; lock wait, liveness, and launch now have explicit deadlines without weakening exactly-one-launch behavior, and macOS/Ubuntu gates pass.
4. **Build compatibility (2026-07-13): complete.** The exact Rust 1.80 check failed before Ocean compilation because current ACP and other resolved dependencies require newer Cargo/Rust; downgrading that graph was broader risk than correcting the false contract. The workspace now declares/enforces Rust 1.88, with one behavior-equivalent session path comparison fix. Dependency-free xtask lanes cover strict stable Clippy for daemon `livekit-tap` and `deepgram-stt`, release-profile workspace all-target compilation, and default plus supported-feature compilation under pinned Rust 1.88. Fresh-target local runs completed in about 4m19s per lane; corrected hosted run `29231934039` passed macOS, Ubuntu, pinned Rust 1.88, and `cargo-deny`, with the slowest job under nine minutes. See `docs/specs/2026-07-13-ocean-build-compatibility-characterization.md`.

**Gate:** focused regressions; `cargo test -p ocean-runtime`; `cargo test -p ocean-daemon`; `cargo check --workspace --tests`; supported feature checks; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --all -- --check`; and a fresh security-focused review of process/payload behavior.

### Phase 2 — Behavior-neutral structural extraction

Every item below is a separate, reviewable move. No feature changes or opportunistic fixes.

Before each move, write a short extraction manifest naming exact symbols/files, inbound and outbound dependencies, expected visibility changes (normally none), relevant route/middleware or public-path snapshots, focused tests, explicit exclusions, rollback commit, and the reviewer. If the manifest exposes a required design decision, stop and move that work to Phase 3.

#### 2A. `ocean-agent` intact module moves

1. **Complete:** moved the embedded `system_prompt` module to `src/system_prompt.rs` intact; focused/full tests and independent review passed. Manifest: `docs/specs/2026-07-12-ocean-agent-system-prompt-extraction-manifest.md`.
2. **Complete:** moved the embedded session module to `src/session/mod.rs` intact; focused/full tests, the full repository gate, and independent review passed. Manifest: `docs/specs/2026-07-12-ocean-agent-session-extraction-manifest.md`.
3. Preserve all `ocean_agent::...` public paths through re-exports.
4. Stop after the intact moves. Splitting session internals changes the internal dependency/privacy graph and requires separate Phase 3 approval.

**Critical invariants:** session serde compatibility, atomic save order, deterministic duplicate healing, strict resume/create behavior, workspace rebinding, same-session lock scope spanning load→run→save, raw message/image retention.

**Gate:** `cargo test -p ocean-agent`, targeted session/project-prompt tests, workspace check, fmt.

#### 2B. `ocean-tui` legacy and mesh isolation

1. Leave the active `src/shell/` component/Elm architecture unchanged.
2. Move the retained legacy daemon surface as one unit under `src/legacy/`.
3. Move mesh command/state/ingestion/rendering under `src/mesh/`.
4. Reduce `main.rs` to CLI parsing, project-root resolution, and dispatch.
5. Stop after whole-surface isolation. Fine-grained legacy reducer/stream/render splits require separate Phase 3 approval.

**Critical invariants:** enum variants are additive, default route remains `shell::run`, Elm dispatch remains the active shell's only mutation path, legacy SSE replay remains exactly once, agent mirrors do not double-render, cursor remains a UTF-8 byte offset, all rendered external text remains terminal-safe.

**Gate:** `cargo test -p ocean-tui`; `cargo build -p ocean-tui --release`; workspace check/tests when shared enums are touched; fmt. Before the move starts, the extraction manifest must name or add a checked PTY/script harness for default, legacy, and mesh dispatch with explicit expected exit/frame conditions—“manual smoke” alone is not a completion gate.

#### 2C. `ocean-daemon` composition and leaf extraction

1. **Complete (2026-07-14):** established a reusable internal router seam and checked the full explicit method/path, discovery-banner, operator-guide, merge, fallback, CORS, implicit-HEAD, and static/dynamic room-precedence contracts. Characterization found and corrected four pre-existing banner omissions and thirteen operator-guide omissions without changing the mounted route graph. Manifest: `docs/specs/2026-07-14-ocean-daemon-router-parity-extraction-manifest.md`.
2. Move leaf concerns first:
   - **Complete (2026-07-14):** moved turn counters, cumulative latency histogram rendering, Prometheus text generation, the cancellation-safe in-flight RAII guard, and four focused tests intact into private `src/metrics.rs`; the thin state-extracting HTTP handler remains in composition. Manifest: `docs/specs/2026-07-14-ocean-daemon-metrics-extraction-manifest.md`.
   - **Complete (2026-07-14):** moved CORS origin parsing, trust policy, method/header policy, layer construction, and focused tests intact into private `src/cors.rs`; the full-router middleware contract remains unchanged. Manifest: `docs/specs/2026-07-14-ocean-daemon-cors-extraction-manifest.md`.
   - **Complete (2026-07-14):** moved the exhaustive SDK→legacy-core event mirror and SDK SSE event-name adapters intact into private `src/event_adapter.rs`, adding three focused characterization tests while leaving publication, envelope provenance, runtime relay, filtering, replay, and framing in composition. Manifest: `docs/specs/2026-07-14-ocean-daemon-event-adapters-extraction-manifest.md`.
   - **Complete (2026-07-14):** moved the pure lexical traversal, caller-cwd pass-through, and session-detail workspace-scope policy plus nine existing tests intact into private `src/workspace_policy.rs`; startup repo-cwd enforcement, persisted-session lookup, query resolution, HTTP mapping, room/call fallbacks, runtime rebinding, and persistence remain in composition. Manifest: `docs/specs/2026-07-14-ocean-daemon-workspace-policy-extraction-manifest.md`.
   - voice wrappers;
   - **Complete (2026-07-14):** characterized the model get/list/set HTTP contracts, then moved the four catalog/selection adapter symbols intact into private `src/model_catalog.rs`; canonical provider routing, readiness, ordering, credential discovery, and persistence remain owner-controlled, while `/ready`, Longhouse filtering, roles, turn overrides, and YOLO policy stay in composition. Manifest: `docs/specs/2026-07-14-ocean-daemon-model-catalog-extraction-manifest.md`.
   - **Complete (2026-07-15):** characterized missing/malformed/whole-config-invalid fail-open loading, verbatim aliases, exact lookup, blank values, and explicit-model/role/advisor precedence, then moved the three immutable model-role loading/resolution helpers into private `src/model_roles.rs`; `AppState`, startup/caller order, warnings, provider routing/readiness, persisted selection, and advisor execution remain in composition or their established owners. Manifest: `docs/specs/2026-07-15-ocean-daemon-model-roles-extraction-manifest.md`.
   - **Complete (2026-07-14):** characterized exact settings responses, persistence timing, env masking, safe precedence, and the inert request wire flag, then moved all seven YOLO preference/effective-policy and GET/POST adapter symbols intact into private `src/yolo_settings.rs`; router/call sites, permission and decision-token authority, voice fail-fast behavior, shared environment locks/order, and parent tests remain in composition. Manifest: `docs/specs/2026-07-14-ocean-daemon-yolo-settings-extraction-manifest.md`.
   - **Complete (2026-07-14), filesystem half:** characterized symlink escapes, canonical HOME boundaries, status/error envelopes, and existing content/list contracts, then moved the complete home-sandboxed directory/file HTTP policy intact into private `src/filesystem.rs`; router/query parsing and parent tests remain in composition. Manifest: `docs/specs/2026-07-14-ocean-daemon-filesystem-extraction-manifest.md`;
   - **Complete (2026-07-14), project half:** characterized list enrichment/fallbacks and complete create/get/patch/delete response, persistence, timestamp, workspace-session, and delete-retention contracts, then moved all ten project-registry HTTP adapter symbols intact into private `src/project_registry.rs`; `ocean-agent` remains the persistence/pagination/session authority and turn cwd/project integration stays in composition. Manifest: `docs/specs/2026-07-14-ocean-daemon-project-registry-extraction-manifest.md`;
   - **Complete (2026-07-14):** aligned with the extension program, characterized all-op daemon/runtime key parity plus coupled TTL/cap lifecycle behavior, introduced a checked synchronous GC seam, then moved all ten Slack Canvas host-fulfillment symbols intact into private `src/slack_canvas_fulfillment.rs`; state assembly, route mounting, generic scheduling, and the initial pending runtime-event relay remain in composition, while Socket Mode, Slack API/credentials, reconnect, replies, files, and real Canvas delivery remain extension-owned. Manifest: `docs/specs/2026-07-14-ocean-daemon-canvas-bridge-extraction-manifest.md`.
   - **Complete (2026-07-15):** characterized all five response branches and the one-shot lifecycle, then moved the exact state-free `POST /v1/component/event` fulfillment adapter into private `src/component_interaction.rs`; parent composition retains route/banner/operator-guide ownership, while `ocean-runtime` retains the wait registry, permission/session binding, registration, timeout, and ordinary post-await cleanup. Manifest: `docs/specs/2026-07-15-ocean-daemon-component-interaction-extraction-manifest.md`.
   - **Published (2026-07-15):** characterized request/permission status-only snapshots, identity/token/timestamp initialization, live duplicate replacement, missing-handle detachment, matching and mismatched waiter consumption, exact permission and finish transitions, handle ownership, terminal helpers, GC, and shutdown behavior; then moved two private aliases, two records, four terminal-helper methods, and seven bounded free functions into `src/request_control.rs`. `AppState`, permission policy/orchestration, decision-token verification, HTTP/event mapping, active-turn projection, GC scheduling, task draining, and all parent characterization tests remain in composition. Dedicated-target gates, fresh reviews, hosted CI, and PR #286 merge `ee3860a` passed; live daemon supervision/deployment is delegated to the concurrent operator workstream. Manifest: `docs/specs/2026-07-15-ocean-daemon-request-control-extraction-manifest.md`.
   - **Published (2026-07-15):** characterized first-threshold ownership, duplicate-voter idempotence, zero-threshold clamping, carried latching, poison recovery, exact recall HTTP responses, persisted title revocation, and successful-only cleanup; then moved the private tally handle, constructor, lock helper, cast, and named removal into the 52-line private `src/recall_registry.rs`. UUID/live-title validation, persisted title/Revoker authority, carried execution, HTTP mapping, and cleanup call ordering remain in composition. All 338 daemon tests passed serialized; Longhouse recall/escrow, workspace-test compilation, both supported daemon feature checks, formatting, docs, Rust 1.88 MSRV, and hosted CI passed; two fresh security/concurrency reviews found no unresolved medium-or-higher issue; PR #287 merged as `3e051c1`. Manifest: `docs/specs/2026-07-15-ocean-daemon-recall-registry-extraction-manifest.md`.
   - **Published (2026-07-15):** characterized exact real-router room lifecycle/error envelopes, serde defaults, persisted author/event/audit/spawn order, no-false-trigger footprints, closed-room audit asymmetry and bounded replay, shared-handle poison recovery, and static/dynamic route precedence; then moved the shared store alias/lock adapters, nine durable-room handlers, paging helper, and room-agent auto-convene path into private `src/persistent_rooms.rs` (887 current lines after a comment-only owner clarification). `AppState`, startup open/migration, `room_routes()`, call persistence/retries, and LiveKit authorization remain in composition over the same store. All 348 daemon and 38 store tests, workspace test compilation, both supported feature checks, compatibility, Rust 1.88 MSRV, the canonical local CI gate, formatting/docs/diff checks, two fresh extraction reviews, hosted macOS/Ubuntu/MSRV/cargo-deny CI, and PR #293 merge `92e03bf` passed with no unresolved medium-or-higher issue. Manifest: `docs/specs/2026-07-15-ocean-daemon-persistent-rooms-extraction-manifest.md`.
   - **Published (2026-07-15):** characterized exact real-router extractor/method/default behavior, complete prepare/inspect/workflow envelope and PR #292 evidence shapes, privacy/cwd confinement, and all three blocking/cache/fail-open/no-authority lanes; then moved the exact 334-line state-free boundary into the 349-line private `src/longhouse_preparation.rs` owner (including module header/imports and required parent visibility). Route composition, turn-time prompt preparation, librarian query/fetch, compatibility subagent-spec, governance/title/escrow/recall state, and all `ocean-longhouse` algorithms remain in their current owners. After PR #295 reconciliation, focused tests, all 348 daemon tests, 118 Longhouse tests plus one host-dependent ignore, workspace test compilation, both supported features, compatibility, Rust 1.88 MSRV, canonical local CI, formatting/docs/diff checks, two fresh extraction reviews, hosted macOS/Ubuntu/MSRV/cargo-deny CI, and PR #296 merge `29d65f8` passed with no unresolved medium-or-higher issue. Librarian extraction remains deferred behind the separately disclosed cached-path symlink-retarget security disposition. Manifest: `docs/specs/2026-07-15-ocean-daemon-longhouse-preparation-extraction-manifest.md`.
   - **Manifesting (2026-07-15):** proposed a separate private `src/longhouse_turn_preparation.rs` owner for only the fresh default-on opt-out gate, deterministic advisory rendering/application, and cached read-only `TurnPrep` selection inside the existing blocking task and 250 ms deadline. The ordinary prompt, asynchronous request, and agent-turn call sites remain in `main.rs` with their exact caller-cwd, permit, acknowledgement, event, runtime, and prompt-layer ordering. Exact presentation, byte-preserving no-op, environment truth table, blocking/fail-open structure, and all three call-site positions require committed characterization and independent review before the five-symbol move. HTTP adapters, librarian/spec, governance, calls, and broader turn/SSE orchestration remain excluded. Manifest: `docs/specs/2026-07-15-ocean-daemon-longhouse-turn-preparation-extraction-manifest.md`.
3. Move remaining state registries/control plane, Longhouse governance, and calls one domain at a time.
4. Move the main agent-turn/SSE orchestration last.
5. Keep the crate binary-only initially. Do not introduce service traits, substates, or a public library merely to move code.

**Critical invariants:** route/method/middleware parity, health path, caller cwd, permission gates, session ownership, bounded replay semantics, event IDs/order, persistence paths, title tokens, room mention behavior, call feature gates.

**Gate:** before extraction, add a checked method/path plus nesting/fallback/middleware snapshot and a route/banner parity command. Then run targeted route tests, `cargo test -p ocean-daemon`, default/`livekit-tap`/`deepgram-stt` compile checks, `cargo check --workspace`, fmt, and independent review.

### Phase 3 — Architectural improvements after extraction

Requires a new explicit approval because these are design changes rather than mechanical moves.

Candidates:

- split the intact `ocean-agent` session module into model, paths, store, list/detail, and GC modules;
- migrate `ocean-cli` and any remaining clients from the legacy API/event/session rail, add deprecation telemetry, then remove the legacy routes and event bus as one compatibility migration;
- retire the TUI `--legacy`/`OCEAN_TUI_LEGACY` path only after explicit feature-parity and usage/deprecation evidence; then remove the crate-wide dead-code allowance;
- split isolated TUI legacy reducers, streams, rendering, and formatting into narrower modules only if retirement is not yet approved;
- define one canonical room identity/projection contract before attempting to unify durable rooms, Track-0 snapshots, and mutable canvas projections;
- split the universal system prompt into a compact base identity plus capability/surface profiles, with benchmark evidence and prompt-contract tests;
- split `AppState` into domain substates;
- unify duplicate legacy/product turn paths;
- move sync SQLite work behind a dedicated executor;
- generate route metadata instead of duplicating route/banner declarations;
- create a daemon library for external HTTP contract tests;
- redesign browser launch state;
- establish artifact-backed large tool results;
- introduce selective workspace lints after the current inventory is reviewed.

No candidate is automatically approved by this plan.

### Phase 4 — Measured performance work

Use the existing harness benchmark plus focused microbenchmarks.

Priority questions:

1. How much time/allocation is spent serializing and trimming history per provider round?
2. Which clones in `agent_loop.rs` and provider encoders are large enough to matter?
3. What is the memory/latency effect of large tool outputs and slow clients?
4. Does release-profile compilation expose different failures or meaningful binary/runtime changes?
5. Can context caching or retained-size metadata reduce repeated work without weakening correctness?

Every performance change includes before/after results and rollback criteria.

## 6. Change-impact verification matrix

| Change area | Read first | Required validation |
|---|---|---|
| Shared request/event/session serde | `ocean-core`, SDK, daemon, TUI/ACP contracts | Owning tests plus `cargo check --workspace --tests` |
| Agent session persistence | `ocean-agent`, daemon contract | `cargo test -p ocean-agent`, daemon tests, workspace gate |
| Tools/permissions/cwd/cancellation | runtime, agent, daemon contracts | Runtime E2E, permission/cancellation tests, daemon tests, workspace gate |
| Model catalog/routing | providers, protocol, agent, TUI guidance | `cargo test -p ocean-providers`, protocol tests, workspace tests |
| Provider wire/streaming | provider module and retry contract | Focused fixtures, `cargo test -p ocean-protocol` |
| HTTP/SSE routes | daemon plus SDK/core clients | Narrow route tests, daemon tests, client compile/tests |
| TUI enums/render/event flow | TUI contract and shared event owner | TUI tests/release build; workspace tests for shared enums |
| Persistence schema | owning store/memory/context/Longhouse contract | Migration/backward-compat and restart persistence tests |

## 7. Stop conditions

Stop the current wave and request a decision if:

- the required extraction manifest is missing, incomplete, or reveals a design decision;
- a mechanical move requires a wire, serde, session-layout, error-contract, route, permission, or cwd semantic change;
- visibility must become public outside the crate merely to resolve a move;
- a new trait, crate dependency, daemon library, or state architecture is required;
- tests reveal current behavior conflicts with documentation;
- the baseline for touched behavior is already red and cannot be proven pre-existing;
- route order, middleware, event order, lock lifetime, cancellation semantics, or persistence ordering changes unexpectedly;
- a performance change lacks a reproducible baseline;
- concurrent work overlaps the target files;
- reviewer findings remain unresolved.

## 8. Review and commit policy

- One concern per commit; one domain per extraction.
- Prefer move-heavy diffs and inspect with moved-code highlighting.
- Avoid unrelated formatting or renaming during extraction.
- Stage only owned files; never `git add -A`.
- Re-read the applicable `AGENTS.md` chain and target files immediately before editing.
- A fresh reviewer checks correctness, tests, and simplicity before each wave closes.
- Update the nearest devlog contracts and append `events.md` after every meaningful change.

## 9. First implementation checkpoints

After this plan is approved, start with independently reviewable changes:

1. **Ground-truth docs PR — complete:** repo boundaries, handoff, gates, canonical crate index, active links, and before/after cold-agent benchmark are reconciled.
2. **Docs automation PR — complete:** `cargo xtask docs-check`, one executable CI manifest, manifest unit coverage, and GitHub Actions consumption are implemented and passing.
3. **Intact `ocean-agent` extraction wave — complete:** both private modules moved with behavior/tests preserved and independent review passed.
4. **Event-policy characterization/fix — complete:** checked event table, isolated payload/RSS stress, smallest replay-byte retention fix, full gate, and security review passed.
5. **Shell Halt characterization/fix — complete:** direct/descendant PID tests and Unix process-group cleanup pass on macOS and Ubuntu.
6. **Browser characterization/fix — complete:** injected healthy/dead/stalled/cancelled single-flight tests, bounded phases, and real launch-cancellation PID coverage pass on macOS and Ubuntu.
7. **Agent-loop benchmark — complete:** clean 30-sample baseline, independent methodology review, and macOS/Ubuntu repository gates passed.
8. **Strict lint inventory — complete:** exact raw diagnostics and machine-readable 79-site inventory retained without blanket denial; independent reproduction and macOS/Ubuntu gates passed.
9. **Build compatibility — complete:** Rust 1.88 is the truthful enforced floor; supported daemon features, release all-target compilation, and pinned-MSRV compilation pass locally and in the hosted matrix.

The approved foundation program is complete through build compatibility. Ground-truth docs, executable automation, intact extractions, targeted reliability fixes, retained performance/lint evidence, and truthful supported-build lanes are all closed with independent and hosted validation. Further event/daemon/runtime moves remain blocked on their applicable characterization and safety disposition rather than being implied by this plan's completion.

## 10. Decisions requested

Approval of this plan means:

- preserve behavior and public contracts during structural work;
- prioritize agent ground truth and proven safety risks before module aesthetics;
- begin with the independent checkpoints above, allowing unrelated read-only/characterization lanes to proceed without bundling them;
- require an extraction manifest before every mechanical move;
- defer session-internal/TUI-legacy fine splits, `AppState` redesign, service traits, new public libraries, artifact-backed large-result design, and performance rewrites to separately approved phases.
