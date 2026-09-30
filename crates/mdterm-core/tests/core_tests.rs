use std::io::Write;
use std::time::Duration;

use mdterm_core::{
    discover_latest_session_under, parse_claude_jsonl, watch_transcript, CliKind, Role,
    TranscriptEvent,
};

fn fixture_jsonl() -> String {
    let lines = [
        r#"{"type":"summary","summary":"chat about rust","leafUuid":"x"}"#,
        r#"{"type":"user","message":{"role":"user","content":"How do I parse JSONL in Rust?"},"timestamp":"2024-01-01T00:00:00Z","sessionId":"sess-123"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","model":"claude-test","content":[{"type":"text","text":"Use serde_json. First line."},{"type":"text","text":"Second paragraph."}]},"timestamp":"2024-01-01T00:00:01Z","sessionId":"sess-123"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","model":"claude-test","content":[{"type":"tool_use","name":"Read","input":{"file_path":"/tmp/a.rs"}},{"type":"text","text":"Reading that file now."}]},"timestamp":"2024-01-01T00:00:02Z","sessionId":"sess-123"}"#,
        r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"file contents here"}]},"timestamp":"2024-01-01T00:00:03Z","sessionId":"sess-123"}"#,
        r#"{"type":"system","content":"Caveat: local command output"}"#,
        r#"this is not json at all"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"weird_unknown_block"}]},"timestamp":"2024-01-01T00:00:04Z"}"#,
    ];
    lines.join("\n")
}

#[test]
fn parses_fixture_jsonl() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sess-123.jsonl");
    std::fs::write(&path, fixture_jsonl()).unwrap();

    let session = parse_claude_jsonl(&path).unwrap();
    assert_eq!(session.id, "sess-123");
    assert_eq!(session.kind, CliKind::Claude);

    // user text + assistant text + assistant(tool_use+text) + tool result.
    // The unknown-block assistant line contributes no message; malformed and
    // system/summary lines are skipped.
    assert_eq!(session.messages.len(), 4);

    assert_eq!(session.messages[0].role, Role::User);
    assert!(session.messages[0].content.contains("parse JSONL"));
    assert_eq!(
        session.messages[0].timestamp.as_deref(),
        Some("2024-01-01T00:00:00Z")
    );

    // text blocks concatenate
    assert_eq!(session.messages[1].role, Role::Assistant);
    assert!(session.messages[1].content.contains("First line."));
    assert!(session.messages[1].content.contains("Second paragraph."));
    assert_eq!(session.messages[1].model.as_deref(), Some("claude-test"));

    // tool_use summarized as fenced code with name + input JSON
    let tool_msg = &session.messages[2];
    assert_eq!(tool_msg.role, Role::Assistant);
    assert!(tool_msg.content.contains("**Tool use: `Read`**"));
    assert!(tool_msg.content.contains("```json"));
    assert!(tool_msg.content.contains("\"file_path\": \"/tmp/a.rs\""));
    assert!(tool_msg.content.contains("Reading that file now."));

    // tool_result becomes a Tool-role message
    assert_eq!(session.messages[3].role, Role::Tool);
    assert!(session.messages[3].content.contains("file contents here"));
}

#[test]
fn session_id_falls_back_to_file_stem() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fallback-id.jsonl");
    std::fs::write(&path, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n").unwrap();
    let session = parse_claude_jsonl(&path).unwrap();
    assert_eq!(session.id, "fallback-id");
    assert_eq!(session.messages.len(), 1);
}

#[test]
fn missing_file_is_an_error_not_a_panic() {
    let res = parse_claude_jsonl(std::path::Path::new("/nonexistent/nope.jsonl"));
    assert!(res.is_err());
}

#[test]
fn discovery_picks_newest_claude_transcript() {
    let home = tempfile::tempdir().unwrap();
    let proj_a = home.path().join(".claude/projects/proj-a");
    let proj_b = home.path().join(".claude/projects/nested/proj-b");
    std::fs::create_dir_all(&proj_a).unwrap();
    std::fs::create_dir_all(&proj_b).unwrap();

    let older = proj_a.join("older.jsonl");
    let newest = proj_b.join("newest.jsonl");
    let not_jsonl = proj_b.join("ignore.json");
    std::fs::write(&older, "{}").unwrap();
    std::fs::write(&newest, "{}").unwrap();
    std::fs::write(&not_jsonl, "{}").unwrap();

    // Force mtimes: older strictly older than newest.
    let now = filetime_now();
    set_mtime(&older, now - 100);
    set_mtime(&newest, now);

    let found = discover_latest_session_under(CliKind::Claude, home.path()).unwrap();
    assert_eq!(found, newest);
}

#[test]
fn discovery_returns_none_when_no_transcripts() {
    let home = tempfile::tempdir().unwrap();
    assert!(discover_latest_session_under(CliKind::Claude, home.path()).is_none());
    assert!(discover_latest_session_under(CliKind::Kimi, home.path()).is_none());
}

#[test]
fn discovery_probes_kimi_layout() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".kimi/projects/default");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("k.jsonl");
    std::fs::write(&f, "{}").unwrap();
    let found = discover_latest_session_under(CliKind::Kimi, home.path()).unwrap();
    assert_eq!(found, f);
}

fn filetime_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn set_mtime(path: &std::path::Path, secs: i64) {
    // Touch with an explicit mtime using `touch -d` to avoid extra deps.
    let status = std::process::Command::new("touch")
        .arg("-d")
        .arg(format!("@{secs}"))
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// F9 regression: a burst of writes must be coalesced into few re-parses,
/// and the final emit must reflect the final file state.
#[tokio::test]
async fn watcher_debounces_write_bursts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("burst.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"m0\"},\"sessionId\":\"burst\"}\n",
    )
    .unwrap();

    let mut rx = watch_transcript(path.clone(), CliKind::Claude);

    // Consume the initial bootstrap event.
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("no initial event")
        .expect("watcher channel closed");

    // Burst: 10 appends, 20ms apart (~200ms total — inside one debounce
    // window plus tail). Without debouncing this produced up to 10+
    // re-parses and broadcasts.
    for i in 1..=10 {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            f,
            "{}",
            format!(
                "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"m{i}\"}},\"sessionId\":\"burst\"}}"
            )
        )
        .unwrap();
        f.flush().unwrap();
        drop(f);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Collect Updated events until the final state (11 messages) is seen.
    let mut updates = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("timed out waiting for the final state")
            .expect("watcher channel closed");
        if let TranscriptEvent::Updated(s) = ev {
            updates += 1;
            if s.messages.len() == 11 {
                break;
            }
        }
    }
    assert!(
        updates <= 4,
        "burst should be coalesced into a few re-parses, got {updates}"
    );
}

#[tokio::test]
async fn watcher_emits_initial_update_then_update_after_append() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("watch-me.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"initial\"},\"sessionId\":\"w1\"}\n",
    )
    .unwrap();

    let mut rx = watch_transcript(path.clone(), CliKind::Claude);

    // Initial bootstrap event.
    let first = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("timed out waiting for initial event")
        .expect("watcher channel closed");
    match first {
        TranscriptEvent::Updated(s) => {
            assert_eq!(s.messages.len(), 1);
            assert!(s.messages[0].content.contains("initial"));
        }
        other => panic!("expected initial Updated, got {other:?}"),
    }

    // Append a new line; watcher should re-parse and emit Updated.
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(
            f,
            "{}",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"appended reply\"}]},\"sessionId\":\"w1\"}"
        )
        .unwrap();
        f.flush().unwrap();
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let ev = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .expect("timed out waiting for append event")
            .expect("watcher channel closed");
        if let TranscriptEvent::Updated(s) = ev {
            if s.messages
                .iter()
                .any(|m| m.content.contains("appended reply"))
            {
                assert_eq!(s.messages.len(), 2);
                break;
            }
        }
        // tolerate duplicate/intermediate events until the appended line shows
    }
}
