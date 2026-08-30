//! RR-0109 — `amux why`: provenance over the durable trails.
//!
//! The property under test is NOT "the endpoint returns a story". It is that
//! the story is checkable and that its absence is stated:
//!
//! - every timeline line names the table and row it came from,
//! - every consulted source is listed WITH ITS PREDICATE even when it matched
//!   nothing, because a zero from a probe that could have matched and a zero
//!   from a probe that never could look identical otherwise,
//! - `verdict` reaches `cannot_tell` when the evidence does not support an
//!   answer (ethos rule 4: an instrument that cannot express "I don't know"
//!   will confabulate, and a confident wrong answer is the expensive kind).

use amux_server::api::{router, AppState};
use amux_server::db::{Store, WriteOutcome};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::sync::Arc;
use tower::ServiceExt;

fn app() -> (axum::Router, Arc<Store>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("amux-test.db")).unwrap());
    let state = AppState {
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    };
    (router(state), store, dir)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let req = Request::builder().method("GET").uri(path).body(Body::empty()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn sources(v: &Value) -> Vec<&str> {
    v["sources"].as_array().unwrap().iter().map(|s| s["table"].as_str().unwrap()).collect()
}

fn kinds(v: &Value) -> Vec<&str> {
    v["timeline"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_card_with_history_gets_a_cited_timeline() {
    let (app, store, _d) = app();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO issues (id, title, desc, status, session, creator, created, updated, log)
                 VALUES ('AM-1','fix the thing','because it broke','doing','lane-a','lane-a',
                         1785000000, 1785000500, '`09:00` created\n`09:12` a: todo -> doing\n')",
                [],
            )?;
            // A request that touched the card, with its attribution.
            conn.execute(
                "INSERT INTO _amux_request_log (ts, method, path, family, status, latency_ms, amux_session, answered_by)
                 VALUES (1785000500.0, 'PATCH', '/api/board/AM-1', 'board', 200, 4.2, 'lane-a', 'native')",
                [],
            )?;
            // Two journal events with payloads, so a transition can be NAMED.
            conn.execute("UPDATE _amux_rev SET rev = 7 WHERE id = 1", [])?;
            conn.execute(
                "INSERT INTO _amux_state_events (rev, entity_type, entity_id, mutation, at, payload)
                 VALUES (6,'task','AM-1','{\"kind\":\"created\"}','2026-08-09T09:00:00+00:00','{\"id\":\"AM-1\",\"status\":\"todo\"}'),
                        (7,'task','AM-1','{\"kind\":\"updated\"}','2026-08-09T09:12:00+00:00','{\"id\":\"AM-1\",\"status\":\"doing\"}')",
                [],
            )?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();

    let (st, v) = get(&app, "/api/why/task/AM-1").await;
    assert_eq!(st, StatusCode::OK);
    assert!(v["found"].as_bool().unwrap(), "{v}");
    assert_eq!(v["subject"]["title"], "fix the thing");

    // Every trail that could speak, did.
    for t in ["issues", "issues.log", "_amux_state_events", "_amux_request_log", "_amux_turns", "interaction_log"] {
        assert!(sources(&v).contains(&t), "source {t} must be listed: {:?}", sources(&v));
    }
    let ks = kinds(&v);
    for k in ["entity", "card_log", "state_event", "request"] {
        assert!(ks.contains(&k), "timeline must include a {k} line: {ks:?}");
    }

    // The claim that makes this an instrument and not a narrator: each line
    // cites a table so it can be re-checked with one SELECT.
    for e in v["timeline"].as_array().unwrap() {
        assert!(
            e["source"]["table"].as_str().map(|t| !t.is_empty()).unwrap_or(false),
            "every timeline line must cite its source table: {e}"
        );
    }

    // The transition is NAMED from the payload snapshots, not merely reported
    // as "it changed".
    // Name the target: the STATE_EVENT line, not merely "a line mentioning
    // status". The `issues.updated` line also says the word, and matching it
    // would have passed while proving nothing about payload diffing.
    let transition = v["timeline"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "state_event" && e["summary"].as_str().unwrap_or("").contains("status:"))
        .expect("a status transition must be named from the journal payloads");
    assert!(
        transition["summary"].as_str().unwrap().contains("todo") && transition["summary"].as_str().unwrap().contains("doing"),
        "{transition}"
    );

    // Attribution comes from the request log, not from a guess.
    let req = v["timeline"].as_array().unwrap().iter().find(|e| e["kind"] == "request").unwrap();
    assert_eq!(req["actor"], "lane-a", "{req}");
}

#[tokio::test]
async fn card_log_lines_are_not_given_invented_timestamps() {
    // `issues.log` stores HH:MM with no date. Placing those lines on the
    // timeline with a fabricated epoch would make an old line look recent —
    // exactly the confident-wrong ordering this endpoint exists to prevent.
    let (app, store, _d) = app();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO issues (id, title, desc, status, creator, created, updated, log)
                 VALUES ('AM-2','t','d','todo','x',1785000000,1785000000,'`09:00` created\n`23:59` late line\n')",
                [],
            )?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_, v) = get(&app, "/api/why/task/AM-2").await;
    let log_lines: Vec<&Value> = v["timeline"].as_array().unwrap().iter().filter(|e| e["kind"] == "card_log").collect();
    assert_eq!(log_lines.len(), 2, "{v}");
    for l in &log_lines {
        assert!(l["at_epoch"].is_null(), "a dateless log line must not carry an epoch: {l}");
        assert_eq!(l["ordering"], "append-order");
    }
    // …and the limitation is stated where a reader will see it.
    let gaps = v["gaps"].as_array().unwrap();
    assert!(
        gaps.iter().any(|g| g.as_str().unwrap().contains("HH:MM")),
        "the missing-date limitation must be named in gaps: {gaps:?}"
    );
    // Undated lines sort to the END, after everything with a real time.
    let tl = v["timeline"].as_array().unwrap();
    let first_undated = tl.iter().position(|e| e["at_epoch"].is_null()).unwrap();
    assert!(
        tl[first_undated..].iter().all(|e| e["at_epoch"].is_null()),
        "undated lines must not be interleaved with timestamped ones"
    );
}

#[tokio::test]
async fn a_subject_that_does_not_exist_says_cannot_tell_and_names_the_table() {
    let (app, _s, _d) = app();
    let (st, v) = get(&app, "/api/why/task/NOPE-1").await;
    assert_eq!(st, StatusCode::OK);
    assert!(!v["found"].as_bool().unwrap());
    assert_eq!(v["verdict"], "cannot_tell", "{v}");
    assert!(sources(&v).contains(&"issues"));
    assert!(
        v["gaps"].as_array().unwrap().iter().any(|g| g.as_str().unwrap().contains("issues")),
        "the gap must name where it looked: {v}"
    );
}

#[tokio::test]
async fn a_source_that_matched_nothing_is_still_reported_with_its_predicate() {
    // The whole point: a source that returned zero has to be visible, or a
    // reader cannot tell "nothing happened" from "this trail was never
    // consulted".
    let (app, store, _d) = app();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO issues (id, title, desc, status, creator, created, updated)
                 VALUES ('AM-3','lonely card','',  'todo','x',1785000000,1785000000)",
                [],
            )?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_, v) = get(&app, "/api/why/task/AM-3").await;
    let turns = v["sources"].as_array().unwrap().iter().find(|s| s["table"] == "_amux_turns").unwrap();
    assert_eq!(turns["rows"], 0);
    assert!(turns["query"].as_str().unwrap().contains("AM-3"), "the predicate must be published: {turns}");
    assert!(turns["note"].as_str().is_some(), "an empty source must explain itself: {turns}");
    // Some trail spoke (the issues row itself), so the verdict is partial,
    // not cannot_tell — the distinction is the point.
    assert_eq!(v["verdict"], "partial", "{v}");
}

#[tokio::test]
async fn schedule_runs_name_their_source_so_a_manual_fire_is_not_a_cron_fire() {
    // The incident behind ethos rule 4: a hand-pressed Run-now and a cron fire
    // were byte-identical rows, so a reporting session concluded the scheduler
    // was re-firing. `source` is the discriminator; `why` must surface it.
    let (app, store, _d) = app();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO schedules (id, title, session, command, created, updated, schedule_expr)
                 VALUES ('SCHED-9','nightly sweep','lane-a','do the thing',1785000000,1785000000,'daily at 9am')",
                [],
            )?;
            conn.execute(
                "INSERT INTO schedule_runs (schedule_id, ran_at, status, note, source)
                 VALUES ('SCHED-9', 1785000100, 'ok', NULL, 'cron'),
                        ('SCHED-9', 1785000200, 'ok', NULL, 'manual')",
                [],
            )?;
            conn.execute(
                "INSERT INTO schedule_audit (schedule_id, ts, field, old_value, new_value, source, by_who)
                 VALUES ('SCHED-9', 1785000300, 'enabled', '1', '0', 'api', 'lane-b')",
                [],
            )?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();

    let (_, v) = get(&app, "/api/why/schedule/SCHED-9").await;
    assert!(v["found"].as_bool().unwrap(), "{v}");
    let runs: Vec<&Value> = v["timeline"].as_array().unwrap().iter().filter(|e| e["kind"] == "schedule_run").collect();
    assert_eq!(runs.len(), 2);
    let actors: Vec<&str> = runs.iter().map(|r| r["actor"].as_str().unwrap()).collect();
    assert!(actors.contains(&"cron") && actors.contains(&"manual"), "{actors:?}");
    // The audit row carries WHO, which is the other half of "why".
    let audit = v["timeline"].as_array().unwrap().iter().find(|e| e["kind"] == "schedule_audit").unwrap();
    assert_eq!(audit["actor"], "lane-b", "{audit}");
    assert!(audit["summary"].as_str().unwrap().contains("enabled: 1 -> 0"), "{audit}");
}

#[tokio::test]
async fn a_deleted_schedule_still_explains_itself_from_its_runs() {
    // "Not found" must not end the investigation when the child rows survive.
    let (app, store, _d) = app();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO schedule_runs (schedule_id, ran_at, status, note, source)
                 VALUES ('SCHED-GONE', 1785000100, 'error', 'session was not running', 'cron')",
                [],
            )?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_, v) = get(&app, "/api/why/schedule/SCHED-GONE").await;
    assert!(!v["found"].as_bool().unwrap());
    let runs: Vec<&Value> = v["timeline"].as_array().unwrap().iter().filter(|e| e["kind"] == "schedule_run").collect();
    assert_eq!(runs.len(), 1, "the surviving run rows must still be reported: {v}");
    assert!(runs[0]["summary"].as_str().unwrap().contains("session was not running"));
}

#[tokio::test]
async fn worker_resolves_by_name_and_by_alias() {
    let (app, store, _d) = app();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO _amux_workers (id, display_name, name_aliases, cwd, provider, backend, created_at, updated_at)
                 VALUES ('wrk_1','backend','[\"old-backend\"]','/tmp','claude','tmux',
                         '2026-08-09T00:00:00+00:00','2026-08-09T00:00:00+00:00')",
                [],
            )?;
            conn.execute(
                "INSERT INTO _amux_sessions (id, worker_id, backend, backend_ref, pid, started_at)
                 VALUES ('ses_1','wrk_1','tmux','amux-wrk_1',4242,'2026-08-09T00:01:00+00:00')",
                [],
            )?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();

    // The spec's own example is a NAME: `amux why worker backend`.
    let (_, by_name) = get(&app, "/api/why/worker/backend").await;
    assert!(by_name["found"].as_bool().unwrap(), "{by_name}");
    assert_eq!(by_name["subject"]["id"], "wrk_1");
    // A rename must not orphan the history.
    let (_, by_alias) = get(&app, "/api/why/worker/old-backend").await;
    assert_eq!(by_alias["subject"]["id"], "wrk_1", "{by_alias}");
    let (_, by_id) = get(&app, "/api/why/worker/wrk_1").await;
    assert_eq!(by_id["subject"]["display_name"], "backend");
    assert!(kinds(&by_id).contains(&"session"), "{by_id}");
}

#[tokio::test]
async fn an_unknown_worker_names_what_it_searched() {
    let (app, _s, _d) = app();
    let (_, v) = get(&app, "/api/why/worker/ghost").await;
    assert_eq!(v["verdict"], "cannot_tell");
    let gap = v["gaps"].as_array().unwrap()[0].as_str().unwrap();
    assert!(gap.contains("id") && gap.contains("alias"), "the gap must say how it looked: {gap}");
}

#[tokio::test]
async fn an_integration_with_no_durable_trail_says_so_instead_of_narrating() {
    // The honest answer here is "amux keeps no integrations registry" — an
    // explainer that instead produced a plausible sequence from adjacent data
    // would be worse than useless.
    let (app, _s, _d) = app();
    let (st, v) = get(&app, "/api/why/integration/gmail").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["verdict"], "cannot_tell", "{v}");
    assert!(v["timeline"].as_array().unwrap().is_empty());
    let gaps = v["gaps"].as_array().unwrap();
    assert!(
        gaps.iter().any(|g| g.as_str().unwrap().contains("no integrations registry")),
        "the missing SUBSTRATE must be named, not just the empty result: {gaps:?}"
    );
    // Both trails it consulted are listed, so the claim is checkable.
    assert!(sources(&v).contains(&"_amux_request_log"));
    assert!(sources(&v).contains(&"email_events"));
}

#[tokio::test]
async fn an_integration_with_request_traffic_is_explained_from_it() {
    let (app, store, _d) = app();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO _amux_request_log (ts, method, path, family, status, latency_ms, amux_session, answered_by, error_body)
                 VALUES (1785000100.0,'POST','/api/gmail/send','gmail',500,12.0,'lane-a','native','token expired')",
                [],
            )?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_, v) = get(&app, "/api/why/integration/gmail").await;
    assert!(v["found"].as_bool().unwrap(), "{v}");
    let e = &v["timeline"].as_array().unwrap()[0];
    assert!(e["summary"].as_str().unwrap().contains("token expired"), "{e}");
    assert_eq!(e["actor"], "lane-a");
}

#[tokio::test]
async fn a_session_with_no_attributed_writes_says_the_attribution_is_missing() {
    let (app, _s, _d) = app();
    let (_, v) = get(&app, "/api/why/session/nobody").await;
    assert_eq!(v["verdict"], "cannot_tell");
    let gaps = v["gaps"].as_array().unwrap();
    assert!(
        gaps.iter().any(|g| g.as_str().unwrap().contains("X-Amux-Session")),
        "an unattributed write is invisible here BY CONSTRUCTION and that has to be said: {gaps:?}"
    );
}

#[tokio::test]
async fn window_mode_reports_failures_and_says_it_omitted_the_successes() {
    let (app, store, _d) = app();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO _amux_request_log (ts, method, path, family, status, latency_ms, answered_by, error_body)
                 VALUES (1785000100.0,'POST','/api/board','board',500,3.0,'native','boom'),
                        (1785000110.0,'GET','/api/board','board',200,3.0,'native',NULL)",
                [],
            )?;
            conn.execute(
                "INSERT INTO schedule_runs (schedule_id, ran_at, status, note, source)
                 VALUES ('SCHED-1', 1785000120, 'error', 'no such session', 'cron')",
                [],
            )?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();

    let (st, v) = get(&app, "/api/why?since=1785000000&until=1785000200").await;
    assert_eq!(st, StatusCode::OK);
    assert!(v["found"].as_bool().unwrap(), "{v}");
    let summaries: Vec<&str> = v["timeline"].as_array().unwrap().iter().map(|e| e["summary"].as_str().unwrap()).collect();
    assert!(summaries.iter().any(|s| s.contains("boom")), "{summaries:?}");
    assert!(summaries.iter().any(|s| s.contains("no such session")), "{summaries:?}");
    assert!(!summaries.iter().any(|s| s.contains("-> 200")), "successes are deliberately omitted: {summaries:?}");
    // …and the omission announces itself rather than being silent.
    let probe = v["sources"].as_array().unwrap().iter().find(|s| s["query"].as_str().unwrap().contains("FAILURES ONLY")).unwrap();
    assert!(probe["note"].as_str().unwrap().contains("deliberately not listed"), "{probe}");
}

#[tokio::test]
async fn an_empty_window_says_the_window_may_predate_the_trails() {
    let (app, _s, _d) = app();
    let (_, v) = get(&app, "/api/why?since=1000000000&until=1000000100").await;
    assert_eq!(v["verdict"], "cannot_tell", "{v}");
    assert!(
        v["gaps"].as_array().unwrap()[0].as_str().unwrap().contains("predate"),
        "an empty window must distinguish 'nothing happened' from 'we do not have that far back': {v}"
    );
}

#[tokio::test]
async fn an_unknown_subject_kind_is_a_400_that_lists_the_kinds() {
    let (app, _s, _d) = app();
    let (st, v) = get(&app, "/api/why/banana/x").await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v["kinds"].as_array().unwrap().len() >= 6, "{v}");
}

#[tokio::test]
async fn the_contract_lists_every_kind_the_router_actually_answers() {
    // A contract that drifts from the dispatch is worse than none: it is read
    // first and trusted. Each advertised kind is exercised against the router.
    let (app, _s, _d) = app();
    let (st, c) = get(&app, "/api/why/contract").await;
    assert_eq!(st, StatusCode::OK);
    let kinds: Vec<String> = c["kinds"].as_object().unwrap().keys().cloned().collect();
    assert!(!kinds.is_empty());
    for k in kinds {
        let (st, v) = get(&app, &format!("/api/why/{k}/probe-id")).await;
        assert_eq!(st, StatusCode::OK, "contract advertises `{k}` but the router rejects it: {v}");
        assert!(v["verdict"].is_string(), "{v}");
    }
}

#[tokio::test]
async fn payloadless_journal_events_are_reported_as_unreconstructable() {
    // Pre-0008 rows record THAT an entity changed and not into what. Saying
    // "updated" and stopping is honest; inferring the new state is not.
    let (app, store, _d) = app();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO issues (id, title, desc, status, creator, created, updated)
                 VALUES ('AM-4','t','d','todo','x',1785000000,1785000000)",
                [],
            )?;
            conn.execute(
                "INSERT INTO _amux_state_events (rev, entity_type, entity_id, mutation, at, payload)
                 VALUES (1,'task','AM-4','{\"kind\":\"updated\"}','2026-08-09T09:00:00+00:00',NULL)",
                [],
            )?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_, v) = get(&app, "/api/why/task/AM-4").await;
    let ev = v["timeline"].as_array().unwrap().iter().find(|e| e["kind"] == "state_event").unwrap();
    assert!(ev["summary"].as_str().unwrap().contains("no snapshot recorded"), "{ev}");
    assert!(
        v["gaps"].as_array().unwrap().iter().any(|g| g.as_str().unwrap().contains("post-mutation snapshot")),
        "{v}"
    );
}

/// The retention horizon, which is the difference between "this card's history"
/// and "the surviving tail of it" — two answers with identical shapes.
///
/// `_amux_state_events` is reaped (14d default, `AMUX_STATE_EVENTS_RETAIN_DAYS`),
/// so its floor is a MOVING horizon. The zero-row arm always said so. Every
/// other arm treated the rows it got as the whole story, which for anything
/// older than the window is false in the most misleading available way: the
/// trail renders, it is internally consistent, and it silently begins partway
/// through.
///
/// Both directions are asserted in ONE test on ONE database, deliberately. A
/// test that only pinned the old card would pass just as well against a version
/// that flags EVERY card, which would make the caveat noise and teach readers to
/// skip it — and a caveat nobody reads is the same defect one layer along.
#[tokio::test]
async fn a_card_older_than_the_journal_floor_is_told_its_trail_is_a_tail() {
    let (app, store, _d) = app();
    store
        .write(|conn| {
            // The journal floor for this database is 2026-08-09T09:00:00Z.
            // OLD was created 2026-07-25; NEW on 2026-08-20. Same journal, same
            // number of events each, so the only thing that can separate them is
            // the created-vs-floor comparison.
            conn.execute(
                "INSERT INTO issues (id, title, desc, status, creator, created, updated) VALUES
                   ('AM-OLD','older than the journal','d','done','x',1784937600,1786000000),
                   ('AM-NEW','born inside the window','d','done','x',1787000000,1787000100)",
                [],
            )?;
            conn.execute(
                "INSERT INTO _amux_state_events (rev, entity_type, entity_id, mutation, at, payload) VALUES
                   (1,'task','AM-OLD','{\"kind\":\"updated\"}','2026-08-09T09:00:00+00:00','{\"id\":\"AM-OLD\",\"status\":\"done\"}'),
                   (2,'task','AM-NEW','{\"kind\":\"updated\"}','2026-08-21T09:00:00+00:00','{\"id\":\"AM-NEW\",\"status\":\"done\"}')",
                [],
            )?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();

    let journal_note = |v: &Value| -> String {
        v["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["table"] == "_amux_state_events")
            .and_then(|s| s["note"].as_str())
            .unwrap_or("")
            .to_string()
    };
    let gaps_text = |v: &Value| -> String {
        v["gaps"].as_array().unwrap().iter().filter_map(|g| g.as_str()).collect::<Vec<_>>().join(" | ")
    };

    // OLD: predates the floor. It has an event, so the old zero-row arm never
    // fired; this is exactly the row count that used to read as complete.
    let (_, old) = get(&app, "/api/why/task/AM-OLD").await;
    let n = old["sources"].as_array().unwrap().iter()
        .find(|s| s["table"] == "_amux_state_events").unwrap()["rows"].as_i64().unwrap();
    assert_eq!(n, 1, "the fixture must produce a NON-empty journal probe, or this pins the zero-row arm that was already correct: {old}");
    assert!(
        journal_note(&old).contains("surviving TAIL"),
        "a card predating the journal floor must be told its trail is truncated, not handed 1 event as its history: {}",
        journal_note(&old)
    );
    assert!(
        gaps_text(&old).contains("surviving TAIL"),
        "and truncation is a GAP, not merely a footnote on the probe: {}",
        gaps_text(&old)
    );
    assert!(
        journal_note(&old).contains("2026-08-09"),
        "the floor must be given as a DATE the reader can hold against the card, not only as a rev: {}",
        journal_note(&old)
    );

    // NEW: born inside the window. Must NOT be flagged — and must get the
    // receipt, because a reader who has learned to distrust the horizon needs a
    // way to tell a complete trail from a truncated one.
    let (_, new) = get(&app, "/api/why/task/AM-NEW").await;
    assert!(
        !journal_note(&new).contains("surviving TAIL"),
        "a card born inside the retained window must NOT be flagged as truncated: {}",
        journal_note(&new)
    );
    assert!(
        !gaps_text(&new).contains("surviving TAIL"),
        "and it must not carry a truncation gap: {}",
        gaps_text(&new)
    );
    assert!(
        journal_note(&new).contains("history is complete"),
        "it should say so positively — otherwise the fix makes every trail equally suspect: {}",
        journal_note(&new)
    );
}
