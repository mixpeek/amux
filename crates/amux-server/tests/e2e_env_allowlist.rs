//! serve-head.sh's env scrub must preserve every AMUX_* key playwright's
//! config passes to the webServer (AC-404, second half).
//!
//! The scrub exists because the fleet shell exports the shared server.env and
//! effective_env() falls back to the process env, so a "throwaway" e2e home
//! inherited fleet toggles (AMUX_TASK_GUARD=1 redded settings_task_guard
//! locally while clean-env CI passed). But the scrub's allowlist and the
//! config's env block live in different files, and the first scrub shipped
//! missing AMUX_RS_NO_LOOPBACK_BYPASS — so every CI e2e server auth-bypassed
//! loopback and six bad-token assertions became checks that cannot pass (run
//! 33525151971). Six unrelated spec failures is the WRONG place for that
//! drift to surface; this file is the right one: add a key to the config
//! without adding it to the scrub's case line and the check job names it.
//!
//! Text scan, so per ethos rule 7 it carries a negative control: a key the
//! config does not pass must NOT satisfy the check, or a pass here proves
//! nothing.

use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root above crates/amux-server")
        .to_path_buf()
}

/// AMUX_* keys the playwright config passes into the webServer's env.
fn config_env_keys(config: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in config.lines() {
        let t = line.trim();
        // Keys inside the `env: {` block are written `AMUX_X: value`.
        if let Some(rest) = t.strip_prefix("AMUX_") {
            if let Some(colon) = rest.find(':') {
                let key = format!("AMUX_{}", &rest[..colon]);
                if key.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
                    out.push(key);
                }
            }
        }
    }
    out
}

/// Does serve-head.sh's scrub case line preserve `key`?
fn scrub_preserves(serve_head: &str, key: &str) -> bool {
    // The allowlist is the one case pattern line ending in `) ;;` that names
    // AMUX_HOME. Matching the real syntax rather than a paraphrase: patterns
    // are |-separated, `AMUX_E2E_*` is a glob.
    let Some(pat_line) = serve_head
        .lines()
        .find(|l| l.trim_end().ends_with(") ;;") && l.contains("AMUX_HOME"))
    else {
        return false;
    };
    let pats = pat_line.trim().trim_end_matches(") ;;");
    pats.split('|').any(|p| {
        let p = p.trim();
        match p.strip_suffix('*') {
            Some(prefix) => key.starts_with(prefix),
            None => key == p,
        }
    })
}

#[test]
fn every_config_env_key_survives_the_serve_head_scrub() {
    let root = repo_root();
    let config = std::fs::read_to_string(root.join("e2e/playwright.config.ts"))
        .expect("read e2e/playwright.config.ts");
    let serve_head =
        std::fs::read_to_string(root.join("e2e/serve-head.sh")).expect("read e2e/serve-head.sh");

    let keys = config_env_keys(&config);
    assert!(
        keys.len() >= 3,
        "the config's env block should pass at least AMUX_HOME, AMUX_RS_PORT and \
         AMUX_RS_NO_LOOPBACK_BYPASS; the extractor found {keys:?} — if the env block \
         moved or changed shape, fix config_env_keys, do not delete this test"
    );

    // NEGATIVE CONTROL: a fleet toggle the scrub exists to eat must NOT read
    // as preserved, or the positive assertions below prove nothing.
    assert!(
        !scrub_preserves(&serve_head, "AMUX_TASK_GUARD"),
        "the scrub must eat AMUX_TASK_GUARD — if this now passes the allowlist, the \
         scrub no longer scrubs the very key it was written for"
    );

    let eaten: Vec<_> = keys
        .iter()
        .filter(|k| !scrub_preserves(&serve_head, k))
        .collect();
    assert!(
        eaten.is_empty(),
        "playwright.config.ts passes env keys that serve-head.sh's scrub will unset \
         before the server boots — every spec relying on them fails in a shape that \
         does not name this cause. Add them to the scrub's allowlist case line: {eaten:?}"
    );
}
