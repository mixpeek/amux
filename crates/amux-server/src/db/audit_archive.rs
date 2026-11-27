//! Rows a migration drops are written out first (primitive integrity audit,
//! 2026-11-26). Before a dropping migration runs, `migrate.rs` calls the hook
//! here, which writes the table's rows to
//! `<db dir>/archive/primitive-audit-2026-11-26/<table>.json` (the same
//! directory and row encoding as the `_amux_messages` drop in `migrate.rs`).
//!
//! - A file already there is kept as it is: a re-run never overwrites the
//!   first copy.
//! - With rows to keep and no directory to keep them in (an in-memory
//!   database), or a write that fails, the migration is refused: the drop
//!   must never be the only copy's end.
//! - Every outcome logs a `verdict` with `measured` and `n_considered`.

use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

use super::migrate::{rows_json, AUDIT_ARCHIVE_DIR as ARCHIVE_DIR};

/// Write every row of `table` (ordered by `order_by`) to
/// `<home>/ARCHIVE_DIR/<table>.json` before `migration` drops it.
pub fn archive_table(
    conn: &Connection,
    home: Option<&Path>,
    migration: &str,
    table: &str,
    order_by: &str,
) -> anyhow::Result<()> {
    let exists = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1", [table], |_| Ok(()))
        .optional()?
        .is_some();
    let rows = if exists {
        rows_json(conn, &format!("SELECT * FROM \"{table}\" ORDER BY {order_by}"))?
    } else {
        Vec::new()
    };
    let file = format!("{table}.json");
    if rows.is_empty() {
        tracing::info!(migration, table, rows = 0, measured = true, n_considered = 0,
            table_exists = exists, verdict = "audit_archive_nothing_to_keep",
            "no rows to archive before the drop");
        return Ok(());
    }
    let Some(home) = home else {
        tracing::error!(migration, table, rows = rows.len(), measured = true, n_considered = rows.len(),
            verdict = "audit_archive_refused", "rows to keep and no directory to keep them in; migration refused");
        anyhow::bail!(
            "{migration} would drop {} row(s) of {table} but this database has no directory to archive them in",
            rows.len()
        );
    };
    let dir = home.join(ARCHIVE_DIR);
    let path = dir.join(&file);
    if path.exists() {
        tracing::info!(migration, path = %path.display(), rows = rows.len(), measured = true,
            n_considered = rows.len(), verdict = "audit_archive_kept_existing",
            "an archive is already there; left as it is");
        return Ok(());
    }
    let write = || -> anyhow::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".{file}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(&rows)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    };
    if let Err(e) = write() {
        tracing::error!(migration, path = %path.display(), rows = rows.len(), measured = true,
            n_considered = rows.len(), error = %e, verdict = "audit_archive_refused",
            "the archive could not be written; migration refused");
        return Err(e.context(format!("{migration}: archiving {table} to {}", path.display())));
    }
    tracing::warn!(migration, path = %path.display(), rows = rows.len(), measured = true,
        n_considered = rows.len(), verdict = "audit_archive_written",
        "rows archived before the table is dropped");
    Ok(())
}

/// The hook for the migration that drops `interaction_log`.
pub fn interaction_log(conn: &Connection, home: Option<&Path>) -> anyhow::Result<()> {
    archive_table(conn, home, "drop_interaction_log", "interaction_log", "id")
}
