//! Ocean **surface convergent-merge layer** — the CRDT-lite that lets the
//! operator and an agent edit the *same* canvas live without clobbering each
//! other (OCEAN-258, E10-P3).
//!
//! # Why this exists
//!
//! The patch log defined in [`crate::surface`] is already *deterministic*: replay
//! the same envelopes in the same order and you get the same ledger. The gap this
//! module closes is **concurrent-edit merge**. Two writers — the human operator
//! dragging a card and an agent emitting a `surface_patch` against the same id —
//! produce two patches with no inherent order. Applied naively (arrival order,
//! last-writer-wins), one silently stomps the other, and the two surfaces can
//! diverge depending on which patch the daemon happened to fan out first.
//!
//! This module is the bounded answer to the concurrent-edit decision test, built on the existing
//! patch log rather than a full CRDT engine. It is **not** full multiplayer
//! (presence cursors, a real-time sync transport, and a Loro/Yjs/Automerge
//! document model are explicitly scoped as follow-up — see the `Scope` section
//! below).
//!
//! # The mechanism: per-component last-write-wins by a logical clock
//!
//! Each component carries a [`ComponentVersion`]: a `(rev, actor)` pair where
//! `rev` is a per-component **Lamport-style revision counter** (a monotonic
//! logical clock, not wall-clock) and `actor` is a stable [`ActorId`]. The pair
//! has a **total order** — compare `rev` first, then `actor` as a deterministic
//! tiebreaker. A write supersedes the stored state **iff its version is strictly
//! greater** in that order.
//!
//! This is a Last-Write-Wins Register keyed per component, the textbook
//! CRDT-lite convergence recipe:
//!
//! - **Commutative & order-independent.** `merge(a, b) == merge(b, a)` — both pick
//!   the `max` version. So two concurrent writes to the same component converge to
//!   the *same* winner no matter which order each surface applies them in.
//! - **Idempotent.** Re-applying an already-merged write is a no-op (its version is
//!   not strictly greater than what's stored).
//! - **Per-component.** Writes to *different* components never contend — both land.
//!
//! Because wall-clock `created_at_ms` is unreliable across actors (clock skew,
//! equal millis), the logical clock — not the timestamp — is the source of truth
//! for ordering. `created_at_ms` is retained on the envelope purely for display.
//!
//! # Scope (OCEAN-258 is a foundation, not full multiplayer)
//!
//! In scope here (the P3 deliverable):
//! - version stamps on patches ([`ComponentVersion`], [`ActorId`]),
//! - a deterministic, commutative per-component merge ([`CanvasMergeState`]),
//! - the Lamport clock that generates converging revisions ([`LamportClock`]).
//!
//! Deliberately **out of scope** (follow-up):
//! - a full CRDT library (Loro/Yjs/Automerge) and rich text/tree CRDTs,
//! - presence cursors and live selection sharing,
//! - the real-time sync transport that ships versioned patches between peers
//!   (today they ride the existing daemon SSE fan-out, one writer at a time).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::surface::{ActorRef, ComponentId};

// ---------------------------------------------------------------------------
// ActorId — a stable, totally-ordered actor identity
// ---------------------------------------------------------------------------

/// A stable, orderable identity for whoever originated a write. Derived from an
/// [`ActorRef`] so the operator (`human:operator`) and an agent (`agent:sage`)
/// get distinct, comparable ids. The string ordering is the deterministic
/// tiebreaker when two concurrent writes share the same revision — so the
/// converged result never depends on arrival order.
///
/// `serde(transparent)`: on the wire an `ActorId` is just `"agent:sage"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActorId(pub String);

impl ActorId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derive a stable id from an [`ActorRef`]. The form is `"{kind}:{id}"` when
    /// the actor has an id (e.g. `"agent:sage"`, `"human:operator"`), else just
    /// the kind (e.g. `"system"`). Two refs with the same kind+id always produce
    /// the same `ActorId`, which is what makes the tiebreak deterministic.
    pub fn from_actor(actor: &ActorRef) -> Self {
        match &actor.id {
            Some(id) => Self(format!("{}:{}", actor.kind, id)),
            None => Self(actor.kind.clone()),
        }
    }
}

impl From<&ActorRef> for ActorId {
    fn from(actor: &ActorRef) -> Self {
        Self::from_actor(actor)
    }
}

impl std::fmt::Display for ActorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// ComponentVersion — a per-component logical clock with a total order
// ---------------------------------------------------------------------------

/// The logical version stamp on one component's state: a Lamport-style revision
/// counter plus the [`ActorId`] that produced this revision.
///
/// # Total order
///
/// `ComponentVersion` derives `Ord` with `rev` as the **primary** key and
/// `actor` as the **secondary** (tiebreak) key — because the field declaration
/// order below is `(rev, actor)` and derived `Ord` is lexicographic over fields.
/// So:
///
/// - a higher `rev` always wins (a later logical write supersedes an earlier one);
/// - on an equal `rev` (a genuine concurrent write — two actors that hadn't yet
///   observed each other), the higher `actor` string wins.
///
/// That second rule is the deterministic conflict resolution: every replica
/// computes the *same* winner from the *same* pair, regardless of the order the
/// two writes arrived.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ComponentVersion {
    /// Monotonic per-component logical revision (Lamport clock value). Compared
    /// first. NOTE: field order matters — derived `Ord` is lexicographic, so
    /// `rev` must be declared before `actor` for "higher rev wins, then higher
    /// actor".
    pub rev: u64,
    /// The actor that authored this revision; the tiebreak on an equal `rev`.
    pub actor: ActorId,
}

impl ComponentVersion {
    pub fn new(rev: u64, actor: ActorId) -> Self {
        Self { rev, actor }
    }

    /// Whether `self` should supersede `other` under the convergent merge rule:
    /// strictly greater in the `(rev, actor)` total order.
    ///
    /// This is the whole merge decision for one component. It is **antisymmetric**
    /// (`a.supersedes(b)` and `b.supersedes(a)` can't both be true) and combined
    /// with [`CanvasMergeState`] yields a commutative, order-independent merge.
    pub fn supersedes(&self, other: &ComponentVersion) -> bool {
        self > other
    }
}

// ---------------------------------------------------------------------------
// LamportClock — generates converging revisions for one actor
// ---------------------------------------------------------------------------

/// A per-actor Lamport logical clock. Each writer (the operator's surface, an
/// agent turn) keeps one. It produces the `rev` values stamped onto
/// [`ComponentVersion`]s so that revisions across actors stay causally ordered
/// without a shared wall clock.
///
/// Protocol (standard Lamport):
/// - **local write** → [`tick`](LamportClock::tick): increment, return the new value;
/// - **observe a remote version** → [`observe`](LamportClock::observe): advance to
///   `max(local, remote)` so the *next* local tick is strictly greater than any
///   revision this actor has seen.
///
/// Keeping the clock per-actor (not global) is what lets two surfaces tick
/// independently and still converge: the `(rev, actor)` total order breaks any
/// resulting ties deterministically.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LamportClock {
    counter: u64,
}

impl LamportClock {
    pub fn new() -> Self {
        Self { counter: 0 }
    }

    /// Start a clock already advanced to `value` (e.g. when resuming a persisted
    /// canvas whose log already reached some revision).
    pub fn at(value: u64) -> Self {
        Self { counter: value }
    }

    /// The current logical time without advancing it.
    pub fn now(&self) -> u64 {
        self.counter
    }

    /// Advance for a local write and return the new logical time. Stamp this onto
    /// the [`ComponentVersion`] you are about to write.
    pub fn tick(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    /// Fold in a revision observed from another actor, advancing this clock to at
    /// least that value so the next [`tick`](LamportClock::tick) is strictly
    /// greater than anything seen so far. Returns the new logical time.
    pub fn observe(&mut self, remote_rev: u64) -> u64 {
        self.counter = self.counter.max(remote_rev);
        self.counter
    }
}

// ---------------------------------------------------------------------------
// CanvasMergeState — the per-canvas version vector + merge decision
// ---------------------------------------------------------------------------

/// What happened when a versioned write was offered to [`CanvasMergeState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeDecision {
    /// The write won and the stored version advanced. The caller should apply the
    /// underlying patch to its ledger.
    Applied,
    /// The write lost to the stored version (it was concurrent-but-lower, stale,
    /// or an exact replay). The caller should **skip** applying the patch — doing
    /// so is how both replicas converge on the same winner regardless of order.
    Superseded,
}

impl MergeDecision {
    /// Convenience: did this write win (and so should be applied to the ledger)?
    pub fn applied(self) -> bool {
        matches!(self, MergeDecision::Applied)
    }
}

/// The per-canvas **version vector**: a map from [`ComponentId`] to the current
/// winning [`ComponentVersion`] for that component. This is the small amount of
/// merge metadata a ledger keeps alongside its components so that an out-of-order
/// or concurrent write can be resolved deterministically.
///
/// A ledger drives it like this, for each incoming versioned patch:
///
/// ```text
/// match merge_state.merge(component_id, incoming_version) {
///     MergeDecision::Applied    => ledger.apply_patch(...),  // this write won
///     MergeDecision::Superseded => { /* skip: a higher version already won */ }
/// }
/// ```
///
/// Because [`merge`](CanvasMergeState::merge) accepts a write **iff** its version
/// strictly supersedes the stored one (`max` over the total order), the end state
/// is identical no matter what order the writes are fed in. Different components
/// live under different keys, so concurrent writes to different components both
/// apply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasMergeState {
    /// Per-component winning versions. A `BTreeMap` (sorted by `ComponentId`)
    /// gives a deterministic, dependency-free serialization — the merge result is
    /// independent of insertion order anyway, so canonical key ordering is the
    /// right invariant to expose on the wire.
    versions: BTreeMap<ComponentId, ComponentVersion>,
}

impl CanvasMergeState {
    pub fn new() -> Self {
        Self {
            versions: BTreeMap::new(),
        }
    }

    /// The current winning version for a component, if any write has landed.
    pub fn version(&self, id: &ComponentId) -> Option<&ComponentVersion> {
        self.versions.get(id)
    }

    /// Number of components tracked.
    pub fn len(&self) -> usize {
        self.versions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.versions.is_empty()
    }

    /// Offer a versioned write for `id` and get the deterministic decision.
    ///
    /// - First write for a component → always [`MergeDecision::Applied`].
    /// - Otherwise → `Applied` iff `incoming.supersedes(stored)` (strictly greater
    ///   in the `(rev, actor)` total order), else [`MergeDecision::Superseded`].
    ///
    /// On `Applied`, the stored version advances to `incoming`. This is the single
    /// commutative operation the whole convergence guarantee rests on: feeding the
    /// same set of writes in any order leaves `versions` in the same state.
    pub fn merge(&mut self, id: &ComponentId, incoming: ComponentVersion) -> MergeDecision {
        match self.versions.get(id) {
            Some(stored) if !incoming.supersedes(stored) => MergeDecision::Superseded,
            _ => {
                self.versions.insert(id.clone(), incoming);
                MergeDecision::Applied
            }
        }
    }

    /// The highest revision tracked across all components — useful when resuming a
    /// canvas to seed a [`LamportClock`] past every revision already on disk, so
    /// fresh local writes are strictly greater than the whole replayed history.
    pub fn max_rev(&self) -> u64 {
        self.versions.values().map(|v| v.rev).max().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(name: &str) -> ActorId {
        ActorId::from_actor(&ActorRef::agent(Some(name.to_string())))
    }

    fn human() -> ActorId {
        ActorId::from_actor(&ActorRef::human(Some("operator".to_string())))
    }

    // -----------------------------------------------------------------------
    // ActorId derivation
    // -----------------------------------------------------------------------

    #[test]
    fn actor_id_is_stable_and_distinct_per_actor() {
        assert_eq!(agent("sage").as_str(), "agent:sage");
        assert_eq!(human().as_str(), "human:operator");
        // System (no id) collapses to just the kind.
        assert_eq!(
            ActorId::from_actor(&ActorRef {
                kind: "system".into(),
                id: None,
                label: None
            })
            .as_str(),
            "system"
        );
        // Same kind+id → same id (deterministic tiebreak).
        assert_eq!(agent("sage"), agent("sage"));
        assert_ne!(agent("sage"), agent("flux"));
        assert_ne!(agent("sage"), human());
    }

    #[test]
    fn actor_id_is_transparent_on_the_wire() {
        let v = serde_json::to_value(agent("sage")).unwrap();
        assert_eq!(v, serde_json::json!("agent:sage"));
    }

    // -----------------------------------------------------------------------
    // ComponentVersion total order
    // -----------------------------------------------------------------------

    #[test]
    fn higher_revision_always_wins() {
        let lo = ComponentVersion::new(1, agent("zzz")); // even with a "bigger" actor
        let hi = ComponentVersion::new(2, agent("aaa"));
        assert!(hi.supersedes(&lo));
        assert!(!lo.supersedes(&hi));
    }

    #[test]
    fn equal_revision_breaks_tie_on_actor_deterministically() {
        let a = ComponentVersion::new(5, agent("flux")); // "agent:flux"
        let b = ComponentVersion::new(5, agent("sage")); // "agent:sage" > "agent:flux"
        assert!(b.supersedes(&a), "higher actor id wins the tie");
        assert!(!a.supersedes(&b));
        // Antisymmetry: not both can supersede.
        assert!(!(a.supersedes(&b) && b.supersedes(&a)));
    }

    #[test]
    fn identical_version_does_not_supersede_itself() {
        let v = ComponentVersion::new(3, agent("sage"));
        assert!(
            !v.supersedes(&v.clone()),
            "a replay must not win (idempotent)"
        );
    }

    // -----------------------------------------------------------------------
    // LamportClock
    // -----------------------------------------------------------------------

    #[test]
    fn lamport_tick_is_monotonic() {
        let mut c = LamportClock::new();
        assert_eq!(c.tick(), 1);
        assert_eq!(c.tick(), 2);
        assert_eq!(c.now(), 2);
    }

    #[test]
    fn lamport_observe_advances_past_remote_then_ticks_strictly_greater() {
        let mut c = LamportClock::new();
        c.tick(); // 1
        c.observe(9); // jump to 9
        assert_eq!(c.now(), 9);
        assert_eq!(
            c.tick(),
            10,
            "next local write is strictly greater than any seen"
        );
        // Observing something smaller never rewinds.
        c.observe(3);
        assert_eq!(c.now(), 10);
    }

    // -----------------------------------------------------------------------
    // CanvasMergeState — the core convergence guarantees (OCEAN-258 acceptance)
    // -----------------------------------------------------------------------

    /// Two concurrent writes to the SAME component must converge to the SAME
    /// winner regardless of the order each replica applies them. This is the P3
    /// acceptance test for converging concurrent writes without stomping.
    #[test]
    fn same_component_converges_regardless_of_order() {
        let id = ComponentId::new("brief-1");
        // Operator and agent each write at the same logical rev (genuinely
        // concurrent — neither observed the other). The tiebreak decides.
        let operator_write = ComponentVersion::new(1, human()); // "human:operator"
        let agent_write = ComponentVersion::new(1, agent("sage")); // "agent:sage"

        // Replica A sees operator first, then agent.
        let mut a = CanvasMergeState::new();
        let d1 = a.merge(&id, operator_write.clone());
        let d2 = a.merge(&id, agent_write.clone());

        // Replica B sees agent first, then operator (opposite order).
        let mut b = CanvasMergeState::new();
        b.merge(&id, agent_write.clone());
        b.merge(&id, operator_write.clone());

        // Both converge to the SAME winning version...
        assert_eq!(
            a.version(&id),
            b.version(&id),
            "the two replicas must converge to an identical winner"
        );
        // ...and the winner is the deterministic one (higher actor id on the tie).
        // "human:operator" > "agent:sage" lexicographically, so the operator wins.
        assert_eq!(a.version(&id).unwrap(), &operator_write);

        // First-applied vs second-applied decisions are consistent with the rule:
        // replica A applied operator (first → Applied) then rejected the lower agent.
        assert_eq!(d1, MergeDecision::Applied);
        assert_eq!(d2, MergeDecision::Superseded);
    }

    /// A higher-revision write wins no matter which order it arrives in, and a
    /// late-arriving *lower* revision is rejected (so a slow stale patch can't
    /// stomp a newer one).
    #[test]
    fn higher_revision_wins_in_either_arrival_order() {
        let id = ComponentId::new("c1");
        let early = ComponentVersion::new(1, agent("sage"));
        let late = ComponentVersion::new(2, human());

        // Order 1: early then late.
        let mut s1 = CanvasMergeState::new();
        assert!(s1.merge(&id, early.clone()).applied());
        assert!(s1.merge(&id, late.clone()).applied());
        assert_eq!(s1.version(&id).unwrap(), &late);

        // Order 2: late then early (early arrives out of order, must lose).
        let mut s2 = CanvasMergeState::new();
        assert!(s2.merge(&id, late.clone()).applied());
        assert_eq!(s2.merge(&id, early.clone()), MergeDecision::Superseded);
        assert_eq!(s2.version(&id).unwrap(), &late);

        // Same converged winner either way.
        assert_eq!(s1.version(&id), s2.version(&id));
    }

    /// Concurrent writes to DIFFERENT components must BOTH land — they never
    /// contend. (The other half of the §17 decision test.)
    #[test]
    fn different_components_both_land() {
        let a = ComponentId::new("a");
        let b = ComponentId::new("b");
        let mut s = CanvasMergeState::new();

        assert!(s.merge(&a, ComponentVersion::new(1, human())).applied());
        assert!(s
            .merge(&b, ComponentVersion::new(1, agent("sage")))
            .applied());

        assert_eq!(s.len(), 2, "both components are tracked");
        assert_eq!(s.version(&a).unwrap().actor, human());
        assert_eq!(s.version(&b).unwrap().actor, agent("sage"));
    }

    /// Merge is idempotent: replaying the exact same write (e.g. an SSE redelivery)
    /// is a no-op and never advances state.
    #[test]
    fn replaying_the_same_write_is_idempotent() {
        let id = ComponentId::new("c1");
        let w = ComponentVersion::new(3, agent("sage"));
        let mut s = CanvasMergeState::new();

        assert!(s.merge(&id, w.clone()).applied());
        assert_eq!(
            s.merge(&id, w.clone()),
            MergeDecision::Superseded,
            "an exact replay must not re-apply"
        );
        assert_eq!(s.version(&id).unwrap(), &w);
    }

    /// Property-style check: feeding a fixed multiset of writes in several
    /// different orders always lands on the same final version vector. This is the
    /// commutativity/convergence guarantee stated operationally.
    #[test]
    fn convergence_is_order_independent_across_many_writes() {
        let id = ComponentId::new("shared");
        let writes = [
            ComponentVersion::new(1, agent("sage")),
            ComponentVersion::new(2, human()),
            ComponentVersion::new(2, agent("flux")),
            ComponentVersion::new(3, agent("sage")),
            ComponentVersion::new(1, human()),
        ];
        // The true max under the total order is (3, agent:sage).
        let expected = ComponentVersion::new(3, agent("sage"));

        // Try a handful of permutations; each must converge to `expected`.
        let permutations = [
            vec![0usize, 1, 2, 3, 4],
            vec![4, 3, 2, 1, 0],
            vec![2, 0, 4, 1, 3],
            vec![3, 3, 0, 1, 2, 4], // includes a duplicate (replay)
        ];
        for perm in permutations {
            let mut s = CanvasMergeState::new();
            for i in perm {
                s.merge(&id, writes[i].clone());
            }
            assert_eq!(
                s.version(&id).unwrap(),
                &expected,
                "every order must converge to the max version"
            );
        }
    }

    #[test]
    fn max_rev_seeds_a_resuming_clock() {
        let mut s = CanvasMergeState::new();
        s.merge(&ComponentId::new("a"), ComponentVersion::new(4, human()));
        s.merge(
            &ComponentId::new("b"),
            ComponentVersion::new(7, agent("sage")),
        );
        assert_eq!(s.max_rev(), 7);

        // A resuming actor seeds its clock past the whole replayed history.
        let mut clock = LamportClock::at(s.max_rev());
        assert_eq!(
            clock.tick(),
            8,
            "fresh writes are strictly greater than history"
        );
    }

    #[test]
    fn merge_state_roundtrips_through_json() {
        let mut s = CanvasMergeState::new();
        s.merge(&ComponentId::new("a"), ComponentVersion::new(1, human()));
        let json = serde_json::to_string(&s).unwrap();
        let back: CanvasMergeState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    // -----------------------------------------------------------------------
    // End-to-end: drive the merge from real SurfacePatchEnvelopes, the way a
    // ledger will. This proves the version stamp + target_component() routing +
    // CanvasMergeState compose into the "two concurrent patches converge"
    // behaviour the OCEAN-258 ticket specifies — not just the bare version math.
    // -----------------------------------------------------------------------

    use crate::surface::{CanvasId, PatchId, SurfaceId, SurfacePatch, SurfacePatchEnvelope};
    use crate::AgentSessionId;

    /// A versioned upsert that *moves* a component to `x` (stands in for either
    /// the operator dragging or the agent repositioning the same card).
    fn versioned_move(id: &str, x: f32, rev: u64, actor: ActorId) -> SurfacePatchEnvelope {
        SurfacePatchEnvelope {
            patch_id: PatchId::new(format!("{id}@{rev}:{actor}")),
            session_id: AgentSessionId::new_v4(),
            surface_id: SurfaceId::new("surface:local"),
            canvas_id: CanvasId::new("canvas:main"),
            actor: ActorRef {
                kind: actor
                    .as_str()
                    .split(':')
                    .next()
                    .unwrap_or("system")
                    .to_string(),
                id: actor.as_str().split_once(':').map(|(_, i)| i.to_string()),
                label: None,
            },
            created_at_ms: 0,
            patch: SurfacePatch::MoveComponent {
                component_id: ComponentId::new(id),
                x,
                y: 0.0,
            },
            version: Some(ComponentVersion::new(rev, actor)),
        }
    }

    /// Simulate the ledger merge loop: for each envelope, route its `version`
    /// through `CanvasMergeState` keyed on `patch.target_component()`. Winners
    /// land in `applied` (here: the component's resulting `x`); losers are
    /// skipped. Returns the converged (component -> x) map. Unversioned or
    /// non-component-targeting patches always apply (legacy / view-state path).
    fn simulate_ledger(
        envelopes: &[SurfacePatchEnvelope],
    ) -> std::collections::BTreeMap<String, f32> {
        let mut merge = CanvasMergeState::new();
        let mut applied: std::collections::BTreeMap<String, f32> = Default::default();
        for env in envelopes {
            let winner = match (env.patch.target_component(), &env.version) {
                (Some(id), Some(v)) => merge.merge(id, v.clone()).applied(),
                // No version or no single target → not gated by the merge.
                _ => true,
            };
            if winner {
                if let SurfacePatch::MoveComponent {
                    component_id, x, ..
                } = &env.patch
                {
                    applied.insert(component_id.as_str().to_string(), *x);
                }
            }
        }
        applied
    }

    #[test]
    fn two_concurrent_patches_to_same_component_converge_via_envelopes() {
        // Operator drags brief-1 to x=100; agent moves brief-1 to x=900. Both at
        // the same logical rev → genuinely concurrent, tiebreak decides.
        let op = versioned_move("brief-1", 100.0, 1, human());
        let ag = versioned_move("brief-1", 900.0, 1, agent("sage"));

        // Two replicas, opposite arrival orders.
        let replica_a = simulate_ledger(&[op.clone(), ag.clone()]);
        let replica_b = simulate_ledger(&[ag, op]);

        assert_eq!(
            replica_a, replica_b,
            "both replicas must converge on the same x for brief-1 regardless of order"
        );
        // Deterministic winner: "human:operator" > "agent:sage", so operator's x=100.
        assert_eq!(replica_a.get("brief-1"), Some(&100.0));
    }

    #[test]
    fn concurrent_patches_to_different_components_both_land_via_envelopes() {
        // Operator moves card-a; agent moves card-b. Different components.
        let op = versioned_move("card-a", 50.0, 1, human());
        let ag = versioned_move("card-b", 60.0, 1, agent("sage"));

        let result = simulate_ledger(&[op, ag]);
        assert_eq!(
            result.get("card-a"),
            Some(&50.0),
            "operator's card-a landed"
        );
        assert_eq!(result.get("card-b"), Some(&60.0), "agent's card-b landed");
        assert_eq!(result.len(), 2, "both edits survived — no clobber");
    }

    #[test]
    fn out_of_order_stale_envelope_does_not_clobber_newer_one() {
        // A newer write (rev 2) arrives, then a *stale* older write (rev 1) for the
        // same component shows up late. The stale one must lose.
        let newer = versioned_move("c1", 200.0, 2, agent("sage"));
        let stale = versioned_move("c1", 10.0, 1, human());

        let result = simulate_ledger(&[newer, stale]);
        assert_eq!(
            result.get("c1"),
            Some(&200.0),
            "a late stale (lower-rev) patch must not overwrite the newer one"
        );
    }
}
