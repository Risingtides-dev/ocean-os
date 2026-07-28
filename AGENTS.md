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
- Project code is `MIT OR Apache-2.0`, copyright © 2026 Rising Tides. Preserve `NOTICE.md` and local donor notices; Ocean names, logos, and distinctive brand assets remain outside the open-source grants under `TRADEMARKS.md`.
- Rust workspace packages are private distribution inputs, not crates.io releases: `[workspace.package] publish = false`, and every member must inherit it or set `publish = false` explicitly. `cargo xtask docs-check` enforces this for new members.

## Workspace Routing

Core execution flow:

```text
clients (TUI / CLI / ACP / surface)
  -> ocean-daemon (HTTP/SSE authority)
  -> ocean-agent (sessions, prompts, capability assembly)
  -> ocean-runtime (agent loop, permissions, tools)
  -> ocean-protocol + ocean-providers (wire encoding + model/auth routing)
```

Use `crates/AGENTS.md` for all current workspace packages, ownership exclusions, entry points, local contracts, and narrow validation commands.

## Work Guidance

- Current architecture and operations live in `docs/ARCHITECTURE.md` and `docs/OPERATIONS.md`; open work lives in `ROADMAP.md`.
- The active extension architecture and staged migration program is governed by `docs/specs/2026-07-14-ocean-extensions-architecture-and-migration-manifest.md`; Phases 0–1 are accepted. The operator-accepted Phase 6 orchestration-transport ratification is `docs/specs/2026-07-18-ocean-crew-orchestration-and-durable-workflow-manifest.md`; the exact Stage A implementation contract at `docs/specs/2026-07-27-ocean-extension-stage-a-implementation-manifest.md` authorizes only its ordered A1 → A2a → A2b → A3a → A3b → A4 → A5 sequence. Stage A1 is accepted: additive SDK v1 DTOs plus an unwired pure daemon lifecycle adapter. Stage A2a is accepted after independent correctness, protocol, process-security, and portability review: the sole descriptor/reparse-safe read-only four-file registry and common exact service-authority derivation plus minimum exact-grant native stdio supervision, typed post-ready EOF/read-I/O versus protocol-failure causes, descriptor/file-id-bound executable and assigned-root generations on macOS/Linux, minimal environment, explicit secret resolution, bounded generation-safe Unix group cleanup with retained exceptional-failure authority, validated unsupported-platform cache projection, a repository-owned actual-source Windows portability cross-build, and a non-probing status cache. It adds no registry mutation/route/CLI, lifecycle producer wiring/replay, or health/restart policy. Because A2a owns no mutation or durable first-publication marker, only A3a may distinguish a deleted post-A0 `service-grants.json` from the accepted A0 absence form; A3a must add that marker atomically and fail closed on marked absence. A2b is the next authorized slice. Crew Stages B–E each require their own implementation manifest and keep extension-provided Longhouse facades over an extension-owned Crew engine, not core.
- The Ocean Observatory program is governed by `docs/specs/2026-07-17-ocean-observatory-architecture.md` with Gate 0 accepted in `docs/specs/2026-07-17-observatory-gate0-decisions.md` (operator ruling: 90s pixel-game visual parity on truthful real events with a durable event store; security invariants unchanged). The operator accepted the Gate 1 implementation manifest at `docs/specs/2026-07-17-observatory-gate1-implementation-manifest.md` on 2026-07-17; tasks 2–9 are authorized under its strict dependency order and stop conditions. Tasks 2–8 are landed; the Task 9 independent review is retained at `docs/specs/2026-07-20-observatory-gate1-task9-independent-review.md` with gating repairs G1–G5 that must land and pass delta review before production Ocean Floor renderer work. Do not build from the Claude Design export code or the raw global agent firehose.
- The Ocean Browser program is governed by `docs/specs/2026-07-19-ocean-webkit-browser-program.md` (operator ruling 2026-07-19: a custom WebKit engine with earned Chrome DevTools protocol parity, built outside the Cargo graph in a dedicated repository). The legacy Chromium backend is quarantined behind the default-off `legacy-chromium` feature; the supervised daemon keeps interim browsing via `ops/install-ocean-daemon.sh`. Acceptance gates are fixed in the manifest; do not ship partial-fidelity network capture as parity.
- The active behavior-neutral daemon refactor is governed by `docs/DAEMON_REFACTOR_MISSION.md` and the supporting code-health plan under `docs/specs/`.
- Optimize for cold-agent discoverability: ownership, entry point, critical invariant, and narrow validation must remain findable from the root, `docs/README.md`, and `crates/AGENTS.md`.
- Behavior-neutral extraction requires a written extraction manifest and must not bundle redesign, protocol changes, renames, or opportunistic fixes.
- Subagent definitions, dispatch, lifecycle, and orchestration policy are extension-owned. Do not add a core daemon/runtime `task`, `spawn_worker`, fleet scheduler, or named-subagent runtime; core may expose only generic permission-gated execution, cancellation, capability-provider, and extension event/tool seams. Existing core subagent-shaped metadata/spec routes are compatibility surfaces pending a separately approved extension migration.
- Once the operator explicitly authorizes an ongoing program, continue through safe approved checkpoints without repeated approval prompts. Close each bounded change with verification, review, commit, upstream reconciliation, and a clean tree; pause only for a concrete blocker or required design decision.
- A feature is not delivered while it exists only in a stash, detached worktree,
  local commit, or unmerged branch. Build and test feature branches normally;
  do not interpret production-deploy provenance rules as permission to skip
  compilation. Close authorized work by reconciling current `origin/main`,
  obtaining fresh review, landing the commit upstream, fast-forwarding the
  canonical checkout, and rebuilding/installing every affected operator-facing
  binary. If an external-action gate blocks landing, report the exact branch,
  commit, remaining command, and blocker; never describe the feature as shipped.
- The Chromium browser backend is quarantined behind the default-off `legacy-chromium` feature (details and validation in `crates/AGENTS.md`) pending the OceanWebKit browser host; default builds must compile no chromiumoxide and must never launch or probe a browser.
- Minimum supported Rust is 1.88, enforced by the MSRV lane; do not lower it without pinning the resolved dependency graph and proving every supported feature.
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
- `cargo xtask ci --compatibility` checks supported daemon features and release-profile all-target compilation on stable Rust.
- `cargo +1.88.0 xtask ci --msrv` checks default and supported daemon feature paths at the declared MSRV.

### Merge / PR gate (mirrors CI)

- `cargo xtask ci` is the canonical local gate: docs/index integrity, workspace build/test, all-target Clippy with denied warnings, format, and `cargo deny check`.
- `cargo xtask ci --dry-run` prints the portable command manifest plus omitted CI-only matrix/setup lanes without executing them.
- Fresh reviewer acknowledgement is required for feature, logic, security, protocol, or architecture changes.

CI consumes the repository and compatibility manifests on macOS and Ubuntu, runs the MSRV manifest under pinned Rust 1.88 on Ubuntu, and keeps `cargo-deny` in a separate Ubuntu job. A local single-host run does not replace those lanes.

## Child devlog Index

- `.ocean/` — Ocean runtime artifacts, config, and session data → `.ocean/AGENTS.md`
- `crates/` — canonical Rust workspace ownership/entry/test index and crate contracts → `crates/AGENTS.md`
- `docs/` — architecture, operator documentation, active plans, and historical archive policy → `docs/AGENTS.md`
- `integrations/` — distributable adapters for external host extension surfaces → `integrations/AGENTS.md`
- `packaging/` — team distribution: npm wrapper package publishing prebuilt `ocean` + `ocean-daemon` binaries via the tag-triggered release workflow → `packaging/AGENTS.md`
