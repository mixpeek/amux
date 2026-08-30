//! Calendar events API (RR-0089): CRUD over the LIVE `cal_events` table +
//! the iCal feed, field-compatible with the Python endpoints
//! (`/api/cal-events`, `/api/calendar.ics`) so the dashboard and Google/
//! Apple subscriptions work unchanged.
//!
//! Python parity decisions, recorded here so they are not "fixed" later:
//! - POST/PATCH return the FULL row as `dict(zip(cols, row))` — every
//!   column including `deleted`, discovered dynamically so live-DB appended
//!   columns pass through.
//! - POST answers 200 (not 201) like Python's `self._json(...)` default.
//! - DELETE is a soft delete and answers `{"deleted": id}` without checking
//!   existence (Python does not).
//! - PATCH ports Python's `body[k] or None` truthiness: a falsy value
//!   (empty string, null, 0, false) NULLs the column; `all_day` coerces to
//!   1/0.
//! - Ids are `EVT-N` minted from the SHARED `issue_counters` table
//!   (`_next_issue_id("EVT")`), so the two servers can never collide.
//!
//! Every mutation triggers a feed publish attempt (Python `_push_ical_bg`,
//! minus the 2s debounce — the publish is a no-op until the S3 publisher
//! lands, and the honest state goes to the IntegrationRegistry instead).

use super::AppState;
use crate::db::board_store::next_issue_id;
use crate::db::{PendingEvent, WriteOutcome};
use crate::integrations::calendar::{generate_ical_now, IcalPublisher, NoopPublisher};
use crate::integrations::{self, IntegrationRegistry, IntegrationState};
use amux_core::revision::{EntityType, MutationKind};
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use rusqlite::Connection;
use serde_json::{json, Map, Value};
use std::sync::{Arc, Mutex};

/// Feed-publishing context: swapped in tests, defaulted in production.
pub struct CalendarCtx {
    pub publisher: Arc<dyn IcalPublisher>,
    pub registry: Arc<IntegrationRegistry>,
    /// `AMUX_S3_BUCKET` — publish is only ATTEMPTED when the deployment
    /// asks for it, exactly like Python's `_push_ical_bg` early-return.
    pub s3_bucket: Option<String>,
}

impl CalendarCtx {
    pub fn new_default() -> Arc<Self> {
        // The real publisher (SA-124): aws-CLI backed, no new dependency —
        // the Noop stays only for deployments with no bucket configured or
        // no aws binary, and its refusal still reaches the registry.
        let publisher: Arc<dyn IcalPublisher> =
            match crate::integrations::calendar::CliS3Publisher::from_env() {
                Some(p) if p.is_configured() => Arc::new(p),
                _ => Arc::new(NoopPublisher),
            };
        Arc::new(CalendarCtx {
            publisher,
            registry: integrations::global_registry().clone(),
            s3_bucket: std::env::var("AMUX_S3_BUCKET").ok().filter(|s| !s.trim().is_empty()),
        })
    }
}

pub fn routes() -> Router<AppState> {
    routes_with(CalendarCtx::new_default())
}

pub fn routes_with(ctx: Arc<CalendarCtx>) -> Router<AppState> {
    Router::new()
        .route("/", get(list).post(create))
        .route("/{id}", axum::routing::patch(patch_event).delete(delete_event))
        .layer(Extension(ctx))
}

// ---- shared helpers -------------------------------------------------------

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

use super::internal;

fn ev(id: &str, mutation: MutationKind) -> PendingEvent {
    PendingEvent {
        entity_type: EntityType::Other("cal_event".into()),
        entity_id: id.to_string(),
        mutation,
        payload: None,
    }
}

/// `dict(zip(cols, row))` for a whole SELECT — column names discovered from
/// the statement so appended live-DB columns survive the port (crm.rs
/// reuses this).
pub(crate) fn query_rows_json(
    conn: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::types::ToSql],
) -> rusqlite::Result<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let rows = stmt.query_map(params, |row| {
        let mut obj = Map::new();
        for (i, name) in cols.iter().enumerate() {
            let v = match row.get_ref(i)? {
                rusqlite::types::ValueRef::Null => Value::Null,
                rusqlite::types::ValueRef::Integer(n) => json!(n),
                rusqlite::types::ValueRef::Real(f) => json!(f),
                rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                rusqlite::types::ValueRef::Blob(_) => Value::Null,
            };
            obj.insert(name.clone(), v);
        }
        Ok(Value::Object(obj))
    })?;
    rows.collect()
}

fn get_event_json(conn: &Connection, id: &str) -> rusqlite::Result<Option<Value>> {
    Ok(query_rows_json(conn, "SELECT * FROM cal_events WHERE id = ?1", &[&id])?.pop())
}

/// Python `_push_ical_bg` + `_upload_ical_to_s3`, honesty-first: skipped
/// when no bucket is configured; when a bucket IS configured but the
/// publisher cannot deliver (the pre-aws-sdk-s3 state), the registry says
/// so instead of a fake success (RR-0073).
fn push_ical(state: &AppState, ctx: &CalendarCtx) {
    if ctx.s3_bucket.is_none() {
        return;
    }
    if !ctx.publisher.is_configured() {
        ctx.registry.set(
            "calendar_s3",
            IntegrationState::Unavailable {
                reason: "AMUX_S3_BUCKET is set but the S3 publisher is not built \
                         (aws-sdk-s3 dep decision pending, RR-0089) — the iCal feed \
                         serves locally but does NOT publish to S3"
                    .into(),
            },
        );
        return;
    }
    let feed = match state.store.read().and_then(|conn| {
        Ok(query_rows_json(
            &conn,
            "SELECT * FROM cal_events WHERE deleted IS NULL ORDER BY start ASC",
            &[],
        )?)
    }) {
        Ok(events) => generate_ical_now(&events),
        Err(e) => {
            tracing::warn!(error = %e, "ical generation for publish failed");
            return;
        }
    };
    match ctx.publisher.publish(&feed) {
        Ok(()) => ctx.registry.set("calendar_s3", IntegrationState::Available),
        Err(e) => {
            tracing::warn!(error = %e, "ical publish failed");
            ctx.registry.set("calendar_s3", IntegrationState::Degraded { reason: e });
        }
    }
}

// ---- GET /api/cal-events --------------------------------------------------

pub async fn list(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Value>> {
        let conn = store.read()?;
        Ok(query_rows_json(
            &conn,
            "SELECT * FROM cal_events WHERE deleted IS NULL ORDER BY start ASC",
            &[],
        )?)
    })
    .await;
    match joined {
        Ok(Ok(rows)) => Json(Value::Array(rows)).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---- POST /api/cal-events -------------------------------------------------

pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<Arc<CalendarCtx>>,
    Json(body): Json<Value>,
) -> Response {
    let get_trim =
        |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
    let title = get_trim("title");
    let start = get_trim("start");
    if title.is_empty() || start.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "title and start are required" }));
    }
    // Python: `body.get("end") or None` — falsy strings become NULL.
    let opt = |k: &str| -> Option<String> {
        body.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from)
    };
    let all_day = body.get("all_day").map(truthy).unwrap_or(false) as i64;
    let (end, location, description, rrule) =
        (opt("end"), opt("location"), opt("description"), opt("rrule"));

    let slot: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let id = next_issue_id(conn, "EVT")?;
            let now = chrono::Utc::now().timestamp();
            conn.execute(
                "INSERT INTO cal_events (id,title,start,end,all_day,location,description,rrule,created,updated) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                rusqlite::params![id, title, start, end, all_day, location, description, rrule, now, now],
            )?;
            let row = get_event_json(conn, &id)?.unwrap_or(Value::Null);
            *slot_w.lock().expect("slot") = Some(row);
            Ok(WriteOutcome { applied: true, events: vec![ev(&id, MutationKind::Created)] })
        })
        .await;
    match write {
        Ok(_) => {
            push_ical(&state, &ctx); // events drive the external calendar feed
            let row = slot.lock().expect("slot").take().unwrap_or(Value::Null);
            Json(row).into_response() // Python answers 200, not 201
        }
        Err(e) => internal(e),
    }
}

/// Python truthiness over a JSON value (`if body.get("all_day")`).
use super::py_truthy as truthy;

// ---- PATCH /api/cal-events/{id} -------------------------------------------

const PATCH_ALLOWED: [&str; 7] =
    ["title", "start", "end", "all_day", "location", "description", "rrule"];

pub async fn patch_event(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(ctx): Extension<Arc<CalendarCtx>>,
    Json(body): Json<Value>,
) -> Response {
    // Python: `1 if (k=="all_day" and body[k]) else (0 if k=="all_day" else
    // (body[k] or None))` — ported including the falsy->NULL edge.
    let mut sets: Vec<String> = Vec::new();
    let mut vals: Vec<rusqlite::types::Value> = Vec::new();
    for k in PATCH_ALLOWED {
        let Some(v) = body.get(k) else { continue };
        sets.push(format!("{k}=?"));
        let stored: rusqlite::types::Value = if k == "all_day" {
            rusqlite::types::Value::Integer(truthy(v) as i64)
        } else if !truthy(v) {
            rusqlite::types::Value::Null
        } else {
            match v {
                Value::String(s) => rusqlite::types::Value::Text(s.clone()),
                Value::Number(n) if n.is_i64() => {
                    rusqlite::types::Value::Integer(n.as_i64().unwrap_or(0))
                }
                Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0)),
                Value::Bool(_) => rusqlite::types::Value::Integer(1),
                other => rusqlite::types::Value::Text(other.to_string()),
            }
        };
        vals.push(stored);
    }
    if sets.is_empty() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "no updatable fields" }));
    }

    let slot: Arc<Mutex<Option<Option<Value>>>> = Arc::new(Mutex::new(None));
    let slot_w = slot.clone();
    let id_w = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let now = chrono::Utc::now().timestamp();
            let sql =
                format!("UPDATE cal_events SET {},updated=? WHERE id=?", sets.join(","));
            let mut params: Vec<rusqlite::types::Value> = vals;
            params.push(rusqlite::types::Value::Integer(now));
            params.push(rusqlite::types::Value::Text(id_w.clone()));
            let n = conn.execute(&sql, rusqlite::params_from_iter(params))?;
            let row = get_event_json(conn, &id_w)?;
            *slot_w.lock().expect("slot") = Some(row);
            Ok(WriteOutcome {
                applied: n > 0,
                events: if n > 0 { vec![ev(&id_w, MutationKind::Updated)] } else { vec![] },
            })
        })
        .await;
    match write {
        Ok(_) => {
            push_ical(&state, &ctx);
            match slot.lock().expect("slot").take().flatten() {
                Some(row) => Json(row).into_response(),
                None => err(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
            }
        }
        Err(e) => internal(e),
    }
}

// ---- DELETE /api/cal-events/{id} ------------------------------------------

pub async fn delete_event(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Extension(ctx): Extension<Arc<CalendarCtx>>,
) -> Response {
    let id_w = id.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let now = chrono::Utc::now().timestamp();
            let n = conn.execute(
                "UPDATE cal_events SET deleted=?1,updated=?1 WHERE id=?2",
                rusqlite::params![now, id_w],
            )?;
            Ok(WriteOutcome {
                applied: n > 0,
                events: if n > 0 { vec![ev(&id_w, MutationKind::Deleted)] } else { vec![] },
            })
        })
        .await;
    match write {
        Ok(_) => {
            push_ical(&state, &ctx);
            // Python answers {"deleted": id} without checking existence.
            Json(json!({ "deleted": id })).into_response()
        }
        Err(e) => internal(e),
    }
}

// ---- GET /api/calendar.ics ------------------------------------------------

/// The subscription feed. Mounted PUBLIC (Python `_PUBLIC_PATHS` includes
/// `/api/calendar.ics`): Google/Apple fetchers cannot send a bearer token.
pub async fn ics_feed(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
        let conn = store.read()?;
        let events = query_rows_json(
            &conn,
            "SELECT * FROM cal_events WHERE deleted IS NULL ORDER BY start ASC",
            &[],
        )?;
        Ok(generate_ical_now(&events))
    })
    .await;
    match joined {
        Ok(Ok(feed)) => (
            [
                (header::CONTENT_TYPE, "text/calendar; charset=utf-8"),
                (header::CONTENT_DISPOSITION, "inline; filename=\"amux.ics\""),
            ],
            feed,
        )
            .into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(e),
    }
}

// ---------------------------------------------------------------------------
// Tests — temp-DB stores; router nested directly (the api/mod.rs mount
// belongs to the integrator).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    struct RecordingPublisher {
        published: Mutex<Vec<String>>,
    }
    impl IcalPublisher for RecordingPublisher {
        fn is_configured(&self) -> bool {
            true
        }
        fn publish(&self, ical: &str) -> Result<(), String> {
            self.published.lock().unwrap().push(ical.to_string());
            Ok(())
        }
    }

    fn app_with(
        bucket: Option<&str>,
        publisher: Arc<dyn IcalPublisher>,
    ) -> (axum::Router, tempfile::TempDir, Arc<IntegrationRegistry>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("cal-api-test.db")).unwrap();
        let state = AppState {
            store: Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        };
        let registry = Arc::new(IntegrationRegistry::new());
        let ctx = Arc::new(CalendarCtx {
            publisher,
            registry: registry.clone(),
            s3_bucket: bucket.map(String::from),
        });
        let router = Router::new()
            .nest("/api/cal-events", routes_with(ctx))
            .route("/api/calendar.ics", get(ics_feed))
            .with_state(state);
        (router, dir, registry)
    }

    fn app() -> (axum::Router, tempfile::TempDir, Arc<IntegrationRegistry>) {
        app_with(None, Arc::new(NoopPublisher))
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
                .header("content-type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        };
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, v)
    }

    #[tokio::test]
    async fn create_mints_evt_id_and_returns_full_row() {
        let (app, _dir, _reg) = app();
        let (st, row) = send(
            &app,
            "POST",
            "/api/cal-events",
            Some(json!({
                "title": "Call IRS", "start": "2026-08-10T10:00:00",
                "end": "2026-08-10T10:30:00", "description": "fee removal"
            })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{row}"); // Python answers 200
        assert_eq!(row["id"], json!("EVT-1")); // shared issue_counters format
        assert_eq!(row["title"], json!("Call IRS"));
        assert_eq!(row["all_day"], json!(0));
        assert_eq!(row["deleted"], Value::Null); // full row incl. deleted
        assert!(row["created"].as_i64().is_some());

        // Missing title/start -> Python's exact 400.
        let (st, e) = send(&app, "POST", "/api/cal-events", Some(json!({ "title": "x" }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], json!("title and start are required"));
    }

    #[tokio::test]
    async fn python_shaped_row_round_trips_column_by_column() {
        let (app, dir, _reg) = app();
        // Insert a row exactly as the Python server would (raw SQL, its
        // column set and value shapes).
        {
            let conn = rusqlite::Connection::open(dir.path().join("cal-api-test.db")).unwrap();
            conn.execute(
                "INSERT INTO cal_events (id,title,start,end,all_day,location,description,rrule,created,updated) \
                 VALUES ('EVT-77','Py event','2026-09-01',NULL,1,NULL,'desc','FREQ=DAILY',1754000000,1754000001)",
                [],
            )
            .unwrap();
        }
        let (st, list) = send(&app, "GET", "/api/cal-events", None).await;
        assert_eq!(st, StatusCode::OK);
        let row = &list.as_array().unwrap()[0];
        assert_eq!(row["id"], json!("EVT-77"));
        assert_eq!(row["title"], json!("Py event"));
        assert_eq!(row["start"], json!("2026-09-01"));
        assert_eq!(row["end"], Value::Null);
        assert_eq!(row["all_day"], json!(1));
        assert_eq!(row["location"], Value::Null);
        assert_eq!(row["description"], json!("desc"));
        assert_eq!(row["rrule"], json!("FREQ=DAILY"));
        assert_eq!(row["created"], json!(1754000000));
        assert_eq!(row["updated"], json!(1754000001));
        assert_eq!(row["deleted"], Value::Null);

        // PATCH round-trips against the Python-shaped row.
        let (st, patched) = send(
            &app,
            "PATCH",
            "/api/cal-events/EVT-77",
            Some(json!({ "title": "Renamed", "all_day": false, "start": "2026-09-01T09:00" })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{patched}");
        assert_eq!(patched["title"], json!("Renamed"));
        assert_eq!(patched["all_day"], json!(0));
        assert_eq!(patched["start"], json!("2026-09-01T09:00"));
        // Untouched columns survive.
        assert_eq!(patched["rrule"], json!("FREQ=DAILY"));
    }

    #[tokio::test]
    async fn patch_falsy_values_null_the_column_like_python() {
        let (app, _dir, _reg) = app();
        let (_, row) = send(
            &app,
            "POST",
            "/api/cal-events",
            Some(json!({ "title": "t", "start": "2026-08-10T10:00", "location": "HQ" })),
        )
        .await;
        let id = row["id"].as_str().unwrap().to_string();
        // Python: body[k] or None -> "" becomes NULL.
        let (st, patched) =
            send(&app, "PATCH", &format!("/api/cal-events/{id}"), Some(json!({ "location": "" })))
                .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(patched["location"], Value::Null);

        // No updatable fields -> Python's exact 400.
        let (st, e) =
            send(&app, "PATCH", &format!("/api/cal-events/{id}"), Some(json!({ "bogus": 1 })))
                .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(e["error"], json!("no updatable fields"));

        // Unknown id -> 404 {"error":"not found"}.
        let (st, e) =
            send(&app, "PATCH", "/api/cal-events/EVT-999", Some(json!({ "title": "x" }))).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(e["error"], json!("not found"));
    }

    #[tokio::test]
    async fn delete_is_soft_and_hides_from_list_and_feed() {
        let (app, _dir, _reg) = app();
        let (_, row) = send(
            &app,
            "POST",
            "/api/cal-events",
            Some(json!({ "title": "gone soon", "start": "2026-08-10T10:00" })),
        )
        .await;
        let id = row["id"].as_str().unwrap().to_string();
        let (st, del) = send(&app, "DELETE", &format!("/api/cal-events/{id}"), None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(del["deleted"], json!(id));
        let (_, list) = send(&app, "GET", "/api/cal-events", None).await;
        assert_eq!(list.as_array().unwrap().len(), 0);
        let (_, feed) = send(&app, "GET", "/api/calendar.ics", None).await;
        assert!(!feed.as_str().unwrap().contains("gone soon"));
    }

    #[tokio::test]
    async fn ics_feed_serves_rfc5545_with_calendar_headers() {
        let (app, _dir, _reg) = app();
        let (_, _) = send(
            &app,
            "POST",
            "/api/cal-events",
            Some(json!({ "title": "Feed check", "start": "2026-08-10T10:00:00" })),
        )
        .await;
        let req = Request::builder().uri("/api/calendar.ics").body(Body::empty()).unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/calendar; charset=utf-8"
        );
        assert_eq!(
            res.headers().get(header::CONTENT_DISPOSITION).unwrap(),
            "inline; filename=\"amux.ics\""
        );
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let feed = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(feed.starts_with("BEGIN:VCALENDAR\r\n"));
        assert!(feed.contains("SUMMARY:Feed check"));
        assert!(feed.ends_with("END:VCALENDAR\r\n"));
        assert!(feed.contains("EVT-1@amux"));
    }

    #[tokio::test]
    async fn s3_configured_without_publisher_surfaces_unavailable() {
        // RR-0073 honesty: bucket set + no publisher = Unavailable in the
        // registry, never a silent fake success.
        let (app, _dir, reg) = app_with(Some("some-bucket"), Arc::new(NoopPublisher));
        let (_, _) = send(
            &app,
            "POST",
            "/api/cal-events",
            Some(json!({ "title": "t", "start": "2026-08-10T10:00" })),
        )
        .await;
        match reg.get("calendar_s3") {
            Some(IntegrationState::Unavailable { reason }) => {
                assert!(reason.contains("S3 publisher is not built"), "{reason}");
            }
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn configured_publisher_receives_feed_and_reports_available() {
        let publisher = Arc::new(RecordingPublisher { published: Mutex::new(Vec::new()) });
        let (app, _dir, reg) = app_with(Some("some-bucket"), publisher.clone());
        let (_, _) = send(
            &app,
            "POST",
            "/api/cal-events",
            Some(json!({ "title": "Published event", "start": "2026-08-10T10:00" })),
        )
        .await;
        let feeds = publisher.published.lock().unwrap();
        assert_eq!(feeds.len(), 1);
        assert!(feeds[0].contains("SUMMARY:Published event"));
        assert_eq!(reg.get("calendar_s3"), Some(IntegrationState::Available));
    }

    #[tokio::test]
    async fn no_bucket_means_no_publish_attempt_and_no_registry_noise() {
        let publisher = Arc::new(RecordingPublisher { published: Mutex::new(Vec::new()) });
        let (app, _dir, reg) = app_with(None, publisher.clone());
        let (_, _) = send(
            &app,
            "POST",
            "/api/cal-events",
            Some(json!({ "title": "t", "start": "2026-08-10T10:00" })),
        )
        .await;
        assert!(publisher.published.lock().unwrap().is_empty());
        assert_eq!(reg.get("calendar_s3"), None);
    }
}
