/**
 * Ocean-OS — GitHub ingestion worker.
 *
 * Receives GitHub webhook events, validates the signature, writes to github.events.
 * Stateless. Deploy as a long-running HTTP server on Railway or as a CF Worker.
 */

import Fastify from "fastify";
import crypto from "node:crypto";
import pg from "pg";

const PORT = Number(process.env.PORT ?? 8080);
const SECRET = process.env.GITHUB_WEBHOOK_SECRET;
const DB_URL = process.env.OCEAN_DB_URL;

if (!SECRET) throw new Error("GITHUB_WEBHOOK_SECRET not set");
if (!DB_URL) throw new Error("OCEAN_DB_URL not set");

const pool = new pg.Pool({ connectionString: DB_URL });
const app = Fastify({ logger: true });

function verifySignature(body: string, signature: string | undefined): boolean {
  if (!signature) return false;
  const hmac = crypto.createHmac("sha256", SECRET!);
  hmac.update(body);
  const expected = `sha256=${hmac.digest("hex")}`;
  // timingSafeEqual requires equal-length buffers
  if (signature.length !== expected.length) return false;
  return crypto.timingSafeEqual(Buffer.from(signature), Buffer.from(expected));
}

app.post("/webhook", async (req, reply) => {
  const raw = JSON.stringify(req.body);
  const sig = req.headers["x-hub-signature-256"] as string | undefined;
  if (!verifySignature(raw, sig)) {
    reply.code(401).send({ ok: false, error: "bad signature" });
    return;
  }

  const eventType = req.headers["x-github-event"] as string;
  const deliveryId = req.headers["x-github-delivery"] as string;
  const payload = req.body as Record<string, unknown>;
  const repo = (payload.repository as Record<string, unknown> | undefined)?.full_name as string | undefined;
  const actor =
    (payload.sender as Record<string, unknown> | undefined)?.login as string | undefined;

  await pool.query(
    `insert into github.events (external_id, type, repo, actor, occurred_at, payload)
       values ($1, $2, $3, $4, now(), $5)
       on conflict (external_id) do nothing`,
    [deliveryId, eventType, repo ?? "unknown", actor ?? null, payload]
  );

  reply.send({ ok: true });
});

app.get("/health", async () => ({ ok: true }));

app.listen({ port: PORT, host: "0.0.0.0" }).catch((err) => {
  app.log.error(err);
  process.exit(1);
});
