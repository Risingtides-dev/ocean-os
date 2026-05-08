# Orchestrator

Accepts inbound webhook events, persists them as `orchestrator.tasks` rows, and lets twin bridges claim work via `POST /tasks/:id/claim`. Dispatch routing is stubbed — tasks are logged, not forwarded yet.

## Env vars

| Variable | Required | Default | Description |
|---|---|---|---|
| `PORT` | no | `8082` | Port to listen on |
| `OCEAN_DATABASE_URL` | yes | — | Postgres connection string with write access to the `orchestrator` schema |
| `ORCHESTRATOR_SECRET` | no | *(empty)* | Bearer token required on every inbound request. Omit for local dev; **always set in production** |

## Schema

Run `schema/002_orchestrator.sql` against your database before starting the service. It is **not** applied automatically.

```bash
psql $OCEAN_DATABASE_URL -f ../schema/002_orchestrator.sql
```

## Local dev

```bash
# Install deps
pnpm install        # or npm install / yarn

# Start with hot-reload
OCEAN_DATABASE_URL=postgres://user:pass@localhost:5432/ocean \
  pnpm dev

# Or build and run
pnpm build && pnpm start
```

## Endpoints

### `POST /events`

Accept a webhook envelope and create a task row.

**Headers:**
```
Authorization: Bearer <ORCHESTRATOR_SECRET>   # omit if secret is empty
Content-Type: application/json
```

**Body:**
```json
{
  "source": "github",
  "event_type": "issues.opened",
  "source_ref": "abc123-delivery-id",
  "payload": { "...": "raw webhook body" }
}
```

**Response `202`:**
```json
{ "ok": true, "task_id": "uuid" }
```

`source_ref` is optional — use it to link back to the originating webhook delivery ID or Slack event ID.

---

### `POST /tasks/:id/claim`

A twin's bridge calls this when it's ready to execute a task.

**Body:**
```json
{ "twin_id": "smaths-bot" }
```

**Response `200`:**
```json
{
  "ok": true,
  "dispatch_id": "uuid",
  "task": { "id": "uuid", "payload": { "..." : "..." } }
}
```

Fails with `409` if the task is already claimed.

---

### `GET /health`

Returns `{ "ok": true }`. No auth required.

## What's not wired yet

- **Real dispatch routing** — tasks are logged but not forwarded to any twin. Planned in a follow-up issue.
- **Auth handshake** — `ORCHESTRATOR_SECRET` is a simple shared bearer token for now. HMAC or JWT handshake is a separate issue.
- **Outcome reporting** — twins can't yet report `done`/`failed` back. That needs a `PATCH /dispatches/:id` endpoint in a follow-up.
- **Queue / retry** — no backoff or dead-letter handling yet. Tasks that are claimed but never resolved stay in `claimed` state indefinitely.
