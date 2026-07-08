//! End-to-end: the real `LspClient` against the in-repo `fake_lsp` server
//! binary (spawned via `CARGO_BIN_EXE_fake_lsp` — no rust-analyzer needed).

use std::path::PathBuf;
use std::time::Duration;

use ocean_lsp::client::LspClient;
use serde_json::json;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ocean-lsp-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start_client(root: &std::path::Path) -> LspClient {
    let bin = env!("CARGO_BIN_EXE_fake_lsp");
    LspClient::start("fake-lsp", bin, &[], root)
        .await
        .expect("handshake with fake server")
}

#[tokio::test]
async fn handshake_open_and_diagnostics_flow() {
    let root = scratch("diag");
    let file = root.join("main.rs");
    std::fs::write(&file, "BUG here\nok\n").unwrap();

    let client = start_client(&root).await;
    client.ensure_open(&file).await.unwrap();
    let diags = client
        .wait_for_diagnostics(&file, Duration::from_secs(5))
        .await;
    assert_eq!(diags.len(), 1, "one diagnostic for the BUG marker");
    assert_eq!(diags[0].severity, "error");
    assert!(diags[0].message.contains("BUG"));

    // Fix the file, re-sync (didChange), diagnostics clear.
    std::fs::write(&file, "clean now\n").unwrap();
    client.ensure_open(&file).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let diags = client.diagnostics_for(&file);
    assert!(diags.is_empty(), "fixed file publishes empty diagnostics");
}

#[tokio::test]
async fn requests_multiplex_and_answer() {
    let root = scratch("req");
    let file = root.join("lib.rs");
    std::fs::write(&file, "fn foo() {}\n").unwrap();

    let client = start_client(&root).await;
    client.ensure_open(&file).await.unwrap();
    let uri = ocean_lsp::client::uri_for(&file);

    // Two concurrent requests — the multiplexed IO task answers both.
    let hover = client.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 0, "character": 3 } }),
    );
    let defn = client.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": { "line": 0, "character": 3 } }),
    );
    let (hover, defn) = tokio::join!(hover, defn);
    let hover = hover.expect("hover answers");
    assert_eq!(hover["contents"]["value"], "hover at 0:3");
    let defn = defn.expect("definition answers");
    assert_eq!(defn["range"]["start"]["line"], 0);
}

/// Real-world smoke against rust-analyzer (present on dev boxes; `#[ignore]`d
/// so CI without RA stays fast — run with `cargo test -p ocean-lsp -- --ignored`).
/// Drives the full LspTool surface the way a model would: hover a symbol,
/// find its definition, then read diagnostics for a file with a real error.
#[tokio::test]
#[ignore]
async fn real_rust_analyzer_smoke() {
    use ocean_lsp::servers::binary_on_path;
    use ocean_lsp::LspTool;
    use ocean_runtime::types::AgentTool;

    if !binary_on_path("rust-analyzer") {
        eprintln!("rust-analyzer not on PATH; skipping");
        return;
    }
    let root = scratch("ra");
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn add(a: u64, b: u64) -> u64 { a + b }\npub fn broken() -> u64 { missing_fn() }\n",
    )
    .unwrap();

    let tool = LspTool::new(root.clone(), Some("smoke".into()));

    // status: rust-analyzer detected for this root.
    let status = tool
        .execute("1", json!({ "action": "status" }))
        .await
        .unwrap();
    let status_text = status.content[0].as_text().unwrap();
    assert!(status_text.contains("rust-analyzer"), "{status_text}");

    // hover the `add` symbol (RA needs a beat to index even a tiny crate; the
    // client's 30s request deadline covers it).
    let hover = tool
        .execute(
            "2",
            json!({ "action": "hover", "file": "src/lib.rs", "line": 1, "symbol": "add" }),
        )
        .await
        .unwrap();
    let hover_text = hover.content[0].as_text().unwrap();
    assert!(
        hover_text.contains("fn add"),
        "hover should show the signature, got: {hover_text}"
    );

    // definition of the `missing_fn` callsite's neighbour `broken` resolves.
    let defn = tool
        .execute(
            "3",
            json!({ "action": "definition", "file": "src/lib.rs", "line": 2, "symbol": "broken" }),
        )
        .await
        .unwrap();
    let defn_text = defn.content[0].as_text().unwrap();
    assert!(defn_text.contains("lib.rs:2"), "definition: {defn_text}");

    // diagnostics: the missing_fn error surfaces.
    let mut found = String::new();
    for _ in 0..10 {
        let diags = tool
            .execute(
                "4",
                json!({ "action": "diagnostics", "file": "src/lib.rs", "all": true }),
            )
            .await
            .unwrap();
        found = diags.content[0].as_text().unwrap().to_string();
        if found.contains("missing_fn") || found.contains("cannot find") {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    assert!(
        found.contains("missing_fn") || found.contains("cannot find"),
        "diagnostics should flag the unresolved call, got: {found}"
    );
}
