//! Conservative, standalone command-output minimization.
//!
//! Callers provide an already-tokenized [`Invocation`]. This crate deliberately
//! does not parse shell source, read configuration, persist artifacts, or wire
//! itself into an Ocean runtime. Unsupported invocations and output shapes pass
//! through byte-for-byte.

mod filters;

/// Maximum capture accepted by [`minimize`]: 4 MiB.
pub const MAX_CAPTURE_BYTES: usize = 4 * 1024 * 1024;
/// Maximum logical lines in a changed result, including the omission marker.
pub const FINAL_LINE_CAP: usize = 200;

/// Programs supported by the M1 standalone minimizer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Program {
    Cargo,
    Git,
    Gh,
    Npm,
    Npx,
    Pytest,
}

/// An already-tokenized process invocation.
///
/// `args` excludes the executable name. No shell expansion or command-string
/// parsing is performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: Program,
    pub args: Vec<String>,
}

impl Invocation {
    #[must_use]
    pub fn new<I, S>(program: Program, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program,
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// Minimize one UTF-8 capture using this invocation's tokenized metadata.
    #[must_use]
    pub fn minimize(&self, capture: &str, exit_code: i32) -> Minimization {
        minimize(self, capture, exit_code)
    }
}

/// Why a capture was deliberately returned unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PassthroughReason {
    /// The capture exceeded [`MAX_CAPTURE_BYTES`].
    CaptureTooLarge,
    /// The program is known, but this M1 command shape is out of scope.
    UnsupportedInvocation,
    /// An option or NUL-delimited capture requested a machine-readable shape.
    MachineReadableMode,
    /// The caller explicitly requested raw/live/diff/log output.
    ExplicitRawMode,
    /// The human output did not match the conservative shape recognizer.
    AmbiguousOutput,
    /// The safe filter and final cap made no byte-level change.
    NoChange,
    /// A rewrite was possible but would not reduce byte size.
    NoSavings,
}

/// Whether the visible output changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Disposition {
    Minimized,
    Passthrough(PassthroughReason),
}

/// Exact UTF-8 byte and logical-line counts before and after minimization.
///
/// A non-empty final fragment counts as a line; a trailing `\n` does not create
/// an extra empty line. Thus `""` is zero lines and both `"a"` and `"a\n"`
/// are one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Accounting {
    pub input_bytes: usize,
    pub output_bytes: usize,
    pub input_lines: usize,
    pub output_lines: usize,
}

/// Result of one minimization attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Minimization {
    pub text: String,
    pub disposition: Disposition,
    pub accounting: Accounting,
    /// Present exactly when [`Disposition::Minimized`] is returned.
    pub original_text: Option<String>,
}

/// Minimize a capture for an already-tokenized invocation.
///
/// This function is deterministic and fail-open: all unsupported, explicit
/// machine/raw, oversized, and ambiguous cases preserve the input bytes.
#[must_use]
pub fn minimize(invocation: &Invocation, capture: &str, exit_code: i32) -> Minimization {
    if capture.len() > MAX_CAPTURE_BYTES {
        return passthrough(capture, PassthroughReason::CaptureTooLarge);
    }
    if capture.contains('\0') {
        return passthrough(capture, PassthroughReason::MachineReadableMode);
    }

    let filtered = match invocation.program {
        Program::Cargo => filters::cargo(invocation, capture, exit_code),
        Program::Git => filters::git(invocation, capture),
        Program::Gh => filters::gh(invocation, capture),
        Program::Npm => filters::npm(invocation, capture, exit_code),
        Program::Npx => filters::npx(invocation, capture),
        Program::Pytest => filters::pytest(invocation, capture, exit_code),
    };

    let text = match filtered {
        filters::FilterResult::Text(text) => cap_lines(&text, FINAL_LINE_CAP),
        filters::FilterResult::Passthrough(reason) => return passthrough(capture, reason),
    };

    if text == capture {
        return passthrough(capture, PassthroughReason::NoChange);
    }
    if text.len() >= capture.len() {
        return passthrough(capture, PassthroughReason::NoSavings);
    }

    let accounting = Accounting {
        input_bytes: capture.len(),
        output_bytes: text.len(),
        input_lines: logical_lines(capture),
        output_lines: logical_lines(&text),
    };
    Minimization {
        text,
        disposition: Disposition::Minimized,
        accounting,
        original_text: Some(capture.to_owned()),
    }
}

fn passthrough(capture: &str, reason: PassthroughReason) -> Minimization {
    let bytes = capture.len();
    let lines = logical_lines(capture);
    Minimization {
        text: capture.to_owned(),
        disposition: Disposition::Passthrough(reason),
        accounting: Accounting {
            input_bytes: bytes,
            output_bytes: bytes,
            input_lines: lines,
            output_lines: lines,
        },
        original_text: None,
    }
}

fn logical_lines(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.as_bytes()
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            + usize::from(!text.ends_with('\n'))
    }
}

fn cap_lines(text: &str, cap: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= cap {
        return text.to_owned();
    }

    // Keep more head context while retaining summaries and failures at the tail.
    let head = cap * 3 / 5;
    let tail = cap.saturating_sub(head + 1);
    let omitted = lines.len() - head - tail;
    let mut out = String::new();
    for line in lines.iter().take(head) {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("[…");
    out.push_str(&omitted.to_string());
    out.push_str(" lines elided…]\n");
    for line in lines.iter().skip(lines.len() - tail) {
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_logical_line_accounting() {
        for (text, lines) in [("", 0), ("a", 1), ("a\n", 1), ("a\nb", 2), ("\n", 1)] {
            let output = minimize(&Invocation::new(Program::Git, ["bogus"]), text, 0);
            assert_eq!(output.accounting.input_lines, lines, "{text:?}");
            assert_eq!(output.accounting.output_lines, lines, "{text:?}");
        }
    }

    #[test]
    fn oversized_capture_fails_open_at_strictly_more_than_four_mib() {
        let capture = "x".repeat(MAX_CAPTURE_BYTES + 1);
        let output = minimize(&Invocation::new(Program::Cargo, ["check"]), &capture, 0);
        assert_eq!(
            output.disposition,
            Disposition::Passthrough(PassthroughReason::CaptureTooLarge)
        );
        assert_eq!(output.text, capture);
        assert!(output.original_text.is_none());
    }

    #[test]
    fn fixed_final_cap_has_exactly_two_hundred_lines() {
        let mut capture = String::new();
        for index in 0..300 {
            capture.push_str(&format!("Diff in file{index}.rs\n"));
        }
        let output = minimize(&Invocation::new(Program::Cargo, ["fmt"]), &capture, 1);
        assert_eq!(output.disposition, Disposition::Minimized);
        assert_eq!(output.accounting.output_lines, FINAL_LINE_CAP);
        assert!(output.text.contains("101 lines elided"));
    }
}
