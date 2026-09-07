//! AMUX-2646 — a self-report must come from the session it describes, and a
//! test must not be able to write into the live fleet's report store.
//!
//! THE INCIDENT. `amux-rust` showed `idle` on its card while its pane plainly
//! read `esc to interrupt`. Its stored self-report was
//! `{"state":"idle","source":"stop-hook-test","age_s":1076}` — a hand-run hook
//! test had written a fabricated `idle` onto a LIVE working lane. Every
//! consumer of that store (the card, the steering gate, the board's `stale`
//! flag) then believed it, and the derivation's asymmetric freshness rule
//! meant it would have been believed for 24 hours.
//!
//! TWO SEPARATE HOLES, and they need separate fixes because neither closes the
//! other:
//!
//!   * `/report` accepted any state for any session from any caller, and kept
//!     no verified record of who wrote it. `source` is a free string the
//!     CALLER picks, so it labels a write, it does not attribute one. Closed
//!     by the `X-Amux-Session` stamp check, tested below.
//!   * a test process could reach `~/.amux/amux.db` at all. Closed by this
//!     file running against a temp store AND a temp `AMUX_HOME`, with a guard
//!     test that fails if that ever stops being true — a test-isolation rule
//!     nobody can verify is one that quietly lapses.
//!
//! Why not "refuse sources that look synthetic" (a `*-test` suffix check):
//! `source` is chosen by the caller, so the identical write named `stop-hook`
//! passes. That is a check that cannot fail against the case it exists for.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use tower::ServiceExt;

/// Lane names no fleet session answers to, so nothing in the delivery path
/// can reach a real tmux pane even if a code path tries. One pair per test:
/// the report store is keyed by session name, so shared names would let two
/// parallel tests overwrite each other's row.
const OTHER: &str = "amux2646-other-lane";
const LANE_FOREIGN: &str = "amux2646-foreign-probe";
const LANE_SELF: &str = "amux2646-self-probe";
const LANE_UNSTAMPED: &str = "amux2646-unstamped-probe";

type Rig = (axum::Router, std::sync::Arc<Store>);

/// ONE temp home for the whole file, set exactly once.
///
/// `AMUX_HOME` is process-global and `cargo test` runs these in PARALLEL
/// threads, so a per-test `set_var` is not isolation — it is a race: the
/// second test's assignment silently redirects the first test's session
/// lookups into a directory with none of its files, and the failure surfaces
/// as an unrelated 404. (Observed here on the first run, which is why this is
/// a `OnceLock` and not four `set_var`s.) One home, set before any router
/// exists, is deterministic.
static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();

fn home() -> &'static std::path::Path {
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        // Isolation, not decoration: session paths resolve `AMUX_HOME` at CALL
        // time, so a suite that leaves it unset reads and writes the machine's
        // real fleet directory.
        std::env::set_var("AMUX_HOME", dir.path());
        dir
    })
    .path()
}

/// A router over a FRESH store, plus env files for this test's lanes.
///
/// Lane names are per-test so two parallel tests cannot overwrite each other's
/// report rows — the report store is keyed by session name.
fn app(lanes: &[&str]) -> Rig {
    let home = home();
    let sessions = home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    // The verb router 404s a session with no env file. These live in the TEMP
    // home and match no tmux session, so even a code path that tried to reach
    // a pane would find nothing.
    for lane in lanes {
        std::fs::write(
            sessions.join(format!("{lane}.env")),
            format!("CC_DIR=\"{}\"\nCC_PROVIDER=\"claude\"\n", home.display()),
        )
        .unwrap();
    }
    let db = home.join(format!("db-{}.sqlite", lanes.first().copied().unwrap_or("rig")));
    let store = std::sync::Arc::new(Store::open(&db).unwrap());
    let state = AppState {
        secrets: std::sync::Arc::new(amux_server::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
        store: store.clone(),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    (router(state), store)
}

async fn post(
    app: &axum::Router,
    path: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut b = Request::builder().method("POST").uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let req = b
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

/// The PERSISTED report, read straight out of the same prefs row the
/// derivation reads. Deliberately not via an API projection: the claim under
/// test is what LANDED in the store, and a projection can agree with the
/// response while the row says something else.
fn stored_report(store: &Store, lane: &str) -> Value {
    let conn = store.read().expect("store readable");
    let raw: String = conn
        .query_row("SELECT value FROM prefs WHERE key='session_reports'", [], |r| r.get(0))
        .unwrap_or_else(|_| "{}".into());
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    v[lane].clone()
}

/// THE NEGATIVE CONTROL, rebuilt from the incident's own artifact: another
/// session posting `stop-hook-test` idle onto a working lane.
#[tokio::test]
async fn a_foreign_session_cannot_report_state_for_another_lane() {
    let (app, store) = app(&[LANE_FOREIGN, OTHER]);
    let lane = LANE_FOREIGN;
    // The lane reports itself active, as its own prompt hook would.
    let (st, _) = post(
        &app,
        &format!("/api/sessions/{lane}/report"),
        json!({"state": "active", "source": "prompt-hook"}),
        &[("X-Amux-Session", lane)],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "a lane must be able to report its own state");

    // Now the incident: a DIFFERENT session writes idle onto it.
    let (st, body) = post(
        &app,
        &format!("/api/sessions/{lane}/report"),
        json!({"state": "idle", "source": "stop-hook-test"}),
        &[("X-Amux-Session", OTHER)],
    )
    .await;
    assert_eq!(
        st,
        StatusCode::FORBIDDEN,
        "a stamped cross-session report must be refused, got body {body}"
    );
    assert_eq!(body["origin"], json!(OTHER), "the refusal must name who tried");
    assert_eq!(body["target"], json!(lane));

    // And it must be refused at the STORE, not merely in the response. A 403
    // that wrote anyway is the shape of bug this repo keeps finding: confirm
    // at the field, never at the status code.
    let rep = stored_report(&store, lane);
    assert_eq!(
        rep["state"],
        json!("active"),
        "the refused write must not have landed: {rep}"
    );
    assert_eq!(rep["source"], json!("prompt-hook"), "source must be the lane's own: {rep}");
}

/// A lane reporting its own state records the SERVER-VERIFIED writer beside
/// the caller-chosen label, so "who wrote this" is answerable from the store.
#[tokio::test]
async fn a_self_stamped_report_records_its_verified_origin() {
    let (app, store) = app(&[LANE_SELF]);
    let lane = LANE_SELF;
    let (st, _) = post(
        &app,
        &format!("/api/sessions/{lane}/report"),
        json!({"state": "idle", "source": "stop-hook"}),
        &[("X-Amux-Session", lane)],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let rep = stored_report(&store, lane);
    assert_eq!(rep["state"], json!("idle"));
    assert_eq!(
        rep["origin"],
        json!(lane),
        "the verified writer must be stored, not just the caller's label: {rep}"
    );
}

/// THE RESIDUAL, asserted rather than assumed. The shipped hooks in
/// `~/.claude/settings.json` post with no `X-Amux-Session` header, so an
/// unstamped report is still accepted — refusing it would silence every lane
/// on the fleet at the next hook fire. This test exists so the trade-off is
/// visible and so a future "tighten it up" change has to face it deliberately
/// rather than discovering it in production.
#[tokio::test]
async fn an_unstamped_report_is_still_accepted_and_marked_unattributed() {
    let (app, store) = app(&[LANE_UNSTAMPED]);
    let lane = LANE_UNSTAMPED;
    let (st, _) = post(
        &app,
        &format!("/api/sessions/{lane}/report"),
        json!({"state": "waiting", "source": "hook"}),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "the shipped hooks send no header — they must keep working");
    let rep = stored_report(&store, lane);
    assert_eq!(rep["state"], json!("waiting"));
    assert_eq!(rep["origin"], json!(""), "unattributed must READ as unattributed: {rep}");
}

/// The isolation guard. Everything above is worthless if the suite can reach
/// the live fleet's store, and "we set a temp dir at the top" is exactly the
/// kind of claim that lapses silently when someone adds a test that builds its
/// own state. Fail loudly instead.
#[tokio::test]
async fn this_suite_cannot_reach_the_live_amux_home() {
    let (_app, store) = app(&["amux2646-isolation-probe"]);
    let set = std::env::var("AMUX_HOME").expect("AMUX_HOME must be set by the rig");
    let set = std::path::Path::new(&set);
    assert_eq!(set, home(), "every rig in this file must share the one temp home");
    let real = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux");
    assert_ne!(set, real, "a test must never target the live ~/.amux");
    assert!(
        set.starts_with(std::env::temp_dir()),
        "AMUX_HOME must be under the system temp dir, got {set:?}"
    );
    // …and the STORE too: an isolated home with a live DB handle would still
    // write session_reports into the fleet's database.
    let path: String = store
        .read()
        .unwrap()
        .query_row("PRAGMA database_list", [], |r| r.get(2))
        .unwrap_or_default();
    // Canonicalised on both sides: on macOS the temp dir is `/var/...` and
    // SQLite reports `/private/var/...` for the same file, so a raw prefix
    // compare fails on a correctly-isolated store — a probe that reports the
    // fix as broken.
    let real_set = set.canonicalize().unwrap_or_else(|_| set.to_path_buf());
    let real_db = std::path::Path::new(&path)
        .canonicalize()
        .unwrap_or_else(|_| std::path::PathBuf::from(&path));
    assert!(
        !path.is_empty() && real_db.starts_with(&real_set),
        "the store must live inside the temp home, got {real_db:?} vs {real_set:?}"
    );
}

/// AMUX-2676: the harness can report its own model and token spend.
///
/// Ethan, 01:54: "this worker says working but is not doing anything." It was
/// working; the card just had nothing behind the badge — `active_model` and
/// `tokens` were hardcoded empty on 48/48 running sessions since the python
/// scanner that held them in memory retired. Accepting them on the report
/// endpoint is D1's exit: the harness states its own facts, and a better
/// harness improves the card with no amux change.
#[tokio::test]
async fn a_report_can_carry_the_model_and_token_spend() {
    let lane = "lane-model";
    let (app, store) = app(&[lane]);
    let (st, _) = post(
        &app,
        &format!("/api/sessions/{lane}/report"),
        json!({
            "state": "active", "source": "prompt-hook",
            "model": "claude-opus-5",
            "tokens": {"input": 1200, "output": 340}
        }),
        &[("X-Amux-Session", lane)],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let rep = stored_report(&store, lane);
    assert_eq!(rep["model"], json!("claude-opus-5"), "{rep}");
    // total is derived when the caller omits it — a hook should not have to do
    // arithmetic to be useful.
    assert_eq!(rep["tokens"]["total"], json!(1540), "{rep}");
}

/// ABSENT IS NOT EMPTY — the subtle one, and the reason this is not a plain
/// overwrite.
///
/// tool-hook fires on EVERY tool call and carries no model. If a heartbeat
/// blanked the field, the model would appear and vanish many times a minute:
/// worse than never showing it, because it would look like the model kept
/// changing. Only a present, non-empty value overwrites.
#[tokio::test]
async fn a_heartbeat_without_a_model_does_not_erase_the_one_already_reported() {
    let lane = "lane-carry";
    let (app, store) = app(&[lane]);
    post(
        &app,
        &format!("/api/sessions/{lane}/report"),
        json!({"state": "active", "source": "prompt-hook", "model": "claude-opus-5",
               "tokens": {"input": 10, "output": 5}}),
        &[("X-Amux-Session", lane)],
    )
    .await;
    // A bare heartbeat, exactly as the shipped tool-hook sends it today.
    let (st, _) = post(
        &app,
        &format!("/api/sessions/{lane}/report"),
        json!({"state": "active", "source": "tool-hook"}),
        &[("X-Amux-Session", lane)],
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let rep = stored_report(&store, lane);
    assert_eq!(rep["model"], json!("claude-opus-5"), "model was erased: {rep}");
    assert_eq!(rep["tokens"]["total"], json!(15), "tokens were erased: {rep}");
}

/// An all-zero token payload is what an UNINSTRUMENTED caller sends. Recording
/// it would replace "not reported" with a confident zero — the same class of
/// mislabelling that made the card claim "working" with no evidence.
#[tokio::test]
async fn an_all_zero_token_payload_is_not_recorded_as_a_measurement() {
    let lane = "lane-zero";
    let (app, store) = app(&[lane]);
    post(
        &app,
        &format!("/api/sessions/{lane}/report"),
        json!({"state": "active", "source": "prompt-hook",
               "tokens": {"input": 0, "output": 0, "total": 0}}),
        &[("X-Amux-Session", lane)],
    )
    .await;
    let rep = stored_report(&store, lane);
    assert!(
        !rep["tokens"].is_object(),
        "an all-zero payload must not be stored as a measurement: {rep}"
    );
}
