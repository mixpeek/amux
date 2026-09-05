//! /api/scope — NATIVE port of the uniform scope contract (AMUX-2608: the
//! LAST `PROXIED_FAMILIES` row; its retirement empties the registry).
//!
//! Ported from the deleted Python server (historical amux-server.py, deleted at 792ce1f; line refs are into git history):
//! - descriptors: `_SCOPE_CAPS` (py:16076) — one row per scopable capability
//!   (memory/rules/env/gates/status_mode), each naming its storage, its
//!   settable levels and its OWN precedence order. The UI renders one widget
//!   per row and never learns that gates live in SQLite while memory lives
//!   in a .md file; a capability that does not accept a level SAYS SO.
//! - read: `_scope_read` (py:16103) — what is set AT one level, for every
//!   capability, reported even when unset ("nothing is set here" is the
//!   answer a configuration screen most needs).
//! - write: `_scope_write` (py:16215) — descriptor-driven; refuses an
//!   unsupported (level, capability) pair with 409 rather than accepting a
//!   value the resolver would outrank (the AMUX-2140 shape). Authorization
//!   `_scope_write_allowed` (py:16173) is restrictive BY POLICY: a session
//!   may write only its own worker layer; group/global belong to the human
//!   (no session header = the dashboard), unless AMUX_SCOPE_WRITE_AGENTS=1.
//! - routing: `path == "/api/scope"` with method==PUT for the write and NO
//!   method guard on the read — so GET/POST/PATCH/DELETE/HEAD all answer the
//!   read (verified live 2026-08-09: POST /api/scope returns the read, 200).
//!   Sub-paths fall through to Python's generic 404, which the static-files
//!   JSON 404 reproduces on this origin.
//!
//! Storage, shared with the Python server byte-for-byte:
//! - memory/rules: `~/.amux/memory/{_global,.md files}` (global), `memory/
//!   tags/<group>[.rules].md`, `memory/<worker>[.rules].md`
//! - env: `~/.amux/amux.env` (global), `~/.amux/env/<group>.env`,
//!   `~/.amux/sessions/<worker>.env` (0600 — env files carry credentials)
//! - gates: shared-DB `statuses.gate` (global) / `session_gates` with the
//!   `group:<name>` key convention (group) or the bare name (worker)
//! - status_mode: `statuses.mode` (global) / `status_scope` rows
//!
//! The write audit is REAL, not claimed (ethos rule 6): every scope mutation
//! inserts the same `interaction_log` row Python's `_ilog` writes — kind
//! "scope", action "write:<cap>", per-(kind,target) seq, before/after blobs
//! capped at 4000 chars — into the shared DB, where the existing audit
//! consumers already look.
//!
//! Memory compose: `write_claude_memory` in `api/session_verbs.rs` now
//! propagates worker memory writes into Claude's project MEMORY.md after
//! updating the store file.

use super::AppState;
use axum::extract::{Query, Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

/// Mounted at /api/scope in mod.rs. One `any` route: Python dispatches
/// PUT to the write and EVERY other method to the read (no method guard on
/// the read branch — live-verified), so a method router would invent 405s
/// Python never answers.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", axum::routing::any(handler))
        // Sub-paths are Python's generic 404 — routed explicitly because in
        // the full composition the static SPA catch-all out-competes a
        // nested fallback (the AMUX-2594 swallow) and would serve index.html.
        .route("/{*rest}", axum::routing::any(not_found))
}

async fn not_found() -> Response {
    j(404, json!({"error": "not found"}))
}

// ---------------------------------------------------------------------------
// The capability descriptors — Python's _SCOPE_CAPS, verbatim.
// ---------------------------------------------------------------------------

pub struct ScopeCap {
    pub key: &'static str,
    pub label: &'static str,
    pub levels: &'static [&'static str],
    pub kind: &'static str,
    /// The capability's OWN precedence order, least-specific first. Gates
    /// list `type` and `card` here because they OUTRANK the settable levels
    /// while being intrinsic to a card — the read publishes the order so the
    /// UI can say where a value actually comes from.
    pub order: &'static [&'static str],
    pub merge: &'static str,
    /// The per-worker "why did I get this value" endpoint. Carried in the
    /// descriptor as documentation (Python keeps it there too and, like us,
    /// never emits it in the read payload).
    ///
    /// `env` NOW ANSWERS; `memory` and `rules` STILL DO NOT (AF-295).
    ///
    /// All three pointed at a 501 until 2026-08-28. `env-explain` is
    /// implemented: it walks `scope_env_layers`, the same ordered list every
    /// consumer reads, and reports which layer supplied the value. It returns
    /// key names and never values, because these files are 0600 and carry
    /// credentials; an explain that echoed them would be a credential read over
    /// HTTP.
    ///
    /// `memory` and `rules` keep pointing at `memory-explain`, which is still
    /// 501 — now for a stated reason rather than a port gap. The composition it
    /// would explain is itself incomplete: these caps advertise a `group` level
    /// and `write_claude_memory` reads only global + worker, so
    /// `memory/tags/<group>.md` is written here and consumed by nothing
    /// (AF-296). An explain built on that would report a defect rather than
    /// explain a behaviour. Fix the composition first.
    ///
    /// Bounded either way, and checked rather than assumed: this field never
    /// reaches a consumer — nothing in the read payload carries it, verified
    /// against the live endpoint — so the only reader misled is someone reading
    /// this table, and the table now says so (ethos rule 6).
    pub explain: &'static str,
}

pub const SCOPE_CAPS: &[ScopeCap] = &[
    ScopeCap {
        key: "memory",
        label: "Memory",
        levels: &["global", "group", "worker"],
        kind: "text",
        order: &["global", "group", "worker"],
        merge: "concat",
        explain: "/api/sessions/{worker}/memory-explain",
    },
    ScopeCap {
        key: "rules",
        label: "Rules (binding)",
        levels: &["global", "group", "worker"],
        kind: "text",
        order: &["global", "group", "worker"],
        merge: "concat",
        explain: "/api/sessions/{worker}/memory-explain",
    },
    ScopeCap {
        key: "env",
        label: "Environment",
        levels: &["global", "group", "worker"],
        kind: "keys",
        order: &["global", "group", "worker"],
        merge: "merge-by-key",
        explain: "/api/sessions/{worker}/env-explain",
    },
    ScopeCap {
        key: "gates",
        label: "Board gates",
        levels: &["global", "group", "worker"],
        kind: "list-per-status",
        // card and type outrank all three and are NOT settable here: they
        // are intrinsic to a card, not to a scope (Python's own comment).
        order: &["global", "group", "worker", "type", "card"],
        merge: "replace",
        explain: "/api/board/gate?item={id}&status={status}",
    },
    // AMUX-2678. A SKIN is not a new subsystem — it is a scopable capability
    // like the five above, so it inherits their precedence (global -> group ->
    // worker), their write authorization, and their audit row for free. That is
    // the whole reason it belongs here rather than in a `/api/skins` of its
    // own: a vertical's UX becomes CONFIGURATION of a primitive, not code.
    //
    // merge-by-key, not replace: a group skin that renames one noun must not
    // have to restate the global palette, and a worker override of a single
    // colour must not drop the group's terminology.
    ScopeCap {
        key: "skin",
        label: "Skin (terms, colours, tabs)",
        levels: &["global", "group", "worker"],
        kind: "json",
        order: &["global", "group", "worker"],
        merge: "merge-by-key",
        explain: "/api/skin?worker={worker}",
    },
    ScopeCap {
        key: "status_mode",
        label: "Status availability",
        levels: &["global", "group", "worker"],
        kind: "mode",
        order: &["global", "group", "worker"],
        merge: "replace",
        explain: "/api/board/status-scope?session={worker}",
    },
    // Connectors: a configured third-party integration (Gmail, Slack, Drive, an
    // MCP tool server) turned on and mapped to an account per scope. Per the
    // connectors design (docs/design/connectors.md), a connector is NOT a new
    // subsystem — it is a scopable capability, so it inherits the same precedence
    // (global -> group -> worker), write authorization, and audit row as the six
    // above. Value shape per scope:
    //   { "<connector>": { "enabled": true, "account": "<id>", "mcp": "<server>" } }
    // merge-by-key like skin: a worker enabling one connector must not have to
    // restate the group's set, and a group mapping one account must not drop the
    // global defaults. Step 3 of the build order; storage mirrors skin (prefs).
    ScopeCap {
        key: "connectors",
        label: "Connectors (integrations)",
        levels: &["global", "group", "worker"],
        kind: "json",
        order: &["global", "group", "worker"],
        merge: "merge-by-key",
        explain: "/api/connectors?worker={worker}",
    },
];

fn cap_by_key(key: &str) -> Option<&'static ScopeCap> {
    SCOPE_CAPS.iter().find(|c| c.key == key)
}

/// Python `_SCOPE_TEXT_CAP`: display cap on shipped memory/rules text.
/// `truncated` exists so a capped read can never be written back — that
/// would turn a display limit into data loss.
fn scope_text_cap() -> usize {
    std::env::var("AMUX_SCOPE_TEXT_CAP")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(200_000)
}

// ---------------------------------------------------------------------------
// Storage paths (shared with Python)
// ---------------------------------------------------------------------------

/// memory/rules file for (level, name, cap-key) — py:16117-16121 (read) and
/// py:16233-16237 (write), identical derivations.
fn memory_file(home: &Path, level: &str, name: &str, key: &str) -> PathBuf {
    let suf = if key == "rules" { ".rules.md" } else { ".md" };
    let mem = home.join("memory");
    match level {
        "global" => mem.join(if key == "rules" { "_rules.md" } else { "_global.md" }),
        "group" => mem.join("tags").join(format!("{name}{suf}")),
        _ => mem.join(format!("{name}{suf}")),
    }
}

/// env file for (level, name) — py:16138-16140 / py:16259-16261.
fn env_file(home: &Path, level: &str, name: &str) -> PathBuf {
    match level {
        "global" => home.join("amux.env"),
        "group" => home.join("env").join(format!("{name}.env")),
        _ => home.join("sessions").join(format!("{name}.env")),
    }
}

// ---------------------------------------------------------------------------
// Fleet scan (groups / members decorations on the read)
// ---------------------------------------------------------------------------

/// (name, tags-in-declared-order, archived) for every non-blocked session —
/// the slice of Python's `list_sessions()` the scope read consumes. Blocked
/// (quarantined) sessions are excluded exactly as `list_sessions` excludes
/// them; `archived` is CC_ARCHIVED=1 (py:20346).
fn scan_fleet(home: &Path) -> Vec<(String, Vec<String>, bool)> {
    let blocked = super::groups::blocked_names(home);
    let Ok(entries) = std::fs::read_dir(home.join("sessions")) else {
        return vec![];
    };
    let mut rows: Vec<(String, Vec<String>, bool)> = vec![];
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("env") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        if blocked.contains(&name) {
            continue;
        }
        let env = crate::config::parse_env_file(&path);
        let tags = split_tags(env.get("CC_TAGS").map(String::as_str).unwrap_or(""));
        let archived = env.get("CC_ARCHIVED").map(|v| v == "1").unwrap_or(false);
        rows.push((name, tags, archived));
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// CC_TAGS split on ',' and TRIMMED, declared order kept — Python
/// `_session_tags_of` (py:21724; order is the documented AMUX-2311
/// tie-break, so no sorting here).
fn split_tags(raw: &str) -> Vec<String> {
    raw.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()
}

/// pub(crate) so the skin resolver uses the SAME reader (AMUX-2678). Two
/// readers of "which groups is this worker in" would eventually disagree, and
/// the scope screen and the resolved skin must not.
pub(crate) fn session_tags_of(home: &Path, name: &str) -> Vec<String> {
    let env = crate::config::parse_env_file(&home.join("sessions").join(format!("{name}.env")));
    split_tags(env.get("CC_TAGS").map(String::as_str).unwrap_or(""))
}

// ---------------------------------------------------------------------------
// The uniform read — Python _scope_read
// ---------------------------------------------------------------------------

/// What is set AT this level, for every capability. Each capability is
/// reported even when unset. Per-capability failures land in that row's
/// `error` field (Python's try/except per row) rather than failing the read.
fn scope_read(conn: Option<&rusqlite::Connection>, home: &Path, level: &str, name: &str) -> Value {
    let mut caps: Vec<Value> = vec![];
    for cap in SCOPE_CAPS {
        let mut row = Map::new();
        row.insert("key".into(), json!(cap.key));
        row.insert("label".into(), json!(cap.label));
        row.insert("kind".into(), json!(cap.kind));
        row.insert("order".into(), json!(cap.order));
        row.insert("merge".into(), json!(cap.merge));
        let supported = cap.levels.contains(&level);
        row.insert("supported".into(), json!(supported));
        row.insert("value".into(), Value::Null);
        row.insert("set_here".into(), json!(false));
        if supported {
            match read_cap_value(conn, home, level, name, cap.key) {
                Ok((value, set_here)) => {
                    row.insert("value".into(), value);
                    row.insert("set_here".into(), json!(set_here));
                }
                Err(e) => {
                    // Python: row["error"] = str(e)[:120]
                    let msg: String = e.chars().take(120).collect();
                    row.insert("error".into(), json!(msg));
                }
            }
        }
        caps.push(Value::Object(row));
    }
    json!({ "level": level, "name": name, "capabilities": caps })
}

/// One capability's (value, set_here) at one level — the storage-specific
/// half of the read.
fn read_cap_value(
    conn: Option<&rusqlite::Connection>,
    home: &Path,
    level: &str,
    name: &str,
    key: &str,
) -> Result<(Value, bool), String> {
    match key {
        "skin" => {
            // Stored in `prefs` rather than a file: a skin is structured
            // config the SPA reads on every load, and the prefs table is
            // already the DB-backed key/value the branding prefs use.
            let k = skin_pref_key(level, name);
            let raw: Option<String> = conn.and_then(|c| {
                c.query_row("SELECT value FROM prefs WHERE key=?1", rusqlite::params![k], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
            });
            let set = raw.as_deref().map(|v| !v.trim().is_empty()).unwrap_or(false);
            let parsed: Value = raw
                .as_deref()
                .and_then(|v| serde_json::from_str(v).ok())
                .unwrap_or_else(|| json!({}));
            Ok((json!({ "skin": parsed }), set))
        }
        "connectors" => {
            // Same prefs-backed JSON storage as skin (structured config the SPA
            // and the launch-time connector resolver read, keyed by scope).
            let k = connectors_pref_key(level, name);
            let raw: Option<String> = conn.and_then(|c| {
                c.query_row("SELECT value FROM prefs WHERE key=?1", rusqlite::params![k], |r| {
                    r.get::<_, String>(0)
                })
                .ok()
            });
            let set = raw.as_deref().map(|v| !v.trim().is_empty()).unwrap_or(false);
            let parsed: Value = raw
                .as_deref()
                .and_then(|v| serde_json::from_str(v).ok())
                .unwrap_or_else(|| json!({}));
            Ok((json!({ "connectors": parsed }), set))
        }
        "memory" | "rules" => {
            let f = memory_file(home, level, name, key);
            let sz = std::fs::metadata(&f).map(|m| m.len()).unwrap_or(0) as usize;
            let exists = f.exists();
            let cap = scope_text_cap();
            // Ship the TEXT, not just its size — an editor prefilled from a
            // size renders empty, and saving an empty box would silently
            // replace the file (Python's comment, kept because it is the
            // reason `text` + `truncated` are in this payload at all).
            let (text, truncated) = if sz > 0 {
                let raw = std::fs::read(&f).map_err(|e| e.to_string())?;
                let s = String::from_utf8_lossy(&raw);
                // Python slices CHARACTERS ([:_SCOPE_TEXT_CAP]) but compares
                // BYTES for `truncated` (st_size > cap) — both kept.
                let capped: String = s.chars().take(cap).collect();
                (capped, sz > cap)
            } else {
                (String::new(), false)
            };
            Ok((
                json!({
                    "path": f.to_string_lossy(),
                    "bytes": sz,
                    "text": text,
                    "truncated": truncated,
                }),
                exists && sz > 0,
            ))
        }
        "env" => {
            let f = env_file(home, level, name);
            let keys: Vec<String> = if f.exists() {
                crate::config::parse_env_file(&f).keys().cloned().collect()
            } else {
                vec![]
            };
            // BTreeMap keys are already sorted (Python: sorted(...keys())).
            let set = !keys.is_empty();
            Ok((json!({ "path": f.to_string_lossy(), "keys": keys }), set))
        }
        "gates" => {
            let conn = conn.ok_or("no db connection")?;
            let g: Vec<(String, Value)> = if level == "global" {
                load_board_statuses(conn)
                    .into_iter()
                    .map(|s| (s.id, s.gate))
                    .collect()
            } else {
                let skey = session_gate_key(level, name);
                load_session_gates(conn, &skey)?
            };
            // Only non-empty gates are the value (py: {k: v for ... if v}).
            let mut out = Map::new();
            for (st, items) in g {
                if crate::api::py_truthy(&items) {
                    out.insert(st, items);
                }
            }
            let set = !out.is_empty();
            Ok((Value::Object(out), set))
        }
        "status_mode" => {
            let conn = conn.ok_or("no db connection")?;
            if level == "global" {
                let mut out = Map::new();
                let mut any_explicit = false;
                for s in load_board_statuses(conn) {
                    any_explicit |= s.mode == "explicit";
                    out.insert(s.id, json!(s.mode));
                }
                Ok((Value::Object(out), any_explicit))
            } else {
                let lv = if level == "group" { "tag" } else { "session" };
                let mut stmt = conn
                    .prepare(
                        "SELECT DISTINCT status FROM status_scope
                         WHERE scope_type=?1 AND scope_value=?2 ORDER BY status",
                    )
                    .map_err(|e| e.to_string())?;
                let rows: Vec<String> = stmt
                    .query_map([lv, name], |r| r.get::<_, String>(0))
                    .map_err(|e| e.to_string())?
                    .flatten()
                    .collect();
                let set = !rows.is_empty();
                Ok((json!(rows), set))
            }
        }
        _ => Err(format!("unknown capability '{key}'")),
    }
}

/// session_gates key convention: `group:<name>` for groups, the bare name
/// for workers (py:16147 / py:16290).
/// Where a skin lives in `prefs`. One key shape for all three levels so a
/// resolver can enumerate them without knowing which exist.
fn skin_pref_key(level: &str, name: &str) -> String {
    match level {
        "global" => "skin:global".to_string(),
        "group" => format!("skin:group:{name}"),
        _ => format!("skin:worker:{name}"),
    }
}

/// Write a skin layer. The value is a JSON object; `{}` or null CLEARS the
/// layer rather than storing an empty object, so "unset" and "set to nothing"
/// stay distinguishable — the resolver treats an absent layer as transparent
/// and an empty one identically, but a configuration screen must be able to
/// say which it is.
async fn write_skin(
    state: &AppState,
    level: &str,
    name: &str,
    value: &Value,
) -> Result<(), (u16, String)> {
    let obj = value.get("skin").unwrap_or(value);
    if !obj.is_object() && !obj.is_null() {
        return Err((400, "skin must be a JSON object".into()));
    }
    let key = skin_pref_key(level, name);
    let empty = obj.is_null() || obj.as_object().map(|m| m.is_empty()).unwrap_or(true);
    let payload = if empty { None } else { Some(obj.to_string()) };
    state
        .store
        .write_async(move |conn| {
            match &payload {
                None => {
                    conn.execute("DELETE FROM prefs WHERE key=?1", rusqlite::params![key])?;
                }
                Some(v) => {
                    conn.execute(
                        "INSERT INTO prefs (key, value) VALUES (?1, ?2) \
                         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        rusqlite::params![key, v],
                    )?;
                }
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
        .map_err(|e| (500, format!("skin write failed: {e}")))?;
    Ok(())
}

/// Where a connectors layer lives in `prefs`. One key shape for all three levels
/// so the launch-time resolver can enumerate global -> group -> worker without
/// knowing which exist (mirrors [`skin_pref_key`]).
fn connectors_pref_key(level: &str, name: &str) -> String {
    match level {
        "global" => "connectors:global".to_string(),
        "group" => format!("connectors:group:{name}"),
        _ => format!("connectors:worker:{name}"),
    }
}

/// Write a connectors layer. The value is a JSON object mapping connector name to
/// `{ enabled, account, mcp }`; `{}` or null CLEARS the layer rather than storing
/// an empty object, so "unset" and "set to nothing" stay distinguishable. Storage
/// and semantics mirror [`write_skin`] exactly (prefs-backed, DB write_async).
async fn write_connectors(
    state: &AppState,
    level: &str,
    name: &str,
    value: &Value,
) -> Result<(), (u16, String)> {
    let obj = value.get("connectors").unwrap_or(value);
    if !obj.is_object() && !obj.is_null() {
        return Err((400, "connectors must be a JSON object".into()));
    }
    let key = connectors_pref_key(level, name);
    let empty = obj.is_null() || obj.as_object().map(|m| m.is_empty()).unwrap_or(true);
    let payload = if empty { None } else { Some(obj.to_string()) };
    state
        .store
        .write_async(move |conn| {
            match &payload {
                None => {
                    conn.execute("DELETE FROM prefs WHERE key=?1", rusqlite::params![key])?;
                }
                Some(v) => {
                    conn.execute(
                        "INSERT INTO prefs (key, value) VALUES (?1, ?2) \
                         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                        rusqlite::params![key, v],
                    )?;
                }
            }
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await
        .map_err(|e| (500, format!("connectors write failed: {e}")))?;
    Ok(())
}

fn session_gate_key(level: &str, name: &str) -> String {
    if level == "group" {
        format!("group:{name}")
    } else {
        name.to_string()
    }
}

struct StatusRow {
    id: String,
    gate: Value,
    mode: String,
}

/// Python `_load_board_statuses` (py:15933), the slice this module needs:
/// id + parsed gate + mode ordered by position, with the builtin default
/// set (which carries NO gates/modes) when the table is empty.
fn load_board_statuses(conn: &rusqlite::Connection) -> Vec<StatusRow> {
    let mut out = vec![];
    if let Ok(mut stmt) =
        conn.prepare("SELECT id, label, gate, mode FROM statuses ORDER BY position")
    {
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        }) {
            for (id, gate, mode) in rows.flatten() {
                out.push(StatusRow {
                    id,
                    gate: gate
                        .as_deref()
                        .and_then(|g| serde_json::from_str(g).ok())
                        .unwrap_or_else(|| json!([])),
                    mode: mode.filter(|m| !m.is_empty()).unwrap_or_else(|| "implicit".into()),
                });
            }
        }
    }
    if out.is_empty() {
        // _DEFAULT_STATUSES have no gate/mode keys; `s.get("gate") or []`
        // and `s.get("mode") or "implicit"` produce exactly this.
        for id in ["backlog", "todo", "doing", "review", "done", "verified", "discarded"] {
            out.push(StatusRow { id: id.into(), gate: json!([]), mode: "implicit".into() });
        }
    }
    out
}

/// The one session's gate overrides {status: items}, non-empty only
/// (Python `_load_session_gates` filtered to the key the read wants).
fn load_session_gates(
    conn: &rusqlite::Connection,
    skey: &str,
) -> Result<Vec<(String, Value)>, String> {
    let mut stmt = conn
        .prepare("SELECT status, gate FROM session_gates WHERE session=?1")
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, Value)> = stmt
        .query_map([skey], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|(st, g)| {
            (
                st,
                g.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or(json!([])),
            )
        })
        .collect();
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Authorization — Python _scope_write_allowed, verbatim strings
// ---------------------------------------------------------------------------

/// Who may write which scope layer -> (allowed, why). RESTRICTIVE BY POLICY
/// (Ethan's, flagged under ethos #8 in the Python source): a session that
/// could write the group or global layer could grant ITSELF whatever that
/// layer confers, and every peer wearing the tag inherits it. A session
/// writing its OWN worker layer grants itself nothing it did not have.
/// "Human" = no X-Amux-Worker/X-Amux-Session header = the dashboard (the
/// same convention `_caller_scope` uses for tag isolation).
fn scope_write_allowed(level: &str, name: &str, actor: &str) -> (bool, String) {
    let open = std::env::var("AMUX_SCOPE_WRITE_AGENTS")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false);
    if open {
        return (true, "AMUX_SCOPE_WRITE_AGENTS=1 (all levels open to sessions)".into());
    }
    if actor.is_empty() {
        return (true, "human (no session header — dashboard)".into());
    }
    if level == "worker" && name == actor {
        return (true, format!("session writing its own worker layer ({actor})"));
    }
    if level == "worker" {
        return (
            false,
            format!(
                "'{actor}' may not configure another worker ('{name}'). \
                 A session reconfiguring a peer is a human's call."
            ),
        );
    }
    (
        false,
        format!(
            "'{actor}' may not write the {level} layer. Everything wearing \
             it inherits the change, so a session could grant itself and \
             its peers new capability. Do it from the dashboard, or set \
             AMUX_SCOPE_WRITE_AGENTS=1 if you want sessions to have this."
        ),
    )
}

// ---------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------

fn j(status: u16, v: Value) -> Response {
    (StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), Json(v))
        .into_response()
}

pub async fn handler(State(state): State<AppState>, req: Request) -> Response {
    if *req.method() == Method::PUT {
        return put_scope(state, req).await;
    }
    // EVERY non-PUT method answers the read — Python's read branch carries
    // no method guard (verified live: POST /api/scope returns the read).
    let q: HashMap<String, String> = Query::try_from_uri(req.uri())
        .map(|Query(q)| q)
        .unwrap_or_default();
    // Python: (qs.get("level", ["global"])[0] or "global").strip() — the
    // default applies to a MISSING/empty param, and the strip runs after, so
    // `level=%20` validates (and 400s) as "" rather than defaulting.
    let level_owned = match q.get("level").map(String::as_str) {
        None | Some("") => "global".to_string(),
        Some(v) => v.to_string(),
    };
    let level = level_owned.trim();
    let name = q.get("name").map(|s| s.trim()).unwrap_or("");
    if !matches!(level, "global" | "group" | "worker") {
        return j(400, json!({"error": "level must be global, group or worker"}));
    }
    if level != "global" && name.is_empty() {
        return j(400, json!({"error": format!("name required for level={level}")}));
    }
    let home = super::groups::amux_home();
    let conn = state.store.read().ok();
    let mut out = match scope_read(conn.as_deref(), &home, level, name) {
        Value::Object(m) => m,
        _ => unreachable!("scope_read returns an object"),
    };
    // The level-specific decoration (py:71432-71441): a worker names its
    // groups, a group its members, global every group in the fleet.
    match level {
        "worker" => {
            out.insert("groups".into(), json!(session_tags_of(&home, name)));
        }
        "group" => {
            let members: Vec<String> = scan_fleet(&home)
                .into_iter()
                .filter(|(_, tags, archived)| !archived && tags.iter().any(|t| t == name))
                .map(|(n, _, _)| n)
                .collect();
            // scan_fleet is name-sorted; Python sorts too.
            out.insert("members".into(), json!(members));
        }
        _ => {
            let groups: BTreeSet<String> =
                scan_fleet(&home).into_iter().flat_map(|(_, tags, _)| tags).collect();
            out.insert("groups".into(), json!(groups));
        }
    }
    j(200, Value::Object(out))
}

async fn put_scope(state: AppState, req: Request) -> Response {
    let actor = super::groups::hdr_worker(req.headers());
    let bytes = match axum::body::to_bytes(req.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(e) => return j(400, json!({"error": e.to_string()})),
    };
    let body: Value = if bytes.is_empty() {
        json!({})
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({}))
    };
    let level = body["level"].as_str().unwrap_or("").trim().to_string();
    let name = body["name"].as_str().unwrap_or("").trim().to_string();
    let capability = body["capability"].as_str().unwrap_or("").trim().to_string();

    // A level naming a real PRECEDENCE layer that is not SETTABLE (`type`,
    // `card`) falls through to scope_write, which explains WHY rather than a
    // flat "level must be…" — someone trying level=type is reasoning
    // correctly from what the read half told them; the answer should teach.
    // (Python's comment; without this the 409 below would be unreachable —
    // an unfalsifiable guard, which ethos #7 counts as no guard at all.)
    let known_layers: BTreeSet<&str> =
        SCOPE_CAPS.iter().flat_map(|c| c.order.iter().copied()).collect();
    if !matches!(level.as_str(), "global" | "group" | "worker")
        && !known_layers.contains(level.as_str())
    {
        return j(
            400,
            json!({
                "error": "level must be global, group or worker",
                "precedence_layers": known_layers,
            }),
        );
    }
    if matches!(level.as_str(), "group" | "worker") && name.is_empty() {
        return j(400, json!({"error": format!("name required for level={level}")}));
    }
    if capability.is_empty() {
        return j(
            400,
            json!({
                "error": "capability required",
                "capabilities": SCOPE_CAPS.iter().map(|c| c.key).collect::<Vec<_>>(),
            }),
        );
    }
    scope_write(&state, &level, &name, &capability, body.get("value"), &actor).await
}

/// Python `_scope_write`: validate → authorize → before-read → storage write
/// → after-read → audit → respond. Errors carry Python's exact strings.
async fn scope_write(
    state: &AppState,
    level: &str,
    name: &str,
    key: &str,
    value: Option<&Value>,
    actor: &str,
) -> Response {
    let Some(cap) = cap_by_key(key) else {
        return j(400, json!({"error": format!("unknown capability '{key}'")}));
    };
    if !cap.levels.contains(&level) {
        let mut order_rev: Vec<&str> = cap.order.to_vec();
        order_rev.reverse();
        return j(
            409,
            json!({"error": format!(
                "'{}' is not settable at {} level (settable at: {}). Its \
                 precedence order is {}, and levels outside that list are \
                 intrinsic to the item, not to a scope.",
                cap.label,
                level,
                cap.levels.join(", "),
                order_rev.join(" > "),
            )}),
        );
    }
    let (ok, why) = scope_write_allowed(level, name, actor);
    if !ok {
        return j(403, json!({"error": why}));
    }

    let home = super::groups::amux_home();
    let before = {
        let conn = state.store.read().ok();
        read_cap_value(conn.as_deref(), &home, level, name, key).ok()
    };

    let value = value.cloned().unwrap_or(Value::Null);
    let write_result: Result<(), (u16, String)> = match key {
        "memory" | "rules" => write_memory(&home, level, name, key, &value),
        "env" => write_env(&home, level, name, &value),
        "gates" => write_gates(state, level, name, &value).await,
        "status_mode" => write_status_mode(state, level, name, &value).await,
        "skin" => write_skin(state, level, name, &value).await,
        "connectors" => write_connectors(state, level, name, &value).await,
        // Structural fallback (py:16316) — unreachable while every
        // descriptor above has a write arm, kept so a future capability
        // without one fails honestly instead of silently succeeding.
        _ => Err((501, format!("capability '{key}' has no write path"))),
    };
    if let Err((status, msg)) = write_result {
        return j(status, json!({"error": msg}));
    }

    let after = {
        let conn = state.store.read().ok();
        read_cap_value(conn.as_deref(), &home, level, name, key).ok()
    };
    let (after_value, set_here) = after.clone().unwrap_or((Value::Null, false));

    // The audit row (ethos rule 6: a scope MUTATION with no trace is the
    // same gap as an unlogged gate bypass). Same table, same fields, same
    // per-(kind,target) seq as Python's _ilog — the consumers already read
    // interaction_log. Best-effort like Python: logging must never break
    // the action that succeeded.
    ilog_scope_write(
        state,
        actor,
        level,
        name,
        key,
        &why,
        before.map(|(v, _)| v),
        after_value.clone(),
    )
    .await;

    j(
        200,
        json!({
            "ok": true,
            "level": level,
            "name": name,
            "capability": key,
            "set_here": set_here,
            "value": after_value,
            "actor": if actor.is_empty() { "human" } else { actor },
            "why_allowed": why,
        }),
    )
}

// ---------------------------------------------------------------------------
// Storage writers
// ---------------------------------------------------------------------------

/// memory/rules: the store file is the source of truth (worker-level push
/// into Claude's MEMORY.md is the module-doc deviation).
fn write_memory(
    home: &Path,
    level: &str,
    name: &str,
    key: &str,
    value: &Value,
) -> Result<(), (u16, String)> {
    let f = memory_file(home, level, name, key);
    if let Some(parent) = f.parent() {
        std::fs::create_dir_all(parent).map_err(|e| (500, e.to_string()))?;
    }
    // Python: value if str else (value or {}).get("text", "")
    let text = match value {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
        _ => String::new(),
    };
    std::fs::write(&f, text).map_err(|e| (500, e.to_string()))
}

/// env: whole-file replace for a string value; merge-by-key for an object
/// (null deletes a key). 0600 — env files carry credentials.
fn write_env(home: &Path, level: &str, name: &str, value: &Value) -> Result<(), (u16, String)> {
    let f = env_file(home, level, name);
    match value {
        Value::String(s) => {
            let text = if s.ends_with('\n') {
                s.clone()
            } else {
                format!("{s}\n")
            };
            crate::api::session_verbs::EnvFile::replace_text(&f, &text)
                .map_err(|e| (500, e.to_string()))?;
        }
        _ => {
            let mut updates = Vec::new();
            if let Some(obj) = value.as_object() {
                for (k, v) in obj {
                    let value = match v {
                        Value::Null => None,
                        Value::String(s) => Some(s.clone()),
                        Value::Bool(b) => Some(if *b { "True".into() } else { "False".into() }),
                        other => Some(other.to_string()),
                    };
                    updates.push((k.clone(), value));
                }
            }
            crate::api::session_verbs::EnvFile::merge_plain(&f, &updates)
                .map_err(|e| (500, e.to_string()))?;
        }
    }
    Ok(())
}

/// gates: global → statuses.gate per status; group/worker → session_gates
/// upsert (non-empty) / delete (empty) under the group:/bare key.
async fn write_gates(
    state: &AppState,
    level: &str,
    name: &str,
    value: &Value,
) -> Result<(), (u16, String)> {
    let items: Vec<(String, Value)> = value
        .as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let global = level == "global";
    let skey = session_gate_key(level, name);
    state
        .store
        .write_async(move |conn| {
            for (st, gate_items) in &items {
                if global {
                    let gate_txt: Option<String> = if crate::api::py_truthy(gate_items) {
                        Some(serde_json::to_string(gate_items).unwrap_or_else(|_| "[]".into()))
                    } else {
                        None
                    };
                    conn.execute(
                        // See AMUX-2641 / board.rs: a human-authored gate is
                        // flagged so enforcement can tell it from a seed row.
                        "UPDATE statuses SET gate=?1, gate_custom=1 WHERE id=?2",
                        rusqlite::params![gate_txt, st],
                    )?;
                } else if crate::api::py_truthy(gate_items) {
                    conn.execute(
                        "INSERT INTO session_gates (session, status, gate) VALUES (?1,?2,?3) \
                         ON CONFLICT(session, status) DO UPDATE SET gate=excluded.gate",
                        rusqlite::params![
                            skey,
                            st,
                            serde_json::to_string(gate_items).unwrap_or_else(|_| "[]".into())
                        ],
                    )?;
                } else {
                    conn.execute(
                        "DELETE FROM session_gates WHERE session=?1 AND status=?2",
                        rusqlite::params![skey, st],
                    )?;
                }
            }
            Ok(scope_outcome())
        })
        .await
        .map(|_| ())
        .map_err(|e| (500, e.to_string()))
}

/// status_mode: global → statuses.mode (implicit|explicit only); group/
/// worker → status_scope set-diff under scope_type tag/session.
async fn write_status_mode(
    state: &AppState,
    level: &str,
    name: &str,
    value: &Value,
) -> Result<(), (u16, String)> {
    if level == "global" {
        let modes: Vec<(String, String)> = value
            .as_object()
            .map(|o| {
                o.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default();
        // Validate BEFORE applying: Python bails mid-loop without commit, so
        // an invalid mode leaves nothing persisted on either origin.
        for (_, mode) in &modes {
            if mode != "implicit" && mode != "explicit" {
                return Err((
                    400,
                    format!("mode must be implicit or explicit, got '{mode}'"),
                ));
            }
        }
        state
            .store
            .write_async(move |conn| {
                for (st, mode) in &modes {
                    conn.execute(
                        "UPDATE statuses SET mode=?1 WHERE id=?2",
                        rusqlite::params![mode, st],
                    )?;
                }
                Ok(scope_outcome())
            })
            .await
            .map(|_| ())
            .map_err(|e| (500, e.to_string()))
    } else {
        let lv = if level == "group" { "tag" } else { "session" };
        let wanted: BTreeSet<String> = value
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let name = name.to_string();
        state
            .store
            .write_async(move |conn| {
                let have: BTreeSet<String> = conn
                    .prepare(
                        "SELECT DISTINCT status FROM status_scope \
                         WHERE scope_type=?1 AND scope_value=?2",
                    )?
                    .query_map([lv, name.as_str()], |r| r.get::<_, String>(0))?
                    .flatten()
                    .collect();
                for st in wanted.difference(&have) {
                    conn.execute(
                        "INSERT OR REPLACE INTO status_scope (status, scope_type, scope_value) \
                         VALUES (?1,?2,?3)",
                        rusqlite::params![st, lv, name],
                    )?;
                }
                for st in have.difference(&wanted) {
                    conn.execute(
                        "DELETE FROM status_scope WHERE status=?1 AND scope_type=?2 \
                         AND scope_value=?3",
                        rusqlite::params![st, lv, name],
                    )?;
                }
                Ok(scope_outcome())
            })
            .await
            .map(|_| ())
            .map_err(|e| (500, e.to_string()))
    }
}

/// Every scope write journals one Other("scope") event — the delta-sync/SSE
/// clients learn "scope changed, refetch" the same way group_config writes
/// announce themselves.
fn scope_outcome() -> crate::db::WriteOutcome {
    crate::db::WriteOutcome {
        applied: true,
        events: vec![crate::db::PendingEvent {
            entity_type: amux_core::revision::EntityType::Other("scope".into()),
            entity_id: "scope".into(),
            mutation: amux_core::revision::MutationKind::Updated,
            payload: None,
        }],
    }
}

// ---------------------------------------------------------------------------
// Audit — Python _ilog, the scope-write call site
// ---------------------------------------------------------------------------

/// Python `_ILOG_BLOB_CAP`: per-field cap so one huge value cannot bloat
/// the audit table.
const ILOG_BLOB_CAP: usize = 4000;

fn blob(v: Option<Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.chars().take(ILOG_BLOB_CAP).collect(),
        Some(other) => serde_json::to_string(&other)
            .unwrap_or_default()
            .chars()
            .take(ILOG_BLOB_CAP)
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn ilog_scope_write(
    state: &AppState,
    actor: &str,
    level: &str,
    name: &str,
    key: &str,
    why: &str,
    before: Option<Value>,
    result: Value,
) {
    let kind = "scope".to_string();
    let action = format!("write:{key}");
    let actor = if actor.is_empty() { "human".to_string() } else { actor.to_string() };
    let target = format!("{level}:{}", if name.is_empty() { "global" } else { name });
    let detail = blob(Some(json!({
        "capability": key, "level": level, "name": name, "why_allowed": why,
    })));
    let before = blob(before);
    let result = blob(Some(result));
    let ts = chrono::Utc::now().timestamp_millis();
    let res = state
        .store
        .write_async(move |conn| {
            // Per-(kind,target) sequence, exactly Python's derivation.
            let seq: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(seq),0) FROM interaction_log WHERE kind=?1 AND target=?2",
                    rusqlite::params![kind, target],
                    |r| r.get(0),
                )
                .unwrap_or(0)
                + 1;
            conn.execute(
                "INSERT INTO interaction_log \
                 (ts,kind,actor,target,action,url,detail,before,result,ok,ms,seq) \
                 VALUES (?1,?2,?3,?4,?5,'',?6,?7,?8,1,0,?9)",
                rusqlite::params![ts, kind, actor, target, action, detail, before, result, seq],
            )?;
            // Audit rows ride the transaction but announce nothing: Python's
            // _ilog emits no client-visible event either.
            Ok(crate::db::WriteOutcome { applied: false, events: vec![] })
        })
        .await;
    if let Err(e) = res {
        tracing::warn!("scope ilog failed (write already applied): {e}");
    }
}

// ---------------------------------------------------------------------------
// Tests — hermetic: temp AMUX_HOME via explicit paths, temp store.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    fn state() -> AppState {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(crate::db::Store::open(&dir.path().join("t.db")).unwrap());
        std::mem::forget(dir);
        AppState {
            secrets: std::sync::Arc::new(crate::secrets::SecretStore::new(std::path::PathBuf::new(), std::path::PathBuf::new())),
            store,
            started: std::time::Instant::now(),
            build_hash: "test".into(),
            auth_token: None,
        reconciled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn app(state: &AppState) -> Router {
        Router::new().nest("/api/scope", routes()).with_state(state.clone())
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
        session: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut b = axum::http::Request::builder().method(method).uri(uri);
        if let Some(s) = session {
            b = b.header("x-amux-session", s);
        }
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v).unwrap())
            }
            None => Body::empty(),
        };
        let res = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn fleet(home: &Path, sessions: &[(&str, &str, bool)]) {
        let sd = home.join("sessions");
        std::fs::create_dir_all(&sd).unwrap();
        for (name, tags, archived) in sessions {
            std::fs::write(
                sd.join(format!("{name}.env")),
                format!("CC_TAGS=\"{tags}\"\nCC_ARCHIVED={}\n", if *archived { 1 } else { 0 }),
            )
            .unwrap();
        }
    }

    fn cap<'a>(v: &'a Value, key: &str) -> &'a Value {
        v["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["key"] == key)
            .unwrap_or_else(|| panic!("capability {key} missing"))
    }

    #[tokio::test]
    async fn read_shapes_and_level_decorations() {
        let home = tempfile::tempdir().unwrap();
        fleet(
            home.path(),
            &[("w1", "alpha, beta", false), ("w2", "alpha", true), ("w3", "", false)],
        );
        // Shared AMUX_HOME guard (settings::test_env): the var is
        // process-global; the RAII guard serializes and restores it.
        let _home = crate::api::settings::test_env::set_home(home.path());
        let st = state();
        let app = app(&st);

        // Default level is global; every capability reported, unset.
        let (s, v) = call(&app, "GET", "/api/scope", None, None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["level"], "global");
        // The FIVE python capabilities must all still be here — that is what
        // this assertion was protecting. Amux-native additions (skin,
        // AMUX-2678) are allowed on top now that python is deleted, so assert
        // the parity SET rather than a count, which would have to be edited on
        // every addition and says nothing about whether parity survived.
        {
            // The READ publishes descriptor OBJECTS (key/label/levels/...),
            // unlike the PUT refusal which lists bare names — two shapes, and
            // asserting the wrong one unwraps None.
            let caps: Vec<&str> = v["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|c| c.get("key").and_then(|k| k.as_str()))
                .collect();
            for expected in ["memory", "rules", "env", "gates", "status_mode"] {
                assert!(caps.contains(&expected), "python capability {expected} is missing: {caps:?}");
            }
            assert!(caps.contains(&"skin"), "the native skin capability should be published: {caps:?}");
        }
        // Global groups include ARCHIVED sessions' tags (Python filters
        // archived only for group members).
        assert_eq!(v["groups"], json!(["alpha", "beta"]));
        let m = cap(&v, "memory");
        assert_eq!(m["set_here"], false);
        assert_eq!(m["value"]["bytes"], 0);
        assert_eq!(m["value"]["text"], "");
        assert_eq!(m["value"]["truncated"], false);
        assert!(m["value"]["path"].as_str().unwrap().ends_with("memory/_global.md"));
        assert_eq!(cap(&v, "gates")["value"], json!({}), "seeded statuses carry no gates");
        // Seeded builtin statuses: all implicit.
        assert_eq!(cap(&v, "status_mode")["value"]["verified"], "implicit");
        assert_eq!(cap(&v, "status_mode")["set_here"], false);
        assert_eq!(cap(&v, "gates")["order"], json!(["global", "group", "worker", "type", "card"]));

        // POST answers the read too (Python has no method guard on it).
        let (s, v2) = call(&app, "POST", "/api/scope", None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v2["level"], "global");

        // Group: members exclude archived, exact-case tag match, sorted.
        let (s, v) = call(&app, "GET", "/api/scope?level=group&name=alpha", None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["members"], json!(["w1"]), "w2 is archived");
        assert!(v["capabilities"][0]["value"]["path"]
            .as_str()
            .unwrap()
            .ends_with("memory/tags/alpha.md"));

        // Worker: groups in DECLARED order.
        let (s, v) = call(&app, "GET", "/api/scope?level=worker&name=w1", None, None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["groups"], json!(["alpha", "beta"]));

        // Validation, Python's exact strings.
        let (s, v) = call(&app, "GET", "/api/scope?level=bogus", None, None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "level must be global, group or worker");
        let (s, v) = call(&app, "GET", "/api/scope?level=group", None, None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "name required for level=group");

    }

    #[tokio::test]
    async fn put_validation_and_authorization_python_strings() {
        let home = tempfile::tempdir().unwrap();
        fleet(home.path(), &[("w1", "alpha", false)]);
        let _home = crate::api::settings::test_env::set_home(home.path());
        let st = state();
        let app = app(&st);

        // Unknown level → 400 naming the real precedence layers.
        let (s, v) = call(&app, "PUT", "/api/scope", Some(json!({"level": "bogus"})), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "level must be global, group or worker");
        assert_eq!(v["precedence_layers"], json!(["card", "global", "group", "type", "worker"]));

        // A KNOWN precedence layer that is not settable reaches the 409
        // that teaches (not the flat 400) — the guard is falsifiable.
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "type", "capability": "gates", "value": {}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{v}");
        assert_eq!(
            v["error"],
            "'Board gates' is not settable at type level (settable at: global, group, worker). \
             Its precedence order is card > type > worker > group > global, and levels outside \
             that list are intrinsic to the item, not to a scope."
        );

        let (s, v) =
            call(&app, "PUT", "/api/scope", Some(json!({"level": "group"})), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "name required for level=group");

        let (s, v) =
            call(&app, "PUT", "/api/scope", Some(json!({"level": "global"})), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "capability required");
        {
            // Same rule as above: the python five, in their python ORDER
            // (the refusal lists them and the UI renders them in order), with
            // native additions permitted to follow or interleave.
            let caps: Vec<&str> =
                v["capabilities"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
            let py: Vec<&str> = caps
                .iter()
                .copied()
                .filter(|c| ["memory", "rules", "env", "gates", "status_mode"].contains(c))
                .collect();
            assert_eq!(py, vec!["memory", "rules", "env", "gates", "status_mode"], "{caps:?}");
        }

        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "global", "capability": "nope"})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "unknown capability 'nope'");

        // A session may not write global/group, nor another worker; it may
        // write itself. Exact policy strings (the SPA shows them verbatim).
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "global", "capability": "memory", "value": {"text": "x"}})),
            Some("probe"),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(
            v["error"],
            "'probe' may not write the global layer. Everything wearing it inherits the \
             change, so a session could grant itself and its peers new capability. Do it \
             from the dashboard, or set AMUX_SCOPE_WRITE_AGENTS=1 if you want sessions to \
             have this."
        );
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "worker", "name": "other", "capability": "memory",
                        "value": {"text": "x"}})),
            Some("probe"),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(
            v["error"],
            "'probe' may not configure another worker ('other'). A session reconfiguring \
             a peer is a human's call."
        );
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "worker", "name": "probe", "capability": "memory",
                        "value": {"text": "self-note"}})),
            Some("probe"),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["why_allowed"], "session writing its own worker layer (probe)");
        assert_eq!(v["actor"], "probe");

    }

    #[tokio::test]
    async fn writes_land_in_python_storage_and_are_audited() {
        let home = tempfile::tempdir().unwrap();
        fleet(home.path(), &[("w1", "alpha", false)]);
        let _home = crate::api::settings::test_env::set_home(home.path());
        let st = state();
        let app = app(&st);

        // memory at group level → memory/tags/alpha.md; read reflects it.
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "group", "name": "alpha", "capability": "memory",
                        "value": {"text": "# alpha notes\n"}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["ok"], true);
        assert_eq!(v["set_here"], true);
        assert_eq!(v["actor"], "human");
        assert_eq!(v["why_allowed"], "human (no session header — dashboard)");
        assert_eq!(v["value"]["text"], "# alpha notes\n");
        assert_eq!(
            std::fs::read_to_string(home.path().join("memory/tags/alpha.md")).unwrap(),
            "# alpha notes\n"
        );

        // env merge-by-key: null deletes, values stringify; file is 0600.
        std::fs::create_dir_all(home.path().join("env")).unwrap();
        std::fs::write(home.path().join("env/alpha.env"), "OLD=1\nKEEP=x\n").unwrap();
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "group", "name": "alpha", "capability": "env",
                        "value": {"NEW": "val", "OLD": null}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let env_txt = std::fs::read_to_string(home.path().join("env/alpha.env")).unwrap();
        assert_eq!(env_txt, "KEEP=x\nNEW=val\n");
        assert_eq!(v["value"]["keys"], json!(["KEEP", "NEW"]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.path().join("env/alpha.env"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "env files carry credentials");
        }

        // gates at group level → session_gates under the group: key; empty
        // list deletes the row. Read round-trips.
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "group", "name": "alpha", "capability": "gates",
                        "value": {"verified": ["peer-reviewed"], "done": []}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["value"], json!({"verified": ["peer-reviewed"]}));
        {
            let conn = st.store.read().unwrap();
            let (sess, gate): (String, String) = conn
                .query_row(
                    "SELECT session, gate FROM session_gates WHERE session='group:alpha'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(sess, "group:alpha");
            assert_eq!(gate, "[\"peer-reviewed\"]");
        }
        let (_, v) = call(&app, "GET", "/api/scope?level=group&name=alpha", None, None).await;
        assert_eq!(cap(&v, "gates")["value"], json!({"verified": ["peer-reviewed"]}));
        assert_eq!(cap(&v, "gates")["set_here"], true);
        // Empty list removes the override.
        let (s, _) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "group", "name": "alpha", "capability": "gates",
                        "value": {"verified": []}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, v) = call(&app, "GET", "/api/scope?level=group&name=alpha", None, None).await;
        assert_eq!(cap(&v, "gates")["value"], json!({}));

        // gates at GLOBAL level → statuses.gate.
        let (s, _) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "global", "capability": "gates",
                        "value": {"done": ["merged"]}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, v) = call(&app, "GET", "/api/scope?level=global", None, None).await;
        assert_eq!(cap(&v, "gates")["value"], json!({"done": ["merged"]}));

        // status_mode global: validation refuses a bad mode with nothing
        // persisted; a good write flips the column.
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "global", "capability": "status_mode",
                        "value": {"verified": "sometimes"}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "mode must be implicit or explicit, got 'sometimes'");
        let (s, _) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "global", "capability": "status_mode",
                        "value": {"verified": "explicit"}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, v) = call(&app, "GET", "/api/scope", None, None).await;
        assert_eq!(cap(&v, "status_mode")["value"]["verified"], "explicit");
        assert_eq!(cap(&v, "status_mode")["set_here"], true);

        // status_mode group: set-diff against status_scope (tag rows).
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "group", "name": "alpha", "capability": "status_mode",
                        "value": ["verified"]})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["value"], json!(["verified"]));
        let (s, _) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "group", "name": "alpha", "capability": "status_mode",
                        "value": []})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        let (_, v) = call(&app, "GET", "/api/scope?level=group&name=alpha", None, None).await;
        assert_eq!(cap(&v, "status_mode")["value"], json!([]));

        // The audit is REAL (ethos rule 6): every write above left an
        // interaction_log row with Python's field shape, and seq counts
        // per (kind, target).
        let conn = st.store.read().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM interaction_log WHERE kind='scope'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(n >= 7, "expected an audit row per applied write, got {n}");
        let (actor, action, target, seq): (String, String, String, i64) = conn
            .query_row(
                "SELECT actor, action, target, seq FROM interaction_log \
                 WHERE kind='scope' AND action='write:gates' AND target='group:alpha' \
                 ORDER BY id DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(actor, "human");
        assert_eq!(action, "write:gates");
        assert_eq!(target, "group:alpha");
        // seq counts per (kind, target) across ALL actions — Python's
        // derivation. group:alpha saw memory, env, gates, gates in order,
        // so the second gates write is the 4th row at this target.
        assert_eq!(seq, 4, "4th write at (scope, group:alpha)");

    }

    /// Connectors step 3 (AMUX-3105 data model): `connectors` is a scopable
    /// capability whose per-scope JSON round-trips through prefs, mirroring skin.
    #[tokio::test]
    async fn connectors_layer_writes_and_reads_back() {
        let home = tempfile::tempdir().unwrap();
        fleet(home.path(), &[("w1", "alpha", false)]);
        let _home = crate::api::settings::test_env::set_home(home.path());
        let st = state();
        let app = app(&st);

        // It is advertised as a capability (the connectors tab reads the list).
        let (s, v) = call(&app, "PUT", "/api/scope", Some(json!({"level": "global"})), None).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let caps: Vec<&str> =
            v["capabilities"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
        assert!(caps.contains(&"connectors"), "connectors must be advertised: {caps:?}");

        // A worker-level connector mapping round-trips.
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "worker", "name": "w1", "capability": "connectors",
                        "value": {"gmail": {"enabled": true, "account": "ethan@mixpeek.com", "mcp": "gmail"}}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["ok"], true);
        assert_eq!(v["set_here"], true);
        assert_eq!(v["value"]["connectors"]["gmail"]["account"], "ethan@mixpeek.com");
        assert_eq!(v["value"]["connectors"]["gmail"]["enabled"], true);

        // A second write REPLACES the layer; cross-LEVEL merge-by-key is the
        // resolver's job, not the writer's (mirrors skin).
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "worker", "name": "w1", "capability": "connectors",
                        "value": {"slack": {"enabled": true, "account": "T123"}}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["value"]["connectors"]["slack"]["account"], "T123");
        assert!(v["value"]["connectors"].get("gmail").is_none(), "same-level write replaces");

        // `{}` CLEARS the layer (unset vs empty stays distinguishable).
        let (s, v) = call(
            &app,
            "PUT",
            "/api/scope",
            Some(json!({"level": "worker", "name": "w1", "capability": "connectors", "value": {}})),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["set_here"], false, "an empty object clears the layer");
    }

    #[tokio::test]
    async fn truncated_memory_read_is_flagged_never_writable_back() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("memory")).unwrap();
        std::fs::create_dir_all(home.path().join("sessions")).unwrap();
        std::fs::write(home.path().join("memory/_global.md"), "x".repeat(300)).unwrap();
        let _home = crate::api::settings::test_env::set_home(home.path());
        std::env::set_var("AMUX_SCOPE_TEXT_CAP", "100");
        let st = state();
        let app = app(&st);
        let (_, v) = call(&app, "GET", "/api/scope", None, None).await;
        let m = cap(&v, "memory");
        assert_eq!(m["value"]["bytes"], 300);
        assert_eq!(m["value"]["text"].as_str().unwrap().len(), 100);
        assert_eq!(m["value"]["truncated"], true);
        std::env::remove_var("AMUX_SCOPE_TEXT_CAP");
    }
}
