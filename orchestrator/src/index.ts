/**
 * Ocean-OS Orchestrator
 *
 * Accepts inbound webhook events from GitHub and Slack, persists them as task
 * rows, and exposes a claim endpoint that twin bridges call when they're ready
 * to work. Dispatch routing is stubbed — tasks are logged but not forwarded yet.
 *
 * Env:
 *   PORT                  — default 8082
 *   OCEAN_DATABASE_URL    — Postgres connection string (write role for orchestrator schema)
 *   ORCHESTRATOR_SECRET   — Bearer token required on POST /events (optional for local dev)
 */

import Fastify from "fastify";
import { Pool } from "pg";
import { z } from "zod";

const PORT = Number(process.env.PORT ?? 8082);
const DB_URL = process.env.OCEAN_DATABASE_URL;
const ORCHESTRATOR_SECRET = process.env.ORCHESTRATOR_SECRET ?? "";

const pool = new Pool({ connectionString: DB_URL });

const app = Fastify({ logger: true });

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const WebhookEnvelopeSchema = z.object({
  source: z.enum(["github", "slack"]),
  event_type: z.string().min(1),
  source_ref: z.string().optional(),
  payload: z.record(z.unknown()),
});

const ClaimBodySchema = z.object({
  twin_id: z.string().min(1),
});

// ---------------------------------------------------------------------------
// Auth helper — skipped if no secret is configured (local dev)
// ---------------------------------------------------------------------------

function checkBearer(req: Parameters<typeof app.get>[1] extends undefined ? never : Parameters<typeof app.addHook>[1] extends never ? never : import("fastify").FastifyRequest, secret: string): boolean {
  if (!secret) return true;
  const auth = req.headers.authorization ?? "";
  const token = auth.startsWith("Bearer ") ? auth.slice(7) : "";
  return token === secret;
}

// ---------------------------------------------------------------------------
// POST /events — accept an inbound webhook envelope, create a task row
// ---------------------------------------------------------------------------

app.post("/events", async (req, reply) => {
  if (!checkBearer(req, ORCHESTRATOR_SECRET)) {
    return reply.code(401).send({ error: "unauthorized" });
  }

  const parsed = WebhookEnvelopeSchema.safeParse(req.body);
  if (!parsed.success) {
    return reply.code(400).send({ error: "invalid envelope", details: parsed.error.flatten() });
  }

  const { source, event_type, source_ref, payload } = parsed.data;

  const result = await pool.query<{ id: string }>(
    `INSERT INTO orchestrator.tasks (source, event_type, source_ref, payload)
     VALUES ($1, $2, $3, $4)
     RETURNING id`,
    [source, event_type, source_ref ?? null, payload]
  );

  const taskId = result.rows[0].id;

  // Stub: log the task instead of routing it anywhere.
  req.log.info({ taskId, source, event_type }, "task created — dispatch stubbed");

  return reply.code(202).send({ ok: true, task_id: taskId });
});

// ---------------------------------------------------------------------------
// POST /tasks/:id/claim — a twin bridge calls this when it's ready to work
// ---------------------------------------------------------------------------

app.post<{ Params: { id: string } }>("/tasks/:id/claim", async (req, reply) => {
  if (!checkBearer(req, ORCHESTRATOR_SECRET)) {
    return reply.code(401).send({ error: "unauthorized" });
  }

  const { id } = req.params;

  const parsed = ClaimBodySchema.safeParse(req.body);
  if (!parsed.success) {
    return reply.code(400).send({ error: "invalid body", details: parsed.error.flatten() });
  }

  const { twin_id } = parsed.data;

  // Verify the task exists and is still claimable.
  const taskResult = await pool.query<{ id: string; status: string; payload: unknown }>(
    `SELECT id, status, payload FROM orchestrator.tasks WHERE id = $1`,
    [id]
  );

  if (taskResult.rowCount === 0) {
    return reply.code(404).send({ error: "task not found" });
  }

  const task = taskResult.rows[0];
  if (task.status !== "pending") {
    return reply.code(409).send({ error: "task already claimed", status: task.status });
  }

  // Claim atomically: update task status, insert dispatch row.
  await pool.query("BEGIN");
  try {
    await pool.query(
      `UPDATE orchestrator.tasks SET status = 'claimed', updated_at = now() WHERE id = $1`,
      [id]
    );
    const dispatchResult = await pool.query<{ id: string }>(
      `INSERT INTO orchestrator.dispatches (task_id, twin_id) VALUES ($1, $2) RETURNING id`,
      [id, twin_id]
    );
    await pool.query("COMMIT");

    const dispatchId = dispatchResult.rows[0].id;
    req.log.info({ taskId: id, dispatchId, twin_id }, "task claimed");

    return reply.code(200).send({
      ok: true,
      dispatch_id: dispatchId,
      task: { id: task.id, payload: task.payload },
    });
  } catch (err) {
    await pool.query("ROLLBACK");
    req.log.error({ err }, "claim failed");
    return reply.code(500).send({ error: "claim failed" });
  }
});

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

app.get("/health", async () => ({ ok: true }));

// ---------------------------------------------------------------------------

app.listen({ port: PORT, host: "0.0.0.0" }).catch((err) => {
  app.log.error(err);
  process.exit(1);
});
