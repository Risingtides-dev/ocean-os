# Ocean Project Map

Status: active cross-repository ownership and routing map.

This map identifies repository boundaries. Read the target repository's
`AGENTS.md` chain before editing.

## Visibility and role

| Repository | Visibility | Role |
| --- | --- | --- |
| [`ocean-os`](https://github.com/Risingtides-dev/ocean-os) | public | Rust runtime, daemon, tools, providers, sessions, permissions, TUI/CLI/ACP, and generic coordination seams |
| [`ocean-surface`](https://github.com/Risingtides-dev/ocean-surface) | public | Thin client surfaces: Leptos/WASM UI, Tauri and editor hosts, browser/PWA, voice and canvas presentation |
| [`ocean-agents`](https://github.com/Risingtides-dev/ocean-agents) | public | Organization-neutral surface profiles, package conventions, generic transport, and reference packages |
| `risingtides-agents` | private | Rising Tides production assistants, couriers, Slack intake, SOPs, internal workflows, and deployment behavior |
| `ocean-bedrock` | private, optional | Authenticated shared knowledge/data plane, context, handoffs, ledger, graph/search, workflows, and team records |

Public Ocean use must not require either private repository.

## First routing rule

| Request concerns | Start in |
| --- | --- |
| Runtime, daemon API, tools, permissions, models/providers, sessions, events, TUI, ACP, MCP client | `../../ocean-os` |
| Web/native/editor/browser/mobile/voice/canvas client behavior | `../../ocean-surface` |
| Generic surface profile, package schema, Bonzai, manifest router, file-courier, reusable transport | `../../ocean-agents` |
| Rising Tides content, campaign, artist, distribution, email, Slack app, production courier, factory SOP, internal workflow | `../../risingtides-agents` (authorized checkout) |
| Shared team knowledge, context, handoff, ledger, semantic search, graph, workflow record, Bedrock API | `../../ocean-bedrock` (authorized checkout) |

## System shape

```text
client surfaces
  ocean-surface / TUI / ACP
            |
            | sessions, turns, SSE, permissions
            v
      ocean-os daemon :4780
            |
            | loads reusable profile/package data
            v
    public ocean-agents

Authorized Rising Tides deployment only:

  risingtides-agents  ---> production behavior and SOPs
            |
            +--------> optional authenticated ocean-bedrock context/data
```

## Ownership contracts

### `ocean-os`

Owns execution authority: agent loop, provider/model routing, permission gates,
tools, cancellation, sessions, queues, events, local memory primitives, and the
daemon API. It does not own organization-specific agents or product UI chrome.

### `ocean-surface`

Owns presentation and client interaction. Surfaces select sessions, send turns,
subscribe to daemon events, and render state. They do not call providers
directly or become session/tool authority.

### Public `ocean-agents`

Owns reusable package data and examples. Public files must remain usable without
Rising Tides accounts, knowledge, destinations, campaigns, media, private SOPs,
or Bedrock. Packages request capabilities; they cannot grant permissions.

### Private `risingtides-agents`

Owns deployed Rising Tides agent behavior: named production assistants,
automation couriers, Slack intake, campaign/artist workflows, internal skills,
and factory/orchestrator references. It may extend public packages and request
optional Bedrock context through normal runtime capability and permission seams.

### Private `ocean-bedrock`

Owns authenticated shared records and knowledge services for authorized team
deployments. It is not a local execution shell and does not replace daemon
permission, session, provider, or tool authority.

## Shared invariants

- `Project -> Workspace -> Session -> Turns -> Events` is daemon-side state.
- A surface explicitly creates or selects a session before submitting a turn.
- `client_type` identifies the medium, not a session or workspace.
- Public packages contain no credentials, destinations, private data, or
  organization-specific operating procedures.
- Private package behavior remains separate from Bedrock data/storage authority.
- Local side effects always route through Ocean runtime permission gates.
- Subagent definitions and orchestration policy remain extension-owned; core
  exposes only generic execution, cancellation, capability-provider, and event
  seams.

## Live defaults

| Service | Default | Owner |
| --- | --- | --- |
| Ocean daemon | `127.0.0.1:4780` | `ocean-os` |
| Standalone Longhouse direction | `127.0.0.1:4781` | `ocean-os` / extension boundary |
| Surface proxy/dev app | `0.0.0.0:8790` | `ocean-surface` |
| Bedrock service | deployment-defined; local default `:8080` | private `ocean-bedrock` |

Daemon health is `GET /health`. Provider credentials resolve only inside
`ocean-os`.

## Source anchors

Public:

- `../../ocean-os/AGENTS.md`
- `../../ocean-os/docs/ARCHITECTURE.md`
- `../../ocean-surface/AGENTS.md`
- `../../ocean-agents/AGENTS.md`
- `../../ocean-agents/docs/AGENT_FILESYSTEM_ARCHITECTURE.md`

Authorized private maintainers may additionally inspect:

- `../../risingtides-agents/AGENTS.md`
- `../../risingtides-agents/docs/PUBLIC_PRIVATE_REPOSITORY_SEPARATION_2026-07-20.md`
- `../../ocean-bedrock/AGENTS.md`
- `../../ocean-bedrock/docs/API.md`

When private repositories are unavailable, route from public contracts and
state the limitation instead of inventing private behavior.
