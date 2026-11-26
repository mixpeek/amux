//! `GET /api/outbound/feed`: every outbound message a worker wants to send, on
//! every channel, in one list for the dashboard's Outbound tab.
//!
//! A PROJECTION, NOT A STORE (A1, A10). Each kind keeps its own record and its
//! own approve path, and this module only reads them:
//! - email: the held drafts in `~/.amux/email-approvals` (api/email_approval.rs),
//!   sent with the Gmail connector;
//! - chat: `bridge_outbound`, a reply into a linked WhatsApp, Slack, Telegram,
//!   Discord or self chat (bridge/mod.rs);
//! - social: `social_queue`, a public post (api/social_publish.rs);
//! - browser: `outbound_requests`, a message a worker types on a site with no
//!   API, such as an X or LinkedIn DM (api/outbound.rs).
//!
//! The full context a hold carries (reason, the message it answers, the recent
//! thread) is read from its approval card's link (`_amux_approval_cards.meta`),
//! the same object the Needs-you card renders. The recipient is matched to the
//! CRM by email or handle. Deciding goes through the existing paths:
//! `/api/approvals/decide` for every linked hold, and `/api/email/approve` for
//! an email edit or a scheduled send.
//!
//! Every item says how it will go out (`via`: api, browser or mac_app) and the
//! connector it needs, so the owner sees a send that cannot happen yet.

use std::collections::HashMap;
use std::path::Path;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Map, Value};

use super::AppState;

/// How many decided items each source contributes to the history views.
const HISTORY: usize = 200;

/// The views the tab shows, in order. Every item is in exactly one state.
pub const STATES: &[&str] = &["needs", "scheduled", "approved", "sent", "declined", "failed"];

fn now() -> f64 {
    crate::config::now_f64()
}

/// How a channel goes out and which connector it needs.
/// `(via, connector id, connector label)`.
pub fn route_of(kind: &str, channel: &str) -> (&'static str, &'static str, &'static str) {
    match (kind, channel) {
        ("email", _) => ("api", "google-gmail", "Gmail"),
        ("chat", "slack") => ("api", "slack-bot", "Slack bot"),
        ("chat", "telegram") => ("api", "telegram", "Telegram bot"),
        ("chat", "discord") => ("api", "discord", "Discord bot"),
        ("chat", "whatsapp") => ("api", "whatsapp", "WhatsApp Cloud API"),
        ("chat", "email") => ("api", "google-gmail", "Gmail"),
        ("chat", _) => ("mac_app", "", "the app on this Mac"),
        ("social", "devto") => ("api", "devto", "DEV"),
        ("social", "youtube") => ("api", "google-youtube", "YouTube"),
        ("social", _) => ("browser", "", "the amux browser"),
        _ => ("browser", "", "the amux browser"),
    }
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The approval links that carry context, keyed by `approval_ref`.
fn links_in(conn: &Connection) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    let Ok(mut st) = conn.prepare(
        "SELECT approval_ref, card_id, meta, decision, decided_at, reason FROM _amux_approval_cards
         WHERE kind IN ('email','chat','social','outbound') ORDER BY filed_at DESC LIMIT 2000",
    ) else {
        return out;
    };
    let rows = st.query_map([], |r| {
        let meta: String = r.get(2)?;
        Ok((r.get::<_, String>(0)?, json!({
            "card": r.get::<_, Option<String>>(1)?,
            "meta": serde_json::from_str::<Value>(&meta).unwrap_or_else(|_| json!({})),
            "decision": r.get::<_, Option<String>>(3)?,
            "decided_at": r.get::<_, Option<f64>>(4)?,
            "reason": r.get::<_, Option<String>>(5)?,
        })))
    });
    if let Ok(rows) = rows {
        for (k, v) in rows.flatten() {
            out.insert(k, v);
        }
    }
    out
}

/// One item in the shape the tab renders.
#[allow(clippy::too_many_arguments)]
fn item(kind: &str, id: &str, channel: &str, state: &str, worker: &str, created: f64, text: &str, links: &HashMap<String, Value>) -> Map<String, Value> {
    let approval_ref = format!("{kind}:{id}");
    let link = links.get(&approval_ref);
    let ctx = link.map(|l| l["meta"]["outbound"].clone()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    let (via, connector, connector_label) = route_of(kind, channel);
    let mut m = Map::new();
    m.insert("ref".into(), json!(approval_ref));
    m.insert("kind".into(), json!(kind));
    m.insert("id".into(), json!(id));
    m.insert("channel".into(), json!(channel));
    m.insert("state".into(), json!(state));
    m.insert("worker".into(), json!(worker));
    m.insert("created".into(), json!(created));
    m.insert("text".into(), json!(if text.is_empty() { s(&ctx, "text") } else { text.to_string() }));
    m.insert("action".into(), json!(s(&ctx, "action")));
    m.insert("account".into(), json!(s(&ctx, "account")));
    m.insert("to".into(), json!(s(&ctx, "to")));
    m.insert("target".into(), json!(s(&ctx, "target")));
    m.insert("subject".into(), json!(s(&ctx, "subject")));
    m.insert("reason".into(), json!(s(&ctx, "reason")));
    m.insert("parent".into(), ctx.get("parent").cloned().unwrap_or(Value::Null));
    m.insert("thread".into(), ctx.get("thread").cloned().unwrap_or(Value::Null));
    m.insert("notes".into(), json!(s(&ctx, "notes")));
    m.insert("via".into(), json!(via));
    m.insert("connector".into(), json!(connector));
    m.insert("connector_label".into(), json!(connector_label));
    // The approval card this hold filed. `decidable` is what the Approve
    // button needs: an open link `/api/approvals/decide` can resolve.
    m.insert("card".into(), link.and_then(|l| l["card"].as_str().map(str::to_string)).map(Value::from).unwrap_or(Value::Null));
    m.insert("decidable".into(), json!(state == "needs" && link.is_some_and(|l| l["decided_at"].is_null())));
    m.insert("decision_reason".into(), link.and_then(|l| l["reason"].as_str().map(str::to_string)).map(Value::from).unwrap_or(Value::Null));
    m
}

// ── States: one mapping per source, used by the list and the totals ──────

/// Every state's total over each source's whole record, which the list (capped
/// at HISTORY per source) cannot give.
type Totals = HashMap<&'static str, u64>;

fn add(t: &mut Totals, state: &'static str, n: u64) {
    *t.entry(state).or_insert(0) += n;
}

fn browser_state(st: &str) -> &'static str {
    match st {
        "held" => "needs",
        "approved" => "approved",
        "sent" | "posted" | "done" => "sent",
        "rejected" | "cancelled" => "declined",
        _ => "failed",
    }
}

fn social_state(status: &str, future: bool) -> &'static str {
    match status {
        "held" => "needs",
        "queued" if future => "scheduled",
        "queued" | "claimed" | "publishing" => "approved",
        "posted" => "sent",
        "cancelled" | "rejected" => "declined",
        _ => "failed",
    }
}

fn chat_state(st: &str) -> &'static str {
    match st {
        "held" => "needs",
        "approved" | "sending" => "approved",
        "sent" => "sent",
        "rejected" => "declined",
        _ => "failed",
    }
}

/// The table sources' totals, grouped in SQL.
fn table_totals(conn: &Connection, t: &mut Totals) {
    let mut group = |sql: &str, map: &dyn Fn(&str, i64) -> &'static str| {
        let Ok(mut st) = conn.prepare(sql) else { return };
        let now = now() as i64;
        let Ok(rows) = st.query_map([now], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))) else { return };
        for (state, flag, n) in rows.flatten() {
            add(t, map(&state, flag), n as u64);
        }
    };
    group("SELECT state, 0, COUNT(*) FROM outbound_requests WHERE ?1 IS NOT NULL GROUP BY state", &|s, _| browser_state(s));
    group("SELECT status, scheduled_at > ?1, COUNT(*) FROM social_queue GROUP BY 1, 2", &|s, f| social_state(s, f != 0));
    group("SELECT state, 0, COUNT(*) FROM bridge_outbound WHERE ?1 IS NOT NULL GROUP BY state", &|s, _| chat_state(s));
}

// ── Email ──────────────────────────────────────────────────────────────────

fn email_items(home: &Path, links: &HashMap<String, Value>, out: &mut Vec<Value>, totals: &mut Totals) {
    for doc in crate::api::email_approval::list_pending(home) {
        let id = s(&doc, "id");
        if id.is_empty() {
            continue;
        }
        let state = if crate::api::email_approval::is_scheduled(&doc) { "scheduled" } else { "needs" };
        add(totals, state, 1);
        out.push(email_item(&doc, &id, state, None, links, true));
    }
    // Decided drafts: renamed in place to .approved / .rejected / .expired.
    let dir = crate::api::email_approval::approvals_dir(home);
    let mut done: Vec<(f64, std::path::PathBuf, &str)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let state = if name.ends_with(".approved.json") {
                "sent"
            } else if name.ends_with(".rejected.json") || name.ends_with(".expired.json") {
                "declined"
            } else {
                continue;
            };
            let at = e.metadata().and_then(|m| m.modified()).ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            add(totals, state, 1);
            done.push((at, e.path(), state));
        }
    }
    done.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (at, path, state) in done.into_iter().take(HISTORY) {
        let Some(doc) = std::fs::read_to_string(&path).ok().and_then(|r| serde_json::from_str::<Value>(&r).ok()) else { continue };
        let id = s(&doc, "id");
        if !id.is_empty() {
            out.push(email_item(&doc, &id, state, Some(at), links, false));
        }
    }
}

fn email_item(doc: &Value, id: &str, state: &str, decided: Option<f64>, links: &HashMap<String, Value>, pending: bool) -> Value {
    let p = &doc["payload"];
    let pv = &doc["preview"];
    // A pending draft carries its whole text in `review` (list_pending drops
    // the payload, and puts a scheduled edit there); a decided one in `payload`.
    let body = doc["review"]["body"].as_str().or_else(|| p["body"].as_str()).or_else(|| pv["body"].as_str()).unwrap_or("");
    let mut m = item("email", id, "email", state, &s(doc, "session"), doc["created"].as_f64().unwrap_or(0.0), body, links);
    // The preview is what the owner reviews: it names the recipients the
    // payload resolves to (a reply's are read from the thread).
    let ctx = pv["outbound"].clone();
    let pick = |k: &str, alt: &str| {
        let v = s(pv, k);
        if !v.is_empty() { v } else if !alt.is_empty() { s(&ctx, alt) } else { String::new() }
    };
    m.insert("to".into(), json!(pick("to", "to")));
    m.insert("cc".into(), json!(s(pv, "cc")));
    let from = pick("from", "account");
    m.insert("account".into(), json!(if from.is_empty() { s(p, "from") } else { from }));
    m.insert("subject".into(), json!(pick("subject", "subject")));
    let endpoint = s(doc, "endpoint");
    let ctx_action = s(&ctx, "action");
    let action = if !ctx_action.is_empty() {
        ctx_action
    } else if endpoint == "reply" {
        // The preview records it for a pending draft; the payload for a decided one.
        if pv["reply_all"].as_bool().or_else(|| p["reply_all"].as_bool()) == Some(true) { "reply all".into() } else { "reply".into() }
    } else {
        "send".into()
    };
    m.insert("action".into(), json!(action));
    if ctx.is_object() {
        for k in ["reason", "notes"] {
            if m.get(k).and_then(Value::as_str).is_none_or(str::is_empty) {
                m.insert(k.into(), json!(s(&ctx, k)));
            }
        }
        for k in ["parent", "thread"] {
            if m.get(k).is_none_or(Value::is_null) {
                m.insert(k.into(), ctx.get(k).cloned().unwrap_or(Value::Null));
            }
        }
    }
    m.insert("attachments".into(), json!(if pending {
        doc["review"]["attachments"].as_array().cloned().unwrap_or_default()
    } else {
        p["attachments"].as_array().map(|a| a.iter().enumerate().map(|(i, x)| json!({
            "index": i, "filename": x["filename"].as_str().map(str::to_string).or_else(|| x["path"].as_str()
                .and_then(|q| Path::new(q).file_name()).map(|n| n.to_string_lossy().into_owned())).unwrap_or_else(|| "attachment".into()),
            "size": Value::Null, "ok": false,
        })).collect::<Vec<_>>()).unwrap_or_default()
    }));
    if let Some(at) = doc["scheduled"]["send_at"].as_f64() {
        m.insert("scheduled_at".into(), json!(at));
    }
    if let Some(at) = decided {
        m.insert(if state == "sent" { "sent_at" } else { "decided_at" }.into(), json!(at));
    }
    // An email is decidable from its own endpoint whether or not a card links it.
    m.insert("decidable".into(), json!(state == "needs"));
    m.insert("editable".into(), json!(pending));
    Value::Object(m)
}

// ── Browser-typed (outbound_requests) ─────────────────────────────────────

fn browser_items(conn: &Connection, links: &HashMap<String, Value>, out: &mut Vec<Value>) {
    let Ok(mut st) = conn.prepare(
        "SELECT id, worker, channel, action, account, recipient, target_url, host, body, state, created_at, decided_at,
                done_at, result_url, result_note, window_until, decision_note
         FROM outbound_requests ORDER BY (state='held') DESC, id DESC LIMIT ?1",
    ) else {
        return;
    };
    let rows = st.query_map([HISTORY as i64 + 200], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
            r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, String>(7)?,
            r.get::<_, String>(8)?, r.get::<_, String>(9)?, r.get::<_, i64>(10)?, r.get::<_, Option<i64>>(11)?,
            r.get::<_, Option<i64>>(12)?, r.get::<_, String>(13)?, r.get::<_, String>(14)?, r.get::<_, Option<i64>>(15)?,
            r.get::<_, String>(16)?))
    });
    let Ok(rows) = rows else { return };
    for (id, worker, channel, action, account, to, target, host, body, st_, created, decided, done, url, note, window, dnote) in rows.flatten() {
        let label = crate::api::outbound::id_label(id);
        let state = browser_state(&st_);
        let mut m = item("outbound", &label, &channel, state, &worker, created as f64, &body, links);
        m.insert("action".into(), json!(action));
        m.insert("account".into(), json!(account));
        m.insert("to".into(), json!(to));
        m.insert("target".into(), json!(target));
        m.insert("host".into(), json!(host));
        if let Some(d) = decided {
            m.insert("decided_at".into(), json!(d));
        }
        if let Some(d) = done {
            m.insert("sent_at".into(), json!(d));
        }
        if let Some(w) = window {
            m.insert("window_until".into(), json!(w));
        }
        m.insert("result_url".into(), json!(url));
        if !note.is_empty() {
            m.insert("result_note".into(), json!(note));
        }
        if !dnote.is_empty() && m.get("decision_reason").is_none_or(Value::is_null) {
            m.insert("decision_reason".into(), json!(dnote));
        }
        out.push(Value::Object(m));
    }
}

// ── Social posts ──────────────────────────────────────────────────────────

fn social_items(conn: &Connection, links: &HashMap<String, Value>, out: &mut Vec<Value>) {
    let Ok(mut st) = conn.prepare(
        "SELECT id, platform, account, draft, scheduled_at, status, error, url, session, created_at, updated_at
         FROM social_queue ORDER BY (status='held') DESC, id DESC LIMIT ?1",
    ) else {
        return;
    };
    let rows = st.query_map([HISTORY as i64 + 200], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?, r.get::<_, String>(5)?, r.get::<_, Option<String>>(6)?, r.get::<_, String>(7)?,
            r.get::<_, String>(8)?, r.get::<_, i64>(9)?, r.get::<_, i64>(10)?))
    });
    let Ok(rows) = rows else { return };
    let t = now() as i64;
    for (id, platform, account, draft, when, status, error, url, session, created, updated) in rows.flatten() {
        let d: Value = serde_json::from_str(&draft).unwrap_or_else(|_| json!({}));
        let state = social_state(&status, when > t);
        let mut m = item("social", &id.to_string(), &platform, state, &session, created as f64, &s(&d, "caption"), links);
        m.insert("action".into(), json!("post"));
        m.insert("account".into(), json!(account));
        if m.get("subject").and_then(Value::as_str).is_none_or(str::is_empty) {
            m.insert("subject".into(), json!(s(&d, "title")));
        }
        m.insert("scheduled_at".into(), json!(when));
        if state == "sent" {
            m.insert("sent_at".into(), json!(updated));
        }
        m.insert("result_url".into(), json!(url));
        if let Some(e) = error.filter(|e| !e.is_empty()) {
            m.insert("error".into(), json!(e));
        }
        let file = s(&d, "file");
        if !file.is_empty() {
            let name = Path::new(&file).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(file.clone());
            m.insert("attachments".into(), json!([{"index": 0, "filename": name, "path": file, "size": Value::Null, "ok": false}]));
        }
        out.push(Value::Object(m));
    }
}

// ── Chat bridge replies ───────────────────────────────────────────────────

fn chat_items(conn: &Connection, links: &HashMap<String, Value>, out: &mut Vec<Value>) {
    let Ok(mut st) = conn.prepare(
        "SELECT o.id, o.platform, o.conversation, o.state, o.error, o.created_at, o.sent_at,
                COALESCE(p.body, ''), COALESCE(p.author_key, ''), COALESCE(p.conv, '')
         FROM bridge_outbound o LEFT JOIN conversation_posts p ON p.id = o.post_id
         ORDER BY (o.state='held') DESC, o.created_at DESC LIMIT ?1",
    ) else {
        return;
    };
    let rows = st.query_map([HISTORY as i64 + 200], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?, r.get::<_, i64>(5)?, r.get::<_, Option<i64>>(6)?,
            r.get::<_, String>(7)?, r.get::<_, String>(8)?, r.get::<_, String>(9)?))
    });
    let Ok(rows) = rows else { return };
    for (id, platform, conversation, st_, error, created, sent, body, author, conv) in rows.flatten() {
        let state = chat_state(&st_);
        // created_at is milliseconds in this table.
        let created_s = if created > 100_000_000_000 { created as f64 / 1000.0 } else { created as f64 };
        let worker = author.trim_start_matches("worker:").to_string();
        let mut m = item("chat", &id, &platform, state, &worker, created_s, &body, links);
        if m.get("to").and_then(Value::as_str).is_none_or(str::is_empty) {
            m.insert("to".into(), json!(conversation));
        }
        m.insert("conv".into(), json!(conv));
        if m.get("action").and_then(Value::as_str).is_none_or(str::is_empty) {
            m.insert("action".into(), json!("message"));
        }
        if let Some(t) = sent {
            m.insert("sent_at".into(), json!(if t > 100_000_000_000 { t as f64 / 1000.0 } else { t as f64 }));
        }
        if let Some(e) = error.filter(|e| !e.is_empty()) {
            m.insert("error".into(), json!(e));
        }
        out.push(Value::Object(m));
    }
}

// ── CRM ────────────────────────────────────────────────────────────────────

/// The first email address in a recipient list.
fn first_email(to: &str) -> Option<String> {
    to.split([',', ';']).find_map(|part| {
        let p = part.trim();
        let addr = match (p.rfind('<'), p.rfind('>')) {
            (Some(a), Some(b)) if b > a => &p[a + 1..b],
            _ => p,
        };
        let addr = addr.trim().to_ascii_lowercase();
        // local@domain.tld: a handle like `@rio` has no local part.
        let ok = addr.split_once('@').is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.'))
            && !addr.contains(' ');
        ok.then_some(addr)
    })
}

/// The CRM contact and company a recipient matches, by email or handle.
pub fn crm_for(conn: &Connection, channel: &str, to: &str, target: &str) -> Value {
    let contact = |sql: &str, key: &str| -> Option<Value> {
        conn.query_row(sql, [key], |r| Ok(json!({
            "id": r.get::<_, String>(0)?, "name": r.get::<_, Option<String>>(1)?, "role": r.get::<_, Option<String>>(2)?,
            "company": r.get::<_, Option<String>>(3)?, "email": r.get::<_, Option<String>>(4)?,
            "status": r.get::<_, Option<String>>(5)?, "account_id": r.get::<_, Option<String>>(6)?,
            "linkedin": r.get::<_, Option<String>>(7)?, "twitter": r.get::<_, Option<String>>(8)?,
        }))).optional().ok().flatten()
    };
    const COLS: &str = "id, name, role, company, email, status, account_id, linkedin, twitter";
    let found = if let Some(email) = first_email(to) {
        contact(&format!("SELECT {COLS} FROM crm_contacts WHERE deleted IS NULL AND lower(email) = ?1 LIMIT 1"), &email)
    } else {
        let handle = to.trim().trim_start_matches('@').to_ascii_lowercase();
        let col = match channel {
            "x" | "twitter" => Some("twitter"),
            "linkedin" => Some("linkedin"),
            _ => None,
        };
        col.and_then(|c| {
            let key = if !target.is_empty() && c == "linkedin" { target.to_ascii_lowercase() } else { handle.clone() };
            (!key.is_empty()).then(|| contact(&format!(
                "SELECT {COLS} FROM crm_contacts WHERE deleted IS NULL AND {c} <> '' AND (lower({c}) = ?1 OR instr(?1, lower({c})) > 0 OR instr(lower({c}), ?1) > 0) LIMIT 1"), &key)).flatten()
        })
    };
    let Some(mut c) = found else { return Value::Null };
    if let Some(aid) = c["account_id"].as_str().filter(|a| !a.is_empty()).map(str::to_string) {
        let acct = conn.query_row(
            "SELECT id, name, domain, status, industry FROM crm_accounts WHERE id = ?1 AND deleted IS NULL",
            [&aid],
            |r| Ok(json!({"id": r.get::<_, String>(0)?, "name": r.get::<_, Option<String>>(1)?, "domain": r.get::<_, Option<String>>(2)?,
                "status": r.get::<_, Option<String>>(3)?, "industry": r.get::<_, Option<String>>(4)?})),
        ).optional().ok().flatten();
        c["account"] = acct.unwrap_or(Value::Null);
    }
    let last = conn.query_row(
        "SELECT MAX(date) FROM crm_interactions WHERE contact_id = ?1",
        [c["id"].as_str().unwrap_or("")],
        |r| r.get::<_, Option<String>>(0),
    ).ok().flatten();
    c["last_touch"] = json!(last);
    c
}

// ── The route ─────────────────────────────────────────────────────────────

#[derive(serde::Deserialize, Default)]
pub struct FeedQuery {
    /// One of [`STATES`], or `all` (the default).
    #[serde(default)]
    pub state: Option<String>,
}

/// Every item from every source, newest first, with the count per state.
pub fn build(conn: &Connection, home: &Path, q: &FeedQuery) -> Value {
    let links = links_in(conn);
    let mut items = Vec::new();
    let mut totals = Totals::new();
    email_items(home, &links, &mut items, &mut totals);
    table_totals(conn, &mut totals);
    browser_items(conn, &links, &mut items);
    social_items(conn, &links, &mut items);
    chat_items(conn, &links, &mut items);
    // Counts are over each source's whole record; the list keeps the
    // waiting items and the latest HISTORY decided ones per source.
    let counts: Map<String, Value> = STATES.iter().map(|s| (s.to_string(), json!(totals.get(s).copied().unwrap_or(0)))).collect();
    let n_considered: u64 = totals.values().sum();
    let listed = items.len();
    let mut channels: Map<String, Value> = Map::new();
    for it in &items {
        let st = it["state"].as_str().unwrap_or("");
        let ch = it["channel"].as_str().unwrap_or("").to_string();
        let e = channels.entry(ch).or_insert_with(|| json!({"total": 0, "needs": 0, "via": it["via"], "connector": it["connector"], "connector_label": it["connector_label"]}));
        e["total"] = json!(e["total"].as_u64().unwrap_or(0) + 1);
        if st == "needs" {
            e["needs"] = json!(e["needs"].as_u64().unwrap_or(0) + 1);
        }
    }
    let want = q.state.as_deref().filter(|s| !s.is_empty() && *s != "all");
    let mut items: Vec<Value> = items.into_iter().filter(|it| want.is_none_or(|w| it["state"] == w)).collect();
    // Waiting items oldest first (the longest wait is the most urgent);
    // everything else newest first.
    let key = |it: &Value| it["sent_at"].as_f64().or_else(|| it["decided_at"].as_f64()).unwrap_or_else(|| it["created"].as_f64().unwrap_or(0.0));
    items.sort_by(|a, b| {
        let (an, bn) = (a["state"] == "needs", b["state"] == "needs");
        bn.cmp(&an).then_with(|| if an && bn { key(a).total_cmp(&key(b)) } else { key(b).total_cmp(&key(a)) })
    });
    for it in items.iter_mut() {
        let crm = crm_for(conn, it["channel"].as_str().unwrap_or(""), it["to"].as_str().unwrap_or(""), it["target"].as_str().unwrap_or(""));
        it["crm"] = crm;
    }
    json!({
        "items": items, "counts": counts, "channels": channels, "states": STATES,
        "measured": true, "n_considered": n_considered, "listed": listed,
        "sources": ["email", "chat", "social", "outbound"],
        "history_per_source": HISTORY,
    })
}

async fn feed(State(state): State<AppState>, Query(q): Query<FeedQuery>) -> Response {
    let store = state.store.clone();
    let home = crate::config::amux_home();
    let t0 = std::time::Instant::now();
    let res = tokio::task::spawn_blocking(move || {
        let conn = store.read().map_err(|e| e.to_string())?;
        Ok::<Value, String>(build(&conn, &home, &q))
    })
    .await;
    let msg = match res {
        Ok(Ok(v)) => {
            tracing::debug!(n = v["n_considered"].as_u64().unwrap_or(0), ms = t0.elapsed().as_millis() as u64,
                verdict = "outbound_feed_built", "outbound feed");
            return Json(v).into_response();
        }
        Ok(Err(e)) => e,
        Err(e) => e.to_string(),
    };
    tracing::warn!(error = %msg, verdict = "outbound_feed_failed", "the outbound feed could not be built");
    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"measured": false, "n_considered": 0, "why_unmeasured": msg, "items": [], "counts": {}}))).into_response()
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/outbound/feed", axum::routing::get(feed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        crate::db::migrate::test_memdb()
    }

    #[test]
    fn each_channel_names_how_it_goes_out() {
        assert_eq!(route_of("email", "email"), ("api", "google-gmail", "Gmail"));
        assert_eq!(route_of("chat", "slack").0, "api");
        assert_eq!(route_of("chat", "whatsapp_app").0, "mac_app");
        assert_eq!(route_of("social", "devto").0, "api");
        assert_eq!(route_of("social", "linkedin").0, "browser");
        assert_eq!(route_of("outbound", "x").0, "browser");
    }

    /// The route census (`GET /api/debug/routes`) and the request log read
    /// ROUTE_TABLE, and tests/route_table.rs only scans mod.rs, so a route
    /// declared in this merged sub-router would be unlogged without this.
    #[test]
    fn the_feed_route_is_in_the_route_table() {
        assert!(
            crate::api::request_log::ROUTE_TABLE.iter().any(|r| r.path == "/api/outbound/feed" && r.methods.contains(&"GET")),
            "GET /api/outbound/feed must be in request_log::ROUTE_TABLE"
        );
    }

    #[test]
    fn the_first_address_is_read_from_a_named_list() {
        assert_eq!(first_email("Max <Max@EGYM.com>, d@egym.com").as_deref(), Some("max@egym.com"));
        assert_eq!(first_email("@rio_longacre"), None);
    }

    #[test]
    fn every_source_lands_in_one_list_with_its_state_and_context() {
        let c = conn();
        let tmp = tempfile::tempdir().unwrap();
        // Browser-typed: one held, one sent.
        c.execute_batch(
            "INSERT INTO outbound_requests (worker, channel, action, recipient, host, body, state, created_at)
               VALUES ('bizdev', 'x', 'dm', '@rio', 'x.com', 'Following up on MVS', 'held', 100);
             INSERT INTO outbound_requests (worker, channel, action, recipient, host, body, state, created_at, done_at, result_url)
               VALUES ('bizdev', 'linkedin', 'comment', 'Fei', 'linkedin.com', 'Congrats', 'sent', 50, 60, 'https://linkedin.com/p/1');
             INSERT INTO social_queue (group_id, platform, account, draft, scheduled_at, next_attempt_at, status, session, created_at, updated_at)
               VALUES ('g', 'devto', 'mixpeek', '{\"caption\":\"New on MVS\"}', 9999999999, 9999999999, 'queued', 'marketing', 10, 10);
             INSERT INTO crm_contacts (id, name, company, twitter, created, updated) VALUES ('c1', 'Rio Longacre', 'Signal', 'rio', 1, 1);",
        ).unwrap();
        c.execute(
            "INSERT INTO _amux_approval_cards (approval_ref, kind, item_id, card_id, requester, meta, filed_at)
             VALUES ('outbound:OUT-1', 'outbound', 'OUT-1', 'AMUX-9', 'bizdev', ?1, 100)",
            [json!({"outbound": {"reason": "they asked for the release date", "parent": {"author": "@rio", "text": "when?"}}}).to_string()],
        ).unwrap();
        let v = build(&c, tmp.path(), &FeedQuery::default());
        assert_eq!(v["measured"], true);
        assert_eq!(v["counts"]["needs"], 1);
        assert_eq!(v["counts"]["sent"], 1);
        assert_eq!(v["counts"]["scheduled"], 1);
        let first = &v["items"][0];
        assert_eq!(first["ref"], "outbound:OUT-1", "the waiting item leads");
        assert_eq!(first["reason"], "they asked for the release date");
        assert_eq!(first["parent"]["text"], "when?");
        assert_eq!(first["decidable"], true);
        assert_eq!(first["card"], "AMUX-9");
        assert_eq!(first["via"], "browser");
        assert_eq!(first["crm"]["name"], "Rio Longacre", "an X handle matches the CRM contact");
        let only = build(&c, tmp.path(), &FeedQuery { state: Some("sent".into()) });
        assert_eq!(only["items"].as_array().unwrap().len(), 1);
        assert_eq!(only["items"][0]["result_url"], "https://linkedin.com/p/1");
        assert_eq!(only["counts"]["needs"], 1, "counts cover every state whatever the filter");
    }

    #[test]
    fn counts_cover_the_whole_record_while_the_list_is_capped() {
        let c = conn();
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..(HISTORY + 250) {
            c.execute(
                "INSERT INTO outbound_requests (worker, channel, action, recipient, host, body, state, created_at) VALUES ('w', 'x', 'dm', '@a', 'x.com', ?1, 'sent', ?2)",
                rusqlite::params![format!("m{i}"), i as i64],
            ).unwrap();
        }
        let v = build(&c, tmp.path(), &FeedQuery { state: Some("sent".into()) });
        assert_eq!(v["counts"]["sent"], (HISTORY + 250) as u64, "the count is every sent message");
        assert!(v["items"].as_array().unwrap().len() < HISTORY + 250, "the list is capped");
        assert_eq!(v["n_considered"], (HISTORY + 250) as u64);
    }

    #[test]
    fn a_held_email_is_read_from_its_draft_with_the_full_body() {
        let c = conn();
        let tmp = tempfile::tempdir().unwrap();
        let dir = crate::api::email_approval::approvals_dir(tmp.path());
        let doc = json!({"id": "apr_00000000000000aa", "session": "gtm-engine", "endpoint": "reply", "created": now(),
            "preview": {"to": "max@egym.com", "from": "ethan@mixpeek.com", "subject": "Re: call", "body": "short", "reply_all": true,
                "outbound": {"reason": "answering his question", "parent": {"author": "Max", "text": "where is it live?"}}},
            "payload": {"body": "the whole body", "reply_all": true}});
        std::fs::write(dir.join("apr_00000000000000aa.json"), doc.to_string()).unwrap();
        let mut sent = doc.clone();
        sent["id"] = json!("apr_00000000000000bb");
        std::fs::write(dir.join("apr_00000000000000bb.approved.json"), sent.to_string()).unwrap();
        let v = build(&c, tmp.path(), &FeedQuery::default());
        assert_eq!(v["counts"]["needs"], 1);
        assert_eq!(v["counts"]["sent"], 1);
        let held = v["items"].as_array().unwrap().iter().find(|i| i["state"] == "needs").unwrap();
        assert_eq!(held["text"], "the whole body", "the frozen payload, not the clipped preview");
        assert_eq!(held["action"], "reply all");
        assert_eq!(held["reason"], "answering his question");
        assert_eq!(held["decidable"], true);
        assert_eq!(held["editable"], true);
    }
}
