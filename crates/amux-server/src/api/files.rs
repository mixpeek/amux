//! Legacy Files spellings (RR-0093), now adapters onto the one browse API and
//! the one upload writer (primitive integrity audit 2026-11-26, P1).
//!
//! Routes (nested at `/api/files`), each rooted at `$HOME` (override:
//! `AMUX_FILES_ROOT`, which is also how tests pin a temp root):
//! - `GET  /api/files?path=<rel>`          — resolves `<rel>` under the root,
//!   then answers EXACTLY what `GET /api/fs/list?path=<abs>` answers
//!   (`{path, count, entries:[{name, dir, size, modified}], truncated?}`).
//! - `GET  /api/files/download?path=<rel>` — resolves, then answers what
//!   `GET /api/file/raw?path=<abs>&download=1` answers (streamed, ranges,
//!   attachment disposition; no 50MB buffer cap any more).
//! - `POST /api/files/upload?path=<rel>`   — RAW REQUEST BODY (not multipart),
//!   written through `upload::publish`, the chunked upload's writer. Replaces
//!   an existing file, as this route always did. 50MB cap (413 beyond):
//!   larger files belong on the chunked `/api/upload`.
//!
//! These stay because callers outside this repo can reach them (the `amux`
//! skill doc taught the curl recipes; `cloud/export_env.py` lists through
//! `/api/files`). No dashboard code calls them. Every hit is counted
//! (`legacy_routes::record`, `GET /api/debug/legacy-routes`) so a sweep can
//! tell when they are unused and can be deleted.
//!
//! Path traversal: every request path is joined to the root, canonicalized
//! (symlinks resolved), and prefix-checked against the canonicalized root.
//! `../` escapes and symlinks pointing outside the root both fail the same
//! check. The fs handlers then apply their own deny sets on top. Uploads
//! additionally reject `..`/absolute components lexically BEFORE any
//! filesystem work, because the target may not exist yet.

use super::AppState;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, Request};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

pub const MAX_BYTES: usize = 50 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list))
        .route("/download", get(download))
        // Slack above the cap so the explicit 413 below (with a JSON body
        // naming the limit) fires before the layer's bare-text rejection.
        .route(
            "/upload",
            post(upload).layer(DefaultBodyLimit::max(MAX_BYTES + 64 * 1024)),
        )
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// The files root: `AMUX_FILES_ROOT` (tests, containers) else `$HOME`.
fn files_root() -> PathBuf {
    std::env::var("AMUX_FILES_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| "/".into())
        })
}

/// Resolve a request path that must EXIST (list/download): join, canonicalize
/// (resolving `..` and symlinks), then require the result to stay under the
/// canonicalized root. Returns a caller-safe error string.
pub fn resolve_existing(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let canon_root = root
        .canonicalize()
        .map_err(|e| format!("files root {} unusable: {e}", root.display()))?;
    let rel = rel.trim();
    let joined = if rel.is_empty() {
        canon_root.clone()
    } else {
        let p = Path::new(rel);
        // Absolute paths are allowed but get the identical containment check.
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            canon_root.join(p)
        }
    };
    let canon = joined
        .canonicalize()
        .map_err(|_| format!("no such path: {rel}"))?;
    if !canon.starts_with(&canon_root) {
        return Err("path escapes the files root".into());
    }
    Ok(canon)
}

/// Lexical validation for upload targets (which may not exist yet): every
/// component must be a normal name — no `..`, no absolute prefix, no `.`.
pub fn validate_upload_rel(rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Err("path required".into());
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return Err("upload path must be relative to the files root".into());
    }
    let mut clean = PathBuf::new();
    for c in p.components() {
        match c {
            Component::Normal(seg) => clean.push(seg),
            Component::CurDir => {}
            _ => return Err("upload path may not contain '..' or a root prefix".into()),
        }
    }
    if clean.as_os_str().is_empty() {
        return Err("path required".into());
    }
    Ok(clean)
}

#[derive(Deserialize)]
pub struct PathParam {
    #[serde(default)]
    path: String,
}

fn legacy_hit(legacy: &'static str, canonical: &'static str, headers: &HeaderMap) {
    super::legacy_routes::record(
        legacy,
        canonical,
        legacy,
        &super::legacy_routes::caller_of(headers),
    );
}

async fn list(headers: HeaderMap, Query(q): Query<PathParam>) -> Response {
    legacy_hit("/api/files", "/api/fs/list", &headers);
    let root = files_root();
    let dir = match resolve_existing(&root, &q.path) {
        Ok(d) => d,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let abs = dir.to_string_lossy().into_owned();
    match crate::db::interactions::spawn_blocking(move || super::fs::list_dir_at(&abs)).await {
        Ok(res) => res,
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": e.to_string() }),
        ),
    }
}

async fn download(req: Request) -> Response {
    legacy_hit("/api/files/download", "/api/file/raw?download=1", req.headers());
    let rel = super::fs::parse_qs(req.uri().query().unwrap_or(""))
        .into_iter()
        .find(|(k, _)| k == "path")
        .map(|(_, v)| v)
        .unwrap_or_default();
    let root = files_root();
    let file = match resolve_existing(&root, &rel) {
        Ok(f) => f,
        Err(e) => return err(StatusCode::NOT_FOUND, json!({ "error": e })),
    };
    let mut url = match reqwest::Url::parse("http://localhost/api/file/raw") {
        Ok(u) => u,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("could not build the raw URL: {e}") }),
            )
        }
    };
    url.query_pairs_mut()
        .append_pair("path", &file.to_string_lossy())
        .append_pair("download", "1");
    let target = format!("{}?{}", url.path(), url.query().unwrap_or_default());
    let (mut parts, body) = req.into_parts();
    parts.uri = match target.parse() {
        Ok(u) => u,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": format!("could not build the raw URI: {e}") }),
            )
        }
    };
    super::file_viewer::raw(Request::from_parts(parts, body)).await
}

async fn upload(headers: HeaderMap, Query(q): Query<PathParam>, body: Bytes) -> Response {
    legacy_hit("/api/files/upload", "/api/upload", &headers);
    if body.len() > MAX_BYTES {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({ "error": format!("body is {} bytes; upload cap is {} bytes (use the chunked /api/upload for larger files)", body.len(), MAX_BYTES), "limit": MAX_BYTES }),
        );
    }
    let root = files_root();
    let rel = match validate_upload_rel(&q.path) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let canon_root = match root.canonicalize() {
        Ok(r) => r,
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": e.to_string() }),
            )
        }
    };
    let target = canon_root.join(&rel);
    let (Some(parent), Some(name)) = (target.parent(), target.file_name()) else {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "path required" }));
    };
    let name = name.to_string_lossy().into_owned();
    if let Err(e) = tokio::fs::create_dir_all(parent).await {
        return err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": e.to_string() }),
        );
    }
    // Post-create containment check: the lexical filter above blocks
    // `..`, but a symlink INSIDE the root can still point outside it —
    // canonicalize the now-existing parent and re-verify.
    let real_parent = match parent.canonicalize() {
        Ok(real_parent) if real_parent.starts_with(&canon_root) => real_parent,
        Ok(_) => {
            return err(
                StatusCode::BAD_REQUEST,
                json!({ "error": "upload path resolves outside the files root (symlink)" }),
            )
        }
        Err(e) => {
            return err(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": e.to_string() }),
            )
        }
    };
    let size = body.len();
    let written = crate::db::interactions::spawn_blocking(move || {
        super::upload::publish(&real_parent, &name, super::upload::OnExisting::Replace, |f| {
            std::io::Write::write_all(f, &body)
        })
    })
    .await;
    match written {
        Ok(Ok(_)) => Json(json!({ "ok": true, "path": rel.display().to_string(), "size": size }))
            .into_response(),
        Ok(Err(e)) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": e.to_string() }),
        ),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "error": e.to_string() }),
        ),
    }
}

// ---- tests ----------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn traversal_escape_is_rejected() {
        let dir = root();
        std::fs::write(dir.path().join("inside.txt"), "ok").unwrap();

        // The classic ../ escape — must be rejected even though /etc exists.
        assert!(resolve_existing(dir.path(), "../../../../etc").is_err());
        assert!(resolve_existing(dir.path(), "../").is_err());
        // Absolute path outside the root: same check, same refusal.
        assert!(resolve_existing(dir.path(), "/etc/hosts").is_err());
        // Mixed: legit prefix, escaping suffix.
        assert!(resolve_existing(dir.path(), "a/../../outside").is_err());
        // Symlink inside the root pointing outside must not resolve.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", dir.path().join("sneaky")).unwrap();
            assert!(
                resolve_existing(dir.path(), "sneaky/hosts").is_err(),
                "symlink escape"
            );
        }
        // ...while honest paths resolve.
        assert!(resolve_existing(dir.path(), "inside.txt").is_ok());
        assert!(
            resolve_existing(dir.path(), "").is_ok(),
            "empty path = root"
        );
    }

    #[test]
    fn upload_rel_validation_is_lexical() {
        assert!(validate_upload_rel("notes/a.txt").is_ok());
        assert!(validate_upload_rel("./a.txt").is_ok());
        assert!(validate_upload_rel("").is_err());
        assert!(validate_upload_rel("   ").is_err());
        assert!(validate_upload_rel("../x").is_err());
        assert!(validate_upload_rel("a/../../x").is_err());
        assert!(validate_upload_rel("/abs/path").is_err());
    }
}
