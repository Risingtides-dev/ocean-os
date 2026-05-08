# Orchestrator

Two surfaces on one Fastify service, sharing the `orchestrator.tasks` table:

- **Dispatch surface** — accepts inbound webhook events, persists them as task rows, and lets twin bridges claim work via `POST /tasks/:id/claim`. Dispatch routing is stubbed; tasks are logged, not forwarded yet.
- **Agent-task surface** — tracks long-running tasks an agent is executing on behalf of a human. Counts rounds, detects stalls (max rounds, repeated failures, twin unreachable), and DMs the human owner over Slack. See [`docs/escalation.md`](../docs/escalation.md) for the full escalation policy.

Dispatch rows have `human_owner = NULL` and are ignored by the stall detector. Agent-task rows have `human_owner` set.

## Env vars

| Variable | Required | Default | Description |
|---|---|---|---|
| `PORT` | no | `8082` | Port to listen on |
| `OCEAN_DATABASE_URL` | yes | — | Postgres connection string with write access to the `orchestrator` schema |
| `ORCHESTRATOR_SECRET` | no | *(empty)* | Bearer token required on dispatch endpoints (`POST /events`, `POST /tasks/:id/claim`). Omit for local dev; **always set in production** |
| `SLACK_BOT_TOKEN` | yes | — | Bot token with `chat:write` scope, used to DM humans on escalation. Service starts without it but escalation DMs will fail. |

## Schema

Run both migrations against the database before starting the service. Neither runs automatically.

```bash
psql $OCEAN_DATABASE_URL -f ../schema/002_orchestrator.sql
psql $OCEAN_DATABASE_URL -f ../schema/005_orchestrator_escalation.sql
```

`002_orchestrator.sql` creates `orchestrator.tasks` and `orchestrator.dispatches`. `005_orchestrator_escalation.sql` adds the agent-task columns (`human_owner`, `description`, `rounds`, `max_rounds`, `consecutive_failures`, `stall_reason`, `escalated_at`) as nullable additive columns and the supporting indexes.

## Local dev

```bash
pnpm install        # or npm install / yarn

OCEAN_DATABASE_URL=postgres://user:pass@localhost:5432/ocean \
  SLACK_BOT_TOKEN=xoxb-... \
  pnpm dev

# Or build and run
pnpm build && pnpm start
```

## Deploy

Same pattern as `ingestion/github` — Railway service, one process, health check at `GET /health`.

## Endpoints

### Health

| Method | Path | Description |
|---|---|---|
| `GET` | `/health` | Returns `{ "ok": true }`. No auth. |

### Dispatch surface

| Method | Path | Description |
|---|---|---|
| `POST` | `/events` | Accept a webhook envelope, create a `pending` task |
| `POST` | `/tasks/:id/claim` | Twin bridge claims the task; transitions it to `claimed` and records a row in `orchestrator.dispatches` |

Both require `Authorization: Bearer <ORCHESTRATOR_SECRET>` if `ORCHESTRATOR_SECRET` is set.

**`POST /events`** — body:
```json
{
  "source": "github",
  "event_type": "issues.opened",
  "source_ref": "abc123-delivery-id",
  "payload": { "...": "raw webhook body" }
}
```
Returns `202 { "ok": true, "task_id": "uuid" }`.

**`POST /tasks/:id/claim`** — body:
```json
{ "twin_id": "smaths-bot" }
```
Returns `200 { "ok": true, "dispatch_id": "uuid", "task": { "id": "uuid", "payload": {...} } }`. Fails `409` if already claimed.

### Agent-task surface

| Method | Path | Description |
|---|---|---|
| `POST` | `/tasks` | Create an agent task (`{ human_owner, twin_id?, description, max_rounds? }`) |
| `GET` | `/tasks/:id` | Read a task |
| `POST` | `/tasks/:id/progress` | Tick a round (`{ "failure": true }` to record an error) — fires escalation immediately if a stall condition is hit |
| `POST` | `/tasks/:id/complete` | Mark completed |
| `POST` | `/tasks/:id/simulate-stall` | Force escalation immediately (testing) |
| `GET` | `/queue/:slack_user_id` | Stalled + escalated tasks for that human |

Stall conditions, any one is sufficient: `rounds >= max_rounds` (default 10), `consecutive_failures >= 3`, status manually set to `stalled`. On stall, the orchestrator posts a Slack DM to `human_owner` and stamps `status = 'escalated'`, `escalated_at = now()`. Stall detector also runs every 60 s as a safety net for tasks that didn't trip the inline check.

## What's not wired yet

- **Real dispatch routing** — claimed tasks aren't forwarded anywhere. Planned in a follow-up.
- **Auth handshake** — `ORCHESTRATOR_SECRET` is a simple shared bearer. HMAC or JWT handshake is a separate issue.
- **Outcome reporting from twins** — no `PATCH /dispatches/:id` yet. Twins can't report `done`/`failed` back.
- **Queue / retry** — no backoff or dead-letter for claimed-but-stuck tasks.
- **Cross-human reassignment when a human is OOO** — separate issue.
- **UI for `/queue/:slack_user_id`** — JSON-only for now.
- **`twin_unreachable` as a first-class stall condition** — currently surfaces via `repeated_failure`. A dedicated heartbeat table is a natural follow-up.
