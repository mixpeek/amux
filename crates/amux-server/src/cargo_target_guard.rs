//! The builder and reclaim execute the same embedded guard, including the
//! filesystem mutation. No check-then-rename window, and no checkout dependency
//! in a deployed server. Python is already required by the builder's tooling.
use std::path::{Path, PathBuf};

const GUARD: &str = include_str!("../../../scripts/cargo-target-guard.py");

fn mutate(action: &str, path: &Path, destination: Option<&Path>, originals: &[String], extra_roots: &[PathBuf]) -> Result<(), String> {
    let home = crate::api::reclaim::home_dir();
    let mut roots = vec![home.join(".amux/rust-build-target"), home.join(".amux/rust-build-target-e2e-head")];
    if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
        roots.push(PathBuf::from(target));
    }
    roots.extend_from_slice(extra_roots);
    if !overlaps_targets(path, destination, originals, &roots) {
        return match destination {
            Some(dest) => std::fs::rename(path, dest).map_err(|e| e.to_string()),
            None => match std::fs::remove_dir_all(path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
                _ => Ok(()),
            },
        };
    }
    let mut command = std::process::Command::new("python3");
    command.args(["-c", GUARD, action, "--path"]).arg(path);
    for root in roots {
        command.arg("--target").arg(root);
    }
    if let Some(dest) = destination {
        command.arg("--destination").arg(dest);
    }
    for original in originals {
        command.arg("--protected-path").arg(original);
    }
    let output = command.output().map_err(|e| format!("Cargo reclaim guard unmeasured: {e}"));
    let result = output.and_then(|out| {
        let verdict: serde_json::Value = serde_json::from_slice(&out.stdout)
            .map_err(|e| format!("Cargo reclaim guard returned no valid verdict: {e}"))?;
        if !out.status.success() {
            return Err(verdict["reason"].as_str().unwrap_or("Cargo reclaim guard refused").to_string());
        }
        tracing::info!(path = %path.display(), action, verdict = %verdict, "cargo_reclaim_result");
        Ok(())
    });
    if let Err(reason) = &result {
        tracing::warn!(path = %path.display(), action, reason, "cargo_reclaim_deferred");
    }
    result
}

pub(crate) fn rename(path: &Path, destination: &Path) -> Result<(), String> {
    mutate("move", path, Some(destination), &[], &[])
}

pub(crate) fn purge(path: &Path, originals: &[String]) -> Result<(), String> {
    mutate("purge", path, None, originals, &[])
}

/// Retention discovers arbitrary old target names. They must be leased even
/// when they are absent from the two conventional roots above.
pub(crate) fn purge_build_target(path: &Path) -> Result<(), String> {
    mutate("purge", path, None, &[], &[path.to_path_buf()])
}

// Resolve existing symlink ancestors even when a restore destination is absent.
fn resolved(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if parent != path => resolved(parent).join(name),
        _ => path.to_path_buf(),
    }
}

fn overlaps_targets(path: &Path, destination: Option<&Path>, originals: &[String], roots: &[PathBuf]) -> bool {
    let mut paths = vec![resolved(path)];
    paths.extend(destination.map(resolved));
    paths.extend(originals.iter().map(|p| resolved(Path::new(p))));
    roots.iter().map(|p| resolved(p)).any(|root| paths.iter().any(|p| p.starts_with(&root) || root.starts_with(p)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retention_keeps_arbitrarily_named_target_with_a_live_lease() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("rust-build-target-old-proof");
        std::fs::create_dir_all(target.join("debug/deps")).unwrap();
        let marker = target.join("debug/deps/running-test");
        std::fs::write(&marker, "active artifact").unwrap();
        let lease = std::fs::File::create(dir.path().join(".rust-build-target-old-proof.reclaim.lock")).unwrap();
        // Same advisory lock held by safe-cargo through compiler/test lifetime.
        assert_eq!(unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH) }, 0);
        let error = purge_build_target(&target).unwrap_err();
        assert!(error.contains("lock busy"), "{error}");
        std::fs::File::open(&target)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        assert_eq!(crate::runtime_jobs::storage::prune_stale_build_targets(dir.path()), (0, 0));
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "active artifact");
    }

    #[test]
    fn unrelated_reclaim_stays_native_and_cargo_sources_destinations_originals_are_guarded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("target");
        std::fs::create_dir(&root).unwrap();
        let roots = vec![root.clone()];
        let unrelated = dir.path().join("unrelated");
        let staged = dir.path().join("quarantine");
        assert!(!overlaps_targets(&unrelated, Some(&staged), &[], &roots));
        assert!(overlaps_targets(&root.join("debug/deps"), Some(&staged), &[], &roots));
        assert!(overlaps_targets(&staged, Some(&root.join("debug/deps")), &[], &roots));
        assert!(overlaps_targets(&staged, None, &[root.to_string_lossy().into()], &roots));
        std::os::unix::fs::symlink(&root, dir.path().join("alias")).unwrap();
        assert!(overlaps_targets(&dir.path().join("alias/debug/deps"), None, &[], &roots));
        // These exercise the native branch even on hosts without Python.
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(unrelated.join("artifact"), "ordinary cache").unwrap();
        rename(&unrelated, &staged).unwrap();
        assert!(staged.join("artifact").exists());
        purge(&staged, &[]).unwrap();
        assert!(!staged.exists());
    }
}
