# Ocean Extensions Architecture and Migration Manifest

**Date:** 2026-07-14
**Type:** cross-repo architecture and staged extraction manifest
**Status:** Approved direction; Phases 0–1 accepted, Phase 2 next
**Owner:** Smaths / Ocean
**Authoring baseline:** `ocean-os` `3fe699924811`
**Current ratification:** `ocean-os` `529e0ed1de2fb9b3b04b8c44979db8e8406f1ca1`; subagent ownership decision `c5740fd428d3d23f8aea57881c8f6b0c40f4a9ee`
**Council evidence:** Longhouse topic `21df6975-1d33-4e3c-ad58-43c00a662b78`

## 1. Decision

Create `ocean-extensions` as Ocean's public package ecosystem for installable integrations and reusable extension examples.

An Ocean extension is a **distribution package**, not a new execution authority. A package may contribute several resource kinds:

- executable tool plugins;
- supervised service adapters;
- agents and nested subagents;
- skills, prompts, and surface profiles;
- lifecycle observers and, later, narrowly trusted interceptors;
- external-host metadata such as a Herdr plugin manifest.

Ocean OS remains the sole local execution and enforcement substrate for extension-requested turns, tools, processes, permissions, cwd/workspace binding, sessions, secrets, cancellation, and scoped host-event delivery. An orchestration extension owns subagent definitions, dispatch, spawn/join/retry lifecycle, parent/child graph semantics, worker budgets, recursion policy, and result aggregation. Extension ownership never permits a package to widen host grants or execute local side effects outside Ocean OS's generic permission-gated seams.

This direction deliberately copies Pi's strongest idea—one installable package can contribute multiple resource types—without copying its unrestricted in-process extension execution model or introducing a second local execution authority.

## 2. Why this is a manifest, not an implementation wave

The current system already contains several extension-like mechanisms, but they are not one contract:

- `crates/ocean-plugin` loads model-callable subprocess tools from `plugin.toml`.
- `crates/ocean-hooks` defines the subprocess runner for the live `Stop` continuation hook; it is not the public extension lifecycle-observer API.
- `crates/ocean-agent/src/agentdir.rs` discovers folder agents, skills, tool grants, subprocess capabilities, and nested `subagents/`.
- `crates/ocean-agent/src/lib.rs` discovers global plugin providers from the Ocean config directory and binds per-agent subprocess capabilities.
- `crates/ocean-daemon` owns session-scoped SSE events and process/runtime state.
- `crates/ocean-agent-sdk` contains typed external daemon vocabulary, including the current Slack Canvas contract.
- Public `ocean-agents` contains reusable package/profile data; private `risingtides-agents` contains production package data plus transitional executable bridges, including the live Slack Socket Mode and delivery path.
- `ocean-bedrock` contains source-runner adapters for knowledge ingestion, which are related to but distinct from live surface/service adapters.

No files move until the host contract, trust boundary, package schema, and parity gates below are implemented or explicitly resolved. Behavior-neutral extraction and architectural redesign must remain separate changes.

## 3. Vocabulary

Use these terms consistently in code, manifests, CLI, and documentation.

| Term | Meaning | Runtime semantics |
| --- | --- | --- |
| **Extension package** | Installable bundle and distribution unit | Indexes one or more resources; does not execute by itself |
| **Plugin** | Model-callable executable capability | Loaded through `ocean-plugin`; tools are namespaced and permission-gated |
| **Service adapter** | Long-lived bridge to an external system | Daemon-supervised process with declared events, health, and restart policy |
| **Agent package resource** | Folder-defined reasoning identity | Package data and policy are extension-owned; every turn is host-executed and permission-gated |
| **Subagent** | Nested agent definition delegated to by a parent | Delegation policy and worker lifecycle are extension-owned; every child turn remains host-executed and permission-gated |
| **Skill** | On-demand procedure or knowledge resource | Loaded as declared data; no independent execution authority |
| **Profile** | Surface-specific behavior/presentation source | Selected by client/surface context and composed by the host |
| **Observer** | Read-only lifecycle event consumer | Cannot mutate or cancel host behavior |
| **Interceptor** | Trusted hook capable of changing host behavior | Deferred; separately declared, timed, audited, and permissioned |
| **Surface** | Client medium attached to daemon sessions | Creates/selects sessions, submits turns, renders daemon state |
| **Bedrock source adapter** | External-source-to-shared-knowledge ingestion runner | Writes normalized records through Bedrock; not a live Ocean session surface |

`plugin` must not become a synonym for every optional Ocean resource. `extension` must not become a synonym for a client surface.

## 4. Repository ownership after activation

The active project map remains four repositories until `ocean-extensions` exists and passes its bootstrap gate. At that point, mirror an updated five-repo map across all Ocean repositories.

### 4.1 `ocean-os` — host and authority

Owns:

- extension manifest schema and compatibility validation;
- package discovery, installation records, enable/disable state, and local trust/activation enforcement;
- executable and service supervision;
- lifecycle event vocabulary and delivery;
- permission, cwd, session, model, provider, and secret authority;
- capability registration and namespacing;
- generic permission-gated turn execution, cancellation/process cleanup, scoped events, package/actor audit identity, opaque correlation seams, and operator-wide resource/rate ceilings;
- SDK contracts and conformance fixtures used by third parties.

Does not own:

- catalog source for individual integrations;
- provider- or company-specific agent content;
- external service credentials in manifests;
- a mandatory cloud registry.

### 4.2 `ocean-extensions` — public extension catalog and examples

Owns:

- first-party installable integrations such as `ocean-herdr` and `ocean-slack`;
- reference tool, observer, service, agent-pack, and extension-owned orchestration examples;
- package templates and author guidance;
- cross-language SDK helpers built on host protocols;
- package-level conformance fixtures.

Does not own:

- daemon runtime or permission enforcement;
- local package trust decisions;
- Rising Tides company agent identities and private operational knowledge;
- first-party Ocean application chrome;
- shared cloud knowledge storage.

Third-party packages, including orchestration extensions, may live in any Git repository or local directory. `ocean-extensions` is a distribution catalog and reference owner, not a global runtime authority; inclusion there is not required for installation.

### 4.3 `risingtides-agents` — Rising Tides deployments

Owns:

- Rising Tides agent identities and deployment packages;
- company-specific instructions, SOPs, skills, profiles, and context declarations;
- fixtures proving those deployments bind correctly to Ocean runtime contracts.

Generic transports and live external-system harnesses should move out only after a replacement extension reaches verified parity. For Slack, the generic bridge moves while `content-agent` remains here and depends on the generic Slack extension.

### 4.4 `ocean-surface` — first-party Ocean clients

Owns first-party web/PWA, Tauri desktop/mobile, browser/editor, voice, canvas, and shared rendering products that steer the daemon.

A bridge hosted inside another product is normally a service adapter, not automatically an `ocean-surface` product. Herdr and generic Slack integration therefore begin in `ocean-extensions`.

### 4.5 `ocean-bedrock` — shared knowledge and optional registry metadata

May own:

- searchable package metadata, documentation, provenance, digests, signatures, compatibility, and audit status;
- extension-produced shared context and durable collaboration artifacts;
- source adapters that ingest external systems into shared knowledge.

Must not own:

- local package execution or enablement;
- local project trust;
- raw extension secrets;
- daemon sessions or local execution enforcement, extension orchestration state by default, or local permission decisions.

Direct path and Git installation must remain possible without Bedrock. A Bedrock registry is an optional discovery and provenance plane, never the only route to participation.

## 5. Package layout

A package may contain any supported subset of resources:

```text
my-ocean-extension/
├── ocean-extension.toml
├── README.md
├── plugins/
│   └── <tool-plugin>/plugin.toml
├── services/
│   └── <adapter>/
├── agents/
│   └── <agent>/
│       ├── agent.toml
│       ├── instructions.md
│       ├── skills/
│       └── subagents/
├── skills/
├── profiles/
├── prompts/
├── hooks/
├── themes/
├── external/
└── tests/
```

The outer manifest indexes package resources. Existing specialized inner contracts remain specialized:

- `ocean-extension.toml` — package identity, compatibility, resources, and requested capabilities;
- `plugin.toml` — one executable tool provider and its stdio JSON-RPC launch contract;
- `agent.toml` — one folder agent's model/tool/capability policy;
- `SKILL.md` or its eventual typed successor — one skill;
- external manifests such as `herdr-plugin.toml` — metadata consumed by the external host.

## 6. Draft `ocean-extension.toml` contract

Accepted schema-v1 parsing plus filesystem-independent metadata validation and non-executing canonical package validation live in `ocean-extension`. Install automation remains deferred to Phase 3.

```toml
schema_version = 1
id = "risingtides.ocean-herdr"
name = "Ocean for Herdr"
version = "0.1.0"
description = "Run Ocean as a managed Herdr agent."
license = "MIT"
min_ocean_version = "0.8.0"

[package]
homepage = "https://github.com/risingtides-dev/ocean-extensions"
source = "https://github.com/risingtides-dev/ocean-extensions"

[trust]
# Informational declaration checked by the host; never grants itself trust.
project_local = false

[[services]]
id = "lifecycle"
entry = "services/lifecycle/ocean-herdr"
args = []
events = [
  "session_started",
  "turn_started",
  "permission_requested",
  "permission_resolved",
  "turn_finished",
  "session_stopped",
]
restart = "on-failure"

[services.health]
kind = "process"
startup_timeout_ms = 5000

[services.capabilities]
network = []
filesystem = []
env = ["HERDR_ENV", "HERDR_PANE_ID", "HERDR_BIN_PATH"]
secrets = []

[[external]]
kind = "herdr"
manifest = "external/herdr-plugin.toml"
```

A mixed agent/tool package would use the same outer contract:

```toml
schema_version = 1
id = "example.research-suite"
name = "Research Suite"
version = "1.0.0"
min_ocean_version = "0.8.0"

[[plugins]]
id = "source-tools"
path = "plugins/source-tools"

[[agents]]
id = "researcher"
path = "agents/researcher"

[[skills]]
id = "citation-check"
path = "skills/citation-check"

[[profiles]]
surface = "TUI"
path = "profiles/TUI"
```

### 6.1 Required schema rules

- `schema_version`, `id`, `name`, `version`, and `min_ocean_version` are required.
- `id` is globally stable and reverse-domain-like; installed identity never comes from a mutable display name.
- `version` and compatibility ranges use real SemVer parsing, not lexical comparison.
- Every resource path is relative to the package root, canonicalized, and forbidden from escaping it.
- Duplicate resource IDs are invalid.
- Declared resources do not imply activation; the host records enablement separately.
- Requested capabilities are review input, not self-granted permissions.
- Raw credentials are forbidden. Manifests name environment variables or host-resolvable secret references only.
- Schema-v1 secret references use `<scheme>:<key>`. The scheme is lowercase ASCII alphanumeric with hyphens only between alphanumeric characters. The nonempty key permits only ASCII alphanumeric characters plus `_`, `-`, `.`, and `/`; whitespace/control characters, `=`, URL `://` forms, absolute paths, and parent traversal are invalid. This syntax keeps raw credentials outside the declared contract but cannot prevent a malicious publisher from mislabeling a value.
- Schema-v1 discriminator values fail closed: external host kind `herdr`, service health kind `process`, and optional restart policy `on-failure` are the currently supported values.
- Unknown required resource kinds fail closed. Unknown optional metadata may be retained and ignored according to the schema-version policy.
- Installation never runs package code. Build steps, if introduced later, require a separate explicit trust action.

## 7. Trust and security contract

The existing plugin lane is not a sandbox merely because each advertised model tool requires permission. Phase 0 evidence showed that legacy plugin subprocesses launched as the local user, inherited the daemon environment, and could act before a tool call was approved. Accepted Phase 1 supplies an explicit minimal child environment and canonical real cwd/PWD, separates installed/trusted/enabled state, and exposes static no-execution inspect/doctor reads. Process isolation, lifecycle supervision, and package-management mutations remain later-phase work.

Broad third-party installation is blocked until the following host protections exist:

1. **Install and activation are separate.** Downloading or copying a package does not execute it.
2. **Scope is explicit.** Installation and trust are user-global; enablement may additionally be scoped only to a registered Ocean `ProjectId` in daemon-owned user config.
3. **Trust is operator-owned.** A repository, project, session, or package manifest cannot grant trust to itself; project enablement cannot override a missing or revoked artifact trust grant.
4. **Environment is allowlisted.** Child processes do not inherit the daemon's complete environment by default.
5. **Secrets use references.** Resolution happens in the host and only declared secret values enter the child environment.
6. **Capabilities are inspectable.** Network domains, filesystem roots, events, executables, secrets, and service lifetime are visible before activation.
7. **Observers and interceptors differ.** Read-only observation cannot silently gain mutation/cancellation authority.
8. **Processes are bounded.** Startup timeout, message size, restart rate, shutdown, health, cwd, and output behavior are host-enforced.
9. **Audit state is local.** Source, digest, version, grants, enablement, and last health are inspectable without a cloud service.
10. **Permission gates remain central.** Extension-contributed model tools use the daemon's existing policy and cannot shadow built-ins.
11. **Subagent labels never widen grants.** An orchestration extension cannot widen its activation grants, mint permission decisions, bypass cwd/session binding, or acquire unscoped lifecycle payloads by labeling a request as a subagent. The host intersects every requested capability with activation grants and operator policy.
12. **Generic host ceilings are defense in depth.** Ocean OS may enforce operator/package-wide concurrency, rate, and cost ceilings, but these do not make worker topology or worker-budget policy core-owned.

WASM/WASI should become the preferred least-authority lane for portable tools, but native supervised services remain necessary for integrations such as Slack Socket Mode and Herdr process coordination.

## 8. Host event lifecycle contract

### 8.1 Initial observer events

Phase 2 exposes versioned, read-only host events. This vocabulary and its scoped delivery are daemon-owned; worker spawn/join/retry/result lifecycle remains orchestration-extension-owned:

```text
daemon_started
session_started
turn_started
permission_requested
permission_resolved
tool_started
tool_finished
turn_finished
session_stopped
daemon_stopping
```

Each envelope must include only the identifiers needed for attribution and must follow the daemon's existing payload-size policy. Raw prompts, full transcripts, tool arguments/results, credentials, and arbitrary environment data are excluded by default.

### 8.2 Deferred interceptors

The following are explicitly deferred until observer delivery, trust, ordering, timeout, and audit behavior are proven:

```text
before_turn
before_tool_call
before_session_switch
context_transform
provider_request_transform
```

No implementation wave may introduce Pi-style unrestricted context or provider-request mutation as an incidental part of Herdr or Slack work.

### 8.3 Existing `ocean-hooks`

`ocean-hooks` is evidence and reusable machinery, not yet the public extension lifecycle API. Before reuse it needs:

- event vocabulary expansion;
- live execution wiring;
- observer/interceptor separation;
- versioned envelopes;
- bounded execution and failure policy;
- explicit environment and cwd rules;
- tests proving hook failure cannot corrupt a turn or session.

## 9. Service supervision contract

A service adapter is long-lived and not model-invoked. The daemon host must own:

- exact executable and canonical package cwd;
- minimal environment injection;
- startup timeout and health state;
- bounded restart policy with backoff;
- process-group shutdown and daemon-exit cleanup;
- event subscription scope;
- log routing without secret exposure;
- enable/disable state;
- package/version attribution in diagnostics.

A service may call documented daemon HTTP/SSE APIs. It may not open or mutate daemon session files directly.

## 10. Agent and subagent packaging contract

Extensions may package complete folder agents and nested subagents, reusing the current `agentdir` data shape. Packaging does not grant independent agent-loop execution, local side-effect authority, or additional host capabilities.

An installed orchestration extension owns and tests:

- subagent roles, definitions, objectives, and prompts;
- parent/child worker graphs and spawn/join/retry semantics;
- depth, count, concurrency, cycle, and recursion policy;
- model, skill, and tool selection within host grants;
- worker token, cost, and turn budgets;
- result aggregation and orchestration memory namespaces;
- extension-specific status and lifecycle events.

Ocean OS owns and tests the generic execution and enforcement seam used by that extension:

- validate the installed, trusted, and enabled package caller;
- intersect requested capabilities with activation grants and operator policy;
- execute ordinary turns and enforce permissions, cwd/workspace binding, secrets, and session integrity;
- expose generic cancellation and scoped event/tool transport, and clean up host-owned processes;
- apply generic operator/package resource ceilings and auditable package/actor identity;
- carry opaque correlation identifiers when required without interpreting them as a core named-worker graph.

The extension decides which workers belong to an orchestration job and when to request cancellation; the host owns the cancellation primitive and local process cleanup. The future orchestration transport, persistence model, and correlation contract require separate design ratification.

The current `/v1/subagents/spec`, `AgentDef.subagents`, and filesystem `subagents/` discovery are compatibility surfaces pending a separately approved migration. They describe or parse metadata and do not prove or authorize a core scheduler.

## 11. First reference extensions

### 11.1 `ocean-herdr`

Purpose: prove direct installation, external-host metadata, lifecycle observation, and first-class agent status without forking Herdr or embedding Herdr-specific code across `ocean-tui`.

Expected resources:

```text
extensions/ocean-herdr/
├── ocean-extension.toml
├── README.md
├── external/herdr-plugin.toml
├── services/lifecycle/
├── bin/launch-ocean
└── tests/
```

Parity target:

- launch Ocean from a Herdr action/pane;
- register the pane as agent `ocean`;
- map turn start to `working`;
- map permission wait to `blocked`;
- map permission resolution back to `working`;
- map settled turn to `idle` or Herdr's equivalent completion state;
- fail soft when not running inside Herdr;
- require no Herdr marketplace listing.

### 11.2 `ocean-slack`

Purpose: move the generic live Slack transport/bridge out of deployment-specific agent content after a Rust or otherwise supported extension reaches parity.

Candidate source inventory:

- `risingtides-agents/assistants/bridge/`;
- `risingtides-agents/couriers/transport/slack.py`;
- generic Slack base profiles and bridge fixtures where ownership review confirms they are not content-agent-specific.

Retained in `risingtides-agents`:

- `assistants/content-agent/` identity, instructions, company SOPs, tool grants, and knowledge declarations;
- agent-specific Slack overrides;
- dependency declaration on `risingtides.ocean-slack`.

Parity gate:

- Socket Mode reconnect and acknowledgement;
- mention, DM, and slash-command intake;
- deterministic thread-to-session identity;
- daemon turn submission and scoped SSE consumption;
- reply, file, and Canvas delivery;
- no raw Slack tokens in manifests, logs, events, or Bedrock;
- live fixture equivalence before deleting the Python bridge.

The existing typed Slack Canvas protocol remains in place during parity extraction. Generalizing it is a separate redesign.

## 12. Installation and CLI direction

The host should ultimately support:

```text
ocean extension install <path|git-url>
ocean extension remove <id>
ocean extension list
ocean extension inspect <id>
ocean extension enable <id>
ocean extension disable <id>
ocean extension doctor <id>
```

Required behavior:

- local path and Git URL work before any registry exists;
- installs are content-addressed or record a verified source revision/digest;
- inspect shows resources, requested capabilities, compatibility, source, digest, scope, grants, and health;
- enabling executable resources is an explicit trust transition;
- updates do not silently broaden grants;
- removal refuses or stages cleanup when active services/sessions depend on the package;
- no package code runs during list, inspect, or disabled discovery.

### 12.1 Daemon-owned local state decision

Phase 0 fixes the local state layout under the exact `ocean_agent::config_dir_from_env()` result:

```text
<config_dir>/extensions/
├── installs.json
├── trust.json
├── enabled.json
├── store/<extension-id>/<version-or-digest>/
└── .state.lock
```

- `installs.json` is the user-global inventory and records immutable payload provenance and digests; installation does not imply trust or enablement.
- `trust.json` records separate user-global operator grants bound to the reviewed artifact identity/digest. A changed digest is untrusted until explicitly granted.
- `enabled.json` records a global activation default and optional overrides keyed only by a registered Ocean `ProjectId`. Project-specific enablement lives in daemon-owned user config, never repository-local files or session state. An unregistered workspace receives only global policy; no path-derived authorization identity is minted.
- Effective activation requires installed, trusted, and enabled state. A project override cannot supply or widen trust.
- `store/` contains immutable, verified payloads published atomically; `.state.lock` serializes daemon-owned mutations and coherent reads. The CLI/TUI remain daemon clients rather than independent registry writers.
- Existing `<config_dir>/plugins` and `OCEAN_PLUGINS_DIR` sources remain `legacy-unmanaged` compatibility sources. Phase 1 does not auto-adopt, copy, trust, or enable them; adoption and eventual cutoff require explicit operator action and a separate migration.
- The three JSON documents use strict schema v1 arrays and the same nonzero `state_revision`; partial, malformed, oversized, duplicate, unknown-field/version, or revision-mismatched state fails closed. A wholly absent `extensions/` directory is the only empty-state case and read paths never create it.
- Installed artifacts use `sha256:<64 lowercase hex>` identities. `sha256-tree-v1` hashes a sorted descriptor-anchored inventory: UTF-8 relative entry paths, executable bits, byte lengths, domain-separated file-content hashes, and explicit trailing-slash directory records, including empty directories. Symlinks, hardlinks, special files, path replacement, non-UTF-8 entries, in-read mutations, more than 10,000 entries, depth over 64, or payloads over 256 MiB fail closed. The frozen known-answer test owns exact encoding.
- Every untrusted state/store component is opened descriptor-relatively with no-follow semantics; `.state.lock` is held across the three-file snapshot, package digest, and manifest inventory validation. Lock wait is bounded to 250 ms and static artifact inspection to four concurrent blocking tasks.
- Provenance is either a lexically canonical absolute local path (no `.`/`..`, repeated separators, or trailing-separator aliases) or a credential-free HTTPS/SSH Git locator pinned to a 40- or 64-character lowercase hexadecimal revision. Trust grants bind the exact artifact digest and cannot exceed the package's requested capabilities.
- `GET /v1/extensions/{id}/inspect` and `GET /v1/extensions/{id}/doctor` (plus `ocean-rs extension inspect|doctor`) are daemon-owned reads. Optional project selection accepts only a registered `ProjectId`. Responses expose resources, requested/effective grants, compatibility, source/digest, global/project enablement, and static health; doctor never launches a plugin/service/hook, runs Git/shell/provider calls, or performs a health probe.
- This state is not stored under `<config_dir>/sessions`, repository `.ocean/`, or `projects.json`, and it does not change session authority, activate packages, or promise hot loading.

## 13. Staged implementation plan

### Phase 0 — contract and characterization (accepted 2026-07-14)

1. Ratify this manifest and mirror only a pointer into sibling-repo planning docs as needed.
2. Characterize current plugin launch environment, startup timing, shutdown, cwd, and failure behavior.
3. Characterize the Phase 0 `ocean-hooks` reachability baseline, which then had no live turn invocation.
4. Snapshot global and per-agent plugin discovery behavior and namespacing.
5. Snapshot Slack bridge parity fixtures and live operational requirements.
6. Decide the local install-state path without changing session-storage authority.

Evidence recorded on 2026-07-14:

- At the Phase 0 baseline, global plugin discovery scanned immediate directories under `OCEAN_PLUGINS_DIR` or `<config_dir>/plugins` in unsorted filesystem order, launched sequentially during capability-registry assembly, and treated a live `list_tools` call as readiness. Folder-agent subprocess capabilities launched sequentially per applicable turn. Both legacy launch paths executed before model-tool permission and inherited the daemon environment and real cwd; a declared folder-agent `cwd` changed only `PWD`. The implemented Phase 1 schema/tool-lane checkpoint supersedes the environment/cwd behavior as described below, while shutdown remains implicit reference-counted stdin closure/direct-child kill without a protocol shutdown, graceful wait, or proven descendant cleanup.
- Current plugin names are flattened as `plugin__<plugin>__<tool>` without component grammar or delimiter escaping; ordered registry composition is first-wins. Built-ins remain unshadowable, but duplicate plugin identities, separator ambiguity, manifest/live-tool mismatch, and filesystem-dependent global collision winners remain characterized migration debt.
- At the Phase 0 baseline, `ocean-hooks` was loaded and validated only as daemon configuration and no live turn path called `run_hooks`. Since 2026-07-16, completed production turns invoke the configured `Stop` hook chain and a block decision can run a bounded continuation turn. The hook runner remains sequential and fail-open, inherits the daemon environment, and lacks cancellation process-tree cleanup; this live interceptor is not the deferred extension lifecycle-observer API.
- Slack parity remains a cross-repository snapshot, not a completed extraction: preserve Socket Mode acknowledgement/reconnect/dedupe, stable thread-to-session identity, scoped daemon turns, threaded replies, uploads, Canvas operations/fulfillment, slash intake, token/rate-limit behavior, and content-agent-specific overrides. Direct transport/reply tests and a live Slack smoke remain missing; the live chat path currently reduces daemon output to text, so documented structured Canvas/file output parity is not yet proven.
- The daemon-owned local install-state decision is fixed in Section 12.1.
- The three-file characterization slice adds behavior-neutral coverage for plugin environment overlay, inherited real cwd, first-wire `list_tools`, folder-agent `PWD` versus real cwd, and direct hook-chain context/control flow. It deliberately records current insecure ambient behavior without treating it as a permanent contract.

**Gate status:** accepted on 2026-07-14. Documentation validation, focused plugin/hooks/agent tests, workspace test compilation, strict touched-crate Clippy, formatting, and fresh independent code/security/architecture review passed after the normal-binary test probe was removed and the evidence count was corrected. No source extraction is authorized; Phase 1 is the next implementation gate.

### Phase 1 — package schema and hardened tool lane

1. Add typed `OceanExtensionManifest` parsing in an appropriate `ocean-os` crate after dependency-direction review.
2. Add SemVer compatibility and canonical path validation.
3. Separate installed, trusted, and enabled state.
4. Make plugin child environment explicit and minimal.
5. Add extension inspect/doctor read paths before install automation.
6. Preserve existing `plugin.toml` loading compatibility during migration.

**Acceptance status (2026-07-22): accepted.** `ocean-extension` provides fail-closed schema-v1 parsing, filesystem-independent metadata validation, SemVer compatibility, canonical confined resource validation, versioned observer-event validation, and no-execution tests. The plugin and folder-agent subprocess lane uses explicit minimal environment and canonical real cwd/PWD while preserving existing `plugin.toml` loading and permission/namespacing behavior. The daemon now reads strict installed/trusted/enabled state coherently under `.state.lock`, verifies immutable artifacts through the frozen descriptor-anchored tree digest, intersects exact-digest grants, honors only registered-project overrides, and exposes static inspect/doctor routes through a thin daemon-client CLI. No install/enable mutation, lifecycle observer, service supervision, hot loading, or Crew Stage B–E code was added.

**Gate result:** malformed/path-escape/duplicate/compatibility/state/security tests, plugin E2E, touched-package suites and strict Clippy, workspace build/tests, compatibility/MSRV manifests, full `cargo xtask ci`, formatting, docs/index integrity, and fresh independent security/correctness review passed. Reviewer-found TOCTOU, FIFO blocking, unbounded enumeration, source-reflection, in-read mutation, and directory-digest gaps were repaired and delta-reviewed before acceptance.

### Phase 2 — lifecycle observers and supervised services

1. Add the versioned read-only lifecycle envelope.
2. Implement bounded subscriptions and service supervision.
3. Prove daemon/session behavior is unaffected by observer absence, timeout, crash, or restart loop.
4. Keep interceptors out of scope.

**Gate:** lifecycle ordering tests, payload policy review, crash/restart/process-tree tests, permission and secret boundary review.

### Phase 3 — local/Git package management

1. Implement install/remove/list/enable/disable, retaining and extending the accepted inspect/doctor read paths without making clients state writers.
2. Support local path and pinned Git source.
3. Record digest, source, version, scope, grants, and enablement locally.
4. Add update grant-diff confirmation.

**Gate:** untrusted project package tests, offline path install, Git revision pinning, rollback/uninstall tests, no-code-execution-on-inspect proof.

### Phase 4 — `ocean-herdr` reference package

1. Create the `ocean-extensions` repository only after Phases 1–3 establish a host it can target, unless a scaffold-only repository is explicitly approved earlier.
2. Build the Herdr package against public observer/service contracts.
3. Verify the same visible daily lifecycle as built-in Herdr agents where Herdr's public API permits it.
4. Document remaining external-host differences honestly.

**Gate:** install from path and Git, Herdr launch/status smoke, non-Herdr fail-soft behavior, no `ocean-tui` Herdr special case unless a generic client event was genuinely missing.

### Phase 5 — `ocean-slack` parity extraction

1. Write a separate exact-file extraction manifest against then-current `risingtides-agents` main.
2. Port generic bridge/transport behavior without agent redesign.
3. Keep content-agent behavior and identity in `risingtides-agents`.
4. Run old and new parity fixtures and a live Slack smoke.
5. Retire transitional Python only after replacement acceptance.

**Gate:** full Slack parity matrix, secret audit, reconnect/session/canvas tests, rollback point, fresh cross-repo review.

### Phase 6 — installable agent resources and extension-owned orchestration

1. Install agents, skills, prompts, and profiles from packages.
2. Resolve dependency and collision policy.
3. Begin with a separate protocol/design ratification that selects the generic extension transport, persistence/correlation contract, and compatibility plan; this manifest does not select them.
4. Implement orchestration as an extension over ordinary host turns, cancellation, capabilities, and scoped events. Do not add a core `task`, `spawn_worker`, fleet scheduler, or named-worker runtime.
5. Prove that extension worker policy remains within host grants and that every local turn/process/side effect remains host-enforced.

**Gate:** extension tests own graph/depth/cycle/worker-budget/retry/join/result behavior; host conformance tests own grant non-widening, permissions, cwd/session isolation, scoped events, generic cancellation/cleanup, audit identity, and package removal safety.

### Phase 7 — optional Bedrock discovery

1. Publish token-free package metadata, source, digest, compatibility, provenance, and audit status.
2. Add signature/attestation policy only after threat-model review.
3. Preserve direct path/Git installation.

**Gate:** registry outage does not break installed extensions; no raw secret enters shared metadata; local trust remains authoritative.

## 14. Exact source boundaries for future extraction

This manifest authorizes investigation, tests, and schema work. It does **not** authorize mechanical movement yet.

Candidate source boundaries that require fresh manifests at extraction time:

| Candidate | Current owner | Future owner | Required separate manifest |
| --- | --- | --- | --- |
| Herdr adapter | New work | `ocean-extensions/extensions/ocean-herdr` | Creation manifest and host-contract version |
| Slack Socket Mode bridge | `risingtides-agents/assistants/bridge/` | `ocean-extensions/extensions/ocean-slack` | Exact-file parity extraction manifest |
| Slack outbound transport | `risingtides-agents/couriers/transport/slack.py` | `ocean-slack` | Exact-file parity extraction manifest |
| Slack base profile | `ocean-agents/assistants/_base/SLACK/` | To be decided by generic-vs-agent-specific audit | Profile ownership manifest |
| Slack Canvas protocol | `ocean-os/crates/ocean-agent-sdk` plus runtime events | Retain initially | Separate protocol-generalization proposal only |
| Plugin manifest/transport | `ocean-os/crates/ocean-plugin` | Retain | Harden in place; no cross-repo move |
| Hook runner | `ocean-os/crates/ocean-hooks` | Retain | Lifecycle API design/change manifest |
| Folder agents/subagents | `ocean-os/crates/ocean-agent/src/agentdir.rs` | Core may retain parsing/package-data loading as compatibility; an extension owns delegating graph interpretation and worker lifecycle | Separate loader/install and compatibility-migration manifest |

## 15. Critical invariants

Every implementation wave preserves:

- daemon authority over sessions, provider calls, tools, permissions, persistence, and local side effects;
- existing session file location and compatibility;
- cwd/workspace binding and neutral daemon cwd requirements;
- current HTTP/SSE compatibility unless a separately versioned protocol change is approved;
- built-in tool names and first-wins collision protection;
- plugin tool permission gates;
- direct path/Git participation without registry approval;
- no raw credentials in package manifests, logs, shared metadata, or Bedrock;
- no deletion of transitional Slack behavior before live parity;
- no orchestration request may widen host activation grants, permission decisions, cwd/session binding, secrets, or event scope; extension worker policy/budgets remain extension-owned and operator-visible while generic host ceilings remain enforceable;
- fail-soft behavior for disabled, absent, unhealthy, or incompatible optional extensions.

## 16. Explicit exclusions

This program does not currently include:

- an Ocean marketplace or mandatory central listing;
- package monetization, ranking, or social approval mechanisms;
- unrestricted in-process native plugins;
- arbitrary provider-request or transcript mutation;
- moving first-party Ocean UI products out of `ocean-surface`;
- moving Rising Tides agent identities or company knowledge into the public catalog;
- treating Bedrock source runners as live session surfaces;
- rewriting Slack Canvas while extracting Slack transport;
- automatic execution of downloaded build/install scripts;
- a core named-subagent runtime, worker-graph scheduler, `task`, `spawn_worker`, or fleet scheduler;
- claiming current subagent discovery is a complete delegation runtime.

## 17. Stop conditions requiring a design decision

Pause the active wave if any of the following appears:

- a resource needs to bypass daemon permission or session authority;
- the package schema cannot represent a required integration without embedding arbitrary host code;
- package trust and effective activation cannot be determined before executable launch;
- an extension requires the daemon's unrestricted environment;
- lifecycle event delivery would expose prompts, transcript content, tool payloads, or secrets by default;
- Slack parity requires changing content-agent behavior in the same extraction;
- package installation would depend on Bedrock availability;
- an orchestration extension cannot enforce bounded topology/recursion or the host cannot enforce grant non-widening;
- a design requires core named-subagent, worker-graph, `task`, `spawn_worker`, or fleet-scheduler machinery;
- an existing public route, SSE shape, tool name, or session schema would change incidentally;
- an extraction diff mixes file movement with redesign.

## 18. Validation program

### Documentation-only closeout for this manifest

- `cargo xtask docs-check`
- manual heading, source-path, and current-behavior review
- `git diff --check`

**Phase 0 result (2026-07-14):** `cargo xtask docs-check` passed with 25 indexed packages, 103 active Markdown files, and 109 checked local links; `git diff --check` passed. The full `ocean-plugin`, `ocean-hooks`, and `ocean-agent` suites, workspace check/test compilation, strict touched-crate Clippy, and formatting passed. Fresh code/security/architecture review confirmed the ratified ownership split, source-backed evidence, process-test cleanup, and four-repo activation boundary after the normal-binary test probe was removed and the evidence count was corrected. Phase 0 is accepted.

**Phase 1 result (2026-07-22):** `cargo test -p ocean-extension`, `cargo test -p ocean-plugin`, `cargo test -p ocean-agent`, `cargo test -p ocean-cli`, and `cargo test -p ocean-daemon` passed, including descriptor-replacement, symlink, FIFO, digest known-answer, mutation-revalidation, resource-inventory, trust/enablement, registered-project, HTTP-envelope, and no-execution coverage. `cargo check --workspace --tests`, strict touched-package Clippy, `cargo fmt --all -- --check`, `cargo xtask docs-check`, compatibility/MSRV manifests, and the canonical `cargo xtask ci` gate passed. Fresh independent security and correctness delta reviews approved the repaired implementation. Phase 1 is accepted; Phase 2 is next. Crew Stage A remains open for extension-manifest Phases 2–3 (lifecycle observers, supervised services, and local/Git package management); Crew Stages B–E retain their own manifest-first gates.

### Minimum source gates for later waves

- nearest crate tests from `crates/AGENTS.md`;
- `cargo check --workspace`;
- `cargo check --workspace --tests` for shared events/contracts;
- `cargo fmt --all -- --check`;
- strict all-target Clippy for touched packages;
- relevant E2E/process/security tests;
- fresh independent review for feature, logic, security, protocol, or architecture changes;
- full `cargo xtask ci` before merge where required by the root contract.

## 19. Rollback and activation

This document is the rollback point for architectural intent: until a phase is accepted, the current four-repo ownership map and existing runtime behavior remain authoritative.

The fifth repository becomes active only when all of the following are true:

1. `ocean-extensions` exists with its own `AGENTS.md`, ledger, license, templates, and CI;
2. at least one package validates against a host-owned schema;
3. local path installation and inspect/disable behavior work without executing untrusted code;
4. the cross-repo project map is updated and mirrored across all five repositories;
5. no existing Slack or agent deployment path has been deleted merely to establish the repo.

After activation, each extraction still receives its own then-current rollback commit and exact-file manifest.

## 20. Acceptance criteria

This architecture is ready to leave planning when:

- a cold contributor can distinguish extension, plugin, service, surface, agent, and Bedrock adapter;
- a third party can build and directly install a package without catalog inclusion;
- install does not imply trust or execution;
- all executable resources declare and receive only reviewed capabilities;
- Herdr can integrate through public lifecycle/service seams rather than TUI-specific hardcoding;
- generic Slack integration can move without moving content-agent identity or changing behavior;
- extension-packaged subagents are orchestrated by an extension, while every local turn, process, and side effect remains under host-owned sessions, permissions, cwd/secrets, capability grants, cancellation, and audit controls;
- Bedrock can improve discovery/provenance without becoming local execution authority;
- every migration wave is independently testable, reviewable, reversible, and fail-soft.
