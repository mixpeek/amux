//! `pub` items in a lib crate are invisible to `dead_code` — this is the check
//! that can fail (AMUX-2910).
//!
//! On 2026-08-11 `api/py_proxy.rs` was found carrying an entire python
//! forwarder — `forward_built`, `forward_to_python`, `forward_handler`,
//! `passthrough_routes` — with ZERO call sites anywhere. Not merely dead: its
//! default target had become this same process on the retired legacy bind, so a
//! future proxy row would have self-proxy-looped. `cargo clippy --workspace
//! --all-targets -- -D warnings`, the standing pre-push gate that CI also
//! enforces, passed on it every single run, because rustc treats a `pub` item
//! in a lib crate as reachable by definition. amux-server is almost entirely
//! `pub`, so the gate that is supposed to catch exactly this is blind to all of
//! it.
//!
//! **WHAT THIS TEST IS NOT.** It matches BARE NAMES across the workspace, so a
//! function mentioned only in a comment, or sharing a name with an unrelated
//! identifier, counts as referenced. That direction is deliberate — it makes
//! false ALARMS very unlikely and false CLEARS possible — so the result is a
//! FLOOR on dead code, never a proof of its absence. A green run means "no
//! NEW obviously-dead pub fn", not "nothing here is dead".
//!
//! It is worth having anyway: the forwarder would have been caught by it, and
//! measured against the whole crate the answer is small enough to act on (496
//! `pub fn`, 1 unreferenced), which is what makes a hard assertion affordable
//! rather than a wall of noise somebody force-merges past.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Known-unreferenced, with the reason — same contract as
/// `legacy_port_guard.rs`'s ALLOW: a row must carry why it is exempt, and the
/// test below fails if a row stops being true, so the list cannot go stale.
// The list is EMPTY today. Its one row ("from_env", the herdr backend's) left
// on 2026-08-22: the guard keys on the bare fn name, and CliS3Publisher::from_env
// (SA-124) is genuinely referenced — so the name is now called somewhere and the
// row is stale by the guard's own rule. If the herdr from_env goes unreferenced
// again after a rename, it re-earns its row with its reason.
const ALLOW: &[(&str, &str)] = &[];

fn rs_files(root: &str) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if !matches!(
                    p.file_name().and_then(|s| s.to_str()).unwrap_or(""),
                    "target" | "node_modules" | ".git"
                ) {
                    walk(&p, out);
                }
            } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
                out.push(p);
            }
        }
    }
    let mut v = Vec::new();
    walk(&repo_root().join(root), &mut v);
    v
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// `pub fn name` / `pub async fn name` at the start of a line (ignoring
/// indentation). Not `pub(crate)` — those ARE covered by `dead_code`, which is
/// the whole asymmetry this test exists for.
fn pub_fn_names(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in src.lines() {
        let t = line.trim_start();
        let rest = t
            .strip_prefix("pub async fn ")
            .or_else(|| t.strip_prefix("pub fn "));
        if let Some(rest) = rest {
            let name: String =
                rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            if !name.is_empty() {
                out.push(name);
            }
        }
    }
    out
}

/// Occurrences of `name` as a whole word.
fn word_count(hay: &str, name: &str) -> usize {
    let is_w = |c: char| c.is_alphanumeric() || c == '_';
    let mut n = 0;
    let b = hay.as_bytes();
    for (i, _) in hay.match_indices(name) {
        let before_ok = i == 0 || !is_w(b[i - 1] as char);
        let j = i + name.len();
        let after_ok = j >= b.len() || !is_w(b[j] as char);
        if before_ok && after_ok {
            n += 1;
        }
    }
    n
}

/// The census itself, over an explicit corpus so the detector is testable
/// (`the_detector_can_actually_fail` below feeds it a planted case).
fn unreferenced(defs: &BTreeMap<String, usize>, corpus: &[String]) -> Vec<String> {
    defs.iter()
        .filter(|(name, def_sites)| {
            let total: usize = corpus.iter().map(|t| word_count(t, name)).sum();
            total <= **def_sites
        })
        .map(|(n, _)| n.clone())
        .collect()
}

#[test]
fn no_new_unreferenced_pub_fn_in_amux_server() {
    let src = rs_files("crates/amux-server/src");
    let mut defs: BTreeMap<String, usize> = BTreeMap::new();
    for p in &src {
        let t = std::fs::read_to_string(p).unwrap_or_default();
        for n in pub_fn_names(&t) {
            *defs.entry(n).or_insert(0) += 1;
        }
    }
    assert!(defs.len() > 100, "found only {} pub fns — the scanner is broken, not the code", defs.len());

    // THIS FILE IS EXCLUDED FROM ITS OWN CORPUS. Every ALLOW row names a
    // function, so the allowlist is itself a bare-word reference to each name it
    // exempts — leaving it in made `from_env` look referenced and the first run
    // failed with "ALLOW rows that are now REFERENCED". An instrument that
    // measures itself reads its own output as data (frustrations.md's header
    // indents its template for exactly this reason).
    let me = Path::new(file!()).file_name().unwrap_or_default().to_owned();
    let corpus: Vec<String> = rs_files("crates")
        .iter()
        .filter(|p| p.file_name().unwrap_or_default() != me)
        .map(|p| std::fs::read_to_string(p).unwrap_or_default())
        .collect();

    let found = unreferenced(&defs, &corpus);
    let allowed: Vec<&str> = ALLOW.iter().map(|(n, _)| *n).collect();

    let new: Vec<&String> = found.iter().filter(|n| !allowed.contains(&n.as_str())).collect();
    assert!(
        new.is_empty(),
        "unreferenced `pub fn` in amux-server (dead_code cannot see these — a `pub` item in a \
         lib crate is reachable by definition):\n{}\n\nDelete them, demote to `pub(crate)` so \
         the compiler polices them for free, or add to ALLOW with the reason.",
        new.iter().map(|n| format!("  - {n}")).collect::<Vec<_>>().join("\n")
    );

    // The mirror: an ALLOW row that is no longer unreferenced is stale and must
    // go, or the list slowly becomes a place where live code hides.
    let stale: Vec<&str> =
        allowed.iter().copied().filter(|a| !found.iter().any(|f| f == a)).collect();
    assert!(
        stale.is_empty(),
        "ALLOW rows that are now REFERENCED (or gone) — delete them: {stale:?}"
    );
}

#[test]
fn the_detector_can_actually_fail() {
    // Planted corpus: `orphan` is defined and never called; `used` is called
    // once elsewhere. Without this, a census that returned an empty vec for any
    // input would look identical to a clean codebase.
    let src = "pub fn orphan() {}\npub async fn used() {}\n";
    let names = pub_fn_names(src);
    assert_eq!(names, vec!["orphan".to_string(), "used".to_string()]);

    let mut defs = BTreeMap::new();
    defs.insert("orphan".to_string(), 1usize);
    defs.insert("used".to_string(), 1usize);
    let corpus = vec![src.to_string(), "fn caller() { used(); }".to_string()];

    let out = unreferenced(&defs, &corpus);
    assert_eq!(out, vec!["orphan".to_string()], "must flag the orphan and clear the used one");

    // Word boundaries: `used_elsewhere` must not count as a reference to `used`
    // — the substring bug that makes a census silently clear real dead code.
    let out2 = unreferenced(&defs, &[src.to_string(), "fn f() { used_elsewhere(); }".into()]);
    assert!(out2.contains(&"used".to_string()), "substring must not count as a reference");
}
