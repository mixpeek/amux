//! `amux-rs --version` names the source it was built from, not the workspace's
//! placeholder `version = "0.1.0"`, which no release has ever been.
//!
//! The expected value is read from the same checkout with the same
//! `git describe` the build script runs, so the test holds on a full clone
//! (`v0.9.108-7740-g…`) and on CI's shallow, tagless checkout (a bare short sha).
//! Offline: `--version` needs no server.

use std::path::Path;
use std::process::Command;

fn amux_rs_version() -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_amux-rs"))
        .arg("--version")
        .output()
        .expect("run amux-rs --version");
    assert!(out.status.success(), "exit {:?}", out.status.code());
    String::from_utf8(out.stdout)
        .expect("utf-8")
        .trim()
        .to_string()
}

#[test]
fn version_is_not_the_workspace_placeholder() {
    let v = amux_rs_version();
    assert_ne!(v, format!("amux-rs {}", env!("CARGO_PKG_VERSION")));
    assert!(v.starts_with("amux-rs "), "{v}");
}

#[test]
fn version_is_the_checkouts_git_describe() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let described = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["describe", "--tags", "--match", "v[0-9]*", "--always"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let Some(described) = described else {
        // Built outside a git checkout: the build says so instead of guessing.
        assert_eq!(amux_rs_version(), "amux-rs unknown");
        return;
    };
    let v = amux_rs_version();
    // `-dirty` rides along when crates/ or Cargo.* have uncommitted edits.
    assert!(
        v == format!("amux-rs {described}") || v == format!("amux-rs {described}-dirty"),
        "got {v:?}, checkout describes as {described:?}"
    );
}
