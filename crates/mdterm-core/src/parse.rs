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

// ---------------------------------------------------------------------------
// provider dispatch & format sniffing
// ---------------------------------------------------------------------------

/// Parse a transcript for an explicit provider, or sniff the format from
/// the file's contents when `kind` is `None`. Unknown content defaults to
/// the Claude parser (the most permissive: unrecognized lines are skipped).
pub fn parse_transcript(path: &Path, kind: Option<CliKind>) -> anyhow::Result<Session> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading transcript {}", path.display()))?;
    let kind = kind.unwrap_or_else(|| sniff_kind(&raw).unwrap_or(CliKind::Claude));
    let id = match kind {
        CliKind::Kimi => kimi_fallback_id(path),
        _ => fallback_id(path),
    };
    Ok(match kind {
        CliKind::Claude => parse_claude_jsonl_str(&raw, id),
        CliKind::Kimi => parse_kimi_jsonl_str(&raw, id),
        CliKind::Codex => parse_codex_jsonl_str(&raw, id),
        CliKind::Selfdefined => parse_selfdefined_jsonl_str(&raw, id),
    })
}

/// Detect the transcript format from the first JSON lines. Signatures are
/// mutually exclusive across providers; first hit wins.
fn sniff_kind(raw: &str) -> Option<CliKind> {
    for line in raw.lines().take(200) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");

        // selfdefined canonical lines: {"type":"message",...} /
        // {"type":"metadata","sessionId":...} (no protocol_version).
        if t == "message"
            || (t == "metadata" && v.get("protocol_version").is_none()
                && (v.get("sessionId").is_some() || v.get("session_id").is_some()))
        {
            return Some(CliKind::Selfdefined);
        }
        // codex rollout: typed envelope + payload object.
        if matches!(t, "response_item" | "event_msg" | "session_meta" | "turn_context")
            && v.get("payload").is_some()
        {
            return Some(CliKind::Codex);
        }
        // kimi-code wire (protocol >= 1.5): dotted event types + agentId.
        if t.contains('.') || v.get("agentId").is_some() {
            return Some(CliKind::Kimi);
        }
        // any kimi wire: metadata banner shared by old and new protocols.
        if t == "metadata" && v.get("protocol_version").is_some() {
            return Some(CliKind::Kimi);
        }
        // old kimi wire / kimi context.jsonl: no top-level "type".
        if t.is_empty() {
            let mt = v
                .pointer("/message/type")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            if matches!(mt, "TurnBegin" | "ContentPart" | "StepBegin" | "TurnEnd") {
                return Some(CliKind::Kimi);
            }
            if v.get("role").is_some() && v.get("content").is_some() {
                return Some(CliKind::Kimi);
            }
        }
        // Claude Code: {"type":"user"|"assistant"|"system"|"summary",...}.
        if matches!(t, "user" | "assistant" | "system" | "summary") {
            return Some(CliKind::Claude);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Kimi (kimi-code wire v1.5, legacy wire, context.jsonl — one parser)
// ---------------------------------------------------------------------------

/// Parse a Kimi transcript into a [`Session`]. Three line shapes are
/// handled (unknown/malformed lines are skipped, never fatal):
///
/// - kimi-code wire (protocol >= 1.5):
///   `{"type":"agent.message.appended","time":<ms>,
///     "message":{"message":{"role":…,"content":[blocks]}}}`
/// - legacy wire (~/.kimi): `{"timestamp":<secs>,
///   "message":{"type":"TurnBegin"|"ContentPart","payload":…}}`
/// - context.jsonl message list: `{"role":…,"content":…,"tool_calls":…}`
pub fn parse_kimi_jsonl(path: &Path) -> anyhow::Result<Session> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading transcript {}", path.display()))?;
    Ok(parse_kimi_jsonl_str(&raw, kimi_fallback_id(path)))
}

pub(crate) fn parse_kimi_jsonl_str(raw: &str, fallback_id: String) -> Session {
    let mut messages = Vec::new();

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            tracing::debug!("skipping malformed jsonl line");
            continue;
        };
        let t = v.get("type").and_then(|x| x.as_str()).unwrap_or("");

        if t == "agent.message.appended" {
            let ts = v.get("time").and_then(|x| x.as_i64()).map(epoch_ms_to_iso);
            if let Some(m) = v.pointer("/message/message") {
                extract_kimi_message(m, ts, &mut messages);
            }
            continue;
        }

        if t.is_empty() {
            if v.get("role").is_some() {
                // context.jsonl shape: the line IS the message.
                let ts = v
                    .get("timestamp")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string());
                extract_kimi_message(&v, ts, &mut messages);
            } else {
                // legacy wire shape: nested message.type envelope.
                let mt = v
                    .pointer("/message/type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let ts = v
                    .get("timestamp")
                    .and_then(|x| x.as_f64())
                    .map(|s| epoch_ms_to_iso((s * 1000.0) as i64));
                match mt {
                    "TurnBegin" => {
                        let text = join_text_blocks(
                            &v.pointer("/message/payload/user_input")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                            "text",
                        );
                        if !text.trim().is_empty() {
                            messages.push(Message {
                                role: Role::User,
                                content: text,
                                timestamp: ts,
                                model: None,
                            });
                        }
                    }
                    "ContentPart" => {
                        let p = v.pointer("/message/payload").cloned().unwrap_or_default();
                        if p.get("type").and_then(|x| x.as_str()) == Some("text") {
                            if let Some(text) = p.get("text").and_then(|x| x.as_str()) {
                                if !text.is_empty() {
                                    messages.push(Message {
                                        role: Role::Assistant,
                                        content: text.to_string(),
                                        timestamp: ts,
                                        model: None,
                                    });
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Session {
        id: fallback_id,
        kind: CliKind::Kimi,
        messages,
    }
}

/// Shared extraction for the two kimi shapes that carry a whole message
/// object (`role` + `content` + optional `tool_calls`).
fn extract_kimi_message(
    m: &serde_json::Value,
    timestamp: Option<String>,
    out: &mut Vec<Message>,
) {
    let role = match m.get("role").and_then(|r| r.as_str()) {
        Some("user") => Role::User,
        Some("assistant") => Role::Assistant,
        Some("tool") => Role::Tool,
        _ => return, // system prompts and unknown roles are not conversation
    };
    let mut body_parts: Vec<String> = Vec::new();
    match m.get("content") {
        Some(serde_json::Value::String(s)) => {
            if !s.trim().is_empty() {
                body_parts.push(s.clone());
            }
        }
        Some(serde_json::Value::Array(_)) => {
            let text = join_text_blocks(m.get("content").unwrap(), "text");
            if !text.trim().is_empty() {
                body_parts.push(text);
            }
        }
        _ => {}
    }
    // context.jsonl assistant turns carry tool_calls alongside the content.
    if let Some(calls) = m.get("tool_calls").and_then(|c| c.as_array()) {
        for call in calls {
            let name = call
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .unwrap_or("unknown");
            let input = call
                .pointer("/function/arguments")
                .and_then(|a| a.as_str())
                .and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())
                .unwrap_or(serde_json::Value::Null);
            let input_pretty =
                serde_json::to_string_pretty(&input).unwrap_or_else(|_| input.to_string());
            body_parts.push(format!("**Tool use: `{name}`**\n\n```json\n{input_pretty}\n```"));
        }
    }
    let body = body_parts.join("\n\n");
    if body.trim().is_empty() {
        return; // think-only or tool-call-routing messages carry no prose
    }
    let content = if role == Role::Tool {
        format!("```\n{body}\n```")
    } else {
        body
    };
    out.push(Message {
        role,
        content,
        timestamp,
        model: None,
    });
}

/// Join the `text`-typed blocks of a content array with blank lines.
fn join_text_blocks(content: &serde_json::Value, text_key: &str) -> String {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some(text_key))
                .filter_map(|b| b.get(text_key).and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

/// Session id fallback for kimi layouts: use the session directory name
/// rather than the generic file stem (`wire` / `context`).
fn kimi_fallback_id(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if matches!(stem, "wire" | "context") {
        for anc in path.ancestors().skip(1) {
            let name = anc.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !matches!(name, "main" | "agents" | "sessions" | "") {
                return name.to_string();
            }
        }
    }
    fallback_id(path)
}

// ---------------------------------------------------------------------------
// Codex (~/.codex/sessions/**/rollout-*.jsonl)
// ---------------------------------------------------------------------------

/// Parse a Codex rollout into a [`Session`]. Primary source:
/// `response_item` lines with `payload.type == "message"` (role
/// user/assistant, text in `input_text`/`output_text` blocks); tool calls
/// (`function_call` / `custom_tool_call`) are summarized like Claude's
/// `tool_use`, outputs become Tool messages. Rollouts without response_item
/// messages fall back to `event_msg` `user_message` / `agent_message`
/// lines. Unknown or malformed lines are skipped, never fatal.
pub fn parse_codex_jsonl(path: &Path) -> anyhow::Result<Session> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading transcript {}", path.display()))?;
    Ok(parse_codex_jsonl_str(&raw, fallback_id(path)))
}

pub(crate) fn parse_codex_jsonl_str(raw: &str, fallback_id: String) -> Session {
    let mut messages = Vec::new();
    let mut session_id: Option<String> = None;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            tracing::debug!("skipping malformed jsonl line");
            continue;
        };
        let timestamp = v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string());
        let payload = v.get("payload").cloned().unwrap_or(serde_json::Value::Null);

        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "session_meta" => {
                if session_id.is_none() {
                    session_id = payload
                        .get("id")
                        .or_else(|| payload.get("session_id"))
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                }
            }
            "response_item" => {
                let pt = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match pt {
                    "message" => {
                        let role = match payload.get("role").and_then(|r| r.as_str()) {
                            Some("user") => Role::User,
                            Some("assistant") => Role::Assistant,
                            _ => continue,
                        };
                        let mut text = payload
                            .get("content")
                            .map(|c| join_codex_blocks(c))
                            .unwrap_or_default();
                        if role == Role::User {
                            text = strip_environment_context(&text);
                        }
                        if !text.trim().is_empty() {
                            messages.push(Message {
                                role,
                                content: text,
                                timestamp,
                                model: None,
                            });
                        }
                    }
                    "function_call" | "custom_tool_call" => {
                        let name = payload
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("unknown");
                        let input = payload
                            .get("arguments")
                            .or_else(|| payload.get("input"))
                            .cloned()
                            .unwrap_or(serde_json::Value::Null);
                        let input = match input {
                            serde_json::Value::String(s) => {
                                serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s))
                            }
                            other => other,
                        };
                        let pretty = serde_json::to_string_pretty(&input)
                            .unwrap_or_else(|_| input.to_string());
                        messages.push(Message {
                            role: Role::Assistant,
                            content: format!("**Tool use: `{name}`**\n\n```json\n{pretty}\n```"),
                            timestamp,
                            model: None,
                        });
                    }
                    "function_call_output" | "custom_tool_call_output" => {
                        let text = payload
                            .get("output")
                            .map(codex_output_text)
                            .unwrap_or_default();
                        if !text.trim().is_empty() {
                            messages.push(Message {
                                role: Role::Tool,
                                content: format!("```\n{text}\n```"),
                                timestamp,
                                model: None,
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if messages.is_empty() {
        // Older/minimal rollouts: only event_msg display lines exist.
        for line in raw.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            if v.get("type").and_then(|t| t.as_str()) != Some("event_msg") {
                continue;
            }
            let payload = v.get("payload").cloned().unwrap_or_default();
            let role = match payload.get("type").and_then(|t| t.as_str()) {
                Some("user_message") => Role::User,
                Some("agent_message") => Role::Assistant,
                _ => continue,
            };
            let text = payload
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            if !text.trim().is_empty() {
                messages.push(Message {
                    role,
                    content: text,
                    timestamp: v
                        .get("timestamp")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string()),
                    model: None,
                });
            }
        }
    }

    Session {
        id: session_id.unwrap_or(fallback_id),
        kind: CliKind::Codex,
        messages,
    }
}

/// Text of a codex message's content blocks (`input_text` / `output_text`).
fn join_codex_blocks(content: &serde_json::Value) -> String {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| {
                    matches!(
                        b.get("type").and_then(|t| t.as_str()),
                        Some("input_text") | Some("output_text") | Some("text")
                    )
                })
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

/// Codex wraps turn-opening user messages in an `<environment_context>`
/// envelope; drop it so the viewer shows the actual prompt.
fn strip_environment_context(text: &str) -> String {
    let trimmed = text.trim_start();
    if !trimmed.starts_with("<environment_context>") {
        return text.to_string();
    }
    match trimmed.find("</environment_context>") {
        Some(end) => trimmed[end + "</environment_context>".len()..]
            .trim_start()
            .to_string(),
        None => text.to_string(),
    }
}

/// Tool outputs may be a plain string or an object/array carrying text.
fn codex_output_text(output: &serde_json::Value) -> String {
    match output {
        serde_json::Value::String(s) => s.clone(),
        other => join_codex_blocks(other),
    }
}

// ---------------------------------------------------------------------------
// Selfdefined: mdterm's canonical transcript format
// ---------------------------------------------------------------------------

/// Parse mdterm's canonical self-defined transcript format — one JSON
/// object per line:
///
/// ```json
/// {"type":"metadata","sessionId":"my-session","title":"optional"}
/// {"type":"message","role":"user","content":"markdown text","timestamp":"2026-09-30T13:00:00Z"}
/// {"type":"message","role":"assistant","content":[{"type":"text","text":"..."}],"model":"..."}
/// ```
///
/// `role` is `user` | `assistant` | `system` | `tool`; `content` is a
/// string or an array of `text` blocks; `timestamp` and `model` are
/// optional strings. Unknown or malformed lines are skipped, never fatal.
pub fn parse_selfdefined_jsonl(path: &Path) -> anyhow::Result<Session> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading transcript {}", path.display()))?;
    Ok(parse_selfdefined_jsonl_str(&raw, fallback_id(path)))
}

pub(crate) fn parse_selfdefined_jsonl_str(raw: &str, fallback_id: String) -> Session {
    let mut messages = Vec::new();
    let mut session_id: Option<String> = None;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            tracing::debug!("skipping malformed jsonl line");
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "metadata" => {
                if session_id.is_none() {
                    session_id = v
                        .get("sessionId")
                        .or_else(|| v.get("session_id"))
                        .or_else(|| v.get("id"))
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                }
            }
            "message" => {
                let role = match v.get("role").and_then(|r| r.as_str()) {
                    Some("user") => Role::User,
                    Some("assistant") => Role::Assistant,
                    Some("system") => Role::System,
                    Some("tool") => Role::Tool,
                    _ => continue,
                };
                let timestamp = v
                    .get("timestamp")
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string());
                let model = v
                    .get("model")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string());
                extract_messages(role, v.get("content"), timestamp, model, &mut messages);
            }
            _ => {}
        }
    }

    Session {
        id: session_id.unwrap_or(fallback_id),
        kind: CliKind::Selfdefined,
        messages,
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// Epoch milliseconds → ISO-8601 UTC string (no chrono dependency).
/// Uses Howard Hinnant's civil-from-days algorithm.
fn epoch_ms_to_iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86400);
    let tod = secs.rem_euclid(86400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}
