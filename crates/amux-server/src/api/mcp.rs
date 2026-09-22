//! /api/mcp — the MCP registry the Skills-adjacent MCP tab reads and edits.
//!
//! NEVER PORTED, and the tab is visible by default, so clicking it produced
//! "Could not read the registry." (app.js:24254). Found by the route census
//! (AMUX-2623/AMUX-2871).
//!
//! Worth more than one dead tab: ethos rule 1's own worked example is the MCP
//! incident — six configured servers reached 0 of 101 sessions because nothing
//! surfaced whether they were wired. This endpoint is the surface that would
//! have shown it, and `ready` is deliberately computed from whether the
//! credentials a server references actually EXIST, not from whether it is
//! listed.
//!
//! THE REGISTRY IS ~/.amux/mcp.json, not the repo's mcp.json. That is the file
//! session_verbs.rs:3705 passes to `claude --mcp-config`, so it is the one that
//! decides what a worker actually gets; the repo copy is a seed. Writing the
//! repo file from a dashboard button would also dirty a shared checkout, which
//! this codebase has paid for before.

use super::AppState;
use axum::extract::Path;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Map, Value};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/", get(list_mcp).post(import_mcp))
        .route("/{name}", axum::routing::delete(delete_mcp))
}

fn registry_path() -> std::path::PathBuf {
    crate::api::session_verbs::home().join("mcp.json")
}
fn env_path() -> std::path::PathBuf {
    crate::api::session_verbs::home().join("amux.env")
}

fn read_registry() -> Map<String, Value> {
    std::fs::read_to_string(registry_path())
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("mcpServers").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

/// Every `${VAR}` the server config references, anywhere in it.
///
/// Scanning the SERIALISED config rather than known fields on purpose: a
/// credential can sit in `headers`, `env`, `args` or a url, and a field list
/// would go stale the first time a server used a shape nobody predicted —
/// silently reporting `ready` for a server whose key is missing.
fn referenced_vars(cfg: &Value) -> Vec<String> {
    let text = cfg.to_string();
    let mut out = Vec::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == '$' && bytes[i + 1] == '{' {
            let mut j = i + 2;
            let mut name = String::new();
            while j < bytes.len() && bytes[j] != '}' {
                name.push(bytes[j]);
                j += 1;
            }
            // An env var name is [A-Za-z0-9_]. Without that rule an UNCLOSED
            // `${VAR` still matched, because this scans the SERIALISED config
            // and JSON supplies a later `}` — capturing `VAR"` and reporting a
            // credential that does not exist, which marks a working server
            // "needs credentials" forever. Caught by this module's own test.
            let valid =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if j < bytes.len() && valid && !out.contains(&name) {
                out.push(name);
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out.sort();
    out
}

fn env_keys() -> Vec<String> {
    let mut v: Vec<String> = crate::config::parse_env_file(&env_path())
        .into_keys()
        .collect();
    v.sort();
    v
}

async fn list_mcp() -> Response {
    let servers = read_registry();
    let keys = env_keys();
    let exists = env_path().exists();
    let out: Vec<Value> = servers
        .iter()
        .map(|(name, cfg)| {
            let missing: Vec<String> = referenced_vars(cfg)
                .into_iter()
                .filter(|v| !keys.contains(v))
                .collect();
            let kind = cfg
                .get("type")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| {
                    if cfg.get("url").is_some() {
                        "http".into()
                    } else {
                        "stdio".into()
                    }
                });
            json!({
                "name": name,
                "type": kind,
                // READY MEANS ITS CREDENTIALS EXIST, not that it is listed.
                // A registry that calls every entry ready is the decoration
                // ethos rule 1 warns about.
                "ready": missing.is_empty(),
                "missing": missing,
                "url": cfg.get("url").and_then(Value::as_str).unwrap_or(""),
                "command": cfg.get("command").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect();
    Json(json!({
        "servers": out,
        "path": registry_path().to_string_lossy(),
        "env_file": env_path().to_string_lossy(),
        "env_exists": exists,
        "env_keys": keys,
    }))
    .into_response()
}

fn write_registry(servers: &Map<String, Value>) -> std::io::Result<()> {
    let body = json!({ "mcpServers": servers });
    let p = registry_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write-then-rename: a half-written registry is the file every worker is
    // launched with, so a torn write would break session spawn fleet-wide.
    let tmp = p.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    )?;
    std::fs::rename(&tmp, &p)
}

/// `{bulk: {...}}` — either `{"mcpServers":{...}}` or a bare `{"name":{...}}`,
/// because that is what a server's README actually puts on the clipboard
/// (app.js says so where it builds the dialog).
async fn import_mcp(Json(body): Json<Value>) -> Response {
    let bulk = body.get("bulk").cloned().unwrap_or(Value::Null);
    let incoming = bulk
        .get("mcpServers")
        .and_then(Value::as_object)
        .cloned()
        .or_else(|| bulk.as_object().cloned());
    let Some(incoming) = incoming.filter(|m| !m.is_empty()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "expected {\"mcpServers\":{…}} or {\"name\":{…}}" })),
        )
            .into_response();
    };
    if incoming.values().any(|v| !v.is_object()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "each server must be an object" })),
        )
            .into_response();
    }
    let mut servers = read_registry();
    let mut added = Vec::new();
    for (k, v) in incoming {
        added.push(k.clone());
        servers.insert(k, v);
    }
    match write_registry(&servers) {
        Ok(()) => Json(json!({ "ok": true, "imported": added })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn delete_mcp(Path(name): Path<String>) -> Response {
    if name.is_empty() || name.contains('/') {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid name" })),
        )
            .into_response();
    }
    let mut servers = read_registry();
    let removed = servers.remove(&name).is_some();
    if !removed {
        // Say so rather than return a cheerful ok — a delete that reports
        // success for a name that was never there is indistinguishable from one
        // that worked.
        return Json(json!({ "ok": true, "removed": false, "name": name })).into_response();
    }
    match write_registry(&servers) {
        Ok(()) => Json(json!({ "ok": true, "removed": true, "name": name })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referenced_vars_finds_credentials_wherever_they_sit() {
        let cfg = json!({
            "url": "https://x/${TENANT}/mcp",
            "headers": { "Authorization": "Bearer ${MIXPEEK_API_KEY}" },
            "env": { "TOKEN": "${GH_TOKEN}" }
        });
        assert_eq!(
            referenced_vars(&cfg),
            vec!["GH_TOKEN", "MIXPEEK_API_KEY", "TENANT"]
        );
    }

    #[test]
    fn a_config_with_no_placeholders_needs_nothing() {
        assert!(referenced_vars(&json!({"command": "npx", "args": ["-y", "srv"]})).is_empty());
    }

    /// A malformed placeholder must not be reported as a required credential —
    /// that would mark a working server "needs credentials" forever.
    #[test]
    fn an_unclosed_placeholder_is_not_a_variable() {
        assert!(referenced_vars(&json!({"url": "https://x/${UNCLOSED"})).is_empty());
    }
}
