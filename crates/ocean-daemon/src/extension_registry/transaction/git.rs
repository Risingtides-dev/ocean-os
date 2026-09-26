//! Stage A4 pinned public Git acquisition (Stage A manifest §13.2).
//!
//! Fills an A3a acquisition lease from one exact commit of a public HTTPS
//! repository. Everything happens in `quarantine/<op>/` WITHOUT `.state.lock`,
//! under the lease's acquisition permit and one 60-second deadline; the result
//! is sealed by the same `sha256-tree-v1` digest and manifest validation as a
//! local install, so publication is the unchanged A3a path.
//!
//! Load-bearing rules:
//!
//! - **Grammar.** `https://<public-host>[:443]/<path>[.git]` plus a 40- or
//!   64-hex lowercase object id, nothing else: no userinfo, query, fragment,
//!   percent-encoding, IP literal, other port, reserved/single-label host, or
//!   extra field. The persisted source stays A0's `{kind,locator,revision}`.
//! - **Connection pinning.** The hostname is resolved ONCE per acquisition
//!   through an injectable resolver; the canonicalized, deduplicated answer set
//!   is rejected whole if any member is not public unicast. Each fetch attempt
//!   is a fresh `git` process given exactly one checked address through
//!   `http.curloptResolve=<host>:443:<address>` (no leading `+`, so libcurl
//!   never consults DNS for that host), while the URL keeps the hostname so
//!   TLS SNI and certificate verification are unchanged. Fallback walks the
//!   same checked set under the same deadline; there is no unpinned attempt.
//! - **Capability.** Host `git` comes from a fixed list of absolute paths (no
//!   `PATH` lookup), must report 2.37.0 or newer (the first release with
//!   `http.curloptResolve`), and must carry the `git-remote-https` helper.
//!   Anything else — including any non-macOS/Linux platform, where no reviewed
//!   generation-safe group primitive exists — fails closed as
//!   `git_connection_pinning_unavailable`.
//! - **Isolation.** Every `git` runs with `env_clear` plus a fixed environment
//!   (`PATH=/usr/bin:/bin`, an empty mode-0700 `HOME`, no system/global config,
//!   no prompt, `GIT_ASKPASS=/usr/bin/false`, no optional locks) and a fixed
//!   `-c` list: every protocol but the one in use is disallowed, redirects are
//!   off, proxies, extra headers, cookies, and credential helpers are empty,
//!   hooks point at `/dev/null`, submodule recursion and LFS filters are off,
//!   and fsck runs on received objects. Stderr is discarded, never logged.
//! - **Process groups.** Each `git` is the leader of a new process group. The
//!   leader is observed with `waitid(WNOWAIT)` and reaped only after its group
//!   is proven empty; a timeout or temp-size excess follows §10.5 (SIGTERM,
//!   2 s, SIGKILL, 2 s, then reap). A group that cannot be proven empty is left
//!   with its leader unreaped, so its PGID can never be reused and signaled.
//! - **No checkout, no filters.** The tree is listed with `ls-tree` and blob
//!   bytes are read with `cat-file --batch`, which applies no smudge/clean or
//!   text conversion, and written by this module through descriptor-relative
//!   exclusive creates. Only `100644`/`100755` blobs are accepted: a symlink,
//!   gitlink (submodule), `.gitmodules`, `.lfsconfig`, `.gitattributes` that
//!   declares a `filter=`, `.git` component, or unsafe path is refused.
//! - **Bounds.** 512 MiB for everything Git writes (measured while each
//!   process runs and after it exits); the A0 package limits (10,000 entries,
//!   depth 64, 256 MiB) on the listed tree before a byte is extracted.
//!
//! Nothing here runs package content, and no response or log carries a URL
//! credential (none is accepted), a resolved address, or Git output.

use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{mpsc, Arc};

use super::*;

pub(crate) const GIT_UNAVAILABLE: &str = "git_connection_pinning_unavailable";
const INVALID_GIT_SOURCE: &str = "invalid_git_source";
const HOST_NOT_PUBLIC: &str = "git_host_not_public";
const RESOLUTION_FAILED: &str = "git_resolution_failed";
const FETCH_FAILED: &str = "git_fetch_failed";
const TIMEOUT: &str = "git_acquisition_timeout";
const LIMIT: &str = "git_acquisition_limit";
const REVISION_MISMATCH: &str = "git_revision_mismatch";
const TREE_UNSUPPORTED: &str = "git_tree_unsupported";
const CLEANUP_FAILED: &str = "git_process_cleanup_failed";

/// §13.2 total acquisition deadline (DNS, every process, and extraction).
pub(crate) const ACQUISITION_DEADLINE: Duration = Duration::from_secs(60);
/// §13.2 ceiling for everything Git writes under `quarantine/<op>/fetch`.
const TEMP_CEILING: u64 = 512 * 1024 * 1024;
/// First Git release carrying `http.curloptResolve`.
const MINIMUM_GIT: (u64, u64, u64) = (2, 37, 0);
const HTTPS_PORT: u16 = 443;
const MAX_URL_BYTES: usize = 2048;
const MAX_COMPONENT_BYTES: usize = 255;
/// Bounded `ls-tree -z` output: 10,000 entries of at most ~4 KiB each.
const MAX_TREE_LISTING: u64 = 64 * 1024 * 1024;
const MAX_TREE_RECORD: usize = 8 * 1024;
const MAX_ATTRIBUTES_BYTES: usize = 1024 * 1024;
const MAX_SMALL_OUTPUT: u64 = 64 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const MEASURE_INTERVAL: Duration = Duration::from_millis(200);
/// §10.5 group grace after SIGTERM and again after SIGKILL.
const GROUP_GRACE: Duration = Duration::from_secs(2);

/// Host `git` candidates, in order. Never a `PATH` lookup: package or
/// environment influence cannot select the acquisition tool.
const GIT_CANDIDATES: [&str; 3] = [
    "/usr/bin/git",
    "/usr/local/bin/git",
    "/opt/homebrew/bin/git",
];
const FIXED_PATH: &str = "/usr/bin:/bin";
const DENY_PROGRAM: &str = "/usr/bin/false";

/// Top-level labels that never name a public HTTPS host (RFC 6761/6762/7686/
/// 8375/9476 and ICANN's reserved `internal`).
const RESERVED_TLDS: [&str; 10] = [
    "alt",
    "arpa",
    "example",
    "internal",
    "invalid",
    "local",
    "localdomain",
    "localhost",
    "onion",
    "test",
];

/// Fixed hardening applied to every `git` invocation (`-c key=value`). The
/// one allowed protocol is appended per call.
const HARDENED_CONFIG: [&str; 28] = [
    "protocol.allow=never",
    "http.followRedirects=false",
    "http.proxy=",
    "remote.origin.proxy=",
    "http.sslVerify=true",
    "http.extraHeader=",
    "http.cookieFile=",
    "http.saveCookies=false",
    "http.emptyAuth=false",
    "credential.helper=",
    "core.askPass=/usr/bin/false",
    "core.sshCommand=/usr/bin/false",
    "core.hooksPath=/dev/null",
    "core.fsmonitor=false",
    "core.symlinks=false",
    "core.protectHFS=true",
    "core.protectNTFS=true",
    "fetch.recurseSubmodules=false",
    "submodule.recurse=false",
    "fetch.fsckObjects=true",
    "transfer.fsckObjects=true",
    "fetch.writeCommitGraph=false",
    "gc.auto=0",
    "maintenance.auto=false",
    "filter.lfs.required=false",
    "filter.lfs.smudge=",
    "filter.lfs.clean=",
    "filter.lfs.process=",
];

// ---------------------------------------------------------------------------
// Source grammar.
// ---------------------------------------------------------------------------

/// A validated §13.2 source. `url` keeps the hostname: it is both the persisted
/// locator and the TLS URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitSource {
    pub(crate) url: String,
    pub(crate) host: String,
    pub(crate) revision: String,
}

/// Strict §13.2 grammar. Refusal is the closed `invalid_git_source` code and
/// happens before any permit, quarantine, DNS, or process.
pub(crate) fn parse_git_source(url: &str, revision: &str) -> Result<GitSource, &'static str> {
    let reject = Err(INVALID_GIT_SOURCE);
    if !matches!(revision.len(), 40 | 64)
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return reject;
    }
    if url.is_empty()
        || url.len() > MAX_URL_BYTES
        || !url.bytes().all(|byte| byte.is_ascii_graphic())
        || url.contains(['?', '#', '%', '\\', '@', '[', ']'])
    {
        return reject;
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return reject;
    };
    let Some((authority, path)) = rest.split_once('/') else {
        return reject;
    };
    let host = match authority.split_once(':') {
        Some((host, "443")) => host,
        Some(_) => return reject,
        None => authority,
    };
    if !public_hostname(host) || !repository_path(path) {
        return reject;
    }
    // Every generation must stay readable by accepted A0's strict schema.
    let persisted = InstallSource {
        kind: InstallSourceKind::Git,
        locator: url.to_string(),
        revision: Some(revision.to_string()),
    };
    if validate_source(&persisted).is_err() {
        return reject;
    }
    Ok(GitSource {
        url: url.to_string(),
        host: host.to_string(),
        revision: revision.to_string(),
    })
}

/// Lowercase LDH DNS name with at least two labels, an alphabetic (or IDNA)
/// top-level label, and no reserved/special-use suffix. IP literals in any
/// spelling fail the top-level rule.
fn public_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 || host.parse::<IpAddr>().is_ok() {
        return false;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let ldh = |label: &&str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    };
    if !labels.iter().all(ldh) {
        return false;
    }
    let tld = labels[labels.len() - 1];
    let alphabetic = tld.len() >= 2 && tld.bytes().all(|byte| byte.is_ascii_lowercase());
    (alphabetic || tld.starts_with("xn--")) && !RESERVED_TLDS.contains(&tld)
}

fn repository_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'~')
                })
        })
}

// ---------------------------------------------------------------------------
// Address policy and resolution.
// ---------------------------------------------------------------------------

/// Public unicast only. IPv4-mapped IPv6 answers are canonicalized to IPv4
/// first; IPv6 must be global unicast `2000::/3` outside every special block.
pub(crate) fn is_public_address(address: IpAddr) -> bool {
    match address.to_canonical() {
        IpAddr::V4(address) => public_v4(address),
        IpAddr::V6(address) => public_v6(address),
    }
}

fn public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    let special = a == 0 // "this network"
        || a == 10
        || a == 127
        || a >= 224 // multicast and reserved, including broadcast
        || (a == 100 && (64..=127).contains(&b)) // shared address space (CGNAT)
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && (c == 0 || c == 2)) // IETF assignments, TEST-NET-1
        || (a == 192 && b == 88 && c == 99) // deprecated 6to4 relay anycast
        || (a == 192 && b == 168)
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || (a == 198 && b == 51 && c == 100) // TEST-NET-2
        || (a == 203 && b == 0 && c == 113); // TEST-NET-3
    !special
}

fn public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if segments[0] & 0xe000 != 0x2000 {
        // Loopback, unspecified, mapped/compatible, NAT64, ULA, link-local,
        // site-local, multicast, discard: everything outside 2000::/3.
        return false;
    }
    let special = (segments[0] == 0x2001 && segments[1] < 0x0200) // IETF protocol assignments (Teredo, ORCHID, benchmarking)
        || (segments[0] == 0x2001 && segments[1] == 0x0db8) // documentation
        || segments[0] == 0x2002 // 6to4 embeds an arbitrary IPv4 address
        || (segments[0] == 0x3fff && segments[1] & 0xf000 == 0); // documentation 3fff::/20
    !special
}

/// Injectable hostname resolution. Production uses the system resolver on a
/// helper thread bounded by the remaining deadline.
pub(crate) trait HostResolver: Send + Sync {
    /// Every answer for `host`, or `None` when resolution failed or timed out.
    fn resolve(&self, host: &str, timeout: Duration) -> Option<Vec<IpAddr>>;
}

struct SystemResolver;

impl HostResolver for SystemResolver {
    fn resolve(&self, host: &str, timeout: Duration) -> Option<Vec<IpAddr>> {
        let (sender, receiver) = mpsc::channel();
        let host = host.to_owned();
        // getaddrinfo has no deadline of its own; a stuck lookup finishes on
        // its helper thread after this acquisition has already failed.
        std::thread::Builder::new()
            .name("ocean-git-resolve".into())
            .spawn(move || {
                let answers = (host.as_str(), HTTPS_PORT)
                    .to_socket_addrs()
                    .ok()
                    .map(|addresses| addresses.map(|address| address.ip()).collect());
                let _ = sender.send(answers);
            })
            .ok()?;
        receiver.recv_timeout(timeout).ok().flatten()
    }
}

/// Resolve once, canonicalize, deduplicate, and reject the whole set if any
/// answer is not public.
fn checked_addresses(
    resolver: &dyn HostResolver,
    host: &str,
    deadline: Instant,
) -> Step<Vec<IpAddr>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(Fail::Reject(TIMEOUT));
    }
    let answers = resolver
        .resolve(host, remaining)
        .ok_or(Fail::Reject(RESOLUTION_FAILED))?;
    let set: BTreeSet<IpAddr> = answers
        .into_iter()
        .map(|address| address.to_canonical())
        .collect();
    if set.is_empty() {
        return Err(Fail::Reject(RESOLUTION_FAILED));
    }
    if !set.iter().all(|address| is_public_address(*address)) {
        return Err(Fail::Reject(HOST_NOT_PUBLIC));
    }
    Ok(set.into_iter().collect())
}

/// Git's `CURLOPT_RESOLVE` entry. No leading `+`: the pin never expires for
/// the bounded process, so libcurl never asks DNS about this host.
fn pin_entry(host: &str, port: u16, address: IpAddr) -> String {
    match address {
        IpAddr::V4(address) => format!("{host}:{port}:{address}"),
        IpAddr::V6(address) => format!("{host}:{port}:[{address}]"),
    }
}

// ---------------------------------------------------------------------------
// The acquirer.
// ---------------------------------------------------------------------------

/// Pinned Git acquisition policy. Production is [`GitAcquirer::system`]; tests
/// inject a resolver, a tool, tighter bounds, or a local fixture remote.
pub(crate) struct GitAcquirer {
    resolver: Arc<dyn HostResolver>,
    /// `None` resolves the host tool from [`GIT_CANDIDATES`].
    program: Option<PathBuf>,
    deadline: Duration,
    temp_ceiling: u64,
    max_entries: usize,
    max_depth: usize,
    max_bytes: u64,
    /// Test-only: fetch from a local repository (`file://`) after the full
    /// resolution and address check, so extraction is exercised offline.
    #[cfg(test)]
    file_remote: Option<PathBuf>,
}

impl GitAcquirer {
    pub(crate) fn system() -> Self {
        Self {
            resolver: Arc::new(SystemResolver),
            program: None,
            deadline: ACQUISITION_DEADLINE,
            temp_ceiling: TEMP_CEILING,
            max_entries: MAX_PACKAGE_ENTRIES,
            max_depth: MAX_PACKAGE_DEPTH,
            max_bytes: MAX_PACKAGE_BYTES,
            #[cfg(test)]
            file_remote: None,
        }
    }

    /// The acquirer for one registry. Outside tests this is always the system
    /// policy; tests register per-config overrides and otherwise get a
    /// resolver that refuses, so no test can reach the network by accident.
    pub(crate) fn for_config(config_dir: &Path) -> Arc<Self> {
        #[cfg(test)]
        {
            let key = fs::canonicalize(config_dir).unwrap_or_else(|_| config_dir.to_path_buf());
            if let Some(found) = test_support::registered(&key) {
                return found;
            }
            Arc::new(Self::system().with_resolver(Arc::new(test_support::Offline)))
        }
        #[cfg(not(test))]
        {
            let _ = config_dir;
            Arc::new(Self::system())
        }
    }

    /// The URL git actually fetches and the one protocol it may use.
    fn fetch_target(&self, source: &GitSource) -> (String, &'static str) {
        #[cfg(test)]
        if let Some(remote) = &self.file_remote {
            return (format!("file://{}", remote.display()), "file");
        }
        (source.url.clone(), "https")
    }

    /// Everything after the lease exists: probe the tool, resolve and check
    /// the host, fetch through each pinned address in turn, verify the exact
    /// commit, and extract its root tree into `lease.artifact`.
    fn fill(
        &self,
        lease: &mut AcquisitionLease,
        source: &GitSource,
        deadline: Instant,
    ) -> Step<()> {
        if lease.source.is_some() {
            return Err(Fail::Reject("invalid_request"));
        }
        let work = Workspace::create(lease)?;
        let result = self.fill_in(&work, &lease.artifact, source, deadline);
        // Git's scratch never reaches staging. On failure the dropped lease
        // deletes the whole quarantine anyway.
        let removed = remove_tree_at(&lease.quarantine, c"fetch", 0);
        result?;
        removed.map_err(unavailable)?;
        fsync_dir(&lease.artifact).map_err(unavailable)?;
        fsync_dir(&lease.quarantine).map_err(unavailable)?;
        lease.source = Some(InstallSource {
            kind: InstallSourceKind::Git,
            locator: source.url.clone(),
            revision: Some(source.revision.clone()),
        });
        Ok(())
    }

    fn fill_in(
        &self,
        work: &Workspace,
        artifact: &File,
        source: &GitSource,
        deadline: Instant,
    ) -> Step<()> {
        let program = self.probe(work, deadline)?;
        let addresses = checked_addresses(&*self.resolver, &source.host, deadline)?;
        let (target, scheme) = self.fetch_target(source);
        let mut fetched = false;
        for address in addresses {
            let pin = pin_entry(&source.host, HTTPS_PORT, address);
            match self.fetch_attempt(
                &program,
                work,
                &target,
                scheme,
                Some(&pin),
                &source.revision,
                deadline,
            ) {
                Ok(()) => {
                    fetched = true;
                    break;
                }
                // Another member of the originally checked set, in a fresh
                // pinned process, under the same deadline.
                Err(Fail::Reject(FETCH_FAILED)) => continue,
                Err(other) => return Err(other),
            }
        }
        if !fetched {
            return Err(Fail::Reject(FETCH_FAILED));
        }
        self.verify_commit(&program, work, &source.revision, deadline)?;
        let entries = self.list_tree(&program, work, &source.revision, deadline)?;
        self.extract(&program, work, artifact, &entries, deadline)
    }

    // -- capability -------------------------------------------------------

    /// Resolve the tool without package influence and prove it can pin: Git
    /// 2.37.0+ with the HTTPS remote helper. Any doubt is fail-closed.
    fn probe(&self, work: &Workspace, deadline: Instant) -> Step<PathBuf> {
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            return Err(Fail::Reject(GIT_UNAVAILABLE));
        }
        let program = match &self.program {
            Some(program) => program.clone(),
            None => locate_git().ok_or(Fail::Reject(GIT_UNAVAILABLE))?,
        };
        let unavailable_unless_bounded = |fail: Fail| match fail {
            Fail::Reject(TIMEOUT) | Fail::Reject(CLEANUP_FAILED) => fail,
            _ => Fail::Reject(GIT_UNAVAILABLE),
        };
        let version = self
            .capture(
                &program,
                work,
                &["--version".into()],
                c"probe.out",
                deadline,
            )
            .map_err(unavailable_unless_bounded)?;
        let version = std::str::from_utf8(&version).map_err(|_| Fail::Reject(GIT_UNAVAILABLE))?;
        match parse_git_version(version) {
            Some(found) if found >= MINIMUM_GIT => {}
            _ => return Err(Fail::Reject(GIT_UNAVAILABLE)),
        }
        let exec_path = self
            .capture(
                &program,
                work,
                &["--exec-path".into()],
                c"probe.out",
                deadline,
            )
            .map_err(unavailable_unless_bounded)?;
        let exec_path = std::str::from_utf8(&exec_path)
            .map(str::trim_end)
            .map_err(|_| Fail::Reject(GIT_UNAVAILABLE))?;
        let helper = Path::new(exec_path).join("git-remote-https");
        if !Path::new(exec_path).is_absolute()
            || !fs::metadata(helper).is_ok_and(|metadata| metadata.is_file())
        {
            return Err(Fail::Reject(GIT_UNAVAILABLE));
        }
        Ok(program)
    }

    // -- fetch ------------------------------------------------------------

    /// One pinned attempt in a fresh bare repository: exact object, depth 1,
    /// no tags, no submodules, no auto-gc.
    #[allow(clippy::too_many_arguments)]
    fn fetch_attempt(
        &self,
        program: &Path,
        work: &Workspace,
        target: &str,
        scheme: &str,
        pin: Option<&str>,
        revision: &str,
        deadline: Instant,
    ) -> Step<()> {
        remove_tree_at(&work.dir, c"repo.git", 0).map_err(unavailable)?;
        let repo = work.path.join("repo.git");
        let mut init: Vec<OsString> = vec!["init".into(), "--bare".into(), "--quiet".into()];
        // An empty template: no sample hooks, no info/exclude, nothing inherited.
        init.push("--template=".into());
        if revision.len() == 64 {
            init.push("--object-format=sha256".into());
        }
        init.push(repo.clone().into_os_string());
        let status = self.run(program, work, scheme, None, &init, None, None, deadline)?;
        if !status.success() {
            return Err(Fail::Reject(GIT_UNAVAILABLE));
        }
        let fetch: Vec<OsString> = vec![
            git_dir(&repo),
            "fetch".into(),
            "--quiet".into(),
            "--no-tags".into(),
            "--no-recurse-submodules".into(),
            "--no-auto-gc".into(),
            "--depth=1".into(),
            "--end-of-options".into(),
            target.into(),
            revision.into(),
        ];
        let status = self.run(program, work, scheme, pin, &fetch, None, None, deadline)?;
        if status.success() {
            Ok(())
        } else {
            Err(Fail::Reject(FETCH_FAILED))
        }
    }

    /// `FETCH_HEAD` names exactly the requested id and that object is a
    /// commit (a tree or blob id fetched by mistake is refused).
    fn verify_commit(
        &self,
        program: &Path,
        work: &Workspace,
        revision: &str,
        deadline: Instant,
    ) -> Step<()> {
        let repo = open_dir_nofollow(&work.dir, c"repo.git").map_err(unavailable)?;
        let mut head = open_regular_file_at(&repo, OsStr::new("FETCH_HEAD"), "FETCH_HEAD")
            .map_err(|_| Fail::Reject(REVISION_MISMATCH))?;
        let head = read_capped(&mut head, MAX_SMALL_OUTPUT, "FETCH_HEAD")
            .map_err(|_| Fail::Reject(REVISION_MISMATCH))?;
        let mut lines = head
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty());
        let first = lines.next().ok_or(Fail::Reject(REVISION_MISMATCH))?;
        if lines.next().is_some()
            || first.len() <= revision.len()
            || &first[..revision.len()] != revision.as_bytes()
            || first[revision.len()] != b'\t'
        {
            return Err(Fail::Reject(REVISION_MISMATCH));
        }
        let kind = self.capture(
            program,
            work,
            &[
                git_dir(&work.path.join("repo.git")),
                "cat-file".into(),
                "-t".into(),
                "--end-of-options".into(),
                revision.into(),
            ],
            c"type.out",
            deadline,
        )?;
        if kind != b"commit\n" {
            return Err(Fail::Reject(REVISION_MISMATCH));
        }
        Ok(())
    }

    // -- tree -------------------------------------------------------------

    fn list_tree(
        &self,
        program: &Path,
        work: &Workspace,
        revision: &str,
        deadline: Instant,
    ) -> Step<Vec<TreeEntry>> {
        let repo = work.path.join("repo.git");
        let args: Vec<OsString> = vec![
            git_dir(&repo),
            "ls-tree".into(),
            "-r".into(),
            "-l".into(),
            "-z".into(),
            "--full-tree".into(),
            "--end-of-options".into(),
            revision.into(),
        ];
        let output = work.output(c"tree.out")?;
        let status = self.run(
            program,
            work,
            "https",
            None,
            &args,
            None,
            Some(output),
            deadline,
        )?;
        if !status.success() {
            return Err(Fail::Reject(REVISION_MISMATCH));
        }
        let listing = work.input(c"tree.out")?;
        if listing.metadata().map_err(unavailable)?.len() > MAX_TREE_LISTING {
            return Err(Fail::Reject(PACKAGE_INVALID));
        }
        self.parse_tree(BufReader::new(listing), revision.len())
    }

    /// Validate every listed entry and the A0 limits before any byte is
    /// extracted.
    fn parse_tree(&self, reader: impl BufRead, oid_len: usize) -> Step<Vec<TreeEntry>> {
        let mut entries = Vec::new();
        let mut directories: BTreeSet<String> = BTreeSet::new();
        let mut bytes = 0u64;
        for record in reader.split(0) {
            let record = record.map_err(unavailable)?;
            if record.len() > MAX_TREE_RECORD {
                return Err(Fail::Reject(TREE_UNSUPPORTED));
            }
            let text = std::str::from_utf8(&record).map_err(|_| Fail::Reject(TREE_UNSUPPORTED))?;
            let (meta, path) = text
                .split_once('\t')
                .ok_or(Fail::Reject(TREE_UNSUPPORTED))?;
            let mut fields = meta.split_ascii_whitespace();
            let (Some(mode), Some(kind), Some(oid), Some(size), None) = (
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
                fields.next(),
            ) else {
                return Err(Fail::Reject(TREE_UNSUPPORTED));
            };
            // Only plain blobs: `120000` symlinks and `160000` gitlinks
            // (submodules) are refused, never materialized.
            let mode: libc::mode_t = match (mode, kind) {
                ("100644", "blob") => 0o644,
                ("100755", "blob") => 0o755,
                _ => return Err(Fail::Reject(TREE_UNSUPPORTED)),
            };
            if oid.len() != oid_len
                || !oid
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err(Fail::Reject(TREE_UNSUPPORTED));
            }
            let size: u64 = size.parse().map_err(|_| Fail::Reject(TREE_UNSUPPORTED))?;
            let components: Vec<&str> = path.split('/').collect();
            if !components.iter().all(|component| safe_component(component)) {
                return Err(Fail::Reject(TREE_UNSUPPORTED));
            }
            if components.len() - 1 > self.max_depth {
                return Err(Fail::Reject(PACKAGE_INVALID));
            }
            for depth in 1..components.len() {
                directories.insert(components[..depth].join("/"));
            }
            bytes = bytes
                .checked_add(size)
                .ok_or(Fail::Reject(PACKAGE_INVALID))?;
            entries.push(TreeEntry {
                path: path.to_string(),
                oid: oid.to_string(),
                size,
                mode,
            });
            if bytes > self.max_bytes || entries.len() + directories.len() > self.max_entries {
                return Err(Fail::Reject(PACKAGE_INVALID));
            }
        }
        if entries.is_empty() {
            return Err(Fail::Reject(PACKAGE_INVALID));
        }
        Ok(entries)
    }

    /// Stream every listed blob through `cat-file --batch` (no filters, no
    /// text conversion) and write it with exclusive descriptor-relative
    /// creates. A header that disagrees with the listing, or trailing bytes,
    /// fails the acquisition.
    fn extract(
        &self,
        program: &Path,
        work: &Workspace,
        artifact: &File,
        entries: &[TreeEntry],
        deadline: Instant,
    ) -> Step<()> {
        {
            let mut request = work.output(c"batch.in")?;
            for entry in entries {
                request
                    .write_all(entry.oid.as_bytes())
                    .and_then(|()| request.write_all(b"\n"))
                    .map_err(unavailable)?;
            }
        }
        let repo = work.path.join("repo.git");
        let args: Vec<OsString> = vec![git_dir(&repo), "cat-file".into(), "--batch".into()];
        let input = work.input(c"batch.in")?;
        let output = work.output(c"blobs.out")?;
        let status = self.run(
            program,
            work,
            "https",
            None,
            &args,
            Some(input),
            Some(output),
            deadline,
        )?;
        if !status.success() {
            return Err(Fail::Reject(PACKAGE_INVALID));
        }
        let mut reader = BufReader::new(work.input(c"blobs.out")?);
        let mut writer = TreeWriter::new(artifact);
        let mut buffer = vec![0u8; 64 * 1024];
        for entry in entries {
            let mut header = Vec::new();
            (&mut reader)
                .take(256)
                .read_until(b'\n', &mut header)
                .map_err(unavailable)?;
            if header != format!("{} blob {}\n", entry.oid, entry.size).as_bytes() {
                return Err(Fail::Reject(PACKAGE_INVALID));
            }
            let mut output = writer.create(&entry.path, entry.mode)?;
            let attributes = entry.path.rsplit('/').next() == Some(".gitattributes");
            let mut collected = Vec::new();
            let mut remaining = entry.size;
            while remaining > 0 {
                let want = usize::try_from(remaining.min(buffer.len() as u64)).unwrap_or(0);
                let count = reader.read(&mut buffer[..want]).map_err(unavailable)?;
                if count == 0 {
                    return Err(Fail::Reject(PACKAGE_INVALID));
                }
                output.write_all(&buffer[..count]).map_err(unavailable)?;
                if attributes {
                    if collected.len() + count > MAX_ATTRIBUTES_BYTES {
                        return Err(Fail::Reject(TREE_UNSUPPORTED));
                    }
                    collected.extend_from_slice(&buffer[..count]);
                }
                remaining -= count as u64;
            }
            let mut separator = [0u8; 1];
            reader
                .read_exact(&mut separator)
                .map_err(|_| Fail::Reject(PACKAGE_INVALID))?;
            if separator != *b"\n" {
                return Err(Fail::Reject(PACKAGE_INVALID));
            }
            output.sync_all().map_err(unavailable)?;
            if attributes && declares_filter(&collected) {
                return Err(Fail::Reject(TREE_UNSUPPORTED));
            }
        }
        let mut trailing = [0u8; 1];
        if reader.read(&mut trailing).map_err(unavailable)? != 0 {
            return Err(Fail::Reject(PACKAGE_INVALID));
        }
        writer.finish()
    }

    // -- process execution --------------------------------------------------

    /// Run `git <args>` and return its stdout (bounded) when it succeeds.
    fn capture(
        &self,
        program: &Path,
        work: &Workspace,
        args: &[OsString],
        name: &CStr,
        deadline: Instant,
    ) -> Step<Vec<u8>> {
        let output = work.output(name)?;
        let status = self.run(
            program,
            work,
            "https",
            None,
            args,
            None,
            Some(output),
            deadline,
        )?;
        if !status.success() {
            return Err(Fail::Reject(GIT_UNAVAILABLE));
        }
        let mut file = work.input(name)?;
        read_capped(&mut file, MAX_SMALL_OUTPUT, "git output")
            .map_err(|_| Fail::Reject(GIT_UNAVAILABLE))
    }

    /// One bounded `git` process: stripped environment, hardened config,
    /// discarded stderr, a new process group, the shared deadline, and the
    /// temp ceiling measured while it runs and after it exits.
    #[allow(clippy::too_many_arguments)]
    fn run(
        &self,
        program: &Path,
        work: &Workspace,
        scheme: &str,
        pin: Option<&str>,
        args: &[OsString],
        stdin: Option<File>,
        stdout: Option<File>,
        deadline: Instant,
    ) -> Step<ExitStatus> {
        let command = command(program, work, scheme, pin, args, stdin, stdout);
        let status = run_group(command, deadline, || work.bytes() > self.temp_ceiling)?;
        if work.bytes() > self.temp_ceiling {
            return Err(Fail::Reject(LIMIT));
        }
        Ok(status)
    }
}

#[derive(Debug)]
struct TreeEntry {
    path: String,
    oid: String,
    size: u64,
    mode: libc::mode_t,
}

fn git_dir(repo: &Path) -> OsString {
    let mut argument = OsString::from("--git-dir=");
    argument.push(repo.as_os_str());
    argument
}

fn locate_git() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    GIT_CANDIDATES.iter().map(PathBuf::from).find(|candidate| {
        fs::metadata(candidate)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

/// `git version X.Y[.Z…]` → `(X, Y, Z)`. Vendor suffixes such as
/// ` (Apple Git-155)` or `.windows.1` are ignored; a missing or non-numeric
/// major/minor is unparseable and therefore unsupported.
pub(crate) fn parse_git_version(text: &str) -> Option<(u64, u64, u64)> {
    let token = text
        .strip_prefix("git version ")?
        .split_ascii_whitespace()
        .next()?;
    let mut parts = token.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()
        .map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// A path component git may hand us that is safe to create: no empty, `.`,
/// `..`, `.git` (any case), `.gitmodules`, or `.lfsconfig`; no control
/// character; at most 255 bytes.
fn safe_component(component: &str) -> bool {
    !component.is_empty()
        && component.len() <= MAX_COMPONENT_BYTES
        && component != "."
        && component != ".."
        && !component.eq_ignore_ascii_case(".git")
        && !component.eq_ignore_ascii_case(".gitmodules")
        && !component.eq_ignore_ascii_case(".lfsconfig")
        && !component.chars().any(char::is_control)
}

/// True when any `.gitattributes` line assigns a `filter` driver (Git LFS or
/// any smudge/clean filter), including through an `[attr]` macro.
fn declares_filter(attributes: &[u8]) -> bool {
    attributes.split(|byte| *byte == b'\n').any(|line| {
        let mut tokens = line
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|token| !token.is_empty());
        match tokens.next() {
            Some(first) if first.starts_with(b"#") => false,
            Some(_) => tokens.any(|token| token.starts_with(b"filter=")),
            None => false,
        }
    })
}

/// The exact environment every `git` sees. Nothing is inherited.
fn fixed_environment(home: &Path) -> [(&'static str, OsString); 10] {
    [
        ("PATH", FIXED_PATH.into()),
        ("HOME", home.as_os_str().to_owned()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ("GIT_TERMINAL_PROMPT", "0".into()),
        ("GIT_ASKPASS", DENY_PROGRAM.into()),
        ("GIT_OPTIONAL_LOCKS", "0".into()),
        ("GIT_NO_REPLACE_OBJECTS", "1".into()),
        ("GIT_PROTOCOL_FROM_USER", "0".into()),
        ("LC_ALL", "C".into()),
    ]
}

/// `-c` hardening, the single allowed protocol, and at most one pin.
fn config_arguments(scheme: &str, pin: Option<&str>) -> Vec<OsString> {
    let mut arguments = Vec::with_capacity(HARDENED_CONFIG.len() * 2 + 4);
    for pair in HARDENED_CONFIG {
        arguments.push("-c".into());
        arguments.push(pair.into());
    }
    arguments.push("-c".into());
    arguments.push(format!("protocol.{scheme}.allow=always").into());
    if let Some(pin) = pin {
        arguments.push("-c".into());
        arguments.push(format!("http.curloptResolve={pin}").into());
    }
    arguments
}

fn command(
    program: &Path,
    work: &Workspace,
    scheme: &str,
    pin: Option<&str>,
    args: &[OsString],
    stdin: Option<File>,
    stdout: Option<File>,
) -> Command {
    use std::os::unix::process::CommandExt as _;
    let mut command = Command::new(program);
    command.env_clear();
    for (name, value) in fixed_environment(&work.home) {
        command.env(name, value);
    }
    command
        .args(config_arguments(scheme, pin))
        .args(args)
        .current_dir(&work.path)
        .stdin(stdin.map_or_else(Stdio::null, Stdio::from))
        .stdout(stdout.map_or_else(Stdio::null, Stdio::from))
        // Git's stderr can echo URLs and server text; it is never read.
        .stderr(Stdio::null())
        .process_group(0);
    command
}

// ---------------------------------------------------------------------------
// Scratch workspace.
// ---------------------------------------------------------------------------

/// `quarantine/<op>/fetch`: the only directory Git writes. It is removed
/// before the lease is sealed.
struct Workspace {
    dir: File,
    path: PathBuf,
    home: PathBuf,
}

impl Workspace {
    fn create(lease: &AcquisitionLease) -> Step<Self> {
        let dir = mkdir_open(&lease.quarantine, c"fetch", 0o700, false).map_err(unavailable)?;
        let path = lease.path.join("fetch");
        // Git takes paths, not descriptors: prove the path names the
        // directory this acquisition created before any process sees it.
        let named = File::open(&path).map_err(unavailable)?;
        if !same_file(&dir, &named).map_err(unavailable)? {
            return Err(Fail::Reject(STATE_UNAVAILABLE));
        }
        mkdir_open(&dir, c"home", 0o700, false).map_err(unavailable)?;
        Ok(Self {
            home: path.join("home"),
            dir,
            path,
        })
    }

    fn output(&self, name: &CStr) -> Step<File> {
        remove_tree_at(&self.dir, name, 0).map_err(unavailable)?;
        create_file_at(&self.dir, name, 0o600).map_err(unavailable)
    }

    fn input(&self, name: &CStr) -> Step<File> {
        open_regular_file_at(&self.dir, OsStr::from_bytes(name.to_bytes()), "git scratch")
            .map_err(|_| Fail::Reject(STATE_UNAVAILABLE))
    }

    /// Apparent bytes under the workspace, never following a link. An
    /// unmeasurable or implausibly deep/wide tree counts as over the ceiling.
    fn bytes(&self) -> u64 {
        fn walk(path: &Path, depth: usize, total: &mut u64, entries: &mut usize) -> bool {
            if depth > 32 {
                return false;
            }
            let Ok(listing) = fs::read_dir(path) else {
                return false;
            };
            for entry in listing {
                let Ok(entry) = entry else { return false };
                *entries += 1;
                if *entries > 1_000_000 {
                    return false;
                }
                // DirEntry::metadata does not traverse symlinks on Unix.
                let Ok(metadata) = entry.metadata() else {
                    return false;
                };
                *total = total.saturating_add(metadata.len());
                if metadata.is_dir() && !walk(&entry.path(), depth + 1, total, entries) {
                    return false;
                }
            }
            true
        }
        let mut total = 0;
        let mut entries = 0;
        if walk(&self.path, 0, &mut total, &mut entries) {
            total
        } else {
            u64::MAX
        }
    }
}

/// Creates the extracted tree through exclusive, descriptor-relative
/// operations. A directory that already exists but was not created by this
/// writer (a case-folding or normalization collision) or a file that already
/// exists is refused rather than merged.
struct TreeWriter<'a> {
    root: &'a File,
    chain: Vec<(String, File)>,
    created: HashSet<String>,
}

impl<'a> TreeWriter<'a> {
    fn new(root: &'a File) -> Self {
        Self {
            root,
            chain: Vec::new(),
            created: HashSet::new(),
        }
    }

    fn create(&mut self, path: &str, mode: libc::mode_t) -> Step<File> {
        let components: Vec<&str> = path.split('/').collect();
        let (name, directories) = components
            .split_last()
            .ok_or(Fail::Reject(TREE_UNSUPPORTED))?;
        let common = self
            .chain
            .iter()
            .zip(directories)
            .take_while(|((open, _), wanted)| open == *wanted)
            .count();
        while self.chain.len() > common {
            let (_, directory) = self.chain.pop().expect("chain is longer than common");
            fsync_dir(&directory).map_err(unavailable)?;
        }
        for depth in common..directories.len() {
            let prefix = directories[..=depth].join("/");
            let component =
                CString::new(directories[depth]).map_err(|_| Fail::Reject(TREE_UNSUPPORTED))?;
            let parent = self.chain.last().map_or(self.root, |(_, file)| file);
            // SAFETY: parent is a live directory descriptor and the component
            // is NUL-terminated.
            let made = unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o755) } == 0;
            if !made {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EEXIST) || !self.created.contains(&prefix) {
                    return Err(Fail::Reject(TREE_UNSUPPORTED));
                }
            }
            let directory = open_dir_nofollow(parent, &component)
                .map_err(|_| Fail::Reject(TREE_UNSUPPORTED))?;
            if made {
                // Exact mode regardless of umask, matching local acquisition.
                // SAFETY: directory is a live descriptor.
                cvt(unsafe { libc::fchmod(directory.as_raw_fd(), 0o755) }).map_err(unavailable)?;
            }
            self.created.insert(prefix);
            self.chain.push((directories[depth].to_string(), directory));
        }
        let parent = self.chain.last().map_or(self.root, |(_, file)| file);
        let name = CString::new(*name).map_err(|_| Fail::Reject(TREE_UNSUPPORTED))?;
        create_file_at(parent, &name, mode).map_err(|error| {
            if error.raw_os_error() == Some(libc::EEXIST) {
                Fail::Reject(TREE_UNSUPPORTED)
            } else {
                unavailable(error)
            }
        })
    }

    fn finish(mut self) -> Step<()> {
        while let Some((_, directory)) = self.chain.pop() {
            fsync_dir(&directory).map_err(unavailable)?;
        }
        fsync_dir(self.root).map_err(unavailable)
    }
}

// ---------------------------------------------------------------------------
// Generation-safe process groups (§10.5 applied to the acquisition tool).
// ---------------------------------------------------------------------------

/// Spawn `command` as the leader of a new process group and wait for it
/// under `deadline`, polling `over_limit` while it runs.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn run_group(
    mut command: Command,
    deadline: Instant,
    over_limit: impl Fn() -> bool,
) -> Step<ExitStatus> {
    let child = command.spawn().map_err(|_| Fail::Reject(GIT_UNAVAILABLE))?;
    let group = ToolGroup::new(child)?;
    let mut next_measure = Instant::now() + MEASURE_INTERVAL;
    loop {
        match group.leader_exited() {
            Some(true) => break,
            Some(false) => {}
            // Ownership is unprovable: never signal a group we cannot pin.
            None => return Err(Fail::Reject(CLEANUP_FAILED)),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(group.terminate(TIMEOUT));
        }
        if now >= next_measure {
            if over_limit() {
                return Err(group.terminate(LIMIT));
            }
            next_measure = now + MEASURE_INTERVAL;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    group.finish()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn run_group(_: Command, _: Instant, _: impl Fn() -> bool) -> Step<ExitStatus> {
    Err(Fail::Reject(GIT_UNAVAILABLE))
}

/// A spawned group leader that is never reaped until its group is proven
/// empty. While the leader is an unreaped child (running or zombie) its PID —
/// and therefore the PGID — cannot be reused, so every signal names only this
/// generation. Dropping an unfinished group deliberately leaves the leader
/// unreaped.
#[cfg(any(target_os = "macos", target_os = "linux"))]
struct ToolGroup {
    child: Option<Child>,
    leader: libc::pid_t,
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
impl ToolGroup {
    fn new(child: Child) -> Step<Self> {
        let leader = libc::pid_t::try_from(child.id()).map_err(|_| Fail::Reject(CLEANUP_FAILED))?;
        Ok(Self {
            child: Some(child),
            leader,
        })
    }

    /// `Some(true)` once the leader has exited (still unreaped), `Some(false)`
    /// while it runs, `None` if it is no longer our unreaped child.
    fn leader_exited(&self) -> Option<bool> {
        self.child.as_ref()?;
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        // SAFETY: info is writable siginfo storage. WNOWAIT keeps the zombie.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.leader as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return None;
        }
        // SAFETY: waitid initialized info on success; si_pid is 0 while the
        // child is still running.
        Some(unsafe { info.assume_init().si_pid() } == self.leader)
    }

    fn signal(&self, signal: libc::c_int) -> bool {
        if self.leader_exited().is_none() {
            return false;
        }
        // SAFETY: the leader is our unreaped child, so -leader names only
        // this generation's process group.
        if unsafe { libc::kill(-self.leader, signal) } == 0 {
            return true;
        }
        io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    fn wait_empty(&self, grace: Duration) -> bool {
        let until = Instant::now() + grace;
        loop {
            if let Ok(false) = crate::extension_service::group_has_live_members(self.leader) {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn reap(&mut self) -> Option<ExitStatus> {
        self.child.take()?.wait().ok()
    }

    /// §10.5: SIGTERM, grace, SIGKILL, grace, then reap only a proven-empty
    /// group. Returns the failure to report.
    fn terminate(mut self, code: &'static str) -> Fail {
        let emptied = (self.signal(libc::SIGTERM) && self.wait_empty(GROUP_GRACE))
            || (self.signal(libc::SIGKILL) && self.wait_empty(GROUP_GRACE));
        if emptied && self.reap().is_some() {
            Fail::Reject(code)
        } else {
            Fail::Reject(CLEANUP_FAILED)
        }
    }

    /// The leader exited on its own: kill any member that outlived it (the
    /// zombie leader still pins the PGID), prove the group empty, then reap.
    fn finish(mut self) -> Step<ExitStatus> {
        let empty = matches!(
            crate::extension_service::group_has_live_members(self.leader),
            Ok(false)
        ) || (self.signal(libc::SIGKILL) && self.wait_empty(GROUP_GRACE));
        if !empty {
            return Err(Fail::Reject(CLEANUP_FAILED));
        }
        self.reap().ok_or(Fail::Reject(CLEANUP_FAILED))
    }
}

// ---------------------------------------------------------------------------
// Writer entry points.
// ---------------------------------------------------------------------------

impl RegistryWriter {
    /// §13.2: permit, quarantine, pinned fetch, extraction, and seal for one
    /// exact public Git commit, under the system policy.
    pub(crate) fn acquire_git(
        &self,
        url: &str,
        revision: &str,
    ) -> Result<VerifiedQuarantine, MutationError> {
        let acquirer = GitAcquirer::for_config(&self.config_dir);
        self.acquire_git_with(&acquirer, url, revision)
    }

    pub(crate) fn acquire_git_with(
        &self,
        acquirer: &GitAcquirer,
        url: &str,
        revision: &str,
    ) -> Result<VerifiedQuarantine, MutationError> {
        let started = Instant::now();
        let refuse = |code| MutationError {
            operation_id: Uuid::new_v4(),
            committed: false,
            state_revision: 0,
            code,
        };
        let source = parse_git_source(url, revision).map_err(refuse)?;
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            return Err(refuse(GIT_UNAVAILABLE));
        }
        let mut lease = self.begin_acquisition()?;
        let operation_id = lease.operation_id;
        acquirer
            .fill(&mut lease, &source, started + acquirer.deadline)
            .map_err(|fail| self.error(operation_id, Failure::pre(0)(fail)))?;
        self.seal(lease)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    static REGISTERED: Mutex<BTreeMap<PathBuf, Arc<GitAcquirer>>> = Mutex::new(BTreeMap::new());

    /// Refuses every lookup: the default for tests that register nothing.
    pub(crate) struct Offline;

    impl HostResolver for Offline {
        fn resolve(&self, _: &str, _: Duration) -> Option<Vec<IpAddr>> {
            None
        }
    }

    /// Answers every lookup with a fixed set.
    pub(crate) struct Fixed(pub(crate) Vec<IpAddr>);

    impl HostResolver for Fixed {
        fn resolve(&self, _: &str, _: Duration) -> Option<Vec<IpAddr>> {
            Some(self.0.clone())
        }
    }

    pub(super) fn registered(key: &Path) -> Option<Arc<GitAcquirer>> {
        REGISTERED.lock().unwrap().get(key).cloned()
    }

    /// Route the registry at `config_dir` to `acquirer` until the guard drops.
    pub(crate) fn register(config_dir: &Path, acquirer: GitAcquirer) -> Registration {
        let key = fs::canonicalize(config_dir).unwrap();
        REGISTERED
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::new(acquirer));
        Registration(key)
    }

    pub(crate) struct Registration(PathBuf);

    impl Drop for Registration {
        fn drop(&mut self) {
            REGISTERED.lock().unwrap().remove(&self.0);
        }
    }

    impl GitAcquirer {
        pub(crate) fn with_resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
            self.resolver = resolver;
            self
        }

        pub(crate) fn with_program(mut self, program: PathBuf) -> Self {
            self.program = Some(program);
            self
        }

        pub(crate) fn with_deadline(mut self, deadline: Duration) -> Self {
            self.deadline = deadline;
            self
        }

        pub(crate) fn with_temp_ceiling(mut self, ceiling: u64) -> Self {
            self.temp_ceiling = ceiling;
            self
        }

        pub(crate) fn with_limits(mut self, entries: usize, depth: usize, bytes: u64) -> Self {
            self.max_entries = entries;
            self.max_depth = depth;
            self.max_bytes = bytes;
            self
        }

        pub(crate) fn with_file_remote(mut self, remote: PathBuf) -> Self {
            self.file_remote = Some(remote);
            self
        }
    }
}

#[cfg(test)]
mod tests;
