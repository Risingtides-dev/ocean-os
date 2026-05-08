-- Feedback loop: every agent action lands here.

create schema if not exists feedback;

create table if not exists feedback.actions (
  id          bigserial primary key,
  bot_id      text not null,        -- Slack bot user id
  tool        text not null,        -- e.g. 'ocean.post_content_to_telegram'
  input       jsonb not null,
  output      jsonb,
  outcome     text,                 -- 'accepted', 'rejected', 'ignored', 'errored'
  outcome_meta jsonb default '{}'::jsonb,
  occurred_at timestamptz not null default now()
);
create index if not exists feedback_bot_idx on feedback.actions (bot_id, occurred_at desc);
create index if not exists feedback_tool_idx on feedback.actions (tool);
