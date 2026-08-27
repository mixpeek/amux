//! ROUTE_TABLE honesty test (AMUX-2610).
//!
//! `request_log::ROUTE_TABLE` is the hand-maintained routing truth behind
//! `GET /api/debug/routes` and the 404/405 annotations + verdicts in
//! `GET /api/logs/analyze`. axum's Router cannot enumerate its routes, so
//! the table can only stay honest by being WALKED against the real
//! composition (`api::router`), and the walk must be able to fail BOTH
//! directions:
//!
//! - **Claimed but not routed**: an OPTIONS probe at a table path that axum
//!   does not route lands in the SPA catch-all — a nested miss answers 404,
//!   a top-level miss answers the catch-all's 405 whose `Allow` is exactly
//!   `GET(,HEAD)`. Concrete entries assert 405 with `Allow` EQUAL (as a
//!   set, HEAD/OPTIONS aside) to the table's methods, so a bogus path fails
//!   either on status or on the Allow set; the one ambiguous case — a
//!   GET-only entry, whose Allow matches the GET-only catch-all's — is
//!   disambiguated by firing the GET and rejecting the catch-all's
//!   BYTE-EXACT 404 body (`{"error": "not found"}` WITH the space —
//!   hand-written only in static_files.rs; every module body is serde-
//!   serialized without it).
//! - **Routed but not claimed** (an under-listed method): the `Allow` set
//!   equality catches it, and the negative twin — a method the table does
//!   NOT list — is fired and must answer 405. If the router actually
//!   mounts that method, the handler answers something else and the walk
//!   fails, forcing the table to list it.
//!
//! Both directions were demonstrated against deliberately-wrong entries
//! before this landed (a nonexistent path, and a GET-only claim on
//! /api/board which also routes POST); see AMUX-2610.
//!
//! `["*"]` (any()) entries invoke their handler on OPTIONS, so for them the
//! walk asserts the answer is NOT the catch-all's signature (405 with
//! Allow ⊆ {GET, HEAD}, or an HTML 404).

use amux_server::api::request_log::ROUTE_TABLE;
use amux_server::api::{router, AppState};
use amux_server::db::Store;
use axum::body::Body;
use axum::http::Request;
use std::collections::BTreeSet;
use tower::ServiceExt;

/// The SPA catch-all's 404 body for /api paths, byte-exact (the SPACE is the
/// discriminator — see module doc).
const STATIC_404_BODY: &str = "{\"error\": \"not found\"}";

fn concretize(pattern: &str) -> String {
    pattern
        .split('/')
        .map(|seg| {
            if seg.starts_with("{*") {
                "zz-probe"
            } else if seg.starts_with('{') {
                "zz-probe-1"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn allow_set(res: &axum::response::Response) -> BTreeSet<String> {
    res.headers()
        .get("allow")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(',')
        .map(|m| m.trim().to_uppercase())
        .filter(|m| !m.is_empty() && m != "HEAD" && m != "OPTIONS")
        .collect()
}

/// Per-probe wall-clock budget. In-process oneshot fires answer in
/// microseconds; 15s is ~10^5 headroom, chosen loud rather than tight.
const FIRE_BUDGET: std::time::Duration = std::time::Duration::from_secs(15);

/// AF-129: this walk had no per-route timeout, so ONE blocking route turned
/// the documented pre-push gate into a silent hang — it could not go red
/// (rule 7) and nothing named which route wedged it (rule 4). Three orphaned
/// test processes, the oldest 23h, accumulated before anyone connected the
/// timeouts, because "over 60 seconds" reads as slowness and gets waited on.
///
/// The shape matters: the wedge parks with ~zero CPU and every thread asleep
/// (desktop's diagnosis of the orphans), i.e. the handler BLOCKS its thread
/// rather than yielding — and `timeout(dur, future)` cannot preempt a poll
/// that never returns. So the probe runs as its OWN task on the multi_thread
/// runtime (see the test attribute) and the timeout is on the JoinHandle:
/// the join times out on another worker even while the wedged task stays
/// blocked, and the panic names route+method, turning the next occurrence
/// into a one-line read instead of a process listing.
async fn fire(app: &axum::Router, method: &str, path: &str) -> axum::response::Response {
    let app = app.clone();
    let req = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let task = tokio::spawn(async move { app.oneshot(req).await.unwrap() });
    match tokio::time::timeout(FIRE_BUDGET, task).await {
        Ok(joined) => joined.expect("probe task panicked"),
        Err(_) => panic!(
            "{method} {path}: no answer in {FIRE_BUDGET:?} — this route BLOCKS, and without \
             this timeout the whole pre-push gate hangs instead of failing (AF-129). The \
             wedged probe task is still parked; chase this route's handler for the block \
             (a held store connection or an unbounded external wait are the known shapes)."
        ),
    }
}

/// One test fn on purpose: it mutates process env (AMUX_PY_URL, AMUX_HOME)
/// and tests within a binary share the process (same shape as
/// proxy_composition.rs).
///
/// multi_thread flavor is load-bearing for the AF-129 timeout in `fire` —
/// on the default current-thread runtime a synchronously-blocking handler
/// starves the timer and the timeout can never fire. Two workers: one to
/// stay wedged, one to keep the test making progress.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_table_matches_the_real_router_both_directions() {
    // Dead python: /api/scope's any() probe must answer a deterministic 502,
    // never touch a real server.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    std::env::set_var("AMUX_PY_URL", &dead);
    // Hermetic fleet home: probes on sessions/groups/files must not read the
    // developer's real ~/.amux.
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("sessions")).unwrap();
    std::env::set_var("AMUX_HOME", home.path());
    // AF-129, the named wedge: mdai_root() falls back to $HOME, so the fired
    // GET /api/files/mdai walked the developer's ENTIRE home tree — unbounded
    // in time on a real machine (one slow or kernel-blocking directory and
    // the gate hangs; CI's tiny $HOME is why CI stayed green). mdai.rs says
    // "tests pin a temp root" — this test constructs the full router and
    // fires real GETs, so it is exactly such a test and must pin it too.
    std::env::set_var("AMUX_FILES_ROOT", home.path());

    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("t.db")).unwrap();
    let app = router(AppState {
        store: std::sync::Arc::new(store),
        started: std::time::Instant::now(),
        build_hash: "test".into(),
        auth_token: None,
    });

    let twin_candidates = ["DELETE", "PUT", "PATCH", "POST", "GET"];
    for entry in ROUTE_TABLE {
        let path = concretize(entry.path);
        let res = fire(&app, "OPTIONS", &path).await;
        let status = res.status().as_u16();

        if entry.methods.contains(&"*") {
            // any(): OPTIONS reaches the handler. The only failure shape is
            // the catch-all answering instead of a route.
            let allow = allow_set(&res);
            let catchall_405 =
                status == 405 && !allow.is_empty() && allow.iter().all(|m| m == "GET");
            assert!(
                !catchall_405,
                "{}: claimed any() but OPTIONS hit the GET-only SPA catch-all (405, allow={allow:?})",
                entry.path
            );
            let ct = res
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            assert!(
                !(status == 404 && ct.starts_with("text/html")),
                "{}: claimed any() but answered the SPA shell",
                entry.path
            );
            continue;
        }

        // Concrete methods: the route's MethodRouter must answer the OPTIONS
        // probe with 405 + an Allow set equal to the table's claim.
        assert_eq!(
            status, 405,
            "{}: not routed where the table claims (OPTIONS answered {status}, not the \
             method-router's 405)",
            entry.path
        );
        let allow = allow_set(&res);
        let claimed: BTreeSet<String> =
            entry.methods.iter().map(|m| m.to_uppercase()).collect();
        assert_eq!(
            allow, claimed,
            "{}: table methods disagree with the router's Allow set",
            entry.path
        );

        // GET-only ambiguity: the catch-all is itself a GET route at every
        // path, so Allow == {GET} does not prove THIS route exists. The GET
        // answer does: the catch-all's /api 404 body is byte-exact.
        if claimed.len() == 1 && claimed.contains("GET") {
            let res = fire(&app, "GET", &path).await;
            let status = res.status().as_u16();
            // Read the body ONLY on 404: a non-404 already proves a real
            // route answered, and some GET routes stream forever
            // (/api/events is SSE — to_bytes on it never returns).
            if status == 404 {
                // Same AF-129 budget: a 404 body that never finishes streaming
                // must fail naming the route, not hang the gate.
                let body = tokio::time::timeout(
                    FIRE_BUDGET,
                    axum::body::to_bytes(res.into_body(), 64 * 1024),
                )
                .await
                .unwrap_or_else(|_| {
                    panic!("GET {}: 404 body did not finish in {FIRE_BUDGET:?} (AF-129)", entry.path)
                })
                .unwrap();
                let body = String::from_utf8_lossy(&body);
                assert!(
                    body != STATIC_404_BODY,
                    "{}: GET answered the SPA catch-all's 404 — the table claims a \
                     route that does not exist",
                    entry.path
                );
            }
        }

        // Negative twin: a method the table does NOT list must 405 (an
        // under-listed method would reach a handler and answer otherwise).
        let twin = twin_candidates
            .iter()
            .find(|m| !claimed.contains(**m))
            .expect("a method outside the claimed set always exists");
        let res = fire(&app, twin, &path).await;
        assert_eq!(
            res.status().as_u16(),
            405,
            "{}: unlisted method {twin} did not 405 — the router mounts more than the \
             table lists",
            entry.path
        );
    }

    std::env::remove_var("AMUX_PY_URL");
    std::env::remove_var("AMUX_HOME");
    std::env::remove_var("AMUX_FILES_ROOT");
}

/// THE OTHER DIRECTION: a route MOUNTED but absent from the table.
///
/// The module comment above claimed ROUTE_TABLE was "kept honest BOTH
/// directions by tests/route_table.rs". It was not — the test above iterates
/// `for entry in ROUTE_TABLE`, so it can only catch a table row with no route.
/// A route with no table row is invisible to it, and one sat that way:
/// `/api/log-search` was mounted and answering while the route census (which
/// reads the TABLE) reported it as "no route matches this path" and someone
/// nearly ported it a second time.
///
/// Only DIRECT `.route("/api/...")` calls in api/mod.rs are checked here.
/// Nested families (`.nest("/api/x", x::routes())`) declare their sub-paths
/// inside the module and are not visible to a source scan of mod.rs — stating
/// that plainly rather than implying full coverage, which is the overclaim this
/// test exists to correct.
/// Nested routes known to be missing from ROUTE_TABLE (AMUX-2937).
///
/// These answer for real — probed live, they return JSON from their handlers,
/// not the SPA catch-all's HTML — while `/api/debug/routes` and the
/// `route.callers_have_routes` census both read the TABLE and so report them as
/// unrouted. They are listed rather than fixed in the same pass because a
/// ROUTE_TABLE row carries METHODS, and a row with the WRONG methods is worse
/// than a missing one: it manufactures false 405 verdicts in the very census
/// this table feeds. Extracting the verbs mechanically resolved 6 of 17
/// reliably, so the rest need reading by hand.
///
/// The list is checked in BOTH directions below, so it cannot go stale: adding
/// a row to ROUTE_TABLE without removing it here fails too.
const KNOWN_UNTABLED: &[&str] = &[];

#[test]
fn every_directly_routed_api_path_is_in_the_table() {
    let src = include_str!("../src/api/mod.rs");
    let tabled: std::collections::HashSet<&str> =
        ROUTE_TABLE.iter().map(|e| e.path).collect();
    // SCANS THE WHOLE SOURCE, NOT LINE BY LINE. The first version of this test
    // read one line at a time and so missed every multi-line declaration —
    //     .route(
    //         "/api/memory/global",
    //         axum::routing::get(...),
    //     )
    // which is the shape rustfmt produces for anything with two handlers. It
    // passed while /api/memory/global, /api/review/* and /api/channels sat
    // mounted-and-untabled, i.e. it had exactly the blind spot it was written
    // to close, one formatting style over.
    let mut missing = Vec::new();
    let mut rest = src;
    while let Some(i) = rest.find(".route(") {
        rest = &rest[i + ".route(".len()..];
        // Skip whitespace/newlines between `.route(` and the path literal.
        let after = rest.trim_start();
        let Some(stripped) = after.strip_prefix('"') else { continue };
        let Some(end) = stripped.find('"') else { continue };
        let path = &stripped[..end];
        if path.starts_with("/api/") && !tabled.contains(path) {
            missing.push(path.to_string());
        }
    }
    // AND FOLLOW THE COMPOSITION — `.nest()` INTO THE SUB-ROUTER, `.merge()`
    // INTO THE MERGED ONE, AND `.merge()` WRITTEN *INSIDE* A NEST.
    //
    // The scan above only sees `.route()` calls written in api/mod.rs itself,
    // so an entire family mounted as `.nest("/api/crm", crm::routes())` was
    // invisible: its routes live in crm.rs. That is the same blind spot as the
    // multi-line one above, one level up — and it cost exactly what the
    // docstring predicts. The CRM port (AMUX-2929) shipped five routes that
    // answered 200 while `/api/debug/routes` and `route.callers_have_routes`
    // both called them unrouted, and this test stayed green through all of it.
    //
    // Found by the CLI caller scan (AMUX-2917) reporting `POST /api/crm/contacts`
    // as having no route — a NEW instrument catching the gap an older one was
    // structurally unable to see.
    //
    // A merged router carries ABSOLUTE paths (merge adds no prefix), which is
    // exactly why modules use it — the public gmail callback must not sit under
    // a nest wildcard. Following only `.nest()` hid a whole merged family: the
    // gmail mailbox port (AMUX-2883) shipped four routes with none of them
    // tabled, found only because the graph port a day earlier used `.nest` and
    // DID trip it, and the asymmetry begged the question.
    //
    // THE THIRD SHAPE, and the reason this is now ONE walk instead of two
    // (2026-08-26, the general fix @esteininger asked for on #155). A merge can
    // be written INSIDE a nest argument:
    //
    //     .nest("/api/workers", workers::routes().merge(workers_deadletters::routes()))
    //
    // Both previous scanners saw it and neither could report it. The nest
    // scanner took only the FIRST `module::fn` before the `(` and never looked
    // at the rest of the expression, so `workers_deadletters` was not followed
    // at all. The merge scanner DID find it — it scanned the whole file for
    // `.merge(` — but merged paths are absolute *only when the merge is at the
    // top level*, and this one is relative to the enclosing nest, so its own
    // `if !sub.starts_with("/api/") { continue }` threw the route away as
    // out-of-scope. Two scanners, one route, invisible to both.
    //
    // Measured before the fix, on `a78ed225`: deleting the
    // `/api/workers/{id}/dead-letters` row from ROUTE_TABLE left this test AND
    // `absolute_route_literals_in_api_files_are_tabled_even_if_never_mounted`
    // (request_log.rs, renamed from `every_absolute_route_literal_is_in_route_table`
    // for claiming coverage it does not have) green, while deleting
    // the `/api/workers/{id}/peek` row next to it turned this test red. The
    // control is what makes that a measurement rather than a story: the two
    // rows are adjacent, describe sibling routes, and differ only in that one
    // arrives through `.merge()`.
    let api_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api");

    /// The index of the `)` closing the `(` at `open`, respecting nesting and
    /// string literals. Needed because a nest ARGUMENT is an expression that
    /// itself contains parens — `workers::routes().merge(x::routes())` — so
    /// "the next `)`" ends in the middle of it.
    fn matching_paren(s: &str, open: usize) -> Option<usize> {
        let b = s.as_bytes();
        let mut depth = 0i32;
        let mut in_str = false;
        let mut i = open;
        while i < b.len() {
            match b[i] {
                b'\\' if in_str => i += 1,
                b'"' => in_str = !in_str,
                b'(' if !in_str => depth += 1,
                b')' if !in_str => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            }
            i += 1;
        }
        None
    }

    /// Every `module::fn` CALLED in an expression, in source order.
    ///
    /// Taking only the first is what hid `workers_deadletters`. Rust's own
    /// casing is the only filter: modules and fns are snake_case while types are
    /// CamelCase, so `Router::new()` is excluded by the shape of the name rather
    /// than by a hand-kept list — a list is the thing that goes stale silently
    /// here.
    ///
    /// DELIBERATELY NOT FILTERED ON THE FN NAME. An earlier version also required
    /// the fn to be named `*routes*`, which passes today (every router fn in this
    /// crate is `routes`, `routes_with`, `tags_routes`, `serve_routes` or
    /// `callback_routes`) and would silently stop following the first one called
    /// anything else — the same off-not-red failure the canaries below exist to
    /// catch. Measured with the filter removed: no false failures. The residual
    /// risk is the opposite sign, a non-router `snake::snake()` call written
    /// inside a nest/merge argument being followed and reported as unreadable,
    /// and that is the direction to prefer: it is loud, it names the callee, and
    /// it is fixed in one line, whereas the miss is invisible.
    fn callees_of(expr: &str) -> Vec<String> {
        let b = expr.as_bytes();
        let mut out: Vec<String> = Vec::new();
        for (i, c) in expr.char_indices() {
            if c != '(' {
                continue;
            }
            let mut s = i;
            while s > 0 {
                let ch = b[s - 1];
                if ch.is_ascii_alphanumeric() || ch == b'_' || ch == b':' {
                    s -= 1;
                } else {
                    break;
                }
            }
            let cand = expr[s..i].trim();
            let segs: Vec<&str> = cand.split("::").collect();
            if segs.len() < 2 {
                continue;
            }
            let module = segs[segs.len() - 2];
            let func = segs[segs.len() - 1];
            // A router-producing call is `snake_module::snake_fn`. This also
            // drops `axum::routing::get` (module `routing`, but it is a handler
            // wrapper, not a router) — harmless, because a module we do follow
            // and cannot read is reported, and one we skip that mounts nothing
            // costs nothing.
            if module.is_empty()
                || module == "crate"
                || module.starts_with(|c: char| c.is_ascii_uppercase())
                || func.starts_with(|c: char| c.is_ascii_uppercase())
            {
                continue;
            }
            if !out.iter().any(|o| o == cand) {
                out.push(cand.to_string());
            }
        }
        out
    }

    /// `crate::runtime_jobs::autofix::routes` -> the source that defines it.
    ///
    /// Resolves the FULL module path, not just the last segment: the first cut
    /// of the merge scanner looked only under src/api/ and reported four
    /// perfectly readable runtime_jobs modules as unreadable.
    fn module_source(api_dir: &std::path::Path, callee: &str) -> Option<String> {
        let segs: Vec<&str> = callee.split("::").map(str::trim).collect();
        let module = segs[segs.len() - 2];
        let src_dir = api_dir.parent().expect("src/api has a parent");
        let joined: String = segs[..segs.len() - 1]
            .iter()
            .copied()
            .filter(|s| *s != "crate")
            .collect::<Vec<_>>()
            .join("/");
        // `push` is a DIRECTORY module (src/push/mod.rs), so try both layouts
        // before declaring it unreadable.
        std::fs::read_to_string(api_dir.join(format!("{module}.rs")))
            .or_else(|_| std::fs::read_to_string(api_dir.join(module).join("mod.rs")))
            .or_else(|_| std::fs::read_to_string(src_dir.join(format!("{joined}.rs"))))
            .or_else(|_| std::fs::read_to_string(src_dir.join(&joined).join("mod.rs")))
            .ok()
    }

    /// The body of `fn <func>(`, braces balanced.
    fn fn_body<'a>(msrc: &'a str, func: &str) -> Option<&'a str> {
        let fstart = msrc.find(&format!("fn {func}("))?;
        let start = fstart + msrc[fstart..].find('{')?;
        let mut depth = 0i32;
        for (k, c) in msrc[start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(&msrc[start..start + k]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Every `.route("<literal>", <handler-ish text>)` in `body`.
    fn route_literals(body: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut rest = body;
        while let Some(j) = rest.find(".route(") {
            rest = &rest[j + ".route(".len()..];
            let a = rest.trim_start();
            let Some(st) = a.strip_prefix('"') else { continue };
            let Some(e) = st.find('"') else { continue };
            let handler_end = st[e..].find(')').map(|k| e + k).unwrap_or(st.len());
            out.push((st[..e].to_string(), st[e..handler_end].to_string()));
        }
        out
    }

    // HONOUR THE TABLE'S OWN DOCUMENTED EXCLUSION. request_log.rs says plainly
    // what is deliberately NOT a row: "module-internal catch-alls whose only job
    // is answering a JSON 404 … they are 'no such route' answerers, not
    // capabilities", and it names /api/fs/{*rest}, /api/browser/{*rest} and four
    // more.
    //
    // My first version reported all six as gaps, which is the same mistake I
    // made filing AMUX-2917 against /api/stripe: read a missing row, call it an
    // omission, never read the policy sitting above the list. A test that
    // contradicts the documented design of the thing it checks trains people to
    // allowlist their way past it.
    //
    // Detected from the HANDLER, not the path shape, because the two diverge:
    // /api/groups/{*rest} is a real capability and IS tabled, while
    // /api/fs/{*rest} answers 404. Whatever the policy is, the handler is where
    // it is true. Matched on the NAMING CONVENTION, not one spelling: browser.rs
    // calls its answerer `catalog_404`, fs.rs calls it `not_found`, dictation.rs
    // inlines `route_not_found()`.
    fn is_404_answerer(handler: &str) -> bool {
        handler.contains("not_found") || handler.contains("_404")
    }

    // ---- the walk ---------------------------------------------------------
    //
    // Nest spans are recorded FIRST so the top-level merge pass can tell a
    // `.merge()` that adds no prefix from one that inherits a nest's. That
    // distinction is the entire fix; without it the same call site is either
    // skipped (relative paths dropped) or fabricated (prefix glued onto an
    // absolute path).
    // EVERY path the walk RESOLVES, tabled or not. `missing` alone cannot say
    // whether the walk still reaches a given shape, so a scanner that quietly
    // stopped following one would stay green for as long as the table happened
    // to be complete — the check would be off, not red. This is the positive
    // control asserted at the bottom.
    let mut mounted: Vec<String> = Vec::new();
    let mut nest_spans: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0usize;
    while let Some(i) = src[pos..].find(".nest(") {
        let open = pos + i + ".nest(".len() - 1;
        pos = open + 1;
        let Some(close) = matching_paren(src, open) else { continue };
        nest_spans.push((open, close));
        let arg = &src[open + 1..close];
        let a = arg.trim_start();
        let Some(stripped) = a.strip_prefix('"') else { continue };
        let Some(end) = stripped.find('"') else { continue };
        let prefix = &stripped[..end];
        if !prefix.starts_with("/api/") {
            continue;
        }
        for callee in callees_of(&stripped[end..]) {
            let func = callee.rsplit("::").next().unwrap_or_default();
            let Some(msrc) = module_source(&api_dir, &callee) else {
                // A module we cannot read is NOT a pass — a silently skipped
                // nest is the failure this block exists to fix.
                missing.push(format!("<unreadable module for nest {prefix} -> {callee}>"));
                continue;
            };
            // ONLY THE NAMED FUNCTION'S BODY. Scanning the whole file was the
            // first version and it was wrong in two ways at once: a module may
            // define SEVERAL routers (groups.rs has routes() and tags_routes()),
            // and any `.route("/api/...")` belonging to a non-nested one got the
            // nest prefix glued on — producing "/api/cal-events/api/calendar.ics"
            // and "/api/logs/api/board", paths that exist nowhere. A check that
            // invents paths is worse than the blindness it replaced.
            let Some(body) = fn_body(&msrc, func) else {
                // Previously a silent `continue`, which is the same silence the
                // whole test exists to remove: a renamed router fn would have
                // turned the check off rather than red.
                missing.push(format!("<no fn {func}() found for nest {prefix} -> {callee}>"));
                continue;
            };
            for (sub, handler) in route_literals(body) {
                // A nested router's paths are RELATIVE. An absolute /api/
                // literal here means the scan wandered outside the nested
                // router, so gluing the prefix on would fabricate a path.
                if sub.starts_with("/api/") || is_404_answerer(&handler) {
                    continue;
                }
                let full =
                    if sub == "/" { prefix.to_string() } else { format!("{}{}", prefix, sub) };
                if !tabled.contains(full.as_str()) {
                    missing.push(full.clone());
                }
                mounted.push(full);
            }
        }
    }

    // TOP-LEVEL `.merge()` ONLY — anything inside a nest span was resolved
    // above, with the prefix it actually inherits.
    let mut pos = 0usize;
    while let Some(i) = src[pos..].find(".merge(") {
        let open = pos + i + ".merge(".len() - 1;
        pos = open + 1;
        if nest_spans.iter().any(|(s, e)| open > *s && open < *e) {
            continue;
        }
        let Some(close) = matching_paren(src, open) else { continue };
        for callee in callees_of(&src[open + 1..close]) {
            let Some(msrc) = module_source(&api_dir, &callee) else {
                missing.push(format!("<unreadable module for merge {callee}>"));
                continue;
            };
            // The WHOLE FILE, not the named fn's body — deliberately different
            // from the nest walk above. Merged paths carry no prefix, so an
            // absolute `/api/` literal anywhere in a merged module is a real
            // route, and fn-body scoping is what let this family hide:
            // `routes()` delegating to `routes_with(ctx)` has no `.route()`
            // calls of its own, which is exactly the gmail_auth/gmail shape.
            for (sub, handler) in route_literals(&msrc) {
                // Merged paths are absolute-as-written; anything not under
                // /api/ is not this table's business.
                if !sub.starts_with("/api/") || is_404_answerer(&handler) {
                    continue;
                }
                if !tabled.contains(sub.as_str()) {
                    missing.push(sub.clone());
                }
                mounted.push(sub);
            }
        }
    }
    // THE POSITIVE CONTROL — one canary per composition shape this walk claims
    // to follow. Without these the test can only fail when the TABLE is wrong,
    // never when the WALK is; a scanner rewritten back to "first callee only"
    // drops a whole family and every assertion below still passes.
    //
    // If one of these routes is legitimately deleted, do NOT just delete the
    // line: move the canary to another route of the SAME SHAPE, or the shape
    // goes unwatched again. The shape is named next to each one.
    for (shape, canary) in [
        ("`.nest()` into a sub-router", "/api/workers/{id}/peek"),
        ("`.merge()` INSIDE a `.nest()`", "/api/workers/{id}/dead-letters"),
        ("top-level `.merge()` of a module outside src/api", "/api/debug/storage"),
    ] {
        assert!(
            mounted.iter().any(|m| m == canary),
            "the walk no longer resolves {shape} — it did not find {canary}, so every \
             route reached that way is now unchecked and this test would stay green \
             while they went untabled. Fix the walk, or move the canary to another \
             route of the same shape if this one was deliberately removed."
        );
    }

    missing.sort();
    missing.dedup();

    // The mirror, so the allowlist cannot rot: a path that has since been
    // tabled must be dropped from KNOWN_UNTABLED, or the list slowly becomes a
    // place where real gaps hide.
    let stale: Vec<&str> =
        KNOWN_UNTABLED.iter().copied().filter(|k| !missing.iter().any(|m| m == k)).collect();
    assert!(
        stale.is_empty(),
        "KNOWN_UNTABLED lists paths that are now tabled (or gone) — delete them: {stale:?}"
    );

    let missing: Vec<String> =
        missing.into_iter().filter(|m| !KNOWN_UNTABLED.contains(&m.as_str())).collect();
    assert!(
        missing.is_empty(),
        "mounted in api/mod.rs but absent from ROUTE_TABLE — the route census reads \
         the TABLE, so these are reported as unrouted while they answer fine: {missing:?}"
    );
}
