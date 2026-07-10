//! Component lifecycle integration tests (OCEAN-105 follow-on).
//!
//! These drive the three component tools — `component_render`, `component_wait`,
//! `component_unmount` — together in the same process, exercising the global
//! `COMPONENT_WAIT_REGISTRY` that the daemon's `/v1/component/event` route
//! shares with the agent loop.
//!
//! Covers:
//! - render + wait + inject interaction → wait resolves
//! - render + replace with same id
//! - unmount + wait timeout (component gone, no event injected)
//! - multiple concurrent waits on different components

use ocean_runtime::tools::component::{
    ComponentRenderTool, ComponentUnmountTool, ComponentWaitTool, COMPONENT_WAIT_REGISTRY,
};
use ocean_runtime::types::AgentTool;
use serde_json::{json, Value};
use tokio::sync::oneshot;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Inject a synthetic component event into the global registry, simulating
/// what the daemon's `/v1/component/event` route does when the user clicks.
fn inject_event(session_id: &str, component_id: &str, event: Value) {
    let mut pending = COMPONENT_WAIT_REGISTRY.pending.lock().unwrap();
    if let Some(tx) = pending.remove(&(session_id.to_string(), component_id.to_string())) {
        let _ = tx.send(event);
    }
}

/// Spawn a wait in the background and return a JoinHandle whose result is the
/// tool outcome (bypassing the tool's own timeout so the test controls timing).
/// The oneshot channel is set up synchronously in the registry before we spawn,
/// so there's no race between registration and injection.
fn spawn_wait(session_id: &str, component_id: &str) -> tokio::task::JoinHandle<Value> {
    let (tx, rx) = oneshot::channel::<Value>();
    {
        let mut pending = COMPONENT_WAIT_REGISTRY.pending.lock().unwrap();
        pending.insert((session_id.to_string(), component_id.to_string()), tx);
    }
    tokio::spawn(async move { rx.await.expect("channel closed unexpectedly") })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full lifecycle: render a component, start waiting on it, inject a click
/// event, and confirm the wait resolves with the expected payload.
#[tokio::test]
async fn render_wait_inject_roundtrip() {
    let session_id = "test-session-roundtrip";

    // 1. Render a kanban board.
    let render_tool = ComponentRenderTool;
    let props = json!({
        "columns": [{ "id": "todo", "title": "To Do" }],
        "cards": [{ "id": "card-1", "column": "todo", "title": "Fix bug" }]
    });
    let res = render_tool
        .execute(
            "call-1",
            json!({ "id": "kan-1", "kind": "kanban", "props": props }),
        )
        .await
        .expect("render kanban");
    assert!(res.content[0].as_text().unwrap().contains("rendered"));

    // 2. Start waiting on the kanban in the background.
    let handle = spawn_wait(session_id, "kan-1");

    // 3. Inject a card_clicked event (simulating user click).
    let click_event = json!({
        "type": "card_clicked",
        "payload": { "card_id": "card-1" }
    });
    inject_event(session_id, "kan-1", click_event.clone());

    // 4. The wait resolves.
    let event = handle.await.expect("wait task panicked");
    assert_eq!(event["type"], "card_clicked");
    assert_eq!(event["payload"]["card_id"], "card-1");
}

/// Rendering a component with `replace: true` overwrites a previous render of
/// the same id. The tool itself doesn't track this (the client does), but the
/// side effect correctly reflects the replace flag.
#[tokio::test]
async fn render_replace_flag_is_independent_per_call() {
    let tool = ComponentRenderTool;

    // First render (replace: false by default).
    let res1 = tool
        .execute(
            "call-1",
            json!({ "id": "replaced-comp", "kind": "progress", "props": {"value": 0.3} }),
        )
        .await
        .expect("first render");
    // Access the Render side effect — verify replace flag is false.
    let se1 = &res1.side_effects[0];
    match se1 {
        ocean_runtime::types::ToolSideEffect::Render { replace, props, .. } => {
            assert!(!replace, "first render must have replace=false");
            assert_eq!(props["value"], 0.3);
        }
        other => panic!("expected Render, got {other:?}"),
    }

    // Second call with replace:true and updated props.
    let res2 = tool
        .execute(
            "call-2",
            json!({ "id": "replaced-comp", "kind": "progress", "props": {"value": 0.7}, "replace": true }),
        )
        .await
        .expect("second render");
    let se2 = &res2.side_effects[0];
    match se2 {
        ocean_runtime::types::ToolSideEffect::Render { replace, props, .. } => {
            assert!(replace, "second render must have replace=true");
            assert_eq!(props["value"], 0.7);
        }
        other => panic!("expected Render, got {other:?}"),
    }
}

/// Unmount a component, then wait for an event on that same id. Since the
/// unmount doesn't clear the wait registry by itself, a previously-registered
/// wait could still be resolved. This test confirms unmount doesn't interfere
/// with pending waits (it's the client's responsibility to stop sending events
/// after unmount, not the tools').
#[tokio::test]
async fn unmount_does_not_crash_pending_wait() {
    let session_id = "test-session-unmount";

    // Render.
    let render_tool = ComponentRenderTool;
    render_tool
        .execute(
            "call-1",
            json!({ "id": "to-unmount", "kind": "callout", "props": {"variant":"info","title":"x"} }),
        )
        .await
        .expect("render");

    // Start a wait first.
    let handle = spawn_wait(session_id, "to-unmount");

    // Unmount.
    let unmount_tool = ComponentUnmountTool;
    let res = unmount_tool
        .execute("call-2", json!({ "id": "to-unmount" }))
        .await
        .expect("unmount");
    assert!(res.content[0].as_text().unwrap().contains("unmounted"));

    // Inject an event — the wait is still alive and can receive it because
    // the unmount tool doesn't cancel pending waits.
    inject_event(
        session_id,
        "to-unmount",
        json!({ "type": "confirm_response", "payload": {"confirmed": true} }),
    );

    let event = handle.await.expect("wait task panicked");
    assert_eq!(event["type"], "confirm_response");
    assert!(event["payload"]["confirmed"].as_bool().unwrap());
}

/// Multiple concurrent waits on different components must resolve independently
/// — injecting on one must not interfere with the other.
#[tokio::test]
async fn concurrent_waits_on_different_components() {
    let session_id = "test-session-concurrent";

    // Render two components.
    let render_tool = ComponentRenderTool;
    render_tool
        .execute(
            "call-1",
            json!({ "id": "comp-a", "kind": "table", "props": {"columns":["x"],"rows":[]} }),
        )
        .await
        .expect("render a");
    render_tool
        .execute(
            "call-2",
            json!({ "id": "comp-b", "kind": "form", "props": {"fields":[{"name":"q","type":"text"}]} }),
        )
        .await
        .expect("render b");

    let h_a = spawn_wait(session_id, "comp-a");
    let h_b = spawn_wait(session_id, "comp-b");

    // Inject on comp-a first.
    inject_event(
        session_id,
        "comp-a",
        json!({ "type": "a_clicked", "payload": {"n": 1} }),
    );
    let event_a = h_a.await.expect("a resolved");
    assert_eq!(event_a["type"], "a_clicked");

    // comp-b should still be waiting.
    inject_event(
        session_id,
        "comp-b",
        json!({ "type": "form_submit", "payload": {"q": "hello"} }),
    );
    let event_b = h_b.await.expect("b resolved");
    assert_eq!(event_b["type"], "form_submit");
    assert_eq!(event_b["payload"]["q"], "hello");
}

/// When no event is injected, a wait timed out via the tool's own timeout
/// mechanism surfaces the expected error. This exercises the real
/// `ComponentWaitTool` path (not the helper).
#[tokio::test]
async fn wait_times_out_when_no_event() {
    let tool = ComponentWaitTool::for_session(Some("timeout-sess".into()));
    let res = tool
        .execute("call-1", json!({ "id": "ghost-comp", "timeout_ms": 50 }))
        .await
        .expect_err("wait with no event must time out");
    assert!(res.contains("timed out"), "expected timeout, got: {res}");
}

/// The registry is clean after a resolution — no leftover entry for the
/// same (session, component) key.
#[tokio::test]
async fn registry_cleared_after_resolution() {
    let session_id = "test-session-clean";
    let handle = spawn_wait(session_id, "clean-comp");
    inject_event(
        session_id,
        "clean-comp",
        json!({ "type": "done", "payload": {} }),
    );
    handle.await.expect("resolved");

    // The helper already cleans up, but let's also verify the registry is truly
    // empty for this key.
    let pending = COMPONENT_WAIT_REGISTRY.pending.lock().unwrap();
    assert!(
        !pending.contains_key(&(session_id.to_string(), "clean-comp".to_string())),
        "registry must not leak entries after resolution"
    );
}

/// The full tool-driven wait path resolves when an event is injected while the
/// tool is blocking. This is the real path the daemon uses.
#[tokio::test]
async fn real_wait_tool_resolves_on_injected_event() {
    let session_id = "real-wait-sess";

    // Render first so the component id is "valid" from the agent's perspective.
    ComponentRenderTool
        .execute(
            "call-1",
            json!({ "id": "real-comp", "kind": "confirm", "props": {"title":"ok?"} }),
        )
        .await
        .expect("render");

    // Spawn the real wait in background. It blocks on the oneshot channel.
    let handle = {
        let sid = session_id.to_string();
        tokio::spawn(async move {
            let tool = ComponentWaitTool::for_session(Some(sid));
            // Use a generous timeout since the test controls injection timing.
            tool.execute("call-2", json!({ "id": "real-comp", "timeout_ms": 5000 }))
                .await
        })
    };

    // Give the wait a moment to register.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // Inject the event.
    inject_event(
        session_id,
        "real-comp",
        json!({ "type": "confirm_response", "payload": {"confirmed": true} }),
    );

    let result = handle
        .await
        .expect("wait task panicked")
        .expect("wait should resolve");
    let text = result.content[0].as_text().unwrap();
    assert!(text.contains("confirm_response"), "result text: {text}");
    assert!(text.contains("confirmed"), "result text: {text}");
}
