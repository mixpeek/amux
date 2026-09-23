//! Verification pipeline (RR-0076, Invariants 7, 28).
//!
//! Executes a task's verification criteria cheapest-verifier-first: the
//! DETERMINISTIC verifiers actually run (command, HTTP, file); the expensive
//! or unimplemented ones report honestly instead of green-lighting
//! (ethos rule 7: a check that cannot fail is theatre). The verdict feeds
//! the board's done -> verified / verification-failed transitions.

use amux_core::board::GateCriterion;
use amux_core::verification::{
    run_cheapest_first, CriteriaRun, Evidence, EvidenceSource, VerificationResult, VerifierKind,
};
use serde::Serialize;
use std::io::{Read, Seek, SeekFrom};
use std::time::Duration;

/// Per-verifier execution budget. A verifier that hangs is a failed
/// verifier, not a hung pipeline.
const VERIFIER_TIMEOUT: Duration = Duration::from_secs(60);
const OUTPUT_LIMIT: u64 = 32 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct VerifierDetail {
    pub verifier: VerifierKind,
    pub result: VerificationResult,
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationExecution {
    pub run: CriteriaRun,
    pub evidence: Vec<Evidence>,
    pub details: Vec<VerifierDetail>,
    pub duration_ms: u64,
}

fn bounded_read(file: &mut std::fs::File) -> String {
    let _ = file.seek(SeekFrom::Start(0));
    let mut bytes = Vec::new();
    let _ = file.take(OUTPUT_LIMIT).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn execute_command(cmd: &str, cwd: &str, timeout: Duration) -> VerifierDetail {
    let started = std::time::Instant::now();
    let mut stdout = match tempfile::tempfile() {
        Ok(f) => f,
        Err(e) => {
            let result = VerificationResult::Failed {
                reason: format!("cannot allocate verifier stdout: {e}"),
            };
            return VerifierDetail {
                verifier: VerifierKind::Command {
                    cmd: cmd.into(),
                    expected_exit: 0,
                },
                result,
                duration_ms: 0,
                stdout: String::new(),
                stderr: String::new(),
            };
        }
    };
    let mut stderr = match tempfile::tempfile() {
        Ok(f) => f,
        Err(e) => {
            let result = VerificationResult::Failed {
                reason: format!("cannot allocate verifier stderr: {e}"),
            };
            return VerifierDetail {
                verifier: VerifierKind::Command {
                    cmd: cmd.into(),
                    expected_exit: 0,
                },
                result,
                duration_ms: 0,
                stdout: String::new(),
                stderr: String::new(),
            };
        }
    };
    let child = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(if cwd.is_empty() { "." } else { cwd })
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(
            stdout.try_clone().expect("temp stdout clone"),
        ))
        .stderr(std::process::Stdio::from(
            stderr.try_clone().expect("temp stderr clone"),
        ))
        .spawn();
    let (result, timed_out) = match child {
        Err(e) => (
            VerificationResult::Failed {
                reason: format!("command failed to start: {cmd}: {e}"),
            },
            false,
        ),
        Ok(mut child) => loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let code = status.code().unwrap_or(-1);
                    break (
                        VerificationResult::Failed {
                            reason: format!("command exited {code}: {cmd}"),
                        },
                        false,
                    );
                }
                Ok(None) if started.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break (
                        VerificationResult::Failed {
                            reason: format!(
                                "command timed out after {}s: {cmd}",
                                timeout.as_secs()
                            ),
                        },
                        true,
                    );
                }
                Err(e) => {
                    break (
                        VerificationResult::Failed {
                            reason: format!("command wait failed: {cmd}: {e}"),
                        },
                        false,
                    )
                }
            }
        },
    };
    let stdout_text = bounded_read(&mut stdout);
    let stderr_text = bounded_read(&mut stderr);
    VerifierDetail {
        verifier: VerifierKind::Command {
            cmd: cmd.into(),
            expected_exit: 0,
        },
        result,
        duration_ms: started.elapsed().as_millis() as u64,
        stdout: stdout_text,
        stderr: if timed_out {
            format!("{stderr_text}\n[terminated by verifier timeout]")
        } else {
            stderr_text
        },
    }
}

fn expect_exit(mut detail: VerifierDetail, expected_exit: i32) -> VerifierDetail {
    if let VerificationResult::Failed { reason } = &detail.result {
        if let Some(code) = reason
            .strip_prefix("command exited ")
            .and_then(|s| s.split(':').next())
            .and_then(|s| s.parse::<i32>().ok())
        {
            detail.result = if code == expected_exit {
                VerificationResult::Passed
            } else {
                VerificationResult::Failed {
                    reason: format!(
                        "command exited {code} (expected {expected_exit}) — stderr: {}",
                        detail.stderr.chars().take(400).collect::<String>()
                    ),
                }
            };
        }
    }
    detail
}

/// Execute one criterion's verifier. Blocking (called from the sync runner
/// closure); the async wrapper below parks the whole run on the blocking
/// pool.
fn execute(kind: &VerifierKind, cwd: &str) -> VerifierDetail {
    let started = std::time::Instant::now();
    let mut detail = match kind {
        VerifierKind::Command { cmd, expected_exit } => {
            let mut detail = execute_command(cmd, cwd, VERIFIER_TIMEOUT);
            detail.verifier = kind.clone();
            expect_exit(detail, *expected_exit)
        }
        VerifierKind::HttpCheck {
            url,
            expected_status,
        } => {
            let client = reqwest::blocking::Client::builder()
                .timeout(VERIFIER_TIMEOUT)
                .danger_accept_invalid_certs(url.starts_with("https://localhost"))
                .build();
            match client.and_then(|c| c.get(url).send()) {
                Ok(resp) => {
                    let got = resp.status().as_u16();
                    let result = if got == *expected_status {
                        VerificationResult::Passed
                    } else {
                        VerificationResult::Failed {
                            reason: format!("{url} returned {got}, expected {expected_status}"),
                        }
                    };
                    VerifierDetail {
                        verifier: kind.clone(),
                        result,
                        duration_ms: 0,
                        stdout: format!("HTTP {got}"),
                        stderr: String::new(),
                    }
                }
                Err(e) => VerifierDetail {
                    verifier: kind.clone(),
                    result: VerificationResult::Failed {
                        reason: format!("http check unreachable: {url}: {e}"),
                    },
                    duration_ms: 0,
                    stdout: String::new(),
                    stderr: e.to_string(),
                },
            }
        }
        VerifierKind::FileExists { path } => {
            let resolved = if path.is_absolute() {
                path.clone()
            } else {
                std::path::Path::new(cwd).join(path)
            };
            let result = if resolved.exists() {
                VerificationResult::Passed
            } else {
                VerificationResult::Failed {
                    reason: format!("artifact missing: {}", resolved.display()),
                }
            };
            VerifierDetail {
                verifier: kind.clone(),
                result,
                duration_ms: 0,
                stdout: resolved.display().to_string(),
                stderr: String::new(),
            }
        }
        VerifierKind::Temporal { after } => {
            let now = chrono::Utc::now();
            let result = if now >= *after {
                VerificationResult::Passed
            } else {
                VerificationResult::Failed {
                    reason: format!(
                        "temporal gate: not yet past {} (now: {})",
                        after.to_rfc3339(),
                        now.to_rfc3339()
                    ),
                }
            };
            VerifierDetail {
                verifier: kind.clone(),
                result,
                duration_ms: 0,
                stdout: now.to_rfc3339(),
                stderr: String::new(),
            }
        }
        VerifierKind::PlaywrightAssertion { script } => {
            let mut detail = execute_command(
                &format!("node -e {}", shell_quote(script)),
                cwd,
                VERIFIER_TIMEOUT,
            );
            detail.verifier = kind.clone();
            expect_exit(detail, 0)
        }
        VerifierKind::ModelJudgment { prompt } => VerifierDetail {
            verifier: kind.clone(),
            result: VerificationResult::Failed {
                reason: format!(
                    "model judgment requires a configured independent verifier: {prompt}"
                ),
            },
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
        },
    };
    detail.duration_ms = started.elapsed().as_millis() as u64;
    detail
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// Run a full criteria set for a task. Deterministic verifiers execute in
/// cost order with required-failure short-circuit (core's runner decides
/// ordering; this only supplies muscle).
pub fn run_verification(criteria: &[GateCriterion], cwd: &str) -> CriteriaRun {
    run_cheapest_first(criteria, |c| execute(&c.verifier, cwd).result)
}

pub fn run_verification_detailed(criteria: &[GateCriterion], cwd: &str) -> VerificationExecution {
    let started = std::time::Instant::now();
    let mut details = Vec::new();
    let run = run_cheapest_first(criteria, |c| {
        let detail = execute(&c.verifier, cwd);
        let result = detail.result.clone();
        details.push(detail);
        result
    });
    let evidence = run
        .ran
        .iter()
        .filter(|outcome| outcome.result.is_passed())
        .map(|outcome| {
            let criterion = &criteria[outcome.index];
            Evidence {
                kind: criterion.verifier.evidence_kind(),
                description: format!("passed: {}", criterion.description),
                artifact: Some(format!("verification-detail:{}", outcome.index)),
                produced_at: chrono::Utc::now(),
                source: EvidenceSource::Independent,
            }
        })
        .collect();
    VerificationExecution {
        run,
        evidence,
        details,
        duration_ms: started.elapsed().as_millis() as u64,
    }
}

/// Async wrapper: verification runs shell commands and blocking HTTP —
/// park it off the runtime workers.
pub async fn run_verification_async(
    criteria: Vec<GateCriterion>,
    cwd: String,
) -> anyhow::Result<VerificationExecution> {
    Ok(tokio::task::spawn_blocking(move || run_verification_detailed(&criteria, &cwd)).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use amux_core::verification::EvidenceKind;

    fn crit(verifier: VerifierKind, required: bool) -> GateCriterion {
        GateCriterion {
            description: "test".into(),
            verifier,
            required,
        }
    }

    #[test]
    fn command_verifier_passes_and_fails_on_exit_code() {
        let run = run_verification(
            &[crit(
                VerifierKind::Command {
                    cmd: "true".into(),
                    expected_exit: 0,
                },
                true,
            )],
            "",
        );
        assert_eq!(run.verdict, VerificationResult::Passed);

        let run = run_verification(
            &[crit(
                VerifierKind::Command {
                    cmd: "false".into(),
                    expected_exit: 0,
                },
                true,
            )],
            "",
        );
        assert!(
            matches!(&run.verdict, VerificationResult::Failed { reason } if reason.contains("exited 1"))
        );
    }

    #[test]
    fn file_exists_verifier() {
        let run = run_verification(
            &[crit(
                VerifierKind::FileExists {
                    path: "/etc/hosts".into(),
                },
                true,
            )],
            "",
        );
        assert_eq!(run.verdict, VerificationResult::Passed);
        let run = run_verification(
            &[crit(
                VerifierKind::FileExists {
                    path: "/nonexistent-xyz".into(),
                },
                true,
            )],
            "",
        );
        assert!(matches!(run.verdict, VerificationResult::Failed { .. }));
    }

    #[test]
    fn relative_file_evidence_is_resolved_inside_the_task_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("artifact.txt"), "proof").unwrap();
        let run = run_verification(
            &[crit(
                VerifierKind::FileExists {
                    path: "artifact.txt".into(),
                },
                true,
            )],
            dir.path().to_str().unwrap(),
        );
        assert_eq!(run.verdict, VerificationResult::Passed);
    }

    #[test]
    fn free_failure_short_circuits_expensive_verifiers() {
        // The failing command is required and FREE; the model judgment must
        // never execute (rule 2: no model calls a free check can preempt).
        let run = run_verification(
            &[
                crit(
                    VerifierKind::ModelJudgment {
                        prompt: "judge".into(),
                    },
                    true,
                ),
                crit(
                    VerifierKind::Command {
                        cmd: "false".into(),
                        expected_exit: 0,
                    },
                    true,
                ),
            ],
            "",
        );
        assert!(matches!(run.verdict, VerificationResult::Failed { .. }));
        // The command ran (index 1 in original order), the judgment did not.
        assert_eq!(run.skipped, vec![0], "model judgment skipped: {run:?}");
    }

    #[test]
    fn playwright_assertions_execute_and_fail_honestly() {
        let passed = run_verification(
            &[crit(
                VerifierKind::PlaywrightAssertion {
                    script: "process.exit(0)".into(),
                },
                true,
            )],
            "",
        );
        assert_eq!(passed.verdict, VerificationResult::Passed);

        let run = run_verification(
            &[crit(
                VerifierKind::PlaywrightAssertion {
                    script: "process.exit(7)".into(),
                },
                true,
            )],
            "",
        );
        assert!(
            matches!(&run.verdict, VerificationResult::Failed { reason } if reason.contains("exited 7")),
            "{run:?}"
        );
    }

    #[test]
    fn command_runs_in_the_given_cwd() {
        let dir = std::env::temp_dir().join(format!("amux-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), "x").unwrap();
        let run = run_verification(
            &[crit(
                VerifierKind::Command {
                    cmd: "test -f marker".into(),
                    expected_exit: 0,
                },
                true,
            )],
            dir.to_str().unwrap(),
        );
        assert_eq!(run.verdict, VerificationResult::Passed);
        std::fs::remove_dir_all(&dir).ok();
        let _ = EvidenceKind::CommandOutput; // silence unused-import style drift
    }
}
