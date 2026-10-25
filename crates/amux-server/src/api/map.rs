//! /api/map — the Map tab: pins, tags, saved lists, place search
//! (AMUX-2586 fix #6, ported from amux-server.py:66470-66640).
//!
//! Pins live in SQLite since 2026-10-20 (map_store.rs, migration 0128), one
//! row per pin with an edit history. ~/.amux/map.json is imported once on the
//! first read and kept as the backup. GET /api/map still answers the whole
//! document the SPA has always loaded, and the old whole-document POST still
//! works (applied pin by pin), so a cached older dashboard keeps working.
//!
//! Places come from Google Places when GOOGLE_MAPS_API_KEY is set (search,
//! link, refresh); every Google call is counted in the response, because it
//! is billed.

use super::map_store::{self as ms, MapError};
use super::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_map).post(post_map))
        .route("/meta", put(put_meta))
        .route("/pins", post(post_pin))
        .route("/pins/bulk", post(post_bulk))
        .route("/pins/merge", post(post_merge))
        .route("/pins/{id}", put(put_pin).delete(delete_pin))
        .route("/pins/{id}/history", get(pin_history))
        .route("/duplicates", get(get_duplicates))
        .route("/visits/match", post(post_match_visits))
        .route("/places/link", post(post_places_link))
        .route("/places/refresh", post(post_places_refresh))
        .route("/lists/{id}/share", get(get_share).post(post_share).delete(delete_share))
        .route("/search", get(search))
        // Location history (AMUX-5458): owned by the Map, stored in SQLite.
        .merge(super::location::routes())
}

/// Read-only shared lists. Both paths sit under the auth layer's public
/// prefixes (/s/, /api/share/); the token is the only credential.
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/api/share/map/{token}", get(shared_json))
        .route("/s/map/{token}", get(shared_page))
}

fn map_path() -> std::path::PathBuf {
    let home = std::env::var("AMUX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux")
        });
    home.join("map.json")
}

fn actor(h: &HeaderMap) -> String {
    h.get("x-amux-session")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| format!("worker:{s}"))
        .unwrap_or_else(|| "dashboard".into())
}

/// Run a store operation in one write transaction. A refused operation (bad
/// input, conflict, not found) rolls back like a database error does.
async fn run<T, F>(state: &AppState, f: F) -> Result<T, MapError>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> ms::R<T> + Send + 'static,
{
    let slot: std::sync::Arc<std::sync::Mutex<Option<ms::R<T>>>> = Default::default();
    let s2 = slot.clone();
    let path = map_path();
    let res = state
        .store
        .write_async(move |c| {
            let out = ms::ensure_imported(c, &path).and_then(|imported| {
                if let Some(n) = imported {
                    tracing::info!(target: "amux::map", verdict = "map_imported", pins = n, measured = true,
                        n_considered = n, "map.json imported into the map tables (file kept as backup)");
                }
                f(c)
            });
            let failed = out.is_err();
            // The writer reports this as a failed mutation, so it carries the
            // refusal's own reason rather than a generic "refused".
            let db_err = match &out {
                Err(MapError::Db(e)) => Some(e.to_string()),
                Err(MapError::Bad(m) | MapError::NotFound(m)) => Some(format!("map: refused ({m}), rolled back")),
                Err(MapError::Conflict(_)) => Some("map: refused (conflict), rolled back".into()),
                Ok(_) => None,
            };
            *s2.lock().unwrap_or_else(|e| e.into_inner()) = Some(out);
            if failed {
                return Err(rusqlite::Error::InvalidParameterName(db_err.unwrap_or_else(|| "map: refused, rolled back".into())));
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    let out = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
    match (out, res) {
        (Some(r), _) => r,
        (None, Err(e)) => Err(MapError::Bad(format!("map store unavailable: {e}"))),
        (None, Ok(_)) => Err(MapError::Bad("map store produced no result".into())),
    }
}

fn err(e: MapError) -> Response {
    match e {
        MapError::Bad(m) => (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": m}))).into_response(),
        MapError::NotFound(m) => (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": m}))).into_response(),
        MapError::Conflict(v) => (StatusCode::CONFLICT, Json(v)).into_response(),
        MapError::Db(e) => {
            tracing::warn!(target: "amux::map", verdict = "map_store_failed", error = %e, "map store write failed");
            (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"ok": false, "error": e.to_string()}))).into_response()
        }
    }
}

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

async fn get_map(State(state): State<AppState>) -> Response {
    match run(&state, ms::doc).await {
        Ok(mut d) => {
            d["settings"]["googleMapsKey"] = json!(std::env::var("GOOGLE_MAPS_API_KEY").unwrap_or_default());
            ok(d)
        }
        Err(e) => err(e),
    }
}

#[derive(serde::Deserialize, Default)]
pub struct ReplaceParam {
    #[serde(default)]
    replace: Option<String>,
}

/// The old whole-document write (kept for older dashboards and scripts).
async fn post_map(State(state): State<AppState>, h: HeaderMap, Query(q): Query<ReplaceParam>, Json(body): Json<Value>) -> Response {
    let replace = matches!(q.replace.as_deref(), Some("1") | Some("true") | Some("yes"));
    let who = actor(&h);
    let mut body = body;
    if let Some(s) = body.get_mut("settings").and_then(Value::as_object_mut) {
        s.remove("googleMapsKey");
    }
    match run(&state, move |c| ms::replace_doc(c, &body, replace, &who)).await {
        Ok(v) => {
            calendar_changed(&state);
            ok(v)
        }
        Err(e) => err(e),
    }
}

/// Tags, settings, trials, saved lists and tag categories. Only keys present
/// in the body are written.
async fn put_meta(State(state): State<AppState>, Json(mut body): Json<Value>) -> Response {
    if let Some(s) = body.get_mut("settings").and_then(Value::as_object_mut) {
        s.remove("googleMapsKey");
    }
    match run(&state, move |c| {
        let mut wrote = vec![];
        for k in ms::META_KEYS {
            if let Some(v) = body.get(*k) {
                ms::set_meta(c, k, v)?;
                wrote.push(*k);
            }
        }
        Ok(json!({"ok": true, "wrote": wrote}))
    })
    .await
    {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

/// Add a pin (also the agents' and CLI's way in). Body is the pin or {pin}.
async fn post_pin(State(state): State<AppState>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    let pin = match body.get("pin") {
        Some(p) if p.is_object() => p.clone(),
        _ => body,
    };
    let who = actor(&h);
    match run(&state, move |c| {
        let (_, saved) = ms::upsert(c, &pin, &who, None)?;
        let total: i64 = c.query_row("SELECT count(*) FROM map_pins", [], |r| r.get(0))?;
        Ok(json!({"ok": true, "pin": saved, "total_pins": total}))
    })
    .await
    {
        Ok(v) => {
            calendar_changed(&state);
            ok(v)
        }
        Err(e) => err(e),
    }
}

/// Save one pin. Send the `rev` you loaded; a newer edit from another device
/// answers 409 with the current pin instead of being overwritten.
async fn put_pin(State(state): State<AppState>, h: HeaderMap, Path(id): Path<String>, Json(mut pin): Json<Value>) -> Response {
    let base = pin.get("rev").and_then(Value::as_i64);
    pin["id"] = json!(id);
    let who = actor(&h);
    match run(&state, move |c| {
        let (kind, saved) = ms::upsert(c, &pin, &who, base)?;
        Ok(json!({"ok": true, "result": format!("{kind:?}").to_lowercase(), "pin": saved}))
    })
    .await
    {
        Ok(v) => {
            calendar_changed(&state);
            ok(v)
        }
        Err(e) => err(e),
    }
}

async fn delete_pin(State(state): State<AppState>, h: HeaderMap, Path(id): Path<String>) -> Response {
    let who = actor(&h);
    match run(&state, move |c| Ok(json!({"ok": true, "deleted": ms::delete(c, &id, &who)?}))).await {
        Ok(v) => {
            calendar_changed(&state);
            ok(v)
        }
        Err(e) => err(e),
    }
}

async fn pin_history(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match run(&state, move |c| Ok(json!({"ok": true, "history": ms::history(c, &id)?}))).await {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

#[derive(serde::Deserialize)]
struct BulkBody {
    ids: Vec<String>,
    op: String,
    #[serde(default)]
    value: Value,
}

async fn post_bulk(State(state): State<AppState>, h: HeaderMap, Json(b): Json<BulkBody>) -> Response {
    let who = actor(&h);
    match run(&state, move |c| ms::bulk(c, &b.ids, &b.op, &b.value, &who)).await {
        Ok(v) => {
            tracing::info!(target: "amux::map", verdict = "map_bulk", op = %v["op"], changed = %v["changed"], "map bulk edit");
            calendar_changed(&state);
            ok(v)
        }
        Err(e) => err(e),
    }
}

#[derive(serde::Deserialize)]
struct MergeBody {
    keep: String,
    drop: String,
}

async fn post_merge(State(state): State<AppState>, h: HeaderMap, Json(b): Json<MergeBody>) -> Response {
    let who = actor(&h);
    match run(&state, move |c| Ok(json!({"ok": true, "pin": ms::merge(c, &b.keep, &b.drop, &who)?}))).await {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

async fn get_duplicates(State(state): State<AppState>) -> Response {
    match run(&state, |c| {
        let pairs = ms::duplicates(c)?;
        let n: i64 = c.query_row("SELECT count(*) FROM map_pins", [], |r| r.get(0))?;
        Ok(json!({"ok": true, "measured": true, "n_considered": n, "pairs": pairs}))
    })
    .await
    {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

/// Mark pins "been" from location history. Also runs after each visits upload.
async fn post_match_visits(State(state): State<AppState>) -> Response {
    match match_visits_now(&state).await {
        Ok(v) => ok(v),
        Err(e) => err(e),
    }
}

pub(crate) async fn match_visits_now(state: &AppState) -> Result<Value, MapError> {
    let v = run(state, |c| ms::match_visits(c, "location-history")).await?;
    tracing::info!(target: "amux::map", verdict = "map_visits_matched", updated = %v["updated"],
        marked_been = %v["marked_been"], visits = %v["visits_considered"], measured = true,
        n_considered = %v["n_considered"], "pins matched against location visits");
    Ok(v)
}

/// A dated pin writes the calendar table directly; publish the feed so the
/// subscribed calendars see it.
fn calendar_changed(state: &AppState) {
    super::calendar::publish_feed(state);
}

// ---------------------------------------------------------------- places

#[derive(serde::Deserialize, Default)]
struct PlacesBody {
    #[serde(default)]
    ids: Option<Vec<String>>,
    #[serde(default)]
    limit: Option<usize>,
}

fn places_key() -> Result<String, MapError> {
    std::env::var("GOOGLE_MAPS_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| MapError::Bad("GOOGLE_MAPS_API_KEY is not set, so pins cannot be linked to Google places".into()))
}

fn places_base() -> String {
    std::env::var("AMUX_GOOGLE_PLACES_BASE").unwrap_or_else(|_| "https://places.googleapis.com/v1".into())
}

const PLACE_FIELDS: &str = "id,displayName,location,businessStatus,rating,userRatingCount,regularOpeningHours.weekdayDescriptions,googleMapsUri";

/// Pick pins to work on: the ids given, or up to `limit` that need it.
async fn pick_pins(state: &AppState, ids: Option<Vec<String>>, limit: usize, linked: bool) -> Result<Vec<Value>, MapError> {
    run(state, move |c| {
        let mut pins = ms::all_pins(c)?;
        if let Some(ids) = ids {
            pins.retain(|p| ids.iter().any(|i| p["id"].as_str() == Some(i)));
        } else if linked {
            pins.retain(|p| p["placeId"].as_str().is_some_and(|s| !s.is_empty()));
            pins.sort_by(|a, b| a["place"]["refreshedAt"].as_str().unwrap_or("").cmp(b["place"]["refreshedAt"].as_str().unwrap_or("")));
        } else {
            pins.retain(|p| p["placeId"].as_str().is_none_or(str::is_empty) && p["placeLink"]["status"] != "no_match");
        }
        pins.truncate(limit);
        Ok(pins)
    })
    .await
}

async fn save_pins(state: &AppState, pins: Vec<Value>, who: &'static str) -> Result<usize, MapError> {
    run(state, move |c| {
        let mut n = 0;
        for p in &pins {
            if ms::upsert(c, p, who, None)?.0 == ms::Upsert::Updated {
                n += 1;
            }
        }
        Ok(n)
    })
    .await
}

/// Link pins to their Google place: a text search near the pin, accepted only
/// for a similar name within 150 m (or anything within 25 m). Default 10 pins
/// per call; each pin is one billed Text Search request.
async fn post_places_link(State(state): State<AppState>, Json(b): Json<PlacesBody>) -> Response {
    let key = match places_key() {
        Ok(k) => k,
        Err(e) => return err(e),
    };
    let pins = match pick_pins(&state, b.ids, b.limit.unwrap_or(10).min(200), false).await {
        Ok(p) => p,
        Err(e) => return err(e),
    };
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(8)).build().expect("client");
    let (mut out, mut calls, mut linked, mut no_match, mut failed) = (Vec::new(), 0, 0, 0, 0);
    for mut p in pins {
        let Some((lat, lng)) = ms::latlng(&p) else { continue };
        let body = json!({"textQuery": p["name"], "maxResultCount": 5,
            "locationBias": {"circle": {"center": {"latitude": lat, "longitude": lng}, "radius": 200.0}}});
        calls += 1;
        let resp = client
            .post(format!("{}/places:searchText", places_base()))
            .header("X-Goog-Api-Key", &key)
            .header("X-Goog-FieldMask", PLACE_FIELDS.split(',').map(|f| format!("places.{f}")).collect::<Vec<_>>().join(","))
            .json(&body)
            .send()
            .await;
        let data: Option<Value> = match resp {
            Ok(r) if r.status().is_success() => r.json().await.ok(),
            _ => None,
        };
        let Some(data) = data else {
            failed += 1;
            continue;
        };
        let cands = data["places"].as_array().cloned().unwrap_or_default();
        match ms::pick_candidate(&p, &cands) {
            Some(place) => {
                ms::apply_place(&mut p, place);
                if let Some(o) = p.as_object_mut() {
                    o.remove("placeLink");
                }
                linked += 1;
            }
            None => {
                p["placeLink"] = json!({"status": "no_match", "at": chrono::Utc::now().to_rfc3339(), "candidates": cands.len()});
                no_match += 1;
            }
        }
        out.push(p);
    }
    let saved = match save_pins(&state, out, "google-places").await {
        Ok(n) => n,
        Err(e) => return err(e),
    };
    tracing::info!(target: "amux::map", verdict = "map_places_linked", linked, no_match, failed, google_calls = calls,
        measured = true, n_considered = calls, "pins linked to Google places");
    ok(json!({"ok": true, "linked": linked, "no_match": no_match, "failed": failed, "saved": saved, "google_calls": calls}))
}

/// Refresh linked pins from Google (hours, rating, open or closed), oldest
/// refresh first. A permanently closed place turns its pin "closed". Each pin
/// is one billed Place Details request.
async fn post_places_refresh(State(state): State<AppState>, Json(b): Json<PlacesBody>) -> Response {
    let key = match places_key() {
        Ok(k) => k,
        Err(e) => return err(e),
    };
    let pins = match pick_pins(&state, b.ids, b.limit.unwrap_or(25).min(500), true).await {
        Ok(p) => p,
        Err(e) => return err(e),
    };
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(8)).build().expect("client");
    let (mut out, mut calls, mut failed, mut closed) = (Vec::new(), 0, 0, 0);
    for mut p in pins {
        let Some(id) = p["placeId"].as_str().map(str::to_string) else { continue };
        calls += 1;
        let resp = client
            .get(format!("{}/places/{id}", places_base()))
            .header("X-Goog-Api-Key", &key)
            .header("X-Goog-FieldMask", PLACE_FIELDS)
            .send()
            .await;
        let data: Option<Value> = match resp {
            Ok(r) if r.status().is_success() => r.json().await.ok(),
            _ => None,
        };
        let Some(place) = data else {
            failed += 1;
            continue;
        };
        let was = p["status"].clone();
        ms::apply_place(&mut p, &place);
        if p["status"] == "closed" && was != "closed" {
            closed += 1;
        }
        out.push(p);
    }
    let refreshed = out.len();
    let saved = match save_pins(&state, out, "google-places").await {
        Ok(n) => n,
        Err(e) => return err(e),
    };
    tracing::info!(target: "amux::map", verdict = "map_places_refreshed", refreshed, closed, failed, google_calls = calls,
        measured = true, n_considered = calls, "linked pins refreshed from Google");
    ok(json!({"ok": true, "refreshed": refreshed, "newly_closed": closed, "failed": failed, "saved": saved, "google_calls": calls}))
}

// ---------------------------------------------------------------- shares

fn share_url(token: &str) -> String {
    format!("/s/map/{token}")
}

async fn get_share(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match run(&state, move |c| ms::share_token_for(c, &id)).await {
        Ok(t) => ok(json!({"ok": true, "shared": t.is_some(), "url": t.as_deref().map(share_url)})),
        Err(e) => err(e),
    }
}

async fn post_share(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match run(&state, move |c| ms::share(c, &id)).await {
        Ok(t) => {
            tracing::info!(target: "amux::map", verdict = "map_list_shared", "a saved map list was shared read-only");
            ok(json!({"ok": true, "url": share_url(&t)}))
        }
        Err(e) => err(e),
    }
}

async fn delete_share(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match run(&state, move |c| ms::unshare(c, &id)).await {
        Ok(n) => ok(json!({"ok": true, "revoked": n})),
        Err(e) => err(e),
    }
}

async fn shared(state: &AppState, token: String) -> Option<Value> {
    if token.len() != 36 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    run(state, move |c| ms::shared_view(c, &token)).await.ok().flatten()
}

async fn shared_json(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    match shared(&state, token).await {
        Some(v) => ok(v),
        None => (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": "this link does not exist or was turned off"}))).into_response(),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

/// The shared list as a page anyone with the link can open: a map and a list.
async fn shared_page(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    let Some(v) = shared(&state, token).await else {
        return (StatusCode::NOT_FOUND, Html("<!doctype html><meta charset=utf-8><title>Not found</title><p style=\"font:16px system-ui;margin:3rem\">This link does not exist or was turned off.</p>".to_string())).into_response();
    };
    let name = esc(v["list"]["name"].as_str().unwrap_or("Shared places"));
    let mut items = String::new();
    for p in v["pins"].as_array().cloned().unwrap_or_default() {
        let tags: String = p["tags"].as_array().cloned().unwrap_or_default().iter()
            .map(|t| format!("<span class=t style=\"background:{}\">{}</span>", esc(t["color"].as_str().unwrap_or("#888")), esc(t["name"].as_str().unwrap_or(""))))
            .collect();
        let maps = p["mapsUri"].as_str().map(str::to_string).unwrap_or_else(|| {
            format!("https://www.google.com/maps/search/?api=1&query={},{}", p["lat"], p["lng"])
        });
        let rating = p["myRating"].as_i64().map(|r| format!(" · {}", "★".repeat(r as usize))).unwrap_or_default();
        items.push_str(&format!(
            "<li><b>{}</b>{}<div>{}</div><div class=d>{}</div><a href=\"{}\" target=_blank rel=noopener>Open in Google Maps</a></li>",
            esc(p["name"].as_str().unwrap_or("")), rating, tags, esc(p["desc"].as_str().unwrap_or("")), esc(&maps)
        ));
    }
    let data = serde_json::to_string(&v["pins"]).unwrap_or_else(|_| "[]".into()).replace("</", "<\\/");
    let n = v["pins"].as_array().map(Vec::len).unwrap_or(0);
    let page = format!(r#"<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content="width=device-width,initial-scale=1">
<meta name=robots content=noindex><title>{name}</title>
<link rel=stylesheet href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css">
<style>body{{margin:0;font:15px/1.45 system-ui,sans-serif;color:#1d1d1f}}header{{padding:14px 16px}}h1{{font-size:1.2rem;margin:0}}
#m{{height:45vh}}ul{{list-style:none;margin:0;padding:0 16px 24px}}li{{padding:12px 0;border-bottom:1px solid #eee}}
.t{{display:inline-block;color:#fff;border-radius:8px;padding:0 7px;font-size:12px;margin:3px 4px 0 0}}.d{{color:#666;font-size:13px}}
a{{color:#0b63ce;font-size:13px}}@media (prefers-color-scheme:dark){{body{{background:#111;color:#eee}}li{{border-color:#333}}.d{{color:#aaa}}a{{color:#8ab4ff}}}}</style></head>
<body><header><h1>{name}</h1><div class=d>{n} places · shared from amux</div></header><div id=m></div><ul>{items}</ul>
<script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js"></script>
<script>var P={data};var m=L.map('m');L.tileLayer('https://{{s}}.tile.openstreetmap.org/{{z}}/{{x}}/{{y}}.png',{{attribution:'© OpenStreetMap'}}).addTo(m);
var b=[];P.forEach(function(p){{var ll=[p.lat,p.lng];b.push(ll);L.marker(ll).addTo(m).bindPopup(p.name.replace(/</g,'&lt;'))}});
if(b.length)m.fitBounds(b,{{padding:[30,30],maxZoom:15}});else m.setView([40.73,-73.99],12);</script></body></html>"#);
    Html(page).into_response()
}

#[derive(serde::Deserialize, Default)]
pub struct SearchParams {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    lat: Option<String>,
    #[serde(default)]
    lon: Option<String>,
}

async fn search(State(_): State<AppState>, Query(p): Query<SearchParams>) -> Response {
    let q = p.q.as_deref().unwrap_or("").trim().to_string();
    if q.is_empty() {
        return Json(json!([])).into_response();
    }
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return Json(json!([])).into_response(),
    };
    let gkey = std::env::var("GOOGLE_MAPS_API_KEY").unwrap_or_default();
    if !gkey.is_empty() {
        if let Some(results) = google_places(&client, &gkey, &q, &p).await {
            return Json(Value::Array(results)).into_response();
        }
        // fall through to Nominatim, like Python
    }
    let results = nominatim(&client, &q).await.unwrap_or_default();
    Json(Value::Array(results)).into_response()
}

/// Google Places text search, Python's field mask + result mapping
/// (py:66546-66610). None on any failure -> caller falls to Nominatim.
async fn google_places(
    client: &reqwest::Client,
    gkey: &str,
    q: &str,
    p: &SearchParams,
) -> Option<Vec<Value>> {
    let mut payload = json!({ "textQuery": q, "languageCode": "en", "maxResultCount": 6 });
    if let (Some(lat), Some(lon)) = (
        p.lat.as_deref().and_then(|v| v.parse::<f64>().ok()),
        p.lon.as_deref().and_then(|v| v.parse::<f64>().ok()),
    ) {
        payload["locationBias"] = json!({ "circle": {
            "center": { "latitude": lat, "longitude": lon },
            "radius": 50000.0
        }});
    }
    let resp = client
        .post("https://places.googleapis.com/v1/places:searchText")
        .header("Content-Type", "application/json")
        .header("X-Goog-Api-Key", gkey)
        .header(
            "X-Goog-FieldMask",
            "places.id,places.displayName,places.formattedAddress,places.location,places.types,\
             places.rating,places.userRatingCount,places.priceLevel,places.currentOpeningHours,\
             places.photos",
        )
        .json(&payload)
        .send()
        .await
        .ok()?;
    let data: Value = resp.json().await.ok()?;
    let price_map = |lvl: &str| -> Option<i64> {
        match lvl {
            "PRICE_LEVEL_FREE" => Some(0),
            "PRICE_LEVEL_INEXPENSIVE" => Some(1),
            "PRICE_LEVEL_MODERATE" => Some(2),
            "PRICE_LEVEL_EXPENSIVE" => Some(3),
            "PRICE_LEVEL_VERY_EXPENSIVE" => Some(4),
            _ => None,
        }
    };
    let mut results = Vec::new();
    for place in data
        .get("places")
        .and_then(|p| p.as_array())
        .unwrap_or(&Vec::new())
    {
        let loc = &place["location"];
        let mut r = json!({
            "name": place["displayName"]["text"].as_str().unwrap_or(""),
            "display_name": place["formattedAddress"].as_str().unwrap_or(""),
            // Python str()s the coordinates; the SPA parses them back.
            "lat": loc["latitude"].as_f64().unwrap_or(0.0).to_string(),
            "lon": loc["longitude"].as_f64().unwrap_or(0.0).to_string(),
            "type": place["types"].as_array().and_then(|t| t.first()).and_then(|t| t.as_str()).unwrap_or(""),
            "source": "google",
            "place_id": place["id"].as_str().unwrap_or(""),
        });
        let o = r.as_object_mut().expect("result is an object");
        if let Some(rating) = place["rating"].as_f64() {
            o.insert("rating".into(), json!(rating));
        }
        if let Some(n) = place["userRatingCount"].as_i64() {
            o.insert("rating_count".into(), json!(n));
        }
        if let Some(pl) = place["priceLevel"].as_str().and_then(price_map) {
            o.insert("price_level".into(), json!(pl));
        }
        if let Some(open_now) = place["currentOpeningHours"]["openNow"].as_bool() {
            o.insert("open_now".into(), json!(open_now));
        }
        if let Some(photo) = place["photos"]
            .as_array()
            .and_then(|p| p.first())
            .and_then(|p| p["name"].as_str())
            .filter(|s| !s.is_empty())
        {
            o.insert("photo_name".into(), json!(photo));
        }
        results.push(r);
    }
    Some(results)
}

/// Nominatim fallback (py:66612-66633).
async fn nominatim(client: &reqwest::Client, q: &str) -> Option<Vec<Value>> {
    let url = format!(
        "https://nominatim.openstreetmap.org/search?q={}&format=json&limit=6&addressdetails=1",
        urlencoding_encode(q)
    );
    let resp = client
        .get(&url)
        .header("Accept-Language", "en")
        .header("User-Agent", "amux/1.0")
        .send()
        .await
        .ok()?;
    let data: Value = resp.json().await.ok()?;
    let mut results = Vec::new();
    for r in data.as_array()? {
        let display = r["display_name"].as_str().unwrap_or("");
        let name = r["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| display.split(',').next().unwrap_or(""));
        results.push(json!({
            "name": name,
            "display_name": display,
            "lat": r["lat"].as_str().unwrap_or("0"),
            "lon": r["lon"].as_str().unwrap_or("0"),
            "type": r["type"].as_str().unwrap_or(""),
            "category": r["category"].as_str().unwrap_or(""),
            "source": "osm",
        }));
    }
    Some(results)
}

/// Python's urllib.parse.quote for the query (no external crate needed).
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_matches_python_urllib_defaults() {
        // urllib.parse.quote keeps '/' by default; spaces become %20.
        assert_eq!(
            urlencoding_encode("cafe near 5th/main"),
            "cafe%20near%205th/main"
        );
        assert_eq!(urlencoding_encode("naïve"), "na%C3%AFve");
    }

}
