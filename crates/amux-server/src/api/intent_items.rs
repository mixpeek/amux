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
//! A goal is composed from primitives (A1): a versioned goal contract
//! (`planning.rs`, `_amux_goal_contracts`), an outcome thread routed to a
//! worker, and a schedule running a cheap shell check
//! (`POST /api/intent/goals/{id}/check`). The check fingerprints the goal's
//! inputs and calls a model only when they changed; then the status line is
//! rewritten and, when the change needs work, the goal's worker is asked to
//! prepare it. Goals start on their own (decided 2026-11-12) but only read and
//! draft: the worker is told so, and the first outside action waits for the
//! owner's Approve (`outside_ok`).

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
    type Row = (String, String, String, Option<String>, i64, Option<String>);
    let rows: Vec<Row> = state.store.read().ok().and_then(|c| {
        let mut st = c.prepare("SELECT id, title, session, next_run, created, \
            CASE WHEN sched_type='recurring' THEN schedule_expr END FROM schedules \
            WHERE enabled=1 AND deleted IS NULL AND kind<>'shell' AND sched_type IN ('once','recurring')").ok()?;
        let r = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, Option<String>>(2)?.unwrap_or_default(), r.get(3)?,
            r.get::<_, i64>(4)?, r.get::<_, Option<String>>(5)?))).ok()?;
        Some(r.flatten().collect())
    }).unwrap_or_default();
    let n = rows.len();
    let mut out: Vec<Value> = rows.into_iter()
        .filter(|(_, _, sess, _, _, _)| helpers.contains(sess))
        .filter_map(|(id, title, sess, next, created, every)| {
            let next = next?;
            let when = chrono::NaiveDateTime::parse_from_str(&next, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(&next, "%Y-%m-%dT%H:%M"))
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(&next, "%Y-%m-%d %H:%M:%S")).ok()?;
            (when > now).then(|| json!({"id": id, "title": reminder_title(&title), "when": when.format("%Y-%m-%dT%H:%M").to_string(),
                "worker": sess, "created": created, "every": every.filter(|e| !e.is_empty())}))
        })
        .collect();
    out.sort_by(|a, b| a["when"].as_str().cmp(&b["when"].as_str()));
    Json(json!({"reminders": out, "measured": true, "n_considered": n})).into_response()
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

fn existing_fingerprints(state: &AppState) -> (BTreeSet<String>, Vec<String>) {
    let Ok(c) = state.store.read() else { return (BTreeSet::new(), vec![]) };
    let Ok(mut st) = c.prepare("SELECT kind, fingerprint, title FROM intent_items") else { return (BTreeSet::new(), vec![]) };
    let rows: Vec<(String, String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map(|r| r.flatten().collect()).unwrap_or_default();
    (rows.iter().map(|(k, f, _)| format!("{k}:{f}")).collect(), rows.iter().map(|(_, _, t)| t.clone()).collect())
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

fn item_id(kind: &str) -> String {
    format!("{}{}", if kind == "goal" { "g" } else { "i" }, ulid::Ulid::new().to_string().to_lowercase())
}

/// Store an idea, or store and start a goal.
async fn store_offer(state: &AppState, o: &Offer) -> Result<String, String> {
    let id = item_id(o.kind);
    let (o2, id2) = (o.clone(), id.clone());
    let status = if o.kind == "goal" { "active" } else { "offered" };
    state.store.write_async(move |c| {
        c.execute(
            "INSERT OR IGNORE INTO intent_items (id, kind, tier, title, body, request, status, why, sources, due, status_line, status_at, fingerprint, created_ms, updated_ms) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?14)",
            rusqlite::params![id2, o2.kind, o2.tier, o2.title, o2.body, o2.request, status, o2.why,
                Value::Array(o2.sources.clone()).to_string(), o2.due, o2.status_line,
                (!o2.status_line.is_empty()).then(now_ms), fingerprint(&o2.title), now_ms()],
        )?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await.map_err(|e| e.to_string())?;
    if o.kind == "goal" {
        if let Err(e) = start_goal(state, &id).await {
            tracing::warn!(goal = %id, error = %e, measured = true, n_considered = 1, verdict = "intent_goal_not_started",
                "intent: an inferred goal was stored but could not be started");
        }
    }
    Ok(id)
}

// ---- items ----------------------------------------------------------------------

pub(crate) fn get_item(state: &AppState, id: &str) -> Option<Value> {
    let c = state.store.read().ok()?;
    c.query_row(
        "SELECT id, kind, tier, title, body, request, status, why, sources, due, status_line, status_at, worker, outcome_id, goal_id, schedule_id, outside_ok, created_ms, updated_ms \
         FROM intent_items WHERE id=?1",
        [id],
        item_row,
    ).ok()
}

fn item_row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "tier": r.get::<_, String>(2)?,
        "title": r.get::<_, String>(3)?, "body": r.get::<_, String>(4)?, "request": r.get::<_, String>(5)?,
        "status": r.get::<_, String>(6)?, "why": r.get::<_, String>(7)?,
        "sources": serde_json::from_str::<Value>(&r.get::<_, String>(8)?).unwrap_or(json!([])),
        "due": r.get::<_, Option<String>>(9)?, "status_line": r.get::<_, String>(10)?, "status_at": r.get::<_, Option<i64>>(11)?,
        "worker": r.get::<_, String>(12)?, "outcome_id": r.get::<_, Option<String>>(13)?, "goal_id": r.get::<_, Option<String>>(14)?,
        "schedule_id": r.get::<_, Option<String>>(15)?, "outside_ok": r.get::<_, i64>(16)? != 0,
        "created_ms": r.get::<_, i64>(17)?, "updated_ms": r.get::<_, i64>(18)?,
    }))
}

fn items(state: &AppState, kind: &str, statuses: &[&str]) -> Result<Vec<Value>, String> {
    let c = state.store.read().map_err(|e| e.to_string())?;
    let mut st = c.prepare(
        "SELECT id, kind, tier, title, body, request, status, why, sources, due, status_line, status_at, worker, outcome_id, goal_id, schedule_id, outside_ok, created_ms, updated_ms \
         FROM intent_items WHERE kind=?1 ORDER BY created_ms DESC LIMIT 200",
    ).map_err(|e| e.to_string())?;
    let rows: Vec<Value> = st.query_map([kind], item_row).map_err(|e| e.to_string())?.flatten()
        .filter(|v| statuses.contains(&v["status"].as_str().unwrap_or(""))).collect();
    Ok(rows)
}

async fn set_fields(state: &AppState, id: &str, fields: Vec<(&'static str, rusqlite::types::Value)>) -> Result<(), String> {
    let id = id.to_string();
    state.store.write_async(move |c| {
        for (k, v) in fields {
            c.execute(&format!("UPDATE intent_items SET {k}=?2, updated_ms=?3 WHERE id=?1"), rusqlite::params![id, v, now_ms()])?;
        }
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await.map(|_| ()).map_err(|e| e.to_string())
}

fn txt(s: impl Into<String>) -> rusqlite::types::Value {
    rusqlite::types::Value::Text(s.into())
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
    let o: i64 = c.query_row("SELECT COUNT(*) FROM intent_outcomes WHERE created_ms > ?1", [last], |r| r.get(0)).unwrap_or(0);
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
            let oid = v["outcome"]["id"].as_str().unwrap_or("").to_string();
            logged("idea started", set_fields(&state, &id, vec![("status", txt("started")), ("outcome_id", txt(oid))]).await);
            remember_feedback(&state, &fingerprint(it["title"].as_str().unwrap_or("")),
                format!("Owner STARTED the idea: {}", it["title"].as_str().unwrap_or(""))).await;
            Json(v).into_response()
        }
        Err((code, e)) => bad(code, e),
    }
}

async fn dismiss_idea(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(it) = get_item(&state, &id) else { return bad(StatusCode::NOT_FOUND, format!("no idea {id}")) };
    if let Err(e) = set_fields(&state, &id, vec![("status", txt("dismissed"))]).await {
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

async fn start_goal(state: &AppState, id: &str) -> Result<(), String> {
    let it = get_item(state, id).ok_or("goal vanished")?;
    let title = it["title"].as_str().unwrap_or("").to_string();
    let request = it["request"].as_str().unwrap_or("").to_string();
    // 1. The versioned goal intent (planning.rs).
    let contract = amux_core::harness::GoalContract {
        id: format!("goal_{id}"),
        objective: title.clone(),
        non_goals: vec!["Sending, buying or changing anything outside amux before the owner approves".into()],
        success_metrics: vec![request.clone()],
        performance_requirements: vec![],
        resource_constraints: vec!["Read and draft only until the owner approves the first outside action".into()],
        dependency_policy: "inferred by amux from the owner's requests".into(),
        release_policy: "the owner approves every outside action".into(),
        expected_scope_min: 1,
        expected_scope_max: 1,
        version: 0,
        updated_by: "amux:intent".into(),
        updated_at: chrono::Utc::now(),
    };
    let gid = contract.id.clone();
    state.store.write_async(move |c| {
        crate::db::harness_store::put_goal_contract(c, &contract)?;
        Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
    }).await.map_err(|e| e.to_string())?;
    // 2. Its outcome thread, routed like any request.
    let brief = goal_brief(&title, &request, it["tier"].as_str().unwrap_or(""), it["due"].as_str());
    let started = super::intent_store::start_outcome(state, &brief, None, Some(id.to_string()), Some(title.clone()), it["sources"].clone())
        .await.map_err(|(_, e)| e)?;
    let oid = started["outcome"]["id"].as_str().unwrap_or("").to_string();
    let worker = started["outcome"]["worker"].as_str().unwrap_or("").to_string();
    // 3. The cheap check, on the scheduler.
    let sched = create_check_schedule(state, id, &title).await;
    let mut fields = vec![("goal_id", txt(gid)), ("outcome_id", txt(oid)), ("worker", txt(worker))];
    if let Some(s) = sched {
        fields.push(("schedule_id", txt(s)));
    }
    set_fields(state, id, fields).await?;
    tracing::info!(goal = %id, measured = true, n_considered = 1, verdict = "intent_goal_started",
        "intent: an inferred goal started (read and draft only until the owner approves)");
    Ok(())
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

/// A pending ask: the newest worker post's ASK: line, when the owner has not
/// approved outside actions for this goal.
pub(crate) fn ask_from_posts(posts: &[Value]) -> Option<String> {
    let last = posts.iter().rev().find(|p| p["author_key"].as_str().unwrap_or("").starts_with("worker:"))?;
    last["body"].as_str()?.lines().find_map(|l| l.trim().trim_start_matches(['*', ' ']).strip_prefix("ASK:")).map(|s| s.trim().to_string())
}

fn goal_view(state: &AppState, g: &Value) -> Value {
    let mut g = g.clone();
    if let Some(oid) = g["outcome_id"].as_str() {
        let posts = super::intent_store::posts_of(state, oid);
        if let Some((line, ts)) = status_from_posts(&posts) {
            if ts >= g["status_at"].as_i64().unwrap_or(0) {
                g["status_line"] = json!(line);
                g["status_at"] = json!(ts);
            }
        }
        if !g["outside_ok"].as_bool().unwrap_or(false) {
            g["pending_ask"] = json!(ask_from_posts(&posts));
        }
    }
    g
}

async fn list_goals(State(state): State<AppState>) -> Response {
    first_pass_if_none(&state);
    match items(&state, "goal", &["active", "paused"]) {
        Ok(v) => {
            let n = v.len();
            let goals: Vec<Value> = v.iter().map(|g| goal_view(&state, g)).collect();
            Json(json!({"goals": goals, "measured": true, "n_considered": n})).into_response()
        }
        Err(e) => Json(json!({"goals": [], "measured": false, "n_considered": 0, "why_unmeasured": e})).into_response(),
    }
}

async fn patch_goal(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let Some(g) = get_item(&state, &id) else { return bad(StatusCode::NOT_FOUND, format!("no goal {id}")) };
    let mut fields = vec![];
    if let Some(t) = body["title"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
        fields.push(("title", txt(t.chars().take(90).collect::<String>())));
        // A reworded goal is a new version of its goal contract.
        if let Some(gid) = g["goal_id"].as_str() {
            let (gid, t) = (gid.to_string(), t.to_string());
            logged("store write", state.store.write_async(move |c| {
                if let Ok(Some(mut gc)) = crate::db::harness_store::get_goal_contract(c, &gid) {
                    gc.objective = t;
                    gc.updated_by = "owner".into();
                    gc.updated_at = chrono::Utc::now();
                    crate::db::harness_store::put_goal_contract(c, &gc)?;
                }
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).await);
        }
    }
    if let Some(s) = body["status"].as_str() {
        if !["active", "paused", "done", "dismissed"].contains(&s) {
            return bad(StatusCode::BAD_REQUEST, "status is active, paused, done or dismissed");
        }
        fields.push(("status", txt(s)));
        if let Some(sid) = g["schedule_id"].as_str() {
            let on = if s == "active" { 1 } else { 0 };
            let sid = sid.to_string();
            logged("store write", state.store.write_async(move |c| {
                c.execute("UPDATE schedules SET enabled=?2 WHERE id=?1", rusqlite::params![sid, on])?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            }).await);
        }
    }
    if let Err(e) = set_fields(&state, &id, fields).await {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    Json(json!({"ok": true, "goal": get_item(&state, &id).map(|g| goal_view(&state, &g))})).into_response()
}

async fn approve_goal(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(g) = get_item(&state, &id) else { return bad(StatusCode::NOT_FOUND, format!("no goal {id}")) };
    if let Err(e) = set_fields(&state, &id, vec![("outside_ok", rusqlite::types::Value::Integer(1))]).await {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, e);
    }
    if let Some(oid) = g["outcome_id"].as_str() {
        logged("approval delivered", super::intent_store::start_outcome(&state,
            "I approve what you asked to do for this goal. Go ahead with that one step; any outside email still goes through amux's own approval.",
            Some(oid), None, None, json!([])).await.map_err(|(_, e)| e));
    }
    tracing::info!(goal = %id, measured = true, n_considered = 1, verdict = "intent_goal_approved",
        "intent: the owner approved an inferred goal's first outside action");
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
    if let Some(oid) = g["outcome_id"].as_str() {
        delivered = super::intent_store::start_outcome(&state,
            "Something changed in my email or calendar since your last update on this goal. Check what it means for the goal, \
             prepare what is needed (read and draft only), and give me the new STATUS: line.",
            Some(oid), None, None, json!([])).await.is_ok();
    }
    logged("goal checked", set_fields(&state, &id, vec![("status_at", rusqlite::types::Value::Integer(now_ms()))]).await);
    tracing::info!(goal = %id, delivered, measured = true, n_considered = n, verdict = "intent_goal_changed",
        "intent: a goal's inputs changed; its worker was asked to update it");
    Json(json!({"ok": true, "changed": true, "delivered": delivered, "measured": true, "n_considered": n, "mail_measured": mail_measured})).into_response()
}

#[cfg(test)]
mod tests {
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
