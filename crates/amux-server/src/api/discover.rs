//! Discovery: everything amux can do, for an agent pointed at it (owner,
//! 2026-11-27: "make sure all amux capabilities/apis/featuers are
//! discoverable via api and mcp. We should be able to point an agent to amux
//! and it should be able to figure everything out and how to use amux
//! appropriately and troubleshoot").
//!
//! The catalog is the code's own words: build.rs reads every API module's
//! `//!` doc and the routes it defines, and the route table
//! (`request_log::ROUTE_TABLE`) gives every route and its methods. Nothing here
//! is a second description to keep in step (A10); a new module or route shows
//! up on the next build. The one hand-written page is `docs/agents.md` (what
//! amux is, auth, conventions, troubleshooting), served as the guide.
//!
//!   GET  /llms.txt                     the guide and the family index, as markdown (no data in it, so no login)
//!   GET  /api/discover                 the guide, every family with a summary and its route count, coverage
//!   GET  /api/discover/{family}        one family: its routes and methods, and the docs of the modules that serve them
//!   GET  /api/discover/search?q=       families and modules whose docs or paths match the words
//!   POST /api/mcp-server               the same as an MCP server (JSON-RPC over HTTP): tools discover,
//!                                      search, call, troubleshoot, events; resources amux://guide and
//!                                      amux://family/<name>
//!
//! The MCP `call` tool reaches any /api route on loopback with the CALLER's own
//! identity headers (bearer, worker header, cookie), so every gate, owner-only
//! refusal and attribution applies exactly as if the caller had called the
//! route itself; it never acts as the owner on a worker's behalf.
//!
//! Coverage is measured, not claimed: `/api/discover` says how many routes map
//! to a documented module and lists the ones that do not.

use super::AppState;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

mod catalog {
    include!(concat!(env!("OUT_DIR"), "/api_catalog.rs"));
}

pub const GUIDE: &str = include_str!("../../../../docs/agents.md");

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/discover", get(index_route))
        .route("/api/discover/search", get(search_route))
        .route("/api/discover/{family}", get(family_route))
        .route("/api/mcp-server", post(mcp_route))
}

pub fn public_routes() -> Router<AppState> {
    Router::new().route("/llms.txt", get(llms_route))
}

/// The family a route belongs to: `/api/<family>/...`, else its first segment.
pub(crate) fn family_of(path: &str) -> String {
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if segs.first() == Some(&"api") && segs.len() > 1 {
        segs[1].to_string()
    } else {
        segs.first().map(|s| s.to_string()).unwrap_or_default()
    }
}

fn norm(p: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in p.chars() {
        match c {
            '{' => { inside = true; out.push_str("{}"); }
            '}' => inside = false,
            _ if inside => {}
            _ => out.push(c),
        }
    }
    let t = out.trim_end_matches('/');
    if t.is_empty() { "/".into() } else { t.to_string() }
}

fn doc_of(module: &str) -> Option<&'static str> {
    catalog::MODULE_DOCS.iter().find(|(m, _)| *m == module).map(|(_, d)| *d)
}

/// The first paragraph of a doc, on one line and at most ~240 characters.
fn summary_of(doc: &str) -> String {
    let para: Vec<&str> = doc.lines().take_while(|l| !l.trim().is_empty()).map(str::trim).collect();
    let s = para.join(" ");
    if s.chars().count() > 240 { format!("{}...", s.chars().take(237).collect::<String>()) } else { s }
}

/// One route of the table with the modules that serve it.
#[derive(Clone)]
struct Route {
    path: &'static str,
    methods: Vec<&'static str>,
    modules: Vec<String>,
}

/// Every route, grouped by family, each with its modules: the module that
/// defines it, else the modules of the family's other routes, else a module
/// named for the family.
fn families() -> BTreeMap<String, Vec<Route>> {
    let mut by_path: HashMap<&str, Vec<&str>> = HashMap::new();
    for (p, m) in catalog::ROUTE_MODULES {
        by_path.entry(p).or_default().push(m);
    }
    let mut out: BTreeMap<String, Vec<Route>> = BTreeMap::new();
    for r in super::request_log::ROUTE_TABLE {
        let n = norm(r.path);
        let mods: Vec<String> = by_path.get(n.as_str()).map(|v| v.iter().map(|s| s.to_string()).collect()).unwrap_or_default();
        out.entry(family_of(r.path)).or_default().push(Route { path: r.path, methods: r.methods.to_vec(), modules: mods });
    }
    for routes in out.values_mut() {
        let fam_mods: BTreeSet<String> = routes.iter().flat_map(|r| r.modules.clone()).collect();
        for r in routes.iter_mut() {
            if r.modules.is_empty() {
                r.modules = fam_mods.iter().cloned().collect();
            }
        }
    }
    for (fam, routes) in out.iter_mut() {
        let guess = fam.replace('-', "_");
        if routes.iter().all(|r| r.modules.is_empty()) && doc_of(&guess).is_some() {
            for r in routes.iter_mut() {
                r.modules = vec![guess.clone()];
            }
        }
    }
    out
}

fn family_summary(routes: &[Route]) -> (String, Vec<String>) {
    let mods: BTreeSet<String> = routes.iter().flat_map(|r| r.modules.clone()).collect();
    let mods: Vec<String> = mods.into_iter().collect();
    let summary = mods.iter().filter_map(|m| doc_of(m)).map(summary_of).next().unwrap_or_default();
    (summary, mods)
}

/// The index: the guide, every family with its summary, and how much of the
/// route table maps to a documented module.
pub(crate) fn index() -> Value {
    let fams = families();
    let mut list = Vec::new();
    let (mut total, mut documented) = (0usize, 0usize);
    let mut undocumented = Vec::new();
    for (fam, routes) in &fams {
        let (summary, mods) = family_summary(routes);
        for r in routes {
            total += 1;
            if r.modules.iter().any(|m| doc_of(m).is_some()) {
                documented += 1;
            } else {
                undocumented.push(r.path);
            }
        }
        list.push(json!({ "family": fam, "summary": summary, "routes": routes.len(), "modules": mods, "detail": format!("/api/discover/{fam}") }));
    }
    json!({
        "about": "amux runs a fleet of coding-agent workers on one machine and gives them and you an HTTP API over everything they share. Read `guide` first; then GET /api/discover/<family> for any family below, or GET /api/discover/search?q=<words>.",
        "guide": GUIDE,
        "families": list,
        "coverage": { "routes": total, "documented": documented, "undocumented": undocumented, "modules_with_docs": catalog::MODULE_DOCS.len() },
        "mcp": { "endpoint": "/api/mcp-server", "transport": "streamable-http (JSON-RPC over POST)", "tools": ["discover", "search", "call", "troubleshoot", "events"] },
        "troubleshoot": ["GET /health", "GET /api/pull", "GET /api/health/invariants", "GET /api/logs/analyze?since_h=1", "GET /api/debug/routes"],
        "events": "GET /api/subscriptions/catalog",
        "measured": true, "n_considered": total,
    })
}

pub(crate) fn family(name: &str) -> Option<Value> {
    let fams = families();
    let routes = fams.get(name)?;
    let (summary, mods) = family_summary(routes);
    let docs: Vec<Value> = mods.iter().filter_map(|m| doc_of(m).map(|d| json!({ "module": m, "doc": d }))).collect();
    Some(json!({
        "family": name, "summary": summary,
        "routes": routes.iter().map(|r| json!({ "path": r.path, "methods": r.methods, "modules": r.modules })).collect::<Vec<_>>(),
        "modules": docs,
        "measured": true, "n_considered": routes.len(),
    }))
}

/// Families and modules whose docs or route paths hold the words, best first,
/// each with the lines that matched.
pub(crate) fn search(q: &str) -> Value {
    let words: Vec<String> = q.split(|c: char| !c.is_alphanumeric() && c != '_' && c != '.' && c != '-').filter(|w| w.len() > 1).map(str::to_lowercase).collect();
    if words.is_empty() {
        return json!({ "query": q, "results": [], "measured": false, "n_considered": 0, "why_unmeasured": "give q some words" });
    }
    let fams = families();
    let mut results: Vec<(usize, Value)> = Vec::new();
    for (fam, routes) in &fams {
        let (summary, mods) = family_summary(routes);
        let mut score = 0usize;
        let mut lines: Vec<String> = Vec::new();
        let mut paths: Vec<&str> = Vec::new();
        for r in routes {
            let p = r.path.to_lowercase();
            if words.iter().any(|w| p.contains(w.as_str())) {
                score += 3;
                paths.push(r.path);
            }
        }
        for m in &mods {
            if let Some(d) = doc_of(m) {
                for l in d.lines() {
                    let ll = l.to_lowercase();
                    let hits = words.iter().filter(|w| ll.contains(w.as_str())).count();
                    if hits > 0 {
                        score += hits * hits;
                        if lines.len() < 4 {
                            lines.push(format!("{m}: {}", l.trim()));
                        }
                    }
                }
            }
        }
        if fam.contains(words[0].as_str()) {
            score += 10;
        }
        if score > 0 {
            results.push((score, json!({ "family": fam, "summary": summary, "matching_routes": paths.into_iter().take(8).collect::<Vec<_>>(), "matching_lines": lines, "detail": format!("/api/discover/{fam}") })));
        }
    }
    results.sort_by_key(|r| std::cmp::Reverse(r.0));
    let n = fams.len();
    json!({ "query": q, "results": results.into_iter().take(12).map(|(_, v)| v).collect::<Vec<_>>(), "measured": true, "n_considered": n })
}

/// The guide and the family index as markdown: what an agent reads first.
pub(crate) fn llms_txt() -> String {
    let mut s = String::from(GUIDE);
    s.push_str("\n## Every family\n\nEach links to its routes and the docs of the code that serves it (`GET /api/discover/<family>`).\n\n");
    for (fam, routes) in families() {
        let (summary, _) = family_summary(&routes);
        s.push_str(&format!("- `{fam}` ({} route{}): {}\n", routes.len(), if routes.len() == 1 { "" } else { "s" }, if summary.is_empty() { "no module docs".into() } else { summary }));
    }
    s
}

async fn index_route() -> Response {
    Json(index()).into_response()
}

async fn family_route(Path(name): Path<String>) -> Response {
    match family(&name) {
        Some(v) => Json(v).into_response(),
        None => {
            let close: Vec<String> = families().keys().filter(|f| f.contains(&name) || name.contains(f.as_str())).cloned().collect();
            (StatusCode::NOT_FOUND, Json(json!({ "error": format!("no family {name}"), "code": "discover_unknown_family",
                "close": close, "next": "GET /api/discover for every family, or /api/discover/search?q=<words>" }))).into_response()
        }
    }
}

async fn search_route(Query(q): Query<HashMap<String, String>>) -> Response {
    Json(search(q.get("q").map(String::as_str).unwrap_or(""))).into_response()
}

async fn llms_route() -> Response {
    ([(axum::http::header::CONTENT_TYPE, "text/markdown; charset=utf-8")], llms_txt()).into_response()
}

// ---- MCP ----------------------------------------------------------------------------------

const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn text_result(v: &Value, is_error: bool) -> Value {
    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(v).unwrap_or_default() }], "structuredContent": v, "isError": is_error })
}

pub(crate) fn tool_defs() -> Value {
    let ro = json!({ "readOnlyHint": true, "destructiveHint": false, "openWorldHint": false });
    json!([
        { "name": "discover", "title": "What amux can do",
          "description": "With no family: the guide (what amux is, auth, conventions, troubleshooting) and every API family with a summary. With a family (for example crm, board, workers, schedules): its routes, methods and the docs of the code that serves them, including request bodies and refusal codes.",
          "inputSchema": { "type": "object", "properties": { "family": { "type": "string" } } }, "annotations": ro },
        { "name": "search", "title": "Find where something is",
          "description": "Families and modules whose docs or routes match the words, best first, with the matching lines. Use it for a task (\"send email\", \"schedule a shell command\") or a refusal code you got back.",
          "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } }, "required": ["q"] }, "annotations": ro },
        { "name": "call", "title": "Call the amux API",
          "description": "Call any amux route (a path starting /api/ or /health) as yourself: your identity headers go with it, so every gate, owner-only refusal and attribution applies. Returns the status and the JSON body. Read the family's docs first with discover.",
          "inputSchema": { "type": "object", "properties": {
              "method": { "type": "string", "enum": ["GET", "POST", "PATCH", "PUT", "DELETE"] },
              "path": { "type": "string", "description": "for example /api/board?all=1" },
              "body": { "type": "object" } }, "required": ["method", "path"] },
          "annotations": { "readOnlyHint": false, "destructiveHint": true, "openWorldHint": false } },
        { "name": "troubleshoot", "title": "What is wrong right now",
          "description": "The server's health and commit, whether a deploy is in progress, the failing invariants, and the error groups in the logs over since_h hours (default 1), together, with what to read next.",
          "inputSchema": { "type": "object", "properties": { "since_h": { "type": "number" } } }, "annotations": ro },
        { "name": "events", "title": "Events you can subscribe to",
          "description": "Every event type amux emits, with its fields and an example. Subscribe with POST /api/subscriptions instead of polling.",
          "inputSchema": { "type": "object", "properties": {} }, "annotations": ro },
    ])
}

fn resources() -> Value {
    let mut list = vec![json!({ "uri": "amux://guide", "name": "guide", "title": "amux for agents", "mimeType": "text/markdown" })];
    for (fam, routes) in families() {
        let (summary, _) = family_summary(&routes);
        list.push(json!({ "uri": format!("amux://family/{fam}"), "name": fam, "description": summary, "mimeType": "application/json" }));
    }
    json!({ "resources": list })
}

/// The caller's identity, carried on the loopback hop so the call is theirs.
fn identity(headers: &HeaderMap) -> Vec<(String, String)> {
    ["authorization", "x-amux-session", "x-amux-worker", "cookie"]
        .iter()
        .filter_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()).map(|v| (h.to_string(), v.to_string())))
        .collect()
}

async fn local(ident: &[(String, String)], method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let client = reqwest::Client::builder().danger_accept_invalid_certs(true).timeout(std::time::Duration::from_secs(60)).build().unwrap_or_default();
    let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let url = format!("https://127.0.0.1:{}{path}", crate::legacy_port::canonical_port());
    let mut rb = client.request(m, &url).header("x-amux-client", "amux-mcp-server");
    for (k, v) in ident {
        rb = rb.header(k.as_str(), v.as_str());
    }
    if let Some(b) = body {
        rb = rb.json(&b);
    }
    match rb.send().await {
        Ok(r) => {
            let code = r.status().as_u16();
            let text = r.text().await.unwrap_or_default();
            (code, serde_json::from_str(&text).unwrap_or(json!({ "body": text.chars().take(4000).collect::<String>() })))
        }
        Err(e) => (0, json!({ "error": format!("amux API unreachable on loopback: {e}") })),
    }
}

/// Why `call` refuses a path, or None when it may go.
pub(crate) fn call_refusal(method: &str, path: &str) -> Option<&'static str> {
    if !["GET", "POST", "PATCH", "PUT", "DELETE"].contains(&method) {
        return Some("method is GET, POST, PATCH, PUT or DELETE");
    }
    // Checked on the path as it will be sent: a client normalizes `..`, `.`
    // and encoded dots after this check, so any of them could step outside
    // /api (land review F1: `/api/../oauth/token`). A plain path needs none.
    let p = path.split('?').next().unwrap_or("");
    let lower = p.to_ascii_lowercase();
    if p.split('/').any(|seg| seg == ".." || seg == ".") || p.contains("//") || p.contains('\\') || lower.contains("%2e") || lower.contains("%2f") || lower.contains("%5c") {
        return Some("path is a plain path: no `.` or `..` segments, doubled slashes, backslashes or encoded dots and slashes");
    }
    if !(p.starts_with("/api/") || p == "/health") {
        return Some("path starts with /api/ or is /health");
    }
    if path.starts_with("/api/mcp-server") {
        return Some("the MCP server does not call itself");
    }
    None
}

async fn call_tool(headers: &HeaderMap, name: &str, args: &Value) -> Value {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match name {
        "discover" => {
            let f = s("family");
            if f.is_empty() {
                text_result(&index(), false)
            } else {
                match family(&f) {
                    Some(v) => text_result(&v, false),
                    None => text_result(&json!({ "error": format!("no family {f}"), "next": "call discover with no family for the list, or search" }), true),
                }
            }
        }
        "search" => text_result(&search(&s("q")), false),
        "call" => {
            let (method, path) = (s("method").to_uppercase(), s("path"));
            if let Some(why) = call_refusal(&method, &path) {
                return text_result(&json!({ "error": why, "code": "mcp_call_refused" }), true);
            }
            let (code, body) = local(&identity(headers), &method, &path, args.get("body").cloned().filter(|b| !b.is_null())).await;
            tracing::info!(target: "amux::discover", method = %method, path = %path, status = code, measured = true, n_considered = 1, verdict = "mcp_call", "an MCP client called the amux API");
            text_result(&json!({ "status": code, "body": body }), !(200..300).contains(&code))
        }
        "troubleshoot" => {
            let since = args.get("since_h").and_then(Value::as_f64).unwrap_or(1.0).clamp(0.1, 168.0);
            let id = identity(headers);
            let logs_path = format!("/api/logs/analyze?since_h={since}");
            let (h, pull, inv, logs) = tokio::join!(
                local(&id, "GET", "/health", None),
                local(&id, "GET", "/api/pull", None),
                local(&id, "GET", "/api/health/invariants", None),
                local(&id, "GET", &logs_path, None),
            );
            text_result(&json!({
                "health": h.1, "deploy": pull.1, "invariants": inv.1, "log_errors": logs.1,
                "next": ["a refusal's `code` names its rule: search it", "GET /api/debug/routes: does the route exist, with which methods",
                         "GET /api/workers/<you>: your worker's status and why it waits", "raw logs: ~/.amux/logs/server-rs.log on the host (grep -a)"],
            }), false)
        }
        "events" => {
            let (code, body) = local(&identity(headers), "GET", "/api/subscriptions/catalog", None).await;
            text_result(&body, !(200..300).contains(&code))
        }
        other => text_result(&json!({ "error": format!("no tool {other}"), "tools": ["discover", "search", "call", "troubleshoot", "events"] }), true),
    }
}

async fn mcp_route(State(_state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))).into_response(),
    };
    if req.is_array() {
        return Json(rpc_error(Value::Null, -32600, "batches are not supported")).into_response();
    }
    let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
    let Some(id) = req.get("id").cloned() else {
        return StatusCode::ACCEPTED.into_response();
    };
    let params = req.get("params").cloned().unwrap_or(Value::Null);
    let out = match method.as_str() {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let v = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
            rpc_ok(id, json!({
                "protocolVersion": v,
                "capabilities": { "tools": { "listChanged": false }, "resources": { "listChanged": false } },
                "serverInfo": { "name": "amux", "title": "amux", "version": env!("CARGO_PKG_VERSION") },
                "instructions": "amux runs a fleet of coding-agent workers and an API over everything they share (workers, board, schedules, files, groups, memories, settings, messages, and the integrations on top). Start with the discover tool, or read the amux://guide resource: it says how auth, refusals and diagnostics work. Use search to find where something is, discover with a family for its routes and docs, call to use any route as yourself, and troubleshoot when something does not work.",
            }))
        }
        "ping" => rpc_ok(id, json!({})),
        "tools/list" => rpc_ok(id, json!({ "tools": tool_defs() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            rpc_ok(id, call_tool(&headers, &name, &args).await)
        }
        "resources/list" => rpc_ok(id, resources()),
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
            if uri == "amux://guide" {
                rpc_ok(id, json!({ "contents": [{ "uri": uri, "mimeType": "text/markdown", "text": GUIDE }] }))
            } else if let Some(f) = uri.strip_prefix("amux://family/").and_then(family) {
                rpc_ok(id, json!({ "contents": [{ "uri": uri, "mimeType": "application/json", "text": serde_json::to_string_pretty(&f).unwrap_or_default() }] }))
            } else {
                rpc_error(id, -32002, &format!("no resource {uri}"))
            }
        }
        other => rpc_error(id, -32601, &format!("method not found: {other}")),
    };
    Json(out).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_route_maps_to_documented_code_and_the_rest_is_named() {
        let v = index();
        let c = &v["coverage"];
        let (total, documented) = (c["routes"].as_u64().unwrap(), c["documented"].as_u64().unwrap());
        assert_eq!(total as usize, super::super::request_log::ROUTE_TABLE.len(), "the index covers the whole route table");
        assert!(c["modules_with_docs"].as_u64().unwrap() > 200, "build.rs read the module docs: {c}");
        // A route nobody can explain is a route no agent can use (owner,
        // 2026-11-27: "all amux capabilities/apis/featuers are discoverable").
        // Every route maps to a module with a `//!` doc. A new one that does not:
        // give the module that serves it a `//!` doc naming the route, or, for a
        // route declared in api/mod.rs, call a handler from that module.
        assert_eq!(documented, total, "{documented} of {total} routes map to documented code; undocumented: {}", c["undocumented"]);
    }

    #[test]
    fn a_family_carries_its_routes_and_the_docs_that_explain_them() {
        let f = family("crm").expect("crm is a family");
        let paths: Vec<&str> = f["routes"].as_array().unwrap().iter().filter_map(|r| r["path"].as_str()).collect();
        assert!(paths.contains(&"/api/crm/touches/{id}/approve"), "{paths:?}");
        let docs = f["modules"].as_array().unwrap();
        assert!(docs.iter().any(|d| d["doc"].as_str().unwrap_or("").contains("touches")), "the agent side's own doc is there");
        assert!(family("no-such-family").is_none());
    }

    #[test]
    fn search_finds_a_task_and_a_refusal_code() {
        let r = search("schedule shell command");
        let fams: Vec<&str> = r["results"].as_array().unwrap().iter().filter_map(|x| x["family"].as_str()).collect();
        assert!(fams.contains(&"schedules"), "{fams:?}");
        let r = search("crm_touch_suppressed");
        assert_eq!(r["results"][0]["family"], "crm", "{r}");
        assert_eq!(search("")["measured"], false);
    }

    #[test]
    fn the_guide_and_index_read_as_markdown_and_the_mcp_tools_are_listed() {
        let t = llms_txt();
        assert!(t.starts_with("# amux for agents"));
        assert!(t.contains("- `crm` ("), "the family index follows the guide");
        let names: Vec<String> = tool_defs().as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["discover", "search", "call", "troubleshoot", "events"]);
    }

    #[test]
    fn call_goes_only_to_the_api_and_never_to_itself() {
        assert!(call_refusal("GET", "/api/board").is_none());
        assert!(call_refusal("GET", "/health").is_none());
        assert!(call_refusal("GET", "https://example.com/x").is_some());
        assert!(call_refusal("GET", "/oauth/token").is_some());
        assert!(call_refusal("POST", "/api/mcp-server").is_some());
        assert!(call_refusal("TRACE", "/api/board").is_some());
        // Traversal that a URL parser would normalize past the check (land review F1).
        for p in ["/api/../oauth/token", "/api/../api/mcp-server", "/api/./x", "/api//x", "/api/%2e%2e/oauth/token", "/api/%2E%2E/x", "/api/..%2foauth", "/api\\..\\oauth"] {
            assert!(call_refusal("GET", p).is_some(), "{p} must be refused");
        }
        assert!(call_refusal("GET", "/api/board?q=a..b").is_none(), "dots in a query are not a path");
    }

    #[tokio::test]
    async fn the_mcp_server_initializes_lists_and_reads_its_resources() {
        let state = crate::api::standing_approvals::tests::test_state();
        let app = routes().with_state(state);
        let rpc = |body: Value| { let app = app.clone(); async move {
            let req = axum::http::Request::post("/api/mcp-server").header("content-type", "application/json").body(axum::body::Body::from(body.to_string())).unwrap();
            let res = tower::ServiceExt::oneshot(app, req).await.unwrap();
            let status = res.status();
            let b = axum::body::to_bytes(res.into_body(), 4 << 20).await.unwrap();
            (status, serde_json::from_slice::<Value>(&b).unwrap_or(Value::Null))
        } };
        let (_, init) = rpc(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).await;
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(init["result"]["instructions"].as_str().unwrap().contains("discover"));
        let (st, _) = rpc(json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        assert_eq!(st, StatusCode::ACCEPTED);
        let (_, tools) = rpc(json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})).await;
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 5);
        let (_, d) = rpc(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"discover","arguments":{"family":"schedules"}}})).await;
        assert_eq!(d["result"]["structuredContent"]["family"], "schedules");
        let (_, c) = rpc(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"call","arguments":{"method":"GET","path":"/oauth/token"}}})).await;
        assert_eq!((c["result"]["isError"].as_bool(), c["result"]["structuredContent"]["code"].as_str()), (Some(true), Some("mcp_call_refused")));
        let (_, res) = rpc(json!({"jsonrpc":"2.0","id":5,"method":"resources/list"})).await;
        assert!(res["result"]["resources"].as_array().unwrap().iter().any(|r| r["uri"] == "amux://family/crm"));
        let (_, g) = rpc(json!({"jsonrpc":"2.0","id":6,"method":"resources/read","params":{"uri":"amux://guide"}})).await;
        assert!(g["result"]["contents"][0]["text"].as_str().unwrap().contains("## Troubleshoot"));
        let (_, e) = rpc(json!({"jsonrpc":"2.0","id":7,"method":"nope"})).await;
        assert_eq!(e["error"]["code"], -32601);
    }
}
