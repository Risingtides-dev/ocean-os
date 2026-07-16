use std::fmt::Write as _;

use crate::{Invocation, PassthroughReason};

pub(crate) enum FilterResult {
    Text(String),
    Passthrough(PassthroughReason),
}

pub(crate) fn cargo(invocation: &Invocation, input: &str, exit_code: i32) -> FilterResult {
    let Some(subcommand) = invocation.args.first().map(String::as_str) else {
        return unsupported();
    };
    if has_any_flag(
        &invocation.args,
        &["--message-format", "--format", "--json", "--unit-graph"],
    ) {
        return machine();
    }
    if subcommand == "test" && has_any_flag(&invocation.args, &["--nocapture", "--show-output"]) {
        return FilterResult::Passthrough(PassthroughReason::ExplicitRawMode);
    }

    let text = match subcommand {
        "build" | "check" => strip_cargo_progress(input),
        "test" => filter_cargo_test(input, exit_code),
        "clippy" => strip_cargo_progress(input),
        // rustfmt diffs are already dense. The shared final cap is the only M1
        // transform, preserving every line for ordinary outputs.
        "fmt" => input.to_owned(),
        _ => return unsupported(),
    };
    FilterResult::Text(text)
}

fn strip_cargo_progress(input: &str) -> String {
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim_start();
        if is_cargo_progress(trimmed) || is_generated_warning_rollup(trimmed) {
            continue;
        }
        push_line(&mut out, line);
    }
    out
}

fn is_cargo_progress(line: &str) -> bool {
    [
        "Compiling ",
        "Checking ",
        "Fresh ",
        "Finished ",
        "Documenting ",
        "Running ",
        "Downloading ",
        "Downloaded ",
        "Locking ",
        "Updating ",
        "Blocking ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

fn is_generated_warning_rollup(line: &str) -> bool {
    line.strip_prefix("warning: ").is_some_and(|rest| {
        rest.contains(" generated ") && (rest.ends_with(" warning") || rest.ends_with(" warnings"))
    })
}

#[derive(Default)]
struct CargoTotals {
    suites: usize,
    passed: u64,
    failed: u64,
    ignored: u64,
    measured: u64,
    filtered: u64,
    warnings: u64,
    duration: Option<String>,
}

fn filter_cargo_test(input: &str, exit_code: i32) -> String {
    if exit_code != 0 {
        let mut out = String::new();
        for line in input.lines() {
            let trimmed = line.trim_start();
            if is_cargo_progress(trimmed)
                || is_generated_warning_rollup(trimmed)
                || is_passing_cargo_test(trimmed)
            {
                continue;
            }
            push_line(&mut out, line);
        }
        return out;
    }

    let mut totals = CargoTotals::default();
    for line in input.lines() {
        let trimmed = line.trim();
        if let Some(summary) = trimmed.strip_prefix("test result: ok.") {
            totals.suites += 1;
            collect_cargo_summary(summary, &mut totals);
        } else if let Some(count) = generated_warning_count(trimmed) {
            totals.warnings += count;
        }
    }
    if totals.suites == 0 {
        return strip_cargo_progress(input);
    }

    // Keep only lines that are neither known cargo/test framing nor per-test
    // success rows. This preserves warnings, diagnostics, and --nocapture text.
    let mut out = String::new();
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || is_cargo_progress(line.trim_start())
            || is_generated_warning_rollup(trimmed)
            || trimmed.starts_with("running ")
            || is_passing_cargo_test(trimmed)
            || trimmed.starts_with("test result: ok.")
            || trimmed.starts_with("Doc-tests ")
        {
            continue;
        }
        push_line(&mut out, line);
    }

    let _ = write!(out, "cargo test: {} passed (", totals.passed);
    let _ = write!(
        out,
        "{} suite{}",
        totals.suites,
        if totals.suites == 1 { "" } else { "s" }
    );
    if totals.failed > 0 {
        let _ = write!(out, ", {} failed", totals.failed);
    }
    if totals.ignored > 0 {
        let _ = write!(out, ", {} ignored", totals.ignored);
    }
    if totals.measured > 0 {
        let _ = write!(out, ", {} measured", totals.measured);
    }
    if totals.filtered > 0 {
        let _ = write!(out, ", {} filtered", totals.filtered);
    }
    if totals.warnings > 0 {
        let _ = write!(out, ", {} warnings", totals.warnings);
    }
    if let Some(duration) = totals.duration {
        let _ = write!(out, ", {duration}");
    }
    out.push_str(")\n");
    out
}

fn is_passing_cargo_test(line: &str) -> bool {
    line.starts_with("test ") && (line.ends_with(" ... ok") || line.ends_with("... ok"))
}

fn generated_warning_count(line: &str) -> Option<u64> {
    if !is_generated_warning_rollup(line) {
        return None;
    }
    line.rsplit_once(" warning")?
        .0
        .rsplit_once(' ')?
        .1
        .parse()
        .ok()
}

fn collect_cargo_summary(summary: &str, totals: &mut CargoTotals) {
    for part in summary.split(';') {
        let part = part.trim().trim_end_matches('.');
        let Some((count, label)) = part.split_once(' ') else {
            if let Some(duration) = part.strip_prefix("finished in ") {
                totals.duration = Some(duration.to_owned());
            }
            continue;
        };
        let Ok(count) = count.parse::<u64>() else {
            if let Some(duration) = part.strip_prefix("finished in ") {
                totals.duration = Some(duration.to_owned());
            }
            continue;
        };
        match label {
            "passed" => totals.passed += count,
            "failed" => totals.failed += count,
            "ignored" => totals.ignored += count,
            "measured" => totals.measured += count,
            "filtered out" => totals.filtered += count,
            _ => {}
        }
    }
}

pub(crate) fn git(invocation: &Invocation, input: &str) -> FilterResult {
    let Some(subcommand) = invocation.args.first().map(String::as_str) else {
        return unsupported();
    };
    match subcommand {
        "status" => {
            let rest = &invocation.args[1..];
            if is_git_machine_status(rest) || input.contains('\0') {
                return machine();
            }
            if !rest.is_empty() {
                return unsupported();
            }
            match condense_git_status(input) {
                Some(text) => FilterResult::Text(text),
                None => ambiguous(),
            }
        }
        "log" => {
            let rest = &invocation.args[1..];
            if has_any_flag(
                rest,
                &[
                    "--format",
                    "--pretty",
                    "--oneline",
                    "--raw",
                    "--numstat",
                    "--name-only",
                    "--name-status",
                    "--stat",
                    "--shortstat",
                    "--dirstat",
                    "--patch",
                    "-p",
                    "-z",
                ],
            ) {
                return machine();
            }
            match condense_git_log(input) {
                Some(text) => FilterResult::Text(text),
                None => ambiguous(),
            }
        }
        _ => unsupported(),
    }
}

fn is_git_machine_status(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--porcelain" | "--porcelain=v1" | "--porcelain=v2" | "--null" | "-z"
        ) || (arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains('z'))
    })
}

#[derive(Default)]
struct StatusSummary {
    branch: Option<String>,
    divergence: Option<String>,
    clean: bool,
    staged: usize,
    unstaged: usize,
    untracked: usize,
    conflicts: usize,
    paths: Vec<String>,
}

#[derive(Clone, Copy)]
enum StatusSection {
    None,
    Staged,
    Unstaged,
    Untracked,
    Conflicts,
}

fn condense_git_status(input: &str) -> Option<String> {
    let mut summary = StatusSummary::default();
    let mut section = StatusSection::None;
    let mut saw_signal = false;

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(branch) = trimmed.strip_prefix("On branch ") {
            summary.branch = Some(branch.to_owned());
            saw_signal = true;
            continue;
        }
        if trimmed.starts_with("Your branch is up to date with ") {
            continue;
        }
        if trimmed.starts_with("Your branch is ahead")
            || trimmed.starts_with("Your branch is behind")
            || trimmed.starts_with("Your branch and ")
            || trimmed.starts_with("HEAD detached")
        {
            summary.divergence = Some(trimmed.to_owned());
            saw_signal = true;
            continue;
        }
        if trimmed.starts_with("Changes to be committed:") {
            section = StatusSection::Staged;
            continue;
        }
        if trimmed.starts_with("Changes not staged for commit:") {
            section = StatusSection::Unstaged;
            continue;
        }
        if trimmed.starts_with("Untracked files:") {
            section = StatusSection::Untracked;
            continue;
        }
        if trimmed.starts_with("Unmerged paths:") {
            section = StatusSection::Conflicts;
            continue;
        }
        // Long-status paths are indented. Parse them before footer phrases so
        // valid filenames such as `working tree clean` are not mistaken for
        // repository state.
        if line.starts_with('\t') && parse_long_status_entry(line, section, &mut summary) {
            saw_signal = true;
            continue;
        }
        if trimmed.starts_with("nothing to commit") || trimmed == "working tree clean" {
            summary.clean = true;
            saw_signal = true;
            continue;
        }
        if trimmed.starts_with("(use ")
            || trimmed.starts_with("no changes added")
            || trimmed.starts_with("nothing added to commit")
            || trimmed == "You have unmerged paths."
        {
            continue;
        }
        return None;
    }

    if !saw_signal {
        return None;
    }
    let mut out = String::new();
    if let Some(branch) = summary.branch {
        let _ = writeln!(out, "branch {branch}");
    }
    if let Some(divergence) = summary.divergence {
        let _ = writeln!(out, "{divergence}");
    }
    if summary.clean && summary.paths.is_empty() {
        out.push_str("clean\n");
        return Some(out);
    }
    let _ = write!(
        out,
        "staged {}, unstaged {}, untracked {}",
        summary.staged, summary.unstaged, summary.untracked
    );
    if summary.conflicts > 0 {
        let _ = write!(out, ", conflicts {}", summary.conflicts);
    }
    out.push('\n');
    for path in summary.paths {
        push_line(&mut out, &path);
    }
    Some(out)
}

fn parse_long_status_entry(
    line: &str,
    section: StatusSection,
    summary: &mut StatusSummary,
) -> bool {
    let trimmed = line.trim();
    if matches!(section, StatusSection::Untracked) && !trimmed.ends_with(':') {
        summary.untracked += 1;
        summary.paths.push(format!("?? {trimmed}"));
        return true;
    }

    let entries = [
        ("modified:", "M"),
        ("deleted:", "D"),
        ("new file:", "A"),
        ("renamed:", "R"),
        ("both modified:", "UU"),
        ("both added:", "AA"),
        ("both deleted:", "DD"),
        ("added by us:", "AU"),
        ("added by them:", "UA"),
        ("deleted by us:", "DU"),
        ("deleted by them:", "UD"),
    ];
    for (prefix, label) in entries {
        let Some(path) = trimmed.strip_prefix(prefix).map(str::trim) else {
            continue;
        };
        if path.is_empty() {
            return false;
        }
        match section {
            StatusSection::Staged => summary.staged += 1,
            StatusSection::Unstaged => summary.unstaged += 1,
            StatusSection::Conflicts => summary.conflicts += 1,
            StatusSection::None | StatusSection::Untracked => return false,
        }
        summary.paths.push(format!("{label} {path}"));
        return true;
    }
    false
}

struct LogEntry {
    hash: String,
    decoration: String,
    subject: String,
    body: Vec<String>,
    elided_body_lines: usize,
}

fn condense_git_log(input: &str) -> Option<String> {
    if input.is_empty() {
        return Some(String::new());
    }
    let mut entries = Vec::new();
    let mut current: Option<LogEntry> = None;

    for line in input.lines() {
        if let Some(rest) = line.strip_prefix("commit ") {
            if let Some(entry) = current.take() {
                if entry.subject.is_empty() {
                    return None;
                }
                entries.push(entry);
            }
            let hash_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            let hash = &rest[..hash_end];
            if hash.len() < 7 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return None;
            }
            current = Some(LogEntry {
                hash: hash.chars().take(7).collect(),
                decoration: rest[hash_end..].trim().to_owned(),
                subject: String::new(),
                body: Vec::new(),
                elided_body_lines: 0,
            });
            continue;
        }

        let entry = current.as_mut()?;
        let trimmed = line.trim();
        if trimmed.is_empty()
            || line.starts_with("Author:")
            || line.starts_with("Date:")
            || line.starts_with("Merge:")
        {
            continue;
        }
        if !line.starts_with("    ") {
            return None;
        }
        if entry.subject.is_empty() {
            entry.subject = trimmed.to_owned();
        } else if !is_git_trailer(trimmed) {
            if entry.body.len() < 3 {
                entry.body.push(trimmed.to_owned());
            } else {
                entry.elided_body_lines += 1;
            }
        }
    }
    if let Some(entry) = current {
        if entry.subject.is_empty() {
            return None;
        }
        entries.push(entry);
    }
    if entries.is_empty() {
        return None;
    }

    let mut out = String::new();
    for entry in entries {
        out.push_str(&entry.hash);
        if entry.decoration.is_empty() {
            out.push(' ');
            out.push_str(&entry.subject);
            out.push('\n');
        } else {
            out.push(' ');
            out.push_str(&entry.decoration);
            out.push('\n');
            out.push_str("  ");
            out.push_str(&entry.subject);
            out.push('\n');
        }
        for line in entry.body {
            out.push_str("  ");
            out.push_str(&line);
            out.push('\n');
        }
        if entry.elided_body_lines > 0 {
            let _ = writeln!(out, "  […{} body lines elided…]", entry.elided_body_lines);
        }
    }
    Some(out)
}

fn is_git_trailer(line: &str) -> bool {
    [
        "Signed-off-by:",
        "Co-authored-by:",
        "Acked-by:",
        "Reviewed-by:",
        "Tested-by:",
        "Reported-by:",
        "Change-Id:",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

pub(crate) fn gh(invocation: &Invocation, input: &str) -> FilterResult {
    if invocation.args.first().is_some_and(|arg| arg == "api")
        || has_any_flag(&invocation.args, &["--json", "--jq", "--template"])
    {
        return machine();
    }
    if command_path(&invocation.args, "pr", "diff")
        || command_path(&invocation.args, "run", "view")
            && has_any_flag(&invocation.args, &["--log", "--log-failed"])
        || command_path(&invocation.args, "pr", "checks")
            && has_any_flag(&invocation.args, &["--watch", "--interval", "-i"])
    {
        return FilterResult::Passthrough(PassthroughReason::ExplicitRawMode);
    }
    if !command_path(&invocation.args, "pr", "checks") {
        return unsupported();
    }

    let mut passed = 0usize;
    let mut pending = 0usize;
    let mut skipping = 0usize;
    let mut failed = Vec::new();
    for line in input.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim_start();
        if trimmed.split('\t').count() < 4 {
            return ambiguous();
        }
        match trimmed.chars().next() {
            Some('✓') => passed += 1,
            Some('X' | '✗' | '×') => failed.push(line.trim_end()),
            Some('*') => pending += 1,
            Some('-') => skipping += 1,
            _ => return ambiguous(),
        }
    }
    if passed + pending + skipping + failed.len() == 0 {
        return ambiguous();
    }

    let mut out = format!("checks: {passed} passed, {} failed", failed.len());
    if pending > 0 {
        let _ = write!(out, ", {pending} pending");
    }
    if skipping > 0 {
        let _ = write!(out, ", {skipping} skipping");
    }
    out.push('\n');
    for row in failed {
        push_line(&mut out, row);
    }
    FilterResult::Text(out)
}

pub(crate) fn npm(invocation: &Invocation, input: &str, exit_code: i32) -> FilterResult {
    let Some(subcommand) = invocation.args.first().map(String::as_str) else {
        return unsupported();
    };
    if !matches!(subcommand, "install" | "i" | "ci") {
        return unsupported();
    }
    if has_any_flag(&invocation.args, &["--json", "--parseable"]) {
        return machine();
    }
    if has_any_flag(&invocation.args, &["--foreground-scripts"]) {
        return FilterResult::Passthrough(PassthroughReason::ExplicitRawMode);
    }
    if exit_code != 0 {
        return ambiguous();
    }

    let saw_noise = input.lines().any(is_npm_noise_line);
    if !saw_noise {
        return ambiguous();
    }

    let segments: Vec<&str> = input.split_inclusive('\n').collect();
    let mut out = String::new();
    let mut fund_hint_pending = false;
    for (index, line_with_ending) in segments.iter().enumerate() {
        let line = line_with_ending.trim_end_matches(['\r', '\n']);
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if is_npm_noise_line(line) {
            fund_hint_pending |= is_npm_funding_line(trimmed);
            continue;
        }
        if fund_hint_pending && (trimmed.is_empty() || lower == "run `npm fund` for details") {
            continue;
        }
        fund_hint_pending = false;
        let next_starts_funding_block = segments
            .get(index + 1)
            .is_some_and(|next| is_npm_funding_line(next.trim_end_matches(['\r', '\n']).trim()));
        if trimmed.is_empty() && next_starts_funding_block {
            continue;
        }
        out.push_str(line_with_ending);
    }
    FilterResult::Text(out)
}

fn is_npm_noise_line(line: &str) -> bool {
    let trimmed = line.trim();
    let lower = trimmed.to_ascii_lowercase();
    trimmed.starts_with("npm http fetch ")
        || trimmed.starts_with("Progress: resolved ")
        || lower.starts_with("npm notice")
        || is_npm_resolved_line(trimmed)
        || is_npm_reused_line(trimmed)
        || is_npm_funding_line(trimmed)
        || lower == "found 0 vulnerabilities"
}

fn is_npm_resolved_line(line: &str) -> bool {
    line.strip_prefix("resolved ").is_some_and(|rest| {
        rest.split_once(' ').is_some_and(|(count, detail)| {
            count.parse::<u64>().is_ok()
                && detail
                    .strip_prefix("packages in ")
                    .is_some_and(is_pytest_duration)
        })
    })
}

fn is_npm_reused_line(line: &str) -> bool {
    line.strip_prefix("reused ").is_some_and(|rest| {
        rest.split_once(',').is_some_and(|(count, detail)| {
            count.trim().parse::<u64>().is_ok()
                && detail.contains("downloaded ")
                && detail.contains("added ")
        })
    })
}

fn is_npm_funding_line(line: &str) -> bool {
    let Some((count, rest)) = line.split_once(' ') else {
        return false;
    };
    count.parse::<u64>().is_ok()
        && matches!(
            rest,
            "package is looking for funding" | "packages are looking for funding"
        )
}

pub(crate) fn npx(invocation: &Invocation, input: &str) -> FilterResult {
    if invocation.args.is_empty() {
        return unsupported();
    }
    if has_any_flag(&invocation.args, &["--json"])
        || invocation
            .args
            .iter()
            .any(|arg| arg == "-c" || arg == "--call")
    {
        return machine();
    }

    let lines: Vec<&str> = input.lines().collect();
    let Some(header) = lines.iter().position(|line| {
        line.trim_start()
            .starts_with("Need to install the following packages")
    }) else {
        return ambiguous();
    };
    let Some(prompt) = lines
        .iter()
        .skip(header + 1)
        .position(|line| line.trim_start().starts_with("Ok to proceed? (y)"))
        .map(|offset| header + 1 + offset)
    else {
        return ambiguous();
    };

    let mut out = String::new();
    let mut tool_started = false;
    for (index, line) in lines.iter().enumerate() {
        if index == header || index == prompt {
            continue;
        }
        // npm's first-run warning/notice chatter sits immediately after the
        // prompt. Once tool output starts, preserve every later line verbatim.
        if index > prompt && !tool_started {
            if line.trim_start().starts_with("npm warn")
                || line.trim_start().starts_with("npm notice")
                || line.trim().is_empty()
            {
                continue;
            }
            tool_started = true;
        }
        push_line(&mut out, line);
    }
    FilterResult::Text(out)
}

pub(crate) fn pytest(invocation: &Invocation, input: &str, exit_code: i32) -> FilterResult {
    if has_any_flag(
        &invocation.args,
        &["--json-report", "--junitxml", "--junit-xml", "--result-log"],
    ) {
        return machine();
    }
    let has_summary = input.lines().any(|line| is_pytest_summary(line.trim()));
    if !has_summary {
        return ambiguous();
    }

    let mut out = String::new();
    let mut in_failure = false;
    for line in input.lines() {
        let trimmed = line.trim();
        if is_pytest_summary(trimmed)
            && (!in_failure || (trimmed.starts_with('=') && trimmed.ends_with('=')))
        {
            in_failure = false;
            out.push_str("pytest: ");
            out.push_str(trimmed.trim_matches('=').trim());
            out.push('\n');
            continue;
        }
        if trimmed.contains("short test summary info") || trimmed.contains("warnings summary") {
            in_failure = false;
            continue;
        }
        if is_pytest_failure_banner(trimmed) || is_pytest_error_banner(trimmed) {
            in_failure = true;
            push_line(&mut out, line);
            continue;
        }
        if is_pytest_named_section(trimmed) {
            in_failure = trimmed.contains("FAILURES") || trimmed.contains("ERRORS");
            continue;
        }
        if is_pytest_section_delimiter(trimmed) {
            in_failure = false;
            continue;
        }
        if trimmed.starts_with("FAILED ")
            || trimmed.starts_with("ERROR ")
            || trimmed.starts_with("XFAIL ")
            || trimmed.starts_with("XPASS ")
        {
            push_line(&mut out, line);
            continue;
        }
        if in_failure {
            push_line(&mut out, line);
            continue;
        }
        if is_pytest_noise(trimmed) {
            continue;
        }
        // Preserve custom reporter and captured-output lines instead of trying
        // to infer whether they are important.
        if !trimmed.is_empty() || exit_code != 0 {
            push_line(&mut out, line);
        }
    }
    FilterResult::Text(out)
}

fn is_pytest_summary(line: &str) -> bool {
    let compact = line.trim_matches('=').trim();
    let Some((summary, duration)) = compact.rsplit_once(" in ") else {
        return false;
    };
    if !is_pytest_duration(duration) {
        return false;
    }
    summary
        .split(',')
        .all(|part| looks_like_pytest_summary_part(part.trim()))
}

fn is_pytest_duration(duration: &str) -> bool {
    let Some(number) = duration.strip_suffix('s') else {
        return false;
    };
    if let Some((whole, fraction)) = number.split_once('.') {
        !whole.is_empty()
            && !fraction.is_empty()
            && whole.bytes().all(|byte| byte.is_ascii_digit())
            && fraction.bytes().all(|byte| byte.is_ascii_digit())
            && !fraction.contains('.')
    } else {
        !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
    }
}

fn looks_like_pytest_summary_part(part: &str) -> bool {
    if part == "no tests ran" {
        return true;
    }
    let Some((count, status)) = part.split_once(' ') else {
        return false;
    };
    count.parse::<u64>().is_ok()
        && matches!(
            status,
            "passed"
                | "failed"
                | "errors"
                | "error"
                | "skipped"
                | "warnings"
                | "warning"
                | "xfailed"
                | "xpassed"
        )
}

fn is_pytest_failure_banner(line: &str) -> bool {
    line.starts_with('_')
        && line.ends_with('_')
        && line.contains("test")
        && !is_pytest_error_banner(line)
}

fn is_pytest_error_banner(line: &str) -> bool {
    line.starts_with('_')
        && line.ends_with('_')
        && line.trim_matches('_').trim().starts_with("ERROR ")
}

fn is_pytest_named_section(line: &str) -> bool {
    line.starts_with('=')
        && line.ends_with('=')
        && [" FAILURES ", " ERRORS "]
            .iter()
            .any(|name| line.contains(name))
}

fn is_pytest_section_delimiter(line: &str) -> bool {
    line.len() >= 6
        && line
            .chars()
            .all(|character| matches!(character, '_' | '=' | '-'))
}

fn is_pytest_noise(line: &str) -> bool {
    line.is_empty()
        || line.contains("test session starts")
        || line.starts_with("collecting ")
        || line.starts_with("collected ")
        || line.starts_with("rootdir:")
        || line.starts_with("configfile:")
        || line.starts_with("plugins:")
        || line.starts_with("platform ")
        || line.starts_with("cachedir:")
        || is_pytest_verbose_result(line)
        || is_pytest_progress(line)
        || line
            .chars()
            .all(|character| matches!(character, '.' | 's' | 'S' | 'x' | 'X' | 'f' | 'F' | 'E'))
}

fn is_pytest_verbose_result(line: &str) -> bool {
    line.contains("::")
        && line
            .split_whitespace()
            .any(|part| matches!(part, "PASSED" | "SKIPPED" | "XFAIL" | "XPASS"))
        && !line.starts_with("XFAIL ")
        && !line.starts_with("XPASS ")
}

fn is_pytest_progress(line: &str) -> bool {
    let Some((path, statuses)) = line.split_once(char::is_whitespace) else {
        return false;
    };
    path.ends_with(".py")
        && statuses
            .trim()
            .chars()
            .all(|character| matches!(character, '.' | 's' | 'S' | 'x' | 'X' | 'f' | 'F' | 'E'))
}

fn has_any_flag(args: &[String], flags: &[&str]) -> bool {
    args.iter().any(|arg| {
        flags.iter().any(|flag| {
            arg == flag
                || arg
                    .strip_prefix(flag)
                    .is_some_and(|rest| rest.starts_with('='))
        })
    })
}

fn command_path(args: &[String], first: &str, second: &str) -> bool {
    matches!(args, [actual_first, actual_second, ..] if actual_first == first && actual_second == second)
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

fn unsupported() -> FilterResult {
    FilterResult::Passthrough(PassthroughReason::UnsupportedInvocation)
}

fn machine() -> FilterResult {
    FilterResult::Passthrough(PassthroughReason::MachineReadableMode)
}

fn ambiguous() -> FilterResult {
    FilterResult::Passthrough(PassthroughReason::AmbiguousOutput)
}
