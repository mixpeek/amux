//! `GET`/`PUT /api/files/mdai/doc`: a `.mdai` file as structured fields, so the
//! dashboard can show and edit every part of a node without a YAML editor.
//!
//! WHY THE SERVER OWNS THIS. The dashboard used to parse and rewrite frontmatter
//! with its own line parser. Anything it did not recognise (a `fetch:` block, a
//! key added later) made the whole form read-only, and a node could only be
//! changed by hand-editing YAML. The engine already parses these files with
//! serde_yaml; this endpoint uses the same parser to READ, and writes back in
//! one canonical layout that the engine is guaranteed to read the same way.
//! Keys this module does not know are carried through untouched (`extra`), so
//! an older dashboard cannot delete a newer file's settings by saving it.
//!
//! The format stays the one existing files already use: `model`, `sources`
//! (`path` + `prompt`), `fetch`, and the markdown body. Three optional keys are
//! added, all ignored by an engine that predates them:
//!
//! - `title`: a display title (the list falls back to the body's first line);
//! - `refresh`: `open` runs the node when its page is opened (the behaviour a
//!   file without the key keeps), `manual` runs it only when asked;
//! - `timeout`: this node's model-call budget in seconds (30-3600).

use super::{
    canon_root, check_fetch_url, is_mdai, mdai_root, parse_mdai, rel_key, split_frontmatter,
    MDAI_EXT,
};
use crate::api::AppState;
use crate::db::WriteOutcome;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// Keys this module reads and writes itself; everything else is `extra`.
const KNOWN: &[&str] = &["title", "model", "refresh", "timeout", "fetch", "sources"];
pub(crate) const MIN_TIMEOUT_S: u64 = 30;
pub(crate) const MAX_TIMEOUT_S: u64 = 3600;

#[derive(Debug)]
pub(crate) struct FieldError {
    pub status: StatusCode,
    pub field: &'static str,
    pub msg: String,
}

impl FieldError {
    fn bad(field: &'static str, msg: impl Into<String>) -> Self {
        FieldError { status: StatusCode::BAD_REQUEST, field, msg: msg.into() }
    }
    fn conflict(field: &'static str, msg: impl Into<String>) -> Self {
        FieldError { status: StatusCode::CONFLICT, field, msg: msg.into() }
    }
}

impl IntoResponse for FieldError {
    fn into_response(self) -> Response {
        if self.status.is_server_error() {
            tracing::warn!(target: "amux::mdai", field = self.field, error = %self.msg,
                verdict = "mdai_doc_write_failed", "mdai doc request failed");
        }
        (self.status, Json(json!({"error": self.msg, "field": self.field}))).into_response()
    }
}

fn yaml_to_json(v: &serde_yaml::Value) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn json_to_yaml(v: &Value) -> serde_yaml::Value {
    serde_yaml::to_value(v).unwrap_or(serde_yaml::Value::Null)
}

fn ystr(s: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(s.to_string())
}

/// Read a file into the structured view. A frontmatter the YAML parser rejects
/// still answers, with `parse_error` set and `doc` null, so the page can offer
/// the raw text for repair instead of an error screen.
pub(crate) fn read_doc(root_canon: &Path, file: &Path) -> Result<Value, FieldError> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| FieldError::bad("path", format!("{}: {e}", file.display())))?;
    let mtime_ms = mtime_ms(file);
    let rel = rel_key(root_canon, file);
    let (doc, parse_error) = match parse_view(&text) {
        Ok(d) => (d, Value::Null),
        Err(e) => (Value::Null, json!(e)),
    };
    Ok(json!({
        "path": rel,
        "abs": file.to_string_lossy(),
        "name": file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "mtime_ms": mtime_ms,
        "doc": doc,
        "parse_error": parse_error,
        "read_only": Value::Null,
        "raw": text,
    }))
}

fn mtime_ms(file: &Path) -> Option<i64> {
    std::fs::metadata(file)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
}

/// The structured view of a document's text.
pub(crate) fn parse_view(text: &str) -> Result<Value, String> {
    // The engine's parser is the authority on whether this file runs at all.
    parse_mdai(text).map_err(|e| e.to_string())?;
    let (yaml, mut body) = split_frontmatter(text);
    // split_frontmatter joins lines and so drops the file's final newline; keep
    // it, or a body typed with a trailing newline reads back without one.
    if text.ends_with('\n') && !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    let map: serde_yaml::Mapping = match yaml {
        Some(y) if !y.trim().is_empty() => serde_yaml::from_str::<serde_yaml::Value>(&y)
            .map_err(|e| e.to_string())?
            .as_mapping()
            .cloned()
            .ok_or_else(|| "frontmatter is not a mapping".to_string())?,
        _ => serde_yaml::Mapping::new(),
    };
    let get = |k: &str| map.get(ystr(k));
    let s = |k: &str| get(k).and_then(|v| v.as_str()).map(|v| v.to_string());
    let sources: Vec<Value> = match get("sources") {
        Some(serde_yaml::Value::Sequence(seq)) => seq
            .iter()
            .map(|e| match e {
                serde_yaml::Value::String(p) => json!({"path": p, "prompt": ""}),
                serde_yaml::Value::Mapping(m) => json!({
                    "path": m.get(ystr("path")).and_then(|v| v.as_str()).unwrap_or(""),
                    "prompt": m.get(ystr("prompt")).and_then(|v| v.as_str()).unwrap_or(""),
                }),
                _ => json!({"path": "", "prompt": ""}),
            })
            .collect(),
        _ => Vec::new(),
    };
    let fetch = match get("fetch") {
        Some(serde_yaml::Value::String(u)) => json!({"url": u, "method": "GET", "headers": {},
            "body": "", "connector": "", "select": ""}),
        Some(serde_yaml::Value::Mapping(m)) => {
            let f = |k: &str| m.get(ystr(k)).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let headers: Map<String, Value> = match m.get(ystr("headers")) {
                Some(serde_yaml::Value::Mapping(h)) => h
                    .iter()
                    .filter_map(|(k, v)| Some((k.as_str()?.to_string(), json!(v.as_str()?))))
                    .collect(),
                _ => Map::new(),
            };
            json!({"url": f("url"), "method": if f("method").is_empty() { "GET".into() } else { f("method").to_ascii_uppercase() },
                "headers": headers, "body": f("body"), "connector": f("connector"), "select": f("select")})
        }
        _ => Value::Null,
    };
    let timeout = get("timeout").and_then(|v| v.as_u64());
    let mut extra = Map::new();
    for (k, v) in &map {
        if let Some(k) = k.as_str() {
            if !KNOWN.contains(&k) {
                extra.insert(k.to_string(), yaml_to_json(v));
            }
        }
    }
    Ok(json!({
        "title": s("title"),
        "model": s("model").filter(|m| !m.trim().is_empty()),
        "refresh": s("refresh"),
        "timeout_s": timeout,
        "sources": sources,
        "fetch": fetch,
        "body": body,
        "extra": extra,
    }))
}

fn opt_str(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Validate a structured doc and render it as `.mdai` text. Every rule names the
/// field it is about, so the form can put the message beside the control.
pub(crate) fn render_doc(doc: &Value) -> Result<String, FieldError> {
    let mut map = serde_yaml::Mapping::new();
    if let Some(t) = opt_str(doc, "title") {
        if t.contains('\n') || t.chars().count() > 200 {
            return Err(FieldError::bad("title", "A title is one line of at most 200 characters."));
        }
        map.insert(ystr("title"), ystr(&t));
    }
    if let Some(m) = opt_str(doc, "model") {
        let ok = m.len() <= 100
            && m.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/[]@".contains(c));
        if !ok {
            return Err(FieldError::bad("model", format!("'{m}' is not a model name (letters, digits and - _ . : / [ ] @).")));
        }
        map.insert(ystr("model"), ystr(&m));
    }
    match opt_str(doc, "refresh").as_deref() {
        None => {}
        Some(r @ ("open" | "manual")) => {
            map.insert(ystr("refresh"), ystr(r));
        }
        Some(other) => {
            return Err(FieldError::bad("refresh", format!("refresh is 'open' or 'manual', not '{other}'.")))
        }
    }
    match doc.get("timeout_s") {
        None | Some(Value::Null) => {}
        Some(v) => {
            let n = v
                .as_u64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                .ok_or_else(|| FieldError::bad("timeout_s", "Timeout is a whole number of seconds."))?;
            if !(MIN_TIMEOUT_S..=MAX_TIMEOUT_S).contains(&n) {
                return Err(FieldError::bad("timeout_s",
                    format!("Timeout must be between {MIN_TIMEOUT_S} and {MAX_TIMEOUT_S} seconds.")));
            }
            map.insert(ystr("timeout"), serde_yaml::Value::Number(n.into()));
        }
    }
    let has_fetch = match doc.get("fetch") {
        None | Some(Value::Null) => false,
        Some(f) => {
            let url = opt_str(f, "url").unwrap_or_default();
            if url.is_empty() {
                false
            } else {
                check_fetch_url(&url).map_err(|e| FieldError::bad("fetch.url", e.to_string()))?;
                let method = opt_str(f, "method").unwrap_or_else(|| "GET".into()).to_ascii_uppercase();
                if !["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"].contains(&method.as_str()) {
                    return Err(FieldError::bad("fetch.method", format!("Unsupported method '{method}'.")));
                }
                let mut fm = serde_yaml::Mapping::new();
                fm.insert(ystr("url"), ystr(&url));
                if method != "GET" {
                    fm.insert(ystr("method"), ystr(&method));
                }
                if let Some(Value::Object(h)) = f.get("headers") {
                    let mut hm = serde_yaml::Mapping::new();
                    for (k, v) in h {
                        let k = k.trim();
                        if k.is_empty() {
                            continue;
                        }
                        if k.eq_ignore_ascii_case("authorization") {
                            return Err(FieldError::bad("fetch.headers",
                                "Credentials do not go in the file. Name a connector instead; its key stays in server.env."));
                        }
                        hm.insert(ystr(k), ystr(v.as_str().unwrap_or("")));
                    }
                    if !hm.is_empty() {
                        fm.insert(ystr("headers"), serde_yaml::Value::Mapping(hm));
                    }
                }
                for k in ["body", "connector", "select"] {
                    if let Some(v) = f.get(k).and_then(|x| x.as_str()).filter(|s| !s.trim().is_empty()) {
                        fm.insert(ystr(k), ystr(v));
                    }
                }
                map.insert(ystr("fetch"), serde_yaml::Value::Mapping(fm));
                true
            }
        }
    };
    let mut seq = Vec::new();
    if let Some(Value::Array(srcs)) = doc.get("sources") {
        for (i, s) in srcs.iter().enumerate() {
            let path = opt_str(s, "path").unwrap_or_default();
            if path.is_empty() {
                return Err(FieldError::bad("sources", format!("Source {} has no path.", i + 1)));
            }
            if let Some(spec) = path.strip_prefix("amux:") {
                let kind = spec.split('?').next().unwrap_or("").trim();
                if kind != "messages" {
                    return Err(FieldError::bad("sources", format!(
                        "Source {}: unknown amux source '{kind}' (supported: messages).", i + 1)));
                }
            }
            let prompt = s.get("prompt").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
            if prompt.chars().count() > 8000 {
                return Err(FieldError::bad("sources", format!("Source {}: the edge prompt is over 8000 characters.", i + 1)));
            }
            let mut m = serde_yaml::Mapping::new();
            m.insert(ystr("path"), ystr(&path));
            m.insert(ystr("prompt"), ystr(&prompt));
            seq.push(serde_yaml::Value::Mapping(m));
        }
    }
    map.insert(ystr("sources"), serde_yaml::Value::Sequence(seq));
    if let Some(Value::Object(extra)) = doc.get("extra") {
        for (k, v) in extra {
            if !KNOWN.contains(&k.as_str()) && !k.trim().is_empty() {
                map.insert(ystr(k), json_to_yaml(v));
            }
        }
    }
    let body = doc.get("body").and_then(|b| b.as_str()).unwrap_or("").to_string();
    if body.trim().is_empty() && !has_fetch {
        return Err(FieldError::bad("body",
            "Write the instruction: what should the model produce from the sources?"));
    }
    let front = serde_yaml::to_string(&serde_yaml::Value::Mapping(map))
        .map_err(|e| FieldError::bad("doc", e.to_string()))?;
    let mut out = String::from("---\n");
    out.push_str(&front);
    if !front.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(&body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    // The engine must read back exactly what was written.
    parse_mdai(&out).map_err(|e| FieldError::bad("doc", format!("would not parse: {e}")))?;
    Ok(out)
}

/// Resolve a path for writing, inside the root, without requiring it to exist.
fn resolve_for_write(root_canon: &Path, raw: &str) -> Result<PathBuf, FieldError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(FieldError::bad("path", "path required"));
    }
    let p = Path::new(raw);
    if p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(FieldError::bad("path", "A path may not contain '..'."));
    }
    let joined = if p.is_absolute() { p.to_path_buf() } else { root_canon.join(p) };
    // Canonicalize what exists (the file, or its nearest existing ancestor), so
    // a symlink cannot carry the write outside the root.
    let mut probe = joined.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(c) = probe.canonicalize() {
            let mut full = c;
            for t in tail.iter().rev() {
                full.push(t);
            }
            if !full.starts_with(root_canon) {
                return Err(FieldError::bad("path", format!("{raw} is outside the MDAI folder {}.", root_canon.display())));
            }
            if !is_mdai(&full) {
                return Err(FieldError::bad("path", format!("A node's file name ends in .{MDAI_EXT}.")));
            }
            return Ok(full);
        }
        match (probe.file_name().map(|n| n.to_os_string()), probe.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                probe = parent.to_path_buf();
            }
            _ => return Err(FieldError::bad("path", format!("{raw} is not a usable path."))),
        }
    }
}

fn write_atomic(file: &Path, text: &str) -> Result<(), FieldError> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| FieldError {
            status: StatusCode::INTERNAL_SERVER_ERROR, field: "path", msg: format!("{}: {e}", dir.display()) })?;
    }
    let tmp = file.with_extension(format!("{MDAI_EXT}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, file)).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        FieldError { status: StatusCode::INTERNAL_SERVER_ERROR, field: "path", msg: format!("{}: {e}", file.display()) }
    })
}

/// `GET /api/files/mdai/doc?path=`
pub(crate) async fn get_doc(Query(q): Query<HashMap<String, String>>) -> Response {
    let path = q.get("path").cloned().unwrap_or_default();
    let res = crate::db::interactions::spawn_blocking(move || -> Result<Value, FieldError> {
        let root_canon = canon_root(&mdai_root()).map_err(|e| FieldError::bad("path", e.to_string()))?;
        let file = match super::resolve_existing(&root_canon, &root_canon, &path) {
            Ok(f) => f,
            // A .mdai file outside the MDAI folder can still be READ (the file
            // viewer already reads it), and is answered read-only: the engine
            // will not run it and this endpoint will not write it, and the
            // page says so instead of failing to open it.
            Err(super::MdaiError::Escape(_)) if Path::new(&path).is_absolute() && is_mdai(Path::new(&path)) => {
                let f = Path::new(&path).canonicalize().map_err(|e| FieldError::bad("path", e.to_string()))?;
                let mut v = read_doc(&root_canon, &f)?;
                v["read_only"] = json!(format!(
                    "This file is outside the MDAI folder ({}), so it can be read here but not saved or run.",
                    root_canon.display()));
                return Ok(v);
            }
            Err(e) => return Err(FieldError { status: StatusCode::NOT_FOUND, field: "path", msg: e.to_string() }),
        };
        if !is_mdai(&file) {
            return Err(FieldError::bad("path", format!("not a .{MDAI_EXT} file")));
        }
        read_doc(&root_canon, &file)
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// `PUT /api/files/mdai/doc {path, doc | raw, create?, base_mtime_ms?, rename_to?}`
///
/// - `doc` is the structured view (as GET returns it); `raw` is the whole file
///   text, accepted only if the engine parses it.
/// - `create: true` refuses to overwrite: an existing file is somebody's node.
/// - `base_mtime_ms` refuses a save over a file that changed since it was read.
/// - `rename_to` renames the file in its folder and moves its run history.
pub(crate) async fn put_doc(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let store = state.store.clone();
    let res = crate::db::interactions::spawn_blocking(move || -> Result<Value, FieldError> {
        let root_canon = canon_root(&mdai_root()).map_err(|e| FieldError::bad("path", e.to_string()))?;
        let raw_path = body.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let file = resolve_for_write(&root_canon, &raw_path)?;
        let create = body.get("create").and_then(|v| v.as_bool()).unwrap_or(false);
        let exists = file.exists();
        if create && exists {
            return Err(FieldError::conflict("name", format!(
                "{} already exists in that folder. Pick another name.",
                file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default())));
        }
        if !create && !exists {
            return Err(FieldError { status: StatusCode::NOT_FOUND, field: "path",
                msg: format!("{raw_path} does not exist (it may have been renamed or deleted).") });
        }
        if let (false, Some(base)) = (create, body.get("base_mtime_ms").and_then(|v| v.as_i64())) {
            if let Some(cur) = mtime_ms(&file) {
                if cur != base {
                    return Err(FieldError::conflict("doc",
                        "This node changed on disk since you opened it. Reload to see the current version, then apply your edit again."));
                }
            }
        }
        let text = match (body.get("raw").and_then(|v| v.as_str()), body.get("doc")) {
            (Some(raw), _) => {
                parse_mdai(raw).map_err(|e| FieldError::bad("raw", e.to_string()))?;
                raw.to_string()
            }
            (None, Some(doc)) if doc.is_object() => {
                // Unknown keys survive a save even if the client never saw them.
                let mut doc = doc.clone();
                if !create && doc.get("extra").is_none() {
                    if let Ok(prev) = std::fs::read_to_string(&file).map_err(|_| ()).and_then(|t| parse_view(&t).map_err(|_| ())) {
                        doc["extra"] = prev["extra"].clone();
                    }
                }
                render_doc(&doc)?
            }
            _ => return Err(FieldError::bad("doc", "send `doc` (fields) or `raw` (file text)")),
        };
        write_atomic(&file, &text)?;
        let mut target = file.clone();
        if let Some(new_name) = body.get("rename_to").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) {
            target = rename(&store, &root_canon, &file, new_name)?;
        }
        tracing::info!(target: "amux::mdai", path = %rel_key(&root_canon, &target), create,
            bytes = text.len(), verdict = "mdai_doc_saved", measured = true, n_considered = 1,
            "mdai node saved from structured fields");
        read_doc(&root_canon, &target)
    })
    .await;
    match res {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => e.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))).into_response(),
    }
}

/// Rename a node inside its folder and carry its run history to the new name,
/// so the Runs tab does not reset to zero because the file was renamed.
fn rename(store: &crate::db::Store, root_canon: &Path, file: &Path, new_name: &str) -> Result<PathBuf, FieldError> {
    let mut name = new_name.to_string();
    if !name.to_ascii_lowercase().ends_with(&format!(".{MDAI_EXT}")) {
        name.push_str(&format!(".{MDAI_EXT}"));
    }
    if name.contains('/') || name.contains('\0') || name.starts_with('.') {
        return Err(FieldError::bad("name", "A name cannot contain '/' or start with '.'."));
    }
    let target = file.with_file_name(&name);
    if target == file {
        return Ok(target);
    }
    if target.exists() {
        return Err(FieldError::conflict("name", format!("{name} already exists in that folder.")));
    }
    let old_key = rel_key(root_canon, file);
    if super::runs::active(&old_key).is_some() {
        return Err(FieldError::conflict("name", "This node is running. Rename it after the run finishes."));
    }
    std::fs::rename(file, &target).map_err(|e| FieldError {
        status: StatusCode::INTERNAL_SERVER_ERROR, field: "name", msg: e.to_string() })?;
    let new_key = rel_key(root_canon, &target);
    let (o, n) = (old_key.clone(), new_key.clone());
    let moved = store.write(move |conn| {
        let n = conn.execute("UPDATE mdai_runs SET path=?1 WHERE path=?2", rusqlite::params![n, o])?;
        let _ = n;
        Ok(WriteOutcome { applied: true, events: vec![] })
    });
    if let Err(e) = moved {
        tracing::warn!(target: "amux::mdai", from = %old_key, to = %new_key, error = %e,
            verdict = "mdai_rename_history_not_moved", "renamed a node but its run history stayed under the old name");
    }
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "---\ntitle: Workstreams\nmodel: opus\nrefresh: manual\ntimeout: 900\n\
fetch:\n  url: https://example.com/x\n  method: post\n  headers: {Accept: application/json}\n  connector: linear\n  select: data.items\n\
sources:\n- path: amux:messages?days=30&limit=500\n  prompt: Owner messages\n- notes/plan.md\n\
owner_note: keep me\n---\n# Where I left off\n\nWrite it.\n";

    #[test]
    fn every_field_round_trips_and_unknown_keys_survive() {
        let view = parse_view(FULL).unwrap();
        assert_eq!(view["title"], "Workstreams");
        assert_eq!(view["model"], "opus");
        assert_eq!(view["refresh"], "manual");
        assert_eq!(view["timeout_s"], 900);
        assert_eq!(view["fetch"]["method"], "POST");
        assert_eq!(view["fetch"]["headers"]["Accept"], "application/json");
        assert_eq!(view["sources"][0]["path"], "amux:messages?days=30&limit=500");
        assert_eq!(view["sources"][1]["path"], "notes/plan.md");
        assert_eq!(view["extra"]["owner_note"], "keep me");
        let text = render_doc(&view).unwrap();
        let again = parse_view(&text).unwrap();
        assert_eq!(again, view, "render then parse must be the identity on the view");
        let engine = parse_mdai(&text).unwrap();
        assert_eq!(engine.model.as_deref(), Some("opus"));
        assert_eq!(engine.sources.len(), 2);
        assert_eq!(engine.fetch.unwrap().connector, "linear");
        assert!(engine.body.starts_with("# Where I left off"));
        assert_eq!(view["body"], "# Where I left off\n\nWrite it.\n", "the body reads back exactly, final newline included");
    }

    #[test]
    fn invalid_fields_are_refused_and_named() {
        let base = parse_view(FULL).unwrap();
        let cases: Vec<(&str, Value, &str)> = vec![
            ("body", json!(""), "body"),
            ("timeout_s", json!(5), "timeout_s"),
            ("refresh", json!("always"), "refresh"),
            ("model", json!("opus; rm -rf"), "model"),
            ("sources", json!([{"path": "", "prompt": ""}]), "sources"),
            ("sources", json!([{"path": "amux:board", "prompt": ""}]), "sources"),
        ];
        for (k, v, field) in cases {
            let mut d = base.clone();
            d[k] = v.clone();
            if k == "body" {
                d["fetch"] = Value::Null;
            }
            let e = render_doc(&d).expect_err(&format!("{k}={v} must be refused"));
            assert_eq!(e.field, field, "{k}={v}");
        }
        let mut d = base.clone();
        d["fetch"]["headers"] = json!({"Authorization": "Bearer sk-x"});
        assert_eq!(render_doc(&d).unwrap_err().field, "fetch.headers");
    }

    #[test]
    fn a_write_cannot_leave_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(resolve_for_write(&root, "../x.mdai").is_err());
        assert!(resolve_for_write(&root, "/etc/x.mdai").is_err());
        assert!(resolve_for_write(&root, "notes.md").is_err(), "only .mdai files");
        let ok = resolve_for_write(&root, "new/folder/a.mdai").unwrap();
        assert!(ok.starts_with(&root));
    }
}
