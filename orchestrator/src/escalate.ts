/**
 * Escalation logic.
 *
 * detectAndEscalate() is called on a cron (every 60 s). It finds tasks that
 * have hit a stall condition but haven't been escalated yet, posts a Slack DM
 * to the human owner, and stamps escalated_at.
 *
 * Stall conditions (any one is sufficient):
 *   - rounds >= max_rounds         → stall_reason: max_rounds
 *   - consecutive_failures >= 3    → stall_reason: repeated_failure
 *   - status = 'stalled' already   → used by simulate-stall endpoint
 */

import { Pool } from "pg";
import { sendSlackDm, buildEscalationMessage } from "./slack.js";

export type Task = {
  id: string;
  human_owner: string;
  twin_id: string | null;
  description: string;
  status: string;
  stall_reason: string | null;
  rounds: number;
  max_rounds: number;
  consecutive_failures: number;
  escalated_at: string | null;
};

export async function detectAndEscalate(pool: Pool, log: (msg: string) => void): Promise<void> {
  // Find agent-tasks (human_owner IS NOT NULL) that meet a stall condition but
  // haven't been escalated yet. Dispatch tasks (human_owner IS NULL) are ignored —
  // they have no human to DM and a different status vocabulary (pending/claimed/done).
  const { rows } = await pool.query<Task>(`
    SELECT id, human_owner, twin_id, description, status,
           stall_reason, rounds, max_rounds, consecutive_failures, escalated_at
    FROM orchestrator.tasks
    WHERE human_owner IS NOT NULL
      AND escalated_at IS NULL
      AND status != 'completed'
      AND (
        rounds >= max_rounds
        OR consecutive_failures >= 3
        OR status = 'stalled'
      )
  `);

  for (const task of rows) {
    const reason = task.stall_reason ?? deriveReason(task);
    await escalateTask(pool, task, reason, log);
  }
}

export async function escalateTask(
  pool: Pool,
  task: Task,
  reason: string,
  log: (msg: string) => void
): Promise<void> {
  const message = buildEscalationMessage({
    id: task.id,
    human_owner: task.human_owner,
    description: task.description,
    stall_reason: reason,
    rounds: task.rounds,
    max_rounds: task.max_rounds,
    twin_id: task.twin_id,
  });

  try {
    await sendSlackDm(task.human_owner, message);
    log(`[escalate] DM sent to ${task.human_owner} for task ${task.id}`);
  } catch (err) {
    log(`[escalate] DM failed for task ${task.id}: ${(err as Error).message}`);
    // Still stamp the row so we don't retry endlessly on a bad token.
  }

  await pool.query(
    `UPDATE orchestrator.tasks
     SET status = 'escalated',
         stall_reason = $1,
         escalated_at = now(),
         updated_at = now()
     WHERE id = $2`,
    [reason, task.id]
  );
}

function deriveReason(task: Task): string {
  if (task.rounds >= task.max_rounds) return "max_rounds";
  if (task.consecutive_failures >= 3) return "repeated_failure";
  return "stalled";
}
