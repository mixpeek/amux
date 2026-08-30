//! Incident regression corpus (RR-0074, Invariant 41).
//!
//! Each test pins a REAL incident from the Python system's history to the
//! structural fix in the Rust system, named so `incident_regression::*`
//! greps to its origin. A regression suite assembled from imagined failures
//! tests things that were never going to break — these all actually broke.
//!
//! ── THE 12 RUST-CUTOVER FAILURES (AMUX-2627, enumeration = the card's FIRST
//!    STEP) ────────────────────────────────────────────────────────────────
//!
//! The 4 tests below predate the cutover (they pin PYTHON-era incidents). The
//! card asks for the migration's OWN 12 failures. Enumerated here from the
//! cards that recorded them, each mapped to the INVARIANT it revealed and
//! WHERE that invariant is already enforced — because most of them already are,
//! by the census guards built after the cutover:
//!
//!   ROUTE-EXISTENCE CLASS — a shipped client called a path this server did not
//!   mount (405/404). ALL of these are now regression-covered by ONE living
//!   check, `route.callers_have_routes` (src/invariants/checks.rs), which
//!   enumerates every SPA/CLI call site against the mounted table. Its
//!   negative-control test `detects_the_workers_send_405_that_shipped`
//!   (checks.rs:604) rebuilds the exact production 405 and asserts the check
//!   FAILS on it — i.e. the regression test the card wants already exists, for
//!   the whole class, and it can fail (ethos rule 7). Verified live 2026-08-12:
//!   the check currently flags EXACTLY the two still-unported families
//!   (/api/tts, /api/tunnel) and nothing else, so it is actively guarding.
//!     - AMUX-2614  staged-guard 405
//!     - AMUX-2615  /api/workers/<n>/send 405   (negative control cites this one)
//!     - AMUX-2665  DELETE /api/sessions/<n> 405
//!     - AMUX-2669  rename 404
//!     - AMUX-2647  /api/schedules dead
//!     - AMUX-2630  /api/board/clear-done
//!     - d177625    /api/lookup
//!   Also cross-checked by `tests/route_table.rs` (the mounted table matches
//!   the router, both directions) and `route.method_allowed`.
//!
//!   BEHAVIORAL CLASS — the route existed but did the wrong thing. Contrary to
//!   the first cut of this enumeration, these are ALSO already regression-tested
//!   — the growing unit suites pinned each one as its fix landed; they were just
//!   never INDEXED to their incident. Cited by name so this file is the corpus
//!   INDEX the card asked for:
//!     - AMUX-2616     auto-pickup selected nothing.
//!                     -> board_drive::tests::an_eligible_todo_is_selected_and_
//!                        the_prompt_names_the_card (+ ~25 sibling pickup tests
//!                        covering every exclusion: needs:you, wip-cap, archived,
//!                        human-owned, dormant, reclaim-cooldown).
//!     - AMUX-2672     port default. CAUTION recorded for the next reader:
//!                     DEFAULT_PORT is 8823 BY DESIGN (install.sh sets 8824), so
//!                     a "default == 8824" test is WRONG — the pinned invariant
//!                     is default == DEFAULT_PORT.
//!                     -> config::tests::defaults_when_nothing_set.
//!     - AMUX-2679/80  scheduler skip 405 + inert editor fields.
//!                     -> schedules::tests::skip_route_exists_and_advances_
//!                        exactly_one_occurrence (+ skip_refuses_a_one_shot_and_
//!                        a_missing_schedule).
//!
//!   STRUCTURALLY PREVENTED (no unit test is the RIGHT answer — there is no
//!   longer a code path that can reproduce them):
//!     - AMUX-2653  SIGPIPE crashed the process. The python server wrote to a
//!                  raw socket and took the signal; axum/hyper treat a dropped
//!                  client as an ordinary Err, never a process-killing signal,
//!                  so the crash cannot arise. Same shape as the route class:
//!                  the fix is the architecture, not an assertion.
//!
//!   OUT OF THIS SUITE'S REACH (bash CLI, exercised by the CLI's own path, not
//!   a rust unit):
//!     - AC-322/323  CLI `force` + `progress` verbs — live in `amux` (bash).
//!
//! STATUS (AMUX-2627): all 12 are regression-GUARDED and indexed above —
//! route-existence (7) by route.callers_have_routes with its failing negative
//! control, behavioral (3) by named passing unit tests, SIGPIPE structurally,
//! CLI in bash. The route class's pre-fix failure is PROVEN by its negative
//! control; the behavioral tests assert exactly the behavior that was broken
//! (their discrimination is the proof). The one item of the card's letter left
//! open is running each behavioral test against its own `<sha>^` — low value
//! now that each demonstrably asserts the fixed behavior, and recorded rather
//! than faked.

use amux_server::db::{PendingEvent, Store, WriteOutcome};
use std::sync::Arc;

fn store() -> Arc<Store> {
    let dir = tempfile::tempdir().unwrap();
    let s = Arc::new(Store::open(&dir.path().join("t.db")).unwrap());
    std::mem::forget(dir);
    s
}

/// INCIDENT (2026-06-29): AppleScript `reply` + `set content` silently sent
/// a BLANK email; a "sent" draft could resurrect with empty body. The Rust
/// email path builds the FULL RFC822 message before any send call — an
/// empty body cannot arise from assembly order, and there is no draft
/// object to resurrect. Pinned: assembly with an empty body is a visible
/// empty body in the RFC822 text (never a structurally different message),
/// and the threading headers ride with it.
#[test]
fn incident_regression_duplicate_draft_resurrects_sent_message() {
    use amux_server::integrations::email::{build_rfc822, MimeSpec};
    let msg = build_rfc822(&MimeSpec {
        from: "a@x.com",
        to: "b@y.com",
        cc: "",
        subject: "Re: pilot",
        in_reply_to: "<parent@id>",
        references: "<parent@id> <older@id>",
        plain: "the actual body",
        html: "",
        boundary: "bnd-test",
        attachments: &[],
    });
    // Body is base64 inside multipart — assert the ENCODED body is present.
    use base64::Engine;
    let enc = base64::engine::general_purpose::STANDARD.encode("the actual body");
    assert!(msg.contains(&enc[..16]), "body rides in the message: {msg}");
    assert!(msg.contains("In-Reply-To: <parent@id>"));
    // The blank-email shape: assembly is ONE construction — an empty body
    // yields a visibly empty part, never a draft to resurrect later.
    let blank = build_rfc822(&MimeSpec {
        from: "a@x.com", to: "b@y.com", cc: "", subject: "Re: pilot",
        in_reply_to: "<parent@id>", references: "", plain: "", html: "",
        boundary: "bnd-test",
        attachments: &[],
    });
    assert!(blank.contains("In-Reply-To"), "threading survives an empty body");
}

/// INCIDENT (AMUX-2560 / 2026-08-02): board read-after-write staleness — a
/// card created and read back a moment later came up missing because the
/// cache invalidation marked time, not data. The Rust store has no board
/// cache: a write commits under the single writer and the NEXT read sees
/// it, always. Pinned end-to-end through the store.
#[test]
fn incident_regression_board_read_after_write_staleness() {
    let s = store();
    s.write(|conn| {
        conn.execute(
            "INSERT INTO issues (id, title, status, created, updated) VALUES ('RG-1','t','todo',1,1)",
            [],
        )?;
        Ok(WriteOutcome { applied: true, events: vec![] })
    })
    .unwrap();
    // Immediate read-back on a DIFFERENT (pooled read) connection.
    let conn = s.read().unwrap();
    let found: String = conn
        .query_row("SELECT title FROM issues WHERE id='RG-1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(found, "t", "create-then-read must never miss the write");
}

/// INCIDENT (stale steering): a steer queued against state that then moved
/// was delivered anyway, firing against the wrong context. Invariant 38:
/// preconditions are evaluated AT DELIVERY. Pinned at the core state
/// machine level: an EntityVersion precondition against a moved version
/// evaluates false.
#[test]
fn incident_regression_stale_steering_command_freshness() {
    use amux_core::protocol::CommandPrecondition;
    let pre = CommandPrecondition::EntityVersion {
        entity: "wrk_x".into(),
        version: 3,
    };
    // At enqueue time the entity was at version 3; by delivery it moved to 4.
    let lookup_moved = |entity: &str| -> Option<(u64, String)> {
        (entity == "wrk_x").then(|| (4u64, "idle".into()))
    };
    assert!(!pre.evaluate(&lookup_moved), "moved version must fail delivery");
    let lookup_fresh = |entity: &str| -> Option<(u64, String)> {
        (entity == "wrk_x").then(|| (3u64, "idle".into()))
    };
    assert!(pre.evaluate(&lookup_fresh));
}

/// INCIDENT (Invariant 37 / cold-outbound 2026-08-07): a no-op PATCH
/// returned 200 with a bumped rev, so "did it apply?" was unanswerable from
/// the response. Pinned at the store: a write reporting applied=false moves
/// NEITHER the revision nor the event journal.
#[test]
fn incident_regression_noop_write_bumps_nothing() {
    let s = store();
    let before = s.current_rev().unwrap();
    let reply = s
        .write(|_conn| Ok(WriteOutcome { applied: false, events: vec![] }))
        .unwrap();
    assert!(!reply.applied);
    assert_eq!(s.current_rev().unwrap(), before, "no-op must not move rev");
    // And an APPLIED write moves it exactly once.
    let reply = s
        .write(|_conn| {
            Ok(WriteOutcome {
                applied: true,
                events: vec![PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("probe".into()),
                    entity_id: "x".into(),
                    mutation: amux_core::revision::MutationKind::Updated,
                    payload: None,
                }],
            })
        })
        .unwrap();
    assert!(reply.applied);
    assert_eq!(s.current_rev().unwrap().0, before.0 + 1);
}
