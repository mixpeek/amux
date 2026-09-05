//! Verification vocabulary: what proved a claim, and at what cost
//! (RR-0015; Invariants 7 and 28).
//!
//! Done is a worker's claim; Verified is the harness's conclusion (Invariant 7).
//! The types here are how that conclusion is reached and recorded:
//!
//! - [`VerifierKind`] — the single evaluation primitive, shared by gates
//!   (Invariant 18) and verification (Invariant 28). One spec, one evaluation
//!   path. There is deliberately NO `HumanReview` variant: capability policy
//!   replaces approval gates (Invariant 52) — nothing in the system blocks on a
//!   human pressing a button.
//! - [`Evidence`] — what a verification produced. Every piece carries its
//!   provenance ([`EvidenceSource`]), because three separate incidents
//!   (`8bc9eb3`, `7870384`, `9cd2892`) were verifications satisfied by output
//!   the verified actor produced itself. Evidence that cannot express WHO
//!   produced it is the "instrument cannot express the discriminator" failure
//!   (ethos rule 4) applied to verification.
//! - [`Verification`] / [`VerificationResult`] — the durable record and its
//!   verdict. `Failed` returns the task to in-progress via
//!   `board::BoardTransition::VerificationFailed` (Invariant 7).
//! - [`run_cheapest_first`] — the cost-ordered runner: free checks run first,
//!   and once a required free check fails, expensive verifiers are never
//!   invoked (Invariant 28: "if the free checks fail, expensive ones never
//!   run"). Never call a model when a deterministic check suffices.
//!
//! CORE IS PURE: nothing here executes a command, makes an HTTP call, or reads
//! a clock. `VerifierKind` is a *spec*; the server-side gate runner executes it
//! and feeds results back through the closure in [`run_cheapest_first`].

use crate::board::GateCriterion;
use crate::ids::{CriterionId, TaskId, VerificationId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The single evaluation primitive, unified across gates and verification
/// (Invariant 18: "GateEvaluator is now VerifierKind").
///
/// Variants are declared in COST ORDER, cheapest first:
/// `Command < HttpCheck < FileExists < PlaywrightAssertion < ModelJudgment`.
/// The derived `Ord` follows declaration order, so sorting a mixed list yields
/// cheapest-first directly; [`VerifierKind::cost_rank`] is the explicit,
/// semantically-meaningful comparison (derived `Ord` additionally tie-breaks
/// on payload fields, which is harmless for sorting).
///
/// No `HumanReview` variant (Invariant 52): a check a human must perform is a
/// capability-policy concern, not a verifier — a gate that can only be
/// satisfied by waiting on a person is where autonomous work goes to die
/// (Invariant 10).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerifierKind {
    /// Run a command, compare exit code. Free (Invariant 28 cost table).
    Command { cmd: String, expected_exit: i32 },
    /// HTTP request, compare status. Free. (`String`, not a `Url` type: core
    /// takes no url-crate dependency; the runner validates at execution.)
    HttpCheck { url: String, expected_status: u16 },
    /// Artifact exists on disk (`stat`). Free.
    FileExists { path: PathBuf },
    /// Time gate: blocks until a date passes. Free (a clock read). Used for
    /// release embargoes, scheduled transitions, and follow-up reminders.
    Temporal { after: DateTime<Utc> },
    /// Browser assertion. Cheap — deterministic, but costs a browser.
    PlaywrightAssertion { script: String },
    /// Model judgment. Expensive, non-deterministic, runs LAST and only if
    /// the deterministic checks passed (rule 2 of the ethos: never spend a
    /// model call on something a free check can decide).
    ModelJudgment { prompt: String },
}

impl VerifierKind {
    /// Explicit cost ordering, 0 = cheapest. This is the number the gate
    /// runner sorts by; it exists (rather than only derived `Ord`) so the
    /// ordering is greppable and cannot silently change if someone reorders
    /// the enum for readability.
    pub fn cost_rank(&self) -> u8 {
        match self {
            VerifierKind::Command { .. } => 0,
            VerifierKind::HttpCheck { .. } => 1,
            VerifierKind::FileExists { .. } => 2,
            VerifierKind::Temporal { .. } => 3,
            VerifierKind::PlaywrightAssertion { .. } => 4,
            VerifierKind::ModelJudgment { .. } => 5,
        }
    }

    /// Free verifiers per the Invariant 28 cost table: exit codes, HTTP
    /// statuses, and `stat` cost nothing. A required free-verifier failure
    /// short-circuits the whole run.
    pub fn is_free(&self) -> bool {
        self.cost_rank() <= 3
    }

    /// Deterministic verifiers run before `ModelJudgment` (Invariant 18:
    /// "ModelJudgment runs last and only if deterministic checks pass").
    /// Playwright counts as deterministic: a scripted assertion either holds
    /// or does not.
    pub fn is_deterministic(&self) -> bool {
        !matches!(self, VerifierKind::ModelJudgment { .. })
    }

    /// The kind of evidence this verifier produces when it runs. Gates match
    /// submitted [`Evidence`] against their criteria through this mapping
    /// (`board::GateCriterion::satisfied_by`): core checks evidence SHAPE,
    /// the runner vouched for evidence TRUTH when it produced the record.
    pub fn evidence_kind(&self) -> EvidenceKind {
        match self {
            VerifierKind::Command { .. } => EvidenceKind::CommandOutput,
            VerifierKind::HttpCheck { .. } => EvidenceKind::HttpResponse,
            VerifierKind::FileExists { .. } => EvidenceKind::FileStat,
            VerifierKind::Temporal { .. } => EvidenceKind::TemporalCheck,
            VerifierKind::PlaywrightAssertion { .. } => EvidenceKind::PlaywrightArtifact,
            VerifierKind::ModelJudgment { .. } => EvidenceKind::ModelTranscript,
        }
    }

    /// The command a blocked caller should run to satisfy this criterion —
    /// the "suggested command" line of `why-blocked` (Invariant 18: no opaque
    /// "gate failed"). `None` for verifiers with no one-line shell equivalent;
    /// the why-blocked output still names the criterion and the missing
    /// evidence kind.
    pub fn suggested_command(&self) -> Option<String> {
        match self {
            VerifierKind::Command { cmd, .. } => Some(cmd.clone()),
            VerifierKind::HttpCheck {
                url,
                expected_status,
            } => Some(format!(
                "curl -s -o /dev/null -w '%{{http_code}}' '{url}'  # expect {expected_status}"
            )),
            VerifierKind::FileExists { path } => Some(format!("test -e '{}'", path.display())),
            VerifierKind::Temporal { after } => {
                Some(format!("date -d @{} '+%Y-%m-%dT%H:%M:%S'  # gate opens at this time", after.timestamp()))
            }
            VerifierKind::PlaywrightAssertion { .. } => None,
            VerifierKind::ModelJudgment { .. } => None,
        }
    }
}

/// What kind of artifact a verification produced. Mirrors [`VerifierKind`]
/// one-to-one (see [`VerifierKind::evidence_kind`]) so gate criteria can be
/// matched against submitted evidence without core re-running anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    CommandOutput,
    HttpResponse,
    FileStat,
    TemporalCheck,
    PlaywrightArtifact,
    ModelTranscript,
}

/// Who produced a piece of evidence (Invariant 28, evidence independence):
/// "verification cannot be satisfied solely by output produced by the actor
/// whose claim is being verified, when independent evidence is available."
///
/// The default is `SelfReported` — the WEAKEST trust level — so evidence that
/// never declared its provenance is never silently treated as independent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    /// The harness or an external tool ran the check itself.
    Independent,
    /// The actor being verified reported it. Insufficient on its own when
    /// independent evidence is available.
    #[default]
    SelfReported,
    /// Self-reported AND independently confirmed.
    Corroborated,
}

/// What a verification produced: the artifact trail behind a verdict.
///
/// `artifact` is a reference (path, URL, or entity id) to the produced thing —
/// a test log, a screenshot, an HTTP transcript — so a later reader can check
/// the evidence itself rather than trusting the description (ethos rule 6:
/// an audit trail that is claimed but not inspectable is not an audit trail).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// What kind of artifact this is; matched against gate criteria via
    /// [`VerifierKind::evidence_kind`].
    pub kind: EvidenceKind,
    /// Human-readable one-liner ("cargo test: 214 passed, 0 failed").
    pub description: String,
    /// Path / URL / id of the artifact itself, when one exists.
    pub artifact: Option<String>,
    /// When the evidence was produced. Core is pure: this is supplied by the
    /// producer, never read from a clock here.
    pub produced_at: DateTime<Utc>,
    /// Provenance (Invariant 28). `#[serde(default)]` = `SelfReported`, so
    /// records written before this field existed deserialize at the weakest
    /// trust level rather than an inflated one.
    #[serde(default)]
    pub source: EvidenceSource,
}

impl Evidence {
    /// True when the harness itself (or an external tool) stands behind this
    /// evidence — `Independent` or `Corroborated`.
    pub fn is_independent(&self) -> bool {
        matches!(
            self.source,
            EvidenceSource::Independent | EvidenceSource::Corroborated
        )
    }
}

/// The verdict of a verification (Invariant 7).
///
/// `Failed` returns the task to in-progress: the board transition that
/// implements this is `board::BoardTransition::VerificationFailed`, which is
/// only valid `Done -> Doing`. Done is a claim; a failed verification revokes
/// the claim, it does not discard the work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerificationResult {
    Passed,
    Failed { reason: String },
}

impl VerificationResult {
    pub fn is_passed(&self) -> bool {
        matches!(self, VerificationResult::Passed)
    }
}

/// The durable record of one verification pass (Invariant 7, RR-0015).
///
/// `verifier` is an `Actor`, not a string: WHO concluded this is part of the
/// record (a `System` verifier ran checks; a `Worker` verifier is peer
/// review). `criteria` references acceptance criteria by id (`CriterionId`) —
/// the criteria entities themselves are owned by the criteria module
/// (Invariant 50); this record links, it does not duplicate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    pub id: VerificationId,
    pub task_id: TaskId,
    /// Who reached the conclusion.
    pub verifier: crate::events::Actor,
    /// Which acceptance criteria were checked.
    pub criteria: Vec<CriterionId>,
    /// The artifact trail. A `Passed` with empty evidence is a smell the
    /// storage layer should reject; core keeps the shape honest by making
    /// evidence part of the record, not a side channel.
    pub evidence: Vec<Evidence>,
    pub result: VerificationResult,
    /// Supplied by the caller (core never reads a clock).
    pub verified_at: DateTime<Utc>,
}

/// Outcome of one criterion inside a [`CriteriaRun`]. `index` points into the
/// criteria slice AS THE CALLER PASSED IT (original position, not run order),
/// so results map back to gate definitions without guessing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CriterionOutcome {
    pub index: usize,
    pub result: VerificationResult,
}

/// The full account of a cheapest-first run: what ran (in cost order), what
/// was skipped and why that is visible at all.
///
/// `skipped` exists because of ethos rule 4: a verifier that never ran is
/// indistinguishable from one that found nothing unless the skip is recorded.
/// When a free check fails and the model judgment is skipped, whoever reads
/// the result must SEE that the expensive check did not run, not infer it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CriteriaRun {
    /// Outcomes in execution order (cheapest verifier first).
    pub ran: Vec<CriterionOutcome>,
    /// Original indices of criteria never executed because a cheaper REQUIRED
    /// criterion already failed.
    pub skipped: Vec<usize>,
    /// `Passed` iff every required criterion ran and passed.
    pub verdict: VerificationResult,
}

/// Order a criteria list cheapest-verifier-first (Invariant 28).
///
/// Returns indices into `criteria` in execution order. The sort is stable:
/// criteria with equal cost keep their authored order, so a gate author's
/// sequencing within a tier is respected and the plan is deterministic.
pub fn cheapest_first_order(criteria: &[GateCriterion]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..criteria.len()).collect();
    order.sort_by_key(|&i| criteria[i].verifier.cost_rank());
    order
}

/// Run a criteria list cheapest-verifier-first, short-circuiting on the first
/// required failure (Invariant 28: "Verifiers run in cost order. If the free
/// checks fail, expensive ones never run.").
///
/// `run` executes ONE criterion and reports its result — this is the seam
/// where the impure runner (server-side command execution, HTTP, Playwright,
/// model calls) plugs into pure core. Core decides ordering and stopping;
/// the runner only answers "did this one pass".
///
/// Halting rule: the first REQUIRED failure halts the run. Everything not yet
/// run costs at least as much (we run cheapest-first) and the verdict is
/// already decided, so running it would be spend that cannot change the
/// answer. An OPTIONAL criterion's failure never halts — it cannot decide the
/// verdict, and halting there would skip required criteria and leave the
/// verdict unknowable (ethos rule 7: make the answer space match the claim).
///
/// An empty criteria list is vacuously `Passed`: a gate with no criteria does
/// not block. `board::apply_transition` relies on this.
pub fn run_cheapest_first(
    criteria: &[GateCriterion],
    mut run: impl FnMut(&GateCriterion) -> VerificationResult,
) -> CriteriaRun {
    let order = cheapest_first_order(criteria);
    let mut ran = Vec::new();
    let mut verdict = VerificationResult::Passed;
    let mut halted_at: Option<usize> = None; // position in `order`, not original index

    for (pos, &idx) in order.iter().enumerate() {
        let criterion = &criteria[idx];
        let result = run(criterion);
        let required_failure = criterion.required && !result.is_passed();
        if required_failure {
            let detail = match &result {
                VerificationResult::Failed { reason } => reason.clone(),
                VerificationResult::Passed => unreachable!("required_failure implies !Passed"),
            };
            verdict = VerificationResult::Failed {
                reason: format!(
                    "required criterion '{}' failed: {detail}",
                    criterion.description
                ),
            };
        }
        ran.push(CriterionOutcome { index: idx, result });
        if required_failure {
            halted_at = Some(pos);
            break;
        }
    }

    let skipped = match halted_at {
        Some(pos) => order[pos + 1..].to_vec(),
        None => Vec::new(),
    };
    CriteriaRun {
        ran,
        skipped,
        verdict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        DateTime::from_timestamp(1_754_000_000, 0).unwrap()
    }

    fn command() -> VerifierKind {
        VerifierKind::Command {
            cmd: "cargo test --workspace".into(),
            expected_exit: 0,
        }
    }
    fn http() -> VerifierKind {
        VerifierKind::HttpCheck {
            url: "https://localhost:8824/health".into(),
            expected_status: 200,
        }
    }
    fn file() -> VerifierKind {
        VerifierKind::FileExists {
            path: PathBuf::from("/tmp/artifact.png"),
        }
    }
    fn temporal() -> VerifierKind {
        VerifierKind::Temporal {
            after: t0(),
        }
    }
    fn playwright() -> VerifierKind {
        VerifierKind::PlaywrightAssertion {
            script: "expect(page.locator('#board')).toBeVisible()".into(),
        }
    }
    fn model() -> VerifierKind {
        VerifierKind::ModelJudgment {
            prompt: "does the screenshot show a rendered board?".into(),
        }
    }

    fn crit(verifier: VerifierKind, required: bool) -> GateCriterion {
        GateCriterion {
            description: format!("criterion via {:?}", verifier.evidence_kind()),
            verifier,
            required,
        }
    }

    #[test]
    fn verifier_cost_order_is_command_http_file_temporal_playwright_model() {
        let ordered = [command(), http(), file(), temporal(), playwright(), model()];
        for pair in ordered.windows(2) {
            assert!(pair[0].cost_rank() < pair[1].cost_rank());
            assert!(pair[0] < pair[1]);
        }
        assert!(command().is_free() && http().is_free() && file().is_free() && temporal().is_free());
        assert!(!playwright().is_free() && !model().is_free());
        assert!(temporal().is_deterministic());
        assert!(playwright().is_deterministic());
        assert!(!model().is_deterministic());
    }

    #[test]
    fn evidence_kind_maps_one_to_one() {
        assert_eq!(command().evidence_kind(), EvidenceKind::CommandOutput);
        assert_eq!(http().evidence_kind(), EvidenceKind::HttpResponse);
        assert_eq!(file().evidence_kind(), EvidenceKind::FileStat);
        assert_eq!(temporal().evidence_kind(), EvidenceKind::TemporalCheck);
        assert_eq!(
            playwright().evidence_kind(),
            EvidenceKind::PlaywrightArtifact
        );
        assert_eq!(model().evidence_kind(), EvidenceKind::ModelTranscript);
    }

    #[test]
    fn cheapest_first_order_sorts_by_cost_and_is_stable() {
        let criteria = vec![
            crit(model(), true),      // rank 4
            crit(command(), true),    // rank 0
            crit(playwright(), true), // rank 3
            crit(file(), true),       // rank 2
            crit(http(), true),       // rank 1
        ];
        assert_eq!(cheapest_first_order(&criteria), vec![1, 4, 3, 2, 0]);

        // Stability: two commands keep authored order.
        let criteria = vec![
            crit(command(), true),
            crit(model(), true),
            crit(command(), true),
        ];
        assert_eq!(cheapest_first_order(&criteria), vec![0, 2, 1]);
    }

    #[test]
    fn required_free_failure_short_circuits_before_model_judgment() {
        // Model judgment listed FIRST by the author; the runner must still run
        // the free command first and never invoke the model after it fails.
        let criteria = vec![crit(model(), true), crit(command(), true)];
        let mut model_calls = 0u32;
        let outcome = run_cheapest_first(&criteria, |c| match c.verifier {
            VerifierKind::Command { .. } => VerificationResult::Failed {
                reason: "exit 1 (3 tests failed)".into(),
            },
            VerifierKind::ModelJudgment { .. } => {
                model_calls += 1;
                VerificationResult::Passed
            }
            _ => VerificationResult::Passed,
        });

        assert_eq!(
            model_calls, 0,
            "expensive verifier ran after a free failure"
        );
        assert_eq!(outcome.ran.len(), 1);
        assert_eq!(outcome.ran[0].index, 1); // the command, by original index
        assert_eq!(outcome.skipped, vec![0]); // the model judgment, visibly skipped
        match &outcome.verdict {
            VerificationResult::Failed { reason } => {
                assert!(
                    reason.contains("exit 1"),
                    "verdict carries the failure detail"
                );
            }
            VerificationResult::Passed => panic!("verdict must be Failed"),
        }
    }

    #[test]
    fn optional_failure_does_not_halt_or_decide_the_verdict() {
        let criteria = vec![crit(command(), false), crit(file(), true)];
        let outcome = run_cheapest_first(&criteria, |c| match c.verifier {
            VerifierKind::Command { .. } => VerificationResult::Failed {
                reason: "optional lint warning".into(),
            },
            _ => VerificationResult::Passed,
        });
        assert_eq!(outcome.ran.len(), 2, "the required criterion still ran");
        assert!(outcome.skipped.is_empty());
        assert!(outcome.verdict.is_passed());
    }

    #[test]
    fn all_pass_runs_everything_in_cost_order() {
        let criteria = vec![
            crit(model(), true),
            crit(command(), true),
            crit(file(), true),
        ];
        let outcome = run_cheapest_first(&criteria, |_| VerificationResult::Passed);
        let run_order: Vec<usize> = outcome.ran.iter().map(|o| o.index).collect();
        assert_eq!(run_order, vec![1, 2, 0]);
        assert!(outcome.skipped.is_empty());
        assert!(outcome.verdict.is_passed());
    }

    #[test]
    fn empty_criteria_are_vacuously_passed() {
        let outcome = run_cheapest_first(&[], |_| unreachable!());
        assert!(outcome.verdict.is_passed());
        assert!(outcome.ran.is_empty() && outcome.skipped.is_empty());
    }

    #[test]
    fn serde_shapes_are_tagged_snake_case() {
        let v = serde_json::to_value(command()).unwrap();
        assert_eq!(v["kind"], "command");
        assert_eq!(v["cmd"], "cargo test --workspace");

        let r = serde_json::to_value(VerificationResult::Failed {
            reason: "nope".into(),
        })
        .unwrap();
        assert_eq!(r["kind"], "failed");
        assert_eq!(r["reason"], "nope");
    }

    #[test]
    fn evidence_without_source_deserializes_as_self_reported() {
        // Provenance-less legacy evidence must land at the WEAKEST trust
        // level, never silently at Independent (Invariant 28).
        let json = format!(
            r#"{{"kind":"command_output","description":"tests green","artifact":null,"produced_at":"{}"}}"#,
            t0().to_rfc3339()
        );
        let ev: Evidence = serde_json::from_str(&json).unwrap();
        assert_eq!(ev.source, EvidenceSource::SelfReported);
        assert!(!ev.is_independent());

        let independent = Evidence {
            kind: EvidenceKind::CommandOutput,
            description: "harness ran cargo test".into(),
            artifact: Some("/tmp/test.log".into()),
            produced_at: t0(),
            source: EvidenceSource::Independent,
        };
        assert!(independent.is_independent());
    }
}
