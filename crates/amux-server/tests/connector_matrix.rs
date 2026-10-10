//! Every built-in connector through the production HTTP router and durable store.
//! Only provider URLs are translated to a loopback HTTP fixture, which validates
//! credentials, PKCE, redirects and rotation. The Amux server runs in a child
//! process so SIGKILL/restart proves storage rather than graceful shutdown.
use amux_server::{
    api::{router_with_connector_transport, AppState},
    db::Store,
    integrations::email::{HttpTransport, ReqwestTransport},
};
use axum::{
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use serde_json::{json, Value};
use std::future::IntoFuture;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::{Child, Command},
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::{Duration, Instant},
};

struct Wire {
    origin: String,
    http: ReqwestTransport,
}
impl Wire {
    fn url(&self, url: &str) -> String {
        let u = reqwest::Url::parse(url).unwrap();
        // An allowlist prevents an unexpected provider from reaching the network.
        assert!(matches!(
            u.host_str(),
            Some(
                "accounts.google.com"
                    | "oauth2.googleapis.com"
                    | "www.googleapis.com"
                    | "gmail.googleapis.com"
                    | "admin.googleapis.com"
                    | "slack.com"
                    | "api.telegram.org"
                    | "public-api.granola.ai"
                    | "127.0.0.1"
            )
        ));
        format!(
            "{}/{}{}{}",
            self.origin,
            u.host_str().unwrap(),
            u.path(),
            u.query().map(|q| format!("?{q}")).unwrap_or_default()
        )
    }
}
#[async_trait::async_trait]
impl HttpTransport for Wire {
    async fn get(&self, u: &str, b: Option<&str>) -> Result<(u16, Value), String> {
        self.http.get(&self.url(u), b).await
    }
    async fn post_json(&self, u: &str, b: Option<&str>, v: &Value) -> Result<(u16, Value), String> {
        self.http.post_json(&self.url(u), b, v).await
    }
    async fn post_form(&self, u: &str, v: &[(String, String)]) -> Result<(u16, Value), String> {
        self.http.post_form(&self.url(u), v).await
    }
    async fn post_json_with_header(
        &self,
        u: &str,
        v: &Value,
        h: &str,
    ) -> Result<(u16, Value, Option<String>), String> {
        self.http.post_json_with_header(&self.url(u), v, h).await
    }
}

#[tokio::test]
async fn fixture_server_child() {
    let Ok(home) = std::env::var("AMUX_MATRIX_CHILD_HOME") else {
        return;
    };
    let port: u16 = std::env::var("AMUX_MATRIX_CHILD_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let state = AppState {
        store: Arc::new(Store::open(&PathBuf::from(home).join("test.db")).unwrap()),
        started: Instant::now(),
        build_hash: "connector-matrix-child".into(),
        auth_token: Some("matrix-owner".into()),
        reconciled: Arc::new(AtomicBool::new(true)),
    };
    let wire = Arc::new(Wire {
        origin: std::env::var("AMUX_MATRIX_PROVIDER").unwrap(),
        http: ReqwestTransport::new(),
    });
    let poll_wire = wire.clone();
    let poll_state = state.clone();
    let app = router_with_connector_transport(state, wire).route(
        "/matrix/poll",
        axum::routing::post(move || {
            let wire = poll_wire.clone();
            let state = poll_state.clone();
            async move {
                amux_server::runtime_jobs::telegram_poll::poll_once(
                    wire.as_ref(),
                    "matrix-bot",
                    &state,
                )
                .await
                .unwrap();
                Json(json!({"ok":true}))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .unwrap();
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .unwrap();
}

#[derive(Default)]
struct Provider {
    codes: BTreeMap<String, BTreeMap<String, String>>,
    refresh: BTreeMap<String, String>,
    revoked: Vec<String>,
    calls: BTreeMap<String, usize>,
    serial: usize,
    updates: Vec<Value>,
    sends: Vec<Value>,
    offsets: Vec<i64>,
    fail_format: bool,
    reject_send: bool,
    rsa_public: Vec<u8>,
    sa_audience: String,
    deny_delegation: bool,
    sa_claims: Vec<Value>,
}
type Fixture = Arc<Mutex<Provider>>;
async fn provider(
    State(s): State<Fixture>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use amux_server::integrations::email::base64url_nopad;
    use sha2::{Digest, Sha256};
    let path = uri.path();
    let mut s = s.lock().unwrap();
    *s.calls.entry(path.to_string()).or_default() += 1;
    let form: BTreeMap<String, String> = reqwest::Url::parse(&format!(
        "http://fixture/?{}",
        String::from_utf8_lossy(&body)
    ))
    .unwrap()
    .query_pairs()
    .into_owned()
    .collect();
    let bearer = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .strip_prefix("Bearer ")
        .unwrap_or("");
    let reply =
        |code: u16, v: Value| (StatusCode::from_u16(code).unwrap(), Json(v)).into_response();
    if path == "/approve" {
        let q: BTreeMap<String, String> = serde_json::from_slice(&body).unwrap();
        s.serial += 1;
        let code = format!("code-{}", s.serial);
        s.codes.insert(code.clone(), q);
        return reply(200, json!({"code":code}));
    }
    if path.ends_with("/o/oauth2/auth") {
        return reply(200, json!("synthetic consent"));
    }
    if path == "/sa/token" {
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_audience(&[s.sa_audience.clone()]);
        let key = jsonwebtoken::DecodingKey::from_rsa_pem(&s.rsa_public).unwrap();
        let Ok(decoded) =
            jsonwebtoken::decode::<Value>(form.get("assertion").unwrap(), &key, &validation)
        else {
            return reply(400, json!({"error":"invalid_grant"}));
        };
        assert_eq!(decoded.claims["iss"], "fixture-sa@example.test");
        assert!(decoded.claims["scope"]
            .as_str()
            .unwrap()
            .contains("https://www.googleapis.com/auth/"));
        s.sa_claims.push(decoded.claims.clone());
        if s.deny_delegation {
            return reply(400, json!({"error":"unauthorized_client"}));
        }
        return reply(
            200,
            json!({"access_token":format!("access-{}",decoded.claims["sub"].as_str().unwrap()),"expires_in":3600}),
        );
    }
    if path.ends_with("/getUpdates") {
        assert!(path.starts_with("/api.telegram.org/botmatrix-bot/"));
        let offset = uri
            .query()
            .and_then(|q| reqwest::Url::parse(&format!("http://fixture/?{q}")).ok())
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "offset")
            .unwrap()
            .1
            .parse::<i64>()
            .unwrap();
        s.offsets.push(offset);
        return reply(
            200,
            json!({"ok":true,"result":s.updates.iter().filter(|u|u["update_id"].as_i64().unwrap()>=offset).collect::<Vec<_>>()}),
        );
    }
    if path.ends_with("/sendMessage") {
        assert!(path.starts_with("/api.telegram.org/botmatrix-bot/"));
        let message: Value = serde_json::from_slice(&body).unwrap();
        if s.reject_send {
            return reply(
                200,
                json!({"ok":false,"description":"fixture rejected message"}),
            );
        }
        if s.fail_format && message["parse_mode"] == "HTML" {
            return reply(
                400,
                json!({"ok":false,"description":"can't parse entities"}),
            );
        }
        s.sends.push(message);
        return reply(
            200,
            json!({"ok":true,"result":{"message_id":s.sends.len()}}),
        );
    }
    if path.ends_with("/token") || path.ends_with("/oauth.v2.access") {
        let identity = if form.get("grant_type").map(String::as_str) == Some("authorization_code") {
            let Some(q) = form.get("code").and_then(|c| s.codes.remove(c)) else {
                return reply(400, json!({"error":"invalid_grant"}));
            };
            if form.get("redirect_uri") != q.get("redirect_uri")
                || form.get("client_id") != q.get("client_id")
                || form.get("client_secret").map(String::as_str) != Some("matrix-secret")
            {
                return reply(400, json!({"error":"invalid_client"}));
            }
            if let Some(challenge) = q.get("code_challenge") {
                let computed = base64url_nopad(&Sha256::digest(
                    form.get("code_verifier").unwrap().as_bytes(),
                ));
                if &computed != challenge {
                    return reply(400, json!({"error":"invalid_grant"}));
                }
            }
            let identity = q
                .get("login_hint")
                .or_else(|| q.get("fixture_account"))
                .unwrap()
                .clone();
            s.revoked.retain(|i| i != &identity);
            identity
        } else {
            let Some(identity) = form.get("refresh_token").and_then(|r| s.refresh.remove(r)) else {
                return reply(400, json!({"error":"invalid_grant"}));
            };
            if s.revoked.contains(&identity) {
                return reply(400, json!({"error":"invalid_grant"}));
            }
            identity
        };
        s.serial += 1;
        let refresh = format!("refresh-{}", s.serial);
        s.refresh.insert(refresh.clone(), identity.clone());
        return reply(
            200,
            json!({"ok":true,"access_token":format!("access-{identity}"),"refresh_token":refresh,"expires_in":3600,"scope":"https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/drive https://www.googleapis.com/auth/calendar https://www.googleapis.com/auth/admin.directory.user chat:write","team":{"name":identity}}),
        );
    }
    if path.ends_with("/users/login") {
        let login: Value = serde_json::from_slice(&body).unwrap();
        if login["password"] != "matrix-password" {
            return reply(401, json!({"message":"incorrect password"}));
        }
        s.revoked
            .retain(|i| i != login["login_id"].as_str().unwrap());
        let mut response = reply(200, json!({"id":"matrix-user"}));
        response.headers_mut().insert(
            "Token",
            format!("access-{}", login["login_id"].as_str().unwrap())
                .parse()
                .unwrap(),
        );
        return response;
    }
    let token = if path.contains("/bot") {
        if path.contains("/botmatrix-bot/") {
            "matrix-bot"
        } else {
            "invalid"
        }
    } else {
        bearer
    };
    let identity = token.strip_prefix("access-");
    let valid = token == "matrix-api-key"
        || token == "matrix-bot"
        || identity.is_some_and(|i| !s.revoked.contains(&i.to_string()));
    if path.ends_with("/auth.test") || path.ends_with("/getMe") {
        return reply(
            200,
            json!({"ok":valid,"error":if valid {Value::Null}else{json!("invalid_auth")},"result":{"username":"matrix_bot"}}),
        );
    }
    if !valid {
        return reply(401, json!({"error":"invalid_token", "echo":token}));
    }
    if path.ends_with("/userinfo") {
        return reply(200, json!({"email":identity.unwrap()}));
    }
    reply(
        200,
        json!({"fixture":true,"identity":identity,"notes":[],"items":[],"users":[],"emailAddress":identity,"user":{"emailAddress":identity},"records":if identity==Some("beth@example.test") {json!([{"id":"A","rev":1,"amount":110,"status":"billable"},{"id":"B","rev":1,"amount":200,"status":"billable"},{"id":"C","rev":1,"amount":45,"status":"billable"},{"id":"A","rev":2,"amount":80,"status":"billable"},{"id":"B","rev":2,"amount":200,"status":"void"},{"id":"C","rev":1,"amount":45,"status":"billable"},{"id":"D","rev":1,"amount":12,"status":"billable"}])}else {json!([{"id":"X","rev":1,"amount":9,"status":"billable"}])}}),
    )
}

struct Rig {
    home: PathBuf,
    binary: PathBuf,
    port: u16,
    provider: String,
    child: Option<Child>,
    client: reqwest::Client,
}
impl Rig {
    fn start(&mut self) {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.join("server.log"))
            .unwrap();
        let child = Command::new(&self.binary)
            .args(["--exact", "fixture_server_child", "--nocapture"])
            .env("AMUX_MATRIX_CHILD_HOME", &self.home)
            .env("AMUX_HOME", &self.home)
            .env("AMUX_RS_PORT", self.port.to_string())
            .env("AMUX_MATRIX_CHILD_PORT", self.port.to_string())
            .env("AMUX_MATRIX_PROVIDER", &self.provider)
            .env("AMUX_NO_SELF_ADOPT", "1")
            .env("AMUX_RS_NO_LOOPBACK_BYPASS", "1")
            .env_remove("AMUX_SESSION")
            .env_remove("TELEGRAM_BOT_TOKEN")
            .env_remove("GRANOLA_API_KEY")
            .env_remove("GOOGLE_OAUTH_CLIENT_ID")
            .env_remove("GOOGLE_OAUTH_CLIENT_SECRET")
            .env_remove("SLACK_CLIENT_ID")
            .env_remove("SLACK_CLIENT_SECRET")
            .env_remove("MATTERMOST_URL")
            .env_remove("MATTERMOST_LOGIN")
            .env_remove("MATTERMOST_PASSWORD")
            .env_remove("GOOGLE_SA_KEY_FILE")
            .env_remove("GOOGLE_SA_SUBJECT")
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
    async fn ready(&self) {
        for _ in 0..100 {
            if self
                .client
                .get(format!("http://127.0.0.1:{}/health", self.port))
                .send()
                .await
                .is_ok()
            {
                return;
            };
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("server did not start");
    }
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        v: Value,
        worker: bool,
    ) -> (u16, Value) {
        let mut r = self
            .client
            .request(method, format!("http://127.0.0.1:{}{path}", self.port))
            .bearer_auth("matrix-owner")
            .json(&v);
        if worker {
            r = r.header("X-Amux-Session", "matrix-worker");
        }
        let response = r.send().await.unwrap();
        let code = response.status().as_u16();
        let raw = response.text().await.unwrap();
        (code, serde_json::from_str(&raw).unwrap_or(json!(raw)))
    }
    async fn post(&self, path: &str, v: Value) -> (u16, Value) {
        self.call(reqwest::Method::POST, path, v, false).await
    }
    async fn grant(&self, id: &str, account: &str) {
        let (code, begin) = self
            .post(
                &format!("/api/connectors/{id}/auth?account={account}"),
                json!({}),
            )
            .await;
        assert_eq!(code, 200, "begin {id}");
        let u = reqwest::Url::parse(begin["authorize_url"].as_str().unwrap()).unwrap();
        let mut q: BTreeMap<String, String> = u.query_pairs().into_owned().collect();
        q.insert("fixture_account".into(), account.into());
        let approved: Value = self
            .client
            .post(format!("{}/approve", self.provider))
            .json(&q)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        // Exercise the registered Gmail callback, including delegation to the family broker.
        let callback = if id.starts_with("google-") {
            "/api/gmail/callback".to_string()
        } else {
            format!("/api/connectors/{id}/callback")
        };
        let path = format!(
            "{callback}?state={}&code={}",
            q["state"],
            approved["code"].as_str().unwrap()
        );
        let (code, _) = self
            .call(reqwest::Method::GET, &path, json!({}), false)
            .await;
        assert_eq!(code, 200, "callback {id}");
        assert_eq!(
            self.call(reqwest::Method::GET, &path, json!({}), false)
                .await
                .0,
            400,
            "callback replay {id}"
        );
    }
    async fn scope(&self, id: &str, account: &str, enabled: bool) {
        assert_eq!(self.call(reqwest::Method::PUT,"/api/scope",json!({"level":"worker","name":"matrix-worker","capability":"connectors","value":{"connectors":{id:{"enabled":enabled,"account":account}},"merge":true}}),false).await.0,200);
    }
}
impl Drop for Rig {
    fn drop(&mut self) {
        if self.child.is_some() {
            self.kill();
        }
    }
}

#[tokio::test]
async fn every_builtin_connector_lifecycle_over_http_and_sigkill() {
    let fixture: Fixture = Arc::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_origin = format!("http://{}", listener.local_addr().unwrap());
    let fixture_task = tokio::spawn(
        axum::serve(
            listener,
            axum::Router::new()
                .fallback(provider)
                .with_state(fixture.clone()),
        )
        .into_future(),
    );
    let tmp = tempfile::tempdir().unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let binary = tmp.path().join("immutable-fixture-server");
    std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
    use sha2::{Digest, Sha256};
    let binary_digest = Sha256::digest(std::fs::read(&binary).unwrap());
    println!("FIXTURE executable SHA256: {binary_digest:x}");
    let mut rig = Rig {
        home: tmp.path().into(),
        binary,
        port,
        provider: provider_origin,
        child: None,
        client: reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap(),
    };
    std::fs::create_dir_all(rig.home.join("sessions")).unwrap();
    std::fs::write(
        rig.home.join("sessions/matrix-worker.env"),
        "AMUX_ISOLATED=1\n",
    )
    .unwrap();
    rig.start();
    rig.ready().await;
    let ids = [
        "granola",
        "google-gmail",
        "google-calendar",
        "google-drive",
        "google-admin",
        "slack",
        "telegram",
        "mattermost",
    ];
    let (_, list) = rig
        .call(reqwest::Method::GET, "/api/connectors", json!({}), false)
        .await;
    let actual: Vec<_> = list["connectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(actual, ids, "inventory additions need flow coverage");
    assert_eq!(
        rig.client
            .get(format!("http://127.0.0.1:{port}/api/connectors"))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16(),
        401,
        "owner bearer required"
    );
    for id in ids {
        let (_, v) = rig
            .post(&format!("/api/connectors/{id}/test"), json!({}))
            .await;
        assert_eq!(v["ok"], false, "unconfigured {id}");
    }
    for (id, key, value) in [
        ("granola", "GRANOLA_API_KEY", "matrix-api-key"),
        ("telegram", "TELEGRAM_BOT_TOKEN", "matrix-bot"),
    ] {
        assert_eq!(
            rig.post(
                &format!("/api/connectors/{id}/credentials"),
                json!({key:"invalid"})
            )
            .await
            .0,
            200
        );
        assert_eq!(
            rig.post(&format!("/api/connectors/{id}/test"), json!({}))
                .await
                .1["ok"],
            false,
            "invalid key {id}"
        );
        rig.post(
            &format!("/api/connectors/{id}/credentials"),
            json!({key:value}),
        )
        .await;
        assert_eq!(
            rig.post(&format!("/api/connectors/{id}/auth"), json!({}))
                .await
                .1["auth"],
            "apikey"
        );
        assert_eq!(
            rig.post(&format!("/api/connectors/{id}/test"), json!({}))
                .await
                .1["ok"],
            true,
            "valid key {id}"
        );
        assert_eq!(
            rig.post(&format!("/api/connectors/{id}/token"), json!({}))
                .await
                .0,
            400,
            "no OAuth mint {id}"
        );
        println!(
            "PASS {id}: missing credential, invalid key, save, auth model, provider canary, unsupported OAuth mint"
        );
    }
    // An untrusted provider can echo the submitted bearer in an error body.
    let echoed_key = "KEY-SHOULD-NEVER-APPEAR-IN-TEST-RESULT";
    assert_eq!(
        rig.post(
            "/api/connectors",
            json!({
                "id":"fixture-key", "label":"Matrix API key", "kind":"api_key",
                "key_env":"MATRIX_API_KEY", "test_url":format!("{}/data",rig.provider)
            })
        )
        .await
        .0,
        200
    );
    rig.post(
        "/api/connectors/fixture-key/credentials",
        json!({"MATRIX_API_KEY":echoed_key}),
    )
    .await;
    let (_, rejected) = rig
        .post("/api/connectors/fixture-key/test", json!({}))
        .await;
    assert_eq!(rejected["ok"], false);
    assert_eq!(rejected["http_status"], 401);
    assert!(
        !rejected.to_string().contains(echoed_key),
        "provider echo leaked in test response"
    );
    let persisted = std::fs::read_to_string(rig.home.join("connector-tests.json")).unwrap();
    assert!(
        !persisted.contains(echoed_key),
        "provider echo leaked in durable last test"
    );
    let (_, inventory) = rig
        .call(reqwest::Method::GET, "/api/connectors", json!({}), false)
        .await;
    assert!(
        !inventory.to_string().contains(echoed_key),
        "provider echo leaked in connector inventory"
    );
    rig.post(
        "/api/connectors/fixture-key/credentials",
        json!({"MATRIX_API_KEY":"matrix-api-key"}),
    )
    .await;
    assert_eq!(
        rig.post("/api/connectors/fixture-key/test", json!({}))
            .await
            .1["ok"],
        true
    );
    println!("PASS declared API key: provider credential echo excluded from response, inventory and durable test result; corrected key succeeds");
    // Telegram's operational path must immediately consume the saved credential.
    assert_eq!(
        rig.call(
            reqwest::Method::GET,
            "/api/telegram/status",
            json!({}),
            false
        )
        .await
        .1["bot_token_set"],
        true
    );
    assert_eq!(
        rig.post(
            "/api/telegram/mappings",
            json!({"chat_id":-100,"session":"matrix-worker","chat_type":"group"})
        )
        .await
        .0,
        409,
        "group requires a private link"
    );
    fixture.lock().unwrap().updates = vec![
        json!({"update_id":41,"message":{"chat":{"id":100,"type":"private"},"text":"/link matrix-worker","from":{"username":"fixture"}}}),
    ];
    assert_eq!(rig.post("/matrix/poll", json!({})).await.0, 200);
    let mappings = rig
        .call(
            reqwest::Method::GET,
            "/api/telegram/mappings",
            json!({}),
            false,
        )
        .await
        .1;
    assert_eq!(mappings["mappings"][0]["session"], "matrix-worker");
    assert_eq!(
        rig.post(
            "/api/telegram/mappings",
            json!({"chat_id":-100,"session":"matrix-worker","chat_type":"group"})
        )
        .await
        .0,
        200
    );
    rig.scope("telegram", "", false).await;
    let sends = fixture.lock().unwrap().sends.len();
    assert_eq!(
        rig.call(
            reqwest::Method::POST,
            "/api/telegram/send",
            json!({"chat_id":100,"text":"denied fixture message"}),
            true
        )
        .await
        .0,
        403
    );
    assert_eq!(
        fixture.lock().unwrap().sends.len(),
        sends,
        "denied worker makes no provider call"
    );
    rig.scope("telegram", "", true).await;
    fixture.lock().unwrap().fail_format = true;
    assert_eq!(
        rig.call(
            reqwest::Method::POST,
            "/api/telegram/send",
            json!({"chat_id":100,"text":"<b>fixture only</b>","parse_mode":"HTML"}),
            true
        )
        .await
        .0,
        200
    );
    assert_eq!(
        fixture.lock().unwrap().sends.last().unwrap()["text"],
        "fixture only",
        "format retry sent exact plain text"
    );
    fixture.lock().unwrap().reject_send = true;
    assert_eq!(
        rig.post(
            "/api/telegram/send",
            json!({"chat_id":100,"text":"provider rejection"})
        )
        .await
        .0,
        502,
        "HTTP200/okfalse is a rejection"
    );
    fixture.lock().unwrap().reject_send = false;
    println!(
        "PASS telegram: saved key consumed without restart, real poll /link, private/group gate, scoped send, HTML retry, application-level rejection"
    );
    rig.post("/api/connectors/google-drive/credentials",json!({"GOOGLE_OAUTH_CLIENT_ID":"matrix-client","GOOGLE_OAUTH_CLIENT_SECRET":"matrix-secret"})).await;
    rig.post(
        "/api/connectors/slack/credentials",
        json!({"SLACK_CLIENT_ID":"matrix-client","SLACK_CLIENT_SECRET":"matrix-secret"}),
    )
    .await;
    for id in &ids[1..6] {
        rig.grant(id, "alice@example.test").await;
        rig.grant(id, "beth@example.test").await;
    }
    for id in &ids[1..6] {
        let (_, begin) = rig
            .post(
                &format!("/api/connectors/{id}/auth?account=alice@example.test"),
                json!({}),
            )
            .await;
        let u = reqwest::Url::parse(begin["authorize_url"].as_str().unwrap()).unwrap();
        let mut q: BTreeMap<String, String> = u.query_pairs().into_owned().collect();
        q.insert("fixture_account".into(), "alice@example.test".into());
        let callback = if id.starts_with("google-") {
            "/api/gmail/callback".to_string()
        } else {
            format!("/api/connectors/{id}/callback")
        };
        assert_eq!(
            rig.call(
                reqwest::Method::GET,
                &format!("{callback}?state={}&error=access_denied", q["state"]),
                json!({}),
                false
            )
            .await
            .0,
            400
        );
        let approved: Value = rig
            .client
            .post(format!("{}/approve", rig.provider))
            .json(&q)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            rig.call(
                reqwest::Method::GET,
                &format!(
                    "{callback}?state={}&code={}",
                    q["state"],
                    approved["code"].as_str().unwrap()
                ),
                json!({}),
                false
            )
            .await
            .0,
            400,
            "cancelled consent must not exchange {id}"
        );
        println!(
            "PASS {id}: cancel consumes registered callback state; later code cannot revive consent"
        );
    }
    rig.post("/api/connectors/mattermost/credentials",json!({"MATTERMOST_URL":rig.provider,"MATTERMOST_LOGIN":"alice@example.test","MATTERMOST_PASSWORD":"invalid"})).await;
    assert_eq!(
        rig.post("/api/connectors/mattermost/auth", json!({}))
            .await
            .0,
        401
    );
    for account in ["alice@example.test", "beth@example.test"] {
        rig.post("/api/connectors/mattermost/credentials",json!({"MATTERMOST_URL":rig.provider,"MATTERMOST_LOGIN":account,"MATTERMOST_PASSWORD":"matrix-password"})).await;
        assert_eq!(
            rig.post(
                &format!("/api/connectors/mattermost/auth?account={account}"),
                json!({})
            )
            .await
            .0,
            200
        );
    }
    for id in [&ids[1..6], &ids[7..]].concat() {
        rig.scope(id, "beth@example.test", true).await;
        let (status, token) = rig
            .call(
                reqwest::Method::POST,
                &format!("/api/connectors/{id}/token"),
                json!({}),
                true,
            )
            .await;
        assert_eq!(status, 200, "pinned mint {id}");
        assert_eq!(
            token[if id == "mattermost" {
                "account"
            } else {
                "subject"
            }],
            "beth@example.test",
            "pin {id}"
        );
        assert_eq!(
            rig.call(
                reqwest::Method::POST,
                &format!("/api/connectors/{id}/token?account=alice@example.test"),
                json!({}),
                true
            )
            .await
            .0,
            403,
            "different account {id}"
        );
        rig.scope(id, "beth@example.test", false).await;
        assert_eq!(
            rig.call(
                reqwest::Method::POST,
                &format!("/api/connectors/{id}/token"),
                json!({}),
                true
            )
            .await
            .0,
            403,
            "disabled {id}"
        );
        rig.scope(id, "beth@example.test", true).await;
        let (_, tested) = rig
            .post(&format!("/api/connectors/{id}/test"), json!({}))
            .await;
        assert_eq!(tested["ok"], true, "saved grant canary {id}");
        assert_eq!(
            tested["accounts"].as_array().unwrap().len(),
            2,
            "test every account {id}"
        );
        println!(
            "PASS {id}: connect two accounts, replay rejection (OAuth), pinned worker mint, other-account/disabled denial, canary for both accounts"
        );
    }
    // Expire grants on disk only to move the synthetic clock; tokens still issued over HTTP.
    for family in ["google", "slack"] {
        let path = rig
            .home
            .join(format!("connectors/{family}/beth@example.test.json"));
        let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v["expires_at"] = json!(0);
        std::fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
    }
    for id in ["google-drive", "slack"] {
        assert_eq!(
            rig.call(
                reqwest::Method::POST,
                &format!("/api/connectors/{id}/token"),
                json!({}),
                true
            )
            .await
            .0,
            200,
            "refresh {id}"
        );
    }
    let refresh_calls = fixture.lock().unwrap().calls.clone();
    // Optional explicit-owner native CLI pass; CI never sets this variable.
    // Only synthetic credentials live here. No fleet jobs, hook injection or MCP.
    if let Ok(receipt) = std::env::var("AMUX_MATRIX_OWNER_HANDOFF") {
        std::fs::write(rig.home.join("auth_token"), "matrix-owner").unwrap();
        let release = rig.home.join("owner-release");
        std::fs::write(&receipt,json!({"amux_origin":format!("http://127.0.0.1:{}",rig.port),"provider_origin":rig.provider,"home":rig.home,"release_file":release}).to_string()).unwrap();
        println!("OWNER HANDOFF: private synthetic server awaiting explicit native worker check");
        let started = Instant::now();
        while !release.exists() {
            assert!(
                started.elapsed() < Duration::from_secs(600),
                "owner handoff timed out"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    rig.kill();
    rig.start();
    rig.ready().await;
    let sends = fixture.lock().unwrap().sends.len();
    assert_eq!(rig.post("/matrix/poll", json!({})).await.0, 200);
    assert_eq!(
        fixture.lock().unwrap().sends.len(),
        sends,
        "SIGKILL does not replay acknowledged inbound /link"
    );
    assert_eq!(
        *fixture.lock().unwrap().offsets.last().unwrap(),
        42,
        "poll cursor survived SIGKILL"
    );
    assert_eq!(
        rig.post(
            "/api/telegram/send",
            json!({"session":"matrix-worker","text":"synthetic outbound after restart"})
        )
        .await
        .0,
        200,
        "mapping survived SIGKILL"
    );

    for id in ids {
        assert_eq!(
            rig.post(&format!("/api/connectors/{id}/test"), json!({}))
                .await
                .1["ok"],
            true,
            "post-SIGKILL canary {id}"
        );
    }
    for id in ["google-drive", "slack", "mattermost"] {
        assert_eq!(
            rig.call(
                reqwest::Method::POST,
                &format!("/api/connectors/{id}/token"),
                json!({}),
                true
            )
            .await
            .0,
            200
        );
    }
    for path in [
        "/oauth2.googleapis.com/token",
        "/slack.com/api/oauth.v2.access",
    ] {
        assert_eq!(
            fixture.lock().unwrap().calls.get(path),
            refresh_calls.get(path),
            "restart reused persisted token/rotation"
        );
    }
    fixture
        .lock()
        .unwrap()
        .revoked
        .push("beth@example.test".into());
    for id in [&ids[1..6], &ids[7..]].concat() {
        let (_, v) = rig
            .post(&format!("/api/connectors/{id}/test"), json!({}))
            .await;
        assert_eq!(v["ok"], false, "revoked {id}");
        let accounts = v["accounts"].as_array().unwrap();
        assert!(accounts
            .iter()
            .any(|a| a["account"] == "alice@example.test" && a["ok"] == true));
        assert!(accounts
            .iter()
            .any(|a| a["account"] == "beth@example.test" && a["ok"] == false));
        println!("PASS {id}: revocation visible, other account remains healthy");
    }
    for family in ["google", "slack"] {
        let path = rig
            .home
            .join(format!("connectors/{family}/beth@example.test.json"));
        let mut v: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        v["expires_at"] = json!(0);
        std::fs::write(path, serde_json::to_vec(&v).unwrap()).unwrap();
        let id = if family == "google" {
            "google-drive"
        } else {
            "slack"
        };
        let (code, v) = rig
            .call(
                reqwest::Method::POST,
                &format!("/api/connectors/{id}/token"),
                json!({}),
                true,
            )
            .await;
        assert_eq!(code, 409, "revoked refresh {id}");
        assert_eq!(v["status"], "needs_reauth");
        assert!(v["reconnect"].is_string());
        println!("PASS {id}: revoked rotating refresh returns needs_reauth and reconnect action");
    }
    for id in ["google-drive", "slack"] {
        rig.grant(id, "beth@example.test").await;
    }
    assert_eq!(
        rig.post(
            "/api/connectors/mattermost/auth?account=beth@example.test",
            json!({})
        )
        .await
        .0,
        200,
        "renew expired Mattermost login"
    );
    for id in ids {
        assert_eq!(
            rig.post(&format!("/api/connectors/{id}/test"), json!({}))
                .await
                .1["ok"],
            true
        );
        println!(
            "PASS {id}: credentials/grants and scopes survive real SIGKILL; recovery canary passes"
        );
    }
    assert_eq!(
        rig.call(
            reqwest::Method::DELETE,
            "/api/telegram/mappings/100",
            json!({}),
            false
        )
        .await
        .0,
        204
    );
    assert_eq!(
        rig.post(
            "/api/telegram/send",
            json!({"session":"missing","text":"fixture only"})
        )
        .await
        .0,
        404
    );
    // A fixture RSA key exercises the actual JWT signing + HTTP delegation path.
    let pem = rig.home.join("sa.pem");
    let public = rig.home.join("sa-public.pem");
    assert!(Command::new("openssl")
        .args(["genrsa", "-out"])
        .arg(&pem)
        .arg("2048")
        .output()
        .unwrap()
        .status
        .success());
    assert!(Command::new("openssl")
        .args(["rsa", "-in"])
        .arg(&pem)
        .args(["-pubout", "-out"])
        .arg(&public)
        .output()
        .unwrap()
        .status
        .success());
    let sa_uri = format!("{}/sa/token", rig.provider);
    {
        let mut f = fixture.lock().unwrap();
        f.rsa_public = std::fs::read(&public).unwrap();
        f.sa_audience = sa_uri.clone();
    }
    let sa = rig.home.join("sa.json");
    std::fs::write(&sa,json!({"client_email":"fixture-sa@example.test","private_key":std::fs::read_to_string(&pem).unwrap(),"token_uri":sa_uri}).to_string()).unwrap();
    std::fs::remove_dir_all(rig.home.join("connectors/google")).unwrap();
    std::fs::remove_dir_all(rig.home.join("gmail-tokens")).unwrap();
    let env = rig.home.join("server.env");
    let mut data = std::fs::read_to_string(&env).unwrap();
    data.push_str(&format!(
        "\nGOOGLE_SA_KEY_FILE={}\nGOOGLE_SA_SUBJECT=alice@example.test\n",
        sa.display()
    ));
    std::fs::write(&env, data).unwrap();
    for id in &ids[1..5] {
        let (code, v) = rig
            .call(
                reqwest::Method::POST,
                &format!("/api/connectors/{id}/token"),
                json!({}),
                true,
            )
            .await;
        assert_eq!(code, 200, "delegation {id}");
        assert_eq!(
            v["subject"], "beth@example.test",
            "scope pin controls SA subject {id}"
        );
        assert_eq!(
            rig.post(&format!("/api/connectors/{id}/test"), json!({}))
                .await
                .1["ok"],
            true,
            "SA canary {id}"
        );
        println!(
            "PASS {id}: real signed RSA JWT, verified audience/signature/scope/subject, service-account exchange and provider canary"
        );
    }
    fixture.lock().unwrap().deny_delegation = true;
    assert_eq!(
        rig.call(
            reqwest::Method::POST,
            "/api/connectors/google-admin/token",
            json!({}),
            true
        )
        .await
        .0,
        403,
        "delegation refusal"
    );
    assert_eq!(
        rig.post("/api/connectors/google-admin/test", json!({}))
            .await
            .1["status"],
        "needs_auth"
    );
    fixture.lock().unwrap().deny_delegation = false;
    std::fs::remove_file(&sa).unwrap();
    assert_eq!(
        rig.call(
            reqwest::Method::POST,
            "/api/connectors/google-drive/token",
            json!({}),
            true
        )
        .await
        .0,
        404,
        "missing SA key cannot impersonate"
    );
    println!(
        "PASS google delegation: explicit scope refusal, missing durable key never falsely connected"
    );
    assert_eq!(
        Sha256::digest(std::fs::read(&rig.binary).unwrap()),
        binary_digest,
        "private executable bytes remained unchanged across crash/restart and native handoff"
    );
    rig.kill();
    fixture_task.abort();
}
