# Ocean Master Plan

**Owner:** Ocean-Orchestrator on tide-net  
**Date:** 2026-05-25  
**Canonical runtime:** `/home/smathdaddy/code/rust/ocean-rs`

## Executive Decision

Ocean is a local-first Rust-native agentic distro, but it must grow from one canonical Rust-native Pi-style coding-agent harness/runtime:

```text
ocean-rs daemon/runtime node
  ├─ Ocean TUI active steering cockpit + Tides Mesh MeshFloor
  ├─ Ocean OS native GUI client
  └─ distro/service integration layer
```

Do not create a second agent runtime in Ocean TUI, Ocean OS GUI, or any service-layer crate. All clients steer `ocean-rs` through stable protocol types from `ocean-core`. `ocean-tui` is not a passive daemon dashboard; it is the primary operator control surface for prompts, requests, sessions, events, approvals, cancellation, and the Rust-native Tides Mesh floor.

## Current Verified State

Verified on tide-net:

```text
ocean-rs health -> ok ocean-daemon backend=ocean-native-deepseek
cargo check --workspace --all-targets -> pass
```

Current workspace crates:

```text
crates/ocean-core      shared protocol types
crates/ocean-agent     in-process agent runtime facade
crates/ocean-daemon    canonical local daemon
crates/ocean-cli       thin CLI client
crates/ocean-tui       active TUI steering cockpit + MeshFloor scaffold
```

Current Crew task state:

```text
done: task-1 protocol type framing
done: task-3 ocean-tui health scaffold
done: task-7 GUI runtime split audit
done: task-8 native internals map
next: task-2 daemon event broadcaster + SSE
next: task-4 TUI prompt composer + sessions
next: task-9 first GUI daemon-client slice
```

## Current MeshFloor context

The Tides Mesh floor is part of the main Ocean TUI product context, not an auxiliary side document. Current layout and parity references:

- [`docs/OCEAN_TUI_TMUX_LAYOUT_MAP.md`](OCEAN_TUI_TMUX_LAYOUT_MAP.md) — live tmux floor blueprint, including Glyph upper-left audit, KNOX, Charlotte, Orchestrator, BRICK, PIXEL, WritersRoom/Henry, Rev, and ops context.
- [`docs/OCEAN_TUI_TIDES_MESH_PARITY.md`](OCEAN_TUI_TIDES_MESH_PARITY.md) — no-feature-drop contract for replacing the existing Tides Mesh operator floor with Rust-native Ocean TUI panels.

The product target is a single cockpit where daemon steering and Tides Mesh floor visibility reinforce each other while daemon/harness authority remains clear.

## Team Domains

### tide-net — Runtime / Protocol / Systems Architecture

Owns:

- canonical `ocean-rs` daemon
- `ocean-core` protocol
- `ocean-agent` runtime facade
- request/session/event model
- SSE/event streaming
- cancellation and permission protocol
- kernel/systems design notes for future distro shape

### mac-mini — Service Layer / Supervisor

Owns:

- service supervisor design
- daemon lifecycle management
- IPC contract around `ocean-rs`
- telemetry/event drain service
- process restart policy
- future local service registry

Must not own agent runtime authority.

### macbook — Integration / Docs / Federation

Owns:

- team alignment docs
- build pipeline and release notes
- federation coordination
- cross-device work queue hygiene
- syncing `TEAM_ALIGNMENT.md` into the canonical Ocean docs

## Milestone 1 — Runtime Protocol Becomes Real

Deliverables:

1. `GET /v1/events` SSE endpoint.
2. Daemon event broadcaster.
3. Events emitted for:
   - session/request created
   - user message
   - assistant delta
   - tool started/output/finished
   - permission request
   - turn finished
   - error/cancelled
4. Request IDs on active prompt/request path.
5. Session IDs consistently returned and persisted.

Primary tasks:

```text
task-2: daemon event broadcaster + SSE
task-6: cancellation + permission decision protocol
```

## Milestone 2 — TUI Steers the Runtime

Deliverables:

1. TUI health/status view.
2. Prompt composer calling daemon.
3. Sessions panel.
4. Event rail consuming SSE.
5. Cancel/permission surfaces once daemon supports them.

Primary tasks:

```text
task-4: prompt composer + sessions
task-5: event rail
```

## Milestone 3 — GUI Becomes Thin Client

Deliverables:

1. Ocean OS GUI calls daemon for health/prompt/sessions.
2. GUI removes or bypasses in-process runtime authority.
3. GUI consumes same protocol types/shape as TUI.

Primary tasks:

```text
task-9: first GUI daemon-client slice
```

## Milestone 4 — Native Ocean Internals

Deliverables:

1. `ocean-providers` replaces borrowed provider surface.
2. `ocean-tools` replaces borrowed builtin tools.
3. `ocean-store` owns session/event persistence.
4. `ocean-agent` owns the loop fully.

Order:

```text
provider seam -> tool seam -> store/event replay -> agent loop ownership
```

Rule: preserve `ocean-rs prompt "Reply exactly: OCEAN_OK"` after each extraction.

## Milestone 5 — Distro / OS Shape

This begins after the runtime protocol and TUI are usable.

Deliverables:

1. hardened systemd user service
2. optional Unix socket
3. service supervisor integration
4. desktop notification hooks
5. sandbox profiles for tools
6. installer/default config
7. native shell/desktop integration

## Kernel / Systems Track

The phrase “ocean-os” should be treated as the long-term distro/system direction, not a reason to fork the runtime today.

Systems architecture work should start as design docs and service boundaries:

```text
kernel/systems notes
process supervision model
memory/resource policy
hardware abstraction notes
Linux-hosted distro assumptions
```

Do not begin a bare-metal kernel before the hosted runtime, TUI, GUI, service supervisor, and distro integration layers are coherent.

## Immediate Command Plan

1. Tide-net starts `task-2` and `task-4` in parallel if file reservations allow.
2. Mac-mini designs the service/supervisor layer around the daemon boundary.
3. Macbook sends/syncs `TEAM_ALIGNMENT.md`, then owns docs/build/federation support.
4. Tide-net reports progress after event endpoint and TUI prompt path are verified.

## Acceptance Gates

Runtime gate:

```bash
cd /home/smathdaddy/code/rust/ocean-rs
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
ocean-rs health
ocean-rs prompt "Reply exactly: OCEAN_OK"
```

GUI gate when `/home/ocean-os` is touched:

```bash
cd /home/ocean-os
./scripts/check.sh
```
