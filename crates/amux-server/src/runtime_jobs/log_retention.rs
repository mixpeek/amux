//! Retention for worker diagnostic run folders. Never infer inactivity from
//! the top directory mtime alone: writing an existing child does not change it.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// Cap on the reference snapshot. The SECOND probe in this module with the
/// AMUX-4791 shape: breach it and retention defers entirely, exactly as the
/// open-file cap did, and the board only ever grows.
///
/// Measured 2026-09-18 on this box, worst marker: `/logs/` at 4,575,537 bytes
/// over 1042 rows, 27.3% of this cap. Headroom today. `/evidence/` 18.4%,
/// `/audits/` 5.9%. The number is recorded because "it is fine" ages badly and
/// a percentage tells the next reader whether it still is.
const REFERENCE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Read reference-bearing durable records, including undelivered attachments.
/// Missing tables/columns and oversized snapshots are failures, not empty sets.
pub(super) fn reference_texts(
    conn: &rusqlite::Connection,
    marker: &str,
) -> anyhow::Result<Vec<String>> {
    let mut texts = Vec::new();
    let mut bytes = 0usize;
    for sql in [
        "SELECT COALESCE(title,'') || ' ' || COALESCE(desc,'') || ' ' || COALESCE(evidence,'') || ' ' || COALESCE(log,'') || ' ' || COALESCE(next_action,'') || ' ' || COALESCE(last_result,'') || ' ' || COALESCE(unresolved,'') || ' ' || COALESCE(ask_question,'') || ' ' || COALESCE(ask_unblocks,'') AS text FROM issues WHERE COALESCE(archived,0)=0 AND COALESCE(status,'')!='discarded'",
        "SELECT ref_value AS text FROM _amux_task_artifacts",
        "SELECT body AS text FROM _amux_messages",
        "SELECT text FROM saved_messages",
        "SELECT text FROM steering_queue",
        "SELECT text FROM steering_history",
        "SELECT text FROM cmd_history",
    ] {
        // SQL does the filtering; do not deserialize the entire message history.
        let sql = format!("SELECT text FROM ({sql}) WHERE instr(replace(replace(lower(text), char(92)||'/', '/'), '%2f', '/'), ?1)>0");
        let mut stmt = conn.prepare(&sql)?;
        for row in stmt.query_map(rusqlite::params![marker], |row| row.get::<_, String>(0))? {
            let text = row?;
            bytes += text.len();
            anyhow::ensure!(
                bytes as u64 <= REFERENCE_MAX_BYTES,
                "reference snapshot exceeds 16 MiB; retention deferred"
            );
            // Decode percent-escaped file URLs and JSON-escaped separators.
            texts.push(decode_reference(&text));
        }
    }
    // Same signal as the open-file probe, for the same reason: this cap also
    // defers retention outright when breached, and the board only grows.
    warn_on_thin_headroom(bytes, REFERENCE_MAX_BYTES);
    Ok(texts)
}

pub(super) async fn references(
    state: &crate::api::AppState,
    marker: &str,
) -> anyhow::Result<Vec<String>> {
    let store = state.store.clone();
    let marker = marker.to_string();
    tokio::task::spawn_blocking(move || {
        let conn = store.read()?;
        reference_texts(&conn, &marker)
    })
    .await?
}

fn decode_reference(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).replace("\\/", "/")
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Report {
    pub measured: bool,
    pub n_considered: usize,
    pub removed: usize,
    pub bytes_freed: u64,
    pub kept: usize,
    pub why_unmeasured: Option<String>,
}

/// A single bounded system probe includes open files AND process working dirs.
/// Missing lsof, permissions, truncation or timeout defer cleanup altogether.
fn open_file_command() -> tokio::process::Command {
    // launchd's service PATH need not contain /usr/sbin. Use the OS-provided
    // executable directly, without changing the worker/service environment.
    #[cfg(target_os = "macos")]
    let program = "/usr/sbin/lsof";
    #[cfg(not(target_os = "macos"))]
    let program = "lsof";
    let mut command = tokio::process::Command::new(program);
    command.args([
        "-nP",
        "-F",
        "n",
        "-u",
        &unsafe { libc::geteuid() }.to_string(),
    ]);
    command
}

/// Cap on the open-file probe's output (AMUX-4791).
///
/// 8 MiB, until this box quietly grew past it. `lsof` here emits 8,565,894
/// bytes across ~182,000 open files, 2.1% over, and because the job defers
/// whenever it cannot enumerate open files, retention stopped running
/// altogether: three directories deferring every tick, indefinitely, with
/// nothing pruned from logs, evidence or audits on the machine that runs amux
/// 24/7. A 2% overshoot bought a total outage.
///
/// 128 MiB is ~15x the current measurement. The number is not the interesting
/// part though: ANY fixed cap converts into this same silent outage on the day
/// a host grows past it, so a bigger constant only moves the cliff. What
/// removes the silence is `warn_on_thin_headroom` below, which announces the
/// approach rather than the arrival.
const PROBE_MAX_BYTES: u64 = 128 * 1024 * 1024;

/// Announce a probe whose output is closing on the cap, while it still works.
///
/// The failure this comes from had no early signal at all: the probe succeeded
/// at 7.9 MiB and failed completely at 8.2 MiB, and the only difference a
/// reader saw was a WARN that had been steady-state long enough to stop being
/// news. Half the cap is arbitrary; announcing BEFORE the cliff is not.
///
/// Shared by BOTH capped probes in this module (the open-file enumeration and
/// the reference snapshot), because a cap whose breach defers retention is one
/// shape, not two.
/// Returns whether it warned, so a test can exercise THIS function rather than
/// restate its threshold — a re-typed predicate passes even when the shipped
/// one is wrong.
fn warn_on_thin_headroom(used: usize, max: u64) -> bool {
    if (used as u64).saturating_mul(2) <= max {
        return false;
    }
    tracing::warn!(
        probe_bytes = used,
        cap_bytes = max,
        pct_of_cap = (used as u64 * 100).checked_div(max).unwrap_or(0),
        "open-file probe is past half its output cap; retention defers entirely once it is exceeded (AMUX-4791)"
    );
    true
}

async fn open_paths() -> anyhow::Result<Vec<PathBuf>> {
    probe_open_paths(
        &mut open_file_command(),
        Duration::from_secs(5),
        PROBE_MAX_BYTES,
    )
    .await
}

/// The probe hit the OUTPUT cap rather than the time budget. Named so the
/// producer below and the test that tolerates it read one string instead of
/// two copies that can drift (AMUX-4787, same reason as `PROBE_TIMEOUT_REASON`
/// in `memory_consumers`).
///
/// This is the SECOND host-sized dimension, and it is the one that caught me
/// out: the card was written about deadlines, and a busy host is also a BIG
/// host. `lsof` here emits 8,565,894 bytes against the 8 MiB production cap,
/// 2.1% over, so the probe truncates and the job defers every tick.
const PROBE_TRUNCATED_REASON: &str = "open-file probe truncated";

async fn probe_open_paths(
    command: &mut tokio::process::Command,
    timeout: Duration,
    max: u64,
) -> anyhow::Result<Vec<PathBuf>> {
    use tokio::io::AsyncReadExt;
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            anyhow::anyhow!(
                "cannot start open-file probe {:?}: {error}",
                command.as_std().get_program()
            )
        })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("lsof stdout unavailable"))?;
    let mut data = Vec::new();
    tokio::time::timeout(timeout, async {
        stdout.take(max + 1).read_to_end(&mut data).await?;
        anyhow::ensure!(data.len() <= max as usize, "{PROBE_TRUNCATED_REASON}");
        warn_on_thin_headroom(data.len(), max);
        anyhow::ensure!(child.wait().await?.success(), "open-file probe failed");
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let text = std::str::from_utf8(&data)?;
    let paths: Vec<_> = text
        .lines()
        .filter_map(|line| line.strip_prefix('n'))
        .filter(|path| path.starts_with('/'))
        .map(PathBuf::from)
        .collect();
    anyhow::ensure!(!paths.is_empty(), "open-file probe returned no paths");
    Ok(paths)
}

// Metadata only: no file contents, no symlinks, and a fixed budget per tick.
fn old_tree_bytes(
    path: &Path,
    cutoff: SystemTime,
    budget: &mut usize,
    deadline: Instant,
) -> anyhow::Result<Option<u64>> {
    anyhow::ensure!(
        *budget > 0 && Instant::now() < deadline,
        "run-log scan budget exhausted"
    );
    *budget -= 1;
    let md = std::fs::symlink_metadata(path)?;
    if path.file_name().is_some_and(|name| name == ".git")
        || md.file_type().is_symlink()
        || md.modified()? >= cutoff
    {
        return Ok(None);
    }
    if md.is_file() {
        return Ok(Some(md.len()));
    }
    if !md.is_dir() {
        return Ok(None);
    }
    let mut bytes = 0;
    for entry in std::fs::read_dir(path)? {
        let Some(n) = old_tree_bytes(&entry?.path(), cutoff, budget, deadline)? else {
            return Ok(None);
        };
        bytes += n;
    }
    Ok(Some(bytes))
}

fn prune(logs: &Path, days: u64, refs: &[String], open: &[PathBuf]) -> Report {
    let mut report = Report {
        measured: true,
        ..Default::default()
    };
    let Some(cutoff) =
        SystemTime::now().checked_sub(Duration::from_secs(days.saturating_mul(86_400)))
    else {
        report.measured = false;
        report.why_unmeasured = Some("retention age out of range".into());
        return report;
    };
    let result = (|| -> anyhow::Result<()> {
        let md = match std::fs::symlink_metadata(logs) {
            Ok(md) => md,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(
            md.is_dir() && !md.file_type().is_symlink(),
            "retention root must be a real directory"
        );
        let mut budget = 20_000;
        let deadline = Instant::now() + Duration::from_secs(2);
        for entry in std::fs::read_dir(logs)? {
            let entry = entry?;
            // Only run directories; top-level logs have their own rotation.
            if !entry.file_type()?.is_dir() || entry.file_name().to_string_lossy().starts_with('.')
            {
                continue;
            }
            report.n_considered += 1;
            let path = entry.path();
            let relative = format!(
                "/{}/{}",
                logs.file_name().unwrap_or_default().to_string_lossy(),
                entry.file_name().to_string_lossy()
            );
            if refs.iter().any(|text| text.contains(&relative))
                || open.iter().any(|p| p.starts_with(&path))
            {
                report.kept += 1;
                continue;
            }
            let Some(bytes) = old_tree_bytes(&path, cutoff, &mut budget, deadline)? else {
                report.kept += 1;
                continue;
            };
            // Recheck the root after scanning. Child recency was checked above;
            // the age window and open-file snapshot protect ongoing runs.
            if std::fs::symlink_metadata(&path)?.modified()? >= cutoff {
                report.kept += 1;
                continue;
            }
            std::fs::remove_dir_all(&path)?;
            report.removed += 1;
            report.bytes_freed += bytes;
        }
        Ok(())
    })();
    if let Err(error) = result {
        report.measured = false;
        report.why_unmeasured = Some(error.to_string());
    }
    report
}

pub(super) async fn sweep(
    home: &Path,
    name: &str,
    days: u64,
    refs: anyhow::Result<Vec<String>>,
) -> Report {
    if days == 0 {
        return Report {
            why_unmeasured: Some("disabled by zero retain days".into()),
            ..Default::default()
        };
    }
    let result = async {
        let refs = refs?;
        let open = open_paths().await?;
        let logs = home.join(name);
        Ok::<_, anyhow::Error>(
            tokio::task::spawn_blocking(move || prune(&logs, days, &refs, &open)).await?,
        )
    }
    .await;
    let report = result.unwrap_or_else(|error| Report {
        why_unmeasured: Some(error.to_string()),
        ..Default::default()
    });
    if !report.measured {
        tracing::warn!(directory = name, reason = ?report.why_unmeasured, "diagnostic directory retention deferred");
    } else if report.removed > 0 {
        tracing::info!(
            directory = name,
            removed = report.removed,
            bytes_freed = report.bytes_freed,
            kept = report.kept,
            "diagnostic directory retention sweep"
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn age(path: &Path) {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(40 * 86_400))
            .unwrap();
    }

    fn run(logs: &Path, name: &str) -> PathBuf {
        let dir = logs.join(name);
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested/proof.json"), b"proof").unwrap();
        age(&dir.join("nested/proof.json"));
        age(&dir.join("nested"));
        age(&dir);
        dir
    }

    #[test]
    fn retention_removes_old_orphan_but_keeps_nested_writes_open_files_and_references() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join("logs");
        let orphan = run(&logs, "orphan");
        let fresh = run(&logs, "fresh");
        let active = run(&logs, "active");
        let linked = run(&logs, "linked");
        let saved = run(&logs, "saved");
        let cwd = run(&logs, "cwd");
        // An existing-file write leaves both ancestor directory mtimes OLD.
        std::fs::write(fresh.join("nested/proof.json"), b"new output").unwrap();
        let conn = crate::db::migrate::test_memdb();
        conn.execute("INSERT INTO _amux_task_artifacts(id,task_id,kind,ref_value,created_at,updated_at) VALUES('a','t','verification',?1,0,0)",
            [linked.join("nested/proof.json").to_string_lossy().replace('/', "%2F")]).unwrap();
        conn.execute("INSERT INTO _amux_messages(id,from_actor,target,body,created_at,delivery) VALUES('m','{}','{}',?1,'2026-09-01','{}')",
            [saved.join("nested/proof.json").to_string_lossy().replace('/', "\\/")]).unwrap();
        let refs = reference_texts(&conn, "/logs/").unwrap();
        assert_eq!(refs.len(), 2);
        let r = prune(
            &logs,
            30,
            &refs,
            &[active.join("nested/proof.json"), cwd.clone()],
        );
        assert!(r.measured, "{r:?}");
        assert_eq!(
            (r.n_considered, r.removed, r.bytes_freed, r.kept),
            (6, 1, 5, 5)
        );
        assert!(!orphan.exists());
        for path in [fresh, active, linked, saved, cwd] {
            assert!(path.exists(), "{}", path.display());
        }
    }

    #[test]
    fn symlinked_retention_root_cannot_delete_outside_amux() {
        let home = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let user_work = run(external.path(), "user-work");
        let logs = home.path().join("logs");
        std::os::unix::fs::symlink(external.path(), &logs).unwrap();
        let report = prune(&logs, 30, &[], &[]);
        assert!(!report.measured);
        assert_eq!(report.removed, 0);
        assert!(report.why_unmeasured.unwrap().contains("real directory"));
        assert!(user_work.exists());
    }

    #[test]
    fn symlinks_and_exhausted_scan_budget_never_authorize_deletion() {
        let home = tempfile::tempdir().unwrap();
        let logs = home.path().join("logs");
        let dir = run(&logs, "symlink");
        let outside = home.path().join("user-work");
        std::fs::write(&outside, b"keep").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("external")).unwrap();
        age(&dir);
        let r = prune(&logs, 30, &[], &[]);
        assert_eq!((r.removed, r.kept), (0, 1));
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
        assert!(old_tree_bytes(
            &dir,
            SystemTime::now(),
            &mut 0,
            Instant::now() + Duration::from_secs(1)
        )
        .is_err());
    }

    /// AMUX-4787. This unwrapped a 5s deadline, so it panicked with "deadline
    /// has elapsed" whenever the box was busy. `lsof` was timed at 5.5-6.7s
    /// here at load 38, enumerating ~182,000 open files, which is a fact about
    /// the machine and not about this probe.
    ///
    /// The deadline below is THE TEST'S, not production's: `open_paths` keeps
    /// its 5s budget, which is the right cost for a background tick that must
    /// not stall one. What this test claims is that the absolute lsof path
    /// works under the launchd PATH and that the parser sees a held file, and
    /// neither claim needs a stopwatch.
    ///
    /// Widening alone would have MOVED the flake rather than removed it, so
    /// the budgets stop being a panic as well. A missing binary, a non-zero
    /// exit and a parse that found no paths all still fail here.
    ///
    /// THERE ARE TWO HOST-SIZED DIMENSIONS, NOT ONE, and the first version of
    /// this fix only handled time. A busy host is also a BIG host: `lsof` here
    /// emits 8,565,894 bytes, 2.1% over the 8 MiB cap this test used to pass
    /// through from production, so the probe truncated and the test failed
    /// with "open-file probe truncated" on the very run that was meant to
    /// confirm the deadline fix. Both budgets are the TEST'S now, and both
    /// host conditions are tolerated by name.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn native_open_file_probe_works_with_launchd_path_and_observes_held_file() {
        // Both generous, for the same reason: `open_paths` keeps production's
        // 5s and 8 MiB, which are the right costs for a background tick. What
        // this test claims is that the absolute lsof path works under the
        // launchd PATH and that the parser sees a held file, and neither claim
        // needs a stopwatch or a byte budget.
        const TEST_PROBE_TIMEOUT: Duration = Duration::from_secs(60);
        const TEST_PROBE_MAX_BYTES: u64 = 256 * 1024 * 1024;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("held.txt");
        let _held = std::fs::File::create(&path).unwrap();
        let mut command = open_file_command();
        command.env("PATH", "/usr/bin:/bin");
        match probe_open_paths(&mut command, TEST_PROBE_TIMEOUT, TEST_PROBE_MAX_BYTES).await {
            Ok(paths) => assert!(
                paths.contains(&path) || paths.contains(&path.canonicalize().unwrap()),
                "held file missing from native probe"
            ),
            Err(error) => {
                // The tolerated set is exactly two, both meaning "this machine
                // is bigger than the budget": tokio's `Elapsed`, matched by
                // TYPE, and the producer's own truncation constant. Anything
                // else is a defect and still fails.
                let elapsed = error
                    .downcast_ref::<tokio::time::error::Elapsed>()
                    .is_some();
                let truncated = error.to_string() == PROBE_TRUNCATED_REASON;
                assert!(
                    elapsed || truncated,
                    "the native probe failed for a reason that is not host size: {error}"
                );
                eprintln!(
                    "lsof exceeded {TEST_PROBE_TIMEOUT:?} / {TEST_PROBE_MAX_BYTES} bytes on this host, \
                     so the held-file assertion did not run: {error}"
                );
            }
        }
    }

    /// AMUX-4791: the cap still REFUSES, and the approach is announced.
    ///
    /// Both halves in one test because they are one decision. Raising the cap
    /// without an early signal only moves the cliff — this box failed at 2.1%
    /// over 8 MiB with no warning at all — and adding the signal while
    /// removing the refusal would let a truncated enumeration authorise a
    /// delete, which is the one outcome this job exists to prevent.
    #[tokio::test]
    async fn the_output_cap_still_refuses_and_its_approach_is_announced() {
        // One byte over the cap is still a hard refusal, carrying the shared
        // reason the test in this module tolerates by name.
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "printf 'n/tmp/a\\nn/tmp/bbbb\\n'"]);
        let err = probe_open_paths(&mut command, Duration::from_secs(5), 8)
            .await
            .expect_err("output over the cap must refuse");
        assert_eq!(err.to_string(), PROBE_TRUNCATED_REASON, "{err}");

        // Comfortably under the cap: the probe measures, and nothing about the
        // headroom warning changes the returned value.
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args(["-c", "printf 'p1\\nn/tmp/a\\n'"]);
        assert_eq!(
            probe_open_paths(&mut command, Duration::from_secs(5), 1024)
                .await
                .unwrap(),
            vec![PathBuf::from("/tmp/a")],
        );

        // The SHIPPED predicate, called rather than restated: strictly more
        // than half the cap, so an exactly-half probe stays quiet and the
        // signal cannot be read as "any large probe". The 8,565,894-against-
        // 8-MiB row is the real measurement that opened this card.
        for (used, max, want) in [
            (4usize, 8u64, false),
            (5, 8, true),
            (0, 8, false),
            (129, 256, true),
            (8_565_894, 8 * 1024 * 1024, true),
            (8_565_894, PROBE_MAX_BYTES, false),
        ] {
            assert_eq!(
                warn_on_thin_headroom(used, max),
                want,
                "used={used} max={max}"
            );
        }
        // No bare `assert!(PROBE_MAX_BYTES > ...)` here: clippy rejects it as a
        // constant assertion, and it is right — a comparison of two literals
        // cannot fail, so it would read as coverage while testing nothing. The
        // last two rows above make the same claim through the shipped
        // predicate, where a regressed cap really does change the answer.
    }

    #[tokio::test]
    async fn open_file_probe_rejects_failed_empty_truncated_and_hung_output() {
        for script in [
            "exit 1",
            "printf 'p123\\n'",
            "head -c 2048 /dev/zero",
            "exec sleep 10",
        ] {
            let mut command = tokio::process::Command::new("/bin/sh");
            command.args(["-c", script]);
            assert!(
                probe_open_paths(&mut command, Duration::from_millis(100), 1024)
                    .await
                    .is_err(),
                "{script}"
            );
        }
        let mut command = tokio::process::Command::new("/bin/sh");
        command.args([
            "-c",
            "printf 'p123\\nn/tmp/active-run\\nn/tmp/open-file\\n'",
        ]);
        assert_eq!(
            probe_open_paths(&mut command, Duration::from_secs(1), 1024)
                .await
                .unwrap(),
            vec![
                PathBuf::from("/tmp/active-run"),
                PathBuf::from("/tmp/open-file")
            ]
        );
    }

    #[test]
    fn registered_evidence_and_repository_folders_survive_age_retention() {
        let home = tempfile::tempdir().unwrap();
        for name in ["evidence", "audits"] {
            let root = home.path().join(name);
            let linked = run(&root, "linked");
            let orphan = run(&root, "orphan");
            let repo = run(&root, "repo");
            std::fs::create_dir(repo.join(".git")).unwrap();
            age(&repo.join(".git"));
            age(&repo);
            let report = prune(
                &root,
                7,
                &[linked
                    .join("nested/proof.json")
                    .to_string_lossy()
                    .into_owned()],
                &[],
            );
            assert!(report.measured);
            assert_eq!((report.removed, report.kept), (1, 2));
            assert!(!orphan.exists());
            assert!(linked.exists());
            assert!(repo.exists());
        }
    }

    #[test]
    fn reference_probe_ignores_large_prose_that_only_mentions_logs() {
        let conn = crate::db::migrate::test_memdb();
        let prose = format!("Investigate logs: {}", "x".repeat(16 * 1024 * 1024));
        conn.execute("INSERT INTO saved_messages(text) VALUES(?1)", [prose])
            .unwrap();
        conn.execute(
            "INSERT INTO saved_messages(text) VALUES('/logs/real/proof.json')",
            [],
        )
        .unwrap();
        assert_eq!(
            reference_texts(&conn, "/logs/").unwrap(),
            vec!["/logs/real/proof.json"]
        );
    }

    #[test]
    fn oversized_reference_snapshot_defers_instead_of_returning_partial_references() {
        let conn = crate::db::migrate::test_memdb();
        let text = format!("/logs/keep/{}", "x".repeat(16 * 1024 * 1024));
        conn.execute("INSERT INTO saved_messages(text) VALUES(?1)", [text])
            .unwrap();
        assert!(reference_texts(&conn, "/logs/")
            .unwrap_err()
            .to_string()
            .contains("16 MiB"));
    }

    #[tokio::test]
    async fn unavailable_references_and_disabled_retention_keep_old_run() {
        let home = tempfile::tempdir().unwrap();
        let dir = run(&home.path().join("logs"), "orphan");
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let r = sweep(home.path(), "logs", 30, reference_texts(&conn, "/logs/")).await;
        assert!(!r.measured);
        assert!(r.why_unmeasured.unwrap().contains("no such table"));
        assert!(dir.exists());
        let r = sweep(home.path(), "logs", 0, Ok(vec![])).await;
        assert!(r.why_unmeasured.unwrap().contains("disabled"));
        assert!(dir.exists());
    }
}
