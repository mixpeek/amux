//! Every timestamp-shaped column in the schema declares its unit (AMUX-3952).
//!
//! THIS EXISTED AS A RUNTIME INVARIANT AND ONLY AS ONE. Adding a timestamp
//! column is a two-part change: the migration, and the `TIMESTAMP_COLUMNS`
//! entry that says whether it is seconds or milliseconds. Nothing checked the
//! second part until the server had been built, deployed, and evaluated four
//! times.
//!
//! It worked -- `issues.entered_state_at` shipped undeclared at 22:45 and the
//! invariant filed AMUX-3952 by 22:47. But that path costs a deploy, a detector
//! cycle and a lane's turn to learn something the schema knew at compile time,
//! and the card it files reads like a production fault rather than a missing
//! table entry.
//!
//! Runs the SHIPPED predicate (`undeclared_timestamp_columns`) against a
//! database built FROM THE MIGRATION CHAIN, so a new column is in scope the
//! moment its migration is registered and this cannot drift from the runtime
//! check the way a re-derivation would (AMUX-3814).

/// A column added without its unit fails HERE, before it can reach a board card.
#[test]
fn every_timestamp_shaped_column_declares_its_unit() {
    let conn = amux_server::db::migrate::test_memdb_pub();
    let (undeclared, _n) = amux_server::invariants::monitor::undeclared_timestamp_columns(&conn);
    assert!(
        undeclared.is_empty(),
        "timestamp-shaped columns with no declared unit: {undeclared:?}\n\
         Add each to TIMESTAMP_COLUMNS in crates/amux-server/src/invariants/checks.rs \
         with `false` for seconds or `true` for milliseconds. The column NAME cannot say \
         which, which is the whole reason that table exists."
    );
}

/// CONTROL: the check can actually see the schema.
///
/// An empty `undeclared` list is the pass condition, and it is also what a
/// broken scan returns. Without this, deleting the scan body would leave the
/// test above permanently green -- the exact failure mode the invariant's own
/// `found.is_empty()` arm guards against at runtime ("the schema read failed,
/// this is not a clean bill").
#[test]
fn the_scan_can_see_the_schema_so_an_empty_result_means_something() {
    let conn = amux_server::db::migrate::test_memdb_pub();
    let (_undeclared, n_scanned) =
        amux_server::invariants::monitor::undeclared_timestamp_columns(&conn);
    // Through the SAME function the assertion above trusts, not a second query
    // against the schema. The first version of this control asked
    // `pragma_table_info` directly, which proved the columns EXIST and said
    // nothing about whether the scan finds them -- and a mutant that disabled
    // the scan's match left it green.
    assert!(
        n_scanned > 40,
        "the scan found only {n_scanned} timestamp-shaped columns; this schema has ~50, \
         so an empty undeclared-list would mean the scan is broken rather than the \
         schema clean"
    );
}
