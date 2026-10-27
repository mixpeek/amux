//! A person's picture: shown wherever amux draws their initial (Messages, Team).
//!
//! Ethan, 2026-10-27: "use these images as our avatars" for himself and Doron on
//! the sandbox. One file per person under `<amux home>/avatars/`, named by their
//! email, so nothing new goes in the database and deleting a member's file is the
//! whole cleanup.
//!
//! Who may set one: the person themselves (a signed-in member, for their own
//! email), an admin member, or the owner. Never a worker: a picture is how a
//! person is recognised in a conversation, so a lane must not be able to change
//! whose face sits beside a message. Only raster images (PNG, JPEG, WebP, GIF)
//! are kept, recognised by their bytes, so an uploaded SVG or HTML page can
//! never be served back from this origin.

use axum::body::Bytes;
use axum::extract::{Path as AxPath, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

use crate::config::amux_home;

/// Largest picture kept. Avatars render at 34px; 1 MB is generous.
const MAX_BYTES: usize = 1024 * 1024;

pub fn routes() -> Router<super::AppState> {
    Router::new()
        .route("/api/avatars", get(list))
        .route("/api/avatars/{email}", get(serve).put(set).delete(remove))
}

fn dir() -> PathBuf {
    amux_home().join("avatars")
}

/// The file name for an email, or None when it is not a plausible address. The
/// check keeps the name inside `avatars/` (no `/`, no leading dot, no `..`).
fn file_for(email: &str) -> Option<PathBuf> {
    let e = email.trim().to_ascii_lowercase();
    let ok = e.len() <= 254
        && e.contains('@')
        && !e.starts_with('.')
        && !e.contains("..")
        && e.chars().all(|c| c.is_ascii_alphanumeric() || "@._+-".contains(c));
    ok.then(|| dir().join(e))
}

/// The image type, from the bytes alone.
fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else {
        None
    }
}

/// Every stored picture, email -> modified time (the client's cache key).
async fn list() -> Json<Value> {
    let mut out = Map::new();
    if let Ok(rd) = std::fs::read_dir(dir()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if file_for(&name).is_none() {
                continue;
            }
            let ts = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.insert(name, json!(ts));
        }
    }
    Json(Value::Object(out))
}

async fn serve(AxPath(email): AxPath<String>) -> Response {
    let Some(path) = file_for(&email) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(kind) = sniff(&bytes) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "private, max-age=86400"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response()
}

fn allowed(state: &super::AppState, headers: &HeaderMap, uri: &Uri, email: &str) -> Result<String, &'static str> {
    let owner_credential = state.auth_token.is_none() || super::auth::has_owner_token(state, headers, uri);
    decide(headers, owner_credential, email)
}

/// Who is changing `email`'s picture, or why they may not. `owner_credential`:
/// the request carries the owner bearer, or this host has no auth token.
fn decide(headers: &HeaderMap, owner_credential: bool, email: &str) -> Result<String, &'static str> {
    if let Some(actor) = super::org::local_member_actor(headers) {
        if headers.contains_key("x-amux-worker") {
            return Err("a worker cannot change a person's picture");
        }
        let me = actor.strip_prefix("member:").unwrap_or(actor);
        if me.eq_ignore_ascii_case(email) || super::org::local_member_is_admin(headers) {
            return Ok(actor.to_string());
        }
        return Err("a member can change only their own picture; an admin can change anyone's");
    }
    if ["x-amux-session", "x-amux-worker"].iter().any(|h| headers.contains_key(*h)) {
        return Err("a worker cannot change a person's picture");
    }
    if !owner_credential {
        return Err("send the owner bearer");
    }
    Ok("owner".to_string())
}

fn refused(email: &str, why: &'static str) -> Response {
    tracing::warn!(email, why, measured = true, n_considered = 1, verdict = "avatar_refused",
        "avatars: a picture change was refused");
    (StatusCode::FORBIDDEN, Json(json!({"error": why}))).into_response()
}

async fn set(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    uri: Uri,
    AxPath(email): AxPath<String>,
    body: Bytes,
) -> Response {
    let Some(path) = file_for(&email) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "not an email address"}))).into_response();
    };
    let who = match allowed(&state, &headers, &uri, &email) {
        Ok(who) => who,
        Err(why) => return refused(&email, why),
    };
    if body.len() > MAX_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error": "the picture is over 1 MB"}))).into_response();
    }
    if sniff(&body).is_none() {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(json!({"error": "send a PNG, JPEG, WebP or GIF image"}))).into_response();
    }
    // Write beside, then rename: a reader never sees half a picture.
    let tmp = path.with_extension("upload");
    let wrote = std::fs::create_dir_all(dir())
        .and_then(|_| std::fs::write(&tmp, &body))
        .and_then(|_| std::fs::rename(&tmp, &path));
    if let Err(e) = wrote {
        tracing::warn!(email, error = %e, measured = true, n_considered = 1, verdict = "avatar_write_failed",
            "avatars: the picture could not be saved");
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response();
    }
    tracing::info!(email, by = %who, bytes = body.len(), measured = true, n_considered = 1,
        verdict = "avatar_set", "avatars: a person's picture was set");
    Json(json!({"ok": true, "email": email.to_ascii_lowercase(), "bytes": body.len()})).into_response()
}

async fn remove(
    State(state): State<super::AppState>,
    headers: HeaderMap,
    uri: Uri,
    AxPath(email): AxPath<String>,
) -> Response {
    let Some(path) = file_for(&email) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "not an email address"}))).into_response();
    };
    let who = match allowed(&state, &headers, &uri, &email) {
        Ok(who) => who,
        Err(why) => return refused(&email, why),
    };
    let existed = std::fs::remove_file(&path).is_ok();
    tracing::info!(email, by = %who, existed, measured = true, n_considered = 1,
        verdict = "avatar_removed", "avatars: a person's picture was removed");
    Json(json!({"ok": true, "removed": existed})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_plain_email_names_a_file_inside_the_avatar_folder() {
        assert!(file_for("Ethan@Example.com").unwrap().ends_with("avatars/ethan@example.com"));
        for bad in ["../secrets.json", "a/b@c.d", ".hidden@x.y", "x@y..z", "no-at-sign", "a@b.c\0", "a b@c.d"] {
            assert!(file_for(bad).is_none(), "{bad} must not name a file");
        }
    }

    #[test]
    fn only_raster_images_are_recognised() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\nrest"), Some("image/png"));
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'><script/></svg>"), None);
        assert_eq!(sniff(b"<html><script>alert(1)</script>"), None);
    }

    #[test]
    fn a_member_sets_only_their_own_picture_and_an_admin_sets_anyones() {
        let member = crate::api::org::test_member_headers("doron@example.com", false);
        assert!(decide(&member, false, "Doron@Example.com").is_ok(), "their own picture");
        assert!(decide(&member, false, "ethan@example.com").is_err(), "someone else's");
        let admin = crate::api::org::test_member_headers("ethan@example.com", true);
        assert_eq!(decide(&admin, false, "doron@example.com").as_deref(), Ok("member:ethan@example.com"));
        let mut admin_lane = admin.clone();
        admin_lane.insert("x-amux-worker", "some-worker".parse().unwrap());
        assert!(decide(&admin_lane, false, "doron@example.com").is_err(), "a lane holding an admin's cookie");
        let mut lane = HeaderMap::new();
        lane.insert("x-amux-session", "some-worker".parse().unwrap());
        assert!(decide(&lane, true, "ethan@example.com").is_err(), "a worker, even with the owner bearer");
        assert_eq!(decide(&HeaderMap::new(), true, "ethan@example.com").as_deref(), Ok("owner"));
        assert!(decide(&HeaderMap::new(), false, "ethan@example.com").is_err(), "no owner credential");
    }
}
