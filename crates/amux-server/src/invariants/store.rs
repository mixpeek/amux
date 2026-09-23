//! Durable persistence for invariant results and incidents (AMUX-2622).
//!
//! The dedupe rule lives here rather than in the caller so it cannot be
//! forgotten by a future check: recording a failure for an
//! (invariant_id, entity_key) that is already failing UPDATES that incident
//! (occurrences++, last_seen moves) instead of inserting a second one, and
//! recording a pass RESOLVES it. A check firing every 30s for a day is one
//! incident with 2880 occurrences.

use super::{InvariantResult, Status};
use crate::db::{SharedStore, WriteOutcome};
use serde_json::json;

/// Retention for the append-only evaluation log. Short by design: its purpose
/// is "is this check still running / when did it last pass", not history.
/// Incidents are the durable record and are never trimmed here.
///
/// Differential (AMUX-3489): a flat 7-day window held 8M rows (~2GB of DB) —
/// 15 invariants fanned out per-entity write ~13 rows/sec, overwhelmingly
/// green heartbeats nothing reads past the first minutes. Failures keep the
/// week (they are what gets grepped after an incident); passes keep an hour,
/// 12x the 300s stale_checks threshold that is the only consumer of
/// pass-row age. Flap history lives in _amux_invariant_incident either way.
const RESULT_RETAIN_SECS: f64 = 7.0 * 86400.0;
const PASS_RETAIN_SECS: f64 = 3600.0;
/// Trim cap per write cycle: the first cycle after the differential retention
/// ships faces a ~7M-row backlog, and deleting it in one transaction would
/// balloon the WAL past the DB's own size. The monitor cycles constantly, so
/// a capped trim drains the backlog in hours anyway.
const TRIM_BATCH_ROWS: i64 = 20_000;

/// Results per writer acquisition (AMUX-4750).
///
/// A LONG HOLD IS THE FAULT, NOT THE TOTAL WORK. Every non-GET request in the
/// fleet awaits `record_receipt` in `policy::enforce` before its handler runs,
/// so the single writer's hold is a floor under every write anyone makes: a
/// 2.5s hold is 2.5s added to somebody's POST. Chunking does not reduce the
/// work, it bounds how long any one interactive write can be stuck behind it.
///
/// This deliberately walks back part of the "one write per pass" decision above,
/// and the reason that decision was right still holds: a write PER RESULT would
/// be 727 lock acquisitions. This is ~8, each releasing the writer so anything
/// queued behind it can go.
///
/// Sized from the measurement: ~1,150 statements per pass at ~2ms each, so 100
/// results is roughly 150 statements and well inside the 250ms the card asks
/// for. Every row of a pass still carries the SAME `ts`, computed once, so a
/// batch remains identifiable even though a reader can now catch one half
/// written.
const RESULTS_PER_WRITE: usize = 100;

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Persist a batch of results and reconcile incidents in ONE write.
///
/// Batched deliberately: the store is single-writer, and a per-result write
/// would put a full checker pass (dozens of results) into as many write-lock
/// acquisitions as it has checks — the shape that produced log-lock contention
/// before. Returns the number of incidents newly OPENED, which is the signal a
/// caller acts on.
pub async fn record(store: &SharedStore, results: Vec<InvariantResult>, duration_ms: i64) -> usize {
    if results.is_empty() {
        return 0;
    }
    // SERIALIZE BEFORE TAKING THE WRITER (AMUX-4750). `evidence.to_string()` is
    // pure CPU with no connection in it, and it ran inside the closure — twice
    // per failing result, because the INSERT and the UPSERT each called it on
    // the same value. Measured on the live DB: 727 results a pass, 37 KB of
    // evidence, the largest single blob 19 KB. Every millisecond of that was
    // added to every non-GET request in the fleet, which awaits `record_receipt`
    // in `policy::enforce` before its handler runs.
    let results: Vec<(InvariantResult, String)> = results
        .into_iter()
        .map(|r| {
            let evidence = r.evidence.to_string();
            (r, evidence)
        })
        .collect();
    // Also hoisted: it is the same value for every row and was a call per row.
    let build = crate::build_hash();
    let opened = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // ONE TIMESTAMP FOR THE WHOLE PASS, computed before the first chunk. The
    // batch has to stay identifiable now that it is written across several
    // transactions instead of one.
    let ts = now();
    let mut remaining = results;
    while !remaining.is_empty() {
        let rest = remaining.split_off(remaining.len().min(RESULTS_PER_WRITE));
        let results = std::mem::replace(&mut remaining, rest);
        let opened_w = opened.clone();
        let build = build.clone();
        let _ = store
        .write_async(move |conn| {
            for (r, evidence) in &results {
                // PREPARE ONCE, not once per row. `conn.execute(sql, ..)`
                // compiles its SQL every call: at 727 results with an incident
                // arm that fires for hundreds of them, this path was asking
                // SQLite to parse the same three statements over a thousand
                // times per pass while holding the single writer.
                conn.prepare_cached(
                    // Stamp the loaded process image on every row. Replacing
                    // the file on disk does not change the code executing this
                    // pass; build_hash caches startup identity until exec.
                    "INSERT INTO _amux_invariant_result
                       (ts, invariant_id, status, entity_key, expected, observed, evidence, duration_ms, build)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                )?
                .execute(rusqlite::params![
                    ts,
                    r.invariant_id,
                    r.status.as_str(),
                    r.entity_key,
                    r.expected,
                    r.observed,
                    evidence,
                    duration_ms,
                    build,
                ])?;

                if r.status.opens_incident() {
                    // UPSERT: one incident per (invariant, entity), forever.
                    // first_seen is preserved by the DO UPDATE (it is simply
                    // not assigned), which is what makes "this has been broken
                    // since 09:14" answerable after 2880 occurrences.
                    // RETURNING, so the occurrence count comes back from the
                    // upsert itself. This used to be a second round trip: a
                    // separate `SELECT occurrences` per failing result, on a
                    // row the statement above had just written.
                    let occ: i64 = conn
                        .prepare_cached(
                        "INSERT INTO _amux_invariant_incident
                           (invariant_id, entity_key, status, first_seen, last_seen,
                            occurrences, expected, observed, evidence)
                         VALUES (?1,?2,?3,?4,?4,1,?5,?6,?7)
                         ON CONFLICT(invariant_id, entity_key) DO UPDATE SET
                           status      = excluded.status,
                           last_seen   = excluded.last_seen,
                           occurrences = _amux_invariant_incident.occurrences + 1,
                           expected    = excluded.expected,
                           observed    = excluded.observed,
                           evidence    = excluded.evidence,
                           -- A previously-resolved incident that fails again
                           -- REOPENS rather than staying closed, so a flap is
                           -- visible as one incident with a resolved_at that
                           -- went back to NULL.
                           resolved_at = NULL,
                           -- EPISODE IDENTITY (AMUX-4798), advanced on exactly
                           -- the transition above: only when this row WAS
                           -- resolved and is now failing again. A continuing
                           -- failure leaves it alone, so an episode is one
                           -- unbroken failing run.
                           --
                           -- autofix keys its dedupe on this. It used to key on
                           -- `first_seen`, which its own comment described as
                           -- the start of the current run; `first_seen` is the
                           -- first failure EVER and this statement is why. The
                           -- signature therefore froze for the lifetime of the
                           -- pair and one card was filed per invariant forever
                           -- — 77 incidents were in that state, still failing up
                           -- to 25 days after their card was minted, including
                           -- the one that pinned three lanes for four hours on
                           -- 2026-09-18.
                           --
                           -- first_seen is deliberately NOT reset: it is what
                           -- makes \"broken since\" answerable after 2880
                           -- occurrences, and resetting it would buy a
                           -- per-episode signature by destroying the flap
                           -- history this table exists to hold.
                           episode = _amux_invariant_incident.episode
                               + CASE WHEN _amux_invariant_incident.resolved_at IS NULL THEN 0 ELSE 1 END
                         RETURNING occurrences",
                        )?
                        .query_row(
                            rusqlite::params![
                                r.invariant_id,
                                r.entity_key,
                                r.status.as_str(),
                                ts,
                                r.expected,
                                r.observed,
                                evidence,
                            ],
                            |row| row.get(0),
                        )?;
                    // occurrences == 1 means this upsert INSERTED rather than
                    // updated, which is the definition of a newly opened
                    // incident. The old `changed > 0` half could not
                    // discriminate: an upsert reports one row changed either
                    // way, which is what its own comment said.
                    //
                    // NOT SWALLOWED. The count this feeds is what `monitor`
                    // pages on, and the read it replaced defaulted to 1 on
                    // error — the exact value that means "newly opened", so a
                    // broken read would have reported every repeat failure in
                    // the fleet as brand new, silently. An error here fails the
                    // batch instead, which is loud and recoverable.
                    if occ == 1 {
                        opened_w.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                } else if r.status == Status::Pass || r.status == Status::Unknown {
                    // A pass RESOLVES the matching live incident. Deliberately
                    // not a DELETE: "broke at 09:14, healed at 09:51" is a real
                    // signal, and deleting the row erases the flap.
                    //
                    // UNKNOWN RESOLVES IT TOO, and records itself as `unknown`
                    // rather than `pass` so the row still says what happened
                    // (AMUX-3575). Unknown is what a check reports when it STOPS
                    // BEING ABLE TO RUN — the table it reads went empty, the
                    // probe's target disappeared. Before this, only Pass
                    // resolved, so a fail -> unknown transition left the
                    // incident open FOREVER: nothing could clear it, and no
                    // action by anyone would.
                    //
                    // The specimen: `schema.timestamp_units_declared` opened on
                    // waitlist.ts while its unit was undeclared, 7aaa2a32
                    // declared it, and the verdict moved fail -> unknown ("no
                    // rows to check the unit against") rather than fail -> pass,
                    // because `waitlist` has zero rows and NO WRITER anywhere in
                    // the Rust codebase. It can never have rows, so it could
                    // never pass, so the incident could never resolve — and it
                    // auto-filed a card for a condition no action can satisfy,
                    // which is ethos rule 3.
                    //
                    // Keeping it open is the worse failure, not the safer one:
                    // 22 of 24 open incidents measured on 2026-08-23 had no
                    // evaluation in over 24h, and an open list that is 94% dead
                    // trains people to skim past the two that are real.
                    // Distinguishing the terminal state is what keeps this
                    // honest — "ended unknown" is not a claim that it healed.
                    let end_status =
                        if r.status == Status::Pass { "pass" } else { "unknown" };
                    conn.prepare_cached(
                        "UPDATE _amux_invariant_incident
                            SET resolved_at = ?3, status = ?4
                          WHERE invariant_id = ?1 AND entity_key = ?2
                            AND resolved_at IS NULL",
                    )?
                    .execute(rusqlite::params![
                        r.invariant_id,
                        r.entity_key,
                        ts,
                        end_status
                    ])?;
                }
            }
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    }
    // The trim is once per pass, in its own short write, not once per chunk.
    let _ = store
        .write_async(move |conn| {
            // Opportunistic trim of the evaluation log only. The rowid-IN shape
            // is because DELETE..LIMIT needs a nonstandard SQLite build flag.
            //
            // TWO STATEMENTS, ONE PER RETENTION RULE (AMUX-4750). This was one
            // DELETE whose predicate read `ts < pass_cut AND (status = 'pass'
            // OR ts < fail_cut)`. `idx_inv_result_ts` can only serve that by
            // walking the entire tail older than the pass cut and testing
            // `status` on every row — and the fleet's rows are overwhelmingly
            // FAIL rows younger than the fail cut, so the walk deletes nothing.
            // Measured on the live distribution, 311k rows: 377ms per monitor
            // cycle to delete 0, with the single writer held throughout.
            //
            // Split, each arm seeks: the pass arm on idx_inv_result_status_ts
            // (migration 0080) straight to (status='pass', ts < cut), the age
            // arm on idx_inv_result_ts. Same rows deleted, 0.1ms.
            let _ = conn
                .prepare_cached(
                    "DELETE FROM _amux_invariant_result WHERE rowid IN (
                        SELECT rowid FROM _amux_invariant_result
                         WHERE status = 'pass' AND ts < ?1
                         LIMIT ?2)",
                )
                .and_then(|mut s| {
                    s.execute(rusqlite::params![ts - PASS_RETAIN_SECS, TRIM_BATCH_ROWS])
                });
            let _ = conn
                .prepare_cached(
                    "DELETE FROM _amux_invariant_result WHERE rowid IN (
                        SELECT rowid FROM _amux_invariant_result
                         WHERE ts < ?1
                         LIMIT ?2)",
                )
                .and_then(|mut s| {
                    s.execute(rusqlite::params![ts - RESULT_RETAIN_SECS, TRIM_BATCH_ROWS])
                });
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
    opened.load(std::sync::atomic::Ordering::Relaxed)
}

/// Live (unresolved) incidents, worst-and-freshest first.
pub fn live_incidents(store: &SharedStore) -> anyhow::Result<Vec<serde_json::Value>> {
    let conn = store.read()?;
    let mut stmt = conn.prepare(
        "SELECT invariant_id, entity_key, status, first_seen, last_seen, occurrences,
                expected, observed, evidence, board_issue
           FROM _amux_invariant_incident
          WHERE resolved_at IS NULL
          ORDER BY last_seen DESC",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(json!({
                "invariant_id": r.get::<_, String>(0)?,
                "entity":       r.get::<_, String>(1)?,
                "status":       r.get::<_, String>(2)?,
                "first_seen":   r.get::<_, f64>(3)?,
                "last_seen":    r.get::<_, f64>(4)?,
                "occurrences":  r.get::<_, i64>(5)?,
                "expected":     r.get::<_, String>(6)?,
                "observed":     r.get::<_, String>(7)?,
                "evidence":     serde_json::from_str::<serde_json::Value>(
                                    &r.get::<_, String>(8)?).unwrap_or(serde_json::Value::Null),
                "board_issue":  r.get::<_, String>(9)?,
            }))
        })?
        .flatten()
        .collect();
    Ok(rows)
}

/// Latest evaluation per invariant — the "is this check still running?" view.
///
/// `age_s` is the point of this query. A check whose newest row is hours old
/// has stopped running, and that must be visible as staleness rather than as
/// its last (stale) verdict — evidence freshness, spec §25.
pub fn latest_per_invariant(store: &SharedStore) -> anyhow::Result<Vec<serde_json::Value>> {
    let conn = store.read()?;
    // AEAB-44. The obvious spelling of this — a `GROUP BY invariant_id`
    // subquery — scans the WHOLE evaluation log to produce 304 rows, and that
    // log is the largest table amux has (9.4M rows / 1.7 GB with indexes on
    // 2026-08-22, see AEAB-41). Measured on the live DB: 12.9s for the GROUP BY
    // form, 0.002s for this one, returning row-for-row identical results.
    //
    // The recursive CTE is a loose index scan: walk the DISTINCT invariant_ids
    // by repeatedly asking for the next one greater than the last, then ask
    // `max(ts)` per id. Both steps are index seeks on
    // idx_inv_result_id (invariant_id, ts DESC) — the index was already there,
    // the GROUP BY simply could not use it. Cost is O(distinct ids), not
    // O(rows), so this stays fast as the log grows rather than degrading in
    // exact proportion to the thing it measures. That mattered: this is the
    // ONLY view of live incidents and of checks that have STOPPED running, so
    // it was least usable at the moment it was most needed.
    //
    // NOTE it is deliberately NOT one row per invariant despite the name. It
    // returns every row of the newest BATCH for each invariant — one per
    // entity — which is what the caller renders. Collapsing to one row per
    // invariant would silently hide 63 of the 64 entities of
    // route.callers_have_routes.
    let mut stmt = conn.prepare(
        "WITH RECURSIVE ids(id) AS (
             SELECT (SELECT min(invariant_id) FROM _amux_invariant_result)
             UNION ALL
             SELECT (SELECT min(invariant_id) FROM _amux_invariant_result
                      WHERE invariant_id > ids.id)
               FROM ids WHERE ids.id IS NOT NULL
         ),
         latest AS (
             SELECT id AS invariant_id,
                    (SELECT max(ts) FROM _amux_invariant_result r
                      WHERE r.invariant_id = ids.id) AS mt
               FROM ids WHERE id IS NOT NULL
         )
         SELECT r.invariant_id, r.status, r.entity_key, r.expected, r.observed, r.ts, r.evidence, r.build
           FROM _amux_invariant_result r
           JOIN latest l ON l.invariant_id = r.invariant_id AND l.mt = r.ts
          ORDER BY r.invariant_id",
    )?;
    let now = now();
    let rows = stmt
        .query_map([], |r| {
            let ts: f64 = r.get(5)?;
            let status: String = r.get(1)?;
            let mut row = json!({
                "invariant_id": r.get::<_, String>(0)?,
                "status":       status,
                "entity":       r.get::<_, String>(2)?,
                "expected":     r.get::<_, String>(3)?,
                "observed":     r.get::<_, String>(4)?,
                "checked_at":   ts,
                "age_s":        (now - ts).max(0.0),
                // Which BUILD produced this verdict. '' for rows written before
                // migration 0078, which reads as unknown rather than mismatched.
                "build":        r.get::<_, String>(7).unwrap_or_default(),
            });
            // AMUX-4538. Checks store their causal slice in `evidence` (a
            // failure's per-card sample, for instance) and this read dropped
            // it, so a verdict that said "see evidence.sample" pointed at a
            // field no endpoint returned. Carried on non-pass rows only: the
            // pass rows are ~640 and their evidence is a population count the
            // verdict already implies.
            if row["status"] != "pass" {
                let raw: String = r.get(6)?;
                if !raw.trim().is_empty() && raw.trim() != "{}" {
                    row["evidence"] =
                        serde_json::from_str(&raw).unwrap_or(serde_json::Value::String(raw));
                }
            }
            Ok(row)
        })?
        .flatten()
        .collect();
    Ok(rows)
}

/// Size of the evaluation log itself: (rows, oldest_age_s). Feeds both
/// `/api/debug/invariants` and the store.result_log_bounded check (AMUX-3489)
/// so the meter a human reads and the meter that pages cannot disagree.
pub fn result_log_stats(store: &SharedStore) -> anyhow::Result<(i64, f64)> {
    let conn = store.read()?;
    let (rows, oldest): (i64, Option<f64>) = conn.query_row(
        "SELECT COUNT(*), MIN(ts) FROM _amux_invariant_result",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((rows, oldest.map(|t| (now() - t).max(0.0)).unwrap_or(0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invariants::InvariantResult;

    #[tokio::test]
    async fn invariant_record_does_not_rehash_the_binary_inside_the_writer() {
        let (s, _d) = store();
        // Production measures identity at startup before starting jobs too.
        let build = crate::build_hash();
        let rows = (0..128)
            .map(|n| {
                let mut r = InvariantResult::pass("test.writer_latency");
                r.entity_key = n.to_string();
                r
            })
            .collect();
        let recording_store = s.clone();
        let recording = tokio::spawn(async move { record(&recording_store, rows, 0).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let started = std::time::Instant::now();
        s.write_async(|_| {
            Ok(WriteOutcome {
                applied: false,
                events: vec![],
            })
        })
        .await
        .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "interactive write waited {:?} behind invariant recording",
            started.elapsed()
        );
        recording.await.unwrap();
        let c = s.read().unwrap();
        let stamped:i64=c.query_row("SELECT count(*) FROM _amux_invariant_result WHERE invariant_id='test.writer_latency' AND build=?1",[build],|r|r.get(0)).unwrap();
        assert_eq!(
            stamped, 128,
            "every result retains the executing image identity"
        );
    }

    /// AMUX-4538. A failing check's stored evidence reaches the reader through
    /// the SHIPPED writer (`record`) and reader (`latest_per_invariant`); a
    /// passing row carries none.
    #[tokio::test]
    async fn a_failing_rows_stored_evidence_is_returned_and_a_pass_row_carries_none() {
        let (s, _d) = store();
        record(
            &s,
            vec![
                InvariantResult::fail(
                    "test.evidence_fail",
                    "every card complete",
                    "2 of 5 incomplete",
                )
                .evidence(
                    json!({"sample": [{"id": "A-1", "gaps": ["priority"]}], "n_considered": 5}),
                ),
                InvariantResult::pass("test.evidence_pass").evidence(json!({"n_considered": 9})),
            ],
            1,
        )
        .await;
        let rows = latest_per_invariant(&s).unwrap();
        let fail = rows
            .iter()
            .find(|r| r["invariant_id"] == "test.evidence_fail")
            .expect("fail row");
        assert_eq!(fail["evidence"]["sample"][0]["id"], "A-1", "{fail}");
        assert_eq!(fail["evidence"]["n_considered"], 5, "{fail}");
        let pass = rows
            .iter()
            .find(|r| r["invariant_id"] == "test.evidence_pass")
            .expect("pass row");
        assert!(
            pass.get("evidence").is_none(),
            "pass rows stay small: {pass}"
        );
    }

    fn store() -> (SharedStore, tempfile::TempDir) {
        let d = tempfile::tempdir().unwrap();
        let s = crate::db::Store::open(&d.path().join("t.db")).unwrap();
        (std::sync::Arc::new(s), d)
    }

    /// Insert an evaluation row at an EXPLICIT ts, so the test does not depend
    /// on wall-clock ordering between two `record` calls.
    async fn put(s: &SharedStore, ts: f64, id: &str, entity: &str, status: &str) {
        let (id, entity, status) = (id.to_string(), entity.to_string(), status.to_string());
        s.write_async(move |c| {
            c.execute(
                "INSERT INTO _amux_invariant_result
                   (ts, invariant_id, status, entity_key, expected, observed, evidence, duration_ms)
                 VALUES (?1,?2,?3,?4,'','','{}',0)",
                rusqlite::params![ts, id, status, entity],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await
        .unwrap();
    }

    /// AMUX-3575: a check that stops being ABLE to run must not leave a
    /// permanently-open incident.
    ///
    /// The specimen, rebuilt from the live one: `schema.timestamp_units_declared`
    /// opened on `waitlist.ts` while its unit was undeclared; 7aaa2a32 declared
    /// it, and the verdict went fail -> UNKNOWN ("no rows to check the unit
    /// against") rather than fail -> pass, because `waitlist` has zero rows and
    /// no writer anywhere. It can never pass, so under the old rule the incident
    /// could never resolve, and it auto-filed a card for a condition no action
    /// satisfies.
    ///
    /// The control is the half that keeps this honest: resolving on unknown must
    /// NOT record it as a pass. "Stopped being checkable" and "healed" are
    /// different facts and the row has to keep saying which.
    #[tokio::test]
    async fn an_unknown_resolves_an_incident_without_claiming_it_healed() {
        let (s, _d) = store();
        let res = |st: &str| {
            let mut r = InvariantResult::new(
                "schema.timestamp_units_declared",
                match st {
                    "fail" => Status::Fail,
                    "pass" => Status::Pass,
                    _ => Status::Unknown,
                },
            );
            r.entity_key = "waitlist.ts".into();
            r
        };

        record(&s, vec![res("fail")], 0).await;
        let (status, resolved): (String, Option<f64>) = s
            .read()
            .unwrap()
            .query_row(
                "SELECT status, resolved_at FROM _amux_invariant_incident \
                 WHERE entity_key='waitlist.ts'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            status, "fail",
            "precondition: the incident must actually be open"
        );
        assert!(resolved.is_none(), "precondition: and unresolved");

        record(&s, vec![res("unknown")], 0).await;
        let (status, resolved): (String, Option<f64>) = s
            .read()
            .unwrap()
            .query_row(
                "SELECT status, resolved_at FROM _amux_invariant_incident \
                 WHERE entity_key='waitlist.ts'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            resolved.is_some(),
            "an unknown must clear the incident — otherwise a check that stops being able \
             to run stays open forever and nobody can action it"
        );
        assert_eq!(
            status, "unknown",
            "and it must NOT be recorded as a pass: 'stopped being checkable' is not 'healed'"
        );
    }

    /// AEAB-44. `latest_per_invariant` returns the newest BATCH for each
    /// invariant — every entity in it — not one row per invariant.
    ///
    /// Both halves can fail independently and both have to be here:
    ///   - drop the older generation (a query that returns everything passes a
    ///     "the newest row is present" assertion and is still wrong), and
    ///   - keep ALL entities of the newest generation (collapsing to one row
    ///     per invariant would hide 63 of the 64 entities of
    ///     route.callers_have_routes in production, and would pass any
    ///     assertion phrased per-invariant).
    ///
    /// This is the test the index rewrite had to survive. Verified against the
    /// live 9.4M-row table too: old and new forms returned 304 rows with
    /// `EXCEPT` empty in BOTH directions — and note that had to be done inside
    /// ONE transaction, because comparing two queries against a table the
    /// monitor is writing to every 30s reports every row as different.
    #[tokio::test]
    async fn latest_per_invariant_is_the_newest_batch_with_every_entity() {
        let (s, _d) = store();
        // Older generation — must NOT appear.
        put(&s, 100.0, "a.check", "e1", "fail").await;
        put(&s, 100.0, "a.check", "e2", "fail").await;
        put(&s, 100.0, "b.check", "", "pass").await;
        // Newest generation for a.check: three entities, all must appear.
        put(&s, 200.0, "a.check", "e1", "pass").await;
        put(&s, 200.0, "a.check", "e2", "fail").await;
        put(&s, 200.0, "a.check", "e3", "unknown").await;
        // b.check's newest is older than a.check's — per-invariant, not global.
        put(&s, 150.0, "b.check", "", "fail").await;

        let got = latest_per_invariant(&s).unwrap();

        let a: Vec<_> = got
            .iter()
            .filter(|r| r["invariant_id"] == "a.check")
            .collect();
        assert_eq!(
            a.len(),
            3,
            "every entity of the newest batch must appear: {got:?}"
        );
        assert!(
            a.iter().all(|r| r["checked_at"].as_f64().unwrap() == 200.0),
            "the older generation must be gone: {got:?}"
        );
        let mut ents: Vec<&str> = a.iter().map(|r| r["entity"].as_str().unwrap()).collect();
        ents.sort();
        assert_eq!(ents, vec!["e1", "e2", "e3"]);

        let b: Vec<_> = got
            .iter()
            .filter(|r| r["invariant_id"] == "b.check")
            .collect();
        assert_eq!(b.len(), 1, "b.check has one entity: {got:?}");
        assert_eq!(
            b[0]["checked_at"].as_f64().unwrap(),
            150.0,
            "each invariant's own newest ts, not the table's: {got:?}"
        );
        assert_eq!(b[0]["status"], "fail");
    }

    /// The empty case, because the recursive CTE seeds itself from
    /// `min(invariant_id)` — which is NULL on an empty table. A seed that is
    /// NULL must terminate the recursion rather than loop or error, and
    /// "nothing has ever run" must come back as an empty list rather than as a
    /// failure the caller renders as `unwrap_or_default()` silence.
    #[tokio::test]
    async fn latest_per_invariant_is_empty_and_ok_on_an_empty_table() {
        let (s, _d) = store();
        assert_eq!(latest_per_invariant(&s).unwrap().len(), 0);
    }

    /// THE dedupe property: the same failure 100 times is ONE incident.
    /// Without this the health surface becomes a scrolling wall and the second
    /// distinct failure is invisible — which defeats the point of having it.
    #[tokio::test]
    async fn repeated_failure_is_one_incident_with_a_count() {
        let (s, _d) = store();
        for _ in 0..100 {
            record(
                &s,
                vec![InvariantResult::fail("x.check", "a", "b").entity("w1")],
                1,
            )
            .await;
        }
        let inc = live_incidents(&s).unwrap();
        assert_eq!(inc.len(), 1, "100 identical failures must be ONE incident");
        assert_eq!(inc[0]["occurrences"], 100);
    }

    /// `record` RETURNS the number of newly opened incidents, and nothing
    /// asserted it (AMUX-4750). `monitor.rs` is the one consumer and pages on
    /// it, so a wrong count is a wrong page.
    ///
    /// This became load-bearing when the separate `SELECT occurrences` after
    /// the upsert was replaced with `RETURNING occurrences`: the read is
    /// `.unwrap_or(1)`, and 1 is exactly the value that means "newly opened".
    /// If the RETURNING ever stopped yielding a row, every repeat failure in
    /// the fleet would report as brand new and nothing else in this file would
    /// notice — the suite was fully green with the count untested.
    #[tokio::test]
    async fn the_opened_count_is_one_only_the_first_time_each_incident_appears() {
        let (s, _d) = store();
        let fail = || vec![InvariantResult::fail("x.count", "a", "b").entity("w1")];

        assert_eq!(
            record(&s, fail(), 1).await,
            1,
            "the first failure opens an incident"
        );
        // THE LEG THAT CATCHES A DEAD RETURNING: the fallback value is 1.
        assert_eq!(
            record(&s, fail(), 1).await,
            0,
            "a repeat failure opens nothing"
        );
        assert_eq!(record(&s, fail(), 1).await, 0);

        // A different entity is a different incident, so it opens.
        assert_eq!(
            record(
                &s,
                vec![InvariantResult::fail("x.count", "a", "b").entity("w2")],
                1
            )
            .await,
            1
        );
        // A pass opens nothing at all.
        assert_eq!(
            record(&s, vec![InvariantResult::pass("x.count").entity("w1")], 1).await,
            0
        );
        // And a REOPEN is not an open: the row already exists with a count
        // above 1. Preserved behaviour, asserted here because the rewrite could
        // silently have changed it.
        assert_eq!(
            record(&s, fail(), 1).await,
            0,
            "a reopened incident is not a new one"
        );
        assert_eq!(live_incidents(&s).unwrap().len(), 2);
    }

    /// Two entities failing the same check are two incidents — collapsing them
    /// on invariant_id alone would hide the second worker completely.
    #[tokio::test]
    async fn distinct_entities_are_distinct_incidents() {
        let (s, _d) = store();
        record(
            &s,
            vec![InvariantResult::fail("x", "a", "b").entity("w1")],
            1,
        )
        .await;
        record(
            &s,
            vec![InvariantResult::fail("x", "a", "b").entity("w2")],
            1,
        )
        .await;
        assert_eq!(live_incidents(&s).unwrap().len(), 2);
    }

    /// A pass closes the incident, and a later failure REOPENS the same row so
    /// the flap is one story rather than two unrelated ones.
    #[tokio::test]
    async fn pass_resolves_and_a_later_failure_reopens() {
        let (s, _d) = store();
        record(
            &s,
            vec![InvariantResult::fail("x", "a", "b").entity("w1")],
            1,
        )
        .await;
        assert_eq!(live_incidents(&s).unwrap().len(), 1);

        record(&s, vec![InvariantResult::pass("x").entity("w1")], 1).await;
        assert_eq!(live_incidents(&s).unwrap().len(), 0, "pass must resolve it");

        record(
            &s,
            vec![InvariantResult::fail("x", "a", "b").entity("w1")],
            1,
        )
        .await;
        let inc = live_incidents(&s).unwrap();
        assert_eq!(inc.len(), 1, "a re-failure must reopen, not stay closed");
        assert_eq!(
            inc[0]["occurrences"], 2,
            "history is preserved across the flap"
        );
    }

    /// Unknown must NOT open an incident (it is a gap in observation, not a
    /// proven contradiction) but must still be recorded so it is visible.
    #[tokio::test]
    async fn unknown_records_but_does_not_page() {
        let (s, _d) = store();
        record(
            &s,
            vec![InvariantResult::unknown("x", "probe down").entity("w1")],
            1,
        )
        .await;
        assert_eq!(
            live_incidents(&s).unwrap().len(),
            0,
            "unknown must not open an incident"
        );
        let latest = latest_per_invariant(&s).unwrap();
        assert_eq!(latest.len(), 1, "...but it must still be recorded");
        assert_eq!(latest[0]["status"], "unknown");
    }

    /// AMUX-3489: the differential trim. A 2h-old PASS row (past the 1h pass
    /// window, inside the 7d fail window) must age out on the next write
    /// cycle; an equally old FAIL row must survive the week. Without the
    /// differential this table held 8M rows of green heartbeats.
    #[tokio::test]
    async fn old_pass_rows_trim_while_equally_old_fail_rows_survive() {
        let (s, _d) = store();
        let old = now() - 7200.0;
        let _ = s
            .write_async(move |conn| {
                for (st, id) in [("pass", "old.pass"), ("fail", "old.fail")] {
                    conn.execute(
                        "INSERT INTO _amux_invariant_result
                           (ts, invariant_id, status, entity_key, expected, observed, evidence, duration_ms)
                         VALUES (?1, ?2, ?3, '', '', '', '{}', 1)",
                        rusqlite::params![old, id, st],
                    )?;
                }
                Ok(WriteOutcome { applied: true, events: vec![] })
            })
            .await;
        // Any record() triggers the opportunistic trim.
        record(&s, vec![InvariantResult::pass("fresh.check")], 1).await;

        let conn = s.read().unwrap();
        let mut stmt = conn
            .prepare("SELECT invariant_id FROM _amux_invariant_result ORDER BY invariant_id")
            .unwrap();
        let left: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(
            left,
            vec!["fresh.check".to_string(), "old.fail".to_string()],
            "stale pass gone, stale fail kept, fresh row kept"
        );

        let (rows, oldest) = result_log_stats(&s).unwrap();
        assert_eq!(rows, 2);
        assert!(
            oldest >= 7000.0,
            "oldest_age_s must reflect the surviving fail row, got {oldest}"
        );
    }
}
