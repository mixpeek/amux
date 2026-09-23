//! ROUTE_TABLE COMPLETENESS: a mounted route must appear in the table
//! (AMUX-4668).
//!
//! `tests/route_table.rs` already walks the table against the real router and
//! can fail in two directions: a path CLAIMED but not routed, and a listed path
//! whose methods are under-listed. Both walks start from ROUTE_TABLE.
//!
//! That leaves one direction structurally uncoverable there: a path mounted by
//! the router and ABSENT from the table entirely is never probed, because the
//! walk has no entry to probe from. axum's Router cannot enumerate its routes,
//! so the missing enumeration has to come from somewhere else. It comes from
//! the source here.
//!
//! MEASURED COST, 2026-09-17. `POST /api/board/{id}/fan-out` shipped mounted
//! and unlisted. Everything downstream of the table then reported it as
//! missing: `GET /api/debug/routes` (which calls itself "the routing truth")
//! omitted it, and the `route.callers_have_routes` invariant failed with "no
//! route matches this path" while the route was answering 409. That filed an
//! autofix card whose stated fault did not exist.
//!
//! WHY A SOURCE SCAN IS THE HONEST INSTRUMENT HERE, and what it cannot do: it
//! reads `.route("…")` literals, so a route mounted through a helper or a
//! computed path is invisible to it. It therefore reports how many modules and
//! routes it actually covered, and refuses to pass on a scan that found
//! implausibly little. A completeness check that silently covered nothing would
//! be the exact failure it exists to prevent.

use amux_server::api::request_log::ROUTE_TABLE;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

fn api_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/api")
}

/// Strip `//` line comments so a `.route(` inside prose is not read as code.
/// The self-match trap: a doc comment describing a route is not a route.
fn code_only(src: &str) -> String {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `.nest("/api/x", module::routes())` -> {"module" => "/api/x"}.
///
/// Modules reached by `.merge(…)` are deliberately absent: they declare whole
/// paths themselves and have no prefix to prepend, so guessing one would
/// manufacture paths that are not mounted.
fn nest_prefixes(mod_rs: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for raw in mod_rs.lines() {
        let line = raw.trim();
        if line.starts_with("//") {
            continue;
        }
        let Some(rest) = line.split_once(".nest(\"").map(|x| x.1) else {
            continue;
        };
        let Some((prefix, tail)) = rest.split_once('"') else {
            continue;
        };
        // `module::routes()` — take the module ident before `::`.
        let Some(after) = tail.split_once("::").map(|x| x.0) else {
            continue;
        };
        let module: String = after
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if module.is_empty() || !prefix.starts_with('/') {
            continue;
        }
        // A module nested at two prefixes cannot be resolved from the file
        // alone, so drop it rather than pick one and report a wrong path.
        out.entry(module)
            .and_modify(|v: &mut String| v.clear())
            .or_insert_with(|| prefix.to_string());
    }
    out.retain(|_, v| !v.is_empty());
    out
}

/// Every `.route("…"` literal in a module.
fn routes_in(src: &str) -> Vec<String> {
    let code = code_only(src);
    let mut out = Vec::new();
    let mut rest = code.as_str();
    while let Some(i) = rest.find(".route(\"") {
        rest = &rest[i + ".route(\"".len()..];
        if let Some((path, tail)) = rest.split_once('"') {
            if path.starts_with('/') {
                out.push(path.to_string());
            }
            rest = tail;
        } else {
            break;
        }
    }
    out
}

fn join(prefix: &str, sub: &str) -> String {
    let joined = format!("{prefix}{sub}");
    let trimmed = joined.trim_end_matches('/');
    if trimmed.is_empty() {
        prefix.to_string()
    } else {
        trimmed.to_string()
    }
}

#[test]
fn every_mounted_route_appears_in_route_table() {
    let dir = api_dir();
    let mod_rs = std::fs::read_to_string(dir.join("mod.rs")).expect("api/mod.rs");
    let prefixes = nest_prefixes(&mod_rs);
    assert!(
        prefixes.len() >= 20,
        "only {} nest prefixes parsed out of api/mod.rs; the parser regressed and a \
         green result would mean nothing",
        prefixes.len()
    );

    let listed: BTreeSet<&str> = ROUTE_TABLE.iter().map(|e| e.path).collect();

    let mut covered_modules = 0usize;
    let mut covered_routes = 0usize;
    let mut missing: Vec<String> = Vec::new();

    for (module, prefix) in &prefixes {
        let path = dir.join(format!("{module}.rs"));
        let Ok(src) = std::fs::read_to_string(&path) else {
            continue;
        };
        covered_modules += 1;
        for sub in routes_in(&src) {
            // DOCUMENTED NON-ROWS, quoted from ROUTE_TABLE's own header: the
            // module-internal catch-alls "whose only job is answering a JSON
            // 404 ... are 'no such route' answerers, not capabilities". Those
            // are `{*rest}` and the module-root `any(catalog_404)`.
            if sub.contains("{*") || sub == "/" {
                continue;
            }
            // A literal already rooted at /api/ is an absolute mount, not a
            // suffix: prefixing it produced "/api/cal-events/api/calendar.ics".
            let full = if sub.starts_with("/api/") {
                sub.clone()
            } else {
                join(prefix, &sub)
            };
            covered_routes += 1;
            // A module may host a SECOND router mounted somewhere else
            // (org.rs declares `/invite/{token}`, merged at TOP LEVEL, and
            // /api/org/invite/zz really does 404 while /invite/zz answers).
            // The route is listed under its real path then, so the bare
            // sub-path counts as listed. Without this the check reports a
            // correct table entry as missing.
            if !listed.contains(full.as_str()) && !listed.contains(sub.as_str()) {
                missing.push(format!("{full}   (mounted in api/{module}.rs)"));
            }
        }
    }

    // POSITIVE CONTROL. A scan that covered nothing would report zero missing
    // routes and read exactly like a clean fleet.
    assert!(
        covered_modules >= 40 && covered_routes >= 150,
        "the scan covered only {covered_modules} module(s) and {covered_routes} route(s), \
         against 50/196 when this landed; it is not reading the API surface, so \
         'no missing routes' would be meaningless"
    );

    assert!(
        missing.is_empty(),
        "{} mounted route(s) are absent from ROUTE_TABLE, so GET /api/debug/routes and the \
         route.callers_have_routes invariant will both report them as having no route \
         (AMUX-4668 shipped exactly this for /api/board/{{id}}/fan-out):\n  {}\n\
         Scanned {covered_modules} modules / {covered_routes} routes.",
        missing.len(),
        missing.join("\n  ")
    );
}

/// The parser must actually find the board module's prefix, which is the one
/// the measured incident belongs to. Without this, a prefix parser that quietly
/// stopped recognising `.nest` would drop modules and the cell above would pass
/// on a smaller surface.
#[test]
fn the_module_that_regressed_is_inside_the_scanned_surface() {
    let dir = api_dir();
    let mod_rs = std::fs::read_to_string(dir.join("mod.rs")).expect("api/mod.rs");
    let prefixes = nest_prefixes(&mod_rs);
    assert_eq!(
        prefixes.get("board").map(String::as_str),
        Some("/api/board"),
        "the board module's nest prefix must resolve; got {:?}",
        prefixes.get("board")
    );

    let src = std::fs::read_to_string(dir.join("board.rs")).expect("api/board.rs");
    let subs = routes_in(&src);
    assert!(
        subs.len() >= 20,
        "only {} routes parsed out of board.rs; the literal scan regressed",
        subs.len()
    );
    assert!(
        subs.iter().any(|s| s == "/{id}/fan-out"),
        "the scan must see the route whose omission this file exists for"
    );
}
