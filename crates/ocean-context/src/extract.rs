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

/// v1 confidence is DERIVED, never free-typed (handoff finding F3):
/// base by section, small bump per extra anchor.
fn derive_confidence(anchor_count: usize, declared_verified: bool) -> f32 {
    let base = if declared_verified { 0.8 } else { 0.5 };
    (base + 0.05 * anchor_count.min(4) as f32).min(1.0)
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
                    if let Some((a, b)) = part.split_once('-') {
                        if let Ok(n) = a.parse::<u32>() {
                            lines.push(n);
                        }
                        if let Ok(n) = b.parse::<u32>() {
                            lines.push(n);
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
        let confidence = derive_confidence(anchors.len(), in_verified);
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
