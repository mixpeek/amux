//! Branding / white-label API + the dynamic PWA manifest (Python
//! amux-server.py:67562 `/api/branding`, :66000 `/manifest.json`).
//!
//! - GET /api/branding — `brand_*` prefs (key minus the prefix) plus
//!   `icon_url`/`logo_url` when a custom asset file exists under
//!   `<amux_home>/branding/`.
//! - POST /api/branding — text prefs (name/tagline/color) upserted as
//!   `brand_<key>`; icon/logo accepted as data-URL or raw base64, sniffed by
//!   magic bytes (PNG/JPEG/WebP/SVG only), capped at 5 MB, written to the
//!   branding dir replacing any older extension of the same asset.
//! - DELETE /api/branding — remove all `brand_*` prefs and asset files.
//! - GET /api/branding/{fname} — serve an asset file. PUBLIC in Python
//!   (`_PUBLIC_PREFIXES`), because the SPA loads these via plain `<img src>`
//!   with no auth header — mounted outside require_bearer in api/mod.rs for
//!   the same reason.
//! - GET /manifest.json — the embedded manifest with `brand_name` /
//!   `brand_tagline` / `brand_color` overrides applied at serve time, so the
//!   installed PWA's name/color follow the pref (the extraction had left
//!   this file static). Python-parity: name becomes "<name> — <tagline>"
//!   when a tagline is set, short_name is the bare name, theme_color is the
//!   brand color; cached like Python's `_raw(cache=True)`.

use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use serde_json::{json, Map, Value};
use std::path::PathBuf;

use super::settings::amux_home;
use super::AppState;

/// Python `CC_BRANDING = CC_HOME / "branding"`.
fn branding_dir() -> PathBuf {
    amux_home().join("branding")
}

const ASSETS: [&str; 2] = ["icon", "logo"];
const EXTS: [&str; 5] = [".png", ".jpg", ".jpeg", ".svg", ".webp"];

fn content_type_for(ext: &str) -> &'static str {
    match ext {
        ".png" => "image/png",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".svg" => "image/svg+xml",
        ".webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

async fn brand_prefs(state: &AppState) -> Result<Vec<(String, String)>, String> {
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<(String, String)>> {
        let conn = store.read()?;
        let mut stmt = conn.prepare("SELECT key, value FROM prefs WHERE key LIKE 'brand_%'")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

// ---- GET /api/branding ------------------------------------------------------

pub async fn get_branding(State(state): State<AppState>) -> Response {
    let rows = match brand_prefs(&state).await {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e })),
    };
    let mut result = Map::new();
    for (k, v) in rows {
        let key = k.strip_prefix("brand_").unwrap_or(&k).to_string();
        result.insert(key, Value::String(v));
    }
    let dir = branding_dir();
    for asset in ASSETS {
        for ext in EXTS {
            if dir.join(format!("{asset}{ext}")).exists() {
                result.insert(format!("{asset}_url"), json!(format!("/api/branding/{asset}{ext}")));
                break;
            }
        }
    }
    Json(Value::Object(result)).into_response()
}

// ---- POST /api/branding -----------------------------------------------------

/// Magic-byte sniff, Python's exact rules and order.
fn sniff_ext(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(".png");
    }
    if data.starts_with(b"\xff\xd8") {
        return Some(".jpg");
    }
    if data.starts_with(b"RIFF") && data.len() > 12 && &data[8..12] == b"WEBP" {
        return Some(".webp");
    }
    let head = &data[..data.len().min(500)];
    if head.windows(4).any(|w| w == b"<svg") {
        return Some(".svg");
    }
    None
}

pub async fn post_branding(State(state): State<AppState>, body: Option<Json<Value>>) -> Response {
    let body = body.map(|Json(v)| v).unwrap_or_else(|| json!({}));
    let mut saved = Map::new();

    // Text prefs. Python `str(val)` on whatever arrived; strings pass
    // through, other scalars via their JSON spelling.
    let mut pref_writes: Vec<(String, String)> = Vec::new();
    for key in ["name", "tagline", "color"] {
        match body.get(key) {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) => {
                pref_writes.push((format!("brand_{key}"), s.clone()));
                saved.insert(key.to_string(), json!(s));
            }
            Some(v) => {
                let s = v.to_string();
                pref_writes.push((format!("brand_{key}"), s.clone()));
                saved.insert(key.to_string(), json!(s));
            }
        }
    }
    if !pref_writes.is_empty() {
        let res = state
            .store
            .write_async(move |conn| {
                for (k, v) in &pref_writes {
                    conn.execute(
                        "INSERT INTO prefs (key, value) VALUES (?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value = ?2",
                        rusqlite::params![k, v],
                    )?;
                }
                Ok(crate::db::WriteOutcome {
                    applied: true,
                    events: vec![crate::db::PendingEvent {
                        entity_type: amux_core::revision::EntityType::Other("pref".into()),
                        entity_id: "branding".into(),
                        mutation: amux_core::revision::MutationKind::Updated,
                        payload: None,
                    }],
                })
            })
            .await;
        if let Err(e) = res {
            return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }));
        }
    }

    // Image assets (base64, data-URL or raw).
    for asset in ASSETS {
        let Some(b64_raw) = body.get(asset).and_then(Value::as_str) else { continue };
        if b64_raw.is_empty() {
            continue;
        }
        let b64 = match b64_raw.split_once(',') {
            Some((_, rest)) => rest,
            None => b64_raw,
        };
        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
            return err(StatusCode::BAD_REQUEST, json!({ "error": format!("invalid base64 for {asset}") }));
        };
        if data.len() > 5 * 1024 * 1024 {
            return err(StatusCode::BAD_REQUEST, json!({ "error": format!("{asset} too large (max 5 MB)") }));
        }
        let Some(ext) = sniff_ext(&data) else {
            return err(
                StatusCode::BAD_REQUEST,
                json!({ "error": format!("{asset} must be PNG, JPEG, WebP, or SVG") }),
            );
        };
        let dir = branding_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }));
        }
        // Remove any older extension of this asset, then write the new one.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.path().file_stem().and_then(|s| s.to_str()) == Some(asset) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        if let Err(e) = std::fs::write(dir.join(format!("{asset}{ext}")), &data) {
            return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }));
        }
        saved.insert(format!("{asset}_url"), json!(format!("/api/branding/{asset}{ext}")));
    }

    let mut out = Map::new();
    out.insert("ok".into(), json!(true));
    out.extend(saved);
    Json(Value::Object(out)).into_response()
}

// ---- DELETE /api/branding ---------------------------------------------------

pub async fn delete_branding(State(state): State<AppState>) -> Response {
    let res = state
        .store
        .write_async(|conn| {
            conn.execute("DELETE FROM prefs WHERE key LIKE 'brand_%'", [])?;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![crate::db::PendingEvent {
                    entity_type: amux_core::revision::EntityType::Other("pref".into()),
                    entity_id: "branding".into(),
                    mutation: amux_core::revision::MutationKind::Deleted,
                    payload: None,
                }],
            })
        })
        .await;
    if let Err(e) = res {
        return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }));
    }
    if let Ok(entries) = std::fs::read_dir(branding_dir()) {
        for e in entries.flatten() {
            let _ = std::fs::remove_file(e.path()); // best-effort, like Python
        }
    }
    Json(json!({ "ok": true })).into_response()
}

// ---- GET /api/branding/{fname} (PUBLIC) -------------------------------------

pub async fn serve_asset(AxumPath(fname): AxumPath<String>) -> Response {
    // Python's containment rule: no separators, no dotfiles.
    if fname.contains('/') || fname.contains('\\') || fname.starts_with('.') {
        return err(StatusCode::NOT_FOUND, json!({ "error": "not found" }));
    }
    let path = branding_dir().join(&fname);
    let Ok(data) = std::fs::read(&path) else {
        return err(StatusCode::NOT_FOUND, json!({ "error": "not found" }));
    };
    let ext = fname
        .rfind('.')
        .map(|i| fname[i..].to_lowercase())
        .unwrap_or_default();
    ([(header::CONTENT_TYPE, content_type_for(&ext))], data).into_response()
}

// ---- GET /manifest.json (PUBLIC) --------------------------------------------

pub async fn manifest(State(state): State<AppState>) -> Response {
    let base = amux_dashboard::DashboardAssets::get("manifest.json")
        .map(|f| f.data.into_owned())
        .unwrap_or_else(|| b"{}".to_vec());
    let mut manifest: Value = serde_json::from_slice(&base).unwrap_or_else(|_| json!({}));
    // Best-effort override, like Python's try/except: a prefs read failure
    // serves the stock manifest rather than failing the PWA install.
    if let Ok(rows) = brand_prefs(&state).await {
        let get = |k: &str| rows.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        if let Some(name) = get("brand_name") {
            manifest["short_name"] = json!(name);
            manifest["name"] = match get("brand_tagline") {
                Some(tag) => json!(format!("{name} — {tag}")),
                None => json!(name),
            };
        }
        if let Some(color) = get("brand_color") {
            manifest["theme_color"] = json!(color);
        }
    }
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            // Python `_raw(..., cache=True)`.
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        manifest.to_string(),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::settings::test_env;
    use axum::body::Body;
    use axum::http::Request;
    use axum::Router;
    use tower::ServiceExt;

    fn app() -> Router {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::db::Store::open(&dir.path().join("branding-test.db")).unwrap();
        std::mem::forget(dir);
        let state = AppState {
            secrets: std::sync::Arc::new(crate::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
            store: std::sync::Arc::new(store),
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        Router::new()
            .route(
                "/api/branding",
                axum::routing::get(get_branding).post(post_branding).delete(delete_branding),
            )
            .route("/api/branding/{fname}", axum::routing::get(serve_asset))
            .route("/manifest.json", axum::routing::get(manifest))
            .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
            .with_state(state)
    }

    async fn send(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Vec<u8>) {
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
        (status, bytes.to_vec())
    }

    async fn send_json(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (st, bytes) = send(app, method, path, body).await;
        (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

    fn png_data_url() -> String {
        let mut bytes = PNG_MAGIC.to_vec();
        bytes.extend_from_slice(b"fake-png-body");
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        )
    }

    #[tokio::test]
    async fn branding_full_round_trip_prefs_assets_delete() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let app = app();

        // Empty start: Python serves {} (no prefs, no assets).
        let (st, v) = send_json(&app, "GET", "/api/branding", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v, json!({}));

        // POST text prefs + a PNG icon.
        let (st, v) = send_json(
            &app,
            "POST",
            "/api/branding",
            Some(json!({ "name": "AcmeOps", "tagline": "Ops Console", "color": "#ff8800",
                          "icon": png_data_url() })),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["name"], json!("AcmeOps"));
        assert_eq!(v["icon_url"], json!("/api/branding/icon.png"));
        assert!(dir.path().join("branding/icon.png").exists());

        // GET reflects prefs (keys minus brand_ prefix) + detected asset.
        let (_, v) = send_json(&app, "GET", "/api/branding", None).await;
        assert_eq!(v["name"], json!("AcmeOps"));
        assert_eq!(v["tagline"], json!("Ops Console"));
        assert_eq!(v["color"], json!("#ff8800"));
        assert_eq!(v["icon_url"], json!("/api/branding/icon.png"));
        assert!(v.get("logo_url").is_none());

        // The asset serves publicly with its content type and exact bytes.
        let (st, bytes) = send(&app, "GET", "/api/branding/icon.png", None).await;
        assert_eq!(st, StatusCode::OK);
        assert!(bytes.starts_with(PNG_MAGIC));

        // Re-uploading as JPEG replaces the PNG (old extension removed).
        let mut jpg = b"\xff\xd8".to_vec();
        jpg.extend_from_slice(b"fake-jpeg");
        let (st, v) = send_json(
            &app,
            "POST",
            "/api/branding",
            Some(json!({ "icon": base64::engine::general_purpose::STANDARD.encode(&jpg) })),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["icon_url"], json!("/api/branding/icon.jpg"));
        assert!(!dir.path().join("branding/icon.png").exists(), "old asset must be replaced");
        assert!(dir.path().join("branding/icon.jpg").exists());

        // DELETE clears prefs and files.
        let (st, v) = send_json(&app, "DELETE", "/api/branding", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v, json!({ "ok": true }));
        let (_, v) = send_json(&app, "GET", "/api/branding", None).await;
        assert_eq!(v, json!({}));
        assert!(!dir.path().join("branding/icon.jpg").exists());
    }

    #[tokio::test]
    async fn post_rejects_bad_images_with_pythons_errors() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let app = app();

        let (st, v) =
            send_json(&app, "POST", "/api/branding", Some(json!({ "icon": "%%%not-base64%%%" }))).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], json!("invalid base64 for icon"));

        // Valid base64, unknown magic bytes.
        let (st, v) = send_json(
            &app,
            "POST",
            "/api/branding",
            Some(json!({ "logo": base64::engine::general_purpose::STANDARD.encode(b"GIF89a-nope") })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], json!("logo must be PNG, JPEG, WebP, or SVG"));

        // Over the 5 MB cap.
        let mut big = PNG_MAGIC.to_vec();
        big.resize(5 * 1024 * 1024 + 1, 0u8);
        let (st, v) = send_json(
            &app,
            "POST",
            "/api/branding",
            Some(json!({ "icon": base64::engine::general_purpose::STANDARD.encode(&big) })),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], json!("icon too large (max 5 MB)"));
    }

    #[tokio::test]
    async fn asset_serving_containment_matches_python() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        std::fs::create_dir_all(dir.path().join("branding")).unwrap();
        std::fs::write(dir.path().join("branding/.secret"), b"nope").unwrap();
        let app = app();
        // Dotfiles and misses are the same 404.
        let (st, v) = send_json(&app, "GET", "/api/branding/.secret", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        assert_eq!(v["error"], json!("not found"));
        let (st, _) = send_json(&app, "GET", "/api/branding/nothere.png", None).await;
        assert_eq!(st, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn manifest_follows_branding_prefs() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = test_env::set_home(dir.path());
        let app = app();

        // Stock manifest before any branding.
        let (st, v) = send_json(&app, "GET", "/manifest.json", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["short_name"], json!("amux"));
        let stock_theme = v["theme_color"].clone();

        // Brand it; the PWA identity follows the prefs (Python :66000).
        let (_, r) = send_json(
            &app,
            "POST",
            "/api/branding",
            Some(json!({ "name": "AcmeOps", "tagline": "Ops Console", "color": "#ff8800" })),
        )
        .await;
        assert_eq!(r["ok"], json!(true));
        let (_, v) = send_json(&app, "GET", "/manifest.json", None).await;
        assert_eq!(v["short_name"], json!("AcmeOps"));
        assert_eq!(v["name"], json!("AcmeOps — Ops Console"));
        assert_eq!(v["theme_color"], json!("#ff8800"));
        assert_ne!(v["theme_color"], stock_theme);

        // Name without tagline: name == short_name (Python's else branch).
        let (_, _) = send_json(&app, "DELETE", "/api/branding", None).await;
        let (_, r) = send_json(&app, "POST", "/api/branding", Some(json!({ "name": "Solo" }))).await;
        assert_eq!(r["ok"], json!(true));
        let (_, v) = send_json(&app, "GET", "/manifest.json", None).await;
        assert_eq!(v["name"], json!("Solo"));
        assert_eq!(v["short_name"], json!("Solo"));
    }

    #[test]
    fn magic_sniffing_matches_python() {
        assert_eq!(sniff_ext(PNG_MAGIC), Some(".png"));
        assert_eq!(sniff_ext(b"\xff\xd8\xff\xe0rest"), Some(".jpg"));
        let mut webp = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        webp.extend_from_slice(b"VP8 ");
        assert_eq!(sniff_ext(&webp), Some(".webp"));
        assert_eq!(sniff_ext(b"<?xml version=\"1.0\"?><svg xmlns=\"x\"/>"), Some(".svg"));
        assert_eq!(sniff_ext(b"GIF89a"), None);
        // <svg deeper than 500 bytes is NOT sniffed (Python checks data[:500]).
        let mut late_svg = vec![b' '; 600];
        late_svg.extend_from_slice(b"<svg/>");
        assert_eq!(sniff_ext(&late_svg), None);
    }
}
