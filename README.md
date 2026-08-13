# Ocean OS

Ocean OS is the local Rust runtime for Ocean. A long-running daemon owns agent
turns, provider calls, permission-gated tools, sessions, and event delivery.
The terminal UI, CLI, ACP bridge, and sibling product surfaces are clients of
that daemon; they are not separate agent runtimes.

## What this repository owns

```text
clients (TUI / CLI / ACP / Ocean Surface)
  -> ocean-daemon (:4780; HTTP + SSE authority)
  -> ocean-agent (sessions, prompts, capabilities)
  -> ocean-runtime (agent loop, tools, permissions, cancellation)
  -> ocean-protocol + ocean-providers (provider wire + model/auth routing)
```

The canonical package, entry-point, and narrow-test index for all workspace
members is [`crates/AGENTS.md`](crates/AGENTS.md). The cross-repository boundary
is [`docs/OCEAN_PROJECT_MAP.md`](docs/OCEAN_PROJECT_MAP.md).

Ocean OS does **not** own product UI chrome, agent-package content, or the shared
Bedrock data plane:

- [`ocean-surface`](https://github.com/Risingtides-dev/ocean-surface) owns the
  Leptos UI and its browser, extension, and Tauri hosts.
- [`ocean-agents`](https://github.com/Risingtides-dev/ocean-agents) owns editable
  assistant profiles, specialist packages, and couriers.
- `ocean-bedrock` (private) owns the authenticated shared files, ledger,
  ingest, graph, and semantic-search plane. It is an optional team knowledge
  service; Ocean OS runs fully without it.

## Quick start

Prerequisites: Rust 1.88 or newer and a configured model/provider. See
[`docs/OPERATIONS.md`](docs/OPERATIONS.md) for credentials, supported build
lanes, supervision, and recovery.

```bash
# Builds missing release binaries, starts the daemon when needed, and opens the TUI.
./ocean
```

The launcher preserves the caller's working directory for project/session binding while the TUI starts the daemon from a neutral directory. Pass TUI arguments through normally (`./ocean --help`). For direct binary and CLI workflows, see [`docs/OPERATIONS.md`](docs/OPERATIONS.md).

For the supervised macOS service, use `ops/install-ocean-daemon.sh` rather than hand-copying a daemon binary. The script enforces the `main` branch; operators must separately verify a clean tree and `HEAD == origin/main` as documented in `docs/OPERATIONS.md`.

## Runtime contracts

- The daemon is the execution and HTTP/SSE authority.
- Product clients create or select a session, subscribe to that session's
  events, and submit turns carrying `session_id`, `cwd`, and `client_type`.
- Session persistence lives in `ocean-agent`; a session defect affects every
  first-party client.
- Mutating tools remain permission-gated unless the operator explicitly selects
  a trusted bypass.
- The daemon must run from a neutral cwd.
- Durable collaboration uses `/v1/rooms/persistent/*`; the retired Track-0 room
  projection is not an active client contract.

## Documentation

Start at [`docs/README.md`](docs/README.md).

| Need | Canonical document |
| --- | --- |
| Current architecture and state ownership | [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) |
| Build, run, verify, deploy, recover | [`docs/OPERATIONS.md`](docs/OPERATIONS.md) |
| Detailed runtime/API operator reference | [`docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md`](docs/OCEAN_RUNTIME_OPERATOR_GUIDE.md) |
| Four-repository routing and contracts | [`docs/OCEAN_PROJECT_MAP.md`](docs/OCEAN_PROJECT_MAP.md) |
| Package ownership and narrow tests | [`crates/AGENTS.md`](crates/AGENTS.md) |
| External host adapters, including Herdr | [`integrations/AGENTS.md`](integrations/AGENTS.md) |
| Open work only | [`ROADMAP.md`](ROADMAP.md) |
| Historical chronology | [`events.md`](events.md) |

Plans and characterization reports under `docs/specs/` are evidence, not the
current architecture unless a current contract explicitly says otherwise.
Completed or superseded context belongs under `docs/.agentarchive/`.

## Development gate

```bash
cargo xtask docs-check       # docs/index/archive integrity
cargo xtask ci               # canonical local merge gate
cargo xtask ci --dry-run     # print the executable manifest
```

Compatibility lanes also run supported daemon features and release builds on
stable Rust and the default/supported feature paths on pinned Rust 1.88. See
[`AGENTS.md`](AGENTS.md) and [`xtask/README.md`](xtask/README.md).

## License, trademarks, and credits

Ocean OS code is available under **MIT OR Apache-2.0**, at your option.
Project-authored documentation and non-brand assets use the same posture unless
marked otherwise. Ocean names and logos are excluded from the open-source
grants; reasonable nominative use is described in the trademark policy.

- [`LICENSE`](LICENSE) — scope and license choice
- [`TRADEMARKS.md`](TRADEMARKS.md) — names, logos, and nominative use
- [`CREDITS.md`](CREDITS.md) — human acknowledgements
- [`NOTICE.md`](NOTICE.md) — legal third-party notices and donor provenance

## Contributing

Read [`AGENTS.md`](AGENTS.md) before editing and then follow every nearer
`AGENTS.md` on the path to the target file. See
[`.github/CONTRIBUTING.md`](.github/CONTRIBUTING.md) for the contribution flow
and [`NOTICE.md`](NOTICE.md) for third-party attributions.
