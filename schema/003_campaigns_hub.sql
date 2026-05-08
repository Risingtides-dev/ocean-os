-- Campaign Hub ingestion additions.
-- Run after 000_init.sql and 001_event_log.sql.
--
-- !! DO NOT RUN AGAINST PRODUCTION WITHOUT REVIEW !!
-- Adds hub_id to campaigns.bookings (idempotency key for Campaign Hub upserts),
-- creates campaigns.payments and campaigns.poll_cursor.

-- campaigns.bookings — add hub_id for idempotent upserts from Campaign Hub.
ALTER TABLE campaigns.bookings ADD COLUMN IF NOT EXISTS hub_id text UNIQUE;

-- campaigns.payments — payment ledger. Campaign Hub is the source of truth.
-- Never populated from Cobrand or any other source.
CREATE TABLE IF NOT EXISTS campaigns.payments (
  id            bigserial PRIMARY KEY,
  hub_id        text NOT NULL UNIQUE,
  campaign_slug text NOT NULL,
  creator       text NOT NULL,
  amount_cents  bigint NOT NULL,
  status        text NOT NULL,          -- pending, paid, reversed
  paid_at       timestamptz,
  occurred_at   timestamptz NOT NULL,
  ingested_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS campaigns_payments_slug_idx
  ON campaigns.payments (campaign_slug, occurred_at DESC);
CREATE INDEX IF NOT EXISTS campaigns_payments_creator_idx
  ON campaigns.payments (creator);

-- campaigns.poll_cursor — tracks last successful sync timestamp per entity type.
-- The Campaign Hub worker reads and advances this on each successful poll.
CREATE TABLE IF NOT EXISTS campaigns.poll_cursor (
  entity_type    text PRIMARY KEY,
  last_synced_at timestamptz NOT NULL DEFAULT '1970-01-01 00:00:00+00'
);
