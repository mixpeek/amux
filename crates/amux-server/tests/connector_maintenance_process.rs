//! Actual scheduled upkeep against an HTTP provider, with child SIGKILL/restart.
use amux_server::{
    api::connectors::ConnectorsCtx,
    integrations::email::{HttpTransport, ReqwestTransport},
};
use axum::{
    Json,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::any,
};
use serde_json::{Value, json};
use std::future::IntoFuture;
use std::{
    path::PathBuf,
    process::{Child, Command},
    sync::{Arc, Mutex},
    time::Duration,
};

struct Wire {
    origin: String,
    http: ReqwestTransport,
}
#[async_trait::async_trait]
impl HttpTransport for Wire {
    async fn get(&self, u: &str, b: Option<&str>) -> Result<(u16, Value), String> {
        let target = if u == "https://slack.com/api/auth.test" {
            format!("{}/slack", self.origin)
        } else {
            assert!(u.starts_with(&self.origin));
            u.into()
        };
        self.http.get(&target, b).await
    }
    async fn post_json(&self, _: &str, _: Option<&str>, _: &Value) -> Result<(u16, Value), String> {
        Err("unexpected POST JSON".into())
    }
    async fn post_form(&self, u: &str, f: &[(String, String)]) -> Result<(u16, Value), String> {
        assert!(u.starts_with(&self.origin));
        self.http.post_form(u, f).await
    }
}
#[tokio::test]
async fn connector_maintenance_child() {
    let Ok(home) = std::env::var("AMUX_MAINTENANCE_CHILD_HOME") else {
        return;
    };
    let origin = std::env::var("AMUX_MAINTENANCE_PROVIDER").unwrap();
    let ctx = Arc::new(ConnectorsCtx {
        home: home.into(),
        http: Arc::new(Wire {
            origin,
            http: ReqwestTransport::new(),
        }),
    });
    let _job = amux_server::runtime_jobs::connector_maintenance::spawn_with(ctx, 1);
    std::future::pending::<()>().await;
}
#[derive(Default)]
struct Provider {
    refreshes: Vec<String>,
    reject: bool,
}
async fn provider(
    State(state): State<Arc<Mutex<Provider>>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    axum::extract::Form(form): axum::extract::Form<std::collections::BTreeMap<String, String>>,
) -> axum::response::Response {
    let mut state = state.lock().unwrap();
    if uri.path() == "/token" {
        let client = form.get("client_id").unwrap();
        assert_eq!(form.get("refresh_token").unwrap(), &format!("old-{client}"));
        state.refreshes.push(client.clone());
        if state.reject {
            return(StatusCode::BAD_REQUEST,Json(json!({"error":"invalid_grant","error_description":"old-secret echoed by fixture"}))).into_response();
        }
        return Json(json!({"access_token":format!("new-{client}"),"refresh_token":format!("rotated-{client}"),"expires_in":3600})).into_response();
    }
    let bearer = headers.get("authorization").unwrap().to_str().unwrap();
    assert!(matches!(
        bearer,
        "Bearer new-slack" | "Bearer new-fixture-oauth" | "Bearer unique-key-credential"
    ));
    Json(json!({"ok":true})).into_response()
}
struct Rig {
    child: Option<Child>,
    binary: PathBuf,
    home: PathBuf,
    provider: String,
}
impl Rig {
    fn start(&mut self) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.join("child.log"))
            .unwrap();
        let child = Command::new(&self.binary)
            .args(["--exact", "connector_maintenance_child", "--nocapture"])
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap())
            .env("AMUX_MAINTENANCE_CHILD_HOME", &self.home)
            .env("AMUX_MAINTENANCE_PROVIDER", &self.provider)
            .env("AMUX_HOME", &self.home)
            .env("AMUX_AUTOFIX_SECS", "0")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.child = Some(child);
    }
    fn kill(&mut self) {
        let mut child = self.child.take().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
    async fn receipt(&self, after: f64) -> Value {
        for _ in 0..200 {
            if let Ok(raw) = std::fs::read(self.home.join("connectors/maintenance.json")) {
                if let Ok(v) = serde_json::from_slice::<Value>(&raw) {
                    if v["checked_at"].as_f64().is_some_and(|t| t > after) {
                        return v;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "no scheduled durable receipt; {}",
            std::fs::read_to_string(self.home.join("child.log")).unwrap()
        );
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.kill()
        }
    }
}

#[tokio::test]
async fn scheduled_connector_maintenance_rotates_idle_grants_and_survives_sigkill() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(Mutex::new(Provider::default()));
    let server = tokio::spawn(
        axum::serve(
            listener,
            axum::Router::new()
                .fallback(any(provider))
                .with_state(state.clone()),
        )
        .into_future(),
    );
    let home = tempfile::tempdir().unwrap();
    let definitions = json!([{"id":"fixture-oauth","label":"Beth only","kind":"oauth2","test_url":format!("{origin}/canary")},{"id":"fixture-key","label":"Key fixture","kind":"api_key","key_env":"FIXTURE_KEY","test_url":format!("{origin}/key")}]);
    std::fs::create_dir_all(home.path().join("connectors")).unwrap();
    std::fs::write(
        home.path().join("connectors/custom.json"),
        definitions.to_string(),
    )
    .unwrap();
    std::fs::write(
        home.path().join("server.env"),
        "FIXTURE_KEY=unique-key-credential\nOWNER_UNRELATED=stable\n",
    )
    .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    for family in ["google", "slack", "fixture-oauth"] {
        let path = home.path().join("connectors").join(family);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("Beth.json"),json!({"token":format!("old-access-{family}"),"refresh_token":format!("old-{family}"),"expires_at":now+300.0,"token_uri":format!("{origin}/token"),"client_id":family,"client_secret":"fixture-client-secret","owner_metadata":{"organization":"Beth","pin":"unchanged"}}).to_string()).unwrap();
    }
    let binary = home.path().join("maintenance-test-binary");
    std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
    use sha2::Digest;
    let binary_hash = sha2::Sha256::digest(std::fs::read(&binary).unwrap());
    let mut rig = Rig {
        child: None,
        binary: binary.clone(),
        home: home.path().into(),
        provider: origin,
    };
    rig.start();
    let first = rig.receipt(0.0).await;
    assert_eq!(first["durable"], true);
    assert_eq!(first["failures"], 0);
    assert_eq!(state.lock().unwrap().refreshes.len(), 3);
    assert_eq!(first["api_keys"][0]["ok"], true);
    for family in ["google", "slack", "fixture-oauth"] {
        let v: Value = serde_json::from_slice(
            &std::fs::read(home.path().join(format!("connectors/{family}/Beth.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(v["refresh_token"], format!("rotated-{family}"));
        assert_eq!(v["owner_metadata"]["pin"], "unchanged");
    }
    rig.kill();
    rig.start();
    let second = rig.receipt(first["checked_at"].as_f64().unwrap()).await;
    assert_eq!(second["durable"], true);
    assert_eq!(
        state.lock().unwrap().refreshes.len(),
        3,
        "restart must reuse committed rotations"
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("connectors/custom.json")).unwrap(),
        definitions.to_string()
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("server.env")).unwrap(),
        "FIXTURE_KEY=unique-key-credential\nOWNER_UNRELATED=stable\n"
    );
    rig.kill();
    // Explicit disconnect must remain disconnected when the scheduled job restarts.
    let custom = home.path().join("connectors/fixture-oauth/Beth.json");
    std::fs::write(&custom, "{\"disconnected\":true}").unwrap();
    let before = second["checked_at"].as_f64().unwrap();
    rig.start();
    rig.receipt(before).await;
    rig.kill();
    assert_eq!(
        std::fs::read_to_string(custom).unwrap(),
        "{\"disconnected\":true}"
    );
    assert_eq!(state.lock().unwrap().refreshes.len(), 3);
    for secret in [
        "unique-key-credential",
        "fixture-client-secret",
        "old-google",
        "rotated-google",
        "new-google",
    ] {
        let receipt =
            std::fs::read_to_string(home.path().join("connectors/maintenance.json")).unwrap();
        assert!(!receipt.contains(secret), "receipt exposed credential");
    }
    // Real provider revocation is not a reason to replace or erase the grant.
    let google_path = home.path().join("connectors/google/Beth.json");
    let mut revoked: Value = serde_json::from_slice(&std::fs::read(&google_path).unwrap()).unwrap();
    revoked["expires_at"] = json!(0);
    revoked["refresh_token"] = json!("old-google");
    let before_bytes = revoked.to_string();
    std::fs::write(&google_path, &before_bytes).unwrap();
    state.lock().unwrap().reject = true;
    let last: Value = serde_json::from_slice(
        &std::fs::read(home.path().join("connectors/maintenance.json")).unwrap(),
    )
    .unwrap();
    rig.start();
    let failed = rig.receipt(last["checked_at"].as_f64().unwrap()).await;
    rig.kill();
    assert_eq!(failed["grants"][0]["status"], "needs_reauth");
    assert!(failed["failures"].as_u64().unwrap() > 0);
    assert_eq!(std::fs::read_to_string(&google_path).unwrap(), before_bytes);
    assert!(!failed.to_string().contains("old-secret"));
    state.lock().unwrap().reject = false;
    rig.start();
    let recovered = rig.receipt(failed["checked_at"].as_f64().unwrap()).await;
    rig.kill();
    assert_eq!(recovered["grants"][0]["status"], "ok");
    assert_eq!(recovered["failures"], 0);
    assert_eq!(
        std::fs::read_to_string(home.path().join("connectors/custom.json")).unwrap(),
        definitions.to_string()
    );
    println!(
        "PASS scheduled upkeep: 3 idle renewable families, real HTTP, early rotation, immutable owner configuration, independent autofix setting, SIGKILL/restart reuse, disconnect preserved"
    );
    assert_eq!(
        sha2::Sha256::digest(std::fs::read(&binary).unwrap()),
        binary_hash,
        "private child executable changed across restart"
    );
    server.abort();
}
