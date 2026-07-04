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

## Cursor Cloud specific instructions

Toolchain and build commands are in the root `## Work Guidance` / `## Verification` sections above; the notes below are only the non-obvious caveats for this environment.

- **Rust toolchain must be ≥ 1.85.** A transitive dep (`agent-client-protocol-derive`) needs the `edition2024` cargo feature; the default preinstalled `1.83` fails to even parse manifests. This VM's default is `stable` (currently 1.96) via `rustup default stable`.
- **System libraries required for the build:** `pkg-config` and `libssl-dev` (needed by `openssl-sys`). Missing them yields a "Could not find openssl via pkg-config" build error.
- **Running the daemon:** it refuses to start when its cwd is inside a git repo (guards against unbound turns binding to the repo). Launch it from a neutral dir (`cd ~ && .../ocean-daemon`) or set `OCEAN_ALLOW_REPO_CWD=1`.
- **Credential-free local run:** set `OCEAN_MODEL=fake` (no provider key needed) to exercise the full daemon → runtime → session flow; add `OCEAN_YOLO=1` to auto-approve tool calls. Real work needs a provider key + a real `OCEAN_MODEL` (see README "Provider configuration").
- **Clients bind to a project dir, not the daemon cwd.** Use `ocean-rs --project <dir> prompt ...` or pass `cwd` in `POST /v1/agent/turns`; unbound turns are rejected. Sessions/state persist under `~/.config/ocean-rs/` (bundled SQLite, no external DB).
- **Known environment-limited test:** `cargo test -p ocean-protocol` → `http::tests::connect_timeout_trips_on_non_routable_address` fails here because the sandbox network does not fast-fail connections to the non-routable `192.0.2.1` (the connect takes ~5s instead of tripping the 200ms timeout). This is a network-environment artifact, not a code defect; the rest of the workspace tests pass.
