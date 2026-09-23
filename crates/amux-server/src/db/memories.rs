//! Memory entry store over `_amux_memories` (RR-0071, Invariant 42).
//!
//! Rows are the canonical MemoryEntry table; this module is a thin
//! (de)serialization layer around `amux_core::memory` so every rule — the
//! version-bump discipline, soft-delete refusal, and above all the ONE
//! visibility predicate [`amux_core::memory::visible`] — lives in core and
//! is merely applied here, never re-implemented. Scope filtering is done in
//! Rust through that predicate rather than ported into SQL: a second SQL
//! spelling of `Scope::applies_to` is exactly the per-subsystem
//! re-implementation Invariant 2 exists to forbid, and the table is small
//! (config-sized, not log-sized), so the resolver's correctness is worth
//! more than an index scan.

use amux_core::memory::{visible_at, MemoryEntry, MemoryProvenance, MemoryType};
use amux_core::scope::{ResolutionTarget, Scope};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};

/// Map a serde failure on a column we wrote ourselves into the rusqlite
/// error channel so it surfaces as a 500 with the cause, not a panic.
fn corrupt(idx: usize, e: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
}

/// The bare serde token for a MemoryType ("user", "project", ...).
fn type_to_db(t: MemoryType) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "reference".into())
}

fn type_from_db(s: &str) -> Result<MemoryType, serde_json::Error> {
    serde_json::from_str(&format!("\"{s}\""))
}

const COLS: &str = "id, scope, name, content, memory_type, version, created_at, updated_at, \
                    deleted_at, provenance, expires_at, last_validated_at, superseded_by";

fn entry_from_row(r: &Row<'_>) -> rusqlite::Result<MemoryEntry> {
    let id: String = r.get(0)?;
    let scope: String = r.get(1)?;
    let memory_type: String = r.get(4)?;
    let created_at: String = r.get(6)?;
    let updated_at: String = r.get(7)?;
    let deleted_at: Option<String> = r.get(8)?;
    let provenance: String = r.get(9)?;
    let expires_at: Option<String> = r.get(10)?;
    let last_validated_at: Option<String> = r.get(11)?;
    Ok(MemoryEntry {
        id: amux_core::ids::MemoryId::parse(&id).map_err(|e| corrupt(0, e))?,
        scope: serde_json::from_str::<Scope>(&scope).map_err(|e| corrupt(1, e))?,
        name: r.get(2)?,
        content: r.get(3)?,
        memory_type: type_from_db(&memory_type).map_err(|e| corrupt(4, e))?,
        version: r.get::<_, i64>(5)? as u64,
        created_at: created_at
            .parse::<DateTime<Utc>>()
            .map_err(|e| corrupt(6, e))?,
        updated_at: updated_at
            .parse::<DateTime<Utc>>()
            .map_err(|e| corrupt(7, e))?,
        deleted_at: deleted_at
            .map(|s| s.parse::<DateTime<Utc>>())
            .transpose()
            .map_err(|e| corrupt(8, e))?,
        provenance: serde_json::from_str::<MemoryProvenance>(&provenance)
            .map_err(|e| corrupt(9, e))?,
        expires_at: expires_at
            .map(|s| s.parse::<DateTime<Utc>>())
            .transpose()
            .map_err(|e| corrupt(10, e))?,
        last_validated_at: last_validated_at
            .map(|s| s.parse::<DateTime<Utc>>())
            .transpose()
            .map_err(|e| corrupt(11, e))?,
        superseded_by: r.get(12)?,
    })
}

/// The canonical serde spelling of a Scope — also the stored form, so SQL
/// equality on the column IS scope equality (the partial unique index on
/// (scope, name) depends on this being one spelling per value).
pub fn scope_to_db(scope: &Scope) -> String {
    serde_json::to_string(scope).unwrap_or_else(|_| "{\"level\":\"global\"}".into())
}

/// Insert a fresh entry (id/version/timestamps already minted by the caller
/// via `MemoryEntry::new`).
pub fn insert(conn: &Connection, e: &MemoryEntry) -> rusqlite::Result<()> {
    conn.execute(
        &format!("INSERT INTO _amux_memories ({COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)"),
        params![
            e.id.as_str(),
            scope_to_db(&e.scope),
            e.name,
            e.content,
            type_to_db(e.memory_type),
            e.version as i64,
            e.created_at.to_rfc3339(),
            e.updated_at.to_rfc3339(),
            e.deleted_at.map(|t| t.to_rfc3339()),
            serde_json::to_string(&e.provenance).map_err(|er| corrupt(9, er))?,
            e.expires_at.map(|t| t.to_rfc3339()),
            e.last_validated_at.map(|t| t.to_rfc3339()),
            e.superseded_by.as_deref(),
        ],
    )?;
    Ok(())
}

/// One entry by id — soft-deleted rows INCLUDED (they are history, and a
/// direct fetch must be able to show `deleted_at` rather than pretend the
/// row never existed). Visibility filtering is [`list_visible`]'s job.
pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<MemoryEntry>> {
    conn.query_row(
        &format!("SELECT {COLS} FROM _amux_memories WHERE id = ?1"),
        params![id],
        entry_from_row,
    )
    .optional()
}

/// A live (non-deleted) entry by (scope, name) — the duplicate check backing
/// the partial unique index, run first so a duplicate is a clean 409 with
/// the existing id, not a constraint-violation string.
pub fn get_live_by_scope_name(
    conn: &Connection,
    scope: &Scope,
    name: &str,
) -> rusqlite::Result<Option<MemoryEntry>> {
    conn.query_row(
        &format!(
            "SELECT {COLS} FROM _amux_memories \
             WHERE scope = ?1 AND name = ?2 AND deleted_at IS NULL"
        ),
        params![scope_to_db(scope), name],
        entry_from_row,
    )
    .optional()
}

/// Every row, deleted included, ordered by id (ULID = creation order).
/// Callers needing visibility semantics go through [`list_visible`].
pub fn list_all(conn: &Connection) -> rusqlite::Result<Vec<MemoryEntry>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM _amux_memories ORDER BY id"))?;
    let rows = stmt.query_map([], entry_from_row)?;
    rows.collect()
}

/// The entries `target` may see: core's `visible` applied to the table —
/// excludes soft-deleted rows and everything outside the target's scope
/// chain (a worker-scoped memory is only returned when resolving for that
/// worker; Invariant 42's isolation rule, decided by Invariant 2's one
/// resolver).
pub fn list_visible(
    conn: &Connection,
    target: &ResolutionTarget,
) -> rusqlite::Result<Vec<MemoryEntry>> {
    let all = list_all(conn)?;
    Ok(visible_at(&all, target, Utc::now())
        .into_iter()
        .cloned()
        .collect())
}

/// Persist the mutated fields of an entry AFTER a core transition
/// (`MemoryEntry::update` / `soft_delete`) has been applied to it. Guarded
/// on the pre-mutation version so the row cannot be clobbered if it moved
/// underneath (belt-and-braces under the single-writer; the REAL optimistic
/// check when these queries compose elsewhere). Returns rows changed (0 =
/// version moved).
pub fn persist_mutation(
    conn: &Connection,
    e: &MemoryEntry,
    expected_version_before: u64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE _amux_memories
         SET content = ?1, version = ?2, updated_at = ?3, deleted_at = ?4,
             expires_at = ?5, last_validated_at = ?6, superseded_by = ?7
         WHERE id = ?8 AND version = ?9",
        params![
            e.content,
            e.version as i64,
            e.updated_at.to_rfc3339(),
            e.deleted_at.map(|t| t.to_rfc3339()),
            e.expires_at.map(|t| t.to_rfc3339()),
            e.last_validated_at.map(|t| t.to_rfc3339()),
            e.superseded_by.as_deref(),
            e.id.as_str(),
            expected_version_before as i64,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_core::ids::{GroupId, MemoryId, WorkerId};

    fn conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::db::migrate::apply_all(&mut c).unwrap();
        c
    }

    fn wid(n: u64) -> WorkerId {
        WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n as u128))
    }

    fn gid(n: u64) -> GroupId {
        GroupId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n as u128))
    }

    fn entry(n: u64, scope: Scope, name: &str) -> MemoryEntry {
        MemoryEntry::new(
            MemoryId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n as u128)),
            scope,
            name,
            "the content",
            MemoryType::Project,
            MemoryProvenance::HumanWritten,
            "2026-08-01T00:00:00Z".parse().unwrap(),
        )
    }

    #[test]
    fn insert_get_round_trips_every_field() {
        let c = conn();
        let e = MemoryEntry::new(
            MemoryId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, 1)),
            Scope::Worker { id: wid(9) },
            "api-shapes",
            "POST /api/board takes title+status",
            MemoryType::Reference,
            MemoryProvenance::WorkerWritten { worker: wid(9) },
            Utc::now(),
        );
        insert(&c, &e).unwrap();
        let back = get(&c, e.id.as_str()).unwrap().unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn scope_isolation_worker_a_cannot_see_worker_b() {
        let c = conn();
        insert(&c, &entry(1, Scope::Worker { id: wid(1) }, "a-private")).unwrap();
        insert(&c, &entry(2, Scope::Worker { id: wid(2) }, "b-private")).unwrap();
        insert(&c, &entry(3, Scope::Global, "shared")).unwrap();
        insert(&c, &entry(4, Scope::Group { id: gid(7) }, "team")).unwrap();

        let a = ResolutionTarget {
            worker: Some(wid(1)),
            group: Some(gid(7)),
        };
        let names: Vec<String> = list_visible(&c, &a)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert!(names.contains(&"a-private".to_string()));
        assert!(names.contains(&"shared".to_string()));
        assert!(names.contains(&"team".to_string()));
        assert!(!names.contains(&"b-private".to_string()), "{names:?}");

        // B, in no group: only its own + global.
        let b = ResolutionTarget {
            worker: Some(wid(2)),
            group: None,
        };
        let names: Vec<String> = list_visible(&c, &b)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(!names.contains(&"a-private".to_string()));
        assert!(!names.contains(&"team".to_string()));
    }

    #[test]
    fn update_bumps_version_and_persists_and_deleted_rows_leave_visible() {
        let c = conn();
        let mut e = entry(1, Scope::Global, "note");
        insert(&c, &e).unwrap();

        let before = e.version;
        assert!(e.update("new content", Utc::now()).unwrap());
        assert_eq!(persist_mutation(&c, &e, before).unwrap(), 1);
        let back = get(&c, e.id.as_str()).unwrap().unwrap();
        assert_eq!(back.version, 2);
        assert_eq!(back.content, "new content");

        // Soft delete: row stays, visibility drops, direct get still answers.
        let before = back.version;
        let mut d = back;
        d.soft_delete(Utc::now()).unwrap();
        assert_eq!(persist_mutation(&c, &d, before).unwrap(), 1);
        assert!(list_visible(&c, &ResolutionTarget::default())
            .unwrap()
            .is_empty());
        let raw = get(&c, d.id.as_str()).unwrap().unwrap();
        assert!(raw.is_deleted());
        assert_eq!(raw.version, 3);
    }

    #[test]
    fn stale_version_guard_changes_nothing() {
        let c = conn();
        let mut e = entry(1, Scope::Global, "note");
        insert(&c, &e).unwrap();
        e.update("edited", Utc::now()).unwrap();
        // Wrong expected version: guarded UPDATE must not apply.
        assert_eq!(persist_mutation(&c, &e, 41).unwrap(), 0);
        assert_eq!(
            get(&c, e.id.as_str()).unwrap().unwrap().content,
            "the content"
        );
    }

    #[test]
    fn live_scope_name_unique_but_deleted_frees_the_name() {
        let c = conn();
        let mut e = entry(1, Scope::Global, "runbook");
        insert(&c, &e).unwrap();
        assert!(get_live_by_scope_name(&c, &Scope::Global, "runbook")
            .unwrap()
            .is_some());
        // Same name at a DIFFERENT scope is fine (uniqueness is per-scope).
        insert(&c, &entry(2, Scope::Worker { id: wid(1) }, "runbook")).unwrap();
        // Same name, same scope: the partial unique index refuses.
        assert!(insert(&c, &entry(3, Scope::Global, "runbook")).is_err());
        // After soft delete the name is free again.
        let before = e.version;
        e.soft_delete(Utc::now()).unwrap();
        persist_mutation(&c, &e, before).unwrap();
        assert!(get_live_by_scope_name(&c, &Scope::Global, "runbook")
            .unwrap()
            .is_none());
        insert(&c, &entry(3, Scope::Global, "runbook")).unwrap();
    }
}
