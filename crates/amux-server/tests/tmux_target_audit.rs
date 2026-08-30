//! Every tmux `-t` target in the crate must come from the exact-match L2
//! helpers (`session_target` / `pane_target`, reached as `st`/`pt`/`stq`/`ptq`).
//!
//! This is a SOURCE audit rather than a behavioural test on purpose: the
//! failure it guards cannot be reproduced on demand. tmux resolves a bare `-t
//! foo` by PREFIX, and `amux-amux` is a prefix of `amux-amux-frustrations`,
//! `amux-amux-rust`, `amux-amux-cloud` and five more. A non-exact target is
//! therefore correct every single time the exact session exists, and silently
//! addresses a SIBLING's pane only in the window where it does not — which is
//! precisely a restart, a rename, or a start/stop race. The 2026-08-09
//! `amux-frustrations.log` carried another session's launch command and a
//! third session's nudge text from exactly such a window (AMUX-1888 is the
//! same hazard class in the CLI).
//!
//! So the check is: you cannot merge a hand-spelled target. If you need a new
//! target shape, add it to the helpers in `backend/tmux.rs` and it is covered
//! everywhere at once.

use std::path::{Path, PathBuf};

/// Identifiers that are, by construction, `session_target()`/`pane_target()`
/// output. Deliberately a SHORT closed list — the point is that the set of
/// ways to name a pane stays small enough to audit by eye.
const ALLOWED: &[&str] = &["st", "pt", "stq", "ptq"];

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            // tests/ may legitimately build literal targets for throwaway
            // sessions it created itself.
            if p.file_name().and_then(|s| s.to_str()) == Some("tests") {
                continue;
            }
            rust_sources(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// The expression following a `"-t",` in an argv array, normalised: leading
/// `&`, and a trailing `.as_str()` / `.clone()` accessor, are not part of the
/// identity of the value.
fn normalise(expr: &str) -> String {
    let e = expr.trim().trim_start_matches('&').trim();
    let e = e.split('.').next().unwrap_or(e);
    e.trim().to_string()
}

/// Walk one source text and return every non-exact `-t` target as
/// `(byte offset of the "-t" literal, normalised target expression)`.
///
/// AEAB-23. This loop used to exist TWICE: here, over real files, and again
/// inline inside `the_audit_detects_a_planted_non_exact_target`, over a string
/// fixture. Two copies meant the test could not observe a change to the real
/// scanner — and one had already slipped through: 8db43264 added the
/// `Command::new("x")` exemption below and nothing tested it, because the
/// planted test simply did not contain that logic and passed identically with
/// or without it.
///
/// Simulating what you believe a function does cannot catch that function doing
/// something else (ethos rule 7). One implementation, both callers.
///
/// Pure text-in/values-out so a test can drive it with a fixture; the file
/// walking and message formatting stay in `offenders()`.
fn scan(src: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = src[from..].find("\"-t\"") {
        let at = from + rel;
        from = at + 4;
        // Skip a `-t` that belongs to a NON-tmux external command in the
        // SAME statement — e.g. `Command::new("touch").args(["-t", stamp,…])`
        // setting a file mtime in a #[cfg(test)] module (email.rs:1330).
        // Only tmux's -t names a session/pane; `touch -t` is a timestamp.
        // Real tmux -t calls go through the tmux()/self.run() helpers and
        // carry no Command::new in the statement, so they are unaffected;
        // a literal `Command::new("tmux")` is still audited (name == tmux).
        let before = &src[..at];
        let stmt_start = before
            .rfind([';', '{', '}'])
            .map(|i| i + 1)
            .unwrap_or(0);
        if let Some(cn) = before[stmt_start..].rfind("Command::new(") {
            let after = &before[stmt_start + cn + "Command::new(".len()..];
            let name = after.trim_start().strip_prefix('"').and_then(|s| s.split('"').next());
            if matches!(name, Some(n) if n != "tmux") {
                continue;
            }
        }
        // Skip the separator after the literal, then take the argument up
        // to the next `,` / `]` / `)` at this nesting level.
        let rest = &src[from..];
        let Some(comma) = rest.find(',') else { continue };
        let tail = &rest[comma + 1..];
        let mut depth = 0i32;
        let mut end = tail.len();
        for (i, c) in tail.char_indices() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    if depth == 0 {
                        end = i;
                        break;
                    }
                    depth -= 1;
                }
                ',' if depth == 0 => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        let expr = normalise(&tail[..end]);
        if ALLOWED.contains(&expr.as_str()) {
            continue;
        }
        found.push((at, expr));
    }
    found
}

fn offenders() -> Vec<String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&root, &mut files);
    assert!(!files.is_empty(), "found no sources to audit under {}", root.display());

    let mut bad = Vec::new();
    for f in files {
        let src = std::fs::read_to_string(&f).unwrap_or_default();
        for (at, expr) in scan(&src) {
            let line = src[..at].matches('\n').count() + 1;
            bad.push(format!(
                "{}:{line}: tmux -t target `{expr}` is not one of {ALLOWED:?} \
                 (build it with session_target()/pane_target())",
                f.display()
            ));
        }
    }
    bad
}

#[test]
fn every_tmux_target_uses_the_exact_match_helpers() {
    let bad = offenders();
    assert!(
        bad.is_empty(),
        "hand-spelled tmux target(s) found — a non-exact `-t` lands in a \
         SIBLING session's pane whenever the exact session is briefly absent:\n{}",
        bad.join("\n")
    );
}

/// The audit is only worth having if it can fail, and a source-scanning check
/// is exactly the kind that silently matches nothing after a refactor renames
/// something (ethos rule 7 — "can your check actually fail?"). So prove the
/// scanner FINDS a planted offender rather than trusting that it would.
#[test]
fn the_audit_detects_a_planted_non_exact_target() {
    // Same text shape the scanner walks in a real source file, including the
    // prefix-matching target that motivated the rule. Drives the SHIPPED `scan`
    // rather than a copy of it (AEAB-23) — this test used to re-implement the
    // loop inline, which is why the `Command::new` exemption below went in
    // untested.
    let planted = r#"
        let _ = tmux(&["pipe-pane", "-t", &format!("amux-{name}"), &cmd]).await;
        let _ = tmux(&["send-keys", "-t", "amux-amux", "Enter"]).await;
        let _ = tmux(&["kill-session", "-t", &stq]).await;
    "#;
    let found: Vec<String> = scan(planted).into_iter().map(|(_, e)| e).collect();
    assert_eq!(
        found.len(),
        2,
        "the scanner must flag BOTH planted offenders and leave `stq` alone; got {found:?}"
    );
    assert!(found.iter().any(|f| f.contains("format!")), "missed the format! target: {found:?}");
    assert!(found.iter().any(|f| f.contains("\"amux-amux\"")), "missed the literal prefix target: {found:?}");
}

/// The `Command::new("x")` exemption (8db43264), in BOTH directions.
///
/// It shipped untested: the planted test above re-implemented the scanner, so
/// nothing exercised the exemption at all. It is the kind of change that is easy
/// to get subtly wrong in the expensive direction — an exemption that is a shade
/// too broad silences the guard while leaving `main` green, which is
/// indistinguishable from the guard working.
///
/// So each case below asserts a different half:
///   - `touch -t` must be exempt          (the false positive that failed CI)
///   - `Command::new("tmux")` must NOT be (or the fix deleted the guard)
///   - helper-invoked tmux must NOT be    (the fail-safe: nothing to attribute)
#[test]
fn the_non_tmux_exemption_silences_touch_but_never_tmux() {
    // The real specimen, reduced from integrations/email.rs. `touch -t` is a
    // timestamp, not a pane; flagging it failed main and made every open PR
    // inherit a red check.
    let touch = r#"
        let set = |name: &str, stamp: &str| {
            std::process::Command::new("touch")
                .args(["-t", stamp, tok.join(format!("{name}.json")).to_str().unwrap()])
                .status()
                .unwrap();
        };
    "#;
    assert!(scan(touch).is_empty(), "touch -t is a timestamp, not a pane: {:?}", scan(touch));

    // CONTROL. Same shape, program `tmux`: still an offender.
    let tmux_cmd = r#"
        std::process::Command::new("tmux")
            .args(["send-keys", "-t", "amux-amux", "Enter"])
            .status()
            .unwrap();
    "#;
    let f: Vec<String> = scan(tmux_cmd).into_iter().map(|(_, e)| e).collect();
    assert_eq!(f.len(), 1, "a literal Command::new(\"tmux\") target must still be flagged: {f:?}");
    assert!(f[0].contains("amux-amux"), "wrong expression captured: {f:?}");

    // FAIL-SAFE. api/metrics.rs's real shape: tmux reached through a helper, so
    // there is no literal program to attribute. Absent positive evidence the site
    // stays audited — this is the case a broader exemption would silently lose.
    let helper = r#"
        let _ = cmd_output("tmux", &["list-panes", "-t", "amux-amux", "-F", "x"]);
    "#;
    assert_eq!(scan(helper).len(), 1, "a helper-invoked tmux target must stay audited");

    // And an ALLOWED target reached the same way is still allowed, so the guard
    // is discriminating on the expression rather than on the call shape.
    let ok = r#"
        let _ = cmd_output("tmux", &["list-panes", "-t", &pt, "-F", "x"]);
    "#;
    assert!(scan(ok).is_empty(), "pane_target() output must pass: {:?}", scan(ok));
}
