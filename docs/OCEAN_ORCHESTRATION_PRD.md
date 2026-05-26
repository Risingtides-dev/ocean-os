# Ocean Orchestration PRD

## Mission

Use Pi as the orchestration layer to build Ocean into a local-first Rust-native agentic distro. The canonical runtime is `ocean-rs`: a Rust-native Pi-style coding-agent harness/runtime. The first serious product surface is Ocean TUI: an active steering cockpit and Rust-native Tides Mesh MeshFloor over that harness. Ocean OS GUI follows as a thin native client; distro integration comes after the runtime/client protocol is stable.

## Non-goals

- Do not create another runtime in `/home/ocean-os`.
- Do not make Ocean TUI own provider calls, tools, sessions, or agent loops.
- Do not frame Ocean TUI as a passive daemon dashboard; it is the active operator cockpit over daemon-owned authority.
- Do not build on Electron, Tauri, or browser-first product assumptions.
- Do not hide daemon state behind GUI-only behavior.

## Current state

`ocean-rs` exists at:

```text
/home/smathdaddy/code/rust/ocean-rs
```

Current crates:

```text
ocean-core
ocean-agent
ocean-daemon
ocean-cli
```

Current daemon API:

```text
GET  /health
POST /v1/prompt
GET  /v1/sessions
```

The prompt path now runs in-process through `crates/ocean-agent` and reports:

```text
backend=ocean-native-deepseek
```

## Product direction

### Phase 1: Runtime + TUI steering cockpit

Create a TUI cockpit that steers `ocean-rs`, exposes the daemon honestly, and brings the Tides Mesh MeshFloor into the main product surface.

Deliverables:

- `crates/ocean-tui`
- health status panel
- prompt composer
- transcript panel
- sessions panel
- event/activity rail
- request/session indicators
- cancel affordance
- permission prompt surface once daemon supports it
- MeshFloor panels for board/events/inbox/agents
- live floor context for Glyph audit, KNOX review, Charlotte research, Orchestrator routing, BRICK runtime, PIXEL UI, WritersRoom/Henry, and Rev review

MeshFloor references that belong to the main TUI/current-context docs:

- [`docs/OCEAN_TUI_TMUX_LAYOUT_MAP.md`](OCEAN_TUI_TMUX_LAYOUT_MAP.md)
- [`docs/OCEAN_TUI_TIDES_MESH_PARITY.md`](OCEAN_TUI_TIDES_MESH_PARITY.md)

### Phase 2: Streaming daemon protocol

Add protocol features required by the TUI and later GUI.

Deliverables:

- request IDs
- session IDs everywhere
- event broadcaster
- SSE endpoint
- structured assistant delta events
- structured tool started/output/finished events
- structured permission request events
- cancellation endpoint
- session inspection endpoint

### Phase 3: GUI as thin client

Update `/home/ocean-os/ocean-native` to call the same daemon protocol.

Deliverables:

- daemon health integration
- prompt integration
- sessions integration
- stream/event integration
- removal or retirement path for `/home/ocean-os/ocean-runtime` as an authority

### Phase 4: Native Ocean internals

Replace remaining Pi Rust component dependencies with Ocean-owned crates.

Deliverables:

- `ocean-providers`
- `ocean-tools`
- native agent loop ownership in `ocean-agent`
- `ocean-store` for session/event persistence

### Phase 5: Distro shape

Only after the runtime protocol is stable:

- systemd/user service hardening
- Unix socket support
- desktop notifications
- installer defaults
- sandbox profiles
- theme/client protocol
- plugin model

## Future Telegram bridge controls

Product requirement for the Telegram/Ocean bridge:

- `/reload` reloads runtime extensions/config only.
- `/reconnect` reconnects Telegram/bridge/session transports only.
- Both commands require explicit operator approval boundaries.
- Reload/reconnect must not reveal secrets or broaden auth scope.
- Do not implement these commands until a routed task authorizes the bridge work.

## Crew task seed

A planner should split work approximately this way:

1. Add request/session protocol fields in `ocean-core`.
2. Add daemon event broadcaster and SSE stream.
3. Add `ocean-tui` crate scaffold with health screen.
4. Wire TUI prompt composer to existing `/v1/prompt`.
5. Add sessions panel.
6. Convert prompt path to emit events.
7. Add cancellation protocol.
8. Add permission request/decision protocol.
9. Start GUI client alignment after TUI can steer the daemon.

## Verification

Required runtime checks:

```bash
cd /home/smathdaddy/code/rust/ocean-rs
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
ocean-rs health
ocean-rs prompt "Reply exactly: OCEAN_OK"
```

Required GUI checks if `/home/ocean-os` is touched:

```bash
cd /home/ocean-os
./scripts/check.sh
```
