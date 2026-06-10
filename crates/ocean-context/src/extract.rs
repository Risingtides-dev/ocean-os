//! Pass-1 claim extraction: deterministically pull anchored claims out of
//! prose HANDOFF.md docs. Zero LLM. Faithful port of the validated Python
//! prototype (51-claim corpus); the regression tests freeze its behavior.

use crate::claim::{Anchor, Claim, ClaimEvent, ClaimStatus, KnowledgeTier, Provenance};
use regex::Regex;
use std::sync::OnceLock;

pub struct ExtractCtx<'a> {
    /// Commit the claims are dated against.
    pub commit_sha: &'a str,
    /// Unix seconds for the `written` history event.
    pub now: i64,
    pub by_session: &'a str,
}

fn anchor_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"([A-Za-z0-9_./-]+\.(?:rs|ts|tsx|js|jsx|py|go|toml|md|sql|json))(?::(\d+(?:[,\-–]\d+)*))?",
        )
        .expect("anchor regex")
    })
}

fn ticket_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\b([A-Z]{2,}-\d+)\b").expect("ticket regex"))
}

fn symbol_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"`([a-zA-Z_][a-zA-Z0-9_]*(?:::[a-zA-Z0-9_]+)*)\(?\)?`").expect("symbol regex")
    })
}

fn verified_hdr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)(verified|ground truth|already done|current state|don.?t re-?verify)")
            .expect("verified header regex")
    })
}

pub fn extract_claims(text: &str, ctx: &ExtractCtx) -> Vec<Claim> {
    let mut claims = Vec::new();
    let mut in_verified = false;
    for raw in text.lines() {
        if raw.starts_with('#') {
            in_verified = verified_hdr_re().is_match(raw);
        }
        let l = raw
            .trim_matches(|c: char| matches!(c, ' ' | '\t' | '-' | '*' | '•'))
            .trim();
        if l.chars().count() < 12 {
            continue;
        }
        let mut anchors = Vec::new();
        for cap in anchor_re().captures_iter(l) {
            let mut lines = Vec::new();
            if let Some(ls) = cap.get(2) {
                for part in ls.as_str().split(',') {
                    let part = part.replace('–', "-");
                    if part.contains('-') {
                        // Faithful to the prototype: a range contributes its
                        // first two dash-separated endpoints ("10-20-30" → 10, 20).
                        for end in part.split('-').take(2) {
                            if let Ok(n) = end.parse::<u32>() {
                                lines.push(n);
                            }
                        }
                    } else if let Ok(n) = part.parse::<u32>() {
                        lines.push(n);
                    }
                }
            }
            anchors.push(Anchor { file: cap[1].to_string(), symbol: None, lines, sig_hash: None });
        }
        if anchors.is_empty() {
            continue; // pass-1: only structurally-anchored claims
        }
        // v1 heuristic: pair the i-th backticked symbol with the i-th anchor.
        let symbols: Vec<String> =
            symbol_re().captures_iter(l).map(|c| c[1].to_string()).take(6).collect();
        for (anchor, sym) in anchors.iter_mut().zip(symbols.iter()) {
            anchor.symbol = Some(sym.clone());
        }
        let ticket = ticket_re().captures(l).map(|c| c[1].to_string());
        // F3: confidence is DERIVED from anchor richness, never free-typed.
        let confidence = Claim::derive_confidence(&anchors, in_verified);
        claims.push(Claim {
            id: format!("c{}", claims.len() + 1),
            text: l.chars().take(280).collect(),
            provenance: Provenance { anchors, ticket, commit_sha: ctx.commit_sha.to_string() },
            status: if in_verified { ClaimStatus::Verified } else { ClaimStatus::Asserted },
            knowledge_tier: KnowledgeTier::Individual,
            ps_anchor: None,
            confidence,
            borrowed_from: None,
            history: vec![ClaimEvent {
                at: ctx.now,
                event: "written".to_string(),
                by_session: ctx.by_session.to_string(),
            }],
        });
    }
    claims
}
