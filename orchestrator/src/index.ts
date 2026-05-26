/**
 * Ocean-OS Orchestrator
 *
 * Two endpoint groups on one Fastify app, sharing the orchestrator.tasks table:
 *
 *   1. Dispatch surface (PR #13)
 *      Accepts inbound webhook envelopes from GitHub and Slack, persists them
 *      as task rows, and lets a twin bridge claim a task to work on it.
 *
 *   2. Agent-task surface (PR #15)
 *      Tracks long-running tasks an agent is executing on behalf of a human.
 *      Counts rounds, detects stalls (max_rounds, repeated failures), and
 *      escalates to the human owner over Slack DM.
 *
 * Rows from the dispatch surface have human_owner = NULL and are ignored by
 * the stall detector. Rows from the agent-task surface have human_owner set.
 *
 * Env:
 *   PORT                  — default 8082
 *   OCEAN_DATABASE_URL    — Postgres connection string (write role for orchestrator schema)
 *   ORCHESTRATOR_SECRET   — Bearer token required on dispatch endpoints (optional for local dev)
 *   SLACK_BOT_TOKEN       — Bot token with chat:write scope, used to DM humans on escalation
 *
 * Endpoints:
 *   GET  /health
 *   POST /events                       — dispatch: accept webhook envelope, create task
 *   POST /tasks/:id/claim              — dispatch: twin bridge claims a task
 *   POST /tasks                        — agent-task: create
 *   GET  /tasks/:id                    — agent-task: read
 *   POST /tasks/:id/progress           — agent-task: tick a round (or record a failure)
 *   POST /tasks/:id/complete           — agent-task: mark completed
 *   POST /tasks/:id/simulate-stall     — agent-task: force escalation immediately (testing)
 *   GET  /queue/:slack_user_id         — agent-task: stalled + escalated tasks for a human
 */

import Fastify from "fastify";
import { Pool } from "pg";
import { z } from "zod";
import { detectAndEscalate, escalateTask, type Task } from "./escalate.js";

const PORT = Number(process.env.PORT ?? 8082);
const DB_URL = process.env.OCEAN_DATABASE_URL;
const ORCHESTRATOR_SECRET = process.env.ORCHESTRATOR_SECRET ?? "";

const pool = new Pool({ connectionString: DB_URL });
const app = Fastify({ logger: true });

// ---------------------------------------------------------------------------
// Stall detector — runs every 60 s, ignores dispatch-only rows
// ---------------------------------------------------------------------------

setInterval(() => {
  detectAndEscalate(pool, (msg) => app.log.info(msg)).catch((err) => {
    app.log.error({ err }, "stall detector error");
  });
}, 60_000);

// ---------------------------------------------------------------------------
// Auth helper — skipped if no secret is configured (local dev)
// ---------------------------------------------------------------------------

function checkBearer(req: { headers: { authorization?: string } }, secret: string): boolean {
  if (!secret) return true;
  const auth = req.headers.authorization ?? "";
  const token = auth.startsWith("Bearer ") ? auth.slice(7) : "";
  return token === secret;
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

app.get("/health", async () => ({ ok: true }));

// ===========================================================================
// Dispatch surface — webhook ingest + twin claim
// ===========================================================================

const WebhookEnvelopeSchema = z.object({
  source: z.enum(["github", "slack"]),
  event_type: z.string().min(1),
  source_ref: z.string().optional(),
  payload: z.record(z.unknown()),
});

const ClaimBodySchema = z.object({
  twin_id: z.string().min(1),
});

// POST /events — accept an inbound webhook envelope, create a task row
app.post("/events", async (req, reply) => {
  if (!checkBearer(req, ORCHESTRATOR_SECRET)) {
    return reply.code(401).send({ error: "unauthorized" });
  }

  const parsed = WebhookEnvelopeSchema.safeParse(req.body);
  if (!parsed.success) {
    return reply.code(400).send({ error: "invalid envelope", details: parsed.error.flatten() });
  }

  const { source, event_type, source_ref, payload } = parsed.data;

  let taskId: string;
  try {
    const result = await pool.query<{ id: string }>(
      `INSERT INTO orchestrator.tasks (source, event_type, source_ref, payload, status)
       VALUES ($1, $2, $3, $4, 'pending')
       RETURNING id`,
      [source, event_type, source_ref ?? null, payload]
    );
    taskId = result.rows[0].id;
  } catch (err) {
    req.log.error({ err }, "ingestion failed");
    return reply.code(500).send({ error: "ingestion failed" });
  }

  req.log.info({ taskId, source, event_type }, "task created — dispatch stubbed");
  return reply.code(202).send({ ok: true, task_id: taskId });
});

// POST /tasks/:id/claim — a twin bridge calls this when it's ready to work
app.post<{ Params: { id: string } }>("/tasks/:id/claim", async (req, reply) => {
  if (!checkBearer(req, ORCHESTRATOR_SECRET)) {
    return reply.code(401).send({ error: "unauthorized" });
  }

  const { id } = req.params;
  if (!UUID_RE.test(id)) {
    return reply.code(400).send({ error: "invalid task id" });
  }

  const parsed = ClaimBodySchema.safeParse(req.body);
  if (!parsed.success) {
    return reply.code(400).send({ error: "invalid body", details: parsed.error.flatten() });
  }

  const { twin_id } = parsed.data;

  const client = await pool.connect();
  try {
    await client.query("BEGIN");

    const claimResult = await client.query<{ id: string; payload: unknown }>(
      `UPDATE orchestrator.tasks
       SET status = 'claimed', updated_at = now()
       WHERE id = $1 AND status = 'pending'
       RETURNING id, payload`,
      [id]
    );

    if ((claimResult.rowCount ?? 0) === 0) {
      await client.query("ROLLBACK");
      const exists = await client.query<{ status: string }>(
        `SELECT status FROM orchestrator.tasks WHERE id = $1`,
        [id]
      );
      if ((exists.rowCount ?? 0) === 0) {
        return reply.code(404).send({ error: "task not found" });
      }
      return reply.code(409).send({ error: "task already claimed", status: exists.rows[0].status });
    }

    const task = claimResult.rows[0];

    const dispatchResult = await client.query<{ id: string }>(
      `INSERT INTO orchestrator.dispatches (task_id, twin_id) VALUES ($1, $2) RETURNING id`,
      [id, twin_id]
    );

    await client.query("COMMIT");

    const dispatchId = dispatchResult.rows[0].id;
    req.log.info({ taskId: id, dispatchId, twin_id }, "task claimed");

    return reply.code(200).send({
      ok: true,
      dispatch_id: dispatchId,
      task: { id: task.id, payload: task.payload },
    });
  } catch (err) {
    await client.query("ROLLBACK");
    if ((err as { code?: string }).code === "23505") {
      return reply.code(409).send({ error: "task already claimed" });
    }
    req.log.error({ err }, "claim failed");
    return reply.code(500).send({ error: "claim failed" });
  } finally {
    client.release();
  }
});

// ===========================================================================
// Agent-task surface — long-running tasks with escalation
// ===========================================================================

const CreateTaskSchema = z.object({
  human_owner: z.string().min(1),
  twin_id: z.string().optional(),
  description: z.string().min(1),
  max_rounds: z.number().int().positive().optional(),
});

// POST /tasks — create an agent task
app.post("/tasks", async (req, reply) => {
  const body = CreateTaskSchema.safeParse(req.body);
  if (!body.success) return reply.code(400).send({ error: body.error.flatten() });

  const { human_owner, twin_id, description, max_rounds = 10 } = body.data;
  const { rows } = await pool.query<Task>(
    `INSERT INTO orchestrator.tasks
       (human_owner, twin_id, description, max_rounds, status, source, event_type, payload)
     VALUES ($1, $2, $3, $4, 'running', 'agent', 'agent_task', '{}'::jsonb)
     RETURNING *`,
    [human_owner, twin_id ?? null, description, max_rounds]
  );
  return reply.code(201).send(rows[0]);
});

// GET /tasks/:id
app.get<{ Params: { id: string } }>("/tasks/:id", async (req, reply) => {
  const { rows } = await pool.query<Task>(
    `SELECT * FROM orchestrator.tasks WHERE id = $1`,
    [req.params.id]
  );
  if (!rows.length) return reply.code(404).send({ error: "not found" });
  return rows[0];
});

const ProgressSchema = z.object({
  failure: z.boolean().optional(),
});

// POST /tasks/:id/progress — tick a round, optionally record a failure
app.post<{ Params: { id: string } }>("/tasks/:id/progress", async (req, reply) => {
  const body = ProgressSchema.safeParse(req.body);
  if (!body.success) return reply.code(400).send({ error: body.error.flatten() });

  const failure = body.data.failure ?? false;
  const { rows } = await pool.query<Task>(
    `UPDATE orchestrator.tasks
     SET rounds = rounds + 1,
         consecutive_failures = CASE WHEN $1 THEN consecutive_failures + 1 ELSE 0 END,
         updated_at = now()
     WHERE id = $2 AND status = 'running' AND human_owner IS NOT NULL
     RETURNING *`,
    [failure, req.params.id]
  );

  if (!rows.length) return reply.code(404).send({ error: "task not found or not a running agent task" });
  const task = rows[0];

  // Sub-60 s escalation: detector also catches it on the next tick.
  const hitCap = task.rounds >= task.max_rounds;
  const repeatedFailure = task.consecutive_failures >= 3;
  if ((hitCap || repeatedFailure) && !task.escalated_at) {
    const reason = hitCap ? "max_rounds" : "repeated_failure";
    await pool.query(
      `UPDATE orchestrator.tasks SET status = 'stalled', updated_at = now() WHERE id = $1`,
      [task.id]
    );
    await escalateTask(pool, { ...task, status: "stalled" }, reason, (msg) =>
      app.log.info(msg)
    );
    const { rows: updated } = await pool.query<Task>(
      `SELECT * FROM orchestrator.tasks WHERE id = $1`,
      [task.id]
    );
    return updated[0];
  }

  return task;
});

// POST /tasks/:id/complete
app.post<{ Params: { id: string } }>("/tasks/:id/complete", async (req, reply) => {
  const { rows } = await pool.query<Task>(
    `UPDATE orchestrator.tasks
     SET status = 'completed', updated_at = now()
     WHERE id = $1 AND status = 'running' AND human_owner IS NOT NULL
     RETURNING *`,
    [req.params.id]
  );
  if (!rows.length) return reply.code(404).send({ error: "task not found or not a running agent task" });
  return rows[0];
});

// POST /tasks/:id/simulate-stall — force escalation immediately (testing)
app.post<{ Params: { id: string } }>("/tasks/:id/simulate-stall", async (req, reply) => {
  const { rows } = await pool.query<Task>(
    `UPDATE orchestrator.tasks
     SET status = 'stalled', stall_reason = 'simulated', updated_at = now()
     WHERE id = $1 AND escalated_at IS NULL AND human_owner IS NOT NULL
     RETURNING *`,
    [req.params.id]
  );
  if (!rows.length) {
    return reply.code(404).send({ error: "task not found, already escalated, or not an agent task" });
  }

  const task = rows[0];
  await escalateTask(pool, task, "simulated", (msg) => app.log.info(msg));

  const { rows: updated } = await pool.query<Task>(
    `SELECT * FROM orchestrator.tasks WHERE id = $1`,
    [task.id]
  );
  return updated[0];
});

// GET /queue/:slack_user_id — stalled + escalated tasks for a human
app.get<{ Params: { slack_user_id: string } }>("/queue/:slack_user_id", async (req) => {
  const { slack_user_id } = req.params;
  const { rows } = await pool.query<Task>(
    `SELECT * FROM orchestrator.tasks
     WHERE human_owner = $1
       AND status IN ('stalled', 'escalated')
     ORDER BY created_at DESC`,
    [slack_user_id]
  );

  const stalled = rows.filter((t) => t.status === "stalled");
  const escalated = rows.filter((t) => t.status === "escalated");

  return { human: slack_user_id, stalled, escalated };
});

// ===========================================================================

app.listen({ port: PORT, host: "0.0.0.0" }).catch((err) => {
  app.log.error(err);
  process.exit(1);
});
