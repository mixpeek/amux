//! Durable file storage for connector credentials and OAuth grants.
//! Keep the existing file formats, but commit whole private files and serialize
//! read/modify/write operations with an OS lock released on process death.
use serde::de::DeserializeOwned;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

pub(crate) fn read_json<T: DeserializeOwned + Default>(path: &Path) -> io::Result<T> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e),
    }
}

// Sync new directory links as well as the final file's directory: otherwise
// the first grant's newly created family directory can disappear after a crash.
fn private_directories(dir: &Path) -> io::Result<()> {
    let mut missing = Vec::new();
    let mut current = dir;
    while !current.exists() {
        missing.push(current.to_path_buf());
        current = current
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
    }
    for path in missing.iter().rev() {
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => {}
            Err(e) => return Err(e),
        }
        File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()?;
    }
    Ok(())
}

/// Never truncate a sole copy of a credential. The sibling temp starts 0600;
/// sync its bytes before atomic publication, then sync the directory entry.
/// NamedTempFile removes an unpublished temp on ordinary errors.
pub(crate) fn write(path: &Path, content: &[u8]) -> io::Result<()> {
    let result = write_before_publish(path, content, |_| Ok(()));
    if let Err(ref error) = result {
        tracing::warn!(path = %path.display(), %error, measured = true, n_considered = 1,
            verdict = "connector_store_commit_failed", "connector state was not acknowledged as durable");
    }
    result
}

fn write_before_publish(
    path: &Path,
    content: &[u8],
    before: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    private_directories(dir)?;
    let mut pending = tempfile::Builder::new()
        .prefix(".amux-connector-")
        .tempfile_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        pending
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    pending.write_all(content)?;
    pending.as_file().sync_all()?;
    before(pending.path())?;
    pending.persist(path).map_err(|e| e.error)?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Sidecar inode must remain stable while the data file is renamed. A bounded
/// wait surfaces contention rather than hanging the request forever. File::lock
/// is OS-backed, so two server processes serialize and SIGKILL releases it.
pub(crate) fn lock(path: &Path) -> io::Result<File> {
    let dir = path.parent().unwrap_or(Path::new("."));
    private_directories(dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "store has no filename"))?;
    let sidecar = dir.join(format!(".{}.lock", name.to_string_lossy()));
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(sidecar)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                tracing::warn!(path = %path.display(), verdict = "connector_store_lock_timeout", "connector store is busy; no update applied");
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "connector store lock timed out",
                ));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_failure_keeps_the_original_and_removes_private_temp() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("grant.json");
        write(&path, b"old").unwrap();
        let result = write_before_publish(&path, b"new", |pending| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(pending)?.permissions().mode() & 0o777,
                    0o600
                );
            }
            Err(io::Error::other("injected pre-publication failure"))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"old");
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_publications_never_expose_partial_json_or_share_a_temp() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("grant.json");
        let body =
            serde_json::to_vec(&serde_json::json!({"token":"fixture".repeat(65536)})).unwrap();
        write(&path, &body).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let path = &path;
                let body = &body;
                scope.spawn(move || {
                    for _ in 0..20 {
                        write(path, body).unwrap();
                    }
                });
            }
            for _ in 0..300 {
                let _: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            }
        });
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
    }

    #[test]
    fn crash_child() {
        let Ok(root) = std::env::var("AMUX_CONNECTOR_CRASH_FIXTURE") else {
            return;
        };
        let root = Path::new(&root);
        let path = root.join("grant.json");
        let _lock = lock(&path).unwrap();
        write_before_publish(&path, b"replacement", |_| {
            std::fs::write(root.join("staged"), b"ready")?;
            loop {
                std::thread::sleep(Duration::from_millis(50));
            }
        })
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn sigkill_before_publication_preserves_committed_grant_and_releases_lock() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("grant.json");
        write(&path, b"committed").unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "integrations::secure_store::tests::crash_child",
                "--nocapture",
            ])
            .env("AMUX_CONNECTOR_CRASH_FIXTURE", home.path())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !home.path().join("staged").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let staged = home.path().join("staged").exists();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(staged, "child must reach publication boundary");
        assert_eq!(std::fs::read(&path).unwrap(), b"committed");
        let _lock = lock(&path).unwrap();
        write(&path, b"recovered").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"recovered");
    }
}
