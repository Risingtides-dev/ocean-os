# Ocean Canvas — Convergent Merge for Concurrent Edits (OCEAN-258)

Status: landed in `ocean-os` (`ocean-agent-sdk`) and the shared
`ocean-surface-ui` canvas ledger.

This is the bounded **CRDT-lite** that lets the operator and an agent edit the
*same* canvas concurrently without clobbering each other. It is the staged answer
for the existing patch log, **not** a full CRDT engine.

## The problem

`docs/OCEAN_ECOSYSTEM_CONTRACT.md` calls a Canvas "a tldraw/CRDT document."
Without a merge gate, its serialized patch stream would be applied in arrival
order, so two writers updating the same component could diverge.

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

The daemon remains transport. Local operator changes and streamed agent patches
meet in the shared Surface ledger, so that ledger owns the merge decision.

The implementation lives in:

- `../ocean-surface/crates/ocean-surface-ui/src/daemon.rs`: structural wire
  mirrors for `ActorId`, `ComponentVersion`, `LamportClock`,
  `CanvasMergeState`, and the envelope's optional `version`.
- `../ocean-surface/crates/ocean-surface-ui/src/canvas.rs`:
  `WebCanvasLedger` holds one merge state and logical clock per canvas.
  `apply_envelope` observes carried versions, stamps unversioned daemon patches,
  accepts `Applied`, skips `Superseded`, and directly applies patches that do
  not target one component.
- `MultiCanvasLedger` keeps merge state isolated by `canvas_id` and reconstructs
  the same merge-gated scene during replay.

This keeps the rendered state and the next-turn canvas context on the same
authoritative client-side ledger path.

## Scope — foundation, not full multiplayer

**In scope (landed):** version stamps on patches, the deterministic commutative
per-component merge, the Lamport clock, the optional wire field, and the
convergence test suite.

**Out of scope (follow-up):**

- A full CRDT library (Loro / Yjs / Automerge) and rich text/tree CRDTs. The
  version-vector merge here is the staged step before it. Loro becomes worth it when
  per-component LWW is too coarse — e.g. two writers editing *different fields of the
  same component's content* should both survive (LWW currently picks one whole
  version). The merge boundary could later move from per-component to per-field
  without changing the wire envelope.
- **Presence** — live cursors, shared selection, "who's editing what."
- **Real-time sync transport** — today versioned patches ride the existing daemon SSE
  fan-out (one writer at a time per turn). A peer-to-peer / multi-writer transport
  that ships versioned patches between surfaces concurrently is separate work.
