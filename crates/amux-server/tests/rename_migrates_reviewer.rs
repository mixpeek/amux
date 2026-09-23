//! A rename must carry a lane's name on EVERY column that addresses it
//! (AMUX-3751).
//!
//! The cascade migrated `issues.session` — the card's owner — and left
//! `issues.reviewer` and `issues.shepherd` pointing at the dead name. So
//! renaming a lane silently orphaned every card that had asked it to review
//! something: the card still reads `review`, which looks healthy, while the
//! reviewer nudge is addressed to a session that no longer exists.
//!
//! Measured on the live board 2026-08-26: two cards still named `amux-rust`
//! as reviewer, a lane renamed to `amux` long ago, and 7 open cards sat in
//! `review` naming a reviewer that resolves to no registered worker at all.
//!
//! This RUNS the shipped statements against a real SQLite table rather than
//! restating the list. A test that asserts "the array contains a reviewer
//! entry" is green whichever SQL that entry holds, and green against a
//! typo'd column name that fails only in production.

use amux_server::api::session_verbs::RENAME_MIGRATIONS;

fn issues_fixture() -> rusqlite::Connection {
    // This test exercises the shipped rename SQL, so its table must come from
    // the shipped migrations too. The former hand-written five-column copy
    // drifted as soon as issues gained another live routing field and made the
    // whole workspace suite fail before it reached any rename assertion.
    let c = amux_server::db::migrate::test_memdb_pub();
    c.execute_batch(
        "INSERT INTO issues (id,title,session,reviewer,shepherd,deleted,created,updated)
             VALUES ('A','owner','amux-rust',NULL,NULL,NULL,1,1);
         INSERT INTO issues (id,title,session,reviewer,shepherd,deleted,created,updated)
             VALUES ('B','reviewer','ecology','amux-rust',NULL,NULL,1,1);
         INSERT INTO issues (id,title,session,reviewer,shepherd,deleted,created,updated)
             VALUES ('C','shepherd','ecology',NULL,'amux-rust',NULL,1,1);
         INSERT INTO issues (id,title,session,reviewer,shepherd,deleted,created,updated)
             VALUES ('D','deleted','ecology','amux-rust',NULL,1,1,1);",
    )
    .unwrap();
    c
}

fn run_issue_migrations(c: &rusqlite::Connection, old: &str, new: &str) {
    for (table, sql) in RENAME_MIGRATIONS {
        if !table.starts_with("issues") {
            continue;
        }
        c.execute(sql, rusqlite::params![new, old])
            .unwrap_or_else(|e| panic!("{table}: {e}"));
    }
}

fn one(c: &rusqlite::Connection, sql: &str) -> String {
    c.query_row(sql, [], |r| r.get::<_, Option<String>>(0))
        .unwrap()
        .unwrap_or_default()
}

#[test]
fn a_rename_carries_owner_reviewer_and_shepherd() {
    let c = issues_fixture();
    run_issue_migrations(&c, "amux-rust", "amux");

    assert_eq!(
        one(&c, "SELECT session FROM issues WHERE id='A'"),
        "amux",
        "owner"
    );
    assert_eq!(
        one(&c, "SELECT reviewer FROM issues WHERE id='B'"),
        "amux",
        "the reviewer column is what AMUX-3751 was about: a card left naming the dead lane \
         waits in `review` for a session that cannot be addressed"
    );
    assert_eq!(
        one(&c, "SELECT shepherd FROM issues WHERE id='C'"),
        "amux",
        "shepherd"
    );
}

#[test]
fn a_deleted_card_keeps_the_old_name() {
    // Python parity, and deliberate: historical rows record what happened
    // under the name it happened under. Pinned so a future widening of the
    // cascade is a decision rather than an accident.
    let c = issues_fixture();
    run_issue_migrations(&c, "amux-rust", "amux");
    assert_eq!(
        one(&c, "SELECT reviewer FROM issues WHERE id='D'"),
        "amux-rust"
    );
}

#[test]
fn the_migrations_name_columns_that_exist() {
    // Every statement must actually run against the real schema. A typo'd
    // column name is invisible in the source and fails only in production,
    // where the cascade swallows the error as "table absent (fresh home)".
    let c = issues_fixture();
    for (table, sql) in RENAME_MIGRATIONS {
        if table.starts_with("issues") {
            c.execute(sql, rusqlite::params!["x", "y"])
                .unwrap_or_else(|e| panic!("{table}: {e}"));
        }
    }
}
