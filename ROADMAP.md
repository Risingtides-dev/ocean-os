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
- [ ] Keep cross-repository session, voice, component, and room contracts under
      executable drift checks rather than prose-only synchronization.

## Ocean Rooms distributed workspace

Architecture ratified 2026-08-17 in
[`docs/specs/2026-08-16-ocean-rooms-distributed-workspace-architecture.md`](docs/specs/2026-08-16-ocean-rooms-distributed-workspace-architecture.md).
Rooms is the authorization and work context through which members contribute
folders and compute from their Ocean nodes and room-authorized agents work
across those computers. The architecture does not itself authorize code changes.

- [x] Ratify room-scoped agent bindings, contributed resource grants, the logical
      cross-node namespace, Tailscale-backed direct operations, node-local
      enforcement, and extension-owned multi-node workflow policy.
- [ ] Resolve the Phase 0 identity, trust, public coordinator, tailnet boundary,
      approval-default, revocation, transfer-budget, and device-loss decisions.
- [ ] Write and independently review the exact Phase 1 implementation manifest
      for local room-agent authorization before changing routes or schemas.

## Ocean Observatory

- [x] Gate 0 decisions accepted — see [`docs/specs/2026-07-17-observatory-gate0-decisions.md`](docs/specs/2026-07-17-observatory-gate0-decisions.md), including the operator's 90s-game visual-parity ruling on truthful events with a durable event store.
- [x] Gate 1 implementation manifest accepted on 2026-07-17 — schema, auth token format, persistence contract, admission/binding contract, strict task dependencies, and test requirements are fixed in [`docs/specs/2026-07-17-observatory-gate1-implementation-manifest.md`](docs/specs/2026-07-17-observatory-gate1-implementation-manifest.md).
- [ ] Implement and independently review the daemon-owned metadata projection, durable ordered replay, authenticated snapshot/live API, and extension admission/binding contract before building the production animated Surface renderer. Implementation (tasks 2–8) is landed and the Task 9 independent review is retained at [`docs/specs/2026-07-20-observatory-gate1-task9-independent-review.md`](docs/specs/2026-07-20-observatory-gate1-task9-independent-review.md); its gating repairs G1–G5 must land and pass delta review before this gate opens.
- [ ] Repair and contract-test end-to-end event resume through the Surface proxy (`Last-Event-ID` or an approved explicit cursor equivalent); do not use `/v1/agent/events?all=1` as the product feed.

## Ocean Browser (OceanWebKit)

Program direction ratified 2026-07-19 in
[`docs/specs/2026-07-19-ocean-webkit-browser-program.md`](docs/specs/2026-07-19-ocean-webkit-browser-program.md):
a custom WebKit engine with earned Chrome DevTools protocol parity, built
outside the Cargo graph in a dedicated `ocean-webkit` repository.

- [x] Quarantine the Chromium backend behind the default-off `legacy-chromium`
      feature; default builds compile no chromiumoxide, the 19 `browser_*` tool
      schemas stay pinned, and the daemon browser routes serve a frozen
      `no-browser` contract without an engine.
- [x] Keep interim browsing on the supervised daemon: `ops/install-ocean-daemon.sh`
      builds with `--features legacy-chromium` until the OceanWebKit host ships.
- [ ] Pass the first hard checkpoint (manifest §7): pinned-fork MiniBrowser, one
      custom CDP command end-to-end, full-traffic capture with bodies, trusted
      Automation input, unmodified Chrome DevTools frontend connection, minimal
      Tauri embedding, signed helper packaging, and build/size/memory measurements.
- [ ] Earn per-domain CDP parity with generated conformance tests; ship the
      usable browser at the manifest's ship gate rather than protocol completeness.

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
- [x] Port the standalone shared walker mechanism as an independent M1 crate. It is
      intentionally outside `default-members`; only the standalone typed-search crate consumes it,
      and neither crate is wired into production runtime capabilities.
- [x] Build the standalone typed-search M1 over the accepted walker with bounded typed output,
      exact native path identity, strict candidate policy, fresh opened-handle validation, and
      deterministic ordered commit. It remains outside `default-members` and unwired.
- [ ] Adopt standalone typed search in live `grep`/`glob` only after explicit parity and security
      review; do not bundle the M1 engine with runtime replacement. Runtime adoption must add
      point-of-use descriptor/handle-relative confinement for adversarial roots, intermediate
      renames, symlink/reparse swaps, cached candidates, and every supported OS; walker
      `FollowLinks`/`same_file_system` and search leaf-open controls are not a sandbox.
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
