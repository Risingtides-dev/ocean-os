# ingestion/cobrand

Polling worker for Cobrand campaign performance. Runs on a cron schedule, fetches snapshots from the Cobrand API, and writes to the `cobrand` schema in Ocean's Postgres.

## How it works

1. On startup (and every `POLL_INTERVAL_MS` thereafter), queries `campaigns.bookings WHERE status = 'active'` for the current list of live campaign slugs.
2. For each slug, calls the Cobrand API to get the campaign summary and its posts.
3. Hashes the response. If the hash matches the last recorded snapshot, no DB write is made (delta-only).
4. On a change: opens a transaction and writes:
   - `cobrand.events` — raw append-only snapshot (idempotent store of truth)
   - `cobrand.campaigns` — upserted current state
   - `cobrand.posts` — upserted current state (performance columns only on conflict)
5. Sleeps `INTER_CAMPAIGN_DELAY_MS` between campaigns to respect Cobrand's rate limits.
6. Retries 429 responses with exponential backoff (2 s, 4 s, 8 s, 16 s).

## Env vars

| Variable | Required | Default | Description |
|---|---|---|---|
| `COBRAND_API_KEY` | ✓ | — | Bearer token for the Cobrand API |
| `COBRAND_API_BASE_URL` | ✓ | — | e.g. `https://app.cobrand.io/api/v1` |
| `OCEAN_DATABASE_URL` | ✓ | — | Postgres connection string. Role must have write access to `cobrand.*` and read access to `campaigns.bookings`. |
| `PORT` | | `8082` | HTTP port for the `/health` endpoint |
| `POLL_INTERVAL_MS` | | `900000` | Poll cadence in milliseconds (default 15 min) |
| `INTER_CAMPAIGN_DELAY_MS` | | `500` | Sleep between campaign fetches (rate limit courtesy) |

## Running locally

```bash
cd ingestion/cobrand
npm install           # or pnpm install

# copy and fill in your values
cp .env.example .env

npm run dev
```

Check the health endpoint:

```bash
curl http://localhost:8082/health
# {"ok":true}
```

Watch logs — the worker will poll immediately on startup and log a summary line like:

```json
{"updated":1,"no_change":2,"not_found":0,"error":0,"msg":"[poll] cycle complete"}
```

## Checking results

```sql
-- Latest snapshot per campaign
SELECT campaign_id, observed_at, payload->>'hash'
  FROM cobrand.events
 WHERE event_type = 'snapshot'
 ORDER BY observed_at DESC
 LIMIT 20;

-- Current campaign state
SELECT * FROM cobrand.campaigns ORDER BY updated_at DESC;

-- Top posts by views
SELECT * FROM cobrand.posts ORDER BY views DESC LIMIT 20;
```

## Deploying

**Railway** (recommended — keeps it close to the Postgres instance):

1. Create a new Railway service pointing at `ingestion/cobrand/`.
2. Set the env vars above in the Railway service settings.
3. Railway will build with `npm run build && npm run start`.
4. Add a healthcheck on `GET /health`.

The worker doesn't need a public URL — it only makes outbound calls to Cobrand and inbound calls to Postgres.

## Adjusting for the actual Cobrand API

The client in `src/client.ts` assumes:
- `GET /campaigns/{slug}` → `CobrandCampaign`
- `GET /campaigns/{id}/posts` → `CobrandPost[]` or `{ posts: CobrandPost[] }`

If the real Cobrand API (or share pages) differ, update:
1. The endpoint paths in `client.ts` (`fetchCampaign`, `fetchCampaignPosts`)
2. The response types (`CobrandCampaign`, `CobrandPost`) to match actual field names
3. If share pages need HTML scraping instead of JSON, replace the `fetch` + JSON parse in `get()` with a scraper; the rest of the worker is unaffected.

Reference: `campaign_manager/services/cobrand.py` in the `risingtides-campaign-hub` repo has the existing integration patterns.

## Schema

Defined in `schema/000_init.sql` at the repo root. No schema changes needed — the `cobrand` schema tables were included in the initial migration:

- `cobrand.events` — append-only snapshot log
- `cobrand.campaigns` — current campaign state
- `cobrand.posts` — current post state
