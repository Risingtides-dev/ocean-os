//! `check-webrtc-cache` — detect a poisoned/partial libwebrtc cache *before*
//! the build fails with the cryptic `could not find native static library
//! `webrtc`` error.
//!
//! ## The poison mechanism (recap of OCEAN-252 / #170)
//!
//! `webrtc-sys-build`'s `download_webrtc()` downloads a prebuilt libwebrtc
//! archive into a persistent *scratch* dir, then guards re-download with:
//!
//! ```text
//! let webrtc_dir = webrtc_dir();
//! if webrtc_dir.exists() { return Ok(()); }
//! ```
//!
//! If the download/extract is interrupted (Ctrl-C, dropped network, OOM kill)
//! *after* `webrtc_dir` is created but *before* `libwebrtc.a` lands in it, the
//! dir exists-but-is-incomplete. The next build sees the dir, early-returns a
//! false "success", never re-fetches — and the linker then fails. OCEAN-252
//! gave us `clear-webrtc-cache` to fix it *after* the fact; the problem was that
//! a human had to recognise the cryptic linker error and run the command.
//!
//! ## What this command does
//!
//! It detects the poison up front, by content rather than by guessing:
//!
//! 1. Walk `target/{debug,release}/build/scratch-*`.
//! 2. A scratch dir is a **libwebrtc cache** iff it contains the marker
//!    `out/livekit_webrtc/livekit/` (that's the `SCRATH_PATH = "livekit_webrtc"`
//!    base that `webrtc-sys-build` extracts into — see its `prebuilt_dir()`).
//!    Other crates also use the `scratch` crate and create `scratch-*` dirs that
//!    have *no* such marker; those are skipped, so we never false-positive.
//! 3. For each cache, find `lib/libwebrtc.a` underneath the marker. The real
//!    archive is ~700 MB; a healthy cache has it present and non-trivially
//!    sized. A cache whose marker dir exists but whose `libwebrtc.a` is
//!    **missing, zero-size, or truncated below the floor** is *poisoned* — that
//!    is exactly the existing-but-incomplete state the build script can't see.
//!
//! On a poisoned cache: report a clear, actionable message and exit non-zero so
//! a CI/preflight step fails loudly. With `--fix`, clear the poisoned artifacts
//! (the scratch dir + the matching `webrtc-sys-*` build and `.fingerprint`
//! dirs) so the next build re-downloads cleanly — self-healing.
//!
//! std-only, no new deps — same constraint as the rest of `xtask`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::paths::{dirs_with_prefix, resolve_target_dir};

/// Profiles whose build artifacts we inspect.
const PROFILES: &[&str] = &["debug", "release"];

/// The marker subpath, relative to a `scratch-*` dir, that identifies it as a
/// libwebrtc download cache. `webrtc-sys-build` extracts into
/// `scratch::path("livekit_webrtc")/livekit/{triple}-{tag}/{triple}/...`, and
/// the `scratch` crate roots that under `<scratch-dir>/out/`. So the presence
/// of `out/livekit_webrtc/livekit` is a reliable, platform-independent tell.
const CACHE_MARKER: &[&str] = &["out", "livekit_webrtc", "livekit"];

/// The static library every healthy libwebrtc cache must contain.
const LIB_NAME: &str = "libwebrtc.a";

/// Minimum plausible size for a real `libwebrtc.a`, in bytes.
///
/// The genuine archive is ~700 MB across platforms; the smallest real builds
/// are still tens of MB. We set a deliberately *low* floor (1 MiB) so we never
/// flag a legitimately-built library, while still catching the poison cases
/// that matter: a missing file, a zero-byte file, or a download truncated to a
/// stub. Anything at or above this is treated as healthy.
const MIN_LIBWEBRTC_BYTES: u64 = 1 << 20; // 1 MiB

/// What a single scratch-dir cache looks like after inspection.
#[derive(Debug, PartialEq, Eq)]
enum Health {
    /// `libwebrtc.a` present and >= the size floor.
    Healthy { lib: PathBuf, size: u64 },
    /// Marker dir present, but no usable `libwebrtc.a` underneath.
    Poisoned(PoisonKind),
}

#[derive(Debug, PartialEq, Eq)]
enum PoisonKind {
    /// The `out/livekit_webrtc/livekit` marker exists but no `libwebrtc.a` was
    /// found anywhere under it — an interrupted extract.
    LibMissing,
    /// `libwebrtc.a` exists but is below the size floor (zero / truncated).
    LibTooSmall { lib: PathBuf, size: u64 },
}

/// A cache we found + classified, paired with the scratch dir it lives in.
struct Finding {
    profile: &'static str,
    scratch_dir: PathBuf,
    health: Health,
}

pub fn run(args: &[String]) -> ExitCode {
    let mut fix = false;
    for a in args {
        match a.as_str() {
            "--fix" => fix = true,
            other => {
                eprintln!("check-webrtc-cache: unknown flag `{other}`");
                eprintln!("  supported: --fix");
                return ExitCode::FAILURE;
            }
        }
    }

    let target_dir = resolve_target_dir();
    println!("Target dir: {}", target_dir.display());

    if !target_dir.exists() {
        println!("OK: no target dir yet — nothing to check (a fresh build will download cleanly).");
        return ExitCode::SUCCESS;
    }

    let findings = scan(&target_dir);

    if findings.is_empty() {
        println!("OK: no libwebrtc cache present yet.");
        println!(
            "(Looked for a `{}` marker under build/scratch-* in debug + release.)",
            CACHE_MARKER.join("/")
        );
        return ExitCode::SUCCESS;
    }

    let mut poisoned: Vec<&Finding> = Vec::new();
    for f in &findings {
        match &f.health {
            Health::Healthy { lib, size } => {
                println!(
                    "OK [{}]: libwebrtc cache healthy — {} ({})",
                    f.profile,
                    lib.display(),
                    human_size(*size)
                );
            }
            Health::Poisoned(kind) => {
                println!("POISONED [{}]: {}", f.profile, f.scratch_dir.display());
                match kind {
                    PoisonKind::LibMissing => {
                        println!("  reason: cache dir exists but `{LIB_NAME}` is missing (interrupted download/extract).");
                    }
                    PoisonKind::LibTooSmall { lib, size } => {
                        println!(
                            "  reason: `{}` is only {} (< {}); the download was truncated.",
                            lib.display(),
                            human_size(*size),
                            human_size(MIN_LIBWEBRTC_BYTES)
                        );
                    }
                }
                poisoned.push(f);
            }
        }
    }

    if poisoned.is_empty() {
        println!("\nAll libwebrtc caches are healthy.");
        return ExitCode::SUCCESS;
    }

    if !fix {
        eprintln!(
            "\n{} poisoned/partial libwebrtc cache(s) detected — the build would fail with",
            poisoned.len()
        );
        eprintln!("  \"could not find native static library `webrtc`\"");
        eprintln!("Run `cargo xtask clear-webrtc-cache` (or `cargo xtask check-webrtc-cache --fix`)");
        eprintln!("to remove the partial cache so the next build re-downloads it cleanly.");
        return ExitCode::FAILURE;
    }

    // --fix: self-heal. Remove the poisoned scratch dir plus the matching
    // webrtc-sys build + fingerprint dirs in the same profile, mirroring what
    // `clear-webrtc-cache` sweeps, so cargo actually re-runs the build script.
    println!("\n--fix: clearing poisoned cache artifacts so the next build re-downloads…");
    let mut removed = 0usize;
    let mut errors: Vec<(PathBuf, std::io::Error)> = Vec::new();

    for f in &poisoned {
        let profile_dir = target_dir.join(f.profile);
        let mut to_remove: Vec<PathBuf> = vec![f.scratch_dir.clone()];
        // Also clear webrtc-sys-* build outputs + fingerprints in this profile;
        // otherwise cargo's fingerprint still says "built" and skips the script.
        to_remove.extend(dirs_with_prefix(&profile_dir.join("build"), "webrtc-sys-"));
        to_remove.extend(dirs_with_prefix(&profile_dir.join(".fingerprint"), "webrtc-sys-"));

        for dir in to_remove {
            match fs::remove_dir_all(&dir) {
                Ok(()) => {
                    println!("  removed {}", dir.display());
                    removed += 1;
                }
                Err(e) => errors.push((dir, e)),
            }
        }
    }

    if !errors.is_empty() {
        eprintln!("\nFailed to remove {} dir(s):", errors.len());
        for (p, e) in &errors {
            eprintln!("  - {}: {e}", p.display());
        }
        return ExitCode::FAILURE;
    }

    println!(
        "\nCleared {removed} artifact dir(s). The next `cargo build -p ocean-call --features livekit-tap` will re-fetch libwebrtc."
    );
    ExitCode::SUCCESS
}

/// Scan all profiles for libwebrtc caches and classify each.
fn scan(target_dir: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();
    for profile in PROFILES {
        let build_dir = target_dir.join(profile).join("build");
        for scratch_dir in dirs_with_prefix(&build_dir, "scratch-") {
            // Only treat this scratch dir as a webrtc cache if it carries the
            // marker. Skips unrelated `scratch-*` dirs from other crates.
            let marker = join_all(&scratch_dir, CACHE_MARKER);
            if !marker.is_dir() {
                continue;
            }
            findings.push(Finding {
                profile,
                health: classify(&marker),
                scratch_dir,
            });
        }
    }
    findings
}

/// Given the cache marker dir (`…/out/livekit_webrtc/livekit`), decide whether
/// the cache is healthy or poisoned by locating `lib/libwebrtc.a` and checking
/// its size.
fn classify(marker: &Path) -> Health {
    match find_libwebrtc(marker) {
        Some(lib) => {
            let size = fs::metadata(&lib).map(|m| m.len()).unwrap_or(0);
            if size >= MIN_LIBWEBRTC_BYTES {
                Health::Healthy { lib, size }
            } else {
                Health::Poisoned(PoisonKind::LibTooSmall { lib, size })
            }
        }
        None => Health::Poisoned(PoisonKind::LibMissing),
    }
}

/// Find a `lib/libwebrtc.a` under `marker`. The real layout is
/// `livekit/{triple}-{tag}/{triple}/lib/libwebrtc.a`, but the triple/tag vary
/// by platform, so we do a bounded recursive search for any `lib/libwebrtc.a`
/// rather than hard-coding the host triple. Returns the first match.
fn find_libwebrtc(marker: &Path) -> Option<PathBuf> {
    // Depth cap: the lib sits ~3 levels below the marker; 6 is generous and
    // keeps a pathological tree from turning this into a full-disk walk.
    find_named(marker, LIB_NAME, 6)
}

/// Bounded depth-first search for a *file* named `name` under `dir`.
fn find_named(dir: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    let entries = fs::read_dir(dir).ok()?;
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_file() {
            if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                return Some(path);
            }
        } else if ft.is_dir() && depth > 0 {
            subdirs.push(path);
        }
    }
    for sub in subdirs {
        if let Some(found) = find_named(&sub, name, depth - 1) {
            return Some(found);
        }
    }
    None
}

/// Join a sequence of path components onto a base.
fn join_all(base: &Path, parts: &[&str]) -> PathBuf {
    let mut p = base.to_path_buf();
    for part in parts {
        p.push(part);
    }
    p
}

/// Human-friendly byte size for messages (e.g. `680.2 MiB`, `0 B`).
fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_tmp(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("xtask-check-test-{tag}-{nanos}-{}", std::process::id()));
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Build a fake webrtc scratch dir under `<root>/<profile>/build/<name>`,
    /// optionally writing a `libwebrtc.a` of `lib_size` bytes (None = no lib,
    /// i.e. the marker exists but the file is absent).
    fn make_cache(root: &Path, profile: &str, name: &str, lib_size: Option<u64>) -> PathBuf {
        let scratch = root.join(profile).join("build").join(name);
        // marker: out/livekit_webrtc/livekit/<triple>-<tag>/<triple>/lib
        let lib_dir = scratch
            .join("out")
            .join("livekit_webrtc")
            .join("livekit")
            .join("mac-arm64-release-webrtc-51ef663")
            .join("mac-arm64-release")
            .join("lib");
        fs::create_dir_all(&lib_dir).unwrap();
        if let Some(n) = lib_size {
            let lib = lib_dir.join(LIB_NAME);
            // Write `n` bytes without allocating a huge buffer.
            let f = fs::File::create(&lib).unwrap();
            f.set_len(n).unwrap();
        }
        scratch
    }

    #[test]
    fn healthy_cache_is_detected() {
        let root = unique_tmp("healthy");
        let scratch = make_cache(&root, "debug", "scratch-aaa", Some(MIN_LIBWEBRTC_BYTES));
        let marker = join_all(&scratch, CACHE_MARKER);
        match classify(&marker) {
            Health::Healthy { size, .. } => assert_eq!(size, MIN_LIBWEBRTC_BYTES),
            other => panic!("expected healthy, got {other:?}"),
        }
        let findings = scan(&root);
        assert_eq!(findings.len(), 1);
        assert!(matches!(findings[0].health, Health::Healthy { .. }));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_lib_is_poisoned() {
        let root = unique_tmp("missing-lib");
        // Marker dir present, but no libwebrtc.a — the classic interrupted extract.
        make_cache(&root, "release", "scratch-bbb", None);
        let findings = scan(&root);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].health,
            Health::Poisoned(PoisonKind::LibMissing)
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn zero_byte_lib_is_poisoned() {
        let root = unique_tmp("zero");
        make_cache(&root, "debug", "scratch-ccc", Some(0));
        let findings = scan(&root);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0].health,
            Health::Poisoned(PoisonKind::LibTooSmall { size: 0, .. })
        ));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn truncated_lib_is_poisoned() {
        let root = unique_tmp("trunc");
        // 512 KiB — below the 1 MiB floor → truncated download.
        make_cache(&root, "release", "scratch-ddd", Some(512 * 1024));
        let findings = scan(&root);
        assert_eq!(findings.len(), 1);
        assert!(matches!(
            findings[0].health,
            Health::Poisoned(PoisonKind::LibTooSmall { .. })
        ));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn non_webrtc_scratch_dir_is_ignored() {
        let root = unique_tmp("non-webrtc");
        // A `scratch-*` dir from some *other* crate: no livekit_webrtc marker.
        let other = root.join("debug").join("build").join("scratch-other");
        fs::create_dir_all(other.join("out")).unwrap();
        fs::write(other.join("build-script-build"), b"not webrtc").unwrap();
        // And one genuine healthy webrtc cache alongside it.
        make_cache(&root, "debug", "scratch-webrtc", Some(MIN_LIBWEBRTC_BYTES));

        let findings = scan(&root);
        // Only the webrtc one is picked up; the unrelated scratch is skipped.
        assert_eq!(findings.len(), 1);
        assert!(findings[0]
            .scratch_dir
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("scratch-webrtc"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn no_cache_scans_empty() {
        let root = unique_tmp("empty");
        fs::create_dir_all(root.join("debug").join("build")).unwrap();
        assert!(scan(&root).is_empty());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn human_size_formats() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1 << 20), "1.0 MiB");
        assert_eq!(human_size(700 * (1 << 20)), "700.0 MiB");
    }
}
