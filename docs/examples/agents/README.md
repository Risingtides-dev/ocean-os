# Example folder-as-agent

A working reference agent for Ocean's folder-as-agent system — copy it, rename
the folder, edit the files. Full convention: [`docs/specs/folder-as-agent.md`](../../specs/folder-as-agent.md).

## Layout

```
researcher/                      the agent name IS the folder name
├── agent.toml                   model, description, tools, capabilities
├── instructions.md              the system prompt (the only required slot)
├── skills/
│   └── summarize.md             an on-demand procedure
└── subagents/
    └── fact-checker/            a specialist child, same shape
        ├── agent.toml           must declare a `description`
        └── instructions.md
```

Identity comes from the path — you never write a `name`/`id` field.

## Use it

1. Copy `researcher/` into your agents root — `$OCEAN_AGENTS_DIR`, else
   `<ocean-config>/agents/` (sibling of `assistants/`).
2. `GET /v1/agents` — the daemon discovers it (returns name, description, model,
   skill count, subagents).
3. Run it on a turn: `POST /v1/agent/turns` with `"agent": "researcher"` — the
   agent's `instructions.md` drives the turn. Omit `agent` for the default
   surface profile (unchanged).

Hot-read: edit a file, the next turn picks it up — no rebuild.

This example is resolved by a unit test (`shipped_example_agent_resolves`), so it
stays valid as the resolver evolves.
