//! Executable manifests for the repository's required Rust quality gates.

use std::process::{Command, ExitCode};

const MINIMUM_RUST_MINOR: &str = "1.88";

#[derive(Debug)]
struct Gate {
    label: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    deny: bool,
}

const REPOSITORY_GATES: &[Gate] = &[
    Gate {
        label: "workspace build",
        program: "cargo",
        args: &["build", "--workspace"],
        deny: false,
    },
    Gate {
        label: "workspace tests",
        program: "cargo",
        args: &["test", "--workspace"],
        deny: false,
    },
    Gate {
        label: "Clippy",
        program: "cargo",
        args: &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
        deny: false,
    },
    Gate {
        label: "format",
        program: "cargo",
        args: &["fmt", "--all", "--", "--check"],
        deny: false,
    },
    Gate {
        label: "dependency policy",
        program: "cargo",
        args: &["deny", "check"],
        deny: true,
    },
];

const COMPATIBILITY_GATES: &[Gate] = &[
    Gate {
        label: "daemon livekit-tap feature Clippy",
        program: "cargo",
        args: &[
            "clippy",
            "-p",
            "ocean-daemon",
            "--features",
            "livekit-tap",
            "--",
            "-D",
            "warnings",
        ],
        deny: false,
    },
    Gate {
        label: "daemon deepgram-stt feature Clippy",
        program: "cargo",
        args: &[
            "clippy",
            "-p",
            "ocean-daemon",
            "--features",
            "deepgram-stt",
            "--",
            "-D",
            "warnings",
        ],
        deny: false,
    },
    Gate {
        label: "daemon legacy-chromium feature Clippy",
        program: "cargo",
        args: &[
            "clippy",
            "-p",
            "ocean-daemon",
            "--features",
            "legacy-chromium",
            "--",
            "-D",
            "warnings",
        ],
        deny: false,
    },
    Gate {
        label: "release-profile workspace all-target check",
        program: "cargo",
        args: &["check", "--workspace", "--all-targets", "--release"],
        deny: false,
    },
];

const MSRV_GATES: &[Gate] = &[
    Gate {
        label: "MSRV workspace all-target check",
        program: "cargo",
        args: &["check", "--workspace", "--all-targets"],
        deny: false,
    },
    Gate {
        label: "MSRV daemon livekit-tap feature",
        program: "cargo",
        args: &["check", "-p", "ocean-daemon", "--features", "livekit-tap"],
        deny: false,
    },
    Gate {
        label: "MSRV daemon deepgram-stt feature",
        program: "cargo",
        args: &["check", "-p", "ocean-daemon", "--features", "deepgram-stt"],
        deny: false,
    },
    Gate {
        label: "MSRV daemon legacy-chromium feature",
        program: "cargo",
        args: &[
            "check",
            "-p",
            "ocean-daemon",
            "--features",
            "legacy-chromium",
        ],
        deny: false,
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lane {
    Repository,
    Compatibility,
    Msrv,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Options {
    lane: Lane,
    dry_run: bool,
    skip_deny: bool,
}

pub fn run(args: &[String]) -> ExitCode {
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("ci: {error}");
            return ExitCode::FAILURE;
        }
    };

    if options.dry_run {
        print_manifest(options);
        return ExitCode::SUCCESS;
    }

    match options.lane {
        Lane::Repository => run_repository_lane(options.skip_deny),
        Lane::Compatibility => run_gate_lane("compatibility", COMPATIBILITY_GATES),
        Lane::Msrv => {
            if !is_expected_msrv() {
                return ExitCode::FAILURE;
            }
            run_gate_lane("MSRV", MSRV_GATES)
        }
    }
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut dry_run = false;
    let mut skip_deny = false;
    let mut requested_lane = None;

    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--skip-deny" => skip_deny = true,
            "--compatibility" => set_lane(&mut requested_lane, Lane::Compatibility)?,
            "--msrv" => set_lane(&mut requested_lane, Lane::Msrv)?,
            other => {
                return Err(format!(
                    "unknown argument `{other}` (expected --dry-run, --skip-deny, --compatibility, or --msrv)"
                ));
            }
        }
    }

    let lane = requested_lane.unwrap_or(Lane::Repository);
    if skip_deny && lane != Lane::Repository {
        return Err("--skip-deny is valid only for the repository lane".into());
    }

    Ok(Options {
        lane,
        dry_run,
        skip_deny,
    })
}

fn set_lane(requested_lane: &mut Option<Lane>, lane: Lane) -> Result<(), String> {
    if requested_lane.replace(lane).is_some() {
        return Err("--compatibility and --msrv are mutually exclusive".into());
    }
    Ok(())
}

fn print_manifest(options: Options) {
    match options.lane {
        Lane::Repository => {
            println!("# documentation/index integrity");
            println!("cargo xtask docs-check");
            print_gate_commands(REPOSITORY_GATES, options.skip_deny);
            println!("# CI-only matrix/setup (reported, not run by the repository lane)");
            println!("# - ubuntu-latest and macos-latest runners");
            println!("# - macOS: brew install pkg-config");
            println!("# - Ubuntu feature lanes: apt-get install libglib2.0-dev");
            if options.skip_deny {
                println!("# - cargo deny check runs in the separate Ubuntu cargo-deny job");
            }
            println!("# - stable compatibility lane on both runners:");
            println!("#   cargo xtask ci --compatibility");
            for gate in COMPATIBILITY_GATES {
                println!("#   {} {}", gate.program, gate.args.join(" "));
            }
            println!("# - Rust {MINIMUM_RUST_MINOR} MSRV lane on Ubuntu:");
            println!("#   cargo xtask ci --msrv");
            for gate in MSRV_GATES {
                println!("#   {} {}", gate.program, gate.args.join(" "));
            }
        }
        Lane::Compatibility => print_gate_commands(COMPATIBILITY_GATES, false),
        Lane::Msrv => {
            println!("# requires rustc {MINIMUM_RUST_MINOR}.x");
            print_gate_commands(MSRV_GATES, false);
        }
    }
}

fn print_gate_commands(gates: &[Gate], skip_deny: bool) {
    for gate in gates.iter().filter(|gate| !skip_deny || !gate.deny) {
        println!("# {}", gate.label);
        println!("{} {}", gate.program, gate.args.join(" "));
    }
}

fn run_repository_lane(skip_deny: bool) -> ExitCode {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("ci: could not locate xtask binary: {error}");
            return ExitCode::FAILURE;
        }
    };
    if !run_command(
        "documentation/index integrity",
        &current_exe,
        &["docs-check"],
    ) {
        return ExitCode::FAILURE;
    }

    for gate in REPOSITORY_GATES
        .iter()
        .filter(|gate| !skip_deny || !gate.deny)
    {
        if !run_command(gate.label, gate.program, gate.args) {
            return ExitCode::FAILURE;
        }
    }
    println!("\nci: local repository gate passed; GitHub Actions supplies the OS matrix");
    if skip_deny {
        println!("ci: cargo-deny omitted here and must run in the separate Ubuntu job");
    }
    ExitCode::SUCCESS
}

fn run_gate_lane(label: &str, gates: &[Gate]) -> ExitCode {
    for gate in gates {
        if !run_command(gate.label, gate.program, gate.args) {
            return ExitCode::FAILURE;
        }
    }
    println!("\nci: {label} lane passed");
    ExitCode::SUCCESS
}

fn is_expected_msrv() -> bool {
    let output = match Command::new("rustc").arg("--version").output() {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            eprintln!("ci: rustc --version failed with {}", output.status);
            return false;
        }
        Err(error) => {
            eprintln!("ci: could not run rustc --version: {error}");
            return false;
        }
    };
    let version = String::from_utf8_lossy(&output.stdout);
    let expected_prefix = format!("rustc {MINIMUM_RUST_MINOR}.");
    if version.starts_with(&expected_prefix) {
        true
    } else {
        eprintln!(
            "ci: MSRV lane requires rustc {MINIMUM_RUST_MINOR}.x, found `{}`",
            version.trim()
        );
        eprintln!(
            "ci: rust-toolchain.toml pins a newer toolchain; run the MSRV lane as \
             `RUSTUP_TOOLCHAIN={MINIMUM_RUST_MINOR}.0 cargo xtask ci --msrv` \
             (the env override outranks the pin file)"
        );
        false
    }
}

fn run_command<P>(label: &str, program: P, args: &[&str]) -> bool
where
    P: AsRef<std::ffi::OsStr>,
{
    println!("\n==> {label}");
    match Command::new(program).args(args).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("ci: {label} failed with {status}");
            false
        }
        Err(error) => {
            eprintln!("ci: could not start {label}: {error}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(gates: &[Gate]) -> Vec<String> {
        gates
            .iter()
            .map(|gate| format!("{} {}", gate.program, gate.args.join(" ")))
            .collect()
    }

    #[test]
    fn repository_manifest_matches_required_gate() {
        assert_eq!(
            commands(REPOSITORY_GATES),
            [
                "cargo build --workspace",
                "cargo test --workspace",
                "cargo clippy --workspace --all-targets -- -D warnings",
                "cargo fmt --all -- --check",
                "cargo deny check",
            ]
        );
    }

    #[test]
    fn compatibility_manifest_is_exact() {
        assert_eq!(
            commands(COMPATIBILITY_GATES),
            [
                "cargo clippy -p ocean-daemon --features livekit-tap -- -D warnings",
                "cargo clippy -p ocean-daemon --features deepgram-stt -- -D warnings",
                "cargo clippy -p ocean-daemon --features legacy-chromium -- -D warnings",
                "cargo check --workspace --all-targets --release",
            ]
        );
    }

    #[test]
    fn msrv_manifest_is_exact() {
        assert_eq!(
            commands(MSRV_GATES),
            [
                "cargo check --workspace --all-targets",
                "cargo check -p ocean-daemon --features livekit-tap",
                "cargo check -p ocean-daemon --features deepgram-stt",
                "cargo check -p ocean-daemon --features legacy-chromium",
            ]
        );
    }

    #[test]
    fn lane_flags_are_mutually_exclusive() {
        let args = vec!["--compatibility".into(), "--msrv".into()];
        assert!(parse_options(&args).is_err());
    }

    #[test]
    fn skip_deny_is_repository_only() {
        let args = vec!["--msrv".into(), "--skip-deny".into()];
        assert!(parse_options(&args).is_err());
    }

    #[test]
    fn workflow_consumes_each_manifest_lane() {
        let workflow = include_str!("../../.github/workflows/ci.yml");
        assert!(workflow.contains("cargo xtask ci --skip-deny"));
        assert!(workflow.contains("cargo xtask ci --compatibility"));
        assert!(workflow.contains("cargo xtask ci --msrv"));
        assert!(workflow.contains("dtolnay/rust-toolchain@1.88.0"));
        assert!(workflow.matches("libglib2.0-dev").count() >= 2);
    }

    #[test]
    fn workspace_declares_enforced_msrv() {
        let manifest = include_str!("../../Cargo.toml");
        assert!(manifest.contains("rust-version = \"1.88\""));
    }
}
