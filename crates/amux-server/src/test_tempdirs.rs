//! Temp dirs a test keeps alive past the helper that made them, deleted when
//! the test process exits.
//!
//! A test helper that opens a `Store` or builds a `Router` has to keep the
//! SQLite file's directory after it returns, so 57 helpers did (auth.rs keeps one, as an owner-tier file)
//! `std::mem::forget(dir)` or `Box::leak(Box::new(dir))`. Nothing ever removed
//! those directories: every `cargo test` on the Mac left them in `$TMPDIR` as
//! `.tmpXXXXXX`, about 400 an hour across the lanes, 11,735 of them holding
//! 29G when disk RCA 20261029-103955 found them.
//!
//! `keep` does what those calls did (the directory outlives the helper) and
//! registers it for removal by an `atexit` hook, which runs when the test
//! binary exits normally or through `std::process::exit`, as the harness does
//! after a failure. A test process that is killed skips the hook; the
//! cleanup tick reaps those dirs once their holder is gone.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

static KEPT: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static HOOK: Once = Once::new();

extern "C" fn remove_kept() {
    // try_lock: a thread still holding the lock at exit must not hang the
    // process; its directories are then left for the cleanup tick.
    if let Ok(dirs) = KEPT.try_lock() {
        for dir in dirs.iter() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// A directory kept for the rest of the process.
pub struct KeptDir(PathBuf);

impl KeptDir {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Keep `dir` until the process exits, then delete it.
pub fn keep(dir: tempfile::TempDir) -> &'static KeptDir {
    HOOK.call_once(|| {
        // SAFETY: `remove_kept` is a plain extern "C" fn with no arguments,
        // which is what atexit takes; registering it has no other effect.
        unsafe {
            libc::atexit(remove_kept);
        }
    });
    let path = dir.keep();
    if let Ok(mut dirs) = KEPT.lock() {
        dirs.push(path.clone());
    }
    Box::leak(Box::new(KeptDir(path)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_kept_dir_outlives_its_helper_and_is_removed_at_process_exit() {
        // Run a child process that keeps a dir and exits; the dir must be gone.
        if let Ok(path) = std::env::var("AMUX_TEST_TEMPDIRS_CHILD_OUT") {
            let kept = super::keep(tempfile::tempdir().unwrap());
            std::fs::write(kept.path().join("t.db"), b"x").unwrap();
            std::fs::write(&path, kept.path().to_string_lossy().as_bytes()).unwrap();
            assert!(kept.path().join("t.db").exists(), "kept while the process runs");
            return;
        }
        let out = tempfile::NamedTempFile::new().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "test_tempdirs::tests::a_kept_dir_outlives_its_helper_and_is_removed_at_process_exit", "--test-threads=1"])
            .env("AMUX_TEST_TEMPDIRS_CHILD_OUT", out.path())
            .status()
            .unwrap();
        assert!(status.success());
        let kept = std::fs::read_to_string(out.path()).unwrap();
        assert!(!kept.is_empty(), "the child reported its kept dir");
        assert!(!std::path::Path::new(&kept).exists(), "{kept} was left behind after the process exited");
    }
}
