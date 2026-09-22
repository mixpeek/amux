//! /api/graph — the Map tab's mind-map / org-chart backend (AMUX-2886).
//!
//! **A PORT OF A LIVE CONTRACT, not a new feature** (same situation as
//! crm.rs/AMUX-2929): the SPA's graph client ships in every dashboard —
//! `_graphLoad`, the Obsidian import dialog, drag-to-pin — and
//! `graph_nodes`/`graph_edges` came across in `0001_baseline.sql`, but no
//! route was ever mounted, so the Map tab's graph mode has 404'd since the
//! python retirement while migrations kept maintaining its tables.
//!
//! Contract recovered from `792ce1f^:amux-server.py:73853-73985` + the fleet
//! projection at :64759-64900, shapes matched not redesigned:
//!
//!   GET   /api/graph/fleet                 org chart PROJECTED from live
//!                                          sessions (never stored rows;
//!                                          layout overlays from graph_nodes
//!                                          WHERE graph_id='fleet')
//!   GET   /api/graph/{id}                  {"nodes":[...],"edges":[...]}
//!   POST  /api/graph/{id}/import-vault     parse an Obsidian vault: .md files
//!                                          -> nodes, [[wikilinks]] -> edges;
//!                                          REPLACES the graph's rows
//!   PATCH /api/graph/{id}/nodes/{nid}      update x/y/pinned/label/body/
//!                                          color/folder
//!
//! The fleet route must win over the generic `{id}` read — axum's static-
//! segment precedence gives us that where python needed explicit ordering.
//! Vault paths go through the same `fs::is_path_allowed` guard as every other
//! file API (python used its `_is_path_allowed`; this is the rust port of it).

use super::{internal, AppState};
use crate::db::WriteOutcome;
use axum::extract::{Path as AxPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/fleet", get(fleet_graph))
        .route("/board", get(board_graph))
        .route("/board/verify", get(verify_board_graph))
        .route("/{id}", get(get_graph))
        .route("/{id}/import-vault", post(import_vault))
        .route("/{id}/nodes/{nid}", patch(patch_node))
}

// The board graph is a read-only projection. Mutations continue through the
// attributed, revision-checked board APIs rather than the map editor.
async fn board_graph(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let read = tokio::task::spawn_blocking(move || {
        let conn = store.read()?;
        crate::db::task_graph_store::snapshot(&conn)
    })
    .await;
    match read {
        Ok(Ok(snapshot)) => {
            if !snapshot.verification.valid {
                tracing::warn!(target: "amux::board", verdict = "task_graph_invalid",
                    measured = true, n_considered = snapshot.n_considered,
                    findings = snapshot.verification.findings.len(),
                    revision = snapshot.revision, sha256 = snapshot.sha256,
                    "board graph verification failed; GET /api/graph/board identifies the broken relations");
            }
            Json(snapshot).into_response()
        }
        result => {
            let why = match result {
                Ok(Err(e)) => e.to_string(),
                Err(e) => e.to_string(),
                _ => unreachable!(),
            };
            tracing::warn!(target: "amux::board", verdict = "task_graph_unmeasured", %why,
                "board graph snapshot could not be read");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"measured":false,"n_considered":0,
                "why_unmeasured":why,"error":"board graph unavailable"})),
            )
                .into_response()
        }
    }
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

// Routine preflights must not download every task's description and evidence.
// Measured on 13,381 live tasks: the audit snapshot was 58 MB. This shares the
// periodic verifier and keeps the full snapshot available for explicit export.
async fn verify_board_graph(State(state): State<AppState>) -> Response {
    let store = state.store.clone();
    let read = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let tx = conn.unchecked_transaction()?;
        let revision: u64 = tx.query_row("SELECT rev FROM _amux_rev WHERE id=1", [], |r| r.get(0))?;
        let (n, verification) = crate::db::task_graph_store::verify_graph(&tx)?;
        tx.commit()?;
        if !verification.valid {
            tracing::warn!(target: "amux::board", verdict = "task_graph_invalid", projection = "validation",
                measured = true, n_considered = n, findings = verification.findings.len(), revision,
                "board graph preflight found structural problems");
        }
        Ok(json!({"schema_version":1,"projection":"validation","revision":revision,
            "measured":true,"n_considered":n,"verification":verification}))
    }).await;
    match read {
        Ok(Ok(value)) => Json(value).into_response(),
        result => {
            let why = match result {
                Ok(Err(e)) => e.to_string(),
                Err(e) => e.to_string(),
                _ => unreachable!(),
            };
            tracing::warn!(target: "amux::board", verdict = "task_graph_unmeasured", %why,
                "board graph preflight could not be measured");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"measured":false,"n_considered":0,
                "why_unmeasured":why,"error":"board graph preflight unavailable"})),
            )
                .into_response()
        }
    }
}

fn board_graph_read_only() -> Response {
    tracing::warn!(target: "amux::board", verdict = "task_graph_projection_write_rejected",
        "task graph writes must use the attributed board APIs");
    (StatusCode::CONFLICT, Json(json!({"error":"the board graph is a read-only projection",
        "code":"task_graph_read_only", "remedy":"Use /api/board task, decomposition and artifact mutations"}))).into_response()
}

// ---- GET /api/graph/{id} ----------------------------------------------------

async fn get_graph(State(state): State<AppState>, AxPath(gid): AxPath<String>) -> Response {
    let store = state.store.clone();
    let read = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let conn = store.read()?;
        let mut nodes = vec![];
        let mut st = conn.prepare(
            "SELECT id,label,body,color,folder,source_path,x,y,pinned FROM graph_nodes WHERE graph_id=?",
        )?;
        let mut rows = st.query([&gid])?;
        while let Some(r) = rows.next()? {
            nodes.push(json!({
                "id": r.get::<_, String>(0)?,
                "label": r.get::<_, String>(1)?,
                "body": r.get::<_, String>(2)?,
                "color": r.get::<_, String>(3)?,
                "folder": r.get::<_, String>(4)?,
                "source_path": r.get::<_, String>(5)?,
                "x": r.get::<_, Option<f64>>(6)?,
                "y": r.get::<_, Option<f64>>(7)?,
                "pinned": r.get::<_, i64>(8)?,
            }));
        }
        let mut edges = vec![];
        let mut st = conn
            .prepare("SELECT id,source,target,label FROM graph_edges WHERE graph_id=?")?;
        let mut rows = st.query([&gid])?;
        while let Some(r) = rows.next()? {
            edges.push(json!({
                "id": r.get::<_, String>(0)?,
                "source": r.get::<_, String>(1)?,
                "target": r.get::<_, String>(2)?,
                "label": r.get::<_, String>(3)?,
            }));
        }
        Ok(json!({ "nodes": nodes, "edges": edges }))
    })
    .await;
    match read {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!("join: {e}")),
    }
}

// ---- GET /api/graph/fleet ---------------------------------------------------

/// Eight categorical hues validated against the dark dashboard surface
/// (lightness band, chroma floor, CVD separation, ≥3:1 contrast — the full
/// derivation lives with the python original and is preserved verbatim).
/// Eight is the cap on purpose: a ninth department reuses a slot, which is
/// safe because every node also carries its department label.
const FLEET_PALETTE: [&str; 8] = [
    "#3987e5", "#d95926", "#199e70", "#c98500", "#d55181", "#008300", "#9085e9", "#e66767",
];

/// Leading segment of a session name — `Amux-gtm` → `Amux`. A THROWAWAY
/// heuristic until missions land (python's own words); do not build on it.
fn fleet_dept_of(name: &str) -> String {
    name.trim()
        .split(['-', '_', ' '])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(name.trim())
        .to_string()
}

async fn fleet_graph(State(state): State<AppState>) -> Response {
    // The same serializer the fleet list uses — the projection must agree
    // with the Workers view about who exists and what they are doing.
    let store = state.store.clone();
    let sessions_json = tokio::task::spawn_blocking(move || {
        crate::api::sessions_legacy::legacy_sessions_array(&store)
    })
    .await;
    let sessions: Vec<Value> = match sessions_json {
        Ok(Ok(j)) => serde_json::from_str(&j).unwrap_or_default(),
        Ok(Err(e)) => return crate::api::sessions_legacy::discovery_failure(&e, e.to_string()),
        Err(e) => return internal(anyhow::anyhow!("join: {e}")),
    };

    // Saved layout overlay, keyed by node id (stable across reads).
    let store = state.store.clone();
    let saved: BTreeMap<String, (Option<f64>, Option<f64>, i64)> =
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let conn = store.read()?;
            let mut out = BTreeMap::new();
            let mut st =
                conn.prepare("SELECT id,x,y,pinned FROM graph_nodes WHERE graph_id='fleet'")?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                out.insert(
                    r.get::<_, String>(0)?,
                    (
                        r.get::<_, Option<f64>>(1)?,
                        r.get::<_, Option<f64>>(2)?,
                        r.get::<_, i64>(3)?,
                    ),
                );
            }
            Ok(out)
        })
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();

    // Departments: case-insensitive grouping, label = casing of the first
    // (sorted) session that claimed the segment, so the department does not
    // rename itself when an unrelated session joins or leaves.
    let names: Vec<String> = sessions
        .iter()
        .filter_map(|s| s.get("name").and_then(Value::as_str))
        .filter(|n| !n.is_empty())
        .map(String::from)
        .collect();
    let mut canon: BTreeMap<String, String> = BTreeMap::new();
    {
        let mut sorted = names.clone();
        sorted.sort();
        for n in &sorted {
            let seg = fleet_dept_of(n);
            canon.entry(seg.to_lowercase()).or_insert(seg);
        }
    }
    let dept_of = |name: &str| -> String {
        let seg = fleet_dept_of(name);
        canon.get(&seg.to_lowercase()).cloned().unwrap_or(seg)
    };

    // Colours: palette slots taken IN ORDER over departments sorted
    // case-insensitively — the order the palette's separation guarantees were
    // validated in (python tried hashing twice and both attempts collided in
    // instructive ways; see the original's comment).
    let mut depts: Vec<String> = names
        .iter()
        .map(|n| dept_of(n))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    depts.sort_by_key(|d| d.to_lowercase());
    let colors: BTreeMap<String, &str> = depts
        .iter()
        .enumerate()
        .map(|(i, d)| (d.clone(), FLEET_PALETTE[i % FLEET_PALETTE.len()]))
        .collect();

    let mut nodes = vec![];
    for s in &sessions {
        let name = s.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let nid = format!("sess:{name}");
        let dept = dept_of(name);
        let running = s.get("running").and_then(Value::as_bool).unwrap_or(false);
        let status = {
            let st = s.get("status").and_then(Value::as_str).unwrap_or("").trim();
            if st.is_empty() {
                if running {
                    "running"
                } else {
                    "stopped"
                }
            } else {
                st
            }
            .to_string()
        };
        let task = s
            .get("task_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        let pos = saved.get(&nid);
        nodes.push(json!({
            "id": nid,
            "label": name,
            // What this agent is doing right now, in the field the graph
            // client already renders — not a new one it must learn.
            "body": if task.is_empty() { status.clone() } else { format!("{status} — {task}") },
            "color": colors.get(&dept).copied().unwrap_or(FLEET_PALETTE[0]),
            "folder": dept,   // drives the existing department filter chips
            "source_path": "",
            "x": pos.and_then(|p| p.0),
            "y": pos.and_then(|p| p.1),
            "pinned": pos.map(|p| p.2).unwrap_or(0),
            // Projection-only extras; the default graph has no equivalent.
            "session": name,
            "status": status,
            "running": running,
            "task": task,
            "provider": s.get("provider").and_then(Value::as_str).unwrap_or(""),
        }));
    }

    // color_authority: these colours are DELIBERATE — without it the client
    // falls back to its own folder-colour map and repaints the fleet from a
    // local palette, silently discarding the validated one above.
    Json(json!({ "nodes": nodes, "edges": [], "color_authority": true })).into_response()
}

// ---- POST /api/graph/{id}/import-vault --------------------------------------

fn make_nid(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

async fn import_vault(
    State(state): State<AppState>,
    AxPath(gid): AxPath<String>,
    body: Option<Json<Value>>,
) -> Response {
    if gid == "board" {
        return board_graph_read_only();
    }
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    let vault_path = body
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if vault_path.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "path required"})),
        )
            .into_response();
    }
    let vp = super::fs::expanduser(vault_path);
    let Ok(vp) = vp.canonicalize() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "not a directory"})),
        )
            .into_response();
    };
    if !vp.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "not a directory"})),
        )
            .into_response();
    }
    if !super::fs::is_path_allowed(&vp) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "path not allowed"})),
        )
            .into_response();
    }

    // Parse the vault OFF the writer: walking + reading a big vault is pure
    // filesystem work and must not hold the single write slot while it runs.
    struct Note {
        label: String,
        body: String,
        folder: String,
        links: Vec<String>,
        path: String,
    }
    let parsed = tokio::task::spawn_blocking(move || {
        let mut files: Vec<std::path::PathBuf> = walk_md(&vp);
        files.sort();
        let link_re = regex::Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]+)?\]\]").expect("static regex");
        // BTreeMap keeps python's deterministic ordering (sorted rglob).
        let mut notes: BTreeMap<String, Note> = BTreeMap::new();
        let mut label_to_key: BTreeMap<String, String> = BTreeMap::new();
        for md in files {
            let Ok(rel) = md.strip_prefix(&vp) else {
                continue;
            };
            let parts: Vec<_> = rel.components().collect();
            let folder = if parts.len() > 1 {
                parts[0].as_os_str().to_string_lossy().to_string()
            } else {
                "root".to_string()
            };
            let label = md
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let key = rel.with_extension("").to_string_lossy().to_string();
            let content = std::fs::read_to_string(&md).unwrap_or_default();
            let links: Vec<String> = link_re
                .captures_iter(&content)
                .map(|c| {
                    let l = c.get(1).map(|m| m.as_str()).unwrap_or("");
                    // Resolve `Folder/Note` links by their final segment,
                    // like python.
                    l.rsplit('/').next().unwrap_or(l).to_string()
                })
                .collect();
            // First one wins for link resolution by label (python parity).
            label_to_key.entry(label.clone()).or_insert(key.clone());
            notes.insert(
                key,
                Note {
                    label,
                    body: content,
                    folder,
                    links,
                    path: md.to_string_lossy().to_string(),
                },
            );
        }
        (notes, label_to_key)
    })
    .await;
    let Ok((notes, label_to_key)) = parsed else {
        return internal(anyhow::anyhow!("vault parse task failed"));
    };

    // Known folder colours (python's map, verbatim), gray root, palette
    // rotation for the rest in first-seen order.
    const KNOWN: [(&str, &str); 5] = [
        ("Memories", "#C97B3A"),
        ("Patterns", "#4A6FA5"),
        ("Beliefs", "#A54A4A"),
        ("Behaviors", "#4A9A6F"),
        ("Relationship - Her", "#7A4AA5"),
    ];
    const PALETTE: [&str; 12] = [
        "#C97B3A", "#4A6FA5", "#A54A4A", "#4A9A6F", "#7A4AA5", "#6B8E8A", "#B5651D", "#8B5CF6",
        "#EC4899", "#10B981", "#F59E0B", "#6366F1",
    ];

    let n_notes = notes.len();
    let slot: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let slot_w = slot.clone();
    let gid_w = gid.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let now = now_secs();
            // REPLACE the graph's rows (python parity: import is a full
            // re-sync of the vault, layout included).
            conn.execute("DELETE FROM graph_edges WHERE graph_id=?", [&gid_w])?;
            conn.execute("DELETE FROM graph_nodes WHERE graph_id=?", [&gid_w])?;
            let mut folder_colors: BTreeMap<String, &str> = BTreeMap::new();
            let mut assigned = 0usize;
            for (key, n) in &notes {
                let color = if let Some((_, c)) = KNOWN.iter().find(|(f, _)| f == &n.folder) {
                    *c
                } else if n.folder == "root" {
                    "#888888"
                } else {
                    *folder_colors.entry(n.folder.clone()).or_insert_with(|| {
                        let c = PALETTE[assigned % PALETTE.len()];
                        assigned += 1;
                        c
                    })
                };
                conn.execute(
                    "INSERT INTO graph_nodes (id,graph_id,label,body,color,folder,source_path,created,updated) \
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?8)",
                    rusqlite::params![make_nid(key), &gid_w, &n.label, &n.body, color, &n.folder, &n.path, now],
                )?;
            }
            let mut eidx = 0i64;
            let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
            for (key, n) in &notes {
                let src = make_nid(key);
                for link in &n.links {
                    let Some(tgt_key) = label_to_key.get(link) else { continue };
                    let tgt = make_nid(tgt_key);
                    if tgt == src || !seen.insert((src.clone(), tgt.clone())) {
                        continue;
                    }
                    eidx += 1;
                    conn.execute(
                        "INSERT OR IGNORE INTO graph_edges (id,graph_id,source,target,created) VALUES (?1,?2,?3,?4,?5)",
                        rusqlite::params![format!("e-{eidx}"), &gid_w, src, tgt, now],
                    )?;
                }
            }
            *slot_w.lock().expect("slot") = eidx as usize;
            Ok(WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    match write {
        Ok(_) => {
            let edges = *slot.lock().expect("slot");
            Json(json!({ "ok": true, "nodes": n_notes, "edges": edges })).into_response()
        }
        Err(e) => internal(e),
    }
}

/// Recursive *.md walk. Hand-rolled (no walkdir dependency); symlinked dirs
/// are not followed — a vault symlink pointing outside the allowed root would
/// otherwise bypass the path guard that admitted the vault itself.
fn walk_md(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let is_symlink = e.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            if p.is_dir() && !is_symlink {
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("md") {
                out.push(p);
            }
        }
    }
    out
}

// ---- PATCH /api/graph/{id}/nodes/{nid} --------------------------------------

async fn patch_node(
    State(state): State<AppState>,
    AxPath((gid, nid)): AxPath<(String, String)>,
    body: Option<Json<Value>>,
) -> Response {
    if gid == "board" {
        return board_graph_read_only();
    }
    let body = body.map(|Json(v)| v).unwrap_or(Value::Null);
    // Whitelisted fields, python parity. Values pass through as their JSON
    // types (x/y numbers, pinned 0/1, strings for the rest).
    let mut sets: Vec<String> = vec![];
    let mut vals: Vec<rusqlite::types::Value> = vec![];
    for k in ["x", "y", "pinned", "label", "body", "color", "folder"] {
        if let Some(v) = body.get(k) {
            sets.push(format!("{k}=?"));
            vals.push(match v {
                Value::Number(n) if n.is_i64() => {
                    rusqlite::types::Value::Integer(n.as_i64().unwrap_or(0))
                }
                Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0)),
                Value::Bool(b) => rusqlite::types::Value::Integer(*b as i64),
                Value::Null => rusqlite::types::Value::Null,
                other => rusqlite::types::Value::Text(
                    other
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| other.to_string()),
                ),
            });
        }
    }
    if sets.is_empty() {
        // Python answered {"ok": true} to an empty patch; kept for parity.
        return Json(json!({ "ok": true })).into_response();
    }
    let write = state
        .store
        .write_async(move |conn| {
            sets.push("updated=?".into());
            vals.push(rusqlite::types::Value::Integer(now_secs()));
            vals.push(rusqlite::types::Value::Text(nid));
            vals.push(rusqlite::types::Value::Text(gid));
            let sql = format!(
                "UPDATE graph_nodes SET {} WHERE id=? AND graph_id=?",
                sets.join(",")
            );
            conn.execute(&sql, rusqlite::params_from_iter(vals))?;
            Ok(WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Node ids must match python's `re.sub(r"[^a-zA-Z0-9_-]", "_", key).lower()`
    /// exactly — the SPA stores layout keyed by these ids, so a divergence
    /// orphans every saved position on the first rust import.
    #[test]
    fn nid_matches_python_sanitizer() {
        assert_eq!(make_nid("Patterns/Gamma"), "patterns_gamma");
        assert_eq!(make_nid("A b.c-D_e"), "a_b_c-d_e");
        assert_eq!(make_nid("Ünïcode"), "_n_code");
    }

    /// `Amux-gtm` → `Amux`; a name with no separator is its own department.
    #[test]
    fn dept_is_leading_segment() {
        assert_eq!(fleet_dept_of("amux-gtm"), "amux");
        assert_eq!(fleet_dept_of("mixpeek_studio x"), "mixpeek");
        assert_eq!(fleet_dept_of("solo"), "solo");
    }
}
