//! /api/file (+ /raw /vtt /prepare /transcode) + /api/library — the SPA's
//! file VIEWER + media pipeline, NATIVE (AMUX-2598 cutover of the
//! PROXIED_FAMILIES row "/api/file (+ /raw /prepare /transcode) +
//! /api/library").
//!
//! Ported from the deleted Python server (historical amux-server.py, deleted at 792ce1f; line refs are into git history):
//! - GET  /api/file (viewer payload)        py:67956-68067
//! - PUT  /api/file (text write-back)       py:67901-67935
//! - GET  /api/file/vtt (SRT → WebVTT)      py:67858-67877
//! - GET  /api/file/raw (range streaming)   py:68069-68140
//!   (`_media_keepalive` py:64934, `_stream_file_body` py:64951)
//! - GET  /api/file/prepare                 py:68146-68186
//!   (`_media_prepare_job` py:64543-64607, `_MEDIA_PREP_JOBS` py:64540)
//! - GET  /api/file/transcode               py:68188-68253
//! - GET  /api/library                      py:67940-67954
//!   (`_lib_index` py:655, `_lib_calibre` py:516, `_lib_opf_scan` py:589,
//!   `_lib_facets` py:634, `_LIB_EBOOK_EXTS`/`_LIB_FMT_RANK` py:507-508)
//! - image inline cap `_IMG_INLINE_MAX`     py:20623 (AMUX_IMG_INLINE_MAX)
//! - ebook sets `EBOOK_RENDERABLE` etc.     py:156-158
//!
//! Path guards are IMPORTED from api/fs.rs (`is_path_allowed`,
//! `is_dangerous_write`) — one port of the containment rules, not two.
//!
//! Deliberate differences from the python origin, all named:
//! - **Durable prepare jobs.** Python held remux job state in process memory,
//!   so a restart orphaned every in-flight job invisibly. Jobs now live in
//!   the shared DB (`_amux_media_jobs`, migration 0009): a 'running' row
//!   whose heartbeat went stale — or a 'done' row whose output file is gone —
//!   is detected at the next poll and restarted instead of being reported as
//!   progressing/ready forever. The cache KEY derivation is byte-identical to
//!   python's (sha1 of "path|mtime|size", first 24 hex), so copies already in
//!   ~/.amux/media-cache keep being found.
//! - **Ebook → HTML is an honest 501.** Python renders EPUB/FB2/CBZ/MOBI/AZW
//!   to inline HTML with ~320 lines of stdlib (zipfile+deflate, ElementTree,
//!   PalmDOC/MOBI record decoding). None of that is in this crate's
//!   dependency tree; a half-faithful re-render would fake success (ethos
//!   rule 3), so a renderable ebook answers 501 naming exactly what is
//!   missing. Non-renderable/oversize ebooks get the same download-card
//!   payload python serves.
//! - **Keep-alive is the transport default here.** Python had to hand-roll
//!   HTTP/1.1 keep-alive per media response (`_media_keepalive`, AMUX-1820:
//!   scrubbing = hundreds of Range requests, each paying TCP+TLS on a
//!   close-per-response server). axum/hyper connections are persistent by
//!   default, which IS that semantic; no per-response header dance exists to
//!   port.
//! - A malformed Range whose start lies past end streams 0 bytes with
//!   `Content-Length: 0`; python emits a NEGATIVE Content-Length there
//!   (68105: end-start+1 unchecked), which hyper cannot express. Sane
//!   requests are byte-identical.
//! - ffmpeg/ffprobe are found via ABSOLUTE candidate paths before $PATH:
//!   under launchd there is no shell PATH (the restic lesson), and python's
//!   shutil.which quietly fails there.

use super::fs::{
    expanduser, is_dangerous_write, is_path_allowed, j, mtime_secs, not_found, parse_body,
    parse_qs, pystr, qs_get,
};
use super::AppState;
use crate::db::WriteOutcome;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, RawQuery, Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use base64::Engine as _;
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use sha1::Digest as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub fn routes() -> Router<AppState> {
    Router::new()
        // Python routes on (method, path) pairs and falls through to its
        // generic 404 on a method mismatch — any() + in-handler checks keep
        // that contract (same convention as api/fs.rs).
        .route("/", any(file_root))
        .route("/raw", any(raw))
        .route("/xlsx", any(xlsx))
        .route("/vtt", any(vtt))
        .route("/prepare", any(prepare))
        .route("/transcode", any(transcode))
        .route("/{*rest}", any(not_found))
        // Python has no body cap on PUT /api/file; axum's 2MB default would
        // 413 a large markdown save the python origin accepts.
        .layer(DefaultBodyLimit::disable())
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// MIME tables, verbatim from the python handlers (py:67966-67981 viewer,
/// py:68082-68093 raw).
const IMAGE_MIMES: &[(&str, &str)] = &[
    (".png", "image/png"),
    (".jpg", "image/jpeg"),
    (".jpeg", "image/jpeg"),
    (".gif", "image/gif"),
    (".webp", "image/webp"),
    (".svg", "image/svg+xml"),
    (".bmp", "image/bmp"),
    (".ico", "image/x-icon"),
];
const VIDEO_MIMES: &[(&str, &str)] = &[
    (".mp4", "video/mp4"),
    (".mov", "video/quicktime"),
    (".webm", "video/webm"),
    (".avi", "video/x-msvideo"),
    (".mkv", "video/x-matroska"),
    (".m4v", "video/mp4"),
];
const AUDIO_MIMES: &[(&str, &str)] = &[
    (".mp3", "audio/mpeg"),
    (".wav", "audio/wav"),
    (".ogg", "audio/ogg"),
    (".m4a", "audio/mp4"),
    (".aac", "audio/aac"),
    (".flac", "audio/flac"),
];
const PDF_MIME: &[(&str, &str)] = &[(".pdf", "application/pdf")];

/// Text / web types the browser renders in place. Python's raw table
/// (py:68082-68093) had ONLY media + pdf and fell straight to octet-stream for
/// these, so navigating the dashboard's file viewer to an HTML report (or .svg,
/// .json, .csv) DOWNLOADED it and the page stayed about:blank instead of
/// rendering (amax-gtm, 2026-08-13, Wexus deliverable). This is a native
/// improvement over the deleted python contract, not a parity port. Code/markdown
/// go out as text/plain so a direct navigation shows the SOURCE rather than
/// letting a browser try to execute or reflow it; .html/.json/.csv get their
/// real types so they render. charset=utf-8 because bytes are served as-is.
const TEXT_MIMES: &[(&str, &str)] = &[
    (".html", "text/html; charset=utf-8"),
    (".htm", "text/html; charset=utf-8"),
    (".json", "application/json; charset=utf-8"),
    (".csv", "text/csv; charset=utf-8"),
    (".tsv", "text/tab-separated-values; charset=utf-8"),
    (".xml", "text/xml; charset=utf-8"),
    (".txt", "text/plain; charset=utf-8"),
    (".md", "text/plain; charset=utf-8"),
    (".markdown", "text/plain; charset=utf-8"),
    (".log", "text/plain; charset=utf-8"),
    (".yml", "text/plain; charset=utf-8"),
    (".yaml", "text/plain; charset=utf-8"),
    (".toml", "text/plain; charset=utf-8"),
    (".css", "text/plain; charset=utf-8"),
    (".js", "text/plain; charset=utf-8"),
];

/// Ebook sets (py:156-158).
const EBOOK_RENDERABLE: &[&str] = &[".epub", ".fb2", ".cbz", ".mobi", ".azw"];
const EBOOK_DOWNLOAD_ONLY: &[&str] = &[".azw3", ".azw4", ".kfx", ".cbr", ".djvu", ".lit", ".pdb"];

fn mime_of<'a>(table: &[(&'a str, &'a str)], ext: &str) -> Option<&'a str> {
    table.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

/// Python `Path.suffix.lower()`: extension of the final component,
/// dot-prefixed; leading-dot names (".gitignore") have NO suffix — Rust's
/// `extension()` agrees.
fn py_suffix(p: &Path) -> String {
    match p.extension() {
        Some(e) if !e.is_empty() => format!(".{}", e.to_string_lossy().to_lowercase()),
        _ => String::new(),
    }
}

/// `urllib.parse.quote` with the default safe set (`/` plus unreserved) —
/// what python uses for the viewer's raw_url (py:68009).
fn py_quote(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'.' | b'-' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The shared `path`+`cwd` resolution of every GET /api/file* handler
/// (py:67957-67964 et al): relative path + cwd joins; relative without cwd
/// is a 400; the path is used UNRESOLVED (only the containment check
/// resolves internally).
fn qpath(qs: &[(String, String)]) -> Result<PathBuf, Box<Response>> {
    let fpath = qs_get(qs, "path").unwrap_or("");
    let cwd = qs_get(qs, "cwd").unwrap_or("");
    if fpath.is_empty() {
        return Err(Box::new(j(400, json!({"error": "missing path"}))));
    }
    let p = expanduser(fpath);
    if p.is_absolute() {
        Ok(p)
    } else if !cwd.is_empty() {
        Ok(expanduser(cwd).join(p))
    } else {
        Err(Box::new(j(400, json!({"error": "relative path without cwd"}))))
    }
}

/// `_IMG_INLINE_MAX` (py:20623): inline an image as base64 up to this size,
/// stream anything larger via /api/file/raw. Config, not a constant — the
/// hard 5MB refusal it replaced is the AMUX-2344 incident.
fn img_inline_max() -> u64 {
    std::env::var("AMUX_IMG_INLINE_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(2_000_000)
}

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

// ---------------------------------------------------------------------------
// GET /api/file — viewer payload (py:67956-68067) · PUT — write-back
// ---------------------------------------------------------------------------

async fn file_root(state: State<AppState>, req: Request) -> Response {
    let _ = state; // reserved: keeps the handler signature uniform in this router
    match *req.method() {
        Method::GET => view(req).await,
        Method::PUT => put_file(req).await,
        _ => not_found().await,
    }
}

async fn view(req: Request) -> Response {
    let qs = parse_qs(req.uri().query().unwrap_or(""));
    let p = match qpath(&qs) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    if !is_path_allowed(&p) {
        return j(403, json!({"error": "access denied"}));
    }
    let meta = match std::fs::metadata(&p) {
        Ok(m) if m.is_file() => m,
        _ => return j(404, json!({"error": "file not found"})),
    };
    let ext = py_suffix(&p);

    if let Some(mime) = mime_of(IMAGE_MIMES, &ext) {
        // Large images STREAM instead of being refused (AMUX-2344): small
        // ones inline as a data_url (instant render + offline IDB cache),
        // larger get a raw_url the browser fetches itself.
        let sz = meta.len();
        let mut out = json!({"path": pystr(&p), "is_image": true, "mime": mime, "size": sz});
        if sz <= img_inline_max() {
            let bytes = match std::fs::read(&p) {
                Ok(b) => b,
                Err(e) => return j(500, json!({"error": e.to_string()})),
            };
            out["data_url"] = json!(format!("data:{mime};base64,{}", B64.encode(&bytes)));
        } else {
            out["raw_url"] = json!(format!("/api/file/raw?path={}", py_quote(&pystr(&p))));
        }
        return j(200, out);
    }

    if ext == ".pdf" {
        let raw = match std::fs::read(&p) {
            Ok(b) => b,
            Err(e) => return j(500, json!({"error": e.to_string()})),
        };
        if raw.len() > 10_000_000 {
            return j(400, json!({"error": "PDF too large (>10 MB)"}));
        }
        let data_url = format!("data:application/pdf;base64,{}", B64.encode(&raw));
        return j(200, json!({"path": pystr(&p), "is_pdf": true, "data_url": data_url}));
    }

    if let Some(mime) = mime_of(VIDEO_MIMES, &ext) {
        let srt = p.with_extension("srt");
        let meta_file = p.with_extension("json");
        let side = std::fs::read_to_string(&meta_file)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .unwrap_or(Value::Null);
        return j(
            200,
            json!({
                "path": pystr(&p), "is_video": true, "mime": mime,
                "size": meta.len(), "modified": mtime_secs(&meta),
                "srt": if srt.exists() { json!(pystr(&srt)) } else { Value::Null },
                "profile": side.get("profile").cloned().unwrap_or(Value::Null),
                "task": side.get("task").cloned().unwrap_or(Value::Null),
            }),
        );
    }

    if let Some(mime) = mime_of(AUDIO_MIMES, &ext) {
        return j(
            200,
            json!({"path": pystr(&p), "is_audio": true, "mime": mime, "size": meta.len()}),
        );
    }

    let renderable = EBOOK_RENDERABLE.contains(&ext.as_str());
    if renderable || EBOOK_DOWNLOAD_ONLY.contains(&ext.as_str()) {
        let fsize = meta.len();
        let kind = ext.trim_start_matches('.');
        let cap: u64 = if ext == ".cbz" { 60_000_000 } else { 25_000_000 };
        if renderable && fsize <= cap {
            // Python renders these to inline HTML (`ebook_to_html`, py:491)
            // with stdlib zip/deflate + ElementTree + MOBI/PalmDOC decoding.
            // No rust port exists; faking a download card here would silently
            // degrade a book python renders, so: honest 501 naming the gap.
            return j(
                501,
                json!({
                    "error": "ebook rendering not implemented on this origin: \
                              EPUB/FB2/CBZ/MOBI/AZW → HTML conversion runs on \
                              python stdlib (zipfile/deflate, ElementTree XML, \
                              PalmDOC/MOBI record decoding) and has no rust port \
                              yet — the python origin renders these",
                    "path": pystr(&p), "is_ebook": true, "ebook_kind": kind, "size": fsize,
                }),
            );
        }
        // Too big or proprietary → download card, same shape as python.
        return j(
            200,
            json!({
                "path": pystr(&p), "is_binary": true, "is_ebook": true,
                "ebook_kind": kind, "size": fsize, "ext": ext,
            }),
        );
    }

    // Binary sniff: NUL byte in the first 8KB (py:68041).
    {
        use std::io::Read;
        let mut sample = [0u8; 8192];
        if let Ok(mut f) = std::fs::File::open(&p) {
            let mut got = 0usize;
            while got < sample.len() {
                match f.read(&mut sample[got..]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got += n,
                }
            }
            if sample[..got].contains(&0) {
                return j(
                    200,
                    json!({"path": pystr(&p), "is_binary": true, "size": meta.len(), "ext": ext}),
                );
            }
        }
    }

    // Text. Python read_text(errors="replace") vs from_utf8_lossy: both
    // substitute U+FFFD; python replaces per-byte where lossy replaces
    // per-sequence — divergence only inside already-mangled bytes.
    let raw = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let mut content = String::from_utf8_lossy(&raw).into_owned();
    let is_md = matches!(ext.as_str(), ".md" | ".markdown" | ".mdx");
    let is_csv = matches!(ext.as_str(), ".csv" | ".tsv");
    let is_html = matches!(ext.as_str(), ".html" | ".htm");
    // Python limits are in CHARACTERS (str slicing), not bytes.
    let limit: usize = if is_csv { 5_000_000 } else { 200_000 };
    if content.chars().count() > limit {
        let cut = content.char_indices().nth(limit).map(|(i, _)| i).unwrap_or(content.len());
        content.truncate(cut);
        content.push_str(if is_csv {
            "\n... (truncated at 5MB)"
        } else {
            "\n\n... (truncated at 200KB)"
        });
    }
    j(
        200,
        json!({
            "path": pystr(&p), "content": content,
            "is_markdown": is_md, "is_csv": is_csv, "is_html": is_html,
        }),
    )
}

/// Python's writable-extension allowlist for PUT /api/file (py:67909-67918).
/// Entries like ".env"/".gitignore" are reachable only as real suffixes
/// ("foo.env") — a FILE named ".env" has no suffix and passes the
/// extensionless branch, exactly as in python.
const WRITABLE_EXTS: &[&str] = &[
    ".md", ".markdown", ".mdx", ".mdai", ".txt", ".json", ".yml", ".yaml", ".toml", ".ini", ".cfg", ".sh",
    ".bash", ".zsh", ".py", ".js", ".ts", ".jsx", ".tsx", ".mjs", ".cjs", ".css", ".scss",
    ".less", ".html", ".htm", ".xml", ".svg", ".csv", ".sql", ".graphql", ".proto", ".go", ".rs",
    ".java", ".rb", ".php", ".swift", ".kt", ".c", ".cpp", ".h", ".cs", ".r", ".lua", ".pl",
    ".env", ".gitignore", ".dockerignore", ".tf", ".hcl", ".conf", ".log", ".makefile",
];

async fn put_file(req: Request) -> Response {
    let bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let body = match parse_body(&bytes) {
        Ok(v) => v,
        Err(e) => return j(500, json!({"error": e})),
    };
    let fpath = body.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if fpath.is_empty() {
        return j(400, json!({"error": "missing path"}));
    }
    let p = expanduser(fpath);
    if !p.is_absolute() {
        return j(400, json!({"error": "absolute path required"}));
    }
    if !is_path_allowed(&p) {
        return j(403, json!({"error": "access denied"}));
    }
    let ext = py_suffix(&p);
    if !ext.is_empty() && !WRITABLE_EXTS.contains(&ext.as_str()) {
        return j(400, json!({"error": format!("file type not writable: {ext}")}));
    }
    // Code-execution-vector writes are refused even when extensionless
    // (shell rc files, launch agents, git hooks) — py:67924.
    if is_dangerous_write(&p) {
        return j(403, json!({"error": "refused: writing this file could execute code"}));
    }
    if let Some(parent) = p.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return j(500, json!({"error": e.to_string()}));
        }
    }
    let content = body.get("content").and_then(|v| v.as_str()).unwrap_or("");
    match std::fs::write(&p, content) {
        Ok(()) => j(200, json!({"ok": true, "path": pystr(&p)})),
        Err(e) => j(500, json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// GET /api/file/vtt — SRT → WebVTT for <track> elements (py:67858-67877)
// ---------------------------------------------------------------------------

/// GET /api/file/xlsx?path=... — parse a spreadsheet (.xlsx/.xls/.ods) and return
/// its sheets as JSON rows, so the Files viewer renders it as a table instead of
/// forcing a download (AMUX-3343). Caps sheets/rows/cols/cell-length so a huge
/// workbook cannot blow the payload; `truncated` says when a cap was hit. Parsing
/// is blocking IO, so it runs on a blocking thread.
async fn xlsx(req: Request) -> Response {
    use calamine::{open_workbook_auto, DataType, Reader};
    if req.method() != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(req.uri().query().unwrap_or(""));
    let p = match qpath(&qs) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    if !is_path_allowed(&p) {
        return j(403, json!({"error": "access denied"}));
    }
    if !std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false) {
        return j(404, json!({"error": "file not found"}));
    }
    const MAX_SHEETS: usize = 30;
    const MAX_ROWS: usize = 1000;
    const MAX_COLS: usize = 60;
    const MAX_CELL: usize = 500;
    let parsed = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, String> {
        let mut wb = open_workbook_auto(&p).map_err(|e| e.to_string())?;
        let names = wb.sheet_names().to_owned();
        let mut truncated = names.len() > MAX_SHEETS;
        let mut sheets = Vec::new();
        for name in names.into_iter().take(MAX_SHEETS) {
            let range = match wb.worksheet_range(&name) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if range.rows().count() > MAX_ROWS {
                truncated = true;
            }
            let mut rows = Vec::new();
            for row in range.rows().take(MAX_ROWS) {
                if row.len() > MAX_COLS {
                    truncated = true;
                }
                let cells: Vec<String> = row
                    .iter()
                    .take(MAX_COLS)
                    .map(|c| {
                        let s = if c.is_empty() { String::new() } else { c.to_string() };
                        if s.chars().count() > MAX_CELL {
                            s.chars().take(MAX_CELL).collect::<String>() + "…"
                        } else {
                            s
                        }
                    })
                    .collect();
                rows.push(cells);
            }
            sheets.push(json!({ "name": name, "rows": rows }));
        }
        Ok(json!({ "sheets": sheets, "truncated": truncated }))
    })
    .await;
    match parsed {
        Ok(Ok(v)) => j(200, v),
        Ok(Err(e)) => j(422, json!({"error": format!("could not parse spreadsheet: {e}")})),
        Err(e) => j(500, json!({"error": format!("parse task failed: {e}")})),
    }
}

async fn vtt(method: Method, RawQuery(q): RawQuery) -> Response {
    if method != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(q.as_deref().unwrap_or(""));
    let srt_p = qs_get(&qs, "path").unwrap_or("");
    if srt_p.is_empty() {
        return j(400, json!({"error": "missing path"}));
    }
    let srt_file = expanduser(srt_p);
    if !is_path_allowed(&srt_file) {
        return j(403, json!({"error": "access denied"}));
    }
    let raw = match std::fs::metadata(&srt_file) {
        Ok(m) if m.is_file() => std::fs::read(&srt_file),
        _ => return j(404, json!({"error": "not found"})),
    };
    let raw = match raw {
        Ok(b) => b,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let srt_text = String::from_utf8_lossy(&raw);
    static TS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = TS.get_or_init(|| {
        regex::Regex::new(r"(\d{2}:\d{2}:\d{2}),(\d{3})").expect("vtt timestamp regex")
    });
    let body = format!("WEBVTT\n\n{}", re.replace_all(&srt_text, "$1.$2"));
    (
        StatusCode::OK,
        [
            ("content-type", "text/vtt; charset=utf-8"),
            ("cache-control", "no-store"),
        ],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /api/file/raw — range streaming (py:68069-68140)
// ---------------------------------------------------------------------------

/// The union MIME table of the raw endpoint. Extended beyond python's
/// media-only table (py:68082-68093) with TEXT_MIMES so renderable files serve
/// with a real content-type instead of octet-stream (amax-gtm, 2026-08-13);
/// octet-stream stays the fallback for genuine binaries only.
fn raw_mime(ext: &str) -> &'static str {
    mime_of(IMAGE_MIMES, ext)
        .or_else(|| mime_of(PDF_MIME, ext))
        .or_else(|| mime_of(VIDEO_MIMES, ext))
        .or_else(|| mime_of(AUDIO_MIMES, ext))
        .or_else(|| mime_of(TEXT_MIMES, ext))
        .unwrap_or("application/octet-stream")
}

/// Stream `length` bytes of `path` from `start` in 1MB chunks. Player aborts
/// (seeks, quality probes, teardown) just end the stream quietly — python's
/// `_stream_file_body` contract.
fn stream_file(path: PathBuf, start: u64, length: u64) -> Body {
    struct St {
        path: PathBuf,
        start: u64,
        remaining: u64,
        file: Option<tokio::fs::File>,
    }
    let init = St { path, start, remaining: length, file: None };
    let stream = futures::stream::unfold(init, |mut st| async move {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        if st.remaining == 0 {
            return None;
        }
        if st.file.is_none() {
            let mut f = tokio::fs::File::open(&st.path).await.ok()?;
            f.seek(std::io::SeekFrom::Start(st.start)).await.ok()?;
            st.file = Some(f);
        }
        let want = st.remaining.min(1 << 20) as usize;
        let mut buf = vec![0u8; want];
        match st.file.as_mut().expect("file opened above").read(&mut buf).await {
            Ok(0) | Err(_) => None, // early EOF / client teardown: end quietly
            Ok(n) => {
                buf.truncate(n);
                st.remaining -= n as u64;
                Some((Ok::<_, std::io::Error>(Bytes::from(buf)), st))
            }
        }
    });
    Body::from_stream(stream)
}

async fn raw(req: Request) -> Response {
    if req.method() != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(req.uri().query().unwrap_or(""));
    let p = match qpath(&qs) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    if !is_path_allowed(&p) {
        return j(403, json!({"error": "access denied"}));
    }
    let meta = match std::fs::metadata(&p) {
        Ok(m) if m.is_file() => m,
        _ => return j(404, json!({"error": "file not found"})),
    };
    let mime = raw_mime(&py_suffix(&p));
    let file_size = meta.len();
    // ETag from mtime+size (py:68097) — If-None-Match short-circuits to 304.
    let etag = format!("\"{}-{}\"", mtime_secs(&meta), file_size);
    if req.headers().get("if-none-match").and_then(|v| v.to_str().ok()) == Some(etag.as_str()) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .body(Body::empty())
            .expect("static 304");
    }
    // Render in place for anything the browser can display; force download only
    // for true binaries — or when ?download=1 is explicitly requested. Before
    // this, everything except video/audio got `attachment`, so an HTML report,
    // image, or PDF served by amux DOWNLOADED instead of rendering and the page
    // stayed about:blank (amax-gtm, 2026-08-13).
    let kind = mime.split('/').next().unwrap_or("");
    let force_dl = matches!(qs_get(&qs, "download").unwrap_or(""), "1" | "true" | "yes");
    let renderable = matches!(kind, "video" | "audio" | "image" | "text")
        || mime == "application/pdf"
        || mime.starts_with("application/json");
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    let range_header =
        req.headers().get("range").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    // Content-Disposition is a DOWNLOAD directive — emit it ONLY to force a
    // download (attachment). For an inline render we send NO disposition at all:
    // a `Content-Disposition` header (even `inline`, even one that only carries a
    // filename=) sends Chrome-over-CDP down a download path on navigation, which
    // commits the tab to a download and leaves it documentless — body null,
    // ready_state stuck "loading", Page.captureScreenshot times out (amux-gtm,
    // 2026-08-13, verified: same bytes render over file:// but not via this route
    // until the header was dropped). With no disposition present the browser
    // chooses render-vs-download from Content-Type alone, which is exactly what a
    // renderable type wants. Streamable media stays inline the same way (py:68103
    // wanted inline so iOS Safari streams a 3GB video instead of downloading it).
    let mut common: Vec<(&str, String)> = vec![
        ("content-type", mime.to_string()),
        ("accept-ranges", "bytes".to_string()),
        ("etag", etag),
        ("cache-control", "private, max-age=3600, immutable".to_string()),
    ];
    if force_dl || !renderable {
        common.push(("content-disposition", format!("attachment; filename=\"{name}\"")));
    }
    if !range_header.is_empty() {
        // Python `re.match(r'bytes=(\d*)-(\d*)')`: absent groups default to
        // 0 / EOF; an UNPARSABLE Range still answers 206 over the full file
        // (m is None → the same defaults). Suffix ranges ("bytes=-500") are
        // therefore full-file-from-0 — python's (non-RFC) behavior, kept.
        static RANGE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = RANGE_RE
            .get_or_init(|| regex::Regex::new(r"^bytes=(\d*)-(\d*)").expect("range regex"));
        let (mut start, mut end): (i128, i128) = (0, file_size as i128 - 1);
        if let Some(c) = re.captures(&range_header) {
            if let Ok(v) = c[1].parse::<i128>() {
                start = v;
            }
            if let Ok(v) = c[2].parse::<i128>() {
                end = v;
            }
        }
        end = end.min(file_size as i128 - 1);
        let length = (end - start + 1).max(0) as u64;
        let mut b = Response::builder().status(StatusCode::PARTIAL_CONTENT);
        for (k, v) in common {
            b = b.header(k, v);
        }
        b.header("content-range", format!("bytes {start}-{end}/{file_size}"))
            .header("content-length", length.to_string())
            .body(stream_file(p, start.max(0) as u64, length))
            .expect("206 response")
    } else {
        // Non-range full-file. Serve small files IN-MEMORY rather than streamed:
        // a streamed Body::from_stream over HTTP/2 leaves Chrome-over-CDP
        // navigation stuck at ready_state="loading" — the body commits but never
        // signals completion, so captureScreenshot times out at 30s and the tab
        // renders nothing (amux-gtm, 2026-08-13: the identical bytes render over
        // `python -m http.server` and via the dashboard's own in-memory responses,
        // and fail ONLY on this streamed route; header delta ruled out). Large
        // files (media fetched WITHOUT a Range) stay streamed so a 3GB video is
        // never read into RAM — players always send a Range and take the 206 path
        // above, so they never reach here.
        const INMEM_MAX: u64 = 16 * 1024 * 1024;
        let mut b = Response::builder().status(StatusCode::OK);
        for (k, v) in common {
            b = b.header(k, v);
        }
        b = b.header("content-length", file_size.to_string());
        if file_size <= INMEM_MAX {
            match std::fs::read(&p) {
                Ok(bytes) => b.body(Body::from(bytes)).expect("200 response"),
                Err(e) => j(500, json!({ "error": e.to_string() })),
            }
        } else {
            b.body(stream_file(p, 0, file_size)).expect("200 response")
        }
    }
}

// ---------------------------------------------------------------------------
// ffmpeg plumbing shared by prepare + transcode
// ---------------------------------------------------------------------------

/// Absolute candidates FIRST — launchd has no shell PATH, so a bare `which`
/// lookup reports ffmpeg missing on the machine it is installed on — then a
/// $PATH scan for everything else.
fn find_bin(name: &str) -> Option<PathBuf> {
    for d in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/opt/local/bin"] {
        let c = Path::new(d).join(name);
        if c.is_file() {
            return Some(c);
        }
    }
    for d in std::env::var("PATH").unwrap_or_default().split(':').filter(|s| !s.is_empty()) {
        let c = Path::new(d).join(name);
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

/// Best-effort free-disk probe via `df -Pk` (std has no statvfs; python wraps
/// its check in try/except the same way — a probe failure skips the check,
/// never fails the request).
fn disk_free_bytes(path: &Path) -> Option<u64> {
    let df = ["/bin/df", "/usr/bin/df"].iter().find(|c| Path::new(c).is_file())?;
    let out = std::process::Command::new(df).arg("-Pk").arg(path).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let last = text.lines().last()?;
    let avail_kb: u64 = last.split_whitespace().nth(3)?.parse().ok()?;
    Some(avail_kb * 1024)
}

/// `ffprobe` one file → (duration_secs, vcodec, acodec). Any failure is
/// (0.0, "", "") — python's try/except pass.
async fn probe_media(ffprobe: &Path, src: &Path) -> (f64, String, String) {
    let ran = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(ffprobe)
            .args([
                "-v",
                "quiet",
                "-show_entries",
                "format=duration:stream=codec_type,codec_name",
                "-of",
                "json",
            ])
            .arg(src)
            .output(),
    )
    .await;
    let Ok(Ok(out)) = ran else { return (0.0, String::new(), String::new()) };
    let info: Value = serde_json::from_slice(&out.stdout).unwrap_or(json!({}));
    let dur = info["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0);
    let mut vcodec = String::new();
    let mut acodec = String::new();
    for s in info["streams"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
        let codec = s["codec_name"].as_str().unwrap_or("").to_string();
        match s["codec_type"].as_str() {
            Some("video") if vcodec.is_empty() => vcodec = codec,
            Some("audio") if acodec.is_empty() => acodec = codec,
            _ => {}
        }
    }
    (dur, vcodec, acodec)
}

// ---------------------------------------------------------------------------
// GET /api/file/prepare — background remux to a SEEKABLE on-disk MP4
// (py:68146-68186; job body py:64543-64607). iOS AVPlayer never starts on the
// live /transcode pipe (no Content-Length, no ranges); the prepared copy
// streams through /api/file/raw instead.
// ---------------------------------------------------------------------------

/// ~/.amux/media-cache, python's hardcoded location — env-overridable here
/// (config, not constant) and test-overridable for hermeticity.
fn media_cache_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(d) = tests::MEDIA_CACHE_OVERRIDE.lock().expect("cache override").clone() {
        return d;
    }
    if let Ok(v) = std::env::var("AMUX_MEDIA_CACHE_DIR") {
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux/media-cache")
}

/// Python's cache key, byte-identical: sha1("{path}|{mtime}|{size}")[:24]
/// (py:68158) — existing prepared copies must keep being found.
fn media_key(p: &Path, meta: &std::fs::Metadata) -> String {
    let mut h = sha1::Sha1::new();
    h.update(format!("{}|{}|{}", pystr(p), mtime_secs(meta), meta.len()).as_bytes());
    hex::encode(h.finalize())[..24].to_string()
}

/// A 'running' row whose heartbeat is older than this is an orphan (the
/// server restarted mid-job, or the job task died). ffmpeg's `-progress`
/// ticks land ~1/s while a job is alive, so 60s of silence discriminates
/// dead from slow.
const JOB_STALE_S: i64 = 60;

#[derive(Debug, Clone)]
struct JobRow {
    status: String,
    progress: f64,
    error: String,
    updated_at: i64,
}

/// The poll decision, PURE so the restart/orphan logic is testable without
/// ffmpeg. The claim-write re-applies the same predicate transactionally.
#[derive(Debug, PartialEq)]
enum PrepDecision {
    Ready,
    Progress(f64),
    /// Error is reported ONCE and the row cleared so the next poll retries
    /// (python pops the job dict entry the same way, py:68166).
    ErrorOnce(String),
    StartNew,
}

fn prep_decision(row: Option<&JobRow>, out_exists: bool, now: i64) -> PrepDecision {
    match row {
        Some(r) if r.status == "error" => PrepDecision::ErrorOnce(r.error.clone()),
        Some(r) if r.status == "done" => {
            if out_exists {
                PrepDecision::Ready
            } else {
                // Durable 'done' pointing at a pruned/lost file must not
                // answer ready forever — restart honestly.
                PrepDecision::StartNew
            }
        }
        Some(r) if r.status == "running" => {
            if now - r.updated_at > JOB_STALE_S {
                PrepDecision::StartNew // orphan: heartbeat ceased
            } else {
                PrepDecision::Progress((r.progress * 10.0).round() / 10.0)
            }
        }
        Some(_) => PrepDecision::StartNew, // unknown status: treat as orphan
        None => {
            if out_exists {
                PrepDecision::Ready
            } else {
                PrepDecision::StartNew
            }
        }
    }
}

fn read_job(state: &AppState, key: &str) -> Option<JobRow> {
    let conn = state.store.read().ok()?;
    conn.query_row(
        "SELECT status, progress, error, updated_at FROM _amux_media_jobs WHERE key = ?1",
        [key],
        |r| {
            Ok(JobRow {
                status: r.get(0)?,
                progress: r.get(1)?,
                error: r.get(2)?,
                updated_at: r.get(3)?,
            })
        },
    )
    .optional()
    .ok()
    .flatten()
}

/// Job-table writes report `applied: false` on purpose: the table is POLLED
/// by /api/file/prepare, not event-synced, and a per-second progress tick
/// bumping the fleet-wide revision would wake every SSE client for the
/// duration of an encode.
async fn job_write(
    state: &AppState,
    f: impl FnOnce(&rusqlite::Connection) -> rusqlite::Result<()> + Send + 'static,
) {
    let res = state
        .store
        .write_async(move |conn| {
            f(conn)?;
            Ok(WriteOutcome { applied: false, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        tracing::warn!("media job write failed: {e}");
    }
}

async fn prepare(State(state): State<AppState>, req: Request) -> Response {
    if req.method() != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(req.uri().query().unwrap_or(""));
    let p = match qpath(&qs) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    if !is_path_allowed(&p) {
        return j(403, json!({"error": "access denied"}));
    }
    let meta = match std::fs::metadata(&p) {
        Ok(m) if m.is_file() => m,
        _ => return j(404, json!({"error": "file not found"})),
    };
    // prepare is a VIDEO remux-for-iOS path. Feeding it a non-video (xlsx, pdf,
    // zip, docx) sent it straight to ffmpeg, which failed with "Invalid data
    // found when processing input" — a parse error that reads like a corrupt
    // file rather than "this type can't be prepared" (amax-gtm, 2026-08-13).
    // Gate on real video extensions and answer honestly for everything else.
    // The WARN is the log signal (two-fixes rule): the next time prepare is
    // called on a type it can never handle, a log sweep / /api/logs sees it
    // instead of an opaque ffmpeg exit code.
    let ext = py_suffix(&p);
    if mime_of(VIDEO_MIMES, &ext).is_none() {
        tracing::warn!(
            "[media-prep] prepare called on non-video {} (ext '{}') — unsupported, not sent to ffmpeg",
            pystr(&p), ext
        );
        return j(
            200,
            json!({
                "ready": false,
                "reason": "unsupported type",
                "detail": format!(
                    "prepare only remuxes video; '{ext}' cannot be prepared. \
                     View it directly via /api/file/raw."
                ),
                "ext": ext,
            }),
        );
    }
    let Some(ffmpeg) = find_bin("ffmpeg") else {
        return j(500, json!({"error": "ffmpeg not installed"}));
    };
    let key = media_key(&p, &meta);
    let out = media_cache_dir().join(format!("{key}.mp4"));

    let now = chrono::Utc::now().timestamp();
    match prep_decision(read_job(&state, &key).as_ref(), out.exists(), now) {
        PrepDecision::Ready => j(200, json!({"ready": true, "cached_path": pystr(&out)})),
        PrepDecision::Progress(pr) => j(200, json!({"ready": false, "progress": pr})),
        PrepDecision::ErrorOnce(e) => {
            let k = key.clone();
            job_write(&state, move |conn| {
                conn.execute("DELETE FROM _amux_media_jobs WHERE key = ?1", [&k])?;
                Ok(())
            })
            .await;
            j(200, json!({"ready": false, "error": e}))
        }
        PrepDecision::StartNew => {
            // A remux lands at roughly input size — need that free, plus
            // slack (py:68177). Probe failure skips the check, like python's
            // try/except.
            let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
            if let Some(free) = disk_free_bytes(&home) {
                if free < meta.len() + meta.len() / 5 {
                    return j(507, json!({"error": "not enough free disk for prepared copy"}));
                }
            }
            // Claim atomically on the writer thread: the same staleness
            // predicate as prep_decision, re-checked transactionally so two
            // concurrent polls spawn one job (python's _media_prep_lock).
            let claimed = Arc::new(AtomicBool::new(false));
            let (c2, k2) = (claimed.clone(), key.clone());
            let (src_s, out_s) = (pystr(&p), pystr(&out));
            state
                .store
                .write_async(move |conn| {
                    let existing: Option<(String, i64)> = conn
                        .query_row(
                            "SELECT status, updated_at FROM _amux_media_jobs WHERE key = ?1",
                            [&k2],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    let now = chrono::Utc::now().timestamp();
                    let take = match existing {
                        None => true,
                        Some((st, up)) => st != "running" || now - up > JOB_STALE_S,
                    };
                    if take {
                        conn.execute("DELETE FROM _amux_media_jobs WHERE key = ?1", [&k2])?;
                        conn.execute(
                            "INSERT INTO _amux_media_jobs \
                             (key, src_path, out_path, status, progress, error, created_at, updated_at) \
                             VALUES (?1, ?2, ?3, 'running', 0, '', ?4, ?4)",
                            rusqlite::params![k2, src_s, out_s, now],
                        )?;
                        c2.store(true, Ordering::SeqCst);
                    }
                    Ok(WriteOutcome { applied: false, events: vec![] })
                })
                .await
                .ok();
            if !claimed.load(Ordering::SeqCst) {
                // Someone else claimed between our read and write.
                return j(200, json!({"ready": false, "progress": 0.0}));
            }
            tokio::spawn(run_prepare(state.clone(), ffmpeg, p, out, key));
            j(200, json!({"ready": false, "progress": 0.0, "started": true}))
        }
    }
}

/// The remux job (python `_media_prepare_job`, py:64543): stream-copy when
/// codecs are iOS-playable (HEVC needs the hvc1 tag or AVPlayer rejects it),
/// else transcode via the videotoolbox hardware encoder — python pins the
/// same macOS-specific encoder; parity kept deliberately.
async fn run_prepare(state: AppState, ffmpeg: PathBuf, src: PathBuf, out: PathBuf, key: String) {
    let src_name = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    // Per-attempt tmp name (pid-tagged): an ffmpeg orphaned by a previous
    // server process may still be writing ITS tmp; unique names keep a
    // restarted job from colliding with it.
    let tmp = out.with_file_name(format!(
        "{}.{}.part.mp4",
        out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        std::process::id()
    ));
    match do_prepare(&state, &ffmpeg, &src, &out, &tmp, &key).await {
        Ok(()) => {
            let sz = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
            tracing::info!("[media-prep] {src_name} → {} ready ({sz} bytes)",
                out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        }
        Err(e) => {
            let msg: String = e.chars().take(300).collect(); // python str(e)[:300]
            tracing::warn!("[media-prep] {src_name} FAILED: {msg}");
            let k = key.clone();
            job_write(&state, move |conn| {
                conn.execute(
                    "UPDATE _amux_media_jobs SET status='error', error=?2, updated_at=?3 \
                     WHERE key = ?1",
                    rusqlite::params![k, msg, chrono::Utc::now().timestamp()],
                )?;
                Ok(())
            })
            .await;
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }
}

async fn do_prepare(
    state: &AppState,
    ffmpeg: &Path,
    src: &Path,
    out: &Path,
    tmp: &Path,
    key: &str,
) -> Result<(), String> {
    let cache = out.parent().ok_or("cache dir has no parent")?;
    std::fs::create_dir_all(cache).map_err(|e| e.to_string())?;
    prune_cache(cache);

    let (dur, vcodec, acodec) = match find_bin("ffprobe") {
        Some(fp) => probe_media(&fp, src).await,
        None => (0.0, String::new(), String::new()),
    };
    let vcopy = matches!(vcodec.as_str(), "h264" | "hevc");
    let acopy = matches!(acodec.as_str(), "aac" | "mp3");
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.arg("-y").arg("-i").arg(src).args(["-map", "0:v:0", "-map", "0:a:0?"]);
    cmd.args(["-c:v", if vcopy { "copy" } else { "h264_videotoolbox" }]);
    if vcopy && vcodec == "hevc" {
        cmd.args(["-tag:v", "hvc1"]);
    }
    if !vcopy {
        cmd.args(["-b:v", "6000k"]);
    }
    cmd.args(["-c:a", if acopy { "copy" } else { "aac" }]);
    if !acopy {
        cmd.args(["-b:a", "192k", "-ac", "2"]);
    }
    cmd.args(["-movflags", "+faststart", "-f", "mp4", "-progress", "pipe:1", "-nostats",
        "-loglevel", "error"])
        .arg(tmp);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    tracing::info!(
        "[media-prep] {}: v={vcodec}{} a={acodec}{}",
        src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        if vcopy { "(copy)" } else { "→h264" },
        if acopy { "(copy)" } else { "→aac" },
    );
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg spawn: {e}"))?;
    if let Some(pid) = child.id() {
        let k = key.to_string();
        job_write(state, move |conn| {
            conn.execute(
                "UPDATE _amux_media_jobs SET pid=?2, updated_at=?3 WHERE key = ?1",
                rusqlite::params![k, pid as i64, chrono::Utc::now().timestamp()],
            )?;
            Ok(())
        })
        .await;
    }
    // Drain stderr concurrently so a chatty error can never fill the pipe
    // and wedge ffmpeg.
    let stderr = child.stderr.take();
    let err_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut e) = stderr {
            use tokio::io::AsyncReadExt;
            let _ = e.read_to_string(&mut buf).await;
        }
        buf
    });
    // Every `out_time_us=` tick is ALSO the heartbeat: progress may stay 0
    // when the probe found no duration, but updated_at must keep moving or
    // the poll declares this job an orphan.
    let stdout = child.stdout.take().ok_or("ffmpeg stdout not piped")?;
    let mut lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout));
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(us) = line.strip_prefix("out_time_us=") {
            let progress = match (us.parse::<f64>(), dur > 0.0) {
                // Caps at 99 — the faststart moov-relocation pass runs after.
                (Ok(us), true) => (us / 1e6 / dur * 100.0).min(99.0),
                _ => -1.0,
            };
            let k = key.to_string();
            job_write(state, move |conn| {
                if progress >= 0.0 {
                    conn.execute(
                        "UPDATE _amux_media_jobs SET progress=?2, updated_at=?3 WHERE key = ?1",
                        rusqlite::params![k, progress, chrono::Utc::now().timestamp()],
                    )?;
                } else {
                    conn.execute(
                        "UPDATE _amux_media_jobs SET updated_at=?2 WHERE key = ?1",
                        rusqlite::params![k, chrono::Utc::now().timestamp()],
                    )?;
                }
                Ok(())
            })
            .await;
        }
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    let stderr_text = err_task.await.unwrap_or_default();
    if !status.success() {
        let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
        let snippet: String = stderr_text.chars().take(300).collect();
        return Err(format!("ffmpeg exit {code}: {snippet}"));
    }
    tokio::fs::rename(tmp, out).await.map_err(|e| e.to_string())?;
    let k = key.to_string();
    job_write(state, move |conn| {
        conn.execute(
            "UPDATE _amux_media_jobs SET status='done', progress=100, updated_at=?2 \
             WHERE key = ?1",
            rusqlite::params![k, chrono::Utc::now().timestamp()],
        )?;
        Ok(())
    })
    .await;
    Ok(())
}

/// Prune prepared copies idle >30 days (atime, python parity) and abandoned
/// .part.mp4 tmp files older than a day (orphaned writers from dead server
/// processes — the durable-jobs analogue of python's tmp unlink).
fn prune_cache(cache: &Path) {
    let now = std::time::SystemTime::now();
    let Ok(rd) = std::fs::read_dir(cache) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(md) = e.metadata() else { continue };
        let idle = |t: std::io::Result<std::time::SystemTime>| {
            t.ok().and_then(|t| now.duration_since(t).ok()).map(|d| d.as_secs()).unwrap_or(0)
        };
        let stale = if name.ends_with(".part.mp4") {
            idle(md.modified()) > 86_400
        } else if name.ends_with(".mp4") {
            idle(md.accessed()) > 30 * 86_400
        } else {
            false
        };
        if stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/file/transcode — live remux MKV/AVI → fragmented MP4 over a
// chunked pipe (py:68188-68253). hyper chunk-encodes a streaming body with
// no content-length itself — python set Transfer-Encoding by hand.
// ---------------------------------------------------------------------------

async fn transcode(req: Request) -> Response {
    if req.method() != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(req.uri().query().unwrap_or(""));
    let p = match qpath(&qs) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    if !is_path_allowed(&p) {
        return j(403, json!({"error": "access denied"}));
    }
    if !std::fs::metadata(&p).map(|m| m.is_file()).unwrap_or(false) {
        return j(404, json!({"error": "file not found"}));
    }
    let Some(ffmpeg) = find_bin("ffmpeg") else {
        return j(500, json!({"error": "ffmpeg not installed"}));
    };
    // Probe codecs to decide remux (stream copy) vs full transcode.
    let (mut vcodec, mut acodec) = (String::from("unknown"), String::from("unknown"));
    if let Some(ffprobe) = find_bin("ffprobe") {
        for (sel, slot) in [("v:0", &mut vcodec), ("a:0", &mut acodec)] {
            let ran = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::process::Command::new(&ffprobe)
                    .args(["-v", "quiet", "-select_streams", sel, "-show_entries",
                        "stream=codec_name", "-of", "csv=p=0"])
                    .arg(&p)
                    .output(),
            )
            .await;
            if let Ok(Ok(out)) = ran {
                let first =
                    String::from_utf8_lossy(&out.stdout).trim().lines().next().unwrap_or("")
                        .to_string();
                if !first.is_empty() {
                    *slot = first;
                }
            }
        }
    }
    let copy_safe = matches!(vcodec.as_str(), "h264" | "hevc" | "mpeg4");
    let audio_copy_safe = matches!(acodec.as_str(), "aac" | "mp3" | "ac3" | "eac3");
    let mut cmd = tokio::process::Command::new(&ffmpeg);
    cmd.arg("-i").arg(&p).args(["-f", "mp4"]);
    cmd.args(["-c:v", if copy_safe { "copy" } else { "libx264" }]);
    cmd.args(["-c:a", if audio_copy_safe { "copy" } else { "aac" }]);
    cmd.args(["-movflags", "frag_keyframe+empty_moov+default_base_moof"]);
    if !copy_safe {
        cmd.args(["-preset", "fast", "-crf", "23"]);
    }
    cmd.args(["-loglevel", "error", "-y", "pipe:1"]);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // A client abort (seek away, close tab) must kill the encoder, not
        // leave it writing into a dead pipe.
        .kill_on_drop(true);
    tracing::info!(
        "[transcode] {}: vcodec={vcodec} acodec={acodec} → {}",
        p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        if copy_safe { "remux" } else { "transcode" }
    );
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let Some(stdout) = child.stdout.take() else {
        return j(500, json!({"error": "ffmpeg stdout not piped"}));
    };
    let stderr = child.stderr.take();
    // Stream stdout in 64KB chunks; on EOF reap the child and surface a
    // nonzero exit in the log (the bytes are already gone — python printed
    // the same post-hoc).
    let stream = futures::stream::unfold(
        (child, stdout, stderr),
        |(mut child, mut so, stderr)| async move {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 65536];
            match so.read(&mut buf).await {
                Ok(0) | Err(_) => {
                    let status = child.wait().await.ok();
                    if !status.map(|s| s.success()).unwrap_or(true) {
                        let mut err = String::new();
                        if let Some(mut e) = stderr {
                            let _ = e.read_to_string(&mut err).await;
                        }
                        let snippet: String = err.chars().take(500).collect();
                        tracing::warn!("[transcode] ffmpeg failed: {snippet}");
                    }
                    None
                }
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok::<_, std::io::Error>(Bytes::from(buf)), (child, so, stderr)))
                }
            }
        },
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "video/mp4")
        .header("cache-control", "no-store")
        .body(Body::from_stream(stream))
        .expect("transcode response")
}

// ---------------------------------------------------------------------------
// GET /api/library — flat metadata index of an ebook folder (py:67940-67954,
// engine py:507-663). Calibre metadata.db preferred, .opf sidecar scan as
// fallback. Mounted at the TOP level (mod.rs), like python.
// ---------------------------------------------------------------------------

const LIB_EBOOK_EXTS: &[&str] =
    &[".epub", ".mobi", ".azw", ".azw3", ".azw4", ".kfx", ".fb2", ".cbz", ".cbr", ".pdf", ".djvu"];

fn lib_fmt_rank(fmt: &str) -> i64 {
    match fmt.to_lowercase().as_str() {
        "epub" => 0,
        "azw3" => 1,
        "mobi" => 2,
        "fb2" => 3,
        "cbz" => 4,
        "pdf" => 5,
        "azw" => 6,
        "djvu" => 9,
        _ => 8,
    }
}

pub async fn library(method: Method, RawQuery(q): RawQuery) -> Response {
    if method != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(q.as_deref().unwrap_or(""));
    let dpath = qs_get(&qs, "path").unwrap_or("");
    if dpath.is_empty() {
        return j(400, json!({"error": "missing path"}));
    }
    let d = expanduser(dpath);
    if !d.is_absolute() {
        return j(400, json!({"error": "absolute path required"}));
    }
    if !is_path_allowed(&d) {
        return j(403, json!({"error": "access denied"}));
    }
    if !d.is_dir() {
        return j(404, json!({"error": "not a directory"}));
    }
    match lib_index(&d) {
        Ok(v) => j(200, v),
        Err(e) => j(500, json!({"error": e})),
    }
}

fn lib_index(root: &Path) -> Result<Value, String> {
    let db = root.join("metadata.db");
    let (books, source) = if db.exists() {
        (lib_calibre(&db, root).map_err(|e| e.to_string())?, "calibre")
    } else {
        (lib_opf_scan(root, 5000), "opf")
    };
    let facets = lib_facets(&books);
    Ok(json!({
        "is_library": !books.is_empty(), "source": source, "count": books.len(),
        "books": books, "facets": facets,
    }))
}

fn lib_calibre(db_path: &Path, root: &Path) -> rusqlite::Result<Vec<Value>> {
    // Read-only + immutable, exactly python's URI open — a live Calibre app
    // holding the DB must never see our reads as contention.
    let con = rusqlite::Connection::open_with_flags(
        format!("file:{}?mode=ro&immutable=1", db_path.display()),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;

    struct B {
        id: i64,
        title: String,
        authors: Vec<String>,
        tags: Vec<String>,
        series: Option<String>,
        series_index: Option<f64>,
        formats: Vec<(String, String, i64)>, // fmt, path, size
        cover: Option<String>,
        pubdate: String,
        rating: Value,
        rel_path: String,
        has_cover: bool,
    }
    let mut order: Vec<i64> = Vec::new();
    let mut books: std::collections::HashMap<i64, B> = Default::default();
    {
        let mut stmt = con.prepare(
            "SELECT id,title,author_sort,path,has_cover,pubdate,series_index,timestamp \
             FROM books ORDER BY timestamp DESC LIMIT ?1",
        )?;
        let mut rows = stmt.query([5000i64])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let title: Option<String> = r.get(1)?;
            let pubdate: Option<String> = r.get(5).unwrap_or(None);
            order.push(id);
            books.insert(
                id,
                B {
                    id,
                    title: match title {
                        Some(t) if !t.is_empty() => t,
                        _ => "Untitled".into(),
                    },
                    authors: vec![],
                    tags: vec![],
                    series: None,
                    series_index: r.get(6).unwrap_or(None),
                    formats: vec![],
                    cover: None,
                    pubdate: pubdate.unwrap_or_default().chars().take(10).collect(),
                    rating: json!(0),
                    rel_path: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    has_cover: r.get::<_, Option<i64>>(4).unwrap_or(None).unwrap_or(0) != 0,
                },
            );
        }
    }
    // One explicit loop per link table (the boxed-closure helper this
    // replaced tripped clippy::type_complexity for zero abstraction gain).
    macro_rules! link {
        ($sql:expr, |$b:ident, $r:ident| $body:block) => {{
            let mut stmt = con.prepare($sql)?;
            let mut rows = stmt.query([])?;
            while let Some($r) = rows.next()? {
                let book: i64 = $r.get(0)?;
                if let Some($b) = books.get_mut(&book) $body
            }
        }};
    }
    link!(
        "SELECT bal.book, a.name FROM books_authors_link bal \
         JOIN authors a ON a.id=bal.author ORDER BY bal.id",
        |b, r| { b.authors.push(r.get(1)?); }
    );
    link!(
        "SELECT btl.book, t.name FROM books_tags_link btl \
         JOIN tags t ON t.id=btl.tag ORDER BY t.name",
        |b, r| { b.tags.push(r.get(1)?); }
    );
    link!(
        "SELECT bsl.book, s.name FROM books_series_link bsl \
         JOIN series s ON s.id=bsl.series",
        |b, r| { b.series = Some(r.get(1)?); }
    );
    link!(
        "SELECT brl.book, rt.rating FROM books_ratings_link brl \
         JOIN ratings rt ON rt.id=brl.rating",
        |b, r| {
            let raw: Option<f64> = r.get(1)?;
            b.rating = json!((raw.unwrap_or(0.0) / 2.0 * 10.0).round() / 10.0);
        }
    );
    {
        let mut stmt =
            con.prepare("SELECT book, format, name, uncompressed_size FROM data")?;
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let book: i64 = r.get(0)?;
            let Some(b) = books.get_mut(&book) else { continue };
            let fmt: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
            let name: String = r.get::<_, Option<String>>(2)?.unwrap_or_default();
            let size: i64 = r.get::<_, Option<i64>>(3)?.unwrap_or(0);
            let fp = root.join(&b.rel_path).join(format!("{name}.{}", fmt.to_lowercase()));
            b.formats.push((fmt.to_uppercase(), pystr(&fp), size));
        }
    }

    let mut out: Vec<Value> = Vec::new();
    for id in order {
        let Some(mut b) = books.remove(&id) else { continue };
        b.formats.retain(|(_, path, _)| Path::new(path).exists());
        if b.formats.is_empty() {
            continue;
        }
        if b.has_cover {
            let cov = root.join(&b.rel_path).join("cover.jpg");
            if cov.exists() {
                b.cover = Some(pystr(&cov));
            }
        }
        b.formats.sort_by_key(|(fmt, _, _)| lib_fmt_rank(fmt));
        if b.authors.is_empty() {
            b.authors.push("Unknown".into());
        }
        out.push(json!({
            "id": b.id, "title": b.title, "authors": b.authors, "tags": b.tags,
            "series": b.series, "series_index": b.series_index,
            "formats": b.formats.iter().map(|(fmt, path, size)| json!({
                "fmt": fmt, "path": path, "size": size,
            })).collect::<Vec<_>>(),
            "cover": b.cover, "pubdate": b.pubdate, "rating": b.rating,
        }));
    }
    out.sort_by_key(|b| b["title"].as_str().unwrap_or("").to_lowercase());
    Ok(out)
}

#[derive(Default)]
struct OpfMeta {
    title: Option<String>,
    authors: Vec<String>,
    tags: Vec<String>,
    series: Option<String>,
}

fn xml_unescape(s: &str) -> String {
    // The entities OPF metadata realistically carries. Python used a real
    // XML parser; this regex-grade fallback scan trades exotic-entity
    // fidelity for zero new dependencies, on a path that is already a
    // heuristic (calibre's metadata.db is the primary source).
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
}

/// Python `_lib_parse_opf` (py:576): first title, all creators/subjects,
/// calibre:series meta. Parse failure = empty meta, never an error.
fn parse_opf(opf: &Path) -> OpfMeta {
    let Ok(raw) = std::fs::read(opf) else { return OpfMeta::default() };
    let text = String::from_utf8_lossy(&raw);
    static ELEM: std::sync::OnceLock<[regex::Regex; 4]> = std::sync::OnceLock::new();
    let [title_re, creator_re, subject_re, meta_re] = ELEM.get_or_init(|| {
        let e = |tag: &str| {
            regex::Regex::new(&format!(
                r"(?is)<(?:[A-Za-z0-9_.-]+:)?{tag}\b[^>]*>([^<]*)</(?:[A-Za-z0-9_.-]+:)?{tag}>"
            ))
            .expect("opf regex")
        };
        [
            e("title"),
            e("creator"),
            e("subject"),
            regex::Regex::new(r"(?is)<(?:[A-Za-z0-9_.-]+:)?meta\b[^>]*>").expect("opf meta regex"),
        ]
    });
    let attr = |tag: &str, name: &str| -> Option<String> {
        let re = regex::Regex::new(&format!(r#"(?i)\b{name}\s*=\s*"([^"]*)""#)).ok()?;
        re.captures(tag).map(|c| xml_unescape(&c[1]))
    };
    let mut meta = OpfMeta {
        title: title_re
            .captures(&text)
            .map(|c| xml_unescape(c[1].trim()))
            .filter(|t| !t.is_empty()),
        authors: creator_re
            .captures_iter(&text)
            .map(|c| xml_unescape(c[1].trim()))
            .filter(|a| !a.is_empty())
            .collect(),
        tags: subject_re
            .captures_iter(&text)
            .map(|c| xml_unescape(c[1].trim()))
            .filter(|t| !t.is_empty())
            .collect(),
        series: None,
    };
    for m in meta_re.find_iter(&text) {
        let tag = m.as_str();
        if attr(tag, "name").as_deref() == Some("calibre:series") {
            meta.series = attr(tag, "content");
        }
    }
    meta
}

/// Python `_lib_opf_scan` (py:589): walk, skip dot-dirs (2000/dir cap),
/// group ebook files by stem, title/author from metadata.opf or filename
/// heuristics.
fn lib_opf_scan(root: &Path, limit: usize) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut count: usize = 0;
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    static TITLE_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static AUTHOR_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let title_re =
        TITLE_RE.get_or_init(|| regex::Regex::new(r"\s*-\s*[^-]+$").expect("title regex"));
    let author_re =
        AUTHOR_RE.get_or_init(|| regex::Regex::new(r"-\s*([^-]+)$").expect("author regex"));
    while let Some(dir) = stack.pop() {
        if count >= limit {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut subdirs: Vec<PathBuf> = Vec::new();
        let mut ebooks: Vec<String> = Vec::new();
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if !name.starts_with('.') {
                    subdirs.push(e.path());
                }
            } else if LIB_EBOOK_EXTS.contains(&py_suffix(Path::new(&name)).as_str()) {
                ebooks.push(name);
            }
        }
        subdirs.truncate(2000);
        for s in subdirs.into_iter().rev() {
            stack.push(s);
        }
        if ebooks.is_empty() {
            continue;
        }
        let opf = dir.join("metadata.opf");
        let meta = if opf.exists() { parse_opf(&opf) } else { OpfMeta::default() };
        let cover = ["cover.jpg", "cover.jpeg", "cover.png"]
            .iter()
            .map(|c| dir.join(c))
            .find(|c| c.exists())
            .map(|c| pystr(&c));
        // Group by stem: "Title - Author.epub" + "Title - Author.mobi" is
        // one book with two formats.
        let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
        for f in ebooks {
            let fp = Path::new(&f);
            let base = fp.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            let ext = py_suffix(fp).trim_start_matches('.').to_string();
            match groups.iter_mut().find(|(b, _)| *b == base) {
                Some((_, v)) => v.push((ext, f)),
                None => groups.push((base, vec![(ext, f)])),
            }
        }
        for (base, fmts) in groups {
            let title = meta
                .title
                .clone()
                .or_else(|| {
                    let t = title_re.replace(&base, "").trim().to_string();
                    if t.is_empty() {
                        None
                    } else {
                        Some(t)
                    }
                })
                .unwrap_or_else(|| base.clone());
            let authors = if !meta.authors.is_empty() {
                meta.authors.clone()
            } else if let Some(c) = author_re.captures(&base) {
                vec![c[1].trim().to_string()]
            } else {
                vec!["Unknown".into()]
            };
            let mut formats: Vec<(String, String, i64)> = fmts
                .into_iter()
                .map(|(ext, fname)| {
                    let fp = dir.join(&fname);
                    let size = std::fs::metadata(&fp).map(|m| m.len() as i64).unwrap_or(0);
                    (ext.to_uppercase(), pystr(&fp), size)
                })
                .collect();
            formats.sort_by_key(|(fmt, _, _)| lib_fmt_rank(fmt));
            out.push(json!({
                "id": count, "title": title, "authors": authors,
                "tags": meta.tags, "series": meta.series, "series_index": Value::Null,
                "formats": formats.iter().map(|(fmt, path, size)| json!({
                    "fmt": fmt, "path": path, "size": size,
                })).collect::<Vec<_>>(),
                "cover": cover, "pubdate": "", "rating": 0,
            }));
            count += 1;
            if count >= limit {
                break;
            }
        }
    }
    out.sort_by_key(|b| b["title"].as_str().unwrap_or("").to_lowercase());
    out
}

/// Python `_lib_facets` (py:634): count authors/formats/tags/series, top-N
/// by (-count, name.lower()).
fn lib_facets(books: &[Value]) -> Value {
    let mut authors: std::collections::HashMap<String, i64> = Default::default();
    let mut formats: std::collections::HashMap<String, i64> = Default::default();
    let mut tags: std::collections::HashMap<String, i64> = Default::default();
    let mut series: std::collections::HashMap<String, i64> = Default::default();
    for b in books {
        for a in b["authors"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if let Some(a) = a.as_str() {
                *authors.entry(a.to_string()).or_insert(0) += 1;
            }
        }
        let mut seen: std::collections::HashSet<String> = Default::default();
        for f in b["formats"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if let Some(fmt) = f["fmt"].as_str() {
                if seen.insert(fmt.to_string()) {
                    *formats.entry(fmt.to_string()).or_insert(0) += 1;
                }
            }
        }
        for t in b["tags"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
            if let Some(t) = t.as_str() {
                *tags.entry(t.to_string()).or_insert(0) += 1;
            }
        }
        if let Some(s) = b["series"].as_str() {
            *series.entry(s.to_string()).or_insert(0) += 1;
        }
    }
    let top = |d: &std::collections::HashMap<String, i64>, n: usize| -> Vec<Value> {
        let mut kv: Vec<(&String, &i64)> = d.iter().collect();
        kv.sort_by_key(|e| (std::cmp::Reverse(e.1), e.0.to_lowercase()));
        kv.into_iter().take(n).map(|(k, v)| json!({"name": k, "count": v})).collect()
    };
    json!({
        "authors": top(&authors, 60), "formats": top(&formats, 12),
        "tags": top(&tags, 60), "series": top(&series, 60),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::http::Request as HttpRequest;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// Test override for the media-cache dir (the py_proxy PY_BASE_OVERRIDE
    /// pattern: process env is not hermetic across parallel tests).
    pub(crate) static MEDIA_CACHE_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
    /// Serializes the tests that set it.
    static CACHE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        AppState {
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        }
    }

    fn app() -> axum::Router {
        Router::new()
            .nest("/api/file", routes())
            .route("/api/library", any(library))
            .with_state(state())
    }

    async fn send(app: &axum::Router, req: HttpRequest<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap().to_vec();
        (status, headers, body)
    }

    async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
        let (status, _, body) = send(
            app,
            HttpRequest::builder().uri(path).body(Body::empty()).unwrap(),
        )
        .await;
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    fn enc(s: &str) -> String {
        s.bytes().map(|b| format!("%{b:02X}")).collect()
    }

    fn ffmpeg_available() -> bool {
        find_bin("ffmpeg").is_some()
    }

    /// Generate a tiny media fixture with lavfi (never user files). Returns
    /// None when ffmpeg is absent — callers SKIP LOUDLY, because a silently
    /// green ffmpeg test on a box without ffmpeg is theatre.
    fn gen_fixture(dir: &Path, name: &str, container_args: &[&str]) -> Option<PathBuf> {
        let ffmpeg = find_bin("ffmpeg")?;
        let out = dir.join(name);
        let ok = std::process::Command::new(ffmpeg)
            .args(["-y", "-f", "lavfi", "-i", "testsrc=duration=0.5:size=64x64:rate=10",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=0.5", "-shortest"])
            .args(container_args)
            .arg(&out)
            .output()
            .ok()?
            .status
            .success();
        ok.then_some(out)
    }

    // -- range/streaming semantics (the pinned 206 contract) ---------------

    #[tokio::test]
    async fn raw_range_206_headers_are_exact() {
        let app = app();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("clip.mp4");
        let payload: Vec<u8> = (0u8..=99).collect();
        std::fs::write(&p, &payload).unwrap();
        let meta = std::fs::metadata(&p).unwrap();
        let etag = format!("\"{}-{}\"", mtime_secs(&meta), 100);
        let uri = format!("/api/file/raw?path={}", enc(p.to_str().unwrap()));

        // Bounded range.
        let (status, h, body) = send(
            &app,
            HttpRequest::builder().uri(&uri).header("range", "bytes=10-19")
                .body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h["content-range"], "bytes 10-19/100");
        assert_eq!(h["content-length"], "10");
        assert_eq!(h["accept-ranges"], "bytes");
        assert_eq!(h["content-type"], "video/mp4");
        // Inline media carries NO Content-Disposition (its presence wedges
        // Chrome-over-CDP into a download; amux-gtm 2026-08-13).
        assert!(h.get("content-disposition").is_none());
        assert_eq!(h["etag"], etag.as_str());
        assert_eq!(h["cache-control"], "private, max-age=3600, immutable");
        assert_eq!(body, payload[10..20].to_vec());

        // Open-ended range (what scrubbing players send).
        let (status, h, body) = send(
            &app,
            HttpRequest::builder().uri(&uri).header("range", "bytes=90-")
                .body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h["content-range"], "bytes 90-99/100");
        assert_eq!(h["content-length"], "10");
        assert_eq!(body, payload[90..].to_vec());

        // End past EOF clamps.
        let (status, h, _) = send(
            &app,
            HttpRequest::builder().uri(&uri).header("range", "bytes=0-100000")
                .body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(h["content-range"], "bytes 0-99/100");
        assert_eq!(h["content-length"], "100");

        // No Range: plain 200, still range-advertising + cacheable.
        let (status, h, body) = send(
            &app,
            HttpRequest::builder().uri(&uri).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(h["content-length"], "100");
        assert_eq!(h["accept-ranges"], "bytes");
        assert_eq!(h["etag"], etag.as_str());
        assert_eq!(body, payload);

        // If-None-Match short-circuits to 304.
        let (status, _, body) = send(
            &app,
            HttpRequest::builder().uri(&uri).header("if-none-match", &etag)
                .body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_MODIFIED);
        assert!(body.is_empty());

        // Non-media extension: attachment disposition, octet-stream.
        let t = dir.path().join("notes.bin");
        std::fs::write(&t, b"x").unwrap();
        let (_, h, _) = send(
            &app,
            HttpRequest::builder()
                .uri(format!("/api/file/raw?path={}", enc(t.to_str().unwrap())))
                .body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(h["content-type"], "application/octet-stream");
        assert_eq!(h["content-disposition"], "attachment; filename=\"notes.bin\"");

        // Renderable types serve with a real content-type AND inline disposition
        // so a browser navigation renders instead of downloading (amax-gtm bug 3).
        let html = dir.path().join("report.html");
        std::fs::write(&html, b"<h1>hi</h1>").unwrap();
        let (_, h, _) = send(
            &app,
            HttpRequest::builder()
                .uri(format!("/api/file/raw?path={}", enc(html.to_str().unwrap())))
                .body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(h["content-type"], "text/html; charset=utf-8");
        // No Content-Disposition at all for an inline render — its mere presence
        // (even `inline`) wedges Chrome-over-CDP into a download (amux-gtm).
        assert!(
            h.get("content-disposition").is_none(),
            "renderable types must not carry a Content-Disposition: {:?}",
            h.get("content-disposition")
        );

        // ?download=1 forces attachment even for a renderable type.
        let (_, h, _) = send(
            &app,
            HttpRequest::builder()
                .uri(format!("/api/file/raw?path={}&download=1", enc(html.to_str().unwrap())))
                .body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(h["content-type"], "text/html; charset=utf-8");
        assert_eq!(h["content-disposition"], "attachment; filename=\"report.html\"");
    }

    #[tokio::test]
    async fn raw_guards_and_errors() {
        let app = app();
        let (status, v) = get(&app, "/api/file/raw?path=").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "missing path");
        let (status, v) = get(&app, "/api/file/raw?path=rel.txt").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "relative path without cwd");
        let (status, v) = get(&app, &format!("/api/file/raw?path={}", enc("/~none/x"))).await;
        // nonexistent but allowed → 404
        assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
        let home = std::env::var("HOME").unwrap();
        let (status, v) = get(&app, &format!("/api/file/raw?path={}", enc(&format!("{home}/.ssh/id_rsa")))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(v["error"], "access denied");
        // wrong method → python's generic 404
        let (status, _, body) = send(
            &app,
            HttpRequest::builder().method("POST").uri("/api/file/raw?path=/tmp/x")
                .body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "not found");
    }

    // -- viewer payloads ----------------------------------------------------

    #[tokio::test]
    async fn viewer_text_markdown_csv_binary_image() {
        let app = app();
        let dir = tempfile::tempdir().unwrap();

        let md = dir.path().join("doc.md");
        std::fs::write(&md, "# hi\n").unwrap();
        let (status, v) = get(&app, &format!("/api/file?path={}", enc(md.to_str().unwrap()))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["content"], "# hi\n");
        assert_eq!(v["is_markdown"], true);
        assert_eq!(v["is_csv"], false);
        assert_eq!(v["is_html"], false);
        assert_eq!(v["path"], md.to_str().unwrap());

        // 200KB char truncation with the python suffix.
        let big = dir.path().join("big.txt");
        std::fs::write(&big, "a".repeat(200_001)).unwrap();
        let (_, v) = get(&app, &format!("/api/file?path={}", enc(big.to_str().unwrap()))).await;
        let content = v["content"].as_str().unwrap();
        assert!(content.ends_with("\n\n... (truncated at 200KB)"));
        assert_eq!(content.chars().count(), 200_000 + "\n\n... (truncated at 200KB)".chars().count());

        // CSV keeps 5MB and flags is_csv.
        let csv = dir.path().join("t.csv");
        std::fs::write(&csv, "a,b\n1,2\n").unwrap();
        let (_, v) = get(&app, &format!("/api/file?path={}", enc(csv.to_str().unwrap()))).await;
        assert_eq!(v["is_csv"], true);

        // NUL byte → binary card.
        let bin = dir.path().join("blob.dat");
        std::fs::write(&bin, b"abc\x00def").unwrap();
        let (_, v) = get(&app, &format!("/api/file?path={}", enc(bin.to_str().unwrap()))).await;
        assert_eq!(v["is_binary"], true);
        assert_eq!(v["size"], 7);
        assert_eq!(v["ext"], ".dat");

        // Small image inlines as a data_url.
        let img = dir.path().join("i.png");
        std::fs::write(&img, b"\x89PNG-not-really").unwrap();
        let (_, v) = get(&app, &format!("/api/file?path={}", enc(img.to_str().unwrap()))).await;
        assert_eq!(v["is_image"], true);
        assert_eq!(v["mime"], "image/png");
        assert!(v["data_url"].as_str().unwrap().starts_with("data:image/png;base64,"));
        assert!(v.get("raw_url").is_none());

        // Large image streams: raw_url, no data_url (AMUX-2344).
        let big_img = dir.path().join("photo.jpg");
        let f = std::fs::File::create(&big_img).unwrap();
        f.set_len(2_000_001).unwrap();
        drop(f);
        let (_, v) = get(&app, &format!("/api/file?path={}", enc(big_img.to_str().unwrap()))).await;
        assert_eq!(v["is_image"], true);
        assert!(v.get("data_url").is_none());
        assert_eq!(
            v["raw_url"],
            format!("/api/file/raw?path={}", py_quote(big_img.to_str().unwrap()))
        );

        // Video card carries srt + sidecar profile/task.
        let vid = dir.path().join("run.mp4");
        std::fs::write(&vid, b"not-actually-mp4").unwrap();
        std::fs::write(dir.path().join("run.srt"), "1\n00:00:01,000 --> 00:00:02,000\nhi\n").unwrap();
        std::fs::write(dir.path().join("run.json"), r#"{"profile":"default","task":"demo"}"#).unwrap();
        let (_, v) = get(&app, &format!("/api/file?path={}", enc(vid.to_str().unwrap()))).await;
        assert_eq!(v["is_video"], true);
        assert_eq!(v["mime"], "video/mp4");
        assert_eq!(v["srt"], dir.path().join("run.srt").to_str().unwrap());
        assert_eq!(v["profile"], "default");
        assert_eq!(v["task"], "demo");

        // Directory → 404 file not found (python p.is_file()).
        let (status, v) = get(&app, &format!("/api/file?path={}", enc(dir.path().to_str().unwrap()))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(v["error"], "file not found");
    }

    #[tokio::test]
    async fn viewer_ebooks_honest_501_and_download_card() {
        let app = app();
        let dir = tempfile::tempdir().unwrap();
        // Renderable EPUB under cap: python renders; this origin says 501
        // naming the missing capability — never a fake success.
        let epub = dir.path().join("book.epub");
        std::fs::write(&epub, b"PK-zip-ish").unwrap();
        let (status, v) = get(&app, &format!("/api/file?path={}", enc(epub.to_str().unwrap()))).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{v}");
        assert!(v["error"].as_str().unwrap().contains("no rust port"), "{v}");
        assert_eq!(v["is_ebook"], true);
        assert_eq!(v["ebook_kind"], "epub");
        // Proprietary format: the download card python also serves.
        let azw3 = dir.path().join("book.azw3");
        std::fs::write(&azw3, b"BOOKMOBI").unwrap();
        let (status, v) = get(&app, &format!("/api/file?path={}", enc(azw3.to_str().unwrap()))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["is_binary"], true);
        assert_eq!(v["is_ebook"], true);
        assert_eq!(v["ebook_kind"], "azw3");
        assert_eq!(v["ext"], ".azw3");
    }

    // -- PUT /api/file ------------------------------------------------------

    #[tokio::test]
    async fn put_file_writes_texts_and_refuses_vectors() {
        let app = app();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("new/notes.md");
        let (status, _, body) = send(
            &app,
            HttpRequest::builder().method("PUT").uri("/api/file")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"path": target.to_str().unwrap(), "content": "hello"}).to_string(),
                ))
                .unwrap(),
        )
        .await;
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["ok"], true);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");

        // A .mdai computed-node file is writable via the normal save path
        // (AMUX-3256), so the MDAI UI does not need the upload bridge.
        let mdai = dir.path().join("graph/node.mdai");
        let (status, _, body) = send(
            &app,
            HttpRequest::builder().method("PUT").uri("/api/file")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"path": mdai.to_str().unwrap(), "content": "---\nsources: []\n---\nbody"}).to_string(),
                ))
                .unwrap(),
        )
        .await;
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(status, StatusCode::OK, "mdai must be writable: {v}");
        assert_eq!(std::fs::read_to_string(&mdai).unwrap(), "---\nsources: []\n---\nbody");

        // Unlisted extension → 400 with python's message.
        let exe = dir.path().join("x.exe");
        let (status, _, body) = send(
            &app,
            HttpRequest::builder().method("PUT").uri("/api/file")
                .body(Body::from(json!({"path": exe.to_str().unwrap(), "content": ""}).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "file type not writable: .exe");

        // Extensionless code-execution vector → 403 refused.
        let ak = dir.path().join("authorized_keys");
        let (status, _, body) = send(
            &app,
            HttpRequest::builder().method("PUT").uri("/api/file")
                .body(Body::from(json!({"path": ak.to_str().unwrap(), "content": "ssh-ed25519 X"}).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "refused: writing this file could execute code");
        assert!(!ak.exists());
    }

    // -- vtt ----------------------------------------------------------------

    #[tokio::test]
    async fn vtt_converts_srt_timestamps() {
        let app = app();
        let dir = tempfile::tempdir().unwrap();
        let srt = dir.path().join("s.srt");
        std::fs::write(&srt, "1\n00:00:01,500 --> 00:00:03,250\nhello\n").unwrap();
        let (status, h, body) = send(
            &app,
            HttpRequest::builder()
                .uri(format!("/api/file/vtt?path={}", enc(srt.to_str().unwrap())))
                .body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(h["content-type"], "text/vtt; charset=utf-8");
        assert_eq!(h["cache-control"], "no-store");
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with("WEBVTT\n\n"));
        assert!(text.contains("00:00:01.500 --> 00:00:03.250"));
    }

    // -- prepare: durable job model ------------------------------------------

    #[test]
    fn prep_decision_covers_restart_orphans() {
        let now = 1_000_000i64;
        let row = |status: &str, progress: f64, error: &str, updated_at: i64| JobRow {
            status: status.into(),
            progress,
            error: error.into(),
            updated_at,
        };
        // no row: cache hit vs fresh start
        assert_eq!(prep_decision(None, true, now), PrepDecision::Ready);
        assert_eq!(prep_decision(None, false, now), PrepDecision::StartNew);
        // live running row reports (rounded) progress
        assert_eq!(
            prep_decision(Some(&row("running", 42.34, "", now - 5)), false, now),
            PrepDecision::Progress(42.3)
        );
        // running + stale heartbeat = the restart orphan: MUST restart, not
        // report a progress number nobody is advancing.
        assert_eq!(
            prep_decision(Some(&row("running", 42.3, "", now - JOB_STALE_S - 1)), false, now),
            PrepDecision::StartNew
        );
        // error reports once (caller clears the row for retry)
        assert_eq!(
            prep_decision(Some(&row("error", 0.0, "ffmpeg exit 1: boom", now)), false, now),
            PrepDecision::ErrorOnce("ffmpeg exit 1: boom".into())
        );
        // done + file present = ready; done + file GONE (pruned) restarts
        // instead of pointing at nothing forever.
        assert_eq!(prep_decision(Some(&row("done", 100.0, "", now)), true, now), PrepDecision::Ready);
        assert_eq!(prep_decision(Some(&row("done", 100.0, "", now)), false, now), PrepDecision::StartNew);
    }

    #[test]
    fn media_key_matches_python_derivation() {
        // Oracle RUN against real python (2026-08-09, not transcribed from
        // memory — the first draft of this constant was a from-memory guess
        // and failed against a correct implementation):
        //   $ python3 -c "import hashlib; print(hashlib.sha1(
        //         '/tmp/x.mkv|1700000000|12345'.encode()).hexdigest()[:24])"
        //   6eebad2c7ceb4210eec8f307
        let mut h = sha1::Sha1::new();
        h.update(b"/tmp/x.mkv|1700000000|12345");
        let key = &hex::encode(h.finalize())[..24];
        assert_eq!(key, "6eebad2c7ceb4210eec8f307");
    }

    #[tokio::test]
    async fn prepare_runs_a_durable_job_end_to_end() {
        if !ffmpeg_available() {
            eprintln!("SKIP prepare_runs_a_durable_job_end_to_end: ffmpeg not found");
            return;
        }
        let _guard = CACHE_LOCK.lock().await;
        let cache = tempfile::tempdir().unwrap();
        *MEDIA_CACHE_OVERRIDE.lock().unwrap() = Some(cache.path().to_path_buf());

        let st = state();
        let app = Router::new().nest("/api/file", routes()).with_state(st.clone());
        let dir = tempfile::tempdir().unwrap();
        // Tiny lavfi-generated MKV — never a user file.
        let Some(src) = gen_fixture(dir.path(), "clip.mkv", &["-c:v", "libx264", "-c:a", "aac"])
        else {
            eprintln!("SKIP: fixture generation failed");
            *MEDIA_CACHE_OVERRIDE.lock().unwrap() = None;
            return;
        };
        let uri = format!("/api/file/prepare?path={}", enc(src.to_str().unwrap()));
        let (status, v) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["ready"], false, "{v}");
        assert_eq!(v["started"], true, "{v}");

        // The job row is DURABLE: visible in the shared DB, not process RAM.
        let meta = std::fs::metadata(&src).unwrap();
        let key = media_key(&src, &meta);
        let row_status: String = st
            .store
            .read()
            .unwrap()
            .query_row("SELECT status FROM _amux_media_jobs WHERE key=?1", [&key], |r| r.get(0))
            .expect("durable job row exists");
        assert!(row_status == "running" || row_status == "done", "{row_status}");

        // Poll to completion.
        let mut ready = false;
        for _ in 0..120 {
            let (_, v) = get(&app, &uri).await;
            if v["ready"] == true {
                ready = true;
                let cached = v["cached_path"].as_str().unwrap();
                assert!(cached.ends_with(&format!("{key}.mp4")), "{cached}");
                assert!(Path::new(cached).exists());
                break;
            }
            assert!(v.get("error").is_none(), "job failed: {v}");
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        assert!(ready, "prepare never became ready");
        // Second call: instant cache hit.
        let (_, v) = get(&app, &uri).await;
        assert_eq!(v["ready"], true);
        *MEDIA_CACHE_OVERRIDE.lock().unwrap() = None;
    }

    #[tokio::test]
    async fn prepare_missing_ffmpeg_shape_and_guards() {
        let app = app();
        // guards identical to raw's
        let (status, v) = get(&app, "/api/file/prepare?path=").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "missing path");
        let (status, _) = get(&app, &format!("/api/file/prepare?path={}", enc("/nope/x.mkv"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn prepare_non_video_is_honest_not_an_ffmpeg_error() {
        // A non-video (xlsx/pdf/zip/docx) must NOT be sent to ffmpeg — it used
        // to fail with "Invalid data found when processing input", which reads
        // like a corrupt file (amax-gtm bug 3). The gate returns before find_bin,
        // so this passes with or without ffmpeg on the box.
        let app = app();
        let dir = tempfile::tempdir().unwrap();
        for name in ["deliverable.xlsx", "report.pdf", "bundle.zip", "doc.docx"] {
            let f = dir.path().join(name);
            std::fs::write(&f, b"PK\x03\x04 not really media").unwrap();
            let (status, v) = get(
                &app,
                &format!("/api/file/prepare?path={}", enc(f.to_str().unwrap())),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{name}: {v}");
            assert_eq!(v["ready"], false, "{name}: {v}");
            assert_eq!(v["reason"], "unsupported type", "{name}: {v}");
            assert!(v.get("error").is_none(), "{name} must not surface an ffmpeg error: {v}");
        }
    }

    // -- transcode -----------------------------------------------------------

    #[tokio::test]
    async fn transcode_streams_fragmented_mp4() {
        if !ffmpeg_available() {
            eprintln!("SKIP transcode_streams_fragmented_mp4: ffmpeg not found");
            return;
        }
        let app = app();
        let dir = tempfile::tempdir().unwrap();
        let Some(src) = gen_fixture(dir.path(), "clip.mkv", &["-c:v", "libx264", "-c:a", "aac"])
        else {
            eprintln!("SKIP: fixture generation failed");
            return;
        };
        let (status, h, body) = send(
            &app,
            HttpRequest::builder()
                .uri(format!("/api/file/transcode?path={}", enc(src.to_str().unwrap())))
                .body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(h["content-type"], "video/mp4");
        assert_eq!(h["cache-control"], "no-store");
        assert!(h.get("content-length").is_none(), "live pipe must not claim a length");
        // Fragmented MP4 starts with an ftyp box.
        assert!(body.len() > 8, "got {} bytes", body.len());
        assert_eq!(&body[4..8], b"ftyp");
    }

    // -- library -------------------------------------------------------------

    #[tokio::test]
    async fn library_opf_scan_groups_formats_and_facets() {
        let app = app();
        let dir = tempfile::tempdir().unwrap();
        let bookdir = dir.path().join("Author/Title");
        std::fs::create_dir_all(&bookdir).unwrap();
        std::fs::write(bookdir.join("Dune - Frank Herbert.epub"), b"e").unwrap();
        std::fs::write(bookdir.join("Dune - Frank Herbert.mobi"), b"mm").unwrap();
        std::fs::write(bookdir.join("cover.jpg"), b"jpg").unwrap();
        std::fs::write(
            bookdir.join("metadata.opf"),
            r#"<?xml version="1.0"?><package xmlns:dc="http://purl.org/dc/elements/1.1/">
               <metadata><dc:title>Dune</dc:title><dc:creator>Frank Herbert</dc:creator>
               <dc:subject>scifi</dc:subject>
               <meta name="calibre:series" content="Dune Chronicles"/></metadata></package>"#,
        )
        .unwrap();
        let (status, v) = get(&app, &format!("/api/library?path={}", enc(dir.path().to_str().unwrap()))).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["is_library"], true);
        assert_eq!(v["source"], "opf");
        assert_eq!(v["count"], 1);
        let b = &v["books"][0];
        assert_eq!(b["title"], "Dune");
        assert_eq!(b["authors"], json!(["Frank Herbert"]));
        assert_eq!(b["tags"], json!(["scifi"]));
        assert_eq!(b["series"], "Dune Chronicles");
        // EPUB ranks before MOBI.
        assert_eq!(b["formats"][0]["fmt"], "EPUB");
        assert_eq!(b["formats"][1]["fmt"], "MOBI");
        assert_eq!(b["formats"][1]["size"], 2);
        assert!(b["cover"].as_str().unwrap().ends_with("cover.jpg"));
        assert_eq!(v["facets"]["authors"][0], json!({"name": "Frank Herbert", "count": 1}));
        assert_eq!(v["facets"]["formats"].as_array().unwrap().len(), 2);

        // Filename heuristics without an opf.
        let plain = dir.path().join("loose");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("Neuromancer - William Gibson.epub"), b"x").unwrap();
        let (_, v) = get(&app, &format!("/api/library?path={}", enc(plain.to_str().unwrap()))).await;
        let b = &v["books"][0];
        assert_eq!(b["title"], "Neuromancer");
        assert_eq!(b["authors"], json!(["William Gibson"]));

        // Empty dir: honest not-a-library.
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let (status, v) = get(&app, &format!("/api/library?path={}", enc(empty.to_str().unwrap()))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["is_library"], false);
        assert_eq!(v["count"], 0);

        // Missing dir → 404 python shape.
        let (status, v) = get(&app, &format!("/api/library?path={}", enc("/nope/lib"))).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(v["error"], "not a directory");
    }

    #[tokio::test]
    async fn library_calibre_db_is_read_and_filtered_to_existing_files() {
        let app = app();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Minimal calibre schema subset the query reads.
        let con = rusqlite::Connection::open(root.join("metadata.db")).unwrap();
        con.execute_batch(
            "CREATE TABLE books(id INTEGER PRIMARY KEY, title TEXT, author_sort TEXT, path TEXT,
                 has_cover BOOL, pubdate TEXT, series_index REAL, timestamp TEXT);
             CREATE TABLE authors(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE books_authors_link(id INTEGER PRIMARY KEY, book INTEGER, author INTEGER);
             CREATE TABLE tags(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE books_tags_link(id INTEGER PRIMARY KEY, book INTEGER, tag INTEGER);
             CREATE TABLE series(id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE books_series_link(id INTEGER PRIMARY KEY, book INTEGER, series INTEGER);
             CREATE TABLE ratings(id INTEGER PRIMARY KEY, rating INTEGER);
             CREATE TABLE books_ratings_link(id INTEGER PRIMARY KEY, book INTEGER, rating INTEGER);
             CREATE TABLE data(id INTEGER PRIMARY KEY, book INTEGER, format TEXT, name TEXT,
                 uncompressed_size INTEGER);
             INSERT INTO books VALUES
                 (1,'Dune','Herbert','Herbert/Dune (1)',1,'1965-08-01T00:00:00+00:00',1.0,'2024-01-01'),
                 (2,'Ghost','Nobody','Nobody/Ghost (2)',0,NULL,NULL,'2024-01-02');
             INSERT INTO authors VALUES (1,'Frank Herbert');
             INSERT INTO books_authors_link VALUES (1,1,1);
             INSERT INTO tags VALUES (1,'scifi');
             INSERT INTO books_tags_link VALUES (1,1,1);
             INSERT INTO series VALUES (1,'Dune Chronicles');
             INSERT INTO books_series_link VALUES (1,1,1);
             INSERT INTO ratings VALUES (1,9);
             INSERT INTO books_ratings_link VALUES (1,1,1);
             INSERT INTO data VALUES (1,1,'EPUB','Dune - Frank Herbert',12345),
                                     (2,2,'EPUB','Ghost',1);",
        )
        .unwrap();
        drop(con);
        // Only book 1's file exists on disk; book 2 must be filtered out.
        let bdir = root.join("Herbert/Dune (1)");
        std::fs::create_dir_all(&bdir).unwrap();
        std::fs::write(bdir.join("Dune - Frank Herbert.epub"), b"e").unwrap();
        std::fs::write(bdir.join("cover.jpg"), b"jpg").unwrap();

        let (status, v) = get(&app, &format!("/api/library?path={}", enc(root.to_str().unwrap()))).await;
        assert_eq!(status, StatusCode::OK, "{v}");
        assert_eq!(v["source"], "calibre");
        assert_eq!(v["count"], 1, "book with no on-disk file must be dropped: {v}");
        let b = &v["books"][0];
        assert_eq!(b["id"], 1);
        assert_eq!(b["title"], "Dune");
        assert_eq!(b["authors"], json!(["Frank Herbert"]));
        assert_eq!(b["series"], "Dune Chronicles");
        assert_eq!(b["series_index"], 1.0);
        assert_eq!(b["rating"], 4.5);
        assert_eq!(b["pubdate"], "1965-08-01");
        assert_eq!(b["formats"][0]["fmt"], "EPUB");
        assert_eq!(b["formats"][0]["size"], 12345);
        assert!(b["cover"].as_str().unwrap().ends_with("cover.jpg"));
    }

    // -- misc contract -------------------------------------------------------

    #[tokio::test]
    async fn unknown_file_subpaths_are_python_404s() {
        let app = app();
        let (status, v) = get(&app, "/api/file/bogus").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(v["error"], "not found");
    }

    #[test]
    fn py_quote_matches_urllib_default_safe() {
        // urllib.parse.quote("/a b/ü.png") == '/a%20b/%C3%BC.png'
        assert_eq!(py_quote("/a b/ü.png"), "/a%20b/%C3%BC.png");
        assert_eq!(py_quote("/plain/path.mp4"), "/plain/path.mp4");
    }
}
