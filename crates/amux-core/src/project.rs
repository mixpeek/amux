//! Project execution is a policy on a group and a projection of board issues.
//! Workers are assignments; they never become the identity of the project.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionPolicy {
    pub repository: String,
    pub coordinator: ModelProfile,
    pub executor: ModelProfile,
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
}
fn one() -> usize {
    1
}
fn two() -> u32 {
    2
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
        }
        if !(1..=3).contains(&self.max_executors) {
            return Err("max_executors must be 1..3; one executor is the default");
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
    fn policy() -> ExecutionPolicy {
        serde_json::from_value(serde_json::json!({"repository":"/repo","coordinator":{"provider":"claude","model":"haiku"},"executor":{"provider":"codex","model":"configured-model"},"verify_command":"./verify.sh"})).unwrap()
    }
    #[test]
    fn defaults_are_serial_and_inert_until_enabled() {
        let p = policy();
        assert_eq!(p.max_executors, 1);
        assert_eq!(p.max_attempts, 2);
        assert!(!p.enabled);
        assert_eq!(p.token_budget, None);
        p.validate().unwrap();
    }
    #[test]
    fn policy_refuses_unbounded_fanout_and_invalid_budgets() {
        let mut p = policy();
        p.max_executors = 4;
        assert!(p.validate().is_err());
        p.max_executors = 3;
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
