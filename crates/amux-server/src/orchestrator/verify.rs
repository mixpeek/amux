//! Verification pipeline (RR-0076, Invariants 7, 28).
//!
//! Executes a task's verification criteria cheapest-verifier-first: the
//! DETERMINISTIC verifiers actually run (command, HTTP, file); the expensive
//! or unimplemented ones report honestly instead of green-lighting
//! (ethos rule 7: a check that cannot fail is theatre). The verdict feeds
//! the board's done -> verified / verification-failed transitions.

use amux_core::board::GateCriterion;
use amux_core::verification::{run_cheapest_first, CriteriaRun, VerificationResult, VerifierKind};
use std::time::Duration;

/// Per-verifier execution budget. A verifier that hangs is a failed
/// verifier, not a hung pipeline.
const VERIFIER_TIMEOUT: Duration = Duration::from_secs(60);

/// Execute one criterion's verifier. Blocking (called from the sync runner
/// closure); the async wrapper below parks the whole run on the blocking
/// pool.
fn execute(kind: &VerifierKind, cwd: &str) -> VerificationResult {
    match kind {
        VerifierKind::Command { cmd, expected_exit } => {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .current_dir(if cwd.is_empty() { "." } else { cwd })
                .stdin(std::process::Stdio::null())
                .output();
            match out {
                Ok(out) => {
                    let code = out.status.code().unwrap_or(-1);
                    if code == *expected_exit {
                        VerificationResult::Passed
                    } else {
                        let tail: String = String::from_utf8_lossy(&out.stderr)
                            .chars()
                            .rev()
                            .take(400)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect();
                        VerificationResult::Failed {
                            reason: format!(
                                "command exited {code} (expected {expected_exit}): {cmd} — stderr tail: {tail}"
                            ),
                        }
                    }
                }
                Err(e) => VerificationResult::Failed {
                    reason: format!("command failed to start: {cmd}: {e}"),
                },
            }
        }
        VerifierKind::HttpCheck { url, expected_status } => {
            let client = reqwest::blocking::Client::builder()
                .timeout(VERIFIER_TIMEOUT)
                .danger_accept_invalid_certs(url.starts_with("https://localhost"))
                .build();
            match client.and_then(|c| c.get(url).send()) {
                Ok(resp) => {
                    let got = resp.status().as_u16();
                    if got == *expected_status {
                        VerificationResult::Passed
                    } else {
                        VerificationResult::Failed {
                            reason: format!("{url} returned {got}, expected {expected_status}"),
                        }
                    }
                }
                Err(e) => VerificationResult::Failed {
                    reason: format!("http check unreachable: {url}: {e}"),
                },
            }
        }
        VerifierKind::FileExists { path } => {
            if path.exists() {
                VerificationResult::Passed
            } else {
                VerificationResult::Failed {
                    reason: format!("artifact missing: {}", path.display()),
                }
            }
        }
        VerifierKind::Temporal { after } => {
            if chrono::Utc::now() >= *after {
                VerificationResult::Passed
            } else {
                VerificationResult::Failed {
                    reason: format!(
                        "temporal gate: not yet past {} (now: {})",
                        after.to_rfc3339(),
                        chrono::Utc::now().to_rfc3339()
                    ),
                }
            }
        }
        VerifierKind::PlaywrightAssertion { script } => VerificationResult::Failed {
            reason: format!(
                "playwright verifier not yet executable in this phase (script: {script}) — \
                 criterion cannot be honestly satisfied; keep the task at done"
            ),
        },
        VerifierKind::ModelJudgment { prompt } => VerificationResult::Failed {
            reason: format!(
                "model-judgment verifier not yet wired (prompt: {prompt}) — \
                 criterion cannot be honestly satisfied; keep the task at done"
            ),
        },
    }
}

/// Run a full criteria set for a task. Deterministic verifiers execute in
/// cost order with required-failure short-circuit (core's runner decides
/// ordering; this only supplies muscle).
pub fn run_verification(criteria: &[GateCriterion], cwd: &str) -> CriteriaRun {
    run_cheapest_first(criteria, |c| execute(&c.verifier, cwd))
}

/// Async wrapper: verification runs shell commands and blocking HTTP —
/// park it off the runtime workers.
pub async fn run_verification_async(
    criteria: Vec<GateCriterion>,
    cwd: String,
) -> anyhow::Result<CriteriaRun> {
    Ok(tokio::task::spawn_blocking(move || run_verification(&criteria, &cwd)).await?)
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
            &[crit(VerifierKind::Command { cmd: "true".into(), expected_exit: 0 }, true)],
            "",
        );
        assert_eq!(run.verdict, VerificationResult::Passed);

        let run = run_verification(
            &[crit(VerifierKind::Command { cmd: "false".into(), expected_exit: 0 }, true)],
            "",
        );
        assert!(matches!(&run.verdict, VerificationResult::Failed { reason } if reason.contains("exited 1")));
    }

    #[test]
    fn file_exists_verifier() {
        let run = run_verification(
            &[crit(VerifierKind::FileExists { path: "/etc/hosts".into() }, true)],
            "",
        );
        assert_eq!(run.verdict, VerificationResult::Passed);
        let run = run_verification(
            &[crit(VerifierKind::FileExists { path: "/nonexistent-xyz".into() }, true)],
            "",
        );
        assert!(matches!(run.verdict, VerificationResult::Failed { .. }));
    }

    #[test]
    fn free_failure_short_circuits_expensive_verifiers() {
        // The failing command is required and FREE; the model judgment must
        // never execute (rule 2: no model calls a free check can preempt).
        let run = run_verification(
            &[
                crit(VerifierKind::ModelJudgment { prompt: "judge".into() }, true),
                crit(VerifierKind::Command { cmd: "false".into(), expected_exit: 0 }, true),
            ],
            "",
        );
        assert!(matches!(run.verdict, VerificationResult::Failed { .. }));
        // The command ran (index 1 in original order), the judgment did not.
        assert_eq!(run.skipped, vec![0], "model judgment skipped: {run:?}");
    }

    #[test]
    fn unimplemented_verifiers_fail_honestly() {
        let run = run_verification(
            &[crit(VerifierKind::PlaywrightAssertion { script: "x".into() }, true)],
            "",
        );
        assert!(
            matches!(&run.verdict, VerificationResult::Failed { reason } if reason.contains("cannot be honestly satisfied")),
            "{run:?}"
        );
    }

    #[test]
    fn command_runs_in_the_given_cwd() {
        let dir = std::env::temp_dir().join(format!("amux-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), "x").unwrap();
        let run = run_verification(
            &[crit(VerifierKind::Command { cmd: "test -f marker".into(), expected_exit: 0 }, true)],
            dir.to_str().unwrap(),
        );
        assert_eq!(run.verdict, VerificationResult::Passed);
        std::fs::remove_dir_all(&dir).ok();
        let _ = EvidenceKind::CommandOutput; // silence unused-import style drift
    }
}
