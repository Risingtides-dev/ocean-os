# ocean-daemon — HTTP Daemon

## Purpose

This crate owns the long-running Ocean HTTP service on `:4780`, including API routes, SSE event streaming, daemon runtime authority, and client-facing turn orchestration.

## Ownership

- **Scope:** `crates/ocean-daemon/`
- **Parent contracts:** `../../AGENTS.md` and `../AGENTS.md`
- **Primary responsibilities:** daemon API, health endpoints, turn/event routes, runtime wiring, permission boundary enforcement

## Local Contracts

- Daemon health is `GET /health`, not `/v1/health`.
- Restart the daemon only by specific PID; do not use blind `pkill` sweeps.
- HTTP turn routes must resolve effective cwd from client cwd/project metadata and must never fall back to daemon process cwd.
- Do not bypass runtime permission gates from daemon route code.
- Session behavior lives in `ocean-agent`; route changes must not create a separate session model.
- Agent SSE replay is globally bounded by both 2,048 events and 32 MiB of serialized event payload. Oldest envelopes evict until both limits hold; an individually oversized event remains live but is not replay-retained. Preserve full live delivery and the existing explicit subscriber-lag signal.
- Build provenance must follow normal branch commits and linked worktrees: `build.rs` watches Git `HEAD`, its resolved symbolic branch ref, and `packed-refs`; `/health` and `/ready` must report the exact main-built revision after deployment.
- `AgentEvent::TurnCheckpoint` is an internal persistence signal consumed by `ocean-agent`; daemon bridges must filter it rather than exposing transcript deltas on SSE.
- The Track-0 projection routes (`GET /v1/rooms`, detail, snapshot, events) are retired. Preserve `/v1/rooms/persistent/*` and `/v1/rooms/{room_id}/livekit-token`; these are separate durable-collaboration and media contracts.
- The explicit method/path set in `app_router`, `banner_routes()`, and the operator-guide HTTP quick reference must remain identical. Preserve Axum's default 404/405 fallback and the global layer order: HTTP tracing outside CORS outside route dispatch.

## Work Guidance

- Keep HTTP/SSE contracts stable for both `ocean-tui` and `ocean-surface`.
- Turns execute in the caller's cwd; never pin resumed turns to the daemon launch cwd or the first session cwd.
- Build from up-to-date `main` before daemon restarts when doing operator work.
- Prefer narrow route tests for API behavior and workspace checks before merge.

## Verification

- `cargo test -p ocean-daemon bus::tests::`
- `cargo test -p ocean-daemon cors::tests:: -- --nocapture`
- `cargo test -p ocean-daemon event_adapter::tests:: -- --nocapture`
- `cargo test -p ocean-daemon metrics::tests:: -- --nocapture`
- `cargo test -p ocean-daemon router_contract -- --nocapture`
- `cargo test -p ocean-daemon`
- `cargo check --workspace`
- Manual daemon health check when route/startup behavior changes: `curl http://127.0.0.1:4780/health`

## Child devlog Index

No child boundaries defined within `ocean-daemon/` at this time.
