//! axum router assembly (RR-0021, Invariant 13).
//!
//! Route groups land phase by phase. Every route added here must also appear
//! in the OpenAPI spec (RR checklist) and — for legacy-compat paths — in the
//! alias registry (RR-0018a).

pub mod alerts;
pub mod branding;
pub mod channels;
pub mod stats;
pub mod usage;
pub mod aliases;
pub mod auth;
pub mod board;
pub mod criteria;
pub mod browser;
pub mod calendar;
pub mod connectors;
pub mod dictation;
pub mod tts;
pub mod email;
pub mod file_viewer;
pub mod files;
pub mod fs;
pub mod git_guard;
pub mod gmail_auth;
pub mod google_sa;
pub mod groups;
pub mod crm;
pub mod speedtest;
pub mod habits;
pub mod health;
pub mod env_config;
pub mod gmail;
pub mod graph;
pub mod history;
pub mod reports;
pub mod terminal;
pub mod invariants_api;
pub mod journal;
pub mod layout_presets;
pub mod log_search;
pub mod map;
pub mod mdai;
pub mod memories;
pub mod metrics;
pub mod observability;
pub mod offline_origin;
pub mod messages;
pub mod org;
pub mod prefs;
pub mod proxies;
pub mod py_proxy;
pub mod reclaim;
pub mod request_log;
pub mod review;
pub mod saved_messages;
pub mod schedules;
pub mod scope;
pub mod search;
pub mod self_update;
pub mod session_verbs;
pub mod board_themes;
pub mod lookup;
pub mod orchestrate;
pub mod simple;
pub mod config_iac;
pub mod skin;
pub mod commit_mentions;
pub mod sessions_git;
pub mod sessions_legacy;
pub mod settings;
pub mod mcp;
pub mod skills;
pub mod sql;
pub mod sse;
pub mod static_files;
pub mod sync;
pub mod torrents;
pub mod upload;
pub mod verify;
pub mod why;
pub mod worker_create;
pub mod workers;
pub mod workers_deadletters;

use crate::db::SharedStore;
use axum::Router;
use std::time::Instant;
use tower_http::compression::CompressionLayer;

/// Shared application state for handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: SharedStore,
    pub started: Instant,
    /// Content hash of the running binary, the `build` discriminator that
    /// the Python CLAUDE.md workflow leans on: "did the server actually
    /// change under me?" must be answerable from /health.
    pub build_hash: String,
    /// Bearer token; None disables auth (tests, first-run).
    pub auth_token: Option<String>,
}

pub fn router(state: AppState) -> Router {
    // For the request-log layer below — `state` itself is consumed by
    // `.with_state` before the outermost wrap.
    let store_for_reqlog = state.store.clone();
    let protected = Router::new()
        .route("/api/sync", axum::routing::get(sync::delta_sync))
        .route("/api/events", axum::routing::get(sse::events))
        .nest("/api/board", board::routes())
        // Dead-letter routes merge into the workers nest (RR-0068): same
        // /api/workers prefix, second nest at one path is an axum conflict.
        .nest(
            "/api/workers",
            workers::routes().merge(workers_deadletters::routes()),
        )
        .route(
            "/api/ollama/models",
            axum::routing::get(workers::ollama_models),
        )
        .nest("/api/memories", memories::routes())
        .nest("/api/messages", messages::routes())
        .nest("/api/schedules", schedules::routes())
        // RR-0110: universal FTS5 search across cards (incl. their log lines),
        // messages, memories, workers, journal and schedules. Net-new — there
        // was never a /api/search in the Python server or the SPA.
        .nest("/api/search", search::routes())
        // RR-0109: `why` — the provenance explainer over the durable trails
        // (state-event journal, request log, card log, schedule runs/audit,
        // turn ledger). The CLI verb is a printer over this; the correlation
        // lives here so there is one place to be wrong.
        .nest("/api/why", why::routes())
        .nest("/api/verify", verify::routes())
        .nest("/api/prefs", prefs::routes())
        .nest("/api/criteria", criteria::routes())
        .nest("/api/metrics", metrics::routes())
        .nest("/api/reclaim", reclaim::routes())
        .nest("/api/usage", usage::routes())
        .nest("/api/review", review::routes())
        .nest("/api/channels", channels::routes())
        .nest("/api/alert", alerts::routes())
        .route("/api/stats/daily", axum::routing::get(stats::daily))
        // The writer for the baseline `daily` already reads (AMUX-2871).
        .route("/api/stats/reset", axum::routing::post(stats::reset))
        .route("/api/branding", axum::routing::get(branding::get_branding)
            .post(branding::post_branding).delete(branding::delete_branding))
        // base64 icons: the handler's own 5MB check must answer (Python's
        // 400), not axum's 2MB default 413.
        .layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024))
        .route("/api/branding/asset/{fname}", axum::routing::get(branding::serve_asset))
        .nest("/api/email", email::routes())
        .nest("/api/cal-events", calendar::routes())
        // Legacy SHAPE (not just path): the SPA renders this array (RR-0075).
        // POST creates a FLEET worker (an env file), which is a different
        // substrate from POST /api/workers (a `workers` table row) — the
        // cutover moved GET and left the SPA's create dialog 405ing.
        .route(
            "/api/sessions",
            axum::routing::get(sessions_legacy::list_sessions_legacy)
                .post(sessions_legacy::create_session_legacy),
        )
        // Per-name session verbs — NATIVE (AMUX-2598): peek/send/config/
        // start/stop/resize/duplicate/clone/steer/share/… answer from the
        // fleet substrate (env files + tmux + shared DB), api/session_verbs.rs.
        // Was PROXIED_FAMILIES' SessionVerbs row; the rust-worker 501 pointer
        // guard moved into the module's dispatch.
        .merge(session_verbs::routes())
        // Identity is config-introspection over server.env + ~/.claude.json
        // this server already reads — NATIVE, not a fleet capability
        // (AMUX-2597 addendum: the SPA's _initIdentity boot call 404'd here).
        .route("/api/identity", axum::routing::get(identity))
        // Uniform per-capability scope read/write — NATIVE (AMUX-2608: the
        // LAST python-proxied family; its cutover emptied PROXIED_FAMILIES).
        .nest("/api/scope", scope::routes())
        .nest("/api/orchestrate", orchestrate::routes())
        // Nothing proxies. py_proxy::PROXIED_FAMILIES is EMPTY post-AMUX-2608
        // and the forwarder it fed was deleted in AMUX-2906, so the merge that
        // used to sit here (already a no-op) is gone too — the registry, the
        // /api/debug/boundary view and tests/proxy_composition.rs remain the
        // standing proof of the cutover. Matrix:
        // docs/rust-migration/server-boundary.md.
        .nest("/api/browser", browser::routes())
        // File VIEWER family — NATIVE (AMUX-2598): payload + raw range
        // streaming + vtt + ffmpeg prepare/transcode with durable job state
        // (api/file_viewer.rs; was PROXIED_FAMILIES' /api/file namespace row).
        .nest("/api/file", file_viewer::routes())
        // Ebook library index (calibre metadata.db / opf scan) — top-level
        // path in python, so top-level here.
        .route("/api/library", axum::routing::any(file_viewer::library))
        .nest("/api/files", files::routes())
        // The .mdai computed-file engine (AMUX-3240): run a node's upstream DAG,
        // list nodes, browse run history, connect an edge. Nested one level
        // deeper than /api/files above; files::routes() has no /mdai or wildcard,
        // so the two prefixes do not conflict.
        .nest("/api/files/mdai", mdai::routes())
        // /api/fs/* is the SPA's Files contract (multipart upload + dir
        // field, open/mkdir/rename/read/list/search/delete on ABSOLUTE
        // paths) — a different contract from /api/files above. NATIVE port
        // of the python handlers (api/fs.rs), same wire shapes.
        .nest("/api/fs", fs::routes())
        // The SPA Files BROWSER's listing + dir autocomplete — native, same
        // module (top-level paths in python, so top-level here).
        .route("/api/ls", axum::routing::any(fs::ls))
        .route("/api/autocomplete/dir", axum::routing::any(fs::autocomplete_dir))
        // Chunked upload: /api/upload/start, /api/upload/:id/chunk/:n,
        // /api/upload/:id/finish — native Rust (no Python proxy).
        .nest("/api/upload", upload::routes())
        .nest("/api/uploads", upload::serve_routes())
        // Groups/tags (AMUX-2594/AMUX-2597): NATIVE — the list derives from
        // CC_TAGS env files, config from the shared group_config table
        // (api/groups.rs). Both spellings, matching python's alias.
        .nest("/api/groups", groups::routes())
        .nest("/api/tags", groups::tags_routes())
        .nest("/api/journal", journal::routes())
        // CRM: a PORT of the python contract, not a new feature — the schema and
        // 308 live contacts survived the cutover with no routes to reach them
        // (AMUX-2929).
        .nest("/api/crm", crm::routes())
        .nest("/api/speedtest", speedtest::routes())
        .nest("/api/layout-presets", layout_presets::routes())
        // The New Worker / Connect modals' supporting reads (AMUX-2871).
        .merge(worker_create::routes())
        .nest("/api/saved-messages", saved_messages::routes())
        .merge(habits::routes())
        .merge(observability::routes())
        .merge(connectors::routes())
        .merge(self_update::routes())
        .nest("/api/proxies", proxies::routes())
        // Skills / slash-commands / map: the SPA tabs' data (AMUX-2586 #6).
        .nest("/api/skills", skills::routes())
        .nest("/api/mcp", mcp::routes())
        .nest("/api/sql", sql::routes())
        .nest("/api/slash-commands", skills::slash_routes())
        .nest("/api/map", map::routes())
        // Map tab's graph mode: mind-map store + Obsidian import + the fleet
        // org-chart projection (AMUX-2886 — tables existed, routes did not).
        .nest("/api/graph", graph::routes())
        // Workspace tab web-terminal panes: local-shell PTY over base64 I/O +
        // long-poll output (AMUX-2885 — every keystroke 404'd since cutover).
        .nest("/api/terminal", terminal::routes())
        // Metrics tab report cards: CRUD + registry + ops-server refresh
        // (AMUX-2884 — the table kept its rows, the routes never mounted).
        .nest("/api/reports", reports::routes())
        // Declarative environment config: one YAML -> the primitives
        // (groups, workers; phase 2: schedules/columns/gates/files) — AMUX-2977.
        .nest("/api/env", env_config::routes())
        .nest("/api/history", history::routes())
        // Logs tab (AMUX-2605): python-shape /api/logs + /api/logs/raw over
        // the structured request log + tracing tail (api/request_log.rs).
        .nest("/api/logs", request_log::routes())
        .nest("/api/settings", settings::routes())
        .nest("/api/push", crate::push::routes())
        .nest("/api/dictation", dictation::routes())
        // Transcription lives at the TOP-LEVEL /api/dictate (python parity);
        // the dictation module owns it and answers NATIVELY (AMUX-2598:
        // whisper worker + gemini fallback in this process). Body limit off:
        // audio runs to 25MB raw / ~33MB base64, and the handler's own 413
        // must answer, not axum's 2MB default.
        .route(
            "/api/dictate",
            axum::routing::post(dictation::dictate)
                .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        // Text-to-speech (read-aloud): top-level EXACT paths the SPA calls
        // (POST /api/tts, GET /api/tts/voices) — mounted flat rather than
        // nested so `POST /api/tts` matches with no trailing-slash ambiguity.
        // Default body limit is fine: the body is text, not audio.
        .route("/api/tts", axum::routing::post(tts::synth))
        .route("/api/tts/voices", axum::routing::get(tts::voices))
        .nest("/api/torrents", torrents::routes())
        .nest("/api/org", org::routes())
        // Absolute-path routes (merged, not nested): the gmail callback
        // below is public, and a nest wildcard at /api/gmail would shadow it.
        .merge(gmail_auth::routes())
        // The mailbox half — labels/inbox/thread/send over GmailClient
        // (AMUX-2883: the Mail view's calls 404'd since the python retirement).
        .merge(gmail::routes())
        // AMUX-2599 — three endpoints the SPA calls on EVERY load that 404'd
        // here. Each 404 body deserialized cleanly into the client's success
        // path, so all three failed silently: no branch badges, per-worker
        // gates rendering as deleted, and an unactionable red offline banner.
        .route("/api/sessions-git", axum::routing::get(sessions_git::sessions_git))
        .route("/api/board/themes", axum::routing::get(board_themes::board_themes))
        .route("/api/lookup", axum::routing::post(lookup::lookup))
        .route("/api/skin", axum::routing::get(skin::get_skin))
        .route("/api/config/export", axum::routing::get(config_iac::export))
        .route("/api/config/apply", axum::routing::put(config_iac::apply))
        .route(
            "/api/board/commit-mentions",
            axum::routing::get(commit_mentions::commit_mentions),
        )
        // The shared-checkout staged-guard's endpoint. UNROUTED since the
        // rust cutover — 405, ~1,147 calls/hour, and the installed
        // .git/hooks/amux-staged-guard swallowed every one of them
        // (`except Exception: return 0`), so every commit on every shared
        // checkout has been unguarded and SILENT about it. See api/git_guard.rs.
        .route("/api/git/staged-guard", axum::routing::post(git_guard::staged_guard))
        // Client diagnostic beacons (AR-128): the SPA sends geometry/tap
        // traces for mobile layout debugging.
        //
        // This used to log ONLY `kind`, at debug!, and throw the body away.
        // Measured 2026-08-13: 299 beacons received — 47 peek-geo, 51 boot-geo,
        // 31 peek-geo-kbd, 2 card-menu-geo — every one of them from a real
        // phone, every payload discarded. The numbers these beacons exist to
        // carry (the rect the menu ACTUALLY landed at, visualViewport offset,
        // innerHeight) never survived the receiver, so the one instrument built
        // to answer "why does this render off-screen on Ethan's iPhone but not
        // in any simulator" could not answer it. A beacon whose payload is
        // dropped is the same as no beacon, and it costs more, because it
        // reads as covered (ethos rule 4).
        //
        // Now: full payload at INFO (greppable in server-rs.log) AND a bounded
        // ring readable over HTTP, because "ask the server, do not grep for it"
        // is this repo's own rule and a grep cannot be pointed at a phone.
        .route(
            "/api/client-debug",
            axum::routing::post(client_debug_post).get(client_debug_get),
        )
        .route("/api/log-search", axum::routing::get(log_search::search))
        .route(
            "/api/memory/global",
            axum::routing::get(global_memory_get).post(global_memory_post),
        )
        .route(
            "/api/offline-origin",
            axum::routing::get(offline_origin::offline_origin),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_bearer,
        ));

    let app = Router::new()
        // Public: the PWA shell + health must load before auth happens.
        .route("/health", axum::routing::get(health::health))
        .route("/api/debug/tmux", axum::routing::get(health::debug_tmux))
        // The terminal-scan loop's last pass (AF-80): which lanes were demoted
        // off pane-scraping and on what basis, so a skip leaves a trace instead
        // of reading as a scan that found nothing (ethos rule 4, the D1 "scan"
        // deviation). Advertised in ethos.md and the job registry but unrouted
        // until now. Public like its debug siblings (lane names and timings).
        .route("/api/debug/scan", axum::routing::get(health::debug_scan))
        .route("/api/debug/downtime", axum::routing::get(health::debug_downtime))
        // Per-session logging health + a computed stale verdict (AMUX-2628).
        // Public like its debug siblings: session names and byte counts only.
        .route("/api/debug/logs", axum::routing::get(session_verbs::debug_logs))
        // The rust/python ownership registry, readable where a debugging
        // session already looks (ethos rule 4): which families answer
        // natively, which proxy to python and why, and the cutover exit for
        // each. Route names only — nothing secret, so public like its
        // debug sibling above.
        .route("/api/debug/boundary", axum::routing::get(py_proxy::boundary))
        // Legacy-port retirement counter (ethos rule 4): "can we stop
        // answering 8822 yet?" as a number rather than a guess — hit count,
        // which IPs/user-agents are still calling it, and the exit condition
        // in the payload itself. Public like its debug siblings.
        .route(
            "/api/debug/legacy-port",
            axum::routing::get(crate::legacy_port::debug),
        )
        .merge(invariants_api::routes())
        .merge(crate::runtime_jobs::board_drive::routes())
        .merge(crate::runtime_jobs::autofix::routes())
        .merge(crate::runtime_jobs::storage::routes())
        // Every internal background job, its last tick, and a status that can
        // say STALLED (AMUX-2703). Public like its debug siblings: job names
        // and timings only.
        .merge(crate::runtime_jobs::registry::routes())
        // The routing truth (AMUX-2610): the ROUTE_TABLE as JSON, so "is X
        // routed, with which methods" is a GET, not a grep. Public like its
        // debug siblings (route names only, nothing secret).
        .route("/api/debug/routes", axum::routing::get(request_log::debug_routes))
        // Public: calendar fetchers (Google/Apple) cannot send bearer tokens.
        .route("/api/calendar.ics", axum::routing::get(calendar::ics_feed))
        // Dynamic manifest: PWA name/color follow the branding prefs (the
        // static file is the fallback inside the handler).
        .route("/manifest.json", axum::routing::get(branding::manifest))
        // Public: Google's OAuth redirect carries no bearer token. Python
        // admits it via the localhost auth bypass; require_bearer has no
        // such bypass, so the callback must sit outside it (single-use
        // server-minted state is the guard).
        .merge(gmail_auth::callback_routes())
        .merge(static_files::routes())
        .merge(protected)
        .with_state(state);

    // Legacy route aliases (RR-0018a): /api/sessions/* rewrites to
    // /api/workers/* BEFORE routing, so the rewrite must wrap the finished
    // router. Auth is inside the wrapper — legacy paths are exactly as
    // protected as canonical ones.
    let app = aliases::alias_layer(app);

    // Transparent gzip compression for every response whose client sends
    // Accept-Encoding: gzip. Board slim drops from 690KB to 162KB,
    // sessions from 149KB to 21KB, and the SPA shell from 215KB to 46KB.
    let app = app.layer(CompressionLayer::new());

    // Structured request log (AMUX-2605): the OUTERMOST wrap, so every
    // request — including alias-rewritten and fallback paths — is recorded
    // with the RAW path the client sent. Never blocks or fails a request
    // (rows ride a bounded channel to the single-writer store).
    request_log::layer(app, store_for_reqlog)
}

// ---------------------------------------------------------------------------
// GET /api/identity — cloud user info + auth-config introspection, NATIVE
// (python source: amux-server.py:73349-73385; placeholder set py:77268-77283).
// The SPA's `_initIdentity` calls this at boot (app.js:568); it 404'd on the
// rust origin. Lives here rather than a module because it is a single
// handler over config this server already loads.
// ---------------------------------------------------------------------------

/// Python's `_PLACEHOLDER_API_KEYS` (py:77268), verbatim: obvious
/// template/dummy values that must not count as "a key is configured".
const PLACEHOLDER_API_KEYS: &[&str] = &[
    "changeme", "change-me", "change_me",
    "your-api-key", "your_api_key", "your-key", "yourkey",
    "your_key_here", "your-key-here", "your-api-key-here",
    "placeholder", "dummy", "example", "sample",
    "test", "test-key", "testkey",
    "replace-me", "replace_me",
    "xxx", "xxxx",
    "sk-ant-xxx", "sk-ant-your-key", "sk-ant-example", "sk-ant-placeholder",
];

/// The 500 every API module was spelling for itself (AMUX-2919).
///
/// Fourteen identical private copies, in two spellings — half via a local
/// `err()`, half constructing the tuple inline — all producing exactly
/// `500 {"error": "<display>"}`. Identical output, so unlike `truthy` below
/// this one really was duplication rather than divergence. Shared so the error
/// SHAPE cannot drift per-module: a client parsing `.error` should not have to
/// care which family answered.
pub(crate) fn internal(e: impl std::fmt::Display) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}

/// Python truthiness for a JSON value ({} , "" , [] , 0 falsy) — identity's
/// `oauthAccount` check and scope's `if items:` gate share it.
pub(crate) fn py_truthy(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => false,
        serde_json::Value::Bool(b) => *b,
        serde_json::Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        serde_json::Value::String(s) => !s.is_empty(),
        serde_json::Value::Array(a) => !a.is_empty(),
        serde_json::Value::Object(o) => !o.is_empty(),
    }
}

/// Recent client-debug beacons, newest last. Bounded so a chatty client cannot
/// grow it without limit.
///
/// This ring is VOLATILE and the GET says so in its own response rather than
/// letting a reader assume otherwise: the builder swaps the running binary
/// whenever anyone commits, which clears it. That is acceptable HERE and would
/// not be for state anything decides on — the durable copy is the INFO log line
/// written on every POST, and this ring exists so a session can ask a question
/// instead of grepping. Do not promote it to a source of truth without giving
/// it a table (D1: "in-memory state is fiction").
static CLIENT_DEBUG_RING: std::sync::Mutex<Vec<serde_json::Value>> =
    std::sync::Mutex::new(Vec::new());
const CLIENT_DEBUG_RING_MAX: usize = 500;

async fn client_debug_post(
    headers: axum::http::HeaderMap,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::Json<serde_json::Value> {
    let kind = body
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // The WHOLE body, at INFO. The payload is the entire point of the beacon;
    // logging just `kind` is how 299 of these arrived carrying real device
    // geometry and told us nothing.
    tracing::info!(kind = %kind, ua = %ua, payload = %body, "client-debug beacon");

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut ring) = CLIENT_DEBUG_RING.lock() {
        ring.push(serde_json::json!({ "ts": ts, "kind": kind, "ua": ua, "body": body }));
        let n = ring.len();
        if n > CLIENT_DEBUG_RING_MAX {
            ring.drain(0..n - CLIENT_DEBUG_RING_MAX);
        }
    }
    axum::Json(serde_json::json!({"ok": true}))
}

/// `GET /api/client-debug?kind=<k>&limit=<n>` — read back what real devices
/// reported. Answers "what did HIS phone actually measure", which no simulator
/// and no grep on this machine can.
async fn client_debug_get(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::Json<serde_json::Value> {
    let want = q.get("kind").cloned();
    let limit: usize = q
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(50)
        .min(CLIENT_DEBUG_RING_MAX);

    let ring = match CLIENT_DEBUG_RING.lock() {
        Ok(r) => r.clone(),
        Err(_) => Vec::new(),
    };
    let total = ring.len();
    let mut rows: Vec<serde_json::Value> = ring
        .into_iter()
        .filter(|r| match &want {
            Some(k) => r.get("kind").and_then(|v| v.as_str()) == Some(k.as_str()),
            None => true,
        })
        .collect();
    let matched = rows.len();
    if rows.len() > limit {
        rows.drain(0..matched - limit);
    }

    axum::Json(serde_json::json!({
        "beacons": rows,
        "matched": matched,
        "held": total,
        "cap": CLIENT_DEBUG_RING_MAX,
        // Stated, not implied: an empty result here means "nothing since this
        // binary started", NOT "the client never sent one". The builder swaps
        // the binary on any commit. The durable copy is the INFO log line.
        "volatile": true,
        "note": "in-memory since process start; durable copy is the 'client-debug beacon' INFO line in ~/.amux/logs/server-rs.log",
    }))
}

async fn identity(headers: axum::http::HeaderMap) -> axum::Json<serde_json::Value> {
    let email = headers
        .get("x-amux-user-email")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // has_api_key counts ONLY server.env (python's comment: never
    // Docker-injected process env), and only non-placeholder values.
    let home = std::env::var("AMUX_HOME").map(std::path::PathBuf::from).unwrap_or_else(|_| {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux")
    });
    let mut has_key_in_env = false;
    let mut base = String::new();
    let mut tok = String::new();
    if let Ok(env_text) = std::fs::read_to_string(home.join("server.env")) {
        for l in env_text.lines() {
            let l = l.trim();
            if let Some(val) = l.strip_prefix("ANTHROPIC_API_KEY=") {
                let v = val.trim().trim_matches('"').trim_matches('\'').to_lowercase();
                if !v.is_empty() && !PLACEHOLDER_API_KEYS.contains(&v.as_str()) {
                    has_key_in_env = true;
                }
            } else if let Some(val) = l.strip_prefix("ANTHROPIC_BASE_URL=") {
                base = val.trim().to_string();
            } else if let Some(val) = l.strip_prefix("ANTHROPIC_AUTH_TOKEN=") {
                tok = val.trim().to_string();
            }
        }
    }
    // A managed upstream (amux cloud's Anthropic proxy) is fully configured
    // auth — never prompt such a workspace for a key (python's comment).
    let has_proxy = !base.is_empty() && !tok.is_empty();
    let has_oauth = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".claude.json"))
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .map(|v| py_truthy(&v["oauthAccount"]))
        .unwrap_or(false);
    // key_valid/key_error: python serves a CACHED background validation
    // (_api_key_status). This server runs no validator, so it answers what
    // python answers before its first validation — null/"" — rather than
    // inventing a verdict (Invariant 20: never invent state).
    axum::Json(serde_json::json!({
        "email": email,
        "is_cloud": !email.is_empty(),
        "has_api_key": has_key_in_env || has_oauth || has_proxy,
        "has_oauth": has_oauth,
        "managed_upstream": has_proxy,
        "key_valid": serde_json::Value::Null,
        "key_error": "",
    }))
}

// ---- /api/memory/global ----

fn global_memory_path() -> std::path::PathBuf {
    let home = std::env::var("AMUX_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".amux")
        });
    home.join("memory").join("_global.md")
}

async fn global_memory_get() -> axum::Json<serde_json::Value> {
    let content = std::fs::read_to_string(global_memory_path()).unwrap_or_default();
    axum::Json(serde_json::json!({"content": content}))
}

async fn global_memory_post(
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let content = body
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let path = global_memory_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&path, content) {
        Ok(_) => axum::Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod truthy_reachability {
    /// Does `Number::as_f64()` EVER return None in this build?
    ///
    /// The whole premise of AMUX-2928 was that py_truthy and calendar/email's
    /// truthy disagree on a number that does not fit f64. Worth confirming such
    /// a number can exist here before "fixing" anything — without the
    /// `arbitrary_precision` feature, serde_json stores every number as
    /// u64/i64/f64 and `as_f64()` casts lossily rather than failing.
    #[test]
    fn as_f64_never_none_without_arbitrary_precision() {
        for s in ["18446744073709551615", "-9223372036854775808", "0", "1", "1.5"] {
            let v: serde_json::Value = serde_json::from_str(s).unwrap();
            let n = v.as_number().expect("parsed as a number");
            assert!(
                n.as_f64().is_some(),
                "{s} produced as_f64() == None — the truthy divergence IS reachable, \
                 and AMUX-2928's fix is a behaviour change after all"
            );
        }
    }
}
