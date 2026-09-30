//! mdterm-core: transcript model, discovery, watch and parse.
//!
//! See SPEC.md — this crate is CLI-agnostic plumbing: it locates the newest
//! session transcript written by an upstream AI coding CLI (Claude Code,
//! Kimi CLI), parses it into a [`Session`], and watches it for updates.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod parse;
pub mod watch;

pub use parse::parse_claude_jsonl;
pub use watch::watch_transcript;

/// Which upstream CLI produced a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CliKind {
    Claude,
    Kimi,
}

/// Role of one message in a session transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    System,
    Tool,
}

/// One message in a session transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    /// Markdown body (tool calls summarized as fenced code).
    pub content: String,
    pub timestamp: Option<String>,
    pub model: Option<String>,
}

/// A fully-parsed session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub kind: CliKind,
    pub messages: Vec<Message>,
}

impl Session {
    /// An empty session placeholder (used before the first transcript event).
    pub fn empty(kind: CliKind) -> Self {
        Session {
            id: String::new(),
            kind,
            messages: Vec::new(),
        }
    }
}

/// Events emitted by the watcher.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TranscriptEvent {
    Updated(Session),
    Error(String),
}

fn home_dir() -> Option<PathBuf> {
    #[allow(deprecated)]
    std::env::home_dir()
}

/// Locate the most recently modified transcript for a CLI.
///
/// Claude: newest `*.jsonl` under `~/.claude/projects/**/`.
/// Kimi: probe `~/.kimi/projects/**/` and `~/.config/kimi/**/sessions`
/// (best-effort); returns `None` if not found (the caller may pass
/// `--transcript` explicitly).
pub fn discover_latest_session(kind: CliKind) -> Option<PathBuf> {
    let home = home_dir()?;
    discover_latest_session_under(kind, &home)
}

/// Same as [`discover_latest_session`] but with an explicit home directory.
/// Exposed for tests and for callers that sandbox `$HOME`.
pub fn discover_latest_session_under(kind: CliKind, home: &Path) -> Option<PathBuf> {
    let roots: Vec<PathBuf> = match kind {
        CliKind::Claude => vec![home.join(".claude").join("projects")],
        CliKind::Kimi => vec![
            home.join(".kimi").join("projects"),
            home.join(".config").join("kimi"),
        ],
    };

    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&root)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if !entry.file_type().is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            // For Kimi's ~/.config/kimi root, only consider files under a
            // `sessions` directory (best-effort probe per spec).
            if kind == CliKind::Kimi
                && root.ends_with("kimi")
                && !path
                    .ancestors()
                    .any(|a| a.file_name().and_then(|n| n.to_str()) == Some("sessions"))
            {
                continue;
            }
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            let replace = match &newest {
                Some((t, _)) => mtime > *t,
                None => true,
            };
            if replace {
                newest = Some((mtime, path.to_path_buf()));
            }
        }
    }
    newest.map(|(_, p)| p)
}
