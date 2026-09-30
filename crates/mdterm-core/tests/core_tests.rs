use std::io::Write;
use std::time::Duration;

use mdterm_core::{
    discover_latest_any_under, discover_latest_session_under, parse_claude_jsonl,
    parse_codex_jsonl, parse_kimi_jsonl, parse_selfdefined_jsonl, parse_transcript,
    watch_transcript, CliKind, Role, TranscriptEvent,
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
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(secs, 0)).unwrap();
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

    let mut rx = watch_transcript(path.clone(), Some(CliKind::Claude));

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

    let mut rx = watch_transcript(path.clone(), Some(CliKind::Claude));

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

// ---------------------------------------------------------------------------
// kimi
// ---------------------------------------------------------------------------

fn kimi_wire_v15_fixture() -> String {
    [
        r#"{"type":"metadata","protocol_version":"1.5","created_at":1790745890773}"#,
        r#"{"type":"runtime.set_binding","workspaceId":"wd_x","runtimeId":"local","agentId":"main","time":1790745890790}"#,
        r#"{"type":"agent.message.appended","time":1700000000000,"agentId":"main","message":{"message":{"role":"user","content":[{"type":"text","text":"hello kimi"}]},"meta":{}}}"#,
        r#"{"type":"agent.message.appended","time":1700000001000,"agentId":"main","message":{"message":{"role":"assistant","content":[{"type":"think","think":"hmm"}]},"meta":{}}}"#,
        r#"{"type":"agent.message.appended","time":1700000002000,"agentId":"main","message":{"message":{"role":"assistant","content":[{"type":"text","text":"**answer** here"}]},"meta":{}}}"#,
        r#"{"type":"agent.message.appended","time":1700000003000,"agentId":"main","message":{"message":{"role":"tool","content":[{"type":"text","text":"tool out"}]},"meta":{}}}"#,
    ]
    .join("\n")
}

#[test]
fn parses_kimi_wire_v15() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wire.jsonl");
    std::fs::write(&path, kimi_wire_v15_fixture()).unwrap();

    let session = parse_kimi_jsonl(&path).unwrap();
    assert_eq!(session.kind, CliKind::Kimi);
    // think-only assistant message contributes no message.
    assert_eq!(session.messages.len(), 3);
    assert_eq!(session.messages[0].role, Role::User);
    assert!(session.messages[0].content.contains("hello kimi"));
    assert_eq!(
        session.messages[0].timestamp.as_deref(),
        Some("2023-11-14T22:13:20.000Z")
    );
    assert_eq!(session.messages[1].role, Role::Assistant);
    assert!(session.messages[1].content.contains("**answer** here"));
    assert_eq!(session.messages[2].role, Role::Tool);
    assert!(session.messages[2].content.contains("```"));
}

#[test]
fn parses_kimi_context_jsonl() {
    let lines = [
        r#"{"role":"_system_prompt","content":"sys"}"#,
        r#"{"role":"user","content":"q?"}"#,
        r#"{"role":"assistant","content":[{"type":"think","think":"t"},{"type":"text","text":"a!"}],"tool_calls":[{"type":"function","id":"x","function":{"name":"Read","arguments":"{\"path\":\"f\"}"}}]}"#,
        r#"{"role":"tool","content":"raw out"}"#,
    ]
    .join("\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("context.jsonl");
    std::fs::write(&path, lines).unwrap();

    let session = parse_kimi_jsonl(&path).unwrap();
    assert_eq!(session.kind, CliKind::Kimi);
    assert_eq!(session.messages.len(), 3);
    assert_eq!(session.messages[0].role, Role::User);
    assert_eq!(session.messages[1].role, Role::Assistant);
    assert!(session.messages[1].content.contains("a!"));
    assert!(session.messages[1].content.contains("**Tool use: `Read`**"));
    assert_eq!(session.messages[2].role, Role::Tool);
    assert!(session.messages[2].content.contains("raw out"));
}

#[test]
fn parses_kimi_legacy_wire() {
    let lines = [
        r#"{"type":"metadata","protocol_version":"1.9"}"#,
        r#"{"timestamp":1700000000.0,"message":{"type":"TurnBegin","payload":{"user_input":[{"type":"text","text":"legacy q"}]}}}"#,
        r#"{"timestamp":1700000001.0,"message":{"type":"StepBegin","payload":{"n":1}}}"#,
        r#"{"timestamp":1700000002.0,"message":{"type":"ContentPart","payload":{"type":"think","think":"t"}}}"#,
        r#"{"timestamp":1700000003.0,"message":{"type":"ContentPart","payload":{"type":"text","text":"legacy a"}}}"#,
    ]
    .join("\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wire.jsonl");
    std::fs::write(&path, lines).unwrap();

    let session = parse_kimi_jsonl(&path).unwrap();
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].role, Role::User);
    assert!(session.messages[0].content.contains("legacy q"));
    assert_eq!(
        session.messages[0].timestamp.as_deref(),
        Some("2023-11-14T22:13:20.000Z")
    );
    assert_eq!(session.messages[1].role, Role::Assistant);
    assert!(session.messages[1].content.contains("legacy a"));
}

// ---------------------------------------------------------------------------
// codex
// ---------------------------------------------------------------------------

fn codex_fixture() -> String {
    [
        r#"{"timestamp":"2026-08-27T14:35:32.872Z","type":"session_meta","payload":{"id":"codex-session-1"}}"#,
        r#"{"timestamp":"2026-08-27T14:35:48.081Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/x</cwd>\n</environment_context>\n\nreal question"}]}}"#,
        r#"{"timestamp":"2026-08-27T14:35:52.033Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"codex answer"}]}}"#,
        r#"{"timestamp":"2026-08-27T14:35:53.000Z","type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}"#,
        r#"{"timestamp":"2026-08-27T14:35:54.000Z","type":"response_item","payload":{"type":"function_call_output","output":"file-a"}}"#,
        r#"{"timestamp":"2026-08-27T14:35:55.000Z","type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"skip me"}]}}"#,
    ]
    .join("\n")
}

#[test]
fn parses_codex_rollout() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-1.jsonl");
    std::fs::write(&path, codex_fixture()).unwrap();

    let session = parse_codex_jsonl(&path).unwrap();
    assert_eq!(session.kind, CliKind::Codex);
    assert_eq!(session.id, "codex-session-1");
    assert_eq!(session.messages.len(), 4);
    assert_eq!(session.messages[0].role, Role::User);
    assert_eq!(session.messages[0].content, "real question");
    assert_eq!(session.messages[1].role, Role::Assistant);
    assert!(session.messages[1].content.contains("codex answer"));
    assert_eq!(session.messages[2].role, Role::Assistant);
    assert!(session.messages[2].content.contains("**Tool use: `shell`**"));
    assert_eq!(session.messages[3].role, Role::Tool);
    assert!(session.messages[3].content.contains("file-a"));
}

#[test]
fn codex_falls_back_to_event_msg() {
    let lines = [
        r#"{"timestamp":"2026-08-27T14:35:48.098Z","type":"event_msg","payload":{"type":"user_message","message":"fallback q"}}"#,
        r#"{"timestamp":"2026-08-27T14:35:52.033Z","type":"event_msg","payload":{"type":"agent_message","message":"fallback a"}}"#,
        r#"{"timestamp":"2026-08-27T14:35:53.033Z","type":"event_msg","payload":{"type":"token_count"}}"#,
    ]
    .join("\n");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rollout-2.jsonl");
    std::fs::write(&path, lines).unwrap();

    let session = parse_codex_jsonl(&path).unwrap();
    assert_eq!(session.messages.len(), 2);
    assert_eq!(session.messages[0].role, Role::User);
    assert!(session.messages[0].content.contains("fallback q"));
    assert_eq!(session.messages[1].role, Role::Assistant);
    assert!(session.messages[1].content.contains("fallback a"));
}

// ---------------------------------------------------------------------------
// selfdefined
// ---------------------------------------------------------------------------

fn selfdefined_fixture() -> String {
    [
        r#"{"type":"metadata","sessionId":"sd-1","title":"demo"}"#,
        r#"{"type":"message","role":"user","content":"sd question","timestamp":"2026-09-30T13:00:00Z"}"#,
        r#"{"type":"message","role":"assistant","content":[{"type":"text","text":"sd answer"}],"model":"m1"}"#,
        r#"{"type":"message","role":"robot","content":"skipped"}"#,
        r#"{"type":"message","role":"tool","content":"tool body"}"#,
        "{garbage line",
    ]
    .join("\n")
}

#[test]
fn parses_selfdefined() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sd.jsonl");
    std::fs::write(&path, selfdefined_fixture()).unwrap();

    let session = parse_selfdefined_jsonl(&path).unwrap();
    assert_eq!(session.kind, CliKind::Selfdefined);
    assert_eq!(session.id, "sd-1");
    assert_eq!(session.messages.len(), 3);
    assert_eq!(session.messages[0].role, Role::User);
    assert_eq!(
        session.messages[0].timestamp.as_deref(),
        Some("2026-09-30T13:00:00Z")
    );
    assert_eq!(session.messages[1].role, Role::Assistant);
    assert_eq!(session.messages[1].model.as_deref(), Some("m1"));
    assert_eq!(session.messages[2].role, Role::Tool);
}

// ---------------------------------------------------------------------------
// dispatch & sniffing
// ---------------------------------------------------------------------------

#[test]
fn parse_transcript_sniffs_each_format() {
    let dir = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, String, CliKind)> = vec![
        ("c.jsonl", fixture_jsonl(), CliKind::Claude),
        ("k-wire.jsonl", kimi_wire_v15_fixture(), CliKind::Kimi),
        ("cx.jsonl", codex_fixture(), CliKind::Codex),
        ("sd.jsonl", selfdefined_fixture(), CliKind::Selfdefined),
    ];
    for (name, body, want) in cases {
        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        let session = parse_transcript(&path, None).unwrap();
        assert_eq!(session.kind, want, "sniffed wrong provider for {name}");
        assert!(!session.messages.is_empty(), "no messages parsed for {name}");
    }
}

#[test]
fn parse_transcript_explicit_provider_overrides_sniff() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("k.jsonl");
    std::fs::write(&path, kimi_wire_v15_fixture()).unwrap();
    // Parsed as Claude: kimi lines match nothing, so no messages.
    let session = parse_transcript(&path, Some(CliKind::Claude)).unwrap();
    assert_eq!(session.kind, CliKind::Claude);
    assert!(session.messages.is_empty());
}

#[test]
fn parse_transcript_unknown_content_defaults_to_claude() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mystery.jsonl");
    std::fs::write(&path, "{\"hello\":\"world\"}\n").unwrap();
    let session = parse_transcript(&path, None).unwrap();
    assert_eq!(session.kind, CliKind::Claude);
}

// ---------------------------------------------------------------------------
// discovery: new roots
// ---------------------------------------------------------------------------

#[test]
fn discovery_finds_kimi_code_layout() {
    let home = tempfile::tempdir().unwrap();
    let dir = home
        .path()
        .join(".kimi-code/sessions/wd_x/session_y/agents/main");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("wire.jsonl");
    std::fs::write(&f, "{}").unwrap();
    let found = discover_latest_session_under(CliKind::Kimi, home.path()).unwrap();
    assert_eq!(found, f);
}

#[test]
fn discovery_finds_codex_layout() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(".codex/sessions/2026/08/27");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("rollout-1.jsonl");
    std::fs::write(&f, "{}").unwrap();
    let found = discover_latest_session_under(CliKind::Codex, home.path()).unwrap();
    assert_eq!(found, f);
}

#[test]
fn discovery_any_picks_newest_across_providers() {
    let home = tempfile::tempdir().unwrap();
    let claude_dir = home.path().join(".claude/projects/p");
    let kimi_dir = home
        .path()
        .join(".kimi-code/sessions/wd_x/session_y/agents/main");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::create_dir_all(&kimi_dir).unwrap();
    let claude_f = claude_dir.join("c.jsonl");
    let kimi_f = kimi_dir.join("wire.jsonl");
    std::fs::write(&claude_f, "{}").unwrap();
    std::fs::write(&kimi_f, "{}").unwrap();

    let now = filetime_now();
    set_mtime(&claude_f, now - 100);
    set_mtime(&kimi_f, now);

    let (path, kind) = discover_latest_any_under(home.path()).unwrap();
    assert_eq!(path, kimi_f);
    assert_eq!(kind, CliKind::Kimi);

    // Claude newer -> Claude wins.
    set_mtime(&claude_f, now + 100);
    let (path, kind) = discover_latest_any_under(home.path()).unwrap();
    assert_eq!(path, claude_f);
    assert_eq!(kind, CliKind::Claude);
}
