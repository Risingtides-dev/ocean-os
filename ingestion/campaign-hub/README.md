# ingestion/campaign-hub

Polling ingestion worker for the Rising Tides Campaign Hub (Flask + PostgreSQL on Railway).

Campaign Hub does not emit webhooks, so this worker polls on a configurable interval, diffs against a cursor stored in `campaigns.poll_cursor`, and writes mutations to Ocean-OS's `campaigns.*` tables.

## What it ingests

| Entity | Events table type | Materialized table |
|---|---|---|
| Campaigns | `campaign_snapshot` | — (event log only; campaigns don't have a dedicated materialized table yet) |
| Bookings | `booking_snapshot` | `campaigns.bookings` |
| Creators | `creator_snapshot` | `campaigns.creators` |
| Payments | `payment_snapshot` | `campaigns.payments` |

## Idempotency

Running the worker twice in a row with no upstream changes is a no-op:

- **Events** (`campaigns.events`): keyed on `external_id = "{entity}:{hub_id}:{updated_at}"`. `ON CONFLICT DO NOTHING`.
- **Bookings / creators / payments**: upserted via `ON CONFLICT (hub_id) DO UPDATE ... WHERE updated_at < EXCLUDED.updated_at`. Stale writes are silently dropped.
- **Cursor** (`campaigns.poll_cursor`): only advances if the new timestamp is later than the stored one.

## Env vars

| Variable | Required | Description |
|---|---|---|
| `CAMPAIGN_HUB_API_URL` | yes | Base URL of the Campaign Hub Flask app on Railway, no trailing slash. Example: `https://campaign-hub.up.railway.app` |
| `CAMPAIGN_HUB_SERVICE_TOKEN` | yes | Bearer token Campaign Hub uses to authenticate service-to-service calls. Set as an env var on both sides. |
| `OCEAN_DATABASE_URL` | yes | Postgres connection string with write access to the `campaigns` schema. Example: `postgres://ocean:secret@host/oceandb` |
| `PORT` | no | Port for the `/health` endpoint. Default `8082`. |
| `POLL_INTERVAL_MS` | no | Poll frequency in milliseconds. Default `300000` (5 min). |

## Campaign Hub API contract

The worker calls these endpoints. If the Campaign Hub API differs, the worker's Zod schemas and path constants need updating.

### Authentication

All requests include `Authorization: Bearer <CAMPAIGN_HUB_SERVICE_TOKEN>`.

### Pagination

All list endpoints accept `cursor` (opaque string, from `next_cursor` in the previous response) and `per_page` (integer). When `next_cursor` is `null` or absent, there are no more pages.

```
GET /api/campaigns?updated_since=<ISO8601>&per_page=100[&cursor=<token>]
GET /api/bookings?updated_since=<ISO8601>&per_page=100[&cursor=<token>]
GET /api/creators?updated_since=<ISO8601>&per_page=100[&cursor=<token>]
GET /api/payments?updated_since=<ISO8601>&per_page=100[&cursor=<token>]
```

### Response shape (all entities)

```jsonc
{
  "data": [ /* array of entity objects */ ],
  "next_cursor": "opaque-string-or-null"
}
```

### Entity schemas

**Campaign**
```jsonc
{
  "id": "cam_abc123",
  "slug": "lazy-rosana-may-2025",
  "client": "Lazy Rosana",        // nullable
  "artist": "Lazy Rosana",        // nullable
  "status": "active",
  "budget_cents": 500000,         // nullable
  "updated_at": "2025-05-01T12:00:00Z"
}
```

**Booking**
```jsonc
{
  "id": "bk_456",
  "campaign_slug": "lazy-rosana-may-2025",
  "creator_handle": "@tiktokstar",
  "platform": "tiktok",
  "rate_cents": 50000,            // nullable
  "posts_owed": 3,                // nullable
  "status": "confirmed",
  "updated_at": "2025-05-01T12:00:00Z"
}
```

**Creator**
```jsonc
{
  "handle": "@tiktokstar",
  "platform": "tiktok",
  "metadata": { "follower_count": 100000 },  // nullable, arbitrary JSON
  "updated_at": "2025-05-01T12:00:00Z"
}
```

**Payment**
```jsonc
{
  "id": "pay_789",
  "campaign_slug": "lazy-rosana-may-2025",
  "creator_handle": "@tiktokstar",
  "amount_cents": 50000,
  "status": "paid",               // pending | paid | reversed
  "paid_at": "2025-05-02T00:00:00Z",  // nullable
  "occurred_at": "2025-05-02T00:00:00Z",
  "updated_at": "2025-05-02T00:00:00Z"
}
```

## Schema migration

Before deploying, run `schema/002_campaigns_hub.sql` against the Ocean-OS database. This adds `hub_id` to `campaigns.bookings`, creates `campaigns.payments`, and creates `campaigns.poll_cursor`.

```bash
psql "$OCEAN_DATABASE_URL" -f schema/002_campaigns_hub.sql
```

**Do not run against production without review — see PR notes.**

## Running locally

```bash
cd ingestion/campaign-hub
npm install           # or pnpm install
cp .env.example .env  # fill in your values

# dev (ts-node, hot reload)
npm run dev

# production build
npm run build && npm start
```

Health check: `GET http://localhost:8082/health` → `{"ok":true,"polling":false}`

## Deploying on Railway

1. Create a new Railway service pointing at this directory.
2. Set the env vars above in Railway's variable panel.
3. Set the start command to `npm run build && npm start`.
4. Railway cron is not required — the worker runs its own `setInterval`. If you want tighter cron control, set `POLL_INTERVAL_MS` to a large value and trigger the service externally via Railway's cron restart.

The `/health` endpoint lets Railway's health check confirm the process is alive.
