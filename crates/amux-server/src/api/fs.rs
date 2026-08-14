//! /api/fs — the SPA's Files surface, NATIVE (AMUX-2597 boundary work).
//!
//! Ported from the Python owner's handlers so the wire contract is
//! byte-compatible (statuses, error strings, field names). This is a
//! DIFFERENT contract from the modern `/api/files` (raw-body upload,
//! `?path=` relative to a root) — both stay mounted; see
//! docs/rust-migration/server-boundary.md.
//!
//! Ported from the deleted Python server (historical amux-server.py, deleted at 792ce1f; line refs are into git history):
//! - path containment  `_is_path_allowed`      py:93-121  (deny sets py:82-91)
//! - dangerous writes  `_is_dangerous_write`   py:670-698
//! - POST   /api/fs/mkdir                      py:67883-67899
//! - POST   /api/fs/open                       py:68339-68367
//! - POST   /api/fs/upload  (multipart, `dir`) py:68369-68430
//! - POST   /api/fs/rename                     py:68435-68458
//! - GET    /api/fs/read                       py:68468-68500
//! - GET    /api/fs/search  (`_fs_search`)     py:68505-68521, core py:21103-21210
//! - GET    /api/fs/list                       py:68523-68554
//! - DELETE /api/fs/delete                     py:68556-68570
//! - GET    /api/ls                            py:68308-68336 (the SPA's Files
//!   BROWSER; /api/fs/list is the API-caller surface — different shapes)
//! - GET    /api/autocomplete/dir              py:68255-68285
//!
//! The rest of the SPA's file views — `/api/file` (viewer payload),
//! `/api/file/raw|prepare|transcode`, `/api/library` — stay PYTHON-OWNED
//! (py_proxy registry: media pipeline with in-process job state). The
//! worker page's Files tab search is THIS module's /api/fs/search with the
//! worker's CC_DIR as `path` (AMUX-2420: one engine for both surfaces).
//!
//! Contract notes that are easy to lose in a rewrite:
//! - Wrong METHOD on a real path is Python's generic 404 `{"error": "not
//!   found"}`, not a 405 — Python routes on (method, path) pairs and falls
//!   through. Every route here is `any()` + an in-handler method check to
//!   preserve that.
//! - `/api/fs/read`'s "binary" detection is exactly "did the bytes decode as
//!   UTF-8": a NUL byte is valid UTF-8 and comes back as text, while a
//!   VALID UTF-8 file truncated mid-codepoint by `max_bytes` comes back
//!   base64 (both verified against the live server, fixture
//!   tests/fixtures/boundary/live_recorded.json).
//! - Upload never clobbers by default (suffix `_1`, `_2`, ...); `overwrite`
//!   field opts in (py:68414-68424, the re-seed lesson).
//! - `_fs_search` reports every filter that can produce a silent zero
//!   (max_filesize, ignored/hidden) — keep that honesty when touching it.

use super::AppState;
use axum::extract::{DefaultBodyLimit, RawQuery, Request};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/mkdir", any(mkdir))
        .route("/open", any(open_native))
        .route("/upload", any(upload))
        .route("/rename", any(rename))
        .route("/read", any(read_file))
        .route("/search", any(search))
        .route("/list", any(list_dir))
        .route("/delete", any(delete_path))
        // Anything else under /api/fs — including the bare namespace root —
        // is Python's generic 404. EXPLICIT wildcard, not `.fallback()`: in
        // the full composition the static SPA catch-all out-competes a
        // nested fallback (the AMUX-2594 swallow).
        .route("/", any(not_found))
        .route("/{*rest}", any(not_found))
        // Python has NO body cap on these endpoints ("No size limit on file
        // uploads", py:68374); axum's 2MB default would 413 real uploads.
        .layer(DefaultBodyLimit::disable())
}

// ---------------------------------------------------------------------------
// Shared JSON plumbing
// ---------------------------------------------------------------------------

pub(crate) fn j(status: u16, v: Value) -> Response {
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(v),
    )
        .into_response()
}

/// Python's generic fallthrough 404 (`{"error": "not found"}`), which is
/// what wrong-method and unknown /api/fs/* requests get.
pub(crate) async fn not_found() -> Response {
    j(404, json!({"error": "not found"}))
}

/// Python's `_read_body` (py:65303-65326): empty body is `{}`; invalid JSON
/// gets one repair pass that escapes lone backslashes (agents sending
/// Windows paths / regexes), then a parse failure is a 500 — the generic
/// exception path in Python's `_route`.
///
/// Python's repair is `re.sub(r'\\(?!["\\/bfnrt]|u[0-9a-fA-F]{4})', r'\\\\')`;
/// the regex crate has no look-ahead (clippy::invalid_regex caught the naive
/// port), so the same rule is applied by hand: a `\` NOT followed by a valid
/// JSON escape is doubled.
pub(crate) fn parse_body(bytes: &[u8]) -> Result<Value, String> {
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        return Ok(v);
    }
    let text = String::from_utf8_lossy(bytes);
    let chars: Vec<char> = text.chars().collect();
    let mut fixed = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            let next = chars.get(i + 1).copied();
            let valid = match next {
                Some('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') => true,
                Some('u') => chars[i + 2..]
                    .iter()
                    .take(4)
                    .filter(|c| c.is_ascii_hexdigit())
                    .count()
                    == 4,
                _ => false,
            };
            if valid {
                fixed.push('\\');
            } else {
                fixed.push_str("\\\\");
            }
            i += 1;
            continue;
        }
        fixed.push(chars[i]);
        i += 1;
    }
    serde_json::from_str::<Value>(&fixed).map_err(|e| e.to_string())
}

/// str-typed body field with Python `.get(k) or ""` semantics.
pub(crate) fn body_str(body: &Value, key: &str) -> String {
    body.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string()
}

/// Python `urllib.parse.parse_qs` equivalent: repeated keys kept, `+` is a
/// space, percent-decoding is utf-8 lossy. Hand-rolled because axum's Query
/// collapses repeated keys (search's `glob` is repeatable).
pub(crate) fn parse_qs(query: &str) -> Vec<(String, String)> {
    fn dec(s: &str) -> String {
        let s = s.replace('+', " ");
        let bytes = s.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }
    query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((dec(k), dec(v)))
        })
        .collect()
}

pub(crate) fn qs_get<'a>(qs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    qs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

// ---------------------------------------------------------------------------
// Path semantics — Python-faithful resolution + containment
// ---------------------------------------------------------------------------

fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

/// `Path(s).expanduser()`. `~user` forms are left untouched (Python would
/// consult passwd; such a path stays relative here and fails the same
/// `is_absolute` checks downstream).
pub(crate) fn expanduser(s: &str) -> PathBuf {
    if s == "~" {
        home_dir()
    } else if let Some(rest) = s.strip_prefix("~/") {
        home_dir().join(rest)
    } else {
        PathBuf::from(s)
    }
}

/// `str(Path(...))` — Python's PurePath normalization: duplicate slashes and
/// interior `.` collapse, `..` is KEPT, trailing slash dropped. Rust's
/// `Components` applies the same normalization, so rebuilding from it
/// matches Python's string form.
pub(crate) fn pystr(p: &Path) -> String {
    let mut out = String::new();
    for c in p.components() {
        match c {
            Component::RootDir => out.push('/'),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.is_empty() && !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str("..");
            }
            Component::Normal(seg) => {
                if !out.is_empty() && !out.ends_with('/') {
                    out.push('/');
                }
                out.push_str(&seg.to_string_lossy());
            }
            Component::Prefix(_) => {}
        }
    }
    if out.is_empty() {
        ".".into()
    } else {
        out
    }
}

/// `Path.resolve()` in non-strict mode (what every Python fs handler calls):
/// symlinks in the EXISTING prefix are resolved component-by-component,
/// `..` applies to the resolved prefix (realpath semantics, not lexical),
/// and a non-existent tail is appended as-is. A symlink budget guards loops;
/// on exhaustion remaining links pass through unresolved, which errs toward
/// the containment check seeing the raw path (deny-safe: the blocked-set
/// checks run on whatever comes back).
fn resolve_nonstrict(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(p)
    };
    let mut work: VecDeque<OsString> = abs
        .components()
        .filter_map(|c| match c {
            Component::RootDir | Component::Prefix(_) => None,
            Component::CurDir => Some(OsString::from(".")),
            Component::ParentDir => Some(OsString::from("..")),
            Component::Normal(s) => Some(s.to_os_string()),
        })
        .collect();
    let mut out = PathBuf::from("/");
    let mut budget = 64usize;
    while let Some(c) = work.pop_front() {
        if c == "." {
            continue;
        }
        if c == ".." {
            out.pop();
            continue;
        }
        let cand = out.join(&c);
        let is_link = std::fs::symlink_metadata(&cand)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);
        if is_link && budget > 0 {
            budget -= 1;
            match std::fs::read_link(&cand) {
                Ok(target) => {
                    if target.is_absolute() {
                        out = PathBuf::from("/");
                    }
                    let comps: Vec<OsString> = target
                        .components()
                        .filter_map(|c| match c {
                            Component::RootDir | Component::Prefix(_) => None,
                            Component::CurDir => Some(OsString::from(".")),
                            Component::ParentDir => Some(OsString::from("..")),
                            Component::Normal(s) => Some(s.to_os_string()),
                        })
                        .collect();
                    for c in comps.into_iter().rev() {
                        work.push_front(c);
                    }
                }
                Err(_) => out = cand,
            }
        } else {
            out = cand;
        }
    }
    out
}

/// Python `_SENSITIVE_PATHS` / `_BLOCKED_SYSTEM_*` (py:82-91), verbatim.
const SENSITIVE_HOME: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".netrc",
    ".npmrc",
    ".docker",
    ".config/gcloud",
    ".config/gh",
];
const BLOCKED_SYSTEM_PATHS: &[&str] = &[
    "/etc/shadow",
    "/etc/sudoers",
    "/etc/master.passwd",
    "/private/etc/shadow",
    "/private/etc/sudoers",
    "/var/db/sudo",
    "/private/var/db/sudo",
];
const BLOCKED_SYSTEM_PREFIXES: &[&str] =
    &["/etc/ssh/", "/private/etc/ssh/", "/var/run/secrets/", "/run/secrets/"];

/// Python `_is_path_allowed` (py:93-121). Case-INSENSITIVE on purpose:
/// macOS/APFS is case-insensitive by default and a case-sensitive check let
/// `~/.SSH/id_ed25519` through (Python's own docstring records the
/// incident). Casefolding errs toward denying on case-sensitive volumes.
pub fn is_path_allowed(p: &Path) -> bool {
    let resolved = resolve_nonstrict(p);
    let resolved_lower = resolved.to_string_lossy().to_lowercase();
    if BLOCKED_SYSTEM_PATHS.iter().any(|b| resolved_lower == b.to_lowercase()) {
        return false;
    }
    if BLOCKED_SYSTEM_PREFIXES.iter().any(|pfx| resolved_lower.starts_with(&pfx.to_lowercase())) {
        return false;
    }
    let home = resolve_nonstrict(&home_dir());
    if let Ok(rel) = resolved.strip_prefix(&home) {
        let parts: Vec<String> = rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(s) => Some(s.to_string_lossy().to_lowercase()),
                _ => None,
            })
            .collect();
        for sensitive in SENSITIVE_HOME {
            let sens: Vec<&str> = sensitive.split('/').collect();
            if parts.len() >= sens.len() && parts[..sens.len()].iter().map(String::as_str).eq(sens.iter().copied())
            {
                return false;
            }
        }
    }
    true
}

/// Python `_DANGEROUS_WRITE_*` + `_is_dangerous_write` (py:670-698):
/// basenames/suffixes/dir-markers whose write is a code-execution primitive,
/// checked on upload in ADDITION to `is_path_allowed`.
const DANGEROUS_WRITE_BASENAMES: &[&str] = &[
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".bash_login",
    ".zlogin",
    ".kshrc",
    ".cshrc",
    ".tcshrc",
    ".login",
    "authorized_keys",
    ".netrc",
    ".pam_environment",
    ".forward",
    "config.fish",
    "crontab",
];
const DANGEROUS_WRITE_SUFFIXES: &[&str] = &[".plist", ".command", ".desktop", ".scpt", ".terminal"];
const DANGEROUS_WRITE_DIR_MARKERS: &[&str] = &[
    "/launchagents/",
    "/launchdaemons/",
    "/.config/autostart/",
    "/library/launchagents/",
    "/library/launchdaemons/",
    "/.config/systemd/",
    "/bin/",
    "/sbin/",
    "/.git/hooks/",
];

pub fn is_dangerous_write(p: &Path) -> bool {
    let resolved = resolve_nonstrict(p);
    let name = resolved
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let low = resolved.to_string_lossy().to_lowercase();
    if DANGEROUS_WRITE_BASENAMES.contains(&name.as_str()) {
        return true;
    }
    if let Some(ext) = resolved.extension() {
        let suffix = format!(".{}", ext.to_string_lossy().to_lowercase());
        if DANGEROUS_WRITE_SUFFIXES.contains(&suffix.as_str()) {
            return true;
        }
    }
    // Python checks markers against `str(resolved).lower() + "/"` so a path
    // ENDING in e.g. `.../bin` also matches "/bin/".
    let hay = format!("{low}/");
    DANGEROUS_WRITE_DIR_MARKERS.iter().any(|m| hay.contains(m))
}

pub(crate) fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// POST /api/fs/mkdir (py:67883-67899)
// ---------------------------------------------------------------------------

async fn mkdir(req: Request) -> Response {
    if req.method() != Method::POST {
        return not_found().await;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let body = match parse_body(&bytes) {
        Ok(v) => v,
        Err(e) => return j(500, json!({"error": e})),
    };
    let fpath = body_str(&body, "path").trim().to_string();
    if fpath.is_empty() {
        return j(400, json!({"error": "missing path"}));
    }
    let p = expanduser(&fpath);
    if !p.is_absolute() {
        return j(400, json!({"error": "absolute path required"}));
    }
    if !is_path_allowed(&p) {
        return j(403, json!({"error": "access denied"}));
    }
    // Python: p.mkdir(parents=True, exist_ok=False) — existing FINAL dir is
    // the 409; parents are created freely.
    if p.exists() {
        return j(409, json!({"error": "already exists"}));
    }
    match std::fs::create_dir_all(&p) {
        // Response path is the EXPANDED, unresolved path (Python str(p)).
        Ok(()) => j(200, json!({"ok": true, "path": pystr(&p)})),
        Err(e) => j(500, json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// POST /api/fs/open (py:68339-68367) — reveal in native file manager
// ---------------------------------------------------------------------------

async fn open_native(req: Request) -> Response {
    if req.method() != Method::POST {
        return not_found().await;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let body = match parse_body(&bytes) {
        Ok(v) => v,
        Err(e) => return j(500, json!({"error": e})),
    };
    let dir_path = body.get("path").and_then(|v| v.as_str()).unwrap_or("/").to_string();
    let mut target = resolve_nonstrict(&expanduser(&dir_path));
    // Same containment as the other file APIs — prevents an existence oracle
    // AND `open`-launching an arbitrary .app bundle (Python's comment).
    if !is_path_allowed(&target) {
        return j(403, json!({"error": "access denied"}));
    }
    if !target.exists() {
        return j(404, json!({"error": "path not found"}));
    }
    if target.is_file() {
        target = target.parent().map(Path::to_path_buf).unwrap_or(target);
    }
    let cmd = match std::env::consts::OS {
        "macos" => "open",
        "linux" => "xdg-open",
        "windows" => "explorer",
        other => return j(400, json!({"error": format!("unsupported platform: {other}")})),
    };
    match std::process::Command::new(cmd).arg(&target).spawn() {
        Ok(_) => j(200, json!({"ok": true, "path": pystr(&target)})),
        Err(e) => j(500, json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// POST /api/fs/upload (py:68369-68430) — multipart, `dir` field
// ---------------------------------------------------------------------------

/// One decoded multipart part. Mirrors what Python reads off the `email`
/// parser: the Content-Disposition `name`/`filename` params and the
/// CTE-decoded payload.
#[derive(Debug)]
struct MPart {
    name: Option<String>,
    filename: Option<String>,
    data: Vec<u8>,
}

/// Header-param extraction (`name="x"`, `filename="y"`), tolerant of
/// unquoted values and RFC2231 `filename*=utf-8''...` (which Python's
/// `get_param` also decodes).
fn disposition_param(header: &str, param: &str) -> Option<String> {
    for seg in header.split(';').map(str::trim) {
        let Some((k, v)) = seg.split_once('=') else { continue };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        if k == format!("{param}*") {
            // RFC2231: charset'lang'percent-encoded
            let enc = v.trim_matches('"');
            let enc = enc.splitn(3, '\'').nth(2).unwrap_or(enc);
            let decoded = parse_qs(&format!("k={}", enc.replace('+', "%2B")));
            return decoded.into_iter().next().map(|(_, v)| v);
        }
        if k == param {
            let v = v.strip_prefix('"').unwrap_or(v);
            let v = v.strip_suffix('"').unwrap_or(v);
            return Some(v.replace("\\\"", "\""));
        }
    }
    None
}

fn boundary_of(ctype: &str) -> Option<String> {
    for seg in ctype.split(';').map(str::trim) {
        if let Some((k, v)) = seg.split_once('=') {
            if k.trim().eq_ignore_ascii_case("boundary") {
                let v = v.trim().trim_matches('"');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Decode per Content-Transfer-Encoding, as Python's
/// `part.get_payload(decode=True)` does (base64 / quoted-printable /
/// identity for 7bit, 8bit, binary and absent).
fn decode_cte(cte: &str, data: &[u8]) -> Vec<u8> {
    match cte.trim().to_ascii_lowercase().as_str() {
        "base64" => {
            let compact: Vec<u8> =
                data.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
            base64::engine::general_purpose::STANDARD
                .decode(&compact)
                .unwrap_or_else(|_| data.to_vec())
        }
        "quoted-printable" => {
            let mut out = Vec::with_capacity(data.len());
            let mut i = 0;
            while i < data.len() {
                if data[i] == b'=' {
                    if data[i + 1..].starts_with(b"\r\n") {
                        i += 3;
                        continue;
                    }
                    if data[i + 1..].starts_with(b"\n") {
                        i += 2;
                        continue;
                    }
                    if i + 2 < data.len() {
                        if let Ok(b) = u8::from_str_radix(
                            &String::from_utf8_lossy(&data[i + 1..i + 3]),
                            16,
                        ) {
                            out.push(b);
                            i += 3;
                            continue;
                        }
                    }
                }
                out.push(data[i]);
                i += 1;
            }
            out
        }
        _ => data.to_vec(),
    }
}

/// Minimal multipart/form-data parser matching what Python's `email`
/// module (compat32) yields for browser/curl bodies. Returns None when the
/// body does not decompose into parts (Python's "invalid multipart" 400:
/// `msg.get_payload()` not a list).
fn parse_multipart(ctype: &str, body: &[u8]) -> Option<Vec<MPart>> {
    let boundary = boundary_of(ctype)?;
    let delim: Vec<u8> = [b"--", boundary.as_bytes()].concat();

    // Positions of boundary LINES (start-of-input or preceded by a newline).
    let mut marks: Vec<(usize, bool)> = Vec::new(); // (offset of delim, is_close)
    let mut i = 0;
    while i + delim.len() <= body.len() {
        if body[i..].starts_with(&delim) && (i == 0 || body[i - 1] == b'\n') {
            let after = &body[i + delim.len()..];
            let is_close = after.starts_with(b"--");
            marks.push((i, is_close));
            if is_close {
                break;
            }
        }
        i += 1;
    }
    if marks.is_empty() {
        return None;
    }

    let mut parts = Vec::new();
    for w in 0..marks.len() {
        let (start, is_close) = marks[w];
        if is_close {
            break;
        }
        // Part content runs from the end of this boundary line to just
        // before the next boundary line (minus its preceding CRLF/LF).
        let mut content_start = start + delim.len();
        if body[content_start..].starts_with(b"\r\n") {
            content_start += 2;
        } else if body[content_start..].starts_with(b"\n") {
            content_start += 1;
        }
        let content_end = if w + 1 < marks.len() {
            let next = marks[w + 1].0;
            if next >= 2 && &body[next - 2..next] == b"\r\n" {
                next - 2
            } else if next >= 1 && body[next - 1] == b'\n' {
                next - 1
            } else {
                next
            }
        } else {
            body.len()
        };
        if content_start > content_end {
            continue;
        }
        let raw = &body[content_start..content_end];
        // Split headers from payload at the first blank line.
        let (head, payload) = if let Some(pos) = find(raw, b"\r\n\r\n") {
            (&raw[..pos], &raw[pos + 4..])
        } else if let Some(pos) = find(raw, b"\n\n") {
            (&raw[..pos], &raw[pos + 2..])
        } else {
            (&[][..], raw)
        };
        let head = String::from_utf8_lossy(head);
        let mut name = None;
        let mut filename = None;
        let mut cte = String::new();
        for line in head.lines() {
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("content-disposition:") {
                name = disposition_param(line, "name");
                filename = disposition_param(line, "filename");
            } else if let Some(v) = lower.strip_prefix("content-transfer-encoding:") {
                cte = v.trim().to_string();
            }
        }
        parts.push(MPart { name, filename, data: decode_cte(&cte, payload) });
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Python's upload-name sanitizer (py:68411):
/// `re.sub(r'[^\w.\- ]', '_', Path(filename).name)[:240] or "upload"`.
fn sanitize_upload_name(filename: &str) -> String {
    let base = Path::new(filename)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"[^\w.\- ]").expect("sanitize regex"));
    let cleaned: String = re.replace_all(&base, "_").chars().take(240).collect();
    if cleaned.is_empty() {
        "upload".into()
    } else {
        cleaned
    }
}

async fn upload(req: Request) -> Response {
    if req.method() != Method::POST {
        return not_found().await;
    }
    let ctype = req
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !ctype.contains("multipart/form-data") {
        return j(400, json!({"error": "expected multipart/form-data"}));
    }
    // Unbounded buffer on purpose: parity with Python's
    // `rfile.read(length)` (py:68372-68374 — "No size limit").
    let raw = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let Some(parts) = parse_multipart(&ctype, &raw) else {
        return j(400, json!({"error": "invalid multipart"}));
    };

    let mut target_dir: Option<String> = None;
    let mut overwrite = false;
    for part in &parts {
        match part.name.as_deref() {
            Some("dir") => {
                target_dir =
                    Some(String::from_utf8_lossy(&part.data).trim().to_string());
            }
            Some("overwrite") => {
                let v = String::from_utf8_lossy(&part.data).trim().to_lowercase();
                overwrite = matches!(v.as_str(), "1" | "true" | "yes" | "on");
            }
            _ => {}
        }
    }
    let target_dir = match target_dir {
        Some(d) if !d.is_empty() => d,
        _ => return j(400, json!({"error": "missing 'dir' field"})),
    };
    let dest_dir = resolve_nonstrict(&expanduser(&target_dir));
    if !is_path_allowed(&dest_dir) {
        return j(403, json!({"error": "access denied"}));
    }
    if !dest_dir.is_dir() {
        return j(400, json!({"error": format!("not a directory: {target_dir}")}));
    }

    let mut saved: Vec<Value> = Vec::new();
    for part in &parts {
        let Some(filename) = part.filename.as_deref().filter(|f| !f.is_empty()) else {
            continue;
        };
        let safe_name = sanitize_upload_name(filename);
        let mut dest = dest_dir.join(&safe_name);
        // Refuse code-execution-vector uploads (launch agents, shell rc,
        // .plist/.command/.desktop) — uploads have no extension allowlist.
        if is_dangerous_write(&dest) {
            saved.push(json!({"name": safe_name, "error": "refused: could execute code"}));
            continue;
        }
        // Never clobber by default — a person dragging a file into the
        // dashboard must not lose the one already there; `overwrite=1` is
        // the declarative opt-in (py:68414-68424, the re-seed lesson).
        if dest.exists() && !overwrite {
            let stem = dest
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let suffix = dest
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            let mut i = 1;
            while dest.exists() {
                dest = dest_dir.join(format!("{stem}_{i}{suffix}"));
                i += 1;
            }
        }
        if let Err(e) = std::fs::write(&dest, &part.data) {
            return j(500, json!({"error": e.to_string()}));
        }
        let final_name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(safe_name);
        saved.push(json!({"name": final_name, "size": part.data.len()}));
    }
    j(200, json!({"saved": saved}))
}

// ---------------------------------------------------------------------------
// POST /api/fs/rename (py:68435-68458)
// ---------------------------------------------------------------------------

async fn rename(req: Request) -> Response {
    if req.method() != Method::POST {
        return not_found().await;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let body = match parse_body(&bytes) {
        Ok(v) => v,
        Err(e) => return j(500, json!({"error": e})),
    };
    let src_path = body_str(&body, "path").trim().to_string();
    let new_name = body_str(&body, "new_name").trim().to_string();
    if src_path.is_empty() || new_name.is_empty() {
        return j(400, json!({"error": "missing 'path' or 'new_name'"}));
    }
    if new_name.contains('/') || new_name.contains('\0') || new_name == "." || new_name == ".." {
        return j(400, json!({"error": "invalid name"}));
    }
    let src = resolve_nonstrict(&expanduser(&src_path));
    if !is_path_allowed(&src) {
        return j(403, json!({"error": "access denied"}));
    }
    if !src.exists() {
        return j(404, json!({"error": "not found"}));
    }
    let dst = src.parent().unwrap_or(Path::new("/")).join(&new_name);
    if !is_path_allowed(&dst) {
        return j(403, json!({"error": "access denied"}));
    }
    if dst.exists() {
        return j(409, json!({"error": "target already exists"}));
    }
    match std::fs::rename(&src, &dst) {
        Ok(()) => j(200, json!({"ok": true, "path": pystr(&dst)})),
        Err(e) => j(500, json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// GET /api/fs/read (py:68468-68500)
// ---------------------------------------------------------------------------

async fn read_file(method: Method, RawQuery(q): RawQuery) -> Response {
    if method != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(q.as_deref().unwrap_or(""));
    let target_path = qs_get(&qs, "path").unwrap_or("").trim().to_string();
    if target_path.is_empty() {
        return j(400, json!({"error": "missing 'path'"}));
    }
    let target = expanduser(&target_path);
    if !is_path_allowed(&target) {
        return j(403, json!({"error": "access denied"}));
    }
    let target = resolve_nonstrict(&target);
    if !target.exists() {
        return j(404, json!({"error": "not found", "path": pystr(&target)}));
    }
    if target.is_dir() {
        return j(
            400,
            json!({"error": "is a directory — use /api/fs/list", "path": pystr(&target)}),
        );
    }
    let meta = match std::fs::metadata(&target) {
        Ok(m) => m,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let size = meta.len();
    // Python: cap = min(int(max_bytes or 1048576), 8MiB); an unparsable
    // value is Python's generic 500. Negative caps read the WHOLE file
    // (Python's file.read(negative)).
    let raw_cap = qs_get(&qs, "max_bytes").filter(|v| !v.is_empty()).unwrap_or("1048576");
    let cap: i64 = match raw_cap.parse::<i64>() {
        Ok(v) => v.min(8 * 1024 * 1024),
        Err(_) => {
            return j(
                500,
                json!({"error": format!("invalid literal for int() with base 10: '{raw_cap}'")}),
            )
        }
    };
    let raw = if cap < 0 {
        std::fs::read(&target)
    } else {
        use std::io::Read;
        std::fs::File::open(&target).and_then(|f| {
            let mut buf = Vec::new();
            f.take(cap as u64).read_to_end(&mut buf).map(|_| buf)
        })
    };
    let raw = match raw {
        Ok(b) => b,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    // "Binary" is EXACTLY "not valid UTF-8" — a truncated multibyte char at
    // the cap boundary flips a text file to base64 (fixture-verified).
    let (content, encoding) = match std::str::from_utf8(&raw) {
        Ok(text) => (Value::String(text.to_string()), "utf-8"),
        Err(_) => (
            Value::String(base64::engine::general_purpose::STANDARD.encode(&raw)),
            "base64",
        ),
    };
    j(
        200,
        json!({
            "path": pystr(&target),
            "size": size,
            "returned": raw.len(),
            "truncated": size as usize > raw.len(),
            "encoding": encoding,
            "content": content,
            "modified": mtime_secs(&meta),
        }),
    )
}

// ---------------------------------------------------------------------------
// GET /api/fs/search (py:68505-68521; core `_fs_search` py:21103-21210)
// ---------------------------------------------------------------------------

fn search_max_results() -> i64 {
    std::env::var("AMUX_SEARCH_MAX_RESULTS").ok().and_then(|v| v.parse().ok()).unwrap_or(300)
}
fn search_timeout_s() -> f64 {
    std::env::var("AMUX_SEARCH_TIMEOUT_S").ok().and_then(|v| v.parse().ok()).unwrap_or(20.0)
}
fn search_max_filesize() -> String {
    std::env::var("AMUX_SEARCH_MAX_FILESIZE").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "20M".into())
}

/// The rg binary. Python's error text promises "set AMUX_SEARCH_RG to its
/// path" but its `_RG_BIN` only consults PATH (py:21056) — the promise was
/// never implemented. This port implements it (ethos rule 6: either
/// implement the claim or delete it): env override first, then PATH.
fn rg_bin() -> Option<String> {
    if let Ok(v) = std::env::var("AMUX_SEARCH_RG") {
        if !v.trim().is_empty() {
            return Some(v);
        }
    }
    let paths = std::env::var("PATH").unwrap_or_default();
    for dir in paths.split(':') {
        let cand = Path::new(dir).join("rg");
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

fn chars_truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `_fs_search` ported field-for-field. Every early return matches Python's
/// (missing query, access denied, engine missing, timeout), and the
/// zero-result `note` names every filter that could be responsible — an
/// empty result set is the one answer a caller cannot debug from the rows.
pub(crate) async fn fs_search(
    root: &str,
    q: &str,
    limit: i64,
    literal: bool,
    case: &str,
    include_ignored: bool,
    globs: &[String],
) -> Map<String, Value> {
    let t0 = std::time::Instant::now();
    let mut out = Map::new();
    out.insert("query".into(), json!(q));
    out.insert("root".into(), json!(""));
    out.insert("engine".into(), json!(""));
    out.insert("results".into(), json!([]));
    out.insert("files".into(), json!(0));
    out.insert("matches".into(), json!(0));
    out.insert("truncated".into(), json!(false));
    out.insert("searched_ignored".into(), json!(include_ignored));
    out.insert("searched_hidden".into(), json!(include_ignored));
    out.insert(
        "limit".into(),
        json!(if limit != 0 { limit } else { search_max_results() }),
    );
    if q.is_empty() {
        out.insert("error".into(), json!("missing query"));
        return out;
    }
    let rp = expanduser(root);
    if !is_path_allowed(&rp) || !rp.is_dir() {
        out.insert("error".into(), json!("access denied or not a directory"));
        return out;
    }
    let rp = resolve_nonstrict(&rp);
    out.insert("root".into(), json!(pystr(&rp)));
    let cap = if limit != 0 { limit } else { search_max_results() };

    let Some(rg) = rg_bin() else {
        // Say so rather than silently returning nothing: a search reporting
        // zero because its ENGINE is missing must be distinguishable from
        // one that genuinely found nothing.
        out.insert("engine".into(), json!("none"));
        out.insert(
            "error".into(),
            json!("ripgrep (rg) not found on PATH — install it, or set AMUX_SEARCH_RG to its path"),
        );
        return out;
    };

    let max_filesize = search_max_filesize();
    out.insert("max_filesize".into(), json!(max_filesize));
    let mut cmd = tokio::process::Command::new(&rg);
    cmd.args(["--json", "--line-number", "--max-columns", "400"])
        .args(["--max-filesize", &max_filesize])
        .args(["--threads", "4"]);
    if literal {
        cmd.arg("--fixed-strings");
    }
    match case {
        "sensitive" => {
            cmd.arg("--case-sensitive");
        }
        "insensitive" => {
            cmd.arg("--ignore-case");
        }
        _ => {
            cmd.arg("--smart-case");
        }
    }
    if include_ignored {
        cmd.args(["--no-ignore", "--hidden"]);
    }
    for g in globs {
        cmd.args(["--glob", g]);
    }
    // -e marks the query as a PATTERN: without it a query starting with '-'
    // is re-read as a flag (the private-key-scan defect, py:21155).
    cmd.arg("-e").arg(q).arg("--").arg(&rp);
    cmd.stdin(std::process::Stdio::null());
    cmd.kill_on_drop(true);

    let timeout = std::time::Duration::from_secs_f64(search_timeout_s());
    let ran = tokio::time::timeout(timeout, cmd.output()).await;
    let output = match ran {
        Err(_) => {
            out.insert("engine".into(), json!("ripgrep"));
            out.insert(
                "error".into(),
                json!(format!("search timed out after {:.0}s", search_timeout_s())),
            );
            out.insert("truncated".into(), json!(true));
            return out;
        }
        Ok(Err(e)) => {
            out.insert("engine".into(), json!("ripgrep"));
            out.insert("error".into(), json!(e.to_string()));
            return out;
        }
        Ok(Ok(o)) => o,
    };

    out.insert("engine".into(), json!("ripgrep"));
    let mut results: Vec<Value> = Vec::new();
    let mut seen_files: std::collections::BTreeSet<String> = Default::default();
    let mut truncated = false;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if results.len() as i64 >= cap {
            truncated = true;
            break;
        }
        let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
        if ev.get("type").and_then(|t| t.as_str()) != Some("match") {
            continue;
        }
        let d = ev.get("data").cloned().unwrap_or(json!({}));
        let pth = d["path"]["text"].as_str().unwrap_or("").to_string();
        let txt = d["lines"]["text"].as_str().unwrap_or("").trim_end_matches('\n');
        let rel = Path::new(&pth)
            .strip_prefix(&rp)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_else(|_| pth.clone());
        seen_files.insert(rel.clone());
        let spans: Vec<Value> = d["submatches"]
            .as_array()
            .map(|subs| {
                subs.iter().map(|s| json!([s.get("start"), s.get("end")])).collect()
            })
            .unwrap_or_default();
        results.push(json!({
            "path": rel,
            "abs": pth,
            // rg's match event carries line_number inside `data`; absent
            // (it never is for --line-number) Python emits null.
            "line": d.get("line_number").cloned().unwrap_or(Value::Null),
            "text": chars_truncate(txt, 400),
            "spans": spans,
        }));
    }
    out.insert("files".into(), json!(seen_files.len()));
    out.insert("matches".into(), json!(results.len()));
    let n_results = results.len();
    out.insert("truncated".into(), json!(truncated));
    out.insert("results".into(), Value::Array(results));
    out.insert("elapsed_ms".into(), json!(t0.elapsed().as_millis() as i64));
    // rg exits 1 on "no matches" — that is not an error. Anything above 1 is.
    let code = output.status.code().unwrap_or(0);
    if code > 1 && n_results == 0 {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let msg = if stderr.is_empty() {
            format!("rg exit {code}")
        } else {
            chars_truncate(&stderr, 400)
        };
        out.insert("error".into(), json!(msg));
    }
    if n_results == 0 {
        // Name every filter that could be responsible for the zero.
        let mut why = vec![format!("files over {max_filesize} and binaries were skipped")];
        if !include_ignored {
            why.insert(
                0,
                ".gitignore'd and hidden files were NOT searched (retry with ignored=1)".into(),
            );
        }
        out.insert("note".into(), json!(format!("No matches. {}.", why.join("; "))));
    }
    out
}

async fn search(method: Method, RawQuery(q): RawQuery) -> Response {
    if method != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(q.as_deref().unwrap_or(""));
    let sp = qs_get(&qs, "path").unwrap_or("").trim().to_string();
    let sq = qs_get(&qs, "q").unwrap_or("").trim().to_string();
    if sp.is_empty() {
        return j(400, json!({"error": "missing 'path'"}));
    }
    let limit = qs_get(&qs, "limit")
        .unwrap_or("")
        .trim()
        .parse::<i64>()
        .ok()
        .map(|v| v.clamp(1, 2000))
        .unwrap_or(0);
    let literal = !matches!(
        qs_get(&qs, "literal").unwrap_or("1").to_lowercase().as_str(),
        "0" | "false" | "no"
    );
    let case = qs_get(&qs, "case").filter(|v| !v.is_empty()).unwrap_or("smart").to_lowercase();
    let include_ignored =
        matches!(qs_get(&qs, "ignored").unwrap_or("0").to_lowercase().as_str(), "1" | "true" | "yes");
    let globs: Vec<String> = qs
        .iter()
        .filter(|(k, v)| k == "glob" && !v.is_empty())
        .map(|(_, v)| v.clone())
        .collect();
    let res = fs_search(&sp, &sq, limit, literal, &case, include_ignored, &globs).await;
    let status = if res.get("error").and_then(|e| e.as_str()) == Some("missing query") {
        400
    } else {
        200
    };
    j(status, Value::Object(res))
}

// ---------------------------------------------------------------------------
// GET /api/fs/list (py:68523-68554)
// ---------------------------------------------------------------------------

async fn list_dir(method: Method, RawQuery(q): RawQuery) -> Response {
    if method != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(q.as_deref().unwrap_or(""));
    let target_path = qs_get(&qs, "path").unwrap_or("").trim().to_string();
    if target_path.is_empty() {
        return j(400, json!({"error": "missing 'path'"}));
    }
    let target = expanduser(&target_path);
    if !is_path_allowed(&target) {
        return j(403, json!({"error": "access denied"}));
    }
    let target = resolve_nonstrict(&target);
    if !target.exists() {
        return j(404, json!({"error": "not found", "path": pystr(&target)}));
    }
    if !target.is_dir() {
        return j(
            400,
            json!({"error": "not a directory — use /api/fs/read", "path": pystr(&target)}),
        );
    }
    let rd = match retry_eintr(|| std::fs::read_dir(&target)) {
        Ok(rd) => rd,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    // Python: sorted(iterdir, key=(not is_dir, name.lower())) — dirs first,
    // then case-folded name; broken symlinks sort as files and render as
    // {"name", "error": "unreadable"} (stat fails, is_dir() is False).
    let mut items: Vec<(bool, String, String, Option<std::fs::Metadata>)> = rd
        .flatten()
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = std::fs::metadata(e.path()).ok(); // follows symlinks, like Path.stat()
            let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
            (!is_dir, name.to_lowercase(), name, meta)
        })
        .collect();
    items.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let total = items.len();
    let entries: Vec<Value> = items
        .into_iter()
        .take(1000)
        .map(|(not_dir, _, name, meta)| match meta {
            Some(m) => json!({
                "name": name,
                "dir": !not_dir,
                "size": m.len(),
                "modified": mtime_secs(&m),
            }),
            None => json!({"name": name, "error": "unreadable"}),
        })
        .collect();
    // count is emitted separately from the (capped) list so a truncated
    // listing can never read as a complete short one (py:68545-68551).
    let mut out = Map::new();
    out.insert("path".into(), json!(pystr(&target)));
    out.insert("count".into(), json!(total));
    out.insert("entries".into(), Value::Array(entries));
    if total > 1000 {
        out.insert("truncated".into(), json!(true));
    }
    j(200, Value::Object(out))
}

// ---------------------------------------------------------------------------
// GET /api/ls (py:68308-68336) — the SPA Files browser's listing. Mounted at
// the TOP level (mod.rs), not under /api/fs. Differences from /api/fs/list
// that are part of the contract: hidden entries are FILTERED unless
// hidden=1, a stat failure OMITS the entry (fs/list reports "unreadable"),
// dirs carry size null, a missing path is 400 "not a directory" (resolve
// succeeds, is_dir fails), and there is no 1000-entry cap.
// ---------------------------------------------------------------------------

/// Retry a syscall that failed with `EINTR`.
///
/// `EINTR` is not a failure. It means a signal arrived while the call was
/// blocked, and the only correct response is to try again — but neither
/// `std::fs::read_dir` nor `std::fs::metadata` retries for you.
///
/// This matters here specifically because the server spawns child processes
/// constantly (tmux, git, `du`), so `SIGCHLD` is routine, and listing a large
/// directory is a wide enough window to catch one. When that happened, `ls`
/// fell through to its catch-all arm and returned HTTP 500 carrying the raw
/// `io::Error` string, so the Files browser told the user
/// "Interrupted system call (os error 4)" — which reads as a broken directory
/// rather than a retryable blip, and is not actionable by anyone who sees it.
///
/// Bounded rather than unbounded: a genuine repeated `EINTR` should surface,
/// not spin.
fn retry_eintr<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    for _ in 0..4 {
        match f() {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
    f()
}

pub async fn ls(method: Method, RawQuery(q): RawQuery) -> Response {
    if method != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(q.as_deref().unwrap_or(""));
    let ls_path = qs_get(&qs, "path").unwrap_or("").to_string();
    if ls_path.is_empty() {
        return j(400, json!({"error": "missing path"}));
    }
    let show_hidden = qs_get(&qs, "hidden").unwrap_or("0") == "1";
    let p = resolve_nonstrict(&expanduser(&ls_path));
    if !is_path_allowed(&p) {
        return j(403, json!({"error": "access denied"}));
    }
    if !p.is_dir() {
        return j(400, json!({"error": "not a directory"}));
    }
    let rd = match retry_eintr(|| std::fs::read_dir(&p)) {
        Ok(rd) => rd,
        // Python: PermissionError on iterdir → 403.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return j(403, json!({"error": "permission denied"}))
        }
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let mut items: Vec<(bool, String, String, std::fs::Metadata)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                return None;
            }
            // Python: st = item.stat() inside try — a failing stat (broken
            // symlink) SKIPS the entry.
            let meta = retry_eintr(|| std::fs::metadata(e.path())).ok()?;
            let is_dir = meta.is_dir();
            Some((!is_dir, name.to_lowercase(), name, meta))
        })
        .collect();
    items.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let entries: Vec<Value> = items
        .into_iter()
        .map(|(not_dir, _, name, meta)| {
            json!({
                "name": name,
                "type": if not_dir { "file" } else { "dir" },
                // Python: st_size if item.is_file() else None — dirs null.
                "size": if !not_dir { Value::Null } else { json!(meta.len()) },
                "modified": mtime_secs(&meta),
            })
        })
        .collect();
    let parent = p.parent().filter(|par| *par != p).map(pystr);
    j(200, json!({"path": pystr(&p), "parent": parent, "entries": entries}))
}

// ---------------------------------------------------------------------------
// GET /api/autocomplete/dir (py:68255-68285) — dir suggestions for the
// new-session / browse inputs. Bare-array response; every failure mode is
// an empty array, never an error (the input keeps working mid-keystroke).
// ---------------------------------------------------------------------------

pub async fn autocomplete_dir(method: Method, RawQuery(q): RawQuery) -> Response {
    if method != Method::GET {
        return not_found().await;
    }
    let qs = parse_qs(q.as_deref().unwrap_or(""));
    let query = qs_get(&qs, "q").unwrap_or("").to_string();
    if query.is_empty() {
        return j(200, json!([]));
    }
    let p = expanduser(&query);
    if !is_path_allowed(&p) {
        return j(200, json!([]));
    }
    // Query ending in "/" lists that dir; otherwise complete the last
    // component against its parent.
    let (parent, prefix) = if query.ends_with('/') && p.is_dir() {
        (p.clone(), String::new())
    } else {
        (
            p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/")),
            p.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default(),
        )
    };
    if !parent.is_dir() {
        return j(200, json!([]));
    }
    let rd = match retry_eintr(|| std::fs::read_dir(&parent)) {
        Ok(rd) => rd,
        Err(_) => return j(200, json!([])), // PermissionError → []
    };
    let mut names: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    names.sort();
    let mut results: Vec<String> = Vec::new();
    for item in names {
        let name = item.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if !is_path_allowed(&item) {
            continue;
        }
        if item.is_dir() && name.to_lowercase().starts_with(&prefix) {
            results.push(format!("{}/", pystr(&item)));
            if results.len() >= 10 {
                break;
            }
        }
    }
    j(200, json!(results))
}

// ---------------------------------------------------------------------------
// DELETE /api/fs/delete (py:68556-68570)
// ---------------------------------------------------------------------------

async fn delete_path(req: Request) -> Response {
    if req.method() != Method::DELETE {
        return not_found().await;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return j(500, json!({"error": e.to_string()})),
    };
    let body = match parse_body(&bytes) {
        Ok(v) => v,
        Err(e) => return j(500, json!({"error": e})),
    };
    let target_path = body_str(&body, "path");
    if target_path.is_empty() {
        return j(400, json!({"error": "missing 'path'"}));
    }
    let target = resolve_nonstrict(&expanduser(&target_path));
    if !is_path_allowed(&target) {
        return j(403, json!({"error": "access denied"}));
    }
    if !target.exists() {
        return j(404, json!({"error": "not found"}));
    }
    let res = if target.is_dir() {
        std::fs::remove_dir_all(&target)
    } else {
        std::fs::remove_file(&target)
    };
    match res {
        Ok(()) => j(200, json!({"ok": true, "deleted": pystr(&target)})),
        Err(e) => j(500, json!({"error": e.to_string()})),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {

    use std::cell::Cell;
    use std::io::{Error, ErrorKind};

    /// A transient EINTR must be retried, not surfaced. This is the incident:
    /// the Files browser showed "Interrupted system call (os error 4)" for a
    /// directory that was perfectly readable a moment later.
    #[test]
    fn retry_eintr_retries_past_a_transient_interrupt() {
        let calls = Cell::new(0);
        let got = super::retry_eintr(|| {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(Error::new(ErrorKind::Interrupted, "eintr"))
            } else {
                Ok(42)
            }
        });
        assert_eq!(got.unwrap(), 42);
        assert_eq!(calls.get(), 3, "should have retried until it succeeded");
    }

    /// CONTROL, and the one that makes the two tests around it mean anything:
    /// a non-EINTR error must come straight back, un-retried. Without this, a
    /// helper that blindly retried EVERY error would pass the other two — and
    /// would turn a real PermissionDenied into four syscalls and the same
    /// error, hiding a genuine fault behind a retry loop.
    #[test]
    fn retry_eintr_does_not_retry_other_errors() {
        let calls = Cell::new(0);
        let got: std::io::Result<u8> = super::retry_eintr(|| {
            calls.set(calls.get() + 1);
            Err(Error::new(ErrorKind::PermissionDenied, "nope"))
        });
        assert_eq!(got.unwrap_err().kind(), ErrorKind::PermissionDenied);
        assert_eq!(calls.get(), 1, "a non-EINTR error must not be retried");
    }

    /// A PERSISTENT EINTR must still terminate and surface, rather than spin.
    /// Bounded retry is the point; an unbounded one trades a visible error for
    /// a hung request, which is strictly worse to diagnose.
    #[test]
    fn retry_eintr_surfaces_a_persistent_interrupt() {
        let calls = Cell::new(0);
        let got: std::io::Result<u8> = super::retry_eintr(|| {
            calls.set(calls.get() + 1);
            Err(Error::new(ErrorKind::Interrupted, "eintr"))
        });
        assert_eq!(got.unwrap_err().kind(), ErrorKind::Interrupted);
        assert!(calls.get() <= 8, "retry must be bounded, got {} calls", calls.get());
    }
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        AppState {
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        }
    }

    fn app() -> Router {
        Router::new().nest("/api/fs", routes()).with_state(state())
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        ctype: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, Value) {
        let mut b = axum::http::Request::builder().method(method).uri(uri);
        if let Some(ct) = ctype {
            b = b.header("content-type", ct);
        }
        let res = app.clone().oneshot(b.body(Body::from(body)).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, v)
    }

    // ---- path guards ----

    #[test]
    fn path_allowed_blocks_sensitive_and_system_paths() {
        let home = home_dir();
        assert!(!is_path_allowed(&home.join(".ssh/id_ed25519")));
        // Case-insensitive: the ~/.SSH incident from Python's docstring.
        assert!(!is_path_allowed(&home.join(".SSH/id_ed25519")));
        assert!(!is_path_allowed(&home.join(".config/gcloud/credentials.db")));
        assert!(!is_path_allowed(Path::new("/etc/shadow")));
        assert!(!is_path_allowed(Path::new("/etc/ssh/sshd_config")));
        // A non-existent tail with `..` resolves lexically over the resolved
        // prefix (realpath semantics) — traversal cannot dodge the check.
        assert!(!is_path_allowed(&home.join("nope-dir/../.ssh/id_rsa")));
        assert!(is_path_allowed(Path::new("/tmp")));
        assert!(is_path_allowed(&home.join("Dev")));
    }

    #[test]
    fn symlink_into_sensitive_dir_is_denied() {
        let td = tempfile::tempdir().unwrap();
        let link = td.path().join("sneaky");
        std::os::unix::fs::symlink(home_dir().join(".ssh"), &link).unwrap();
        assert!(!is_path_allowed(&link.join("id_rsa")));
    }

    #[test]
    fn dangerous_write_matches_python_sets() {
        assert!(is_dangerous_write(Path::new("/tmp/x/.zshrc")));
        assert!(is_dangerous_write(Path::new("/tmp/Evil.PLIST")));
        assert!(is_dangerous_write(Path::new("/tmp/repo/.git/hooks/pre-commit")));
        assert!(is_dangerous_write(&home_dir().join("Library/LaunchAgents/com.x.plist")));
        // `low + "/"`: a path ENDING in /bin matches the /bin/ marker.
        assert!(is_dangerous_write(Path::new("/usr/local/bin")));
        assert!(!is_dangerous_write(Path::new("/tmp/notes.md")));
    }

    #[test]
    fn sanitize_matches_python_regex() {
        assert_eq!(sanitize_upload_name("report (final).pdf"), "report _final_.pdf");
        assert_eq!(sanitize_upload_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_upload_name(""), "upload");
        assert_eq!(sanitize_upload_name("naïve café.txt"), "naïve café.txt"); // \w is unicode
    }

    // ---- multipart ----

    fn mp_body(boundary: &str, fields: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, filename, data) in fields {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            match filename {
                Some(f) => out.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\r\n"
                    )
                    .as_bytes(),
                ),
                None => out.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                ),
            }
            out.extend_from_slice(data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        out
    }

    #[test]
    fn multipart_parses_fields_and_files() {
        let body = mp_body(
            "XX",
            &[
                ("dir", None, b"/tmp/dest"),
                ("file", Some("a.txt"), b"hello\r\nworld"),
            ],
        );
        let parts = parse_multipart("multipart/form-data; boundary=XX", &body).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name.as_deref(), Some("dir"));
        assert_eq!(parts[0].data, b"/tmp/dest");
        assert_eq!(parts[1].filename.as_deref(), Some("a.txt"));
        // Interior CRLF survives; only the boundary's own CRLF is stripped.
        assert_eq!(parts[1].data, b"hello\r\nworld");
    }

    #[test]
    fn multipart_binary_content_with_boundary_like_bytes() {
        // "--XX" INSIDE content, not at line start after CRLF+delim — must
        // not split the part.
        let body = mp_body("ZZ", &[("file", Some("b.bin"), b"data --XX more\x00\x01")]);
        let parts = parse_multipart("multipart/form-data; boundary=ZZ", &body).unwrap();
        assert_eq!(parts[0].data, b"data --XX more\x00\x01");
    }

    #[test]
    fn multipart_base64_cte_decodes() {
        let boundary = "BB";
        let mut body = Vec::new();
        body.extend_from_slice(b"--BB\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.bin\"\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--BB--\r\n");
        let parts =
            parse_multipart(&format!("multipart/form-data; boundary={boundary}"), &body).unwrap();
        assert_eq!(parts[0].data, b"hello");
    }

    #[test]
    fn multipart_invalid_shapes_are_none() {
        assert!(parse_multipart("multipart/form-data", b"whatever").is_none()); // no boundary param
        assert!(parse_multipart("multipart/form-data; boundary=QQ", b"no delimiter here").is_none());
    }

    // ---- endpoint behavior (statuses + bodies pinned to the Python source) ----

    #[tokio::test]
    async fn upload_saves_suffixes_and_refuses_dangerous() {
        let app = app();
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().to_string_lossy().into_owned();
        let body = mp_body(
            "XX",
            &[
                ("dir", None, dir.as_bytes()),
                ("file", Some("a.txt"), b"one"),
                ("file", Some("evil.plist"), b"<plist/>"),
            ],
        );
        let (st, v) = call(
            &app,
            "POST",
            "/api/fs/upload",
            Some("multipart/form-data; boundary=XX"),
            body.clone(),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert_eq!(v["saved"][0], json!({"name": "a.txt", "size": 3}));
        assert_eq!(
            v["saved"][1],
            json!({"name": "evil.plist", "error": "refused: could execute code"})
        );
        assert_eq!(std::fs::read_to_string(td.path().join("a.txt")).unwrap(), "one");
        assert!(!td.path().join("evil.plist").exists());

        // Second identical upload: no clobber, suffixes _1 (py:68414-68424).
        let (st, v) = call(
            &app,
            "POST",
            "/api/fs/upload",
            Some("multipart/form-data; boundary=XX"),
            body,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["saved"][0], json!({"name": "a_1.txt", "size": 3}));

        // overwrite=1 clobbers in place.
        let body = mp_body(
            "XX",
            &[
                ("dir", None, dir.as_bytes()),
                ("overwrite", None, b"1"),
                ("file", Some("a.txt"), b"newer"),
            ],
        );
        let (st, v) = call(
            &app,
            "POST",
            "/api/fs/upload",
            Some("multipart/form-data; boundary=XX"),
            body,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["saved"][0], json!({"name": "a.txt", "size": 5}));
        assert_eq!(std::fs::read_to_string(td.path().join("a.txt")).unwrap(), "newer");
    }

    #[tokio::test]
    async fn upload_error_contract() {
        let app = app();
        let (st, v) = call(&app, "POST", "/api/fs/upload", Some("application/json"), b"{}".to_vec()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "expected multipart/form-data");

        let body = mp_body("XX", &[("file", Some("a.txt"), b"x")]);
        let (st, v) = call(
            &app,
            "POST",
            "/api/fs/upload",
            Some("multipart/form-data; boundary=XX"),
            body,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "missing 'dir' field");

        let body = mp_body("XX", &[("dir", None, b"/no/such/dir-xyz"), ("file", Some("a.txt"), b"x")]);
        let (st, v) = call(
            &app,
            "POST",
            "/api/fs/upload",
            Some("multipart/form-data; boundary=XX"),
            body,
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "not a directory: /no/such/dir-xyz");
    }

    #[tokio::test]
    async fn mkdir_rename_delete_lifecycle() {
        let app = app();
        let td = tempfile::tempdir().unwrap();
        let newdir = td.path().join("made/nested");
        let (st, v) = call(
            &app,
            "POST",
            "/api/fs/mkdir",
            Some("application/json"),
            serde_json::to_vec(&json!({"path": newdir.to_str().unwrap()})).unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert!(newdir.is_dir());
        // Existing dir → Python's 409.
        let (st, v) = call(
            &app,
            "POST",
            "/api/fs/mkdir",
            Some("application/json"),
            serde_json::to_vec(&json!({"path": newdir.to_str().unwrap()})).unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
        assert_eq!(v["error"], "already exists");
        // Relative path → 400 before any containment check.
        let (st, v) =
            call(&app, "POST", "/api/fs/mkdir", Some("application/json"), b"{\"path\":\"rel/x\"}".to_vec())
                .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "absolute path required");

        // rename
        std::fs::write(td.path().join("old.txt"), "z").unwrap();
        let (st, v) = call(
            &app,
            "POST",
            "/api/fs/rename",
            Some("application/json"),
            serde_json::to_vec(
                &json!({"path": td.path().join("old.txt").to_str().unwrap(), "new_name": "new.txt"}),
            )
            .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert!(td.path().join("new.txt").exists());
        let (st, v) = call(
            &app,
            "POST",
            "/api/fs/rename",
            Some("application/json"),
            serde_json::to_vec(
                &json!({"path": td.path().join("new.txt").to_str().unwrap(), "new_name": "a/b"}),
            )
            .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "invalid name");

        // delete
        let (st, v) = call(
            &app,
            "DELETE",
            "/api/fs/delete",
            Some("application/json"),
            serde_json::to_vec(&json!({"path": td.path().join("new.txt").to_str().unwrap()}))
                .unwrap(),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{v}");
        assert!(!td.path().join("new.txt").exists());
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn wrong_method_is_pythons_generic_404_not_405() {
        let app = app();
        for (m, p) in [
            ("GET", "/api/fs/mkdir"),
            ("GET", "/api/fs/upload"),
            ("POST", "/api/fs/list"),
            ("POST", "/api/fs/read"),
            ("GET", "/api/fs/delete"),
            ("GET", "/api/fs"),
            ("GET", "/api/fs/definitely-not"),
        ] {
            let (st, v) = call(&app, m, p, None, vec![]).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{m} {p}");
            assert_eq!(v, json!({"error": "not found"}), "{m} {p}");
        }
    }

    #[tokio::test]
    async fn read_denied_paths_answer_403() {
        let app = app();
        let uri = format!(
            "/api/fs/read?path={}",
            urlencode(&home_dir().join(".ssh/config").to_string_lossy())
        );
        let (st, v) = call(&app, "GET", &uri, None, vec![]).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        assert_eq!(v["error"], "access denied");
    }

    fn urlencode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn read_truncation_flips_to_base64_like_python() {
        // The fixture case: "café\n" cut at 4 bytes lands mid-é → base64
        // "Y2Fmww==" (live-recorded from the Python server).
        let app = app();
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("utf8.txt");
        std::fs::write(&f, "café\n").unwrap();
        let uri = format!("/api/fs/read?path={}&max_bytes=4", urlencode(&f.to_string_lossy()));
        let (st, v) = call(&app, "GET", &uri, None, vec![]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["encoding"], "base64");
        assert_eq!(v["content"], "Y2Fmww==");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["returned"], 4);
        assert_eq!(v["size"], 6);
    }

    #[tokio::test]
    async fn search_contract_edges() {
        let app = app();
        // missing q → the ONE 400 in the search contract.
        let (st, v) = call(&app, "GET", "/api/fs/search?path=%2Ftmp", None, vec![]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "missing query");
        // bad root → 200 with the named error (Python returns 200 here).
        let (st, v) =
            call(&app, "GET", "/api/fs/search?path=%2Fno%2Fsuch-dir-xyz&q=x", None, vec![]).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["error"], "access denied or not a directory");
        assert_eq!(v["root"], "");
        // missing path → 400.
        let (st, v) = call(&app, "GET", "/api/fs/search?q=x", None, vec![]).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "missing 'path'");
    }
}
