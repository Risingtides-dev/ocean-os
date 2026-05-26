# Tides Mesh Development Team Agents

## What this is

This directory is the canonical source for the Rising Tides development-team personas used inside Tides Mesh.

Each file is intended to be usable as a pi system prompt for a fresh specialist session. These prompts are sanitized from older persona drafts in `Risingtides-dev/rising-tides-agents/personas/` and tuned for the current Rising Tides stack, not a generic Vercel/Supabase/Next.js template. Older drafts elsewhere are superseded for Tides Mesh runtime use.

Patch-scope note: this README and `rotation.md` may be included in a docs-only rotation update without including every persona prompt file. Persona prompt files (`owl.md`, `knox.md`, `brick.md`, etc.) may be maintained, reviewed, staged, or added separately unless an operator/reviewer explicitly includes them in the current patch.

## Rotation guide

Every Tides Mesh agent should read `rotation.md` at shift start. It defines authority order, Ocean framing, lane ownership, handoff format, review gates, and Glyph ledger inputs.

## Current cast

### Coordination roles
- **OWL** — `owl.md`; orchestrator. Plans waves, routes work, keeps the mesh clean, and drives revise/review/redo loops.
- **KNOX / Rev** — `knox.md`; reviewer, PR/Git handler, release gatekeeper.

### Worker personas
- **BRICK** — `brick.md`; backend/API/runtime specialist.
- **PIXEL** — `pixel.md`; frontend/app/operator UX specialist.
- **FLUX** — `flux.md`; optional generalist implementation agent for mixed tickets.
- **Charlotte** — `charlotte.md`; research, intel, docs, and implementation briefs.
- **Henry** — `henry.md`; Writer’s Room agent for drafting, rewriting, scripts, and editorial synthesis.
- **Glyph** — `glyph.md`; ledger/minutes/audit agent for Ocean/Tides decisions, evidence, failures, handoffs, and follow-ups.

The main Tides Mesh worker panes are **KNOX**, **BRICK**, **Charlotte**, **PIXEL**, and **Glyph**. Window `#2 WritersRoom` runs **Henry** beside NoteDash and filetree. Window `#7` is reserved for the larger **Rev** review/PR workspace.

The prompt-box info line should show `TIDES-MESH <callsign>` for every Tides Mesh terminal:

- orchestrator/OWL: `TIDES-MESH orchestrator`
- BRICK: `TIDES-MESH BRICK`
- PIXEL: `TIDES-MESH PIXEL`
- FLUX: `TIDES-MESH FLUX`
- Charlotte: `TIDES-MESH Charlotte`
- Rev/KNOX: `TIDES-MESH Rev` or `TIDES-MESH KNOX`, depending on launcher alias.
- Henry: `TIDES-MESH Henry`
- Glyph: `TIDES-MESH Glyph`

## Target model profiles
These are operator-facing defaults. Switch manually in pi with `/model` or Ctrl+L unless/until the launcher enforces them.

| Callsign | Target model profile | Notes |
|---|---|---|
| **orchestrator / OWL** | `openai-codex/gpt-5.5:high` | leadership, routing, planning, safety decisions |
| **KNOX / Rev** | `openai-codex/gpt-5.4:high` | review/PR/deploy gatekeeper; strong but not flagship-default |
| **BRICK** | `openai-codex/gpt-5.4-mini:high` | backend/API/runtime build work with lower cost/latency |
| **Charlotte** | `moonshot/kimi-k2.6:high` | research, intel, synthesis, implementation briefs |
| **PIXEL** | `openai-codex/gpt-5.3-codex-spark:minimal` | frontend/app/operator UX lane starts fast |
| **FLUX** | `openai-codex/gpt-5.3-codex-spark:minimal` | fast generalist mixed-ticket lane |
| **Henry** | `minimax/MiniMax-M2.7` | Writer’s Room drafting/rewriting lane |
| **Glyph** | `openai-codex/gpt-5.3-codex-spark:minimal` | ledger/minutes/audit lane; hook-driven Ocean/Tides summaries |

### Optional background helpers
These are not persistent agents. They are temporary helpers created only with explicit scope and cleanup.

- **SAGE** — `sage.md`; strategic architect.
- **KAI** — `kai.md`; context/workflow engineer.
- **sdk-reference-researcher** — `sdk-reference-researcher.md`; SDK/API documentation lookups.

## Current stack bias

Prefer and assume:

- **Runtime/orchestration:** pi runtime, pi_messenger, Tides Mesh, tmux on Lennox.
- **Host:** Railway-hosted Arch Linux container with Dockerfile/bootstrap scripts.
- **Process supervision:** `catatonit` PID 1, shell entrypoint, Caddy reverse proxy.
- **Edge/public:** Cloudflare Workers, routes, DNS, Tunnels, R2.
- **Backend languages:** JavaScript, TypeScript, Python, and Rust when justified.
- **API patterns:** Cloudflare Worker fetch handlers, Hono-style routing where added intentionally, Express for local helpers, FastAPI for Python services when appropriate.
- **Storage:** repo markdown for instructions, file-backed `.pi` state for current mesh, Cloudflare R2 for assets/backups, Postgres/SQLite only when a workflow explicitly adopts it.
- **Interfaces:** Telegram, web dashboard, Notion/Linear/GitHub/Slack as integrations become connected.

Do **not** assume Vercel, Supabase, Next.js, Prisma, or Upstash unless a specific repo/task already uses them or the human approves adding them.

## Launch pattern

Use the launcher so the matching prompt file is loaded as a fresh pi system prompt:

```bash
cd /root/dev
scripts/start-tides-agent.sh brick "Run pi_messenger status and wait for assignment."
```

Supported launcher names:

```text
owl
knox / rev
brick
pixel
flux
charlotte
henry / writer
glyph / ledger
sage
kai
sdk-reference-researcher / sdk
```

Manual equivalent:

```bash
PI_MESH_MODE=1 TIDES_MESH_CALLSIGN=BRICK PI_AGENT_NAME=BRICK pi --system-prompt "$(cat docs/agents/tides-mesh/brick.md)"
```

For Tides Mesh panes, launch only into an idle placeholder pane or with explicit human approval.
