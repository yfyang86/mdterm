//! Claude Code session JSONL parsing.
//!
//! Each line is a JSON object with fields like:
//!   - `type`: "user" | "assistant" | "system" | "summary" | ...
//!   - `message.role`, `message.content` (string or array of blocks with
//!     `type`: "text" | "tool_use" | "tool_result" | ...)
//!   - `timestamp`, `sessionId`
//!
//! Text blocks are concatenated; `tool_use` blocks become fenced code block
//! summaries (tool name + input JSON); `tool_result` blocks become `Tool`
//! role messages. Unknown or malformed lines are skipped, never fatal.

use std::path::Path;

use anyhow::Context;

use crate::{CliKind, Message, Role, Session};

/// Parse a Claude Code session JSONL file into a [`Session`].
pub fn parse_claude_jsonl(path: &Path) -> anyhow::Result<Session> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading transcript {}", path.display()))?;
    Ok(parse_claude_jsonl_str(&raw, fallback_id(path)))
}

/// The session id falls back to the file stem when no `sessionId` field
/// is present on any line.
fn fallback_id(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("session")
        .to_string()
}

/// Split out for testability: parse JSONL content directly.
pub(crate) fn parse_claude_jsonl_str(raw: &str, fallback_id: String) -> Session {
    let mut messages = Vec::new();
    let mut session_id: Option<String> = None;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            tracing::debug!("skipping malformed jsonl line");
            continue; // never fatal
        };

        if session_id.is_none() {
            if let Some(id) = v.get("sessionId").and_then(|s| s.as_str()) {
                if !id.is_empty() {
                    session_id = Some(id.to_string());
                }
            }
        }

        let line_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if line_type != "user" && line_type != "assistant" {
            continue; // system / summary / unknown lines are skipped
        }

        let message = v.get("message").cloned().unwrap_or(serde_json::Value::Null);
        let role = match message
            .get("role")
            .and_then(|r| r.as_str())
            .unwrap_or(line_type)
        {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        let timestamp = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());
        let model = message
            .get("model")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());

        extract_messages(role, message.get("content"), timestamp, model, &mut messages);
    }

    Session {
        id: session_id.unwrap_or(fallback_id),
        kind: CliKind::Claude,
        messages,
    }
}

/// Turn one line's `content` (string or block array) into messages.
fn extract_messages(
    role: Role,
    content: Option<&serde_json::Value>,
    timestamp: Option<String>,
    model: Option<String>,
    out: &mut Vec<Message>,
) {
    let mut body_parts: Vec<String> = Vec::new();

    match content {
        Some(serde_json::Value::String(s)) => {
            if !s.trim().is_empty() {
                body_parts.push(s.clone());
            }
        }
        Some(serde_json::Value::Array(blocks)) => {
            for block in blocks {
                let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match btype {
                    "text" => {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            if !t.is_empty() {
                                body_parts.push(t.to_string());
                            }
                        }
                    }
                    "tool_use" => {
                        body_parts.push(tool_use_summary(block));
                    }
                    "tool_result" => {
                        // Tool results ride on user lines but get their own
                        // Tool-role message so they render distinctly.
                        let text = tool_result_text(block);
                        if !text.trim().is_empty() {
                            out.push(Message {
                                role: Role::Tool,
                                content: text,
                                timestamp: timestamp.clone(),
                                model: model.clone(),
                            });
                        }
                    }
                    _ => { /* thinking, image, unknown blocks: skipped */ }
                }
            }
        }
        _ => { /* missing / null / unexpected content shape: skip */ }
    }

    let body = body_parts.join("\n\n");
    if !body.trim().is_empty() {
        out.push(Message {
            role,
            content: body,
            timestamp,
            model,
        });
    }
}

/// tool_use → fenced code block summary (name + input JSON).
fn tool_use_summary(block: &serde_json::Value) -> String {
    let name = block
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("unknown");
    let input = block.get("input").cloned().unwrap_or(serde_json::Value::Null);
    let input_pretty =
        serde_json::to_string_pretty(&input).unwrap_or_else(|_| input.to_string());
    format!("**Tool use: `{name}`**\n\n```json\n{input_pretty}\n```")
}

/// Flatten a tool_result block's content (string or array of text blocks).
fn tool_result_text(block: &serde_json::Value) -> String {
    match block.get("content") {
        Some(serde_json::Value::String(s)) => format!("```\n{s}\n```"),
        Some(serde_json::Value::Array(parts)) => {
            let text: Vec<&str> = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect();
            if text.is_empty() {
                String::new()
            } else {
                format!("```\n{}\n```", text.join("\n"))
            }
        }
        _ => String::new(),
    }
}
