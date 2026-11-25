//! A voice in the CRM (owner 2026-11-24: "a voice that we're able to capture
//! and adhere to throughout each of these engagements and touchpoints", across
//! email, LinkedIn, X, Slack, handwritten notes and mailed items).
//!
//! A voice belongs to a sender (the owner, or a team voice): a few core lines,
//! hard rules, and per-channel limits, each keyed `channel/action` like a touch
//! (`email/email`, `email/followup` for a later email step, `linkedin/connect`,
//! `post/letter`), with `channel/*` as a channel-wide fallback.
//!   GET  /api/crm/voices, POST {sender, name, rules}
//!   GET  /api/crm/voices/{id}           the voice and its examples
//!   PATCH /api/crm/voices/{id}           {name?, rules?}
//!   POST /api/crm/voices/{id}/check      {channel, action, step?, body}
//!
//! rules = {core: [line], banned: [phrase], no_dashes, no_sign_off, no_reframe
//! (default true), booking_url, wrong_booking: [substring], channels:
//! {"email/email": {max_words?, max_chars?, links?, opens?, ask?, never: [phrase]}}}
//!
//! Every touch text is checked when it is written (crm_agent.rs): a worker's
//! draft that breaks a rule is refused with the problems (422
//! crm_touch_voice) unless it names `voice_override: "<why>"`, which the owner
//! sees on the card; a dead link is refused with no override. A sequence's
//! planned text and the owner's own edits are checked and flagged, never
//! refused. The result is stored on the touch (`voice_check`) and shown on its
//! card. The owner's edit of a drafted touch is kept as an example (the
//! before and after), and so is a text he approves unchanged; the brief
//! carries the newest examples per channel, so drafts learn from his edits.
//! Verdicts: crm_touch_voice_refused, crm_touch_voice_flagged, crm_voice_example_kept.

use super::calendar::query_rows_json;
use super::crm_accounts::{ev, now_secs, source_of};
use super::{internal, AppState};
use crate::db::board_store::next_issue_id;
use crate::db::WriteOutcome;
use amux_core::revision::MutationKind;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/voices", get(list_voices).post(create_voice))
        .route("/voices/{id}", get(get_voice).patch(patch_voice))
        .route("/voices/{id}/check", post(check_route))
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

fn slot<T>(s: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn rules_of(row: &Value) -> Value {
    match &row["rules"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(json!({})),
        v => v.clone(),
    }
}

/// The voice a touch is written in: the one named (id or sender), else the
/// owner's (sender "owner"). None when there is neither: a team voice is
/// used only by the touches that name it.
pub(crate) fn voice_for(conn: &rusqlite::Connection, hint: &str) -> Option<(String, Value)> {
    let q = "SELECT id, rules FROM crm_voices WHERE deleted IS NULL AND (?1<>'' AND (id=?1 OR sender=?1) OR ?1='' AND sender='owner') ORDER BY updated DESC LIMIT 1";
    let pick = |h: &str| conn.query_row(q, [h], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).ok();
    let (id, rules) = pick(hint).or_else(|| pick(""))?;
    Some((id, serde_json::from_str(&rules).unwrap_or(json!({}))))
}

/// The rule key for a touch: an email after step 1 is a follow-up.
pub(crate) fn key_of(channel: &str, action: &str, step: i64) -> String {
    if channel == "email" && action == "email" && step > 1 { "email/followup".into() } else { format!("{channel}/{action}") }
}

fn channel_rules<'a>(rules: &'a Value, key: &str) -> Option<(&'a Value, String)> {
    let ch = &rules["channels"];
    if let Some(r) = ch.get(key).filter(|v| v.is_object()) {
        return Some((r, key.to_string()));
    }
    let wide = format!("{}/*", key.split('/').next().unwrap_or(""));
    ch.get(&wide).filter(|v| v.is_object()).map(|r| (r, wide))
}

fn on(rules: &Value, k: &str) -> bool {
    rules.get(k).and_then(Value::as_bool).unwrap_or(true)
}

fn reframe() -> Option<&'static regex::Regex> {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(concat!(
        r"(?i)\b(?:it'?s|that'?s|this is|isn'?t|is)\s+not\s+(?:just\s+)?[^.,;]{1,40}[,.;]\s*(?:it'?s|that'?s|but)\b",
        r"|\bnot\s+(?:just\s+)?[^.,;]{1,30},\s*but\b",
        r"|\bisn'?t\s+(?:about\s+)?[^.,;]{1,40}\.\s*it'?s\b")).ok()).as_ref()
}

fn sign_off() -> Option<&'static regex::Regex> {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"(?im)^\s*(best|thanks|cheers|regards|best regards|warmly)[,!]?\s*\n+\s*-?\s*[a-z]+\s*$").ok()).as_ref()
}

/// The mechanical rules of a voice for one channel and action. Problems are
/// plain sentences; `key` is the channel rule that applied ("" when none did).
pub(crate) fn lint(rules: &Value, key: &str, text: &str) -> (Vec<String>, String) {
    let mut p = Vec::new();
    let low = text.to_lowercase();
    let cr = channel_rules(rules, key);
    let applied = cr.as_ref().map(|(_, k)| k.clone()).unwrap_or_default();
    let (words, chars) = (text.split_whitespace().count(), text.trim().chars().count());
    if let Some((c, k)) = &cr {
        if let Some(m) = c.get("max_words").and_then(Value::as_u64).filter(|m| words as u64 > *m) {
            p.push(format!("{words} words; {k} is at most {m}"));
        }
        if let Some(m) = c.get("max_chars").and_then(Value::as_u64).filter(|m| chars as u64 > *m) {
            p.push(format!("{chars} characters; {k} is at most {m}"));
        }
        let urls = super::email_links::urls(text);
        if c.get("links").and_then(Value::as_bool) == Some(false) && !urls.is_empty() {
            p.push(format!("{k} carries no links; found {}", urls.len()));
        }
        for n in c.get("never").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if !n.trim().is_empty() && low.contains(&n.to_lowercase()) {
                p.push(format!("{k} never has \"{n}\""));
            }
        }
    }
    if on(rules, "no_dashes") && text.contains(['\u{2014}', '\u{2013}']) {
        p.push("em or en dash".into());
    }
    if on(rules, "no_sign_off") && sign_off().is_some_and(|re| re.is_match(text)) {
        p.push("manual sign-off: the account adds the signature".into());
    }
    if on(rules, "no_reframe") {
        if let Some(m) = reframe().and_then(|re| re.find(text)) {
            p.push(format!("negation reframe (\"not X, it's Y\"): \"{}\"", m.as_str().trim()));
        }
    }
    for b in rules["banned"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !b.trim().is_empty() && low.contains(&b.to_lowercase()) {
            p.push(format!("banned phrase: \"{b}\""));
        }
    }
    if let Some(url) = rules["booking_url"].as_str().filter(|u| !u.is_empty()) {
        for w in rules["wrong_booking"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if !w.is_empty() && low.contains(&w.to_lowercase()) {
                p.push(format!("booking link is {url}; found \"{w}\""));
            }
        }
    }
    (p, applied)
}

/// lint plus the park-time link check (email_links.rs). `ok` is false when a
/// rule broke or a link is dead; `dead_links` says which kind of refusal it is.
pub(crate) async fn check(voice: Option<&(String, Value)>, channel: &str, action: &str, step: i64, text: &str) -> Value {
    let Some((vid, rules)) = voice else {
        let links = super::email_links::check(text).await;
        let dead = links["dead"].as_array().is_some_and(|d| !d.is_empty());
        return json!({"voice": null, "measured": true, "ok": !dead, "problems": [], "dead_links": dead, "links": links,
                      "why": "no voice named and no owner voice; only links were checked"});
    };
    let key = key_of(channel, action, step);
    let (mut problems, applied) = lint(rules, &key, text);
    let links = super::email_links::check(text).await;
    let dead = links["dead"].as_array().cloned().unwrap_or_default();
    for d in &dead {
        problems.push(format!("dead link {} ({})", d["url"].as_str().unwrap_or(""), d["why"].as_str().unwrap_or("")));
    }
    json!({"voice": vid, "key": key, "rule": applied, "measured": true, "ok": problems.is_empty(),
           "problems": problems, "dead_links": !dead.is_empty(), "links": links})
}

/// Keep an example against a voice: the owner's edit (before, after) or a
/// text he approved as written (after only).
pub(crate) fn keep_example(conn: &rusqlite::Connection, voice: &str, key: &str, kind: &str, before: &str, after: &str, touch: &str) -> rusqlite::Result<()> {
    if voice.is_empty() || after.trim().is_empty() || before.trim() == after.trim() && kind == "owner_edit" {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO crm_voice_examples (voice_id, rule_key, kind, before, after, touch_id, created) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![voice, key, kind, before, after, touch, now_secs()],
    )?;
    tracing::info!(target: "amux::crm", voice, key, kind, touch, measured = true, n_considered = 1,
        verdict = "crm_voice_example_kept", "kept an example against the voice");
    Ok(())
}

/// The voice as a drafter needs it: core, rules, and the newest examples per
/// rule key (the owner's edits first).
pub(crate) fn voice_view(conn: &rusqlite::Connection, id: &str, per_key: usize) -> rusqlite::Result<Option<Value>> {
    let Some(mut row) = query_rows_json(conn, "SELECT * FROM crm_voices WHERE id=?1 AND deleted IS NULL", &[&id as &dyn rusqlite::ToSql])?.into_iter().next() else {
        return Ok(None);
    };
    row["rules"] = rules_of(&row);
    let all = query_rows_json(conn,
        "SELECT rule_key, kind, before, after, touch_id, created FROM crm_voice_examples WHERE voice_id=?1 \
         ORDER BY CASE kind WHEN 'owner_edit' THEN 0 ELSE 1 END, created DESC", &[&id as &dyn rusqlite::ToSql])?;
    let mut by: serde_json::Map<String, Value> = serde_json::Map::new();
    let total = all.len();
    for e in all {
        let k = e["rule_key"].as_str().unwrap_or("").to_string();
        let list = by.entry(k).or_insert_with(|| json!([]));
        if let Some(a) = list.as_array_mut().filter(|a| a.len() < per_key) {
            a.push(e);
        }
    }
    row["examples"] = Value::Object(by);
    row["n_examples"] = json!(total);
    Ok(Some(row))
}

/// The voice block of a person's brief: the voice and the rule for each
/// channel the person can be reached on.
pub(crate) fn brief_voice(conn: &rusqlite::Connection, channels: &[Value], person: &Value) -> rusqlite::Result<Value> {
    let Some((id, _)) = voice_for(conn, "") else {
        return Ok(json!({"measured": true, "voice": null, "why": "no owner voice in the CRM yet (POST /api/crm/voices with sender owner)"}));
    };
    let mut v = voice_view(conn, &id, 3)?.unwrap_or(Value::Null);
    let mut kinds: Vec<String> = channels.iter().filter_map(|c| c["kind"].as_str().map(String::from)).collect();
    if person["email"].as_str().is_some_and(|e| !e.is_empty()) {
        kinds.push("email".into());
    }
    kinds.sort();
    kinds.dedup();
    let rules = v["rules"]["channels"].as_object().cloned().unwrap_or_default();
    v["for_this_person"] = Value::Object(rules.into_iter().filter(|(k, _)| kinds.iter().any(|c| k.starts_with(&format!("{c}/")))).collect());
    v["measured"] = json!(true);
    Ok(v)
}

/// The owner's voice for a text written outside the CRM (a held email), as
/// rule problems only: the caller has checked its links already. `slop` is the
/// drafter's attestation that the text went through the /slop pass (owner
/// 2026-11-24: "everything should use my founder voice and be run thru anti
/// slop skill"); a regex covers the banned phrases, and the judgement half of
/// /slop is the drafter's, so the card says when it is not attested.
pub(crate) async fn check_text(state: &AppState, key: &str, text: &str, slop: bool) -> Value {
    let store = state.store.clone();
    let voice = tokio::task::spawn_blocking(move || store.read().ok().and_then(|c| voice_for(&c, ""))).await.ok().flatten();
    let Some((vid, rules)) = voice else {
        return json!({"measured": true, "voice": null, "ok": true, "problems": [], "slop": slop, "why": "no owner voice in the CRM"});
    };
    let (problems, applied) = lint(&rules, key, text);
    json!({"voice": vid, "key": key, "rule": applied, "measured": true, "ok": problems.is_empty(), "problems": problems, "slop": slop})
}

/// One line for an approval card: what the voice found, a worker's reason for
/// sending it anyway, and whether the /slop pass was attested.
pub(crate) fn card_line(v: &Value) -> String {
    if v["measured"] != json!(true) {
        return format!("Voice: not checked ({})", v["why_unmeasured"].as_str().unwrap_or("?"));
    }
    let mut s = if v["voice"].is_null() {
        "Voice: none set".to_string()
    } else if v["ok"] == json!(true) {
        format!("Voice: fits {}", v["voice"].as_str().unwrap_or(""))
    } else {
        format!("Voice: {}", v["problems"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).unwrap_or_default())
    };
    if let Some(o) = v["override"].as_str() {
        s += &format!(" (sent as written because: {o})");
    }
    s += if v["slop"] == json!(true) { " · slop pass attested" } else { " · slop pass NOT attested" };
    s
}

/// A touch text's voice check, read and fetched outside the writer: the
/// voice named (or the default), the channel rule for its step, and links.
pub(crate) async fn check_touch(state: &AppState, hint: &str, channel: &str, action: &str, step: i64, text: &str) -> Value {
    let store = state.store.clone();
    let h = hint.to_string();
    let voice = tokio::task::spawn_blocking(move || store.read().ok().and_then(|c| voice_for(&c, &h))).await.ok().flatten();
    check(voice.as_ref(), channel, action, step, text).await
}

/// What happens to a written text whose check failed: None to store it, or
/// the refusal. A dead link is refused for everyone; a broken rule is refused
/// for a worker that names no `voice_override`, and flagged for the owner.
pub(crate) fn refusal(check: &mut Value, owner: bool, override_why: &str, touch: &str) -> Option<Response> {
    if check["ok"] == json!(true) {
        return None;
    }
    let problems = check["problems"].clone();
    if check["dead_links"] == json!(true) || !owner && override_why.is_empty() {
        tracing::info!(target: "amux::crm", touch, problems = %problems, measured = true, n_considered = 1,
            verdict = "crm_touch_voice_refused", "a touch text broke the voice");
        let why = if check["dead_links"] == json!(true) { "a link in the text does not load" } else { "the text breaks the voice" };
        return Some(err(StatusCode::UNPROCESSABLE_ENTITY, json!({
            "error": format!("{why}: {}. Rewrite it{}; nothing was saved.",
                problems.as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).unwrap_or_default(),
                if check["dead_links"] == json!(true) { "" } else { ", or send voice_override with the reason it should go as written" }),
            "code": "crm_touch_voice", "voice_check": check.clone(),
        })));
    }
    check["flagged"] = json!(true);
    if !override_why.is_empty() {
        check["override"] = json!(override_why);
    }
    tracing::info!(target: "amux::crm", touch, problems = %problems, owner, measured = true, n_considered = 1,
        verdict = "crm_touch_voice_flagged", "a touch text broke the voice and was kept, flagged");
    None
}

/// The check of a text planned inside the writer (a sequence step): the
/// mechanical rules now; links are fetched by the next CRM tick.
pub(crate) fn plan_check(conn: &rusqlite::Connection, channel: &str, action: &str, step: i64, text: &str) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let (vid, rules) = voice_for(conn, "")?;
    let key = key_of(channel, action, step);
    let (problems, applied) = lint(&rules, &key, text);
    Some(json!({"voice": vid, "key": key, "rule": applied, "measured": true, "ok": problems.is_empty(), "problems": problems,
                "flagged": !problems.is_empty(), "dead_links": false,
                "links": {"measured": false, "why_unmeasured": "checked by the next CRM tick", "checked": 0}}).to_string())
}

/// Fetch the links of drafted touches planned inside the writer (plan_check).
pub(crate) async fn recheck_planned(state: &AppState) {
    let store = state.store.clone();
    let rows = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        Ok(query_rows_json(&*store.read()?,
            "SELECT id, channel, action, step, body, json_extract(voice_check,'$.voice') AS voice FROM crm_touches \
             WHERE deleted IS NULL AND status IN ('drafted','planned') AND body<>'' \
               AND json_extract(voice_check,'$.links.measured')=0 ORDER BY created LIMIT 20", &[])?)
    }).await;
    let rows = match rows {
        Ok(Ok(r)) => r,
        other => {
            tracing::warn!(target: "amux::crm", measured = false, n_considered = 0, verdict = "crm_voice_recheck_unreadable", "{other:?}");
            return;
        }
    };
    for t in rows {
        let s = |k: &str| t[k].as_str().unwrap_or("").to_string();
        let mut c = check_touch(state, &s("voice"), &s("channel"), &s("action"), t["step"].as_i64().unwrap_or(1), &s("body")).await;
        if c["ok"] != json!(true) {
            c["flagged"] = json!(true);
        }
        let (id, cs) = (s("id"), c.to_string());
        let tid = id.clone();
        if let Err(e) = state.store.write_async(move |conn| {
            let n = conn.execute("UPDATE crm_touches SET voice_check=?2 WHERE id=?1", rusqlite::params![&id, &cs])?;
            Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_touch", &id, MutationKind::Updated)] } else { vec![] } })
        }).await {
            tracing::warn!(target: "amux::crm", touch = %tid, error = %e, measured = false, n_considered = 1,
                verdict = "crm_voice_recheck_unwritten", "a planned touch's voice check could not be stored");
        }
    }
}

async fn list_voices(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        let mut rows = query_rows_json(&conn,
            "SELECT v.*, (SELECT COUNT(*) FROM crm_voice_examples e WHERE e.voice_id=v.id) AS n_examples FROM crm_voices v WHERE v.deleted IS NULL ORDER BY v.updated DESC", &[])?;
        for r in &mut rows {
            r["rules"] = rules_of(r);
        }
        Ok(rows)
    }).await {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

fn valid_rules(v: &Value) -> Result<String, String> {
    if !v.is_object() {
        return Err("rules is an object {core, banned, channels: {\"email/email\": {max_words, max_chars, links, never}}}".into());
    }
    if let Some(ch) = v.get("channels") {
        let ch = ch.as_object().ok_or("rules.channels is an object keyed channel/action")?;
        for (k, r) in ch {
            if !k.contains('/') || !r.is_object() {
                return Err(format!("rules.channels.{k}: keys are channel/action (or channel/*) and values objects"));
            }
        }
    }
    Ok(v.to_string())
}

async fn create_voice(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let sender = body["sender"].as_str().unwrap_or("owner").trim().to_string();
    let name = body["name"].as_str().unwrap_or("").trim().to_string();
    let rules = match valid_rules(body.get("rules").unwrap_or(&json!({}))) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let source = source_of(&body, &headers);
    let out: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let w = out.clone();
    let res = state.store.write_async(move |conn| {
        let id = next_issue_id(conn, "VOX")?;
        let now = now_secs();
        conn.execute("INSERT INTO crm_voices (id,sender,name,rules,source,created,updated) VALUES (?1,?2,?3,?4,?5,?6,?6)",
            rusqlite::params![&id, &sender, &name, &rules, &source, now])?;
        *slot(&w) = Some(id.clone());
        Ok(WriteOutcome { applied: true, events: vec![ev("crm_voice", &id, MutationKind::Created)] })
    }).await;
    match res {
        Ok(_) => (StatusCode::CREATED, Json(json!({ "ok": true, "id": slot(&out).take() }))).into_response(),
        Err(e) => internal(e),
    }
}

async fn get_voice(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    match tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Value>> { Ok(voice_view(&*store.read()?, &id, 20)?) }).await {
        Ok(Ok(Some(v))) => Json(v).into_response(),
        Ok(Ok(None)) => err(StatusCode::NOT_FOUND, json!({ "error": "no such voice" })),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!(e)),
    }
}

async fn patch_voice(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let name = body["name"].as_str().map(str::to_string);
    let sender = body["sender"].as_str().map(str::to_string);
    let rules = match body.get("rules").map(valid_rules) {
        Some(Err(e)) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
        Some(Ok(r)) => Some(r),
        None => None,
    };
    let res = state.store.write_async(move |conn| {
        let now = now_secs();
        let n = conn.execute(
            "UPDATE crm_voices SET name=COALESCE(?2,name), sender=COALESCE(?3,sender), rules=COALESCE(?4,rules), updated=?5 WHERE id=?1 AND deleted IS NULL",
            rusqlite::params![&id, name, sender, rules, now])?;
        Ok(WriteOutcome { applied: n > 0, events: if n > 0 { vec![ev("crm_voice", &id, MutationKind::Updated)] } else { vec![] } })
    }).await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

async fn check_route(State(state): State<AppState>, Path(id): Path<String>, Json(body): Json<Value>) -> Response {
    let s = |k: &str| body[k].as_str().unwrap_or("").trim().to_lowercase();
    let (channel, action) = (s("channel"), s("action"));
    let text = body["body"].as_str().unwrap_or("").to_string();
    let step = body["step"].as_i64().unwrap_or(1);
    let store = state.store.clone();
    let voice = match tokio::task::spawn_blocking(move || -> anyhow::Result<Option<(String, Value)>> { Ok(voice_for(&*store.read()?, &id)) }).await {
        Ok(Ok(Some(v))) => v,
        Ok(Ok(None)) => return err(StatusCode::NOT_FOUND, json!({ "error": "no such voice" })),
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(anyhow::anyhow!(e)),
    };
    Json(check(Some(&voice), &channel, &action, step, &text).await).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Value {
        json!({
            "banned": ["circling back", "leverage"],
            "booking_url": "https://mixpeek.com/demo", "wrong_booking": ["cal.mixpeek.com"],
            "channels": {
                "email/email": {"max_words": 12},
                "email/followup": {"max_words": 6, "never": ["following up"]},
                "linkedin/connect": {"max_chars": 40, "links": false},
                "post/*": {"max_words": 5, "links": false},
            }
        })
    }

    #[test]
    fn each_rule_names_what_it_found() {
        let r = rules();
        assert_eq!(lint(&r, "email/email", "Saw your post on drift. Curious how you catch it?").0, Vec::<String>::new());
        let (p, k) = lint(&r, "email/followup", "Just following up here with one more thing");
        assert_eq!(k, "email/followup");
        assert!(p.iter().any(|x| x.starts_with("8 words")), "{p:?}");
        assert!(p.iter().any(|x| x.contains("never has \"following up\"")), "{p:?}");
        let p = lint(&r, "linkedin/connect", "See https://x.io for more about it all").0;
        assert!(p.iter().any(|x| x.contains("carries no links")), "{p:?}");
        let p = lint(&r, "email/email", "It's not a tool, it's a habit \u{2014} leverage it.\n\nBest,\nEthan").0;
        for want in ["em or en dash", "manual sign-off", "negation reframe", "banned phrase: \"leverage\""] {
            assert!(p.iter().any(|x| x.contains(want)), "{want} in {p:?}");
        }
        assert!(lint(&r, "email/email", "Book at https://cal.mixpeek.com").0.iter().any(|x| x.contains("booking link is https://mixpeek.com/demo")));
        // A channel-wide rule applies to every action on it.
        let (p, k) = lint(&r, "post/letter", "Great to meet you at the summit last week");
        assert_eq!(k, "post/*");
        assert!(p.iter().any(|x| x.starts_with("9 words")), "{p:?}");
        // No rule for the channel: only the hard rules.
        assert_eq!(lint(&r, "x/dm", "hello there friend, a long note that has no limit").1, "");
    }

    #[test]
    fn a_later_email_step_is_a_followup() {
        assert_eq!(key_of("email", "email", 1), "email/email");
        assert_eq!(key_of("email", "email", 2), "email/followup");
        assert_eq!(key_of("email", "followup", 0), "email/followup", "a follow-up made outside a sequence keys the same rule");
        assert!(super::super::crm_agent::ACTIONS.contains(&"followup"));
        assert_eq!(key_of("linkedin", "dm", 3), "linkedin/dm");
    }

    #[tokio::test]
    async fn a_worker_draft_is_held_to_the_voice_and_the_owners_edit_teaches_it() {
        use crate::api::standing_approvals::tests::{call, test_state};
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = crate::api::crm::routes().merge(crate::api::crm_agent::routes()).merge(crate::api::crm_outbound::routes())
            .merge(routes()).with_state(state.clone());
        let owner = [("authorization", "Bearer owner-secret")];
        let w = [("x-amux-session", "amux-gtm")];
        let (_, p) = call(&app, "POST", "/contacts", &owner, Some(json!({"name": "Ada Lovelace", "email": "ada@acme.io"}))).await;
        let ada = p["id"].as_str().unwrap().to_string();
        let (st, v) = call(&app, "POST", "/voices", &owner, Some(json!({"sender": "owner", "name": "Ethan", "rules": rules()}))).await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
        let vid = v["id"].as_str().unwrap().to_string();

        // A worker's draft that breaks the voice is refused with what broke.
        let long = "We are circling back with a long note about drift in your courses today";
        let (st, r) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": ada, "channel": "email", "action": "email", "body": long}))).await;
        assert_eq!((st, r["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("crm_touch_voice")), "{r}");
        assert!(r["error"].as_str().unwrap().contains("banned phrase: \"circling back\""), "{r}");
        // With a reason it is kept, flagged, and the reason rides the touch.
        let (st, r) = call(&app, "POST", "/touches", &w, Some(json!({"contact_id": ada, "channel": "email", "action": "email", "body": long,
            "voice_override": "quoting their own subject line"}))).await;
        assert_eq!(st, StatusCode::CREATED, "{r}");
        let tid = r["id"].as_str().unwrap().to_string();
        let (_, list) = call(&app, "GET", "/touches", &owner, None).await;
        let row = list.as_array().unwrap().iter().find(|t| t["id"] == json!(tid)).unwrap().clone();
        let vc: Value = serde_json::from_str(row["voice_check"].as_str().unwrap()).unwrap();
        assert_eq!((vc["voice"].as_str(), vc["flagged"].as_bool(), vc["override"].as_str()), (Some(vid.as_str()), Some(true), Some("quoting their own subject line")));

        // The owner rewrites it: stored (flagged if it broke anything), and
        // the before and after become an example of the voice.
        let better = "Saw your drift post. Curious how you catch it?";
        let (st, _) = call(&app, "PATCH", &format!("/touches/{tid}"), &owner, Some(json!({"body": better}))).await;
        assert_eq!(st, StatusCode::OK);
        let (st, _) = call(&app, "POST", &format!("/touches/{tid}/approve"), &owner, None).await;
        assert_eq!(st, StatusCode::OK);
        let (_, view) = call(&app, "GET", &format!("/voices/{vid}"), &owner, None).await;
        let ex = view["examples"]["email/email"].as_array().unwrap();
        assert_eq!(ex[0]["kind"], "owner_edit", "{view}");
        assert_eq!((ex[0]["before"].as_str(), ex[0]["after"].as_str()), (Some(long), Some(better)));
        assert!(ex.iter().any(|e| e["kind"] == "approved" && e["after"] == json!(better)), "{view}");
        // And a drafter reads it in the brief.
        let (_, b) = call(&app, "GET", &format!("/contacts/{ada}/brief"), &w, None).await;
        assert_eq!(b["voice"]["id"], json!(vid), "{b}");
        assert!(b["voice"]["for_this_person"]["email/email"].is_object(), "{b}");
        assert_eq!(b["voice"]["examples"]["email/email"][0]["after"], json!(better));
    }

    #[tokio::test]
    async fn the_check_route_runs_the_rules_for_a_channel() {
        use crate::api::standing_approvals::tests::{call, test_state};
        let state = test_state();
        let app = routes().with_state(state.clone());
        let (_, v) = call(&app, "POST", "/voices", &[], Some(json!({"rules": rules()}))).await;
        let vid = v["id"].as_str().unwrap().to_string();
        let (_, c) = call(&app, "POST", &format!("/voices/{vid}/check"), &[], Some(json!({"channel": "email", "action": "email", "step": 2,
            "body": "Following up here with a quick new example for you"}))).await;
        assert_eq!((c["key"].as_str(), c["ok"].as_bool()), (Some("email/followup"), Some(false)), "{c}");
        let (st, _) = call(&app, "POST", "/voices", &[], Some(json!({"rules": {"channels": {"email": {}}}}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "a channel key names an action");
    }

    #[test]
    fn hard_rules_can_be_turned_off() {
        let r = json!({"no_dashes": false, "no_sign_off": false});
        assert!(lint(&r, "email/email", "a \u{2014} b\n\nThanks,\nEthan").0.is_empty());
    }
}
