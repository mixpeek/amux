//! GET /api/file/history: a file's commits and the file as it was in one
//! (the markdown editor's History tab, owner 2026-10-29).

use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::process::Command;
use std::sync::Arc;
use tower::ServiceExt;

fn state(dir: &std::path::Path) -> AppState {
    AppState {
        store: Arc::new(Store::open(&dir.join("amux-test.db")).unwrap()),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    }
}

async fn get(dir: &std::path::Path, uri: String) -> (StatusCode, Value) {
    let res = router(state(dir)).oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let v: Value = serde_json::from_slice(&axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap()).unwrap();
    (status, v)
}

/// git in `repo` with a fixed identity, so commits work on any machine.
fn git(repo: &std::path::Path, args: &[&str]) {
    let ok = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=Tester", "-c", "user.email=tester@localhost", "-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
}

fn enc(p: &std::path::Path) -> String {
    p.display().to_string().replace(' ', "%20")
}

#[tokio::test]
async fn a_files_commits_newest_first_and_the_file_as_it_was_in_one() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("notes")).unwrap();
    git(&repo, &["init", "-q"]);
    let f = repo.join("notes/todo.md");
    std::fs::write(&f, "# To-Do\n\n- first\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "start the list"]);
    std::fs::write(&f, "# To-Do\n\n- first\n- second\n").unwrap();
    git(&repo, &["commit", "-qam", "add a second item"]);

    let (st, v) = get(dir.path(), format!("/api/file/history?path={}", enc(&f))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["measured"], true);
    assert_eq!(v["in_repo"], true);
    assert_eq!(v["n_considered"], 2);
    assert_eq!(v["commits"][0]["subject"], "add a second item");
    assert_eq!(v["commits"][1]["subject"], "start the list");
    assert_eq!(v["commits"][0]["author"], "Tester");
    assert_eq!(v["uncommitted"], false);

    // The first version, by its commit.
    let first = v["commits"][1]["sha"].as_str().unwrap().to_string();
    let (st, old) = get(dir.path(), format!("/api/file/history?path={}&sha={first}", enc(&f))).await;
    assert_eq!(st, StatusCode::OK, "{old}");
    assert_eq!(old["content"], "# To-Do\n\n- first\n");

    // An edit not committed yet is said.
    std::fs::write(&f, "# To-Do\n\n- first\n- second\n- third\n").unwrap();
    let (_, v) = get(dir.path(), format!("/api/file/history?path={}", enc(&f))).await;
    assert_eq!(v["uncommitted"], true);

    // Not a commit id: refused, not handed to git.
    let (st, _) = get(dir.path(), format!("/api/file/history?path={}&sha=HEAD;rm", enc(&f))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_file_outside_any_repository_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("loose.md");
    std::fs::write(&f, "# Loose\n").unwrap();
    let (st, v) = get(dir.path(), format!("/api/file/history?path={}", enc(&f))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["measured"], true);
    assert_eq!(v["in_repo"], false);
    assert_eq!(v["commits"], serde_json::json!([]));
}
