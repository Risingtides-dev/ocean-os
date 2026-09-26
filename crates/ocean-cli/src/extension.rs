//! `ocean-rs extension …` — a thin daemon client for the Stage A §15 extension
//! contract. It never reads or writes `<config_dir>/extensions`: every state
//! change is a daemon HTTP mutation. The only local file it reads is the
//! operator key, which authenticates mutations exactly as the daemon's other
//! operator routes are authenticated.
//!
//! Exit codes (§15): 0 for HTTP 200, 3 for a committed 202 (reconciliation or
//! reap still pending/blocked), 4 for a committed `registry_recovery_required`,
//! and 1 for any pre-commit refusal or transport failure. A committed response
//! is never retried; a conflict is never silently retried.

use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use clap::{ArgGroup, Subcommand};
use serde_json::{json, Value};

/// Header carrying the local operator credential (header-only by daemon rule).
const OPERATOR_HEADER: &str = "x-ocean-operator";
/// Read attempts when the registry is momentarily busy under a mutation.
const BUSY_READ_ATTEMPTS: u32 = 5;
const BUSY_READ_DELAY: std::time::Duration = std::time::Duration::from_millis(300);

pub(crate) const EXIT_COMMITTED_PENDING: i32 = 3;
pub(crate) const EXIT_RECOVERY_REQUIRED: i32 = 4;

/// Daemon-owned extension state commands.
#[derive(Debug, Subcommand)]
pub(crate) enum ExtensionCmd {
    /// List every extension with registry state plus its cached runtime status.
    List {
        #[arg(long)]
        project_id: Option<uuid::Uuid>,
    },
    /// Inspect installed/trusted/enabled state without executing package code.
    Inspect {
        id: String,
        #[arg(long)]
        project_id: Option<uuid::Uuid>,
    },
    /// Run static state, digest, manifest, trust, and enablement diagnostics.
    Doctor {
        id: String,
        #[arg(long)]
        project_id: Option<uuid::Uuid>,
    },
    /// Show the supervisor's cached runtime status (never probes).
    Status { id: String },
    /// Install a package from a local directory (or, once available, a
    /// pinned public Git revision). Grants nothing and starts nothing.
    #[command(group(ArgGroup::new("source").required(true).args(["path", "git"])))]
    Install {
        #[arg(long, conflicts_with = "git")]
        path: Option<PathBuf>,
        #[arg(long, requires = "rev")]
        git: Option<String>,
        #[arg(long, requires = "git", conflicts_with = "path")]
        rev: Option<String>,
    },
    /// Preview (default) or apply (`--confirm-grant-diff`) an exact-digest
    /// trust grant with per-service native-process acknowledgement.
    Trust {
        id: String,
        #[arg(long)]
        digest: String,
        /// Grant a requested ordinary environment name (repeatable).
        #[arg(long = "grant-env", value_name = "NAME")]
        grant_env: Vec<String>,
        /// Grant a requested secret reference such as `env:SOURCE` (repeatable).
        #[arg(long = "grant-secret", value_name = "REF")]
        grant_secret: Vec<String>,
        /// Acknowledge daemon-user-equivalent native authority for SERVICE.
        #[arg(long = "ack-native-process", value_name = "SERVICE")]
        ack_native_process: Vec<String>,
        /// Bind a secret for SERVICE as `SERVICE:TARGET=REF` (repeatable).
        #[arg(long = "bind-secret", value_name = "SERVICE:TARGET=REF")]
        bind_secret: Vec<String>,
        /// Apply the grant shown by a preview with exactly this confirmation.
        #[arg(long = "confirm-grant-diff", value_name = "HASH")]
        confirm_grant_diff: Option<String>,
    },
    /// Enable globally, or for one registered project.
    Enable {
        id: String,
        #[arg(long)]
        project_id: Option<uuid::Uuid>,
    },
    /// Disable globally, or for one registered project; returns after reap.
    Disable {
        id: String,
        #[arg(long)]
        project_id: Option<uuid::Uuid>,
    },
    /// Remove a fully disabled and stopped package.
    Remove {
        id: String,
        #[arg(long)]
        purge_state: bool,
    },
    /// Replace a disabled and stopped package; the new digest is untrusted.
    #[command(group(ArgGroup::new("source").required(true).args(["path", "git"])))]
    Update {
        id: String,
        #[arg(long, conflicts_with = "git")]
        path: Option<PathBuf>,
        #[arg(long, requires = "rev")]
        git: Option<String>,
        #[arg(long, requires = "git", conflicts_with = "path")]
        rev: Option<String>,
    },
}

/// Outcome of one command: the process exit code to use.
pub(crate) type ExitCode = i32;

pub(crate) async fn run(
    client: &reqwest::Client,
    base: &str,
    command: ExtensionCmd,
) -> anyhow::Result<ExitCode> {
    let base = base.trim_end_matches('/');
    match command {
        ExtensionCmd::List { project_id } => {
            let mut url = format!("{base}/v1/extensions");
            if let Some(project_id) = project_id {
                url.push_str(&format!("?project_id={project_id}"));
            }
            print_read(client, url).await
        }
        ExtensionCmd::Inspect { id, project_id } => {
            print_read(client, id_read_url(base, &id, "inspect", project_id)).await
        }
        ExtensionCmd::Doctor { id, project_id } => {
            print_read(client, id_read_url(base, &id, "doctor", project_id)).await
        }
        ExtensionCmd::Status { id } => {
            print_read(
                client,
                format!("{base}/v1/extensions/{}/status", encode(&id)),
            )
            .await
        }
        ExtensionCmd::Install { path, git, rev } => {
            let source = source_body(path, git, rev)?;
            let revision = current_revision(client, base, None).await?;
            let body = json!({"expected_state_revision": revision, "source": source});
            mutate(
                client,
                reqwest::Method::POST,
                format!("{base}/v1/extensions/install"),
                body,
            )
            .await
        }
        ExtensionCmd::Update { id, path, git, rev } => {
            let source = source_body(path, git, rev)?;
            let revision = current_revision(client, base, Some(&id)).await?;
            let body = json!({"expected_state_revision": revision, "source": source});
            mutate(
                client,
                reqwest::Method::POST,
                format!("{base}/v1/extensions/{}/update", encode(&id)),
                body,
            )
            .await
        }
        ExtensionCmd::Trust {
            id,
            digest,
            grant_env,
            grant_secret,
            ack_native_process,
            bind_secret,
            confirm_grant_diff,
        } => {
            let service_grants = service_grants(&ack_native_process, &bind_secret)?;
            let revision = current_revision(client, base, Some(&id)).await?;
            let body = json!({
                "expected_state_revision": revision,
                "digest": digest,
                "capabilities": {"network": [], "filesystem": [], "env": grant_env, "secrets": grant_secret},
                "service_grants": service_grants,
                "confirm_grant_diff": confirm_grant_diff,
            });
            mutate(
                client,
                reqwest::Method::POST,
                format!("{base}/v1/extensions/{}/trust", encode(&id)),
                body,
            )
            .await
        }
        ExtensionCmd::Enable { id, project_id } => {
            scope_mutation(client, base, &id, "enable", project_id).await
        }
        ExtensionCmd::Disable { id, project_id } => {
            scope_mutation(client, base, &id, "disable", project_id).await
        }
        ExtensionCmd::Remove { id, purge_state } => {
            let revision = current_revision(client, base, Some(&id)).await?;
            let body = json!({"expected_state_revision": revision, "purge_state": purge_state});
            mutate(
                client,
                reqwest::Method::DELETE,
                format!("{base}/v1/extensions/{}", encode(&id)),
                body,
            )
            .await
        }
    }
}

async fn scope_mutation(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    verb: &str,
    project_id: Option<uuid::Uuid>,
) -> anyhow::Result<ExitCode> {
    let revision = current_revision(client, base, Some(id)).await?;
    let scope = match project_id {
        Some(project_id) => json!({"kind": "project", "project_id": project_id}),
        None => json!({"kind": "global"}),
    };
    let body = json!({"expected_state_revision": revision, "scope": scope});
    mutate(
        client,
        reqwest::Method::POST,
        format!("{base}/v1/extensions/{}/{verb}", encode(id)),
        body,
    )
    .await
}

fn id_read_url(base: &str, id: &str, action: &str, project_id: Option<uuid::Uuid>) -> String {
    let mut url = format!("{base}/v1/extensions/{}/{action}", encode(id));
    if let Some(project_id) = project_id {
        url.push_str(&format!("?project_id={project_id}"));
    }
    url
}

/// Percent-encode a path segment (URL path ids are revalidated by the daemon).
pub(crate) fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// §13.1: a relative operand is made absolute against the CLI's own cwd and
/// normalized LEXICALLY (no symlink resolution — the daemon itself refuses a
/// symlinked package root, and resolving it here would hide one).
pub(crate) fn absolute_source_path(path: &Path, cwd: &Path) -> anyhow::Result<String> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => normal.push(prefix.as_os_str()),
            Component::RootDir => normal.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            Component::Normal(part) => normal.push(part),
        }
    }
    normal
        .to_str()
        .map(str::to_owned)
        .context("package path must be valid UTF-8")
}

pub(crate) fn source_body(
    path: Option<PathBuf>,
    git: Option<String>,
    rev: Option<String>,
) -> anyhow::Result<Value> {
    match (path, git, rev) {
        (Some(path), None, None) => {
            let cwd = std::env::current_dir().context("resolve current directory")?;
            Ok(json!({"kind": "local-path", "path": absolute_source_path(&path, &cwd)?}))
        }
        (None, Some(url), Some(revision)) => {
            Ok(json!({"kind": "git", "url": url, "revision": revision}))
        }
        _ => anyhow::bail!("pass exactly one of --path PATH or --git URL --rev HEX"),
    }
}

/// Build canonical per-service rows from `--ack-native-process SERVICE` and
/// `--bind-secret SERVICE:TARGET=REF`. A binding for a service that was not
/// acknowledged is sent with `native_process_ack:false` so the daemon refuses
/// it; the CLI never acknowledges on the operator's behalf.
pub(crate) fn service_grants(acks: &[String], binds: &[String]) -> anyhow::Result<Vec<Value>> {
    let mut rows: std::collections::BTreeMap<String, (bool, Vec<Value>)> =
        std::collections::BTreeMap::new();
    for service in acks {
        rows.entry(service.clone()).or_default().0 = true;
    }
    for bind in binds {
        let (service, binding) = bind
            .split_once(':')
            .with_context(|| format!("--bind-secret must be SERVICE:TARGET=REF, got {bind:?}"))?;
        let (target, reference) = binding
            .split_once('=')
            .with_context(|| format!("--bind-secret must be SERVICE:TARGET=REF, got {bind:?}"))?;
        anyhow::ensure!(
            !service.is_empty() && !target.is_empty() && !reference.is_empty(),
            "--bind-secret must be SERVICE:TARGET=REF, got {bind:?}"
        );
        rows.entry(service.to_owned())
            .or_default()
            .1
            .push(json!({"target_env": target, "reference": reference}));
    }
    Ok(rows
        .into_iter()
        .map(|(service_id, (ack, bindings))| {
            json!({"service_id": service_id, "native_process_ack": ack, "secret_bindings": bindings})
        })
        .collect())
}

/// GET with bounded retries while the registry is busy under a mutation. A
/// read is safe to repeat; a mutation never is.
async fn read_json(client: &reqwest::Client, url: &str) -> anyhow::Result<(u16, Value)> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let response = client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = response.status().as_u16();
        let body: Value = response.json().await.context("decode extension response")?;
        let busy = status == 503 && body["error"] == "extension_state_busy";
        if !busy || attempt >= BUSY_READ_ATTEMPTS {
            return Ok((status, body));
        }
        tokio::time::sleep(BUSY_READ_DELAY).await;
    }
}

async fn print_read(client: &reqwest::Client, url: String) -> anyhow::Result<ExitCode> {
    let (status, body) = read_json(client, &url).await?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    anyhow::ensure!(
        (200..300).contains(&status),
        "extension request failed with HTTP {status}"
    );
    anyhow::ensure!(
        body.get("ok").and_then(Value::as_bool) == Some(true),
        "extension diagnostics reported a failure"
    );
    Ok(0)
}

/// The revision every mutation must name. Read from inspect for an id (from
/// list when the id has no state yet, so the daemon can answer precisely).
async fn current_revision(
    client: &reqwest::Client,
    base: &str,
    id: Option<&str>,
) -> anyhow::Result<u64> {
    if let Some(id) = id {
        let (status, body) = read_json(client, &id_read_url(base, id, "inspect", None)).await?;
        if status == 200 {
            return body["extension"]["state_revision"]
                .as_u64()
                .context("inspect response has no state_revision");
        }
        anyhow::ensure!(
            status == 404 && body["error"] == "extension_not_found",
            "could not read extension state (HTTP {status}): {}",
            body["error"]
        );
    }
    let (status, body) = read_json(client, &format!("{base}/v1/extensions")).await?;
    anyhow::ensure!(
        status == 200,
        "could not read extension state (HTTP {status}): {}",
        body["error"]
    );
    body["state_revision"]
        .as_u64()
        .context("list response has no state_revision")
}

/// Read the daemon's operator key for this user without following a
/// symlink, and only from a single-link regular file no other user can read.
pub(crate) fn operator_key(config_dir: &Path) -> anyhow::Result<String> {
    let path = config_dir.join("operator.key");
    let unavailable = || {
        format!(
            "operator key unavailable at {}; extension mutations require the daemon's operator credential",
            path.display()
        )
    };
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Never follow a symlink, and never block opening a FIFO planted at
        // the key path.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options.open(&path).with_context(unavailable)?;
    let opened = file.metadata().with_context(unavailable)?;
    anyhow::ensure!(opened.is_file(), "{}", unavailable());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no preconditions.
        let euid = unsafe { libc::geteuid() };
        anyhow::ensure!(
            opened.uid() == euid && opened.nlink() == 1 && opened.mode() & 0o077 == 0,
            "{}",
            unavailable()
        );
    }
    // The daemon's key is 43 base64url characters plus a newline; read a
    // bounded prefix so a replaced huge file cannot exhaust memory.
    let mut key = String::new();
    std::io::Read::read_to_string(&mut std::io::Read::take(file, MAX_KEY_BYTES), &mut key)
        .with_context(unavailable)?;
    let key = key.trim().to_owned();
    anyhow::ensure!(!key.is_empty(), "{}", unavailable());
    Ok(key)
}

const MAX_KEY_BYTES: u64 = 1024;

/// The operator credential is sent only to a loopback daemon. A remote
/// `--url` (for example a tailnet or proxy address) would carry the local
/// operator key off the box, so it is refused before the key is read.
pub(crate) fn require_loopback(url: &str) -> anyhow::Result<()> {
    let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid daemon URL {url:?}"))?;
    let loopback = parsed.host_str().is_some_and(|host| {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    anyhow::ensure!(
        loopback,
        "refusing to send the operator key to a non-loopback daemon URL; extension mutations must target a local daemon"
    );
    Ok(())
}

async fn mutate(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    body: Value,
) -> anyhow::Result<ExitCode> {
    require_loopback(&url)?;
    let key = operator_key(&ocean_agent::config_dir_from_env())?;
    let response = client
        .request(method, &url)
        .header(OPERATOR_HEADER, key)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("send extension mutation to {url}"))?;
    let status = response.status().as_u16();
    let body: Value = response
        .json()
        .await
        .context("decode extension mutation response")?;
    println!("{}", serde_json::to_string_pretty(&body)?);
    println!("{}", mutation_summary(&body));
    let code = mutation_exit_code(status, &body);
    match code {
        EXIT_COMMITTED_PENDING => eprintln!(
            "committed at revision {}; reconciliation/reap is still pending — do not retry, check `ocean-rs extension status` or `inspect`",
            body["mutation"]["state_revision"]
        ),
        EXIT_RECOVERY_REQUIRED => eprintln!(
            "committed at revision {} but the registry needs recovery — do not retry; restart the daemon to run journal recovery",
            body["mutation"]["state_revision"]
        ),
        _ if body["error"]["retryable"] == true => eprintln!(
            "nothing was committed and the refusal is transient ({}); inspect again and retry the command shortly",
            body["error"]["code"]
        ),
        _ => {}
    }
    Ok(code)
}

/// One line carrying every §15 field an operator acts on.
pub(crate) fn mutation_summary(body: &Value) -> String {
    if body["applied"] == false && body["preview"].is_object() {
        return format!(
            "preview applied=false committed=false state_revision={} confirmation={} (rerun with --confirm-grant-diff to apply)",
            body["state_revision"], body["preview"]["confirmation"]
        );
    }
    let mutation = &body["mutation"];
    let mut line = format!(
        "committed={} operation_id={} state_revision={}",
        mutation["committed"], mutation["operation_id"], mutation["state_revision"]
    );
    if body["ok"] == true {
        line.push_str(&format!(
            " reconciliation={} reap={}",
            mutation["reconciliation"], mutation["reap"]
        ));
    } else {
        line.push_str(&format!(
            " error={} message={}",
            body["error"]["code"], body["error"]["message"]
        ));
    }
    line
}

/// §15 exit contract: 0 for 200 (including a trust preview), 3 for a
/// committed 202, 4 for a committed recovery error, 1 for everything else.
pub(crate) fn mutation_exit_code(status: u16, body: &Value) -> ExitCode {
    let committed = body["mutation"]["committed"] == true;
    match status {
        200 if body["ok"] == true => 0,
        202 if committed => EXIT_COMMITTED_PENDING,
        500 if committed && body["error"]["code"] == "registry_recovery_required" => {
            EXIT_RECOVERY_REQUIRED
        }
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_committed_contract() {
        let committed = |reconciliation: &str| json!({"ok": true, "mutation": {"operation_id": "u", "committed": true, "state_revision": 9, "reconciliation": reconciliation, "reap": "not_required"}});
        assert_eq!(mutation_exit_code(200, &committed("complete")), 0);
        assert_eq!(
            mutation_exit_code(202, &committed("pending")),
            EXIT_COMMITTED_PENDING
        );
        assert_eq!(
            mutation_exit_code(202, &committed("blocked")),
            EXIT_COMMITTED_PENDING
        );
        let recovery = json!({"ok": false, "mutation": {"operation_id": "u", "committed": true, "state_revision": 9}, "error": {"code": "registry_recovery_required", "message": "extension registry recovery is required"}});
        assert_eq!(mutation_exit_code(500, &recovery), EXIT_RECOVERY_REQUIRED);
        let precommit = json!({"ok": false, "mutation": {"operation_id": "u", "committed": false, "state_revision": 8}, "error": {"code": "state_revision_conflict", "message": "m"}});
        assert_eq!(mutation_exit_code(409, &precommit), 1);
        // A 500 that did not commit is an ordinary failure, never code 4.
        let uncommitted_500 = json!({"ok": false, "mutation": {"committed": false, "state_revision": 8}, "error": {"code": "registry_recovery_required"}});
        assert_eq!(mutation_exit_code(500, &uncommitted_500), 1);
        let preview = json!({"ok": true, "applied": false, "committed": false, "state_revision": 3, "preview": {"confirmation": "sha256:x"}});
        assert_eq!(mutation_exit_code(200, &preview), 0);
    }

    #[test]
    fn summary_always_prints_commit_identity_revision_and_progress() {
        let line = mutation_summary(
            &json!({"ok": true, "mutation": {"operation_id": "op", "committed": true, "state_revision": 9, "reconciliation": "pending", "reap": "pending"}}),
        );
        for part in [
            "committed=true",
            "operation_id=\"op\"",
            "state_revision=9",
            "reconciliation=\"pending\"",
            "reap=\"pending\"",
        ] {
            assert!(line.contains(part), "{line}");
        }
        let line = mutation_summary(
            &json!({"ok": false, "mutation": {"operation_id": "op", "committed": false, "state_revision": 8}, "error": {"code": "extension_active", "message": "m"}}),
        );
        assert!(line.contains("committed=false") && line.contains("extension_active"));
        let line = mutation_summary(
            &json!({"ok": true, "applied": false, "committed": false, "state_revision": 3, "preview": {"confirmation": "sha256:x"}}),
        );
        assert!(line.contains("applied=false") && line.contains("sha256:x"));
    }

    #[test]
    fn operator_key_is_only_sent_to_loopback_daemons() {
        for url in [
            "http://127.0.0.1:4780/v1/extensions/install",
            "http://localhost:4780/v1/extensions/x",
            "http://[::1]:4780/v1/extensions/x",
            "http://127.9.9.9/v1/extensions/x",
        ] {
            assert!(require_loopback(url).is_ok(), "{url}");
        }
        for url in [
            "http://100.65.142.80:4780/v1/extensions/install",
            "https://daemon.example.com/v1/extensions/x",
            "http://localhost.example.com/v1/extensions/x",
            "http://0.0.0.0:4780/v1/extensions/x",
            "not a url",
        ] {
            assert!(require_loopback(url).is_err(), "{url}");
        }
    }

    #[test]
    fn relative_paths_are_made_absolute_lexically() {
        let cwd = Path::new("/work/tree");
        assert_eq!(
            absolute_source_path(Path::new("pkg/./noop"), cwd).unwrap(),
            "/work/tree/pkg/noop"
        );
        assert_eq!(
            absolute_source_path(Path::new("../other/noop/"), cwd).unwrap(),
            "/work/other/noop"
        );
        assert_eq!(
            absolute_source_path(Path::new("/abs//noop"), cwd).unwrap(),
            "/abs/noop"
        );
    }

    #[test]
    fn service_grants_never_acknowledge_on_the_operators_behalf() {
        let rows = service_grants(
            &["lifecycle".into()],
            &[
                "lifecycle:TOKEN=env:SOURCE".into(),
                "other:NAME=env:X".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            rows,
            vec![
                json!({"service_id": "lifecycle", "native_process_ack": true, "secret_bindings": [{"target_env": "TOKEN", "reference": "env:SOURCE"}]}),
                json!({"service_id": "other", "native_process_ack": false, "secret_bindings": [{"target_env": "NAME", "reference": "env:X"}]}),
            ]
        );
        assert!(service_grants(&[], &["missing-separator".into()]).is_err());
        assert!(service_grants(&[], &["svc:TARGET".into()]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn operator_key_is_read_only_from_a_private_regular_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("operator.key");
        assert!(operator_key(dir.path()).is_err());
        std::fs::write(&path, "secret-key\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(operator_key(dir.path()).is_err(), "group/world readable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(operator_key(dir.path()).unwrap(), "secret-key");
        let linked = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(&path, linked.path().join("operator.key")).unwrap();
        assert!(operator_key(linked.path()).is_err(), "symlink followed");
        // A FIFO at the key path is refused without blocking the CLI.
        let fifo = tempfile::tempdir().unwrap();
        let fifo_path = std::ffi::CString::new(
            fifo.path()
                .join("operator.key")
                .to_str()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        assert!(operator_key(fifo.path()).is_err(), "fifo accepted");
    }
}
