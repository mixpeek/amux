//! RR-0110 — universal search (FTS5): index maintenance and ranking.
//!
//! These tests write to the source tables with RAW SQL rather than through the
//! board/memory APIs, on purpose. The design claim of migration 0013 is that
//! the index is maintained by SQLite triggers and therefore cannot be bypassed
//! by a writer that forgets to index — a test that went through the API would
//! pass equally well against a Rust write-hook implementation, so it could not
//! tell the two apart and could not fail on the thing being claimed.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
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
    reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), store, dir)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let req = Request::builder().method("GET").uri(path).body(Body::empty()).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

/// Insert a board card straight into `issues`. `log` is the card's history
/// column (`` `HH:MM` text `` lines).
fn card(store: &Store, id: &str, title: &str, desc: &str, log: Option<&str>) {
    let (id, title, desc, log) = (
        id.to_string(),
        title.to_string(),
        desc.to_string(),
        log.map(str::to_string),
    );
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO issues (id, title, desc, status, session, creator, created, updated, log)
                 VALUES (?1, ?2, ?3, 'todo', 'lane-a', 'tester', 1785000000, 1785000000, ?4)",
                rusqlite::params![id, title, desc, log],
            )?;
            Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
}

fn hit_ids(v: &Value) -> Vec<String> {
    v["hits"]
        .as_array()
        .map(|a| a.iter().map(|h| h["id"].as_str().unwrap_or("").to_string()).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------

#[tokio::test]
async fn fts5_is_compiled_in_and_the_migration_created_the_index() {
    // If the bundled SQLite lacked FTS5 the migration would have failed at
    // Store::open with "no such module: fts5" — this asserts the shape rather
    // than trusting that the build flags are what we think they are.
    let (_app, store, _d) = app();
    let conn = store.read().unwrap();
    let kinds: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE name IN ('search_docs','search_fts')")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(kinds.len(), 2, "both the content table and the FTS index must exist: {kinds:?}");
}

#[tokio::test]
async fn index_counts_match_table_counts_and_status_says_so() {
    let (app, store, _d) = app();
    for i in 1..=5 {
        card(&store, &format!("T-{i}"), &format!("card {i}"), "body text", None);
    }
    let (st, v) = get(&app, "/api/search/status").await;
    assert_eq!(st, StatusCode::OK);
    assert!(v["consistent"].as_bool().unwrap(), "status: {v}");
    let task = v["families"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["type"] == "task")
        .cloned()
        .unwrap();
    assert_eq!(task["indexed"], 5);
    assert_eq!(task["live"], 5);
    assert_eq!(v["docs_total"], v["fts_rows"], "content table and FTS index must agree");
}

#[tokio::test]
async fn a_term_present_only_in_the_card_log_is_findable() {
    let (app, store, _d) = app();
    // The discriminating case: `parsnip` appears in NEITHER the title nor the
    // desc — only in a history line. A body built from title+desc would pass
    // every other test in this file and fail this one.
    card(&store, "T-1", "unrelated title", "unrelated description", Some("`09:12` moved to doing by parsnip\n"));
    card(&store, "T-2", "another card", "nothing to see", None);

    let (_, v) = get(&app, "/api/search?q=parsnip").await;
    assert_eq!(hit_ids(&v), vec!["T-1"], "log-only term must be findable: {v}");
    assert!(
        v["hits"][0]["snippet"].as_str().unwrap().contains("<mark>parsnip</mark>"),
        "the match must be highlighted where it was found: {}",
        v["hits"][0]["snippet"]
    );
}

#[tokio::test]
async fn deleting_a_card_removes_it_from_the_index() {
    let (app, store, _d) = app();
    card(&store, "T-1", "quokka sighting", "in the desc", None);
    let (_, v) = get(&app, "/api/search?q=quokka").await;
    assert_eq!(hit_ids(&v), vec!["T-1"]);

    // The board soft-deletes (sets `deleted`), which is the path that actually
    // happens; a hard DELETE is covered below because a restored/repaired DB
    // can take that path.
    store
        .write(|conn| {
            conn.execute("UPDATE issues SET deleted = 1785000001 WHERE id = 'T-1'", [])?;
            Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_, v) = get(&app, "/api/search?q=quokka").await;
    assert!(hit_ids(&v).is_empty(), "soft-deleted card must leave the index: {v}");

    // …and the index must not be left holding an orphan row either, which a
    // hit count alone would not reveal.
    let (_, st) = get(&app, "/api/search/status").await;
    assert_eq!(st["docs_total"], 0, "{st}");
    assert_eq!(st["fts_rows"], 0, "the FTS side must be cleaned too: {st}");
    assert!(st["consistent"].as_bool().unwrap());

    card(&store, "T-2", "quokka again", "x", None);
    store
        .write(|conn| {
            conn.execute("DELETE FROM issues WHERE id = 'T-2'", [])?;
            Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_, v) = get(&app, "/api/search?q=quokka").await;
    assert!(hit_ids(&v).is_empty(), "hard-deleted card must leave the index: {v}");
}

#[tokio::test]
async fn updating_a_card_reindexes_rather_than_duplicating() {
    let (app, store, _d) = app();
    card(&store, "T-1", "before", "old word: tapir", None);
    store
        .write(|conn| {
            conn.execute("UPDATE issues SET desc = 'new word: okapi', updated = 1785000002 WHERE id = 'T-1'", [])?;
            Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_, old) = get(&app, "/api/search?q=tapir").await;
    assert!(hit_ids(&old).is_empty(), "the superseded text must not still match: {old}");
    let (_, new) = get(&app, "/api/search?q=okapi").await;
    assert_eq!(hit_ids(&new), vec!["T-1"]);
    let (_, st) = get(&app, "/api/search/status").await;
    assert_eq!(st["docs_total"], 1, "an update must not leave a second doc: {st}");
}

#[tokio::test]
async fn ranking_puts_a_title_match_above_a_body_match() {
    let (app, store, _d) = app();
    // Same term, different field. bm25 weights title 10x, so the title hit
    // must come first regardless of insertion order — which is why the body
    // card is inserted FIRST (a stable-order bug would pass otherwise).
    card(&store, "BODY-1", "some other heading", "a long description that mentions numbat once", None);
    card(&store, "TITLE-1", "numbat", "a long description with no such term at all", None);

    let (_, v) = get(&app, "/api/search?q=numbat").await;
    let ids = hit_ids(&v);
    assert_eq!(ids.len(), 2, "{v}");
    assert_eq!(ids[0], "TITLE-1", "title match must outrank body match: {v}");
    let ranks: Vec<f64> = v["hits"].as_array().unwrap().iter().map(|h| h["rank"].as_f64().unwrap()).collect();
    assert!(ranks[0] < ranks[1], "bm25 rank is smaller-is-better: {ranks:?}");
}

#[tokio::test]
async fn type_filter_restricts_and_is_reported() {
    let (app, store, _d) = app();
    card(&store, "T-1", "wombat card", "x", None);
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO schedules (id, title, session, command, created, updated)
                 VALUES ('SCHED-1', 'wombat schedule', 'lane-a', 'echo wombat', 1785000000, 1785000000)",
                [],
            )?;
            Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();

    let (_, all) = get(&app, "/api/search?q=wombat").await;
    assert_eq!(all["total"], 2, "{all}");
    let (_, only) = get(&app, "/api/search?q=wombat&types=schedule").await;
    assert_eq!(hit_ids(&only), vec!["SCHED-1"], "{only}");
    assert_eq!(only["types"][0], "schedule");
}

#[tokio::test]
async fn a_query_that_is_fts_syntax_is_treated_as_text_not_as_an_error() {
    let (app, store, _d) = app();
    card(&store, "T-1", "notes about a:b", "and NOT much else", None);
    // Each of these is a syntax error or an unintended operator if passed to
    // FTS5 raw. A 500 here is the bug this guards.
    for q in ["a%3Ab", "NOT", "%22unbalanced", "*", "%28paren"] {
        let (st, v) = get(&app, &format!("/api/search?q={q}")).await;
        assert_eq!(st, StatusCode::OK, "query {q:?} must not error: {v}");
        assert!(v["error"].is_null(), "query {q:?}: {v}");
    }
}

#[tokio::test]
async fn snippets_cannot_inject_markup() {
    let (app, store, _d) = app();
    card(&store, "T-1", "xss probe", "<script>alert('pwned')</script> containing echidna", None);
    let (_, v) = get(&app, "/api/search?q=echidna").await;
    let snip = v["hits"][0]["snippet"].as_str().unwrap();
    assert!(snip.contains("<mark>echidna</mark>"), "{snip}");
    assert!(!snip.contains("<script>"), "raw markup must not survive into the snippet: {snip}");
    assert!(snip.contains("&lt;script&gt;"), "{snip}");
}

#[tokio::test]
async fn an_empty_query_says_so_instead_of_matching_everything() {
    let (app, store, _d) = app();
    card(&store, "T-1", "anything", "at all", None);
    let (st, v) = get(&app, "/api/search?q=").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["total"], 0, "an empty query must not return the corpus: {v}");
    assert!(v["note"].as_str().unwrap().contains("empty query"), "{v}");
}

#[tokio::test]
async fn status_reports_drift_and_reindex_repairs_it() {
    let (app, store, _d) = app();
    card(&store, "T-1", "indexed card", "x", None);
    // Simulate the failure this instrument exists for: docs removed without
    // their source rows. Without a status view, the only symptom is a search
    // that returns nothing — indistinguishable from "no matches".
    store
        .write(|conn| {
            conn.execute("DELETE FROM search_docs", [])?;
            Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_, st) = get(&app, "/api/search/status").await;
    assert!(!st["consistent"].as_bool().unwrap(), "drift must be visible: {st}");

    let req = Request::builder()
        .method("POST")
        .uri("/api/search/reindex")
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let report: Value = serde_json::from_slice(&bytes).unwrap();
    // The rebuild reports its counts — that is the backfill's report.
    assert!(report["before"]["consistent"] == false, "{report}");
    assert!(report["after"]["consistent"] == true, "{report}");
    let task = report["per_family"].as_array().unwrap().iter().find(|f| f["type"] == "task").cloned().unwrap();
    assert_eq!(task["indexed"], 1, "{report}");

    let (_, v) = get(&app, "/api/search?q=indexed").await;
    assert_eq!(hit_ids(&v), vec!["T-1"]);
}

#[tokio::test]
async fn memories_messages_and_workers_are_indexed_too() {
    let (app, store, _d) = app();
    store
        .write(|conn| {
            conn.execute(
                "INSERT INTO _amux_memories (id, scope, name, content, memory_type, created_at, updated_at, provenance)
                 VALUES ('mem_1', '{\"level\":\"global\"}', 'pangolin-note', 'remember the pangolin', 'user',
                         '2026-08-09T00:00:00+00:00', '2026-08-09T00:00:00+00:00', '{}')",
                [],
            )?;
            conn.execute(
                "INSERT INTO _amux_messages (id, from_actor, target, body, created_at, delivery)
                 VALUES ('msg_1', '{\"id\":\"lane-a\"}', '{\"id\":\"lane-b\"}', 'ping about the pangolin',
                         '2026-08-09T00:00:00+00:00', '{}')",
                [],
            )?;
            conn.execute(
                "INSERT INTO _amux_workers (id, display_name, cwd, provider, backend, created_at, updated_at)
                 VALUES ('wrk_1', 'pangolin-lane', '/tmp', 'claude', 'tmux',
                         '2026-08-09T00:00:00+00:00', '2026-08-09T00:00:00+00:00')",
                [],
            )?;
            Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();

    let (_, v) = get(&app, "/api/search?q=pangolin").await;
    let mut types: Vec<&str> = v["hits"].as_array().unwrap().iter().map(|h| h["type"].as_str().unwrap()).collect();
    types.sort_unstable();
    assert_eq!(types, vec!["memory", "message", "worker"], "{v}");
    // Provenance chips the plan asks a SearchHit to carry.
    for h in v["hits"].as_array().unwrap() {
        assert!(h["link"].as_str().map(|l| !l.is_empty()).unwrap_or(false), "hit needs a link target: {h}");
        assert!(h["updated_at"].as_i64().unwrap() > 0, "hit needs a timestamp: {h}");
    }
    let (_, st) = get(&app, "/api/search/status").await;
    assert!(st["consistent"].as_bool().unwrap(), "{st}");
}

#[tokio::test]
async fn archived_cards_stay_searchable_and_carry_the_flag() {
    // Ethos rule 1's archived-filter trap: an index that quietly drops
    // archived cards makes the majority of the board unfindable, and the
    // symptom is a plausible-looking empty result.
    let (app, store, _d) = app();
    card(&store, "T-1", "archived aardvark", "x", None);
    store
        .write(|conn| {
            conn.execute("UPDATE issues SET archived = 1 WHERE id = 'T-1'", [])?;
            Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_, v) = get(&app, "/api/search?q=aardvark").await;
    assert_eq!(hit_ids(&v), vec!["T-1"], "archived cards must remain findable: {v}");
    assert_eq!(v["hits"][0]["meta"]["archived"], 1, "the flag must ride along so a client can filter: {v}");
}

/// TG-3303: a zero from search must say whether the measurement RAN.
///
/// ts-gke searched for a card they had created two hours earlier, got
/// `{"hits":[]}` with HTTP 200, and were one step from re-filing a peer's work
/// as untracked. Only a known-positive control — searching for a card id they
/// had personally created that evening — told them the instrument was dead
/// rather than the data absent.
///
/// What makes it worse than a papercut: `/api/board` REFUSES `q=` with a 400
/// whose own text redirects the caller here. So the documented way to find a
/// card by content was the one that could silently answer "nothing", and a
/// caller following the documentation had no reason to doubt the zero. A
/// silently-empty search does not fail, it AGREES WITH YOU, which makes every
/// "no prior art exists, I will build it" decision through it unfalsifiable.
///
/// THREE CELLS, and the third is the one that stops this being a blunt "503 on
/// any zero" that would break every legitimate no-match search.
#[tokio::test]
async fn an_empty_index_is_an_error_and_a_real_no_match_is_not() {
    let (app, store, _d) = app();

    // CELL 1 — nothing indexed at all. This cannot answer any query, so a 200
    // with an empty list is the lie. It is a 503 naming the repair.
    let (st, v) = get(&app, "/api/search?q=anything").await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE, "an empty index must not serve a quiet zero: {v}");
    assert_eq!(v["index_docs"], serde_json::json!(0));
    assert!(v["error"].as_str().unwrap_or_default().contains("empty"), "{v}");
    assert!(v["fix"].as_str().unwrap_or_default().contains("reindex"), "name the repair: {v}");

    card(&store, "AM-1", "tunnel relay port", "the long-poll client", None);

    // CELL 2 — the positive control. Without it, a handler that 503'd
    // everything would pass cell 1 and cell 3 both.
    let (st, v) = get(&app, "/api/search?q=tunnel").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(hit_ids(&v), vec!["AM-1"], "{v}");

    // CELL 3 — a REAL no-match, which must stay a 200. The index has documents
    // and none of them match, and that is a legitimate answer to a legitimate
    // question. `index_docs` rides along so the caller can tell this zero from
    // cell 1's without running a control of their own.
    let (st, v) = get(&app, "/api/search?q=zzzzznotathing").await;
    assert_eq!(st, StatusCode::OK, "a genuine no-match is not an error: {v}");
    assert_eq!(v["hits"].as_array().map(Vec::len), Some(0));
    assert_eq!(v["index_docs"], serde_json::json!(1), "the zero must publish what was searched: {v}");
}

// ---------------------------------------------------------------------------
// AF-499 — the human's own prompts are searchable
// ---------------------------------------------------------------------------

/// Insert straight into `cmd_history`, for the same reason `card` does: the
/// claim is that a TRIGGER indexes it, and a test going through the API could
/// not tell a trigger from a write hook.
fn prompt(store: &Store, id: i64, session: &str, kind: &str, text: &str) {
    let (session, kind, text) = (session.to_string(), kind.to_string(), text.to_string());
    store
        .write(move |conn| {
            conn.execute(
                "INSERT INTO cmd_history (id, text, type, session, ts, origin, card_id)
                 VALUES (?1, ?2, ?3, ?4, 1785000000, '', NULL)",
                rusqlite::params![id, text, kind, session],
            )?;
            Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
}

/// THE CELL THIS EXISTS FOR. Reported live: "you ask it a question ... and then
/// the answer to that question is gonna be lost in the ether." The question was
/// stored the whole time and no instrument could reach it — measured on the live
/// DB, 10,569 cmd_history rows against an index that held none of them.
#[tokio::test]
async fn a_question_a_human_asked_a_worker_is_findable_afterwards() {
    let (app, store, _d) = app();
    prompt(&store, 1, "gtm-engine", "user", "can you check whether the HubSpot token still works");
    let (st, v) = get(&app, "/api/search?q=HubSpot").await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        hit_ids(&v).contains(&"1".to_string()),
        "the prompt was not indexed: {v:#}"
    );
}

/// The predicate, which is the whole design. cmd_history is mostly MACHINE
/// traffic — auto-pickup dispatches, peer relays, scheduler commands — 8,923 of
/// 10,569 rows on the live DB. Indexing those turns a searchable corpus into a
/// log, which is the thing that stops it discriminating (ethos rule 5).
#[tokio::test]
async fn machine_traffic_in_the_same_table_is_not_indexed() {
    let (app, store, _d) = app();
    prompt(&store, 1, "gtm-engine", "user", "zebrafish question from a human");
    prompt(&store, 2, "gtm-engine", "pickup", "zebrafish auto-pickup dispatch");
    prompt(&store, 3, "gtm-engine", "session", "zebrafish peer relay");
    prompt(&store, 4, "gtm-engine", "schedule", "zebrafish scheduled command");
    let (_st, v) = get(&app, "/api/search?q=zebrafish").await;
    assert_eq!(
        hit_ids(&v),
        vec!["1".to_string()],
        "exactly the human prompt should be indexed, nothing else: {v:#}"
    );
}

/// The status view must describe the SAME population the index holds, or a
/// drifted index reads as a corpus with nothing in it (ethos rule 4). This is
/// the cell that fails if FAMILIES' predicate and the trigger's disagree.
#[tokio::test]
async fn the_status_count_for_prompts_matches_what_is_actually_indexed() {
    let (app, store, _d) = app();
    prompt(&store, 1, "a", "user", "one");
    prompt(&store, 2, "a", "pickup", "two");
    prompt(&store, 3, "b", "user", "three");
    let (st, v) = get(&app, "/api/search/status").await;
    assert_eq!(st, StatusCode::OK);
    let row = v["families"]
        .as_array()
        .and_then(|a| a.iter().find(|t| t["type"] == "prompt").cloned())
        .unwrap_or_else(|| panic!("no prompt family in status: {v:#}"));
    assert_eq!(row["live"], 2, "the live count is not the type='user' population: {row:#}");
    assert_eq!(row["indexed"], 2, "the indexed count disagrees with the source: {row:#}");
    assert_eq!(row["consistent"], true, "{row:#}");
    assert_eq!(row["predicate"], "type = 'user'", "the status describes a different population: {row:#}");
}

/// Deleting the history row removes it from the index. Without this a prompt
/// someone deleted stays searchable, which is the opposite of what deleting it
/// meant.
#[tokio::test]
async fn deleting_a_prompt_removes_it_from_the_index() {
    let (app, store, _d) = app();
    prompt(&store, 7, "a", "user", "kumquat");
    let (_s, before) = get(&app, "/api/search?q=kumquat").await;
    assert_eq!(hit_ids(&before), vec!["7".to_string()]);
    store
        .write(|conn| {
            conn.execute("DELETE FROM cmd_history WHERE id = 7", [])?;
            Ok(amux_server::db::WriteOutcome { applied: false, events: vec![] })
        })
        .unwrap();
    let (_s, after) = get(&app, "/api/search?q=kumquat").await;
    assert!(hit_ids(&after).is_empty(), "a deleted prompt is still searchable: {after:#}");
}
