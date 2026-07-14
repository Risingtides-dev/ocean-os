# Ocean OS bootstrap

**Read [`AGENTS.md`](AGENTS.md) first.** It is the binding cross-harness work
contract. This file is only a compact orientation pointer.

## Current system boundary

Ocean is four connected repositories with separate authorities:

- `ocean-os` — local runtime, daemon, providers, tools, permissions, sessions,
  events, TUI, CLI, ACP, and local coordination primitives;
- `ocean-surface` — the Leptos product UI and its browser, extension, proxy, and
  Tauri shells;
- `ocean-agents` — editable profiles, named specialist packages, and couriers;
- `ocean-bedrock` — authenticated shared files, ledger, ingest, graph, and
  semantic-search services.

Use [`docs/OCEAN_PROJECT_MAP.md`](docs/OCEAN_PROJECT_MAP.md) before making a
cross-repository claim.

## Runtime authority

```text
client -> ocean-daemon -> ocean-agent -> ocean-runtime -> provider/tool
                     \-> session-scoped SSE -> client
```

The daemon owns execution. Clients carry intent and render state; they do not
own provider calls, permissions, or session persistence. The product session
flow is:

```text
POST /v1/agent/sessions
GET  /v1/agent/events?session_id=<id>
POST /v1/agent/turns
```

## Navigation

- Documentation index: [`docs/README.md`](docs/README.md)
- Architecture: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- Operations: [`docs/OPERATIONS.md`](docs/OPERATIONS.md)
- Package index: [`crates/AGENTS.md`](crates/AGENTS.md)
- Open work: [`ROADMAP.md`](ROADMAP.md)
- Chronology: [`events.md`](events.md)

Historical plans and handoffs are not current authority. Inspect source, Git
state, and live health directly when the distinction matters.
