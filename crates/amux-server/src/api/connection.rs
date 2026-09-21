//! Connection setup: existing owner-session exchange and TLS material only.
//! Public read/exchange routes deliberately do not inherit loopback bypass.
use super::AppState;
use axum::{extract::{State, OriginalUri}, http::{HeaderMap, StatusCode, Uri}, response::{IntoResponse, Response}, Json};
use serde::Deserialize;
use std::path::PathBuf;
use axum::{Extension, Router, routing::{get,post}};

#[derive(Clone)]
struct TlsDirectory(PathBuf);
pub fn routes(dir: PathBuf) -> Router<AppState> {
    Router::new()
        .route("/api/connection/security",get(status))
        .route("/api/connection/session",post(sign_in).layer(axum::extract::DefaultBodyLimit::max(4096)))
        .route("/api/connection/certificate",post(certificate).layer(axum::extract::DefaultBodyLimit::max(100_000)))
        .layer(Extension(TlsDirectory(dir)))
}

use serde_json::json;

fn refusal(status: StatusCode, reason: &'static str) -> Response {
    tracing::warn!(target:"amux::auth", verdict="connection_security_refused", reason, "connection action refused (credentials withheld from diagnostics)");
    (status, [("cache-control", "no-store")], Json(json!({"error":reason}))).into_response()
}
/// Host is the actual HTTP authority, not an asserted forwarded host. JSON +
/// exact HTTPS origin + Fetch Metadata prevent CSRF even on loopback and even
/// with an existing owner cookie. No Origin is not a browser sign-in request.
fn same_origin(headers: &HeaderMap, uri: &Uri) -> bool {
    let authority = headers.get("host").and_then(|h|h.to_str().ok()).or_else(||uri.authority().map(|a|a.as_str()));
    let origin = headers.get("origin").and_then(|h|h.to_str().ok());
    let (Some(authority), Some(origin)) = (authority,origin) else { return false; };
    let Ok(expected) = reqwest::Url::parse(&format!("https://{authority}")) else { return false; };
    if origin != expected.origin().ascii_serialization() { return false; }
    if headers.get("sec-fetch-site").is_some_and(|h| h != "same-origin") { return false; }
    uri.query().is_none()
}
fn owner(state: &AppState, headers: &HeaderMap) -> bool {
    // No locality, worker identity, member cookie or query credential can
    // authorize a certificate change. Reuse the existing credential/session.
    super::auth::has_owner_token(state, headers, &Uri::from_static("/"))
        || (!super::org::is_verified_local_member(headers)
            && !super::org::has_local_member_cookie(headers)
            && super::static_files::owner_session_status(state,headers)=="valid")
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignIn { token: String }

pub async fn sign_in(State(state): State<AppState>, OriginalUri(uri): OriginalUri, headers: HeaderMap, input: Result<Json<SignIn>, axum::extract::rejection::JsonRejection>) -> Response {
    if !same_origin(&headers,&uri) { return refusal(StatusCode::FORBIDDEN,"same_origin_required"); }
    let Ok(Json(input)) = input else { return refusal(StatusCode::BAD_REQUEST,"invalid_sign_in_request"); };
    // This is intentionally independent of middleware's localhost admission.
    if !state.auth_token.as_deref().is_some_and(|expected| !input.token.is_empty()
        && super::auth::constant_time_eq(input.token.as_bytes(),expected.as_bytes())) {
        return refusal(StatusCode::UNAUTHORIZED,"invalid_owner_token");
    }
    let established = super::static_files::establish_owner_session(&state);
    let mut response = ([("cache-control","no-store")],Json(json!({"ok":true,"reload":"/api/_clear_sw"}))).into_response();
    if let Some(cookie) = established.headers().get("set-cookie") {
        response.headers_mut().insert("set-cookie",cookie.clone());
    }
    response
}
async fn status(State(state): State<AppState>, Extension(dir): Extension<TlsDirectory>, headers: HeaderMap) -> Response {
    let auth = if owner(&state,&headers) { "owner" }
        else if super::org::is_verified_local_member(&headers) { "member" }
        else if state.auth_token.is_none() { "not_configured" }
        else { "sign_in_required" };
    let tls = crate::tls::connection_status(&dir.0);
    ([("cache-control","no-store")], Json(json!({"auth":auth, "owner_session":super::static_files::owner_session_status(&state,&headers),
        "can_configure":auth=="owner", "tls":tls}))).into_response()
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertificateUpload { certificate_pem: String, private_key_pem: String }
async fn certificate(State(state): State<AppState>, Extension(dir): Extension<TlsDirectory>, OriginalUri(uri): OriginalUri, headers: HeaderMap, input: Result<Json<CertificateUpload>, axum::extract::rejection::JsonRejection>) -> Response {
    if !same_origin(&headers,&uri) { return refusal(StatusCode::FORBIDDEN,"same_origin_required"); }
    if !owner(&state,&headers) { return refusal(StatusCode::FORBIDDEN,"owner_session_or_token_required"); }
    let Ok(Json(input)) = input else { return refusal(StatusCode::BAD_REQUEST,"invalid_certificate_request"); };
    let origin = headers.get("origin").and_then(|h|h.to_str().ok()).unwrap_or("");
    let host = reqwest::Url::parse(origin).ok().and_then(|u|u.host_str().map(|h|h.trim_matches(['[',']']).to_string())).unwrap_or_default();
    let dir = dir.0;
    let result = tokio::task::spawn_blocking(move || crate::tls::install_connection_certificate(&dir, &input.certificate_pem, &input.private_key_pem, &host)).await;
    match result {
        Ok(Ok(metadata)) => ([("cache-control","no-store")],Json(json!({"ok":true,"saved":metadata,"applied":false,"restart_required":true}))).into_response(),
        Ok(Err(err)) => {
            // Only fixed error codes can reach request logs; filesystem or
            // parser errors must never echo uploaded contents or private paths.
            let reason = match err.to_string().as_str() {
                "certificate_invalid_or_key_mismatch"=>"certificate_invalid_or_key_mismatch",
                "certificate_hostname_mismatch"=>"certificate_hostname_mismatch",
                "certificate_not_currently_valid"=>"certificate_not_currently_valid",
                "certificate_field_contains_private_key"=>"certificate_field_contains_private_key",
                "certificate_too_large"=>"certificate_too_large",
                "certificate_inspector_unavailable"=>"certificate_inspector_unavailable",
                "certificate_inspector_timeout"=>"certificate_inspector_timeout",
                _=>"certificate_validation_or_save_failed",
            };
            refusal(StatusCode::UNPROCESSABLE_ENTITY,reason)
        }
        Err(_)=>refusal(StatusCode::INTERNAL_SERVER_ERROR,"certificate_save_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::{Body,to_bytes}, http::Request, extract::ConnectInfo};
    use std::sync::Arc;
    use tower::ServiceExt;
    fn app(token: Option<&str>, dir: &std::path::Path) -> Router {
        let state=AppState {
            store:Arc::new(crate::db::Store::open(&dir.join("test.db")).unwrap()),
            started:std::time::Instant::now(),build_hash:"test".into(),auth_token:token.map(str::to_string),
            reconciled:Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        routes(dir.join("tls"))
            .route("/api/_clear_sw",get(super::super::static_files::clear_sw_landing))
            .layer(axum::middleware::from_fn_with_state(state.clone(),super::super::org::local_member_identity))
            .with_state(state)
    }
    async fn call(app:&Router,path:&str,body:Option<serde_json::Value>,origin:Option<&str>,cookie:Option<&str>) -> (StatusCode,HeaderMap,serde_json::Value) {
        let mut req=Request::builder().uri(path).method(if body.is_some(){"POST"}else{"GET"}).header("host","localhost:18972").header("content-type","application/json");
        if let Some(o)=origin {req=req.header("origin",o);}
        if let Some(c)=cookie {req=req.header("cookie",c);}
        let mut req=req.body(body.map_or(Body::empty(),|v|Body::from(v.to_string()))).unwrap();
        req.extensions_mut().insert(ConnectInfo("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()));
        let response=app.clone().oneshot(req).await.unwrap();
        let status=response.status();let headers=response.headers().clone();
        let bytes=to_bytes(response.into_body(),200000).await.unwrap();
        (status,headers,serde_json::from_slice(&bytes).unwrap())
    }
    #[tokio::test]
    async fn connection_security_sign_in_requires_explicit_valid_same_origin_token_even_on_loopback() {
        let dir=tempfile::tempdir().unwrap();let router=app(Some("owner-secret"),dir.path());
        for (path,body,origin,expected) in [
            ("/api/connection/session",json!({"token":""}),Some("https://localhost:18972"),401),
            ("/api/connection/session",json!({}),Some("https://localhost:18972"),400),
            ("/api/connection/session",json!({"token":"bad-secret"}),Some("https://localhost:18972"),401),
            ("/api/connection/session",json!({"token":"owner-secret"}),None,403),
            ("/api/connection/session",json!({"token":"owner-secret"}),Some("https://evil.test"),403),
            ("/api/connection/session",json!({"token":"owner-secret"}),Some("http://localhost:18972"),403),
            ("/api/connection/session?_token=owner-secret",json!({"token":"owner-secret"}),Some("https://localhost:18972"),403),
        ] {
            let (status,headers,body)=call(&router,path,Some(body),origin,None).await;
            assert_eq!(status.as_u16(),expected);assert!(!headers.contains_key("set-cookie"));
            assert!(!body.to_string().contains("secret"));
        }
        let (status,headers,body)=call(&router,"/api/connection/session",Some(json!({"token":"owner-secret"})),Some("https://localhost:18972"),Some("amux_member=revoked-member")).await;
        assert_eq!(status,StatusCode::OK);assert_eq!(body["reload"],"/api/_clear_sw");
        let cookie=headers["set-cookie"].to_str().unwrap();
        assert!(cookie.contains("HttpOnly; Secure; SameSite=Lax"));assert!(!cookie.contains("owner-secret"));
        let (_,_,owner)=call(&router,"/api/connection/security",None,None,Some(cookie)).await;
        assert_eq!(owner["auth"],"owner");
        // Reload uses the same existing session helper; rotation invalidates it.
        let rotated=app(Some("rotated-secret"),dir.path());
        let (_,_,revoked)=call(&rotated,"/api/connection/security",None,None,Some(cookie)).await;
        assert_eq!(revoked["auth"],"sign_in_required");assert_eq!(revoked["owner_session"],"invalid");
        let open=app(None,dir.path());
        assert_eq!(call(&open,"/api/connection/session",Some(json!({"token":"owner-secret"})),Some("https://localhost:18972"),None).await.0,StatusCode::UNAUTHORIZED);
    }
    #[tokio::test]
    async fn connection_security_certificate_route_rejects_members_and_bad_origin_then_persists_matching_pair() {
        let dir=tempfile::tempdir().unwrap();let router=app(Some("owner-secret"),dir.path());
        let tls_dir=dir.path().join("tls");
        let original=crate::tls::load_or_generate(&tls_dir).unwrap();
        // A real scoped member cookie, resolved by production identity middleware.
        let conn=rusqlite::Connection::open(dir.path().join("test.db")).unwrap();
        conn.execute("INSERT INTO org_members(id,email,role,joined_at,scope_level,scope_name) VALUES('scoped','member@example.test','member',1,'worker','only-worker')",[]).unwrap();
        conn.execute("INSERT INTO org_invites(token,created_at,expires_at,used_at,used_by) VALUES('member',1,9999999999,1,'scoped')",[]).unwrap();
        let (_,_,member)=call(&router,"/api/connection/security",None,None,Some("amux_member=member")).await;
        assert_eq!(member["auth"],"member");assert_eq!(member["can_configure"],false);
        assert_eq!(call(&router,"/api/connection/session",Some(json!({"token":""})),Some("https://localhost:18972"),Some("amux_member=member")).await.0,StatusCode::UNAUTHORIZED);

        let cert=rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let payload=json!({"certificate_pem":cert.cert.pem(),"private_key_pem":cert.key_pair.serialize_pem()});
        for cookie in [None,Some("amux_member=member"),Some("__Host-amux_owner=revoked")] {
            let (status,_,body)=call(&router,"/api/connection/certificate",Some(payload.clone()),Some("https://localhost:18972"),cookie).await;
            assert_eq!(status,StatusCode::FORBIDDEN);assert_eq!(body["error"],"owner_session_or_token_required");
            assert!(!tls_dir.join("connection.pem").exists());
        }
        let (_,headers,_)=call(&router,"/api/connection/session",Some(json!({"token":"owner-secret"})),Some("https://localhost:18972"),None).await;
        let cookie=headers["set-cookie"].to_str().unwrap();
        assert_eq!(call(&router,"/api/connection/certificate",Some(payload.clone()),Some("https://evil.test"),Some(cookie)).await.0,StatusCode::FORBIDDEN);
        let wrong=rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let (status,_,body)=call(&router,"/api/connection/certificate",Some(json!({"certificate_pem":cert.cert.pem(),"private_key_pem":wrong.key_pair.serialize_pem()})),Some("https://localhost:18972"),Some(cookie)).await;
        assert_eq!(status,StatusCode::UNPROCESSABLE_ENTITY);assert_eq!(body["error"],"certificate_invalid_or_key_mismatch");
        assert_eq!(crate::tls::load_or_generate(&tls_dir).unwrap().cert_pem,original.cert_pem);
        let (status,_,saved)=call(&router,"/api/connection/certificate",Some(payload),Some("https://localhost:18972"),Some(cookie)).await;
        assert_eq!(status,StatusCode::OK);assert_eq!(saved["applied"],false);assert_eq!(saved["restart_required"],true);
        assert!(!saved.to_string().contains("PRIVATE KEY"));
        assert!(tls_dir.join("connection.pem").exists());
        let (_,_,state)=call(&router,"/api/connection/security",None,None,Some(cookie)).await;
        assert_eq!(state["tls"]["saved_sha256"],saved["saved"]["sha256"]);
        assert_eq!(state["tls"]["trust"],"unknown_to_server");
        // API refusal before persistence, not merely a UI disabled button.
        let snapshot=std::fs::read(tls_dir.join("connection.pem")).unwrap();
        assert_eq!(call(&router,"/api/connection/certificate",Some(json!({"certificate_pem":"invalid-private-sentinel","private_key_pem":"secret-sentinel"})),Some("https://localhost:18972"),Some(cookie)).await.0,StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(snapshot,std::fs::read(tls_dir.join("connection.pem")).unwrap());
    }
}
