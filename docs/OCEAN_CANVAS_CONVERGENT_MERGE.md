# Ocean Canvas — Convergent Merge for Concurrent Edits (OCEAN-258)

Status: foundation landed in `ocean-os` (`ocean-agent-sdk`). Surface application is
a documented follow-up (see [Surface integration](#surface-integration-the-ledger-is-the-merge-point)).

This is the bounded **CRDT-lite** that lets the operator and an agent edit the
*same* canvas concurrently without clobbering each other. It is the staged answer
to the open CRDT decision in `gpui_masterbuild.md` §17 — built on the existing
patch log, **not** a full CRDT engine.

## The problem

`docs/OCEAN_ECOSYSTEM_CONTRACT.md` calls a Canvas "a tldraw/CRDT document," but
patches today are a serialized append list applied in **arrival order**. The
ledger's `apply_patch` (in `ocean-surface`, `crates/ocean-gui/src/shell/canvas/ledger.rs`)
is blind last-writer-wins: whichever patch the daemon happened to fan out last
wins, and two surfaces can diverge. Two writers on the same component clobber.

The patch log is already **deterministic** (replay the same envelopes in the same
order → same ledger). The missing piece is **concurrent-edit merge**: when the
operator drags a card and an agent repositions the *same* card, their two patches
have no inherent order and must converge to one deterministic result.

## The mechanism: per-component last-write-wins by a logical clock

Source of truth: `crates/ocean-agent-sdk/src/surface_merge.rs`.

Each component carries a **`ComponentVersion { rev: u64, actor: ActorId }`**:

- `rev` — a per-component **Lamport-style revision counter** (a monotonic logical
  clock, *not* wall-clock — `created_at_ms` is unreliable across actors).
- `actor` — a stable, orderable **`ActorId`** derived from the patch's `ActorRef`
  (`"human:operator"`, `"agent:sage"`, `"system"`).

`ComponentVersion` has a **total order**: compare `rev` first, then `actor` as the
deterministic tiebreak. A write supersedes the stored state **iff its version is
strictly greater** in that order. This is a Last-Write-Wins Register keyed per
component:

- **Commutative / order-independent** — `merge(a, b) == merge(b, a)`; both pick the
  `max` version. Two concurrent writes to the same component converge to the *same*
  winner no matter which order each surface applies them in.
- **Idempotent** — re-applying an already-merged write (e.g. an SSE redelivery) is a
  no-op.
- **Per-component** — writes to *different* components never contend; both land.

### Types (all in `ocean-agent-sdk::surface_merge`)

| Type | Role |
|---|---|
| `ActorId` | Stable, orderable actor identity from `ActorRef`. The tiebreak. |
| `ComponentVersion { rev, actor }` | Per-component logical version with a total order. `supersedes()` is the merge decision. |
| `LamportClock` | Per-actor logical clock. `tick()` on local write; `observe(remote_rev)` advances past anything seen. Generates converging `rev`s. |
| `CanvasMergeState` | The per-canvas **version vector** (`BTreeMap<ComponentId, ComponentVersion>`). `merge(id, incoming) -> MergeDecision` is the commutative op the whole guarantee rests on. |
| `MergeDecision` | `Applied` (write won → apply the patch) / `Superseded` (write lost → skip it). |

### Wire change

`SurfacePatchEnvelope` (`ocean-agent-sdk::surface`) gained an **optional**
`version: Option<ComponentVersion>`:

- `#[serde(default, skip_serializing_if = "Option::is_none")]` — **additive**.
  Producers that predate the merge layer (and the `ocean-surface` mirror until it
  adopts versioning) omit it; it's absent on the wire for them.
- Mutations that don't last-write-wins a single component leave it `None`.
  `SurfacePatch::target_component()` returns the contended component for the
  per-component ops (`UpsertComponent` / `MoveComponent` / `ResizeComponent` /
  `DeleteComponent`) and `None` for the rest (`Connect`/`Disconnect` mutate an edge;
  `Select`/`Focus`/`SetViewport` are view state; `Layout`/`Group` touch many
  components as a unit).

## What converges deterministically

- ✅ **Same component, concurrent writes** → one deterministic winner, identical on
  every replica regardless of arrival order (higher `rev`; equal `rev` → higher
  `actor` id).
- ✅ **Different components, concurrent writes** → both land.
- ✅ **Out-of-order / stale delivery** → a late lower-`rev` patch can't stomp a newer
  one.
- ✅ **Replays** → idempotent no-op.

Proven by the test module in `surface_merge.rs` (17 tests), including end-to-end
tests that drive real `SurfacePatchEnvelope`s through the merge the way a ledger
will (`two_concurrent_patches_to_same_component_converge_via_envelopes`,
`concurrent_patches_to_different_components_both_land_via_envelopes`,
`out_of_order_stale_envelope_does_not_clobber_newer_one`).

## Surface integration (the ledger is the merge point)

> **Why the ledger, not the daemon?** `gpui_masterbuild.md` §4: *"No daemon table
> should become the source of truth for the canvas."* The operator's **local**
> drags/resizes and the agent's **streamed** patches actually *meet* in the
> `ocean-surface` `CanvasLedger`. That is the one place both writers' patches are
> seen, so that is where the merge runs. The daemon stays a transport and relays
> agent patches with `version: None` (see `crates/ocean-daemon/src/main.rs`, the
> `AgentEvent::SurfacePatch` relay).

The follow-up work in **`ocean-surface`** (`crates/ocean-gui/src/shell/canvas/`):

1. **Mirror the SDK types.** `ocean-surface` keeps a structural copy of the patch
   wire types in `canvas/patch.rs` (it doesn't depend on the ocean-os workspace).
   Add the mirror of `ComponentVersion` / `ActorId` and the optional
   `version` field on its `SurfacePatchEnvelope`, exactly as the SDK has them. (The
   JSON is identical; the only existing intentional difference is `session_id` is a
   `String` there.)

2. **Hold a `CanvasMergeState` + per-actor `LamportClock` on the ledger.** Add both
   to `CanvasLedger` (`canvas/ledger.rs`) next to `patch_log`. Seed the clock with
   `CanvasMergeState::max_rev()` when resuming from disk (`canvas/persistence.rs`)
   so fresh local writes are strictly greater than the replayed history.

3. **Gate `apply_patch` through the merge.** Today `CanvasLedger::apply_patch`
   (ledger.rs ~line 229) applies unconditionally. Change the apply path so that for
   a patch with `patch.target_component() == Some(id)`:
   - **local edit** (operator drag/resize): `let rev = clock.tick();` stamp
     `ComponentVersion { rev, actor: operator }`, `merge_state.merge(id, v)` (always
     wins locally), then apply + record the version on the envelope.
   - **remote/agent patch** (from the daemon `SurfacePatch` event, applied in
     `shell/view.rs` `apply_patches_to_ledger_with_store` ~line 8135): if the
     envelope carries a `version`, `clock.observe(version.rev)` then
     `match merge_state.merge(id, version) { Applied => apply, Superseded => skip }`.
     If the envelope has **no** version (legacy), fall back to today's direct apply.
   - Patches with `target_component() == None` apply directly as today.

4. **Stamp agent patches with a version somewhere authoritative.** Because the daemon
   relays `version: None`, the surface assigns the agent patch's `rev` from the
   canvas clock at apply time (step 3, remote branch). This keeps a single clock per
   canvas on the ledger — no split-brain.

The merge call site is small and local; the SDK already proves the convergence math
and the envelope routing. The surface change is "where to call `merge` and which
branch stamps the version," documented above.

## Scope — foundation, not full multiplayer

**In scope (landed):** version stamps on patches, the deterministic commutative
per-component merge, the Lamport clock, the optional wire field, and the
convergence test suite.

**Out of scope (follow-up):**

- A full CRDT library (Loro / Yjs / Automerge) and rich text/tree CRDTs. Per
  masterbuild §17 this is deferred until "after patch semantics stabilize"; the
  version-vector merge here is the staged step before it. Loro becomes worth it when
  per-component LWW is too coarse — e.g. two writers editing *different fields of the
  same component's content* should both survive (LWW currently picks one whole
  version). The merge boundary could later move from per-component to per-field
  without changing the wire envelope.
- **Presence** — live cursors, shared selection, "who's editing what."
- **Real-time sync transport** — today versioned patches ride the existing daemon SSE
  fan-out (one writer at a time per turn). A peer-to-peer / multi-writer transport
  that ships versioned patches between surfaces concurrently is separate work.
