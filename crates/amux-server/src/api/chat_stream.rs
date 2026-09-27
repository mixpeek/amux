//! Chat streaming: provider stdout lines become ordered, resumable events.
//!
//! Three pieces, all pure so they can be tested without a provider:
//!
//! - [`Parser`] turns one provider stdout line (Claude `stream-json`, Codex
//!   `exec --json`) into zero or more chat events: text deltas, thinking,
//!   tool start / streamed args / final input / result, usage, rate-limit
//!   state, errors.
//! - [`Assembly`] folds those events into the state of one turn. The server
//!   keeps one for the in-flight snapshot (what a client that connects
//!   mid-turn is handed) and one per turn for the persisted message, so the
//!   finished message is built by the same code that built the stream and
//!   cannot disagree with it. `app.js` `_chatApply` mirrors `apply`.
//! - [`Journal`] stamps every event with a per-lane `seq` and the lane's
//!   `epoch`, keeps a bounded ring of recent events, and answers "replay
//!   everything after seq N" for a reconnecting client. A client that asks
//!   for something the ring no longer holds (evicted, or a previous server
//!   process) is told `gap` with the reason and refetches history, which is
//!   durable; it never silently misses events.
//!
//! Bounds (no silent caps): the ring holds at most `RING_MAX_EVENTS` events
//! and `RING_MAX_BYTES` bytes; a tool result is kept to `TOOL_RESULT_MAX`
//! bytes and streamed args to `TOOL_ARGS_MAX`, with the dropped byte count
//! carried on the event (`truncated`) so the UI can say so; thinking is kept
//! to `THINKING_MAX` with `thinking_truncated` counting the rest.

use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};

pub(crate) const RING_MAX_EVENTS: usize = 4096;
pub(crate) const RING_MAX_BYTES: usize = 4 << 20;
pub(crate) const TOOL_RESULT_MAX: usize = 8 * 1024;
pub(crate) const TOOL_ARGS_MAX: usize = 16 * 1024;
pub(crate) const THINKING_MAX: usize = 64 * 1024;

/// Longest prefix of `s` that is at most `max` bytes and ends on a char
/// boundary, plus how many bytes were dropped.
pub(crate) fn clip(s: &str, max: usize) -> (&str, usize) {
    if s.len() <= max {
        return (s, 0);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], s.len() - end)
}

/// Text of a tool_result `content`, which is a string or a list of blocks.
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|b| match b["type"].as_str() {
                Some("text") => b["text"].as_str().map(str::to_string),
                Some("image") => Some("[image]".to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn tool_result_event(id: &str, text: &str, is_error: bool) -> Value {
    let (kept, dropped) = clip(text, TOOL_RESULT_MAX);
    json!({"type": "tool_result", "id": id, "text": kept, "is_error": is_error, "truncated": dropped})
}

/// Provider-line parser for one turn. Holds the little state the formats need
/// (which content block index is which tool, whether text has started).
#[derive(Default)]
pub(crate) struct Parser {
    provider: String,
    /// Content block index -> tool_use id, for `input_json_delta`.
    tool_blocks: HashMap<i64, String>,
    /// Streamed args bytes per tool id, for the args bound.
    args_bytes: HashMap<String, usize>,
    /// Index of the text block last streamed, to separate text blocks.
    text_block: i64,
    text_started: bool,
    pub conversation_id: String,
    pub model: String,
}

impl Parser {
    pub(crate) fn new(provider: &str) -> Self {
        Parser {
            provider: provider.to_string(),
            text_block: -1,
            ..Default::default()
        }
    }

    fn text_delta(&mut self, t: &str, idx: Option<i64>) -> Option<Value> {
        if t.is_empty() {
            return None;
        }
        // Separate text blocks (before and after a tool call, or two codex
        // agent messages) with a blank line, the way a transcript shows them.
        let mut delta = String::new();
        let new_block = match idx {
            Some(i) => i != self.text_block,
            None => true,
        };
        if self.text_started && new_block {
            delta.push_str("\n\n");
        }
        if let Some(i) = idx {
            self.text_block = i;
        }
        self.text_started = true;
        delta.push_str(t);
        Some(json!({"type": "delta", "text": delta}))
    }

    /// Events for one stdout line. Unparseable or irrelevant lines yield none.
    pub(crate) fn line(&mut self, line: &str) -> Vec<Value> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![];
        };
        if self.provider == "codex" {
            self.codex(&v)
        } else {
            self.claude(&v)
        }
    }

    fn claude(&mut self, v: &Value) -> Vec<Value> {
        let mut evs = vec![];
        match v["type"].as_str().unwrap_or("") {
            "system" => match v["subtype"].as_str().unwrap_or("") {
                "init" => {
                    self.conversation_id = v["session_id"].as_str().unwrap_or("").to_string();
                    self.model = v["model"].as_str().unwrap_or("").to_string();
                    evs.push(json!({"type": "meta", "model": self.model}));
                }
                "status" => {
                    if let Some(s) = v["status"].as_str() {
                        evs.push(json!({"type": "phase", "phase": s}));
                    }
                }
                _ => {}
            },
            "stream_event" => {
                let ev = &v["event"];
                let idx = ev["index"].as_i64();
                match ev["type"].as_str().unwrap_or("") {
                    "message_start" => {
                        // New message: its block indexes restart at 0.
                        self.text_block = -1;
                        self.tool_blocks.clear();
                    }
                    "content_block_start" => {
                        let cb = &ev["content_block"];
                        if cb["type"] == "tool_use" {
                            let id = cb["id"].as_str().unwrap_or("").to_string();
                            if let Some(i) = idx {
                                self.tool_blocks.insert(i, id.clone());
                            }
                            evs.push(json!({"type": "tool_start", "id": id,
                                "name": cb["name"].as_str().unwrap_or("tool")}));
                        }
                    }
                    "content_block_delta" => {
                        let d = &ev["delta"];
                        match d["type"].as_str().unwrap_or("") {
                            "text_delta" => {
                                evs.extend(self.text_delta(d["text"].as_str().unwrap_or(""), idx));
                            }
                            "thinking_delta" => {
                                let t = d["thinking"].as_str().unwrap_or("");
                                if !t.is_empty() {
                                    evs.push(json!({"type": "thinking", "text": t}));
                                }
                            }
                            "input_json_delta" => {
                                let t = d["partial_json"].as_str().unwrap_or("");
                                let id = idx.and_then(|i| self.tool_blocks.get(&i)).cloned();
                                if let (Some(id), false) = (id, t.is_empty()) {
                                    let used = self.args_bytes.entry(id.clone()).or_default();
                                    let room = TOOL_ARGS_MAX.saturating_sub(*used);
                                    let (kept, dropped) = clip(t, room);
                                    *used += kept.len();
                                    if !kept.is_empty() || dropped > 0 {
                                        evs.push(json!({"type": "tool_args", "id": id,
                                            "text": kept, "truncated": dropped}));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            // The complete assistant message after each content block: the
            // authoritative tool input (the streamed args are a preview).
            "assistant" => {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks {
                        if b["type"] == "tool_use" {
                            let input = b["input"].to_string();
                            let (kept, dropped) = clip(&input, TOOL_ARGS_MAX);
                            evs.push(json!({"type": "tool_input",
                                "id": b["id"].as_str().unwrap_or(""),
                                "name": b["name"].as_str().unwrap_or("tool"),
                                "input": kept, "truncated": dropped}));
                        }
                    }
                }
            }
            "user" => {
                if let Some(blocks) = v["message"]["content"].as_array() {
                    for b in blocks {
                        if b["type"] == "tool_result" {
                            evs.push(tool_result_event(
                                b["tool_use_id"].as_str().unwrap_or(""),
                                &tool_result_text(&b["content"]),
                                b["is_error"].as_bool().unwrap_or(false),
                            ));
                        }
                    }
                }
            }
            "rate_limit_event" => {
                let i = &v["rate_limit_info"];
                evs.push(json!({"type": "limit",
                    "status": i["status"].as_str().unwrap_or(""),
                    "resets_at": i["resetsAt"],
                    "window": i["rateLimitType"],
                    "utilization": i["unifiedWindows"]}));
            }
            "result" => {
                if let Some(id) = v["session_id"].as_str() {
                    self.conversation_id = id.to_string();
                }
                let u = &v["usage"];
                evs.push(json!({"type": "usage",
                    "input_tokens": u["input_tokens"],
                    "output_tokens": u["output_tokens"],
                    "cache_read_input_tokens": u["cache_read_input_tokens"],
                    "cache_creation_input_tokens": u["cache_creation_input_tokens"],
                    "cost_usd": v["total_cost_usd"],
                    "duration_api_ms": v["duration_api_ms"],
                    "num_turns": v["num_turns"]}));
                if v["is_error"].as_bool() == Some(true) {
                    let text = v["result"].as_str().unwrap_or("provider reported an error");
                    evs.push(json!({"type": "error", "text": text}));
                } else if !self.text_started {
                    // Non-streaming provider build: the result carries the text.
                    evs.extend(self.text_delta(v["result"].as_str().unwrap_or(""), None));
                }
            }
            _ => {}
        }
        evs
    }

    fn codex(&mut self, v: &Value) -> Vec<Value> {
        let mut evs = vec![];
        let kind = v["type"].as_str().unwrap_or("");
        match kind {
            "thread.started" => {
                self.conversation_id = v["thread_id"].as_str().unwrap_or("").to_string();
            }
            "turn.started" => evs.push(json!({"type": "phase", "phase": "requesting"})),
            "item.started" | "item.updated" | "item.completed" => {
                let item = &v["item"];
                let id = item["id"].as_str().unwrap_or("").to_string();
                let done = kind == "item.completed";
                let started = kind == "item.started";
                match item["type"].as_str().unwrap_or("") {
                    "agent_message" if done => {
                        evs.extend(self.text_delta(item["text"].as_str().unwrap_or(""), None));
                    }
                    "reasoning" if done => {
                        let t = item["text"].as_str().unwrap_or("");
                        if !t.is_empty() {
                            evs.push(json!({"type": "thinking", "text": format!("{t}\n\n")}));
                        }
                    }
                    "command_execution" if started => {
                        evs.push(json!({"type": "tool_start", "id": id, "name": "shell"}));
                        let input = json!({"command": item["command"]}).to_string();
                        evs.push(json!({"type": "tool_input", "id": id, "name": "shell",
                            "input": input, "truncated": 0}));
                    }
                    "command_execution" if done => {
                        let code = item["exit_code"].as_i64();
                        evs.push(tool_result_event(
                            &id,
                            item["aggregated_output"].as_str().unwrap_or(""),
                            code.is_some_and(|c| c != 0) || item["status"] == "failed",
                        ));
                    }
                    "mcp_tool_call" | "web_search" if started => {
                        let name = if item["type"] == "web_search" {
                            "web_search".to_string()
                        } else {
                            format!(
                                "{}.{}",
                                item["server"].as_str().unwrap_or("mcp"),
                                item["tool"].as_str().unwrap_or("tool")
                            )
                        };
                        evs.push(json!({"type": "tool_start", "id": id, "name": name}));
                        let args = if item["type"] == "web_search" {
                            json!({"query": item["query"]})
                        } else {
                            item["arguments"].clone()
                        };
                        if !args.is_null() {
                            let s = args.to_string();
                            let (kept, dropped) = clip(&s, TOOL_ARGS_MAX);
                            evs.push(json!({"type": "tool_input", "id": id, "name": name,
                                "input": kept, "truncated": dropped}));
                        }
                    }
                    "mcp_tool_call" | "web_search" if done => {
                        let text = match &item["result"] {
                            Value::Null => item["error"]["message"].as_str().unwrap_or("").to_string(),
                            r => tool_result_text(&r["content"]).to_string(),
                        };
                        evs.push(tool_result_event(&id, &text, item["status"] == "failed"));
                    }
                    _ => {}
                }
            }
            "turn.completed" => {
                let u = &v["usage"];
                evs.push(json!({"type": "usage",
                    "input_tokens": u["input_tokens"],
                    "output_tokens": u["output_tokens"],
                    "cache_read_input_tokens": u["cached_input_tokens"]}));
            }
            "turn.failed" | "error" => {
                let msg = v["error"]["message"]
                    .as_str()
                    .or_else(|| v["message"].as_str())
                    .unwrap_or("codex turn failed");
                evs.push(json!({"type": "error", "text": msg}));
            }
            _ => {}
        }
        evs
    }
}

/// One tool call as the UI shows it.
#[derive(Clone, Debug, Default, serde::Serialize, PartialEq)]
pub(crate) struct ToolCall {
    pub id: String,
    pub name: String,
    /// Streamed argument JSON (possibly partial), then the final input.
    pub args: String,
    pub args_truncated: u64,
    pub result: Option<String>,
    pub result_truncated: u64,
    pub is_error: bool,
    pub done: bool,
}

/// The state of one turn, folded from its events. `apply` is the one
/// definition of what an event means; `app.js` `_chatApply` mirrors it.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct Assembly {
    pub turn_id: String,
    pub text: String,
    pub thinking: String,
    pub thinking_truncated: u64,
    pub tools: Vec<ToolCall>,
    pub usage: Value,
    pub limit: Value,
    pub error: Option<String>,
    pub phase: String,
    pub model: String,
}

impl Assembly {
    pub(crate) fn new(turn_id: &str) -> Self {
        Assembly {
            turn_id: turn_id.to_string(),
            ..Default::default()
        }
    }

    fn tool(&mut self, id: &str) -> Option<&mut ToolCall> {
        self.tools.iter_mut().rev().find(|t| t.id == id)
    }

    pub(crate) fn apply(&mut self, ev: &Value) {
        let s = |k: &str| ev[k].as_str().unwrap_or("").to_string();
        let n = |k: &str| ev[k].as_u64().unwrap_or(0);
        match ev["type"].as_str().unwrap_or("") {
            "delta" => {
                self.text.push_str(ev["text"].as_str().unwrap_or(""));
                self.phase = "writing".into();
            }
            "thinking" => {
                let t = ev["text"].as_str().unwrap_or("");
                let room = THINKING_MAX.saturating_sub(self.thinking.len());
                let (kept, dropped) = clip(t, room);
                self.thinking.push_str(kept);
                self.thinking_truncated += dropped as u64;
                self.phase = "thinking".into();
            }
            "tool_start" => {
                self.tools.push(ToolCall {
                    id: s("id"),
                    name: s("name"),
                    ..Default::default()
                });
                self.phase = "tool".into();
            }
            "tool_args" => {
                let (text, dropped) = (s("text"), n("truncated"));
                if let Some(t) = self.tool(ev["id"].as_str().unwrap_or("")) {
                    t.args.push_str(&text);
                    t.args_truncated += dropped;
                }
            }
            "tool_input" => {
                let (id, input, dropped) = (s("id"), s("input"), n("truncated"));
                match self.tool(&id) {
                    Some(t) => {
                        t.args = input;
                        t.args_truncated = dropped;
                    }
                    None => self.tools.push(ToolCall {
                        id,
                        name: s("name"),
                        args: input,
                        args_truncated: dropped,
                        ..Default::default()
                    }),
                }
            }
            "tool_result" => {
                let (id, text, dropped) = (s("id"), s("text"), n("truncated"));
                let is_error = ev["is_error"].as_bool().unwrap_or(false);
                if let Some(t) = self.tool(&id) {
                    t.result = Some(text);
                    t.result_truncated = dropped;
                    t.is_error = is_error;
                    t.done = true;
                }
                self.phase = "requesting".into();
            }
            "usage" => {
                let mut u = ev.clone();
                if let Some(m) = u.as_object_mut() {
                    m.remove("type");
                    m.remove("seq");
                    m.remove("epoch");
                    m.remove("turn_id");
                }
                self.usage = u;
            }
            "limit" => {
                let mut l = ev.clone();
                if let Some(m) = l.as_object_mut() {
                    m.remove("type");
                    m.remove("seq");
                    m.remove("epoch");
                    m.remove("turn_id");
                }
                self.limit = l;
            }
            "error" => self.error = Some(s("text")),
            "phase" => self.phase = s("phase"),
            "meta" => self.model = s("model"),
            _ => {}
        }
    }

    /// Whether the provider said this turn hit a usage limit.
    pub(crate) fn limited(&self) -> bool {
        matches!(self.limit["status"].as_str(), Some(s) if s != "allowed" && s != "allowed_warning")
    }
}

/// What a reconnecting client gets.
#[derive(Debug, PartialEq)]
pub(crate) enum Replay {
    /// Every event after the requested seq, in order (possibly none).
    Events(Vec<String>),
    /// The ring cannot serve the request; the client must refetch history.
    Gap(&'static str),
}

/// The per-lane ordered event log (see module doc).
pub(crate) struct Journal {
    pub epoch: String,
    pub seq: u64,
    ring: VecDeque<(u64, String)>,
    bytes: usize,
    pub evicted: u64,
    /// The in-flight turn, or `None` between turns.
    pub live: Option<Assembly>,
}

impl Journal {
    pub(crate) fn new(epoch: &str) -> Self {
        Journal {
            epoch: epoch.to_string(),
            seq: 0,
            ring: VecDeque::new(),
            bytes: 0,
            evicted: 0,
            live: None,
        }
    }

    /// Stamp, fold into the live turn, and retain one event. Returns the
    /// stamped event serialized, ready to broadcast. The caller holds the
    /// journal lock across this and the broadcast so seq order is send order.
    pub(crate) fn push(&mut self, mut ev: Value) -> String {
        self.seq += 1;
        ev["seq"] = json!(self.seq);
        ev["epoch"] = json!(self.epoch);
        match ev["type"].as_str().unwrap_or("") {
            "user" => {
                self.live = Some(Assembly::new(ev["turn_id"].as_str().unwrap_or("")));
            }
            "done" | "stopped" => self.live = None,
            _ => {
                if let Some(a) = self.live.as_mut() {
                    a.apply(&ev);
                }
            }
        }
        let s = ev.to_string();
        self.bytes += s.len();
        self.ring.push_back((self.seq, s.clone()));
        while self.ring.len() > RING_MAX_EVENTS || (self.bytes > RING_MAX_BYTES && self.ring.len() > 1) {
            if let Some((_, old)) = self.ring.pop_front() {
                self.bytes -= old.len();
                self.evicted += 1;
            }
        }
        s
    }

    pub(crate) fn replay_after(&self, epoch: &str, after: u64) -> Replay {
        if epoch != self.epoch {
            return Replay::Gap("epoch");
        }
        if after > self.seq {
            return Replay::Gap("ahead");
        }
        if after == self.seq {
            return Replay::Events(vec![]);
        }
        match self.ring.front() {
            Some((first, _)) if *first <= after + 1 => {}
            _ => return Replay::Gap("evicted"),
        }
        Replay::Events(
            self.ring
                .iter()
                .filter(|(s, _)| *s > after)
                .map(|(_, e)| e.clone())
                .collect(),
        )
    }

    pub(crate) fn retained(&self) -> (usize, usize) {
        (self.ring.len(), self.bytes)
    }
}

/// Parse `Last-Event-ID` / `?after=` in the `epoch:seq` form the stream emits.
pub(crate) fn parse_cursor(s: &str) -> Option<(String, u64)> {
    let (e, n) = s.trim().rsplit_once(':')?;
    Some((e.to_string(), n.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed from a real `claude -p --output-format stream-json --verbose
    /// --include-partial-messages` run on 2026-09-26 (haiku, one Bash call).
    const CLAUDE_FIXTURE: &str = include_str!("chat_stream_fixture.jsonl");

    fn run(provider: &str, lines: &str) -> (Parser, Assembly, Vec<Value>) {
        let mut p = Parser::new(provider);
        let mut a = Assembly::new("t1");
        let mut all = vec![];
        for l in lines.lines() {
            for e in p.line(l) {
                a.apply(&e);
                all.push(e);
            }
        }
        (p, a, all)
    }

    #[test]
    fn real_claude_stream_assembles_text_tool_and_usage() {
        let (p, a, evs) = run("claude", CLAUDE_FIXTURE);
        assert_eq!(p.conversation_id, "cf8e7fc4-f9d6-419c-acf6-5e8d5503916f");
        assert!(a.text.starts_with("The command ran successfully"), "{}", a.text);
        let deltas = evs.iter().filter(|e| e["type"] == "delta").count();
        assert!(deltas > 5, "text must arrive as many deltas, got {deltas}");
        assert_eq!(a.tools.len(), 1);
        let t = &a.tools[0];
        assert_eq!(t.name, "Bash");
        assert!(t.args.contains("echo hello-from-tool"), "{}", t.args);
        assert!(t.result.as_deref().unwrap().starts_with("hello-from-tool"));
        assert!(t.done && !t.is_error);
        assert_eq!(a.usage["output_tokens"], 214);
        assert_eq!(a.limit["status"], "allowed");
        assert!(!a.limited());
        assert!(a.error.is_none());
        // Tool args streamed as a preview before the authoritative input.
        let first_args = evs.iter().position(|e| e["type"] == "tool_args").unwrap();
        let input = evs.iter().position(|e| e["type"] == "tool_input").unwrap();
        assert!(first_args < input);
    }

    #[test]
    fn claude_text_blocks_are_separated_and_thinking_is_kept_apart() {
        let lines = [
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ponder"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"hi "}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"there"}}}"#,
            r#"{"type":"stream_event","event":{"type":"message_start"}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}}"#,
            r#"{"type":"result","is_error":false,"session_id":"s-1","total_cost_usd":0.01,"result":"done","usage":{}}"#,
        ]
        .join("\n");
        let (_, a, _) = run("claude", &lines);
        assert_eq!(a.text, "hi there\n\ndone");
        assert_eq!(a.thinking, "ponder");
        assert_eq!(a.usage["cost_usd"], 0.01);
    }

    #[test]
    fn claude_error_result_is_an_error_not_a_reply() {
        let (_, a, _) = run(
            "claude",
            r#"{"type":"result","is_error":true,"result":"No conversation found with session ID: x"}"#,
        );
        assert!(a.text.is_empty());
        assert!(a.error.unwrap().contains("No conversation found"));
    }

    #[test]
    fn rejected_rate_limit_marks_the_turn_limited() {
        let (_, a, _) = run(
            "claude",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790476200,"rateLimitType":"five_hour"}}"#,
        );
        assert!(a.limited());
        assert_eq!(a.limit["resets_at"], 1790476200);
    }

    #[test]
    fn codex_events_map_to_the_same_shapes() {
        let lines = [
            r#"{"type":"thread.started","thread_id":"th-9"}"#,
            r#"{"type":"item.completed","item":{"id":"r1","type":"reasoning","text":"plan"}}"#,
            r#"{"type":"item.started","item":{"id":"c1","type":"command_execution","command":"ls","status":"in_progress"}}"#,
            r#"{"type":"item.completed","item":{"id":"c1","type":"command_execution","command":"ls","aggregated_output":"a\nb","exit_code":0,"status":"completed"}}"#,
            r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"first"}}"#,
            r#"{"type":"item.completed","item":{"id":"m2","type":"agent_message","text":"second"}}"#,
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":2,"output_tokens":5}}"#,
        ]
        .join("\n");
        let (p, a, _) = run("codex", &lines);
        assert_eq!(p.conversation_id, "th-9");
        assert_eq!(a.text, "first\n\nsecond");
        assert_eq!(a.thinking, "plan\n\n");
        assert_eq!(a.tools.len(), 1);
        assert_eq!(a.tools[0].name, "shell");
        assert!(a.tools[0].args.contains("\"ls\""));
        assert_eq!(a.tools[0].result.as_deref(), Some("a\nb"));
        assert_eq!(a.usage["output_tokens"], 5);
    }

    #[test]
    fn oversized_tool_result_is_clipped_and_says_by_how_much() {
        let big = "é".repeat(TOOL_RESULT_MAX); // 2 bytes each
        let ev = tool_result_event("x", &big, false);
        let kept = ev["text"].as_str().unwrap();
        assert!(kept.len() <= TOOL_RESULT_MAX);
        assert_eq!(kept.len() as u64 + ev["truncated"].as_u64().unwrap(), big.len() as u64);
    }

    fn journal_with(n: usize) -> Journal {
        let mut j = Journal::new("E");
        j.push(json!({"type": "user", "turn_id": "t", "text": "q"}));
        for i in 0..n {
            j.push(json!({"type": "delta", "turn_id": "t", "text": format!("{i},")}));
        }
        j
    }

    #[test]
    fn journal_orders_events_and_folds_the_live_turn() {
        let j = journal_with(3);
        assert_eq!(j.seq, 4);
        assert_eq!(j.live.as_ref().unwrap().text, "0,1,2,");
        let Replay::Events(all) = j.replay_after("E", 0) else { panic!() };
        let seqs: Vec<u64> = all
            .iter()
            .map(|s| serde_json::from_str::<Value>(s).unwrap()["seq"].as_u64().unwrap())
            .collect();
        assert_eq!(seqs, vec![1, 2, 3, 4]);
    }

    #[test]
    fn resume_replays_exactly_the_missed_events_without_duplicates() {
        let mut j = journal_with(3);
        // A client saw everything through seq 2, then dropped.
        let Replay::Events(missed) = j.replay_after("E", 2) else { panic!() };
        assert_eq!(missed.len(), 2);
        // Rebuilding from the seq-2 state plus the replay equals the live turn.
        let mut a = Assembly::new("t");
        a.apply(&json!({"type": "delta", "text": "0,"}));
        for m in &missed {
            a.apply(&serde_json::from_str(m).unwrap());
        }
        assert_eq!(a.text, j.live.as_ref().unwrap().text);
        assert_eq!(j.replay_after("E", j.seq), Replay::Events(vec![]));
        j.push(json!({"type": "done", "turn_id": "t"}));
        assert!(j.live.is_none());
    }

    #[test]
    fn resume_across_a_restart_or_past_the_ring_is_a_gap_not_a_silent_skip() {
        let j = journal_with(RING_MAX_EVENTS + 10);
        assert_eq!(j.replay_after("OLD", 3), Replay::Gap("epoch"));
        assert_eq!(j.replay_after("E", 3), Replay::Gap("evicted"));
        assert_eq!(j.replay_after("E", j.seq + 5), Replay::Gap("ahead"));
        let (events, bytes) = j.retained();
        assert!(events <= RING_MAX_EVENTS && bytes <= RING_MAX_BYTES);
        assert_eq!(j.evicted as usize + events, RING_MAX_EVENTS + 11);
        // The oldest retained event is still servable.
        let oldest = j.seq - events as u64;
        assert!(matches!(j.replay_after("E", oldest), Replay::Events(v) if v.len() == events));
    }

    #[test]
    fn ring_is_bounded_by_bytes_too() {
        let mut j = Journal::new("E");
        let chunk = "x".repeat(64 * 1024);
        for _ in 0..200 {
            j.push(json!({"type": "delta", "text": chunk}));
        }
        let (_, bytes) = j.retained();
        assert!(bytes <= RING_MAX_BYTES, "{bytes}");
    }

    #[test]
    fn cursor_round_trips() {
        assert_eq!(parse_cursor("01ABC:42"), Some(("01ABC".into(), 42)));
        assert_eq!(parse_cursor("junk"), None);
    }
}
