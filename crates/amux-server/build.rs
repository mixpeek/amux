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
    // HEAD moves on every commit; refs/heads/main on every branch update.
    // A missing path makes cargo re-run the script each build, which is the
    // right degradation for the no-.git case (cheap, and keeps it "unknown").
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads/main");
    let git = |args: &[&str]| -> Option<String> {
        let o = std::process::Command::new("git").args(args).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let mut sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    if sha.is_empty() {
        sha = "unknown".into();
    } else if git(&["status", "--porcelain", "--", "crates", "Cargo.toml", "Cargo.lock"])
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        sha.push_str("-dirty");
    }
    println!("cargo:rustc-env=AMUX_BUILD_COMMIT={sha}");
}
