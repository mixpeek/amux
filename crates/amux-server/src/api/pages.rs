//! Pages: an HTML app whose backend is a worker and a SQLite file
//! (docs/research/pages.md, Ethan 2026-10-21).
//!
//! A page is a folder under `~/.amux/pages/<name>/`:
//!
//! ```text
//! index.html   what the page shows; the owner or the worker writes it
//! page.db      its data; the page and the worker both read and write it
//! CLAUDE.md    the page contract the worker reads
//! page.json    which worker answers for it
//! ```
//!
//! The page's own code runs in a sandboxed iframe with an opaque origin, so it
//! can read neither amux's bearer nor any amux API. It reaches its data
//! through a host (pages-host.js, in the dashboard and at `/pages/<name>`) that
//! relays `amux.sql`, `amux.ask` and `amux.file` to the routes below, for that
//! page only.
//!
//! THE TABLE IS THE API. Page code writes rows into `asks` when it needs
//! judgment or outside work. After any write from the page, this server reads
//! the new `asks` rows, snapshots the page, and delivers one message to the
//! page's worker naming them. The worker does the work with its own tools,
//! writes results into the page's tables and marks each ask done; the page
//! sees the file change and re-renders. Nothing here decides what a page
//! stores or how the worker answers.
//!
//! Log signal (two-fix rule): `verdict=page_asks_delivered` with the ids and
//! the send's own verdict, `page_asks_delivery_refused` when the send path
//! refused, `page_sql_refused` for a statement SQLite rejected (ATTACH
//! included), `page_view_refused` for a viewer without owner access.

use super::AppState;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rusqlite::types::{Value as SqlValue, ValueRef};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Rows a single read returns. A page that needs more pages through them.
const ROW_CAP: usize = 5000;
/// Snapshots kept per page; the oldest go first.
const SNAPSHOT_KEEP: usize = 30;
/// A burst of writes (a form saving three tables) becomes one delivery.
const ASK_DEBOUNCE_MS: u64 = 1200;

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{name}", get(detail).delete(remove))
        .route("/{name}/html", axum::routing::put(put_html))
        .route("/{name}/sql", post(run_sql))
        .route("/{name}/ask", post(ask))
        .route("/{name}/version", get(version))
        .route("/{name}/file", get(file))
        .route("/{name}/snapshots", get(snapshots).post(snapshot_now))
        .route("/{name}/restore", post(restore))
}

/// The standalone URL. Same auth as the dashboard shell: owner access (this
/// machine, the owner token, or the owner session cookie), or a server with
/// no token of its own (a cloud server behind its gateway, which has already
/// signed the visitor in).
pub fn view_routes() -> Router<AppState> {
    Router::new()
        .route("/pages/{name}", get(view))
        .route("/pages/{name}/", get(view))
}

pub(crate) fn root() -> PathBuf {
    std::env::var("AMUX_PAGES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| super::session_verbs::home().join("pages"))
}

/// Lowercase letters, digits and dashes: a folder name, a URL segment and
/// part of a worker name, with nothing to escape in any of them.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn page_dir(name: &str) -> Option<PathBuf> {
    valid_name(name).then(|| root().join(name))
}

fn bad(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

fn existing(name: &str) -> Result<PathBuf, Box<Response>> {
    let Some(dir) = page_dir(name) else {
        return Err(Box::new(bad(StatusCode::BAD_REQUEST, "a page name is lowercase letters, digits and dashes")));
    };
    if !dir.join("index.html").is_file() {
        return Err(Box::new(bad(StatusCode::NOT_FOUND, format!("no page named {name}"))));
    }
    Ok(dir)
}

fn meta(dir: &Path) -> Value {
    std::fs::read_to_string(dir.join("page.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

pub(crate) fn worker_for(name: &str) -> String {
    page_dir(name)
        .map(|d| meta(&d))
        .and_then(|m| m.get("worker").and_then(Value::as_str).map(String::from))
        .unwrap_or_else(|| format!("page-{name}"))
}

// ── The page database ──

const PAGE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS asks (
  id INTEGER PRIMARY KEY,
  kind TEXT,
  text TEXT,
  data TEXT,
  status TEXT NOT NULL DEFAULT 'new',
  result TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
  done_at TEXT
);
CREATE TABLE IF NOT EXISTS _amux (key TEXT PRIMARY KEY, value TEXT);
";

/// A connection to one page's file and nothing else: `ATTACH` is refused by
/// SQLite itself (the attached-database limit is zero), so a statement cannot
/// reach amux.db or another page whatever shape it takes.
fn open_db(dir: &Path) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(dir.join("page.db"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.set_limit(rusqlite::limits::Limit::SQLITE_LIMIT_ATTACHED, 0);
    conn.execute_batch(PAGE_SCHEMA)?;
    Ok(conn)
}

/// A blob that is UTF-8 comes back as text: a worker storing HTML with
/// `readfile()` or a `X'..'` literal writes a BLOB, and the page then showed
/// "<624 bytes>" in place of the draft (MTP page, 2026-10-21). Any other blob
/// comes back as base64, so nothing a page stores is unreadable to it.
fn cell(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(f) => json!(f),
        ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(b) => match std::str::from_utf8(b) {
            Ok(text) => json!(text),
            Err(_) => {
                use base64::Engine as _;
                json!({ "base64": base64::engine::general_purpose::STANDARD.encode(b), "bytes": b.len() })
            }
        },
    }
}

fn to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .unwrap_or_else(|| SqlValue::Real(n.as_f64().unwrap_or(0.0))),
        Value::String(s) => SqlValue::Text(s.clone()),
        other => SqlValue::Text(other.to_string()),
    }
}

/// Runs one statement. A read returns its rows as objects; a write returns
/// what it changed. `params` is an array (`?`) or an object (`:name`).
fn exec(conn: &rusqlite::Connection, sql: &str, params: &Value) -> rusqlite::Result<(Value, bool)> {
    let mut stmt = conn.prepare(sql)?;
    match params {
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                stmt.raw_bind_parameter(i + 1, to_sql(v))?;
            }
        }
        Value::Object(o) => {
            for (k, v) in o {
                let key = if k.starts_with([':', '@', '$']) { k.clone() } else { format!(":{k}") };
                if let Some(i) = stmt.parameter_index(&key)? {
                    stmt.raw_bind_parameter(i, to_sql(v))?;
                }
            }
        }
        _ => {}
    }
    if stmt.readonly() && stmt.column_count() > 0 {
        let cols: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
        let mut rows = stmt.raw_query();
        let mut out = Vec::new();
        let mut truncated = false;
        while let Some(r) = rows.next()? {
            if out.len() >= ROW_CAP {
                truncated = true;
                break;
            }
            let mut obj = serde_json::Map::new();
            for (i, c) in cols.iter().enumerate() {
                obj.insert(c.clone(), cell(r.get_ref(i)?));
            }
            out.push(Value::Object(obj));
        }
        Ok((json!({ "columns": cols, "rows": out, "truncated": truncated }), false))
    } else {
        let changes = stmt.raw_execute()?;
        Ok((json!({ "changes": changes, "last_id": conn.last_insert_rowid() }), true))
    }
}

// ── Routes ──

fn modified(p: &Path) -> Option<String> {
    let m = std::fs::metadata(p).ok()?.modified().ok()?;
    Some(chrono::DateTime::<chrono::Utc>::from(m).to_rfc3339())
}

fn summary(name: &str, dir: &Path) -> Value {
    let m = meta(dir);
    let title = std::fs::read_to_string(dir.join("index.html"))
        .ok()
        .and_then(|h| {
            let lo = h.to_ascii_lowercase();
            let a = lo.find("<title>")? + 7;
            let b = lo[a..].find("</title>")? + a;
            Some(h[a..b].trim().to_string())
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| name.to_string());
    json!({
        "name": name,
        "title": title,
        "worker": worker_for(name),
        "description": m.get("description").cloned().unwrap_or(Value::Null),
        "created_at": m.get("created_at").cloned().unwrap_or(Value::Null),
        "updated_at": modified(&dir.join("index.html")),
        "data_updated_at": modified(&dir.join("page.db-wal")).max(modified(&dir.join("page.db"))),
        "url": format!("/pages/{name}"),
    })
}

async fn list() -> Response {
    let dir = root();
    let mut pages = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if valid_name(&name) && e.path().join("index.html").is_file() {
                pages.push(summary(&name, &e.path()));
            }
        }
    }
    pages.sort_by(|a, b| b["updated_at"].as_str().cmp(&a["updated_at"].as_str()));
    let n = pages.len();
    Json(json!({ "pages": pages, "root": dir, "measured": true, "n_considered": n })).into_response()
}

const STARTER_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>New page</title>
<style>body{font:15px/1.5 system-ui,sans-serif;max-width:720px;margin:48px auto;padding:0 20px}</style>
</head><body>
<h1>New page</h1>
<p>Tell this page's worker what to build. It writes this file and the page's database.</p>
</body></html>
"#;

fn contract(name: &str, dir: &Path, instructions: &str) -> String {
    format!(
        "# Page: {name}\n\n\
This folder is an amux page. You are its worker: you build it and you are its backend.\n\n\
- `index.html` is the page. It is served at `/pages/{name}` and in the dashboard's Apps tab,\n  \
inside a sandboxed frame: keep it one self-contained file (inline CSS and JavaScript). External\n  \
links and CDNs work; relative files do not (use `amux.file(path)` for those).\n\
- `page.db` is its SQLite database ({db}). Read and write it with `sqlite3 page.db`.\n  \
You choose the tables. The page opens instantly from what is stored here, so store\n  \
what it shows rather than computing it on load.\n\
- In the page, `window.amux` is ready before any script runs:\n  \
  - `await amux.sql(sql, params)` runs one statement on page.db. A read returns `{{rows, columns}}`\n    \
(rows are objects); a write returns `{{changes, last_id}}`.\n  \
  - `await amux.batch(sql)` runs several statements (schema, seed data).\n  \
  - `await amux.ask(text, data, kind)` adds a row to `asks` and wakes you.\n  \
  - `amux.on('change', fn)` fires when page.db changes, whoever wrote it.\n  \
  - `await amux.file(path)` reads another file in this folder.\n\n\
## When you are woken\n\n\
amux sends you a message naming new rows in the `asks` table. For each one:\n\n\
1. Read it: `sqlite3 -json page.db \"select * from asks where status='new'\"`.\n\
2. Do the work it asks, with your own tools.\n\
3. Write the results into the tables the page reads.\n\
4. Mark it: `update asks set status='done', result='<one line>', done_at=strftime('%Y-%m-%dT%H:%M:%SZ','now') where id=N`\n   \
(or `status='failed'` with the reason in `result`).\n\n\
The page re-renders by itself when page.db changes. amux snapshots page.db and index.html\n\
before each delivery, so the owner can restore any earlier version.\n\n\
Nothing a person outside the company reads is sent from a page without the owner's approval.\n\n\
## This page\n\n{instructions}\n",
        db = dir.join("page.db").display(),
        instructions = if instructions.trim().is_empty() {
            "No instructions yet. The owner will tell you what this page is for."
        } else {
            instructions.trim()
        },
    )
}

/// POST /api/pages {name, html?, instructions?, seed_sql?, description?, worker?}
///
/// `worker` names an existing worker to answer for the page; without it a
/// worker `page-<name>` is created with the page folder as its directory.
async fn create(State(state): State<AppState>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let s = |k: &str| body.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let name = s("name").trim().to_lowercase();
    let Some(dir) = page_dir(&name) else {
        return bad(StatusCode::BAD_REQUEST, "a page name is lowercase letters, digits and dashes (up to 48)");
    };
    if dir.join("index.html").exists() {
        return bad(StatusCode::CONFLICT, format!("a page named {name} already exists"));
    }
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("could not create {}: {e}", dir.display()));
    }
    let html = s("html");
    let html = if html.trim().is_empty() { STARTER_HTML.to_string() } else { html };
    let attach = s("worker").trim().to_string();
    let worker = if attach.is_empty() { format!("page-{name}") } else { attach.clone() };
    let seeded = {
        let dir = dir.clone();
        let seed = s("seed_sql");
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = open_db(&dir)?;
            if !seed.trim().is_empty() {
                conn.execute_batch(&seed)?;
            }
            Ok(())
        })
        .await
    };
    match seeded {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return bad(StatusCode::BAD_REQUEST, format!("seed_sql failed: {e}")),
        Err(e) => return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("seed task failed: {e}")),
    }
    let page_meta = json!({
        "worker": worker,
        "description": s("description"),
        "created_at": chrono::Utc::now().to_rfc3339(),
    });
    let writes = [
        (dir.join("index.html"), html),
        (dir.join("CLAUDE.md"), contract(&name, &dir, &s("instructions"))),
        (dir.join("page.json"), serde_json::to_string_pretty(&page_meta).unwrap_or_default()),
    ];
    for (p, text) in writes {
        if let Err(e) = std::fs::write(&p, text) {
            return bad(StatusCode::INTERNAL_SERVER_ERROR, format!("could not write {}: {e}", p.display()));
        }
    }
    // The worker is an ordinary amux worker whose folder is the page: same
    // create path as the dashboard's, so every worker capability comes with it.
    let worker_status = if attach.is_empty() {
        let desc = format!("Backend for the page /pages/{name}");
        super::sessions_legacy::create_session_legacy(
            State(state.clone()),
            headers,
            Some(Json(json!({ "name": worker, "dir": dir, "desc": desc, "creator": "pages" }))),
        )
        .await
        .status()
    } else {
        StatusCode::OK
    };
    tracing::info!(target: "amux::pages", page = %name, worker = %worker, worker_status = worker_status.as_u16(),
        verdict = "page_created", "page created");
    let mut out = summary(&name, &dir);
    out["worker_created"] = json!(attach.is_empty() && worker_status.is_success());
    out["worker_attached"] = json!(!attach.is_empty());
    out["worker_status"] = json!(worker_status.as_u16());
    (StatusCode::CREATED, Json(out)).into_response()
}

async fn detail(UrlPath(name): UrlPath<String>) -> Response {
    let dir = match existing(&name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let html = std::fs::read_to_string(dir.join("index.html")).unwrap_or_default();
    let mut out = summary(&name, &dir);
    out["html"] = json!(html);
    out["version"] = json!(signature(&dir));
    Json(out).into_response()
}

/// DELETE moves the folder to `.trash/`, so a deleted page can be put back
/// by hand. Its worker is left alone; the owner removes it like any worker.
async fn remove(UrlPath(name): UrlPath<String>) -> Response {
    let dir = match existing(&name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let trash = root().join(".trash");
    let _ = std::fs::create_dir_all(&trash);
    let dest = trash.join(format!("{name}-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S")));
    match std::fs::rename(&dir, &dest) {
        Ok(()) => {
            tracing::info!(target: "amux::pages", page = %name, moved_to = %dest.display(), verdict = "page_trashed", "page moved to trash");
            Json(json!({ "ok": true, "moved_to": dest, "worker": worker_for(&name) })).into_response()
        }
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, format!("could not move the page: {e}")),
    }
}

async fn put_html(UrlPath(name): UrlPath<String>, Json(body): Json<Value>) -> Response {
    let dir = match existing(&name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let Some(html) = body.get("html").and_then(Value::as_str) else {
        return bad(StatusCode::BAD_REQUEST, "html is required");
    };
    let _ = take_snapshot(&dir, "before-html-edit");
    match std::fs::write(dir.join("index.html"), html) {
        Ok(()) => Json(json!({ "ok": true, "version": signature(&dir) })).into_response(),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/pages/{name}/sql {sql, params?} or {batch: "..."}
async fn run_sql(State(state): State<AppState>, UrlPath(name): UrlPath<String>, Json(body): Json<Value>) -> Response {
    let dir = match existing(&name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let batch = body.get("batch").and_then(Value::as_str).map(String::from);
    let sql = body.get("sql").and_then(Value::as_str).map(String::from);
    let params = body.get("params").cloned().unwrap_or(Value::Null);
    if batch.is_none() && sql.as_deref().is_none_or(|s| s.trim().is_empty()) {
        return bad(StatusCode::BAD_REQUEST, "sql (one statement) or batch (several) is required");
    }
    let d = dir.clone();
    let started = std::time::Instant::now();
    let ran = tokio::task::spawn_blocking(move || -> rusqlite::Result<(Value, bool)> {
        let conn = open_db(&d)?;
        match batch {
            Some(b) => {
                conn.execute_batch(&b)?;
                Ok((json!({ "ok": true }), true))
            }
            None => exec(&conn, sql.as_deref().unwrap_or(""), &params),
        }
    })
    .await;
    match ran {
        Ok(Ok((mut out, wrote))) => {
            out["ms"] = json!(started.elapsed().as_millis() as u64);
            if wrote {
                schedule_ask_delivery(state, name);
            }
            Json(out).into_response()
        }
        Ok(Err(e)) => {
            tracing::info!(target: "amux::pages", page = %name, error = %e, verdict = "page_sql_refused", "page statement refused");
            bad(StatusCode::BAD_REQUEST, e.to_string())
        }
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/pages/{name}/ask {text, data?, kind?}: a row in `asks`, which
/// wakes the worker like any other write.
async fn ask(State(state): State<AppState>, UrlPath(name): UrlPath<String>, Json(body): Json<Value>) -> Response {
    let dir = match existing(&name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let text = body.get("text").and_then(Value::as_str).unwrap_or("").to_string();
    if text.trim().is_empty() {
        return bad(StatusCode::BAD_REQUEST, "text is required");
    }
    let kind = body.get("kind").and_then(Value::as_str).unwrap_or("ask").to_string();
    let data = match body.get("data") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => Some(v.to_string()),
    };
    let inserted = tokio::task::spawn_blocking(move || -> rusqlite::Result<i64> {
        let conn = open_db(&dir)?;
        conn.execute("INSERT INTO asks (kind, text, data) VALUES (?1, ?2, ?3)", rusqlite::params![kind, text, data])?;
        Ok(conn.last_insert_rowid())
    })
    .await;
    match inserted {
        Ok(Ok(id)) => {
            schedule_ask_delivery(state, name);
            Json(json!({ "id": id, "status": "new" })).into_response()
        }
        Ok(Err(e)) => bad(StatusCode::BAD_REQUEST, e.to_string()),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ── Waking the worker ──

fn pending_delivery() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static P: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    P.get_or_init(Default::default)
}

/// New asks the worker has not been told about: id above the high-water mark.
fn undelivered(dir: &Path) -> rusqlite::Result<Vec<(i64, String, String)>> {
    let conn = open_db(dir)?;
    let mark: i64 = conn
        .query_row("SELECT CAST(value AS INTEGER) FROM _amux WHERE key='delivered_ask'", [], |r| r.get(0))
        .unwrap_or(0);
    let mut stmt = conn.prepare(
        "SELECT id, COALESCE(kind,''), COALESCE(text,'') FROM asks WHERE status='new' AND id > ?1 ORDER BY id",
    )?;
    let rows = stmt
        .query_map([mark], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn mark_delivered(dir: &Path, upto: i64) -> rusqlite::Result<()> {
    let conn = open_db(dir)?;
    conn.execute(
        "INSERT INTO _amux (key, value) VALUES ('delivered_ask', ?1)
         ON CONFLICT(key) DO UPDATE SET value = MAX(CAST(value AS INTEGER), CAST(excluded.value AS INTEGER))",
        [upto],
    )?;
    Ok(())
}

pub(crate) fn delivery_text(name: &str, dir: &Path, asks: &[(i64, String, String)]) -> String {
    let ids: Vec<String> = asks.iter().map(|a| a.0.to_string()).collect();
    let preview: Vec<String> = asks
        .iter()
        .take(5)
        .map(|(id, kind, text)| {
            let t: String = text.chars().take(140).collect();
            format!("- #{id} {kind}: {t}")
        })
        .collect();
    format!(
        "[page {name}] {n} new ask{s} in {db} (id{s} {ids}).\n{preview}\n\
Read them with `sqlite3 -json page.db \"select * from asks where status='new'\"` in {dir}, do the work, \
write your results into the page's tables, then set each ask's status to 'done' (or 'failed' with the \
reason in result). The page re-renders when page.db changes. The contract is in {dir}/CLAUDE.md.",
        n = asks.len(),
        s = if asks.len() == 1 { "" } else { "s" },
        db = dir.join("page.db").display(),
        ids = ids.join(", "),
        preview = preview.join("\n"),
        dir = dir.display(),
    )
}

fn schedule_ask_delivery(state: AppState, name: String) {
    {
        let mut p = pending_delivery().lock().unwrap_or_else(|e| e.into_inner());
        if !p.insert(name.clone()) {
            return; // a delivery for this page is already waiting out the debounce
        }
    }
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(ASK_DEBOUNCE_MS)).await;
        pending_delivery().lock().unwrap_or_else(|e| e.into_inner()).remove(&name);
        let Some(dir) = page_dir(&name) else { return };
        let d = dir.clone();
        let asks = match tokio::task::spawn_blocking(move || undelivered(&d)).await {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                tracing::warn!(target: "amux::pages", page = %name, error = %e, verdict = "page_asks_unreadable", "could not read asks");
                return;
            }
            Err(_) => return,
        };
        if asks.is_empty() {
            return;
        }
        let worker = worker_for(&name);
        let upto = asks.iter().map(|a| a.0).max().unwrap_or(0);
        let d = dir.clone();
        let _ = tokio::task::spawn_blocking(move || take_snapshot(&d, "before-asks")).await;
        let text = delivery_text(&name, &dir, &asks);
        let guard = format!("page:{name}");
        let out = super::session_verbs::deliver_automated(&state, &worker, &text, &guard).await;
        let ids: Vec<i64> = asks.iter().map(|a| a.0).collect();
        if out.refused {
            // Not marked delivered: the next write from the page tries again.
            tracing::warn!(target: "amux::pages", page = %name, worker = %worker, ?ids, reason = %out.message,
                verdict = "page_asks_delivery_refused", measured = true, n_considered = ids.len(), "page asks were not delivered");
            return;
        }
        let d = dir.clone();
        let _ = tokio::task::spawn_blocking(move || mark_delivered(&d, upto)).await;
        tracing::info!(target: "amux::pages", page = %name, worker = %worker, ?ids, submission = out.submission,
            send = %out.message, queue_id = ?out.queue_id, verdict = "page_asks_delivered", measured = true,
            n_considered = ids.len(), "page asks delivered to the worker");
    });
}

// ── Change detection, files, snapshots ──

/// Cheap change signature: size and mtime of the db, its WAL and the html.
/// The worker writes with the sqlite3 CLI, which this server never sees, so
/// a file signature is the one thing every writer moves.
fn signature(dir: &Path) -> Value {
    let sig = |f: &str| {
        std::fs::metadata(dir.join(f))
            .ok()
            .map(|m| {
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                format!("{}:{}", m.len(), t)
            })
            .unwrap_or_default()
    };
    json!({
        "db": format!("{}|{}", sig("page.db"), sig("page.db-wal")),
        "html": sig("index.html"),
    })
}

async fn version(UrlPath(name): UrlPath<String>) -> Response {
    match existing(&name) {
        Ok(dir) => Json(signature(&dir)).into_response(),
        Err(r) => *r,
    }
}

#[derive(serde::Deserialize)]
struct FileQuery {
    path: String,
}

async fn file(UrlPath(name): UrlPath<String>, Query(q): Query<FileQuery>) -> Response {
    let dir = match existing(&name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let rel = q.path.trim_start_matches('/');
    let Ok(base) = dir.canonicalize() else {
        return bad(StatusCode::NOT_FOUND, "no such file");
    };
    let Ok(full) = base.join(rel).canonicalize() else {
        return bad(StatusCode::NOT_FOUND, "no such file");
    };
    // Inside the page folder only, and never its database or dotfiles.
    let hidden = full.strip_prefix(&base).map(|p| {
        p.components().any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
            || p.to_string_lossy().starts_with("page.db")
    });
    if !full.starts_with(&base) || hidden.unwrap_or(true) || !full.is_file() {
        return bad(StatusCode::NOT_FOUND, "no such file");
    }
    match std::fs::read(&full) {
        Ok(bytes) => {
            let mime = mime_guess(&full);
            ([(header::CONTENT_TYPE, mime)], bytes).into_response()
        }
        Err(e) => bad(StatusCode::NOT_FOUND, e.to_string()),
    }
}

fn mime_guess(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "csv" => "text/csv; charset=utf-8",
        "md" | "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn snapshot_dir(dir: &Path) -> PathBuf {
    dir.join(".snapshots")
}

/// A consistent copy of page.db (`VACUUM INTO`, which reads through any open
/// WAL) and index.html, named by time and reason.
pub(crate) fn take_snapshot(dir: &Path, reason: &str) -> Result<String, String> {
    let snaps = snapshot_dir(dir);
    let id = format!("{}-{}", chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ"), reason);
    let out = snaps.join(&id);
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    if dir.join("page.db").exists() {
        let conn = rusqlite::Connection::open(dir.join("page.db")).map_err(|e| e.to_string())?;
        let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        conn.execute("VACUUM INTO ?1", [out.join("page.db").to_string_lossy().to_string()])
            .map_err(|e| e.to_string())?;
    }
    let _ = std::fs::copy(dir.join("index.html"), out.join("index.html"));
    // Keep the newest SNAPSHOT_KEEP.
    if let Ok(rd) = std::fs::read_dir(&snaps) {
        let mut all: Vec<_> = rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        all.sort();
        let excess = all.len().saturating_sub(SNAPSHOT_KEEP);
        for old in all.into_iter().take(excess) {
            let _ = std::fs::remove_dir_all(old);
        }
    }
    Ok(id)
}

async fn snapshots(UrlPath(name): UrlPath<String>) -> Response {
    let dir = match existing(&name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let mut ids: Vec<String> = std::fs::read_dir(snapshot_dir(&dir))
        .map(|rd| rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    ids.sort();
    ids.reverse();
    let n = ids.len();
    let rows: Vec<Value> = ids
        .into_iter()
        .map(|id| {
            let (ts, reason) = id.split_once('-').map(|(a, b)| (a.to_string(), b.to_string())).unwrap_or((id.clone(), String::new()));
            json!({ "id": id, "ts": ts, "reason": reason })
        })
        .collect();
    Json(json!({ "snapshots": rows, "measured": true, "n_considered": n })).into_response()
}

async fn snapshot_now(UrlPath(name): UrlPath<String>) -> Response {
    let dir = match existing(&name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    match tokio::task::spawn_blocking(move || take_snapshot(&dir, "manual")).await {
        Ok(Ok(id)) => Json(json!({ "id": id })).into_response(),
        Ok(Err(e)) => bad(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// POST /api/pages/{name}/restore {id}: the current state is snapshotted
/// first, so a restore can itself be undone.
async fn restore(UrlPath(name): UrlPath<String>, Json(body): Json<Value>) -> Response {
    let dir = match existing(&name) {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let id = body.get("id").and_then(Value::as_str).unwrap_or("").to_string();
    if id.is_empty() || id.contains('/') || id.contains("..") || id.starts_with('.') {
        return bad(StatusCode::BAD_REQUEST, "id names a snapshot");
    }
    let src = snapshot_dir(&dir).join(&id);
    if !src.is_dir() {
        return bad(StatusCode::NOT_FOUND, format!("no snapshot {id}"));
    }
    let done = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let undo = take_snapshot(&dir, "before-restore")?;
        if src.join("page.db").is_file() {
            let tmp = dir.join("page.db.restoring");
            std::fs::copy(src.join("page.db"), &tmp).map_err(|e| e.to_string())?;
            for f in ["page.db-wal", "page.db-shm"] {
                let _ = std::fs::remove_file(dir.join(f));
            }
            std::fs::rename(&tmp, dir.join("page.db")).map_err(|e| e.to_string())?;
        }
        if src.join("index.html").is_file() {
            std::fs::copy(src.join("index.html"), dir.join("index.html")).map_err(|e| e.to_string())?;
        }
        Ok(undo)
    })
    .await;
    match done {
        Ok(Ok(undo)) => {
            tracing::info!(target: "amux::pages", page = %name, snapshot = %id, undo = %undo, verdict = "page_restored", "page restored");
            Json(json!({ "ok": true, "restored": id, "undo": undo })).into_response()
        }
        Ok(Err(e)) => bad(StatusCode::INTERNAL_SERVER_ERROR, e),
        Err(e) => bad(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ── The standalone page ──

const HOST_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<title>__TITLE__</title>
<!-- AMUX-BOOTSTRAP-BEGIN --><!-- AMUX-BOOTSTRAP-END -->
<style>html,body{margin:0;height:100%;background:#fff}#page{position:fixed;inset:0}#page iframe{border:0;width:100%;height:100%;display:block}
.amux-page-err{font:15px/1.5 system-ui,sans-serif;max-width:560px;margin:20vh auto;padding:0 20px;color:#333}</style>
</head><body><div id="page"></div>
<script src="/pages-host.js"></script>
<script>
AmuxPages.mount(document.getElementById('page'), __NAME__, {
  headers: () => (window._AMUX_AUTH_TOKEN ? { Authorization: 'Bearer ' + window._AMUX_AUTH_TOKEN } : {}),
  onTitle: t => { if (t) document.title = t; },
});
</script></body></html>
"#;

const SIGN_IN_HTML: &str = r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sign in to amux</title>
<style>body{font:15px/1.5 system-ui,sans-serif;max-width:520px;margin:20vh auto;padding:0 20px;color:#222}a{color:#0969da}</style>
</head><body><h1>Sign in to view this page</h1>
<p>This page belongs to an amux server that requires sign-in. Open <a href="/">amux</a> and sign in, then open this link again.</p>
</body></html>
"#;

async fn view(
    State(state): State<AppState>,
    UrlPath(name): UrlPath<String>,
    headers: HeaderMap,
    uri: Uri,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
) -> Response {
    let allowed = state.auth_token.is_none()
        || super::static_files::owner_access(&state, &headers, &uri, peer.map(|axum::Extension(c)| c.0.ip()));
    if !allowed {
        tracing::info!(target: "amux::pages", page = %name, verdict = "page_view_refused", "page view without owner access");
        return (StatusCode::UNAUTHORIZED, Html(SIGN_IN_HTML)).into_response();
    }
    let dir = match existing(&name) {
        Ok(d) => d,
        Err(_) => return (StatusCode::NOT_FOUND, Html(format!("<p>No page named {}.</p>", html_escape(&name)))).into_response(),
    };
    let title = summary(&name, &dir)["title"].as_str().unwrap_or(&name).to_string();
    let html = HOST_HTML
        .replace("__TITLE__", &html_escape(&title))
        .replace("__NAME__", &serde_json::to_string(&name).unwrap_or_default());
    let html = super::static_files::bootstrap_for(&html, &state, &headers, &uri, peer.map(|axum::Extension(c)| c.0.ip()));
    let mut resp = Html(html).into_response();
    resp.headers_mut().insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    resp
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    /// One pages root for the whole test process: AMUX_PAGES_DIR is global.
    fn test_root() -> PathBuf {
        static R: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        R.get_or_init(|| {
            let d = std::env::temp_dir().join(format!("amux-pages-root-{}", ulid::Ulid::new()));
            std::fs::create_dir_all(&d).unwrap();
            std::env::set_var("AMUX_PAGES_DIR", &d);
            d
        })
        .clone()
    }

    fn app_state(token: Option<&str>) -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        AppState {
            store,
            started: Instant::now(),
            build_hash: "test".into(),
            auth_token: token.map(String::from),
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    async fn view_as(token: Option<&str>, uri: &str, page: &str) -> (StatusCode, String) {
        let dir = test_root().join(page);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<title>Lead list</title><p>hi</p>").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "amux.invalid".parse().unwrap());
        // TEST-NET-3 with a reserved TLD: a remote browser on every machine.
        let peer = "203.0.113.5:443".parse::<std::net::SocketAddr>().unwrap();
        let resp = view(
            State(app_state(token)),
            UrlPath(page.to_string()),
            headers,
            uri.parse().unwrap(),
            Some(axum::Extension(axum::extract::ConnectInfo(peer))),
        )
        .await;
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&body).to_string())
    }

    #[tokio::test]
    async fn a_remote_viewer_without_owner_access_gets_the_sign_in_page() {
        let (status, body) = view_as(Some("tok123"), "/pages/locked", "locked").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(body.contains("Sign in to view this page"), "{body}");
        assert!(!body.contains("tok123") && !body.contains("AmuxPages.mount"), "nothing of the page or the token leaks: {body}");
    }

    #[tokio::test]
    async fn the_owner_token_opens_the_page_with_the_host() {
        let (status, body) = view_as(Some("tok123"), "/pages/open?_token=tok123", "open").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("AmuxPages.mount(document.getElementById('page'), \"open\""), "{body}");
        assert!(body.contains("<title>Lead list</title>"), "the title comes from the page: {body}");
        assert!(body.contains("window._AMUX_AUTH_TOKEN=\"tok123\""), "the host gets the bearer: {body}");
    }

    #[tokio::test]
    async fn a_server_with_no_token_serves_the_page() {
        // A cloud server runs behind its gateway, which signed the visitor in.
        let (status, _) = view_as(None, "/pages/cloudy", "cloudy").await;
        assert_eq!(status, StatusCode::OK);
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("amux-pages-test-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("index.html"), "<title>T</title>").unwrap();
        d
    }

    #[test]
    fn names_are_plain_segments() {
        assert!(valid_name("mtp"));
        assert!(valid_name("lead-research-2"));
        for bad in ["", "MTP", "../x", "a/b", "-x", "a b", "a.b", &"x".repeat(49)] {
            assert!(!valid_name(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_page_statement_cannot_attach_another_database() {
        let d = tmp();
        let conn = open_db(&d).unwrap();
        let other = d.join("other.db");
        let err = conn
            .execute_batch(&format!("ATTACH DATABASE '{}' AS x", other.display()))
            .expect_err("ATTACH must be refused");
        assert!(err.to_string().contains("too many attached"), "{err}");
        assert!(!other.exists(), "the refused ATTACH must not create the file");
        // VACUUM INTO writes a file anywhere; it goes through the same limit.
        let out = d.join("copy.db");
        let r = conn.execute_batch(&format!("VACUUM INTO '{}'", out.display()));
        assert!(r.is_err() && !out.exists(), "VACUUM INTO must be refused on a page connection: {r:?}");
    }

    #[test]
    fn reads_return_objects_and_writes_return_changes() {
        let d = tmp();
        let conn = open_db(&d).unwrap();
        exec(&conn, "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, n REAL)", &Value::Null).unwrap();
        let (w, wrote) = exec(&conn, "INSERT INTO t (name, n) VALUES (?, ?)", &json!(["a", 1.5])).unwrap();
        assert!(wrote);
        assert_eq!(w["changes"], 1);
        exec(&conn, "INSERT INTO t (name, n) VALUES (:name, :n)", &json!({"name": "b", "n": 2})).unwrap();
        let (r, wrote) = exec(&conn, "SELECT name, n FROM t ORDER BY id", &Value::Null).unwrap();
        assert!(!wrote);
        assert_eq!(r["rows"], json!([{"name": "a", "n": 1.5}, {"name": "b", "n": 2.0}]));
    }

    #[test]
    fn a_text_blob_reads_as_text_and_binary_as_base64() {
        let d = tmp();
        let conn = open_db(&d).unwrap();
        conn.execute_batch("CREATE TABLE t (b BLOB); INSERT INTO t VALUES (CAST('<p>Hi</p>' AS BLOB)), (X'FF00');").unwrap();
        let (r, _) = exec(&conn, "SELECT b, typeof(b) AS ty FROM t ORDER BY rowid", &Value::Null).unwrap();
        assert_eq!(r["rows"][0], json!({"b": "<p>Hi</p>", "ty": "blob"}));
        assert_eq!(r["rows"][1]["b"], json!({"base64": "/wA=", "bytes": 2}));
    }

    #[test]
    fn only_new_asks_above_the_mark_are_undelivered() {
        let d = tmp();
        let conn = open_db(&d).unwrap();
        for t in ["one", "two", "three"] {
            conn.execute("INSERT INTO asks (kind, text) VALUES ('ask', ?1)", [t]).unwrap();
        }
        conn.execute("UPDATE asks SET status='done' WHERE id=2", []).unwrap();
        let a = undelivered(&d).unwrap();
        assert_eq!(a.iter().map(|x| x.0).collect::<Vec<_>>(), vec![1, 3]);
        mark_delivered(&d, 3).unwrap();
        assert!(undelivered(&d).unwrap().is_empty(), "delivered asks are not sent twice");
        // The mark only moves forward.
        mark_delivered(&d, 1).unwrap();
        conn.execute("INSERT INTO asks (kind, text) VALUES ('ask', 'four')", []).unwrap();
        assert_eq!(undelivered(&d).unwrap().iter().map(|x| x.0).collect::<Vec<_>>(), vec![4]);
        let text = delivery_text("mtp", &d, &undelivered(&d).unwrap());
        assert!(text.contains("#4 ask: four") && text.contains("status to 'done'"), "{text}");
    }

    #[test]
    fn a_snapshot_restores_data_and_html() {
        let d = tmp();
        let conn = open_db(&d).unwrap();
        conn.execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('before');").unwrap();
        let id = take_snapshot(&d, "test").unwrap();
        conn.execute("UPDATE t SET v='after'", []).unwrap();
        drop(conn);
        std::fs::write(d.join("index.html"), "<title>changed</title>").unwrap();
        let src = snapshot_dir(&d).join(&id);
        assert!(src.join("page.db").is_file() && src.join("index.html").is_file());
        std::fs::copy(src.join("page.db"), d.join("page.db")).unwrap();
        for f in ["page.db-wal", "page.db-shm"] {
            let _ = std::fs::remove_file(d.join(f));
        }
        let conn = open_db(&d).unwrap();
        let v: String = conn.query_row("SELECT v FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "before");
    }
}
