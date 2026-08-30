//! Every `amux board <verb>` the server tells an agent to run must be a verb the
//! CLI actually dispatches.
//!
//! AMUX-2140 is the incident this guards: every assignment notification told
//! sessions to run `amux board claim <id>`, the verb did not exist, the CLI fell
//! through to its help text and exited 0 — so following the instruction EXACTLY
//! produced a success signal and no claim. Nothing could catch that by being
//! careful, because the instruction and the failure were the same action.
//!
//! AMUX-3707 found two more of the same class in one nudge, three years of
//! firings later: the decompose ask told lanes to "discard it" (the `discard`
//! alias does dispatch, but it was missing from `amux board` help, so it was
//! undiscoverable) and to "set each child's `epic`" (no verb existed at all,
//! while `epic` was a real PATCH field reachable only by hand-rolled curl —
//! which drops the worker header and produces exactly the unattributed writes
//! the ledger depends on not having).
//!
//! So this does not pin one string. It extracts every command the SERVER emits
//! into a lane's context and checks it against the CLI's own dispatch table. A
//! new nudge that names a verb nobody implemented fails here instead of in a
//! lane at 3am.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // crates/amux-server -> up two.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn walk_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk_rs(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// Verbs the bash CLI dispatches, read off its `case` arms.
///
/// Deliberately reads the SCRIPT rather than carrying a copy of the list: a
/// second copy is how `amux board type`'s usage line came to advertise two types
/// the server rejects (AMUX-2479). A hardcoded allowlist here would pass this
/// test forever while the CLI drifted underneath it — the exact "check pinning
/// the wrong layer" failure (AF-161).
fn cli_verbs(root: &Path) -> BTreeSet<String> {
    let src = std::fs::read_to_string(root.join("amux")).expect("read ./amux");
    let mut out = BTreeSet::new();
    for line in src.lines() {
        // Case arms in the verb dispatchers sit at four spaces: `    retitle)`,
        // `    archive|unarchive)`, `    done|doing|todo|backlog|discard|...)`.
        let Some(rest) = line.strip_prefix("    ") else { continue };
        let Some(arm) = rest.strip_suffix(')') else { continue };
        if arm.is_empty() || !arm.chars().all(|c| c.is_ascii_lowercase() || "|_-".contains(c)) {
            continue;
        }
        for v in arm.split('|') {
            if !v.is_empty() {
                out.insert(v.to_string());
            }
        }
    }
    assert!(
        out.len() > 20,
        "parsed only {} case arms from ./amux — the dispatch shape changed and this test is now \
         reading nothing, which would pass silently forever. Fix the parser, do not delete the \
         assert.\nparsed: {out:?}",
        out.len()
    );
    out
}

/// Every `amux board <verb>` literal the server source emits.
fn board_commands_named_by_server(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    walk_rs(&root.join("crates/amux-server/src"), &mut files);
    let re = regex::Regex::new(r"amux board ([a-z][a-z-]*)").expect("re");
    let mut out = Vec::new();
    for f in files {
        let Ok(src) = std::fs::read_to_string(&f) else { continue };
        let rel = f.strip_prefix(root).unwrap_or(&f).display().to_string();
        for c in re.captures_iter(&strip_test_modules(&src)) {
            out.push((c[1].to_string(), rel.clone()));
        }
    }
    out
}

/// The source with every `#[cfg(test)]` module blanked out.
///
/// SHIPPED code only. A test module legitimately names verbs that do not exist:
/// board_drive.rs's `a_prompt_naming_a_nonexistent_board_verb_is_caught` feeds
/// `amux board shwo` to its own guard as a negative control, and flagging that
/// would be this test calling a correct test a defect.
///
/// Cutting each file at its FIRST `#[cfg(test)]` is what this did first, and it
/// was wrong in the dangerous direction: board_drive.rs has four test modules
/// INTERLEAVED with shipped code (the first at line 590 of 5700), so that cut
/// discarded 90% of the file and the sweep quietly stopped covering the very
/// nudge it was written for. My own positive control caught it — which is the
/// whole reason it is there.
///
/// A module ends at the first subsequent line that is exactly `}` at column 0.
/// Verified against all 8 test modules in board_drive.rs and board.rs before
/// this shipped; it needs no lexer, so string literals and char literals inside
/// a test cannot confuse it the way brace-counting would.
fn strip_test_modules(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut skipping = false;
    for line in src.lines() {
        if !skipping && line.starts_with("#[cfg(test)]") {
            skipping = true;
        } else if skipping && line == "}" {
            skipping = false;
        }
        // Whole-line comments are never emitted to a lane, and a comment that
        // EXPLAINS a command trips this sweep exactly as hard as a real one —
        // the note-header fix below was flagged by its own rationale comment.
        // Only a line whose first non-space is `//` is dropped: a trailing
        // comment could not be cut without risking a `//` inside a string
        // literal (a URL), and losing a real command is the worse direction.
        let is_comment = line.trim_start().starts_with("//");
        // Keep the line count stable so any future line-number reporting is honest.
        out.push_str(if skipping || is_comment { "" } else { line });
        out.push('\n');
    }
    out
}

#[test]
fn every_board_command_the_server_tells_a_lane_to_run_actually_dispatches() {
    let root = workspace_root();
    let verbs = cli_verbs(&root);
    let named = board_commands_named_by_server(&root);

    // The probe must be able to find something. An empty `named` would make the
    // loop below vacuous and green — a filter that silently matches nothing looks
    // exactly like a clean pass (ethos rule 7).
    assert!(
        !named.is_empty(),
        "found no `amux board <verb>` strings in crates/amux-server/src — the extractor is \
         broken, not the code"
    );

    // And a POSITIVE control: the specimen this test was written for must be in
    // the extraction. If `epic` stops being found, the extractor regressed.
    assert!(
        named.iter().any(|(v, _)| v == "epic"),
        "the AMUX-3707 specimen (`amux board epic`) is not in the extraction — extractor \
         regressed.\nfound: {:?}",
        named.iter().map(|(v, _)| v.as_str()).collect::<BTreeSet<_>>()
    );

    // ...and the NEGATIVE control, which is the half that proves the test-module
    // stripper actually strips. `shwo` is board_drive.rs's own deliberate fake
    // verb, inside a `#[should_panic]` fixture. If it appears here, the stripper
    // is inert and this test would start filing correct tests as defects; if the
    // stripper over-reaches instead, the `epic` control above goes red. Neither
    // control alone can tell you the window is right — a sweep that matches
    // everything and one that matches nothing both look like a clean pass from
    // the failures list alone.
    assert!(
        !named.iter().any(|(v, _)| v == "shwo"),
        "the negative control leaked: `shwo` is a test fixture's fake verb and must not be \
         read as a shipped instruction — strip_test_modules is not stripping"
    );

    let mut missing: Vec<String> = named
        .iter()
        .filter(|(v, _)| !verbs.contains(v))
        .map(|(v, f)| format!("`amux board {v}` (named in {f})"))
        .collect();
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "the server tells a lane to run {} command(s) the CLI does not dispatch. Following the \
         instruction literally hits the help text and exits 0 — a success signal and no action \
         (AMUX-2140).\n  {}\nCLI dispatches: {:?}",
        missing.len(),
        missing.join("\n  "),
        verbs
    );
}

#[test]
fn every_board_verb_the_nudges_name_is_discoverable_in_help() {
    // `discard` dispatched for months and was absent from `amux board` help, so
    // the one command the capture-shell nudge most needed could not be found by
    // anyone who did not already know it (AMUX-3707). Dispatching is necessary;
    // being findable is what makes it the EASY path, which is the only thing that
    // keeps agents off hand-rolled curl (ethos rule 6).
    let root = workspace_root();
    let src = std::fs::read_to_string(root.join("amux")).expect("read ./amux");
    let help: String = src
        .lines()
        .filter(|l| l.trim_start().starts_with("amux board "))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        help.contains("amux board ls"),
        "did not locate the board help block — the extractor is reading nothing"
    );

    let named = board_commands_named_by_server(&root);
    let mut undocumented: Vec<String> = named
        .iter()
        .filter(|(v, _)| !help.contains(&format!("amux board {v} ")))
        .map(|(v, _)| v.clone())
        .collect();
    undocumented.sort();
    undocumented.dedup();
    assert!(
        undocumented.is_empty(),
        "these verbs are named in server-emitted instructions but do not appear in `amux board` \
         help, so a lane cannot discover them: {undocumented:?}"
    );
}
