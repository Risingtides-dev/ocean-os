/**
 * Ocean-OS Campaign Hub ingestion worker.
 *
 * Polling worker — Campaign Hub does not emit webhooks today.
 * Runs on a configurable interval, diffs against last-seen state stored in
 * campaigns.poll_cursor, and writes to:
 *   campaigns.events     — append-only event log (idempotent on external_id)
 *   campaigns.bookings   — upserted current state
 *   campaigns.creators   — upserted current state
 *   campaigns.payments   — upserted current state
 *
 * Env:
 *   PORT                       — HTTP health endpoint port, default 8082
 *   CAMPAIGN_HUB_API_URL       — base URL of Campaign Hub Flask app on Railway (no trailing slash)
 *   CAMPAIGN_HUB_SERVICE_TOKEN — bearer token for Campaign Hub service auth
 *   OCEAN_DATABASE_URL         — Postgres connection string (write role for campaigns schema)
 *   POLL_INTERVAL_MS           — poll frequency in ms, default 300_000 (5 min)
 */

import Fastify from "fastify";
import { Pool } from "pg";
import { z } from "zod";

const PORT = Number(process.env.PORT ?? 8082);
const HUB_URL = (process.env.CAMPAIGN_HUB_API_URL ?? "").replace(/\/$/, "");
const HUB_TOKEN = process.env.CAMPAIGN_HUB_SERVICE_TOKEN ?? "";
const DB_URL = process.env.OCEAN_DATABASE_URL;
const POLL_INTERVAL_MS = Number(process.env.POLL_INTERVAL_MS ?? 300_000);

const pool = new Pool({ connectionString: DB_URL });

// ---------------------------------------------------------------------------
// API response shapes — matches the contract documented in README.md
// ---------------------------------------------------------------------------

function pagedOf<T extends z.ZodTypeAny>(item: T) {
  return z.object({
    data: z.array(item),
    next_cursor: z.string().nullable().optional(),
  });
}

const CampaignRow = z.object({
  id: z.string(),
  slug: z.string(),
  client: z.string().nullish(),
  artist: z.string().nullish(),
  status: z.string(),
  budget_cents: z.number().int().nullish(),
  updated_at: z.string(),
});

const BookingRow = z.object({
  id: z.string(),
  campaign_slug: z.string(),
  creator_handle: z.string(),
  platform: z.string(),
  rate_cents: z.number().int().nullish(),
  posts_owed: z.number().int().nullish(),
  status: z.string(),
  updated_at: z.string(),
});

const CreatorRow = z.object({
  handle: z.string(),
  platform: z.string(),
  metadata: z.record(z.unknown()).nullish(),
  updated_at: z.string(),
});

const PaymentRow = z.object({
  id: z.string(),
  campaign_slug: z.string(),
  creator_handle: z.string(),
  amount_cents: z.number().int(),
  status: z.string(),
  paid_at: z.string().nullish(),
  occurred_at: z.string(),
  updated_at: z.string(),
});

// ---------------------------------------------------------------------------
// Cursor helpers
// ---------------------------------------------------------------------------

async function getCursor(entityType: string): Promise<Date> {
  const res = await pool.query<{ last_synced_at: Date }>(
    `SELECT last_synced_at FROM campaigns.poll_cursor WHERE entity_type = $1`,
    [entityType],
  );
  return res.rows[0]?.last_synced_at ?? new Date(0);
}

async function advanceCursor(entityType: string, to: Date): Promise<void> {
  await pool.query(
    `INSERT INTO campaigns.poll_cursor (entity_type, last_synced_at)
     VALUES ($1, $2)
     ON CONFLICT (entity_type)
     DO UPDATE SET last_synced_at = EXCLUDED.last_synced_at
     WHERE campaigns.poll_cursor.last_synced_at < EXCLUDED.last_synced_at`,
    [entityType, to],
  );
}

// ---------------------------------------------------------------------------
// HTTP helper
// ---------------------------------------------------------------------------

async function hubFetch(path: string, params: Record<string, string>): Promise<unknown> {
  const url = new URL(`${HUB_URL}${path}`);
  for (const [k, v] of Object.entries(params)) url.searchParams.set(k, v);
  const res = await fetch(url.toString(), {
    headers: {
      Authorization: `Bearer ${HUB_TOKEN}`,
      Accept: "application/json",
    },
    signal: AbortSignal.timeout(30_000),
  });
  if (!res.ok) throw new Error(`Campaign Hub ${path} → HTTP ${res.status}`);
  return res.json();
}

// ---------------------------------------------------------------------------
// Entity syncs
// ---------------------------------------------------------------------------

async function syncCampaigns(since: Date): Promise<Date> {
  let cursor: string | undefined;
  let latest = since;

  do {
    const params: Record<string, string> = { updated_since: since.toISOString(), per_page: "100" };
    if (cursor) params.cursor = cursor;

    const raw = await hubFetch("/api/campaigns", params);
    const page = pagedOf(CampaignRow).parse(raw);

    for (const c of page.data) {
      const externalId = `campaign:${c.id}:${c.updated_at}`;
      await pool.query(
        `INSERT INTO campaigns.events (external_id, type, campaign_slug, payload, occurred_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (external_id) DO NOTHING`,
        [externalId, "campaign_snapshot", c.slug, c, new Date(c.updated_at)],
      );
      const ts = new Date(c.updated_at);
      if (ts > latest) latest = ts;
    }

    cursor = page.next_cursor ?? undefined;
  } while (cursor);

  return latest;
}

async function syncBookings(since: Date): Promise<Date> {
  let cursor: string | undefined;
  let latest = since;

  do {
    const params: Record<string, string> = { updated_since: since.toISOString(), per_page: "100" };
    if (cursor) params.cursor = cursor;

    const raw = await hubFetch("/api/bookings", params);
    const page = pagedOf(BookingRow).parse(raw);

    for (const b of page.data) {
      const externalId = `booking:${b.id}:${b.updated_at}`;

      await pool.query(
        `INSERT INTO campaigns.bookings
           (hub_id, campaign_slug, creator, rate_cents, posts_owed, status, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (hub_id) DO UPDATE SET
           campaign_slug = EXCLUDED.campaign_slug,
           creator       = EXCLUDED.creator,
           rate_cents    = EXCLUDED.rate_cents,
           posts_owed    = EXCLUDED.posts_owed,
           status        = EXCLUDED.status,
           updated_at    = EXCLUDED.updated_at
         WHERE campaigns.bookings.updated_at < EXCLUDED.updated_at`,
        [b.id, b.campaign_slug, b.creator_handle, b.rate_cents ?? null, b.posts_owed ?? null, b.status, new Date(b.updated_at)],
      );

      await pool.query(
        `INSERT INTO campaigns.events (external_id, type, campaign_slug, payload, occurred_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (external_id) DO NOTHING`,
        [externalId, "booking_snapshot", b.campaign_slug, b, new Date(b.updated_at)],
      );

      const ts = new Date(b.updated_at);
      if (ts > latest) latest = ts;
    }

    cursor = page.next_cursor ?? undefined;
  } while (cursor);

  return latest;
}

async function syncCreators(since: Date): Promise<Date> {
  let cursor: string | undefined;
  let latest = since;

  do {
    const params: Record<string, string> = { updated_since: since.toISOString(), per_page: "100" };
    if (cursor) params.cursor = cursor;

    const raw = await hubFetch("/api/creators", params);
    const page = pagedOf(CreatorRow).parse(raw);

    for (const c of page.data) {
      const externalId = `creator:${c.handle}:${c.updated_at}`;

      await pool.query(
        `INSERT INTO campaigns.creators (handle, platform, metadata, updated_at)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (handle) DO UPDATE SET
           platform   = EXCLUDED.platform,
           metadata   = EXCLUDED.metadata,
           updated_at = EXCLUDED.updated_at
         WHERE campaigns.creators.updated_at < EXCLUDED.updated_at`,
        [c.handle, c.platform, c.metadata ?? {}, new Date(c.updated_at)],
      );

      await pool.query(
        `INSERT INTO campaigns.events (external_id, type, campaign_slug, payload, occurred_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (external_id) DO NOTHING`,
        [externalId, "creator_snapshot", null, c, new Date(c.updated_at)],
      );

      const ts = new Date(c.updated_at);
      if (ts > latest) latest = ts;
    }

    cursor = page.next_cursor ?? undefined;
  } while (cursor);

  return latest;
}

async function syncPayments(since: Date): Promise<Date> {
  let cursor: string | undefined;
  let latest = since;

  do {
    const params: Record<string, string> = { updated_since: since.toISOString(), per_page: "100" };
    if (cursor) params.cursor = cursor;

    const raw = await hubFetch("/api/payments", params);
    const page = pagedOf(PaymentRow).parse(raw);

    for (const p of page.data) {
      const externalId = `payment:${p.id}:${p.updated_at}`;

      await pool.query(
        `INSERT INTO campaigns.payments
           (hub_id, campaign_slug, creator, amount_cents, status, paid_at, occurred_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)
         ON CONFLICT (hub_id) DO UPDATE SET
           campaign_slug = EXCLUDED.campaign_slug,
           creator       = EXCLUDED.creator,
           amount_cents  = EXCLUDED.amount_cents,
           status        = EXCLUDED.status,
           paid_at       = EXCLUDED.paid_at`,
        [p.id, p.campaign_slug, p.creator_handle, p.amount_cents, p.status, p.paid_at ? new Date(p.paid_at) : null, new Date(p.occurred_at)],
      );

      await pool.query(
        `INSERT INTO campaigns.events (external_id, type, campaign_slug, payload, occurred_at)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (external_id) DO NOTHING`,
        [externalId, "payment_snapshot", p.campaign_slug, p, new Date(p.occurred_at)],
      );

      const ts = new Date(p.updated_at);
      if (ts > latest) latest = ts;
    }

    cursor = page.next_cursor ?? undefined;
  } while (cursor);

  return latest;
}

// ---------------------------------------------------------------------------
// Poll loop
// ---------------------------------------------------------------------------

let polling = false;

async function poll(): Promise<void> {
  if (polling) {
    log("WARN", "previous poll still running — skipping");
    return;
  }
  if (!HUB_URL || !HUB_TOKEN) {
    log("WARN", "poll skipped — CAMPAIGN_HUB_API_URL or CAMPAIGN_HUB_SERVICE_TOKEN not set");
    return;
  }

  polling = true;
  const start = Date.now();
  log("INFO", "poll started");

  try {
    const [campaignSince, bookingSince, creatorSince, paymentSince] = await Promise.all([
      getCursor("campaigns"),
      getCursor("bookings"),
      getCursor("creators"),
      getCursor("payments"),
    ]);

    const results = await Promise.allSettled([
      syncCampaigns(campaignSince).then((ts) => advanceCursor("campaigns", ts)),
      syncBookings(bookingSince).then((ts) => advanceCursor("bookings", ts)),
      syncCreators(creatorSince).then((ts) => advanceCursor("creators", ts)),
      syncPayments(paymentSince).then((ts) => advanceCursor("payments", ts)),
    ]);

    const entities = ["campaigns", "bookings", "creators", "payments"] as const;
    for (const [i, r] of results.entries()) {
      if (r.status === "rejected") {
        log("ERROR", `${entities[i]} sync failed: ${(r.reason as Error).message}`);
      }
    }

    log("INFO", `poll complete in ${Date.now() - start}ms`);
  } catch (err) {
    log("ERROR", `poll loop error: ${(err as Error).message}`);
  } finally {
    polling = false;
  }
}

function log(level: "INFO" | "WARN" | "ERROR", msg: string): void {
  process.stdout.write(
    JSON.stringify({ level, msg, ts: new Date().toISOString(), service: "ingestion-campaign-hub" }) + "\n",
  );
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

const app = Fastify({ logger: false });

app.get("/health", async () => ({ ok: true, polling }));

app.listen({ port: PORT, host: "0.0.0.0" }).then(() => {
  log("INFO", `health endpoint listening on :${PORT}`);
  poll(); // run immediately on startup
  setInterval(poll, POLL_INTERVAL_MS);
}).catch((err: Error) => {
  log("ERROR", `failed to start server: ${err.message}`);
  process.exit(1);
});
