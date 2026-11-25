//! Every direct outbound message waits for the owner, with its full context.
//!
//! THE OWNER'S RULE (2026-11-25): "all direct outbound comms (slack, email,
//! twitter response/dm, linkedin comment/dm, etc.) should go thru the amux
//! approval flow", and the approval should be "rich enough for full context".
//! Email (email_approval.rs), chat-bridge posts (bridge/mod.rs), phone calls
//! (calls.rs) and social posts (social_publish.rs) hold through their own
//! records. This module holds the rest: a message a worker sends by driving a
//! site in the browser (an X reply or DM, a LinkedIn comment or DM, a Reddit
//! or HN comment), which the server cannot send itself.
//!
//! THE FLOW. The worker files `POST /api/outbound` with the exact text, where
//! it goes, and why. It becomes a held row here and one Needs-you approval
//! card (approval_cards, kind `outbound`). Approving it opens a window
//! (`AMUX_OUTBOUND_WINDOW_S`, default 30 min) in which that worker's amux
//! browser may type on that site, and tells the worker to go ahead. The
//! worker then reports `POST /api/outbound/<id>/done` with the posted URL.
//! Outside a window, the browser refuses a worker's typing on an outbound site
//! (`outbound_needs_approval`), so the hold is enforced where the send happens.
//! An approved CRM touch (crm_agent approve_touch) opens the same window for
//! its target, because the owner already saw that exact message there.
//!
//! FULL CONTEXT. Every hold, here and in the other kinds, carries one context
//! object (see [`context`]): channel, action, account, recipient, target, the
//! full text, the worker's reason, the message being answered and the recent
//! thread. The card's description renders it ([`context_desc`]) and the
//! Needs-you item carries it whole (`outbound`), so the owner decides from
//! the same facts the worker had.

use axum::extract::{Path as AxPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use super::AppState;

/// Sites whose compose boxes send a message to someone outside. A host
/// matches itself and its subdomains; twitter.com is x.com.
pub const HOSTS: &[&str] = &[
    "x.com",
    "linkedin.com",
    "reddit.com",
    "news.ycombinator.com",
    "facebook.com",
    "messenger.com",
    "instagram.com",
    "threads.net",
    "bsky.app",
    "discord.com",
    "app.slack.com",
    "web.whatsapp.com",
    "web.telegram.org",
    "producthunt.com",
    "indiehackers.com",
    "dev.to",
    "medium.com",
];

const WINDOW_ENV: &str = "AMUX_OUTBOUND_WINDOW_S";
const MAX_TEXT: usize = 40_000;

fn now() -> i64 {
    crate::config::now_f64() as i64
}

fn window_s() -> i64 {
    std::env::var(WINDOW_ENV).ok().and_then(|v| v.trim().parse().ok()).filter(|v: &i64| *v > 0).unwrap_or(1800)
}

/// The host of `url`, lowercased, without `www.`.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.trim().split_once("://").map(|(_, r)| r).unwrap_or(url.trim());
    let host = rest.split(['/', '?', '#']).next()?.rsplit('@').next()?.split(':').next()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    (!host.is_empty()).then_some(host)
}

/// The outbound site `url` is on, if any (`twitter.com` reads as `x.com`).
pub fn outbound_host(url: &str) -> Option<&'static str> {
    let h = host_of(url)?;
    let h = if h == "twitter.com" || h.ends_with(".twitter.com") { "x.com".to_string() } else { h };
    HOSTS.iter().copied().find(|s| h == *s || h.ends_with(&format!(".{s}")))
}

/// The window key for a channel addressed by recipient rather than URL (a
/// Telegram chat id): `telegram:<chat_id>`.
pub fn recipient_key(channel: &str, to: &str) -> String {
    format!("{}:{}", channel.trim().to_ascii_lowercase(), to.trim())
}

pub fn id_label(n: i64) -> String {
    format!("OUT-{n}")
}

fn parse_id(s: &str) -> Option<i64> {
    s.trim().trim_start_matches("OUT-").trim_start_matches("out-").parse().ok()
}

fn bad(code: StatusCode, err: &str, msg: String) -> Response {
    (code, Json(json!({"ok": false, "code": err, "error": msg}))).into_response()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

// ── The shared context object ──────────────────────────────────────────────

/// What a worker may say about a send beyond the send itself: `reason` (or
/// `why`), and `context` {parent: {author, text, url, ts}, thread: [{author,
/// text, ts}], contact, notes, links}. Unknown keys are dropped, text is
/// bounded, so a card stays readable.
pub fn caller_context(body: &Value) -> Value {
    let s = |v: &Value, k: &str, n: usize| v.get(k).and_then(Value::as_str).map(|t| clip(t.trim(), n)).unwrap_or_default();
    let reason = [s(body, "reason", 2000), s(body, "why", 2000)].into_iter().find(|t| !t.is_empty()).unwrap_or_default();
    let c = body.get("context").cloned().unwrap_or(Value::Null);
    let mut out = json!({"reason": reason});
    if let Some(p) = c.get("parent").filter(|p| p.is_object()) {
        out["parent"] = json!({"author": s(p, "author", 200), "text": s(p, "text", 8000), "url": s(p, "url", 500), "ts": s(p, "ts", 60)});
    }
    if let Some(t) = c.get("thread").and_then(Value::as_array) {
        out["thread"] = Value::Array(
            t.iter().rev().take(12).rev()
                .map(|m| json!({"author": s(m, "author", 200), "text": s(m, "text", 2000), "ts": s(m, "ts", 60)}))
                .collect(),
        );
    }
    for k in ["contact", "notes", "source"] {
        if let Some(v) = c.get(k).and_then(Value::as_str).filter(|v| !v.trim().is_empty()) {
            out[k] = json!(clip(v.trim(), 2000));
        }
    }
    if let Some(l) = c.get("links").and_then(Value::as_array) {
        out["links"] = json!(l.iter().filter_map(Value::as_str).take(20).map(|u| clip(u, 500)).collect::<Vec<_>>());
    }
    out
}

/// One context object for a held send of any kind. `extra` is
/// [`caller_context`] or the kind's own facts (a reply's parent message).
#[allow(clippy::too_many_arguments)]
pub fn context(channel: &str, action: &str, account: &str, to: &str, target: &str, subject: &str, text: &str,
    worker: &str, extra: &Value) -> Value {
    let mut c = json!({
        "channel": channel, "action": action, "account": account, "to": to, "target_url": target,
        "subject": subject, "text": text, "worker": worker,
    });
    if let Some(o) = extra.as_object() {
        for (k, v) in o {
            if !(v.is_null() || v.as_str() == Some("") || v.as_array().is_some_and(|a| a.is_empty())) {
                c[k] = v.clone();
            }
        }
    }
    c
}

/// The context as card text: what goes where, why, what it answers, the
/// recent thread, and the full text last.
pub fn context_desc(c: &Value) -> String {
    let s = |k: &str| c.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let mut out = Vec::new();
    let what = [s("channel"), s("action")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" ");
    if !what.is_empty() {
        out.push(format!("What: {what}{}", if s("account").is_empty() { String::new() } else { format!(", as {}", s("account")) }));
    }
    for (k, label) in [("to", "To"), ("target_url", "Where"), ("subject", "Subject"), ("worker", "Asked by"), ("contact", "Contact")] {
        if !s(k).is_empty() {
            out.push(format!("{label}: {}", s(k)));
        }
    }
    out.push(format!("Why: {}", if s("reason").is_empty() { "the worker gave no reason".into() } else { s("reason") }));
    if let Some(p) = c.get("parent").filter(|p| p.is_object()) {
        let ps = |k: &str| p.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        let who = [ps("author"), ps("ts")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(", ");
        out.push(format!("\nReplying to{}:", if who.is_empty() { String::new() } else { format!(" ({who})") }));
        if !ps("url").is_empty() {
            out.push(ps("url"));
        }
        let text = ps("text");
        if text.is_empty() {
            out.push(format!("(not shown: {})", p.get("why_unmeasured").and_then(Value::as_str).unwrap_or("no text was given")));
        } else {
            out.extend(clip(&text, 4000).lines().map(|l| format!("> {l}")));
        }
    }
    if let Some(t) = c.get("thread").and_then(Value::as_array).filter(|t| !t.is_empty()) {
        out.push("\nRecent conversation:".into());
        for m in t {
            let ms = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
            out.push(format!("- {}: {}", if ms("author").is_empty() { "?".into() } else { ms("author") }, clip(&ms("text").replace('\n', " "), 400)));
        }
    }
    if !s("notes").is_empty() {
        out.push(format!("\nNotes: {}", s("notes")));
    }
    if let Some(l) = c.get("links").and_then(Value::as_array).filter(|l| !l.is_empty()) {
        out.push(format!("Links: {}", l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")));
    }
    out.push(format!("\nFull text:\n{}", s("text")));
    out.join("\n")
}

// ── Rows ───────────────────────────────────────────────────────────────────

fn row_json(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let ctx: String = r.get(9)?;
    Ok(json!({
        "id": id_label(r.get(0)?), "worker": r.get::<_, String>(1)?, "channel": r.get::<_, String>(2)?,
        "action": r.get::<_, String>(3)?, "account": r.get::<_, String>(4)?, "to": r.get::<_, String>(5)?,
        "target_url": r.get::<_, String>(6)?, "host": r.get::<_, String>(7)?, "text": r.get::<_, String>(8)?,
        "context": serde_json::from_str::<Value>(&ctx).unwrap_or_else(|_| json!({})),
        "crm_touch": r.get::<_, String>(10)?, "state": r.get::<_, String>(11)?, "created_at": r.get::<_, i64>(12)?,
        "decided_at": r.get::<_, Option<i64>>(13)?, "decided_by": r.get::<_, String>(14)?,
        "decision_note": r.get::<_, String>(15)?, "window_until": r.get::<_, Option<i64>>(16)?,
        "done_at": r.get::<_, Option<i64>>(17)?, "result_url": r.get::<_, String>(18)?,
    }))
}

const COLS: &str = "id, worker, channel, action, account, recipient, target_url, host, body, context, crm_touch, state, \
    created_at, decided_at, decided_by, decision_note, window_until, done_at, result_url";

pub fn get_in(conn: &Connection, id: i64) -> rusqlite::Result<Option<Value>> {
    conn.query_row(&format!("SELECT {COLS} FROM outbound_requests WHERE id = ?1"), [id], row_json).optional()
}

/// The approved request (or approved CRM touch) that lets `worker` type on
/// `host` now, as its label: `OUT-<n>` or `touch:<id>`.
pub fn window_in(conn: &Connection, worker: &str, host: &str, at: i64) -> Option<String> {
    let out = conn
        .query_row(
            "SELECT id FROM outbound_requests WHERE worker = ?1 AND host = ?2 AND state = 'approved'
               AND COALESCE(window_until, 0) > ?3 ORDER BY id DESC LIMIT 1",
            rusqlite::params![worker, host, at],
            |r| r.get::<_, i64>(0),
        )
        .optional()
        .ok()
        .flatten()
        .map(id_label);
    out.or_else(|| {
        let mut st = conn
            .prepare("SELECT id, target_url FROM crm_touches WHERE assignee = ?1 AND status = 'approved'
                        AND deleted IS NULL AND COALESCE(approved_at, 0) > ?2")
            .ok()?;
        let rows: Vec<(String, String)> = st
            .query_map(rusqlite::params![worker, at - window_s()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .ok()?
            .flatten()
            .collect();
        rows.into_iter().find(|(_, u)| outbound_host(u) == Some(host)).map(|(id, _)| format!("touch:{id}"))
    })
}

// ── The browser gate ───────────────────────────────────────────────────────

/// Whether a browser action puts text into the page: typing, filling, a
/// paste, a file upload, or script that writes into a field or clicks.
pub fn action_writes(action: &str, body: &Value) -> bool {
    match action {
        "type" | "input" | "files" => true,
        "key" => {
            let k = body.get("key").and_then(Value::as_str).unwrap_or("").to_ascii_lowercase();
            (k.contains("meta") || k.contains("control") || k.contains("ctrl") || k.contains("cmd")) && k.ends_with('v')
        }
        "eval" => {
            let s = body.get("script").and_then(Value::as_str).unwrap_or("");
            ["insertText", "execCommand", ".value=", ".value =", "innerText=", "innerText =", "textContent=",
             "textContent =", "innerHTML=", "innerHTML =", "dispatchEvent", ".click(", ".submit(", "DataTransfer"]
                .iter()
                .any(|p| s.contains(p))
        }
        _ => false,
    }
}

/// The refusal for a worker's write into an outbound site with no approved
/// request open, or `None` to let it through. `server_drive` is the server's
/// own publisher (it holds its own approval: social_publish).
pub fn browser_gate(state: &AppState, worker: &str, page_url: &str, action: &str, body: &Value) -> Option<Response> {
    if worker.trim().is_empty() || !action_writes(action, body) {
        return None;
    }
    let host = outbound_host(page_url)?;
    let open = state.store.read().ok().and_then(|c| window_in(&c, worker, host, now()));
    if let Some(id) = open {
        tracing::info!(worker, host, action, approval = %id, measured = true, n_considered = 1,
            verdict = "outbound_browser_allowed", "outbound: an approved request lets this worker type on this site");
        return None;
    }
    tracing::warn!(worker, host, action, url = %clip(page_url, 200), measured = true, n_considered = 1,
        verdict = "outbound_browser_refused", "outbound: a worker tried to type on an outbound site with no approved request");
    Some((StatusCode::FORBIDDEN, Json(json!({
        "ok": false, "code": "outbound_needs_approval", "host": host, "action": action,
        "error": format!("{host} sends messages to people outside; typing there needs the owner's approval of the exact message first"),
        "next": "POST $AMUX_URL/api/outbound {channel, action, target_url, to, text, reason, context: {parent: {author, text, url}, thread: [...]}} with your X-Amux-Session; once the owner approves, you are told and this browser may type there for a while. Then POST /api/outbound/<id>/done {url}.",
    }))).into_response())
}

// ── Routes ─────────────────────────────────────────────────────────────────

fn caller(headers: &HeaderMap) -> Option<String> {
    headers.get("x-amux-session").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty()).map(String::from)
}

/// POST /api/outbound: hold a direct outbound message for the owner.
async fn create(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<Value>) -> Response {
    let Some(worker) = caller(&headers) else {
        return bad(StatusCode::BAD_REQUEST, "worker_required",
            "send X-Amux-Session: an outbound request is a worker's ask to the owner (the owner sends directly)".into());
    };
    let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let text = [s("text"), s("body")].into_iter().find(|t| !t.is_empty()).unwrap_or_default();
    let (channel, action, target, to) = (s("channel").to_ascii_lowercase(), s("action").to_ascii_lowercase(), s("target_url"), s("to"));
    let mut missing = Vec::new();
    for (k, v) in [("channel", &channel), ("action", &action), ("text", &text)] {
        if v.is_empty() {
            missing.push(k);
        }
    }
    if target.is_empty() && to.is_empty() {
        missing.push("target_url or to");
    }
    let extra = caller_context(&b);
    if extra["reason"].as_str().unwrap_or("").is_empty() {
        missing.push("reason");
    }
    if !missing.is_empty() {
        return bad(StatusCode::BAD_REQUEST, "missing_fields", format!("required: {}", missing.join(", ")));
    }
    if text.chars().count() > MAX_TEXT {
        return bad(StatusCode::BAD_REQUEST, "text_too_long", format!("text is over {MAX_TEXT} characters"));
    }
    let host = if target.is_empty() {
        recipient_key(&channel, &to)
    } else {
        outbound_host(&target).map(str::to_string).or_else(|| host_of(&target)).unwrap_or_default()
    };
    let ctx = context(&channel, &action, &s("account"), &to, &target, "", &text, &worker, &extra);
    let touch = s("crm_touch");
    let at = now();
    let cell = std::sync::Arc::new(std::sync::Mutex::new(None::<(i64, bool)>));
    let w = cell.clone();
    let (wk, ch, ac, acct, rcp, tg, hs, tx, cx, tc) =
        (worker.clone(), channel.clone(), action.clone(), s("account"), to.clone(), target.clone(), host.clone(), text.clone(), ctx.to_string(), touch.clone());
    let res = state
        .store
        .write_async(move |conn| {
            // The same message asked again while it waits is the same request.
            let existing: Option<i64> = conn
                .query_row("SELECT id FROM outbound_requests WHERE worker=?1 AND channel=?2 AND action=?3 AND target_url=?4
                              AND recipient=?5 AND body=?6 AND state='held'",
                    rusqlite::params![wk, ch, ac, tg, rcp, tx], |r| r.get(0))
                .optional()?;
            if let Some(id) = existing {
                *w.lock().unwrap_or_else(|e| e.into_inner()) = Some((id, false));
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            conn.execute(
                "INSERT INTO outbound_requests (worker, channel, action, account, recipient, target_url, host, body, context, crm_touch, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![wk, ch, ac, acct, rcp, tg, hs, tx, cx, tc, at],
            )?;
            *w.lock().unwrap_or_else(|e| e.into_inner()) = Some((conn.last_insert_rowid(), true));
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, "write_failed", format!("could not record the request: {e}; nothing was approved"));
    }
    let Some((n, fresh)) = *cell.lock().unwrap_or_else(|e| e.into_inner()) else {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, "write_failed", "the request was not recorded".into());
    };
    let id = id_label(n);
    if fresh {
        let where_ = if to.is_empty() { target.clone() } else { to.clone() };
        crate::api::approval_cards::file(crate::api::approval_cards::Ask {
            kind: "outbound",
            item_id: id.clone(),
            requester: worker.clone(),
            title: format!("{worker}: {channel} {action} to {}: {}", clip(&where_, 60), clip(&text.replace('\n', " "), 70)),
            question: format!("Let {worker} send this {channel} {action} to {}?", clip(&where_, 120)),
            unblocks: format!("{worker} may post exactly this text on {} for {} minutes. Rejecting tells it not to; nothing is sent.",
                if host.is_empty() { channel.clone() } else { host.clone() }, window_s() / 60),
            desc: context_desc(&ctx),
            ask_type: "customer_outbound",
            expires_at: None,
            meta: json!({"outbound": ctx, "host": host}),
        })
        .await;
        tracing::info!(worker = %worker, id = %id, channel = %channel, action = %action, host = %host, measured = true,
            n_considered = 1, verdict = "outbound_held", "outbound: a worker's direct outbound message waits for the owner");
    }
    (StatusCode::ACCEPTED, Json(json!({
        "ok": true, "id": id, "state": "held", "reused": !fresh,
        "next": "wait: you are told when the owner approves or rejects it. Do not post it before then.",
    }))).into_response()
}

#[derive(serde::Deserialize, Default)]
struct ListQ {
    state: Option<String>,
    worker: Option<String>,
    limit: Option<i64>,
}

async fn list(State(state): State<AppState>, Query(q): Query<ListQ>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return bad(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable", e.to_string()),
    };
    let sql = format!("SELECT {COLS} FROM outbound_requests WHERE (?1 = '' OR state = ?1) AND (?2 = '' OR worker = ?2) ORDER BY id DESC LIMIT ?3");
    let rows: Result<Vec<Value>, _> = conn.prepare(&sql).and_then(|mut st| {
        st.query_map(rusqlite::params![q.state.unwrap_or_default(), q.worker.unwrap_or_default(), q.limit.unwrap_or(100).clamp(1, 1000)], row_json)?
            .collect()
    });
    match rows {
        Ok(items) => Json(json!({"measured": true, "n_considered": items.len(), "items": items})).into_response(),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, "read_failed", e.to_string()),
    }
}

async fn one(State(state): State<AppState>, AxPath(id): AxPath<String>) -> Response {
    let Some(n) = parse_id(&id) else { return bad(StatusCode::BAD_REQUEST, "bad_id", format!("{id} is not OUT-<n>")) };
    match state.store.read().map_err(|e| e.to_string()).and_then(|c| get_in(&c, n).map_err(|e| e.to_string())) {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => bad(StatusCode::NOT_FOUND, "not_found", format!("no {id}")),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, "read_failed", e),
    }
}

/// Decide a held request: the owner's approve or reject, through the card
/// (approval_cards dispatch) or these routes. A worker never decides.
pub async fn decide(state: &AppState, headers: &HeaderMap, id: &str, approve: bool, note: Option<String>) -> Response {
    if let Some(w) = super::capabilities::acting_worker(&crate::config::amux_home(), headers) {
        return bad(StatusCode::FORBIDDEN, "worker_cannot_decide", format!("{w} is a worker; only a person decides {id}"));
    }
    if let Some(r) = crate::api::approval_cards::duties("outbound", id, headers) {
        return r;
    }
    let by = match super::human_proof::check(headers, if approve { "outbound_approve" } else { "outbound_reject" }) {
        super::human_proof::Verdict::Refused => return super::human_proof::refusal("outbound_decide"),
        super::human_proof::Verdict::Proven(p) => p.actor(),
        super::human_proof::Verdict::Unproven => "owner (unproven)".to_string(),
    };
    let Some(n) = parse_id(id) else { return bad(StatusCode::BAD_REQUEST, "bad_id", format!("{id} is not OUT-<n>")) };
    let at = now();
    let until = at + window_s();
    let (by2, note2) = (by.clone(), note.clone().unwrap_or_default());
    let res = state
        .store
        .write_async(move |conn| {
            let n_rows = conn.execute(
                "UPDATE outbound_requests SET state = ?2, decided_at = ?3, decided_by = ?4, decision_note = ?5,
                   window_until = CASE WHEN ?2 = 'approved' THEN ?6 ELSE NULL END
                 WHERE id = ?1 AND state = 'held'",
                rusqlite::params![n, if approve { "approved" } else { "rejected" }, at, by2, note2, until],
            )?;
            if n_rows > 0 && approve {
                // A CRM touch filed through here is approved with it.
                conn.execute(
                    "UPDATE crm_touches SET status='approved', approved_by='owner', approved_at=?2, updated=?2
                     WHERE id = (SELECT crm_touch FROM outbound_requests WHERE id = ?1) AND status IN ('planned','drafted')",
                    rusqlite::params![n, at],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: n_rows > 0, events: vec![] })
        })
        .await;
    let applied = match res {
        Ok(r) => r.applied,
        Err(e) => return bad(StatusCode::INTERNAL_SERVER_ERROR, "write_failed", e.to_string()),
    };
    let row = state.store.read().ok().and_then(|c| get_in(&c, n).ok().flatten());
    let Some(row) = row else { return bad(StatusCode::NOT_FOUND, "not_found", format!("no {id}")) };
    if !applied {
        return (StatusCode::CONFLICT, Json(json!({"ok": false, "code": "not_held", "state": row["state"], "id": id}))).into_response();
    }
    let decision = if approve { "approved" } else { "rejected" };
    crate::api::approval_cards::resolved("outbound", id, decision, &by, note.clone()).await;
    let worker = row["worker"].as_str().unwrap_or("").to_string();
    let host = row["host"].as_str().unwrap_or("").to_string();
    let text = if approve {
        format!(
            "[amux outbound] {id} approved by {by}: send your {} {} now, exactly as approved, within {} minutes ({} accepts your typing until then). When it is posted: curl -sk -X POST -H 'Content-Type: application/json' -H \"X-Amux-Session: $AMUX_SESSION\" -d '{{\"url\":\"<the post's URL>\"}}' $AMUX_URL/api/outbound/{id}/done{}",
            row["channel"].as_str().unwrap_or(""), row["action"].as_str().unwrap_or(""), window_s() / 60,
            if host.is_empty() { "the site".to_string() } else { host.clone() },
            note.as_deref().map(|n| format!("\nOwner's note: {n}")).unwrap_or_default()
        )
    } else {
        format!("[amux outbound] {id} rejected by {by}: do not send it.{}", note.as_deref().map(|n| format!(" Owner's note: {n}")).unwrap_or_default())
    };
    let told = super::session_verbs::steer_enqueue_idempotent(state, &worker, &text, "", "outbound", &format!("outbound-{decision}-{id}")).await;
    if let Err(e) = &told {
        tracing::warn!(worker = %worker, id = %id, error = %e, measured = true, n_considered = 1, verdict = "outbound_decision_undelivered",
            "outbound: the decision is recorded but the worker could not be told");
    }
    tracing::info!(worker = %worker, id = %id, decision, by = %by, measured = true, n_considered = 1, verdict = "outbound_decided",
        "outbound: the owner decided a held outbound message");
    Json(json!({"ok": true, "id": id, "state": decision, "window_until": row["window_until"], "worker_told": told.is_ok()})).into_response()
}

/// The server sent an approved message itself (a Telegram send): record it
/// and close its window, so one approval sends one message.
pub async fn mark_sent(state: &AppState, id: &str, worker: &str, note: &str) {
    let Some(n) = parse_id(id) else { return };
    let (wk, note, at) = (worker.to_string(), note.to_string(), now());
    let res = state
        .store
        .write_async(move |conn| {
            let k = conn.execute(
                "UPDATE outbound_requests SET state='sent', done_at=?3, result_note=?4, window_until=?3
                 WHERE id=?1 AND worker=?2 AND state='approved'",
                rusqlite::params![n, wk, at, note],
            )?;
            Ok(crate::db::WriteOutcome { applied: k > 0, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        tracing::warn!(id, error = %e, measured = true, n_considered = 1, verdict = "outbound_mark_sent_failed",
            "outbound: a message was sent but its request could not be closed; its window stays open until it expires");
    }
}

#[derive(serde::Deserialize, Default)]
struct DecideBody {
    #[serde(default)]
    reason: Option<String>,
}

async fn approve_route(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>, b: Option<Json<DecideBody>>) -> Response {
    decide(&state, &headers, &id, true, b.and_then(|Json(b)| b.reason)).await
}

async fn reject_route(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>, b: Option<Json<DecideBody>>) -> Response {
    decide(&state, &headers, &id, false, b.and_then(|Json(b)| b.reason)).await
}

/// POST /api/outbound/<id>/done {url?, note?, failed?}: the worker reports the
/// approved message posted (or that it could not post it). Closes the window.
async fn done(State(state): State<AppState>, headers: HeaderMap, AxPath(id): AxPath<String>, b: Option<Json<Value>>) -> Response {
    let Some(worker) = caller(&headers) else {
        return bad(StatusCode::BAD_REQUEST, "worker_required", "send X-Amux-Session".into());
    };
    let Some(n) = parse_id(&id) else { return bad(StatusCode::BAD_REQUEST, "bad_id", format!("{id} is not OUT-<n>")) };
    let b = b.map(|Json(v)| v).unwrap_or(Value::Null);
    let s = |k: &str| b.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let failed = b.get("failed").and_then(Value::as_bool).unwrap_or(false);
    let (url, note, at, wk) = (clip(&s("url"), 1000), clip(&s("note"), 2000), now(), worker.clone());
    let res = state
        .store
        .write_async(move |conn| {
            let n_rows = conn.execute(
                "UPDATE outbound_requests SET state = ?3, done_at = ?4, result_url = ?5, result_note = ?6, window_until = ?4
                 WHERE id = ?1 AND worker = ?2 AND state = 'approved'",
                rusqlite::params![n, wk, if failed { "failed" } else { "sent" }, at, url, note],
            )?;
            Ok(crate::db::WriteOutcome { applied: n_rows > 0, events: vec![] })
        })
        .await;
    match res {
        Ok(r) if r.applied => {
            tracing::info!(worker = %worker, id = %id, failed, measured = true, n_considered = 1,
                verdict = if failed { "outbound_failed" } else { "outbound_sent" }, "outbound: the worker reported an approved message");
            Json(json!({"ok": true, "id": id, "state": if failed { "failed" } else { "sent" }})).into_response()
        }
        Ok(_) => {
            let st = state.store.read().ok().and_then(|c| get_in(&c, n).ok().flatten());
            (StatusCode::CONFLICT, Json(json!({"ok": false, "code": "not_approved",
                "error": format!("{id} is not an approved request of {worker}'s"),
                "state": st.as_ref().map(|r| r["state"].clone()).unwrap_or(Value::Null)}))).into_response()
        }
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, "write_failed", e.to_string()),
    }
}

/// POST /api/outbound/check {url, action, key?, script?}: the browser route
/// driver asks before a CDP or CUA write. 200 when allowed, else the gate's 403.
async fn check(State(state): State<AppState>, headers: HeaderMap, Json(b): Json<Value>) -> Response {
    let worker = caller(&headers).unwrap_or_default();
    let url = b.get("url").and_then(Value::as_str).unwrap_or("");
    let action = b.get("action").and_then(Value::as_str).unwrap_or("");
    match browser_gate(&state, &worker, url, action, &b) {
        Some(refused) => refused,
        None => Json(json!({"ok": true, "allowed": true, "outbound_host": outbound_host(url)})).into_response(),
    }
}

pub fn routes() -> Router<AppState> {
    use axum::routing::{get, post};
    Router::new()
        .route("/api/outbound/check", post(check))
        .route("/api/outbound", get(list).post(create))
        .route("/api/outbound/{id}", get(one))
        .route("/api/outbound/{id}/approve", post(approve_route))
        .route("/api/outbound/{id}/reject", post(reject_route))
        .route("/api/outbound/{id}/done", post(done))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_sites_are_matched_by_host_and_subdomain() {
        assert_eq!(outbound_host("https://x.com/someone/status/1"), Some("x.com"));
        assert_eq!(outbound_host("https://mobile.twitter.com/a"), Some("x.com"));
        assert_eq!(outbound_host("https://www.linkedin.com/feed/update/urn:li:activity:1"), Some("linkedin.com"));
        assert_eq!(outbound_host("https://old.reddit.com/r/rust"), Some("reddit.com"));
        assert_eq!(outbound_host("https://app.slack.com/client/T1/C1"), Some("app.slack.com"));
        assert_eq!(outbound_host("https://mixpeek.com/demo"), None);
        assert_eq!(outbound_host("https://notx.com/"), None, "a host that only ends in the name is not the site");
        assert_eq!(outbound_host("https://slack.com/pricing"), None, "only Slack's app sends messages");
    }

    #[test]
    fn writing_actions_are_told_apart_from_reading_ones() {
        assert!(action_writes("type", &json!({"text": "hi"})));
        assert!(action_writes("input", &json!({"text": "hi"})));
        assert!(action_writes("key", &json!({"key": "Meta+v"})));
        assert!(!action_writes("key", &json!({"key": "Enter"})), "Enter alone sends nothing that was not typed");
        assert!(action_writes("eval", &json!({"script": "document.execCommand('insertText', false, 'hi')"})));
        assert!(action_writes("eval", &json!({"script": "document.querySelector('button').click()"})));
        assert!(!action_writes("eval", &json!({"script": "document.title"})));
        assert!(!action_writes("click", &json!({"x": 1, "y": 2})));
        assert!(!action_writes("scroll", &json!({"dy": 400})));
    }

    #[test]
    fn the_card_text_carries_the_reason_the_parent_the_thread_and_the_full_text() {
        let extra = caller_context(&json!({
            "reason": "she asked how we index video",
            "context": {"parent": {"author": "@jane", "text": "How do you index video?", "url": "https://x.com/jane/status/9"},
                        "thread": [{"author": "@jane", "text": "first"}, {"author": "@ethan", "text": "second"}],
                        "contact": "PPL-7 Jane Doe", "ignored": "x"}
        }));
        assert!(extra.get("ignored").is_none());
        let c = context("x", "reply", "@ethansteininger", "@jane", "https://x.com/jane/status/9", "", "Line one\nLine two", "gtm", &extra);
        let d = context_desc(&c);
        for want in ["What: x reply, as @ethansteininger", "To: @jane", "Asked by: gtm", "Contact: PPL-7 Jane Doe",
            "Why: she asked how we index video", "Replying to (@jane):", "> How do you index video?",
            "Recent conversation:", "- @ethan: second", "Full text:\nLine one\nLine two"] {
            assert!(d.contains(want), "missing {want:?} in:\n{d}");
        }
        let bare = context_desc(&context("x", "dm", "", "@bob", "", "", "hi", "w", &json!({})));
        assert!(bare.contains("Why: the worker gave no reason"), "{bare}");
    }

    fn conn() -> (tempfile::TempDir, crate::db::Store, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ob.db");
        let store = crate::db::Store::open(&path).unwrap();
        let c = Connection::open(&path).unwrap();
        (dir, store, c)
    }

    #[test]
    fn only_an_approved_open_request_for_that_worker_and_site_opens_the_window() {
        let (_d, _s, c) = conn();
        let ins = |worker: &str, host: &str, state: &str, until: i64| {
            c.execute("INSERT INTO outbound_requests (worker, channel, action, host, body, state, created_at, window_until)
                       VALUES (?1, 'x', 'reply', ?2, 'hi', ?3, 1, ?4)", rusqlite::params![worker, host, state, until]).unwrap();
            id_label(c.last_insert_rowid())
        };
        ins("w", "x.com", "held", 2000);
        assert_eq!(window_in(&c, "w", "x.com", 1000), None, "a held request opens nothing");
        ins("w", "x.com", "approved", 900);
        assert_eq!(window_in(&c, "w", "x.com", 1000), None, "an expired window is closed");
        let open = ins("w", "x.com", "approved", 2000);
        assert_eq!(window_in(&c, "w", "x.com", 1000), Some(open));
        assert_eq!(window_in(&c, "other", "x.com", 1000), None, "another worker's approval is not this one's");
        assert_eq!(window_in(&c, "w", "linkedin.com", 1000), None, "an approval for X does not open LinkedIn");
    }

    #[test]
    fn an_approved_crm_touch_opens_the_window_for_its_target() {
        let (_d, _s, c) = conn();
        let at = 10_000_i64;
        c.execute("INSERT INTO crm_touches (id, contact_id, channel, action, body, target_url, assignee, status, approved_at, created, updated)
                   VALUES ('T1', 'PPL-1', 'linkedin', 'comment', 'nice', 'https://www.linkedin.com/feed/update/1', 'gtm', 'approved', ?1, ?1, ?1)",
            [at]).unwrap();
        assert_eq!(window_in(&c, "gtm", "linkedin.com", at + 60), Some("touch:T1".into()));
        assert_eq!(window_in(&c, "gtm", "x.com", at + 60), None);
        assert_eq!(window_in(&c, "gtm", "linkedin.com", at + window_s() + 1), None, "the touch's window closes too");
    }

    fn state() -> (tempfile::TempDir, AppState) {
        let home = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(crate::db::Store::open(&home.path().join("amux.db")).unwrap());
        let state = AppState { store, started: std::time::Instant::now(), build_hash: "test".into(),
            auth_token: None, reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)) };
        (home, state)
    }

    fn worker(name: &'static str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-amux-session", axum::http::HeaderValue::from_static(name));
        h
    }

    async fn json_of(r: Response) -> (StatusCode, Value) {
        let st = r.status();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    /// The whole flow: a worker's X reply is held with its context, the
    /// browser refuses its typing on x.com, the owner approves, the browser
    /// lets it type there (and nowhere else), and its report closes the window.
    #[tokio::test]
    async fn a_held_reply_opens_the_browser_only_once_the_owner_approves_it() {
        let (home, st) = state();
        let _g = crate::api::settings::test_env::set_home(home.path());
        let body = json!({"channel": "x", "action": "reply", "target_url": "https://x.com/jane/status/9", "to": "@jane",
            "text": "We index video frame by frame.", "reason": "she asked",
            "context": {"parent": {"author": "@jane", "text": "How do you index video?"}}});
        let (code, v) = json_of(create(State(st.clone()), worker("gtm"), Json(body.clone())).await).await;
        assert_eq!(code, StatusCode::ACCEPTED, "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        let (_, again) = json_of(create(State(st.clone()), worker("gtm"), Json(body)).await).await;
        assert_eq!(again["id"], json!(id), "the same message asked again is the same request");
        assert_eq!(again["reused"], json!(true));
        let row = get_in(&st.store.read().unwrap(), parse_id(&id).unwrap()).unwrap().unwrap();
        assert_eq!(row["state"], "held");
        assert_eq!(row["context"]["parent"]["text"], "How do you index video?");
        assert_eq!(row["context"]["reason"], "she asked");

        let typing = json!({"action": "type", "text": "We index video"});
        let refused = browser_gate(&st, "gtm", "https://x.com/jane/status/9", "type", &typing).expect("held is not approved");
        let (code, v) = json_of(refused).await;
        assert_eq!((code, v["code"].as_str()), (StatusCode::FORBIDDEN, Some("outbound_needs_approval")));
        assert!(browser_gate(&st, "gtm", "https://mixpeek.com/", "type", &typing).is_none(), "other sites are not gated");
        assert!(browser_gate(&st, "gtm", "https://x.com/jane", "scroll", &json!({})).is_none(), "reading is not gated");

        let (code, v) = json_of(decide(&st, &worker("gtm"), &id, true, None).await).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "a worker cannot approve its own request: {v}");
        let (code, v) = json_of(decide(&st, &HeaderMap::new(), &id, true, Some("fine".into())).await).await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v["state"], "approved");
        assert!(browser_gate(&st, "gtm", "https://x.com/jane/status/9", "type", &typing).is_none(), "approved: it may type on x.com");
        assert!(browser_gate(&st, "ops", "https://x.com/jane/status/9", "type", &typing).is_some(), "another worker may not");
        assert!(browser_gate(&st, "gtm", "https://www.linkedin.com/feed/", "type", &typing).is_some(), "nor on another site");
        let (code, _) = json_of(decide(&st, &HeaderMap::new(), &id, false, None).await).await;
        assert_eq!(code, StatusCode::CONFLICT, "a decided request is not decided again");

        let (code, v) = json_of(done(State(st.clone()), worker("ops"), AxPath(id.clone()), Some(Json(json!({"url": "u"})))).await).await;
        assert_eq!(code, StatusCode::CONFLICT, "only the requester reports it: {v}");
        let (code, v) = json_of(done(State(st.clone()), worker("gtm"), AxPath(id.clone()), Some(Json(json!({"url": "https://x.com/e/status/10"})))).await).await;
        assert_eq!((code, v["state"].as_str()), (StatusCode::OK, Some("sent")));
        assert!(browser_gate(&st, "gtm", "https://x.com/jane/status/9", "type", &typing).is_some(), "a sent message closes its window");
    }

    #[tokio::test]
    async fn a_request_without_a_reason_or_a_target_is_refused() {
        let (home, st) = state();
        let _g = crate::api::settings::test_env::set_home(home.path());
        let (code, v) = json_of(create(State(st.clone()), worker("gtm"), Json(json!({"channel": "x", "action": "dm", "text": "hi"}))).await).await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        let e = v["error"].as_str().unwrap_or("");
        assert!(e.contains("target_url or to") && e.contains("reason"), "{e}");
        let (code, _) = json_of(create(State(st.clone()), HeaderMap::new(), Json(json!({}))).await).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "the owner does not file requests to themself");
    }
}

