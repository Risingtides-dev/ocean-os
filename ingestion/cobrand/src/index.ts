/**
 * Ocean-OS Cobrand ingestion worker.
 *
 * Polls Cobrand for campaign performance data on a cron schedule.
 * For each active campaign slug in campaigns.bookings it fetches the latest
 * performance snapshot and writes:
 *   - cobrand.events   — raw append-only snapshot (skipped when nothing changed)
 *   - cobrand.campaigns — upserted current state
 *   - cobrand.posts    — upserted current state (delta-only, per snapshot hash)
 *
 * Cobrand has no webhooks so this worker is polling-only.
 *
 * Env:
 *   PORT                    — HTTP port, default 8082
 *   COBRAND_API_KEY         — Bearer token for Cobrand API
 *   COBRAND_API_BASE_URL    — e.g. https://app.cobrand.io/api/v1
 *   POLL_INTERVAL_MS        — poll cadence in ms, default 900000 (15 min)
 *   INTER_CAMPAIGN_DELAY_MS — sleep between campaigns to respect rate limits, default 500
 *   OCEAN_DATABASE_URL      — Postgres connection string (write role for cobrand + campaigns schemas)
 */

import crypto from "node:crypto";
import Fastify from "fastify";
import { Pool, type PoolClient } from "pg";
import { CobrandClient, sleep } from "./client.js";
import type { CobrandCampaign, CobrandPost } from "./client.js";

// ---- Config ----------------------------------------------------------------

const PORT = Number(process.env.PORT ?? 8082);
const DB_URL = process.env.OCEAN_DATABASE_URL;
const COBRAND_API_KEY = process.env.COBRAND_API_KEY ?? "";
const COBRAND_API_BASE_URL = process.env.COBRAND_API_BASE_URL ?? "";
const POLL_INTERVAL_MS = Number(process.env.POLL_INTERVAL_MS ?? 15 * 60 * 1000);
const INTER_CAMPAIGN_DELAY_MS = Number(process.env.INTER_CAMPAIGN_DELAY_MS ?? 500);

if (!COBRAND_API_KEY || !COBRAND_API_BASE_URL) {
  console.error(
    "[ingestion-cobrand] COBRAND_API_KEY and COBRAND_API_BASE_URL must both be set"
  );
  process.exit(1);
}

// ---- Infrastructure --------------------------------------------------------

const pool = new Pool({ connectionString: DB_URL });
const cobrand = new CobrandClient(COBRAND_API_BASE_URL, COBRAND_API_KEY);
const app = Fastify({ logger: true });

app.get("/health", async () => ({ ok: true }));

// ---- DB helpers ------------------------------------------------------------

async function getActiveCampaignSlugs(): Promise<string[]> {
  const result = await pool.query<{ campaign_slug: string }>(
    `SELECT DISTINCT campaign_slug FROM campaigns.bookings WHERE status = 'active'`
  );
  return result.rows.map((r) => r.campaign_slug);
}

async function getLastSnapshotHash(campaignId: string): Promise<string | null> {
  const result = await pool.query<{ hash: string }>(
    `SELECT payload->>'hash' AS hash
       FROM cobrand.events
      WHERE campaign_id = $1
        AND event_type = 'snapshot'
      ORDER BY observed_at DESC
      LIMIT 1`,
    [campaignId]
  );
  return result.rows[0]?.hash ?? null;
}

async function writeDelta(
  tx: PoolClient,
  campaign: CobrandCampaign,
  posts: CobrandPost[],
  hash: string
): Promise<void> {
  // 1. Upsert campaign current state
  await tx.query(
    `INSERT INTO cobrand.campaigns
         (campaign_id, slug, client, artist, status, total_views, total_posts, updated_at)
       VALUES ($1, $2, $3, $4, $5, $6, $7, now())
       ON CONFLICT (campaign_id) DO UPDATE SET
         slug        = EXCLUDED.slug,
         client      = EXCLUDED.client,
         artist      = EXCLUDED.artist,
         status      = EXCLUDED.status,
         total_views = EXCLUDED.total_views,
         total_posts = EXCLUDED.total_posts,
         updated_at  = now()`,
    [
      campaign.id,
      campaign.slug,
      campaign.client,
      campaign.artist,
      campaign.status,
      campaign.total_views,
      campaign.total_posts,
    ]
  );

  // 2. Append snapshot event (raw payload + hash for delta detection)
  await tx.query(
    `INSERT INTO cobrand.events (event_type, campaign_id, payload)
       VALUES ('snapshot', $1, $2)`,
    [campaign.id, JSON.stringify({ hash, campaign, posts })]
  );

  // 3. Upsert posts — only performance columns on conflict (immutable fields stay)
  for (const post of posts) {
    await tx.query(
      `INSERT INTO cobrand.posts
           (post_id, campaign_id, creator, url, views, engagement, posted_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now())
         ON CONFLICT (post_id) DO UPDATE SET
           views      = EXCLUDED.views,
           engagement = EXCLUDED.engagement,
           updated_at = now()`,
      [
        post.id,
        campaign.id,
        post.creator_handle,
        post.url,
        post.views,
        post.engagement,
        post.posted_at ?? null,
      ]
    );
  }
}

// ---- Snapshot hash ---------------------------------------------------------

function snapshotHash(campaign: CobrandCampaign, posts: CobrandPost[]): string {
  const canonical = {
    campaign,
    // Sort posts so hash is stable regardless of API return order
    posts: [...posts].sort((a, b) => a.id.localeCompare(b.id)),
  };
  return crypto
    .createHash("sha256")
    .update(JSON.stringify(canonical))
    .digest("hex");
}

// ---- Per-campaign poll -----------------------------------------------------

type PollResult = "updated" | "no_change" | "not_found";

async function pollCampaign(slug: string): Promise<PollResult> {
  const campaign = await cobrand.fetchCampaign(slug);
  if (!campaign) return "not_found";

  const posts = await cobrand.fetchCampaignPosts(campaign.id);
  const hash = snapshotHash(campaign, posts);
  const lastHash = await getLastSnapshotHash(campaign.id);

  if (hash === lastHash) return "no_change";

  const tx = await pool.connect();
  try {
    await tx.query("BEGIN");
    await writeDelta(tx, campaign, posts, hash);
    await tx.query("COMMIT");
  } catch (err) {
    await tx.query("ROLLBACK");
    throw err;
  } finally {
    tx.release();
  }

  return "updated";
}

// ---- Poll loop -------------------------------------------------------------

async function runPoll(): Promise<void> {
  app.log.info("[poll] starting");

  let slugs: string[];
  try {
    slugs = await getActiveCampaignSlugs();
  } catch (err) {
    app.log.error({ err }, "[poll] failed to read campaigns.bookings — skipping cycle");
    return;
  }

  if (slugs.length === 0) {
    app.log.info("[poll] no active campaigns in campaigns.bookings");
    return;
  }

  app.log.info({ count: slugs.length }, "[poll] campaigns to check");

  const summary = { updated: 0, no_change: 0, not_found: 0, error: 0 };

  for (const slug of slugs) {
    try {
      const result = await pollCampaign(slug);
      summary[result]++;
      app.log.info({ slug, result }, "[poll] campaign done");
    } catch (err) {
      summary.error++;
      app.log.error({ slug, err }, "[poll] campaign failed — continuing");
    }
    if (INTER_CAMPAIGN_DELAY_MS > 0) await sleep(INTER_CAMPAIGN_DELAY_MS);
  }

  app.log.info(summary, "[poll] cycle complete");
}

// ---- Startup ---------------------------------------------------------------

app
  .listen({ port: PORT, host: "0.0.0.0" })
  .then(() => {
    runPoll().catch((err) =>
      app.log.error({ err }, "[poll] initial poll failed")
    );
    setInterval(() => {
      runPoll().catch((err) =>
        app.log.error({ err }, "[poll] scheduled poll failed")
      );
    }, POLL_INTERVAL_MS);
  })
  .catch((err) => {
    app.log.error(err);
    process.exit(1);
  });
