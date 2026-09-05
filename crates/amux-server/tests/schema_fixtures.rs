//! Test fixtures build the `issues` schema FROM the migrations (AF-328).
//!
//! Four test fixtures used to hand-write `CREATE TABLE issues (...)` mirroring
//! `migrations/`, and nothing kept them in step. The failure when one fell
//! behind was badly misleading: `COLS` selects the new column, `prepare` fails,
//! an `unwrap_or_default()` swallows the error, the query returns `None`, and
//! the test reports its OWN assertion. Migration 0037 produced 38 failures
//! across `board_drive` and not one mentioned a schema or named a column — the
//! top one read "the 3-day-old card must be worked before the fresh one,
//! left: None", which sends you to read the scoring logic. Migration 0036 cost
//! the same tax.
//!
//! Converting them to `migrate::test_memdb()` also surfaced that the fixtures
//! were more PERMISSIVE than production, which is the sharper half: they
//! declared `title TEXT NOT NULL DEFAULT ''` and `created INTEGER NOT NULL
//! DEFAULT 0` where the real schema has no defaults, and they created
//! `statuses` EMPTY where the migrations seed it. So tests were passing against
//! constraints that do not exist. Drift does not only hide columns.
//!
//! This guard is what stops a literal coming back. It reads the source rather
//! than the schema, because the hazard is textual: someone adds a fixture by
//! copying an old one.

use std::path::Path;

/// The only `CREATE TABLE issues` literals allowed in the crate's sources, and
/// why each is exempt.
///
/// Every one is DELIBERATELY NARROW: it declares only the columns its own test
/// touches, so it mirrors nothing and cannot drift. That is the discriminator —
/// not "it is a test", but "it does not claim to be the real schema". One of
/// them is deliberately BROKEN, which is the clearest case of the same rule.
const ALLOWED: &[(&str, &str)] = &[
    (
        "id TEXT PRIMARY KEY, status TEXT, session TEXT",
        "the REAL-timestamp regression fixture: it needs a specific 7-column shape to \
         reproduce one bad cell, and adding columns would not make it more faithful",
    ),
    (
        "id TEXT PRIMARY KEY, depends_on TEXT, deleted INT",
        "the depends_on cycle fixture: three columns, no schema claim",
    ),
    (
        "id TEXT, status TEXT, closed_at INTEGER",
        "the 0031 migration specimen: three columns, exercising one index",
    ),
    (
        "nope TEXT",
        "DELIBERATELY BROKEN. It proves a reader failure surfaces as a failed invariant, \
         so replacing it with the real schema would delete the test",
    ),
    (
        "id TEXT, source_ref TEXT, status TEXT",
        "the autofix one-card-per-fault fixture: names only what it queries",
    ),
];

fn sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                if let Ok(s) = std::fs::read_to_string(&p) {
                    out.push((p.display().to_string(), s));
                }
            }
        }
    }
    out
}

#[test]
fn no_test_fixture_hand_rolls_the_full_issues_schema() {
    let mut offenders: Vec<String> = Vec::new();
    let mut seen = 0usize;
    for (path, src) in sources() {
        for (idx, _) in src.match_indices("CREATE TABLE issues (") {
            // SKIP COMMENTS. The first run of this guard flagged its own
            // sibling's DOC COMMENT, which quotes the literal it is warning
            // about — a scanner that cannot tell prose from code reports the
            // documentation of a rule as a violation of it.
            let line_start = src[..idx].rfind('\n').map_or(0, |n| n + 1);
            let prefix = src[line_start..idx].trim_start();
            if prefix.starts_with("//") || prefix.starts_with("*") {
                continue;
            }
            seen += 1;
            // The literal runs to the closing paren of the column list.
            let tail = &src[idx..];
            let body: String = tail.chars().take(400).collect();
            if ALLOWED.iter().any(|(sig, _)| body.contains(sig)) {
                continue;
            }
            let line = src[..idx].matches('\n').count() + 1;
            offenders.push(format!("{path}:{line}"));
        }
    }

    // POSITIVE CONTROL. If the scan stops finding the sanctioned literals,
    // it is reading nothing and an empty offender list means nothing — the same
    // "a check that cannot fail" shape this whole card is about (ethos rule 7).
    assert!(
        seen >= ALLOWED.len(),
        "the source scan found {seen} `CREATE TABLE issues (` literals but {} are known to \
         exist — the scan is broken, not the tree",
        ALLOWED.len()
    );

    assert!(
        offenders.is_empty(),
        "hand-rolled `issues` schema in test code at {offenders:?}.\n\
         Use `crate::db::migrate::test_memdb()` instead: it applies the real migration chain, \
         so a new column is present the moment its migration is registered and a fixture can \
         never fall behind.\n\
         If the fixture is deliberately NARROW (declaring only the columns its own test \
         touches, claiming to be nothing), add its signature to ALLOWED in this file with the \
         reason."
    );
}

/// The helper really does carry the CURRENT schema, not a stale snapshot.
///
/// Pinned against the newest columns rather than a count, because a count would
/// pass against any 33-column table and the property is "the latest migration is
/// in here".
#[test]
fn the_migrated_test_db_carries_the_newest_columns() {
    let mut c = rusqlite::Connection::open_in_memory().unwrap();
    amux_server::db::migrate::apply_all(&mut c).expect("migrations apply to a fresh db");
    let cols: Vec<String> = c
        .prepare("PRAGMA table_info(issues)")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(1))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    for newest in [
        "evidence",
        "ask_type",
        "ask_question",
        "ask_unblocks",
        "ask_actor",
        "requested_by",
        "callback_session",
        "callback_prompt",
        "callback_state",
        "callback_message_id",
        "callback_fired_at",
        "callback_error",
    ] {
        assert!(cols.contains(&newest.to_string()), "{newest} missing from {cols:?}");
    }
    // And the constraints the old fixtures relaxed are really there, since that
    // divergence is what let tests pass against a schema that does not exist.
    let notnull_no_default: Vec<String> = c
        .prepare("SELECT name FROM pragma_table_info('issues') WHERE \"notnull\"=1 AND dflt_value IS NULL")
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    for strict in ["title", "created", "updated"] {
        assert!(
            notnull_no_default.contains(&strict.to_string()),
            "{strict} should be NOT NULL with no default; the old fixtures gave it one and hid \
             every insert that omitted it. got {notnull_no_default:?}"
        );
    }
}
