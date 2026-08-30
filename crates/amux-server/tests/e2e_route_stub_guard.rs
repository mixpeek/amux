//! A spec that stubs a request must route through `e2e/fixtures.ts` (AF-47).
//!
//! `page.route` fails SILENTLY when its pattern never matches. The stub does not
//! error, the app keeps talking to the real endpoint, and the spec's assertions
//! are made against live data — so the failure that surfaces is a confident,
//! specific, WRONG claim about whatever was being asserted. In the measured
//! instance a service worker swallowed `/api/system-jobs` and the test failed on
//! "the stalled-row styling is broken under WebKit". The natural response is to
//! go read the CSS.
//!
//! `e2e/fixtures.ts` wraps `page.route` so a stub that matched zero requests
//! fails the test by name. That only reaches a spec that imports `test` from it,
//! and a per-file convention that must be REMEMBERED is exactly what failed the
//! first time — playwright.config.ts's own `serviceWorkers: 'block'` comment
//! records the same lesson three specs later. So the guard is here rather than
//! in a review checklist.
//!
//! # What counts as a violation
//!
//! A `.spec.ts` under `e2e/` that calls `page.route(` or `context.route(` while
//! importing `test` from `@playwright/test` instead of `./fixtures`.
//!
//! # What this guard does NOT cover, said out loud
//!
//! `context.route` is NOT wrapped by the fixture — only `page.route` is. No spec
//! uses it today. It is flagged here anyway, with its own message, because the
//! honest options for a spec that needs it are "use page.route" or "extend the
//! fixture", and both are better than an unguarded stub nobody knew was
//! unguarded. A guard that stayed silent about the gap would be claiming a
//! coverage it does not have.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // crates/amux-server -> up two.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn spec_files() -> Vec<PathBuf> {
    let dir = workspace_root().join("e2e");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".spec.ts"))
        .collect();
    out.sort();
    out
}

#[test]
fn a_spec_that_stubs_a_request_routes_through_the_fixture_that_notices_a_dead_stub() {
    let specs = spec_files();
    // THE GUARD MUST HAVE SOMETHING TO WALK. An empty e2e dir — a rename, a
    // moved testDir — would make every assertion below vacuously true, which is
    // the shape that lets a guard rot into a rubber stamp.
    assert!(
        specs.len() > 10,
        "found only {} .spec.ts under e2e/ — the guard is walking the wrong directory, \
         and an empty walk passes every check below",
        specs.len()
    );

    let mut offenders: Vec<String> = Vec::new();
    let mut context_route: Vec<String> = Vec::new();
    let mut guarded = 0usize;

    for p in &specs {
        let Ok(src) = std::fs::read_to_string(p) else { continue };
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        let uses_page = src.contains("page.route(");
        let uses_ctx = src.contains("context.route(");
        if !uses_page && !uses_ctx {
            continue;
        }
        if uses_ctx {
            context_route.push(name.clone());
        }
        if src.contains("from './fixtures'") {
            guarded += 1;
        } else {
            offenders.push(name);
        }
    }

    // A POSITIVE CONTROL. If nothing in the tree stubs a request, the offender
    // list is empty for a reason that has nothing to do with compliance, and
    // this test would pass forever over a fixture nobody imports.
    assert!(
        guarded > 0,
        "no spec imports test from './fixtures' — either the fixture was removed \
         or every stub was deleted. Either way this guard is now measuring nothing."
    );

    assert!(
        offenders.is_empty(),
        "these specs stub a request but import `test` from '@playwright/test', so a stub \
         that matches ZERO requests passes silently and the spec asserts against the REAL \
         endpoint (AF-47):\n    {}\n\n  Fix: import {{ test, expect }} from './fixtures';\n  \
         Type-only imports can stay on '@playwright/test' (import type {{ Page }} from ...).",
        offenders.join("\n    ")
    );

    assert!(
        context_route.is_empty(),
        "these specs use `context.route(`, which e2e/fixtures.ts does NOT wrap — only \
         page.route is counted, so a dead context stub is still silent:\n    {}\n\n  \
         Either use page.route, or extend the fixture to wrap the context and delete this \
         assertion in the same commit.",
        context_route.join("\n    ")
    );
}
