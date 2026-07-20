//! Standalone, bounded, typed byte search over memory or trusted filesystem roots.
//!
//! This crate is deliberately not wired to Ocean's runtime tools. Path search preserves exact
//! native path identity and uses lossy normalized strings only for display and glob matching.
//! It is a trusted-root, path-based engine, not a sandbox: live adoption needs separate
//! descriptor/handle-relative authorization for roots, intermediate components, renames,
//! symlink/reparse swaps, and cached candidates on every supported operating system.
//!
//! Cancellation is cooperative. Heartbeats surround traversal, open, bounded read chunks,
//! matching callbacks, ordered commit, and successful return, but cannot interrupt a blocking
//! filesystem syscall or a matcher call already in progress.

use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{Searcher, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch};
use ocean_walker::{
    CompiledWalkGlob, DirectoryErrorMode, FileCandidate, FollowLinks, SizeHintPolicy, WalkDetail,
    WalkFilter, WalkOrder, WalkRequest,
};
use std::{
    error::Error,
    ffi::OsString,
    fmt,
    fs::{File, OpenOptions},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Mutex,
};

const READ_CHUNK_BYTES: usize = 64 * 1024;
const REGEX_PROGRAM_BYTES: usize = 8 * 1024 * 1024;
const REGEX_DFA_BYTES_PER_THREAD: usize = 2 * 1024 * 1024;
const REGEX_NEST_LIMIT: u32 = 250;
const ABSOLUTE_PATTERN_BYTES: usize = 1024 * 1024;
const ABSOLUTE_GLOB_BYTES: usize = 256 * 1024;
const ABSOLUTE_GLOB_COUNT: usize = 4096;
const ABSOLUTE_FILE_BYTES: usize = 64 * 1024 * 1024;
const ABSOLUTE_MATCHES: u64 = 10_000_000;
const ABSOLUTE_ITEMS: usize = 1_000_000;
const ABSOLUTE_CONTEXT_LINES: usize = 10_000;
const ABSOLUTE_LINE_BYTES: usize = 4 * 1024 * 1024;
const ABSOLUTE_RESULT_TEXT_BYTES: usize = 256 * 1024 * 1024;
const ABSOLUTE_WINDOW: usize = 1024;
const ABSOLUTE_STAGED_TEXT_LINES: usize = 1_000_000;

/// How the supplied pattern is interpreted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternMode {
    /// Compile as a Rust linear-time regular expression and report compilation errors.
    Regex,
    /// Escape all regular-expression syntax and match literally.
    Literal,
    /// Try a regular expression, then fall back to a literal on compilation failure.
    RegexOrLiteral,
}

/// Actual interpretation used by a compiled pattern.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PatternInterpretation {
    /// The pattern compiled as a regular expression.
    Regex,
    /// Literal mode was explicitly requested.
    Literal,
    /// Regex compilation failed and `RegexOrLiteral` used a literal expression.
    LiteralFallback,
}

/// Typed output shape. Count and file modes never manufacture content rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputMode {
    Content,
    Count,
    FilesWithMatches,
}

/// Validated, finite resource limits. `Default` is suitable for interactive trusted-root use.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchLimits {
    pub max_pattern_bytes: usize,
    pub max_glob_bytes: usize,
    pub max_globs: usize,
    pub max_file_bytes: usize,
    pub max_global_matches: u64,
    pub max_global_items: usize,
    pub max_matches_per_file: usize,
    pub max_reported_files: usize,
    pub max_context_lines: usize,
    pub max_line_bytes: usize,
    pub max_result_text_bytes: usize,
    /// Number of path-ordered candidates admitted and completed before ordered commit.
    pub path_window: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_pattern_bytes: 64 * 1024,
            max_glob_bytes: 16 * 1024,
            max_globs: 128,
            max_file_bytes: 4 * 1024 * 1024,
            max_global_matches: 100_000,
            max_global_items: 10_000,
            max_matches_per_file: 1_000,
            max_reported_files: 10_000,
            max_context_lines: 100,
            max_line_bytes: 64 * 1024,
            max_result_text_bytes: 8 * 1024 * 1024,
            path_window: 64,
        }
    }
}

impl SearchLimits {
    fn validate(&self) -> Result<(), SearchError> {
        let invalid = self.max_pattern_bytes == 0
            || self.max_pattern_bytes > ABSOLUTE_PATTERN_BYTES
            || self.max_glob_bytes == 0
            || self.max_glob_bytes > ABSOLUTE_GLOB_BYTES
            || self.max_globs > ABSOLUTE_GLOB_COUNT
            || self.max_file_bytes == 0
            || self.max_file_bytes > ABSOLUTE_FILE_BYTES
            || self.max_global_matches > ABSOLUTE_MATCHES
            || self.max_global_items > ABSOLUTE_ITEMS
            || self.max_matches_per_file > ABSOLUTE_ITEMS
            || self.max_reported_files > ABSOLUTE_ITEMS
            || self.max_context_lines > ABSOLUTE_CONTEXT_LINES
            || self.max_line_bytes == 0
            || self.max_line_bytes > ABSOLUTE_LINE_BYTES
            || self.max_result_text_bytes > ABSOLUTE_RESULT_TEXT_BYTES
            || self.path_window == 0
            || self.path_window > ABSOLUTE_WINDOW;
        if invalid {
            return Err(SearchError::InvalidLimits(
                "one or more limits are zero where prohibited or exceed the finite engine ceiling"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// Pattern and result-selection options shared by memory and filesystem search.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchOptions {
    pub pattern: String,
    pub pattern_mode: PatternMode,
    pub output_mode: OutputMode,
    pub ignore_case: bool,
    /// Cross-line matching is opt-in; it is never inferred from pattern syntax.
    pub multiline: bool,
    pub context_before: usize,
    pub context_after: usize,
    /// Global unit offset: grep-searcher matching records for content/count, matched files for
    /// files mode. A line with several regex occurrences is one record; a multiline record may
    /// span several lines.
    pub offset: u64,
    /// Finite global matching-record/file limit after offset.
    pub limit: u64,
    pub limits: SearchLimits,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            pattern_mode: PatternMode::Regex,
            output_mode: OutputMode::Content,
            ignore_case: false,
            multiline: false,
            context_before: 0,
            context_after: 0,
            offset: 0,
            limit: 10_000,
            limits: SearchLimits::default(),
        }
    }
}

impl SearchOptions {
    fn validate(&self) -> Result<(), SearchError> {
        self.limits.validate()?;
        if self.pattern.len() > self.limits.max_pattern_bytes {
            return Err(SearchError::InvalidRequest(
                "pattern byte limit exceeded".into(),
            ));
        }
        if self.context_before > self.limits.max_context_lines
            || self.context_after > self.limits.max_context_lines
            || self
                .context_before
                .checked_add(self.context_after)
                .is_none_or(|sum| sum > self.limits.max_context_lines)
        {
            return Err(SearchError::InvalidRequest(
                "context line limit exceeded".into(),
            ));
        }
        if self.limit > self.limits.max_global_matches
            || usize::try_from(self.limit).map_or(true, |v| v > self.limits.max_global_items)
        {
            return Err(SearchError::InvalidRequest(
                "global result limit exceeded".into(),
            ));
        }
        let selected_end = self
            .offset
            .checked_add(self.limit)
            .ok_or_else(|| SearchError::InvalidRequest("offset plus limit overflow".into()))?;
        if selected_end > self.limits.max_global_matches {
            return Err(SearchError::InvalidRequest(
                "offset plus limit exceeds the global matching-record bound".into(),
            ));
        }
        if self.output_mode == OutputMode::Content {
            let lines_per_match = self
                .context_before
                .checked_add(self.context_after)
                .and_then(|lines| lines.checked_add(1))
                .ok_or_else(|| SearchError::InvalidRequest("staged line count overflow".into()))?;
            let staged_lines = self
                .limits
                .path_window
                .checked_mul(self.limits.max_matches_per_file)
                .and_then(|matches| matches.checked_mul(lines_per_match))
                .ok_or_else(|| SearchError::InvalidRequest("staged line count overflow".into()))?;
            if staged_lines > ABSOLUTE_STAGED_TEXT_LINES {
                return Err(SearchError::InvalidRequest(
                    "content limits can stage too many text-line records in one path window".into(),
                ));
            }
        }
        Ok(())
    }

    fn path_stage_budget_per_candidate(&self) -> usize {
        self.limits
            .max_result_text_bytes
            .checked_div(self.limits.path_window)
            .unwrap_or(0)
    }
}

/// Owned native extension/basename filter. Matching is exact and platform-native.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NativeTypeFilter {
    pub extensions: Vec<OsString>,
    pub basenames: Vec<OsString>,
}

impl NativeTypeFilter {
    pub fn new(
        extensions: impl IntoIterator<Item = OsString>,
        basenames: impl IntoIterator<Item = OsString>,
    ) -> Self {
        Self {
            extensions: extensions.into_iter().collect(),
            basenames: basenames.into_iter().collect(),
        }
    }

    fn matches(&self, relative: &Path) -> bool {
        relative
            .file_name()
            .is_some_and(|name| self.basenames.iter().any(|candidate| candidate == name))
            || relative
                .extension()
                .is_some_and(|ext| self.extensions.iter().any(|candidate| candidate == ext))
    }

    fn validate(&self, limits: &SearchLimits) -> Result<(), SearchError> {
        let count = self
            .extensions
            .len()
            .checked_add(self.basenames.len())
            .ok_or_else(|| SearchError::InvalidRequest("type-filter count overflow".into()))?;
        if count > limits.max_globs {
            return Err(SearchError::InvalidRequest(
                "type-filter count limit exceeded".into(),
            ));
        }
        let bytes = self
            .extensions
            .iter()
            .chain(self.basenames.iter())
            .try_fold(0usize, |total, value| {
                total
                    .checked_add(value.as_encoded_bytes().len())
                    .ok_or_else(|| {
                        SearchError::InvalidRequest("type-filter byte count overflow".into())
                    })
            })?;
        if bytes > limits.max_glob_bytes {
            return Err(SearchError::InvalidRequest(
                "type-filter byte limit exceeded".into(),
            ));
        }
        Ok(())
    }
}

/// Filesystem-specific policy. Glob patterns are strict and OR-composed.
///
/// `root` itself is explicit. Hidden/ignore/`.git`/`node_modules` policy applies to
/// descendants of a directory root, not to an explicit file root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathOptions {
    pub root: PathBuf,
    pub search: SearchOptions,
    pub globs: Vec<String>,
    pub type_filter: Option<NativeTypeFilter>,
    pub include_hidden: bool,
    pub use_gitignore: bool,
}

impl PathOptions {
    pub fn new(root: impl Into<PathBuf>, search: SearchOptions) -> Self {
        Self {
            root: root.into(),
            search,
            globs: Vec::new(),
            type_filter: None,
            include_hidden: true,
            use_gitignore: true,
        }
    }
}

/// A cooperative interruption with a caller-controlled diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Interruption(pub String);

impl fmt::Display for Interruption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl Error for Interruption {}

/// Typed search failures. Skippable candidate failures are represented in `SearchSummary`.
#[derive(Debug)]
pub enum SearchError {
    InvalidLimits(String),
    InvalidRequest(String),
    Regex { pattern: String, message: String },
    Glob { pattern: String, message: String },
    Root { path: PathBuf, message: String },
    Walk(String),
    Interrupted(Interruption),
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits(message) => write!(f, "invalid search limits: {message}"),
            Self::InvalidRequest(message) => write!(f, "invalid search request: {message}"),
            Self::Regex { message, .. } => write!(f, "regex error: {message}"),
            Self::Glob { message, .. } => write!(f, "glob error: {message}"),
            Self::Root { path, message } => write!(f, "root {}: {message}", path.display()),
            Self::Walk(message) => write!(f, "walk error: {message}"),
            Self::Interrupted(interruption) => interruption.fmt(f),
        }
    }
}

impl Error for SearchError {}

/// Describes explicit byte truncation of returned lossy UTF-8 text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Truncation {
    pub original_bytes: usize,
    pub returned_bytes: usize,
}

/// One bounded returned line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextLine {
    pub line_number: u64,
    pub text: String,
    pub truncation: Option<Truncation>,
}

/// A content match with optional line context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentMatch {
    pub line: TextLine,
    pub context_before: Vec<TextLine>,
    pub context_after: Vec<TextLine>,
    /// Byte position supplied by the matcher, used for deterministic tie-breaking.
    pub match_position: u64,
}

/// Exact operational path identity plus a display-only projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub absolute: PathBuf,
    pub native_relative: PathBuf,
    pub display_relative: String,
}

/// Content output associated with a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileContentMatch {
    pub file: FileIdentity,
    pub matched: ContentMatch,
}

/// Count output associated with a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileCount {
    pub file: FileIdentity,
    pub count: u64,
}

/// Matched-file output, without content sentinel fields.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MatchedFile {
    pub file: FileIdentity,
}

/// In-memory typed output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MemoryOutput {
    Content(Vec<ContentMatch>),
    Count(u64),
    FilesWithMatches(bool),
}

/// Filesystem typed output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PathOutput {
    Content(Vec<FileContentMatch>),
    Count(Vec<FileCount>),
    FilesWithMatches(Vec<MatchedFile>),
}

/// Explicit skip counters; wide integers avoid wraparound.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SkipCounters {
    pub binary: u64,
    pub oversized: u64,
    pub open_errors: u64,
    pub read_errors: u64,
    pub not_regular: u64,
    pub symlinks: u64,
    pub special: u64,
    pub search_errors: u64,
    pub filtered: u64,
}

impl SkipCounters {
    fn add_assign(&mut self, other: Self) {
        self.binary = self.binary.saturating_add(other.binary);
        self.oversized = self.oversized.saturating_add(other.oversized);
        self.open_errors = self.open_errors.saturating_add(other.open_errors);
        self.read_errors = self.read_errors.saturating_add(other.read_errors);
        self.not_regular = self.not_regular.saturating_add(other.not_regular);
        self.symlinks = self.symlinks.saturating_add(other.symlinks);
        self.special = self.special.saturating_add(other.special);
        self.search_errors = self.search_errors.saturating_add(other.search_errors);
        self.filtered = self.filtered.saturating_add(other.filtered);
    }
}

/// Search accounting. Parallel scheduling may change `overscanned_matches`, never output order.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SearchSummary {
    pub candidates: u64,
    pub files_searched: u64,
    pub files_with_matches: u64,
    /// Observed grep-searcher matching records. This is a lower bound when any collection,
    /// staging, or output limit stops a search early; it is not a regex-occurrence count.
    pub matches_seen: u64,
    pub units_returned: u64,
    pub reported_files: u64,
    pub result_text_bytes: u64,
    pub overscanned_matches: u64,
    pub limit_reached: bool,
    pub skipped: SkipCounters,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemorySearchResult {
    pub interpretation: PatternInterpretation,
    pub output: MemoryOutput,
    pub summary: SearchSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathSearchResult {
    pub interpretation: PatternInterpretation,
    pub output: PathOutput,
    pub summary: SearchSummary,
}

/// Search bytes with no cancellation source.
pub fn search_bytes(
    content: &[u8],
    options: &SearchOptions,
) -> Result<MemorySearchResult, SearchError> {
    search_bytes_with_heartbeat(content, options, &|| Ok(()))
}

/// Search bytes with cooperative cancellation.
pub fn search_bytes_with_heartbeat<H>(
    content: &[u8],
    options: &SearchOptions,
    heartbeat: &H,
) -> Result<MemorySearchResult, SearchError>
where
    H: Fn() -> Result<(), Interruption> + Sync,
{
    beat(heartbeat)?;
    options.validate()?;
    if content.len() > options.limits.max_file_bytes {
        return Err(SearchError::InvalidRequest(
            "in-memory content exceeds max_file_bytes".into(),
        ));
    }
    let (matcher, interpretation) = compile_matcher(options)?;
    let mut summary = SearchSummary {
        candidates: 1,
        ..SearchSummary::default()
    };
    let file = search_loaded(
        content,
        options,
        &matcher,
        options.limits.max_result_text_bytes,
        heartbeat,
    )?;
    summary.skipped.add_assign(file.skipped);
    summary.limit_reached = file.limited;
    if file.searched {
        summary.files_searched = 1;
    }
    if file.match_count > 0 {
        summary.files_with_matches = 1;
    }
    summary.matches_seen = file.match_count;
    let output = commit_memory(file, options, &mut summary, heartbeat)?;
    beat(heartbeat)?;
    Ok(MemorySearchResult {
        interpretation,
        output,
        summary,
    })
}

/// Search a file or directory with no cancellation source.
pub fn search_path(options: &PathOptions) -> Result<PathSearchResult, SearchError> {
    search_path_with_heartbeat(options, &|| Ok(()))
}

/// Search one trusted file or directory. Candidates are ordered by display projection, then exact
/// native relative path, processed in fixed windows through ocean-walker's centralized helper,
/// and committed by candidate ordinal.
pub fn search_path_with_heartbeat<H>(
    options: &PathOptions,
    heartbeat: &H,
) -> Result<PathSearchResult, SearchError>
where
    H: Fn() -> Result<(), Interruption> + Sync,
{
    beat(heartbeat)?;
    options.search.validate()?;
    if let Some(filter) = &options.type_filter {
        filter.validate(&options.search.limits)?;
    }
    let compiled_globs = compile_globs(options)?;
    let (matcher, interpretation) = compile_matcher(&options.search)?;
    let root = absolute_path(&options.root)?;
    let mut summary = SearchSummary::default();
    let mut candidates = collect_candidates(
        options,
        &root,
        compiled_globs.as_ref(),
        heartbeat,
        &mut summary,
    )?;
    candidates.sort_by(|left, right| {
        left.identity
            .display_relative
            .cmp(&right.identity.display_relative)
            .then_with(|| {
                left.identity
                    .native_relative
                    .cmp(&right.identity.native_relative)
            })
    });
    summary.candidates = u64::try_from(candidates.len()).unwrap_or(u64::MAX);

    let mut output = empty_path_output(options.search.output_mode);
    let window_size = options.search.limits.path_window;
    let stage_budget = options.search.path_stage_budget_per_candidate();
    for (window_index, window) in candidates.chunks(window_size).enumerate() {
        beat(heartbeat)?;
        let slots = Mutex::new(Vec::<(usize, FileWork)>::with_capacity(window.len()));
        let walker_candidates = window.iter().map(Candidate::as_walker).collect::<Vec<_>>();
        ocean_walker::execute_candidates(&walker_candidates, |walker_candidate| {
            let ordinal = window
                .iter()
                .position(|candidate| candidate.identity.absolute == walker_candidate.path)
                .expect("window candidate identity is preserved");
            let work = search_candidate(
                &window[ordinal],
                &options.search,
                &matcher,
                stage_budget,
                heartbeat,
            )?;
            slots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((ordinal, work));
            Ok::<(), SearchError>(())
        })?;
        let mut completed = slots
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        completed.sort_by_key(|(ordinal, _)| *ordinal);
        for (ordinal, work) in completed {
            beat(heartbeat)?;
            commit_file(
                &window[ordinal].identity,
                work,
                &options.search,
                &mut output,
                &mut summary,
                heartbeat,
            )?;
        }
        let admitted = window_index
            .saturating_add(1)
            .saturating_mul(window_size)
            .min(candidates.len());
        if path_output_saturated(&options.search, &summary) && admitted < candidates.len() {
            summary.limit_reached = true;
            break;
        }
    }
    beat(heartbeat)?;
    Ok(PathSearchResult {
        interpretation,
        output,
        summary,
    })
}

fn beat<H>(heartbeat: &H) -> Result<(), SearchError>
where
    H: Fn() -> Result<(), Interruption> + Sync,
{
    heartbeat().map_err(SearchError::Interrupted)
}

fn compile_matcher(
    options: &SearchOptions,
) -> Result<(RegexMatcher, PatternInterpretation), SearchError> {
    let build = |pattern: &str| {
        let mut builder = RegexMatcherBuilder::new();
        builder
            .case_insensitive(options.ignore_case)
            .multi_line(options.multiline)
            .size_limit(REGEX_PROGRAM_BYTES)
            .dfa_size_limit(REGEX_DFA_BYTES_PER_THREAD)
            .nest_limit(REGEX_NEST_LIMIT);
        if !options.multiline {
            builder.line_terminator(Some(b'\n'));
        }
        builder.build(pattern)
    };
    match options.pattern_mode {
        PatternMode::Literal => build(&regex::escape(&options.pattern))
            .map(|matcher| (matcher, PatternInterpretation::Literal))
            .map_err(|error| SearchError::Regex {
                pattern: options.pattern.clone(),
                message: error.to_string(),
            }),
        PatternMode::Regex => build(&options.pattern)
            .map(|matcher| (matcher, PatternInterpretation::Regex))
            .map_err(|error| SearchError::Regex {
                pattern: options.pattern.clone(),
                message: error.to_string(),
            }),
        PatternMode::RegexOrLiteral => match build(&options.pattern) {
            Ok(matcher) => Ok((matcher, PatternInterpretation::Regex)),
            Err(_) => build(&regex::escape(&options.pattern))
                .map(|matcher| (matcher, PatternInterpretation::LiteralFallback))
                .map_err(|error| SearchError::Regex {
                    pattern: options.pattern.clone(),
                    message: error.to_string(),
                }),
        },
    }
}

fn compile_globs(options: &PathOptions) -> Result<Option<CompiledWalkGlob>, SearchError> {
    if options.globs.len() > options.search.limits.max_globs {
        return Err(SearchError::InvalidRequest(
            "glob count limit exceeded".into(),
        ));
    }
    let total = options.globs.iter().try_fold(0usize, |sum, glob| {
        sum.checked_add(glob.len())
            .ok_or_else(|| SearchError::InvalidRequest("glob byte count overflow".into()))
    })?;
    if total > options.search.limits.max_glob_bytes {
        return Err(SearchError::InvalidRequest(
            "glob byte limit exceeded".into(),
        ));
    }
    if options.globs.is_empty() {
        return Ok(None);
    }
    let normalized: Vec<String> = options
        .globs
        .iter()
        .map(|glob| {
            if glob.contains('/') || glob.contains('\\') {
                glob.replace('\\', "/")
            } else {
                format!("**/{glob}")
            }
        })
        .collect();
    CompiledWalkGlob::new(normalized)
        .map(Some)
        .map_err(|error| SearchError::Glob {
            pattern: options.globs.join(","),
            message: error.to_string(),
        })
}

fn absolute_path(path: &Path) -> Result<PathBuf, SearchError> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| SearchError::Root {
                path: path.to_path_buf(),
                message: error.to_string(),
            })
    }
}

#[derive(Clone)]
struct Candidate {
    identity: FileIdentity,
}

impl Candidate {
    fn as_walker(&self) -> FileCandidate {
        FileCandidate {
            path: self.identity.absolute.clone(),
            display_relative: self.identity.display_relative.clone(),
            mtime: None,
            size: None,
        }
    }
}

fn collect_candidates<H>(
    options: &PathOptions,
    root: &Path,
    globs: Option<&CompiledWalkGlob>,
    heartbeat: &H,
    summary: &mut SearchSummary,
) -> Result<Vec<Candidate>, SearchError>
where
    H: Fn() -> Result<(), Interruption> + Sync,
{
    beat(heartbeat)?;
    let metadata = std::fs::symlink_metadata(root).map_err(|error| SearchError::Root {
        path: root.to_path_buf(),
        message: error.to_string(),
    })?;
    if metadata.file_type().is_symlink() {
        summary.skipped.symlinks = 1;
        return Ok(Vec::new());
    }
    if !metadata.is_file() && !metadata.is_dir() {
        summary.skipped.special = 1;
        return Ok(Vec::new());
    }
    if options.search.limit == 0 || options.search.limits.max_global_items == 0 {
        beat(heartbeat)?;
        return Ok(Vec::new());
    }
    if metadata.is_file() {
        let native_relative = root.file_name().map(PathBuf::from).unwrap_or_default();
        let display_relative = display_path(&native_relative);
        let glob_allowed = globs.is_none_or(|compiled| compiled.is_match(&display_relative));
        let type_allowed = options
            .type_filter
            .as_ref()
            .is_none_or(|filter| filter.matches(&native_relative));
        if glob_allowed && type_allowed {
            return Ok(vec![Candidate {
                identity: FileIdentity {
                    absolute: root.to_path_buf(),
                    native_relative,
                    display_relative,
                },
            }]);
        }
        summary.skipped.filtered = 1;
        return Ok(Vec::new());
    }
    let mut filter = WalkFilter::files_only()
        .node_modules_unless_mentioned(mentions_node_modules(&options.globs));
    if let Some(glob) = globs {
        filter = filter.glob(glob.clone());
    }
    let request = WalkRequest::new(root)
        .hidden(options.include_hidden)
        .gitignore(options.use_gitignore)
        .skip_git(true)
        .skip_node_modules(false)
        .follow_links(FollowLinks::Never)
        .detail(WalkDetail::Minimal)
        .size_hints(SizeHintPolicy::Never)
        .order(WalkOrder::Path)
        .emit_root(false)
        .depth(1, usize::MAX)
        .directory_errors(DirectoryErrorMode::SkipSkippable)
        .cache(false)
        .filter(filter);
    let mut candidates = Vec::new();
    let mut exceeded = false;
    request
        .for_each_entry_with_heartbeat(
            heartbeat,
            |entry| {
                let absolute = entry.absolute_path.into_owned();
                let native_relative = absolute
                    .strip_prefix(root)
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|_| absolute.clone());
                let type_allowed = options
                    .type_filter
                    .as_ref()
                    .is_none_or(|filter| filter.matches(&native_relative));
                if !type_allowed {
                    summary.skipped.filtered = summary.skipped.filtered.saturating_add(1);
                    return Ok(ocean_walker::WalkDecision::Include);
                }
                if candidates.len() >= options.search.limits.max_global_items {
                    exceeded = true;
                    return Ok(ocean_walker::WalkDecision::Stop);
                }
                candidates.push(Candidate {
                    identity: FileIdentity {
                        absolute,
                        native_relative,
                        display_relative: entry.display_relative_path.to_string(),
                    },
                });
                Ok(ocean_walker::WalkDecision::Include)
            },
            |_| Ok(ocean_walker::WalkDecision::Include),
        )
        .map_err(|error| match error {
            ocean_walker::WalkError::Interrupted(interruption) => {
                SearchError::Interrupted(interruption)
            }
            other => SearchError::Walk(other.to_string()),
        })?;
    if exceeded {
        return Err(SearchError::InvalidRequest(
            "candidate count exceeds max_global_items".into(),
        ));
    }
    Ok(candidates)
}

fn mentions_node_modules(globs: &[String]) -> bool {
    globs.iter().any(|glob| {
        glob.replace('\\', "/")
            .split('/')
            .any(|component| component == "node_modules")
    })
}

fn display_path(path: &Path) -> String {
    let display = path.to_string_lossy();
    if cfg!(windows) {
        display.replace('\\', "/")
    } else {
        display.into_owned()
    }
}

#[derive(Default)]
struct FileWork {
    searched: bool,
    matches: Vec<ContentMatch>,
    match_count: u64,
    limited: bool,
    skipped: SkipCounters,
}

fn search_candidate<H>(
    candidate: &Candidate,
    options: &SearchOptions,
    matcher: &RegexMatcher,
    stage_budget: usize,
    heartbeat: &H,
) -> Result<FileWork, SearchError>
where
    H: Fn() -> Result<(), Interruption> + Sync,
{
    beat(heartbeat)?;
    let mut file = match open_leaf(&candidate.identity.absolute) {
        Ok(file) => file,
        Err(OpenFailure::Symlink) => {
            return Ok(FileWork {
                skipped: SkipCounters {
                    symlinks: 1,
                    ..SkipCounters::default()
                },
                ..FileWork::default()
            });
        }
        Err(OpenFailure::NotRegular) => {
            return Ok(FileWork {
                skipped: SkipCounters {
                    not_regular: 1,
                    ..SkipCounters::default()
                },
                ..FileWork::default()
            });
        }
        Err(OpenFailure::Io) => {
            return Ok(FileWork {
                skipped: SkipCounters {
                    open_errors: 1,
                    ..SkipCounters::default()
                },
                ..FileWork::default()
            });
        }
    };
    let read_limit = options
        .limits
        .max_file_bytes
        .checked_add(1)
        .ok_or_else(|| {
            SearchError::InvalidLimits("file cap plus classifier byte overflow".into())
        })?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    while bytes.len() < read_limit {
        beat(heartbeat)?;
        let remaining = read_limit - bytes.len();
        let count = match file.read(&mut chunk[..remaining.min(READ_CHUNK_BYTES)]) {
            Ok(count) => count,
            Err(_) => {
                return Ok(FileWork {
                    skipped: SkipCounters {
                        read_errors: 1,
                        ..SkipCounters::default()
                    },
                    ..FileWork::default()
                });
            }
        };
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    beat(heartbeat)?;
    if bytes.len() > options.limits.max_file_bytes {
        return Ok(FileWork {
            skipped: SkipCounters {
                oversized: 1,
                ..SkipCounters::default()
            },
            ..FileWork::default()
        });
    }
    search_loaded(&bytes, options, matcher, stage_budget, heartbeat)
}

#[derive(Clone, Copy)]
enum OpenFailure {
    Symlink,
    NotRegular,
    Io,
}

fn open_leaf(path: &Path) -> Result<File, OpenFailure> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .map_err(|error| {
                if error.raw_os_error() == Some(libc::ELOOP) {
                    OpenFailure::Symlink
                } else {
                    OpenFailure::Io
                }
            })?
    };
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)
            .map_err(|_| OpenFailure::Io)?
    };
    #[cfg(not(any(unix, windows)))]
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| OpenFailure::Io)?;

    let metadata = file.metadata().map_err(|_| OpenFailure::Io)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(OpenFailure::Symlink);
        }
    }
    if !metadata.is_file() {
        return Err(OpenFailure::NotRegular);
    }
    Ok(file)
}

fn search_loaded<H>(
    content: &[u8],
    options: &SearchOptions,
    matcher: &RegexMatcher,
    stage_budget: usize,
    heartbeat: &H,
) -> Result<FileWork, SearchError>
where
    H: Fn() -> Result<(), Interruption> + Sync,
{
    beat(heartbeat)?;
    if content.contains(&0) {
        return Ok(FileWork {
            skipped: SkipCounters {
                binary: 1,
                ..SkipCounters::default()
            },
            ..FileWork::default()
        });
    }
    let collect_content = options.output_mode == OutputMode::Content;
    let mut searcher = SearcherBuilder::new()
        .line_number(collect_content)
        .multi_line(options.multiline)
        .before_context(if collect_content {
            options.context_before
        } else {
            0
        })
        .after_context(if collect_content {
            options.context_after
        } else {
            0
        })
        .build();
    let mut sink = CollectSink {
        heartbeat,
        collect_content,
        stop_after: match options.output_mode {
            OutputMode::Content => options.search_per_file_limit(),
            OutputMode::FilesWithMatches => 1,
            OutputMode::Count => usize::MAX,
        },
        max_line_bytes: options.limits.max_line_bytes,
        stage_budget,
        staged_bytes: 0,
        matches: Vec::new(),
        count: 0,
        before: Vec::new(),
        limited: false,
        interrupted: None,
    };
    if searcher.search_slice(matcher, content, &mut sink).is_err() {
        if let Some(interruption) = sink.interrupted {
            return Err(SearchError::Interrupted(interruption));
        }
        return Ok(FileWork {
            searched: true,
            skipped: SkipCounters {
                search_errors: 1,
                ..SkipCounters::default()
            },
            ..FileWork::default()
        });
    }
    beat(heartbeat)?;
    Ok(FileWork {
        searched: true,
        matches: sink.matches,
        match_count: sink.count,
        limited: sink.limited,
        skipped: SkipCounters::default(),
    })
}

impl SearchOptions {
    fn search_per_file_limit(&self) -> usize {
        self.limits.max_matches_per_file
    }
}

struct CollectSink<'a, H> {
    heartbeat: &'a H,
    collect_content: bool,
    stop_after: usize,
    max_line_bytes: usize,
    stage_budget: usize,
    staged_bytes: usize,
    matches: Vec<ContentMatch>,
    count: u64,
    before: Vec<TextLine>,
    limited: bool,
    interrupted: Option<Interruption>,
}

impl<H> CollectSink<'_, H>
where
    H: Fn() -> Result<(), Interruption> + Sync,
{
    fn heartbeat(&mut self) -> io::Result<()> {
        match (self.heartbeat)() {
            Ok(()) => Ok(()),
            Err(interruption) => {
                self.interrupted = Some(interruption);
                Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "search interrupted",
                ))
            }
        }
    }

    fn stage_line(&mut self, line_number: u64, bytes: &[u8]) -> Option<TextLine> {
        let remaining = self.stage_budget.saturating_sub(self.staged_bytes);
        if remaining == 0 {
            self.limited = true;
            return None;
        }
        let line = text_line(line_number, bytes, self.max_line_bytes.min(remaining));
        self.staged_bytes = self.staged_bytes.saturating_add(line.text.len());
        Some(line)
    }
}

impl<H> Sink for CollectSink<'_, H>
where
    H: Fn() -> Result<(), Interruption> + Sync,
{
    type Error = io::Error;

    fn matched(
        &mut self,
        _searcher: &Searcher,
        matched: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        self.heartbeat()?;
        self.count = self.count.saturating_add(1);
        if !self.collect_content {
            return Ok(if self.stop_after == usize::MAX {
                true
            } else {
                usize::try_from(self.count).is_ok_and(|count| count < self.stop_after)
            });
        }
        if self.matches.len() >= self.stop_after {
            self.before.clear();
            self.limited = true;
            return Ok(false);
        }
        let Some(line) = self.stage_line(matched.line_number().unwrap_or(0), matched.bytes())
        else {
            self.before.clear();
            return Ok(false);
        };
        self.matches.push(ContentMatch {
            line,
            context_before: std::mem::take(&mut self.before),
            context_after: Vec::new(),
            match_position: matched.absolute_byte_offset(),
        });
        // Continue so grep-searcher can deliver trailing context for this match. If this match
        // filled the per-file cap, the next matched record supplies truthful truncation evidence.
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        self.heartbeat()?;
        if !self.collect_content {
            return Ok(true);
        }
        let Some(line) = self.stage_line(context.line_number().unwrap_or(0), context.bytes())
        else {
            return Ok(false);
        };
        match context.kind() {
            SinkContextKind::Before => self.before.push(line),
            SinkContextKind::After => {
                if let Some(last) = self.matches.last_mut() {
                    last.context_after.push(line);
                }
            }
            SinkContextKind::Other => {}
        }
        Ok(true)
    }
}

fn trim_line_ending(mut bytes: &[u8]) -> &[u8] {
    if bytes.last() == Some(&b'\n') {
        bytes = &bytes[..bytes.len() - 1];
        if bytes.last() == Some(&b'\r') {
            bytes = &bytes[..bytes.len() - 1];
        }
    }
    bytes
}

fn text_line(line_number: u64, bytes: &[u8], max_bytes: usize) -> TextLine {
    let bytes = trim_line_ending(bytes);
    let original_bytes = bytes.len();
    // Bound the raw slice before lossy conversion. Invalid UTF-8 can expand by at most 3x, but
    // this prevents an arbitrarily long matching record from being materialized before clipping.
    let raw_prefix_bytes = original_bytes.min(max_bytes);
    let mut text = String::from_utf8_lossy(&bytes[..raw_prefix_bytes]).into_owned();
    let lossy_bytes = text.len();
    if text.len() > max_bytes {
        let mut boundary = max_bytes;
        while boundary > 0 && !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
    }
    let was_truncated = raw_prefix_bytes < original_bytes || text.len() < lossy_bytes;
    let truncation = was_truncated.then_some(Truncation {
        original_bytes,
        returned_bytes: text.len(),
    });
    TextLine {
        line_number,
        text,
        truncation,
    }
}

fn content_text_bytes(matched: &ContentMatch) -> usize {
    matched
        .context_before
        .iter()
        .chain(std::iter::once(&matched.line))
        .chain(matched.context_after.iter())
        .fold(0usize, |sum, line| sum.saturating_add(line.text.len()))
}

fn commit_memory<H>(
    mut work: FileWork,
    options: &SearchOptions,
    summary: &mut SearchSummary,
    heartbeat: &H,
) -> Result<MemoryOutput, SearchError>
where
    H: Fn() -> Result<(), Interruption> + Sync,
{
    beat(heartbeat)?;
    match options.output_mode {
        OutputMode::Content => {
            let mut selected = Vec::new();
            let mut skipped = 0u64;
            for matched in work.matches.drain(..) {
                beat(heartbeat)?;
                if skipped < options.offset {
                    skipped += 1;
                    continue;
                }
                if u64::try_from(selected.len()).unwrap_or(u64::MAX) >= options.limit {
                    summary.limit_reached = true;
                    break;
                }
                let bytes = content_text_bytes(&matched);
                if usize::try_from(summary.result_text_bytes)
                    .unwrap_or(usize::MAX)
                    .saturating_add(bytes)
                    > options.limits.max_result_text_bytes
                {
                    summary.limit_reached = true;
                    break;
                }
                summary.result_text_bytes = summary.result_text_bytes.saturating_add(bytes as u64);
                selected.push(matched);
            }
            summary.units_returned = selected.len() as u64;
            summary.reported_files = u64::from(!selected.is_empty());
            summary.overscanned_matches =
                summary.matches_seen.saturating_sub(summary.units_returned);
            Ok(MemoryOutput::Content(selected))
        }
        OutputMode::Count => {
            let available = work.match_count.saturating_sub(options.offset);
            let count = available.min(options.limit);
            summary.units_returned = count;
            summary.reported_files = u64::from(count > 0);
            summary.limit_reached = count < available;
            Ok(MemoryOutput::Count(count))
        }
        OutputMode::FilesWithMatches => {
            let matched = work.match_count > 0 && options.offset == 0 && options.limit > 0;
            summary.units_returned = u64::from(matched);
            summary.reported_files = u64::from(matched);
            summary.limit_reached = work.match_count > 0 && !matched;
            Ok(MemoryOutput::FilesWithMatches(matched))
        }
    }
}

fn empty_path_output(mode: OutputMode) -> PathOutput {
    match mode {
        OutputMode::Content => PathOutput::Content(Vec::new()),
        OutputMode::Count => PathOutput::Count(Vec::new()),
        OutputMode::FilesWithMatches => PathOutput::FilesWithMatches(Vec::new()),
    }
}

fn path_output_saturated(options: &SearchOptions, summary: &SearchSummary) -> bool {
    summary.units_returned >= options.limit
        || summary.reported_files >= options.limits.max_reported_files as u64
        || (options.output_mode == OutputMode::Content
            && summary.result_text_bytes >= options.limits.max_result_text_bytes as u64)
}

fn commit_file<H>(
    identity: &FileIdentity,
    mut work: FileWork,
    options: &SearchOptions,
    output: &mut PathOutput,
    summary: &mut SearchSummary,
    heartbeat: &H,
) -> Result<(), SearchError>
where
    H: Fn() -> Result<(), Interruption> + Sync,
{
    beat(heartbeat)?;
    summary.limit_reached |= work.limited;
    summary.skipped.add_assign(work.skipped);
    if work.searched {
        summary.files_searched = summary.files_searched.saturating_add(1);
    }
    if work.match_count > 0 {
        summary.files_with_matches = summary.files_with_matches.saturating_add(1);
    }
    summary.matches_seen = summary.matches_seen.saturating_add(work.match_count);
    match output {
        PathOutput::Content(rows) => {
            let mut reported_this_file = false;
            for matched in work.matches.drain(..) {
                beat(heartbeat)?;
                if summary.overscanned_matches < options.offset {
                    summary.overscanned_matches = summary.overscanned_matches.saturating_add(1);
                    continue;
                }
                if summary.units_returned >= options.limit
                    || (!reported_this_file
                        && summary.reported_files >= options.limits.max_reported_files as u64)
                {
                    summary.limit_reached = true;
                    summary.overscanned_matches = summary.overscanned_matches.saturating_add(1);
                    continue;
                }
                let text_bytes = content_text_bytes(&matched);
                let owned = usize::try_from(summary.result_text_bytes).unwrap_or(usize::MAX);
                if owned.saturating_add(text_bytes) > options.limits.max_result_text_bytes {
                    summary.limit_reached = true;
                    summary.overscanned_matches = summary.overscanned_matches.saturating_add(1);
                    continue;
                }
                summary.result_text_bytes =
                    summary.result_text_bytes.saturating_add(text_bytes as u64);
                rows.push(FileContentMatch {
                    file: identity.clone(),
                    matched,
                });
                if !reported_this_file {
                    summary.reported_files = summary.reported_files.saturating_add(1);
                    reported_this_file = true;
                }
                summary.units_returned = summary.units_returned.saturating_add(1);
            }
        }
        PathOutput::Count(rows) => {
            let skipped = options
                .offset
                .saturating_sub(summary.overscanned_matches)
                .min(work.match_count);
            summary.overscanned_matches = summary.overscanned_matches.saturating_add(skipped);
            let available = work.match_count.saturating_sub(skipped);
            let selected = available.min(options.limit.saturating_sub(summary.units_returned));
            if selected > 0 && rows.len() < options.limits.max_reported_files {
                rows.push(FileCount {
                    file: identity.clone(),
                    count: selected,
                });
                summary.reported_files = summary.reported_files.saturating_add(1);
                summary.units_returned = summary.units_returned.saturating_add(selected);
            } else if selected > 0 {
                summary.limit_reached = true;
            }
            if selected < available {
                summary.limit_reached = true;
            }
        }
        PathOutput::FilesWithMatches(rows) => {
            if work.match_count == 0 {
                return Ok(());
            }
            if summary.overscanned_matches < options.offset {
                summary.overscanned_matches = summary.overscanned_matches.saturating_add(1);
            } else if summary.units_returned < options.limit
                && rows.len() < options.limits.max_reported_files
            {
                rows.push(MatchedFile {
                    file: identity.clone(),
                });
                summary.reported_files = summary.reported_files.saturating_add(1);
                summary.units_returned = summary.units_returned.saturating_add(1);
            } else {
                summary.limit_reached = true;
            }
        }
    }
    Ok(())
}
