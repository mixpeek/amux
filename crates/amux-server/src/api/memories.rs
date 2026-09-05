//! Memory API: CRUD over `_amux_memories` (RR-0071, Invariant 42).
//!
//! Mounted at `/api/memories` inside the `protected` router (api/mod.rs).
//! Semantics come from `amux_core::memory`: create mints version 1; update
//! bumps the version iff content actually changed (Invariant 37) and REFUSES
//! soft-deleted rows rather than resurrecting them; delete is soft — the row
//! stays as history. Concurrent writers use `expect_version` and get a 409,
//! never last-writer-wins (Invariant 35).
//!
//! Listing is scope-isolated through core's ONE visibility predicate
//! (`amux_core::memory::visible`, Invariant 2): `GET /?worker=<id|name>`
//! resolves for that worker (its private + its group's + global/org), a bare
//! `GET /` resolves for nobody in particular and sees only org/global.
//! `GET /{id}` is a direct fetch by identity and returns soft-deleted rows
//! with `deleted_at` set — a forensic read, not a resolution.

use super::AppState;
use crate::db::{memories, queries, PendingEvent, WriteOutcome};
use amux_core::ids::{GroupId, MemoryId, WorkerId};
use amux_core::memory::{MemoryEntry, MemoryError, MemoryProvenance, MemoryType};
use amux_core::revision::{EntityType, MutationKind};
use amux_core::scope::{ResolutionTarget, Scope};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_memories).post(create_memory))
        .route(
            "/{id}",
            get(get_memory).patch(patch_memory).delete(delete_memory),
        )
}

// ---- shared helpers -----------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

fn not_found(id: &str) -> Response {
    err(
        StatusCode::NOT_FOUND,
        json!({ "error": "memory not found", "id": id }),
    )
}

fn entry_body(e: &MemoryEntry) -> Value {
    serde_json::to_value(e).unwrap_or_else(|_| json!({ "id": e.id.as_str() }))
}

fn finish<T>(
    slot: &Mutex<Option<T>>,
    outcome: T,
    write: WriteOutcome,
) -> rusqlite::Result<WriteOutcome> {
    *slot.lock().expect("outcome slot poisoned") = Some(outcome);
    Ok(write)
}

fn no_write() -> WriteOutcome {
    WriteOutcome {
        applied: false,
        events: Vec::new(),
    }
}

fn ev(id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Memory,
        entity_id: id.to_string(),
        mutation,
        payload: None,
    }
}

/// Validate the id shapes riding inside a request Scope. `WorkerId`/`GroupId`
/// are serde-transparent strings, so deserialization alone does NOT check the
/// prefix/ULID shape — an unvalidated scope would store a row nobody can ever
/// resolve for (a memory that exists but is invisible; ethos rule 1).
fn validate_scope(scope: &Scope) -> Result<(), Box<Response>> {
    let bad = |what: &str, got: &str, e: String| {
        Err(Box::new(err(
            StatusCode::BAD_REQUEST,
            json!({ "error": e, "field": format!("scope.{what}"), "got": got }),
        )))
    };
    match scope {
        Scope::Org | Scope::Global => Ok(()),
        Scope::Group { id } => match GroupId::parse(id.as_str()) {
            Ok(_) => Ok(()),
            Err(e) => bad("id", id.as_str(), e.to_string()),
        },
        Scope::Worker { id } => match WorkerId::parse(id.as_str()) {
            Ok(_) => Ok(()),
            Err(e) => bad("id", id.as_str(), e.to_string()),
        },
    }
}

// ---- GET /api/memories --------------------------------------------------

#[derive(Deserialize)]
pub struct ListParams {
    /// Resolve visibility for this worker: a `wrk_` id (used as-is, its
    /// group looked up) or a display name/alias (resolved via the worker
    /// directory; unknown names are 404, not an empty list — an empty list
    /// must mean "nothing visible", never "you typo'd the name").
    #[serde(default)]
    pub worker: Option<String>,
    /// Explicit group override (`grp_` id). Wins over the worker's own group.
    #[serde(default)]
    pub group: Option<String>,
}

pub async fn list_memories(
    State(state): State<AppState>,
    Query(p): Query<ListParams>,
) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Result<_, Response>> {
        let conn = store.read()?;
        let mut target = ResolutionTarget::default();
        if let Some(w) = &p.worker {
            if let Ok(id) = WorkerId::parse(w) {
                // A raw wrk_ id resolves even if the worker row is gone —
                // its memories outlive it (they are history, Invariant 42).
                target.group = queries::get_worker(&conn, w)?
                    .and_then(|row| row.group_id)
                    .and_then(|g| GroupId::parse(&g).ok());
                target.worker = Some(id);
            } else {
                match queries::get_worker(&conn, w)? {
                    Some(row) => {
                        target.worker = WorkerId::parse(&row.id).ok();
                        target.group =
                            row.group_id.as_deref().and_then(|g| GroupId::parse(g).ok());
                    }
                    None => {
                        return Ok(Err(err(
                            StatusCode::NOT_FOUND,
                            json!({ "error": "worker not found", "worker": w }),
                        )))
                    }
                }
            }
        }
        if let Some(g) = &p.group {
            match GroupId::parse(g) {
                Ok(id) => target.group = Some(id),
                Err(e) => {
                    return Ok(Err(err(
                        StatusCode::BAD_REQUEST,
                        json!({ "error": e.to_string(), "group": g }),
                    )))
                }
            }
        }
        let items = memories::list_visible(&conn, &target)?;
        Ok(Ok(items))
    })
    .await;
    let items = match joined {
        Ok(Ok(Ok(items))) => items,
        Ok(Ok(Err(resp))) => return resp,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    let total = items.len();
    let items: Vec<Value> = items.iter().map(entry_body).collect();
    // No paging: the memory table is config-sized. `total == items.len()`
    // says so honestly (Invariant 40 — nothing silently omitted).
    Json(json!({ "items": items, "total": total })).into_response()
}

// ---- POST /api/memories -------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // Invariant 37: unknown fields rejected, not dropped
pub struct CreateMemoryBody {
    /// `{"level":"org"|"global"}` or `{"level":"group"|"worker","id":"..."}`.
    pub scope: Scope,
    pub name: String,
    pub content: String,
    pub memory_type: MemoryType,
    /// Defaults to human-written; workers pass their own provenance.
    #[serde(default)]
    pub provenance: Option<MemoryProvenance>,
}

enum CreateOutcome {
    Duplicate { existing_id: String },
    Created,
}

pub async fn create_memory(
    State(state): State<AppState>,
    Json(body): Json<CreateMemoryBody>,
) -> Response {
    if body.name.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "name is required" }));
    }
    if let Err(resp) = validate_scope(&body.scope) {
        return *resp;
    }
    if let Some(MemoryProvenance::WorkerWritten { worker }) = &body.provenance {
        if let Err(e) = WorkerId::parse(worker.as_str()) {
            return err(
                StatusCode::BAD_REQUEST,
                json!({ "error": e.to_string(), "field": "provenance.worker" }),
            );
        }
    }

    let entry = MemoryEntry::new(
        MemoryId::from_ulid(ulid::Ulid::new()),
        body.scope,
        body.name.trim(),
        body.content,
        body.memory_type,
        body.provenance.unwrap_or(MemoryProvenance::HumanWritten),
        chrono::Utc::now(),
    );

    let slot: Arc<Mutex<Option<CreateOutcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let entry_w = entry.clone();
    let write = state
        .store
        .write_async(move |conn| {
            // Duplicate check first, so a same-name-same-scope create is a
            // clean 409 naming the existing entry (the partial unique index
            // stays as the backstop, not the error path).
            if let Some(existing) =
                memories::get_live_by_scope_name(conn, &entry_w.scope, &entry_w.name)?
            {
                return finish(
                    &slot_w,
                    CreateOutcome::Duplicate { existing_id: existing.id.as_str().to_string() },
                    no_write(),
                );
            }
            memories::insert(conn, &entry_w)?;
            finish(
                &slot_w,
                CreateOutcome::Created,
                WriteOutcome {
                    applied: true,
                    events: vec![ev(entry_w.id.as_str(), MutationKind::Created)],
                },
            )
        })
        .await;
    let reply = match write {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        None => internal("create produced no outcome"),
        Some(CreateOutcome::Duplicate { existing_id }) => err(
            StatusCode::CONFLICT,
            json!({
                "error": "a live memory with this name already exists in this scope",
                "existing_id": existing_id,
            }),
        ),
        Some(CreateOutcome::Created) => {
            let mut v = entry_body(&entry);
            v["rev"] = json!(reply.rev.0);
            (StatusCode::CREATED, Json(v)).into_response()
        }
    }
}

// ---- GET /api/memories/{id} ---------------------------------------------

pub async fn get_memory(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let store = state.store.clone();
    let key = id.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let conn = store.read()?;
        Ok(memories::get(&conn, &key)?)
    })
    .await;
    match joined {
        Ok(Ok(Some(e))) => Json(entry_body(&e)).into_response(),
        Ok(Ok(None)) => not_found(&id),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- PATCH /api/memories/{id} -------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)] // Invariant 37
pub struct PatchMemoryBody {
    pub content: String,
    /// Optimistic concurrency (Invariant 35/42): when present, the write
    /// only applies if the entry is still at this version; otherwise 409.
    #[serde(default)]
    pub expect_version: Option<u64>,
}

enum MutateOutcome {
    NotFound,
    Conflict { current_version: u64 },
    /// Soft-deleted rows refuse mutation (Invariant 42) -> 409.
    Deleted { deleted_at: String },
    Noop { body: Value },
    Applied { body: Value },
}

pub async fn patch_memory(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PatchMemoryBody>,
) -> Response {
    let slot: Arc<Mutex<Option<MutateOutcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let key = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(mut e) = memories::get(conn, &key)? else {
                return finish(&slot_w, MutateOutcome::NotFound, no_write());
            };
            // Conflict outranks everything else: a stale caller should learn
            // their view is old before learning anything about the content.
            if let Some(expect) = body.expect_version {
                if expect != e.version {
                    return finish(
                        &slot_w,
                        MutateOutcome::Conflict { current_version: e.version },
                        no_write(),
                    );
                }
            }
            let before = e.version;
            match e.update(body.content.clone(), chrono::Utc::now()) {
                Err(MemoryError::AlreadyDeleted { .. }) => finish(
                    &slot_w,
                    MutateOutcome::Deleted {
                        deleted_at: e
                            .deleted_at
                            .map(|t| t.to_rfc3339())
                            .unwrap_or_default(),
                    },
                    no_write(),
                ),
                Ok(false) => {
                    // Identical content: honest no-op — no version bump, no
                    // rev bump, no event (Invariant 37).
                    finish(&slot_w, MutateOutcome::Noop { body: entry_body(&e) }, no_write())
                }
                Ok(true) => {
                    memories::persist_mutation(conn, &e, before)?;
                    finish(
                        &slot_w,
                        MutateOutcome::Applied { body: entry_body(&e) },
                        WriteOutcome {
                            applied: true,
                            events: vec![ev(e.id.as_str(), MutationKind::Updated)],
                        },
                    )
                }
            }
        })
        .await;
    mutate_response(write, slot, &id)
}

// ---- DELETE /api/memories/{id} ------------------------------------------

/// Soft delete: `deleted_at` is set, the row stays (Invariant 42 — history
/// is never lost). Double-delete is a 409, not a no-op: two actors both
/// believing they own the entry's lifecycle is worth surfacing.
pub async fn delete_memory(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let slot: Arc<Mutex<Option<MutateOutcome>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let key = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let Some(mut e) = memories::get(conn, &key)? else {
                return finish(&slot_w, MutateOutcome::NotFound, no_write());
            };
            let before = e.version;
            match e.soft_delete(chrono::Utc::now()) {
                Err(MemoryError::AlreadyDeleted { .. }) => finish(
                    &slot_w,
                    MutateOutcome::Deleted {
                        deleted_at: e
                            .deleted_at
                            .map(|t| t.to_rfc3339())
                            .unwrap_or_default(),
                    },
                    no_write(),
                ),
                Ok(()) => {
                    memories::persist_mutation(conn, &e, before)?;
                    finish(
                        &slot_w,
                        MutateOutcome::Applied { body: entry_body(&e) },
                        WriteOutcome {
                            applied: true,
                            events: vec![ev(e.id.as_str(), MutationKind::Deleted)],
                        },
                    )
                }
            }
        })
        .await;
    mutate_response(write, slot, &id)
}

fn mutate_response(
    write: anyhow::Result<crate::db::WriteReply>,
    slot: Arc<Mutex<Option<MutateOutcome>>>,
    id: &str,
) -> Response {
    let reply = match write {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let outcome = slot.lock().expect("outcome slot poisoned").take();
    match outcome {
        None => internal("mutation produced no outcome"),
        Some(MutateOutcome::NotFound) => not_found(id),
        Some(MutateOutcome::Conflict { current_version }) => err(
            StatusCode::CONFLICT,
            json!({ "error": "version conflict", "current_version": current_version }),
        ),
        Some(MutateOutcome::Deleted { deleted_at }) => err(
            StatusCode::CONFLICT,
            json!({
                "error": "memory is soft-deleted; mutations on deleted entries are rejected",
                "deleted_at": deleted_at,
            }),
        ),
        Some(MutateOutcome::Noop { body }) => {
            let mut v = body;
            v["applied"] = json!(false);
            Json(v).into_response()
        }
        Some(MutateOutcome::Applied { body }) => {
            let mut v = body;
            v["applied"] = json!(true);
            v["rev"] = json!(reply.rev.0);
            Json(v).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{router, AppState};
    use crate::db::Store;
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;

    fn app() -> (axum::Router, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("amux-test.db")).unwrap();
        let state = AppState {
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (router(state), dir)
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path);
        let req = match body {
            Some(v) => b
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, v)
    }

    fn wrk(n: u64) -> String {
        WorkerId::from_ulid(ulid::Ulid::from_parts(1_700_000_000_000, n as u128))
            .as_str()
            .to_string()
    }

    async fn create(app: &axum::Router, scope: Value, name: &str) -> Value {
        let (st, body) = send(
            app,
            "POST",
            "/api/memories",
            Some(json!({
                "scope": scope,
                "name": name,
                "content": format!("content of {name}"),
                "memory_type": "project",
            })),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "create failed: {body}");
        body
    }

    #[tokio::test]
    async fn crud_round_trip_with_version_bumps() {
        let (app, _dir) = app();
        let created = create(&app, json!({"level": "global"}), "runbook").await;
        let id = created["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("mem_"));
        assert_eq!(created["version"], json!(1));

        let (st, got) = send(&app, "GET", &format!("/api/memories/{id}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(got["content"], json!("content of runbook"));
        assert_eq!(got["scope"]["level"], json!("global"));

        // Update: version bumps, content replaced.
        let (st, patched) = send(
            &app,
            "PATCH",
            &format!("/api/memories/{id}"),
            Some(json!({ "content": "v2", "expect_version": 1 })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{patched}");
        assert_eq!(patched["applied"], json!(true));
        assert_eq!(patched["version"], json!(2));
        assert_eq!(patched["content"], json!("v2"));

        // Identical content: honest no-op, no bump (Invariant 37).
        let (st, noop) = send(
            &app,
            "PATCH",
            &format!("/api/memories/{id}"),
            Some(json!({ "content": "v2" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(noop["applied"], json!(false));
        assert_eq!(noop["version"], json!(2));

        // Stale expect_version: 409, nothing applied.
        let (st, conflict) = send(
            &app,
            "PATCH",
            &format!("/api/memories/{id}"),
            Some(json!({ "content": "v3", "expect_version": 1 })),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(conflict["current_version"], json!(2));
        let (_, back) = send(&app, "GET", &format!("/api/memories/{id}"), None).await;
        assert_eq!(back["content"], json!("v2"));
    }

    #[tokio::test]
    async fn scope_isolation_worker_a_cannot_list_worker_b() {
        let (app, _dir) = app();
        let (a, b) = (wrk(1), wrk(2));
        create(&app, json!({"level": "worker", "id": a}), "a-private").await;
        create(&app, json!({"level": "worker", "id": b}), "b-private").await;
        create(&app, json!({"level": "global"}), "shared").await;

        let (st, list) = send(&app, "GET", &format!("/api/memories?worker={a}"), None).await;
        assert_eq!(st, StatusCode::OK);
        let names: Vec<&str> = list["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"a-private"));
        assert!(names.contains(&"shared"));
        assert!(!names.contains(&"b-private"), "{names:?}");
        assert_eq!(list["total"], json!(2));

        // No target: org/global only.
        let (_, bare) = send(&app, "GET", "/api/memories", None).await;
        assert_eq!(bare["total"], json!(1));
        assert_eq!(bare["items"][0]["name"], json!("shared"));
    }

    #[tokio::test]
    async fn soft_delete_hides_from_list_refuses_mutation_survives_get() {
        let (app, _dir) = app();
        let created = create(&app, json!({"level": "global"}), "doomed").await;
        let id = created["id"].as_str().unwrap().to_string();

        let (st, deleted) = send(&app, "DELETE", &format!("/api/memories/{id}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(deleted["applied"], json!(true));
        assert_eq!(deleted["version"], json!(2)); // delete bumps too

        // Gone from every list...
        let (_, list) = send(&app, "GET", "/api/memories", None).await;
        assert_eq!(list["total"], json!(0));
        // ...but a direct GET still answers, with deleted_at set (history).
        let (st, got) = send(&app, "GET", &format!("/api/memories/{id}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(!got["deleted_at"].is_null());

        // Mutating a deleted entry is refused, not a resurrection.
        let (st, refused) = send(
            &app,
            "PATCH",
            &format!("/api/memories/{id}"),
            Some(json!({ "content": "back from the dead" })),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert!(refused["error"].as_str().unwrap().contains("soft-deleted"));
        // Double delete: 409 as well.
        let (st, _) = send(&app, "DELETE", &format!("/api/memories/{id}"), None).await;
        assert_eq!(st, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn duplicate_name_in_scope_is_409_but_other_scope_is_fine() {
        let (app, _dir) = app();
        let first = create(&app, json!({"level": "global"}), "runbook").await;
        let (st, dup) = send(
            &app,
            "POST",
            "/api/memories",
            Some(json!({
                "scope": {"level": "global"},
                "name": "runbook",
                "content": "x",
                "memory_type": "reference",
            })),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(dup["existing_id"], first["id"]);
        // Same name, different scope: allowed.
        create(&app, json!({"level": "worker", "id": wrk(3)}), "runbook").await;
    }

    #[tokio::test]
    async fn malformed_scope_and_unknown_worker_are_clean_errors() {
        let (app, _dir) = app();
        // A worker scope whose id is not a wrk_ ULID must be rejected at
        // create time, or the row could never be resolved for anyone.
        let (st, body) = send(
            &app,
            "POST",
            "/api/memories",
            Some(json!({
                "scope": {"level": "worker", "id": "not-a-worker-id"},
                "name": "x",
                "content": "y",
                "memory_type": "user",
            })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{body}");

        // Unknown worker NAME on list: 404, never a silently empty list.
        let (st, _) = send(&app, "GET", "/api/memories?worker=ghost", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }
}
