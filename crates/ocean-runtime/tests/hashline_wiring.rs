//! End-to-end wiring test for W1 hashline edits through the real tool path:
//! `BuiltinProvider` under a hashline `SessionContext` → the `read` tool emits a
//! `[path#HASH]` tag and records a snapshot → the `hashline_edit` tool applies a
//! patch against that tag and rewrites the file. Also proves the profile gate:
//! with `hashline: false`, `read` is untagged and no `hashline_edit` is offered.

use std::sync::Arc;

use ocean_runtime::capability::{BuiltinProvider, CapabilityProvider, SessionContext};
use serde_json::json;

fn body(r: &ocean_runtime::types::AgentToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.as_text())
        .unwrap_or("")
        .to_string()
}

async fn tool_named(
    provider: &BuiltinProvider,
    ctx: &SessionContext,
    name: &str,
) -> Option<Arc<dyn ocean_runtime::types::AgentTool>> {
    provider
        .tools(ctx)
        .await
        .into_iter()
        .find(|t| t.name() == name)
}

#[tokio::test]
async fn hashline_read_then_edit_roundtrip() {
    let dir = std::env::temp_dir().join(format!("ocean-hl-wire-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("f.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
    let path = file.to_string_lossy().to_string();

    let provider = BuiltinProvider::new();
    let ctx = SessionContext {
        cwd: dir.clone(),
        session_id: Some("s-hl".into()),
        hashline: true,
        artifacts: false,
        code_intelligence: true,
    };

    // 1. read emits a [path#HASH] tag.
    let read = tool_named(&provider, &ctx, "read")
        .await
        .expect("read tool");
    let out = read
        .execute("1", json!({ "path": path }))
        .await
        .expect("read ok");
    let text = body(&out);
    let first = text.lines().next().unwrap_or("");
    assert!(
        first.starts_with(&format!("[{path}#")) && first.ends_with(']'),
        "read must emit a [path#HASH] tag, got: {first:?}"
    );
    // Extract the 4-hex hash between '#' and ']'.
    let hash = first
        .rsplit_once('#')
        .and_then(|(_, rest)| rest.strip_suffix(']'))
        .expect("hash in tag")
        .to_string();
    assert_eq!(hash.len(), 4, "file hash is 4 hex chars");

    // 2. hashline_edit applies a SWAP against that tag.
    let edit = tool_named(&provider, &ctx, "hashline_edit")
        .await
        .expect("hashline_edit tool present under hashline profile");
    let patch = format!("[{path}#{hash}]\nSWAP 2:\n+BETA\n");
    let res = edit
        .execute("2", json!({ "patch": patch }))
        .await
        .expect("hashline_edit applies against a fresh tag");
    assert!(body(&res).contains("applied"), "edit reports applied");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "alpha\nBETA\ngamma\n",
        "the file was rewritten via hashline"
    );

    // 3. a stale tag (wrong hash) is rejected, not silently misapplied.
    let stale = format!("[{path}#0000]\nSWAP 1:\n+X\n");
    let err = edit
        .execute("3", json!({ "patch": stale }))
        .await
        .expect_err("a stale tag must be rejected");
    assert!(err.contains("stale"), "stale rejection message, got: {err}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// No-op loop guard wiring: a `hashline_edit` whose patch applies cleanly but
/// changes NOTHING (the file already matches the target content) is tolerated
/// once, then the identical repeat trips the session guard to a hard error —
/// the model gets told to stop re-issuing the changeless edit. A genuinely
/// changing edit to the same path clears the counter.
#[tokio::test]
async fn repeated_noop_hashline_edit_trips_the_loop_guard() {
    let dir = std::env::temp_dir().join(format!("ocean-hl-noop-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("f.txt");
    std::fs::write(&file, "alpha\nbeta\ngamma\n").unwrap();
    let path = file.to_string_lossy().to_string();

    let provider = BuiltinProvider::new();
    let ctx = SessionContext {
        cwd: dir.clone(),
        session_id: Some("s-noop".into()),
        hashline: true,
        artifacts: false,
        code_intelligence: true,
    };

    // Read to record a snapshot + get the live tag.
    let read = tool_named(&provider, &ctx, "read")
        .await
        .expect("read tool");
    let out = read
        .execute("1", json!({ "path": path }))
        .await
        .expect("read ok");
    let first = body(&out).lines().next().unwrap_or("").to_string();
    let hash = first
        .rsplit_once('#')
        .and_then(|(_, rest)| rest.strip_suffix(']'))
        .expect("hash in tag")
        .to_string();

    let edit = tool_named(&provider, &ctx, "hashline_edit")
        .await
        .expect("hashline_edit tool");

    // A patch that swaps line 2 to its EXISTING content — applies cleanly,
    // changes nothing.
    let noop_patch = format!("[{path}#{hash}]\nSWAP 2:\n+beta\n");

    // First no-op: tolerated, reported honestly as a no-op.
    let res = edit
        .execute("2", json!({ "patch": noop_patch }))
        .await
        .expect("first no-op edit is tolerated");
    assert!(
        body(&res).contains("no-op"),
        "a changeless edit must be reported as a no-op, got: {}",
        body(&res)
    );

    // Second identical no-op: the guard trips (default tolerance is 2).
    let err = edit
        .execute("3", json!({ "patch": noop_patch }))
        .await
        .expect_err("the repeated identical no-op must trip the loop guard");
    assert!(
        err.contains("no change") && err.contains("Stop re-issuing"),
        "guard error must tell the model to stop looping, got: {err}"
    );

    // A REAL edit to the same path succeeds and clears the counter.
    let changing_patch = format!("[{path}#{hash}]\nSWAP 2:\n+BETA\n");
    let res = edit
        .execute("4", json!({ "patch": changing_patch }))
        .await
        .expect("a changing edit succeeds after the guard tripped");
    assert!(body(&res).contains("applied"));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "alpha\nBETA\ngamma\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn non_hashline_profile_is_untagged_and_has_no_edit_tool() {
    let dir = std::env::temp_dir().join(format!("ocean-hl-off-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join("f.txt");
    std::fs::write(&file, "one\ntwo\n").unwrap();
    let path = file.to_string_lossy().to_string();

    let provider = BuiltinProvider::new();
    let ctx = SessionContext {
        cwd: dir.clone(),
        session_id: Some("s-plain".into()),
        hashline: false,
        artifacts: false,
        code_intelligence: true,
    };

    let read = tool_named(&provider, &ctx, "read")
        .await
        .expect("read tool");
    let out = read
        .execute("1", json!({ "path": path }))
        .await
        .expect("read ok");
    assert!(
        !body(&out).contains(&format!("[{path}#")),
        "plain profile read must NOT emit a hash tag"
    );
    assert!(
        tool_named(&provider, &ctx, "hashline_edit").await.is_none(),
        "hashline_edit must not be offered off-profile"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
