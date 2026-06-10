# Phase 2 — handoff

All code for Phase 2 has been written. The bash classifier was unavailable
for the duration of the session, so `pnpm install`, migration, typecheck,
and commit still need to be run by hand.

## What was built

### Slice A — daemon (`brain watch`)
- `packages/daemon/` — new package
  - `src/ipc.ts` — Unix-socket JSON-RPC server + client (`IpcClient`, `tryConnectIpcClient`)
  - `src/watch.ts` — `WatchManager` (chokidar, per-path debounce)
  - `src/tick.ts` — `TickScheduler` (no overlap, immediate first run)
  - `src/state.ts` — `updateDaemonState` / `clearDaemonState`
  - `src/daemon.ts` — `BrainDaemon` class, `runDaemon()` entrypoint with SIGINT/SIGTERM
  - `src/client.ts` — `DaemonClient` typed wrapper
- `packages/cli/src/commands/watch.ts` — `brain watch / status / stop`
- Migration `0003_phase2.notx.sql` adds `daemon_state`, `embedding_models`,
  infra unique index, open-loop refinement columns, project-summary
  provenance columns, and new embedding_owner enum values.
  `.notx.sql` suffix bypasses the per-file transaction wrapper (needed for
  `ALTER TYPE ADD VALUE`).

### Slice B — infra scanner
- `packages/indexer/src/scanners/infra.ts` — `scanLocalInfra()` + parsers for
  `docker ps --format '{{json .}}'` and `lsof -iTCP -sTCP:LISTEN -F pcPn`,
  plus framework heuristics and project assignment via compose labels or
  longest-prefix cwd match.
- `packages/indexer/src/infra-sync.ts` — `syncLocalInfra()` upserts to
  `infra_resources` (raw SQL because the unique index uses
  `COALESCE(project_id, zero-uuid)`) and marks not-seen-this-run rows as
  `stopped`.
- Wired into:
  - `BrainDaemon.runPeriodicTick()`
  - `brain scan` (new `--no-infra` flag)
  - `brain standup` → new "RUNNING NOW" section
  - `deriveAlerts` → two new alert kinds: `service_down` (exited container)
    and `port_conflict` (>1 pid on same port in last hour)

### Slice C — embeddings + semantic recall
- `packages/embedder/` — new package
  - `types.ts` — `Embedder` interface + `EmbedderConfig`
  - `openai.ts` — default, 1536-dim `text-embedding-3-small` (dim override
    supported)
  - `voyage.ts` — Voyage AI alternative
  - `ollama.ts` — local self-hosted alternative
  - `config.ts` — reads `[embedder]` section from
    `~/.config/brain/config.toml`
  - `factory.ts` — `createEmbedder()` dispatches by kind
  - `store.ts` — `embedAndStore()` with content-hash dedupe,
    `searchByVector()` pgvector cosine-distance
  - `summarize.ts` — `summarizeProjects()` using Claude Haiku 4.5, writes to
    `projects.summary` + embeds as `project_summary`
  - `refine-loops.ts` — `refineOpenLoops()` rewrites messy TODOs/loops into
    actionable one-liners, writes to `open_loops.refined_text` + embeds as
    `open_loop`
- `packages/cli/src/commands/ask.ts` — three new commands: `brain ask <q>`,
  `brain summarize`, `brain refine`

### Tests
- `packages/indexer/src/scanners/infra.test.ts` — docker/lsof parser
  fixtures, framework/project assignment
- `packages/daemon/src/ipc.test.ts` — full round-trip RPC, error surfaces,
  stale-socket rejection, malformed-frame tolerance
- `packages/embedder/src/openai.test.ts` — mocked HTTP

## What still needs to happen

```bash
# 1. install new packages (@brain/embedder, vitest in indexer/embedder,
#    smol-toml in embedder)
pnpm install

# 2. migration — must be run against a running pg16+pgvector instance
pnpm db:up
pnpm db:migrate       # picks up 0003_phase2.notx.sql automatically

# 3. typecheck every package
pnpm -r typecheck

# 4. run the new tests
pnpm -r test

# 5. commit
git add -A
git commit -m "feat: phase 2 — daemon + infra + embeddings"
```

## Config

Put in `~/.config/brain/config.toml`:

```toml
[embedder]
kind = "openai"
model = "text-embedding-3-small"
dim = 1536
```

Export `OPENAI_API_KEY` and `ANTHROPIC_API_KEY` in your shell.

## Known lint noise

Until `pnpm install` runs, every file shows `Cannot find module '…'` TS
errors for `@brain/db`, `@brain/shared`, `@brain/indexer`, `drizzle-orm`,
`smol-toml`, `@brain/embedder`. All will resolve after install.

Implicit-any warnings on `r.map((r) => …)` callbacks inside alerts.ts and
scan.ts are the same root cause — once TS can see the drizzle result types,
it infers the parameter types. No runtime effect.
