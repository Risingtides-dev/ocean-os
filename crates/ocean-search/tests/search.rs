//! Public black-box tests adapted from the pinned Oh My Pi grep donor at
//! `03c48d073bd4849726cc14750b5aecfa310bdf26`.
//! Deliberately excluded donor policies: N-API/TypeScript, PCRE2, regex repair,
//! automatic multiline, prefix search/reopen of oversized files, and timing claims.

use ocean_search::{
    search_bytes, search_bytes_with_heartbeat, search_path, search_path_with_heartbeat,
    ContentMatch, Interruption, MemoryOutput, NativeTypeFilter, OutputMode, PathOptions,
    PathOutput, PatternInterpretation, PatternMode, SearchError, SearchOptions,
};
use std::{
    ffi::OsString,
    fs,
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
};
use tempfile::TempDir;

fn options(pattern: &str) -> SearchOptions {
    SearchOptions {
        pattern: pattern.into(),
        ..SearchOptions::default()
    }
}

fn content(result: ocean_search::MemorySearchResult) -> Vec<ContentMatch> {
    match result.output {
        MemoryOutput::Content(rows) => rows,
        other => panic!("expected content, got {other:?}"),
    }
}

fn write(root: &Path, relative: &str, bytes: &[u8]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

#[test]
fn regex_literal_and_fallback_provenance_and_case() {
    let regex = search_bytes(b"abc\nABC\na.c\n", &options("a.c")).unwrap();
    assert_eq!(regex.interpretation, PatternInterpretation::Regex);
    assert_eq!(content(regex).len(), 2);

    let mut literal = options("a.c");
    literal.pattern_mode = PatternMode::Literal;
    assert_eq!(
        content(search_bytes(b"abc\na.c\n", &literal).unwrap()).len(),
        1
    );

    let mut fallback = options("(");
    fallback.pattern_mode = PatternMode::RegexOrLiteral;
    let result = search_bytes(b"left(right\n", &fallback).unwrap();
    assert_eq!(
        result.interpretation,
        PatternInterpretation::LiteralFallback
    );
    assert_eq!(content(result).len(), 1);

    let error = search_bytes(b"anything", &options("(")).unwrap_err();
    assert!(matches!(error, SearchError::Regex { .. }));

    let mut insensitive = options("abc");
    insensitive.ignore_case = true;
    assert_eq!(
        content(search_bytes(b"ABC\n", &insensitive).unwrap()).len(),
        1
    );
}

#[test]
fn multiline_is_explicit_and_cross_line() {
    let plain = options("alpha\\nbeta");
    assert!(matches!(
        search_bytes(b"alpha\nbeta\n", &plain),
        Err(SearchError::Regex { .. })
    ));
    let mut multiline = plain;
    multiline.multiline = true;
    let rows = content(search_bytes(b"alpha\nbeta\n", &multiline).unwrap());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].line.text, "alpha\nbeta");
}

#[test]
fn context_crlf_byte_truncation_and_invalid_utf8_are_bounded() {
    let mut request = options("hit");
    request.context_before = 1;
    request.context_after = 1;
    request.limits.max_line_bytes = 5;
    let rows = content(search_bytes(b"before\r\nhit-\xff-long\r\nafter\r\n", &request).unwrap());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].context_before[0].text, "befor");
    assert_eq!(rows[0].context_after[0].text, "after");
    assert!(rows[0].line.truncation.is_some());
    assert!(rows[0].line.text.is_char_boundary(rows[0].line.text.len()));
    assert!(!rows[0].line.text.ends_with('\r'));
}

#[test]
fn per_file_cap_preserves_after_context_and_reports_incomplete_records() {
    let mut request = options("hit");
    request.context_after = 1;
    request.limits.max_matches_per_file = 1;
    let result = search_bytes(b"hit\nafter\nhit\nlast\n", &request).unwrap();
    assert!(result.summary.limit_reached);
    assert_eq!(
        result.summary.matches_seen, 2,
        "the extra record proves truncation"
    );
    let rows = content(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].context_after.len(), 1);
    assert_eq!(rows[0].context_after[0].text, "after");
}

#[test]
fn count_units_are_matching_records_not_regex_occurrences() {
    let mut request = options("hit");
    request.output_mode = OutputMode::Count;
    let result = search_bytes(b"hit hit\n", &request).unwrap();
    assert_eq!(result.output, MemoryOutput::Count(1));
    assert_eq!(result.summary.matches_seen, 1);
}

#[test]
fn nul_is_binary_in_every_mode() {
    for mode in [
        OutputMode::Content,
        OutputMode::Count,
        OutputMode::FilesWithMatches,
    ] {
        let mut request = options("hit");
        request.output_mode = mode;
        let result = search_bytes(b"hit\0again", &request).unwrap();
        assert_eq!(result.summary.skipped.binary, 1);
        assert_eq!(result.summary.matches_seen, 0);
        match result.output {
            MemoryOutput::Content(rows) => assert!(rows.is_empty()),
            MemoryOutput::Count(count) => assert_eq!(count, 0),
            MemoryOutput::FilesWithMatches(found) => assert!(!found),
        }
    }

    let temp = TempDir::new().unwrap();
    write(temp.path(), "binary", b"hit\0again");
    for mode in [
        OutputMode::Content,
        OutputMode::Count,
        OutputMode::FilesWithMatches,
    ] {
        let mut request = options("hit");
        request.output_mode = mode;
        let result = search_path(&PathOptions::new(temp.path(), request)).unwrap();
        assert_eq!(result.summary.skipped.binary, 1);
        assert_eq!(result.summary.units_returned, 0);
    }
}

#[test]
fn global_offset_limit_and_zero_limit_are_typed() {
    let mut request = options("hit");
    request.offset = 1;
    request.limit = 2;
    let result = search_bytes(b"hit\nhit\nhit\nhit\n", &request).unwrap();
    assert_eq!(content(result).len(), 2);

    request.limit = 0;
    let beats = AtomicUsize::new(0);
    let result = search_bytes_with_heartbeat(b"hit\n", &request, &|| {
        beats.fetch_add(1, Ordering::Relaxed);
        Ok(())
    })
    .unwrap();
    assert!(content(result).is_empty());
    assert!(beats.load(Ordering::Relaxed) >= 2);
}

#[test]
fn memory_input_and_staging_are_bounded_before_output_commit() {
    let mut oversized = options("hit");
    oversized.limits.max_file_bytes = 4;
    assert!(matches!(
        search_bytes(b"hit!!", &oversized),
        Err(SearchError::InvalidRequest(_))
    ));

    let mut no_stage = options("hit");
    no_stage.limits.max_result_text_bytes = 0;
    let result = search_bytes(b"hit\n", &no_stage).unwrap();
    assert!(content(result.clone()).is_empty());
    assert!(result.summary.limit_reached);

    let mut invalid_utf8 = options("a");
    invalid_utf8.limits.max_line_bytes = 5;
    let rows = content(search_bytes(b"\xffabcd\n", &invalid_utf8).unwrap());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].line.text.len(), 5);
    assert!(rows[0].line.truncation.is_some());
}

#[test]
fn path_modes_have_distinct_typed_shapes_and_per_file_diversity() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "a.txt", b"hit\nhit\nhit\n");
    write(temp.path(), "b.txt", b"hit\n");
    let mut request = options("hit");
    request.limits.max_matches_per_file = 1;
    let result = search_path(&PathOptions::new(temp.path(), request.clone())).unwrap();
    assert!(result.summary.limit_reached);
    assert!(result.summary.matches_seen >= 3);
    let PathOutput::Content(rows) = result.output else {
        panic!("content shape")
    };
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].file.native_relative, rows[1].file.native_relative);
    assert!(rows.iter().all(|row| row.file.absolute.is_absolute()));

    request.output_mode = OutputMode::Count;
    let result = search_path(&PathOptions::new(temp.path(), request.clone())).unwrap();
    let PathOutput::Count(rows) = result.output else {
        panic!("count shape")
    };
    assert_eq!(rows.iter().map(|row| row.count).sum::<u64>(), 4);

    request.output_mode = OutputMode::FilesWithMatches;
    let result = search_path(&PathOptions::new(temp.path(), request)).unwrap();
    let PathOutput::FilesWithMatches(rows) = result.output else {
        panic!("files shape")
    };
    assert_eq!(rows.len(), 2);
}

#[test]
fn hidden_git_gitignore_node_modules_and_strict_globs() {
    let temp = TempDir::new().unwrap();
    fs::create_dir(temp.path().join(".git")).unwrap();
    write(temp.path(), ".git/config", b"hit\n");
    write(temp.path(), ".hidden.rs", b"hit\n");
    write(temp.path(), "ignored.rs", b"hit\n");
    write(temp.path(), "nested/good.rs", b"hit\n");
    write(temp.path(), "nested/bad.rs.bak", b"hit\n");
    write(temp.path(), "node_modules/pkg/mod.rs", b"hit\n");
    fs::write(temp.path().join(".gitignore"), "ignored.rs\n").unwrap();

    let mut path = PathOptions::new(temp.path(), options("hit"));
    path.include_hidden = false;
    path.globs = vec!["*.rs".into()];
    let PathOutput::Content(rows) = search_path(&path).unwrap().output else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].file.display_relative, "nested/good.rs");

    path.globs = vec!["node_modules/**/*.rs".into()];
    let PathOutput::Content(rows) = search_path(&path).unwrap().output else {
        panic!()
    };
    assert_eq!(rows.len(), 1);
    assert!(rows[0].file.display_relative.starts_with("node_modules/"));
}

#[test]
fn native_type_filter_uses_exact_native_basename_or_extension() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "Dockerfile", b"hit\n");
    write(temp.path(), "main.rs", b"hit\n");
    write(temp.path(), "main.ts", b"hit\n");
    let mut path = PathOptions::new(temp.path(), options("hit"));
    path.type_filter = Some(NativeTypeFilter::new(
        [OsString::from("rs")],
        [OsString::from("Dockerfile")],
    ));
    let PathOutput::Content(rows) = search_path(&path).unwrap().output else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
}

#[test]
fn explicit_direct_file_roots_ignore_descendant_traversal_policy() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "one.rs", b"hit\n");
    let mut direct = PathOptions::new(temp.path().join("one.rs"), options("hit"));
    direct.globs = vec!["*.rs".into()];
    let PathOutput::Content(rows) = search_path(&direct).unwrap().output else {
        panic!()
    };
    assert_eq!(rows.len(), 1);

    direct.globs = vec!["*.ts".into()];
    let PathOutput::Content(rows) = search_path(&direct).unwrap().output else {
        panic!()
    };
    assert!(rows.is_empty());

    write(temp.path(), ".hidden.rs", b"hit\n");
    let mut hidden = PathOptions::new(temp.path().join(".hidden.rs"), options("hit"));
    hidden.include_hidden = false;
    let PathOutput::Content(rows) = search_path(&hidden).unwrap().output else {
        panic!()
    };
    assert_eq!(
        rows.len(),
        1,
        "the explicit root itself is not a hidden descendant"
    );

    fs::write(temp.path().join(".gitignore"), "ignored.rs\n").unwrap();
    write(temp.path(), "ignored.rs", b"hit\n");
    let ignored = PathOptions::new(temp.path().join("ignored.rs"), options("hit"));
    let PathOutput::Content(rows) = search_path(&ignored).unwrap().output else {
        panic!()
    };
    assert_eq!(
        rows.len(),
        1,
        "gitignore applies below directory roots, not to the explicit root"
    );

    write(temp.path(), "node_modules/direct.rs", b"hit\n");
    let mut node = PathOptions::new(temp.path().join("node_modules/direct.rs"), options("hit"));
    let PathOutput::Content(rows) = search_path(&node).unwrap().output else {
        panic!()
    };
    assert_eq!(
        rows.len(),
        1,
        "node_modules pruning applies below directory roots"
    );
    node.globs = vec!["*.rs".into(), "node_modules/**".into()];
    let PathOutput::Content(rows) = search_path(&node).unwrap().output else {
        panic!()
    };
    assert_eq!(rows.len(), 1);

    let mut zero = options("hit");
    zero.limit = 0;
    zero.limits.max_global_items = 0;
    let result = search_path(&PathOptions::new(temp.path().join("one.rs"), zero)).unwrap();
    assert_eq!(result.summary.candidates, 0);
    assert_eq!(result.summary.files_searched, 0);

    let missing = PathOptions::new(temp.path().join("missing"), options("hit"));
    assert!(matches!(
        search_path(&missing),
        Err(SearchError::Root { .. })
    ));

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(temp.path().join("one.rs"), temp.path().join("link.rs"))
            .unwrap();
        let result = search_path(&PathOptions::new(
            temp.path().join("link.rs"),
            options("hit"),
        ))
        .unwrap();
        assert_eq!(result.summary.skipped.symlinks, 1);
    }
}

#[cfg(unix)]
#[test]
fn fifo_root_is_skipped_without_blocking_open() {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("pipe");
    let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
    let result = search_path(&PathOptions::new(&path, options("hit"))).unwrap();
    assert_eq!(result.summary.skipped.special, 1);
}

#[test]
fn oversized_files_are_skipped_and_read_bound_is_cap_plus_one() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "big.txt", b"hit-xxxxxxxxxxxxxxxx");
    let mut request = options("hit");
    request.limits.max_file_bytes = 8;
    let result = search_path(&PathOptions::new(temp.path(), request)).unwrap();
    assert_eq!(result.summary.skipped.oversized, 1);
    assert_eq!(result.summary.files_searched, 0);
}

#[test]
fn cancellation_heartbeat_precedes_invalid_validation() {
    let mut invalid = options("hit");
    invalid.limits.max_file_bytes = usize::MAX;
    let error = search_bytes_with_heartbeat(b"", &invalid, &|| {
        Err(Interruption("cancel-before-validation".into()))
    })
    .unwrap_err();
    assert!(matches!(error, SearchError::Interrupted(_)));
}

#[test]
fn cancellation_wins_at_entry_empty_single_directory_and_commit() {
    let request = options("hit");
    let error = search_bytes_with_heartbeat(b"", &request, &|| Err(Interruption("stop".into())))
        .unwrap_err();
    assert!(matches!(error, SearchError::Interrupted(_)));

    let temp = TempDir::new().unwrap();
    write(temp.path(), "a", b"hit\n");
    write(temp.path(), "b", b"hit\n");
    let beats = AtomicUsize::new(0);
    let error = search_path_with_heartbeat(&PathOptions::new(temp.path(), request), &|| {
        let beat = beats.fetch_add(1, Ordering::SeqCst);
        if beat >= 5 {
            Err(Interruption("mid-operation".into()))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(matches!(error, SearchError::Interrupted(_)));
}

#[test]
fn cancellation_is_checked_between_direct_file_read_chunks() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "large", &vec![b'x'; 1024 * 1024]);
    let mut request = options("never-matches");
    request.limits.max_file_bytes = 2 * 1024 * 1024;
    let beats = AtomicUsize::new(0);
    let error = search_path_with_heartbeat(
        &PathOptions::new(temp.path().join("large"), request),
        &|| {
            let beat = beats.fetch_add(1, Ordering::SeqCst);
            if beat >= 12 {
                Err(Interruption("mid-read".into()))
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();
    assert!(matches!(error, SearchError::Interrupted(_)));
    assert!(beats.load(Ordering::SeqCst) > 12);
}

#[test]
fn deterministic_path_order_and_parallel_window_parity() {
    let temp = TempDir::new().unwrap();
    for index in (0..300).rev() {
        let name = format!("{index:03}");
        write(temp.path(), &name, format!("hit {name}\n").as_bytes());
    }
    let mut serial = options("hit");
    serial.limit = 300;
    serial.limits.path_window = 1;
    let mut parallel = serial.clone();
    parallel.limits.path_window = 300;
    let left = search_path(&PathOptions::new(temp.path(), serial)).unwrap();
    let right = search_path(&PathOptions::new(temp.path(), parallel)).unwrap();
    assert_eq!(left.output, right.output);
}

#[test]
fn saturated_output_stops_before_admitting_later_windows() {
    let temp = TempDir::new().unwrap();
    for index in 0..10 {
        write(temp.path(), &format!("{index:02}"), b"hit\n");
    }
    let mut request = options("hit");
    request.limit = 1;
    request.limits.path_window = 2;
    let result = search_path(&PathOptions::new(temp.path(), request)).unwrap();
    assert_eq!(result.summary.candidates, 10);
    assert_eq!(
        result.summary.files_searched, 2,
        "only the admitted window may overscan"
    );
    assert_eq!(result.summary.units_returned, 1);
    assert!(result.summary.limit_reached);
}

#[test]
fn cancellation_during_parallel_window_commit_returns_no_partial_success() {
    if ocean_walker::walk_workers() <= 1 {
        return;
    }
    let temp = TempDir::new().unwrap();
    for index in 0..300 {
        write(temp.path(), &format!("{index:03}"), b"hit\n");
    }
    let mut request = options("hit");
    request.limit = 300;
    request.limits.path_window = 300;
    let caller = std::thread::current().id();
    let worker_beats = AtomicUsize::new(0);
    let error = search_path_with_heartbeat(&PathOptions::new(temp.path(), request), &|| {
        if std::thread::current().id() != caller {
            worker_beats.fetch_add(1, Ordering::SeqCst);
            Ok(())
        } else if worker_beats.load(Ordering::SeqCst) > 0 {
            Err(Interruption("cancel-at-ordered-commit".into()))
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(worker_beats.load(Ordering::SeqCst) > 0);
    assert!(matches!(error, SearchError::Interrupted(_)));
}

#[test]
fn invalid_globs_and_resource_bounds_are_typed() {
    let temp = TempDir::new().unwrap();
    let mut path = PathOptions::new(temp.path(), options("hit"));
    path.globs = vec!["[".into()];
    assert!(matches!(search_path(&path), Err(SearchError::Glob { .. })));

    let mut invalid = options("hit");
    invalid.limits.max_file_bytes = usize::MAX;
    assert!(matches!(
        search_bytes(b"", &invalid),
        Err(SearchError::InvalidLimits(_))
    ));

    let mut too_many = options("hit");
    too_many.limit = too_many.limits.max_global_matches + 1;
    assert!(matches!(
        search_bytes(b"", &too_many),
        Err(SearchError::InvalidRequest(_))
    ));

    let mut offset_too_large = options("hit");
    offset_too_large.offset = offset_too_large.limits.max_global_matches;
    offset_too_large.limit = 1;
    assert!(matches!(
        search_bytes(b"", &offset_too_large),
        Err(SearchError::InvalidRequest(_))
    ));

    let nested_pattern = format!("{}a{}", "(".repeat(300), ")".repeat(300));
    assert!(matches!(
        search_bytes(b"a", &options(&nested_pattern)),
        Err(SearchError::Regex { .. })
    ));

    let mut staged = options("hit");
    staged.limits.path_window = 1024;
    staged.limits.max_matches_per_file = 1_000_000;
    assert!(matches!(
        search_bytes(b"", &staged),
        Err(SearchError::InvalidRequest(_))
    ));

    let mut typed = PathOptions::new(temp.path(), options("hit"));
    typed.search.limits.max_globs = 1;
    typed.type_filter = Some(NativeTypeFilter::new(
        [OsString::from("rs"), OsString::from("ts")],
        std::iter::empty::<OsString>(),
    ));
    assert!(matches!(
        search_path(&typed),
        Err(SearchError::InvalidRequest(_))
    ));

    write(temp.path(), "a", b"hit\n");
    write(temp.path(), "b", b"hit\n");
    let mut bounded = options("hit");
    bounded.limit = 1;
    bounded.limits.max_global_items = 1;
    assert!(matches!(
        search_path(&PathOptions::new(temp.path(), bounded)),
        Err(SearchError::InvalidRequest(_))
    ));
}

#[cfg(unix)]
#[test]
fn invalid_byte_display_collisions_preserve_native_identity() {
    use std::os::unix::ffi::OsStringExt;
    let temp = TempDir::new().unwrap();
    let first = OsString::from_vec(vec![b'a', 0x80]);
    let second = OsString::from_vec(vec![b'a', 0x81]);
    if fs::write(temp.path().join(&first), b"hit\n").is_err()
        || fs::write(temp.path().join(&second), b"hit\n").is_err()
    {
        // Some Unix filesystems (notably default macOS volumes) reject invalid UTF-8 names.
        return;
    }
    let PathOutput::Content(rows) = search_path(&PathOptions::new(temp.path(), options("hit")))
        .unwrap()
        .output
    else {
        panic!()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].file.display_relative, rows[1].file.display_relative);
    assert_ne!(rows[0].file.native_relative, rows[1].file.native_relative);
    assert_ne!(rows[0].file.absolute, rows[1].file.absolute);
}
