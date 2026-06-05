# Ocean Longhouse

Ocean Longhouse is the **local-first agentic operations hub** for Ocean OS: the place agents go before they act.

Longhouse is not a separate "Hive" product. **Longhouse is the hive.** It is the shared coordination layer that centralizes how agents discover context, select routines, convene peers, and decide what should happen next.

## Responsibility split

Ocean has two load-bearing pieces:

- **`ocean-daemon` = local runtime/body**
  - Owns sessions and transcript persistence.
  - Streams client events over the Ocean HTTP/SSE API.
  - Executes local filesystem, shell, tool, MCP, and destructive actions.
  - Enforces local permission gates.
  - Remains the authority for actions on a user machine.

- **`ocean-longhouse` = hive brain / coordination layer**
  - Owns skills, SOPs, routines, workflows, tool/MCP discovery, shared memory/knowledge, subagent specs, and quorum/council logic.
  - Coordinates and recommends; it does not bypass daemon permissions.
  - May host model subagents and council workflows, but local file edits/bash/destructive actions must route back through the daemon execution authority.

This separation lets Ocean become team-aware without turning a remote service into a root shell on every coworker laptop.

## What Longhouse centralizes

Longhouse is the canonical place for:

1. **SOPs** — company/project operating procedures, guardrails, review rules, escalation policies.
2. **Routines/workflows** — repeatable preflight/planning/check/review sequences.
3. **Tools and MCP discovery** — what tools exist, when to use them, and which daemon can execute them.
4. **Skills** — compact, task-specific capability briefs that can be injected into the main Ocean agent.
5. **Data/memory layers** — company memory, project memory, per-agent notes, run records, and knowledge retrieval.
6. **Subagent specs/runtimes** — role, objective, model policy, allowed tools, memory namespace, output schema, max turns, and budget.
7. **Quorum/council workflows** — multi-agent propose → endorse/inhibit → converge flows with deterministic quorum arithmetic.

## Deployment modes

Longhouse should support the same API shape across four modes:

- **`disabled`** — no Longhouse integration. Ocean behaves with built-ins/MCP only.
- **`embedded`** — daemon exposes Longhouse routes in-process. This exists today for demo/convene routes.
- **`local`** — Longhouse runs as a separate local Rust service, usually:

  ```bash
  cargo run -p ocean-longhouse --bin ocean-longhouse -- serve
  ```

  Default bind target: `127.0.0.1:4781`.

- **`remote`** — local daemons connect to a team Longhouse over HTTPS with an `OCEAN_TOKEN`. The daemon still owns local execution authority.

## Configuration shape

The intended daemon-facing configuration model is:

```text
mode: disabled | embedded | local | remote
url: optional service URL
token_env: environment variable name for bearer token
timeout_ms: request timeout
```

Environment variables:

- `OCEAN_LONGHOUSE_MODE` — one of `disabled`, `embedded`, `local`, `remote`.
- `OCEAN_LONGHOUSE_URL` — service URL, e.g. `http://127.0.0.1:4781`.
- `OCEAN_LONGHOUSE_TOKEN_ENV` — token env var name; defaults to `OCEAN_TOKEN`.
- `OCEAN_TOKEN` — bearer token for remote Longhouse.
- `OCEAN_LONGHOUSE_TIMEOUT_MS` — HTTP timeout budget.

Defaults should be conservative: disabled or embedded-only unless explicitly configured.

## API direction

Existing embedded daemon routes:

- `POST /v1/longhouse/demo`
- `POST /v1/longhouse/convene`

Local/remote service shape now starts with:

- `GET /health`
- `POST /v1/council/convene` — standalone HTTP wrapper around the existing `convene` flow; returns the ordered `LonghouseEvent` list plus outcome JSON.

Future Longhouse APIs should add:

- `GET /v1/skills/query` or `POST /v1/skills/query`
- `GET /v1/skills/fetch`
- `POST /v1/workflows/prepare`
- `POST /v1/subagents/spec`

Keep the daemon-side embedded routes working while adding the standalone service; clients and daemons can bridge versions during migration.

## First safe integration slice

The smallest useful dynamic integration is a **read-only preparation step**:

1. Daemon receives a user turn.
2. If Longhouse is enabled, daemon sends a compact task/session brief to Longhouse.
3. Longhouse returns compact active briefs:
   - relevant skills,
   - SOP reminders,
   - workflow/routine suggestions,
   - optional subagent/council recommendation.
4. Daemon injects those briefs into the main agent prompt/context.
5. Main agent still uses daemon tools and permission gates for all local execution.

This creates value without allowing Longhouse to perform local side effects.

## Skill Librarian future

Longhouse should eventually index and query:

- `~/.spawner/skills/**/skill.yaml`
- `~/.codex/skills/**/SKILL.md`
- repo-local `./skills/**`

Selection path:

1. Deterministic prefilter from task/session brief.
2. Cheap fast model reranker.
3. Return 3–7 compact skill briefs.
4. Daemon injects those briefs into the main Ocean agent.

## Subagent future

Longhouse should assemble subagent specs from skills + routines + token scopes + memory + model/tool policy.

A subagent spec should include:

- role,
- objective,
- model policy,
- skill ids,
- allowed tools,
- memory namespace,
- output schema,
- max turns,
- budget.

Subagents can be orchestrated by Longhouse, but local side effects still return through daemon permission gates.

## Current implementation notes

`crates/ocean-longhouse` is a Rust crate. It already contains:

- deterministic quorum logic in `quorum.rs`,
- cheap-model single-turn workers using the existing Ocean provider stack in `agent.rs`,
- real council flow in `convene.rs`,
- replay/tuning support in `replay.rs` and `lh-tune`.

`ocean-daemon` currently embeds Longhouse routes for demo and convene flows. The next step is to let `ocean-longhouse` also run as a local service on `127.0.0.1:4781`, then teach the daemon to consult it dynamically in read-only preparation mode.
