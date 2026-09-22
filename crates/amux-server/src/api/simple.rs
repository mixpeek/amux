//! `GET /api/sessions/<n>/simple` — plain-English narratives of what a worker
//! did, for the peek **Simple** tab (Ethan 2026-08-13, AMUX-3056).
//!
//! SPAN, not just the last message (Ethan): each narrative synthesizes the work
//! from the worker's LAST HUMAN-sent message through to now — the request plus
//! everything the assistant did in response — into a DETAILED plain-English
//! account (not a two-sentence blurb). Generated via the same fastest/cheapest
//! helper as `/api/lookup` (`lookup::helper_answer`; `AMUX_HELPER_MODEL` wins).
//!
//! HISTORY: the tab keeps a LIST of generated narratives, each stamped with when
//! it was generated. `GET` returns the stored list (generating a first one if the
//! list is empty); `GET ?generate=1` forces a new narrative for the current span
//! and appends it. The standing prompt rides in via `?prompt=` (client-resolved
//! global default + per-worker override), else `DEFAULT_PROMPT`.

use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_PROMPT: &str = "Below is a person's request to an AI coding assistant, \
followed by everything the assistant did in response. Explain, in plain English for a \
non-technical person, what the assistant was asked to do and what it actually did: the \
steps it took, what it found or decided, and what changed as a result. Be thorough and \
specific — cover the whole span of work, not just the end — but use short sentences and \
common everyday words (no jargon, no code, no markdown). A few short paragraphs is good.";

/// How many chars of the assistant's work to feed the model (keep the TAIL —
/// most recent work is most relevant), plus a smaller cap on the human request.
const HUMAN_CAP: usize = 3_000;
const ASSISTANT_CAP: usize = 16_000;
/// Bound on the per-session history list.
const HISTORY_MAX: usize = 30;

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One generated narrative.
#[derive(Clone)]
struct SimpleEntry {
    generated_at: i64,
    summary: String,
    via: String,
    span_key: u64,
}
impl SimpleEntry {
    fn to_json(&self) -> Value {
        json!({
            "generated_at": self.generated_at,
            "summary": self.summary,
            "via": self.via,
        })
    }
}

/// session -> its history (oldest first). In-memory: cheap to regenerate.
fn cache() -> &'static Mutex<HashMap<String, Vec<SimpleEntry>>> {
    static C: OnceLock<Mutex<HashMap<String, Vec<SimpleEntry>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn history_json(name: &str) -> Vec<Value> {
    cache()
        .lock()
        .ok()
        .and_then(|c| {
            c.get(name)
                .map(|v| v.iter().rev().map(|e| e.to_json()).collect())
        })
        .unwrap_or_default()
}

fn newest_span_key(name: &str) -> Option<u64> {
    cache()
        .lock()
        .ok()
        .and_then(|c| c.get(name).and_then(|v| v.last().map(|e| e.span_key)))
}

fn push_entry(name: &str, entry: SimpleEntry) {
    if let Ok(mut c) = cache().lock() {
        let list = c.entry(name.to_string()).or_default();
        list.push(entry);
        let overflow = list.len().saturating_sub(HISTORY_MAX);
        if overflow > 0 {
            list.drain(0..overflow);
        }
    }
}

fn hash_key(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Claude Code `content` is either a bare string or an array of blocks; keep the
/// text blocks (tool_use / tool_result carry no prose worth summarizing).
fn extract_text(content: &Value) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for b in arr {
        if b["type"].as_str() == Some("text") {
            if let Some(t) = b["text"].as_str() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
    }
    out
}

/// The work span: the LAST human-sent message and everything the assistant did
/// after it, formatted for the synthesis prompt. Returns `None` when the worker
/// has no human prompt in its (tailed) transcript yet.
///
/// "Human message" = a `user` record with real TEXT — a tool_result is also role
/// `user` but carries no text, so `extract_text` empty filters it out, leaving
/// the actual prompt.
fn work_since_last_human(name: &str) -> Option<String> {
    let path = crate::api::session_verbs::session_jsonl_path(name)?;
    let f = std::fs::File::open(&path).ok()?;
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    // A generous tail: we want the whole span since the last prompt, not just
    // the last frame. Still bounded — a long lane's transcript is hundreds of MB.
    const TAIL: u64 = 1_500_000;
    let mut rdr = std::io::BufReader::new(f);
    if len > TAIL {
        use std::io::Seek;
        let _ = rdr.seek(std::io::SeekFrom::Start(len - TAIL));
    }
    use std::io::Read;
    let mut buf = String::new();
    let _ = rdr.read_to_string(&mut buf);
    let recs: Vec<Value> = buf
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .collect();

    let role_of = |v: &Value| -> String {
        v["message"]["role"]
            .as_str()
            .or_else(|| v["type"].as_str())
            .unwrap_or("")
            .to_string()
    };
    // Last human prompt index.
    let start = recs.iter().rposition(|v| {
        role_of(v) == "user" && !extract_text(&v["message"]["content"]).trim().is_empty()
    })?;

    let mut human = String::new();
    let mut assistant = String::new();
    for v in &recs[start..] {
        let text = extract_text(&v["message"]["content"]);
        if text.trim().is_empty() {
            continue;
        }
        match role_of(v).as_str() {
            "user" if human.is_empty() => human = text,
            "assistant" => {
                if !assistant.is_empty() {
                    assistant.push_str("\n\n");
                }
                assistant.push_str(&text);
            }
            _ => {}
        }
    }
    // Cap: keep the request head and the assistant TAIL (most recent work).
    let human: String = human.trim().chars().take(HUMAN_CAP).collect();
    let assistant = assistant.trim();
    let assistant: String = if assistant.chars().count() > ASSISTANT_CAP {
        let skip = assistant.chars().count() - ASSISTANT_CAP;
        format!("…{}", assistant.chars().skip(skip).collect::<String>())
    } else {
        assistant.to_string()
    };
    Some(format!(
        "THE PERSON ASKED:\n{human}\n\nWHAT THE ASSISTANT DID IN RESPONSE:\n{}",
        if assistant.is_empty() {
            "(the assistant has not responded in words yet)"
        } else {
            &assistant
        }
    ))
}

pub async fn simple_response(name: &str, prompt: Option<&str>, generate: bool) -> Response {
    // Not an explicit generate, and history exists -> just return it (instant).
    if !generate {
        let list = history_json(name);
        if !list.is_empty() {
            return Json(json!({ "entries": list })).into_response();
        }
        // Empty history: fall through and generate the first narrative.
    }
    let Some(span) = work_since_last_human(name) else {
        return Json(json!({
            "entries": history_json(name),
            "reason": "no human prompt in this worker's transcript yet",
        }))
        .into_response();
    };
    let prompt = prompt
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_PROMPT);
    let span_key = hash_key(&format!("{prompt}\u{0}{span}"));
    // On the AUTO path (empty-history first fill), don't duplicate a narrative
    // that already covers this exact span+prompt. The explicit generate button
    // always makes a fresh one — that is what "generate a new one" means.
    if !generate && newest_span_key(name) == Some(span_key) {
        return Json(json!({ "entries": history_json(name), "cached": true })).into_response();
    }
    let full = format!("{prompt}\n\n---\n{span}");
    match crate::api::lookup::helper_answer(&full).await {
        Ok((via, summary)) => {
            push_entry(
                name,
                SimpleEntry {
                    generated_at: now_unix(),
                    summary,
                    via,
                    span_key,
                },
            );
            Json(json!({ "entries": history_json(name), "cached": false })).into_response()
        }
        Err((code, msg)) => (
            code,
            Json(json!({ "entries": history_json(name), "error": msg })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_text_handles_string_and_block_array() {
        assert_eq!(extract_text(&json!("hi there")), "hi there");
        let blocks = json!([
            {"type":"text","text":"first"},
            {"type":"tool_use","name":"Bash"},
            {"type":"text","text":"second"}
        ]);
        assert_eq!(extract_text(&blocks), "first\nsecond");
        assert_eq!(
            extract_text(&json!([{"type":"tool_result","content":"x"}])),
            ""
        );
    }

    #[test]
    fn history_is_bounded_and_newest_first() {
        let s = "sess-bound-test";
        for i in 0..(HISTORY_MAX + 5) {
            push_entry(
                s,
                SimpleEntry {
                    generated_at: i as i64,
                    summary: format!("n{i}"),
                    via: "t".into(),
                    span_key: i as u64,
                },
            );
        }
        let list = history_json(s);
        assert_eq!(list.len(), HISTORY_MAX, "history is capped");
        // newest first
        assert_eq!(
            list[0]["generated_at"].as_i64(),
            Some((HISTORY_MAX + 4) as i64)
        );
        assert_eq!(newest_span_key(s), Some((HISTORY_MAX + 4) as u64));
    }
}
