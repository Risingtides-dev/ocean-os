# Ocean Local Rust Team

Ocean needs a local Rust-heavy team on tide-net. This team builds the canonical runtime and first clients before broader distro work expands.

## Team Shape

### 1. Runtime Protocol Rust Dev

Owns:

- `crates/ocean-core`
- `crates/ocean-daemon`
- request IDs
- session IDs
- event envelopes
- SSE endpoint
- cancellation and permission protocol

Current primary tasks:

```text
task-2 daemon event broadcaster + SSE
task-6 cancellation + permission decisions
```

### 2. TUI Rust Dev

Owns:

- `crates/ocean-tui`
- ratatui/crossterm app structure
- health/status screen
- prompt composer
- sessions panel
- event rail

Current primary tasks:

```text
task-4 prompt composer + sessions
task-5 event rail
```

### 3. Native Runtime Internals Rust Dev

Owns:

- `crates/ocean-agent`
- future `crates/ocean-providers`
- future `crates/ocean-tools`
- future `crates/ocean-store`

Current primary task:

```text
task-8 follow-on extraction plan and seams
```

### 4. GUI Adapter Rust Dev

Owns:

- `/home/ocean-os/ocean-native`
- daemon-client adapter
- migration away from `/home/ocean-os/ocean-runtime` as authority

Current primary task:

```text
task-9 first GUI daemon-client slice
```

### 5. Systems/Distro Rust Dev

Owns design first, implementation later:

- service boundaries
- process lifecycle expectations
- IPC/Unix socket future
- resource policy
- sandbox profiles
- hosted-Linux distro assumptions

This role coordinates with mac-mini service-layer work and must not fork the runtime.

## Crew Configuration

Local Crew is configured for four parallel Rust workers:

```text
planner/reviewer: openai-codex/gpt-5.5
worker/analyst: openai-codex/gpt-5.4-mini
thinking: high
workers: 4
max: 6
coordination: chatty
```

Config path:

```text
/home/smathdaddy/code/rust/ocean-rs/.pi/messenger/crew/config.json
```

## Execution Rule

Start with ready tasks only:

```text
task-2 Runtime Protocol
 task-4 TUI composer/sessions
 task-9 GUI first daemon-client slice
```

Do not start distro/kernel implementation until the daemon protocol and TUI steering loop are working.

## Verification

Every Rust worker must run at minimum:

```bash
cd /home/smathdaddy/code/rust/ocean-rs
cargo fmt --all
cargo check --workspace --all-targets
```

Before marking a task done, run:

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets
```
