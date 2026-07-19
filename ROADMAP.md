# Ocean OS roadmap

Status: open work only. Implemented behavior belongs in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); completed work belongs in
[`events.md`](events.md) or retained characterization reports.

This roadmap is intentionally short. An unchecked item is a direction, not an
approved design or permission to alter public contracts.

## Near-term integration gaps

- [ ] Decide the Tauri effective harness classification. `ocean-agent` now maps
      `client_type = "surface-tauri"` to the `TAURI` prompt identity and native-
      shell component guidance, while the daemon harness seam intentionally
      retains CLI-compatible hashline/artifact gates pending cross-repo policy.
- [ ] Wire configured lifecycle hooks into the completed-turn path. The
      `ocean-hooks` protocol and config parsing exist, but production turn code
      does not currently call `run_hooks`.
- [ ] Keep cross-repository session, voice, component, and room contracts under
      executable drift checks rather than prose-only synchronization.

## Ocean Observatory

- [x] Gate 0 decisions accepted — see [`docs/specs/2026-07-17-observatory-gate0-decisions.md`](docs/specs/2026-07-17-observatory-gate0-decisions.md), including the operator's 90s-game visual-parity ruling on truthful events with a durable event store.
- [x] Gate 1 implementation manifest accepted on 2026-07-17 — schema, auth token format, persistence contract, admission/binding contract, strict task dependencies, and test requirements are fixed in [`docs/specs/2026-07-17-observatory-gate1-implementation-manifest.md`](docs/specs/2026-07-17-observatory-gate1-implementation-manifest.md).
- [ ] Implement and independently review the daemon-owned metadata projection, durable ordered replay, authenticated snapshot/live API, and extension admission/binding contract before building the production animated Surface renderer.
- [ ] Repair and contract-test end-to-end event resume through the Surface proxy (`Last-Event-ID` or an approved explicit cursor equivalent); do not use `/v1/agent/events?all=1` as the product feed.

## Harness evolution

The source-researched mechanism inventory and dated implementation matrix live in
[`docs/specs/2026-07-03-omp-port-map.md`](docs/specs/2026-07-03-omp-port-map.md).
Ocean ports mechanisms into current owners rather than reproducing OMP package boundaries.

- [x] Reconcile the effective harness-profile contract: only hashline edits and artifact spill
      are surface-scoped and carried into `PromptControl`; LSP/memory remain globally registered,
      while unwired stream-rule/rich-context/minimizer claims were removed. New external surface
      classification remains a separate policy decision above.
- [x] Bound and attribute the post-turn advisor before broadening it: authoritative source-turn
      identity, a dedicated two-permit fail-open limiter, a fixed 30-second timeout, and
      fixed-cardinality outcome/latency metrics are live in `ocean-daemon`.
- [x] Make the default-on Longhouse pre-turn consult inspectable and tune relevance while
      preserving its read-only, permission-neutral, fail-open boundary: the path-redacted
      `/v1/longhouse/inspect` projection and fixture-backed exact-token ranker are live.
- [x] Implement the standalone M1 command-output minimizer as a dependency-free,
      already-tokenized library with fixed conservative cargo/git/gh/npm/npx/pytest filters.
      It is intentionally outside `default-members` and has no live runtime wiring.
- [x] Design the command-capture/runtime integration for `ocean-minimizer` over the existing
      artifact and capability seams. The reviewed M2 design limits integration to explicitly
      tokenized model-invoked Bash argv, keeps live/checkpoint/session history raw, minimizes only
      active-run provider request clones, and pins exact recovery artifacts for that run; design
      acceptance did not enable a harness-profile capability.
- [ ] Implement the reviewed minimizer M2 design as a characterization-first checkpoint.
- [ ] Port the standalone shared walker mechanism as an independent, unwired checkpoint.
- [ ] Build a typed search engine over the accepted walker, then adopt it in live `grep`/`glob`
      only after explicit parity review; do not bundle traversal, search, and runtime replacement.
- [ ] Route isolation, task dispatch, typed yields, joins, budgets, and orchestration policy
      through the approved extension architecture; do not revive their superseded core placement.

## Reliability and scale

- [ ] Design a bounded policy for the runtime-to-daemon per-turn event channel;
      replay retention is bounded, but the live per-turn MPSC design remains a
      separate decision.
- [ ] Establish artifact-backed large tool results before increasing retained
      event or transcript payload limits.
- [ ] Add end-to-end retained-size and slow-client measurements before changing
      history/event architecture.
- [ ] Continue process-tree portability work beyond the current Unix
      process-group guarantee if non-Unix execution becomes supported.

## Structure

The active behavior-neutral daemon extraction is tracked in [`docs/DAEMON_REFACTOR_MISSION.md`](docs/DAEMON_REFACTOR_MISSION.md), not duplicated as unchecked roadmap items. The executable 72-route parity seam, CORS leaf, and metrics leaf have landed; subsequent checkpoints remain bounded by that mission and their extraction manifests.

- [ ] Split the intact session module, `AppState`, or public daemon shape only under a separately approved architecture plan after the behavior-neutral mission reaches its target.

## Platform and operations

- [ ] Decide whether to support systemd and a Unix socket; macOS launchd and
      loopback HTTP are the current operated path.
- [ ] Define sandbox profiles before advertising stronger isolation than
      permission gates and cwd/process controls provide.
- [ ] Keep supported model, feature, release, and Rust 1.88 compatibility lanes
      truthful as the dependency graph changes.

## Explicitly not implied

This file does not approve legacy API removal, session schema changes, room
federation, Longhouse economic policy, a daemon library, or a performance
rewrite. Those require current evidence, a bounded design, compatibility gates,
and independent review.
