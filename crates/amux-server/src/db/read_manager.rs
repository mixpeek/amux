//! The read pool's r2d2 connection manager.
//!
//! This replaces the `r2d2_sqlite` crate, which had no release for rusqlite
//! 0.41 when RUSTSEC-2026-0459 made 0.41 necessary: rusqlite 0.32 bundled
//! SQLite 3.46, which can return uninitialized page-cache memory for a
//! database opened beside a crafted `-wal` file. The read pool used only
//! `file(path).with_init(..)`, so this is that and nothing else.

use rusqlite::Connection;
use std::path::{Path, PathBuf};

type Init = Box<dyn Fn(&mut Connection) -> rusqlite::Result<()> + Send + Sync + 'static>;

pub struct SqliteConnectionManager {
    path: PathBuf,
    init: Init,
}

impl SqliteConnectionManager {
    /// Connections to the database file at `path`, opened with rusqlite's
    /// default flags (read-write, create, URI, no mutex), as `r2d2_sqlite` did.
    pub fn file(path: impl AsRef<Path>) -> Self {
        Self { path: path.as_ref().to_path_buf(), init: Box::new(|_| Ok(())) }
    }

    /// Run `init` on every new connection before the pool hands it out.
    pub fn with_init<F>(self, init: F) -> Self
    where
        F: Fn(&mut Connection) -> rusqlite::Result<()> + Send + Sync + 'static,
    {
        Self { init: Box::new(init), ..self }
    }
}

impl r2d2::ManageConnection for SqliteConnectionManager {
    type Connection = Connection;
    type Error = rusqlite::Error;

    fn connect(&self) -> Result<Connection, rusqlite::Error> {
        let mut c = Connection::open(&self.path)?;
        (self.init)(&mut c)?;
        Ok(c)
    }

    fn is_valid(&self, c: &mut Connection) -> Result<(), rusqlite::Error> {
        c.execute_batch("")
    }

    fn has_broken(&self, _: &mut Connection) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_runs_on_every_pooled_connection() {
        let dir = tempfile::tempdir().unwrap();
        let m = SqliteConnectionManager::file(dir.path().join("t.db"))
            .with_init(|c| c.pragma_update(None, "query_only", "ON"));
        let pool = r2d2::Pool::builder().max_size(2).build(m).unwrap();
        let (a, b) = (pool.get().unwrap(), pool.get().unwrap());
        for c in [&a, &b] {
            let on: i64 = c.query_row("PRAGMA query_only", [], |r| r.get(0)).unwrap();
            assert_eq!(on, 1, "the init closure must run on each connection");
            assert!(c.execute_batch("CREATE TABLE x(y)").is_err(), "query_only must refuse a write");
        }
    }

    #[test]
    fn bundled_sqlite_is_patched_for_rustsec_2026_0459() {
        // SQLite 3.54.0 fixed the uninitialized WAL page read. A downgrade
        // of rusqlite or libsqlite3-sys fails here, by name.
        assert!(rusqlite::version_number() >= 3_054_000, "bundled SQLite {} predates 3.54.0", rusqlite::version());
    }
}
