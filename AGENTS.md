# Ocean OS — Devlog Root

## Purpose

This is the root devlog contract for the `ocean-os` repository. Every agent entering this repo — Claude, Codex, Pi, ocean-native, or any other harness — reads this file first.

## Ownership

- **Repo:** `risingtides-dev/ocean-os`
- **Runtime:** Rust workspace, daemon on `:4780`, TUI binary `ocean`
- **Connected Ocean repos:** route cross-repo work through `docs/OCEAN_PROJECT_MAP.md`; do not infer ownership from proximity.

## Local Contracts

- Read this file before editing anything in this repo.
- Walk from repo root to each target path and read every `AGENTS.md` along the route.
- Use the nearest `AGENTS.md` as the local contract; parent docs set repo-wide rules.
- No child doc may weaken this root contract.
- Cross-repo routing map: `docs/OCEAN_PROJECT_MAP.md`.
- Canonical workspace package/entry/test index: `crates/AGENTS.md`. Do not maintain a second crate inventory here.
- After any meaningful change, do a devlog pass: update the nearest owning `AGENTS.md`, refresh affected child indexes, remove stale text, and append a root `events.md` entry with `worktree:`.

## Workspace Routing

Core execution flow:

```text
clients (TUI / CLI / ACP / surface)
  -> ocean-daemon (HTTP/SSE authority)
  -> ocean-agent (sessions, prompts, capability assembly)
  -> ocean-runtime (agent loop, permissions, tools)
  -> ocean-protocol + ocean-providers (wire encoding + model/auth routing)
```

Use `crates/AGENTS.md` for all 25 workspace packages, ownership exclusions, entry points, local contracts, and narrow validation commands.

## Work Guidance

- Active improvement program: `docs/specs/2026-07-12-ocean-code-health-and-agent-readiness-plan.md`.
- Optimize for cold-agent discoverability: ownership, entry point, critical invariant, and narrow validation must remain findable from the root plus `crates/AGENTS.md`.
- Behavior-neutral extraction requires a written extraction manifest and must not bundle redesign, protocol changes, renames, or opportunistic fixes.
- Build: `cargo build --workspace --release`.
- TUI change: `cargo build -p ocean-tui --release`.
- Daemon restarts: standing authorization to restart from `main`; use specific-PID kill, not blind `pkill`.
- Daemon health: `GET /health` (not `/v1/health`).
- Supervised daemon (`dev.risingtides.ocean-daemon` LaunchAgent): install/reinstall via `ops/install-ocean-daemon.sh`, which refuses non-`main`, builds release, installs the plist, and bootstraps launchd. Ship new code by rebuilding from updated `main`, then `launchctl kickstart -k gui/$(id -u)/dev.risingtides.ocean-daemon`.
- The daemon must run from a neutral cwd (`$HOME` by default), never the repo. Its startup guard rejects repository cwd so unbound fallback turns cannot bind to ocean-os; do not bypass this with `OCEAN_ALLOW_REPO_CWD=1`.
- Sessions live under daemon authority via `ocean-agent`; session bugs break TUI and ocean-surface together.

## Verification

### Fast edit loop

- Run the nearest owning crate's narrow command from `crates/AGENTS.md` or its local contract.
- For docs-only changes, run `cargo xtask docs-check`; it validates active repo-local Markdown file targets (not heading fragments), archive boundaries, canonical workspace-index parity, and non-default-member rationale.

### Workspace completion gate

- `cargo check --workspace`
- Relevant crate tests; use `cargo check --workspace --tests` when shared enums/events fan out across crates.
- Feature-specific checks named by the owning crate contract.

### Merge / PR gate (mirrors CI)

- `cargo xtask ci` is the canonical local gate: docs/index integrity, workspace build/test, all-target Clippy with denied warnings, format, and `cargo deny check`.
- `cargo xtask ci --dry-run` prints the portable command manifest plus omitted CI-only matrix/setup lanes without executing them.
- Fresh reviewer acknowledgement is required for feature, logic, security, protocol, or architecture changes.

CI consumes the same xtask manifest on macOS and Ubuntu with `--skip-deny`; `cargo-deny` remains a separate Ubuntu job. A local single-host run does not replace that matrix.

## Child devlog Index

- `.ocean/` — Ocean runtime artifacts, config, and session data → `.ocean/AGENTS.md`
- `crates/` — canonical Rust workspace ownership/entry/test index and crate contracts → `crates/AGENTS.md`
- `docs/` — architecture, operator documentation, active plans, and historical archive policy → `docs/AGENTS.md`
