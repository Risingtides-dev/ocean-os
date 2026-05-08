/**
 * Ocean-OS Content Lab ingestion worker.
 *
 * Two intake modes (not mutually exclusive):
 *
 *   Webhook (preferred)
 *     Content Lab POSTs to POST /webhook when job state changes.
 *     Requires a thin /api/internal/events endpoint on the Lab side — coordinate
 *     with smaths before enabling. Validates an optional HMAC-SHA256 header.
 *
 *   Polling (fallback)
 *     Enabled when POLL_INTERVAL_MS is set. Calls CONTENT_LAB_API_URL/api/jobs
 *     with a `since` cursor and ingests deltas. Assumes the response shape
 *     documented in README.md — confirm with smaths before turning on.
 *
 * Writes to:
 *   content.events        append-only event log (idempotent on external_id)
 *   content.jobs          upserted current state (idempotent on lab_job_id)
 *   content.distributions upserted distribution state (idempotent on lab_distribution_id)
 *
 * Env vars — see README.md.
 */

import crypto from "node:crypto";
import Fastify from "fastify";
import { Pool } from "pg";

const PORT = Number(process.env.PORT ?? 8082);
const DB_URL = process.env.OCEAN_DATABASE_URL;
const WEBHOOK_SECRET = process.env.CONTENT_LAB_WEBHOOK_SECRET ?? "";
const LAB_API_URL = (process.env.CONTENT_LAB_API_URL ?? "").replace(/\/$/, "");
const LAB_API_KEY = process.env.CONTENT_LAB_API_KEY ?? "";
const POLL_INTERVAL_MS = process.env.POLL_INTERVAL_MS
  ? Number(process.env.POLL_INTERVAL_MS)
  : null;

if (!DB_URL) {
  console.error("[ingestion-content-lab] OCEAN_DATABASE_URL is required");
  process.exit(1);
}

const pool = new Pool({ connectionString: DB_URL });
const app = Fastify({ logger: true });

// ---------------------------------------------------------------------------
// Webhook intake
// ---------------------------------------------------------------------------

app.post("/webhook", async (req, reply) => {
  if (WEBHOOK_SECRET) {
    const sig = req.headers["x-content-lab-signature"] as string | undefined;
    if (!sig || !verifyHmac(JSON.stringify(req.body), sig, WEBHOOK_SECRET)) {
      return reply.code(401).send({ error: "invalid signature" });
    }
  }

  try {
    await ingestEvent(req.body as LabEvent);
  } catch (err) {
    req.log.error({ err }, "ingestion failed");
    return reply.code(500).send({ error: "ingestion failed" });
  }

  return reply.code(202).send({ ok: true });
});

app.get("/health", async () => ({ ok: true }));

// ---------------------------------------------------------------------------
// Start server + optional polling loop
// ---------------------------------------------------------------------------

app.listen({ port: PORT, host: "0.0.0.0" }).catch((err) => {
  app.log.error(err);
  process.exit(1);
});

if (POLL_INTERVAL_MS) {
  if (!LAB_API_URL) {
    app.log.warn("POLL_INTERVAL_MS set but CONTENT_LAB_API_URL is empty — polling disabled");
  } else {
    app.log.info({ interval: POLL_INTERVAL_MS }, "polling mode enabled");
    pollContentLab().catch((err) => app.log.error({ err }, "initial poll failed"));
    setInterval(() => {
      pollContentLab().catch((err) => app.log.error({ err }, "poll failed"));
    }, POLL_INTERVAL_MS);
  }
}

// ---------------------------------------------------------------------------
// Core ingestion
// ---------------------------------------------------------------------------

interface LabEvent {
  event?: string;
  job_id?: string;
  job_type?: string;
  status?: string;
  inputs?: unknown;
  outputs?: unknown;
  project_id?: string;
  occurred_at?: string;
  distribution_id?: string;
  surface?: string;
  target?: string;
  [key: string]: unknown;
}

async function ingestEvent(event: LabEvent): Promise<void> {
  const occurredAt = event.occurred_at ?? new Date().toISOString();
  const externalId = deriveExternalId(event);
  const type = event.event ?? "unknown";

  // Append-only event log — idempotent on external_id.
  const inserted = await pool.query(
    `INSERT INTO content.events (external_id, type, project_id, payload, occurred_at)
     VALUES ($1, $2, $3, $4, $5)
     ON CONFLICT (external_id) DO NOTHING
     RETURNING id`,
    [externalId, type, event.project_id ?? null, event, occurredAt]
  );

  // Already ingested — skip materialized upserts.
  if (inserted.rows.length === 0) return;

  if (event.job_id && event.job_type) {
    await upsertJob(event);
  }

  if (event.distribution_id && event.job_id) {
    await upsertDistribution(event);
  }
}

function deriveExternalId(event: LabEvent): string {
  if (event.distribution_id) {
    return `distribution:${event.distribution_id}:${event.event ?? "event"}`;
  }
  if (event.job_id) {
    return `job:${event.job_id}:${event.event ?? "event"}`;
  }
  // No stable id from Lab — fall back to a hash of the payload.
  return `raw:${crypto.createHash("sha256").update(JSON.stringify(event)).digest("hex").slice(0, 24)}`;
}

async function upsertJob(event: LabEvent): Promise<void> {
  const terminal = event.status === "completed" || event.status === "failed";
  const finishedAt = terminal ? (event.occurred_at ?? new Date().toISOString()) : null;

  await pool.query(
    `INSERT INTO content.jobs (lab_job_id, job_type, status, inputs, outputs, finished_at)
     VALUES ($1, $2, $3, $4, $5, $6)
     ON CONFLICT (lab_job_id) DO UPDATE SET
       status     = EXCLUDED.status,
       outputs    = COALESCE(EXCLUDED.outputs,     content.jobs.outputs),
       finished_at = COALESCE(EXCLUDED.finished_at, content.jobs.finished_at)`,
    [
      event.job_id,
      event.job_type,
      event.status ?? "unknown",
      event.inputs ?? null,
      event.outputs ?? null,
      finishedAt,
    ]
  );
}

async function upsertDistribution(event: LabEvent): Promise<void> {
  const jobRow = await pool.query(
    `SELECT id FROM content.jobs WHERE lab_job_id = $1`,
    [event.job_id]
  );
  const jobId = jobRow.rows[0]?.id ?? null;

  await pool.query(
    `INSERT INTO content.distributions (lab_distribution_id, job_id, surface, target, status)
     VALUES ($1, $2, $3, $4, $5)
     ON CONFLICT (lab_distribution_id) DO UPDATE SET
       status      = EXCLUDED.status,
       observed_at = now()`,
    [
      event.distribution_id,
      jobId,
      event.surface ?? "unknown",
      event.target ?? "unknown",
      event.status ?? "unknown",
    ]
  );
}

// ---------------------------------------------------------------------------
// Polling
// ---------------------------------------------------------------------------

// Cursor: ingest everything that changed since this timestamp.
let lastPolledAt = new Date(Date.now() - 5 * 60 * 1000).toISOString();

async function pollContentLab(): Promise<void> {
  app.log.info({ since: lastPolledAt }, "polling Content Lab for jobs");

  const url = `${LAB_API_URL}/api/jobs?since=${encodeURIComponent(lastPolledAt)}&limit=100`;
  const res = await fetch(url, {
    headers: LAB_API_KEY ? { Authorization: `Bearer ${LAB_API_KEY}` } : {},
  });

  if (!res.ok) {
    app.log.warn({ status: res.status, url }, "poll request failed — skipping cycle");
    return;
  }

  const data = (await res.json()) as { jobs?: LabJobSnapshot[] };
  const jobs = data.jobs ?? [];

  for (const job of jobs) {
    await ingestEvent({
      event: "job.polled",
      job_id: job.id,
      job_type: job.type,
      status: job.status,
      inputs: job.inputs,
      outputs: job.outputs,
      project_id: job.project_id,
      occurred_at: job.updated_at ?? job.created_at,
    });

    for (const dist of job.distributions ?? []) {
      await ingestEvent({
        event: "distribution.polled",
        distribution_id: dist.id,
        job_id: job.id,
        surface: dist.surface,
        target: dist.target,
        status: dist.status,
        occurred_at: dist.updated_at,
      });
    }
  }

  if (jobs.length > 0) {
    app.log.info({ count: jobs.length }, "poll cycle complete");
  }

  lastPolledAt = new Date().toISOString();
}

interface LabJobSnapshot {
  id: string;
  type: string;
  status: string;
  inputs?: unknown;
  outputs?: unknown;
  project_id?: string;
  created_at: string;
  updated_at?: string;
  distributions?: LabDistributionSnapshot[];
}

interface LabDistributionSnapshot {
  id: string;
  surface: string;
  target: string;
  status: string;
  updated_at?: string;
}

// ---------------------------------------------------------------------------
// HMAC helpers
// ---------------------------------------------------------------------------

function verifyHmac(body: string, sigHeader: string, secret: string): boolean {
  const hmac = crypto.createHmac("sha256", secret);
  hmac.update(body);
  const expected = `sha256=${hmac.digest("hex")}`;
  if (expected.length !== sigHeader.length) return false;
  return crypto.timingSafeEqual(Buffer.from(expected), Buffer.from(sigHeader));
}
