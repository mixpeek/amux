//! A browser error body must carry its CAUSE, not just its outermost frame
//! (AMUX-3886).
//!
//! `anyhow::Error::to_string()` prints one frame and drops the source chain. For
//! the `reqwest` errors under `cdp_list` that frame carries no diagnosis at all:
//!
//! ```text
//! to_string(): error sending request for url (http://127.0.0.1:49731/json/list)
//! {e:#}:       error sending request for url (http://127.0.0.1:49731/json/list):
//!              client error (Connect): tcp connect error: Connection refused (os error 61)
//! ```
//!
//! The first string is what two 502s from `general-canvas-apps` left on record
//! on 2026-08-29, and it is IDENTICAL for a refused connect, a DNS failure and a
//! 3s timeout. Three faults, three different fixes, one indistinguishable body.
//!
//! The unit test beside `cdp_list` pins the RENDERING. This pins the CALL SITES:
//! `api/browser.rs` builds ~28 error bodies, every one of them was written with
//! `to_string()`, and nothing stops the twenty-ninth being written the same way.
//! A fix that only holds for the sites that existed on the day it landed is the
//! shape ethos rule 7 is about.
//!
//! # Scope, said out loud
//!
//! `api/browser.rs` only, and only `to_string()` applied to an identifier that
//! names an error. A cause-dropping render in another module, or one written
//! through a differently-named binding, passes here. That is deliberate: this is
//! the file whose bodies are the browser API's contract with its callers.

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

const FILE: &str = "crates/amux-server/src/api/browser.rs";

/// Bindings this file uses for a caught error. `to_string()` on any of them
/// drops the chain.
const ERROR_BINDINGS: [&str; 3] = ["e", "err", "first"];

/// Lines outside every `#[cfg(test)]` module, plus the count of what was
/// skipped so an over-eager skip cannot pass by emptiness.
///
/// Test code is excluded deliberately: this guard is about the bodies the API
/// RETURNS, and the `with_cause` unit test in `api/browser.rs` renders an error
/// with plain Display ON PURPOSE, as the control proving the two renderers still
/// differ. Flagging that control would force the test to delete the very thing
/// that makes it meaningful.
fn production_lines(src: &str) -> (Vec<(usize, &str)>, usize) {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let mut in_test_mod = false;
    for (i, line) in src.lines().enumerate() {
        if !in_test_mod && line.trim_end() == "#[cfg(test)]" {
            in_test_mod = true;
        }
        if in_test_mod {
            skipped += 1;
            // Top-level test modules close with a `}` in column 0.
            if line == "}" {
                in_test_mod = false;
            }
            continue;
        }
        out.push((i + 1, line));
    }
    (out, skipped)
}

#[test]
fn no_browser_error_body_renders_without_its_cause() {
    let src = std::fs::read_to_string(workspace_root().join(FILE)).expect("read api/browser.rs");
    let (lines, skipped) = production_lines(&src);

    // THE COUNT BESIDE THE ZERO. A skip rule that ran away would leave this
    // test scanning nothing and passing, which reads identically to a real pass.
    assert!(
        lines.len() > skipped,
        "more of {FILE} was skipped as test code ({skipped} lines) than scanned ({}) — the \
         `#[cfg(test)]` skip has run away and this guard is checking almost nothing",
        lines.len()
    );

    let mut offenders: Vec<String> = Vec::new();
    for (lineno, line) in lines {
        // The doc comment on `with_cause` quotes the banned form on purpose.
        if line.trim_start().starts_with("//") {
            continue;
        }
        for b in ERROR_BINDINGS {
            let pat = format!("{b}.to_string()");
            let Some(at) = line.find(&pat) else { continue };
            // `profile.to_string()` ends in "e.to_string()"; require the binding
            // to start at a word boundary.
            let boundary = at == 0
                || !line[..at].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_');
            if boundary {
                offenders.push(format!("{}:{}: {}", FILE, lineno, line.trim()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "these render an error WITHOUT its cause — use `with_cause(&e)`, which formats the whole \
         chain, so a reader can tell a refused connect from a timeout (AMUX-3886):\n{}",
        offenders.join("\n")
    );
}

/// CONTROL: the check must be able to fail. If `with_cause` were deleted and
/// every site rewritten, the test above would go green by emptiness rather than
/// by correctness — a check pinning nothing reads exactly like a passing one.
#[test]
fn the_guard_is_actually_looking_at_something() {
    let src = std::fs::read_to_string(workspace_root().join(FILE)).expect("read api/browser.rs");
    let n = src.matches("with_cause(&").count();
    assert!(
        n >= 20,
        "expected api/browser.rs to render its error bodies through `with_cause`, found {n} call \
         sites. Either the helper was removed (then this guard protects nothing) or the file was \
         split (then point this test at the new location)."
    );
    assert!(
        src.contains("fn with_cause("),
        "`with_cause` is the sanctioned renderer and must exist in this file"
    );
}

/// AMUX-3886 follow-up. Both recovery counters must appear in BOTH arms of
/// `status`, because they vanished in exactly the state they describe.
///
/// A browser that died leaves `running: false`, and "how many times did a verb
/// find a corpse" is the question asked AFTER that, not during. The live
/// endpoint returned three keys — running, last_exit, last_exit_note — while the
/// counter this card added sat in the other branch, unreachable. A zero you
/// cannot read and a field that is absent are the same thing to a caller.
///
/// Pinned as a source check rather than a request test because reaching the
/// not-running arm for real means stopping whatever browser a peer is holding.
#[test]
fn both_status_arms_publish_the_recovery_counters() {
    let src = std::fs::read_to_string(workspace_root().join(FILE)).expect("read api/browser.rs");
    for counter in ["stale_binding_recoveries", "dead_browser_recoveries"] {
        let n = src.matches(&format!("\"{counter}\":")).count();
        assert!(
            n >= 2,
            "`{counter}` is emitted {n} time(s) in {FILE}; it must appear in the running arm \
             AND the `running: false` arm. A counter absent from the not-running payload is \
             invisible in the state it exists to report (AMUX-3886)."
        );
    }
}
