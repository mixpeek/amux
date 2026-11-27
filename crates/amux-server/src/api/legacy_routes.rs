//! Retired route spellings that still answer, and whether anyone still calls
//! them (primitive integrity audit 2026-11-26, P1 / step 7).
//!
//! Files had two browse APIs and three upload paths. One of each is canonical
//! now: `/api/fs` (plus `/api/file/raw` for bytes) to browse, the chunked
//! `/api/upload` protocol to write. The older spellings keep answering because
//! callers outside this repo can reach them (the iOS share extension's
//! history, `cloud/seed.py`, the `amux` skill doc taught `curl` recipes for
//! both). Each one is a thin adapter or a URI alias onto the canonical
//! handler, so there is one implementation per job.
//!
//! "Can we delete it yet" is a question about traffic, so it gets an
//! instrument rather than a guess:
//!
//! - **Per hit**: [`record`] writes an INFO line with
//!   `verdict = "legacy_route_served"`, the legacy and canonical spellings and
//!   the caller. `grep -a legacy_route_served ~/.amux/logs/server-rs.log`
//!   finds every one.
//! - **Over a window**: `GET /api/debug/legacy-routes?since_h=168` counts each
//!   legacy spelling in the request log, which records the RAW path a client
//!   sent (before any alias rewrite), so the count survives the restarts the
//!   auto-builder does on every commit. Each route carries its own verdict,
//!   `unused_in_window` or `in_use`, beside the population it was drawn from.

use super::AppState;
use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

/// How a legacy spelling reaches its canonical handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// The legacy path and everything under it (`aliases::ROUTE_ALIASES`).
    Prefix,
    /// Exactly this path, served by an adapter in the legacy module.
    Exact,
}

#[derive(Debug, Clone, Copy)]
pub struct LegacyRoute {
    pub legacy: &'static str,
    pub canonical: &'static str,
    pub shape: Shape,
    pub how: &'static str,
}

/// Every retired spelling that still answers. A ROUTE_ALIASES row that is not
/// listed here fails `every_route_alias_is_counted` below.
pub const LEGACY_ROUTES: &[LegacyRoute] = &[
    LegacyRoute {
        legacy: "/api/files/mdai",
        canonical: "/api/mdai",
        shape: Shape::Prefix,
        how: "URI rewrite before routing (aliases.rs)",
    },
    LegacyRoute {
        legacy: "/api/issues",
        canonical: "/api/tasks",
        shape: Shape::Prefix,
        how: "URI rewrite before routing (aliases.rs)",
    },
    LegacyRoute {
        legacy: "/api/files",
        canonical: "/api/fs/list",
        shape: Shape::Exact,
        how: "resolves ?path= under the files root, then serves the /api/fs/list handler (files.rs)",
    },
    LegacyRoute {
        legacy: "/api/files/download",
        canonical: "/api/file/raw?download=1",
        shape: Shape::Exact,
        how: "resolves ?path= under the files root, then serves the /api/file/raw handler (files.rs)",
    },
    LegacyRoute {
        legacy: "/api/files/upload",
        canonical: "/api/upload",
        shape: Shape::Exact,
        how: "raw body written through upload::publish, the chunked path's writer (files.rs)",
    },
    LegacyRoute {
        legacy: "/api/fs/upload",
        canonical: "/api/upload",
        shape: Shape::Exact,
        how: "multipart parts written through upload::publish, the chunked path's writer (fs.rs)",
    },
];

/// Count and log one hit on a legacy spelling.
///
/// `sent` is the path the client sent; `caller` is its `X-Amux-Session` or,
/// failing that, its user agent, so the log line names who to move.
pub fn record(legacy: &'static str, canonical: &'static str, sent: &str, caller: &str) {
    tracing::info!(
        target: "amux::routing",
        verdict = "legacy_route_served",
        legacy,
        canonical,
        sent,
        caller,
        "a retired route spelling was served; move the caller to the canonical route"
    );
}

/// The caller label [`record`] logs: the session header, else the user agent.
pub fn caller_of(headers: &axum::http::HeaderMap) -> String {
    let get = |k: &str| {
        headers
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.chars().take(120).collect::<String>())
    };
    get("x-amux-session")
        .map(|s| format!("session:{s}"))
        .or_else(|| get("user-agent").map(|s| format!("ua:{s}")))
        .unwrap_or_else(|| "unknown".into())
}

#[derive(Deserialize)]
pub struct WindowQuery {
    since_h: Option<f64>,
}

/// SQL predicate on the raw request-log path for one legacy route.
fn path_matches(r: &LegacyRoute) -> (String, Vec<String>) {
    match r.shape {
        Shape::Exact => ("path = ?".into(), vec![r.legacy.to_string()]),
        Shape::Prefix => (
            "(path = ? OR path LIKE ? ESCAPE '\\')".into(),
            vec![r.legacy.to_string(), format!("{}/%", r.legacy.replace('_', "\\_"))],
        ),
    }
}

/// Count each legacy route in the request log over `since_h` hours.
///
/// The family column is indexed with ts, so each count scans one family's
/// window rather than the whole log. Prefix routes must not double-count an
/// exact sibling: `/api/files` is Exact, so `/api/files/mdai/run` is never
/// counted as a `/api/files` listing.
pub fn window_counts(conn: &rusqlite::Connection, since_ts: f64) -> rusqlite::Result<(Vec<Value>, u64)> {
    let mut rows = Vec::new();
    let mut total = 0u64;
    for r in LEGACY_ROUTES {
        let family = super::request_log::family_of(r.legacy);
        let (pred, args) = path_matches(r);
        let sql = format!(
            "SELECT COUNT(*), MAX(ts) FROM _amux_request_log \
             WHERE family = ? AND ts >= ? AND {pred}"
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(family.clone()), Box::new(since_ts)];
        for a in &args {
            params.push(Box::new(a.clone()));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        let (hits, last): (i64, Option<f64>) =
            conn.query_row(&sql, refs.as_slice(), |row| Ok((row.get(0)?, row.get(1)?)))?;
        // The loudest callers, so a non-zero count names who to move.
        let csql = format!(
            "SELECT COALESCE(NULLIF(amux_session, ''), 'ua:' || COALESCE(user_agent, '?')), COUNT(*) \
             FROM _amux_request_log WHERE family = ? AND ts >= ? AND {pred} \
             GROUP BY 1 ORDER BY 2 DESC LIMIT 5"
        );
        let mut stmt = conn.prepare(&csql)?;
        let callers: Vec<Value> = stmt
            .query_map(refs.as_slice(), |row| {
                Ok(json!({"caller": row.get::<_, String>(0)?, "hits": row.get::<_, i64>(1)?}))
            })?
            .collect::<rusqlite::Result<_>>()?;
        total += hits.max(0) as u64;
        rows.push(json!({
            "legacy": r.legacy,
            "canonical": r.canonical,
            "shape": match r.shape { Shape::Prefix => "prefix", Shape::Exact => "exact" },
            "how": r.how,
            "hits": hits,
            "last_hit_ts": last,
            "top_callers": callers,
            "verdict": if hits == 0 { "unused_in_window" } else { "in_use" },
        }));
    }
    Ok((rows, total))
}

/// `GET /api/debug/legacy-routes[?since_h=168]`.
pub async fn debug(State(state): State<AppState>, Query(q): Query<WindowQuery>) -> Json<Value> {
    let since_h = q.since_h.filter(|h| h.is_finite() && *h > 0.0).unwrap_or(168.0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let since_ts = now - since_h * 3600.0;
    let store = state.store.clone();
    let got = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<Value>, u64, i64)> {
        let conn = store.read()?;
        let (rows, total) = window_counts(&conn, since_ts)?;
        // The population: every request logged in the window. A zero beside a
        // non-zero population is "nobody called"; a zero beside zero requests
        // is a log that recorded nothing, which proves nothing.
        let logged: i64 = conn.query_row(
            "SELECT COUNT(*) FROM _amux_request_log WHERE ts >= ?",
            [since_ts],
            |r| r.get(0),
        )?;
        Ok((rows, total, logged))
    })
    .await;
    match got {
        Ok(Ok((rows, total, logged))) => {
            let unused = rows.iter().filter(|r| r["verdict"] == "unused_in_window").count();
            let body = json!({
                "since_h": since_h,
                "routes": rows,
                "hits_total": total,
                "requests_logged_in_window": logged,
                "routes_unused": unused,
                "verdict": if logged == 0 {
                    "no_traffic_logged"
                } else if total == 0 {
                    "all_legacy_routes_unused"
                } else {
                    "legacy_routes_in_use"
                },
                "note": "counts read the request log's RAW path (pre alias-rewrite); \
                         each hit is also logged as verdict=legacy_route_served",
            });
            Json(super::measured::measured(body, logged.max(0) as usize))
        }
        Ok(Err(e)) => Json(super::measured::unmeasured(
            json!({"routes": [], "since_h": since_h}),
            &format!("request log query failed: {e}"),
        )),
        Err(e) => Json(super::measured::unmeasured(
            json!({"routes": [], "since_h": since_h}),
            &format!("request log query task failed: {e}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_alias_is_counted() {
        for (legacy, canonical) in super::super::aliases::route_aliases() {
            if *legacy == "/api/sessions" {
                // Never rewritten: it has its own Python-shape handler.
                continue;
            }
            assert!(
                LEGACY_ROUTES
                    .iter()
                    .any(|r| r.legacy == *legacy && r.canonical == *canonical && r.shape == Shape::Prefix),
                "ROUTE_ALIASES row {legacy} -> {canonical} is not in LEGACY_ROUTES, so its hits are never counted"
            );
        }
    }

    fn log(conn: &rusqlite::Connection, path: &str, ts: f64, session: &str) {
        conn.execute(
            "INSERT INTO _amux_request_log (ts, method, path, family, status, latency_ms, amux_session) \
             VALUES (?1, 'GET', ?2, ?3, 200, 1.0, ?4)",
            rusqlite::params![ts, path, super::super::request_log::family_of(path), session],
        )
        .unwrap();
    }

    #[test]
    fn window_counts_each_spelling_and_only_its_own() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(include_str!("../../migrations/0010_request_log.sql")).unwrap();
        log(&conn, "/api/files/mdai/run", 100.0, "lane-a");
        log(&conn, "/api/files/mdai", 101.0, "lane-a");
        log(&conn, "/api/files", 102.0, "lane-b");
        log(&conn, "/api/fs/upload", 103.0, "");
        log(&conn, "/api/fs/list", 104.0, "lane-c"); // canonical: never counted
        log(&conn, "/api/mdai/run", 105.0, "lane-c"); // canonical: never counted
        log(&conn, "/api/files/mdaix", 106.0, "lane-c"); // near miss of the prefix
        log(&conn, "/api/files/mdai/old", 1.0, "lane-z"); // outside the window
        let (rows, total) = window_counts(&conn, 50.0).unwrap();
        let hits = |legacy: &str| {
            rows.iter().find(|r| r["legacy"] == legacy).unwrap()["hits"].as_i64().unwrap()
        };
        assert_eq!(hits("/api/files/mdai"), 2);
        assert_eq!(hits("/api/files"), 1, "exact: mdai paths are not listings");
        assert_eq!(hits("/api/fs/upload"), 1);
        assert_eq!(hits("/api/files/upload"), 0);
        assert_eq!(total, 4);
        let mdai = rows.iter().find(|r| r["legacy"] == "/api/files/mdai").unwrap();
        assert_eq!(mdai["verdict"], "in_use");
        assert_eq!(mdai["top_callers"][0]["caller"], "lane-a");
        let up = rows.iter().find(|r| r["legacy"] == "/api/files/upload").unwrap();
        assert_eq!(up["verdict"], "unused_in_window");
    }
}
