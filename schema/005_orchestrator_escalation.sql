-- Orchestrator escalation columns.
-- Extends orchestrator.tasks (created in 002_orchestrator.sql) with the
-- agent-task lifecycle fields needed for stall detection and human escalation.
--
-- All additions are nullable (or have defaults) so existing dispatch rows
-- — which have human_owner IS NULL — remain valid. The stall detector
-- filters on human_owner IS NOT NULL to avoid acting on dispatch rows.
--
-- Run after 002_orchestrator.sql. Safe to re-run.

ALTER TABLE orchestrator.tasks ADD COLUMN IF NOT EXISTS human_owner          text;
ALTER TABLE orchestrator.tasks ADD COLUMN IF NOT EXISTS description          text;
ALTER TABLE orchestrator.tasks ADD COLUMN IF NOT EXISTS rounds               int  NOT NULL DEFAULT 0;
ALTER TABLE orchestrator.tasks ADD COLUMN IF NOT EXISTS max_rounds           int  NOT NULL DEFAULT 10;
ALTER TABLE orchestrator.tasks ADD COLUMN IF NOT EXISTS consecutive_failures int  NOT NULL DEFAULT 0;
ALTER TABLE orchestrator.tasks ADD COLUMN IF NOT EXISTS stall_reason         text;
  -- max_rounds | repeated_failure | simulated
ALTER TABLE orchestrator.tasks ADD COLUMN IF NOT EXISTS escalated_at         timestamptz;
ALTER TABLE orchestrator.tasks ADD COLUMN IF NOT EXISTS twin_id              text;
  -- 002_orchestrator.sql does not have twin_id on tasks (it's on dispatches);
  -- agent-task rows reference the agent running them directly.

-- The status column already permits arbitrary text in 002_orchestrator.sql,
-- so no CHECK constraint needs altering — it just carries a wider vocabulary now:
--   dispatch:   pending | claimed | done | failed
--   agent-task: running | stalled  | escalated | completed

CREATE INDEX IF NOT EXISTS orchestrator_tasks_human_status_idx
  ON orchestrator.tasks (human_owner, status, created_at DESC)
  WHERE human_owner IS NOT NULL;

CREATE INDEX IF NOT EXISTS orchestrator_tasks_status_escalated_idx
  ON orchestrator.tasks (status, escalated_at)
  WHERE escalated_at IS NULL AND human_owner IS NOT NULL;
