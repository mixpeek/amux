//! Every server call in the bash CLI must be unable to HANG (2026-08-23, AEAB-46).
//!
//! `set -euo pipefail` and a `die` that names both lost facts (AEAB-36) are
//! already in `amux`, and neither of them can fire while curl is still waiting:
//! without `--connect-timeout`, an unreachable server does not fail, it hangs.
//! Measured on this machine — a dead localhost port drops the SYN rather than
//! refusing it, so `amux board done <id> --outcome ...` sat until its CALLER
//! timed out (2 minutes) and printed nothing at all. The same command against a
//! host that fails FAST (DNS, curl exit 6) printed AEAB-36's message correctly.
//! One instrument, two shapes of unreachable server, and it could only report
//! the shape that was already easy to see.
//!
//! Not one of the CLI's 41 curl call sites carried a connect timeout; 33 carried
//! no timeout of any kind and 8 carried only `-m`. That count is the reason this
//! is a guard and not 41 edits: a per-call-site flag is a thing every future call
//! site has to remember, and the record is that 41 of 41 did not remember it. So
//! the rule is structural — a curl invocation either goes through
//! `_curl` (which injects the connect timeout and breadcrumbs the failure), or
//! it names `--connect-timeout` itself on the same line.
//!
//! `-m`/`--max-time` deliberately does NOT satisfy this guard. It caps the whole
//! transfer, so it is the wrong knob for "the server is not there" and the right
//! one for "this response is slow": `amux peek --live` and `amux send` may
//! legitimately take a while, and the eight sites that already had `-m` still
//! hung for their full budget on a dropped connect.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    // tests/ -> amux-server/ -> crates/ -> repo root
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root above crates/amux-server")
        .to_path_buf()
}

/// A `curl` token that is a real invocation, as opposed to the word appearing in
/// prose or inside a string the CLI prints as a recipe.
///
/// The discriminator is the character immediately before the token: a command
/// starts a line or follows a shell operator. `_curl` is excluded because the
/// preceding character is a word character — that is the wrapper, which is the
/// whole point of the fix.
fn is_invocation(line: &str, at: usize) -> bool {
    let before = line[..at].trim_end();
    match before.chars().last() {
        None => true,
        Some('(' | '|' | '&' | ';' | '{' | '!') => true,
        // A shell keyword can precede a command too, and `if curl ...` is where
        // three of the eight pre-existing `-m` sites live — a guard that missed
        // them would have reported the file as clean while the `if` form went on
        // being written without a connect timeout.
        _ => matches!(
            before.rsplit([' ', '\t']).next().unwrap_or(""),
            "if" | "elif" | "while" | "until" | "then" | "else" | "do" | "command"
        ),
    }
}

#[test]
fn every_curl_in_the_bash_cli_cannot_hang_on_connect() {
    let path = repo_root().join("amux");
    let src = std::fs::read_to_string(&path).expect("read the bash CLI");

    let mut offenders: Vec<(usize, String)> = Vec::new();
    let mut wrapper_calls = 0usize;

    for (i, raw) in src.lines().enumerate() {
        let lineno = i + 1;
        let trimmed = raw.trim_start();
        // Comments are prose ABOUT curl. There is a lot of it in this file and
        // it is history, not traffic — guarding it would make the guard noise.
        if trimmed.starts_with('#') {
            continue;
        }
        // Lines that PRINT a recipe (`echo "  curl -sk ..."`) are documentation
        // handed to a human, not a call this CLI makes.
        if trimmed.starts_with("echo ")
            || trimmed.starts_with("printf ")
            || trimmed.starts_with("die ")
        {
            continue;
        }
        if raw.contains("_curl ") {
            wrapper_calls += 1;
        }
        for (at, _) in raw.match_indices("curl ") {
            if !is_invocation(raw, at) {
                continue;
            }
            // `command curl` is the sanctioned escape from the wrapper (the
            // wrapper itself, and the breadcrumb flush, which must not
            // breadcrumb its own failure). It pays the flag explicitly.
            if raw.contains("--connect-timeout") {
                continue;
            }
            offenders.push((lineno, raw.trim().to_string()));
        }
    }

    assert!(
        offenders.is_empty(),
        "curl invocations that can hang forever on an unreachable server \
         (route them through _curl, or pass --connect-timeout on the line):\n{}",
        offenders
            .iter()
            .map(|(n, l)| format!("  amux:{n}: {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        wrapper_calls > 20,
        "expected the CLI's server calls to go through _curl; found {wrapper_calls} \
         — did the wrapper get reverted, or did this guard stop matching it?"
    );
}
