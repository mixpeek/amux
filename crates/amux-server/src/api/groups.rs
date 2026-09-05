//! /api/groups + /api/tags — NATIVE group list + per-group config
//! (AMUX-2597 boundary work; previously proxied to Python, AMUX-2594).
//!
//! Ported from the deleted Python server (historical amux-server.py, deleted at 792ce1f; line refs are into git history):
//! - alias: `/api/groups/*` rewrites to `/api/tags/*` EXCEPT paths ending in
//!   `/config` (py:65345-65347) — net effect: both spellings serve the same
//!   list; only the /api/groups spelling has a `/config` sub-resource, and
//!   `/api/tags/<x>` is the generic 404.
//! - list: GET /api/tags | /api/groups (py:66078-66113). DERIVED FROM
//!   CC_TAGS across the fleet's session env files, never a stored list — a
//!   second table would drift the first time someone edited one and not the
//!   other (Python's own comment). Tags are split on ',' and TRIMMED (the
//!   " gtm" vs "gtm" double-group bug). Blocked (quarantined) sessions are
//!   excluded because Python's `list_sessions()` skips them (py:20393).
//! - scope: `_caller_scope` (py:15208-15224): a SESSION caller
//!   (X-Amux-Worker, legacy X-Amux-Session) sees same-tag sessions only; an
//!   untagged/unknown caller sees itself only; no header (the dashboard) is
//!   unscoped. Tag comparison is lowercased.
//! - config: GET/PATCH /api/groups/<name>/config (py:66119-66152) over the
//!   `group_config` table in the SHARED SQLite DB (schema py:10346-10353).
//!   Absent row reads as all-defaults; other methods are Python's explicit
//!   405.
//!
//! PATCH semantics, faithfully ported and worth knowing: Python binds
//! `body.get("department", "")` etc., so an ABSENT key RESETS the column to
//! its default ("" / '[]' / 0) — partial updates must resend the full
//! object. The upsert's COALESCE arms look like they make an explicit JSON
//! null preserve the old value; they do not, and cannot: SQL NULL trips the
//! columns' NOT NULL constraints BEFORE conflict resolution, so an explicit
//! null is a 500 ("NOT NULL constraint failed: group_config.department") on
//! the Python origin too — verified against Python's exact schema+SQL in
//! sqlite (2026-08-09), not assumed from the code. The COALESCE is dead
//! code inherited faithfully. Documented in
//! docs/rust-migration/server-boundary.md.

use super::AppState;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Nested at /api/groups: the list plus the /config sub-resource. ONE
/// wildcard route dispatching on the sub-path, exactly like Python's
/// `re.match(r"/api/groups/([^/]+)/config$", path)` — matchit rejects a
/// catch-all beside `/{name}/config`, and a nested `.fallback()` loses to
/// the static SPA catch-all in the full composition (AMUX-2594), so the
/// regex-shaped dispatch is both the faithful and the mountable form.
pub fn routes() -> Router<AppState> {
    Router::new().route("/", any(list_groups)).route("/{*rest}", any(groups_subpath))
}

/// `<name>/config` → the config resource; anything else under /api/groups
/// is Python's generic 404 (the alias rewrites it to /api/tags/<x>, which
/// has no route).
async fn groups_subpath(
    State(state): State<AppState>,
    AxumPath(rest): AxumPath<String>,
    req: Request,
) -> Response {
    match rest.strip_suffix("/config") {
        Some(name) if !name.is_empty() && !name.contains('/') => {
            group_config(state, name.to_string(), req).await
        }
        _ => not_found().await,
    }
}

/// Nested at /api/tags: same list handler; every sub-path is the generic
/// 404 (Python's tags spelling has no /config).
pub fn tags_routes() -> Router<AppState> {
    Router::new().route("/", any(list_groups)).route("/{*rest}", any(not_found))
}

fn j(status: u16, v: Value) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(v),
    )
        .into_response()
}

async fn not_found() -> Response {
    j(404, json!({"error": "not found"}))
}

pub(crate) use crate::config::amux_home;

/// Python `_hdr_worker` (py:15100 region): X-Amux-Worker is canonical,
/// X-Amux-Session still honored. Shared with api/scope.rs (same attribution
/// rule for scope writes).
pub(crate) fn hdr_worker(headers: &HeaderMap) -> String {
    for h in ["x-amux-worker", "x-amux-session"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() {
                return v.to_string();
            }
        }
    }
    String::new()
}

/// Quarantined session names (`~/.amux/blocked-sessions.txt`,
/// py `_blocked_session_names`): excluded from the fleet scan.
pub(crate) fn blocked_names(home: &Path) -> BTreeSet<String> {
    std::fs::read_to_string(home.join("blocked-sessions.txt"))
        .map(|t| {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// The fleet's (session, tags) pairs from `~/.amux/sessions/*.env` —
/// CC_TAGS split on ',' and TRIMMED, exactly like Python's
/// `list_sessions()` rows (py:20418) and the Rust session list
/// (sessions_legacy.rs). Pure in `home` so tests need no env mutation.
fn scan_session_tags(home: &Path) -> Vec<(String, Vec<String>)> {
    let blocked = blocked_names(home);
    let Ok(entries) = std::fs::read_dir(home.join("sessions")) else {
        return vec![];
    };
    let mut out = vec![];
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        if blocked.contains(&name) {
            continue;
        }
        let env = crate::config::parse_env_file(&path);
        let tags: Vec<String> = env
            .get("CC_TAGS")
            .map(|t| {
                t.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
            })
            .unwrap_or_default();
        out.push((name, tags));
    }
    out
}

/// `_caller_scope` (py:15208-15224): (scoped, caller_tags_lowercased, name).
fn caller_scope(home: &Path, headers: &HeaderMap) -> (bool, BTreeSet<String>, String) {
    let name = hdr_worker(headers);
    if name.is_empty() {
        return (false, BTreeSet::new(), String::new());
    }
    let env_file = home.join("sessions").join(format!("{name}.env"));
    if !env_file.exists() {
        // Unknown claimant: strictest honest scope — self only.
        return (true, BTreeSet::new(), name);
    }
    let env = crate::config::parse_env_file(&env_file);
    let tags: BTreeSet<String> = env
        .get("CC_TAGS")
        .map(|t| {
            t.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default();
    (true, tags, name)
}

/// Per-group config rows, tolerant of the table not existing yet (a fresh
/// Rust-only store; Python creates it at boot in the shared DB).
fn load_group_configs(conn: &rusqlite::Connection) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    let Ok(mut stmt) =
        conn.prepare("SELECT name, department, goal, kpis, human_cost FROM group_config")
    else {
        return out;
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
        ))
    });
    if let Ok(rows) = rows {
        for (name, department, goal, kpis, human_cost) in rows.flatten() {
            let kpis: Value = serde_json::from_str(&kpis).unwrap_or(json!([]));
            out.insert(
                name,
                json!({
                    "department": department,
                    "goal": goal,
                    "kpis": kpis,
                    "human_cost": human_cost,
                }),
            );
        }
    }
    out
}

/// The list body (py:66078-66113), pure in its inputs for testability.
fn build_group_list(
    rows: &[(String, Vec<String>)],
    scope: &(bool, BTreeSet<String>, String),
    cfgs: &BTreeMap<String, Value>,
) -> Value {
    let (scoped, ctags, cname) = scope;
    let visible: Vec<&(String, Vec<String>)> = rows
        .iter()
        .filter(|(name, tags)| {
            if !scoped {
                return true;
            }
            name == cname
                || (!ctags.is_empty()
                    && tags.iter().any(|t| ctags.contains(&t.to_lowercase())))
        })
        .collect();
    let mut counts: BTreeMap<String, i64> = BTreeMap::new();
    for (_, tags) in &visible {
        for t in tags {
            let t = t.trim();
            if !t.is_empty() {
                *counts.entry(t.to_string()).or_insert(0) += 1;
            }
        }
    }
    let mut sorted: Vec<(String, i64)> = counts.into_iter().collect();
    // Python: sorted(counts.items(), key=lambda kv: (-kv[1], kv[0])).
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total = sorted.len();
    let groups: Vec<Value> = sorted
        .into_iter()
        .map(|(g, n)| {
            let mut obj = Map::new();
            obj.insert("name".into(), json!(g));
            obj.insert("workers".into(), json!(n));
            // Python spreads `**cfgs.get(g, {})` — config keys appear only
            // when a row exists.
            if let Some(cfg) = cfgs.get(&g).and_then(|c| c.as_object()) {
                for (k, v) in cfg {
                    obj.insert(k.clone(), v.clone());
                }
            }
            Value::Object(obj)
        })
        .collect();
    json!({
        "groups": groups,
        "total": total,
        "derived_from": "CC_TAGS across workers — not a stored list, so it cannot drift",
        "set_on_a_worker": "PATCH /api/sessions/<name>/config {\"tags\": \"a, b\"}",
        "configure_group": "PATCH /api/groups/<name>/config {\"goal\": \"...\", \"department\": \"sales\", \"kpis\": [...], \"human_cost\": 300000}",
    })
}

async fn list_groups(State(state): State<AppState>, method: Method, headers: HeaderMap) -> Response {
    // Python answers this list for GET only; any other method falls through
    // the (rewritten) route table to the generic 404.
    if method != Method::GET {
        return not_found().await;
    }
    let home = amux_home();
    let rows = scan_session_tags(&home);
    let scope = caller_scope(&home, &headers);
    let cfgs = match state.store.read() {
        Ok(conn) => load_group_configs(&conn),
        Err(_) => BTreeMap::new(),
    };
    j(200, build_group_list(&rows, &scope, &cfgs))
}

// ---------------------------------------------------------------------------
// GET/PATCH /api/groups/<name>/config (py:66119-66152)
// ---------------------------------------------------------------------------

const GROUP_CONFIG_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS group_config (
    name       TEXT PRIMARY KEY,
    department TEXT NOT NULL DEFAULT '',
    goal       TEXT NOT NULL DEFAULT '',
    kpis       TEXT NOT NULL DEFAULT '[]',
    human_cost INTEGER NOT NULL DEFAULT 0,
    updated    INTEGER NOT NULL DEFAULT 0
)";

/// A body field bound the way Python's sqlite3 binds `body.get(k, default)`:
/// absent → the default; explicit null → SQL NULL (which then FAILS the
/// NOT NULL constraint exactly as it does on the Python origin — see the
/// module doc); scalars bind as themselves.
fn bind_scalar(v: Option<&Value>, default: rusqlite::types::Value) -> rusqlite::types::Value {
    use rusqlite::types::Value as Sql;
    match v {
        None => default,
        Some(Value::Null) => Sql::Null,
        Some(Value::String(s)) => Sql::Text(s.clone()),
        Some(Value::Bool(b)) => Sql::Integer(*b as i64),
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64() {
                Sql::Integer(i)
            } else {
                Sql::Real(n.as_f64().unwrap_or(0.0))
            }
        }
        // Python would raise sqlite3.InterfaceError (a 500) for a dict/list
        // here; storing its JSON text is the nearest honest behavior and
        // keeps the write atomic.
        Some(other) => Sql::Text(other.to_string()),
    }
}

async fn group_config(state: AppState, name: String, req: Request) -> Response {
    match *req.method() {
        Method::GET => {
            let conn = match state.store.read() {
                Ok(c) => c,
                Err(e) => return j(500, json!({"error": e.to_string()})),
            };
            let row = conn
                .query_row(
                    "SELECT name, department, goal, kpis, human_cost FROM group_config WHERE name=?1",
                    [&name],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)?,
                        ))
                    },
                )
                .ok();
            match row {
                None => j(
                    200,
                    json!({"name": name, "department": "", "goal": "", "kpis": [], "human_cost": 0}),
                ),
                Some((name, department, goal, kpis, human_cost)) => {
                    let kpis: Value = serde_json::from_str(&kpis).unwrap_or(json!([]));
                    j(
                        200,
                        json!({"name": name, "department": department, "goal": goal,
                               "kpis": kpis, "human_cost": human_cost}),
                    )
                }
            }
        }
        Method::PATCH => {
            let bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
                Ok(b) => b,
                Err(e) => return j(500, json!({"error": e.to_string()})),
            };
            let body: Value = if bytes.is_empty() {
                json!({})
            } else {
                match serde_json::from_slice(&bytes) {
                    Ok(v) => v,
                    Err(e) => return j(500, json!({"error": e.to_string()})),
                }
            };
            use rusqlite::types::Value as Sql;
            let department = bind_scalar(body.get("department"), Sql::Text(String::new()));
            let goal = bind_scalar(body.get("goal"), Sql::Text(String::new()));
            // Python: json.dumps(body.get("kpis", [])) — an explicit null
            // becomes the STRING "null" (not SQL NULL), faithfully kept.
            let kpis = Sql::Text(
                serde_json::to_string(body.get("kpis").unwrap_or(&json!([])))
                    .unwrap_or_else(|_| "[]".into()),
            );
            let human_cost = bind_scalar(body.get("human_cost"), Sql::Integer(0));
            let updated = chrono::Utc::now().timestamp();
            let gname = name.clone();
            let result = state
                .store
                .write_async(move |conn| {
                    conn.execute(GROUP_CONFIG_SCHEMA, [])?;
                    // SQL identical to Python's upsert (py:66137-66146),
                    // dead COALESCE arms included — see the module doc for
                    // why NULL can never reach them.
                    conn.execute(
                        "INSERT INTO group_config (name, department, goal, kpis, human_cost, updated) \
                         VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(name) DO UPDATE SET \
                         department=COALESCE(excluded.department, department), \
                         goal=COALESCE(excluded.goal, goal), \
                         kpis=COALESCE(excluded.kpis, kpis), \
                         human_cost=COALESCE(excluded.human_cost, human_cost), \
                         updated=excluded.updated",
                        rusqlite::params![gname, department, goal, kpis, human_cost, updated],
                    )?;
                    Ok(crate::db::WriteOutcome {
                        applied: true,
                        events: vec![crate::db::PendingEvent {
                            entity_type: amux_core::revision::EntityType::Other(
                                "group_config".into(),
                            ),
                            entity_id: gname.clone(),
                            mutation: amux_core::revision::MutationKind::Updated,
                            payload: None,
                        }],
                    })
                })
                .await;
            match result {
                Ok(_) => j(200, json!({"ok": true, "name": name})),
                Err(e) => j(500, json!({"error": e.to_string()})),
            }
        }
        _ => j(405, json!({"error": "method not allowed"})),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        AppState {
            secrets: std::sync::Arc::new(crate::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn fleet(dir: &Path, sessions: &[(&str, &str)]) {
        let sd = dir.join("sessions");
        std::fs::create_dir_all(&sd).unwrap();
        for (name, tags) in sessions {
            std::fs::write(sd.join(format!("{name}.env")), format!("CC_TAGS=\"{tags}\"\n"))
                .unwrap();
        }
    }

    /// The derivation core, hermetic: no env mutation, explicit home.
    #[test]
    fn list_derives_counts_scoping_and_order() {
        let td = tempfile::tempdir().unwrap();
        fleet(
            td.path(),
            &[
                ("a", "gtm, ops"),
                ("b", "ops"),
                ("c", " gtm "), // trimming: " gtm " and "gtm" are ONE group
                ("d", ""),
                ("quarantined", "ops"),
            ],
        );
        std::fs::write(td.path().join("blocked-sessions.txt"), "quarantined\n").unwrap();
        let rows = scan_session_tags(td.path());
        assert_eq!(rows.len(), 4, "blocked session excluded");

        // Dashboard (unscoped): count desc, then name asc.
        let v = build_group_list(&rows, &(false, Default::default(), String::new()), &Default::default());
        assert_eq!(
            v["groups"],
            json!([{"name": "gtm", "workers": 2}, {"name": "ops", "workers": 2}]),
        );
        assert_eq!(v["total"], 2);
        assert_eq!(
            v["derived_from"],
            "CC_TAGS across workers — not a stored list, so it cannot drift"
        );

        // Scoped to a tagged caller: same-tag rows only.
        let mut ctags = std::collections::BTreeSet::new();
        ctags.insert("ops".to_string());
        let v = build_group_list(&rows, &(true, ctags, "b".into()), &Default::default());
        // visible rows: a (ops), b (ops) → gtm counted once via a's tags.
        assert_eq!(
            v["groups"],
            json!([{"name": "ops", "workers": 2}, {"name": "gtm", "workers": 1}]),
        );

        // Untagged caller: self only → no tags → empty groups.
        let v = build_group_list(&rows, &(true, Default::default(), "d".into()), &Default::default());
        assert_eq!(v["groups"], json!([]));
        assert_eq!(v["total"], 0);
    }

    #[test]
    fn config_rows_spread_into_list_entries() {
        let rows = vec![("w".to_string(), vec!["gtm".to_string()])];
        let mut cfgs = std::collections::BTreeMap::new();
        cfgs.insert(
            "gtm".to_string(),
            json!({"department": "Sales", "goal": "g", "kpis": [{"name": "k"}], "human_cost": 5}),
        );
        let v = build_group_list(&rows, &(false, Default::default(), String::new()), &cfgs);
        assert_eq!(
            v["groups"][0],
            json!({"name": "gtm", "workers": 1, "department": "Sales", "goal": "g",
                   "kpis": [{"name": "k"}], "human_cost": 5}),
        );
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut b = axum::http::Request::builder().method(method).uri(uri);
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let res = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn config_get_patch_roundtrip_with_python_null_semantics() {
        let app: Router = Router::new().nest("/api/groups", routes()).with_state(state());
        // Missing row → all defaults, 200 (py:66126-66128).
        let (st, v) = call(&app, "GET", "/api/groups/gtm/config", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(
            v,
            json!({"name": "gtm", "department": "", "goal": "", "kpis": [], "human_cost": 0})
        );

        let (st, v) = call(
            &app,
            "PATCH",
            "/api/groups/gtm/config",
            Some(json!({"department": "Sales", "goal": "12 meetings", "kpis": [{"n": 1}], "human_cost": 300000})),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v, json!({"ok": true, "name": "gtm"}));

        let (_, v) = call(&app, "GET", "/api/groups/gtm/config", None).await;
        assert_eq!(
            v,
            json!({"name": "gtm", "department": "Sales", "goal": "12 meetings",
                   "kpis": [{"n": 1}], "human_cost": 300000})
        );

        // Explicit null is a 500 on BOTH origins: SQL NULL trips the NOT
        // NULL constraints before the upsert's COALESCE can run (verified
        // against Python's exact schema+SQL — the COALESCE is dead code,
        // ported faithfully). The row is untouched.
        let (st, v) = call(
            &app,
            "PATCH",
            "/api/groups/gtm/config",
            Some(json!({"department": null, "goal": "new goal", "kpis": [{"n": 1}], "human_cost": null})),
        )
        .await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            v["error"].as_str().unwrap_or("").contains("NOT NULL constraint failed"),
            "{v}"
        );
        let (_, v) = call(&app, "GET", "/api/groups/gtm/config", None).await;
        assert_eq!(v["department"], "Sales", "failed PATCH left the row untouched");

        let (st, _) = call(&app, "PATCH", "/api/groups/gtm/config", Some(json!({"goal": "only goal"}))).await;
        assert_eq!(st, StatusCode::OK);
        let (_, v) = call(&app, "GET", "/api/groups/gtm/config", None).await;
        assert_eq!(v["department"], "", "absent key RESETS (faithful to Python)");
        assert_eq!(v["kpis"], json!([]));

        // Other methods: Python's explicit 405.
        let (st, v) = call(&app, "DELETE", "/api/groups/gtm/config", None).await;
        assert_eq!(st, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(v, json!({"error": "method not allowed"}));
    }

    #[tokio::test]
    async fn sub_paths_and_non_get_list_are_generic_404() {
        let app: Router = Router::new()
            .nest("/api/groups", routes())
            .nest("/api/tags", tags_routes())
            .with_state(state());
        for (m, p) in [
            ("GET", "/api/groups/some-group"),
            ("GET", "/api/tags/some-tag"),
            ("GET", "/api/tags/some-tag/config"),
            ("POST", "/api/groups"),
            ("POST", "/api/tags"),
        ] {
            let (st, v) = call(&app, m, p, None).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{m} {p}");
            assert_eq!(v, json!({"error": "not found"}), "{m} {p}");
        }
    }
}
