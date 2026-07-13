# Ocean OS — bootstrap context

**Read `AGENTS.md` first.** The `AGENTS.md` hierarchy is the cross-harness work contract for Claude, Codex, Pi, ocean-native agents, and every other runtime. This file is only a compact bootstrap pointer and never overrides that contract.

## System boundary

Ocean is a connected four-repo system. The canonical routing and ownership map is [`docs/OCEAN_PROJECT_MAP.md`](docs/OCEAN_PROJECT_MAP.md):

- runtime, daemon, tools, permissions, providers, sessions, TUI, ACP, MCP client → `ocean-os`;
- GUI/web/PWA/browser/editor/voice/canvas surfaces → `ocean-surface`;
- assistant packages, profiles, SOPs, couriers → `ocean-agents`;
- shared files, context, handoffs, ledger, graph/search, Bedrock APIs → `ocean-bedrock`.

Do not maintain another repo inventory here. Read the target repo's `AGENTS.md` before editing it.

## Runtime authority

`ocean-daemon` is the local runtime/body. It owns provider calls, the agent loop, permission-gated tools, local sessions, and SSE events. Clients carry a `session_id` and steer the same daemon; they do not own independent session state.

```text
POST /v1/agent/turns    { prompt, cwd, session_id?, ... }
GET  /v1/agent/events   session-scoped SSE
GET  /health            daemon liveness
```

Session persistence lives in `crates/ocean-agent`. A load/save/rebind bug there affects TUI and ocean-surface together.

Longhouse is the local-first coordination hive for SOPs, routines/workflows, tool/skill discovery, memory/knowledge, subagent specs, and councils. It coordinates and recommends; it never bypasses daemon execution or permission authority. Canonical reference: [`docs/LONGHOUSE.md`](docs/LONGHOUSE.md).

## Workspace navigation

The canonical index for all 25 workspace packages is [`crates/AGENTS.md`](crates/AGENTS.md). It records ownership, exclusions, entry points, local contracts, narrow tests, non-default-member rationale, and cross-crate fanout.

Core flow:

```text
clients -> ocean-daemon -> ocean-agent -> ocean-runtime -> ocean-protocol/providers
```

## Build and run

```bash
cargo build --workspace --release
./target/release/ocean-daemon
curl -fsS http://127.0.0.1:4780/health
./target/release/ocean-tui   # or: ocean
```

The supervised daemon runs from a neutral cwd, not the repo. After TUI changes, rebuild `cargo build -p ocean-tui --release` because the global `ocean` command points at that artifact.

## Current work

- Active code-health/agent-readiness program: [`docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`](docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md)
- Architecture: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- Operator guide: [`docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md`](docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md)
- Cross-repo routing: [`docs/OCEAN_PROJECT_MAP.md`](docs/OCEAN_PROJECT_MAP.md)
- Historical chronology: `events.md`
