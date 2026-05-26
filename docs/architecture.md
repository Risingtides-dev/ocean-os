# Ocean-OS architecture

## Design principles

1. **Append-only event log is the source.** Every ingestion worker writes raw events as immutable rows. Materialized views and aggregates are derived. We can always rebuild current state from the log.
2. **Source schemas are independent.** Each source has its own Postgres schema (`github`, `slack`, `cobrand`, etc.). One source's migration never touches another's tables.
3. **Workers fail independently.** If the Cobrand ingestion worker dies, GitHub still flows. Each worker is its own service with its own deploy.
4. **Read replica only — never write to production sources from Ocean.** Ocean is a sandbox. Agents query Ocean. Agents that need to *act* on a production system call that system's real API through the MCP, but Ocean itself is read-only with respect to upstream truth.
5. **One MCP, many tools.** Every agent loads the same Ocean MCP. The tool surface is curated — we don't expose raw SQL, we expose verbs that map to operator intent.

## Data model — top level

### Source schemas

```
github.events           append-only webhook events
github.repos            current state (materialized view)
github.deploys          current state (materialized view)

slack.events            append-only Events API payloads
slack.messages          flattened message rows for query
slack.threads           thread roots + reply counts

cobrand.events          campaign performance snapshots
cobrand.posts           current state per post
cobrand.campaigns       current state per campaign

campaigns.events        Campaign Hub mutations
campaigns.bookings      current bookings
campaigns.creators      creator roster + history
campaigns.payments      payment ledger

content.jobs            Content Posting Lab jobs
content.distributions   Telegram distribution events

deploys.events          Railway + Cloudflare deploy events
deploys.services        current service state per repo

notion.events           CRM mutations
notion.clients          current client state
```

### Vector + graph layer

```
embeddings              pgvector table — entity_type, entity_id, vector, model
relationships           edges — from_entity, to_entity, relation_type, properties
```

Relationships are derived during ingestion. Examples:
- `campaign → creator` (booked)
- `creator → post` (delivered)
- `post → campaign` (belongs_to)
- `commit → deploy` (triggered)
- `thread → campaign` (mentions)

### Feedback log

```
agent.events            who, what tool, what args, what result
agent.outcomes          downstream signal — was it useful, was it accepted
```

## Ingestion patterns

Two flavors:

### Webhook-driven (preferred)

GitHub, Slack, Notion, Cobrand (where possible) — push events to a Cloudflare Worker that validates the signature and forwards to a queue. Workers consume the queue and write to Postgres. Idempotent on event ID.

### Polling

Railway, Cloudflare itself, Campaign Hub state — no webhooks, so workers poll on a cron. Diff against last-seen state, write deltas to the event log.

## MCP tool surface

Initial verbs:

| Tool | Purpose |
|---|---|
| `ocean.query_campaign(slug)` | Full campaign state — bookings, creators, posts, performance |
| `ocean.search_threads(query, limit)` | Semantic search across Slack history |
| `ocean.search_threads_by_topic(query)` | Filter to threads about a topic, with summaries |
| `ocean.deployments_for_repo(name, since)` | Recent deploys, status, related commits |
| `ocean.creator_history(handle)` | Every campaign a creator has worked on, performance summary |
| `ocean.post_content_to_telegram(folder, brief)` | Generate + distribute via Content Lab pipeline |
| `ocean.diagnose_deploy(repo, commit)` | Pull related commits, Railway logs, Cloudflare state |
| `ocean.find_top_posts(artist, limit)` | Top-performing posts for an artist, with embeddings |
| `ocean.log_agent_action(action, args, result, outcome)` | Write to feedback log |

This list will grow. The principle: every tool maps to an operator intent, not a raw query.

## Hosting decisions (open)

| Component | Option A | Option B | Option C |
|---|---|---|---|
| Postgres | Railway | Supabase | Cloudflare D1 |
| Vector store | pgvector on Railway/Supabase | Pinecone | Cloudflare Vectorize |
| Webhook intake | Cloudflare Workers | Railway | Vercel functions |
| Workers | Railway services | Cloudflare Workers (cron) | Hybrid |
| Blob cache | Cloudflare R2 | Supabase Storage | — |

Recommendation pending discussion in `#claude-ops`. Default leaning: Postgres on Railway with pgvector, Cloudflare Workers for webhook intake, Railway services for ingestion workers, R2 for blobs.

## Security model

- Production sources are **only** read by ingestion workers, never by the MCP or agents directly.
- The MCP server holds read-only Postgres credentials.
- Agent actions that mutate state (e.g. `post_content_to_telegram`) call the upstream system's real API with scoped credentials, not Ocean's DB.
- Feedback log writes are append-only and tagged with the agent identity.

## Scaling assumptions

- ≤ 10k events/day across all sources at current company size. Postgres handles this trivially.
- Vector search over ≤ 100k embeddings. pgvector handles this trivially.
- Bottleneck will be ingestion worker reliability long before DB scale matters.
