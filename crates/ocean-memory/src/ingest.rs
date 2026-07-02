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

use ocean_context::okf;
use ocean_context::{Anchor, ClaimEvent, ClaimStatus, Provenance};
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::{
    Memory, MemoryError, MemoryId, MemoryKind, MemoryScope, MemoryStore, PrincipalId, Result,
};

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
    /// A structured source artifact seeding the session — a handoff (TOML), a
    /// memory/vault file (YAML), or an `events.md` entry (devlog). When present,
    /// its frontmatter is normalized through the OKF profile registry (rather
    /// than hand-parsed or dropped) and written as one OKF-typed memory row.
    #[serde(default)]
    pub frontmatter: Option<Frontmatter>,
}

/// A structured on-disk artifact attached to a session, carried through the OKF
/// profile registry at ingest time. Rather than re-implementing frontmatter
/// parsing here, ingest hands `text` to [`okf::load`] as the declared [`okf::Source`]
/// and OKF concept `concept_type`; the normalized [`okf::Fields`] and its
/// diagnostics land verbatim on the resulting memory row's body.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frontmatter {
    /// The on-disk shape (`toml` / `yaml` / `devlog`).
    pub source: FrontmatterSource,
    /// The OKF concept type the artifact declares (`"handoff"`, `"memory"`,
    /// `"event"`, …). Passed straight to [`okf::load`] as the routing
    /// discriminator; an unknown type is tolerated as a diagnostic, never
    /// rejected — matching OKF's validate-by-diagnostics contract.
    pub concept_type: String,
    /// The raw artifact text (frontmatter block plus any body/prose).
    pub text: String,
}

/// Serde-friendly mirror of [`okf::Source`]. OKF's enum is `Copy` but not
/// `Serialize`; this keeps `Transcript` (de)serializable while delegating the
/// actual parsing to OKF via [`FrontmatterSource::as_okf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrontmatterSource {
    /// `+++`-delimited TOML frontmatter (handoffs).
    Toml,
    /// `---`-delimited YAML frontmatter (vault notes, memory files).
    Yaml,
    /// A single `events.md` entry (`key: value` header lines then prose).
    Devlog,
}

impl FrontmatterSource {
    /// The OKF [`okf::Source`] this maps to — the single point that couples this
    /// crate to OKF's source discriminator.
    fn as_okf(self) -> okf::Source {
        match self {
            FrontmatterSource::Toml => okf::Source::Toml,
            FrontmatterSource::Yaml => okf::Source::Yaml,
            FrontmatterSource::Devlog => okf::Source::Devlog,
        }
    }
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
    /// OKF-normalized frontmatter rows written (0 or 1 — the transcript's
    /// [`Transcript::frontmatter`], normalized through the profile registry).
    pub normalized: usize,
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
// OKF frontmatter normalization
// ===========================================================================

/// Run `fm` through the OKF profile registry ([`okf::load`]) and build one
/// memory row carrying the normalized, typed fields plus its diagnostics.
///
/// This is the crate's single OKF call site: the frontmatter is *not* hand-parsed
/// here — `okf::load` applies the profile's `maps_from` migrations and produces
/// the conformance diagnostics, and both land verbatim on the row's body. Per
/// OKF's validate-by-diagnostics contract, diagnostics are *carried*, never a
/// reason to reject — a malformed block (an actual parse error) is the only
/// `Err`, and the caller may skip it exactly as `store.rs` skips a bad claims
/// block. The row's `kind` follows the concept type where it names a memory
/// nature; everything else is a [`MemoryKind::Fact`] about the artifact.
///
/// A malformed frontmatter block is OKF's one `Err`; it is surfaced as
/// [`MemoryError::BadInput`] so the caller can skip that artifact. Diagnostics
/// are never an error here — they ride on the row's body.
fn build_frontmatter_memory(fm: &Frontmatter, ctx: &IngestContext<'_>) -> Result<Memory> {
    let (normalized, diagnostics) = okf::load(fm.source.as_okf(), &fm.concept_type, &fm.text)
        .map_err(|e| MemoryError::BadInput(format!("malformed OKF frontmatter: {e}")))?;

    // Diagnostics are structured (severity/code/message) so a later normalizer
    // pass can query the carried work-items, not just read prose.
    let diagnostics: Vec<serde_json::Value> = diagnostics
        .iter()
        .map(|d| {
            serde_json::json!({
                "severity": match d.severity {
                    okf::Severity::Warn => "warn",
                    okf::Severity::Error => "error",
                },
                "code": d.code,
                "message": d.message,
            })
        })
        .collect();

    let body = serde_json::json!({
        "okf": {
            "concept_type": fm.concept_type,
            // Normalized (post-`maps_from`) typed fields, carried verbatim.
            "fields": normalized,
            // Conformance work-items — carried, not gating.
            "diagnostics": diagnostics,
        }
    });

    Ok(Memory {
        id: MemoryId::new(),
        scope: ctx.scope,
        owner: ctx.owner.clone(),
        kind: frontmatter_memory_kind(&fm.concept_type),
        body,
        provenance: Provenance {
            anchors: Vec::new(),
            tickets: Vec::new(),
            commit_sha: ctx.commit_sha.to_string(),
        },
        trust: ClaimStatus::Asserted,
        seq: 0, // assigned by the store on insert
        written_at: ctx.now,
        updated_at: ctx.now,
        history: vec![ClaimEvent {
            at: ctx.now,
            event: "written".into(),
            by_session: ctx.by_session.to_string(),
        }],
    })
}

/// The memory kind for a normalized-frontmatter row. A `memory` concept is a
/// [`MemoryKind::Fact`]; an `event`/devlog entry an [`MemoryKind::Event`]; every
/// other artifact (handoff/claim/agent/note/unknown) is recorded as a
/// [`MemoryKind::Fact`] *about* that artifact.
fn frontmatter_memory_kind(concept_type: &str) -> MemoryKind {
    match concept_type {
        "event" => MemoryKind::Event,
        _ => MemoryKind::Fact,
    }
}

// ===========================================================================
// The ingest entry point
// ===========================================================================

/// Ingest a transcript: OKF frontmatter → deterministic pass → store writes →
/// residue seam.
///
/// If the transcript carries [`Transcript::frontmatter`], it is first normalized
/// through the OKF profile registry ([`okf::load`]) and written as one row whose
/// body holds the typed, migrated fields plus their conformance diagnostics —
/// the frontmatter is neither dropped nor hand-parsed here. Deterministic
/// candidates are then written immediately (one [`Memory`] each, proven = the
/// structured signal). Turns that yield no candidate accumulate as [`Residue`]
/// and are handed to `residue` — a no-op by default, the cheap-model seam.
///
/// Returns counts; never aborts the run on a single bad write (a `put` failure
/// short-circuits, since the store is the source of truth). A *malformed*
/// frontmatter block is the one OKF `Err` — surfaced as [`MemoryError::BadInput`]
/// so the caller can skip that artifact, mirroring `store.rs`'s treatment of an
/// unparseable claims block. Diagnostics are never such an error: they are
/// carried on the row, per OKF's validate-by-diagnostics contract.
pub fn ingest(
    transcript: &Transcript,
    ctx: &IngestContext<'_>,
    store: &mut impl MemoryStore,
    residue: &dyn ResidueExtractor,
) -> Result<IngestReport> {
    let mut report = IngestReport::default();
    let mut residue_buf = Residue::default();

    if let Some(fm) = &transcript.frontmatter {
        let mem = build_frontmatter_memory(fm, ctx)?;
        store.put(mem)?;
        report.normalized += 1;
        report.written += 1;
    }

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
            frontmatter: None,
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
            frontmatter: None,
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

    #[test]
    fn devlog_frontmatter_is_okf_normalized_into_a_memory_row() {
        // A real events.md entry ingested through ocean-memory: OKF's maps_from
        // migration must fire (`type:` → canonical `category`) and the normalized
        // fields + diagnostics must land on a written memory row — proving the
        // frontmatter was carried through the profile registry, not hand-parsed
        // or dropped.
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let owner = PrincipalId::new("coworker-a");
        let t = Transcript {
            session_id: "sess-1".into(),
            turns: Vec::new(),
            frontmatter: Some(Frontmatter {
                source: FrontmatterSource::Devlog,
                concept_type: "event".into(),
                text: "time:      [12:34pm] [06-15-26]\nagent:     [claude] [opus 4.8]\n\
                       type:      feature-request\narea:      backend\n\n\
                       What I did and why."
                    .into(),
            }),
        };

        let report = ingest(&t, &ctx(&owner), &mut store, &NoResidue).unwrap();
        assert_eq!(report.normalized, 1, "one OKF-normalized frontmatter row");
        assert_eq!(report.written, 1);
        assert_eq!(report.deterministic, 0);

        // The written row carries the OKF-normalized fields.
        let page = store.list_page(&owner, None, None).unwrap();
        assert_eq!(page.memories.len(), 1);
        let row = &page.memories[0];
        assert_eq!(row.kind, MemoryKind::Event, "an event concept is an Event");

        let okf = &row.body["okf"];
        assert_eq!(okf["concept_type"], "event");
        // maps_from migration fired: the devlog's `type:` was lifted to the
        // canonical `category`, and does NOT collide with the OKF concept type.
        assert_eq!(
            okf["fields"]["category"], "feature-request",
            "OKF normalize() lifted `type` → `category`: {:?}",
            okf["fields"]
        );
        // A conforming event (time + agent present) carries no diagnostics.
        assert_eq!(
            okf["diagnostics"].as_array().map(Vec::len),
            Some(0),
            "conforming event has no diagnostics"
        );
    }

    #[test]
    fn frontmatter_diagnostics_are_carried_not_rejected() {
        // A memory artifact whose `kind` is only reachable via metadata.type: OKF
        // must heal it (needs-migration warning), and the normalized `kind` plus
        // any carried diagnostics must be attached — never a reason to reject.
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let owner = PrincipalId::new("a");
        let t = Transcript {
            session_id: "s".into(),
            turns: Vec::new(),
            frontmatter: Some(Frontmatter {
                source: FrontmatterSource::Yaml,
                concept_type: "memory".into(),
                text: "---\nname: campaign-hub-real-data\nmetadata:\n  node_type: memory\n  \
                       type: project\n---\n\nthe fact body"
                    .into(),
            }),
        };

        let report = ingest(&t, &ctx(&owner), &mut store, &NoResidue).unwrap();
        assert_eq!(report.normalized, 1);

        let page = store.list_page(&owner, None, None).unwrap();
        let okf = &page.memories[0].body["okf"];
        // maps_from healed metadata.type → kind and name → title.
        assert_eq!(okf["fields"]["kind"], "project");
        assert_eq!(okf["fields"]["title"], "campaign-hub-real-data");
        // No hard errors were carried (the shape healed to conformance).
        let diags = okf["diagnostics"].as_array().unwrap();
        assert!(
            diags.iter().all(|d| d["severity"] != "error"),
            "healed shape carries no error diagnostics: {diags:?}"
        );
    }

    #[test]
    fn malformed_frontmatter_is_skippable_bad_input() {
        // Delimiters present but the YAML inside is broken → OKF returns Err,
        // surfaced as BadInput so the caller can skip the artifact (mirrors the
        // unparseable-claims handling in store.rs). Diagnostics never do this.
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let owner = PrincipalId::new("a");
        let t = Transcript {
            session_id: "s".into(),
            turns: Vec::new(),
            frontmatter: Some(Frontmatter {
                source: FrontmatterSource::Yaml,
                concept_type: "note".into(),
                text: "---\nname: [unterminated\n  : : :\n---\nbody".into(),
            }),
        };
        let err = ingest(&t, &ctx(&owner), &mut store, &NoResidue).unwrap_err();
        assert!(
            matches!(err, MemoryError::BadInput(_)),
            "malformed frontmatter is skippable BadInput, got {err:?}"
        );
        // Nothing was written.
        assert_eq!(store.count(&owner).unwrap(), 0);
    }

    #[test]
    fn frontmatter_coexists_with_deterministic_pass() {
        // Frontmatter row AND turn candidates both write: the OKF row is additive,
        // not a replacement for the deterministic transcript pass.
        let mut store = SqliteMemoryStore::open_in_memory().unwrap();
        let owner = PrincipalId::new("a");
        let mut t = transcript("touched router.py and OCEAN-9");
        t.frontmatter = Some(Frontmatter {
            source: FrontmatterSource::Toml,
            concept_type: "handoff".into(),
            text: "+++\nsession_id = \"s-1\"\nrepo = \"ocean-os\"\nbranch = \"main\"\n\
                   commit_anchor = \"abc1234\"\n+++\n\nnarrative"
                .into(),
        });
        let report = ingest(&t, &ctx(&owner), &mut store, &NoResidue).unwrap();
        assert_eq!(report.normalized, 1, "one OKF row");
        assert_eq!(report.deterministic, 2, "router.py + OCEAN-9");
        assert_eq!(report.written, 3);
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
