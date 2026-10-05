//! /api/map/location: the owner's location history (AMUX-5458).
//!
//! Design: docs/design/location-history.md. The amux iPhone app records points
//! with Core Location + Core Motion, buffers them on the phone and uploads them
//! here in batches. This module stores them (append-only, idempotent by the
//! phone-generated point id), and computes stops and trips ON READ, so a better
//! classifier improves every past day without a migration.
//!
//! Owned by the Map feature, not a new primitive: map.json holds the pins the
//! owner places, these tables hold where the owner has been. Points are not in
//! map.json because that document is rewritten whole on every save and a day of
//! driving is thousands of points.
//!
//! WRITES ARE OWNER-ONLY. When the server has an owner token, ingest and delete
//! require that bearer even from loopback: every worker runs on this machine,
//! and the loopback shortcut in auth.rs would otherwise let any lane invent
//! history. Reads follow normal dashboard auth, so the owner's own agents can
//! associate things with where he was.

use super::AppState;
use axum::extract::{OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rusqlite::{params, Connection};
use serde::Deserialize;
use serde_json::{json, Value};

/// Mounted under /api/map (see map::routes).
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/location/points", post(ingest_points).get(list_points))
        .route("/location/points/range", delete(delete_range))
        .route("/location/visits", post(ingest_visits))
        .route("/location/timeline", get(timeline))
        .route("/location/segments/{id}", get(segment_one))
        .route("/location/summary", get(summary))
        .route("/location/motion", post(ingest_motion))
        .route("/location/stats", get(stats))
        .route("/location/heatmap", get(heatmap))
        .route("/location/export", get(export))
}

/// Largest batch one POST may carry. The phone sends up to 500.
const MAX_BATCH: usize = 5000;
/// A stop: points that stay within this radius ...
const STOP_RADIUS_M: f64 = 100.0;
/// ... for at least this long.
const STOP_MIN_S: f64 = 300.0;
/// Wake-up fixes this long after the last live fix mean the live stream stalled.
const LIVE_STALL_S: f64 = 1800.0;
/// Longest gap between two fixes counted as moving time when weighting modes.
const PAIR_CAP_S: f64 = 120.0;
/// Most points a trip path carries in a timeline response.
const PATH_MAX: usize = 600;

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn owner_write_allowed(state: &AppState, headers: &HeaderMap, uri: &axum::http::Uri) -> bool {
    match &state.auth_token {
        Some(expected) => super::auth::provided_owner_token(headers, uri)
            .is_some_and(|t| super::auth::constant_time_eq(t.as_bytes(), expected.as_bytes())),
        // No owner token configured (first run, tests): still refuse a request
        // that says it comes from a worker lane.
        None => !headers.contains_key("x-amux-session") && !headers.contains_key("x-amux-worker"),
    }
}

fn refuse_write(what: &str) -> Response {
    tracing::warn!(target: "amux::location", verdict = "location_write_refused", what,
        "location history write refused: owner credential required");
    (
        StatusCode::FORBIDDEN,
        Json(json!({"ok": false, "error": "location history is written only by the owner's device (owner bearer required)"})),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Ingest
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug, Clone)]
pub(crate) struct InPoint {
    pub id: String,
    pub ts: f64,
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub alt: Option<f64>,
    #[serde(default)]
    pub h_acc: Option<f64>,
    #[serde(default)]
    pub v_acc: Option<f64>,
    #[serde(default)]
    pub speed: Option<f64>,
    #[serde(default)]
    pub course: Option<f64>,
    #[serde(default)]
    pub activity: Option<String>,
    #[serde(default)]
    pub activity_conf: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    // Raw capture (0100): every remaining CLLocation field.
    #[serde(default)]
    pub ell_alt: Option<f64>,
    #[serde(default)]
    pub speed_acc: Option<f64>,
    #[serde(default)]
    pub course_acc: Option<f64>,
    #[serde(default)]
    pub floor: Option<i64>,
    #[serde(default)]
    pub simulated: Option<bool>,
    #[serde(default)]
    pub accessory: Option<bool>,
    #[serde(default)]
    pub age_s: Option<f64>,
}

#[derive(Deserialize)]
struct PointBatch {
    #[serde(default)]
    device: String,
    points: Vec<InPoint>,
}

/// Why a point is refused, or None when it is storable.
pub(crate) fn point_problem(p: &InPoint, now: f64) -> Option<&'static str> {
    if p.id.trim().is_empty() || p.id.len() > 100 {
        return Some("id missing or longer than 100 characters");
    }
    if !p.ts.is_finite() || p.ts < 946_684_800.0 || p.ts > now + 86_400.0 {
        return Some("ts is not epoch seconds between 2000 and tomorrow");
    }
    if !p.lat.is_finite() || !(-90.0..=90.0).contains(&p.lat) || !p.lon.is_finite() || !(-180.0..=180.0).contains(&p.lon) {
        return Some("lat/lon out of range");
    }
    // RAW: a negative accuracy, speed or course is Core Location's own
    // "invalid" marker and is stored as delivered; the cleaned view ignores it.
    // Only a value that is not a number at all is refused.
    let nums = [p.alt, p.h_acc, p.v_acc, p.speed, p.course, p.ell_alt, p.speed_acc, p.course_acc, p.age_s];
    if nums.iter().any(|v| v.is_some_and(|x| !x.is_finite())) {
        return Some("a numeric field is not a finite number");
    }
    None
}

/// (accepted, duplicate, rejected[(id, why)]) for one ingest batch.
pub(crate) type IngestOutcome = (usize, usize, Vec<(String, &'static str)>);

/// Store a batch.
pub(crate) fn db_ingest_points(
    c: &Connection,
    device: &str,
    points: &[InPoint],
    now: f64,
) -> rusqlite::Result<IngestOutcome> {
    let (mut accepted, mut duplicate, mut rejected) = (0, 0, Vec::new());
    let mut stmt = c.prepare_cached(
        "INSERT OR IGNORE INTO location_points
           (id, device, ts, lat, lon, alt, h_acc, v_acc, speed, course, activity, activity_conf, source, received,
            ell_alt, speed_acc, course_acc, floor, simulated, accessory, age_s)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
    )?;
    for p in points {
        if let Some(why) = point_problem(p, now) {
            rejected.push((p.id.clone(), why));
            continue;
        }
        // Stored exactly as delivered, invalid markers (-1) included.
        let n = stmt.execute(params![
            p.id, device, p.ts, p.lat, p.lon, p.alt, p.h_acc, p.v_acc, p.speed, p.course,
            p.activity, p.activity_conf, p.source.clone().unwrap_or_default(), now,
            p.ell_alt, p.speed_acc, p.course_acc, p.floor, p.simulated.map(i64::from), p.accessory.map(i64::from), p.age_s
        ])?;
        if n == 1 { accepted += 1 } else { duplicate += 1 }
    }
    Ok((accepted, duplicate, rejected))
}

async fn write_value<T, F>(state: &AppState, f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
{
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let s2 = slot.clone();
    state
        .store
        .write_async(move |c| {
            let v = f(c)?;
            *s2.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await?;
    let v = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    v.ok_or_else(|| anyhow::anyhow!("write produced no value"))
}

fn server_error(e: anyhow::Error) -> Response {
    tracing::warn!(target: "amux::location", verdict = "location_store_failed", error = %e, "location history store failed");
    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "measured": false, "n_considered": 0, "why_unmeasured": e.to_string()}))).into_response()
}

/// First iPhone build with the live-stream restart (b86760b2, AMUX-5549).
/// Build numbers are the git commit count at the build's commit.
const LIVE_RESTART_BUILD: u64 = 6964;

/// The iPhone app's build number from its User-Agent ("AmuxApp/6925 CFNetwork/...").
fn app_build(headers: &HeaderMap) -> Option<u64> {
    let ua = headers.get(axum::http::header::USER_AGENT)?.to_str().ok()?;
    ua.split_whitespace().find_map(|t| t.strip_prefix("AmuxApp/"))?.parse().ok()
}

async fn ingest_points(
    State(state): State<AppState>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Json(batch): Json<PointBatch>,
) -> Response {
    if !owner_write_allowed(&state, &headers, &uri) {
        return refuse_write("points");
    }
    if batch.points.len() > MAX_BATCH {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"ok": false, "error": format!("at most {MAX_BATCH} points per request")}))).into_response();
    }
    let n = batch.points.len();
    let device = batch.device.chars().take(100).collect::<String>();
    let t = now();
    // A batch of only wake-up fixes ("manager": significant change, visit
    // relaunch) from a phone that has streamed live fixes before means its
    // continuous recording stalled. 2026-10-03: live stopped at 9:31 and the
    // day's moves went unrecorded with nothing in the log to say so.
    let wake_newest = batch.points.iter().filter(|p| p.source.as_deref() == Some("manager")).map(|p| p.ts).fold(f64::NAN, f64::max);
    let batch_has_live = batch.points.iter().any(|p| p.source.as_deref() == Some("live"));
    let dev = device.clone();
    let build = app_build(&headers);
    match write_value(&state, move |c| {
        let out = db_ingest_points(c, &device, &batch.points, t)?;
        let last_live: Option<f64> = if batch_has_live || wake_newest.is_nan() {
            None
        } else {
            c.query_row("SELECT MAX(ts) FROM location_points WHERE device=?1 AND source='live'", [&device], |r| r.get(0))?
        };
        Ok((out, last_live))
    })
    .await
    {
        Ok(((accepted, duplicate, rejected), last_live)) => {
            if let Some(live) = last_live.filter(|l| wake_newest - l > LIVE_STALL_S) {
                // Name the app build: on 2026-10-05 the stall ran 48 h because the
                // phone was still on build 6925, older than the restart fix, and
                // nothing said so.
                let has_fix = build.map(|b| b >= LIVE_RESTART_BUILD);
                tracing::warn!(target: "amux::location", verdict = "location_live_stalled", device = %dev,
                    stalled_s = (wake_newest - live).round(), app_build = ?build, app_has_restart_fix = ?has_fix,
                    measured = true, n_considered = n,
                    "location: phone sends only wake-up fixes; its live stream stopped (expected only in battery saver mode; app_has_restart_fix=false means update the iPhone app from TestFlight)");
            }
            tracing::info!(target: "amux::location", verdict = "location_ingest", accepted, duplicate,
                rejected = rejected.len(), measured = true, n_considered = n, "location points ingested");
            Json(json!({
                "ok": true, "accepted": accepted, "duplicate": duplicate,
                "rejected": rejected.iter().map(|(id, why)| json!({"id": id, "why": why})).collect::<Vec<_>>(),
                "measured": true, "n_considered": n,
            }))
            .into_response()
        }
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize, Debug, Clone)]
pub(crate) struct InVisit {
    pub id: String,
    pub arrival: f64,
    #[serde(default)]
    pub departure: Option<f64>,
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub h_acc: Option<f64>,
}

#[derive(Deserialize)]
struct VisitBatch {
    #[serde(default)]
    device: String,
    visits: Vec<InVisit>,
}

pub(crate) fn db_ingest_visits(c: &Connection, device: &str, visits: &[InVisit], now: f64) -> rusqlite::Result<(usize, usize)> {
    let (mut stored, mut rejected) = (0, 0);
    for v in visits {
        let ok = !v.id.trim().is_empty() && v.id.len() <= 100 && v.arrival.is_finite() && v.arrival > 946_684_800.0
            && (-90.0..=90.0).contains(&v.lat) && (-180.0..=180.0).contains(&v.lon);
        if !ok {
            rejected += 1;
            continue;
        }
        // CLVisit reports distantFuture/distantPast for an open edge; keep NULL.
        let departure = v.departure.filter(|d| d.is_finite() && *d > v.arrival && *d < now + 86_400.0);
        // An open visit is re-sent when it closes: the later report wins.
        c.execute(
            "INSERT INTO location_visits (id, device, arrival, departure, lat, lon, h_acc, received)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(id) DO UPDATE SET departure=COALESCE(excluded.departure, location_visits.departure), received=excluded.received",
            params![v.id, device, v.arrival, departure, v.lat, v.lon, v.h_acc, now],
        )?;
        stored += 1;
    }
    Ok((stored, rejected))
}

async fn ingest_visits(
    State(state): State<AppState>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Json(batch): Json<VisitBatch>,
) -> Response {
    if !owner_write_allowed(&state, &headers, &uri) {
        return refuse_write("visits");
    }
    let n = batch.visits.len();
    let device = batch.device.chars().take(100).collect::<String>();
    let t = now();
    match write_value(&state, move |c| db_ingest_visits(c, &device, &batch.visits, t)).await {
        Ok((stored, rejected)) => {
            tracing::info!(target: "amux::location", verdict = "location_visits_ingest", stored, rejected,
                measured = true, n_considered = n, "location visits ingested");
            Json(json!({"ok": true, "stored": stored, "rejected": rejected, "measured": true, "n_considered": n})).into_response()
        }
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
struct RangeQ {
    from: Option<f64>,
    to: Option<f64>,
    #[serde(default)]
    confirm: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn delete_range(
    State(state): State<AppState>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Query(q): Query<RangeQ>,
) -> Response {
    if !owner_write_allowed(&state, &headers, &uri) {
        return refuse_write("delete");
    }
    let (Some(from), Some(to)) = (q.from, q.to) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": "from and to (epoch seconds) are required"}))).into_response();
    };
    if q.confirm.as_deref() != Some("delete") || to <= from {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": "pass confirm=delete and a range with to > from"}))).into_response();
    }
    match write_value(&state, move |c| {
        let p = c.execute("DELETE FROM location_points WHERE ts >= ?1 AND ts < ?2", params![from, to])?;
        let v = c.execute("DELETE FROM location_visits WHERE arrival >= ?1 AND arrival < ?2", params![from, to])?;
        Ok((p, v))
    })
    .await
    {
        Ok((points, visits)) => {
            tracing::warn!(target: "amux::location", verdict = "location_range_deleted", from, to, points, visits,
                "owner deleted a range of location history");
            Json(json!({"ok": true, "deleted_points": points, "deleted_visits": visits})).into_response()
        }
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------------------
// Reading and segmenting
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct Pt {
    pub id: String,
    pub ts: f64,
    pub lat: f64,
    pub lon: f64,
    pub speed: Option<f64>,
    pub activity: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct Visit {
    pub id: String,
    pub arrival: f64,
    pub departure: Option<f64>,
    pub lat: f64,
    pub lon: f64,
    /// The phone never reported a departure: the end is the next visit's
    /// arrival, or now. An inference, and the timeline says so.
    pub open: bool,
}

/// One motion-coprocessor sample. `mode` is None when the phone was still or
/// could not tell, which ends a moving run.
#[derive(Debug, Clone)]
pub(crate) struct Motion {
    pub ts: f64,
    pub mode: Option<&'static str>,
}

pub(crate) fn haversine_m(a_lat: f64, a_lon: f64, b_lat: f64, b_lon: f64) -> f64 {
    let r = 6_371_000.0_f64;
    let (p1, p2) = (a_lat.to_radians(), b_lat.to_radians());
    let dp = (b_lat - a_lat).to_radians();
    let dl = (b_lon - a_lon).to_radians();
    let h = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * r * h.sqrt().asin()
}

/// THE CLEANED VIEW, computed at query time over the raw rows (which are never
/// thinned): usable horizontal accuracy (0 to 100 m), not stale on arrival
/// (30 s), not simulated. Speed below 0 is Core Location's "invalid".
pub(crate) const CLEAN_SQL: &str = "(h_acc IS NULL OR (h_acc >= 0 AND h_acc <= 100)) \
    AND (age_s IS NULL OR age_s <= 30) AND COALESCE(simulated, 0) = 0";

fn load_points(c: &Connection, from: f64, to: f64) -> rusqlite::Result<Vec<Pt>> {
    let mut stmt = c.prepare(&format!(
        "SELECT id, ts, lat, lon, speed, activity FROM location_points WHERE ts >= ?1 AND ts < ?2 AND {CLEAN_SQL} ORDER BY ts, id"
    ))?;
    let rows = stmt.query_map(params![from, to], |r| {
        let speed: Option<f64> = r.get(4)?;
        Ok(Pt { id: r.get(0)?, ts: r.get(1)?, lat: r.get(2)?, lon: r.get(3)?,
            speed: speed.filter(|v| *v >= 0.0), activity: r.get(5)? })
    })?;
    rows.collect()
}

fn count_raw(c: &Connection, from: f64, to: f64) -> rusqlite::Result<i64> {
    c.query_row("SELECT COUNT(*) FROM location_points WHERE ts >= ?1 AND ts < ?2", params![from, to], |r| r.get(0))
}

/// Visits as they happened, read so that an arrival report that never closed
/// cannot become a stop running to now.
///
/// iOS reports a visit twice, on arrival (open) and on departure, and the two
/// reports carry slightly different coordinates. The phone built the visit id
/// from arrival time AND coordinates, so the departure landed as a second row
/// and the arrival row stayed open forever. 2026-10-04: 13 such rows; the
/// oldest (10-03 12:10) read as one 24 h stop that swallowed the whole next
/// day, its six visits included. So, per device:
///
/// - an open row with a closed row of the same arrival (within 5 s) is that
///   visit's arrival report and is dropped;
/// - any other open row ends where that device's next visit begins.
///
/// Read-time only: the stored rows are the phone's raw reports and stay.
fn load_visits(c: &Connection, from: f64, to: f64) -> rusqlite::Result<Vec<Visit>> {
    let mut stmt = c.prepare(
        "SELECT id, arrival, dep, lat, lon, open FROM (
           SELECT v.id, v.arrival, v.lat, v.lon, v.departure IS NULL AS open,
                  COALESCE(v.departure,
                    (SELECT MIN(n.arrival) FROM location_visits n
                      WHERE n.device = v.device AND n.arrival > v.arrival + 5.0)) AS dep
             FROM location_visits v
            WHERE NOT (v.departure IS NULL AND EXISTS (
                    SELECT 1 FROM location_visits d
                     WHERE d.device = v.device AND d.departure IS NOT NULL
                       AND d.arrival BETWEEN v.arrival - 5.0 AND v.arrival + 5.0)))
          WHERE arrival < ?2 AND COALESCE(dep, ?2) >= ?1 ORDER BY arrival",
    )?;
    let rows = stmt.query_map(params![from, to], |r| {
        Ok(Visit { id: r.get(0)?, arrival: r.get(1)?, departure: r.get(2)?, lat: r.get(3)?, lon: r.get(4)?, open: r.get(5)? })
    })?;
    rows.collect()
}

/// Motion samples around a range, oldest first. Read wider than the range so
/// a move that began just before the first stop can still be timed.
fn load_motion(c: &Connection, from: f64, to: f64) -> rusqlite::Result<Vec<Motion>> {
    let mut stmt = c.prepare(
        "SELECT ts, walking, running, cycling, automotive FROM location_motion
          WHERE ts >= ?1 AND ts < ?2 ORDER BY ts",
    )?;
    let rows = stmt.query_map(params![from - 3600.0, to + 3600.0], |r| {
        let on = |k: usize| r.get::<_, Option<i64>>(k).map(|v| v.unwrap_or(0) != 0);
        let mode = if on(1)? {
            Some("walking")
        } else if on(2)? {
            Some("running")
        } else if on(3)? {
            Some("cycling")
        } else if on(4)? {
            Some("driving")
        } else {
            None
        };
        Ok(Motion { ts: r.get(0)?, mode })
    })?;
    rows.collect()
}

/// The mode a fix's motion activity names, or None for stationary/unknown.
fn activity_mode(a: Option<&str>) -> Option<&'static str> {
    match a.unwrap_or("") {
        "walking" => Some("walking"),
        "running" => Some("running"),
        "cycling" => Some("cycling"),
        "automotive" => Some("driving"),
        _ => None,
    }
}

fn speed_mode(mps: f64) -> &'static str {
    if mps < 2.5 {
        "walking"
    } else if mps < 7.0 {
        "cycling"
    } else {
        "driving"
    }
}

/// (start, end, lat, lon, id, point_count, end_inferred) of one stop while segmenting.
type StopRow = (f64, f64, f64, f64, String, usize, bool);

/// Stops and trips over points sorted by time, merged with iOS visits.
/// Pure, so the tests drive it with fixtures.
#[cfg(test)]
pub(crate) fn segment(points: &[Pt], visits: &[Visit], now: f64) -> Vec<Value> {
    segment_with_motion(points, visits, &[], now)
}

/// [`segment`], with unrecorded moves timed from the phone's motion samples.
pub(crate) fn segment_with_motion(points: &[Pt], visits: &[Visit], motion: &[Motion], now: f64) -> Vec<Value> {
    let n = points.len();
    // 1. Stop ranges over point indexes: an anchor point and every later fix
    //    within STOP_RADIUS_M of it, if that run lasts STOP_MIN_S. Live updates
    //    go quiet while the phone is still, so a stop is often just two fixes
    //    far apart in time; the anchor rule handles that.
    let mut stop_ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        let mut j = i + 1;
        while j < n && haversine_m(points[i].lat, points[i].lon, points[j].lat, points[j].lon) <= STOP_RADIUS_M {
            j += 1;
        }
        // Trim moving fixes off both ends: the anchor run also catches the
        // approach to a place and the first steps away from it, which belong
        // to the trips on either side.
        let moving = |p: &Pt| p.speed.is_some_and(|v| v > 1.0);
        let (mut a, mut b) = (i, j);
        while a < b && moving(&points[a]) {
            a += 1;
        }
        while b > a && moving(&points[b - 1]) {
            b -= 1;
        }
        if b > a + 1 && points[b - 1].ts - points[a].ts >= STOP_MIN_S {
            stop_ranges.push((a, b));
            i = b.max(i + 1);
        } else {
            i += 1;
        }
    }
    // 2. Fold iOS visits in: a visit is a stop for its whole span, and the
    //    points inside it belong to it.
    let mut stops: Vec<StopRow> = stop_ranges
        .iter()
        .map(|&(a, b)| {
            let k = (b - a) as f64;
            let lat = points[a..b].iter().map(|p| p.lat).sum::<f64>() / k;
            let lon = points[a..b].iter().map(|p| p.lon).sum::<f64>() / k;
            (points[a].ts, points[b - 1].ts, lat, lon, format!("stop_{}", points[a].id), b - a, false)
        })
        .collect();
    for v in visits {
        let end = v.departure.unwrap_or(now);
        // Strict: an open visit now ends at the next one's arrival, so two
        // visits that only TOUCH are neighbours, not the same stop (the 10:45
        // visit vanished behind a 10:05 one ending at 10:45:46).
        if stops.iter().any(|s| s.0 < end && v.arrival < s.1) {
            continue; // already found from points
        }
        let inside = points.iter().filter(|p| p.ts >= v.arrival && p.ts <= end).count();
        stops.push((v.arrival, end, v.lat, v.lon, format!("stop_visit_{}", v.id), inside, v.open || v.departure.is_none()));
    }
    stops.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut out: Vec<Value> = Vec::new();
    let in_stop = |t: f64| stops.iter().any(|s| t >= s.0 && t <= s.1);
    // 3. Trips: maximal runs of points outside every stop.
    let mut k = 0;
    let mut trips: Vec<(usize, usize)> = Vec::new();
    while k < n {
        if in_stop(points[k].ts) {
            k += 1;
            continue;
        }
        let start = k;
        while k < n && !in_stop(points[k].ts) {
            k += 1;
        }
        if k - start >= 2 {
            trips.push((start, k));
        }
    }
    for &(a, b) in &trips {
        let t = trip_json(&points[a..b]);
        // A few fixes in seconds between two stops (9:18:09 to 9:18:16, 10 m)
        // is the anchor rule handing over, not a trip.
        if t["distance_m"].as_f64().unwrap_or(0.0) < 50.0 && t["duration_s"].as_f64().unwrap_or(0.0) < 60.0 {
            continue;
        }
        out.push(t);
    }
    for s in &stops {
        out.push(json!({
            "id": s.4, "kind": "stop", "start": s.0, "end": s.1, "duration_s": (s.1 - s.0).max(0.0),
            "lat": s.2, "lon": s.3, "point_count": s.5, "end_inferred": s.6,
        }));
    }
    out.sort_by(|a, b| a["start"].as_f64().unwrap_or(0.0).total_cmp(&b["start"].as_f64().unwrap_or(0.0)));
    // 4. Gaps: travel the phone did not record (Ethan, 2026-10-03, "capturing
    //    movements is wrong": live fixes stopped at 9:31 and the day read as
    //    four stops back to back). Two neighbours that end and start at
    //    different places with no fixes between them were joined by a move
    //    nobody measured, so the timeline says so instead of hiding it.
    let place = |s: &Value, end: bool| -> Option<(f64, f64)> {
        if s["kind"] == "stop" {
            Some((s["lat"].as_f64()?, s["lon"].as_f64()?))
        } else {
            let k = if end { "to" } else { "from" };
            Some((s[k][0].as_f64()?, s[k][1].as_f64()?))
        }
    };
    let fixes_between = |a: f64, b: f64| {
        points.partition_point(|p| p.ts < b) - points.partition_point(|p| p.ts <= a)
    };
    let mut gaps = Vec::new();
    for w in out.windows(2) {
        let (Some(a), Some(b)) = (place(&w[0], true), place(&w[1], false)) else { continue };
        let d = haversine_m(a.0, a.1, b.0, b.1);
        let (s, e) = (w[0]["end"].as_f64().unwrap_or(0.0), w[1]["start"].as_f64().unwrap_or(0.0));
        // Two point-derived stops seconds apart were recorded continuously;
        // an iOS visit is coarse, so a move next to one counts at any length.
        let visit = |v: &Value| v["id"].as_str().is_some_and(|id| id.starts_with("stop_visit_"));
        let unrecorded = fixes_between(s, e) < 2 && (e - s >= 120.0 || visit(&w[0]) || visit(&w[1]));
        if d <= STOP_RADIUS_M || !unrecorded {
            continue;
        }
        gaps.push(json!({
            "id": format!("gap_{}", w[0]["id"].as_str().unwrap_or("")), "kind": "gap",
            "start": s, "end": e.max(s), "duration_s": (e - s).max(0.0),
            "from": [a.0, a.1], "to": [b.0, b.1], "distance_m": d.round(), "point_count": 0,
        }));
    }
    if !gaps.is_empty() {
        out.extend(gaps);
        // Start, then end: a zero-length gap between two visits shares its
        // start with the next stop and must sort before it.
        let key = |v: &Value, k: &str| v[k].as_f64().unwrap_or(0.0);
        out.sort_by(|a, b| key(a, "start").total_cmp(&key(b, "start")).then(key(a, "end").total_cmp(&key(b, "end"))));
        time_gaps(&mut out, motion, now);
    }
    out
}

/// Moving runs in the motion stream: (start, end, dominant mode). A run starts
/// at a moving sample and ends at the next still or unknown one; time between
/// samples is credited to the earlier sample's mode.
fn moving_runs(motion: &[Motion]) -> Vec<(f64, f64, &'static str)> {
    let mut runs = Vec::new();
    let mut cur: Option<(f64, std::collections::BTreeMap<&'static str, f64>)> = None;
    for (i, m) in motion.iter().enumerate() {
        match (m.mode, cur.as_mut()) {
            (Some(mode), None) => {
                let mut w = std::collections::BTreeMap::new();
                w.insert(mode, 0.0);
                cur = Some((m.ts, w));
            }
            (Some(_), Some(_)) => {}
            (None, Some(_)) => {
                let (start, w) = cur.take().unwrap_or_default();
                let mode = w.iter().max_by(|a, b| a.1.total_cmp(b.1)).map(|(k, _)| *k).unwrap_or("unknown");
                runs.push((start, m.ts, mode));
            }
            (None, None) => {}
        }
        if let (Some(mode), Some((_, w)), Some(next)) = (m.mode, cur.as_mut(), motion.get(i + 1)) {
            *w.entry(mode).or_default() += (next.ts - m.ts).max(0.0);
        }
    }
    if let (Some((start, w)), Some(last)) = (cur, motion.last()) {
        let mode = w.iter().max_by(|a, b| a.1.total_cmp(b.1)).map(|(k, _)| *k).unwrap_or("unknown");
        runs.push((start, last.ts, mode));
    }
    runs
}

/// Give each unrecorded move the time the phone's motion sensor says it took
/// (Ethan, 2026-10-05, "inaccurate": iOS stamped an arrival one second after the
/// previous departure, 0.3 mi away, and the day read "Moved · 0 min" between
/// stops that were really walking time). The move takes the moving run nearest
/// its boundary, and the stops on either side shrink to make room. A move with
/// no motion to time it keeps its instant but says the duration is unknown.
fn time_gaps(out: &mut [Value], motion: &[Motion], now: f64) {
    let runs = moving_runs(motion);
    let f = |v: &Value, k: &str| v[k].as_f64().unwrap_or(0.0);
    for i in 0..out.len() {
        if out[i]["kind"] != "gap" {
            continue;
        }
        let (s, e) = (f(&out[i], "start"), f(&out[i], "end"));
        // Never eat a neighbour whole: each keeps at least a minute.
        let lo = if i > 0 { f(&out[i - 1], "start") + 60.0 } else { s - 1200.0 };
        let hi = if i + 1 < out.len() { f(&out[i + 1], "end") - 60.0 } else { now };
        let (lo, hi) = (lo.min(s), hi.max(e));
        let near: Vec<&(f64, f64, &str)> =
            runs.iter().filter(|r| r.1 > lo.max(s - 1200.0) && r.0 < hi.min(e + 2700.0)).collect();
        // Chain outward from the run nearest the boundary, across pauses under 10 min.
        let dist = |r: &(f64, f64, &str)| if r.1 < s { s - r.1 } else if r.0 > e { r.0 - e } else { 0.0 };
        let Some(k) = (0..near.len()).min_by(|&a, &b| dist(near[a]).total_cmp(&dist(near[b]))) else {
            if let Some(o) = out[i].as_object_mut() {
                o.insert("duration_known".into(), json!(false));
            }
            continue;
        };
        let (mut a, mut b) = (k, k);
        while a > 0 && near[a].0 - near[a - 1].1 < 600.0 {
            a -= 1;
        }
        while b + 1 < near.len() && near[b + 1].0 - near[b].1 < 600.0 {
            b += 1;
        }
        let start = near[a].0.min(s).max(lo);
        let end = near[b].1.max(e).min(hi);
        let mut w: std::collections::BTreeMap<&str, f64> = Default::default();
        for r in &near[a..=b] {
            *w.entry(r.2).or_default() += (r.1 - r.0).max(1.0);
        }
        let mode = w.iter().max_by(|x, y| x.1.total_cmp(y.1)).map(|(m, _)| *m).unwrap_or("unknown");
        if let Some(o) = out[i].as_object_mut() {
            o.insert("start".into(), json!(start));
            o.insert("end".into(), json!(end));
            o.insert("duration_s".into(), json!(end - start));
            o.insert("duration_known".into(), json!(true));
            o.insert("timed_by".into(), json!("motion"));
            o.insert("mode".into(), json!(mode));
        }
        if i > 0 && out[i - 1]["kind"] == "stop" {
            let st = f(&out[i - 1], "start");
            if let Some(o) = out[i - 1].as_object_mut() {
                o.insert("end".into(), json!(start));
                o.insert("duration_s".into(), json!((start - st).max(0.0)));
            }
        }
        if i + 1 < out.len() && out[i + 1]["kind"] == "stop" {
            let en = f(&out[i + 1], "end");
            if let Some(o) = out[i + 1].as_object_mut() {
                o.insert("start".into(), json!(end));
                o.insert("duration_s".into(), json!((en - end).max(0.0)));
            }
        }
    }
}

fn trip_json(pts: &[Pt]) -> Value {
    let mut distance = 0.0;
    let mut weights: std::collections::BTreeMap<&'static str, f64> = Default::default();
    let mut moving_s = 0.0;
    let mut dwell_runs = 0usize;
    let mut dwell = 0.0;
    for w in pts.windows(2) {
        let (p, q) = (&w[0], &w[1]);
        let d = haversine_m(p.lat, p.lon, q.lat, q.lon);
        let dt = (q.ts - p.ts).max(0.0);
        distance += d;
        let pair_speed = if dt > 0.0 { d / dt } else { 0.0 };
        let spd = p.speed.unwrap_or(pair_speed);
        let capped = dt.min(PAIR_CAP_S);
        let mode = activity_mode(p.activity.as_deref()).unwrap_or_else(|| speed_mode(spd));
        *weights.entry(mode).or_default() += capped;
        moving_s += capped;
        // Station-like dwells: a run of near-zero speed lasting 20 to 180 s.
        if spd < 1.0 {
            dwell += dt;
        } else {
            if (20.0..=180.0).contains(&dwell) {
                dwell_runs += 1;
            }
            dwell = 0.0;
        }
    }
    let first = &pts[0];
    let last = &pts[pts.len() - 1];
    let duration = (last.ts - first.ts).max(0.0);
    let mut mode = weights
        .iter()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map(|(m, _)| *m)
        .unwrap_or("unknown");
    let mut confidence = if moving_s > 0.0 && weights.get(mode).copied().unwrap_or(0.0) / moving_s >= 0.6 { "measured" } else { "mixed" };
    // TRAIN IS A GUESS AND SAYS SO: the phone reports "automotive" for both.
    let straightness = if distance > 0.0 { haversine_m(first.lat, first.lon, last.lat, last.lon) / distance } else { 0.0 };
    let avg = if duration > 0.0 { distance / duration } else { 0.0 };
    if mode == "driving" && straightness >= 0.9 && (8.0..=45.0).contains(&avg) && dwell_runs >= 1 {
        mode = "train";
        confidence = "inferred";
    }
    let stride = (pts.len() / PATH_MAX).max(1);
    let mut path: Vec<Value> = pts.iter().step_by(stride).map(|p| json!([p.lat, p.lon])).collect();
    if !(pts.len() - 1).is_multiple_of(stride) {
        path.push(json!([last.lat, last.lon]));
    }
    let (mut lo_lat, mut lo_lon, mut hi_lat, mut hi_lon) = (90.0_f64, 180.0_f64, -90.0_f64, -180.0_f64);
    for p in pts {
        lo_lat = lo_lat.min(p.lat);
        lo_lon = lo_lon.min(p.lon);
        hi_lat = hi_lat.max(p.lat);
        hi_lon = hi_lon.max(p.lon);
    }
    json!({
        "id": format!("trip_{}", first.id), "kind": "trip", "mode": mode, "mode_confidence": confidence,
        "start": first.ts, "end": last.ts, "duration_s": duration, "distance_m": distance.round(),
        "from": [first.lat, first.lon], "to": [last.lat, last.lon],
        "bbox": [lo_lat, lo_lon, hi_lat, hi_lon], "point_count": pts.len(), "path": path,
    })
}

#[derive(Deserialize)]
struct TimelineQ {
    from: f64,
    to: f64,
}

async fn timeline(State(state): State<AppState>, Query(q): Query<TimelineQ>) -> Response {
    let valid = q.from.is_finite() && q.to.is_finite() && q.to > q.from && q.to - q.from <= 400.0 * 86_400.0;
    if !valid {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "measured": false, "n_considered": 0,
            "why_unmeasured": "from/to must be epoch seconds, to > from, at most 400 days"}))).into_response();
    }
    let (from, to) = (q.from, q.to);
    let t = now();
    match state
        .store
        .read_async(move |c| {
            let pts = load_points(c, from, to)?;
            let visits = load_visits(c, from, to)?;
            let motion = load_motion(c, from, to)?;
            let raw = count_raw(c, from, to)?;
            let segs = segment_with_motion(&pts, &visits, &motion, t);
            Ok((pts.len(), visits.len(), raw, segs))
        })
        .await
    {
        Ok((n, nv, raw, segs)) => Json(json!({
            "ok": true, "from": from, "to": to, "measured": true, "n_considered": n,
            "n_raw": raw, "visits_considered": nv, "segments": segs,
        }))
        .into_response(),
        Err(e) => server_error(e),
    }
}

/// One stop or trip by its stable id. The id carries its first point (or the
/// visit) id, so the server finds its time and recomputes around it.
async fn segment_one(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let key = id.clone();
    let t = now();
    match state
        .store
        .read_async(move |c| -> anyhow::Result<Option<Value>> {
            let anchor_ts: Option<f64> = if let Some(v) = key.strip_prefix("stop_visit_") {
                c.query_row("SELECT arrival FROM location_visits WHERE id=?1", [v], |r| r.get(0)).ok()
            } else {
                let pid = key.strip_prefix("stop_").or_else(|| key.strip_prefix("trip_")).unwrap_or("");
                c.query_row("SELECT ts FROM location_points WHERE id=?1", [pid], |r| r.get(0)).ok()
            };
            let Some(ts) = anchor_ts else { return Ok(None) };
            let (from, to) = (ts - 12.0 * 3600.0, ts + 36.0 * 3600.0);
            let segs = segment_with_motion(&load_points(c, from, to)?, &load_visits(c, from, to)?, &load_motion(c, from, to)?, t);
            Ok(segs.into_iter().find(|s| s["id"] == key.as_str()))
        })
        .await
    {
        Ok(Some(seg)) => Json(json!({"ok": true, "segment": seg})).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": "no stop or trip with that id"}))).into_response(),
        Err(e) => server_error(e),
    }
}

async fn list_points(State(state): State<AppState>, Query(q): Query<RangeQ>) -> Response {
    let (Some(from), Some(to)) = (q.from, q.to) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "measured": false, "n_considered": 0,
            "why_unmeasured": "from and to (epoch seconds) are required"}))).into_response();
    };
    let limit = q.limit.unwrap_or(10_000).min(50_000);
    match state
        .store
        .read_async(move |c| {
            let mut stmt = c.prepare(
                &format!("SELECT {RAW_COLS} FROM location_points WHERE ts >= ?1 AND ts < ?2 ORDER BY ts LIMIT ?3"),
            )?;
            let rows = stmt.query_map(params![from, to, limit as i64], raw_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<Value>>>()?)
        })
        .await
    {
        Ok(points) => Json(json!({"ok": true, "measured": true, "n_considered": points.len(), "limit": limit, "points": points})).into_response(),
        Err(e) => server_error(e),
    }
}

async fn summary(State(state): State<AppState>) -> Response {
    match state
        .store
        .read_async(|c| {
            let (n, first, last, received): (i64, Option<f64>, Option<f64>, Option<f64>) = c.query_row(
                "SELECT COUNT(*), MIN(ts), MAX(ts), MAX(received) FROM location_points",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            let visits: i64 = c.query_row("SELECT COUNT(*) FROM location_visits", [], |r| r.get(0))?;
            let motion: i64 = c.query_row("SELECT COUNT(*) FROM location_motion", [], |r| r.get(0))?;
            let mut stmt = c.prepare("SELECT device, COUNT(*), MAX(ts) FROM location_points GROUP BY device ORDER BY 3 DESC")?;
            let devices = stmt
                .query_map([], |r| Ok(json!({"device": r.get::<_, String>(0)?, "points": r.get::<_, i64>(1)?, "last_ts": r.get::<_, f64>(2)?})))?
                .collect::<rusqlite::Result<Vec<Value>>>()?;
            Ok(json!({"ok": true, "measured": true, "n_considered": n, "points": n, "visits": visits, "motion": motion,
                "first_ts": first, "last_ts": last, "last_received": received, "devices": devices}))
        })
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => server_error(e),
    }
}


// ---------------------------------------------------------------------------
// Raw capture: every column, motion stream, export (Ethan 2026-10-01 22:35)
// ---------------------------------------------------------------------------

/// Every raw column, in export order.
pub(crate) const RAW_COLS: &str = "id, device, ts, lat, lon, alt, ell_alt, h_acc, v_acc, speed, speed_acc, \
    course, course_acc, floor, simulated, accessory, age_s, activity, activity_conf, source, received";
const RAW_NAMES: [&str; 21] = ["id", "device", "ts", "lat", "lon", "alt", "ell_alt", "h_acc", "v_acc", "speed",
    "speed_acc", "course", "course_acc", "floor", "simulated", "accessory", "age_s", "activity", "activity_conf",
    "source", "received"];

fn raw_row(r: &rusqlite::Row) -> rusqlite::Result<Value> {
    let mut m = serde_json::Map::new();
    for (i, name) in RAW_NAMES.iter().enumerate() {
        let v: rusqlite::types::Value = r.get(i)?;
        let j = match v {
            rusqlite::types::Value::Null => Value::Null,
            rusqlite::types::Value::Integer(n) => match *name {
                "simulated" | "accessory" => json!(n != 0),
                _ => json!(n),
            },
            rusqlite::types::Value::Real(f) => json!(f),
            rusqlite::types::Value::Text(t) => json!(t),
            rusqlite::types::Value::Blob(_) => Value::Null,
        };
        m.insert((*name).to_string(), j);
    }
    Ok(Value::Object(m))
}

#[derive(Deserialize, Debug, Clone)]
pub(crate) struct InMotion {
    pub id: String,
    pub ts: f64,
    #[serde(default)]
    pub stationary: bool,
    #[serde(default)]
    pub walking: bool,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub cycling: bool,
    #[serde(default)]
    pub automotive: bool,
    #[serde(default)]
    pub unknown: bool,
    #[serde(default)]
    pub confidence: String,
}

#[derive(Deserialize)]
struct MotionBatch {
    #[serde(default)]
    device: String,
    motion: Vec<InMotion>,
}

/// Store Core Motion transitions. Returns (accepted, duplicate, rejected).
pub(crate) fn db_ingest_motion(c: &Connection, device: &str, items: &[InMotion], now: f64) -> rusqlite::Result<(usize, usize, usize)> {
    let (mut acc, mut dup, mut rej) = (0, 0, 0);
    let mut stmt = c.prepare_cached(
        "INSERT OR IGNORE INTO location_motion (id, device, ts, stationary, walking, running, cycling, automotive, unknown, confidence, received)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
    )?;
    for m in items {
        if m.id.trim().is_empty() || m.id.len() > 100 || !m.ts.is_finite() || m.ts < 946_684_800.0 || m.ts > now + 86_400.0 {
            rej += 1;
            continue;
        }
        let n = stmt.execute(params![m.id, device, m.ts, m.stationary, m.walking, m.running, m.cycling,
            m.automotive, m.unknown, m.confidence.chars().take(20).collect::<String>(), now])?;
        if n == 1 { acc += 1 } else { dup += 1 }
    }
    Ok((acc, dup, rej))
}

async fn ingest_motion(
    State(state): State<AppState>,
    headers: HeaderMap,
    OriginalUri(uri): OriginalUri,
    Json(batch): Json<MotionBatch>,
) -> Response {
    if !owner_write_allowed(&state, &headers, &uri) {
        return refuse_write("motion");
    }
    if batch.motion.len() > MAX_BATCH {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"ok": false, "error": format!("at most {MAX_BATCH} items per request")}))).into_response();
    }
    let n = batch.motion.len();
    let device = batch.device.chars().take(100).collect::<String>();
    let t = now();
    match write_value(&state, move |c| db_ingest_motion(c, &device, &batch.motion, t)).await {
        Ok((accepted, duplicate, rejected)) => {
            tracing::info!(target: "amux::location", verdict = "location_motion_ingest", accepted, duplicate, rejected,
                measured = true, n_considered = n, "motion activity ingested");
            Json(json!({"ok": true, "accepted": accepted, "duplicate": duplicate, "rejected": rejected,
                "measured": true, "n_considered": n})).into_response()
        }
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
struct ExportQ {
    from: f64,
    to: f64,
    #[serde(default)]
    format: Option<String>,
}

/// Most rows one export returns; a larger range says so in a header.
const EXPORT_MAX: i64 = 2_000_000;

fn csv_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) if s.contains([',', '"', '\n']) => format!("\"{}\"", s.replace('"', "\"\"")),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub(crate) fn render_export(rows: &[Value], format: &str) -> (String, &'static str, &'static str) {
    match format {
        "csv" => {
            let mut out = RAW_NAMES.join(",");
            out.push('\n');
            for r in rows {
                let line: Vec<String> = RAW_NAMES.iter().map(|k| csv_cell(&r[*k])).collect();
                out.push_str(&line.join(","));
                out.push('\n');
            }
            (out, "text/csv; charset=utf-8", "csv")
        }
        "gpx" => {
            let mut out = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<gpx version=\"1.1\" creator=\"amux\" \
                xmlns=\"http://www.topografix.com/GPX/1/1\" xmlns:amux=\"https://amux.io/gpx/1\">\n<trk><name>amux location history</name><trkseg>\n");
            for r in rows {
                let ts = r["ts"].as_f64().unwrap_or(0.0);
                let time = chrono::DateTime::<chrono::Utc>::from_timestamp(ts.floor() as i64, ((ts.fract()) * 1e9) as u32)
                    .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                    .unwrap_or_default();
                out.push_str(&format!("<trkpt lat=\"{}\" lon=\"{}\">", r["lat"], r["lon"]));
                if let Some(a) = r["alt"].as_f64() {
                    out.push_str(&format!("<ele>{a}</ele>"));
                }
                out.push_str(&format!("<time>{time}</time><extensions>"));
                for k in ["h_acc", "v_acc", "speed", "speed_acc", "course", "course_acc", "floor", "age_s", "activity", "activity_conf", "source", "id"] {
                    if !r[k].is_null() {
                        let v = match &r[k] { Value::String(s) => xml_escape(s), other => other.to_string() };
                        out.push_str(&format!("<amux:{k}>{v}</amux:{k}>"));
                    }
                }
                out.push_str("</extensions></trkpt>\n");
            }
            out.push_str("</trkseg></trk>\n</gpx>\n");
            (out, "application/gpx+xml", "gpx")
        }
        _ => {
            let features: Vec<Value> = rows.iter().map(|r| {
                let mut coords = vec![r["lon"].clone(), r["lat"].clone()];
                if !r["alt"].is_null() {
                    coords.push(r["alt"].clone());
                }
                json!({"type": "Feature", "geometry": {"type": "Point", "coordinates": coords}, "properties": r})
            }).collect();
            (json!({"type": "FeatureCollection", "features": features}).to_string(), "application/geo+json", "geojson")
        }
    }
}

async fn export(State(state): State<AppState>, Query(q): Query<ExportQ>) -> Response {
    if !(q.from.is_finite() && q.to.is_finite() && q.to > q.from) {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "measured": false, "n_considered": 0,
            "why_unmeasured": "from/to must be epoch seconds with to > from"}))).into_response();
    }
    let (from, to) = (q.from, q.to);
    let format = q.format.unwrap_or_else(|| "geojson".into());
    if !["geojson", "gpx", "csv"].contains(&format.as_str()) {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": "format must be geojson, gpx or csv"}))).into_response();
    }
    match state
        .store
        .read_async(move |c| {
            let total = count_raw(c, from, to)?;
            let mut stmt = c.prepare(&format!("SELECT {RAW_COLS} FROM location_points WHERE ts >= ?1 AND ts < ?2 ORDER BY ts, id LIMIT ?3"))?;
            let rows = stmt.query_map(params![from, to, EXPORT_MAX], raw_row)?.collect::<rusqlite::Result<Vec<Value>>>()?;
            Ok((total, rows))
        })
        .await
    {
        Ok((total, rows)) => {
            let (body, ctype, ext) = render_export(&rows, &format);
            tracing::info!(target: "amux::location", verdict = "location_export", format = %format, rows = rows.len(),
                total, measured = true, n_considered = total, "raw location export");
            let name = format!("attachment; filename=\"amux-location-{}-{}.{ext}\"", from as i64, to as i64);
            (
                [
                    (axum::http::header::CONTENT_TYPE, ctype.to_string()),
                    (axum::http::header::CONTENT_DISPOSITION, name),
                    (axum::http::HeaderName::from_static("x-amux-rows"), rows.len().to_string()),
                    (axum::http::HeaderName::from_static("x-amux-truncated"), if (rows.len() as i64) < total { "1" } else { "0" }.to_string()),
                ],
                body,
            )
                .into_response()
        }
        Err(e) => server_error(e),
    }
}

// ---------------------------------------------------------------------------
// Analytics over the cleaned view
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct HeatQ {
    #[serde(default)]
    from: Option<f64>,
    #[serde(default)]
    to: Option<f64>,
    /// Cell size in metres (default 50).
    #[serde(default)]
    cell_m: Option<f64>,
    /// Comma-separated modes to keep (driving,walking,cycling,running,still).
    #[serde(default)]
    modes: Option<String>,
}

/// One fix's travel mode, for the per-mode heatmap. Core Motion's activity
/// wins; without it the fix's own speed decides (m/s, the same thresholds as
/// `speed_mode`), and a fix with no usable speed is `still`. Per fix, so it is
/// cheap over years of points; trips use the segment classifier instead.
pub(crate) const MODE_SQL: &str = "CASE \
    WHEN activity = 'automotive' THEN 'driving' WHEN activity = 'cycling' THEN 'cycling' \
    WHEN activity = 'running' THEN 'running' WHEN activity = 'walking' THEN 'walking' \
    WHEN speed >= 7.0 THEN 'driving' WHEN speed >= 2.5 THEN 'cycling' WHEN speed >= 0.6 THEN 'walking' \
    ELSE 'still' END";
const HEAT_MODES: [&str; 5] = ["driving", "walking", "cycling", "running", "still"];

const HEAT_MAX_CELLS: i64 = 20_000;

async fn heatmap(State(state): State<AppState>, Query(q): Query<HeatQ>) -> Response {
    let from = q.from.unwrap_or(0.0);
    let to = q.to.unwrap_or(f64::MAX);
    let deg = q.cell_m.unwrap_or(50.0).clamp(10.0, 5000.0) / 111_320.0;
    // Only known mode names reach the SQL, so the list is safe to inline.
    let wanted: Vec<&'static str> = match q.modes.as_deref() {
        Some(list) => HEAT_MODES.iter().copied().filter(|m| list.split(',').any(|x| x.trim() == *m)).collect(),
        None => HEAT_MODES.to_vec(),
    };
    let mode_filter = format!("m IN ({})", wanted.iter().map(|m| format!("'{m}'")).collect::<Vec<_>>().join(","));
    match state
        .store
        .read_async(move |c| {
            // Integer grid cells per mode; the +1e7 offset makes CAST round down for negatives too.
            let sql = format!(
                "SELECT a, b, m, COUNT(*) AS n FROM (
                   SELECT CAST(lat/?3 + 10000000 AS INTEGER) - 10000000 AS a, CAST(lon/?3 + 10000000 AS INTEGER) - 10000000 AS b,
                          {MODE_SQL} AS m
                     FROM location_points WHERE ts >= ?1 AND ts < ?2 AND {CLEAN_SQL})
                  WHERE {mode_filter} GROUP BY a, b, m ORDER BY n DESC LIMIT ?4"
            );
            let mut stmt = c.prepare(&sql)?;
            let cells = stmt
                .query_map(params![from, to, deg, HEAT_MAX_CELLS + 1], |r| {
                    let (a, b, m, n): (i64, i64, String, i64) = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
                    Ok(json!([(a as f64 + 0.5) * deg, (b as f64 + 0.5) * deg, n, m]))
                })?
                .collect::<rusqlite::Result<Vec<Value>>>()?;
            let mut per_mode = serde_json::Map::new();
            let mut stmt = c.prepare(&format!(
                "SELECT {MODE_SQL} AS m, COUNT(*) FROM location_points WHERE ts >= ?1 AND ts < ?2 AND {CLEAN_SQL} GROUP BY m"))?;
            let mut n: i64 = 0;
            for row in stmt.query_map(params![from, to], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (m, k) = row?;
                n += k;
                per_mode.insert(m, json!(k));
            }
            Ok((n, cells, per_mode))
        })
        .await
    {
        Ok((n, mut cells, per_mode)) => {
            let truncated = cells.len() as i64 > HEAT_MAX_CELLS;
            cells.truncate(HEAT_MAX_CELLS as usize);
            Json(json!({"ok": true, "measured": true, "n_considered": n, "cell_deg": deg, "cells": cells,
                "n_cells": cells.len(), "truncated": truncated, "points_by_mode": per_mode,
                "modes": wanted})).into_response()
        }
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
struct StatsQ {
    from: f64,
    to: f64,
    #[serde(default)]
    bucket: Option<String>,
    /// Minutes east of UTC for the viewer's day boundaries (e.g. -240 for New York in summer).
    #[serde(default)]
    tz_offset_min: Option<f64>,
}

/// A place: stops grouped on a fixed ~150 m grid. One function, so a better
/// clustering can replace it without touching storage or callers.
pub(crate) fn place_key(lat: f64, lon: f64) -> (i64, i64) {
    ((lat / 0.00135).floor() as i64, (lon / 0.0018).floor() as i64)
}

/// An area: places grouped on a ~5 km grid, the unit "Top Places" counts
/// visits in and names ("New York, NY"). Coarse on purpose: one reverse geocode
/// per area, cached in location_place_names.
pub(crate) fn area_key(lat: f64, lon: f64) -> String {
    // "b_": names looked up from the cell centre (see area_lookup_url). Names
    // cached under "a_" were looked up with precise visit-weighted centres.
    format!("b_{}_{}", (lat / AREA_DEG).floor() as i64, (lon / AREA_DEG).floor() as i64)
}

const AREA_DEG: f64 = 0.05;

/// The reverse-geocode request for an area. PRIVACY: it carries only the
/// centre of the area's ~5 km grid cell, rounded to 2 decimals (~1 km), never
/// where the owner actually stopped: an area dominated by home would otherwise
/// put a home-precise coordinate in a third party's request log.
pub(crate) fn area_lookup_url(base: &str, key: &str) -> Option<String> {
    let mut parts = key.strip_prefix("b_")?.split('_');
    let (i, j): (i64, i64) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
    if parts.next().is_some() {
        return None;
    }
    let (lat, lon) = ((i as f64 + 0.5) * AREA_DEG, (j as f64 + 0.5) * AREA_DEG);
    Some(format!("{base}/reverse?format=jsonv2&lat={lat:.2}&lon={lon:.2}&zoom=10&addressdetails=1"))
}

/// A city spans several ~5 km cells, so cells that resolved to the same name
/// are one place: visits and time add up, the position is visit-weighted, and
/// the busiest cell's key is kept. Unnamed cells stay separate.
pub(crate) fn merge_named_areas(areas: Vec<Value>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut by_name: std::collections::HashMap<String, usize> = Default::default();
    for a in areas {
        let name = a["name"].as_str().map(str::to_string);
        match name.as_ref().and_then(|n| by_name.get(n).copied()) {
            Some(i) => {
                let (v0, v1) = (out[i]["visits"].as_i64().unwrap_or(0), a["visits"].as_i64().unwrap_or(0));
                let w = (v0 + v1).max(1) as f64;
                let lat = (out[i]["lat"].as_f64().unwrap_or(0.0) * v0 as f64 + a["lat"].as_f64().unwrap_or(0.0) * v1 as f64) / w;
                let lon = (out[i]["lon"].as_f64().unwrap_or(0.0) * v0 as f64 + a["lon"].as_f64().unwrap_or(0.0) * v1 as f64) / w;
                let t = out[i]["time_s"].as_f64().unwrap_or(0.0) + a["time_s"].as_f64().unwrap_or(0.0);
                let o = out[i].as_object_mut().expect("area object");
                o.insert("visits".into(), json!(v0 + v1));
                o.insert("time_s".into(), json!(t));
                o.insert("lat".into(), json!(lat));
                o.insert("lon".into(), json!(lon));
            }
            None => {
                if let Some(n) = name {
                    by_name.insert(n, out.len());
                }
                out.push(a);
            }
        }
    }
    out.sort_by(|a, b| b["visits"].as_i64().cmp(&a["visits"].as_i64()));
    out
}

/// A failed lookup is retried after this long; a successful one is kept.
const AREA_RETRY_S: f64 = 86_400.0;

/// (name, status, fetched_at) of a cached area name.
type CachedAreaName = (Option<String>, String, f64);

/// (visits, time_s, lat * visits, lon * visits, weight) while grouping places into areas.
type AreaAcc = (i64, f64, f64, f64, i64);

/// "New York, NY" from a Nominatim reverse result (zoom 10). The place is the
/// city, town or village; the region is the ISO subdivision code where the
/// country uses short codes (US, CA, AU), else the state, else the country.
pub(crate) fn area_name(v: &Value) -> Option<String> {
    let a = v.get("address")?;
    let place = ["city", "town", "village", "municipality", "hamlet", "county"]
        .iter()
        .find_map(|k| a.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))?;
    let cc = a.get("country_code").and_then(Value::as_str).unwrap_or("");
    let iso = a.get("ISO3166-2-lvl4").and_then(Value::as_str).unwrap_or("");
    let region = if matches!(cc, "us" | "ca" | "au") && iso.contains('-') {
        iso.rsplit('-').next().map(str::to_string)
    } else {
        a.get("state").or_else(|| a.get("country")).and_then(Value::as_str).map(str::to_string)
    };
    Some(match region.filter(|r| !r.is_empty() && r != place) {
        Some(r) => format!("{place}, {r}"),
        None => place.to_string(),
    })
}

fn bucket_label(day: i64, bucket: &str) -> String {
    let d = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap() + chrono::Duration::days(day);
    match bucket {
        "month" => d.format("%Y-%m").to_string(),
        "week" => {
            let monday = d - chrono::Duration::days(i64::from(chrono::Datelike::weekday(&d).num_days_from_monday()));
            monday.format("%Y-%m-%d").to_string()
        }
        _ => d.format("%Y-%m-%d").to_string(),
    }
}

#[derive(Default)]
struct PlaceAcc {
    time_s: f64,
    visits: i64,
    lat_sum: f64,
    lon_sum: f64,
    first: f64,
}

/// The first analytics set. Segments one local day at a time so memory stays
/// bounded however long the range is (a trip that crosses midnight counts in
/// both days).
pub(crate) fn compute_stats(c: &Connection, from: f64, to: f64, bucket: &str, off_s: f64, now: f64) -> anyhow::Result<Value> {
    let mut days: Vec<i64> = Vec::new();
    {
        let mut stmt = c.prepare(&format!(
            "SELECT DISTINCT CAST((ts + ?3) / 86400 AS INTEGER) FROM location_points WHERE ts >= ?1 AND ts < ?2 AND {CLEAN_SQL} ORDER BY 1"
        ))?;
        for d in stmt.query_map(params![from, to, off_s], |r| r.get::<_, i64>(0))? {
            days.push(d?);
        }
    }
    let mut buckets: std::collections::BTreeMap<String, std::collections::BTreeMap<&'static str, (f64, f64, i64)>> = Default::default();
    let mut totals: std::collections::BTreeMap<&'static str, (f64, f64, i64)> = Default::default();
    let mut places: std::collections::HashMap<(i64, i64), PlaceAcc> = Default::default();
    let mut longest: Option<Value> = None;
    let mut n_points = 0usize;
    for day in &days {
        let d_from = (*day as f64) * 86_400.0 - off_s;
        let (lo, hi) = (d_from.max(from), (d_from + 86_400.0).min(to));
        let pts = load_points(c, lo, hi)?;
        n_points += pts.len();
        let visits = load_visits(c, lo, hi)?;
        let motion = load_motion(c, lo, hi)?;
        let label = bucket_label(*day, bucket);
        for s in segment_with_motion(&pts, &visits, &motion, now) {
            if s["kind"] == "trip" {
                let mode: &'static str = match s["mode"].as_str().unwrap_or("unknown") {
                    "walking" => "walking", "running" => "running", "cycling" => "cycling",
                    "driving" => "driving", "train" => "train", _ => "unknown",
                };
                let (dist, dur) = (s["distance_m"].as_f64().unwrap_or(0.0), s["duration_s"].as_f64().unwrap_or(0.0));
                for m in [buckets.entry(label.clone()).or_default(), &mut totals] {
                    let e = m.entry(mode).or_default();
                    e.0 += dist;
                    e.1 += dur;
                    e.2 += 1;
                }
                if longest.as_ref().is_none_or(|l| l["distance_m"].as_f64().unwrap_or(0.0) < dist) {
                    let mut l = s.clone();
                    if let Some(o) = l.as_object_mut() {
                        o.remove("path");
                    }
                    longest = Some(l);
                }
            } else if s["kind"] == "stop" {
                let (lat, lon) = (s["lat"].as_f64().unwrap_or(0.0), s["lon"].as_f64().unwrap_or(0.0));
                let p = places.entry(place_key(lat, lon)).or_default();
                if p.visits == 0 {
                    p.first = s["start"].as_f64().unwrap_or(0.0);
                }
                p.time_s += s["duration_s"].as_f64().unwrap_or(0.0);
                p.visits += 1;
                p.lat_sum += lat;
                p.lon_sum += lon;
            }
        }
    }
    let modes = |m: &std::collections::BTreeMap<&'static str, (f64, f64, i64)>| -> Value {
        Value::Object(m.iter().map(|(k, (d, t, n))| ((*k).to_string(), json!({
            "distance_m": d.round(), "moving_s": t.round(), "trips": n,
            "avg_speed_mps": if *t > 0.0 { (d / t * 100.0).round() / 100.0 } else { 0.0 },
        }))).collect())
    };
    let mut ranked: Vec<((i64, i64), PlaceAcc)> = places.into_iter().collect();
    ranked.sort_by(|a, b| b.1.time_s.total_cmp(&a.1.time_s));
    let place_json = |k: &(i64, i64), p: &PlaceAcc, new: bool| json!({
        "id": format!("place_{}_{}", k.0, k.1), "lat": p.lat_sum / p.visits as f64, "lon": p.lon_sum / p.visits as f64,
        "time_s": p.time_s.round(), "visits": p.visits, "first_seen_in_range": p.first, "new": new,
    });
    // New = no cleaned fix inside the place's cell before the range began.
    let mut top = Vec::new();
    let mut new_places = Vec::new();
    for (k, p) in ranked.iter().take(50) {
        let (la0, lo0) = (k.0 as f64 * 0.00135, k.1 as f64 * 0.0018);
        let seen: bool = c.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM location_points WHERE ts < ?1 AND lat >= ?2 AND lat < ?3 AND lon >= ?4 AND lon < ?5 AND {CLEAN_SQL})"),
            params![from, la0, la0 + 0.00135, lo0, lo0 + 0.0018], |r| r.get(0))?;
        let j = place_json(k, p, !seen);
        if !seen {
            new_places.push(j.clone());
        }
        if top.len() < 10 {
            top.push(j);
        }
    }
    // Areas: places grouped on the ~5 km grid, ranked by visits, named from the
    // cache. A name the cache lacks reads "pending" and the stats handler
    // resolves it in the background; nothing here calls the geocoder.
    let mut areas: std::collections::HashMap<String, AreaAcc> = Default::default();
    for (_, p) in &ranked {
        let (lat, lon) = (p.lat_sum / p.visits as f64, p.lon_sum / p.visits as f64);
        let e = areas.entry(area_key(lat, lon)).or_default();
        e.0 += p.visits;
        e.1 += p.time_s;
        e.2 += lat * p.visits as f64;
        e.3 += lon * p.visits as f64;
        e.4 += p.visits;
    }
    let mut area_rows: Vec<(String, AreaAcc)> = areas.into_iter().collect();
    area_rows.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(b.1 .1.total_cmp(&a.1 .1)));
    let mut areas_json = Vec::new();
    for (key, (visits, time_s, lat_w, lon_w, w)) in &area_rows {
        let cached: Option<CachedAreaName> = c
            .query_row("SELECT name, status, fetched_at FROM location_place_names WHERE key = ?1", params![key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .ok();
        let (name, status) = match cached {
            Some((Some(n), st, _)) if st == "named" => (Some(n), "named"),
            Some((_, st, _)) if st == "none" => (None, "unnamed"),
            Some((_, _, at)) if now - at < AREA_RETRY_S => (None, "unavailable"),
            _ => (None, "pending"),
        };
        areas_json.push(json!({"key": key, "name": name, "name_status": status, "visits": visits,
            "time_s": time_s.round(), "lat": lat_w / *w as f64, "lon": lon_w / *w as f64}));
    }
    let areas_json = merge_named_areas(areas_json);
    let buckets_json: Vec<Value> = buckets.iter().map(|(label, m)| json!({"bucket": label, "modes": modes(m)})).collect();
    Ok(json!({
        "ok": true, "measured": true, "n_considered": n_points, "days_with_data": days.len(),
        "bucket": bucket, "from": from, "to": to,
        "totals": modes(&totals), "buckets": buckets_json, "top_places": top, "new_places": new_places,
        "places_considered": ranked.len(), "longest_trip": longest,
        "areas": areas_json, "areas_considered": area_rows.len(),
    }))
}

async fn stats(State(state): State<AppState>, Query(q): Query<StatsQ>) -> Response {
    let valid = q.from.is_finite() && q.to.is_finite() && q.to > q.from && q.to - q.from <= 3660.0 * 86_400.0;
    if !valid {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "measured": false, "n_considered": 0,
            "why_unmeasured": "from/to must be epoch seconds, to > from, at most 10 years"}))).into_response();
    }
    let bucket = match q.bucket.as_deref() { Some("week") => "week", Some("month") => "month", _ => "day" };
    let off_s = q.tz_offset_min.unwrap_or(0.0).clamp(-14.0 * 60.0, 14.0 * 60.0) * 60.0;
    let (from, to) = (q.from, q.to);
    let t = now();
    match state.store.read_async(move |c| compute_stats(c, from, to, bucket, off_s, t)).await {
        Ok(v) => {
            let pending: Vec<(String, f64, f64)> = v["areas"].as_array().map(|a| a.iter()
                .filter(|x| x["name_status"] == "pending")
                .filter_map(|x| Some((x["key"].as_str()?.to_string(), x["lat"].as_f64()?, x["lon"].as_f64()?)))
                .take(AREA_NAMES_PER_CALL).collect()).unwrap_or_default();
            if !pending.is_empty() {
                spawn_area_names(state.clone(), pending);
            }
            Json(v).into_response()
        }
        Err(e) => server_error(e),
    }
}

const AREA_NAMES_PER_CALL: usize = 20;

/// Keys being resolved right now, so overlapping stats calls do not ask twice.
fn area_names_in_flight() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// Name areas in the background: one Nominatim reverse lookup per area, about
/// one a second (its usage policy), each cached in location_place_names.
/// AMUX_REVERSE_GEOCODE=0 turns it off; AMUX_NOMINATIM_URL points it elsewhere.
fn spawn_area_names(state: AppState, wanted: Vec<(String, f64, f64)>) {
    if std::env::var("AMUX_REVERSE_GEOCODE").as_deref() == Ok("0") {
        return;
    }
    let mine: Vec<(String, f64, f64)> = {
        let mut set = area_names_in_flight().lock().unwrap_or_else(|e| e.into_inner());
        wanted.into_iter().filter(|(k, _, _)| set.insert(k.clone())).collect()
    };
    if mine.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let base = std::env::var("AMUX_NOMINATIM_URL").unwrap_or_else(|_| "https://nominatim.openstreetmap.org".into());
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(8)).build();
        let (mut named, mut failed) = (0usize, 0usize);
        for (i, (key, lat, lon)) in mine.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            }
            let got: Option<Value> = match (&client, area_lookup_url(&base, key)) {
                (Ok(cl), Some(url)) => match cl.get(&url).header("User-Agent", "amux/1.0").header("Accept-Language", "en").send().await {
                    Ok(r) if r.status().is_success() => r.json().await.ok(),
                    _ => None,
                },
                _ => None,
            };
            let (name, status) = match &got {
                Some(v) => match area_name(v) { Some(n) => (Some(n), "named"), None => (None, "none") },
                None => (None, "failed"),
            };
            if status == "failed" { failed += 1 } else { named += 1 }
            let _ = (lat, lon); // the visit-weighted centre never leaves this process
            let (k, t) = (key.clone(), now());
            let (la, lo) = area_lookup_url("", key).and_then(|u| {
                let q = u.split('?').nth(1)?.to_string();
                let get = |n: &str| q.split('&').find_map(|kv| kv.strip_prefix(n)).and_then(|v| v.parse::<f64>().ok());
                Some((get("lat=")?, get("lon=")?))
            }).unwrap_or((0.0, 0.0));
            let res = write_value(&state, move |c| c.execute(
                "INSERT INTO location_place_names (key, name, status, lat, lon, fetched_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(key) DO UPDATE SET name = excluded.name, status = excluded.status, lat = excluded.lat,
                   lon = excluded.lon, fetched_at = excluded.fetched_at",
                params![k, name, status, la, lo, t])).await;
            if let Err(e) = res {
                tracing::warn!(verdict = "location_area_name_store_failed", key = %key, error = %e, "area name not cached");
            }
        }
        {
            let mut set = area_names_in_flight().lock().unwrap_or_else(|e| e.into_inner());
            for (k, _, _) in &mine {
                set.remove(k);
            }
        }
        if failed > 0 {
            tracing::warn!(verdict = "location_area_names_partial", measured = true, n_considered = mine.len(), named, failed,
                "some areas could not be named; retried after a day");
        } else {
            tracing::info!(verdict = "location_area_names_resolved", measured = true, n_considered = mine.len(), named,
                "areas named for Top Places");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(id: &str, ts: f64, lat: f64, lon: f64, speed: Option<f64>, act: Option<&str>) -> Pt {
        Pt { id: id.into(), ts, lat, lon, speed, activity: act.map(str::to_string) }
    }
    fn inp(id: &str, ts: f64) -> InPoint {
        InPoint { id: id.into(), ts, lat: 40.7, lon: -74.0, alt: None, h_acc: Some(5.0), v_acc: None,
            speed: Some(-1.0), course: None, activity: Some("walking".into()), activity_conf: Some("high".into()), source: None,
            ell_alt: None, speed_acc: None, course_acc: None, floor: None, simulated: None, accessory: None, age_s: None }
    }

    #[test]
    fn raw_rows_keep_every_field_and_the_cleaned_view_skips_bad_fixes() {
        let c = crate::db::migrate::test_memdb();
        let t = 1_790_000_000.0;
        let good = InPoint { ell_alt: Some(-30.0), speed_acc: Some(0.5), course_acc: Some(3.0), floor: Some(2),
            simulated: Some(false), accessory: Some(false), age_s: Some(0.2), speed: Some(1.2), ..inp("g", t) };
        let poor = InPoint { h_acc: Some(450.0), ..inp("poor", t + 1.0) };
        let invalid = InPoint { h_acc: Some(-1.0), ..inp("inv", t + 2.0) };
        let stale = InPoint { age_s: Some(120.0), ..inp("stale", t + 3.0) };
        let sim = InPoint { simulated: Some(true), ..inp("sim", t + 4.0) };
        let (acc, _, rej) = db_ingest_points(&c, "iphone", &[good, poor, invalid, stale, sim], t + 10.0).unwrap();
        // RAW: all five are stored, the -1 "invalid" accuracy included.
        assert_eq!((acc, rej.len()), (5, 0), "{rej:?}");
        let row = c.query_row(&format!("SELECT {RAW_COLS} FROM location_points WHERE id='g'"), [], raw_row).unwrap();
        assert_eq!(row["ell_alt"], -30.0);
        assert_eq!(row["floor"], 2);
        assert_eq!(row["simulated"], false);
        assert_eq!(row["speed_acc"], 0.5);
        // The cleaned view (map, timeline, stats) sees only the good fix.
        let clean = load_points(&c, t - 1.0, t + 100.0).unwrap();
        assert_eq!(clean.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), vec!["g"]);
        assert_eq!(count_raw(&c, t - 1.0, t + 100.0).unwrap(), 5);
    }

    #[test]
    fn motion_stream_is_stored_raw_and_idempotent() {
        let c = crate::db::migrate::test_memdb();
        let t = 1_790_000_000.0;
        let m = |id: &str, ts: f64| InMotion { id: id.into(), ts, stationary: false, walking: true, running: false,
            cycling: false, automotive: false, unknown: false, confidence: "high".into() };
        assert_eq!(db_ingest_motion(&c, "iphone", &[m("m1", t), m("m2", t + 5.0)], t + 10.0).unwrap(), (2, 0, 0));
        assert_eq!(db_ingest_motion(&c, "iphone", &[m("m1", t)], t + 20.0).unwrap(), (0, 1, 0));
    }

    #[test]
    fn exports_carry_every_raw_field_in_each_format() {
        let rows = vec![json!({"id": "a", "device": "d", "ts": 1790000000.5, "lat": 40.7, "lon": -74.0, "alt": 12.0,
            "ell_alt": -20.0, "h_acc": 5.0, "v_acc": 3.0, "speed": 1.4, "speed_acc": 0.3, "course": 90.0, "course_acc": 5.0,
            "floor": 1, "simulated": false, "accessory": false, "age_s": 0.1, "activity": "walking", "activity_conf": "high",
            "source": "live", "received": 1790000001.0})];
        let (csv, _, _) = render_export(&rows, "csv");
        assert!(csv.starts_with(&RAW_NAMES.join(",")));
        assert_eq!(csv.lines().nth(1).unwrap().split(',').count(), RAW_NAMES.len());
        let (gpx, _, _) = render_export(&rows, "gpx");
        assert!(gpx.contains("<trkpt lat=\"40.7\" lon=\"-74.0\">") && gpx.contains("<amux:speed_acc>0.3</amux:speed_acc>"), "{gpx}");
        assert!(gpx.contains("2026-09-21T") , "{gpx}");
        let (geo, _, _) = render_export(&rows, "geojson");
        let g: Value = serde_json::from_str(&geo).unwrap();
        assert_eq!(g["features"][0]["geometry"]["coordinates"], json!([-74.0, 40.7, 12.0]));
        assert_eq!(g["features"][0]["properties"]["course_acc"], 5.0);
    }

    #[test]
    fn stats_count_distance_per_mode_places_new_places_and_the_longest_trip() {
        let c = crate::db::migrate::test_memdb();
        let t = 1_790_049_600.0; // a midday, UTC
        let mut pts = vec![
            InPoint { activity: Some("stationary".into()), speed: Some(0.0), ..inp("h1", t) },
            InPoint { activity: Some("stationary".into()), speed: Some(0.0), lat: 40.70002, ..inp("h2", t + 1200.0) },
        ];
        for k in 1..=90 {
            pts.push(InPoint { lat: 40.7 + k as f64 * 0.000126, speed: Some(1.4), ..inp(&format!("w{k}"), t + 1200.0 + k as f64 * 10.0) });
        }
        db_ingest_points(&c, "iphone", &pts, t + 5000.0).unwrap();
        let v = compute_stats(&c, t - 3600.0, t + 7200.0, "day", 0.0, t + 5000.0).unwrap();
        assert_eq!(v["measured"], true);
        assert_eq!(v["n_considered"], 92);
        let walk = &v["totals"]["walking"];
        assert!((1100.0..1400.0).contains(&walk["distance_m"].as_f64().unwrap()), "{v}");
        assert!(walk["avg_speed_mps"].as_f64().unwrap() > 1.0);
        assert_eq!(v["longest_trip"]["mode"], "walking");
        assert_eq!(v["top_places"].as_array().unwrap().len(), 1);
        // Nothing existed before the range, so the place is new.
        assert_eq!(v["new_places"].as_array().unwrap().len(), 1);
        // Asked again for a later range only, the same place is no longer new.
        let later = compute_stats(&c, t + 1000.0, t + 7200.0, "day", 0.0, t + 5000.0).unwrap();
        assert!(later["new_places"].as_array().unwrap().iter().all(|p| p["id"] != v["new_places"][0]["id"]), "{later}");
    }

    #[test]
    fn area_names_read_like_city_and_region() {
        let ny = json!({"address": {"city": "New York", "state": "New York", "ISO3166-2-lvl4": "US-NY", "country_code": "us"}});
        assert_eq!(area_name(&ny).as_deref(), Some("New York, NY"));
        let bath = json!({"address": {"town": "Bath", "state": "England", "country_code": "gb"}});
        assert_eq!(area_name(&bath).as_deref(), Some("Bath, England"));
        let paris = json!({"address": {"city": "Paris", "country": "France", "country_code": "fr"}});
        assert_eq!(area_name(&paris).as_deref(), Some("Paris, France"));
        assert_eq!(area_name(&json!({"address": {"road": "I-87"}})), None);
        assert_eq!(area_name(&json!({"error": "Unable to geocode"})), None);
    }

    #[test]
    fn per_fix_mode_uses_motion_first_then_speed() {
        let c = crate::db::migrate::test_memdb();
        let t = 1_790_000_000.0;
        let pts = vec![
            InPoint { activity: Some("automotive".into()), speed: Some(1.0), ..inp("auto", t) },
            InPoint { activity: Some("cycling".into()), ..inp("bike", t + 1.0) },
            InPoint { activity: None, speed: Some(12.0), ..inp("fast", t + 2.0) },
            InPoint { activity: None, speed: Some(4.0), ..inp("mid", t + 3.0) },
            InPoint { activity: None, speed: Some(1.3), ..inp("slow", t + 4.0) },
            InPoint { activity: Some("stationary".into()), speed: Some(-1.0), ..inp("still", t + 5.0) },
        ];
        db_ingest_points(&c, "iphone", &pts, t + 10.0).unwrap();
        let mut stmt = c.prepare(&format!("SELECT id, {MODE_SQL} FROM location_points ORDER BY ts")).unwrap();
        let got: Vec<(String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(Result::unwrap).collect();
        let want = [("auto", "driving"), ("bike", "cycling"), ("fast", "driving"), ("mid", "cycling"), ("slow", "walking"), ("still", "still")];
        assert_eq!(got, want.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect::<Vec<_>>());
    }

    #[test]
    fn areas_group_stops_into_named_areas_ranked_by_visits() {
        let c = crate::db::migrate::test_memdb();
        let t = 1_790_049_600.0;
        let stay = |id: &str, lat: f64, lon: f64, at: f64| vec![
            InPoint { activity: Some("stationary".into()), speed: Some(0.0), lat, lon, ..inp(&format!("{id}1"), at) },
            InPoint { activity: Some("stationary".into()), speed: Some(0.0), lat: lat + 0.00002, lon, ..inp(&format!("{id}2"), at + 1200.0) },
        ];
        // Area A (two places ~1 km apart, three stays) and area B (one stay).
        let mut pts = Vec::new();
        pts.extend(stay("a", 40.712, -73.99, t));
        pts.extend(stay("b", 40.721, -73.99, t + 4000.0));
        pts.extend(stay("c", 40.712, -73.99, t + 8000.0));
        pts.extend(stay("d", 41.512, -74.5, t + 12000.0));
        db_ingest_points(&c, "iphone", &pts, t + 20000.0).unwrap();
        let ka = area_key(40.712, -73.99);
        let kb = area_key(41.512, -74.5);
        assert_eq!(ka, area_key(40.721, -73.99), "both A places share one area");
        c.execute("INSERT INTO location_place_names (key, name, status, lat, lon, fetched_at) VALUES (?1, 'New York, NY', 'named', 40.7, -74.0, ?2)",
            params![ka, t]).unwrap();
        let v = compute_stats(&c, t - 3600.0, t + 20000.0, "day", 0.0, t + 20000.0).unwrap();
        let areas = v["areas"].as_array().unwrap();
        assert_eq!(areas.len(), 2, "{v}");
        assert_eq!((areas[0]["name"].as_str(), areas[0]["visits"].as_i64(), areas[0]["name_status"].as_str()),
            (Some("New York, NY"), Some(3), Some("named")), "{v}");
        assert_eq!((areas[1]["key"].as_str(), areas[1]["name_status"].as_str()), (Some(kb.as_str()), Some("pending")));
        // A recent failed lookup reads "unavailable"; an old one is retried ("pending").
        c.execute("INSERT INTO location_place_names (key, name, status, lat, lon, fetched_at) VALUES (?1, NULL, 'failed', 0, 0, ?2)",
            params![kb, t + 19000.0]).unwrap();
        let v = compute_stats(&c, t - 3600.0, t + 20000.0, "day", 0.0, t + 20000.0).unwrap();
        assert_eq!(v["areas"][1]["name_status"], "unavailable");
        let v = compute_stats(&c, t - 3600.0, t + 20000.0, "day", 0.0, t + 19000.0 + AREA_RETRY_S + 1.0).unwrap();
        assert_eq!(v["areas"][1]["name_status"], "pending");
        assert_eq!(v["areas_considered"], 2);
    }

    #[test]
    fn the_geocoder_only_ever_sees_the_cell_centre_at_two_decimals() {
        // A stop at a precise home-like position...
        let (lat, lon) = (40.741_123_4, -73.989_765_4);
        let key = area_key(lat, lon);
        let url = area_lookup_url("https://geo.example", &key).unwrap();
        let q: std::collections::HashMap<&str, &str> = url.split('?').nth(1).unwrap().split('&')
            .filter_map(|kv| kv.split_once('=')).collect();
        for k in ["lat", "lon"] {
            let v = q[k];
            let decimals = v.split('.').nth(1).map_or(0, str::len);
            assert!(decimals <= 2, "{k}={v} carries more than 2 decimals: {url}");
        }
        // ...is sent as the centre of its 0.05 degree cell, not as itself.
        let (i, j) = ((lat / 0.05).floor(), (lon / 0.05).floor());
        assert_eq!(q["lat"], format!("{:.2}", (i + 0.5) * 0.05));
        assert_eq!(q["lon"], format!("{:.2}", (j + 0.5) * 0.05));
        assert_ne!(q["lat"], format!("{lat:.2}"), "the request must not be the stop rounded, but the cell centre");
        assert_eq!(q["zoom"], "10");
        assert_eq!(area_lookup_url("x", "a_1_2"), None, "old-style keys are never looked up");
    }

    #[test]
    fn cells_with_the_same_name_merge_into_one_place() {
        let a = |key: &str, name: Option<&str>, visits: i64, lat: f64| json!({"key": key, "name": name,
            "name_status": if name.is_some() { "named" } else { "pending" }, "visits": visits, "time_s": 10.0, "lat": lat, "lon": -74.0});
        let merged = merge_named_areas(vec![
            a("b_1", Some("New York, NY"), 30, 40.70), a("b_2", Some("New York, NY"), 10, 40.80),
            a("b_3", None, 25, 41.5), a("b_4", None, 5, 41.6), a("b_5", Some("Boston, MA"), 20, 42.3),
        ]);
        let names: Vec<(Option<&str>, i64)> = merged.iter().map(|x| (x["name"].as_str(), x["visits"].as_i64().unwrap())).collect();
        assert_eq!(names, vec![(Some("New York, NY"), 40), (None, 25), (Some("Boston, MA"), 20), (None, 5)]);
        assert_eq!(merged[0]["key"], "b_1");
        assert!((merged[0]["lat"].as_f64().unwrap() - 40.725).abs() < 1e-9, "{}", merged[0]);
        assert_eq!(merged[0]["time_s"], 20.0);
    }

    #[test]
    fn ingest_is_idempotent_and_rejects_bad_points_with_a_reason() {
        let c = crate::db::migrate::test_memdb();
        let t = 1_790_000_000.0;
        let batch = vec![inp("a", t), inp("b", t + 5.0), InPoint { lat: 95.0, ..inp("bad", t) }];
        let (acc, dup, rej) = db_ingest_points(&c, "iphone", &batch, t + 10.0).unwrap();
        assert_eq!((acc, dup, rej.len()), (2, 0, 1));
        assert_eq!(rej[0].1, "lat/lon out of range");
        // A retried upload after a lost response stores nothing new.
        let (acc, dup, _) = db_ingest_points(&c, "iphone", &batch[..2], t + 20.0).unwrap();
        assert_eq!((acc, dup), (0, 2));
        let n: i64 = c.query_row("SELECT COUNT(*) FROM location_points", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        // RAW: Core Location's -1 "invalid speed" is stored exactly as delivered.
        let s: Option<f64> = c.query_row("SELECT speed FROM location_points WHERE id='a'", [], |r| r.get(0)).unwrap();
        assert_eq!(s, Some(-1.0));
    }

    #[test]
    fn a_still_phone_with_sparse_fixes_is_a_stop_and_movement_between_is_a_trip() {
        let t = 1_790_000_000.0;
        let mut pts = vec![
            // Home: two fixes 20 minutes apart (live updates go quiet when still).
            pt("h1", t, 40.7000, -74.0000, Some(0.0), Some("stationary")),
            pt("h2", t + 1200.0, 40.70002, -74.00001, Some(0.0), Some("stationary")),
        ];
        // Walk ~1.2 km north at 1.4 m/s, a fix every 10 s.
        for k in 1..=90 {
            pts.push(pt(&format!("w{k}"), t + 1200.0 + k as f64 * 10.0, 40.7000 + k as f64 * 0.000126, -74.0, Some(1.4), Some("walking")));
        }
        let segs = segment(&pts, &[], t + 5000.0);
        let kinds: Vec<&str> = segs.iter().map(|s| s["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["stop", "trip"], "{segs:#?}");
        assert_eq!(segs[0]["id"], "stop_h1");
        assert_eq!(segs[1]["id"], "trip_w1");
        assert_eq!(segs[1]["mode"], "walking");
        let d = segs[1]["distance_m"].as_f64().unwrap();
        assert!((1100.0..1400.0).contains(&d), "walk distance {d}");
    }

    #[test]
    fn a_straight_fast_automotive_trip_with_station_dwells_is_an_inferred_train() {
        let t = 1_790_000_000.0;
        let mut pts = Vec::new();
        let mut lat = 40.70;
        let mut ts = t;
        let mut k = 0;
        // Two legs at ~22 m/s due north with a 60 s station stop between them.
        for leg in 0..2 {
            for _ in 0..60 {
                k += 1;
                pts.push(pt(&format!("p{k}"), ts, lat, -74.0, Some(22.0), Some("automotive")));
                ts += 10.0;
                lat += 0.00198; // ~220 m
            }
            if leg == 0 {
                for _ in 0..6 {
                    k += 1;
                    pts.push(pt(&format!("p{k}"), ts, lat, -74.0, Some(0.0), Some("automotive")));
                    ts += 10.0;
                }
            }
        }
        let segs = segment(&pts, &[], ts + 10.0);
        let trip = segs.iter().find(|s| s["kind"] == "trip").expect("one trip");
        assert_eq!(trip["mode"], "train", "{trip}");
        assert_eq!(trip["mode_confidence"], "inferred");
        // The same path with no station dwell stays driving: the dwell is load-bearing.
        let no_dwell: Vec<Pt> = pts.iter().filter(|p| p.speed != Some(0.0)).cloned().collect();
        let segs = segment(&no_dwell, &[], ts + 10.0);
        let trip = segs.iter().find(|s| s["kind"] == "trip").unwrap();
        assert_eq!(trip["mode"], "driving");
    }

    #[test]
    fn an_ios_visit_becomes_a_stop_and_its_points_are_not_a_trip() {
        let t = 1_790_000_000.0;
        let pts = vec![
            pt("x1", t + 10.0, 40.7, -74.0, Some(0.2), None),
            pt("x2", t + 20.0, 40.7001, -74.0, Some(0.2), None),
        ];
        let visits = vec![Visit { id: "v1".into(), arrival: t, departure: Some(t + 100.0), lat: 40.7, lon: -74.0, open: false }];
        let segs = segment(&pts, &visits, t + 1000.0);
        assert_eq!(segs.len(), 1, "{segs:#?}");
        assert_eq!(segs[0]["id"], "stop_visit_v1");
    }

    #[test]
    fn an_arrival_report_that_never_closed_does_not_swallow_the_next_day() {
        // The stored shape BEFORE this fix: each visit as two rows, the open
        // arrival report and the closed departure report, ids differing only
        // in their coordinates; plus one open row with no twin, then a later
        // day's visits. Times are 2026-10-03/04 as seen live.
        let c = crate::db::migrate::test_memdb();
        let day1 = 1_791_043_856.0; // 10-03 12:10:56
        let day2 = 1_791_125_146.0; // 10-04 10:45:46
        let v = |id: &str, a: f64, d: Option<f64>, lat: f64, lon: f64| InVisit {
            id: id.into(), arrival: a, departure: d, lat, lon, h_acc: None,
        };
        let rows = vec![
            v("a-open", day1, None, 40.73475, -74.00245),
            v("a-closed", day1, Some(day1 + 7452.0), 40.73492, -74.00258),
            v("b-open-no-twin", day1 + 20_000.0, None, 40.7376, -74.0078),
            v("b2-closed", day1 + 30_000.0, Some(day1 + 31_000.0), 40.7339, -74.0045),
            v("c-open", day2, None, 40.73748, -74.00826),
            v("c-closed", day2, Some(day2 + 2666.0), 40.73719, -74.00835),
            v("d-open-current", day2 + 5000.0, None, 40.7360, -74.0049),
        ];
        db_ingest_visits(&c, "phone", &rows, day2 + 6000.0).unwrap();
        // The second day only.
        let (from, to) = (day2 - 10_000.0, day2 + 76_400.0);
        let vs = load_visits(&c, from, to).unwrap();
        let ids: Vec<&str> = vs.iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, vec!["c-closed", "d-open-current"], "{vs:#?}");
        let segs = segment(&[], &vs, day2 + 6000.0);
        let stops: Vec<&str> = segs.iter().filter(|s| s["kind"] == "stop").map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(stops, vec!["stop_visit_c-closed", "stop_visit_d-open-current"], "{segs:#?}");
        // An open row followed directly by another visit: both are stops, the
        // second is not swallowed because the first now ends where it begins.
        let touching = vec![
            v("e-open", day2 + 20_000.0, None, 40.7377, -74.0018),
            v("f-closed", day2 + 22_000.0, Some(day2 + 24_000.0), 40.7372, -74.0084),
        ];
        db_ingest_visits(&c, "phone", &touching, day2 + 25_000.0).unwrap();
        let vs = load_visits(&c, day2 + 19_000.0, day2 + 25_000.0).unwrap();
        let segs = segment(&[], &vs, day2 + 25_000.0);
        let stops: Vec<&str> = segs.iter().filter(|s| s["kind"] == "stop").map(|s| s["id"].as_str().unwrap()).collect();
        // d (open) now ends at e's arrival, so it reaches into this window too.
        assert_eq!(stops, vec!["stop_visit_d-open-current", "stop_visit_e-open", "stop_visit_f-closed"], "{segs:#?}");
        // The gap between e and f (they touch, 600 m apart) sits between them.
        let kinds: Vec<&str> = segs.iter().map(|s| s["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["stop", "gap", "stop", "gap", "stop"], "{segs:#?}");
        // The open row with no twin ends at the next visit, so on its own day
        // it is a bounded stop, not one running to now.
        let vs1 = load_visits(&c, day1 - 1000.0, day1 + 40_000.0).unwrap();
        let b = vs1.iter().find(|v| v.id == "b-open-no-twin").expect("open row kept");
        assert_eq!(b.departure, Some(day1 + 30_000.0), "{vs1:#?}");
        assert!(!vs1.iter().any(|v| v.id == "a-open"), "an arrival report with a closed twin is dropped: {vs1:#?}");
    }

    #[test]
    fn the_app_build_is_read_from_the_user_agent() {
        let mut h = HeaderMap::new();
        h.insert(axum::http::header::USER_AGENT, "AmuxApp/6925 CFNetwork/3860.700.1 Darwin/25.6.0".parse().unwrap());
        assert_eq!(app_build(&h), Some(6925));
        assert!(app_build(&h).unwrap() < LIVE_RESTART_BUILD, "the 2026-10-05 phone lacked the fix");
        h.insert(axum::http::header::USER_AGENT, "Python-urllib/3.11".parse().unwrap());
        assert_eq!(app_build(&h), None);
    }

    #[test]
    fn two_stops_at_different_places_with_no_fixes_between_have_an_unrecorded_gap() {
        // The 2026-10-03 shape: live fixes stop, then only iOS visits arrive.
        let t = 1_790_000_000.0;
        let visits = vec![
            Visit { id: "a".into(), arrival: t, departure: Some(t + 3600.0), lat: 40.73492, lon: -74.00258, open: false },
            Visit { id: "b".into(), arrival: t + 3601.0, departure: Some(t + 4300.0), lat: 40.73852, lon: -74.00286, open: false },
            // Same place as b: no move, so no gap.
            Visit { id: "c".into(), arrival: t + 5000.0, departure: Some(t + 6000.0), lat: 40.73853, lon: -74.00287, open: false },
        ];
        let segs = segment(&[], &visits, t + 7000.0);
        let kinds: Vec<&str> = segs.iter().map(|s| s["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["stop", "gap", "stop", "stop"], "{segs:#?}");
        let d = segs[1]["distance_m"].as_f64().unwrap();
        assert!((350.0..450.0).contains(&d), "straight-line gap distance {d}");
        assert_eq!(segs[1]["start"], json!(t + 3600.0));
    }

    #[test]
    fn an_unrecorded_move_takes_its_time_from_motion_and_an_untimed_one_says_so() {
        // 2026-10-05, Ethan: "inaccurate". iOS closed the overnight visit at
        // 07:48:20 and opened the next one 0.3 mi away at 07:48:21, so the day
        // read "Moved · 0 min". The motion sensor had running from 07:47:41
        // and walking until 07:53:02. The 08:06 move had no motion at all.
        let t = 1_791_200_000.0; // 07:33:20 local on the day
        let v = |id: &str, a: f64, d: Option<f64>, lat: f64, lon: f64, open: bool| Visit {
            id: id.into(), arrival: t + a, departure: d.map(|d| t + d), lat, lon, open,
        };
        let visits = vec![
            v("home", -42_000.0, Some(900.0), 40.73730, -74.00850, false),
            v("cafe", 901.0, Some(2001.0), 40.73325, -74.01056, true), // open: ended by the next arrival
            v("home2", 2001.0, Some(8940.0), 40.73729, -74.00876, false),
        ];
        let m = |a: f64, mode: Option<&'static str>| Motion { ts: t + a, mode };
        let motion = vec![
            m(-2000.0, None),
            m(861.0, Some("running")),
            m(998.0, Some("walking")),
            m(1182.0, None),
            m(2925.0, None),
        ];
        let segs = segment_with_motion(&[], &visits, &motion, t + 9000.0);
        let kinds: Vec<&str> = segs.iter().map(|s| s["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds, vec!["stop", "gap", "stop", "gap", "stop"], "{segs:#?}");
        let first_move = &segs[1];
        assert_eq!(first_move["timed_by"], json!("motion"), "{first_move:#?}");
        assert_eq!(first_move["start"], json!(t + 861.0), "the move starts when running starts");
        assert_eq!(first_move["end"], json!(t + 1182.0), "and ends when walking ends");
        assert_eq!(first_move["mode"], json!("walking"), "184 s walking outweighs 137 s running");
        assert_eq!(segs[0]["end"], json!(t + 861.0), "the stop before shrinks to the move");
        assert_eq!(segs[2]["start"], json!(t + 1182.0), "the stop after starts on arrival");
        assert_eq!(segs[2]["end_inferred"], json!(true), "the phone never reported leaving the cafe");
        assert_eq!(segs[0]["end_inferred"], json!(false));
        // The second move: no motion near it, so its duration is unknown, not 0.
        assert_eq!(segs[3]["duration_known"], json!(false), "{:#?}", segs[3]);
        assert!(segs[3].get("timed_by").is_none());
    }

    #[test]
    fn a_motion_run_is_used_once_and_never_swallows_a_whole_stop() {
        let t = 1_791_200_000.0;
        let visits = vec![
            Visit { id: "a".into(), arrival: t, departure: Some(t + 1000.0), lat: 40.70, lon: -74.00, open: false },
            Visit { id: "b".into(), arrival: t + 1001.0, departure: Some(t + 1300.0), lat: 40.71, lon: -74.00, open: false },
            Visit { id: "c".into(), arrival: t + 1301.0, departure: Some(t + 5000.0), lat: 40.72, lon: -74.00, open: false },
        ];
        // One long walk that straddles all of stop b.
        let motion = vec![Motion { ts: t + 900.0, mode: Some("walking") }, Motion { ts: t + 1500.0, mode: None }];
        let segs = segment_with_motion(&[], &visits, &motion, t + 6000.0);
        let f = |i: usize, k: &str| segs[i][k].as_f64().unwrap();
        for i in 0..segs.len() {
            if segs[i]["kind"] == "stop" {
                assert!(f(i, "duration_s") >= 59.0, "stop {i} kept a minute: {segs:#?}");
            }
            if i + 1 < segs.len() {
                assert!(f(i, "end") <= f(i + 1, "start") + 0.001, "segments overlap at {i}: {segs:#?}");
            }
        }
    }

    #[test]
    fn recorded_fixes_between_stops_are_never_a_gap_and_a_seconds_long_stub_is_not_a_trip() {
        let t = 1_790_000_000.0;
        let mut pts = Vec::new();
        // Stop A, ten minutes still.
        for k in 0..20 {
            pts.push(pt(&format!("a{k}"), t + k as f64 * 30.0, 40.7000, -74.0000, Some(0.0), None));
        }
        // Seven fixes in seven seconds that shuffle 10 m: the anchor rule handing over.
        for k in 0..7 {
            pts.push(pt(&format!("s{k}"), t + 600.0 + k as f64, 40.70005 + k as f64 * 0.00001, -74.0, Some(1.5), None));
        }
        // Stop B 150 m north, ten minutes still.
        for k in 0..20 {
            pts.push(pt(&format!("b{k}"), t + 610.0 + k as f64 * 30.0, 40.70135, -74.0000, Some(0.0), None));
        }
        let segs = segment(&pts, &[], t + 2000.0);
        let kinds: Vec<&str> = segs.iter().map(|s| s["kind"].as_str().unwrap()).collect();
        assert!(!kinds.contains(&"gap"), "recorded fixes sit between the stops: {segs:#?}");
        assert!(!kinds.contains(&"trip"), "a 7 s, 10 m shuffle is not a trip: {segs:#?}");
    }
}
