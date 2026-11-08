//! Verify installed bytes before deciding whether an mtime change merits exec,
//! and say which commit this process serves.
use std::path::{Path, PathBuf};

pub fn file_build_hash(path: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path)?;
    let mut hash = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(hex::encode(&hash.finalize()[..8]))
}

/// The receipt the builder writes beside the installed binary:
/// `{"sha": <commit>, "build": <file_build_hash of the binary>}`.
pub fn receipt_path(exe: &Path) -> PathBuf {
    exe.with_file_name(format!(
        "{}.identity.json",
        exe.file_name().unwrap_or_default().to_string_lossy()
    ))
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The commit a process serves, and where that answer came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    pub full: String,
    pub short: String,
    /// "receipt": the builder's receipt names these exact bytes. "compiled":
    /// the stamp build.rs compiled in.
    pub source: &'static str,
    /// The dashboard snapshot the receipt names (`dashboard_dir`), served in
    /// place of the embedded copy. Only a receipt for these bytes names one.
    pub dashboard_dir: Option<PathBuf>,
}

/// A commit whose compile inputs equal the installed build's needs no compile
/// (scripts/rust-auto-build.sh re-stamps the receipt instead), so the bytes
/// can serve a later commit than the one they were compiled at. The receipt
/// names it, and is believed only when its `build` is this process's own hash:
/// a receipt for other bytes says nothing about these.
pub fn served_from(receipt: Option<&[u8]>, running_build: &str, compiled_full: &str, compiled_short: &str) -> Served {
    let compiled = Served {
        full: compiled_full.to_owned(),
        short: compiled_short.to_owned(),
        source: "compiled",
        dashboard_dir: None,
    };
    let Some(bytes) = receipt else { return compiled };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else { return compiled };
    match (v["sha"].as_str(), v["build"].as_str()) {
        (Some(sha), Some(build)) if build == running_build && is_full_sha(sha) => Served {
            full: sha.to_owned(),
            short: sha[..12].to_owned(),
            source: "receipt",
            dashboard_dir: v["dashboard_dir"].as_str().filter(|d| !d.is_empty()).map(PathBuf::from),
        },
        _ => compiled,
    }
}

/// The commit this process serves: the receipt's when it names these bytes,
/// else the compiled stamp. Read once; a new commit arrives by exec.
pub fn served() -> &'static Served {
    static SERVED: std::sync::OnceLock<Served> = std::sync::OnceLock::new();
    SERVED.get_or_init(|| {
        let receipt = std::env::current_exe().ok().and_then(|exe| std::fs::read(receipt_path(&exe)).ok());
        // Hash the executable only when there is a receipt to check it against:
        // the first call can be inside a handler, and a binary with no receipt
        // (a test, a hand build) must not pay for reading itself there.
        let running = if receipt.is_some() { crate::build_hash() } else { String::new() };
        let mut served = served_from(receipt.as_deref(), &running,
            env!("AMUX_BUILD_COMMIT_FULL"), env!("AMUX_BUILD_COMMIT"));
        // A receipt that names a dashboard snapshot claims its commit only if
        // the snapshot is what this process serves. Otherwise it serves the
        // embedded copy and says the compiled commit, so the builder sees the
        // re-stamp was not adopted, restores the receipt and compiles.
        if let Some(dir) = served.dashboard_dir.clone() {
            match amux_dashboard::use_snapshot(&dir) {
                Ok(o) => tracing::info!(verdict = "dashboard_snapshot_served", dir = %dir.display(),
                    version = %o.version, embedded_version = amux_dashboard::ASSET_VER, commit = %served.full,
                    measured = true, n_considered = 1, "dashboard assets served from the deployed snapshot"),
                Err(error) => {
                    tracing::warn!(verdict = "dashboard_snapshot_refused", dir = %dir.display(), %error,
                        receipt_commit = %served.full, measured = true, n_considered = 1,
                        "the receipt's dashboard snapshot cannot be served; serving the embedded copy as the compiled commit");
                    served = served_from(None, "", env!("AMUX_BUILD_COMMIT_FULL"), env!("AMUX_BUILD_COMMIT"));
                }
            }
        }
        tracing::info!(verdict = "served_commit_measured", commit = %served.full, source = served.source,
            build_commit = env!("AMUX_BUILD_COMMIT_FULL"), measured = true,
            "this process serves this commit");
        served
    })
}

#[derive(Debug)]
pub struct Candidate {
    pub build: String,
    pub commit: Option<String>,
}

impl Candidate {
    pub fn read(exe: &Path) -> anyhow::Result<Self> {
        let build = file_build_hash(exe)?;
        let receipt = receipt_path(exe);
        let commit = match std::fs::read(receipt) {
            Ok(bytes) => {
                let v: serde_json::Value = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(v["build"].as_str() == Some(build.as_str()), "installed identity hash does not match executable; install may still be in flight");
                Some(
                    v["sha"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("installed identity has no sha"))?
                        .to_owned(),
                )
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        Ok(Self { build, commit })
    }

    pub fn skip_reason(&self, running_commit: &str, running_build: &str) -> Option<&'static str> {
        if self.build == running_build {
            // The same bytes with a receipt for another commit is a deploy that
            // needed no compile: exec so the new process serves that commit.
            return match self.commit.as_deref() {
                Some(c) if is_full_sha(c) && c != running_commit => None,
                _ => Some("same_binary"),
            };
        }
        let clean = is_full_sha(running_commit);
        if clean && self.commit.as_deref() == Some(running_commit) {
            return Some("same_revision");
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_bytes_and_receipt_control_adoption() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("server");
        let receipt = d.path().join("server.identity.json");
        let sha = "a".repeat(40);
        std::fs::write(&exe, b"first build").unwrap();
        let original = file_build_hash(&exe).unwrap();
        assert_eq!(
            Candidate::read(&exe).unwrap().skip_reason(&sha, &original),
            Some("same_binary")
        );
        // A non-reproducible rebuild of the SAME revision must not exec.
        std::fs::write(&exe, b"same source, different build directory").unwrap();
        let build = file_build_hash(&exe).unwrap();
        std::fs::write(
            &receipt,
            serde_json::json!({"sha":sha,"build":build}).to_string(),
        )
        .unwrap();
        let candidate = Candidate::read(&exe).unwrap();
        assert_eq!(
            candidate.skip_reason(&sha, &original),
            Some("same_revision")
        );
        assert_eq!(candidate.skip_reason(&"b".repeat(40), &original), None);
        assert_eq!(candidate.skip_reason("unknown", &original), None);
        // A foreign overwrite cannot borrow the previous install's identity.
        std::fs::write(&exe, b"foreign overwrite").unwrap();
        assert!(Candidate::read(&exe)
            .unwrap_err()
            .to_string()
            .contains("hash does not match"));
        std::fs::remove_file(&receipt).unwrap();
        assert_eq!(
            Candidate::read(&exe).unwrap().skip_reason(&sha, &original),
            None
        );
    }

    #[test]
    fn a_receipt_for_these_bytes_and_a_new_commit_is_adopted() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("server");
        std::fs::write(&exe, b"one build").unwrap();
        let build = file_build_hash(&exe).unwrap();
        let (old, new) = ("a".repeat(40), "b".repeat(40));
        let stamp = |sha: &str| std::fs::write(receipt_path(&exe), serde_json::json!({"sha":sha,"build":build}).to_string()).unwrap();
        stamp(&old);
        assert_eq!(Candidate::read(&exe).unwrap().skip_reason(&old, &build), Some("same_binary"));
        stamp(&new);
        assert_eq!(Candidate::read(&exe).unwrap().skip_reason(&old, &build), None,
            "same bytes re-stamped for a later commit must exec");
        stamp("not-a-sha");
        assert_eq!(Candidate::read(&exe).unwrap().skip_reason(&old, &build), Some("same_binary"));
    }

    #[test]
    fn the_served_commit_comes_from_a_receipt_only_for_these_bytes() {
        let sha = "c".repeat(40);
        let receipt = serde_json::json!({"sha":sha,"build":"0123456789abcdef"}).to_string();
        let served = served_from(Some(receipt.as_bytes()), "0123456789abcdef", "f".repeat(40).as_str(), "ffffffffffff");
        assert_eq!((served.full.as_str(), served.short.as_str(), served.source), (sha.as_str(), &sha[..12], "receipt"));
        for (bytes, build) in [(Some(receipt.as_bytes()), "fedcba9876543210"), (None, "0123456789abcdef"),
                               (Some(b"not json".as_slice()), "0123456789abcdef")] {
            let s = served_from(bytes, build, "deadbeef", "deadbeef");
            assert_eq!((s.full.as_str(), s.source), ("deadbeef", "compiled"));
        }
        assert_eq!(served.dashboard_dir, None);
        let with_dir = serde_json::json!({"sha":sha,"build":"0123456789abcdef","dashboard_dir":"/snap/x"}).to_string();
        let s = served_from(Some(with_dir.as_bytes()), "0123456789abcdef", "deadbeef", "deadbeef");
        assert_eq!(s.dashboard_dir.as_deref(), Some(Path::new("/snap/x")));
        let s = served_from(Some(with_dir.as_bytes()), "fedcba9876543210", "deadbeef", "deadbeef");
        assert_eq!(s.dashboard_dir, None, "a receipt for other bytes names no snapshot");
    }
}
