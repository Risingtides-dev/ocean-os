//! Real-Chrome smoke test. Ignored by default (needs a Chrome binary + display
//! or headless). Run with: `cargo test -p ocean-browser --features legacy-chromium --test smoke -- --ignored`.

#![cfg(feature = "legacy-chromium")]

use ocean_browser::{BrowserHandle, LaunchConfig};

#[tokio::test]
#[ignore]
async fn navigate_and_screenshot() {
    let cfg = LaunchConfig {
        profile_dir: std::env::temp_dir().join("ocean-test-profile"),
        profile_directory: None,
        extension_dir: None,
        chrome_executable: None,
        headless: true,
        port: 0,
    };
    let h = BrowserHandle::launch(cfg).await.expect("launch");
    let title = h.navigate("https://example.com").await.expect("nav");
    assert!(title.to_lowercase().contains("example"));
    let png_b64 = h.screenshot(false).await.expect("shot");
    assert!(png_b64.len() > 100);
}

#[tokio::test]
#[ignore]
async fn read_page_finds_link() {
    let cfg = LaunchConfig {
        profile_dir: std::env::temp_dir().join("ocean-test-profile2"),
        profile_directory: None,
        extension_dir: None,
        chrome_executable: None,
        headless: true,
        port: 0,
    };
    let h = BrowserHandle::launch(cfg).await.expect("launch");
    h.navigate("https://example.com").await.expect("nav");
    let read = h.read_page().await.expect("read");
    assert!(read.text.to_lowercase().contains("example"));
    assert!(read
        .elements
        .iter()
        .any(|e| e.role == "a" || e.role == "link"));
}
