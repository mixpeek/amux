//! RR-0111a — replay-capable event log, AUDIT slice (Invariants 24, 30).
//!
//! The `_amux_state_events` journal always recorded THAT something changed
//! (rev, entity, mutation kind); since migration 0008 each event can also
//! carry the POST-MUTATION snapshot of its entity row (`payload`, written in
//! the same transaction — see `PendingEvent.payload` in `db/mod.rs`). This
//! module folds that journal back into state and checks the fold against the
//! live tables: the plan's **audit replay** mode ("read-only, verify that
//! events reproduce the recorded state transitions" — a744dde). Debug/fork
//! replay and the per-turn capture contract need turn instrumentation that
//! does not exist yet; they layer on top of this journal when it does.
//!
//! Honesty rules, in order of how expensively they were learned:
//!
//! - **Never pretend to reconstruct what was not recorded.** An event without
//!   a payload leaves its entity's state UNKNOWN from that rev on — replay
//!   reports it under `pre_payload_horizon` with the first rev from which a
//!   full replay is possible, instead of passing off an older snapshot as
//!   current (ethos rule 7: a check that cannot fail is theatre; a replay
//!   that fabricates is a check that cannot fail).
//! - **Divergences are named individually** — entity id, differing fields,
//!   both values — never rolled into a count (ethos rule 4: the discriminator
//!   belongs where the reader already looks).
//! - **Caps announce themselves.** The report's lists are capped for the API
//!   response, but totals are always exact and `lists_capped_at` says what
//!   was cut (Invariant 40: an omission must announce itself).

use crate::db::board_store::{self, ArchivedFilter};
use crate::db::queries;
use amux_core::revision::MutationKind;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// Entity type tags whose events carry post-mutation snapshots today:
/// workers (`api/workers.rs`) and board cards (`api/board.rs`). Extend this
/// WITH the writing sites — a type listed here without a populating site
/// would report its whole history as horizon, and a populating site not
/// listed here would never be verified.
pub const PAYLOAD_BEARING: &[&str] = &["worker", "task"];

/// Cap on each NAMED list in the API-facing report. Totals are always exact.
const LIST_CAP: usize = 200;

// ---------------------------------------------------------------------------
// Journal parsing
// ---------------------------------------------------------------------------

/// Canonical tag for the stored `entity_type` column.
///
/// The writer serializes `EntityType` with serde's adjacent tagging, so the
/// column actually holds `{"kind":"worker"}` / `{"kind":"other","data":"x"}`
/// (the `trim_matches('"')` in `apply_write` only strips quotes from a shape
/// serde never produces for this enum). This helper reads BOTH that shape and
/// a bare tag, so replay keeps working if the storage format is ever fixed to
/// the bare spelling. It deliberately does not guess beyond those two.
fn entity_tag(raw: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(raw) {
        if let Some(kind) = v.get("kind").and_then(Value::as_str) {
            if kind == "other" {
                if let Some(data) = v.get("data").and_then(Value::as_str) {
                    return data.to_string();
                }
            }
            return kind.to_string();
        }
        // Valid JSON but not the tagged-object shape (e.g. a bare JSON
        // string, which is how a fixed writer would store it).
        if let Some(s) = v.as_str() {
            return s.to_string();
        }
    }
    raw.to_string()
}

// ---------------------------------------------------------------------------
// Replayed state
// ---------------------------------------------------------------------------

/// One entity as the journal reproduces it at the requested rev.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReplayedEntity {
    pub entity_type: String,
    pub entity_id: String,
    /// Rev of the last event folded into this entity.
    pub last_rev: u64,
    /// The post-mutation snapshot of the LAST folded event, when that event
    /// carried one. `None` means the latest recorded mutation did not journal
    /// its resulting state: the entity's state is honestly UNKNOWN — an older
    /// snapshot is stale the moment an unrecorded mutation lands, so it is
    /// never passed off as current.
    pub state: Option<Value>,
    /// Whether the last folded event was a `Deleted` mutation.
    pub deleted: bool,
    pub events_folded: u64,
    pub payload_events: u64,
}

/// The stretch of journal that predates payload capture (migration 0008, or
/// an event site that does not journal snapshots yet). Everything before
/// `first_full_replay_rev` can be audited for WHAT changed but not replayed
/// into state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrePayloadHorizon {
    /// Payload-bearing-type events with no recorded payload.
    pub payloadless_events: u64,
    pub first_payloadless_rev: u64,
    pub last_payloadless_rev: u64,
    /// The first rev from which every payload-bearing event carries its
    /// snapshot — full replay is only possible from here forward.
    pub first_full_replay_rev: u64,
}

/// The journal folded into per-entity state, up to a rev.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayedState {
    pub up_to_rev: u64,
    /// Oldest event rev actually in the journal (`None` = empty journal).
    /// When this is > 1 the journal has been pruned or predated: mutations
    /// before it are not replayable AND not enumerable.
    pub first_event_rev: Option<u64>,
    /// entity_type tag -> entity_id -> replayed entity.
    pub entities: BTreeMap<String, BTreeMap<String, ReplayedEntity>>,
    /// Present iff any payload-bearing event in range lacks a payload.
    pub pre_payload_horizon: Option<PrePayloadHorizon>,
}

/// Fold the event journal into entity state at `up_to_rev` (inclusive).
///
/// Post-mutation snapshot semantics: the state after event N is exactly event
/// N's payload. An event without a payload therefore CLEARS the entity's
/// reconstructed state rather than leaving the previous snapshot in place —
/// the previous snapshot describes the row before a mutation the journal did
/// not capture, and reporting it as current would be fabrication.
pub fn replay_state(conn: &Connection, up_to_rev: u64) -> rusqlite::Result<ReplayedState> {
    let first_event_rev: Option<u64> = conn
        .query_row("SELECT MIN(rev) FROM _amux_state_events", [], |r| r.get(0))
        .unwrap_or(None);

    // rowid breaks ties inside a transaction: several events share one rev,
    // and insertion order is the only order the writer had.
    let mut stmt = conn.prepare(
        "SELECT rev, entity_type, entity_id, mutation, payload FROM _amux_state_events
         WHERE rev <= ?1 ORDER BY rev ASC, rowid ASC",
    )?;
    let rows = stmt.query_map([up_to_rev], |r| {
        Ok((
            r.get::<_, u64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;

    let mut entities: BTreeMap<String, BTreeMap<String, ReplayedEntity>> = BTreeMap::new();
    let mut payloadless_revs: Vec<u64> = Vec::new();
    for row in rows {
        let (rev, etype_raw, entity_id, mutation_raw, payload_raw) = row?;
        let etype = entity_tag(&etype_raw);
        let mutation: MutationKind =
            serde_json::from_str(&mutation_raw).unwrap_or(MutationKind::Updated);
        // An unparseable payload is treated as absent, not silently dropped
        // into a partial state: absence routes the entity into the horizon
        // report where a human sees it.
        let payload: Option<Value> = payload_raw
            .as_deref()
            .and_then(|p| serde_json::from_str(p).ok());
        if PAYLOAD_BEARING.contains(&etype.as_str()) && payload.is_none() {
            payloadless_revs.push(rev);
        }
        let ent = entities
            .entry(etype.clone())
            .or_default()
            .entry(entity_id.clone())
            .or_insert_with(|| ReplayedEntity {
                entity_type: etype.clone(),
                entity_id: entity_id.clone(),
                last_rev: 0,
                state: None,
                deleted: false,
                events_folded: 0,
                payload_events: 0,
            });
        ent.events_folded += 1;
        if payload.is_some() {
            ent.payload_events += 1;
        }
        ent.last_rev = rev;
        ent.deleted = matches!(mutation, MutationKind::Deleted);
        ent.state = payload;
    }

    let pre_payload_horizon = match (payloadless_revs.first(), payloadless_revs.last()) {
        (Some(&first), Some(&last)) => Some(PrePayloadHorizon {
            payloadless_events: payloadless_revs.len() as u64,
            first_payloadless_rev: first,
            last_payloadless_rev: last,
            first_full_replay_rev: last + 1,
        }),
        _ => None,
    };

    Ok(ReplayedState {
        up_to_rev,
        first_event_rev,
        entities,
        pre_payload_horizon,
    })
}

// ---------------------------------------------------------------------------
// Verification against the live tables
// ---------------------------------------------------------------------------

/// A field where the replayed snapshot and the live row disagree. `Null` on
/// either side means the key is absent there.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldDiff {
    pub field: String,
    pub replayed: Value,
    pub live: Value,
}

/// One named divergence between replayed state and a live table.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Divergence {
    /// Replay reproduces an entity the live table does not have.
    MissingLive {
        entity_type: String,
        entity_id: String,
        last_rev: u64,
    },
    /// The journal's last word is a snapshotless Deleted, but the live table
    /// still has the row.
    DeletedButLive {
        entity_type: String,
        entity_id: String,
        last_rev: u64,
    },
    /// Both sides have the entity and they disagree, field by field.
    FieldMismatch {
        entity_type: String,
        entity_id: String,
        last_rev: u64,
        fields: Vec<FieldDiff>,
    },
}

/// An entity replay knows about but cannot check, or a live row the journal
/// never saw. Named — never a bare count — so a wrong answer stays detectable
/// from the report alone (ethos rule 4).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EntityRef {
    pub entity_type: String,
    pub entity_id: String,
    /// Rev of the entity's last journal event; 0 for live rows with no
    /// journal events at all.
    pub last_rev: u64,
}

/// The audit-replay verdict: replayed state at HEAD vs the live tables.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    pub head_rev: u64,
    pub payload_bearing_types: Vec<String>,
    /// Entities compared snapshot-to-live (excludes horizon entities).
    pub entities_checked: u64,
    pub entities_matched: u64,
    pub divergences_total: u64,
    pub divergences: Vec<Divergence>,
    /// Present iff payload-bearing events without payloads exist.
    pub pre_payload_horizon: Option<PrePayloadHorizon>,
    /// Entities whose LATEST event predates payload capture: their replayed
    /// state is unknown, so they are reported here — not compared, and never
    /// counted as matched. This is the `pre_payload_horizon` verdict per
    /// entity: recorded as changed, unreconstructable by design.
    pub horizon_entities_total: u64,
    pub horizon_entities: Vec<EntityRef>,
    /// Live rows with no journal events at all (pre-journal history, a
    /// pruned journal, or a writer that bypassed the event path — the Python
    /// server writes these tables too). Named, never silently skipped;
    /// distinguishing which cause applies needs `first_event_rev`.
    pub live_not_in_journal_total: u64,
    pub live_not_in_journal: Vec<EntityRef>,
    /// Oldest rev still in the journal (see [`ReplayedState`]).
    pub first_event_rev: Option<u64>,
    /// The cap applied to the three lists above; totals are always exact.
    pub lists_capped_at: usize,
}

fn diff_fields(replayed: &Value, live: &Value) -> Vec<FieldDiff> {
    let (Some(a), Some(b)) = (replayed.as_object(), live.as_object()) else {
        // Non-object snapshots: compare wholesale under a root marker.
        return if replayed == live {
            Vec::new()
        } else {
            vec![FieldDiff {
                field: "<root>".into(),
                replayed: replayed.clone(),
                live: live.clone(),
            }]
        };
    };
    let mut keys: Vec<&String> = a.keys().chain(b.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| FieldDiff {
            field: k.clone(),
            replayed: a.get(k).cloned().unwrap_or(Value::Null),
            live: b.get(k).cloned().unwrap_or(Value::Null),
        })
        .collect()
}

fn push_capped<T>(list: &mut Vec<T>, item: T) {
    if list.len() < LIST_CAP {
        list.push(item);
    }
}

/// Audit replay at HEAD: fold the journal, compare payload-bearing entities
/// against their live tables, and name every disagreement individually.
pub fn verify_replay(conn: &Connection) -> rusqlite::Result<ReplayReport> {
    let head_rev: u64 =
        conn.query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))?;
    let replayed = replay_state(conn, head_rev)?;

    // Live snapshots, via the SAME serialization the event writers journal
    // (WorkerRow::snapshot / IssueRow::snapshot) — the comparison must share
    // its predicate with the mechanism it audits.
    let mut live: BTreeMap<&'static str, BTreeMap<String, Value>> = BTreeMap::new();
    live.insert(
        "worker",
        queries::all_workers_for_replay(conn)?
            .into_iter()
            .map(|w| (w.id.clone(), w.snapshot()))
            .collect(),
    );
    live.insert(
        "task",
        board_store::list_issues(conn, &[], &[], ArchivedFilter::All)?
            .into_iter()
            .map(|i| (i.id.clone(), i.snapshot()))
            .collect(),
    );

    let mut report = ReplayReport {
        head_rev,
        payload_bearing_types: PAYLOAD_BEARING.iter().map(|s| s.to_string()).collect(),
        entities_checked: 0,
        entities_matched: 0,
        divergences_total: 0,
        divergences: Vec::new(),
        pre_payload_horizon: replayed.pre_payload_horizon.clone(),
        horizon_entities_total: 0,
        horizon_entities: Vec::new(),
        live_not_in_journal_total: 0,
        live_not_in_journal: Vec::new(),
        first_event_rev: replayed.first_event_rev,
        lists_capped_at: LIST_CAP,
    };

    for &etype in PAYLOAD_BEARING {
        let replayed_ents = replayed.entities.get(etype);
        let live_ents = live.get(etype).expect("live map covers PAYLOAD_BEARING");

        if let Some(ents) = replayed_ents {
            for (eid, ent) in ents {
                let live_snap = live_ents.get(eid);
                if ent.state.is_none() && !ent.deleted {
                    // The journal's last word on this entity predates payload
                    // capture: state unknown. Report, do not compare, do not
                    // fabricate.
                    report.horizon_entities_total += 1;
                    push_capped(
                        &mut report.horizon_entities,
                        EntityRef {
                            entity_type: etype.to_string(),
                            entity_id: eid.clone(),
                            last_rev: ent.last_rev,
                        },
                    );
                    continue;
                }
                report.entities_checked += 1;
                let divergence = match (&ent.state, live_snap) {
                    (Some(snap), Some(live_snap)) => {
                        let fields = diff_fields(snap, live_snap);
                        if fields.is_empty() {
                            None
                        } else {
                            Some(Divergence::FieldMismatch {
                                entity_type: etype.to_string(),
                                entity_id: eid.clone(),
                                last_rev: ent.last_rev,
                                fields,
                            })
                        }
                    }
                    (Some(_), None) => Some(Divergence::MissingLive {
                        entity_type: etype.to_string(),
                        entity_id: eid.clone(),
                        last_rev: ent.last_rev,
                    }),
                    // Snapshotless Deleted: the journal expects the row gone.
                    (None, Some(_)) => Some(Divergence::DeletedButLive {
                        entity_type: etype.to_string(),
                        entity_id: eid.clone(),
                        last_rev: ent.last_rev,
                    }),
                    (None, None) => None, // deleted, and it is gone: consistent
                };
                match divergence {
                    Some(d) => {
                        report.divergences_total += 1;
                        push_capped(&mut report.divergences, d);
                    }
                    None => report.entities_matched += 1,
                }
            }
        }

        for eid in live_ents.keys() {
            let in_journal = replayed_ents.map(|m| m.contains_key(eid)).unwrap_or(false);
            if !in_journal {
                report.live_not_in_journal_total += 1;
                push_capped(
                    &mut report.live_not_in_journal,
                    EntityRef {
                        entity_type: etype.to_string(),
                        entity_id: eid.clone(),
                        last_rev: 0,
                    },
                );
            }
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Tests: fold semantics + horizon honesty on a raw journal
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrate;
    use serde_json::json;

    fn test_conn() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        migrate::apply_all(&mut c).unwrap();
        c
    }

    /// Insert a journal row exactly as the writer stores it (adjacent-tagged
    /// entity_type object, mutation JSON, optional payload), bumping the
    /// global rev like `apply_write` does.
    fn journal(
        conn: &Connection,
        etype: &str,
        eid: &str,
        mutation: &str,
        payload: Option<Value>,
    ) -> u64 {
        conn.execute("UPDATE _amux_rev SET rev = rev + 1 WHERE id = 1", [])
            .unwrap();
        let rev: u64 = conn
            .query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO _amux_state_events (rev, entity_type, entity_id, mutation, at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                rev,
                format!("{{\"kind\":\"{etype}\"}}"),
                eid,
                format!("{{\"kind\":\"{mutation}\"}}"),
                "2026-08-09T00:00:00+00:00",
                payload.map(|p| p.to_string()),
            ],
        )
        .unwrap();
        rev
    }

    #[test]
    fn entity_tag_reads_stored_and_bare_shapes() {
        // What the writer actually stores today:
        assert_eq!(entity_tag("{\"kind\":\"worker\"}"), "worker");
        assert_eq!(
            entity_tag("{\"kind\":\"other\",\"data\":\"probe\"}"),
            "probe"
        );
        // What a fixed writer would store:
        assert_eq!(entity_tag("worker"), "worker");
        assert_eq!(entity_tag("\"task\""), "task");
    }

    #[test]
    fn fold_keeps_last_snapshot_and_slices_by_rev() {
        let conn = test_conn();
        let r1 = journal(
            &conn,
            "task",
            "T-1",
            "created",
            Some(json!({"id": "T-1", "title": "a"})),
        );
        let r2 = journal(
            &conn,
            "task",
            "T-1",
            "updated",
            Some(json!({"id": "T-1", "title": "b"})),
        );

        // At HEAD: the last payload wins.
        let s = replay_state(&conn, r2).unwrap();
        let ent = &s.entities["task"]["T-1"];
        assert_eq!(ent.state, Some(json!({"id": "T-1", "title": "b"})));
        assert_eq!(ent.events_folded, 2);
        assert_eq!(ent.last_rev, r2);
        assert!(s.pre_payload_horizon.is_none());

        // Sliced at r1: replay reproduces the EARLIER state, which is the
        // whole point of a replayable journal.
        let s = replay_state(&conn, r1).unwrap();
        assert_eq!(
            s.entities["task"]["T-1"].state,
            Some(json!({"id": "T-1", "title": "a"}))
        );
    }

    #[test]
    fn payloadless_event_clears_state_and_reports_horizon() {
        let conn = test_conn();
        let r1 = journal(
            &conn,
            "task",
            "T-1",
            "created",
            Some(json!({"id": "T-1", "title": "a"})),
        );
        // A mutation the journal did not capture a snapshot for: the OLD
        // snapshot must not be passed off as the state after it.
        let r2 = journal(&conn, "task", "T-1", "updated", None);

        let s = replay_state(&conn, r2).unwrap();
        let ent = &s.entities["task"]["T-1"];
        assert_eq!(
            ent.state, None,
            "stale snapshot must not survive an uncaptured mutation"
        );
        let h = s.pre_payload_horizon.expect("horizon must be reported");
        assert_eq!(h.payloadless_events, 1);
        assert_eq!(h.first_payloadless_rev, r2);
        assert_eq!(h.first_full_replay_rev, r2 + 1);
        assert!(r1 < h.first_full_replay_rev);
    }

    #[test]
    fn non_payload_bearing_types_do_not_pollute_the_horizon() {
        let conn = test_conn();
        journal(&conn, "schedule", "SCHED-1", "updated", None);
        journal(&conn, "other", "probe-1", "updated", None);
        let head: u64 = conn
            .query_row("SELECT rev FROM _amux_rev WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        let s = replay_state(&conn, head).unwrap();
        assert!(
            s.pre_payload_horizon.is_none(),
            "types that never journal payloads are not a replay gap"
        );
        // They are still FOLDED (the journal knows they changed), just not
        // reconstructable — and not claimed to be.
        assert_eq!(s.entities["schedule"]["SCHED-1"].state, None);
    }

    #[test]
    fn verify_names_horizon_entities_instead_of_comparing_them() {
        let conn = test_conn();
        // A payload-bearing event with no payload and no live row: the
        // pre-0008 shape. It must surface as horizon, never as a divergence
        // and never as a silent skip.
        let r = journal(&conn, "task", "GHOST-1", "created", None);
        let report = verify_replay(&conn).unwrap();
        assert_eq!(report.entities_checked, 0);
        assert_eq!(report.divergences_total, 0, "{:?}", report.divergences);
        assert_eq!(report.horizon_entities_total, 1);
        assert_eq!(report.horizon_entities[0].entity_id, "GHOST-1");
        assert_eq!(report.horizon_entities[0].last_rev, r);
        let h = report.pre_payload_horizon.expect("horizon block present");
        assert_eq!(h.first_full_replay_rev, r + 1);
    }

    #[test]
    fn verify_names_live_rows_the_journal_never_saw() {
        let conn = test_conn();
        // A row written with no journal event (Python writes, pre-journal
        // history, pruning). It must be NAMED, not silently skipped and not
        // called a divergence.
        conn.execute(
            "INSERT INTO issues (id, title, status, created, updated) VALUES ('PY-1','t','todo',1,1)",
            [],
        )
        .unwrap();
        let report = verify_replay(&conn).unwrap();
        assert_eq!(report.live_not_in_journal_total, 1);
        assert_eq!(report.live_not_in_journal[0].entity_id, "PY-1");
        assert_eq!(report.divergences_total, 0);
    }
}
