//! mdterm-core: transcript model, discovery, watch and parse.
//!
//! See SPEC.md — this crate is CLI-agnostic plumbing: it locates the newest
//! session transcript written by an upstream AI coding CLI (Claude Code,
//! Kimi CLI, Codex), parses it into a [`Session`], and watches it for
//! updates. `Selfdefined` transcripts use mdterm's own canonical JSONL
//! format (see `parse::parse_selfdefined_jsonl`).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub mod parse;
pub mod watch;

pub use parse::{parse_claude_jsonl, parse_codex_jsonl, parse_kimi_jsonl, parse_selfdefined_jsonl, parse_transcript};
pub use watch::watch_transcript;

/// Which upstream CLI produced a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CliKind {
    Claude,
    Kimi,
    Codex,
    /// mdterm's own canonical JSONL format; never discovered, always
    /// passed explicitly via `--transcript`.
    Selfdefined,
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
/// Kimi: newest `*.jsonl` under `~/.kimi-code/sessions/**/` or
/// `~/.kimi/sessions/**/` (plus legacy probes of `~/.kimi/projects/**/` and
/// `~/.config/kimi/**/sessions`).
/// Codex: newest `*.jsonl` under `~/.codex/sessions/**/`.
/// Selfdefined has no discovery roots; returns `None` (pass `--transcript`).
pub fn discover_latest_session(kind: CliKind) -> Option<PathBuf> {
    let home = home_dir()?;
    discover_latest_session_under(kind, &home)
}

/// Locate the newest transcript across all discoverable providers;
/// returns the path together with the provider implied by its root.
pub fn discover_latest_any() -> Option<(PathBuf, CliKind)> {
    let home = home_dir()?;
    discover_latest_any_under(&home)
}

/// Same as [`discover_latest_any`] but with an explicit home directory.
pub fn discover_latest_any_under(home: &Path) -> Option<(PathBuf, CliKind)> {
    [CliKind::Claude, CliKind::Kimi, CliKind::Codex]
        .into_iter()
        .filter_map(|kind| {
            newest_jsonl_under(&roots_for(kind, home)).map(|(t, p)| (t, p, kind))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, path, kind)| (path, kind))
}

/// Same as [`discover_latest_session`] but with an explicit home directory.
/// Exposed for tests and for callers that sandbox `$HOME`.
pub fn discover_latest_session_under(kind: CliKind, home: &Path) -> Option<PathBuf> {
    newest_jsonl_under(&roots_for(kind, home)).map(|(_, p)| p)
}

fn roots_for(kind: CliKind, home: &Path) -> Vec<PathBuf> {
    match kind {
        CliKind::Claude => vec![home.join(".claude").join("projects")],
        CliKind::Kimi => vec![
            home.join(".kimi-code").join("sessions"),
            home.join(".kimi").join("sessions"),
            home.join(".kimi").join("projects"),
            home.join(".config").join("kimi"),
        ],
        CliKind::Codex => vec![home.join(".codex").join("sessions")],
        CliKind::Selfdefined => vec![],
    }
}

/// Newest `*.jsonl` (by mtime) under any of `roots`, oldest root last.
fn newest_jsonl_under(roots: &[PathBuf]) -> Option<(std::time::SystemTime, PathBuf)> {
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        for entry in walkdir::WalkDir::new(root)
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
            if root.ends_with("kimi")
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
    newest
}
