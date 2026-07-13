//! One executable manifest for the repository's required Rust quality gates.

use std::process::{Command, ExitCode};

#[derive(Debug)]
struct Gate {
    label: &'static str,
    program: &'static str,
    args: &'static [&'static str],
    deny: bool,
}

const GATES: &[Gate] = &[
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

pub fn run(args: &[String]) -> ExitCode {
    let mut dry_run = false;
    let mut skip_deny = false;
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--skip-deny" => skip_deny = true,
            other => {
                eprintln!("ci: unknown argument `{other}` (expected --dry-run or --skip-deny)");
                return ExitCode::FAILURE;
            }
        }
    }

    if dry_run {
        println!("# documentation/index integrity");
        println!("cargo xtask docs-check");
        for gate in GATES.iter().filter(|gate| !skip_deny || !gate.deny) {
            println!("# {}", gate.label);
            println!("{} {}", gate.program, gate.args.join(" "));
        }
        println!("# CI-only matrix/setup (reported, not run locally)");
        println!("# - ubuntu-latest and macos-latest runners");
        println!("# - macOS: brew install pkg-config");
        if skip_deny {
            println!("# - cargo deny check runs in the separate Ubuntu cargo-deny job");
        }
        return ExitCode::SUCCESS;
    }

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

    for gate in GATES.iter().filter(|gate| !skip_deny || !gate.deny) {
        if !run_command(gate.label, gate.program, gate.args) {
            return ExitCode::FAILURE;
        }
    }
    println!("\nci: local gate passed; GitHub Actions still supplies the macOS/Ubuntu matrix and runner setup");
    if skip_deny {
        println!("ci: cargo-deny omitted here and must run in the separate Ubuntu job");
    }
    ExitCode::SUCCESS
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

    #[test]
    fn manifest_matches_repository_gate() {
        let commands: Vec<String> = GATES
            .iter()
            .map(|gate| format!("{} {}", gate.program, gate.args.join(" ")))
            .collect();
        assert_eq!(
            commands,
            [
                "cargo build --workspace",
                "cargo test --workspace",
                "cargo clippy --workspace --all-targets -- -D warnings",
                "cargo fmt --all -- --check",
                "cargo deny check",
            ]
        );
    }
}
