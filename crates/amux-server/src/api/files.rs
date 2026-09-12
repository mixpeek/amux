//! Files API (RR-0093): browse / download / upload rooted at `$HOME`
//! (override: `AMUX_FILES_ROOT`, which is also how tests pin a temp root).
//!
//! Routes (nested at `/api/files`):
//! - `GET  /api/files?path=<rel>`            — directory listing
//!   `{path, entries:[{name,size,mtime,kind}]}` (kind: file|dir|symlink|other)
//! - `GET  /api/files/download?path=<rel>`   — raw bytes, attachment headers
//! - `POST /api/files/upload?path=<rel>`     — RAW REQUEST BODY, not
//!   multipart: the workspace's axum has no `multipart` feature (checked —
//!   default features + http2 only), and adding one for a single endpoint is
//!   not worth the surface. Callers send the file bytes as the body:
//!   `curl --data-binary @file '$URL/api/files/upload?path=dir/file'`.
//!
//! Size limits are honest: 50MB on upload (413 beyond, enforced by both the
//! body-limit layer and an explicit check) and the same cap on download —
//! the handler buffers the file, so serving a 4GB file would really mean
//! buffering 4GB; refusing with 413 tells the truth instead.
//!
//! Path traversal: every request path is joined to the root, canonicalized
//! (symlinks resolved), and prefix-checked against the canonicalized root.
//! `../` escapes and symlinks pointing outside the root both fail the same
//! check. Uploads additionally reject `..`/absolute components lexically
//! BEFORE any filesystem work, because the target may not exist yet.

use super::AppState;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query};
use axum::http::{header, StatusCode};
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
        .route("/upload", post(upload).layer(DefaultBodyLimit::max(MAX_BYTES + 64 * 1024)))
}

fn err(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// The files root: `AMUX_FILES_ROOT` (tests, containers) else `$HOME`.
fn files_root() -> PathBuf {
    std::env::var("AMUX_FILES_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| "/".into()))
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
        if p.is_absolute() { p.to_path_buf() } else { canon_root.join(p) }
    };
    let canon = joined.canonicalize().map_err(|_| format!("no such path: {rel}"))?;
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

#[derive(serde::Serialize)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub mtime: i64,
    pub kind: &'static str,
}

fn entry_kind(meta: &std::fs::Metadata) -> &'static str {
    let ft = meta.file_type();
    if ft.is_dir() {
        "dir"
    } else if ft.is_file() {
        "file"
    } else if ft.is_symlink() {
        "symlink"
    } else {
        "other"
    }
}

/// Directory listing, sorted dirs-first then by name (the shape the
/// dashboard's file browser renders).
pub fn list_dir(dir: &Path) -> std::io::Result<Vec<FileEntry>> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        // symlink_metadata so a symlink is reported AS a symlink instead of
        // silently followed into whatever it points at.
        let Ok(meta) = e.path().symlink_metadata() else { continue };
        out.push(FileEntry {
            name: e.file_name().to_string_lossy().into_owned(),
            size: meta.len(),
            mtime: meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            kind: entry_kind(&meta),
        });
    }
    out.sort_by(|a, b| {
        (b.kind == "dir")
            .cmp(&(a.kind == "dir"))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

async fn list(Query(q): Query<PathParam>) -> Response {
    let root = files_root();
    let dir = match resolve_existing(&root, &q.path) {
        Ok(d) => d,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    if !dir.is_dir() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "not a directory", "path": q.path }));
    }
    let entries = match crate::db::interactions::spawn_blocking(move || list_dir(&dir)).await {
        Ok(Ok(entries)) => entries,
        Ok(Err(e)) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
    };
    Json(json!({ "path": q.path, "entries": entries })).into_response()
}

async fn download(Query(q): Query<PathParam>) -> Response {
    let root = files_root();
    let file = match resolve_existing(&root, &q.path) {
        Ok(f) => f,
        Err(e) => return err(StatusCode::NOT_FOUND, json!({ "error": e })),
    };
    if !file.is_file() {
        return err(StatusCode::BAD_REQUEST, json!({ "error": "not a regular file", "path": q.path }));
    }
    match file.metadata() {
        Ok(m) if m.len() > MAX_BYTES as u64 => {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({
                    "error": format!(
                        "file is {} bytes; this endpoint buffers whole files and caps at {} bytes",
                        m.len(), MAX_BYTES
                    ),
                    "limit": MAX_BYTES,
                }),
            );
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
        _ => {}
    }
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    match tokio::fs::read(&file).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", name.replace('"', "_")),
                ),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
    }
}

async fn upload(Query(q): Query<PathParam>, body: Bytes) -> Response {
    if body.len() > MAX_BYTES {
        return err(
            StatusCode::PAYLOAD_TOO_LARGE,
            json!({ "error": format!("body is {} bytes; upload cap is {} bytes", body.len(), MAX_BYTES), "limit": MAX_BYTES }),
        );
    }
    let root = files_root();
    let rel = match validate_upload_rel(&q.path) {
        Ok(r) => r,
        Err(e) => return err(StatusCode::BAD_REQUEST, json!({ "error": e })),
    };
    let canon_root = match root.canonicalize() {
        Ok(r) => r,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
    };
    let target = canon_root.join(&rel);
    if let Some(parent) = target.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() }));
        }
        // Post-create containment check: the lexical filter above blocks
        // `..`, but a symlink INSIDE the root can still point outside it —
        // canonicalize the now-existing parent and re-verify.
        match parent.canonicalize() {
            Ok(real_parent) if real_parent.starts_with(&canon_root) => {}
            Ok(_) => {
                return err(
                    StatusCode::BAD_REQUEST,
                    json!({ "error": "upload path resolves outside the files root (symlink)" }),
                )
            }
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
        }
    }
    let size = body.len();
    match tokio::fs::write(&target, body).await {
        Ok(()) => Json(json!({ "ok": true, "path": rel.display().to_string(), "size": size })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": e.to_string() })),
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
            assert!(resolve_existing(dir.path(), "sneaky/hosts").is_err(), "symlink escape");
        }
        // ...while honest paths resolve.
        assert!(resolve_existing(dir.path(), "inside.txt").is_ok());
        assert!(resolve_existing(dir.path(), "").is_ok(), "empty path = root");
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

    #[test]
    fn dir_listing_shape() {
        let dir = root();
        std::fs::write(dir.path().join("b.txt"), "12345").unwrap();
        std::fs::create_dir(dir.path().join("adir")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("b.txt", dir.path().join("link")).unwrap();

        let entries = list_dir(dir.path()).unwrap();
        // dirs first, then names
        assert_eq!(entries[0].name, "adir");
        assert_eq!(entries[0].kind, "dir");
        let file = entries.iter().find(|e| e.name == "b.txt").unwrap();
        assert_eq!(file.kind, "file");
        assert_eq!(file.size, 5);
        assert!(file.mtime > 0, "mtime populated");
        #[cfg(unix)]
        {
            let link = entries.iter().find(|e| e.name == "link").unwrap();
            assert_eq!(link.kind, "symlink", "symlinks reported as symlinks, not followed");
        }
    }
}
