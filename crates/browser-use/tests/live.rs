//! End-to-end smoke test against a real engine.
//!
//! Ignored by default because it launches a browser and hits the local file
//! system; run with:
//!
//! ```bash
//! EFFIBUDDY_BROWSER_ALLOW_FILE=1 cargo test -p effibuddy-browser --test live -- --ignored --nocapture
//! ```

use effibuddy_browser::cdp::CdpBackend;
use effibuddy_browser::discover::default_engine_preference;
use effibuddy_browser::{BrowserBackend, SnapshotMode, WaitPolicy};

const PAGE: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>EffiBuddy Fixture</title>
</head>
<body>
  <h1>Fixture page</h1>
  <p id="text">Read me.</p>
  <label for="name">Your name</label>
  <input id="name" type="text" placeholder="type here">
  <label><input id="agree" type="checkbox"> I agree</label>
  <button id="go" onclick="document.getElementById('out').textContent = 'clicked ' + (document.getElementById('name').value || 'nobody');">Submit</button>
  <div id="out">not clicked</div>
  <script>
    console.log('fixture loaded');
    console.warn('this is a warning');
  </script>
</body>
</html>"#;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real browser"]
async fn snapshot_click_and_journal_work_against_a_real_engine() {
    let dir = tempfile::tempdir().unwrap();
    let page = dir.path().join("fixture.html");
    std::fs::write(&page, PAGE).unwrap();
    let url = format!("file://{}", page.display());

    let backend = CdpBackend::new(default_engine_preference());
    backend.set_headless(true);

    let info = backend.ensure_ready().await.expect("browser should start");
    println!("ready: {info:?}");

    let page_info = backend
        .navigate(&url, WaitPolicy::Load)
        .await
        .expect("navigate");
    println!("navigated: {page_info:?}");
    assert!(page_info.title.contains("Fixture"), "{page_info:?}");

    let snapshot = backend
        .snapshot(SnapshotMode::Interesting, 400)
        .await
        .expect("snapshot");
    let rendered = effibuddy_browser::render_snapshot(&snapshot);
    println!("--- snapshot ---\n{rendered}\n---");
    assert!(!snapshot.nodes.is_empty(), "snapshot had no nodes");

    let button = snapshot
        .nodes
        .iter()
        .find(|node| node.role == "button" && node.name.contains("Submit"))
        .expect("submit button in snapshot");
    println!("button ref = {}", button.id);

    let input = snapshot
        .nodes
        .iter()
        .find(|node| node.role == "textbox")
        .expect("text input in snapshot");

    backend
        .type_ref(&input.id, "Effi", false, WaitPolicy::None)
        .await
        .expect("type into the input");

    backend
        .click_ref(
            &button.id,
            effibuddy_browser::MouseButton::Left,
            1,
            &[],
            WaitPolicy::None,
        )
        .await
        .expect("click the button");

    let result = backend
        .evaluate("document.getElementById('out').textContent")
        .await
        .expect("read the result");
    println!("result text = {result}");
    assert!(
        result.as_str().unwrap_or_default().contains("Effi"),
        "expected typed text in the result, got {result}"
    );

    let console = backend
        .console(&Default::default())
        .await
        .expect("console journal");
    println!("console entries: {:?}", console.entries.len());
    assert!(
        console
            .entries
            .iter()
            .any(|entry| entry.text.contains("fixture loaded")),
        "console.log did not reach the journal: {:?}",
        console.entries
    );

    let capture = backend
        .screenshot(Default::default())
        .await
        .expect("screenshot");
    println!(
        "screenshot: {} bytes, {}x{}",
        capture.png.len(),
        capture.frame.image_width,
        capture.frame.image_height
    );
    assert!(capture.png.starts_with(b"\x89PNG"));

    backend.end_session().await.expect("end session");
}
