//! `GET /api/move/export` · `POST /api/move/import`: the move bundle
//! (docs/design/cloud-self-service.md section 7.1).
//!
//! One JSON document carries a whole amux server from one place to another:
//! local to Cloud, Cloud to local, Cloud to Cloud. It holds the declarative
//! config (`/api/config/export`, unchanged) plus the user's data from an
//! explicit table ALLOWLIST.
//!
//! # Never a raw database copy
//!
//! `amux.db` also holds secrets: ChatGPT OAuth tokens, share, member and
//! invite tokens, connector state. So the bundle names the tables it carries,
//! and the importer refuses any table not on [`ALLOWLIST`]. The unit test
//! `allowlist_carries_no_secret_shaped_table_or_column` fails if a table whose
//! name or columns look like a secret is ever added.
//!
//! # Import only adds
//!
//! Rows merge by primary key (or a natural key for tables whose integer id is
//! local to one server). A row that already exists is left alone, never
//! overwritten; nothing is deleted. All rows go in one transaction, so a
//! failure leaves the target untouched. Then config goes through the existing
//! config-apply path, which DECLARES workers and starts none.
//!
//! Dry run is the default (`?dry_run=1`); it reports per-table new/existing
//! counts and the config diff apply would make. Only the host owner may move a
//! server: org members are refused by `org::authorize_local_member_request`.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};

use super::AppState;

pub const FORMAT: &str = "amuxmove";
pub const VERSION: i64 = 1;

/// One table the bundle may carry.
pub struct MoveTable {
    pub name: &'static str,
    /// Columns that identify a row across servers. A row whose key already
    /// exists on the target is skipped.
    pub key: &'static [&'static str],
    /// Columns exported but not inserted: an AUTOINCREMENT id means nothing
    /// on another server, so those tables merge on a natural key instead.
    pub drop_on_import: &'static [&'static str],
}

/// User data that moves. Parents come before children (issue_tags after
/// issues), so foreign keys resolve inside the one import transaction.
///
/// Deliberately absent: everything holding a credential (`chatgpt_oauth_*`,
/// `share_tokens`, `org_invites`, `org_members`, `push_subscriptions`,
/// `email_accounts`, `telegram_*`), per-machine runtime state (`_amux_workers`,
/// `_amux_sessions`, leases, request logs, host metrics) and the CDC log
/// `board_change_log`, whose rows the target writes itself on insert.
/// Largest bundle `/api/move/import` accepts, on the wire and after gzip is undone.
pub const MAX_IMPORT_BYTES: usize = 1024 * 1024 * 1024;

/// The request body as JSON, gunzipped first when `Content-Encoding: gzip`.
fn bundle_json(headers: &HeaderMap, body: &[u8]) -> Result<Value, String> {
    let gz = headers
        .get(axum::http::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("gzip"));
    if !gz {
        return serde_json::from_slice(body).map_err(|e| format!("bundle is not valid JSON: {e}"));
    }
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(body)
        .take(MAX_IMPORT_BYTES as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("bundle is not valid gzip: {e}"))?;
    if out.len() > MAX_IMPORT_BYTES {
        return Err(format!("bundle is larger than {} MB once decompressed", MAX_IMPORT_BYTES >> 20));
    }
    serde_json::from_slice(&out).map_err(|e| format!("bundle is not valid JSON: {e}"))
}

pub const ALLOWLIST: &[MoveTable] = &[
    // Board cards. Each card's history travels in its own `log` column.
    MoveTable { name: "issues", key: &["id"], drop_on_import: &[] },
    MoveTable { name: "issue_tags", key: &["issue_id", "tag"], drop_on_import: &[] },
    MoveTable { name: "issue_files", key: &["issue_id", "path"], drop_on_import: &[] },
    MoveTable { name: "schedules", key: &["id"], drop_on_import: &[] },
    MoveTable { name: "_amux_memories", key: &["id"], drop_on_import: &[] },
    MoveTable { name: "crm_contacts", key: &["id"], drop_on_import: &[] },
    MoveTable { name: "crm_tags", key: &["contact_id", "tag"], drop_on_import: &[] },
    MoveTable { name: "crm_interactions", key: &["id"], drop_on_import: &[] },
    MoveTable { name: "skills", key: &["name"], drop_on_import: &[] },
    MoveTable { name: "cal_events", key: &["id"], drop_on_import: &[] },
    MoveTable { name: "journal_entries", key: &["id"], drop_on_import: &[] },
    MoveTable { name: "_amux_messages", key: &["id"], drop_on_import: &[] },
    // Messages tab posts: integer id is local, so merge on who/where/when.
    MoveTable {
        name: "conversation_posts",
        key: &["conv", "author_key", "ts"],
        drop_on_import: &["id"],
    },
];

fn allowlisted(name: &str) -> Option<&'static MoveTable> {
    ALLOWLIST.iter().find(|t| t.name == name)
}

fn err(status: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "ok": false, "error": msg.into() })))
}

/// Highest applied migration: the schema this server's rows are shaped by.
fn schema_version(conn: &rusqlite::Connection) -> i64 {
    conn.query_row("SELECT COALESCE(MAX(version), 0) FROM _amux_migrations", [], |r| r.get(0))
        .unwrap_or(0)
}

fn table_columns(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let Ok(mut st) = conn.prepare(&format!("PRAGMA table_info(\"{table}\")")) else {
        return Vec::new();
    };
    st.query_map([], |r| r.get::<_, String>(1))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

fn sql_to_json(v: SqlValue) -> Value {
    match v {
        SqlValue::Null => Value::Null,
        SqlValue::Integer(i) => json!(i),
        SqlValue::Real(f) => json!(f),
        SqlValue::Text(s) => json!(s),
        SqlValue::Blob(b) => json!({ "$blob_hex": hex::encode(b) }),
    }
}

fn json_to_sql(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => SqlValue::Real(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Object(m) if m.len() == 1 && m.contains_key("$blob_hex") => m["$blob_hex"]
            .as_str()
            .and_then(|h| hex::decode(h).ok())
            .map(SqlValue::Blob)
            .unwrap_or(SqlValue::Null),
        other => SqlValue::Text(other.to_string()),
    }
}

fn dump_table(conn: &rusqlite::Connection, table: &str) -> Result<Vec<Value>, String> {
    let cols = table_columns(conn, table);
    if cols.is_empty() {
        return Ok(Vec::new()); // table absent on this schema: nothing to carry
    }
    let mut st = conn
        .prepare(&format!("SELECT * FROM \"{table}\""))
        .map_err(|e| format!("{table}: {e}"))?;
    let rows = st
        .query_map([], |r| {
            let mut m = Map::new();
            for (i, c) in cols.iter().enumerate() {
                m.insert(c.clone(), sql_to_json(r.get::<_, SqlValue>(i)?));
            }
            Ok(Value::Object(m))
        })
        .map_err(|e| format!("{table}: {e}"))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|e| format!("{table}: {e}"))
}

/// The whole server as one document.
pub async fn export(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let config = match super::config_iac::export_doc(&state) {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store unreadable: {e}")),
    };
    let mut tables = Map::new();
    let mut counts = Map::new();
    for t in ALLOWLIST {
        match dump_table(&conn, t.name) {
            Ok(rows) => {
                counts.insert(t.name.into(), json!(rows.len()));
                tables.insert(t.name.into(), Value::Array(rows));
            }
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
        }
    }
    let schema = schema_version(&conn);
    let total: u64 = counts.values().filter_map(Value::as_u64).sum();
    tracing::info!(verdict = "move_export", schema_version = schema, rows = total,
        tables = %serde_json::to_string(&counts).unwrap_or_default(), "move bundle exported");
    (
        StatusCode::OK,
        Json(json!({
            "format": FORMAT,
            "version": VERSION,
            "schema_version": schema,
            "source_commit": env!("AMUX_BUILD_COMMIT"),
            "created_at": chrono::Utc::now().to_rfc3339(),
            "counts": counts,
            "config": config,
            "tables": tables,
        })),
    )
}

#[derive(serde::Deserialize, Default)]
pub struct ImportQuery {
    /// Default true: an import that has not been asked to apply only reports.
    pub dry_run: Option<String>,
}

/// A table's plan: which rows are new on the target, which already exist.
struct TablePlan {
    name: &'static str,
    columns: Vec<String>,
    new_rows: Vec<Vec<SqlValue>>,
    existing: usize,
    skipped: Vec<Value>,
}

/// Validate the bundle's shape against this server. Errors here refuse the
/// whole import: a bundle we do not understand must not be half-applied.
fn validate(bundle: &Value, target_schema: i64) -> Result<&Map<String, Value>, String> {
    if bundle.get("format").and_then(Value::as_str) != Some(FORMAT) {
        return Err(format!("not a move bundle: format must be \"{FORMAT}\""));
    }
    let version = bundle.get("version").and_then(Value::as_i64);
    if version != Some(VERSION) {
        return Err(format!(
            "unsupported bundle version {}: this server reads version {VERSION}",
            version.map(|v| v.to_string()).unwrap_or_else(|| "(missing)".into())
        ));
    }
    let Some(source_schema) = bundle.get("schema_version").and_then(Value::as_i64) else {
        return Err("bundle has no schema_version".into());
    };
    if source_schema > target_schema {
        return Err(format!(
            "bundle schema {source_schema} is newer than this server's {target_schema}: \
             update this amux first, then import"
        ));
    }
    static EMPTY: OnceLock<Map<String, Value>> = OnceLock::new();
    let tables = match bundle.get("tables") {
        None | Some(Value::Null) => EMPTY.get_or_init(Map::new),
        Some(Value::Object(m)) => m,
        Some(_) => return Err("tables must be an object of table -> rows".into()),
    };
    let unknown: Vec<&String> = tables.keys().filter(|k| allowlisted(k).is_none()).collect();
    if !unknown.is_empty() {
        return Err(format!(
            "refusing tables not on the move allowlist: {}",
            unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    Ok(tables)
}

fn plan(conn: &rusqlite::Connection, tables: &Map<String, Value>) -> Result<Vec<TablePlan>, String> {
    let mut plans = Vec::new();
    for t in ALLOWLIST {
        let Some(rows) = tables.get(t.name) else { continue };
        let Some(rows) = rows.as_array() else {
            return Err(format!("{}: rows must be an array", t.name));
        };
        let target_cols = table_columns(conn, t.name);
        let mut p = TablePlan {
            name: t.name,
            columns: Vec::new(),
            new_rows: Vec::new(),
            existing: 0,
            skipped: Vec::new(),
        };
        if target_cols.is_empty() {
            p.skipped.push(json!({"rows": rows.len(), "reason": "table does not exist on this server"}));
            plans.push(p);
            continue;
        }
        let where_sql = t
            .key
            .iter()
            .enumerate()
            .map(|(i, k)| format!("\"{k}\" IS ?{}", i + 1))
            .collect::<Vec<_>>()
            .join(" AND ");
        let mut exists = conn
            .prepare(&format!("SELECT 1 FROM \"{}\" WHERE {where_sql} LIMIT 1", t.name))
            .map_err(|e| format!("{}: {e}", t.name))?;
        let mut seen: HashSet<String> = HashSet::new();
        // The insert column list: every target column the bundle can fill.
        // Columns the source lacks (an older schema) take the target default.
        let mut cols_union: Vec<String> = Vec::new();
        let mut staged: Vec<Map<String, Value>> = Vec::new();
        for row in rows {
            let Some(obj) = row.as_object() else {
                p.skipped.push(json!({"row": row, "reason": "row is not an object"}));
                continue;
            };
            let mut obj = obj.clone();
            for d in t.drop_on_import {
                obj.remove(*d);
            }
            if let Some(bad) = obj.keys().find(|c| !target_cols.contains(c)) {
                return Err(format!(
                    "{}: column '{bad}' does not exist on this server (bundle schema differs)",
                    t.name
                ));
            }
            // Schedules arrive PAUSED. The source keeps firing until its owner
            // turns it off, so an enabled copy here double-fires every one,
            // and a tmux schedule's worker does not exist on this host yet.
            if t.name == "schedules" && target_cols.iter().any(|c| c == "enabled") {
                obj.insert("enabled".into(), json!(0));
            }
            if t.key.iter().any(|k| obj.get(*k).is_none_or(Value::is_null)) {
                p.skipped.push(json!({"row": row, "reason": format!("missing key {:?}", t.key)}));
                continue;
            }
            let key_vals: Vec<SqlValue> = t.key.iter().map(|k| json_to_sql(&obj[*k])).collect();
            let fingerprint = Value::Array(t.key.iter().map(|k| obj[*k].clone()).collect()).to_string();
            if !seen.insert(fingerprint) {
                p.skipped.push(json!({"key": t.key.iter().map(|k| obj[*k].clone()).collect::<Vec<_>>(),
                    "reason": "duplicate key inside the bundle"}));
                continue;
            }
            let found = exists
                .exists(rusqlite::params_from_iter(key_vals.iter()))
                .map_err(|e| format!("{}: {e}", t.name))?;
            if found {
                p.existing += 1;
                continue;
            }
            for c in obj.keys() {
                if !cols_union.contains(c) {
                    cols_union.push(c.clone());
                }
            }
            staged.push(obj);
        }
        p.new_rows = staged
            .iter()
            .map(|o| {
                cols_union
                    .iter()
                    .map(|c| o.get(c).map(json_to_sql).unwrap_or(SqlValue::Null))
                    .collect()
            })
            .collect();
        p.columns = cols_union;
        plans.push(p);
    }
    Ok(plans)
}

/// Insert every planned row. Called inside the writer's transaction, so an
/// error rolls all of it back. A row that collides with a DIFFERENT unique
/// key on the target (a memory with the same scope and name under another
/// id) is left out and reported, never overwritten: the plan moves it from
/// `new` to `skipped`.
fn insert_all(conn: &rusqlite::Connection, plans: &mut [TablePlan]) -> rusqlite::Result<()> {
    for p in plans.iter_mut() {
        let mut kept: Vec<Vec<SqlValue>> = Vec::new();
        if p.new_rows.is_empty() {
            continue;
        }
        // A column absent from some rows would insert NULL over a NOT NULL
        // default; bind only the columns each row actually carries.
        for row in std::mem::take(&mut p.new_rows) {
            let (cols, vals): (Vec<&String>, Vec<&SqlValue>) = p
                .columns
                .iter()
                .zip(row.iter())
                .filter(|(_, v)| !matches!(v, SqlValue::Null))
                .unzip();
            let sql = format!(
                "INSERT INTO \"{}\" ({}) VALUES ({}) ON CONFLICT DO NOTHING",
                p.name,
                cols.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(", "),
                (1..=cols.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(", ")
            );
            let n = conn.execute(&sql, rusqlite::params_from_iter(vals))?;
            if n == 0 {
                let key: Vec<Value> = p
                    .columns
                    .iter()
                    .zip(row.iter())
                    .filter(|(c, _)| ["id", "name"].contains(&c.as_str()))
                    .map(|(_, v)| sql_to_json(v.clone()))
                    .collect();
                p.skipped.push(json!({"key": key,
                    "reason": "conflicts with a different existing row (unique constraint); left as is"}));
            } else {
                kept.push(row);
            }
        }
        p.new_rows = kept;
    }
    // Imported card and schedule ids (AMUX-123, SCHED-9) must never be
    // minted again here: lift each prefix's counter past the highest one.
    for p in plans.iter().filter(|p| p.name == "issues" || p.name == "schedules") {
        let Some(idx) = p.columns.iter().position(|c| c == "id") else { continue };
        for row in &p.new_rows {
            let SqlValue::Text(id) = &row[idx] else { continue };
            let Some((prefix, n)) = id.rsplit_once('-') else { continue };
            let Ok(n) = n.parse::<i64>() else { continue };
            conn.execute(
                "INSERT INTO issue_counters (prefix, next_n) VALUES (?1, ?2) \
                 ON CONFLICT(prefix) DO UPDATE SET next_n = MAX(next_n, excluded.next_n)",
                rusqlite::params![prefix, n + 1],
            )?;
        }
    }
    Ok(())
}

const SCHEDULES_NOTE: &str = "Imported schedules arrive paused so they do not fire twice. \
Turn them off where they came from, then enable them here (Schedules tab).";

fn table_report(plans: &[TablePlan]) -> Map<String, Value> {
    plans
        .iter()
        .map(|p| {
            let mut v = json!({"new": p.new_rows.len(), "existing": p.existing});
            if !p.skipped.is_empty() {
                v["skipped"] = json!(p.skipped);
            }
            (p.name.to_string(), v)
        })
        .collect()
}

/// Dry run (default) or apply a move bundle.
pub async fn import(
    State(state): State<AppState>,
    Query(q): Query<ImportQuery>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> (StatusCode, Json<Value>) {
    let bundle = match bundle_json(&headers, &body) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(verdict = "move_import_refused", reason = %e, "move import refused");
            return err(StatusCode::BAD_REQUEST, e);
        }
    };
    drop(body);
    let dry_run = !matches!(q.dry_run.as_deref(), Some("0" | "false" | "no"));
    if !dry_run {
        if let Some(e) = super::config_iac::agent_write_refusal(&headers, "import a move bundle") {
            tracing::warn!(verdict = "move_import_refused", reason = "agent_session", "move import refused");
            return err(StatusCode::FORBIDDEN, e);
        }
    }
    let (target_schema, plans) = {
        let conn = match state.store.read() {
            Ok(c) => c,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("store unreadable: {e}")),
        };
        let target_schema = schema_version(&conn);
        let tables = match validate(&bundle, target_schema) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(verdict = "move_import_refused", reason = %e, "move import refused");
                return err(StatusCode::BAD_REQUEST, e);
            }
        };
        match plan(&conn, tables) {
            Ok(p) => (target_schema, p),
            Err(e) => {
                tracing::warn!(verdict = "move_import_refused", reason = %e, "move import refused");
                return err(StatusCode::BAD_REQUEST, e);
            }
        }
    };
    let config = bundle.get("config").filter(|c| c.is_object());
    let new_total: usize = plans.iter().map(|p| p.new_rows.len()).sum();
    let existing_total: usize = plans.iter().map(|p| p.existing).sum();

    if dry_run {
        let config_report = match config {
            Some(c) => super::config_iac::apply_doc(&state, c, true).await.1,
            None => json!({"summary": "bundle carries no config"}),
        };
        tracing::info!(verdict = "move_import_dry_run", new = new_total, existing = existing_total,
            "move bundle dry run");
        return (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "dry_run": true,
                "schema_version": target_schema,
                "tables": table_report(&plans),
                "config": config_report,
                "summary": format!("{new_total} rows would be imported, {existing_total} already here"),
                "note": SCHEDULES_NOTE,
            })),
        );
    }

    // Plan again inside the write so a row added between the dry run above and
    // this transaction is still treated as existing, never duplicated.
    let tables = validate(&bundle, target_schema).cloned().unwrap_or_default();
    let applied: Arc<Mutex<Option<Map<String, Value>>>> = Arc::new(Mutex::new(None));
    let out = applied.clone();
    let res = state
        .store
        .write_async(move |conn| {
            let mut plans = plan(conn, &tables).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
            })?;
            insert_all(conn, &mut plans)?;
            *out.lock().unwrap() = Some(table_report(&plans));
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        tracing::warn!(verdict = "move_import_failed", error = %e,
            "move import rolled back; nothing was written");
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("import rolled back, nothing was written: {e}"),
        );
    }
    let report = applied.lock().unwrap().take().unwrap_or_default();
    let imported: u64 = report.values().filter_map(|v| v["new"].as_u64()).sum();
    let existing: u64 = report.values().filter_map(|v| v["existing"].as_u64()).sum();
    let (config_ok, config_report) = match config {
        Some(c) => super::config_iac::apply_doc(&state, c, false).await,
        None => (true, json!({"summary": "bundle carries no config"})),
    };
    tracing::warn!(verdict = "move_import_applied", imported, existing, config_ok,
        tables = %serde_json::to_string(&report).unwrap_or_default(),
        "move bundle imported (rows added, none overwritten)");
    (
        if config_ok { StatusCode::OK } else { StatusCode::MULTI_STATUS },
        Json(json!({
            "ok": config_ok,
            "dry_run": false,
            "schema_version": target_schema,
            "tables": report,
            "config": config_report,
            "summary": format!("{imported} rows imported, {existing} already here (skipped, not overwritten)"),
            "note": format!("workers are declared, not started: `amux workers start <name>`. {SCHEDULES_NOTE}"),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No table on the allowlist may look like it holds a secret, by name or by
    /// any column, on a fully migrated schema. Adding `chatgpt_oauth_tokens`,
    /// `share_tokens`, `org_invites` (its `token` column) or a table with a
    /// `password` column fails here.
    #[test]
    fn allowlist_carries_no_secret_shaped_table_or_column() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("move.db")).unwrap();
        let conn = store.read().unwrap();
        const SECRETISH: &[&str] = &["token", "secret", "credential", "oauth", "password", "api_key"];
        let mut checked = 0;
        for t in ALLOWLIST {
            let lname = t.name.to_lowercase();
            assert!(
                !SECRETISH.iter().any(|s| lname.contains(s)),
                "move allowlist table {} looks like it holds secrets",
                t.name
            );
            let cols = table_columns(&conn, t.name);
            assert!(!cols.is_empty(), "allowlisted table {} does not exist in the schema", t.name);
            for c in &cols {
                let lc = c.to_lowercase();
                assert!(
                    !SECRETISH.iter().any(|s| lc.contains(s)),
                    "move allowlist table {} has secret-shaped column {c}",
                    t.name
                );
            }
            for k in t.key.iter().chain(t.drop_on_import) {
                assert!(cols.iter().any(|c| c == k), "{}: key column {k} missing", t.name);
            }
            checked += 1;
        }
        assert_eq!(checked, ALLOWLIST.len());
        // The probe can fire: tables that really hold secrets trip it, by
        // column as well as by name (org_invites carries its token in a column).
        for secret_table in ["org_invites", "share_tokens"] {
            let cols = table_columns(&conn, secret_table);
            assert!(
                cols.iter().any(|c| SECRETISH.iter().any(|s| c.to_lowercase().contains(s))),
                "{secret_table} columns {cols:?} should look secret-shaped"
            );
        }
    }

    /// A moved schedule must not fire on both servers at once.
    #[test]
    fn imported_schedules_arrive_paused() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("move.db")).unwrap();
        let conn = store.read().unwrap();
        let tables = json!({"schedules": [{"id": "SCHED-7", "title": "t",
            "schedule_expr": "every 1m", "enabled": 1}]});
        let plans = plan(&conn, tables.as_object().unwrap()).unwrap();
        let p = plans.iter().find(|p| p.name == "schedules").unwrap();
        let idx = p.columns.iter().position(|c| c == "enabled").unwrap();
        assert_eq!(p.new_rows.len(), 1);
        assert!(matches!(p.new_rows[0][idx], SqlValue::Integer(0)), "{:?}", p.new_rows[0][idx]);
    }

    #[test]
    fn validate_refuses_unknown_tables_formats_and_newer_schemas() {
        let ok = json!({"format": FORMAT, "version": 1, "schema_version": 5, "tables": {"issues": []}});
        assert!(validate(&ok, 5).is_ok());
        let e = validate(&ok, 4).unwrap_err();
        assert!(e.contains("newer"), "{e}");
        let bad = json!({"format": FORMAT, "version": 1, "schema_version": 1,
            "tables": {"share_tokens": []}});
        assert!(validate(&bad, 5).unwrap_err().contains("share_tokens"));
        let wrong = json!({"format": "sqlite", "version": 1, "schema_version": 1});
        assert!(validate(&wrong, 5).is_err());
        let v2 = json!({"format": FORMAT, "version": 2, "schema_version": 1});
        assert!(validate(&v2, 5).unwrap_err().contains("version 2"));
    }
}
