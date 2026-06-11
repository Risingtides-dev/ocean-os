//! Regression lock against the validated Python prototype
//! (`claude-monorepo/.superpowers/brainstorm/sim/extract_claims.py`).
//!
//! The two fixture docs are VENDORED byte-for-byte copies (md5-verified at
//! vendoring time) of the real corpus, so CI never depends on external paths:
//!   fixtures/ocean-os-HANDOFF.md            ← ocean-os/HANDOFF.md
//!   fixtures/claude-monorepo-PHASE2_HANDOFF.md ← claude-monorepo/docs/PHASE2_HANDOFF.md
//!
//! Prototype ground truth (run 2026-06-10): 22 claims (15 declared-verified)
//! from the ocean-os doc + 29 claims (0 declared-verified) from the
//! claude-monorepo doc = the 51-claim corpus the schema was validated on.

use ocean_context::claim::ClaimStatus;
use ocean_context::extract::{extract_claims, ExtractCtx};

fn ctx() -> ExtractCtx<'static> {
    ExtractCtx { commit_sha: "d9a9bc9", now: 1_780_980_000, by_session: "regression-test" }
}

#[test]
fn ocean_os_handoff_yields_22_claims_15_verified() {
    let text = include_str!("fixtures/ocean-os-HANDOFF.md");
    let claims = extract_claims(text, &ctx());
    assert_eq!(claims.len(), 22);
    let verified = claims.iter().filter(|c| c.status == ClaimStatus::Verified).count();
    assert_eq!(verified, 15); // prototype: declared_verified == 15
}

#[test]
fn phase2_handoff_yields_29_claims_0_verified() {
    let text = include_str!("fixtures/claude-monorepo-PHASE2_HANDOFF.md");
    let claims = extract_claims(text, &ctx());
    assert_eq!(claims.len(), 29);
    assert!(claims.iter().all(|c| c.status == ClaimStatus::Asserted));
}

#[test]
fn corpus_totals_51_claims() {
    let a = extract_claims(include_str!("fixtures/ocean-os-HANDOFF.md"), &ctx());
    let b = extract_claims(include_str!("fixtures/claude-monorepo-PHASE2_HANDOFF.md"), &ctx());
    assert_eq!(a.len() + b.len(), 51);
}

#[test]
fn prototype_quirks_are_preserved() {
    // The prototype's leftmost-first alternation matches "manifest.json" as
    // "manifest.js" (js before json in the extension alternation) — frozen.
    let a = extract_claims(include_str!("fixtures/ocean-os-HANDOFF.md"), &ctx());
    assert!(a
        .iter()
        .any(|c| c.provenance.anchors.iter().any(|an| an.file.as_deref() == Some("manifest.js"))));
    // "~/.config/brain/config.toml" matches from the slash ("~" is not in the
    // file char class) — frozen too.
    let b = extract_claims(include_str!("fixtures/claude-monorepo-PHASE2_HANDOFF.md"), &ctx());
    assert!(b.iter().any(|c| c
        .provenance
        .anchors
        .iter()
        .any(|an| an.file.as_deref() == Some("/.config/brain/config.toml"))));
}

#[test]
fn confidence_is_derived_not_free_typed() {
    // F3: extractor confidence comes from Claim::derive_confidence.
    use ocean_context::claim::Claim;
    let claims = extract_claims(include_str!("fixtures/ocean-os-HANDOFF.md"), &ctx());
    for c in &claims {
        let expected =
            Claim::derive_confidence(&c.provenance.anchors, c.status == ClaimStatus::Verified);
        assert!((c.confidence - expected).abs() < f32::EPSILON, "claim {}", c.id);
    }
}

#[test]
fn input_rs_anchor_parses_line_list_and_verified_section() {
    let text = include_str!("fixtures/ocean-os-HANDOFF.md");
    let claims = extract_claims(text, &ctx());
    // The "Verified ground truth" section anchors `input.rs:29,67,97,130`.
    // (An earlier claim anchors a bare `input.rs` with no lines, so match on both.)
    let c = claims
        .iter()
        .find(|c| {
            c.provenance
                .anchors
                .iter()
                .any(|a| a.file.as_deref() == Some("input.rs") && !a.lines.is_empty())
        })
        .expect("input.rs claim with line list present");
    let a =
        c.provenance.anchors.iter().find(|a| a.file.as_deref() == Some("input.rs")).unwrap();
    assert_eq!(a.lines, vec![29, 67, 97, 130]);
    assert_eq!(c.status, ClaimStatus::Verified);
}

#[test]
fn unanchored_lines_are_skipped() {
    let claims = extract_claims("This long sentence mentions no file anchors at all.", &ctx());
    assert!(claims.is_empty());
}

#[test]
fn range_lines_normalize_en_dash() {
    let claims = extract_claims("Single browser + single active page: lib.rs:37–82 holds it.", &ctx());
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].provenance.anchors[0].lines, vec![37, 82]);
}

#[test]
fn ticket_and_symbol_are_captured() {
    let claims = extract_claims(
        "Phase 1 done (OCEAN-16): `append_client_type` arm in crates/ocean-agent/src/lib.rs.",
        &ctx(),
    );
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].provenance.tickets, vec!["OCEAN-16".to_string()]);
    assert_eq!(claims[0].provenance.anchors[0].symbol.as_deref(), Some("append_client_type"));
}

#[test]
fn multiple_tickets_on_one_line_are_all_kept() {
    // Schema friction #3: lines carry multiple tickets; keep them all,
    // first-seen order, deduped.
    let claims = extract_claims(
        "Done across OCEAN-16 and OCEAN-23 (follow-up of OCEAN-16): crates/x/lib.rs updated.",
        &ctx(),
    );
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].provenance.tickets, vec!["OCEAN-16".to_string(), "OCEAN-23".to_string()]);
}

#[test]
fn symbols_pair_by_proximity_not_blind_zip() {
    // Schema friction #4: both symbols sit next to input.rs; index-zipping
    // would hand `handle_input` to nav.rs. Proximity pairing must not.
    let claims = extract_claims(
        "The nav.rs module now delegates to input.rs via `handle_input` and `route_event` hooks.",
        &ctx(),
    );
    assert_eq!(claims.len(), 1);
    let anchors = &claims[0].provenance.anchors;
    assert_eq!(anchors.len(), 2);
    let nav = anchors.iter().find(|a| a.file.as_deref() == Some("nav.rs")).unwrap();
    let input = anchors.iter().find(|a| a.file.as_deref() == Some("input.rs")).unwrap();
    assert_eq!(input.symbol.as_deref(), Some("handle_input"));
    assert_eq!(nav.symbol, None, "far-away symbol must not be zipped onto nav.rs");
}

#[test]
fn adjacent_symbol_anchor_pairs_attach_correctly() {
    // Two symbol+anchor pairs in one line: each symbol lands on ITS anchor.
    let claims = extract_claims(
        "`requires_permission` moved into input.rs while nav.rs keeps `route_event` dispatch.",
        &ctx(),
    );
    assert_eq!(claims.len(), 1);
    let anchors = &claims[0].provenance.anchors;
    let nav = anchors.iter().find(|a| a.file.as_deref() == Some("nav.rs")).unwrap();
    let input = anchors.iter().find(|a| a.file.as_deref() == Some("input.rs")).unwrap();
    assert_eq!(input.symbol.as_deref(), Some("requires_permission"));
    assert_eq!(nav.symbol.as_deref(), Some("route_event"));
}
