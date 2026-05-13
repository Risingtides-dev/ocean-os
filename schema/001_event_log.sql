-- Append-only event log additions on top of 000_init.sql.
--
-- History: this file originally redefined every source's `events` table with
-- a different column shape than 000_init.sql. Because of IF NOT EXISTS, those
-- CREATE TABLE statements silently no-op'd, then the follow-up CREATE INDEX
-- statements tried to index columns that didn't exist and the file failed to
-- apply. See PR for "fix(schema): remove duplicate table defs in
-- 001_event_log.sql".
--
-- 000_init.sql is the source of truth for github/slack/deploys/notion events;
-- the github/slack/cobrand ingestion workers target that shape. The
-- campaign-hub and content-lab workers were built against this file's
-- "external_id / type / occurred_at" shape, so we keep that shape for
-- campaigns.events (additively, alongside 000's columns) and content.events
-- (which 000 does not define). Telegram is net new.
--
-- Apply order: after 000_init.sql, before 002_orchestrator.sql.
-- Safe to re-run.

-- ============================================================
-- Telegram (net new — not in 000)
-- ============================================================

CREATE SCHEMA IF NOT EXISTS telegram;

CREATE TABLE IF NOT EXISTS telegram.events (
  id           bigserial PRIMARY KEY,
  external_id  text NOT NULL UNIQUE,
  type         text NOT NULL,
  chat_id      text,
  payload      jsonb NOT NULL,
  occurred_at  timestamptz NOT NULL,
  ingested_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS telegram_events_chat_occurred_idx
  ON telegram.events (chat_id, occurred_at DESC);
CREATE INDEX IF NOT EXISTS telegram_events_type_idx
  ON telegram.events (type);

-- ============================================================
-- content.events — Content Posting Lab event log
-- 000 defines content.jobs and content.distributions but not content.events.
-- The content-lab ingestion worker writes to this table.
-- ============================================================

CREATE TABLE IF NOT EXISTS content.events (
  id           bigserial PRIMARY KEY,
  external_id  text NOT NULL UNIQUE,
  type         text NOT NULL,
  project_id   text,
  payload      jsonb NOT NULL,
  occurred_at  timestamptz NOT NULL,
  ingested_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS content_events_project_occurred_idx
  ON content.events (project_id, occurred_at DESC);
CREATE INDEX IF NOT EXISTS content_events_type_idx
  ON content.events (type);

-- ============================================================
-- campaigns.events — Campaign Hub additive columns
-- 000 created this table with a different shape (event_type/observed_at).
-- The campaign-hub poller writes (external_id, type, campaign_slug, payload,
-- occurred_at) so we add those columns and the unique index it depends on.
-- event_type is dropped to NULLABLE so the new INSERT path doesn't need to
-- supply it; nothing currently writes the 000 shape for this table.
-- ============================================================

ALTER TABLE campaigns.events ADD COLUMN IF NOT EXISTS external_id text;
ALTER TABLE campaigns.events ADD COLUMN IF NOT EXISTS type        text;
ALTER TABLE campaigns.events ADD COLUMN IF NOT EXISTS occurred_at timestamptz;
ALTER TABLE campaigns.events ADD COLUMN IF NOT EXISTS ingested_at timestamptz NOT NULL DEFAULT now();
ALTER TABLE campaigns.events ALTER COLUMN event_type DROP NOT NULL;

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_constraint
    WHERE conname = 'campaigns_events_external_id_key'
  ) THEN
    ALTER TABLE campaigns.events
      ADD CONSTRAINT campaigns_events_external_id_key UNIQUE (external_id);
  END IF;
END $$;

CREATE INDEX IF NOT EXISTS campaigns_events_slug_occurred_idx
  ON campaigns.events (campaign_slug, occurred_at DESC);
