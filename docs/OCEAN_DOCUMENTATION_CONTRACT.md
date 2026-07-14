# Ocean documentation contract

Status: current cross-repository documentation policy. Ocean OS owns the
canonical ecosystem map; each repository owns its local implementation and
operating truth.

## Purpose

Documentation must let a human or cold agent answer, without reconstructing
project history:

1. What does this repository own?
2. What is implemented now?
3. Where does execution or state authority live?
4. How is it built, tested, run, deployed, and recovered?
5. Which source files and manifests are authoritative?
6. What remains open?
7. Which material is historical only?

## Document classes

### Current contract

Present-tense implemented behavior, ownership, invariants, and operations.
Current contracts name source anchors and verification. They are rewritten when
truth changes; they do not accumulate dated overlays.

Examples: `README.md`, `AGENTS.md`, architecture, operations, API, design-system,
and package-index documents.

### Reference

Detailed protocol, subsystem, schema, or troubleshooting material. A reference
may be longer than an entry-point document, but it must identify its authority
and must not silently become a roadmap.

### Plan

Proposed or approved future work. Every plan must distinguish built, pending,
and rejected scope. A plan is not current architecture and should stop being a
default entry point when complete.

### Historical evidence

Completed plans, old handoffs, migration diaries, benchmark snapshots, and
incident evidence. Retain these where useful, but keep them out of default
onboarding and clearly separate them from current contracts.

## One authority per fact

- Ocean OS owns the canonical four-repository connection map in
  `ocean-os/docs/OCEAN_PROJECT_MAP.md`.
- Each sibling repository keeps a short local boundary page pointing to that
  canonical map; it does not copy the full topology.
- Each package or service inventory is derived from its real manifest or source
  registry whenever possible.
- API prose points to typed source or a machine-readable declaration.
- Deployment claims name the reproducible script/config that proves them. If a
  deployment depends on external control-plane state not stored in the repo,
  say so explicitly.

## Human and agent entry points

Every Ocean repository should provide:

- `README.md` — human orientation and quick start;
- `AGENTS.md` — binding work contract and validation;
- `docs/README.md` — documentation map with status classification;
- an implemented architecture document;
- an operations/runbook document;
- a roadmap containing open work only;
- `events.md` — append-only chronology.

Domain-specific references remain local: provider/runtime references in
Ocean OS, package authoring in Ocean Agents, API/data-plane references in
Bedrock, and product/platform/design references in Ocean Surface.

## Writing rules

- Start from source, manifests, tests, workflows, and deployment scripts—not an
  old plan or handoff.
- Use present tense for current behavior and future tense only in roadmap/plan
  material.
- Do not mix live state such as PIDs, dirty-file counts, balances, or current
  deployment hashes into durable architecture.
- Avoid copied inventories that can drift. Link to the canonical index or make
  the inventory executable.
- Mark known mismatches honestly; documentation cleanup must not pretend a code
  gap is solved.
- Prefer exact commands that can be run from a clean checkout.
- Never include credentials, secret values, or private token locations.

## Change and verification policy

A behavior change updates the nearest owning current contract in the same
change. A docs-only reset must run that repository's documentation/link check
and any source-derived inventory check. Cross-repository claims require reading
the target repository's current source or canonical contract.

A documentation wave is complete when:

- a cold reader can route ownership and run the supported checks;
- current, planned, and historical material are visibly separated;
- active local links resolve;
- generated/manifests-backed indexes pass;
- no current entry point depends on archived context;
- each repository lands independently from a clean, current base.
