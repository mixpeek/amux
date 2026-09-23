//! Project execution is a policy on a group and a projection of board issues.
//! Workers are assignments; they never become the identity of the project.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicy {
    pub repository: String,
    /// Project executors default to a dedicated Git worktree. Shared-checkout mode is explicit,
    /// single-lane, and reserved for projects where the operator wants one worker in the saved
    /// project directory instead of a disposable candidate checkout.
    #[serde(default = "enabled_by_default")]
    pub worktree: bool,
    pub coordinator: ModelProfile,
    pub executor: ModelProfile,
    /// Explicitly allow a Codex executor to use local host tools such as a
    /// Docker socket. Off by default; the candidate container's own network
    /// isolation remains an independent acceptance requirement.
    #[serde(default)]
    pub executor_full_host_access: bool,
    #[serde(default = "one")]
    pub max_executors: usize,
    #[serde(default = "two")]
    pub max_attempts: u32,
    #[serde(default)]
    pub token_budget: Option<u64>,
    #[serde(default)]
    pub cost_budget_usd: Option<f64>,
    #[serde(default)]
    pub paused: bool,
    /// A project can be prepared and previewed without owning any dispatch.
    #[serde(default)]
    pub enabled: bool,
    /// A repository-owned candidate check, executed through fanout_workspace.
    pub verify_command: String,
    #[serde(default = "verification_timeout_default")]
    pub verification_timeout_secs: u64,
    /// Operator-approved project acceptance contract. `None` is a legacy project: acceptance is
    /// reported as not configured, never inferred from task completion.
    #[serde(default)]
    pub acceptance: Option<AcceptanceContract>,
}
/// The project-level acceptance contract. It is operator authority: executors report against it and
/// can neither replace a verifier, add, omit or repeat a criterion, nor approve a human criterion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceContract {
    /// Server-owned. It increments whenever the criteria change; a client value is ignored.
    #[serde(default)]
    pub revision: u32,
    pub criteria: Vec<ContractCriterion>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractCriterion {
    /// Stable identity, referenced by tasks as `contract:<id>`.
    pub id: String,
    pub requirement: String,
    pub verifier: ContractVerifier,
    /// Candidate-relative files the verifier must leave behind. They are retained as passive assets.
    #[serde(default)]
    pub evidence: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContractVerifier {
    /// A repository command run on the composed project candidate. Exit 0 passes. This is for static
    /// properties (tests, compilation, file structure), not claims about a running system.
    Command {
        id: String,
        command: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    /// A command that must produce a fresh, machine-readable execution receipt. The harness binds
    /// that receipt to this invocation and exact candidate before any human review can open.
    Execution {
        id: String,
        command: String,
        #[serde(default)]
        timeout_secs: Option<u64>,
        /// Candidate-relative JSON path created by this invocation. It must not exist in Git.
        receipt: String,
        /// Every named lifecycle stage must occur exactly once with state `passed` and evidence.
        required_stages: Vec<String>,
        /// Harness-checked measurements from raw files produced by this invocation.
        /// Every required stage needs at least one assertion outside the receipt.
        #[serde(default)]
        assertions: Vec<ExecutionAssertion>,
    },
    /// Explicit human review. Only the operator can approve it, bound to the exact revision.
    Human { id: String, instructions: String },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionAssertion {
    pub stage: String,
    pub artifact: String,
    /// JSON Pointer into the raw artifact (RFC 6901).
    pub pointer: String,
    pub operator: ExecutionAssertionOperator,
    /// A JSON literal, such as 100, true, or "passed".
    pub expected: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionAssertionOperator {
    Equals,
    AtLeast,
}
impl ContractVerifier {
    pub fn id(&self) -> &str {
        match self {
            Self::Command { id, .. } | Self::Execution { id, .. } | Self::Human { id, .. } => id,
        }
    }
    pub fn is_human(&self) -> bool {
        matches!(self, Self::Human { .. })
    }
}
pub const MAX_CONTRACT_CRITERIA: usize = 32;
pub fn valid_contract_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 48
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}
/// Passive formats only, the same set the report asset store retains.
pub fn valid_evidence_path(path: &str) -> bool {
    let p = std::path::Path::new(path);
    !path.is_empty()
        && path.len() <= 240
        && p.is_relative()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        && p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "md" | "json" | "txt" | "png" | "webm"))
}
impl AcceptanceContract {
    pub fn validate(&self) -> Result<(), String> {
        // An empty contract must never pass vacuously.
        if self.criteria.is_empty() {
            return Err("acceptance contract needs at least one criterion".into());
        }
        if self.criteria.len() > MAX_CONTRACT_CRITERIA {
            return Err(format!(
                "acceptance contract allows at most {MAX_CONTRACT_CRITERIA} criteria"
            ));
        }
        let mut ids = std::collections::HashSet::new();
        let mut verifiers = std::collections::HashSet::new();
        for c in &self.criteria {
            if !valid_contract_id(&c.id) {
                return Err(format!(
                    "criterion id {:?} must match [a-z0-9][a-z0-9_-]{{0,47}}",
                    c.id
                ));
            }
            if !ids.insert(c.id.as_str()) {
                return Err(format!("duplicate criterion id {}", c.id));
            }
            if c.requirement.trim().is_empty() || c.requirement.len() > 500 {
                return Err(format!("{}: requirement must be 1..500 characters", c.id));
            }
            if !valid_contract_id(c.verifier.id()) || !verifiers.insert(c.verifier.id()) {
                return Err(format!(
                    "{}: verifier id must be a valid, unique identity",
                    c.id
                ));
            }
            match &c.verifier {
                ContractVerifier::Command {
                    command,
                    timeout_secs,
                    ..
                }
                | ContractVerifier::Execution {
                    command,
                    timeout_secs,
                    ..
                } => {
                    if command.trim().is_empty() || command.len() > 4000 {
                        return Err(format!("{}: command must be 1..4000 characters", c.id));
                    }
                    if timeout_secs
                        .is_some_and(|t| !(1..=MAX_VERIFICATION_TIMEOUT_SECS).contains(&t))
                    {
                        return Err(format!(
                            "{}: timeout_secs must be 1..{MAX_VERIFICATION_TIMEOUT_SECS}",
                            c.id
                        ));
                    }
                }
                ContractVerifier::Human { instructions, .. } => {
                    if instructions.trim().is_empty() || instructions.len() > 2000 {
                        return Err(format!(
                            "{}: human review needs 1..2000 characters of instructions",
                            c.id
                        ));
                    }
                }
            }
            match &c.verifier {
                ContractVerifier::Command { command, .. }
                    if runtime_claim(&c.requirement, command) =>
                {
                    return Err(format!(
                        "{}: a runtime/end-to-end claim must use an execution verifier with a fresh receipt",
                        c.id
                    ));
                }
                ContractVerifier::Execution {
                    receipt,
                    required_stages,
                    assertions,
                    ..
                } => {
                    if !valid_evidence_path(receipt) || !receipt.ends_with(".json") {
                        return Err(format!(
                            "{}: execution receipt must be a relative .json evidence path",
                            c.id
                        ));
                    }
                    if !c.evidence.iter().any(|path| path == receipt) {
                        return Err(format!(
                            "{}: execution receipt must also be retained in evidence",
                            c.id
                        ));
                    }
                    if required_stages.is_empty() || required_stages.len() > 32 {
                        return Err(format!(
                            "{}: execution verifier needs 1..32 required stages",
                            c.id
                        ));
                    }
                    let mut stages = std::collections::HashSet::new();
                    if required_stages
                        .iter()
                        .any(|stage| !valid_contract_id(stage) || !stages.insert(stage.as_str()))
                    {
                        return Err(format!(
                            "{}: execution stages must have unique [a-z0-9][a-z0-9_-] identities",
                            c.id
                        ));
                    }
                    if assertions.is_empty() || assertions.len() > 64 {
                        return Err(format!(
                            "{}: execution verifier needs 1..64 harness-checked raw evidence assertions",
                            c.id
                        ));
                    }
                    for stage in required_stages {
                        if !assertions.iter().any(|a| &a.stage == stage) {
                            return Err(format!(
                                "{}: execution stage {stage} needs a raw evidence assertion",
                                c.id
                            ));
                        }
                    }
                    for assertion in assertions {
                        if !required_stages.contains(&assertion.stage)
                            || assertion.artifact == *receipt
                            || !c.evidence.contains(&assertion.artifact)
                            || !valid_evidence_path(&assertion.artifact)
                            || !assertion.artifact.ends_with(".json")
                            || !assertion.pointer.starts_with('/')
                            || serde_json::from_str::<serde_json::Value>(&assertion.expected)
                                .is_err()
                        {
                            return Err(format!(
                                "{}: execution assertion for {} needs a required stage, retained raw JSON artifact, JSON pointer, and JSON literal",
                                c.id, assertion.stage
                            ));
                        }
                        if matches!(assertion.operator, ExecutionAssertionOperator::AtLeast)
                            && !serde_json::from_str::<serde_json::Value>(&assertion.expected)
                                .ok()
                                .is_some_and(|v| v.is_number())
                        {
                            return Err(format!(
                                "{}: at_least assertion for {} needs a numeric expected value",
                                c.id, assertion.stage
                            ));
                        }
                    }
                }
                _ => {}
            }
            if c.evidence.len() > 8 || c.evidence.iter().any(|e| !valid_evidence_path(e)) {
                return Err(format!(
                    "{}: evidence must be at most 8 relative md/json/txt/png/webm paths",
                    c.id
                ));
            }
        }
        Ok(())
    }
    pub fn criterion(&self, id: &str) -> Option<&ContractCriterion> {
        self.criteria.iter().find(|c| c.id == id)
    }
}

/// Runtime claims need provenance that an exit code or prose artifact cannot provide. This bounded
/// classifier is deliberately conservative: projects can always choose `execution` explicitly,
/// while these unmistakable phrases may never be represented by a static command.
fn runtime_claim(requirement: &str, command: &str) -> bool {
    let text = format!("{}\n{}", requirement, command).to_ascii_lowercase();
    [
        "end-to-end",
        "end to end",
        "e2e",
        "running image",
        "runtime lifecycle",
        "full lifecycle",
        "browser flow",
        "chaos test",
        "docker run",
        "build docker image",
        "build the docker image",
        "standalone docker image",
        "playwright",
        "no external calls",
        "network isolation",
    ]
    .iter()
    .any(|term| text.contains(term))
}

pub const MAX_VERIFICATION_TIMEOUT_SECS: u64 = 3600;
pub fn verification_timeout_default() -> u64 {
    600
}
fn one() -> usize {
    1
}
fn two() -> u32 {
    2
}
fn enabled_by_default() -> bool {
    true
}

impl ExecutionPolicy {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !std::path::Path::new(&self.repository).is_absolute() {
            return Err("repository must be an absolute path");
        }
        for profile in [&self.coordinator, &self.executor] {
            if profile.provider.trim().is_empty() || profile.model.trim().is_empty() {
                return Err("coordinator and executor each need a provider and model");
            }
            if profile.effort.as_deref().is_some_and(|effort| {
                !effort.is_empty()
                    && !matches!(
                        effort,
                        "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
                    )
            }) {
                return Err(
                    "effort must be one of none, minimal, low, medium, high, xhigh, max, ultra",
                );
            }
        }
        if self.executor_full_host_access && self.executor.provider != "codex" {
            return Err("full host access is supported only for Codex executors");
        }
        if !(1..=3).contains(&self.max_executors) {
            return Err("max_executors must be 1..3; one executor is the default");
        }
        if !self.worktree && self.max_executors != 1 {
            return Err("shared-checkout projects support exactly one executor; enable dedicated worktrees for parallel execution");
        }
        if !(1..=5).contains(&self.max_attempts) {
            return Err("max_attempts must be 1..5");
        }
        if self.token_budget == Some(0)
            || self
                .cost_budget_usd
                .is_some_and(|n| !n.is_finite() || n <= 0.0)
        {
            return Err("budgets must be positive finite values or null (unconfigured)");
        }
        if !(1..=MAX_VERIFICATION_TIMEOUT_SECS).contains(&self.verification_timeout_secs) {
            return Err("verification_timeout_secs must be an integer from 1 to 3600");
        }
        if self.verify_command.trim().is_empty() {
            return Err("verify_command is required; completion needs artifact checks");
        }
        Ok(())
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
        && name
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Intake,
    Ready,
    Working,
    Waiting,
    Verifying,
    Verified,
    Closed,
    Unrecognized,
}

/// Closed is an explicit non-success disposition. Never add it to a verified
/// outcome count. Readiness additionally needs the planner's waiting reasons.
pub fn phase(status: &str, structured: bool) -> Phase {
    match status {
        "verified" => Phase::Verified,
        "discarded" | "cancelled" | "quarantined" => Phase::Closed,
        "done" | "review" => Phase::Verifying,
        "doing" => Phase::Working,
        "backlog" | "todo" | "blocked" | "needsyou" if structured => Phase::Ready,
        "backlog" | "todo" | "blocked" | "needsyou" => Phase::Intake,
        _ => Phase::Unrecognized,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn acceptance_contract_is_stable_bounded_and_never_vacuous() {
        let good = |v: serde_json::Value| serde_json::from_value::<AcceptanceContract>(v).unwrap();
        let ok = good(
            serde_json::json!({"criteria":[{"id":"unit","requirement":"unit tests pass","verifier":{"type":"command","id":"unit-tests","command":"cargo test"},"evidence":["report.md"]},{"id":"review","requirement":"owner reviews","verifier":{"type":"human","id":"owner","instructions":"look"}}]}),
        );
        assert_eq!(ok.validate(), Ok(()));
        assert!(ok.criterion("unit").is_some() && ok.criterion("nope").is_none());
        assert!(
            good(serde_json::json!({"criteria":[]})).validate().is_err(),
            "empty criteria never pass"
        );
        let mut dup = ok.clone();
        dup.criteria[1].id = "unit".into();
        assert!(dup.validate().unwrap_err().contains("duplicate"));
        let mut same_verifier = ok.clone();
        same_verifier.criteria[1].verifier = ContractVerifier::Human {
            id: "unit-tests".into(),
            instructions: "x".into(),
        };
        assert!(same_verifier.validate().is_err());
        for bad in ["Unit", "", "-x", "a b", &"a".repeat(49)] {
            let mut c = ok.clone();
            c.criteria[0].id = bad.into();
            assert!(c.validate().is_err(), "{bad:?}");
        }
        for path in ["/etc/passwd", "../x.md", "a/../b.md", "x.sh", "", "x"] {
            assert!(!valid_evidence_path(path), "{path}");
        }
        assert!(
            valid_evidence_path("out/report.md")
                && valid_evidence_path("notes/mobile.txt")
                && valid_evidence_path("shot.png")
        );
        let mut long = ok.clone();
        long.criteria[0].verifier = ContractVerifier::Command {
            id: "unit-tests".into(),
            command: "  ".into(),
            timeout_secs: None,
        };
        assert!(long.validate().is_err());
        long.criteria[0].verifier = ContractVerifier::Command {
            id: "unit-tests".into(),
            command: "true".into(),
            timeout_secs: Some(3601),
        };
        assert!(long.validate().is_err());
        assert!(serde_json::from_value::<AcceptanceContract>(
            serde_json::json!({"criteria":[],"extra":1})
        )
        .is_err());
        let static_runtime_claim = good(serde_json::json!({"criteria":[{
            "id":"lifecycle","requirement":"Full lifecycle passes in a running image with no external calls",
            "verifier":{"type":"command","id":"grep-report","command":"grep -q passed report.md"},
            "evidence":["report.md"]
        }]}));
        assert!(
            static_runtime_claim
                .validate()
                .unwrap_err()
                .contains("fresh receipt"),
            "runtime prose may not be accepted by a static string check"
        );
        let runtime = good(serde_json::json!({"criteria":[{
            "id":"lifecycle","requirement":"Full lifecycle passes in a running image with no external calls",
            "verifier":{"type":"execution","id":"run-lifecycle","command":"./scripts/run-lifecycle.sh","receipt":"artifacts/execution.json","required_stages":["image-build","api-lifecycle","network-isolation"],
                "assertions":[
                    {"stage":"image-build","artifact":"artifacts/raw.json","pointer":"/image/id","operator":"equals","expected":"\"sha256:abc\""},
                    {"stage":"api-lifecycle","artifact":"artifacts/raw.json","pointer":"/documents","operator":"at_least","expected":"100"},
                    {"stage":"network-isolation","artifact":"artifacts/raw.json","pointer":"/network/external_calls_allowed","operator":"equals","expected":"false"}
                ]},
            "evidence":["artifacts/execution.json","artifacts/raw.json","artifacts/report.md"]
        }]}));
        assert_eq!(runtime.validate(), Ok(()));
        let mut no_raw_checks = runtime.clone();
        if let ContractVerifier::Execution { assertions, .. } =
            &mut no_raw_checks.criteria[0].verifier
        {
            assertions.clear();
        }
        assert!(no_raw_checks
            .validate()
            .unwrap_err()
            .contains("raw evidence assertions"));
        let mut missing_stage_check = runtime.clone();
        if let ContractVerifier::Execution { assertions, .. } =
            &mut missing_stage_check.criteria[0].verifier
        {
            assertions.retain(|a| a.stage != "network-isolation");
        }
        assert!(missing_stage_check
            .validate()
            .unwrap_err()
            .contains("network-isolation"));
        let mut missing_receipt = runtime.clone();
        missing_receipt.criteria[0].evidence.remove(0);
        assert!(missing_receipt.validate().unwrap_err().contains("retained"));
        // A legacy policy without a contract still parses and reads as unconfigured.
        assert!(policy().acceptance.is_none());
    }
    #[test]
    fn project_verification_timeout_is_defaulted_positive_and_bounded() {
        let mut p = policy();
        assert_eq!(p.verification_timeout_secs, 600);
        for n in [1, 600, 3600] {
            p.verification_timeout_secs = n;
            assert!(p.validate().is_ok());
        }
        for n in [0, 3601, u64::MAX] {
            p.verification_timeout_secs = n;
            assert!(p.validate().is_err());
        }
        for n in [
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!("Infinity"),
            serde_json::Value::Null,
        ] {
            let mut v = serde_json::to_value(policy()).unwrap();
            v["verification_timeout_secs"] = n;
            assert!(serde_json::from_value::<ExecutionPolicy>(v).is_err());
        }
    }
    fn policy() -> ExecutionPolicy {
        serde_json::from_value(serde_json::json!({"repository":"/repo","coordinator":{"provider":"claude","model":"haiku"},"executor":{"provider":"codex","model":"configured-model"},"verify_command":"./verify.sh"})).unwrap()
    }
    #[test]
    fn defaults_are_serial_and_inert_until_enabled() {
        let p = policy();
        assert_eq!(p.max_executors, 1);
        assert_eq!(p.max_attempts, 2);
        assert!(p.worktree);
        assert!(!p.enabled);
        assert_eq!(p.token_budget, None);
        assert!(!p.executor_full_host_access);
        p.validate().unwrap();
    }
    #[test]
    fn host_tools_need_explicit_codex_executor_policy() {
        let mut p = policy();
        p.executor_full_host_access = true;
        p.validate().unwrap();
        p.executor.provider = "claude".into();
        assert!(p.validate().is_err());
    }
    #[test]
    fn policy_refuses_unbounded_fanout_and_invalid_budgets() {
        let mut p = policy();
        p.max_executors = 4;
        assert!(p.validate().is_err());
        p.max_executors = 3;
        p.worktree = false;
        assert!(p.validate().is_err());
        p.worktree = true;
        p.cost_budget_usd = Some(f64::NAN);
        assert!(p.validate().is_err());
        p.cost_budget_usd = Some(0.0);
        assert!(p.validate().is_err());
        p.cost_budget_usd = None;
        p.token_budget = Some(0);
        assert!(p.validate().is_err());
        p.token_budget = Some(1000);
        p.verify_command.clear();
        assert!(p.validate().is_err());
    }
    #[test]
    fn projection_never_equates_done_discarded_or_raw_capture_with_verified() {
        assert_eq!(phase("done", true), Phase::Verifying);
        assert_eq!(phase("discarded", true), Phase::Closed);
        assert_eq!(phase("todo", false), Phase::Intake);
        assert_eq!(phase("verified", true), Phase::Verified);
        assert_eq!(phase("custom", true), Phase::Unrecognized);
        assert!(!valid_name("../../other"));
        assert!(!valid_name("Mixed Case"));
        assert!(valid_name("project-one"));
    }
}
