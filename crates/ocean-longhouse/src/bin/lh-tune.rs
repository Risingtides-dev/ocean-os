//! `lh-tune` — replay a recorded council through a grid of quorum configs and
//! print a comparison table. The deterministic tuning cockpit for the engine.
//!
//! A recording is the JSON mark-stream captured from a real council
//! (`ConveneOutcome::recording`, serialized). Replaying it under many configs is
//! instant and free — no LLM calls, fully deterministic — so you can see at a
//! glance which threshold / margin / decay makes the SAME council converge
//! eagerly, deadlock, or split.
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
use ocean_longhouse::replay::{replay, RecordedMark, RecordedMarkKind, Recording, ReplayResult};
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
    println!();
}

fn print_table(rows: &[(QuorumConfig, ReplayResult)]) {
    println!(
        "  {:<26} {:>8} {:>9} {:>10}  {}",
        "config", "conv@", "winner", "lead net", "outcome"
    );
    println!("  {}", "─".repeat(72));
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
        println!(
            "  {:<26} {:>8} {:>9} {:>10}  {}",
            describe_config(cfg),
            conv,
            winner,
            lead,
            outcome
        );
    }
    println!();
}

fn print_legend() {
    println!("  conv@   = first mark that crossed quorum mid-stream (— = never)");
    println!("  winner  = converged proposal, or the deadline force-resolve pick");
    println!("  outcome = converged (mid-stream) / deadline-resolved / SPLIT (no leader)");
    println!();
    println!("  Tuning read:");
    println!("   • lots of 'mark 1-2' converges → cutoff too LOW (converges on noise)");
    println!("   • lots of '—' / SPLIT          → cutoff too HIGH (deadlocks)");
    println!("   • the sweet spot converges mid-stream but only after real support forms");
    println!();
}

/// The starting tuning grid: cutoff × margin × decay, plus a couple sigmoids.
/// This is the knob-space — edit freely to explore a specific recording.
fn build_grid() -> Vec<QuorumConfig> {
    let mut grid = Vec::new();
    let cutoffs = [1.5f32, 2.0, 2.5, 3.0, 4.0];
    let margins = [0.5f32, 1.0, 1.5];
    let ttls = [30_000i64, 60_000, 120_000];

    for &cutoff in &cutoffs {
        for &margin in &margins {
            for &ttl in &ttls {
                grid.push(QuorumConfig {
                    rule: QuorumRule::NetWeight { cutoff, margin },
                    mark_ttl_ms: ttl,
                    tie_break_seed: 0xC0FFEE,
                });
            }
        }
    }
    // A few sigmoid responses for comparison.
    for &steepness in &[2.0f32, 4.0] {
        for &midpoint in &[2.0f32, 3.0] {
            grid.push(QuorumConfig {
                rule: QuorumRule::Sigmoid {
                    midpoint,
                    steepness,
                    margin: 1.0,
                },
                mark_ttl_ms: 60_000,
                tie_break_seed: 0xC0FFEE,
            });
        }
    }
    grid
}

fn describe_config(cfg: &QuorumConfig) -> String {
    let rule = match cfg.rule {
        QuorumRule::NetWeight { cutoff, margin } => {
            format!("net c={cutoff:.1} m={margin:.1}")
        }
        QuorumRule::Sigmoid {
            midpoint,
            steepness,
            margin,
        } => format!("sig μ={midpoint:.1} k={steepness:.0} m={margin:.1}"),
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
