//! Event subscriptions, the parts beside the engine in `signals.rs`
//! (docs/design/event-subscriptions.md; Ethan, 2026-11-11: "workers can
//! 'subscribe' to events and there's a list of events across every piece of
//! activity it can listen to and define its own natural language -> filters
//! criteria").
//!
//! - The catalog: every event type actually emitted, with its count, fields
//!   and an example, computed from `session_events` and `board_change_log`.
//! - Filters: a JSON predicate over an event, checked by code on every event.
//! - Compile: the helper model turns plain words into a type pattern and a
//!   filter ONCE, at subscribe time, against the catalog's real fields.
//! - Judge: for fuzzy criteria, the model says yes or no on events the filter
//!   already passed, capped per subscription per hour.
//! - Notify: a post in #general by "amux events" (later fires of the same
//!   subscription reply in its thread) and a push to the owner's devices.
//! - Board CDC rows as `board.card.*` events.

use serde_json::{json, Map, Value};
use std::sync::Arc;

use super::mdai::ModelClient;
use super::AppState;

/// Columns 0157 adds, also added here when missing, so a store that runs only
/// `ensure_subscription_tables` (router tests) has the same shape.
const SUB_COLUMNS: &[(&str, &str)] = &[
    ("criteria", "TEXT NOT NULL DEFAULT ''"),
    ("filter", "TEXT NOT NULL DEFAULT ''"),
    ("owner", "TEXT NOT NULL DEFAULT ''"),
    ("judge", "INTEGER NOT NULL DEFAULT 0"),
    ("dedupe_key", "TEXT NOT NULL DEFAULT ''"),
    ("notify_post", "INTEGER"),
];
const FIRE_COLUMNS: &[(&str, &str)] = &[("event_id", "INTEGER"), ("dedupe_key", "TEXT NOT NULL DEFAULT ''")];

pub(crate) fn ensure_columns(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    for (table, cols) in [("signal_subscriptions", SUB_COLUMNS), ("signal_fires", FIRE_COLUMNS)] {
        let have: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<Result<_, _>>()?;
        for (name, ty) in cols {
            if !have.iter().any(|h| h == name) {
                // Two connections can race to add the same column; the loser's
                // "duplicate column" is the outcome both wanted.
                if let Err(e) = conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {name} {ty}"), []) {
                    if !e.to_string().contains("duplicate column") {
                        tracing::warn!(target: "amux::signals", table, column = name, error = %e,
                            verdict = "subscription_column_add_failed", "could not add an event-subscription column");
                        return Err(e);
                    }
                }
            }
        }
    }
    conn.execute(
        "CREATE TABLE IF NOT EXISTS event_dispatch_cursor (name TEXT PRIMARY KEY, last_id INTEGER NOT NULL, updated_at INTEGER NOT NULL)",
        [],
    )?;
    Ok(())
}

// ---- the event as a filter sees it ------------------------------------------

/// One flat object for filters and templates: `type`, `session`, `source`
/// and every top-level field of `data` (data wins only where it does not
/// collide with those three).
pub(crate) fn event_view(event_type: &str, session: &str, source: &str, data: &Value) -> Value {
    let mut m = Map::new();
    if let Some(o) = data.as_object() {
        for (k, v) in o {
            m.insert(k.clone(), v.clone());
        }
    }
    m.insert("type".into(), json!(event_type));
    m.insert("session".into(), json!(session));
    m.insert("source".into(), json!(source));
    Value::Object(m)
}

fn field<'a>(ev: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = ev;
    for part in path.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn as_num(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Whether `ev` passes `filter`. An empty or null filter passes everything.
/// Shapes: `{"all": [..]}`, `{"any": [..]}`, `{"not": f}`, and a condition
/// `{"field": "reason", "op": "eq", "value": "captcha"}` with ops `eq`, `ne`,
/// `in`, `contains` (case-insensitive), `regex`, `gt`, `lt` and `exists`.
/// A filter this code cannot read fails closed: it matches nothing.
pub(crate) fn filter_matches(filter: &Value, ev: &Value) -> bool {
    match filter {
        Value::Null => true,
        Value::Object(o) if o.is_empty() => true,
        Value::Object(o) => {
            if let Some(all) = o.get("all").and_then(Value::as_array) {
                return all.iter().all(|f| filter_matches(f, ev));
            }
            if let Some(any) = o.get("any").and_then(Value::as_array) {
                return any.iter().any(|f| filter_matches(f, ev));
            }
            if let Some(not) = o.get("not") {
                return !filter_matches(not, ev);
            }
            let (Some(path), Some(op)) = (o.get("field").and_then(Value::as_str), o.get("op").and_then(Value::as_str)) else {
                return false;
            };
            let want = o.get("value").unwrap_or(&Value::Null);
            let got = field(ev, path);
            match op {
                "exists" => got.is_some_and(|g| !g.is_null()),
                "eq" => got.is_some_and(|g| as_text(g).eq_ignore_ascii_case(&as_text(want))),
                // A field the event does not have is not "different": it fails,
                // like every other value operator.
                "ne" => got.is_some_and(|g| !g.is_null() && !as_text(g).eq_ignore_ascii_case(&as_text(want))),
                "in" => got.is_some_and(|g| {
                    want.as_array().is_some_and(|a| a.iter().any(|w| as_text(g).eq_ignore_ascii_case(&as_text(w))))
                }),
                "contains" => got.is_some_and(|g| as_text(g).to_lowercase().contains(&as_text(want).to_lowercase())),
                "regex" => got.is_some_and(|g| {
                    regex::RegexBuilder::new(&as_text(want)).case_insensitive(true).size_limit(1 << 16).build()
                        .is_ok_and(|re| re.is_match(&as_text(g)))
                }),
                "gt" => got.and_then(as_num).zip(as_num(want)).is_some_and(|(g, w)| g > w),
                "lt" => got.and_then(as_num).zip(as_num(want)).is_some_and(|(g, w)| g < w),
                _ => false,
            }
        }
        _ => false,
    }
}

/// `{{field}}` placeholders filled from the event view; unknown ones become
/// empty, so a dedupe key never carries a literal `{{...}}`.
pub(crate) fn fill(template: &str, ev: &Value) -> String {
    let mut out = String::new();
    let mut rest = template;
    while let Some(i) = rest.find("{{") {
        out.push_str(&rest[..i]);
        let after = &rest[i + 2..];
        let Some(j) = after.find("}}") else {
            out.push_str(&rest[i..]);
            return out;
        };
        let key = after[..j].trim();
        if key == "data" {
            out.push_str(&ev.to_string());
        } else {
            out.push_str(&field(ev, key).map(as_text).unwrap_or_default());
        }
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    out
}

// ---- the board as events ------------------------------------------------------

/// A `board_change_log` row as an event: `board.card.created`, `.moved` (the
/// status changed), `.updated` or `.deleted`. `session` is the card's lane.
pub(crate) fn board_event(conn: &rusqlite::Connection, row_id: &str, op: &str, old_status: Option<&str>,
    new_status: Option<&str>, changed_by: Option<&str>) -> (String, String, Value) {
    let (title, session, item_type) = conn
        .query_row("SELECT title, COALESCE(session,''), COALESCE(type,'') FROM issues WHERE id = ?1", [row_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        .unwrap_or_default();
    let kind = match op {
        "INSERT" => "created",
        "DELETE" => "deleted",
        _ if old_status.is_some() && old_status != new_status => "moved",
        _ => "updated",
    };
    let data = json!({
        "id": row_id, "title": title, "lane": session, "card_type": item_type,
        "old_status": old_status, "status": new_status, "changed_by": changed_by,
    });
    (format!("board.card.{kind}"), session, data)
}

// ---- the catalog --------------------------------------------------------------

/// What each common type means. A type that is emitted and not listed here is
/// reported `documented: false`, so the catalog itself shows the drift.
const MEANINGS: &[(&str, &str)] = &[
    ("browser.blocked", "a browser route landed on a page it cannot get past by itself: reason captcha or bot_check, with provider, url, profile and worker"),
    ("browser.route.start", "a worker started a browser route (profile, url)"),
    ("browser.route.action", "a worker acted in a browser route (click, type, key) and the page it ended on"),
    ("browser.route.state", "a worker read a browser route's page"),
    ("browser.route.status", "a browser route's status was read"),
    ("browser.started", "a browser was launched for a profile"),
    ("browser.stopped", "a browser was stopped"),
    ("browser.navigated", "a browser went to a url"),
    ("board.card.created", "a board card was created (id, title, lane, status)"),
    ("board.card.moved", "a board card changed status (old_status, status)"),
    ("board.card.updated", "a board card was edited without a status change"),
    ("board.card.deleted", "a board card was deleted"),
    ("session.native_status", "a hook from a worker's own CLI: event is UserPromptSubmit (a turn started), PreToolUse, PostToolUse, or Stop (it finished its turn); session is the worker's name"),
    ("session.status_decided", "the status amux decided for a worker"),
    ("session.desc_updated", "a worker's description changed (from, to, by)"),
    ("session.context_low", "a worker is low on context"),
    ("message.sent", "a message was sent to a worker"),
    ("message.queued", "a message was queued for a busy worker"),
    ("message.delivered", "a queued message reached its worker"),
    ("deploy.wake", "a new build went live and a worker was told to check its commits"),
    ("chat.message", "a message in a worker's Chat tab"),
    ("task.renamed", "a worker's card title changed"),
];

pub(crate) fn meaning(event_type: &str) -> Option<&'static str> {
    MEANINGS.iter().find(|(t, _)| *t == event_type).map(|(_, m)| *m)
}

const SAMPLES_PER_TYPE: i64 = 20;

/// Every type emitted in the last `days`, busiest first: count, last seen,
/// the field names in its recent `data`, one example and its meaning.
pub(crate) fn catalog(conn: &rusqlite::Connection, days: i64) -> rusqlite::Result<Value> {
    let since = crate::config::now_f64() - (days * 86_400) as f64;
    let mut stmt = conn.prepare(
        "SELECT type, COUNT(*), MAX(ts) FROM session_events WHERE ts >= ?1 GROUP BY type ORDER BY COUNT(*) DESC",
    )?;
    let types: Vec<(String, i64, f64)> =
        stmt.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<_, _>>()?;
    let mut sample = conn.prepare(
        "SELECT session, source, data FROM session_events WHERE type = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let mut out = Vec::new();
    let mut considered = 0i64;
    for (t, n, last) in &types {
        considered += n;
        let rows: Vec<(String, String, Option<String>)> = sample
            .query_map(rusqlite::params![t, SAMPLES_PER_TYPE], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<_, _>>()?;
        let mut fields = std::collections::BTreeSet::new();
        let mut example = Value::Null;
        for (session, source, data) in &rows {
            let d: Value = data.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
            if let Some(o) = d.as_object() {
                fields.extend(o.keys().cloned());
            }
            if example.is_null() {
                example = clip(event_view(t, session, source, &d));
            }
        }
        out.push(entry(t, *n, *last as i64, fields, example));
    }
    // The board, from its change log (changed_at is seconds).
    let mut board = conn.prepare(
        "SELECT operation, (old_status IS NOT NULL AND COALESCE(old_status,'') <> COALESCE(new_status,'')), COUNT(*), MAX(changed_at) \
         FROM board_change_log WHERE changed_at >= ?1 GROUP BY 1, 2",
    )?;
    let rows: Vec<(String, bool, i64, f64)> = board
        .query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<Result<_, _>>()
        .unwrap_or_default();
    let mut by_kind: std::collections::BTreeMap<&str, (i64, i64)> = Default::default();
    for (op, moved, n, last) in rows {
        let kind = match op.as_str() {
            "INSERT" => "board.card.created",
            "DELETE" => "board.card.deleted",
            _ if moved => "board.card.moved",
            _ => "board.card.updated",
        };
        let e = by_kind.entry(kind).or_insert((0, 0));
        e.0 += n;
        e.1 = e.1.max(last as i64);
    }
    let board_fields: std::collections::BTreeSet<String> =
        ["id", "title", "lane", "card_type", "old_status", "status", "changed_by"].iter().map(|s| s.to_string()).collect();
    for (kind, (n, last)) in by_kind {
        considered += n;
        out.push(entry(kind, n, last, board_fields.clone(), Value::Null));
    }
    // A type that exists in code but has not fired yet still belongs in the
    // list a subscriber picks from.
    for (t, _) in MEANINGS {
        if !out.iter().any(|e| e["type"] == *t) {
            out.push(entry(t, 0, 0, Default::default(), Value::Null));
        }
    }
    Ok(json!({
        "measured": true, "n_considered": considered, "days": days,
        "types": out, "count": out.len(),
        "undocumented": out.iter().filter(|e| e["documented"] == false).count(),
    }))
}

fn entry(t: &str, n: i64, last: i64, fields: std::collections::BTreeSet<String>, example: Value) -> Value {
    let mut fields: Vec<String> = fields.into_iter().collect();
    for base in ["type", "session", "source"] {
        if !fields.iter().any(|f| f == base) {
            fields.push(base.into());
        }
    }
    json!({
        "type": t, "count": n, "last_ts": last, "fields": fields, "example": example,
        "meaning": meaning(t), "documented": meaning(t).is_some(),
    })
}

/// An example small enough to read: long strings cut at 160 characters.
fn clip(v: Value) -> Value {
    match v {
        Value::String(s) if s.chars().count() > 160 => Value::String(format!("{}…", s.chars().take(160).collect::<String>())),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, v)| (k, clip(v))).collect()),
        Value::Array(a) => Value::Array(a.into_iter().take(5).map(clip).collect()),
        other => other,
    }
}

/// Recent events, newest last, for `amux events tail`: rows after `after_id`
/// whose type matches `pattern` (a glob, `*` for all).
pub(crate) fn recent(conn: &rusqlite::Connection, pattern: &str, after_id: i64, limit: i64) -> rusqlite::Result<Vec<Value>> {
    let after = if after_id <= 0 {
        conn.query_row("SELECT COALESCE(MAX(id),0) FROM session_events", [], |r| r.get::<_, i64>(0))? - 200
    } else {
        after_id
    };
    let mut stmt = conn.prepare(
        "SELECT id, ts, session, type, source, data FROM session_events WHERE id > ?1 ORDER BY id ASC LIMIT 2000",
    )?;
    let rows = stmt.query_map([after], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?,
            r.get::<_, String>(4)?, r.get::<_, Option<String>>(5)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, ts, session, t, source, data) = row?;
        if !super::signals::glob_matches(pattern, &t) {
            continue;
        }
        let d: Value = data.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
        out.push(json!({"id": id, "ts": ts, "type": t, "session": session, "source": source, "data": clip(d)}));
        if out.len() as i64 >= limit {
            break;
        }
    }
    Ok(out)
}

/// How many of the last `days` of events a pattern and filter would have
/// matched, with up to three of them: the evidence shown before a
/// subscription is saved, and the number that says a filter is not silent.
pub(crate) fn backtest(conn: &rusqlite::Connection, pattern: &str, filter: &Value, days: i64) -> rusqlite::Result<Value> {
    let since = crate::config::now_f64() - (days * 86_400) as f64;
    let types: Vec<String> = conn
        .prepare("SELECT DISTINCT type FROM session_events WHERE ts >= ?1")?
        .query_map([since], |r| r.get(0))?
        .collect::<Result<Vec<String>, _>>()?
        .into_iter()
        .filter(|t| super::signals::glob_matches(pattern, t))
        .collect();
    let mut stmt = conn.prepare(
        "SELECT id, ts, session, source, data FROM session_events WHERE type = ?1 AND ts >= ?2 ORDER BY id DESC LIMIT 5000",
    )?;
    let (mut scanned, mut matched) = (0i64, 0i64);
    let mut examples = Vec::new();
    for t in &types {
        let rows: Vec<(i64, f64, String, String, Option<String>)> = stmt
            .query_map(rusqlite::params![t, since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
            .collect::<Result<_, _>>()?;
        for (id, ts, session, source, data) in rows {
            scanned += 1;
            let d: Value = data.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
            let ev = event_view(t, &session, &source, &d);
            if filter_matches(filter, &ev) {
                matched += 1;
                if examples.len() < 3 {
                    examples.push(json!({"id": id, "ts": ts, "event": clip(ev)}));
                }
            }
        }
    }
    // Board events live in the board's change log, not the ledger: a
    // board.card.* subscription read "0 of 0" over a busy board (live, 2026-11-11).
    let mut types = types;
    if ["board.card.created", "board.card.moved", "board.card.updated", "board.card.deleted"]
        .iter().any(|t| super::signals::glob_matches(pattern, t))
    {
        let mut stmt = conn.prepare(
            "SELECT seq, changed_at, row_id, operation, old_status, new_status, changed_by FROM board_change_log \
             WHERE table_name = 'issues' AND changed_at >= ?1 ORDER BY seq DESC LIMIT 5000",
        )?;
        type Raw = (i64, f64, String, String, Option<String>, Option<String>, Option<String>);
        let rows: Vec<Raw> = stmt
            .query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
            .collect::<Result<_, _>>()?;
        for (seq, ts, id, op, old, new, by) in rows {
            let (t, session, data) = board_event(conn, &id, &op, old.as_deref(), new.as_deref(), by.as_deref());
            if !super::signals::glob_matches(pattern, &t) {
                continue;
            }
            scanned += 1;
            if !types.contains(&t) {
                types.push(t.clone());
            }
            let ev = event_view(&t, &session, "board", &data);
            if filter_matches(filter, &ev) {
                matched += 1;
                if examples.len() < 3 {
                    examples.push(json!({"id": seq, "ts": ts, "event": clip(ev)}));
                }
            }
        }
    }
    Ok(json!({"measured": true, "n_considered": scanned, "types": types, "matched": matched, "examples": examples, "days": days}))
}

// ---- compile: words to a pattern and a filter ----------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Compiled {
    pub pattern: String,
    pub filter: Value,
    pub summary: String,
    pub judge: bool,
    pub dedupe_key: String,
}

pub(crate) fn compile_prompt(criteria: &str, catalog: &Value) -> String {
    let mut lines = Vec::new();
    for t in catalog["types"].as_array().into_iter().flatten() {
        let fields: Vec<&str> = t["fields"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        // A real example shows what the values look like: without one the
        // model read "mixpeek-general finishes its turn" as session_id =
        // mixpeek-general and native_status = idle (live, 2026-11-11), fields
        // that hold a uuid and a boolean.
        let example = if t["example"].is_null() { String::new() } else {
            let e = t["example"].to_string();
            format!("; example {}", e.chars().take(300).collect::<String>())
        };
        lines.push(format!(
            "- {} ({} in 7 days){}: fields {}{}",
            t["type"].as_str().unwrap_or(""),
            t["count"],
            t["meaning"].as_str().map(|m| format!(", {m}")).unwrap_or_default(),
            fields.join(", "),
            example
        ));
    }
    format!(
        "Turn a request to be told about something into an event subscription for amux.\n\n\
         Request: {criteria}\n\n\
         Event types amux emits, with their fields and a real example:\n{}\n\n\
         In every event, `session` is the amux name of the worker it is about (such as \"backend\"), and \
         `type` is the event type. Match a worker by `session`, never by an id field. When several types \
         carry the fields you need (a page's url or title is on every browser.route.* event), use the prefix \
         pattern rather than the busiest single type.\n\n\
         Answer with one JSON object inside <json></json> and nothing else:\n\
         {{\"pattern\": an event type from the list, or a prefix ending in \".*\",\n\
          \"filter\": a predicate over the fields, built from {{\"all\": [..]}}, {{\"any\": [..]}}, {{\"not\": f}} and \
         {{\"field\": name, \"op\": eq|ne|in|contains|regex|gt|lt|exists, \"value\": v}}, or {{}} for every event of the pattern,\n\
          \"summary\": one plain sentence saying what will match,\n\
          \"judge\": true only if the request needs judgement no filter can express,\n\
          \"dedupe_key\": a template such as \"{{{{session}}}}|{{{{url}}}}\" naming what makes two events the same occurrence, or \"\"}}\n\
         Use only field names from the list. If nothing in the list fits, answer <json>{{\"error\": \"why\"}}</json>.",
        lines.join("\n")
    )
}

/// The model's answer, checked against the catalog: the pattern must match a
/// type in it, and every field the filter reads must be one of those types'
/// fields. A wrong compile is refused here rather than saved as a filter that
/// silently matches nothing.
pub(crate) fn parse_compiled(out: &str, catalog: &Value) -> Result<Compiled, String> {
    let v = answer_json(out).ok_or_else(|| {
        format!("the model's answer held no JSON object: {:?}", out.chars().take(200).collect::<String>())
    })?;
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        return Err(format!("no event fits: {e}"));
    }
    let pattern = v["pattern"].as_str().unwrap_or("").trim().to_string();
    let types: Vec<&Value> = catalog["types"].as_array().into_iter().flatten()
        .filter(|t| super::signals::glob_matches(&pattern, t["type"].as_str().unwrap_or(""))).collect();
    if pattern.is_empty() || types.is_empty() {
        return Err(format!("pattern {pattern:?} matches no event type amux emits"));
    }
    let known: std::collections::BTreeSet<&str> = types.iter()
        .flat_map(|t| t["fields"].as_array().into_iter().flatten().filter_map(Value::as_str)).collect();
    let filter = v.get("filter").cloned().unwrap_or(json!({}));
    let mut used = Vec::new();
    filter_fields(&filter, &mut used);
    let unknown: Vec<String> = used.into_iter()
        .filter(|f| !known.contains(f.split('.').next().unwrap_or(""))).collect();
    if !unknown.is_empty() && types.iter().any(|t| t["count"].as_i64().unwrap_or(0) > 0) {
        return Err(format!("the filter reads fields {pattern} does not have: {}", unknown.join(", ")));
    }
    Ok(Compiled {
        pattern,
        filter,
        summary: v["summary"].as_str().unwrap_or("").trim().to_string(),
        judge: v["judge"].as_bool().unwrap_or(false),
        dedupe_key: v["dedupe_key"].as_str().unwrap_or("").trim().to_string(),
    })
}

/// The JSON object in a model answer: inside `<json></json>` as asked, else a
/// fenced block, else the first `{...}` that parses. The live helper model
/// (2026-11-11) answered with a fenced block and no tags.
pub(crate) fn answer_json(out: &str) -> Option<Value> {
    if let Some(i) = out.find("<json>") {
        let rest = &out[i + 6..];
        if let Some(j) = rest.find("</json>") {
            if let Ok(v) = serde_json::from_str::<Value>(rest[..j].trim()) {
                return Some(v);
            }
        }
    }
    for (i, _) in out.match_indices('{') {
        let mut de = serde_json::Deserializer::from_str(&out[i..]).into_iter::<Value>();
        if let Some(Ok(v)) = de.next() {
            if v.is_object() {
                return Some(v);
            }
        }
    }
    None
}

fn filter_fields(f: &Value, out: &mut Vec<String>) {
    if let Some(o) = f.as_object() {
        for k in ["all", "any"] {
            for x in o.get(k).and_then(Value::as_array).into_iter().flatten() {
                filter_fields(x, out);
            }
        }
        if let Some(n) = o.get("not") {
            filter_fields(n, out);
        }
        if let Some(name) = o.get("field").and_then(Value::as_str) {
            out.push(name.to_string());
        }
    }
}

pub(crate) async fn compile(client: Arc<dyn ModelClient>, criteria: String, catalog: Value) -> Result<Compiled, String> {
    let prompt = super::session_verbs::redact_prompt_secrets(&compile_prompt(&criteria, &catalog));
    let model = super::mdai::resolve_model(None);
    let out = tokio::task::spawn_blocking(move || client.complete(&model, &prompt))
        .await
        .map_err(|e| e.to_string())??;
    parse_compiled(&out, &catalog)
}

// ---- judge ------------------------------------------------------------------------

/// Judge calls allowed per subscription per hour.
pub(crate) const JUDGE_PER_HOUR: usize = 30;

static JUDGED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<i64, Vec<i64>>>> = std::sync::OnceLock::new();

/// Whether this subscription may spend another judge call now (and counts it).
pub(crate) fn judge_allowed(sub_id: i64, now: i64) -> bool {
    let m = JUDGED.get_or_init(Default::default);
    let mut g = m.lock().unwrap_or_else(|e| e.into_inner());
    let calls = g.entry(sub_id).or_default();
    calls.retain(|t| now - t < 3600);
    if calls.len() >= JUDGE_PER_HOUR {
        return false;
    }
    calls.push(now);
    true
}

pub(crate) fn judge_prompt(criteria: &str, ev: &Value) -> String {
    format!(
        "Someone asked amux to tell them: {criteria}\n\nThis event just happened:\n{}\n\n\
         Does it match what they asked for? Decide from what the event shows: a title, a status, a reason. \
         A word in a URL is not enough: if the request is about what a page shows and the event has no \
         title, answer NO (a later event for the same page carries its title). If the event does not show \
         that it matches, answer NO. Answer YES or NO, and nothing else.",
        clip(ev.clone())
    )
}

pub(crate) fn judge_says_yes(out: &str) -> bool {
    out.trim().trim_matches(|c: char| !c.is_alphanumeric()).to_ascii_uppercase().starts_with("YES")
}

// ---- notify -------------------------------------------------------------------

/// Post a fire in #general as "amux events" (a reply under the subscription's
/// first notice, so one subscription is one thread) and push it to the owner's
/// devices. Returns the outcome and the root post id to keep.
pub(crate) async fn notify(state: &AppState, sub_name: &str, root: Option<i64>, text: &str, ev: &Value) -> (String, Option<i64>) {
    let worker = ev["worker"].as_str().or_else(|| ev["session"].as_str()).unwrap_or("").to_string();
    let body = if root.is_some() { text.to_string() } else { format!("**{sub_name}**\n\n{text}") };
    let posted = super::conversations::bridge_post(
        state, "g:general", "amux:events", "amux events", &body, None, root, Box::new(|_, _| Ok(())),
    )
    .await;
    let url = if worker.is_empty() { "/#messages=g:general".to_string() } else { format!("/#peek={worker}") };
    let pushed = crate::push::send_all(state, &format!("amux: {sub_name}"), &text.chars().take(180).collect::<String>(),
        &worker, &format!("sub-{sub_name}"), &url).await;
    match posted {
        Ok(v) => {
            let id = v["id"].as_i64();
            (format!("posted:{}:push={}", id.unwrap_or(0), pushed.len()), root.or(id))
        }
        Err(e) => (format!("post_failed:{e}:push={}", pushed.len()), root),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(data: Value) -> Value {
        event_view("browser.blocked", "amux", "browser-api", &data)
    }

    #[test]
    fn a_filter_reads_fields_with_every_operator() {
        let e = ev(json!({"reason": "captcha", "url": "https://www.google.com/recaptcha/api2/demo", "n": 5, "page": {"title": "Demo"}}));
        let c = |f: Value| filter_matches(&f, &e);
        assert!(c(json!({})));
        assert!(c(json!({"field": "reason", "op": "eq", "value": "CAPTCHA"})));
        assert!(!c(json!({"field": "reason", "op": "ne", "value": "captcha"})));
        assert!(c(json!({"field": "reason", "op": "in", "value": ["bot_check", "captcha"]})));
        assert!(c(json!({"field": "url", "op": "contains", "value": "RECAPTCHA"})));
        assert!(c(json!({"field": "url", "op": "regex", "value": "^https://www\\.google\\."})));
        assert!(c(json!({"field": "n", "op": "gt", "value": 4})) && !c(json!({"field": "n", "op": "lt", "value": 4})));
        assert!(c(json!({"field": "page.title", "op": "eq", "value": "demo"})), "dotted paths read nested data");
        assert!(c(json!({"field": "session", "op": "eq", "value": "amux"})), "the event's own session is a field");
        assert!(!c(json!({"field": "missing", "op": "exists"})));
        assert!(!c(json!({"field": "missing", "op": "ne", "value": "captcha"})), "ne on an absent field does not match");
        assert!(c(json!({"all": [{"field": "reason", "op": "eq", "value": "captcha"}, {"not": {"field": "n", "op": "gt", "value": 9}}]})));
        assert!(c(json!({"any": [{"field": "reason", "op": "eq", "value": "login"}, {"field": "n", "op": "eq", "value": 5}]})));
        assert!(!c(json!({"field": "reason", "op": "bogus", "value": "x"})), "an operator it cannot read fails closed");
        assert!(!c(json!("captcha")), "so does a filter that is not an object");
    }

    #[test]
    fn templates_fill_from_the_event_and_drop_unknown_names() {
        let e = ev(json!({"reason": "captcha", "url": "https://x/"}));
        assert_eq!(fill("{{session}}|{{url}}", &e), "amux|https://x/");
        assert_eq!(fill("{{ nope }}-{{reason}}", &e), "-captcha");
        assert_eq!(fill("open {{", &e), "open {{");
    }

    fn cat() -> Value {
        json!({"types": [
            {"type": "browser.blocked", "count": 2, "fields": ["reason", "url", "worker", "type", "session", "source"]},
            {"type": "session.native_status", "count": 900, "fields": ["status", "type", "session", "source"]},
        ]})
    }

    #[test]
    fn a_compile_is_checked_against_the_catalog() {
        let ok = parse_compiled(r#"Sure. <json>{"pattern":"browser.blocked","filter":{"field":"reason","op":"eq","value":"captcha"},"summary":"A browser hits a captcha","judge":false,"dedupe_key":"{{worker}}|{{url}}"}</json>"#, &cat()).unwrap();
        assert_eq!(ok.pattern, "browser.blocked");
        assert_eq!(ok.dedupe_key, "{{worker}}|{{url}}");
        assert!(parse_compiled(r#"<json>{"pattern":"browser.nope","filter":{}}</json>"#, &cat()).unwrap_err().contains("matches no event type"));
        assert!(parse_compiled(r#"<json>{"pattern":"browser.*","filter":{"field":"colour","op":"eq","value":"red"}}</json>"#, &cat())
            .unwrap_err().contains("colour"));
        assert!(parse_compiled(r#"<json>{"error":"amux records no weather"}</json>"#, &cat()).unwrap_err().contains("weather"));
        assert!(parse_compiled("KEEP", &cat()).is_err());
        // A fenced answer with no tags (the live helper model's shape) still parses.
        let fenced = "```json\n{\"pattern\": \"browser.blocked\", \"filter\": {\"field\": \"reason\", \"op\": \"eq\", \"value\": \"captcha\"}}\n```";
        assert_eq!(parse_compiled(fenced, &cat()).unwrap().pattern, "browser.blocked");
    }

    #[test]
    fn the_compile_prompt_shows_real_values_and_how_to_name_a_worker() {
        let c = json!({"types": [{"type": "session.native_status", "count": 9, "meaning": "hooks",
            "fields": ["event", "session_id", "session"], "example": {"event": "Stop", "session": "mixpeek-general", "session_id": "b61f"}}]});
        let p = compile_prompt("mixpeek-general finishes its turn", &c);
        assert!(p.contains("example {") && p.contains("\"event\":\"Stop\"") && p.contains("mixpeek-general"), "{p}");
        assert!(p.contains("Match a worker by `session`"), "{p}");
    }

    #[test]
    fn the_judge_answers_yes_or_no_and_is_capped() {
        let p = judge_prompt("a browser lands on a sign-in page", &json!({"url": "https://en.wikipedia.org/wiki/Login"}));
        assert!(p.contains("has no \\\ntitle, answer NO") || p.contains("title, answer NO"), "{p}");
        assert!(judge_says_yes("YES") && judge_says_yes(" yes.") && !judge_says_yes("No") && !judge_says_yes(""));
        let id = 987_654;
        for _ in 0..JUDGE_PER_HOUR {
            assert!(judge_allowed(id, 1000));
        }
        assert!(!judge_allowed(id, 1001), "capped within the hour");
        assert!(judge_allowed(id, 1000 + 3600), "the window moves");
    }

    #[test]
    fn board_rows_become_card_events() {
        let conn = crate::db::migrate::test_memdb();
        conn.execute("INSERT INTO issues (id,title,desc,status,session,created,updated,owner_type,type) VALUES ('A-1','Ship it','','done','w',1,1,'agent','task')", []).unwrap();
        let (t, s, d) = board_event(&conn, "A-1", "UPDATE", Some("doing"), Some("done"), Some("w"));
        assert_eq!((t.as_str(), s.as_str()), ("board.card.moved", "w"));
        assert_eq!((d["title"].as_str(), d["status"].as_str(), d["old_status"].as_str()), (Some("Ship it"), Some("done"), Some("doing")));
        assert_eq!(board_event(&conn, "A-1", "UPDATE", Some("done"), Some("done"), None).0, "board.card.updated");
        assert_eq!(board_event(&conn, "A-1", "INSERT", None, Some("todo"), None).0, "board.card.created");
    }

    #[test]
    fn the_catalog_lists_emitted_types_with_fields_and_marks_the_undocumented() {
        let conn = crate::db::migrate::test_memdb();
        let now = crate::config::now_f64();
        for (t, d) in [("browser.blocked", r#"{"reason":"captcha","url":"u"}"#), ("weird.thing", r#"{"x":1}"#), ("weird.thing", r#"{"y":2}"#)] {
            conn.execute("INSERT INTO session_events (ts,session,type,data,source) VALUES (?1,'w',?2,?3,'t')", rusqlite::params![now, t, d]).unwrap();
        }
        let c = catalog(&conn, 7).unwrap();
        assert_eq!(c["measured"], true);
        let weird = c["types"].as_array().unwrap().iter().find(|t| t["type"] == "weird.thing").unwrap();
        assert_eq!(weird["count"], 2);
        assert_eq!(weird["documented"], false);
        let f: Vec<&str> = weird["fields"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        assert!(f.contains(&"x") && f.contains(&"y") && f.contains(&"session"), "{f:?}");
        let silent = c["types"].as_array().unwrap().iter().find(|t| t["type"] == "deploy.wake").unwrap();
        assert_eq!(silent["count"], 0, "a known type that has not fired is still listed");
        let b = backtest(&conn, "browser.*", &json!({"field": "reason", "op": "eq", "value": "captcha"}), 7).unwrap();
        assert_eq!((b["matched"].as_i64(), b["n_considered"].as_i64()), (Some(1), Some(1)));
    }
}
