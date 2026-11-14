//! Ideas and Goals (docs/design/intent-first.md, phases 3 and 4).
//!
//! One pass reads what the owner has been asking for (their messages to every
//! worker in `cmd_history`, and their outcomes), the email and calendar items
//! near it, and what they started or dismissed before (memories), and the
//! helper model offers:
//! - **ideas**: the next useful step for something they asked ("I can pull the
//!   Camp Kaylie thread and draft a second nudge, ready for your approval").
//!   One tap starts it as an outcome through the router.
//! - **goals**: a recurring theme worth pursuing without being asked ("Never
//!   miss a school form"), tiered `tracking` (finite, a deadline) or `standing`.
//!
//! Every idea and goal records the sources it came from; one whose sources do
//! not exist is dropped and counted (`dropped_unsourced`), never kept.
//!
//! Both are board cards (A1): an idea is a backlog card tagged `idea`, a goal
//! a card tagged `goal` on its helper's board, `owner_type: human` so nothing
//! dispatches them. A goal also has a conversation (an outcome thread with its
//! helper) and a scheduled shell check (`POST /api/intent/goals/{id}/check`)
//! that fingerprints its inputs and asks the helper only when they changed.
//! The status line is the helper's newest STATUS: line in that conversation.
//! Goals start on their own but only read and draft: an outside email is a
//! held email-API send the owner approves, and any other outside step is an
//! ASK: line the owner answers in the conversation.

use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/intent/ideas", get(list_ideas))
        .route("/api/intent/ideas/refresh", post(refresh))
        .route("/api/intent/ideas/{id}/start", post(start_idea))
        .route("/api/intent/ideas/{id}/dismiss", post(dismiss_idea))
        .route("/api/intent/goals", get(list_goals))
        .route("/api/intent/goals/{id}", axum::routing::patch(patch_goal))
        .route("/api/intent/goals/{id}/check", post(check_goal))
        .route("/api/intent/goals/{id}/approve", post(approve_goal))
        .route("/api/intent/reminders", get(reminders))
        .route("/api/intent/todos", post(add_todo))
}

/// Reminders a helper set for an outcome (`amux wake` one-time schedules on
/// the helpers outcomes are routed to), still to come. Shown in Goals, so a
/// reminder promised in a chat can be seen saved.
async fn reminders(State(state): State<AppState>) -> Response {
    let helpers: BTreeSet<String> = match super::intent_store::all_outcomes(&state) {
        Ok(rows) => rows.into_iter().map(|o| o.worker).filter(|w| !w.is_empty()).collect(),
        Err(e) => return Json(json!({"reminders": [], "measured": false, "n_considered": 0, "why_unmeasured": e})).into_response(),
    };
    let now = chrono::Local::now().naive_local();
    // A reminder is a scheduler entry a helper set with `amux wake`: once, or
    // repeating (`--every`). amux's own checks for goals and ideas are shell
    // schedules and are not reminders.
    type Row = (String, String, String, Option<String>, i64, Option<String>, String);
    let rows: Vec<Row> = state.store.read().ok().and_then(|c| {
        let mut st = c.prepare("SELECT id, title, session, next_run, created, \
            CASE WHEN sched_type='recurring' THEN schedule_expr END, COALESCE(command,'') FROM schedules \
            WHERE enabled=1 AND deleted IS NULL AND kind<>'shell' AND sched_type IN ('once','recurring')").ok()?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default(), r.get(3)?,
            r.get::<_, i64>(4)?, r.get::<_, Option<String>>(5)?, r.get::<_, String>(6)?))).ok()?;
        Some(r.flatten().collect())
    }).unwrap_or_default();
    let n = rows.len();
    let mut out: Vec<Value> = rows.into_iter()
        .filter(|(_, _, sess, _, _, _, _)| helpers.contains(sess))
        .filter_map(|(id, title, sess, next, created, every, command)| {
            let next = next?;
            let when = chrono::NaiveDateTime::parse_from_str(&next, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(&next, "%Y-%m-%dT%H:%M"))
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(&next, "%Y-%m-%d %H:%M:%S")).ok()?;
            (when > now).then(|| json!({"id": id, "title": reminder_title(&title), "when": when.format("%Y-%m-%dT%H:%M").to_string(),
                "worker": sess, "created": created, "every": every.filter(|e| !e.is_empty()),
                "conv": conv_named(&command)}))
        })
        .collect();
    out.sort_by(|a, b| a["when"].as_str().cmp(&b["when"].as_str()));
    let (out, folded) = fold_repeats(out);
    Json(json!({"reminders": out, "folded": folded, "measured": true, "n_considered": n})).into_response()
}

/// The conversation a reminder's prompt names (`amux post 'o:<id>'`, as the
/// brief asks), so a conversation shows its own reminders and not another
/// goal's on the same helper (walkthrough run 2).
pub(crate) fn conv_named(command: &str) -> Option<String> {
    let i = command.find("o:o")?;
    let id: String = command[i + 2..].chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (id.len() > 3).then(|| format!("o:{id}"))
}

/// One reminder per subject in each conversation: a goal's checks setting
/// "Check Ezra's Hudson Prep application" again and again filled Goals with
/// twenty rows (walkthrough). Two conversations that each set the same title
/// keep one each, so each still shows its own (land review). Kept: the
/// soonest; returned: how many folded.
pub(crate) fn fold_repeats(sorted: Vec<Value>) -> (Vec<Value>, usize) {
    let mut seen = BTreeSet::new();
    let before = sorted.len();
    let out: Vec<Value> = sorted.into_iter().filter(|r| {
        let whose = r["conv"].as_str().or(r["worker"].as_str()).unwrap_or("").to_string();
        seen.insert((whose, fingerprint(r["title"].as_str().unwrap_or(""))))
    }).collect();
    let folded = before - out.len();
    (out, folded)
}

/// A wake prompt's first line, as a person reads a reminder: no "Remind Dana
/// to", no instructions to the helper after it.
pub(crate) fn reminder_title(title: &str) -> String {
    let first = title.lines().next().unwrap_or("").trim();
    let t = first.trim_start_matches("wake:").trim().trim_start_matches("Reminder:").trim();
    let cut: String = t.chars().take(90).collect();
    if cut.is_empty() { "Reminder".into() } else { cut }
}

/// This server's own address, baked into its shell schedules: `amux url`
/// would answer for the owner's main server even on a test or demo instance.
fn self_url() -> String {
    format!("https://localhost:{}", crate::config::canonical_port())
}

/// A write or delivery whose failure does not stop the request, said out
/// loud: an `intent_step_failed` WARN names the step (A4).
fn logged<T, E: std::fmt::Display>(step: &str, r: Result<T, E>) {
    if let Err(e) = r {
        tracing::warn!(step, error = %e, measured = true, n_considered = 1, verdict = "intent_step_failed",
            "intent: a step failed; the request went on without it");
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn bad(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({"ok": false, "error": msg.into()}))).into_response()
}

/// A title reduced to its words, so one pass never offers what an earlier
/// pass already offered (or the owner dismissed).
pub(crate) fn fingerprint(title: &str) -> String {
    title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !["the", "and", "for", "your", "with", "from", "this", "that"].contains(w))
        .collect::<Vec<_>>()
        .join("-")
}

// ---- the sources a pass reads ------------------------------------------------

/// One thing the pass may cite, by id: `m:<cmd_history id>` an owner message,
/// `o:<outcome id>` an outcome, `e:<Message-ID>` an email, `c:<event id>` a
/// calendar event.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Src {
    pub id: String,
    pub kind: &'static str,
    pub text: String,
    pub when: String,
}

impl Src {
    fn to_json(&self) -> Value {
        json!({"id": self.id, "kind": self.kind, "text": self.text.chars().take(240).collect::<String>(), "when": self.when})
    }
}

/// The owner's recent messages to any worker. Messages amux wrapped for
/// delivery (a Messages post, an outcome) are skipped here; outcomes are read
/// as outcomes.
fn owner_messages(state: &AppState, limit: usize) -> Vec<Src> {
    let Ok(c) = state.store.read() else { return vec![] };
    let Ok(mut st) = c.prepare(
        "SELECT id, text, ts FROM cmd_history WHERE type='user' AND text NOT LIKE '%amux Messages%' \
         AND length(text) > 12 ORDER BY id DESC LIMIT ?1",
    ) else {
        return vec![];
    };
    st.query_map([limit as i64], |r| {
        let ts: i64 = r.get(2)?;
        Ok(Src {
            id: format!("m:{}", r.get::<_, i64>(0)?),
            kind: "message",
            text: r.get::<_, String>(1)?.chars().take(400).collect(),
            when: chrono::DateTime::from_timestamp_millis(ts).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

fn outcome_sources(state: &AppState, limit: usize) -> Vec<Src> {
    let Ok(rows) = super::intent_store::all_outcomes(state) else { return vec![] };
    rows.into_iter()
        .take(limit)
        .map(|o| {
            let req = o.sources.as_array().and_then(|a| a.iter().find(|s| s["kind"] == "request"))
                .and_then(|s| s["text"].as_str()).unwrap_or("").to_string();
            let posts = super::intent_store::posts_of(state, &o.id);
            let reply = posts.iter().rev().find(|p| p["author_key"].as_str().unwrap_or("").starts_with("worker:"))
                .and_then(|p| p["body"].as_str()).unwrap_or("");
            Src {
                id: format!("o:{}", o.id),
                kind: "outcome",
                text: format!("{} | asked: {} | latest answer: {}", o.title, req, reply.chars().take(300).collect::<String>()),
                when: chrono::DateTime::from_timestamp_millis(o.updated_ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default(),
            }
        })
        .collect()
}

fn calendar_sources(state: &AppState, days_ahead: i64) -> Vec<Src> {
    let Ok(c) = state.store.read() else { return vec![] };
    let today = chrono::Local::now().date_naive();
    let from = (today - chrono::Duration::days(2)).format("%Y-%m-%d").to_string();
    let to = (today + chrono::Duration::days(days_ahead)).format("%Y-%m-%d").to_string();
    let Ok(mut st) = c.prepare(
        "SELECT id, title, start, COALESCE(location,''), COALESCE(description,'') FROM cal_events \
         WHERE (deleted IS NULL OR deleted=0) AND substr(start,1,10) >= ?1 AND substr(start,1,10) <= ?2 ORDER BY start LIMIT 60",
    ) else {
        return vec![];
    };
    st.query_map([from, to], |r| {
        let (t, s, l, d): (String, String, String, String) = (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?);
        Ok(Src {
            id: format!("c:{}", r.get::<_, String>(0)?),
            kind: "event",
            text: format!("{t}{}{}", if l.is_empty() { String::new() } else { format!(" at {l}") },
                if d.is_empty() { String::new() } else { format!(" ({})", d.chars().take(160).collect::<String>()) }),
            when: s,
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

fn mail_src(m: &Value) -> Option<Src> {
    let id = m["message_id"].as_str().or_else(|| m["id"].as_str())?.trim_start_matches('<').trim_end_matches('>').to_string();
    Some(Src {
        id: format!("e:{id}"),
        kind: "email",
        text: format!("From {} | {} | {}", m["from"].as_str().unwrap_or(""), m["subject"].as_str().unwrap_or(""),
            m["snippet"].as_str().unwrap_or("").chars().take(200).collect::<String>()),
        when: m["date"].as_str().unwrap_or("").to_string(),
    })
}

/// Recent mail and mail matching `queries`, from Gmail and file mailboxes.
async fn mail_sources(queries: &[String], recent: usize) -> (Vec<Src>, bool) {
    let home = crate::config::amux_home();
    let client = crate::integrations::email::GmailClient::new_default();
    let accounts = client.connected_accounts();
    let mut rows: Vec<Value> = crate::integrations::local_mailbox::inbox(&home, "", recent, 21.0);
    let mut measured = !accounts.is_empty() || !crate::integrations::local_mailbox::accounts(&home).is_empty();
    for q in queries {
        rows.extend(crate::integrations::local_mailbox::search(&home, q, "", 8, 120));
    }
    for a in &accounts {
        let fut = client.inbox_messages(a, recent.min(25), "", 7.0, None);
        match tokio::time::timeout(std::time::Duration::from_secs(20), fut).await {
            Ok(Ok(v)) => rows.extend(v["messages"].as_array().cloned().unwrap_or_default()),
            _ => measured = false,
        }
        for q in queries.iter().take(4) {
            let gq = format!("{q} newer_than:120d");
            let fut = client.inbox_messages(a, 6, &gq, 0.0, None);
            if let Ok(Ok(v)) = tokio::time::timeout(std::time::Duration::from_secs(20), fut).await {
                rows.extend(v["messages"].as_array().cloned().unwrap_or_default());
            }
        }
    }
    let mut seen = BTreeSet::new();
    let out = rows.iter().filter_map(mail_src).filter(|s| seen.insert(s.id.clone())).take(60).collect();
    (out, measured)
}

/// What the owner started or dismissed before, from memories.
fn feedback(state: &AppState) -> Vec<String> {
    let Ok(c) = state.store.read() else { return vec![] };
    let Ok(mut st) = c.prepare(
        "SELECT content FROM _amux_memories WHERE name LIKE 'intent-idea-%' AND deleted_at IS NULL ORDER BY updated_at DESC LIMIT 30",
    ) else {
        return vec![];
    };
    st.query_map([], |r| r.get::<_, String>(0)).map(|r| r.flatten().collect()).unwrap_or_default()
}

/// Save what the owner did with an idea as a memory, so the next pass learns.
/// Scoped to the `intent-home` group, which no worker is in, so it shapes the
/// pass and never lands in a worker's context.
async fn remember_feedback(state: &AppState, fp: &str, content: String) {
    let name = format!("intent-idea-{}", fp.chars().take(80).collect::<String>());
    let body = json!({"scope": {"level": "group", "id": "intent-home"}, "name": name,
        "content": content, "memory_type": "feedback"});
    let parsed = match serde_json::from_value::<super::memories::CreateMemoryBody>(body) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(error = %e, verdict = "intent_feedback_not_saved", "intent: feedback memory body did not parse");
            return;
        }
    };
    let resp = super::memories::create_memory(State(state.clone()), Json(parsed)).await;
    if !resp.status().is_success() && resp.status() != StatusCode::CONFLICT {
        tracing::warn!(status = resp.status().as_u16(), verdict = "intent_feedback_not_saved", measured = true, n_considered = 1,
            "intent: an idea's start or dismissal could not be saved as a memory");
    }
}

// ---- the pass ------------------------------------------------------------------

fn queries_prompt(requests: &[Src]) -> String {
    let mut p = String::from(
        "These are things a person recently asked an assistant for. For each, write one short email search \
         (2 to 4 plain keywords, no operators) that would find the email threads it is about, such as the earlier \
         thread with the person or organization named. Reply with ONLY a JSON array of strings, at most 6.\n\n",
    );
    for r in requests.iter().take(8) {
        p.push_str(&format!("- {}\n", r.text.chars().take(240).collect::<String>()));
    }
    p
}

pub(crate) fn pass_prompt(srcs: &[Src], existing: &[String], fb: &[String], today: &str) -> String {
    let mut p = format!(
        "You help a busy person stay on top of their life. Today is {today}. Below is what they asked for lately, \
         their recent email and calendar, and what they did with earlier suggestions.\n\
         Offer:\n\
         - ideas: up to 5 concrete next steps an assistant could take NOW for something they asked, each saying what it \
         can do, from what, and the result (for example: \"I can pull the Camp Kaylie thread and draft a second nudge, \
         ready for your approval\"). Prefer ideas that use a specific email or event below.\n\
         - goals: themes that keep coming up (or that they asked to keep up with) worth tracking without being asked. \
         tier \"tracking\" for something finite with a deadline (give due as YYYY-MM-DD), \"standing\" for an ongoing \
         goal (for example \"Never miss a school form\", \"Stay ahead of the family calendar\", \"Plan the week's dinners\"). \
         Give a status_line: one sentence on where it stands now, naming the next thing and what is ready for it; \
         write dates as a person says them (Tue Nov 17).\n\
         An idea is a NEXT step: never restate a request or report what an answer already said; skip anything \
         already done or being worked on in WHAT THEY ASKED FOR. \
         Rules: plain words for someone who is not technical (never worker, agent, session, model, AI). Nothing is sent, bought or changed outside; an idea that \
         would send something drafts it for approval. Every idea and goal must cite the ids of the items it came from in \
         sources (ids exactly as given). Skip anything already offered or dismissed. Return empty lists if nothing fits.\n\
         Reply with ONLY JSON: {{\"ideas\":[{{\"title\":\"...\",\"body\":\"...\",\"request\":\"what to ask the assistant to do, \
         in their voice\",\"why\":\"...\",\"sources\":[\"...\"]}}],\"goals\":[{{\"title\":\"...\",\"tier\":\"tracking|standing\",\
         \"due\":\"YYYY-MM-DD or null\",\"request\":\"what to keep doing, in their voice\",\"status_line\":\"...\",\"why\":\"...\",\
         \"sources\":[\"...\"]}}]}}\n\n"
    );
    let mut by_kind: BTreeMap<&str, Vec<&Src>> = BTreeMap::new();
    for s in srcs {
        by_kind.entry(s.kind).or_default().push(s);
    }
    for (k, label) in [("outcome", "WHAT THEY ASKED FOR (outcomes)"), ("message", "THEIR MESSAGES"), ("email", "EMAIL"), ("event", "CALENDAR")] {
        if let Some(list) = by_kind.get(k) {
            p.push_str(&format!("{label}:\n"));
            for s in list {
                p.push_str(&format!("- [{}] {} {}\n", s.id, s.when, s.text.chars().take(300).collect::<String>()));
            }
            p.push('\n');
        }
    }
    if !existing.is_empty() {
        p.push_str(&format!("ALREADY OFFERED: {}\n", existing.join("; ")));
    }
    if !fb.is_empty() {
        p.push_str("WHAT THEY DID WITH EARLIER IDEAS:\n");
        for f in fb {
            p.push_str(&format!("- {f}\n"));
        }
    }
    p
}

/// One idea or goal the model offered, after validation.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Offer {
    pub kind: &'static str,
    pub tier: String,
    pub title: String,
    pub body: String,
    pub request: String,
    pub why: String,
    pub due: Option<String>,
    pub status_line: String,
    pub sources: Vec<Value>,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct PassCounts {
    pub offered: usize,
    pub dropped_unsourced: usize,
    pub dropped_repeat: usize,
    pub dropped_empty: usize,
}

/// Keep only offers that cite real sources, are new, and say something.
pub(crate) fn validate_offers(raw: &Value, srcs: &[Src], taken: &BTreeSet<String>) -> (Vec<Offer>, PassCounts) {
    let by_id: HashMap<&str, &Src> = srcs.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut out = vec![];
    let mut c = PassCounts::default();
    let mut seen: BTreeSet<String> = taken.clone();
    for (kind, key) in [("idea", "ideas"), ("goal", "goals")] {
        for it in raw[key].as_array().into_iter().flatten() {
            c.offered += 1;
            let st = |k: &str| it[k].as_str().unwrap_or("").trim().to_string();
            let title: String = st("title").chars().take(90).collect();
            if title.is_empty() || (kind == "idea" && st("request").is_empty()) {
                c.dropped_empty += 1;
                continue;
            }
            let sources: Vec<Value> = it["sources"].as_array().into_iter().flatten()
                .filter_map(Value::as_str)
                .filter_map(|id| by_id.get(id.trim()))
                .map(|s| s.to_json())
                .collect();
            if sources.is_empty() {
                c.dropped_unsourced += 1;
                continue;
            }
            let fp = format!("{kind}:{}", fingerprint(&title));
            if !seen.insert(fp) {
                c.dropped_repeat += 1;
                continue;
            }
            let tier = if kind == "goal" {
                if st("tier") == "tracking" { "tracking".to_string() } else { "standing".to_string() }
            } else {
                String::new()
            };
            let due = st("due");
            let due = (kind == "goal" && chrono::NaiveDate::parse_from_str(&due, "%Y-%m-%d").is_ok()).then_some(due);
            out.push(Offer {
                kind,
                tier: if kind == "goal" && due.is_none() && tier == "tracking" { "standing".into() } else { tier },
                title,
                body: st("body").chars().take(400).collect(),
                request: if st("request").is_empty() { st("title") } else { st("request") },
                why: st("why").chars().take(200).collect(),
                due,
                status_line: st("status_line").chars().take(240).collect(),
                sources,
            });
        }
    }
    (out, c)
}

/// Every idea and goal card ever offered (any status), as `kind:fingerprint`,
/// and their titles for the pass prompt, so nothing is offered twice.
fn existing_fingerprints(state: &AppState) -> (BTreeSet<String>, Vec<String>) {
    let all: Vec<Value> = ["idea", "goal"].iter().flat_map(|k| all_items(state, k)).collect();
    (all.iter().map(|v| format!("{}:{}", v["kind"].as_str().unwrap_or(""), fingerprint(v["title"].as_str().unwrap_or("")))).collect(),
     all.iter().map(|v| v["title"].as_str().unwrap_or("").to_string()).collect())
}

/// Run one pass. `focus` is an outcome just started, read first.
pub(crate) async fn run_pass(state: &AppState, reason: &str) -> Value {
    let started = std::time::Instant::now();
    let mut srcs: Vec<Src> = outcome_sources(state, 12);
    srcs.extend(owner_messages(state, 30));
    let requests: Vec<Src> = srcs.iter().take(8).cloned().collect();
    let queries: Vec<String> = if requests.is_empty() {
        vec![]
    } else {
        match super::lookup::helper_answer(&queries_prompt(&requests)).await {
            Ok((_, a)) => extract_array(&a).map(|v| v.iter().filter_map(Value::as_str).map(str::to_string).take(6).collect()).unwrap_or_default(),
            Err(_) => vec![],
        }
    };
    let (mail, mail_measured) = mail_sources(&queries, 25).await;
    srcs.extend(mail);
    srcs.extend(calendar_sources(state, 30));
    if srcs.is_empty() {
        return json!({"ok": true, "measured": true, "n_considered": 0, "created": 0, "reason": reason,
            "why": "nothing to read yet: no requests, messages, email or calendar"});
    }
    let (taken, titles) = existing_fingerprints(state);
    let today = chrono::Local::now().format("%A %Y-%m-%d").to_string();
    let prompt = pass_prompt(&srcs, &titles, &feedback(state), &today);
    let (via, answer) = match super::lookup::helper_answer(&prompt).await {
        Ok(x) => x,
        Err((_, e)) => {
            tracing::warn!(error = %e, measured = false, n_considered = srcs.len(), verdict = "intent_pass_unmeasured",
                "intent: the ideas pass could not reach the helper model");
            return json!({"ok": false, "measured": false, "n_considered": srcs.len(), "why_unmeasured": e, "created": 0});
        }
    };
    let Some(raw) = super::intent::parse_object(&answer) else {
        tracing::warn!(measured = false, n_considered = srcs.len(), verdict = "intent_pass_unparsed",
            "intent: the ideas pass answer was not JSON");
        return json!({"ok": false, "measured": false, "n_considered": srcs.len(), "why_unmeasured": "the model's answer was not JSON",
            "raw": answer.chars().take(400).collect::<String>(), "created": 0});
    };
    let (offers, counts) = validate_offers(&raw, &srcs, &taken);
    let mut created = vec![];
    for o in offers {
        match store_offer(state, &o).await {
            Ok(id) => created.push(json!({"id": id, "kind": o.kind, "title": o.title})),
            Err(e) => tracing::warn!(error = %e, verdict = "intent_offer_not_stored", "intent: an idea or goal could not be stored"),
        }
    }
    tracing::info!(reason, via = %via, created = created.len(), offered = counts.offered, dropped_unsourced = counts.dropped_unsourced,
        dropped_repeat = counts.dropped_repeat, measured = true, n_considered = srcs.len(), verdict = "intent_pass",
        "intent: the ideas and goals pass ran");
    json!({"ok": true, "measured": true, "n_considered": srcs.len(), "mail_measured": mail_measured,
        "created": created.len(), "items": created, "offered": counts.offered,
        "dropped_unsourced": counts.dropped_unsourced, "dropped_repeat": counts.dropped_repeat,
        "dropped_empty": counts.dropped_empty, "queries": queries, "via": via, "reason": reason,
        "took_ms": started.elapsed().as_millis() as u64})
}

fn extract_array(s: &str) -> Option<Vec<Value>> {
    let (a, b) = (s.find('[')?, s.rfind(']')?);
    serde_json::from_str::<Value>(s.get(a..=b)?).ok()?.as_array().cloned()
}

// ---- ideas and goals are board cards ----------------------------------------------
//
// An idea is a backlog card tagged `idea`; a goal is a card tagged `goal` on
// its helper's board. Both are `owner_type: human` (the owner's own), so no
// dispatcher picks them up, prompts the helper about them or promotes them
// (board_drive filters on owner_type agent). The request starting one is the
// card's `next_action`; what it came from is listed in its description; a
// goal's due date is the card's `due`, which makes it Tracking.

const CREATOR: &str = "amux-intent";

/// The card statuses each item status is, both ways.
fn card_status(kind: &str, status: &str) -> &'static str {
    match (kind, status) {
        ("idea", "offered") => "backlog",
        ("idea", "started") => "done",
        ("goal", "active") => "doing",
        ("goal", "paused") => "backlog",
        ("goal", "done") => "done",
        _ => "discarded",
    }
}

fn item_status(kind: &str, card: &str) -> &'static str {
    match (kind, card) {
        ("idea", "backlog" | "todo") => "offered",
        ("idea", "done" | "verified" | "doing" | "review") => "started",
        ("goal", "doing" | "todo" | "review") => "active",
        ("goal", "backlog") => "paused",
        ("goal", "done" | "verified") => "done",
        _ => "dismissed",
    }
}

/// The card's description: what it offers, why, its status line, and the
/// sources it came from, one per line, so a person reading the board sees them
/// and `parse_desc` reads them back.
pub(crate) fn card_desc(body: &str, why: &str, status_line: &str, sources: &[Value]) -> String {
    let mut d = body.trim().to_string();
    if !why.trim().is_empty() {
        d.push_str(&format!("\n\nWhy: {}", why.trim()));
    }
    if !status_line.trim().is_empty() {
        d.push_str(&format!("\n\nNow: {}", status_line.trim()));
    }
    if !sources.is_empty() {
        d.push_str("\n\nFrom:");
        for s in sources {
            let text: String = s["text"].as_str().unwrap_or("").replace('\n', " ").chars().take(200).collect();
            d.push_str(&format!("\n- {} {}: {}", s["kind"].as_str().unwrap_or("message"),
                s["id"].as_str().map(str::to_string).unwrap_or_else(|| s["id"].to_string()), text));
        }
    }
    d
}

/// `card_desc` read back: (body, why, status line, sources).
pub(crate) fn parse_desc(desc: &str) -> (String, String, String, Vec<Value>) {
    let (main, from) = match desc.split_once("\n\nFrom:") { Some((m, f)) => (m, f), None => (desc, "") };
    let mut parts = main.split("\n\n");
    let mut body = vec![];
    let (mut why, mut now) = (String::new(), String::new());
    for p in parts.by_ref() {
        if let Some(w) = p.strip_prefix("Why: ") { why = w.to_string(); }
        else if let Some(n) = p.strip_prefix("Now: ") { now = n.to_string(); }
        else { body.push(p); }
    }
    let sources = from.lines().filter_map(|l| {
        let l = l.strip_prefix("- ")?;
        let (head, text) = l.split_once(": ")?;
        let (kind, id) = head.split_once(' ')?;
        Some(json!({"kind": kind, "id": id, "text": text}))
    }).collect();
    (body.join("\n\n"), why, now, sources)
}

/// Every card of a kind (tag `idea` or `goal`), newest first, as the item
/// the routes return. Archived cards are left out.
fn all_items(state: &AppState, kind: &str) -> Vec<Value> {
    let Ok(c) = state.store.read() else { return vec![] };
    let Ok(mut st) = c.prepare(
        "SELECT i.id, i.title, COALESCE(i.desc,''), i.status, COALESCE(i.session,''), i.due, COALESCE(i.next_action,''), \
         i.created, i.updated FROM issues i JOIN issue_tags t ON t.issue_id = i.id \
         WHERE t.tag = ?1 AND COALESCE(i.archived,0) = 0 AND i.deleted IS NULL ORDER BY i.created DESC LIMIT 300") else {
        return vec![];
    };
    let rows: Vec<Value> = st.query_map([kind], |r| {
        let desc: String = r.get(2)?;
        let (body, why, now, sources) = parse_desc(&desc);
        let card: String = r.get(3)?;
        let due: Option<String> = r.get(5)?;
        let ms = |v: i64| if v < 1_000_000_000_000 { v * 1000 } else { v };
        Ok(json!({
            "id": r.get::<_, String>(0)?, "kind": kind, "title": r.get::<_, String>(1)?, "body": body, "why": why,
            "status_line": now, "sources": sources, "status": item_status(kind, &card), "card_status": card,
            "worker": r.get::<_, String>(4)?, "tier": if due.is_some() { "tracking" } else { "standing" }, "due": due,
            "request": r.get::<_, String>(6)?, "created_ms": ms(r.get::<_, f64>(7).map(|v| v as i64).unwrap_or(0)),
            "updated_ms": ms(r.get::<_, f64>(8).map(|v| v as i64).unwrap_or(0)),
        }))
    }).map(|r| r.flatten().collect()).unwrap_or_default();
    rows
}

pub(crate) fn get_item(state: &AppState, id: &str) -> Option<Value> {
    ["idea", "goal"].iter().flat_map(|k| all_items(state, k)).find(|v| v["id"] == id)
}

fn items(state: &AppState, kind: &str, statuses: &[&str]) -> Result<Vec<Value>, String> {
    Ok(all_items(state, kind).into_iter().filter(|v| statuses.contains(&v["status"].as_str().unwrap_or(""))).collect())
}

/// Every goal card's id, any status.
pub(crate) fn goal_ids(state: &AppState) -> BTreeSet<String> {
    all_items(state, "goal").into_iter().filter_map(|g| g["id"].as_str().map(str::to_string)).collect()
}

/// The helpers of active goals (tidying never stops one).
pub(crate) fn active_goal_workers(state: &AppState) -> Vec<String> {
    items(state, "goal", &["active"]).unwrap_or_default().iter()
        .filter_map(|g| g["worker"].as_str().filter(|w| !w.is_empty()).map(str::to_string)).collect()
}

/// A response body from a board handler called in-process.
async fn body_of(resp: Response) -> (bool, Value) {
    let ok = resp.status().is_success();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap_or_default();
    (ok, serde_json::from_slice(&bytes).unwrap_or(json!({})))
}

/// Create the card through the board's own create (attributed to amux-intent,
/// tagged, so intake never folds it into another card).
async fn create_card(state: &AppState, body: Value) -> Result<String, String> {
    let (ok, v) = body_of(super::board::create_item(State(state.clone()), HeaderMap::new(), Json(body)).await).await;
    if !ok || v.get("card_created") == Some(&json!(false)) {
        return Err(format!("the board did not create the card: {}", v["error"].as_str().unwrap_or(&v.to_string())));
    }
    v["id"].as_str().map(str::to_string).ok_or_else(|| "the board answered with no card id".into())
}

/// Change a card through the board's own update. Done carries its evidence.
async fn patch_card(state: &AppState, id: &str, mut body: Value) -> Result<(), String> {
    if body["status"] == "done" && body.get("evidence").is_none() {
        body["evidence"] = json!("none: the owner closed it from Home");
    }
    if body.get("status").is_some() {
        body["gate_ack"] = json!(true);
    }
    let (ok, v) = body_of(super::board::patch_item(State(state.clone()), Path(id.to_string()),
        super::org::policy_headers(CREATOR), Json(body)).await).await;
    if ok { Ok(()) } else { Err(format!("the board refused the change to {id}: {}", v["error"].as_str().unwrap_or(&v.to_string()))) }
}

/// Store an idea as a backlog card, or a goal as a card on its helper's
/// board (and start it). A title already offered, under any status, is not
/// offered again.
async fn store_offer(state: &AppState, o: &Offer) -> Result<String, String> {
    let fp = fingerprint(&o.title);
    if all_items(state, o.kind).iter().any(|v| fingerprint(v["title"].as_str().unwrap_or("")) == fp) {
        return Err(format!("already offered: {}", o.title));
    }
    let mut body = json!({
        "title": o.title, "desc": card_desc(&o.body, &o.why, &o.status_line, &o.sources),
        "status": card_status(o.kind, if o.kind == "goal" { "active" } else { "offered" }),
        "session": "", "owner_type": "human", "creator": CREATOR, "type": "chore",
        "tags": [o.kind], "next_action": o.request,
    });
    if let Some(d) = o.due.as_deref().filter(|d| !d.is_empty()) {
        body["due"] = json!(d);
    }
    let id = create_card(state, body).await?;
    if o.kind == "goal" {
        if let Err(e) = start_goal(state, &id).await {
            tracing::warn!(goal = %id, error = %e, measured = true, n_considered = 1, verdict = "intent_goal_not_started",
                "intent: an inferred goal was carded but could not be started");
        }
    }
    Ok(id)
}

/// A pass runs when the owner opens Ideas with none offered, on refresh, and
/// on the ideas schedule. `if_new=N` skips the pass unless N owner messages
/// or outcomes arrived since the last one (the cheap check: no model call).
async fn refresh(State(state): State<AppState>, Query(q): Query<HashMap<String, String>>) -> Response {
    if let Some(n) = q.get("if_new").and_then(|v| v.parse::<i64>().ok()) {
        let fresh = new_since_last_pass(&state);
        if fresh < n {
            return Json(json!({"ok": true, "skipped": true, "new_since_last": fresh, "measured": true, "n_considered": fresh,
                "why": format!("{fresh} new request(s) since the last pass; {n} start one")})).into_response();
        }
    }
    let v = PASSES.run_waiting(run_pass(&state, "refresh")).await;
    mark_pass(&state).await;
    Json(v).into_response()
}

fn new_since_last_pass(state: &AppState) -> i64 {
    let Ok(c) = state.store.read() else { return 0 };
    let last: i64 = c.query_row("SELECT value FROM prefs WHERE key='intent_last_pass_ms'", [], |r| r.get::<_, String>(0))
        .ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let m: i64 = c.query_row("SELECT COUNT(*) FROM cmd_history WHERE type='user' AND ts > ?1", [last], |r| r.get(0)).unwrap_or(0);
    // A new outcome is its first record post (`"outcome"` in its attachment).
    let o: i64 = c.query_row(
        "SELECT COUNT(*) FROM conversation_posts WHERE author_key=?1 AND conv LIKE 'o:%' AND ts > ?2 \
         AND attachments LIKE '%\"outcome\"%' AND deleted_at IS NULL",
        rusqlite::params![super::intent_record::AUTHOR, last], |r| r.get(0)).unwrap_or(0);
    m + o
}

async fn mark_pass(state: &AppState) {
    let v = now_ms().to_string();
    logged("store write", state.store.write_async(move |c| {
        c.execute("INSERT INTO prefs (key, value) VALUES ('intent_last_pass_ms', ?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [v])?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await);
}

/// One pass at a time. Two at once each read the same items and both offered
/// the same ideas (the walkthrough recording: two passes 200ms apart, 9 ideas
/// each, "Rafi's zoo slip" twice). A pass asked for while one runs is folded
/// into one more run after it, which sees what the first one made.
static PASSES: Coalescer = Coalescer::new();

/// Runs one job at a time; a run asked for while one is going becomes one
/// more run after it (any number of asks fold into that one).
pub(crate) struct Coalescer {
    lock: tokio::sync::Mutex<()>,
    again: std::sync::atomic::AtomicBool,
}

impl Coalescer {
    pub(crate) const fn new() -> Self {
        Self { lock: tokio::sync::Mutex::const_new(()), again: std::sync::atomic::AtomicBool::new(false) }
    }

    /// Runs `job` now, or folds it into the run after the current one.
    /// Returns false when it was folded.
    pub(crate) async fn run<F, Fut>(&self, job: F) -> bool
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        use std::sync::atomic::Ordering;
        let Ok(_one) = self.lock.try_lock() else {
            self.again.store(true, Ordering::SeqCst);
            return false;
        };
        loop {
            job().await;
            if !self.again.swap(false, Ordering::SeqCst) {
                return true;
            }
        }
    }

    /// Waits its turn, then runs `job` once (an explicit refresh wants its answer).
    pub(crate) async fn run_waiting<T, Fut: std::future::Future<Output = T>>(&self, job: Fut) -> T {
        let _one = self.lock.lock().await;
        job.await
    }
}

/// After an outcome starts: a pass, in the background, so its ideas and any
/// goal it implies show up while the person is still looking.
pub(crate) fn pass_after_outcome(state: &AppState) {
    let st = state.clone();
    tokio::spawn(async move {
        let ran = PASSES.run(|| async {
            let v = run_pass(&st, "outcome_started").await;
            tracing::debug!(created = %v["created"], verdict = "intent_pass_after_outcome", "intent: pass after a new request");
            mark_pass(&st).await;
        }).await;
        if !ran {
            tracing::info!(measured = true, n_considered = 1, verdict = "intent_pass_coalesced",
                "intent: a pass was already running; one more runs after it");
        }
    });
}

/// A person who has never had a pass (new to Home) gets one on first look,
/// in the background, so their email and calendar fill Ideas and Goals.
fn first_pass_if_none(state: &AppState) {
    let ran = state.store.read().ok().and_then(|c| c.query_row(
        "SELECT 1 FROM prefs WHERE key='intent_last_pass_ms'", [], |_| Ok(())).ok()).is_some();
    if !ran {
        static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            tracing::info!(measured = true, n_considered = 0, verdict = "intent_first_pass",
                "intent: first look at Ideas or Goals with no pass yet; starting one");
            pass_after_outcome(state);
        }
    }
}

async fn list_ideas(State(state): State<AppState>) -> Response {
    ensure_ideas_schedule(&state).await;
    first_pass_if_none(&state);
    match items(&state, "idea", &["offered"]) {
        Ok(v) => {
            let n = v.len();
            Json(json!({"ideas": v, "measured": true, "n_considered": n})).into_response()
        }
        Err(e) => Json(json!({"ideas": [], "measured": false, "n_considered": 0, "why_unmeasured": e})).into_response(),
    }
}

async fn start_idea(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(it) = get_item(&state, &id) else { return bad(StatusCode::NOT_FOUND, format!("no idea {id}")) };
    let req = it["request"].as_str().unwrap_or("").to_string();
    let title = it["title"].as_str().map(str::to_string);
    match super::intent_store::start_outcome(&state, &req, None, Some(id.clone()), title, it["sources"].clone()).await {
        Ok(v) => {
            // The card moves to the helper it went to, closed with where the request now lives.
            let oid = v["outcome"]["id"].as_str().unwrap_or("").to_string();
            logged("idea started", patch_card(&state, &id, json!({"status": "done", "session": v["outcome"]["worker"],
                "evidence": format!("none: started as a request in Chat (o:{oid})")})).await);
            remember_feedback(&state, &fingerprint(it["title"].as_str().unwrap_or("")),
                format!("Owner STARTED the idea: {}", it["title"].as_str().unwrap_or(""))).await;
            Json(v).into_response()
        }
        Err((code, e)) => bad(code, e),
    }
}

async fn dismiss_idea(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(it) = get_item(&state, &id) else { return bad(StatusCode::NOT_FOUND, format!("no idea {id}")) };
    if let Err(e) = patch_card(&state, &id, json!({"status": "discarded"})).await {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    remember_feedback(&state, &fingerprint(it["title"].as_str().unwrap_or("")),
        format!("Owner DISMISSED the idea: {}", it["title"].as_str().unwrap_or(""))).await;
    Json(json!({"ok": true})).into_response()
}

// ---- goals ------------------------------------------------------------------------

/// What the goal's worker is told when the goal starts. It reads and drafts
/// only until the owner approves its first outside action.
pub(crate) fn goal_brief(title: &str, request: &str, tier: &str, due: Option<&str>) -> String {
    format!(
        "Keep this goal for me: {title}. {request}\n\n\
         {}Check my email and calendar for what this needs, and keep it ready: read and draft only. \
         Do not send, buy, sign up for, or change anything outside amux; if a step needs that, write the draft \
         and start a line with ASK: saying what you want to do, and wait for my OK. Start every answer with one \
         line beginning STATUS: that says where this stands, naming the next thing and what is ready for it, with dates \
         written as a person says them (Tue Nov 17).",
        match (tier, due) {
            ("tracking", Some(d)) => format!("It has a deadline: {d}. "),
            _ => String::new(),
        }
    )
}

/// Start a goal: its conversation, routed like any request, then the card
/// moves to that helper's board, and a cheap check goes on the scheduler.
async fn start_goal(state: &AppState, id: &str) -> Result<(), String> {
    let it = get_item(state, id).ok_or("goal vanished")?;
    let title = it["title"].as_str().unwrap_or("").to_string();
    let request = it["request"].as_str().unwrap_or("").to_string();
    let brief = goal_brief(&title, &request, it["tier"].as_str().unwrap_or(""), it["due"].as_str());
    let started = super::intent_store::start_outcome(state, &brief, None, Some(id.to_string()), Some(title.clone()), it["sources"].clone())
        .await.map_err(|(_, e)| e)?;
    let worker = started["outcome"]["worker"].as_str().unwrap_or("").to_string();
    if !worker.is_empty() {
        patch_card(state, id, json!({"session": worker})).await?;
    }
    create_check_schedule(state, id, &title).await;
    tracing::info!(goal = %id, worker = %worker, measured = true, n_considered = 1, verdict = "intent_goal_started",
        "intent: an inferred goal started on its helper's board (read and draft only until the owner approves)");
    Ok(())
}

/// A goal's conversation: the outcome its card started.
fn goal_outcome(state: &AppState, id: &str) -> Option<String> {
    super::intent_store::all_outcomes(state).ok()?.into_iter().find(|o| o.item_id.as_deref() == Some(id)).map(|o| o.id)
}

/// A goal's scheduled check, found by the route it calls.
fn goal_schedule(state: &AppState, id: &str) -> Option<String> {
    let c = state.store.read().ok()?;
    c.query_row("SELECT id FROM schedules WHERE deleted IS NULL AND command LIKE ?1 LIMIT 1",
        [format!("%/api/intent/goals/{id}/check%")], |r| r.get(0)).ok()
}

async fn create_schedule(state: &AppState, body: Value) -> Option<String> {
    let parsed: super::schedules::ScheduleBody = serde_json::from_value(body).ok()?;
    let mut h = HeaderMap::new();
    h.insert("x-amux-session", axum::http::HeaderValue::from_static("amux-intent"));
    let resp = super::schedules::create(State(state.clone()), h, Json(parsed)).await;
    let ok = resp.status().is_success();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap_or_default();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    if !ok {
        tracing::warn!(error = %v, verdict = "intent_schedule_not_created", measured = true, n_considered = 1,
            "intent: a scheduled check could not be created");
        return None;
    }
    v["id"].as_str().or_else(|| v["schedule"]["id"].as_str()).map(str::to_string)
}

async fn create_check_schedule(state: &AppState, id: &str, title: &str) -> Option<String> {
    create_schedule(state, json!({
        "title": format!("Goal check: {}", title.chars().take(60).collect::<String>()),
        "kind": "shell",
        "schedule_expr": "every 2h",
        "command": format!("curl -sk --max-time 120 -X POST {}/api/intent/goals/{id}/check", self_url()),
    })).await
}

/// The ideas pass on the scheduler: a shell check every 30 minutes that runs
/// the model only when three new requests arrived (`if_new=3`). Created once.
async fn ensure_ideas_schedule(state: &AppState) {
    let exists = state.store.read().ok().and_then(|c| c.query_row(
        "SELECT 1 FROM schedules WHERE title='Ideas: look for next steps' LIMIT 1", [], |_| Ok(())).ok()).is_some();
    if exists {
        return;
    }
    create_schedule(state, json!({
        "title": "Ideas: look for next steps",
        "kind": "shell",
        "schedule_expr": "every 30m",
        "command": format!("curl -sk --max-time 300 -X POST '{}/api/intent/ideas/refresh?if_new=3'", self_url()),
    })).await;    // Phase 5: idle helpers amux created are stopped once a day.
    create_schedule(state, json!({
        "title": "Home: stop idle helpers",
        "kind": "shell",
        "schedule_expr": "daily at 4:15",
        "command": format!("curl -sk --max-time 120 -X POST '{}/api/intent/tidy?idle_days=7'", self_url()),
    })).await;
}

/// The goal's status line: the newest STATUS: line its worker posted in the
/// goal's thread, else the line the pass wrote. Computed from the thread.
pub(crate) fn status_from_posts(posts: &[Value]) -> Option<(String, i64)> {
    posts.iter().rev().filter(|p| p["author_key"].as_str().unwrap_or("").starts_with("worker:")).find_map(|p| {
        let body = p["body"].as_str().unwrap_or("");
        body.lines().find_map(|l| l.trim().trim_start_matches(['*', '#', ' ']).strip_prefix("STATUS:"))
            .map(|s| (s.trim().trim_matches('*').trim().to_string(), p["ts"].as_i64().unwrap_or(0)))
    })
}

/// A pending ask: the ASK: line of the helper's post, while that post is the
/// newest one (the owner's answer, an OK or anything else, closes it).
pub(crate) fn ask_from_posts(posts: &[Value]) -> Option<String> {
    let last = posts.last().filter(|p| p["author_key"].as_str().unwrap_or("").starts_with("worker:"))?;
    last["body"].as_str()?.lines().find_map(|l| l.trim().trim_start_matches(['*', ' ']).strip_prefix("ASK:")).map(|s| s.trim().to_string())
}

fn goal_view(state: &AppState, g: &Value) -> Value {
    let mut g = g.clone();
    g["schedule_id"] = json!(goal_schedule(state, g["id"].as_str().unwrap_or("")));
    if let Some(oid) = goal_outcome(state, g["id"].as_str().unwrap_or("")) {
        let posts = super::intent_store::posts_of(state, &oid);
        if let Some((line, ts)) = status_from_posts(&posts) {
            g["status_line"] = json!(line);
            g["status_at"] = json!(ts);
        }
        g["pending_ask"] = json!(ask_from_posts(&posts));
        g["outcome_id"] = json!(oid);
    }
    g
}

/// A to-do she asked a helper for: a card on her own board (unassigned,
/// `owner_type: human`, tagged `todo`), made through the board's own create.
/// A helper cannot write to its board (new workers have their board switches
/// off, owner ruling 2026-11-02), so it asks here (owner, 2026-11-13).
/// The caller is the helper (`X-Amux-Session`); its name goes in the card.
async fn add_todo(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let title: String = body["title"].as_str().unwrap_or("").trim().chars().take(120).collect();
    if title.is_empty() {
        return bad(StatusCode::BAD_REQUEST, "say what the to-do is (title)");
    }
    let helper = super::email::hdr_lane(&headers).unwrap_or_default();
    let mut card = json!({"title": title, "status": "todo", "session": "", "owner_type": "human", "creator": CREATOR,
        "type": "chore", "tags": ["todo"],
        "desc": if helper.is_empty() { "Asked for in Chat.".to_string() } else { format!("Asked for in Chat; noted by {helper}.") }});
    if let Some(d) = body["due"].as_str().filter(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok()) {
        card["due"] = json!(d);
    }
    match create_card(&state, card).await {
        Ok(id) => {
            tracing::info!(card = %id, helper = %helper, measured = true, n_considered = 1, verdict = "intent_todo_added",
                "intent: a to-do she asked for is a card on her board");
            Json(json!({"ok": true, "id": id, "title": title})).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, helper = %helper, measured = true, n_considered = 1, verdict = "intent_todo_not_added",
                "intent: a to-do could not be carded");
            bad(StatusCode::BAD_GATEWAY, e)
        }
    }
}

/// Her open to-dos (cards tagged `todo`), newest first.
pub(crate) fn todos(state: &AppState) -> Vec<Value> {
    let Ok(c) = state.store.read() else { return vec![] };
    let Ok(mut st) = c.prepare(
        "SELECT i.id, i.title, i.status, i.due FROM issues i JOIN issue_tags t ON t.issue_id = i.id \
         WHERE t.tag = 'todo' AND i.status IN ('backlog','todo','doing','review') AND COALESCE(i.archived,0) = 0 \
         AND i.deleted IS NULL ORDER BY i.created DESC LIMIT 50") else { return vec![] };
    st.query_map([], |r| Ok(json!({"id": r.get::<_, String>(0)?, "title": r.get::<_, String>(1)?,
        "status": r.get::<_, String>(2)?, "due": r.get::<_, Option<String>>(3)?})))
        .map(|rows| rows.flatten().collect()).unwrap_or_default()
}

async fn list_goals(State(state): State<AppState>) -> Response {
    first_pass_if_none(&state);
    match items(&state, "goal", &["active", "paused"]) {
        Ok(v) => {
            let n = v.len();
            let goals: Vec<Value> = v.iter().map(|g| goal_view(&state, g)).collect();
            Json(json!({"goals": goals, "todos": todos(&state), "measured": true, "n_considered": n})).into_response()
        }
        Err(e) => Json(json!({"goals": [], "measured": false, "n_considered": 0, "why_unmeasured": e})).into_response(),
    }
}

async fn patch_goal(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    if get_item(&state, &id).is_none() {
        return bad(StatusCode::NOT_FOUND, format!("no goal {id}"));
    }
    let mut change = json!({});
    if let Some(t) = body["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
        change["title"] = json!(t.chars().take(90).collect::<String>());
    }
    if let Some(st) = body["status"].as_str() {
        if !["active", "paused", "done", "dismissed"].contains(&st) {
            return bad(StatusCode::BAD_REQUEST, "status is active, paused, done or dismissed");
        }
        change["status"] = json!(card_status("goal", st));
        // Its check runs only while it is active.
        if let Some(sid) = goal_schedule(&state, &id) {
            let on = if st == "active" { 1 } else { 0 };
            logged("store write", state.store.write_async(move |c| {
                c.execute("UPDATE schedules SET enabled=?2 WHERE id=?1", rusqlite::params![sid, on])?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).await);
        }
    }
    if let Err(e) = patch_card(&state, &id, change).await {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    Json(json!({"ok": true, "goal": get_item(&state, &id).map(|g| goal_view(&state, &g))})).into_response()
}

/// The owner's OK for the step a goal's helper asked about (its ASK: line):
/// said in the goal's own conversation, so the helper reads it there. An
/// email still goes through the email API's own approval.
async fn approve_goal(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(oid) = goal_outcome(&state, &id) else { return bad(StatusCode::NOT_FOUND, format!("no goal {id}")) };
    if let Err((code, e)) = super::intent_store::start_outcome(&state,
        "I approve what you asked to do for this goal. Go ahead with that one step; any outside email still goes through amux's own approval.",
        Some(&oid), None, None, json!([])).await {
        return bad(code, e);
    }
    tracing::info!(goal = %id, measured = true, n_considered = 1, verdict = "intent_goal_approved",
        "intent: the owner approved the step a goal's helper asked about");
    Json(json!({"ok": true})).into_response()
}

/// The fingerprint of what a goal watches: the calendar for the next 30
/// days and the mail its sources point at. Same inputs, same fingerprint.
pub(crate) fn inputs_fingerprint(events: &[Src], mail: &[Src]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for s in events.iter().chain(mail.iter()) {
        s.id.hash(&mut h);
        s.text.hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

/// The scheduled check. Cheap when nothing changed: no model call, no worker
/// turn. When the inputs changed, the goal's worker is asked to update it.
async fn check_goal(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(g) = get_item(&state, &id) else { return bad(StatusCode::NOT_FOUND, format!("no goal {id}")) };
    if g["status"] != "active" {
        return Json(json!({"ok": true, "skipped": true, "why": "the goal is not active", "measured": true, "n_considered": 0})).into_response();
    }
    let events = calendar_sources(&state, 30);
    let words: Vec<String> = fingerprint(g["title"].as_str().unwrap_or("")).split('-').filter(|w| w.len() > 3).take(3).map(str::to_string).collect();
    let (mail, mail_measured) = mail_sources(&words, 10).await;
    let fp = inputs_fingerprint(&events, &mail);
    let key = format!("intent_goal_fp_{id}");
    let prev: Option<String> = state.store.read().ok().and_then(|c| c.query_row("SELECT value FROM prefs WHERE key=?1", [&key], |r| r.get(0)).ok());
    let n = events.len() + mail.len();
    if prev.as_deref() == Some(fp.as_str()) {
        return Json(json!({"ok": true, "changed": false, "measured": true, "n_considered": n, "mail_measured": mail_measured})).into_response();
    }
    let fp2 = fp.clone();
    logged("store write", state.store.write_async(move |c| {
        c.execute("INSERT INTO prefs (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", rusqlite::params![key, fp2])?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await);
    let mut delivered = false;
    if let Some(oid) = goal_outcome(&state, &id).as_deref() {
        delivered = super::intent_store::start_outcome(&state,
            "Something changed in my email or calendar since your last update on this goal. Check what it means for the goal, \
             prepare what is needed (read and draft only), and give me the new STATUS: line.",
            Some(oid), None, None, json!([])).await.is_ok();
    }
    tracing::info!(goal = %id, delivered, measured = true, n_considered = n, verdict = "intent_goal_changed",
        "intent: a goal's inputs changed; its worker was asked to update it");
    Json(json!({"ok": true, "changed": true, "delivered": delivered, "measured": true, "n_considered": n, "mail_measured": mail_measured})).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_reminder_names_its_conversation_and_repeats_fold() {
        use super::{conv_named, fold_repeats};
        use serde_json::json;
        assert_eq!(conv_named("Post the reminder here with amux post 'o:o01abc9z' then stop").as_deref(), Some("o:o01abc9z"));
        assert_eq!(conv_named("Post the reminder"), None);
        let r = |t: &str, w: &str| json!({"title": t, "when": w});
        let (out, folded) = fold_repeats(vec![r("Check Ezra's Hudson Prep application", "2026-11-14T09:00"),
            r("Call the camp", "2026-11-15T10:00"), r("Check Ezra's Hudson Prep application!", "2026-11-16T09:00")]);
        assert_eq!(folded, 1);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["when"], "2026-11-14T09:00", "the soonest is kept");
        // The same title in two conversations: one each, so each shows its own.
        let c = |conv: &str| json!({"title": "Check the application", "when": "2026-11-14T09:00", "conv": conv});
        let (out, folded) = fold_repeats(vec![c("o:oa"), c("o:ob"), c("o:oa")]);
        assert_eq!((out.len(), folded), (2, 1));
    }

    #[test]
    fn a_card_description_reads_back_as_the_idea() {
        use super::{card_desc, parse_desc};
        use serde_json::json;
        let src = vec![json!({"kind": "email", "id": "<m1@camp>", "text": "Rafi is #4 on the waitlist"}),
            json!({"kind": "outcome", "id": "o:o1", "text": "camp | asked: call"})];
        let d = card_desc("I can draft a second nudge.", "No reply in 12 days.", "", &src);
        let (body, why, now, sources) = parse_desc(&d);
        assert_eq!((body.as_str(), why.as_str(), now.as_str()), ("I can draft a second nudge.", "No reply in 12 days.", ""));
        assert_eq!(sources, src, "the sources it came from survive the card: {d}");
        let (_, _, now, _) = parse_desc(&card_desc("Keep school forms done.", "", "3 forms open", &[]));
        assert_eq!(now, "3 forms open");
    }

    #[test]
    fn an_ask_waits_only_until_she_answers() {
        use super::ask_from_posts;
        use serde_json::json;
        let w = json!({"author_key": "worker:family", "body": "STATUS: ready\nASK: May I sign Rafi up for the waitlist call?"});
        let me = json!({"author_key": "person:owner", "body": "I approve"});
        assert_eq!(ask_from_posts(std::slice::from_ref(&w)).as_deref(), Some("May I sign Rafi up for the waitlist call?"));
        assert_eq!(ask_from_posts(&[w.clone(), me.clone()]), None, "her answer closes it");
        assert_eq!(ask_from_posts(&[w, me, json!({"author_key": "worker:family", "body": "Done."})]), None, "no new ask");
    }

    #[test]
    fn idea_and_goal_states_map_to_card_statuses_and_back() {
        use super::{card_status, item_status};
        for (kind, st) in [("idea", "offered"), ("idea", "started"), ("idea", "dismissed"),
            ("goal", "active"), ("goal", "paused"), ("goal", "done"), ("goal", "dismissed")] {
            assert_eq!(item_status(kind, card_status(kind, st)), st, "{kind} {st}");
        }
    }

    #[tokio::test]
    async fn overlapping_passes_fold_into_one_more_run() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let c = Arc::new(Coalescer::new());
        let runs = Arc::new(AtomicUsize::new(0));
        let job = |runs: Arc<AtomicUsize>| move || {
            let runs = runs.clone();
            async move {
                runs.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        };
        let first = tokio::spawn({ let (c, r) = (c.clone(), runs.clone()); async move { c.run(job(r)).await } });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        // Three asks while the first runs: none runs alongside it, and they fold into one.
        for _ in 0..3 {
            assert!(!c.run(job(runs.clone())).await, "an overlapping ask is folded");
        }
        assert!(first.await.unwrap());
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the first run, then one more for the folded asks");
        assert!(c.run(job(runs.clone())).await, "with nothing running, it runs");
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    use super::*;

    fn srcs() -> Vec<Src> {
        vec![
            Src { id: "o:o1".into(), kind: "outcome", text: "Call camp director about Rafi's waitlist".into(), when: "2026-11-12".into() },
            Src { id: "e:camp-1@x".into(), kind: "email", text: "From Camp Kaylie | Waitlist".into(), when: "".into() },
            Src { id: "c:EVT-1".into(), kind: "event", text: "Parent-teacher conferences".into(), when: "2026-11-20".into() },
        ]
    }

    #[test]
    fn offers_without_real_sources_are_dropped_and_counted() {
        let raw = json!({"ideas": [
            {"title": "Draft a nudge to Camp Kaylie", "request": "Draft a second nudge", "sources": ["e:camp-1@x", "e:made-up"]},
            {"title": "Book a dentist", "request": "Book it", "sources": ["e:made-up"]},
            {"title": "", "request": "x", "sources": ["o:o1"]},
        ], "goals": [
            {"title": "Never miss a school form", "tier": "standing", "sources": ["c:EVT-1"], "status_line": "Next: conferences"},
        ]});
        let (offers, c) = validate_offers(&raw, &srcs(), &BTreeSet::new());
        assert_eq!(offers.len(), 2);
        assert_eq!(offers[0].sources.len(), 1, "only the real source is kept");
        assert_eq!(c.dropped_unsourced, 1);
        assert_eq!(c.dropped_empty, 1);
        assert_eq!(offers[1].kind, "goal");
        assert_eq!(offers[1].status_line, "Next: conferences");
    }

    #[test]
    fn an_offer_already_made_is_not_made_again() {
        let raw = json!({"ideas": [{"title": "Draft a nudge to Camp Kaylie", "request": "r", "sources": ["o:o1"]},
                                   {"title": "draft a NUDGE to camp kaylie!", "request": "r", "sources": ["o:o1"]}]});
        let taken: BTreeSet<String> = BTreeSet::new();
        let (offers, c) = validate_offers(&raw, &srcs(), &taken);
        assert_eq!(offers.len(), 1);
        assert_eq!(c.dropped_repeat, 1);
        let taken: BTreeSet<String> = [format!("idea:{}", fingerprint("Draft a nudge to Camp Kaylie"))].into();
        assert!(validate_offers(&raw, &srcs(), &taken).0.is_empty(), "a dismissed or earlier idea never returns");
    }

    #[test]
    fn a_tracking_goal_needs_a_real_due_date() {
        let raw = json!({"goals": [
            {"title": "Open enrollment", "tier": "tracking", "due": "2026-12-15", "sources": ["c:EVT-1"]},
            {"title": "Something", "tier": "tracking", "due": "soon", "sources": ["c:EVT-1"]},
        ]});
        let (offers, _) = validate_offers(&raw, &srcs(), &BTreeSet::new());
        assert_eq!((offers[0].tier.as_str(), offers[0].due.as_deref()), ("tracking", Some("2026-12-15")));
        assert_eq!((offers[1].tier.as_str(), offers[1].due.as_deref()), ("standing", None), "no date, no tracking tier");
    }

    #[test]
    fn status_and_asks_come_from_the_goal_thread() {
        let posts = vec![
            json!({"author_key": "person:owner", "body": "STATUS: not mine", "ts": 1}),
            json!({"author_key": "worker:family", "body": "STATUS: Next is the dentist on Tue; the form is filled.\nmore", "ts": 5}),
            json!({"author_key": "worker:family", "body": "Here is the draft.\nASK: May I email the school office?", "ts": 9}),
        ];
        assert_eq!(status_from_posts(&posts), Some(("Next is the dentist on Tue; the form is filled.".into(), 5)));
        assert_eq!(ask_from_posts(&posts).as_deref(), Some("May I email the school office?"));
        assert_eq!(ask_from_posts(&posts[..2]), None, "the newest worker post has no ask");
        // The owner quoting a STATUS line (or pasting one) never becomes the goal's status.
        let mut quoted = posts.clone();
        quoted.push(json!({"author_key": "person:owner", "body": "STATUS: what the owner typed", "ts": 12}));
        assert_eq!(status_from_posts(&quoted).map(|s| s.0), Some("Next is the dentist on Tue; the form is filled.".into()));
    }

    #[test]
    fn the_check_fingerprint_moves_only_with_its_inputs() {
        let a = srcs();
        let fp = inputs_fingerprint(&a[2..], &a[1..2]);
        assert_eq!(fp, inputs_fingerprint(&a[2..], &a[1..2]));
        let mut b = a.clone();
        b[2].text = "Parent-teacher conferences moved".into();
        assert_ne!(fp, inputs_fingerprint(&b[2..], &b[1..2]));
    }

    #[test]
    fn a_reminder_reads_as_a_reminder() {
        assert_eq!(reminder_title("Reminder: call Marcy Feld about Rafi\nThen post it with amux post"), "call Marcy Feld about Rafi");
        assert_eq!(reminder_title(""), "Reminder");
        assert_eq!(reminder_title("wake: Reminder: sign Rafi's slip"), "sign Rafi's slip");
    }

    #[test]
    fn the_goal_brief_says_read_and_draft_only() {
        let b = goal_brief("Never miss a school form", "Watch for forms", "tracking", Some("2026-12-01"));
        assert!(b.contains("read and draft only"));
        assert!(b.contains("ASK:"));
        assert!(b.contains("STATUS:"));
        assert!(b.contains("2026-12-01"));
    }
}
