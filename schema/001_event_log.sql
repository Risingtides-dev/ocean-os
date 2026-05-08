-- Append-only event log shape, repeated per source.
-- Each source's `events` table follows this contract.

create table if not exists github.events (
  id           bigserial primary key,
  external_id  text not null,
  type         text not null,
  repo         text not null,
  actor        text,
  occurred_at  timestamptz not null,
  payload      jsonb not null,
  ingested_at  timestamptz not null default now(),
  unique (external_id)
);
create index if not exists github_events_repo_occurred_idx
  on github.events (repo, occurred_at desc);
create index if not exists github_events_type_idx
  on github.events (type);

create table if not exists slack.events (
  id           bigserial primary key,
  external_id  text not null,
  type         text not null,
  channel_id   text,
  thread_ts    text,
  user_id      text,
  occurred_at  timestamptz not null,
  payload      jsonb not null,
  ingested_at  timestamptz not null default now(),
  unique (external_id)
);
create index if not exists slack_events_channel_thread_idx
  on slack.events (channel_id, thread_ts, occurred_at);
create index if not exists slack_events_type_idx on slack.events (type);

create table if not exists campaigns.events (
  id           bigserial primary key,
  external_id  text not null,
  type         text not null,
  campaign_slug text,
  payload      jsonb not null,
  occurred_at  timestamptz not null,
  ingested_at  timestamptz not null default now(),
  unique (external_id)
);
create index if not exists campaigns_events_slug_idx
  on campaigns.events (campaign_slug, occurred_at desc);

create table if not exists content.events (
  id           bigserial primary key,
  external_id  text not null,
  type         text not null,
  project_id   text,
  payload      jsonb not null,
  occurred_at  timestamptz not null,
  ingested_at  timestamptz not null default now(),
  unique (external_id)
);

create table if not exists deploys.events (
  id           bigserial primary key,
  external_id  text not null,
  type         text not null,
  service      text not null,         -- 'railway:campaign-hub', 'cloudflare:tracker'
  status       text,                  -- 'started', 'succeeded', 'failed'
  payload      jsonb not null,
  occurred_at  timestamptz not null,
  ingested_at  timestamptz not null default now(),
  unique (external_id)
);
create index if not exists deploys_events_service_idx
  on deploys.events (service, occurred_at desc);

create table if not exists notion.events (
  id           bigserial primary key,
  external_id  text not null,
  type         text not null,
  page_id      text,
  payload      jsonb not null,
  occurred_at  timestamptz not null,
  ingested_at  timestamptz not null default now(),
  unique (external_id)
);

create table if not exists telegram.events (
  id           bigserial primary key,
  external_id  text not null,
  type         text not null,
  chat_id      text,
  payload      jsonb not null,
  occurred_at  timestamptz not null,
  ingested_at  timestamptz not null default now(),
  unique (external_id)
);
