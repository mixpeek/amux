//! Files the owner opens, for the Cmd-K palette (Ethan, 2026-10-30: "cmd k
//! should include files as well but needs to be intelligent about picking
//! which files ... ones ive visited frequently/recent", with
//! /Users/ethan/Vault/Mixpeek/To-Do.md opened many times and missing from it).
//!
//! The palette cannot index the filesystem on every keystroke, and most of it
//! is noise. What it needs is the files a person actually opens: one row per
//! path with how often and when last. A view of a file in the dashboard
//! (`GET /api/file` with no `X-Amux-Session`, which workers send) bumps it.
//! Five minutes after a boot, once, the table is seeded from the request log's
//! 14 days of such views, so the palette knows today's habits rather than
//! starting empty. The scan runs on a read connection; only the inserts take
//! the writer. A row with an empty path marks the seed as done.

use rusqlite::Connection;

pub(crate) const CREATE: &str = "CREATE TABLE IF NOT EXISTS file_visits (
    path TEXT PRIMARY KEY, visits INTEGER NOT NULL DEFAULT 0, last_at REAL NOT NULL)";

pub(crate) fn exists(conn: &Connection) -> bool {
    conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='file_visits'", [], |_| Ok(()))
        .is_ok()
}

/// One owner view of `path`.
pub(crate) fn record(conn: &Connection, path: &str, at: f64) -> rusqlite::Result<()> {
    conn.execute_batch(CREATE)?;
    conn.execute(
        "INSERT INTO file_visits (path, visits, last_at) VALUES (?1, 1, ?2)
         ON CONFLICT(path) DO UPDATE SET visits = visits + 1, last_at = MAX(last_at, excluded.last_at)",
        rusqlite::params![path, at],
    )?;
    Ok(())
}

pub(crate) fn seeded(conn: &Connection) -> bool {
    exists(conn) && conn.query_row("SELECT 1 FROM file_visits WHERE path = ''", [], |_| Ok(())).is_ok()
}

type Counts = std::collections::HashMap<String, (i64, f64)>;

/// The request log's owner views of `/api/file`, per path (the slow part,
/// on a read connection).
pub(crate) fn seed_counts(conn: &Connection) -> rusqlite::Result<Counts> {
    let mut counts = Counts::new();
    let has_log = conn
        .query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name='_amux_request_log'", [], |_| Ok(()))
        .is_ok();
    if !has_log {
        return Ok(counts);
    }
    {
        let mut st = conn.prepare(
            "SELECT ts, req_meta FROM _amux_request_log
             WHERE path = '/api/file' AND method = 'GET' AND COALESCE(amux_session, '') = '' AND req_meta LIKE '{\"query\":%'",
        )?;
        let rows = st.query_map([], |r| Ok((r.get::<_, f64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (ts, meta) = row?;
            let Some(path) = path_of(&meta) else { continue };
            let e = counts.entry(path).or_insert((0, 0.0));
            e.0 += 1;
            e.1 = e.1.max(ts);
        }
    }
    Ok(counts)
}

/// Add the counts and mark the seed done (the short part, on the writer).
pub(crate) fn seed_write(conn: &Connection, counts: &Counts) -> rusqlite::Result<usize> {
    conn.execute_batch(CREATE)?;
    if seeded(conn) {
        return Ok(0);
    }
    for (path, (n, last)) in counts {
        conn.execute(
            "INSERT INTO file_visits (path, visits, last_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET visits = visits + excluded.visits, last_at = MAX(last_at, excluded.last_at)",
            rusqlite::params![path, n, last],
        )?;
    }
    conn.execute("INSERT OR IGNORE INTO file_visits (path, visits, last_at) VALUES ('', 0, 0)", [])?;
    tracing::info!(target: "amux::file_visits", files = counts.len(), measured = true, n_considered = counts.len(),
        verdict = "file_visits_seeded", "file visits seeded from the request log's owner views");
    Ok(counts.len())
}

/// Seed once, five minutes after boot (outside the boot burst).
pub(crate) fn spawn_seed(store: std::sync::Arc<crate::db::Store>) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        let read = store.clone();
        let counts = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<Counts>> {
            let conn = read.read()?;
            if seeded(&conn) {
                return Ok(None);
            }
            Ok(Some(seed_counts(&conn)?))
        })
        .await;
        let counts = match counts {
            Ok(Ok(Some(c))) => c,
            Ok(Ok(None)) => return,
            Ok(Err(e)) => return tracing::warn!(target: "amux::file_visits", error = %e, measured = false, n_considered = 0, verdict = "file_visits_seed_failed", "file visits seed could not read the request log"),
            Err(e) => return tracing::warn!(target: "amux::file_visits", error = %e, measured = false, n_considered = 0, verdict = "file_visits_seed_failed", "file visits seed task failed"),
        };
        let res = store
            .write_async(move |c| {
                seed_write(c, &counts)?;
                Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
            })
            .await;
        if let Err(e) = res {
            tracing::warn!(target: "amux::file_visits", error = %e, measured = false, n_considered = 0, verdict = "file_visits_seed_failed", "file visits seed could not write");
        }
    });
}

/// The absolute `path` a logged `/api/file` query names.
fn path_of(req_meta: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(req_meta).ok()?;
    let qs = crate::api::fs::parse_qs(v.get("query")?.as_str()?);
    let p = crate::api::fs::qs_get(&qs, "path")?;
    p.starts_with('/').then(|| p.to_string())
}

/// Visited files that still exist: (path, visits, last_at).
pub(crate) fn list(conn: &Connection) -> rusqlite::Result<Vec<(String, i64, i64)>> {
    if !exists(conn) {
        return Ok(Vec::new());
    }
    let mut st = conn.prepare("SELECT path, visits, CAST(last_at AS INTEGER) FROM file_visits WHERE path <> '' ORDER BY last_at DESC LIMIT 2000")?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let mut out = Vec::new();
    for row in rows {
        let (p, n, at): (String, i64, i64) = row?;
        if std::path::Path::new(&p).is_file() {
            out.push((p, n, at));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_views_are_seeded_from_the_request_log_and_then_counted() {
        let dir = tempfile::tempdir().unwrap();
        let todo = dir.path().join("To-Do.md");
        std::fs::write(&todo, "x").unwrap();
        let todo = todo.to_string_lossy().to_string();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE _amux_request_log (id INTEGER PRIMARY KEY, ts REAL, method TEXT, path TEXT, amux_session TEXT, req_meta TEXT)",
        )
        .unwrap();
        let q = |p: &str| format!("{{\"query\":\"path={}\"}}", p.replace('/', "%2F"));
        for (ts, who) in [(10.0, ""), (20.0, ""), (30.0, "some-worker")] {
            conn.execute(
                "INSERT INTO _amux_request_log (ts, method, path, amux_session, req_meta) VALUES (?1, 'GET', '/api/file', ?2, ?3)",
                rusqlite::params![ts, who, q(&todo)],
            )
            .unwrap();
        }
        record(&conn, &todo, 5.0).unwrap(); // a view before the seed does not stop it
        assert!(!seeded(&conn));
        let counts = seed_counts(&conn).unwrap();
        assert_eq!(seed_write(&conn, &counts).unwrap(), 1);
        assert_eq!(seed_write(&conn, &counts).unwrap(), 0, "the seed runs once");
        let l = list(&conn).unwrap();
        assert_eq!(l, vec![(todo.clone(), 3, 20)], "one live view plus two logged owner views; the worker's is not the owner's habit");
        record(&conn, &todo, 40.0).unwrap();
        assert_eq!(list(&conn).unwrap(), vec![(todo.clone(), 4, 40)]);
        std::fs::remove_file(&todo).unwrap();
        assert!(list(&conn).unwrap().is_empty(), "a file that is gone is not offered");
    }
}
