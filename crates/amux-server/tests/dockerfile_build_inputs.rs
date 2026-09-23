//! The cloud image build fails when an `include_str!` in amux-server reaches a
//! repo file the Dockerfile does not COPY into its build context. It has broken
//! the cloud deploy TWICE and each time stayed invisible until a deploy actually
//! RAN the build: `check` CI compiles from the FULL checkout where the file
//! exists, but the Docker build context only carries what the Dockerfile COPYs.
//!   - 2026-08-12: `include_str!("../../../../amux")` (the bash CLI) -> COPY amux
//!   - 2026-08-15: include_str! of scripts/git-hooks/git-shared-guard.py +
//!     scripts/hooks/hook-report.sh -> COPY scripts (this test's motivating bug)
//!
//! This test runs in the `check` job, so a NEW external include_str! that the
//! Dockerfile does not cover fails HERE, on a green checkout, instead of on the
//! first green-main deploy build. It is the class-kill for a bug whose whole
//! nature is that it is latent until a deploy.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <repo>/crates/amux-server
    normalize(&Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(".."))
}

/// Resolve `..`/`.` lexically, no filesystem touch (the target need not exist).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The repo-top-level names an `include_str!` under `<crate_manifest>/src/`
/// resolves to, EXCLUDING anything under `crates/` (that rides in via
/// `COPY crates crates`). Parameterized by crate so the check can cover EVERY
/// crate the Dockerfile builds, not just this test's own — amux-frustrations'
/// catch: a class-kill must not share the blind spot of the gates it guards, and
/// the image build compiles amux-cli too.
fn external_include_roots(crate_manifest: &Path) -> BTreeSet<String> {
    let root = repo_root();
    let mut roots = BTreeSet::new();
    let src = crate_manifest.join("src");
    if !src.is_dir() {
        return roots;
    }
    let mut stack = vec![src];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|x| x.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            let src_dir = p.parent().unwrap();
            for abs in include_str_targets(&text, src_dir, crate_manifest) {
                if let Ok(rr) = abs.strip_prefix(&root) {
                    if let Some(first) = rr.components().next().and_then(|c| c.as_os_str().to_str())
                    {
                        if !first.is_empty() && first != "crates" {
                            roots.insert(first.to_string());
                        }
                    }
                }
            }
        }
    }
    roots
}

/// The crates the Dockerfile's `cargo build --release -p X -p Y` compiles, read
/// FROM the Dockerfile so the coverage tracks the build line and cannot drift.
/// Package name == crate dir here (workspace convention); a missing dir just
/// contributes nothing.
fn built_crate_dirs(dockerfile: &str, root: &Path) -> Vec<PathBuf> {
    let line = dockerfile
        .lines()
        .find(|l| l.contains("cargo build --release"))
        .expect("Dockerfile builds the workspace with `cargo build --release`");
    line.split("-p ")
        .skip(1)
        .filter_map(|s| s.split_whitespace().next())
        .map(|name| root.join("crates").join(name))
        .collect()
}

/// Every `include_str!` target in `text`, resolved to a normalized absolute path.
/// Handles the two forms in this crate: a bare literal (relative to the source
/// file's dir) and `concat!(env!("CARGO_MANIFEST_DIR"), "/...")` (relative to the
/// crate manifest dir, where the literal begins with `/` as a separator, so it is
/// string-concatenated rather than path-joined — join would treat it as absolute).
fn include_str_targets(text: &str, src_dir: &Path, manifest: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = text[i..].find("include_str!") {
        let start = i + p;
        let Some(open) = text[start..].find('(').map(|o| start + o) else {
            i = start + 12;
            continue;
        };
        // Matching close paren for the macro call.
        let mut depth = 0i32;
        let mut end = open;
        for (k, ch) in text[open..].char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + k;
                        break;
                    }
                }
                _ => {}
            }
        }
        let call = &text[open..=end.min(text.len() - 1)];
        i = end + 1;
        // The PATH literal, not the first literal: `env!("CARGO_MANIFEST_DIR")`
        // puts "CARGO_MANIFEST_DIR" ahead of the real path, and it has no `/`.
        let Some(lit) = path_literal(call) else {
            continue;
        };
        let base = if call.contains("CARGO_MANIFEST_DIR") {
            manifest
        } else {
            src_dir
        };
        let joined = if lit.starts_with('/') {
            PathBuf::from(format!("{}{}", base.to_string_lossy(), lit))
        } else {
            base.join(&lit)
        };
        out.push(normalize(&joined));
    }
    out
}

/// The first string literal in `s` that looks like a PATH (contains `/`). This
/// skips `env!("CARGO_MANIFEST_DIR")`'s literal and any non-path token. A same-dir
/// `include_str!("foo.txt")` (no `/`) is intentionally ignored: it resolves under
/// the crate, which `COPY crates crates` already covers.
fn path_literal(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            let mut j = i + 1;
            let mut val = String::new();
            while j < b.len() && b[j] != b'"' {
                val.push(b[j] as char);
                j += 1;
            }
            if val.contains('/') {
                return Some(val);
            }
            i = j + 1;
            continue;
        }
        i += 1;
    }
    None
}

/// The source token of every `COPY <src> ...` in the Dockerfile, skipping
/// stage-copies (`COPY --from=...`). Trailing `./` normalized off.
fn dockerfile_copied_sources(dockerfile: &str) -> BTreeSet<String> {
    dockerfile
        .lines()
        .filter_map(|l| l.trim().strip_prefix("COPY "))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|s| !s.starts_with("--"))
        .map(|s| s.trim_start_matches("./").trim_end_matches('/').to_string())
        .collect()
}

#[test]
fn dockerfile_copies_every_external_include_str_input() {
    let root = repo_root();
    let dockerfile = std::fs::read_to_string(root.join("cloud/docker/Dockerfile"))
        .expect("cloud/docker/Dockerfile is readable");

    // Union the external include_str! roots across EVERY crate the Dockerfile
    // builds, not just this test's own crate — the image build compiles amux-cli
    // too, so a gap there breaks it the same way (amux-frustrations' catch).
    let mut needed = BTreeSet::new();
    for crate_dir in built_crate_dirs(&dockerfile, &root) {
        needed.extend(external_include_roots(&crate_dir));
    }
    // The scraper must find the two known amux-server externals, or it is broken
    // and a green pass here would be meaningless (the empty-probe trap).
    assert!(
        needed.contains("amux") && needed.contains("scripts"),
        "the include_str! scraper found {needed:?} — expected at least {{amux, scripts}}; it is broken"
    );

    let copied = dockerfile_copied_sources(&dockerfile);

    let missing: Vec<&String> = needed.iter().filter(|r| !copied.contains(*r)).collect();
    assert!(
        missing.is_empty(),
        "cloud/docker/Dockerfile does not COPY {missing:?} into the build context, but \
         amux-server include_str!s a file under each at COMPILE time. The image build will die \
         with `couldn't read .../{first}/...: No such file` — latent until a deploy runs the \
         build (this is the amux-CLI/scripts recurrence). Add `COPY {first} {first}` to the \
         build stage.",
        first = missing.first().map(|s| s.as_str()).unwrap_or("<dir>")
    );

    // Negative-control confidence: the assertion above can only be trusted if it
    // WOULD fire. Prove the mechanism by removing a known-required root from the
    // copied set and confirming it is then reported missing.
    let mut copied_without_scripts = copied.clone();
    copied_without_scripts.remove("scripts");
    let would_miss: Vec<&String> = needed
        .iter()
        .filter(|r| !copied_without_scripts.contains(*r))
        .collect();
    assert!(
        would_miss.iter().any(|r| *r == "scripts"),
        "the check cannot detect a missing COPY — it is theatre"
    );
}
