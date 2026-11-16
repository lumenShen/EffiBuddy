//! End-to-end smoke test against a real engine.
//!
//! Ignored by default because it launches a browser and binds a loopback
//! socket; run with:
//!
//! ```bash
//! cargo test -p effibuddy-browser --test live -- --ignored --nocapture
//! ```

use effibuddy_browser::cdp::CdpBackend;
use effibuddy_browser::discover::default_engine_preference;
use effibuddy_browser::{
    BrowserBackend, EditorOp, EditorRequest, EvalRequest, RequestWait, SnapshotMode, WaitPolicy,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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
  <textarea id="code">hello editor</textarea>
  <script>
    console.log('fixture loaded');
    console.warn('this is a warning');
  </script>
</body>
</html>"#;

/// Serve one JSON payload so the network body path has something to fetch.
async fn serve_json() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let mut buffer = [0u8; 4096];
            let _ = socket.read(&mut buffer).await;
            let body = r#"{"ok":true,"stage":"game_build","value":42}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    format!("http://{address}/")
}

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
        .evaluate(&EvalRequest {
            expression: "document.getElementById('out').textContent".to_string(),
            timeout_ms: 5_000,
            await_promise: true,
        })
        .await
        .expect("read the result");
    println!("result text = {}", result.value);
    assert!(
        result.value.as_str().unwrap_or_default().contains("Effi"),
        "expected typed text in the result, got {}",
        result.value
    );
    assert!(!result.timed_out);

    // `text --selector`
    let text = backend
        .element_text("#text", false, 200)
        .await
        .expect("selector text");
    assert_eq!(text.text, "Read me.");
    assert_eq!(text.matches, 1);

    // In-page editor: the fixture's textarea is the "IDE".
    let editor = backend
        .editor(&EditorRequest {
            op: EditorOp::Read,
            target: None,
            content: None,
            max_chars: 200,
        })
        .await
        .expect("editor read");
    println!("editor = {editor}");
    assert_eq!(editor["kind"], "textarea");
    assert_eq!(editor["value"], "hello editor");

    backend
        .editor(&EditorRequest {
            op: EditorOp::Write,
            target: None,
            content: Some("written by the agent".to_string()),
            max_chars: 200,
        })
        .await
        .expect("editor write");
    let after = backend
        .editor(&EditorRequest {
            op: EditorOp::Read,
            target: None,
            content: None,
            max_chars: 200,
        })
        .await
        .expect("editor read back");
    assert_eq!(after["value"], "written by the agent");

    // Init scripts: install, reload, and the helper must still be there.
    backend
        .set_init_scripts(&["globalThis.__effi_probe = 'ready';".to_string()])
        .await
        .expect("install init script");
    backend.reload(WaitPolicy::Load).await.expect("reload");
    let probe = backend
        .evaluate(&EvalRequest {
            expression: "globalThis.__effi_probe || 'missing'".to_string(),
            timeout_ms: 5_000,
            await_promise: true,
        })
        .await
        .expect("probe init script");
    assert_eq!(probe.value, "ready", "init script did not survive reload");

    // Network bodies: fetch the loopback endpoint from the page, then wait on it.
    let origin = serve_json().await;
    backend
        .navigate(&format!("{origin}index.html"), WaitPolicy::Load)
        .await
        .expect("navigate to the fixture server");
    backend
        .evaluate(&EvalRequest {
            expression: "fetch('/api/game_build').then((r) => r.text())".to_string(),
            timeout_ms: 5_000,
            await_promise: true,
        })
        .await
        .expect("fetch the API");
    let body = backend
        .wait_for_request(&RequestWait {
            pattern: "/api/game_build".to_string(),
            method: Some("GET".to_string()),
            since: None,
            timeout_ms: 10_000,
            max_chars: 4_000,
        })
        .await
        .expect("wait for the request");
    println!("body = {}", body.body);
    assert!(body.available, "{body:?}");
    assert!(body.body.contains("game_build"), "{body:?}");

    // Drag: a sortable list is the classic use; assert the gesture lands.
    backend
        .navigate(&url, WaitPolicy::Load)
        .await
        .expect("navigate back");
    let box_go = backend
        .element_box("#go")
        .await
        .expect("box")
        .expect("button box");
    backend
        .drag(
            box_go.center(),
            (box_go.x + 10.0, box_go.y + 120.0),
            8,
            WaitPolicy::None,
        )
        .await
        .expect("drag");

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

#[tokio::test(flavor = "multi_thread")]
#[ignore = "launches a real browser"]
async fn a_timed_out_evaluation_returns_the_partial_log() {
    let backend = CdpBackend::new(default_engine_preference());
    backend.set_headless(true);
    backend.ensure_ready().await.expect("browser should start");

    let outcome = backend
        .evaluate(&EvalRequest {
            expression: "console.log('poll 1'); console.log('poll 2'); new Promise(() => {})"
                .to_string(),
            timeout_ms: 700,
            await_promise: true,
        })
        .await
        .expect("evaluate");
    assert!(outcome.timed_out, "{outcome:?}");
    assert!(
        outcome.logs.iter().any(|line| line.contains("poll 1")),
        "partial output was dropped: {outcome:?}"
    );

    backend.end_session().await.expect("end session");
}
