# Ocean project map

Status: canonical current-state ownership and connection map for the Ocean
repositories. Local source and each target repository's `AGENTS.md` remain the
final authority for implementation work.

Documentation policy: [`OCEAN_DOCUMENTATION_CONTRACT.md`](OCEAN_DOCUMENTATION_CONTRACT.md).

## Route work by authority

| Work concerns | Start in | Authority |
| --- | --- | --- |
| daemon, agent loop, providers, models, tools, permissions, sessions, events, TUI, CLI, ACP, MCP client, local browser/call/room execution | [`ocean-os`](https://github.com/Risingtides-dev/ocean-os) | Rust runtime and local execution plane |
| web/PWA UI, Chrome extension, Tauri shell, proxy, product interaction, visual design, responsive/mobile presentation | [`ocean-surface`](https://github.com/Risingtides-dev/ocean-surface) | Thin product surfaces over the daemon |
| assistant profiles, surface prompts, named specialist packages, couriers, package SOPs and harness declarations | [`ocean-agents`](https://github.com/Risingtides-dev/ocean-agents) | Provider-agnostic behavior/package layer |
| authenticated shared files, mounts, ledger, ingest, source registry, graph, semantic search, shared context, Bedrock API/MCP | [`ocean-bedrock`](https://github.com/Risingtides-dev/ocean-bedrock) | Shared knowledge and data plane |

Read the target repository's `AGENTS.md` before editing. Do not infer ownership
from a similarly named historical document in another repository.

## Implemented system shape

```text
operator
  |
  v
ocean-surface / ocean-tui / ocean-cli / ocean-acp
  |
  | session, cwd/project intent, client_type, prompt
  v
ocean-os daemon :4780
  |- ocean-agent: prompt assembly, capabilities, sessions/history
  |- ocean-runtime: provider rounds, tools, permissions, cancellation
  |- ocean-protocol/providers: model wire, credentials, readiness
  `- local stores: rooms, memory, Longhouse titles/coordination
  |
  `-- session-scoped events/results --> clients

ocean-agents
  `-- editable surface profiles + specialist/courier packages consumed by turns

ocean-bedrock :8080 (default)
  `-- authenticated shared files + ledger + ingest/search/graph/API services
```

Ocean Agents and Bedrock are not alternate execution runtimes. Ocean Agents
declares behavior and deterministic/agentic package entry points. Bedrock owns
shared knowledge and records. Local machine effects still execute through Ocean
OS permissions and cwd/tool policy.

## Repository boundaries

### ocean-os

Owns:

- local daemon HTTP/SSE authority on `127.0.0.1:4780` by default;
- provider/model routing and wire protocols;
- agent turns, tools, permissions, cancellation, cwd binding, and local sessions;
- TUI, CLI, ACP bridge, MCP client, browser control, call pipeline;
- durable local rooms and local coordination/memory primitives.

Does not own product UI chrome, agent-package content, or Bedrock's shared cloud
storage and ingest service.

### ocean-surface

Owns:

- one Leptos/WASM product UI;
- browser/PWA hosting through `ocean-surface-proxy`;
- Chrome extension and Tauri hosts around the same UI bundle;
- product interaction, responsive behavior, host-capability seam, and design
  system;
- a retained, soft-deprecated GPUI crate for reference only.

Does not own provider calls, tool execution, permission policy, or session
persistence. The proxy may relay requests but must not become another agent
runtime or credential owner.

### ocean-agents

Owns:

- composed surface profiles loaded by Ocean OS from
  `assistants/<SURFACE>/system.md`;
- named assistant packages, their protocols/SOPs, and thin harness declarations;
- courier manifests and the slash-command router;
- package-side authoring and composition checks.

Does not own runtime enforcement, model credentials, or daemon session state.
An external engine referenced by a package remains in its own repository; Ocean
Agents stores the package identity and integration contract, not a vendored copy.

### ocean-bedrock

Owns:

- bearer-authenticated shared filesystem and mount routing;
- scoped roles, tokens/invites, locks, and audit history;
- Ocean Ledger and context/semantic/graph services;
- source adapters, sync-run lineage, ingest workers, and MCP/API access;
- shared collaboration artifacts and data-plane persistence.

Does not own the user's local shell/filesystem authority, provider routing, or
product session state.

## Cross-repository contracts

### Surface to runtime

The first-party product path is daemon-owned and session-scoped:

```text
POST /v1/agent/sessions
GET  /v1/agent/events?session_id=<id>
POST /v1/agent/turns
```

A surface renders daemon state and sends intent. It must not adopt a session
from an unrelated global stream or invent a second transcript authority.

### Profiles and packages to runtime

Ocean OS resolves a turn's `client_type` to a surface flag and prefers the
published profile under the configured assistants root. Named folder-as-agent
turns use the daemon's agent-folder contract. Agentic couriers submit prompts to
the daemon; deterministic couriers execute their declared harness directly.

The daemon currently loads one published profile file per surface. Ocean Agents
performs `_shared` + `_base/<SURFACE>` composition before publication; runtime
composition is not implemented.

### Runtime and Bedrock

Ocean OS may read or write shared context through Bedrock API/MCP contracts, but
Bedrock does not execute local tools. The local and shared halves of Longhouse
remain distinct: `ocean-longhouse` provides local coordination logic;
Ocean Bedrock provides shared data-plane services.

### Surface and Bedrock

Surface features should normally use daemon-owned product contracts or a thin
proxy. Direct Bedrock access must remain authenticated and must not transfer
shared-storage or federation authority into browser state.

## Default endpoints

| Service | Default | Repository |
| --- | --- | --- |
| Ocean daemon | `http://127.0.0.1:4780` | `ocean-os` |
| Ocean Surface local/prod proxy | `http://127.0.0.1:8790` | `ocean-surface` |
| Ocean Bedrock | `http://127.0.0.1:8080` for local use (`0.0.0.0:8080` bind by default) | `ocean-bedrock` |
| Ocean Agents | no standalone core service | `ocean-agents` |

Deployment URLs and process identifiers are operational state and belong in
the owning repository's current runbook, not this architecture map.

## Known current mismatch

`ocean-surface` emits `client_type = "surface-tauri"` inside its Tauri host, but
`ocean-agent::surface_flag` does not currently map that string. Those turns use
the generic fallback prompt instead of a dedicated or GUI profile. This is a
code-level integration gap tracked in [`../ROADMAP.md`](../ROADMAP.md), not a
documentation convention to paper over.

## Source anchors

- Ocean OS: [`../AGENTS.md`](../AGENTS.md), [`../crates/AGENTS.md`](../crates/AGENTS.md),
  [`ARCHITECTURE.md`](ARCHITECTURE.md)
- Ocean Surface: `ocean-surface/AGENTS.md`, `Cargo.toml`,
  `crates/ocean-surface-ui/src/daemon.rs`, `ops/README.md`
- Ocean Agents: `ocean-agents/AGENTS.md`, `assistants/README.md`,
  `couriers/hub/router.py`, each `courier.toml`
- Ocean Bedrock: `ocean-bedrock/AGENTS.md`, `package.json`, `src/server.mjs`,
  `docs/openapi.yaml`

When a connection contract changes, update this canonical map first. Sibling
repositories keep only a local boundary summary plus a link here; they do not
maintain another full copy.
