//! Explicit candidate assets, retained before workspace disposal. No prose discovery.
use crate::db::artifact_store as registry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub path: String,
    pub sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Retained {
    pub source: Asset,
    pub head: String,
    pub path: String,
}
fn extension(asset: &Asset) -> anyhow::Result<&str> {
    let path = Path::new(&asset.path);
    anyhow::ensure!(
        !asset.path.is_empty() && path.components().all(|p| matches!(p, Component::Normal(_))),
        "asset must be candidate-relative without traversal"
    );
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    anyhow::ensure!(
        matches!(ext, "md" | "json" | "txt" | "png" | "webm"),
        "only passive Markdown, JSON, text, PNG and WebM assets supported"
    );
    anyhow::ensure!(
        asset.sha256.len() == 64
            && asset
                .sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
        "asset SHA256 required"
    );
    Ok(ext)
}
pub fn validate_manifest(assets: &[Asset]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !assets.is_empty(),
        "report must include at least one retained Markdown, JSON, text, PNG or WebM asset"
    );
    anyhow::ensure!(assets.len() <= 16, "at most 16 assets");
    for asset in assets {
        extension(asset)?;
    }
    Ok(())
}

pub async fn retain(
    home: &Path,
    root: &Path,
    report: &super::planner::Report,
) -> anyhow::Result<Vec<Retained>> {
    validate_manifest(&report.assets)?;
    let root = root.canonicalize()?;
    let target = home.join("artifacts/project-reports");
    std::fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    anyhow::ensure!(
        target.starts_with(home.canonicalize()?),
        "asset store escaped private home"
    );
    let mut retained = Vec::new();
    let mut total = 0;
    for asset in &report.assets {
        let ext = extension(asset)?;
        let file = root.join(&asset.path).canonicalize()?;
        anyhow::ensure!(
            file.starts_with(&root) && file.is_file(),
            "asset escaped candidate"
        );
        let size = std::fs::metadata(&file)?.len();
        total += size;
        anyhow::ensure!(
            size <= 64 * 1024 * 1024 && total <= 256 * 1024 * 1024,
            "asset size limit"
        );
        let mut bytes = Vec::new();
        std::fs::File::open(&file)?
            .take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() <= 64 * 1024 * 1024,
            "asset size changed beyond limit"
        );
        anyhow::ensure!(
            hex::encode(Sha256::digest(&bytes)) == asset.sha256,
            "asset identity mismatch"
        );
        if matches!(ext, "md" | "json" | "txt") {
            let output = tokio::process::Command::new("git")
                .args(["show", &format!("{}:{}", report.head, asset.path)])
                .current_dir(&root)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .await?;
            anyhow::ensure!(
                output.status.success() && output.stdout == bytes,
                "report must be committed in reported candidate"
            );
            if ext == "json" {
                let _: serde_json::Value = serde_json::from_slice(&bytes)?;
            }
        } else if ext == "png" {
            anyhow::ensure!(
                bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                "invalid PNG signature"
            );
        } else {
            anyhow::ensure!(
                bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]),
                "invalid WebM signature"
            );
        }
        let destination = target.join(format!("{}.{}", asset.sha256, ext));
        // Persist via rename rather than following a pre-existing destination symlink.
        let temp = target.join(format!(".{}", ulid::Ulid::new()));
        std::fs::write(&temp, &bytes)?;
        std::fs::rename(&temp, &destination)?;
        retained.push(Retained {
            source: asset.clone(),
            head: report.head.clone(),
            path: destination.to_string_lossy().into(),
        });
    }
    check(&retained)?;
    tracing::info!(
        measured = true,
        n_considered = retained.len(),
        verdict = "project.assets_retained",
        "explicit candidate assets retained and hash checked"
    );
    Ok(retained)
}

/// Retain one JSON receipt produced by an already-validated acceptance invocation. It cannot be
/// committed without changing the candidate SHA it proves, so this is intentionally narrower than
/// `retain`: only the caller-named JSON path is accepted, its bytes are parsed and hash-addressed,
/// and ordinary report JSON continues to require a committed candidate blob.
pub async fn retain_generated_json(
    home: &Path,
    root: &Path,
    head: &str,
    path: &str,
) -> anyhow::Result<Retained> {
    let root = root.canonicalize()?;
    let file = root.join(path).canonicalize()?;
    anyhow::ensure!(
        file.starts_with(&root) && file.is_file(),
        "asset escaped candidate"
    );
    let bytes = std::fs::read(&file)?;
    anyhow::ensure!(bytes.len() <= 64 * 1024 * 1024, "asset size limit");
    let _: serde_json::Value = serde_json::from_slice(&bytes)?;
    let asset = Asset {
        path: path.into(),
        sha256: hex::encode(Sha256::digest(&bytes)),
    };
    anyhow::ensure!(
        extension(&asset)? == "json",
        "generated receipt must be JSON"
    );
    let target = home.join("artifacts/project-reports");
    std::fs::create_dir_all(&target)?;
    let target = target.canonicalize()?;
    anyhow::ensure!(
        target.starts_with(home.canonicalize()?),
        "asset store escaped private home"
    );
    let destination = target.join(format!("{}.json", asset.sha256));
    let temp = target.join(format!(".{}", ulid::Ulid::new()));
    std::fs::write(&temp, &bytes)?;
    std::fs::rename(&temp, &destination)?;
    let retained = Retained {
        source: asset,
        head: head.into(),
        path: destination.to_string_lossy().into(),
    };
    check(std::slice::from_ref(&retained))?;
    tracing::info!(
        measured = true,
        n_considered = 1,
        verdict = "project.generated_receipt_retained",
        "fresh generated JSON receipt retained and hash checked"
    );
    Ok(retained)
}
pub fn check(assets: &[Retained]) -> anyhow::Result<()> {
    for a in assets {
        extension(&a.source)?;
        anyhow::ensure!(
            hex::encode(Sha256::digest(std::fs::read(&a.path)?)) == a.source.sha256,
            "retained asset identity changed"
        );
    }
    Ok(())
}
pub fn register(c: &rusqlite::Connection, id: &str, assets: &[Retained]) -> rusqlite::Result<()> {
    for a in assets {
        if registry::get_for_task_ref(c, id, &a.path)?.is_none() {
            let now = chrono::Utc::now().timestamp();
            registry::insert(
                c,
                &registry::ArtifactRow {
                    id: ulid::Ulid::new().to_string(),
                    task_id: id.into(),
                    kind: if a.source.path.ends_with(".md")
                        || a.source.path.ends_with(".json")
                        || a.source.path.ends_with(".txt")
                    {
                        "doc"
                    } else {
                        "screenshot"
                    }
                    .into(),
                    ref_value: a.path.clone(),
                    state: "submitted".into(),
                    description: Some(serde_json::to_string(a).unwrap()),
                    created_at: now,
                    updated_at: now,
                },
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn project_assets_bind_committed_reports_and_retain_after_disposal() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init"]);
        git(&["config", "user.name", "Fixture"]);
        git(&["config", "user.email", "fixture@example.invalid"]);
        std::fs::write(repo.path().join("report.md"), "# Result\n").unwrap();
        git(&["add", "report.md"]);
        git(&["commit", "-m", "fixture"]);
        let spec = Asset {
            path: "report.md".into(),
            sha256: hex::encode(Sha256::digest(b"# Result\n")),
        };
        let mut report = super::super::planner::Report {
            head: git(&["rev-parse", "HEAD"]),
            checks: vec![],
            summary: "test".into(),
            assets: vec![spec.clone()],
        };
        let retained = retain(home.path(), repo.path(), &report).await.unwrap();
        let db = crate::db::Store::open(&home.path().join("db")).unwrap();
        db.write(move |c| {
            register(c, "A", &retained)?;
            register(c, "A", &retained)?;
            Ok(crate::db::WriteOutcome {
                applied: true,
                events: vec![],
            })
        })
        .unwrap();
        assert_eq!(
            db.read()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM _amux_task_artifacts", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        std::fs::write(repo.path().join("report.md"), "changed").unwrap();
        assert!(retain(home.path(), repo.path(), &report).await.is_err());
        report.assets[0].sha256 = hex::encode(Sha256::digest(b"changed"));
        assert!(
            retain(home.path(), repo.path(), &report).await.is_err(),
            "uncommitted report refused even with matching hash"
        );
        for path in ["../report.md", "/tmp/report.md", "run.mdai", "page.html"] {
            report.assets[0].path = path.into();
            assert!(retain(home.path(), repo.path(), &report).await.is_err());
        }
        let path = home
            .path()
            .join(format!("artifacts/project-reports/{}.md", spec.sha256));
        drop(repo);
        assert_eq!(std::fs::read(&path).unwrap(), b"# Result\n");
        let retained = vec![Retained {
            source: spec,
            head: report.head,
            path: path.to_string_lossy().into(),
        }];
        check(&retained).unwrap();
        std::fs::write(&path, "corrupted").unwrap();
        assert!(check(&retained).is_err());
    }
    #[tokio::test]
    async fn project_assets_retain_passive_media_and_refuse_symlink_escape() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let png = b"\x89PNG\r\n\x1a\nfixture";
        let webm = &[0x1a, 0x45, 0xdf, 0xa3, 0x42];
        let mut report = super::super::planner::Report {
            head: "a".repeat(40),
            checks: vec![],
            summary: String::new(),
            assets: vec![],
        };
        for (name, bytes) in [
            ("image.png", png.as_slice()),
            ("capture.webm", webm.as_slice()),
        ] {
            std::fs::write(repo.path().join(name), bytes).unwrap();
            report.assets.push(Asset {
                path: name.into(),
                sha256: hex::encode(Sha256::digest(bytes)),
            });
        }
        let retained = retain(home.path(), repo.path(), &report).await.unwrap();
        assert_eq!(retained.len(), 2);
        check(&retained).unwrap();
        std::fs::write(outside.path().join("image.png"), png).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                outside.path().join("image.png"),
                repo.path().join("escape.png"),
            )
            .unwrap();
            report.assets[0].path = "escape.png".into();
            assert!(retain(home.path(), repo.path(), &report).await.is_err());
        }
    }

    #[tokio::test]
    async fn generated_json_retention_is_explicit_and_does_not_relax_reports() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(repo.path().join("receipt.json"), br#"{"state":"passed"}"#).unwrap();
        let retained =
            retain_generated_json(home.path(), repo.path(), &"a".repeat(40), "receipt.json")
                .await
                .unwrap();
        assert_eq!(retained.source.path, "receipt.json");
        assert!(std::path::Path::new(&retained.path).is_file());
        let report = super::super::planner::Report {
            head: "a".repeat(40),
            checks: vec![],
            summary: String::new(),
            assets: vec![retained.source],
        };
        assert!(retain(home.path(), repo.path(), &report).await.is_err());
        std::fs::write(repo.path().join("bad.json"), b"not-json").unwrap();
        assert!(
            retain_generated_json(home.path(), repo.path(), &"a".repeat(40), "bad.json")
                .await
                .is_err()
        );
    }
}
