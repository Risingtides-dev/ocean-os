//! Stage A4 pinned public Git acquisition tests (manifest §13.2, §19.3–§19.4).
//!
//! Everything runs offline: extraction tests fetch from a local bare
//! repository through the test-only `file://` remote (after the full
//! resolution and public-address check), capability/argv/process-group tests
//! drive a scripted fake `git`, and the connection-pinning tests point the
//! real host `git` at a loopback listener under a `.invalid` hostname that DNS
//! can never answer. The single real-network smoke is `#[ignore]`d.

use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::test_support::{Fixed, Offline};
use super::*;

const URL: &str = "https://git.ocean-fixture.com/ocean/noop.git";
const ID: &str = "example.noop";
const PUBLIC_V4: IpAddr = IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34));
const PUBLIC_V6: IpAddr = IpAddr::V6(Ipv6Addr::new(
    0x2606, 0x2800, 0x0220, 0x0001, 0x0248, 0x1893, 0x25c8, 0x1946,
));

/// A no-op service entry that must never run during acquisition.
const MANIFEST: &str = "schema_version = 1\nid = \"example.noop\"\nname = \"Noop\"\nversion = \"1.0.0\"\nmin_ocean_version = \"0.1.0\"\n\n[[services]]\nid = \"lifecycle\"\nentry = \"services/lifecycle\"\nevents = []\n";

fn host_git() -> PathBuf {
    locate_git().expect("host git is required for the A4 fixture tests")
}

/// A bare "remote" plus a separate work tree, so the package directory never
/// contains `.git` and can also be installed as a local path.
struct Remote {
    temp: tempfile::TempDir,
    canary: PathBuf,
    /// A gitlink (submodule) entry staged after `add -A` at commit time.
    gitlink: std::cell::RefCell<Option<String>>,
}

impl Remote {
    fn new() -> Self {
        Self::with_format("sha1")
    }

    fn with_format(format: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let canary = temp.path().join("PACKAGE_CANARY_RAN");
        let remote = Self {
            temp,
            canary,
            gitlink: std::cell::RefCell::new(None),
        };
        fs::create_dir(remote.tree()).unwrap();
        remote.git(&[
            "init",
            "--bare",
            "--quiet",
            &format!("--object-format={format}"),
            remote.repo().to_str().unwrap(),
        ]);
        remote
    }

    fn repo(&self) -> PathBuf {
        self.temp.path().join("remote.git")
    }

    fn tree(&self) -> PathBuf {
        self.temp.path().join("tree")
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new(host_git())
            .args([
                "-c",
                "user.name=Ocean Test",
                "-c",
                "user.email=test@ocean.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(self.temp.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", self.temp.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn tree_git(&self, args: &[&str]) -> String {
        let git_dir = format!("--git-dir={}", self.repo().display());
        let work_tree = format!("--work-tree={}", self.tree().display());
        let mut full = vec![git_dir.as_str(), work_tree.as_str()];
        full.extend_from_slice(args);
        self.git(&full)
    }

    fn write(&self, path: &str, contents: &[u8], executable: bool) {
        let target = self.tree().join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, contents).unwrap();
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The Stage A no-op package plus executable canaries in every resource
    /// path: acquisition must never run any of them.
    fn package(&self) {
        self.write("ocean-extension.toml", MANIFEST.as_bytes(), false);
        let canary = format!("#!/bin/sh\nprintf ran > '{}'\n", self.canary.display());
        for path in ["services/lifecycle", "hooks/post-checkout", "run-me"] {
            self.write(path, canary.as_bytes(), true);
        }
    }

    fn commit(&self) -> String {
        self.tree_git(&["add", "-A"]);
        if let Some(path) = self.gitlink.borrow().as_deref() {
            let target = "0123456789abcdef0123456789abcdef01234567";
            self.tree_git(&[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{target},{path}"),
            ]);
        }
        self.tree_git(&["commit", "--quiet", "--allow-empty", "-m", "fixture"]);
        self.tree_git(&["rev-parse", "HEAD"])
    }

    fn assert_no_canary_ran(&self) {
        assert!(
            !self.canary.exists(),
            "package code executed during acquisition"
        );
    }
}

fn acquirer(remote: &Remote) -> GitAcquirer {
    GitAcquirer::system()
        .with_resolver(Arc::new(Fixed(vec![PUBLIC_V4])))
        .with_file_remote(remote.repo())
}

fn code<T>(result: Result<T, MutationError>) -> &'static str {
    match result {
        Ok(_) => "ok",
        Err(error) => {
            assert!(!error.committed);
            error.code
        }
    }
}

/// Nothing an acquisition created may outlive it.
fn assert_no_acquisition_residue(config: &Path) {
    for entry in fs::read_dir(config).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(
            !name.starts_with(BOOTSTRAP_PREFIX),
            "bootstrap residue {name}"
        );
    }
    if let Ok(entries) = fs::read_dir(config.join("extensions/quarantine")) {
        assert_eq!(entries.count(), 0, "quarantine residue");
    }
}

fn pid_gone(pid: libc::pid_t) -> bool {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        // SAFETY: probing with signal 0 sends nothing.
        if unsafe { libc::kill(pid, 0) } != 0
            && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            return true;
        }
        // A killed, reparented process can linger as a zombie where PID 1
        // does not reap (some containers); it is no longer running.
        #[cfg(target_os = "linux")]
        if fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, rest)| rest.starts_with('Z'))
        }) {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// ---------------------------------------------------------------------------
// Scripted fake git.
// ---------------------------------------------------------------------------

struct FakeGit {
    temp: tempfile::TempDir,
}

impl FakeGit {
    /// A `git` that logs every invocation's argv and environment, answers the
    /// capability probe with `version`, creates the repository on `init`, and
    /// runs `fetch_body` for `fetch`.
    fn new(version: &str, fetch_body: &str) -> Self {
        Self::scripted(version, fetch_body, "")
    }

    /// As [`Self::new`], with extra raw `case` arms (matched against
    /// `" $* "`) for the post-fetch verbs; `$GD` is the `--git-dir` value.
    fn scripted(version: &str, fetch_body: &str, arms: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("log");
        let exec = temp.path().join("libexec");
        fs::create_dir(&log).unwrap();
        fs::create_dir(&exec).unwrap();
        fs::write(exec.join("git-remote-https"), b"").unwrap();
        let script = format!(
            "#!/bin/sh\nLOG='{log}'\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$LOG/$$.argv\"\nenv > \"$LOG/$$.env\"\nGD=; for a in \"$@\"; do case \"$a\" in --git-dir=*) GD=\"${{a#--git-dir=}}\";; esac; done\ncase \" $* \" in\n  *' --version '*) printf '%s\\n' '{version}'; exit 0 ;;\n  *' --exec-path '*) printf '%s\\n' '{exec}'; exit 0 ;;\n  *' init '*) for last in \"$@\"; do :; done; mkdir -p \"$last\"; exit 0 ;;\n  *' fetch '*) {fetch_body} ;;\n{arms}\nesac\nexit 1\n",
            log = log.display(),
            exec = exec.display(),
        )
        .replace("{FAKE}", &temp.path().display().to_string());
        let program = temp.path().join("git");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Self { temp }
    }

    fn program(&self) -> PathBuf {
        self.temp.path().join("git")
    }

    fn log(&self) -> PathBuf {
        self.temp.path().join("log")
    }

    /// `(argv, env)` of every logged invocation whose argv contains `verb`.
    fn invocations(&self, verb: &str) -> Vec<(Vec<String>, Vec<String>)> {
        let mut found = Vec::new();
        for entry in fs::read_dir(self.log()).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("argv") {
                continue;
            }
            let argv: Vec<String> = fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect();
            if argv.iter().any(|arg| arg == verb) {
                let env = fs::read_to_string(path.with_extension("env"))
                    .unwrap()
                    .lines()
                    .map(str::to_string)
                    .collect();
                found.push((argv, env));
            }
        }
        found
    }

    fn grandchild(&self) -> libc::pid_t {
        let path = self.log().join("grandchild.pid");
        let until = Instant::now() + Duration::from_secs(5);
        while !path.exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::read_to_string(path).unwrap().trim().parse().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Grammar and address policy.
// ---------------------------------------------------------------------------

#[test]
fn a4_git_source_grammar_is_exact() {
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let rev64 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    for (url, revision) in [
        ("https://github.com/example/repo", rev),
        ("https://github.com/example/repo.git", rev64),
        ("https://git.example-host.co.uk:443/a/b/c.git", rev),
        ("https://xn--bcher-kva.xn--p1ai/r", rev),
    ] {
        let source = parse_git_source(url, revision).unwrap_or_else(|_| panic!("{url}"));
        assert_eq!(source.url, url);
        assert_eq!(source.revision, revision);
    }
    assert_eq!(
        parse_git_source("https://git.example-host.co.uk:443/a", rev)
            .unwrap()
            .host,
        "git.example-host.co.uk"
    );
    let rejected_urls = [
        "http://github.com/example/repo",
        "ssh://git@github.com/example/repo",
        "git@github.com:example/repo",
        "git://github.com/example/repo",
        "file:///tmp/repo",
        "HTTPS://github.com/example/repo",
        "https://token@github.com/example/repo",
        "https://user:pass@github.com/example/repo",
        "https://github.com/example/repo?token=secret",
        "https://github.com/example/repo#main",
        "https://github.com/example/%2e%2e/repo",
        "https://github.com/example/../repo",
        "https://github.com/example//repo",
        "https://github.com/example/repo/",
        "https://github.com/",
        "https://github.com",
        "https://github.com:8443/example/repo",
        "https://github.com:/example/repo",
        "https://GitHub.com/example/repo",
        "https://127.0.0.1/repo",
        "https://[::1]/repo",
        "https://2130706433/repo",
        "https://0x7f.0x1/repo",
        "https://localhost/repo",
        "https://intranet/repo",
        "https://git.corp.local/repo",
        "https://git.internal/repo",
        "https://pinned.invalid/repo",
        "https://repo.test/repo",
        "https://github.com/example repo",
        "https://github.com/ex\u{7f}ample/repo",
        "https://gïthub.com/example/repo",
        "https://-github.com/example/repo",
        "https://github.com/example\\repo",
    ];
    for url in rejected_urls {
        assert_eq!(
            parse_git_source(url, rev),
            Err(INVALID_GIT_SOURCE),
            "{url} must be refused"
        );
    }
    for revision in [
        "main",
        "HEAD",
        "v1.0.0",
        "refs/heads/main",
        "0123456",
        "0123456789ABCDEF0123456789ABCDEF01234567",
        "0123456789abcdef0123456789abcdef0123456",
        "0123456789abcdef0123456789abcdef012345678",
        "0123456789abcdef0123456789abcdef0123456g",
        "",
    ] {
        assert_eq!(
            parse_git_source("https://github.com/example/repo", revision),
            Err(INVALID_GIT_SOURCE),
            "{revision:?} must be refused"
        );
    }
}

#[test]
fn a4_address_policy_admits_only_public_unicast() {
    for public in [
        "93.184.216.34",
        "1.1.1.1",
        "8.8.8.8",
        "2606:2800:220:1:248:1893:25c8:1946",
        "2a00:1450:4001:80b::200e",
        "::ffff:93.184.216.34",
    ] {
        assert!(
            is_public_address(public.parse().unwrap()),
            "{public} is public"
        );
    }
    for special in [
        "0.0.0.0",
        "0.1.2.3",
        "10.1.2.3",
        "100.64.0.1",
        "100.127.255.254",
        "127.0.0.1",
        "169.254.169.254",
        "172.16.0.1",
        "172.31.255.255",
        "192.0.0.8",
        "192.0.2.1",
        "192.88.99.1",
        "192.168.1.1",
        "198.18.0.1",
        "198.19.255.255",
        "198.51.100.7",
        "203.0.113.9",
        "224.0.0.1",
        "239.255.255.250",
        "240.0.0.1",
        "255.255.255.255",
        "::",
        "::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::127.0.0.1",
        "64:ff9b::a00:1",
        "100::1",
        "fc00::1",
        "fd12:3456::1",
        "fe80::1",
        "fec0::1",
        "ff02::1",
        "2001::1",
        "2001:2::1",
        "2001:10::1",
        "2001:db8::1",
        "2002:a00:1::1",
        "3fff::1",
    ] {
        assert!(
            !is_public_address(special.parse().unwrap()),
            "{special} must be refused"
        );
    }
}

#[test]
fn a4_non_public_or_failed_resolution_refuses_the_whole_acquisition() {
    let fake = FakeGit::new("git version 2.40.1", "exit 1");
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    for (answers, expected) in [
        (
            vec![PUBLIC_V4, "10.0.0.7".parse().unwrap()],
            HOST_NOT_PUBLIC,
        ),
        (vec!["::ffff:127.0.0.1".parse().unwrap()], HOST_NOT_PUBLIC),
        (vec!["169.254.169.254".parse().unwrap()], HOST_NOT_PUBLIC),
        (vec![PUBLIC_V6, "fd00::1".parse().unwrap()], HOST_NOT_PUBLIC),
        (Vec::new(), RESOLUTION_FAILED),
    ] {
        let acquirer = GitAcquirer::system()
            .with_program(fake.program())
            .with_resolver(Arc::new(Fixed(answers.clone())));
        let rev = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            code(writer.acquire_git_with(&acquirer, URL, rev)),
            expected,
            "{answers:?}"
        );
    }
    let offline = GitAcquirer::system()
        .with_program(fake.program())
        .with_resolver(Arc::new(Offline));
    assert_eq!(
        code(writer.acquire_git_with(&offline, URL, "0123456789abcdef0123456789abcdef01234567")),
        RESOLUTION_FAILED
    );
    // The probe ran, but no fetch was ever attempted for a refused answer set.
    assert!(!fake.invocations("--version").is_empty());
    assert!(fake.invocations("fetch").is_empty());
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_invalid_grammar_refuses_before_permit_quarantine_or_process() {
    let fake = FakeGit::new("git version 2.40.1", "exit 1");
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let acquirer = GitAcquirer::system()
        .with_program(fake.program())
        .with_resolver(Arc::new(Fixed(vec![PUBLIC_V4])));
    for (url, revision) in [
        ("https://github.com/example/repo", "main"),
        (
            "https://token@github.com/example/repo",
            "0123456789abcdef0123456789abcdef01234567",
        ),
        (
            "ssh://git@github.com/example/repo",
            "0123456789abcdef0123456789abcdef01234567",
        ),
    ] {
        assert_eq!(
            code(writer.acquire_git_with(&acquirer, url, revision)),
            INVALID_GIT_SOURCE
        );
    }
    assert_eq!(
        fs::read_dir(fake.log()).unwrap().count(),
        0,
        "no process ran"
    );
    assert_eq!(fs::read_dir(config.path()).unwrap().count(), 0);
}

// ---------------------------------------------------------------------------
// Capability, argv, environment, and pinning.
// ---------------------------------------------------------------------------

#[test]
fn a4_git_capability_fails_closed_without_pinning_support() {
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let rev = "0123456789abcdef0123456789abcdef01234567";
    let with = |program: PathBuf| {
        GitAcquirer::system()
            .with_program(program)
            .with_resolver(Arc::new(Offline))
    };
    for version in [
        "git version 2.36.9",
        "git version 2.36.99 (Apple Git-140)",
        "git version 1.99.0",
        "git version two.forty",
        "garbage",
        "",
    ] {
        let fake = FakeGit::new(version, "exit 1");
        assert_eq!(
            code(writer.acquire_git_with(&with(fake.program()), URL, rev)),
            GIT_UNAVAILABLE,
            "{version:?}"
        );
    }
    // Exactly the first pinning release passes the probe (and then stops at
    // the offline resolver, proving the probe was the only gate).
    for version in ["git version 2.37.0", "git version 2.50.1 (Apple Git-155)"] {
        let fake = FakeGit::new(version, "exit 1");
        assert_eq!(
            code(writer.acquire_git_with(&with(fake.program()), URL, rev)),
            RESOLUTION_FAILED,
            "{version:?}"
        );
    }
    // No HTTPS helper in the exec path.
    let fake = FakeGit::new("git version 2.40.1", "exit 1");
    fs::remove_file(fake.temp.path().join("libexec/git-remote-https")).unwrap();
    assert_eq!(
        code(writer.acquire_git_with(&with(fake.program()), URL, rev)),
        GIT_UNAVAILABLE
    );
    // Missing tool.
    assert_eq!(
        code(writer.acquire_git_with(&with(config.path().join("no-such-git")), URL, rev)),
        GIT_UNAVAILABLE
    );
    assert_eq!(parse_git_version("git version 2.37.0"), Some((2, 37, 0)));
    assert_eq!(
        parse_git_version("git version 2.45.2.windows.1"),
        Some((2, 45, 2))
    );
    assert_eq!(parse_git_version("git version 2.37.rc0"), Some((2, 37, 0)));
    assert_eq!(parse_git_version("git 2.40.0"), None);
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_every_git_process_gets_a_stripped_environment_and_one_checked_pin() {
    let fake = FakeGit::new("git version 2.40.1", "exit 1");
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let acquirer = GitAcquirer::system()
        .with_program(fake.program())
        .with_resolver(Arc::new(Fixed(vec![PUBLIC_V6, PUBLIC_V4, PUBLIC_V4])));
    let rev = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(
        code(writer.acquire_git_with(&acquirer, URL, rev)),
        FETCH_FAILED
    );

    let fetches = fake.invocations("fetch");
    // Deduplicated set, one fresh pinned process per checked address.
    assert_eq!(fetches.len(), 2, "{fetches:?}");
    let mut pins = BTreeSet::new();
    for (argv, _) in &fetches {
        let pinned: Vec<&String> = argv
            .iter()
            .filter(|arg| arg.starts_with("http.curloptResolve"))
            .collect();
        assert_eq!(pinned.len(), 1, "exactly one pin per process: {argv:?}");
        pins.insert(pinned[0].clone());
        for required in [
            "protocol.allow=never",
            "protocol.https.allow=always",
            "http.followRedirects=false",
            "http.proxy=",
            "remote.origin.proxy=",
            "http.sslVerify=true",
            "http.extraHeader=",
            "credential.helper=",
            "core.askPass=/usr/bin/false",
            "core.hooksPath=/dev/null",
            "fetch.recurseSubmodules=false",
            "fetch.fsckObjects=true",
            "transfer.bundleURI=false",
            "fetch.uriProtocols=",
            "filter.lfs.smudge=",
            "--no-tags",
            "--no-recurse-submodules",
            "--depth=1",
        ] {
            assert!(
                argv.iter().any(|arg| arg == required),
                "{required} missing from {argv:?}"
            );
        }
        assert!(!argv.iter().any(|arg| arg.starts_with("protocol.file")
            || arg.starts_with("protocol.http.")
            || arg.starts_with("protocol.ssh")));
        // The URL keeps the hostname (TLS SNI and certificate checks are
        // unchanged); the exact object id follows it.
        assert_eq!(&argv[argv.len() - 2..], [URL.to_string(), rev.to_string()]);
    }
    assert_eq!(
        pins,
        BTreeSet::from([
            "http.curloptResolve=git.ocean-fixture.com:443:93.184.216.34".to_string(),
            "http.curloptResolve=git.ocean-fixture.com:443:[2606:2800:220:1:248:1893:25c8:1946]"
                .to_string(),
        ])
    );

    // Local-only commands (probe, init) may use no transport at all.
    for (argv, _) in [fake.invocations("--version"), fake.invocations("init")].concat() {
        assert!(
            argv.iter()
                .filter(|arg| arg.starts_with("protocol."))
                .all(|arg| arg == "protocol.allow=never"),
            "{argv:?}"
        );
        assert!(!argv
            .iter()
            .any(|arg| arg.starts_with("http.curloptResolve")));
    }

    // Every process, probe included, saw exactly the fixed environment.
    let every = [
        fake.invocations("--version"),
        fake.invocations("init"),
        fetches,
    ]
    .concat();
    for (argv, env) in every {
        let mut names: Vec<&str> = env
            .iter()
            .filter_map(|line| line.split_once('=').map(|(name, _)| name))
            // The shell itself may export these into `env`'s view.
            .filter(|name| !matches!(*name, "PWD" | "SHLVL" | "OLDPWD" | "_"))
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "GIT_ASKPASS",
                "GIT_CONFIG_GLOBAL",
                "GIT_CONFIG_NOSYSTEM",
                "GIT_NO_REPLACE_OBJECTS",
                "GIT_OPTIONAL_LOCKS",
                "GIT_PROTOCOL_FROM_USER",
                "GIT_TERMINAL_PROMPT",
                "HOME",
                "LC_ALL",
                "PATH",
            ],
            "{argv:?}"
        );
        for exact in [
            "PATH=/usr/bin:/bin",
            "GIT_CONFIG_NOSYSTEM=1",
            "GIT_CONFIG_GLOBAL=/dev/null",
            "GIT_TERMINAL_PROMPT=0",
            "GIT_ASKPASS=/usr/bin/false",
            "GIT_OPTIONAL_LOCKS=0",
        ] {
            assert!(env.iter().any(|line| line == exact), "{exact}");
        }
        let home = env
            .iter()
            .find_map(|line| line.strip_prefix("HOME="))
            .unwrap();
        assert!(home.ends_with("/fetch/home"), "{home}");
        assert!(home.contains("/quarantine/"), "{home}");
    }
    assert_no_acquisition_residue(config.path());
}

/// Records every connection a pinned `git` makes to a loopback listener.
struct Listener {
    port: u16,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Listener {
    fn start(respond: impl Fn(u16, &[u8]) -> Vec<u8> + Send + 'static) -> Self {
        Self::start_on("127.0.0.1:0", respond).unwrap()
    }

    fn start_on(
        address: &str,
        respond: impl Fn(u16, &[u8]) -> Vec<u8> + Send + 'static,
    ) -> io::Result<Self> {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind(address)?;
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (seen, halt) = (Arc::clone(&requests), Arc::clone(&stop));
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(60);
            while !halt.load(Ordering::SeqCst) && Instant::now() < until {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 16 * 1024];
                while let Ok(count) = stream.read(&mut buffer) {
                    if count == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    // An HTTP head, or a TLS record (ClientHello) that
                    // carries SNI in its first flight.
                    if request.windows(4).any(|window| window == b"\r\n\r\n")
                        || request.first() == Some(&0x16)
                    {
                        break;
                    }
                }
                let response = respond(port, &request);
                seen.lock().unwrap().push(request);
                let _ = stream.write_all(&response);
            }
        });
        Ok(Self {
            port,
            requests,
            stop,
        })
    }

    fn requests(&self) -> Vec<Vec<u8>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

const PINNED_HOST: &str = "pinned.ocean-a4.invalid";

/// Run one real pinned fetch attempt through the production process path.
fn pinned_attempt(scheme: &str, port: u16, pin: Option<IpAddr>) -> Step<()> {
    let pin = pin.map(|address| pin_entry(PINNED_HOST, port, address));
    fetch_once(
        &format!("{scheme}://{PINNED_HOST}:{port}/ocean/noop.git"),
        scheme,
        pin.as_deref(),
        "0123456789abcdef0123456789abcdef01234567",
    )
}

/// One real `fetch_attempt` through the production process path.
fn fetch_once(target: &str, scheme: &str, pin: Option<&str>, revision: &str) -> Step<()> {
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let lease = writer.begin_acquisition().unwrap();
    let work = Workspace::create(&lease).unwrap();
    let acquirer = GitAcquirer::system().with_program(host_git());
    let deadline = Instant::now() + Duration::from_secs(30);
    let program = acquirer.probe(&work, deadline).unwrap();
    acquirer.fetch_attempt(&program, &work, target, scheme, pin, revision, deadline)
}

fn head_has(request: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(request)
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

#[test]
fn a4_pinned_connection_reaches_only_the_checked_address() {
    let not_found = |_: u16, _: &[u8]| {
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    };
    let listener = Listener::start(not_found);
    // `.invalid` never resolves: any connection proves the pin was used.
    assert!(matches!(
        pinned_attempt("http", listener.port, Some(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        Err(Fail::Reject(FETCH_FAILED))
    ));
    let requests = listener.requests();
    assert!(
        !requests.is_empty(),
        "the pinned address was never contacted"
    );
    for request in &requests {
        assert!(
            head_has(request, &format!("host: {PINNED_HOST}:{}", listener.port)),
            "the hostname stays the request authority: {}",
            String::from_utf8_lossy(request)
        );
    }

    // Control: without the pin nothing can reach the listener.
    let unpinned = Listener::start(not_found);
    assert!(pinned_attempt("http", unpinned.port, None).is_err());
    assert!(unpinned.requests().is_empty());
}

#[test]
fn a4_pinned_https_keeps_hostname_sni_and_tls() {
    let listener = Listener::start(|_, _| Vec::new());
    assert!(matches!(
        pinned_attempt(
            "https",
            listener.port,
            Some(IpAddr::V4(Ipv4Addr::LOCALHOST))
        ),
        Err(Fail::Reject(FETCH_FAILED))
    ));
    let requests = listener.requests();
    assert!(
        !requests.is_empty(),
        "the pinned address was never contacted"
    );
    for hello in &requests {
        assert_eq!(hello.first(), Some(&0x16), "TLS handshake, not plaintext");
        assert!(
            hello
                .windows(PINNED_HOST.len())
                .any(|window| window == PINNED_HOST.as_bytes()),
            "ClientHello must carry the hostname as SNI"
        );
    }
}

#[test]
fn a4_redirects_are_never_followed() {
    let listener = Listener::start(|port, _| {
        format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{PINNED_HOST}:{port}/moved/noop.git/info/refs?service=git-upload-pack\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes()
    });
    assert!(pinned_attempt("http", listener.port, Some(IpAddr::V4(Ipv4Addr::LOCALHOST))).is_err());
    let requests = listener.requests();
    assert!(!requests.is_empty());
    assert!(
        !requests.iter().any(|request| head_has(request, "/moved/")),
        "a redirect was followed"
    );
}

#[test]
fn a4_credential_challenges_are_never_answered() {
    let listener = Listener::start(|_, _| {
        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"ocean\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    });
    assert!(matches!(
        pinned_attempt("http", listener.port, Some(IpAddr::V4(Ipv4Addr::LOCALHOST))),
        Err(Fail::Reject(FETCH_FAILED))
    ));
    let requests = listener.requests();
    assert!(!requests.is_empty());
    assert!(
        !requests
            .iter()
            .any(|request| head_has(request, "authorization:")),
        "a credential was sent"
    );
}

// ---------------------------------------------------------------------------
// Process groups, deadline, and size bounds.
// ---------------------------------------------------------------------------

#[test]
fn a4_timeout_kills_the_whole_git_process_group() {
    let fake = FakeGit::new(
        "git version 2.40.1",
        "sleep 300 & echo $! > \"$LOG/grandchild.pid\"; sleep 300",
    );
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let acquirer = GitAcquirer::system()
        .with_program(fake.program())
        .with_resolver(Arc::new(Fixed(vec![PUBLIC_V4])))
        .with_deadline(Duration::from_secs(2));
    let started = Instant::now();
    assert_eq!(
        code(writer.acquire_git_with(&acquirer, URL, "0123456789abcdef0123456789abcdef01234567")),
        TIMEOUT
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(
        pid_gone(fake.grandchild()),
        "grandchild survived the timeout"
    );
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_a_leader_exit_never_leaves_a_surviving_descendant() {
    let fake = FakeGit::new(
        "git version 2.40.1",
        "sleep 300 & echo $! > \"$LOG/grandchild.pid\"; exit 0",
    );
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let acquirer = GitAcquirer::system()
        .with_program(fake.program())
        .with_resolver(Arc::new(Fixed(vec![PUBLIC_V4])));
    // The "successful" fetch wrote no FETCH_HEAD, so verification refuses it;
    // the orphaned sleeper was killed while the zombie leader pinned its group.
    assert_eq!(
        code(writer.acquire_git_with(&acquirer, URL, "0123456789abcdef0123456789abcdef01234567")),
        REVISION_MISMATCH
    );
    assert!(
        pid_gone(fake.grandchild()),
        "grandchild outlived its leader"
    );
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_temp_ceiling_is_enforced_while_git_runs() {
    let fake = FakeGit::new(
        "git version 2.40.1",
        "while :; do head -c 65536 /dev/zero >> big; sleep 0.02; done",
    );
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let acquirer = GitAcquirer::system()
        .with_program(fake.program())
        .with_resolver(Arc::new(Fixed(vec![PUBLIC_V4])))
        .with_temp_ceiling(1024 * 1024)
        .with_deadline(Duration::from_secs(30));
    let started = Instant::now();
    assert_eq!(
        code(writer.acquire_git_with(&acquirer, URL, "0123456789abcdef0123456789abcdef01234567")),
        LIMIT
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "ceiling was not live"
    );
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_object_ceiling_applies_to_a_real_fetch() {
    let remote = Remote::new();
    remote.package();
    let mut noise = vec![0u8; 512 * 1024];
    for (index, byte) in noise.iter_mut().enumerate() {
        *byte = (index.wrapping_mul(2_654_435_761) >> 13) as u8;
    }
    remote.write("assets/noise.bin", &noise, false);
    let rev = remote.commit();
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let tight = acquirer(&remote).with_temp_ceiling(128 * 1024);
    assert_eq!(code(writer.acquire_git_with(&tight, URL, &rev)), LIMIT);
    assert_no_acquisition_residue(config.path());
    // The same source fits the production ceiling.
    assert_eq!(
        code(writer.acquire_git_with(&acquirer(&remote), URL, &rev)),
        "ok"
    );
    remote.assert_no_canary_ran();
}

// ---------------------------------------------------------------------------
// Real fetch, verification, and extraction.
// ---------------------------------------------------------------------------

#[test]
fn a4_git_install_matches_local_digest_and_records_the_exact_source() {
    let remote = Remote::new();
    remote.package();
    remote.write("docs/nested/deep/readme.txt", b"hello\n", false);
    remote.write(".gitattributes", b"*.txt text eol=lf\n", false);
    let rev = remote.commit();
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());

    let quarantine = writer
        .acquire_git_with(&acquirer(&remote), URL, &rev)
        .unwrap();
    let digest = quarantine.digest().to_string();
    let local = writer
        .acquire_local(remote.tree().to_str().unwrap())
        .unwrap();
    assert_eq!(
        local.digest(),
        digest,
        "Git and local bytes hash identically"
    );
    drop(local);

    let outcome = writer.install(0, quarantine).unwrap();
    assert!(outcome.committed);
    let state = read_locked_state(config.path()).unwrap().snapshot;
    let install = state.installs.iter().find(|row| row.id == ID).unwrap();
    assert_eq!(install.digest, digest);
    assert!(matches!(install.source.kind, InstallSourceKind::Git));
    assert_eq!(install.source.locator, URL);
    assert_eq!(install.source.revision.as_deref(), Some(rev.as_str()));
    // Install grants nothing.
    assert!(state.grants.iter().all(|grant| grant.id != ID));
    assert!(state.service_grants.iter().all(|grant| grant.id != ID));
    // The stored artifact carries the executable bits git recorded and no
    // Git metadata.
    let hex = digest_hex(&digest).unwrap();
    let artifact = config.path().join("extensions/store").join(ID).join(hex);
    let mode = fs::metadata(artifact.join("services/lifecycle"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(!artifact.join(".git").exists());
    assert_eq!(
        fs::read(artifact.join("docs/nested/deep/readme.txt")).unwrap(),
        b"hello\n"
    );
    remote.assert_no_canary_ran();
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_sha256_repositories_fetch_the_exact_64_hex_commit() {
    let remote = Remote::with_format("sha256");
    remote.package();
    let rev = remote.commit();
    assert_eq!(rev.len(), 64);
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let quarantine = writer
        .acquire_git_with(&acquirer(&remote), URL, &rev)
        .unwrap();
    let local = writer
        .acquire_local(remote.tree().to_str().unwrap())
        .unwrap();
    assert_eq!(quarantine.digest(), local.digest());
    drop((quarantine, local));
    // A 40-hex id cannot name an object in a SHA-256 repository.
    assert_eq!(
        code(writer.acquire_git_with(&acquirer(&remote), URL, &rev[..40])),
        FETCH_FAILED
    );
    remote.assert_no_canary_ran();
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_revision_must_name_the_exact_fetched_commit() {
    let remote = Remote::new();
    remote.package();
    let rev = remote.commit();
    let tree = remote.tree_git(&["rev-parse", &format!("{rev}^{{tree}}")]);
    let blob = remote.tree_git(&["rev-parse", &format!("{rev}:ocean-extension.toml")]);
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    for object in [&tree, &blob] {
        let result = code(writer.acquire_git_with(&acquirer(&remote), URL, object));
        assert!(
            matches!(result, REVISION_MISMATCH | FETCH_FAILED),
            "a non-commit object id must be refused, got {result}"
        );
    }
    assert_eq!(
        code(writer.acquire_git_with(&acquirer(&remote), URL, &tree)),
        REVISION_MISMATCH,
        "a fetched tree is not a commit"
    );
    let absent = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(
        code(writer.acquire_git_with(&acquirer(&remote), URL, absent)),
        FETCH_FAILED
    );
    remote.assert_no_canary_ran();
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_symlinks_submodules_filters_and_unsafe_trees_are_refused() {
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let refused = |prepare: &dyn Fn(&Remote)| {
        let remote = Remote::new();
        remote.package();
        prepare(&remote);
        let rev = remote.commit();
        let result = code(writer.acquire_git_with(&acquirer(&remote), URL, &rev));
        remote.assert_no_canary_ran();
        result
    };

    // Symlink entry (mode 120000), even one pointing inside the tree.
    assert_eq!(
        refused(
            &|remote| std::os::unix::fs::symlink("/etc/passwd", remote.tree().join("escape"))
                .unwrap()
        ),
        TREE_UNSUPPORTED
    );
    assert_eq!(
        refused(
            &|remote| std::os::unix::fs::symlink("run-me", remote.tree().join("alias")).unwrap()
        ),
        TREE_UNSUPPORTED
    );
    // Gitlink (submodule, mode 160000) with and without `.gitmodules`.
    assert_eq!(
        refused(&|remote| *remote.gitlink.borrow_mut() = Some("vendor/lib".into())),
        TREE_UNSUPPORTED
    );
    assert_eq!(
        refused(&|remote| remote.write(
            ".gitmodules",
            b"[submodule \"x\"]\n\tpath = x\n\turl = https://example.com/x\n",
            false
        )),
        TREE_UNSUPPORTED
    );
    // Git LFS and any other filter driver, directly or through a macro.
    for attributes in [
        &b"*.bin filter=lfs diff=lfs merge=lfs -text\n"[..],
        b"[attr]big filter=lfs\n*.bin big\n",
        b"# comment\n*.txt  filter=secret-smudge\n",
    ] {
        assert_eq!(
            refused(&|remote| remote.write("assets/.gitattributes", attributes, false)),
            TREE_UNSUPPORTED
        );
    }
    assert_eq!(
        refused(&|remote| remote.write(".lfsconfig", b"[lfs]\n", false)),
        TREE_UNSUPPORTED
    );
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_crafted_tree_paths_never_escape_the_artifact() {
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    for hostile in ["..", ".git", ".GIT", "a\u{1}b"] {
        let remote = Remote::new();
        remote.package();
        let rev = remote.commit();
        let blob = remote.tree_git(&["rev-parse", &format!("{rev}:run-me")]);
        let root = remote.tree_git(&["rev-parse", &format!("{rev}^{{tree}}")]);
        // Graft one hostile entry beside the valid package entries.
        let listing = remote.tree_git(&["ls-tree", &root]);
        let crafted = format!("{listing}\n100644 blob {blob}\t{hostile}\n");
        let mut mktree = Command::new(host_git())
            .arg(format!("--git-dir={}", remote.repo().display()))
            .args(["mktree", "--missing"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        mktree
            .stdin
            .take()
            .unwrap()
            .write_all(crafted.as_bytes())
            .unwrap();
        let output = mktree.wait_with_output().unwrap();
        if !output.status.success() {
            continue; // this git refuses to even build the tree
        }
        let tree = String::from_utf8(output.stdout).unwrap().trim().to_string();
        let commit = remote.git(&[
            &format!("--git-dir={}", remote.repo().display()),
            "commit-tree",
            &tree,
            "-m",
            "hostile",
        ]);
        let result = code(writer.acquire_git_with(&acquirer(&remote), URL, &commit));
        assert!(
            matches!(result, TREE_UNSUPPORTED | FETCH_FAILED),
            "{hostile:?} must be refused, got {result}"
        );
        remote.assert_no_canary_ran();
    }
    assert!(!config.path().join("..").join("run-me").exists());
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_package_limits_apply_to_the_listing_before_extraction() {
    let remote = Remote::new();
    remote.package();
    remote.write("a/b/c/d/e.txt", b"deep\n", false);
    let rev = remote.commit();
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    for limited in [
        acquirer(&remote).with_limits(4, MAX_PACKAGE_DEPTH, MAX_PACKAGE_BYTES),
        acquirer(&remote).with_limits(MAX_PACKAGE_ENTRIES, 3, MAX_PACKAGE_BYTES),
        acquirer(&remote).with_limits(MAX_PACKAGE_ENTRIES, MAX_PACKAGE_DEPTH, 64),
    ] {
        assert_eq!(
            code(writer.acquire_git_with(&limited, URL, &rev)),
            PACKAGE_INVALID
        );
    }
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_git_update_rollback_and_failed_update_keep_trust_cleared() {
    struct Stopped;
    impl ServiceActivity for Stopped {
        fn package_stopped(&self, _: &str) -> bool {
            true
        }
    }
    let remote = Remote::new();
    remote.package();
    let first = remote.commit();
    remote.write("CHANGELOG", b"two\n", false);
    let second = remote.commit();
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let git = acquirer(&remote);

    let installed = writer
        .install(0, writer.acquire_git_with(&git, URL, &first).unwrap())
        .unwrap();
    let updated = writer
        .update(
            ID,
            installed.state_revision,
            writer.acquire_git_with(&git, URL, &second).unwrap(),
            &Stopped,
        )
        .unwrap();
    assert_ne!(updated.digest, installed.digest);

    // A failed Git update (absent commit) is pre-commit: nothing changes.
    let absent = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(
        code(writer.acquire_git_with(&git, URL, absent)),
        FETCH_FAILED
    );
    assert_eq!(
        read_locked_state(config.path()).unwrap().snapshot.revision,
        updated.state_revision
    );

    // Explicit rollback to the first exact revision re-derives the first
    // digest, retained as an unreferenced payload, and is untrusted.
    let rolled_back = writer
        .update(
            ID,
            updated.state_revision,
            writer.acquire_git_with(&git, URL, &first).unwrap(),
            &Stopped,
        )
        .unwrap();
    assert_eq!(rolled_back.digest, installed.digest);
    let state = read_locked_state(config.path()).unwrap().snapshot;
    let install = state.installs.iter().find(|row| row.id == ID).unwrap();
    assert_eq!(install.source.revision.as_deref(), Some(first.as_str()));
    assert!(state.grants.iter().all(|grant| grant.id != ID));
    assert!(state.service_grants.iter().all(|grant| grant.id != ID));
    remote.assert_no_canary_ran();
    assert_no_acquisition_residue(config.path());
}

/// The separately recorded exact public-commit smoke (§19.4): the real
/// system resolver and host `git` fetch one pinned public commit over HTTPS.
/// Run explicitly: `cargo test -p ocean-daemon a4_public_commit_smoke --
/// --ignored`. octocat/Hello-World is not an Ocean package, so this stops
/// after extraction instead of sealing.
#[test]
#[ignore = "network: exact public-commit smoke"]
fn a4_public_commit_smoke() {
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let source = parse_git_source(
        "https://github.com/octocat/Hello-World.git",
        "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d",
    )
    .unwrap();
    let acquirer = GitAcquirer::system();
    let mut lease = writer.begin_acquisition().unwrap();
    let deadline = Instant::now() + ACQUISITION_DEADLINE;
    acquirer
        .fill(&mut lease, &source, deadline)
        .unwrap_or_else(|fail| panic!("public smoke failed: {fail:?}"));
    let readme = open_regular_file_at(&lease.artifact, OsStr::new("README"), "README").unwrap();
    assert!(readme.metadata().unwrap().len() > 0);
}

// ---------------------------------------------------------------------------
// Knox review follow-ups.
// ---------------------------------------------------------------------------

/// Build a tree object from `ls-tree`-format lines in `remote`.
fn mktree(remote: &Remote, listing: &str) -> String {
    let mut child = Command::new(host_git())
        .arg(format!("--git-dir={}", remote.repo().display()))
        .args(["mktree", "--missing"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(listing.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "mktree refused {listing:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn a4_case_and_normalization_aliases_are_refused_on_every_host() {
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let aliased = |extra: &dyn Fn(&Remote, &str) -> String| {
        let remote = Remote::new();
        remote.package();
        let rev = remote.commit();
        let root = remote.tree_git(&["rev-parse", &format!("{rev}^{{tree}}")]);
        let blob = remote.tree_git(&["rev-parse", &format!("{rev}:run-me")]);
        let listing = remote.tree_git(&["ls-tree", &root]);
        let tree = mktree(&remote, &format!("{listing}\n{}", extra(&remote, &blob)));
        let commit = remote.git(&[
            &format!("--git-dir={}", remote.repo().display()),
            "commit-tree",
            &tree,
            "-m",
            "aliased",
        ]);
        let result = code(writer.acquire_git_with(&acquirer(&remote), URL, &commit));
        remote.assert_no_canary_ran();
        result
    };
    // README + readme.
    assert_eq!(
        aliased(&|_, blob| format!("100644 blob {blob}\tREADME\n100644 blob {blob}\treadme\n")),
        TREE_UNSUPPORTED
    );
    // café in NFC and in NFD.
    assert_eq!(
        aliased(&|_, blob| format!(
            "100644 blob {blob}\tcaf\u{e9}\n100644 blob {blob}\tcafe\u{301}\n"
        )),
        TREE_UNSUPPORTED
    );
    // DIR/ + dir/ holding different subtrees.
    assert_eq!(
        aliased(&|remote, blob| {
            let upper = mktree(remote, &format!("100644 blob {blob}\tone\n"));
            let lower = mktree(remote, &format!("100755 blob {blob}\ttwo\n"));
            format!("040000 tree {upper}\tDIR\n040000 tree {lower}\tdir\n")
        }),
        TREE_UNSUPPORTED
    );
    // A file and a directory that fold together.
    assert_eq!(
        aliased(&|remote, blob| {
            let sub = mktree(remote, &format!("100644 blob {blob}\tinner\n"));
            format!("100644 blob {blob}\tDocs\n040000 tree {sub}\tdocs\n")
        }),
        TREE_UNSUPPORTED
    );
    assert_no_acquisition_residue(config.path());
    assert_eq!(fold_component("Caf\u{c9}"), fold_component("cafe\u{301}"));
    assert_ne!(fold_component("a"), fold_component("b"));
}

/// The filesystem-level guard beneath the fold check: a directory the writer
/// did not create itself is never merged into, which is what a case-folding
/// or normalizing filesystem presents for `DIR/` + `dir/`.
#[test]
fn a4_tree_writer_never_merges_into_a_directory_it_did_not_create() {
    let root = tempfile::tempdir().unwrap();
    let artifact = File::open(root.path()).unwrap();

    // Deterministic on every filesystem: a pre-existing directory.
    fs::create_dir(root.path().join("planted")).unwrap();
    let mut writer = TreeWriter::new(&artifact);
    assert!(matches!(
        writer.create("planted/x", 0o644),
        Err(Fail::Reject(TREE_UNSUPPORTED))
    ));

    // The same directory it created is re-entered freely.
    let mut writer = TreeWriter::new(&artifact);
    writer.create("DIR/one", 0o644).unwrap();
    writer.create("other/x", 0o644).unwrap();
    writer.create("DIR/three", 0o644).unwrap();

    // DIR/ then dir/: on a case-insensitive filesystem `dir` already exists
    // but was never created under that spelling, so it is refused.
    let probe = root.path().join("CaseProbe");
    fs::write(&probe, b"").unwrap();
    let case_insensitive = root.path().join("caseprobe").exists();
    let result = writer.create("dir/two", 0o644);
    if case_insensitive {
        assert!(matches!(result, Err(Fail::Reject(TREE_UNSUPPORTED))));
    } else {
        assert!(result.is_ok());
    }
}

#[test]
fn a4_safe_component_refuses_git_metadata_names_in_any_case() {
    for refused in [
        ".git",
        ".GIT",
        ".Git",
        ".gitmodules",
        ".GITMODULES",
        ".lfsconfig",
        ".LFSCONFIG",
        "",
        ".",
        "..",
        "a\u{1}b",
        "tab\there",
    ] {
        assert!(!safe_component(refused), "{refused:?}");
    }
    assert!(!safe_component(&"x".repeat(256)));
    for allowed in [
        ".github",
        "gitmodules",
        ".gitignore",
        "a",
        "café",
        &"x".repeat(255),
    ] {
        assert!(safe_component(allowed), "{allowed:?}");
    }
}

/// A fake git that completes the whole flow for one manifest-only package:
/// `fetch_head` is the shell that writes `$GD/FETCH_HEAD` (`$last` is the
/// requested id) and `header_size` overrides the `cat-file --batch` size.
fn complete_fake(fetch_head: &str, header_size: Option<usize>) -> FakeGit {
    let manifest = "schema_version = 1\nid = \"example.noop\"\nname = \"Noop\"\nversion = \"1.0.0\"\nmin_ocean_version = \"0.1.0\"\n";
    let oid = "ab".repeat(20);
    let size = manifest.len();
    let arms = format!(
        "  *' cat-file -t '*) printf 'commit\\n'; exit 0 ;;\n  *' ls-tree '*) printf '100644 blob %s %s\\tocean-extension.toml\\000' '{oid}' '{size}'; exit 0 ;;\n  *' --batch '*) printf '%s blob %s\\n' '{oid}' '{header}'; cat '{{FAKE}}/manifest.toml'; printf '\\n'; exit 0 ;;",
        header = header_size.unwrap_or(size),
    );
    let fake = FakeGit::scripted(
        "git version 2.40.1",
        &format!("for last in \"$@\"; do :; done; {fetch_head}; exit 0"),
        &arms,
    );
    fs::write(fake.temp.path().join("manifest.toml"), manifest).unwrap();
    fake
}

fn fake_acquirer(fake: &FakeGit) -> GitAcquirer {
    GitAcquirer::system()
        .with_program(fake.program())
        .with_resolver(Arc::new(Fixed(vec![PUBLIC_V4])))
}

const FAKE_REV: &str = "0123456789abcdef0123456789abcdef01234567";

#[test]
fn a4_fetch_head_must_name_exactly_one_line() {
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    // Control: one exact line lets the scripted flow install.
    let exact = complete_fake(r#"printf '%s\t\tx\n' "$last" > "$GD/FETCH_HEAD""#, None);
    assert_eq!(
        code(writer.acquire_git_with(&fake_acquirer(&exact), URL, FAKE_REV)),
        "ok"
    );
    for fetch_head in [
        r#"printf '%s\t\tx\n%s\t\ty\n' "$last" "$last" > "$GD/FETCH_HEAD""#,
        r#"printf '%s\t\tx\n' 1111111111111111111111111111111111111111 > "$GD/FETCH_HEAD""#,
        r#"printf '%sX\n' "$last" > "$GD/FETCH_HEAD""#,
        ":",
    ] {
        let fake = complete_fake(fetch_head, None);
        assert_eq!(
            code(writer.acquire_git_with(&fake_acquirer(&fake), URL, FAKE_REV)),
            REVISION_MISMATCH,
            "{fetch_head}"
        );
    }
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_blob_headers_must_match_the_listing() {
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let fetch_head = r#"printf '%s\t\tx\n' "$last" > "$GD/FETCH_HEAD""#;
    let manifest_len = complete_fake(fetch_head, None);
    let size = fs::metadata(manifest_len.temp.path().join("manifest.toml"))
        .unwrap()
        .len() as usize;
    // A header one byte long, whose stream is otherwise self-consistent with
    // the listing, must still be refused.
    for wrong in [size + 1, size - 1] {
        let fake = complete_fake(fetch_head, Some(wrong));
        assert_eq!(
            code(writer.acquire_git_with(&fake_acquirer(&fake), URL, FAKE_REV)),
            PACKAGE_INVALID,
            "header size {wrong}"
        );
    }
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_a_failing_type_probe_is_a_revision_mismatch() {
    let fake = FakeGit::scripted(
        "git version 2.40.1",
        r#"for last in "$@"; do :; done; printf '%s\t\tx\n' "$last" > "$GD/FETCH_HEAD"; exit 0"#,
        "  *' cat-file -t '*) exit 128 ;;",
    );
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    assert_eq!(
        code(writer.acquire_git_with(&fake_acquirer(&fake), URL, FAKE_REV)),
        REVISION_MISMATCH
    );
}

#[test]
fn a4_tree_bombs_are_killed_while_ls_tree_streams() {
    let fetch =
        r#"for last in "$@"; do :; done; printf '%s\t\tx\n' "$last" > "$GD/FETCH_HEAD"; exit 0"#;
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let oid = "ab".repeat(20);
    // Endless records: stopped at the record cap.
    let records = FakeGit::scripted(
        "git version 2.40.1",
        fetch,
        &format!(
            "  *' cat-file -t '*) printf 'commit\\n'; exit 0 ;;\n  *' ls-tree '*) i=0; while :; do i=$((i+1)); printf '100644 blob {oid} 1\\tf%s\\000' \"$i\"; done ;;"
        ),
    );
    let started = Instant::now();
    assert_eq!(
        code(
            writer.acquire_git_with(
                &fake_acquirer(&records)
                    .with_limits(50, MAX_PACKAGE_DEPTH, MAX_PACKAGE_BYTES)
                    .with_deadline(Duration::from_secs(20)),
                URL,
                FAKE_REV
            )
        ),
        PACKAGE_INVALID
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    // Endless bytes and no record separator: stopped at the byte cap.
    let bytes = FakeGit::scripted(
        "git version 2.40.1",
        fetch,
        "  *' cat-file -t '*) printf 'commit\\n'; exit 0 ;;\n  *' ls-tree '*) yes | tr -d '\\n' ;;",
    );
    let started = Instant::now();
    assert_eq!(
        code(writer.acquire_git_with(
            &fake_acquirer(&bytes).with_deadline(Duration::from_secs(30)),
            URL,
            FAKE_REV
        )),
        PACKAGE_INVALID
    );
    assert!(started.elapsed() < Duration::from_secs(25));
    assert_no_acquisition_residue(config.path());
}

#[test]
fn a4_a_file_remote_is_refused_under_the_https_protocol_allowance() {
    let remote = Remote::new();
    remote.package();
    let rev = remote.commit();
    let target = format!("file://{}", remote.repo().display());
    assert!(matches!(
        fetch_once(&target, "https", None, &rev),
        Err(Fail::Reject(FETCH_FAILED))
    ));
    // Control: the same fetch succeeds only when `file` is the allowed one.
    assert!(fetch_once(&target, "file", None, &rev).is_ok());
}

#[test]
fn a4_pinned_ipv6_connection_reaches_only_the_checked_address() {
    let not_found = |_: u16, _: &[u8]| {
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()
    };
    let Ok(listener) = Listener::start_on("[::1]:0", not_found) else {
        eprintln!("skipping: IPv6 loopback is unavailable on this host");
        return;
    };
    assert!(matches!(
        pinned_attempt("http", listener.port, Some(IpAddr::V6(Ipv6Addr::LOCALHOST))),
        Err(Fail::Reject(FETCH_FAILED))
    ));
    let requests = listener.requests();
    assert!(
        !requests.is_empty(),
        "the pinned [::1] address was never contacted"
    );
    for request in &requests {
        assert!(head_has(
            request,
            &format!("host: {PINNED_HOST}:{}", listener.port)
        ));
    }
    let unpinned = Listener::start_on("[::1]:0", not_found).unwrap();
    assert!(pinned_attempt("http", unpinned.port, None).is_err());
    assert!(unpinned.requests().is_empty());
}

#[test]
fn a4_at_most_eight_checked_addresses_are_attempted() {
    let fake = FakeGit::new("git version 2.40.1", "exit 1");
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let answers = (1..=12)
        .map(|last| IpAddr::V4(Ipv4Addr::new(93, 184, 216, last)))
        .collect();
    let acquirer = GitAcquirer::system()
        .with_program(fake.program())
        .with_resolver(Arc::new(Fixed(answers)));
    assert_eq!(
        code(writer.acquire_git_with(&acquirer, URL, FAKE_REV)),
        FETCH_FAILED
    );
    assert_eq!(fake.invocations("fetch").len(), MAX_ADDRESSES);
}

#[test]
fn a4_resolver_threads_are_capped_daemon_wide() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let (release, gate) = std::sync::mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(gate));
    for _ in 0..MAX_RESOLVER_THREADS {
        let gate = Arc::clone(&gate);
        let answer = bounded_lookup(
            move || {
                let _ = gate.lock().unwrap().recv();
                Some(Vec::new())
            },
            Duration::from_millis(20),
        );
        assert!(
            answer.is_none(),
            "a stuck lookup times out but keeps its slot"
        );
    }
    // Every slot is held: refused without running the lookup at all, and the
    // production resolver reports it as a resolution failure.
    let ran = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    assert!(bounded_lookup(
        move || {
            flag.store(true, Ordering::SeqCst);
            Some(vec![PUBLIC_V4])
        },
        Duration::from_secs(1)
    )
    .is_none());
    assert!(!ran.load(Ordering::SeqCst));
    assert!(matches!(
        checked_addresses(
            &SystemResolver,
            "git.ocean-fixture.com",
            Instant::now() + Duration::from_secs(5)
        ),
        Err(Fail::Reject(RESOLUTION_FAILED))
    ));
    for _ in 0..MAX_RESOLVER_THREADS {
        release.send(()).unwrap();
    }
    let until = Instant::now() + Duration::from_secs(5);
    while RESOLVER_THREADS.load(Ordering::SeqCst) != 0 && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        bounded_lookup(|| Some(vec![PUBLIC_V4]), Duration::from_secs(1)),
        Some(vec![PUBLIC_V4])
    );
}

#[test]
fn a4_transient_spawn_failures_are_not_reported_as_unpinnable() {
    for errno in [libc::EAGAIN, libc::ENOMEM, libc::EMFILE, libc::ENFILE] {
        assert!(matches!(
            spawn_failure(&io::Error::from_raw_os_error(errno)),
            Fail::Reject(FETCH_FAILED)
        ));
    }
    for errno in [libc::ENOENT, libc::EACCES, libc::ENOEXEC] {
        assert!(matches!(
            spawn_failure(&io::Error::from_raw_os_error(errno)),
            Fail::Reject(GIT_UNAVAILABLE)
        ));
    }
}

#[test]
fn a4_macos_git_shim_is_used_only_when_developer_tools_back_it() {
    let tools = tempfile::tempdir().unwrap();
    let xcode = tools.path().join("Xcode");
    let clt = tools.path().join("CommandLineTools");
    fs::create_dir_all(clt.join("usr/bin")).unwrap();
    fs::write(clt.join("usr/bin/git"), b"").unwrap();
    // No selection: either default location backs the shim.
    assert!(macos_shim_backed(None, &[&xcode, &clt]));
    assert!(!macos_shim_backed(None, &[&xcode]));
    // A recorded selection is authoritative even when a default exists.
    assert!(!macos_shim_backed(Some(&xcode), &[&xcode, &clt]));
    assert!(macos_shim_backed(Some(&clt), &[&xcode]));
}

/// Polls `condition` for up to `limit`.
fn eventually(limit: Duration, condition: impl Fn() -> bool) -> bool {
    let until = Instant::now() + limit;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= until {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// An unproven group cleanup keeps its acquisition's permit only while the
/// group is live: the detached waiter releases it (and reaps the leader) once
/// the group is proven empty, so four such failures no longer exhaust the
/// gate until restart. The fake's fetch leaves a short-lived member behind and
/// the acquirer reports every synchronous cleanup as unprovable.
#[test]
fn a4_unproven_cleanup_holds_its_permit_only_until_the_group_is_empty() {
    let fake = FakeGit::new(
        "git version 2.40.1",
        "sleep 3 & echo $! > \"$LOG/grandchild.pid\"; exit 1",
    );
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let key = writer.gate_key();
    let active = || registry_gates().get(&key).map_or(0, |gate| gate.active);
    let acquirer = GitAcquirer::system()
        .with_program(fake.program())
        .with_resolver(Arc::new(Fixed(vec![PUBLIC_V4])))
        .with_unprovable_cleanup();
    assert_eq!(
        code(writer.acquire_git_with(&acquirer, URL, "0123456789abcdef0123456789abcdef01234567")),
        CLEANUP_FAILED
    );
    let grandchild = fake.grandchild();
    // The member is still running: the permit is held and the quarantine is
    // already gone.
    // SAFETY: probing with signal 0 sends nothing.
    assert_eq!(unsafe { libc::kill(grandchild, 0) }, 0, "member still live");
    assert_eq!(active(), 1, "a live stranded group keeps its permit");
    assert_no_acquisition_residue(config.path());
    // The member exits on its own; the waiter proves the group empty, reaps
    // the leader, and releases the permit.
    assert!(
        eventually(Duration::from_secs(15), || active() == 0),
        "the permit was never released"
    );
    assert!(pid_gone(grandchild));
    assert_eq!(fake.invocations("fetch").len(), 1, "fallback stops");
    // Capacity is back: four fresh acquisitions can begin.
    let leases: Vec<_> = (0..4)
        .map(|_| writer.begin_acquisition().unwrap())
        .collect();
    drop(leases);
}

/// The waiter never releases early or leaks: a permit handed over with a
/// running group stays held until that group exits, and the leader is reaped.
#[test]
fn a4_stranded_group_waiter_releases_only_after_exit() {
    use std::os::unix::process::CommandExt as _;
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let key = writer.gate_key();
    let active = || registry_gates().get(&key).map_or(0, |gate| gate.active);
    let mut lease = writer.begin_acquisition().unwrap();
    let child = Command::new("/bin/sh")
        .args(["-c", "sleep 1"])
        .process_group(0)
        .spawn()
        .unwrap();
    let leader = libc::pid_t::try_from(child.id()).unwrap();
    let group = ToolGroup::new(child).unwrap();
    release_permit_when_empty(vec![group], lease.permit.take());
    drop(lease);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(active(), 1, "released while the group was live");
    assert!(
        eventually(Duration::from_secs(15), || active() == 0),
        "never released after the group exited"
    );
    // Reaped: no zombie remains under this pid.
    // SAFETY: probing with signal 0 sends nothing.
    assert!(eventually(Duration::from_secs(5), || unsafe {
        libc::kill(leader, 0) != 0
    }));
}

/// The macOS shim gate at its call site: the shim is skipped for the next
/// candidate unless developer tools back it, consulted only when reached, and
/// non-executables are never chosen.
#[test]
fn a4_locate_git_skips_an_unbacked_shim_for_the_next_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let make = |name: &str, mode: u32| {
        let path = dir.path().join(name);
        fs::write(&path, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    };
    let shim = make("shim-git", 0o755);
    let brew = make("brew-git", 0o755);
    let plain = make("plain", 0o644);
    let candidates = [shim.as_path(), brew.as_path()];
    assert_eq!(
        locate_git_from(&candidates, Some(&shim), || false).as_deref(),
        Some(brew.as_path())
    );
    assert_eq!(
        locate_git_from(&candidates, Some(&shim), || true).as_deref(),
        Some(shim.as_path())
    );
    // Off macOS there is no shim: the first executable wins.
    assert_eq!(
        locate_git_from(&candidates, None, || false).as_deref(),
        Some(shim.as_path())
    );
    // The gate is not consulted for other candidates.
    assert_eq!(
        locate_git_from(&[plain.as_path(), brew.as_path()], Some(&shim), || {
            panic!("shim gate consulted for a non-shim")
        })
        .as_deref(),
        Some(brew.as_path())
    );
    assert_eq!(locate_git_from(&[plain.as_path()], None, || true), None);
}

#[test]
fn a4_listing_fold_collisions_are_refused_before_any_filesystem_write() {
    let acquirer = GitAcquirer::system();
    let oid = "ab".repeat(20);
    let listing = |paths: &[&str]| {
        paths
            .iter()
            .map(|path| format!("100644 blob {oid} 1\t{path}\0"))
            .collect::<String>()
    };
    for paths in [
        &["README", "readme"][..],
        &["caf\u{e9}", "cafe\u{301}"],
        &["DIR/one", "dir/two"],
        &["Docs", "docs/inner"],
        &["a/B/x", "a/b/y"],
        // Full, not simple, case folding (APFS collides all of these).
        &["stra\u{df}e", "strasse"],
        &["\u{3b1}\u{3c2}", "\u{3b1}\u{3c3}"],
        &["\u{fb01}le", "file"],
        &["x\u{345}", "x\u{3b9}"],
        &["dir/stra\u{df}e/a", "DIR/STRASSE/b"],
    ] {
        assert!(
            matches!(
                acquirer.parse_tree(listing(paths).as_bytes(), 40),
                Err(Fail::Reject(TREE_UNSUPPORTED))
            ),
            "{paths:?}"
        );
    }
    // Turkish dotless and dotted i stay distinct (so does APFS): folding is
    // the default, not the Turkic, mapping.
    assert_eq!(
        acquirer
            .parse_tree(listing(&["\u{131}", "i"]).as_bytes(), 40)
            .unwrap()
            .len(),
        2
    );
    assert_ne!(fold_component("\u{131}"), fold_component("i"));
    // Siblings under one directory are fine.
    assert_eq!(
        acquirer
            .parse_tree(listing(&["dir/one", "dir/two", "other"]).as_bytes(), 40)
            .unwrap()
            .len(),
        3
    );
}
