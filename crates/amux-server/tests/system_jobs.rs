//! `GET /api/system-jobs` + the tripwire that keeps its registry honest
//! (AMUX-2703).
//!
//! The registry is only worth anything if it cannot silently miss a job. Two
//! mechanisms make that true, and this file tests both:
//!
//! 1. `spawn_periodic_every` is the ONLY constructor of a `PeriodicTask` and
//!    registers unconditionally — covered by the unit test in
//!    `runtime_jobs::registry`.
//! 2. Long-lived loops that are not `PeriodicTask`s go through
//!    `registry::spawn_loop` / `adopt` at their call site in `lib.rs`. Nothing
//!    in the type system enforces that, so [`unregistered_spawns`] reads
//!    `lib.rs` AS TEXT and fails on a bare `tokio::spawn` of a background
//!    loop.
//!
//! That second check is a source scan, which is exactly the shape of probe
//! ethos rule 7 warns about — a hand-written guess about where the answer
//! lives, whose miss is indistinguishable from a pass. So it is run against a
//! DOCTORED copy of the same source as a negative control: if the scanner
//! cannot flag an obviously-bad file, its verdict on the real one means
//! nothing.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use amux_server::runtime_jobs::registry::CATALOG;
use serde_json::Value;
use tower::ServiceExt;

/// `tokio::spawn` call sites in `lib.rs` that are NOT background jobs, each
/// with the reason it is exempt. Anything else must go through
/// `registry::spawn_loop`, or it is invisible on /api/system-jobs.
///
/// Matched on the ~200 characters that FOLLOW the call, because that is where
/// the distinguishing text lives (`async move {` alone matches everything).
const ALLOWED_BARE_SPAWNS: &[(&str, &str)] = &[
    (
        "_amux_conversations",
        "StoreConversationSink::save — a fire-and-forget DB write, not a loop",
    ),
    (
        "DELETE FROM _amux_conversations",
        "StoreConversationSink::forget — same",
    ),
    (
        "tunnel::maybe_boot_start",
        "one-shot boot decision, not a loop: it starts nothing unless BOTH a token and an \
         explicit AMUX_TUNNEL_PORT are set, and the relay task it may create registers itself \
         under ids::TUNNEL via registry::adopt inside tunnel::start. Registering this shot \
         would put a job on /api/system-jobs that exits immediately by design (AMUX-2888)",
    ),
    (
        "reconcile_on_startup",
        "one-shot startup reconciliation moved off the bind path so the listener answers \
         immediately instead of connection-refusing for ~88s (AMUX-3969b). It runs once, \
         flips the `reconciled` flag /health serves, and exits; the long-lived ORCH_RUNTIME \
         loop it hands off to registers itself via jobs::spawn_loop inside the same block. \
         A death mid-shot is visible as `reconciled: false` on /health, so registering the \
         shot would only add a job that exits immediately by design (same as AMUX-2888)",
    ),
    ];

/// Every `tokio::spawn(` in `src` that is not on the allow-list, returned with
/// its byte offset so a failure names where to look.
fn unregistered_spawns(src: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while let Some(i) = src[at..].find("tokio::spawn(") {
        let pos = at + i;
        // Look FORWARD, not backward: the identifying text (the SQL, the bind
        // call) is inside the spawned block. A backward window would land in
        // the comment above it, which is prose and would match anything.
        let win = &src[pos..src.len().min(pos + 400)];
        if !ALLOWED_BARE_SPAWNS.iter().any(|(needle, _)| win.contains(needle)) {
            let line = src[..pos].matches('\n').count() + 1;
            out.push((line, win.lines().take(3).collect::<Vec<_>>().join(" ")));
        }
        at = pos + "tokio::spawn(".len();
    }
    out
}

#[test]
fn every_background_loop_in_lib_rs_goes_through_the_registry() {
    let src = include_str!("../src/lib.rs");

    // NEGATIVE CONTROL FIRST. A source scan that cannot flag a bad file is
    // theatre, and its green result on the real file looks identical either
    // way. So: doctor the real source with a spawn that is obviously not on
    // the allow-list, and require the scanner to catch it. If this assert ever
    // fails, the verdict below is meaningless.
    let doctored = format!("{src}\nfn later() {{ tokio::spawn(async move {{ forever().await; }}); }}\n");
    let caught = unregistered_spawns(&doctored);
    assert!(
        !caught.is_empty(),
        "the scanner cannot detect an unregistered spawn — its pass on the real file proves nothing"
    );

    // ...and it must not be flagging the allow-listed ones, or it would be the
    // opposite failure: a check that fires on everything, which gets disabled.
    assert_eq!(
        caught.len(),
        1,
        "scanner flagged more than the one planted spawn: {caught:?}"
    );

    let found = unregistered_spawns(src);
    assert!(
        found.is_empty(),
        "lib.rs spawns background work outside runtime_jobs::registry, so it will not appear on \
         /api/system-jobs and a death will be invisible. Use `registry::spawn_loop(ids::X, \
         interval, fut)` (or `adopt` if the callee owns the spawn), or add an entry to \
         ALLOWED_BARE_SPAWNS with a reason. Offenders (line, text): {found:?}"
    );

    // The allow-list must not rot into a list of things that no longer exist:
    // an entry that matches nothing would silently widen the exemption if the
    // code it described came back in another shape.
    for (needle, why) in ALLOWED_BARE_SPAWNS {
        assert!(
            src.contains(needle),
            "allow-list entry {needle:?} ({why}) matches nothing in lib.rs — delete it rather \
             than leaving a standing exemption for code that is gone"
        );
    }
}

/// `mod ids` is a set of constants and Rust cannot iterate one, so
/// `registry::ALL_IDS` is the enumerable copy that `CATALOG` is checked
/// against (unit test `every_id_has_a_doc_and_every_doc_has_an_id`). That
/// makes ALL_IDS itself the weak link: a new `pub const` that never reaches it
/// escapes both checks and renders nameless. So this reads the module as text.
///
/// Named target before searching, per ethos rule 7: a POSITIVE here is the
/// line `pub const SOMETHING: &str = "...";` inside `pub mod ids`, and the
/// negative control below proves the scan can produce one.
#[test]
fn every_id_constant_is_enumerated_in_all_ids() {
    let src = include_str!("../src/runtime_jobs/registry.rs");

    fn const_names(src: &str) -> Vec<String> {
        let start = match src.find("pub mod ids {") {
            Some(i) => i,
            None => return Vec::new(),
        };
        // Bounded on the CODE (the module's closing brace at column 0), not on
        // an arbitrary character count that a long comment would overrun.
        let end = src[start..].find("\n}\n").map(|i| start + i).unwrap_or(src.len());
        src[start..end]
            .lines()
            .filter_map(|l| l.trim().strip_prefix("pub const "))
            .filter_map(|l| l.split(':').next())
            .map(|s| s.trim().to_string())
            .collect()
    }

    // NEGATIVE CONTROL: the scan must find a planted constant, or its silence
    // about the real ones is uninformative.
    let doctored = src.replace(
        "pub mod ids {",
        "pub mod ids {\n    pub const PLANTED_FOR_TEST: &str = \"planted\";",
    );
    assert!(
        const_names(&doctored).contains(&"PLANTED_FOR_TEST".to_string()),
        "the scan cannot see an id constant at all — its verdict below is meaningless"
    );

    let names = const_names(src);
    assert!(names.len() >= 10, "scan found only {names:?} — it is not reading the ids module");
    // Every constant NAME must appear in the ALL_IDS list body. Comparing
    // names (not values) on purpose: ALL_IDS is written as `ids::NAME`, so
    // that is the text a forgotten entry is missing.
    let all_ids_block = {
        let s = src.find("pub const ALL_IDS").expect("ALL_IDS exists");
        let e = src[s..].find("];").map(|i| s + i).unwrap_or(src.len());
        &src[s..e]
    };
    let missing: Vec<&String> = names
        .iter()
        .filter(|n| !all_ids_block.contains(&format!("ids::{n},")))
        .collect();
    assert!(
        missing.is_empty(),
        "id constants missing from ALL_IDS, so nothing checks they have a CATALOG row and they \
         would render nameless in the UI: {missing:?}"
    );
}

async fn get_json(app: &axum::Router, path: &str) -> (u16, Value) {
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("GET")
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status().as_u16();
    let body = axum::body::to_bytes(res.into_body(), 2 * 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn test_app() -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.db")).unwrap();
    let app = router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    });
    (app, dir)
}

/// The endpoint answers, names every documented job, and — because this test
/// process spawns none of them — reports them as NOT RUNNING rather than as
/// healthy. That inversion is the point: the failure mode being defended
/// against is a job that is not running while everything looks fine.
#[tokio::test]
async fn system_jobs_lists_every_documented_job_and_reports_unspawned_ones() {
    let (app, _dir) = test_app();
    let (status, body) = get_json(&app, "/api/system-jobs").await;
    assert_eq!(status, 200, "body: {body}");

    let jobs = body["jobs"].as_array().expect("jobs array");
    for d in CATALOG {
        let j = jobs
            .iter()
            .find(|j| j["id"] == d.id)
            .unwrap_or_else(|| panic!("{} documented but absent from the payload", d.id));
        assert_eq!(j["purpose"], d.purpose, "{}: purpose must reach the client", d.id);
        assert_eq!(j["documented"], true);
        // Nothing was spawned in this process, so every one of them must say
        // so. If this ever comes back "ok", the endpoint is inventing health.
        let st = j["status"].as_str().unwrap_or("");
        assert!(
            st == "not_spawned" || st == "disabled",
            "{} reports {st:?} in a process that spawned nothing",
            d.id
        );
    }
    assert!(body["unhealthy"].as_u64().unwrap_or(0) > 0, "unspawned jobs must count as unhealthy");
    // No mutation verbs: these are machinery, not user data.
    assert!(body.get("run_now").is_none());
}

/// A spawned job shows up as running, with the interval and tick count the
/// SPAWNER recorded — not one the job self-reported. Deliberately spawned
/// under an id NO catalog row claims, which proves the other half at the same
/// time: an undocumented job is rendered (flagged, with a null purpose), never
/// hidden. Hiding it is how a rogue loop stays invisible.
///
/// The id is unique to this test because the registry is process-global and
/// `cargo test` runs these concurrently — reusing a catalog id here made the
/// "nothing is spawned" test above see a live job and fail.
#[tokio::test]
async fn a_spawned_periodic_job_reports_ok_with_real_ticks() {
    const ID: &str = "test-only-undocumented-job";
    assert!(!CATALOG.iter().any(|d| d.id == ID), "this test needs an id with no doc row");
    let (app, _dir) = test_app();
    let t = amux_server::runtime_jobs::spawn_periodic_every(
        ID,
        std::time::Duration::from_millis(20),
        || async {},
    );
    tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    let (status, body) = get_json(&app, "/api/system-jobs").await;
    assert_eq!(status, 200);
    let j = body["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == ID)
        .cloned()
        .expect("a job nobody documented must still be listed, not hidden");
    assert_eq!(j["status"], "ok", "{j}");
    assert_eq!(j["kind"], "periodic");
    assert_eq!(j["documented"], false);
    assert!(j["purpose"].is_null(), "an undocumented job's purpose is a visible blank");
    assert!(j["ticks"].as_u64().unwrap_or(0) >= 2, "{j}");
    assert!(j["last_tick_age_s"].as_f64().unwrap_or(1e9) < 1.0, "{j}");
    assert!(j["instrumented"].as_bool().unwrap_or(false));
    t.abort();
}

/// Control affordances must be honest: exactly one live switch (the autofix
/// pref, which the server re-reads every tick) and env vars as READOUTS. A
/// toggle over a startup-time env var would claim an effect it cannot have.
#[tokio::test]
async fn only_prefs_are_editable_env_controls_are_readouts() {
    let (app, _dir) = test_app();
    let (_, body) = get_json(&app, "/api/system-jobs").await;
    // READ THE FIELDS THE API ACTUALLY EMITS. This walked `j["control"]`, which
    // has never existed: `Doc` carries `env` (a LIST of EnvControl) and `pref`
    // (an Option<PrefControl>) as separate fields, and the response mirrors
    // that. Indexing a missing key yields Null, `Null["kind"]` is Null, so every
    // arm fell through to `_ => {}` and `editable` came back empty — the test
    // could only ever fail, and did, from the commit that introduced it.
    //
    // Worth naming because it cost me a wrong diagnosis on the way here: I first
    // probed the live endpoint for a `control` key, got None on every job, and
    // concluded the field "was not being populated". The field was never there;
    // I had searched for the name the TEST used instead of the name the API
    // uses. Naming the target before searching for it is the cheap version of
    // this check — `sorted(job.keys())` answered it in one call.
    let mut editable = Vec::new();
    for j in body["jobs"].as_array().unwrap() {
        for e in j["env"].as_array().into_iter().flatten() {
            assert_eq!(e["editable"], false, "{}: env control claims to be editable", j["id"]);
            assert!(e["var"].is_string(), "{}: env control must name its var", j["id"]);
        }
        let p = &j["pref"];
        if p.get("kind").and_then(|k| k.as_str()) == Some("pref") {
            assert_eq!(p["editable"], true, "{}: a pref control must be editable", j["id"]);
            assert!(p["key"].is_string(), "{}: a pref control must name its key", j["id"]);
            editable.push(j["id"].as_str().unwrap_or("").to_string());
        }
    }
    assert_eq!(
        editable,
        vec!["autofix".to_string()],
        "the only live toggle should be the autofix pref that already exists in Settings"
    );
}
