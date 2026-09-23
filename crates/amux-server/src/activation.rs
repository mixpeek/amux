//! Verify installed bytes before deciding whether an mtime change merits exec.
use std::path::Path;

pub fn file_build_hash(path: &Path) -> std::io::Result<String> {
    use sha2::Digest;
    let mut file = std::fs::File::open(path)?;
    let mut hash = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    Ok(hex::encode(&hash.finalize()[..8]))
}

#[derive(Debug)]
pub struct Candidate {
    pub build: String,
    pub commit: Option<String>,
}

impl Candidate {
    pub fn read(exe: &Path) -> anyhow::Result<Self> {
        let build = file_build_hash(exe)?;
        let receipt = exe.with_file_name(format!(
            "{}.identity.json",
            exe.file_name().unwrap().to_string_lossy()
        ));
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
            return Some("same_binary");
        }
        let clean =
            running_commit.len() == 40 && running_commit.bytes().all(|b| b.is_ascii_hexdigit());
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
}
