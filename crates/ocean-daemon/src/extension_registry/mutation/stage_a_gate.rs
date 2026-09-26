//! Stage A5 integrated gate (manifest §18 A5, §20).
//!
//! Every earlier slice proved its own mechanism in isolation. These tests run
//! the pieces together, the way the daemon composes them: the §15 HTTP routes,
//! the A3a writer, the supervisor, the lifecycle dispatcher, and ordinary
//! fake-provider turns through `agent_turn`, over one real no-op native service
//! that speaks the strict stdio protocol and records every frame it receives
//! under its assigned `data/` root. Nothing here adds production behavior.
//!
//! Scope limits are stated where they apply. §20 step 10's project-scope disable
//! and the credential class for mutation routes are open operator rulings (see
//! the manifest's A3b note), so no test here decides them.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{Method, StatusCode};
use axum::{Json, Router};
use ocean_agent_sdk::{AgentSessionId, AgentTurnRequest, AgentTurnResponse, AgentTurnStatus};
use serde_json::{json, Value};
use uuid::Uuid;

use super::tests::{
    assert_committed, assert_precommit, get_json, install_local, post_op, process_alive, router,
    scope, send, wait_for_status, with_supervisor, Auth, Fixture, ID,
};
use crate::extension_lifecycle::{LifecycleDispatcher, LifecycleSource};
use crate::tests::{fake_convene_state, TestEnvRestore, AUTO_CONVENE_ENV_LOCK};
use crate::AppState;

const SUBSCRIPTIONS: &str =
    r#"["daemon_started","session_started","turn_started","turn_finished"]"#;

/// The §20 no-op service. It handshakes (resuming from the cursor it persisted
/// under `data/` when one exists), ACKs every event, answers ping, and records
/// its environment, cwd, argv, and every received frame. It also starts one
/// cooperative grandchild in its own process group for the reap proofs, and
/// touches `cache/` and its connection `tmp/` so retention is observable.
fn gate_service() -> String {
    format!(
        r#"#!/bin/sh
me=$$
printf '%s\n' "$me" >> "$HOME/starts"
if [ -e "$HOME/frames" ]; then printf '%s\n' "$me" >> "$HOME/saw-retained-data"; fi
env | LC_ALL=C sort > "$HOME/env-$me"
printf '%s|' "$0" "$@" > "$HOME/argv-$me"
for name in HOME XDG_STATE_HOME XDG_CACHE_HOME TMPDIR PWD; do
  eval "path=\$$name"
  printf '%s %s\n' "$name" "$(ls -idL "$path" | awk '{{print $1}}')" >> "$HOME/ids-$me"
done
printf 'CWD %s\n' "$(ls -idL . | awk '{{print $1}}')" >> "$HOME/ids-$me"
printf 'ARGV0 %s\n' "$(ls -iL "$0" | awk '{{print $1}}')" >> "$HOME/ids-$me"
printf cache > "$XDG_CACHE_HOME/cache-proof"
printf tmp > "$TMPDIR/tmp-proof"
sleep 600 </dev/null >/dev/null 2>&1 &
printf '%s\n' "$!" > "$HOME/grandchild-$me"
IFS= read -r hello || exit 10
printf '%s %s\n' "$me" "$hello" >> "$HOME/frames"
boot=$(printf '%s' "$hello" | sed -n 's/.*"daemon_boot_id":"\([^"]*\)".*/\1/p')
epoch=$(printf '%s' "$hello" | sed -n 's/.*"activation_epoch":"\([^"]*\)".*/\1/p')
resume=null
if [ -s "$HOME/cursor" ]; then
  read -r cboot cepoch cseq < "$HOME/cursor"
  resume="{{\"daemon_boot_id\":\"$cboot\",\"activation_epoch\":\"$cepoch\",\"after_sequence\":\"$cseq\"}}"
fi
printf '{{"protocol":"ocean.extension.service","version":1,"frame":"service_hello","subscriptions":{SUBSCRIPTIONS},"resume":%s}}\n' "$resume"
while IFS= read -r frame; do
  printf '%s %s\n' "$me" "$frame" >> "$HOME/frames"
  case "$frame" in
    *'"frame":"event"'*)
      seq=$(printf '%s' "$frame" | sed -n 's/.*"sequence":"\([0-9]*\)".*/\1/p')
      printf '{{"protocol":"ocean.extension.service","version":1,"frame":"ack","sequence":"%s"}}\n' "$seq"
      printf '%s %s %s\n' "$boot" "$epoch" "$seq" > "$HOME/cursor" ;;
    *'"frame":"ping"'*)
      nonce=$(printf '%s' "$frame" | sed -n 's/.*"nonce":"\([^"]*\)".*/\1/p')
      printf '{{"protocol":"ocean.extension.service","version":1,"frame":"pong","nonce":"%s"}}\n' "$nonce" ;;
    *'"frame":"shutdown"'*)
      printf '%s\n' '{{"protocol":"ocean.extension.service","version":1,"frame":"shutdown_complete"}}'
      exit 0 ;;
  esac
done
"#
    )
}

/// Write the gate package: one native service subscribed to four kinds, no
/// capability request unless `capabilities` is given, plus executable canaries
/// at every other resource path.
fn gate_package(fixture: &Fixture, name: &str, version: &str, capabilities: &str) -> String {
    gate_package_with(fixture, name, version, capabilities, &gate_service())
}

fn gate_package_with(
    fixture: &Fixture,
    name: &str,
    version: &str,
    capabilities: &str,
    service: &str,
) -> String {
    use std::os::unix::fs::PermissionsExt;
    let dir = fixture.sources.path().join(name);
    fs::create_dir_all(dir.join("services")).unwrap();
    fs::create_dir_all(dir.join("hooks")).unwrap();
    fs::write(
        dir.join("ocean-extension.toml"),
        format!(
            "schema_version = 1\nid = \"{ID}\"\nname = \"Noop\"\nversion = \"{version}\"\nmin_ocean_version = \"0.1.0\"\n\n[[services]]\nid = \"lifecycle\"\nentry = \"services/lifecycle\"\nevents = {SUBSCRIPTIONS}\n{capabilities}"
        ),
    )
    .unwrap();
    fs::write(dir.join("services/lifecycle"), service).unwrap();
    for canary in ["hooks/on-install", "run-me"] {
        fs::write(
            dir.join(canary),
            format!("#!/bin/sh\nprintf ran > '{}'\n", fixture.canary.display()),
        )
        .unwrap();
    }
    for executable in ["services/lifecycle", "hooks/on-install", "run-me"] {
        fs::set_permissions(dir.join(executable), fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir.to_str().unwrap().to_owned()
}

fn data_dir(fixture: &Fixture) -> PathBuf {
    fixture.root().join("state").join(ID).join("data")
}

/// Decode one recorded host→child frame with the strict SDK decoder for its
/// declared type and re-encode it: the host must have written exactly the
/// canonical v1 bytes (§7.1, §8.1). The shell fixture only records lines; this
/// is what proves host-frame conformance.
fn assert_strict_host_frame(line: &str) -> Value {
    use ocean_agent_sdk::extension_lifecycle::{
        decode_frame, encode_frame, HostHello, Lag, LifecycleEvent, Ping, Ready, Reset, Shutdown,
    };
    let bytes = format!("{line}\n").into_bytes();
    let value: Value = serde_json::from_str(line).unwrap();
    fn exact<T: serde::de::DeserializeOwned + serde::Serialize>(bytes: &[u8]) {
        let decoded: T = decode_frame(bytes)
            .unwrap_or_else(|error| panic!("{error:?}: {}", String::from_utf8_lossy(bytes)));
        assert_eq!(
            encode_frame(&decoded).unwrap(),
            bytes,
            "host frame is not canonical"
        );
    }
    match value["frame"].as_str().unwrap_or_default() {
        "host_hello" => exact::<HostHello>(&bytes),
        "ready" => exact::<Ready>(&bytes),
        "event" => exact::<LifecycleEvent>(&bytes),
        "lag" => exact::<Lag>(&bytes),
        "reset" => exact::<Reset>(&bytes),
        "ping" => exact::<Ping>(&bytes),
        "shutdown" => exact::<Shutdown>(&bytes),
        other => panic!("the host wrote a non-v1 frame kind {other:?}: {line}"),
    }
    value
}

/// Every frame the service received, in order, as `(service pid, frame)`.
/// Each one is first proven to be a strict, canonical v1 host frame.
fn frames(fixture: &Fixture) -> Vec<(String, Value)> {
    fs::read_to_string(data_dir(fixture).join("frames"))
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let (pid, frame) = line.split_once(' ').unwrap();
            (pid.to_owned(), assert_strict_host_frame(frame))
        })
        .collect()
}

fn frames_of(fixture: &Fixture, pid: i64) -> Vec<Value> {
    frames(fixture)
        .into_iter()
        .filter(|(owner, _)| *owner == pid.to_string())
        .map(|(_, frame)| frame)
        .collect()
}

fn events_of(fixture: &Fixture, pid: i64) -> Vec<Value> {
    frames_of(fixture, pid)
        .into_iter()
        .filter(|frame| frame["frame"] == "event")
        .collect()
}

fn sequence(frame: &Value) -> u64 {
    frame["sequence"].as_str().unwrap().parse().unwrap()
}

fn starts(fixture: &Fixture) -> usize {
    fs::read_to_string(data_dir(fixture).join("starts"))
        .unwrap_or_default()
        .lines()
        .count()
}

fn grandchild(fixture: &Fixture, pid: i64) -> i64 {
    fs::read_to_string(data_dir(fixture).join(format!("grandchild-{pid}")))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

async fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(tokio::time::Instant::now() < deadline, "never held: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn healthy(app: &Router) -> (i64, Value) {
    let body = wait_for_status(app, ID, |body| {
        body["services"][0]["state"] == "healthy" && body["services"][0]["pid"].is_u64()
    })
    .await;
    (body["services"][0]["pid"].as_i64().unwrap(), body)
}

/// Preview, check the complete authority notice, then apply the exact grant.
async fn trust(
    app: &Router,
    expected: u64,
    digest: &str,
    capabilities: Value,
    bindings: Value,
) -> (StatusCode, Value, Value) {
    let request = json!({
        "expected_state_revision": expected,
        "digest": digest,
        "capabilities": capabilities,
        "service_grants": [{"service_id": "lifecycle", "native_process_ack": true, "secret_bindings": bindings}],
        "confirm_grant_diff": null
    });
    let (status, preview) =
        post_op(app, &format!("/v1/extensions/{ID}/trust"), request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{preview}");
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["committed"], false);
    assert_eq!(
        preview["preview"]["native_authority_notice"],
        super::super::transaction::NATIVE_AUTHORITY_NOTICE
    );
    let mut apply = request;
    apply["confirm_grant_diff"] = preview["preview"]["confirmation"].clone();
    let (status, applied) = post_op(app, &format!("/v1/extensions/{ID}/trust"), apply).await;
    (status, preview, applied)
}

/// Counts every hostname lookup the Git acquirer would make.
#[derive(Default)]
struct CountingResolver(std::sync::atomic::AtomicUsize);

impl super::super::transaction::git::HostResolver for CountingResolver {
    fn resolve(&self, _: &str, _: Duration) -> Option<Vec<std::net::IpAddr>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        None
    }
}

fn turn_request(session: Option<AgentSessionId>, prompt: &str, cwd: &str) -> AgentTurnRequest {
    AgentTurnRequest {
        session_id: session,
        prompt: prompt.to_owned(),
        cwd: cwd.to_owned(),
        guidance: None,
        project_id: None,
        client_type: Some("test".to_owned()),
        thinking_level: None,
        model_id: None,
        role: None,
        images: None,
        decision_token: None,
        agent: None,
        client_context: None,
        advisor: None,
    }
}

/// One ordinary turn through the real handler, awaited until the dispatcher
/// retains its `turn_finished`. Returns the ACK and the ordinary client event
/// types the turn produced on the agent event bus, in emission order.
async fn ordinary_turn(
    state: &AppState,
    session: Option<AgentSessionId>,
    prompt: &str,
    cwd: &str,
) -> (AgentTurnResponse, Vec<String>) {
    let before = {
        let (history, _live) = state.agent_events.subscribe_with_full_replay();
        history.iter().map(|envelope| envelope.seq).max()
    };
    let finished_before = match &session {
        Some(id) => {
            let id = serde_json::to_value(id).unwrap();
            state
                .extension_lifecycle
                .attach()
                .retained
                .iter()
                .filter(|event| {
                    let event = serde_json::to_value(event).unwrap();
                    event["kind"] == "turn_finished" && event["scope"]["session_id"] == id
                })
                .count()
        }
        None => 0,
    };
    let (status, Json(ack)) = crate::agent_turn(
        State(state.clone()),
        Json(turn_request(session, prompt, cwd)),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{ack:?}");
    assert!(ack.ok);
    assert_eq!(ack.status, AgentTurnStatus::Running);
    let session = serde_json::to_value(ack.session_id).unwrap();
    let finished = |session: &Value| {
        state
            .extension_lifecycle
            .attach()
            .retained
            .iter()
            .filter(|event| {
                let event = serde_json::to_value(event).unwrap();
                event["kind"] == "turn_finished" && event["scope"]["session_id"] == *session
            })
            .count()
    };
    wait_for("the turn's lifecycle terminal fact", || {
        finished(&session) > finished_before
    })
    .await;
    let (history, _live) = state.agent_events.subscribe_with_full_replay();
    let session = session.as_str().unwrap().to_owned();
    let types = history
        .iter()
        .filter(|envelope| before.is_none_or(|before| envelope.seq > before))
        .map(|envelope| serde_json::to_value(&envelope.event).unwrap())
        .filter(|event| event.to_string().contains(&session))
        .map(|event| event["type"].as_str().unwrap_or_default().to_owned())
        .collect();
    (ack, types)
}

/// The ordinary client-visible outcome of a turn with its per-run identifiers
/// removed, so runs with and without an extension can be compared exactly.
fn client_shape(ack: &AgentTurnResponse, types: &[String]) -> Value {
    let mut ack = serde_json::to_value(ack).unwrap();
    for field in ["turn_id", "session_id", "event_id_prefix"] {
        ack.as_object_mut().unwrap().remove(field);
    }
    json!({"ack": ack, "events": types})
}

fn publish_daemon_started(lifecycle: &LifecycleDispatcher) {
    lifecycle.publish(LifecycleSource::DaemonStarted {
        daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
        stamp: crate::lifecycle_stamp(),
    });
}

fn pgid(pid: i64) -> i64 {
    // SAFETY: getpgid only reads process-table state for the given pid.
    i64::from(unsafe { libc::getpgid(pid as libc::pid_t) })
}

fn canonical(path: &FsPath) -> String {
    fs::canonicalize(path).unwrap().to_str().unwrap().to_owned()
}

/// Assert no file under `root` contains `needle`, naming the first that does.
fn assert_tree_lacks(root: &FsPath, needle: &str, skip: Option<&FsPath>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.map(Result::unwrap) {
            let path = entry.path();
            if skip.is_some_and(|skip| path.starts_with(skip)) {
                continue;
            }
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() {
                let bytes = fs::read(&path).unwrap();
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(needle),
                    "{} contains the sentinel",
                    path.display()
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// §20 steps 1–7, 9, 11, 12, 14 over a local no-op package.
// ---------------------------------------------------------------------------

/// The Stage A no-op service gate over a local package, in §20 order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stage_a_gate_local_noop_package_end_to_end() {
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&[
        "OCEAN_CONFIG_DIR",
        "OCEAN_MODEL",
        "OCEAN_YOLO",
        "OCEAN_A5_AMBIENT_SENTINEL",
    ]);
    // A daemon-side variable the child must never inherit.
    let ambient = format!("a5-ambient-{}", Uuid::new_v4());
    std::env::set_var("OCEAN_A5_AMBIENT_SENTINEL", &ambient);
    let fixture = Fixture::new();
    let mut state = fake_convene_state(&fixture.config);
    publish_daemon_started(&state.extension_lifecycle);
    let supervisor = with_supervisor(&mut state, fixture.config.path()).await;
    let app = router(state.clone());
    let prompt_sentinel = format!("a5-prompt-sentinel-{}", Uuid::new_v4());
    let cwd = fixture
        .sources
        .path()
        .join(format!("a5-cwd-sentinel-{}", Uuid::new_v4()));
    fs::create_dir(&cwd).unwrap();
    let cwd = cwd.to_str().unwrap().to_owned();

    // Baseline: an ordinary turn with no extension installed at all.
    let (baseline_ack, baseline_types) = ordinary_turn(&state, None, "ping", &cwd).await;
    let baseline = client_shape(&baseline_ack, &baseline_types);
    assert!(baseline_types.iter().any(|kind| kind == "turn_finished"));

    // Any Git acquisition for this registry would go through a counting
    // resolver and a canary `git`; local install and update must touch
    // neither (§13.1 offline).
    let git_canary = fixture.sources.path().join("GIT_CANARY_RAN");
    let fake_git = fixture.sources.path().join("fake-git");
    fs::write(
        &fake_git,
        format!(
            "#!/bin/sh\nprintf ran > '{}'\nexit 1\n",
            git_canary.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fake_git, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let lookups = Arc::new(CountingResolver::default());
    let _offline_proof = super::super::transaction::git::test_support::register(
        fixture.config.path(),
        super::super::transaction::git::GitAcquirer::system()
            .with_resolver(Arc::clone(&lookups) as _)
            .with_program(fake_git),
    );

    // 1. Local install (no network is reachable from this path): committed,
    //    untrusted, and nothing ran.
    let package = gate_package(&fixture, "noop", "1.0.0", "");
    let (status, installed) = install_local(&app, 0, &package).await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    assert_committed(&installed, 1);
    assert_eq!(installed["mutation"]["effective"], false);
    let digest = installed["mutation"]["digest"].as_str().unwrap().to_owned();
    assert!(
        !data_dir(&fixture).exists(),
        "install created mutable state"
    );
    fixture.assert_no_canary_ran();

    // 2. list / inspect / doctor / status are read-only and execute nothing.
    for uri in [
        "/v1/extensions".to_owned(),
        format!("/v1/extensions/{ID}/inspect"),
        format!("/v1/extensions/{ID}/doctor"),
        format!("/v1/extensions/{ID}/status"),
    ] {
        let (status, body) = get_json(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
    }
    let (_, doctor) = get_json(&app, &format!("/v1/extensions/{ID}/doctor")).await;
    assert_eq!(doctor["checks"]["package_code_executed"], false, "{doctor}");
    let (_, idle) = get_json(&app, &format!("/v1/extensions/{ID}/status")).await;
    assert_eq!(idle["services"], json!([]));
    assert_eq!(starts(&fixture), 0);
    fixture.assert_no_canary_ran();

    // 3. Trust: the preview carries the complete notice; the exact apply
    //    publishes the strict grant row but neither enables nor starts.
    let (status, preview, trusted) = trust(&app, 1, &digest, json!({}), json!([])).await;
    assert_eq!(status, StatusCode::OK, "{trusted}");
    assert_eq!(
        preview["preview"]["added"]["service_grants"][0]["native_process_ack"],
        true
    );
    assert_committed(&trusted, 2);
    assert_eq!(trusted["mutation"]["effective"], false);
    let grants: Value =
        serde_json::from_slice(&fs::read(fixture.root().join("service-grants.json")).unwrap())
            .unwrap();
    assert_eq!(grants["state_revision"], 2);
    assert_eq!(
        grants["service_grants"],
        json!([{"id": ID, "digest": digest, "service_id": "lifecycle", "native_process_ack": true, "secret_bindings": []}])
    );
    let (_, inspected) = get_json(&app, &format!("/v1/extensions/{ID}/inspect")).await;
    assert_eq!(inspected["extension"]["trusted"], true, "{inspected}");
    assert_eq!(inspected["extension"]["enabled"], false, "{inspected}");
    assert_eq!(starts(&fixture), 0);

    // 4. Enable: host-injected identity, assigned roots, minimal environment,
    //    readiness, and exactly one live process group.
    let (status, enabled) = scope(&app, ID, "enable", 2).await;
    assert_eq!(status, StatusCode::OK, "{enabled}");
    assert_committed(&enabled, 3);
    let (first, status_a) = healthy(&app).await;
    let epoch_a = status_a["services"][0]["activation_epoch"].clone();
    wait_for("ready frame", || {
        frames_of(&fixture, first)
            .iter()
            .any(|f| f["frame"] == "ready")
    })
    .await;
    let hello = &frames_of(&fixture, first)[0];
    assert_eq!(hello["frame"], "host_hello");
    assert_eq!(
        hello["daemon_boot_id"],
        json!(state.extension_lifecycle.daemon_boot_id())
    );
    assert_eq!(
        hello["identity"],
        json!({
            "package_id": ID,
            "package_version": "1.0.0",
            "package_digest": digest,
            "service_id": "lifecycle",
            "activation_revision": 3,
            "activation_epoch": epoch_a,
            "replay_floor": status_a["services"][0]["replay_floor"],
        })
    );
    let env = fs::read_to_string(data_dir(&fixture).join(format!("env-{first}"))).unwrap();
    assert!(
        !env.contains(&ambient),
        "an ambient daemon variable was inherited"
    );
    let env: Vec<(&str, &str)> = env
        .lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(name, _)| !matches!(*name, "SHLVL" | "_"))
        .collect();
    let names: BTreeSet<&str> = env.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "HOME",
            "PATH",
            "PWD",
            "TMPDIR",
            "XDG_CACHE_HOME",
            "XDG_STATE_HOME"
        ])
    );
    let value = |name: &str| env.iter().find(|(n, _)| *n == name).unwrap().1.to_owned();
    // The host hands the child descriptor-derived paths — a volfs
    // `/.vol/<dev>/<ino>` spelling on macOS and `/proc/self/fd/<n>` on Linux,
    // which only the child can resolve — so the child records the inode each
    // path names from its own context, and the gate compares it with the
    // directory or file the root must be.
    let state_root = fixture.root().join("state").join(ID);
    let hex = digest.strip_prefix("sha256:").unwrap();
    let package_root = fixture.root().join("store").join(ID).join(hex);
    assert_eq!(value("PATH"), "/usr/bin:/bin");
    let ids: std::collections::HashMap<String, u64> =
        fs::read_to_string(data_dir(&fixture).join(format!("ids-{first}")))
            .unwrap()
            .lines()
            .map(|line| {
                let (name, inode) = line.split_once(' ').unwrap();
                (name.to_owned(), inode.trim().parse().unwrap_or(0))
            })
            .collect();
    let inode = |path: &FsPath| {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(path).unwrap().ino()
    };
    let connection_dirs: Vec<PathBuf> = fs::read_dir(state_root.join("tmp/lifecycle"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        connection_dirs.len(),
        1,
        "one connection temp root per launch"
    );
    assert!(connection_dirs[0].join("tmp-proof").exists());
    for (name, expected) in [
        ("HOME", state_root.join("data")),
        ("XDG_STATE_HOME", state_root.join("data")),
        ("XDG_CACHE_HOME", state_root.join("cache")),
        ("TMPDIR", connection_dirs[0].clone()),
        ("PWD", package_root.clone()),
        ("CWD", package_root.clone()),
        ("ARGV0", package_root.join("services/lifecycle")),
    ] {
        assert_eq!(
            ids[name],
            inode(&expected),
            "{name} is not {}",
            expected.display()
        );
    }
    let argv = fs::read_to_string(data_dir(&fixture).join(format!("argv-{first}"))).unwrap();
    let (_entry, rest) = argv.split_once('|').unwrap();
    assert_eq!(rest, "", "the manifest declares no arguments");
    let child_of_first = grandchild(&fixture, first);
    assert_eq!(pgid(first), first, "the service leads its own group");
    assert_eq!(pgid(child_of_first), first, "the grandchild shares it");
    assert_eq!(starts(&fixture), 1);

    // 5. Ordinary sessions — new, then resumed — deliver exact scoped
    //    metadata and nothing else, and the client-visible turn is unchanged.
    let (new_ack, new_types) = ordinary_turn(&state, None, &prompt_sentinel, &cwd).await;
    assert_eq!(client_shape(&new_ack, &new_types), baseline);
    let session = serde_json::to_value(new_ack.session_id).unwrap();
    let (resumed_ack, resumed_types) =
        ordinary_turn(&state, Some(new_ack.session_id), &prompt_sentinel, &cwd).await;
    assert_eq!(resumed_ack.session_id, new_ack.session_id);
    assert!(!resumed_types.iter().any(|kind| kind == "session_created"));
    wait_for("both turns delivered", || {
        events_of(&fixture, first)
            .iter()
            .filter(|e| e["kind"] == "turn_finished" && e["scope"]["session_id"] == session)
            .count()
            == 2
    })
    .await;
    let delivered = events_of(&fixture, first);
    let kinds: Vec<&str> = delivered
        .iter()
        .filter(|event| event["scope"]["session_id"] == session)
        .map(|event| event["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "session_started",
            "turn_started",
            "turn_finished",
            "turn_started",
            "turn_finished"
        ],
        "a resumed session omits session_started"
    );
    assert!(delivered
        .windows(2)
        .all(|pair| sequence(&pair[0]) < sequence(&pair[1])));
    for event in &delivered {
        assert_eq!(event["daemon_boot_id"], hello["daemon_boot_id"]);
        let kind = event["kind"].as_str().unwrap();
        assert!(
            [
                "daemon_started",
                "session_started",
                "turn_started",
                "turn_finished"
            ]
            .contains(&kind),
            "an unsubscribed kind was delivered: {event}"
        );
        assert_eq!(event["scope"]["project_id"], Value::Null, "{event}");
    }
    let raw = fs::read_to_string(data_dir(&fixture).join("frames")).unwrap();
    assert!(
        !raw.contains(&prompt_sentinel),
        "a prompt reached the service"
    );
    assert!(!raw.contains(&cwd), "a cwd reached the service");
    assert!(!raw.contains("a5-cwd-sentinel"));

    // 7. Disable → events → re-enable with the stale cursor: the new epoch's
    //    floor hides every interval fact and the stale resume is reset.
    let last_seen = sequence(delivered.last().unwrap());
    let (status, disabled) = scope(&app, ID, "disable", 3).await;
    assert_eq!(status, StatusCode::OK, "{disabled}");
    assert_eq!(disabled["mutation"]["reap"], "complete");
    assert!(!process_alive(first), "disable returned before reap");
    assert!(
        !process_alive(child_of_first),
        "the grandchild survived disable"
    );
    let shutdown = frames_of(&fixture, first);
    assert_eq!(
        shutdown.last().unwrap()["frame"],
        "shutdown",
        "{shutdown:?}"
    );
    assert_eq!(shutdown.last().unwrap()["reason"], "disabled");
    let (interval_ack, _) = ordinary_turn(&state, None, "ping", &cwd).await;
    let interval_session = serde_json::to_value(interval_ack.session_id).unwrap();
    let interval_high = state.extension_lifecycle.current_sequence().0;
    assert!(interval_high > last_seen);
    let (status, reenabled) = scope(&app, ID, "enable", 4).await;
    assert_eq!(status, StatusCode::OK, "{reenabled}");
    let (second, status_b) = healthy(&app).await;
    assert_ne!(second, first);
    assert_ne!(status_b["services"][0]["activation_epoch"], epoch_a);
    let floor_b: u64 = status_b["services"][0]["replay_floor"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        floor_b >= interval_high,
        "the new floor admits interval facts"
    );
    wait_for("reset or live boundary for the stale cursor", || {
        frames_of(&fixture, second)
            .iter()
            .any(|f| f["frame"] == "reset")
    })
    .await;
    let reset = frames_of(&fixture, second)
        .into_iter()
        .find(|f| f["frame"] == "reset")
        .unwrap();
    assert_eq!(reset["reason"], "activation_changed", "{reset}");
    let (after_ack, _) = ordinary_turn(&state, None, "ping", &cwd).await;
    let after_session = serde_json::to_value(after_ack.session_id).unwrap();
    wait_for("live delivery after re-enable", || {
        events_of(&fixture, second)
            .iter()
            .any(|e| e["kind"] == "turn_finished" && e["scope"]["session_id"] == after_session)
    })
    .await;
    for event in events_of(&fixture, second) {
        let sequence = sequence(&event);
        assert!(
            sequence <= last_seen || sequence > interval_high,
            "interval fact {sequence} disclosed after re-enable: {event}"
        );
        assert_ne!(event["scope"]["session_id"], interval_session);
    }

    // 9. Daemon restart: graceful stop, then a new dispatcher (new boot id)
    //    and the production host seam over the same config directory.
    let old_boot = state.extension_lifecycle.daemon_boot_id();
    let child_of_second = grandchild(&fixture, second);
    state.extension_lifecycle.stop_publication();
    supervisor.shutdown().await;
    assert!(!process_alive(second), "daemon shutdown left the service");
    assert!(
        !process_alive(child_of_second),
        "daemon shutdown left a grandchild"
    );
    let stopped = frames_of(&fixture, second);
    assert_eq!(stopped.last().unwrap()["frame"], "shutdown");
    assert_eq!(stopped.last().unwrap()["reason"], "daemon_stopping");

    let lifecycle = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
    publish_daemon_started(&lifecycle);
    let supervisor = crate::start_extension_host(
        fixture.config.path(),
        Arc::clone(&lifecycle),
        HashSet::new(),
    )
    .await;
    state.extension_lifecycle = Arc::clone(&lifecycle);
    state.extension_supervisor = Some(Arc::clone(&supervisor));
    let app = router(state.clone());
    let (third, status_c) = healthy(&app).await;
    assert_eq!(status_c["services"].as_array().unwrap().len(), 1);
    assert_ne!(lifecycle.daemon_boot_id(), old_boot);
    wait_for("restart handshake", || {
        frames_of(&fixture, third)
            .iter()
            .any(|f| f["frame"] == "reset")
    })
    .await;
    let restarted = frames_of(&fixture, third);
    assert_eq!(
        restarted[0]["daemon_boot_id"],
        json!(lifecycle.daemon_boot_id())
    );
    let reset = restarted.iter().find(|f| f["frame"] == "reset").unwrap();
    assert_eq!(reset["reason"], "boot_changed", "{reset}");
    for event in restarted.iter().filter(|f| f["frame"] == "event") {
        assert_eq!(event["daemon_boot_id"], json!(lifecycle.daemon_boot_id()));
    }
    assert!(
        fs::read_to_string(data_dir(&fixture).join("saw-retained-data"))
            .unwrap()
            .lines()
            .any(|pid| pid == third.to_string()),
        "package data did not survive the restart"
    );
    assert_eq!(starts(&fixture), 3, "exactly one service per activation");

    // 11. Active update/remove refuse; after disable, an exact update installs
    //     an untrusted digest and starts nothing until trust + enable.
    let revision = fixture.revision();
    let package_v2 = gate_package(&fixture, "noop-v2", "2.0.0", "");
    let (status, active) = post_op(
        &app,
        &format!("/v1/extensions/{ID}/update"),
        json!({"expected_state_revision": revision, "source": {"kind": "local-path", "path": package_v2}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{active}");
    assert_precommit(&active, "extension_active", revision);
    let (status, active) = send(
        &app,
        Method::DELETE,
        &format!("/v1/extensions/{ID}"),
        Some(json!({"expected_state_revision": revision, "purge_state": false})),
        Auth::Operator,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{active}");
    assert_precommit(&active, "extension_active", revision);
    assert!(process_alive(third));
    let child_of_third = grandchild(&fixture, third);
    let (status, _) = scope(&app, ID, "disable", revision).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!process_alive(third) && !process_alive(child_of_third));
    let (status, updated) = post_op(
        &app,
        &format!("/v1/extensions/{ID}/update"),
        json!({"expected_state_revision": revision + 1, "source": {"kind": "local-path", "path": package_v2}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    let digest_v2 = updated["mutation"]["digest"].as_str().unwrap().to_owned();
    assert_ne!(digest_v2, digest);
    // §12.4 update column: data/ and cache/ are retained, no temp root
    // survives the reap, and the local update touched no resolver or `git`.
    let state_dir = fixture.root().join("state").join(ID);
    assert!(
        state_dir.join("data/frames").exists(),
        "update dropped data/"
    );
    assert!(
        state_dir.join("cache/cache-proof").exists(),
        "update dropped cache/"
    );
    assert!(
        fs::read_dir(state_dir.join("tmp/lifecycle"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true),
        "a connection temp root survived the reap"
    );
    assert_eq!(lookups.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(!git_canary.exists(), "a local source ran git");
    let (_, inspected) = get_json(&app, &format!("/v1/extensions/{ID}/inspect")).await;
    assert_eq!(inspected["extension"]["trusted"], false, "{inspected}");
    // Disable removed the global row and update grants no trust, so enable
    // is refused until a separate trust transition.
    let (status, refused) = scope(&app, ID, "enable", revision + 2).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_precommit(&refused, "trust_required", revision + 2);
    let (_, idle) = get_json(&app, &format!("/v1/extensions/{ID}/status")).await;
    assert_eq!(idle["services"], json!([]));
    assert_eq!(starts(&fixture), 3);

    // 12. Remove preserving state, identical-digest reinstall (untrusted),
    //     retrust + enable reuses data/, then disable + purge removes it all.
    let (status, removed) = send(
        &app,
        Method::DELETE,
        &format!("/v1/extensions/{ID}"),
        Some(json!({"expected_state_revision": revision + 2, "purge_state": false})),
        Auth::Operator,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert!(
        state_dir.join("data/frames").exists(),
        "remove dropped data/"
    );
    assert!(!state_dir.join("cache").exists(), "remove kept cache/");
    assert!(
        fs::read_dir(state_dir.join("tmp/lifecycle"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true),
        "a connection temp root survived"
    );
    let grants: Value =
        serde_json::from_slice(&fs::read(fixture.root().join("service-grants.json")).unwrap())
            .unwrap();
    assert_eq!(grants["service_grants"], json!([]));
    let (status, _) = get_json(&app, &format!("/v1/extensions/{ID}/inspect")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, reinstalled) = install_local(&app, revision + 3, &package_v2).await;
    assert_eq!(status, StatusCode::OK, "{reinstalled}");
    assert_eq!(reinstalled["mutation"]["digest"], digest_v2.as_str());
    let (_, inspected) = get_json(&app, &format!("/v1/extensions/{ID}/inspect")).await;
    assert_eq!(
        inspected["extension"]["trusted"], false,
        "identical bytes regained trust"
    );
    assert_eq!(inspected["extension"]["enabled"], false);
    let (status, _, _) = trust(&app, revision + 4, &digest_v2, json!({}), json!([])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = scope(&app, ID, "enable", revision + 5).await;
    assert_eq!(status, StatusCode::OK);
    let (fourth, _) = healthy(&app).await;
    assert!(
        fs::read_to_string(data_dir(&fixture).join("saw-retained-data"))
            .unwrap()
            .lines()
            .any(|pid| pid == fourth.to_string()),
        "the reinstalled package did not see its retained data/"
    );
    wait_for("cache recreated at activation", || {
        state_dir.join("cache/cache-proof").exists()
    })
    .await;
    let child_of_fourth = grandchild(&fixture, fourth);
    let trust_rows: Value =
        serde_json::from_slice(&fs::read(fixture.root().join("trust.json")).unwrap()).unwrap();
    assert!(trust_rows.to_string().contains(&digest_v2), "{trust_rows}");
    let (status, _) = scope(&app, ID, "disable", revision + 6).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!process_alive(fourth) && !process_alive(child_of_fourth));
    let (status, purged) = send(
        &app,
        Method::DELETE,
        &format!("/v1/extensions/{ID}"),
        Some(json!({"expected_state_revision": revision + 7, "purge_state": true})),
        Auth::Operator,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{purged}");
    assert!(!state_dir.exists(), "purge kept package state");
    // This remove revoked a live trust + service grant (non-vacuously).
    let grants: Value =
        serde_json::from_slice(&fs::read(fixture.root().join("service-grants.json")).unwrap())
            .unwrap();
    assert_eq!(grants["service_grants"], json!([]));
    let trust_rows: Value =
        serde_json::from_slice(&fs::read(fixture.root().join("trust.json")).unwrap()).unwrap();
    assert!(!trust_rows.to_string().contains(ID), "{trust_rows}");
    let enabled_rows: Value =
        serde_json::from_slice(&fs::read(fixture.root().join("enabled.json")).unwrap()).unwrap();
    assert!(!enabled_rows.to_string().contains(ID), "{enabled_rows}");
    assert!(
        !fixture.root().join("store").join(ID).exists()
            || fixture.entries(&format!("store/{ID}")).is_empty(),
        "remove kept an unreferenced payload"
    );

    // 14. With the extension absent, an ordinary turn is indistinguishable
    //     from the baseline that ran before anything was installed.
    let (absent_ack, absent_types) = ordinary_turn(&state, None, "ping", &cwd).await;
    assert_eq!(client_shape(&absent_ack, &absent_types), baseline);

    supervisor.shutdown().await;
    fixture.assert_no_canary_ran();
}

// ---------------------------------------------------------------------------
// §19.2 "never delays a turn" over a live service that never reads.
// ---------------------------------------------------------------------------

/// A service that completes its handshake and then never reads stdin again
/// cannot delay ordinary turns. A burst of concurrent turns publishes far more
/// than the 256-frame data queue and the 64 KiB pipe hold, so the host both
/// coalesces lost frames into `lag` and blocks on a full stdin. Every turn
/// is still acknowledged and finishes within its bound. The blocked write
/// fails the connection at the 2 s deadline (as a protocol failure), no
/// earlier than 2 s after the burst began and within a bounded slack of its
/// end, and the process group is reaped.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stage_a_gate_stalled_service_never_delays_ordinary_turns() {
    const BURST: usize = 160;
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let fixture = Fixture::new();
    let mut state = fake_convene_state(&fixture.config);
    let supervisor = with_supervisor(&mut state, fixture.config.path()).await;
    let app = router(state.clone());
    let stalled = format!(
        "#!/bin/sh\nIFS= read -r hello\nprintf '%s\\n' '{{\"protocol\":\"ocean.extension.service\",\"version\":1,\"frame\":\"service_hello\",\"subscriptions\":{SUBSCRIPTIONS},\"resume\":null}}'\nIFS= read -r ready\nprintf stalled > \"$HOME/stalled\"\nexec sleep 600\n"
    );
    let package = gate_package_with(&fixture, "stalled", "1.0.0", "", &stalled);
    let (_, installed) = install_local(&app, 0, &package).await;
    let digest = installed["mutation"]["digest"].as_str().unwrap().to_owned();
    let (status, _, _) = trust(&app, 1, &digest, json!({}), json!([])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = scope(&app, ID, "enable", 2).await;
    assert_eq!(status, StatusCode::OK);
    let (pid, _) = healthy(&app).await;
    let child = {
        wait_for("stalled marker", || {
            data_dir(&fixture).join("stalled").exists()
        })
        .await;
        pid
    };

    let cwd = fixture.sources.path().to_str().unwrap().to_owned();
    let before = state.extension_lifecycle.current_sequence().0;
    let burst_started = tokio::time::Instant::now();
    let mut turns = tokio::task::JoinSet::new();
    for _ in 0..BURST {
        let state = state.clone();
        let cwd = cwd.clone();
        turns.spawn(async move {
            let started = tokio::time::Instant::now();
            let (status, Json(ack)) =
                crate::agent_turn(State(state.clone()), Json(turn_request(None, "ping", &cwd)))
                    .await;
            assert_eq!(status, StatusCode::ACCEPTED, "{ack:?}");
            let session = serde_json::to_value(ack.session_id).unwrap();
            wait_for("burst turn finished", || {
                state
                    .extension_lifecycle
                    .attach()
                    .retained
                    .iter()
                    .any(|event| {
                        let event = serde_json::to_value(event).unwrap();
                        event["kind"] == "turn_finished" && event["scope"]["session_id"] == session
                    })
            })
            .await;
            started.elapsed()
        });
    }
    let mut slowest = Duration::ZERO;
    while let Some(elapsed) = tokio::time::timeout(Duration::from_secs(30), turns.join_next())
        .await
        .expect("a stalled service delayed an ordinary turn")
    {
        slowest = slowest.max(elapsed.unwrap());
    }
    let burst_ended = tokio::time::Instant::now();
    assert!(
        slowest < Duration::from_secs(20),
        "slowest turn took {slowest:?}"
    );
    let published = state.extension_lifecycle.current_sequence().0 - before;
    assert!(
        published as usize >= 3 * BURST,
        "the burst published {published} facts"
    );

    // The stalled connection lagged and then failed at the write deadline:
    // `stopping` (with its fixed reason) is published the moment the
    // connection fails, and the terminal `unhealthy` follows the §10.5
    // bounded cleanup (shutdown write deadline, TERM, KILL, reap).
    let stopping_at = loop {
        let (_, body) = get_json(&app, &format!("/v1/extensions/{ID}/status")).await;
        let state = body["services"][0]["state"].clone();
        if state == "stopping" || state == "unhealthy" {
            assert_eq!(
                body["services"][0]["reason"], "protocol_violation",
                "{body}"
            );
            break tokio::time::Instant::now();
        }
        assert_eq!(state, "healthy", "{body}");
        assert!(
            burst_ended.elapsed() < Duration::from_secs(10),
            "the blocked write never failed: {body}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert!(
        stopping_at.duration_since(burst_started) >= Duration::from_secs(2),
        "the connection failed before its 2 s write deadline"
    );
    assert!(
        stopping_at.duration_since(burst_ended) < Duration::from_millis(3500),
        "the blocked write outlived its 2 s deadline: {:?} after the burst",
        stopping_at.duration_since(burst_ended)
    );
    let failed =
        wait_for_status(&app, ID, |body| body["services"][0]["state"] == "unhealthy").await;
    assert!(
        stopping_at.elapsed() < Duration::from_secs(8),
        "cleanup exceeded the §10.5 bounds"
    );
    let row = &failed["services"][0];
    assert!(
        row["lag_count"].as_u64() > Some(0),
        "no lag recorded: {failed}"
    );
    assert_eq!(row["reason"], "protocol_violation", "{failed}");
    assert_eq!(row["pid"], Value::Null);
    assert!(!process_alive(child), "the stalled group was not reaped");
    supervisor.shutdown().await;
    fixture.assert_no_canary_ran();
}

// ---------------------------------------------------------------------------
// §20 step 5 with a permission and a tool, through the real turn path.
// ---------------------------------------------------------------------------

const ALL_PRODUCED: &str = r#"["daemon_started","session_started","turn_started","permission_requested","permission_resolved","tool_started","tool_finished","turn_finished","daemon_stopping"]"#;

/// An ordinary turn on the keyless `fake-tool` model asks for `write`, the
/// real daemon permission policy suspends it, the operator allows it through
/// the existing waiter, and the real tool runs. The subscribed service
/// receives exactly the ratified metadata sequence with host UUIDs, and no
/// argument, path, content, or runtime tool-call id reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stage_a_gate_live_permission_and_tool_facts_are_metadata_only() {
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let fixture = Fixture::new();
    let mut state = fake_convene_state(&fixture.config);
    // The gating (non-yolo) policy and the tool-calling fake model.
    std::env::remove_var("OCEAN_YOLO");
    std::env::set_var("OCEAN_MODEL", ocean_runtime::FAKE_TOOL_MODEL);
    state.runtime = Arc::new(
        crate::AgentRuntime::with_config_dir(fixture.config.path().to_path_buf())
            .expect("fake-tool runtime"),
    );
    let target = FsPath::new(ocean_runtime::FAKE_TOOL_TARGET_PATH);
    let _ = fs::remove_file(target);
    let supervisor = with_supervisor(&mut state, fixture.config.path()).await;
    let app = router(state.clone());

    let service = gate_service().replace(SUBSCRIPTIONS, ALL_PRODUCED);
    let package = gate_package_with(&fixture, "tools", "1.0.0", "", &service);
    let manifest = FsPath::new(&package).join("ocean-extension.toml");
    let rewritten = fs::read_to_string(&manifest).unwrap().replace(
        &format!("events = {SUBSCRIPTIONS}"),
        &format!("events = {ALL_PRODUCED}"),
    );
    fs::write(&manifest, rewritten).unwrap();
    let (_, installed) = install_local(&app, 0, &package).await;
    let digest = installed["mutation"]["digest"].as_str().unwrap().to_owned();
    let (status, _, _) = trust(&app, 1, &digest, json!({}), json!([])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = scope(&app, ID, "enable", 2).await;
    assert_eq!(status, StatusCode::OK);
    let (pid, _) = healthy(&app).await;
    wait_for("ready", || {
        frames_of(&fixture, pid)
            .iter()
            .any(|f| f["frame"] == "ready")
    })
    .await;

    let cwd = fixture.sources.path().to_str().unwrap().to_owned();
    let (status, Json(ack)) = crate::agent_turn(
        State(state.clone()),
        Json(turn_request(None, "write the file", &cwd)),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{ack:?}");
    // The real policy suspends the turn on a waiter; allow it once.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let sender = {
            let mut permissions = state.permissions.write().await;
            permissions
                .values_mut()
                .find_map(|waiter| waiter.sender.take())
        };
        if let Some(sender) = sender {
            sender
                .send(crate::AgentPermissionDecision::Allow)
                .expect("decision");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no permission waiter"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let session = serde_json::to_value(ack.session_id).unwrap();
    wait_for("tool turn delivered", || {
        events_of(&fixture, pid)
            .iter()
            .any(|e| e["kind"] == "turn_finished" && e["scope"]["session_id"] == session)
    })
    .await;
    assert!(target.exists(), "the allowed tool did not run");

    let delivered: Vec<Value> = events_of(&fixture, pid)
        .into_iter()
        .filter(|event| event["scope"]["session_id"] == session)
        .collect();
    let kinds: Vec<&str> = delivered
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "session_started",
            "turn_started",
            "permission_requested",
            "permission_resolved",
            "tool_started",
            "tool_finished",
            "turn_finished"
        ]
    );
    let by_kind = |kind: &str| delivered.iter().find(|e| e["kind"] == kind).unwrap();
    assert_eq!(
        by_kind("permission_requested")["metadata"],
        json!({"tool_name": "write"})
    );
    assert_eq!(
        by_kind("permission_resolved")["metadata"],
        json!({"outcome": "allowed"})
    );
    assert_eq!(
        by_kind("permission_requested")["scope"]["permission_id"],
        by_kind("permission_resolved")["scope"]["permission_id"]
    );
    assert!(Uuid::parse_str(
        by_kind("permission_requested")["scope"]["permission_id"]
            .as_str()
            .unwrap()
    )
    .is_ok());
    assert_eq!(
        by_kind("tool_started")["metadata"],
        json!({"tool_name": "write"})
    );
    let finished = &by_kind("tool_finished")["metadata"];
    assert_eq!(finished["tool_name"], "write");
    assert_eq!(finished["outcome"], "success");
    let tool_call = by_kind("tool_started")["scope"]["tool_call_id"].clone();
    assert_eq!(by_kind("tool_finished")["scope"]["tool_call_id"], tool_call);
    assert!(
        Uuid::parse_str(tool_call.as_str().unwrap()).is_ok(),
        "host UUID"
    );
    assert_eq!(by_kind("turn_finished")["metadata"]["outcome"], "completed");
    let raw = fs::read_to_string(data_dir(&fixture).join("frames")).unwrap();
    for forbidden in [
        ocean_runtime::FAKE_TOOL_TARGET_PATH,
        ocean_runtime::FAKE_TOOL_CALL_ID,
        "write the file",
    ] {
        assert!(!raw.contains(forbidden), "{forbidden} reached the service");
    }

    // Deny: the runtime's compatibility PermissionDenied pair is never
    // translated into execution facts, because no tool ran.
    let _ = fs::remove_file(target);
    let (status, Json(ack)) = crate::agent_turn(
        State(state.clone()),
        Json(turn_request(None, "write the file", &cwd)),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{ack:?}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let sender = {
            let mut permissions = state.permissions.write().await;
            permissions
                .values_mut()
                .find_map(|waiter| waiter.sender.take())
        };
        if let Some(sender) = sender {
            sender
                .send(crate::AgentPermissionDecision::Deny {
                    reason: "a5-deny-reason-sentinel".to_owned(),
                })
                .expect("decision");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no permission waiter"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let denied_session = serde_json::to_value(ack.session_id).unwrap();
    wait_for("denied turn delivered", || {
        events_of(&fixture, pid)
            .iter()
            .any(|e| e["kind"] == "turn_finished" && e["scope"]["session_id"] == denied_session)
    })
    .await;
    assert!(!target.exists(), "a denied tool ran");
    let denied: Vec<Value> = events_of(&fixture, pid)
        .into_iter()
        .filter(|event| event["scope"]["session_id"] == denied_session)
        .collect();
    let kinds: Vec<&str> = denied
        .iter()
        .map(|event| event["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        [
            "session_started",
            "turn_started",
            "permission_requested",
            "permission_resolved",
            "turn_finished"
        ],
        "a denial fabricated tool execution"
    );
    assert_eq!(denied[3]["metadata"], json!({"outcome": "denied"}));
    let raw = fs::read_to_string(data_dir(&fixture).join("frames")).unwrap();
    assert!(!raw.contains("a5-deny-reason-sentinel"));

    supervisor.shutdown().await;
    let _ = fs::remove_file(target);
    fixture.assert_no_canary_ran();
}

// ---------------------------------------------------------------------------
// §19.2 / §20 step 7: live project-scope widening.
// ---------------------------------------------------------------------------

async fn create_project(state: &AppState, name: &str, root: &FsPath) -> Uuid {
    let (status, Json(created)) = crate::project_create(
        State(state.clone()),
        Json(
            serde_json::from_value(json!({"name": name, "workspace_root": root.to_str().unwrap()}))
                .unwrap(),
        ),
    )
    .await;
    assert!(status.is_success(), "{:?}", created.error);
    created.project.unwrap().id
}

/// A service enabled for project A receives A's facts and never B's. Widening
/// to A+B mints a new epoch whose floor is at or above every fact B produced
/// while it was out of scope, the stale cursor is reset, and none of B's
/// interval facts is ever delivered; B's facts after the widening are.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stage_a_gate_project_scope_widening_never_replays_interval_facts() {
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let fixture = Fixture::new();
    let mut state = fake_convene_state(&fixture.config);
    let supervisor = with_supervisor(&mut state, fixture.config.path()).await;
    let app = router(state.clone());
    let root_a = fixture.sources.path().join("project-a");
    let root_b = fixture.sources.path().join("project-b");
    let project_a = create_project(&state, "a", &root_a).await;
    let project_b = create_project(&state, "b", &root_b).await;
    let (cwd_a, cwd_b) = (canonical(&root_a), canonical(&root_b));

    let package = gate_package(&fixture, "scoped", "1.0.0", "");
    let (_, installed) = install_local(&app, 0, &package).await;
    let digest = installed["mutation"]["digest"].as_str().unwrap().to_owned();
    let (status, _, _) = trust(&app, 1, &digest, json!({}), json!([])).await;
    assert_eq!(status, StatusCode::OK);
    let enable_project = |expected: u64, project: Uuid| {
        let app = app.clone();
        async move {
            post_op(
                &app,
                &format!("/v1/extensions/{ID}/enable"),
                json!({"expected_state_revision": expected, "scope": {"kind": "project", "project_id": project}}),
            )
            .await
        }
    };
    let (status, enabled) = enable_project(2, project_a).await;
    assert_eq!(status, StatusCode::OK, "{enabled}");
    let (first, status_a) = healthy(&app).await;
    let epoch_a = status_a["services"][0]["activation_epoch"].clone();

    let (in_a, _) = ordinary_turn(&state, None, "ping", &cwd_a).await;
    let in_a = serde_json::to_value(in_a.session_id).unwrap();
    wait_for("A delivered", || {
        events_of(&fixture, first)
            .iter()
            .any(|e| e["kind"] == "turn_finished" && e["scope"]["session_id"] == in_a)
    })
    .await;
    let (in_b, _) = ordinary_turn(&state, None, "ping", &cwd_b).await;
    let in_b = serde_json::to_value(in_b.session_id).unwrap();
    let b_high = state.extension_lifecycle.current_sequence().0;
    for event in events_of(&fixture, first) {
        assert_ne!(
            event["scope"]["session_id"], in_b,
            "out-of-scope B fact delivered"
        );
        if event["kind"] != "daemon_started" {
            assert_eq!(event["scope"]["project_id"], json!(project_a), "{event}");
        }
    }

    // Widen to A+B.
    let (status, widened) = enable_project(3, project_b).await;
    assert_eq!(status, StatusCode::OK, "{widened}");
    let body = wait_for_status(&app, ID, |body| {
        body["services"][0]["state"] == "healthy"
            && body["services"][0]["activation_epoch"] != epoch_a
            && body["services"][0]["pid"].is_u64()
    })
    .await;
    let second = body["services"][0]["pid"].as_i64().unwrap();
    let floor: u64 = body["services"][0]["replay_floor"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        floor >= b_high,
        "the widened floor {floor} admits B's interval facts"
    );
    wait_for("stale cursor reset", || {
        frames_of(&fixture, second)
            .iter()
            .any(|f| f["frame"] == "reset")
    })
    .await;
    let reset = frames_of(&fixture, second)
        .into_iter()
        .find(|f| f["frame"] == "reset")
        .unwrap();
    assert_eq!(reset["reason"], "activation_changed", "{reset}");

    let (after_b, _) = ordinary_turn(&state, None, "ping", &cwd_b).await;
    let after_b = serde_json::to_value(after_b.session_id).unwrap();
    wait_for("B delivered after widening", || {
        events_of(&fixture, second)
            .iter()
            .any(|e| e["kind"] == "turn_finished" && e["scope"]["session_id"] == after_b)
    })
    .await;
    for event in events_of(&fixture, second) {
        assert_ne!(
            event["scope"]["session_id"], in_b,
            "B's interval fact replayed"
        );
        assert!(sequence(&event) > b_high || event["kind"] == "daemon_started");
    }
    assert_eq!(
        events_of(&fixture, second)
            .iter()
            .find(|e| e["scope"]["session_id"] == after_b)
            .unwrap()["scope"]["project_id"],
        json!(project_b)
    );

    supervisor.shutdown().await;
    fixture.assert_no_canary_ran();
}

// ---------------------------------------------------------------------------
// §19.3 secret sentinel through the integrated HTTP path.
// ---------------------------------------------------------------------------

/// A secret bound through the HTTP trust route reaches exactly the confirmed
/// child variable, and its value appears in no registry file, journal, HTTP
/// response, runtime status, lifecycle event, or the child's argv; the one
/// stderr line that echoes it is counted as a redaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stage_a_gate_bound_secret_never_leaves_the_spawn_environment() {
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let source = format!("OCEAN_A5_GATE_SECRET_{}", Uuid::new_v4().simple()).to_uppercase();
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    struct SecretGuard(String);
    impl Drop for SecretGuard {
        fn drop(&mut self) {
            std::env::remove_var(&self.0);
        }
    }
    let sentinel = format!("a5-secret-sentinel-{}", Uuid::new_v4());
    std::env::set_var(&source, &sentinel);
    let _secret = SecretGuard(source.clone());

    let fixture = Fixture::new();
    let mut state = fake_convene_state(&fixture.config);
    let supervisor = with_supervisor(&mut state, fixture.config.path()).await;
    let app = router(state.clone());
    // The expected value lives outside the package, so the package bytes
    // (and therefore the immutable store) never contain the sentinel.
    let expected = fixture.sources.path().join("expected-secret");
    fs::write(&expected, &sentinel).unwrap();
    let service = format!(
        "#!/bin/sh\n[ \"$A5_TOKEN\" = \"$(cat '{}')\" ] && printf exact > \"$HOME/secret-proof\"\n[ -z \"${{{source}+present}}\" ] || printf leaked > \"$HOME/source-leaked\"\nprintf '%s\\n' \"token=$A5_TOKEN\" >&2\n{}",
        expected.display(),
        gate_service().trim_start_matches("#!/bin/sh\n")
    );
    let capabilities =
        format!("[services.capabilities]\nenv = [\"A5_TOKEN\"]\nsecrets = [\"env:{source}\"]\n");
    let package = gate_package_with(&fixture, "secret", "1.0.0", &capabilities, &service);
    let mut responses = Vec::new();
    let (status, installed) = install_local(&app, 0, &package).await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    let digest = installed["mutation"]["digest"].as_str().unwrap().to_owned();
    responses.push(installed);
    let (status, preview, trusted) = trust(
        &app,
        1,
        &digest,
        json!({"env": ["A5_TOKEN"], "secrets": [format!("env:{source}")]}),
        json!([{"target_env": "A5_TOKEN", "reference": format!("env:{source}")}]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{trusted}");
    responses.extend([preview, trusted]);
    let (status, enabled) = scope(&app, ID, "enable", 2).await;
    assert_eq!(status, StatusCode::OK, "{enabled}");
    responses.push(enabled);
    let (pid, _) = healthy(&app).await;
    wait_for("secret proof", || {
        data_dir(&fixture).join("secret-proof").exists()
    })
    .await;
    assert_eq!(
        fs::read_to_string(data_dir(&fixture).join("secret-proof")).unwrap(),
        "exact"
    );
    assert!(!data_dir(&fixture).join("source-leaked").exists());
    // The child echoed the secret on stderr. Stderr counters reach status at
    // connection end, and disable prunes the status row, so the connection is
    // ended first by killing the leader (no restart policy): the row stays,
    // unhealthy, with the redaction counted and no secret text.
    ordinary_turn(
        &state,
        None,
        "ping",
        fixture.sources.path().to_str().unwrap(),
    )
    .await;
    for uri in [
        "/v1/extensions".to_owned(),
        format!("/v1/extensions/{ID}/inspect"),
        format!("/v1/extensions/{ID}/doctor"),
        format!("/v1/extensions/{ID}/status"),
    ] {
        responses.push(get_json(&app, &uri).await.1);
    }
    let argv = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&argv.stdout).contains(&sentinel));
    // SAFETY: signals only the observed leader of this test's service.
    assert_eq!(unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) }, 0);
    let ended = wait_for_status(&app, ID, |body| body["services"][0]["state"] == "unhealthy").await;
    assert!(
        ended["services"][0]["stderr_redactions"].as_u64() >= Some(1),
        "the echoed secret was not counted as a redaction: {ended}"
    );
    responses.push(ended);
    let (status, disabled) = scope(&app, ID, "disable", 3).await;
    assert_eq!(status, StatusCode::OK);
    responses.push(disabled);

    for response in &responses {
        assert!(!response.to_string().contains(&sentinel), "{response}");
    }
    let supervisor_debug = format!("{:?}", supervisor.status_cache().snapshot());
    assert!(!supervisor_debug.contains(&sentinel));
    for event in state.extension_lifecycle.attach().retained {
        assert!(!serde_json::to_string(&event).unwrap().contains(&sentinel));
    }
    // Registry files, the publication marker, journals, store, and state —
    // except the child's own data/, where it wrote the equality proof.
    assert_tree_lacks(
        &fixture.root(),
        &sentinel,
        Some(&fixture.root().join("state").join(ID).join("data")),
    );
    let frames = fs::read_to_string(data_dir(&fixture).join("frames")).unwrap();
    assert!(
        !frames.contains(&sentinel),
        "an event frame carried the secret"
    );

    supervisor.shutdown().await;
    fixture.assert_no_canary_ran();
}

// ---------------------------------------------------------------------------
// §20 steps 6 and 8: same-epoch retained replay after a process failure, then
// a crash loop with numeric backoff, circuit-open, group cleanup, and the
// explicit disable → enable retry.
// ---------------------------------------------------------------------------

/// The gate service, except that it exits 17 right after spawning its
/// grandchild while `$HOME/crash` exists, and records (but neither ACKs nor
/// advances its cursor for) frames while `$HOME/freeze` exists.
fn crashing_gate_service() -> String {
    gate_service()
        .replacen(
            "IFS= read -r hello || exit 10\n",
            "if [ -e \"$HOME/crash\" ]; then exit 17; fi\nIFS= read -r hello || exit 10\n",
            1,
        )
        .replacen(
            "    *'\"frame\":\"event\"'*)\n",
            "    *'\"frame\":\"event\"'*)\n      if [ -e \"$HOME/freeze\" ]; then continue; fi\n",
            1,
        )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stage_a_gate_crash_resume_backoff_circuit_and_explicit_retry() {
    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let fixture = Fixture::new();
    let mut state = fake_convene_state(&fixture.config);
    publish_daemon_started(&state.extension_lifecycle);
    let supervisor = with_supervisor(&mut state, fixture.config.path()).await;
    let app = router(state.clone());
    let service = crashing_gate_service();
    assert!(service.contains("$HOME/crash") && service.contains("$HOME/freeze"));
    let package = gate_package_with(
        &fixture,
        "crashy",
        "1.0.0",
        "restart = \"on-failure\"\n",
        &service,
    );
    let (_, installed) = install_local(&app, 0, &package).await;
    let digest = installed["mutation"]["digest"].as_str().unwrap().to_owned();
    let (status, _, _) = trust(&app, 1, &digest, json!({}), json!([])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = scope(&app, ID, "enable", 2).await;
    assert_eq!(status, StatusCode::OK);
    let cwd = fixture.sources.path().to_str().unwrap().to_owned();

    // 6. A turn is processed and its cursor persisted; the next turn arrives
    //    while the service is "frozen" (received, not processed). A process
    //    failure keeps the epoch, and the resume replays exactly the
    //    unprocessed retained facts without a reset.
    let (first, status_a) = healthy(&app).await;
    let epoch = status_a["services"][0]["activation_epoch"].clone();
    let (processed, _) = ordinary_turn(&state, None, "ping", &cwd).await;
    let processed = serde_json::to_value(processed.session_id).unwrap();
    wait_for("processed turn in the cursor", || {
        events_of(&fixture, first)
            .iter()
            .any(|e| e["kind"] == "turn_finished" && e["scope"]["session_id"] == processed)
    })
    .await;
    wait_for("cursor written", || {
        fs::read_to_string(data_dir(&fixture).join("cursor"))
            .is_ok_and(|cursor| cursor.split_whitespace().count() == 3)
    })
    .await;
    let cursor: u64 = fs::read_to_string(data_dir(&fixture).join("cursor"))
        .unwrap()
        .split_whitespace()
        .nth(2)
        .unwrap()
        .parse()
        .unwrap();
    fs::write(data_dir(&fixture).join("freeze"), "").unwrap();
    let (missed, _) = ordinary_turn(&state, None, "ping", &cwd).await;
    let missed = serde_json::to_value(missed.session_id).unwrap();
    wait_for("frozen turn received", || {
        frames_of(&fixture, first)
            .iter()
            .any(|e| e["kind"] == "turn_finished" && e["scope"]["session_id"] == missed)
    })
    .await;
    fs::remove_file(data_dir(&fixture).join("freeze")).unwrap();
    let first_child = grandchild(&fixture, first);
    // SAFETY: signals only the service leader this test spawned and observed.
    assert_eq!(
        unsafe { libc::kill(first as libc::pid_t, libc::SIGKILL) },
        0
    );
    let second = {
        let body = wait_for_status(&app, ID, |body| {
            body["services"][0]["state"] == "healthy"
                && body["services"][0]["pid"]
                    .as_i64()
                    .is_some_and(|pid| pid != first)
        })
        .await;
        assert_eq!(
            body["services"][0]["activation_epoch"], epoch,
            "a process failure kept the epoch"
        );
        assert_eq!(body["services"][0]["restart_count"], 1);
        body["services"][0]["pid"].as_i64().unwrap()
    };
    assert!(
        !process_alive(first_child),
        "the failed leader's grandchild survived"
    );
    wait_for("replayed facts", || {
        events_of(&fixture, second)
            .iter()
            .any(|e| e["kind"] == "turn_finished" && e["scope"]["session_id"] == missed)
    })
    .await;
    let resumed = frames_of(&fixture, second);
    assert!(
        !resumed.iter().any(|f| f["frame"] == "reset"),
        "a valid same-epoch cursor was reset: {resumed:?}"
    );
    let replayed = events_of(&fixture, second);
    assert!(
        replayed.iter().all(|e| sequence(e) > cursor),
        "replay is strictly after the cursor"
    );
    assert!(
        !replayed
            .iter()
            .any(|e| e["scope"]["session_id"] == processed),
        "an already-processed fact was replayed"
    );

    // 8. Crash repeatedly. Failure 1 was the kill above; kill once more, then
    //    every start exits 17. Starts are timestamped by polling so the
    //    backoff between consecutive crashes is measured, not assumed.
    fs::write(data_dir(&fixture).join("crash"), "").unwrap();
    let second_child = grandchild(&fixture, second);
    let before = starts(&fixture);
    // SAFETY: as above, the observed leader of this test's service.
    assert_eq!(
        unsafe { libc::kill(second as libc::pid_t, libc::SIGKILL) },
        0
    );
    let mut stamps = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (_, body) = get_json(&app, &format!("/v1/extensions/{ID}/status")).await;
        let seen = starts(&fixture);
        while stamps.len() < seen - before {
            stamps.push(tokio::time::Instant::now());
        }
        if body["services"][0]["state"] == "circuit_open" {
            assert_eq!(body["services"][0]["restart_count"], 4, "{body}");
            // A child that exits before its hello fails the handshake; the
            // fixed reason is the transport's, never a shutdown.
            assert!(
                body["services"][0]["reason"]
                    .as_str()
                    .is_some_and(|reason| reason != "shutdown"),
                "{body}"
            );
            assert_eq!(body["services"][0]["pid"], Value::Null);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "circuit never opened: {body}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Failures 3 and 4 were crash starts; failure 5 opened the circuit. The
    // gaps between the three crash starts are the 1 s and 2 s delays.
    assert_eq!(
        stamps.len(),
        3,
        "three crash starts before the circuit opened"
    );
    for (gap, delay) in stamps.windows(2).zip([1000u64, 2000]) {
        let gap = gap[1].duration_since(gap[0]).as_millis() as u64;
        assert!(
            gap + 60 >= delay && gap < delay + 500,
            "backoff gap {gap} ms is not the ratified {delay} ms"
        );
    }
    assert!(!process_alive(second_child));
    let pids: Vec<i64> = fs::read_to_string(data_dir(&fixture).join("starts"))
        .unwrap()
        .lines()
        .map(|pid| pid.parse().unwrap())
        .collect();
    for pid in &pids {
        assert!(!process_alive(*pid), "service {pid} survived the circuit");
        assert!(
            !process_alive(grandchild(&fixture, *pid)),
            "a grandchild of {pid} survived"
        );
    }
    // The open circuit never closes on a timer: wait past the next (4 s)
    // backoff step that an un-opened circuit would have taken.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(starts(&fixture), pids.len());

    // Explicit retry: disable → enable mints a new epoch with fresh history.
    fs::remove_file(data_dir(&fixture).join("crash")).unwrap();
    let revision = fixture.revision();
    let (status, disabled) = scope(&app, ID, "disable", revision).await;
    assert_eq!(status, StatusCode::OK, "{disabled}");
    let (status, _) = scope(&app, ID, "enable", revision + 1).await;
    assert_eq!(status, StatusCode::OK);
    let (retried, body) = healthy(&app).await;
    assert!(!pids.contains(&retried));
    assert_eq!(body["services"][0]["restart_count"], 0, "{body}");
    assert_ne!(body["services"][0]["activation_epoch"], epoch);

    supervisor.shutdown().await;
    assert!(!process_alive(retried));
    fixture.assert_no_canary_ran();
}

// ---------------------------------------------------------------------------
// §20 step 13 over the pinned Git acquirer.
// ---------------------------------------------------------------------------

/// The same no-op tree committed to a repository installs through the pinned
/// acquirer with the local digest, is untrusted until trusted, runs only after
/// enable, and every adjacent non-exact source is refused before acquisition.
/// The acquirer fetches through a test-only `file://` remote reached only after
/// the full one-resolution public-address check (see the A4 note); the
/// socket-level pin itself is proven by the A4 loopback conformance tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stage_a_gate_pinned_git_noop_installs_untrusted_and_activates_only_after_trust() {
    use super::super::transaction::git::{test_support, GitAcquirer};

    let _guard = AUTO_CONVENE_ENV_LOCK.lock().await;
    let _restore = TestEnvRestore::capture(&["OCEAN_CONFIG_DIR", "OCEAN_MODEL", "OCEAN_YOLO"]);
    let fixture = Fixture::new();
    let mut state = fake_convene_state(&fixture.config);
    let supervisor = with_supervisor(&mut state, fixture.config.path()).await;
    let app = router(state.clone());
    let url = "https://git.ocean-fixture.com/ocean/noop.git";

    // The local digest of the tree, from a separate registry.
    let local = Fixture::new();
    let local_app = router(fake_convene_state(&local.config));
    let tree = gate_package(&fixture, "noop", "1.0.0", "");
    let (_, local_install) = install_local(&local_app, 0, &tree).await;
    let local_digest = local_install["mutation"]["digest"]
        .as_str()
        .unwrap()
        .to_owned();

    let repo = fixture.sources.path().join("remote.git");
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .arg(format!("--git-dir={}", repo.display()))
            .arg(format!("--work-tree={tree}"))
            .args([
                "-c",
                "user.name=Ocean Test",
                "-c",
                "user.email=t@ocean.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(output.status.success(), "{args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    assert!(std::process::Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(&repo)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap()
        .success());
    git(&["add", "-A"]);
    git(&["commit", "--quiet", "-m", "noop"]);
    let commit = git(&["rev-parse", "HEAD"]);
    let _registration = test_support::register(
        fixture.config.path(),
        GitAcquirer::system()
            .with_resolver(Arc::new(test_support::Fixed(vec!["93.184.216.34"
                .parse()
                .unwrap()])))
            .with_file_remote(repo.clone()),
    );

    // Adjacent sources are refused before any acquisition or registry root.
    let short = &commit[..12];
    let upper = commit.to_uppercase();
    for (source, code) in [
        (
            json!({"kind": "git", "url": url, "revision": "main"}),
            "invalid_git_source",
        ),
        (
            json!({"kind": "git", "url": url, "revision": "v1.0.0"}),
            "invalid_git_source",
        ),
        (
            json!({"kind": "git", "url": url, "revision": "HEAD"}),
            "invalid_git_source",
        ),
        (
            json!({"kind": "git", "url": url, "revision": short}),
            "invalid_git_source",
        ),
        (
            json!({"kind": "git", "url": url, "revision": upper}),
            "invalid_git_source",
        ),
        (
            json!({"kind": "git", "url": "https://user:pw@git.ocean-fixture.com/ocean/noop.git", "revision": commit}),
            "invalid_git_source",
        ),
        (
            json!({"kind": "git", "url": "http://git.ocean-fixture.com/ocean/noop.git", "revision": commit}),
            "invalid_git_source",
        ),
        (
            json!({"kind": "git", "url": "ssh://git@git.ocean-fixture.com/ocean/noop.git", "revision": commit}),
            "invalid_git_source",
        ),
        (
            json!({"kind": "git", "url": url, "revision": commit, "subdir": "services"}),
            "invalid_request",
        ),
    ] {
        let (status, response) = post_op(
            &app,
            "/v1/extensions/install",
            json!({"expected_state_revision": 0, "source": source}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{source}: {response}");
        assert_precommit(&response, code, 0);
        assert!(!fixture.root().exists(), "{source} began an acquisition");
    }

    // The exact pinned commit installs with the local tree's digest.
    let (status, installed) = post_op(
        &app,
        "/v1/extensions/install",
        json!({"expected_state_revision": 0, "source": {"kind": "git", "url": url, "revision": commit}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{installed}");
    assert_committed(&installed, 1);
    assert_eq!(installed["mutation"]["digest"], local_digest.as_str());
    assert_eq!(installed["mutation"]["effective"], false);
    let (_, inspected) = get_json(&app, &format!("/v1/extensions/{ID}/inspect")).await;
    assert_eq!(inspected["extension"]["trusted"], false);
    assert_eq!(inspected["extension"]["source"]["kind"], "git");
    assert_eq!(
        inspected["extension"]["source"]["revision"],
        commit.as_str()
    );
    let (status, refused) = scope(&app, ID, "enable", 1).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_precommit(&refused, "trust_required", 1);
    assert_eq!(starts(&fixture), 0);

    let (status, _, _) = trust(&app, 1, &local_digest, json!({}), json!([])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(starts(&fixture), 0, "trust started the service");
    let (status, _) = scope(&app, ID, "enable", 2).await;
    assert_eq!(status, StatusCode::OK);
    let (pid, _) = healthy(&app).await;
    let child = grandchild(&fixture, pid);
    let (status, disabled) = scope(&app, ID, "disable", 3).await;
    assert_eq!(status, StatusCode::OK, "{disabled}");
    assert!(!process_alive(pid) && !process_alive(child));

    // A pinned Git update to an exact second commit installs a new, untrusted
    // digest, keeps the Git provenance, and starts nothing.
    fs::write(FsPath::new(&tree).join("CHANGELOG"), "two\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "--quiet", "-m", "two"]);
    let second = git(&["rev-parse", "HEAD"]);
    let starts_before = starts(&fixture);
    let (status, updated) = post_op(
        &app,
        &format!("/v1/extensions/{ID}/update"),
        json!({"expected_state_revision": 4, "source": {"kind": "git", "url": url, "revision": second}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_committed(&updated, 5);
    assert_ne!(updated["mutation"]["digest"], local_digest.as_str());
    let (_, inspected) = get_json(&app, &format!("/v1/extensions/{ID}/inspect")).await;
    assert_eq!(inspected["extension"]["trusted"], false, "{inspected}");
    assert_eq!(
        inspected["extension"]["source"]["revision"],
        second.as_str()
    );
    let (status, refused) = scope(&app, ID, "enable", 5).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_precommit(&refused, "trust_required", 5);
    assert_eq!(starts(&fixture), starts_before);

    supervisor.shutdown().await;
    fixture.assert_no_canary_ran();
    local.assert_no_canary_ran();
}

// ---------------------------------------------------------------------------
// §19.1/§19.2 structural boundaries of the composed daemon.
// ---------------------------------------------------------------------------

/// `daemon_started` is published once, on a fresh boot id, before the
/// extension host starts; `daemon_stopping` has exactly one producer, reached
/// only after the served router returns (graceful shutdown) and before the
/// supervisor drains. A crash never reaches that line, so no restarted boot
/// can carry a false `daemon_stopping`; the dispatcher itself never
/// synthesizes one.
#[test]
fn stage_a_gate_boot_and_stop_facts_have_one_ordered_producer_each() {
    let source = include_str!("../../main.rs");
    let body = &source[source.find("#[tokio::main]").expect("daemon main")..];
    // `main` ends at its first column-zero closing brace.
    let body = &body[..body.find("\n}\n").expect("end of main")];
    let find_once = |needle: &str| {
        let hits: Vec<usize> = body.match_indices(needle).map(|(at, _)| at).collect();
        assert_eq!(hits.len(), 1, "{needle}: {hits:?}");
        hits[0]
    };
    let dispatcher = find_once("LifecycleDispatcher::new(Uuid::new_v4()");
    let started = find_once("LifecycleSource::DaemonStarted");
    let host = find_once("= start_extension_host(");
    let stopping = find_once(".stop_publication()");
    let drain = find_once("extension_supervisor.shutdown().await");
    let served = body.rfind("axum::serve(").expect("router is served");
    assert!(
        dispatcher < started && started < host,
        "boot fact precedes the host"
    );
    assert!(
        served < stopping && stopping < drain,
        "stop fact is graceful-only"
    );
    // No other daemon source calls the stop producer.
    for (name, other) in [
        (
            "extension_service.rs",
            include_str!("../../extension_service.rs"),
        ),
        (
            "extension_registry.rs",
            include_str!("../../extension_registry.rs"),
        ),
        ("mutation.rs", include_str!("../mutation.rs")),
        ("transaction.rs", include_str!("../transaction.rs")),
    ] {
        assert!(
            !other.contains(".stop_publication()"),
            "{name} stops publication"
        );
    }

    // Behaviorally: without the graceful call no `daemon_stopping` exists.
    let lifecycle = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
    publish_daemon_started(&lifecycle);
    let kinds = |lifecycle: &LifecycleDispatcher| -> Vec<Value> {
        lifecycle
            .attach()
            .retained
            .iter()
            .map(|event| serde_json::to_value(event).unwrap()["kind"].clone())
            .collect()
    };
    assert_eq!(kinds(&lifecycle), [json!("daemon_started")]);
    drop(lifecycle);
    let next = LifecycleDispatcher::new(Uuid::new_v4(), HashSet::new());
    publish_daemon_started(&next);
    next.stop_publication();
    assert_eq!(
        kinds(&next),
        [json!("daemon_started"), json!("daemon_stopping")]
    );
}

/// The lifecycle adapter and supervisor never reach Observatory storage,
/// cursors, tokens, or routes, nor the client event buses; and the
/// Observatory side never reaches them. Delivery is a separate adapter.
#[test]
fn stage_a_gate_lifecycle_and_observatory_share_no_authority() {
    let lifecycle_side = [
        (
            "extension_lifecycle.rs",
            include_str!("../../extension_lifecycle.rs"),
        ),
        (
            "extension_service.rs",
            include_str!("../../extension_service.rs"),
        ),
    ];
    for (name, source) in lifecycle_side {
        for forbidden in [
            "observatory",
            "Observatory",
            "AgentEventBus",
            "agent_events",
            "EventBus",
            "ocean_observatory",
        ] {
            assert!(!source.contains(forbidden), "{name} references {forbidden}");
        }
    }
    let observatory_side = [
        ("observatory.rs", include_str!("../../observatory.rs")),
        (
            "observatory_adapter.rs",
            include_str!("../../observatory_adapter.rs"),
        ),
        (
            "observatory_auth.rs",
            include_str!("../../observatory_auth.rs"),
        ),
    ];
    for (name, source) in observatory_side {
        for forbidden in [
            "extension_lifecycle",
            "extension_service",
            "LifecycleDispatcher",
        ] {
            assert!(!source.contains(forbidden), "{name} references {forbidden}");
        }
    }
}
