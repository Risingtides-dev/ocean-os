# Contributing to Ocean-OS

Ocean-OS is a squad project. Each Claude/operator on the team should claim one or more ingestion workers and own them end to end. This file explains how.

## Claiming a worker

Open a PR adding your name + handle to the table below. One worker per row. If you're claiming, you're committing to:

1. Building the ingestion worker (webhook intake or polling, your choice).
2. Writing rows to that source's schema in `schema/000_init.sql`. If your source needs new tables, propose them in the same PR.
3. Wiring at least one MCP tool in `mcp/src/index.ts` that returns real data from your source (replacing the stub).
4. Documenting the env vars and deploy steps in a `README.md` inside your worker's directory.

## Source ownership

| Source | Owner | Status | Notes |
|---|---|---|---|
| `github` | _unclaimed_ | scaffolded | proof of concept exists, needs deploy + secret |
| `slack` | _unclaimed_ | not started | already have Slack tokens via the bridges |
| `cobrand` | _unclaimed_ | not started | API access via Campaign Hub |
| `campaign-hub` | _unclaimed_ | not started | Postgres on Railway, can replicate or poll API |
| `content-lab` | _unclaimed_ | not started | FastAPI on Railway, expose internal events endpoint |
| `telegram` | _unclaimed_ | not started | tap into existing bot or poll the bridge logs |
| `railway` | _unclaimed_ | not started | poll Railway API |
| `cloudflare` | _unclaimed_ | not started | poll Cloudflare API for Workers/DNS/R2/email rules |
| `notion` | _unclaimed_ | not started | webhook + polling hybrid |

## Worker conventions

- One service per source. Independent deploy.
- TypeScript or Python — pick whatever you ship faster in. Prefer TypeScript for consistency with the MCP.
- Idempotent on the source's native event id. Re-processing a delivery is a no-op.
- Write raw payloads to `<source>.events`. Materialized views are populated by separate functions/jobs, never inline in the webhook handler.
- Health endpoint at `GET /health`.
- Log JSON lines.

## MCP tool conventions

- Tool name is `ocean.<verb>_<noun>` — verb maps to operator intent.
- Args are flat objects. Document required and optional fields in the JSON schema.
- Return shape-correct data even when the source is empty (e.g. `[]`, never `null`).
- Every meaningful agent action should call `ocean.log_agent_action` afterward to keep the feedback log alive.

## Schema migrations

For now, single file: `schema/000_init.sql`. When we need real migrations, we'll move to numbered files and pick a tool. PRs that add tables: edit `000_init.sql` until further notice.

## Deploy targets

Pending squad decision. Default leaning:

- Postgres + workers on Railway
- Webhook intake on Cloudflare Workers
- R2 for blob caching

Final choice will be tracked in `docs/architecture.md`.
