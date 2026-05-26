-- Ocean-OS initial schema
-- Run on a fresh Postgres 15+ database with pgvector extension available.

CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- ============================================================
-- Source schemas — append-only event logs
-- ============================================================

CREATE SCHEMA IF NOT EXISTS github;
CREATE SCHEMA IF NOT EXISTS slack;
CREATE SCHEMA IF NOT EXISTS cobrand;
CREATE SCHEMA IF NOT EXISTS campaigns;
CREATE SCHEMA IF NOT EXISTS content;
CREATE SCHEMA IF NOT EXISTS deploys;
CREATE SCHEMA IF NOT EXISTS notion;
CREATE SCHEMA IF NOT EXISTS agent;

-- ============================================================
-- GitHub
-- ============================================================

CREATE TABLE IF NOT EXISTS github.events (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  delivery_id   text UNIQUE NOT NULL,                -- X-GitHub-Delivery, idempotent key
  event_type    text NOT NULL,                       -- push, pull_request, deployment, etc.
  repo          text NOT NULL,                       -- owner/name
  actor         text,
  payload       jsonb NOT NULL,
  received_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_github_events_repo_time ON github.events (repo, received_at DESC);
CREATE INDEX IF NOT EXISTS idx_github_events_type ON github.events (event_type);

-- Materialized current state for repos. Refreshed on cron from events.
CREATE TABLE IF NOT EXISTS github.repos (
  full_name        text PRIMARY KEY,
  default_branch   text,
  description      text,
  last_pushed_at   timestamptz,
  open_pr_count    int DEFAULT 0,
  updated_at       timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS github.deploys (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  repo            text NOT NULL,
  environment     text NOT NULL,                     -- production, preview, etc.
  status          text NOT NULL,                     -- success, failure, in_progress
  sha             text,
  triggered_by    text,
  started_at      timestamptz NOT NULL,
  finished_at     timestamptz,
  source_event_id uuid REFERENCES github.events(id)
);
CREATE INDEX IF NOT EXISTS idx_github_deploys_repo_time ON github.deploys (repo, started_at DESC);

-- ============================================================
-- Slack
-- ============================================================

CREATE TABLE IF NOT EXISTS slack.events (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  event_id     text UNIQUE NOT NULL,                 -- Slack's own event_id, idempotent
  event_type   text NOT NULL,
  team_id      text,
  channel_id   text,
  user_id      text,
  thread_ts    text,
  ts           text NOT NULL,
  payload      jsonb NOT NULL,
  received_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_slack_events_channel_time ON slack.events (channel_id, ts DESC);
CREATE INDEX IF NOT EXISTS idx_slack_events_thread ON slack.events (thread_ts);

-- Flattened messages for query
CREATE TABLE IF NOT EXISTS slack.messages (
  ts            text NOT NULL,
  channel_id    text NOT NULL,
  thread_ts     text,
  user_id       text,
  text          text,
  bot_id        text,
  PRIMARY KEY (channel_id, ts)
);
CREATE INDEX IF NOT EXISTS idx_slack_messages_thread ON slack.messages (channel_id, thread_ts);

-- ============================================================
-- Cobrand (campaign performance)
-- ============================================================

CREATE TABLE IF NOT EXISTS cobrand.events (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  event_type    text NOT NULL,                       -- snapshot, submission, etc.
  campaign_id   text NOT NULL,
  post_id       text,
  payload       jsonb NOT NULL,
  observed_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_cobrand_events_campaign ON cobrand.events (campaign_id, observed_at DESC);

CREATE TABLE IF NOT EXISTS cobrand.campaigns (
  campaign_id   text PRIMARY KEY,
  slug          text,
  client        text,
  artist        text,
  status        text,
  total_views   bigint,
  total_posts   int,
  updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS cobrand.posts (
  post_id       text PRIMARY KEY,
  campaign_id   text REFERENCES cobrand.campaigns(campaign_id),
  creator       text,
  url           text,
  views         bigint,
  engagement    bigint,
  posted_at     timestamptz,
  updated_at    timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_cobrand_posts_campaign ON cobrand.posts (campaign_id);
CREATE INDEX IF NOT EXISTS idx_cobrand_posts_views ON cobrand.posts (views DESC);

-- ============================================================
-- Campaigns (Campaign Hub)
-- ============================================================

CREATE TABLE IF NOT EXISTS campaigns.events (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  event_type    text NOT NULL,                       -- booking_created, payment_logged, etc.
  campaign_slug text NOT NULL,
  payload       jsonb NOT NULL,
  observed_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_campaigns_events_slug ON campaigns.events (campaign_slug, observed_at DESC);

CREATE TABLE IF NOT EXISTS campaigns.bookings (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  campaign_slug text NOT NULL,
  creator       text NOT NULL,
  rate_cents    bigint,
  posts_owed    int,
  status        text,
  updated_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS campaigns.creators (
  handle        text PRIMARY KEY,
  platform      text NOT NULL,                       -- tiktok, instagram
  metadata      jsonb,
  updated_at    timestamptz NOT NULL DEFAULT now()
);

-- ============================================================
-- Content Posting Lab
-- ============================================================

CREATE TABLE IF NOT EXISTS content.jobs (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  job_type      text NOT NULL,                       -- generate, clip, burn, distribute
  status        text NOT NULL,
  inputs        jsonb,
  outputs       jsonb,
  created_at    timestamptz NOT NULL DEFAULT now(),
  finished_at   timestamptz
);

CREATE TABLE IF NOT EXISTS content.distributions (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  job_id        uuid REFERENCES content.jobs(id),
  surface       text NOT NULL,                       -- telegram, postiz
  target        text NOT NULL,                       -- folder/poster id
  status        text NOT NULL,
  observed_at   timestamptz NOT NULL DEFAULT now()
);

-- ============================================================
-- Deploys (Railway + Cloudflare)
-- ============================================================

CREATE TABLE IF NOT EXISTS deploys.events (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  provider      text NOT NULL,                       -- railway, cloudflare
  service       text NOT NULL,
  event_type    text NOT NULL,                       -- deploy_started, deploy_succeeded, etc.
  payload       jsonb NOT NULL,
  observed_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_deploys_events_service_time ON deploys.events (service, observed_at DESC);

-- ============================================================
-- Notion CRM
-- ============================================================

CREATE TABLE IF NOT EXISTS notion.events (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  event_type    text NOT NULL,
  page_id       text NOT NULL,
  payload       jsonb NOT NULL,
  observed_at   timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS notion.clients (
  page_id       text PRIMARY KEY,
  name          text,
  status        text,
  metadata      jsonb,
  updated_at    timestamptz NOT NULL DEFAULT now()
);

-- ============================================================
-- Embeddings + relationships (knowledge graph)
-- ============================================================

CREATE TABLE IF NOT EXISTS embeddings (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  entity_type   text NOT NULL,                       -- slack_thread, post_caption, pr_description, etc.
  entity_id     text NOT NULL,
  model         text NOT NULL,                       -- e.g. text-embedding-3-large
  vector        vector(1536),
  metadata      jsonb,
  created_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (entity_type, entity_id, model)
);
CREATE INDEX IF NOT EXISTS idx_embeddings_entity ON embeddings (entity_type, entity_id);
-- ANN index — tune lists based on row count
CREATE INDEX IF NOT EXISTS idx_embeddings_vector ON embeddings USING ivfflat (vector vector_cosine_ops) WITH (lists = 100);

CREATE TABLE IF NOT EXISTS relationships (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  from_entity     text NOT NULL,
  from_id         text NOT NULL,
  to_entity       text NOT NULL,
  to_id           text NOT NULL,
  relation_type   text NOT NULL,                     -- booked, delivered, mentions, triggered, etc.
  properties      jsonb,
  created_at      timestamptz NOT NULL DEFAULT now(),
  UNIQUE (from_entity, from_id, to_entity, to_id, relation_type)
);
CREATE INDEX IF NOT EXISTS idx_rel_from ON relationships (from_entity, from_id);
CREATE INDEX IF NOT EXISTS idx_rel_to ON relationships (to_entity, to_id);
CREATE INDEX IF NOT EXISTS idx_rel_type ON relationships (relation_type);

-- ============================================================
-- Agent feedback log
-- ============================================================

CREATE TABLE IF NOT EXISTS agent.events (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  agent_id      text NOT NULL,                       -- smaths-bot, jakes-claude, etc.
  tool_name     text NOT NULL,
  args          jsonb,
  result        jsonb,
  occurred_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS idx_agent_events_agent ON agent.events (agent_id, occurred_at DESC);

CREATE TABLE IF NOT EXISTS agent.outcomes (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  event_id      uuid REFERENCES agent.events(id),
  outcome       text NOT NULL,                       -- accepted, rejected, ignored, errored
  signal        jsonb,                               -- downstream metrics if available
  observed_at   timestamptz NOT NULL DEFAULT now()
);
