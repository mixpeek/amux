//! A flag in the name slot never creates a worker (AMUX-3116).
//!
//! `amux register --help` got this guard in August; `amux exec --help` did not,
//! and on 2026-10-15 it registered AND started a live Claude worker named
//! "--help" in the caller's checkout. Both verbs now share
//! `require_worker_name`. This runs the real script against a throwaway home
//! and checks the one outcome that matters: no `.env` file was written.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("repo root above crates/amux-server")
        .to_path_buf()
}

#[test]
fn a_flag_in_the_name_slot_registers_nothing() {
    let cli = repo_root().join("amux");
    for args in [&["exec", "--help"][..], &["exec", "-m"], &["exec"], &["register", "--help"], &["register", "--yolo"]] {
        let home = tempfile::tempdir().unwrap();
        let out = Command::new("bash")
            .arg(&cli)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", home.path())
            .env("CC_HOME", home.path().join(".amux"))
            .current_dir(home.path())
            .output()
            .expect("run ./amux");
        assert!(!out.status.success(), "{args:?} must refuse, got exit 0");
        let sessions = home.path().join(".amux/sessions");
        let written: Vec<_> = std::fs::read_dir(&sessions)
            .map(|d| d.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(written.is_empty(), "{args:?} wrote {written:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("usage: amux") || err.contains("looks like a flag"), "{args:?}: {err}");
    }
}
