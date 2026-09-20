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
        .route("/{name}/commands", axum::routing::post(command))
        .route("/{name}/tasks/{id}/report", axum::routing::post(report))
        .route("/{name}/tasks/{id}/wait", axum::routing::post(wait))
        .route("/{name}/tasks/{id}/retry", axum::routing::post(retry))
        .route("/{name}/migration/preview", axum::routing::post(preview))
        .route("/{name}/migration/apply", axum::routing::post(migrate))
        .route("/{name}/migration/rollback", axum::routing::post(rollback))
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
    if [&body.policy.coordinator, &body.policy.executor]
        .iter()
        .any(|p| {
            !super::session_verbs::SESSION_PROVIDERS.contains(&p.provider.as_str())
                || p.provider == "iterm2"
        })
    {
        return error(StatusCode::BAD_REQUEST, "unsupported execution provider");
    }
    if body.policy.coordinator.provider != "claude" {
        return error(
            StatusCode::BAD_REQUEST,
            "read-only intake currently supports Claude coordinator models",
        );
    }
    let stop = body.policy.paused || !body.policy.enabled;
    let key = name.clone();
    match state
        .store
        .write_async(move |c| {
            store::save(c, &key, body.expect_rev, &body.policy, "operator")
                .map_err(store::sql_error)
        })
        .await
    {
        Ok(_) => {
            if let Err(e) = crate::project_execution::driver::apply_pause(&state, &name, stop).await
            {
                return error(
                    StatusCode::CONFLICT,
                    format!("policy saved; process stop pending: {e}"),
                );
            }
            match state
                .store
                .read_async(move |c| store::board(c, &name))
                .await
            {
                Ok(mut value) => {
                    value["applied"] = json!(true);
                    Json(value).into_response()
                }
                Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
            }
        }
        Err(e) => error(
            if e.to_string().contains("revision conflict")
                || e.to_string().contains("fixed while executions")
            {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    idempotency_key: String,
    text: String,
}
async fn command(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Command>,
) -> Response {
    if !permitted(&headers, &name) {
        return error(StatusCode::FORBIDDEN, "outside project scope");
    }
    let captured = std::sync::Arc::new(std::sync::Mutex::new(0));
    let copy = captured.clone();
    let result = state
        .store
        .write_async(move |c| {
            let (id, out) = crate::project_execution::intake::receive(
                c,
                &name,
                &body.idempotency_key,
                &body.text,
            )
            .map_err(store::sql_error)?;
            *copy.lock().expect("receipt") = id;
            Ok(out)
        })
        .await;
    match result {
        Ok(_) => (
            StatusCode::ACCEPTED,
            Json(json!({"id":*captured.lock().expect("receipt"),"state":"accepted","phase":"accepted"})),
        )
            .into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportBody {
    generation: i64,
    input_hash: String,
    report: crate::project_execution::planner::Report,
}
async fn report(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<ReportBody>,
) -> Response {
    if !permitted(&headers, &name) {
        return error(StatusCode::FORBIDDEN, "outside project scope");
    }
    let worker = groups::hdr_worker(&headers);
    match state
        .store
        .write_async(move |c| {
            crate::project_execution::planner::record_report(
                c,
                &name,
                &id,
                &worker,
                body.generation,
                &body.input_hash,
                &body.report,
            )
            .map_err(store::sql_error)
        })
        .await
    {
        Ok(_) => Json(json!({"state":"reported","applied":true,"verified":false})).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitBody {
    generation: i64,
    input_hash: String,
    reason: String,
    category: String,
}
async fn wait(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<WaitBody>,
) -> Response {
    if !permitted(&headers, &name) {
        return error(StatusCode::FORBIDDEN, "outside project scope");
    }
    if body.reason.trim().is_empty()
        || !["operational", "spend", "customer_outbound"].contains(&body.category.as_str())
    {
        return error(
            StatusCode::BAD_REQUEST,
            "a concrete reason and valid category are required",
        );
    }
    let worker = groups::hdr_worker(&headers);
    match state
        .store
        .write_async(move |c| {
            let row = crate::db::board_store::get_issue(c, &id)?
                .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
            let mut e =
                crate::project_execution::planner::execution(c, &id).map_err(store::sql_error)?;
            if row.project_group.as_deref() != Some(&name)
                || e.worker != worker
                || e.generation != body.generation
                || e.input_hash != body.input_hash
                || e.input_hash != crate::project_execution::planner::input_hash(&row)
                || !matches!(e.stage.as_str(), "working" | "reserved")
            {
                return Err(rusqlite::Error::InvalidQuery);
            }
            e.stage = "waiting".into();
            e.waiting = Some(format!("{}: {}", body.category, body.reason));
            crate::project_execution::planner::save_execution(c, &row, &e, "project.waiting")
                .map_err(store::sql_error)
        })
        .await
    {
        Ok(_) => Json(json!({"state":"waiting","phase":"waiting"})).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Migrate {
    source_workers: Vec<String>,
    fingerprint: String,
}
async fn migrate(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Migrate>,
) -> Response {
    if !operator(&headers) {
        return error(StatusCode::FORBIDDEN, "migration requires operator scope");
    }
    for worker in &body.source_workers {
        if !super::session_verbs::valid_session_name(worker) {
            return error(StatusCode::BAD_REQUEST, "invalid source worker");
        }
        let path = super::session_verbs::env_path(worker);
        let env = super::session_verbs::parse_env(worker);
        if !path.exists()
            || ["CC_PAUSED", "CC_ARCHIVED", "CC_ISOLATED"]
                .iter()
                .any(|key| env.get(key) == Some("1"))
        {
            return error(
                StatusCode::CONFLICT,
                format!("protected or missing source worker {worker}"),
            );
        }
    }
    match state
        .store
        .write_async(move |c| {
            store::apply_migration(c, &name, &body.source_workers, &body.fingerprint)
                .map_err(store::sql_error)
        })
        .await
    {
        Ok(_) => Json(json!({"state":"migrated","applied":true})).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rollback {
    migration: String,
}
async fn rollback(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Rollback>,
) -> Response {
    if !operator(&headers) {
        return error(StatusCode::FORBIDDEN, "rollback requires operator scope");
    }
    match state
        .store
        .write_async(move |c| {
            store::rollback_migration(c, &name, &body.migration).map_err(store::sql_error)
        })
        .await
    {
        Ok(_) => Json(json!({"state":"rolled_back","applied":true})).into_response(),
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

async fn retry(
    State(state): State<AppState>,
    Path((name, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !operator(&headers) {
        return error(StatusCode::FORBIDDEN, "retry requires operator scope");
    }
    match state.store.write_async(move|c| {
        let row=crate::db::board_store::get_issue(c,&id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        let mut e=crate::project_execution::planner::execution(c,&id).map_err(store::sql_error)?;
        if row.project_group.as_deref()!=Some(&name) || matches!(e.stage.as_str(),"working"|"reserved"|"reported"|"verifying") || row.status=="verified" {return Err(rusqlite::Error::InvalidQuery);}
        e.last_failure=e.waiting.take().or(e.last_failure);e.stage.clear();e.attempt=0;e.input_hash.clear();e.report=None;
        c.execute("UPDATE issues SET status='todo',lease_owner=NULL,lease_expires_at=NULL WHERE id=?1",[&id])?;
        crate::project_execution::planner::save_execution(c,&row,&e,"project.operator_retry").map_err(store::sql_error)
    }).await {Ok(_)=>Json(json!({"state":"ready","applied":true})).into_response(),Err(e)=>error(StatusCode::CONFLICT,e)}
}

/// A temporary executor cannot expand its resource graph through legacy APIs.
/// Claims, budgets and delegation remain decisions of its project controller.
pub(crate) fn executor_mutation_guard(
    method: &axum::http::Method,
    path: &str,
    headers: &HeaderMap,
) -> Option<Response> {
    if matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    ) {
        return None;
    }
    let worker = groups::hdr_worker(headers);
    if worker.is_empty() {
        return None;
    }
    let env = super::session_verbs::parse_env(&worker);
    let project = env.get("CC_PROJECT")?;
    let owns_report = path.starts_with(&format!("/api/projects/{project}/tasks/"))
        && (path.ends_with("/report") || path.ends_with("/wait"));
    let runtime_report =
        path == format!("/api/sessions/{worker}/report") || path == "/api/client-debug";
    let graph_mutation = [
        "/api/board",
        "/api/workers",
        "/api/sessions",
        "/api/groups",
        "/api/projects",
        "/api/schedules",
        "/api/messages",
    ]
    .iter()
    .any(|prefix| path.starts_with(prefix));
    if owns_report || runtime_report || !graph_mutation {
        None
    } else {
        tracing::warn!(session=%worker,project,path,verdict="project_executor_scope_refused",measured=true,n_considered=1,"temporary executor cannot create independent boards or resources");
        Some(error(StatusCode::FORBIDDEN,"project executors report task results; command intake, claims and resource policy belong to the project"))
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
        assert_eq!(super::super::interactions::classify(200, &value), "applied");
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
