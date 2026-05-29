# Ocean-OS — Agent Guidance

> Agentic knowledge layer for Rising Tides. See [README.md](README.md) for full product/architecture context.

## Team & Repo Routing

**This repo is the canonical home for the `Ocean-OS` Linear team.** Linear issue prefix: `OCEAN-NNN`.

Team is the routing primitive — not project. Workspace-wide map:

| Linear team | Canonical GitHub repo |
|---|---|
| Campaign Hub | `Risingtides-dev/risingtides-campaign-hub` |
| Sales-Agents | `Risingtides-dev/sales-agent` |
| Ocean-OS    | `Risingtides-dev/ocean-os` *(this repo)* |
| Content-hub | `KINGMAKER-SYSTEMS/content-posting-lab` |

**Rules for agents working on `OCEAN-NNN` issues:**

- Use **only** `Risingtides-dev/ocean-os` for implementation, branches, commits, PRs, and code investigation.
- If a Linear issue mentions or links to a different repo, **flag it as misrouted** and state which repo it should belong to — do not start implementation.
- Before coding, inspect the issue title, description, parent/related issues, existing GitHub links, and recent PRs to confirm repo fit. If still ambiguous, **stop and ask** instead of guessing.
- Post all implementation updates back to the Linear issue: branch name, PR link, merge status, and any required follow-up steps.
- Never open PRs in `risingtides-campaign-hub`, `sales-agent`, or `content-posting-lab` for Ocean-OS work.

**Hard rule:** do not guess the repository. Do not silently switch repositories. If cross-repo work is genuinely required, state that explicitly on the Linear issue before proceeding.
