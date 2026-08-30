//! The dashboard's shipped assets must be INTACT and IN STEP — a guard for two
//! classes the standing checks provably cannot catch.
//!
//! 1. TRUNCATION. On 2026-08-11 a one-liner of the shape
//!    `open(p,'w').write(open(p).read().replace(...))` emptied `sw.js`: the
//!    write handle truncates the file before the argument is evaluated, so the
//!    read returned "" and 6123 bytes became 0 — committed and shipped. The
//!    PostToolUse hook runs `node --check`, which PASSED, because an empty
//!    program is valid JavaScript. A parse check is not a content check, and no
//!    amount of care substitutes for one that can fail (ethos rule 7).
//!
//! 2. VERSION SKEW. CLAUDE.md requires `APP_VER` (app.js) and `CACHE` (sw.js)
//!    to be bumped together — a browser holding the cached script otherwise
//!    never receives the fix. That rule has lived only in prose, so the one
//!    thing every client-side deploy depends on was enforced by memory.
//!
//! These read the SAME files `static_files.rs` embeds at compile time, so a
//! green run is about the bytes that actually ship.

use std::path::PathBuf;

fn asset(name: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../amux-dashboard/static")
        .join(name);
    std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// `const NAME = '...'` / `"..."` — the two declarations this repo actually uses.
fn const_str(src: &str, name: &str) -> Option<String> {
    let i = src.find(&format!("const {name}"))?;
    let rest = &src[i..];
    let eq = rest.find('=')? + 1;
    let tail = rest[eq..].trim_start();
    let quote = tail.chars().next()?;
    if quote != '\'' && quote != '"' {
        return None;
    }
    let body = &tail[1..];
    let end = body.find(quote)?;
    Some(body[..end].to_string())
}

#[test]
fn the_service_worker_still_contains_a_service_worker() {
    let sw = asset("sw.js");
    // The size floor is the blunt half and it is the one that would have caught
    // the incident: 0 bytes parses clean.
    assert!(
        sw.len() > 2000,
        "sw.js is {} bytes — suspiciously small; it was 0 once and `node --check` passed",
        sw.len()
    );
    // The specific half: name the handlers whose absence breaks a PWA, so a
    // partial write is caught too, not just a total one.
    for needle in [
        "addEventListener('install'",
        "addEventListener('activate'",
        "addEventListener('fetch'",
        "addEventListener('push'",
        "addEventListener('notificationclick'",
        "SHELL_URLS",
        "caches.open",
    ] {
        assert!(sw.contains(needle), "sw.js lost `{needle}` — a partial write, or a deletion nobody meant");
    }
}

#[test]
fn the_app_bundle_still_contains_an_app() {
    let app = asset("app.js");
    assert!(app.len() > 500_000, "app.js is {} bytes — far below the shipped bundle", app.len());
    let html = asset("index.html");
    assert!(html.len() > 50_000, "index.html is {} bytes — far below the shipped shell", html.len());
    // The SPA is unusable without these, and each has been broken by a delete
    // at least once in this repo's history.
    for needle in ["function openPeek", "function closePeek", "serviceWorker"] {
        assert!(app.contains(needle) || html.contains(needle), "the SPA lost `{needle}`");
    }
}

/// CLAUDE.md: "Client JS changes need APP_VER and the CACHE version bumped
/// together, or a browser holding the cached script never receives the fix."
/// Enforced here rather than remembered.
#[test]
fn app_ver_and_the_sw_cache_version_agree() {
    let app_ver = const_str(&asset("app.js"), "APP_VER")
        .expect("app.js must declare `const APP_VER = '<version>'`");
    let cache = const_str(&asset("sw.js"), "CACHE")
        .expect("sw.js must declare `const CACHE = 'amux-v<version>'`");

    let expected = format!("amux-v{app_ver}");
    assert_eq!(
        cache, expected,
        "APP_VER ({app_ver}) and the sw.js CACHE ({cache}) disagree. Bump BOTH: a client \
         holding the cached script never receives a fix shipped under a stale cache key."
    );
}

/// The parser above must be able to FAIL, or the test above it is theatre —
/// a `const_str` that always returned None would make both sides `expect`-panic,
/// but one that silently returned the same string for everything would make the
/// comparison vacuous.
#[test]
fn the_version_parser_reads_real_values_and_rejects_junk() {
    assert_eq!(const_str("const APP_VER = '1.2.3';", "APP_VER").as_deref(), Some("1.2.3"));
    assert_eq!(const_str("const CACHE = \"amux-v1.2.3\";", "CACHE").as_deref(), Some("amux-v1.2.3"));
    // A trailing comment must not be swallowed into the value — app.js's real
    // line carries one ("// bump together with the sw.js CACHE version").
    assert_eq!(
        const_str("const APP_VER = '9.9.9';   // bump together", "APP_VER").as_deref(),
        Some("9.9.9")
    );
    assert_eq!(const_str("const APP_VER = 5;", "APP_VER"), None, "unquoted is not a version");
    assert_eq!(const_str("nothing here", "APP_VER"), None);
}

/// 3. DUPLICATE TOP-LEVEL FUNCTION NAMES. A third class the parse check cannot
///    see, and the one that shipped a live regression on 2026-08-25.
///
/// AMUX-3715 added `function _renderArchivedSection(container)` for the board's
/// archived section. The SESSIONS view already had a `_renderArchivedSection`
/// eleven thousand lines earlier. Function declarations hoist and the LAST one
/// wins, so the board version silently replaced the sessions version — and every
/// sessions call site passes no arguments, so it hit `container.appendChild` on
/// `undefined` and threw before the loading overlay could be hidden. The main
/// dashboard view was dead. gtm-research diagnosed and fixed it (7607ee46).
///
/// WHY EVERY EXISTING CHECK WAS GREEN, and this is the part worth keeping: the
/// LANGUAGE makes one of the two shapes an error and the other legal. A
/// duplicate `let`/`const` at the same scope is a SyntaxError that `node --check`
/// catches. A duplicate `function` is valid JavaScript. So the parse check gave
/// real coverage on half the failure and none on the other half, and nothing
/// distinguished the two halves from the outside.
///
/// The author's own commit message that day said every function the new code
/// CALLED had been checked to exist — which is the one-directional version of
/// this check, and the direction that was already covered. Every name you call
/// must exist; every name you define must not already. This is the mirror.
#[test]
fn no_two_top_level_functions_in_app_js_share_a_name() {
    let src = asset("app.js");
    // Column-0 anchored: nested functions are indented, and this file's
    // top-level declarations are not. `const x = function` is not a
    // declaration and cannot collide by hoisting, so it is correctly excluded.
    let mut seen: std::collections::BTreeMap<String, usize> = Default::default();
    for line in src.lines() {
        let rest = match line.strip_prefix("async function ") {
            Some(r) => r,
            None => match line.strip_prefix("function ") {
                Some(r) => r,
                None => continue,
            },
        };
        let name: String =
            rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
        if !name.is_empty() {
            *seen.entry(name).or_insert(0) += 1;
        }
    }

    // PREMISE, asserted: the extractor found the population it is meant to
    // check. An anchor that stopped matching would make this pass over an empty
    // map forever, which is the vacuous green this whole file exists to refuse.
    assert!(
        seen.len() > 200,
        "extracted only {} top-level functions from app.js — the extractor is broken, not the \
         code. Fix it; do not delete the assert.",
        seen.len()
    );
    // And a name known to be there, so a match that silently narrowed is caught
    // as well as one that broke outright.
    assert!(seen.contains_key("renderBoard"), "extractor regressed: renderBoard not found");

    let dupes: Vec<String> =
        seen.iter().filter(|(_, n)| **n > 1).map(|(k, n)| format!("{k} ({n}x)")).collect();
    assert!(
        dupes.is_empty(),
        "two top-level functions share a name in app.js. Declarations HOIST, so the last one \
         silently replaces the earlier one and every earlier call site starts running the wrong \
         body — `node --check` cannot see this because a duplicate `function` is legal (a \
         duplicate `let` would be a SyntaxError, which is why that half was already covered). \
         Rename one: {}",
        dupes.join(", ")
    );
}
