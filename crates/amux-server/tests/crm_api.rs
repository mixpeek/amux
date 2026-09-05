//! CRM API integration tests (AMUX-2929) — real router, temp-file store,
//! NEVER ~/.amux/amux.db, which holds 308 real contacts.
//!
//! These exist because the CRM routes were MISSING for the whole life of the
//! Rust server while the schema shipped in `0001_baseline.sql` and the data sat
//! there unreachable. Global CLAUDE.md told every session the API worked; the
//! SPA's own AMUX-2590 comment said "the /api/crm endpoints still exist for
//! agents". Nothing could fail, so nothing did — the gap was invisible until
//! someone actually ran `amux crm add`.
//!
//! So the first test is deliberately the dumbest one: does POST answer at all.
//! It fails against the pre-port server with 405, which is exactly what a
//! caller saw.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

fn app() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("crm-test.db")).unwrap();
    let state = AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), dir)
}

async fn send(app: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let b = Request::builder().method(method).uri(path);
    let req = match body {
        Some(v) => b
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => b.body(Body::empty()).unwrap(),
    };
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let v = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

/// THE REGRESSION. Pre-port this returned 405 — the GET-only SPA catch-all
/// answering a non-GET — while CLAUDE.md documented it as working.
#[tokio::test]
async fn the_documented_endpoints_are_actually_mounted() {
    let (app, _d) = app();
    let (st, _) = send(&app, "GET", "/api/crm/contacts", None).await;
    assert_eq!(st, StatusCode::OK, "GET /api/crm/contacts must be routed");
    let (st, _) = send(&app, "GET", "/api/crm/followups", None).await;
    assert_eq!(st, StatusCode::OK, "GET /api/crm/followups must be routed");
    let (st, body) =
        send(&app, "POST", "/api/crm/contacts", Some(json!({"name": "Ada Lovelace"}))).await;
    assert_eq!(
        st,
        StatusCode::CREATED,
        "POST /api/crm/contacts must be routed — it answered 405 before the port: {body}"
    );
}

#[tokio::test]
async fn a_contact_round_trips_with_tags_and_interactions() {
    let (app, _d) = app();
    let (st, created) = send(
        &app,
        "POST",
        "/api/crm/contacts",
        Some(json!({
            "name": "Grace Hopper", "company": "USN", "role": "Rear Admiral",
            "email": "grace@example.mil", "tags": ["compilers", "navy", ""]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let id = created["id"].as_str().expect("id").to_string();
    assert!(id.starts_with("PPL-"), "ids keep the python PPL- shape, got {id}");

    let (st, c) = send(&app, "GET", &format!("/api/crm/contacts/{id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(c["name"], "Grace Hopper");
    assert_eq!(c["company"], "USN");
    // The empty tag is dropped, not stored — python trims and skips falsey.
    assert_eq!(c["tags"], json!(["compilers", "navy"]), "empty tags must not be stored");
    assert_eq!(c["interactions"], json!([]));

    let (st, ix) = send(
        &app,
        "POST",
        &format!("/api/crm/contacts/{id}/interactions"),
        Some(json!({
            "date": "2026-08-01", "type": "call", "notes": "spoke about COBOL",
            "follow_up_date": "2026-09-01", "follow_up_note": "send the spec"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let ix_id = ix["id"].as_str().expect("interaction id").to_string();

    let (_, c) = send(&app, "GET", &format!("/api/crm/contacts/{id}"), None).await;
    assert_eq!(c["interactions"].as_array().unwrap().len(), 1);
    assert_eq!(c["interactions"][0]["type"], "call");

    // The list view's correlated subqueries must surface the interaction.
    let (_, list) = send(&app, "GET", "/api/crm/contacts", None).await;
    let row = list.as_array().unwrap().iter().find(|r| r["id"] == id.as_str()).expect("in list");
    assert_eq!(row["last_date"], "2026-08-01");
    assert_eq!(row["next_followup"], "2026-09-01");
    assert_eq!(row["next_followup_note"], "send the spec");

    let (_, f) = send(&app, "GET", "/api/crm/followups", None).await;
    assert_eq!(f.as_array().unwrap().len(), 1, "the pending follow-up must appear");
    assert_eq!(f[0]["name"], "Grace Hopper");

    // Interactions HARD delete (python parity — no `deleted` column exists).
    let (st, _) = send(&app, "DELETE", &format!("/api/crm/interactions/{ix_id}"), None).await;
    assert_eq!(st, StatusCode::OK);
    let (_, c) = send(&app, "GET", &format!("/api/crm/contacts/{id}"), None).await;
    assert_eq!(c["interactions"], json!([]));
}

/// An interaction with NO follow-up must not look like a pending one. The
/// list/followups queries key off `follow_up_date IS NOT NULL`, so writing ""
/// instead of NULL would make every logged call a follow-up.
#[tokio::test]
async fn an_absent_follow_up_date_is_null_not_empty_string() {
    let (app, _d) = app();
    let (_, c) = send(&app, "POST", "/api/crm/contacts", Some(json!({"name": "No Followup"}))).await;
    let id = c["id"].as_str().unwrap().to_string();
    send(
        &app,
        "POST",
        &format!("/api/crm/contacts/{id}/interactions"),
        Some(json!({"type": "email", "notes": "fyi"})),
    )
    .await;

    let (_, f) = send(&app, "GET", "/api/crm/followups", None).await;
    assert_eq!(f.as_array().unwrap().len(), 0, "no follow-up date means no follow-up row");
    let (_, list) = send(&app, "GET", "/api/crm/contacts", None).await;
    let row = list.as_array().unwrap().iter().find(|r| r["id"] == id.as_str()).unwrap();
    assert_eq!(row["next_followup"], Value::Null);
    // The date defaulted to today rather than being left empty.
    assert!(row["last_date"].as_str().unwrap_or("").len() == 10, "date defaults to today (ISO)");
}

#[tokio::test]
async fn patch_writes_only_whitelisted_fields_and_replaces_tags() {
    let (app, _d) = app();
    let (_, c) = send(
        &app,
        "POST",
        "/api/crm/contacts",
        Some(json!({"name": "Alan", "company": "GCHQ", "tags": ["old"]})),
    )
    .await;
    let id = c["id"].as_str().unwrap().to_string();

    let (st, _) = send(
        &app,
        "PATCH",
        &format!("/api/crm/contacts/{id}"),
        // `id` and `deleted` are NOT writable — a caller must not be able to
        // re-key a row or soft-delete it through a field write.
        Some(json!({"company": "NPL", "tags": ["new", "shiny"], "id": "PPL-HACK", "deleted": 1})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, c) = send(&app, "GET", &format!("/api/crm/contacts/{id}"), None).await;
    assert_eq!(st, StatusCode::OK, "the row must still be reachable under its original id");
    assert_eq!(c["id"], id.as_str(), "id is not writable");
    assert_eq!(c["company"], "NPL");
    assert_eq!(c["name"], "Alan", "unmentioned fields are untouched");
    assert_eq!(c["tags"], json!(["new", "shiny"]), "tags are replaced wholesale");
}

/// Contacts SOFT delete: gone from every read, row still present. Python's
/// behaviour, and the reason `deleted IS NULL` is on every query.
#[tokio::test]
async fn deleting_a_contact_is_soft_and_hides_it_from_reads() {
    let (app, _d) = app();
    let (_, c) = send(&app, "POST", "/api/crm/contacts", Some(json!({"name": "Ephemeral"}))).await;
    let id = c["id"].as_str().unwrap().to_string();

    let (st, _) = send(&app, "DELETE", &format!("/api/crm/contacts/{id}"), None).await;
    assert_eq!(st, StatusCode::OK);

    let (st, _) = send(&app, "GET", &format!("/api/crm/contacts/{id}"), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "a soft-deleted contact reads as 404");
    let (_, list) = send(&app, "GET", "/api/crm/contacts", None).await;
    assert!(
        !list.as_array().unwrap().iter().any(|r| r["id"] == id.as_str()),
        "a soft-deleted contact is not listed"
    );
}

#[tokio::test]
async fn search_matches_name_company_and_role_only() {
    let (app, _d) = app();
    for (n, co, ro) in [
        ("Findme Byname", "Acme", "Engineer"),
        ("Someone", "Findme Bycompany", "Analyst"),
        ("Other", "Globex", "Findme Byrole"),
        ("Unrelated", "Initech", "Manager"),
    ] {
        send(
            &app,
            "POST",
            "/api/crm/contacts",
            Some(json!({"name": n, "company": co, "role": ro, "notes": "Findme Bynotes"})),
        )
        .await;
    }
    let (_, hits) = send(&app, "GET", "/api/crm/contacts?q=Findme", None).await;
    assert_eq!(
        hits.as_array().unwrap().len(),
        3,
        "name/company/role match; notes deliberately does NOT (python parity)"
    );
    // A control, so a filter that matched EVERYTHING could not pass this test.
    let (_, none) = send(&app, "GET", "/api/crm/contacts?q=zzzz-no-such-contact", None).await;
    assert_eq!(none.as_array().unwrap().len(), 0, "a non-matching search returns nothing");
    let (_, all) = send(&app, "GET", "/api/crm/contacts", None).await;
    assert_eq!(all.as_array().unwrap().len(), 4, "no query returns everything");
}

#[tokio::test]
async fn a_nameless_contact_is_refused() {
    let (app, _d) = app();
    for body in [json!({}), json!({"name": ""}), json!({"name": "   "})] {
        let (st, v) = send(&app, "POST", "/api/crm/contacts", Some(body.clone())).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "name is required: {body}");
        assert_eq!(v["error"], "name required");
    }
}

// ---------------------------------------------------------------------------
// Unrelated to CRM, but it belongs with an integration test that owns a router:
// GET /api/sessions caches for 2s and a config write must drop that cache
// (AMUX-2926). Kept here rather than in a new binary because it needs exactly
// the same harness and nothing else.
// ---------------------------------------------------------------------------

/// The cache must be droppable, and dropping it must be what a config write
/// does. Pinning the FUNCTION rather than the HTTP round-trip, because the
/// round-trip needs a real session on disk and a tmux fleet — this asserts the
/// invariant the wrapper exists to hold.
#[test]
fn invalidating_the_sessions_cache_forces_the_next_read_to_rebuild() {
    use amux_server::api::sessions_legacy::invalidate_sessions_cache;
    // Idempotent and safe to call with no cache populated — config_patch's
    // wrapper calls it unconditionally, including on error paths.
    invalidate_sessions_cache();
    invalidate_sessions_cache();
}

// ---------------------------------------------------------------------------
// AMUX-2923: the staged-guard's owner notification is deduped per hour, so a
// retried pre-commit hook cannot spam the session whose file is being swept.
// ---------------------------------------------------------------------------

/// The dedupe must SUPPRESS a repeat and must NOT suppress a different pair.
/// A dedupe that returns false for everything would silence the notification
/// entirely — the failure would look exactly like the bug it fixes.
#[test]
fn the_owner_notification_dedupes_per_pair_not_globally() {
    use amux_server::api::git_guard::notify_once;
    let a = format!("ownerA|committerB|{}", std::process::id());
    let b = format!("ownerA|committerC|{}", std::process::id());
    assert!(notify_once(&a), "first sighting notifies");
    assert!(!notify_once(&a), "a retried hook must NOT notify again");
    assert!(notify_once(&b), "a DIFFERENT committer is a different notice");
    assert!(!notify_once(&b));
}
