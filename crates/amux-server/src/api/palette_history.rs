//! Where the owner goes, for the Cmd-K palette's ranking (owner, 2026-11-23:
//! "Does our cmd + k search use last active/open as a ranking method?", then
//! "do that"). Two tables:
//!
//! - `nav_visits`: every worker, tab, file, group and card the owner opens,
//!   from anywhere in the dashboard (not only from Cmd-K), with how often and
//!   when last. The palette's frecency and its "Recent" rows read it.
//! - `nav_picks`: what the owner typed in Cmd-K and which result they chose,
//!   so typing the same thing again puts that result first (Alfred's and
//!   Raycast's learned picks).
//!
//! Kept on the server so a phone and a Mac share one history; before this it
//! lived in each browser's localStorage and counted Cmd-K opens only. Owner
//! only: a worker or member reading or writing it is refused. Tables come from
//! migration 0176 and are capped so they cannot grow without bound.

use super::AppState;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/palette/history", axum::routing::get(get_history))
        .route("/api/palette/visit", axum::routing::post(post_visit))
        .route("/api/palette/history/import", axum::routing::post(post_import))
}

/// What the palette can open; anything else is refused.
const KINDS: &[&str] = &[
    "worker", "view", "file", "card", "group", "schedule", "action", "pin", "connector", "skill", "secret", "payment",
];
/// Rows kept, newest first. Older rows add nothing a person would notice.
const KEEP_VISITS: i64 = 500;
const KEEP_PICKS: i64 = 1000;
const MAX_QUERY: usize = 40;

fn exists(conn: &Connection) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='nav_visits'", [], |_| Ok(()))
        .is_ok()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A typed query as stored: lowercased, spaces collapsed, at most 40 chars.
pub(crate) fn norm_query(q: &str) -> String {
    q.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().chars().take(MAX_QUERY).collect()
}

/// One open of `kind:id`, and, when `query` is set, that it was the result
/// chosen for that query.
pub(crate) fn record(conn: &Connection, kind: &str, id: &str, title: &str, query: &str, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO nav_visits (kind, id, title, visits, last_at) VALUES (?1, ?2, ?3, 1, ?4)
         ON CONFLICT(kind, id) DO UPDATE SET visits = visits + 1, last_at = MAX(last_at, excluded.last_at),
           title = CASE WHEN excluded.title = '' THEN title ELSE excluded.title END",
        rusqlite::params![kind, id, title, at],
    )?;
    let q = norm_query(query);
    if !q.is_empty() {
        conn.execute(
            "INSERT INTO nav_picks (query, kind, id, picks, last_at) VALUES (?1, ?2, ?3, 1, ?4)
             ON CONFLICT(query, kind, id) DO UPDATE SET picks = picks + 1, last_at = MAX(last_at, excluded.last_at)",
            rusqlite::params![q, kind, id, at],
        )?;
    }
    conn.execute(
        "DELETE FROM nav_visits WHERE rowid NOT IN (SELECT rowid FROM nav_visits ORDER BY last_at DESC LIMIT ?1)",
        [KEEP_VISITS],
    )?;
    conn.execute(
        "DELETE FROM nav_picks WHERE rowid NOT IN (SELECT rowid FROM nav_picks ORDER BY last_at DESC LIMIT ?1)",
        [KEEP_PICKS],
    )?;
    Ok(())
}

pub(crate) fn history(conn: &Connection) -> rusqlite::Result<(Vec<Value>, Vec<Value>)> {
    if !exists(conn) {
        return Ok((vec![], vec![]));
    }
    let mut st = conn.prepare("SELECT kind, id, title, visits, last_at FROM nav_visits ORDER BY last_at DESC")?;
    let visits = st
        .query_map([], |r| {
            Ok(json!({ "kind": r.get::<_, String>(0)?, "id": r.get::<_, String>(1)?, "title": r.get::<_, String>(2)?,
                       "visits": r.get::<_, i64>(3)?, "last_at": r.get::<_, i64>(4)? }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut st = conn.prepare("SELECT query, kind, id, picks, last_at FROM nav_picks ORDER BY last_at DESC")?;
    let picks = st
        .query_map([], |r| {
            Ok(json!({ "query": r.get::<_, String>(0)?, "kind": r.get::<_, String>(1)?, "id": r.get::<_, String>(2)?,
                       "picks": r.get::<_, i64>(3)?, "last_at": r.get::<_, i64>(4)? }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok((visits, picks))
}

fn bad(code: StatusCode, error: &str) -> Response {
    (code, Json(json!({ "ok": false, "error": error }))).into_response()
}

fn owner(state: &AppState, headers: &HeaderMap, uri: &Uri, what: &str) -> Result<(), Box<Response>> {
    super::land_approvals::owner_only(state, headers, uri, "palette_history_refused", what)
}

async fn get_history(State(state): State<AppState>, OriginalUri(uri): OriginalUri, headers: HeaderMap) -> Response {
    if let Err(r) = owner(&state, &headers, &uri, "read the owner's palette history") {
        return *r;
    }
    let store = state.store.clone();
    let got = tokio::task::spawn_blocking(move || -> anyhow::Result<_> { Ok(history(&*store.read()?)?) }).await;
    match got {
        Ok(Ok((visits, picks))) => {
            let n = visits.len() + picks.len();
            Json(json!({ "visits": visits, "picks": picks, "measured": true, "n_considered": n })).into_response()
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, measured = false, n_considered = 0, verdict = "palette_history_unreadable", "palette history could not be read");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "measured": false, "why_unmeasured": e.to_string(), "visits": [], "picks": [] }))).into_response()
        }
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// `{kind, id, title?, query?}`: kind and id are what was opened.
fn parse(v: &Value) -> Result<(String, String, String, String), &'static str> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let (kind, id) = (s("kind"), s("id"));
    if !KINDS.contains(&kind.as_str()) {
        return Err("kind must be one the palette opens (worker, view, file, card, group, ...)");
    }
    if id.is_empty() || id.len() > 1000 {
        return Err("id must be 1 to 1000 characters");
    }
    Ok((kind, id, s("title").chars().take(200).collect(), s("query")))
}

async fn post_visit(State(state): State<AppState>, OriginalUri(uri): OriginalUri, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(r) = owner(&state, &headers, &uri, "write the owner's palette history") {
        return *r;
    }
    let (kind, id, title, query) = match parse(&body) {
        Ok(x) => x,
        Err(e) => return bad(StatusCode::BAD_REQUEST, e),
    };
    let at = now_ms();
    let res = state
        .store
        .write_async(move |c| {
            record(c, &kind, &id, &title, &query, at)?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    match res {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, measured = true, n_considered = 1, verdict = "palette_visit_unrecorded", "a palette visit could not be written");
            bad(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

/// `{visits: [{kind, id, title, visits, last_at}]}`: a browser's history from
/// before the server kept it. Taken once, into an empty table, so a second
/// browser's import cannot double the counts.
async fn post_import(State(state): State<AppState>, OriginalUri(uri): OriginalUri, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(r) = owner(&state, &headers, &uri, "import the owner's palette history") {
        return *r;
    }
    let offered = body.get("visits").and_then(Value::as_array).map_or(0, Vec::len);
    let rows: Vec<(String, String, String, i64, i64)> = body
        .get("visits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| {
            let (kind, id, title, _) = parse(v).ok()?;
            let n = v.get("visits").and_then(Value::as_i64).unwrap_or(1).clamp(1, 10_000);
            let at = v.get("last_at").and_then(Value::as_i64).unwrap_or(0).clamp(0, now_ms());
            Some((kind, id, title, n, at))
        })
        .take(KEEP_VISITS as usize)
        .collect();
    let res = state
        .store
        .write_async(move |c| {
            let have: i64 = c.query_row("SELECT COUNT(*) FROM nav_visits", [], |r| r.get(0))?;
            if have > 0 {
                return Ok(crate::db::WriteOutcome { applied: false, events: vec![] });
            }
            for (kind, id, title, n, at) in &rows {
                c.execute(
                    "INSERT OR IGNORE INTO nav_visits (kind, id, title, visits, last_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![kind, id, title, n, at],
                )?;
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    match res {
        Ok(o) => {
            tracing::info!(imported = o.applied, measured = true, n_considered = offered,
                verdict = "palette_history_import", "a browser's palette history was offered to the server");
            Json(json!({ "ok": true, "imported": o.applied })).into_response()
        }
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visits_count_up_and_picks_are_kept_per_query() {
        let conn = crate::db::migrate::test_memdb();
        assert_eq!(history(&conn).unwrap(), (vec![], vec![]), "an empty history reads empty");
        record(&conn, "worker", "mxp-decomp", "mxp-decomp", "", 1000).unwrap();
        record(&conn, "worker", "mxp-decomp", "", "  Dec ", 2000).unwrap();
        record(&conn, "view", "board", "Board", "", 1500).unwrap();
        let (v, p) = history(&conn).unwrap();
        assert_eq!(v[0], json!({"kind": "worker", "id": "mxp-decomp", "title": "mxp-decomp", "visits": 2, "last_at": 2000}),
            "an empty title keeps the stored one; newest first");
        assert_eq!(v[1]["id"], "board");
        assert_eq!(p, vec![json!({"query": "dec", "kind": "worker", "id": "mxp-decomp", "picks": 1, "last_at": 2000})]);
    }

    #[test]
    fn the_tables_stay_capped() {
        let conn = crate::db::migrate::test_memdb();
        for i in 0..(KEEP_VISITS + 20) {
            record(&conn, "file", &format!("/f/{i}"), "", "q", i).unwrap();
        }
        let (v, p) = history(&conn).unwrap();
        assert_eq!(v.len() as i64, KEEP_VISITS);
        assert_eq!(v.last().unwrap()["id"], "/f/20", "the oldest go first");
        assert_eq!(p.len() as i64, KEEP_VISITS + 20, "picks have their own, larger cap");
    }

    #[tokio::test]
    async fn only_the_owner_reads_or_writes_it() {
        use crate::api::standing_approvals::tests::{call, test_state};
        let mut state = test_state();
        state.auth_token = Some("owner-secret".into());
        let app = routes().with_state(state);
        let visit = json!({"kind": "worker", "id": "w1", "title": "w1", "query": "w"});
        for (h, why) in [(vec![("x-amux-session", "w1")], "a worker"), (vec![], "no identity and no bearer")] {
            let (st, _) = call(&app, "GET", "/api/palette/history", &h, None).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{why} reading");
            let (st, _) = call(&app, "POST", "/api/palette/visit", &h, Some(visit.clone())).await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{why} writing");
        }
        let owner = [("authorization", "Bearer owner-secret")];
        let (st, _) = call(&app, "POST", "/api/palette/visit", &owner, Some(json!({"kind": "nope", "id": "x"}))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "an unknown kind");
        let (st, _) = call(&app, "POST", "/api/palette/visit", &owner, Some(visit)).await;
        assert_eq!(st, StatusCode::OK);
        let (st, v) = call(&app, "GET", "/api/palette/history", &owner, None).await;
        assert_eq!((st, v["visits"][0]["id"].as_str(), v["picks"][0]["query"].as_str()), (StatusCode::OK, Some("w1"), Some("w")));
        // A browser's old history imports once, into an empty table only.
        let old = json!({"visits": [{"kind": "view", "id": "board", "title": "Board", "visits": 9, "last_at": 5}]});
        let (_, r) = call(&app, "POST", "/api/palette/history/import", &owner, Some(old)).await;
        assert_eq!(r["imported"], false, "the table already has a visit");
    }
}
