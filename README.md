# Ocean OS
<img width="1536" height="1024" alt="image" src="https://github.com/user-attachments/assets/4bf6221b-7b77-4303-9268-3ba2be698cd9" />

> Rust-native coding-agent runtime, daemon, and TUI cockpit.

Ocean OS is an agentic operating system written in Rust. A long-running daemon owns the agent loop, tool execution, provider calls, sessions, and permissions. Clients (CLI, TUI, future GUI / web / voice) are thin shells that steer the daemon over a stable protocol.

## What's in this repo

| Crate | Role |
|---|---|
| `ocean-core` | Shared protocol types: requests, responses, events, sessions |
| `ocean-protocol` | Unified multi-provider LLM wire protocol (Anthropic, OpenAI, Google Gemini, OpenAI-compatible). SSE streaming, retry, cancellation |
| `ocean-runtime` | Agent loop with permission-gated tool execution. Built-in tools: read, write, edit, bash, ls, grep, glob, web_fetch, todo |
| `ocean-mcp` | Ocean as an MCP **client**: connects to external Model Context Protocol servers and exposes their tools to the agent through the runtime |
| `ocean-acp` | ACP (Agent Client Protocol) bridge — exposes the daemon to Zed and other ACP editors over stdio |
| `ocean-providers` | Ocean-owned provider registry: model routing, credential resolution, readiness checks |
| `ocean-longhouse` | Quorum engine + convening flow behind the longhouse deck (multi-agent council) |
| `ocean-heartbeat` | Scheduleable Ocean routines: prompt-injection scheduler hooks for daemon routines (`ocean-heartbeat` binary) |
| `ocean-agent` | Ocean session/history layer wrapping `ocean-runtime` |
| `ocean-agent-sdk` | SDK surface for embedding the agent in other Rust code |
| `ocean-daemon` | Long-running HTTP service on `:4780`. Owns runtime authority |
| `ocean-cli` (`ocean-rs` binary) | CLI client: health, prompt, sessions |
| `ocean-tui` (`ocean` binary) | Terminal Agent surface |
| `ocean-browser` | Typed async handle to a Chrome instance driven over the DevTools Protocol |
| `ocean-call` | Ocean Call Intelligence: daemon-side PSTN call agent (Twilio SIP → LiveKit room) that Ocean joins as a server-side participant |

## Quick start

## Product framing

- `ocean-rs` is the canonical Rust-native coding-agent harness/runtime.
- `ocean-daemon` owns runtime authority: provider calls, agent loops, tools, sessions, permissions, and events.
- `ocean-tui` is the Agent terminal coding surface
- Ocean GUI and service layers remain thin clients until the daemon protocol is stable. see risingtides-dev/ocean-surface repo

Run the daemon:

```bash
# Build
cargo build --workspace --release

# Configure provider credentials (any one of these is enough)
export OCEAN_DEEPSEEK_API_KEY=...   # DeepSeek (default)
export OCEAN_ANTHROPIC_API_KEY=...  # Anthropic
export OCEAN_OPENAI_API_KEY=...     # OpenAI

# Run the daemon
./target/release/ocean-daemon

# In another shell:
curl http://127.0.0.1:4780/health
./target/release/ocean-rs prompt "Reply with: pong"

# Launch the TUI cockpit
./target/release/ocean-tui   # or: ocean   (if symlinked into ~/.local/bin)
```

## Provider configuration

Model selection via `OCEAN_MODEL` — there is no hardcoded default model; with nothing set the daemon errors (`NoModelSelected`) rather than picking one for you (see [`docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md`](docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md#model-selection)). Supported model strings include:

- `deepseek-chat`, `deepseek-reasoner`, `deepseek-v4-flash`, `deepseek-v4-pro`
- `gpt-4o`, `gpt-4o-mini`
- `claude-sonnet-5`, `claude-opus-4-8`, `claude-haiku-4-5` (+ `claude-code-*` subscription variants incl. `claude-code-fable-5`)
- `fake` (no creds — for testing)
- any OpenAI-compatible base via `OCEAN_PROVIDER=openai-compatible` + `OCEAN_BASE_URL`

Provider env-var lookup order is documented in [`crates/ocean-providers/src/lib.rs`](crates/ocean-providers/src/lib.rs).

## Architecture

Cross-repo routing and ownership map: [`docs/OCEAN_PROJECT_MAP.md`](docs/OCEAN_PROJECT_MAP.md).

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the daemon ↔ client model, and [`docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md`](docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md) for runtime ops.

TUI design: [`docs/OCEAN_TUI_MOCKUPS.md`](docs/OCEAN_TUI_MOCKUPS.md), [`docs/OCEAN_TUI_TIDES_MESH_PARITY.md`](docs/OCEAN_TUI_TIDES_MESH_PARITY.md).

Internals: [`docs/OCEAN_NATIVE_INTERNALS_MAP.md`](docs/OCEAN_NATIVE_INTERNALS_MAP.md).

## Roadmap

Active runtime roadmap: [`ROADMAP.md`](ROADMAP.md).

Longer-horizon vision — Ocean OS as a shared agentic knowledge layer (PRDs, market research, ingestion architecture, MCP service design) — lives on the [`roadmap/ocean-os-v2`](https://github.com/Risingtides-dev/ocean-os/tree/roadmap/ocean-os-v2) branch. That branch preserves the v2 product work and the TypeScript ingestion/orchestrator/MCP scaffolding; cherry-pick from it as those pieces graduate into the runtime.

## Third-party attributions

See [`NOTICE.md`](NOTICE.md).

## Contributing

See [`.github/CONTRIBUTING.md`](.github/CONTRIBUTING.md). For repo routing and Linear team rules, see [`docs/linear-teams-routing.md`](docs/linear-teams-routing.md) and [`docs/AGENTS.md`](docs/AGENTS.md).
