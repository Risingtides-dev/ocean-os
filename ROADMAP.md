# Ocean OS roadmap

Status: open work only. Implemented behavior belongs in
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); completed work belongs in
[`events.md`](events.md) or retained characterization reports.

This roadmap is intentionally short. An unchecked item is a direction, not an
approved design or permission to alter public contracts.

## Near-term integration gaps

- [ ] Reconcile the Tauri surface identity. `ocean-surface` currently emits
      `client_type = "surface-tauri"`, while `ocean-agent::surface_flag` does not
      map that value; Tauri turns therefore use the generic fallback profile
      instead of an authored Ocean Agents profile.
- [ ] Wire configured lifecycle hooks into the completed-turn path. The
      `ocean-hooks` protocol and config parsing exist, but production turn code
      does not currently call `run_hooks`.
- [ ] Keep cross-repository session, voice, component, and room contracts under
      executable drift checks rather than prose-only synchronization.

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
