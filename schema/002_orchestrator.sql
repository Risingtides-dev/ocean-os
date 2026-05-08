-- Ocean-OS orchestrator schema
-- !! NOT YET EXECUTED — run manually against the target database after review !!
--
-- Depends on: 000_init.sql (pgcrypto extension, gen_random_uuid())
-- Apply with: psql $OCEAN_DATABASE_URL -f schema/002_orchestrator.sql

CREATE SCHEMA IF NOT EXISTS orchestrator;

-- Tasks are created when an inbound webhook event is accepted.
-- A task represents one unit of work to be claimed and executed by a twin.
CREATE TABLE IF NOT EXISTS orchestrator.tasks (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  source       text NOT NULL,              -- 'github' | 'slack'
  event_type   text NOT NULL,              -- e.g. 'issues.opened', 'message'
  source_ref   text,                       -- delivery_id or slack event_id for traceability
  payload      jsonb NOT NULL,
  status       text NOT NULL DEFAULT 'pending',  -- pending | claimed | done | failed
  created_at   timestamptz NOT NULL DEFAULT now(),
  updated_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_orchestrator_tasks_status ON orchestrator.tasks (status, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_orchestrator_tasks_source ON orchestrator.tasks (source, event_type);

-- Dispatches record each claim attempt by a twin bridge.
-- One task may be claimed exactly once (enforced by the unique partial index below).
CREATE TABLE IF NOT EXISTS orchestrator.dispatches (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  task_id     uuid NOT NULL REFERENCES orchestrator.tasks(id),
  twin_id     text NOT NULL,              -- e.g. 'smaths-bot', 'jakes-claude'
  claimed_at  timestamptz NOT NULL DEFAULT now(),
  outcome     text,                       -- null until the twin reports back: 'done' | 'failed'
  result      jsonb,
  resolved_at timestamptz
);

-- One active or successfully completed dispatch per task.
-- A failed dispatch (outcome = 'failed') is excluded so the task can be retried.
CREATE UNIQUE INDEX IF NOT EXISTS idx_orchestrator_dispatches_task
  ON orchestrator.dispatches (task_id)
  WHERE outcome IS NULL OR outcome = 'done';

CREATE INDEX IF NOT EXISTS idx_orchestrator_dispatches_twin ON orchestrator.dispatches (twin_id, claimed_at DESC);
