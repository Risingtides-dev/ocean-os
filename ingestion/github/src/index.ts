/**
 * Ocean-OS GitHub ingestion worker — proof of concept.
 *
 * Receives GitHub webhook events, validates the X-Hub-Signature-256 header,
 * writes the raw payload to github.events (idempotent on X-GitHub-Delivery),
 * and updates github.deploys for deployment_status events.
 *
 * Env:
 *   PORT                  — default 8081
 *   GITHUB_WEBHOOK_SECRET — shared secret configured on the GitHub App / repo webhook
 *   OCEAN_DATABASE_URL    — Postgres connection string (write role for github schema)
 */

import crypto from "node:crypto";
import Fastify from "fastify";
import { Pool } from "pg";

const PORT = Number(process.env.PORT ?? 8081);
const SECRET = process.env.GITHUB_WEBHOOK_SECRET ?? "";
const DB_URL = process.env.OCEAN_DATABASE_URL;

if (!SECRET) {
  console.warn(
    "[ingestion-github] GITHUB_WEBHOOK_SECRET is empty — refusing to accept events"
  );
}

const pool = new Pool({ connectionString: DB_URL });

const app = Fastify({ logger: true });

app.post("/webhook", async (req, reply) => {
  const sig = req.headers["x-hub-signature-256"];
  const delivery = req.headers["x-github-delivery"];
  const event = req.headers["x-github-event"];
  const raw = JSON.stringify(req.body);

  if (typeof sig !== "string" || typeof delivery !== "string" || typeof event !== "string") {
    return reply.code(400).send({ error: "missing required headers" });
  }
  if (!verifySignature(raw, sig, SECRET)) {
    return reply.code(401).send({ error: "invalid signature" });
  }

  const payload = req.body as Record<string, unknown>;
  const repo =
    (payload?.repository as { full_name?: string } | undefined)?.full_name ?? "unknown";
  const actor =
    (payload?.sender as { login?: string } | undefined)?.login ?? null;

  try {
    // Idempotent insert. Conflict on delivery id = already processed.
    await pool.query(
      `INSERT INTO github.events (delivery_id, event_type, repo, actor, payload)
       VALUES ($1, $2, $3, $4, $5)
       ON CONFLICT (delivery_id) DO NOTHING`,
      [delivery, event, repo, actor, payload]
    );

    if (event === "deployment_status") {
      await upsertDeploy(payload, delivery);
    }
  } catch (err) {
    req.log.error({ err }, "ingestion failed");
    return reply.code(500).send({ error: "ingestion failed" });
  }

  return reply.code(202).send({ ok: true, delivery });
});

app.get("/health", async () => ({ ok: true }));

app.listen({ port: PORT, host: "0.0.0.0" }).catch((err) => {
  app.log.error(err);
  process.exit(1);
});

// ----------------------------------------------------------------------------

function verifySignature(rawBody: string, sigHeader: string, secret: string): boolean {
  if (!secret) return false;
  const hmac = crypto.createHmac("sha256", secret);
  hmac.update(rawBody);
  const expected = `sha256=${hmac.digest("hex")}`;
  // timingSafeEqual on equal-length buffers
  if (expected.length !== sigHeader.length) return false;
  return crypto.timingSafeEqual(Buffer.from(expected), Buffer.from(sigHeader));
}

async function upsertDeploy(
  payload: Record<string, unknown>,
  deliveryId: string
): Promise<void> {
  const deployment = (payload.deployment as Record<string, unknown> | undefined) ?? {};
  const status = (payload.deployment_status as Record<string, unknown> | undefined) ?? {};
  const repo =
    (payload.repository as { full_name?: string } | undefined)?.full_name ?? "unknown";

  const sourceEvent = await pool.query(
    `SELECT id FROM github.events WHERE delivery_id = $1`,
    [deliveryId]
  );
  const sourceEventId = sourceEvent.rows[0]?.id ?? null;

  await pool.query(
    `INSERT INTO github.deploys
       (repo, environment, status, sha, triggered_by, started_at, finished_at, source_event_id)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)`,
    [
      repo,
      (deployment.environment as string) ?? "unknown",
      (status.state as string) ?? "unknown",
      (deployment.sha as string) ?? null,
      ((deployment.creator as { login?: string } | undefined)?.login) ?? null,
      (deployment.created_at as string) ?? new Date().toISOString(),
      status.state === "success" || status.state === "failure"
        ? new Date().toISOString()
        : null,
      sourceEventId,
    ]
  );
}
