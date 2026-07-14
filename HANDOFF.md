# Ocean OS handoff

This is an evergreen routing file, not a branch, deployment, or worktree
snapshot.

## Current authority

- Work contract: [`AGENTS.md`](AGENTS.md)
- Documentation map and status: [`docs/README.md`](docs/README.md)
- Architecture: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
- Operations and deployment: [`docs/OPERATIONS.md`](docs/OPERATIONS.md)
- Package ownership and tests: [`crates/AGENTS.md`](crates/AGENTS.md)
- Cross-repository boundary: [`docs/OCEAN_PROJECT_MAP.md`](docs/OCEAN_PROJECT_MAP.md)
- Open work: [`ROADMAP.md`](ROADMAP.md)
- Chronology: [`events.md`](events.md)

Before continuing work, derive current state directly:

```bash
git status --short --branch
git log -1 --oneline --decorate
curl -fsS http://127.0.0.1:4780/health
```

Historical handoff snapshots and completed programs are optional evidence, not
active instructions. Material that could redirect a cold agent away from the
current state belongs under `docs/.agentarchive/`.
