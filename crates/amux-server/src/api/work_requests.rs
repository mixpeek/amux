//! Native board source adapter. WorkDesk owns source/artifact facts; amux owns
//! task identity and transport. No request here accepts a URL or worker name.
use super::AppState;
use crate::db::{board_store as bs, PendingEvent, WriteOutcome};
use amux_core::revision::{EntityType, MutationKind};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::Duration,
};

static SYNC_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static LAST_SYNC: OnceLock<Mutex<Value>> = OnceLock::new();
type ApiResult<T> = Result<T, (StatusCode, Json<Value>)>;
fn failure(status: StatusCode, error: impl ToString) -> (StatusCode, Json<Value>) {
    tracing::warn!(verdict="work_request_refused", error=%error.to_string());
    (status, Json(json!({"error":error.to_string()})))
}
fn db_error(error: impl ToString) -> (StatusCode, Json<Value>) {
    failure(StatusCode::CONFLICT, error)
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

struct Connector {
    url: reqwest::Url,
    token: String,
    client: reqwest::Client,
}
impl Connector {
    fn new() -> ApiResult<Self> {
        let raw = std::env::var("AMUX_WORKDESK_URL").map_err(|_| {
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "WorkDesk connector not configured",
            )
        })?;
        let url = validate_url(&raw).map_err(|e| failure(StatusCode::SERVICE_UNAVAILABLE, e))?;
        let file = std::env::var("AMUX_WORKDESK_TOKEN_FILE").map_err(|_| {
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "connector token file missing",
            )
        })?;
        let token = std::fs::read_to_string(file)
            .map_err(|_| {
                failure(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "connector token unreadable",
                )
            })?
            .trim()
            .to_string();
        if token.is_empty() {
            return Err(failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "connector token empty",
            ));
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(25))
            .build()
            .map_err(db_error)?;
        Ok(Self { url, token, client })
    }
    async fn request(&self, path: &str, body: Option<&Value>) -> ApiResult<reqwest::Response> {
        let url = self
            .url
            .join(&format!("/internal/amux/{path}"))
            .map_err(db_error)?;
        let req = if let Some(body) = body {
            self.client.put(url).json(body)
        } else {
            self.client.get(url)
        };
        let response = req
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| failure(StatusCode::BAD_GATEWAY, e))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(failure(
                if status.is_client_error() {
                    status
                } else {
                    StatusCode::BAD_GATEWAY
                },
                body,
            ));
        }
        Ok(response)
    }
    async fn json(&self, path: &str, body: Option<&Value>) -> ApiResult<Value> {
        self.request(path, body)
            .await?
            .json()
            .await
            .map_err(|e| failure(StatusCode::BAD_GATEWAY, e))
    }
}
fn validate_url(raw: &str) -> Result<reqwest::Url, &'static str> {
    let url = reqwest::Url::parse(raw).map_err(|_| "invalid connector URL")?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("connector must use an HTTP loopback IP origin without credentials or path");
    }
    Ok(url)
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/sync", post(sync))
        .route("/dispatch", get(dispatch_status).post(dispatch))
}
async fn status() -> Json<Value> {
    let config = Connector::new();
    let last = LAST_SYNC
        .get()
        .and_then(|s| s.lock().ok())
        .map(|v| v.clone())
        .unwrap_or(Value::Null);
    let error = config
        .as_ref()
        .err()
        .map(|(_, v)| v.0.clone())
        .or_else(|| last.get("error").cloned());
    Json(
        json!({"configured":config.is_ok(),"enabled":config.is_ok(),"measured":last["measured"].as_bool().unwrap_or(false),"n_considered":last.get("count").cloned(),"last_success_at":last.get("last_success_at").cloned(),"last_sync":last,"error":error}),
    )
}
fn record_sync_error(error: Value) {
    let mut last = LAST_SYNC
        .get_or_init(|| Mutex::new(Value::Null))
        .lock()
        .unwrap();
    let success = last.get("last_success_at").cloned();
    *last =
        json!({"at":now(),"error":error,"count":null,"measured":false,"last_success_at":success});
}
async fn sync(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    match sync_all(&state).await {
        Ok(report) => Ok(Json(report)),
        Err((code, Json(error))) => {
            record_sync_error(error.clone());
            Err((code, Json(error)))
        }
    }
}
pub(crate) fn last_outcome() -> Option<String> {
    if std::env::var_os("AMUX_WORKDESK_URL").is_none() {
        return Some("connector not configured; no source requests made".into());
    }
    let report = LAST_SYNC.get()?.lock().ok()?;
    if report.get("error").is_some_and(|error| !error.is_null()) {
        Some("source synchronization failed; see /api/work-requests/status".into())
    } else if report["measured"].as_bool() == Some(true) {
        Some(format!(
            "{} source requests synchronized",
            report["count"].as_u64().unwrap_or(0)
        ))
    } else {
        None
    }
}
pub fn spawn(state: AppState) -> crate::runtime_jobs::PeriodicTask {
    crate::runtime_jobs::spawn_periodic(
        crate::runtime_jobs::registry::ids::WORK_REQUESTS,
        60,
        move || {
            let state = state.clone();
            async move {
                if std::env::var_os("AMUX_WORKDESK_URL").is_some() {
                    if let Err((_, Json(error))) = sync_all(&state).await {
                        record_sync_error(error);
                    }
                }
            }
        },
    )
}
async fn snapshot_lock() -> ApiResult<tokio::sync::MutexGuard<'static, ()>> {
    tokio::time::timeout(Duration::from_secs(30), SYNC_LOCK.lock())
        .await
        .map_err(|_| {
            failure(
                StatusCode::SERVICE_UNAVAILABLE,
                "source reconciliation busy; pending operations remain durable",
            )
        })
}
async fn binding(state: &AppState, id: &str) -> ApiResult<(String, i64)> {
    let id = id.to_string();
    state
        .store
        .read_async(move |c| {
            Ok(c.query_row(
                "SELECT instance_id,candidate_id FROM _amux_source_bindings WHERE task_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
        .await
        .map_err(db_error)?
        .ok_or_else(|| failure(StatusCode::NOT_FOUND, "source binding not found"))
}
async fn verified_connector(instance: &str) -> ApiResult<Connector> {
    let connector = Connector::new()?;
    let manifest = connector.json("manifest", None).await?;
    if text(&manifest, "instance_id") != instance || manifest["schema_version"] != 1 {
        return Err(failure(
            StatusCode::CONFLICT,
            "connector instance/schema mismatch",
        ));
    }
    Ok(connector)
}
pub async fn detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _guard = snapshot_lock().await?;
    let (instance, candidate) = binding(&state, &id).await?;
    let connector = verified_connector(&instance).await?;
    let value = connector
        .json(&format!("requests/{candidate}"), None)
        .await?;
    import_request(&state, &instance, value.clone()).await?;
    Ok(Json(value))
}
fn domain_status(v: &Value) -> &'static str {
    let d = v.get("detail").unwrap_or(v);
    // Only a positive transport receipt closes the task; a legacy human status
    // or an artifact's existence is not evidence of submission.
    let delivered = d["delivery"].as_array().is_some_and(|items| {
        items.iter().any(|receipt| {
            text(receipt, "state") == "delivered"
                && receipt["artifact_id"] == d["artifact"]["id"]
                && receipt["evidence"].as_str().is_some_and(|v| {
                    !v.trim().is_empty() && !matches!(v.trim(), "[]" | "{}" | "null")
                })
        })
    });
    if delivered && text(&d["artifact"], "status") == "delivered" {
        return "done";
    }
    match text(v, "status") {
        "dismissed" => "discarded",
        "held" | "hold" => "blocked",
        "drafting" | "generating" | "running" => "doing",
        "draft_failed" => "blocked",
        "review" | "review_ready" | "draft_ready" | "approved" => "review",
        _ => "backlog",
    }
}
async fn import_request(state: &AppState, instance: &str, v: Value) -> ApiResult<()> {
    let candidate = v["candidate_id"]
        .as_i64()
        .ok_or_else(|| failure(StatusCode::BAD_GATEWAY, "candidate_id missing"))?;
    let title = text(&v, "title").to_string();
    let fingerprint = text(&v, "source_fingerprint").to_string();
    if candidate <= 0 || title.is_empty() || fingerprint.is_empty() {
        return Err(failure(
            StatusCode::BAD_GATEWAY,
            "incomplete source request",
        ));
    }
    let instance = instance.to_string();
    let snapshot = v.to_string();
    state.store.write_async(move|c|{
        let existing:Option<String>=c.query_row("SELECT task_id FROM _amux_source_bindings WHERE instance_id=?1 AND candidate_id=?2",params![instance,candidate],|r|r.get(0)).optional()?;
        if let Some(ref id)=existing {
            let (prior,verified):(String,Option<i64>)=c.query_row("SELECT b.snapshot,i.last_verified_at FROM _amux_source_bindings b JOIN issues i ON i.id=b.task_id WHERE b.task_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if prior==snapshot && verified.is_some_and(|at| now()-at<3600) {
                return Ok(WriteOutcome{applied:false,events:vec![]});
            }
        }
        let mut row=if let Some(id)=existing {bs::get_issue(c,&id)?.ok_or(rusqlite::Error::InvalidQuery)?}else{
            bs::create_issue(c,&bs::NewIssue{title:title.clone(),desc:"Source request managed through its native request actions.".into(),status:"backlog".into(),session:None,item_type:"task".into(),creator:"workdesk".into(),owner_type:"human".into(),due:None,due_time:None,reviewer:None,shepherd:None,gate:vec![],depends_on:vec![],tags:vec!["workdesk".into()],ask_type:None,next_action:None,acceptance_criteria:None,ask_question:None,ask_unblocks:None,ask_actor:None,source:Some("workdesk".into()),requested_by:None,callback_session:None,callback_prompt:None},now())?
        };
        c.execute("INSERT INTO _amux_source_write_permits VALUES(?1)",[&row.id])?;
        let issue=&v["detail"]["issue"];
        let due=text(issue,"due_at");
        row.due=due.get(..10).filter(|date|chrono::NaiveDate::parse_from_str(date,"%Y-%m-%d").is_ok()).map(str::to_string);
        row.desc=format!("Requester: {}\nSource: {}\n{}",text(issue,"requester_name"),text(issue,"evidence_permalink"),text(issue,"title"));
        row.title=title; row.source_ref=Some(format!("{instance}:{candidate}"));row.updated=now();
        row.last_verified_at=Some(now());
        // Persist refreshed facts and lifecycle under one writer transaction.
        bs::save_patched(c,&mut row)?;
        let target=domain_status(&v);
        row.evidence=if target=="done" {Some(v["detail"]["delivery"].to_string())} else {None};
        bs::save_patched(c,&mut row)?;
        let mut events=vec![];
        if row.status!=target {
            let opts=crate::db::advance::AdvanceOpts{gate_ack:true,skip_continuation:true,reason:Some("source facts reconciled; delivery completion requires receipt".into()),..Default::default()};
            match crate::db::advance::advance(c,&row.id,target,"workdesk-source",&opts)? {
                Ok(outcome)=>{row=outcome.row;events=outcome.events;}
                Err(reason)=>{tracing::warn!(task=%row.id,?reason,verdict="source_transition_refused");return Err(rusqlite::Error::InvalidQuery);}
            }
        }
        c.execute("DELETE FROM _amux_source_write_permits WHERE task_id=?1",[&row.id])?;
        c.execute("INSERT INTO _amux_source_bindings(task_id,instance_id,candidate_id,fingerprint,snapshot,synced_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(task_id) DO UPDATE SET fingerprint=excluded.fingerprint,snapshot=excluded.snapshot,synced_at=excluded.synced_at",params![row.id,instance,candidate,fingerprint,snapshot,now()])?;
        let artifact=&v["detail"]["artifact"];
        if let Some(aid)=artifact["id"].as_i64() {
            let reference=format!("/api/board/{}/source/artifacts/{aid}/download",row.id);
            let artifact_state=match text(artifact,"status") {"delivered"=>"submitted","stale"=>"invalid",_=>"created"};
            if let Some(existing)=crate::db::artifact_store::get_for_task_ref(c,&row.id,&reference)? {
                crate::db::artifact_store::update_state(c,&existing.id,artifact_state,now())?;
            } else {
                crate::db::artifact_store::insert(c,&crate::db::artifact_store::ArtifactRow {
                    id:format!("ART-{}",ulid::Ulid::new().to_string().to_lowercase()),task_id:row.id.clone(),kind:"doc".into(),ref_value:reference,state:artifact_state.into(),description:Some(format!("WorkDesk version {} SHA256 {}",artifact["version"],text(artifact,"content_hash"))),created_at:now(),updated_at:now()
                })?;
            }
        }
        events.push(PendingEvent{entity_type:EntityType::Task,entity_id:row.id.clone(),mutation:MutationKind::Updated,payload:Some(row.snapshot())});
        Ok(WriteOutcome{applied:true,events})
    }).await.map_err(db_error)?;
    Ok(())
}
async fn sync_all(state: &AppState) -> ApiResult<Value> {
    let _guard = snapshot_lock().await?;
    tokio::time::timeout(Duration::from_secs(50), sync_locked(state))
        .await
        .map_err(|_| {
            failure(
                StatusCode::GATEWAY_TIMEOUT,
                "source scan time budget reached; pending operations remain durable",
            )
        })?
}
async fn sync_locked(state: &AppState) -> ApiResult<Value> {
    let connector = Connector::new()?;
    let manifest = connector.json("manifest", None).await?;
    let instance = text(&manifest, "instance_id");
    if instance.is_empty() || manifest["schema_version"] != 1 {
        return Err(failure(
            StatusCode::BAD_GATEWAY,
            "invalid connector manifest",
        ));
    }
    reconcile_operations(state, &connector, instance).await?;
    let mut cursor = 0_i64;
    let mut count = 0;
    let mut complete = false;
    for _ in 0..100 {
        let batch = connector
            .json(&format!("requests?after_id={cursor}&limit=100"), None)
            .await?;
        let requests = batch["requests"]
            .as_array()
            .ok_or_else(|| failure(StatusCode::BAD_GATEWAY, "invalid source batch"))?;
        for request in requests {
            let candidate = request["candidate_id"]
                .as_i64()
                .ok_or_else(|| failure(StatusCode::BAD_GATEWAY, "candidate missing"))?;
            let detail = connector
                .json(&format!("requests/{candidate}"), None)
                .await?;
            import_request(state, instance, detail).await?;
            count += 1;
        }
        match batch["next_cursor"].as_i64() {
            Some(next) if next > cursor => cursor = next,
            _ => {
                complete = true;
                break;
            }
        }
    }
    if !complete {
        return Err(failure(
            StatusCode::BAD_GATEWAY,
            format!("source scan truncated after {count} records; pagination limit reached"),
        ));
    }
    let report = json!({"at":now(),"last_success_at":now(),"measured":true,"count":count,"instance_id":instance});
    *LAST_SYNC
        .get_or_init(|| Mutex::new(Value::Null))
        .lock()
        .unwrap() = report.clone();
    Ok(report)
}
async fn reconcile_operations(
    state: &AppState,
    connector: &Connector,
    instance: &str,
) -> ApiResult<()> {
    let pending: Vec<(String,String)> = state.store.read_async(|c| {
        let mut statement=c.prepare("SELECT id,request FROM _amux_source_operations WHERE receipt IS NULL OR json_extract(receipt,'$.state')='pending' ORDER BY created_at LIMIT 3")?;
        let rows=statement.query_map([],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<Vec<_>,_>>()?;
        Ok(rows)
    }).await.map_err(db_error)?;
    for (id, payload) in pending {
        let body: Value = serde_json::from_str(&payload).map_err(db_error)?;
        if text(&body, "connector_instance") != instance {
            continue;
        }
        let receipt = match connector
            .json(&format!("operations/{id}"), Some(&body))
            .await
        {
            Ok(receipt) => receipt,
            Err((code, Json(error))) if code.is_client_error() => {
                json!({"operation_id":id,"state":"failed","error":error})
            }
            Err(_) => continue,
        };
        state
            .store
            .write_async(move |c| {
                c.execute(
                    "UPDATE _amux_source_operations SET receipt=?2 WHERE id=?1",
                    params![id, receipt.to_string()],
                )?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .await
            .map_err(db_error)?;
    }
    Ok(())
}

fn valid_operation_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn owner_action(headers: &HeaderMap, uri: &Uri) -> ApiResult<()> {
    if ["x-amux-session", "x-amux-worker"]
        .iter()
        .any(|name| headers.get(*name).is_some_and(|v| !v.as_bytes().is_empty()))
    {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "source actions require owner UI",
        ));
    }
    // Same-origin browser intent is the existing owner boundary, not proof of a
    // human identity. A worker bearer alone cannot invoke approval or delivery.
    let origin = headers
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| reqwest::Url::parse(v).ok());
    let host = super::static_files::request_authority(headers, uri);
    if !origin.is_some_and(|o| {
        if !matches!(o.scheme(), "http" | "https")
            || uri.scheme_str().is_some_and(|scheme| scheme != o.scheme())
            || uri.authority().is_some_and(|authority| {
                host.as_deref()
                    .is_none_or(|host| !host.eq_ignore_ascii_case(authority.as_str()))
            })
        {
            return false;
        }
        let authority = match o.port() {
            Some(p) => format!("{}:{p}", o.host_str().unwrap_or("")),
            None => o.host_str().unwrap_or("").into(),
        };
        Some(authority.as_str()) == host.as_deref()
    }) {
        return Err(failure(
            StatusCode::FORBIDDEN,
            "source actions require same-origin owner UI",
        ));
    }
    Ok(())
}
pub async fn action(
    State(state): State<AppState>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> ApiResult<Json<Value>> {
    owner_action(&headers, &uri)?;
    let kind = text(&body, "kind");
    if matches!(kind, "approve" | "deliver")
        && headers
            .get("x-amux-approver")
            .and_then(|v| v.to_str().ok())
            .is_none_or(|v| v.trim().is_empty())
    {
        return Err(failure(StatusCode::FORBIDDEN, "explicit approver required"));
    }

    if !matches!(
        kind,
        "generate" | "revise" | "approve" | "hold" | "deliver" | "restore"
    ) {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "unsupported source action",
        ));
    }
    let op = text(&body, "operation_id").to_string();
    if !valid_operation_id(&op) || text(&body, "expected_source_fingerprint").is_empty() {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "operation_id UUID and source fingerprint required",
        ));
    }
    let (instance, candidate) = binding(&state, &id).await?;
    body["candidate_id"] = json!(candidate);
    body["amux_task_id"] = json!(id);
    body["connector_instance"] = json!(instance);
    body["actor_claim"] = json!(headers
        .get("x-amux-approver")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("owner UI (identity not independently verified)")
        .chars()
        .take(200)
        .collect::<String>());
    let connector = verified_connector(&instance).await?;
    let (op_w, id_w, payload) = (op.clone(), id.clone(), body.to_string());
    state.store.write_async(move|c|{
        let prior:Option<(String,String)>=c.query_row("SELECT task_id,request FROM _amux_source_operations WHERE id=?1",[&op_w],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if prior.is_some_and(|(task,old)|task!=id_w||old!=payload){return Err(rusqlite::Error::InvalidQuery);}
        let n=c.execute("INSERT OR IGNORE INTO _amux_source_operations(id,task_id,request,created_at) VALUES(?1,?2,?3,?4)",params![op_w,id_w,payload,now()])?;
        Ok(WriteOutcome{applied:n>0,events:vec![]})
    }).await.map_err(db_error)?;
    let _guard = snapshot_lock().await?;
    let receipt = connector
        .json(&format!("operations/{op}"), Some(&body))
        .await?;
    let receipt_w = receipt.to_string();
    state
        .store
        .write_async(move |c| {
            c.execute(
                "UPDATE _amux_source_operations SET receipt=?2 WHERE id=?1",
                params![op, receipt_w],
            )?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await
        .map_err(db_error)?;
    let mut detail = connector
        .json(&format!("requests/{candidate}"), None)
        .await?;
    import_request(&state, &instance, detail.clone()).await?;
    detail["operation"] = receipt;
    Ok(Json(detail))
}
pub async fn download(
    State(state): State<AppState>,
    Path((id, aid)): Path<(String, i64)>,
) -> ApiResult<Response> {
    let (instance, candidate) = binding(&state, &id).await?;
    let connector = verified_connector(&instance).await?;
    let detail = connector
        .json(&format!("requests/{candidate}"), None)
        .await?;
    let d = detail.get("detail").unwrap_or(&detail);
    if d["artifact"]["id"].as_i64() != Some(aid) {
        return Err(failure(
            StatusCode::NOT_FOUND,
            "artifact is not this request's current version",
        ));
    }
    let response = connector.request(&format!("artifacts/{aid}"), None).await?;
    if response
        .content_length()
        .is_some_and(|n| n > 25 * 1024 * 1024)
    {
        return Err(failure(StatusCode::PAYLOAD_TOO_LARGE, "artifact too large"));
    }
    let mut headers = HeaderMap::new();
    for name in ["content-type", "content-disposition"] {
        if let Some(value) = response.headers().get(name) {
            headers.insert(axum::http::HeaderName::from_static(name), value.clone());
        }
    }
    let bytes = response.bytes().await.map_err(db_error)?;
    if bytes.len() > 25 * 1024 * 1024 {
        return Err(failure(StatusCode::PAYLOAD_TOO_LARGE, "artifact too large"));
    }
    Ok((headers, bytes).into_response())
}
#[derive(Deserialize)]
struct Dispatch {
    task_id: String,
    key: String,
    text: String,
}
fn message_id(task: &str, key: &str) -> String {
    format!(
        "work-request-{:x}",
        Sha256::digest(format!("{task}\0{key}").as_bytes())
    )
}
async fn dispatch_status(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let task = q.get("task_id").cloned().unwrap_or_default();
    let key = q.get("key").cloned().unwrap_or_default();
    binding(&state, &task).await?;
    let msg = message_id(&task, &key);
    let lookup = msg.clone();
    let accepted=state.store.read_async(move|c|{Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM steering_queue WHERE id=?1 UNION ALL SELECT 1 FROM steering_history WHERE id=?1)",[lookup],|r|r.get::<_,bool>(0))?)}).await.map_err(db_error)?;
    Ok(Json(if accepted {
        json!({"state":"accepted","message_id":msg})
    } else {
        json!({"state":"not_found"})
    }))
}
async fn dispatch(
    State(state): State<AppState>,
    Json(body): Json<Dispatch>,
) -> ApiResult<Json<Value>> {
    Connector::new()?;
    let worker =
        std::env::var("AMUX_WORKDESK_WORKER").unwrap_or_else(|_| "workdesk-executor".into());
    dispatch_to(&state, body, &worker).await
}
async fn dispatch_to(state: &AppState, body: Dispatch, worker: &str) -> ApiResult<Json<Value>> {
    binding(state, &body.task_id).await?;
    if body.key.is_empty()
        || body.key.len() > 200
        || body.text.is_empty()
        || body.text.len() > 100_000
    {
        return Err(failure(
            StatusCode::BAD_REQUEST,
            "invalid dispatch key/text",
        ));
    }
    let msg = message_id(&body.task_id, &body.key);
    let (task, key, prompt, worker_w, msg_w) = (
        body.task_id.clone(),
        body.key.clone(),
        body.text.clone(),
        worker.to_string(),
        msg.clone(),
    );
    state.store.write_async(move|c|{
        super::session_verbs::ensure_fleet_tables(c)?;
        let prior:Option<(String,String,String)>=c.query_row("SELECT task_id,worker,text FROM _amux_source_dispatch WHERE key=?1",[&key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((prior_task,prior_worker,prior_text))=prior {
            if prior_task!=task || prior_worker!=worker_w || prior_text!=prompt {
                return Err(rusqlite::Error::InvalidParameterName("dispatch key is already bound to a different task, worker or prompt".into()));
            }
            return Ok(WriteOutcome{applied:false,events:vec![]});
        }
        c.execute("INSERT INTO _amux_source_dispatch(task_id,key,message_id,worker,text) VALUES(?1,?2,?3,?4,?5)",params![task,key,msg_w,worker_w,prompt])?;
        Ok(WriteOutcome{applied:true,events:vec![]})
    }).await.map_err(db_error)?;
    // Stable transport ID and nonempty guard avoid semantic intake. Binding is
    // durable before enqueue; a crash is recovered by replaying this exact key.
    super::session_verbs::steer_enqueue_precond_with_id(
        &state.store,
        worker,
        &body.text,
        "work-request",
        "workdesk",
        Some((&body.task_id, -1)),
        Some(&msg),
    )
    .await
    .map_err(db_error)?;
    Ok(Json(json!({"state":"accepted","message_id":msg})))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_state() -> (AppState, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let store =
            std::sync::Arc::new(crate::db::Store::open(&temp.path().join("source.db")).unwrap());
        (
            AppState {
                store,
                started: std::time::Instant::now(),
                build_hash: "test".into(),
                auth_token: None,
                reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            },
            temp,
        )
    }
    #[tokio::test]
    async fn source_import_is_one_native_card_and_generic_routes_cannot_take_over() {
        use tower::ServiceExt;
        let (state, _dir) = test_state();
        let request = json!({"candidate_id":7,"title":"Draft source request","status":"review_ready","source_fingerprint":"v1","detail":{"artifact":{"id":9,"status":"ready_for_review"}}});
        import_request(&state, "instance", request.clone())
            .await
            .unwrap();
        import_request(&state, "instance", request).await.unwrap();
        let (id, count): (String, i64) = state
            .store
            .read_async(|c| {
                Ok((
                    c.query_row("SELECT task_id FROM _amux_source_bindings", [], |r| {
                        r.get(0)
                    })?,
                    c.query_row(
                        "SELECT COUNT(*) FROM issues WHERE source='workdesk'",
                        [],
                        |r| r.get(0),
                    )?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(count, 1);
        let row = bs::get_issue(&state.store.read().unwrap(), &id)
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "review");
        assert_eq!(row.session, None);
        assert_eq!(row.owner_type, "human");
        let app = super::super::router(state.clone());
        for payload in [
            json!({"status":"done","force":true,"evidence":"pretend"}),
            json!({"session":"worker","owner_type":"agent"}),
            json!({"source":"agent"}),
        ] {
            let request = axum::http::Request::builder()
                .method("PATCH")
                .uri(format!("/api/board/{id}"))
                .header("content-type", "application/json")
                .body(axum::body::Body::from(payload.to_string()))
                .unwrap();
            assert_eq!(
                app.clone().oneshot(request).await.unwrap().status(),
                StatusCode::CONFLICT
            );
        }
        let task = id.clone();
        assert!(state
            .store
            .write_async(move |c| {
                c.execute("UPDATE issues SET status='done' WHERE id=?1", [task])?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .await
            .is_err());
        assert!(
            !crate::runtime_jobs::board_drive::claim_card_from(&state, "worker", &id, "review")
                .await
        );
        let task = id.clone();
        assert!(state
            .store
            .write_async(move |c| {
                c.execute("DELETE FROM issues WHERE id=?1", [task])?;
                Ok(WriteOutcome {
                    applied: true,
                    events: vec![],
                })
            })
            .await
            .is_err());
    }
    #[tokio::test]
    async fn unchanged_source_refreshes_missing_or_aged_verification_without_new_card() {
        let (state, _dir) = test_state();
        let request = json!({"candidate_id":4,"title":"Source provenance","status":"awaiting_draft","source_fingerprint":"v1","detail":{}});
        import_request(&state, "instance", request.clone())
            .await
            .unwrap();
        for prior in [None, Some(now() - 3601)] {
            state
                .store
                .write_async(move |c| {
                    c.execute(
                        "UPDATE issues SET last_verified_at=?1 WHERE source='workdesk'",
                        [prior],
                    )?;
                    Ok(WriteOutcome {
                        applied: true,
                        events: vec![],
                    })
                })
                .await
                .unwrap();
            let before = now();
            import_request(&state, "instance", request.clone())
                .await
                .unwrap();
            let c = state.store.read().unwrap();
            let verified: i64 = c
                .query_row(
                    "SELECT last_verified_at FROM issues WHERE source='workdesk'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert!(verified >= before);
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM issues", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
        }
    }
    #[tokio::test]
    async fn stale_source_edit_reopens_review_without_fabricating_delivery() {
        let (state, _dir) = test_state();
        let completed = json!({"candidate_id":1,"title":"Source","status":"delivered","source_fingerprint":"v1","detail":{"artifact":{"id":1,"status":"delivered"},"delivery":[{"artifact_id":1,"state":"delivered","evidence":"slack receipt"}]}});
        import_request(&state, "instance", completed).await.unwrap();
        let id: String = state
            .store
            .read_async(|c| {
                Ok(
                    c.query_row("SELECT task_id FROM _amux_source_bindings", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert_eq!(
            bs::get_issue(&state.store.read().unwrap(), &id)
                .unwrap()
                .unwrap()
                .status,
            "done"
        );
        import_request(&state,"instance",json!({"candidate_id":1,"title":"Edited source","status":"awaiting_draft","source_fingerprint":"v2","detail":{"artifact":{"id":1,"status":"stale"}}})).await.unwrap();
        assert_eq!(
            bs::get_issue(&state.store.read().unwrap(), &id)
                .unwrap()
                .unwrap()
                .status,
            "backlog"
        );
    }
    #[tokio::test]
    async fn bound_dispatch_replay_keeps_one_queue_and_one_card_without_semantic_capture() {
        let (state, _dir) = test_state();
        let _home = crate::api::settings::test_env::set_home(_dir.path());
        import_request(&state,"instance",json!({"candidate_id":1,"title":"Source","status":"drafting","source_fingerprint":"v1","detail":{}})).await.unwrap();
        let id: String = state
            .store
            .read_async(|c| {
                Ok(
                    c.query_row("SELECT task_id FROM _amux_source_bindings", [], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .unwrap();
        state.store.write_async(|c|{c.execute("INSERT INTO _amux_workers(id,display_name,created_at,updated_at) VALUES('wrk_source_fixture','source-test-fixture-worker',0,0)",[])?;Ok(WriteOutcome{applied:true,events:vec![]})}).await.unwrap();
        let packet = || Dispatch {
            task_id: id.clone(),
            key: "exec-1".into(),
            text: "Generate a draft for the bound request".into(),
        };
        let first = dispatch_to(&state, packet(), "source-test-fixture-worker")
            .await
            .unwrap()
            .0;
        assert_eq!(
            dispatch_to(&state, packet(), "source-test-fixture-worker")
                .await
                .unwrap()
                .0,
            first
        );
        let msg = first["message_id"].as_str().unwrap().to_string();
        {
            let c = state.store.read().unwrap();
            assert_eq!(
                c.query_row("SELECT COUNT(*) FROM steering_queue", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                c.query_row("SELECT precond_card FROM steering_queue", [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                id
            );
        }
        let mut changed = packet();
        changed.text = "Different execution".into();
        assert_eq!(
            dispatch_to(&state, changed, "source-test-fixture-worker")
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        import_request(&state,"instance",json!({"candidate_id":2,"title":"Other source","status":"drafting","source_fingerprint":"v1","detail":{}})).await.unwrap();
        let other: String = state
            .store
            .read_async(|c| {
                Ok(c.query_row(
                    "SELECT task_id FROM _amux_source_bindings WHERE candidate_id=2",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        let mut crossed = packet();
        crossed.task_id = other;
        assert_eq!(
            dispatch_to(&state, crossed, "source-test-fixture-worker")
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        let mut wrong = packet();
        wrong.task_id = "NO-SUCH-TASK".into();
        assert_eq!(
            dispatch_to(&state, wrong, "source-test-fixture-worker")
                .await
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
        let mut missing = packet();
        missing.key = "missing-worker".into();
        assert!(dispatch_to(&state, missing, "no-such-source-test-worker")
            .await
            .is_err());
        for _ in 0..2 {
            super::super::session_verbs::capture_delivered_steering(
                &state,
                "source-test-fixture-worker".into(),
                packet().text,
                "work-request".into(),
                "workdesk".into(),
                msg.clone(),
            )
            .await;
        }
        let c = state.store.read().unwrap();
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM issues", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2,
            "only the two imported source tasks exist; no WE/capture sibling"
        );
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM cmd_history", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            c.query_row("SELECT card_id FROM cmd_history", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            id
        );
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM steering_queue", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1,
            "missing worker enqueues nothing"
        );
    }
    #[test]
    fn connector_origin_cannot_escape_loopback() {
        for url in [
            "https://127.0.0.1:9",
            "http://localhost:9",
            "http://example.com",
            "http://127.0.0.1/a",
            "http://user@127.0.0.1",
            "http://127.0.0.1?x=1",
        ] {
            assert!(validate_url(url).is_err(), "{url}");
        }
        assert!(validate_url("http://127.0.0.1:8091").is_ok());
    }
    #[test]
    fn source_done_requires_actual_delivery_receipt() {
        for evidence in [
            json!([]),
            json!({}),
            json!(null),
            json!(""),
            json!(" "),
            json!("[]"),
            json!("{}"),
            json!("null"),
        ] {
            assert_ne!(
                domain_status(
                    &json!({"detail":{"artifact":{"id":1,"status":"delivered"},"delivery":[{"artifact_id":1,"state":"delivered","evidence":evidence}]}})
                ),
                "done"
            );
        }

        assert_eq!(
            domain_status(&json!({"status":"done","detail":{"artifact":{"id":1}}})),
            "backlog"
        );
        assert_eq!(
            domain_status(&json!({"status":"done","detail":{"delivery":{"status":"sent"}}})),
            "backlog"
        );
        assert_eq!(
            domain_status(
                &json!({"detail":{"artifact":{"id":1,"status":"delivered"},"delivery":[{"artifact_id":1,"state":"delivered","evidence":"receipt"}]}})
            ),
            "done"
        );
    }
    #[test]
    fn dispatch_identity_is_stable_and_unambiguous() {
        assert_eq!(message_id("A", "b"), message_id("A", "b"));
        assert_ne!(message_id("Ab", "c"), message_id("A", "bc"));
    }
    #[tokio::test]
    async fn owner_action_accepts_h2_authority_and_keeps_origin_refusals() {
        use tower::ServiceExt;
        let (state, _dir) = test_state();
        let app = super::super::router(state);
        let cases = [
            (
                "http://localhost:8824/api/board/SOURCE-1/source/actions",
                Some("localhost:8824"),
                Some("http://localhost:8824"),
                None,
                axum::http::Version::HTTP_11,
                StatusCode::BAD_REQUEST,
            ),
            (
                "https://mini.example:8844/api/board/SOURCE-1/source/actions",
                None,
                Some("https://mini.example:8844"),
                None,
                axum::http::Version::HTTP_2,
                StatusCode::BAD_REQUEST,
            ),
            (
                "https://mini.example:8844/api/board/SOURCE-1/source/actions",
                None,
                Some("https://other.example:8844"),
                None,
                axum::http::Version::HTTP_2,
                StatusCode::FORBIDDEN,
            ),
            (
                "https://mini.example:8844/api/board/SOURCE-1/source/actions",
                None,
                Some("http://mini.example:8844"),
                None,
                axum::http::Version::HTTP_2,
                StatusCode::FORBIDDEN,
            ),
            (
                "https://mini.example:8844/api/board/SOURCE-1/source/actions",
                None,
                None,
                None,
                axum::http::Version::HTTP_2,
                StatusCode::FORBIDDEN,
            ),
            (
                "https://mini.example:8844/api/board/SOURCE-1/source/actions",
                None,
                Some("https://mini.example:8844"),
                Some("x-amux-worker"),
                axum::http::Version::HTTP_2,
                StatusCode::FORBIDDEN,
            ),
            (
                "https://mini.example:8844/api/board/SOURCE-1/source/actions",
                None,
                Some("https://mini.example:8844"),
                Some("x-amux-session"),
                axum::http::Version::HTTP_2,
                StatusCode::FORBIDDEN,
            ),
            (
                "https://mini.example:8844/api/board/SOURCE-1/source/actions",
                Some("other.example:8844"),
                Some("https://other.example:8844"),
                None,
                axum::http::Version::HTTP_2,
                StatusCode::FORBIDDEN,
            ),
            (
                "/api/board/SOURCE-1/source/actions",
                None,
                Some("https://mini.example:8844"),
                None,
                axum::http::Version::HTTP_11,
                StatusCode::FORBIDDEN,
            ),
        ];
        for (uri, host, origin, worker, version, expected) in cases {
            let mut request = axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .version(version)
                .header("content-type", "application/json");
            if let Some(host) = host {
                request = request.header("host", host);
            }
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            if let Some(worker) = worker {
                request = request.header(worker, "declared-worker");
            }
            let response = app
                .clone()
                .oneshot(request.body(axum::body::Body::from("{}")).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                expected,
                "uri={uri}, host={host:?}, origin={origin:?}, worker={worker:?}"
            );
        }
    }
    #[test]
    fn declared_worker_origin_is_refused_by_owner_intent_guard() {
        let mut h = HeaderMap::new();
        h.insert("host", "localhost:8824".parse().unwrap());
        h.insert("origin", "http://localhost:8824".parse().unwrap());
        assert!(owner_action(&h, &Uri::from_static("/")).is_ok());
        h.insert("x-amux-session", "worker".parse().unwrap());
        assert!(owner_action(&h, &Uri::from_static("/")).is_err());
    }
}
