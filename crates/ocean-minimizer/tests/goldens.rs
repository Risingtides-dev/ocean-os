use std::fs;
use std::path::PathBuf;

use ocean_minimizer::{
    minimize, Disposition, Invocation, PassthroughReason, Program, MAX_CAPTURE_BYTES,
};

fn fixture(path: &str) -> String {
    fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(path),
    )
    .unwrap_or_else(|error| panic!("read fixture {path}: {error}"))
}

fn assert_golden(invocation: Invocation, raw: &str, expected: &str, exit_code: i32) {
    let raw = fixture(raw);
    let expected = fixture(expected);
    let first = minimize(&invocation, &raw, exit_code);
    assert_eq!(first.disposition, Disposition::Minimized);
    assert_eq!(first.text, expected);
    assert_eq!(first.original_text.as_deref(), Some(raw.as_str()));
    assert_eq!(first.accounting.input_bytes, raw.len());
    assert_eq!(first.accounting.output_bytes, expected.len());

    // Repeat enough times to catch accidental order/state dependence.
    for _ in 0..32 {
        assert_eq!(minimize(&invocation, &raw, exit_code), first);
    }
}

#[test]
fn preserved_upstream_cargo_fixture() {
    assert_golden(
        Invocation::new(Program::Cargo, ["test", "--workspace"]),
        "cargo/test-pass.raw",
        "cargo/test-pass.min",
        0,
    );
}

#[test]
fn preserved_upstream_git_fixtures() {
    for case in ["status", "status-long"] {
        assert_golden(
            Invocation::new(Program::Git, ["status"]),
            &format!("git/{case}.raw"),
            &format!("git/{case}.min"),
            0,
        );
    }
    for case in ["log", "log-default"] {
        assert_golden(
            Invocation::new(Program::Git, ["log"]),
            &format!("git/{case}.raw"),
            &format!("git/{case}.min"),
            0,
        );
    }
}

#[test]
fn preserved_upstream_npm_fixture() {
    assert_golden(
        Invocation::new(Program::Npm, ["install"]),
        "npm/install.raw",
        "npm/install.min",
        0,
    );
}

#[test]
fn exact_gh_npx_and_pytest_goldens() {
    assert_golden(
        Invocation::new(Program::Gh, ["pr", "checks", "123"]),
        "gh/pr-checks.raw",
        "gh/pr-checks.min",
        1,
    );
    assert_golden(
        Invocation::new(Program::Npx, ["cowsay", "Hello"]),
        "npx/first-run.raw",
        "npx/first-run.min",
        0,
    );
    assert_golden(
        Invocation::new(Program::Pytest, ["-v"]),
        "pytest/success.raw",
        "pytest/success.min",
        0,
    );
    assert_golden(
        Invocation::new(Program::Pytest, ["tests/test_math.py"]),
        "pytest/failure.raw",
        "pytest/failure.min",
        1,
    );
}

#[test]
fn empty_and_already_small_outputs_are_unchanged() {
    for capture in ["", "warning: keep me\n", "Unicode 🌊 東京\n"] {
        let output = minimize(&Invocation::new(Program::Cargo, ["check"]), capture, 1);
        assert_eq!(
            output.disposition,
            Disposition::Passthrough(PassthroughReason::NoChange)
        );
        assert_eq!(output.text, capture);
        assert!(output.original_text.is_none());
    }
}

#[test]
fn unknown_and_ambiguous_shapes_are_byte_identical() {
    let cases = [
        (
            Invocation::new(Program::Cargo, ["metadata"]),
            "{\"packages\":[]}",
            PassthroughReason::UnsupportedInvocation,
        ),
        (
            Invocation::new(Program::Git, ["status"]),
            "a localized status shape\n",
            PassthroughReason::AmbiguousOutput,
        ),
        (
            Invocation::new(Program::Gh, ["pr", "checks", "1"]),
            "not a check table\n",
            PassthroughReason::AmbiguousOutput,
        ),
        (
            Invocation::new(Program::Gh, ["pr", "view", "checks"]),
            "✓\tbuild\t4m\thttps://example.test\n",
            PassthroughReason::UnsupportedInvocation,
        ),
        (
            Invocation::new(Program::Npm, ["install"]),
            "\n> package@1.0.0 postinstall   \n> custom-script\n",
            PassthroughReason::AmbiguousOutput,
        ),
        (
            Invocation::new(Program::Npm, ["install"]),
            "npm ERR! 2 packages are looking for funding unexpectedly\nfound 0 vulnerabilities while rollback failed\nProgress: migration failed\n",
            PassthroughReason::AmbiguousOutput,
        ),
        (
            Invocation::new(Program::Pytest, ["-q"]),
            "custom reporter without summary\n",
            PassthroughReason::AmbiguousOutput,
        ),
    ];
    for (invocation, capture, reason) in cases {
        let output = minimize(&invocation, capture, 0);
        assert_eq!(output.disposition, Disposition::Passthrough(reason));
        assert_eq!(output.text.as_bytes(), capture.as_bytes());
        assert!(output.original_text.is_none());
    }
}

#[test]
fn machine_and_explicit_raw_modes_are_byte_identical() {
    let cases = [
        Invocation::new(Program::Cargo, ["check", "--message-format=json"]),
        Invocation::new(Program::Git, ["status", "--porcelain=v2", "-z"]),
        Invocation::new(Program::Git, ["log", "--format=%H%x00%s", "-z"]),
        Invocation::new(Program::Gh, ["api", "repos/ocean"]),
        Invocation::new(Program::Gh, ["pr", "checks", "1", "--json", "name"]),
        Invocation::new(Program::Npm, ["install", "--json"]),
        Invocation::new(Program::Pytest, ["--junitxml=report.xml"]),
    ];
    let capture = "\u{1b}[31m{\"東京\":true}\u{1b}[0m\n";
    for invocation in cases {
        let output = minimize(&invocation, capture, 0);
        assert_eq!(
            output.disposition,
            Disposition::Passthrough(PassthroughReason::MachineReadableMode)
        );
        assert_eq!(output.text.as_bytes(), capture.as_bytes());
    }

    for invocation in [
        Invocation::new(Program::Cargo, ["test", "--", "--nocapture"]),
        Invocation::new(Program::Cargo, ["test", "--", "--show-output"]),
        Invocation::new(Program::Npm, ["install", "--foreground-scripts"]),
        Invocation::new(Program::Gh, ["pr", "diff", "1"]),
        Invocation::new(Program::Gh, ["pr", "checks", "1", "--watch"]),
        Invocation::new(Program::Gh, ["run", "view", "1", "--log"]),
    ] {
        let output = minimize(&invocation, capture, 0);
        assert_eq!(
            output.disposition,
            Disposition::Passthrough(PassthroughReason::ExplicitRawMode)
        );
        assert_eq!(output.text.as_bytes(), capture.as_bytes());
    }
}

#[test]
fn failed_npm_install_preserves_even_recognized_progress_shapes() {
    let capture = "Progress: resolved 1 / 2\nnpm ERR! install failed\n";
    let output = minimize(&Invocation::new(Program::Npm, ["install"]), capture, 1);
    assert_eq!(
        output.disposition,
        Disposition::Passthrough(PassthroughReason::AmbiguousOutput)
    );
    assert_eq!(output.text.as_bytes(), capture.as_bytes());
}

#[test]
fn pytest_traceback_status_words_are_not_rewritten_as_summaries() {
    let capture = "============================= FAILURES =============================\n____________________________ test_value ____________________________\nE assert \" failed in \" == expected\n1 failed in retry loop\n1 failed in 1..2s\n1 failed in 2s\n====================== 1 failed in 0.01s ======================\n";
    let output = minimize(&Invocation::new(Program::Pytest, ["-q"]), capture, 1);
    assert_eq!(output.disposition, Disposition::Minimized);
    assert!(output
        .text
        .contains("E assert \" failed in \" == expected\n"));
    assert!(output.text.contains("1 failed in retry loop\n"));
    assert!(output.text.contains("1 failed in 1..2s\n"));
    assert!(output.text.contains("1 failed in 2s\n"));
    assert!(!output.text.contains("pytest: E assert"));
    assert!(!output.text.contains("pytest: 1 failed in retry loop"));
    assert!(!output.text.contains("pytest: 1 failed in 1..2s"));
    assert!(!output.text.contains("pytest: 1 failed in 2s"));
    assert!(output.text.contains("pytest: 1 failed in 0.01s\n"));
}

#[test]
fn npm_filter_preserves_non_noise_bytes_around_recognized_noise() {
    let capture = "npm http fetch GET 200 https://registry.npmjs.org/a 1ms\nimportant line A\n\nimportant line B  \r\n\nnpm notice update available\nimportant line C\n";
    let expected = "important line A\n\nimportant line B  \r\n\nimportant line C\n";
    let output = minimize(&Invocation::new(Program::Npm, ["install"]), capture, 0);
    assert_eq!(output.disposition, Disposition::Minimized);
    assert_eq!(output.text.as_bytes(), expected.as_bytes());
}

#[test]
fn git_status_does_not_treat_an_indented_filename_as_a_footer() {
    let capture = "On branch main\nUntracked files:\n  (use \"git add <file>...\" to include in what will be committed)\n\tworking tree clean\n\t(use this filename)\n\nnothing added to commit but untracked files present\n";
    let output = minimize(&Invocation::new(Program::Git, ["status"]), capture, 0);
    assert_eq!(output.disposition, Disposition::Minimized);
    assert_eq!(
        output.text,
        "branch main\nstaged 0, unstaged 0, untracked 2\n?? working tree clean\n?? (use this filename)\n"
    );
}

#[test]
fn git_log_preserves_metadata_shaped_commit_content() {
    let capture = "commit abcdef1234567890\nAuthor: Real Author <real@example.test>\nDate:   Tue Jul 15 10:00:00 2026 +0000\n\n    Author: subject text\n\n    Date: body text\n    Merge: body text\n";
    let output = minimize(&Invocation::new(Program::Git, ["log"]), capture, 0);
    assert_eq!(output.disposition, Disposition::Minimized);
    assert_eq!(
        output.text,
        "abcdef1 Author: subject text\n  Date: body text\n  Merge: body text\n"
    );
}

#[test]
fn npx_without_a_first_install_preamble_is_not_line_capped() {
    let capture: String = (0..201).map(|index| format!("line {index}\n")).collect();
    let output = minimize(&Invocation::new(Program::Npx, ["tool"]), &capture, 0);
    assert_eq!(
        output.disposition,
        Disposition::Passthrough(PassthroughReason::AmbiguousOutput)
    );
    assert_eq!(output.text.as_bytes(), capture.as_bytes());
}

#[test]
fn nul_and_oversized_captures_fail_open_with_exact_accounting() {
    let nul = " M a\0?? b\0";
    let nul_output = minimize(&Invocation::new(Program::Git, ["status"]), nul, 0);
    assert_eq!(
        nul_output.disposition,
        Disposition::Passthrough(PassthroughReason::MachineReadableMode)
    );
    assert_eq!(nul_output.accounting.input_bytes, nul.len());
    assert_eq!(nul_output.accounting.output_bytes, nul.len());

    let oversized = "🌊".repeat(MAX_CAPTURE_BYTES / "🌊".len() + 1);
    let output = minimize(&Invocation::new(Program::Cargo, ["build"]), &oversized, 0);
    assert_eq!(
        output.disposition,
        Disposition::Passthrough(PassthroughReason::CaptureTooLarge)
    );
    assert_eq!(output.text.as_bytes(), oversized.as_bytes());
    assert_eq!(output.accounting.input_bytes, oversized.len());
    assert_eq!(output.accounting.output_bytes, oversized.len());
    assert!(output.original_text.is_none());
}
