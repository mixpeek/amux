//! Retained worker evidence and integration gates. Project execution owns new work.
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

struct Rig {
    app: axum::Router,
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    _env: tokio::sync::MutexGuard<'static, ()>,
}

async fn rig() -> Rig {
    // AMUX_HOME is process-global. Hold one async lock for the entire fixture,
    // including requests and assertions, so no test borrows a peer's home.
    static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let guard = ENV.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("amux-home");
    std::fs::create_dir_all(home.join("sessions")).unwrap();
    unsafe { std::env::set_var("AMUX_HOME", &home) };
    unsafe { std::env::set_var("AMUX_DONE_LINK_REQUIRED", "0") };
    unsafe { std::env::set_var("AMUX_DONE_EVIDENCE_REQUIRED", "0") };
    unsafe { std::env::set_var("AMUX_APPROVAL_TYPES", "*") };

    let store = Store::open(&dir.path().join("fan-out-test.db")).unwrap();
    let state = AppState {
        store: Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "fan-out-e2e".into(),
        auth_token: None,
        reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    Rig {
        app: router(state),
        _dir: dir,
        home,
        _env: guard,
    }
}

async fn send(
    app: &axum::Router,
    method: &str,
    path: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(value) => builder
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, headers, value)
}

async fn create(app: &axum::Router, body: Value) -> Value {
    let (st, _, v) = send(app, "POST", "/api/board", Some(body), &[]).await;
    assert_eq!(st, StatusCode::CREATED, "create failed: {v}");
    v
}

async fn get_card(app: &axum::Router, id: &str) -> Value {
    let (st, _, v) = send(app, "GET", &format!("/api/board/{id}"), None, &[]).await;
    assert_eq!(st, StatusCode::OK, "get card failed: {v}");
    v
}

fn write_parent_env(home: &std::path::Path, name: &str) {
    let sessions = home.join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    let env_path = sessions.join(format!("{name}.env"));
    std::fs::write(
        &env_path,
        format!("CC_DIR=/tmp/test-{name}\nCC_PROVIDER=claude\n"),
    )
    .unwrap();
}

#[tokio::test]
async fn orchestration_projection_and_integration_configuration_use_public_routes() {
    let r = rig().await;
    write_parent_env(&r.home, "parent");
    write_parent_env(&r.home, "child");
    std::fs::write(
        r.home.join("sessions/child.env"),
        "CC_DIR=/tmp\nCC_EPHEMERAL=1\nCC_PARENT=parent\n",
    )
    .unwrap();
    let epic = create(
        &r.app,
        json!({"title":"Integration epic","type":"epic","session":"parent"}),
    )
    .await;
    let assignment = create(
        &r.app,
        json!({"title":"Assigned outcome","session":"child","epic":epic["id"]}),
    )
    .await;
    let (linked, _, assignment) = send(
        &r.app,
        "PATCH",
        &format!("/api/board/{}", assignment["id"].as_str().unwrap()),
        Some(json!({"epic":epic["id"]})),
        &[],
    )
    .await;
    assert_eq!(linked, StatusCode::OK, "{assignment}");
    assert_eq!(assignment["epic"], epic["id"]);
    let followup = create(
        &r.app,
        json!({"title":"Whole-board follow-up","session":"child","desc":"long private history"}),
    )
    .await;
    let unrelated = create(
        &r.app,
        json!({"title":"Ordinary epic outside fanout","session":"parent","type":"epic"}),
    )
    .await;
    let (st, _, v) = send(&r.app, "GET", "/api/board/orchestrations", None, &[]).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["measured"], true);
    let cards = v["cards"].as_array().unwrap();
    assert!(cards.iter().any(|c| c["id"] == epic["id"]));
    assert!(cards.iter().any(|c| c["id"] == assignment["id"]));
    assert!(cards.iter().any(|c| c["id"] == followup["id"]));
    assert!(!cards.iter().any(|c| c["id"] == unrelated["id"]));
    assert!(v["workers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["name"] == "parent"
            && w["role"] == "orchestrator"
            && w["orchestrator"] == false));
    assert_eq!(v["n_excluded_unrelated"], 1);
    assert!(!v.to_string().contains("long private history"));
    let (st, _, v) = send(
        &r.app,
        "PATCH",
        "/api/sessions/child/config",
        Some(json!({"worktree_verify":"npm test"})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        amux_server::config::parse_env_file(&r.home.join("sessions/child.env"))
            .get("CC_WORKTREE_VERIFY")
            .map(String::as_str),
        Some("npm test")
    );
    let (st, _, _) = send(
        &r.app,
        "PATCH",
        "/api/sessions/child/config",
        Some(json!({"worktree_verify":""})),
        &[],
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _, _) = send(
        &r.app,
        "PATCH",
        "/api/sessions/child/config",
        Some(json!({"worktree_base":"HEAD"})),
        &[],
    )
    .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "legacy adoption requires an exact reviewed commit"
    );
}

#[tokio::test]
async fn verified_requires_the_current_clean_fanout_head_to_be_integrated() {
    let r = rig().await;
    let name = "verified-child";
    std::fs::write(
        r.home.join("sessions").join(format!("{name}.env")),
        "CC_EPHEMERAL=1\n",
    )
    .unwrap();
    let item = create(&r.app, json!({"title":"Publish tested result", "session":name, "status":"done", "type":"code", "evidence":"result.txt tested in candidate"})).await;
    let id = item["id"].as_str().unwrap();
    let path = format!("/api/board/{id}");
    let (_, _, contract) = send(
        &r.app,
        "GET",
        &format!("/api/board/contract?card={id}"),
        None,
        &[],
    )
    .await;
    let checked = contract["card_effective_gates"]["gates"]["verified"].clone();
    assert!(checked.as_array().is_some_and(|a| !a.is_empty()));
    for force in [false, true] {
        let (status, _, refusal) = send(&r.app, "PATCH", &path,
            Some(json!({"status":"verified", "gate_checked":checked,"force":force,"reason":"test that force cannot invent integration"})),
            &[("x-amux-session",name)]).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
        assert_eq!(refusal["code"], "fanout_verification_requires_integration");
        assert_eq!(get_card(&r.app, id).await["status"], "done");
    }
    let (status,_,refusal)=send(&r.app,"POST","/api/board",Some(json!({"title":"Created as verified", "session":name,"status":"verified","type":"code"})),&[]).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refusal}");
    assert_eq!(refusal["code"], "fanout_verification_requires_integration");

    fn git(path: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }
    let repo = r._dir.path().join("repo");
    let remote = r._dir.path().join("remote.git");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&remote, &["init", "--bare"]);
    git(&repo, &["config", "user.email", "fixture@example.test"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(repo.join("base.txt"), "base\n").unwrap();
    git(&repo, &["add", "base.txt"]);
    git(&repo, &["commit", "-m", "base"]);
    git(
        &repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&repo, &["push", "origin", "HEAD:main"]);
    let workspace = amux_server::fanout_workspace::ensure(&r.home, name, repo.to_str().unwrap())
        .await
        .unwrap();
    let work = std::path::Path::new(&workspace.path);
    std::fs::write(work.join("result.txt"), "verified fixture\n").unwrap();
    git(work, &["add", "result.txt"]);
    git(work, &["commit", "-m", "result"]);
    let head = git(work, &["rev-parse", "HEAD"]);
    let (status, _, _) = send(
        &r.app,
        "PATCH",
        &path,
        Some(json!({"status":"verified","gate_checked":checked})),
        &[("x-amux-session", name)],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a real workspace without integration still refuses"
    );
    amux_server::fanout_workspace::integrate(&workspace, "test -f result.txt", || Ok(()))
        .await
        .unwrap();
    std::fs::write(
        r.home
            .join("workspaces")
            .join(format!("{name}.integration.json")),
        json!({"status":"integrated","head":head}).to_string(),
    )
    .unwrap();
    let (status, _, body) = send(
        &r.app,
        "PATCH",
        &path,
        Some(json!({"status":"verified","gate_checked":checked})),
        &[("x-amux-session", name)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "verified");

    std::fs::write(work.join("result.txt"), "changed after verification\n").unwrap();
    let (status, _, body) = send(
        &r.app,
        "PATCH",
        &path,
        Some(json!({"status":"verified","reverify":true,"gate_checked":checked})),
        &[("x-amux-session", name)],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "dirty current work cannot reuse the old receipt: {body}"
    );
    git(work, &["add", "result.txt"]);
    git(work, &["commit", "-m", "next outcome"]);
    let (status, _, body) = send(
        &r.app,
        "PATCH",
        &path,
        Some(json!({"status":"verified","reverify":true,"gate_checked":checked})),
        &[("x-amux-session", name)],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "a new head cannot reuse the old receipt: {body}"
    );
    assert_eq!(body["code"], "fanout_verification_requires_integration");
    let ordinary=create(&r.app,json!({"title":"Ordinary worker outcome", "session":"ordinary","status":"verified","type":"research"})).await;
    assert_eq!(
        ordinary["status"], "verified",
        "ordinary worker gates remain independent of fan-out integration"
    );
}

#[tokio::test]
async fn verification_owner_observation_matches_nullable_patch_semantics() {
    let r = rig().await;
    // Unassigned cards are submitted as session:"" by the dashboard. Explicit
    // clearing and a retained owner must agree with the transaction too.
    for (original, patch, expected) in [
        (Value::Null, json!(""), Value::Null),
        (Value::Null, json!("   "), Value::Null),
        (json!("ordinary"), Value::Null, Value::Null),
        (json!("ordinary"), json!("ordinary"), json!("ordinary")),
    ] {
        let card = create(
            &r.app,
            json!({"title":"Verify nullable owner", "type":"chore",
            "session":original, "gate":["Fixture output checked"]}),
        )
        .await;
        let path = format!("/api/board/{}", card["id"].as_str().unwrap());
        let (status, _, body) = send(
            &r.app,
            "PATCH",
            &path,
            Some(json!({"status":"verified", "session":patch,
                "gate_checked":["Fixture output checked"]})),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "verified", "{body}");
        assert_eq!(body["session"], expected, "{body}");
    }
}

#[tokio::test]
async fn retired_launch_paths_cannot_spawn_workers() {
    let r = rig().await;
    for path in ["/api/board/launch", "/api/board/example/fan-out"] {
        let (status, _, _) = send(
            &r.app,
            "POST",
            path,
            Some(json!({"priorities":["Do work"]})),
            &[],
        )
        .await;
        assert!(
            matches!(
                status,
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "{path}: {status}"
        );
    }
    assert_eq!(
        std::fs::read_dir(r.home.join("sessions")).unwrap().count(),
        0
    );
}
