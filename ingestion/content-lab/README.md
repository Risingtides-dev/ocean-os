# ingestion/content-lab

Ocean-OS ingestion worker for Content Posting Lab.

Receives job-state events from the Content Posting Lab FastAPI service (Railway)
and writes them to `content.events`, `content.jobs`, and `content.distributions`
in the Ocean Postgres database.

## Two intake modes

### Webhook (preferred)

Content Lab calls `POST /webhook` on this worker whenever a job state changes.
This requires a thin `/api/internal/events` endpoint on the Lab side — coordinate
with **smaths** before enabling.

The webhook payload should look like this:

```json
{
  "event": "job.updated",
  "job_id": "abc123",
  "job_type": "generate",
  "status": "completed",
  "project_id": "lazy-rosana",
  "occurred_at": "2025-01-01T00:01:00Z",
  "inputs":  { "brief": "...", "artist": "..." },
  "outputs": { "video_url": "...", "caption": "..." }
}
```

For distribution events, include `distribution_id`, `surface`, and `target`:

```json
{
  "event": "distribution.completed",
  "job_id": "abc123",
  "distribution_id": "def456",
  "surface": "telegram",
  "target": "lazy-rosana-folder",
  "status": "delivered",
  "occurred_at": "2025-01-01T00:02:00Z"
}
```

If `CONTENT_LAB_WEBHOOK_SECRET` is set, the worker validates
`x-content-lab-signature: sha256=<hmac>` on every request (same pattern as
the GitHub worker's `x-hub-signature-256`). Omit the header or leave the secret
empty to skip validation during local development.

### Polling (fallback)

Enable by setting `POLL_INTERVAL_MS`. The worker calls:

```
GET CONTENT_LAB_API_URL/api/jobs?since=<iso8601>&limit=100
```

Expected response shape (confirm with smaths before enabling):

```json
{
  "jobs": [
    {
      "id": "abc123",
      "type": "generate",
      "status": "completed",
      "project_id": "lazy-rosana",
      "created_at": "2025-01-01T00:00:00Z",
      "updated_at": "2025-01-01T00:01:00Z",
      "inputs":  { ... },
      "outputs": { ... },
      "distributions": [
        {
          "id": "def456",
          "surface": "telegram",
          "target": "lazy-rosana-folder",
          "status": "delivered",
          "updated_at": "2025-01-01T00:02:00Z"
        }
      ]
    }
  ]
}
```

Both modes write to the same tables and are idempotent — running both at once
is safe (duplicate events are dropped via `ON CONFLICT DO NOTHING`).

## Env vars

| Var | Required | Description |
|---|---|---|
| `OCEAN_DATABASE_URL` | **yes** | Postgres connection string with write access to the `content` schema |
| `PORT` | no | HTTP port (default `8082`) |
| `CONTENT_LAB_WEBHOOK_SECRET` | no | HMAC secret for webhook signature validation. Leave unset to skip validation. |
| `CONTENT_LAB_API_URL` | polling only | Base URL of the Content Lab FastAPI, e.g. `https://content-lab.up.railway.app` |
| `CONTENT_LAB_API_KEY` | polling only | Bearer token for Content Lab API auth |
| `POLL_INTERVAL_MS` | no | Enables polling mode; interval in milliseconds (e.g. `60000` for 1 min) |

## Database migration

Before deploying the worker, apply the schema migration:

```bash
psql "$OCEAN_DATABASE_URL" -f ../../schema/300_content_lab.sql
```

This adds `lab_job_id` and `lab_distribution_id` columns (with unique constraints)
to `content.jobs` and `content.distributions`. The worker's `ON CONFLICT` upserts
depend on these constraints. Safe to re-run.

## Running locally

```bash
# Install
pnpm install

# Dev (ts-node)
OCEAN_DATABASE_URL=postgres://... pnpm dev

# Or with polling enabled
OCEAN_DATABASE_URL=postgres://... \
CONTENT_LAB_API_URL=https://content-lab.up.railway.app \
CONTENT_LAB_API_KEY=... \
POLL_INTERVAL_MS=60000 \
pnpm dev
```

Send a test event:

```bash
curl -X POST http://localhost:8082/webhook \
  -H "Content-Type: application/json" \
  -d '{
    "event": "job.updated",
    "job_id": "test-001",
    "job_type": "generate",
    "status": "completed",
    "project_id": "lazy-rosana",
    "occurred_at": "2025-01-01T00:01:00Z"
  }'
```

Check the event landed:

```sql
SELECT * FROM content.events ORDER BY ingested_at DESC LIMIT 5;
SELECT * FROM content.jobs   ORDER BY created_at   DESC LIMIT 5;
```

## Deploy (Railway)

1. Add a new Railway service pointing at this repo, root: `ingestion/content-lab/`.
2. Set the env vars above in the Railway service dashboard.
3. Railway will run `pnpm build && pnpm start`.
4. Set the service's public URL as the webhook target in Content Lab's
   `/api/internal/events` config (once that endpoint exists — coordinate with smaths).

## Health

```
GET /health  →  { "ok": true }
```
