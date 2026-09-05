//! ATE-39 — commit output belongs to the task the commit names, not whichever
//! task happened to be updated most recently for that worker.
//!
//! Live specimen: commit `be87f031` named `(ATE-38)` in its subject while
//! ATE-39 was Doing. The post-commit report put both the commit and
//! `sessions_legacy.rs` onto ATE-39 and left ATE-38 empty. This suite builds a
//! real git commit with that shape and verifies the durable artifact rows.

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use serde_json::{json, Value};
use std::process::Command;
use tower::ServiceExt;

static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();

fn home() -> &'static std::path::Path {
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("AMUX_HOME", dir.path());
        dir
    })
    .path()
}

fn app(lane: &str, repo: &std::path::Path) -> axum::Router {
    let fleet = home();
    std::fs::create_dir_all(fleet.join("sessions")).unwrap();
    std::fs::write(
        fleet.join("sessions").join(format!("{lane}.env")),
        format!("CC_DIR=\"{}\"\nCC_PROVIDER=\"codex\"\n", repo.display()),
    )
    .unwrap();
    let store = Store::open(&fleet.join(format!("{lane}.sqlite"))).unwrap();
    router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

async fn request(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    lane: &str,
) -> (StatusCode, Value) {
    let builder = Request::builder()
        .method(method)
        .uri(path)
        .header("X-Amux-Session", lane);
    let req = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, value)
}

async fn create_card(app: &axum::Router, lane: &str, title: &str, status: &str) -> String {
    let (http, body) = request(
        app,
        "POST",
        "/api/board",
        Some(json!({
            "title": title,
            "status": status,
            "type": "chore",
            "session": lane,
        })),
        lane,
    )
    .await;
    assert_eq!(http, StatusCode::CREATED, "card create failed: {body}");
    body["id"].as_str().unwrap().to_string()
}

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn init_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.name", "ATE-39 Test"]);
    git(repo.path(), &["config", "user.email", "ate39@example.test"]);
    repo
}

#[tokio::test]
async fn explicit_subject_task_gets_commit_and_file_rows_not_newest_doing_task() {
    let repo = init_repo();
    let lane = "ate39-commit-exact";
    let app = app(lane, repo.path());
    let exact = create_card(&app, lane, "task named by commit", "todo").await;
    let wrong = create_card(&app, lane, "newest current task", "doing").await;

    let dotenv = repo.path().join("customers/tubescience/.env");
    let source = repo.path().join("src/output.rs");
    std::fs::create_dir_all(dotenv.parent().unwrap()).unwrap();
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&dotenv, "ATE39_CAPTURE=1\n").unwrap();
    std::fs::write(&source, "pub const CAPTURED: bool = true;\n").unwrap();
    git(
        repo.path(),
        &["add", "-f", "customers/tubescience/.env", "src/output.rs"],
    );
    git(repo.path(), &["commit", "-q", "-m", &format!("capture outputs ({exact})")]);
    let full_sha = git(repo.path(), &["rev-parse", "HEAD"]);

    let (status, report) = request(
        &app,
        "POST",
        &format!("/api/sessions/{lane}/commit-report"),
        Some(json!({
            "sha": &full_sha[..8],
            "subject": "truncated hook text without a task id",
            "dir": repo.path(),
        })),
        lane,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "commit report failed: {report}");
    assert_eq!(report["attached"], json!(exact));
    assert_eq!(report["task_source"], json!("subject"));
    assert_eq!(report["artifact_scan"]["measured"], json!(true));
    assert_eq!(report["artifact_scan"]["n_considered"], json!(2));

    let (_, exact_detail) = request(&app, "GET", &format!("/api/board/{exact}"), None, lane).await;
    let refs = exact_detail["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|artifact| artifact["ref"].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(refs.len(), 3, "files and full commit must be durable rows: {exact_detail}");
    assert!(refs.contains("customers/tubescience/.env"));
    assert!(refs.contains("src/output.rs"));
    assert!(refs.contains(full_sha.as_str()));
    assert!(
        exact_detail["log"].as_str().unwrap_or("").contains(&full_sha),
        "the exact task must retain the commit activity: {exact_detail}"
    );
    let resolved = exact_detail["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["ref"] == json!("customers/tubescience/.env"))
        .unwrap()["resolved_ref"]
        .as_str()
        .unwrap();
    assert_eq!(
        std::path::Path::new(resolved).canonicalize().unwrap(),
        dotenv.canonicalize().unwrap(),
        "relative files resolve against the producing worker directory"
    );

    let (_, wrong_detail) = request(&app, "GET", &format!("/api/board/{wrong}"), None, lane).await;
    assert_eq!(wrong_detail["artifacts"], json!([]), "newest Doing task stole outputs: {wrong_detail}");
    assert!(
        !wrong_detail["log"].as_str().unwrap_or("").contains(&full_sha),
        "the commit activity must stay off the guessed task: {wrong_detail}"
    );
}

#[tokio::test]
async fn no_explicit_task_with_multiple_current_cards_refuses_to_guess() {
    let repo = init_repo();
    let lane = "ate39-commit-ambiguous";
    let app = app(lane, repo.path());
    let first = create_card(&app, lane, "first current task", "doing").await;
    let second = create_card(&app, lane, "second current task", "doing").await;
    std::fs::write(repo.path().join("ambiguous.txt"), "no task id\n").unwrap();
    git(repo.path(), &["add", "ambiguous.txt"]);
    git(repo.path(), &["commit", "-q", "-m", "capture ambiguous output"]);
    let sha = git(repo.path(), &["rev-parse", "HEAD"]);

    let (status, report) = request(
        &app,
        "POST",
        &format!("/api/sessions/{lane}/commit-report"),
        Some(json!({"sha": sha, "subject": "capture ambiguous output", "dir": repo.path()})),
        lane,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "ambiguity must not silently attach: {report}");
    assert_eq!(report["code"], json!("commit_task_ambiguous"));
    for id in [first, second] {
        let (_, detail) = request(&app, "GET", &format!("/api/board/{id}"), None, lane).await;
        assert_eq!(detail["artifacts"], json!([]), "refused report still wrote to {id}: {detail}");
    }
}
