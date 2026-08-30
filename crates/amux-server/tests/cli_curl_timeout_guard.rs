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

/// Does this ONE line carry a curl that could hang? Split out of the scan so
/// the rule can be exercised on synthetic lines.
///
/// It has to be, because `amux` is a SYMLINK into the working tree and a SAVE
/// there is a fleet-wide deploy (AF-237). Mutating the real CLI to check that
/// this guard still discriminates would mean shipping a deliberately-broken CLI
/// to 52 lanes for as long as the test takes to run. So the file scan below
/// stays a thin loop, and the judgment lives here where a test can feed it
/// whatever it likes.
fn line_offends(raw: &str) -> bool {
    let trimmed = raw.trim_start();
    // Comments are prose ABOUT curl. There is a lot of it in this file and it
    // is history, not traffic — guarding it would make the guard noise.
    if trimmed.starts_with('#') {
        return false;
    }
    // Lines that PRINT a recipe (`echo "  curl -sk ..."`) are documentation
    // handed to a human, not a call this CLI makes.
    if trimmed.starts_with("echo ") || trimmed.starts_with("printf ") || trimmed.starts_with("die ")
    {
        return false;
    }
    for (at, _) in raw.match_indices("curl ") {
        if !is_invocation(raw, at) {
            continue;
        }
        // PROSE AFTER A SHELL OPERATOR IS STILL PROSE (AMUX-3761's by-catch).
        // The exemption above is anchored on line-START, but the idiom in this
        // CLI is `<check> || die "... (curl exit $rc) ..."`, where `die` follows
        // `||` and the word `curl` sits inside its MESSAGE. `is_invocation`
        // already knows a curl token can follow a shell operator; the prose test
        // did not get the same treatment, so it fired on an error string while
        // the real call two lines above went correctly through `_curl`.
        //
        // This mattered more than a red test: the natural way to make a false
        // positive go away is to reword the message, and this guard's whole
        // reason for existing is that AEAB-36's connect failures were
        // unreadable. A check that pressures people into vaguer error text is
        // working against its own purpose.
        //
        // Scoped to text BEFORE the token on purpose. `curl ... || die "x"` has
        // `die` on the line and is a REAL invocation, so looking at the whole
        // line here would exempt exactly the thing the guard is for.
        if raw[..at].contains("die ")
            || raw[..at].contains("echo ")
            || raw[..at].contains("printf ")
        {
            continue;
        }
        // `command curl` is the sanctioned escape from the wrapper (the wrapper
        // itself, and the breadcrumb flush, which must not breadcrumb its own
        // failure). It pays the flag explicitly.
        if raw.contains("--connect-timeout") {
            continue;
        }
        return true;
    }
    false
}

/// The guard's own controls. A widened exemption is the moment to ask whether
/// the check can still fail, and this one was widened to stop it firing on an
/// error message.
#[test]
fn the_guard_still_catches_a_real_hang_and_leaves_prose_alone() {
    // MUST FIRE.
    for line in [
        r#"  resp=$(curl -sk "$api/api/board")"#,
        r#"  if curl -sk "$api/health" >/dev/null; then"#,
        // The direction the new exemption must NOT swallow: a real call whose
        // failure handler happens to mention curl.
        r#"  curl -sk "$api/x" || die "cannot reach the server (curl exit $rc)""#,
    ] {
        assert!(line_offends(line), "a curl that can hang must be caught: {line}");
    }
    // MUST NOT FIRE.
    for line in [
        r#"  resp=$(_curl -sk "$api/api/board")"#,
        r#"  command curl --connect-timeout 3 -sk "$api/health""#,
        r#"# curl -sk $AMUX_URL/api/board is the raw form"#,
        r#"  echo "  curl -sk $AMUX_URL/api/board""#,
        // The AMUX-3761 specimen, verbatim in shape.
        r#"  [[ "$rc" -eq 0 ]] || die "cannot reach the amux server at $api (curl exit $rc) — NOTHING was changed""#,
    ] {
        assert!(!line_offends(line), "prose and wrapped calls must be left alone: {line}");
    }
}

#[test]
fn every_curl_in_the_bash_cli_cannot_hang_on_connect() {
    let path = repo_root().join("amux");
    let src = std::fs::read_to_string(&path).expect("read the bash CLI");

    let mut offenders: Vec<(usize, String)> = Vec::new();
    let mut wrapper_calls = 0usize;

    for (i, raw) in src.lines().enumerate() {
        if raw.contains("_curl ") {
            wrapper_calls += 1;
        }
        if line_offends(raw) {
            offenders.push((i + 1, raw.trim().to_string()));
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
