//! `POST`/`GET /api/files/mdai/chat`: change a node by talking to it.
//!
//! The owner types an instruction about the document ("look at the messages
//! about lifecycle work and include that in the table"). The server hands the
//! model the node's file, the newest run's output and the instruction, and
//! asks for the revised instruction body (and, when needed, a revised source
//! list). The answer goes through the same validator and renderer the doc
//! editor uses (`doc::render_doc`), so a chat edit can never write a file the
//! engine would read differently, and it is written with the same atomic
//! write and on-disk change check as `PUT /doc`.
//!
//! The exchange is kept per node in a JSON Lines sidecar under
//! `~/.amux/mdai-chat/`, one line per turn, with the file text before and
//! after the edit so the page can show what changed and undo it. A sidecar was
//! chosen over a table so the history needs no migration; a rename carries it
//! to the new name the same way it carries run history.
//!
//! Running the node again is left to the existing run flow (`POST /run`): the
//! page starts it after an edit lands, so the output reflects the change.

use super::doc::{mtime_ms, parse_view, read_doc, render_doc, write_atomic};
use super::{
    canon_root, cap_str, classify_model_err, header_session, is_mdai, latest_run, mdai_root,
    rel_key, resolve_existing, resolve_model, sha256_hex, MdaiError, ModelClient,
};
use crate::api::AppState;
use crate::db::Store;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// How much of the newest run's output the model sees. The head is kept: for
/// a report that is the structure (headings, the table) the owner is talking
/// about.
pub(crate) const CHAT_OUTPUT_CAP: usize = 24 * 1024;
/// Longest instruction accepted from the chat box.
pub(crate) const MAX_MESSAGE_CHARS: usize = 4000;
/// Earlier turns quoted back to the model, so "now also add X" has context.
const PRIOR_TURNS_IN_PROMPT: usize = 6;
/// Most turns returned to the page.
const TURNS_RETURNED: usize = 100;
/// Per-turn cap on the stored before/after text. A file over this cannot be
/// undone from the chat (the turn says `truncated`).
const STORED_TEXT_CAP: usize = 128 * 1024;

#[derive(Debug)]
pub(crate) struct ChatError {
    pub status: StatusCode,
    pub msg: String,
    pub verdict: &'static str,
    /// The turn recorded for this failure, when one was (a model failure or an
    /// unusable answer is part of the conversation; a bad request is not).
    pub turn: Option<Value>,
}

impl ChatError {
    fn bad(msg: impl Into<String>) -> Self {
        ChatError { status: StatusCode::BAD_REQUEST, msg: msg.into(), verdict: "mdai_chat_bad_request", turn: None }
    }
}

impl IntoResponse for ChatError {
    fn into_response(self) -> Response {
        if self.status != StatusCode::BAD_REQUEST && self.status != StatusCode::NOT_FOUND {
            tracing::warn!(target: "amux::mdai", status = self.status.as_u16(), error = %self.msg,
                verdict = self.verdict, measured = true, n_considered = 1,
                "mdai chat edit did not apply");
        }
        (self.status, Json(json!({"error": self.msg, "verdict": self.verdict, "turn": self.turn}))).into_response()
    }
}

// ---------------------------------------------------------------------------
// Sidecar store
// ---------------------------------------------------------------------------

/// Where chat sidecars live in production.
fn chat_dir() -> PathBuf {
    crate::config::amux_home().join("mdai-chat")
}

/// The sidecar for one node, keyed by its root-relative path.
pub(crate) fn chat_file(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{}.jsonl", &sha256_hex(&[key])[..24]))
}

/// Read a node's turns, oldest first, keeping the newest `limit`. Returns the
/// turns and how many lines could not be read, so a damaged sidecar is
/// reported instead of silently shortening the conversation.
pub(crate) fn load_turns(dir: &Path, key: &str, limit: usize) -> (Vec<Value>, usize) {
    let text = match std::fs::read_to_string(chat_file(dir, key)) {
        Ok(t) => t,
        Err(_) => return (Vec::new(), 0),
    };
    let mut bad = 0;
    let mut turns: Vec<Value> = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Value>(line) {
            Ok(v) if v.get("path").and_then(|p| p.as_str()) == Some(key) => turns.push(v),
            Ok(_) => {}
            Err(_) => bad += 1,
        }
    }
    if turns.len() > limit {
        turns.drain(..turns.len() - limit);
    }
    (turns, bad)
}

fn append_turn(dir: &Path, key: &str, turn: &Value) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let file = chat_file(dir, key);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let mut line = serde_json::to_string(turn).map_err(|e| e.to_string())?;
    line.push('\n');
    f.write_all(line.as_bytes()).map_err(|e| format!("{}: {e}", file.display()))
}

/// Record a turn; a failure to record is logged, never fatal to the edit that
/// already landed.
fn record(dir: &Path, key: &str, turn: &Value) {
    if let Err(e) = append_turn(dir, key, turn) {
        tracing::warn!(target: "amux::mdai", path = %key, error = %e,
            verdict = "mdai_chat_history_not_saved", measured = true, n_considered = 1,
            "mdai chat turn applied but its history line could not be written");
    }
}

/// Carry a node's chat to its new name (called by the doc rename). Lines keep
/// their old `path`, so they are rewritten with the new one.
pub(crate) fn move_chat_in(dir: &Path, old_key: &str, new_key: &str) {
    let from = chat_file(dir, old_key);
    if !from.exists() {
        return;
    }
    let (turns, bad) = load_turns(dir, old_key, usize::MAX);
    let mut out = String::new();
    for mut t in turns {
        t["path"] = json!(new_key);
        out.push_str(&t.to_string());
        out.push('\n');
    }
    let to = chat_file(dir, new_key);
    let res = std::fs::write(&to, out).and_then(|_| std::fs::remove_file(&from));
    if let Err(e) = res {
        tracing::warn!(target: "amux::mdai", from = %old_key, to = %new_key, error = %e,
            verdict = "mdai_chat_history_not_moved", measured = true, n_considered = 1,
            "renamed a node but its chat history stayed under the old name");
    } else if bad > 0 {
        tracing::warn!(target: "amux::mdai", from = %old_key, to = %new_key, unreadable = bad,
            verdict = "mdai_chat_history_lines_dropped", measured = true, n_considered = bad,
            "renamed a node's chat history; unreadable lines were not carried");
    }
}

pub(crate) fn move_chat(old_key: &str, new_key: &str) {
    move_chat_in(&chat_dir(), old_key, new_key);
}

// ---------------------------------------------------------------------------
// Prompt and answer
// ---------------------------------------------------------------------------

/// The prompt for one chat edit. Pure, so a test can read exactly what the
/// model is shown.
pub(crate) fn build_chat_prompt(raw: &str, latest_output: Option<&str>, prior: &[Value], message: &str) -> String {
    let mut p = String::new();
    p.push_str(
        "You edit an .mdai file: a markdown node whose body is the instruction a model follows on every run, \
over the sources listed in its YAML frontmatter. The owner is reading the node's latest output and asks you to change \
the node so the NEXT run produces what they want. Change the instruction body (and the sources only if the request \
needs data the current sources do not select). Keep everything the request does not touch exactly as it is: \
headings, wording, order, formatting. Do not run the node and do not write its output.\n\n\
Source paths you may use: a file or folder relative to the node's folder, another .mdai node, or \
`amux:messages?<params>` (the owner's own messages across all workers). amux:messages params: \
days=<n>|all, limit=<newest n, max 4000>, offset=<skip newest n>, kb=<size budget, max 600>, \
q=<word|other word> (keep only messages containing any of these words, case-insensitive), \
session=<worker name> (keep one worker's messages). Each source has a short prompt saying how to use it.\n\n",
    );
    p.push_str("## The current file\n\n<file>\n");
    p.push_str(raw);
    if !raw.ends_with('\n') {
        p.push('\n');
    }
    p.push_str("</file>\n\n");
    match latest_output {
        Some(o) if !o.trim().is_empty() => {
            p.push_str("## The latest run's output (what the owner is looking at)\n\n<output>\n");
            p.push_str(&cap_str(o, CHAT_OUTPUT_CAP));
            p.push_str("\n</output>\n\n");
        }
        _ => p.push_str("## The latest run's output\n\n(The node has no successful run yet.)\n\n"),
    }
    let prior: Vec<&Value> = prior.iter().rev().take(PRIOR_TURNS_IN_PROMPT).collect::<Vec<_>>().into_iter().rev().collect();
    if !prior.is_empty() {
        p.push_str("## Earlier requests in this conversation (already applied unless marked failed)\n\n");
        for t in prior {
            let m = t.get("message").and_then(|v| v.as_str()).unwrap_or("");
            let r = t.get("reply").and_then(|v| v.as_str()).unwrap_or("");
            let failed = t.get("error").map(|e| !e.is_null()).unwrap_or(false);
            p.push_str(&format!("- Owner: {}\n  You: {}{}\n", one_line(m, 400), one_line(r, 400), if failed { " (failed)" } else { "" }));
        }
        p.push('\n');
    }
    p.push_str("## The owner's request\n\n<request>\n");
    p.push_str(message.trim());
    p.push_str("\n</request>\n\n");
    p.push_str(
        "## How to answer\n\n\
Answer with these tagged parts and nothing else:\n\n\
<reply>One or two plain sentences for the owner: what you changed and why, or the answer if the request is only a \
question.</reply>\n\
<body>\nThe COMPLETE revised instruction body (everything after the closing --- of the frontmatter). Omit this part \
entirely if the body should not change.\n</body>\n\
<sources>A JSON array of every source the node should have, as [{\"path\": \"...\", \"prompt\": \"...\"}], \
existing ones included. Omit this part entirely if the sources should not change.</sources>\n",
    );
    p
}

fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        flat.chars().take(max).collect::<String>() + "..."
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Proposal {
    pub reply: String,
    pub body: Option<String>,
    pub sources: Option<Vec<Value>>,
}

/// The text between `<tag>` and the LAST `</tag>` (a body may quote markup).
fn tagged<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text.rfind(&close)?;
    (end >= start).then(|| &text[start..end])
}

/// Read the model's tagged answer. An answer with neither a reply nor an edit
/// is refused: there is nothing honest to show the owner.
pub(crate) fn parse_proposal(text: &str) -> Result<Proposal, String> {
    let reply = tagged(text, "reply").map(|s| s.trim().to_string()).unwrap_or_default();
    let body = tagged(text, "body").map(|b| {
        let b = b.strip_prefix('\n').unwrap_or(b);
        let mut b = b.trim_end().to_string();
        b.push('\n');
        b
    });
    let sources = match tagged(text, "sources") {
        None => None,
        Some(raw) => {
            let raw = raw.trim();
            let raw = raw.strip_prefix("```json").or_else(|| raw.strip_prefix("```")).unwrap_or(raw);
            let raw = raw.strip_suffix("```").unwrap_or(raw).trim();
            let v: Value = serde_json::from_str(raw).map_err(|e| format!("the sources part is not a JSON array: {e}"))?;
            let arr = v.as_array().ok_or("the sources part is not a JSON array")?;
            Some(arr.iter().map(|s| match s {
                Value::String(p) => json!({"path": p, "prompt": ""}),
                other => json!({
                    "path": other.get("path").and_then(|x| x.as_str()).unwrap_or(""),
                    "prompt": other.get("prompt").and_then(|x| x.as_str()).unwrap_or(""),
                }),
            }).collect())
        }
    };
    if reply.is_empty() && body.is_none() && sources.is_none() {
        return Err("the model's answer had no <reply>, <body> or <sources> part".into());
    }
    Ok(Proposal { reply, body, sources })
}

// ---------------------------------------------------------------------------
// The edit
// ---------------------------------------------------------------------------

pub(crate) struct ChatRequest<'a> {
    pub path: &'a str,
    pub message: &'a str,
    pub base_mtime_ms: Option<i64>,
    pub session: Option<String>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn stored(text: &str) -> (String, bool) {
    if text.len() <= STORED_TEXT_CAP {
        (text.to_string(), false)
    } else {
        (cap_str(text, STORED_TEXT_CAP), true)
    }
}

/// Apply one chat instruction to a node. Everything a test needs is injected:
/// the files root, the sidecar folder, the store (for the latest output) and
/// the model.
pub(crate) fn chat_edit(
    store: &Store,
    root_canon: &Path,
    dir: &Path,
    model: &dyn ModelClient,
    req: ChatRequest,
) -> Result<Value, ChatError> {
    let message = req.message.trim();
    if message.is_empty() {
        return Err(ChatError::bad("Type what you want changed."));
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        return Err(ChatError::bad(format!("A message is at most {MAX_MESSAGE_CHARS} characters.")));
    }
    let file = match resolve_existing(root_canon, root_canon, req.path) {
        Ok(f) => f,
        Err(MdaiError::Escape(_)) => {
            return Err(ChatError::bad(format!(
                "This file is outside the MDAI folder ({}), so chat cannot change it.",
                root_canon.display()
            )))
        }
        Err(e) => return Err(ChatError { status: StatusCode::NOT_FOUND, msg: e.to_string(), verdict: "mdai_chat_not_found", turn: None }),
    };
    if !is_mdai(&file) {
        return Err(ChatError::bad("Chat edits .mdai files only."));
    }
    let key = rel_key(root_canon, &file);
    let before = std::fs::read_to_string(&file)
        .map_err(|e| ChatError { status: StatusCode::INTERNAL_SERVER_ERROR, msg: format!("{}: {e}", file.display()), verdict: "mdai_chat_read_failed", turn: None })?;
    let read_mtime = mtime_ms(&file);
    if let (Some(base), Some(cur)) = (req.base_mtime_ms, read_mtime) {
        if base != cur {
            return Err(ChatError { status: StatusCode::CONFLICT, verdict: "mdai_chat_stale",
                msg: "This node changed on disk since you opened it. Reload, then send your message again.".into(), turn: None });
        }
    }
    let view = parse_view(&before).map_err(|e| ChatError::bad(format!("This file does not parse ({e}). Fix it in Edit, Raw file first.")))?;
    let latest = latest_run(store, &key).map(|(_, out)| out);
    let (prior, _) = load_turns(dir, &key, PRIOR_TURNS_IN_PROMPT);
    let prompt = build_chat_prompt(&before, latest.as_deref(), &prior, message);
    let model_name = resolve_model(view.get("model").and_then(|m| m.as_str()));
    let ts = now_ms();
    let base_turn = json!({
        "id": ts, "ts": ts, "path": key, "message": message, "model": model_name,
        "session": req.session, "prompt_bytes": prompt.len(),
        "output_bytes_seen": latest.as_ref().map(|o| o.len().min(CHAT_OUTPUT_CAP)),
    });
    let fail = |status: StatusCode, verdict: &'static str, msg: String, dur_ms: i64| -> ChatError {
        let mut t = base_turn.clone();
        t["error"] = json!(msg);
        t["status"] = json!(verdict);
        t["changed"] = json!(false);
        t["dur_ms"] = json!(dur_ms);
        record(dir, &key, &t);
        ChatError { status, msg, verdict, turn: Some(t) }
    };
    let started = std::time::Instant::now();
    let answer = model.complete(&model_name, &prompt);
    let dur_ms = started.elapsed().as_millis() as i64;
    let answer = match answer {
        Ok(a) => a,
        Err(e) => {
            let err = classify_model_err(e);
            let status = match err {
                MdaiError::ModelTimeout(_) => StatusCode::GATEWAY_TIMEOUT,
                _ => StatusCode::BAD_GATEWAY,
            };
            return Err(fail(status, "mdai_chat_model_failed", format!("The model call failed: {err}"), dur_ms));
        }
    };
    let proposal = parse_proposal(&answer).map_err(|e| {
        fail(StatusCode::UNPROCESSABLE_ENTITY, "mdai_chat_answer_unusable",
            format!("The model's answer could not be used ({e}). Nothing was changed; try rephrasing."), dur_ms)
    })?;
    let mut doc = view.clone();
    if let Some(b) = &proposal.body {
        doc["body"] = json!(b);
    }
    if let Some(s) = &proposal.sources {
        doc["sources"] = json!(s);
    }
    let after = render_doc(&doc).map_err(|e| {
        fail(StatusCode::UNPROCESSABLE_ENTITY, "mdai_chat_edit_invalid",
            format!("The model proposed an edit the file cannot hold ({}: {}). Nothing was changed.", e.field, e.msg), dur_ms)
    })?;
    // Compare the parsed views, not the bytes: a file written by hand renders
    // in the canonical layout, and that alone is not an edit worth a write.
    let changed = parse_view(&after).ok() != Some(view.clone());
    if changed {
        // The model call can take a minute. A save that landed meanwhile wins:
        // this edit was made against the text it replaced.
        if mtime_ms(&file) != read_mtime {
            return Err(fail(StatusCode::CONFLICT, "mdai_chat_stale",
                "This node was saved while the model was working, so the edit was not written. Send your message again.".into(), dur_ms));
        }
        write_atomic(&file, &after).map_err(|e| {
            fail(StatusCode::INTERNAL_SERVER_ERROR, "mdai_chat_write_failed", e.msg, dur_ms)
        })?;
    }
    let (before_s, t1) = stored(&before);
    let (after_s, t2) = stored(if changed { &after } else { &before });
    let mut turn = base_turn.clone();
    turn["reply"] = json!(if proposal.reply.is_empty() { "Updated the node.".to_string() } else { proposal.reply.clone() });
    turn["changed"] = json!(changed);
    turn["body_changed"] = json!(changed && proposal.body.is_some() && doc["body"] != view["body"]);
    turn["sources_changed"] = json!(changed && proposal.sources.is_some() && doc["sources"] != view["sources"]);
    turn["before"] = json!(before_s);
    turn["after"] = json!(after_s);
    turn["truncated"] = json!(t1 || t2);
    turn["dur_ms"] = json!(dur_ms);
    turn["status"] = json!(if changed { "applied" } else { "no_change" });
    turn["error"] = Value::Null;
    record(dir, &key, &turn);
    tracing::info!(target: "amux::mdai", path = %key, changed, dur_ms, prompt_bytes = prompt.len(),
        verdict = if changed { "mdai_chat_applied" } else { "mdai_chat_no_change" },
        measured = true, n_considered = 1, "mdai chat instruction handled");
    let doc_now = read_doc(root_canon, &file).map_err(|e| ChatError {
        status: StatusCode::INTERNAL_SERVER_ERROR, msg: e.msg, verdict: "mdai_chat_read_failed", turn: Some(turn.clone()) })?;
    Ok(json!({"ok": true, "changed": changed, "turn": turn, "doc": doc_now}))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/files/mdai/chat {path, message, base_mtime_ms?}`
pub(crate) async fn chat_post(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let store = state.store.clone();
    let session = header_session(&headers);
    let res = crate::db::interactions::spawn_blocking(move || -> Result<Value, ChatError> {
        let root_canon = canon_root(&mdai_root()).map_err(|e| ChatError::bad(e.to_string()))?;
        let path = body.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let message = body.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if path.trim().is_empty() {
            return Err(ChatError::bad("path required"));
        }
        let model = super::best_model();
        chat_edit(&store, &root_canon, &chat_dir(), model.as_ref(), ChatRequest {
            path: &path,
            message: &message,
            base_mtime_ms: body.get("base_mtime_ms").and_then(|v| v.as_i64()),
            session,
        })
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => ChatError { status: StatusCode::INTERNAL_SERVER_ERROR, msg: e.to_string(), verdict: "mdai_chat_task_failed", turn: None }.into_response(),
    }
}

/// `GET /api/files/mdai/chat?path=` - the node's conversation, oldest first.
pub(crate) async fn chat_get(Query(q): Query<HashMap<String, String>>) -> Response {
    let path = q.get("path").cloned().unwrap_or_default();
    let res = crate::db::interactions::spawn_blocking(move || -> Result<Value, ChatError> {
        let root_canon = canon_root(&mdai_root()).map_err(|e| ChatError::bad(e.to_string()))?;
        let key = match resolve_existing(&root_canon, &root_canon, &path) {
            Ok(f) => rel_key(&root_canon, &f),
            Err(_) => path.trim().to_string(),
        };
        if key.is_empty() {
            return Err(ChatError::bad("path required"));
        }
        let (turns, bad) = load_turns(&chat_dir(), &key, TURNS_RETURNED);
        if bad > 0 {
            tracing::warn!(target: "amux::mdai", path = %key, unreadable = bad,
                verdict = "mdai_chat_history_damaged", measured = true, n_considered = turns.len() + bad,
                "mdai chat history has unreadable lines; they are left out");
        }
        Ok(json!({"path": key, "turns": turns, "unreadable": bad, "measured": true, "n_considered": turns.len() + bad}))
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => ChatError { status: StatusCode::INTERNAL_SERVER_ERROR, msg: e.to_string(), verdict: "mdai_chat_task_failed", turn: None }.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::WriteOutcome;
    use std::sync::Mutex;

    /// Returns a canned answer and keeps every prompt it was given.
    struct Scripted {
        answer: Result<String, String>,
        prompts: Mutex<Vec<String>>,
    }
    impl Scripted {
        fn ok(a: &str) -> Self {
            Scripted { answer: Ok(a.into()), prompts: Mutex::new(Vec::new()) }
        }
        fn err(e: &str) -> Self {
            Scripted { answer: Err(e.into()), prompts: Mutex::new(Vec::new()) }
        }
    }
    impl ModelClient for Scripted {
        fn complete(&self, _m: &str, prompt: &str) -> Result<String, String> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            self.answer.clone()
        }
    }

    const NODE: &str = "---\ntitle: Workstreams\nmodel: sonnet\nrefresh: manual\nsources:\n- path: amux:messages?days=all&limit=4000\n  prompt: My message history.\n---\n# Workstreams\n\nBuild a table of workstreams.\n";

    struct Fx {
        _dir: tempfile::TempDir,
        root: PathBuf,
        chat: PathBuf,
        store: Store,
    }
    fn fx() -> Fx {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("ws.mdai"), NODE).unwrap();
        let store = Store::open(&dir.path().join("t.db")).unwrap();
        let chat = dir.path().join("chat");
        Fx { _dir: dir, root, chat, store }
    }
    fn seed_run(store: &Store, path: &str, output: &str) {
        let (p, o) = (path.to_string(), output.to_string());
        store.write(move |c| {
            c.execute("INSERT INTO mdai_runs (path, ts, inputs_hash, output, model, cached, status) VALUES (?1, 1, 'h', ?2, 'm', 0, 'ok')",
                rusqlite::params![p, o])?;
            Ok(WriteOutcome { applied: true, events: vec![] })
        }).unwrap();
    }
    fn req<'a>(message: &'a str) -> ChatRequest<'a> {
        ChatRequest { path: "ws.mdai", message, base_mtime_ms: None, session: Some("tester".into()) }
    }

    #[test]
    fn an_instruction_rewrites_the_body_adds_a_source_and_is_recorded() {
        let f = fx();
        seed_run(&f.store, "ws.mdai", "| Workstream | State |\n|---|---|\n| Billing | done |\n");
        let model = Scripted::ok(
            "<reply>Added a Lifecycle column fed by a lifecycle message source.</reply>\n<body>\n# Workstreams\n\nBuild a table of workstreams. Add a Lifecycle column from the lifecycle messages.\n</body>\n<sources>[{\"path\": \"amux:messages?days=all&limit=4000\", \"prompt\": \"My message history.\"}, {\"path\": \"amux:messages?days=all&q=lifecycle\", \"prompt\": \"Messages about lifecycle work.\"}]</sources>",
        );
        let out = chat_edit(&f.store, &f.root, &f.chat, &model,
            req("look at the messages responsible for lifecycle work, and include that in the table")).unwrap();
        assert_eq!(out["changed"], true);
        let prompt = model.prompts.lock().unwrap()[0].clone();
        assert!(prompt.contains("Build a table of workstreams."), "the file reaches the model");
        assert!(prompt.contains("| Billing | done |"), "the latest output reaches the model");
        assert!(prompt.contains("lifecycle work, and include that in the table"), "the instruction reaches the model");
        let text = std::fs::read_to_string(f.root.join("ws.mdai")).unwrap();
        let engine = super::super::parse_mdai(&text).unwrap();
        assert_eq!(engine.sources.len(), 2);
        assert_eq!(engine.sources[1].path, "amux:messages?days=all&q=lifecycle");
        assert!(engine.body.contains("Lifecycle column"));
        assert_eq!(engine.model.as_deref(), Some("sonnet"), "fields the edit did not touch survive");
        assert!(text.contains("refresh: manual") && text.contains("title: Workstreams"));
        let turn = &out["turn"];
        assert_eq!(turn["before"], NODE);
        assert_eq!(turn["after"], text);
        assert_eq!(turn["body_changed"], true);
        assert_eq!(turn["sources_changed"], true);
        assert_eq!(out["doc"]["doc"]["sources"][1]["prompt"], "Messages about lifecycle work.");
        let (turns, bad) = load_turns(&f.chat, "ws.mdai", 10);
        assert_eq!((turns.len(), bad), (1, 0));
        assert_eq!(turns[0]["message"], "look at the messages responsible for lifecycle work, and include that in the table");
        assert_eq!(turns[0]["session"], "tester");

        // The next turn quotes the first back to the model.
        let m2 = Scripted::ok("<reply>It reads two sources.</reply>");
        let out2 = chat_edit(&f.store, &f.root, &f.chat, &m2, req("what does it read now?")).unwrap();
        assert_eq!(out2["changed"], false, "a question changes nothing");
        assert_eq!(std::fs::read_to_string(f.root.join("ws.mdai")).unwrap(), text, "and writes nothing");
        assert!(m2.prompts.lock().unwrap()[0].contains("Added a Lifecycle column"), "earlier turns are context");
        assert_eq!(load_turns(&f.chat, "ws.mdai", 10).0.len(), 2);
    }

    #[test]
    fn a_failed_or_unusable_answer_writes_nothing_and_says_why() {
        let f = fx();
        let e = chat_edit(&f.store, &f.root, &f.chat, &Scripted::err("claude exited 1: session limit"), req("x")).unwrap_err();
        assert_eq!(e.status, StatusCode::BAD_GATEWAY);
        assert!(e.msg.contains("session limit"));
        let e = chat_edit(&f.store, &f.root, &f.chat, &Scripted::err(&super::super::model_timeout_msg("claude", 240)), req("x")).unwrap_err();
        assert_eq!(e.status, StatusCode::GATEWAY_TIMEOUT);
        let e = chat_edit(&f.store, &f.root, &f.chat, &Scripted::ok("sure, done!"), req("x")).unwrap_err();
        assert_eq!((e.status, e.verdict), (StatusCode::UNPROCESSABLE_ENTITY, "mdai_chat_answer_unusable"));
        let e = chat_edit(&f.store, &f.root, &f.chat, &Scripted::ok("<reply>r</reply><sources>[{\"path\": \"amux:board\"}]</sources>"), req("x")).unwrap_err();
        assert_eq!(e.verdict, "mdai_chat_edit_invalid", "the doc validator refuses an unknown amux source");
        assert!(e.msg.contains("amux:board") || e.msg.contains("unknown amux source"), "{}", e.msg);
        let e = chat_edit(&f.store, &f.root, &f.chat, &Scripted::ok("<reply>r</reply><body>\n\n</body>"), req("x")).unwrap_err();
        assert_eq!(e.verdict, "mdai_chat_edit_invalid", "an empty body is refused");
        assert_eq!(std::fs::read_to_string(f.root.join("ws.mdai")).unwrap(), NODE, "the file is untouched");
        let (turns, _) = load_turns(&f.chat, "ws.mdai", 10);
        assert_eq!(turns.len(), 5, "every failed attempt is part of the conversation");
        assert!(turns.iter().all(|t| t["error"].is_string() && t["changed"] == false));
    }

    #[test]
    fn a_stale_base_or_a_bad_path_is_refused_before_any_model_call() {
        let f = fx();
        let m = Scripted::ok("<reply>r</reply>");
        let mut r = req("x");
        r.base_mtime_ms = Some(1);
        assert_eq!(chat_edit(&f.store, &f.root, &f.chat, &m, r).unwrap_err().status, StatusCode::CONFLICT);
        let mut r = req("x");
        r.path = "../outside.mdai";
        assert!(chat_edit(&f.store, &f.root, &f.chat, &m, r).is_err());
        assert_eq!(chat_edit(&f.store, &f.root, &f.chat, &m, req("   ")).unwrap_err().status, StatusCode::BAD_REQUEST);
        assert!(m.prompts.lock().unwrap().is_empty(), "no model call was spent");
    }

    #[test]
    fn proposals_parse_with_fences_and_markup_inside_the_body() {
        let p = parse_proposal("<reply> ok </reply>\n<body>\nUse <b>bold</b> and a </body> quote.\n</body>").unwrap();
        assert_eq!(p.reply, "ok");
        assert_eq!(p.body.as_deref(), Some("Use <b>bold</b> and a </body> quote.\n"));
        let p = parse_proposal("<sources>\n```json\n[\"notes.md\"]\n```\n</sources>").unwrap();
        assert_eq!(p.sources.unwrap()[0]["path"], "notes.md");
        assert!(parse_proposal("<sources>not json</sources>").is_err());
        assert!(parse_proposal("plain prose").is_err());
    }

    #[test]
    fn a_rename_carries_the_conversation() {
        let f = fx();
        chat_edit(&f.store, &f.root, &f.chat, &Scripted::ok("<reply>hi</reply>"), req("hello")).unwrap();
        move_chat_in(&f.chat, "ws.mdai", "renamed.mdai");
        assert!(load_turns(&f.chat, "ws.mdai", 10).0.is_empty());
        let (t, _) = load_turns(&f.chat, "renamed.mdai", 10);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0]["path"], "renamed.mdai");
    }
}
