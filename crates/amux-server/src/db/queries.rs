//! Worker/session row CRUD (RR-0034 persistence; Invariants 17, 35, 37, 43).
//!
//! Every function takes `&rusqlite::Connection` so it composes inside
//! `Store::write` closures (the single-writer discipline in `db/mod.rs`) as
//! well as against read-pool connections. Nothing here opens a transaction —
//! the writer loop owns transaction boundaries.
//!
//! Optimistic concurrency (Invariant 35): [`update_worker_config`] guards on
//! `WHERE version = ?expected` and returns rows-affected, so a stale write
//! reports 0 and the caller surfaces a conflict instead of silently
//! clobbering. Inside the single-writer this is belt-and-braces; the guard
//! exists because these queries are also usable from contexts that do not
//! enjoy the writer's serialization.
//!
//! LIFECYCLE (migration 0069): the `lifecycle` column on `_amux_workers`
//! carries the canonical lifecycle state (active/paused/archived/deleted).
//! This replaces the soft-delete sidecar that stored `deleted_at` inside the
//! state JSON column, which was a named deviation resolved by 0067.

use amux_core::ids::{GroupId, WorkerId};
use amux_core::session::ExitReason;
use amux_core::worker::{WorkerConfig, WorkerLifecycle, WorkerState};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::BTreeMap;

/// Column list shared by every worker SELECT so `worker_from_row`'s indices
/// cannot drift from the query text. `lifecycle` is appended after
/// `updated_at` (migration 0067 adds the column; ALTER TABLE ADD COLUMN
/// appends, so existing index positions are stable).
const WORKER_COLS: &str = "id, display_name, name_aliases, cwd, provider, model, backend, \
     environment, permissions, group_id, state, version, created_at, updated_at, lifecycle, worker_type";

/// The lifecycle filter replacing the old soft-delete sidecar. Applied to
/// every normal read and guarded write so deleted workers cannot be resolved
/// or mutated back to life.
const NOT_DELETED: &str = "lifecycle != 'deleted'";

/// One `_amux_workers` row.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerRow {
    pub id: String,
    pub display_name: String,
    pub name_aliases: Vec<String>,
    pub cwd: String,
    pub provider: String,
    pub model: Option<String>,
    pub backend: String,
    pub environment: BTreeMap<String, String>,
    pub permissions: Vec<String>,
    pub group_id: Option<String>,
    pub state: WorkerState,
    /// Legacy compat: derived from lifecycle == Deleted + updated_at.
    /// Replay snapshots include this for continuity with pre-0067 events.
    pub deleted_at: Option<String>,
    pub version: u64,
    pub created_at: String,
    pub updated_at: String,
    pub lifecycle: WorkerLifecycle,
    /// Migration 0087; `coding` for every row that predates it.
    pub worker_type: String,
}

impl WorkerRow {
    /// A fresh worker: version 0, state Stopped, lifecycle Active.
    pub fn new(id: &WorkerId, config: &WorkerConfig, now: &str) -> WorkerRow {
        let mut row = WorkerRow {
            id: id.as_str().to_string(),
            display_name: String::new(),
            name_aliases: Vec::new(),
            cwd: String::new(),
            provider: String::new(),
            model: None,
            backend: String::new(),
            environment: BTreeMap::new(),
            permissions: Vec::new(),
            group_id: None,
            state: WorkerState::Stopped,
            deleted_at: None,
            version: 0,
            created_at: now.to_string(),
            updated_at: now.to_string(),
            lifecycle: WorkerLifecycle::Active,
            worker_type: String::new(),
        };
        row.set_config(config);
        row
    }

    /// Overwrite the config-derived columns from a `WorkerConfig`. Identity
    /// (`id`), state, version and timestamps are untouched (Invariant 43:
    /// config is mutable, identity is not — this helper cannot even express
    /// an id change).
    pub fn set_config(&mut self, config: &WorkerConfig) {
        self.display_name = config.display_name.clone();
        self.name_aliases = config.name_aliases.clone();
        self.cwd = config.cwd.clone();
        self.provider = config.provider.as_str().to_string();
        self.model = config.model.clone();
        self.backend = config.backend.as_str().to_string();
        self.environment = config.environment.clone();
        self.permissions = config.permissions.clone();
        self.group_id = config.group.as_ref().map(|g| g.as_str().to_string());
        self.worker_type = config.worker_type.as_str().to_string();
    }

    /// RR-0111a: the canonical replay snapshot of this row. Written into the
    /// event journal as the event's `payload` at mutation time, and rebuilt
    /// from the live row at verify time — ONE function on both sides, so
    /// `db::replay::verify_replay`'s comparison cannot drift from what the
    /// writer recorded (ethos rule 1's corollary: a view must share the
    /// predicate of the mechanism it describes).
    pub fn snapshot(&self) -> serde_json::Value {
        let mut snap = serde_json::json!({
            "id": self.id,
            "display_name": self.display_name,
            "name_aliases": self.name_aliases,
            "cwd": self.cwd,
            "provider": self.provider,
            "model": self.model,
            "backend": self.backend,
            "environment": self.environment,
            "permissions": self.permissions,
            "lifecycle": self.lifecycle.as_str(),
            "group_id": self.group_id,
            "state": serde_json::to_value(&self.state)
                .unwrap_or_else(|_| serde_json::json!({"state": "stopped"})),
            "deleted_at": self.deleted_at,
            "version": self.version,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        });
        // Absent means coding (migration 0087), so a coding row's snapshot is
        // byte-identical to the ones journaled before the column existed and
        // replay verification of old events keeps matching.
        if self.worker_type != amux_core::worker_type::WorkerTypeId::CODING {
            snap["worker_type"] = serde_json::json!(self.worker_type);
        }
        snap
    }

    /// The row's config as the core type, for `classify_config_change` /
    /// `apply_config`.
    pub fn config(&self) -> WorkerConfig {
        WorkerConfig {
            worker_type: amux_core::worker_type::WorkerTypeId::new(self.worker_type.clone()),
            display_name: self.display_name.clone(),
            name_aliases: self.name_aliases.clone(),
            cwd: self.cwd.clone(),
            provider: amux_core::provider::ProviderId::new(self.provider.clone()),
            model: self.model.clone(),
            backend: amux_core::session::BackendId::from(self.backend.clone()),
            environment: self.environment.clone(),
            permissions: self.permissions.clone(),
            // Rows are only ever written from valid `GroupId`s, so a parse
            // failure here means DB corruption; dropping to None is the
            // read-side containment (the write path never accepts bad ids).
            group: self
                .group_id
                .as_deref()
                .and_then(|g| GroupId::parse(g).ok()),
        }
    }
}

/// One `_amux_sessions` row (Invariant 1: disposable execution instance).
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub worker_id: String,
    pub backend: String,
    pub backend_ref: String,
    pub pid: Option<i64>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub exit_reason: Option<ExitReason>,
}

// ---- JSON column helpers ------------------------------------------------

fn json_col_err(idx: usize, e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
}

fn json_col<T: serde::de::DeserializeOwned>(raw: &str, idx: usize) -> rusqlite::Result<T> {
    serde_json::from_str(raw).map_err(|e| json_col_err(idx, e))
}

/// Serialize a `WorkerState` for the `state` column. The old soft-delete
/// sidecar is no longer written; lifecycle carries deletion status since
/// migration 0067.
fn state_to_column(state: &WorkerState) -> String {
    serde_json::to_value(state)
        .unwrap_or_else(|_| serde_json::json!({"state": "stopped"}))
        .to_string()
}

/// Parse the `state` column. Tolerates (and strips) a leftover `deleted_at`
/// sidecar from pre-0067 data that the migration UPDATE missed (e.g. a row
/// inserted between migration start and the UPDATE step, though in practice
/// the migration is atomic).
fn state_from_column(raw: &str, idx: usize) -> rusqlite::Result<WorkerState> {
    let mut v: serde_json::Value = serde_json::from_str(raw).map_err(|e| json_col_err(idx, e))?;
    if let Some(obj) = v.as_object_mut() {
        obj.remove("deleted_at");
    }
    let state: WorkerState = serde_json::from_value(v).map_err(|e| json_col_err(idx, e))?;
    Ok(state)
}

fn worker_from_row(r: &Row<'_>) -> rusqlite::Result<WorkerRow> {
    let name_aliases: Vec<String> = json_col(&r.get::<_, String>(2)?, 2)?;
    let environment: BTreeMap<String, String> = json_col(&r.get::<_, String>(7)?, 7)?;
    let permissions: Vec<String> = json_col(&r.get::<_, String>(8)?, 8)?;
    let state = state_from_column(&r.get::<_, String>(10)?, 10)?;
    let updated_at: String = r.get(13)?;
    let lifecycle_raw: String = r.get(14)?;
    let lifecycle = WorkerLifecycle::parse(&lifecycle_raw).unwrap_or(WorkerLifecycle::Active);
    let deleted_at = if lifecycle == WorkerLifecycle::Deleted {
        Some(updated_at.clone())
    } else {
        None
    };
    Ok(WorkerRow {
        id: r.get(0)?,
        display_name: r.get(1)?,
        name_aliases,
        cwd: r.get(3)?,
        provider: r.get(4)?,
        model: r.get(5)?,
        backend: r.get(6)?,
        environment,
        permissions,
        group_id: r.get(9)?,
        state,
        deleted_at,
        version: r.get::<_, i64>(11)? as u64,
        created_at: r.get(12)?,
        updated_at,
        lifecycle,
        worker_type: r.get(15)?,
    })
}

fn one_worker(conn: &Connection, sql: &str, key: &str) -> rusqlite::Result<Option<WorkerRow>> {
    conn.query_row(sql, params![key], worker_from_row)
        .optional()
}

// ---- worker CRUD --------------------------------------------------------

pub fn insert_worker(conn: &Connection, row: &WorkerRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO _amux_workers (id, display_name, name_aliases, cwd, provider, model, \
         backend, environment, permissions, group_id, state, version, created_at, updated_at, \
         lifecycle, worker_type) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        params![
            row.id,
            row.display_name,
            serde_json::to_string(&row.name_aliases).map_err(|e| json_col_err(2, e))?,
            row.cwd,
            row.provider,
            row.model,
            row.backend,
            serde_json::to_string(&row.environment).map_err(|e| json_col_err(7, e))?,
            serde_json::to_string(&row.permissions).map_err(|e| json_col_err(8, e))?,
            row.group_id,
            state_to_column(&row.state),
            row.version as i64,
            row.created_at,
            row.updated_at,
            row.lifecycle.as_str(),
            row.worker_type,
        ],
    )?;
    Ok(())
}

/// Resolve a worker by id, display_name, or alias (Invariant 17: `@old-name`
/// keeps resolving after a rename). Resolution priority mirrors the plan:
///
/// 1. `wrk_<ULID>` shape -> lookup by id
/// 2. exact `display_name` match
/// 3. exact match on any entry in `name_aliases`
///
/// (The plan's step 4, unambiguous prefix match, is deliberately NOT
/// implemented here — it belongs with the @mention parser in RR-0044, where
/// ambiguity has a user to report to.)
///
/// Soft-deleted workers never resolve. Ties (duplicate names) break by
/// `created_at, id` so the answer is at least deterministic.
pub fn get_worker(conn: &Connection, key: &str) -> rusqlite::Result<Option<WorkerRow>> {
    if WorkerId::parse(key).is_ok() {
        return one_worker(
            conn,
            &format!("SELECT {WORKER_COLS} FROM _amux_workers WHERE id = ?1 AND {NOT_DELETED}"),
            key,
        );
    }
    if let Some(row) = one_worker(
        conn,
        &format!(
            "SELECT {WORKER_COLS} FROM _amux_workers \
             WHERE display_name = ?1 AND {NOT_DELETED} ORDER BY created_at, id LIMIT 1"
        ),
        key,
    )? {
        return Ok(Some(row));
    }
    one_worker(
        conn,
        &format!(
            "SELECT {WORKER_COLS} FROM _amux_workers \
             WHERE {NOT_DELETED} AND EXISTS (SELECT 1 FROM json_each(_amux_workers.name_aliases) \
             WHERE json_each.value = ?1) ORDER BY created_at, id LIMIT 1"
        ),
        key,
    )
}

/// Page of live (non-deleted) workers plus the total, for the Invariant-40
/// `PagedResponse` envelope (total lets the API compute `truncated` honestly).
pub fn list_workers(
    conn: &Connection,
    offset: u64,
    limit: u64,
) -> rusqlite::Result<(Vec<WorkerRow>, u64)> {
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM _amux_workers WHERE {NOT_DELETED}"),
        [],
        |r| r.get(0),
    )?;
    let mut stmt = conn.prepare(&format!(
        "SELECT {WORKER_COLS} FROM _amux_workers WHERE {NOT_DELETED} \
         ORDER BY created_at, id LIMIT ?1 OFFSET ?2"
    ))?;
    let rows = stmt.query_map(params![limit as i64, offset as i64], worker_from_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok((out, total as u64))
}

/// Workers filtered by lifecycle. When `lifecycles` is empty, returns all
/// non-deleted workers (same as `list_workers`).
pub fn list_workers_by_lifecycle(
    conn: &Connection,
    lifecycles: &[WorkerLifecycle],
    offset: u64,
    limit: u64,
) -> rusqlite::Result<(Vec<WorkerRow>, u64)> {
    if lifecycles.is_empty() {
        return list_workers(conn, offset, limit);
    }
    let placeholders: Vec<String> = lifecycles
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect();
    let filter = format!("lifecycle IN ({})", placeholders.join(", "));
    let mut count_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    for lc in lifecycles {
        count_params.push(Box::new(lc.as_str().to_string()));
    }
    let count_refs: Vec<&dyn rusqlite::types::ToSql> =
        count_params.iter().map(|b| b.as_ref()).collect();
    let total: i64 = conn.query_row(
        &format!("SELECT COUNT(*) FROM _amux_workers WHERE {filter}"),
        count_refs.as_slice(),
        |r| r.get(0),
    )?;
    let mut list_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    for lc in lifecycles {
        list_params.push(Box::new(lc.as_str().to_string()));
    }
    let lp_len = list_params.len();
    list_params.push(Box::new(limit as i64));
    list_params.push(Box::new(offset as i64));
    let list_refs: Vec<&dyn rusqlite::types::ToSql> =
        list_params.iter().map(|b| b.as_ref()).collect();
    let mut stmt = conn.prepare(&format!(
        "SELECT {WORKER_COLS} FROM _amux_workers WHERE {filter} \
         ORDER BY created_at, id LIMIT ?{} OFFSET ?{}",
        lp_len + 1,
        lp_len + 2,
    ))?;
    let rows = stmt.query_map(list_refs.as_slice(), worker_from_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok((out, total as u64))
}

/// EVERY worker row, soft-deleted included, for replay verification
/// (RR-0111a). Deletion is soft (the row survives with `deleted_at` set) and
/// the Deleted event journals a snapshot of that surviving row — so verify
/// must compare against the WHOLE table, not the resolvable subset the
/// NOT_DELETED filter serves everywhere else.
pub fn all_workers_for_replay(conn: &Connection) -> rusqlite::Result<Vec<WorkerRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {WORKER_COLS} FROM _amux_workers ORDER BY id"
    ))?;
    let rows = stmt.query_map([], worker_from_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Optimistic config write: applies iff the stored version still equals
/// `expected_version`, bumping to `expected_version + 1`. Returns rows
/// affected — 0 means the caller's view was stale (report a conflict, do NOT
/// retry blindly; Invariant 35).
pub fn update_worker_config(
    conn: &Connection,
    id: &str,
    config: &WorkerConfig,
    expected_version: u64,
    now: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        &format!(
            "UPDATE _amux_workers SET display_name = ?2, name_aliases = ?3, cwd = ?4, \
             provider = ?5, model = ?6, backend = ?7, environment = ?8, permissions = ?9, \
             group_id = ?10, version = ?11, updated_at = ?12, worker_type = ?14 \
             WHERE id = ?1 AND version = ?13 AND {NOT_DELETED}"
        ),
        params![
            id,
            config.display_name,
            serde_json::to_string(&config.name_aliases).map_err(|e| json_col_err(2, e))?,
            config.cwd,
            config.provider.as_str(),
            config.model,
            config.backend.as_str(),
            serde_json::to_string(&config.environment).map_err(|e| json_col_err(7, e))?,
            serde_json::to_string(&config.permissions).map_err(|e| json_col_err(8, e))?,
            config.group.as_ref().map(|g| g.as_str().to_string()),
            (expected_version + 1) as i64,
            now,
            expected_version as i64,
            config.worker_type.as_str(),
        ],
    )
}

/// Execution-state write. Does NOT touch `version` or `lifecycle`: version
/// counts config mutations, lifecycle is an independent dimension. Refuses
/// to touch deleted rows, so a late state report cannot resurrect one.
pub fn update_worker_state(
    conn: &Connection,
    id: &str,
    state: &WorkerState,
    now: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        &format!(
            "UPDATE _amux_workers SET state = ?2, updated_at = ?3 WHERE id = ?1 AND {NOT_DELETED}"
        ),
        params![id, state_to_column(state), now],
    )
}

/// Transition lifecycle. Returns rows affected (0 = no such row, already in
/// that lifecycle, or deleted). The `from` guard prevents accidental
/// transitions from states the caller did not expect.
pub fn update_worker_lifecycle(
    conn: &Connection,
    id: &str,
    from: &[WorkerLifecycle],
    to: WorkerLifecycle,
    now: &str,
) -> rusqlite::Result<usize> {
    if from.is_empty() {
        return Ok(0);
    }
    let placeholders: Vec<String> = from
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 4))
        .collect();
    let sql = format!(
        "UPDATE _amux_workers SET lifecycle = ?2, updated_at = ?3 \
         WHERE id = ?1 AND lifecycle IN ({})",
        placeholders.join(", ")
    );
    let mut bound: Vec<Box<dyn rusqlite::types::ToSql>> = vec![
        Box::new(id.to_string()),
        Box::new(to.as_str().to_string()),
        Box::new(now.to_string()),
    ];
    for lc in from {
        bound.push(Box::new(lc.as_str().to_string()));
    }
    let refs: Vec<&dyn rusqlite::types::ToSql> = bound.iter().map(|b| b.as_ref()).collect();
    conn.execute(&sql, refs.as_slice())
}

/// Soft delete via lifecycle. Convenience wrapper preserving the old call
/// shape for callers that already check preconditions.
pub fn soft_delete_worker(conn: &Connection, id: &str, now: &str) -> rusqlite::Result<usize> {
    update_worker_lifecycle(
        conn,
        id,
        &[
            WorkerLifecycle::Active,
            WorkerLifecycle::Paused,
            WorkerLifecycle::Archived,
        ],
        WorkerLifecycle::Deleted,
        now,
    )
}

// ---- session rows -------------------------------------------------------

fn session_from_row(r: &Row<'_>) -> rusqlite::Result<SessionRow> {
    let exit_reason: Option<ExitReason> = match r.get::<_, Option<String>>(7)? {
        Some(raw) => Some(json_col(&raw, 7)?),
        None => None,
    };
    Ok(SessionRow {
        id: r.get(0)?,
        worker_id: r.get(1)?,
        backend: r.get(2)?,
        backend_ref: r.get(3)?,
        pid: r.get(4)?,
        started_at: r.get(5)?,
        ended_at: r.get(6)?,
        exit_reason,
    })
}

pub fn insert_session(conn: &Connection, row: &SessionRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO _amux_sessions (id, worker_id, backend, backend_ref, pid, started_at, \
         ended_at, exit_reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            row.id,
            row.worker_id,
            row.backend,
            row.backend_ref,
            row.pid,
            row.started_at,
            row.ended_at,
            row.exit_reason
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| json_col_err(7, e))?,
        ],
    )?;
    Ok(())
}

/// End a session exactly once: `ended_at` and `exit_reason` are set together
/// and only while the row is still live (the SQL twin of
/// `Session::end`'s AlreadyEnded guard). Returns rows affected — 0 means the
/// session was already ended (or never existed), and the ORIGINAL exit
/// reason survives, preserving the audit trail of why it really died.
pub fn end_session(
    conn: &Connection,
    session_id: &str,
    reason: &ExitReason,
    at: &str,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE _amux_sessions SET ended_at = ?2, exit_reason = ?3 \
         WHERE id = ?1 AND ended_at IS NULL",
        params![
            session_id,
            at,
            serde_json::to_string(reason).map_err(|e| json_col_err(7, e))?
        ],
    )
}

/// The worker's live session, if any (`ended_at IS NULL`). At most one
/// should exist by construction; ties break to the newest so an
/// inconsistency at least resolves to the session most likely to be real.
pub fn live_session_for(
    conn: &Connection,
    worker_id: &str,
) -> rusqlite::Result<Option<SessionRow>> {
    conn.query_row(
        "SELECT id, worker_id, backend, backend_ref, pid, started_at, ended_at, exit_reason \
         FROM _amux_sessions WHERE worker_id = ?1 AND ended_at IS NULL \
         ORDER BY started_at DESC, id DESC LIMIT 1",
        params![worker_id],
        session_from_row,
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrate;
    use amux_core::ids::SessionId;
    use amux_core::provider::ProviderId;
    use amux_core::session::BackendId;

    const T0: &str = "2026-08-09T00:00:00+00:00";
    const T1: &str = "2026-08-09T01:00:00+00:00";

    fn test_conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate::apply_all(&mut c).unwrap();
        c
    }

    fn cfg(name: &str) -> WorkerConfig {
        WorkerConfig {
            worker_type: Default::default(),
            display_name: name.into(),
            name_aliases: vec![],
            cwd: "/tmp/w".into(),
            provider: ProviderId::new("claude"),
            model: Some("fable-5".into()),
            backend: BackendId::herdr(),
            environment: BTreeMap::new(),
            permissions: vec!["bash".into()],
            group: None,
        }
    }

    fn insert_new(conn: &Connection, name: &str) -> WorkerRow {
        let row = WorkerRow::new(&WorkerId::from_ulid(ulid::Ulid::new()), &cfg(name), T0);
        insert_worker(conn, &row).unwrap();
        row
    }

    #[test]
    fn insert_then_get_by_id_and_name() {
        let conn = test_conn();
        let row = insert_new(&conn, "backend");
        let by_id = get_worker(&conn, &row.id).unwrap().unwrap();
        assert_eq!(by_id, row);
        let by_name = get_worker(&conn, "backend").unwrap().unwrap();
        assert_eq!(by_name.id, row.id);
        assert_eq!(by_name.version, 0);
        assert_eq!(by_name.state, WorkerState::Stopped);
        // Unknowns are None, not errors.
        assert!(get_worker(&conn, "nope").unwrap().is_none());
        assert!(get_worker(&conn, "wrk_01JGXV0000000000000000ZZZZ")
            .unwrap()
            .is_none());
    }

    #[test]
    fn rename_leaves_alias_that_still_resolves() {
        // Invariant 17 at the persistence layer: after a rename that appended
        // the old display_name to name_aliases, the old name still resolves
        // to the same immutable id.
        let conn = test_conn();
        let row = insert_new(&conn, "backend");
        let mut new_cfg = row.config();
        new_cfg.display_name = "rust-backend".into();
        new_cfg.name_aliases = vec!["backend".into()];
        let n = update_worker_config(&conn, &row.id, &new_cfg, 0, T1).unwrap();
        assert_eq!(n, 1);

        let by_alias = get_worker(&conn, "backend").unwrap().unwrap();
        assert_eq!(by_alias.id, row.id);
        assert_eq!(by_alias.display_name, "rust-backend");
        assert_eq!(by_alias.version, 1);
        assert_eq!(by_alias.updated_at, T1);
        let by_new_name = get_worker(&conn, "rust-backend").unwrap().unwrap();
        assert_eq!(by_new_name.id, row.id);
    }

    #[test]
    fn stale_version_write_reports_conflict_and_changes_nothing() {
        let conn = test_conn();
        let row = insert_new(&conn, "w");
        let mut new_cfg = row.config();
        new_cfg.cwd = "/somewhere/else".into();
        // Stale expectation: version is 0, caller believes 5.
        let n = update_worker_config(&conn, &row.id, &new_cfg, 5, T1).unwrap();
        assert_eq!(n, 0);
        let back = get_worker(&conn, &row.id).unwrap().unwrap();
        assert_eq!(back.cwd, "/tmp/w"); // untouched
        assert_eq!(back.version, 0);

        // Fresh expectation applies and bumps.
        let n = update_worker_config(&conn, &row.id, &new_cfg, 0, T1).unwrap();
        assert_eq!(n, 1);
        let back = get_worker(&conn, &row.id).unwrap().unwrap();
        assert_eq!(back.cwd, "/somewhere/else");
        assert_eq!(back.version, 1);
    }

    #[test]
    fn state_updates_round_trip_without_touching_version() {
        let conn = test_conn();
        let row = insert_new(&conn, "w");
        let starting = WorkerState::Starting;
        assert_eq!(
            update_worker_state(&conn, &row.id, &starting, T1).unwrap(),
            1
        );
        let back = get_worker(&conn, &row.id).unwrap().unwrap();
        assert_eq!(back.state, WorkerState::Starting);
        assert_eq!(back.version, 0); // state is telemetry, not a config edit

        let waiting = WorkerState::Waiting {
            reason: "gate review".into(),
        };
        assert_eq!(
            update_worker_state(&conn, &row.id, &waiting, T1).unwrap(),
            1
        );
        assert_eq!(get_worker(&conn, &row.id).unwrap().unwrap().state, waiting);
    }

    #[test]
    fn soft_delete_hides_from_reads_but_preserves_the_row() {
        let conn = test_conn();
        let row = insert_new(&conn, "doomed");
        assert_eq!(soft_delete_worker(&conn, &row.id, T1).unwrap(), 1);

        assert!(get_worker(&conn, &row.id).unwrap().is_none());
        assert!(get_worker(&conn, "doomed").unwrap().is_none());
        let (items, total) = list_workers(&conn, 0, 10).unwrap();
        assert!(items.is_empty());
        assert_eq!(total, 0);

        // Soft: the row itself survives (audit history keeps its FK anchor).
        let raw: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_workers", [], |r| r.get(0))
            .unwrap();
        assert_eq!(raw, 1);

        // Idempotent: a second delete affects nothing (Invariant 37 — it
        // must not be reportable as a fresh change).
        assert_eq!(soft_delete_worker(&conn, &row.id, T1).unwrap(), 0);
        // And a deleted worker cannot be mutated back to life.
        assert_eq!(
            update_worker_state(&conn, &row.id, &WorkerState::Starting, T1).unwrap(),
            0
        );
        assert_eq!(
            update_worker_config(&conn, &row.id, &cfg("doomed"), 0, T1).unwrap(),
            0
        );
    }

    #[test]
    fn list_workers_pages_and_counts() {
        let conn = test_conn();
        for i in 0..3 {
            insert_new(&conn, &format!("w{i}"));
        }
        let (page, total) = list_workers(&conn, 0, 2).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(total, 3);
        let (page, total) = list_workers(&conn, 2, 2).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(total, 3);
    }

    #[test]
    fn session_lifecycle_live_end_once() {
        let conn = test_conn();
        let w = insert_new(&conn, "w");
        let sid = SessionId::from_ulid(ulid::Ulid::new());
        let ses = SessionRow {
            id: sid.as_str().into(),
            worker_id: w.id.clone(),
            backend: "herdr".into(),
            backend_ref: format!("amux-{}", w.id),
            pid: Some(4242),
            started_at: T0.into(),
            ended_at: None,
            exit_reason: None,
        };
        insert_session(&conn, &ses).unwrap();

        let live = live_session_for(&conn, &w.id).unwrap().unwrap();
        assert_eq!(live, ses);

        assert_eq!(
            end_session(&conn, &ses.id, &ExitReason::Killed, T1).unwrap(),
            1
        );
        assert!(live_session_for(&conn, &w.id).unwrap().is_none());

        // End exactly once: a second end changes nothing, so the original
        // exit reason survives (the SQL twin of Session::end's guard).
        assert_eq!(
            end_session(&conn, &ses.id, &ExitReason::Crashed { signal: Some(9) }, T1).unwrap(),
            0
        );
        let reason: String = conn
            .query_row(
                "SELECT exit_reason FROM _amux_sessions WHERE id = ?1",
                params![ses.id],
                |r| r.get(0),
            )
            .unwrap();
        let back: ExitReason = serde_json::from_str(&reason).unwrap();
        assert_eq!(back, ExitReason::Killed);
    }

    #[test]
    fn worker_row_set_config_cannot_change_identity() {
        let conn = test_conn();
        let mut row = insert_new(&conn, "w");
        let id_before = row.id.clone();
        let mut c = cfg("renamed");
        c.cwd = "/elsewhere".into();
        row.set_config(&c);
        assert_eq!(row.id, id_before); // no path to an id change (Invariant 43)
        assert_eq!(row.display_name, "renamed");
    }
}
