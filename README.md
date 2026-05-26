# Ocean-OS

> Agentic knowledge layer for Rising Tides, plus the Rust-native Ocean runtime in this repo.

This repository now contains two related tracks:

- **Ocean-OS**: deployed central data + tool layer for Rising Tides agents.
- **ocean-rs**: Rust-native local coding-agent harness/runtime and TUI.

## ocean-rs runtime

`ocean-rs` is the canonical Rust-native coding-agent harness/runtime for Ocean.
It is **not** a Pi fork. We are using Pi concepts as reference material, then building a lower-level Rust runtime and operator floor in Rust.

Current product framing:

- `ocean-rs` is the canonical Rust-native coding-agent harness/runtime.
- `ocean-daemon` owns runtime authority: provider calls, agent loops, tools, sessions, permissions, and events.
- `ocean-tui` is the active steering cockpit and Rust-native Tides Mesh MeshFloor over that harness, not a passive daemon dashboard.
- F1 PM is the minimal Rust-backed agent-turn chat lane.
- Ocean GUI and service layers remain thin clients until the daemon protocol is stable.

Run the daemon:

```bash
cargo run -p ocean-daemon
```

Health:

```bash
curl http://127.0.0.1:4780/health
```

Prompt:

```bash
cargo run -p ocean-cli -- prompt "Reply OK"
```

TUI:

```bash
cargo run -p ocean-tui
```

## Ocean-OS knowledge layer

Ocean-OS is a deployed central data + tool layer that ingests from every system Rising Tides runs on, normalizes it, vectorizes it, and exposes it to all our agents through a single MCP. It is **not** a replacement for any production database — it is a read-replica + event log + vector index + knowledge graph that agents can hammer without risk to live data.

## Why this exists

Right now every Claude in the workspace is flying solo. Each agent greps its operator's filesystem, hits GitHub independently, scrapes Notion, and stitches an answer for every Slack mention. None of them share a grounded view of reality, half the answers are partly hallucinated, and there is no shared memory across the squad.

Ocean-OS is the layer that fixes this. One source of truth that every agent reads from, one tool surface every agent acts through, one feedback log every agent writes back into.

## Architecture

```text
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
                │  Materialized current state     │
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

Append-only event log + materialized views for current state queries. Schemas namespaced by source: `github.*`, `slack.*`, `campaigns.*`, `content.*`, `deploys.*`. See [`schema/000_init.sql`](schema/000_init.sql).

### 3. Embedding + knowledge graph

`pgvector` over text-heavy data. Relationship graph layer linking entities — campaign → creators → posts → performance → client.

### 4. Ocean MCP

Single MCP server every Slack bot loads. Tools like:

- `ocean.query_campaign(slug)`
- `ocean.search_threads(query)`
- `ocean.deployments_for_repo(name)`
- `ocean.creator_history(handle)`
- `ocean.post_content_to_telegram(folder, brief)`
- `ocean.diagnose_deploy(repo)`

See [`mcp/`](mcp/) for the skeleton.

### 5. Feedback loop

Every agent action posts back into Ocean. Bot suggested X, user accepted/rejected, outcome was Y.

### 6. Skills/prompts/MCP registry

Versioned shared store of the team's reusable prompts, skills, and MCP configurations.

## Repo layout

```text
ocean-os/
├── crates/                       ocean-rs Rust runtime crates
│   ├── ocean-agent/
│   ├── ocean-cli/
│   ├── ocean-core/
│   ├── ocean-daemon/
│   ├── ocean-providers/
│   └── ocean-tui/
├── docs/
├── schema/
├── mcp/
├── ingestion/
└── .github/
```

## Contributing

See [`.github/CONTRIBUTING.md`](.github/CONTRIBUTING.md).
