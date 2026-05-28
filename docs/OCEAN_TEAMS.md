# Ocean Teams

Pi Messenger crew should work as coordinated teams over one canonical runtime: `ocean-rs`.

## Model policy

Use the real Pi configuration provider:

```text
provider: openai-codex
planner/reviewer: gpt-5.5
worker/analyst: gpt-5.4-mini
thinking: high
```

This is configured in:

```text
/home/smathdaddy/.pi/agent/pi-messenger.json
/home/smathdaddy/code/rust/ocean-rs/.pi/messenger/crew/config.json
```

## Team topology

### 1. Runtime Protocol Team

Scope:

```text
crates/ocean-core
crates/ocean-daemon
crates/ocean-agent
future crates/ocean-store
```

Mission:

- request IDs
- session IDs
- event envelope contract
- SSE stream
- cancellation
- permission request/decision protocol
- persistence boundary

### 2. TUI Steering Team

Scope:

```text
future crates/ocean-tui
```

Mission:

- ratatui/crossterm client
- health/status panel
- prompt composer
- sessions panel
- event/activity rail
- cancellation and approval surfaces

### 3. GUI Client Team

Scope:

```text
/home/ocean-os/ocean-native
/home/ocean-os/ocean-runtime retirement path
```

Mission:

- keep GUI as a thin client
- connect GUI to daemon protocol
- remove GUI-owned runtime authority
- consume the same `ocean-core` protocol as TUI/CLI

### 4. Native Internals Team

Scope:

```text
crates/ocean-protocol
crates/ocean-runtime
crates/ocean-providers
crates/ocean-agent internals
future crates/ocean-tools
```

Mission:

- own and evolve the in-tree ocean-runtime + ocean-protocol crates
- extend the Ocean-owned provider abstraction
- factor tools into a standalone ocean-tools crate when plugin runtime lands
- preserve CLI smoke behavior across changes

### 5. Distro Integration Team

Scope:

```text
systemd user service
Unix socket
notifications
installer defaults
sandbox profiles
plugin/theme protocol
```

Mission:

- only start after runtime/TUI protocol is stable
- make Ocean feel like a local agentic distro, not just a CLI

### 6. Review / Release Team

Scope:

```text
all crates and docs
```

Mission:

- enforce one-runtime architecture
- run checks
- review task outputs
- keep handoff files current

## Coordination rule

Every task must name its team and layer in the task spec. Workers must reserve files before edits and report interface changes to overlapping teams.
