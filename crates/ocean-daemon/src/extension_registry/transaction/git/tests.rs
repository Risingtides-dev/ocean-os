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
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("log");
        let exec = temp.path().join("libexec");
        fs::create_dir(&log).unwrap();
        fs::create_dir(&exec).unwrap();
        fs::write(exec.join("git-remote-https"), b"").unwrap();
        let script = format!(
            "#!/bin/sh\nLOG='{log}'\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done > \"$LOG/$$.argv\"\nenv > \"$LOG/$$.env\"\ncase \" $* \" in\n  *' --version '*) printf '%s\\n' '{version}'; exit 0 ;;\n  *' --exec-path '*) printf '%s\\n' '{exec}'; exit 0 ;;\n  *' init '*) for last in \"$@\"; do :; done; mkdir -p \"$last\"; exit 0 ;;\n  *' fetch '*) {fetch_body} ;;\nesac\nexit 1\n",
            log = log.display(),
            exec = exec.display(),
        );
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
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
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
        Self {
            port,
            requests,
            stop,
        }
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
    let config = tempfile::tempdir().unwrap();
    let writer = RegistryWriter::new(config.path().to_path_buf());
    let lease = writer.begin_acquisition().unwrap();
    let work = Workspace::create(&lease).unwrap();
    let acquirer = GitAcquirer::system().with_program(host_git());
    let deadline = Instant::now() + Duration::from_secs(30);
    let program = acquirer.probe(&work, deadline).unwrap();
    let pin = pin.map(|address| pin_entry(PINNED_HOST, port, address));
    acquirer.fetch_attempt(
        &program,
        &work,
        &format!("{scheme}://{PINNED_HOST}:{port}/ocean/noop.git"),
        scheme,
        pin.as_deref(),
        "0123456789abcdef0123456789abcdef01234567",
        deadline,
    )
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
