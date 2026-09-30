use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

fn fixture_transcript() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cli-fixture.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"cli fixture question\"},\"sessionId\":\"cli-1\"}\n",
    )
    .unwrap();
    (dir, path)
}

#[test]
fn help_lists_all_subcommands() {
    let out = Command::new(env!("CARGO_BIN_EXE_mdterm-llm"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for sub in ["wrap", "serve", "render", "caps"] {
        assert!(stdout.contains(sub), "missing {sub} in --help: {stdout}");
    }
}

#[test]
fn caps_prints_detection() {
    let out = Command::new(env!("CARGO_BIN_EXE_mdterm-llm"))
        .arg("caps")
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for needle in ["graphics protocol:", "truecolor:", "terminal program:", "TERM="] {
        assert!(stdout.contains(needle), "missing {needle:?} in: {stdout}");
    }
}

#[test]
fn render_file_emits_ansi_for_table_code_and_math() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.md");
    std::fs::write(
        &path,
        "# Heading\n\n\
         | A | B |\n|---|---|\n| 1 | 2 |\n\n\
         ```rust\nfn main() {}\n```\n\n\
         Inline math $\\alpha + \\frac{1}{2}$ here.\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mdterm-llm"))
        .args(["render", "--file"])
        .arg(&path)
        .args(["--math", "unicode", "--width", "60"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\x1b["), "expected ANSI escapes: {stdout:?}");
    assert!(stdout.contains('┌'), "expected table border: {stdout:?}");
    assert!(stdout.contains('α'), "expected unicode math: {stdout:?}");
}

#[test]
fn render_transcript_last_message() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(
        &path,
        concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"q\"},\"sessionId\":\"s1\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"**bold answer**\"}]},\"sessionId\":\"s1\"}\n",
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_mdterm-llm"))
        .args(["render", "--last", "--transcript"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("\x1b[1m"), "expected bold ANSI: {stdout:?}");
    assert!(stdout.contains("bold answer"), "expected content: {stdout:?}");
    // --last renders only the assistant message, not the user's "q".
    assert!(!stdout.contains('q'), "expected only last message: {stdout:?}");
}

#[test]
fn serve_watches_transcript_and_serves_session() {
    let (_dir, path) = fixture_transcript();
    let mut child = Command::new(env!("CARGO_BIN_EXE_mdterm-llm"))
        .args(["serve", "--transcript"])
        .arg(&path)
        .args(["--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // First stdout line reports the bound URL.
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        assert!(std::time::Instant::now() < deadline, "no URL line printed");
        if stdout.read_line(&mut line).unwrap() > 0 && line.contains("http://127.0.0.1:") {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let url = line
        .split_whitespace()
        .find(|t| t.starts_with("http://"))
        .expect("URL on first line")
        .trim_end_matches('/')
        .to_string();

    // The server should reflect the fixture transcript.
    let body = reqwest::blocking::get(format!("{url}/api/session"))
        .unwrap()
        .text()
        .unwrap();
    assert!(body.contains("cli-1"), "session body: {body}");
    assert!(body.contains("cli fixture question"), "session body: {body}");

    child.kill().unwrap();
    child.wait().unwrap();
}
