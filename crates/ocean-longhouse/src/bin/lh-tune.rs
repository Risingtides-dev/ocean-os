//! `lh-tune` — replay a recorded council through a grid of quorum configs and
//! print a comparison table. The deterministic tuning cockpit for the engine.
//!
//! A recording is the JSON mark-stream captured from a real council
//! (`ConveneOutcome::recording`, serialized). Replaying it under many configs is
//! instant and free — no LLM calls, fully deterministic — so you can see at a
//! glance which evidence target / correlation cap / query cost / decay makes
//! the SAME council converge, continue, or split.
//!
//! Usage:
//!   lh-tune <recording.json>           # sweep the built-in grid
//!   lh-tune --demo                     # use a synthetic recording (no file)
//!
//! Capture a recording from a live council by serializing the convene outcome:
//!   serde_json::to_writer(file, &outcome.recording)?;
//!
//! The grid below is the starting tuning space. Edit `build_grid` to explore.

use std::process::ExitCode;

use ocean_longhouse::quorum::{QuorumConfig, QuorumRule};
use ocean_longhouse::replay::{
    replay, RecordedMark, RecordedMarkKind, RecordedReviewer, Recording, ReplayResult,
};
use ocean_longhouse::SequentialEvidenceConfig;
use uuid::Uuid;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let recording = match args.first().map(String::as_str) {
        Some("--demo") => demo_recording(),
        Some(path) => match load_recording(path) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error: could not load recording from {path}: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            eprintln!("usage: lh-tune <recording.json>  |  lh-tune --demo");
            return ExitCode::FAILURE;
        }
    };

    print_header(&recording);

    let grid = build_grid();
    let mut rows: Vec<(QuorumConfig, ReplayResult)> = grid
        .iter()
        .map(|cfg| (*cfg, replay(&recording, *cfg)))
        .collect();

    // Sort: converged-early first (fewest marks), then by how decisive the
    // final field is, so the "cleanest" configs float to the top.
    rows.sort_by(|a, b| {
        let ak = a.1.converged_after_marks.unwrap_or(usize::MAX);
        let bk = b.1.converged_after_marks.unwrap_or(usize::MAX);
        ak.cmp(&bk)
    });

    print_table(&rows);
    print_legend();
    ExitCode::SUCCESS
}

fn load_recording(path: &str) -> Result<Recording, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn print_header(rec: &Recording) {
    println!();
    println!("  council: {}", truncate(&rec.question, 70));
    println!(
        "  marks:   {}  across {} proposal(s)",
        rec.marks.len(),
        rec.proposal_ids().len()
    );
    let span = rec.marks.last().map(|m| m.at_ms).unwrap_or(0);
    println!("  span:    {span} ms");
    println!("  reviewer credentials: {}", rec.reviewers.len());
    println!();
}

fn print_table(rows: &[(QuorumConfig, ReplayResult)]) {
    println!(
        "  {:<26} {:>8} {:>9} {:>10} {:>16}  outcome",
        "config", "conv@", "winner", "lead net", "basis"
    );
    println!("  {}", "─".repeat(90));
    for (cfg, res) in rows {
        let conv = res
            .converged_after_marks
            .map(|n| format!("mark {n}"))
            .unwrap_or_else(|| "—".into());
        let winner = res
            .converged_on
            .or(res.force_resolved)
            .map(short_id)
            .unwrap_or_else(|| "split".into());
        let lead = res
            .final_tally
            .first()
            .map(|(_, w)| format!("{w:+.2}"))
            .unwrap_or_else(|| "—".into());
        let outcome = match (res.converged_on, res.force_resolved) {
            (Some(_), _) => "converged",
            (None, Some(_)) => "deadline-resolved",
            (None, None) => "SPLIT",
        };
        let basis = res
            .convergence_basis
            .map(|basis| basis.as_str())
            .unwrap_or("—");
        println!(
            "  {:<26} {:>8} {:>9} {:>10} {:>16}  {}",
            describe_config(cfg),
            conv,
            winner,
            lead,
            basis,
            outcome
        );
    }
    println!();
}

fn print_legend() {
    println!("  conv@   = first mark that crossed quorum mid-stream (— = never)");
    println!("  winner  = converged proposal, or the deadline force-resolve pick");
    println!("  basis   = exact daemon stopping condition");
    println!("  outcome = converged (mid-stream) / deadline-resolved / SPLIT (no leader)");
    println!();
    println!("  Tuning read:");
    println!("   - lots of 'mark 1-2' converges -> evidence/cost bound too loose");
    println!("   - lots of '—' / SPLIT          -> evidence/cost bound too strict");
    println!("   - compare correlated recordings; duplicate models must not add mass");
    println!();
}

/// The starting tuning grid: evidence error × group cap × query cost × decay,
/// plus a small legacy net-weight baseline for comparison.
fn build_grid() -> Vec<QuorumConfig> {
    let mut grid = Vec::new();
    let ttls = [30_000i64, 60_000, 120_000];
    let default_cap = SequentialEvidenceConfig::default().correlation_cap();

    for &target_error in &[0.10, 0.20] {
        for &correlation_cap in &[0.80, default_cap] {
            for &query_cost in &[0.0, 0.02, 0.10] {
                for &ttl in &ttls {
                    if let Ok(evidence) = SequentialEvidenceConfig::new(
                        target_error,
                        0.75,
                        correlation_cap,
                        query_cost,
                        1.0,
                    ) {
                        grid.push(QuorumConfig {
                            rule: QuorumRule::SequentialEvidence(evidence),
                            mark_ttl_ms: ttl,
                            tie_break_seed: 0xC0FFEE,
                        });
                    }
                }
            }
        }
    }
    for &(cutoff, margin) in &[(2.0, 1.0), (3.0, 1.0)] {
        grid.push(QuorumConfig {
            rule: QuorumRule::NetWeight { cutoff, margin },
            mark_ttl_ms: 60_000,
            tie_break_seed: 0xC0FFEE,
        });
    }
    grid
}

fn describe_config(cfg: &QuorumConfig) -> String {
    let rule = match cfg.rule {
        QuorumRule::NetWeight { cutoff, margin } => {
            format!("net c={cutoff:.1} m={margin:.1}")
        }
        QuorumRule::SequentialEvidence(evidence) => format!(
            "seq e={:.2} cap={:.2} c={:.2}",
            evidence.target_error(),
            evidence.correlation_cap(),
            evidence.query_cost()
        ),
    };
    format!("{rule} ttl={}s", cfg.mark_ttl_ms / 1000)
}

fn short_id(id: Uuid) -> String {
    // Last 8 hex chars: real councils use random UUIDs (any slice works), and
    // it disambiguates structured/test ids whose prefixes collide.
    let s = id.simple().to_string();
    s.chars().skip(s.len().saturating_sub(8)).collect()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// A synthetic recording for `--demo`: a clear leader emerges over a rival, with
/// some cross-inhibition, spread across ~3s so decay has something to bite.
fn demo_recording() -> Recording {
    fn uid(n: u8) -> Uuid {
        let mut b = [0u8; 16];
        b[15] = n;
        Uuid::from_bytes(b)
    }
    let (pa, pb) = (uid(1), uid(2));
    let (a1, a2, a3, b1, b2) = (uid(10), uid(11), uid(12), uid(20), uid(21));
    Recording {
        question: "What 3 TikTok hooks to test for an indie-pop launch?".into(),
        reviewers: vec![
            RecordedReviewer {
                agent_id: a1,
                correlation_group: "provider:model-a".into(),
                reliability_prior: 0.75,
            },
            RecordedReviewer {
                agent_id: a2,
                correlation_group: "provider:model-b".into(),
                reliability_prior: 0.75,
            },
            RecordedReviewer {
                agent_id: a3,
                correlation_group: "provider:model-d".into(),
                reliability_prior: 0.75,
            },
            RecordedReviewer {
                agent_id: b1,
                correlation_group: "provider:model-c".into(),
                reliability_prior: 0.75,
            },
            RecordedReviewer {
                agent_id: b2,
                correlation_group: "provider:model-c".into(),
                reliability_prior: 0.75,
            },
        ],
        marks: vec![
            RecordedMark {
                at_ms: 0,
                author: a1,
                kind: RecordedMarkKind::Propose { proposal: pa },
            },
            RecordedMark {
                at_ms: 200,
                author: b1,
                kind: RecordedMarkKind::Propose { proposal: pb },
            },
            RecordedMark {
                at_ms: 1_100,
                author: a2,
                kind: RecordedMarkKind::Endorse { proposal: pa },
            },
            RecordedMark {
                at_ms: 1_400,
                author: b2,
                kind: RecordedMarkKind::Endorse { proposal: pb },
            },
            RecordedMark {
                at_ms: 1_900,
                author: a3,
                kind: RecordedMarkKind::Endorse { proposal: pa },
            },
            RecordedMark {
                at_ms: 2_300,
                author: a1,
                kind: RecordedMarkKind::Inhibit { proposal: pb },
            },
        ],
    }
}
