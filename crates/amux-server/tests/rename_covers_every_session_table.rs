//! A rename must carry every table keyed by a session name (AMUX-4033).
//!
//! Ethan, 2026-09-02: "I tried to rename a worker and it doesnt work ... make
//! sure renames dont change anything under the hood."
//!
//! The rename cascade carried two hand-maintained arrays of table names. A
//! table added to the schema later did not join them, and nothing anywhere
//! said so: the UPDATE simply never ran, the rows kept the dead name, and the
//! lane lost that data with the rename reporting `ok: true` and a tidy list of
//! completed steps. Measured on the live database while renaming
//! `leadership-coaching`: 12 of 21 session-keyed tables were uncovered,
//! including `token_ledger` (the lane's entire cost history) and
//! `telegram_mappings` (chat routing, so replies would address a name that no
//! longer resolves).
//!
//! A LIST NOBODY IS FORCED TO UPDATE IS THE BUG, not the symptom, which is why
//! this reads the SCHEMA rather than checking the list against itself. Built
//! from `migrations/` via `test_memdb_pub`, the same argument AF-328 made for
//! the issues fixtures: a fixture that mirrors the schema by hand drifts, and
//! the drift is invisible until it costs something.

use amux_server::api::session_verbs::{RenameDisposition, SESSION_SCOPED_TABLES};

/// Columns that hold a session NAME. `reviewer` and `shepherd` live on
/// `issues` and address other lanes; they are covered by their own statements
/// in `RENAME_MIGRATIONS` and are named here so a new table carrying one is
/// still caught.
const SESSION_COLUMNS: [&str; 3] = ["session", "reviewer", "shepherd"];

#[test]
fn every_session_keyed_table_declares_what_a_rename_does_with_it() {
    let conn = amux_server::db::migrate::test_memdb_pub();

    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .expect("read schema")
        .query_map([], |r| r.get::<_, String>(0))
        .expect("query tables")
        .filter_map(Result::ok)
        .collect();

    let mut session_keyed: Vec<String> = Vec::new();
    for t in &tables {
        let cols: Vec<String> = conn
            .prepare(&format!("PRAGMA table_info({t})"))
            .and_then(|mut st| {
                st.query_map([], |r| r.get::<_, String>(1)).map(|rows| rows.filter_map(Result::ok).collect())
            })
            .unwrap_or_default();
        if cols.iter().any(|c| SESSION_COLUMNS.contains(&c.as_str())) {
            session_keyed.push(t.clone());
        }
    }

    // The fixture must actually have found something, or this passes by
    // measuring nothing — the failure mode this whole file is about.
    assert!(
        session_keyed.len() >= 10,
        "expected the schema to have many session-keyed tables, found {}: {session_keyed:?}. \
         A near-empty list means the schema did not build, not that the cascade is complete.",
        session_keyed.len()
    );

    let declared: Vec<&str> = SESSION_SCOPED_TABLES.iter().map(|(t, _)| *t).collect();
    let undeclared: Vec<&String> =
        session_keyed.iter().filter(|t| !declared.contains(&t.as_str())).collect();
    assert!(
        undeclared.is_empty(),
        "table(s) keyed by a session name with NO declared rename disposition: {undeclared:?}.\n\
         A rename will silently leave their rows on the old name. Add each to \
         SESSION_SCOPED_TABLES as Migrate (the lane's own state, must follow it) or \
         KeepForAudit(why) (an append-only record of what happened under that name)."
    );

    // The reverse direction: a declared table that no longer exists is a stale
    // entry, and it would make the cascade log a per-rename error forever.
    let stale: Vec<&&str> = declared.iter().filter(|t| !session_keyed.contains(&t.to_string())).collect();
    assert!(
        stale.is_empty(),
        "SESSION_SCOPED_TABLES names table(s) the schema does not have: {stale:?}. \
         Remove them, or the cascade reports a failed UPDATE on every rename."
    );
}

/// The audit exemptions have to SAY why. "Not in the migrate list" and
/// "deliberately excluded" are the same absence otherwise, which is the exact
/// ambiguity that let twelve tables sit uncovered while looking intentional.
#[test]
fn every_audit_exemption_gives_a_reason() {
    for (t, d) in SESSION_SCOPED_TABLES {
        if let RenameDisposition::KeepForAudit(why) = d {
            assert!(
                why.trim().len() > 20,
                "{t} keeps the old name but its reason is too thin to act on: {why:?}"
            );
        }
    }
}

/// The cascade must actually EXECUTE the declaration. A list that nothing reads
/// is exactly as green as one that is wired, so assert the two agree: every
/// Migrate table is either driven by the generic loop or named in the custom
/// statements, and nothing is Migrate-but-unreachable.
#[test]
fn every_migrate_table_is_reachable_by_the_cascade() {
    let simple = amux_server::api::session_verbs::simple_rename_tables();
    let custom: Vec<&str> = amux_server::api::session_verbs::RENAME_MIGRATIONS
        .iter()
        .map(|(name, _)| name.split('.').next().unwrap_or(name))
        .collect();

    for (t, d) in SESSION_SCOPED_TABLES {
        if matches!(d, RenameDisposition::Migrate) {
            assert!(
                simple.contains(t) || custom.contains(t),
                "{t} is declared Migrate but no statement in the cascade touches it"
            );
        }
    }
    // ...and nothing the cascade updates is missing from the declaration, or
    // the schema test above would not see it.
    let declared: Vec<&str> = SESSION_SCOPED_TABLES.iter().map(|(t, _)| *t).collect();
    for t in &simple {
        assert!(declared.contains(t), "the cascade updates {t} but it is not declared");
    }
}
