//! Project views/configuration over existing groups and board issues.
use super::{groups, org, AppState};
use crate::project_execution::store;
use amux_core::project::ExecutionPolicy;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/{name}", get(detail).put(configure))
        .route("/{name}/migration/preview", axum::routing::post(preview))
}

pub(crate) fn permitted(headers: &HeaderMap, name: &str) -> bool {
    let member_allowed = match org::local_member_scope(headers) {
        Some(org::MemberScope::Group(group)) => group.eq_ignore_ascii_case(name),
        Some(org::MemberScope::Worker(worker)) => {
            let mut scoped = HeaderMap::new();
            let Ok(value) = worker.parse() else {
                return false;
            };
            scoped.insert("x-amux-worker", value);
            groups::caller_scope(&crate::config::amux_home(), &scoped)
                .1
                .contains(name)
        }
        Some(org::MemberScope::Global) | None => true,
    };
    let (scoped, tags, _) = groups::caller_scope(&crate::config::amux_home(), headers);
    member_allowed && (!scoped || tags.contains(name))
}

pub(crate) fn operator(headers: &HeaderMap) -> bool {
    groups::hdr_worker(headers).is_empty()
        && matches!(
            org::local_member_scope(headers),
            None | Some(org::MemberScope::Global)
        )
}
fn error(status: StatusCode, message: impl ToString) -> Response {
    (
        status,
        Json(json!({"error":message.to_string(),"measured":false,"n_considered":0})),
    )
        .into_response()
}
async fn list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    match state.store.read_async(store::list).await {
        Ok(mut rows) => {
            rows.retain(|p| permitted(&headers, &p.name));
            Json(json!({"measured":true,"n_considered":rows.len(),"projects":rows})).into_response()
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}
async fn detail(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !permitted(&headers, &name) {
        return error(StatusCode::FORBIDDEN, "outside project scope");
    }
    match state
        .store
        .read_async(move |c| store::board(c, &name))
        .await
    {
        Ok(value) => Json(value).into_response(),
        Err(e) => error(
            if e.to_string() == "project not found" {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            },
            e,
        ),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    expect_rev: i64,
    policy: ExecutionPolicy,
}
async fn configure(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Configure>,
) -> Response {
    if !operator(&headers) {
        return error(
            StatusCode::FORBIDDEN,
            "project resource policy is an operator setting",
        );
    }
    if !amux_core::project::valid_name(&name) {
        return error(StatusCode::BAD_REQUEST, "invalid project name");
    }
    if let Err(e) = body.policy.validate() {
        return error(StatusCode::BAD_REQUEST, e);
    }
    if body.policy.enabled {
        return error(
            StatusCode::CONFLICT,
            "project dispatch is not enabled until the execution stage is installed",
        );
    }
    if [&body.policy.coordinator, &body.policy.executor]
        .iter()
        .any(|p| {
            !super::session_verbs::SESSION_PROVIDERS.contains(&p.provider.as_str())
                || p.provider == "iterm2"
        })
    {
        return error(StatusCode::BAD_REQUEST, "unsupported execution provider");
    }
    let key = name.clone();
    match state
        .store
        .write_async(move |c| {
            store::save(c, &key, body.expect_rev, &body.policy, "operator")
                .map_err(store::sql_error)
        })
        .await
    {
        Ok(_) => detail(State(state), Path(name), headers).await,
        Err(e) => error(
            if e.to_string().contains("revision conflict") {
                StatusCode::CONFLICT
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            },
            e,
        ),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preview {
    source_workers: Vec<String>,
}
async fn preview(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Preview>,
) -> Response {
    if !operator(&headers) {
        return error(
            StatusCode::FORBIDDEN,
            "migration preview requires operator scope",
        );
    }
    let home = crate::config::amux_home();
    match state
        .store
        .read_async(move |c| store::preview(c, &name, &body.source_workers))
        .await
    {
        Ok(mut p) => {
            for worker in &p.source_workers {
                let path = home.join("sessions").join(format!("{worker}.env"));
                if !path.exists() {
                    p.conflicts.push(format!(
                        "source worker {worker} has no active configuration"
                    ));
                    continue;
                }
                let config = crate::config::parse_env_file(&path);
                for key in ["CC_PAUSED", "CC_ARCHIVED", "CC_ISOLATED"] {
                    if config.get(key).is_some_and(|v| v == "1") {
                        p.conflicts.push(format!("source worker {worker} is protected by {key}; its work is not eligible for migration"));
                    }
                }
            }
            Json(json!(p)).into_response()
        }
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use tower::ServiceExt;
    #[tokio::test]
    async fn configured_empty_project_is_visible_and_unknown_workers_cannot_read_or_change_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::db::Store::open(&dir.path().join("db")).unwrap()),
            started: std::time::Instant::now(),
            build_hash: "project-test".into(),
            auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let app = routes().with_state(state);
        let body = json!({"expect_rev":0,"policy":{"repository":"/repo","coordinator":{"provider":"claude","model":"haiku"},"executor":{"provider":"claude","model":"sonnet"},"verify_command":"./verify.sh"}});
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/example")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let value: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(value["project"]["revision"], 1);
        assert_eq!(value["cards"], json!([]));
        assert_eq!(value["usage"]["tokens"], serde_json::Value::Null);
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/example")
                    .header("x-amux-worker", "project-fixture-unknown-worker")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/example")
                    .header("x-amux-worker", "project-fixture-unknown-worker")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
