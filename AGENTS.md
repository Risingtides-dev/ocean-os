# Ocean OS — Devlog Root

## Purpose

This is the root devlog contract for the `ocean-os` repository. Every agent entering this repo — Claude, Codex, Pi, ocean-native, or any other harness — reads this file first.

## Ownership

- **Repo:** `risingtides-dev/ocean-os`
- **Runtime:** Rust workspace, daemon on `:4780`, TUI binary `ocean`
- **Sibling repo:** `../ocean-surface` (browser PWA client, no agent logic)

## Local Contracts

- Read this file before editing anything in this repo.
- Walk from repo root to your target path and read every AGENTS.md along the route.
- Use the nearest AGENTS.md as the local contract; parent docs set repo-wide rules.
- No child doc may weaken this root contract.
- After any meaningful change, do a devlog pass: update the nearest owning AGENTS.md, refresh affected child indexes, remove stale text, and append a root `events.md` entry with `worktree:`.

## Crate Map

| Crate | Role |
|---|---|
| `ocean-core` | Shared protocol types: requests, responses, events, sessions |
| `ocean-protocol` | Multi-provider LLM wire protocol (Anthropic, OpenAI, Gemini) |
| `ocean-runtime` | Agent loop + permission-gated tool execution |
| `ocean-providers` | Provider registry: model routing, credentials, readiness |
| `ocean-agent` | Session/history layer — session load/save lives here |
| `ocean-agent-sdk` | SDK surface for embedding the agent in other Rust code |
| `ocean-daemon` | Long-running HTTP service on `:4780` |
| `ocean-cli` | CLI client |
| `ocean-tui` | Terminal steering cockpit (`ocean` binary) |

## Work Guidance

- Build: `cargo build --workspace --release`
- TUI change: `cargo build -p ocean-tui --release`
- Daemon restarts: standing authorization to restart from `main`; use specific-PID kill, not blind pkill
- Daemon health: `GET /health` (not `/v1/health`)
- Sessions live in the daemon only — session bugs break both TUI and ocean-surface simultaneously

## Verification

- `cargo check --workspace` must pass before any commit
- `cargo fmt --check` is gated in CI
- Knox (review gate) must ack before merge on feature/logic PRs

## Child devlog Index

- `.ocean/` — Ocean runtime artifacts, config, and session data → `.ocean/AGENTS.md`
- `crates/` — Rust workspace crates and crate-specific contracts → `crates/AGENTS.md`
- `docs/` — Architecture and operator documentation → `docs/AGENTS.md`
