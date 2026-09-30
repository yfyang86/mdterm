use std::time::Duration;

use mdterm_core::{CliKind, Message, Role, Session, TranscriptEvent};
use mdterm_viewer::ViewerServer;
use tokio::sync::mpsc;

fn sample_session() -> Session {
    Session {
        id: "test-session".into(),
        kind: CliKind::Claude,
        messages: vec![
            Message {
                role: Role::User,
                content: "hello **world**".into(),
                timestamp: Some("2024-01-01T00:00:00Z".into()),
                model: None,
            },
            Message {
                role: Role::Assistant,
                content: "hi there".into(),
                timestamp: None,
                model: Some("claude-test".into()),
            },
        ],
    }
}

async fn spawn_server() -> (u16, mpsc::Sender<TranscriptEvent>) {
    let (tx, rx) = mpsc::channel(16);
    let port = ViewerServer::start(0, rx).await.expect("server starts on port 0");
    assert!(port > 0, "OS-assigned port reported");
    (port, tx)
}

#[tokio::test]
async fn index_returns_html_and_api_reflects_injected_event() {
    let (port, tx) = spawn_server().await;
    let client = reqwest::Client::new();

    // GET / → 200 HTML
    let resp = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap().to_string();
    assert!(ct.contains("text/html"), "content-type was {ct}");
    let body = resp.text().await.unwrap();
    assert!(body.contains("mdterm"));
    assert!(body.contains("/events"));

    // Inject an event → /api/session reflects it.
    tx.send(TranscriptEvent::Updated(sample_session())).await.unwrap();

    let mut session: Option<Session> = None;
    for _ in 0..50 {
        let s: Session = client
            .get(format!("http://127.0.0.1:{port}/api/session"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if s.id == "test-session" {
            session = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let session = session.expect("/api/session reflects injected event");
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].role, Role::User);
    assert!(session.messages[0].content.contains("hello **world**"));
}

#[tokio::test]
async fn sse_streams_session_events() {
    let (port, tx) = spawn_server().await;
    let client = reqwest::Client::new();

    // Pre-load a session so the first SSE frame carries it.
    tx.send(TranscriptEvent::Updated(sample_session())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut resp = client
        .get(format!("http://127.0.0.1:{port}/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let ct = resp.headers().get("content-type").unwrap().to_str().unwrap().to_string();
    assert!(ct.contains("text/event-stream"), "content-type was {ct}");

    // Push one more event after subscribing and read frames until both
    // sessions have been observed on the stream.
    tx.send(TranscriptEvent::Updated(Session {
        id: "second".into(),
        kind: CliKind::Claude,
        messages: vec![],
    }))
    .await
    .unwrap();

    let mut buf = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !(buf.contains("test-session") && buf.contains("second")) {
        assert!(std::time::Instant::now() < deadline, "timed out; got: {buf}");
        let chunk = tokio::time::timeout(Duration::from_secs(2), resp.chunk())
            .await
            .expect("chunk timeout")
            .unwrap()
            .expect("stream ended");
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
}

/// F3 regression: the mermaid security level must stay "strict" — diagram
/// labels come from attacker-influenceable transcript content and "loose"
/// renders raw HTML labels (XSS on the 127.0.0.1 origin, which can reach
/// /api/session). Also guards against enabling raw HTML in markdown-it.
#[tokio::test]
async fn index_html_has_no_raw_html_sinks() {
    let (port, _tx) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        body.contains("securityLevel: \"strict\""),
        "mermaid must run with securityLevel strict"
    );
    assert!(
        !body.contains("securityLevel: \"loose\""),
        "mermaid securityLevel loose renders raw HTML labels (XSS)"
    );
    assert!(
        body.contains("html: false"),
        "markdown-it must keep raw HTML disabled"
    );
}

/// F5 regression: the viewer's protectMath must mask link destinations
/// and indented code blocks, not just code fences/spans — `$` in a URL
/// must never be treated as math (it rewrites the link). The full
/// behavior was verified against the page's regex with node; this pins
/// the masks in the served HTML.
#[tokio::test]
async fn index_html_math_protection_masks_links_and_indented_code() {
    let (port, _tx) = spawn_server().await;
    let client = reqwest::Client::new();
    let body = client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for needle in [
        "!?\\[[^\\]\\n]*\\]\\([^)\\n]*\\)", // inline link/image destinations
        "\\[[^\\]\\n]+\\]:[^\\n]*",         // link reference definitions
        "(?: {4}|\\t)[^\\n]*",              // 4-space / tab indented code
    ] {
        assert!(body.contains(needle), "protectMath mask missing: {needle}");
    }
}

/// Read SSE chunks into `buf` until `needle` appears (or deadline).
async fn read_until(resp: &mut reqwest::Response, buf: &mut String, needle: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !buf.contains(needle) {
        assert!(std::time::Instant::now() < deadline, "timed out; got: {buf}");
        let chunk = tokio::time::timeout(Duration::from_secs(2), resp.chunk())
            .await
            .expect("chunk timeout")
            .unwrap()
            .expect("stream ended");
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
}

/// F8 regression: the SSE bootstrap subscribes before snapshotting, so an
/// update racing the connection is never lost; the bootstrap snapshot
/// itself (sequence 0, before any update) must still be delivered.
#[tokio::test]
async fn sse_bootstrap_delivers_snapshot_then_update_without_loss() {
    let (port, tx) = spawn_server().await;
    let client = reqwest::Client::new();

    // Connect BEFORE any update exists: the first frame must still arrive
    // (the initial empty snapshot at sequence 0).
    let mut resp = client
        .get(format!("http://127.0.0.1:{port}/events"))
        .send()
        .await
        .unwrap();
    let mut buf = String::new();
    read_until(&mut resp, &mut buf, "data:").await;

    // Now push an update; it must be delivered on this same stream (no
    // subscribe-then-snapshot gap, no snapshot swallowing the update).
    tx.send(TranscriptEvent::Updated(Session {
        id: "raced-update".into(),
        kind: CliKind::Claude,
        messages: vec![],
    }))
    .await
    .unwrap();
    read_until(&mut resp, &mut buf, "raced-update").await;

    // Dedupe check: with the update already reflected in a NEW client's
    // snapshot, its broadcast replay must not produce a duplicate frame
    // for that same update before the next one. Connect a second client,
    // then push a final update; the second client must see the current
    // state once, then the final update.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut resp2 = client
        .get(format!("http://127.0.0.1:{port}/events"))
        .send()
        .await
        .unwrap();
    tx.send(TranscriptEvent::Updated(Session {
        id: "final-update".into(),
        kind: CliKind::Claude,
        messages: vec![],
    }))
    .await
    .unwrap();
    let mut buf2 = String::new();
    read_until(&mut resp2, &mut buf2, "final-update").await;
    assert_eq!(
        buf2.matches("raced-update").count(),
        1,
        "snapshot update delivered exactly once (deduped): {buf2:?}"
    );
    assert!(buf2.contains("final-update"));
}

#[tokio::test]
async fn vendored_assets_are_served() {
    let (port, _tx) = spawn_server().await;
    let client = reqwest::Client::new();
    for (path, needle_ct) in [
        ("/assets/markdown-it.min.js", "text/javascript"),
        ("/assets/highlight.min.js", "text/javascript"),
        ("/assets/mermaid.min.js", "text/javascript"),
        ("/assets/katex.min.css", "text/css"),
        ("/assets/fonts/KaTeX_Main-Regular.woff2", "font/woff2"),
    ] {
        let resp = client
            .get(format!("http://127.0.0.1:{port}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{path}");
        let ct = resp.headers().get("content-type").unwrap().to_str().unwrap().to_string();
        assert!(ct.starts_with(needle_ct), "{path} content-type was {ct}");
        assert!(!resp.bytes().await.unwrap().is_empty(), "{path} empty body");
    }
    let resp = client
        .get(format!("http://127.0.0.1:{port}/assets/nope.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}
