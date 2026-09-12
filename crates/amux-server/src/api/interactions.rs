//! Durable receipts over existing domain mutations, independent of presentation.
use super::AppState;
use crate::db::{interactions, SharedStore, WriteOutcome};
use axum::{
    body::{Body, HttpBody},
    extract::{Path, Query, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/interactions/recent", get(recent))
        .route("/api/interactions/{id}", get(one))
        .route("/api/interactions/{id}/effects", get(effects))
        .route("/api/interactions/{id}/why", get(why))
        .route("/api/debug/interactions", get(debug))
        .route("/api/state/summary", get(summary))
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
}

pub fn tracks(method: &str, path: &str) -> bool {
    matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
        && path.starts_with("/api/")
        && path != "/api/client-debug"
        && !path.starts_with("/api/speedtest")
}

fn primitive(path: &str) -> (&str, &str) {
    let mut parts = path.trim_start_matches('/').split('/').skip(1);
    let domain = parts.next().unwrap_or("environment");
    let kind = match domain {
        "sessions" if path.ends_with("/send") || path.ends_with("/steer") => "message",
        "sessions" | "workers" => "worker",
        "board" => "board",
        "schedules" => "scheduler",
        "files" | "file" | "fs" | "upload" => "filesystem",
        "groups" => "group",
        "memory" | "memories" => "memory",
        "messages" => "message",
        _ => "environment",
    };
    (kind, parts.next().unwrap_or(domain))
}

pub async fn middleware(
    State(store): State<SharedStore>,
    mut req: Request,
    next: Next,
) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    if !tracks(&method, &path) {
        return next.run(req).await;
    }
    let id =
        match req.headers().get("x-amux-interaction-id") {
            Some(value) => match value.to_str().ok().filter(|v| valid_id(v)) {
                Some(value) => value.to_string(),
                None => return (
                    StatusCode::BAD_REQUEST,
                    Json(
                        json!({"error":"Invalid interaction id", "measured":true,"n_considered":1}),
                    ),
                )
                    .into_response(),
            },
            None => format!("int_{}", ulid::Ulid::new()),
        };
    let actor = super::request_log::caller_from_headers(req.headers());
    let (target_kind, target_id) = primitive(&path);
    let kind = req
        .headers()
        .get("x-amux-command-kind")
        .and_then(|v| v.to_str().ok())
        .filter(|v| valid_id(v))
        .map(str::to_string)
        .unwrap_or_else(|| format!("{target_kind}.{}", method.to_lowercase()));
    let (record_id, record_path, record_method, record_actor, record_kind) = (
        id.clone(),
        path.clone(),
        method.clone(),
        actor.clone(),
        kind.clone(),
    );
    let (target_kind, target_id) = (target_kind.to_string(), target_id.to_string());
    let now = chrono::Utc::now().timestamp_millis();
    let accepted = store.write_async(move |conn| {
        let existing: Option<(String,String,String)> = conn.query_row(
            "SELECT method,path,actor FROM _amux_interactions WHERE id=?1", [&record_id],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some(existing) = existing {
            if existing != (record_method,record_path,record_actor) {
                return Err(rusqlite::Error::InvalidParameterName("interaction_identity_conflict".into()));
            }
            conn.execute("UPDATE _amux_interactions SET phase='sending', updated_at=?2, attempts=attempts+1 WHERE id=?1", rusqlite::params![record_id, now])?;
        } else {
            conn.execute("INSERT INTO _amux_interactions (id,command_kind,method,path,actor,target_kind,target_id,phase,created_at,updated_at)
                VALUES (?1,?2,?3,?4,?5,?6,?7,'sending',?8,?8)",
                rusqlite::params![record_id,record_kind,record_method,record_path,record_actor,target_kind,target_id,now])?;
        }
        Ok(WriteOutcome{applied:false,events:vec![]})
    }).await;
    if let Err(error) = accepted {
        tracing::warn!(verdict="interaction_accept_failed", interaction_id=%id, %error, "Command not executed: receipt could not be recorded");
        let status = if error.to_string().contains("interaction_identity_conflict") {
            StatusCode::CONFLICT
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        };
        return (status, Json(json!({"error":"Interaction could not be recorded", "interaction_id":id, "measured":true,"n_considered":1}))).into_response();
    }
    req.headers_mut().insert(
        "x-amux-interaction-id",
        HeaderValue::from_str(&id).expect("validated id"),
    );
    let response = interactions::CURRENT.scope(id.clone(), next.run(req)).await;
    let status = response.status().as_u16();
    // Only bounded, already-sized JSON. Never buffer an upload/download stream
    // or consume a response merely to discover it exceeds a telemetry limit.
    let capture = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"))
        && response
            .body()
            .size_hint()
            .exact()
            .is_some_and(|n| n <= 65536);
    let (mut response, body) = if capture {
        let (parts, body) = response.into_parts();
        match axum::body::to_bytes(body, 65536).await {
            Ok(bytes) => {
                let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
                (Response::from_parts(parts, Body::from(bytes)), value)
            }
            Err(error) => {
                tracing::warn!(verdict="interaction_ack_read_failed", interaction_id=%id, %error);
                (Response::from_parts(parts, Body::empty()), Value::Null)
            }
        }
    } else {
        (response, Value::Null)
    };
    let phase = if capture && body.is_null() {
        "unknown"
    } else {
        classify(status, &body)
    };
    // Persist only acknowledgement fields, never request payloads or returned
    // configuration/credential values.
    let mut ack = json!({"status":status});
    for key in [
        "applied",
        "rev",
        "global_rev",
        "version",
        "ignored_fields",
        "error",
        "remedy",
        "fix",
        "how_to_ack",
        "queued",
    ] {
        if let Some(value) = body.get(key) {
            ack[key] = value.clone();
        }
    }
    let record_id = id.clone();
    let result = store.write_async(move |conn| {
        conn.execute("UPDATE _amux_interactions SET phase=?2,status=?3,acknowledgement=?4,updated_at=?5 WHERE id=?1",
            rusqlite::params![record_id,phase,status,ack.to_string(),chrono::Utc::now().timestamp_millis()])?;
        Ok(WriteOutcome{applied:false,events:vec![]})
    }).await;
    if let Err(error) = result {
        tracing::warn!(verdict="interaction_completion_unrecorded", interaction_id=%id, %error);
    }
    if matches!(phase, "failed" | "refused" | "unknown") {
        tracing::warn!(verdict="interaction_outcome", interaction_id=%id, command_kind=%kind, %phase, status, "Command outcome needs inspection");
    }
    response.headers_mut().insert(
        "x-amux-interaction-id",
        HeaderValue::from_str(&id).expect("validated id"),
    );
    response.headers_mut().insert(
        "x-amux-command-kind",
        HeaderValue::from_str(&kind).expect("validated command kind"),
    );
    response
}

pub fn classify(status: u16, body: &Value) -> &'static str {
    if status >= 500 {
        return "failed";
    }
    if status >= 400 {
        return "refused";
    }
    if body.get("error").is_some_and(|v| !v.is_null())
        || body["ok"] == false
        || body["ignored_fields"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
    {
        return "refused";
    }
    match body["phase"].as_str() {
        Some("accepted") => return "accepted",
        Some("queued") => return "queued",
        Some("sending") => return "sending",
        Some("running") => return "running",
        Some("waiting") => return "waiting",
        Some("blocked") => return "blocked",
        Some("applied") => return "applied",
        Some("noop") => return "noop",
        Some("failed") => return "failed",
        Some("refused") => return "refused",
        Some("unknown") => return "unknown",
        Some("reconciled") => return "reconciled",
        _ => {}
    }
    if body["queued"] == true || body["submission"] == "deferred" {
        return "queued";
    }
    if status == 202 {
        return "running";
    }
    if body["applied"] == false || body["deduped"] == true {
        return "noop";
    }
    if body["applied"] == true || body["ok"] == true || body["submitted"] == true {
        return "applied";
    }
    "unknown"
}

fn read_receipt(conn: &rusqlite::Connection, id: &str) -> anyhow::Result<Option<Value>> {
    let value = conn.query_row("SELECT command_kind,method,path,actor,target_kind,target_id,phase,status,acknowledgement,created_at,updated_at,attempts,applied_writes,unjournaled_writes FROM _amux_interactions WHERE id=?1", [id], |r| {
        let phase: String = r.get(6)?;
        let updated: i64 = r.get(10)?;
        let stale = matches!(phase.as_str(),"sending"|"running") && chrono::Utc::now().timestamp_millis() - updated > 120000;
        let phase = if stale { "unknown" } else { &phase };
        let ack: String = r.get(8)?;
        Ok(json!({"id":id,"command":{"id":id,"kind":r.get::<_,String>(0)?,"target":{"primitive":r.get::<_,String>(4)?,"id":r.get::<_,String>(5)?}},
            "origin":{"actor":if r.get::<_,String>(3)?.is_empty() {"human"} else {"agent"},"session":r.get::<_,String>(3)?,"surface":"api"},"request":{"method":r.get::<_,String>(1)?,"path":r.get::<_,String>(2)?},
            "phase":phase,"acknowledgement":serde_json::from_str::<Value>(&ack).unwrap_or(json!({})),
            "feedback":{"required":true,"persistence":"durable","severity":if matches!(phase,"failed"|"refused") {"error"} else if matches!(phase,"unknown"|"blocked") {"warning"} else {"info"}},
            "measured":!stale,"n_considered":1,"why_unmeasured":if stale {Some("No completion observed; request may still be running or process restarted")} else {None},
            "created_at":r.get::<_,i64>(9)?,"updated_at":updated,"attempts":r.get::<_,i64>(11)?,"applied_writes":r.get::<_,i64>(12)?,"unjournaled_writes":r.get::<_,i64>(13)?}))
    }).optional()?;
    Ok(value)
}

type ReadError = (StatusCode, Json<Value>);

fn unavailable() -> ReadError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(
            json!({"measured":false,"n_considered":0,"why_unmeasured":"Interaction store unavailable"}),
        ),
    )
}

async fn read<T: Send + 'static>(
    store: SharedStore,
    f: impl FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
) -> Result<Json<T>, ReadError> {
    tokio::task::spawn_blocking(move || {
        let conn = store.read()?;
        f(&conn)
    })
    .await
    .map_err(|_| unavailable())?
    .map(Json)
    .map_err(|error| {
        tracing::warn!(verdict="interaction_read_failed", %error);
        unavailable()
    })
}

async fn one(State(state): State<AppState>, Path(id): Path<String>) -> Result<Response, ReadError> {
    let Json(value) = read(state.store, move |conn| read_receipt(conn, &id)).await?;
    Ok(match value {
        Some(v) => Json(v).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"measured":true,"n_considered":0,"error":"Interaction not found"})),
        )
            .into_response(),
    })
}

async fn effects(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ReadError> {
    read(state.store, move |conn| {
        let mut rows = interactions::effects(conn,&id)?;
        let more = rows.len() > 1000;
        rows.truncate(1000);
        Ok(json!({"measured":true,"n_considered":rows.len(),"interaction_id":id,"effects":rows,"more":more}))
    }).await
}

async fn why(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ReadError> {
    read(state.store, move |conn| {
        let receipt = read_receipt(conn,&id)?;
        let effects = interactions::effects(conn,&id)?;
        let mut stmt = conn.prepare("SELECT id,status,error_body FROM _amux_request_log WHERE req_meta IS NOT NULL AND json_extract(req_meta,'$.interaction_id')=?1 ORDER BY id DESC LIMIT 20")?;
        let logs = stmt.query_map([&id], |r| Ok(json!({"id":r.get::<_,i64>(0)?,"status":r.get::<_,i64>(1)?,"error":r.get::<_,Option<String>>(2)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"measured":true,"n_considered":usize::from(receipt.is_some()),"receipt":receipt,"effects":effects,"request_log":logs,
            "effect_coverage":"Journaled writes in the request scope; spawned work must explicitly propagate the interaction scope"}))
    }).await
}

#[derive(Deserialize, Default)]
struct Params {
    scope: Option<String>,
    since_rev: Option<u64>,
    since_h: Option<f64>,
}

fn validate_params(params: &Params) -> Result<(), ReadError> {
    if params
        .scope
        .as_deref()
        .is_some_and(|s| !s.starts_with("worker:") || s.len() <= 7)
    {
        tracing::warn!(
            verdict = "interaction_scope_refused",
            "Unsupported interaction scope"
        );
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"measured":false,"n_considered":0,
            "error":"Unsupported interaction scope; expected worker:<name>",
            "why_unmeasured":"Scope validation failed before querying receipts"})),
        ));
    }
    Ok(())
}

fn recent_rows(conn: &rusqlite::Connection, params: &Params) -> anyhow::Result<Vec<Value>> {
    let cutoff = chrono::Utc::now().timestamp_millis()
        - (params.since_h.unwrap_or(24.0).clamp(0.0, 336.0) * 3600000.0) as i64;
    let actor = params
        .scope
        .as_deref()
        .and_then(|s| s.strip_prefix("worker:"))
        .unwrap_or("");
    let mut stmt = conn.prepare("SELECT i.id FROM _amux_interactions i WHERE i.updated_at>=?1 AND (?2='' OR i.actor=?2 OR (i.target_kind IN ('worker','message') AND i.target_id=?2))
        AND (?3=0 OR i.phase NOT IN ('applied','noop','reconciled') OR EXISTS(SELECT 1 FROM _amux_interaction_effects e WHERE e.interaction_id=i.id AND e.rev>?3)) ORDER BY i.updated_at DESC LIMIT 201")?;
    let ids = stmt
        .query_map(
            rusqlite::params![cutoff, actor, params.since_rev.unwrap_or(0)],
            |r| r.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ids.iter()
        .map(|id| read_receipt(conn, id)?.ok_or_else(|| anyhow::anyhow!("Receipt disappeared")))
        .collect()
}

async fn recent(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Result<Json<Value>, ReadError> {
    validate_params(&params)?;
    read(state.store, move |conn| {
        let mut rows = recent_rows(conn, &params)?;
        let more = rows.len() > 200;
        rows.truncate(200);
        Ok(json!({"measured":true,"n_considered":rows.len(),"interactions":rows,"more":more}))
    })
    .await
}

async fn summary(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Result<Json<Value>, ReadError> {
    validate_params(&params)?;
    read(state.store, move |conn| {
        let mut rows = recent_rows(conn,&params)?;
        let more = rows.len()>200; rows.truncate(200);
        let rev: u64 = conn.query_row("SELECT rev FROM _amux_rev WHERE id=1",[],|r|r.get(0))?;
        let mut effects = vec![];
        for row in &rows { effects.extend(interactions::effects(conn,row["id"].as_str().unwrap_or_default())?); }
        let next: Vec<_> = rows.iter().filter(|r| matches!(r["phase"].as_str(),Some("refused"|"failed"|"blocked"|"unknown"))).map(|r|json!({"interaction_id":r["id"],"reason":r["phase"],"why":format!("/api/interactions/{}/why",r["id"].as_str().unwrap_or_default())})).collect();
        Ok(json!({"measured":true,"n_considered":rows.len(),"rev":rev,"recent_interactions":rows,"recent_effects":effects,"next_actions":next,"more":more,
            "health":{"outbox_pending":null,"why_unmeasured":"Device-local outboxes are measured by each browser"}}))
    }).await
}

async fn debug(
    State(state): State<AppState>,
    Query(params): Query<Params>,
) -> Result<Json<Value>, ReadError> {
    validate_params(&params)?;
    read(state.store, move |conn| {
        let rows = recent_rows(conn,&params)?;
        let mut groups = std::collections::BTreeMap::<(String,String),Vec<String>>::new();
        let mut coverage = [0usize;3];
        for row in &rows {
            let id = row["id"].as_str().unwrap_or_default();
            let count: i64 = conn.query_row("SELECT count(*) FROM _amux_interaction_effects WHERE interaction_id=?1",[id],|r|r.get(0))?;
            coverage[(count as usize).min(2)] += 1;
            groups.entry((row["command"]["kind"].as_str().unwrap_or_default().into(),row["phase"].as_str().unwrap_or_default().into())).or_default().push(id.into());
        }
        let groups: Vec<_> = groups.into_iter().map(|((kind,phase),ids)| json!({"kind":kind,"phase":phase,"count":ids.len(),"sample":ids[0]})).collect();
        Ok(json!({"measured":true,"n_considered":rows.len(),"groups":groups,"sampled":rows.len()>200,"effect_coverage":{"zero":coverage[0],"one":coverage[1],"many":coverage[2]}}))
    }).await
}
