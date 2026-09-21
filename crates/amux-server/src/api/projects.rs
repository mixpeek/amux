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
        .route("/{name}/legacy-receipts/{id}/cancel", axum::routing::post(cancel_legacy_receipt))
        .route("/{name}/commands/{id}/retry", axum::routing::post(retry_intake))
        .route("/{name}/tasks/{id}/report", axum::routing::post(report))
        .route("/{name}/tasks/{id}/wait", axum::routing::post(wait))
        .route("/{name}/tasks/{id}/required-outputs", axum::routing::post(required_outputs))
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
    if !matches!(body.policy.coordinator.provider.as_str(), "claude" | "codex") {
        tracing::warn!(provider=%body.policy.coordinator.provider,model=%body.policy.coordinator.model,
            measured=true,n_considered=1,verdict="project_coordinator_unsupported",
            "project configuration refused an unsupported intake provider");
        return error(
            StatusCode::BAD_REQUEST,
            "read-only intake supports Claude and Codex coordinator models",
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
async fn retry_intake(
    State(state): State<AppState>, Path((name,id)): Path<(String,i64)>,
    headers: HeaderMap, Json(body): Json<crate::project_execution::intake_retry::Request>,
) -> Response {
    if !operator(&headers) { return error(StatusCode::FORBIDDEN,"intake retry requires operator scope"); }
    let project=name.clone();
    match state.store.write_async(move |c|crate::project_execution::intake_retry::grant(c,&project,id,&body,chrono::Utc::now().timestamp()).map_err(store::sql_error)).await {
        Ok(out) => Json(json!({"ok":true,"applied":out.applied,"message_id":id})).into_response(),
        Err(e) => {
            tracing::warn!(project=name,message_id=id,error=%e,measured=true,n_considered=1,verdict="project_intake_retry_refused","intake retry preconditions did not hold");
            error(StatusCode::CONFLICT,e)
        }
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

/// The dedicated project worker remains bound to one task across attempts.
pub(crate) fn executor_task(c:&rusqlite::Connection,worker:&str)->anyhow::Result<Option<(String,String)>> {
    let env=super::session_verbs::parse_env(worker);
    let Some(project)=env.get("CC_PROJECT") else {return Ok(None)};
    let id=env.get("CC_BOARD_CARD").ok_or_else(||anyhow::anyhow!("project worker has no task"))?;
    let row=crate::db::board_store::get_issue(c,id)?.ok_or_else(||anyhow::anyhow!("project task missing"))?;
    let e=crate::project_execution::planner::execution(c,id)?;
    anyhow::ensure!(row.project_group.as_deref()==Some(project) && e.worker==worker && row.session.as_deref()==Some(worker) && row.archived==0 && !crate::db::board_store::is_terminal_status(&row.status),"project worker task identity changed");
    Ok(Some((project.into(),id.into())))
}
/// Owner input is durable intent, never an authorization to start an attempt.
pub(crate) fn executor_steering_hold(c:&rusqlite::Connection,worker:&str)->anyhow::Result<Option<String>> {
    let Some((name,id))=executor_task(c,worker).ok().flatten() else {return Ok(Some("project_task_identity_changed".into()))};
    let Some(project)=store::get(c,&name)? else {return Ok(Some("project_missing".into()))};
    if !project.policy.enabled {return Ok(Some("project_disabled".into()))}
    if project.policy.paused {return Ok(Some("project_paused".into()))}
    if let Some(reason)=crate::project_execution::usage::waiting(c,&project)? {return Ok(Some(reason))}
    let row=crate::db::board_store::get_issue(c,&id)?.ok_or_else(||anyhow::anyhow!("task missing"))?;
    let e=crate::project_execution::planner::execution(c,&id)?;
    if e.input_hash!=crate::project_execution::planner::input_hash(&row) {return Ok(Some("project_requirements_changed".into()))}
    if crate::project_execution::outputs::authorization_hold(c,&row)? || matches!(e.wait_category.as_deref(),Some("spend"|"customer_outbound")) {return Ok(Some("project_authorization_required".into()))}
    if e.wait_category.as_deref()==Some("required_outputs") || !crate::project_execution::outputs::ready(c,&row)? || e.output_wait.as_ref().is_some_and(|w|w.continued_generation.is_none()) {return Ok(Some("project_required_outputs".into()))}
    if e.suspended || e.stage!="working" || row.status!="doing" || e.waiting.is_some() || e.attempt==0 || e.generation<=0 {
        return Ok(Some(format!("project_active_claim_required:{}",e.stage)));
    }
    let packet_pending:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM steering_queue WHERE id=?1)",[&e.delivery_id],|r|r.get(0))?;
    if packet_pending {return Ok(Some("project_claim_packet_pending".into()))}
    Ok(None)
}
#[cfg(test)]
pub(crate) fn executor_steering_allowed(c:&rusqlite::Connection,worker:&str)->anyhow::Result<bool> {
    Ok(executor_steering_hold(c,worker)?.is_none())
}
/// Bind queued owner notes to their original task; unbound historical rows fail closed.
pub(crate) fn steering_delivery_hold(c:&rusqlite::Connection,worker:&str,id:&str)->anyhow::Result<Option<String>> {
    let (guard,card):(String,Option<String>)=c.query_row("SELECT COALESCE(guard,''),precond_card FROM steering_queue WHERE id=?1 AND session=?2",rusqlite::params![id,worker],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let env=super::session_verbs::parse_env(worker);
    let Some(project)=env.get("CC_PROJECT") else {return Ok(if guard=="project-steering" {Some("project_task_identity_changed".into())} else {None})};
    if guard=="project-execution" {
        return Ok((!crate::project_execution::planner::delivery_current(c,project,worker,id)?).then(||"project_claim_delivery_stale".into()));
    }
    if guard!="project-steering" || card.as_deref()!=env.get("CC_BOARD_CARD") || card.is_none() {return Ok(Some("project_steering_task_binding_changed".into()))}
    executor_steering_hold(c,worker)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelLegacy {idempotency_key:String,expect_attempts:i64,reason:String,superseded_by_steering:String}
async fn cancel_legacy_receipt(State(state):State<AppState>,Path((project,id)):Path<(String,i64)>,headers:HeaderMap,Json(body):Json<CancelLegacy>)->Response {
    if !operator(&headers){return error(StatusCode::FORBIDDEN,"cancellation requires operator scope");}
    match state.store.write_async(move|c| {
        let (worker,pending,attempts,retry,card,raw):(String,bool,i64,i64,Option<String>,Option<String>)=c.query_row("SELECT session,capture_pending,intake_attempts,intake_retry_at,card_id,intake_result FROM cmd_history WHERE id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))?;
        let task=executor_task(c,&worker).map_err(store::sql_error)?.ok_or(rusqlite::Error::InvalidQuery)?;
        let prior:serde_json::Value=raw.and_then(|r|serde_json::from_str(&r).ok()).unwrap_or(serde_json::Value::Null);
        if task.0!=project || body.idempotency_key.is_empty() || body.idempotency_key.len()>160 || body.reason.trim().is_empty() || body.reason.len()>4000 {return Err(rusqlite::Error::InvalidQuery);}
        if prior["state"]=="cancelled" && prior["key"]==body.idempotency_key && prior["superseded_by_steering"]==body.superseded_by_steering && prior["reason"]==body.reason && prior["expect_attempts"]==body.expect_attempts {return Ok(crate::db::WriteOutcome{applied:false,events:vec![]});}
        let original:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM steering_queue WHERE id=?1 AND session=?2 UNION ALL SELECT 1 FROM steering_history WHERE id=?1 AND session=?2)",rusqlite::params![body.superseded_by_steering,worker],|r|r.get(0))?;
        if !pending || card.is_some() || attempts!=body.expect_attempts || retry>chrono::Utc::now().timestamp() || !original {return Err(rusqlite::Error::InvalidQuery);}
        c.execute("UPDATE cmd_history SET capture_pending=0,intake_result=?2 WHERE id=?1",rusqlite::params![id,json!({"state":"cancelled","key":body.idempotency_key,"reason":body.reason,"expect_attempts":attempts,"superseded_by_steering":body.superseded_by_steering,"prior_result":prior}).to_string()])?;
        tracing::info!(message_id=id,project,measured=true,n_considered=1,verdict="project.legacy_receipt_cancelled","duplicate legacy intake retained as cancelled; original steering unchanged");
        Ok(crate::db::WriteOutcome{applied:true,events:vec![]})
    }).await {Ok(out)=>Json(json!({"applied":out.applied,"state":"cancelled","submitted":false})).into_response(),Err(e)=>error(StatusCode::CONFLICT,e)}
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
async fn required_outputs(
    State(state):State<AppState>,Path((name,id)):Path<(String,String)>,headers:HeaderMap,
    Json(body):Json<crate::project_execution::outputs::Request>,
)->Response {
    if !permitted(&headers,&name) {return error(StatusCode::FORBIDDEN,"outside project scope");}
    let worker=groups::hdr_worker(&headers);let project=name.clone();let task=id.clone();
    match state.store.write_async(move |c|crate::project_execution::outputs::declare(c,&project,&task,&worker,&body).map_err(store::sql_error)).await {
        Ok(out)=>Json(json!({"applied":out.applied,"task":id,"verified":false})).into_response(),
        Err(e)=>{tracing::warn!(project=name,task=id,error=%e,measured=true,n_considered=1,verdict="project.outputs_refused","structured required-output declaration refused");error(StatusCode::CONFLICT,e)}
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
            e.wait_category=Some(body.category.clone());
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
    State(state): State<AppState>,Path((name,id)):Path<(String,String)>,headers:HeaderMap,
    Json(body):Json<crate::project_execution::task_retry::Request>,
)->Response {
    if !operator(&headers){return error(StatusCode::FORBIDDEN,"retry requires operator scope");}
    let project=name.clone();let task=id.clone();
    match state.store.write_async(move|c|crate::project_execution::task_retry::grant(c,&project,&task,&body).map_err(store::sql_error)).await {
        Ok(out)=>Json(json!({"state":"ready","applied":out.applied})).into_response(),
        Err(e)=>{tracing::warn!(project=name,task=id,error=%e,measured=true,n_considered=1,verdict="project.retry_refused","operator retry refused");error(StatusCode::CONFLICT,e)}
    }
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
        && (path.ends_with("/report") || path.ends_with("/wait") || path.ends_with("/required-outputs"));
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
    #[test]
    fn project_report_rejects_source_commands_before_mutation_and_accepts_same_attempt_correction() {
        let home=tempfile::tempdir().unwrap();let _home=crate::api::settings::test_env::set_home(home.path());
        let repo=home.path().join("repository");std::fs::create_dir(&repo).unwrap();
        let canonical=std::fs::canonicalize(&repo).unwrap();let alias=home.path().join("repo-alias");
        std::os::unix::fs::symlink(&repo,&alias).unwrap();
        let db=crate::db::Store::open(&home.path().join("db")).unwrap();
        db.write(move |c| {
            let policy=serde_json::from_value(json!({"repository":alias.to_string_lossy(),"enabled":true,"coordinator":{"provider":"codex","model":"gpt-6-astra"},"executor":{"provider":"codex","model":"gpt-6-astra"},"verify_command":"./verify.sh"})).unwrap();
            store::save(c,"sample",0,&policy,"test").map_err(store::sql_error)?;
            c.execute("INSERT INTO issues(id,title,status,type,project_group,next_action,acceptance_criteria,created,updated) VALUES('A','Output','todo','doc','sample','Write report','[\"Output passes\"]',1,1)",[])?;
            crate::project_execution::planner::claim(c,"sample","A").map_err(store::sql_error)?;
            let row=crate::db::board_store::get_issue(c,"A")?.unwrap();let mut e=crate::project_execution::planner::execution(c,"A").unwrap();e.stage="working".into();
            crate::project_execution::planner::save_execution(c,&row,&e,"project.execution").map_err(store::sql_error)
        }).unwrap();
        let e=crate::project_execution::planner::execution(&db.read().unwrap(),"A").unwrap();
        crate::project_execution::planner::register_test_workspace(&e.worker,canonical.to_str().unwrap());
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();std::fs::write(home.path().join("sessions").join(format!("{}.env",e.worker)),"CC_PROJECT=sample\nCC_TAGS=sample\n").unwrap();
        let db=std::sync::Arc::new(db);
        let state=AppState{store:db.clone(),started:std::time::Instant::now(),build_hash:"test".into(),auth_token:None,reconciled:std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true))};
        let app=routes().with_state(state);
        let marker=home.path().join("verification-must-not-run");
        let body=json!({"generation":e.generation,"input_hash":e.input_hash,"report":{"head":"a".repeat(40),"summary":"candidate","checks":[{"criterion":"Output passes","command":format!("touch {}; {}/venv/bin/python tests/check.py",marker.display(),canonical.display())}]}});
        let before=crate::db::board_store::get_issue(&db.read().unwrap(),"A").unwrap().unwrap().snapshot_slim();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for (generation,caller) in [(e.generation+1,e.worker.as_str()),(e.generation,"foreign"),(e.generation,e.worker.as_str())] {
                let mut rejected=body.clone();rejected["generation"]=json!(generation);
                let response=app.clone().oneshot(axum::http::Request::builder().method("POST").uri("/sample/tasks/A/report").header("x-amux-session",caller).header("content-type","application/json").body(Body::from(rejected.to_string())).unwrap()).await.unwrap();
                assert!(!response.status().is_success());
                if generation==e.generation && caller==e.worker {
                    let bytes=axum::body::to_bytes(response.into_body(),usize::MAX).await.unwrap();
                    assert!(String::from_utf8_lossy(&bytes).contains("resubmit the same generation"));
                }
                let c=db.read().unwrap();assert_eq!(crate::db::board_store::get_issue(&c,"A").unwrap().unwrap().snapshot_slim(),before);
                assert!(crate::project_execution::planner::execution(&c,"A").unwrap().report.is_none());
                let p=store::get(&c,"sample").unwrap().unwrap();
                assert_ne!(crate::project_execution::planner::plan(&c,&p).unwrap().into_iter().find(|p|p.id=="A").unwrap().action,"verify");
                assert!(!marker.exists());
            }
            let mut corrected=body;corrected["report"]["checks"][0]["command"]=json!("test -f report.md");
            let registered=crate::fanout_workspace::load(home.path(),&e.worker).unwrap();
            for foreign_repo in [false,true] {
                let mut wrong=registered.clone();
                if foreign_repo {wrong.repo=home.path().to_string_lossy().into_owned();} else {wrong.branch="amux/fanout/foreign".into();}
                crate::fanout_workspace::save(home.path(),&e.worker,&wrong).unwrap();
                let response=app.clone().oneshot(axum::http::Request::builder().method("POST").uri("/sample/tasks/A/report").header("x-amux-session",&e.worker).header("content-type","application/json").body(Body::from(corrected.to_string())).unwrap()).await.unwrap();
                assert_eq!(response.status(),StatusCode::CONFLICT);
                assert_eq!(crate::db::board_store::get_issue(&db.read().unwrap(),"A").unwrap().unwrap().snapshot_slim(),before);
            }
            crate::fanout_workspace::save(home.path(),&e.worker,&registered).unwrap();
            let response=app.oneshot(axum::http::Request::builder().method("POST").uri("/sample/tasks/A/report").header("x-amux-session",&e.worker).header("content-type","application/json").body(Body::from(corrected.to_string())).unwrap()).await.unwrap();assert_eq!(response.status(),StatusCode::OK);
        });
        let after=crate::project_execution::planner::execution(&db.read().unwrap(),"A").unwrap();assert_eq!(after.stage,"reported");assert_eq!(after.generation,e.generation);assert_eq!(after.attempt,e.attempt);assert!(!marker.exists());
    }
    #[test]
    fn project_outputs_supported_api_is_scoped_strict_and_idempotent() {
        let home=tempfile::tempdir().unwrap();let _home=crate::api::settings::test_env::set_home(home.path());
        let (_dir,db,body)=crate::project_execution::outputs::tests::fixture();
        let worker=crate::project_execution::planner::execution(&db.read().unwrap(),"A").unwrap().worker;
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        std::fs::write(home.path().join("sessions").join(format!("{worker}.env")),"CC_PROJECT=sample\nCC_TAGS=sample\n").unwrap();
        let state=AppState{store:std::sync::Arc::new(db),started:std::time::Instant::now(),build_hash:"test".into(),auth_token:None,reconciled:std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true))};
        let app=routes().with_state(state);
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for (caller,path,expected) in [("foreign","/sample/tasks/A/required-outputs",StatusCode::FORBIDDEN),(worker.as_str(),"/other/tasks/A/required-outputs",StatusCode::FORBIDDEN),(worker.as_str(),"/sample/tasks/B/required-outputs",StatusCode::CONFLICT),(worker.as_str(),"/sample/tasks/A/required-outputs",StatusCode::OK),(worker.as_str(),"/sample/tasks/A/required-outputs",StatusCode::OK)] {
                let response=app.clone().oneshot(axum::http::Request::builder().method("POST").uri(path).header("x-amux-session",caller).header("content-type","application/json").body(Body::from(serde_json::to_string(&body).unwrap())).unwrap()).await.unwrap();
                assert_eq!(response.status(),expected,"{caller} {path}");
            }
            let mut invalid=serde_json::to_value(&body).unwrap();invalid["category"]=json!("spend");
            let response=app.oneshot(axum::http::Request::builder().method("POST").uri("/sample/tasks/A/required-outputs").header("x-amux-session",&worker).header("content-type","application/json").body(Body::from(invalid.to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(),StatusCode::UNPROCESSABLE_ENTITY);
        });
        let mut headers=HeaderMap::new();headers.insert("x-amux-session",worker.parse().unwrap());
        assert!(executor_mutation_guard(&axum::http::Method::POST,"/api/projects/sample/tasks/A/required-outputs",&headers).is_none());
        assert!(executor_mutation_guard(&axum::http::Method::POST,"/api/projects/other/tasks/A/required-outputs",&headers).is_some());
    }
    #[test]
    fn project_retry_and_legacy_cancellation_routes_fail_closed_and_preserve_history() {
        let home=tempfile::tempdir().unwrap();let _home=crate::api::settings::test_env::set_home(home.path());
        let (_dir,db,_)=crate::project_execution::outputs::tests::fixture();
        let e=crate::project_execution::planner::execution(&db.read().unwrap(),"A").unwrap();
        let row=crate::db::board_store::get_issue(&db.read().unwrap(),"A").unwrap().unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();std::fs::write(home.path().join("sessions").join(format!("{}.env",e.worker)),"CC_PROJECT=sample\nCC_BOARD_CARD=A\n").unwrap();
        let worker=e.worker.clone();
        db.write(move|c| {c.execute("INSERT INTO cmd_history(id,text,type,session,ts,capture_pending,intake_result) VALUES(42,'duplicate','user',?1,1,1,'{\"old_failure\":\"retained\"}')",[&worker])?;c.execute("INSERT INTO steering_queue(id,session,text,queued_at) VALUES('original',?1,'sole delivery',1)",[&worker])?;Ok(crate::db::WriteOutcome{applied:true,events:vec![]})}).unwrap();
        let state=AppState{store:std::sync::Arc::new(db),started:std::time::Instant::now(),build_hash:"test".into(),auth_token:None,reconciled:std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true))};let app=routes().with_state(state.clone());
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let retry=json!({"idempotency_key":"retry","expect_generation":e.generation,"expect_revision":row.rev,"input_hash":e.input_hash});
            let cancel=json!({"idempotency_key":"cancel","expect_attempts":0,"reason":"duplicate instruction","superseded_by_steering":"original"});
            for (path,body,worker,status) in [
                ("/sample/tasks/A/retry",json!({}),false,StatusCode::UNPROCESSABLE_ENTITY),
                ("/sample/tasks/A/retry",retry.clone(),true,StatusCode::FORBIDDEN),
                ("/other/tasks/A/retry",retry.clone(),false,StatusCode::CONFLICT),
                ("/sample/legacy-receipts/42/cancel",cancel.clone(),true,StatusCode::FORBIDDEN),
                ("/other/legacy-receipts/42/cancel",cancel.clone(),false,StatusCode::CONFLICT),
                ("/sample/legacy-receipts/42/cancel",cancel.clone(),false,StatusCode::OK),
                ("/sample/legacy-receipts/42/cancel",cancel,false,StatusCode::OK),
                ("/sample/tasks/A/retry",retry.clone(),false,StatusCode::OK),
                ("/sample/tasks/A/retry",retry,false,StatusCode::OK)] {
                let mut req=axum::http::Request::builder().method("POST").uri(path).header("content-type","application/json");if worker {req=req.header("x-amux-worker","executor");}
                let response=app.clone().oneshot(req.body(Body::from(body.to_string())).unwrap()).await.unwrap();assert_eq!(response.status(),status,"{path}, worker={worker}");
            }
        });
        let c=state.store.read().unwrap();let (pending,result):(bool,String)=c.query_row("SELECT capture_pending,intake_result FROM cmd_history WHERE id=42",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert!(!pending);assert!(result.contains("retained"));
        assert_eq!(c.query_row("SELECT COUNT(*) FROM steering_queue WHERE id='original'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    }
    #[tokio::test]
    async fn project_intake_retry_requires_operator_and_current_receipt() {
        let dir=tempfile::tempdir().unwrap();
        let state=AppState{store:std::sync::Arc::new(crate::db::Store::open(&dir.path().join("db")).unwrap()),started:std::time::Instant::now(),build_hash:"test".into(),auth_token:None,reconciled:std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true))};
        state.store.write(|c| {
            c.execute("INSERT INTO cmd_history(id,text,type,session,ts,capture_pending,project_group,intake_attempts,intake_result) VALUES(42,'request','user','project:sample',1,1,'sample',2,?)",[json!({"state":"pending","error":"provider failed"}).to_string()])?;
            Ok(crate::db::WriteOutcome{applied:true,events:vec![]})
        }).unwrap();
        let app=routes().with_state(state);
        for (worker,project,key,expected) in [(true,"sample","click",StatusCode::FORBIDDEN),(false,"other","click",StatusCode::CONFLICT),(false,"sample","click",StatusCode::OK),(false,"sample","click",StatusCode::OK),(false,"sample","stale-click",StatusCode::CONFLICT)] {
            let mut request=axum::http::Request::builder().method("POST").uri(format!("/{project}/commands/42/retry")).header("content-type","application/json");
            if worker {request=request.header("x-amux-worker","executor");}
            let response=app.clone().oneshot(request.body(Body::from(json!({"idempotency_key":key,"expect_attempts":2,"expect_revision":0}).to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(),expected,"worker={worker}, project={project}, key={key}");
        }
    }
    #[tokio::test]
    async fn project_coordinator_profiles_round_trip_and_refuse_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            store: std::sync::Arc::new(crate::db::Store::open(&dir.path().join("db")).unwrap()),
            started: std::time::Instant::now(), build_hash: "test".into(), auth_token: None,
            reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let app = routes().with_state(state);
        for (provider, model, expected) in [("codex", "gpt-6-astra", StatusCode::OK), ("claude", "haiku", StatusCode::OK), ("gemini", "gemini-pro", StatusCode::BAD_REQUEST)] {
            let body = json!({"expect_rev":0,"policy":{"repository":"/repo","coordinator":{"provider":provider,"model":model},"executor":{"provider":"codex","model":"executor-custom"},"verify_command":"./verify.sh"}});
            let response = app.clone().oneshot(axum::http::Request::builder().method("PUT").uri(format!("/{provider}")).header("content-type","application/json").body(Body::from(body.to_string())).unwrap()).await.unwrap();
            assert_eq!(response.status(), expected, "coordinator {provider}");
            if expected == StatusCode::OK {
                let response = app.clone().oneshot(axum::http::Request::builder().uri(format!("/{provider}")).body(Body::empty()).unwrap()).await.unwrap();
                let data: serde_json::Value = serde_json::from_slice(&to_bytes(response.into_body(),1024*1024).await.unwrap()).unwrap();
                assert_eq!(data["project"]["policy"]["coordinator"], body["policy"]["coordinator"]);
                assert_eq!(data["project"]["policy"]["executor"], body["policy"]["executor"]);
            }
        }
    }

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
