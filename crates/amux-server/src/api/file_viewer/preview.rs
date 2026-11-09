//! Previews for the files a browser cannot show by itself (owner, 2026-11-08:
//! "make sure we have native previews of every file type", over a TIFF that
//! opened as a box and a Download button).
//!
//! - `/api/file/preview-image`: an image the browser cannot decode (TIFF, HEIC,
//!   PSD, ICNS, camera raw) as PNG, converted by the first tool this machine
//!   has (`sips` on a Mac, then ImageMagick, `heif-convert`, ffmpeg). An iWork
//!   file gives the preview image it carries. On a Mac, anything else Quick
//!   Look can draw. Cached under `~/.amux/cache/file-previews`.
//! - `/api/file/archive`: what is inside a zip, tar, 7z or similar.
//! - `/api/file/sqlite`: a SQLite database's tables, opened read-only.
//!
//! The viewer's binary answer names which of these fits (`preview`) and what
//! the bytes are (`kind`, from their magic number); every other file shows
//! its first bytes in hex, so no file is a box with only a Download button.
use super::{find_bin, j, parse_qs, qpath};
use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;

const TOOL_TIMEOUT: Duration = Duration::from_secs(30);
/// Quick Look either draws a file at once or has nothing for it.
const QUICKLOOK_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest side of a converted image.
const MAX_SIDE: u32 = 2400;
const ARCHIVE_MAX_ENTRIES: usize = 5000;
const SQLITE_MAX_TABLES: usize = 50;
const SQLITE_SAMPLE_ROWS: usize = 50;

/// Image formats a browser does not decode everywhere, so the server converts.
const CONVERT_EXTS: &[&str] = &[
    ".tif", ".tiff", ".heic", ".heif", ".psd", ".psb", ".icns", ".tga", ".jxl", ".jp2", ".exr", ".hdr", ".pcx",
    ".dng", ".cr2", ".cr3", ".nef", ".arw", ".orf", ".rw2", ".raf", ".srw", ".pef", ".raw", ".xcf", ".eps",
];
const IWORK_EXTS: &[&str] = &[".key", ".pages", ".numbers"];
const ARCHIVE_EXTS: &[&str] = &[
    ".zip", ".jar", ".war", ".apk", ".aab", ".ipa", ".whl", ".xpi", ".crx", ".nupkg", ".vsix",
    ".tar", ".tgz", ".gz", ".bz2", ".tbz", ".tbz2", ".xz", ".txz", ".zst", ".7z", ".rar", ".iso", ".cpio",
];
const FONT_EXTS: &[&str] = &[".ttf", ".otf", ".woff", ".woff2", ".ttc"];
const SQLITE_EXTS: &[&str] = &[".sqlite", ".sqlite3", ".db", ".db3", ".s3db", ".sl3"];

/// What the bytes are, from their first few, for the viewer's heading.
pub(super) fn magic_kind(head: &[u8]) -> Option<&'static str> {
    let at = |o: usize, sig: &[u8]| head.len() >= o + sig.len() && &head[o..o + sig.len()] == sig;
    Some(match () {
        _ if at(0, b"\x89PNG\r\n\x1a\n") => "PNG image",
        _ if at(0, b"\xff\xd8\xff") => "JPEG image",
        _ if at(0, b"GIF8") => "GIF image",
        _ if at(0, b"II*\0") || at(0, b"MM\0*") => "TIFF image",
        _ if at(0, b"8BPS") => "Photoshop document",
        _ if at(0, b"icns") => "macOS icon",
        _ if at(4, b"ftypheic") || at(4, b"ftypheix") || at(4, b"ftypmif1") || at(4, b"ftyphevc") => "HEIF image",
        _ if at(4, b"ftypavif") => "AVIF image",
        _ if at(4, b"ftyp") => "MP4/QuickTime media",
        _ if at(0, b"RIFF") && at(8, b"WEBP") => "WebP image",
        _ if at(0, b"RIFF") && at(8, b"WAVE") => "WAV audio",
        _ if at(0, b"RIFF") && at(8, b"AVI ") => "AVI video",
        _ if at(0, b"%PDF") => "PDF document",
        _ if at(0, b"PK\x03\x04") || at(0, b"PK\x05\x06") => "ZIP archive",
        _ if at(0, b"\x1f\x8b") => "gzip data",
        _ if at(0, b"BZh") => "bzip2 data",
        _ if at(0, b"\xfd7zXZ\0") => "xz data",
        _ if at(0, b"\x28\xb5\x2f\xfd") => "zstd data",
        _ if at(0, b"7z\xbc\xaf\x27\x1c") => "7-Zip archive",
        _ if at(0, b"Rar!\x1a\x07") => "RAR archive",
        _ if at(257, b"ustar") => "tar archive",
        _ if at(0, b"SQLite format 3\0") => "SQLite database",
        _ if at(0, b"\x7fELF") => "ELF executable",
        _ if at(0, b"\xcf\xfa\xed\xfe") || at(0, b"\xce\xfa\xed\xfe") || at(0, b"\xca\xfe\xba\xbe") => "Mach-O executable",
        _ if at(0, b"MZ") => "Windows executable",
        _ if at(0, b"\0asm") => "WebAssembly module",
        _ if at(0, b"wOFF") => "WOFF font",
        _ if at(0, b"wOF2") => "WOFF2 font",
        _ if at(0, b"OTTO") => "OpenType font",
        _ if at(0, b"\0\x01\0\0") => "TrueType font",
        _ if at(0, b"ttcf") => "TrueType font collection",
        _ if at(0, b"PAR1") => "Parquet data",
        _ if at(0, b"ARROW1") => "Arrow data",
        _ if at(0, b"\x93NUMPY") => "NumPy array",
        _ if at(0, b"OggS") => "Ogg media",
        _ if at(0, b"fLaC") => "FLAC audio",
        _ if at(0, b"ID3") => "MP3 audio",
        _ if at(0, b"\x1a\x45\xdf\xa3") => "Matroska/WebM media",
        _ if at(0, b"glTF") => "glTF 3D model",
        _ if at(0, b"bplist00") => "binary property list",
        _ if at(0, b"\x80\x04\x95") || at(0, b"\x80\x05\x95") => "Python pickle",
        _ => return None,
    })
}

fn has_ext(list: &[&str], ext: &str) -> bool {
    list.contains(&ext)
}

/// Which previewer the viewer should use for a binary file: `image`
/// (converted here), `archive`, `font`, `sqlite`, `quicklook` (a Mac's Quick
/// Look image, if it can draw one) or `hex`.
pub(super) fn preview_for(ext: &str, kind: Option<&str>) -> &'static str {
    let lower = ext.to_ascii_lowercase();
    let ext = lower.as_str();
    if has_ext(CONVERT_EXTS, ext) || has_ext(IWORK_EXTS, ext) || matches!(kind, Some("TIFF image" | "Photoshop document" | "HEIF image" | "macOS icon")) {
        return "image";
    }
    if has_ext(SQLITE_EXTS, ext) && kind.is_none_or(|k| k == "SQLite database") || kind == Some("SQLite database") {
        return "sqlite";
    }
    if has_ext(FONT_EXTS, ext) || kind.is_some_and(|k| k.ends_with(" font") || k == "TrueType font collection") {
        return "font";
    }
    if has_ext(ARCHIVE_EXTS, ext)
        || matches!(kind, Some("ZIP archive" | "tar archive" | "7-Zip archive" | "RAR archive"))
    {
        return "archive";
    }
    // Bytes of a known kind (an executable, a data file) say more in hex than
    // as Quick Look's icon; Quick Look is for kinds only the Mac knows.
    if kind.is_none() && quicklook_here() {
        return "quicklook";
    }
    "hex"
}

/// Quick Look's command line, on every Mac.
const QLMANAGE: &str = "/usr/bin/qlmanage";

fn quicklook_here() -> bool {
    cfg!(target_os = "macos") && Path::new(QLMANAGE).is_file()
}

/// Remove a scratch file or folder of ours. A failure only wastes disk, and
/// is logged.
fn clear(p: &Path) {
    let r = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
    if let Err(e) = r {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %p.display(), error = %e, measured = true, n_considered = 1,
                verdict = "file_preview_scratch_left", "a preview scratch file could not be removed");
        }
    }
}

fn bad(status: u16, msg: &str) -> Response {
    j(status, json!({ "error": msg }))
}

/// The file a preview route was asked for, checked like `/api/file/raw`.
fn target(req: &Request) -> Result<(PathBuf, std::fs::Metadata), Box<Response>> {
    if req.method() != Method::GET {
        return Err(Box::new(bad(405, "GET only")));
    }
    let qs = parse_qs(req.uri().query().unwrap_or(""));
    let p = qpath(&qs)?;
    if !super::is_path_allowed(&p) {
        return Err(Box::new(bad(403, "access denied")));
    }
    match std::fs::metadata(&p) {
        Ok(m) if m.is_file() => Ok((p, m)),
        _ => Err(Box::new(bad(404, "file not found"))),
    }
}

fn ext_of(p: &Path) -> String {
    p.extension().map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase())).unwrap_or_default()
}

/// Run a converter with a deadline; true when it exited 0.
async fn run(bin: &Path, args: &[&std::ffi::OsStr]) -> Result<(), String> {
    run_for(bin, args, TOOL_TIMEOUT).await
}

async fn run_for(bin: &Path, args: &[&std::ffi::OsStr], limit: Duration) -> Result<(), String> {
    let name = bin.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    finish(tokio::process::Command::new(bin).args(args), &name, limit).await
}

async fn finish(cmd: &mut tokio::process::Command, name: &str, limit: Duration) -> Result<(), String> {
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| format!("{name}: {e}"))?;
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(Ok(o)) if o.status.success() => Ok(()),
        Ok(Ok(o)) => {
            let err = String::from_utf8_lossy(&o.stderr);
            Err(format!("{name}: exit {}: {}", o.status.code().unwrap_or(-1), err.lines().last().unwrap_or("").chars().take(160).collect::<String>()))
        }
        Ok(Err(e)) => Err(format!("{name}: {e}")),
        Err(_) => Err(format!("{name}: took over {}s", limit.as_secs())),
    }
}

fn cache_dir() -> PathBuf {
    crate::config::amux_home().join("cache").join("file-previews")
}

fn cache_key(p: &Path, m: &std::fs::Metadata) -> String {
    use sha2::Digest as _;
    let mtime = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
    let mut h = sha2::Sha256::new();
    h.update(p.to_string_lossy().as_bytes());
    h.update(format!("|{mtime}|{}", m.len()).as_bytes());
    hex::encode(&h.finalize()[..16])
}

/// Turn `src` into a PNG at `out`, trying each tool this machine has. Returns
/// the tool that worked, or every attempt's error.
async fn convert(src: &Path, out: &Path, ext: &str) -> Result<&'static str, Vec<String>> {
    let mut tried = Vec::new();
    let os = |s: &str| std::ffi::OsString::from(s);
    let side = MAX_SIDE.to_string();
    if has_ext(IWORK_EXTS, ext) {
        // An iWork file is a zip that carries its own preview image.
        if let Some(unzip) = find_bin("unzip") {
            for inner in ["preview.jpg", "QuickLook/Thumbnail.jpg", "preview-web.jpg", "QuickLook/Preview.jpg"] {
                let o = tokio::process::Command::new(&unzip).arg("-p").arg(src).arg(inner)
                    .stderr(std::process::Stdio::null()).kill_on_drop(true).output();
                if let Ok(Ok(o)) = tokio::time::timeout(TOOL_TIMEOUT, o).await {
                    if o.status.success() && o.stdout.len() > 64 && std::fs::write(out, &o.stdout).is_ok() {
                        return Ok("unzip");
                    }
                }
            }
            tried.push("unzip: no preview image inside".into());
        }
    }
    let image = has_ext(CONVERT_EXTS, ext);
    let png = out.as_os_str();
    if image {
        if let Some(sips) = find_bin("sips").filter(|_| cfg!(target_os = "macos")) {
            match run(&sips, &[&os("-s"), &os("format"), &os("png"), &os("-Z"), &os(&side), src.as_os_str(), &os("--out"), png]).await {
                Ok(()) => return Ok("sips"),
                Err(e) => tried.push(e),
            }
        }
        if matches!(ext, ".heic" | ".heif") {
            if let Some(hc) = find_bin("heif-convert") {
                match run(&hc, &[src.as_os_str(), png]).await {
                    Ok(()) => return Ok("heif-convert"),
                    Err(e) => tried.push(e),
                }
            }
        }
        let first = format!("{}[0]", src.to_string_lossy());
        let fit = format!("{side}x{side}>");
        for (bin, name) in [("magick", "magick"), ("convert", "convert")] {
            if let Some(b) = find_bin(bin) {
                match run(&b, &[&os(&first), &os("-auto-orient"), &os("-resize"), &os(&fit), png]).await {
                    Ok(()) => return Ok(name),
                    Err(e) => tried.push(e),
                }
            }
        }
        if let Some(ff) = find_bin("ffmpeg") {
            let scale = format!("scale='min({side},iw)':-2");
            match run(&ff, &[&os("-y"), &os("-loglevel"), &os("error"), &os("-i"), src.as_os_str(), &os("-frames:v"), &os("1"), &os("-vf"), &os(&scale), png]).await {
                Ok(()) => return Ok("ffmpeg"),
                Err(e) => tried.push(e),
            }
        }
    }
    // Quick Look draws most things a Mac knows how to open.
    if quicklook_here() {
        let dir = out.with_extension("ql");
        match std::fs::create_dir_all(&dir) {
            Err(e) => tried.push(format!("qlmanage: {e}")),
            Ok(()) => {
                // qlmanage's -t is a thumbnail, not a tmux target: named here
                // so tests/tmux_target_audit.rs can tell.
                let r = finish(tokio::process::Command::new("/usr/bin/qlmanage").args(["-t", "-s", &side]).arg("-o").arg(&dir).arg(src),
                    "qlmanage", QUICKLOOK_TIMEOUT).await;
                let made = src.file_name().map(|n| dir.join(format!("{}.png", n.to_string_lossy())));
                let moved = made.as_ref().is_some_and(|m| m.is_file() && std::fs::rename(m, out).is_ok());
                clear(&dir);
                if moved {
                    return Ok("qlmanage");
                }
                tried.push(r.err().unwrap_or_else(|| "qlmanage: drew nothing".into()));
            }
        }
    }
    if tried.is_empty() {
        tried.push(if image { "no converter installed (sips, heif-convert, ImageMagick or ffmpeg)".into() } else { "no previewer for this kind of file".into() });
    }
    Err(tried)
}

/// `GET /api/file/preview-image?path=`: the file as a PNG.
pub(super) async fn image(req: Request) -> Response {
    let (p, m) = match target(&req) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let ext = ext_of(&p);
    let dir = cache_dir();
    let out = dir.join(format!("{}.png", cache_key(&p, &m)));
    let mut tool = "cache";
    if !out.is_file() {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return bad(500, &format!("could not make the preview cache: {e}"));
        }
        let tmp = dir.join(format!("{}.tmp.{}.png", cache_key(&p, &m), std::process::id()));
        let t0 = std::time::Instant::now();
        match convert(&p, &tmp, &ext).await {
            Ok(t) => {
                if std::fs::rename(&tmp, &out).is_err() {
                    clear(&tmp);
                    return bad(500, "could not store the preview");
                }
                tool = t;
                tracing::info!(ext = %ext, tool, ms = t0.elapsed().as_millis() as u64, measured = true, n_considered = 1,
                    verdict = "file_preview_converted", "a file the browser cannot show was converted for the viewer");
            }
            Err(tried) => {
                clear(&tmp);
                tracing::warn!(ext = %ext, tried = %tried.join("; "), measured = true, n_considered = tried.len(),
                    verdict = "file_preview_unconvertible", "no tool here could draw this file; the viewer shows its bytes");
                return j(415, json!({ "error": "no preview for this file on this machine", "tried": tried }));
            }
        }
    }
    match std::fs::read(&out) {
        Ok(bytes) => (
            StatusCode::OK,
            [("content-type", "image/png"), ("cache-control", "private, max-age=3600"), ("x-amux-preview-tool", tool)],
            Body::from(bytes),
        )
            .into_response(),
        Err(e) => bad(500, &format!("could not read the preview: {e}")),
    }
}

/// One line of `unzip -l`, GNU `tar -tv` or bsdtar `-tv`: (size, name).
fn parse_listing_line(line: &str, zip: bool) -> Option<(u64, String)> {
    let f: Vec<&str> = line.split_whitespace().collect();
    if zip {
        // "   1234  2026-11-08 10:00   dir/file.txt"
        if f.len() >= 4 && f[0].bytes().all(|b| b.is_ascii_digit()) && f[1].contains('-') && f[2].contains(':') {
            return Some((f[0].parse().ok()?, rest_after(line, 3)?));
        }
        return None;
    }
    // GNU:    "-rw-r--r-- user/group 1234 2026-11-08 10:00 dir/file"
    // bsdtar: "-rw-r--r--  0 user group 1234 Nov  8 10:00 dir/file"
    if f.len() >= 6 && f[1].contains('/') && f[2].bytes().all(|b| b.is_ascii_digit()) {
        let name = rest_after(line, 5)?;
        return Some((f[2].parse().ok()?, name));
    }
    if f.len() >= 9 && f[4].bytes().all(|b| b.is_ascii_digit()) {
        let name = rest_after(line, 8)?;
        return Some((f[4].parse().ok()?, name));
    }
    None
}

/// The text after the first `n` whitespace-separated fields, spaces kept.
fn rest_after(line: &str, n: usize) -> Option<String> {
    let mut s = line.trim_start();
    for _ in 0..n {
        let i = s.find(char::is_whitespace)?;
        s = s[i..].trim_start();
    }
    (!s.is_empty()).then(|| s.to_string())
}

/// `GET /api/file/archive?path=`: the entries inside an archive.
pub(super) async fn archive(req: Request) -> Response {
    let (p, _) = match target(&req) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let mut head = [0u8; 512];
    let n = {
        use std::io::Read;
        std::fs::File::open(&p).and_then(|mut f| f.read(&mut head)).unwrap_or(0)
    };
    let kind = magic_kind(&head[..n]);
    // A lone .gz (not a tar inside): its text, if it is text.
    if kind == Some("gzip data") && !p.to_string_lossy().contains(".tar") && ext_of(&p) == ".gz" {
        if let Some(text) = gunzip_head(&p) {
            return j(200, json!({ "text": text, "kind": "gzip data" }));
        }
    }
    let zip = kind == Some("ZIP archive");
    // bsdtar reads zip, 7z, rar and iso as well as tar; a Mac's tar is bsdtar.
    let tools: Vec<(PathBuf, Vec<&str>, bool)> = [
        find_bin("bsdtar").map(|b| (b, vec!["-tvf"], false)),
        (cfg!(target_os = "macos")).then(|| find_bin("tar")).flatten().map(|b| (b, vec!["-tvf"], false)),
        zip.then(|| find_bin("unzip")).flatten().map(|b| (b, vec!["-l"], true)),
        (!zip).then(|| find_bin("tar")).flatten().map(|b| (b, vec!["-tvf"], false)),
    ]
    .into_iter()
    .flatten()
    .collect();
    let mut tried = Vec::new();
    for (bin, args, is_zip) in tools {
        let name = bin.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let out = tokio::process::Command::new(&bin).args(&args).arg(&p).stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped()).kill_on_drop(true).output();
        match tokio::time::timeout(TOOL_TIMEOUT, out).await {
            Ok(Ok(o)) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                let mut entries: Vec<Value> = Vec::new();
                let mut total = 0usize;
                for line in text.lines() {
                    if let Some((size, name)) = parse_listing_line(line, is_zip) {
                        total += 1;
                        if entries.len() < ARCHIVE_MAX_ENTRIES {
                            entries.push(json!({ "name": name, "size": size, "dir": name.ends_with('/') }));
                        }
                    }
                }
                return j(200, json!({ "entries": entries, "total": total, "truncated": total > entries.len(), "tool": name, "kind": kind }));
            }
            Ok(Ok(o)) => tried.push(format!("{name}: {}", String::from_utf8_lossy(&o.stderr).lines().next().unwrap_or("failed"))),
            Ok(Err(e)) => tried.push(format!("{name}: {e}")),
            Err(_) => tried.push(format!("{name}: took over {}s", TOOL_TIMEOUT.as_secs())),
        }
    }
    tracing::warn!(path = %p.display(), tried = %tried.join("; "), measured = true, n_considered = tried.len(),
        verdict = "file_preview_archive_unread", "an archive could not be listed; the viewer shows its bytes");
    j(415, json!({ "error": "this archive could not be listed here", "tried": tried }))
}

fn gunzip_head(p: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(p).ok()?;
    let mut d = flate2::read::GzDecoder::new(f);
    let mut buf = vec![0u8; 256 * 1024];
    let mut got = 0;
    while got < buf.len() {
        match d.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => break,
        }
    }
    buf.truncate(got);
    if buf.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// `GET /api/file/sqlite?path=`: tables, their columns, row counts and first rows.
pub(super) async fn sqlite(req: Request) -> Response {
    let (p, _) = match target(&req) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let res = tokio::task::spawn_blocking(move || read_sqlite(&p)).await;
    match res {
        Ok(Ok(v)) => j(200, v),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, measured = true, n_considered = 1, verdict = "file_preview_sqlite_unread",
                "a database could not be read for the viewer");
            bad(415, &format!("this database could not be read: {e}"))
        }
        Err(e) => bad(500, &e.to_string()),
    }
}

fn read_sqlite(p: &Path) -> Result<Value, String> {
    use rusqlite::{types::ValueRef, OpenFlags};
    let conn = rusqlite::Connection::open_with_flags(p, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .map_err(|e| e.to_string())?;
    conn.busy_timeout(Duration::from_secs(2)).map_err(|e| e.to_string())?;
    let mut names: Vec<(String, String)> = {
        let mut st = conn
            .prepare("SELECT name, type FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%' ORDER BY type, name")
            .map_err(|e| e.to_string())?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
        rows.filter_map(Result::ok).collect()
    };
    let total = names.len();
    names.truncate(SQLITE_MAX_TABLES);
    let cell = |v: ValueRef| -> Value {
        match v {
            ValueRef::Null => Value::Null,
            ValueRef::Integer(i) => json!(i),
            ValueRef::Real(f) => json!(f),
            ValueRef::Text(t) => json!(String::from_utf8_lossy(&t[..t.len().min(500)])),
            ValueRef::Blob(b) => json!(format!("<{} bytes>", b.len())),
        }
    };
    let mut tables = Vec::new();
    for (name, ty) in names {
        let q = format!("\"{}\"", name.replace('"', "\"\""));
        let count: Option<i64> = conn.query_row(&format!("SELECT count(*) FROM {q}"), [], |r| r.get(0)).ok();
        let mut st = match conn.prepare(&format!("SELECT * FROM {q} LIMIT {SQLITE_SAMPLE_ROWS}")) {
            Ok(s) => s,
            Err(e) => {
                tables.push(json!({ "name": name, "type": ty, "error": e.to_string() }));
                continue;
            }
        };
        let cols: Vec<String> = st.column_names().map(|c| c.to_string()).collect();
        let n = cols.len();
        let rows: Vec<Value> = match st.query([]) {
            Ok(mut rs) => {
                let mut out = Vec::new();
                while let Ok(Some(r)) = rs.next() {
                    out.push(Value::Array((0..n).map(|i| r.get_ref(i).map(cell).unwrap_or(Value::Null)).collect()));
                }
                out
            }
            Err(_) => Vec::new(),
        };
        tables.push(json!({ "name": name, "type": ty, "columns": cols, "count": count, "rows": rows }));
    }
    Ok(json!({ "tables": tables, "total_tables": total }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_name_their_kind() {
        assert_eq!(magic_kind(b"II*\0abc"), Some("TIFF image"));
        assert_eq!(magic_kind(b"\0\0\0\x18ftypheic...."), Some("HEIF image"));
        assert_eq!(magic_kind(b"SQLite format 3\0"), Some("SQLite database"));
        assert_eq!(magic_kind(b"PK\x03\x04"), Some("ZIP archive"));
        assert_eq!(magic_kind(b"wOF2"), Some("WOFF2 font"));
        assert_eq!(magic_kind(b"hello"), None);
    }

    #[test]
    fn every_binary_gets_a_previewer() {
        assert_eq!(preview_for(".tiff", None), "image");
        assert_eq!(preview_for(".HEIC", None), "image");
        assert_eq!(preview_for(".bin", Some("TIFF image")), "image", "the bytes win over a wrong name");
        assert_eq!(preview_for(".key", None), "image", "iWork carries its own preview");
        assert_eq!(preview_for(".db", Some("SQLite database")), "sqlite");
        assert_eq!(preview_for(".dat", Some("SQLite database")), "sqlite");
        assert_eq!(preview_for(".woff2", None), "font");
        assert_eq!(preview_for(".tgz", None), "archive");
        assert_eq!(preview_for(".whatever", Some("ZIP archive")), "archive");
        let other = preview_for(".bin", None);
        assert!(other == "hex" || other == "quicklook", "{other}");
        assert_eq!(preview_for(".bin", Some("ELF executable")), "hex", "a known kind shows its bytes");
    }

    #[test]
    fn listings_from_each_tool_parse_with_spaces_in_names() {
        assert_eq!(parse_listing_line("     1234  2026-11-08 10:00   dir/my file.txt", true), Some((1234, "dir/my file.txt".into())));
        assert_eq!(parse_listing_line("  Length      Date    Time    Name", true), None);
        assert_eq!(parse_listing_line("-rw-r--r-- me/staff     42 2026-11-08 10:00 a b/c.txt", false), Some((42, "a b/c.txt".into())));
        assert_eq!(parse_listing_line("-rw-r--r--  0 me     staff      42 Nov  8 10:00 a b/c.txt", false), Some((42, "a b/c.txt".into())));
    }

    #[test]
    fn a_database_reads_read_only_with_its_first_rows() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("t.sqlite");
        let c = rusqlite::Connection::open(&p).unwrap();
        c.execute_batch("CREATE TABLE people(name TEXT, n INTEGER, pic BLOB); INSERT INTO people VALUES ('ada', 1, x'0102');").unwrap();
        drop(c);
        let v = read_sqlite(&p).unwrap();
        assert_eq!(v["tables"][0]["name"], "people");
        assert_eq!(v["tables"][0]["count"], 1);
        assert_eq!(v["tables"][0]["rows"][0], json!(["ada", 1, "<2 bytes>"]));
    }
}
