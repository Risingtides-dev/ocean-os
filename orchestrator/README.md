# orchestrator

Tracks tasks the orchestrator is running on behalf of humans. Detects stalls and escalates via Slack DM.

See [`docs/escalation.md`](../docs/escalation.md) for the full escalation policy.

## Env vars

| Var | Required | Description |
|---|---|---|
| `PORT` | no | HTTP port (default `8082`) |
| `OCEAN_DATABASE_URL` | yes | Postgres connection string — write role for `orchestrator` schema |
| `SLACK_BOT_TOKEN` | yes | Bot token with `chat:write` scope — used to send DMs to task owners |

## Schema

Run `schema/300_orchestrator.sql` after `schema/000_init.sql`.

## Running locally

```sh
pnpm install
OCEAN_DATABASE_URL=postgres://... SLACK_BOT_TOKEN=xoxb-... pnpm dev
```

## Deploy

Same pattern as `ingestion/github` — Railway service, one process, health check at `GET /health`.

## Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Liveness check |
| `POST` | `/tasks` | Create a task |
| `GET` | `/tasks/:id` | Get a task |
| `POST` | `/tasks/:id/progress` | Tick a round (pass `{"failure": true}` to record an error) |
| `POST` | `/tasks/:id/complete` | Mark a task completed |
| `POST` | `/tasks/:id/simulate-stall` | Force escalation immediately — for testing |
| `GET` | `/queue/:slack_user_id` | Stalled + escalated tasks for a human |
