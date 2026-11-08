//! The Photos tab's BYO Mixpeek connection (Ethan, 2026-11-08: "Implement
//! Bring Your Own Mixpeek ... connection scaffolding and onboarding UX").
//!
//! One Mixpeek namespace is one media library. The owner connects an existing
//! instance, local or hosted, by endpoint, optional API key and namespace;
//! amux checks that the key can read the namespace before saving anything.
//! The key lives only in `photos/mixpeek.json` under the amux home (a private
//! file written through the connector store) and is never sent back: reads
//! answer `key_set`.
//!
//! - `GET    /api/photos/mixpeek`          the saved connection, no key
//! - `GET    /api/photos/mixpeek?check=1`  the same, re-checked live
//! - `POST   /api/photos/mixpeek`          `{endpoint, api_key?, namespace}`: check, then save
//! - `DELETE /api/photos/mixpeek`          forget it
//!
//! Ingestion, indexing and search are not wired yet; the tab shows sample
//! media until they are.
use crate::api::AppState;
use crate::integrations::email::{HttpTransport, ReqwestTransport};
use crate::integrations::mixpeek::{normalize_endpoint, valid_namespace, Check, MixpeekClient};
use axum::extract::{Extension, Json, Query};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

pub struct PhotosCtx {
    pub home: PathBuf,
    pub http: Arc<dyn HttpTransport>,
}

pub fn routes() -> Router<AppState> {
    routes_with(Arc::new(PhotosCtx {
        home: crate::config::amux_home(),
        http: Arc::new(ReqwestTransport::new()),
    }))
}

pub fn routes_with(ctx: Arc<PhotosCtx>) -> Router<AppState> {
    Router::new()
        .route("/api/photos/mixpeek", get(view).post(connect).delete(disconnect))
        .layer(Extension(ctx))
}

fn store(ctx: &PhotosCtx) -> PathBuf {
    ctx.home.join("photos").join("mixpeek.json")
}

fn saved(ctx: &PhotosCtx) -> Result<Option<Value>, String> {
    crate::integrations::secure_store::read_json::<Value>(&store(ctx))
        .map(|v| v.is_object().then_some(v).filter(|v| v["endpoint"].is_string()))
        .map_err(|e| e.to_string())
}

fn client_of(v: &Value) -> MixpeekClient {
    MixpeekClient {
        endpoint: v["endpoint"].as_str().unwrap_or("").to_string(),
        api_key: v["api_key"].as_str().filter(|k| !k.is_empty()).map(str::to_string),
        namespace: v["namespace"].as_str().unwrap_or("").to_string(),
    }
}

/// The connection as the browser may see it: never the key.
fn public(v: &Value, status: &str, message: &str) -> Value {
    json!({
        "configured": true,
        "endpoint": v["endpoint"],
        "namespace": v["namespace"],
        "namespace_name": v["namespace_name"],
        "key_set": v["api_key"].as_str().is_some_and(|k| !k.is_empty()),
        "status": status,
        "message": message,
        "checked_at": v["checked_at"],
        "connected_at": v["connected_at"],
        "measured": true,
        "n_considered": 1,
    })
}

async fn view(Extension(ctx): Extension<Arc<PhotosCtx>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let v = match saved(&ctx) {
        Ok(Some(v)) => v,
        Ok(None) => {
            return Json(json!({"configured": false, "status": "disconnected", "measured": true, "n_considered": 0}))
                .into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, verdict = "photos_mixpeek_store_unreadable", "the saved Mixpeek connection could not be read");
            return (StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"configured": false, "status": "error", "message": "The saved connection could not be read.", "measured": false, "why_unmeasured": e})))
                .into_response();
        }
    };
    if !matches!(q.get("check").map(String::as_str), Some("1" | "true")) {
        let status = v["last_status"].as_str().unwrap_or("connected").to_string();
        return Json(public(&v, &status, "")).into_response();
    }
    let check = client_of(&v).check(ctx.http.as_ref()).await;
    let mut v = v;
    v["last_status"] = json!(check.status());
    v["checked_at"] = json!(crate::config::now_f64());
    // A failed write only loses the timestamp; the answer below is still live.
    if let Err(e) = crate::integrations::secure_store::write(&store(&ctx), v.to_string().as_bytes()) {
        tracing::warn!(error = %e, verdict = "photos_mixpeek_check_not_saved", "the connection check result was not saved");
    }
    log_check(&v, &check);
    Json(public(&v, check.status(), &check.message())).into_response()
}

fn log_check(v: &Value, check: &Check) {
    tracing::info!(endpoint = v["endpoint"].as_str().unwrap_or(""), namespace = v["namespace"].as_str().unwrap_or(""),
        status = check.status(), measured = true, n_considered = 1, verdict = "photos_mixpeek_checked",
        "BYO Mixpeek connection checked");
}

async fn connect(Extension(ctx): Extension<Arc<PhotosCtx>>, Json(body): Json<Value>) -> Response {
    let bad = |field: &str, message: &str| {
        (StatusCode::BAD_REQUEST, Json(json!({"status": "invalid", "field": field, "message": message}))).into_response()
    };
    let endpoint = match normalize_endpoint(body["endpoint"].as_str().unwrap_or("")) {
        Ok(e) => e,
        Err(m) => return bad("endpoint", &m),
    };
    let namespace = body["namespace"].as_str().unwrap_or("").trim().to_string();
    if !valid_namespace(&namespace) {
        return bad("namespace", "Enter the namespace ID (letters, digits, - _ .).");
    }
    // An empty key on a reconnect to the same instance keeps the saved one, so
    // re-checking does not mean pasting the key again.
    let previous = saved(&ctx).ok().flatten();
    let typed = body["api_key"].as_str().map(str::trim).unwrap_or("").to_string();
    let api_key = if !typed.is_empty() {
        Some(typed)
    } else {
        previous
            .as_ref()
            .filter(|p| p["endpoint"].as_str() == Some(endpoint.as_str()))
            .and_then(|p| p["api_key"].as_str().filter(|k| !k.is_empty()).map(str::to_string))
    };
    let client = MixpeekClient { endpoint: endpoint.clone(), api_key: api_key.clone(), namespace: namespace.clone() };
    let check = client.check(ctx.http.as_ref()).await;
    let now = crate::config::now_f64();
    let v = json!({
        "endpoint": endpoint, "namespace": namespace, "api_key": api_key.unwrap_or_default(),
        "namespace_name": match &check { Check::Connected { namespace_name } => json!(namespace_name), _ => Value::Null },
        "last_status": check.status(), "checked_at": now, "connected_at": now,
    });
    log_check(&v, &check);
    if !matches!(check, Check::Connected { .. }) {
        // Nothing is saved: a connection that cannot read its namespace is not a connection.
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({
            "configured": previous.is_some(), "status": check.status(), "message": check.message(),
            "field": match check { Check::KeyRejected { .. } => "api_key", Check::NamespaceNotFound => "namespace", _ => "endpoint" },
        }))).into_response();
    }
    if let Err(e) = crate::integrations::secure_store::write(&store(&ctx), v.to_string().as_bytes()) {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"status": "error", "message": format!("Connected, but the connection could not be saved: {e}")}))).into_response();
    }
    tracing::info!(endpoint = %v["endpoint"], namespace = %v["namespace"], measured = true, n_considered = 1,
        verdict = "photos_mixpeek_connected", "BYO Mixpeek connection saved");
    Json(public(&v, check.status(), &check.message())).into_response()
}

async fn disconnect(Extension(ctx): Extension<Arc<PhotosCtx>>) -> Response {
    match std::fs::remove_file(store(&ctx)) {
        Ok(()) => {
            tracing::info!(measured = true, n_considered = 1, verdict = "photos_mixpeek_disconnected", "BYO Mixpeek connection removed");
            Json(json!({"configured": false, "status": "disconnected"})).into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Json(json!({"configured": false, "status": "disconnected"})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"status": "error", "message": e.to_string()}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Mock(Mutex<Vec<(String, Option<String>)>>, u16, Value);
    #[async_trait::async_trait]
    impl HttpTransport for Mock {
        async fn get(&self, url: &str, bearer: Option<&str>) -> Result<(u16, Value), String> {
            self.0.lock().unwrap().push((url.into(), bearer.map(str::to_string)));
            if self.1 == 0 { return Err("connection refused".into()); }
            Ok((self.1, self.2.clone()))
        }
        async fn post_json(&self, _: &str, _: Option<&str>, _: &Value) -> Result<(u16, Value), String> { Err("unused".into()) }
        async fn post_form(&self, _: &str, _: &[(String, String)]) -> Result<(u16, Value), String> { Err("unused".into()) }
    }

    async fn call(app: &Router, method: &str, body: Option<Value>, query: &str) -> (StatusCode, Value) {
        use tower::ServiceExt;
        let req = axum::http::Request::builder().method(method).uri(format!("/api/photos/mixpeek{query}"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.map(|b| b.to_string()).unwrap_or_default())).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let st = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn app(home: &std::path::Path, mock: Arc<Mock>) -> Router {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            store: Arc::new(crate::db::Store::open(&dir.path().join("p.db")).unwrap()),
            started: std::time::Instant::now(), build_hash: "test".into(), auth_token: None,
            reconciled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        std::mem::forget(dir);
        routes_with(Arc::new(PhotosCtx { home: home.to_path_buf(), http: mock })).with_state(state)
    }

    #[tokio::test]
    async fn a_connection_is_saved_only_when_its_key_can_read_the_namespace_and_the_key_never_comes_back() {
        let home = tempfile::tempdir().unwrap();
        let ok = Arc::new(Mock(Mutex::new(vec![]), 200, json!({"namespace_id": "ns_1", "namespace_name": "Family photos"})));
        let a = app(home.path(), ok.clone());
        let (st, v) = call(&a, "GET", None, "").await;
        assert_eq!((st, v["configured"].clone()), (StatusCode::OK, json!(false)));

        let (st, v) = call(&a, "POST", Some(json!({"endpoint": "https://mx.example.com/v1/", "api_key": "sk_test_1", "namespace": "ns_1"})), "").await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["status"], json!("connected"));
        assert_eq!(v["namespace_name"], json!("Family photos"));
        assert_eq!(v["key_set"], json!(true));
        assert!(!v.to_string().contains("sk_test_1"), "the key never comes back: {v}");
        assert_eq!(ok.0.lock().unwrap()[0], ("https://mx.example.com/v1/namespaces/ns_1".to_string(), Some("sk_test_1".into())));

        let (_, v) = call(&a, "GET", None, "?check=1").await;
        assert_eq!(v["status"], json!("connected"));
        assert!(!v.to_string().contains("sk_test_1"));
        // A re-check with no key typed keeps the saved one.
        let (st, _) = call(&a, "POST", Some(json!({"endpoint": "https://mx.example.com", "namespace": "ns_1"})), "").await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(ok.0.lock().unwrap().last().unwrap().1, Some("sk_test_1".into()));

        let (_, v) = call(&a, "DELETE", None, "").await;
        assert_eq!(v["configured"], json!(false));
        let (_, v) = call(&a, "GET", None, "").await;
        assert_eq!(v["configured"], json!(false));
    }

    #[tokio::test]
    async fn a_refused_key_a_missing_namespace_or_no_answer_saves_nothing_and_says_which() {
        for (http, status, field) in [(401, "key_rejected", "api_key"), (404, "namespace_not_found", "namespace"), (0, "unreachable", "endpoint")] {
            let home = tempfile::tempdir().unwrap();
            let a = app(home.path(), Arc::new(Mock(Mutex::new(vec![]), http, json!({}))));
            let (st, v) = call(&a, "POST", Some(json!({"endpoint": "http://localhost:8000", "api_key": "k", "namespace": "ns"})), "").await;
            assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
            assert_eq!(v["status"], json!(status));
            assert_eq!(v["field"], json!(field));
            assert!(!home.path().join("photos/mixpeek.json").exists());
        }
        let home = tempfile::tempdir().unwrap();
        let a = app(home.path(), Arc::new(Mock(Mutex::new(vec![]), 200, json!({}))));
        let (st, v) = call(&a, "POST", Some(json!({"endpoint": "localhost:8000", "namespace": "ns"})), "").await;
        assert_eq!((st, v["field"].clone()), (StatusCode::BAD_REQUEST, json!("endpoint")));
        let (st, v) = call(&a, "POST", Some(json!({"endpoint": "http://localhost:8000", "namespace": "../x"})), "").await;
        assert_eq!((st, v["field"].clone()), (StatusCode::BAD_REQUEST, json!("namespace")));
    }
}
