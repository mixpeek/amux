//! Migration runner (RR-0019).
//!
//! Migrations are numbered SQL files embedded at compile time from
//! `crates/amux-server/migrations/`. Applied in order inside one exclusive
//! transaction each, tracked in `_amux_migrations`. Two special forms:
//!
//! - Plain SQL statements: executed as a batch.
//! - `-- ADDCOL: <table> <column> <decl...>` directive lines: applied as
//!   `ALTER TABLE ... ADD COLUMN` ONLY when the column is absent. SQLite has
//!   no `ADD COLUMN IF NOT EXISTS`, and the baseline schema mirrors a live
//!   Python database whose tables may or may not already carry the column —
//!   the directive makes the migration idempotent against both shapes.
//!
//! The Python server must be able to keep opening the same DB file after
//! these run (Phase 11 rollback requirement), so migrations are ADDITIVE
//! ONLY: no drops, no renames, no type changes.

use rusqlite::Connection;

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

// Embedded at compile time so the binary is self-contained (single-artifact
// deploy is one of the four reasons this rewrite exists).
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "0001_baseline",
        sql: include_str!("../../migrations/0001_baseline.sql"),
    },
    Migration {
        version: 2,
        name: "0002_rust_additions",
        sql: include_str!("../../migrations/0002_rust_additions.sql"),
    },
    Migration {
        version: 3,
        name: "0003_workers",
        sql: include_str!("../../migrations/0003_workers.sql"),
    },
    Migration {
        version: 4,
        name: "0004_commands",
        sql: include_str!("../../migrations/0004_commands.sql"),
    },
    Migration {
        version: 5,
        name: "0005_memories_snapshots",
        sql: include_str!("../../migrations/0005_memories_snapshots.sql"),
    },
    Migration {
        version: 6,
        name: "0006_turns_messages",
        sql: include_str!("../../migrations/0006_turns_messages.sql"),
    },
    Migration {
        version: 7,
        name: "0007_criteria",
        sql: include_str!("../../migrations/0007_criteria.sql"),
    },
    Migration {
        version: 8,
        name: "0008_event_payload",
        sql: include_str!("../../migrations/0008_event_payload.sql"),
    },
    Migration {
        version: 9,
        name: "0009_media_jobs",
        sql: include_str!("../../migrations/0009_media_jobs.sql"),
    },
    Migration {
        version: 10,
        name: "0010_request_log",
        sql: include_str!("../../migrations/0010_request_log.sql"),
    },
    Migration {
        version: 11,
        name: "0011_conversations",
        sql: include_str!("../../migrations/0011_conversations.sql"),
    },
    Migration {
        version: 12,
        name: "0012_invariants",
        sql: include_str!("../../migrations/0012_invariants.sql"),
    },
    Migration {
        version: 13,
        name: "0013_search",
        sql: include_str!("../../migrations/0013_search.sql"),
    },
    Migration {
        version: 14,
        name: "0014_message_delivery_meta",
        sql: include_str!("../../migrations/0014_message_delivery_meta.sql"),
    },
    Migration {
        version: 15,
        name: "0015_schedule_run_delivery",
        sql: include_str!("../../migrations/0015_schedule_run_delivery.sql"),
    },
    Migration {
        version: 16,
        name: "0016_submit_verdict",
        sql: include_str!("../../migrations/0016_submit_verdict.sql"),
    },
    Migration {
        version: 17,
        name: "0017_status_gate_custom",
        sql: include_str!("../../migrations/0017_status_gate_custom.sql"),
    },
    Migration {
        version: 18,
        name: "0018_issue_epic",
        sql: include_str!("../../migrations/0018_issue_epic.sql"),
    },
    Migration {
        version: 19,
        name: "0019_reclaim",
        sql: include_str!("../../migrations/0019_reclaim.sql"),
    },
    Migration {
        version: 20,
        name: "0020_mdai_runs",
        sql: include_str!("../../migrations/0020_mdai_runs.sql"),
    },
    Migration {
        version: 21,
        name: "0021_heartbeat",
        sql: include_str!("../../migrations/0021_heartbeat.sql"),
    },
    Migration {
        version: 22,
        name: "0022_downtime_requests_during",
        sql: include_str!("../../migrations/0022_downtime_requests_during.sql"),
    },
    Migration {
        version: 23,
        name: "0023_downtime_backfill",
        sql: include_str!("../../migrations/0023_downtime_backfill.sql"),
    },
    Migration {
        version: 24,
        name: "0024_steering_dead_letter",
        sql: include_str!("../../migrations/0024_steering_dead_letter.sql"),
    },
    Migration {
        version: 25,
        name: "0025_mdai_run_duration",
        sql: include_str!("../../migrations/0025_mdai_run_duration.sql"),
    },
    Migration {
        version: 26,
        name: "0026_reclaim_skipped",
        sql: include_str!("../../migrations/0026_reclaim_skipped.sql"),
    },
    Migration {
        version: 27,
        name: "0027_guard_verdicts",
        sql: include_str!("../../migrations/0027_guard_verdicts.sql"),
    },
    Migration {
        version: 28,
        name: "0028_downtime_cause",
        sql: include_str!("../../migrations/0028_downtime_cause.sql"),
    },
    Migration {
        version: 29,
        name: "0029_steering_history_source",
        sql: include_str!("../../migrations/0029_steering_history_source.sql"),
    },
    Migration {
        version: 30,
        name: "0030_request_log_boot_at",
        sql: include_str!("../../migrations/0030_request_log_boot_at.sql"),
    },
    Migration {
        version: 31,
        name: "0031_issues_closed_at",
        sql: include_str!("../../migrations/0031_issues_closed_at.sql"),
    },
    Migration {
        version: 32,
        name: "0032_state_events_entity_index",
        sql: include_str!("../../migrations/0032_state_events_entity_index.sql"),
    },
    Migration {
        version: 33,
        name: "0033_steering_precondition",
        sql: include_str!("../../migrations/0033_steering_precondition.sql"),
    },
    Migration {
        version: 34,
        name: "0034_request_log_load1",
        sql: include_str!("../../migrations/0034_request_log_load1.sql"),
    },
];

/// Migrations embedded in THIS binary that the DB has not recorded yet.
///
/// Deliberately mirrors `apply_all`'s own "already applied?" test, including
/// the `unwrap_or(false)` — if `_amux_migrations` does not exist yet the query
/// fails and everything is pending, which is the truth for a fresh DB. A
/// separate predicate here would be a second spelling that drifts.
pub fn pending(conn: &Connection) -> Vec<&'static str> {
    MIGRATIONS
        .iter()
        .filter(|m| {
            !conn
                .query_row(
                    "SELECT 1 FROM _amux_migrations WHERE version = ?1",
                    [m.version],
                    |_| Ok(true),
                )
                .unwrap_or(false)
        })
        .map(|m| m.name)
        .collect()
}

/// True when this binary is a `cargo build` artifact run straight from a
/// source checkout, rather than an installed one.
///
/// # This matched the BANNED build dir and not the MANDATED one (AMUX-2799)
///
/// It used to test `s.contains("/target/debug/")`, and a previous version of
/// this doc called the resulting exclusion deliberate: `rust-build-target`
/// contains the substring `target` without a leading slash, so the shared build
/// dir never tripped it. That reasoning was sound for the auto-builder, which
/// compiles into `rust-build-target/release/` and then INSTALLS — it never runs
/// the artifact.
///
/// It was exactly backwards for the hazard this guard exists for. CLAUDE.md
/// tells every session to export `CARGO_TARGET_DIR=~/.amux/rust-build-target`,
/// so an ordinary `cargo run -p amux-server` produces
/// `~/.amux/rust-build-target/debug/amux-server` — which did NOT match, so the
/// guard returned Ok and a working-tree build was free to apply an uncommitted
/// migration to the live database. That is AMUX-2652 verbatim, the incident
/// this guard was written for. Measured:
///
/// ```text
///   false   ~/.amux/rust-build-target/debug/amux-server   <- MANDATED, unguarded
///   true    ~/Dev/amux/target/debug/amux-server           <- BANNED, guarded
///   false   ~/.local/bin/amux-server-rs                   <- installed, correct
/// ```
///
/// Protection was on for the convention nobody is allowed to use and off for
/// the one everybody must. Nothing reported it, because the test had been
/// changed to inject a cargo-shaped fixture rather than read `current_exe()` —
/// which fixed a red test by removing the only assertion that touched the real
/// environmental input (ethos rule 7: a check that cannot fail).
///
/// # The rule now
///
/// A cargo artifact always sits under a `debug` or `release` PROFILE DIRECTORY,
/// whatever `CARGO_TARGET_DIR` is set to; an installed server never does. Both
/// real deployments are `bin/` paths with no such component —
/// `~/.local/bin/amux-server-rs` locally and `/usr/local/bin/amux-server-rs` in
/// the cloud image — so matching the component is precise in both directions
/// and, unlike a substring of one hardcoded layout, does not need revisiting
/// the next time the fleet moves its build directory.
fn is_cargo_target_build(exe: &std::path::Path) -> bool {
    exe.components().any(|c| {
        matches!(c.as_os_str().to_str(), Some("debug") | Some("release"))
    })
}

/// True when `db_path` is the fleet's real database, `$HOME/.amux/amux.db`.
///
/// Compared after `canonicalize` where possible so `AMUX_HOME=~/.amux` (an
/// explicit spelling of the same file) is still recognised as live; a plain
/// string compare would miss it and the guard would not fire.
fn is_live_db(db_path: &std::path::Path, home: Option<&std::path::Path>) -> bool {
    let Some(home) = home else { return false };
    let live = home.join(".amux").join("amux.db");
    let norm = |p: &std::path::Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    norm(db_path) == norm(&live)
}

/// AMUX-2652: a peer's ordinary `cargo run` must not migrate the LIVE database.
///
/// Migrations are `include_str!`-embedded at compile time, so a working-tree
/// build carries whatever migration files that tree has — including a
/// half-written one nobody has committed. The default DB path is the live file,
/// so `cargo run -p amux-server` in this checkout applied 0013_search.sql to
/// ~/.amux/amux.db at 22:18:23, 101 seconds after its author created it and
/// before they had made their read-only copy. It applied cleanly that time.
///
/// The point is not that it broke something; it is that no agent can honour
/// "never touch the live DB" while a PEER's build does it on their behalf. The
/// person who runs the command and the person whose migration runs are
/// different people, so care cannot prevent it — only a check can.
///
/// Scoped as narrowly as the hazard: it fires only for an UNAPPLIED migration,
/// from a cargo target build, against the live file. Anything else — the
/// installed server, a temp `AMUX_HOME`, a container (whose binary is not a
/// target artifact), a dev run with pending migrations against a scratch DB —
/// is untouched. Refusing too eagerly would be worse than the hazard: a server
/// that will not start is a live outage, and this must never be that.
fn guard_live_db(db_path: &std::path::Path, pending: &[&str]) -> anyhow::Result<()> {
    let Ok(exe) = std::env::current_exe() else {
        return Ok(()); // cannot tell what we are: do not block a real server
    };
    guard_live_db_with_exe(db_path, pending, &exe)
}

/// The guard with its one environmental input injected.
///
/// Split out so the positive test can supply a cargo-target-shaped path
/// instead of asserting something about where IT happens to have been built —
/// a test whose precondition depends on the build location cannot survive a
/// change in build location.
///
/// NB an earlier version of this paragraph described `is_cargo_target_build`
/// as "deliberately not matching ~/.amux/rust-build-target/" — PAST behaviour,
/// stated in the present tense. That non-match was the AMUX-2799 hole (guard
/// off for the mandated build dir, on for the banned one) and it is FIXED: the
/// matcher now keys on the debug/release profile component, which the shared
/// dir does contain. See is_cargo_target_build's own doc for the full arc. The
/// paragraph is corrected because a comment describing a closed hole as
/// current design is how the hole gets faithfully re-implemented.
fn guard_live_db_with_exe(
    db_path: &std::path::Path,
    pending: &[&str],
    exe: &std::path::Path,
) -> anyhow::Result<()> {
    if pending.is_empty() || truthy_env("AMUX_ALLOW_LIVE_DB") {
        return Ok(());
    }
    if !is_cargo_target_build(exe) {
        return Ok(());
    }
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    if !is_live_db(db_path, home.as_deref()) {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to apply {n} unapplied migration(s) to the LIVE database.\n\
         \n\
         Migration(s): {names}\n\
         Database    : {db}\n\
         Binary      : {exe} (a cargo target build, not the installed server)\n\
         \n\
         Migrations are embedded at COMPILE time, so this build carries whatever\n\
         is in its working tree — possibly a peer's uncommitted, half-written\n\
         migration. Applying it here would change the schema of the database the\n\
         whole fleet is using, on their behalf and without their knowledge.\n\
         \n\
         Point it somewhere else instead:\n\
           AMUX_HOME=$(mktemp -d) cargo run -p amux-server\n\
           AMUX_DB=/tmp/scratch.db cargo run -p amux-server\n\
         \n\
         Or, if you genuinely mean to migrate the live DB from a working-tree\n\
         build, say so explicitly: AMUX_ALLOW_LIVE_DB=1",
        n = pending.len(),
        names = pending.join(", "),
        db = db_path.display(),
        exe = exe.display(),
    )
}

fn truthy_env(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "no"
        })
        .unwrap_or(false)
}

/// Guard + apply. `Store::open` calls this; the bare [`apply_all`] stays for
/// tests, which run against temp and in-memory databases.
pub fn apply_all_guarded(conn: &mut Connection, db_path: &std::path::Path) -> anyhow::Result<()> {
    guard_live_db(db_path, &pending(conn))?;
    apply_all(conn)
}

pub fn apply_all(conn: &mut Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _amux_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at TEXT NOT NULL,
            -- Fresh databases get this from the start; existing ones get it
            -- from 0032's ADDCOL. Both paths, because CREATE TABLE IF NOT
            -- EXISTS is a no-op against a table that already exists and would
            -- silently leave every deployed database without the column.
            duration_ms INTEGER
        );",
    )?;
    for m in MIGRATIONS {
        let already: bool = conn
            .query_row(
                "SELECT 1 FROM _amux_migrations WHERE version = ?1",
                [m.version],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if already {
            continue;
        }
        // TIME EVERY MIGRATION. This function used to log nothing, so 0031
        // holding the connection for 186 seconds on 2026-08-24 was
        // indistinguishable from a crash, a slow build, or a launchd problem:
        // the only symptom anyone could see was /health not answering. A
        // migration is the one startup step that can take arbitrarily long
        // while looking exactly like a dead process.
        let started = std::time::Instant::now();
        let tx = conn.transaction()?;
        apply_one(&tx, m.sql)?;
        let ms = started.elapsed().as_millis() as i64;
        tx.execute(
            "INSERT INTO _amux_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![m.version, m.name, chrono::Utc::now().to_rfc3339()],
        )?;
        // Best-effort: the column arrives in 0032, so every migration before it
        // on an existing database has nowhere to put this. Failing to RECORD a
        // duration must never fail the migration itself, and the tracing line
        // below carries the number regardless.
        let _ = tx.execute(
            "UPDATE _amux_migrations SET duration_ms = ?1 WHERE version = ?2",
            rusqlite::params![ms, m.version],
        );
        tx.commit()?;
        // WARN, not INFO, above a threshold a human would notice as downtime.
        // 2s is well above every migration here except the one that caused the
        // outage, so this fires on the shape that matters and stays quiet
        // otherwise.
        if ms >= 2_000 {
            tracing::warn!(
                migration = m.name, duration_ms = ms,
                "migration held the database for {:.1}s — the server is unreachable for this whole period",
                ms as f64 / 1000.0
            );
        } else {
            tracing::info!(migration = m.name, duration_ms = ms, "migration applied");
        }
    }
    Ok(())
}

/// Apply ONE migration's body, honouring `-- ADDCOL:` directives.
///
/// `pub(crate)` so tests can build a schema through the SHIPPED path. A fixture
/// that `execute_batch`es a migration file silently SKIPS every ADDCOL line —
/// they are SQL comments — so it would prove a column exists that production
/// has and the test does not, or vice versa (AF-99).
pub(crate) fn apply_one(conn: &Connection, sql: &str) -> anyhow::Result<()> {
    // Split ADDCOL directives from plain SQL. Directives are full-line
    // comments so the file stays valid SQL for external tools.
    let mut plain = String::new();
    let mut addcols: Vec<(String, String, String)> = Vec::new();
    for line in sql.lines() {
        if let Some(rest) = line.trim().strip_prefix("-- ADDCOL:") {
            let mut parts = rest.trim().splitn(3, ' ');
            let (Some(table), Some(column), Some(decl)) =
                (parts.next(), parts.next(), parts.next())
            else {
                anyhow::bail!("malformed ADDCOL directive: {line:?}");
            };
            addcols.push((table.to_string(), column.to_string(), decl.to_string()));
        } else {
            plain.push_str(line);
            plain.push('\n');
        }
    }
    // ADDCOL BEFORE the plain SQL (AMUX-3609). "Add a column, then populate it"
    // is the natural shape for a backfill migration, and with the old order it
    // was impossible: the UPDATE ran first and died on `no such column`. Nobody
    // had hit it because every ADDCOL migration so far only added.
    //
    // Safe for the existing 31 because ADDCOL targets tables that already exist
    // — that is what the directive is FOR (a live Python DB whose tables may or
    // may not already carry the column). Audited before reordering: no
    // migration both CREATEs and ADDCOLs the same table, which is the only
    // arrangement this order would break. `addcol_can_be_used_by_the_same_
    // migration` pins the new guarantee.
    for (table, column, decl) in addcols {
        if !column_exists(conn, &table, &column)? {
            // Identifiers cannot be bound as parameters; they come from our
            // own embedded migration files, not user input.
            conn.execute_batch(&format!(
                "ALTER TABLE \"{table}\" ADD COLUMN \"{column}\" {decl};"
            ))?;
        }
    }
    if !plain.trim().is_empty() {
        conn.execute_batch(&plain)?;
    }
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod registration_guard {
    use super::MIGRATIONS;

    /// A `.sql` file nobody registered is a migration that NEVER RUNS, and
    /// nothing anywhere says so (AF-99).
    ///
    /// MIGRATIONS is a hand-maintained array, so adding a file and forgetting the
    /// entry is silent in every direction: the server boots clean, the schema is
    /// simply older than the code, and the first symptom is a query failing at
    /// runtime somewhere far away. That is exactly what happened — 0022/0023 sat
    /// on disk unregistered while `/api/debug/downtime` returned an empty list,
    /// which reads identically to "there were never any outages".
    #[test]
    fn every_migration_file_on_disk_is_registered() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
            .expect("migrations dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sql"))
            .map(|n| n.trim_end_matches(".sql").to_owned())
            .collect();
        on_disk.sort();

        // The control. If read_dir ever returned nothing — wrong path, renamed
        // directory — an equality assert could only fail loudly, but a future
        // rewrite into a subset check would pass vacuously. Pin it here.
        assert!(
            on_disk.len() >= 23,
            "found {} migration files; the scan is looking in the wrong place",
            on_disk.len()
        );

        let mut registered: Vec<String> =
            MIGRATIONS.iter().map(|m| m.name.to_owned()).collect();
        registered.sort();

        let unregistered: Vec<&String> =
            on_disk.iter().filter(|n| !registered.contains(n)).collect();
        let missing_file: Vec<&String> =
            registered.iter().filter(|n| !on_disk.contains(n)).collect();

        // Name the offending FILE rather than printing two sorted lists. On this
        // shared checkout the usual cause is a peer mid-work who wrote the .sql
        // before editing the array, and a diff of 24 names does not say that.
        assert!(
            unregistered.is_empty(),
            "migration file(s) on disk but NOT in the MIGRATIONS array: {unregistered:?}. \
             An unregistered migration never runs: the server boots clean, the schema \
             silently lags the code, and the first symptom is a query failing somewhere \
             far away. Add a Migration{{version, name, sql: include_str!(..)}} entry. \
             (If this is not your file, a peer is mid-work — tell them rather than \
             registering it for them.)"
        );
        assert!(
            missing_file.is_empty(),
            "MIGRATIONS references file(s) that do not exist: {missing_file:?}"
        );
    }

    /// Versions must be dense and ascending, or `apply_all`'s ordering and the
    /// `_amux_migrations` bookkeeping stop lining up with the filenames.
    #[test]
    fn versions_are_dense_and_match_their_filenames() {
        for (i, m) in MIGRATIONS.iter().enumerate() {
            let expected = i as i64 + 1;
            assert_eq!(m.version, expected, "{} is out of order", m.name);
            let prefix = format!("{:04}_", m.version);
            assert!(
                m.name.starts_with(&prefix),
                "{} does not start with its own version prefix {prefix}",
                m.name
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_to_fresh_db_and_are_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_all(&mut conn).unwrap();
        // Applying again is a no-op, not an error.
        apply_all(&mut conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n as usize, super::MIGRATIONS.len());
        // The revision row exists and starts at 0.
        let rev: u64 = conn
            .query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rev, 0);
    }

    /// AMUX-3609's backfill, driven through the SHIPPED migration body rather
    /// than a paraphrase of its SQL.
    ///
    /// The backfill can only recover what the reaper left: `_amux_state_events`
    /// is swept at 14 days, so ~10,700 of the board's 10,905 rows will end up
    /// NULL forever. That is the point of the negative cases below — a backfill
    /// that guessed a date for them would be worse than the NULL, and a test
    /// with only the positive case would pass against one that did.
    #[test]
    fn closed_at_backfill_recovers_only_what_the_journal_still_holds() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_all(&mut conn).unwrap();

        conn.execute_batch(
            "INSERT INTO issues (id,title,desc,status,creator,created,updated) VALUES
               ('C-CLOSED','a','', 'done',     'x',1,1),
               ('C-REOPEN','b','', 'doing',    'x',1,1),
               ('C-REAPED','c','', 'verified', 'x',1,1),
               ('C-TWICE' ,'d','', 'done',     'x',1,1);
             INSERT INTO _amux_state_events (rev,entity_type,entity_id,mutation,at,payload) VALUES
               (1,'task','C-CLOSED','{\"kind\":\"status_changed\",\"from\":\"doing\",\"to\":\"done\"}','2026-08-20T10:00:00.123456+00:00',NULL),
               -- reopened: its journal says it closed, but it is OPEN now, so
               -- the backfill must skip it or `closed_at IS NOT NULL` would
               -- start meaning \"was closed once\".
               (2,'task','C-REOPEN','{\"kind\":\"status_changed\",\"from\":\"doing\",\"to\":\"done\"}','2026-08-20T11:00:00+00:00',NULL),
               -- a NON-terminal transition must not be mistaken for a close.
               (3,'task','C-TWICE','{\"kind\":\"status_changed\",\"from\":\"todo\",\"to\":\"doing\"}','2026-08-19T09:00:00+00:00',NULL),
               (4,'task','C-TWICE','{\"kind\":\"status_changed\",\"from\":\"doing\",\"to\":\"done\"}','2026-08-20T09:00:00+00:00',NULL),
               (5,'task','C-TWICE','{\"kind\":\"status_changed\",\"from\":\"done\",\"to\":\"doing\"}','2026-08-21T09:00:00+00:00',NULL),
               (6,'task','C-TWICE','{\"kind\":\"status_changed\",\"from\":\"doing\",\"to\":\"done\"}','2026-08-22T09:00:00+00:00',NULL);",
        )
        .unwrap();

        // Re-run the shipped body. ADDCOL is idempotent; the UPDATE is the part
        // under test. Reading it from the same include_str the registry uses
        // means this cannot drift from what production applies.
        apply_one(&conn, super::MIGRATIONS.iter().find(|m| m.version == 31).unwrap().sql).unwrap();

        let at = |id: &str| -> Option<i64> {
            conn.query_row("SELECT closed_at FROM issues WHERE id = ?1", [id], |r| r.get(0))
                .unwrap()
        };

        // 1787220000 = 2026-08-20T10:00:00Z. The microseconds and the +00:00
        // offset are parsed by strftime, which is the assumption the migration
        // rests on — asserted here rather than trusted.
        assert_eq!(at("C-CLOSED"), Some(1787220000), "a closed card recovers its real close time");
        assert_eq!(at("C-REOPEN"), None, "a card that is open NOW must not be backfilled from an old close");
        assert_eq!(at("C-REAPED"), None, "no journal row survives for it: NULL means NOT RECORDED, never a guess");
        assert_eq!(
            at("C-TWICE"),
            Some(1787389200),
            "2026-08-22T09:00:00Z — the LATEST close, matching the live write rule; MIN would make backfilled rows disagree with every row written after this migration"
        );

        // Idempotent: the migration is guarded by `closed_at IS NULL`, so a
        // second run must not disturb what the first wrote.
        apply_one(&conn, super::MIGRATIONS.iter().find(|m| m.version == 31).unwrap().sql).unwrap();
        assert_eq!(at("C-CLOSED"), Some(1787220000));
        assert_eq!(at("C-TWICE"), Some(1787389200));
    }

    /// A migration must be able to ADD a column and then USE it in the same
    /// file. Before AMUX-3609 the runner executed plain SQL first and the
    /// ADDCOLs after, so a backfill died on `no such column` — with nothing in
    /// the mechanism hinting at the ordering. Nobody hit it because every
    /// ADDCOL migration until then only added.
    /// The timing instrument itself must be able to fail. `apply_all` logged
    /// nothing until 0031 held the database for 186 seconds and the only visible
    /// symptom was /health not answering; an instrument added in response to
    /// that, and never checked, would be the same defect with more code.
    #[test]
    fn every_migration_records_how_long_it_held_the_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_all(&mut conn).unwrap();
        let missing: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_migrations WHERE duration_ms IS NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            missing, 0,
            "on a fresh database every migration must record its duration, or the next \
             outage is a forensic exercise again"
        );
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM _amux_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n as usize, super::MIGRATIONS.len(), "and every migration must be recorded at all");
    }

    #[test]
    fn addcol_can_be_used_by_the_same_migration() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, n INTEGER);").unwrap();
        conn.execute("INSERT INTO t VALUES ('a', 7)", []).unwrap();
        apply_one(
            &conn,
            "-- ADDCOL: t doubled INTEGER\nUPDATE t SET doubled = n * 2;",
        )
        .expect("a migration that adds a column and populates it must apply");
        let got: i64 = conn.query_row("SELECT doubled FROM t WHERE id='a'", [], |r| r.get(0)).unwrap();
        assert_eq!(got, 14, "the plain SQL must run AFTER the column exists");
    }

    #[test]
    fn addcol_directive_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (a INTEGER);").unwrap();
        let sql = "-- ADDCOL: t b INTEGER NOT NULL DEFAULT 0\n";
        apply_one(&conn, sql).unwrap();
        apply_one(&conn, sql).unwrap(); // second run: column exists, skipped
        assert!(column_exists(&conn, "t", "b").unwrap());
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// SUPERSEDES `the_auto_build_target_dir_is_not_mistaken_for_a_cargo_run`,
    /// which asserted the exact opposite and was WRONG (AMUX-2799).
    ///
    /// That test required `~/.amux/rust-build-target/release/` NOT to match, on
    /// the stated grounds that "it INSTALLS, and blocking it would stop the real
    /// server booting". The premise is false: `scripts/rust-auto-build.sh` runs
    /// `install -m 0755 .../release/amux-server ~/.local/bin/amux-server-rs`, so
    /// the artifact under the build dir is never the thing that boots. Nothing
    /// executes it in place.
    ///
    /// What the assertion actually bought was a guard that was OFF for the build
    /// dir CLAUDE.md mandates and ON for the per-session dirs it bans — so an
    /// ordinary `cargo run -p amux-server` under the sanctioned environment
    /// could apply an uncommitted migration to the live DB, which is AMUX-2652
    /// verbatim. A test can pin a defect as firmly as it pins a fix.
    #[test]
    fn any_profile_dir_reads_as_a_cargo_build_whatever_the_target_dir_is() {
        // A per-session checkout dir (banned, but must still be caught).
        assert!(is_cargo_target_build(Path::new("/Users/x/Dev/amux/target/debug/amux-server")));
        assert!(is_cargo_target_build(Path::new("/Users/x/Dev/amux/target/release/amux-server")));
        // THE MANDATED SHARED DIR — the case that was unguarded. A `cargo run`
        // here is exactly the working-tree-against-live-DB hazard.
        assert!(
            is_cargo_target_build(Path::new("/Users/x/.amux/rust-build-target/debug/amux-server")),
            "the build dir every session is told to use must be guarded, not exempt"
        );
        assert!(is_cargo_target_build(Path::new(
            "/Users/x/.amux/rust-build-target/release/amux-server"
        )));
        // Any OTHER target dir, because the next move must not need a code change.
        assert!(is_cargo_target_build(Path::new("/tmp/whatever-42/debug/deps/amux_server-abc")));

        // The two paths that actually boot a server, both of which must stay
        // exempt or the fleet cannot start with a pending migration.
        assert!(!is_cargo_target_build(Path::new("/Users/x/.local/bin/amux-server-rs")));
        assert!(
            !is_cargo_target_build(Path::new("/usr/local/bin/amux-server-rs")),
            "the cloud image's path — CMD [\"amux-server-rs\"] off /usr/local/bin"
        );
    }

    /// THE ASSERTION WHOSE REMOVAL HID THIS. The predicate's only real input is
    /// `current_exe()`, and the previous fix for a red test here was to stop
    /// consulting it and inject a hand-built path instead — which made the test
    /// pass under every target dir by testing nothing about any of them.
    ///
    /// This is stable, not fragile: a test binary IS a cargo artifact by
    /// construction, under any `CARGO_TARGET_DIR`, so the assertion holds
    /// wherever it is built — while still failing the moment the predicate stops
    /// recognising the real environment.
    #[test]
    fn this_very_test_binary_is_recognised_as_a_cargo_build() {
        let exe = std::env::current_exe().expect("current_exe must be readable");
        assert!(
            is_cargo_target_build(&exe),
            "a test binary must read as a cargo build, or the guard is off for whoever \
             is running the suite from this target dir: {}",
            exe.display()
        );
    }

    #[test]
    fn only_the_real_home_db_counts_as_live() {
        let home = PathBuf::from("/Users/x");
        assert!(is_live_db(Path::new("/Users/x/.amux/amux.db"), Some(&home)));
        assert!(!is_live_db(Path::new("/tmp/scratch/amux.db"), Some(&home)));
        assert!(!is_live_db(Path::new("/Users/x/.amux/other.db"), Some(&home)));
        // No HOME at all: cannot prove it is live, so do not block.
        assert!(!is_live_db(Path::new("/Users/x/.amux/amux.db"), None));
    }

    // The guard must be silent in every legitimate case, or it becomes an
    // outage. A refusal that fires on the installed server is strictly worse
    // than the hazard it prevents.
    #[test]
    fn nothing_pending_never_refuses_even_on_the_live_db() {
        assert!(guard_live_db(Path::new("/Users/x/.amux/amux.db"), &[]).is_ok());
    }

    #[test]
    fn a_scratch_db_is_never_refused_however_it_was_built() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("amux.db");
        assert!(guard_live_db(&db, &["0099_experiment"]).is_ok());
    }

    #[test]
    fn truthy_env_treats_the_usual_falsey_spellings_as_off() {
        for (v, want) in [
            ("1", true),
            ("true", true),
            ("YES", true),
            ("0", false),
            ("false", false),
            ("no", false),
            ("", false),
            ("  ", false),
        ] {
            std::env::set_var("AMUX_TEST_TRUTHY", v);
            assert_eq!(truthy_env("AMUX_TEST_TRUTHY"), want, "value {v:?}");
        }
        std::env::remove_var("AMUX_TEST_TRUTHY");
        assert!(!truthy_env("AMUX_TEST_TRUTHY"));
    }

    // THE POSITIVE CASE. Everything above proves the guard stays QUIET; a guard
    // that only ever returns Ok would pass all of them. This is the one that
    // proves it can fire.
    //
    // Safe to run against the real live path because guard_live_db only
    // INSPECTS paths — it never opens, reads, or migrates the database. And the
    // test binary is itself a cargo target artifact (target/debug/deps/...),
    // which is exactly the build shape the hazard describes, so this exercises
    // the real predicate rather than a stubbed one.
    #[test]
    fn it_actually_refuses_a_pending_migration_against_the_live_db() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            panic!("HOME unset — this test cannot express its precondition, \
                    which is a broken test, not a passing one");
        };
        // A cargo-target-shaped path, supplied rather than inherited: the real
        // hazard is a binary built from a working tree, and that shape is a
        // property of the PATH, not of wherever this test binary was written.
        // (Asserting on `current_exe()` made the test fail under
        // CARGO_TARGET_DIR=~/.amux/rust-build-target — the shared build dir the
        // workflow now standardises on — while the guard itself was correct.)
        let exe = PathBuf::from("/Users/someone/Dev/amux/target/debug/deps/amux_server-abc123");
        assert!(is_cargo_target_build(&exe), "the fixture must be the shape the guard looks for");

        let live = home.join(".amux").join("amux.db");
        std::env::remove_var("AMUX_ALLOW_LIVE_DB");
        let err = guard_live_db_with_exe(&live, &["0099_pretend_uncommitted"], &exe)
            .expect_err("the guard MUST refuse a pending migration against the live DB");
        let msg = err.to_string();
        assert!(msg.contains("LIVE database"), "{msg}");
        assert!(msg.contains("0099_pretend_uncommitted"), "{msg}");
        // The refusal has to publish its own escape, or the next agent
        // hand-rolls something worse to get past it (the AMUX-2325 lesson).
        assert!(msg.contains("AMUX_ALLOW_LIVE_DB=1"), "{msg}");
        assert!(msg.contains("AMUX_HOME"), "{msg}");

        // The sanctioned escape must actually work, or the message above is a
        // lie and the guard is unwalkable (the AMUX-2325 shape: a constraint
        // whose documented exit does not exist gets routed around by something
        // worse). Same test, so nothing can race on this env var.
        // Injected exe here too. With `current_exe()` this assertion passed
        // VACUOUSLY under a target dir outside the checkout: the guard returns
        // Ok at the is_cargo_target_build check, before it ever reads the env
        // var, so it would have gone green with the escape hatch deleted.
        std::env::set_var("AMUX_ALLOW_LIVE_DB", "1");
        let permitted = guard_live_db_with_exe(&live, &["0099_pretend_uncommitted"], &exe);
        std::env::remove_var("AMUX_ALLOW_LIVE_DB");
        assert!(
            permitted.is_ok(),
            "AMUX_ALLOW_LIVE_DB=1 must permit it: {permitted:?}"
        );
    }

    // NOTE: the refuse and permit cases are ONE test on purpose. As two, they
    // raced on AMUX_ALLOW_LIVE_DB — process-global env, threaded harness — and
    // the escape case failed roughly at random. That is precisely the flake
    // class AMUX-2675 was about; merging removes it by construction instead of
    // adding another lock that the next test must remember to take.
}

/// AF-193 / AMUX-3609: a migration's COST, which the rest of the suite cannot express.
///
/// 0031 backfilled `issues.closed_at` with a correlated subquery over
/// `_amux_state_events`, which carried exactly one index, on `rev`. It
/// full-scanned ~79,000 rows for each of 7,281 terminal cards, inside the
/// exclusive transaction a migration runs in, at server startup, and the server
/// was unreachable for 186 seconds. Every test was green throughout, because
/// migration tests apply their SQL to four fixture rows — and on four rows an
/// index scan and a table scan are indistinguishable. Correctness and cost are
/// different questions and the suite only ever answered the first.
///
/// WHY THE PLAN AND NOT A ROW COUNT. The obvious fix is a fixture with realistic
/// row counts, and it is the wrong one: it needs a threshold, the threshold has
/// to be guessed, and a migration that is 10x too slow on 79,000 rows still
/// passes on whatever number gets picked. Ethos rule 7 — prefer the
/// structurally-absent signal over the tuned parameter. `EXPLAIN QUERY PLAN`
/// answers the actual question at any table size, in microseconds, on an empty
/// database.
///
/// THE RULE: inside a CORRELATED subquery, every table access must use an index.
/// A correlated subquery runs once per outer row, so an unindexed access there is
/// O(outer x inner) by construction — that is the shape that took the server
/// down, and it is a property of the SQL, not of how much data happens to exist.
/// A `SCAN` at the top level is left alone: one pass over the table being
/// updated is unavoidable and is O(n) once.
///
/// MEASURED, not assumed, because the obvious matcher does not work. The plan
/// prints the ALIAS, not the table name, so a check that greps for
/// `_amux_state_events` finds nothing and passes on the incident itself:
///
///   without the index:  `--SEARCH e
///   with the index:     `--SEARCH e USING INDEX idx_amux_state_events_entity
///
/// Note also that it is SEARCH in both cases. A `SCAN` vs `SEARCH` check — the
/// other obvious one — is green on the specimen too. The discriminator is the
/// presence of USING INDEX, and nothing else in that output separates the two.
#[cfg(test)]
mod cost_tests {
    use super::*;

    /// An EXPLAIN QUERY PLAN row: (id, parent, detail).
    ///
    /// `Err` means "not explainable here", which is NOT the same as "costs
    /// nothing" and must never quietly become a pass. Trigger bodies split out
    /// of a CREATE TRIGGER reference `new.*` and cannot prepare standalone, so
    /// this cannot hard-fail — instead `explained` below counts what actually
    /// got checked, and a named statement pins that the check reached the one
    /// that caused the outage.
    fn plan(conn: &Connection, stmt: &str) -> Option<Vec<(i64, i64, String)>> {
        let mut s = conn.prepare(&format!("EXPLAIN QUERY PLAN {stmt}")).ok()?;
        let rows = s
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(3)?))
            })
            .ok()?
            .flatten()
            .collect::<Vec<_>>();
        Some(rows)
    }

    /// Table accesses that run inside a CORRELATED subquery without an index.
    fn unindexed_correlated_access(rows: &[(i64, i64, String)]) -> Vec<String> {
        let parent_of = |id: i64| rows.iter().find(|r| r.0 == id).map(|r| r.1);
        let detail_of = |id: i64| rows.iter().find(|r| r.0 == id).map(|r| r.2.clone());
        let mut bad = Vec::new();
        for (_, parent, d) in rows {
            let is_access = d.starts_with("SCAN ") || d.starts_with("SEARCH ");
            let indexed = d.contains("USING INDEX")
                || d.contains("USING COVERING INDEX")
                || d.contains("USING INTEGER PRIMARY KEY")
                || d.contains("USING ROWID SEARCH");
            if !is_access || indexed {
                continue;
            }
            let mut cur = *parent;
            let mut hops = 0;
            while cur != 0 && hops < 32 {
                let Some(p) = detail_of(cur) else { break };
                if p.contains("CORRELATED") {
                    bad.push(d.clone());
                    break;
                }
                cur = parent_of(cur).unwrap_or(0);
                hops += 1;
            }
        }
        bad
    }

    /// Statements a migration body executes, minus comments and DDL.
    fn dml(sql: &str) -> Vec<String> {
        let stripped: String = sql
            .lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<Vec<_>>()
            .join("\n");
        stripped
            .split(';')
            .map(|s| s.trim().to_string())
            .filter(|s| {
                let u = s.to_uppercase();
                u.starts_with("UPDATE ")
                    || u.starts_with("DELETE ")
                    || u.starts_with("INSERT ")
                    || u.starts_with("SELECT ")
                    || u.starts_with("WITH ")
                    || u.starts_with("REPLACE ")
            })
            .collect()
    }

    fn squash(s: &str) -> String {
        s.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Every migration, applied in order, with each one's DML plan-checked
    /// against the schema THAT MIGRATION LEAVES BEHIND — previous migrations
    /// plus its own DDL, and nothing from later ones.
    ///
    /// Checking against the FINAL schema would be vacuous for exactly the
    /// incident: 0032 adds the index 0031 needed, so a final-schema check reads
    /// 0031 as fine no matter what 0031 does.
    #[test]
    fn no_migration_runs_an_unindexed_correlated_subquery() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS _amux_migrations (
                version INTEGER PRIMARY KEY, name TEXT NOT NULL,
                applied_at TEXT NOT NULL, duration_ms INTEGER);",
        )
        .unwrap();

        let mut offenders: Vec<String> = Vec::new();
        let mut explained = 0usize;
        let mut saw_the_0031_backfill = false;

        for m in MIGRATIONS {
            let tx = conn.transaction().unwrap();
            apply_one(&tx, m.sql).unwrap_or_else(|e| panic!("{} failed to apply: {e}", m.name));
            tx.execute(
                "INSERT INTO _amux_migrations (version, name, applied_at) VALUES (?1, ?2, 'test')",
                rusqlite::params![m.version, m.name],
            )
            .unwrap();
            tx.commit().unwrap();

            for stmt in dml(m.sql) {
                let Some(rows) = plan(&conn, &stmt) else { continue };
                explained += 1;
                let sq = squash(&stmt);
                if sq.starts_with("UPDATE issues SET closed_at") {
                    saw_the_0031_backfill = true;
                }
                for bad in unindexed_correlated_access(&rows) {
                    offenders.push(format!(
                        "{}: `{}` runs once per outer row with no index\n    statement: {}",
                        m.name,
                        bad,
                        sq.chars().take(200).collect::<String>()
                    ));
                }
            }
        }

        // VACUITY GUARDS. This check silently passed on the real incident during
        // development because every statement failed to prepare and the helper
        // returned "nothing to see". A count alone is weak, so the statement
        // that caused the outage is pinned BY NAME: if 0031's backfill ever
        // stops being reached, this fails instead of going quietly green.
        assert!(
            explained >= 5,
            "only {explained} statements were explainable — the check is not reaching the SQL \
             it claims to check"
        );
        assert!(
            saw_the_0031_backfill,
            "0031's `UPDATE issues SET closed_at ...` was never explained. That is the statement \
             that held the database for 186 seconds, and this check exists to see it."
        );

        assert!(
            offenders.is_empty(),
            "a migration would full-scan a table once per outer row, which is how 0031 held \
             the database for 186 seconds at startup. Create the index BEFORE the backfill, \
             in the same migration.\n\n{}",
            offenders.join("\n")
        );
    }

    /// The ordering variant the plan check cannot see.
    ///
    /// `apply_one` hands the plain SQL to `execute_batch`, so statements run in
    /// FILE ORDER. An index created after the backfill that needs it produces
    /// exactly the same final schema and exactly the same 186 seconds. The plan
    /// check above is blind to it — it explains against the finished migration,
    /// by which time the index exists either way.
    ///
    /// ONLY THE READ SIDE. An index on the table the DML WRITES is a different
    /// thing and belongs after: building `issues(closed_at)` once the backfill
    /// has populated it is correct practice, and cheaper than maintaining the
    /// index through the update. 0031 does exactly that, so a rule of "no
    /// CREATE INDEX after any DML" fires on the CORRECT file — measured, it did.
    /// What must come first is an index on a table the statement READS, which is
    /// the one that turns into a scan per outer row.
    #[test]
    fn a_migration_creates_its_read_side_indexes_before_the_dml_that_needs_them() {
        fn indexed_table(stmt_upper: &str) -> Option<String> {
            let on = stmt_upper.find(" ON ")? + 4;
            let rest = stmt_upper[on..].trim_start();
            let end = rest.find(['(', ' ', '\n']).unwrap_or(rest.len());
            Some(rest[..end].trim().trim_matches('"').to_string())
        }
        fn written_table(stmt_upper: &str) -> Option<String> {
            let rest = stmt_upper.strip_prefix("UPDATE ")?;
            let end = rest.find([' ', '\n']).unwrap_or(rest.len());
            Some(rest[..end].trim().trim_matches('"').to_string())
        }

        let mut bad = Vec::new();
        for m in MIGRATIONS {
            let body: String = m
                .sql
                .lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n");
            // (offset, table written) for each DML seen so far.
            let mut dml_seen: Vec<(usize, Option<String>)> = Vec::new();
            let mut offset = 0usize;
            for raw in body.split(';') {
                let t = raw.trim().to_uppercase();
                if t.starts_with("UPDATE ") || t.starts_with("DELETE ") || t.starts_with("WITH ") {
                    dml_seen.push((offset, written_table(&t)));
                }
                if t.starts_with("CREATE INDEX") || t.starts_with("CREATE UNIQUE INDEX") {
                    if let Some(idx_table) = indexed_table(&t) {
                        for (d_off, written) in &dml_seen {
                            if offset > *d_off && written.as_deref() != Some(idx_table.as_str()) {
                                bad.push(format!(
                                    "{}: CREATE INDEX on {} appears AFTER a statement that reads \
                                     it — same final schema, same outage",
                                    m.name, idx_table
                                ));
                            }
                        }
                    }
                }
                offset += raw.len() + 1;
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    /// NEGATIVE CONTROL, built from the incident rather than from a convenient
    /// shape: 0031's own UPDATE, against the schema it actually met on
    /// 2026-08-24 — `_amux_state_events` with its single index on `rev`.
    ///
    /// 88af1ff3 edited 0031 to create the index first, so the checks above now
    /// pass on a fresh database, and a check that passes has told you nothing
    /// until you have seen it fail on the thing it was written for.
    #[test]
    fn the_real_0031_specimen_without_its_index_is_caught() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE _amux_state_events (rev INTEGER, entity_type TEXT, entity_id TEXT,
                                              mutation TEXT, at TEXT);
             CREATE INDEX idx_rev ON _amux_state_events(rev);
             CREATE TABLE issues (id TEXT, status TEXT, closed_at INTEGER);",
        )
        .unwrap();

        let backfill = "UPDATE issues SET closed_at = (
                SELECT CAST(MAX(strftime('%s', e.at)) AS INTEGER)
                  FROM _amux_state_events e
                 WHERE e.entity_type = 'task' AND e.entity_id = issues.id)
             WHERE status IN ('done','verified','discarded') AND closed_at IS NULL";

        let before = unindexed_correlated_access(&plan(&conn, backfill).expect("explainable"));
        assert!(
            !before.is_empty(),
            "the check did not fire on the statement that caused the outage — it cannot fail, \
             so its green tells you nothing"
        );

        conn.execute_batch(
            "CREATE INDEX idx_amux_state_events_entity
                 ON _amux_state_events(entity_type, entity_id);",
        )
        .unwrap();
        let after = unindexed_correlated_access(&plan(&conn, backfill).expect("explainable"));
        assert!(
            after.is_empty(),
            "the index that fixed the incident does not clear the check: {after:?}"
        );
    }
}
