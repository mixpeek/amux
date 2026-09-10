//! /api/skills + /api/slash-commands — the Skills tab's data (AMUX-2586
//! fix #6, ported from amux-server.py:66635-66700 + 11591-11676).
//!
//! Skills live in the shared `skills` TABLE (name, content); the list
//! parses YAML-ish frontmatter for `description:` / `argument-hint:`
//! exactly as Python does (naive line scan, not a YAML parser — parity
//! over cleverness). Slash-commands are the builtin table plus
//! ~/.claude/commands/*.md and ./.claude/commands/*.md, deduped by name.

use super::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};

pub fn routes() -> Router<AppState> {
    // POST/DELETE on {name} were never ported (AMUX-2623 census). The Skills
    // tab has an editor with Save and Delete wired to exactly these two verbs
    // (app.js:23784, 23799) against a GET-only route, so Save toasted "Save
    // failed" and Delete did nothing — a dead editor, shipped and unnoticed
    // because a 405 in a fetch is silent unless someone reads the toast.
    Router::new()
        .route("/", get(list_skills))
        .route("/{name}", get(get_skill).post(save_skill).delete(delete_skill))
}

/// Shared name rule — one predicate, three handlers. `get_skill` had it
/// inline; a second copy in each writer is how the read and the write start
/// disagreeing about what a valid name is.
fn bad_name(name: &str) -> bool {
    name.is_empty() || name.contains('/') || name.contains("..")
}

pub fn slash_routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_slash_commands))
        .route("/{name}", get(get_slash_command))
}

/// Python's frontmatter scan (py:66641-66651): only a leading `---` block,
/// only `description:` / `argument-hint:` lines, first colon splits.
fn frontmatter_fields(text: &str) -> (String, String) {
    let (mut desc, mut hint) = (String::new(), String::new());
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("---") {
            for line in rest[..end].lines() {
                if let Some(v) = line.strip_prefix("description:") {
                    desc = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("argument-hint:") {
                    hint = v.trim().to_string();
                }
            }
        }
    }
    (desc, hint)
}

async fn list_skills(State(state): State<AppState>) -> Response {
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    // Tolerate the table being absent (a fresh Rust-only AMUX_HOME): an
    // empty list is the truthful answer, not a 500.
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare("SELECT name, content FROM skills ORDER BY name") {
        if let Ok(rows) =
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for (name, content) in rows.flatten() {
                let (description, hint) = frontmatter_fields(&content);
                out.push(json!({ "name": name, "description": description, "hint": hint }));
            }
        }
    }
    Json(serde_json::Value::Array(out)).into_response()
}

async fn get_skill(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if name.is_empty() || name.contains('/') {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid name" })))
            .into_response();
    }
    let conn = match state.store.read() {
        Ok(c) => c,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    let row: Option<String> = conn
        .query_row("SELECT content FROM skills WHERE name=?1", [&name], |r| r.get(0))
        .ok();
    match row {
        Some(content) => Json(json!({ "name": name, "content": content })).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response(),
    }
}

/// Python's `_BUILTIN_SLASH_COMMANDS` (py:11591), verbatim.
const BUILTIN_SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/add-dir", "Add a working directory"),
    ("/agents", "Manage agent configurations"),
    ("/batch", "Orchestrate large-scale changes in parallel"),
    ("/clear", "Clear conversation history"),
    ("/color", "Set prompt bar color"),
    ("/compact", "Compact conversation history"),
    ("/config", "Open config panel"),
    ("/context", "Visualize context usage"),
    ("/copy", "Copy last response to clipboard"),
    ("/cost", "Show token usage and cost"),
    ("/debug", "Enable debug logging"),
    ("/doctor", "Check installation health"),
    ("/effort", "Set model effort level"),
    ("/export", "Export conversation as text"),
    ("/extra-usage", "Configure extra usage for rate limits"),
    ("/fast", "Toggle fast mode"),
    ("/feedback", "Submit feedback or report a bug"),
    ("/focus", "Toggle focus view"),
    ("/help", "Show available commands"),
    ("/hooks", "View hook configurations"),
    ("/ide", "Manage IDE integrations"),
    ("/init", "Initialize project CLAUDE.md"),
    ("/login", "Switch account or log in"),
    ("/logout", "Log out of current account"),
    ("/loop", "Run a prompt repeatedly"),
    ("/mcp", "Manage MCP servers"),
    ("/memory", "Edit CLAUDE.md memory"),
    ("/model", "Switch model"),
    ("/permissions", "View/manage permissions"),
    ("/plan", "Enter plan mode"),
    ("/plugin", "Manage plugins"),
    ("/recap", "Summarize current session"),
    ("/release-notes", "View changelog"),
    ("/remote-control", "Enable remote control from claude.ai"),
    ("/rename", "Rename current session"),
    ("/resume", "Resume a conversation"),
    ("/review", "Review a pull request"),
    ("/rewind", "Rewind conversation to a checkpoint"),
    ("/sandbox", "Toggle sandbox mode"),
    ("/schedule", "Create or manage routines"),
    ("/security-review", "Analyze changes for security issues"),
    ("/simplify", "Review code for reuse and quality"),
    ("/skills", "List available skills"),
    ("/stats", "Visualize usage and session history"),
    ("/status", "Show session status"),
    ("/statusline", "Configure status line"),
    ("/tasks", "List and manage background tasks"),
    ("/terminal-setup", "Set up terminal integration"),
    ("/theme", "Change color theme"),
    ("/ultraplan", "Draft a plan with cloud review"),
    ("/ultrareview", "Deep multi-agent code review"),
    ("/usage", "Show plan usage and rate limits"),
    ("/vim", "Edit prompt in Vim"),
    ("/voice", "Toggle voice dictation"),
];

fn command_dirs() -> Vec<std::path::PathBuf> {
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    vec![
        home.join(".claude").join("commands"),
        std::path::PathBuf::from(".").join(".claude").join("commands"),
    ]
}

async fn list_slash_commands() -> Response {
    let mut cmds: Vec<serde_json::Value> = BUILTIN_SLASH_COMMANDS
        .iter()
        .map(|(c, d)| json!({ "cmd": c, "desc": d }))
        .collect();
    let mut seen: std::collections::BTreeSet<String> =
        BUILTIN_SLASH_COMMANDS.iter().map(|(c, _)| c.to_string()).collect();
    for dir in command_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("md"))
            .collect();
        files.sort();
        for f in files {
            let Some(stem) = f.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let name = format!("/{stem}");
            // AMUX-4372: do not reintroduce Claude's diff sidebar through a
            // custom command file after removing its builtin suggestion.
            if name == "/diff" || !seen.insert(name.clone()) {
                continue;
            }
            // Python reads only the frontmatter `description:` here (first
            // match wins, py:11668-11672).
            let desc = std::fs::read_to_string(&f)
                .ok()
                .map(|t| frontmatter_fields(&t).0)
                .unwrap_or_default();
            cmds.push(json!({ "cmd": name, "desc": desc }));
        }
    }
    Json(serde_json::Value::Array(cmds)).into_response()
}

async fn get_slash_command(Path(name): Path<String>) -> Response {
    let name = name.trim_start_matches('/').to_string();
    if name.is_empty() || name.contains('/') {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid" }))).into_response();
    }
    for dir in command_dirs() {
        let f = dir.join(format!("{name}.md"));
        if let Ok(content) = std::fs::read_to_string(&f) {
            return Json(json!({ "name": name, "content": content, "source": "file" }))
                .into_response();
        }
    }
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn slash_commands_do_not_offer_claude_diff() {
        let response = list_slash_commands().await;
        let bytes = axum::body::to_bytes(response.into_body(), 1_000_000).await.unwrap();
        let commands: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let commands = commands.as_array().unwrap();
        assert!(!commands.iter().any(|c| c["cmd"] == "/diff"));
        assert!(commands.iter().any(|c| c["cmd"] == "/context"));
    }

    #[test]
    fn frontmatter_parses_description_and_hint() {
        let text = "---\ndescription: Do the thing\nargument-hint: <target>\n---\nbody";
        let (d, h) = frontmatter_fields(text);
        assert_eq!(d, "Do the thing");
        assert_eq!(h, "<target>");
    }

    #[test]
    fn frontmatter_absent_yields_empties() {
        assert_eq!(frontmatter_fields("no frontmatter here"), (String::new(), String::new()));
        // An unterminated block parses nothing (Python: find("---", 3) < 0).
        assert_eq!(frontmatter_fields("---\ndescription: x"), (String::new(), String::new()));
    }
}

/// Upsert. Python stored skills in the shared `skills` table keyed by name, so
/// saving an existing skill REPLACES it — which is what the editor's Save
/// means, and why this is not an INSERT that 409s on a name you already have.
async fn save_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    if bad_name(&name) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid name" }))).into_response();
    }
    let content = body.get("content").and_then(Value::as_str).unwrap_or("").to_string();
    // An empty body would silently blank a skill the user can no longer see the
    // text of — the same shape as the board's blanked-desc hazard. Refuse it.
    if content.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "content required — use DELETE to remove a skill" })),
        )
            .into_response();
    }
    let n2 = name.clone();
    let write = state
        .store
        .write_async(move |conn| {
            conn.execute(
                "CREATE TABLE IF NOT EXISTS skills (name TEXT PRIMARY KEY, content TEXT NOT NULL)",
                [],
            )?;
            conn.execute(
                "INSERT INTO skills (name, content) VALUES (?1, ?2) \
                 ON CONFLICT(name) DO UPDATE SET content=excluded.content",
                rusqlite::params![n2, content],
            )?;
            Ok(crate::db::WriteOutcome { applied: true, events: vec![] })
        })
        .await;
    match write {
        Ok(_) => Json(json!({ "ok": true, "name": name })).into_response(),
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))
                .into_response()
        }
    }
}

/// Delete. Reports whether a row actually went — `{"deleted": false}` for a
/// name that was not there, rather than a cheerful ok that cannot be
/// distinguished from a real removal.
async fn delete_skill(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    if bad_name(&name) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid name" }))).into_response();
    }
    let n2 = name.clone();
    let slot = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let slot_w = slot.clone();
    let write = state
        .store
        .write_async(move |conn| {
            let n = conn.execute("DELETE FROM skills WHERE name=?1", rusqlite::params![n2])?;
            *slot_w.lock().expect("slot") = n;
            Ok(crate::db::WriteOutcome { applied: n > 0, events: vec![] })
        })
        .await;
    match write {
        Ok(_) => {
            let n = *slot.lock().expect("slot");
            Json(json!({ "ok": true, "name": name, "deleted": n > 0 })).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e.to_string() })))
                .into_response()
        }
    }
}

#[cfg(test)]
mod write_tests {
    use super::*;

    /// The name rule is shared by read and both writes. A writer with its own
    /// copy is how a path that GET rejects becomes one POST accepts.
    #[test]
    fn the_name_rule_rejects_what_would_escape_the_key_space() {
        assert!(bad_name(""), "empty");
        assert!(bad_name("a/b"), "a slash would address another route");
        assert!(bad_name(".."), "traversal-shaped names never reach the table");
        assert!(bad_name("../etc/passwd"));
        assert!(!bad_name("my-skill"));
        assert!(!bad_name("my_skill.v2"));
    }
}
