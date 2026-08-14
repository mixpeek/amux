//! The rust/python boundary REGISTRY — and, post-cutover, the standing proof
//! that nothing proxies any more.
//!
//! **The boundary is a TABLE, not archaeology** (AMUX-2597: "there needs to be
//! a clear separation between the two servers"). [`PROXIED_FAMILIES`] is that
//! table; it is EMPTY (AMUX-2608 cut the last row, `/api/scope`) and
//! `tests/proxy_composition.rs` fails the build if a row reappears. Runtime
//! view: `GET /api/debug/boundary` — ethos rule 4, the separation must be
//! enumerable where people already look. Full matrix:
//! docs/rust-migration/server-boundary.md.
//!
//! **The forwarding machinery is GONE (AMUX-2906), and it is not coming back.**
//! It was kept after the last cutover "for a future row". Two facts retired
//! that rationale:
//!
//! 1. The Python server was DELETED at 792ce1f. CLAUDE.md is explicit — do not
//!    resurrect it or add anything that depends on it — so there is no future
//!    row for the machinery to serve.
//! 2. Worse, it had become actively dangerous. `py_base()` defaulted to
//!    localhost on the RETIRED legacy port — which, since the cutover, is THIS
//!    SAME PROCESS answering on its compatibility bind (verified: identical
//!    `pid` and `build` on the retired port and the canonical one). A
//!    `Namespace` row would therefore have forwarded a request to ourselves,
//!    re-entered the same passthrough, and looped — every hop holding a 600s
//!    reqwest timeout and an unbounded buffered body in RAM. Machinery that
//!    would hang the server is not machinery a future row "needs".
//!
//! `crates/amux-server/tests/legacy_port_guard.rs` had already reasoned to the
//! same conclusion in its allowlist ("this default is unreachable; it dies with
//! the module") and its `allowlist_rows_are_live_and_reasoned` check enforces
//! it: the row is now gone because the literal is.
//!
//! Git history has the forwarder (buffered bodies for Python's
//! `Content-Length`-only reader, a hop-by-hop header denylist, the
//! `x-amux-answered-by: python-proxy` stamp) if a genuine cross-process proxy
//! is ever needed. The transport lesson worth keeping — why a passthrough must
//! use explicit routes rather than `.fallback()` — moved to the SPA catch-all
//! in mod.rs, which is where it stays true.

use serde_json::json;

/// One endpoint family the Python server owns. Retained as the type of the
/// (empty, and permanently empty) [`PROXIED_FAMILIES`] table so
/// `/api/debug/boundary` and `tests/proxy_composition.rs` keep a shape to
/// report on.
pub struct ProxiedFamily {
    pub family: &'static str,
    pub why: &'static str,
    pub exit: &'static str,
}

/// Everything that still proxies, in one place. Runtime view:
/// `GET /api/debug/boundary`. Doc: docs/rust-migration/server-boundary.md.
///
/// **EMPTY, and it MUST stay empty (AMUX-2608).** `/api/scope` was the last
/// row; its native port (api/scope.rs) retired it. There is no forwarder left
/// to mount a row against (see the module doc), so re-proxying is now a
/// deliberate build-it-again decision rather than a table edit — which is the
/// correct cost for resurrecting a boundary the cutover removed.
pub const PROXIED_FAMILIES: &[ProxiedFamily] = &[];

/// The RUST-NATIVE /api families, with one-line notes — the other half of
/// `GET /api/debug/boundary`. Kept adjacent to [`PROXIED_FAMILIES`] so the
/// boundary reads as one file; `tests/proxy_composition.rs` cross-checks this
/// list against the routes mod.rs actually mounts (a view must share the
/// predicate of the mechanism it describes — ethos rule 1).
pub const NATIVE_FAMILIES: &[(&str, &str)] = &[
    ("/health", "health + build discriminator"),
    ("/manifest.json", "PWA manifest from branding prefs"),
    ("/api/calendar.ics", "iCal feed"),
    ("/api/sync", "delta sync"),
    ("/api/events", "SSE stream"),
    ("/api/board", "board/tasks CRUD, gates, contract"),
    ("/api/lookup", "explain-selection helper (peek view)"),
    ("/api/skin", "resolved skin (terms/colours/tabs) for a worker"),
    ("/api/config", "declarative instance config: export + idempotent apply"),
    // ---- Mounted in mod.rs but never declared here, so
    // `every_mounted_api_family_is_claimed_by_the_registry` was RED on main and
    // CI could not go green for anyone. Native like everything else:
    // PROXIED_FAMILIES is empty and Python is deleted, so there is no other
    // value these could take. Found in one pass rather than one per test run —
    // the assert reports only the first offender.
    ("/api/channels", "per-session channels + message history"),
    ("/api/client-debug", "SPA debug beacons (write-only, logged)"),
    ("/api/log-search", "grep across session log files"),
    ("/api/memory", "global memory document"),
    ("/api/review", "weekly trends engine + digest markdown"),
    ("/api/workers", "modern worker API (+dead-letters)"),
    ("/api/sessions", "python-SHAPE session list (rust-derived) + per-name verbs — peek/send/config/start/stop/… native over the fleet substrate (api/session_verbs.rs, AMUX-2598)"),
    ("/api/identity", "cloud user + auth-config introspection over server.env/.claude.json (mod.rs)"),
    ("/api/sessions-git", "bulk {session: {branch, repo}} map for the session cards — REUSES the session list's branch (one answer, not two) and adds repo, one rev-parse per DISTINCT dir, 30s TTL (api/sessions_git.rs, AMUX-2599)"),
    ("/api/offline-origin", "which origin can run a service worker — answers from the TLS dir we ACTUALLY serve, and gives no cert advice on a proxied origin (api/offline_origin.rs, AMUX-2599)"),
    ("/api/git", "POST /api/git/staged-guard — the shared-checkout staged-state guard the installed .git/hooks/amux-staged-guard calls on every commit. UNROUTED from the cutover until 2026-08-09 (405 x ~1,147/hr, swallowed by the hook's fail-open), so the guard was silently off fleet-wide (api/git_guard.rs, AMUX-1730)"),
    ("/api/memories", "memories CRUD"),
    ("/api/messages", "inter-worker messages"),
    ("/api/schedules", "scheduler CRUD + runs"),
    ("/api/search", "universal FTS5 search over cards (incl. their log lines), messages, memories, workers, journal, schedules + the index's own drift status/reindex (api/search.rs, migration 0013, RR-0110). Net-new: python never had this route"),
    ("/api/why", "provenance explainer — correlates the state-event journal, request log, card log, schedule runs/audit and turn ledger for one entity or a time window; every line cites its table (api/why.rs, RR-0109). Net-new"),
    ("/api/verify", "verification endpoints"),
    ("/api/prefs", "key/value prefs"),
    ("/api/criteria", "gate criteria"),
    ("/api/metrics", "metrics"),
    ("/api/usage", "token usage"),
    ("/api/alert", "owner alerts"),
    ("/api/stats", "daily stats"),
    ("/api/branding", "white-label branding + assets"),
    ("/api/email", "email send/read (gmail api)"),
    ("/api/cal-events", "calendar events CRUD"),
    ("/api/browser", "full browser family: launch/profiles + CDP driver verbs (screenshot/state/action/inspect/navigate/search) against the server-machine Chrome; /agent answers 501 — the session's model drives the native verbs (api/browser.rs, AMUX-2598)"),
    ("/api/files", "modern files API (raw-body upload, rooted)"),
    ("/api/file", "file VIEWER: payload + raw range streaming + vtt + prepare/transcode with durable media jobs (api/file_viewer.rs)"),
    ("/api/library", "ebook library index — calibre metadata.db / opf scan (api/file_viewer.rs)"),
    ("/api/fs", "SPA Files surface — native port of the python contract (api/fs.rs)"),
    ("/api/ls", "SPA Files browser listing (api/fs.rs)"),
    ("/api/autocomplete", "dir autocomplete (api/fs.rs)"),
    ("/api/upload", "chunked upload protocol (api/upload.rs)"),
    ("/api/uploads", "serving uploaded files (api/upload.rs)"),
    ("/api/groups", "group list + per-group config (api/groups.rs)"),
    ("/api/tags", "legacy spelling of the group list (api/groups.rs)"),
    ("/api/journal", "journal"),
    ("/api/crm", "contacts/tags/interactions/followups — a PORT of the python contract (api/crm.rs, AMUX-2929). The schema shipped in 0001_baseline and 308 contacts were live the whole time; only the routes were missing, so CLAUDE.md documented a 405 to every session"),
    ("/api/speedtest", "download/upload transfer test — the Metrics tab's speed test, ported (api/speedtest.rs, AMUX-2890)"),
    ("/api/layout-presets", "tab layout presets save/load/delete (api/layout_presets.rs)"),
    ("/api/templates", "worker templates the New Worker modal lists (api/worker_create.rs, AMUX-2871)"),
    ("/api/git-check", "is this dir a git worktree — gates the Worktree checkbox (api/worker_create.rs)"),
    ("/api/git-branches", "existing branches for the create modal's chip row (api/worker_create.rs)"),
    ("/api/suggest-branch", "branch-name suggestions; deterministic with no goal text, helper CLI with one (api/worker_create.rs)"),
    ("/api/tmux-sessions", "tmux sessions amux does not already own, for Connect (api/worker_create.rs)"),
    ("/api/iterm2", "open iTerm2 panes, for Connect-a-pane (api/worker_create.rs)"),
    ("/api/saved-messages", "peek composer's reusable snippets, per worker (api/saved_messages.rs, AMUX-2871)"),
    ("/api/proxies", "Proxies tab CRUD; start/stop answer an honest 501 — the tunnel client is AMUX-2888 (api/proxies.rs, AMUX-2887)"),
    ("/api/pull", "self-update button; routes brew/pip installs and REFUSES a pull that would rewrite a shared checkout (api/self_update.rs, AMUX-2891)"),
    ("/api/observability", "Cost tab rollup over token_ledger; does NOT index on request — the periodic job owns that (api/observability.rs, AMUX-2893)"),
    ("/api/habits", "Habits tab state — one JSON array in ~/.amux/habits.json (api/habits.rs, AMUX-2871)"),
    ("/api/scope", "uniform per-capability scope read/write — memory/rules/env/gates/status_mode at global/group/worker, python storage shared byte-for-byte (api/scope.rs, AMUX-2608: the family whose cutover emptied PROXIED_FAMILIES)"),
    ("/api/mcp", "MCP registry — list/import/remove (AMUX-2871)"),
    ("/api/sql", "SQL browser — schema/rows/query, read-only unless write:true"),
    ("/api/skills", "skills list + save/delete"),
    ("/api/slash-commands", "slash commands"),
    ("/api/map", "map + geocoding"),
    ("/api/graph", "Map tab graph mode: mind-map store + Obsidian vault import + fleet org-chart projection (api/graph.rs, AMUX-2886)"),
    ("/api/terminal", "Workspace tab web-terminal panes: local-shell PTY, base64 I/O, long-poll output (api/terminal.rs, AMUX-2885)"),
    ("/api/reports", "Metrics tab report cards: CRUD + type registry + ops-server refresh fetchers (api/reports.rs, AMUX-2884)"),
    ("/api/env", "Declarative environment config: one YAML/JSON -> primitives (groups, workers; phase-2 schedules/columns/gates/files/global). Idempotent apply + dry-run + schema (api/env_config.rs, AMUX-2977)"),
    ("/api/history", "command history"),
    ("/api/logs", "SPA Logs tab: python-shape events + raw over the structured request log (_amux_request_log) and the server-rs.log tracing tail (api/request_log.rs, AMUX-2605)"),
    ("/api/settings", "settings"),
    ("/api/push", "web push"),
    ("/api/dictation", "dictation history/dict CRUD + engine config (native whisper/gemini, api/dictation.rs)"),
    ("/api/dictate", "transcription — native whisper worker + gemini fallback (api/dictation.rs)"),
    ("/api/tts", "text-to-speech (read-aloud) synth + voices — native, api/tts.rs"),
    ("/api/torrents", "torrents"),
    ("/api/org", "org chart"),
    ("/api/gmail", "gmail oauth"),
    ("/api/debug", "debug endpoints, incl. this boundary registry"),
];

/// GET /api/debug/boundary — the registry as JSON, so which server owns which
/// family is a runtime fact, not archaeology (ethos rule 4).
///
/// `proxy_machinery` is stated explicitly because `proxied: []` alone is a
/// weak signal — CLAUDE.md already warns that an empty list "cannot report
/// incompleteness". Naming the absent forwarder tells a debugging session the
/// emptiness is STRUCTURAL (there is nothing left that could forward) rather
/// than a table that merely happens to be clear today.
pub async fn boundary() -> axum::Json<serde_json::Value> {
    axum::Json(json!({
        "proxied": PROXIED_FAMILIES.iter().map(|f| json!({
            "family": f.family,
            "owner": "python",
            "why": f.why,
            "exit": f.exit,
        })).collect::<Vec<_>>(),
        "native": NATIVE_FAMILIES.iter().map(|(fam, note)| json!({
            "family": fam, "owner": "rust", "note": note,
        })).collect::<Vec<_>>(),
        "proxy_machinery": "removed (AMUX-2906) — the python server was deleted at 792ce1f and \
                            the forwarder's default target had become this same process on the \
                            retired legacy bind, i.e. a self-proxy loop. `proxied` is empty \
                            STRUCTURALLY, not incidentally.",
        "doc": "docs/rust-migration/server-boundary.md",
    }))
}
