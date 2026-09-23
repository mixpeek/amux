//! /api/map — the Map tab's document + pin + place-search endpoints
//! (AMUX-2586 fix #6, ported from amux-server.py:66470-66640).
//!
//! Storage is the JSON FILE ~/.amux/map.json (`CC_MAP`), not a table —
//! ported exactly: same default shape, same whole-document POST with the
//! shrink guard + autobak snapshot, same additive /pins endpoint, same
//! Google-Places-then-Nominatim search with Python's result mapping.

use super::AppState;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(get_map).post(post_map))
        .route("/pins", post(post_pin))
        .route("/search", get(search))
}

fn map_path() -> std::path::PathBuf {
    let home = std::env::var("AMUX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux")
        });
    home.join("map.json")
}

fn read_map_or(default: Value) -> Value {
    std::fs::read_to_string(map_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(default)
}

fn default_doc() -> Value {
    json!({ "pins": [], "tags": [], "settings": {} })
}

fn pins_len(v: &Value) -> usize {
    v.get("pins")
        .and_then(|p| p.as_array())
        .map(|a| a.len())
        .unwrap_or(0)
}

async fn get_map(State(_): State<AppState>) -> Response {
    let mut data = read_map_or(default_doc());
    if !data.is_object() {
        data = default_doc();
    }
    let obj = data.as_object_mut().expect("map doc is an object");
    let settings = obj.entry("settings").or_insert_with(|| json!({}));
    if let Some(s) = settings.as_object_mut() {
        s.insert(
            "googleMapsKey".into(),
            json!(std::env::var("GOOGLE_MAPS_API_KEY").unwrap_or_default()),
        );
    }
    Json(data).into_response()
}

#[derive(serde::Deserialize, Default)]
pub struct ReplaceParam {
    #[serde(default)]
    replace: Option<String>,
}

async fn post_map(
    State(_): State<AppState>,
    Query(q): Query<ReplaceParam>,
    Json(body): Json<Value>,
) -> Response {
    let old = read_map_or(json!({}));
    let old_n = pins_len(&old);
    let new_n = pins_len(&body);
    let replace = matches!(q.replace.as_deref(), Some("1") | Some("true") | Some("yes"));
    // Guard (py:66486-66499): POST replaces the WHOLE document; refuse a
    // write that drops pins unless ?replace=1 — a naive partial POST from a
    // session must not wipe the map.
    if old_n > 0 && new_n < old_n && !replace {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!(
                    "refusing to drop pins {old_n}->{new_n}: POST /api/map replaces the whole map. \
                     To ADD a pin use POST /api/map/pins; to intentionally replace everything pass ?replace=1"
                ),
                "existing_pins": old_n,
                "submitted_pins": new_n,
            })),
        )
            .into_response();
    }
    // Safety net: snapshot before any shrink so a wipe is recoverable.
    if old_n > 0 && new_n < old_n {
        let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let bak = map_path().with_file_name(format!("map.json.autobak-{ts}"));
        let _ = std::fs::write(bak, old.to_string());
    }
    match std::fs::write(map_path(), body.to_string()) {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn post_pin(State(_): State<AppState>, Json(body): Json<Value>) -> Response {
    let mut data = read_map_or(default_doc());
    if !data.is_object() {
        data = default_doc();
    }
    let mut pin = match body.get("pin") {
        Some(p) if p.is_object() => p.clone(),
        _ => body,
    };
    if pin.get("lat").map(Value::is_null).unwrap_or(true)
        || pin.get("lng").map(Value::is_null).unwrap_or(true)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "pin requires lat and lng" })),
        )
            .into_response();
    }
    {
        let p = pin.as_object_mut().expect("pin is an object");
        let falsy = |v: Option<&Value>| {
            v.map(|x| x.is_null() || x.as_str().is_some_and(str::is_empty))
                .unwrap_or(true)
        };
        if falsy(p.get("id")) {
            p.insert(
                "id".into(),
                json!(format!("pin_{}", chrono::Utc::now().timestamp_millis())),
            );
        }
        if !p.contains_key("name") {
            let name = p
                .get("title")
                .and_then(|t| t.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("Pin")
                .to_string();
            p.insert("name".into(), json!(name));
        }
        if !p.contains_key("tags") {
            p.insert("tags".into(), json!([]));
        }
        if !p.contains_key("desc") {
            p.insert("desc".into(), json!(""));
        }
    }
    let obj = data.as_object_mut().expect("map doc is an object");
    let pins = obj.entry("pins").or_insert_with(|| json!([]));
    let total = match pins.as_array_mut() {
        Some(a) => {
            a.push(pin.clone());
            a.len()
        }
        None => {
            *pins = json!([pin.clone()]);
            1
        }
    };
    match std::fs::write(map_path(), data.to_string()) {
        Ok(()) => Json(json!({ "ok": true, "pin": pin, "total_pins": total })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
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

    #[test]
    fn pins_len_counts_only_arrays() {
        assert_eq!(pins_len(&json!({ "pins": [1, 2] })), 2);
        assert_eq!(pins_len(&json!({ "pins": "x" })), 0);
        assert_eq!(pins_len(&json!({})), 0);
    }
}
