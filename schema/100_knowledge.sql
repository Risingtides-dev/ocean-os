-- Embeddings + knowledge graph.

create table if not exists knowledge.embeddings (
  id          bigserial primary key,
  source      text not null,           -- 'github', 'slack', 'campaigns', etc.
  source_id   text not null,           -- id from the source's events table
  kind        text not null,           -- 'pr_description', 'thread_message', 'campaign_brief', 'caption', etc.
  text        text not null,
  vector      vector(1536),            -- text-embedding-3-small dim
  metadata    jsonb default '{}'::jsonb,
  embedded_at timestamptz not null default now(),
  unique (source, source_id, kind)
);
create index if not exists embeddings_vector_idx
  on knowledge.embeddings using hnsw (vector vector_cosine_ops);
create index if not exists embeddings_source_idx
  on knowledge.embeddings (source, kind);

create table if not exists knowledge.entities (
  id     bigserial primary key,
  kind   text not null,    -- 'campaign', 'creator', 'post', 'client', 'deploy', 'channel', 'thread', 'user'
  name   text not null,
  external_ref text,       -- id in the owning source ('warner-may-26', '@user', 'C0B1KSLNY23', ...)
  metadata jsonb default '{}'::jsonb,
  created_at timestamptz not null default now(),
  unique (kind, external_ref)
);
create index if not exists entities_kind_idx on knowledge.entities (kind);

create table if not exists knowledge.edges (
  id            bigserial primary key,
  from_id       bigint not null references knowledge.entities(id) on delete cascade,
  to_id         bigint not null references knowledge.entities(id) on delete cascade,
  relation      text not null,        -- 'creator_in_campaign', 'post_for_campaign', etc.
  evidence_event jsonb,                -- pointer to the source event that produced this edge
  created_at    timestamptz not null default now(),
  unique (from_id, to_id, relation)
);
create index if not exists edges_from_idx on knowledge.edges (from_id, relation);
create index if not exists edges_to_idx on knowledge.edges (to_id, relation);
