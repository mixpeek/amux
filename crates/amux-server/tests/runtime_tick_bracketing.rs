//! AMUX-4828: every runtime loop must BRACKET its tick, and the exceptions must
//! be named.
//!
//! `registry::tick(id)` sets last_start and last_end to the same instant, so it
//! writes no duration. `classify_observed` reads `last_tick_ms` for exactly one
//! purpose, upgrading `ok` to `slow` when a tick exceeds its budget, so for a
//! loop on the one-shot that branch is DEAD CODE: a job that merely runs LONG
//! can only ever present as `ok` or `stalled`. `stalled` is the word a reader
//! acts on, and it means dead.
//!
//! The one-shot is also typically stamped BEFORE the work, reporting a pass
//! that STARTED while every reader takes a tick to mean one that is DONE.
//!
//! This is a TREE-WIDE guard rather than one per loop, because the defect is
//! the same everywhere and seven near-identical source guards would rot. It
//! fails when a NEW one-shot call site appears, and it fails when a listed
//! exception is fixed without being removed from the list, so the list cannot
//! quietly become a graveyard.

use std::path::Path;

/// Loops still on the one-shot, each with the reason it needs judgement rather
/// than a mechanical conversion. Shrinking this list is the work; growing it
/// silently is what this test prevents.
const KNOWN_ONE_SHOT: &[(&str, &str)] = &[
    // Its tick wraps an INNER RETRY LOOP with its own backoff, so "the pass"
    // has no single success arm to stamp: a converted version has to decide
    // whether a tick that succeeded on attempt 3 completed once or three times.
    ("runtime_jobs/scheduler.rs", "SCHEDULER"),
    // `let _ = tick(&home, Some(&store)).await;` discards the result outright,
    // so there is no success arm at all. Converting it means first deciding
    // what failure means for this job, which is a behaviour change, not an
    // instrumentation one.
    ("runtime_jobs/browser_reaper.rs", "BROWSER_REAPER"),
    // Deliberately stamped AFTER the pass already, and its own comment explains
    // why: a SKIP counts as a healthy pass because the loop's job is deciding
    // whether to recompute. Bracketing it needs that intent preserved rather
    // than a mechanical tick_start/tick_end around the recompute.
    ("api/email_intel.rs", "EMAIL_THEMES"),
];

fn src_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// Every `registry::tick(` CALL, with its file. Comments and the definition
/// itself are excluded: a scan that counts prose passes on the description of
/// the code instead of the code, which has bitten this repo repeatedly.
fn one_shot_sites() -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    let mut stack = vec![src_root().join("src")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with("//") || t.starts_with("///") {
                    continue;
                }
                if !line.contains("registry::tick(") {
                    continue;
                }
                // `tick_start(` / `tick_end(` share the prefix; the definition
                // and the string literals inside other guards are not calls.
                if line.contains("tick_start(") || line.contains("tick_end(") {
                    continue;
                }
                if line.contains("contains(") || line.contains("pub fn tick(") {
                    continue;
                }
                let rel = p
                    .strip_prefix(src_root().join("src"))
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, i + 1, t.to_string()));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn the_scan_can_actually_find_a_one_shot() {
    // POSITIVE CONTROL. A scan that silently matches nothing would make the
    // assertion below pass over a tree full of one-shots, which is the failure
    // this whole file exists to prevent. The known list is non-empty today, so
    // finding zero means the scan is broken, not that the work is done.
    let sites = one_shot_sites();
    assert!(
        !sites.is_empty(),
        "the scan found NO one-shot call sites at all. Either every loop is \
         converted (in which case empty KNOWN_ONE_SHOT and delete this control), \
         or the scan is broken and the test below is vacuous"
    );
}

#[test]
fn no_unlisted_loop_uses_the_one_shot_tick() {
    let sites = one_shot_sites();
    let known: Vec<&str> = KNOWN_ONE_SHOT.iter().map(|(f, _)| *f).collect();

    let unlisted: Vec<String> = sites
        .iter()
        .filter(|(f, _, _)| !known.contains(&f.as_str()))
        .map(|(f, l, t)| format!("{f}:{l}  {t}"))
        .collect();
    assert!(
        unlisted.is_empty(),
        "a loop is stamping the one-shot `registry::tick(` and is not in \
         KNOWN_ONE_SHOT. It can never report `slow`, so its slowness will read \
         as `stalled` and somebody will treat a working job as dead. Bracket it \
         with tick_start/tick_end (tick_end in the success arm only), or add it \
         here with the reason it needs judgement:\n  {}",
        unlisted.join("\n  ")
    );

    // THE OTHER DIRECTION, so the list cannot become a graveyard: an entry that
    // no longer has a call site was fixed and should have been deleted.
    let found: Vec<&str> = sites.iter().map(|(f, _, _)| f.as_str()).collect();
    let stale: Vec<&str> = known
        .iter()
        .filter(|f| !found.contains(f))
        .copied()
        .collect();
    assert!(
        stale.is_empty(),
        "KNOWN_ONE_SHOT names files that no longer use the one-shot. They were \
         fixed; delete the entries so the list keeps meaning what it says: {stale:?}"
    );
}
