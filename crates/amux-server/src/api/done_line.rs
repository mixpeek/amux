//! Orchestration contract A3 (AH-389): a project's done line is frozen and
//! measured from day one.
//!
//! A done line is the set of proof cards that finish a project, keyed by the
//! project's epic card (GS-12's is MO-3905). The first freeze can come from
//! any identified caller; every change after that is a new version that needs
//! the owner and a reason, and is kept in `done_line_revisions`. While a card
//! is on a frozen line, a non-owner cannot discard, archive, delete or move it
//! out of the epic: that would change scope without the owner's name.
//!
//! `GET /api/contract/done-line/{epic}` is the measure: the frozen line, how
//! many are verified, how many are done, what is missing, and live cards under
//! the epic that sit outside the line.
//!
//! Verdicts (rule 14): done_line_frozen, done_line_revised,
//! done_line_change_refused, done_line_revision_refused.
use crate::api::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/api/contract/done-line/{epic}",
        axum::routing::get(get_line).post(post_line),
    )
}

/// The frozen line containing `card`, if any: (epic, version).
pub fn line_of(conn: &Connection, card: &str) -> rusqlite::Result<Option<(String, i64)>> {
    conn.query_row(
        "SELECT d.epic, d.version FROM done_lines d, json_each(d.cards) j WHERE j.value = ?1 LIMIT 1",
        [card],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// (cards, version, frozen by, frozen at)
type Line = (Vec<String>, i64, String, f64);

fn load(conn: &Connection, epic: &str) -> rusqlite::Result<Option<Line>> {
    conn.query_row("SELECT cards, version, by, at FROM done_lines WHERE epic = ?1", [epic], |r| {
        let cards: String = r.get(0)?;
        Ok((serde_json::from_str(&cards).unwrap_or_default(), r.get(1)?, r.get(2)?, r.get(3)?))
    })
    .optional()
}

/// Whether a request body would take a card off its line: discard, archive,
/// or a new epic. Pure, so each branch is testable.
pub fn removes_from_line(body: &Value, epic: &str) -> Option<&'static str> {
    if body.get("status").and_then(Value::as_str) == Some("discarded") {
        return Some("discard");
    }
    if body.get("archived").and_then(Value::as_bool) == Some(true) {
        return Some("archive");
    }
    match body.get("epic") {
        Some(Value::String(e)) if e != epic => Some("move out of the epic"),
        Some(Value::Null) => Some("move out of the epic"),
        _ => None,
    }
}

fn refusal(card: &str, epic: &str, version: i64, what: &str) -> Response {
    tracing::info!(card, epic, version, what, measured = true, n_considered = 1,
        verdict = "done_line_change_refused", "a non-owner tried to take a card off a frozen done line");
    (StatusCode::CONFLICT, Json(json!({
        "ok": false,
        "code": "done_line_frozen",
        "error": format!("{card} is on {epic}'s frozen done line (version {version}); a {what} changes the project's scope, which is the owner's decision (contract A3)"),
        "how_to_fix": {
            "worker": format!("ask the owner: POST /api/contract/done-line/{epic} with the new card list and a reason, or keep the card and close it honestly (done, or cannot_satisfy)"),
        },
        "contract": "docs/orchestration-contract.md (A3)",
    }))).into_response()
}

/// The guard every card-removing route calls. `body` is the request as the
/// PATCH route sees it; archive and delete pass a synthetic body.
pub async fn guard(state: &AppState, card: &str, body: &Value, owner: bool) -> Option<Response> {
    if owner {
        return None;
    }
    let c = card.to_string();
    let (epic, version) = state.store.read_async(move |conn| Ok(line_of(conn, &c)?)).await.ok().flatten()?;
    let what = removes_from_line(body, &epic)?;
    Some(refusal(card, &epic, version, what))
}

async fn get_line(State(state): State<AppState>, Path(epic): Path<String>) -> Response {
    let e = epic.clone();
    let r = state.store.read_async(move |conn| {
        let Some((cards, version, by, at)) = load(conn, &e)? else { return Ok(None) };
        let mut rows = Vec::new();
        for id in &cards {
            let row = crate::db::board_store::get_issue(conn, id)?;
            rows.push(match row {
                Some(r) => json!({"id": id, "status": r.status, "archived": r.archived, "title": r.title}),
                None => json!({"id": id, "status": "missing"}),
            });
        }
        let mut st = conn.prepare("SELECT id FROM issues WHERE epic = ?1 AND COALESCE(archived, 0) = 0 AND status NOT IN ('discarded') AND COALESCE(deleted, 0) = 0")?;
        let children: Vec<String> = st.query_map([&e], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let outside: Vec<String> = children.into_iter().filter(|c| !cards.contains(c)).collect();
        let mut rv = conn.prepare("SELECT version, by, reason, at, json_array_length(cards) FROM done_line_revisions WHERE epic = ?1 ORDER BY version")?;
        let revisions: Vec<Value> = rv.query_map([&e], |r| Ok(json!({
            "version": r.get::<_, i64>(0)?, "by": r.get::<_, String>(1)?, "reason": r.get::<_, String>(2)?,
            "at": r.get::<_, f64>(3)?, "cards": r.get::<_, i64>(4)?,
        })))?.collect::<rusqlite::Result<_>>()?;
        Ok(Some((cards, version, by, at, rows, outside, revisions)))
    }).await;
    match r {
        Ok(Some((cards, version, by, at, rows, outside, revisions))) => {
            let count = |s: &str| rows.iter().filter(|r| r["status"] == s && r["archived"] != json!(true)).count();
            let gone: Vec<&Value> = rows.iter()
                .filter(|r| r["status"] == "missing" || r["status"] == "discarded" || r["archived"] == json!(true))
                .map(|r| &r["id"]).collect();
            Json(json!({
                "epic": epic, "version": version, "frozen_by": by, "frozen_at": at,
                "measured": true, "n_considered": cards.len(),
                "verified": count("verified"), "done": count("done"), "line": rows,
                "gone": gone, "outside_line": outside, "revisions": revisions,
                "contract": "docs/orchestration-contract.md (A3)",
            })).into_response()
        }
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({
            "measured": false, "n_considered": 0, "why_unmeasured": format!("{epic} has no frozen done line"),
            "how_to_fix": format!("POST /api/contract/done-line/{epic} with {{\"cards\": [...]}} (default: the epic's live children)"),
        }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"measured": false, "n_considered": 0, "why_unmeasured": e.to_string()}))).into_response(),
    }
}

async fn post_line(State(state): State<AppState>, Path(epic): Path<String>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let owner = crate::api::standing_approvals::is_owner_request(&headers);
    let who = if owner {
        crate::api::turn_end::owner_name()
    } else {
        match headers.get("x-amux-session").and_then(|v| v.to_str().ok()).filter(|s| !s.trim().is_empty()) {
            Some(s) => s.to_string(),
            None => return (StatusCode::UNAUTHORIZED, Json(json!({"ok": false, "code": "identity_required",
                "error": "freezing a done line needs an identified caller"}))).into_response(),
        }
    };
    let cards: Option<Vec<String>> = body.get("cards").and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect());
    let reason = body.get("reason").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let (e, now) = (epic.clone(), crate::config::now_f64());
    let r = crate::api::contract::write_value(&state, move |conn| {
        let existing = load(conn, &e)?;
        if existing.is_some() && !owner {
            return Ok(Err("owner"));
        }
        if existing.is_some() && reason.is_none() {
            return Ok(Err("reason"));
        }
        let cards = match cards {
            Some(c) if !c.is_empty() => c,
            _ => {
                let mut st = conn.prepare("SELECT id FROM issues WHERE epic = ?1 AND COALESCE(archived, 0) = 0 AND status NOT IN ('discarded') AND COALESCE(deleted, 0) = 0 ORDER BY id")?;
                let v: Vec<String> = st.query_map([&e], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
                v
            }
        };
        if cards.is_empty() {
            return Ok(Err("empty"));
        }
        let version = existing.map(|x| x.1 + 1).unwrap_or(1);
        let js = serde_json::to_string(&cards).unwrap_or_else(|_| "[]".into());
        conn.execute("INSERT INTO done_lines (epic, cards, version, by, at) VALUES (?1, ?2, ?3, ?4, ?5)
                      ON CONFLICT(epic) DO UPDATE SET cards = ?2, version = ?3, by = ?4, at = ?5",
            rusqlite::params![e, js, version, who, now])?;
        conn.execute("INSERT INTO done_line_revisions (epic, version, cards, by, reason, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![e, version, js, who, reason.clone().unwrap_or_else(|| "initial freeze".into()), now])?;
        Ok(Ok((version, cards.len())))
    }).await;
    match r {
        Ok(Ok((version, n))) => {
            let verdict = if version == 1 { "done_line_frozen" } else { "done_line_revised" };
            tracing::info!(epic = %epic, version, cards = n, measured = true, n_considered = n, verdict, "a project's done line was frozen or revised");
            Json(json!({"ok": true, "epic": epic, "version": version, "cards": n})).into_response()
        }
        Ok(Err(why)) => {
            tracing::info!(epic = %epic, why, measured = true, n_considered = 1, verdict = "done_line_revision_refused",
                "a done line change was refused");
            let (code, msg) = match why {
                "owner" => (StatusCode::FORBIDDEN, "a frozen done line changes only by the owner (contract A3)"),
                "reason" => (StatusCode::CONFLICT, "a done line revision needs a reason"),
                _ => (StatusCode::CONFLICT, "a done line needs at least one card (none given and the epic has no live children)"),
            };
            (code, Json(json!({"ok": false, "code": format!("done_line_{why}"), "error": msg}))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"ok": false, "error": e.to_string()}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discard_archive_and_moving_epic_take_a_card_off_its_line() {
        assert_eq!(removes_from_line(&json!({"status": "discarded"}), "E"), Some("discard"));
        assert_eq!(removes_from_line(&json!({"archived": true}), "E"), Some("archive"));
        assert_eq!(removes_from_line(&json!({"epic": "OTHER"}), "E"), Some("move out of the epic"));
        assert_eq!(removes_from_line(&json!({"epic": null}), "E"), Some("move out of the epic"));
        assert_eq!(removes_from_line(&json!({"epic": "E", "status": "doing"}), "E"), None, "ordinary work is untouched");
        assert_eq!(removes_from_line(&json!({"status": "done"}), "E"), None);
    }
}
