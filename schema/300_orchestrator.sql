-- Orchestrator task table.
-- Tracks tasks the orchestrator is running on behalf of a human.
-- Escalation fields record when and why a task stalled.
--
-- Run this after 000_init.sql.

create schema if not exists orchestrator;

create table if not exists orchestrator.tasks (
  id                   uuid primary key default gen_random_uuid(),
  human_owner          text not null,           -- Slack user ID, e.g. U0A1BXYZ
  twin_id              text,                    -- which agent is running this, e.g. 'smaths-bot'
  description          text not null,
  status               text not null default 'running',
    -- running | stalled | escalated | completed
  stall_reason         text,
    -- max_rounds | repeated_failure | simulated
  rounds               int not null default 0,
  max_rounds           int not null default 10,
  consecutive_failures int not null default 0,
  escalated_at         timestamptz,
  created_at           timestamptz not null default now(),
  updated_at           timestamptz not null default now()
);

create index if not exists orchestrator_tasks_human_status_idx
  on orchestrator.tasks (human_owner, status, created_at desc);

create index if not exists orchestrator_tasks_status_escalated_idx
  on orchestrator.tasks (status, escalated_at)
  where escalated_at is null;
