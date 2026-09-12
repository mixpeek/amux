// AMUX-3454: stamp the commit the binary was built from, so /health can
// answer "does the running server contain commit X" directly. `build` (a
// content hash) discriminates binaries but not commits, and the AF-82
// time-comparison fails when two commits land seconds apart — which cost two
// wasted verification rounds in one afternoon (a365cd50 read as containing
// 475d74a while it was dec6eaa's build).
//
// Honesty caveats, both deliberate:
// - The local builder compiles the WORKING TREE, not the commit object, so a
//   dirty tree gets a `-dirty` suffix — without it, "the build contains my
//   commit" could confidently mislead when uncommitted edits rode along.
// - Outside a git checkout (the cloud image builds from a COPY without .git)
//   this fails SOFT to "unknown": presence/absence of .git drives the
//   difference, never a build flag (single-codebase rule).
fn main() {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("server manifest directory")).join("../..");
    println!("cargo:rerun-if-changed={}", root.join("crates").display());
    println!("cargo:rerun-if-changed={}", root.join("Cargo.toml").display());
    println!("cargo:rerun-if-changed={}", root.join("Cargo.lock").display());
    let git = |args: &[&str]| -> Option<String> {
        // Pathspecs below are repository-relative. Running from this crate's
        // directory silently inspected nonexistent crates/crates and called
        // a dirty binary clean.
        let o = std::process::Command::new("git").arg("-C").arg(&root).args(args).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    // Linked worktrees store .git as a FILE. Resolve metadata through Git;
    // watching root/.git/HEAD there tells Cargo to rebuild forever because that
    // path cannot exist. Only the current branch can change this build's SHA.
    if let Some(head) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={}", root.join(head).display());
        if let Some(reference) = git(&["symbolic-ref", "-q", "HEAD"]) {
            if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
                let mut path = root.join(path);
                // A packed ref has no loose file yet. Watch its existing parent
                // so a later loose update invalidates the cached commit identity.
                while !path.exists() && path.pop() {}
                println!("cargo:rerun-if-changed={}", path.display());
            }
            if let Some(packed) = git(&["rev-parse", "--git-path", "packed-refs"]) {
                let packed = root.join(packed);
                // When absent, packing deletes the watched loose ref; that
                // already reruns this script and installs the packed-ref watch.
                if packed.exists() {
                    println!("cargo:rerun-if-changed={}", packed.display());
                }
            }
        }
    } else {
        println!("cargo:warning=build_git_metadata_unavailable measured=false n_considered=0 identity=unknown");
    }
    let mut sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    if sha.is_empty() {
        sha = "unknown".into();
    } else if git(&["status", "--porcelain", "--", "crates", "Cargo.toml", "Cargo.lock"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        sha.push_str("-dirty");
    }
    let mut full = git(&["rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".into());
    if sha.ends_with("-dirty") { full.push_str("-dirty"); }
    println!("cargo:rustc-env=AMUX_BUILD_COMMIT_FULL={full}");
    println!("cargo:rustc-env=AMUX_BUILD_COMMIT={sha}");
}
