//! Deterministic transcript ingestion — the low-level memory core.
//!
//! Feeds a session transcript through a **deterministic, zero-LLM** pass that
//! pulls structured memory candidates (tickets, backticked symbols, file paths,
//! explicit `decision:`/`note:`/`todo:` markers) and writes them as [`Memory`]
//! rows via a [`MemoryStore`]. Freeform semantic residue is handed to a
//! [`ResidueExtractor`] seam — a no-op by default, the place a cheap model plugs
//! in later. The deterministic spine is the point: reproducible, free, and the
//! thing that stamps trustable provenance.
//!
//! This is the programmatic-parse half of the architecture's "low-level
//! processing core" (`ocean-agents/docs/AGENT_FILESYSTEM_ARCHITECTURE.md` §5).
//! The cheap-model residue pass and the `ocean-hooks` trigger are follow-ups
//! behind the seams here; this module has **no daemon or provider dependency**
//! and is fully unit-testable.

use std::sync::LazyLock;

use ocean_context::{Anchor, ClaimEvent, ClaimStatus, Provenance};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{Memory, MemoryId, MemoryKind, MemoryScope, MemoryStore, PrincipalId, Result};

// ===========================================================================
// Transcript shape
// ===========================================================================

/// One turn in a session transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptTurn {
    pub role: TurnRole,
    pub text: String,
    /// Unix seconds. Caller-supplied — never `now()` in the lib.
    pub at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnRole {
    User,
    Assistant,
    Tool,
    System,
}

/// A session transcript to ingest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub session_id: String,
    pub turns: Vec<TranscriptTurn>,
}

// ===========================================================================
// Ingest context + report
// ===========================================================================

/// Who/what/when for one ingestion run.
#[derive(Debug, Clone, Copy)]
pub struct IngestContext<'a> {
    pub owner: &'a PrincipalId,
    pub scope: MemoryScope,
    /// Commit the memories are dated against (`provenance.commit_sha`).
    pub commit_sha: &'a str,
    /// Unix seconds for the `written` history event.
    pub now: i64,
    /// Session id stamping provenance/history.
    pub by_session: &'a str,
}

/// Outcome of an ingestion run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestReport {
    /// Candidates the deterministic pass structured.
    pub deterministic: usize,
    /// Extra memories the residue (model) seam contributed.
    pub residue: usize,
    /// Total rows written to the store.
    pub written: usize,
}

// ===========================================================================
// Residue seam (cheap model plugs in here)
// ===========================================================================

/// What the deterministic pass could not structure, handed to the model seam.
#[derive(Debug, Clone, Default)]
pub struct Residue {
    /// Turn texts that yielded no structured candidate.
    pub unstructured_turns: Vec<String>,
}

/// The seam for the cheap-model residue pass. The deterministic extractor hands
/// it the text it could NOT structure; it may return extra [`Memory`] rows.
///
/// Default impl ([`NoResidue`]) returns nothing — a pure deterministic ingest,
/// which is exactly the reproducible, zero-cost baseline.
pub trait ResidueExtractor {
    fn extract(&self, residue: &Residue, ctx: &IngestContext<'_>) -> Vec<Memory>;
}

/// Default no-op residue extractor.
pub struct NoResidue;

impl ResidueExtractor for NoResidue {
    fn extract(&self, _residue: &Residue, _ctx: &IngestContext<'_>) -> Vec<Memory> {
        Vec::new()
    }
}

// ===========================================================================
// Deterministic extraction
// ===========================================================================

/// Compiled once, reused for every turn. Initializers are known at declaration
/// time, so `LazyLock` (not `OnceLock`).
static TICKET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Z]{2,}-\d+)\b").expect("ticket regex"));

static SYMBOL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"`([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z0-9_]+)*)`").expect("symbol regex")
});

static FILEPATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([\w./-]+\.(?:rs|py|ts|mjs|js|md|toml|sql|json))\b").expect("filepath regex")
});

/// One structured candidate pulled from a turn's text.
#[derive(Debug, Clone, PartialEq)]
enum Candidate {
    Ticket(String),
    Symbol(String),
    FilePath(String),
    /// An explicit marker line (`decision:` / `note:` / `todo:` …).
    Marker {
        kind: MemoryKind,
        text: String,
    },
}

/// Pull structured candidates out of one turn's text. Deduped by value so a
/// symbol mentioned three times in one turn yields one memory (each *turn* is
/// the evidence unit; cross-turn repetition is separate evidence).
fn extract_candidates(text: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let push = |c: Candidate, out: &mut Vec<Candidate>| {
        if !out.contains(&c) {
            out.push(c);
        }
    };

    for cap in TICKET_RE.captures_iter(text) {
        push(Candidate::Ticket(cap[1].to_string()), &mut out);
    }
    for cap in SYMBOL_RE.captures_iter(text) {
        push(Candidate::Symbol(cap[1].to_string()), &mut out);
    }
    for cap in FILEPATH_RE.captures_iter(text) {
        let path = &cap[1];
        // A backticked symbol captured above can look like a path filter; skip
        // anything already recorded as a symbol to avoid double-counting.
        if !out
            .iter()
            .any(|c| matches!(c, Candidate::Symbol(s) if s == path))
        {
            push(Candidate::FilePath(path.to_string()), &mut out);
        }
    }

    // Marker lines — per-line, case-insensitive prefix.
    for line in text.lines() {
        let trimmed = line.trim_start_matches(['-', '*', '>', ' ']);
        if let Some((kind, rest)) = parse_marker(trimmed) {
            push(
                Candidate::Marker {
                    kind,
                    text: rest.trim().to_string(),
                },
                &mut out,
            );
        }
    }

    out
}

/// Split a `decision: …` / `note: …` / `todo: …` line into (kind, body).
fn parse_marker(line: &str) -> Option<(MemoryKind, &str)> {
    let lower = line.to_ascii_lowercase();
    let (head, kind) = [
        ("decision:", MemoryKind::Preference),
        ("decided:", MemoryKind::Preference),
        ("note:", MemoryKind::Fact),
        ("todo:", MemoryKind::Event),
    ]
    .into_iter()
    .find(|(h, _)| lower.starts_with(h))?;
    Some((kind, &line[head.len()..]))
}

/// Build a [`Memory`] from one candidate, dated against the originating turn.
fn build_memory(cand: Candidate, turn: &TranscriptTurn, ctx: &IngestContext<'_>) -> Memory {
    let (kind, body, anchors, tickets) = match cand {
        Candidate::Ticket(t) => (
            MemoryKind::Relationship,
            serde_json::json!({ "ticket": t, "role": turn.role }),
            Vec::new(),
            vec![t],
        ),
        Candidate::Symbol(s) => (
            MemoryKind::Fact,
            serde_json::json!({ "symbol": s }),
            vec![Anchor {
                file: None,
                symbol: Some(s),
                lines: Vec::new(),
                sig_hash: None,
            }],
            Vec::new(),
        ),
        Candidate::FilePath(f) => (
            MemoryKind::Fact,
            serde_json::json!({ "file": f }),
            vec![Anchor {
                file: Some(f),
                symbol: None,
                lines: Vec::new(),
                sig_hash: None,
            }],
            Vec::new(),
        ),
        Candidate::Marker { kind, text } => (
            kind,
            serde_json::json!({ "marker": text }),
            Vec::new(),
            Vec::new(),
        ),
    };

    Memory {
        id: MemoryId::new(),
        scope: ctx.scope,
        owner: ctx.owner.clone(),
        kind,
        body,
        provenance: Provenance {
            anchors,
            tickets,
            commit_sha: ctx.commit_sha.to_string(),
        },
        trust: ClaimStatus::Asserted,
        seq: 0, // assigned by the store on insert
        written_at: turn.at,
        updated_at: turn.at,
        history: vec![ClaimEvent {
            at: ctx.now,
            event: "written".into(),
            by_session: ctx.by_session.to_string(),
        }],
    }
}

// ===========================================================================
// The ingest entry point
// ===========================================================================

/// Ingest a transcript: deterministic pass → store writes → residue seam.
///
/// Deterministic candidates are written immediately (one [`Memory`] each, proven
/// = the structured signal). Turns that yield no candidate accumulate as
/// [`Residue`] and are handed to `residue` — a no-op by default, the cheap-model
/// seam. Returns counts; never aborts the run on a single bad write (a `put`
/// failure short-circuits, since the store is the source of truth).
pub fn ingest(
    transcript: &Transcript,
    ctx: &IngestContext<'_>,
    store: &mut impl MemoryStore,
    residue: &dyn ResidueExtractor,
) -> Result<IngestReport> {
    let mut report = IngestReport::default();
    let mut residue_buf = Residue::default();

    for turn in &transcript.turns {
        let candidates = extract_candidates(&turn.text);
        if candidates.is_empty() {
            residue_buf.unstructured_turns.push(turn.text.clone());
            continue;
        }
        for cand in candidates {
            report.deterministic += 1;
            let mem = build_memory(cand, turn, ctx);
            store.put(mem)?;
            report.written += 1;
        }
    }

    let extra = residue.extract(&residue_buf, ctx);
    for mem in extra {
        report.residue += 1;
        store.put(mem)?;
        report.written += 1;
    }

    Ok(report)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SqliteMemoryStore;

    fn ctx<'a>(owner: &'a PrincipalId) -> IngestContext<'a> {
        IngestContext {
            owner,
            scope: MemoryScope::Operator,
            commit_sha: "abc1234",
            now: 1_780_000_000,
            by_session: "sess-1",
        }
    }

    fn transcript(text: &str) -> Transcript {
        Transcript {
            session_id: "sess-1".into(),
            turns: vec![TranscriptTurn {
                role: TurnRole::Assistant,
                text: text.into(),
                at: 1_780_000_000,
            }],
        }
    }

    #[test]
    fn extracts_tickets_symbols_paths_markers() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let owner = PrincipalId::new("coworker-a");
        let t = transcript(
            "Fixed OCEAN-42 by editing `router::dispatch` in couriers/hub/router.py.\n\
             decision: couriers dispatch moves into the daemon.\n\
             Also touched slack.py.",
        );
        let report = ingest(&t, &ctx(&owner), &mut store, &NoResidue).unwrap();

        // OCEAN-42, router::dispatch, couriers/hub/router.py, slack.py, 1 marker = 5.
        assert_eq!(report.deterministic, 5);
        assert_eq!(report.written, 5);
        assert_eq!(report.residue, 0);
        assert_eq!(store.count(&owner).unwrap(), 5);
    }

    #[test]
    fn unstructured_turns_become_residue() {
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let owner = PrincipalId::new("a");
        let t = Transcript {
            session_id: "s".into(),
            turns: vec![TranscriptTurn {
                role: TurnRole::User,
                text: "hey how's it going today".into(),
                at: 1,
            }],
        };
        let report = ingest(&t, &ctx(&owner), &mut store, &NoResidue).unwrap();
        assert_eq!(report.deterministic, 0);
        assert_eq!(report.written, 0);
        // NoResidue consumes nothing — a real model seam would see this turn.
    }

    #[test]
    fn residue_seam_can_contribute_memories() {
        struct FakeModel;
        impl ResidueExtractor for FakeModel {
            fn extract(&self, residue: &Residue, ctx: &IngestContext<'_>) -> Vec<Memory> {
                residue
                    .unstructured_turns
                    .iter()
                    .map(|t| Memory {
                        id: MemoryId::new(),
                        scope: ctx.scope,
                        owner: ctx.owner.clone(),
                        kind: MemoryKind::Fact,
                        body: serde_json::json!({ "inferred": t }),
                        provenance: ProvenContext::from(ctx),
                        trust: ClaimStatus::Asserted,
                        seq: 0,
                        written_at: ctx.now,
                        updated_at: ctx.now,
                        history: Vec::new(),
                    })
                    .collect()
            }
        }

        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let owner = PrincipalId::new("a");
        let t = transcript("just chatting, no structure here");
        let report = ingest(&t, &ctx(&owner), &mut store, &FakeModel).unwrap();
        assert_eq!(report.deterministic, 0);
        assert_eq!(report.residue, 1);
        assert_eq!(store.count(&owner).unwrap(), 1);
    }

    #[test]
    fn dedups_within_a_turn() {
        let text = "see `foo::bar` and `foo::bar` again, OCEAN-1 and OCEAN-1";
        let cands = extract_candidates(text);
        let tickets = cands
            .iter()
            .filter(|c| matches!(c, Candidate::Ticket(_)))
            .count();
        let symbols = cands
            .iter()
            .filter(|c| matches!(c, Candidate::Symbol(_)))
            .count();
        assert_eq!(tickets, 1);
        assert_eq!(symbols, 1);
    }

    #[test]
    fn marker_kinds_map_correctly() {
        assert_eq!(
            parse_marker("decision: use sqlite").map(|(k, _)| k),
            Some(MemoryKind::Preference)
        );
        assert_eq!(
            parse_marker("note: it works").map(|(k, _)| k),
            Some(MemoryKind::Fact)
        );
        assert_eq!(
            parse_marker("TODO: ship it").map(|(k, _)| k),
            Some(MemoryKind::Event)
        );
        assert_eq!(parse_marker("not a marker"), None);
    }

    /// Helper used by the fake-model test: build a [`Provenance`] from a ctx.
    struct ProvenContext;
    impl ProvenContext {
        fn from(ctx: &IngestContext<'_>) -> Provenance {
            Provenance {
                anchors: Vec::new(),
                tickets: Vec::new(),
                commit_sha: ctx.commit_sha.to_string(),
            }
        }
    }
}
