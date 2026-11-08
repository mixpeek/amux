// `amux-rs --version` names the source it was built from. The workspace's
// `version = "0.1.0"` is a placeholder no release has carried, so printing it
// (as clap's bare `version` did) answered "which amux is this?" wrongly on
// every build.
//
// Same identity rules as crates/amux-server/build.rs, so the CLI and /health
// never disagree about what "dirty" or "unknown" mean:
// - `git describe --tags --match 'v[0-9]*' --always` (release tag, commits
//   since, sha; ios-build-*/desktop-latest tags never match). A shallow,
//   tagless checkout gives the bare short sha.
// - `-dirty` when crates/ or Cargo.* carry uncommitted edits, because the
//   build compiles the working tree, not the commit.
// - No .git (the cloud image builds from a copy): AMUX_SOURCE_COMMIT, else
//   "unknown". Never a guess.
fn main() {
    let root = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("cli manifest directory"),
    )
    .join("../..");
    for path in ["crates", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={}", root.join(path).display());
    }
    let git = |args: &[&str]| -> Option<String> {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .output()
            .ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Watch what `describe` reads: HEAD, the branch it points at, packed refs
    // and tags. Resolved through git so linked worktrees (.git is a file) work.
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={}", root.join(head).display());
        let mut watch = vec!["packed-refs".to_string(), "refs/tags".to_string()];
        if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
            watch.push(reference);
        }
        for p in watch {
            if let Some(path) = git(&["rev-parse", "--git-path", &p]) {
                let mut path = root.join(path);
                // A ref not written yet: watch its nearest existing parent.
                while !path.exists() && path.pop() {}
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    } else {
        println!("cargo:warning=build_git_metadata_unavailable measured=false identity=unknown");
    }
    println!("cargo:rerun-if-env-changed=AMUX_SOURCE_COMMIT");
    let version = match git(&["describe", "--tags", "--match", "v[0-9]*", "--always"]) {
        Some(mut described) if !described.is_empty() => {
            let dirty = git(&[
                "status",
                "--porcelain",
                "--",
                "crates",
                "Cargo.toml",
                "Cargo.lock",
            ])
            .is_some_and(|s| !s.is_empty());
            if dirty {
                described.push_str("-dirty");
            }
            described
        }
        _ => std::env::var("AMUX_SOURCE_COMMIT")
            .ok()
            .filter(|c| c.len() >= 12 && c.chars().all(|ch| ch.is_ascii_hexdigit()))
            .map(|c| c[..12].to_string())
            .unwrap_or_else(|| "unknown".into()),
    };
    println!("cargo:rustc-env=AMUX_CLI_VERSION={version}");
}
