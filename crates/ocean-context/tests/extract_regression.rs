use ocean_context::claim::ClaimStatus;
use ocean_context::extract::{extract_claims, ExtractCtx};

fn ctx() -> ExtractCtx<'static> {
    ExtractCtx { commit_sha: "d9a9bc9", now: 1_780_980_000, by_session: "regression-test" }
}

#[test]
fn ocean_os_handoff_yields_22_claims() {
    let text = include_str!("fixtures/ocean-os-HANDOFF.md");
    assert_eq!(extract_claims(text, &ctx()).len(), 22);
}

#[test]
fn phase2_handoff_yields_29_claims() {
    let text = include_str!("fixtures/claude-monorepo-PHASE2_HANDOFF.md");
    assert_eq!(extract_claims(text, &ctx()).len(), 29);
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
            c.provenance.anchors.iter().any(|a| a.file == "input.rs" && !a.lines.is_empty())
        })
        .expect("input.rs claim with line list present");
    let a = c.provenance.anchors.iter().find(|a| a.file == "input.rs").unwrap();
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
    assert_eq!(claims[0].provenance.ticket.as_deref(), Some("OCEAN-16"));
    assert_eq!(claims[0].provenance.anchors[0].symbol.as_deref(), Some("append_client_type"));
}
