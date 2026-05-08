/**
 * Ocean-OS orchestrator service.
 *
 * Tracks tasks that agents are running on behalf of humans. When a task stalls
 * (max rounds hit, repeated failures, twin unreachable), it posts a Slack DM
 * to the human owner and stamps the task row.
 *
 * Env:
 *   PORT                — default 8082
 *   OCEAN_DATABASE_URL  — Postgres connection string (write role for orchestrator schema)
 *   SLACK_BOT_TOKEN     — Bot token with chat:write scope for DMs
 *
 * Endpoints:
 *   GET  /health
 *   POST /tasks                        — create a task
 *   GET  /tasks/:id                    — get a task
 *   POST /tasks/:id/progress           — increment rounds or record a failure
 *   POST /tasks/:id/complete           — mark completed
 *   POST /tasks/:id/simulate-stall     — force escalation immediately (for testing)
 *   GET  /queue/:slack_user_id         — stalled + escalated tasks for a human
 */

import Fastify from "fastify";
import { Pool } from "pg";
import { z } from "zod";
import { detectAndEscalate, escalateTask, type Task } from "./escalate.js";

const PORT = Number(process.env.PORT ?? 8082);
const DB_URL = process.env.OCEAN_DATABASE_URL;

const pool = new Pool({ connectionString: DB_URL });
const app = Fastify({ logger: true });

// ---------------------------------------------------------------------------
// Stall detector — runs every 60 s
// ---------------------------------------------------------------------------

setInterval(() => {
  detectAndEscalate(pool, (msg) => app.log.info(msg)).catch((err) => {
    app.log.error({ err }, "stall detector error");
  });
}, 60_000);

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

app.get("/health", async () => ({ ok: true }));

// ---------------------------------------------------------------------------
// POST /tasks — create a task
// ---------------------------------------------------------------------------

const CreateTaskSchema = z.object({
  human_owner: z.string().min(1),
  twin_id: z.string().optional(),
  description: z.string().min(1),
  max_rounds: z.number().int().positive().optional(),
});

app.post("/tasks", async (req, reply) => {
  const body = CreateTaskSchema.safeParse(req.body);
  if (!body.success) return reply.code(400).send({ error: body.error.flatten() });

  const { human_owner, twin_id, description, max_rounds = 10 } = body.data;
  const { rows } = await pool.query<Task>(
    `INSERT INTO orchestrator.tasks (human_owner, twin_id, description, max_rounds)
     VALUES ($1, $2, $3, $4)
     RETURNING *`,
    [human_owner, twin_id ?? null, description, max_rounds]
  );
  return reply.code(201).send(rows[0]);
});

// ---------------------------------------------------------------------------
// GET /tasks/:id
// ---------------------------------------------------------------------------

app.get<{ Params: { id: string } }>("/tasks/:id", async (req, reply) => {
  const { rows } = await pool.query<Task>(
    `SELECT * FROM orchestrator.tasks WHERE id = $1`,
    [req.params.id]
  );
  if (!rows.length) return reply.code(404).send({ error: "not found" });
  return rows[0];
});

// ---------------------------------------------------------------------------
// POST /tasks/:id/progress — record a round tick or a failure
// ---------------------------------------------------------------------------

const ProgressSchema = z.object({
  failure: z.boolean().optional(),
});

app.post<{ Params: { id: string } }>("/tasks/:id/progress", async (req, reply) => {
  const body = ProgressSchema.safeParse(req.body);
  if (!body.success) return reply.code(400).send({ error: body.error.flatten() });

  const failure = body.data.failure ?? false;
  const { rows } = await pool.query<Task>(
    `UPDATE orchestrator.tasks
     SET rounds = rounds + 1,
         consecutive_failures = CASE WHEN $1 THEN consecutive_failures + 1 ELSE 0 END,
         updated_at = now()
     WHERE id = $2 AND status = 'running'
     RETURNING *`,
    [failure, req.params.id]
  );

  if (!rows.length) return reply.code(404).send({ error: "task not found or not running" });
  const task = rows[0];

  // Immediate escalation if a condition was just crossed — next detector tick
  // will also catch it, but firing here gives sub-60 s latency.
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

// ---------------------------------------------------------------------------
// POST /tasks/:id/complete
// ---------------------------------------------------------------------------

app.post<{ Params: { id: string } }>("/tasks/:id/complete", async (req, reply) => {
  const { rows } = await pool.query<Task>(
    `UPDATE orchestrator.tasks
     SET status = 'completed', updated_at = now()
     WHERE id = $1 AND status = 'running'
     RETURNING *`,
    [req.params.id]
  );
  if (!rows.length) return reply.code(404).send({ error: "task not found or not running" });
  return rows[0];
});

// ---------------------------------------------------------------------------
// POST /tasks/:id/simulate-stall — force escalation immediately (testing)
// ---------------------------------------------------------------------------

app.post<{ Params: { id: string } }>("/tasks/:id/simulate-stall", async (req, reply) => {
  const { rows } = await pool.query<Task>(
    `UPDATE orchestrator.tasks
     SET status = 'stalled', stall_reason = 'simulated', updated_at = now()
     WHERE id = $1 AND escalated_at IS NULL
     RETURNING *`,
    [req.params.id]
  );
  if (!rows.length) {
    return reply.code(404).send({ error: "task not found or already escalated" });
  }

  const task = rows[0];
  await escalateTask(pool, task, "simulated", (msg) => app.log.info(msg));

  const { rows: updated } = await pool.query<Task>(
    `SELECT * FROM orchestrator.tasks WHERE id = $1`,
    [task.id]
  );
  return updated[0];
});

// ---------------------------------------------------------------------------
// GET /queue/:slack_user_id — stalled + escalated tasks for a human
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------

app.listen({ port: PORT, host: "0.0.0.0" }).catch((err) => {
  app.log.error(err);
  process.exit(1);
});
