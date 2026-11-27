//! `GET /api/sessions/{name}/files-written`: the files one worker created or
//! edited, for the worker's Files tab toggle (Ethan, 2026-11-27: "show
//! exclusively the files that were created and modified by the particular
//! worker").
//!
//! The source is the worker's own Claude transcripts. Every conversation the
//! token ledger attributes to the worker is read for its Edit, Write,
//! MultiEdit and NotebookEdit calls, and a Write whose result is
//! `"type":"create"` marks the file as created. A Bash write (`sed -i`, a
//! heredoc) is not counted: only the mtime could tie it to this worker, and on
//! a shared checkout that names a peer's write as often as this one's
//! (git_guard's INFERRED provenance, AMUX-2456).
//!
//! Transcripts are append-only, so each is read once and then only from the
//! byte offset already indexed. The index lives in
//! `~/.amux/worker-files/<worker>.json`. A worker with a thousand past
//! conversations is indexed across several requests under a time budget, and
//! the reply says so (`complete: false`) rather than returning a short list as
//! if it were the whole one.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::session_verbs::{load_meta, meta_str, parse_env, project_name, session_jsonl_path};
use super::AppState;

const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
/// Wall-clock budget for one request's indexing; the rest waits for the next.
const SCAN_BUDGET: Duration = Duration::from_secs(4);
/// A conversation with no transcript anywhere is looked for again after this.
const MISS_RETRY_S: i64 = 6 * 3600;
const MAX_FILES: usize = 5000;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct FileRec {
    pub created: bool,
    pub first_ts: i64,
    pub last_ts: i64,
    pub writes: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct TranscriptRec {
    path: String,
    offset: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Index {
    /// conversation id -> where its transcript is and how far it is read.
    transcripts: BTreeMap<String, TranscriptRec>,
    /// conversation id -> epoch seconds of the last failed search.
    missing: BTreeMap<String, i64>,
    files: BTreeMap<String, FileRec>,
}

fn index_path(home: &Path, name: &str) -> PathBuf {
    home.join("worker-files").join(format!("{name}.json"))
}

fn load_index(home: &Path, name: &str) -> Index {
    std::fs::read(index_path(home, name))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_index(home: &Path, name: &str, idx: &Index) {
    let path = index_path(home, name);
    let tmp = path.with_extension("json.tmp");
    let saved = std::fs::create_dir_all(path.parent().unwrap_or(home))
        .and_then(|()| serde_json::to_vec(idx).map_err(std::io::Error::other))
        .and_then(|b| std::fs::write(&tmp, b))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(error) = saved {
        tracing::warn!(session = %name, %error, measured = true, n_considered = idx.files.len(),
            verdict = "worker_files_index_not_saved",
            "the worker's written-files index could not be saved; the next request reads its transcripts again");
    }
}

fn parse_ts(v: &Value) -> Option<i64> {
    let raw = v.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw).ok().map(|d| d.timestamp())
}

fn touch(files: &mut BTreeMap<String, FileRec>, path: &str, ts: i64, created: bool) {
    if !path.starts_with('/') {
        return;
    }
    let rec = files.entry(path.to_string()).or_insert_with(|| FileRec { first_ts: ts, last_ts: ts, ..Default::default() });
    rec.first_ts = rec.first_ts.min(ts);
    rec.last_ts = rec.last_ts.max(ts);
    rec.created |= created;
}

/// Fold one transcript line into `files`. Pure, so the parsing is testable.
pub(crate) fn fold_line(files: &mut BTreeMap<String, FileRec>, line: &str) {
    // Cheap byte checks first: most lines are prose or tool output.
    let edit = line.contains("\"tool_use\"") && (line.contains("\"file_path\"") || line.contains("\"notebook_path\""));
    let create = line.contains("\"toolUseResult\"") && line.contains("\"create\"");
    if !edit && !create {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else { return };
    let Some(ts) = parse_ts(&v) else { return };
    if create {
        let r = &v["toolUseResult"];
        if r["type"] == "create" {
            if let Some(p) = r["filePath"].as_str() {
                touch(files, p, ts, true);
            }
        }
    }
    if edit {
        for blk in v["message"]["content"].as_array().into_iter().flatten() {
            if blk["type"] != "tool_use" || !EDIT_TOOLS.contains(&blk["name"].as_str().unwrap_or("")) {
                continue;
            }
            let inp = &blk["input"];
            if let Some(p) = inp["file_path"].as_str().or_else(|| inp["notebook_path"].as_str()) {
                touch(files, p, ts, false);
                if let Some(rec) = files.get_mut(p) {
                    rec.writes += 1;
                }
            }
        }
    }
}

/// Read `path` from `offset` to its last complete line; returns the new offset.
fn read_from(path: &Path, offset: u64, files: &mut BTreeMap<String, FileRec>) -> std::io::Result<u64> {
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    // A shorter file was rewritten, not appended to: read it from the start.
    let start = if len < offset { 0 } else { offset };
    f.seek(SeekFrom::Start(start))?;
    let mut reader = std::io::BufReader::new(f.take(len - start));
    let mut pos = start;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() != Some(&b'\n') {
            break; // EOF, or a line still being written
        }
        pos += n as u64;
        fold_line(files, &String::from_utf8_lossy(&buf));
    }
    Ok(pos)
}

/// Where a conversation's transcript is: the worker's own project dirs first,
/// then any project (the walk `find_conversation_jsonl` caches).
fn locate(chome: &Path, bases: &[String], conv: &str) -> Option<PathBuf> {
    for base in bases {
        let cand = chome.join("projects").join(project_name(base)).join(format!("{conv}.jsonl"));
        if cand.is_file() {
            return Some(cand);
        }
    }
    super::session_verbs::find_conversation_jsonl(&chome.join("projects"), conv)
}

pub(crate) struct ScanResult {
    files: BTreeMap<String, FileRec>,
    known: usize,
    indexed: usize,
    missing: usize,
    complete: bool,
}

fn scan(home: &Path, name: &str, conversations: BTreeSet<String>) -> ScanResult {
    let mut idx = load_index(home, name);
    let cfg = parse_env(name);
    let meta = load_meta(name);
    let bases: Vec<String> = [meta_str(&meta, "cc_cwd"), cfg.get_or("CC_DIR", "").to_string()]
        .into_iter()
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .collect();
    let chome = super::claude_config::for_session(name);
    let mut convs = conversations;
    let current = session_jsonl_path(name);
    if let Some(stem) = current.as_ref().and_then(|p| p.file_stem()).and_then(|s| s.to_str()) {
        convs.insert(stem.to_string());
    }
    let started = Instant::now();
    let now = chrono::Utc::now().timestamp();
    let mut complete = true;
    for conv in &convs {
        if started.elapsed() > SCAN_BUDGET {
            complete = false;
            break;
        }
        let rec = match idx.transcripts.get(conv) {
            Some(r) if Path::new(&r.path).is_file() => r.clone(),
            _ => {
                if idx.missing.get(conv).is_some_and(|at| now - at < MISS_RETRY_S) {
                    continue;
                }
                let found = if current.as_ref().and_then(|p| p.file_stem()).and_then(|s| s.to_str()) == Some(conv.as_str()) {
                    current.clone()
                } else {
                    locate(&chome, &bases, conv)
                };
                match found {
                    Some(p) => {
                        idx.missing.remove(conv);
                        TranscriptRec { path: p.to_string_lossy().into_owned(), offset: 0 }
                    }
                    None => {
                        idx.missing.insert(conv.clone(), now);
                        continue;
                    }
                }
            }
        };
        let len = std::fs::metadata(&rec.path).map(|m| m.len()).unwrap_or(0);
        if len == rec.offset {
            idx.transcripts.insert(conv.clone(), rec);
            continue;
        }
        match read_from(Path::new(&rec.path), rec.offset, &mut idx.files) {
            Ok(offset) => {
                idx.transcripts.insert(conv.clone(), TranscriptRec { offset, ..rec });
            }
            Err(error) => {
                tracing::warn!(session = %name, conv = %conv, %error, measured = true, n_considered = 1,
                    verdict = "worker_files_transcript_unreadable", "a worker transcript could not be read");
            }
        }
    }
    let missing = convs.iter().filter(|c| idx.missing.contains_key(*c)).count();
    let indexed = convs.iter().filter(|c| idx.transcripts.contains_key(*c)).count();
    save_index(home, name, &idx);
    ScanResult { files: idx.files, known: convs.len(), indexed, missing, complete }
}

pub async fn files_written(State(state): State<AppState>, AxPath(name): AxPath<String>) -> Response {
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad worker name"}))).into_response();
    }
    let who = name.clone();
    let convs = state
        .store
        .read_async(move |c| {
            let mut st = c.prepare("SELECT DISTINCT conversation FROM token_ledger WHERE session=?1")?;
            let rows = st.query_map([&who], |r| r.get::<_, String>(0))?.flatten().collect::<BTreeSet<_>>();
            Ok(rows)
        })
        .await;
    let convs = match convs {
        Ok(c) => c,
        Err(e) => {
            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"session": name, "measured": false,
                "n_considered": 0, "why_unmeasured": e.to_string()}))).into_response()
        }
    };
    let home = super::settings::amux_home();
    let who = name.clone();
    let Ok(res) = tokio::task::spawn_blocking(move || scan(&home, &who, convs)).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"session": name, "measured": false,
            "n_considered": 0, "why_unmeasured": "scan task failed"}))).into_response();
    };
    let provider = parse_env(&name).get_or("CC_PROVIDER", "claude").to_string();
    let mut rows: Vec<(&String, &FileRec)> = res.files.iter().collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.1.last_ts));
    let truncated = rows.len() > MAX_FILES;
    let files: Vec<Value> = rows
        .into_iter()
        .take(MAX_FILES)
        .map(|(path, r)| {
            let md = std::fs::metadata(path).ok();
            let modified = md.as_ref().and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
            json!({"path": path, "created": r.created, "first_ts": r.first_ts, "last_ts": r.last_ts,
                "writes": r.writes, "exists": md.as_ref().is_some_and(|m| m.is_file()),
                "size": md.as_ref().map(|m| m.len()), "modified": modified})
        })
        .collect();
    if !res.complete {
        tracing::info!(session = %name, known = res.known, indexed = res.indexed, measured = true,
            n_considered = res.known, verdict = "worker_files_index_partial",
            "the worker's transcripts did not all fit in one request's budget; the rest index on the next");
    }
    Json(json!({
        "session": name,
        "measured": true,
        "n_considered": res.files.len(),
        "complete": res.complete,
        "truncated": truncated,
        "transcripts": {"known": res.known, "indexed": res.indexed, "missing": res.missing},
        "provider": provider,
        "source": "Edit, Write, MultiEdit and NotebookEdit calls in the worker's Claude transcripts; shell writes are not counted",
        "files": files,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_that_created_a_file_and_a_later_edit_fold_into_one_created_row() {
        let mut files = BTreeMap::new();
        fold_line(&mut files, r#"{"timestamp":"2026-11-27T10:00:00Z","message":{"content":[{"type":"tool_use","name":"Write","input":{"file_path":"/w/a.md","content":"x"}}]}}"#);
        fold_line(&mut files, r#"{"timestamp":"2026-11-27T10:00:01Z","toolUseResult":{"type":"create","filePath":"/w/a.md"}}"#);
        fold_line(&mut files, r#"{"timestamp":"2026-11-27T11:00:00Z","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/w/a.md","old_string":"x","new_string":"y"}}]}}"#);
        fold_line(&mut files, r#"{"timestamp":"2026-11-27T12:00:00Z","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/w/b.rs"}}]}}"#);
        let a = &files["/w/a.md"];
        assert!(a.created);
        assert_eq!(a.writes, 2);
        assert_eq!(a.last_ts - a.first_ts, 3600);
        assert!(!files["/w/b.rs"].created, "an Edit alone is a modification");
    }

    #[test]
    fn reads_and_shell_writes_are_not_counted() {
        let mut files = BTreeMap::new();
        fold_line(&mut files, r#"{"timestamp":"2026-11-27T10:00:00Z","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/w/read.md"}}]}}"#);
        fold_line(&mut files, r#"{"timestamp":"2026-11-27T10:00:00Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo > /w/x","file_path":"/w/x"}}]}}"#);
        fold_line(&mut files, r#"{"timestamp":"2026-11-27T10:00:00Z","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"relative.md"}}]}}"#);
        assert!(files.is_empty(), "{files:?}");
    }

    #[test]
    fn a_transcript_is_read_once_and_then_only_from_its_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c.jsonl");
        let line = |f: &str| format!("{{\"timestamp\":\"2026-11-27T10:00:00Z\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"name\":\"Edit\",\"input\":{{\"file_path\":\"{f}\"}}}}]}}}}\n");
        std::fs::write(&p, line("/w/1")).unwrap();
        let mut files = BTreeMap::new();
        let off = read_from(&p, 0, &mut files).unwrap();
        // A half-written line is left for the next read.
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        std::io::Write::write_all(&mut f, line("/w/2").trim_end().as_bytes()).unwrap();
        let off2 = read_from(&p, off, &mut files).unwrap();
        assert_eq!(off2, off);
        std::io::Write::write_all(&mut f, b"\n").unwrap();
        let off3 = read_from(&p, off2, &mut files).unwrap();
        assert!(off3 > off2);
        assert_eq!(files["/w/1"].writes, 1, "the first line was not read twice");
        assert!(files.contains_key("/w/2"));
    }
}
