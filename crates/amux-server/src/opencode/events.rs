//! Provider stream-event -> [`WorkerEvent`] translation (RR-0030).
//!
//! Pure functions: one JSON line from a provider's structured stdout stream
//! in, zero or more `WorkerEvent`s out. No I/O, no clock, no state — the
//! caller ([`super::structured::StructuredCliProtocol`]) owns the child
//! process, mints the [`TurnId`], and inserts `TurnStarted` for providers
//! whose streams have no explicit turn-start marker (Claude Code and Gemini
//! CLI; Codex emits a literal `turn.started`).
//!
//! Shapes were captured LIVE on 2026-08-09 from the binaries on this machine
//! (Claude Code 2.1.226, Codex CLI 0.141.0, Gemini CLI 0.53.1); the test
//! fixtures below are the actual captured lines except where a comment says
//! SYNTHETIC. Two discoveries relative to docs/provider-coverage.csv (which
//! recorded rate limits as terminal-scrape-only):
//!
//! - Claude Code 2.1.226 emits `{"type":"rate_limit_event","rate_limit_info":
//!   {...}}` in the stream-json output. We translate it to `RateLimited` when
//!   `status != "allowed"` (only `"allowed"` was observed live, so the
//!   non-allowed mapping is conservative and keeps `raw` for diagnosis —
//!   ethos rule 4).
//! - Codex `exec --json` surfaces its usage limit as a structured
//!   `{"type":"error"}` + `{"type":"turn.failed"}` pair (captured live when
//!   this machine's Codex quota was exhausted). Limit-shaped messages become
//!   `RateLimited`; everything else becomes `Failed`.
//!
//! The terminal adapter remains the rate-limit path for anything these
//! streams do not express (e.g. Claude's interactive rate-limit menu).
//!
//! A malformed or unrecognized line translates to NO events and never
//! panics: unknown event types are forward-compatibility, not errors — a
//! newer CLI adding event kinds must not break the reader (Invariant 8).
//! Tool RESULTS (Claude `user`/`tool_result`, Gemini `tool_result`) are
//! deliberately untranslated: raw tool output belongs in logs, correlated by
//! turn, not in the event stream (Invariant 30).

use amux_core::ids::TurnId;
use amux_core::protocol::{
    Failure, ProgressReport, RateLimit, RateLimitKind, ToolEvent, TurnResult, WorkerEvent,
};
use amux_core::provider::ProviderId;
use serde::Serialize;
use serde_json::Value;

/// One-line, bounded summary for `ProgressReport`/`ToolEvent` detail fields:
/// first line only, capped at 120 chars. Events are headlines, not
/// transcripts (Invariant 30).
fn summarize(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(120).collect();
    if first.chars().count() > 120 {
        out.push('…');
    }
    out
}

/// Best-effort human summary of a tool-call input object: prefer the fields
/// that carry the action (a Bash `command`, an Edit `file_path`), fall back
/// to compact JSON. Never the full payload — that goes to logs.
fn tool_detail(input: &Value) -> Option<String> {
    for key in ["command", "cmd", "description", "file_path", "path", "pattern", "query", "url"] {
        if let Some(s) = input.get(key).and_then(Value::as_str) {
            if !s.trim().is_empty() {
                return Some(summarize(s));
            }
        }
    }
    serde_json::to_string(input).ok().map(|s| summarize(&s))
}

/// Does a provider error message describe a quota/rate limit? Used for Codex,
/// whose limit errors arrive as free-text `error` events (real capture
/// below: "You've hit your usage limit...").
fn is_rate_limit_message(msg: &str) -> bool {
    let m = msg.to_lowercase();
    ["usage limit", "rate limit", "quota", "too many requests"]
        .iter()
        .any(|needle| m.contains(needle))
}

// ---------------------------------------------------------------------------
// Claude Code: `claude --print <prompt> --output-format stream-json --verbose`
// ---------------------------------------------------------------------------

/// Map Claude's `rateLimitType` onto [`RateLimitKind`]. The live capture
/// showed `"five_hour"`, which has no honest cell in the enum — it maps to
/// `Unknown` (a first-class answer, not an error) with the original object
/// preserved in `raw`.
fn claude_rate_limit_kind(t: &str) -> RateLimitKind {
    let t = t.to_lowercase();
    if t.contains("minute") {
        RateLimitKind::PerMinute
    } else if t.contains("daily") || t.contains("day") {
        RateLimitKind::Daily
    } else if t.contains("week") {
        RateLimitKind::Weekly
    } else if t.contains("credit") || t.contains("overage") {
        RateLimitKind::Credit
    } else {
        RateLimitKind::Unknown
    }
}

/// Translate one line of Claude Code stream-json.
///
/// Line types observed live: `system/init`, `system/thinking_tokens`,
/// `assistant` (content blocks: `thinking`, `text`, `tool_use`),
/// `rate_limit_event`, `user` (tool results), `result`.
pub fn translate_claude(line: &str, turn: &TurnId) -> Vec<WorkerEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    match v.get("type").and_then(Value::as_str) {
        Some("system") => match v.get("subtype").and_then(Value::as_str) {
            Some("init") => vec![WorkerEvent::Started],
            Some("thinking_tokens") => vec![WorkerEvent::Progress(ProgressReport {
                summary: "thinking".to_string(),
                tokens_used: v.get("estimated_tokens").and_then(Value::as_u64),
            })],
            _ => Vec::new(),
        },
        Some("assistant") => {
            let mut out = Vec::new();
            if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                if !text.trim().is_empty() {
                                    out.push(WorkerEvent::Progress(ProgressReport {
                                        summary: summarize(text),
                                        tokens_used: None,
                                    }));
                                }
                            }
                        }
                        Some("tool_use") => {
                            let tool = block
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown")
                                .to_string();
                            let detail = block.get("input").and_then(tool_detail);
                            out.push(WorkerEvent::ToolUsed(ToolEvent { tool, detail }));
                        }
                        // `thinking` blocks: transcript, not telemetry.
                        _ => {}
                    }
                }
            }
            out
        }
        Some("rate_limit_event") => {
            let Some(info) = v.get("rate_limit_info") else {
                return Vec::new();
            };
            // Only "allowed" was observed live; anything else is the limit
            // actually biting. `raw` keeps the whole object so a wrong
            // normalization is diagnosable later (ethos rule 4).
            let status = info.get("status").and_then(Value::as_str).unwrap_or("");
            if status == "allowed" {
                return Vec::new();
            }
            let kind = info
                .get("rateLimitType")
                .and_then(Value::as_str)
                .map(claude_rate_limit_kind)
                .unwrap_or(RateLimitKind::Unknown);
            let reset_at = info
                .get("resetsAt")
                .and_then(Value::as_i64)
                .and_then(|secs| chrono::DateTime::from_timestamp(secs, 0));
            vec![WorkerEvent::RateLimited(RateLimit {
                kind,
                reset_at,
                provider: ProviderId::new("claude-code"),
                raw: serde_json::to_string(info).ok(),
            })]
        }
        // `user` lines carry tool RESULTS — logs, not events (Invariant 30).
        Some("result") => {
            let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let subtype = v.get("subtype").and_then(Value::as_str).unwrap_or("unknown");
            let text = v.get("result").and_then(Value::as_str).unwrap_or("");
            if !is_error && subtype == "success" {
                let outcome = if text.trim().is_empty() {
                    "success".to_string()
                } else {
                    format!("success: {}", summarize(text))
                };
                vec![WorkerEvent::TurnCompleted(TurnResult {
                    turn_id: turn.clone(),
                    outcome,
                })]
            } else {
                // A turn that ended in error is still a turn that ended:
                // Failed carries the diagnosis, TurnCompleted closes the turn.
                let reason = if text.trim().is_empty() {
                    subtype.to_string()
                } else {
                    format!("{subtype}: {}", summarize(text))
                };
                vec![
                    WorkerEvent::Failed(Failure {
                        reason,
                        retryable: true,
                    }),
                    WorkerEvent::TurnCompleted(TurnResult {
                        turn_id: turn.clone(),
                        outcome: subtype.to_string(),
                    }),
                ]
            }
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Codex CLI: `codex exec --json <prompt>` (JSONL)
// ---------------------------------------------------------------------------

/// Translate one line of Codex `exec --json` JSONL.
///
/// Event types observed live: `thread.started`, `turn.started`,
/// `item.completed` (item type `agent_message`), `turn.completed`, `error`,
/// `turn.failed`. `item.started`/`item.updated` are deliberately skipped:
/// the live stream delivered only completed items, and translating both
/// halves would double-count tool use when a pair does appear.
pub fn translate_codex(line: &str, turn: &TurnId) -> Vec<WorkerEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    match v.get("type").and_then(Value::as_str) {
        Some("thread.started") => vec![WorkerEvent::Started],
        Some("turn.started") => vec![WorkerEvent::TurnStarted {
            turn_id: turn.clone(),
        }],
        Some("turn.completed") => vec![WorkerEvent::TurnCompleted(TurnResult {
            turn_id: turn.clone(),
            outcome: "completed".to_string(),
        })],
        Some("turn.failed") => {
            let msg = v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("turn failed");
            let closing = WorkerEvent::TurnCompleted(TurnResult {
                turn_id: turn.clone(),
                outcome: format!("failed: {}", summarize(msg)),
            });
            if is_rate_limit_message(msg) {
                vec![codex_rate_limit(msg), closing]
            } else {
                vec![
                    WorkerEvent::Failed(Failure {
                        reason: msg.to_string(),
                        retryable: true,
                    }),
                    closing,
                ]
            }
        }
        Some("error") => {
            let msg = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            if is_rate_limit_message(msg) {
                vec![codex_rate_limit(msg)]
            } else {
                vec![WorkerEvent::Failed(Failure {
                    reason: msg.to_string(),
                    retryable: true,
                })]
            }
        }
        Some("item.completed") => {
            let Some(item) = v.get("item") else {
                return Vec::new();
            };
            match item.get("type").and_then(Value::as_str) {
                Some("agent_message") => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                    vec![WorkerEvent::Progress(ProgressReport {
                        summary: summarize(text),
                        tokens_used: None,
                    })]
                }
                Some("reasoning") => {
                    let text = item.get("text").and_then(Value::as_str).unwrap_or("");
                    let summary = if text.trim().is_empty() {
                        "thinking".to_string()
                    } else {
                        summarize(text)
                    };
                    vec![WorkerEvent::Progress(ProgressReport {
                        summary,
                        tokens_used: None,
                    })]
                }
                Some("command_execution") => {
                    let detail = item
                        .get("command")
                        .and_then(Value::as_str)
                        .map(summarize)
                        .or_else(|| tool_detail(item));
                    vec![WorkerEvent::ToolUsed(ToolEvent {
                        tool: "shell".to_string(),
                        detail,
                    })]
                }
                Some("mcp_tool_call") => {
                    let tool = item
                        .get("tool")
                        .and_then(Value::as_str)
                        .unwrap_or("mcp_tool_call")
                        .to_string();
                    vec![WorkerEvent::ToolUsed(ToolEvent {
                        tool,
                        detail: tool_detail(item),
                    })]
                }
                Some("web_search") => vec![WorkerEvent::ToolUsed(ToolEvent {
                    tool: "web_search".to_string(),
                    detail: tool_detail(item),
                })],
                Some("file_change") => vec![WorkerEvent::ToolUsed(ToolEvent {
                    tool: "file_change".to_string(),
                    detail: tool_detail(item),
                })],
                Some("error") => {
                    let msg = item
                        .get("message")
                        .and_then(Value::as_str)
                        .or_else(|| item.get("text").and_then(Value::as_str))
                        .unwrap_or("item error");
                    vec![WorkerEvent::Failed(Failure {
                        reason: msg.to_string(),
                        retryable: true,
                    })]
                }
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// Codex limit errors are free text; the window/kind is not machine-readable
/// (`Unknown` is the honest cell), and the reset time is an English date we
/// refuse to guess-parse — `raw` keeps the message for whoever needs it.
fn codex_rate_limit(msg: &str) -> WorkerEvent {
    WorkerEvent::RateLimited(RateLimit {
        kind: RateLimitKind::Unknown,
        reset_at: None,
        provider: ProviderId::new("codex"),
        raw: Some(msg.to_string()),
    })
}

// ---------------------------------------------------------------------------
// Gemini CLI: `gemini -p <prompt> --output-format stream-json`
// ---------------------------------------------------------------------------

/// Translate one line of Gemini CLI stream-json.
///
/// Line types observed live: `init`, `message` (role user/assistant, with
/// `delta: true` on streamed assistant chunks), `tool_use`, `tool_result`,
/// `result`. The `error` arm is defensive (not captured live): shaped after
/// the other providers' `message` field so a failure is not silently dropped.
pub fn translate_gemini(line: &str, turn: &TurnId) -> Vec<WorkerEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    match v.get("type").and_then(Value::as_str) {
        Some("init") => vec![WorkerEvent::Started],
        Some("message") => {
            if v.get("role").and_then(Value::as_str) != Some("assistant") {
                return Vec::new(); // the user echo of the prompt
            }
            let content = v.get("content").and_then(Value::as_str).unwrap_or("");
            if content.trim().is_empty() {
                return Vec::new();
            }
            vec![WorkerEvent::Progress(ProgressReport {
                summary: summarize(content),
                tokens_used: None,
            })]
        }
        Some("tool_use") => {
            let tool = v
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let detail = v.get("parameters").and_then(tool_detail);
            vec![WorkerEvent::ToolUsed(ToolEvent { tool, detail })]
        }
        // `tool_result`: raw tool output goes to logs, not events.
        Some("result") => {
            let status = v.get("status").and_then(Value::as_str).unwrap_or("unknown");
            let tokens = v.pointer("/stats/total_tokens").and_then(Value::as_u64);
            if status == "success" {
                let mut out = Vec::new();
                if let Some(t) = tokens {
                    // Final token accounting arrives only here; surface it as
                    // the last progress heartbeat before the turn closes.
                    out.push(WorkerEvent::Progress(ProgressReport {
                        summary: "turn finished".to_string(),
                        tokens_used: Some(t),
                    }));
                }
                out.push(WorkerEvent::TurnCompleted(TurnResult {
                    turn_id: turn.clone(),
                    outcome: "success".to_string(),
                }));
                out
            } else {
                vec![
                    WorkerEvent::Failed(Failure {
                        reason: format!("gemini result status: {status}"),
                        retryable: true,
                    }),
                    WorkerEvent::TurnCompleted(TurnResult {
                        turn_id: turn.clone(),
                        outcome: status.to_string(),
                    }),
                ]
            }
        }
        Some("error") => {
            let msg = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            vec![WorkerEvent::Failed(Failure {
                reason: msg.to_string(),
                retryable: true,
            })]
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Uniform TRANSCRIPT model (RR-0030 render path, AMUX-3201 slice 1).
//
// [`WorkerEvent`] above is TELEMETRY: one-line summaries capped at 120 chars,
// tool OUTPUT deliberately dropped, and no plan concept (Invariant 30:
// "headlines, not transcripts"). Rendering a Codex/Ollama worker's
// conversation the way Claude's is rendered needs CONTENT the telemetry model
// forbids: the full assistant message, the tool's output, and the
// `update_plan` checklist that shows up in the native TUI as "Updated Plan".
// So the transcript path carries its OWN content-bearing model. It is
// co-located with the telemetry translators because it reads the SAME provider
// records; the two never merge (one feeds status detection, one feeds a
// reader), which is why this is a projection of the rollout, not a second copy
// of `translate_codex`; the latter parses `codex exec --json`, a DIFFERENT
// schema from the on-disk rollout parsed here.
//
// Source of truth: Codex's rollout file
// (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`), one JSON object per line
// shaped `{type, payload}`. Every line is read from `response_item`, the
// canonical Responses-API record, and the `event_msg` mirror
// (`agent_message`/`agent_reasoning`/`user_message`, which repeat the same
// text) is skipped so nothing renders twice. Ollama runs through
// `codex --oss`, so it emits this identical format and needs no separate path.

/// One step of a Codex `update_plan` checklist (the native "Updated Plan").
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanStep {
    /// `pending` | `in_progress` | `completed` (Codex's own vocabulary; passed
    /// through verbatim so the renderer, not this parser, owns presentation).
    pub status: String,
    pub step: String,
}

/// A content-bearing transcript entry, provider-agnostic so one client
/// renderer draws Codex the way it draws Claude. `#[serde(tag = "kind")]`
/// matches the `WorkerEvent` wire shape so the client's event handling reads
/// the same way.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEvent {
    /// A prompt the human sent (environment/context wrappers stripped).
    User { text: String },
    /// Assistant prose.
    Assistant { text: String },
    /// The model's visible reasoning ("thinking"). Only emitted when the
    /// rollout actually carries plaintext; newer Codex builds encrypt it, in
    /// which case there is simply no reasoning block, the same as a Claude
    /// turn with no thinking.
    Reasoning { text: String },
    /// A tool call paired with its output (Codex writes the two as separate
    /// records; this reunites them by `call_id`, the way Claude pairs
    /// `tool_use` with `tool_result`).
    Tool {
        tool: String,
        detail: Option<String>,
        output: Option<String>,
    },
    /// An `update_plan` call, rendered as a checklist.
    Plan { steps: Vec<PlanStep> },
}

/// Flatten a `message` payload's `content` (array of `{type,text}` blocks, or a
/// bare string) into one string.
fn message_text(payload: &Value) -> String {
    match payload.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            let mut parts = Vec::new();
            for b in blocks {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    if !t.is_empty() {
                        parts.push(t);
                    }
                }
            }
            parts.join("\n")
        }
        _ => String::new(),
    }
}

/// Remove a `<tag>...</tag>` block (inclusive) wherever it appears. Codex's
/// first user message is a pure `<environment_context>` block, and real prompts
/// can carry a `<user_instructions>` block (harness chrome, not conversation).
fn remove_tag_block(s: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = s.to_string();
    while let Some(start) = out.find(&open) {
        if let Some(rel_end) = out[start..].find(&close) {
            let end = start + rel_end + close.len();
            out.replace_range(start..end, "");
        } else {
            out.replace_range(start.., "");
        }
    }
    out
}

/// Clean a user prompt: drop context/instruction wrappers and a leading
/// `[HH:MM AM]` send-time tag Codex prepends. Returns "" when nothing but
/// chrome remains (e.g. the opening environment_context message).
fn strip_context_wrapper(text: &str) -> String {
    let mut t = remove_tag_block(text, "environment_context");
    t = remove_tag_block(&t, "user_instructions");
    let t = t.trim();
    // Strip a single leading "[..]" time stamp, e.g. "[05:41 PM] do X".
    if let Some(rest) = t.strip_prefix('[') {
        if let Some(idx) = rest.find(']') {
            return rest[idx + 1..].trim().to_string();
        }
    }
    t.to_string()
}

/// Join a `reasoning` payload's `summary` blocks. `None` when the build
/// encrypted the reasoning (empty summary + `encrypted_content`).
fn reasoning_text(payload: &Value) -> Option<String> {
    let arr = payload.get("summary").and_then(Value::as_array)?;
    let mut parts = Vec::new();
    for b in arr {
        if let Some(t) = b.get("text").and_then(Value::as_str) {
            if !t.trim().is_empty() {
                parts.push(t.trim());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// A short human label for a tool call: for `function_call` the `arguments`
/// are a JSON STRING (parse, then prefer the action field via [`tool_detail`]);
/// for `custom_tool_call` (e.g. `apply_patch`) the `input` is raw text.
fn tool_call_detail(payload: &Value) -> Option<String> {
    if let Some(args) = payload.get("arguments").and_then(Value::as_str) {
        if let Ok(v) = serde_json::from_str::<Value>(args) {
            return tool_detail(&v);
        }
        return Some(summarize(args));
    }
    if let Some(input) = payload.get("input").and_then(Value::as_str) {
        return Some(summarize(input));
    }
    None
}

/// Codex wraps `exec_command` output with a metadata preamble
/// (`Chunk ID: …\nWall time: …\nProcess exited with code 0\n…\nOutput:\n<real>`).
/// Keep only the real output, and bound it so one noisy tool result cannot
/// dominate the transcript.
fn clean_tool_output(raw: &str) -> String {
    const CAP: usize = 4000;
    let body = if let Some(idx) = raw.find("\nOutput:\n") {
        &raw[idx + "\nOutput:\n".len()..]
    } else if let Some(rest) = raw.strip_prefix("Output:\n") {
        rest
    } else {
        raw
    };
    let body = body.trim_end();
    if body.chars().count() > CAP {
        let mut s: String = body.chars().take(CAP).collect();
        s.push('…');
        s
    } else {
        body.to_string()
    }
}

/// Pull the text out of a `*_output` payload's `output` field (a string in the
/// versions captured, an object carrying `output`, or native text/image blocks).
pub(crate) fn output_text(payload: &Value) -> String {
    match payload.get("output") {
        Some(Value::String(s)) => clean_tool_output(s),
        Some(Value::Array(blocks)) => {
            let text = blocks.iter().filter_map(|block| {
                if matches!(block["type"].as_str(), Some("text" | "input_text" | "output_text")) {
                    block["text"].as_str()
                } else {
                    None // Images are not terminal text or base64 scrollback.
                }
            }).collect::<Vec<_>>().join("\n");
            clean_tool_output(&text)
        }
        Some(other) => other
            .get("output")
            .and_then(Value::as_str)
            .map(clean_tool_output)
            .unwrap_or_default(),
        None => String::new(),
    }
}

/// Parse an `update_plan` call's `arguments` (JSON string) into steps.
fn parse_plan(payload: &Value) -> Option<Vec<PlanStep>> {
    let args = payload.get("arguments").and_then(Value::as_str)?;
    let v: Value = serde_json::from_str(args).ok()?;
    let arr = v.get("plan").and_then(Value::as_array)?;
    let steps: Vec<PlanStep> = arr
        .iter()
        .filter_map(|s| {
            let step = s.get("step").and_then(Value::as_str)?.trim().to_string();
            if step.is_empty() {
                return None;
            }
            Some(PlanStep {
                status: s
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("pending")
                    .to_string(),
                step,
            })
        })
        .collect();
    if steps.is_empty() {
        None
    } else {
        Some(steps)
    }
}

/// Project a Codex rollout (already parsed into per-line [`Value`]s, in file
/// order) into a uniform [`TranscriptEvent`] stream. Pure: no I/O, no clock;
/// the caller owns tailing the file. Unknown record/payload types are ignored
/// (forward-compatibility, same discipline as the telemetry translators).
pub fn codex_rollout_transcript(lines: &[Value]) -> Vec<TranscriptEvent> {
    // Pass 1: correlate each tool OUTPUT back to its call by `call_id`. Codex
    // writes the call and its result as separate records, and the reader wants
    // one tool block, the way Claude pairs tool_use with tool_result.
    let mut outputs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for v in lines {
        if v.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(p) = v.get("payload") else { continue };
        if matches!(
            p.get("type").and_then(Value::as_str),
            Some("function_call_output") | Some("custom_tool_call_output")
        ) {
            if let Some(id) = p.get("call_id").and_then(Value::as_str) {
                let out = output_text(p);
                if !out.trim().is_empty() {
                    outputs.insert(id.to_string(), out);
                }
            }
        }
    }
    // Pass 2: emit in file order from `response_item` only (the `event_msg`
    // mirror repeats the same text; skipping it is the whole dedup).
    let mut out = Vec::new();
    for v in lines {
        if v.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(p) = v.get("payload") else { continue };
        match p.get("type").and_then(Value::as_str) {
            Some("message") => {
                let role = p.get("role").and_then(Value::as_str).unwrap_or("");
                let text = message_text(p);
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }
                match role {
                    "user" => {
                        let cleaned = strip_context_wrapper(text);
                        if !cleaned.is_empty() {
                            out.push(TranscriptEvent::User { text: cleaned });
                        }
                    }
                    "assistant" => out.push(TranscriptEvent::Assistant {
                        text: text.to_string(),
                    }),
                    // developer/system messages are harness chrome, not the
                    // conversation the user is reading.
                    _ => {}
                }
            }
            Some("reasoning") => {
                if let Some(t) = reasoning_text(p) {
                    out.push(TranscriptEvent::Reasoning { text: t });
                }
            }
            Some("function_call") | Some("custom_tool_call") => {
                let name = p
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string();
                if name == "update_plan" {
                    if let Some(steps) = parse_plan(p) {
                        out.push(TranscriptEvent::Plan { steps });
                        continue;
                    }
                }
                let call_id = p.get("call_id").and_then(Value::as_str).unwrap_or("");
                let output = outputs.get(call_id).cloned();
                out.push(TranscriptEvent::Tool {
                    tool: name,
                    detail: tool_call_detail(p),
                    output,
                });
            }
            // `*_output` records were consumed in pass 1; unknown ignored.
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tests. Fixtures are REAL captured lines (2026-08-09, this machine) unless
// marked SYNTHETIC.
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    // pub(crate): structured.rs's conformance tests replay these REAL
    // captured lines through the full spawn->read->translate path, so the
    // same fixture certifies both the translator and the protocol.
    use super::*;

    fn turn() -> TurnId {
        TurnId::from_ulid("01JGXV0000000000000000TEST".parse().unwrap())
    }

    // -- Claude Code 2.1.226 (REAL captures) --------------------------------

    // REAL: `claude --print "say hi" --output-format stream-json --verbose`
    pub(crate) const CLAUDE_INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/private/tmp/claude-501/-Users-ethan-Dev-amux/c70e7c50-636b-4d3c-94b2-bc27d073f6e6/scratchpad/fixtures","session_id":"b7e8023e-530b-47ac-8515-a4e181bb7d58","tools":["Task","Bash","Edit","Read","Write"],"mcp_servers":[],"model":"claude-fable-5","permissionMode":"default","apiKeySource":"none","claude_code_version":"2.1.226","output_style":"default","uuid":"a00ba2ea-82a7-4bd8-a03e-76f94b5c259a","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required"}"#;
    // ^ REAL line with the tools/slash_commands/plugins arrays shortened for
    //   readability — the translator reads only type/subtype.

    // REAL, verbatim.
    const CLAUDE_THINKING: &str = r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":50,"estimated_tokens_delta":50,"uuid":"2faa13e7-4285-4e90-be0e-8e0b458352f8","session_id":"b7e8023e-530b-47ac-8515-a4e181bb7d58"}"#;

    // REAL, verbatim.
    pub(crate) const CLAUDE_TEXT: &str = r#"{"type":"assistant","message":{"model":"claude-fable-5","id":"msg_011CdsPbM9amFRj9VsRHbHJT","type":"message","role":"assistant","content":[{"type":"text","text":"Hi. Ready when you are — drop a task or question."}],"stop_reason":null,"stop_sequence":null,"stop_details":null,"usage":{"input_tokens":1,"cache_creation_input_tokens":2087,"cache_read_input_tokens":29296,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":2087},"output_tokens":4,"service_tier":"standard","inference_geo":"not_available"},"diagnostics":null,"context_management":null},"parent_tool_use_id":null,"session_id":"b7e8023e-530b-47ac-8515-a4e181bb7d58","uuid":"fc4de16b-0ff1-4866-8c96-0cdb9d5c612f","timestamp":"2026-08-09T15:12:29.804Z","request_id":"req_011CdsPbJYZyZr4sxT4vDJgn"}"#;

    // REAL, verbatim: from `claude --print "Run exactly this bash command:
    // echo amux-fixture. ..." --allowedTools "Bash(echo:*)"`.
    const CLAUDE_TOOL_USE: &str = r#"{"type":"assistant","message":{"model":"claude-fable-5","id":"msg_011CdsPgbhLQWTAcBPK21BHV","type":"message","role":"assistant","content":[{"type":"tool_use","id":"toolu_01WmCNWySEGTnX3KbsPuXNNV","name":"Bash","input":{"command":"echo amux-fixture","description":"Echo amux-fixture"},"caller":{"type":"direct"}}],"stop_reason":null,"stop_sequence":null,"stop_details":null,"usage":{"input_tokens":2,"cache_creation_input_tokens":10485,"cache_read_input_tokens":18833,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":10485},"output_tokens":4,"service_tier":"standard","inference_geo":"not_available"},"diagnostics":null,"context_management":null},"parent_tool_use_id":null,"session_id":"7f98a57c-818a-4214-a869-ac5af20aa972","uuid":"4ac7fa4a-78e8-4e16-b823-9458d01ffefe","timestamp":"2026-08-09T15:13:42.317Z","request_id":"req_011CdsPgaFmfPgeDWyLCJiB5"}"#;

    // REAL, verbatim: the limit was NOT biting (status "allowed").
    const CLAUDE_RATE_ALLOWED: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1786302000,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false},"uuid":"4ea8d06f-6f70-4984-85ee-1b6dc6d09c2d","session_id":"b7e8023e-530b-47ac-8515-a4e181bb7d58"}"#;

    // SYNTHETIC: the real line above with status flipped to "rejected" — a
    // biting limit was not reproducible on demand against a live account.
    const CLAUDE_RATE_REJECTED: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1786302000,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","isUsingOverage":false},"uuid":"4ea8d06f-6f70-4984-85ee-1b6dc6d09c2d","session_id":"b7e8023e-530b-47ac-8515-a4e181bb7d58"}"#;

    // REAL, verbatim: tool result riding a `user` line.
    const CLAUDE_TOOL_RESULT: &str = r#"{"type":"user","message":{"role":"user","content":[{"tool_use_id":"toolu_01WmCNWySEGTnX3KbsPuXNNV","type":"tool_result","content":"amux-fixture","is_error":false}]},"parent_tool_use_id":null,"session_id":"7f98a57c-818a-4214-a869-ac5af20aa972","uuid":"d911babb-a244-4d64-ae57-e561b6c7aaac","timestamp":"2026-08-09T15:13:51.914Z","tool_use_result":{"stdout":"amux-fixture","stderr":"","interrupted":false,"isImage":false,"noOutputExpected":false}}"#;

    // REAL, verbatim. Note "type":"result" sits near the END of the object.
    pub(crate) const CLAUDE_RESULT: &str = r#"{"is_error":false,"duration_api_ms":7617,"num_turns":2,"stop_reason":"end_turn","session_id":"7f98a57c-818a-4214-a869-ac5af20aa972","total_cost_usd":0.269648,"usage":{"input_tokens":4,"cache_creation_input_tokens":10650,"cache_read_input_tokens":48151,"output_tokens":157,"server_tool_use":{"web_search_requests":0,"web_fetch_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":10650,"ephemeral_5m_input_tokens":0},"inference_geo":"not_available","iterations":[{"input_tokens":2,"output_tokens":3,"cache_read_input_tokens":29318,"cache_creation_input_tokens":165,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":165},"type":"message"}],"speed":"standard"},"modelUsage":{},"permission_denials":[],"terminal_reason":"completed","fast_mode_state":"off","fast_mode_disabled_reason":"sdk_opt_in_required","subtype":"success","api_error_status":null,"result":"done","ttft_ms":2677,"ttft_stream_ms":1248,"time_to_request_ms":71,"type":"result","duration_ms":14478,"uuid":"b526a3b7-4e59-4670-bfdb-876113b7a4d6"}"#;
    // ^ REAL line with the modelUsage object emptied for readability — the
    //   translator reads only type/subtype/is_error/result.

    // SYNTHETIC: error-shaped result per --output-format json docs
    // (subtypes error_max_turns / error_during_execution); a real error
    // result was not reproducible on demand.
    const CLAUDE_RESULT_ERROR: &str = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"API error","session_id":"7f98a57c-818a-4214-a869-ac5af20aa972"}"#;

    #[test]
    fn claude_init_is_started() {
        assert_eq!(translate_claude(CLAUDE_INIT, &turn()), vec![WorkerEvent::Started]);
    }

    #[test]
    fn claude_thinking_tokens_is_progress() {
        let evs = translate_claude(CLAUDE_THINKING, &turn());
        assert_eq!(
            evs,
            vec![WorkerEvent::Progress(ProgressReport {
                summary: "thinking".into(),
                tokens_used: Some(50),
            })]
        );
    }

    #[test]
    fn claude_assistant_text_is_progress() {
        let evs = translate_claude(CLAUDE_TEXT, &turn());
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            WorkerEvent::Progress(p) => {
                assert!(p.summary.starts_with("Hi. Ready"), "{}", p.summary)
            }
            other => panic!("expected Progress, got {other:?}"),
        }
    }

    #[test]
    fn claude_tool_use_is_tool_event() {
        let evs = translate_claude(CLAUDE_TOOL_USE, &turn());
        assert_eq!(
            evs,
            vec![WorkerEvent::ToolUsed(ToolEvent {
                tool: "Bash".into(),
                detail: Some("echo amux-fixture".into()),
            })]
        );
    }

    #[test]
    fn claude_rate_limit_allowed_is_silent() {
        // "allowed" means the limit is NOT biting — an event here would be
        // the false-positive class of noise the ethos warns about.
        assert!(translate_claude(CLAUDE_RATE_ALLOWED, &turn()).is_empty());
    }

    #[test]
    fn claude_rate_limit_rejected_is_rate_limited() {
        let evs = translate_claude(CLAUDE_RATE_REJECTED, &turn());
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            WorkerEvent::RateLimited(rl) => {
                // "five_hour" has no honest enum cell -> Unknown, raw kept.
                assert_eq!(rl.kind, RateLimitKind::Unknown);
                assert_eq!(rl.provider, ProviderId::new("claude-code"));
                assert!(rl.reset_at.is_some());
                assert!(rl.raw.as_deref().unwrap().contains("five_hour"));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn claude_tool_result_goes_to_logs_not_events() {
        assert!(translate_claude(CLAUDE_TOOL_RESULT, &turn()).is_empty());
    }

    #[test]
    fn claude_success_result_is_turn_completed() {
        let evs = translate_claude(CLAUDE_RESULT, &turn());
        assert_eq!(
            evs,
            vec![WorkerEvent::TurnCompleted(TurnResult {
                turn_id: turn(),
                outcome: "success: done".into(),
            })]
        );
    }

    #[test]
    fn claude_error_result_is_failed_then_turn_completed() {
        let evs = translate_claude(CLAUDE_RESULT_ERROR, &turn());
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], WorkerEvent::Failed(f) if f.reason.contains("error_during_execution")));
        assert!(matches!(&evs[1], WorkerEvent::TurnCompleted(r) if r.outcome == "error_during_execution"));
    }

    // -- Codex CLI 0.141.0 (REAL captures) ----------------------------------

    // REAL, verbatim: `codex exec --json --skip-git-repo-check "say hi"`.
    pub(crate) const CODEX_THREAD_STARTED: &str =
        r#"{"type":"thread.started","thread_id":"019fe715-0c25-72c3-878b-7728e09afc96"}"#;
    pub(crate) const CODEX_TURN_STARTED: &str = r#"{"type":"turn.started"}"#;
    pub(crate) const CODEX_AGENT_MESSAGE: &str =
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"Hi!"}}"#;
    pub(crate) const CODEX_TURN_COMPLETED: &str = r#"{"type":"turn.completed","usage":{"input_tokens":13836,"cached_input_tokens":3456,"output_tokens":26,"reasoning_output_tokens":18}}"#;

    // REAL, verbatim: this machine's Codex quota was exhausted, so the
    // usage-limit path was captured live — structured rate-limit signal the
    // RR-0028e spike recorded as terminal-only.
    const CODEX_ERROR_LIMIT: &str = r#"{"type":"error","message":"You've hit your usage limit. To continue using Codex and get access to GPT-5.3-Codex, start a free trial of Plus today (https://chatgpt.com/explore/plus), or try again at Aug 27th, 2026 8:00 PM."}"#;
    const CODEX_TURN_FAILED: &str = r#"{"type":"turn.failed","error":{"message":"You've hit your usage limit. To continue using Codex and get access to GPT-5.3-Codex, start a free trial of Plus today (https://chatgpt.com/explore/plus), or try again at Aug 27th, 2026 8:00 PM."}}"#;

    // SYNTHETIC: command_execution item per Codex exec --json docs — the
    // live tool-use run was blocked by the usage limit above.
    const CODEX_COMMAND_EXECUTION: &str = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"echo amux-fixture","aggregated_output":"amux-fixture\n","exit_code":0,"status":"completed"}}"#;

    #[test]
    fn codex_lifecycle_events() {
        assert_eq!(
            translate_codex(CODEX_THREAD_STARTED, &turn()),
            vec![WorkerEvent::Started]
        );
        assert_eq!(
            translate_codex(CODEX_TURN_STARTED, &turn()),
            vec![WorkerEvent::TurnStarted { turn_id: turn() }]
        );
        assert_eq!(
            translate_codex(CODEX_TURN_COMPLETED, &turn()),
            vec![WorkerEvent::TurnCompleted(TurnResult {
                turn_id: turn(),
                outcome: "completed".into(),
            })]
        );
    }

    #[test]
    fn codex_agent_message_is_progress() {
        assert_eq!(
            translate_codex(CODEX_AGENT_MESSAGE, &turn()),
            vec![WorkerEvent::Progress(ProgressReport {
                summary: "Hi!".into(),
                tokens_used: None,
            })]
        );
    }

    #[test]
    fn codex_usage_limit_error_is_rate_limited() {
        let evs = translate_codex(CODEX_ERROR_LIMIT, &turn());
        assert_eq!(evs.len(), 1);
        match &evs[0] {
            WorkerEvent::RateLimited(rl) => {
                assert_eq!(rl.kind, RateLimitKind::Unknown);
                assert_eq!(rl.provider, ProviderId::new("codex"));
                assert!(rl.raw.as_deref().unwrap().contains("usage limit"));
                // The reset is an English date; we refuse to guess-parse it.
                assert!(rl.reset_at.is_none());
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn codex_turn_failed_on_limit_closes_turn_with_rate_limited() {
        let evs = translate_codex(CODEX_TURN_FAILED, &turn());
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], WorkerEvent::RateLimited(_)));
        assert!(matches!(&evs[1], WorkerEvent::TurnCompleted(r) if r.outcome.starts_with("failed:")));
    }

    #[test]
    fn codex_turn_failed_non_limit_is_failed() {
        // SYNTHETIC: non-limit failure message.
        let line = r#"{"type":"turn.failed","error":{"message":"model stream disconnected"}}"#;
        let evs = translate_codex(line, &turn());
        assert!(matches!(&evs[0], WorkerEvent::Failed(f) if f.reason.contains("disconnected")));
        assert!(matches!(&evs[1], WorkerEvent::TurnCompleted(_)));
    }

    #[test]
    fn codex_command_execution_is_tool_used() {
        assert_eq!(
            translate_codex(CODEX_COMMAND_EXECUTION, &turn()),
            vec![WorkerEvent::ToolUsed(ToolEvent {
                tool: "shell".into(),
                detail: Some("echo amux-fixture".into()),
            })]
        );
    }

    // -- Gemini CLI 0.53.1 (REAL captures) ----------------------------------

    // REAL, verbatim: `GEMINI_CLI_TRUST_WORKSPACE=true gemini -p "Run the
    // shell command: echo amux-fixture" -o stream-json --approval-mode yolo`.
    pub(crate) const GEMINI_INIT: &str = r#"{"type":"init","timestamp":"2026-08-09T15:23:21.036Z","session_id":"d1f03c4d-470b-4894-a540-3fd31e12be65","model":"auto"}"#;
    const GEMINI_USER_MSG: &str = r#"{"type":"message","timestamp":"2026-08-09T15:23:21.037Z","role":"user","content":"Run the shell command: echo amux-fixture"}"#;
    pub(crate) const GEMINI_ASSISTANT_DELTA: &str = r#"{"type":"message","timestamp":"2026-08-09T15:23:28.202Z","role":"assistant","content":"amux-fixture","delta":true}"#;
    const GEMINI_TOOL_USE: &str = r#"{"type":"tool_use","timestamp":"2026-08-09T15:23:26.508Z","tool_name":"run_shell_command","tool_id":"run_shell_command__45c957ff","parameters":{"description":"Runs a simple echo command to output the string 'amux-fixture'","command":"echo amux-fixture"}}"#;
    const GEMINI_TOOL_RESULT: &str = r#"{"type":"tool_result","timestamp":"2026-08-09T15:23:26.557Z","tool_id":"run_shell_command__45c957ff","status":"success","output":"amux-fixture"}"#;
    pub(crate) const GEMINI_RESULT: &str = r#"{"type":"result","timestamp":"2026-08-09T15:23:28.211Z","status":"success","stats":{"total_tokens":23632,"input_tokens":22915,"output_tokens":74,"cached":8115,"input":14800,"duration_ms":7175,"tool_calls":1,"models":{"gemini-3.1-flash-lite":{"total_tokens":1066,"input_tokens":963,"output_tokens":28,"cached":0,"input":963},"gemini-3.5-flash":{"total_tokens":22566,"input_tokens":21952,"output_tokens":46,"cached":8115,"input":13837}}}}"#;

    #[test]
    fn gemini_init_is_started() {
        assert_eq!(translate_gemini(GEMINI_INIT, &turn()), vec![WorkerEvent::Started]);
    }

    #[test]
    fn gemini_user_echo_is_silent() {
        assert!(translate_gemini(GEMINI_USER_MSG, &turn()).is_empty());
    }

    #[test]
    fn gemini_assistant_delta_is_progress() {
        assert_eq!(
            translate_gemini(GEMINI_ASSISTANT_DELTA, &turn()),
            vec![WorkerEvent::Progress(ProgressReport {
                summary: "amux-fixture".into(),
                tokens_used: None,
            })]
        );
    }

    #[test]
    fn gemini_tool_use_is_tool_event() {
        // `parameters.command` outranks `description` in the detail.
        assert_eq!(
            translate_gemini(GEMINI_TOOL_USE, &turn()),
            vec![WorkerEvent::ToolUsed(ToolEvent {
                tool: "run_shell_command".into(),
                detail: Some("echo amux-fixture".into()),
            })]
        );
    }

    #[test]
    fn gemini_tool_result_goes_to_logs_not_events() {
        assert!(translate_gemini(GEMINI_TOOL_RESULT, &turn()).is_empty());
    }

    #[test]
    fn gemini_success_result_is_tokens_then_turn_completed() {
        let evs = translate_gemini(GEMINI_RESULT, &turn());
        assert_eq!(evs.len(), 2);
        assert!(
            matches!(&evs[0], WorkerEvent::Progress(p) if p.tokens_used == Some(23632)),
            "{evs:?}"
        );
        assert!(matches!(&evs[1], WorkerEvent::TurnCompleted(r) if r.outcome == "success"));
    }

    #[test]
    fn gemini_error_result_is_failed_then_turn_completed() {
        // SYNTHETIC: status flipped on the real result line.
        let line = r#"{"type":"result","timestamp":"2026-08-09T15:23:28.211Z","status":"error","stats":{"total_tokens":0}}"#;
        let evs = translate_gemini(line, &turn());
        assert!(matches!(&evs[0], WorkerEvent::Failed(_)));
        assert!(matches!(&evs[1], WorkerEvent::TurnCompleted(r) if r.outcome == "error"));
    }

    // -- malformed input: no events, no panic (all providers) ---------------

    #[test]
    fn malformed_lines_produce_no_events_and_no_panic() {
        let cases = [
            "",
            "not json at all",
            "{truncated",
            "42",
            r#""a bare string""#,
            r#"{"no_type_field":true}"#,
            r#"{"type":"something_new_from_a_future_cli","payload":{}}"#,
            r#"{"type":"assistant"}"#,          // missing message.content
            r#"{"type":"item.completed"}"#,     // missing item
            r#"{"type":"rate_limit_event"}"#,   // missing rate_limit_info
        ];
        let t = turn();
        for line in cases {
            assert!(translate_claude(line, &t).is_empty(), "claude: {line}");
            assert!(translate_codex(line, &t).is_empty(), "codex: {line}");
            assert!(translate_gemini(line, &t).is_empty(), "gemini: {line}");
        }
    }

    // -- Codex ROLLOUT -> uniform transcript (REAL captures) ----------------
    //
    // Lines lifted verbatim from real rollout files under
    // ~/.codex/sessions on this machine (2026-05-02 and 2025-11-02 sessions),
    // except the session_meta and function_call_output, whose huge git_info /
    // output blobs are shortened for readability; the parser reads only the
    // fields exercised below.

    // REAL shape, shortened: only `cwd` is read from session_meta.
    const RO_META: &str = r#"{"timestamp":"2026-05-02T21:41:12.719Z","type":"session_meta","payload":{"id":"019deaa2-8e13-7843-9f9c-dbb54b8cd961","cwd":"/Users/ethan/Dev/mixpeek/server/mvs","cli_version":"0.58.0","model_provider":"openai"}}"#;
    // REAL, verbatim: real user prompt with a leading "[05:41 PM]" time tag.
    const RO_USER: &str = r#"{"timestamp":"2026-05-02T21:41:12.719Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"[05:41 PM] take a look at the progress of mvs, where we're at and whats remaining for pre-production, then come up with a plan to get us there"}]}}"#;
    // SYNTHETIC (shape is REAL): the opening environment_context user message,
    // pure harness chrome, must render to nothing.
    const RO_ENV: &str = r#"{"timestamp":"2026-05-02T21:41:12.700Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>/Users/ethan/Dev/mixpeek/server/mvs</cwd>\n  <shell>bash</shell>\n</environment_context>"}]}}"#;
    // REAL, verbatim: reasoning WITH plaintext summary (older build).
    const RO_REASONING: &str = r#"{"timestamp":"2025-11-02T19:15:22.395Z","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"**Checking RESOURCES.md file**"}],"content":null,"encrypted_content":"gAAAAABpB63I"}}"#;
    // REAL, verbatim: reasoning with NO plaintext (newer, encrypted build):
    // must produce no event, not an empty one.
    const RO_REASONING_ENC: &str = r#"{"timestamp":"2026-05-02T21:41:18.194Z","type":"response_item","payload":{"type":"reasoning","summary":[],"content":null,"encrypted_content":"gAAAAABp9m9-bKAY"}}"#;
    // REAL, verbatim: an exec tool call...
    const RO_EXEC: &str = r#"{"timestamp":"2026-05-02T21:41:21.666Z","type":"response_item","payload":{"type":"function_call","name":"exec_command","arguments":"{\"cmd\":\"git status --short --branch\",\"workdir\":\"/Users/ethan/Dev/mixpeek/server/mvs\",\"yield_time_ms\":1000,\"max_output_tokens\":4000}","call_id":"call_oOWSsoiERNccts8M61EX61l6"}}"#;
    // REAL shape, shortened: ...and its output, carrying Codex's metadata
    // preamble that clean_tool_output must strip.
    const RO_EXEC_OUT: &str = r#"{"timestamp":"2026-05-02T21:41:22.036Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_oOWSsoiERNccts8M61EX61l6","output":"Chunk ID: c15eb7\nWall time: 0.2054 seconds\nProcess exited with code 0\nOriginal token count: 2358\nOutput:\n## main...origin/main\n D scenes.json"}}"#;
    // REAL, verbatim: the update_plan call ("Updated Plan").
    const RO_PLAN: &str = r#"{"timestamp":"2025-11-02T19:15:54.780Z","type":"response_item","payload":{"type":"function_call","name":"update_plan","arguments":"{\"plan\":[{\"status\":\"in_progress\",\"step\":\"Adjust retriever sections in RESOURCES.md\"},{\"status\":\"pending\",\"step\":\"Document Ray submission ID metadata\"}]}","call_id":"call_ieMDLIsT8CykfDSDQwtCGTZ2"}}"#;
    // REAL, verbatim: an assistant message.
    const RO_ASSISTANT: &str = r#"{"timestamp":"2026-05-02T21:41:21.661Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"I’ll first map the repo structure, current git state, docs, and test surface."}],"phase":"commentary"}}"#;
    // REAL, verbatim: the event_msg mirror of the assistant text; MUST be
    // skipped (dedup), or every message renders twice.
    const RO_AGENT_MSG_MIRROR: &str = r#"{"timestamp":"2026-05-02T21:41:21.660Z","type":"event_msg","payload":{"type":"agent_message","message":"I’ll first map the repo structure, current git state, docs, and test surface.","phase":"commentary"}}"#;

    fn rollout(lines: &[&str]) -> Vec<Value> {
        lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn codex_rollout_renders_claude_like_turns() {
        let lines = rollout(&[
            RO_META,
            RO_ENV,
            RO_USER,
            RO_REASONING,
            RO_ASSISTANT,
            RO_AGENT_MSG_MIRROR,
            RO_EXEC,
            RO_EXEC_OUT,
            RO_PLAN,
        ]);
        let evs = codex_rollout_transcript(&lines);

        // Structure, in order: the env-context message and the agent_message
        // mirror both vanish, so the reader sees exactly one of each real turn.
        assert_eq!(evs.len(), 5, "unexpected shape: {evs:#?}");

        match &evs[0] {
            TranscriptEvent::User { text } => {
                // leading "[05:41 PM]" time tag stripped.
                assert!(text.starts_with("take a look at the progress"), "{text}");
            }
            other => panic!("expected User first, got {other:?}"),
        }
        assert!(matches!(&evs[1], TranscriptEvent::Reasoning { text } if text.contains("Checking RESOURCES")));
        assert!(matches!(&evs[2], TranscriptEvent::Assistant { text } if text.starts_with("I’ll first map")));
        match &evs[3] {
            TranscriptEvent::Tool { tool, detail, output } => {
                assert_eq!(tool, "exec_command");
                assert_eq!(detail.as_deref(), Some("git status --short --branch"));
                // metadata preamble stripped; output correlated by call_id.
                let o = output.as_deref().unwrap();
                assert!(o.starts_with("## main...origin/main"), "{o}");
                assert!(!o.contains("Wall time"), "preamble leaked: {o}");
            }
            other => panic!("expected Tool, got {other:?}"),
        }
        match &evs[4] {
            TranscriptEvent::Plan { steps } => {
                assert_eq!(steps.len(), 2);
                assert_eq!(steps[0].status, "in_progress");
                assert!(steps[0].step.starts_with("Adjust retriever"));
                assert_eq!(steps[1].status, "pending");
            }
            other => panic!("expected Plan, got {other:?}"),
        }
    }

    #[test]
    fn codex_rollout_encrypted_reasoning_is_absent_not_empty() {
        let evs = codex_rollout_transcript(&rollout(&[RO_REASONING_ENC, RO_ASSISTANT]));
        // No reasoning block at all when the build encrypted it.
        assert_eq!(evs.len(), 1);
        assert!(matches!(&evs[0], TranscriptEvent::Assistant { .. }));
    }

    #[test]
    fn codex_rollout_malformed_and_empty_are_safe() {
        assert!(codex_rollout_transcript(&[]).is_empty());
        // A lone tool OUTPUT with no matching call yields nothing (it is
        // consumed in pass 1 and never re-emitted).
        assert!(codex_rollout_transcript(&rollout(&[RO_EXEC_OUT])).is_empty());
        // Unknown record/payload types are ignored, not fatal.
        let unknown = rollout(&[
            r#"{"type":"turn_context","payload":{}}"#,
            r#"{"type":"response_item","payload":{"type":"world_state_from_a_future_cli"}}"#,
        ]);
        assert!(codex_rollout_transcript(&unknown).is_empty());
    }
}
