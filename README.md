# Ocean-OS

> Agentic knowledge layer for Rising Tides.

Ocean-OS is a deployed central data + tool layer that ingests from every system Rising Tides runs on, normalizes it, vectorizes it, and exposes it to all our agents through a single MCP. It is **not** a replacement for any production database — it is a read-replica + event log + vector index + knowledge graph that agents can hammer without risk to live data.

## Why this exists

Right now every Claude in the workspace is flying solo. Each agent greps its operator's filesystem, hits GitHub independently, scrapes Notion, and stitches an answer for every Slack mention. None of them share a grounded view of reality, half the answers are partly hallucinated, and there is no shared memory across the squad.

Ocean-OS is the layer that fixes this. One source of truth that every agent reads from, one tool surface every agent acts through, one feedback log every agent writes back into.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Production sources                           │
│  Slack  GitHub  Cobrand  Campaign Hub  Content Lab  Telegram        │
│  Railway  Cloudflare  Notion CRM  Cloudflare Email                  │
└────────────────────────────────┬────────────────────────────────────┘
                                 │ webhooks + polling
                ┌────────────────▼────────────────┐
                │    Ingestion workers (per src)  │
                │  Independent services. Fail     │
                │  independently. Own one source. │
                └────────────────┬────────────────┘
                                 │
                ┌────────────────▼────────────────┐
                │       Postgres core             │
                │  Append-only event log per src  │
                │  Materialized "current state"   │
                │  views per source schema        │
                │                                 │
                │  + pgvector embeddings          │
                │  + relationship graph           │
                └────────────────┬────────────────┘
                                 │
                ┌────────────────▼────────────────┐
                │       Ocean MCP server          │
                │  Single MCP every Slack bot     │
                │  loads. Hides source-specific   │
                │  complexity behind clean tools. │
                └────────────────┬────────────────┘
                                 │
        ┌──────────────────┬─────┴──────┬─────────────────┐
        │                  │            │                 │
   smaths-bot         Jake's Claude  Eric's Claude   future agents
```

## Components

### 1. Ingestion workers

Independent services per source. Each owns one source, fails independently, writes to its own schema.

| Source | What it ingests |
|---|---|
| `slack` | Messages, threads, reactions, channel events |
| `github` | Commits, PRs, issues, deploys, branch events |
| `cobrand` | Campaign performance, post submissions, view counts |
| `campaign-hub` | Bookings, creators, payments, budgets |
| `content-lab` | Generation jobs, render outputs, distribution events |
| `telegram` | Distribution events, folder uploads, poster activity |
| `railway` | Deploy state, service logs, env metadata |
| `cloudflare` | DNS state, Workers state, R2 buckets, email routing |
| `notion` | CRM entries, campaign briefs, client data |

### 2. Postgres core

Append-only event log + materialized views for "current state" queries. Schemas namespaced by source: `github.*`, `slack.*`, `campaigns.*`, `content.*`, `deploys.*`. See [`schema/000_init.sql`](schema/000_init.sql).

### 3. Embedding + knowledge graph

`pgvector` over text-heavy data (Slack threads, PR descriptions, campaign briefs, post captions). Relationship graph layer linking entities — campaign → creators → posts → performance → client. Agents can do semantic search **and** traverse relationships.

### 4. Ocean MCP

Single MCP server every Slack bot loads. Tools like:

- `ocean.query_campaign(slug)` — full campaign state
- `ocean.search_threads(query)` — semantic search across Slack
- `ocean.deployments_for_repo(name)` — recent deploys + status
- `ocean.creator_history(handle)` — every campaign a creator has worked on
- `ocean.post_content_to_telegram(folder, brief)` — wraps Content Lab pipeline as one tool
- `ocean.diagnose_deploy(repo)` — pulls related commits + Railway logs + Cloudflare state

See [`mcp/`](mcp/) for the skeleton.

### 5. Feedback loop

Every agent action posts back into Ocean. Bot suggested X, user accepted/rejected, outcome was Y. Training signal for everything we build later.

### 6. Skills/prompts/MCP registry

Versioned shared store of the team's reusable prompts, skills, and MCP configurations. When Jake writes a great campaign-debrief prompt, every other bot picks it up automatically.

## What this unlocks

- **Wrapped-style campaign deliverables, automated.** End of campaign, agent queries Ocean for posts + performance + creators + budget, generates the deliverable, drops it in the client folder.
- **Instagram content grounded in real data.** "Write 5 captions in the style of our top-performing posts for this artist" — agent pulls actual top performers, embeds them, generates on real signal.
- **Content posting from a Slack message.** "Generate a video and send it to the Lazy Rosana telegram folder" — Ocean MCP wraps Content Posting Lab's pipeline as one tool.
- **Cross-team troubleshooting.** "My deploy is broken" — agent queries Ocean for the deploy event, related commits, Railway logs, Cloudflare DNS state, returns a real diagnosis.
- **Onboarding leverage.** New teammate spins up their bridge, points at Ocean, immediately has the same situational awareness as the rest of us.

## Long-term vision

Three layers.

1. **Observability** — every agent sees what's happening company-wide.
2. **Action** — every agent can *do* things across the company through Ocean's tool surface.
3. **Learning** — Ocean accumulates the feedback loop and starts knowing which prompts, creators, and campaign shapes actually work.

Year two, Ocean is the system that makes a 10-person agency operate like a 50-person one.

## Where it runs

Open question, want input from the squad:

- **Railway** for Postgres + ingestion workers (close to existing infra)
- **Cloudflare Workers** for webhook intake (cheap, fast, no cold start)
- **Cloudflare R2** for blob caching
- **Supabase** is also viable for the DB layer (pgvector + auth out of the box)

## Status

Early. Repo just stood up. Architecture and first proof-of-concept ingestion worker (GitHub) are scaffolded — see [`docs/architecture.md`](docs/architecture.md) for detail.

## Contributing

See [`.github/CONTRIBUTING.md`](.github/CONTRIBUTING.md). The plan is for each Claude/operator to claim one or more ingestion workers and own them end to end.

## Repo layout

```
ocean-os/
├── docs/
│   └── architecture.md           Deeper architecture detail
├── schema/
│   └── 000_init.sql              Initial Postgres schema
├── mcp/                          Ocean MCP server (TypeScript)
│   ├── package.json
│   └── src/index.ts
├── ingestion/
│   └── github/                   First ingestion worker (proof of concept)
│       ├── package.json
│       └── src/index.ts
└── .github/
    └── CONTRIBUTING.md
```
